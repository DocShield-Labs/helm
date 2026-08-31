//! Per-host connection lifecycle: connect, event pump, reconnect ladder.
//!
//! One helmd connection per host — the daemon owns PTYs, scrollback,
//! and block segmentation; this module owns getting connected to it
//! and staying connected:
//!
//!   - **Local**: unix socket at `~/.helm/helmd.sock`, auto-spawning
//!     `helmd serve` from the binary next to the app executable.
//!   - **Remote**: one SSH session; the integration scripts + helmd
//!     binary are installed when missing, then a no-PTY
//!     exec channel runs `helmd stdio` and the same frame protocol
//!     flows over it.
//!
//! The pump task translates `DaemonMsg` → `HostEvent` for the frontend,
//! maintains the per-session notification index, and feeds notifications.
//! The supervisor waits for the pump to die and runs the reconnect
//! ladder (`[1, 2, 4, 8, 30]s`, early-woken by reachability + system
//! wake, with a post-wake SSH liveness probe).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use helm_domain::{
    AuthMethod, BlockInfo, CursorInfo, CursorShape, Host, HostEvent, HostId, HostKeyDecision,
    HostStatus, RetiredDaemon, RowAt, RowInfo, ScreenInfo, SearchHit, SessionEvent, SessionInfo,
    SessionTree, SpanInfo,
};
use helm_proto::client::{connect_io, connect_or_spawn_unix, connect_unix, ClientError, Connected};
use helm_proto::{DaemonMsg, TreeSnapshot};
use helm_ssh::{HostKeyPrompter, SshAuth, SshTarget};
use semver::Version;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::commands::emit_event;
use crate::integration;
use crate::notifications;
use crate::state::{
    DaemonCapabilities, HostEntry, NotificationsCtx, SessionHandle, SharedHostEntry,
};

/// Everything a connection task needs beyond its own host entry —
/// including the host registry itself, so discovering a retired daemon
/// generation can register it and connect to it from inside the
/// connect path.
#[derive(Clone)]
pub(crate) struct ConnectDeps {
    pub hosts: Arc<DashMap<HostId, SharedHostEntry>>,
    pub pending_prompts: Arc<DashMap<HostId, oneshot::Sender<HostKeyDecision>>>,
    pub event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    pub network_online: watch::Receiver<bool>,
    pub wake_signal: watch::Receiver<u64>,
    pub notif_ctx: NotificationsCtx,
}

/// All connect logic past the State<'_> prelude. One-shot from the
/// user's view: if the initial connect fails, the host stays in `Error`
/// state and the user re-clicks. Reconnect on transport drop is the
/// supervisor's job.
///
/// Boxed rather than `async fn`: connecting discovers retired daemon
/// generations, and adopting one calls back into `do_connect` — the
/// box is the indirection that recursion through opaque futures needs.
pub(crate) fn do_connect(
    entry: SharedHostEntry,
    host_id: HostId,
    prompter: Arc<dyn HostKeyPrompter>,
    deps: ConnectDeps,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
    Box::pin(do_connect_inner(entry, host_id, prompter, deps))
}

async fn do_connect_inner(
    entry: SharedHostEntry,
    host_id: HostId,
    prompter: Arc<dyn HostKeyPrompter>,
    deps: ConnectDeps,
) -> Result<(), String> {
    let event_tx = deps.event_tx.clone();
    // Serialize connect attempts for this host (StrictMode/HMR double
    // effects, user double-clicks, supervisor reconnects).
    let connect_lock = {
        let g = entry.lock().await;
        g.connect_lock.clone()
    };
    let _connect_guard = connect_lock.lock().await;

    emit_event(
        &event_tx,
        HostEvent::Status {
            host_id,
            status: HostStatus::Connecting,
            error: None,
        },
    );

    let host = {
        let mut guard = entry.lock().await;
        guard.shutdown_session();
        guard.host.clone()
    };

    match connect_once(&entry, host_id, &host, &prompter, &deps).await {
        Ok(pump_dead) => {
            let handle = tokio::spawn(supervise(
                entry.clone(),
                host_id,
                host,
                prompter,
                pump_dead,
                deps,
            ));
            entry.lock().await.supervisor = Some(handle.abort_handle());
            Ok(())
        }
        Err(e) => {
            entry.lock().await.status = HostStatus::Error;
            emit_event(
                &event_tx,
                HostEvent::Status {
                    host_id,
                    status: HostStatus::Error,
                    error: Some(e.clone()),
                },
            );
            Err(e)
        }
    }
}

/// Establish + install + announce Connected. Shared by the initial
/// connect and every supervisor reconnect.
async fn connect_once(
    entry: &SharedHostEntry,
    host_id: HostId,
    host: &Host,
    prompter: &Arc<dyn HostKeyPrompter>,
    deps: &ConnectDeps,
) -> Result<mpsc::UnboundedReceiver<()>, String> {
    let event_tx = &deps.event_tx;
    let mut established = establish(host.clone(), Some(prompter.clone())).await?;

    // Zero-kill upgrade: ask an older daemon to retire in place — it
    // renames its socket, keeps serving its sessions until they end,
    // and the well-known socket frees up for the current binary. The
    // attempt consumes the connection's pre-attach event stream either
    // way, so both outcomes re-establish: success spawns the new
    // daemon; a daemon too old to know the extension answers with an
    // error and gets reconnected as-is (it retires once it empties
    // out — the pre-retirement story).
    if host.retired.is_none() && should_retire_daemon(&established.connected.daemon_version) {
        let old_version = established.connected.daemon_version.clone();
        match try_retire(&mut established).await {
            Some(socket) => {
                tracing::info!(%socket, %old_version, "daemon retired in place; spawning current binary");
            }
            None => {
                tracing::info!(%old_version, "daemon predates in-place retirement; leaving it until it empties");
            }
        }
        drop(established);
        established = establish(host.clone(), Some(prompter.clone())).await?;
    }

    let pump_dead = install_session(
        entry,
        host_id,
        host,
        established,
        event_tx.clone(),
        deps.notif_ctx.clone(),
    )
    .await?;
    emit_event(
        event_tx,
        HostEvent::Status {
            host_id,
            status: HostStatus::Connected,
            error: None,
        },
    );

    // Pick up any retired generations still serving sessions — from
    // the retirement above, or left over from before an app relaunch.
    if host.retired.is_none() {
        tokio::spawn(discover_retired_daemons(
            host.clone(),
            entry.clone(),
            deps.clone(),
        ));
    }
    Ok(pump_dead)
}

/// Send RETIRE over a fresh, pre-attach connection and wait for the
/// reply. `Some(socket)` when the daemon retired; `None` when it
/// doesn't know the extension (or didn't answer in time).
async fn try_retire(established: &mut Established) -> Option<String> {
    const REQ: u64 = 1;
    let client = &established.connected.client;
    client
        .extension(Some(REQ), helm_proto::extensions::RETIRE.into(), Vec::new())
        .ok()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout_at(deadline, established.connected.events.recv())
            .await
            .ok()??;
        match msg {
            DaemonMsg::Extension {
                req_id: Some(REQ),
                payload,
                ..
            } => {
                let reply: helm_proto::extensions::RetireReply =
                    serde_json::from_slice(&payload).ok()?;
                return Some(reply.socket);
            }
            DaemonMsg::Error {
                req_id: Some(REQ),
                message,
                ..
            } => {
                tracing::debug!(%message, "daemon declined retirement");
                return None;
            }
            _ => continue,
        }
    }
}

/// A freshly-established daemon connection plus its transport backing.
struct Established {
    connected: Connected,
    ssh: Option<Arc<helm_ssh::SshSession>>,
}

/// Wire an established connection into the host entry: spawn the pump,
/// stash the `SessionHandle`, sync notifications, deliver offline
/// notifications, and bootstrap a session if the daemon has none.
/// Returns the pump-death receiver for the supervisor.
async fn install_session(
    entry: &SharedHostEntry,
    host_id: HostId,
    host: &Host,
    established: Established,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    notif_ctx: NotificationsCtx,
) -> Result<mpsc::UnboundedReceiver<()>, String> {
    let Established { connected, ssh } = established;
    let Connected {
        client,
        events,
        daemon_version,
        state,
        pending,
    } = connected;
    let retire_when_empty = host.retired.is_none() && should_retire_daemon(&daemon_version);
    tracing::info!("connected to helmd {daemon_version} on {host_id:?}");

    // Prime the breadcrumb index from the initial snapshot, then fold
    // in every notification that accumulated while we were away.
    notifications::sync_session_index(&notif_ctx, &event_tx, host_id, &state);
    let last_note_id = pending.iter().map(|n| n.id.0).max();
    for note in pending {
        notifications::process_daemon_notification(&notif_ctx, &event_tx, host_id, &note);
    }
    let tree = Arc::new(parking_lot::Mutex::new(state));
    let pending_reqs: Arc<DashMap<u64, oneshot::Sender<DaemonMsg>>> = Arc::new(DashMap::new());
    let capabilities = Arc::new(parking_lot::RwLock::new(DaemonCapabilities::default()));

    let (dead_tx, dead_rx) = mpsc::unbounded_channel::<()>();
    let pump = tokio::spawn(pump(
        host_id,
        host.clone(),
        ssh.clone(),
        events,
        tree.clone(),
        pending_reqs.clone(),
        capabilities.clone(),
        retire_when_empty.then(|| client.clone()),
        event_tx.clone(),
        notif_ctx.clone(),
        dead_tx,
    ));

    let session = Arc::new(SessionHandle::new(
        client,
        pump.abort_handle(),
        tree,
        pending_reqs,
        capabilities,
        daemon_version,
    ));

    // We've shown the offline notifications; let the daemon drop them.
    if let Some(max) = last_note_id {
        let _ = session
            .client
            .ack_notifications(helm_proto::NotificationId(max));
    }
    let _ = session.client.attach();

    // A fresh daemon gets one shell session before the initial tree is
    // emitted. An older compatible daemon first gets a brief opportunity
    // to advertise atomic drain support; if it does, retire it before
    // bootstrapping so reconnect launches the binary already on disk.
    // Retired generations get neither: they exist only to carry their
    // remaining sessions to the end.
    if host.retired.is_none() && session.tree.lock().sessions.is_empty() {
        if retire_when_empty {
            for _ in 0..10 {
                if session.capabilities.read().compatibility_baseline.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        if !retire_when_empty
            || !request_retirement_if_empty(&session.client, &session.tree, &session.capabilities)
        {
            if let Err(e) = session
                .request(|id| session.client.new_session(id, None, None, None))
                .await
            {
                tracing::warn!("bootstrap session on {host_id:?}: {e}");
            }
        }
    }

    // Ship the initial tree to the frontend.
    let initial = to_domain_tree(&session.tree.lock());
    {
        let mut guard = entry.lock().await;
        guard.session = Some(session);
        guard.ssh = ssh;
        guard.status = HostStatus::Connected;
    }
    emit_event(
        &event_tx,
        HostEvent::Session {
            host_id,
            event: SessionEvent::Tree { tree: initial },
        },
    );

    Ok(dead_rx)
}

/// Event pump: daemon messages → frontend events + app-side state.
#[allow(clippy::too_many_arguments)]
async fn pump(
    host_id: HostId,
    host: Host,
    ssh: Option<Arc<helm_ssh::SshSession>>,
    mut events: mpsc::UnboundedReceiver<DaemonMsg>,
    tree: Arc<parking_lot::Mutex<TreeSnapshot>>,
    pending: Arc<DashMap<u64, oneshot::Sender<DaemonMsg>>>,
    capabilities: Arc<parking_lot::RwLock<DaemonCapabilities>>,
    retiring_client: Option<helm_proto::client::HelmdClient>,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    notif_ctx: NotificationsCtx,
    dead_tx: mpsc::UnboundedSender<()>,
) {
    let emit = |event: SessionEvent| {
        if let Some(tx) = &event_tx {
            let _ = tx.send(HostEvent::Session { host_id, event });
        }
    };
    while let Some(msg) = events.recv().await {
        match msg {
            // A full screen with a req_id answers a `session_screen`
            // call; without one it's a broadcast repaint.
            DaemonMsg::Screen {
                req_id: None,
                session,
                screen,
            } => {
                emit(SessionEvent::Screen {
                    session_id: session.to_string(),
                    screen: to_domain_screen(screen),
                });
            }
            DaemonMsg::ScreenDiff {
                session,
                top_line,
                scroll,
                rows,
                cursor,
                modes,
            } => {
                emit(SessionEvent::ScreenDiff {
                    session_id: session.to_string(),
                    top_line,
                    scroll,
                    rows: rows
                        .into_iter()
                        .map(|(index, row)| RowAt {
                            index,
                            row: to_domain_row(row),
                        })
                        .collect(),
                    cursor: to_domain_cursor(&cursor),
                    modes,
                });
            }
            DaemonMsg::HistoryAppend {
                session,
                first_line,
                rows,
            } => {
                emit(SessionEvent::HistoryAppend {
                    session_id: session.to_string(),
                    first_line,
                    rows: rows.into_iter().map(to_domain_row).collect(),
                });
            }
            DaemonMsg::Block { session, block } => {
                let session_id = session.to_string();
                crate::tool_integrations::detect_from_block(
                    &notif_ctx.tool_integration_seen,
                    &event_tx,
                    &host,
                    ssh.clone(),
                    host_id,
                    &block,
                );
                emit(SessionEvent::Block {
                    session_id,
                    block: to_domain_block(&block),
                });
            }
            DaemonMsg::ModeChange {
                session,
                alt_screen,
            } => {
                emit(SessionEvent::ModeChange {
                    session_id: session.to_string(),
                    alt_screen,
                });
            }
            DaemonMsg::TreeChanged { state } => {
                *tree.lock() = state.clone();
                notifications::sync_session_index(&notif_ctx, &event_tx, host_id, &state);
                emit(SessionEvent::Tree {
                    tree: to_domain_tree(&state),
                });
                if let Some(client) = &retiring_client {
                    request_retirement_if_empty(client, &tree, &capabilities);
                }
            }
            DaemonMsg::SessionExited { session, status } => {
                emit(SessionEvent::SessionExited {
                    session_id: session.to_string(),
                    status,
                });
            }
            DaemonMsg::Notification { note } => {
                if matches!(note.kind, helm_proto::NotificationKind::Bell) {
                    emit(SessionEvent::Bell {
                        session_id: note.session.to_string(),
                    });
                }
                notifications::process_daemon_notification(&notif_ctx, &event_tx, host_id, &note);
            }
            DaemonMsg::Error {
                req_id: None,
                context,
                message,
            } => {
                tracing::warn!("helmd error on {host_id:?} ({context}): {message}");
            }
            DaemonMsg::HelloAck { .. } => {
                tracing::debug!("unexpected HelloAck mid-stream on {host_id:?}");
            }
            DaemonMsg::Capabilities {
                compatibility_baseline,
                extensions,
            } => {
                let mut negotiated = capabilities.write();
                negotiated.compatibility_baseline = Some(compatibility_baseline);
                negotiated.extensions = extensions.iter().cloned().collect();
                tracing::debug!(
                    compatibility_baseline,
                    ?extensions,
                    "helmd capabilities on {host_id:?}"
                );
                drop(negotiated);
                if let Some(client) = &retiring_client {
                    request_retirement_if_empty(client, &tree, &capabilities);
                }
            }
            // Correlated replies go to whoever is waiting; an unclaimed reply means
            // the waiter already timed out.
            reply => {
                if let Some(id) = reply.req_id() {
                    if let Some((_, tx)) = pending.remove(&id) {
                        let _ = tx.send(reply);
                    }
                }
            }
        }
    }
    let _ = dead_tx.send(());
}

/// Reconnect supervisor. Waits for the pump to die (transport drop),
/// probes SSH liveness on system wake, and runs the backoff ladder.
/// For a retired generation there is one extra exit: when its daemon's
/// socket stops answering, the daemon has served its last session and
/// exited — the entry is removed instead of reconnected.
async fn supervise(
    entry: SharedHostEntry,
    host_id: HostId,
    host: Host,
    prompter: Arc<dyn HostKeyPrompter>,
    mut pump_dead: mpsc::UnboundedReceiver<()>,
    deps: ConnectDeps,
) {
    let event_tx = deps.event_tx.clone();
    let notif_ctx = deps.notif_ctx.clone();
    let mut network_online = deps.network_online.clone();
    let mut wake_signal = deps.wake_signal.clone();
    const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 30];
    const WAKE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    let mut attempt = 0u32;
    let mut last_error: Option<String> = None;
    let mut wake_alive = true;
    let _ = wake_signal.borrow_and_update();

    loop {
        // ----- connected: wait for pump death, probing on wake -----
        loop {
            tokio::select! {
                dead = pump_dead.recv() => {
                    if dead.is_some() {
                        tracing::info!("supervisor: connection dropped for {host_id:?}");
                    }
                    break;
                }
                res = wake_signal.changed(), if wake_alive => {
                    if res.is_err() {
                        wake_alive = false;
                        continue;
                    }
                    let ssh = entry.lock().await.ssh.clone();
                    let Some(ssh) = ssh else {
                        continue; // local — sleep can't kill the socket
                    };
                    tracing::info!("supervisor: system wake; probing ssh for {host_id:?}");
                    let probe = tokio::time::timeout(
                        WAKE_PROBE_TIMEOUT,
                        tokio::task::spawn_blocking(move || ssh.run_oneshot("true".into())),
                    )
                    .await;
                    match probe {
                        Ok(Ok(Ok(_))) => continue,
                        _ => {
                            tracing::warn!(
                                "supervisor: post-wake probe failed for {host_id:?}; reconnecting"
                            );
                            break;
                        }
                    }
                }
            }
        }

        // ----- decide: exit cleanly or reconnect -----
        if finish_if_voluntary(&entry, host_id, &event_tx, &notif_ctx).await {
            return;
        }
        {
            let mut guard = entry.lock().await;
            guard.shutdown_session();
            guard.status = HostStatus::Reconnecting;
        }
        emit_event(
            &event_tx,
            HostEvent::Status {
                host_id,
                status: HostStatus::Reconnecting,
                error: last_error.clone(),
            },
        );

        let bucket_idx = (attempt as usize).min(BACKOFF_SECS.len() - 1);
        let delay = Duration::from_secs(BACKOFF_SECS[bucket_idx]);
        let was_offline = !*network_online.borrow_and_update();
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            res = network_online.changed() => {
                if res.is_ok() && was_offline && *network_online.borrow() {
                    attempt = 0;
                    continue;
                }
            }
            res = wake_signal.changed(), if wake_alive => {
                if res.is_ok() {
                    attempt = 0;
                } else {
                    wake_alive = false;
                }
            }
        }

        if finish_if_voluntary(&entry, host_id, &event_tx, &notif_ctx).await {
            return;
        }

        // ----- attempt reconnect -----
        let connect_lock = entry.lock().await.connect_lock.clone();
        let _reconnect_guard = connect_lock.lock().await;
        match connect_once(&entry, host_id, &host, &prompter, &deps).await {
            Ok(new_dead) => {
                pump_dead = new_dead;
                attempt = 0;
                last_error = None;
            }
            Err(e) if host.retired.is_some() && e.contains(RETIRED_GONE) => {
                tracing::info!("retired daemon for {host_id:?} has exited; removing its entry");
                entry.lock().await.shutdown_session();
                remove_retired_entry(host_id, &host, &deps);
                return;
            }
            Err(e) => {
                tracing::warn!("reconnect attempt {attempt} for {host_id:?} failed: {e}");
                last_error = Some(e);
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// If the user asked to disconnect, tear down, announce Disconnected,
/// clear the inbox for the host and report `true` so the supervisor
/// exits. One lock, one place.
async fn finish_if_voluntary(
    entry: &SharedHostEntry,
    host_id: HostId,
    event_tx: &Option<mpsc::UnboundedSender<HostEvent>>,
    notif_ctx: &NotificationsCtx,
) -> bool {
    {
        let mut guard = entry.lock().await;
        if !guard.voluntary_disconnect {
            return false;
        }
        guard.shutdown_session();
        guard.status = HostStatus::Disconnected;
        guard.supervisor = None;
    }
    emit_event(
        event_tx,
        HostEvent::Status {
            host_id,
            status: HostStatus::Disconnected,
            error: None,
        },
    );
    notifications::dismiss_for_host(notif_ctx, event_tx, host_id);
    true
}

// -------------------------------------------------------------------
// Transport establishment
// -------------------------------------------------------------------

/// Marks a connect error as "the retired daemon is gone": its socket
/// no longer answers, which for a retired generation means it served
/// its last session and exited. The supervisor removes the entry
/// instead of running the reconnect ladder against a ghost.
pub(crate) const RETIRED_GONE: &str = "retired daemon gone";

async fn establish(
    host: Host,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<Established, String> {
    if let Some(retired) = host.retired.clone() {
        if host.port == 0 {
            return establish_local_retired(retired).await;
        }
        return establish_remote_retired(host, retired, prompter).await;
    }
    if host.port == 0 {
        return establish_local().await;
    }
    establish_remote(host, prompter).await
}

/// Attach to a retired local daemon on its renamed socket. Never
/// spawns — a dead retired daemon stays dead.
async fn establish_local_retired(retired: RetiredDaemon) -> Result<Established, String> {
    let socket = PathBuf::from(&retired.socket);
    let name = format!("helm-app {}", env!("CARGO_PKG_VERSION"));
    let connected = tokio::task::spawn_blocking(move || connect_unix(&socket, &name))
        .await
        .map_err(|e| format!("connect join: {e}"))?
        .map_err(|e| match e {
            ClientError::Io(_) | ClientError::HandshakeClosed | ClientError::Closed => {
                format!("{RETIRED_GONE}: {e}")
            }
            other => other.to_string(),
        })?;
    Ok(Established {
        connected,
        ssh: None,
    })
}

/// Attach to a retired remote daemon: an SSH exec channel running
/// `helmd stdio --attach --socket <renamed>`. The bridge binary is the
/// *current* helmd (it's a byte pipe, generation-agnostic); `--attach`
/// keeps it from ever spawning a daemon on the retired socket.
async fn establish_remote_retired(
    host: Host,
    retired: RetiredDaemon,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<Established, String> {
    let target = SshTarget {
        hostname: host.hostname.clone(),
        port: host.port,
        user: host.user.clone(),
        jump: None,
    };
    let auth = ssh_auth(&host)?;
    let name = format!("helm-app {}", env!("CARGO_PKG_VERSION"));
    tokio::task::spawn_blocking(move || -> Result<Established, String> {
        let session = helm_ssh::connect_session(target, auth, Duration::from_secs(15), prompter)
            .map_err(|e| e.to_string())?;
        let session = Arc::new(session);
        let bridge = format!(
            r#"exec "$HOME/.helm/bin/helmd" stdio --attach --socket '{}'"#,
            retired.socket
        );
        let opened = session
            .open_exec(bridge)
            .map_err(|e| ClientError::Io(std::io::Error::other(e.to_string())))
            .and_then(|opened| {
                connect_io(Box::new(opened.reader), Box::new(opened.writer), &name, None)
            });
        match opened {
            Ok(connected) => Ok(Established {
                connected,
                ssh: Some(session),
            }),
            Err(e) => {
                // A failed attach is ambiguous over SSH — distinguish
                // "daemon exited" (socket removed) from a transport
                // hiccup before writing it off.
                let gone = session
                    .run_oneshot(format!(r#"[ -S '{}' ] || echo gone"#, retired.socket))
                    .map(|out| out.stdout.contains("gone"))
                    .unwrap_or(false);
                if gone {
                    Err(format!("{RETIRED_GONE}: socket removed"))
                } else {
                    Err(e.to_string())
                }
            }
        }
    })
    .await
    .map_err(|e| format!("remote connect join: {e}"))?
}

async fn establish_local() -> Result<Established, String> {
    let bin = helmd_bin_path()?;
    let socket = helmd_socket_path();
    let name = format!("helm-app {}", env!("CARGO_PKG_VERSION"));
    let connected = tokio::task::spawn_blocking(move || {
        match connect_or_spawn_unix(&socket, &bin, &name) {
            Err(e) if is_stale_daemon(&e) => Err(ClientError::HandshakeRejected(format!(
                "incompatible local helmd ({e}); existing sessions were left running — reopen them with the matching Helm build or stop that daemon explicitly"
            ))),
            r => r,
        }
    })
    .await
    .map_err(|e| format!("connect join: {e}"))?
    .map_err(|e| e.to_string())?;
    Ok(Established {
        connected,
        ssh: None,
    })
}

/// Did a daemon from another build answer our hello? The clean case is
/// an explicit `protocol mismatch` rejection. But the frame format is
/// bincode, which tags enum variants by index — so once a protocol bump
/// inserts variants ahead of `DaemonMsg::Error`, the old daemon's
/// rejection decodes on our side as some other variant
/// (`UnexpectedHello`) or as nothing at all (`Proto(Decode)`). Anything
/// short of a `HelloAck` means the client cannot safely attach. The
/// daemon may still own live sessions, so callers must leave it alone.
fn is_stale_daemon(err: &ClientError) -> bool {
    match err {
        ClientError::HandshakeRejected(m) => m.contains("protocol mismatch"),
        ClientError::UnexpectedHello => true,
        ClientError::Proto(helm_proto::ProtoError::Decode(_)) => true,
        _ => false,
    }
}

/// The local daemon's socket: `~/.helm/helmd.sock`, or `$HELM_SOCKET`.
/// The override lets a dev build run against its own daemon while the
/// installed app keeps its sessions on the default socket.
fn helmd_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("HELM_SOCKET") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".helm")
        .join("helmd.sock")
}

/// Locate the helmd binary to run/ship. Resolution order:
///   1. `$HELMD_BIN` (dev override)
///   2. `helmd` next to the app executable (bundled sidecar / cargo
///      target dir — both put the two binaries side by side)
///   3. `helmd` on PATH
fn helmd_bin_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("HELMD_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("helmd");
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }
    // Dev convenience only: a release build must run the helmd it
    // shipped with, never whatever happens to be on PATH.
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join("helmd");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Err("helmd binary not found (set HELMD_BIN or place it next to the app binary)".into())
}

/// SSH credentials for a host. A retired generation authenticates as
/// its parent — the Keychain entry lives under the parent's id.
fn ssh_auth(host: &Host) -> Result<SshAuth, String> {
    Ok(match host.auth.clone() {
        AuthMethod::Agent => SshAuth::Agent,
        AuthMethod::KeyFile { path } => SshAuth::KeyFile {
            path: PathBuf::from(path),
            passphrase: None,
        },
        AuthMethod::Password => {
            let keychain_id = host.retired.as_ref().map(|r| r.parent).unwrap_or(host.id);
            let secret = crate::keychain::get_password(keychain_id).map_err(|e| {
                format!("password not in Keychain — save it via host_save_password: {e}")
            })?;
            SshAuth::Password { secret }
        }
    })
}

async fn establish_remote(
    host: Host,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<Established, String> {
    let target = SshTarget {
        hostname: host.hostname.clone(),
        port: host.port,
        user: host.user.clone(),
        jump: None,
    };
    let auth = ssh_auth(&host)?;

    let name = format!("helm-app {}", env!("CARGO_PKG_VERSION"));
    let connected = tokio::task::spawn_blocking(move || -> Result<Established, String> {
        let session = helm_ssh::connect_session(target, auth, Duration::from_secs(15), prompter)
            .map_err(|e| e.to_string())?;
        let session = Arc::new(session);

        // Shell-integration scripts (OSC 133 emitters) — idempotent.
        let install = integration::remote_install_command();
        let _ = session.run_oneshot(install);

        ensure_remote_helmd(&session)?;

        // The bridge channel. No PTY — this is a binary frame stream.
        let bridge = |session: &Arc<helm_ssh::SshSession>| -> Result<Connected, ClientError> {
            let opened = session
                .open_exec(r#"exec "$HOME/.helm/bin/helmd" stdio"#.to_string())
                .map_err(|e| ClientError::Io(std::io::Error::other(e.to_string())))?;
            connect_io(
                Box::new(opened.reader),
                Box::new(opened.writer),
                &name,
                None,
            )
        };
        let connected = match bridge(&session) {
            Err(e) if is_stale_daemon(&e) => return Err(format!(
                "incompatible remote helmd ({e}); existing sessions were left running — reconnect with the matching Helm build or stop that daemon explicitly before retrying"
            )),
            r => r.map_err(|e| e.to_string())?,
        };
        Ok(Established {
            connected,
            ssh: Some(session),
        })
    })
    .await
    .map_err(|e| format!("remote connect join: {e}"))??;

    Ok(connected)
}

/// Make sure the remote has our helmd at `~/.helm/bin/helmd` — right
/// version and protocol. The upload uses an atomic rename: a running daemon
/// keeps its mapped executable and live sessions, while the next spawn gets
/// the current binary.
fn ensure_remote_helmd(session: &Arc<helm_ssh::SshSession>) -> Result<(), String> {
    let expected = format!(
        "helmd {} (proto {})",
        env!("CARGO_PKG_VERSION"),
        helm_proto::PROTOCOL_VERSION
    );
    let installed = remote_helmd_version(session)?;
    if !should_upload_remote_helmd(&installed, &expected, helm_proto::PROTOCOL_VERSION)? {
        return Ok(());
    }

    // Not installed — check platform compatibility before shipping our binary.
    let uname = session
        .run_oneshot("uname -sm".to_string())
        .map_err(|e| e.to_string())?;
    let remote = uname.stdout.trim().to_string();
    let local_os = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        other => other,
    };
    let arch_ok = match std::env::consts::ARCH {
        "aarch64" => remote.contains("arm64") || remote.contains("aarch64"),
        "x86_64" => remote.contains("x86_64"),
        _ => false,
    };
    if !remote.starts_with(local_os) || !arch_ok {
        return Err(format!(
            "remote platform `{remote}` doesn't match this machine \
             ({local_os} {}) — cross-platform helmd bundles aren't shipped yet",
            std::env::consts::ARCH
        ));
    }

    let bin = helmd_bin_path()?;
    let bytes = std::fs::read(&bin).map_err(|e| format!("read {}: {e}", bin.display()))?;
    tracing::info!(
        "uploading helmd ({} KB) to remote (had: {installed:?})",
        bytes.len() / 1024
    );

    // Stream the raw binary through a no-PTY exec channel's stdin.
    // SSH is 8-bit clean, so no base64 detour; EOF on our side lets
    // `cat` finish, then the atomic mv swaps it in.
    let upload = session
        .open_exec(
            r#"mkdir -p "$HOME/.helm/bin" && cat > "$HOME/.helm/bin/.helmd.tmp" \
               && chmod +x "$HOME/.helm/bin/.helmd.tmp" \
               && mv "$HOME/.helm/bin/.helmd.tmp" "$HOME/.helm/bin/helmd""#
                .to_string(),
        )
        .map_err(|e| e.to_string())?;
    {
        let mut writer = upload.writer;
        writer
            .write_all(&bytes)
            .map_err(|e| format!("upload write: {e}"))?;
        writer.flush().map_err(|e| format!("upload flush: {e}"))?;
        // Drop closes the pipe → channel EOF → remote `cat` completes.
    }
    // Wait for the remote pipeline to finish (reader EOF).
    let mut reader = upload.reader;
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink);

    let verified = remote_helmd_version(session)?;
    if verified != expected {
        return Err(format!(
            "helmd upload verification failed (got {verified:?}, wanted {expected:?})"
        ));
    }
    Ok(())
}

fn should_upload_remote_helmd(
    installed: &str,
    expected: &str,
    protocol_version: u32,
) -> Result<bool, String> {
    if installed.is_empty() {
        return Ok(true);
    }
    if installed.contains(&format!("(proto {protocol_version})")) {
        return match (helmd_version(installed), helmd_version(expected)) {
            (Some(installed), Some(expected)) => Ok(installed < expected),
            _ => Ok(false),
        };
    }
    Err(format!(
        "remote has {installed}; this build requires {expected}. Existing sessions were left running — reconnect with the matching Helm build, or stop and remove the old helmd explicitly before retrying"
    ))
}

fn should_retire_daemon(daemon_version: &str) -> bool {
    match (
        Version::parse(daemon_version),
        Version::parse(env!("CARGO_PKG_VERSION")),
    ) {
        (Ok(daemon), Ok(current)) => daemon < current,
        _ => {
            tracing::warn!(%daemon_version, "leaving daemon with unparseable version running");
            false
        }
    }
}

fn helmd_version(version: &str) -> Option<Version> {
    Version::parse(version.split_whitespace().nth(1)?).ok()
}

fn request_retirement_if_empty(
    client: &helm_proto::client::HelmdClient,
    tree: &parking_lot::Mutex<TreeSnapshot>,
    capabilities: &parking_lot::RwLock<DaemonCapabilities>,
) -> bool {
    if !tree.lock().sessions.is_empty()
        || !capabilities
            .read()
            .extensions
            .contains(helm_proto::extensions::DRAIN)
    {
        return false;
    }
    match client.extension(None, helm_proto::extensions::DRAIN.into(), Vec::new()) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "failed to request empty daemon retirement");
            false
        }
    }
}

// -------------------------------------------------------------------
// Retired daemon generations
// -------------------------------------------------------------------

/// Find retired generations still serving sessions on `host`'s machine
/// and adopt each as its own ephemeral host entry, connected like any
/// other host. Ids derive from (parent, socket), so re-running on
/// every reconnect is a no-op for generations already tracked.
async fn discover_retired_daemons(host: Host, parent_entry: SharedHostEntry, deps: ConnectDeps) {
    let sockets = if host.port == 0 {
        local_retired_sockets()
    } else {
        remote_retired_sockets(&parent_entry).await
    };
    for socket in sockets {
        adopt_retired_daemon(&host, &parent_entry, socket, &deps).await;
    }
}

pub(crate) fn local_retired_sockets() -> Vec<String> {
    use std::os::unix::fs::FileTypeExt;
    let primary = helmd_socket_path();
    let stem = primary
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("helmd");
    let prefix = format!("{stem}-retired-");
    let Some(dir) = primary.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".sock") {
            continue;
        }
        if !entry.file_type().map(|t| t.is_socket()).unwrap_or(false) {
            continue;
        }
        out.push(dir.join(name).to_string_lossy().into_owned());
    }
    out.sort();
    out
}

async fn remote_retired_sockets(parent_entry: &SharedHostEntry) -> Vec<String> {
    let ssh = parent_entry.lock().await.ssh.clone();
    let Some(ssh) = ssh else {
        return Vec::new();
    };
    let listed = tokio::task::spawn_blocking(move || {
        ssh.run_oneshot(
            r#"for s in "$HOME"/.helm/helmd-retired-*.sock; do [ -S "$s" ] && echo "$s"; done; true"#
                .to_string(),
        )
    })
    .await;
    match listed {
        Ok(Ok(out)) => out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| line.ends_with(".sock"))
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

async fn adopt_retired_daemon(
    parent: &Host,
    parent_entry: &SharedHostEntry,
    socket: String,
    deps: &ConnectDeps,
) {
    let id = HostId::retired(parent.id, &socket);
    let child = Host {
        id,
        retired: Some(RetiredDaemon {
            parent: parent.id,
            socket: socket.clone(),
        }),
        ..parent.clone()
    };
    let entry = Arc::new(Mutex::new(HostEntry::new(child.clone())));
    match deps.hosts.entry(id) {
        dashmap::mapref::entry::Entry::Occupied(_) => return,
        dashmap::mapref::entry::Entry::Vacant(vacant) => {
            vacant.insert(entry.clone());
        }
    }
    emit_event(&deps.event_tx, HostEvent::HostAdded { host: child.clone() });
    tracing::info!(%socket, parent = %parent.name, "adopting retired daemon generation");

    let prompter = prompter_for(id, deps);
    if let Err(e) = do_connect(entry, id, prompter, deps.clone()).await {
        if e.contains(RETIRED_GONE) {
            // Crash leftover: the socket file exists but nothing serves
            // it. Scrub it so the next discovery doesn't re-adopt.
            tracing::info!(%socket, "retired socket is dead; scrubbing");
            remove_retired_entry(id, &child, deps);
            scrub_dead_socket(parent_entry, parent.port == 0, &socket).await;
        } else {
            tracing::warn!(%socket, error = %e, "could not attach to retired daemon");
        }
    }
}

/// Drop a retired generation's entry and tell the frontend. Called
/// when its daemon has exited (last session ended) or its socket
/// turned out to be dead.
fn remove_retired_entry(host_id: HostId, host: &Host, deps: &ConnectDeps) {
    deps.hosts.remove(&host_id);
    notifications::dismiss_for_host(&deps.notif_ctx, &deps.event_tx, host_id);
    emit_event(&deps.event_tx, HostEvent::HostRemoved { host_id });
    // A cleanly-exiting daemon unlinks its own socket; a crashed local
    // one can't, so scrub here. (Remote scrubbing needs the parent's
    // SSH session and happens at adoption time instead.)
    if host.port == 0 {
        if let Some(retired) = &host.retired {
            let _ = std::fs::remove_file(&retired.socket);
        }
    }
}

async fn scrub_dead_socket(parent_entry: &SharedHostEntry, local: bool, socket: &str) {
    if local {
        let _ = std::fs::remove_file(socket);
        return;
    }
    let ssh = parent_entry.lock().await.ssh.clone();
    if let Some(ssh) = ssh {
        let socket = socket.to_string();
        let _ =
            tokio::task::spawn_blocking(move || ssh.run_oneshot(format!("rm -f '{socket}'"))).await;
    }
}

/// `helmd --version` on the remote (empty string when not installed).
fn remote_helmd_version(session: &Arc<helm_ssh::SshSession>) -> Result<String, String> {
    session
        .run_oneshot(r#""$HOME/.helm/bin/helmd" --version 2>/dev/null || true"#.to_string())
        .map(|out| out.stdout.trim().to_string())
        .map_err(|e| e.to_string())
}

/// Build the host-key prompter for one host: bridges helm-ssh's
/// host-key callback to the frontend event channel, parking the SSH
/// connect on a oneshot until `host_key_prompt_response` answers.
pub(crate) fn prompter_for(host_id: HostId, deps: &ConnectDeps) -> Arc<dyn HostKeyPrompter> {
    Arc::new(AppHostKeyPrompter {
        host_id,
        event_tx: deps.event_tx.clone(),
        pending: deps.pending_prompts.clone(),
    })
}

struct AppHostKeyPrompter {
    host_id: HostId,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    pending: Arc<DashMap<HostId, oneshot::Sender<HostKeyDecision>>>,
}

#[async_trait::async_trait]
impl HostKeyPrompter for AppHostKeyPrompter {
    async fn prompt(
        &self,
        hostname: &str,
        port: u16,
        algorithm: &str,
        fingerprint: &str,
        kind: helm_domain::HostKeyPromptKind,
    ) -> HostKeyDecision {
        let (tx, rx) = oneshot::channel();
        // If a stale entry exists from a prior aborted attempt, drop it
        // so the new prompt is the one the response command picks up.
        self.pending.insert(self.host_id, tx);
        emit_event(
            &self.event_tx,
            HostEvent::HostKeyPrompt {
                host_id: self.host_id,
                hostname: hostname.to_string(),
                port,
                algorithm: algorithm.to_string(),
                fingerprint: fingerprint.to_string(),
                prompt: kind,
            },
        );
        match rx.await {
            Ok(decision) => decision,
            // Receiver dropped — frontend channel closed mid-prompt
            // (webview reload, app shutdown). Default to refusing.
            Err(_) => HostKeyDecision::Reject,
        }
    }
}

// -------------------------------------------------------------------
// proto → domain conversions
// -------------------------------------------------------------------

pub(crate) fn to_domain_tree(t: &TreeSnapshot) -> SessionTree {
    SessionTree {
        sessions: t
            .sessions
            .iter()
            .map(|session| SessionInfo {
                id: session.id.to_string(),
                name: session.name.clone(),
                cols: session.cols,
                rows: session.rows,
                alt_screen: session.alt_screen,
                cwd: session.cwd.clone(),
                branch: session.branch.clone(),
                root: session.root.clone(),
                command: session.command.clone(),
            })
            .collect(),
    }
}

pub(crate) fn to_domain_block(b: &helm_proto::BlockMeta) -> BlockInfo {
    BlockInfo {
        id: b.id.to_string(),
        start_line: b.start_line,
        cmd_line: b.cmd_line,
        output_line: b.output_line,
        end_line: b.end_line,
        cmdline: b.cmdline.clone(),
        cwd: b.cwd.clone(),
        branch: b.branch.clone(),
        root: b.root.clone(),
        exit_code: b.exit_code,
        started_at_ms: b.started_at_ms,
        finished_at_ms: b.finished_at_ms,
    }
}

pub(crate) fn to_domain_hits(matches: &[helm_proto::SearchMatch]) -> Vec<SearchHit> {
    matches
        .iter()
        .map(|m| SearchHit {
            session_id: m.session.to_string(),
            block_id: m.block.map(|b| b.to_string()),
            line: m.line,
            line_text: m.line_text.clone(),
            match_start: m.match_start,
            match_end: m.match_end,
        })
        .collect()
}

/// Packed-colour flag for truecolor (see `helm_domain::SpanInfo`).
/// Exported to the frontend as a binding constant.
pub const TRUECOLOR_FLAG: i32 = 1 << 24;

fn pack_color(c: helm_proto::Color) -> i32 {
    match c {
        helm_proto::Color::Default => -1,
        helm_proto::Color::Indexed(i) => i as i32,
        helm_proto::Color::Rgb(r, g, b) => {
            TRUECOLOR_FLAG | ((r as i32) << 16) | ((g as i32) << 8) | b as i32
        }
    }
}

/// Rows arrive owned from the daemon; moving their strings into the
/// domain type is the only copy on the way to the webview.
pub(crate) fn to_domain_row(r: helm_proto::Row) -> RowInfo {
    RowInfo {
        spans: r
            .spans
            .into_iter()
            .map(|s| SpanInfo {
                text: s.text,
                fg: pack_color(s.style.fg),
                bg: pack_color(s.style.bg),
                attrs: s.style.attrs,
                link: s.style.link,
            })
            .collect(),
        wrapped: r.wrapped,
    }
}

pub(crate) fn to_domain_cursor(c: &helm_proto::Cursor) -> CursorInfo {
    CursorInfo {
        row: c.row,
        col: c.col,
        visible: c.visible,
        shape: match c.shape {
            helm_proto::CursorShape::Block => CursorShape::Block,
            helm_proto::CursorShape::Underline => CursorShape::Underline,
            helm_proto::CursorShape::Beam => CursorShape::Beam,
        },
        blink: c.blink,
    }
}

pub(crate) fn to_domain_screen(s: helm_proto::Screen) -> ScreenInfo {
    ScreenInfo {
        cols: s.cols,
        rows: s.rows,
        top_line: s.top_line,
        history_start: s.history_start,
        cursor: to_domain_cursor(&s.cursor),
        modes: s.modes,
        lines: s.lines.into_iter().map(to_domain_row).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_daemon_is_anything_but_a_hello_ack() {
        let mismatch =
            ClientError::HandshakeRejected("protocol mismatch: client 4, daemon 3".into());
        assert!(is_stale_daemon(&mismatch));
        // An older daemon's `Error` frame lands on a different variant of
        // our `DaemonMsg` — this is what a 0.2.3 daemon looks like to 0.2.4.
        assert!(is_stale_daemon(&ClientError::UnexpectedHello));
        // Not a version problem: leave the daemon alone.
        assert!(!is_stale_daemon(&ClientError::HandshakeRejected(
            "busy".into()
        )));
        assert!(!is_stale_daemon(&ClientError::HandshakeClosed));
        assert!(!is_stale_daemon(&ClientError::Closed));
    }

    #[test]
    fn remote_binary_updates_are_atomic_only_within_the_wire_baseline() {
        assert_eq!(
            should_upload_remote_helmd("", "helmd 0.2.7 (proto 7)", 7),
            Ok(true)
        );
        assert_eq!(
            should_upload_remote_helmd("helmd 0.2.6 (proto 7)", "helmd 0.2.7 (proto 7)", 7),
            Ok(true)
        );
        assert_eq!(
            should_upload_remote_helmd("helmd 0.2.7 (proto 7)", "helmd 0.2.7 (proto 7)", 7),
            Ok(false)
        );
        assert_eq!(
            should_upload_remote_helmd("helmd 0.2.8 (proto 7)", "helmd 0.2.7 (proto 7)", 7),
            Ok(false)
        );
        let error = should_upload_remote_helmd("helmd 0.2.6 (proto 6)", "helmd 0.2.7 (proto 7)", 7)
            .unwrap_err();
        assert!(error.contains("Existing sessions were left running"));
    }

    #[test]
    fn daemon_retirement_is_monotonic() {
        assert!(should_retire_daemon("0.0.1"));
        assert!(!should_retire_daemon(env!("CARGO_PKG_VERSION")));
        assert!(!should_retire_daemon("999.0.0"));
        assert!(!should_retire_daemon("development"));
    }
}

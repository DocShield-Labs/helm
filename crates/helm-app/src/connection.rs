//! Per-host connection lifecycle: connect, event pump, reconnect ladder.
//!
//! One helmd connection per host — the daemon owns PTYs, scrollback,
//! and block segmentation; this module owns getting connected to it
//! and staying connected:
//!
//!   - **Local**: unix socket at `~/.helm/helmd.sock`, auto-spawning
//!     `helmd serve` from the binary next to the app executable.
//!   - **Remote**: one SSH session; the integration scripts + helmd
//!     binary are installed/upgraded on the far side, then a no-PTY
//!     exec channel runs `helmd stdio` and the same frame protocol
//!     flows over it.
//!
//! The pump task translates `DaemonMsg` → `HostEvent` for the frontend,
//! maintains the per-pane breadcrumb index, and feeds notifications.
//! The supervisor waits for the pump to die and runs the reconnect
//! ladder (`[1, 2, 4, 8, 30]s`, early-woken by reachability + system
//! wake, with a post-wake SSH liveness probe).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use helm_domain::{
    AuthMethod, BlockInfo, CursorInfo, CursorShape, Host, HostEvent, HostId, HostStatus,
    PaneInfo, RowAt, RowInfo, ScreenInfo, SearchHit, SessionEvent, SessionTree, SpanInfo,
    WindowInfo, WorkspaceInfo,
};
use helm_proto::client::{connect_io, connect_or_spawn_unix, ClientError, Connected};
use helm_proto::{DaemonMsg, TreeSnapshot};
use helm_ssh::{HostKeyPrompter, SshAuth, SshTarget};
use tokio::sync::{mpsc, oneshot, watch};

use crate::commands::emit_event;
use crate::integration;
use crate::notifications;
use crate::state::{NotificationsCtx, SessionHandle, SharedHostEntry};

/// All connect logic past the State<'_> prelude. One-shot from the
/// user's view: if the initial connect fails, the host stays in `Error`
/// state and the user re-clicks. Reconnect on transport drop is the
/// supervisor's job.
pub(crate) async fn do_connect(
    entry: SharedHostEntry,
    host_id: HostId,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    bootstrap_workspace: Option<String>,
    prompter: Arc<dyn HostKeyPrompter>,
    network_online: watch::Receiver<bool>,
    wake_signal: watch::Receiver<u64>,
    notif_ctx: NotificationsCtx,
) -> Result<(), String> {
    // Serialize connect attempts for this host (StrictMode/HMR double
    // effects, user double-clicks, supervisor reconnects).
    let connect_lock = {
        let g = entry.lock().await;
        g.connect_lock.clone()
    };
    let _connect_guard = connect_lock.lock().await;

    emit_event(&event_tx, HostEvent::Status {
        host_id,
        status: HostStatus::Connecting,
        error: None,
    });

    let host = {
        let mut guard = entry.lock().await;
        guard.shutdown_session();
        let mut h = guard.host.clone();
        if let Some(ws) = bootstrap_workspace {
            h.default_workspace = ws;
        }
        h
    };

    match connect_once(&entry, host_id, &host, &event_tx, &prompter, &notif_ctx).await {
        Ok(pump_dead) => {
            let handle = tokio::spawn(supervise(
                entry.clone(),
                host_id,
                host,
                event_tx.clone(),
                prompter,
                pump_dead,
                network_online,
                wake_signal,
                notif_ctx,
            ));
            entry.lock().await.supervisor = Some(handle.abort_handle());
            Ok(())
        }
        Err(e) => {
            entry.lock().await.status = HostStatus::Error;
            emit_event(&event_tx, HostEvent::Status {
                host_id,
                status: HostStatus::Error,
                error: Some(e.clone()),
            });
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
    event_tx: &Option<mpsc::UnboundedSender<HostEvent>>,
    prompter: &Arc<dyn HostKeyPrompter>,
    notif_ctx: &NotificationsCtx,
) -> Result<mpsc::UnboundedReceiver<()>, String> {
    let established = establish(host.clone(), Some(prompter.clone())).await?;
    let pump_dead =
        install_session(entry, host_id, host, established, event_tx.clone(), notif_ctx.clone())
            .await?;
    emit_event(event_tx, HostEvent::Status {
        host_id,
        status: HostStatus::Connected,
        error: None,
    });
    Ok(pump_dead)
}

/// A freshly-established daemon connection plus its transport backing.
struct Established {
    connected: Connected,
    ssh: Option<Arc<helm_ssh::SshSession>>,
}

/// Wire an established connection into the host entry: spawn the pump,
/// stash the `SessionHandle`, sync the pane index, deliver offline
/// notifications, and bootstrap a workspace if the daemon has none.
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
    let Connected { client, events, daemon_version, state, pending } = connected;
    tracing::info!("connected to helmd {daemon_version} on {host_id:?}");

    // Prime the breadcrumb index from the initial snapshot, then fold
    // in every notification that accumulated while we were away.
    notifications::sync_pane_index(&notif_ctx, &event_tx, host_id, &state);
    let last_note_id = pending.iter().map(|n| n.id.0).max();
    for note in pending {
        notifications::process_daemon_notification(&notif_ctx, &event_tx, host_id, &note);
    }
    let tree = Arc::new(parking_lot::Mutex::new(state));
    let pending_reqs: Arc<DashMap<u64, oneshot::Sender<DaemonMsg>>> = Arc::new(DashMap::new());

    let (dead_tx, dead_rx) = mpsc::unbounded_channel::<()>();
    let pump = tokio::spawn(pump(
        host_id,
        host.clone(),
        ssh.clone(),
        events,
        tree.clone(),
        pending_reqs.clone(),
        event_tx.clone(),
        notif_ctx.clone(),
        dead_tx,
    ));

    let session = Arc::new(SessionHandle::new(
        client,
        pump.abort_handle(),
        tree,
        pending_reqs,
    ));

    // We've shown the offline notifications; let the daemon drop them.
    if let Some(max) = last_note_id {
        let _ = session.client.ack_notifications(helm_proto::NotificationId(max));
    }
    let _ = session.client.attach();

    // A daemon with no workspaces gets one, named after the host's
    // default. Await the reply so the first tree the frontend renders
    // already has it (the pump's TreeChanged updates `tree`).
    if session.tree.lock().workspaces.is_empty() {
        let name = Some(host.default_workspace.clone());
        if let Err(e) = session.request(|id| session.client.new_workspace(id, name)).await {
            tracing::warn!("bootstrap workspace on {host_id:?}: {e}");
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
    emit_event(&event_tx, HostEvent::Session {
        host_id,
        event: SessionEvent::Tree { tree: initial },
    });

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
            DaemonMsg::Screen { req_id: None, pane, screen } => {
                emit(SessionEvent::Screen {
                    pane_id: pane.to_string(),
                    screen: to_domain_screen(screen),
                });
            }
            DaemonMsg::ScreenDiff { pane, top_line, scroll, rows, cursor, modes } => {
                emit(SessionEvent::ScreenDiff {
                    pane_id: pane.to_string(),
                    top_line,
                    scroll,
                    rows: rows
                        .into_iter()
                        .map(|(index, row)| RowAt { index, row: to_domain_row(row) })
                        .collect(),
                    cursor: to_domain_cursor(&cursor),
                    modes,
                });
            }
            DaemonMsg::HistoryAppend { pane, first_line, rows } => {
                emit(SessionEvent::HistoryAppend {
                    pane_id: pane.to_string(),
                    first_line,
                    rows: rows.into_iter().map(to_domain_row).collect(),
                });
            }
            DaemonMsg::Block { pane, block } => {
                let pane_id = pane.to_string();
                crate::tool_integrations::detect_from_block(
                    &notif_ctx.tool_integration_seen,
                    &event_tx,
                    &host,
                    ssh.clone(),
                    host_id,
                    &block,
                );
                emit(SessionEvent::Block { pane_id, block: to_domain_block(&block) });
            }
            DaemonMsg::ModeChange { pane, alt_screen } => {
                emit(SessionEvent::ModeChange { pane_id: pane.to_string(), alt_screen });
            }
            DaemonMsg::TreeChanged { state } => {
                *tree.lock() = state.clone();
                notifications::sync_pane_index(&notif_ctx, &event_tx, host_id, &state);
                emit(SessionEvent::Tree { tree: to_domain_tree(&state) });
            }
            DaemonMsg::PaneExited { pane, status } => {
                emit(SessionEvent::PaneExited { pane_id: pane.to_string(), status });
            }
            DaemonMsg::Notification { note } => {
                if matches!(note.kind, helm_proto::NotificationKind::Bell) {
                    emit(SessionEvent::Bell { pane_id: note.pane.to_string() });
                }
                notifications::process_daemon_notification(&notif_ctx, &event_tx, host_id, &note);
            }
            DaemonMsg::Error { req_id: None, context, message } => {
                tracing::warn!("helmd error on {host_id:?} ({context}): {message}");
            }
            DaemonMsg::HelloAck { .. } => {
                tracing::debug!("unexpected HelloAck mid-stream on {host_id:?}");
            }
            // Replies (Created / SearchResults / Blocks / Pong / Error with
            // a req_id) go to whoever is waiting; an unclaimed reply means
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
async fn supervise(
    entry: SharedHostEntry,
    host_id: HostId,
    host: Host,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    prompter: Arc<dyn HostKeyPrompter>,
    mut pump_dead: mpsc::UnboundedReceiver<()>,
    mut network_online: watch::Receiver<bool>,
    mut wake_signal: watch::Receiver<u64>,
    notif_ctx: NotificationsCtx,
) {
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
        emit_event(&event_tx, HostEvent::Status {
            host_id,
            status: HostStatus::Reconnecting,
            error: last_error.clone(),
        });

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
        match connect_once(&entry, host_id, &host, &event_tx, &prompter, &notif_ctx).await {
            Ok(new_dead) => {
                pump_dead = new_dead;
                attempt = 0;
                last_error = None;
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
    emit_event(event_tx, HostEvent::Status {
        host_id,
        status: HostStatus::Disconnected,
        error: None,
    });
    notifications::dismiss_for_host(notif_ctx, event_tx, host_id);
    true
}

// -------------------------------------------------------------------
// Transport establishment
// -------------------------------------------------------------------

async fn establish(
    host: Host,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<Established, String> {
    if host.port == 0 {
        return establish_local().await;
    }
    establish_remote(host, prompter).await
}

async fn establish_local() -> Result<Established, String> {
    let bin = helmd_bin_path()?;
    let socket = helmd_socket_path();
    let name = format!("helm-app {}", env!("CARGO_PKG_VERSION"));
    let connected = tokio::task::spawn_blocking(move || {
        match connect_or_spawn_unix(&socket, &bin, &name) {
            // A daemon from another build owns the socket. Its sessions
            // are unreachable to us either way; replace it with ours.
            Err(ClientError::HandshakeRejected(m)) if is_protocol_mismatch(&m) => {
                tracing::warn!("local helmd is stale ({m}); restarting it");
                helm_proto::shutdown_socket(&socket)?;
                connect_or_spawn_unix(&socket, &bin, &name)
            }
            r => r,
        }
    })
    .await
    .map_err(|e| format!("connect join: {e}"))?
    .map_err(|e| e.to_string())?;
    Ok(Established { connected, ssh: None })
}

fn is_protocol_mismatch(rejection: &str) -> bool {
    rejection.contains("protocol mismatch")
}

/// The local daemon's socket: `~/.helm/helmd.sock`, or `$HELM_SOCKET`.
/// The override lets a dev build run against its own daemon while the
/// installed app keeps its sessions on the default socket — a protocol
/// bump would otherwise restart the shared daemon on first connect.
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
    let auth = match host.auth.clone() {
        AuthMethod::Agent => SshAuth::Agent,
        AuthMethod::KeyFile { path } => SshAuth::KeyFile {
            path: PathBuf::from(path),
            passphrase: None,
        },
        AuthMethod::Password => {
            let secret = crate::keychain::get_password(host.id).map_err(|e| {
                format!("password not in Keychain — save it via host_save_password: {e}")
            })?;
            SshAuth::Password { secret }
        }
    };

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
        // A daemon left running from an older binary rejects our hello;
        // stop it and let the bridge spawn the current one.
        let bridge = |session: &Arc<helm_ssh::SshSession>| -> Result<Connected, ClientError> {
            let opened = session
                .open_exec(r#"exec "$HOME/.helm/bin/helmd" stdio"#.to_string())
                .map_err(|e| ClientError::Io(std::io::Error::other(e.to_string())))?;
            connect_io(Box::new(opened.reader), Box::new(opened.writer), &name, None)
        };
        let connected = match bridge(&session) {
            Err(ClientError::HandshakeRejected(m)) if is_protocol_mismatch(&m) => {
                tracing::warn!("remote helmd is stale ({m}); restarting it");
                shutdown_remote_helmd(&session)?;
                bridge(&session).map_err(|e| e.to_string())?
            }
            r => r.map_err(|e| e.to_string())?,
        };
        Ok(Established { connected, ssh: Some(session) })
    })
    .await
    .map_err(|e| format!("remote connect join: {e}"))??;

    Ok(connected)
}

/// Make sure the remote has our helmd at `~/.helm/bin/helmd` — right
/// version, right protocol. Uploads our own binary when missing or
/// stale, which requires matching OS/arch (cross-arch bundles land
/// later; helm's fleet is assumed homogeneous until then).
fn ensure_remote_helmd(session: &Arc<helm_ssh::SshSession>) -> Result<(), String> {
    let expected = format!(
        "helmd {} (proto {})",
        env!("CARGO_PKG_VERSION"),
        helm_proto::PROTOCOL_VERSION
    );
    let installed = remote_helmd_version(session)?;
    if installed == expected {
        return Ok(());
    }

    // Version mismatch or not installed — check platform compatibility
    // before shipping our own binary.
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
    tracing::info!("uploading helmd ({} KB) to remote (had: {installed:?})", bytes.len() / 1024);

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
        writer.write_all(&bytes).map_err(|e| format!("upload write: {e}"))?;
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
    // The binary changed under whatever daemon is running; the next
    // `stdio` bridge spawns the new one once the old has exited.
    if !installed.is_empty() {
        shutdown_remote_helmd(session)?;
    }
    Ok(())
}

/// Stop the daemon on the remote (`helmd shutdown`, which also pkills
/// daemons too old to take the frame).
fn shutdown_remote_helmd(session: &Arc<helm_ssh::SshSession>) -> Result<(), String> {
    session
        .run_oneshot(r#""$HOME/.helm/bin/helmd" shutdown 2>/dev/null || pkill -f "helmd serve" || true"#.to_string())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `helmd --version` on the remote (empty string when not installed).
fn remote_helmd_version(session: &Arc<helm_ssh::SshSession>) -> Result<String, String> {
    session
        .run_oneshot(r#""$HOME/.helm/bin/helmd" --version 2>/dev/null || true"#.to_string())
        .map(|out| out.stdout.trim().to_string())
        .map_err(|e| e.to_string())
}

// -------------------------------------------------------------------
// proto → domain conversions
// -------------------------------------------------------------------

pub(crate) fn to_domain_tree(t: &TreeSnapshot) -> SessionTree {
    SessionTree {
        workspaces: t
            .workspaces
            .iter()
            .map(|ws| WorkspaceInfo {
                id: ws.id.to_string(),
                name: ws.name.clone(),
                windows: ws
                    .windows
                    .iter()
                    .map(|w| WindowInfo {
                        id: w.id.to_string(),
                        name: w.name.clone(),
                        panes: w
                            .panes
                            .iter()
                            .map(|p| PaneInfo {
                                id: p.id.to_string(),
                                cols: p.cols,
                                rows: p.rows,
                                alt_screen: p.alt_screen,
                                cwd: p.cwd.clone(),
                                branch: p.branch.clone(),
                                command: p.command.clone(),
                            })
                            .collect(),
                    })
                    .collect(),
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
        exit_code: b.exit_code,
        started_at_ms: b.started_at_ms,
        finished_at_ms: b.finished_at_ms,
    }
}

pub(crate) fn to_domain_hits(matches: &[helm_proto::SearchMatch]) -> Vec<SearchHit> {
    matches
        .iter()
        .map(|m| SearchHit {
            pane_id: m.pane.to_string(),
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

//! Per-host connection lifecycle: bootstrap, supervisor, per-session
//! forwarders, reconnect ladder, incremental session spawn on
//! `%sessions-changed`.
//!
//! This module owns everything between "user clicked connect" and the
//! per-session control clients streaming notifications onto the global
//! event channel. Tauri commands in `commands::host` wrap the entry
//! point [`do_connect`]; the rest is internal.
//!
//! Multi-client model:
//!   - One `tmux -CC attach -t $session` per session on the host.
//!   - Each client owns its own forwarder task that pumps
//!     `TmuxNotification`s into the global event channel and signals
//!     the host supervisor on transport close + `%sessions-changed`.
//!   - The supervisor coalesces signals, promotes a new primary on
//!     individual deaths, and runs the reconnect ladder when every
//!     client has died.
//!   - For remote hosts, a single `SshSession` multiplexes channels
//!     for every per-session control client + any new sessions spawned
//!     mid-flight.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use helm_domain::{AuthMethod, Host, HostEvent, HostId, HostStatus, TmuxNotification};
use helm_ssh::{HostKeyPrompter, SshAuth, SshTarget};
use helm_tmux::TmuxClient;
use tokio::sync::{mpsc, watch};

use crate::commands::emit_event;
use crate::integration;
use crate::notifications;
use crate::state::{NotificationsCtx, SharedHostEntry, SupervisorSignal};

/// All connect logic past the State<'_> prelude. Pulled out of
/// `host_connect` so the `#[tauri::command]` Send bound only has to
/// inspect a small, owned-data future. Carrying `State<'_>` across
/// the SSH connect await trips on HRTB inference inside the macro.
///
/// One-shot from the user's view: if the initial connect fails, the
/// host stays in `Error` state and the user re-clicks. Reconnect on
/// transport drop is the supervisor's job, spawned only once we've
/// successfully connected at least once — initial-connect failures are
/// almost always permanent (auth rejected, host unreachable, key
/// rejected) and silently retrying them spams prompts.
pub(crate) async fn do_connect(
    entry: SharedHostEntry,
    host_id: HostId,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    bootstrap_workspace: Option<String>,
    prompter: Arc<dyn HostKeyPrompter>,
    network_online: watch::Receiver<bool>,
    notif_ctx: NotificationsCtx,
) -> Result<(), String> {
    // Serialize connect attempts for this host. Held for the full
    // function so a concurrent `host_connect` (StrictMode, HMR,
    // host_added re-fire) can't race the long async connect work and
    // leave its just-spawned `-CC` clients silently overwritten by the
    // first caller's. See `HostEntry::connect_lock`.
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

    // Tear down any prior connection state cleanly before starting a
    // fresh one. `shutdown_clients` aborts every per-session forwarder,
    // drops every TmuxClient (each runs cleanup → kills its `-CC`
    // process / drops its SSH channel), and clears the SSH session.
    let host = {
        let mut guard = entry.lock().await;
        guard.shutdown_clients();
        let mut h = guard.host.clone();
        if let Some(ws) = bootstrap_workspace {
            h.default_workspace = ws;
        }
        h
    };

    match connect_host_multi(host.clone(), Some(prompter.clone())).await {
        Ok(connected) => {
            let ssh = connected.ssh.clone();
            let ssh_for_detect = ssh.clone();
            let primary_id = connected.primary_session_id.clone();

            // Channel for per-client forwarders to signal back to the
            // host supervisor (deaths + sessions-changed events).
            let (sup_tx, sup_rx) = mpsc::unbounded_channel::<SupervisorSignal>();

            // Build SessionClient entries + spawn one forwarder per
            // attached session. Each forwarder pumps its TmuxNotification
            // receiver into the global HostEvent channel and signals
            // the supervisor on transport close / sessions-changed.
            let mut session_clients: HashMap<String, Arc<crate::state::SessionClient>> =
                HashMap::new();
            let mut primary_client: Option<Arc<TmuxClient>> = None;
            for (session_id, tmux, events) in connected.clients {
                if session_id == primary_id {
                    primary_client = Some(tmux.clone());
                }
                let (sid, client) = spawn_session_client(
                    host_id,
                    session_id,
                    tmux,
                    events,
                    event_tx.clone(),
                    notif_ctx.clone(),
                    sup_tx.clone(),
                    entry.clone(),
                );
                session_clients.insert(sid, client);
            }

            // Stash everything into the HostEntry. Status flips to
            // Connected here so the frontend renders the tree as soon
            // as we're done bootstrapping (refresh_pane_index runs next
            // and only refines the per-pane breadcrumbs).
            //
            // `connect_lock` should mean the map is empty here, but
            // tear down any straggler defensively — a plain `=` would
            // drop SessionClient Arcs without calling
            // `forwarder.abort()`, leaking the old forwarder + its
            // attached `-CC` PTY into the global event channel.
            {
                let mut guard = entry.lock().await;
                let stragglers =
                    std::mem::replace(&mut guard.clients, session_clients);
                for (_, c) in stragglers {
                    c.forwarder.abort();
                    drop(c);
                }
                guard.primary_session_id = Some(primary_id.clone());
                guard.ssh = ssh;
                guard.supervisor_tx = Some(sup_tx.clone());
                guard.status = HostStatus::Connected;
            }

            // Configure tmux's server env so shells started from now on
            // pick up our integration. Local-only for the ZDOTDIR path:
            // the wrapper directory points at the user's home on the
            // local machine. For remote hosts the equivalent env vars
            // are exported by the per-channel attach commands (see
            // `connect_remote_multi`).
            //
            // Best-effort: integration is a layer on top of bell
            // detection, not a prerequisite for anything else.
            if host.port == 0 {
                if let (Some(home), Some(ref pc)) =
                    (dirs::home_dir(), primary_client.as_ref())
                {
                    // Read HELM_USER_ZDOTDIR (the user's ORIGINAL ZDOTDIR
                    // captured at boot in `lib.rs`) — NOT $ZDOTDIR. The
                    // latter has already been clobbered by lib.rs to
                    // point at our integration wrapper, so re-reading it
                    // here would propagate the wrapper path into tmux's
                    // server-global HELM_USER_ZDOTDIR. New shells would
                    // then set ZDOTDIR back to the wrapper path and
                    // `source $ZDOTDIR/.zshrc` would recursively source
                    // our wrapper itself ("job table full or recursion
                    // limit exceeded").
                    let user_zdotdir = std::env::var("HELM_USER_ZDOTDIR")
                        .unwrap_or_else(|_| home.to_string_lossy().into_owned());
                    if let Err(e) =
                        integration::configure_tmux_env(pc, &home, &user_zdotdir).await
                    {
                        tracing::warn!("configure_tmux_env failed: {e}");
                    }
                }
            }

            // Bootstrap the pane→window/session index so notifications
            // emitted in the next few moments can carry breadcrumbs
            // before the frontend's own refetchTree finishes. Then
            // sweep for any tool-integration suggestions worth
            // surfacing on this connect.
            if let Some(ref pc) = primary_client {
                let _ = notifications::refresh_pane_index(
                    &notif_ctx,
                    &event_tx,
                    pc,
                    host_id,
                )
                .await;
                crate::tool_integrations::detect_and_suggest(
                    &notif_ctx.tool_integration_seen,
                    &event_tx,
                    pc,
                    ssh_for_detect.as_ref(),
                    &host,
                    host_id,
                )
                .await;
            }

            // Spawn the host supervisor: handles reconnect ladder,
            // incremental session spawn (on %sessions-changed), and
            // the death-signal mpsc above.
            let handle = tokio::spawn(supervise(
                entry.clone(),
                host_id,
                host,
                event_tx.clone(),
                prompter,
                sup_rx,
                network_online,
                notif_ctx,
            ));
            {
                let mut guard = entry.lock().await;
                guard.supervisor = Some(handle.abort_handle());
            }

            emit_event(&event_tx, HostEvent::Status {
                host_id,
                status: HostStatus::Connected,
                error: None,
            });
            Ok(())
        }
        Err(e) => {
            {
                let mut guard = entry.lock().await;
                guard.status = HostStatus::Error;
            }
            emit_event(&event_tx, HostEvent::Status {
                host_id,
                status: HostStatus::Error,
                error: Some(e.clone()),
            });
            Err(e)
        }
    }
}

/// Spawn a per-client forwarder task and bundle the resulting state
/// into a `SessionClient`. Returns `(session_id, Arc<SessionClient>)`
/// — the caller decides whether to insert into a fresh `HashMap`
/// (do_connect / supervise reconnect) or into the live
/// `entry.clients` under lock (`spawn_missing_clients`).
///
/// Lives next to `client_forwarder_loop` because the two parameters
/// move in lockstep — every spawn site needs both, and any new field
/// on `SessionClient` lands in this one helper rather than three
/// call sites.
fn spawn_session_client(
    host_id: HostId,
    session_id: String,
    tmux: Arc<TmuxClient>,
    events: mpsc::UnboundedReceiver<TmuxNotification>,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    notif_ctx: NotificationsCtx,
    sup_tx: mpsc::UnboundedSender<SupervisorSignal>,
    entry: SharedHostEntry,
) -> (String, Arc<crate::state::SessionClient>) {
    let forwarder = tokio::spawn(client_forwarder_loop(
        host_id,
        session_id.clone(),
        events,
        event_tx,
        notif_ctx,
        sup_tx,
        entry,
    ));
    let client = Arc::new(crate::state::SessionClient {
        tmux,
        forwarder: forwarder.abort_handle(),
    });
    (session_id, client)
}

/// Per-client forwarder. Drains the TmuxNotification receiver for one
/// attached session, forwards each notification to the global event
/// channel, post-processes `%output` markers, and signals the host
/// supervisor on transport close + `%sessions-changed`.
///
/// Exits naturally when the receiver closes (transport EOF / `%exit`
/// for this session). The supervisor handles the corresponding
/// `clients` map mutation.
async fn client_forwarder_loop(
    host_id: HostId,
    session_id: String,
    mut events: mpsc::UnboundedReceiver<TmuxNotification>,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    notif_ctx: NotificationsCtx,
    sup_tx: mpsc::UnboundedSender<SupervisorSignal>,
    entry: SharedHostEntry,
) {
    while let Some(n) = events.recv().await {
        let is_exit = matches!(n, TmuxNotification::Exit { .. });

        if let TmuxNotification::Output {
            pane_id,
            bytes,
            markers,
        } = &n
        {
            notifications::process_output(
                &notif_ctx,
                &event_tx,
                host_id,
                pane_id,
                bytes,
                markers,
            );
        }

        let trigger_index_refresh = matches!(
            n,
            TmuxNotification::WindowAdded { .. }
                | TmuxNotification::WindowClosed { .. }
                | TmuxNotification::LayoutChanged { .. }
                | TmuxNotification::SessionsChanged
        );
        let trigger_session_spawn = matches!(n, TmuxNotification::SessionsChanged);

        if let Some(ref tx) = event_tx {
            if tx
                .send(HostEvent::Tmux {
                    host_id,
                    notification: n,
                })
                .is_err()
            {
                break;
            }
        }

        if trigger_index_refresh {
            // Refresh through whichever client is currently primary
            // — the primary may have changed since we spawned (other
            // session was killed, etc.). Same client serves the tool-
            // integration sweep that piggy-backs on this refresh — both
            // are read-only enumerations that benefit from running on
            // the freshest tree.
            let ctx = notif_ctx.clone();
            let tx = event_tx.clone();
            let entry = entry.clone();
            tokio::spawn(async move {
                let (client, ssh, host) = {
                    let g = entry.lock().await;
                    (g.primary_client(), g.ssh.clone(), g.host.clone())
                };
                if let Some(client) = client {
                    let _ = notifications::refresh_pane_index(
                        &ctx, &tx, &client, host_id,
                    )
                    .await;
                    crate::tool_integrations::detect_and_suggest(
                        &ctx.tool_integration_seen,
                        &tx,
                        &client,
                        ssh.as_ref(),
                        &host,
                        host_id,
                    )
                    .await;
                }
            });
        }

        if trigger_session_spawn {
            // Best-effort signal — the supervisor coalesces multiple
            // signals into one re-enumeration.
            let _ = sup_tx.send(SupervisorSignal::SessionsChanged);
        }

        if is_exit {
            break;
        }
    }
    // Either the channel closed (transport drop) or we hit %exit.
    // Either way, signal the supervisor that this client is gone.
    let _ = sup_tx.send(SupervisorSignal::ClientDied(session_id));
}

/// Host supervisor — handles per-client deaths, incremental session
/// spawn (on `%sessions-changed`), and the reconnect ladder when every
/// client has died.
///
/// Shape: drains the supervisor-signal mpsc fed by per-client
/// forwarders. ClientDied removes the entry from `clients` and either
/// promotes a new primary (other clients alive) or triggers full
/// reconnect (none alive). SessionsChanged re-enumerates sessions on
/// the host and spawns clients for any newcomers.
///
/// Reconnect ladder: `[1, 2, 4, 8, 30]s` clamped to 30s, with the
/// reachability watch as an early-wake to skip the rest of the
/// current bucket on a `false → true` transition. Applies uniformly
/// to local and remote — local tmux death (server killed, dev
/// `pkill`, OS reaping) re-runs `bootstrap_local` + spawns fresh
/// clients on the next attempt.
async fn supervise(
    entry: SharedHostEntry,
    host_id: HostId,
    host: Host,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    prompter: Arc<dyn HostKeyPrompter>,
    mut sup_rx: mpsc::UnboundedReceiver<SupervisorSignal>,
    mut network_online: watch::Receiver<bool>,
    notif_ctx: NotificationsCtx,
) {
    const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 30];
    let mut attempt = 0u32;
    let mut last_error: Option<String> = None;

    loop {
        // ----- handle signals from forwarders until everyone's dead -----
        while let Some(signal) = sup_rx.recv().await {
            match signal {
                SupervisorSignal::ClientDied(session_id) => {
                    let (now_empty, was_primary) = {
                        let mut guard = entry.lock().await;
                        let removed = guard.clients.remove(&session_id);
                        let was_primary = guard.primary_session_id.as_deref()
                            == Some(session_id.as_str());
                        if was_primary {
                            // Promote any remaining client as primary
                            // so global commands keep routing somewhere.
                            guard.primary_session_id =
                                guard.clients.keys().next().cloned();
                        }
                        // Drop after releasing the guard would invalidate
                        // — drop here to release the AbortHandle (the
                        // forwarder task has already ended).
                        drop(removed);
                        (guard.clients.is_empty(), was_primary)
                    };
                    if was_primary {
                        tracing::info!(
                            "supervisor: primary client died for {session_id} on {host_id:?}; promoted next"
                        );
                    }
                    if now_empty {
                        tracing::info!(
                            "supervisor: every client died on {host_id:?}; entering reconnect"
                        );
                        break;
                    }
                    // Else: this was a single-session death (workspace
                    // killed externally). Other sessions stay live.
                    // Don't trigger a full reconnect — but DO emit a
                    // SessionsChanged-equivalent so the frontend's tree
                    // refetches and sees the workspace gone.
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(HostEvent::Tmux {
                            host_id,
                            notification: TmuxNotification::SessionsChanged,
                        });
                    }
                }
                SupervisorSignal::SessionsChanged => {
                    if let Err(e) = spawn_missing_clients(
                        &entry,
                        host_id,
                        &event_tx,
                        &notif_ctx,
                    )
                    .await
                    {
                        tracing::warn!(
                            "supervisor: spawn_missing_clients failed for {host_id:?}: {e}"
                        );
                    }
                }
            }
        }

        // ----- all clients dead OR sup_rx closed; decide what next -----
        let voluntary = entry.lock().await.voluntary_disconnect;
        if voluntary {
            let mut guard = entry.lock().await;
            guard.shutdown_clients();
            guard.status = HostStatus::Disconnected;
            guard.supervisor = None;
            emit_event(&event_tx, HostEvent::Status {
                host_id,
                status: HostStatus::Disconnected,
                error: None,
            });
            notifications::dismiss_for_host(&notif_ctx, &event_tx, host_id);
            return;
        }

        // Surface "Reconnecting" before the sleep so the UI overlays
        // immediately, not after the backoff window.
        {
            let mut guard = entry.lock().await;
            guard.shutdown_clients();
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
                    tracing::debug!("reachability woke supervisor for {host_id:?}; resetting backoff");
                    attempt = 0;
                    continue;
                }
            }
        }

        let voluntary = entry.lock().await.voluntary_disconnect;
        if voluntary {
            let mut guard = entry.lock().await;
            guard.status = HostStatus::Disconnected;
            guard.supervisor = None;
            emit_event(&event_tx, HostEvent::Status {
                host_id,
                status: HostStatus::Disconnected,
                error: None,
            });
            notifications::dismiss_for_host(&notif_ctx, &event_tx, host_id);
            return;
        }

        // ----- attempt full reconnect (multi-client) -----
        // Hold connect_lock for the attempt so a concurrent
        // `host_connect` Tauri call can't interleave its own
        // connect_host_multi with ours and end up overwriting one
        // set of fresh `-CC` clients with another. The lock is
        // dropped at the end of the match block.
        let connect_lock = entry.lock().await.connect_lock.clone();
        let _reconnect_guard = connect_lock.lock().await;
        match connect_host_multi(host.clone(), Some(prompter.clone())).await {
            Ok(connected) => {
                let primary_id = connected.primary_session_id.clone();
                let ssh = connected.ssh.clone();
                let ssh_for_detect = ssh.clone();

                // Replace sup_rx with a fresh one — old forwarders are
                // gone, new ones get a new sender.
                let (sup_tx_new, sup_rx_new) =
                    mpsc::unbounded_channel::<SupervisorSignal>();
                sup_rx = sup_rx_new;

                let mut session_clients: HashMap<String, Arc<crate::state::SessionClient>> =
                    HashMap::new();
                let mut primary_client: Option<Arc<TmuxClient>> = None;
                for (session_id, tmux, events) in connected.clients {
                    if session_id == primary_id {
                        primary_client = Some(tmux.clone());
                    }
                    let (sid, client) = spawn_session_client(
                        host_id,
                        session_id,
                        tmux,
                        events,
                        event_tx.clone(),
                        notif_ctx.clone(),
                        sup_tx_new.clone(),
                        entry.clone(),
                    );
                    session_clients.insert(sid, client);
                }
                {
                    let mut guard = entry.lock().await;
                    let stragglers =
                        std::mem::replace(&mut guard.clients, session_clients);
                    for (_, c) in stragglers {
                        c.forwarder.abort();
                        drop(c);
                    }
                    guard.primary_session_id = Some(primary_id);
                    guard.ssh = ssh;
                    guard.supervisor_tx = Some(sup_tx_new.clone());
                    guard.status = HostStatus::Connected;
                }
                if let Some(ref pc) = primary_client {
                    let _ = notifications::refresh_pane_index(
                        &notif_ctx,
                        &event_tx,
                        pc,
                        host_id,
                    )
                    .await;
                    crate::tool_integrations::detect_and_suggest(
                        &notif_ctx.tool_integration_seen,
                        &event_tx,
                        pc,
                        ssh_for_detect.as_ref(),
                        &host,
                        host_id,
                    )
                    .await;
                }
                emit_event(&event_tx, HostEvent::Status {
                    host_id,
                    status: HostStatus::Connected,
                    error: None,
                });
                attempt = 0;
                last_error = None;
            }
            Err(e) => {
                tracing::warn!("reconnect attempt {attempt} for {host_id:?} failed: {e}");
                last_error = Some(e);
                attempt = attempt.saturating_add(1);
                continue;
            }
        }
    }
}

/// Walk the host's tmux server, find any sessions that don't have a
/// control client attached yet, and spawn one for each. Idempotent —
/// safe to call repeatedly without dupes (we check `clients` before
/// spawning).
///
/// Triggered by `%sessions-changed` notifications. The frontend creates
/// a new workspace via `tmux_new_session`; tmux fires
/// `%sessions-changed` on every client; one of those forwarders signals
/// the supervisor; supervisor calls this. Other forwarders' signals
/// are picked up immediately after but find nothing missing.
async fn spawn_missing_clients(
    entry: &SharedHostEntry,
    host_id: HostId,
    event_tx: &Option<mpsc::UnboundedSender<HostEvent>>,
    notif_ctx: &NotificationsCtx,
) -> Result<(), String> {
    // Snapshot current state outside the long async work.
    let (existing, primary, ssh, host, sup_tx) = {
        let g = entry.lock().await;
        let existing: std::collections::HashSet<String> = g.clients.keys().cloned().collect();
        let primary = g.primary_client();
        let ssh = g.ssh.clone();
        let host = g.host.clone();
        let sup_tx = g.supervisor_tx.clone();
        (existing, primary, ssh, host, sup_tx)
    };
    let Some(primary) = primary else {
        // Lost the primary mid-flight; the death-handler branch will
        // pick this up.
        return Ok(());
    };
    let Some(sup_tx) = sup_tx else {
        // Supervisor signal channel isn't set up yet (we're between
        // teardown and the next supervise spawn). Newly spawned
        // clients would be orphaned without it. Skip; the next
        // SessionsChanged signal after the supervisor restarts will
        // reach us with sup_tx populated.
        return Ok(());
    };

    // Enumerate via the existing primary client — cheap one-shot.
    let raw = primary
        .list_sessions("#{session_id}")
        .await
        .map_err(|e| e.to_string())?;
    let live_sessions: Vec<String> = raw
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let missing: Vec<String> = live_sessions
        .into_iter()
        .filter(|sid| !existing.contains(sid))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    tracing::info!(
        "supervisor: spawning {} new control client(s) on {host_id:?}",
        missing.len()
    );

    for sid in missing {
        let opened: Result<(Arc<TmuxClient>, mpsc::UnboundedReceiver<TmuxNotification>), String> =
            if host.port == 0 {
                helm_tmux::TmuxClient::spawn_attach_local(&sid)
                    .await
                    .map(|(c, ev)| (Arc::new(c), ev))
                    .map_err(|e| e.to_string())
            } else {
                let Some(ref ssh_session) = ssh else {
                    return Err("remote host missing SshSession".into());
                };
                open_remote_tmux_client(ssh_session, remote_attach_command(&sid)).await
            };
        let (tmux, events) = match opened {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "supervisor: failed to spawn client for new session {sid}: {e}"
                );
                continue;
            }
        };
        let (sid, client) = spawn_session_client(
            host_id,
            sid,
            tmux,
            events,
            event_tx.clone(),
            notif_ctx.clone(),
            sup_tx.clone(),
            entry.clone(),
        );
        let mut guard = entry.lock().await;
        guard.clients.insert(sid, client);
    }
    Ok(())
}

/// Output of the multi-client connect path. Each tuple is one tmux
/// session and its freshly-spawned control client + notifications
/// receiver. `do_connect` / `supervise` wrap each into a
/// `SessionClient` and spawn its forwarder.
struct ConnectedHost {
    clients: Vec<(String, Arc<TmuxClient>, mpsc::UnboundedReceiver<TmuxNotification>)>,
    /// First session listed. Used as the "primary" routing target for
    /// commands that don't care which session they go through (every
    /// existing tmux command, since pane/window/session ids are
    /// server-globally unique).
    primary_session_id: String,
    /// Shared SSH session for remote hosts; None for local. Subsequent
    /// `spawn_missing_clients` opens additional channels on this same
    /// session.
    ssh: Option<Arc<helm_ssh::SshSession>>,
}

/// Branch on host shape. Localhost: enumerate sessions via the local
/// tmux binary, spawn one PTY-backed control client per session.
/// Remote: open one SSH session, exec a setup channel that bootstraps
/// the workspace + attaches the first control client, then open
/// additional channels on the same SSH session for the other sessions
/// already on the remote tmux server.
async fn connect_host_multi(
    host: Host,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<ConnectedHost, String> {
    if host.port == 0 {
        return connect_local_multi(host).await;
    }
    connect_remote_multi(host, prompter).await
}

async fn connect_local_multi(host: Host) -> Result<ConnectedHost, String> {
    let dw = host.default_workspace.clone();
    let sessions = tokio::task::spawn_blocking(move || {
        helm_tmux::TmuxClient::bootstrap_local(&dw)
    })
    .await
    .map_err(|e| format!("bootstrap join: {e}"))?
    .map_err(|e| e.to_string())?;
    if sessions.is_empty() {
        return Err("bootstrap returned no sessions".into());
    }
    let primary = sessions[0].clone();
    let mut clients = Vec::new();
    for sid in sessions {
        let (client, events) = helm_tmux::TmuxClient::spawn_attach_local(&sid)
            .await
            .map_err(|e| e.to_string())?;
        clients.push((sid, Arc::new(client), events));
    }
    Ok(ConnectedHost {
        clients,
        primary_session_id: primary,
        ssh: None,
    })
}

async fn connect_remote_multi(
    host: Host,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<ConnectedHost, String> {
    let target = SshTarget {
        hostname: host.hostname.clone(),
        port: host.port,
        user: host.user.clone(),
        jump: None, // jump-host UI lands in stage D
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

    // Open the SSH session (TCP + auth, no exec yet) — synchronous
    // from this side; helm-ssh owns its own dedicated runtime thread.
    // `spawn_blocking` so the negotiation doesn't tie up the Tauri
    // command thread.
    let session = tokio::task::spawn_blocking(move || {
        helm_ssh::connect_session(target, auth, Duration::from_secs(15), prompter)
    })
    .await
    .map_err(|e| format!("ssh task: {e}"))?
    .map_err(|e| e.to_string())?;
    let session = Arc::new(session);

    // First control client: install integration, bootstrap a tmux
    // session if none exist, **inject the integration env vars into
    // tmux server-globally + per-session**, then attach. The
    // set-environment block is what was missing — without it, panes
    // opened inside tmux (now or later) inherit the session's
    // pre-helm env and our shell hooks' `[ -z "$HELM_INTEGRATION" ]`
    // gate skips the OSC 133 emitters. Existing pre-helm shells in
    // already-running panes can't be retro-fitted (their env is
    // fixed); the user opens a new window (Cmd+T) to pick up the
    // integration. Documented as a known sharp edge.
    let workspace = host.default_workspace.replace('\'', "'\\''");
    let install = integration::remote_install_command();
    let first_command = format!(
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$HOME/homebrew/bin:$PATH\"; \
         {install}; \
         export HELM_INTEGRATION=1; \
         export HELM_USER_ZDOTDIR=\"${{ZDOTDIR:-$HOME}}\"; \
         export ZDOTDIR=\"$HOME/.helm/integration/zsh\"; \
         if [ -z \"$(tmux list-sessions -F '#{{session_id}}' 2>/dev/null)\" ]; then \
            tmux new-session -d -s '{workspace}' 2>/dev/null; \
         fi; \
         tmux set-environment -g HELM_INTEGRATION 1 2>/dev/null; \
         tmux set-environment -g HELM_USER_ZDOTDIR \"$HELM_USER_ZDOTDIR\" 2>/dev/null; \
         tmux set-environment -g ZDOTDIR \"$ZDOTDIR\" 2>/dev/null; \
         for s in $(tmux list-sessions -F '#{{session_id}}' 2>/dev/null); do \
            tmux set-environment -t \"$s\" HELM_INTEGRATION 1 2>/dev/null; \
            tmux set-environment -t \"$s\" HELM_USER_ZDOTDIR \"$HELM_USER_ZDOTDIR\" 2>/dev/null; \
            tmux set-environment -t \"$s\" ZDOTDIR \"$ZDOTDIR\" 2>/dev/null; \
         done; \
         exec tmux -CC attach",
    );
    let (first_client, first_events) = open_remote_tmux_client(&session, first_command).await?;

    // Ask the first client what session it landed on. We need the
    // server-wide session *id* (`$N`) here, not the user-friendly
    // *name* — `list-sessions` below returns ids, and the dedup loop
    // has to compare apples to apples. The earlier version used
    // `#{client_session}` (the name) which never matched, so we'd
    // open a *second* control client for the session our first
    // client was already attached to. Both clients then forwarded
    // the same `%output` for every keystroke, producing the
    // "eeechhoo" double-char artefact.
    let primary_id = first_client
        .send_command("display-message -p '#{session_id}'")
        .await
        .map_err(|e| e.to_string())?
        .trim()
        .to_string();
    if primary_id.is_empty() {
        return Err("could not determine attached session id on remote".into());
    }

    // Enumerate every session on the remote tmux server. For each one
    // that isn't the session our first client already attached to,
    // open another exec channel (without re-running the install
    // heredoc; just set env + exec).
    let raw = first_client
        .list_sessions("#{session_id}")
        .await
        .map_err(|e| e.to_string())?;
    let all_sessions: Vec<String> = raw
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let mut clients = vec![(primary_id.clone(), first_client, first_events)];

    // Open the per-session attach channels concurrently. Each one pays
    // the cost of a remote shell startup — sshd allocates a PTY, our
    // ZDOTDIR wrapper makes zsh interactive (PTY-mode), the user's
    // `.zshrc` runs in full before our `exec tmux` line takes over.
    // For heavy shell configs that's 1-3s each; opening N of them
    // serially scales linearly. Concurrent opens let all the remote
    // shells initialize in parallel, dropping wall time to roughly
    // one shell-init period regardless of session count.
    //
    // The SSH session's I/O thread still processes channel-open
    // requests serially (one round-trip per open), but each await on
    // `spawn_with_io`'s ready gate (tmux's first `%session-changed`)
    // happens in parallel — and that wait is what the slow shell
    // init blocks behind.
    let other_sids: Vec<String> = all_sessions
        .into_iter()
        .filter(|sid| sid != &primary_id)
        .collect();
    if !other_sids.is_empty() {
        let session_for_extras = session.clone();
        let extras = futures_join_attach_channels(session_for_extras, other_sids).await;
        for (sid, result) in extras {
            match result {
                Ok((c, ev)) => clients.push((sid, c, ev)),
                Err(e) => {
                    // Likely MaxSessions hit, slow shell init timing
                    // out, or transient remote tmux hiccup. Surface in
                    // logs but don't tear down the working primary —
                    // partial multi-client is better than no
                    // connection.
                    tracing::warn!(
                        "connect_remote_multi: failed to open extra client for session {sid}: {e}"
                    );
                }
            }
        }
    }

    Ok(ConnectedHost {
        clients,
        primary_session_id: primary_id,
        ssh: Some(session),
    })
}

/// Fan out `open_remote_tmux_client` across `sids` concurrently and
/// collect (sid, result) pairs in input order. Each future runs
/// independently; one stuck channel doesn't block the others.
async fn futures_join_attach_channels(
    session: Arc<helm_ssh::SshSession>,
    sids: Vec<String>,
) -> Vec<(
    String,
    Result<(Arc<TmuxClient>, mpsc::UnboundedReceiver<TmuxNotification>), String>,
)> {
    let mut tasks = Vec::with_capacity(sids.len());
    for sid in sids {
        let session = session.clone();
        let sid_for_task = sid.clone();
        let cmd = remote_attach_command(&sid);
        tasks.push(tokio::spawn(async move {
            let result = open_remote_tmux_client(&session, cmd).await;
            (sid_for_task, result)
        }));
    }
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        match t.await {
            Ok(pair) => out.push(pair),
            Err(e) => out.push((
                String::new(),
                Err(format!("attach task join: {e}")),
            )),
        }
    }
    out
}

/// Per-session attach command for remote hosts. The first attach
/// (which also installs integration) is a different command — see
/// `connect_remote_multi`. This one only sets the env vars (which the
/// fresh remote login process needs) and exec's tmux.
fn remote_attach_command(session_id: &str) -> String {
    let sid = session_id.replace('\'', "'\\''");
    format!(
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$HOME/homebrew/bin:$PATH\"; \
         export HELM_INTEGRATION=1; \
         export HELM_USER_ZDOTDIR=\"${{ZDOTDIR:-$HOME}}\"; \
         export ZDOTDIR=\"$HOME/.helm/integration/zsh\"; \
         exec tmux -CC attach -t '{sid}'"
    )
}

/// Open one exec channel on `session`, hand its sync pipes to
/// `TmuxClient::spawn_with_io`, and return the resulting client +
/// notifications receiver. Cheap (one round-trip for channel open +
/// PTY request + exec) so it's fine to call N times during connect.
async fn open_remote_tmux_client(
    session: &Arc<helm_ssh::SshSession>,
    command: String,
) -> Result<(Arc<TmuxClient>, mpsc::UnboundedReceiver<TmuxNotification>), String> {
    let session_for_blocking = session.clone();
    let opened = tokio::task::spawn_blocking(move || session_for_blocking.open_exec(command))
        .await
        .map_err(|e| format!("ssh open_exec join: {e}"))?
        .map_err(|e| e.to_string())?;

    let reader: Box<dyn std::io::Read + Send> = Box::new(opened.reader);
    let writer: Box<dyn std::io::Write + Send> = Box::new(opened.writer);

    // Cleanup: dropping the boxed reader/writer closes the pipes,
    // which signals the helm-ssh pumps to exit and (eventually) the
    // remote tmux client process to receive EOF on its stdin and
    // exit. The shared SshSession stays alive for sibling channels.
    let cleanup: Box<dyn FnOnce() + Send> = Box::new(|| {
        // No-op: pipes close on Drop of the reader/writer captured
        // by spawn_with_io's threads.
    });

    let (client, events) = TmuxClient::spawn_with_io(
        reader,
        writer,
        cleanup,
        Duration::from_secs(10),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok((Arc::new(client), events))
}

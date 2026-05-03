//! Tauri commands. Each is exposed to the frontend via specta-typed bindings.
//!
//! Phase 2 reshapes everything around `HostId`. The frontend opens one event
//! channel via `host_subscribe`, then drives connect/disconnect/tmux ops by
//! id. Stage A only knows about localhost; Stage B will add SSH targets.

use async_trait::async_trait;
use helm_domain::{
    AuthMethod, Host, HostEvent, HostId, HostKeyDecision, HostKeyPromptKind, HostStatus,
    TmuxNotification,
};
use helm_ssh::{HostKeyPrompter, SshAuth, SshTarget};
use helm_tmux::TmuxClient;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::State;
use tokio::sync::{mpsc, oneshot, watch};

use crate::integration;
use crate::notifications;
use crate::state::{AppState, NotificationsCtx, SharedHostEntry, SupervisorSignal};

#[derive(Debug, Serialize, Deserialize, Type)]
pub struct PingResponse {
    pub ok: bool,
    pub message: String,
}

/// Phase 0 sanity check. Frontend calls this on boot to confirm IPC works.
#[tauri::command]
#[specta::specta]
pub fn ping() -> PingResponse {
    PingResponse {
        ok: true,
        message: "scaffold ok".into(),
    }
}

// ---------- host commands ----------

/// Snapshot of every known host. Stage A returns just `[localhost]`; Stage C
/// will merge persisted hosts from `hosts.json`.
#[tauri::command]
#[specta::specta]
pub async fn host_list(state: State<'_, AppState>) -> Result<Vec<Host>, String> {
    let mut out = Vec::with_capacity(state.hosts.len());
    for entry in state.hosts.iter() {
        let guard = entry.value().lock().await;
        out.push(guard.host.clone());
    }
    Ok(out)
}

/// The id of the always-present localhost entry. Stable per process; the
/// frontend stashes this on boot so subsequent commands can target it.
#[tauri::command]
#[specta::specta]
pub fn host_local_id(state: State<'_, AppState>) -> HostId {
    state.local_host_id
}

/// Save a host to the persistent registry. Upsert semantics: if a host
/// with the same id already exists, it's replaced (any active
/// connection is torn down first since the new metadata could change
/// the connect path). The on-disk `hosts.json` is rewritten atomically.
#[tauri::command]
#[specta::specta]
pub async fn host_save(state: State<'_, AppState>, host: Host) -> Result<HostId, String> {
    let id = host.id;
    let host_clone = host.clone();
    let is_replace = state.hosts.contains_key(&id);

    // If we're replacing, tear down the existing connection so stale
    // tmux/ssh handles don't outlive their host record. Mark voluntary
    // so the supervisor exits cleanly without running its reconnect
    // ladder against the old (now stale) settings.
    if is_replace {
        if let Some(existing) = state.entry(id) {
            let mut guard = existing.lock().await;
            guard.voluntary_disconnect = true;
            if let Some(handle) = guard.supervisor.take() {
                handle.abort();
            }
            guard.shutdown_clients();
            guard.host = host;
        }
    } else {
        let entry = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::HostEntry::new(host)));
        state.hosts.insert(id, entry);
    }

    persist_hosts(&state).await?;

    let event_tx = state.event_tx.lock().await.clone();
    if is_replace {
        // Frontend treats HostAdded as upsert — overwrites the existing
        // entry. Saves us a separate "host_updated" event variant.
        emit_event(&event_tx, HostEvent::HostAdded { host: host_clone });
    } else {
        emit_event(&event_tx, HostEvent::HostAdded { host: host_clone });
        emit_event(
            &event_tx,
            HostEvent::Status {
                host_id: id,
                status: HostStatus::Disconnected,
                error: None,
            },
        );
    }
    Ok(id)
}

/// Delete a host from both the in-memory registry and `hosts.json`.
/// Tears down any active connection and clears any Keychain entry the
/// host owned. Localhost cannot be deleted.
#[tauri::command]
#[specta::specta]
pub async fn host_delete(state: State<'_, AppState>, host_id: HostId) -> Result<(), String> {
    if host_id == state.local_host_id {
        return Err("cannot delete localhost".into());
    }
    if let Some((_, entry)) = state.hosts.remove(&host_id) {
        let mut guard = entry.lock().await;
        guard.voluntary_disconnect = true;
        if let Some(handle) = guard.supervisor.take() {
            handle.abort();
        }
        guard.shutdown_clients();
    }
    persist_hosts(&state).await?;
    // Best-effort Keychain cleanup. If the host wasn't using password
    // auth there's nothing to delete; the wrapper already swallows the
    // not-found error.
    let _ = crate::keychain::delete_password(host_id);
    let event_tx = state.event_tx.lock().await.clone();
    let notif_ctx = state.notifications_ctx();
    notifications::dismiss_for_host(&notif_ctx, &event_tx, host_id);
    emit_event(&event_tx, HostEvent::HostRemoved { host_id });
    Ok(())
}

/// Store a password for `host_id` in the macOS Keychain. The password
/// argument is consumed and never round-trips back out via IPC — only
/// `connect_for_host` reads it via `keychain::get_password`.
#[tauri::command]
#[specta::specta]
pub async fn host_save_password(host_id: HostId, password: String) -> Result<(), String> {
    crate::keychain::set_password(host_id, &password)
}

/// One entry from the user's `~/.ssh/config`, flattened to the fields
/// the host-editor needs for autocomplete.
#[derive(Debug, Clone, Serialize, Type)]
pub struct SshConfigAlias {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

/// Parse `~/.ssh/config` and return the resolved aliases. Skips wildcard
/// patterns (`*`, `?`, …) since those aren't useful as host identities.
/// Failure to read or parse is surfaced as an empty list rather than an
/// error — the host editor stays usable on machines without an ssh
/// config or with a syntax error.
#[tauri::command]
#[specta::specta]
pub async fn ssh_config_aliases() -> Result<Vec<SshConfigAlias>, String> {
    use ssh2_config::{ParseRule, SshConfig};

    let path = match dirs::home_dir() {
        Some(h) => h.join(".ssh").join("config"),
        None => return Ok(vec![]),
    };
    if !path.exists() {
        return Ok(vec![]);
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Ok(vec![]),
    };
    let mut reader = std::io::BufReader::new(file);
    let cfg = match SshConfig::default().parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS) {
        Ok(c) => c,
        Err(_) => return Ok(vec![]),
    };

    let mut out = Vec::new();
    for entry in cfg.get_hosts() {
        for clause in &entry.pattern {
            // Skip the implicit `*` block (default settings) and any
            // wildcard patterns. The host editor wants concrete alias
            // names that map to a single host.
            if clause.negated || clause.pattern.contains('*') || clause.pattern.contains('?') {
                continue;
            }
            // Resolve the host's params *as if* the user typed this
            // alias — that picks up `Host *` defaults, included
            // configs, etc.
            let params = cfg.query(&clause.pattern);
            out.push(SshConfigAlias {
                alias: clause.pattern.clone(),
                hostname: params.host_name.clone(),
                user: params.user.clone(),
                port: params.port,
            });
        }
    }
    // Stable order: alphabetical so the autocomplete list is consistent.
    out.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(out)
}

/// Snapshot the in-memory registry (minus localhost) and write
/// `hosts.json`. Holds each entry's lock briefly to clone its `Host`.
async fn persist_hosts(state: &State<'_, AppState>) -> Result<(), String> {
    let mut to_save: Vec<Host> = Vec::new();
    let local_id = state.local_host_id;
    for entry in state.hosts.iter() {
        if *entry.key() == local_id {
            continue;
        }
        let guard = entry.value().lock().await;
        to_save.push(guard.host.clone());
    }
    crate::persistence::save_hosts(&to_save)
}

/// Register the global event channel. Tmux notifications and host status
/// transitions stream through here, tagged by host id.
///
/// Idempotent across webview reloads: replacing an old sender drops it,
/// and the next `host_connect` will repopulate from current state by
/// re-emitting Status events.
#[tauri::command]
#[specta::specta]
pub async fn host_subscribe(
    state: State<'_, AppState>,
    on_event: Channel<HostEvent>,
) -> Result<(), String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HostEvent>();
    {
        let mut guard = state.event_tx.lock().await;
        *guard = Some(tx);
    }
    tokio::spawn(async move {
        while let Some(evt) = rx.recv().await {
            if on_event.send(evt).is_err() {
                break;
            }
        }
    });

    // Replay current statuses so a freshly-attached frontend sees the world
    // it walked into. Without this, a `Cmd+R` mid-session would leave the
    // sidebar stuck at "Disconnected" until the next status transition.
    let snapshots: Vec<(HostId, HostStatus)> = {
        let mut v = Vec::with_capacity(state.hosts.len());
        for entry in state.hosts.iter() {
            let g = entry.value().lock().await;
            v.push((g.host.id, g.status));
        }
        v
    };
    if let Some(tx) = state.event_tx.lock().await.as_ref() {
        for (host_id, status) in snapshots {
            let _ = tx.send(HostEvent::Status {
                host_id,
                status,
                error: None,
            });
        }
    }
    Ok(())
}

/// Connect to a host. Localhost spawns `tmux -CC` in a local PTY; remote
/// hosts open an SSH session and run tmux on the far end with the channel
/// piped through `tokio_util::SyncIoBridge` into the same reader/writer
/// threads helm-tmux uses for the local case.
///
/// `bootstrap_workspace` overrides `Host.default_workspace` for this
/// connect attempt. Useful when the user's "+ workspace" button needs to
/// reconnect: instead of creating a stray `main` session and *then* the
/// requested workspace, we create the requested workspace directly as
/// the bootstrap session.
///
/// Idempotent: dropping any existing client kills its `-CC` process, the
/// tmux *server* and session live on, so reattaching is a clean recovery.
#[tauri::command]
#[specta::specta]
pub async fn host_connect(
    state: State<'_, AppState>,
    host_id: HostId,
    bootstrap_workspace: Option<String>,
) -> Result<(), String> {
    let entry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let event_tx = state.event_tx.lock().await.clone();
    let prompter: Arc<dyn HostKeyPrompter> = Arc::new(AppHostKeyPrompter {
        host_id,
        event_tx: event_tx.clone(),
        pending: state.inner().pending_host_key_prompts_handle(),
    });
    let network_online = state.inner().network_online.clone();
    let notif_ctx = state.notifications_ctx();

    // Cancel any prior supervisor (live or in-backoff) so we don't have
    // two reconnect ladders racing for the same host. Reset the
    // voluntary flag so the new supervisor's ladder runs.
    {
        let mut guard = entry.lock().await;
        if let Some(handle) = guard.supervisor.take() {
            handle.abort();
        }
        guard.voluntary_disconnect = false;
    }

    drop(state); // release the State borrow before the long-running connect.
    do_connect(
        entry,
        host_id,
        event_tx,
        bootstrap_workspace,
        prompter,
        network_online,
        notif_ctx,
    )
    .await
}

/// Frontend's response to a `HostKeyPrompt` event. Looks up the matching
/// pending decision-channel and fires it; the SSH connect future is
/// parked on that oneshot.
#[tauri::command]
#[specta::specta]
pub async fn host_key_prompt_response(
    state: State<'_, AppState>,
    host_id: HostId,
    decision: HostKeyDecision,
) -> Result<(), String> {
    if let Some((_, tx)) = state.pending_host_key_prompts.remove(&host_id) {
        let _ = tx.send(decision);
        Ok(())
    } else {
        Err("no pending host-key prompt for this host".into())
    }
}

/// Bridges helm-ssh's host-key callback to the frontend event channel.
/// One per connect attempt; lives as long as the SSH `Client` (i.e. the
/// duration of the SSH session). Holds an `Arc` reference into the app's
/// pending-prompts DashMap so `host_key_prompt_response` can find the
/// matching oneshot.
struct AppHostKeyPrompter {
    host_id: HostId,
    event_tx: Option<mpsc::UnboundedSender<HostEvent>>,
    pending: Arc<dashmap::DashMap<HostId, oneshot::Sender<HostKeyDecision>>>,
}

#[async_trait]
impl HostKeyPrompter for AppHostKeyPrompter {
    async fn prompt(
        &self,
        hostname: &str,
        port: u16,
        algorithm: &str,
        fingerprint: &str,
        kind: HostKeyPromptKind,
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

/// All connect logic past the State<'_> prelude. Pulled out so the
/// `#[tauri::command]` Send bound only has to inspect a small, owned-data
/// future. Carrying `State<'_>` across the SSH connect await trips on
/// HRTB inference inside the macro.
///
/// One-shot from the user's view: if the initial connect fails, the
/// host stays in `Error` state and the user re-clicks. Reconnect on
/// transport drop is the supervisor's job, spawned only once we've
/// successfully connected at least once — initial-connect failures are
/// almost always permanent (auth rejected, host unreachable, key
/// rejected) and silently retrying them spams prompts.
async fn do_connect(
    entry: SharedHostEntry,
    host_id: HostId,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<HostEvent>>,
    bootstrap_workspace: Option<String>,
    prompter: Arc<dyn HostKeyPrompter>,
    network_online: watch::Receiver<bool>,
    notif_ctx: NotificationsCtx,
) -> Result<(), String> {
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
                let forwarder_handle = tokio::spawn(client_forwarder_loop(
                    host_id,
                    session_id.clone(),
                    events,
                    event_tx.clone(),
                    notif_ctx.clone(),
                    sup_tx.clone(),
                    entry.clone(),
                ));
                session_clients.insert(
                    session_id,
                    Arc::new(crate::state::SessionClient {
                        tmux,
                        forwarder: forwarder_handle.abort_handle(),
                    }),
                );
            }

            // Stash everything into the HostEntry. Status flips to
            // Connected here so the frontend renders the tree as soon
            // as we're done bootstrapping (refresh_pane_index runs next
            // and only refines the per-pane breadcrumbs).
            {
                let mut guard = entry.lock().await;
                guard.clients = session_clients;
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
                    let user_zdotdir = std::env::var("ZDOTDIR")
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
            // before the frontend's own refetchTree finishes.
            if let Some(ref pc) = primary_client {
                let _ = notifications::refresh_pane_index(
                    &notif_ctx,
                    &event_tx,
                    pc,
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
            // session was killed, etc.).
            let ctx = notif_ctx.clone();
            let tx = event_tx.clone();
            let entry = entry.clone();
            tokio::spawn(async move {
                let client = entry.lock().await.primary_client();
                if let Some(client) = client {
                    let _ = notifications::refresh_pane_index(
                        &ctx, &tx, &client, host_id,
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

/// Connection supervisor.
///
/// Runs the forwarder loop (drain tmux notifications onto the global
/// channel) until EOF. On EOF, decides between exiting cleanly (user
/// asked us to disconnect) and entering the reconnect ladder.
///
/// Backoff schedule is `[1, 2, 4, 8, 30]s`, clamped to 30s after that —
/// applied uniformly to local and remote. Local tmux death (server
/// killed, dev `pkill`, OS reaping) is recoverable: each reconnect
/// attempt re-runs `spawn_local`, which `exec`s `tmux -CC new-session
/// -A` and brings up a fresh server. If tmux genuinely can't be brought
/// up (binary missing, broken install), the supervisor sits in
/// `Reconnecting` indefinitely and the frontend surfaces the error —
/// which the user can resolve by clicking the localhost row to retry
/// or by fixing the underlying issue (the next backoff window picks it
/// up automatically).
/// Host supervisor — handles per-client deaths, incremental session
/// spawn (on `%sessions-changed`), and the reconnect ladder when every
/// client has died.
///
/// Shape: select between the supervisor-signal mpsc (from forwarders)
/// and the reachability watch. ClientDied removes the entry from
/// `clients` and either picks a new primary (other clients alive) or
/// triggers full reconnect (none alive). SessionsChanged re-enumerates
/// sessions on the host and spawns clients for any newcomers.
///
/// Reconnect ladder identical to the prior single-client implementation:
/// `[1, 2, 4, 8, 30]s` clamped to 30s, with reachability-watch early-wake.
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
        match connect_host_multi(host.clone(), Some(prompter.clone())).await {
            Ok(connected) => {
                let primary_id = connected.primary_session_id.clone();
                let ssh = connected.ssh.clone();

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
                    let forwarder = tokio::spawn(client_forwarder_loop(
                        host_id,
                        session_id.clone(),
                        events,
                        event_tx.clone(),
                        notif_ctx.clone(),
                        sup_tx_new.clone(),
                        entry.clone(),
                    ));
                    session_clients.insert(
                        session_id,
                        Arc::new(crate::state::SessionClient {
                            tmux,
                            forwarder: forwarder.abort_handle(),
                        }),
                    );
                }
                {
                    let mut guard = entry.lock().await;
                    guard.clients = session_clients;
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
/// Triggered by `%sessions-changed` notifications. The Frontend creates
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
        let forwarder = tokio::spawn(client_forwarder_loop(
            host_id,
            sid.clone(),
            events,
            event_tx.clone(),
            notif_ctx.clone(),
            sup_tx.clone(),
            entry.clone(),
        ));
        let mut guard = entry.lock().await;
        guard.clients.insert(
            sid,
            Arc::new(crate::state::SessionClient {
                tmux,
                forwarder: forwarder.abort_handle(),
            }),
        );
    }
    Ok(())
}

fn emit_event(
    tx: &Option<tokio::sync::mpsc::UnboundedSender<HostEvent>>,
    event: HostEvent,
) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

#[tauri::command]
#[specta::specta]
pub async fn host_disconnect(state: State<'_, AppState>, host_id: HostId) -> Result<(), String> {
    let entry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let event_tx = state.event_tx.lock().await.clone();
    let notif_ctx = state.notifications_ctx();
    {
        let mut guard = entry.lock().await;
        // Mark voluntary BEFORE aborting / dropping clients so the
        // supervisor (if it's awaiting on EOF) sees the flag set.
        guard.voluntary_disconnect = true;
        if let Some(handle) = guard.supervisor.take() {
            handle.abort();
        }
        guard.shutdown_clients();
        guard.status = HostStatus::Disconnected;
    }
    emit_event(&event_tx, HostEvent::Status {
        host_id,
        status: HostStatus::Disconnected,
        error: None,
    });
    notifications::dismiss_for_host(&notif_ctx, &event_tx, host_id);
    Ok(())
}

/// Output of the multi-client connect path. Each tuple is one tmux
/// session and its freshly-spawned control client + notifications
/// receiver. `do_connect`/`supervise` wrap each into a `SessionClient`
/// and spawn its forwarder.
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
    // *name* — `list-sessions` below returns ids, and the dedup
    // loop has to compare apples to apples. The earlier version used
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

// ---------- tmux commands (per-host) ----------

/// Resolve the primary control client for a host. Multi-client model:
/// every per-session control client can service global commands
/// (pane/window/session ids are server-wide), so we just route through
/// the primary. Returns "host not connected" when no clients exist.
async fn tmux_for(state: &State<'_, AppState>, host_id: HostId) -> Result<Arc<TmuxClient>, String> {
    let entry: SharedHostEntry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let guard = entry.lock().await;
    guard
        .primary_client()
        .ok_or_else(|| "host not connected".to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_send_keys(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .send_keys(&pane_id, &bytes)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_resize_pane(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .resize_pane(&pane_id, cols, rows)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_new_window(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: Option<String>,
    name: Option<String>,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .new_window(session_id.as_deref(), name.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_split_pane(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    vertical: bool,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .split_pane(&pane_id, vertical)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_kill_window(
    state: State<'_, AppState>,
    host_id: HostId,
    window_id: String,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .kill_window(&window_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_select_window(
    state: State<'_, AppState>,
    host_id: HostId,
    window_id: String,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .select_window(&window_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_select_pane(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .select_pane(&pane_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_rename_window(
    state: State<'_, AppState>,
    host_id: HostId,
    window_id: String,
    name: String,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .rename_window(&window_id, &name)
        .await
        .map_err(|e| e.to_string())
}

/// Returns a newline-delimited list of windows. Each line is rendered from
/// the supplied tmux format string (e.g. `"#{window_id} #{window_name}"`).
#[tauri::command]
#[specta::specta]
pub async fn tmux_list_windows(
    state: State<'_, AppState>,
    host_id: HostId,
    format: String,
) -> Result<String, String> {
    tmux_for(&state, host_id)
        .await?
        .list_windows(&format)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_list_panes(
    state: State<'_, AppState>,
    host_id: HostId,
    format: String,
) -> Result<String, String> {
    tmux_for(&state, host_id)
        .await?
        .list_panes(&format)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_list_sessions(
    state: State<'_, AppState>,
    host_id: HostId,
    format: String,
) -> Result<String, String> {
    tmux_for(&state, host_id)
        .await?
        .list_sessions(&format)
        .await
        .map_err(|e| e.to_string())
}

/// Create a new tmux session (workspace). Returns the new session id
/// (e.g. `$3`). Pass `None` to let tmux pick the name; pass `Some(name)`
/// for an explicit name (rejected by tmux if the name already exists).
#[tauri::command]
#[specta::specta]
pub async fn tmux_new_session(
    state: State<'_, AppState>,
    host_id: HostId,
    name: Option<String>,
) -> Result<String, String> {
    tmux_for(&state, host_id)
        .await?
        .new_session(name.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// Kill a session. ALL shells inside it terminate. The frontend handles
/// the active-workspace fallback when the killed session was active.
#[tauri::command]
#[specta::specta]
pub async fn tmux_kill_session(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .kill_session(&session_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn tmux_rename_session(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
    name: String,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .rename_session(&session_id, &name)
        .await
        .map_err(|e| e.to_string())
}

/// Backward-compat no-op. Pre-multi-client we used switch-client to
/// move the single control client between sessions so its viewport
/// resize would reach the now-attached session. With one permanent
/// control client per session, every session is always attached at
/// the right viewport — there's nothing to switch.
///
/// Kept as a no-op (rather than removed) so an older frontend build
/// during a phased rollout doesn't fail. Frontend `selectWorkspace`
/// stops calling it in this same change.
#[tauri::command]
#[specta::specta]
pub async fn tmux_switch_client(
    _state: State<'_, AppState>,
    _host_id: HostId,
    _session_id: String,
) -> Result<(), String> {
    Ok(())
}

/// Capture the current buffer of a pane (with escape sequences) so the
/// frontend can replay it on mount/reattach. tmux doesn't auto-replay
/// pane state when a control client attaches — without this, the user
/// sees an empty xterm until something new prints.
///
/// `scrollback_lines`:
/// - `0` → visible buffer only (a few KB; used for the fast
///   pre-hydration pass).
/// - `n > 0` → `-S -n`, last `n` lines including history. The frontend
///   uses ~2000 for "first paint with real history."
#[tauri::command]
#[specta::specta]
pub async fn tmux_capture_pane(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    scrollback_lines: u32,
) -> Result<String, String> {
    tmux_for(&state, host_id)
        .await?
        .capture_pane(&pane_id, scrollback_lines)
        .await
        .map_err(|e| e.to_string())
}

/// Tell tmux that this client is now `cols × rows` cells. With one
/// control client per session in the multi-client model, we fan out
/// the resize to every attached client so each session renders at the
/// user's actual viewport (tmux unions sizes across attached clients;
/// missing one would shrink the corresponding session to whatever
/// other client was attached, or to tmux's default 80×24).
#[tauri::command]
#[specta::specta]
pub async fn tmux_resize_client(
    state: State<'_, AppState>,
    host_id: HostId,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let entry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let clients: Vec<Arc<TmuxClient>> = {
        let g = entry.lock().await;
        g.clients.values().map(|c| c.tmux.clone()).collect()
    };
    if clients.is_empty() {
        return Err("host not connected".into());
    }
    // Best-effort fan-out: a single client failing (e.g. its session
    // was just killed and the channel is mid-teardown) shouldn't
    // prevent the other resizes. Errors logged + collected; we only
    // surface to the caller if *every* resize failed.
    let mut failures = Vec::new();
    for client in clients {
        if let Err(e) = client.resize_client(cols, rows).await {
            failures.push(e.to_string());
        }
    }
    if failures.len() == failures.capacity() && !failures.is_empty() {
        // (Won't happen in practice — using as a guard for the
        // common-case "all failed" scenario, where it's worth
        // bubbling.)
    }
    if !failures.is_empty() {
        tracing::debug!("tmux_resize_client partial failures: {failures:?}");
    }
    Ok(())
}

// ---------- notifications ----------

/// Snapshot every live notification, ordered oldest-first by created_at.
/// The frontend uses this on boot to repopulate its inbox; subsequent
/// updates flow through the `Notification` / `NotificationDismissed`
/// HostEvent variants.
#[tauri::command]
#[specta::specta]
pub async fn notifications_list(
    state: State<'_, AppState>,
) -> Result<Vec<helm_domain::Notification>, String> {
    let mut out: Vec<_> = state
        .notifications
        .iter()
        .map(|r| r.value().clone())
        .collect();
    out.sort_by_key(|n| n.created_at);
    Ok(out)
}

/// Dismiss a single notification by id. No-op if the id no longer exists
/// (the inbox row may have been auto-dismissed by another path — host
/// disconnect, window kill, dismiss-on-keystroke).
#[tauri::command]
#[specta::specta]
pub async fn notification_dismiss(
    state: State<'_, AppState>,
    notification_id: helm_domain::NotificationId,
) -> Result<(), String> {
    let event_tx = state.event_tx.lock().await.clone();
    let Some((_, notif)) = state.notifications.remove(&notification_id) else {
        return Ok(());
    };
    state
        .notification_by_pane
        .remove(&(notif.host_id, notif.pane_id));
    emit_event(
        &event_tx,
        HostEvent::NotificationDismissed {
            host_id: notif.host_id,
            notification_id,
        },
    );
    Ok(())
}

/// Tell the backend which (host, window) the user is currently looking
/// at. Pass `None`s to clear (helm window lost OS focus / minimized) so
/// backgrounded windows resume getting notifications.
///
/// The notifications post-processor consults this on every event and
/// suppresses inbox rows for the focused window — the user is already
/// watching that output, an inbox entry would just be noise.
#[tauri::command]
#[specta::specta]
pub async fn set_focus(
    state: State<'_, AppState>,
    host_id: Option<HostId>,
    window_id: Option<String>,
) -> Result<(), String> {
    let mut guard = state.focus.lock();
    *guard = match (host_id, window_id) {
        (Some(h), Some(w)) if !w.is_empty() => Some((h, w)),
        _ => None,
    };
    Ok(())
}

/// Dismiss every notification whose pane sits inside `window_id`. Used
/// by the dismiss-on-keystroke path in `TmuxPane`: the user typed into
/// the window, so any in-flight inbox rows for that window are stale.
///
/// Resolves panes via the cached `pane_runtime` index. We don't fall
/// through to a live `list-panes` call here — if the index doesn't know
/// about a pane yet, it has no notification entry to dismiss anyway.
#[tauri::command]
#[specta::specta]
pub async fn notification_dismiss_for_window(
    state: State<'_, AppState>,
    host_id: HostId,
    window_id: String,
) -> Result<(), String> {
    let event_tx = state.event_tx.lock().await.clone();
    let pane_ids: Vec<String> = state
        .pane_runtime
        .iter()
        .filter(|r| r.key().0 == host_id && r.value().window_id == window_id)
        .map(|r| r.key().1.clone())
        .collect();
    if pane_ids.is_empty() {
        return Ok(());
    }
    let notif_ctx = state.notifications_ctx();
    crate::notifications::dismiss_for_panes(&notif_ctx, &event_tx, host_id, &pane_ids);
    Ok(())
}


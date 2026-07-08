//! Host registry, lifecycle, and SSH-config helpers. Plus the Phase 0
//! sanity-ping command.

use async_trait::async_trait;
use helm_domain::{
    Host, HostEvent, HostId, HostKeyDecision, HostKeyPromptKind, HostStatus,
};
use helm_ssh::HostKeyPrompter;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::State;
use tokio::sync::{mpsc, oneshot};

use crate::commands::emit_event;
use crate::notifications;
use crate::state::AppState;

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

// ---------- host registry ----------

/// Snapshot of every known host. Localhost is always present; remote
/// hosts come from `hosts.json` plus any added via the editor.
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
        let entry =
            std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::HostEntry::new(host)));
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

    // Drop from the in-memory registry FIRST. DashMap::remove is
    // synchronous and never blocks on the entry's inner mutex, so this
    // succeeds even if a stuck do_connect / supervise task has the
    // mutex out for a slow SSH negotiation. We hand the Arc off to a
    // detached cleanup task below; the user's intent ("host gone") is
    // decoupled from the supervisor-abort latency.
    let removed = state.hosts.remove(&host_id);

    // Best-effort Keychain cleanup. If the host wasn't using password
    // auth there's nothing to delete; the wrapper already swallows the
    // not-found error.
    let _ = crate::keychain::delete_password(host_id);

    // Drop any pending host-key prompt sender. If the user deletes a
    // host whose connect is parked waiting for their accept/reject,
    // dropping the sender closes the oneshot — the receiver in helm-
    // ssh's prompter callback resolves to RecvError, the prompter
    // returns Reject, and connect_session bails out promptly instead
    // of waiting for the 15s SSH timeout. The detached cleanup task
    // below picks up afterward.
    let _ = state.pending_host_key_prompts.remove(&host_id);

    // Emit HostRemoved + dismiss notifications immediately. The
    // frontend store removes the host on this event — independent of
    // both the disk persist below and the detached supervisor cleanup.
    let event_tx = state.event_tx.lock().await.clone();
    let notif_ctx = state.notifications_ctx();
    notifications::dismiss_for_host(&notif_ctx, &event_tx, host_id);
    emit_event(&event_tx, HostEvent::HostRemoved { host_id });

    // Detached cleanup: lock the entry, mark voluntary, abort the
    // supervisor, drop clients. Not awaited — a stuck connect can
    // hold the entry mutex indefinitely (and a supervisor mid-
    // spawn_blocking SSH negotiation can't be aborted until the
    // blocking call returns), so making the IPC wait would just
    // re-introduce the original "delete does nothing" symptom for
    // a different reason.
    if let Some((_, entry)) = removed {
        tokio::spawn(async move {
            let mut guard = entry.lock().await;
            guard.voluntary_disconnect = true;
            if let Some(handle) = guard.supervisor.take() {
                handle.abort();
            }
            guard.shutdown_clients();
        });
    }

    persist_hosts(&state).await?;
    Ok(())
}

/// Store a password for `host_id` in the macOS Keychain. The password
/// argument is consumed and never round-trips back out via IPC — only
/// the connect path reads it via `keychain::get_password`.
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

// ---------- host event channel + connect lifecycle ----------

/// Register the global event channel. Tmux notifications, host status
/// transitions, notifications, and tool-integration suggestions stream
/// through here, tagged by host id.
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
/// hosts open an SSH session and run tmux on the far end with each
/// per-session control client multiplexed over the same SSH session.
///
/// `bootstrap_workspace` overrides `Host.default_workspace` for this
/// connect attempt. Useful when the user's "+ workspace" button needs to
/// reconnect: instead of creating a stray `main` session and *then* the
/// requested workspace, we create the requested workspace directly as
/// the bootstrap session.
///
/// Idempotent: tearing down any prior clients SIGKILLs each `-CC`
/// process / closes each SSH channel; the tmux *server* and sessions
/// live on, so reattaching is a clean recovery.
#[tauri::command]
#[specta::specta]
pub async fn host_connect(
    state: State<'_, AppState>,
    host_id: HostId,
    bootstrap_workspace: Option<String>,
) -> Result<(), String> {
    connect_host_impl(&state, host_id, bootstrap_workspace).await
}

/// The body of `host_connect`, callable from places other than a Tauri
/// command (notably the scheduler's auto-connect path). Builds the
/// host-key prompter, aborts any prior supervisor, and hands off to
/// `connection::do_connect`. The caller's `&State` borrow is dropped
/// before the long-running connect work begins so we don't pin it
/// across the full SSH handshake.
pub(crate) async fn connect_host_impl(
    state: &State<'_, AppState>,
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
    let wake_signal = state.inner().wake_signal.clone();
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

    crate::connection::do_connect(
        entry,
        host_id,
        event_tx,
        bootstrap_workspace,
        prompter,
        network_online,
        wake_signal,
        notif_ctx,
    )
    .await
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

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
use helm_ssh::{HostKeyPrompter, SshAuth, SshConnection, SshTarget};
use helm_tmux::TmuxClient;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::State;
use tokio::sync::{mpsc, oneshot, watch};

use crate::state::{AppState, SharedHostEntry};

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
            guard.tmux = None;
            guard.ssh = None;
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
        guard.tmux = None;
        guard.ssh = None;
    }
    persist_hosts(&state).await?;
    // Best-effort Keychain cleanup. If the host wasn't using password
    // auth there's nothing to delete; the wrapper already swallows the
    // not-found error.
    let _ = crate::keychain::delete_password(host_id);
    let event_tx = state.event_tx.lock().await.clone();
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
) -> Result<(), String> {
    emit_event(&event_tx, HostEvent::Status {
        host_id,
        status: HostStatus::Connecting,
        error: None,
    });

    // Drop any prior client AND any prior SSH session so old forwarder
    // tasks exit before we start fresh ones. tmux first so its cleanup
    // closure runs while the SSH session is still live.
    let host = {
        let mut guard = entry.lock().await;
        guard.tmux = None;
        guard.ssh = None;
        let mut h = guard.host.clone();
        if let Some(ws) = bootstrap_workspace {
            h.default_workspace = ws;
        }
        h
    };

    match connect_for_host(host.clone(), Some(prompter.clone())).await {
        Ok(Connected { client, events, ssh }) => {
            let client = Arc::new(client);
            {
                let mut guard = entry.lock().await;
                guard.tmux = Some(client);
                guard.ssh = ssh;
                guard.status = HostStatus::Connected;
            }

            // Spawn the supervisor: it owns the forwarder loop AND the
            // reconnect ladder for the lifetime of this connection.
            let handle = tokio::spawn(supervise(
                entry.clone(),
                host_id,
                host,
                event_tx.clone(),
                prompter,
                events,
                network_online,
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
async fn supervise(
    entry: SharedHostEntry,
    host_id: HostId,
    host: Host,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<HostEvent>>,
    prompter: Arc<dyn HostKeyPrompter>,
    initial_events: tokio::sync::mpsc::UnboundedReceiver<TmuxNotification>,
    mut network_online: watch::Receiver<bool>,
) {
    const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 30];

    let mut events = initial_events;
    let mut attempt = 0u32;
    // Last connect error, surfaced on subsequent Reconnecting emits so
    // the overlay can show *why* we're stuck (e.g. "tmux not found")
    // instead of a generic spinner. Cleared on a successful connect.
    let mut last_error: Option<String> = None;

    loop {
        // ----- forwarder loop: drain notifications until the transport closes -----
        while let Some(n) = events.recv().await {
            let is_exit = matches!(n, TmuxNotification::Exit { .. });
            if let Some(ref tx) = event_tx {
                if tx
                    .send(HostEvent::Tmux {
                        host_id,
                        notification: n,
                    })
                    .is_err()
                {
                    return;
                }
            }
            if is_exit {
                break;
            }
        }

        // ----- transport is down; decide what to do next -----
        let voluntary = {
            let g = entry.lock().await;
            g.voluntary_disconnect
        };
        if voluntary {
            let mut guard = entry.lock().await;
            guard.tmux = None;
            guard.ssh = None;
            guard.status = HostStatus::Disconnected;
            guard.supervisor = None;
            emit_event(&event_tx, HostEvent::Status {
                host_id,
                status: HostStatus::Disconnected,
                error: None,
            });
            return;
        }

        // Surface "Reconnecting" before the sleep so the UI overlays
        // immediately, not after the backoff window.
        //
        // We deliberately don't re-read `host` from `entry` here. The
        // only path that mutates a host while a supervisor is running
        // is `host_save`, which aborts the supervisor before mutating
        // — so anything we'd refresh either belongs to the *new*
        // supervisor (not us) or we've already been aborted.
        {
            let mut guard = entry.lock().await;
            guard.tmux = None;
            guard.ssh = None;
            guard.status = HostStatus::Reconnecting;
        }
        emit_event(&event_tx, HostEvent::Status {
            host_id,
            status: HostStatus::Reconnecting,
            error: last_error.clone(),
        });

        let bucket_idx = (attempt as usize).min(BACKOFF_SECS.len() - 1);
        let delay = Duration::from_secs(BACKOFF_SECS[bucket_idx]);
        // Early-wake when reachability flips false → true. Resets the
        // backoff index so the next attempt fires immediately rather
        // than waiting out the rest of this bucket. The `borrow_and_update`
        // marks the value as seen so subsequent `.changed()` only fires
        // on real transitions, not initial-state observations.
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

        // Re-check voluntary after the sleep — host_disconnect may
        // have fired while we slept.
        let voluntary = {
            let g = entry.lock().await;
            g.voluntary_disconnect
        };
        if voluntary {
            let mut guard = entry.lock().await;
            guard.status = HostStatus::Disconnected;
            guard.supervisor = None;
            emit_event(&event_tx, HostEvent::Status {
                host_id,
                status: HostStatus::Disconnected,
                error: None,
            });
            return;
        }

        // ----- attempt reconnect -----
        match connect_for_host(host.clone(), Some(prompter.clone())).await {
            Ok(Connected {
                client,
                events: new_events,
                ssh,
            }) => {
                let client = Arc::new(client);
                {
                    let mut guard = entry.lock().await;
                    guard.tmux = Some(client);
                    guard.ssh = ssh;
                    guard.status = HostStatus::Connected;
                }
                emit_event(&event_tx, HostEvent::Status {
                    host_id,
                    status: HostStatus::Connected,
                    error: None,
                });
                events = new_events;
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
    {
        let mut guard = entry.lock().await;
        // Mark voluntary BEFORE aborting / dropping clients so the
        // supervisor (if it's awaiting on EOF) sees the flag set.
        guard.voluntary_disconnect = true;
        if let Some(handle) = guard.supervisor.take() {
            handle.abort();
        }
        guard.tmux = None;
        guard.ssh = None;
        guard.status = HostStatus::Disconnected;
    }
    emit_event(&event_tx, HostEvent::Status {
        host_id,
        status: HostStatus::Disconnected,
        error: None,
    });
    Ok(())
}

struct Connected {
    client: TmuxClient,
    events: tokio::sync::mpsc::UnboundedReceiver<TmuxNotification>,
    ssh: Option<Arc<helm_ssh::SshSession>>,
}

/// Branch on host shape. Localhost (`port == 0`) uses `spawn_local`; remote
/// targets connect via `helm-ssh` and bridge the channel into the same
/// reader/writer threads.
///
/// Takes `host` by value so the future is `Send` end-to-end — borrows
/// across await points trip up Tauri's command type machinery.
async fn connect_for_host(
    host: Host,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<Connected, String> {
    if host.port == 0 {
        let (client, events) = TmuxClient::spawn_local(&host.default_workspace)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(Connected {
            client,
            events,
            ssh: None,
        });
    }

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
            // Pulled from Keychain only at connect time; never lives
            // in the Host record or crosses IPC.
            let secret = crate::keychain::get_password(host.id).map_err(|e| {
                format!("password not in Keychain — save it via host_save_password: {e}")
            })?;
            SshAuth::Password { secret }
        }
    };

    // Single-quote the workspace name so spaces / metacharacters don't break
    // remote shell parsing. tmux session names rarely contain quotes, but
    // belt-and-braces.
    let workspace = host.default_workspace.replace('\'', "'\\''");
    // Three things going on here:
    //   1. PATH augmentation. Non-interactive SSH on macOS gets a minimal
    //      PATH (no Homebrew); without prefixing, `tmux` isn't found
    //      even when installed.
    //   2. Probe for existing sessions with `list-sessions` *before*
    //      opening the control client. `tmux -CC attach` can race a
    //      dying server (succeeds briefly, exits, our pipe sees EOF
    //      after spawn_with_io has already returned Connected) — see
    //      the matching note in `helm_tmux::spawn_local`.
    //   3. Branch deterministically: attach if any sessions, else
    //      create `default_workspace` and attach to it.
    let command = format!(
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$HOME/homebrew/bin:$PATH\"; \
         if [ -n \"$(tmux list-sessions -F '#{{session_id}}' 2>/dev/null)\" ]; then \
            exec tmux -CC attach; \
         else \
            exec tmux -CC new-session -A -s '{}'; \
         fi",
        workspace
    );

    // helm_ssh::connect is synchronous from this side: it spawns a
    // dedicated OS thread that owns the russh runtime + channel and
    // hands us back blocking pipe halves. Run it on the blocking pool
    // so we don't tie up the Tauri command thread during the negotiation.
    let conn: SshConnection = tokio::task::spawn_blocking(move || {
        helm_ssh::connect(target, auth, command, Duration::from_secs(15), prompter)
    })
    .await
    .map_err(|e| format!("ssh task: {e}"))?
    .map_err(|e| e.to_string())?;

    let session = Arc::new(conn.session);
    let reader: Box<dyn std::io::Read + Send> = Box::new(conn.reader);
    let writer: Box<dyn std::io::Write + Send> = Box::new(conn.writer);

    // Cleanup: on TmuxClient::drop, signal the SSH I/O thread to tear
    // down. `disconnect` is sync and idempotent — safe to call from Drop.
    let cleanup_session = session.clone();
    let cleanup: Box<dyn FnOnce() + Send> = Box::new(move || {
        cleanup_session.disconnect();
    });

    // Generous ready-gate for remote — adds RTT and remote tmux startup
    // (which can be tens to hundreds of ms across the wire).
    let (client, events) = TmuxClient::spawn_with_io(
        reader,
        writer,
        cleanup,
        std::time::Duration::from_secs(10),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(Connected {
        client,
        events,
        ssh: Some(session),
    })
}

// ---------- tmux commands (per-host) ----------

async fn tmux_for(state: &State<'_, AppState>, host_id: HostId) -> Result<Arc<TmuxClient>, String> {
    let entry: SharedHostEntry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let guard = entry.lock().await;
    guard
        .tmux
        .as_ref()
        .cloned()
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

/// Make `session_id` the control client's current session. tmux resizes
/// it to match the client's viewport — which the frontend should call
/// whenever it sets a new active workspace, otherwise sessions created
/// with `new-session -d` render at tmux's default 80×24.
#[tauri::command]
#[specta::specta]
pub async fn tmux_switch_client(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .switch_client(&session_id)
        .await
        .map_err(|e| e.to_string())
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

/// Tell tmux that this control client is now `cols × rows` cells. Tmux
/// resizes the session and SIGWINCHes every pane, so shells redraw at the
/// new width. Call once on mount and again on every xterm resize.
#[tauri::command]
#[specta::specta]
pub async fn tmux_resize_client(
    state: State<'_, AppState>,
    host_id: HostId,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    tmux_for(&state, host_id)
        .await?
        .resize_client(cols, rows)
        .await
        .map_err(|e| e.to_string())
}


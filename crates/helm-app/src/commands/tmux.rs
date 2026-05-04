//! Per-host tmux commands. All commands take an explicit pane / window
//! / session id (server-globally unique in tmux), which means they
//! work through *any* control client. The shared `tmux_for(host_id)`
//! helper resolves the host's primary client and routes through it.
//!
//! `tmux_resize_client` is the one outlier — it fans out to every
//! client because each control client maintains its own viewport size
//! and tmux unions sizes across attached clients.

use helm_domain::HostId;
use std::sync::Arc;
use std::time::Duration;
use tauri::State;

use crate::commands::tmux_for;
use crate::state::AppState;

#[tauri::command]
#[specta::specta]
pub async fn tmux_send_keys(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let result = tmux_for(&state, host_id)
        .await?
        .send_keys(&pane_id, &bytes)
        .await
        .map_err(|e| e.to_string());

    // After-Enter hook for tool-integration detection. tmux doesn't
    // notify on foreground command changes — running `claude` in an
    // existing pane is silent to the control protocol — so we react
    // to the user pressing Enter (the moment a new command might
    // start). Cheap because:
    //   - early-out if every integration's already been processed
    //     (just a HashMap contains check, no IPC)
    //   - bounded by the user's typing speed
    //   - delayed 400ms so tmux has time to update pane_current_command
    //     before our list-panes call fires
    if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
        let notif_ctx = state.notifications_ctx();
        if crate::tool_integrations::any_pending(
            &notif_ctx.tool_integration_seen,
            host_id,
        ) {
            if let Some(entry) = state.entry(host_id) {
                let event_tx = state.event_tx.lock().await.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let (client, ssh, host) = {
                        let g = entry.lock().await;
                        (g.primary_client(), g.ssh.clone(), g.host.clone())
                    };
                    if let Some(client) = client {
                        crate::tool_integrations::detect_and_suggest(
                            &notif_ctx.tool_integration_seen,
                            &event_tx,
                            &client,
                            ssh.as_ref(),
                            &host,
                            host_id,
                        )
                        .await;
                    }
                });
            }
        }
    }

    result
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
    let clients: Vec<Arc<helm_tmux::TmuxClient>> = {
        let g = entry.lock().await;
        g.clients.values().map(|c| c.tmux.clone()).collect()
    };
    if clients.is_empty() {
        return Err("host not connected".into());
    }
    // Best-effort fan-out: a single client failing (e.g. its session
    // was just killed and the channel is mid-teardown) shouldn't
    // prevent the other resizes.
    let mut failures = Vec::new();
    for client in clients {
        if let Err(e) = client.resize_client(cols, rows).await {
            failures.push(e.to_string());
        }
    }
    if !failures.is_empty() {
        tracing::debug!("tmux_resize_client partial failures: {failures:?}");
    }
    Ok(())
}

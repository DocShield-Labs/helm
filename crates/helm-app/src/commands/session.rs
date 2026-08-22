//! Per-host session commands — the IPC surface onto helmd.
//!
//! Ids cross the boundary as the daemon's u64s stringified (same form
//! the `SessionEvent` tree uses). Output flows the other way over the
//! event channel; these commands are the control plane.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use helm_domain::{BlockInfo, HistoryPage, HostId, ScreenInfo, SearchHit, SessionTree};
use helm_proto::{DaemonMsg, PaneId, SearchScope, WindowId, WorkspaceId};
use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::commands::session_for;
use crate::connection::{
    to_domain_block, to_domain_hits, to_domain_row, to_domain_screen, to_domain_tree,
};
use crate::state::AppState;

/// Current tree for a host, from the cached snapshot. The frontend
/// calls this on (re)subscribe; live updates arrive as
/// `SessionEvent::Tree`.
#[tauri::command]
#[specta::specta]
pub async fn session_tree(state: State<'_, AppState>, host_id: HostId) -> Result<SessionTree, String> {
    let session = session_for(&state, host_id).await?;
    let snapshot = session.tree.lock().clone();
    Ok(to_domain_tree(&snapshot))
}

/// Keystrokes / paste for a pane. `data` is base64 so arbitrary bytes
/// (escape sequences, bracketed paste) cross the boundary intact.
#[tauri::command]
#[specta::specta]
pub async fn session_input(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    data: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad base64: {e}"))?;
    tracing::trace!(%pane_id, len = bytes.len(), "session_input");
    session
        .client
        .input(pane_id.parse::<PaneId>()?, bytes)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn session_resize(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .resize(pane_id.parse::<PaneId>()?, cols, rows)
        .map_err(|e| e.to_string())
}

/// The pane's current grid, for the first paint of a pane the frontend
/// shows (later changes stream as `SessionEvent::Screen` /
/// `SessionEvent::ScreenDiff`).
#[tauri::command]
#[specta::specta]
pub async fn session_screen(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
) -> Result<ScreenInfo, String> {
    let session = session_for(&state, host_id).await?;
    tracing::debug!(%pane_id, "session_screen");
    let pane = pane_id.parse::<PaneId>()?;
    match session.request(|id| session.client.screen(id, pane)).await? {
        DaemonMsg::Screen { screen, .. } => Ok(to_domain_screen(screen)),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// History rows in `[from_line, to_line)`, clamped to what the daemon
/// retains and to one page counted back from `to_line` — the frontend
/// pages upward from the grid's top as the user scrolls.
#[tauri::command]
#[specta::specta]
pub async fn session_history(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
    from_line: u64,
    to_line: u64,
) -> Result<HistoryPage, String> {
    let session = session_for(&state, host_id).await?;
    tracing::debug!(%pane_id, from_line, to_line, "session_history");
    let pane = pane_id.parse::<PaneId>()?;
    match session
        .request(|id| session.client.history(id, pane, from_line, to_line))
        .await?
    {
        DaemonMsg::History { from_line, rows, history_start, top_line, .. } => Ok(HistoryPage {
            from_line,
            rows: rows.into_iter().map(to_domain_row).collect(),
            history_start,
            top_line,
        }),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Ids returned by the creating commands.
#[derive(Debug, Clone, Serialize, Type)]
pub struct CreatedIds {
    pub workspace_id: String,
    pub window_id: Option<String>,
    pub pane_id: Option<String>,
}

fn created(reply: DaemonMsg) -> Result<CreatedIds, String> {
    match reply {
        DaemonMsg::Created { workspace, window, pane, .. } => Ok(CreatedIds {
            workspace_id: workspace.to_string(),
            window_id: window.map(|w| w.to_string()),
            pane_id: pane.map(|p| p.to_string()),
        }),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Create a workspace (plus its initial shell window).
#[tauri::command]
#[specta::specta]
pub async fn workspace_new(
    state: State<'_, AppState>,
    host_id: HostId,
    name: Option<String>,
) -> Result<CreatedIds, String> {
    let session = session_for(&state, host_id).await?;
    tracing::debug!(?name, "workspace_new");
    created(session.request(|id| session.client.new_workspace(id, name)).await?)
}

#[tauri::command]
#[specta::specta]
pub async fn workspace_kill(
    state: State<'_, AppState>,
    host_id: HostId,
    workspace_id: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .kill_workspace(workspace_id.parse::<WorkspaceId>()?)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn workspace_rename(
    state: State<'_, AppState>,
    host_id: HostId,
    workspace_id: String,
    name: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .rename_workspace(workspace_id.parse::<WorkspaceId>()?, name)
        .map_err(|e| e.to_string())
}

/// Open a window (one pane) in a workspace. `command` is an argv to
/// exec instead of the default login shell.
#[tauri::command]
#[specta::specta]
pub async fn window_new(
    state: State<'_, AppState>,
    host_id: HostId,
    workspace_id: String,
    name: Option<String>,
    cwd: Option<String>,
    command: Option<Vec<String>>,
) -> Result<CreatedIds, String> {
    let session = session_for(&state, host_id).await?;
    let workspace = workspace_id.parse::<WorkspaceId>()?;
    created(
        session
            .request(|id| session.client.new_window(id, workspace, name, cwd, command))
            .await?,
    )
}

#[tauri::command]
#[specta::specta]
pub async fn window_kill(
    state: State<'_, AppState>,
    host_id: HostId,
    window_id: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .kill_window(window_id.parse::<WindowId>()?)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn window_rename(
    state: State<'_, AppState>,
    host_id: HostId,
    window_id: String,
    name: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .rename_window(window_id.parse::<WindowId>()?, name)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SearchResult {
    pub matches: Vec<SearchHit>,
    pub truncated: bool,
}

/// Search scrollback on one host. `workspace_id` / `pane_id` narrow the
/// scope (pane wins if both given). The palette fans this out across
/// hosts itself.
#[tauri::command]
#[specta::specta]
pub async fn session_search(
    state: State<'_, AppState>,
    host_id: HostId,
    query: String,
    regex: bool,
    case_sensitive: bool,
    workspace_id: Option<String>,
    pane_id: Option<String>,
    max_results: u32,
) -> Result<SearchResult, String> {
    let session = session_for(&state, host_id).await?;
    let scope = if let Some(p) = pane_id {
        SearchScope::Pane(p.parse::<PaneId>()?)
    } else if let Some(w) = workspace_id {
        SearchScope::Workspace(w.parse::<WorkspaceId>()?)
    } else {
        SearchScope::All
    };
    let reply = session
        .request(|id| session.client.search(id, query, regex, case_sensitive, scope, max_results))
        .await?;
    match reply {
        DaemonMsg::SearchResults { matches, truncated, .. } => Ok(SearchResult {
            matches: to_domain_hits(&matches),
            truncated,
        }),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// The daemon's retained block table for a pane (oldest first). Called
/// once when a pane is first shown after (re)connect; live updates
/// arrive as `SessionEvent::Block`.
#[tauri::command]
#[specta::specta]
pub async fn session_blocks(
    state: State<'_, AppState>,
    host_id: HostId,
    pane_id: String,
) -> Result<Vec<BlockInfo>, String> {
    let session = session_for(&state, host_id).await?;
    tracing::debug!(%pane_id, "session_blocks");
    let pane = pane_id.parse::<PaneId>()?;
    match session.request(|id| session.client.blocks(id, pane)).await? {
        DaemonMsg::Blocks { blocks, .. } => Ok(blocks.iter().map(to_domain_block).collect()),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Round-trip time to the host's daemon in milliseconds — the status
/// bar's latency readout. Measures the real transport (socket or SSH),
/// not webview IPC.
#[tauri::command]
#[specta::specta]
pub async fn session_ping(state: State<'_, AppState>, host_id: HostId) -> Result<f64, String> {
    let session = session_for(&state, host_id).await?;
    let start = std::time::Instant::now();
    match session.request(|id| session.client.ping(id)).await? {
        DaemonMsg::Pong { .. } => Ok(start.elapsed().as_secs_f64() * 1000.0),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

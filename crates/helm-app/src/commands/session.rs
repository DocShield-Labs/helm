//! Per-host session commands — the IPC surface onto helmd.
//!
//! Ids cross the boundary as the daemon's u64s stringified (same form
//! the `SessionEvent` tree uses). Output flows the other way over the
//! event channel; these commands are the control plane.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use helm_domain::{
    BlockInfo, HistoryPage, HostId, PathCompletion, PathCompletionResult, PathEntryKind,
    ScreenInfo, SearchHit, SessionTree,
};
use helm_proto::{DaemonMsg, SearchScope, SessionId};
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
pub async fn session_tree(
    state: State<'_, AppState>,
    host_id: HostId,
) -> Result<SessionTree, String> {
    let session = session_for(&state, host_id).await?;
    let snapshot = session.tree.lock().clone();
    Ok(to_domain_tree(&snapshot))
}

/// Keystrokes / paste for a session. `data` is base64 so arbitrary bytes
/// (escape sequences, bracketed paste) cross the boundary intact.
#[tauri::command]
#[specta::specta]
pub async fn session_input(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
    data: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    let bytes = B64
        .decode(data.as_bytes())
        .map_err(|e| format!("bad base64: {e}"))?;
    tracing::trace!(%session_id, len = bytes.len(), "session_input");
    session
        .client
        .input(session_id.parse::<SessionId>()?, bytes)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn session_resize(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .resize(session_id.parse::<SessionId>()?, cols, rows)
        .map_err(|e| e.to_string())
}

/// The session's current grid, for the first paint of a session the frontend
/// shows (later changes stream as `SessionEvent::Screen` /
/// `SessionEvent::ScreenDiff`).
#[tauri::command]
#[specta::specta]
pub async fn session_screen(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
) -> Result<ScreenInfo, String> {
    let session = session_for(&state, host_id).await?;
    tracing::debug!(%session_id, "session_screen");
    let session_id = session_id.parse::<SessionId>()?;
    match session
        .request(|id| session.client.screen(id, session_id))
        .await?
    {
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
    session_id: String,
    from_line: u64,
    to_line: u64,
) -> Result<HistoryPage, String> {
    let session = session_for(&state, host_id).await?;
    tracing::debug!(%session_id, from_line, to_line, "session_history");
    let session_id = session_id.parse::<SessionId>()?;
    match session
        .request(|id| session.client.history(id, session_id, from_line, to_line))
        .await?
    {
        DaemonMsg::History {
            from_line,
            rows,
            history_start,
            top_line,
            ..
        } => Ok(HistoryPage {
            from_line,
            rows: rows.into_iter().map(to_domain_row).collect(),
            history_start,
            top_line,
        }),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Id returned by `session_new`.
#[derive(Debug, Clone, Serialize, Type)]
pub struct CreatedSession {
    pub session_id: String,
}

fn created(reply: DaemonMsg) -> Result<CreatedSession, String> {
    match reply {
        DaemonMsg::Created { session, .. } => Ok(CreatedSession {
            session_id: session.to_string(),
        }),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Create a long-running terminal session.
#[tauri::command]
#[specta::specta]
pub async fn session_new(
    state: State<'_, AppState>,
    host_id: HostId,
    name: Option<String>,
    cwd: Option<String>,
    command: Option<Vec<String>>,
) -> Result<CreatedSession, String> {
    let session = session_for(&state, host_id).await?;
    created(
        session
            .request(|id| session.client.new_session(id, name, cwd, command))
            .await?,
    )
}

#[tauri::command]
#[specta::specta]
pub async fn session_kill(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .kill_session(session_id.parse::<SessionId>()?)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn session_rename(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
    name: String,
) -> Result<(), String> {
    let session = session_for(&state, host_id).await?;
    session
        .client
        .rename_session(session_id.parse::<SessionId>()?, name)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SearchResult {
    pub matches: Vec<SearchHit>,
    pub truncated: bool,
}

/// Search scrollback on one host. `session_id` narrows the scope; the
/// palette fans all-host searches out itself.
#[tauri::command]
#[specta::specta]
pub async fn session_search(
    state: State<'_, AppState>,
    host_id: HostId,
    query: String,
    regex: bool,
    case_sensitive: bool,
    session_id: Option<String>,
    max_results: u32,
) -> Result<SearchResult, String> {
    let session = session_for(&state, host_id).await?;
    let scope = if let Some(session_id) = session_id {
        SearchScope::Session(session_id.parse::<SessionId>()?)
    } else {
        SearchScope::All
    };
    let reply = session
        .request(|id| {
            session
                .client
                .search(id, query, regex, case_sensitive, scope, max_results)
        })
        .await?;
    match reply {
        DaemonMsg::SearchResults {
            matches, truncated, ..
        } => Ok(SearchResult {
            matches: to_domain_hits(&matches),
            truncated,
        }),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// The daemon's retained block table for a session (oldest first). Called
/// once when a session is first shown after (re)connect; live updates
/// arrive as `SessionEvent::Block`.
#[tauri::command]
#[specta::specta]
pub async fn session_blocks(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
) -> Result<Vec<BlockInfo>, String> {
    let session = session_for(&state, host_id).await?;
    tracing::debug!(%session_id, "session_blocks");
    let session_id = session_id.parse::<SessionId>()?;
    match session
        .request(|id| session.client.blocks(id, session_id))
        .await?
    {
        DaemonMsg::Blocks { blocks, .. } => Ok(blocks.iter().map(to_domain_block).collect()),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn session_path_complete(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
    path: String,
    directories_only: bool,
    max_results: u32,
) -> Result<PathCompletionResult, String> {
    let session = session_for(&state, host_id).await?;
    let session_id = session_id.parse::<SessionId>()?;
    match session
        .request(|id| {
            session
                .client
                .complete_path(id, session_id, path, directories_only, max_results)
        })
        .await?
    {
        DaemonMsg::PathCompletions {
            candidates,
            truncated,
            ..
        } => Ok(PathCompletionResult {
            candidates: candidates
                .into_iter()
                .map(|candidate| PathCompletion {
                    value: candidate.value,
                    kind: match candidate.kind {
                        helm_proto::PathEntryKind::File => PathEntryKind::File,
                        helm_proto::PathEntryKind::Directory => PathEntryKind::Directory,
                    },
                })
                .collect(),
            truncated,
        }),
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

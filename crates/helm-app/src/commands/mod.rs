//! Tauri commands. Each is exposed to the frontend via specta-typed bindings.
//!
//! Organization:
//!   - [`host`]            — host registry, connect/disconnect, host-key
//!                           prompts, `~/.ssh/config` autocomplete, ping
//!   - [`tmux`]            — every `tmux_*` command (send_keys, list_*,
//!                           kill_*, capture_pane, resize_client, …)
//!   - [`notifications`]   — inbox: list/dismiss/dismiss-by-window, focus
//!   - [`tools`]           — tool-integration framework commands
//!
//! The connection state machine (do_connect, supervise, per-client
//! forwarders, multi-client connect helpers) lives in
//! [`crate::connection`] — kept out of this tree so command modules
//! stay focused on the IPC surface.

use helm_domain::{HostEvent, HostId};
use helm_tmux::TmuxClient;
use std::sync::Arc;
use tauri::State;
use tokio::sync::mpsc;

use crate::state::{AppState, SharedHostEntry};

pub mod host;
pub mod notifications;
pub mod tmux;
pub mod tools;

/// Small fire-and-forget event emit. Skips silently when the channel
/// hasn't been registered yet (frontend hasn't called `host_subscribe`)
/// or has been dropped (webview reload). Callers expect "best-effort
/// notify" semantics.
pub(crate) fn emit_event(
    tx: &Option<mpsc::UnboundedSender<HostEvent>>,
    event: HostEvent,
) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

/// Resolve the primary control client for a host. Multi-client model:
/// every per-session control client can service global commands
/// (pane/window/session ids are server-wide), so we just route through
/// the primary. Returns "host not connected" when no clients exist.
pub(crate) async fn tmux_for(
    state: &State<'_, AppState>,
    host_id: HostId,
) -> Result<Arc<TmuxClient>, String> {
    let entry: SharedHostEntry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let guard = entry.lock().await;
    guard
        .primary_client()
        .ok_or_else(|| "host not connected".to_string())
}

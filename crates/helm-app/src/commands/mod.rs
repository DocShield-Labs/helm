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

use helm_domain::{host_event_to_anchor_event, AnchorEvent, HostEvent, HostId};
use helm_tmux::TmuxClient;
use std::sync::Arc;
use tauri::State;
use tokio::sync::{broadcast, mpsc};

use crate::state::{AppState, SharedHostEntry};

pub mod anchor;
pub mod fs;
pub mod host;
pub mod notifications;
pub mod schedule;
pub mod system;
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

/// Emit a HostEvent to the Tauri frontend AND broadcast its AnchorEvent
/// equivalent (if any) to anchor RPC subscribers. Use at every emit
/// site whose event variant is translatable (notifications, schedules,
/// host registry). For untranslatable events (Tmux, Status, host-key
/// prompts), use `emit_event` directly — translating would just be a
/// branch that always returns None.
///
/// Cheap: the broadcast send is one Arc bump per subscriber slot;
/// when no anchor server is running, `send()` returns immediately with
/// `Err(SendError)` which we ignore.
pub(crate) fn emit_event_anchored(
    event_tx: &Option<mpsc::UnboundedSender<HostEvent>>,
    anchor_tx: &broadcast::Sender<AnchorEvent>,
    event: HostEvent,
) {
    if let Some(translated) = host_event_to_anchor_event(&event) {
        let _ = anchor_tx.send(translated);
    }
    if let Some(tx) = event_tx {
        let _ = tx.send(event);
    }
}

/// Cheap clone of the active subscriber's RPC client, if any. Returns
/// `None` when this helm process is in implicit-local or anchor mode
/// — in those cases commands hit the local db directly. The Mutex is
/// held only briefly to clone the inner `SubscriberClient` (one Arc
/// bump) so this is safe to call from hot paths.
pub(crate) fn subscriber_client(
    state: &State<'_, AppState>,
) -> Option<crate::subscriber::SubscriberClient> {
    state.subscriber.lock().as_ref().map(|h| h.client.clone())
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

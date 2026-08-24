//! Tauri commands. Each is exposed to the frontend via specta-typed bindings.
//!
//! Organization:
//!   - [`host`]            — host registry, connect/disconnect, host-key
//!                           prompts, `~/.ssh/config` autocomplete, ping
//!   - [`session`]         — everything that talks to a host's helmd:
//!                           input, resize, replay, session
//!                           lifecycle, search
//!   - [`notifications`]   — inbox: list/dismiss/dismiss-by-session, focus
//!   - [`tools`]           — tool-integration framework commands
//!
//! The connection state machine (connect, pump, supervisor) lives in
//! [`crate::connection`].

use helm_domain::{Host, HostEvent, HostId};
use helm_ssh::SshSession;
use std::sync::Arc;
use tauri::State;
use tokio::sync::mpsc;

use crate::state::{AppState, SessionHandle, SharedHostEntry};

pub mod host;
pub mod notifications;
pub mod session;
pub mod system;
pub mod tools;

/// Small fire-and-forget event emit. Skips silently when the channel
/// hasn't been registered yet (frontend hasn't called `host_subscribe`)
/// or has been dropped (webview reload).
pub(crate) fn emit_event(tx: &Option<mpsc::UnboundedSender<HostEvent>>, event: HostEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

/// Resolve the live helmd session for a host. "host not connected"
/// when there isn't one.
pub(crate) async fn session_for(
    state: &State<'_, AppState>,
    host_id: HostId,
) -> Result<Arc<SessionHandle>, String> {
    let entry: SharedHostEntry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let guard = entry.lock().await;
    guard
        .session
        .clone()
        .ok_or_else(|| "host not connected".to_string())
}

/// The host record + its SSH backing for a *connected* host — what
/// integrations need to read/write files on the right side of the wire.
pub(crate) async fn host_ctx(
    state: &State<'_, AppState>,
    host_id: HostId,
) -> Result<(Host, Option<Arc<SshSession>>), String> {
    let entry: SharedHostEntry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let guard = entry.lock().await;
    if guard.session.is_none() {
        return Err("host not connected".into());
    }
    Ok((guard.host.clone(), guard.ssh.clone()))
}

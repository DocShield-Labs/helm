//! Inbox commands. The list/dismiss surface for the per-session
//! notifications managed by `crate::notifications`. `set_focus` lives
//! here too — it's the active-session suppression knob the inbox layer
//! consults on every event.

use helm_domain::{HostEvent, HostId};
use tauri::State;

use crate::commands::emit_event;
use crate::state::AppState;

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
/// disconnect, session kill, dismiss-on-keystroke).
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
        .notification_by_session
        .remove(&(notif.host_id, notif.session_id));
    emit_event(
        &event_tx,
        HostEvent::NotificationDismissed {
            host_id: notif.host_id,
            notification_id,
        },
    );
    Ok(())
}

/// Dismiss the notification for a session after the user types into it.
#[tauri::command]
#[specta::specta]
pub async fn notification_dismiss_for_session(
    state: State<'_, AppState>,
    host_id: HostId,
    session_id: String,
) -> Result<(), String> {
    let event_tx = state.event_tx.lock().await.clone();
    let notif_ctx = state.notifications_ctx();
    crate::notifications::dismiss_for_sessions(&notif_ctx, &event_tx, host_id, &[session_id]);
    Ok(())
}

/// Tell the backend which (host, session) the user is currently looking
/// at. Pass `None`s to clear (helm window lost OS focus / minimized) so
/// backgrounded sessions resume getting notifications.
///
/// The notifications post-processor consults this on every event and
/// suppresses inbox rows for the focused session — the user is already
/// watching that output, an inbox entry would just be noise.
#[tauri::command]
#[specta::specta]
pub async fn set_focus(
    state: State<'_, AppState>,
    host_id: Option<HostId>,
    session_id: Option<String>,
) -> Result<(), String> {
    let mut guard = state.focus.lock();
    *guard = match (host_id, session_id) {
        (Some(host), Some(session)) if !session.is_empty() => Some((host, session)),
        _ => None,
    };
    Ok(())
}

//! Inbox commands. The list/dismiss surface for the per-pane
//! notifications managed by `crate::notifications`. `set_focus` lives
//! here too — it's the active-window suppression knob the inbox layer
//! consults on every event.

use helm_domain::{HostEvent, HostId, RpcOp, RpcResult};
use tauri::State;

use crate::commands::{emit_event_anchored, subscriber_client};
use crate::state::AppState;

/// Snapshot every live notification, ordered oldest-first by created_at.
/// The frontend uses this on boot to repopulate its inbox; subsequent
/// updates flow through the `Notification` / `NotificationDismissed`
/// HostEvent variants. In subscriber mode, the list comes from the
/// anchor over RPC instead of the local db — the local rows are
/// already kept in lockstep via the bridge.
#[tauri::command]
#[specta::specta]
pub async fn notifications_list(
    state: State<'_, AppState>,
) -> Result<Vec<helm_domain::Notification>, String> {
    if let Some(client) = subscriber_client(&state) {
        return match client.request(RpcOp::ListNotifications).await? {
            RpcResult::Notifications { mut notifications } => {
                if let Some(anchor_id) =
                    crate::subscriber::current_anchor_host_id(&state.hosts, state.local_host_id)
                {
                    for n in &mut notifications {
                        crate::subscriber::remap_notification(n, anchor_id);
                    }
                }
                Ok(notifications)
            }
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
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
    if let Some(client) = subscriber_client(&state) {
        // Anchor owns the row. Send the dismiss and let the event
        // bridge propagate the matching NotificationDismissed back
        // to us — the frontend will see the same event shape as if
        // we'd dismissed locally.
        return match client
            .request(RpcOp::DismissNotification { notification_id })
            .await?
        {
            RpcResult::Ack => Ok(()),
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
    let event_tx = state.event_tx.lock().await.clone();
    let anchor_tx = state.anchor_event_tx.clone();
    let Some((_, notif)) = state.notifications.remove(&notification_id) else {
        return Ok(());
    };
    state
        .notification_by_pane
        .remove(&(notif.host_id, notif.pane_id));
    if let Err(e) = state.db.delete_notification(notification_id) {
        tracing::warn!("notification dismiss persist failed: {e}");
    }
    emit_event_anchored(
        &event_tx,
        &anchor_tx,
        HostEvent::NotificationDismissed {
            host_id: notif.host_id,
            notification_id,
        },
    );
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

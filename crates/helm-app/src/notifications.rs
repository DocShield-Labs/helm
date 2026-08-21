//! Inbox notifications — coalescing + lifecycle, fed by helmd.
//!
//! The daemon produces per-pane notifications (bell, command done with
//! exit code / cmdline / duration, plus an ANSI-stripped preview). This
//! layer adds what only the app knows: window/workspace breadcrumbs
//! (from the latest tree snapshot), active-window suppression, and
//! coalescing so a chatty pane produces one inbox row, not a stack.
//!
//! Coalesce key is `(host_id, pane_id)`: one notification slot per pane.
//! Kind class ordering: non-zero `CommandDone` outranks `Bell` outranks
//! successful `CommandDone`; within a class the latest event wins.

use std::time::{SystemTime, UNIX_EPOCH};

use helm_domain::{HostEvent, HostId, Notification, NotificationId, NotificationKind};
use helm_proto::TreeSnapshot;
use tokio::sync::mpsc::UnboundedSender;

use crate::state::NotificationsCtx;
#[cfg(test)]
use crate::state::PaneRuntime;

/// Fold a daemon notification into the inbox.
pub fn process_daemon_notification(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host_id: HostId,
    note: &helm_proto::Notification,
) {
    let pane_id = note.pane.to_string();
    let kind = match &note.kind {
        helm_proto::NotificationKind::Bell => NotificationKind::Bell,
        helm_proto::NotificationKind::Message { text } => NotificationKind::Message { text: text.clone() },
        helm_proto::NotificationKind::CommandDone { exit_code, cmdline, duration_ms } => {
            NotificationKind::CommandDone {
                exit_code: Some(*exit_code),
                command: cmdline.clone().unwrap_or_default(),
                duration_ms: *duration_ms,
            }
        }
    };
    upsert(ctx, event_tx, host_id, &pane_id, kind, &note.preview, note.at_ms.max(unix_ms()));
}

/// Refresh the per-pane window/workspace breadcrumbs from a tree
/// snapshot and drop notifications for panes that no longer exist.
pub fn sync_pane_index(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host_id: HostId,
    snapshot: &TreeSnapshot,
) {
    let mut alive: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ws in &snapshot.workspaces {
        for w in &ws.windows {
            for p in &w.panes {
                let pane_id = p.id.to_string();
                alive.insert(pane_id.clone());
                let mut rt = ctx.pane_runtime.entry((host_id, pane_id)).or_default();
                rt.window_id = w.id.to_string();
                rt.workspace_id = ws.id.to_string();
            }
        }
    }
    // Anything we track for this host that isn't in the snapshot is
    // gone (window killed, shell exited) — whether we track it via a
    // runtime entry or only via an inbox row. Synthetic schedule panes
    // (`schedule:<id>`) aren't daemon panes — leave them alone.
    let is_stale = |pane: &String| !alive.contains(pane) && !pane.starts_with("schedule:");
    let mut stale: std::collections::HashSet<String> = ctx
        .pane_runtime
        .iter()
        .filter(|r| r.key().0 == host_id && is_stale(&r.key().1))
        .map(|r| r.key().1.clone())
        .collect();
    stale.extend(
        ctx.notification_by_pane
            .iter()
            .filter(|r| r.key().0 == host_id && is_stale(&r.key().1))
            .map(|r| r.key().1.clone()),
    );
    let stale: Vec<String> = stale.into_iter().collect();
    if !stale.is_empty() {
        dismiss_for_panes(ctx, event_tx, host_id, &stale);
        for pane_id in &stale {
            ctx.pane_runtime.remove(&(host_id, pane_id.clone()));
        }
    }
}

/// Create or coalesce a notification for `(host_id, pane_id)` and emit
/// a `HostEvent::Notification` so the frontend sees the upsert.
fn upsert(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host_id: HostId,
    pane_id: &str,
    incoming_kind: NotificationKind,
    preview: &str,
    now: u64,
) {
    let pane_key = (host_id, pane_id.to_string());
    let (window_id, workspace_id) = ctx
        .pane_runtime
        .get(&pane_key)
        .map(|rt| (rt.window_id.clone(), rt.workspace_id.clone()))
        .unwrap_or_default();

    // Active-window suppression: the user is already looking at it.
    // Only when we know the window — unknown errs toward notifying.
    if !window_id.is_empty() {
        if let Some((focused_host, focused_window)) = ctx.focus.lock().clone() {
            if focused_host == host_id && focused_window == window_id {
                return;
            }
        }
    }

    let id = match ctx.notification_by_pane.get(&pane_key).map(|r| *r) {
        Some(id) => {
            if let Some(mut existing) = ctx.notifications.get_mut(&id) {
                existing.kind = merged_kind(&existing.kind, &incoming_kind);
                existing.count = existing.count.saturating_add(1);
                existing.updated_at = now;
                existing.preview = preview.to_string();
                if !window_id.is_empty() {
                    existing.window_id = window_id.clone();
                }
                if !workspace_id.is_empty() {
                    existing.workspace_id = Some(workspace_id.clone());
                }
            }
            id
        }
        None => {
            let id = NotificationId::new();
            ctx.notifications.insert(
                id,
                Notification {
                    id,
                    host_id,
                    workspace_id: (!workspace_id.is_empty()).then_some(workspace_id),
                    window_id,
                    pane_id: pane_id.to_string(),
                    kind: incoming_kind,
                    created_at: now,
                    updated_at: now,
                    count: 1,
                    preview: preview.to_string(),
                },
            );
            ctx.notification_by_pane.insert(pane_key, id);
            id
        }
    };

    if let Some(notif) = ctx.notifications.get(&id).map(|r| r.clone()) {
        emit(event_tx, HostEvent::Notification { host_id, notification: notif });
    }
}

/// Coalesce priority. Higher wins when folding two events into one row.
///   3 — CommandDone with non-zero exit / ScheduleFailed
///   2 — Bell / Message (latest wins on a tie)
///   1 — CommandDone success / unknown
fn kind_priority(k: &NotificationKind) -> u8 {
    match k {
        NotificationKind::CommandDone { exit_code: Some(c), .. } if *c != 0 => 3,
        NotificationKind::ScheduleFailed { .. } => 3,
        NotificationKind::Bell | NotificationKind::Message { .. } => 2,
        NotificationKind::CommandDone { .. } => 1,
    }
}

fn merged_kind(existing: &NotificationKind, incoming: &NotificationKind) -> NotificationKind {
    if kind_priority(incoming) >= kind_priority(existing) {
        incoming.clone()
    } else {
        existing.clone()
    }
}

/// Dismiss every notification for `host_id` whose pane is in
/// `pane_ids`, emitting a `NotificationDismissed` for each. Leaves
/// `pane_runtime` alone — dismissal is independent of pane liveness.
pub fn dismiss_for_panes(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host_id: HostId,
    pane_ids: &[String],
) {
    for pane_id in pane_ids {
        let key = (host_id, pane_id.clone());
        if let Some((_, id)) = ctx.notification_by_pane.remove(&key) {
            ctx.notifications.remove(&id);
            emit(event_tx, HostEvent::NotificationDismissed { host_id, notification_id: id });
        }
    }
}

/// Drop every notification + per-pane runtime entry for `host_id`.
pub fn dismiss_for_host(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host_id: HostId,
) {
    let to_remove: Vec<(NotificationId, String)> = ctx
        .notification_by_pane
        .iter()
        .filter(|r| r.key().0 == host_id)
        .map(|r| (*r.value(), r.key().1.clone()))
        .collect();
    for (id, pane_id) in to_remove {
        ctx.notification_by_pane.remove(&(host_id, pane_id));
        ctx.notifications.remove(&id);
        emit(event_tx, HostEvent::NotificationDismissed { host_id, notification_id: id });
    }
    ctx.pane_runtime.retain(|key, _| key.0 != host_id);
}

fn emit(tx: &Option<UnboundedSender<HostEvent>>, event: HostEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;

    fn cmd_done(exit: Option<i32>) -> NotificationKind {
        NotificationKind::CommandDone { exit_code: exit, command: String::new(), duration_ms: None }
    }

    #[test]
    fn merged_kind_priority_bell_beats_success() {
        assert_eq!(merged_kind(&NotificationKind::Bell, &cmd_done(Some(0))), NotificationKind::Bell);
    }

    #[test]
    fn merged_kind_priority_failure_beats_bell() {
        assert_eq!(merged_kind(&NotificationKind::Bell, &cmd_done(Some(1))), cmd_done(Some(1)));
    }

    #[test]
    fn merged_kind_same_priority_latest_wins() {
        assert_eq!(merged_kind(&cmd_done(Some(1)), &cmd_done(Some(127))), cmd_done(Some(127)));
    }

    fn test_ctx() -> NotificationsCtx {
        NotificationsCtx {
            notifications: Arc::new(DashMap::new()),
            notification_by_pane: Arc::new(DashMap::new()),
            pane_runtime: Arc::new(DashMap::new()),
            focus: Arc::new(parking_lot::Mutex::new(None)),
            tool_integration_seen: Arc::new(DashMap::new()),
        }
    }

    #[test]
    fn upsert_coalesces_repeated_events() {
        let ctx = test_ctx();
        let host = HostId::new();
        upsert(&ctx, &None, host, "7", NotificationKind::Bell, "ding", 1000);
        upsert(&ctx, &None, host, "7", NotificationKind::Bell, "dong", 2000);
        assert_eq!(ctx.notifications.len(), 1);
        let id = *ctx.notification_by_pane.get(&(host, "7".to_string())).unwrap();
        let n = ctx.notifications.get(&id).unwrap();
        assert_eq!(n.count, 2);
        assert_eq!(n.updated_at, 2000);
        assert_eq!(n.created_at, 1000);
        assert_eq!(n.preview, "dong");
    }

    #[test]
    fn focused_window_is_suppressed() {
        let ctx = test_ctx();
        let host = HostId::new();
        ctx.pane_runtime.insert(
            (host, "7".into()),
            PaneRuntime { window_id: "3".into(), workspace_id: "1".into(), ..Default::default() },
        );
        *ctx.focus.lock() = Some((host, "3".into()));
        upsert(&ctx, &None, host, "7", NotificationKind::Bell, "", 1);
        assert!(ctx.notifications.is_empty());
        *ctx.focus.lock() = Some((host, "4".into()));
        upsert(&ctx, &None, host, "7", NotificationKind::Bell, "", 2);
        assert_eq!(ctx.notifications.len(), 1);
    }

    #[test]
    fn sync_pane_index_dismisses_stale_and_keeps_schedules() {
        let ctx = test_ctx();
        let host = HostId::new();
        upsert(&ctx, &None, host, "9", NotificationKind::Bell, "", 1);
        upsert(&ctx, &None, host, "schedule:x", NotificationKind::Bell, "", 1);
        assert_eq!(ctx.notifications.len(), 2);
        sync_pane_index(&ctx, &None, host, &TreeSnapshot { workspaces: vec![] });
        // Pane 9 is gone from the tree → dismissed; schedule row survives.
        assert_eq!(ctx.notifications.len(), 1);
        assert!(ctx.notification_by_pane.contains_key(&(host, "schedule:x".into())));
    }
}

//! Inbox notifications — coalescing, lifecycle, and the marker
//! post-processor that turns `OutputMarker`s extracted from pane output
//! into `HostEvent::Notification` events for the frontend.
//!
//! Coalesce key is `(host_id, pane_id)`: one notification slot per pane.
//! New events of the same kind class bump the count + refresh the
//! preview; new events of a different class replace the kind. Kind class
//! ordering: `CommandDone` outranks `Bell`, so a Bell that fires after
//! CommandDone is folded into the existing row's count rather than
//! demoting it back to Bell.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use helm_domain::{
    HostEvent, HostId, MarkerAt, Notification, NotificationId, NotificationKind, OutputMarker,
};
use helm_tmux::TmuxClient;
use tokio::sync::mpsc::UnboundedSender;

use crate::state::{NotificationsCtx, PaneRuntime, PREVIEW_BYTES};

/// Process every marker extracted from a single `%output` chunk for one
/// pane. Updates the pane's runtime state, creates or coalesces inbox
/// notifications, and emits `HostEvent::Notification` for any change.
///
/// Called once per `TmuxNotification::Output` from the supervisor's
/// forwarder loop, *after* the cleaned output bytes themselves have been
/// forwarded to the frontend (so the frontend's xterm sees the bytes
/// before the user sees the inbox row update — feels snappier).
pub fn process_output(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host_id: HostId,
    pane_id: &str,
    bytes: &[u8],
    markers: &[MarkerAt],
) {
    // Always update the per-pane preview ring, even when there are no
    // markers — we want a fresh preview ready the *next* time a marker
    // arrives. This is the cheap path for the steady stream of
    // pane output between markers.
    push_into_preview(ctx, host_id, pane_id, bytes);

    if markers.is_empty() {
        return;
    }

    let now = unix_ms();
    for m in markers {
        match &m.marker {
            OutputMarker::PromptStart { .. } => {
                // No notification triggered — but we use this to clear
                // any in-flight command timer that might still be set
                // from a previous command that didn't emit CommandDone
                // (e.g., shell entered a TUI). The next CommandStart
                // will restart the timer cleanly. We don't store cwd /
                // branch here because the inbox doesn't need them; the
                // frontend's BlockTracker reads them straight off the
                // marker via the wire event.
                with_runtime(ctx, host_id, pane_id, |rt| {
                    rt.command_started_at = None;
                    rt.command_text.clear();
                });
            }
            OutputMarker::CommandStart { command } => {
                let cmd = command.clone().unwrap_or_default();
                with_runtime(ctx, host_id, pane_id, |rt| {
                    rt.command_started_at = Some(now);
                    rt.command_text = cmd;
                });
            }
            OutputMarker::OutputStart => {
                // Reset the preview ring so the snapshot we take on
                // CommandDone is the *output* of this command, not the
                // tail of the previous one's output. The command line
                // itself is short and not interesting for the inbox
                // preview.
                with_runtime(ctx, host_id, pane_id, |rt| {
                    rt.output_ring.clear();
                });
            }
            OutputMarker::Bell => {
                upsert(
                    ctx,
                    event_tx,
                    host_id,
                    pane_id,
                    NotificationKind::Bell,
                    now,
                );
            }
            OutputMarker::CommandDone { exit_code } => {
                let (duration_ms, command) = with_runtime(ctx, host_id, pane_id, |rt| {
                    let dur = rt.command_started_at.map(|t| now.saturating_sub(t));
                    let cmd = std::mem::take(&mut rt.command_text);
                    rt.command_started_at = None;
                    (dur, cmd)
                });
                upsert(
                    ctx,
                    event_tx,
                    host_id,
                    pane_id,
                    NotificationKind::CommandDone {
                        exit_code: *exit_code,
                        command,
                        duration_ms,
                    },
                    now,
                );
            }
        }
    }
}

/// Insert `bytes` into the pane's preview ring, evicting from the front
/// to keep the buffer bounded at `PREVIEW_BYTES`. Stores raw bytes; ANSI
/// stripping happens lazily when we snapshot for a notification preview,
/// so this hot path stays cheap.
fn push_into_preview(ctx: &NotificationsCtx, host_id: HostId, pane_id: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    with_runtime(ctx, host_id, pane_id, |rt| {
        // Common case: incoming chunk fits — extend.
        if rt.output_ring.len() + bytes.len() <= PREVIEW_BYTES {
            rt.output_ring.extend_from_slice(bytes);
            return;
        }
        // Otherwise: drop the appropriate leading chunk so the new bytes
        // fit. If the incoming bytes alone are bigger than the ring,
        // just take their tail.
        if bytes.len() >= PREVIEW_BYTES {
            rt.output_ring.clear();
            rt.output_ring
                .extend_from_slice(&bytes[bytes.len() - PREVIEW_BYTES..]);
            return;
        }
        let drop = (rt.output_ring.len() + bytes.len()).saturating_sub(PREVIEW_BYTES);
        rt.output_ring.drain(..drop);
        rt.output_ring.extend_from_slice(bytes);
    });
}

/// Create or coalesce a notification for `(host_id, pane_id)` of the
/// given kind and emit a `HostEvent::Notification` so the frontend sees
/// the upsert. Uses `notification_by_pane` for O(1) coalesce lookup.
fn upsert(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host_id: HostId,
    pane_id: &str,
    incoming_kind: NotificationKind,
    now: u64,
) {
    let pane_key = (host_id, pane_id.to_string());

    // Snapshot the current preview ring + best-known window/session
    // ids while we're cheap. Holding the runtime guard across the
    // notifications upsert (next block) would invert lock order.
    let (preview, window_id, session_id) =
        runtime_snapshot(&ctx.pane_runtime, host_id, pane_id);
    let focus_snapshot = ctx.focus.lock().clone();
    if window_id.is_empty() {
        // Diagnostic — a notification with no window_id can't be
        // routed to a sidebar row breadcrumb or jumped to via click.
        // If you see this in logs after a refresh storm, the
        // refresh-lock + empty-list guard above didn't catch it and
        // we have a different race to chase.
        tracing::warn!(
            "notification upsert for pane {pane_id} on {host_id:?} has no window_id"
        );
    }

    // Active-window suppression: if the user is staring at the window
    // this notification belongs to, don't surface it. They're
    // *already* watching the output — an inbox row would be noise.
    // The frontend updates `focus` whenever the active host/window
    // changes (and clears it when the helm window loses OS focus, so
    // backgrounded panes still notify).
    //
    // Skip only when we have a known window_id from pane_runtime —
    // an empty window_id means the index hasn't refreshed yet, in
    // which case erring toward notifying is the right default
    // (occasional false positives are better than missing a real
    // event because we couldn't resolve breadcrumbs in time).
    if !window_id.is_empty() {
        if let Some((focused_host, focused_window)) = focus_snapshot {
            if focused_host == host_id && focused_window == window_id {
                return;
            }
        }
    }

    // Find or insert the coalesce slot.
    let id = match ctx.notification_by_pane.get(&pane_key).map(|r| *r) {
        Some(id) => {
            // Existing row for this pane — fold the incoming event in.
            if let Some(mut existing) = ctx.notifications.get_mut(&id) {
                existing.kind = merged_kind(&existing.kind, &incoming_kind);
                existing.count = existing.count.saturating_add(1);
                existing.updated_at = now;
                existing.preview = preview.clone();
                if !window_id.is_empty() {
                    existing.window_id = window_id.clone();
                }
                if !session_id.is_empty() {
                    existing.workspace_id = Some(session_id.clone());
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
                    workspace_id: if session_id.is_empty() {
                        None
                    } else {
                        Some(session_id)
                    },
                    window_id,
                    pane_id: pane_id.to_string(),
                    kind: incoming_kind,
                    created_at: now,
                    updated_at: now,
                    count: 1,
                    preview,
                },
            );
            ctx.notification_by_pane.insert(pane_key.clone(), id);
            id
        }
    };

    // Re-clone out of the registry so we emit the *current* shape,
    // including the merge above.
    if let Some(notif) = ctx.notifications.get(&id).map(|r| r.clone()) {
        emit(event_tx, HostEvent::Notification { host_id, notification: notif });
    }
}

/// Coalesce priority for kind merges. Higher priority wins when
/// folding two events into one inbox row.
///
///   3 — CommandDone with non-zero exit (failure is critical)
///   2 — Bell (explicit attention request)
///   1 — CommandDone with success / unknown exit (informational)
///
/// Within the same priority, the latest event wins so the row always
/// reflects the most recent exit code / duration / count. The previous
/// rule had CommandDone always beating Bell, which meant `printf '\a'`
/// would hide its own bell behind the trailing `exit 0`.
fn kind_priority(k: &NotificationKind) -> u8 {
    match k {
        NotificationKind::CommandDone { exit_code: Some(c), .. } if *c != 0 => 3,
        // Schedule failures sit alongside non-zero exits — both indicate
        // something the user actively needs to fix. Coalesce-priority
        // doesn't matter much in practice since the scheduler emits its
        // own pane key (`schedule:<id>`), so a ScheduleFailed row never
        // shares a slot with a real pane's CommandDone.
        NotificationKind::ScheduleFailed { .. } => 3,
        NotificationKind::Bell => 2,
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

/// Apply `f` to the runtime entry for `(host_id, pane_id)`, creating
/// the entry on demand. Returns whatever `f` returns. Holds the entry
/// guard for the duration — keep `f` short.
fn with_runtime<R>(
    ctx: &NotificationsCtx,
    host_id: HostId,
    pane_id: &str,
    f: impl FnOnce(&mut PaneRuntime) -> R,
) -> R {
    let key = (host_id, pane_id.to_string());
    let mut entry = ctx.pane_runtime.entry(key).or_default();
    f(entry.value_mut())
}

/// Snapshot the bits of `PaneRuntime` we need to populate a Notification:
/// ANSI-stripped tail of the preview ring, plus the best-known window /
/// session ids. Bounded length output (~120 chars) so the wire payload
/// stays small.
fn runtime_snapshot(
    runtime: &DashMap<(HostId, String), PaneRuntime>,
    host_id: HostId,
    pane_id: &str,
) -> (String, String, String) {
    let key = (host_id, pane_id.to_string());
    let Some(rt) = runtime.get(&key) else {
        return (String::new(), String::new(), String::new());
    };
    let preview = format_preview(&rt.output_ring);
    (preview, rt.window_id.clone(), rt.session_id.clone())
}

/// ANSI/control-stripping for the inbox preview.
///
/// Walks the bytes once and:
///   - drops CSI sequences (`ESC [ … <final byte 0x40-0x7E>`)
///   - drops OSC/DCS/SOS/PM/APC envelopes (`ESC [ ] P X ^ _ ] … ST`)
///   - drops single-char ESC + final-byte sequences (`ESC <byte 0x30-0x7E>`)
///   - drops carriage returns and other C0 control chars except newlines
///     and tabs (which are converted to spaces)
///   - collapses runs of whitespace (incl. consumed newlines/tabs) to
///     single spaces
///   - takes the last 120 *characters* of the result
///
/// Doesn't try to be a full ANSI lexer — we just want a sensible inbox
/// preview, not a faithful render. Falls back to lossy UTF-8 conversion
/// at the end so non-UTF-8 bytes don't crash.
pub const PREVIEW_CHAR_LIMIT: usize = 120;
fn format_preview(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'[' {
                // CSI: scan to first byte in 0x40..=0x7E (final byte).
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                i = (j + 1).min(bytes.len());
                continue;
            }
            if matches!(next, b']' | b'P' | b'X' | b'^' | b'_') {
                // ST-terminated envelope.
                let mut j = i + 2;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j.min(bytes.len());
                continue;
            }
            // Single-char ESC sequence (e.g., ESC = , ESC > for keypad).
            // Drop ESC + the next byte.
            if (0x30..=0x7e).contains(&next) {
                i += 2;
                continue;
            }
            // Lone ESC — skip just it.
            i += 1;
            continue;
        }
        // Strip C0 control bytes; convert newlines/tabs/CR to space.
        if b < 0x20 || b == 0x7f {
            if b == b'\n' || b == b'\r' || b == b'\t' {
                out.push(b' ');
            }
            i += 1;
            continue;
        }
        out.push(b);
        i += 1;
    }
    // Collapse whitespace runs.
    let s = String::from_utf8_lossy(&out);
    let collapsed: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Tail by chars (not bytes) so we don't slice mid-codepoint.
    if collapsed.chars().count() > PREVIEW_CHAR_LIMIT {
        collapsed
            .chars()
            .rev()
            .take(PREVIEW_CHAR_LIMIT)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        collapsed
    }
}

/// Dismiss every notification for `host_id` whose pane is in
/// `pane_ids`, emitting a `NotificationDismissed` for each. Used by
/// `notification_dismiss_for_window` (user typed in a live pane) and
/// by the refresh-stale path (pane is genuinely gone).
///
/// Deliberately does NOT touch `pane_runtime` — dismissing a
/// notification is independent of whether the pane is alive. The
/// refresh-stale caller handles its own runtime eviction explicitly,
/// since it's the only path that has confirmation the pane is dead.
/// (Earlier versions evicted runtime here too; the consequence was
/// subsequent events on the same live pane carrying empty window_id,
/// which broke active-window suppression and dismiss-on-keystroke
/// because both depend on the runtime cache.)
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
            emit(
                event_tx,
                HostEvent::NotificationDismissed {
                    host_id,
                    notification_id: id,
                },
            );
        }
    }
}

/// Drop every notification + per-pane runtime entry for `host_id`. Used
/// by the disconnect/delete paths so we don't leak inbox rows for hosts
/// the user just removed. Emits per-row dismiss events so the frontend
/// inbox clears in lockstep.
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
        emit(
            event_tx,
            HostEvent::NotificationDismissed {
                host_id,
                notification_id: id,
            },
        );
    }
    // Drop per-pane runtime for this host too.
    ctx.pane_runtime.retain(|key, _| key.0 != host_id);
}

/// Refresh the `pane_runtime` window/session mapping for every pane on
/// `host_id` from a one-shot `tmux list-panes` call. Cheap — one round
/// trip, single-line-per-pane response.
///
/// Run after every successful connect (so notifications emitted before
/// the frontend has its tree can still carry window/session ids), and
/// after window mutations (`%window-add`, etc.) to keep the index
/// fresh as the tmux tree shifts.
pub async fn refresh_pane_index(
    ctx: &NotificationsCtx,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    client: &Arc<TmuxClient>,
    host_id: HostId,
) -> Result<(), String> {
    // Serialize per host. Without this, multi-client deployments
    // trigger N parallel refreshes (every forwarder sees
    // %window-added / %sessions-changed and queues one), and their
    // stale-cleanup steps race — refresh A reads alive={X,Y} just
    // before pane Z is added; refresh B sees Z and writes it; A
    // processes its alive set, finds Z in pane_runtime but not in
    // its alive set, and dismisses Z's notification + runtime entry.
    let lock = ctx
        .refresh_locks
        .entry(host_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;

    let raw = client
        .list_panes("#{pane_id}|#{window_id}|#{session_id}")
        .await
        .map_err(|e| e.to_string())?;
    let mut alive: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in raw.split('\n').filter(|l| !l.is_empty()) {
        let mut parts = line.splitn(3, '|');
        let pane_id = parts.next().unwrap_or("").to_string();
        let window_id = parts.next().unwrap_or("").to_string();
        let session_id = parts.next().unwrap_or("").to_string();
        if pane_id.is_empty() {
            continue;
        }
        alive.insert(pane_id.clone());
        let key = (host_id, pane_id);
        let mut entry = ctx.pane_runtime.entry(key).or_default();
        entry.window_id = window_id;
        entry.session_id = session_id;
    }
    // Skip stale cleanup if the response was empty. Empty almost
    // always means a transient state (server briefly between sessions,
    // command-queue contention spitting out empty result) rather than
    // "every pane on the server was just killed." Without this guard,
    // an empty response wipes every pane_runtime entry for the host
    // and dismisses every notification — exactly the bug pattern we
    // saw with the multi-client refactor under load.
    if alive.is_empty() {
        return Ok(());
    }
    let stale: Vec<String> = ctx
        .pane_runtime
        .iter()
        .filter(|r| r.key().0 == host_id && !alive.contains(&r.key().1))
        .map(|r| r.key().1.clone())
        .collect();
    if !stale.is_empty() {
        dismiss_for_panes(ctx, event_tx, host_id, &stale);
        // Pane is genuinely gone (not in list-panes). Evict its runtime
        // entry too — `dismiss_for_panes` only handles the notification
        // side. If we left the runtime entry, it'd accumulate over the
        // host's lifetime as panes come and go.
        for pane_id in &stale {
            ctx.pane_runtime.remove(&(host_id, pane_id.clone()));
        }
    }
    Ok(())
}

fn emit(tx: &Option<UnboundedSender<HostEvent>>, event: HostEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_strips_ansi_csi() {
        let raw = b"hello \x1b[31;1merror\x1b[0m world\r\n";
        assert_eq!(format_preview(raw), "hello error world");
    }

    #[test]
    fn preview_collapses_whitespace() {
        let raw = b"a\n\n\nb\t\tc";
        assert_eq!(format_preview(raw), "a b c");
    }

    #[test]
    fn preview_strips_osc() {
        let raw = b"plain\x1b]0;title\x07after";
        assert_eq!(format_preview(raw), "plainafter");
    }

    #[test]
    fn preview_tails_to_120_chars() {
        let raw = vec![b'x'; 200];
        let s = format_preview(&raw);
        assert_eq!(s.chars().count(), PREVIEW_CHAR_LIMIT);
    }

    fn cmd_done(exit: Option<i32>) -> NotificationKind {
        NotificationKind::CommandDone {
            exit_code: exit,
            command: String::new(),
            duration_ms: None,
        }
    }

    #[test]
    fn merged_kind_priority_bell_beats_success() {
        // Bell (2) > CommandDone(ok) (1) — `printf '\a'` followed by
        // exit-0 keeps the bell visible instead of demoting to "done."
        let merged = merged_kind(&NotificationKind::Bell, &cmd_done(Some(0)));
        assert_eq!(merged, NotificationKind::Bell);
    }

    #[test]
    fn merged_kind_priority_failure_beats_bell() {
        // CommandDone(non-zero) (3) > Bell (2) — failures are critical
        // and outrank an explicit bell ring.
        let merged = merged_kind(&NotificationKind::Bell, &cmd_done(Some(1)));
        assert_eq!(merged, cmd_done(Some(1)));
    }

    #[test]
    fn merged_kind_same_priority_latest_wins() {
        // Two successive failures: the latest exit code is what the
        // user cares about, so we replace rather than keep the older.
        let merged = merged_kind(&cmd_done(Some(1)), &cmd_done(Some(127)));
        assert_eq!(merged, cmd_done(Some(127)));
    }

    /// Build a NotificationsCtx with empty maps suitable for upsert tests.
    fn test_ctx() -> NotificationsCtx {
        NotificationsCtx {
            notifications: Arc::new(DashMap::new()),
            notification_by_pane: Arc::new(DashMap::new()),
            pane_runtime: Arc::new(DashMap::new()),
            focus: Arc::new(parking_lot::Mutex::new(None)),
            refresh_locks: Arc::new(DashMap::new()),
            tool_integration_seen: Arc::new(DashMap::new()),
        }
    }

    #[test]
    fn upsert_coalesces_repeated_events() {
        // Two bells on the same pane should collapse to one notification
        // with count=2, not two separate inbox rows.
        let ctx = test_ctx();
        let host = HostId::new();
        let pane = "%1".to_string();

        upsert(&ctx, &None, host, &pane, NotificationKind::Bell, 1000);
        upsert(&ctx, &None, host, &pane, NotificationKind::Bell, 2000);

        assert_eq!(ctx.notifications.len(), 1, "should still be a single inbox row");
        let id = *ctx.notification_by_pane.get(&(host, pane)).unwrap();
        let n = ctx.notifications.get(&id).unwrap();
        assert_eq!(n.count, 2);
        assert_eq!(n.updated_at, 2000);
        assert_eq!(n.created_at, 1000);
    }
}

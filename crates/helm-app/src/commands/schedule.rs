//! Schedule CRUD + manual-fire commands. The supervisor lives in
//! [`crate::scheduler`]; this module is purely the IPC surface the
//! frontend's editor and palette wire onto.
//!
//! Every mutation persists to `helm.db` before returning, mirrors the
//! change into the in-memory registry, and signals the supervisor so
//! its next-fire cache rebuilds. Order matters: the supervisor must see
//! the registry update before its signal, otherwise it'll recompute
//! against stale state. We do registry → persist → signal.

use helm_domain::{HostEvent, RpcOp, RpcResult, Schedule, ScheduleId, ScheduleRun, Trigger};
use tauri::State;

use crate::commands::{emit_event_anchored, subscriber_client};
use crate::scheduler::{self, SchedulerSignal};
use crate::state::AppState;

/// Snapshot the full schedule registry. Most-recent-edit-first ordering
/// would require a separate timestamp; for v1 we sort by name so the
/// palette's "list schedules" view is stable.
#[tauri::command]
#[specta::specta]
pub async fn schedule_list(state: State<'_, AppState>) -> Result<Vec<Schedule>, String> {
    if let Some(client) = subscriber_client(&state) {
        return match client.request(RpcOp::ListSchedules).await? {
            RpcResult::Schedules { mut schedules } => {
                schedules.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                Ok(schedules)
            }
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
    let mut out: Vec<Schedule> = state.schedules.iter().map(|r| r.value().clone()).collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Upsert a schedule by id. Validates the trigger before persisting so
/// a bad cron expression doesn't silently never fire — the editor
/// surfaces the error message directly.
#[tauri::command]
#[specta::specta]
pub async fn schedule_save(
    state: State<'_, AppState>,
    schedule: Schedule,
) -> Result<ScheduleId, String> {
    validate_trigger(&schedule.trigger)?;
    if let Some(client) = subscriber_client(&state) {
        return match client.request(RpcOp::SaveSchedule { schedule }).await? {
            RpcResult::SavedSchedule { schedule_id } => Ok(schedule_id),
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
    let id = schedule.id;
    state.schedules.insert(id, schedule.clone());
    state.db.upsert_schedule(&schedule)?;

    let event_tx = state.event_tx.lock().await.clone();
    let anchor_tx = state.anchor_event_tx.clone();
    emit_event_anchored(
        &event_tx,
        &anchor_tx,
        HostEvent::ScheduleUpserted {
            schedule: schedule.clone(),
        },
    );
    scheduler::signal(&state, SchedulerSignal::Upserted(id));
    Ok(id)
}

/// Delete a schedule. Drops both the registry entry and any in-memory
/// run history. Idempotent — no error if the id is unknown.
#[tauri::command]
#[specta::specta]
pub async fn schedule_delete(
    state: State<'_, AppState>,
    schedule_id: ScheduleId,
) -> Result<(), String> {
    if let Some(client) = subscriber_client(&state) {
        return match client
            .request(RpcOp::DeleteSchedule { schedule_id })
            .await?
        {
            RpcResult::Ack => Ok(()),
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
    state.schedules.remove(&schedule_id);
    state.schedule_runs.remove(&schedule_id);
    state.db.delete_schedule(schedule_id)?;

    let event_tx = state.event_tx.lock().await.clone();
    let anchor_tx = state.anchor_event_tx.clone();
    emit_event_anchored(
        &event_tx,
        &anchor_tx,
        HostEvent::ScheduleRemoved { schedule_id },
    );
    scheduler::signal(&state, SchedulerSignal::Removed(schedule_id));
    Ok(())
}

/// Convenience: flip a schedule's enabled flag without re-sending the
/// whole record. The editor uses `schedule_save` for full edits;
/// row-level "pause / resume" buttons hit this.
#[tauri::command]
#[specta::specta]
pub async fn schedule_set_enabled(
    state: State<'_, AppState>,
    schedule_id: ScheduleId,
    enabled: bool,
) -> Result<(), String> {
    if let Some(client) = subscriber_client(&state) {
        return match client
            .request(RpcOp::SetScheduleEnabled {
                schedule_id,
                enabled,
            })
            .await?
        {
            RpcResult::Ack => Ok(()),
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
    let mut updated: Option<Schedule> = None;
    if let Some(mut entry) = state.schedules.get_mut(&schedule_id) {
        entry.value_mut().enabled = enabled;
        updated = Some(entry.value().clone());
    }
    let Some(updated) = updated else {
        return Err("unknown schedule".into());
    };
    state.db.upsert_schedule(&updated)?;
    let event_tx = state.event_tx.lock().await.clone();
    let anchor_tx = state.anchor_event_tx.clone();
    emit_event_anchored(
        &event_tx,
        &anchor_tx,
        HostEvent::ScheduleUpserted { schedule: updated },
    );
    scheduler::signal(&state, SchedulerSignal::Upserted(schedule_id));
    Ok(())
}

/// Fire a schedule immediately, regardless of trigger. Useful for the
/// editor's "Run now" button and for palette quick-actions on existing
/// schedules. Disabled schedules can still be manually fired (the run
/// is tagged `Manual` in history).
#[tauri::command]
#[specta::specta]
pub async fn schedule_run_now(
    state: State<'_, AppState>,
    schedule_id: ScheduleId,
) -> Result<(), String> {
    if let Some(client) = subscriber_client(&state) {
        return match client
            .request(RpcOp::RunScheduleNow { schedule_id })
            .await?
        {
            RpcResult::Ack => Ok(()),
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
    if !state.schedules.contains_key(&schedule_id) {
        return Err("unknown schedule".into());
    }
    scheduler::signal(&state, SchedulerSignal::RunNow(schedule_id));
    Ok(())
}

/// Recent runs for a schedule, most-recent-first. In-memory only —
/// survives only as long as the app instance.
#[tauri::command]
#[specta::specta]
pub async fn schedule_runs(
    state: State<'_, AppState>,
    schedule_id: ScheduleId,
) -> Result<Vec<ScheduleRun>, String> {
    Ok(state
        .schedule_runs
        .get(&schedule_id)
        .map(|r| r.value().clone())
        .unwrap_or_default())
}

/// Validate a trigger. Bad cron expressions error here so the editor
/// can show "expression doesn't parse"; bad timezones fall through to
/// UTC at fire time, but we still surface them as a warning at save
/// time so the user notices.
fn validate_trigger(t: &Trigger) -> Result<(), String> {
    match t {
        Trigger::Cron { expr, tz } => {
            // `parse_cron` accepts 5-field standard cron and normalizes
            // to the 6-field form the underlying parser expects, so a
            // user typing `0 22 * * *` doesn't get an "invalid expression"
            // error for what is in fact a perfectly good cron line.
            crate::scheduler::parse_cron(expr).map_err(|e| format!("cron: {e}"))?;
            // Empty tz means "use the local zone" at fire time —
            // that's fine. A non-empty tz that fails to parse is a
            // typo we should catch.
            if !tz.is_empty() {
                let _: chrono_tz::Tz = tz.parse().map_err(|e: chrono_tz::ParseError| {
                    format!("timezone: {e}")
                })?;
            }
            Ok(())
        }
        Trigger::Once { at } => {
            // `at == 0` is almost certainly an unset field. Reject so
            // the editor catches it before the schedule silently never
            // fires.
            if *at == 0 {
                return Err("Once trigger needs a target timestamp".into());
            }
            Ok(())
        }
        Trigger::Interval { seconds } => {
            if *seconds == 0 {
                return Err("Interval seconds must be > 0".into());
            }
            Ok(())
        }
    }
}

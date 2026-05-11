//! Local scheduler — fires user-defined `Schedule` entries by opening a
//! tmux window on the target host, cd'ing to the schedule's cwd, and
//! sending the body command.
//!
//! v1 is local-only: schedules live in `schedules.json` next to
//! `hosts.json` and only fire while the helm instance that created them
//! is running. The cloud/synced story is deferred and tracked alongside
//! the same work for notifications.
//!
//! ## Loop
//!
//! A single supervisor task runs forever:
//!   1. Walk every enabled schedule and compute its next-fire time.
//!   2. Sleep until the soonest one — *or* until a `SchedulerSignal`
//!      arrives (schedule added / changed / removed, or `RunNow`).
//!   3. Fire every schedule whose next-fire is `<= now` (handles the
//!      case where many fire at the same instant, e.g. multiple `* * * *
//!      *` crons).
//!   4. Persist the registry, emit `ScheduleUpserted` so the frontend's
//!      projection refreshes.
//!   5. Loop.
//!
//! ## Failure handling
//!
//! Per the design doc, the toast on every fire is noise — attention
//! follow-up reaches the user through the existing notification
//! pipeline (Bell / CommandDone). The one case where the scheduler
//! itself surfaces something is when a fire fails before it reaches the
//! shell: host disconnected, cwd missing, send-keys errored. Those are
//! pushed straight into the inbox as a `NotificationKind::ScheduleFailed`,
//! coalesced per schedule.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use cron::Schedule as CronSchedule;
use helm_domain::{
    HostEvent, HostId, Notification, NotificationId, NotificationKind, Schedule, ScheduleBody,
    ScheduleId, ScheduleRun, ScheduleRunId, ScheduleRunStatus, Trigger, WorkspaceTarget,
};
use helm_tmux::{quote_arg, TmuxClient};
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::commands::emit_event;
use crate::state::{AppState, NotificationsCtx, SCHEDULE_RUN_HISTORY_LIMIT};

/// One signal into the supervisor's mpsc. Tells the loop to wake up and
/// recompute its next-fire map (or, in the case of `RunNow`, fire a
/// specific schedule immediately regardless of its next-fire time).
#[derive(Debug)]
pub enum SchedulerSignal {
    /// A schedule was added or modified in `state.schedules`. The
    /// supervisor reloads its in-memory next-fire entry for the id.
    /// Sent by every `schedule_save` command.
    Upserted(ScheduleId),
    /// A schedule was removed from `state.schedules`. Drop the in-memory
    /// next-fire entry. Sent by `schedule_delete`.
    Removed(ScheduleId),
    /// User clicked "Run now". Fire immediately, bypassing the trigger
    /// schedule — but still post the run record + ScheduleFired event so
    /// the UI updates the same way it would for a natural fire.
    RunNow(ScheduleId),
}

/// Spawn the scheduler supervisor task. Stashes the signal sender on
/// `state.scheduler_tx` so the schedule_* commands can wake the loop.
/// Idempotent across re-entry: replacing the sender drops the old one
/// and the previous loop task self-exits when its receiver closes.
///
/// Uses `tauri::async_runtime::spawn` rather than `tokio::spawn`
/// because Tauri's `setup` callback runs *before* the tokio runtime
/// context is entered for the main thread — calling `tokio::spawn`
/// directly here panics with "there is no reactor running."
/// `async_runtime::spawn` routes through Tauri's managed runtime,
/// which is initialized as soon as the Builder begins running setup.
pub fn spawn_supervisor(app: &AppHandle) {
    let (tx, rx) = mpsc::unbounded_channel::<SchedulerSignal>();
    let app_handle = app.clone();
    {
        let state = app.state::<AppState>();
        // Replace any prior sender. The task spawned with that sender's
        // receiver drops on the next recv() and exits.
        let mut guard = state.scheduler_tx.lock();
        *guard = Some(tx);
    }
    tauri::async_runtime::spawn(async move {
        run(app_handle, rx).await;
    });
}

/// The supervisor body. Walks the schedules map, picks the soonest
/// next-fire, sleeps until then or until a signal lands, fires anyone
/// who's due, and loops.
async fn run(app: AppHandle, mut rx: mpsc::UnboundedReceiver<SchedulerSignal>) {
    info!("scheduler supervisor started");

    // Cache of next-fire timestamps per schedule. Recomputed on signal
    // and after every fire. Keeping this hot avoids reparsing crons
    // every loop iteration when nothing has changed.
    let mut next_fire: HashMap<ScheduleId, u64> = HashMap::new();
    rebuild_next_fire(&app, &mut next_fire);

    loop {
        // Pick the soonest-due fire. None when there are no enabled
        // schedules at all — in that case we just wait for a signal.
        let soonest = next_fire.values().copied().min();
        let now = unix_ms();
        let sleep_ms = soonest
            .map(|t| t.saturating_sub(now))
            // 1h ceiling: re-walk the registry hourly even when the
            // soonest cron is far in the future. Cheap insurance against
            // a stuck recompute if a future signal got dropped.
            .unwrap_or(60 * 60 * 1000);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {
                // Time elapsed — fire everyone whose next-fire is <= now.
                let now = unix_ms();
                let due: Vec<ScheduleId> = next_fire
                    .iter()
                    .filter_map(|(id, t)| if *t <= now { Some(*id) } else { None })
                    .collect();
                for id in due {
                    fire_one(&app, id, false).await;
                    // Recompute this schedule's next-fire after the run.
                    update_next_fire_for(&app, &mut next_fire, id);
                }
            }
            signal = rx.recv() => {
                let Some(signal) = signal else {
                    // Sender dropped — supervisor was replaced or app is
                    // exiting. Bail cleanly.
                    info!("scheduler supervisor exiting (sender dropped)");
                    return;
                };
                match signal {
                    SchedulerSignal::Upserted(id) => {
                        update_next_fire_for(&app, &mut next_fire, id);
                    }
                    SchedulerSignal::Removed(id) => {
                        next_fire.remove(&id);
                    }
                    SchedulerSignal::RunNow(id) => {
                        fire_one(&app, id, true).await;
                        update_next_fire_for(&app, &mut next_fire, id);
                    }
                }
            }
        }
    }
}

/// Walk every schedule in the registry and rebuild `next_fire` from
/// scratch. Called once at startup; signal-driven updates use
/// `update_next_fire_for` instead.
fn rebuild_next_fire(app: &AppHandle, next_fire: &mut HashMap<ScheduleId, u64>) {
    next_fire.clear();
    let state = app.state::<AppState>();
    let now = unix_ms();
    for entry in state.schedules.iter() {
        let s = entry.value();
        if !s.enabled {
            continue;
        }
        if let Some(t) = compute_next_fire(s, now) {
            next_fire.insert(s.id, t);
        }
    }
}

/// Recompute the next-fire entry for a single schedule. Called whenever
/// the schedule changes (add/edit/run) so we don't have to walk the
/// whole registry on every signal.
fn update_next_fire_for(
    app: &AppHandle,
    next_fire: &mut HashMap<ScheduleId, u64>,
    id: ScheduleId,
) {
    let state = app.state::<AppState>();
    let Some(entry) = state.schedules.get(&id) else {
        next_fire.remove(&id);
        return;
    };
    let s = entry.value();
    if !s.enabled {
        next_fire.remove(&id);
        return;
    }
    if let Some(t) = compute_next_fire(s, unix_ms()) {
        next_fire.insert(id, t);
    } else {
        next_fire.remove(&id);
    }
}

/// Compute the next unix-ms time at which `schedule` should fire,
/// strictly after `after_ms`. Returns None for triggers that have no
/// future fire (e.g. a `Once` whose `at` is in the past — those should
/// be flipped to `enabled: false` by the fire path so the supervisor
/// stops considering them).
fn compute_next_fire(schedule: &Schedule, after_ms: u64) -> Option<u64> {
    match &schedule.trigger {
        Trigger::Cron { expr, tz } => {
            let cron = parse_cron(expr).ok()?;
            let zone: Tz = tz.parse().unwrap_or(chrono_tz::UTC);
            let after = ms_to_datetime_in(after_ms, zone)?;
            // Cron's `after` is exclusive — strictly the next instance.
            let next_local = cron.after(&after).next()?;
            Some(datetime_to_ms(&next_local))
        }
        Trigger::Once { at } => {
            if *at > after_ms {
                Some(*at)
            } else {
                None
            }
        }
        Trigger::Interval { seconds } => {
            let secs = *seconds.max(&1) as u64;
            // Anchor on `last_fired_at` if we have one — otherwise just
            // schedule `secs` from now so the first fire isn't immediate.
            let base = schedule.last_fired_at.unwrap_or(after_ms);
            let mut next = base + secs * 1000;
            // Catch up if we slept through several intervals (app was
            // suspended, supervisor wedged briefly). Skip past missed
            // ones — we don't catch-up-fire by design.
            if next <= after_ms {
                let missed = (after_ms - next) / (secs * 1000) + 1;
                next += missed * secs * 1000;
            }
            Some(next)
        }
    }
}

/// Parse a cron expression, normalizing the universal 5-field format
/// (`m h dom mon dow`) into the seconds-prefixed 6-field format the
/// `cron` crate requires. We accept 5, 6, or 7 fields:
///
///   5 fields: standard Unix cron — prepend `0 ` for "at second 0"
///   6 fields: pass through (sec min hour dom mon dow)
///   7 fields: pass through (sec min hour dom mon dow year)
///
/// Anything else falls through to the parser, which will surface a
/// readable error to the editor.
pub(crate) fn parse_cron(expr: &str) -> Result<CronSchedule, cron::error::Error> {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    let normalized: String;
    let to_parse: &str = if field_count == 5 {
        normalized = format!("0 {trimmed}");
        &normalized
    } else {
        trimmed
    };
    CronSchedule::from_str(to_parse)
}

fn ms_to_datetime_in(ms: u64, tz: Tz) -> Option<DateTime<Tz>> {
    let secs = (ms / 1000) as i64;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    Utc.timestamp_opt(secs, nanos).single().map(|dt| dt.with_timezone(&tz))
}

fn datetime_to_ms<Tz: TimeZone>(dt: &DateTime<Tz>) -> u64 {
    dt.timestamp_millis().max(0) as u64
}

/// Fire a single schedule. Used both by the scheduled-time path and
/// by `RunNow`. The `manual` flag controls whether the run is recorded
/// as `ScheduleRunStatus::Manual` (when fired via the user's "Run now"
/// click while the schedule was disabled — runs are still allowed but
/// we tag them so the history view can distinguish).
async fn fire_one(app: &AppHandle, id: ScheduleId, manual: bool) {
    let state = app.state::<AppState>();
    let Some(schedule) = state.schedules.get(&id).map(|r| r.clone()) else {
        warn!(
            "scheduler: fire requested for unknown schedule id {:?} (manual={})",
            id, manual
        );
        return;
    };
    let started_at = unix_ms();
    let run_id = ScheduleRunId::new();
    let event_tx = state.event_tx.lock().await.clone();
    let notif_ctx = state.notifications_ctx();

    let outcome = spawn_scheduled_run(&state, &schedule).await;

    let status = match &outcome {
        Ok(_) if manual && !schedule.enabled => ScheduleRunStatus::Manual,
        Ok(_) => ScheduleRunStatus::Ok,
        Err(reason) => ScheduleRunStatus::Failed {
            reason: reason.clone(),
        },
    };

    // Record the run + advance `last_fired_at` regardless of outcome.
    // The user wants to see "tried at T, failed because X" in the
    // history view; failure shouldn't pretend the fire didn't happen.
    let mut updated_schedule = schedule.clone();
    updated_schedule.last_fired_at = Some(started_at);
    updated_schedule.last_run_status = Some(status.clone());
    // Once-shot triggers disable themselves after firing so the
    // supervisor stops reconsidering them on the next loop.
    if matches!(schedule.trigger, Trigger::Once { .. }) {
        updated_schedule.enabled = false;
    }
    state.schedules.insert(id, updated_schedule.clone());

    let run = ScheduleRun {
        id: run_id,
        schedule_id: id,
        started_at,
        finished_at: unix_ms(),
        status: status.clone(),
        window_id: outcome.as_ref().ok().cloned(),
    };
    push_run(&state, id, run);
    if let Err(e) = crate::schedules::save_schedules(&snapshot_schedules(&state)) {
        warn!("scheduler: persist failed: {e}");
    }

    match outcome {
        Ok(window_id) => {
            info!(
                "schedule fired: name={} window={} host={:?} manual={}",
                updated_schedule.name, window_id, updated_schedule.host_id, manual
            );
            emit_event(
                &event_tx,
                HostEvent::ScheduleFired {
                    schedule_id: id,
                    run_id,
                    started_at,
                    window_id,
                    manual,
                },
            );
        }
        Err(reason) => {
            warn!(
                "schedule fire failed: name={} reason={}",
                updated_schedule.name, reason
            );
            push_failure_notification(
                &notif_ctx,
                &event_tx,
                updated_schedule.host_id,
                id,
                &updated_schedule.name,
                &reason,
            );
        }
    }
    // Always emit ScheduleUpserted so the frontend's projection refreshes
    // its `last_fired_at` / `last_run_status` regardless of outcome.
    emit_event(
        &event_tx,
        HostEvent::ScheduleUpserted {
            schedule: updated_schedule,
        },
    );
}

/// The actual spawn path: ensure the host is connected, resolve the
/// target workspace (creating it if missing), open a new window at the
/// schedule's cwd, materialize the body into a command line, and send
/// it. Returns the new tmux window id on success.
async fn spawn_scheduled_run(
    state: &tauri::State<'_, AppState>,
    schedule: &Schedule,
) -> Result<String, String> {
    let entry = state
        .entry(schedule.host_id)
        .ok_or_else(|| "host not found".to_string())?;
    let primary = {
        let g = entry.lock().await;
        g.primary_client()
    };
    let primary = match primary {
        Some(p) => p,
        None => {
            // Host disconnected — bring it up via the same path
            // `host_connect` uses (host-key prompts + reconnect ladder
            // included) before failing. Lets a 9am cron on an idle
            // host "just work" instead of bouncing into the inbox.
            info!(
                "schedule '{}': host {:?} not connected — auto-connecting",
                schedule.name, schedule.host_id
            );
            crate::commands::host::connect_host_impl(state, schedule.host_id, None)
                .await
                .map_err(|e| format!("auto-connect failed: {e}"))?;
            entry
                .lock()
                .await
                .primary_client()
                .ok_or_else(|| "auto-connect produced no primary client".to_string())?
        }
    };

    // Materialize the rendered body onto the target host, then hand
    // tmux a tiny shell-command that runs it via the user's
    // interactive shell. `prepare_script` returns a ready-to-run
    // invoker string for whichever path it took (local fs::write vs
    // tmux paste-buffer roundtrip).
    let WorkspaceTarget::Named { name } = &schedule.workspace_target;
    let command = render_body(&schedule.body);
    let host_port = entry.lock().await.host.port;
    let invoker = prepare_script(&primary, host_port, &command).await?;
    let window_id = ensure_window(&primary, schedule, name, &invoker).await?;

    // Interactive Claude with a starter prompt: the prompt has been
    // pre-filled into Claude's input box via its positional argument
    // (`claude 'prompt'`), but Claude's TUI doesn't auto-submit — the
    // user would still have to press Enter manually. We do that for
    // them by polling `pane_current_command` until claude is the
    // foreground process, then sending a single Enter.
    //
    // Polling rather than a fixed sleep because Claude's boot time
    // varies wildly: <500ms when authenticated and warm, 5–10s on
    // first-launch / network round-trip. A fixed delay either misses
    // (TUI not up yet, keystroke buffered into a partially-mounted
    // input) or overruns (Claude already submitted the prompt
    // separately, our Enter creates an empty submission).
    if let ScheduleBody::ClaudeCode {
        prompt,
        non_interactive,
        ..
    } = &schedule.body
    {
        if !*non_interactive && !prompt.is_empty() {
            // Best-effort — failure here logs and moves on; the user
            // still has the prompt pre-filled and can hit Enter
            // manually.
            wait_and_press_enter(&primary, &window_id).await;
        }
    }

    Ok(window_id)
}

/// Poll the window's active pane until `pane_current_command` reports
/// `claude` (or its semver-shaped `process.title` rewrite), then send
/// a single `\r` to submit the prompt that was pre-filled via Claude's
/// positional-argv. 10s cap; if Claude hasn't booted by then, leave
/// the prompt unsent rather than firing keystrokes blindly.
async fn wait_and_press_enter(primary: &Arc<TmuxClient>, window_id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::time::Instant::now() >= deadline {
            warn!("schedule: claude didn't reach foreground within 10s — leaving prompt unsent");
            return;
        }
        let probe = primary
            .send_command(format!(
                "display-message -p -t {} '#{{pane_current_command}}'",
                window_id
            ))
            .await
            .ok();
        let process = probe.as_deref().unwrap_or("").trim();
        if process == "claude" || crate::tool_integrations::is_semver_like(process) {
            // Small grace period so Claude's input handler is wired
            // before we send Enter.
            tokio::time::sleep(Duration::from_millis(400)).await;
            if let Err(e) = primary.send_keys(window_id, b"\r").await {
                warn!("schedule: claude submit Enter failed: {e}");
            }
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}


/// Materialize the rendered command on the target host and return
/// the shell-command tmux should run as the pane's initial process.
///
/// Local: `fs::write` to a `.sh` and invoke `$SHELL -i <script>`.
///
/// Remote: write a base64 of the script to a `.b64` via tmux's
/// paste-buffer (rides the existing control channel — see
/// `write_file_via_buffer`), then decode at exec time using
/// `base64 -d < FILE` (the stdin form is portable across BSD and
/// GNU `base64`; the positional `base64 -d FILE` is GNU-only).
///
/// Both invokers run `$SHELL` with `-i` so the user's `.zshrc` /
/// `.bashrc` is sourced and `claude` is on PATH. The trailing
/// `rm -f` cleans up the temp file after the inner shell exits.
async fn prepare_script(
    primary: &Arc<TmuxClient>,
    host_port: u16,
    contents: &str,
) -> Result<String, String> {
    let id = Uuid::new_v4().simple().to_string();
    if host_port == 0 {
        let path = format!("/tmp/helm-sched-{id}.sh");
        let path_owned = path.clone();
        let contents_owned = contents.to_string();
        tokio::task::spawn_blocking(move || std::fs::write(&path_owned, contents_owned))
            .await
            .map_err(|e| format!("write task: {e}"))?
            .map_err(|e| format!("write script: {e}"))?;
        return Ok(format!(
            "${{SHELL:-/bin/sh}} -i {p}; rm -f {p}",
            p = quote_arg(&path)
        ));
    }
    let path = format!("/tmp/helm-sched-{id}.b64");
    let b64 = B64.encode(contents.as_bytes());
    let buffer_name = format!("helm_sched_{id}");
    primary
        .write_file_via_buffer(&buffer_name, &path, &b64)
        .await
        .map_err(|e| format!("tmux buffer write: {e}"))?;
    let pq = quote_arg(&path);
    Ok(format!(
        r#"${{SHELL:-/bin/sh}} -i -c "$(base64 -d < {pq})"; rm -f {pq}"#
    ))
}

/// Resolve the tmux window the schedule should run in. If the target
/// session already exists we open a fresh window inside it. If not, we
/// create the session and adopt its auto-seeded first window (rather
/// than spawning a second one and leaving the default zsh window
/// orphaned next to ours).
///
/// The adopted window keeps the cwd we passed to `new-session`, gets
/// renamed to the schedule's name (with `automatic-rename` disabled
/// inside `rename_window` so the rename sticks past Claude's
/// `process.title` rewrite), and sends-keys against its active pane
/// using the window-id target.
async fn ensure_window(
    primary: &Arc<TmuxClient>,
    schedule: &Schedule,
    session_name: &str,
    command: &str,
) -> Result<String, String> {
    // `list-sessions -F '#{session_id}|#{session_name}'` returns one
    // session per line. We use `|` as the field separator to match the
    // convention in `lib/host.ts` — TAB doesn't survive every SSH
    // server's PTY terminal-mode handling (some sshd setups mangle
    // control characters in non-interactive exec output), so any
    // remote host whose SSHd doesn't honor IUCLC etc. would have its
    // TABs silently rewritten and our parser would see one glued
    // string per line and fail to match. `|` round-trips cleanly.
    let raw = primary
        .list_sessions("#{session_id}|#{session_name}")
        .await
        .map_err(|e| format!("list-sessions failed: {e}"))?;
    let mut existing_session: Option<String> = None;
    for line in raw.lines() {
        let mut parts = line.splitn(2, '|');
        let id = parts.next().unwrap_or("").trim();
        let n = parts.next().unwrap_or("").trim();
        if n == session_name {
            existing_session = Some(id.to_string());
            break;
        }
    }

    if let Some(session_id) = existing_session {
        return primary
            .new_window_with_command_returning_id(
                Some(&session_id),
                Some(&schedule.name),
                Some(&schedule.cwd),
                command,
            )
            .await
            .map_err(|e| format!("new-window failed: {e}"));
    }

    // No matching session — create one with the command as its first
    // window's initial process. tmux returns the new window id directly
    // via `-P -F '#{window_id}'`, so we skip the previous list-windows
    // + rename-window pair entirely (the window is named at create
    // time via `-n`).
    primary
        .new_session_with_command_returning_window_id(
            session_name,
            &schedule.name,
            &schedule.cwd,
            command,
        )
        .await
        .map_err(|e| format!("new-session failed: {e}"))
}

/// Render a schedule body into the literal command line that goes to
/// `send-keys`. Shell bodies are passed through verbatim; Claude bodies
/// expand into one of:
///   - `claude` — interactive, no starter prompt
///   - `claude '<prompt>'` — interactive, prompt submitted on launch
///     (Claude's positional-arg form starts a TUI seeded with the prompt)
///   - `claude -p '<prompt>'` — non-interactive print mode
/// plus optional `--model …` and `--dangerously-skip-permissions`.
///
/// Using the positional arg for the interactive case avoids the
/// previous "launch then sleep then send-keys" race — the prompt is
/// just argv from claude's POV and arrives reliably regardless of how
/// long the TUI takes to boot.
fn render_body(body: &ScheduleBody) -> String {
    match body {
        ScheduleBody::Shell { command } => command.clone(),
        ScheduleBody::ClaudeCode {
            prompt,
            non_interactive,
            model,
            dangerously_skip_permissions,
        } => {
            let mut parts: Vec<String> = vec!["claude".to_string()];
            if let Some(m) = model {
                if !m.is_empty() {
                    parts.push("--model".to_string());
                    parts.push(quote_arg(m));
                }
            }
            if *dangerously_skip_permissions {
                parts.push("--dangerously-skip-permissions".to_string());
            }
            if !prompt.is_empty() {
                if *non_interactive {
                    parts.push("-p".to_string());
                }
                // Positional arg in the interactive case; `-p`'s value
                // in the non-interactive case. Claude parses positional
                // args unambiguously after flags, so no `--` separator
                // needed.
                parts.push(quote_arg(prompt));
            }
            parts.join(" ")
        }
    }
}

/// Push a `ScheduleFailed` notification into the inbox for `host_id`.
/// Coalesces per schedule via a synthetic pane id of `schedule:<id>` so
/// the existing pane-keyed coalesce machinery handles "the same schedule
/// failed twice in a row" without growing the inbox.
fn push_failure_notification(
    ctx: &NotificationsCtx,
    event_tx: &Option<mpsc::UnboundedSender<HostEvent>>,
    host_id: HostId,
    schedule_id: ScheduleId,
    schedule_name: &str,
    reason: &str,
) {
    let synthetic_pane = format!("schedule:{}", schedule_id.0);
    let now = unix_ms();
    let kind = NotificationKind::ScheduleFailed {
        schedule_id,
        schedule_name: schedule_name.to_string(),
        reason: reason.to_string(),
    };
    let pane_key = (host_id, synthetic_pane.clone());
    let id = match ctx.notification_by_pane.get(&pane_key).map(|r| *r) {
        Some(id) => {
            if let Some(mut existing) = ctx.notifications.get_mut(&id) {
                existing.kind = kind;
                existing.count = existing.count.saturating_add(1);
                existing.updated_at = now;
                existing.preview = reason.to_string();
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
                    workspace_id: None,
                    // Schedule failures aren't tied to a real window —
                    // synthesize a sentinel so the frontend can tell
                    // them apart from pane-bound rows and route the
                    // click to the schedule editor instead of a tmux
                    // window.
                    window_id: synthetic_pane.clone(),
                    pane_id: synthetic_pane.clone(),
                    kind,
                    created_at: now,
                    updated_at: now,
                    count: 1,
                    preview: reason.to_string(),
                },
            );
            ctx.notification_by_pane.insert(pane_key, id);
            id
        }
    };
    if let Some(notif) = ctx.notifications.get(&id).map(|r| r.clone()) {
        emit_event(
            event_tx,
            HostEvent::Notification {
                host_id,
                notification: notif,
            },
        );
    }
}

/// Append `run` to the schedule's history ring, evicting the oldest
/// entries past `SCHEDULE_RUN_HISTORY_LIMIT`. Most-recent-first.
fn push_run(state: &tauri::State<'_, AppState>, id: ScheduleId, run: ScheduleRun) {
    let mut entry = state.schedule_runs.entry(id).or_default();
    let v = entry.value_mut();
    v.insert(0, run);
    if v.len() > SCHEDULE_RUN_HISTORY_LIMIT {
        v.truncate(SCHEDULE_RUN_HISTORY_LIMIT);
    }
}

/// Take a snapshot of the schedules registry as a Vec for persistence.
/// Walking the DashMap in place would race with concurrent inserts; the
/// snapshot keeps the on-disk file consistent with a single point-in-time.
fn snapshot_schedules(state: &tauri::State<'_, AppState>) -> Vec<Schedule> {
    state.schedules.iter().map(|r| r.value().clone()).collect()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Helper used by `state.scheduler_tx` callers to nudge the supervisor
/// without each call needing to handle the "supervisor not running yet"
/// case (very-early app boot).
pub fn signal(state: &tauri::State<'_, AppState>, sig: SchedulerSignal) {
    if let Some(tx) = state.scheduler_tx.lock().as_ref() {
        let _ = tx.send(sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule_with_trigger(trigger: Trigger) -> Schedule {
        Schedule {
            id: ScheduleId::new(),
            name: "test".into(),
            host_id: HostId::new(),
            cwd: "/tmp".into(),
            body: ScheduleBody::Shell {
                command: "echo hi".into(),
            },
            trigger,
            workspace_target: WorkspaceTarget::Named {
                name: "scheduled".into(),
            },
            enabled: true,
            last_fired_at: None,
            last_run_status: None,
        }
    }

    #[test]
    fn next_fire_interval_anchors_on_now_for_first_run() {
        let s = schedule_with_trigger(Trigger::Interval { seconds: 60 });
        let now = 1_000_000;
        let next = compute_next_fire(&s, now).unwrap();
        assert_eq!(next, now + 60_000, "first interval fire should be now + period");
    }

    #[test]
    fn next_fire_interval_advances_after_fire() {
        let mut s = schedule_with_trigger(Trigger::Interval { seconds: 60 });
        let now = 1_000_000;
        s.last_fired_at = Some(now - 30_000);
        let next = compute_next_fire(&s, now).unwrap();
        assert_eq!(next, now - 30_000 + 60_000);
    }

    #[test]
    fn next_fire_interval_skips_missed_intervals() {
        // App slept through 5 minutes of an every-1-min interval; we
        // should jump to the *next* future fire, not catch up by firing
        // 5 times.
        let mut s = schedule_with_trigger(Trigger::Interval { seconds: 60 });
        let last = 1_000_000;
        let now = last + 5 * 60_000 + 1;
        s.last_fired_at = Some(last);
        let next = compute_next_fire(&s, now).unwrap();
        assert!(next > now);
        assert_eq!((next - last) % 60_000, 0, "should land on an interval boundary");
    }

    #[test]
    fn next_fire_once_returns_none_if_in_past() {
        let s = schedule_with_trigger(Trigger::Once { at: 500 });
        assert!(compute_next_fire(&s, 1000).is_none());
    }

    #[test]
    fn next_fire_once_returns_at_if_in_future() {
        let s = schedule_with_trigger(Trigger::Once { at: 2000 });
        assert_eq!(compute_next_fire(&s, 1000), Some(2000));
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(quote_arg("hello"), "'hello'");
        assert_eq!(quote_arg("it's"), "'it'\\''s'");
        assert_eq!(quote_arg("$HOME"), "'$HOME'", "no var expansion");
    }

    #[test]
    fn render_body_shell_passes_through() {
        let cmd = render_body(&ScheduleBody::Shell {
            command: "echo $PATH | head -1".into(),
        });
        assert_eq!(cmd, "echo $PATH | head -1");
    }

    #[test]
    fn render_body_claude_non_interactive_with_prompt() {
        let cmd = render_body(&ScheduleBody::ClaudeCode {
            prompt: "summarize the docs".into(),
            non_interactive: true,
            model: None,
            dangerously_skip_permissions: false,
        });
        assert_eq!(cmd, "claude -p 'summarize the docs'");
    }

    #[test]
    fn render_body_claude_interactive_passes_prompt_positional() {
        // Interactive: prompt becomes a positional arg to `claude` so
        // the TUI starts with it submitted — no race-prone follow-up
        // keystroke needed.
        let cmd = render_body(&ScheduleBody::ClaudeCode {
            prompt: "summarize the docs".into(),
            non_interactive: false,
            model: None,
            dangerously_skip_permissions: false,
        });
        assert_eq!(cmd, "claude 'summarize the docs'");
    }

    #[test]
    fn render_body_claude_interactive_no_prompt() {
        // Interactive with empty prompt — bare `claude`.
        let cmd = render_body(&ScheduleBody::ClaudeCode {
            prompt: "".into(),
            non_interactive: false,
            model: None,
            dangerously_skip_permissions: false,
        });
        assert_eq!(cmd, "claude");
    }

    #[test]
    fn parse_cron_accepts_standard_5_field() {
        // Standard Unix cron: `m h dom mon dow`. The user's "every day
        // at 22:00" — must succeed.
        assert!(parse_cron("0 22 * * *").is_ok());
        assert!(parse_cron("*/15 * * * *").is_ok());
        assert!(parse_cron("0 9 * * 1-5").is_ok());
    }

    #[test]
    fn parse_cron_accepts_6_field_native() {
        // The cron crate's native format already includes seconds.
        assert!(parse_cron("0 0 22 * * *").is_ok());
    }

    #[test]
    fn parse_cron_rejects_garbage() {
        assert!(parse_cron("not a cron").is_err());
        assert!(parse_cron("99 99 99 99 99").is_err());
    }

    #[test]
    fn render_body_claude_with_flags() {
        let cmd = render_body(&ScheduleBody::ClaudeCode {
            prompt: "do thing".into(),
            non_interactive: true,
            model: Some("claude-opus-4-7".into()),
            dangerously_skip_permissions: true,
        });
        assert_eq!(
            cmd,
            "claude --model 'claude-opus-4-7' --dangerously-skip-permissions -p 'do thing'"
        );
    }
}

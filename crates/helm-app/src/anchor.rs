//! Anchor RPC server.
//!
//! When this machine is the canonical anchor (`localhost.is_anchor =
//! true`), this module runs a unix-socket listener that accepts
//! connections from subscriber helm processes and serves the
//! notification / schedule / host state over a newline-delimited JSON
//! protocol (`helm_domain::RpcClientMessage` / `RpcServerMessage`).
//!
//! Phase 1b scope is intentionally narrow: handshake + notification
//! list + dismiss + a subscribe stream that pushes
//! `AnchorEvent::Notification` and `AnchorEvent::NotificationDismissed`.
//! Schedule / host ops + the SSH-piped transport (1c) land in later
//! sub-phases.
//!
//! Loopback only. Subscriber transport (SSH-piped stdio) is 1c.

use std::path::PathBuf;
use std::sync::Arc;

use helm_domain::{
    AnchorEvent, RpcClientMessage, RpcOp, RpcResult, RpcServerMessage,
};
use tauri::AppHandle;
use tauri::Manager;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Entry point for the `helm anchor-rpc` subcommand. Connects to the
/// local anchor unix socket and bidirectionally proxies the current
/// process's stdin/stdout. This is what runs on the anchor machine
/// when a subscriber opens an SSH session and runs `helm anchor-rpc`
/// on the remote — SSH ferries the bytes; this proxy splices them
/// onto the local server's socket.
///
/// Standalone runtime (we're not inside Tauri here, so
/// `tauri::async_runtime` would also work but is needless extra
/// machinery; a plain tokio runtime is simpler and faster to spin up).
/// Exits 0 on clean EOF, 1 on connect failure or I/O error.
pub fn run_stdio_proxy() -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("anchor-rpc: build runtime: {e}");
            return 1;
        }
    };
    rt.block_on(async {
        let path = match socket_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("anchor-rpc: socket path: {e}");
                return 1;
            }
        };
        let stream = match tokio::net::UnixStream::connect(&path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "anchor-rpc: connect {:?}: {e} (is helm running and the anchor here?)",
                    path
                );
                return 1;
            }
        };
        let (mut sock_rd, mut sock_wr) = stream.into_split();
        // Run the two copy directions independently and wait for BOTH
        // to finish. Naive `select!` would exit as soon as stdin EOFed
        // (one-shot `printf … | helm anchor-rpc`), leaving the
        // server's reply unread. Instead:
        //   stdin → socket-write: copies until stdin EOF, then
        //       shutdown(SHUT_WR) on the socket so the server sees
        //       client EOF and can close its half cleanly.
        //   socket-read → stdout: copies until the server closes its
        //       write half (i.e., the connection has been fully
        //       drained).
        let upstream = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let r = tokio::io::copy(&mut stdin, &mut sock_wr).await;
            use tokio::io::AsyncWriteExt;
            let _ = sock_wr.shutdown().await;
            r
        });
        let downstream = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            tokio::io::copy(&mut sock_rd, &mut stdout).await
        });
        let (up_res, down_res) = match tokio::try_join!(upstream, downstream) {
            Ok((a, b)) => (a, b),
            Err(e) => {
                eprintln!("anchor-rpc: task join: {e}");
                return 1;
            }
        };
        if let Err(e) = up_res {
            eprintln!("anchor-rpc: stdin→socket: {e}");
            return 1;
        }
        if let Err(e) = down_res {
            eprintln!("anchor-rpc: socket→stdout: {e}");
            return 1;
        }
        0
    })
}

/// Live anchor RPC server. Holds the accept loop's join handle so
/// `shutdown()` can abort it. Removes the socket file on shutdown so
/// a follow-up spawn doesn't trip the "address already in use" check.
pub struct AnchorServerHandle {
    join: tauri::async_runtime::JoinHandle<()>,
}

impl AnchorServerHandle {
    pub fn shutdown(self) {
        self.join.abort();
        if let Ok(path) = socket_path() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

const APP_DIR: &str = "Helm";
const SOCKET_FILE: &str = "anchor.sock";

/// Resolve the on-disk socket path. Same parent dir as `helm.db`.
pub fn socket_path() -> Result<PathBuf, String> {
    let base = dirs::config_dir().ok_or_else(|| "could not locate config dir".to_string())?;
    let dir = base.join(APP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir({:?}): {e}", dir))?;
    Ok(dir.join(SOCKET_FILE))
}

/// Spawn the accept loop on the unix socket. Returns the abort handle
/// so the caller (state.anchor_server) can shut it down on
/// anchor-flag flip. Idempotent on caller side — caller should abort
/// any prior handle before replacing.
///
/// Removes any stale socket file from a prior process before binding;
/// unix sockets don't survive a graceful close cleanly on macOS (the
/// file persists, and a fresh bind to the same path errors with
/// "Address already in use"). Removing-then-binding is the standard
/// idiom and safe because we only ever bind from a single helm
/// process per machine.
pub fn spawn(app: &AppHandle) -> Result<AnchorServerHandle, String> {
    let path = socket_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| format!("remove stale {:?}: {e}", path))?;
    }
    // Bind synchronously with std — works from any thread. The
    // tokio-adaptation step (`UnixListener::from_std`) registers the
    // socket with the current thread's tokio reactor, so it has to
    // run INSIDE the spawned task: when this function is called from
    // Tauri's `setup` callback there's no reactor on the main thread
    // yet, and `from_std` panics with "no reactor running."
    let listener_std = std::os::unix::net::UnixListener::bind(&path)
        .map_err(|e| format!("bind {:?}: {e}", path))?;
    listener_std
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;

    let app_handle = app.clone();
    let path_for_log = path.clone();
    let join = tauri::async_runtime::spawn(async move {
        let listener = match UnixListener::from_std(listener_std) {
            Ok(l) => l,
            Err(e) => {
                warn!("anchor RPC: tokio adapt failed: {e}");
                return;
            }
        };
        info!("anchor RPC server listening on {:?}", path_for_log);
        accept_loop(app_handle, listener).await;
    });
    Ok(AnchorServerHandle { join })
}

async fn accept_loop(app: AppHandle, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                debug!("anchor RPC: accepted connection");
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = handle_connection(app, stream).await {
                        warn!("anchor RPC connection ended with error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("anchor RPC accept failed: {e}");
                // Brief breather so a transient error doesn't tight-loop.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// One connection's request/response/event loop. Reads NDJSON
/// `RpcClientMessage`s, dispatches each to a handler, writes the
/// reply. Holds a `broadcast::Receiver<AnchorEvent>` after the first
/// `Subscribe` op; events forward into the write half via
/// `select!`-multiplexed reads.
async fn handle_connection(app: AppHandle, stream: UnixStream) -> Result<(), String> {
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));

    // Subscriber state. None until the client sends Subscribe; Some
    // afterward, holding the receiver we forward events from. Wrapped
    // in Option so the same select! can branch on its presence.
    let mut events: Option<broadcast::Receiver<AnchorEvent>> = None;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        debug!("anchor RPC: client closed");
                        return Ok(());
                    }
                    Err(e) => return Err(format!("read: {e}")),
                };
                if line.trim().is_empty() {
                    continue;
                }
                let request: RpcClientMessage = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("anchor RPC: bad request json: {e} (line: {line})");
                        continue;
                    }
                };
                handle_request(&app, &writer, &mut events, request).await?;
            }
            evt = next_event(&mut events) => {
                let Some(evt) = evt else { continue };
                let msg = RpcServerMessage::Event { event: evt };
                if let Err(e) = write_message(&writer, &msg).await {
                    return Err(format!("write event: {e}"));
                }
            }
        }
    }
}

/// Helper: yield-forever when `events` is None, recv when Some. Lets
/// the select! arm above branch cleanly on subscription state without
/// branching the whole arm.
async fn next_event(
    events: &mut Option<broadcast::Receiver<AnchorEvent>>,
) -> Option<AnchorEvent> {
    match events {
        Some(rx) => loop {
            match rx.recv().await {
                Ok(evt) => return Some(evt),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("anchor RPC: subscriber lagged {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
        None => std::future::pending().await,
    }
}

async fn handle_request(
    app: &AppHandle,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    events: &mut Option<broadcast::Receiver<AnchorEvent>>,
    msg: RpcClientMessage,
) -> Result<(), String> {
    let RpcClientMessage::Request { id, op } = msg;
    let reply = match op {
        RpcOp::Hello { hostname } => {
            let state = app.state::<AppState>();
            let your_id_on_anchor = match_subscriber_hostname(&state, &hostname);
            Ok(RpcResult::Hello {
                version: env!("CARGO_PKG_VERSION").to_string(),
                your_id_on_anchor,
            })
        }
        RpcOp::Subscribe => {
            let state = app.state::<AppState>();
            // Subscribe BEFORE snapshotting so we can't miss any event
            // emitted between snapshot read and subscribe (gap would
            // leave the subscriber permanently inconsistent until next
            // dismiss).
            *events = Some(state.anchor_event_tx.subscribe());
            let notifications = state.db.list_notifications().unwrap_or_default();
            let schedules = state.db.list_schedules().unwrap_or_default();
            // Send every host EXCEPT this machine's own localhost.
            // The subscriber already has its own representation of
            // the anchor (its remote-host entry whose `is_anchor` is
            // true); shipping our localhost here would either
            // duplicate or overwrite that with anchor-side metadata.
            let hosts: Vec<helm_domain::Host> = state
                .db
                .list_hosts()
                .unwrap_or_default()
                .into_iter()
                .filter(|h| h.id != helm_domain::HostId::local())
                .collect();
            Ok(RpcResult::Subscribed {
                notifications,
                schedules,
                hosts,
            })
        }
        RpcOp::ListNotifications => {
            let state = app.state::<AppState>();
            match state.db.list_notifications() {
                Ok(notifications) => Ok(RpcResult::Notifications { notifications }),
                Err(e) => Err(e),
            }
        }
        RpcOp::DismissNotification { notification_id: notif_id } => {
            let state = app.state::<AppState>();
            // Route through the existing in-memory + db + event path
            // so subscribers see the same NotificationDismissed event
            // a frontend dismiss would produce. We replicate the
            // command handler's body to avoid a Tauri State plumbing
            // jump.
            let event_tx = state.event_tx.lock().await.clone();
            let anchor_tx = state.anchor_event_tx.clone();
            if let Some((_, notif)) = state.notifications.remove(&notif_id) {
                state
                    .notification_by_pane
                    .remove(&(notif.host_id, notif.pane_id));
                if let Err(e) = state.db.delete_notification(notif_id) {
                    warn!("anchor RPC dismiss persist failed: {e}");
                }
                crate::commands::emit_event_anchored(
                    &event_tx,
                    &anchor_tx,
                    helm_domain::HostEvent::NotificationDismissed {
                        host_id: notif.host_id,
                        notification_id: notif_id,
                    },
                );
            }
            Ok(RpcResult::Ack)
        }
        RpcOp::ListSchedules => {
            let state = app.state::<AppState>();
            match state.db.list_schedules() {
                Ok(schedules) => Ok(RpcResult::Schedules { schedules }),
                Err(e) => Err(e),
            }
        }
        RpcOp::SaveSchedule { schedule } => {
            let state = app.state::<AppState>();
            let id = schedule.id;
            state.schedules.insert(id, schedule.clone());
            if let Err(e) = state.db.upsert_schedule(&schedule) {
                return Err(format!("persist: {e}"));
            }
            let event_tx = state.event_tx.lock().await.clone();
            let anchor_tx = state.anchor_event_tx.clone();
            crate::commands::emit_event_anchored(
                &event_tx,
                &anchor_tx,
                helm_domain::HostEvent::ScheduleUpserted {
                    schedule: schedule.clone(),
                },
            );
            crate::scheduler::signal(
                &state,
                crate::scheduler::SchedulerSignal::Upserted(id),
            );
            Ok(RpcResult::SavedSchedule { schedule_id: id })
        }
        RpcOp::DeleteSchedule { schedule_id } => {
            let state = app.state::<AppState>();
            state.schedules.remove(&schedule_id);
            state.schedule_runs.remove(&schedule_id);
            if let Err(e) = state.db.delete_schedule(schedule_id) {
                return Err(format!("delete: {e}"));
            }
            let event_tx = state.event_tx.lock().await.clone();
            let anchor_tx = state.anchor_event_tx.clone();
            crate::commands::emit_event_anchored(
                &event_tx,
                &anchor_tx,
                helm_domain::HostEvent::ScheduleRemoved { schedule_id },
            );
            crate::scheduler::signal(
                &state,
                crate::scheduler::SchedulerSignal::Removed(schedule_id),
            );
            Ok(RpcResult::Ack)
        }
        RpcOp::SetScheduleEnabled {
            schedule_id,
            enabled,
        } => {
            let state = app.state::<AppState>();
            let updated = state
                .schedules
                .get_mut(&schedule_id)
                .map(|mut e| {
                    e.value_mut().enabled = enabled;
                    e.value().clone()
                });
            let Some(updated) = updated else {
                return Err("unknown schedule".into());
            };
            if let Err(e) = state.db.upsert_schedule(&updated) {
                return Err(format!("persist: {e}"));
            }
            let event_tx = state.event_tx.lock().await.clone();
            let anchor_tx = state.anchor_event_tx.clone();
            crate::commands::emit_event_anchored(
                &event_tx,
                &anchor_tx,
                helm_domain::HostEvent::ScheduleUpserted { schedule: updated },
            );
            crate::scheduler::signal(
                &state,
                crate::scheduler::SchedulerSignal::Upserted(schedule_id),
            );
            Ok(RpcResult::Ack)
        }
        RpcOp::RunScheduleNow { schedule_id } => {
            let state = app.state::<AppState>();
            if !state.schedules.contains_key(&schedule_id) {
                return Err("unknown schedule".into());
            }
            crate::scheduler::signal(
                &state,
                crate::scheduler::SchedulerSignal::RunNow(schedule_id),
            );
            Ok(RpcResult::Ack)
        }
    };

    let server_msg = match reply {
        Ok(body) => RpcServerMessage::Ok { id, body },
        Err(message) => RpcServerMessage::Err { id, message },
    };
    write_message(writer, &server_msg).await
}

async fn write_message(
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    msg: &RpcServerMessage,
) -> Result<(), String> {
    let mut buf = serde_json::to_vec(msg).map_err(|e| format!("serialize: {e}"))?;
    buf.push(b'\n');
    let mut w = writer.lock().await;
    w.write_all(&buf).await.map_err(|e| format!("write: {e}"))?;
    w.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

/// Look up which host in the anchor's registry corresponds to the
/// subscriber's machine, by hostname stem match. Returns None when
/// the subscriber's hostname is empty (older subscriber that doesn't
/// send one) or no host entry matches. First match wins — duplicates
/// are rare in practice and hostname is a soft identifier anyway.
fn match_subscriber_hostname(
    state: &tauri::State<'_, AppState>,
    subscriber_hostname: &str,
) -> Option<helm_domain::HostId> {
    if subscriber_hostname.trim().is_empty() {
        return None;
    }
    let wanted = helm_domain::hostname_stem(subscriber_hostname);
    if wanted.is_empty() {
        return None;
    }
    for entry in state.hosts.iter() {
        if let Ok(guard) = entry.value().try_lock() {
            let stem = helm_domain::hostname_stem(&guard.host.hostname);
            if !stem.is_empty() && stem == wanted {
                return Some(guard.host.id);
            }
        }
    }
    None
}


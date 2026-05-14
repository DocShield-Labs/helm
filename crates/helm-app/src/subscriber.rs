//! Subscriber-side anchor RPC client.
//!
//! Owns one transport pair (read half + write half) speaking the
//! newline-delimited JSON protocol defined in `helm_domain`. Two
//! dedicated `std::thread`s drive I/O:
//!
//!   - **reader**: parses incoming `RpcServerMessage`s and demultiplexes
//!     them into either the pending-request `HashMap` (for Ok/Err
//!     replies) or the event broadcast channel (for pushed events).
//!   - **writer**: serializes outgoing `RpcClientMessage::Request`s and
//!     writes them as NDJSON.
//!
//! Sync threads + a sync `std::sync::mpsc` between async callers and
//! the writer matches helm-tmux's existing pattern for SSH channels
//! (PipeReader/PipeWriter are sync) and keeps the lifetime story
//! simple.
//!
//! Two transports are exposed today:
//!
//!   - [`open_local_subprocess`]: spawns `helm anchor-rpc` as a child
//!     process and uses its stdio. Lets us prove the client + protocol
//!     end-to-end on a single machine without involving SSH at all.
//!   - [`open_ssh`]: opens an exec channel on an existing
//!     [`helm_ssh::SshSession`], running `helm anchor-rpc` on the
//!     remote. This is what subscribers use against a real anchor
//!     across a Tailscale-style network.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc as sync_mpsc, Arc};

use helm_domain::{AnchorEvent, RpcClientMessage, RpcOp, RpcResult, RpcServerMessage};
use helm_ssh::{OpenedChannel, SshSession};
use parking_lot::Mutex;
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, warn};

/// Cheap-to-clone handle to a live subscriber client. Cloning a
/// `SubscriberClient` lets multiple parts of the app share the same
/// underlying connection; the I/O threads stop when the *last* clone
/// is dropped (which also drops the embedded transport guard).
#[derive(Clone)]
pub struct SubscriberClient {
    inner: Arc<ClientInner>,
}

#[allow(dead_code)] // event_tx, pending, next_id are read once into
                    // worker threads; the compiler doesn't see those
                    // moves as use-sites of the *struct* fields.
struct ClientInner {
    /// Send a request into the writer thread. Unbounded — caller's
    /// only failure mode is the writer being already gone (transport
    /// died), which surfaces as a closed channel.
    cmd_tx: sync_mpsc::Sender<ClientRequest>,
    /// Broadcast for AnchorEvents pushed by the server. Cloned into
    /// every consumer via `events()`. Holds a large buffer so a
    /// momentarily-slow consumer doesn't miss bursts of pane-bell
    /// events.
    event_tx: broadcast::Sender<AnchorEvent>,
    /// Pending responses keyed by request id. Inserted by the writer
    /// before it writes; removed by the reader when the matching
    /// Ok/Err arrives. Behind a `parking_lot::Mutex` — held only
    /// briefly per access.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<RpcResult, String>>>>>,
    /// Monotonic request id source. Wraps after ~1.8e19 requests, by
    /// which point any same-id collision is long gone.
    next_id: Arc<AtomicU64>,
    /// Lifetime guard for the underlying transport. Owns the
    /// channel/subprocess; dropping it closes the connection and
    /// causes the reader thread to EOF and exit, the writer thread
    /// to error and exit.
    _transport: Box<dyn TransportGuard>,
}

/// Marker trait for transport-lifetime guards. The concrete impl owns
/// whatever resource keeps the connection alive (an `OpenedChannel`
/// for SSH, a `tokio::process::Child` for subprocess). Dropping it
/// must close the connection so the I/O threads exit.
trait TransportGuard: Send + Sync {}

struct ClientRequest {
    op: RpcOp,
    response: oneshot::Sender<Result<RpcResult, String>>,
}

impl SubscriberClient {
    fn from_streams<R, W, G>(reader: R, writer: W, guard: G) -> Self
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        G: TransportGuard + 'static,
    {
        let (cmd_tx, cmd_rx) = sync_mpsc::channel::<ClientRequest>();
        let (event_tx, _) = broadcast::channel(4_096);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<RpcResult, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));

        // Reader thread — parses incoming, fans out to pending map / event broadcast.
        {
            let pending = pending.clone();
            let event_tx = event_tx.clone();
            std::thread::spawn(move || reader_loop(reader, pending, event_tx));
        }
        // Writer thread — serializes outgoing requests + records the
        // oneshot in the pending map so the reader can route the reply.
        {
            let pending = pending.clone();
            let next_id = next_id.clone();
            std::thread::spawn(move || writer_loop(writer, cmd_rx, pending, next_id));
        }

        SubscriberClient {
            inner: Arc::new(ClientInner {
                cmd_tx,
                event_tx,
                pending,
                next_id,
                _transport: Box::new(guard),
            }),
        }
    }

    /// Send a request and await the matching reply. Fails fast if the
    /// transport is dead (writer thread gone), and after the response
    /// channel is dropped (reader thread gone before a reply arrived).
    pub async fn request(&self, op: RpcOp) -> Result<RpcResult, String> {
        let (tx, rx) = oneshot::channel();
        let req = ClientRequest { op, response: tx };
        self.inner
            .cmd_tx
            .send(req)
            .map_err(|_| "subscriber: writer thread gone".to_string())?;
        rx.await
            .map_err(|_| "subscriber: response channel dropped before reply".to_string())?
    }

    /// New receiver for the AnchorEvent push stream. Each subscriber
    /// sees every event from the *moment it subscribed* onward; the
    /// caller is responsible for snapshotting via `request(Subscribe)`
    /// before subscribing if it needs a full picture.
    #[allow(dead_code)] // wired in by 1d (subscriber UI)
    pub fn events(&self) -> broadcast::Receiver<AnchorEvent> {
        self.inner.event_tx.subscribe()
    }
}

fn reader_loop<R: Read>(
    reader: R,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<RpcResult, String>>>>>,
    event_tx: broadcast::Sender<AnchorEvent>,
) {
    let mut br = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match br.read_line(&mut line) {
            Ok(0) => {
                debug!("subscriber: transport EOF");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("subscriber: read error: {e}");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: RpcServerMessage = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                warn!("subscriber: bad server message: {e} (line: {trimmed})");
                continue;
            }
        };
        match msg {
            RpcServerMessage::Ok { id, body } => {
                if let Some(tx) = pending.lock().remove(&id) {
                    let _ = tx.send(Ok(body));
                }
            }
            RpcServerMessage::Err { id, message } => {
                if let Some(tx) = pending.lock().remove(&id) {
                    let _ = tx.send(Err(message));
                }
            }
            RpcServerMessage::Event { event } => {
                let _ = event_tx.send(event);
            }
        }
    }
    // Transport dropped — fail every still-pending request so callers
    // unblock instead of awaiting a reply that will never arrive.
    let mut guard = pending.lock();
    for (_, tx) in guard.drain() {
        let _ = tx.send(Err("subscriber: connection closed".into()));
    }
}

fn writer_loop<W: Write>(
    mut writer: W,
    cmd_rx: sync_mpsc::Receiver<ClientRequest>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<RpcResult, String>>>>>,
    next_id: Arc<AtomicU64>,
) {
    while let Ok(req) = cmd_rx.recv() {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let msg = RpcClientMessage::Request { id, op: req.op };
        let mut bytes = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                let _ = req
                    .response
                    .send(Err(format!("subscriber: serialize: {e}")));
                continue;
            }
        };
        bytes.push(b'\n');
        // Insert BEFORE write so a fast reply can't arrive at the
        // reader before we've registered the matching slot.
        pending.lock().insert(id, req.response);
        if let Err(e) = writer.write_all(&bytes) {
            if let Some(tx) = pending.lock().remove(&id) {
                let _ = tx.send(Err(format!("subscriber: write: {e}")));
            }
            warn!("subscriber: writer exiting after error: {e}");
            return;
        }
        if let Err(e) = writer.flush() {
            warn!("subscriber: flush warning: {e}");
        }
    }
    debug!("subscriber: writer cmd channel closed");
}

// ---------- SSH transport ----------

/// Combined handle: the subscriber client + the bridge task forwarding
/// its events onto the local frontend channel. Stored on `AppState`
/// while a remote anchor is configured; `shutdown()` (or drop) aborts
/// the bridge and closes the client.
pub struct SubscriberHandle {
    pub client: SubscriberClient,
    /// Subscriber's local id for the anchor machine. Used to translate
    /// outgoing requests (subscriber's `anchor_host_id` → anchor's
    /// `HostId::local()`) and the snapshot's local-side flip in the
    /// other direction.
    pub anchor_host_id: helm_domain::HostId,
    /// Anchor's id for the subscriber's own machine (resolved via
    /// hostname stem match on Hello). None when the anchor doesn't
    /// have this machine in its host registry — translation for
    /// "this machine" is then skipped and local capture stays on.
    pub your_id_on_anchor: Option<helm_domain::HostId>,
    /// Bridge task forwarding `AnchorEvent`s → `HostEvent`s onto the
    /// frontend channel. Aborted on shutdown so we don't leak a task
    /// after the user switches anchors.
    bridge: tauri::async_runtime::JoinHandle<()>,
}

impl SubscriberHandle {
    pub fn shutdown(self) {
        self.bridge.abort();
        // Dropping `self.client` drops the inner Arc<ClientInner>;
        // when the last clone goes, the transport guard drops and
        // closes the connection, the reader/writer threads exit.
    }
}

/// Best-effort local hostname, lowercased. Empty string on failure;
/// the Hello protocol treats empty as "subscriber didn't supply one"
/// and skips matching.
fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|os| os.to_str().map(String::from))
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
}

/// Open a subscriber client to a remote anchor over an existing SSH
/// session. Runs `helm anchor-rpc` via the user's *login* shell so the
/// remote's PATH includes whatever they set up in their shell's login
/// rc file (~/.zprofile for zsh, ~/.bash_profile for bash, etc.).
///
/// `${SHELL:-/bin/bash}` rather than a hardcoded `bash` because macOS
/// users are overwhelmingly on zsh — `bash -lc` doesn't read
/// `~/.zprofile`, so a `~/.helm/bin` PATH addition the user thought
/// they'd made would be silently invisible to this exec channel.
/// Expanding $SHELL picks the right interpreter at runtime; the
/// fallback only kicks in on the unusual case where SSH doesn't
/// propagate SHELL (very few sshd setups).
pub fn open_ssh(session: Arc<SshSession>) -> Result<SubscriberClient, String> {
    let command = "${SHELL:-/bin/bash} -lc 'exec helm anchor-rpc'".to_string();
    let channel = session
        .open_exec(command)
        .map_err(|e| format!("subscriber: open_exec: {e}"))?;
    // PipeReader/PipeWriter both implement std::io::Read/Write. Move
    // them out so the guard can hold the OpenedChannel separately.
    // Actually we need them all in scope: split via destructuring.
    let OpenedChannel { reader, writer } = channel;
    // Reconstruct a guard that owns enough to keep the channel alive —
    // we re-pack the (now-empty) OpenedChannel via cloning the
    // backing pipes' Drop semantics. But OpenedChannel fields are
    // public and consumed individually; the I/O thread closes on
    // BOTH ends EOF, so dropping the reader/writer at end of streams
    // is what we rely on. The guard holds the Arc<SshSession> so the
    // session itself doesn't drop out from under us.
    let guard = SshSessionGuard {
        _session: session,
    };
    Ok(SubscriberClient::from_streams(reader, writer, guard))
}

struct SshSessionGuard {
    _session: Arc<SshSession>,
}
impl TransportGuard for SshSessionGuard {}

// ---------- Local subprocess transport ----------

/// Open a subscriber client backed by a local `helm anchor-rpc`
/// subprocess. Useful for verifying the client + protocol end-to-end
/// on a single machine without involving SSH. The subprocess connects
/// to the same local anchor socket the GUI is serving, so what you're
/// really testing is the subscriber client itself.
pub fn open_local_subprocess(helm_bin: &std::path::Path) -> Result<SubscriberClient, String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(helm_bin)
        .arg("anchor-rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("subscriber: spawn helm anchor-rpc: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "subscriber: no stdin handle".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "subscriber: no stdout handle".to_string())?;
    let guard = ChildGuard { _child: child };
    Ok(SubscriberClient::from_streams(stdout, stdin, guard))
}

struct ChildGuard {
    // The Child handle owns the subprocess; dropping it leaves the
    // process running (std behavior). We rely on stdin closing —
    // which happens when our writer thread exits and drops its
    // borrow — to signal the proxy to exit cleanly.
    _child: std::process::Child,
}
impl TransportGuard for ChildGuard {}

// ---------- Bridge: anchor events → local HostEvent channel ----------

/// Open an SSH-backed subscriber for `host_id` and spawn the bridge
/// task that pumps its events onto the local frontend channel.
/// Returns a handle the caller stores on `AppState`.
///
/// Pre-snapshots the anchor's notifications via `Subscribe` so the
/// frontend sees the anchor's existing inbox immediately — without
/// waiting for the next inbound event. Snapshot rows are forwarded as
/// individual `HostEvent::Notification` upserts, matching how the UI
/// already handles delta events.
///
/// `anchor_host_id` is the subscriber's own id for the anchor host
/// (the remote host entry the user designated). The anchor refers to
/// itself internally as `HostId::local()` — a constant that on the
/// subscriber means "this very machine's localhost." Without
/// remapping, every notification the anchor captures on its own
/// localhost would be misattributed to the subscriber's localhost
/// (wrong host, broken window/pane lookups, raw `@N` ids in the UI).
/// We rewrite those ids at the bridge boundary so the frontend sees
/// the anchor's localhost as the user's anchor host entry — which is
/// also the host the subscriber's own SSH session is attached to, so
/// the workspace tree resolves window names correctly.
pub async fn open_anchor_bridge(
    app: tauri::AppHandle,
    session: Arc<SshSession>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<helm_domain::HostEvent>>,
    anchor_host_id: helm_domain::HostId,
) -> Result<SubscriberHandle, String> {
    let client = open_ssh(session)?;
    // Hello first — needs to complete before Subscribe so the bridge
    // knows whether the anchor maps this machine. Surfaces transport
    // failures (helm not on remote PATH, broken pipe) early instead
    // of letting the bridge silently die later.
    let your_id_on_anchor = match client
        .request(helm_domain::RpcOp::Hello {
            hostname: local_hostname(),
        })
        .await?
    {
        helm_domain::RpcResult::Hello {
            your_id_on_anchor, ..
        } => your_id_on_anchor,
        other => return Err(format!("unexpected hello reply: {other:?}")),
    };
    if your_id_on_anchor.is_none() {
        tracing::info!(
            "subscriber: anchor doesn't have this machine in its host list — \
             local-capture suppression will be off, and notifications for this \
             machine will only appear if the user captures locally"
        );
    } else {
        tracing::info!(
            "subscriber: anchor maps this machine to {:?}",
            your_id_on_anchor
        );
    }
    let bridge = spawn_bridge(
        app,
        client.clone(),
        event_tx,
        anchor_host_id,
        your_id_on_anchor,
    );
    Ok(SubscriberHandle {
        client,
        anchor_host_id,
        your_id_on_anchor,
        bridge,
    })
}

/// Translate a host id from the anchor's id space to the subscriber's.
/// Applied to every host id arriving from the anchor (in events and
/// in synchronous list replies) so the subscriber's UI sees coherent
/// ids:
///   - anchor's `HostId::local()` (= the anchor machine itself) →
///     subscriber's `anchor_host_id` (the user's existing remote-host
///     entry for the anchor)
///   - anchor's `your_id_on_anchor` (= subscriber's machine, from
///     anchor's view) → subscriber's `HostId::local()` (the user's
///     own localhost)
///   - anything else → unchanged (these are "ghost" hosts the
///     subscriber sees via host sync)
pub fn translate_id_in(
    id: helm_domain::HostId,
    anchor_host_id: helm_domain::HostId,
    your_id_on_anchor: Option<helm_domain::HostId>,
) -> helm_domain::HostId {
    if id == helm_domain::HostId::local() {
        return anchor_host_id;
    }
    if let Some(your_id) = your_id_on_anchor {
        if id == your_id {
            return helm_domain::HostId::local();
        }
    }
    id
}

/// Reverse direction: subscriber's id space → anchor's. Used when
/// sending outgoing requests that carry a host_id (e.g. SaveSchedule)
/// so the anchor receives ids in its own space.
pub fn translate_id_out(
    id: helm_domain::HostId,
    anchor_host_id: helm_domain::HostId,
    your_id_on_anchor: Option<helm_domain::HostId>,
) -> helm_domain::HostId {
    if id == anchor_host_id {
        return helm_domain::HostId::local();
    }
    if id == helm_domain::HostId::local() {
        if let Some(your_id) = your_id_on_anchor {
            return your_id;
        }
        // No translation possible — pass through. Caller will hit
        // "unknown host" on the anchor side, which surfaces the
        // misconfiguration honestly rather than silently retargeting.
    }
    id
}

/// Apply incoming id translation to a single notification in place.
/// Used by `notifications_list` on the synchronous fetch path.
pub fn remap_notification_in(
    notification: &mut helm_domain::Notification,
    anchor_host_id: helm_domain::HostId,
    your_id_on_anchor: Option<helm_domain::HostId>,
) {
    notification.host_id =
        translate_id_in(notification.host_id, anchor_host_id, your_id_on_anchor);
}

/// Same for a schedule.
pub fn remap_schedule_in(
    schedule: &mut helm_domain::Schedule,
    anchor_host_id: helm_domain::HostId,
    your_id_on_anchor: Option<helm_domain::HostId>,
) {
    schedule.host_id = translate_id_in(schedule.host_id, anchor_host_id, your_id_on_anchor);
}

/// Cheap snapshot of (anchor_host_id, your_id_on_anchor) when a
/// subscriber is active. Returns None when this helm isn't in
/// subscriber mode. Used by command handlers to translate request
/// payloads before sending and reply payloads after receiving.
pub fn current_translation(
    subscriber: &parking_lot::Mutex<Option<SubscriberHandle>>,
) -> Option<(helm_domain::HostId, Option<helm_domain::HostId>)> {
    let guard = subscriber.lock();
    guard.as_ref().map(|h| (h.anchor_host_id, h.your_id_on_anchor))
}

/// Apply incoming id translation to every host_id reference in `evt`.
/// See `translate_id_in` for the mapping rules.
fn translate_event_in(
    mut evt: helm_domain::AnchorEvent,
    anchor_host_id: helm_domain::HostId,
    your_id_on_anchor: Option<helm_domain::HostId>,
) -> helm_domain::AnchorEvent {
    let xform = |id: &mut helm_domain::HostId| {
        *id = translate_id_in(*id, anchor_host_id, your_id_on_anchor);
    };
    match &mut evt {
        helm_domain::AnchorEvent::Notification {
            host_id,
            notification,
        } => {
            xform(host_id);
            xform(&mut notification.host_id);
        }
        helm_domain::AnchorEvent::NotificationDismissed { host_id, .. } => {
            xform(host_id);
        }
        helm_domain::AnchorEvent::ScheduleUpserted { schedule } => {
            xform(&mut schedule.host_id);
        }
        helm_domain::AnchorEvent::ScheduleRemoved { .. }
        | helm_domain::AnchorEvent::ScheduleFired { .. }
        | helm_domain::AnchorEvent::HostUpserted { .. }
        | helm_domain::AnchorEvent::HostRemoved { .. } => {}
    }
    evt
}

fn spawn_bridge(
    _app: tauri::AppHandle,
    client: SubscriberClient,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<helm_domain::HostEvent>>,
    anchor_host_id: helm_domain::HostId,
    your_id_on_anchor: Option<helm_domain::HostId>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut events = client.events();
        match client.request(helm_domain::RpcOp::Subscribe).await {
            Ok(helm_domain::RpcResult::Subscribed {
                mut notifications,
                mut schedules,
                hosts,
            }) => {
                if let Some(tx) = &event_tx {
                    let anchor_local = helm_domain::HostId::local();
                    // Hosts first: the inbox renders host names via
                    // store.hosts lookup, so the host list needs to
                    // be present before notifications referencing
                    // those ids arrive.
                    for host in hosts {
                        // Defensive — anchor's snapshot already
                        // filters out its own localhost, but skip on
                        // our side too. Also skip the anchor's entry
                        // for this very machine: the subscriber's own
                        // localhost is the canonical local
                        // representation, no need for a ghost copy.
                        if host.id == anchor_local
                            || Some(host.id) == your_id_on_anchor
                        {
                            continue;
                        }
                        let _ = tx.send(helm_domain::HostEvent::HostAdded { host });
                    }
                    for notification in &mut notifications {
                        remap_notification_in(
                            notification,
                            anchor_host_id,
                            your_id_on_anchor,
                        );
                    }
                    for notification in notifications {
                        let _ = tx.send(helm_domain::HostEvent::Notification {
                            host_id: notification.host_id,
                            notification,
                        });
                    }
                    for schedule in &mut schedules {
                        remap_schedule_in(schedule, anchor_host_id, your_id_on_anchor);
                    }
                    for schedule in schedules {
                        let _ = tx.send(helm_domain::HostEvent::ScheduleUpserted { schedule });
                    }
                }
            }
            Ok(other) => {
                warn!("subscriber: unexpected Subscribe reply: {other:?}");
            }
            Err(e) => {
                warn!("subscriber: Subscribe failed: {e}");
                return;
            }
        }
        loop {
            match events.recv().await {
                Ok(evt) => {
                    // Skip anchor host-list events for either the
                    // anchor's localhost (the subscriber's anchor
                    // entry is canonical) or the subscriber's own
                    // machine (already represented as local).
                    if let helm_domain::AnchorEvent::HostUpserted { host } = &evt {
                        if host.id == helm_domain::HostId::local()
                            || Some(host.id) == your_id_on_anchor
                        {
                            continue;
                        }
                    }
                    if let helm_domain::AnchorEvent::HostRemoved { host_id } = &evt {
                        if *host_id == helm_domain::HostId::local()
                            || Some(*host_id) == your_id_on_anchor
                        {
                            continue;
                        }
                    }
                    let evt = translate_event_in(evt, anchor_host_id, your_id_on_anchor);
                    let host_event = helm_domain::anchor_event_to_host_event(evt);
                    if let Some(tx) = &event_tx {
                        if tx.send(host_event).is_err() {
                            debug!("subscriber bridge: frontend channel closed, exiting");
                            return;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("subscriber bridge: lagged {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    debug!("subscriber bridge: anchor event stream closed, exiting");
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Minimal in-memory transport for the reader's protocol parsing.
    /// Doesn't exercise the writer — the writer needs a live remote to
    /// hand back replies, which we can't fake without a much heavier
    /// duplex pipe.
    #[test]
    fn reader_routes_ok_to_pending() {
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<RpcResult, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<RpcResult, String>>();
        pending.lock().insert(7, oneshot_tx);

        let body = serde_json::to_string(&RpcServerMessage::Ok {
            id: 7,
            body: RpcResult::Hello {
                version: "test".into(),
                your_id_on_anchor: None,
            },
        })
        .unwrap();
        let mut buf = body.into_bytes();
        buf.push(b'\n');
        let reader = Cursor::new(buf);
        let (event_tx, _) = broadcast::channel(8);
        reader_loop(reader, pending.clone(), event_tx);

        let received = oneshot_rx
            .blocking_recv()
            .expect("reader should have fired the oneshot");
        match received {
            Ok(RpcResult::Hello { version, .. }) => assert_eq!(version, "test"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn reader_broadcasts_events() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(8);

        let event_msg = serde_json::to_string(&RpcServerMessage::Event {
            event: AnchorEvent::NotificationDismissed {
                host_id: helm_domain::HostId::local(),
                notification_id: helm_domain::NotificationId::new(),
            },
        })
        .unwrap();
        let mut buf = event_msg.into_bytes();
        buf.push(b'\n');
        let reader = Cursor::new(buf);

        reader_loop(reader, pending, event_tx);

        // After reader_loop returns (EOF), the event should be sitting
        // in the broadcast buffer.
        let evt = event_rx.try_recv().expect("event should be queued");
        assert!(matches!(evt, AnchorEvent::NotificationDismissed { .. }));
    }
}

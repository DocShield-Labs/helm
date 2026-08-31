//! Global app state.
//!
//! The unit of attachment is the *host*. Localhost is just a `HostId`
//! with port 0 — same code path as a remote, the connect step branches on
//! whether to reach helmd over a local unix socket or an SSH exec
//! channel running `helmd stdio`.
//!
//! The frontend Zustand store owns the session tree projection. The
//! Rust side owns:
//!   - which hosts exist and their connection state
//!   - the live helmd session per connected host
//!   - the single event channel that delivers everything to the frontend

use dashmap::DashMap;
use helm_domain::{
    Host, HostEvent, HostId, HostKeyDecision, HostStatus, Notification, NotificationId,
};
use helm_proto::client::HelmdClient;
use helm_ssh::SshSession;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::AbortHandle;

/// One live helmd connection for a host: the typed client, its event
/// pump, the latest tree snapshot, and the request-correlation table
/// for ops that need a reply (`Created`, `SearchResults`).
pub struct SessionHandle {
    pub client: HelmdClient,
    /// Aborting stops the event pump task. The pump also exits on its
    /// own when the daemon connection drops (events channel closes).
    pub pump: AbortHandle,
    /// Latest `TreeSnapshot` from the daemon, updated by the pump on
    /// every `TreeChanged`. Commands read this to
    /// resolve names → ids without a round-trip. Arc so the pump can
    /// hold a clone without a cycle through the handle.
    pub tree: Arc<parking_lot::Mutex<helm_proto::TreeSnapshot>>,
    /// Pending request-reply slots keyed by `req_id`. The pump routes
    /// `Created` / `SearchResults` here instead of the event channel
    /// when a waiter is registered.
    pub pending: Arc<DashMap<u64, oneshot::Sender<helm_proto::DaemonMsg>>>,
    pub capabilities: Arc<parking_lot::RwLock<DaemonCapabilities>>,
    /// What the daemon reported in its HelloAck — kept for diagnostics
    /// (version skew between app and daemon is the classic
    /// "features silently missing" cause).
    pub daemon_version: String,
    next_req: AtomicU64,
}

impl SessionHandle {
    pub fn new(
        client: HelmdClient,
        pump: AbortHandle,
        tree: Arc<parking_lot::Mutex<helm_proto::TreeSnapshot>>,
        pending: Arc<DashMap<u64, oneshot::Sender<helm_proto::DaemonMsg>>>,
        capabilities: Arc<parking_lot::RwLock<DaemonCapabilities>>,
        daemon_version: String,
    ) -> Self {
        Self {
            client,
            pump,
            tree,
            pending,
            capabilities,
            daemon_version,
            next_req: AtomicU64::new(1),
        }
    }

    /// Send a request that expects a correlated reply and await it.
    /// `send` receives the allocated req_id and issues the message. The
    /// one place that owns id allocation, the pending table, the reply
    /// timeout, dropped-connection handling, and `Error` replies (which
    /// carry the req_id, so a failed op resolves immediately with its
    /// message instead of timing out).
    pub async fn request(
        &self,
        send: impl FnOnce(u64) -> Result<(), helm_proto::client::ClientError>,
    ) -> Result<helm_proto::DaemonMsg, String> {
        let id = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        if let Err(e) = send(id) {
            self.pending.remove(&id);
            return Err(e.to_string());
        }
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(helm_proto::DaemonMsg::Error {
                context, message, ..
            })) => Err(format!("{context}: {message}")),
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(_)) => Err("connection dropped while waiting for reply".into()),
            Err(_) => {
                self.pending.remove(&id);
                Err("daemon did not reply in time".into())
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct DaemonCapabilities {
    pub compatibility_baseline: Option<u32>,
    pub extensions: std::collections::HashSet<String>,
}

/// How long request/reply commands wait on the daemon.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// One host's runtime presence. Wrapped in an `Arc<Mutex<…>>` per entry so
/// connect/disconnect can serialize on a single host without blocking work
/// against any other host.
pub struct HostEntry {
    pub host: Host,
    pub status: HostStatus,
    /// The live helmd session. None when disconnected.
    pub session: Option<Arc<SessionHandle>>,
    /// SSH backing for remote hosts. `None` for localhost or when
    /// disconnected. Kept alive alongside `session` because dropping
    /// the session terminates the stdio channel.
    pub ssh: Option<Arc<SshSession>>,
    /// True when the user explicitly disconnects, saves, or deletes the
    /// host. The reconnect supervisor checks this on each transport
    /// drop to decide between retrying and exiting cleanly. Reset to
    /// false on every fresh `host_connect`.
    pub voluntary_disconnect: bool,
    /// Abort handle for the live supervisor task. Dropping or aborting
    /// this stops the reconnect loop — used by `host_disconnect`,
    /// `host_save`, and `host_delete` to guarantee no background
    /// reconnect attempts outlive a user action.
    pub supervisor: Option<AbortHandle>,
    /// Serializes connect attempts (initial connect via `host_connect`
    /// + supervisor reconnect) for this host. The outer entry mutex is
    /// released across the long async connect work, so two concurrent
    /// `do_connect` calls (React StrictMode double-effect, vite HMR
    /// re-firing the bootstrap effect, user clicks) would otherwise
    /// race and leak a live session + pump. Held across the entire
    /// connect path.
    pub connect_lock: Arc<Mutex<()>>,
}

impl HostEntry {
    pub fn new(host: Host) -> Self {
        Self {
            host,
            status: HostStatus::Disconnected,
            session: None,
            ssh: None,
            voluntary_disconnect: false,
            supervisor: None,
            connect_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Drop the helmd session (aborting its pump) + the underlying SSH
    /// session (if any). Used by every disconnect path (voluntary,
    /// host_save replace, host_delete, supervisor reconnect). The
    /// daemon itself keeps running with all processes intact — that's
    /// the whole point.
    pub fn shutdown_session(&mut self) {
        if let Some(session) = self.session.take() {
            session.pump.abort();
            // Dropping the last Arc drops HelmdClient → its writer
            // thread exits → transport closes → daemon sees EOF.
        }
        self.ssh = None;
    }
}

pub type SharedHostEntry = Arc<Mutex<HostEntry>>;

pub struct AppState {
    /// All known hosts. Localhost is always present; remote hosts are
    /// loaded/saved via `hosts.json`; retired daemon generations are
    /// discovered on connect and removed when they empty out. Arc so
    /// long-lived connection tasks can register/remove retired entries
    /// without holding `State`.
    pub hosts: Arc<DashMap<HostId, SharedHostEntry>>,

    /// Stable id for the always-present localhost entry.
    pub local_host_id: HostId,

    /// Single event channel to the frontend. Set when `host_subscribe`
    /// runs; commands send through it tagged with the originating host.
    pub event_tx: Mutex<Option<mpsc::UnboundedSender<HostEvent>>>,

    /// Pending host-key decisions, keyed by host id. Populated by the
    /// SSH prompter when `check_server_key` raises a UI prompt; drained
    /// by `host_key_prompt_response`.
    pub pending_host_key_prompts: Arc<DashMap<HostId, oneshot::Sender<HostKeyDecision>>>,

    /// Network reachability watch — `true` when the OS thinks any
    /// network path is up. The reconnect supervisor selects on this
    /// during its backoff sleep.
    pub network_online: watch::Receiver<bool>,

    /// System wake watch — a generation counter bumped each time the
    /// machine resumes from sleep. On wake the supervisor probes the
    /// SSH session and forces a reconnect if the probe fails.
    pub wake_signal: watch::Receiver<u64>,

    /// All live inbox notifications, keyed by id. Coalesce semantics
    /// (one per session, latest event wins) live in `crate::notifications`.
    pub notifications: Arc<DashMap<NotificationId, Notification>>,

    /// Coalesce index: the existing notification id (if any) for a
    /// given (host, session id string).
    pub notification_by_session: Arc<DashMap<(HostId, String), NotificationId>>,

    /// The (host, session) the user is actively looking at, surfaced
    /// from the frontend via `set_focus`. Notifications for the
    /// focused session are suppressed.
    pub focus: Arc<parking_lot::Mutex<Option<(HostId, String)>>>,

    /// Per-(host, integration_id) flag tracking whether we've already
    /// surfaced (or the user has dismissed) the suggestion toast for
    /// a tool integration this app session.
    pub tool_integration_seen: Arc<DashMap<(HostId, String), ()>>,
}

/// Cheap-to-clone bundle of handles for long-running tasks (the
/// connection pump, supervisor) so they can call into the
/// notifications layer without the full `AppState`.
#[derive(Clone)]
pub struct NotificationsCtx {
    pub notifications: Arc<DashMap<NotificationId, Notification>>,
    pub notification_by_session: Arc<DashMap<(HostId, String), NotificationId>>,
    pub focus: Arc<parking_lot::Mutex<Option<(HostId, String)>>>,
    pub tool_integration_seen: Arc<DashMap<(HostId, String), ()>>,
}

impl AppState {
    /// Cheap snapshot of the pump-side handles. Clones five `Arc`s.
    pub fn notifications_ctx(&self) -> NotificationsCtx {
        NotificationsCtx {
            notifications: self.notifications.clone(),
            notification_by_session: self.notification_by_session.clone(),
            focus: self.focus.clone(),
            tool_integration_seen: self.tool_integration_seen.clone(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        let local = Host::localhost();
        let local_id = local.id;
        let hosts = DashMap::new();
        hosts.insert(local_id, Arc::new(Mutex::new(HostEntry::new(local))));
        // Hydrate from `hosts.json`. Localhost isn't persisted, so this
        // only adds remote hosts saved by earlier sessions.
        for host in crate::persistence::try_load_hosts() {
            if host.port == 0 {
                continue;
            }
            hosts.insert(host.id, Arc::new(Mutex::new(HostEntry::new(host))));
        }
        Self {
            hosts: Arc::new(hosts),
            local_host_id: local_id,
            event_tx: Mutex::new(None),
            pending_host_key_prompts: Arc::new(DashMap::new()),
            network_online: crate::reachability::spawn(),
            wake_signal: crate::power::spawn(),
            notifications: Arc::new(DashMap::new()),
            notification_by_session: Arc::new(DashMap::new()),
            focus: Arc::new(parking_lot::Mutex::new(None)),
            tool_integration_seen: Arc::new(DashMap::new()),
        }
    }
}

impl AppState {
    /// Look up a host entry by id. Cheap clone of the Arc — caller locks
    /// the inner mutex when they need to mutate.
    pub fn entry(&self, id: HostId) -> Option<SharedHostEntry> {
        self.hosts.get(&id).map(|r| r.clone())
    }
}

//! Global app state.
//!
//! Phase 2 unit of attachment is the *host*. Localhost is just a `HostId`
//! with port 0 — same code path as a remote, the connect step branches on
//! whether to spawn a local PTY or open an SSH session.
//!
//! The frontend Zustand store still owns the workspace tree (windows /
//! panes) projected from tmux notifications. The Rust side owns:
//!   - which hosts exist and their connection state
//!   - the live `TmuxClient` per connected host
//!   - the single event channel that delivers everything to the frontend

use helm_domain::{Host, HostEvent, HostId, HostKeyDecision, HostStatus};
use helm_ssh::SshSession;
use helm_tmux::TmuxClient;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::AbortHandle;

/// One host's runtime presence. Wrapped in an `Arc<Mutex<…>>` per entry so
/// connect/disconnect can serialize on a single host without blocking work
/// against any other host.
pub struct HostEntry {
    pub host: Host,
    pub status: HostStatus,
    pub tmux: Option<Arc<TmuxClient>>,
    /// SSH backing for remote hosts. `None` for localhost or when
    /// disconnected. Kept alive alongside `tmux` because dropping the
    /// session terminates the underlying connection.
    pub ssh: Option<Arc<SshSession>>,
    /// True when the user explicitly disconnects, saves, or deletes the
    /// host. The reconnect supervisor checks this on each transport
    /// drop to decide between retrying and exiting cleanly. Reset to
    /// false on every fresh `host_connect`.
    pub voluntary_disconnect: bool,
    /// Abort handle for the live supervisor task (forwarder + reconnect
    /// ladder). Dropping or aborting this stops the reconnect loop —
    /// used by `host_disconnect`, `host_save`, and `host_delete` to
    /// guarantee no background reconnect attempts outlive a user
    /// action.
    pub supervisor: Option<AbortHandle>,
}

impl HostEntry {
    pub fn new(host: Host) -> Self {
        Self {
            host,
            status: HostStatus::Disconnected,
            tmux: None,
            ssh: None,
            voluntary_disconnect: false,
            supervisor: None,
        }
    }
}

pub type SharedHostEntry = Arc<Mutex<HostEntry>>;

pub struct AppState {
    /// All known hosts. Stage A seeds this with localhost only; Stage C
    /// loads/saves additional hosts from `hosts.json`.
    pub hosts: DashMap<HostId, SharedHostEntry>,

    /// Stable id for the always-present localhost entry. Frontend looks
    /// this up via `host_list()` rather than guessing — id is fresh per
    /// process so we can't bake it into the bindings.
    pub local_host_id: HostId,

    /// Single event channel to the frontend. Set when `host_subscribe`
    /// runs; commands send through it tagged with the originating host.
    pub event_tx: Mutex<Option<mpsc::UnboundedSender<HostEvent>>>,

    /// Pending host-key decisions, keyed by host id. Populated by the
    /// SSH prompter when `check_server_key` raises a UI prompt; drained
    /// by `host_key_prompt_response`. At most one prompt is in flight
    /// per host because the connect future is parked on the answer.
    ///
    /// Shared via `Arc` so the prompter task can hold a handle that
    /// outlives the per-command `State<'_>` borrow.
    pub pending_host_key_prompts: Arc<DashMap<HostId, oneshot::Sender<HostKeyDecision>>>,

    /// Network reachability watch — `true` when the OS thinks any
    /// network path is up. The reconnect supervisor selects on this
    /// during its backoff sleep: a `false → true` transition wakes
    /// the sleep early and resets the backoff index.
    pub network_online: watch::Receiver<bool>,
}

impl AppState {
    /// Cheap clone of the pending-prompts handle, for use by tasks that
    /// need to live past a `State<'_>` borrow.
    pub fn pending_host_key_prompts_handle(
        &self,
    ) -> Arc<DashMap<HostId, oneshot::Sender<HostKeyDecision>>> {
        self.pending_host_key_prompts.clone()
    }
}

impl Default for AppState {
    fn default() -> Self {
        let local = Host::localhost();
        let local_id = local.id;
        let hosts = DashMap::new();
        hosts.insert(local_id, Arc::new(Mutex::new(HostEntry::new(local))));
        // Hydrate from `hosts.json`. Localhost isn't persisted (its id
        // is process-local), so this only adds remote hosts saved by
        // earlier sessions.
        for host in crate::persistence::try_load_hosts() {
            // Skip any persisted localhost entry defensively — earlier
            // versions of the app may have written one.
            if host.port == 0 {
                continue;
            }
            hosts.insert(host.id, Arc::new(Mutex::new(HostEntry::new(host))));
        }
        Self {
            hosts,
            local_host_id: local_id,
            event_tx: Mutex::new(None),
            pending_host_key_prompts: Arc::new(DashMap::new()),
            network_online: crate::reachability::spawn(),
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

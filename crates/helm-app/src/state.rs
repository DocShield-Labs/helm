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

use helm_domain::{
    Host, HostEvent, HostId, HostKeyDecision, HostStatus, Notification, NotificationId, Schedule,
    ScheduleId, ScheduleRun,
};
use helm_ssh::SshSession;
use helm_tmux::TmuxClient;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::AbortHandle;

/// One host's runtime presence. Wrapped in an `Arc<Mutex<…>>` per entry so
/// connect/disconnect can serialize on a single host without blocking work
/// against any other host.
///
/// Multi-client model: each tmux *session* on the host has its own
/// permanently-attached control client (so `%output` flows for every
/// session in real time). `clients` maps tmux session id → SessionClient.
/// The `primary_session_id` picks one for routing global commands
/// (send-keys, kill-window, capture-pane — all of which use server-wide
/// pane/window ids and work through any client).
pub struct HostEntry {
    pub host: Host,
    pub status: HostStatus,
    pub clients: HashMap<String /* session_id */, Arc<SessionClient>>,
    /// First session we successfully attached at connect time. Used as
    /// the routing target for commands that don't care which session
    /// they go through. None when disconnected.
    pub primary_session_id: Option<String>,
    /// SSH backing for remote hosts. `None` for localhost or when
    /// disconnected. Kept alive alongside `clients` because dropping
    /// the session terminates every channel — i.e., every tmux client.
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
    /// Sender into the live supervisor's signal mpsc. Cloned by per-
    /// client forwarders (so they can report deaths / sessions-changed)
    /// and by `spawn_missing_clients` when wiring up newly-spawned
    /// forwarders. Replaced on every reconnect so signals from a stale
    /// forwarder can't leak into a fresh supervisor's stream.
    pub supervisor_tx: Option<mpsc::UnboundedSender<SupervisorSignal>>,
    /// Serializes connect attempts (initial connect via `host_connect`
    /// + supervisor reconnect) for this host. The outer entry mutex is
    /// released across the long async connect work, so two concurrent
    /// `do_connect` calls (React StrictMode double-effect, vite HMR
    /// re-firing the bootstrap effect, host_added re-fire, user clicks)
    /// would otherwise both run `shutdown_clients` on an empty map,
    /// race through `connect_host_multi`, and the second's
    /// `guard.clients = …` would silently drop the first's
    /// SessionClients without aborting their forwarders — leaving
    /// orphan `tmux -CC` PTYs streaming `%output` into the global
    /// event channel and producing duplicated input/output. Held
    /// across the entire connect path; the second caller waits, then
    /// runs its own `shutdown_clients` against whatever the first
    /// installed (last-call-wins, with proper teardown).
    pub connect_lock: Arc<Mutex<()>>,
}

/// One control client (one `tmux -CC attach -t $session_id`) for a
/// single tmux session. Owns the TmuxClient + a per-client forwarder
/// task that drains its notifications onto the host's event channel.
/// The session id lives on the `clients` HashMap key, not duplicated
/// here.
pub struct SessionClient {
    pub tmux: Arc<TmuxClient>,
    /// Aborted by the host supervisor on disconnect or full reconnect.
    /// The forwarder also self-aborts when its notification channel
    /// closes (transport drop / `%exit` for this session).
    pub forwarder: AbortHandle,
}

/// Signal from a per-client forwarder back to the host supervisor.
/// Lives in state.rs so `HostEntry` can hold an mpsc sender of these
/// without commands.rs having to publicly expose the type.
pub enum SupervisorSignal {
    /// One control client's transport closed (`%exit`, EOF, channel
    /// drop). The forwarder has already exited; the supervisor decides
    /// whether to keep the host alive (other clients still attached),
    /// promote a new primary, or fall through to the reconnect ladder.
    ClientDied(String /* session_id */),
    /// `%sessions-changed` arrived — supervisor re-lists sessions on
    /// the server and spawns control clients for any that are now
    /// present without one. Coalesced if multiple clients fire it
    /// in quick succession.
    SessionsChanged,
    /// A `%session-changed` notification carried a session id different
    /// from the one this client was originally attached to. Tmux
    /// migrated us — typically because our session was destroyed while
    /// `detach-on-destroy` was set to `off|previous|next|no-detached`,
    /// so instead of exiting cleanly the client got reattached to a
    /// surviving session.
    ///
    /// The supervisor re-keys this client's entry from `from` to `to`,
    /// or — if `to` already has its own client — aborts this redundant
    /// one (otherwise both forwarders would pump the same `%output`
    /// for `to` into the global event channel, producing N× echoed
    /// input/output).
    ClientMigrated {
        from: String, /* session_id we were keyed under */
        to: String,   /* session_id we're now attached to */
    },
}

impl HostEntry {
    pub fn new(host: Host) -> Self {
        Self {
            host,
            status: HostStatus::Disconnected,
            clients: HashMap::new(),
            primary_session_id: None,
            ssh: None,
            voluntary_disconnect: false,
            supervisor: None,
            supervisor_tx: None,
            connect_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Cheap accessor for the primary client's TmuxClient. Returns None
    /// when disconnected or when the primary session was killed and we
    /// haven't yet picked a new one.
    pub fn primary_client(&self) -> Option<Arc<TmuxClient>> {
        let id = self.primary_session_id.as_ref()?;
        self.clients.get(id).map(|c| c.tmux.clone())
    }

    /// Drop every per-session tmux control client + the underlying SSH
    /// session (if any). Aborts each client's forwarder so its task
    /// stops draining notifications. Used by every disconnect path
    /// (voluntary, host_save replace, host_delete, supervisor reconnect).
    pub fn shutdown_clients(&mut self) {
        for (_, c) in self.clients.drain() {
            c.forwarder.abort();
            // Each TmuxClient::Drop kills its `-CC` process / closes
            // its SSH channel naturally as the Arc count hits zero.
            // We don't need to do anything explicit here.
            drop(c);
        }
        self.primary_session_id = None;
        self.ssh = None;
        // Drop the supervisor sender so any straggler forwarder that
        // still holds a clone can't leak signals into a future
        // supervisor instance.
        self.supervisor_tx = None;
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

    /// All live inbox notifications, keyed by id. Coalesce semantics
    /// (one per pane, latest event wins, repeated same-kind bumps a
    /// counter) live in `crate::notifications` — this map is just the
    /// flat registry the `notifications_list` command and dismiss
    /// handlers walk.
    pub notifications: Arc<DashMap<NotificationId, Notification>>,

    /// Coalesce index: the existing notification id (if any) for a
    /// given (host, pane). Lets the marker post-processor look up
    /// "is there already a row for this pane?" in O(1) instead of
    /// scanning `notifications`.
    pub notification_by_pane: Arc<DashMap<(HostId, String), NotificationId>>,

    /// Per-pane runtime — output preview ring, in-flight command timing,
    /// last-known window mapping. Populated by the marker post-processor;
    /// cleared when the host disconnects or the pane disappears.
    pub pane_runtime: Arc<DashMap<(HostId, String), PaneRuntime>>,

    /// The (host, window) the user is actively looking at in the helm
    /// UI, surfaced from the frontend via `set_focus`. The notifications
    /// post-processor checks this before creating a new inbox row —
    /// when a pane's window matches the focus, we suppress the
    /// notification entirely (the user is already watching the output;
    /// an inbox row would be noise). Cleared (set to None) when the
    /// helm window loses OS focus or is minimized.
    pub focus: Arc<parking_lot::Mutex<Option<(HostId, String)>>>,

    /// Per-host serialization lock for `refresh_pane_index`. See
    /// `NotificationsCtx::refresh_locks` for rationale.
    pub refresh_locks: Arc<DashMap<HostId, Arc<tokio::sync::Mutex<()>>>>,

    /// Per-(host, integration_id) flag tracking whether we've already
    /// surfaced (or the user has dismissed) the suggestion toast for
    /// a tool integration this app session. Prevents the toast from
    /// re-firing every time `refresh_pane_index` runs and re-detects
    /// the same `claude` process. Cleared on app restart — we re-suggest
    /// each launch since the user may have changed their mind.
    pub tool_integration_seen: Arc<DashMap<(HostId, String), ()>>,

    /// User-defined scheduled runs, keyed by id. Persisted to
    /// `schedules.json`. Mutated under the per-id lock the scheduler
    /// holds while reading/firing — the simple choice is a Mutex<Vec>,
    /// but a DashMap mirrors the hosts registry shape and lets reads
    /// through `schedule_list` not block fires.
    pub schedules: Arc<DashMap<ScheduleId, Schedule>>,

    /// In-memory ring of recent runs per schedule. Most recent first;
    /// capped at `SCHEDULE_RUN_HISTORY_LIMIT`. Not persisted — survives
    /// only as long as the app instance.
    pub schedule_runs: Arc<DashMap<ScheduleId, Vec<ScheduleRun>>>,

    /// Sender into the scheduler supervisor's signal channel. Set when
    /// the supervisor is spawned at boot. Used by the schedule_*
    /// commands to wake the supervisor whenever a schedule is
    /// added / changed / removed so it can rebuild its next-fire map.
    /// `parking_lot` rather than tokio so the supervisor's own boot
    /// path (sync setup callback) can stash the sender without needing
    /// an async context, while async commands can also lock cheaply.
    pub scheduler_tx:
        parking_lot::Mutex<Option<mpsc::UnboundedSender<crate::scheduler::SchedulerSignal>>>,
}

/// Cap on the in-memory run history per schedule. ~50 keeps the palette
/// "Recent runs" view useful without unbounded memory growth on a
/// minutely cron that runs for weeks.
pub const SCHEDULE_RUN_HISTORY_LIMIT: usize = 50;

/// Mutable per-pane state the notifications layer accumulates between
/// marker events. Cheap to clone; held briefly under the `pane_runtime`
/// DashMap entry guard during a marker update.
#[derive(Debug, Clone, Default)]
pub struct PaneRuntime {
    /// Most recent ANSI-stripped pane output, capped at `PREVIEW_BYTES`.
    /// Snapshotted (last ~120 chars) into `Notification.preview` whenever
    /// we create or coalesce a notification.
    pub output_ring: Vec<u8>,
    /// Unix-ms timestamp of the most recent `OutputMarker::CommandStart`
    /// (`OSC 133;B`). Lets us compute `duration_ms` on the matching
    /// `CommandDone`. None when we haven't observed a CommandStart since
    /// the last CommandDone — happens for the very first prompt and for
    /// shells without integration installed.
    pub command_started_at: Option<u64>,
    /// Command line captured from the most recent CommandStart marker
    /// (`OSC 133;B;cmdline_b64=…`). Cleared on CommandDone. Empty when
    /// the integration script didn't ship the cmdline param (older
    /// installs or `HELM_KEEP_PROMPT=1`).
    pub command_text: String,
    /// Best-known window id for this pane, refreshed by the periodic
    /// pane-index sweep (see `notifications::refresh_pane_index`). Empty
    /// while we're still bootstrapping or if the lookup hasn't run yet —
    /// the frontend can also resolve this from its own tree.
    pub window_id: String,
    /// Best-known session (workspace) id for this pane, same caveats.
    pub session_id: String,
}

/// Maximum bytes retained in the per-pane output ring. ~512 bytes is
/// enough for ~5-10 lines of typical command output, which is plenty
/// for a single-line preview after we strip ANSI and tail to ~120 chars.
pub const PREVIEW_BYTES: usize = 512;

/// Cheap-to-clone bundle of supervisor-side handles. Passed to long-
/// running tasks (forwarders, supervise, refresh helpers) so they can
/// call into the notifications layer + tool-integration detection
/// without needing the full `AppState` (which is wrapped in Tauri's
/// `State<'_>` and inconvenient to thread through long-running futures).
#[derive(Clone)]
pub struct NotificationsCtx {
    pub notifications: Arc<DashMap<NotificationId, Notification>>,
    pub notification_by_pane: Arc<DashMap<(HostId, String), NotificationId>>,
    pub pane_runtime: Arc<DashMap<(HostId, String), PaneRuntime>>,
    pub focus: Arc<parking_lot::Mutex<Option<(HostId, String)>>>,
    /// Per-host mutex serializing `refresh_pane_index` so concurrent
    /// triggers (every forwarder sees `%window-added` /
    /// `%sessions-changed` and wants to refresh) don't race their
    /// stale-cleanup steps and wipe valid entries.
    pub refresh_locks: Arc<DashMap<HostId, Arc<tokio::sync::Mutex<()>>>>,
    /// Per-(host, integration_id) flag tracking whether we've already
    /// surfaced (or processed) the suggestion toast for a tool
    /// integration this session. See AppState's same-named field.
    pub tool_integration_seen: Arc<DashMap<(HostId, String), ()>>,
}

impl AppState {
    /// Cheap snapshot of the supervisor-side handles. Clones six `Arc`s.
    pub fn notifications_ctx(&self) -> NotificationsCtx {
        NotificationsCtx {
            notifications: self.notifications.clone(),
            notification_by_pane: self.notification_by_pane.clone(),
            pane_runtime: self.pane_runtime.clone(),
            focus: self.focus.clone(),
            refresh_locks: self.refresh_locks.clone(),
            tool_integration_seen: self.tool_integration_seen.clone(),
        }
    }
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
        // Hydrate schedules. Same fail-soft semantics as hosts: log on
        // parse error, start with an empty map.
        let schedules = DashMap::new();
        for s in crate::schedules::try_load_schedules() {
            schedules.insert(s.id, s);
        }
        Self {
            hosts,
            local_host_id: local_id,
            event_tx: Mutex::new(None),
            pending_host_key_prompts: Arc::new(DashMap::new()),
            network_online: crate::reachability::spawn(),
            notifications: Arc::new(DashMap::new()),
            notification_by_pane: Arc::new(DashMap::new()),
            pane_runtime: Arc::new(DashMap::new()),
            focus: Arc::new(parking_lot::Mutex::new(None)),
            refresh_locks: Arc::new(DashMap::new()),
            tool_integration_seen: Arc::new(DashMap::new()),
            schedules: Arc::new(schedules),
            schedule_runs: Arc::new(DashMap::new()),
            scheduler_tx: parking_lot::Mutex::new(None),
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

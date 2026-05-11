//! Shared domain types for Helm.
//!
//! Anything that crosses a crate boundary or the Rust↔TS boundary lives here.
//! No business logic — just the vocabulary every other crate agrees on.

use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

// ---------- Identifiers ----------

macro_rules! newtype_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

newtype_id!(HostId);
newtype_id!(WorkspaceId);
newtype_id!(WindowId);
newtype_id!(PaneId);
newtype_id!(NotificationId);
newtype_id!(ScheduleId);
newtype_id!(ScheduleRunId);

impl HostId {
    /// The localhost host id is stable across app launches so any
    /// frontend state keyed on it (pinned windows, last-active host,
    /// activity dots, …) survives a restart. The previous behavior —
    /// minting a fresh Uuid::new_v4() every boot — left those features
    /// silently broken since the on-disk pin's hostId no longer
    /// matched the in-memory localhost entry.
    ///
    /// Hardcoded constant rather than derived-from-machine because
    /// localhost is a per-app-instance concept; the stability we need
    /// is "same id between launches of THIS app on THIS machine,"
    /// which a constant trivially provides.
    pub fn local() -> Self {
        // Constant rather than `uuid!(...)` so we don't need to opt
        // the workspace into the `macros` feature for this one call.
        Self(Uuid::from_u128(1))
    }
}

// ---------- tmux notifications (cross the IPC boundary) ----------

/// In-band markers extracted from a pane's `%output` byte stream before
/// the bytes are forwarded to xterm. Used to drive notifications
/// (bell, command-completion) and — once the blocks UI lands — to record
/// prompt/output spans against the xterm buffer.
///
/// Bell is a single 0x07 byte; the rest are OSC 133 sequences emitted by
/// shells with helm's integration script sourced. We strip both from the
/// forwarded bytes so xterm doesn't actually beep or render the escape
/// sequences as glyphs.
///
/// `PromptStart` and `CommandStart` carry optional metadata (cwd, branch,
/// command line) shipped by helm's integration scripts as base64 params on
/// the OSC 133 envelope. None when the shell's integration is older than
/// 4F or running under `HELM_KEEP_PROMPT=1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputMarker {
    /// BEL (0x07) seen in pane output. Most CLI tools (Claude Code,
    /// finished long-running commands, IRC pings) emit this when they
    /// want the user's attention.
    Bell,
    /// `OSC 133;A[;cwd_b64=…;branch_b64=…]` — the shell is about to
    /// print a new prompt.
    PromptStart {
        /// Working directory at prompt time, decoded from `cwd_b64`.
        cwd: Option<String>,
        /// Current git branch (if cwd is inside a repo), decoded from
        /// `branch_b64`. None when not in a repo or the integration
        /// script couldn't run `git`.
        branch: Option<String>,
    },
    /// `OSC 133;B[;cmdline_b64=…]` — the prompt has finished printing;
    /// what follows is the user's typed command.
    CommandStart {
        /// The command line the user is about to run, decoded from
        /// `cmdline_b64`. None when emitted from an older integration
        /// script.
        command: Option<String>,
    },
    /// `OSC 133;C` — the user pressed Enter; what follows is command
    /// output.
    OutputStart,
    /// `OSC 133;D[;<exit_code>]` — the previous command finished with the
    /// given exit code. None when the shell didn't include a code (older
    /// integration scripts, partial sequences).
    CommandDone { exit_code: Option<i32> },
}

/// One marker, paired with the byte offset into the cleaned `%output`
/// chunk where it was extracted.
///
/// Without offsets, multiple markers in a single chunk (e.g. `D` from
/// the previous command + `A` for the new prompt + bytes between)
/// can't be correlated to xterm rows on the frontend — the cursor
/// would be sampled at the *end* of the whole chunk for every marker
/// and block boundaries would land on the wrong line. The frontend
/// uses these offsets to slice the byte stream and sample the cursor
/// in xterm at each marker's position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct MarkerAt {
    pub marker: OutputMarker,
    /// Byte offset into the *cleaned* `bytes` field of
    /// `TmuxNotification::Output`. The marker itself was stripped from
    /// that buffer; this offset is the position where the marker would
    /// have started.
    pub offset: u32,
}

/// Wire format for tmux state deltas. Mirrors `helm-tmux::parse::Notification`
/// — kept here so the type lives next to the rest of the IPC vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TmuxNotification {
    /// `bytes` is the cleaned output stream — bells and OSC 133 markers
    /// have been stripped (and surfaced separately in `markers`) so xterm
    /// doesn't beep on every notification or render the escape sequences.
    Output {
        pane_id: String,
        bytes: Vec<u8>,
        markers: Vec<MarkerAt>,
    },
    WindowAdded { window_id: String },
    WindowClosed { window_id: String },
    WindowRenamed { window_id: String, name: String },
    SessionChanged { session_id: String, name: String },
    SessionRenamed { session_id: String, name: String },
    SessionsChanged,
    SessionWindowChanged { session_id: String, window_id: String },
    LayoutChanged { window_id: String, layout: String },
    WindowPaneChanged { window_id: String, pane_id: String },
    PaneModeChanged { pane_id: String },
    Continue { pane_id: String },
    Pause { pane_id: String },
    ClientDetached { client: String },
    Exit { reason: Option<String> },
    Unknown { name: String, args: String },
}

// ---------- Host ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Connected,
    Connecting,
    /// Transport dropped or tmux server bounced; the supervisor is
    /// running its backoff ladder. Distinct from `Connecting` so the UI
    /// can render a different overlay (the user's panes stay mounted
    /// with their last frozen frame instead of being torn down).
    Reconnecting,
    Disconnected,
    Idle,
    Error,
}

/// Single event channel from Rust to the frontend. Tmux notifications,
/// host-status transitions, and registry mutations interleave on the same
/// stream so the frontend sees them in order with everything else.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEvent {
    Tmux {
        host_id: HostId,
        notification: TmuxNotification,
    },
    Status {
        host_id: HostId,
        status: HostStatus,
        error: Option<String>,
    },
    /// A new host was registered (via `host_add` or persistence load).
    /// Frontend store should insert it into the hosts Map.
    HostAdded {
        host: Host,
    },
    /// A host was removed from the registry. Frontend store drops it.
    HostRemoved {
        host_id: HostId,
    },
    /// Mid-connect prompt: the SSH server presented a host key that's
    /// either unknown to `~/.ssh/known_hosts` or has changed since the
    /// last connection. The connect future is parked until the frontend
    /// answers via the `host_key_prompt_response` command.
    HostKeyPrompt {
        host_id: HostId,
        hostname: String,
        port: u16,
        algorithm: String,
        /// SHA-256 fingerprint, OpenSSH-style
        /// (`SHA256:base64(no-padding)`).
        fingerprint: String,
        prompt: HostKeyPromptKind,
    },
    /// A pane wants the user's attention. Sent when a new notification is
    /// created AND when an existing one coalesces (count/updated_at bump,
    /// possibly upgraded kind — e.g., a Bell entry replaced by a newer
    /// CommandDone for the same window). Frontend treats receipt as
    /// upsert keyed by `notification.id`.
    Notification {
        host_id: HostId,
        notification: Notification,
    },
    /// A previously-emitted notification was dismissed — by the user
    /// (× button), by typing into the pane (auto-dismiss-on-keystroke),
    /// or by the host (window killed, host disconnected). Frontend
    /// drops it from the inbox.
    NotificationDismissed {
        host_id: HostId,
        notification_id: NotificationId,
    },
    /// Helm detected a tool running in a pane that has a known
    /// integration available (e.g. Claude Code). Frontend surfaces a
    /// sticky toast offering to install the integration. Coalesced
    /// per (host, integration_id) for the lifetime of the app — once
    /// the user installs or dismisses, no more suggestions for that
    /// integration on that host.
    ToolIntegrationSuggested {
        host_id: HostId,
        integration_id: String,
        name: String,
        description: String,
        post_install_note: String,
    },
    /// A schedule was created / updated / re-enabled. Frontend upserts
    /// into its schedules Map. Also covers the case where the
    /// supervisor advanced `last_fired_at` / `last_run_status` after a
    /// fire — same shape, so the frontend's projection just refreshes.
    ScheduleUpserted {
        schedule: Schedule,
    },
    /// A schedule was deleted from the registry. Frontend drops it.
    ScheduleRemoved {
        schedule_id: ScheduleId,
    },
    /// A scheduled run successfully opened a window. The frontend
    /// doesn't toast — attention-worthy follow-up reaches the user
    /// through the existing notification pipeline (Bell / CommandDone)
    /// — but the projection updates so palette rows show "ran 2m ago."
    ///
    /// `manual` is true when the fire was triggered by the user's
    /// explicit `schedule_run_now` rather than the supervisor's
    /// schedule-time loop. The frontend uses this to auto-jump to the
    /// new window for manual fires (the user expects to see what they
    /// just kicked off) without disrupting the user's current focus on
    /// every cron-time fire.
    ScheduleFired {
        schedule_id: ScheduleId,
        run_id: ScheduleRunId,
        /// Unix ms when the run started.
        started_at: u64,
        /// tmux window id the run landed in.
        window_id: String,
        manual: bool,
    },
}

// ---------- notifications ----------

/// One row in the user's inbox. Coalesced per (host, window, kind-class):
/// repeated bells in the same window bump `count` and `updated_at` rather
/// than stacking, and a fresh CommandDone replaces an older Bell for the
/// same window (commands finishing is more informative than a raw bell).
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Notification {
    pub id: NotificationId,
    pub host_id: HostId,
    /// tmux session id (`$N`). Optional because the bell may arrive
    /// before our workspace tree is hydrated; the frontend can fill in
    /// the breadcrumb from the window id alone.
    pub workspace_id: Option<String>,
    /// tmux window id (`@N`).
    pub window_id: String,
    /// tmux pane id (`%N`) — the pane the marker came from. A window
    /// can hold multiple panes; we surface the originating pane so the
    /// inbox row can route the user to the exact one.
    pub pane_id: String,
    pub kind: NotificationKind,
    /// Unix ms when this notification was first created.
    pub created_at: u64,
    /// Unix ms of the most recent coalesced event. Equal to created_at
    /// for fresh notifications; advances on every coalesce.
    pub updated_at: u64,
    /// How many times this notification has coalesced (1 for fresh).
    pub count: u32,
    /// Short human-readable preview — up to ~120 chars of the most recent
    /// pane output, ANSI-stripped. Drives the secondary line in the inbox
    /// row so the user can decide "still spinning" vs "really done"
    /// without switching to the pane.
    pub preview: String,
}

/// What this notification represents. Drives the inbox dot color and
/// rollup classification in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationKind {
    /// BEL emitted by something running in the pane. The single most
    /// reliable "pay attention" signal — Claude Code, finished builds,
    /// IRC pings, etc.
    Bell,
    /// A command finished. `exit_code` is None when the shell's
    /// integration script doesn't include one (older versions, partial
    /// sequences); the frontend treats None as "succeeded probably."
    CommandDone {
        exit_code: Option<i32>,
        /// The command that ran (captured between `OSC 133;B` and
        /// `OSC 133;C` markers). Empty if we never saw a command-start
        /// marker for this run (e.g., shell entered a TUI, integration
        /// dropped a marker).
        command: String,
        /// Wall-clock duration in milliseconds, B → D. None when we
        /// didn't observe the start marker.
        duration_ms: Option<u64>,
    },
    /// A scheduled run failed before it could even reach the user's
    /// shell — host wouldn't connect, working directory missing,
    /// new-window failed. Coalesced per schedule id (one inbox row per
    /// schedule, latest reason wins). Successful fires produce no
    /// notification of their own; whatever the spawned command does
    /// (Claude waiting, build failing, …) reaches the user through the
    /// normal Bell / CommandDone pipeline.
    ScheduleFailed {
        schedule_id: ScheduleId,
        /// User-facing schedule name at fire time, snapshotted so the
        /// inbox row remains readable if the schedule is later renamed
        /// or deleted.
        schedule_name: String,
        /// Short human-readable reason ("host not connected", "cwd does
        /// not exist", etc.). Mirrors what we'd surface in a toast.
        reason: String,
    },
}

/// Why we're surfacing a host-key prompt to the user.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostKeyPromptKind {
    /// First time seeing this host (hostname:port not in known_hosts).
    Unknown,
    /// Host key differs from what's recorded in known_hosts. Possible
    /// MITM. `previous_line` is the line number in `~/.ssh/known_hosts`
    /// that holds the conflicting entry, surfaced so the user can
    /// inspect and edit by hand.
    Changed { previous_line: u32 },
}

/// User's response to a host-key prompt. Crosses the IPC boundary as the
/// payload of the `host_key_prompt_response` command.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyDecision {
    /// Refuse the connection. The connect future returns an auth error.
    Reject,
    /// Accept for this connection only. `~/.ssh/known_hosts` is unchanged.
    AcceptOnce,
    /// Accept and append to `~/.ssh/known_hosts` so we don't prompt again.
    /// Only valid for `Unknown` prompts — `Changed` always requires the
    /// user to manually edit the file (matches OpenSSH behavior).
    TrustPermanently,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub enum AuthMethod {
    Agent,
    KeyFile { path: String },
    Password, // actual secret is in Keychain, never crosses the boundary
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Host {
    pub id: HostId,
    pub name: String,
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    pub jump_host: Option<HostId>,
    pub tmux_integration: bool,
    pub default_workspace: String,
    pub startup_commands: Vec<String>,
    /// The canonical "always-on" peer for this user's helm network. At
    /// most one host has this flag at a time — enforced atomically by
    /// `host_set_anchor`. When set on localhost, this machine is the
    /// anchor and runs the scheduler + notification capture as the
    /// source of truth. When set on a remote host, this helm is a
    /// subscriber that reads notifications/schedules from there.
    ///
    /// `#[serde(default)]` so existing rows persisted before this field
    /// existed deserialize to `false` instead of failing.
    #[serde(default)]
    pub is_anchor: bool,
}

impl Host {
    /// Convenience constructor for the always-present localhost entry.
    pub fn localhost() -> Self {
        Self {
            id: HostId::local(),
            name: "localhost".into(),
            hostname: "localhost".into(),
            port: 0,
            user: whoami_or_unknown(),
            auth: AuthMethod::Agent,
            jump_host: None,
            tmux_integration: true,
            default_workspace: "default".into(),
            startup_commands: vec![],
            is_anchor: false,
        }
    }
}

fn whoami_or_unknown() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".into())
}

// ---------- Activity ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum Activity {
    Running,
    Attention,
    Failed,
    Idle,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct PaneActivity {
    pub last_output_at: Option<u64>, // unix ms
    pub current_command: String,
    pub is_idle: bool,
    pub bell_count: u32,
    pub last_exit_code: Option<i32>,
    pub started_at: u64,
}

// ---------- Tree ----------

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub host_id: HostId,
    pub name: String,
    pub windows: Vec<WindowId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Window {
    pub id: WindowId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub panes: Vec<PaneId>,
    pub focused_pane: Option<PaneId>,
    pub activity: Activity,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Pane {
    pub id: PaneId,
    pub window_id: WindowId,
    pub cwd: String,
    pub command: String,
    pub activity: PaneActivity,
}

// ---------- Schedules ----------

/// A user-defined scheduled run. Local-only in v1: persisted in
/// `schedules.json` next to `hosts.json`, fired by an in-process
/// supervisor on whichever helm instance saved them. Each fire opens a
/// new tmux window on `host_id`, cd's to `cwd`, and runs the body.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Schedule {
    pub id: ScheduleId,
    /// Human label shown in the palette and used as the new window's
    /// tmux name when fired.
    pub name: String,
    pub host_id: HostId,
    /// Absolute path on the target host. Validated locally at save time
    /// for localhost; remote cwd is trusted at save time and only fails
    /// at fire time (visible as a `ScheduleFailed` notification).
    pub cwd: String,
    pub body: ScheduleBody,
    pub trigger: Trigger,
    /// Workspace (tmux session) to land the new window in. `Named`
    /// creates if missing; the sentinel "scheduled" is the default.
    pub workspace_target: WorkspaceTarget,
    /// When false, the supervisor skips this schedule. The user can
    /// still fire it manually via `schedule_run_now`.
    pub enabled: bool,
    /// Unix ms of the most recent fire. None until first run.
    pub last_fired_at: Option<u64>,
    /// Status of the most recent run. None until first run.
    pub last_run_status: Option<ScheduleRunStatus>,
}

/// What to run when a schedule fires.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleBody {
    /// A literal shell line — sent verbatim followed by `\r`. Includes
    /// any redirections, pipes, or chained commands the user typed.
    Shell { command: String },
    /// First-class Claude Code launch. Materialized at fire time as
    /// either `claude` (interactive, empty prompt) or `claude -p
    /// "<prompt>"` (non-interactive). `dangerously_skip_permissions`
    /// maps to `--dangerously-skip-permissions`. Optional `model` maps
    /// to `--model <id>`.
    ClaudeCode {
        /// Empty string → launch interactive Claude with no prompt.
        prompt: String,
        /// When true, use `-p` (print mode, non-interactive). When
        /// false, launch interactive `claude` and send the prompt as a
        /// follow-up keystroke once the TUI is ready.
        non_interactive: bool,
        /// `--model <id>`. None omits the flag.
        model: Option<String>,
        /// Add `--dangerously-skip-permissions`.
        dangerously_skip_permissions: bool,
    },
}

/// When a schedule fires.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// Standard 5-field cron (`m h dom mon dow`) interpreted in the
    /// user's chosen IANA timezone.
    Cron {
        /// `0 9 * * 1-5` etc. Validated at save time.
        expr: String,
        /// IANA timezone name, e.g. `America/Los_Angeles`. Defaults to
        /// the host machine's local timezone when the user hasn't
        /// picked one.
        tz: String,
    },
    /// Run exactly once at this absolute unix-ms timestamp. The
    /// supervisor disables the schedule after a successful Once fire so
    /// it doesn't spuriously refire on next boot.
    Once { at: u64 },
    /// Run every `seconds` seconds, starting `seconds` from registration.
    Interval { seconds: u32 },
}

/// Where to land the new window.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceTarget {
    /// Resolve by tmux session name; create the workspace if no session
    /// matches. Default `Named { name: "scheduled" }` keeps cron output
    /// out of the user's day-to-day workspaces.
    Named { name: String },
}

/// One historical run of a schedule. Kept in-memory only in v1.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ScheduleRun {
    pub id: ScheduleRunId,
    pub schedule_id: ScheduleId,
    /// Unix ms.
    pub started_at: u64,
    /// Unix ms when the spawn completed (success) or failed.
    pub finished_at: u64,
    pub status: ScheduleRunStatus,
    /// Tmux window id when the run successfully spawned a window.
    pub window_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleRunStatus {
    /// Window opened and command was sent.
    Ok,
    /// Spawn failed (host won't connect, cwd missing, send-keys errored).
    Failed { reason: String },
    /// User triggered `schedule_run_now` while the schedule was disabled.
    Manual,
}

// ---------- Anchor RPC ----------
//
// Wire protocol between subscriber helm processes and the anchor helm
// process. Newline-delimited JSON over a stream (unix socket on
// loopback in 1b; SSH-piped stdio in 1c). Kept narrow — only the
// events subscribers actually need from the anchor (notifications,
// schedules, host list). Tmux output and host-key prompts are
// deliberately excluded; subscribers keep their own SSH connections
// for interactive work.
//
// These types don't carry `#[derive(Type)]` because they don't cross
// the Tauri↔TS boundary. They cross Rust↔Rust over the socket.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnchorEvent {
    /// A pane wants the user's attention. Mirrors `HostEvent::Notification`.
    Notification {
        host_id: HostId,
        notification: Notification,
    },
    /// A previously-emitted notification was dismissed.
    NotificationDismissed {
        host_id: HostId,
        notification_id: NotificationId,
    },
    /// A schedule was created / updated / re-enabled.
    ScheduleUpserted { schedule: Schedule },
    /// A schedule was deleted from the registry.
    ScheduleRemoved { schedule_id: ScheduleId },
    /// A scheduled run successfully opened a window.
    ScheduleFired {
        schedule_id: ScheduleId,
        run_id: ScheduleRunId,
        started_at: u64,
        window_id: String,
        manual: bool,
    },
    /// A host was added or updated. Subscribers use this to keep their
    /// projected host list aligned with the anchor.
    HostUpserted { host: Host },
    /// A host was removed.
    HostRemoved { host_id: HostId },
}

/// Reverse of `host_event_to_anchor_event`. A subscriber translates
/// every event it receives from the anchor back into a HostEvent and
/// pushes it onto the local frontend channel — so the existing UI
/// glue (which listens on HostEvent) Just Works in subscriber mode.
/// Total over `AnchorEvent` since every variant maps to exactly one
/// HostEvent variant.
pub fn anchor_event_to_host_event(event: AnchorEvent) -> HostEvent {
    match event {
        AnchorEvent::Notification {
            host_id,
            notification,
        } => HostEvent::Notification {
            host_id,
            notification,
        },
        AnchorEvent::NotificationDismissed {
            host_id,
            notification_id,
        } => HostEvent::NotificationDismissed {
            host_id,
            notification_id,
        },
        AnchorEvent::ScheduleUpserted { schedule } => HostEvent::ScheduleUpserted { schedule },
        AnchorEvent::ScheduleRemoved { schedule_id } => HostEvent::ScheduleRemoved { schedule_id },
        AnchorEvent::ScheduleFired {
            schedule_id,
            run_id,
            started_at,
            window_id,
            manual,
        } => HostEvent::ScheduleFired {
            schedule_id,
            run_id,
            started_at,
            window_id,
            manual,
        },
        AnchorEvent::HostUpserted { host } => HostEvent::HostAdded { host },
        AnchorEvent::HostRemoved { host_id } => HostEvent::HostRemoved { host_id },
    }
}

/// Translate a HostEvent to its AnchorEvent equivalent, if applicable.
/// Returns None for variants that shouldn't cross to subscribers (Tmux
/// output, host-key prompts, status transitions, tool-integration
/// suggestions). Used at the emit-time fan-out: every emit_event call
/// site that produces a translatable HostEvent also broadcasts the
/// AnchorEvent so the RPC server can forward to subscribers.
pub fn host_event_to_anchor_event(event: &HostEvent) -> Option<AnchorEvent> {
    match event {
        HostEvent::Notification {
            host_id,
            notification,
        } => Some(AnchorEvent::Notification {
            host_id: *host_id,
            notification: notification.clone(),
        }),
        HostEvent::NotificationDismissed {
            host_id,
            notification_id,
        } => Some(AnchorEvent::NotificationDismissed {
            host_id: *host_id,
            notification_id: *notification_id,
        }),
        HostEvent::ScheduleUpserted { schedule } => Some(AnchorEvent::ScheduleUpserted {
            schedule: schedule.clone(),
        }),
        HostEvent::ScheduleRemoved { schedule_id } => Some(AnchorEvent::ScheduleRemoved {
            schedule_id: *schedule_id,
        }),
        HostEvent::ScheduleFired {
            schedule_id,
            run_id,
            started_at,
            window_id,
            manual,
        } => Some(AnchorEvent::ScheduleFired {
            schedule_id: *schedule_id,
            run_id: *run_id,
            started_at: *started_at,
            window_id: window_id.clone(),
            manual: *manual,
        }),
        HostEvent::HostAdded { host } => Some(AnchorEvent::HostUpserted { host: host.clone() }),
        HostEvent::HostRemoved { host_id } => Some(AnchorEvent::HostRemoved { host_id: *host_id }),
        // Deliberately not translated: Tmux output (subscriber has its
        // own SSH connection), Status (subscriber tracks its own
        // connect state), HostKeyPrompt (interactive, host-local),
        // ToolIntegrationSuggested (UI-local to the machine running
        // the integration).
        HostEvent::Tmux { .. }
        | HostEvent::Status { .. }
        | HostEvent::HostKeyPrompt { .. }
        | HostEvent::ToolIntegrationSuggested { .. } => None,
    }
}

/// One request from subscriber to anchor. `id` echoes back in the
/// response so callers can match async replies to their pending
/// futures. The op fields are flattened to the top level — a hello
/// request reads as `{"kind":"request","id":1,"op":"hello"}` rather
/// than the doubly-nested `{"kind":"request","id":1,"op":{"op":"hello"}}`
/// you'd get without the flatten attribute. Easier to write by hand
/// and more idiomatic JSON-RPC-ish.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RpcClientMessage {
    Request {
        id: u64,
        #[serde(flatten)]
        op: RpcOp,
    },
}

/// One message from anchor to subscriber. Either a reply to a prior
/// request (`Ok` / `Err`, both carry the matching `id`) or a pushed
/// `Event` that arrives without a request (post-Subscribe stream).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RpcServerMessage {
    Ok { id: u64, body: RpcResult },
    Err { id: u64, message: String },
    Event { event: AnchorEvent },
}

/// The op the subscriber wants the anchor to perform. Narrow on
/// purpose — Phase 1b only wires the notification ops end-to-end.
/// Schedule + host ops land in 1d when the subscriber-side UI bindings
/// stitch up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RpcOp {
    /// Handshake — returns the anchor's version so subscribers can
    /// detect incompatible versions before doing real work.
    Hello,
    /// Snapshot the current world (notifications + schedules) and
    /// start the push stream. The reply's `body` is
    /// `RpcResult::Subscribed`; subsequent `Event`s arrive without
    /// requests.
    Subscribe,

    ListNotifications,
    /// Renamed from `id` to avoid colliding with the outer request id
    /// once the op is flattened into RpcClientMessage::Request.
    DismissNotification { notification_id: NotificationId },

    ListSchedules,
    SaveSchedule { schedule: Schedule },
    DeleteSchedule { schedule_id: ScheduleId },
    SetScheduleEnabled { schedule_id: ScheduleId, enabled: bool },
    RunScheduleNow { schedule_id: ScheduleId },
}

/// Reply payload for a successful request. Variant chosen by the op.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RpcResult {
    Hello {
        version: String,
    },
    /// Initial snapshot. Subscribers use these to prime their local
    /// projections before processing the event stream. Hosts are
    /// intentionally absent in v1 — subscribers keep their own local
    /// host registry; hosts-on-anchor sync is a later sub-phase.
    Subscribed {
        notifications: Vec<Notification>,
        schedules: Vec<Schedule>,
    },
    Notifications {
        notifications: Vec<Notification>,
    },
    Schedules {
        schedules: Vec<Schedule>,
    },
    SavedSchedule {
        schedule_id: ScheduleId,
    },
    Ack,
}

// ---------- Errors ----------

#[derive(Debug, thiserror::Error, Serialize, Type)]
#[serde(tag = "kind", content = "message")]
pub enum DomainError {
    #[error("host not found")]
    HostNotFound,
    #[error("workspace not found")]
    WorkspaceNotFound,
    #[error("window not found")]
    WindowNotFound,
    #[error("pane not found")]
    PaneNotFound,
    #[error("transport: {0}")]
    Transport(String),
    #[error("tmux: {0}")]
    Tmux(String),
    #[error("io: {0}")]
    Io(String),
}

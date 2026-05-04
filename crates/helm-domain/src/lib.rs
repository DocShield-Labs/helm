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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputMarker {
    /// BEL (0x07) seen in pane output. Most CLI tools (Claude Code,
    /// finished long-running commands, IRC pings) emit this when they
    /// want the user's attention.
    Bell,
    /// `OSC 133;A` — the shell is about to print a new prompt.
    PromptStart,
    /// `OSC 133;B` — the prompt has finished printing; what follows is
    /// the user's typed command.
    CommandStart,
    /// `OSC 133;C` — the user pressed Enter; what follows is command
    /// output.
    OutputStart,
    /// `OSC 133;D[;<exit_code>]` — the previous command finished with the
    /// given exit code. None when the shell didn't include a code (older
    /// integration scripts, partial sequences).
    CommandDone { exit_code: Option<i32> },
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
        markers: Vec<OutputMarker>,
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
}

impl Host {
    /// Convenience constructor for the always-present localhost entry.
    pub fn localhost() -> Self {
        Self {
            id: HostId::new(),
            name: "localhost".into(),
            hostname: "localhost".into(),
            port: 0,
            user: whoami_or_unknown(),
            auth: AuthMethod::Agent,
            jump_host: None,
            tmux_integration: true,
            default_workspace: "default".into(),
            startup_commands: vec![],
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

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

// ---------- session events (helmd → frontend, cross the IPC boundary) ----------
//
// The Rust side talks to a helmd daemon per host (see `helm-proto` /
// `helmd`); these are the frontend-facing mirrors of that protocol.
// Ids are the daemon's u64s stringified ("1", "2", …) — string-keyed
// maps are what the frontend store wants, and the ids stay opaque.

/// The workspace → window → pane tree for one host, as reported by its
/// daemon. Sent whole on every change — small, and keeps the frontend
/// trivially convergent (no delta bookkeeping).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SessionTree {
    pub workspaces: Vec<WorkspaceInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct WindowInfo {
    pub id: String,
    pub name: String,
    pub panes: Vec<PaneInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct PaneInfo {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
    pub alt_screen: bool,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// argv[0] basename of what was spawned, when not the default shell.
    pub command: Option<String>,
}

// ---------- terminal rows (the daemon's model, mirrored) ----------
//
// Colors are packed into one number to keep history pages small on the
// JSON boundary: -1 = default, 0..=255 = indexed, >= 1<<24 = truecolor
// with `(1<<24) | (r<<16) | (g<<8) | b`.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SpanInfo {
    pub text: String,
    pub fg: i32,
    pub bg: i32,
    /// `helm_proto::attrs` bits.
    pub attrs: u16,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct RowInfo {
    pub spans: Vec<SpanInfo>,
    /// Soft-wrapped: the next row continues this logical line.
    pub wrapped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct CursorInfo {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ScreenInfo {
    pub cols: u16,
    pub rows: u16,
    pub top_line: u64,
    /// Oldest history line the daemon still holds.
    pub history_start: u64,
    pub lines: Vec<RowInfo>,
    pub cursor: CursorInfo,
    /// `helm_proto::modes` bits.
    pub modes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct RowAt {
    pub index: u16,
    pub row: RowInfo,
}

/// Reply to `session_history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct HistoryPage {
    pub from_line: u64,
    pub rows: Vec<RowInfo>,
    pub history_start: u64,
    pub top_line: u64,
}

/// One command block, segmented daemon-side from OSC 133 markers. The
/// block-native frontend renders these as first-class list items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct BlockInfo {
    pub id: String,
    /// Absolute lines in the pane's line space (prompt start / command
    /// accepted / output begins / finished).
    pub start_line: u64,
    pub cmd_line: Option<u64>,
    pub output_line: Option<u64>,
    pub end_line: Option<u64>,
    pub cmdline: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

/// One scrollback-search hit with an exact jump anchor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SearchHit {
    pub pane_id: String,
    pub block_id: Option<String>,
    /// Absolute line of the matched row.
    pub line: u64,
    /// The matched row's text.
    pub line_text: String,
    pub match_start: u32,
    pub match_end: u32,
}

/// Per-host session events streamed from the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The pane's whole grid — on request, resize, alt-screen swap, or
    /// any change the model reports as full damage. Paint it all.
    Screen { pane_id: String, screen: ScreenInfo },
    /// Rows that changed since the last screen message, plus cursor
    /// and modes as of now. The grid first scrolled up by `scroll` rows
    /// (delivered as `HistoryAppend`); row indices are post-scroll.
    ScreenDiff {
        pane_id: String,
        top_line: u64,
        scroll: u16,
        rows: Vec<RowAt>,
        cursor: CursorInfo,
        modes: u32,
    },
    /// Rows that scrolled out of the grid: `first_line` is the absolute
    /// line of `rows[0]`; the grid's top is now `first_line + rows.len()`.
    HistoryAppend {
        pane_id: String,
        first_line: u64,
        rows: Vec<RowInfo>,
    },
    /// A block started / gained its command / finished.
    Block { pane_id: String, block: BlockInfo },
    /// The pane entered or left the alternate screen (TUI mode). The
    /// frontend swaps block-list ⇄ grid rendering on this.
    ModeChange { pane_id: String, alt_screen: bool },
    /// Full tree snapshot after any lifecycle change.
    Tree { tree: SessionTree },
    PaneExited { pane_id: String, status: Option<i32> },
    /// The pane rang its bell. Emitted for every bell — including ones
    /// the inbox suppresses because the window is focused — so the pane
    /// view can react (an agent waiting on a choice) without depending
    /// on the notification queue. helmd strips BEL from the byte stream,
    /// so this is the only way the pane hears it.
    Bell { pane_id: String },
}

// ---------- Host ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Connected,
    Connecting,
    /// Transport dropped or the daemon bounced; the supervisor is
    /// running its backoff ladder. Distinct from `Connecting` so the UI
    /// can render a different overlay (the user's panes stay mounted
    /// with their last frozen frame instead of being torn down).
    Reconnecting,
    Disconnected,
    Idle,
    Error,
}

/// Single event channel from Rust to the frontend. Session events,
/// host-status transitions, and registry mutations interleave on the same
/// stream so the frontend sees them in order with everything else.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEvent {
    Session {
        host_id: HostId,
        event: SessionEvent,
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
    /// Workspace id (daemon id, stringified). Optional because the event
    /// may arrive before the tree is known; the frontend can fill in the
    /// breadcrumb from the window id alone.
    pub workspace_id: Option<String>,
    /// Window id (daemon id, stringified).
    pub window_id: String,
    /// Pane id (daemon id, stringified) — the pane the event came from,
    /// so the inbox row can route the user to the exact one.
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
    /// OSC 9 notification with a message ("Claude finished"). Informational:
    /// unlike a bell it does not mean the program is waiting on the user.
    Message { text: String },
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
    pub default_workspace: String,
    pub startup_commands: Vec<String>,
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

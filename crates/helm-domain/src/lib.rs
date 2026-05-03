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

// ---------- tmux notifications (cross the IPC boundary) ----------

/// Wire format for tmux state deltas. Mirrors `helm-tmux::parse::Notification`
/// — kept here so the type lives next to the rest of the IPC vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TmuxNotification {
    Output { pane_id: String, bytes: Vec<u8> },
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

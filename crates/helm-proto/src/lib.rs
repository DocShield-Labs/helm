//! Wire protocol between the helm app and the helmd persistence daemon.
//!
//! Transport-agnostic: the same frames flow over a unix socket (localhost)
//! or an SSH exec channel running `helmd stdio` (remote). Framing is a
//! u32-LE length prefix followed by a bincode-encoded message. Unlike the
//! tmux control-mode protocol this replaces, the stream is binary-safe
//! end to end: no line orientation, no octal escaping, and decoding is
//! stateful across arbitrarily-split reads (see `FrameDecoder`).

use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[cfg(feature = "client")]
pub mod client;

/// Bump on any breaking change to the message types. The daemon refuses
/// mismatched clients in `HelloAck` and the app responds by re-installing
/// the daemon binary it shipped with.
pub const PROTOCOL_VERSION: u32 = 3;

/// Upper bound on a single frame. Output frames are batched well below
/// this (64 KB flushes); the cap exists so a corrupt length prefix fails
/// fast instead of allocating gigabytes.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame length {0} exceeds MAX_FRAME_LEN")]
    FrameTooLarge(u32),
    #[error("encode: {0}")]
    Encode(#[source] bincode::Error),
    #[error("decode: {0}")]
    Decode(#[source] bincode::Error),
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        pub struct $name(pub u64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                s.parse::<u64>()
                    .map(Self)
                    .map_err(|_| format!("invalid {} id: {s:?}", stringify!($name)))
            }
        }
    };
}

id_newtype!(WorkspaceId);
id_newtype!(WindowId);
id_newtype!(PaneId);
id_newtype!(BlockId);
id_newtype!(NotificationId);

// ---------------------------------------------------------------------------
// Client → daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on every connection. The daemon replies `HelloAck`
    /// (or `Error` + close on version mismatch).
    Hello {
        protocol_version: u32,
        /// Human-readable client identity for daemon logs ("helm-app 0.1.1").
        client_name: String,
    },
    /// Subscribe to live output. `resume` carries the last seq the client
    /// has per pane; the daemon replays everything after each point before
    /// switching that pane to live tail — the reattach path, exact by
    /// construction.
    Attach { resume: Vec<(PaneId, u64)> },
    /// Keystrokes / pasted bytes for a pane's PTY. Raw bytes — no
    /// encoding, no hex, no send-keys quoting hazards.
    Input { pane: PaneId, bytes: Vec<u8> },
    /// The focused client owns the size. Last writer wins; no unions.
    Resize { pane: PaneId, cols: u16, rows: u16 },
    /// Request scrollback. Answered with `Output` frames (historical
    /// bytes, correctly seq-stamped) followed by `ReplayDone`.
    Replay { pane: PaneId, from: ReplayFrom },
    /// Create a workspace. Also spawns an initial window (default
    /// shell) so a fresh workspace is immediately usable — the tmux
    /// "session always has a window" semantic. Answered with `Created`
    /// carrying `req_id` (plus a `TreeChanged` broadcast).
    NewWorkspace { req_id: u64, name: Option<String> },
    /// Answered with `Created` carrying `req_id`.
    NewWindow {
        req_id: u64,
        workspace: WorkspaceId,
        name: Option<String>,
        cwd: Option<String>,
        /// argv to exec instead of the default login shell.
        command: Option<Vec<String>>,
    },
    KillWindow { window: WindowId },
    KillWorkspace { workspace: WorkspaceId },
    RenameWorkspace { workspace: WorkspaceId, name: String },
    RenameWindow { window: WindowId, name: String },
    /// Server-side scrollback search over the ANSI-stripped line index.
    /// Answered with `SearchResults` carrying `req_id`.
    Search {
        req_id: u64,
        query: String,
        regex: bool,
        case_sensitive: bool,
        scope: SearchScope,
        max_results: u32,
    },
    /// The pane's block table (historical blocks are not streamed —
    /// a reattaching client asks once per pane). Answered with `Blocks`.
    Blocks { req_id: u64, pane: PaneId },
    /// Round-trip probe (the app's latency readout). Answered with `Pong`.
    Ping { req_id: u64 },
    /// The client has shown these to the user; drop them from the
    /// daemon's offline queue.
    AckNotifications { up_to: NotificationId },
    /// Orderly daemon shutdown (dev tooling; the app never sends this).
    Shutdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ReplayFrom {
    /// Everything at and after this absolute byte offset.
    Seq(u64),
    /// The most recent N bytes (first paint of a pane we've never seen).
    LastBytes(u64),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SearchScope {
    All,
    Workspace(WorkspaceId),
    Pane(PaneId),
}

// ---------------------------------------------------------------------------
// Daemon → client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonMsg {
    HelloAck {
        protocol_version: u32,
        /// Daemon build version (cargo pkg version) for upgrade checks.
        daemon_version: String,
        state: TreeSnapshot,
        /// Everything that happened while no client was attached —
        /// strictly more information than tmux's one-bit bell flag.
        pending: Vec<Notification>,
    },
    /// Live or replayed pane bytes. `seq` is the absolute offset of
    /// `bytes[0]` since pane creation; contiguity is checkable by the
    /// client (`seq + bytes.len()` == next frame's `seq`). Batched by
    /// the daemon (5 ms / 64 KB flush) — never per-line.
    Output { pane: PaneId, seq: u64, bytes: Vec<u8> },
    /// End of a `Replay` response; the pane is live-tailing from `at_seq`.
    ReplayDone { pane: PaneId, at_seq: u64 },
    /// OSC 133 segmentation, parsed statefully at ingest. Sent on block
    /// start (prompt), on command capture, and on finish (exit code).
    Block { pane: PaneId, block: BlockMeta },
    /// DECSET 1049/47 tracking: the frontend swaps the block list for a
    /// plain grid while a TUI holds the alt screen.
    ModeChange { pane: PaneId, alt_screen: bool },
    /// Any workspace/window/pane lifecycle change. Full snapshot — small,
    /// and it keeps the client trivially convergent.
    TreeChanged { state: TreeSnapshot },
    PaneExited { pane: PaneId, status: Option<i32> },
    /// Reply to `NewWorkspace` / `NewWindow`, sent only to the
    /// requesting client. `workspace` always set; `window`/`pane` set
    /// for both ops (a new workspace comes with its initial window).
    Created {
        req_id: u64,
        workspace: WorkspaceId,
        window: Option<WindowId>,
        pane: Option<PaneId>,
    },
    SearchResults { req_id: u64, matches: Vec<SearchMatch>, truncated: bool },
    /// Reply to `Blocks`: every block the daemon retains for the pane,
    /// oldest first.
    Blocks { req_id: u64, pane: PaneId, blocks: Vec<BlockMeta> },
    Pong { req_id: u64 },
    Notification { note: Notification },
    /// A failed operation. `req_id` is set when the failure answers a
    /// request/reply message, so the waiter resolves instead of timing out.
    Error { req_id: Option<u64>, context: String, message: String },
}

impl DaemonMsg {
    /// Correlation id for reply messages. The one place that knows which
    /// variants answer requests — clients route anything with a req_id to
    /// the matching waiter.
    pub fn req_id(&self) -> Option<u64> {
        match self {
            DaemonMsg::Created { req_id, .. }
            | DaemonMsg::SearchResults { req_id, .. }
            | DaemonMsg::Blocks { req_id, .. }
            | DaemonMsg::Pong { req_id } => Some(*req_id),
            DaemonMsg::Error { req_id, .. } => *req_id,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeSnapshot {
    pub workspaces: Vec<WorkspaceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub name: String,
    pub panes: Vec<PaneInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub cols: u16,
    pub rows: u16,
    pub alt_screen: bool,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Foreground command name if known ("claude", "cargo", …).
    pub command: Option<String>,
    /// Next byte offset the pane will produce (== total bytes ever).
    pub head_seq: u64,
    /// Oldest byte still in the ring buffer. Replay below this is gone.
    pub buffer_start_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMeta {
    pub id: BlockId,
    /// OSC 133 A — prompt start.
    pub start_seq: u64,
    /// OSC 133 B — command accepted. None while still at the prompt.
    pub cmd_seq: Option<u64>,
    /// OSC 133 C — output begins.
    pub output_seq: Option<u64>,
    /// OSC 133 D — command finished. None while running.
    pub end_seq: Option<u64>,
    pub cmdline: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub pane: PaneId,
    pub kind: NotificationKind,
    /// ANSI-stripped tail of the pane at event time, ≤ ~120 chars.
    pub preview: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotificationKind {
    Bell,
    /// OSC 9 (`ESC ] 9 ; text BEL`) — a notification with a message,
    /// the convention iTerm2 / ConEmu / Windows Terminal use. Unlike a
    /// bell it does not mean the program is blocked on the user.
    Message { text: String },
    /// A command finished with a non-zero exit. Carries what the daemon
    /// already knows from the block so offline notifications are as
    /// rich as live ones.
    CommandDone {
        exit_code: i32,
        cmdline: Option<String>,
        duration_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub pane: PaneId,
    /// Which block the line belongs to, when block-indexed.
    pub block: Option<BlockId>,
    /// Seq of the first byte of the matched line — the jump anchor.
    pub line_seq: u64,
    /// The matched line, ANSI-stripped, possibly truncated around the match.
    pub line_text: String,
    /// Byte range of the match within `line_text`.
    pub match_start: u32,
    pub match_end: u32,
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Encode one message as a length-prefixed frame.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    // Serialize straight after a length placeholder, then patch it —
    // one allocation, no body copy.
    let mut out = vec![0u8; 4];
    bincode::serialize_into(&mut out, msg).map_err(ProtoError::Encode)?;
    let len = (out.len() - 4) as u32;
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge(len));
    }
    out[..4].copy_from_slice(&len.to_le_bytes());
    Ok(out)
}

/// Incremental frame decoder. Feed it whatever the transport hands you —
/// bytes split at any boundary, including mid-length-prefix — and pull
/// complete messages out. This statefulness across reads is a hard
/// requirement learned from the tmux path, where a per-chunk parser
/// mangled escape sequences split across `%output` chunks.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Start of unconsumed bytes. Consumed frames are skipped by
    /// advancing this and compacted lazily, so a read full of small
    /// frames costs one memmove, not one per frame.
    pos: usize,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Decode the next complete frame, or `Ok(None)` if more bytes are
    /// needed. Errors are fatal for the connection (corrupt stream).
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtoError> {
        let avail = &self.buf[self.pos..];
        if avail.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]);
        if len > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge(len));
        }
        let total = 4 + len as usize;
        if avail.len() < total {
            return Ok(None);
        }
        let msg = bincode::deserialize(&avail[4..total]).map_err(ProtoError::Decode)?;
        self.pos += total;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        Ok(Some(msg))
    }
}

/// Connect to `socket`, spawning `helmd_bin serve --socket <socket>`
/// first when nothing is listening, and polling until it binds. The
/// one spawn ladder shared by the app's local connect and the daemon's
/// own `stdio` bridge. std-only (no feature needed).
#[cfg(unix)]
pub fn connect_or_spawn_socket(
    socket: &std::path::Path,
    helmd_bin: &std::path::Path,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;
    if let Ok(s) = UnixStream::connect(socket) {
        return Ok(s);
    }
    std::process::Command::new(helmd_bin)
        .arg("serve")
        .arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(socket) {
            return Ok(s);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("helmd serve did not come up on {}", socket.display()),
    ))
}

/// Ask the daemon on `socket` to exit and make sure it did. A
/// `Shutdown` frame first (any client may send one — it's the user's
/// own daemon), then a `pkill` of `helmd serve --socket <socket>` for
/// daemons too old to understand the frame, then the socket path is
/// unlinked so a fresh `serve` can bind. Used when a running daemon
/// speaks another protocol version than the app: the sessions inside
/// it are unreachable either way, so restarting loses nothing usable.
#[cfg(unix)]
pub fn shutdown_socket(socket: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let alive = || UnixStream::connect(socket).is_ok();
    let wait_dead = || {
        for _ in 0..20 {
            if !alive() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        !alive()
    };
    if let Ok(mut s) = UnixStream::connect(socket) {
        if let Ok(frame) = encode_frame(&ClientMsg::Shutdown) {
            let _ = s.write_all(&frame);
            let _ = s.flush();
        }
    }
    if !wait_dead() {
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(format!("helmd serve --socket {}", socket.display()))
            .status();
        if !wait_dead() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("helmd on {} would not exit", socket.display()),
            ));
        }
    }
    let _ = std::fs::remove_file(socket);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_client_msgs() -> Vec<ClientMsg> {
        vec![
            ClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "helm-app test".into(),
            },
            ClientMsg::Input {
                pane: PaneId(7),
                bytes: b"cargo test\r".to_vec(),
            },
            ClientMsg::Replay {
                pane: PaneId(7),
                from: ReplayFrom::Seq(4096),
            },
            ClientMsg::Search {
                req_id: 9,
                query: "retry backoff".into(),
                regex: false,
                case_sensitive: false,
                scope: SearchScope::All,
                max_results: 60,
            },
        ]
    }

    fn sample_daemon_msgs() -> Vec<DaemonMsg> {
        vec![
            DaemonMsg::Output {
                pane: PaneId(7),
                seq: 123_456,
                // Deliberately includes the byte patterns that broke the
                // tmux path: bare ESC, BEL, OSC 8 fragments, newlines.
                bytes: b"\x1b]8;;file:///a\x07label\x1b]8;;\x07\ntail\x1b".to_vec(),
            },
            DaemonMsg::Block {
                pane: PaneId(7),
                block: BlockMeta {
                    id: BlockId(3),
                    start_seq: 100,
                    cmd_seq: Some(120),
                    output_seq: Some(140),
                    end_seq: Some(9000),
                    cmdline: Some("cargo build --release".into()),
                    cwd: Some("/Users/x/code".into()),
                    branch: Some("main".into()),
                    exit_code: Some(0),
                    started_at_ms: Some(1_700_000_000_000),
                    finished_at_ms: Some(1_700_000_042_100),
                },
            },
            DaemonMsg::ModeChange {
                pane: PaneId(7),
                alt_screen: true,
            },
        ]
    }

    #[test]
    fn roundtrip_whole_frames() {
        let mut dec = FrameDecoder::new();
        for msg in sample_client_msgs() {
            dec.feed(&encode_frame(&msg).unwrap());
            let back: ClientMsg = dec.next().unwrap().expect("complete frame");
            assert_eq!(format!("{msg:?}"), format!("{back:?}"));
        }
        assert!(dec.next::<ClientMsg>().unwrap().is_none());
    }

    /// The decoder must survive arbitrary split points — the exact
    /// failure mode of the per-chunk tmux parser. Feed byte-at-a-time.
    #[test]
    fn roundtrip_byte_at_a_time() {
        let msgs = sample_daemon_msgs();
        let mut wire = Vec::new();
        for msg in &msgs {
            wire.extend_from_slice(&encode_frame(msg).unwrap());
        }
        let mut dec = FrameDecoder::new();
        let mut out = Vec::new();
        for b in wire {
            dec.feed(&[b]);
            while let Some(msg) = dec.next::<DaemonMsg>().unwrap() {
                out.push(msg);
            }
        }
        assert_eq!(out.len(), msgs.len());
        for (a, b) in msgs.iter().zip(out.iter()) {
            assert_eq!(format!("{a:?}"), format!("{b:?}"));
        }
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut dec = FrameDecoder::new();
        dec.feed(&(MAX_FRAME_LEN + 1).to_le_bytes());
        dec.feed(&[0u8; 8]);
        assert!(matches!(
            dec.next::<DaemonMsg>(),
            Err(ProtoError::FrameTooLarge(_))
        ));
    }
}

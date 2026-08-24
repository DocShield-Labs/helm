//! Wire protocol between the helm app and the helmd persistence daemon.
//!
//! Transport-agnostic: the same frames flow over a unix socket (localhost)
//! or an SSH exec channel running `helmd stdio` (remote). Framing is a
//! u32-LE length prefix followed by a bincode-encoded message. Unlike the
//! tmux control-mode protocol this replaces, the stream is binary-safe
//! end to end: no line orientation, no octal escaping, and decoding is
//! stateful across arbitrarily-split reads (see `FrameDecoder`).
//!
//! Since v4 the daemon owns the terminal model: clients never see raw
//! PTY bytes. A session is a grid (`Screen`, updated by `ScreenDiff`) plus
//! a line history of rows that scrolled off the top (`HistoryAppend`,
//! paged with `History`). Positions are absolute line numbers that are
//! monotonic for the session's lifetime — blocks, search hits and history
//! pages all address the same line space.

use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[cfg(feature = "client")]
pub mod client;

/// Bump on any breaking change to the message types. The daemon refuses
/// mismatched clients in `HelloAck` and the app responds by re-installing
/// the daemon binary it shipped with.
///
/// Frames are bincode, which tags enum variants by index, so after a
/// bump the old daemon's `Error` rejection may decode on the new client
/// as a different variant entirely. The app treats any non-`HelloAck`
/// first message as a stale daemon for that reason — don't rely on the
/// rejection text surviving a version gap.
pub const PROTOCOL_VERSION: u32 = 7;

/// Upper bound on a single frame. History pages are capped well below
/// this; the cap exists so a corrupt length prefix fails fast instead
/// of allocating gigabytes.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Most rows one `History` reply carries. Clients page.
pub const MAX_HISTORY_PAGE: u64 = 4000;

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

id_newtype!(SessionId);
id_newtype!(BlockId);
id_newtype!(NotificationId);

// ---------------------------------------------------------------------------
// Terminal model
// ---------------------------------------------------------------------------

/// A cell color. Indexed covers the 16 ANSI + 256-color cube; the
/// client maps `Default` and indices through its theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// SGR attribute bits carried by `Style::attrs`.
pub mod attrs {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const STRIKE: u16 = 1 << 5;
    pub const HIDDEN: u16 = 1 << 6;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 7;
    pub const UNDERCURL: u16 = 1 << 8;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: u16,
    /// OSC 8 hyperlink target, when the run is a link.
    pub link: Option<String>,
}

/// A run of cells sharing one style. Wide characters appear once (their
/// spacer cell is dropped); zero-width combining characters follow the
/// cell they attach to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One terminal row. Trailing default-style blanks are trimmed, so an
/// empty row has no spans.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Row {
    pub spans: Vec<Span>,
    /// The row was soft-wrapped: the next row continues the same
    /// logical line. Lets a client reflow history at its own width.
    pub wrapped: bool,
}

impl Row {
    /// Plain text of the row (search, previews).
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.spans.iter().map(|sp| sp.text.len()).sum());
        for sp in &self.spans {
            s.push_str(&sp.text);
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: bool,
}

/// DEC private modes a painting client must mirror so its input
/// encoding (arrow keys, mouse reports, paste wrapping, focus events)
/// matches what the application asked for.
pub mod modes {
    pub const APP_CURSOR: u32 = 1 << 0;
    pub const APP_KEYPAD: u32 = 1 << 1;
    pub const BRACKETED_PASTE: u32 = 1 << 2;
    pub const FOCUS_IN_OUT: u32 = 1 << 3;
    pub const MOUSE_CLICK: u32 = 1 << 4;
    pub const MOUSE_DRAG: u32 = 1 << 5;
    pub const MOUSE_MOTION: u32 = 1 << 6;
    pub const SGR_MOUSE: u32 = 1 << 7;
    pub const UTF8_MOUSE: u32 = 1 << 8;
    pub const ALT_SCREEN: u32 = 1 << 9;
    pub const ALTERNATE_SCROLL: u32 = 1 << 10;
}

/// Full grid snapshot — what a client paints on attach, resize, or any
/// change alacritty reports as full damage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// Absolute line number of grid row 0.
    pub top_line: u64,
    /// Oldest history line the daemon still holds.
    pub history_start: u64,
    /// Exactly `rows` entries.
    pub lines: Vec<Row>,
    pub cursor: Cursor,
    pub modes: u32,
}

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
    /// Subscribe to live broadcasts (screen diffs, history, blocks,
    /// tree changes, notifications). Session contents are pulled with
    /// `Screen` / `History` as sessions are shown.
    Attach,
    /// Keystrokes / pasted bytes for a session's PTY. Raw bytes — no
    /// encoding, no hex, no send-keys quoting hazards.
    Input {
        session: SessionId,
        bytes: Vec<u8>,
    },
    /// The focused client owns the size. Last writer wins; no unions.
    Resize {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// The session's current grid. Answered with `Screen` carrying `req_id`.
    Screen {
        req_id: u64,
        session: SessionId,
    },
    /// History rows in `[from_line, to_line)`, clamped to what the
    /// daemon retains and to `MAX_HISTORY_PAGE` (from the end, so a
    /// client paging backwards gets the newest rows first). Answered
    /// with `History` carrying `req_id`.
    History {
        req_id: u64,
        session: SessionId,
        from_line: u64,
        to_line: u64,
    },
    /// Create a long-running terminal session. Answered with `Created`
    /// carrying `req_id` plus a `TreeChanged` broadcast.
    NewSession {
        req_id: u64,
        name: Option<String>,
        cwd: Option<String>,
        /// argv to exec instead of the default login shell.
        command: Option<Vec<String>>,
    },
    KillSession {
        session: SessionId,
    },
    RenameSession {
        session: SessionId,
        name: String,
    },
    /// Server-side search over history + grid rows (plain text of each
    /// row). Answered with `SearchResults` carrying `req_id`.
    Search {
        req_id: u64,
        query: String,
        regex: bool,
        case_sensitive: bool,
        scope: SearchScope,
        max_results: u32,
    },
    /// The session's block table (historical blocks are not streamed —
    /// a reattaching client asks once per session). Answered with `Blocks`.
    Blocks {
        req_id: u64,
        session: SessionId,
    },
    /// Filesystem path candidates relative to the session's current cwd.
    /// `path` is an unescaped shell token prefix; quoting and insertion
    /// remain a client concern. Answered with `PathCompletions`.
    CompletePath {
        req_id: u64,
        session: SessionId,
        path: String,
        directories_only: bool,
        max_results: u32,
    },
    /// Round-trip probe (the app's latency readout). Answered with `Pong`.
    Ping {
        req_id: u64,
    },
    /// The client has shown these to the user; drop them from the
    /// daemon's offline queue.
    AckNotifications {
        up_to: NotificationId,
    },
    /// Orderly daemon shutdown (dev tooling; the app never sends this).
    Shutdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SearchScope {
    All,
    Session(SessionId),
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
    /// Full grid. `req_id` is set when answering `ClientMsg::Screen`;
    /// `None` for broadcasts (resize, alt-screen swap, clear — anything
    /// the model reports as full damage).
    Screen {
        req_id: Option<u64>,
        session: SessionId,
        screen: Screen,
    },
    /// Rows that changed since the last `Screen` / `ScreenDiff`, with
    /// the cursor and modes as of now. Coalesced per session at ≤ 60 Hz.
    /// `scroll` rows left the top of the grid first (they arrived as
    /// `HistoryAppend`): shift the grid up by that much, then apply
    /// `rows`, whose indices are post-shift.
    ScreenDiff {
        session: SessionId,
        top_line: u64,
        scroll: u16,
        rows: Vec<(u16, Row)>,
        cursor: Cursor,
        modes: u32,
    },
    /// Rows that scrolled out of the primary grid since the last flush;
    /// `first_line` is the absolute line of `rows[0]` and the new
    /// `top_line` is `first_line + rows.len()`.
    HistoryAppend {
        session: SessionId,
        first_line: u64,
        rows: Vec<Row>,
    },
    /// Reply to `History`. `from_line` is the absolute line of `rows[0]`
    /// (may be later than asked when clamped); `history_start` is the
    /// oldest line the daemon still holds; `top_line` is the grid top.
    History {
        req_id: u64,
        session: SessionId,
        from_line: u64,
        rows: Vec<Row>,
        history_start: u64,
        top_line: u64,
    },
    /// OSC 133 segmentation, parsed statefully at ingest. Sent on block
    /// start (prompt), on command capture, and on finish (exit code).
    Block {
        session: SessionId,
        block: BlockMeta,
    },
    /// DECSET 1049/47 tracking: the frontend swaps the block list for a
    /// plain grid while a TUI holds the alt screen.
    ModeChange {
        session: SessionId,
        alt_screen: bool,
    },
    /// Any session lifecycle change. Full snapshot — small,
    /// and it keeps the client trivially convergent.
    TreeChanged {
        state: TreeSnapshot,
    },
    SessionExited {
        session: SessionId,
        status: Option<i32>,
    },
    /// Reply to `NewSession`, sent only to the requesting client.
    Created {
        req_id: u64,
        session: SessionId,
    },
    SearchResults {
        req_id: u64,
        matches: Vec<SearchMatch>,
        truncated: bool,
    },
    /// Reply to `Blocks`: every block the daemon retains for the session,
    /// oldest first.
    Blocks {
        req_id: u64,
        session: SessionId,
        blocks: Vec<BlockMeta>,
    },
    PathCompletions {
        req_id: u64,
        session: SessionId,
        candidates: Vec<PathCompletion>,
        truncated: bool,
    },
    Pong {
        req_id: u64,
    },
    Notification {
        note: Notification,
    },
    /// A failed operation. `req_id` is set when the failure answers a
    /// request/reply message, so the waiter resolves instead of timing out.
    Error {
        req_id: Option<u64>,
        context: String,
        message: String,
    },
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
            | DaemonMsg::PathCompletions { req_id, .. }
            | DaemonMsg::History { req_id, .. }
            | DaemonMsg::Pong { req_id } => Some(*req_id),
            DaemonMsg::Screen { req_id, .. } | DaemonMsg::Error { req_id, .. } => *req_id,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCompletion {
    /// Completed token value, without shell escaping.
    pub value: String,
    pub kind: PathEntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeSnapshot {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub alt_screen: bool,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Git toplevel of `cwd` (a worktree's own root); `None` outside a
    /// repo. The sidebar groups sessions by it.
    pub root: Option<String>,
    /// Foreground command name if known ("claude", "cargo", …).
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMeta {
    pub id: BlockId,
    /// OSC 133 A — prompt start (absolute line of the cursor).
    pub start_line: u64,
    /// OSC 133 B — command accepted. None while still at the prompt.
    pub cmd_line: Option<u64>,
    /// OSC 133 C — output begins.
    pub output_line: Option<u64>,
    /// OSC 133 D — command finished. None while running.
    pub end_line: Option<u64>,
    pub cmdline: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Git toplevel of `cwd` at the prompt; `None` outside a repo.
    pub root: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub session: SessionId,
    pub kind: NotificationKind,
    /// Last non-empty grid row at event time, ≤ ~120 chars.
    pub preview: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotificationKind {
    Bell,
    /// OSC 9 (`ESC ] 9 ; text BEL`) — a notification with a message,
    /// the convention iTerm2 / ConEmu / Windows Terminal use. Unlike a
    /// bell it does not mean the program is blocked on the user.
    Message {
        text: String,
    },
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
    pub session: SessionId,
    /// Which block the line belongs to, when block-indexed.
    pub block: Option<BlockId>,
    /// Absolute line of the matched row — the jump anchor.
    pub line: u64,
    /// The matched row's text.
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
/// unlinked so a fresh `serve` can bind. This is an explicit destructive
/// operation: callers must not use it merely because protocols differ,
/// since the daemon may own live sessions.
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
                session: SessionId(7),
                bytes: b"cargo test\r".to_vec(),
            },
            ClientMsg::History {
                session: SessionId(7),
                req_id: 4,
                from_line: 4096,
                to_line: 8192,
            },
            ClientMsg::Search {
                req_id: 9,
                query: "retry backoff".into(),
                regex: false,
                case_sensitive: false,
                scope: SearchScope::All,
                max_results: 60,
            },
            ClientMsg::CompletePath {
                req_id: 10,
                session: SessionId(7),
                path: "src/co".into(),
                directories_only: false,
                max_results: 100,
            },
        ]
    }

    fn sample_row() -> Row {
        Row {
            spans: vec![
                Span {
                    text: "error".into(),
                    style: Style {
                        fg: Color::Indexed(1),
                        ..Default::default()
                    },
                },
                Span {
                    text: ": \u{1f600} wide".into(),
                    style: Style {
                        fg: Color::Rgb(1, 2, 3),
                        bg: Color::Default,
                        attrs: attrs::BOLD | attrs::UNDERLINE,
                        link: Some("https://x.dev".into()),
                    },
                },
            ],
            wrapped: true,
        }
    }

    fn sample_daemon_msgs() -> Vec<DaemonMsg> {
        vec![
            DaemonMsg::ScreenDiff {
                session: SessionId(7),
                top_line: 123_456,
                scroll: 1,
                rows: vec![(3, sample_row()), (4, Row::default())],
                cursor: Cursor {
                    row: 4,
                    col: 0,
                    visible: true,
                    shape: CursorShape::Beam,
                    blink: true,
                },
                modes: modes::BRACKETED_PASTE | modes::FOCUS_IN_OUT,
            },
            DaemonMsg::HistoryAppend {
                session: SessionId(7),
                first_line: 123_455,
                rows: vec![sample_row()],
            },
            DaemonMsg::Block {
                session: SessionId(7),
                block: BlockMeta {
                    id: BlockId(3),
                    start_line: 100,
                    cmd_line: Some(101),
                    output_line: Some(102),
                    end_line: Some(9000),
                    cmdline: Some("cargo build --release".into()),
                    cwd: Some("/Users/x/code".into()),
                    branch: Some("main".into()),
                    root: None,
                    exit_code: Some(0),
                    started_at_ms: Some(1_700_000_000_000),
                    finished_at_ms: Some(1_700_000_042_100),
                },
            },
            DaemonMsg::ModeChange {
                session: SessionId(7),
                alt_screen: true,
            },
            DaemonMsg::PathCompletions {
                req_id: 10,
                session: SessionId(7),
                candidates: vec![PathCompletion {
                    value: "src/components/".into(),
                    kind: PathEntryKind::Directory,
                }],
                truncated: false,
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
    fn row_text_joins_spans() {
        assert_eq!(sample_row().text(), "error: \u{1f600} wide");
        assert_eq!(Row::default().text(), "");
    }

    #[test]
    fn reply_correlation() {
        let screen = Screen {
            cols: 1,
            rows: 1,
            top_line: 0,
            history_start: 0,
            lines: vec![Row::default()],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
                shape: CursorShape::Block,
                blink: false,
            },
            modes: 0,
        };
        assert_eq!(
            DaemonMsg::Screen {
                req_id: Some(5),
                session: SessionId(1),
                screen: screen.clone()
            }
            .req_id(),
            Some(5)
        );
        assert_eq!(
            DaemonMsg::Screen {
                req_id: None,
                session: SessionId(1),
                screen
            }
            .req_id(),
            None
        );
        assert_eq!(
            DaemonMsg::History {
                req_id: 8,
                session: SessionId(1),
                from_line: 0,
                rows: vec![],
                history_start: 0,
                top_line: 0
            }
            .req_id(),
            Some(8)
        );
        assert_eq!(
            DaemonMsg::PathCompletions {
                req_id: 9,
                session: SessionId(1),
                candidates: vec![],
                truncated: false,
            }
            .req_id(),
            Some(9)
        );
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

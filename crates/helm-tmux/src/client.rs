//! Live `tmux -CC` client.
//!
//! tmux requires a controlling TTY even in control mode (it calls `tcgetattr`
//! at startup), so we spawn it inside a PTY rather than over plain pipes.
//!
//! Threading model:
//!   - **Reader thread** (`std::thread`): blocking `read_line` from the PTY
//!     master, parses each line, routes notifications to the event channel
//!     (tokio mpsc, consumed by async callers) and `%begin/%end` block data
//!     to the next pending command's oneshot.
//!   - **Writer thread** (`std::thread`): pulls outbound commands from a
//!     plain `std::sync::mpsc` queue and writes them to the master.
//!   - **Wait thread** (`std::thread`): blocking `child.wait()`, purely
//!     informational logging.
//!
//! PTY I/O is fundamentally blocking, so we use `std::thread` rather than
//! pretending otherwise via tokio's blocking pool. The async surface
//! (`send_command`) just enqueues into the sync channel and awaits a
//! oneshot for the response.

use crate::parse::{decode_octal, parse_line, parse_output_bytes, Notification, TmuxLine};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{mpsc as sync_mpsc, Arc};
use std::thread;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum TmuxError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tmux exited unexpectedly")]
    UnexpectedExit,
    #[error("response: {0}")]
    Response(String),
}

type PendingQueue = Arc<Mutex<VecDeque<oneshot::Sender<Result<String, String>>>>>;
type ReadySignal = Arc<Mutex<Option<oneshot::Sender<()>>>>;

/// Cleanup hook invoked when the `TmuxClient` is dropped. Phase 1 used a
/// `portable_pty::ChildKiller`; phase 2 needs a more general escape hatch
/// because the SSH transport's "kill the tmux client" is "drop the russh
/// channel and abort the bridge task" — not a child process.
pub type Cleanup = Box<dyn FnOnce() + Send>;

pub struct TmuxClient {
    cmd_tx: sync_mpsc::Sender<CommandRequest>,
    /// Run on Drop. Tears down whatever is on the other end of the
    /// reader/writer (local PTY child, SSH channel, …) so the tmux *client*
    /// process exits and the reader thread sees EOF.
    /// The tmux *server* lives on with the session intact for reattach.
    cleanup: Mutex<Option<Cleanup>>,
}

struct CommandRequest {
    line: String,
    response: oneshot::Sender<Result<String, String>>,
}

impl TmuxClient {
    /// Drive an existing `tmux -CC` process over the given byte streams.
    ///
    /// The transport is opaque — phase 1 uses a local PTY pair, phase 2
    /// uses an SSH channel piped through `os_pipe`. The reader/writer
    /// threads only see `Read`/`Write` trait objects either way.
    ///
    /// `cleanup` runs on `Drop` and must terminate the remote tmux client
    /// so the reader thread sees EOF (kill the local process, drop the SSH
    /// channel, etc.).
    pub async fn spawn_with_io(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        cleanup: Cleanup,
        ready_timeout: std::time::Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), TmuxError> {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Notification>();
        let pending: PendingQueue = Arc::new(Mutex::new(VecDeque::new()));
        let (cmd_tx, cmd_rx) = sync_mpsc::channel::<CommandRequest>();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let ready: ReadySignal = Arc::new(Mutex::new(Some(ready_tx)));

        // Reader thread.
        thread::spawn({
            let pending = pending.clone();
            let ready = ready.clone();
            move || reader_loop(reader, event_tx, pending, ready)
        });
        // Writer thread.
        thread::spawn(move || writer_loop(writer, cmd_rx, pending));

        // Wait for tmux to settle. Format expansions like `#{session_name}`
        // return empty until the control client has a current target, which
        // happens when tmux emits `%session-changed`. Without this gate,
        // callers race tmux's startup and silently see empty results.
        match tokio::time::timeout(ready_timeout, ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                cleanup();
                return Err(TmuxError::Response("tmux ready signal dropped".into()));
            }
            Err(_) => {
                cleanup();
                return Err(TmuxError::Response(format!(
                    "tmux did not emit %session-changed within {}ms",
                    ready_timeout.as_millis()
                )));
            }
        }

        let client = Self {
            cmd_tx,
            cleanup: Mutex::new(Some(cleanup)),
        };

        // Keep our control client attached when its current session is
        // destroyed. Default tmux behaviour (`detach-on-destroy on`)
        // would kick us out the moment the user kills the last window
        // of the workspace they're in — even if other sessions exist
        // on the server. With `off`, tmux instead switches us to the
        // most-recently-active remaining session, fires
        // `%session-changed`, and the UI smoothly moves to it. Only a
        // truly empty server (no sessions left) detaches us, which is
        // exactly the "host disconnected" state we *do* want to show.
        //
        // Best effort — failure here doesn't break the connection,
        // we just inherit tmux's default behaviour.
        let _ = client
            .send_command("set-option -g detach-on-destroy off")
            .await;

        Ok((client, event_rx))
    }

    /// Spawn tmux in `-CC` mode inside a local PTY.
    ///
    /// We attach to whatever sessions are already running on the user's
    /// local tmux server; if there are none, we create a fresh session
    /// named `default_workspace`. Multi-workspace UI lives on top of the
    /// existing server's session list — helm doesn't claim ownership of
    /// "the" session.
    ///
    /// tmux requires a controlling TTY even in control mode (it calls
    /// `tcgetattr` at startup), so we always wrap it in `portable-pty`
    /// rather than plain pipes.
    ///
    /// Local startup measures consistently in the 10-50ms range; the 1s
    /// ready-gate is generous headroom without making startup feel sluggish
    /// if something goes catastrophically wrong.
    pub async fn spawn_local(
        default_workspace: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), TmuxError> {
        let tmux = find_tmux().ok_or_else(|| {
            TmuxError::Response(
                "tmux not found. Install it with `brew install tmux`.".to_string(),
            )
        })?;

        // Hygiene pass: reap any leaked `tmux -CC` client process from
        // a prior helm (or other control-mode app) that died without
        // cleanup. The leaked client's PTY master fd is owned by a
        // process that no longer exists, so its slave fd stays open
        // with a kernel buffer that never drains. Tmux's main loop
        // eventually blocks broadcasting output to that dead reader,
        // wedging every command for every attached client.
        //
        // Identifying orphans safely is the subtle part. A `tmux -CC`
        // command line with PPID=1 can be (a) an orphaned client we
        // want to kill, OR (b) the tmux server itself, which is a
        // daemon by design (fork+detach → PPID=1). Killing the server
        // would destroy every tmux session on the machine. So instead
        // of guessing, we ask tmux directly: server's pid via
        // `display-message`, attached clients via `list-clients`.
        // Anything matching `tmux -CC` in ps that *isn't* in that set
        // is a process tmux has lost track of — by definition, an
        // orphan.
        match probe_tmux(&tmux).await {
            TmuxProbe::Healthy(known) => {
                let reaped = kill_orphan_cc_clients(&known);
                if !reaped.is_empty() {
                    tracing::info!(
                        "reaped {} orphan tmux -CC client(s): {:?}",
                        reaped.len(),
                        reaped
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
            }
            // No server running yet — nothing to reap against. The
            // attach-or-create script below will spin a fresh server.
            TmuxProbe::NoServer => {}
            // Server is wedged (unresponsive within the timeout).
            // Surface to the UI rather than risk killing the server
            // by reaping with stale data.
            TmuxProbe::Wedged => {
                return Err(TmuxError::Response(
                    "tmux server is unresponsive. Run `tmux kill-server` \
                     manually if you don't have other tmux sessions you \
                     care about."
                        .into(),
                ));
            }
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TmuxError::Response(format!("openpty: {e}")))?;

        // Probe for existing sessions with a one-shot `list-sessions`
        // *before* we open the control client. This avoids a real race:
        // when the previous tmux server is mid-shutdown (e.g. user just
        // killed the last session), `tmux -CC attach` can briefly
        // "succeed" against the dying server, see EOF, then exit
        // cleanly — leaving the `||` fallback to never run and our
        // reader thread on the way to seeing EOF a heartbeat later. The
        // forwarder then posts `Disconnected` and clears `entry.tmux`,
        // so the next `tmux_*` call fails with "host not connected".
        //
        // `list-sessions` is a single round-trip request/response and
        // can't race a dying server the same way: either it gets data
        // back (server is alive with sessions), or it fails (server is
        // gone or empty). We branch deterministically off that.
        let tmux_path = quote_arg(tmux.to_string_lossy().as_ref());
        let ws = quote_arg(default_workspace);
        let script = format!(
            "if [ -n \"$({tmux_path} list-sessions -F '#{{session_id}}' 2>/dev/null)\" ]; then \
                exec {tmux_path} -CC attach; \
             else \
                exec {tmux_path} -CC new-session -A -s {ws}; \
             fi"
        );
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", &script]);
        // CommandBuilder starts with an empty environment by default; inherit
        // ours so subprocesses tmux launches (the shell, its tools) find what
        // they need.
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TmuxError::Response(format!("spawn tmux: {e}")))?;
        // Drop the slave so the master sees EOF when tmux exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TmuxError::Response(format!("reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TmuxError::Response(format!("writer: {e}")))?;
        let mut killer = child.clone_killer();

        // Wait thread — purely informational.
        thread::spawn(move || {
            let _ = child.wait();
            debug!("tmux process exited");
        });

        let cleanup: Cleanup = Box::new(move || {
            let _ = killer.kill();
        });

        Self::spawn_with_io(reader, writer, cleanup, std::time::Duration::from_secs(1)).await
    }

    /// Send a tmux command and await its full response (joined with newlines).
    pub async fn send_command(&self, command: impl Into<String>) -> Result<String, TmuxError> {
        let (tx, rx) = oneshot::channel();
        let req = CommandRequest {
            line: command.into(),
            response: tx,
        };
        self.cmd_tx
            .send(req)
            .map_err(|_| TmuxError::UnexpectedExit)?;
        match rx.await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => Err(TmuxError::Response(e)),
            Err(_) => Err(TmuxError::UnexpectedExit),
        }
    }

    // ---------- High-level convenience wrappers ----------

    pub async fn send_keys(&self, pane_id: &str, bytes: &[u8]) -> Result<(), TmuxError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let hex: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        let cmd = format!("send-keys -t {} -H {}", pane_id, hex.join(" "));
        self.send_command(cmd).await.map(|_| ())
    }

    pub async fn resize_pane(
        &self,
        pane_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), TmuxError> {
        self.send_command(format!(
            "resize-pane -t {} -x {} -y {}",
            pane_id, cols, rows
        ))
        .await
        .map(|_| ())
    }

    /// Create a new window. If `session_id` is supplied, the window is
    /// created *in that session* (`-t $X`); otherwise tmux uses the control
    /// client's current session. Multi-workspace callers should always
    /// pass an explicit target so the new window doesn't surprise them.
    pub async fn new_window(
        &self,
        session_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<(), TmuxError> {
        let mut parts = vec!["new-window".to_string()];
        if let Some(s) = session_id {
            parts.push("-t".to_string());
            parts.push(s.to_string());
        }
        if let Some(n) = name {
            parts.push("-n".to_string());
            parts.push(quote_arg(n));
        }
        self.send_command(parts.join(" ")).await.map(|_| ())
    }

    pub async fn split_pane(&self, pane_id: &str, vertical: bool) -> Result<(), TmuxError> {
        let dir = if vertical { "-v" } else { "-h" };
        self.send_command(format!("split-window {} -t {}", dir, pane_id))
            .await
            .map(|_| ())
    }

    pub async fn kill_window(&self, window_id: &str) -> Result<(), TmuxError> {
        self.send_command(format!("kill-window -t {}", window_id))
            .await
            .map(|_| ())
    }

    pub async fn select_window(&self, window_id: &str) -> Result<(), TmuxError> {
        self.send_command(format!("select-window -t {}", window_id))
            .await
            .map(|_| ())
    }

    pub async fn select_pane(&self, pane_id: &str) -> Result<(), TmuxError> {
        self.send_command(format!("select-pane -t {}", pane_id))
            .await
            .map(|_| ())
    }

    pub async fn rename_window(&self, window_id: &str, name: &str) -> Result<(), TmuxError> {
        // tmux's `automatic-rename` (default on) re-derives the window name
        // from `pane_current_command` on every prompt redraw, which stomps
        // any manual rename we just applied. Turn it off for this window
        // before renaming so the user's choice sticks.
        self.send_command(format!(
            "set-window-option -t {} automatic-rename off",
            window_id
        ))
        .await
        .map(|_| ())?;
        self.send_command(format!(
            "rename-window -t {} {}",
            window_id,
            quote_arg(name)
        ))
        .await
        .map(|_| ())
    }

    pub async fn list_windows(&self, format: &str) -> Result<String, TmuxError> {
        // `-a` so we see windows in *every* session on the server, not just
        // the control client's current one. Multi-workspace UI depends on this.
        self.send_command(format!("list-windows -a -F {}", quote_arg(format)))
            .await
    }

    pub async fn list_panes(&self, format: &str) -> Result<String, TmuxError> {
        self.send_command(format!("list-panes -a -F {}", quote_arg(format)))
            .await
    }

    pub async fn list_sessions(&self, format: &str) -> Result<String, TmuxError> {
        self.send_command(format!("list-sessions -F {}", quote_arg(format)))
            .await
    }

    /// Create a new detached session. tmux auto-names if `name` is None;
    /// otherwise we pass `-s <name>`. `-d` keeps it from yanking our
    /// control client's current session. `-P -F #{session_id}` echoes the
    /// new session id so the caller can target it immediately.
    pub async fn new_session(&self, name: Option<&str>) -> Result<String, TmuxError> {
        let cmd = match name {
            Some(n) => format!(
                "new-session -d -P -F '#{{session_id}}' -s {}",
                quote_arg(n)
            ),
            None => "new-session -d -P -F '#{session_id}'".to_string(),
        };
        let out = self.send_command(cmd).await?;
        Ok(out.trim().to_string())
    }

    pub async fn kill_session(&self, session_id: &str) -> Result<(), TmuxError> {
        self.send_command(format!("kill-session -t {}", session_id))
            .await
            .map(|_| ())
    }

    /// Switch the control client's current session to `session_id`.
    ///
    /// Why we care: tmux only resizes a session's panes when a client is
    /// attached to it. A session created with `new-session -d` (detached)
    /// keeps tmux's default 80×24 sizing until something attaches. When
    /// the helm UI selects that session as the active workspace, the
    /// xterm is sized to the user's actual viewport (e.g. 200×50), and
    /// rendering desyncs — cursor lands one row off, output goes to dead
    /// columns, etc. Calling `switch-client` makes the new session our
    /// client's current target; tmux then resizes it to match our
    /// `refresh-client -C` viewport.
    pub async fn switch_client(&self, session_id: &str) -> Result<(), TmuxError> {
        self.send_command(format!("switch-client -t {}", session_id))
            .await
            .map(|_| ())
    }

    pub async fn rename_session(&self, session_id: &str, name: &str) -> Result<(), TmuxError> {
        self.send_command(format!(
            "rename-session -t {} {}",
            session_id,
            quote_arg(name)
        ))
        .await
        .map(|_| ())
    }

    /// Capture the current buffer of a pane *with* escape sequences
    /// preserved (`-e`), so colours transfer to xterm. `-J` joins wrapped
    /// lines so column-count reflows look right.
    ///
    /// `scrollback_lines` controls how much history is included:
    /// - `0` → visible buffer only (cheapest; fits in a few KB).
    /// - `n > 0` → `-S -n`, i.e. the last `n` lines including scrollback
    ///   history. `2000` is a sane upper bound for interactive pane
    ///   "scroll up to read" — matches tmux's default `history-limit`.
    ///
    /// We don't expose the unbounded `-S -` form because it can pull
    /// multi-megabyte responses on long-lived panes (one screenful of
    /// densely-coloured output can be 10 KB on the wire), which over
    /// SSH translates to multi-second fetches per pane.
    ///
    /// We append a cursor-positioning escape to the response — capture-pane
    /// only dumps row contents, leaving xterm's cursor at the bottom of the
    /// last written row. tmux knows the *logical* cursor position (where
    /// the shell expects new input); we query it via `display-message` and
    /// emit a CSI `H` to move xterm's cursor there.
    pub async fn capture_pane(
        &self,
        pane_id: &str,
        scrollback_lines: u32,
    ) -> Result<String, TmuxError> {
        let scope = if scrollback_lines > 0 {
            format!("-S -{scrollback_lines} ")
        } else {
            String::new()
        };
        let capture = self
            .send_command(format!("capture-pane -p -e -J {}-t {}", scope, pane_id))
            .await?;

        // Best-effort cursor restoration. If the format response is malformed
        // we just skip the escape and let xterm sit wherever capture left it.
        let cursor = self
            .send_command(format!(
                "display-message -p -t {} '#{{cursor_x}},#{{cursor_y}}'",
                pane_id
            ))
            .await
            .ok()
            .and_then(|resp| {
                let trimmed = resp.trim();
                let (x, y) = trimmed.split_once(',')?;
                let x: u16 = x.trim().parse().ok()?;
                let y: u16 = y.trim().parse().ok()?;
                // CSI H is 1-indexed; tmux's cursor_{x,y} are 0-indexed.
                Some(format!("\x1b[{};{}H", y + 1, x + 1))
            })
            .unwrap_or_default();

        Ok(format!("{capture}{cursor}"))
    }

    /// Resize the *control client* to the given dimensions. For control
    /// clients (`-CC`), tmux interprets this as "the visible terminal is now
    /// this big" and resizes the active session/window accordingly, sending
    /// SIGWINCH to all panes so shells redraw at the new width.
    /// Without this, the session stays at the openpty default (80×24) and
    /// the rendered prompt ends up squashed against the left edge of a
    /// much wider xterm.
    pub async fn resize_client(&self, cols: u16, rows: u16) -> Result<(), TmuxError> {
        self.send_command(format!("refresh-client -C {}x{}", cols, rows))
            .await
            .map(|_| ())
    }
}

impl Drop for TmuxClient {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.lock().take() {
            cleanup();
        }
    }
}

fn reader_loop(
    reader: Box<dyn Read + Send>,
    event_tx: mpsc::UnboundedSender<Notification>,
    pending: PendingQueue,
    ready: ReadySignal,
) {
    let mut buf_reader = BufReader::new(reader);
    let mut line_bytes: Vec<u8> = Vec::new();
    let mut in_block = false;
    // Block data is collected as raw bytes so we can run tmux's octal
    // escape decoder once at the terminator. tmux emits `\011` for tab,
    // `\xxx` for other control chars, and `\\` for backslash inside
    // command-response blocks — without decoding, the tab-delimited
    // formats we use for `list-windows`/`list-panes` come back as a
    // single garbled line.
    let mut block_bytes: Vec<u8> = Vec::new();

    loop {
        line_bytes.clear();
        // `read_line` was attractive but it deserialises into a `String` and
        // panics on invalid UTF-8 — which happens whenever tmux's output
        // buffer splits a multi-byte sequence at the boundary (TUIs like
        // claude that paint full-screen at 60fps do this regularly).
        // `read_until` reads raw bytes; we lossy-decode for the parser, which
        // means partial sequences become U+FFFD instead of crashing the
        // reader. Phase 5's binary IPC will let us pass bytes through more
        // faithfully.
        match buf_reader.read_until(b'\n', &mut line_bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                debug!("tmux read error: {e}");
                break;
            }
        }

        // Strip trailing CR/LF in the byte domain before any string conversion.
        let mut end = line_bytes.len();
        while end > 0 && (line_bytes[end - 1] == b'\n' || line_bytes[end - 1] == b'\r') {
            end -= 1;
        }
        let raw = &line_bytes[..end];

        // `%output`'s data section can carry raw UTF-8 / arbitrary bytes that
        // don't survive lossy String conversion across chunk boundaries
        // (tmux splitting `\xE2\x94\x80` between chunks turns `─` into `��`).
        // Parse from raw bytes so xterm.js receives the original sequence.
        if raw.starts_with(b"%output ") {
            let n = parse_output_bytes(&raw[b"%output ".len()..]);
            if matches!(&n, Notification::SessionChanged { .. }) {
                if let Some(tx) = ready.lock().take() {
                    let _ = tx.send(());
                }
            }
            if event_tx.send(n).is_err() {
                break;
            }
            continue;
        }

        // Other lines are protocol ASCII (block markers, simple notifications).
        // Lossy decode is safe — any non-ASCII would itself be a protocol bug.
        let line = String::from_utf8_lossy(raw);
        let trimmed: &str = &line;

        if in_block {
            // Inside a command response, the ONLY legal `%`-prefixed lines are
            // `%end` and `%error` (block terminators). Anything else — even
            // lines that look like notifications such as `%0` (a pane id from
            // list-panes) — is plain response data.
            if is_block_terminator(trimmed, "%end") {
                in_block = false;
                if let Some(tx) = pending.lock().pop_front() {
                    let decoded = decode_octal(&block_bytes);
                    let _ = tx.send(Ok(String::from_utf8_lossy(&decoded).into_owned()));
                } else {
                    warn!("tmux: %end with no pending command");
                }
                block_bytes.clear();
            } else if is_block_terminator(trimmed, "%error") {
                in_block = false;
                if let Some(tx) = pending.lock().pop_front() {
                    let decoded = decode_octal(&block_bytes);
                    let _ = tx.send(Err(String::from_utf8_lossy(&decoded).into_owned()));
                } else {
                    warn!("tmux: %error with no pending command");
                }
                block_bytes.clear();
            } else {
                if !block_bytes.is_empty() {
                    block_bytes.push(b'\n');
                }
                block_bytes.extend_from_slice(raw);
            }
            continue;
        }

        // Outside a block: only %begin and %notifications are meaningful.
        match parse_line(trimmed) {
            TmuxLine::Begin { .. } => {
                in_block = true;
                block_bytes.clear();
            }
            TmuxLine::Notification(n) => {
                if matches!(&n, Notification::SessionChanged { .. }) {
                    if let Some(tx) = ready.lock().take() {
                        let _ = tx.send(());
                    }
                }
                if event_tx.send(n).is_err() {
                    break;
                }
            }
            TmuxLine::Data(_)
            | TmuxLine::End { .. }
            | TmuxLine::ResponseError { .. } => {
                // Banner / orphaned terminators outside a block — ignore.
            }
        }
    }
    debug!("tmux reader loop exiting");
}

fn is_block_terminator(line: &str, kind: &str) -> bool {
    line == kind || line.starts_with(&format!("{kind} "))
}

fn writer_loop(
    mut writer: Box<dyn Write + Send>,
    cmd_rx: sync_mpsc::Receiver<CommandRequest>,
    pending: PendingQueue,
) {
    while let Ok(req) = cmd_rx.recv() {
        pending.lock().push_back(req.response);

        let line = req.line;
        if writer.write_all(line.as_bytes()).is_err() {
            break;
        }
        if !line.ends_with('\n') && writer.write_all(b"\n").is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
    }
    debug!("tmux writer loop exiting");
}

/// Locate the tmux binary. Tauri-launched processes inherit a minimal PATH
/// that omits Homebrew, so we probe the usual install locations directly,
/// then fall back to whatever `PATH` the parent gave us.
fn find_tmux() -> Option<std::path::PathBuf> {
    const CANDIDATES: &[&str] = &[
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ];
    for c in CANDIDATES {
        let p = std::path::PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = std::path::PathBuf::from(dir).join("tmux");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Three-state classification of the local tmux server's reachability.
enum TmuxProbe {
    /// Server is responsive. Carries the set of PIDs tmux considers
    /// live: the server itself plus every attached client. These are
    /// the PIDs the orphan reaper must preserve.
    Healthy(std::collections::HashSet<u32>),
    /// No tmux server is running. Nothing to reap against; the next
    /// `tmux -CC new-session` will start one cleanly.
    NoServer,
    /// Server exists but doesn't respond within the timeout — the
    /// classic "broadcast queue blocked on a dead reader" signature.
    /// We can't ask it which PIDs are legitimate, so we refuse to
    /// reap and surface the failure to the UI.
    Wedged,
}

/// Probe the local tmux server with a short timeout, returning
/// healthy/no-server/wedged. Spawns one or two short-lived `tmux`
/// commands; never opens a persistent control client.
async fn probe_tmux(tmux: &std::path::Path) -> TmuxProbe {
    use std::collections::HashSet;
    use std::process::Stdio;

    // Server PID — also serves as our healthy-vs-wedged probe. A
    // missing server exits non-zero quickly; a healthy server
    // returns its pid quickly; a wedged server returns nothing
    // before the timeout fires.
    let server_query = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::process::Command::new(tmux)
            .args(["display-message", "-p", "#{pid}"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await;
    let server_pid = match server_query {
        Ok(Ok(out)) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().ok()
        }
        Ok(_) => return TmuxProbe::NoServer,
        Err(_) => return TmuxProbe::Wedged,
    };
    let Some(server_pid) = server_pid else {
        return TmuxProbe::NoServer;
    };

    let mut known = HashSet::new();
    known.insert(server_pid);

    // Attached clients. Best-effort: if list-clients hangs or fails
    // we still have the server pid, which is the critical exclusion.
    if let Ok(Ok(out)) = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::process::Command::new(tmux)
            .args(["list-clients", "-F", "#{client_pid}"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                known.insert(pid);
            }
        }
    }

    TmuxProbe::Healthy(known)
}

/// Find and SIGTERM any `tmux -CC` client process that tmux doesn't
/// recognize as one of its known clients (or the server itself).
/// `known_to_tmux` is the set of PIDs we must preserve — populated
/// from `collect_tmux_known_pids`.
///
/// Concretely, anything matching `tmux -CC` in `ps` that is *not* in
/// `known_to_tmux` is a leaked client whose owning app died without
/// cleanup; tmux has lost track of it and its leaked PTY master fd
/// will eventually wedge the server's broadcast queue.
///
/// Conservative on purpose:
///   - Only matches the literal string "-CC" so plain `tmux` clients
///     and unrelated tools aren't touched.
///   - Skips ourselves.
///   - Skips anything tmux still considers live (a server, or a
///     client that's actually attached and known).
///   - SIGTERM, never SIGKILL — `tmux -CC` exits cleanly on TERM.
fn kill_orphan_cc_clients(known_to_tmux: &std::collections::HashSet<u32>) -> Vec<u32> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let our_pid = std::process::id();
    let mut killed = Vec::new();
    for line in stdout.lines() {
        // `ps -axo pid=,command=` right-aligns the pid with leading
        // spaces, so `split_whitespace` (which collapses runs) is the
        // safe way to grab the first field.
        let mut iter = line.split_whitespace();
        let Some(pid_s) = iter.next() else { continue };
        let cmd = iter.collect::<Vec<_>>().join(" ");
        let Ok(pid) = pid_s.parse::<u32>() else { continue };
        if pid == our_pid {
            continue;
        }
        // Match `tmux -CC` somewhere in the command line. Both
        // `-CC attach` and `-CC new-session ...` are accepted; plain
        // `tmux` use isn't.
        if !cmd.contains("tmux") || !cmd.contains("-CC") {
            continue;
        }
        // The crucial safety check: if tmux thinks this PID is alive
        // (a known client, or the server itself), skip. Anything left
        // is a process tmux has lost track of — i.e. an orphan that
        // tmux's broadcast loop will eventually wedge against.
        if known_to_tmux.contains(&pid) {
            continue;
        }
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        killed.push(pid);
    }
    killed
}

/// Wrap an argument in single quotes, escaping any single quote inside as `'\''`.
/// tmux's command parser accepts shell-style single-quoted strings.
fn quote_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

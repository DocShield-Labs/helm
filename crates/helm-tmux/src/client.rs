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

        // Track per-window bell flags server-side so we can backfill
        // "unread while I was disconnected" on (re)connect by reading
        // `#{window_bell_flag}`. `monitor-bell` is tmux's default, but we
        // set it explicitly so the feature doesn't depend on the user's
        // config. Bells only — `monitor-activity` would flag any
        // background output and flood the inbox with false unreads.
        let _ = client
            .send_command("set-option -g monitor-bell on")
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
    /// One-shot bootstrap of the local tmux server: find binary, probe +
    /// reap orphans, ensure at least one session exists, return every
    /// session id on the server.
    ///
    /// Synchronous (no async) — uses one-shot subprocess invocations,
    /// no control-mode client. Caller wraps in `tokio::task::spawn_blocking`
    /// when running from async context.
    ///
    /// Returns a Vec of `#{session_id}` strings (e.g. `["$0", "$3"]`).
    /// First entry is the most recently active session — caller can use
    /// it as the "primary" for command routing.
    pub fn bootstrap_local(default_workspace: &str) -> Result<Vec<String>, TmuxError> {
        let tmux = find_tmux().ok_or_else(|| {
            TmuxError::Response(
                "tmux not found. Install it with `brew install tmux`.".to_string(),
            )
        })?;

        // Hygiene pass: reap any leaked `tmux -CC` client process from
        // a prior helm (or other control-mode app) that died without
        // cleanup. See the docstring in the prior single-client version
        // (commit history) for the full reasoning — short version: a
        // leaked `-CC` client's PTY can wedge the entire tmux server's
        // command queue, and we identify orphans by asking tmux which
        // pids it considers attached, NOT by guessing from process
        // attributes (which would risk killing the server itself).
        match probe_tmux_sync(&tmux) {
            TmuxProbe::Healthy(known) => {
                let reaped = kill_orphan_cc_clients(&known);
                if !reaped.is_empty() {
                    tracing::info!(
                        "reaped {} orphan tmux -CC client(s): {:?}",
                        reaped.len(),
                        reaped
                    );
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
            }
            TmuxProbe::NoServer => {}
        }

        // Enumerate sessions ordered by most-recent activity. tmux
        // sorts list-sessions by `#{session_activity}` descending by
        // default; we keep that order so the caller's "primary" pick
        // matches the user's intuition (the session they were last in).
        let sessions = list_local_sessions(&tmux)?;
        if !sessions.is_empty() {
            return Ok(sessions);
        }

        // Empty server — create the bootstrap session detached. Capture
        // its session id with `-P -F '#{session_id}'`.
        let out = std::process::Command::new(&tmux)
            .args([
                "new-session",
                "-d",
                "-s",
                default_workspace,
                "-P",
                "-F",
                "#{session_id}",
            ])
            .output()
            .map_err(|e| TmuxError::Response(format!("new-session: {e}")))?;
        if !out.status.success() {
            return Err(TmuxError::Response(format!(
                "new-session failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            return Err(TmuxError::Response(
                "new-session printed no session id".into(),
            ));
        }
        Ok(vec![id])
    }

    /// Spawn a control-mode client attached to a specific local session.
    /// Each call opens a fresh PTY pair and runs `tmux -CC attach -t
    /// $session_id`, wrapping the streams in a TmuxClient.
    ///
    /// One client per session is the basis of helm's multi-client
    /// architecture: tmux only forwards `%output` for the session a
    /// control client is attached to, so we maintain N clients for N
    /// sessions and they all stream output in real time.
    pub async fn spawn_attach_local(
        session_id: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), TmuxError> {
        let tmux = find_tmux().ok_or_else(|| {
            TmuxError::Response(
                "tmux not found. Install it with `brew install tmux`.".to_string(),
            )
        })?;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TmuxError::Response(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(tmux);
        cmd.args(["-CC", "attach", "-t", session_id]);
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TmuxError::Response(format!("spawn tmux: {e}")))?;
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

        let session_id_owned = session_id.to_string();
        thread::spawn(move || {
            let _ = child.wait();
            debug!("tmux -CC attach process exited (session {session_id_owned:?})");
        });

        let cleanup: Cleanup = Box::new(move || {
            let _ = killer.kill();
        });

        Self::spawn_with_io(reader, writer, cleanup, std::time::Duration::from_secs(1)).await
    }

    /// Legacy single-client connect for tests + the main helm-app path
    /// pre-multi-client. Bootstraps the server (creating
    /// `default_workspace` if no sessions exist) and attaches a single
    /// control client to the first session returned. New helm-app code
    /// should call `bootstrap_local` + `spawn_attach_local` directly.
    pub async fn spawn_local(
        default_workspace: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), TmuxError> {
        let dw = default_workspace.to_string();
        let sessions = tokio::task::spawn_blocking(move || Self::bootstrap_local(&dw))
            .await
            .map_err(|e| TmuxError::Response(format!("bootstrap join: {e}")))??;
        let primary = sessions
            .into_iter()
            .next()
            .ok_or_else(|| TmuxError::Response("bootstrap returned no sessions".into()))?;
        Self::spawn_attach_local(&primary).await
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
    ///
    /// `start_dir` becomes the new window's `-c` argument — accepts both
    /// literal paths and tmux format strings like `#{E:HOME}`, which
    /// tmux expands server-side from its environment.
    pub async fn new_window(
        &self,
        session_id: Option<&str>,
        name: Option<&str>,
        start_dir: Option<&str>,
    ) -> Result<(), TmuxError> {
        let mut parts = vec!["new-window".to_string()];
        if let Some(s) = session_id {
            parts.push("-t".to_string());
            parts.push(s.to_string());
        }
        if let Some(c) = start_dir {
            parts.push("-c".to_string());
            parts.push(quote_arg(c));
        }
        if let Some(n) = name {
            parts.push("-n".to_string());
            parts.push(quote_arg(n));
        }
        self.send_command(parts.join(" ")).await.map(|_| ())
    }

    /// Open a new window whose pane runs `command` as its initial
    /// process (via tmux's positional `shell-command` argument).
    /// Returns the new window id via `-P -F '#{window_id}'` so the
    /// caller can target follow-up commands at it. `--` separates
    /// flags from the shell-command so a command that begins with
    /// `-` isn't misparsed as a flag.
    pub async fn new_window_with_command_returning_id(
        &self,
        session_id: Option<&str>,
        name: Option<&str>,
        start_dir: Option<&str>,
        command: &str,
    ) -> Result<String, TmuxError> {
        let mut parts: Vec<String> = vec![
            "new-window".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "'#{window_id}'".to_string(),
        ];
        if let Some(s) = session_id {
            parts.push("-t".to_string());
            parts.push(s.to_string());
        }
        if let Some(c) = start_dir {
            parts.push("-c".to_string());
            parts.push(quote_arg(c));
        }
        if let Some(n) = name {
            parts.push("-n".to_string());
            parts.push(quote_arg(n));
        }
        parts.push("--".to_string());
        parts.push(quote_arg(command));
        let out = self.send_command(parts.join(" ")).await?;
        Ok(out.trim().to_string())
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
    pub async fn new_session(
        &self,
        name: Option<&str>,
        start_dir: Option<&str>,
    ) -> Result<String, TmuxError> {
        let mut parts: Vec<String> = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "'#{session_id}'".to_string(),
        ];
        if let Some(c) = start_dir {
            parts.push("-c".to_string());
            parts.push(quote_arg(c));
        }
        if let Some(n) = name {
            parts.push("-s".to_string());
            parts.push(quote_arg(n));
        }
        let out = self.send_command(parts.join(" ")).await?;
        Ok(out.trim().to_string())
    }

    /// Like `new_session` but additionally takes the first window's
    /// name and a `shell-command` to run as that window's initial
    /// process. Returns the new window's id (not the session's — the
    /// scheduler routes follow-ups to the window). See the rationale
    /// on `new_window_with_command_returning_id` for why this exists
    /// (canonical-mode `MAX_CANON` avoidance).
    pub async fn new_session_with_command_returning_window_id(
        &self,
        session_name: &str,
        window_name: &str,
        start_dir: &str,
        command: &str,
    ) -> Result<String, TmuxError> {
        let parts: Vec<String> = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "'#{window_id}'".to_string(),
            "-s".to_string(),
            quote_arg(session_name),
            "-n".to_string(),
            quote_arg(window_name),
            "-c".to_string(),
            quote_arg(start_dir),
            "--".to_string(),
            quote_arg(command),
        ];
        let out = self.send_command(parts.join(" ")).await?;
        Ok(out.trim().to_string())
    }

    /// Write `content` to `path` on the tmux server's host via tmux's
    /// paste-buffer API: `set-buffer` → `save-buffer` → `delete-buffer`.
    /// Rides the existing tmux control channel — zero new SSH channels,
    /// so this is safe to call from callers (e.g. the scheduler) that
    /// would otherwise exhaust the SSH server's `MaxSessions` budget.
    /// `buffer_name` must be unique per concurrent caller.
    pub async fn write_file_via_buffer(
        &self,
        buffer_name: &str,
        path: &str,
        content: &str,
    ) -> Result<(), TmuxError> {
        self.send_command(format!(
            "set-buffer -b {} {}",
            quote_arg(buffer_name),
            quote_arg(content)
        ))
        .await?;
        self.send_command(format!(
            "save-buffer -b {} {}",
            quote_arg(buffer_name),
            quote_arg(path)
        ))
        .await?;
        // Best-effort cleanup; if delete fails the buffer leaks until
        // the tmux server exits, which is harmless.
        let _ = self
            .send_command(format!("delete-buffer -b {}", quote_arg(buffer_name)))
            .await;
        Ok(())
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

        // Reset the SGR pen after the snapshot. `capture-pane -e` emits
        // colors inline per cell but doesn't guarantee a trailing reset,
        // so the dump can leave xterm's *current* pen in whatever color
        // the last captured cell used (e.g. blue in Claude's UI, a dim
        // gray at a shell). Without this, the first characters the user
        // types echo in that stale color until the program redraws —
        // the "random gray/blue text on a new pane" glitch. The already
        // rendered cells keep their inline colors; only the pen resets.
        Ok(format!("{capture}\x1b[0m{cursor}"))
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

/// Two-state classification of the local tmux server's reachability.
/// The pre-multi-client async probe also distinguished a `Wedged`
/// state (server exists but unresponsive within a timeout); the sync
/// probe used by the bootstrap path doesn't currently have timeout
/// machinery, so a wedged server appears as `NoServer` until the
/// underlying `tmux` invocation eventually returns. Re-add a Wedged
/// variant + sync timeout if that becomes a real failure mode.
enum TmuxProbe {
    /// Server is responsive. Carries the set of PIDs tmux considers
    /// live: the server itself plus every attached client. These are
    /// the PIDs the orphan reaper must preserve.
    Healthy(std::collections::HashSet<u32>),
    /// No tmux server is running. Nothing to reap against; the next
    /// `tmux -CC new-session` will start one cleanly.
    NoServer,
}

/// Probe the local tmux server. The bootstrap path runs from a
/// `spawn_blocking` context that doesn't have a tokio reactor handy
/// for `tokio::process::Command`. Same logic — short-timed
/// `display-message` + `list-clients` — using std::process and a
/// thread-based timeout via `wait_timeout` not available in std, so
/// we approximate with a quick blocking call (tmux returns these in
/// microseconds when healthy).
fn probe_tmux_sync(tmux: &std::path::Path) -> TmuxProbe {
    use std::collections::HashSet;
    use std::process::Stdio;

    let server_query = std::process::Command::new(tmux)
        .args(["display-message", "-p", "#{pid}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let server_pid = match server_query {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().ok()
        }
        Ok(_) => return TmuxProbe::NoServer,
        Err(_) => return TmuxProbe::NoServer,
    };
    let Some(server_pid) = server_pid else {
        return TmuxProbe::NoServer;
    };

    let mut known = HashSet::new();
    known.insert(server_pid);

    if let Ok(out) = std::process::Command::new(tmux)
        .args(["list-clients", "-F", "#{client_pid}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                known.insert(pid);
            }
        }
    }

    TmuxProbe::Healthy(known)
}

/// Enumerate session ids on the local server, ordered most-recent
/// first (tmux's default for `list-sessions`). Empty vec when the
/// server has no sessions; error when tmux itself is unreachable.
fn list_local_sessions(tmux: &std::path::Path) -> Result<Vec<String>, TmuxError> {
    let out = std::process::Command::new(tmux)
        .args(["list-sessions", "-F", "#{session_id}"])
        .output()
        .map_err(|e| TmuxError::Response(format!("list-sessions: {e}")))?;
    if !out.status.success() {
        // Common case: "no server running" → empty list, not an error.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no server running") || stderr.contains("error connecting") {
            return Ok(vec![]);
        }
        return Err(TmuxError::Response(format!(
            "list-sessions failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|s| s.to_string())
        .collect())
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

/// POSIX shell single-quote escape: wrap in `'…'`, replace any inner
/// `'` with `'\''` (close, escaped quote, reopen). Round-trips
/// through tmux's command parser AND through `/bin/sh -c`, so
/// callers in helm-app re-use it for any place a value needs to
/// survive a shell parser intact.
pub fn quote_arg(s: &str) -> String {
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

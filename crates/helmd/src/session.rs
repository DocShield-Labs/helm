//! A session: one PTY, one child process, and one terminal model.
//!
//! The reader thread is the single producer for a session's state: raw
//! PTY bytes → `StreamParser` (strips OSC 133 / bells / OSC 9, detects
//! alt-screen) → `SessionScreen` (the VT model; rows scroll into history)
//! → `SessionEvent`s to the daemon core. Markers are positioned by feeding
//! the model up to each one and reading the cursor's absolute line, so
//! block boundaries and rows can never disagree.

use std::io::Read;
use std::sync::Arc;

use parking_lot::Mutex;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc::UnboundedSender;

use helm_proto::{BlockMeta, SessionId};

use crate::markers::{IngestEvent, StreamParser};
use crate::screen::{SessionScreen, SharedWriter};

/// Bytes per PTY read — also the most lines one feed can scroll out,
/// which bounds the model's own scrollback (see `screen.rs`).
pub const READ_BUF_LEN: usize = 8192;

/// Events flowing from reader/wait threads into the daemon core.
#[derive(Debug)]
pub enum SessionEvent {
    /// Semantic event at an absolute line.
    Ingest {
        session: SessionId,
        line: u64,
        event: IngestEvent,
    },
    /// The model changed; the daemon schedules a flush.
    Dirty { session: SessionId },
    /// Child exited (or the PTY hit EOF).
    Exited {
        session: SessionId,
        status: Option<i32>,
    },
}

/// Mutable session metadata maintained by the daemon core.
#[derive(Debug, Default)]
pub struct SessionMeta {
    pub cols: u16,
    pub rows: u16,
    pub alt_screen: bool,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Git toplevel of `cwd`, from the last prompt; `None` outside a repo.
    pub root: Option<String>,
    /// argv[0] basename of what was spawned ("zsh", "claude", …).
    pub command: Option<String>,
    pub blocks: Vec<BlockMeta>,
    /// Index into `blocks` of the block still awaiting its `D` marker.
    pub open_block: Option<usize>,
    pub next_block_id: u64,
    pub exited: bool,
}

pub struct Session {
    pub id: SessionId,
    pub name: Mutex<String>,
    pub screen: Mutex<SessionScreen>,
    pub meta: Mutex<SessionMeta>,
    writer: SharedWriter,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

pub struct SpawnSpec {
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    /// argv to exec; `None` = the user's login shell.
    pub command: Option<Vec<String>>,
    /// Extra environment (integration vars) layered on the session's base
    /// env — see `crate::env` for what that base is and why it is not
    /// the daemon's own environment.
    pub env: Vec<(String, String)>,
}

impl Session {
    pub fn spawn(
        id: SessionId,
        spec: &SpawnSpec,
        events: UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<Arc<Session>> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows: spec.rows.max(2),
            cols: spec.cols.max(2),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = match &spec.command {
            Some(argv) if !argv.is_empty() => {
                let mut c = CommandBuilder::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
            _ => CommandBuilder::new_default_prog(),
        };
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        } else if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
        }
        // Start from a fixed base, not from whatever launched helmd —
        // a session must look the same whether the app came from the Dock,
        // an `open` inside tmux, or a dev build inside Helm. The
        // user's dotfiles build the rest, as in any other terminal.
        cmd.env_clear();
        for (k, v) in crate::env::session_env(std::env::vars()) {
            cmd.env(k, v);
        }
        // The session's tty path, inherited by every process in the session.
        // Tools that run hooks with no controlling terminal (Claude
        // Code) write their BEL here and helmd sees it on this PTY —
        // works for explicit-command sessions too, which never source a
        // shell integration script.
        if let Some(tty) = pair.master.as_raw_fd().and_then(slave_tty_name) {
            cmd.env("HELM_TTY", tty);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = pair.slave.spawn_command(cmd)?;
        // Close our copy of the slave so PTY EOF propagates when the
        // child exits (otherwise the reader blocks forever).
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let killer = child.clone_killer();

        let command_name = spec.command.as_ref().and_then(|argv| {
            argv.first().map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone())
            })
        });

        let session = Arc::new(Session {
            id,
            name: Mutex::new(String::new()),
            screen: Mutex::new(SessionScreen::new(spec.cols, spec.rows, writer.clone())),
            meta: Mutex::new(SessionMeta {
                cols: spec.cols,
                rows: spec.rows,
                command: command_name,
                cwd: spec.cwd.clone(),
                ..Default::default()
            }),
            writer,
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
        });

        // Reader thread: PTY → parser → model → events.
        {
            let session = session.clone();
            let events = events.clone();
            std::thread::Builder::new()
                .name(format!("session-{id}-read"))
                .spawn(move || reader_loop(session, reader, events))?;
        }

        // Wait thread: child exit status.
        {
            let events = events.clone();
            std::thread::Builder::new()
                .name(format!("session-{id}-wait"))
                .spawn(move || {
                    let status = child.wait().ok().map(|s| s.exit_code() as i32);
                    let _ = events.send(SessionEvent::Exited {
                        session: id,
                        status,
                    });
                })?;
        }

        Ok(session)
    }

    /// Write client keystrokes to the PTY. Raw bytes, no translation.
    pub fn input(&self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut w = self.writer.lock();
        w.write_all(bytes)?;
        w.flush()
    }

    /// Last-writer-wins resize (no tmux-style union across clients).
    /// The model resizes with the PTY so its reflow matches what the
    /// application will redraw into.
    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master.lock().resize(PtySize {
            rows: rows.max(2),
            cols: cols.max(2),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.screen.lock().resize(cols, rows);
        let mut meta = self.meta.lock();
        meta.cols = cols;
        meta.rows = rows;
        Ok(())
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().kill();
    }
}

/// `ptsname(3)` for a master fd. The libc call returns a static buffer,
/// so serialize it; spawns are rare.
fn slave_tty_name(fd: std::os::unix::io::RawFd) -> Option<String> {
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _g = LOCK.lock();
    // SAFETY: fd is a live PTY master owned by `pair.master` for the
    // duration of the call; ptsname returns NULL or a NUL-terminated
    // static string we copy out under the lock.
    unsafe {
        let p = libc::ptsname(fd);
        if p.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

fn reader_loop(
    session: Arc<Session>,
    mut reader: Box<dyn Read + Send>,
    events: UnboundedSender<SessionEvent>,
) {
    let mut parser = StreamParser::new();
    let mut buf = [0u8; READ_BUF_LEN];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break, // EOF: the wait thread reports the exit
            Ok(n) => n,
        };
        let mut out = Vec::with_capacity(n);
        let mut ingest = Vec::new();
        parser.feed(&buf[..n], &mut out, &mut ingest);

        // Feed the model up to each marker and read where the cursor is:
        // that line is the marker's position in the session's line space.
        let mut positioned = Vec::with_capacity(ingest.len());
        {
            let mut screen = session.screen.lock();
            let mut fed = 0usize;
            for e in ingest {
                let at = e.offset.min(out.len());
                screen.advance(&out[fed..at]);
                fed = at;
                positioned.push((screen.cursor_abs_line(), e.event));
            }
            screen.advance(&out[fed..]);
        }

        for (line, event) in positioned {
            if events
                .send(SessionEvent::Ingest {
                    session: session.id,
                    line,
                    event,
                })
                .is_err()
            {
                return; // daemon gone
            }
        }
        if !out.is_empty()
            && events
                .send(SessionEvent::Dirty {
                    session: session.id,
                })
                .is_err()
        {
            return;
        }
    }
}

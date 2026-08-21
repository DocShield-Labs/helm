//! A pane: one PTY, one child process, one ring buffer.
//!
//! The reader thread is the single producer for a pane's byte stream:
//! raw PTY bytes → `StreamParser` (strips OSC 133 / bells, detects
//! alt-screen) → ring append → `PaneEvent`s to the daemon core. Sequence
//! numbers are assigned here, under the ring lock, so `Output` frames
//! and marker events can never disagree about stream positions.

use std::io::Read;
use std::sync::Arc;

use parking_lot::Mutex;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc::UnboundedSender;

use helm_proto::{BlockMeta, PaneId};

use crate::markers::{IngestEvent, StreamParser};
use crate::ring::RingBuffer;

/// 8 MB of raw scrollback per pane. At ~120 bytes/line of dense colored
/// output that's ~70k lines — far past tmux's 2000-line default.
pub const RING_CAPACITY: usize = 8 * 1024 * 1024;

/// Events flowing from reader/wait threads into the daemon core.
#[derive(Debug)]
pub enum PaneEvent {
    /// Cleaned bytes appended to the ring at `seq`.
    Output { pane: PaneId, seq: u64, bytes: Vec<u8> },
    /// Semantic event at an absolute stream position.
    Ingest { pane: PaneId, seq: u64, event: IngestEvent },
    /// Child exited (or the PTY hit EOF).
    Exited { pane: PaneId, status: Option<i32> },
}

/// Mutable pane metadata maintained by the daemon core.
#[derive(Debug, Default)]
pub struct PaneMeta {
    pub cols: u16,
    pub rows: u16,
    pub alt_screen: bool,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// argv[0] basename of what was spawned ("zsh", "claude", …).
    pub command: Option<String>,
    pub blocks: Vec<BlockMeta>,
    /// Index into `blocks` of the block still awaiting its `D` marker.
    pub open_block: Option<usize>,
    pub next_block_id: u64,
    pub exited: bool,
}

pub struct Pane {
    pub id: PaneId,
    pub ring: Mutex<RingBuffer>,
    pub meta: Mutex<PaneMeta>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

pub struct SpawnSpec {
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    /// argv to exec; `None` = the user's login shell.
    pub command: Option<Vec<String>>,
    /// Extra environment (integration vars) layered on the inherited env.
    pub env: Vec<(String, String)>,
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        spec: &SpawnSpec,
        events: UnboundedSender<PaneEvent>,
    ) -> anyhow::Result<Arc<Pane>> {
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
        cmd.env("TERM", "xterm-256color");
        // helmd inherits whatever environment launched the app (an
        // `open` from inside tmux, say); don't let a previous terminal's
        // identity leak into every pane. Identify ourselves instead.
        for k in ["TMUX", "TMUX_PANE", "TERM_SESSION_ID", "ITERM_SESSION_ID", "WARP_SESSION_ID"] {
            cmd.env_remove(k);
        }
        cmd.env("TERM_PROGRAM", "Helm");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        // The pane's tty path, inherited by every process in the pane.
        // Tools that run hooks with no controlling terminal (Claude
        // Code) write their BEL here and helmd sees it on this PTY —
        // works for explicit-command windows too, which never source a
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
        let writer = pair.master.take_writer()?;
        let killer = child.clone_killer();

        let command_name = spec.command.as_ref().and_then(|argv| {
            argv.first().map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone())
            })
        });

        let pane = Arc::new(Pane {
            id,
            ring: Mutex::new(RingBuffer::new(RING_CAPACITY)),
            meta: Mutex::new(PaneMeta {
                cols: spec.cols,
                rows: spec.rows,
                command: command_name,
                cwd: spec.cwd.clone(),
                ..Default::default()
            }),
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
        });

        // Reader thread: PTY → parser → ring → events.
        {
            let pane = pane.clone();
            let events = events.clone();
            std::thread::Builder::new()
                .name(format!("pane-{id}-read"))
                .spawn(move || reader_loop(pane, reader, events))?;
        }

        // Wait thread: child exit status.
        {
            let events = events.clone();
            std::thread::Builder::new()
                .name(format!("pane-{id}-wait"))
                .spawn(move || {
                    let status = child.wait().ok().map(|s| s.exit_code() as i32);
                    let _ = events.send(PaneEvent::Exited { pane: id, status });
                })?;
        }

        Ok(pane)
    }

    /// Write client keystrokes to the PTY. Raw bytes, no translation.
    pub fn input(&self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut w = self.writer.lock();
        w.write_all(bytes)?;
        w.flush()
    }

    /// Last-writer-wins resize (no tmux-style union across clients).
    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master.lock().resize(PtySize {
            rows: rows.max(2),
            cols: cols.max(2),
            pixel_width: 0,
            pixel_height: 0,
        })?;
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
    pane: Arc<Pane>,
    mut reader: Box<dyn Read + Send>,
    events: UnboundedSender<PaneEvent>,
) {
    let mut parser = StreamParser::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break, // EOF: the wait thread reports the exit
            Ok(n) => n,
        };
        let mut out = Vec::with_capacity(n);
        let mut ingest = Vec::new();
        parser.feed(&buf[..n], &mut out, &mut ingest);

        // Assign seq under the ring lock so append order == seq order.
        let base = {
            let mut ring = pane.ring.lock();
            let base = ring.head_seq();
            ring.append(&out);
            base
        };
        if !out.is_empty()
            && events
                .send(PaneEvent::Output { pane: pane.id, seq: base, bytes: out })
                .is_err()
        {
            break; // daemon gone
        }
        for e in ingest {
            let _ = events.send(PaneEvent::Ingest {
                pane: pane.id,
                seq: base + e.offset as u64,
                event: e.event,
            });
        }
    }
}

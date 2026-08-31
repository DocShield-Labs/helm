//! Daemon core: long-running sessions, client fan-out,
//! block bookkeeping, screen/history flushes, and the offline
//! notification queue.
//!
//! One `Mutex<Core>` guards the tree and the client table — contention
//! is negligible (a handful of clients, events already batched by the
//! reader threads), and a single lock keeps session/client
//! invariants trivially consistent. Lock order where two are held:
//! `core` → session `meta`, never the reverse; a session's `screen` is never
//! held together with `core` (a flush encodes the grid under `screen`
//! alone, so a busy session can't stall input to every other session).
//!
//! The event loop task (`Daemon::run`) is the only consumer of
//! `SessionEvent`s. Screen changes are coalesced: a `Dirty` event
//! schedules one flush per session ≥ 16 ms out, and the flush hands every
//! attached client the rows that scrolled out plus the damage since
//! the last flush.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use helm_proto::{
    BlockId, BlockMeta, DaemonMsg, Notification, NotificationId, NotificationKind, PathCompletion,
    SearchMatch, SearchScope, SessionId, SessionInfo, TreeSnapshot,
};

use crate::markers::{IngestEvent, Osc133};
use crate::screen::Update;
use crate::session::{Session, SessionEvent, SessionMeta, SpawnSpec};

/// Cap on the offline notification queue — old entries are dropped
/// first; strictly better than tmux's single bell flag either way.
const MAX_PENDING_NOTIFICATIONS: usize = 500;
/// Blocks retained per session (metadata only; rows live in the model).
const MAX_BLOCKS_PER_SESSION: usize = 1000;
/// Screen/history flush coalescing window (~60 Hz).
const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

pub type ClientId = u64;

struct ClientHandle {
    tx: UnboundedSender<DaemonMsg>,
    /// Set by `Attach` — only attached clients receive live broadcasts.
    attached: bool,
}

#[derive(Default)]
struct Core {
    sessions: HashMap<SessionId, Arc<Session>>,
    clients: HashMap<ClientId, ClientHandle>,
    pending: Vec<Notification>,
    draining: bool,
}

pub struct Daemon {
    core: Mutex<Core>,
    events_tx: UnboundedSender<SessionEvent>,
    /// Sessions with a flush scheduled but not yet run.
    flush_pending: Mutex<HashSet<SessionId>>,
    next_session: AtomicU64,
    next_client: AtomicU64,
    next_notification: AtomicU64,
    completion_slots: Arc<Semaphore>,
    shutdown: Notify,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Daemon {
    /// Create the daemon and the event loop's receiver. The caller
    /// spawns `run(rx)` on its runtime.
    pub fn new() -> (Arc<Self>, UnboundedReceiver<SessionEvent>) {
        let (events_tx, events_rx) = unbounded_channel();
        let daemon = Arc::new(Self {
            core: Mutex::new(Core::default()),
            events_tx,
            flush_pending: Mutex::new(HashSet::new()),
            next_session: AtomicU64::new(1),
            next_client: AtomicU64::new(1),
            next_notification: AtomicU64::new(1),
            completion_slots: Arc::new(Semaphore::new(4)),
            shutdown: Notify::new(),
        });
        (daemon, events_rx)
    }

    // ---------------------------------------------------------------
    // Client lifecycle (called by the server)
    // ---------------------------------------------------------------

    pub fn add_client(&self, tx: UnboundedSender<DaemonMsg>) -> ClientId {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.core.lock().clients.insert(
            id,
            ClientHandle {
                tx,
                attached: false,
            },
        );
        id
    }

    pub fn remove_client(&self, id: ClientId) {
        self.core.lock().clients.remove(&id);
    }

    pub fn hello_ack(&self, daemon_version: &str) -> DaemonMsg {
        let core = self.core.lock();
        DaemonMsg::HelloAck {
            protocol_version: helm_proto::PROTOCOL_VERSION,
            daemon_version: daemon_version.to_string(),
            state: snapshot(&core),
            pending: core.pending.clone(),
        }
    }

    /// Subscribe the client to live broadcasts. Session contents are
    /// pulled with `screen` / `history` as the client shows sessions.
    pub fn attach(&self, client: ClientId) {
        if let Some(h) = self.core.lock().clients.get_mut(&client) {
            h.attached = true;
        }
    }

    pub fn begin_drain(&self) {
        let mut core = self.core.lock();
        core.draining = true;
        self.shutdown_if_drained_and_empty(&core);
    }

    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    pub async fn shutdown_requested(&self) {
        self.shutdown.notified().await;
    }

    fn shutdown_if_drained_and_empty(&self, core: &Core) {
        if core.draining && core.sessions.is_empty() {
            self.shutdown.notify_one();
        }
    }

    /// A client's reply channel and a session, for request/reply handlers.
    fn client_session(
        &self,
        client: ClientId,
        session_id: SessionId,
    ) -> Result<(UnboundedSender<DaemonMsg>, Arc<Session>), String> {
        let core = self.core.lock();
        let tx = core
            .clients
            .get(&client)
            .ok_or("no such client")?
            .tx
            .clone();
        let session = core
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| format!("no session {session_id}"))?;
        Ok((tx, session))
    }

    /// Reply with the session's full grid.
    pub fn screen(
        &self,
        client: ClientId,
        req_id: u64,
        session_id: SessionId,
    ) -> Result<(), String> {
        let (tx, session) = self.client_session(client, session_id)?;
        let screen = session.screen.lock().snapshot();
        let _ = tx.send(DaemonMsg::Screen {
            req_id: Some(req_id),
            session: session_id,
            screen,
        });
        Ok(())
    }

    /// Reply with a page of history rows.
    pub fn history(
        &self,
        client: ClientId,
        req_id: u64,
        session_id: SessionId,
        from_line: u64,
        to_line: u64,
    ) -> Result<(), String> {
        let (tx, session) = self.client_session(client, session_id)?;
        let (from, rows, history_start, top_line) = {
            let s = session.screen.lock();
            let (from, rows) = s.history_page(from_line, to_line);
            (from, rows, s.history_start(), s.top_line())
        };
        let _ = tx.send(DaemonMsg::History {
            req_id,
            session: session_id,
            from_line: from,
            rows,
            history_start,
            top_line,
        });
        Ok(())
    }

    pub fn complete_path(
        &self,
        session_id: SessionId,
        path: &str,
        directories_only: bool,
        max_results: u32,
    ) -> Result<(Vec<PathCompletion>, bool), String> {
        if path.len() > 4096 {
            return Err("completion path is too long".into());
        }
        // Bounded by construction: every blocking-FS operation holds a
        // completion permit, so no dispatch arm can forget to ask.
        let _permit = self.completion_permit()?;
        let session = self.session(session_id)?;
        let home = dirs::home_dir();
        let cwd = session
            .meta
            .lock()
            .cwd
            .clone()
            .or_else(|| {
                home.as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .ok_or("session cwd is unavailable")?;
        crate::completion::complete_path(
            std::path::Path::new(&cwd),
            home.as_deref(),
            path,
            directories_only,
            max_results,
        )
    }

    /// Slash commands available to an agent in this session's project —
    /// enumerated here because the session may be on a remote host.
    pub fn agent_commands(&self, session_id: SessionId) -> Result<Vec<helm_proto::AgentCommand>, String> {
        let _permit = self.completion_permit()?;
        let session = self.session(session_id)?;
        let meta = session.meta.lock();
        let project = meta.root.clone().or_else(|| meta.cwd.clone());
        drop(meta);
        Ok(crate::agent_commands::list(
            project.as_deref().map(std::path::Path::new),
        ))
    }

    /// Fuzzy recursive file search from the session's cwd. Bounded walk;
    /// see `file_search.rs`.
    pub fn file_search(
        &self,
        session_id: SessionId,
        query: &str,
        max_results: u32,
    ) -> Result<(Vec<PathCompletion>, bool), String> {
        if query.len() > 1024 {
            return Err("file search query is too long".into());
        }
        let _permit = self.completion_permit()?;
        let session = self.session(session_id)?;
        let cwd = session
            .meta
            .lock()
            .cwd
            .clone()
            .or_else(|| dirs::home_dir().map(|p| p.to_string_lossy().into_owned()))
            .ok_or("session cwd is unavailable")?;
        Ok(crate::file_search::file_search(
            std::path::Path::new(&cwd),
            query,
            max_results.min(100) as usize,
        ))
    }

    pub fn completion_permit(&self) -> Result<OwnedSemaphorePermit, String> {
        self.completion_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "completion service is busy".into())
    }

    // ---------------------------------------------------------------
    // Session lifecycle
    // ---------------------------------------------------------------

    pub fn new_session(
        &self,
        name: Option<String>,
        cwd: Option<String>,
        command: Option<Vec<String>>,
    ) -> Result<SessionId, String> {
        let mut core = self.core.lock();
        if core.draining {
            return Err("daemon is draining; create the session on the current daemon".into());
        }
        let session_id = SessionId(self.next_session.fetch_add(1, Ordering::Relaxed));
        let spec = SpawnSpec {
            cols: 80,
            rows: 24,
            cwd,
            command,
            env: integration_env(),
        };
        // Spawn while holding the lifecycle lock so drain, insertion and
        // a very short-lived child's `Exited` event have one total order.
        let session = Session::spawn(session_id, &spec, self.events_tx.clone())
            .map_err(|error| error.to_string())?;
        let session_name = name
            .or_else(|| session.meta.lock().command.clone())
            .unwrap_or_else(|| "shell".to_string());
        *session.name.lock() = session_name;

        core.sessions.insert(session_id, session);
        broadcast_tree(&core);
        Ok(session_id)
    }

    pub fn kill_session(&self, session_id: SessionId) -> Result<(), String> {
        let mut core = self.core.lock();
        let session = core
            .sessions
            .remove(&session_id)
            .ok_or_else(|| format!("no session {session_id}"))?;
        session.kill();
        broadcast_tree(&core);
        self.shutdown_if_drained_and_empty(&core);
        Ok(())
    }

    pub fn rename_session(&self, session_id: SessionId, name: String) -> Result<(), String> {
        let core = self.core.lock();
        let session = core
            .sessions
            .get(&session_id)
            .ok_or_else(|| format!("no session {session_id}"))?;
        *session.name.lock() = name;
        broadcast_tree(&core);
        Ok(())
    }

    // ---------------------------------------------------------------
    // Session I/O
    // ---------------------------------------------------------------

    fn session(&self, session: SessionId) -> Result<Arc<Session>, String> {
        self.core
            .lock()
            .sessions
            .get(&session)
            .cloned()
            .ok_or_else(|| format!("no session {session}"))
    }

    pub fn input(&self, session: SessionId, bytes: &[u8]) -> Result<(), String> {
        self.session(session)?
            .input(bytes)
            .map_err(|e| e.to_string())
    }

    /// Resize the PTY and the model; the resulting full damage reaches
    /// clients on the next flush.
    pub fn resize(&self, session: SessionId, cols: u16, rows: u16) -> Result<(), String> {
        let changed = self
            .session(session)?
            .resize(cols, rows)
            .map_err(|e| e.to_string())?;
        let _ = self.events_tx.send(SessionEvent::Dirty { session });
        // Keep the tree truthful: cols/rows ride the snapshot, and
        // clients (diagnostics, the app's size reconciliation) treat it
        // as the daemon's authoritative geometry.
        if changed {
            broadcast_tree(&self.core.lock());
        }
        Ok(())
    }

    /// Retained block table for a session, oldest first.
    pub fn blocks(&self, session: SessionId) -> Vec<BlockMeta> {
        self.core
            .lock()
            .sessions
            .get(&session)
            .map(|session| session.meta.lock().blocks.clone())
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------
    // Search
    // ---------------------------------------------------------------

    pub fn search(
        &self,
        query: &str,
        case_sensitive: bool,
        scope: SearchScope,
        max_results: u32,
    ) -> (Vec<SearchMatch>, bool) {
        // Resolve scope under the core lock, then release it: the scan
        // itself must not stall fan-out for every other session.
        let in_scope: Vec<Arc<Session>> = {
            let core = self.core.lock();
            match scope {
                SearchScope::All => core.sessions.values().cloned().collect(),
                SearchScope::Session(id) => core.sessions.get(&id).cloned().into_iter().collect(),
            }
        };
        let lowered;
        let needle: &str = if case_sensitive {
            query
        } else {
            lowered = query.to_lowercase();
            &lowered
        };
        let mut matches: Vec<SearchMatch> = Vec::new();
        let mut truncated = false;
        // Reused per row so the scan allocates only for hits.
        let mut text = String::new();
        let mut hay = String::new();

        for session in in_scope {
            // Scan rows under the screen lock, resolve blocks after —
            // never hold `screen` and `meta` together.
            let first_hit = matches.len();
            session.screen.lock().for_each_row(|line, row| {
                if truncated {
                    return;
                }
                text.clear();
                for sp in &row.spans {
                    text.push_str(&sp.text);
                }
                let haystack: &str = if case_sensitive {
                    &text
                } else {
                    hay.clear();
                    hay.extend(text.chars().flat_map(char::to_lowercase));
                    &hay
                };
                let Some(pos) = haystack.find(needle) else {
                    return;
                };
                matches.push(SearchMatch {
                    session: session.id,
                    block: None,
                    line,
                    line_text: text.trim_end().to_string(),
                    match_start: pos as u32,
                    match_end: (pos + needle.len()) as u32,
                });
                truncated = matches.len() >= max_results as usize;
            });
            let meta = session.meta.lock();
            for m in &mut matches[first_hit..] {
                // Blocks are sorted by start_line, so the owning block is
                // the last one starting at or before the line.
                let idx = meta.blocks.partition_point(|b| b.start_line <= m.line);
                m.block = idx
                    .checked_sub(1)
                    .map(|i| &meta.blocks[i])
                    .filter(|b| b.end_line.map(|e| m.line < e).unwrap_or(true))
                    .map(|b| b.id);
            }
            if truncated {
                break;
            }
        }
        (matches, truncated)
    }

    // ---------------------------------------------------------------
    // Notifications
    // ---------------------------------------------------------------

    pub fn ack_notifications(&self, up_to: NotificationId) {
        self.core.lock().pending.retain(|n| n.id > up_to);
    }

    // ---------------------------------------------------------------
    // Event loop
    // ---------------------------------------------------------------

    /// Consume session events forever. Spawn on the runtime once.
    pub async fn run(self: Arc<Self>, mut events: UnboundedReceiver<SessionEvent>) {
        while let Some(event) = events.recv().await {
            self.handle_event(event);
        }
    }

    fn handle_event(self: &Arc<Self>, event: SessionEvent) {
        match event {
            SessionEvent::Dirty { session } => self.schedule_flush(session),
            SessionEvent::Ingest {
                session,
                line,
                event,
            } => self.handle_ingest(session, line, event),
            SessionEvent::Exited {
                session: session_id,
                status,
            } => {
                // Paint whatever the process left behind before it goes.
                self.flush_session(session_id);
                let mut core = self.core.lock();
                broadcast(
                    &core,
                    DaemonMsg::SessionExited {
                        session: session_id,
                        status,
                    },
                );
                core.sessions.remove(&session_id);
                broadcast_tree(&core);
                self.shutdown_if_drained_and_empty(&core);
            }
        }
    }

    /// One flush per session per interval: the first `Dirty` after a flush
    /// arms a timer; later ones are absorbed until it fires.
    fn schedule_flush(self: &Arc<Self>, session: SessionId) {
        if !self.flush_pending.lock().insert(session) {
            return;
        }
        let daemon = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FLUSH_INTERVAL).await;
            daemon.flush_session(session);
        });
    }

    /// Hand attached clients the rows that scrolled out and the damage
    /// since the last flush. The core lock is held only to find the
    /// session and its audience; encoding and sending happen under the
    /// session's own lock, which also keeps two flushes of one session from
    /// interleaving their history appends.
    pub fn flush_session(&self, session_id: SessionId) {
        self.flush_pending.lock().remove(&session_id);
        let (session, audience) = {
            let core = self.core.lock();
            let Some(session) = core.sessions.get(&session_id).cloned() else {
                return;
            };
            let audience: Vec<UnboundedSender<DaemonMsg>> = core
                .clients
                .values()
                .filter(|c| c.attached)
                .map(|c| c.tx.clone())
                .collect();
            (session, audience)
        };
        let mut s = session.screen.lock();
        if audience.is_empty() {
            // Nobody listening: forget pending history (clients re-page on
            // attach) and let damage accumulate into one full paint.
            s.discard_pending();
            return;
        }
        let send = |msg: DaemonMsg| {
            for tx in &audience {
                let _ = tx.send(msg.clone());
            }
        };
        let (first_line, rows) = s.take_pending_history();
        if !rows.is_empty() {
            send(DaemonMsg::HistoryAppend {
                session: session_id,
                first_line,
                rows,
            });
        }
        match s.take_update() {
            Update::None => {}
            Update::Full(screen) => send(DaemonMsg::Screen {
                req_id: None,
                session: session_id,
                screen,
            }),
            Update::Partial {
                top_line,
                scroll,
                rows,
                cursor,
                modes,
            } => send(DaemonMsg::ScreenDiff {
                session: session_id,
                top_line,
                scroll,
                rows,
                cursor,
                modes,
            }),
        }
    }

    fn handle_ingest(&self, session_id: SessionId, line: u64, event: IngestEvent) {
        let core = self.core.lock();
        let Some(session) = core.sessions.get(&session_id).cloned() else {
            return;
        };
        match event {
            IngestEvent::AltScreen(on) => {
                session.meta.lock().alt_screen = on;
                broadcast(
                    &core,
                    DaemonMsg::ModeChange {
                        session: session_id,
                        alt_screen: on,
                    },
                );
                // alt_screen rides the tree snapshot too — keep it true.
                broadcast_tree(&core);
            }
            IngestEvent::Bell => {
                drop(core);
                self.notify(session_id, &session, NotificationKind::Bell);
            }
            IngestEvent::Notify(text) => {
                drop(core);
                self.notify(session_id, &session, NotificationKind::Message { text });
            }
            IngestEvent::Marker(marker) => {
                let block = {
                    let mut meta = session.meta.lock();
                    update_blocks(&mut meta, line, marker)
                };
                if let Some(block) = block.clone() {
                    broadcast(
                        &core,
                        DaemonMsg::Block {
                            session: session_id,
                            block,
                        },
                    );
                }
                drop(core);
                if let Some(
                    b @ BlockMeta {
                        exit_code: Some(code),
                        end_line: Some(_),
                        ..
                    },
                ) = block
                {
                    if code != 0 {
                        let duration_ms = match (b.started_at_ms, b.finished_at_ms) {
                            (Some(s), Some(f)) => Some(f.saturating_sub(s)),
                            _ => None,
                        };
                        self.notify(
                            session_id,
                            &session,
                            NotificationKind::CommandDone {
                                exit_code: code,
                                cmdline: b.cmdline,
                                duration_ms,
                            },
                        );
                    }
                }
            }
        }
    }

    fn notify(&self, session_id: SessionId, session: &Arc<Session>, kind: NotificationKind) {
        let preview = session.screen.lock().last_nonempty_text();
        let note = Notification {
            id: NotificationId(self.next_notification.fetch_add(1, Ordering::Relaxed)),
            session: session_id,
            kind,
            preview,
            at_ms: now_ms(),
        };
        let mut core = self.core.lock();
        let any_attached = core.clients.values().any(|c| c.attached);
        if any_attached {
            broadcast(&core, DaemonMsg::Notification { note });
        } else {
            core.pending.push(note);
            let overflow = core.pending.len().saturating_sub(MAX_PENDING_NOTIFICATIONS);
            if overflow > 0 {
                core.pending.drain(..overflow);
            }
        }
    }
}

// -------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------

/// OSC 133 marker → block table transition. Returns the block to
/// broadcast, if any changed.
fn update_blocks(meta: &mut SessionMeta, line: u64, marker: Osc133) -> Option<BlockMeta> {
    match marker {
        Osc133::PromptStart { cwd, branch, root } => {
            // Close a dangling block (its D never arrived).
            if let Some(idx) = meta.open_block.take() {
                if meta.blocks[idx].end_line.is_none() {
                    meta.blocks[idx].end_line = Some(line);
                }
            }
            if cwd.is_some() {
                meta.cwd = cwd.clone();
            }
            meta.branch = branch.clone();
            meta.root = root.clone();
            let block = BlockMeta {
                id: BlockId(meta.next_block_id),
                start_line: line,
                cmd_line: None,
                output_line: None,
                end_line: None,
                cmdline: None,
                cwd,
                branch,
                root,
                exit_code: None,
                started_at_ms: None,
                finished_at_ms: None,
            };
            meta.next_block_id += 1;
            meta.blocks.push(block.clone());
            let overflow = meta.blocks.len().saturating_sub(MAX_BLOCKS_PER_SESSION);
            if overflow > 0 {
                meta.blocks.drain(..overflow);
            }
            meta.open_block = Some(meta.blocks.len() - 1);
            Some(block)
        }
        Osc133::CommandStart { cmdline } => {
            let idx = meta.open_block?;
            let b = &mut meta.blocks[idx];
            b.cmd_line = Some(line);
            b.cmdline = cmdline;
            b.started_at_ms = Some(now_ms());
            Some(b.clone())
        }
        Osc133::OutputStart => {
            let idx = meta.open_block?;
            let b = &mut meta.blocks[idx];
            b.output_line = Some(line);
            Some(b.clone())
        }
        Osc133::CommandDone { exit_code } => {
            let idx = meta.open_block.take()?;
            let b = &mut meta.blocks[idx];
            b.end_line = Some(line);
            b.exit_code = exit_code;
            b.finished_at_ms = Some(now_ms());
            Some(b.clone())
        }
    }
}

fn broadcast(core: &Core, msg: DaemonMsg) {
    for client in core.clients.values() {
        if client.attached {
            let _ = client.tx.send(msg.clone());
        }
    }
}

fn broadcast_tree(core: &Core) {
    broadcast(
        core,
        DaemonMsg::TreeChanged {
            state: snapshot(core),
        },
    );
}

fn snapshot(core: &Core) -> TreeSnapshot {
    let mut sessions: Vec<SessionInfo> = core
        .sessions
        .values()
        .map(|session| {
            let meta = session.meta.lock();
            SessionInfo {
                id: session.id,
                name: session.name.lock().clone(),
                cols: meta.cols,
                rows: meta.rows,
                alt_screen: meta.alt_screen,
                cwd: meta.cwd.clone(),
                branch: meta.branch.clone(),
                root: meta.root.clone(),
                command: meta.command.clone(),
            }
        })
        .collect();
    sessions.sort_by_key(|session| session.id);
    TreeSnapshot { sessions }
}

/// Environment layered onto every spawned shell — replaces tmux's
/// `set-environment` fan-out.
///
/// The user's real zsh directory is always `$HOME` here: sessions start
/// from a fixed base (`crate::env`), so there is no inherited ZDOTDIR to
/// honour, and a `~/.zshenv` that relocates it is picked up by the
/// shim's own `.zshenv` forwarder.
fn integration_env() -> Vec<(String, String)> {
    let mut env = vec![("HELM_INTEGRATION".to_string(), "1".to_string())];
    if let Some(home) = dirs::home_dir() {
        env.push((
            "HELM_USER_ZDOTDIR".to_string(),
            home.to_string_lossy().into_owned(),
        ));
        env.push((
            "ZDOTDIR".to_string(),
            home.join(".helm/integration/zsh")
                .to_string_lossy()
                .into_owned(),
        ));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_lifecycle() {
        let mut meta = SessionMeta::default();
        let b = update_blocks(
            &mut meta,
            10,
            Osc133::PromptStart {
                cwd: Some("/x".into()),
                branch: Some("main".into()),
                root: Some("/x".into()),
            },
        )
        .unwrap();
        assert_eq!(b.start_line, 10);
        assert_eq!(meta.cwd.as_deref(), Some("/x"));

        update_blocks(
            &mut meta,
            11,
            Osc133::CommandStart {
                cmdline: Some("ls".into()),
            },
        )
        .unwrap();
        update_blocks(&mut meta, 12, Osc133::OutputStart).unwrap();
        let done =
            update_blocks(&mut meta, 40, Osc133::CommandDone { exit_code: Some(2) }).unwrap();
        assert_eq!(done.cmdline.as_deref(), Some("ls"));
        assert_eq!(done.exit_code, Some(2));
        assert_eq!(done.output_line, Some(12));
        assert_eq!(done.end_line, Some(40));
        assert!(meta.open_block.is_none());

        // Next prompt opens a fresh block.
        let b2 = update_blocks(
            &mut meta,
            40,
            Osc133::PromptStart {
                cwd: None,
                branch: None,
                root: None,
            },
        )
        .unwrap();
        assert_eq!(b2.id, BlockId(1));
        assert_eq!(meta.blocks.len(), 2);
    }

    #[test]
    fn dangling_block_closed_by_next_prompt() {
        let mut meta = SessionMeta::default();
        update_blocks(
            &mut meta,
            0,
            Osc133::PromptStart {
                cwd: None,
                branch: None,
                root: None,
            },
        );
        update_blocks(
            &mut meta,
            1,
            Osc133::CommandStart {
                cmdline: Some("vim".into()),
            },
        );
        // No D (shell died mid-command); next A closes it.
        update_blocks(
            &mut meta,
            50,
            Osc133::PromptStart {
                cwd: None,
                branch: None,
                root: None,
            },
        );
        assert_eq!(meta.blocks[0].end_line, Some(50));
        assert_eq!(meta.blocks[0].exit_code, None);
    }
}

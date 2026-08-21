//! Daemon core: the workspace → window → pane tree, client fan-out,
//! block bookkeeping, and the offline notification queue.
//!
//! One `Mutex<Core>` guards everything — contention is negligible (a
//! handful of clients, events already batched by the reader threads),
//! and a single lock keeps the tree/pane/client invariants trivially
//! consistent. The event loop task (`Daemon::run`) is the only consumer
//! of `PaneEvent`s.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use helm_proto::{
    BlockId, BlockMeta, DaemonMsg, Notification, NotificationId, NotificationKind, PaneId,
    PaneInfo, ReplayFrom, SearchMatch, SearchScope, TreeSnapshot, WindowId, WindowInfo,
    WorkspaceId, WorkspaceInfo,
};

use crate::markers::{strip_ansi, IngestEvent, Osc133};
use crate::pane::{Pane, PaneEvent, PaneMeta, SpawnSpec};

/// Cap on the offline notification queue — old entries are dropped
/// first; strictly better than tmux's single bell flag either way.
const MAX_PENDING_NOTIFICATIONS: usize = 500;
/// Blocks retained per pane (metadata only; bytes live in the ring).
const MAX_BLOCKS_PER_PANE: usize = 1000;

pub type ClientId = u64;

struct ClientHandle {
    tx: UnboundedSender<DaemonMsg>,
    /// Set by `Attach` — only attached clients receive live output.
    attached: bool,
}

struct Workspace {
    id: WorkspaceId,
    name: String,
    windows: Vec<Window>,
}

struct Window {
    id: WindowId,
    name: String,
    panes: Vec<PaneId>,
}

#[derive(Default)]
struct Core {
    workspaces: Vec<Workspace>,
    panes: HashMap<PaneId, Arc<Pane>>,
    clients: HashMap<ClientId, ClientHandle>,
    pending: Vec<Notification>,
}

pub struct Daemon {
    core: Mutex<Core>,
    events_tx: UnboundedSender<PaneEvent>,
    next_workspace: AtomicU64,
    next_window: AtomicU64,
    next_pane: AtomicU64,
    next_client: AtomicU64,
    next_notification: AtomicU64,
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
    pub fn new() -> (Arc<Self>, UnboundedReceiver<PaneEvent>) {
        let (events_tx, events_rx) = unbounded_channel();
        let daemon = Arc::new(Self {
            core: Mutex::new(Core::default()),
            events_tx,
            next_workspace: AtomicU64::new(1),
            next_window: AtomicU64::new(1),
            next_pane: AtomicU64::new(1),
            next_client: AtomicU64::new(1),
            next_notification: AtomicU64::new(1),
        });
        (daemon, events_rx)
    }

    // ---------------------------------------------------------------
    // Client lifecycle (called by the server)
    // ---------------------------------------------------------------

    pub fn add_client(&self, tx: UnboundedSender<DaemonMsg>) -> ClientId {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.core
            .lock()
            .clients
            .insert(id, ClientHandle { tx, attached: false });
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

    /// Mark attached and replay each requested pane from its resume
    /// point. Replayed bytes go only to this client; from here on it
    /// also receives the live broadcast.
    pub fn attach(&self, client: ClientId, resume: &[(PaneId, u64)]) {
        let core = self.core.lock();
        let Some(handle) = core.clients.get(&client) else { return };
        let tx = handle.tx.clone();
        for (pane_id, from_seq) in resume {
            if let Some(pane) = core.panes.get(pane_id) {
                let slice = pane.ring.lock().slice_from(*from_seq);
                if let Some((seq, bytes)) = slice {
                    let _ = tx.send(DaemonMsg::Output { pane: *pane_id, seq, bytes });
                }
                let at_seq = pane.ring.lock().head_seq();
                let _ = tx.send(DaemonMsg::ReplayDone { pane: *pane_id, at_seq });
            }
        }
        drop(core);
        self.core
            .lock()
            .clients
            .get_mut(&client)
            .map(|h| h.attached = true);
    }

    pub fn replay(&self, client: ClientId, pane_id: PaneId, from: ReplayFrom) {
        let core = self.core.lock();
        let (Some(handle), Some(pane)) = (core.clients.get(&client), core.panes.get(&pane_id))
        else {
            return;
        };
        let ring = pane.ring.lock();
        let slice = match from {
            ReplayFrom::Seq(seq) => ring.slice_from(seq),
            ReplayFrom::LastBytes(n) => Some(ring.last_bytes(n)),
        };
        if let Some((seq, bytes)) = slice {
            if !bytes.is_empty() {
                let _ = handle.tx.send(DaemonMsg::Output { pane: pane_id, seq, bytes });
            }
        }
        let _ = handle.tx.send(DaemonMsg::ReplayDone { pane: pane_id, at_seq: ring.head_seq() });
    }

    // ---------------------------------------------------------------
    // Tree operations
    // ---------------------------------------------------------------

    /// Create a workspace plus its initial window (default shell) — a
    /// workspace always has at least one window, same semantic as a
    /// tmux session. The workspace survives even if the window spawn
    /// fails (rare: PTY exhaustion), so the tree stays consistent.
    pub fn new_workspace(
        &self,
        name: Option<String>,
    ) -> Result<(WorkspaceId, WindowId, PaneId), String> {
        let id = WorkspaceId(self.next_workspace.fetch_add(1, Ordering::Relaxed));
        {
            let mut core = self.core.lock();
            let name = name.unwrap_or_else(|| format!("workspace {}", id.0));
            core.workspaces.push(Workspace { id, name, windows: Vec::new() });
        }
        match self.new_window(id, None, None, None) {
            Ok((window, pane)) => Ok((id, window, pane)),
            Err(e) => {
                broadcast_tree(&self.core.lock());
                Err(e)
            }
        }
    }

    pub fn new_window(
        &self,
        workspace: WorkspaceId,
        name: Option<String>,
        cwd: Option<String>,
        command: Option<Vec<String>>,
    ) -> Result<(WindowId, PaneId), String> {
        let pane_id = PaneId(self.next_pane.fetch_add(1, Ordering::Relaxed));
        let window_id = WindowId(self.next_window.fetch_add(1, Ordering::Relaxed));

        let spec = SpawnSpec {
            cols: 80,
            rows: 24,
            cwd,
            command,
            env: integration_env(),
        };
        let pane =
            Pane::spawn(pane_id, &spec, self.events_tx.clone()).map_err(|e| e.to_string())?;

        let mut core = self.core.lock();
        let ws = core
            .workspaces
            .iter_mut()
            .find(|w| w.id == workspace)
            .ok_or_else(|| format!("no workspace {workspace}"))?;
        let name = name
            .or_else(|| pane.meta.lock().command.clone())
            .unwrap_or_else(|| "shell".to_string());
        ws.windows.push(Window { id: window_id, name, panes: vec![pane_id] });
        core.panes.insert(pane_id, pane);
        broadcast_tree(&core);
        Ok((window_id, pane_id))
    }

    pub fn kill_window(&self, window: WindowId) -> Result<(), String> {
        let mut core = self.core.lock();
        let panes: Vec<PaneId> = core
            .workspaces
            .iter()
            .flat_map(|ws| ws.windows.iter())
            .find(|w| w.id == window)
            .map(|w| w.panes.clone())
            .ok_or_else(|| format!("no window {window}"))?;
        for pane_id in panes {
            remove_pane(&mut core, pane_id, true);
        }
        broadcast_tree(&core);
        Ok(())
    }

    pub fn kill_workspace(&self, workspace: WorkspaceId) -> Result<(), String> {
        let mut core = self.core.lock();
        let Some(idx) = core.workspaces.iter().position(|w| w.id == workspace) else {
            return Err(format!("no workspace {workspace}"));
        };
        let panes: Vec<PaneId> = core.workspaces[idx]
            .windows
            .iter()
            .flat_map(|w| w.panes.iter().copied())
            .collect();
        for pane_id in panes {
            remove_pane(&mut core, pane_id, true);
        }
        core.workspaces.remove(idx);
        broadcast_tree(&core);
        Ok(())
    }

    pub fn rename_workspace(&self, workspace: WorkspaceId, name: String) -> Result<(), String> {
        let mut core = self.core.lock();
        let ws = core
            .workspaces
            .iter_mut()
            .find(|w| w.id == workspace)
            .ok_or_else(|| format!("no workspace {workspace}"))?;
        ws.name = name;
        broadcast_tree(&core);
        Ok(())
    }

    pub fn rename_window(&self, window: WindowId, name: String) -> Result<(), String> {
        let mut core = self.core.lock();
        for ws in &mut core.workspaces {
            if let Some(w) = ws.windows.iter_mut().find(|w| w.id == window) {
                w.name = name;
                broadcast_tree(&core);
                return Ok(());
            }
        }
        Err(format!("no window {window}"))
    }

    // ---------------------------------------------------------------
    // Pane I/O
    // ---------------------------------------------------------------

    pub fn input(&self, pane: PaneId, bytes: &[u8]) -> Result<(), String> {
        let pane = self
            .core
            .lock()
            .panes
            .get(&pane)
            .cloned()
            .ok_or_else(|| format!("no pane {pane}"))?;
        pane.input(bytes).map_err(|e| e.to_string())
    }

    pub fn resize(&self, pane: PaneId, cols: u16, rows: u16) -> Result<(), String> {
        let pane = self
            .core
            .lock()
            .panes
            .get(&pane)
            .cloned()
            .ok_or_else(|| format!("no pane {pane}"))?;
        pane.resize(cols, rows).map_err(|e| e.to_string())
    }

    /// Retained block table for a pane, oldest first.
    pub fn blocks(&self, pane: PaneId) -> Vec<BlockMeta> {
        self.core
            .lock()
            .panes
            .get(&pane)
            .map(|p| p.meta.lock().blocks.clone())
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
        // itself must not stall output fan-out for every other pane.
        let in_scope: Vec<Arc<Pane>> = {
            let core = self.core.lock();
            match scope {
                SearchScope::All => core.panes.values().cloned().collect(),
                SearchScope::Pane(p) => core.panes.get(&p).cloned().into_iter().collect(),
                SearchScope::Workspace(ws_id) => core
                    .workspaces
                    .iter()
                    .filter(|w| w.id == ws_id)
                    .flat_map(|w| w.windows.iter())
                    .flat_map(|w| w.panes.iter())
                    .filter_map(|id| core.panes.get(id).cloned())
                    .collect(),
            }
        };
        let lowered;
        let needle: &str = if case_sensitive {
            query
        } else {
            lowered = query.to_lowercase();
            &lowered
        };
        let mut matches = Vec::new();
        let mut truncated = false;

        'outer: for pane in in_scope {
            let ring = pane.ring.lock();
            let start_seq = ring.start_seq();
            let (a, b) = ring.as_slices();
            // Borrow when the ring hasn't wrapped; copy only when it has.
            let joined: std::borrow::Cow<[u8]> = if b.is_empty() {
                std::borrow::Cow::Borrowed(a)
            } else {
                std::borrow::Cow::Owned([a, b].concat())
            };
            let mut line_start = 0usize;
            for line in joined.split_inclusive(|&c| c == b'\n') {
                let line_seq = start_seq + line_start as u64;
                line_start += line.len();
                let text = strip_ansi(line);
                let hay: std::borrow::Cow<str> = if case_sensitive {
                    std::borrow::Cow::Borrowed(&text)
                } else {
                    std::borrow::Cow::Owned(text.to_lowercase())
                };
                let Some(pos) = hay.find(needle) else { continue };
                // Block lookup only for hits: blocks are sorted by
                // start_seq, so the owning block is the last one starting
                // at or before the line.
                let block = {
                    let meta = pane.meta.lock();
                    let idx = meta.blocks.partition_point(|b| b.start_seq <= line_seq);
                    idx.checked_sub(1)
                        .map(|i| &meta.blocks[i])
                        .filter(|b| b.end_seq.map(|e| line_seq <= e).unwrap_or(true))
                        .map(|b| b.id)
                };
                matches.push(SearchMatch {
                    pane: pane.id,
                    block,
                    line_seq,
                    line_text: text.trim_end().to_string(),
                    match_start: pos as u32,
                    match_end: (pos + needle.len()) as u32,
                });
                if matches.len() >= max_results as usize {
                    truncated = true;
                    break 'outer;
                }
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

    /// Consume pane events forever. Spawn on the runtime once.
    pub async fn run(self: Arc<Self>, mut events: UnboundedReceiver<PaneEvent>) {
        while let Some(event) = events.recv().await {
            self.handle_event(event);
        }
    }

    fn handle_event(&self, event: PaneEvent) {
        match event {
            PaneEvent::Output { pane, seq, bytes } => {
                let core = self.core.lock();
                broadcast(&core, DaemonMsg::Output { pane, seq, bytes });
            }
            PaneEvent::Ingest { pane, seq, event } => self.handle_ingest(pane, seq, event),
            PaneEvent::Exited { pane: pane_id, status } => {
                let mut core = self.core.lock();
                broadcast(&core, DaemonMsg::PaneExited { pane: pane_id, status });
                // A window whose only pane died disappears from the tree;
                // the workspace stays.
                remove_pane(&mut core, pane_id, false);
                broadcast_tree(&core);
            }
        }
    }

    fn handle_ingest(&self, pane_id: PaneId, seq: u64, event: IngestEvent) {
        let core = self.core.lock();
        let Some(pane) = core.panes.get(&pane_id).cloned() else { return };
        match event {
            IngestEvent::AltScreen(on) => {
                pane.meta.lock().alt_screen = on;
                broadcast(&core, DaemonMsg::ModeChange { pane: pane_id, alt_screen: on });
            }
            IngestEvent::Bell => {
                drop(core);
                self.notify(pane_id, &pane, NotificationKind::Bell);
            }
            IngestEvent::Marker(marker) => {
                let block = {
                    let mut meta = pane.meta.lock();
                    update_blocks(&mut meta, seq, marker)
                };
                if let Some(block) = block.clone() {
                    broadcast(&core, DaemonMsg::Block { pane: pane_id, block });
                }
                drop(core);
                if let Some(b @ BlockMeta { exit_code: Some(code), end_seq: Some(_), .. }) = block {
                    if code != 0 {
                        let duration_ms = match (b.started_at_ms, b.finished_at_ms) {
                            (Some(s), Some(f)) => Some(f.saturating_sub(s)),
                            _ => None,
                        };
                        self.notify(
                            pane_id,
                            &pane,
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

    fn notify(&self, pane_id: PaneId, pane: &Arc<Pane>, kind: NotificationKind) {
        let preview = {
            let ring = pane.ring.lock();
            let (_, tail) = ring.last_bytes(512);
            preview_from_tail(&tail)
        };
        let note = Notification {
            id: NotificationId(self.next_notification.fetch_add(1, Ordering::Relaxed)),
            pane: pane_id,
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
fn update_blocks(meta: &mut PaneMeta, seq: u64, marker: Osc133) -> Option<BlockMeta> {
    match marker {
        Osc133::PromptStart { cwd, branch } => {
            // Close a dangling block (its D never arrived).
            if let Some(idx) = meta.open_block.take() {
                if meta.blocks[idx].end_seq.is_none() {
                    meta.blocks[idx].end_seq = Some(seq);
                }
            }
            if cwd.is_some() {
                meta.cwd = cwd.clone();
            }
            meta.branch = branch.clone();
            let block = BlockMeta {
                id: BlockId(meta.next_block_id),
                start_seq: seq,
                cmd_seq: None,
                output_seq: None,
                end_seq: None,
                cmdline: None,
                cwd,
                branch,
                exit_code: None,
                started_at_ms: None,
                finished_at_ms: None,
            };
            meta.next_block_id += 1;
            meta.blocks.push(block.clone());
            let overflow = meta.blocks.len().saturating_sub(MAX_BLOCKS_PER_PANE);
            if overflow > 0 {
                meta.blocks.drain(..overflow);
            }
            meta.open_block = Some(meta.blocks.len() - 1);
            Some(block)
        }
        Osc133::CommandStart { cmdline } => {
            let idx = meta.open_block?;
            let b = &mut meta.blocks[idx];
            b.cmd_seq = Some(seq);
            b.cmdline = cmdline;
            b.started_at_ms = Some(now_ms());
            Some(b.clone())
        }
        Osc133::OutputStart => {
            let idx = meta.open_block?;
            let b = &mut meta.blocks[idx];
            b.output_seq = Some(seq);
            Some(b.clone())
        }
        Osc133::CommandDone { exit_code } => {
            let idx = meta.open_block.take()?;
            let b = &mut meta.blocks[idx];
            b.end_seq = Some(seq);
            b.exit_code = exit_code;
            b.finished_at_ms = Some(now_ms());
            Some(b.clone())
        }
    }
}

/// Drop a pane from the map (killing its process if asked) and from
/// whichever window holds it; a window left empty disappears. The one
/// tree-surgery primitive behind kill_window, kill_workspace and exit.
fn remove_pane(core: &mut Core, pane_id: PaneId, kill: bool) {
    if let Some(pane) = core.panes.remove(&pane_id) {
        if kill {
            pane.kill();
        }
    }
    for ws in &mut core.workspaces {
        for w in &mut ws.windows {
            w.panes.retain(|p| *p != pane_id);
        }
        ws.windows.retain(|w| !w.panes.is_empty());
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
    broadcast(core, DaemonMsg::TreeChanged { state: snapshot(core) });
}

fn snapshot(core: &Core) -> TreeSnapshot {
    TreeSnapshot {
        workspaces: core
            .workspaces
            .iter()
            .map(|ws| WorkspaceInfo {
                id: ws.id,
                name: ws.name.clone(),
                windows: ws
                    .windows
                    .iter()
                    .map(|w| WindowInfo {
                        id: w.id,
                        name: w.name.clone(),
                        panes: w
                            .panes
                            .iter()
                            .filter_map(|id| core.panes.get(id))
                            .map(|p| {
                                let meta = p.meta.lock();
                                let ring = p.ring.lock();
                                PaneInfo {
                                    id: p.id,
                                    cols: meta.cols,
                                    rows: meta.rows,
                                    alt_screen: meta.alt_screen,
                                    cwd: meta.cwd.clone(),
                                    branch: meta.branch.clone(),
                                    command: meta.command.clone(),
                                    head_seq: ring.head_seq(),
                                    buffer_start_seq: ring.start_seq(),
                                }
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Environment layered onto every spawned shell — replaces tmux's
/// `set-environment` fan-out.
fn integration_env() -> Vec<(String, String)> {
    let mut env = vec![("HELM_INTEGRATION".to_string(), "1".to_string())];
    if let Some(home) = dirs::home_dir() {
        let user_zdotdir = std::env::var("ZDOTDIR")
            .ok()
            .filter(|z| !z.contains(".helm/integration"))
            .unwrap_or_else(|| home.to_string_lossy().into_owned());
        env.push(("HELM_USER_ZDOTDIR".to_string(), user_zdotdir));
        env.push((
            "ZDOTDIR".to_string(),
            home.join(".helm/integration/zsh").to_string_lossy().into_owned(),
        ));
    }
    env
}

/// Last non-empty stripped line of the tail, clamped to 120 chars.
fn preview_from_tail(tail: &[u8]) -> String {
    let text = strip_ansi(tail);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    line.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_takes_last_nonempty_line() {
        assert_eq!(preview_from_tail(b"first\n\x1b[32msecond\x1b[0m\n\n"), "second");
    }

    #[test]
    fn block_lifecycle() {
        let mut meta = PaneMeta::default();
        let b = update_blocks(
            &mut meta,
            10,
            Osc133::PromptStart { cwd: Some("/x".into()), branch: Some("main".into()) },
        )
        .unwrap();
        assert_eq!(b.start_seq, 10);
        assert_eq!(meta.cwd.as_deref(), Some("/x"));

        update_blocks(&mut meta, 20, Osc133::CommandStart { cmdline: Some("ls".into()) }).unwrap();
        update_blocks(&mut meta, 25, Osc133::OutputStart).unwrap();
        let done = update_blocks(&mut meta, 90, Osc133::CommandDone { exit_code: Some(2) }).unwrap();
        assert_eq!(done.cmdline.as_deref(), Some("ls"));
        assert_eq!(done.exit_code, Some(2));
        assert_eq!(done.end_seq, Some(90));
        assert!(meta.open_block.is_none());

        // Next prompt opens a fresh block.
        let b2 = update_blocks(&mut meta, 95, Osc133::PromptStart { cwd: None, branch: None }).unwrap();
        assert_eq!(b2.id, BlockId(1));
        assert_eq!(meta.blocks.len(), 2);
    }

    #[test]
    fn dangling_block_closed_by_next_prompt() {
        let mut meta = PaneMeta::default();
        update_blocks(&mut meta, 0, Osc133::PromptStart { cwd: None, branch: None });
        update_blocks(&mut meta, 5, Osc133::CommandStart { cmdline: Some("vim".into()) });
        // No D (shell died mid-command); next A closes it.
        update_blocks(&mut meta, 50, Osc133::PromptStart { cwd: None, branch: None });
        assert_eq!(meta.blocks[0].end_seq, Some(50));
        assert_eq!(meta.blocks[0].exit_code, None);
    }
}

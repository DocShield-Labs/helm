//! Daemon core: the workspace → window → pane tree, client fan-out,
//! block bookkeeping, screen/history flushes, and the offline
//! notification queue.
//!
//! One `Mutex<Core>` guards the tree and the client table — contention
//! is negligible (a handful of clients, events already batched by the
//! reader threads), and a single lock keeps the tree/pane/client
//! invariants trivially consistent. Lock order where two are held:
//! `core` → pane `meta`, never the reverse; a pane's `screen` is never
//! held together with `core` (a flush encodes the grid under `screen`
//! alone, so a busy pane can't stall input to every other pane).
//!
//! The event loop task (`Daemon::run`) is the only consumer of
//! `PaneEvent`s. Screen changes are coalesced: a `Dirty` event
//! schedules one flush per pane ≥ 16 ms out, and the flush hands every
//! attached client the rows that scrolled out plus the damage since
//! the last flush.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use helm_proto::{
    BlockId, BlockMeta, DaemonMsg, Notification, NotificationId, NotificationKind, PaneId,
    PaneInfo, SearchMatch, SearchScope, TreeSnapshot, WindowId, WindowInfo, WorkspaceId,
    WorkspaceInfo,
};

use crate::markers::{IngestEvent, Osc133};
use crate::pane::{Pane, PaneEvent, PaneMeta, SpawnSpec};
use crate::screen::Update;

/// Cap on the offline notification queue — old entries are dropped
/// first; strictly better than tmux's single bell flag either way.
const MAX_PENDING_NOTIFICATIONS: usize = 500;
/// Blocks retained per pane (metadata only; rows live in the model).
const MAX_BLOCKS_PER_PANE: usize = 1000;
/// Screen/history flush coalescing window (~60 Hz).
const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

pub type ClientId = u64;

struct ClientHandle {
    tx: UnboundedSender<DaemonMsg>,
    /// Set by `Attach` — only attached clients receive live broadcasts.
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
    /// Panes with a flush scheduled but not yet run.
    flush_pending: Mutex<HashSet<PaneId>>,
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
            flush_pending: Mutex::new(HashSet::new()),
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

    /// Subscribe the client to live broadcasts. Pane contents are
    /// pulled with `screen` / `history` as the client shows panes.
    pub fn attach(&self, client: ClientId) {
        if let Some(h) = self.core.lock().clients.get_mut(&client) {
            h.attached = true;
        }
    }

    /// A client's reply channel and a pane, for request/reply handlers.
    fn client_pane(
        &self,
        client: ClientId,
        pane_id: PaneId,
    ) -> Result<(UnboundedSender<DaemonMsg>, Arc<Pane>), String> {
        let core = self.core.lock();
        let tx = core.clients.get(&client).ok_or("no such client")?.tx.clone();
        let pane = core.panes.get(&pane_id).cloned().ok_or_else(|| format!("no pane {pane_id}"))?;
        Ok((tx, pane))
    }

    /// Reply with the pane's full grid.
    pub fn screen(&self, client: ClientId, req_id: u64, pane_id: PaneId) -> Result<(), String> {
        let (tx, pane) = self.client_pane(client, pane_id)?;
        let screen = pane.screen.lock().snapshot();
        let _ = tx.send(DaemonMsg::Screen { req_id: Some(req_id), pane: pane_id, screen });
        Ok(())
    }

    /// Reply with a page of history rows.
    pub fn history(
        &self,
        client: ClientId,
        req_id: u64,
        pane_id: PaneId,
        from_line: u64,
        to_line: u64,
    ) -> Result<(), String> {
        let (tx, pane) = self.client_pane(client, pane_id)?;
        let (from, rows, history_start, top_line) = {
            let s = pane.screen.lock();
            let (from, rows) = s.history_page(from_line, to_line);
            (from, rows, s.history_start(), s.top_line())
        };
        let _ = tx.send(DaemonMsg::History {
            req_id,
            pane: pane_id,
            from_line: from,
            rows,
            history_start,
            top_line,
        });
        Ok(())
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

    fn pane(&self, pane: PaneId) -> Result<Arc<Pane>, String> {
        self.core
            .lock()
            .panes
            .get(&pane)
            .cloned()
            .ok_or_else(|| format!("no pane {pane}"))
    }

    pub fn input(&self, pane: PaneId, bytes: &[u8]) -> Result<(), String> {
        self.pane(pane)?.input(bytes).map_err(|e| e.to_string())
    }

    /// Resize the PTY and the model; the resulting full damage reaches
    /// clients on the next flush.
    pub fn resize(&self, pane: PaneId, cols: u16, rows: u16) -> Result<(), String> {
        self.pane(pane)?.resize(cols, rows).map_err(|e| e.to_string())?;
        let _ = self.events_tx.send(PaneEvent::Dirty { pane });
        Ok(())
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
        // itself must not stall fan-out for every other pane.
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
        let mut matches: Vec<SearchMatch> = Vec::new();
        let mut truncated = false;
        // Reused per row so the scan allocates only for hits.
        let mut text = String::new();
        let mut hay = String::new();

        for pane in in_scope {
            // Scan rows under the screen lock, resolve blocks after —
            // never hold `screen` and `meta` together.
            let first_hit = matches.len();
            pane.screen.lock().for_each_row(|line, row| {
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
                let Some(pos) = haystack.find(needle) else { return };
                matches.push(SearchMatch {
                    pane: pane.id,
                    block: None,
                    line,
                    line_text: text.trim_end().to_string(),
                    match_start: pos as u32,
                    match_end: (pos + needle.len()) as u32,
                });
                truncated = matches.len() >= max_results as usize;
            });
            let meta = pane.meta.lock();
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

    /// Consume pane events forever. Spawn on the runtime once.
    pub async fn run(self: Arc<Self>, mut events: UnboundedReceiver<PaneEvent>) {
        while let Some(event) = events.recv().await {
            self.handle_event(event);
        }
    }

    fn handle_event(self: &Arc<Self>, event: PaneEvent) {
        match event {
            PaneEvent::Dirty { pane } => self.schedule_flush(pane),
            PaneEvent::Ingest { pane, line, event } => self.handle_ingest(pane, line, event),
            PaneEvent::Exited { pane: pane_id, status } => {
                // Paint whatever the process left behind before it goes.
                self.flush_pane(pane_id);
                let mut core = self.core.lock();
                broadcast(&core, DaemonMsg::PaneExited { pane: pane_id, status });
                // A window whose only pane died disappears from the tree;
                // the workspace stays.
                remove_pane(&mut core, pane_id, false);
                broadcast_tree(&core);
            }
        }
    }

    /// One flush per pane per interval: the first `Dirty` after a flush
    /// arms a timer; later ones are absorbed until it fires.
    fn schedule_flush(self: &Arc<Self>, pane: PaneId) {
        if !self.flush_pending.lock().insert(pane) {
            return;
        }
        let daemon = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FLUSH_INTERVAL).await;
            daemon.flush_pane(pane);
        });
    }

    /// Hand attached clients the rows that scrolled out and the damage
    /// since the last flush. The core lock is held only to find the
    /// pane and its audience; encoding and sending happen under the
    /// pane's own lock, which also keeps two flushes of one pane from
    /// interleaving their history appends.
    pub fn flush_pane(&self, pane_id: PaneId) {
        self.flush_pending.lock().remove(&pane_id);
        let (pane, audience) = {
            let core = self.core.lock();
            let Some(pane) = core.panes.get(&pane_id).cloned() else { return };
            let audience: Vec<UnboundedSender<DaemonMsg>> =
                core.clients.values().filter(|c| c.attached).map(|c| c.tx.clone()).collect();
            (pane, audience)
        };
        let mut s = pane.screen.lock();
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
            send(DaemonMsg::HistoryAppend { pane: pane_id, first_line, rows });
        }
        match s.take_update() {
            Update::None => {}
            Update::Full(screen) => send(DaemonMsg::Screen { req_id: None, pane: pane_id, screen }),
            Update::Partial { top_line, scroll, rows, cursor, modes } => {
                send(DaemonMsg::ScreenDiff { pane: pane_id, top_line, scroll, rows, cursor, modes })
            }
        }
    }

    fn handle_ingest(&self, pane_id: PaneId, line: u64, event: IngestEvent) {
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
            IngestEvent::Notify(text) => {
                drop(core);
                self.notify(pane_id, &pane, NotificationKind::Message { text });
            }
            IngestEvent::Marker(marker) => {
                let block = {
                    let mut meta = pane.meta.lock();
                    update_blocks(&mut meta, line, marker)
                };
                if let Some(block) = block.clone() {
                    broadcast(&core, DaemonMsg::Block { pane: pane_id, block });
                }
                drop(core);
                if let Some(b @ BlockMeta { exit_code: Some(code), end_line: Some(_), .. }) = block {
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
        let preview = pane.screen.lock().last_nonempty_text();
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
fn update_blocks(meta: &mut PaneMeta, line: u64, marker: Osc133) -> Option<BlockMeta> {
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
                                PaneInfo {
                                    id: p.id,
                                    cols: meta.cols,
                                    rows: meta.rows,
                                    alt_screen: meta.alt_screen,
                                    cwd: meta.cwd.clone(),
                                    branch: meta.branch.clone(),
                                    root: meta.root.clone(),
                                    command: meta.command.clone(),
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
///
/// The user's real zsh directory is always `$HOME` here: panes start
/// from a fixed base (`crate::env`), so there is no inherited ZDOTDIR to
/// honour, and a `~/.zshenv` that relocates it is picked up by the
/// shim's own `.zshenv` forwarder.
fn integration_env() -> Vec<(String, String)> {
    let mut env = vec![("HELM_INTEGRATION".to_string(), "1".to_string())];
    if let Some(home) = dirs::home_dir() {
        env.push(("HELM_USER_ZDOTDIR".to_string(), home.to_string_lossy().into_owned()));
        env.push((
            "ZDOTDIR".to_string(),
            home.join(".helm/integration/zsh").to_string_lossy().into_owned(),
        ));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_lifecycle() {
        let mut meta = PaneMeta::default();
        let b = update_blocks(
            &mut meta,
            10,
            Osc133::PromptStart { cwd: Some("/x".into()), branch: Some("main".into()), root: Some("/x".into()) },
        )
        .unwrap();
        assert_eq!(b.start_line, 10);
        assert_eq!(meta.cwd.as_deref(), Some("/x"));

        update_blocks(&mut meta, 11, Osc133::CommandStart { cmdline: Some("ls".into()) }).unwrap();
        update_blocks(&mut meta, 12, Osc133::OutputStart).unwrap();
        let done = update_blocks(&mut meta, 40, Osc133::CommandDone { exit_code: Some(2) }).unwrap();
        assert_eq!(done.cmdline.as_deref(), Some("ls"));
        assert_eq!(done.exit_code, Some(2));
        assert_eq!(done.output_line, Some(12));
        assert_eq!(done.end_line, Some(40));
        assert!(meta.open_block.is_none());

        // Next prompt opens a fresh block.
        let b2 = update_blocks(&mut meta, 40, Osc133::PromptStart { cwd: None, branch: None, root: None }).unwrap();
        assert_eq!(b2.id, BlockId(1));
        assert_eq!(meta.blocks.len(), 2);
    }

    #[test]
    fn dangling_block_closed_by_next_prompt() {
        let mut meta = PaneMeta::default();
        update_blocks(&mut meta, 0, Osc133::PromptStart { cwd: None, branch: None, root: None });
        update_blocks(&mut meta, 1, Osc133::CommandStart { cmdline: Some("vim".into()) });
        // No D (shell died mid-command); next A closes it.
        update_blocks(&mut meta, 50, Osc133::PromptStart { cwd: None, branch: None, root: None });
        assert_eq!(meta.blocks[0].end_line, Some(50));
        assert_eq!(meta.blocks[0].exit_code, None);
    }
}

/**
 * Client-side projection of the Rust workspace tree.
 *
 * Each host has multiple *workspaces* (= tmux sessions). Each workspace
 * owns a set of windows and panes. One workspace per host is the active
 * one (drives the rendered pane); one host is the active one (drives the
 * sidebar selection).
 *
 * The Rust side emits a single tagged `HostEvent` stream. `lib/host.ts`
 * routes those events into the actions on this store.
 */

import { create } from 'zustand'
import type {
  Host,
  HostId,
  HostKeyPromptKind,
  HostStatus,
  Notification,
  NotificationId,
} from '@bindings'

export interface Bootstrap {
  ready: boolean
  message: string
}

export interface TmuxWindow {
  id: string
  name: string
  active: boolean
}

export interface TmuxPane {
  id: string
  windowId: string
  active: boolean
  command: string
  /** The pane's current working directory, sampled at the last
   * tree refetch. Stale until the next refetch — fine for the
   * footer, not authoritative for command execution. */
  cwd: string
  /** Git branch reported for `cwd` at the last refetch (empty when the
   * directory isn't a git repo or git is unavailable). Refreshes on the
   * same cadence as cwd. */
  branch: string
}

export interface TmuxWorkspace {
  /** tmux session id, e.g. `$3` */
  id: string
  name: string
  windows: Map<string, TmuxWindow>
  panes: Map<string, TmuxPane>
}

export interface HostSessions {
  workspaces: Map<string, TmuxWorkspace>
  /** session id of the workspace whose tree is rendered for this host. */
  activeWorkspaceId: string | null
  /** Workspaces the user has explicitly collapsed. Default is expanded;
   * the set tracks opt-out so newly-discovered workspaces appear with
   * windows visible without us having to backfill anything. */
  collapsedWorkspaces: Set<string>
  /** Folders the user has explicitly collapsed in folder-view mode.
   * Keyed by full cwd path (the synthetic grouping key). Mirrors the
   * opt-out semantics of collapsedWorkspaces — newly-observed folders
   * appear expanded by default. In-memory only. */
  collapsedFolders: Set<string>
  /** Populated from a tmux `%exit` notification — null while live. */
  detachedReason: string | null
}

export const emptyHostSessions = (): HostSessions => ({
  workspaces: new Map(),
  activeWorkspaceId: null,
  collapsedWorkspaces: new Set(),
  collapsedFolders: new Set(),
  detachedReason: null,
})

/** Pending host-key prompt — surfaced when the SSH server's key is
 * unknown to `~/.ssh/known_hosts` or has changed. Modal renders one of
 * these and the user's answer goes back via `host_key_prompt_response`.
 *
 * The connect future on the Rust side is parked on this prompt; until
 * we send a decision, that host stays in `Connecting` state. */
export interface HostKeyPrompt {
  hostId: HostId
  hostname: string
  port: number
  algorithm: string
  fingerprint: string
  kind: HostKeyPromptKind
}

/** Snapshot of a window (and its panes) that's been optimistically
 * removed from the sessions tree pending a 5s undo timer. The toast's
 * Undo button calls `restorePendingWindowKill(key)` which puts the
 * snapshot back into the tree; the deferred kill fires from
 * `commitPendingWindowKill(key)` after the timer elapses. */
export interface PendingWindowKill {
  hostId: string
  workspaceId: string
  window: TmuxWindow
  /** All panes whose `windowId` matched the killed window at snapshot
   * time. Stored alongside so restore brings the full row tree back. */
  panes: TmuxPane[]
}

/** Snapshot of an entire workspace that's been optimistically removed
 * pending a 5s undo. Two paths populate this:
 *   1. User explicitly killed the workspace (UI X button or right-click).
 *   2. User killed the workspace's last window — we cascade the
 *      teardown so the empty workspace doesn't linger in the sidebar.
 * Restore re-inserts the full workspace (with its windows + panes)
 * verbatim. */
export interface PendingWorkspaceKill {
  hostId: string
  workspace: TmuxWorkspace
}

/** Options accepted by `requestConfirm`. Mirrors a tiny subset of the
 * native dialog: a heading, a body line, and a label for the
 * affirmative button (defaults to "Confirm"). `destructive` styles the
 * confirm button red. */
export interface ConfirmOptions {
  title: string
  message: string
  confirmLabel?: string
  destructive?: boolean
}

/** Live confirmation request. `resolve` is invoked with the user's
 * answer (true for confirm, false for cancel/close); `requestConfirm`
 * keeps the matching Promise on the calling side. */
export interface ConfirmPrompt extends ConfirmOptions {
  id: number
  resolve: (answer: boolean) => void
}

/** Transient notification, shown at the bottom-right of the window. May
 * carry a deferred action that fires after `durationMs` unless the toast
 * is dismissed first (used for "undo within 5s" patterns). */
export interface Toast {
  id: string
  message: string
  /** When the toast was pushed; used by ToastHost for the countdown
   * indicator and to schedule the deferred action. */
  startedAt: number
  durationMs?: number
  /** Side-effect fired when `durationMs` elapses without dismissal. */
  deferredAction?: () => void
  action?: {
    label: string
    onClick: () => void
  }
}

const emptyWorkspace = (id: string, name: string): TmuxWorkspace => ({
  id,
  name,
  windows: new Map(),
  panes: new Map(),
})

interface HelmState {
  bootstrap: Bootstrap
  setBootstrap: (b: Bootstrap) => void

  // ---------- chrome ----------
  /** When true, the floating sidebar collapses to a 48px dot rail and
   * hover-reveals workspaces/windows. Persisted via localStorage so the
   * preference survives restarts. */
  sidebarCollapsed: boolean
  setSidebarCollapsed: (v: boolean) => void
  toggleSidebar: () => void

  /** How the sidebar groups windows: by their parent workspace (the
   * default — what tmux sessions actually are), or by the folder of
   * each window's cwd (synthetic labels derived at render time). The
   * choice is purely presentational; switching modes never mutates
   * underlying tmux state. Persisted via localStorage. */
  sidebarViewMode: 'workspace' | 'folder'
  setSidebarViewMode: (v: 'workspace' | 'folder') => void
  toggleSidebarViewMode: () => void

  /** Whether the Pinned section in the expanded sidebar is collapsed.
   * Persisted via localStorage so the preference survives restarts.
   * The Inbox / Pinned / Hosts sections all live in one scrollable
   * column now — the user toggles each independently. */
  pinnedSectionCollapsed: boolean
  setPinnedSectionCollapsed: (v: boolean) => void
  /** Whether the Hosts section in the expanded sidebar is collapsed. */
  hostsSectionCollapsed: boolean
  setHostsSectionCollapsed: (v: boolean) => void

  /** Hosts the user has explicitly expanded. Default is **collapsed**:
   * launch always opens to a quiet sidebar with hosts shown as a tidy
   * list of dots, and the user opts in by clicking a host row to drill
   * into its workspace tree. Not persisted — every launch starts with
   * all hosts collapsed. Independent of `activeHostId`. */
  expandedHosts: Set<HostId>
  toggleHostExpanded: (host: HostId) => void

  /** Command palette open state plus an optional initial query string
   * the palette should boot with. Cmd+K passes nothing (empty palette);
   * Cmd+P passes `'@#'` so the palette opens with the workspace + window
   * filter chips already applied. Sub-modes (@workspaces / #windows /
   * $hosts) are still derived from the input — `paletteInitialQuery`
   * just seeds it. */
  paletteOpen: boolean
  paletteInitialQuery: string
  openPalette: (initialQuery?: string) => void
  closePalette: () => void

  // ---------- pinned windows ----------
  /** User's pinned windows — the working set surfaced in the Pinned
   * tab. Identity is `{hostId, workspaceName, windowId}`: workspace by
   * name (resilient to session-id churn after disconnect), window by id
   * (specific within the session). When the underlying window dies the
   * pin shows as stale and the user can remove it explicitly.
   * Persisted via localStorage. */
  pinnedWindows: PinnedWindow[]
  addPinnedWindow: (pin: PinnedWindow) => void
  removePinnedWindow: (key: string) => void
  /** True when there's a pinned entry matching this window's identity. */
  isWindowPinned: (hostId: HostId, workspaceName: string, windowId: string) => boolean

  // ---------- hosts ----------
  hosts: Map<HostId, Host>
  statuses: Map<HostId, HostStatus>
  /** Last connect error per host, populated from `Status` events that
   * carry one (Reconnecting after a failed attempt, Error). Cleared on
   * successful Connected transitions. The ReconnectingOverlay reads
   * this so the user can see *why* a reconnect is stuck instead of
   * staring at a generic spinner. */
  hostErrors: Map<HostId, string>
  /** The host whose tree drives the sidebar selection / rendered pane. */
  activeHostId: HostId | null

  setHosts: (hosts: Host[]) => void
  addHost: (host: Host) => void
  removeHost: (id: HostId) => void
  setHostStatus: (id: HostId, status: HostStatus) => void
  setHostError: (id: HostId, error: string | null) => void
  setActiveHost: (id: HostId) => void

  // ---------- per-host latency ----------
  /** EWMA-smoothed round-trip time (ms) of the most recent ping to each
   * remote host. Local hosts skip pinging entirely (always ~0ms). */
  hostLatencies: Map<HostId, number>
  observeHostLatency: (id: HostId, ms: number) => void

  // ---------- pane capture cache ----------
  /** Cached `tmux capture-pane` buffer per pane, keyed by
   * `${hostId}::${paneId}`. Pre-fetched after host connect so opening
   * a pane for the first time is instant — no round-trip to capture
   * historic content. Live updates flow through `subscribePaneOutput`
   * once the xterm mounts.
   *
   * `hasScrollback` distinguishes the cheap-but-partial pre-hydrate
   * (visible buffer only) from a full-history capture. TmuxPane uses
   * the partial for instant first paint, then upgrades to full
   * scrollback in the background. */
  paneCaptures: Map<string, { data: string; hasScrollback: boolean }>
  setPaneCapture: (host: HostId, paneId: string, data: string, hasScrollback: boolean) => void
  clearPaneCapturesForHost: (host: HostId) => void

  // ---------- per-pane block selection (Phase 4F) ----------
  /** Currently-selected block id per pane, keyed by
   * `${hostId}::${paneId}`. Drives the Cmd+Up/Down/C/Shift+C/R block
   * action keymap in `TmuxPane`. The block list itself lives in the
   * pane's local React state (no need to round-trip through Zustand
   * for that — only the selection cursor needs to be addressable from
   * outside the component). */
  perPaneSelectedBlock: Map<string, string | null>
  setSelectedBlock: (host: HostId, paneId: string, blockId: string | null) => void

  // ---------- per-host sessions ----------
  sessions: Map<HostId, HostSessions>

  setActiveWorkspace: (host: HostId, workspaceId: string) => void
  /** Wholesale replace a host's workspaces from a refetch. Preserves
   * activeWorkspaceId if the workspace still exists; otherwise picks the
   * first available one (or null if none). Honours `pendingWindowKills`:
   * any window whose kill is mid-undo gets stripped from the incoming
   * tree (along with its panes) so a refetch in that 5s window can't
   * resurrect it. */
  setWorkspaces: (host: HostId, workspaces: TmuxWorkspace[]) => void
  /** Flag a workspace as renamed. */
  renameWorkspace: (host: HostId, workspaceId: string, name: string) => void
  /** Flag the active window within a workspace; demote others. */
  setActiveWindow: (host: HostId, workspaceId: string, windowId: string) => void
  /** Flag the active pane within a window; demote others in that window. */
  setActivePane: (host: HostId, workspaceId: string, windowId: string, paneId: string) => void
  /** Update a single pane's cwd (and branch) in place. Driven by the
   * OSC 133 `prompt_start` marker stream so the sidebar's folder
   * grouping reflects user `cd`s without waiting for the next tree
   * refetch. No-op when the pane isn't in the tree yet — a marker
   * arriving before the next setWorkspaces refetch is a benign race. */
  updatePaneCwd: (host: HostId, paneId: string, cwd: string, branch: string) => void
  /** Toggle whether a workspace's window list is visible. */
  toggleWorkspaceCollapsed: (host: HostId, workspaceId: string) => void
  /** Toggle whether a folder's window list is visible (folder view). */
  toggleFolderCollapsed: (host: HostId, folderPath: string) => void
  markDetached: (host: HostId, reason: string | null) => void

  // ---------- pending window kills (5s undo) ----------
  /** Snapshots of windows that have been optimistically removed from
   * the sessions tree but whose tmux kill hasn't fired yet (toast still
   * counting down). Keyed by `${hostId}::${windowId}`. The store
   * filters these out of `setWorkspaces` so a mid-undo refetch can't
   * put them back. Cleared by `restorePendingWindowKill` (Cmd+Z) or
   * `commitPendingWindowKill` (toast timer elapsed → tmux kill fires
   * for real). */
  pendingWindowKills: Map<string, PendingWindowKill>
  optimisticRemoveWindow: (host: HostId, workspaceId: string, windowId: string) => void
  restorePendingWindowKill: (key: string) => void
  commitPendingWindowKill: (key: string) => void

  // ---------- pending workspace kills (5s undo) ----------
  /** Snapshots of workspaces optimistically removed pending a kill,
   * keyed by `${hostId}::${workspaceId}`. Two entry points: explicit
   * `optimisticRemoveWorkspace` (user clicked the workspace X), and a
   * cascade from `optimisticRemoveWindow` when the killed window was
   * the last one in its workspace. The store strips these from
   * incoming refetches just like pendingWindowKills. */
  pendingWorkspaceKills: Map<string, PendingWorkspaceKill>
  optimisticRemoveWorkspace: (host: HostId, workspaceId: string) => void
  restorePendingWorkspaceKill: (key: string) => void
  commitPendingWorkspaceKill: (key: string) => void

  // ---------- live running indicator ----------
  /** Panes whose most recent OSC 133 marker was `B` (CommandStart) and
   * which haven't yet received a matching `D` (CommandDone). Sourced
   * from the same `output` events that drive BlockTracker — the
   * sidebar dot rolls this up to "is anything in this window/workspace
   * currently busy?" so the user sees a spinner on rows where work is
   * happening live. Keyed by `${hostId}::${paneId}`. */
  runningPanes: Map<string, { hostId: HostId; startedAt: number; command: string | null }>
  markPaneRunning: (host: HostId, paneId: string, command: string | null) => void
  markPaneIdle: (host: HostId, paneId: string) => void
  /** Drop every running entry for a host. Called on disconnect and
   * host removal so a stale spinner doesn't outlive its tmux server. */
  clearRunningForHost: (host: HostId) => void

  // ---------- confirm dialog ----------
  /** Pending confirmation request, or null when no dialog is open.
   * `requestConfirm` parks a Promise here that resolves once the user
   * picks Confirm or Cancel — Tauri 2 webviews no-op `window.confirm`,
   * so we render an in-app Modal via `ConfirmHost` instead. At most
   * one prompt at a time; a second request while one is in flight
   * resolves the previous one as cancelled (fail-safe for double
   * triggers). */
  confirmPrompt: ConfirmPrompt | null
  requestConfirm: (opts: ConfirmOptions) => Promise<boolean>
  resolveConfirm: (answer: boolean) => void

  // ---------- toasts ----------
  toasts: Toast[]
  /** Push a new toast. `id` (caller-supplied) lets us coalesce duplicates
   * — pushing with the same id replaces the existing one, which is useful
   * when a kill-workspace toast is already up and the user clicks again. */
  pushToast: (toast: Omit<Toast, 'startedAt'>) => void
  dismissToast: (id: string) => void

  // ---------- host-key prompts ----------
  /** Pending prompts keyed by host id. At most one prompt per host —
   * the SSH connect future is parked awaiting the answer. */
  hostKeyPrompts: Map<HostId, HostKeyPrompt>
  setHostKeyPrompt: (prompt: HostKeyPrompt) => void
  clearHostKeyPrompt: (hostId: HostId) => void

  // ---------- inbox notifications ----------
  /** Live inbox, keyed by notification id. The backend (helm-app) is
   * the source of truth — it emits one HostEvent::Notification per
   * upsert and one HostEvent::NotificationDismissed per dismiss. The
   * frontend never mutates the registry locally; UI actions (× button,
   * dismiss-on-keystroke) call into the Tauri command, which then
   * round-trips back as a Dismissed event. */
  notifications: Map<NotificationId, Notification>
  upsertNotification: (n: Notification) => void
  removeNotification: (id: NotificationId) => void
  /** Drop every notification belonging to `hostId`. Called when a host
   * is removed from the registry — keeps stale rows from outliving
   * their host. The backend already emits per-row Dismissed events on
   * disconnect/delete; this is belt-and-braces for the host_removed
   * event path. */
  dismissNotificationsForHost: (hostId: HostId) => void

  /** Notification currently being hover-peeked. Drives the
   * NotificationPeek overlay that slides down over the main pane to
   * show the source window's recent text without requiring a click.
   * Set on mouse-enter of an inbox row, cleared on leave (debounced). */
  peekedInboxId: NotificationId | null
  setPeekedInboxId: (id: NotificationId | null) => void

  /** Notification whose peek is mid-merge into the main pane after a
   * click. While set, the peek panel runs its dissolve animation
   * (scale + blur + opacity) instead of unmounting cleanly — so the
   * user perceives one continuous transition from peek → live pane.
   * NotificationPeek clears this along with peekedInboxId once the
   * animation timer fires. */
  mergingInboxId: NotificationId | null
  setMergingInboxId: (id: NotificationId | null) => void

  // ---------- tool integration suggestions ----------
  /** Sticky cards prompting the user to install a tool integration
   * (e.g. Claude Code's bell hooks). Pushed by the backend when it
   * detects a known tool running in a pane that doesn't have its
   * integration installed yet. Cleared when the user clicks Install
   * or Not now. Backend keys these (host, integration_id) so each
   * pair fires at most once per app session. */
  toolSuggestions: ToolIntegrationSuggestion[]
  pushToolSuggestion: (s: ToolIntegrationSuggestion) => void
  dismissToolSuggestion: (hostId: HostId, integrationId: string) => void
}

/** One pending tool-integration suggestion. */
export interface ToolIntegrationSuggestion {
  hostId: HostId
  integrationId: string
  name: string
  description: string
  postInstallNote: string
}

/** A window the user has pinned to their working-set view. We store the
 * display labels (hostName, workspaceName, windowName) at pin time so a
 * stale entry — host removed, workspace renamed, window killed — still
 * has something readable to show before the user clears it.
 *
 * Resolution at render time: find host by id → find session by name →
 * find window by id. Any miss = stale. */
export interface PinnedWindow {
  hostId: HostId
  workspaceName: string
  windowId: string
  /** Snapshot labels captured when the pin was created. */
  hostName: string
  windowName: string
}

/** Stable key for a pin. Using both ids keeps localhost+remote pins
 * distinct even if their tmux ids happen to collide (they won't, but
 * defensive). */
export function pinnedKey(hostId: HostId, workspaceName: string, windowId: string): string {
  return `${hostId}::${workspaceName}::${windowId}`
}

/** Sort a collection of `{id: string}` items ascending by id. Used
 * everywhere we want a stable, sidebar-matching order for windows
 * and similar tmux objects. */
export function sortById<T extends { id: string }>(items: Iterable<T>): T[] {
  return [...items].sort((a, b) => a.id.localeCompare(b.id))
}

/** Best-effort localStorage JSON read. Returns `fallback` on missing
 * key, parse failure, or any thrown error. No validation — the caller
 * decides whether to trust the parsed value. For arrays of items use
 * `readJsonArray` instead. */
export function readJson<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return fallback
    return JSON.parse(raw) as T
  } catch {
    return fallback
  }
}

/** localStorage JSON read for arrays, with a per-item type guard.
 * Items that fail the guard are silently dropped — the caller gets
 * back only the valid subset. Returns `[]` on missing key or non-array
 * payload. */
export function readJsonArray<T>(key: string, isItem: (x: unknown) => x is T): T[] {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return []
    const parsed: unknown = JSON.parse(raw)
    return Array.isArray(parsed) ? parsed.filter(isItem) : []
  } catch {
    return []
  }
}

export function writeJson(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value))
  } catch {
    /* localStorage unavailable — caller's data is in-memory only */
  }
}

function withHostSessions(
  sessions: Map<HostId, HostSessions>,
  host: HostId,
  mutate: (s: HostSessions) => HostSessions,
): Map<HostId, HostSessions> {
  const cur = sessions.get(host) ?? emptyHostSessions()
  const next = new Map(sessions)
  next.set(host, mutate(cur))
  return next
}

function withWorkspace(
  hs: HostSessions,
  workspaceId: string,
  mutate: (w: TmuxWorkspace) => TmuxWorkspace,
): HostSessions {
  const cur = hs.workspaces.get(workspaceId)
  if (!cur) return hs
  const next = new Map(hs.workspaces)
  next.set(workspaceId, mutate(cur))
  return { ...hs, workspaces: next }
}

const SIDEBAR_COLLAPSED_KEY = 'helm.sidebarCollapsed'
const readCollapsed = (): boolean => {
  try {
    return localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === '1'
  } catch {
    return false
  }
}
const writeCollapsed = (v: boolean) => {
  try {
    localStorage.setItem(SIDEBAR_COLLAPSED_KEY, v ? '1' : '0')
  } catch {
    /* localStorage unavailable — preference is in-memory only */
  }
}

const SIDEBAR_VIEW_MODE_KEY = 'helm.sidebarViewMode'
const readViewMode = (): 'workspace' | 'folder' => {
  try {
    return localStorage.getItem(SIDEBAR_VIEW_MODE_KEY) === 'folder' ? 'folder' : 'workspace'
  } catch {
    return 'workspace'
  }
}
const writeViewMode = (v: 'workspace' | 'folder') => {
  try {
    localStorage.setItem(SIDEBAR_VIEW_MODE_KEY, v)
  } catch {
    /* localStorage unavailable — preference is in-memory only */
  }
}

const PINNED_WINDOWS_KEY = 'helm.pinnedWindows'
const isPinnedWindow = (p: unknown): p is PinnedWindow =>
  typeof p === 'object' &&
  p !== null &&
  typeof (p as PinnedWindow).hostId === 'string' &&
  typeof (p as PinnedWindow).workspaceName === 'string' &&
  typeof (p as PinnedWindow).windowId === 'string'
const readPinnedWindows = (): PinnedWindow[] =>
  readJsonArray(PINNED_WINDOWS_KEY, isPinnedWindow)
const writePinnedWindows = (pins: PinnedWindow[]) => writeJson(PINNED_WINDOWS_KEY, pins)

const PINNED_SECTION_COLLAPSED_KEY = 'helm.pinnedSectionCollapsed'
const HOSTS_SECTION_COLLAPSED_KEY = 'helm.hostsSectionCollapsed'
const readBoolPref = (key: string, fallback: boolean): boolean => {
  try {
    const v = localStorage.getItem(key)
    if (v === '1') return true
    if (v === '0') return false
  } catch { /* fall through */ }
  return fallback
}
const writeBoolPref = (key: string, v: boolean) => {
  try {
    localStorage.setItem(key, v ? '1' : '0')
  } catch { /* ignore */ }
}

export const useStore = create<HelmState>((set, get) => ({
  bootstrap: { ready: false, message: '' },
  setBootstrap: (b) => set({ bootstrap: b }),

  sidebarCollapsed: readCollapsed(),
  setSidebarCollapsed: (v) => {
    writeCollapsed(v)
    set({ sidebarCollapsed: v })
  },
  toggleSidebar: () =>
    set((s) => {
      const next = !s.sidebarCollapsed
      writeCollapsed(next)
      return { sidebarCollapsed: next }
    }),

  sidebarViewMode: readViewMode(),
  setSidebarViewMode: (v) => {
    writeViewMode(v)
    set({ sidebarViewMode: v })
  },
  toggleSidebarViewMode: () =>
    set((s) => {
      const next: 'workspace' | 'folder' =
        s.sidebarViewMode === 'workspace' ? 'folder' : 'workspace'
      writeViewMode(next)
      return { sidebarViewMode: next }
    }),

  pinnedWindows: readPinnedWindows(),

  pinnedSectionCollapsed: readBoolPref(PINNED_SECTION_COLLAPSED_KEY, false),
  setPinnedSectionCollapsed: (v) => {
    writeBoolPref(PINNED_SECTION_COLLAPSED_KEY, v)
    set({ pinnedSectionCollapsed: v })
  },
  hostsSectionCollapsed: readBoolPref(HOSTS_SECTION_COLLAPSED_KEY, false),
  setHostsSectionCollapsed: (v) => {
    writeBoolPref(HOSTS_SECTION_COLLAPSED_KEY, v)
    set({ hostsSectionCollapsed: v })
  },

  expandedHosts: new Set(),
  toggleHostExpanded: (host) =>
    set((s) => {
      const next = new Set(s.expandedHosts)
      if (next.has(host)) next.delete(host)
      else next.add(host)
      return { expandedHosts: next }
    }),

  paletteOpen: false,
  paletteInitialQuery: '',
  openPalette: (initialQuery = '') =>
    set({ paletteOpen: true, paletteInitialQuery: initialQuery }),
  closePalette: () => set({ paletteOpen: false }),
  addPinnedWindow: (pin) =>
    set((s) => {
      const k = pinnedKey(pin.hostId, pin.workspaceName, pin.windowId)
      // Idempotent: re-pinning is a no-op rather than a duplicate.
      if (s.pinnedWindows.some(p => pinnedKey(p.hostId, p.workspaceName, p.windowId) === k)) {
        return s
      }
      const next = [...s.pinnedWindows, pin]
      writePinnedWindows(next)
      return { pinnedWindows: next }
    }),
  removePinnedWindow: (key) =>
    set((s) => {
      const next = s.pinnedWindows.filter(
        p => pinnedKey(p.hostId, p.workspaceName, p.windowId) !== key,
      )
      if (next.length === s.pinnedWindows.length) return s
      writePinnedWindows(next)
      return { pinnedWindows: next }
    }),
  isWindowPinned: (hostId, workspaceName, windowId) => {
    const k = pinnedKey(hostId, workspaceName, windowId)
    return get().pinnedWindows.some(
      p => pinnedKey(p.hostId, p.workspaceName, p.windowId) === k,
    )
  },

  hosts: new Map(),
  statuses: new Map(),
  hostErrors: new Map(),
  activeHostId: null,

  setHosts: (hosts) =>
    set(() => {
      const map = new Map<HostId, Host>()
      for (const h of hosts) map.set(h.id, h)
      return { hosts: map }
    }),

  addHost: (host) =>
    set((s) => {
      const next = new Map(s.hosts)
      next.set(host.id, host)
      return { hosts: next }
    }),

  removeHost: (id) =>
    set((s) => {
      // Idempotent: a stale `host_removed` event (from a Cmd+R replay,
      // or duplicate emit) just no-ops if the host is already gone.
      // Without this we'd waste a render cycle rebuilding every map.
      if (!s.hosts.has(id) && !s.statuses.has(id)) return {}

      const nextHosts = new Map(s.hosts)
      nextHosts.delete(id)
      const nextStatuses = new Map(s.statuses)
      nextStatuses.delete(id)
      const nextSessions = new Map(s.sessions)
      nextSessions.delete(id)
      const nextErrors = new Map(s.hostErrors)
      nextErrors.delete(id)
      const nextLatencies = new Map(s.hostLatencies)
      nextLatencies.delete(id)
      const nextHostKeyPrompts = new Map(s.hostKeyPrompts)
      nextHostKeyPrompts.delete(id)
      const nextExpanded = new Set(s.expandedHosts)
      nextExpanded.delete(id)

      // Notifications, pane captures, running panes, and tool
      // suggestions are all keyed by string compounds that include
      // the host id — walk each and drop matching entries. The
      // hostId-prefixed key formats are documented at the field
      // declarations above.
      const nextNotifications = new Map<NotificationId, Notification>()
      for (const [nid, n] of s.notifications) {
        if (n.host_id !== id) nextNotifications.set(nid, n)
      }
      const prefix = `${id}::`
      const nextRunning = new Map(s.runningPanes)
      for (const k of s.runningPanes.keys()) {
        if (k.startsWith(prefix)) nextRunning.delete(k)
      }
      const nextCaptures = new Map(s.paneCaptures)
      for (const k of s.paneCaptures.keys()) {
        if (k.startsWith(prefix)) nextCaptures.delete(k)
      }
      const nextSuggestions = s.toolSuggestions.filter((t) => t.hostId !== id)
      const nextPinned = s.pinnedWindows.filter((p) => p.hostId !== id)
      // Pinned windows persist to localStorage — keep on-disk state
      // consistent with the in-memory list so the host doesn't
      // resurrect its pins on next launch.
      if (nextPinned.length !== s.pinnedWindows.length) writePinnedWindows(nextPinned)

      return {
        hosts: nextHosts,
        statuses: nextStatuses,
        sessions: nextSessions,
        hostErrors: nextErrors,
        hostLatencies: nextLatencies,
        hostKeyPrompts: nextHostKeyPrompts,
        expandedHosts: nextExpanded,
        notifications: nextNotifications,
        runningPanes: nextRunning,
        paneCaptures: nextCaptures,
        toolSuggestions: nextSuggestions,
        pinnedWindows: nextPinned,
        activeHostId: s.activeHostId === id ? null : s.activeHostId,
      }
    }),

  setHostStatus: (id, status) =>
    set((s) => {
      const next = new Map(s.statuses)
      next.set(id, status)
      return { statuses: next }
    }),

  setHostError: (id, error) =>
    set((s) => {
      const next = new Map(s.hostErrors)
      if (error === null) {
        if (!next.has(id)) return {}
        next.delete(id)
      } else {
        if (next.get(id) === error) return {}
        next.set(id, error)
      }
      return { hostErrors: next }
    }),

  setActiveHost: (id) => set({ activeHostId: id }),

  hostLatencies: new Map(),
  observeHostLatency: (id, ms) =>
    set((s) => {
      // Exponential weighted moving average so a single spike doesn't
      // make the segment flicker; α=0.3 weights recent samples enough to
      // surface real degradation within a few measurements.
      const prev = s.hostLatencies.get(id)
      const next = prev === undefined ? ms : prev * 0.7 + ms * 0.3
      const map = new Map(s.hostLatencies)
      map.set(id, next)
      return { hostLatencies: map }
    }),

  paneCaptures: new Map(),

  setPaneCapture: (host, paneId, data, hasScrollback) =>
    set((s) => {
      const next = new Map(s.paneCaptures)
      next.set(`${host}::${paneId}`, { data, hasScrollback })
      return { paneCaptures: next }
    }),

  clearPaneCapturesForHost: (host) =>
    set((s) => {
      const prefix = `${host}::`
      const next = new Map<string, { data: string; hasScrollback: boolean }>()
      for (const [k, v] of s.paneCaptures) {
        if (!k.startsWith(prefix)) next.set(k, v)
      }
      return next.size === s.paneCaptures.size ? {} : { paneCaptures: next }
    }),

  perPaneSelectedBlock: new Map(),
  setSelectedBlock: (host, paneId, blockId) =>
    set((s) => {
      const key = `${host}::${paneId}`
      const prev = s.perPaneSelectedBlock.get(key) ?? null
      if (prev === blockId) return {}
      const next = new Map(s.perPaneSelectedBlock)
      if (blockId === null) next.delete(key)
      else next.set(key, blockId)
      return { perPaneSelectedBlock: next }
    }),

  sessions: new Map(),

  setActiveWorkspace: (host, workspaceId) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) => ({
        ...cur,
        activeWorkspaceId: workspaceId,
      })),
    })),

  setWorkspaces: (host, workspaces) =>
    set((s) => {
      // Strip windows AND workspaces whose kill is mid-undo. Without
      // this filter, any refetch in the 5s window (e.g. from a
      // `%window-add` event in another workspace) would resurrect a row
      // the user just killed, and the deferred kill would still fire —
      // so the row would die again 5s later with no visible cause. Both
      // the window/workspace AND any associated panes are removed;
      // restore puts both back from the snapshot.
      const prefix = `${host}::`
      const pendingForHost = new Set<string>()
      for (const k of s.pendingWindowKills.keys()) {
        if (k.startsWith(prefix)) pendingForHost.add(k.slice(prefix.length))
      }
      const pendingWorkspaceForHost = new Set<string>()
      for (const k of s.pendingWorkspaceKills.keys()) {
        if (k.startsWith(prefix)) pendingWorkspaceForHost.add(k.slice(prefix.length))
      }
      const workspaceFiltered = pendingWorkspaceForHost.size === 0
        ? workspaces
        : workspaces.filter((ws) => !pendingWorkspaceForHost.has(ws.id))
      const filtered = pendingForHost.size === 0
        ? workspaceFiltered
        : workspaceFiltered.map((ws) => {
            // Skip allocation when this workspace has no pending-kill
            // overlap — the common case, since pending kills are rare
            // and scoped to one workspace.
            let hasOverlap = false
            for (const wid of ws.windows.keys()) {
              if (pendingForHost.has(wid)) {
                hasOverlap = true
                break
              }
            }
            if (!hasOverlap) return ws
            const windows = new Map(ws.windows)
            for (const wid of ws.windows.keys()) {
              if (pendingForHost.has(wid)) windows.delete(wid)
            }
            const panes = new Map(ws.panes)
            for (const [pid, p] of ws.panes) {
              if (pendingForHost.has(p.windowId)) panes.delete(pid)
            }
            return { ...ws, windows, panes }
          })

      const incoming = new Map<string, TmuxWorkspace>()
      for (const w of filtered) incoming.set(w.id, w)
      const cur = s.sessions.get(host) ?? emptyHostSessions()
      // Keep the existing active selection if the workspace still exists;
      // otherwise pick the first incoming one (or null if zero workspaces).
      const stillThere = cur.activeWorkspaceId && incoming.has(cur.activeWorkspaceId)
      const nextActive = stillThere
        ? cur.activeWorkspaceId
        : filtered[0]?.id ?? null
      const next = new Map(s.sessions)
      next.set(host, {
        ...cur,
        workspaces: incoming,
        activeWorkspaceId: nextActive,
      })
      return { sessions: next }
    }),

  renameWorkspace: (host, workspaceId, name) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) =>
        withWorkspace(cur, workspaceId, (w) => ({ ...w, name })),
      ),
    })),

  setActiveWindow: (host, workspaceId, windowId) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) =>
        withWorkspace(cur, workspaceId, (w) => {
          // Don't early-return if `windowId` isn't in the map yet — tmux
          // sometimes fires `%session-window-changed` BEFORE `%window-add`.
          // The next refetch will pick it up.
          const next = new Map<string, TmuxWindow>()
          for (const [wid, win] of w.windows) {
            next.set(wid, { ...win, active: wid === windowId })
          }
          return { ...w, windows: next }
        }),
      ),
    })),

  setActivePane: (host, workspaceId, windowId, paneId) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) =>
        withWorkspace(cur, workspaceId, (w) => {
          const next = new Map<string, TmuxPane>()
          for (const [pid, p] of w.panes) {
            if (p.windowId === windowId) {
              next.set(pid, { ...p, active: pid === paneId })
            } else {
              next.set(pid, p)
            }
          }
          return { ...w, panes: next }
        }),
      ),
    })),

  updatePaneCwd: (host, paneId, cwd, branch) =>
    set((s) => {
      const hs = s.sessions.get(host)
      if (!hs) return {}
      // Find the workspace owning this pane. We don't index by paneId
      // globally — pane sets per workspace are small (single digits to
      // low tens), so this scan is cheap relative to the cost of a
      // re-render. Bail if the pane isn't in the tree yet (race with
      // tree refetch).
      let targetWsId: string | undefined
      let prev: TmuxPane | undefined
      for (const ws of hs.workspaces.values()) {
        const p = ws.panes.get(paneId)
        if (p) {
          targetWsId = ws.id
          prev = p
          break
        }
      }
      if (!targetWsId || !prev) return {}
      // Skip the re-render if nothing actually changed — `prompt_start`
      // fires on every prompt redraw, so the same cwd+branch arrives
      // many times in a row inside one folder.
      if (prev.cwd === cwd && prev.branch === branch) return {}
      return {
        sessions: withHostSessions(s.sessions, host, (cur) =>
          withWorkspace(cur, targetWsId!, (w) => {
            const next = new Map(w.panes)
            const p = next.get(paneId)
            if (!p) return w
            next.set(paneId, { ...p, cwd, branch })
            return { ...w, panes: next }
          }),
        ),
      }
    }),

  toggleWorkspaceCollapsed: (host, workspaceId) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) => {
        const next = new Set(cur.collapsedWorkspaces)
        if (next.has(workspaceId)) next.delete(workspaceId)
        else next.add(workspaceId)
        return { ...cur, collapsedWorkspaces: next }
      }),
    })),

  toggleFolderCollapsed: (host, folderPath) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) => {
        const next = new Set(cur.collapsedFolders)
        if (next.has(folderPath)) next.delete(folderPath)
        else next.add(folderPath)
        return { ...cur, collapsedFolders: next }
      }),
    })),

  markDetached: (host, reason) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) => ({
        ...cur,
        detachedReason: reason,
      })),
    })),

  pendingWindowKills: new Map(),
  optimisticRemoveWindow: (host, workspaceId, windowId) =>
    set((s) => {
      const hs = s.sessions.get(host)
      const ws = hs?.workspaces.get(workspaceId)
      const win = ws?.windows.get(windowId)
      if (!hs || !ws || !win) return {}
      // Snapshot the window + the panes that belong to it. Restore
      // re-inserts both verbatim except for the active flag, which gets
      // forced to false so a Cmd+Z doesn't yank the user's view back to
      // the restored window when they've already navigated elsewhere.
      const panes: TmuxPane[] = []
      for (const p of ws.panes.values()) {
        if (p.windowId === windowId) panes.push(p)
      }
      const key = `${host}::${windowId}`
      const nextPending = new Map(s.pendingWindowKills)
      nextPending.set(key, { hostId: host, workspaceId, window: win, panes })

      // If this is the last window in the workspace, cascade the
      // teardown: snapshot the (pre-strip) workspace into
      // pendingWorkspaceKills and remove it from the sessions tree, so
      // the user doesn't see a ghost empty workspace until the timer
      // elapses. Restore brings both back together.
      const willBeEmpty = ws.windows.size === 1 && ws.windows.has(windowId)
      const nextSessions = new Map(s.sessions)
      let nextPendingWorkspace = s.pendingWorkspaceKills
      if (willBeEmpty) {
        const wsKey = `${host}::${workspaceId}`
        nextPendingWorkspace = new Map(s.pendingWorkspaceKills)
        nextPendingWorkspace.set(wsKey, { hostId: host, workspace: ws })
        const nextWorkspaces = new Map(hs.workspaces)
        nextWorkspaces.delete(workspaceId)
        // Pick a fallback active workspace if the killed one was active.
        const nextActive =
          hs.activeWorkspaceId === workspaceId
            ? [...nextWorkspaces.values()].sort((a, b) => a.name.localeCompare(b.name))[0]?.id ?? null
            : hs.activeWorkspaceId
        nextSessions.set(host, {
          ...hs,
          workspaces: nextWorkspaces,
          activeWorkspaceId: nextActive,
        })
      } else {
        // Workspace stays — strip the window + its panes only.
        const nextWindows = new Map(ws.windows)
        nextWindows.delete(windowId)
        const nextPanes = new Map(ws.panes)
        for (const p of panes) nextPanes.delete(p.id)
        const nextWorkspaces = new Map(hs.workspaces)
        nextWorkspaces.set(workspaceId, { ...ws, windows: nextWindows, panes: nextPanes })
        nextSessions.set(host, { ...hs, workspaces: nextWorkspaces })
      }
      return {
        sessions: nextSessions,
        pendingWindowKills: nextPending,
        pendingWorkspaceKills: nextPendingWorkspace,
      }
    }),
  restorePendingWindowKill: (key) =>
    set((s) => {
      const snap = s.pendingWindowKills.get(key)
      if (!snap) return {}
      const hs = s.sessions.get(snap.hostId)
      // If the workspace was cascaded out, restore it first so we have
      // somewhere to put the window back.
      const wsKey = `${snap.hostId}::${snap.workspaceId}`
      const wsSnap = s.pendingWorkspaceKills.get(wsKey)
      let workingHs = hs
      let nextPendingWorkspace = s.pendingWorkspaceKills
      if (wsSnap && (!hs || !hs.workspaces.has(snap.workspaceId))) {
        const baseHs = hs ?? emptyHostSessions()
        const restoredWorkspaces = new Map(baseHs.workspaces)
        restoredWorkspaces.set(snap.workspaceId, wsSnap.workspace)
        workingHs = { ...baseHs, workspaces: restoredWorkspaces }
        nextPendingWorkspace = new Map(s.pendingWorkspaceKills)
        nextPendingWorkspace.delete(wsKey)
      }
      const ws = workingHs?.workspaces.get(snap.workspaceId)
      if (!workingHs || !ws) {
        const nextPending = new Map(s.pendingWindowKills)
        nextPending.delete(key)
        return {
          pendingWindowKills: nextPending,
          pendingWorkspaceKills: nextPendingWorkspace,
        }
      }
      const nextWindows = new Map(ws.windows)
      // Force `active: false` on restore so the user's current focus
      // (which they may have moved to a sibling during the 5s undo
      // window) doesn't get yanked back. They asked for "doesn't have
      // to navigate back" — this is what makes that true.
      nextWindows.set(snap.window.id, { ...snap.window, active: false })
      const nextPanes = new Map(ws.panes)
      for (const p of snap.panes) nextPanes.set(p.id, p)
      const nextWorkspaces = new Map(workingHs.workspaces)
      nextWorkspaces.set(snap.workspaceId, { ...ws, windows: nextWindows, panes: nextPanes })
      const nextSessions = new Map(s.sessions)
      nextSessions.set(snap.hostId, { ...workingHs, workspaces: nextWorkspaces })
      const nextPending = new Map(s.pendingWindowKills)
      nextPending.delete(key)
      return {
        sessions: nextSessions,
        pendingWindowKills: nextPending,
        pendingWorkspaceKills: nextPendingWorkspace,
      }
    }),
  commitPendingWindowKill: (key) =>
    set((s) => {
      if (!s.pendingWindowKills.has(key)) return {}
      const next = new Map(s.pendingWindowKills)
      const snap = s.pendingWindowKills.get(key)
      next.delete(key)
      // Drop the cascaded workspace snapshot too — once tmux kills the
      // last window, the empty session tears down server-side anyway.
      let nextWorkspacePending = s.pendingWorkspaceKills
      if (snap) {
        const wsKey = `${snap.hostId}::${snap.workspaceId}`
        if (s.pendingWorkspaceKills.has(wsKey)) {
          nextWorkspacePending = new Map(s.pendingWorkspaceKills)
          nextWorkspacePending.delete(wsKey)
        }
      }
      return {
        pendingWindowKills: next,
        pendingWorkspaceKills: nextWorkspacePending,
      }
    }),

  pendingWorkspaceKills: new Map(),
  optimisticRemoveWorkspace: (host, workspaceId) =>
    set((s) => {
      const hs = s.sessions.get(host)
      const ws = hs?.workspaces.get(workspaceId)
      if (!hs || !ws) return {}
      const wsKey = `${host}::${workspaceId}`
      const nextPending = new Map(s.pendingWorkspaceKills)
      nextPending.set(wsKey, { hostId: host, workspace: ws })
      const nextWorkspaces = new Map(hs.workspaces)
      nextWorkspaces.delete(workspaceId)
      const nextActive =
        hs.activeWorkspaceId === workspaceId
          ? [...nextWorkspaces.values()].sort((a, b) => a.name.localeCompare(b.name))[0]?.id ?? null
          : hs.activeWorkspaceId
      const nextSessions = new Map(s.sessions)
      nextSessions.set(host, {
        ...hs,
        workspaces: nextWorkspaces,
        activeWorkspaceId: nextActive,
      })
      return { sessions: nextSessions, pendingWorkspaceKills: nextPending }
    }),
  restorePendingWorkspaceKill: (key) =>
    set((s) => {
      const snap = s.pendingWorkspaceKills.get(key)
      if (!snap) return {}
      const hs = s.sessions.get(snap.hostId) ?? emptyHostSessions()
      const nextWorkspaces = new Map(hs.workspaces)
      nextWorkspaces.set(snap.workspace.id, snap.workspace)
      const nextSessions = new Map(s.sessions)
      nextSessions.set(snap.hostId, { ...hs, workspaces: nextWorkspaces })
      const nextPending = new Map(s.pendingWorkspaceKills)
      nextPending.delete(key)
      return { sessions: nextSessions, pendingWorkspaceKills: nextPending }
    }),
  commitPendingWorkspaceKill: (key) =>
    set((s) => {
      if (!s.pendingWorkspaceKills.has(key)) return {}
      const next = new Map(s.pendingWorkspaceKills)
      next.delete(key)
      return { pendingWorkspaceKills: next }
    }),

  runningPanes: new Map(),
  markPaneRunning: (host, paneId, command) =>
    set((s) => {
      const key = `${host}::${paneId}`
      const next = new Map(s.runningPanes)
      next.set(key, { hostId: host, startedAt: Date.now(), command })
      return { runningPanes: next }
    }),
  markPaneIdle: (host, paneId) =>
    set((s) => {
      const key = `${host}::${paneId}`
      if (!s.runningPanes.has(key)) return {}
      const next = new Map(s.runningPanes)
      next.delete(key)
      return { runningPanes: next }
    }),
  clearRunningForHost: (host) =>
    set((s) => {
      const prefix = `${host}::`
      const next = new Map<string, { hostId: HostId; startedAt: number; command: string | null }>()
      for (const [k, v] of s.runningPanes) {
        if (!k.startsWith(prefix)) next.set(k, v)
      }
      return next.size === s.runningPanes.size ? {} : { runningPanes: next }
    }),

  confirmPrompt: null,
  requestConfirm: (opts) =>
    new Promise<boolean>((resolve) => {
      set((s) => {
        // Fail-safe: any in-flight prompt is implicitly cancelled. The
        // ConfirmHost only ever shows one at a time, so a stale
        // resolver hanging around would otherwise leak.
        s.confirmPrompt?.resolve(false)
        const id = (s.confirmPrompt?.id ?? 0) + 1
        return { confirmPrompt: { id, ...opts, resolve } }
      })
    }),
  resolveConfirm: (answer) =>
    set((s) => {
      if (!s.confirmPrompt) return {}
      s.confirmPrompt.resolve(answer)
      return { confirmPrompt: null }
    }),

  toasts: [],
  pushToast: (toast) =>
    set((s) => {
      const startedAt = Date.now()
      const next = s.toasts.filter((t) => t.id !== toast.id)
      next.push({ ...toast, startedAt })
      return { toasts: next }
    }),
  dismissToast: (id) =>
    set((s) => {
      const next = s.toasts.filter((t) => t.id !== id)
      return next.length === s.toasts.length ? {} : { toasts: next }
    }),

  hostKeyPrompts: new Map(),
  setHostKeyPrompt: (prompt) =>
    set((s) => {
      const next = new Map(s.hostKeyPrompts)
      next.set(prompt.hostId, prompt)
      return { hostKeyPrompts: next }
    }),
  clearHostKeyPrompt: (hostId) =>
    set((s) => {
      if (!s.hostKeyPrompts.has(hostId)) return {}
      const next = new Map(s.hostKeyPrompts)
      next.delete(hostId)
      return { hostKeyPrompts: next }
    }),

  notifications: new Map(),
  upsertNotification: (n) =>
    set((s) => {
      const next = new Map(s.notifications)
      next.set(n.id, n)
      return { notifications: next }
    }),
  removeNotification: (id) =>
    set((s) => {
      if (!s.notifications.has(id)) return {}
      const next = new Map(s.notifications)
      next.delete(id)
      return { notifications: next }
    }),
  dismissNotificationsForHost: (hostId) =>
    set((s) => {
      let changed = false
      const next = new Map<NotificationId, Notification>()
      for (const [id, n] of s.notifications) {
        if (n.host_id === hostId) {
          changed = true
          continue
        }
        next.set(id, n)
      }
      return changed ? { notifications: next } : {}
    }),

  peekedInboxId: null,
  setPeekedInboxId: (id) => set({ peekedInboxId: id }),

  mergingInboxId: null,
  setMergingInboxId: (id) => set({ mergingInboxId: id }),

  toolSuggestions: [],
  pushToolSuggestion: (sug) =>
    set((s) => {
      // De-dupe per (host, integration). Backend gates this too, but
      // a webview reload could push twice if the backend re-emits.
      const existing = s.toolSuggestions.find(
        (x) => x.hostId === sug.hostId && x.integrationId === sug.integrationId,
      )
      if (existing) return {}
      return { toolSuggestions: [...s.toolSuggestions, sug] }
    }),
  dismissToolSuggestion: (hostId, integrationId) =>
    set((s) => {
      const next = s.toolSuggestions.filter(
        (x) => !(x.hostId === hostId && x.integrationId === integrationId),
      )
      return next.length === s.toolSuggestions.length ? {} : { toolSuggestions: next }
    }),
}))

// ---------- selector helpers ----------

/** Find which workspace owns a given window id (within a host). */
export function workspaceForWindow(
  hs: HostSessions | undefined,
  windowId: string,
): TmuxWorkspace | undefined {
  if (!hs) return undefined
  for (const w of hs.workspaces.values()) {
    if (w.windows.has(windowId)) return w
  }
  return undefined
}

export { emptyWorkspace }

// ---------- notification selectors ----------

/** All notifications for a host, ordered oldest-first by created_at. */
export function notificationsForHost(
  notifications: Map<NotificationId, Notification>,
  hostId: HostId,
): Notification[] {
  const out: Notification[] = []
  for (const n of notifications.values()) {
    if (n.host_id === hostId) out.push(n)
  }
  out.sort((a, b) => a.created_at - b.created_at)
  return out
}

/** All notifications whose pane sits inside the given window. The
 * window_id field on a Notification is best-effort (populated by the
 * backend's pane_runtime cache); when empty we fall back to looking
 * the pane up via the live workspace tree so the rollup still works
 * for notifications created before the index refreshed. */
export function notificationsForWindow(
  notifications: Map<NotificationId, Notification>,
  hostSessions: HostSessions | undefined,
  hostId: HostId,
  windowId: string,
): Notification[] {
  const out: Notification[] = []
  for (const n of notifications.values()) {
    if (n.host_id !== hostId) continue
    if (n.window_id === windowId) {
      out.push(n)
      continue
    }
    // Backend hadn't resolved the window yet — try the local tree.
    if (n.window_id === '' && hostSessions) {
      for (const ws of hostSessions.workspaces.values()) {
        const pane = ws.panes.get(n.pane_id)
        if (pane && pane.windowId === windowId) {
          out.push(n)
          break
        }
      }
    }
  }
  out.sort((a, b) => a.created_at - b.created_at)
  return out
}

/** True when any pane in this window has an open command (we received
 * a `command_start` marker that hasn't been closed by `command_done`).
 * Walks the workspace's pane map filtered by windowId — cheap relative
 * to the per-pane keys lookup we'd need to do otherwise. */
export function isWindowRunning(
  runningPanes: Map<string, { hostId: HostId; startedAt: number; command: string | null }>,
  hostId: HostId,
  workspace: TmuxWorkspace,
  windowId: string,
): boolean {
  for (const pane of workspace.panes.values()) {
    if (pane.windowId !== windowId) continue
    if (runningPanes.has(`${hostId}::${pane.id}`)) return true
  }
  return false
}

/** True when any pane anywhere in this workspace is currently running
 * a command. Used by the workspace-row activity dot. */
export function isWorkspaceRunning(
  runningPanes: Map<string, { hostId: HostId; startedAt: number; command: string | null }>,
  hostId: HostId,
  workspace: TmuxWorkspace,
): boolean {
  for (const pane of workspace.panes.values()) {
    if (runningPanes.has(`${hostId}::${pane.id}`)) return true
  }
  return false
}

/** All notifications for a workspace (any window). Same window_id
 * fallback as notificationsForWindow. */
export function notificationsForWorkspace(
  notifications: Map<NotificationId, Notification>,
  hostSessions: HostSessions | undefined,
  hostId: HostId,
  workspaceId: string,
): Notification[] {
  const ws = hostSessions?.workspaces.get(workspaceId)
  if (!ws) return []
  const windowIds = new Set([...ws.windows.keys()])
  const out: Notification[] = []
  for (const n of notifications.values()) {
    if (n.host_id !== hostId) continue
    if (n.workspace_id === workspaceId || windowIds.has(n.window_id)) {
      out.push(n)
      continue
    }
    if (n.window_id === '' && ws.panes.has(n.pane_id)) {
      out.push(n)
    }
  }
  out.sort((a, b) => a.created_at - b.created_at)
  return out
}

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
  /** Populated from a tmux `%exit` notification — null while live. */
  detachedReason: string | null
}

export const emptyHostSessions = (): HostSessions => ({
  workspaces: new Map(),
  activeWorkspaceId: null,
  collapsedWorkspaces: new Set(),
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

  // ---------- per-host sessions ----------
  sessions: Map<HostId, HostSessions>

  setActiveWorkspace: (host: HostId, workspaceId: string) => void
  /** Wholesale replace a host's workspaces from a refetch. Preserves
   * activeWorkspaceId if the workspace still exists; otherwise picks the
   * first available one (or null if none). */
  setWorkspaces: (host: HostId, workspaces: TmuxWorkspace[]) => void
  /** Flag a workspace as renamed. */
  renameWorkspace: (host: HostId, workspaceId: string, name: string) => void
  /** Flag the active window within a workspace; demote others. */
  setActiveWindow: (host: HostId, workspaceId: string, windowId: string) => void
  /** Flag the active pane within a window; demote others in that window. */
  setActivePane: (host: HostId, workspaceId: string, windowId: string, paneId: string) => void
  /** Toggle whether a workspace's window list is visible. */
  toggleWorkspaceCollapsed: (host: HostId, workspaceId: string) => void
  markDetached: (host: HostId, reason: string | null) => void

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

export const useStore = create<HelmState>((set) => ({
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
      if (!s.hosts.has(id)) return s
      const nextHosts = new Map(s.hosts)
      nextHosts.delete(id)
      const nextStatuses = new Map(s.statuses)
      nextStatuses.delete(id)
      const nextSessions = new Map(s.sessions)
      nextSessions.delete(id)
      const nextErrors = new Map(s.hostErrors)
      nextErrors.delete(id)
      const nextNotifications = new Map<NotificationId, Notification>()
      for (const [nid, n] of s.notifications) {
        if (n.host_id !== id) nextNotifications.set(nid, n)
      }
      return {
        hosts: nextHosts,
        statuses: nextStatuses,
        sessions: nextSessions,
        hostErrors: nextErrors,
        notifications: nextNotifications,
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
      const incoming = new Map<string, TmuxWorkspace>()
      for (const w of workspaces) incoming.set(w.id, w)
      const cur = s.sessions.get(host) ?? emptyHostSessions()
      // Keep the existing active selection if the workspace still exists;
      // otherwise pick the first incoming one (or null if zero workspaces).
      const stillThere = cur.activeWorkspaceId && incoming.has(cur.activeWorkspaceId)
      const nextActive = stillThere
        ? cur.activeWorkspaceId
        : workspaces[0]?.id ?? null
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

  toggleWorkspaceCollapsed: (host, workspaceId) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) => {
        const next = new Set(cur.collapsedWorkspaces)
        if (next.has(workspaceId)) next.delete(workspaceId)
        else next.add(workspaceId)
        return { ...cur, collapsedWorkspaces: next }
      }),
    })),

  markDetached: (host, reason) =>
    set((s) => ({
      sessions: withHostSessions(s.sessions, host, (cur) => ({
        ...cur,
        detachedReason: reason,
      })),
    })),

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

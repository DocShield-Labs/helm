/**
 * Client-side projection of each host's long-running sessions.
 *
 * Each host owns a flat set of sessions (helmd ids stringified). One
 * session per host is active; one host is active globally.
 *
 * The Rust side emits a single tagged `HostEvent` stream. `lib/host.ts`
 * routes those events into the actions on this store.
 */

import { create } from 'zustand'
import { DEFAULT_THEME_NAME } from '@lib/terminal'
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

export interface Session {
  id: string
  name: string
  command: string
  /** The session's current working directory, sampled at the last
   * tree refetch. Stale until the next refetch — fine for the
   * footer, not authoritative for command execution. */
  cwd: string
  /** Git branch reported for `cwd` at the last refetch (empty when the
   * directory isn't a git repo or git is unavailable). Refreshes on the
   * same cadence as cwd. */
  branch: string
  /** Git toplevel of `cwd` (a worktree's own root) at the last refetch;
   * empty outside a repo. The sidebar groups sessions by it. */
  root: string
}

export interface HostSessions {
  sessions: Map<string, Session>
  activeSessionId: string | null
}

export const emptyHostSessions = (): HostSessions => ({
  sessions: new Map(),
  activeSessionId: null,
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

/** Snapshot of a session optimistically removed pending a 5s undo. */
export interface PendingSessionKill {
  hostId: string
  session: Session
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

interface HelmState {
  bootstrap: Bootstrap
  setBootstrap: (b: Bootstrap) => void

  // ---------- chrome ----------
  /** When true, the floating sidebar collapses to a 48px dot rail and
   * hides the session sidebar. Persisted via localStorage so the
   * preference survives restarts. */
  sidebarCollapsed: boolean
  setSidebarCollapsed: (v: boolean) => void
  toggleSidebar: () => void

  /** Active terminal theme name. Drives both xterm's palette and the
   * `--terminal-*` CSS variables the session chrome reads. Persisted
   * via localStorage. */
  themeName: string
  setThemeName: (name: string) => void
  /** Transient theme override applied while the user is cycling
   * through the theme picker. Subscribers prefer this when set. The
   * palette clears it on close — Esc reverts, Enter persists by
   * calling `setThemeName()` and then closing (which clears the
   * preview without changing what's effectively rendered). */
  previewThemeName: string | null
  setPreviewThemeName: (name: string | null) => void

  /** Command palette open state plus an optional initial query string
   * the palette should boot with. Cmd+K passes nothing (empty palette);
   * Cmd+P passes `'#'` so the palette opens with the session filter
   * chip applied. Sub-modes (#sessions / $hosts) are derived from the input;
   * `paletteInitialQuery`
   * just seeds it. */
  paletteOpen: boolean
  paletteInitialQuery: string
  openPalette: (initialQuery?: string) => void
  closePalette: () => void

  // ---------- hosts ----------
  hosts: Map<HostId, Host>
  statuses: Map<HostId, HostStatus>
  /** Last connect error per host, populated from `Status` events that
   * carry one (Reconnecting after a failed attempt, Error). Cleared on
   * successful Connected transitions. The ReconnectingOverlay reads
   * this so the user can see *why* a reconnect is stuck instead of
   * staring at a generic spinner. */
  hostErrors: Map<HostId, string>
  /** The host whose tree drives the sidebar selection / rendered session. */
  activeHostId: HostId | null

  setHosts: (hosts: Host[]) => void
  addHost: (host: Host) => void
  removeHost: (id: HostId) => void
  setHostStatus: (id: HostId, status: HostStatus) => void
  setHostError: (id: HostId, error: string | null) => void
  setActiveHost: (id: HostId) => void

  // ---------- per-host latency ----------

  // ---------- per-host sessions ----------
  sessions: Map<HostId, HostSessions>

  setActiveSession: (host: HostId, sessionId: string) => void
  /** Replace a host's sessions while preserving selection and filtering
   * sessions whose kill is still inside the undo window. */
  setSessions: (host: HostId, sessions: Session[]) => void
  /** Update a single session's cwd (and branch) in place. Driven by
   * block events (each prompt reports cwd/branch) so the sidebar's
   * folder grouping reflects user `cd`s live. No-op when the session
   * isn't in the tree yet. */
  updateSessionCwd: (host: HostId, sessionId: string, cwd: string, branch: string, root: string) => void

  // ---------- pending session kills (5s undo) ----------
  pendingSessionKills: Map<string, PendingSessionKill>
  optimisticRemoveSession: (host: HostId, sessionId: string) => void
  restorePendingSessionKill: (key: string) => void
  commitPendingSessionKill: (key: string) => void

  // ---------- live running indicator ----------
  /** Sessions with an open block (command accepted, not yet finished). */
  runningSessions: Map<string, { hostId: HostId; startedAt: number; command: string | null }>
  markSessionRunning: (host: HostId, sessionId: string, command: string | null) => void
  markSessionIdle: (host: HostId, sessionId: string) => void
  /** Drop every running entry for a host. Called on disconnect and
   * host removal so a stale spinner doesn't outlive its daemon. */
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
   * — pushing with the same id replaces the existing one. */
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
   * NotificationPeek overlay that slides down over the main session to
   * show the source session's recent text without requiring a click.
   * Set on mouse-enter of an inbox row, cleared on leave (debounced). */
  peekedInboxId: NotificationId | null
  setPeekedInboxId: (id: NotificationId | null) => void

  /** Notification whose peek is mid-merge into the main session after a
   * click. While set, the peek overlay runs its dissolve animation
   * (scale + blur + opacity) instead of unmounting cleanly — so the
   * user perceives one continuous transition from peek → live session.
   * NotificationPeek clears this along with peekedInboxId once the
   * animation timer fires. */
  mergingInboxId: NotificationId | null
  setMergingInboxId: (id: NotificationId | null) => void

  // ---------- tool integration suggestions ----------
  /** Sticky cards prompting the user to install a tool integration
   * (e.g. Claude Code's bell hooks). Pushed by the backend when it
   * detects a known tool running in a session that doesn't have its
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

/** Sort a collection of `{id: string}` items ascending by id. helmd
 * ids are monotonic integers stringified, so compare numerically
 * ("10" after "9"); non-numeric ids fall back to string order. */
export function compareIds(a: string, b: string): number {
  const na = Number(a), nb = Number(b)
  if (Number.isFinite(na) && Number.isFinite(nb)) return na - nb
  return a.localeCompare(b)
}

export function sortById<T extends { id: string }>(items: Iterable<T>): T[] {
  return [...items].sort((a, b) => compareIds(a.id, b.id))
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

/** Generic string-pref read/write. Falls back to `fallback` on any
 * localStorage failure (Safari private mode, quota, etc.) and on
 * `validate` rejection. Used for sidebar mode, theme name, and any
 * future enum-shaped preference. Boolean prefs use `readBoolPref`
 * below; JSON-shaped prefs use `readJson`/`writeJson` above. */
const readStringPref = <T extends string>(
  key: string,
  fallback: T,
  validate?: (v: string) => v is T,
): T => {
  try {
    const v = localStorage.getItem(key)
    if (v === null) return fallback
    if (validate && !validate(v)) return fallback
    return v as T
  } catch {
    return fallback
  }
}
const writeStringPref = (key: string, v: string) => {
  try {
    localStorage.setItem(key, v)
  } catch {
    /* localStorage unavailable — preference is in-memory only */
  }
}

const SIDEBAR_COLLAPSED_KEY = 'helm.sidebarCollapsed'
const THEME_NAME_KEY = 'helm.themeName'

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

export const useStore = create<HelmState>((set) => ({
  bootstrap: { ready: false, message: '' },
  setBootstrap: (b) => set({ bootstrap: b }),

  sidebarCollapsed: readBoolPref(SIDEBAR_COLLAPSED_KEY, false),
  setSidebarCollapsed: (v) => {
    writeBoolPref(SIDEBAR_COLLAPSED_KEY, v)
    set({ sidebarCollapsed: v })
  },
  toggleSidebar: () =>
    set((s) => {
      const next = !s.sidebarCollapsed
      writeBoolPref(SIDEBAR_COLLAPSED_KEY, next)
      return { sidebarCollapsed: next }
    }),

  themeName: readStringPref(THEME_NAME_KEY, DEFAULT_THEME_NAME),
  setThemeName: (v) => {
    writeStringPref(THEME_NAME_KEY, v)
    set({ themeName: v })
  },
  previewThemeName: null,
  setPreviewThemeName: (v) => set({ previewThemeName: v }),

  paletteOpen: false,
  paletteInitialQuery: '',
  openPalette: (initialQuery = '') =>
    set({ paletteOpen: true, paletteInitialQuery: initialQuery }),
  closePalette: () => set({ paletteOpen: false }),
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
      const nextHostKeyPrompts = new Map(s.hostKeyPrompts)
      nextHostKeyPrompts.delete(id)

      // Notifications, running sessions, and tool suggestions are all
      // keyed by string compounds that include
      // the host id — walk each and drop matching entries. The
      // hostId-prefixed key formats are documented at the field
      // declarations above.
      const nextNotifications = new Map<NotificationId, Notification>()
      for (const [nid, n] of s.notifications) {
        if (n.host_id !== id) nextNotifications.set(nid, n)
      }
      const prefix = `${id}::`
      const nextRunning = new Map(s.runningSessions)
      for (const k of s.runningSessions.keys()) {
        if (k.startsWith(prefix)) nextRunning.delete(k)
      }
      const nextSuggestions = s.toolSuggestions.filter((t) => t.hostId !== id)

      return {
        hosts: nextHosts,
        statuses: nextStatuses,
        sessions: nextSessions,
        hostErrors: nextErrors,
        hostKeyPrompts: nextHostKeyPrompts,
        notifications: nextNotifications,
        runningSessions: nextRunning,
        toolSuggestions: nextSuggestions,
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


  sessions: new Map(),

  setActiveSession: (host, sessionId) =>
    set((state) => ({
      sessions: withHostSessions(state.sessions, host, (current) => ({
        ...current,
        activeSessionId: sessionId,
      })),
    })),

  setSessions: (host, sessions) =>
    set((state) => {
      const prefix = `${host}::`
      const pending = new Set<string>()
      for (const key of state.pendingSessionKills.keys()) {
        if (key.startsWith(prefix)) pending.add(key.slice(prefix.length))
      }
      const filtered = pending.size === 0
        ? sessions
        : sessions.filter((session) => !pending.has(session.id))
      const current = state.sessions.get(host) ?? emptyHostSessions()
      const incoming = new Map(filtered.map((session) => [session.id, session]))
      const activeSessionId =
        current.activeSessionId && incoming.has(current.activeSessionId)
          ? current.activeSessionId
          : sortById(filtered)[0]?.id ?? null
      const next = new Map(state.sessions)
      next.set(host, { sessions: incoming, activeSessionId })
      return { sessions: next }
    }),

  updateSessionCwd: (host, sessionId, cwd, branch, root) =>
    set((state) => {
      const hostSessions = state.sessions.get(host)
      const previous = hostSessions?.sessions.get(sessionId)
      if (!hostSessions || !previous) return {}
      if (previous.cwd === cwd && previous.branch === branch && previous.root === root) return {}
      const sessions = new Map(hostSessions.sessions)
      sessions.set(sessionId, { ...previous, cwd, branch, root })
      const next = new Map(state.sessions)
      next.set(host, { ...hostSessions, sessions })
      return { sessions: next }
    }),

  pendingSessionKills: new Map(),
  optimisticRemoveSession: (host, sessionId) =>
    set((state) => {
      const hostSessions = state.sessions.get(host)
      const session = hostSessions?.sessions.get(sessionId)
      if (!hostSessions || !session) return {}
      const key = `${host}::${sessionId}`
      const pendingSessionKills = new Map(state.pendingSessionKills)
      pendingSessionKills.set(key, { hostId: host, session })
      const sessions = new Map(hostSessions.sessions)
      sessions.delete(sessionId)
      const activeSessionId = hostSessions.activeSessionId === sessionId
        ? sortById(sessions.values())[0]?.id ?? null
        : hostSessions.activeSessionId
      const next = new Map(state.sessions)
      next.set(host, { sessions, activeSessionId })
      return { sessions: next, pendingSessionKills }
    }),
  restorePendingSessionKill: (key) =>
    set((state) => {
      const snapshot = state.pendingSessionKills.get(key)
      if (!snapshot) return {}
      const hostSessions = state.sessions.get(snapshot.hostId) ?? emptyHostSessions()
      const sessions = new Map(hostSessions.sessions)
      sessions.set(snapshot.session.id, snapshot.session)
      const next = new Map(state.sessions)
      next.set(snapshot.hostId, {
        sessions,
        activeSessionId: hostSessions.activeSessionId ?? snapshot.session.id,
      })
      const pendingSessionKills = new Map(state.pendingSessionKills)
      pendingSessionKills.delete(key)
      return { sessions: next, pendingSessionKills }
    }),
  commitPendingSessionKill: (key) =>
    set((state) => {
      if (!state.pendingSessionKills.has(key)) return {}
      const pendingSessionKills = new Map(state.pendingSessionKills)
      pendingSessionKills.delete(key)
      return { pendingSessionKills }
    }),

  runningSessions: new Map(),
  markSessionRunning: (host, sessionId, command) =>
    set((s) => {
      const key = `${host}::${sessionId}`
      const next = new Map(s.runningSessions)
      next.set(key, { hostId: host, startedAt: Date.now(), command })
      return { runningSessions: next }
    }),
  markSessionIdle: (host, sessionId) =>
    set((s) => {
      const key = `${host}::${sessionId}`
      if (!s.runningSessions.has(key)) return {}
      const next = new Map(s.runningSessions)
      next.delete(key)
      return { runningSessions: next }
    }),
  clearRunningForHost: (host) =>
    set((s) => {
      const prefix = `${host}::`
      const next = new Map<string, { hostId: HostId; startedAt: number; command: string | null }>()
      for (const [k, v] of s.runningSessions) {
        if (!k.startsWith(prefix)) next.set(k, v)
      }
      return next.size === s.runningSessions.size ? {} : { runningSessions: next }
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

/** All notifications for one session. */
export function notificationsForSession(
  notifications: Map<NotificationId, Notification>,
  hostId: HostId,
  sessionId: string,
): Notification[] {
  const out: Notification[] = []
  for (const n of notifications.values()) {
    if (n.host_id === hostId && n.session_id === sessionId) out.push(n)
  }
  out.sort((a, b) => a.created_at - b.created_at)
  return out
}

/** Session ids with unread notifications for one host. */
export function notificationSessionIds(
  notifications: Map<NotificationId, Notification>,
  hostId: HostId,
): Set<string> {
  const ids = new Set<string>()
  for (const notification of notifications.values()) {
    if (notification.host_id === hostId) ids.add(notification.session_id)
  }
  return ids
}

export function isSessionRunning(
  runningSessions: Map<string, { hostId: HostId; startedAt: number; command: string | null }>,
  hostId: HostId,
  sessionId: string,
): boolean {
  return runningSessions.has(`${hostId}::${sessionId}`)
}

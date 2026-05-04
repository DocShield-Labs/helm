/**
 * Phase 2 / multi-workspace landing.
 *
 * Boots the global event channel, lists hosts, picks localhost as the
 * active host, connects, and renders each host's tmux workspaces in the
 * sidebar. Each host can have many workspaces (= tmux sessions); each
 * workspace owns its windows; one window in the active workspace renders
 * its active pane through xterm.
 *
 * Selection model (all in store.ts):
 *   - activeHostId            — which host's tree drives the sidebar
 *   - per-host activeWorkspaceId — within a host, which workspace's windows show
 *   - per-window active flag  — tmux's own notion, surfaced via notifications
 */

import { useEffect, useMemo, useState } from 'react'
import { commands } from '@lib/ipc'
import {
  useStore,
  workspaceForWindow,
  type HostSessions,
  type TmuxWorkspace,
} from '@lib/store'
import { connectHost, selectWorkspace, subscribeHostEvents } from '@lib/host'
import { displayedHostStatus } from '@lib/host-status'
import { TmuxPane } from '@features/shell/TmuxPane'
import { HostEditorModal } from '@features/host-editor/HostEditorModal'
import { HostKeyPromptModal } from '@features/host-key/HostKeyPromptModal'
import { IntegrationSuggestionHost } from '@features/activity-feed/IntegrationSuggestionHost'
import { NotificationPeek } from '@features/activity-feed/NotificationPeek'
import { ReconnectingOverlay } from '@features/workspace/ReconnectingOverlay'
import {
  Sidebar,
  StatusBarHostSegment,
  StatusBarSegment,
  StatusBarDivider,
  ToastHost,
} from '@ui'
import type { Host, HostStatus } from '@bindings'

// Module-level flag: only run the boot chain once per process. A
// component-scoped `useRef` doesn't work here because React 19's
// StrictMode mounts → unmounts → re-mounts the component in dev,
// creating a fresh ref on the second mount; the boot chain would
// then run twice and spawn two `tmux -CC` clients on localhost in
// parallel. Module state survives mount/unmount cycles within the
// same process, so the second mount's effect sees `true` and bails.
let bootStarted = false

export function App() {
  const bootstrap = useStore((s) => s.bootstrap)
  const setBootstrap = useStore((s) => s.setBootstrap)
  const hosts = useStore((s) => s.hosts)
  const statuses = useStore((s) => s.statuses)
  const sessions = useStore((s) => s.sessions)
  const activeHostId = useStore((s) => s.activeHostId)
  const setHosts = useStore((s) => s.setHosts)
  const setActiveHost = useStore((s) => s.setActiveHost)
  const toggleSidebar = useStore((s) => s.toggleSidebar)
  const sidebarCollapsed = useStore((s) => s.sidebarCollapsed)
  const hostLatencies = useStore((s) => s.hostLatencies)
  const hostErrors = useStore((s) => s.hostErrors)
  const [bootError, setBootError] = useState<string | null>(null)
  // Host-editor modal state. `editing` carries the host being edited
  // (or null for "add new").
  const [editorOpen, setEditorOpen] = useState(false)
  const [editing, setEditing] = useState<Host | null>(null)
  // Panes the user has activated at least once. Mounted TmuxPane instances
  // for these are kept alive across workspace/window switches — switching
  // back to a previously-visited pane is instant because its xterm buffer
  // still has all the prior content (and the subscription kept consuming
  // live output while the pane was hidden).
  const [mountedPaneKeys, setMountedPaneKeys] = useState<Set<string>>(new Set())
  useEffect(() => {
    if (bootStarted) return
    bootStarted = true
    void (async () => {
      try {
        const ping = await commands.ping()
        setBootstrap({ ready: ping.ok, message: ping.message })

        await subscribeHostEvents()

        const list = await commands.hostList()
        if (list.status !== 'ok') throw new Error(list.error)
        setHosts(list.data)

        const localId = await commands.hostLocalId()
        setActiveHost(localId)

        await connectHost(localId)

        // Pre-connect every remote host that has a pin so the user's
        // working set comes alive immediately on launch — no need to
        // click each pin to wake it up. Fired in parallel; errors are
        // silenced because a stuck remote shouldn't block boot, and
        // the row will just resolve to "offline · click to connect"
        // if the auto-connect fails.
        const seen = new Set<string>([localId])
        for (const pin of useStore.getState().pinnedWindows) {
          if (seen.has(pin.hostId)) continue
          seen.add(pin.hostId)
          if (!useStore.getState().hosts.has(pin.hostId)) continue
          void connectHost(pin.hostId).catch(() => {})
        }
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e)
        setBootError(msg)
        setBootstrap({ ready: false, message: msg })
      }
    })()
  }, [setBootstrap, setHosts, setActiveHost])

  const activeHost: Host | undefined = activeHostId ? hosts.get(activeHostId) : undefined
  const activeStatus: HostStatus | undefined = activeHostId ? statuses.get(activeHostId) : undefined
  const activeHostSessions: HostSessions | undefined = activeHostId
    ? sessions.get(activeHostId)
    : undefined

  const activeWorkspace: TmuxWorkspace | undefined = useMemo(() => {
    if (!activeHostSessions) return undefined
    const id = activeHostSessions.activeWorkspaceId
    if (!id) return undefined
    return activeHostSessions.workspaces.get(id)
  }, [activeHostSessions])

  // Stable order by tmux id within a workspace.
  const windowList = useMemo(() => {
    if (!activeWorkspace) return []
    return [...activeWorkspace.windows.values()].sort((a, b) => a.id.localeCompare(b.id))
  }, [activeWorkspace])

  const activeWindow = useMemo(
    () => windowList.find((w) => w.active) ?? windowList[0],
    [windowList],
  )

  const activePane = useMemo(() => {
    if (!activeWorkspace || !activeWindow) return undefined
    const inWindow = [...activeWorkspace.panes.values()].filter(
      (p) => p.windowId === activeWindow.id,
    )
    return inWindow.find((p) => p.active) ?? inWindow[0]
  }, [activeWorkspace, activeWindow])

  const activePaneKey =
    activeHostId && activePane ? `${activeHostId}::${activePane.id}` : null

  // Add the active pane's key to the mounted set the first time it
  // appears. We never explicitly drop keys that fall out of "active";
  // the GC effect below removes only keys whose underlying pane no
  // longer exists in any workspace.
  useEffect(() => {
    if (!activePaneKey) return
    setMountedPaneKeys((prev) => {
      if (prev.has(activePaneKey)) return prev
      const next = new Set(prev)
      next.add(activePaneKey)
      return next
    })
  }, [activePaneKey])

  // Garbage-collect mounted panes whose tmux pane id no longer exists
  // (workspace killed, window closed, host removed, etc.). Without this
  // we'd leak xterm instances every time the user kills something.
  useEffect(() => {
    setMountedPaneKeys((prev) => {
      const next = new Set<string>()
      for (const key of prev) {
        const sep = key.indexOf('::')
        const hostId = key.slice(0, sep)
        const paneId = key.slice(sep + 2)
        const hs = sessions.get(hostId)
        if (!hs) continue
        let stillThere = false
        for (const ws of hs.workspaces.values()) {
          if (ws.panes.has(paneId)) {
            stillThere = true
            break
          }
        }
        if (stillThere) next.add(key)
      }
      return next.size === prev.size ? prev : next
    })
  }, [sessions])

  // ---------- keyboard shortcuts ----------
  // Captured at the document level so xterm doesn't swallow the keys first;
  // TmuxPane filters Cmd+ from going to the shell anyway.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (!e.metaKey || e.altKey || e.ctrlKey) return

      // Sidebar toggle is host-agnostic — fire even when no host is active.
      if (e.key === '\\') {
        e.preventDefault()
        toggleSidebar()
        return
      }

      if (!activeHostId) return

      switch (e.key.toLowerCase()) {
        case 'i':
          if (!e.shiftKey) return
          // Cmd+Shift+I → focus oldest inbox notification.
          // Cross-host: if the oldest notification belongs to a
          // different host than the active one, switch hosts before
          // selecting the window. The notification stays in the inbox
          // (peek doesn't dismiss); typing in the pane will dismiss
          // via the keystroke handler in TmuxPane.
          e.preventDefault()
          jumpToOldestNotification()
          return
        case 't':
          e.preventDefault()
          if (e.shiftKey) {
            // Cmd+Shift+T → new workspace on active host
            void createWorkspace(activeHostId)
          } else {
            // Cmd+T → new window in active workspace
            if (activeWorkspace) {
              void commands.tmuxNewWindow(activeHostId, activeWorkspace.id, null)
            }
          }
          return
        case 'w': {
          e.preventDefault()
          if (e.shiftKey) {
            // Cmd+Shift+W → kill active workspace (with confirm)
            if (activeWorkspace) {
              void killWorkspace(activeHostId, activeWorkspace)
            }
          } else if (activeWindow) {
            void commands.tmuxKillWindow(activeHostId, activeWindow.id)
          }
          return
        }
        case ']':
        case 'arrowright': {
          e.preventDefault()
          // In Pinned mode, cycle through the user's working set
          // across hosts. Otherwise, cycle within the active workspace.
          const state = useStore.getState()
          if (state.sidebarTab === 'pinned' && state.pinnedWindows.length > 0) {
            void cyclePinnedWindow(+1)
          } else {
            const next = neighbourWindowId(windowList, activeWindow?.id, +1)
            if (next) {
              void commands.tmuxSelectWindow(activeHostId, next).then((res) => {
                if (res.status !== 'ok') {
                  console.warn('tmux_select_window failed:', res.error)
                }
              })
            }
          }
          return
        }
        case '[':
        case 'arrowleft': {
          e.preventDefault()
          const state = useStore.getState()
          if (state.sidebarTab === 'pinned' && state.pinnedWindows.length > 0) {
            void cyclePinnedWindow(-1)
          } else {
            const prev = neighbourWindowId(windowList, activeWindow?.id, -1)
            if (prev) {
              void commands.tmuxSelectWindow(activeHostId, prev).then((res) => {
                if (res.status !== 'ok') {
                  console.warn('tmux_select_window failed:', res.error)
                }
              })
            }
          }
          return
        }
      }
    }
    document.addEventListener('keydown', handler, true)
    return () => document.removeEventListener('keydown', handler, true)
  }, [windowList, activeWindow, activeWorkspace, activeHostId, toggleSidebar])

  // ---------- active-window focus reporting ----------
  // Tell the backend which (host, window) the user is looking at, so
  // its notifications post-processor can suppress inbox rows for that
  // window. Updates whenever the active host or active window changes,
  // and clears when the helm window itself loses OS focus or is
  // minimized — backgrounded windows then start collecting inbox rows
  // normally.
  useEffect(() => {
    const push = () => {
      // Treat a hidden helm as "no focus" so notifications resume for
      // every window while the user is in another app. visibilitychange
      // covers the macOS Cmd+H / minimize / different-desktop cases.
      if (document.hidden || !activeHostId || !activeWindow) {
        void commands.setFocus(null, null)
        return
      }
      void commands.setFocus(activeHostId, activeWindow.id)
    }
    push()
    const onVis = () => push()
    document.addEventListener('visibilitychange', onVis)
    window.addEventListener('blur', onVis)
    window.addEventListener('focus', onVis)
    return () => {
      document.removeEventListener('visibilitychange', onVis)
      window.removeEventListener('blur', onVis)
      window.removeEventListener('focus', onVis)
    }
  }, [activeHostId, activeWindow])

  // ---------- latency probe ----------
  // Time a cheap tmux command (list-sessions with the smallest possible
  // format) to gauge round-trip on the active host. Only run for remote
  // hosts that are actually connected — local is always ~0 and a probe
  // on a disconnected host would just queue up failures.
  useEffect(() => {
    if (!activeHostId) return
    const host = hosts.get(activeHostId)
    if (!host || host.port === 0) return
    if (activeStatus !== 'connected' && activeStatus !== 'idle') return
    const observe = useStore.getState().observeHostLatency
    const probe = async () => {
      const start = performance.now()
      const res = await commands.tmuxListSessions(activeHostId, '#{session_id}')
      if (res.status === 'ok') {
        observe(activeHostId, performance.now() - start)
      }
    }
    void probe()
    const id = window.setInterval(() => void probe(), 4000)
    return () => window.clearInterval(id)
  }, [activeHostId, activeStatus, hosts])

  return (
    <div className="flex h-screen w-screen flex-col overflow-hidden bg-canvas text-text-primary">
      {/* drag bar — title centered to the geometric middle of the window
          (macOS convention) via absolute positioning so the traffic-light
          reservation and right-side affordances don't bias it off-center. */}
      <div className="relative flex h-10 items-center justify-end border-b border-white/[0.06] bg-sidebar pl-[76px] pr-3">
        <span className="pointer-events-none absolute left-1/2 -translate-x-1/2 text-[11px] text-text-tertiary">
          {activeHost?.name ?? '—'} · {activeWorkspace?.name ?? '—'} ·{' '}
          {activeWindow?.name ?? '—'}
        </span>
        <span className="font-mono text-[10px] text-text-tertiary">
          {bootstrap.ready ? '⌘K Search' : '…booting'}
        </span>
      </div>

      <div className="relative flex-1 overflow-hidden">
        {/* The sidebar is styled as a floating panel (rounded, shadowed)
            but its width still pushes the terminal pane: the shell's
            cursor sits flush to the sidebar's right edge, not behind it.
            Sidebar margins: left=8, width=collapsed?48:280, gap=8 →
            terminal padding-left = 8 + W + 8 = W + 16. */}
        <Sidebar
          onAddHost={() => {
            setEditing(null)
            setEditorOpen(true)
          }}
          onEditHost={(h) => {
            setEditing(h)
            setEditorOpen(true)
          }}
          onDeleteHost={(h) => void deleteHost(h)}
          onCreateWorkspace={(hostId) => void createWorkspace(hostId)}
          onKillWorkspace={(hostId, w) => void killWorkspace(hostId, w)}
        />

        <main
          className="absolute top-0 right-0 bottom-0 flex overflow-hidden"
          style={{
            left: sidebarCollapsed ? 64 : 296,
            transition: 'left 180ms cubic-bezier(0.2, 0.7, 0.2, 1)',
          }}
        >
          {/* Keep-alive pane stack: one TmuxPane per pane the user has
              ever visited; only the active one is visible. Hidden panes
              continue to receive live output and keep their xterm
              buffer warm, so switching back is instant. */}
          {[...mountedPaneKeys].map((key) => {
            const sep = key.indexOf('::')
            const hostId = key.slice(0, sep)
            const paneId = key.slice(sep + 2)
            const isVisible = key === activePaneKey
            return (
              <div
                key={key}
                className="absolute inset-0 flex"
                // `display: none` on hidden panes stops the browser from
                // laying them out (and stops their ResizeObserver from
                // firing spurious resizes); the xterm + subscription
                // keep working in memory.
                style={{ display: isVisible ? 'flex' : 'none' }}
              >
                <TmuxPane hostId={hostId} paneId={paneId} isVisible={isVisible} />
              </div>
            )
          })}
          {!activePaneKey && (
            <div className="flex flex-1 items-center justify-center font-mono text-[12px] text-text-tertiary">
              {emptyStatePaneText(
                bootError,
                activeHostId ? hostErrors.get(activeHostId) ?? null : null,
                activeStatus,
                activeHostSessions,
              )}
            </div>
          )}
          {activeHost && activeStatus === 'reconnecting' && (
            <ReconnectingOverlay host={activeHost} />
          )}
          <NotificationPeek />
        </main>
      </div>

      <footer className="flex h-7 items-center border-t border-white/[0.08] bg-sidebar px-1">
        <StatusBarHostSegment
          hostName={activeHost?.name ?? '—'}
          state={
            activeHost
              ? displayedHostStatus(activeHost, activeStatus, activeHostSessions?.detachedReason ?? null)
              : 'disconnected'
          }
        />
        <StatusBarDivider />
        <StatusBarSegment>
          <span className="font-mono text-[11px] text-text-secondary">◫</span>
          <span className="text-[11px] text-text-primary">{activeWorkspace?.name ?? '—'}</span>
        </StatusBarSegment>
        <StatusBarDivider />
        <StatusBarSegment>
          <span className="font-mono text-[11px] text-text-secondary">▢</span>
          <span className="text-[11px] text-text-primary">{activeWindow?.name ?? '—'}</span>
        </StatusBarSegment>
        {activePane?.cwd && (
          <>
            <StatusBarDivider />
            <StatusBarSegment>
              <span className="font-mono text-[11px] text-text-secondary">
                {prettyPath(activePane.cwd)}
              </span>
            </StatusBarSegment>
          </>
        )}
        <span className="flex-1" />
        {activePane?.branch && (
          <>
            <StatusBarSegment>
              <span className="font-mono text-[11px] text-text-secondary">⎇</span>
              <span className="font-mono text-[11px] text-text-tertiary">
                {activePane.branch}
              </span>
            </StatusBarSegment>
            <StatusBarDivider />
          </>
        )}
        {activeHost && (activeStatus === 'connected' || activeStatus === 'idle') && (
          <StatusBarSegment>
            <span className="font-mono text-[11px] text-text-secondary">⇄</span>
            <span className="font-mono text-[11px] text-text-tertiary">
              {activeHost.port === 0
                ? 'local'
                : formatLatency(hostLatencies.get(activeHost.id))}
            </span>
          </StatusBarSegment>
        )}
      </footer>

      <HostEditorModal
        open={editorOpen}
        initial={editing ?? undefined}
        onClose={() => setEditorOpen(false)}
        onSaved={(id) => {
          // Make the just-saved/edited host active so the user can
          // immediately connect with the next click.
          setActiveHost(id)
        }}
      />

      <HostKeyPromptModal />

      <IntegrationSuggestionHost />
      <ToastHost />
    </div>
  )
}

/** Cmd+Shift+I handler — jumps to the oldest inbox notification. */
function jumpToOldestNotification(): void {
  const state = useStore.getState()
  let oldest: { hostId: string; windowId: string; createdAt: number } | null = null
  for (const n of state.notifications.values()) {
    if (oldest === null || n.created_at < oldest.createdAt) {
      oldest = { hostId: n.host_id, windowId: n.window_id, createdAt: n.created_at }
    }
  }
  if (!oldest) return
  state.setActiveHost(oldest.hostId)
  const hs = state.sessions.get(oldest.hostId)
  const ws = workspaceForWindow(hs, oldest.windowId)
  if (ws) {
    state.setActiveWindow(oldest.hostId, ws.id, oldest.windowId)
    void selectWorkspace(oldest.hostId, ws.id)
  }
  void commands.tmuxSelectWindow(oldest.hostId, oldest.windowId)
}

/** Cmd+] / Cmd+[ in Pinned mode — walk the user's pinned working set
 * across hosts. Skips stale/loading pins so the user can't get stuck on
 * a pin whose window doesn't exist anymore; if every pin is unreachable
 * (rare), we just leave the active selection alone. */
async function cyclePinnedWindow(dir: 1 | -1): Promise<void> {
  const state = useStore.getState()
  const pins = state.pinnedWindows
  if (pins.length === 0) return

  // Resolve which pin (if any) is currently active so we can step from it.
  const curIdx = pins.findIndex((p) => {
    if (p.hostId !== state.activeHostId) return false
    const hs = state.sessions.get(p.hostId)
    if (!hs) return false
    const ws = [...hs.workspaces.values()].find((w) => w.name === p.workspaceName)
    if (!ws || hs.activeWorkspaceId !== ws.id) return false
    const win = ws.windows.get(p.windowId)
    return !!win && win.active
  })

  // Try every pin once starting from the next slot — skipping any that
  // don't resolve so we don't land on a stale row.
  const start = curIdx === -1 ? (dir > 0 ? 0 : pins.length - 1) : curIdx + dir
  for (let i = 0; i < pins.length; i++) {
    const idx = ((start + i * dir) % pins.length + pins.length) % pins.length
    const target = pins[idx]
    const hs = state.sessions.get(target.hostId)
    const ws = hs ? [...hs.workspaces.values()].find((w) => w.name === target.workspaceName) : undefined
    const win = ws?.windows.get(target.windowId)
    if (!ws || !win) continue // stale or loading; skip

    state.setActiveHost(target.hostId)
    state.setActiveWindow(target.hostId, ws.id, win.id)
    await selectWorkspace(target.hostId, ws.id)
    void commands.tmuxSelectWindow(target.hostId, win.id)
    return
  }
}

/** Pick a free `workspace N` name and create it on the host. */
async function createWorkspace(hostId: string): Promise<void> {
  const state = useStore.getState()
  const hs = state.sessions.get(hostId)
  const used = new Set<number>()
  if (hs) {
    for (const w of hs.workspaces.values()) {
      const m = w.name.match(/^workspace (\d+)$/)
      if (m) used.add(parseInt(m[1], 10))
    }
  }
  let n = 1
  while (used.has(n)) n++
  const name = `workspace ${n}`

  // If the host isn't connected, connect first — passing `name` as the
  // bootstrap workspace so if no sessions exist on the server, we create
  // exactly this one (instead of a stray `main` session). When other
  // sessions DO exist, attach succeeds and `name` doesn't get created
  // by the bootstrap; we'll create it via tmux_new_session below.
  const status = state.statuses.get(hostId)
  if (status !== 'connected' && status !== 'idle') {
    try {
      await connectHost(hostId, name)
    } catch (e) {
      console.error('connect failed:', e)
      return
    }
  }

  // After connect (or if we were already connected): see if our named
  // workspace exists. The bootstrap may have created it; if not, we
  // create it explicitly.
  const post = useStore.getState().sessions.get(hostId)
  let workspaceId: string | undefined
  if (post) {
    for (const w of post.workspaces.values()) {
      if (w.name === name) {
        workspaceId = w.id
        break
      }
    }
  }
  if (!workspaceId) {
    const res = await commands.tmuxNewSession(hostId, name)
    if (res.status !== 'ok') {
      console.error('new-session failed:', res.error)
      return
    }
    workspaceId = res.data
  }
  await selectWorkspace(hostId, workspaceId)
}

/** Schedule a workspace kill with a 5s undo window. The toast carries the
 * actual kill as its deferred action; the Undo button dismisses the toast
 * and the kill never fires. The active-workspace fallback is handled in
 * setWorkspaces — when the killed one disappears from the incoming list,
 * we pick the first remaining workspace, or null. */
async function killWorkspace(hostId: string, workspace: TmuxWorkspace): Promise<void> {
  // Coalesce by `${hostId}::${workspace.id}` so re-clicking the same
  // workspace just resets the timer rather than stacking toasts.
  const toastId = `kill-workspace::${hostId}::${workspace.id}`
  const { pushToast } = useStore.getState()
  pushToast({
    id: toastId,
    message: `Killing workspace "${workspace.name}"…`,
    durationMs: 5_000,
    deferredAction: () => {
      void commands.tmuxKillSession(hostId, workspace.id)
    },
    action: { label: 'Undo', onClick: () => {} },
  })
}

/** Remove a remote host from the registry. Disconnects (if connected),
 * clears its Keychain entry, and rewrites hosts.json. Confirmation
 * dialog because this is destructive — the user's saved password and
 * other host metadata are gone. */
async function deleteHost(host: Host): Promise<void> {
  const ok = window.confirm(
    `Delete host "${host.name}"? This removes it from your saved list and clears any stored password. tmux sessions on the remote machine are unaffected.`,
  )
  if (!ok) return
  await commands.hostDelete(host.id)
}

/** Render a tmux-command round-trip latency for the connection segment.
 * Sub-millisecond samples (effectively in-process queue) show as `<1ms`
 * so they don't render as a flickery "0ms". Anything ≥ 1s switches to
 * seconds with one decimal — at that point the user cares about magnitude
 * more than precision. Pre-first-sample shows an em-dash. */
function formatLatency(ms: number | undefined): string {
  if (ms === undefined) return '—'
  if (ms < 1) return '<1ms'
  if (ms < 1000) return `${Math.round(ms)}ms`
  return `${(ms / 1000).toFixed(1)}s`
}

/** Replace a `/Users/<user>` or `/home/<user>` prefix with `~`. We can't
 * read $HOME from the renderer so we pattern-match the conventional
 * roots; that covers macOS and Linux for both local and remote hosts.
 * Anything else (Windows paths, jails, weird mount points) falls
 * through unchanged. */
function prettyPath(p: string): string {
  if (!p) return ''
  const m = p.match(/^\/(?:Users|home|root)\/[^/]+(\/.*)?$/)
  if (m) return '~' + (m[1] ?? '')
  if (p === '/root' || p.startsWith('/Users/') || p.startsWith('/home/')) return p
  return p
}

function emptyStateText(hs: HostSessions | undefined): string {
  if (!hs) return 'opening session…'
  if (hs.workspaces.size === 0) return 'no workspaces. press ⌘⇧T to create one.'
  if (!hs.activeWorkspaceId) return 'select a workspace from the sidebar.'
  return 'opening session…'
}

/** Empty-state copy for the pane area when no pane is active. Surfaces
 * boot errors and per-host errors (Error status with stashed message)
 * so a stuck localhost reads "error · tmux not found" instead of an
 * upbeat "no workspaces" prompt that hides the real failure. */
function emptyStatePaneText(
  bootError: string | null,
  hostError: string | null,
  status: HostStatus | undefined,
  hs: HostSessions | undefined,
): string {
  if (bootError) return `error · ${bootError}`
  if (status === 'error' && hostError) return `error · ${hostError}`
  return emptyStateText(hs)
}

function neighbourWindowId(
  windowList: { id: string }[],
  currentId: string | undefined,
  direction: 1 | -1,
): string | undefined {
  if (windowList.length < 2 || !currentId) return undefined
  const stable = [...windowList].sort((a, b) => a.id.localeCompare(b.id))
  const idx = stable.findIndex((w) => w.id === currentId)
  if (idx < 0) return undefined
  const next = (idx + direction + stable.length) % stable.length
  return stable[next].id
}


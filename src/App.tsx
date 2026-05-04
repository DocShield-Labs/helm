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
  sortById,
  useStore,
  type HostSessions,
  type TmuxWorkspace,
} from '@lib/store'
import { connectHost, subscribeHostEvents } from '@lib/host'
import { displayedHostStatus } from '@lib/host-status'
import { useGlobalKeymap } from '@lib/keymap-engine'
import { createWorkspace, killWorkspace } from '@lib/actions/workspace'
import { TmuxPane } from '@features/shell/TmuxPane'
import { HostEditorModal } from '@features/host-editor/HostEditorModal'
import { HostKeyPromptModal } from '@features/host-key/HostKeyPromptModal'
import { IntegrationSuggestionHost } from '@features/activity-feed/IntegrationSuggestionHost'
import { NotificationPeek } from '@features/activity-feed/NotificationPeek'
import { ReconnectingOverlay } from '@features/workspace/ReconnectingOverlay'
import { PaletteHost } from '@features/palette/PaletteHost'
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
  const windowList = useMemo(
    () => (activeWorkspace ? sortById(activeWorkspace.windows.values()) : []),
    [activeWorkspace],
  )

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
  // The engine reads `STATIC_ACTIONS` from the registry, layers user
  // overrides, and dispatches at document level. xterm vetoes Cmd+ at
  // the terminal layer (terminal/index.ts:69-71) so global combos
  // always reach us.
  useGlobalKeymap()

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
      <PaletteHost />
      <ToastHost />
    </div>
  )
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


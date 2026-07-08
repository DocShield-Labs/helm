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
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { commands } from '@lib/ipc'
import {
  sortById,
  useStore,
  type HostSessions,
  type TmuxWorkspace,
} from '@lib/store'
import { connectHost, subscribeHostEvents } from '@lib/host'
import { displayedHostStatus } from '@lib/host-status'
import { useAppUpdate } from '@lib/updater'
import { useGlobalKeymap } from '@lib/keymap-engine'
import {
  applyThemeCssVars,
  getTheme,
  setThemeForAllTerminals,
} from '@lib/terminal'
import { createWorkspace, killWorkspace } from '@lib/actions/workspace'
import { TmuxPane } from '@features/shell/TmuxPane'
import { HostEditorModal } from '@features/host-editor/HostEditorModal'
import { ScheduleEditorModal } from '@features/schedule/ScheduleEditorModal'
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
  ConfirmHost,
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

  // Single subscriber: push the active palette into the chrome CSS
  // variables and fan out to every attached xterm. `previewThemeName`
  // wins so the picker can show live previews; the palette clears it
  // on close (Esc reverts, Enter persists).
  const themeName = useStore((s) => s.themeName)
  const previewThemeName = useStore((s) => s.previewThemeName)
  useEffect(() => {
    const theme = getTheme(previewThemeName ?? themeName)
    applyThemeCssVars(theme)
    setThemeForAllTerminals(theme)
  }, [themeName, previewThemeName])

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

  // ---------- file drag-and-drop → active pane ----------
  // Tauri's WebView swallows native HTML5 drop events and re-emits them
  // as `tauri://drag-drop` carrying real filesystem paths. Type each
  // dropped path into the active pane via the same `tmux_send_keys`
  // path as a keystroke, with iTerm2-style backslash escaping so a
  // shell or a TUI like Claude Code both receive it as if typed.
  //
  // The focus check skips xterm's hidden helper textarea, which is the
  // active element whenever the terminal has focus — exactly when we
  // want a drop to land. Real text inputs (host editor, palette) still
  // suppress the drop so it doesn't silently type into a hidden pane.
  useEffect(() => {
    if (!activeHostId || !activePane) return
    const hostId = activeHostId
    const paneId = activePane.id

    let unlisten: (() => void) | undefined
    let cancelled = false
    void (async () => {
      const fn = await getCurrentWebview().onDragDropEvent((event) => {
        if (event.payload.type !== 'drop') return
        const active = document.activeElement as HTMLElement | null
        if (
          active &&
          !active.classList.contains('xterm-helper-textarea') &&
          (active.tagName === 'INPUT' ||
            active.tagName === 'TEXTAREA' ||
            active.isContentEditable)
        ) {
          return
        }
        const paths = event.payload.paths
        if (!paths || paths.length === 0) return
        const text = paths.map(escapeShellPath).join(' ') + ' '
        const bytes = Array.from(new TextEncoder().encode(text))
        void commands.tmuxSendKeys(hostId, paneId, bytes).then((res) => {
          if (res.status !== 'ok') {
            console.warn('drag-drop tmux_send_keys failed:', res.error)
          }
        })
      })
      if (cancelled) {
        fn()
      } else {
        unlisten = fn
      }
    })()

    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [activeHostId, activePane])

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

  // Self-update: non-null when a newer signed release is available.
  const appUpdate = useAppUpdate()

  return (
    <div className="flex h-screen w-screen flex-col overflow-hidden bg-canvas text-text-primary">
      {/* drag bar — title centered to the geometric middle of the window
          (macOS convention) via absolute positioning so the traffic-light
          reservation and right-side affordances don't bias it off-center.
          `data-tauri-drag-region` makes the bar draggable; with the Overlay
          title-bar style the OS has no native title bar to grab, so this
          attribute is what lets the user move the window. */}
      <div
        data-tauri-drag-region
        className="relative flex h-10 items-center justify-end border-b border-white/[0.06] bg-sidebar pl-[76px] pr-3"
      >
        <span
          data-tauri-drag-region
          className="pointer-events-none absolute left-1/2 -translate-x-1/2 text-[12px] text-text-tertiary"
        >
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
            <PaneEmptyState
              bootError={bootError}
              hostError={activeHostId ? hostErrors.get(activeHostId) ?? null : null}
              status={activeStatus}
              hs={activeHostSessions}
            />
          )}
          {activeHost && activeStatus === 'reconnecting' && (
            <ReconnectingOverlay host={activeHost} />
          )}
          <NotificationPeek />
        </main>
      </div>

      <footer className="flex h-8 items-center border-t border-white/[0.08] bg-sidebar px-1">
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
          <span className="font-mono text-[12px] text-text-secondary">◫</span>
          <span className="text-[12px] text-text-primary">{activeWorkspace?.name ?? '—'}</span>
        </StatusBarSegment>
        <StatusBarDivider />
        <StatusBarSegment>
          <span className="font-mono text-[12px] text-text-secondary">▢</span>
          <span className="text-[12px] text-text-primary">{activeWindow?.name ?? '—'}</span>
        </StatusBarSegment>
        {activePane?.cwd && (
          <>
            <StatusBarDivider />
            <StatusBarSegment
              onClick={
                activeHost?.port === 0
                  ? () => void commands.revealInFinder(activePane.cwd!)
                  : undefined
              }
              title={activeHost?.port === 0 ? 'Reveal in Finder' : undefined}
            >
              <span className="font-mono text-[12px] text-text-secondary">
                {prettyPath(activePane.cwd)}
              </span>
            </StatusBarSegment>
          </>
        )}
        <span className="flex-1" />
        {appUpdate && (
          <>
            <StatusBarSegment
              onClick={appUpdate.installing ? undefined : appUpdate.install}
              title={`Install Helm ${appUpdate.version} and relaunch — tmux sessions survive`}
            >
              <span className="font-mono text-[12px] text-text-secondary">⬆</span>
              <span className="text-[12px] text-text-primary">
                {appUpdate.installing
                  ? `installing ${appUpdate.version}…`
                  : `${appUpdate.version} available`}
              </span>
            </StatusBarSegment>
            <StatusBarDivider />
          </>
        )}
        {activePane?.branch && (
          <>
            <StatusBarSegment>
              <span className="font-mono text-[12px] text-text-secondary">⎇</span>
              <span className="font-mono text-[12px] text-text-tertiary">
                {activePane.branch}
              </span>
            </StatusBarSegment>
            <StatusBarDivider />
          </>
        )}
        {activeHost && (activeStatus === 'connected' || activeStatus === 'idle') && (
          <StatusBarSegment>
            <span className="font-mono text-[12px] text-text-secondary">⇄</span>
            <span className="font-mono text-[12px] text-text-tertiary">
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
      <ScheduleEditorModal />

      <IntegrationSuggestionHost />
      <PaletteHost />
      <ConfirmHost />
      <ToastHost />
    </div>
  )
}

/** Remove a remote host from the registry. Disconnects (if connected),
 * clears its Keychain entry, and rewrites hosts.json. Confirmation
 * dialog because this is destructive — the user's saved password and
 * other host metadata are gone. The Rust side emits HostRemoved before
 * persistence runs, so the UI updates either way; a toast surfaces the
 * persistence error if the on-disk write failed. */
async function deleteHost(host: Host): Promise<void> {
  const ok = await useStore.getState().requestConfirm({
    title: `Delete host "${host.name}"?`,
    message:
      'This removes it from your saved list and clears any stored password. tmux sessions on the remote machine are unaffected.',
    confirmLabel: 'Delete',
    destructive: true,
  })
  if (!ok) return
  let res: Awaited<ReturnType<typeof commands.hostDelete>>
  try {
    res = await commands.hostDelete(host.id)
  } catch (e) {
    useStore.getState().pushToast({
      id: `host-delete-error::${host.id}`,
      message: `Delete threw: ${String(e)}`,
      durationMs: 8_000,
    })
    return
  }
  if (res.status !== 'ok') {
    useStore.getState().pushToast({
      id: `host-delete-error::${host.id}`,
      message: `Couldn't fully delete "${host.name}": ${res.error}`,
      durationMs: 8_000,
    })
  }
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

/** Backslash-escape characters in a filesystem path that would otherwise
 * be interpreted by a POSIX shell — spaces, quotes, parens, glob metas,
 * etc. Mirrors iTerm2's default drag-drop escaping. Non-shell receivers
 * (Claude Code, REPLs) still parse this fine because they normalize
 * backslash escapes when extracting paths from typed input. */
function escapeShellPath(p: string): string {
  return p.replace(/([ '"\\$`!*?(){}[\]<>;&|#~])/g, '\\$1')
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
  if (!hs) return 'Opening session…'
  if (hs.workspaces.size === 0) return 'No workspaces yet.'
  if (!hs.activeWorkspaceId) return 'Select a workspace from the sidebar.'
  return 'Opening session…'
}

/** Centered empty state for the pane area when no pane is active.
 * Surfaces boot/host errors prominently (red, no hints) so a stuck
 * localhost reads "error · tmux not found" instead of an upbeat prompt
 * that hides the real failure. For benign "nothing here yet" states it
 * adds a quiet row of keyboard hints so a first-run user knows where to
 * start. */
function PaneEmptyState({
  bootError,
  hostError,
  status,
  hs,
}: {
  bootError: string | null
  hostError: string | null
  status: HostStatus | undefined
  hs: HostSessions | undefined
}) {
  const isError = !!bootError || (status === 'error' && !!hostError)
  const message = bootError
    ? `error · ${bootError}`
    : status === 'error' && hostError
      ? `error · ${hostError}`
      : emptyStateText(hs)
  const showHints = !isError && status !== 'connecting' && status !== 'reconnecting'
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-5 px-8 text-center">
      <div
        className={`font-mono text-[13px] ${isError ? 'text-status-error' : 'text-text-secondary'}`}
      >
        {message}
      </div>
      {showHints && (
        <div className="flex flex-wrap items-center justify-center gap-x-5 gap-y-2">
          <EmptyHint keys={['⌘', 'K']} label="commands" />
          <EmptyHint keys={['⌘', 'T']} label="new window" />
          <EmptyHint keys={['⌘', '\\']} label="toggle sidebar" />
        </div>
      )}
    </div>
  )
}

function EmptyHint({ keys, label }: { keys: string[]; label: string }) {
  return (
    <span className="flex items-center gap-1.5 text-[12px] text-text-tertiary">
      <span className="flex gap-0.5">
        {keys.map((k, i) => (
          <kbd
            key={i}
            className="rounded border border-white/[0.08] bg-white/[0.03] px-1.5 py-0.5 font-mono text-[11px] leading-none text-text-secondary"
          >
            {k}
          </kbd>
        ))}
      </span>
      {label}
    </span>
  )
}


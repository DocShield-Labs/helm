/**
 * Phase 2 / multi-workspace landing.
 *
 * Boots the global event channel, lists hosts, picks localhost as the
 * active host, connects, and renders each host's workspaces in the
 * sidebar. Each host can have many workspaces; each workspace owns its
 * windows; one window in the active workspace renders its active pane
 * as a block list + live tail (BlockPane).
 *
 * Selection model (all in store.ts, purely frontend):
 *   - activeHostId            — which host's tree drives the sidebar
 *   - per-host activeWorkspaceId — within a host, which workspace's windows show
 *   - per-window / per-pane active flags
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
import { useAppUpdate } from '@lib/updater'
import { useGlobalKeymap } from '@lib/keymap-engine'
import {
  applyThemeCssVars,
  getTheme,
  setThemeForAllTerminals,
} from '@lib/terminal'
import { BlockPane } from '@features/shell/BlockPane'
import { sendInput } from '@lib/session/stream'
import { HostEditorModal } from '@features/host-editor/HostEditorModal'
import { ScheduleEditorModal } from '@features/schedule/ScheduleEditorModal'
import { HostKeyPromptModal } from '@features/host-key/HostKeyPromptModal'
import { IntegrationSuggestionHost } from '@features/activity-feed/IntegrationSuggestionHost'
import { NotificationPeek } from '@features/activity-feed/NotificationPeek'
import { ReconnectingOverlay } from '@features/workspace/ReconnectingOverlay'
import { PaletteHost } from '@features/palette/PaletteHost'
import { ToastHost, ConfirmHost, TopBar } from '@ui'
import { Sidebar } from '@features/sessions/Sidebar'
import type { Host, HostStatus } from '@bindings'

// Module-level flag: only run the boot chain once per process. A
// component-scoped `useRef` doesn't work here because React 19's
// StrictMode mounts → unmounts → re-mounts the component in dev,
// creating a fresh ref on the second mount; the boot chain would
// then run twice and double-connect localhost. Module state survives
// mount/unmount cycles within the
// same process, so the second mount's effect sees `true` and bails.
let bootStarted = false

export function App() {
  const setBootstrap = useStore((s) => s.setBootstrap)
  const hosts = useStore((s) => s.hosts)
  const statuses = useStore((s) => s.statuses)
  const sessions = useStore((s) => s.sessions)
  const activeHostId = useStore((s) => s.activeHostId)
  const setHosts = useStore((s) => s.setHosts)
  const setActiveHost = useStore((s) => s.setActiveHost)
  const hostErrors = useStore((s) => s.hostErrors)
  const [bootError, setBootError] = useState<string | null>(null)
  // Host-editor modal state. `editing` carries the host being edited
  // (or null for "add new").
  const [editorOpen, setEditorOpen] = useState(false)
  const [editing, setEditing] = useState<Host | null>(null)
  // Panes the user has activated at least once. Mounted BlockPane instances
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

  // Stable order by id within a workspace.
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

  // Garbage-collect mounted panes whose pane id no longer exists
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
  // dropped path into the active pane via the same `session_input`
  // path as a keystroke, with iTerm2-style backslash escaping so a
  // shell or a TUI like Claude Code both receive it as if typed.
  //
  // Lands in the composer when it has focus, else in the terminal
  // (xterm's hidden helper textarea is the active element then). Other
  // text inputs (host editor, palette) suppress the drop.
  useEffect(() => {
    if (!activeHostId || !activePane) return
    const hostId = activeHostId
    const paneId = activePane.id

    let unlisten: (() => void) | undefined
    let cancelled = false
    void (async () => {
      const fn = await getCurrentWebview().onDragDropEvent((event) => {
        if (event.payload.type !== 'drop') return
        const paths = event.payload.paths
        if (!paths || paths.length === 0) return
        const text = paths.map(escapeShellPath).join(' ') + ' '
        const active = document.activeElement as HTMLElement | null
        // The composer is the input at a prompt: insert there (as a
        // native edit, so React sees it). Other text fields swallow
        // the drop rather than typing into a hidden pane.
        if (active?.classList.contains('helm-composer-editor')) {
          document.execCommand('insertText', false, text)
          return
        }
        if (
          active &&
          !active.classList.contains('xterm-helper-textarea') &&
          (active.tagName === 'INPUT' ||
            active.tagName === 'TEXTAREA' ||
            active.isContentEditable)
        ) {
          return
        }
        void sendInput(hostId, paneId, text)
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

  // Self-update: non-null when a newer signed release is available.
  const appUpdate = useAppUpdate()

  const title = [activeHost?.name, activeWindow?.name].filter(Boolean).join(' — ')

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-canvas text-text-primary">
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
      />

      <div className="flex min-w-0 flex-1 flex-col">
        <TopBar title={title} update={appUpdate} />
        <main className="relative flex flex-1 overflow-hidden">
          {/* Keep-alive pane stack: one BlockPane per pane the user has
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
                <BlockPane hostId={hostId} paneId={paneId} isVisible={isVisible} />
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
      'This removes it from your saved list and clears any stored password. Sessions on the remote machine are unaffected.',
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

/** Render a round-trip latency for the connection segment.
 * Sub-millisecond samples (effectively in-process queue) show as `<1ms`
 * so they don't render as a flickery "0ms". Anything ≥ 1s switches to
 * seconds with one decimal — at that point the user cares about magnitude
 * more than precision. Pre-first-sample shows an em-dash. */
function escapeShellPath(p: string): string {
  return p.replace(/([ '"\\$`!*?(){}[\]<>;&|#~])/g, '\\$1')
}

/** Replace a `/Users/<user>` or `/home/<user>` prefix with `~`. We can't
 * read $HOME from the renderer so we pattern-match the conventional
 * roots; that covers macOS and Linux for both local and remote hosts.
 * Anything else (Windows paths, jails, weird mount points) falls
 * through unchanged. */
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


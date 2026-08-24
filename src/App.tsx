/** Boots hosts and renders the selected long-running session. */

import { useEffect, useMemo, useState } from 'react'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { commands } from '@lib/ipc'
import { useStore, type HostSessions } from '@lib/store'
import { connectHost, deleteHost, subscribeHostEvents } from '@lib/host'
import { useAppUpdate } from '@lib/updater'
import { useGlobalKeymap } from '@lib/keymap-engine'
import {
  applyThemeCssVars,
  getTheme,
  setThemeForAllTerminals,
} from '@lib/terminal'
import { SessionView } from '@features/shell/SessionView'
import { sendInput } from '@lib/session/screen'
import { HostEditorModal } from '@features/host-editor/HostEditorModal'
import { HostKeyPromptModal } from '@features/host-key/HostKeyPromptModal'
import { IntegrationSuggestionHost } from '@features/activity-feed/IntegrationSuggestionHost'
import { NotificationPeek } from '@features/activity-feed/NotificationPeek'
import { ReconnectingOverlay } from '@features/host/ReconnectingOverlay'
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
  // Keep visited sessions mounted so switching preserves warm terminal buffers.
  const [mountedSessionKeys, setMountedSessionKeys] = useState<Set<string>>(new Set())
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

  const activeSession = useMemo(() => {
    if (!activeHostSessions?.activeSessionId) return undefined
    return activeHostSessions.sessions.get(activeHostSessions.activeSessionId)
  }, [activeHostSessions])

  const activeSessionKey =
    activeHostId && activeSession ? `${activeHostId}::${activeSession.id}` : null

  useEffect(() => {
    if (!activeSessionKey) return
    setMountedSessionKeys((prev) => {
      if (prev.has(activeSessionKey)) return prev
      const next = new Set(prev)
      next.add(activeSessionKey)
      return next
    })
  }, [activeSessionKey])

  // Drop terminal instances after their underlying session disappears.
  useEffect(() => {
    setMountedSessionKeys((prev) => {
      const next = new Set<string>()
      for (const key of prev) {
        const sep = key.indexOf('::')
        const hostId = key.slice(0, sep)
        const sessionId = key.slice(sep + 2)
        const hs = sessions.get(hostId)
        if (hs?.sessions.has(sessionId)) next.add(key)
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

  // ---------- active-session focus reporting ----------
  // Tell the backend which session the user is looking at, so
  // its notifications post-processor can suppress inbox rows for that
  // session. Updates whenever the active host or session changes, and
  // clears when the Helm window itself loses OS focus or is minimized.
  // normally.
  useEffect(() => {
    const push = () => {
      // Treat a hidden helm as "no focus" so notifications resume for
      // every session while the user is in another app. visibilitychange
      // covers the macOS Cmd+H / minimize / different-desktop cases.
      if (document.hidden || !activeHostId || !activeSession) {
        void commands.setFocus(null, null)
        return
      }
      void commands.setFocus(activeHostId, activeSession.id)
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
  }, [activeHostId, activeSession])

  // ---------- file drag-and-drop → active session ----------
  // Tauri's WebView swallows native HTML5 drop events and re-emits them
  // as `tauri://drag-drop` carrying real filesystem paths. Type each
  // dropped path into the active session via the same `session_input`
  // path as a keystroke, with iTerm2-style backslash escaping so a
  // shell or a TUI like Claude Code both receive it as if typed.
  //
  // Lands in the composer when it has focus, else in the terminal
  // (xterm's hidden helper textarea is the active element then). Other
  // text inputs (host editor, palette) suppress the drop.
  useEffect(() => {
    if (!activeHostId || !activeSession) return
    const hostId = activeHostId
    const sessionId = activeSession.id

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
        // the drop rather than typing into a hidden session.
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
        void sendInput(hostId, sessionId, text)
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
  }, [activeHostId, activeSession])

  // Self-update: non-null when a newer signed release is available.
  const appUpdate = useAppUpdate()

  const title = [activeHost?.name, activeSession?.name].filter(Boolean).join(' — ')

  return (
    // Column: the top bar spans the full window; the sidebar and session
    // area share the row beneath it.
    <div className="flex h-screen w-screen flex-col overflow-hidden bg-canvas text-text-primary">
      <TopBar title={title} update={appUpdate} />
      <div className="flex min-h-0 flex-1">
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

        <main className="relative flex min-w-0 flex-1 overflow-hidden">
          {/* Keep-alive session stack: one terminal per session the user has
              ever visited; only the active one is visible. Hidden sessions
              continue to receive live output and keep their xterm
              buffer warm, so switching back is instant. */}
          {[...mountedSessionKeys].map((key) => {
            const sep = key.indexOf('::')
            const hostId = key.slice(0, sep)
            const sessionId = key.slice(sep + 2)
            const isVisible = key === activeSessionKey
            return (
              <div
                key={key}
                className="absolute inset-0 flex"
                // `display: none` on hidden sessions stops the browser from
                // laying them out (and stops their ResizeObserver from
                // firing spurious resizes); the xterm + subscription
                // keep working in memory.
                style={{ display: isVisible ? 'flex' : 'none' }}
              >
                <SessionView hostId={hostId} sessionId={sessionId} isVisible={isVisible} />
              </div>
            )
          })}
          {!activeSessionKey && (
            <SessionEmptyState
              bootError={bootError}
              hostError={activeHostId ? hostErrors.get(activeHostId) ?? null : null}
              status={activeStatus}
              hs={activeHostSessions}
              host={activeHost}
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

      <IntegrationSuggestionHost />
      <PaletteHost />
      <ConfirmHost />
      <ToastHost />
    </div>
  )
}

/** Quote a path for insertion into the active shell. */
function escapeShellPath(p: string): string {
  return p.replace(/([ '"\\$`!*?(){}[\]<>;&|#~])/g, '\\$1')
}

/** Message for a reachable host without an active session. */
function emptyStateText(hs: HostSessions | undefined): string {
  if (!hs) return 'Opening session…'
  if (hs.sessions.size === 0) return 'No sessions yet.'
  if (!hs.activeSessionId) return 'Select a session from the sidebar.'
  return 'Opening session…'
}

/** Centered empty state for the session area when no session is active.
 * Surfaces boot/host errors prominently (red, no hints) so a stuck
 * localhost reads "error · tmux not found" instead of an upbeat prompt
 * that hides the real failure. For benign "nothing here yet" states it
 * adds a quiet row of keyboard hints so a first-run user knows where to
 * start. */
function SessionEmptyState({
  bootError,
  hostError,
  status,
  hs,
  host,
}: {
  bootError: string | null
  hostError: string | null
  status: HostStatus | undefined
  hs: HostSessions | undefined
  host: Host | undefined
}) {
  const isError = !!bootError || (status === 'error' && !!hostError)
  // A disconnected remote has no sessions to show and can't open one —
  // say that, and offer the only thing that helps.
  const offline = !isError && !!host && host.port !== 0 && status === 'disconnected'
  const message = bootError
    ? `error · ${bootError}`
    : status === 'error' && hostError
      ? `error · ${hostError}`
      : offline
        ? `Not connected to ${host.name}.`
        : emptyStateText(hs)
  // ⌘T only means something on a host we can actually reach.
  const showHints = !isError && !offline && status !== 'connecting' && status !== 'reconnecting'
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-5 px-8 text-center">
      <div
        className={`font-mono text-[13px] ${isError ? 'text-status-error' : 'text-text-secondary'}`}
      >
        {message}
      </div>
      {offline && host && (
        <button
          type="button"
          onClick={() => void connectHost(host.id).catch(() => {})}
          className="rounded-md bg-accent-muted px-3 py-1 text-[12px] text-accent-text hover:bg-[var(--accent-border)]"
        >
          Reconnect
        </button>
      )}
      {showHints && (
        <div className="flex flex-wrap items-center justify-center gap-x-5 gap-y-2">
          <EmptyHint keys={['⌘', 'K']} label="commands" />
          <EmptyHint keys={['⌘', 'T']} label="new session" />
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

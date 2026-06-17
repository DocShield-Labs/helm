/**
 * TmuxPane — one xterm pane bound to a tmux pane via `(hostId, paneId)`.
 *
 * Stays mounted across workspace/window switches: when `isVisible` flips
 * to false, the parent hides this instance via `display: none` but never
 * unmounts it. The xterm keeps consuming live output via its
 * subscription, so switching back is instant — no re-capture, no remount,
 * no flash of empty prompt.
 *
 * Lifecycle:
 *  1. mount        → attach xterm, resize the host's tmux client, capture buffer, subscribe
 *  2. tmux output  → term.write (continues even when hidden)
 *  3. user input   → term.onData → tmux_send_keys
 *  4. xterm size   → term.onResize → tmux_resize_client
 *  5. visible flip → re-fit + refocus
 *  6. unmount      → abort in-flight async, dispose terminal
 *
 * Rendering is plain xterm.js — the program underneath (shell, vim, tmux,
 * a TUI, etc.) owns the grid completely, so resize and full-screen apps
 * behave exactly as they would in any terminal. OSC 133 markers are
 * consumed upstream in `host.ts` for sidebar state (cwd·branch,
 * running/idle); they do not drive any in-pane chrome.
 */

import { useEffect, useRef, useState } from 'react'
import { commands } from '@lib/ipc'
import { attachTerminal, getTheme } from '@lib/terminal'
import { subscribePaneOutput } from '@lib/host'
import { useStore } from '@lib/store'
import type { HostId } from '@bindings'
import { TerminalScrollbar } from './TerminalScrollbar'

interface TmuxPaneProps {
  hostId: HostId
  paneId: string
  /** When false, parent renders us as `display: none` — we keep our
   * xterm + subscription alive but stop processing keystrokes (so a
   * background pane doesn't eat keys meant for the visible one). */
  isVisible?: boolean
}

export function TmuxPane({ hostId, paneId, isVisible = true }: TmuxPaneProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  // Refs let the visibility effect reach into the mount-effect's scope
  // without restarting the whole pane every time `isVisible` flips.
  const termRef = useRef<ReturnType<typeof attachTerminal> | null>(null)
  const visibleRef = useRef(isVisible)
  // Mirror the attached terminal into state so the render below can mount
  // the scrollbar once xterm exists (and remount it if the term identity
  // changes on a hostId/paneId swap).
  const [helmTerm, setHelmTerm] = useState<ReturnType<typeof attachTerminal> | null>(null)

  const paneKey = `${hostId}::${paneId}`

  useEffect(() => {
    const host = hostRef.current
    if (!host) return

    const ac = new AbortController()
    const aborted = () => ac.signal.aborted
    const encoder = new TextEncoder()
    const { previewThemeName, themeName } = useStore.getState()
    const attached = attachTerminal(host, {
      theme: getTheme(previewThemeName ?? themeName),
    })
    const { term, fit, dispose } = attached
    termRef.current = attached
    setHelmTerm(attached)

    // Try the pre-fetched capture first — populated by `prehydrateCaptures`
    // after every refetchTree. Hits are instant (no IPC, no SSH RTT) but
    // pre-hydration is visible-buffer-only to keep wire bytes small;
    // we upgrade to bounded-history scrollback in the background below.
    const cached = useStore.getState().paneCaptures.get(paneKey)
    if (cached && cached.data.length > 0) {
      term.write(cached.data)
    }

    let unsub: (() => void) | null = null
    void (async () => {
      try {
        await commands.tmuxResizeClient(hostId, term.cols, term.rows)
        if (aborted()) return

        const needsFullHistory = !cached || !cached.hasScrollback
        if (needsFullHistory) {
          const cap = await commands.tmuxCapturePane(hostId, paneId, 2000)
          if (aborted()) return
          if (cap.status === 'ok' && cap.data.length > 0) {
            if (cached) term.reset()
            term.write(cap.data)
            useStore.getState().setPaneCapture(hostId, paneId, cap.data, true)
          }
        }

        // Live output. helm-tmux already stripped OSC 133 markers from
        // these bytes during forwarding (they're delivered separately and
        // consumed in host.ts for sidebar state), so a straight write is
        // all the terminal needs.
        unsub = subscribePaneOutput(hostId, paneId, (bytes) => {
          term.write(new Uint8Array(bytes))
        })
      } catch {
        /* benign — unmount races, transient IPC errors */
      }
    })()

    const inputDisp = term.onData((data) => {
      if (aborted()) return
      // Hidden panes ignore input — keystrokes belong to the visible pane.
      if (!visibleRef.current) return

      // Dismiss-on-keystroke: a real user keypress in THIS pane signals
      // they're acting on whatever notifications were sitting for this
      // window. Fire-and-forget dismiss so the inbox row disappears
      // without round-tripping through React.
      if (isUserKeystroke(data)) {
        const store = useStore.getState()
        const hs = store.sessions.get(hostId)
        let windowId: string | null = null
        if (hs) {
          for (const ws of hs.workspaces.values()) {
            const pane = ws.panes.get(paneId)
            if (pane) {
              windowId = pane.windowId
              break
            }
          }
        }
        if (windowId) {
          const hasNotif = [...store.notifications.values()].some(
            (n) =>
              n.host_id === hostId &&
              (n.window_id === windowId || n.pane_id === paneId),
          )
          if (hasNotif) {
            void commands.notificationDismissForWindow(hostId, windowId)
          }
        }
      }

      void commands.tmuxSendKeys(hostId, paneId, Array.from(encoder.encode(data))).then((res) => {
        if (res.status !== 'ok') {
          console.warn('tmux_send_keys failed:', res.error)
        }
      })
    })

    const resizeDisp = term.onResize(({ cols, rows }) => {
      if (aborted()) return
      void commands.tmuxResizeClient(hostId, cols, rows)
    })

    let resizeTimer: ReturnType<typeof setTimeout> | undefined
    const ro = new ResizeObserver(() => {
      if (resizeTimer) clearTimeout(resizeTimer)
      resizeTimer = setTimeout(() => {
        try {
          fit.fit()
        } catch {
          /* terminal may not be visible yet */
        }
      }, 50)
    })
    ro.observe(host)

    if (visibleRef.current) term.focus()

    return () => {
      ac.abort()
      if (resizeTimer) clearTimeout(resizeTimer)
      unsub?.()
      inputDisp.dispose()
      resizeDisp.dispose()
      ro.disconnect()
      dispose()
      termRef.current = null
      setHelmTerm(null)
    }
  }, [hostId, paneId, paneKey])

  // (Theme changes fan out via App.tsx's single subscriber and the
  // attached-terminals registry — no per-pane subscription needed.)

  // When the pane becomes visible after being hidden, the container has
  // gone from 0×0 (display:none) to its real dimensions. ResizeObserver
  // didn't fire, so xterm's grid is still sized to whatever it was at
  // mount time. Force a re-fit + focus so input goes here.
  useEffect(() => {
    visibleRef.current = isVisible
    if (!isVisible) return
    const attached = termRef.current
    if (!attached) return
    try {
      attached.fit.fit()
    } catch {
      /* terminal may not be ready yet */
    }
    attached.term.focus()
  }, [isVisible])

  return (
    <div className="relative h-full w-full overflow-hidden bg-[var(--terminal-bg)]">
      {/* xterm fills the whole pane; typing and rendering are native. */}
      <div
        ref={hostRef}
        className="absolute inset-0 overflow-hidden pl-6 pr-2 py-2"
      />
      {helmTerm && <TerminalScrollbar term={helmTerm.term} />}
    </div>
  )
}

/** True iff `data` represents real user input (typed character,
 * pasted text, control key, arrow, etc.) and NOT a terminal-state
 * report that xterm sends back to the host as a side-effect of
 * rendering or focus changes.
 *
 * The set of "not user input" sequences is small and well-known:
 *   - Focus enter/exit (DECSET 1004): `\x1b[I` / `\x1b[O`. Fires when
 *     we call `term.focus()` after making the pane visible.
 *   - Mouse events (DECSET 1006 SGR / DECSET 1000 X10): `\x1b[<...M`,
 *     `\x1b[<...m`, `\x1b[M...`. Mouse clicks inside the pane area.
 *   - Cursor-position reports (CPR / DSR): `\x1b[<row>;<col>R`. Sent
 *     in response to an app-issued query, not a user keypress.
 *
 * Anything else — bytes that look like keystrokes — counts as user
 * input. This keeps the dismiss-on-keystroke behaviour intuitive.
 */
function isUserKeystroke(data: string): boolean {
  if (data === '\x1b[I' || data === '\x1b[O') return false
  // SGR mouse: `ESC [ < flags ; col ; row M|m`
  if (/^\x1b\[<\d+;\d+;\d+[Mm]/.test(data)) return false
  // X10 mouse: `ESC [ M <byte><byte><byte>`
  if (data.startsWith('\x1b[M') && data.length === 6) return false
  // CPR / DSR response: `ESC [ row ; col R`
  if (/^\x1b\[\d+;\d+R$/.test(data)) return false
  return true
}

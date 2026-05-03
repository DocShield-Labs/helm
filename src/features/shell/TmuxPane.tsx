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
 *  3. user input   → term.onData → tmux_send_keys (only when visible — see below)
 *  4. xterm size   → term.onResize → tmux_resize_client
 *  5. visible flip → re-fit + refocus (browser stops firing ResizeObserver
 *                    on display:none elements, so we have to nudge it
 *                    when the container becomes visible again)
 *  6. unmount      → abort in-flight async, dispose terminal (tmux pane lives on)
 */

import { useEffect, useRef } from 'react'
import { commands } from '@lib/ipc'
import { attachTerminal } from '@lib/terminal'
import { subscribePaneOutput } from '@lib/host'
import { useStore } from '@lib/store'
import type { HostId } from '@bindings'

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

  useEffect(() => {
    const host = hostRef.current
    if (!host) return

    const ac = new AbortController()
    const aborted = () => ac.signal.aborted
    const encoder = new TextEncoder()
    const attached = attachTerminal(host)
    const { term, fit, dispose } = attached
    termRef.current = attached

    // Try the pre-fetched capture first — populated by `prehydrateCaptures`
    // after every refetchTree. Hits are instant (no IPC, no SSH RTT) but
    // pre-hydration is visible-buffer-only to keep wire bytes small;
    // we upgrade to bounded-history scrollback in the background below.
    const cached = useStore.getState().paneCaptures.get(`${hostId}::${paneId}`)
    if (cached && cached.data.length > 0) {
      term.write(cached.data)
    }

    // Then in the background:
    //   1. Resize the control client so tmux's pane matches xterm's width
    //      (cheap — usually a no-op since pre-hydration sized things
    //      already, but enforces correctness for the first connect or
    //      after a window resize).
    //   2. If we don't already have full scrollback for this pane,
    //      fetch it. Two cases:
    //        - cache miss        → pane appeared after pre-hydration
    //          (e.g. just created); pull `-S -` from scratch.
    //        - cache hit, partial → reset xterm and rewrite with full
    //          history. Brief flicker, but the user gets real
    //          scrollback they can page up through.
    //      The result is cached as `hasScrollback: true` so subsequent
    //      mounts of the same pane skip this round-trip.
    //   3. Subscribe to live output going forward.
    let unsub: (() => void) | null = null
    void (async () => {
      try {
        await commands.tmuxResizeClient(hostId, term.cols, term.rows)
        if (aborted()) return

        const needsFullHistory = !cached || !cached.hasScrollback
        if (needsFullHistory) {
          // 2000 lines matches tmux's default history-limit and the
          // FULL_CAPTURE_LINES cap in lib/host.ts — bounds wire bytes
          // to a few hundred KB even on heavily-coloured panes.
          const cap = await commands.tmuxCapturePane(hostId, paneId, 2000)
          if (aborted()) return
          if (cap.status === 'ok' && cap.data.length > 0) {
            // If we already painted a partial buffer, clear it before
            // writing the full version — otherwise the visible buffer
            // would print twice (once from the partial, once embedded
            // in the full history).
            if (cached) term.reset()
            term.write(cap.data)
            useStore.getState().setPaneCapture(hostId, paneId, cap.data, true)
          }
        }

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
      // Log send failures instead of swallowing. Most common case is
      // "host not connected" when tmux died and the supervisor is
      // mid-respawn — the ReconnectingOverlay surfaces that to the
      // user; the log is for our own debugging.
      void commands.tmuxSendKeys(hostId, paneId, Array.from(encoder.encode(data))).then((res) => {
        if (res.status !== 'ok') {
          console.warn('tmux_send_keys failed:', res.error)
        }
      })
    })

    // Resize: term → resize the *whole control client*. For multi-pane
    // layouts we'll layer per-pane resize on top of this in phase 1e.
    const resizeDisp = term.onResize(({ cols, rows }) => {
      if (aborted()) return
      void commands.tmuxResizeClient(hostId, cols, rows)
    })

    // Re-fit on container resize, debounced. ResizeObserver doesn't fire
    // for display:none elements, so this only catches changes while the
    // pane is visible — the visibility effect handles the
    // hidden-then-shown transition.
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
    }
  }, [hostId, paneId])

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
    <div className="relative h-full w-full overflow-hidden bg-[#0A0B0D]">
      <div ref={hostRef} className="absolute inset-0 overflow-hidden px-3 py-2" />
    </div>
  )
}

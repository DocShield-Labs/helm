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
 *  2. tmux output  → BlockTracker.ingest → term.write (continues even when hidden)
 *  3. user input   → term.onData → tmux_send_keys
 *  4. xterm size   → term.onResize → tmux_resize_client
 *  5. visible flip → re-fit + refocus
 *  6. unmount      → abort in-flight async, dispose terminal
 *
 * Phase 4F layer (chrome only — typing stays native to xterm):
 *  - BlockTracker — turns OSC 133 markers into a per-block row model.
 *  - BlockOverlay — left border, hover tint, status chip, action chips.
 *  - StickyRunHeader — pinned status bar when a running block scrolls off.
 *  - Block-action keybindings (Cmd+Up/Down, Cmd+C, Cmd+Shift+C, Cmd+R).
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { commands } from '@lib/ipc'
import { attachTerminal } from '@lib/terminal'
import { subscribePaneOutput } from '@lib/host'
import { useStore } from '@lib/store'
import type { HostId } from '@bindings'
import {
  BlockTracker,
  clipBlockToViewport,
  type BlockSnapshot,
  type PromptState,
} from './blockTracker'
import { BlockOverlay } from './BlockOverlay'
import { StickyRunHeader } from './StickyRunHeader'
import { TerminalScrollbar } from './TerminalScrollbar'

const HOST_PADDING_TOP = 8

interface TmuxPaneProps {
  hostId: HostId
  paneId: string
  /** When false, parent renders us as `display: none` — we keep our
   * xterm + subscription alive but stop processing keystrokes (so a
   * background pane doesn't eat keys meant for the visible one). */
  isVisible?: boolean
}

const DEFAULT_PROMPT_STATE: PromptState = {
  atPrompt: false,
  altScreen: false,
}

export function TmuxPane({ hostId, paneId, isVisible = true }: TmuxPaneProps) {
  const wrapperRef = useRef<HTMLDivElement>(null)
  const hostRef = useRef<HTMLDivElement>(null)
  // Refs let the visibility effect reach into the mount-effect's scope
  // without restarting the whole pane every time `isVisible` flips.
  const termRef = useRef<ReturnType<typeof attachTerminal> | null>(null)
  const trackerRef = useRef<BlockTracker | null>(null)
  const visibleRef = useRef(isVisible)

  const paneKey = `${hostId}::${paneId}`

  // Local mirrors of tracker state. Prompt state drives the re-run
  // chip's `canDispatch` gate; the block list drives chrome rendering
  // and the keymap handler.
  const [blocks, setBlocks] = useState<BlockSnapshot[]>([])
  const [promptState, setPromptState] = useState<PromptState>(DEFAULT_PROMPT_STATE)
  const [hoveredId, setHoveredId] = useState<string | null>(null)
  const blocksRef = useRef(blocks)
  blocksRef.current = blocks

  const selectedId = useStore((s) => s.perPaneSelectedBlock.get(paneKey) ?? null)
  const setSelectedBlock = useStore((s) => s.setSelectedBlock)

  useEffect(() => {
    const host = hostRef.current
    if (!host) return

    const ac = new AbortController()
    const aborted = () => ac.signal.aborted
    const encoder = new TextEncoder()
    const attached = attachTerminal(host)
    const { term, fit, dispose } = attached
    termRef.current = attached

    const tracker = new BlockTracker(term, paneKey, {
      onBlocksChanged: (snaps) => {
        if (aborted()) return
        setBlocks(snaps)
      },
      onPromptStateChanged: (st) => {
        if (aborted()) return
        setPromptState(st)
      },
    })
    trackerRef.current = tracker

    // Try the pre-fetched capture first — populated by `prehydrateCaptures`
    // after every refetchTree. Hits are instant (no IPC, no SSH RTT) but
    // pre-hydration is visible-buffer-only to keep wire bytes small;
    // we upgrade to bounded-history scrollback in the background below.
    //
    // The pre-hydration capture comes from `tmux capture-pane` which strips
    // OSC 133 markers (they were already consumed by helm-tmux's parser
    // during live forwarding). So writing it directly to xterm without
    // going through the tracker is fine — there are no markers in there.
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

        unsub = subscribePaneOutput(hostId, paneId, (bytes, markers) => {
          tracker.ingest(new Uint8Array(bytes), markers)
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
      tracker.dispose()
      dispose()
      termRef.current = null
      trackerRef.current = null
    }
  }, [hostId, paneId, paneKey])

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

  // ---------- hover detection (Y-range based) ----------

  // BlockOverlay is `pointer-events: none` so wheel events scroll xterm
  // and clicks select text. We can't use mouseEnter on the block divs.
  // Instead, listen for mousemove on the outer wrapper, compute Y in
  // wrapper-local coordinates, and find which block's row range
  // contains that Y. Same `clipBlockToViewport` helper as the render
  // path so the two stay agreed on which blocks are interactive.
  const handleMouseMove = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    const wrapper = wrapperRef.current
    const term = termRef.current?.term
    const host = hostRef.current
    if (!wrapper || !term || !host) return
    const rect = wrapper.getBoundingClientRect()
    const y = e.clientY - rect.top - HOST_PADDING_TOP

    const row = host.querySelector('.xterm-rows > div') as HTMLElement | null
    const cellH = row ? row.getBoundingClientRect().height || 17 : 17
    const viewportY = term.buffer.active.viewportY

    let next: string | null = null
    for (const block of blocksRef.current) {
      const clipped = clipBlockToViewport(block.startLine, block.endLine, term)
      if (!clipped) continue
      const top = (clipped.visibleTop - viewportY) * cellH
      const heightPx = (clipped.visibleBottom - clipped.visibleTop + 1) * cellH
      if (y >= top && y < top + heightPx) {
        next = block.id
        break
      }
    }
    setHoveredId((prev) => (prev === next ? prev : next))
  }, [])

  const handleMouseLeave = useCallback(() => setHoveredId(null), [])

  // ---------- block-action keybindings ----------

  useEffect(() => {
    if (!isVisible) return
    const onKeyDown = (e: KeyboardEvent) => {
      if (!e.metaKey) return

      // Don't fight text inputs for these chords. macOS users expect
      // Cmd+Up/Down/C to behave as standard text-editing within an
      // editable element. Block actions only fire when focus is in
      // xterm (or non-editable chrome).
      const activeEl = document.activeElement as HTMLElement | null
      if (activeEl) {
        const tag = activeEl.tagName
        if (
          tag === 'INPUT' ||
          tag === 'TEXTAREA' ||
          activeEl.isContentEditable
        ) {
          return
        }
      }

      // Cmd+Up / Cmd+Down → move selected block within this pane.
      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
        if (blocks.length === 0) return
        const idx = blocks.findIndex((b) => b.id === selectedId)
        const delta = e.key === 'ArrowUp' ? -1 : 1
        // -1 (no selection) + Up wraps to last; -1 + Down picks first.
        const nextIdx =
          idx === -1
            ? delta === -1
              ? blocks.length - 1
              : 0
            : Math.max(0, Math.min(blocks.length - 1, idx + delta))
        e.preventDefault()
        setSelectedBlock(hostId, paneId, blocks[nextIdx]?.id ?? null)
        return
      }

      // The remaining shortcuts only fire when a block is selected.
      const block = blocks.find((b) => b.id === selectedId)
      if (!block) return
      const term = termRef.current?.term

      if (e.key === 'r' || e.key === 'R') {
        // Cmd+R → re-run. Gated on atPrompt to avoid stuffing bytes
        // into a running command's stdin.
        e.preventDefault()
        if (
          block.command &&
          promptState.atPrompt &&
          !promptState.altScreen
        ) {
          const bytes = Array.from(new TextEncoder().encode(block.command + '\r'))
          void commands.tmuxSendKeys(hostId, paneId, bytes)
        }
        return
      }

      if (e.shiftKey && (e.key === 'C' || e.key === 'c')) {
        // Cmd+Shift+C → copy command.
        e.preventDefault()
        if (block.command) {
          void navigator.clipboard.writeText(block.command).catch(() => {})
        }
        return
      }

      if (!e.shiftKey && (e.key === 'C' || e.key === 'c')) {
        // Cmd+C → copy output. Only intercept if there's no live
        // selection in xterm — otherwise let the browser's native
        // copy run.
        if (term && term.hasSelection()) return
        e.preventDefault()
        if (block.startLine >= 0 && term) {
          const end =
            block.endLine ?? term.buffer.active.baseY + term.buffer.active.cursorY
          const lines: string[] = []
          for (let row = block.startLine + 1; row <= end; row++) {
            const line = term.buffer.active.getLine(row)
            if (!line) continue
            lines.push(line.translateToString(true).replace(/\s+$/, ''))
          }
          void navigator.clipboard.writeText(lines.join('\n')).catch(() => {})
        }
        return
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [
    blocks,
    selectedId,
    isVisible,
    hostId,
    paneId,
    setSelectedBlock,
    promptState.atPrompt,
    promptState.altScreen,
  ])

  const handleJumpTo = useMemo(
    () => (line: number) => {
      const term = termRef.current?.term
      if (!term) return
      // Scroll the viewport so `line` becomes the new top row. xterm's
      // public `scrollLines(delta)` is the cleanest way: it clamps to
      // the buffer extents and respects scrollback bounds.
      const delta = line - term.buffer.active.viewportY
      if (delta !== 0) term.scrollLines(delta)
    },
    [],
  )

  return (
    <div
      ref={wrapperRef}
      className="relative h-full w-full overflow-hidden bg-[#0A0B0D]"
      onMouseMove={handleMouseMove}
      onMouseLeave={handleMouseLeave}
    >
      {/* xterm fills the whole pane. Block chrome is drawn as React
          overlays anchored to row positions; typing is native (xterm
          handles input directly into the shell underneath). */}
      {termRef.current && hostRef.current && (
        <StickyRunHeader
          term={termRef.current.term}
          blocks={blocks}
          onJumpTo={handleJumpTo}
        />
      )}
      <div
        ref={hostRef}
        className="absolute inset-0 overflow-hidden pl-6 pr-2 py-2"
      />
      {termRef.current && hostRef.current && (
        <BlockOverlay
          term={termRef.current.term}
          hostElement={hostRef.current}
          blocks={blocks}
          selectedId={selectedId}
          hoveredId={hoveredId}
          hostId={hostId}
          paneId={paneId}
          canDispatch={promptState.atPrompt && !promptState.altScreen}
          onJumpTo={handleJumpTo}
        />
      )}
      {termRef.current && (
        <TerminalScrollbar term={termRef.current.term} />
      )}
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
 * input. This keeps the dismiss-on-keystroke behaviour intuitive
 * while letting the click-to-jump UX preserve the notification.
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

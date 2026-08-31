/**
 * SessionView — one session: the whole normal-screen document as DOM,
 * an xterm for alt-screen TUIs and input encoding, and the composer.
 *
 * Model (PLAN.md M8): helmd owns the terminal. Rows that scroll out of
 * the grid are history, addressed by absolute line; the grid's top row
 * is `topLine`. Blocks are line ranges over that space. This component
 * renders, top to bottom:
 *
 *   sentinel   — loads an older page of history when scrolled into view
 *   blocks     — finished commands, each its rows from history or the
 *                mirror of the grid
 *   running    — the command in flight as a block with the same header:
 *                its rows — scrolled-out AND still on the grid — render
 *                as the same DOM as everything else, with the terminal
 *                cursor spliced in as an inline caret. One renderer for
 *                the whole document: selection crosses freely, fonts
 *                never shift, and there is no seam to misalign.
 *
 * The xterm is a hidden overlay in normal mode: it stays painted (so
 *   its DEC modes encode keys/paste the way the application expects,
 *   and the alt screen appears instantly) and its size drives the PTY
 *   dimensions, but the DOM is what you see. On the alt screen the
 *   overlay becomes visible and the TUI owns it.
 *
 * Input is the composer, not the shell's prompt (see sessionState.ts):
 *   prompt   → grid hidden, blocks pinned to the bottom, composer
 *   running  → DOM shows the command; composer hidden for a shell,
 *              shown in Agent mode for an agent (Claude Code)
 *   alt      → the TUI owns the xterm; agent composer if it's an agent
 *   raw      → plain terminal (no integration, or the process exited)
 * Agent bells feed notifications, but do not change the input surface:
 * terminal programs use bells for both attention and ordinary turn
 * completion, so they are not reliable evidence of a permission prompt.
 *
 * Stays mounted across switches: when `isVisible` flips to false the
 * parent hides us via `display: none`; the mirror keeps updating and
 * the painter catches up when we're shown again.
 */

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { MAX_HISTORY_PAGE } from '@bindings'
import type { HostId } from '@bindings'
import { commands } from '@lib/ipc'
import { attachTerminal, getTheme, type HelmTerminal } from '@lib/terminal'
import { domAdvancePx } from '@lib/terminal/cellHeight'
import { useStore } from '@lib/store'
import * as blocks from '@lib/session/blocks'
import {
  bodyStartLine,
  consumeJump,
  lastFinished,
  useSessionBlocks,
  usePendingJump,
} from '@lib/session/blocks'
import * as screen from '@lib/session/screen'
import { useCursor, useScreenMeta } from '@lib/session/screen'
import { attachPainter, type Painter } from '@lib/session/painter'
import {
  forgetSession,
  reportEffective,
  setComposerMode,
  useComposerMode,
  type ComposerMode,
} from '@lib/session/composer'
import { deriveSessionState } from '@lib/session/sessionState'
import { agentName, agentNameForCommand, buildAgentCommand } from '@lib/session/agents'
import { BlockHeader } from './Block'
import { BlockList } from './BlockList'
import { Composer } from './Composer'
import { CHUNK, RowsView, rowsToText } from './Rows'
import { SearchOverlay } from './SearchOverlay'
import { ChevronDownIcon } from '@features/sessions/icons'

interface SessionViewProps {
  hostId: HostId
  sessionId: string
  isVisible?: boolean
}

/** Rows the DOM renders below the fold before windowing kicks in. The
 * client may hold 60k loaded rows; mounting them all as DOM is a
 * multi-second layout (measured via perf.ts — session switches stalled
 * 2-5s). Only this window exists as elements; the top sentinel widens
 * it before it falls through to daemon history paging. */
const RENDER_WINDOW = 3_000
/** Rows each sentinel hit adds to the window. */
const RENDER_PAGE = 1_500

/** Scroll geometry captured when an older page is requested, so the
 * content under the cursor stays put once the page lands above it. */
interface ScrollAnchor {
  loadedFrom: number | null
  scrollHeight: number
  scrollTop: number
}

export function SessionView({ hostId, sessionId, isVisible = true }: SessionViewProps) {
  const rootRef = useRef<HTMLDivElement>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const contentRef = useRef<HTMLDivElement>(null)
  const sentinelRef = useRef<HTMLDivElement>(null)
  const xtermHostRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<HelmTerminal | null>(null)
  const painterRef = useRef<Painter | null>(null)
  const visibleRef = useRef(isVisible)
  /** Following the bottom: true until the user scrolls up. */
  const atBottomRef = useRef(true)
  const lastScrollTopRef = useRef(0)
  const anchorRef = useRef<ScrollAnchor | null>(null)

  const [helmTerm, setHelmTerm] = useState<HelmTerminal | null>(null)
  const [ready, setReady] = useState(false)
  const [searchOpen, setSearchOpen] = useState(false)
  /** Absolute line the DOM starts at; null = window pinned to the tail
   * (follows output, DOM stays bounded). Scrolling up freezes it. */
  const [renderFromState, setRenderFrom] = useState<number | null>(null)
  const [atBottom, setAtBottom] = useState(true)
  const [focusKey, setFocusKey] = useState(0)
  const pb = useSessionBlocks(hostId, sessionId)
  const meta = useScreenMeta(hostId, sessionId)
  const cursor = useCursor(hostId, sessionId)
  const jump = usePendingJump(hostId, sessionId)

  const session = useStore((s) => s.sessions.get(hostId)?.sessions.get(sessionId))
  const runningSession = useStore((s) => s.runningSessions.get(`${hostId}::${sessionId}`))
  const sessionCwd = session?.cwd ?? null
  const sessionBranch = session?.branch ?? null
  const spawned = session?.command ?? null
  const defaultAgentId = useStore((s) => s.defaultAgentId)
  const customAgentTemplate = useStore((s) => s.customAgentTemplate)

  const ps = useMemo(
    () => deriveSessionState(pb, spawned || null, customAgentTemplate, runningSession?.agentName),
    [pb, spawned, customAgentTemplate, runningSession?.agentName],
  )
  const defaultAgentName = agentName(defaultAgentId, customAgentTemplate)
  const runningAgentName =
    runningSession?.agentName ??
    agentNameForCommand(ps.current?.cmdline ?? spawned, customAgentTemplate) ??
    defaultAgentName
  const mode = useComposerMode(hostId, sessionId, ps.kind)
  reportEffective(hostId, sessionId, ps.kind, mode)

  const xtermShown = ps.phase !== 'prompt'

  // ONE definition of the terminal's size: rows from xterm's fit (cell
  // height measured and corrected to the DOM line height), cols from
  // the DOM's own glyph advance so a cols-wide line exactly fills the
  // rendered text area (see cellHeight.ts). Reached three ways — the
  // resize observers (via scheduleFit), the terminal lib's first render
  // and line-height corrections (via the `refit` option), and mount —
  // and nothing else sizes the PTY.
  const fitNow = useCallback(() => {
    const t = termRef.current
    const host = xtermHostRef.current
    if (!t || !host || !visibleRef.current) return
    const dimensions = t.fit.proposeDimensions()
    if (!dimensions) return
    const cols = Math.max(2, Math.floor(host.clientWidth / domAdvancePx()))
    const rows = dimensions.rows
    if (cols === t.term.cols && rows === t.term.rows) return
    painterRef.current?.resizeAndRepaint(() => {
      try {
        t.term.resize(cols, rows)
      } catch {
        /* not laid out yet */
      }
    })
  }, [])
  // At most one fit per frame, so the grid tracks a window drag live
  // without fitting more than the display refreshes.
  const fitRafRef = useRef(0)
  const scheduleFit = useCallback(() => {
    if (fitRafRef.current) return
    fitRafRef.current = requestAnimationFrame(() => {
      fitRafRef.current = 0
      fitNow()
    })
  }, [fitNow])
  useEffect(() => () => cancelAnimationFrame(fitRafRef.current), [])
  const composerShown = ps.phase === 'prompt' || (ps.kind === 'agent' && mode === 'agent')
  /** Agent session in Terminal mode: typing lands in the TUI; keep the
   * mode control reachable. */
  const nativeBar = ps.kind === 'agent' && mode === 'terminal'

  const history = useMemo(() => {
    const out: string[] = []
    for (const b of pb.blocks) {
      const c = b.cmdline?.trim()
      if (!c || b.end_line === null) continue
      if (out[out.length - 1] !== c) out.push(c)
    }
    return out.length > 200 ? out.slice(-200) : out
  }, [pb.blocks])

  const moreAbove = meta.loadedFrom === null || meta.loadedFrom > meta.historyStart

  // The running block's body starts where its output does; with no
  // running block (no integration, process exited, the gap between D
  // and the next A) everything after the last finished block is shown.
  const running = meta.alt ? null : ps.current
  const bodyFrom = Math.max(
    running ? bodyStartLine(running) : (lastFinished(pb.blocks)?.end_line ?? 0),
    meta.loadedFrom ?? meta.topLine,
    pb.clearedBefore,
  )
  // The live tail renders as DOM through the document's end — one
  // definition shared with the `clear` threshold (screen.documentEnd).
  const liveTo = screen.documentEnd(screen.getSessionScreen(hostId, sessionId), ps.kind === 'agent')
  /** The whole normal-screen document is DOM while the tail is live. */
  const showLive = ready && xtermShown && !meta.alt
  // First line the DOM renders. Following the tail, it slides with the
  // output; frozen (user scrolled up and the sentinel widened it), it
  // holds until the user re-pins to the bottom. Quantized to the chunk
  // grid so it moves once per CHUNK lines, not every frame — the
  // boundary chunk's range stays cache-stable while output streams.
  const loadedFloor = Math.max(meta.loadedFrom ?? meta.topLine, pb.clearedBefore)
  const rawFloor = renderFromState !== null ? Math.min(renderFromState, liveTo) : liveTo - RENDER_WINDOW
  const windowFloor = Math.max(loadedFloor, Math.floor(Math.max(0, rawFloor) / CHUNK) * CHUNK)
  const windowRestricted = windowFloor > loadedFloor
  // Effects observe the floors through refs: their values move with
  // every flush while streaming, and effects keyed on them would tear
  // down and rebuild observers ~30x/s.
  const windowFloorRef = useRef(windowFloor)
  windowFloorRef.current = windowFloor
  const loadedFloorRef = useRef(loadedFloor)
  loadedFloorRef.current = loadedFloor
  const copyRunning = () =>
    void navigator.clipboard.writeText(
      rowsToText(screen.rowsBetween(screen.getSessionScreen(hostId, sessionId), bodyFrom, liveTo)),
    )

  // ---- xterm lifecycle + first paint ----
  useEffect(() => {
    const host = xtermHostRef.current
    if (!host) return
    const ac = new AbortController()
    const { previewThemeName, themeName } = useStore.getState()
    const attached = attachTerminal(host, { theme: getTheme(previewThemeName ?? themeName), refit: fitNow })
    const { term, dispose } = attached
    termRef.current = attached
    setHelmTerm(attached)
    const painter = attachPainter(term, hostId, sessionId, () => visibleRef.current)
    painterRef.current = painter

    void (async () => {
      await Promise.all([blocks.ensureLoaded(hostId, sessionId), screen.ensureScreen(hostId, sessionId)])
      if (ac.signal.aborted) return
      setReady(true)
      const top = screen.getSessionScreen(hostId, sessionId).topLine
      void screen.ensureHistory(hostId, sessionId, top - MAX_HISTORY_PAGE)
      // Fit BEFORE the first resize call: xterm still has its 80×24
      // construction defaults here, and pushing those at the daemon
      // just to correct them one fit later delivers two SIGWINCHes —
      // each one a full repaint (and, for Claude Code, a duplicated
      // frame in history). Fitted first, the explicit call reconciles
      // the daemon to the real dimensions once; helmd drops it as a
      // no-op when nothing changed.
      fitNow()
      void commands.sessionResize(hostId, sessionId, term.cols, term.rows)
    })()

    const inputDisp = term.onData((data) => {
      if (ac.signal.aborted || !visibleRef.current) return
      if (isUserKeystroke(data)) {
        dismissNotificationsFor(hostId, sessionId)
      }
      void screen.sendInput(hostId, sessionId, data)
    })
    // PTY resize is trailing-debounced: a window drag crosses dozens of
    // cell boundaries, and every SIGWINCH makes a streaming Claude Code
    // re-emit its tail block — measured ~60 duplicate re-emissions for
    // a 60-step drag (scratchpad cc-capture/raw4). One SIGWINCH at the
    // end of the gesture keeps the transcript clean; the visual fit
    // (xterm dims, DOM) still tracks the drag live.
    let resizeTimer = 0
    const resizeDisp = term.onResize(({ cols, rows }) => {
      if (ac.signal.aborted) return
      window.clearTimeout(resizeTimer)
      resizeTimer = window.setTimeout(() => {
        if (!ac.signal.aborted) void commands.sessionResize(hostId, sessionId, cols, rows)
      }, 200)
    })

    return () => {
      ac.abort()
      window.clearTimeout(resizeTimer)
      painter.dispose()
      inputDisp.dispose()
      resizeDisp.dispose()
      dispose()
      forgetSession(hostId, sessionId)
      termRef.current = null
      painterRef.current = null
      setHelmTerm(null)
      setReady(false)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, sessionId, fitNow])

  // ---- sizing, and following the bottom ----
  // Terminal mode follows the scroll viewport; Agent mode uses the full
  // pane so composer growth clips rather than resizes the PTY. Whenever
  // either geometry or the content changes, stay pinned to the bottom
  // unless the user has scrolled up.
  useEffect(() => {
    const sc = scrollRef.current
    const content = contentRef.current
    const root = rootRef.current
    if (!sc || !content || !root) return
    const pin = () => {
      if (atBottomRef.current && visibleRef.current) scrollToBottom(sc)
    }
    const viewport = new ResizeObserver(() => {
      // Fit live so the grid tracks the window during a drag, not 30ms
      // after it stops.
      scheduleFit()
      pin()
    })
    const body = new ResizeObserver(pin)
    const pane = new ResizeObserver(scheduleFit)
    viewport.observe(sc)
    body.observe(content)
    pane.observe(root)
    return () => {
      viewport.disconnect()
      body.disconnect()
      pane.disconnect()
    }
  }, [scheduleFit])

  // Refit on the state transitions that change the overlay's size without
  // a viewport resize: the session becoming visible, the native bar
  // showing or hiding. The live resize above covers dragging.
  useEffect(() => {
    if (ready && isVisible) scheduleFit()
  }, [ready, nativeBar, isVisible, scheduleFit])

  // ---- paging: the sentinel widens the render window over rows the
  // client already holds (cheap, synchronous), and only once the window
  // reaches the loaded floor does it page older rows from the daemon.
  useEffect(() => {
    const el = sentinelRef.current
    const root = scrollRef.current
    if (!el || !root || !ready || !isVisible || !(moreAbove || windowRestricted)) return
    const io = new IntersectionObserver(
      (entries) => {
        if (!entries.some((e) => e.isIntersecting)) return
        anchorRef.current = {
          loadedFrom: screen.getSessionScreen(hostId, sessionId).loadedFrom,
          scrollHeight: root.scrollHeight,
          scrollTop: root.scrollTop,
        }
        if (windowFloorRef.current > loadedFloorRef.current) {
          setRenderFrom(Math.max(loadedFloorRef.current, windowFloorRef.current - RENDER_PAGE))
          return
        }
        const s = screen.getSessionScreen(hostId, sessionId)
        void screen.ensureHistory(hostId, sessionId, (s.loadedFrom ?? s.topLine) - MAX_HISTORY_PAGE)
      },
      { root, rootMargin: '400px 0px 0px 0px' },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [hostId, sessionId, ready, isVisible, moreAbove, meta.loadedFrom, windowRestricted])

  // ---- pin after every commit ----
  // Runs synchronously after each DOM update, before the browser can
  // dispatch a scroll event against the new geometry — so a block
  // landing, the composer appearing, or the band growing can't leave
  // the view short of the bottom even for one frame. (The resize
  // observers cover growth that isn't a React commit.)
  useLayoutEffect(() => {
    const sc = scrollRef.current
    if (sc && isVisible && atBottomRef.current) scrollToBottom(sc)
  })

  // ---- keep the viewport steady when content lands above it — an
  // older daemon page, or the render window widening over loaded rows.
  useLayoutEffect(() => {
    const sc = scrollRef.current
    if (!sc || !isVisible || atBottomRef.current) return
    const anchor = anchorRef.current
    if (!anchor) return
    const paged = meta.loadedFrom !== null && meta.loadedFrom < (anchor.loadedFrom ?? Infinity)
    if (paged || sc.scrollHeight !== anchor.scrollHeight) {
      sc.scrollTop = anchor.scrollTop + (sc.scrollHeight - anchor.scrollHeight)
      anchorRef.current = null
    }
  }, [meta.loadedFrom, meta.historyVersion, renderFromState, isVisible, ready])

  // ---- focus follows the input surface; a shown session repaints ----
  useEffect(() => {
    visibleRef.current = isVisible
    if (!isVisible) return
    screen.setForeground(hostId, sessionId)
    painterRef.current?.repaintIfDirty()
    if (composerShown) {
      setFocusKey((k) => k + 1)
    } else {
      termRef.current?.term.focus()
    }
  }, [isVisible, composerShown, ready, hostId, sessionId])

  // ---- Cmd+F: find across rows + the live grid (visible session only) ----
  useEffect(() => {
    if (!isVisible) return
    const onKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && (e.key === 'f' || e.key === 'F')) {
        e.preventDefault()
        e.stopPropagation()
        setSearchOpen(true)
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [isVisible])

  // ---- palette jump-to-line: page the rows in, then scroll ----
  useEffect(() => {
    if (!jump || !ready || !isVisible) return
    const sc = scrollRef.current
    if (!sc) return
    const s = screen.getSessionScreen(hostId, sessionId)
    if (jump.line >= s.historyStart && jump.line < (s.loadedFrom ?? s.topLine)) {
      void screen.ensureHistory(hostId, sessionId, jump.line - 40)
      return // re-runs when the page lands (meta.loadedFrom changes)
    }
    if (jump.line < windowFloorRef.current) {
      setRenderFrom(Math.max(loadedFloorRef.current, jump.line - 40))
      return // re-runs when the window widens (renderFromState changes)
    }
    consumeJump(jump)
    const el =
      sc.querySelector<HTMLElement>(`[data-line="${jump.line}"]`) ??
      (jump.blockId ? sc.querySelector<HTMLElement>(`[data-block-id="${jump.blockId}"]`) : null)
    if (el) {
      atBottomRef.current = false
      el.scrollIntoView({ block: 'center' })
      const flash = el.closest<HTMLElement>('[data-block-id]') ?? el
      flash.classList.add('helm-block-flash')
      window.setTimeout(() => flash.classList.remove('helm-block-flash'), 1200)
    } else {
      scrollToBottom(sc)
    }
  }, [jump, ready, isVisible, hostId, sessionId, meta.loadedFrom, renderFromState])

  // Following stops only when the position actually moves up — a
  // wheel, a scrollbar drag. Content growing underneath (the position
  // didn't move but the bottom did) and clamps (content shrank) never
  // unpin: scroll events are dispatched a frame late, so judging them
  // by geometry alone would unpin on our own growth.
  const onScroll = () => {
    const sc = scrollRef.current
    if (!sc) return
    const at = sc.scrollTop + sc.clientHeight >= sc.scrollHeight - 4
    const movedUp = sc.scrollTop < lastScrollTopRef.current - 1
    lastScrollTopRef.current = sc.scrollTop
    const pinned = at ? true : movedUp ? false : atBottomRef.current
    // Window policy rides the pin state: pinned, the window follows the
    // tail (and the DOM shrinks back to it); unpinned, the floor
    // freezes so streaming can't slide rows out from under the reader.
    if (pinned && renderFromState !== null) setRenderFrom(null)
    else if (!pinned && renderFromState === null) setRenderFrom(windowFloorRef.current)
    atBottomRef.current = pinned
    setAtBottom(pinned)
  }

  const focusInput = () => {
    if (composerShown) setFocusKey((k) => k + 1)
    else termRef.current?.term.focus()
  }

  /** Text to the session as typed input, ending with ⏎.
   *
   * To an agent, always a bracketed paste followed by ⏎ in a separate
   * write: unbracketed text makes Claude Code's paste heuristic wait
   * for the chunk to settle (visible lag), and a `\r` in the same
   * chunk is taken as an inserted newline rather than a submit.
   * Bracketed text is inserted at once, so the ⏎ can follow on the
   * next tick. A shell gets `text⏎` in one write, multi-line as a
   * bracketed paste so it runs as a unit. */
  const sendText = (text: string) => {
    dismissNotificationsFor(hostId, sessionId)
    const multiline = text.includes('\n')
    const bracketed = `\x1b[200~${text}\x1b[201~`
    if (ps.kind === 'agent') {
      void screen
        .sendInput(hostId, sessionId, bracketed)
        .then(() => new Promise<void>((r) => window.setTimeout(r, 8)))
        .then(() => screen.sendInput(hostId, sessionId, '\r'))
    } else if (multiline) {
      void screen
        .sendInput(hostId, sessionId, bracketed)
        .then(() => new Promise<void>((r) => window.setTimeout(r, 30)))
        .then(() => screen.sendInput(hostId, sessionId, '\r'))
    } else {
      void screen.sendInput(hostId, sessionId, `${text}\r`)
    }
    atBottomRef.current = true
  }

  const onSend = (text: string) => {
    if (mode === 'agent' && ps.kind !== 'agent') {
      sendText(buildAgentCommand(defaultAgentId, customAgentTemplate, text))
      return
    }
    // `clear` clears the block list too (Warp does the same); the
    // shell still runs it so its own state agrees. The threshold is
    // where the NEXT prompt will land — the document's end, the same
    // definition the live tail renders to. (`topLine + rows` would
    // overshoot: alacritty's clear scrolls out only the rows in use.)
    if (mode === 'terminal' && /^(clear|reset)$/.test(text.trim())) {
      blocks.clearBefore(hostId, sessionId, liveTo)
    }
    sendText(text)
  }

  const onModeChange = (m: ComposerMode) => {
    setComposerMode(hostId, sessionId, ps.kind, m)
    if (m === 'terminal' && ps.kind === 'agent') termRef.current?.term.focus()
  }

  return (
    <div
      ref={rootRef}
      className="relative flex h-full w-full flex-col overflow-hidden bg-[var(--terminal-bg)]"
    >
      <div
        ref={scrollRef}
        onScroll={onScroll}
        onMouseUp={() => {
          // Clicking in the block area shouldn't strand the keyboard —
          // unless the user is selecting text to copy, in the DOM or
          // in the grid (xterm's selection is not a DOM selection).
          const domSelecting = !window.getSelection()?.isCollapsed
          const gridSelecting = termRef.current?.term.hasSelection() ?? false
          if (!domSelecting && !gridSelecting) focusInput()
        }}
        className="helm-scroll relative flex min-h-0 flex-1 flex-col overflow-y-auto overflow-x-hidden"
      >
        <div ref={contentRef} className="mt-auto">
          {ready && !meta.alt && (
            <>
              {(moreAbove || windowRestricted) && <div ref={sentinelRef} className="helm-sentinel" />}
              {!moreAbove && !windowRestricted && meta.historyStart > 0 && (
                <div className="px-4 pt-3 font-mono text-[11px] text-text-disabled">
                  older output no longer retained
                </div>
              )}
              <BlockList
                hostId={hostId}
                sessionId={sessionId}
                blocks={pb.blocks}
                clearedBefore={pb.clearedBefore}
                renderFrom={windowFloor}
              />
            </>
          )}
          {/* The running block: the live tail as the same DOM rows as
              everything above it, terminal cursor spliced in as an
              inline caret. */}
          {showLive && (
            <div
              className={running ? 'helm-block group relative' : undefined}
              data-block-id={running?.id}
            >
              {running && <BlockHeader block={running} copyOutput={copyRunning} />}
              <pre className="helm-block-output">
                <RowsView
                  hostId={hostId}
                  sessionId={sessionId}
                  from={Math.max(bodyFrom, windowFloor)}
                  to={liveTo}
                  cursor={cursor}
                />
              </pre>
            </div>
          )}
        </div>
      </div>

      {/* The xterm: hidden input encoder in normal mode, the TUI's
          surface on the alt screen. Always mounted and painted — its
          size drives the PTY dimensions and its DEC modes drive key
          encoding — sized to the pane (minus the native bar) so a
          growing composer overlays the grid instead of resizing the
          PTY on every newline. */}
      <div
        ref={xtermHostRef}
        // Geometry from the same CSS variables the text column and the
        // bar use, so PTY sizing can't drift from what the DOM renders:
        // the side insets mirror `.helm-block-output`'s padding (any
        // wider and every full-width line soft-wraps), and the height
        // stops above `.helm-bar`.
        className={`absolute top-0 ${meta.alt ? 'z-10' : 'pointer-events-none opacity-0'}`}
        style={{
          left: 'var(--helm-pad-x)',
          right: 'var(--helm-pad-x)',
          height: nativeBar ? 'calc(100% - var(--helm-bar-h) - 2 * var(--helm-bar-margin))' : '100%',
        }}
      />

      {composerShown && (
        <Composer
          mode={mode}
          kind={ps.kind}
          cwd={sessionCwd}
          branch={sessionBranch}
          history={history}
          agentName={ps.kind === 'agent' ? runningAgentName : defaultAgentName}
          onModeChange={onModeChange}
          onSend={onSend}
          onRaw={(bytes) => void screen.sendInput(hostId, sessionId, bytes)}
          onFileSearch={async (query) => {
            const result = await commands.sessionFileSearch(hostId, sessionId, query, 20)
            if (result.status === 'error') throw new Error(result.error)
            return result.data
          }}
          onAgentCommands={async () => {
            const result = await commands.sessionAgentCommands(hostId, sessionId)
            return result.status === 'ok' ? result.data : []
          }}
          onPathComplete={async (path, directoriesOnly) => {
            const result = await commands.sessionPathComplete(
              hostId,
              sessionId,
              path,
              directoriesOnly,
              100,
            )
            if (result.status === 'error') throw new Error(result.error)
            return result.data
          }}
          focusKey={focusKey}
        />
      )}
      {nativeBar && (
        <div className="helm-bar">
          <span className="flex-1 text-[12px] text-text-tertiary">
            typing in {runningAgentName} directly
          </span>
          <button
            type="button"
            onClick={() => onModeChange('agent')}
            className="rounded-md bg-[var(--stroke-subtle)] px-2 py-0.5 text-[11px] font-medium text-text-secondary hover:text-text-primary"
          >
            Agent composer
          </button>
        </div>
      )}

      {searchOpen && helmTerm && (
        <SearchOverlay
          helm={helmTerm}
          container={scrollRef.current}
          alt={meta.alt}
          onClose={() => setSearchOpen(false)}
        />
      )}
      {!atBottom && (
        <button
          type="button"
          onClick={() => {
            atBottomRef.current = true
            scrollToBottom(scrollRef.current)
          }}
          title="Jump to latest output"
          className="absolute left-1/2 z-20 flex -translate-x-1/2 items-center gap-1.5 rounded-full border border-[var(--stroke-default)] bg-elevated py-1 pl-2.5 pr-3 text-[12px] text-text-secondary hover:text-text-primary"
          style={{ bottom: composerShown ? 100 : 16, boxShadow: 'var(--elevation-2)' }}
        >
          <ChevronDownIcon size={13} />
          latest
        </button>
      )}
      {pb.exited && (
        <div className="pointer-events-none absolute inset-x-0 bottom-0 flex justify-center pb-2">
          <span className="rounded-full bg-elevated px-2.5 py-0.5 font-mono text-[11px] text-text-tertiary">
            process exited
          </span>
        </div>
      )}
    </div>
  )
}

function scrollToBottom(el: HTMLElement | null) {
  if (!el) return
  el.scrollTop = el.scrollHeight
}

/** Dismiss-on-keystroke: a real keypress acknowledges this session. */
function dismissNotificationsFor(hostId: HostId, sessionId: string) {
  const store = useStore.getState()
  const hasNotif = [...store.notifications.values()].some(
    (n) => n.host_id === hostId && n.session_id === sessionId,
  )
  if (hasNotif) void commands.notificationDismissForSession(hostId, sessionId)
}

/** True iff `data` is real user input and NOT a terminal-state report
 * (focus in/out, mouse events, cursor-position responses) that xterm
 * emits as a side effect of rendering or focus changes. */
function isUserKeystroke(data: string): boolean {
  if (data === '\x1b[I' || data === '\x1b[O') return false
  if (/^\x1b\[<\d+;\d+;\d+[Mm]/.test(data)) return false
  if (data.startsWith('\x1b[M') && data.length === 6) return false
  if (/^\x1b\[\d+;\d+R$/.test(data)) return false
  return true
}

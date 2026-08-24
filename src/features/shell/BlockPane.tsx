/**
 * BlockPane — one pane: history as DOM, the live grid in one xterm, and
 * the composer.
 *
 * Model (PLAN.md M8): helmd owns the terminal. Rows that scroll out of
 * the grid are history, addressed by absolute line; the grid's top row
 * is `topLine`. Blocks are line ranges over that space. This component
 * renders, top to bottom:
 *
 *   sentinel   — loads an older page of history when scrolled into view
 *   blocks     — finished commands, each its rows from history (and,
 *                while the grid is hidden, from the mirror of the grid)
 *   running    — the command in flight as a block with the same header:
 *                its body is the rows that already scrolled out (DOM)
 *                followed by the live band — the xterm, painted from
 *                screen diffs with no scrollback, clipped to the rows
 *                the command occupies in the grid. Cells are as tall as
 *                DOM rows, so when the command finishes the band becomes
 *                its block's rows without anything moving: Warp's
 *                active tail.
 *
 * Input is the composer, not the shell's prompt (see paneState.ts):
 *   prompt   → grid hidden, blocks pinned to the bottom, composer
 *   running  → grid shows the command; composer hidden for a shell,
 *              shown in Agent mode for an agent (Claude Code)
 *   alt      → the TUI owns the grid; agent composer if it's an agent
 *   raw      → plain terminal (no integration, or the process exited)
 * An agent that rings the bell is blocked on the user (a permission
 * prompt — the Claude hook only rings for those; end of turn is an
 * OSC 9 message): the composer closes and keys go straight to the TUI
 * until the user answers or presses ⏎ to reply.
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
import { locatePane, useStore } from '@lib/store'
import * as blocks from '@lib/session/blocks'
import {
  bodyStartLine,
  consumeJump,
  lastFinished,
  usePaneBlocks,
  usePendingJump,
} from '@lib/session/blocks'
import * as screen from '@lib/session/screen'
import { useScreenMeta } from '@lib/session/screen'
import { attachPainter, type Painter } from '@lib/session/painter'
import {
  forgetPane,
  reportEffective,
  setComposerMode,
  useComposerMode,
  type ComposerMode,
} from '@lib/session/composer'
import { AGENT_LAUNCH_COMMAND, derivePaneState, shellQuote } from '@lib/session/paneState'
import { BlockHeader } from './Block'
import { BlockList } from './BlockList'
import { Composer } from './Composer'
import { RowsView, rowsToText } from './Rows'
import { SearchOverlay } from './SearchOverlay'
import { ChevronDownIcon, SparkIcon } from '@features/sessions/icons'

interface BlockPaneProps {
  hostId: HostId
  paneId: string
  isVisible?: boolean
}

/** `.helm-block-output`'s top padding: the live band sits inside one. */
const BODY_PAD_TOP = 10

/** Scroll geometry captured when an older page is requested, so the
 * content under the cursor stays put once the page lands above it. */
interface ScrollAnchor {
  loadedFrom: number | null
  scrollHeight: number
  scrollTop: number
}

export function BlockPane({ hostId, paneId, isVisible = true }: BlockPaneProps) {
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
  const [slotHeight, setSlotHeight] = useState(0)
  const [searchOpen, setSearchOpen] = useState(false)
  const [atBottom, setAtBottom] = useState(true)
  const [focusKey, setFocusKey] = useState(0)
  const pb = usePaneBlocks(hostId, paneId)
  const meta = useScreenMeta(hostId, paneId)
  const jump = usePendingJump(hostId, paneId)

  // Primitive selectors: zustand re-renders on reference change.
  const paneCwd = useStore((s) => locatePane(s.sessions.get(hostId), paneId)?.pane.cwd ?? null)
  const paneBranch = useStore((s) => locatePane(s.sessions.get(hostId), paneId)?.pane.branch ?? null)
  const spawned = useStore((s) => locatePane(s.sessions.get(hostId), paneId)?.pane.command ?? null)

  const ps = useMemo(() => derivePaneState(pb, spawned || null), [pb, spawned])
  const mode = useComposerMode(hostId, paneId, ps.kind)
  reportEffective(hostId, paneId, ps.kind, mode)

  // Bells acknowledged so far; an agent pane with a newer bell is blocked.
  const [ackedBells, setAckedBells] = useState(pb.bells)
  const bellsRef = useRef(pb.bells)
  bellsRef.current = pb.bells
  const blocked = ps.kind === 'agent' && pb.bells > ackedBells
  const blockedRef = useRef(blocked)
  blockedRef.current = blocked
  const ackBells = () => setAckedBells(bellsRef.current)

  const xtermShown = ps.phase !== 'prompt'
  const shownRef = useRef(xtermShown)
  shownRef.current = xtermShown

  // Fit the xterm to the viewport, at most once per frame. Called live
  // from the viewport ResizeObserver so the grid grows and shrinks with
  // the window instead of snapping after a debounce. fit() only resizes
  // the PTY when the row/col count actually changes, so a drag that
  // doesn't cross a cell boundary costs nothing downstream.
  const fitRafRef = useRef(0)
  const scheduleFit = useCallback(() => {
    if (fitRafRef.current) return
    fitRafRef.current = requestAnimationFrame(() => {
      fitRafRef.current = 0
      const t = termRef.current
      if (!t || !visibleRef.current || !shownRef.current) return
      try {
        t.fit.fit()
      } catch {
        /* not laid out yet */
      }
    })
  }, [])
  useEffect(() => () => cancelAnimationFrame(fitRafRef.current), [])
  const composerShown =
    ps.phase === 'prompt' || (ps.kind === 'agent' && mode === 'agent' && !blocked)
  /** Agent pane in Terminal mode: typing lands in the TUI; keep the
   * mode control reachable. */
  const nativeBar = ps.kind === 'agent' && mode === 'terminal' && !blocked

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
  // The band of the grid the xterm shows: a TUI on the alt screen owns
  // it all; otherwise from the body's first row through the last row in
  // use. Lines below the band render as DOM.
  const cellH = termRef.current?.getCellSize().height ?? 20
  const liveStartRow = meta.alt ? 0 : Math.max(0, Math.min(meta.rows, bodyFrom - meta.topLine))
  const liveEndRow = meta.alt ? meta.rows : Math.max(meta.usedRows, liveStartRow + 1)
  /** First absolute line the grid shows; DOM renders lines below it. */
  const gridFrom = xtermShown ? meta.topLine + liveStartRow : Infinity
  /** The xterm is always viewport-sized: that is the grid. */
  const hostHeight = Math.max(0, slotHeight - BODY_PAD_TOP)
  const liveHeight = Math.min(hostHeight, (liveEndRow - liveStartRow) * cellH)
  const copyRunning = () =>
    void navigator.clipboard.writeText(
      rowsToText(screen.rowsBetween(screen.getPaneScreen(hostId, paneId), bodyFrom, meta.topLine + liveEndRow)),
    )

  // ---- xterm lifecycle + first paint ----
  useEffect(() => {
    const host = xtermHostRef.current
    if (!host) return
    const ac = new AbortController()
    const { previewThemeName, themeName } = useStore.getState()
    const attached = attachTerminal(host, { theme: getTheme(previewThemeName ?? themeName) })
    const { term, dispose } = attached
    termRef.current = attached
    setHelmTerm(attached)
    const painter = attachPainter(term, hostId, paneId, () => visibleRef.current)
    painterRef.current = painter

    void (async () => {
      await Promise.all([blocks.ensureLoaded(hostId, paneId), screen.ensureScreen(hostId, paneId)])
      if (ac.signal.aborted) return
      setReady(true)
      const top = screen.getPaneScreen(hostId, paneId).topLine
      void screen.ensureHistory(hostId, paneId, top - MAX_HISTORY_PAGE)
      void commands.sessionResize(hostId, paneId, term.cols, term.rows)
    })()

    const inputDisp = term.onData((data) => {
      if (ac.signal.aborted || !visibleRef.current) return
      if (isUserKeystroke(data)) {
        dismissNotificationsFor(hostId, paneId)
        if (blockedRef.current) {
          // The user answered the agent — or asked to reply (⏎).
          ackBells()
          if (data === '\r') {
            setFocusKey((k) => k + 1)
            return
          }
        }
      }
      void screen.sendInput(hostId, paneId, data)
    })
    const resizeDisp = term.onResize(({ cols, rows }) => {
      if (ac.signal.aborted) return
      void commands.sessionResize(hostId, paneId, cols, rows)
    })

    return () => {
      ac.abort()
      painter.dispose()
      inputDisp.dispose()
      resizeDisp.dispose()
      dispose()
      forgetPane(hostId, paneId)
      termRef.current = null
      painterRef.current = null
      setHelmTerm(null)
      setReady(false)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, paneId])

  // ---- the wheel over the grid scrolls the pane, not the terminal ----
  // The grid has no scrollback, so there is nothing for xterm to
  // scroll; left to itself it would hand the wheel to an application
  // that asked for mouse reports (Claude Code turns those on and
  // treats wheel-up as "previous input"). Capture it on the host,
  // scroll the pane's document, and keep it from xterm. Alt-screen
  // TUIs (vim, less, htop) own the wheel as before, and Option+wheel
  // passes through to any application that wants it.
  useEffect(() => {
    const host = xtermHostRef.current
    const sc = scrollRef.current
    if (!host || !sc) return
    const onWheel = (e: WheelEvent) => {
      if (e.ctrlKey || e.metaKey || e.altKey) return
      const t = termRef.current
      if (!t || t.term.buffer.active.type === 'alternate') return
      e.preventDefault()
      e.stopPropagation()
      const px =
        e.deltaMode === 1
          ? e.deltaY * t.getCellSize().height
          : e.deltaMode === 2
            ? e.deltaY * sc.clientHeight
            : e.deltaY
      sc.scrollTop += px
    }
    // Capture phase + passive:false: xterm's own listener sits on a
    // child element and must never see the event.
    host.addEventListener('wheel', onWheel, { capture: true, passive: false })
    return () => host.removeEventListener('wheel', onWheel, { capture: true } as EventListenerOptions)
  }, [])

  // ---- sizing, and following the bottom ----
  // The xterm is sized to the scroll viewport. Whenever the viewport
  // or its content changes size — a block lands, the band grows, the
  // composer shows or hides, fonts settle — stay pinned to the bottom
  // unless the user has scrolled up. Observing sizes (not React state)
  // means no growth can slip past without a re-pin.
  useEffect(() => {
    const sc = scrollRef.current
    const content = contentRef.current
    if (!sc || !content) return
    const pin = () => {
      if (atBottomRef.current && visibleRef.current) scrollToBottom(sc)
    }
    const viewport = new ResizeObserver((entries) => {
      setSlotHeight(Math.floor(entries[0]?.contentRect.height ?? 0))
      // Fit live so the grid tracks the window during a drag, not 30ms
      // after it stops.
      scheduleFit()
      pin()
    })
    const body = new ResizeObserver(pin)
    viewport.observe(sc)
    body.observe(content)
    setSlotHeight(Math.floor(sc.getBoundingClientRect().height))
    return () => {
      viewport.disconnect()
      body.disconnect()
    }
  }, [scheduleFit])

  // Refit on the state transitions that change the grid's slot without a
  // viewport resize: the pane becoming visible, the composer showing or
  // hiding, alt-screen toggling. The live resize above covers dragging.
  useEffect(() => {
    if (ready && xtermShown && isVisible) scheduleFit()
  }, [ready, xtermShown, meta.alt, isVisible, scheduleFit])

  // ---- history paging: the sentinel at the top pulls older rows ----
  useEffect(() => {
    const el = sentinelRef.current
    const root = scrollRef.current
    if (!el || !root || !ready || !isVisible || !moreAbove) return
    const io = new IntersectionObserver(
      (entries) => {
        if (!entries.some((e) => e.isIntersecting)) return
        const s = screen.getPaneScreen(hostId, paneId)
        anchorRef.current = {
          loadedFrom: s.loadedFrom,
          scrollHeight: root.scrollHeight,
          scrollTop: root.scrollTop,
        }
        void screen.ensureHistory(hostId, paneId, (s.loadedFrom ?? s.topLine) - MAX_HISTORY_PAGE)
      },
      { root, rootMargin: '400px 0px 0px 0px' },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [hostId, paneId, ready, isVisible, moreAbove, meta.loadedFrom])

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

  // ---- keep the viewport steady when an older page lands above it ----
  useLayoutEffect(() => {
    const sc = scrollRef.current
    if (!sc || !isVisible || atBottomRef.current) return
    const anchor = anchorRef.current
    if (anchor && meta.loadedFrom !== null && meta.loadedFrom < (anchor.loadedFrom ?? Infinity)) {
      sc.scrollTop = anchor.scrollTop + (sc.scrollHeight - anchor.scrollHeight)
      anchorRef.current = null
    }
  }, [meta.loadedFrom, meta.historyVersion, isVisible, ready])

  // ---- focus follows the input surface; a shown pane repaints ----
  useEffect(() => {
    visibleRef.current = isVisible
    if (!isVisible) return
    painterRef.current?.repaintIfDirty()
    if (composerShown) {
      setFocusKey((k) => k + 1)
    } else {
      termRef.current?.term.focus()
    }
  }, [isVisible, composerShown, ready])

  // ---- Cmd+F: find across rows + the live grid (visible pane only) ----
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
    const s = screen.getPaneScreen(hostId, paneId)
    if (jump.line >= s.historyStart && jump.line < (s.loadedFrom ?? s.topLine)) {
      void screen.ensureHistory(hostId, paneId, jump.line - 40)
      return // re-runs when the page lands (meta.loadedFrom changes)
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
  }, [jump, ready, isVisible, hostId, paneId, meta.loadedFrom])

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
    atBottomRef.current = pinned
    setAtBottom(pinned)
  }

  const focusInput = () => {
    if (composerShown) setFocusKey((k) => k + 1)
    else termRef.current?.term.focus()
  }

  /** Text to the pane as typed input, ending with ⏎.
   *
   * To an agent, always a bracketed paste followed by ⏎ in a separate
   * write: unbracketed text makes Claude Code's paste heuristic wait
   * for the chunk to settle (visible lag), and a `\r` in the same
   * chunk is taken as an inserted newline rather than a submit.
   * Bracketed text is inserted at once, so the ⏎ can follow on the
   * next tick. A shell gets `text⏎` in one write, multi-line as a
   * bracketed paste so it runs as a unit. */
  const sendText = (text: string) => {
    dismissNotificationsFor(hostId, paneId)
    const multiline = text.includes('\n')
    const bracketed = `\x1b[200~${text}\x1b[201~`
    if (ps.kind === 'agent') {
      void screen
        .sendInput(hostId, paneId, bracketed)
        .then(() => new Promise<void>((r) => window.setTimeout(r, 8)))
        .then(() => screen.sendInput(hostId, paneId, '\r'))
    } else if (multiline) {
      void screen
        .sendInput(hostId, paneId, bracketed)
        .then(() => new Promise<void>((r) => window.setTimeout(r, 30)))
        .then(() => screen.sendInput(hostId, paneId, '\r'))
    } else {
      void screen.sendInput(hostId, paneId, `${text}\r`)
    }
    atBottomRef.current = true
  }

  const onSend = (text: string) => {
    if (mode === 'agent' && ps.kind !== 'agent') {
      sendText(`${AGENT_LAUNCH_COMMAND} ${shellQuote(text)}`)
      return
    }
    // `clear` clears the block list too (Warp does the same); the
    // shell still runs it so its own state agrees.
    if (mode === 'terminal' && /^(clear|reset)$/.test(text.trim())) {
      blocks.clearBefore(hostId, paneId, meta.topLine + meta.rows)
    }
    sendText(text)
  }

  const onModeChange = (m: ComposerMode) => {
    setComposerMode(hostId, paneId, ps.kind, m)
    if (m === 'terminal' && ps.kind === 'agent') termRef.current?.term.focus()
  }

  return (
    <div className="relative flex h-full w-full flex-col overflow-hidden bg-[var(--terminal-bg)]">
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
              {moreAbove && <div ref={sentinelRef} className="helm-sentinel" />}
              {!moreAbove && meta.historyStart > 0 && (
                <div className="px-4 pt-3 font-mono text-[11px] text-text-disabled">
                  older output no longer retained
                </div>
              )}
              <BlockList
                hostId={hostId}
                paneId={paneId}
                blocks={pb.blocks}
                clearedBefore={pb.clearedBefore}
                meta={meta}
                gridFrom={gridFrom}
              />
            </>
          )}
          {/* The running block. Its xterm host keeps this exact spot in
              the tree whatever the pane's phase, so the terminal is
              attached once; the pre around it hides at a prompt. */}
          <div
            className={running ? 'helm-block group relative' : undefined}
            data-block-id={running?.id}
          >
            {ready && running && <BlockHeader block={running} copyOutput={copyRunning} />}
            <pre className="helm-block-output" style={xtermShown ? undefined : { display: 'none' }}>
              {ready && xtermShown && !meta.alt && bodyFrom < gridFrom && (
                <RowsView hostId={hostId} paneId={paneId} from={bodyFrom} to={gridFrom} />
              )}
              {/* The band: the viewport-sized xterm, clipped by sliding
                  it up to the running command's first row. `clip`, not
                  `hidden`: a clipped box can't be scrolled by anything
                  (a focus, a caret move), so the band shows exactly the
                  rows it's sized for. */}
              <div className="relative" style={{ height: liveHeight, overflow: 'clip' }}>
                <div
                  ref={xtermHostRef}
                  className="absolute inset-x-0"
                  style={{ top: -liveStartRow * cellH, height: hostHeight, overflow: 'clip' }}
                />
              </div>
            </pre>
          </div>
        </div>
      </div>

      {composerShown && (
        <Composer
          mode={mode}
          kind={ps.kind}
          cwd={paneCwd}
          branch={paneBranch}
          history={history}
          agentName={AGENT_LAUNCH_COMMAND}
          onModeChange={onModeChange}
          onSend={onSend}
          onRaw={(bytes) => void screen.sendInput(hostId, paneId, bytes)}
          onPathComplete={async (path, directoriesOnly) => {
            const result = await commands.sessionPathComplete(
              hostId,
              paneId,
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
      {blocked && (
        <button
          type="button"
          onClick={() => {
            ackBells()
            setFocusKey((k) => k + 1)
          }}
          className="helm-bar text-left"
          title="Reply in the composer"
        >
          <SparkIcon size={14} className="shrink-0 text-[var(--terminal-claude,#D97757)]" />
          <span className="flex-1 text-[12px] text-text-secondary">
            {AGENT_LAUNCH_COMMAND} needs your approval — keys go straight to it
          </span>
          <span className="font-mono text-[11px] text-text-disabled">⏎ reply</span>
        </button>
      )}
      {nativeBar && (
        <div className="helm-bar">
          <span className="flex-1 text-[12px] text-text-tertiary">
            typing in {AGENT_LAUNCH_COMMAND} directly
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

/** Dismiss-on-keystroke: a real keypress in this pane means the user
 * is acting on whatever notifications sat on its window. */
function dismissNotificationsFor(hostId: HostId, paneId: string) {
  const store = useStore.getState()
  const windowId = locatePane(store.sessions.get(hostId), paneId)?.pane.windowId
  if (!windowId) return
  const hasNotif = [...store.notifications.values()].some(
    (n) => n.host_id === hostId && (n.window_id === windowId || n.pane_id === paneId),
  )
  if (hasNotif) void commands.notificationDismissForWindow(hostId, windowId)
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

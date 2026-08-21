/**
 * BlockPane — one pane: finished command blocks as DOM above a live
 * xterm tail, inside a single scroll container.
 *
 * Model: helmd segments the pane's byte stream into blocks (OSC 133).
 * Everything before the last finished block's `end_seq` is history and
 * renders as static DOM (`BlockList`: selectable, searchable, cheap).
 * Everything from that seq onward — the current prompt, the in-flight
 * command and its output — streams into one xterm instance. When a
 * block finishes, the xterm is reset and re-fed from the new `end_seq`
 * so the just-completed command "crystallizes" into a DOM block above
 * it with no visual discontinuity.
 *
 * Alt-screen (TUIs: Claude Code, vim, htop) hides the block list and
 * the xterm takes the whole pane — the terminal owns the grid exactly
 * as any other terminal would.
 *
 * Stays mounted across workspace/window switches: when `isVisible`
 * flips to false the parent hides us via `display: none`; the stream
 * keeps buffering, the xterm keeps consuming, switching back is instant.
 */

import { useEffect, useRef, useState } from 'react'
import { commands } from '@lib/ipc'
import { attachTerminal, getTheme, type HelmTerminal } from '@lib/terminal'
import { locatePane, useStore } from '@lib/store'
import * as stream from '@lib/session/stream'
import * as blocks from '@lib/session/blocks'
import { consumeJump, lastFinished, usePaneBlocks, usePendingJump } from '@lib/session/blocks'
import type { HostId } from '@bindings'
import { BlockList } from './BlockList'
import { SearchOverlay } from './SearchOverlay'

interface BlockPaneProps {
  hostId: HostId
  paneId: string
  isVisible?: boolean
}

/** Insets around the xterm box inside its slot (top/right/bottom/left). */
const XTERM_INSET = { top: 8, right: 8, bottom: 12, left: 20 }

export function BlockPane({ hostId, paneId, isVisible = true }: BlockPaneProps) {
  const rootRef = useRef<HTMLDivElement>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const xtermHostRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<HelmTerminal | null>(null)
  const visibleRef = useRef(isVisible)
  const tailFromRef = useRef<number | null>(null)
  const atBottomRef = useRef(true)

  const [helmTerm, setHelmTerm] = useState<HelmTerminal | null>(null)
  const [ready, setReady] = useState(false)
  const [rootHeight, setRootHeight] = useState(0)
  const [searchOpen, setSearchOpen] = useState(false)
  const [atBottom, setAtBottom] = useState(true)
  const pb = usePaneBlocks(hostId, paneId)
  const jump = usePendingJump(hostId, paneId)

  /** Point the xterm at stream position `from`: reset and re-feed
   * everything from there to the head. Used for first paint and each
   * time a block finishes (its bytes move up into the DOM list). */
  const rotateTail = (term: HelmTerminal['term'], from: number) => {
    const bytes = stream.slice(hostId, paneId, from, stream.head(hostId, paneId))
    term.reset()
    if (bytes && bytes.length > 0) term.write(bytes)
    tailFromRef.current = from
    if (atBottomRef.current) requestAnimationFrame(() => scrollToBottom(scrollRef.current))
  }

  // ---- xterm lifecycle + initial hydration ----
  useEffect(() => {
    const host = xtermHostRef.current
    if (!host) return
    const ac = new AbortController()
    const { previewThemeName, themeName } = useStore.getState()
    const attached = attachTerminal(host, { theme: getTheme(previewThemeName ?? themeName) })
    const { term, dispose } = attached
    termRef.current = attached
    setHelmTerm(attached)

    let unsubTail: (() => void) | null = null
    void (async () => {
      await Promise.all([blocks.ensureLoaded(hostId, paneId), stream.ensureHistory(hostId, paneId)])
      if (ac.signal.aborted) return
      const lastDone = lastFinished(blocks.getPaneBlocks(hostId, paneId).blocks)
      rotateTail(term, lastDone?.end_seq ?? stream.start(hostId, paneId))
      unsubTail = stream.subscribeTail(hostId, paneId, (chunk) => {
        term.write(chunk)
        if (atBottomRef.current) scrollToBottom(scrollRef.current)
      })
      setReady(true)
      void commands.sessionResize(hostId, paneId, term.cols, term.rows)
    })()

    const inputDisp = term.onData((data) => {
      if (ac.signal.aborted || !visibleRef.current) return
      if (isUserKeystroke(data)) dismissNotificationsFor(hostId, paneId)
      void stream.sendInput(hostId, paneId, data)
    })
    const resizeDisp = term.onResize(({ cols, rows }) => {
      if (ac.signal.aborted) return
      void commands.sessionResize(hostId, paneId, cols, rows)
    })
    if (visibleRef.current) term.focus()

    return () => {
      ac.abort()
      unsubTail?.()
      inputDisp.dispose()
      resizeDisp.dispose()
      dispose()
      termRef.current = null
      tailFromRef.current = null
      setHelmTerm(null)
      setReady(false)
    }
  }, [hostId, paneId])

  // ---- sizing: the xterm slot is always exactly one pane tall ----
  useEffect(() => {
    const root = rootRef.current
    if (!root) return
    const ro = new ResizeObserver((entries) => {
      const h = entries[0]?.contentRect.height ?? 0
      setRootHeight(Math.floor(h))
    })
    ro.observe(root)
    setRootHeight(Math.floor(root.getBoundingClientRect().height))
    return () => ro.disconnect()
  }, [])

  useEffect(() => {
    const t = termRef.current
    if (!t || rootHeight <= 0) return
    const id = window.setTimeout(() => {
      try {
        t.fit.fit()
      } catch {
        /* not laid out yet */
      }
    }, 30)
    return () => window.clearTimeout(id)
  }, [rootHeight, ready, pb.altScreen, isVisible])

  // ---- a block finished: crystallize it, rotate the tail ----
  useEffect(() => {
    if (!ready) return
    const t = termRef.current
    if (!t) return
    const lastDone = lastFinished(pb.blocks)
    if (!lastDone || lastDone.end_seq === null) return
    if (lastDone.end_seq <= (tailFromRef.current ?? -1)) return
    rotateTail(t.term, lastDone.end_seq)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pb.blocks, ready, hostId, paneId])

  // ---- visibility: re-fit + focus when we come back on screen ----
  useEffect(() => {
    visibleRef.current = isVisible
    if (!isVisible) return
    const t = termRef.current
    if (!t) return
    try {
      t.fit.fit()
    } catch {
      /* not ready */
    }
    t.term.focus()
  }, [isVisible])

  // ---- Cmd+F: find across blocks + the live tail (visible pane only) ----
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

  // ---- palette jump-to-block: consumed once we're ready + visible ----
  useEffect(() => {
    if (!jump || !ready || !isVisible) return
    const sc = scrollRef.current
    if (!sc) return
    consumeJump(jump)
    const el = jump.blockId ? sc.querySelector<HTMLElement>(`[data-block-id="${jump.blockId}"]`) : null
    if (el) {
      el.scrollIntoView({ block: 'center' })
      el.classList.add('helm-block-flash')
      window.setTimeout(() => el.classList.remove('helm-block-flash'), 1200)
    } else {
      scrollToBottom(sc)
    }
  }, [jump, ready, isVisible])

  const onScroll = () => {
    const sc = scrollRef.current
    if (!sc) return
    const pinned = sc.scrollTop + sc.clientHeight >= sc.scrollHeight - 4
    atBottomRef.current = pinned
    setAtBottom(pinned)
  }

  const slotHeight = Math.max(0, rootHeight)

  return (
    <div ref={rootRef} className="relative h-full w-full overflow-hidden bg-[var(--terminal-bg)]">
      <div
        ref={scrollRef}
        onScroll={onScroll}
        onMouseUp={() => {
          // Clicking in the block area shouldn't strand the keyboard —
          // unless the user is selecting text to copy.
          if (window.getSelection()?.isCollapsed) termRef.current?.term.focus()
        }}
        className="helm-scroll absolute inset-0 overflow-y-auto overflow-x-hidden"
      >
        {ready && !pb.altScreen && <BlockList hostId={hostId} paneId={paneId} blocks={pb.blocks} />}
        <div className="relative" style={{ height: slotHeight }}>
          <div
            ref={xtermHostRef}
            className="absolute overflow-hidden"
            style={{
              top: XTERM_INSET.top,
              right: XTERM_INSET.right,
              bottom: XTERM_INSET.bottom,
              left: XTERM_INSET.left,
            }}
          />
        </div>
      </div>
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
          onClick={() => scrollToBottom(scrollRef.current)}
          title="Jump to latest output"
          className="absolute bottom-3 left-1/2 z-20 flex -translate-x-1/2 items-center gap-1.5 rounded-full border border-white/[0.08] bg-elevated py-1 pl-2.5 pr-3 text-[12px] text-text-secondary hover:text-text-primary"
          style={{ boxShadow: 'var(--elevation-2)' }}
        >
          <ChevronDownIcon />
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

function ChevronDownIcon() {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="13"
      height="13"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="m6 9 6 6 6-6" />
    </svg>
  )
}

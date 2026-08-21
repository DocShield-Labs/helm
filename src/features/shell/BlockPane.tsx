/**
 * BlockPane — one pane: finished command blocks as DOM, one live xterm,
 * and the composer.
 *
 * Model: helmd segments the pane's byte stream into blocks (OSC 133).
 * Everything before the last finished block's `end_seq` is history and
 * renders as static DOM (`BlockList`: selectable, searchable, cheap).
 * Everything from that seq onward streams into one xterm instance.
 * When a block finishes, the xterm is reset and re-fed from the new
 * `end_seq` so the just-completed command "crystallizes" into a DOM
 * block above it.
 *
 * Input is the composer, not the shell's prompt (see paneState.ts):
 *   prompt   → xterm hidden, blocks pinned to the bottom, composer
 *   running  → xterm shows the command; composer hidden for a shell,
 *              shown in Agent mode for an agent (Claude Code)
 *   alt      → the TUI owns the grid; agent composer if it's an agent
 *   raw      → plain terminal (no integration, or the process exited)
 * An agent that rings the bell is waiting on the user (permission
 * prompt, end of turn): the composer closes and keys go straight to
 * the TUI until the user answers or presses ⏎ to reply.
 *
 * Stays mounted across switches: when `isVisible` flips to false the
 * parent hides us via `display: none`; the stream keeps buffering.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import { commands } from '@lib/ipc'
import { attachTerminal, getTheme, type HelmTerminal } from '@lib/terminal'
import { locatePane, useStore } from '@lib/store'
import * as stream from '@lib/session/stream'
import * as blocks from '@lib/session/blocks'
import { consumeJump, lastFinished, usePaneBlocks, usePendingJump } from '@lib/session/blocks'
import {
  forgetPane,
  reportEffective,
  setComposerMode,
  useComposerMode,
  type ComposerMode,
} from '@lib/session/composer'
import { AGENT_LAUNCH_COMMAND, derivePaneState, shellQuote } from '@lib/session/paneState'
import type { HostId } from '@bindings'
import { BlockList } from './BlockList'
import { Composer } from './Composer'
import { SearchOverlay } from './SearchOverlay'
import { ChevronDownIcon, SparkIcon } from '@features/sessions/icons'

interface BlockPaneProps {
  hostId: HostId
  paneId: string
  isVisible?: boolean
}

/** Insets around the xterm box inside its slot. Left matches the
 * blocks' 16px text inset. */
const XTERM_INSET = { top: 8, right: 16, bottom: 8, left: 16 }

export function BlockPane({ hostId, paneId, isVisible = true }: BlockPaneProps) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const xtermHostRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<HelmTerminal | null>(null)
  const visibleRef = useRef(isVisible)
  const tailFromRef = useRef<number | null>(null)
  const atBottomRef = useRef(true)

  const [helmTerm, setHelmTerm] = useState<HelmTerminal | null>(null)
  const [ready, setReady] = useState(false)
  const [slotHeight, setSlotHeight] = useState(0)
  const [searchOpen, setSearchOpen] = useState(false)
  const [atBottom, setAtBottom] = useState(true)
  const [focusKey, setFocusKey] = useState(0)
  const pb = usePaneBlocks(hostId, paneId)
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
  const composerShown =
    ps.phase === 'prompt' || (ps.kind === 'agent' && mode === 'agent' && !blocked)
  /** Agent pane in Terminal mode: typing lands in the TUI; keep the
   * mode control reachable. */
  const nativeBar = ps.kind === 'agent' && mode === 'terminal' && !blocked

  const history = useMemo(() => {
    const out: string[] = []
    for (const b of pb.blocks) {
      const c = b.cmdline?.trim()
      if (!c || b.end_seq === null) continue
      if (out[out.length - 1] !== c) out.push(c)
    }
    return out.length > 200 ? out.slice(-200) : out
  }, [pb.blocks])

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
      void stream.sendInput(hostId, paneId, data)
    })
    const resizeDisp = term.onResize(({ cols, rows }) => {
      if (ac.signal.aborted) return
      void commands.sessionResize(hostId, paneId, cols, rows)
    })

    return () => {
      ac.abort()
      unsubTail?.()
      inputDisp.dispose()
      resizeDisp.dispose()
      dispose()
      forgetPane(hostId, paneId)
      termRef.current = null
      tailFromRef.current = null
      setHelmTerm(null)
      setReady(false)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, paneId])

  // ---- sizing: the xterm slot is exactly the scroll viewport ----
  useEffect(() => {
    const sc = scrollRef.current
    if (!sc) return
    const ro = new ResizeObserver((entries) => {
      setSlotHeight(Math.floor(entries[0]?.contentRect.height ?? 0))
    })
    ro.observe(sc)
    setSlotHeight(Math.floor(sc.getBoundingClientRect().height))
    return () => ro.disconnect()
  }, [])

  useEffect(() => {
    const t = termRef.current
    if (!t || slotHeight <= 0 || !xtermShown || !isVisible) return
    const id = window.setTimeout(() => {
      try {
        t.fit.fit()
      } catch {
        /* not laid out yet */
      }
    }, 30)
    return () => window.clearTimeout(id)
  }, [slotHeight, ready, xtermShown, pb.altScreen, isVisible])

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

  // ---- focus follows the input surface ----
  useEffect(() => {
    visibleRef.current = isVisible
    if (!isVisible) return
    if (composerShown) {
      setFocusKey((k) => k + 1)
    } else {
      termRef.current?.term.focus()
    }
  }, [isVisible, composerShown, ready])

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

  const focusInput = () => {
    if (composerShown) setFocusKey((k) => k + 1)
    else termRef.current?.term.focus()
  }

  /** Text to the pane as typed input, ending with ⏎. Multi-line goes
   * as a bracketed paste so the shell (or agent) takes it whole. */
  const sendText = (text: string) => {
    dismissNotificationsFor(hostId, paneId)
    if (text.includes('\n')) {
      void stream.sendInput(hostId, paneId, `\x1b[200~${text}\x1b[201~`).then(
        () => new Promise<void>((r) => window.setTimeout(r, 30)),
      ).then(() => stream.sendInput(hostId, paneId, '\r'))
    } else {
      void stream.sendInput(hostId, paneId, `${text}\r`)
    }
    atBottomRef.current = true
  }

  const onSend = (text: string) => {
    if (mode === 'agent' && ps.kind !== 'agent') {
      sendText(`${AGENT_LAUNCH_COMMAND} ${shellQuote(text)}`)
    } else {
      sendText(text)
    }
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
          // unless the user is selecting text to copy.
          if (window.getSelection()?.isCollapsed) focusInput()
        }}
        className="helm-scroll relative flex min-h-0 flex-1 flex-col overflow-y-auto overflow-x-hidden"
      >
        {ready && !pb.altScreen && (
          <div className="mt-auto">
            <BlockList hostId={hostId} paneId={paneId} blocks={pb.blocks} />
          </div>
        )}
        <div
          className="relative shrink-0"
          style={{
            height: xtermShown ? slotHeight : 0,
            visibility: xtermShown ? 'visible' : 'hidden',
          }}
        >
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
          onRaw={(bytes) => void stream.sendInput(hostId, paneId, bytes)}
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
            {AGENT_LAUNCH_COMMAND} is waiting for you — keys go straight to it
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
          onClick={() => scrollToBottom(scrollRef.current)}
          title="Jump to latest output"
          className="absolute left-1/2 z-20 flex -translate-x-1/2 items-center gap-1.5 rounded-full border border-[var(--stroke-default)] bg-elevated py-1 pl-2.5 pr-3 text-[12px] text-text-secondary hover:text-text-primary"
          style={{ bottom: composerShown ? 120 : 16, boxShadow: 'var(--elevation-2)' }}
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

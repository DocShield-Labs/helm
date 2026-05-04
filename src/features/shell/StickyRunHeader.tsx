/**
 * StickyRunHeader — slim 24px bar pinned at the top of a pane while a
 * running block has scrolled out of view. Shows `● <command> · <duration>`
 * so the user always sees what's running. Clicking jumps xterm back to
 * the block's start row.
 */

import { useEffect, useState } from 'react'
import type { Terminal } from '@xterm/xterm'
import { formatDuration } from '@lib/format'
import type { BlockSnapshot } from './blockTracker'

interface Props {
  term: Terminal
  blocks: BlockSnapshot[]
  /** Called with the absolute buffer line we should scroll back to. */
  onJumpTo: (line: number) => void
}

export function StickyRunHeader({ term, blocks, onJumpTo }: Props) {
  const [, force] = useState(0)

  // Tick once a second so the duration display advances. The block's
  // `startedAt` doesn't change while running, so we only need a wall-
  // clock heartbeat — no event subscription.
  useEffect(() => {
    const id = window.setInterval(() => force((t) => t + 1), 1000)
    return () => window.clearInterval(id)
  }, [])

  // Re-render when xterm scrolls so we know whether the running block's
  // header row is on screen.
  useEffect(() => {
    const onScroll = () => force((t) => t + 1)
    const disp = term.onScroll(onScroll)
    return () => disp.dispose()
  }, [term])

  const running = blocks.find((b) => b.status === 'running' && b.startLine >= 0)
  if (!running) return null

  // Hide the sticky header when the block's row is currently visible —
  // no need to duplicate it.
  const viewportY = term.buffer.active.viewportY
  if (running.startLine >= viewportY) return null

  const elapsed = formatDuration(
    Date.now() - (running.commandStartedAt ?? running.openedAt),
  )
  const command = running.command ?? '(unknown command)'

  return (
    <button
      type="button"
      onClick={() => onJumpTo(running.startLine)}
      className="absolute left-0 right-0 top-0 z-10 flex h-6 items-center gap-2 border-b border-white/[0.06] bg-[#14171b]/90 px-4 font-mono text-[11px] text-[#9da4ad] backdrop-blur transition-colors hover:bg-[#14171b]"
      title="Jump back to running block"
    >
      <span
        className="size-1.5 rounded-full bg-[#5cb97d]"
        style={{ boxShadow: '0 0 6px rgba(92,185,125,0.7)' }}
      />
      <span className="font-medium text-[#ecedee]">{command}</span>
      <span className="text-[#6b7380]">· {elapsed}</span>
    </button>
  )
}

/**
 * Finished blocks for a session, oldest first, each with the row range it
 * should render right now — the one place that knows how the loaded
 * history, the live grid and a block's line range combine:
 *
 *   - blocks entirely above the loaded history wait for their page
 *     (history pages in backwards as the user scrolls up), and blocks
 *     the daemon no longer retains never appear.
 */

import { useMemo } from 'react'
import type { BlockInfo, HostId } from '@bindings'
import { blockRange, isRenderable } from '@lib/session/blocks'
import { Block } from './Block'

/** Hard cap on blocks in the DOM; rows inside are chunked, so this
 * bounds block chrome, not output. */
const MAX_RENDERED = 400

interface BlockListProps {
  hostId: HostId
  sessionId: string
  blocks: readonly BlockInfo[]
  clearedBefore: number
  /** Absolute line the render window starts at: rows below it don't
   * exist as DOM. Unlike `clearedBefore` (which drops whole blocks —
   * `clear` semantics), this CLIPS a block straddling the floor to the
   * rows inside the window. Already clamped by the caller to the loaded
   * floor, so it is the one clipping bound. */
  renderFrom: number
}

export function BlockList({ hostId, sessionId, blocks, clearedBefore, renderFrom }: BlockListProps) {
  const shown = useMemo(() => {
    const from = renderFrom
    const out: Array<{ block: BlockInfo; lo: number; hi: number }> = []
    for (const b of blocks) {
      if (b.start_line < clearedBefore || !isRenderable(b)) continue
      const [bodyStart, end] = blockRange(b)
      if (end <= from) continue
      out.push({ block: b, lo: Math.max(bodyStart, from), hi: end })
    }
    return out.length > MAX_RENDERED ? out.slice(out.length - MAX_RENDERED) : out
  }, [blocks, clearedBefore, renderFrom])
  if (shown.length === 0) return null
  return (
    <div>
      {shown.map(({ block, lo, hi }, i) => (
        <div key={block.id}>
          {i > 0 && <div className="helm-divider" />}
          <Block hostId={hostId} sessionId={sessionId} block={block} lo={lo} hi={hi} />
        </div>
      ))}
      <div className="helm-divider" />
    </div>
  )
}

/**
 * Finished blocks for a pane, oldest first, each with the row range it
 * should render right now — the one place that knows how the loaded
 * history, the live grid and a block's line range combine:
 *
 *   - rows from `gridFrom` on are the live grid's (the running
 *     command's band); everything below that line renders as DOM, from
 *     history or from the mirror of the grid;
 *   - blocks entirely above the loaded history wait for their page
 *     (history pages in backwards as the user scrolls up), and blocks
 *     the daemon no longer retains never appear.
 */

import { useMemo } from 'react'
import type { BlockInfo, HostId } from '@bindings'
import { blockRange, isRenderable } from '@lib/session/blocks'
import type { ScreenMeta } from '@lib/session/screen'
import { Block } from './Block'

/** Hard cap on blocks in the DOM; rows inside are chunked, so this
 * bounds block chrome, not output. */
const MAX_RENDERED = 400

interface BlockListProps {
  hostId: HostId
  paneId: string
  blocks: readonly BlockInfo[]
  clearedBefore: number
  meta: ScreenMeta
  /** First absolute line the live grid is showing (Infinity: none). */
  gridFrom: number
}

export function BlockList({ hostId, paneId, blocks, clearedBefore, meta, gridFrom }: BlockListProps) {
  const { loadedFrom, topLine } = meta
  const shown = useMemo(() => {
    const from = loadedFrom ?? topLine
    const out: Array<{ block: BlockInfo; lo: number; hi: number }> = []
    for (const b of blocks) {
      if (b.start_line < clearedBefore || !isRenderable(b)) continue
      const [bodyStart, end] = blockRange(b)
      if (end <= from) continue
      out.push({ block: b, lo: Math.max(bodyStart, from), hi: Math.min(end, gridFrom) })
    }
    return out.length > MAX_RENDERED ? out.slice(out.length - MAX_RENDERED) : out
  }, [blocks, clearedBefore, loadedFrom, topLine, gridFrom])
  if (shown.length === 0) return null
  return (
    <div>
      {shown.map(({ block, lo, hi }, i) => (
        <div key={block.id}>
          {i > 0 && <div className="helm-divider" />}
          <Block hostId={hostId} paneId={paneId} block={block} lo={lo} hi={hi} />
        </div>
      ))}
      <div className="helm-divider" />
    </div>
  )
}

/**
 * The finished-command list above the live tail. Blocks are separated
 * by full-width hairlines (Warp). Render is capped to the most recent
 * blocks with a "show older" affordance — a full virtualizer can come
 * later if real usage demands it.
 */

import { useState } from 'react'
import type { BlockInfo, HostId } from '@bindings'
import { isRenderable } from '@lib/session/blocks'
import { Block } from './Block'

const MAX_RENDERED = 250

interface BlockListProps {
  hostId: HostId
  paneId: string
  blocks: readonly BlockInfo[]
}

export function BlockList({ hostId, paneId, blocks }: BlockListProps) {
  const [extra, setExtra] = useState(0)
  const finished = blocks.filter(isRenderable)
  const limit = MAX_RENDERED + extra
  const hidden = Math.max(0, finished.length - limit)
  const shown = hidden > 0 ? finished.slice(hidden) : finished
  if (shown.length === 0) return null
  return (
    <div>
      {hidden > 0 && (
        <button
          type="button"
          onClick={() => setExtra((e) => e + MAX_RENDERED)}
          className="mx-4 my-2 rounded-md border border-[var(--stroke-default)] px-2.5 py-1 text-[11px] text-text-secondary hover:text-text-primary"
        >
          show {Math.min(hidden, MAX_RENDERED)} older {hidden === 1 ? 'block' : 'blocks'}
        </button>
      )}
      {shown.map((b, i) => (
        <div key={b.id}>
          {i > 0 && <div className="helm-divider" />}
          <Block hostId={hostId} paneId={paneId} block={b} />
        </div>
      ))}
      <div className="helm-divider" />
    </div>
  )
}

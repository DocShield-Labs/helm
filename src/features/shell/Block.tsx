/**
 * One finished command block at Warp's geometry: a prompt row (cwd on
 * branch, duration right-aligned), the command line, then the output —
 * the rows in `[lo, hi)`, decided by `BlockList` (rows the grid is
 * showing are left to it; rows above the loaded history wait for their
 * page). Failed commands washed red; a hover belt (copy command / copy
 * output) at the top right.
 */

import { memo, type CSSProperties } from 'react'
import type { BlockInfo, HostId } from '@bindings'
import { formatDuration } from '@lib/format'
import { homeRelative } from '@lib/path'
import { getSessionScreen, rowsBetween } from '@lib/session/screen'
import { CopyIcon, TerminalIcon } from '@features/sessions/icons'
import { RowsView, rowsToText } from './Rows'

interface BlockProps {
  hostId: HostId
  sessionId: string
  block: BlockInfo
  /** Rows to render, `[lo, hi)`. */
  lo: number
  hi: number
}

export const Block = memo(function Block({ hostId, sessionId, block, lo, hi }: BlockProps) {
  const failed = block.exit_code !== null && block.exit_code !== 0
  const copyOutput = () =>
    void navigator.clipboard.writeText(rowsToText(rowsBetween(getSessionScreen(hostId, sessionId), lo, hi)))

  return (
    <div
      data-block-id={block.id}
      className={`helm-block group relative ${failed ? 'helm-block-failed' : ''}`}
    >
      <BlockHeader block={block} copyOutput={copyOutput} />
      {hi > lo ? (
        <pre className="helm-block-output">
          <RowsView hostId={hostId} sessionId={sessionId} from={lo} to={hi} />
        </pre>
      ) : (
        <div className="h-4" />
      )}
    </div>
  )
})

/** The prompt row, the command line and the hover belt — shared by
 * finished blocks and the running one (which has no exit or duration
 * yet). Sticky so the command stays in view while its output scrolls. */
export function BlockHeader({ block, copyOutput }: { block: BlockInfo; copyOutput: () => void }) {
  const failed = block.exit_code !== null && block.exit_code !== 0
  const duration =
    block.started_at_ms !== null && block.finished_at_ms !== null
      ? formatDuration(block.finished_at_ms - block.started_at_ms)
      : ''
  const cwd = homeRelative(block.cwd)
  return (
    <div className="helm-block-header sticky top-0 z-10">
      <div className="helm-prompt-row">
        <span className="min-w-0 truncate">
          {cwd && <span>{cwd}</span>}
          {block.branch && (
            <>
              <span className="text-text-disabled"> on </span>
              <span>{block.branch}</span>
            </>
          )}
        </span>
        <span className="flex-1" />
        <span
          className={`shrink-0 select-none group-hover:invisible ${
            failed ? 'text-[var(--terminal-failed)]' : ''
          }`}
        >
          {failed ? `exit ${block.exit_code}${duration ? ` · ${duration}` : ''}` : duration}
        </span>
      </div>
      <div className="helm-cmd-row">{block.cmdline ?? ''}</div>
      <div className="helm-belt opacity-0 transition-opacity group-hover:opacity-100">
        <BeltButton
          title="Copy command"
          onClick={() => void navigator.clipboard.writeText(block.cmdline ?? '')}
        >
          <TerminalIcon size={14} />
        </BeltButton>
        <BeltButton title="Copy output" onClick={copyOutput}>
          <CopyIcon size={14} />
        </BeltButton>
      </div>
    </div>
  )
}

function BeltButton({
  title,
  onClick,
  children,
}: {
  title: string
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      className="flex h-6 w-6 items-center justify-center rounded-[4px] text-text-tertiary hover:bg-white/[0.06] hover:text-text-primary"
      style={{ cursor: 'pointer' } satisfies CSSProperties}
    >
      {children}
    </button>
  )
}

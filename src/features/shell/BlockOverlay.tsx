/**
 * BlockOverlay — block chrome rendered as React absolutes over xterm.
 *
 * Pointer-events: the overlay is `pointer-events: none` end-to-end so
 * wheel and click pass through to xterm. Only the action-chip card
 * re-enables interactivity for itself. Hover tracking lives in TmuxPane
 * because these divs can't fire mouseEnter under `pointer-events: none`.
 *
 * Visual model (lifted from Warp's `block_list_element.rs`):
 *   - Blocks are flat — no card, no border, no rounded corners.
 *   - The only structural separator is a 1-px hairline between blocks
 *     at fg @ 10%, painted in the gap rows the integration script
 *     reserves between adjacent blocks.
 *   - State signals via full-bleed low-opacity overlays:
 *       failed   → red wash @ 10% + 5-px red flag pole on the left.
 *       selected → accent wash @ 18% + 2-px accent outline (suppresses
 *                  the failed flag pole).
 *       hover    → no row fill; only the action-chip card fades in.
 *   - Inline duration chip on the cwd row carries the live status cue.
 */

import { useEffect, useState } from 'react'
import { commands } from '@lib/ipc'
import { formatDuration } from '@lib/format'
import type { HostId } from '@bindings'
import {
  clipBlockToViewport,
  HOST_PADDING_LEFT,
  HOST_PADDING_TOP,
  type BlockSnapshot,
} from './blockTracker'
import type { HelmTerminal } from '@lib/terminal'

interface Props {
  helm: HelmTerminal
  blocks: BlockSnapshot[]
  selectedId: string | null
  hoveredId: string | null
  hostId: HostId
  paneId: string
  /** Whether the parent considers the input atPrompt — re-run is gated
   * on this so we don't dispatch into the middle of a running command. */
  canDispatch: boolean
  /** Scroll the pane to a particular block (used by the chip's "jump
   * to" affordance). */
  onJumpTo: (line: number) => void
}

/** Width of the failed-block left stripe. From Warp's
 * `LEFT_STRIPE_WIDTH = 5.0` in `app/src/terminal/warpify/render.rs`. */
const FLAG_POLE_WIDTH = 5

/** Cells of horizontal gap between the cwd · branch text and the
 * inline status chip. */
const HEADER_CHIP_GAP_CELLS = 2

export function BlockOverlay({
  helm,
  blocks,
  selectedId,
  hoveredId,
  hostId,
  paneId,
  canDispatch,
  onJumpTo,
}: Props) {
  const term = helm.term
  const [, setTick] = useState(0)

  // Re-render on viewport scroll. We deliberately do NOT subscribe to
  // `term.onRender` here — that fires on every paint while output
  // streams (30–60 Hz under build / log-tail), which would cost a full
  // overlay reconciliation per frame for no visual benefit. Block
  // positions only change on scroll or when the block list itself
  // changes, both of which already trigger re-renders.
  useEffect(() => {
    const dispose = term.onScroll(() => setTick((t) => t + 1))
    return () => dispose.dispose()
  }, [term])

  // 1-Hz tick so the running-block duration chip advances. Only
  // schedule when there's actually a running block — idle terminals
  // do zero work.
  const hasRunning = blocks.some((b) => b.status === 'running')
  useEffect(() => {
    if (!hasRunning) return
    const id = window.setInterval(() => setTick((t) => t + 1), 1000)
    return () => window.clearInterval(id)
  }, [hasRunning])

  const { width: cellW, height: cellH } = helm.getCellSize()

  return (
    <div className="pointer-events-none absolute inset-0 z-10 overflow-hidden">
      {blocks.map((block) => {
        const clipped = clipBlockToViewport(
          block.startLine,
          block.endLine,
          term,
        )
        if (!clipped) return null
        const { visibleTop, visibleBottom } = clipped
        const viewportY = term.buffer.active.viewportY
        const viewportEnd = viewportY + term.rows - 1

        const startTop = (visibleTop - viewportY) * cellH
        const heightPx = (visibleBottom - visibleTop + 1) * cellH

        const isSelected = block.id === selectedId
        const isHovered = block.id === hoveredId
        const isPending = block.status === 'pending'
        const isFailed = block.status === 'failed'
        const showChips = (isHovered || isSelected) && !isPending

        const tintColor = isSelected
          ? 'var(--terminal-accent-18)'
          : isFailed
            ? 'var(--terminal-failed-10)'
            : 'transparent'

        const showFlagPole = isFailed && !isSelected

        // Chip + divider both anchor at the cwd row. `headerRow` is
        // -1 until BlockTracker has actually seen the header text
        // print (between A and the first `print -P` output), at which
        // point it stays valid because IMarker tracks the row.
        const headerRow = block.headerRow
        const chipVisible =
          headerRow >= 0 &&
          headerRow >= viewportY &&
          headerRow <= viewportEnd
        const headerOffsetPx = (headerRow - block.startLine) * cellH
        const chipLeft =
          HOST_PADDING_LEFT +
          (block.headerText.length + HEADER_CHIP_GAP_CELLS) * cellW

        return (
          <div
            key={block.id}
            className="pointer-events-none absolute"
            style={{
              top: HOST_PADDING_TOP + startTop,
              left: 0,
              right: 0,
              height: heightPx,
              // Clips the action-chip card so it can't bleed into a
              // sibling block when the header row is partially scrolled
              // off. Dividers are rendered outside the wrapper (see the
              // separate pass below) so they aren't affected.
              overflow: 'hidden',
            }}
          >
            {tintColor !== 'transparent' && (
              <div
                className="absolute inset-0 transition-colors duration-100"
                style={{ background: tintColor }}
              />
            )}
            {showFlagPole && (
              <div
                className="absolute top-0 bottom-0 left-0"
                style={{
                  width: FLAG_POLE_WIDTH,
                  background: 'var(--terminal-failed)',
                }}
              />
            )}
            {isSelected && (
              <div
                className="pointer-events-none absolute inset-0"
                style={{
                  outline: '2px solid var(--terminal-accent)',
                  outlineOffset: -2,
                }}
              />
            )}

            {chipVisible && (
              <StatusChip
                top={headerOffsetPx}
                left={chipLeft}
                height={cellH}
                block={block}
              />
            )}

            <div
              className={`pointer-events-auto absolute right-3 z-20 flex items-center gap-0.5 rounded-md px-1.5 py-1 text-[10px] font-medium tracking-tight shadow-md backdrop-blur transition-opacity duration-100 ${
                showChips ? 'opacity-100' : 'pointer-events-none opacity-0'
              }`}
              style={{
                top: cellH + 4,
                background: 'var(--terminal-chip-bg)',
                border: '1px solid var(--terminal-fg-06)',
                color: 'var(--terminal-fg-60)',
              }}
              data-block-chip
            >
              {block.command && (
                <ActionButton
                  onClick={() => copyToClipboard(block.command ?? '')}
                  title="Copy command (⌘⇧C)"
                >
                  cmd
                </ActionButton>
              )}
              <ActionButton
                onClick={() => copyOutputToClipboard(term, block)}
                title="Copy output (⌘C)"
              >
                output
              </ActionButton>
              {block.command && (
                <ActionButton
                  disabled={!canDispatch}
                  onClick={() => rerunBlock(hostId, paneId, block.command!)}
                  title="Re-run (⌘R)"
                >
                  ↻
                </ActionButton>
              )}
              <ActionButton
                onClick={() => onJumpTo(block.startLine)}
                title="Scroll to block"
              >
                ↑
              </ActionButton>
            </div>
          </div>
        )
      })}

      {/* Hairlines, one per block boundary, painted in the gap row
          between blocks. Rendered outside the per-block wrapper so the
          wrapper's `overflow: hidden` (kept for the action-chip card)
          doesn't clip them. */}
      {blocks.map((block, idx) => {
        if (idx === 0 || block.startLine <= 0) return null
        const viewportY = term.buffer.active.viewportY
        const viewportEnd = viewportY + term.rows - 1
        const dividerRow = block.startLine - 1
        if (dividerRow < viewportY || dividerRow > viewportEnd) return null
        const dividerY = HOST_PADDING_TOP + (dividerRow - viewportY) * cellH
        return (
          <div
            key={`d-${block.id}`}
            className="pointer-events-none absolute left-0 right-0 h-px"
            style={{ top: dividerY, background: 'var(--terminal-divider)' }}
          />
        )
      })}
    </div>
  )
}

/** Inline status chip on the cwd row — running shows live elapsed,
 * failed shows exit + duration, ok shows duration. Rendered as a
 * single element so the styling stays consistent across statuses. */
function StatusChip({
  top,
  left,
  height,
  block,
}: {
  top: number
  left: number
  height: number
  block: BlockSnapshot
}) {
  const duration =
    block.commandStartedAt !== null && block.endedAt !== null
      ? formatDuration(block.endedAt - block.commandStartedAt)
      : null
  const elapsed =
    block.commandStartedAt !== null && block.endedAt === null
      ? formatDuration(Date.now() - block.commandStartedAt)
      : null

  let body: React.ReactNode = null
  if (block.status === 'running' && elapsed !== null) {
    body = <span style={{ color: 'var(--terminal-accent)' }}>{elapsed}</span>
  } else if (block.status === 'failed') {
    body = (
      <>
        <span style={{ color: 'var(--terminal-failed)' }}>
          exit {block.exitCode ?? '?'}
        </span>
        {duration !== null && (
          <>
            <span style={{ color: 'var(--terminal-fg-30)' }}>·</span>
            <span style={{ color: 'var(--terminal-fg-60)' }}>{duration}</span>
          </>
        )}
      </>
    )
  } else if (block.status === 'ok' && duration !== null) {
    body = <span style={{ color: 'var(--terminal-fg-60)' }}>{duration}</span>
  }
  if (body === null) return null

  return (
    <div
      className="absolute select-none flex items-center gap-1.5 font-mono text-[12px]"
      style={{ top, left, height }}
    >
      <span style={{ color: 'var(--terminal-fg-30)' }}>·</span>
      {body}
    </div>
  )
}

function ActionButton({
  onClick,
  title,
  disabled,
  children,
}: {
  onClick: () => void
  title: string
  disabled?: boolean
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      title={title}
      className="rounded px-1.5 py-0.5 enabled:hover:bg-white/[0.06] enabled:hover:text-[var(--terminal-fg)] disabled:opacity-40"
    >
      {children}
    </button>
  )
}

async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    /* clipboard API may be blocked in some webview contexts */
  }
}

async function copyOutputToClipboard(
  term: HelmTerminal['term'],
  block: BlockSnapshot,
): Promise<void> {
  if (block.startLine < 0) return
  const end = block.endLine ?? term.buffer.active.baseY + term.buffer.active.cursorY
  const lines: string[] = []
  // Skip the cwd · branch header row — copied output starts at the
  // command itself and runs through the last output line.
  for (let row = block.startLine + 1; row <= end; row++) {
    const line = term.buffer.active.getLine(row)
    if (!line) continue
    lines.push(line.translateToString(true).replace(/\s+$/, ''))
  }
  await copyToClipboard(lines.join('\n'))
}

async function rerunBlock(
  hostId: HostId,
  paneId: string,
  command: string,
): Promise<void> {
  const encoder = new TextEncoder()
  const bytes = Array.from(encoder.encode(command + '\r'))
  await commands.tmuxSendKeys(hostId, paneId, bytes)
}

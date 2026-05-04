/**
 * BlockOverlay — block chrome rendered as React absolutes over xterm.
 *
 * Pointer-events policy: this whole overlay is `pointer-events: none`
 * end-to-end so wheel events scroll xterm naturally and clicks select
 * text. The action chip card is the only interactive surface and
 * re-enables `pointer-events: auto` for itself.
 *
 * Hover detection lives in the parent (TmuxPane) — it tracks the
 * cursor's Y position over the pane wrapper and computes which block
 * the cursor is over from the row ranges. The result is passed in as
 * `hoveredId`. We can't use mouseEnter on these block divs because
 * they're pointer-events: none.
 *
 * What lives here:
 *   - 2px coloured left border per block, drawn in the host's left
 *     padding gap so glyphs can't ever cover it.
 *   - Subtle background tint over the block's row range while hovered
 *     or selected, plus a faint persistent wash on failed blocks.
 *   - Inline status chip (running · 14s, exit 1 · 2s, plain duration)
 *     positioned at the end of the shell-printed cwd · branch text by
 *     measuring xterm cell width.
 *   - Hover-revealed action chips (cmd / output / re-run / jump-to).
 *
 * Re-render cadence: ticks on `term.onScroll` and `term.onRender` (so
 * running blocks' borders/backgrounds grow live with output) plus a
 * 1-second wall-clock interval so the running chip's elapsed advances.
 */

import { useEffect, useRef, useState } from 'react'
import type { Terminal } from '@xterm/xterm'
import { commands } from '@lib/ipc'
import { formatDuration } from '@lib/format'
import type { HostId } from '@bindings'
import { clipBlockToViewport, type BlockSnapshot } from './blockTracker'

interface Props {
  term: Terminal
  hostElement: HTMLElement
  blocks: BlockSnapshot[]
  selectedId: string | null
  hoveredId: string | null
  hostId: HostId
  paneId: string
  /** Whether the parent considers the input atPrompt — re-run is
   * gated on this so we don't dispatch a command into the middle of a
   * running command. */
  canDispatch: boolean
  /** Called when the user wants to scroll the pane to a particular
   * block. Used by the chip's "jump to" affordance. */
  onJumpTo: (line: number) => void
}

/** xterm host has `pl-6 pr-2 py-2`. Coordinates here are
 * relative to the OUTER pane wrapper (the same parent xterm sits in),
 * so we offset by the host's padding to line up with rendered rows. */
const HOST_PADDING_TOP = 8
const HOST_PADDING_LEFT = 24
/** Distance from the screen edge to the centre of the left border.
 * Sits inside the xterm padding gap so it can't be occluded by glyphs. */
const BORDER_LEFT = 10
/** Gap between the end of the shell-printed header text and the
 * inline running/exit chip. */
const HEADER_CHIP_GAP_CELLS = 2

export function BlockOverlay({
  term,
  hostElement,
  blocks,
  selectedId,
  hoveredId,
  hostId,
  paneId,
  canDispatch,
  onJumpTo,
}: Props) {
  const [, setTick] = useState(0)
  const cellHeightRef = useRef(17)
  const cellWidthRef = useRef(8)

  useEffect(() => {
    const force = () => setTick((t) => t + 1)
    const scrollDisp = term.onScroll(force)
    const renderDisp = term.onRender(force)
    const id = window.setInterval(force, 1000)
    return () => {
      scrollDisp.dispose()
      renderDisp.dispose()
      window.clearInterval(id)
    }
  }, [term])

  useEffect(() => {
    const measure = () => {
      const row = hostElement.querySelector('.xterm-rows > div') as HTMLElement | null
      if (row) {
        const rect = row.getBoundingClientRect()
        if (rect.height > 0) cellHeightRef.current = rect.height
        if (rect.width > 0 && term.cols > 0) {
          cellWidthRef.current = rect.width / term.cols
        }
      }
    }
    measure()
    const ro = new ResizeObserver(measure)
    ro.observe(hostElement)
    return () => ro.disconnect()
  }, [hostElement, term])

  const cellH = cellHeightRef.current
  const cellW = cellWidthRef.current

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
        // Distance from the *clipped* top down to the original start
        // row. Used to figure out where on the still-visible portion
        // the inline chip lives — when the start row scrolls past the
        // viewport, the chip is hidden because its row (start + 1) is
        // outside the visible intersection.
        const startRowOffsetPx = (block.startLine - visibleTop) * cellH

        const isSelected = block.id === selectedId
        const isHovered = block.id === hoveredId
        const isPending = block.status === 'pending'
        const isRunning = block.status === 'running'
        const isFailed = block.status === 'failed'
        const showChips = (isHovered || isSelected) && !isPending

        const borderColor = isFailed
          ? '#f7768e'
          : isRunning
            ? '#7aa2f7'
            : isSelected
              ? 'rgba(122,162,247,0.55)'
              : isHovered
                ? 'rgba(255,255,255,0.10)'
                : 'transparent'

        const tintColor = isFailed
          ? 'rgba(247,118,142,0.04)'
          : isHovered || isSelected
            ? 'rgba(255,255,255,0.025)'
            : 'transparent'

        // The block's first row (`startLine`) is the blank top-pad
        // row that the integration script reserves; the cwd · branch
        // header text is one row below that. Measure the header row
        // length to land the inline chip immediately after the text.
        const headerRow = block.startLine + 1
        const headerLine = term.buffer.active.getLine(headerRow)
        const headerText =
          headerLine?.translateToString(true).replace(/\s+$/, '') ?? ''
        const chipLeft =
          HOST_PADDING_LEFT +
          headerText.length * cellW +
          HEADER_CHIP_GAP_CELLS * cellW
        // Chip sits on the header row (one cell-height below the block
        // start). When the block has scrolled so the header row is
        // above the visible top, the chip would land at a negative
        // offset within our clipped wrapper — hide it in that case.
        const chipTop = cellH + startRowOffsetPx
        const chipVisible = headerRow >= viewportY && headerRow <= viewportEnd

        return (
          <div
            key={block.id}
            className="pointer-events-none absolute"
            style={{
              top: HOST_PADDING_TOP + startTop,
              left: 0,
              right: 0,
              height: heightPx,
              // Clip overflow so the chip / chips card don't leak
              // into a sibling block's territory when the header row
              // is hidden by the clip.
              overflow: 'hidden',
            }}
          >
            <div
              className="absolute inset-0 transition-colors duration-100"
              style={{ background: tintColor }}
            />
            <div
              className="absolute top-0 bottom-0 transition-all duration-100"
              style={{
                left: BORDER_LEFT,
                width: isSelected ? 3 : 2,
                background: borderColor,
                borderRadius: 1,
              }}
            />

            {chipVisible && isRunning && block.commandStartedAt !== null && (
              <div
                className="absolute select-none flex items-center gap-1.5 font-mono text-[12px]"
                style={{ top: chipTop, left: chipLeft, height: cellH }}
              >
                <span className="text-[#404853]">·</span>
                {/* Accent blue, matches the cursor / prompt chevron —
                    a quiet "this one is live" indicator. */}
                <span className="text-[#7aa2f7]">
                  {formatDuration(Date.now() - block.commandStartedAt)}
                </span>
              </div>
            )}
            {chipVisible && isFailed && (
              <div
                className="absolute select-none flex items-center gap-1.5 font-mono text-[12px]"
                style={{ top: chipTop, left: chipLeft, height: cellH }}
              >
                <span className="text-[#404853]">·</span>
                <span className="text-[#f7768e]">
                  exit {block.exitCode ?? '?'}
                </span>
                {block.endedAt && block.commandStartedAt && (
                  <>
                    <span className="text-[#404853]">·</span>
                    <span className="text-[#6b7380]">
                      {formatDuration(block.endedAt - block.commandStartedAt)}
                    </span>
                  </>
                )}
              </div>
            )}
            {chipVisible &&
              block.status === 'ok' &&
              block.endedAt &&
              block.commandStartedAt && (
                <div
                  className="absolute select-none flex items-center gap-1.5 font-mono text-[12px] text-[#6b7380]"
                  style={{ top: chipTop, left: chipLeft, height: cellH }}
                >
                  <span className="text-[#404853]">·</span>
                  <span>{formatDuration(block.endedAt - block.commandStartedAt)}</span>
                </div>
              )}

            <div
              className={`pointer-events-auto absolute right-3 z-20 flex items-center gap-1 rounded-md border border-white/[0.06] bg-[#14171b]/95 px-1.5 py-1 text-[10px] font-medium tracking-tight text-[#9da4ad] shadow-md backdrop-blur transition-opacity duration-100 ${
                showChips ? 'opacity-100' : 'pointer-events-none opacity-0'
              }`}
              style={{ top: cellH + 4 }}
              data-block-chip
            >
              {block.command && (
                <button
                  type="button"
                  className="rounded px-1.5 py-0.5 hover:bg-white/[0.06] hover:text-[#ecedee]"
                  onClick={() => copyToClipboard(block.command ?? '')}
                  title="Copy command (⌘⇧C)"
                >
                  cmd
                </button>
              )}
              <button
                type="button"
                className="rounded px-1.5 py-0.5 hover:bg-white/[0.06] hover:text-[#ecedee]"
                onClick={() => copyOutputToClipboard(term, block)}
                title="Copy output (⌘C)"
              >
                output
              </button>
              {block.command && (
                <button
                  type="button"
                  disabled={!canDispatch}
                  className="rounded px-1.5 py-0.5 enabled:hover:bg-white/[0.06] enabled:hover:text-[#ecedee] disabled:opacity-40"
                  onClick={() => rerunBlock(hostId, paneId, block.command!)}
                  title="Re-run (⌘R)"
                >
                  ↻
                </button>
              )}
              <button
                type="button"
                className="rounded px-1.5 py-0.5 hover:bg-white/[0.06] hover:text-[#ecedee]"
                onClick={() => onJumpTo(block.startLine)}
                title="Scroll to block"
              >
                ↑
              </button>
            </div>
          </div>
        )
      })}
    </div>
  )
}

async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    /* clipboard API may be blocked in some webview contexts */
  }
}

async function copyOutputToClipboard(term: Terminal, block: BlockSnapshot): Promise<void> {
  if (block.startLine < 0) return
  const end = block.endLine ?? term.buffer.active.baseY + term.buffer.active.cursorY
  const lines: string[] = []
  // Skip the first 2 rows (top-pad blank + cwd · branch header) — the
  // copied output starts at the prompt row's command (which we'll
  // include) and runs through the last output line.
  for (let row = block.startLine + 2; row <= end; row++) {
    const line = term.buffer.active.getLine(row)
    if (!line) continue
    lines.push(line.translateToString(true).replace(/\s+$/, ''))
  }
  await copyToClipboard(lines.join('\n'))
}

async function rerunBlock(hostId: HostId, paneId: string, command: string): Promise<void> {
  const encoder = new TextEncoder()
  const bytes = Array.from(encoder.encode(command + '\r'))
  await commands.tmuxSendKeys(hostId, paneId, bytes)
}

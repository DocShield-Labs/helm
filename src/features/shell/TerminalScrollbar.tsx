/**
 * TerminalScrollbar — custom React scrollbar for an xterm pane.
 *
 * xterm's native browser scrollbar (`.xterm-viewport::-webkit-scrollbar`)
 * has two problems for us:
 *   1. It sits on the viewport DOM layer, *underneath* `.xterm-screen`.
 *      A text selection that extends to the rightmost column visually
 *      covers the scrollbar — it's still functional, just invisible.
 *   2. Tauri webview's overlay scrollbar is auto-hiding, which makes
 *      the affordance awkward (no constant visual cue of scroll
 *      position).
 *
 * This component renders a slim track + thumb absolutely positioned in
 * the pane's right gutter at z-30, reads
 * `term.buffer.active.viewportY` for thumb position, and routes
 * mouse-drag → `term.scrollLines()` for grabbing. xterm's own
 * scrollbar is hidden via CSS in `index.css` so the two don't fight
 * for the same gutter.
 */

import { useEffect, useRef, useState } from 'react'
import type { Terminal } from '@xterm/xterm'

interface Props {
  term: Terminal
}

export function TerminalScrollbar({ term }: Props) {
  const [, force] = useState(0)
  const trackRef = useRef<HTMLDivElement>(null)
  const draggingRef = useRef(false)

  useEffect(() => {
    const onChange = () => force((t) => t + 1)
    const sd = term.onScroll(onChange)
    const rd = term.onRender(onChange)
    return () => {
      sd.dispose()
      rd.dispose()
    }
  }, [term])

  const buffer = term.buffer.active
  const total = buffer.length
  const visible = term.rows
  const viewportY = buffer.viewportY

  // Hide entirely when there's nothing to scroll. Avoids a stray track
  // on freshly-attached panes whose scrollback hasn't filled yet.
  if (total <= visible) return null

  const thumbHeightFrac = Math.max(0.04, visible / total)
  const thumbTopFrac = total > visible ? viewportY / (total - visible) : 0
  // Translate "position fraction along the track" → CSS `top` for the
  // thumb. Using `calc(<frac>% * (1 - <thumbHeight>))` keeps the thumb
  // fully inside the track at both extremes.
  const thumbTopPct = thumbTopFrac * (1 - thumbHeightFrac) * 100

  const onThumbMouseDown = (e: React.MouseEvent) => {
    e.preventDefault()
    e.stopPropagation()
    draggingRef.current = true
    const trackEl = trackRef.current
    if (!trackEl) return
    const trackHeight = trackEl.getBoundingClientRect().height
    const startClientY = e.clientY
    const startViewportY = viewportY
    const range = total - visible // max possible viewportY
    if (range <= 0) return

    const onMove = (ev: MouseEvent) => {
      if (!draggingRef.current) return
      const dy = ev.clientY - startClientY
      // The thumb spans `thumbHeightFrac` of the track; its movable
      // travel is `(1 - thumbHeightFrac) * trackHeight`. Map dy in that
      // travel to viewportY in the [0, range] domain.
      const travelPx = (1 - thumbHeightFrac) * trackHeight
      if (travelPx <= 0) return
      const rowsPerPx = range / travelPx
      const targetViewportY = Math.max(
        0,
        Math.min(range, startViewportY + dy * rowsPerPx),
      )
      const delta = Math.round(
        targetViewportY - term.buffer.active.viewportY,
      )
      if (delta !== 0) term.scrollLines(delta)
    }
    const onUp = () => {
      draggingRef.current = false
      document.removeEventListener('mousemove', onMove)
      document.removeEventListener('mouseup', onUp)
    }
    document.addEventListener('mousemove', onMove)
    document.addEventListener('mouseup', onUp)
  }

  // Click on the track (not the thumb) → page-scroll toward the click.
  // Matches what users expect from native scrollbars.
  const onTrackMouseDown = (e: React.MouseEvent<HTMLDivElement>) => {
    if (e.target !== trackRef.current) return // thumb's own handler runs
    const trackEl = trackRef.current
    if (!trackEl) return
    const rect = trackEl.getBoundingClientRect()
    const clickFrac = (e.clientY - rect.top) / rect.height
    const range = total - visible
    if (range <= 0) return
    const targetViewportY = Math.round(clickFrac * range)
    const delta = targetViewportY - viewportY
    if (delta !== 0) term.scrollLines(delta)
  }

  return (
    <div
      ref={trackRef}
      onMouseDown={onTrackMouseDown}
      className="pointer-events-auto absolute right-0 top-2 bottom-2 z-30 w-2 rounded-full"
      style={{ background: 'rgba(255,255,255,0.02)' }}
    >
      <div
        className="absolute left-0 right-0 rounded-full bg-white/[0.18] transition-colors hover:bg-white/[0.28]"
        style={{
          top: `${thumbTopPct}%`,
          height: `${thumbHeightFrac * 100}%`,
          cursor: 'grab',
        }}
        onMouseDown={onThumbMouseDown}
      />
    </div>
  )
}

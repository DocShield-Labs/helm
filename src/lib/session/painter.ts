/**
 * Paint the daemon's grid into an xterm.js instance.
 *
 * xterm is a renderer and an input encoder here, nothing more: it has no
 * scrollback (history lives in the DOM above it) and never sees
 * application bytes. Each screen / diff becomes a short sequence of
 * cursor-positioned, SGR-styled row rewrites; the DEC private modes the
 * application turned on are mirrored so xterm encodes keys, mouse
 * reports, paste and focus events the way the application expects.
 *
 * The sequence builders are pure; `attachPainter` wires them to a session.
 */

import type { Terminal } from '@xterm/xterm'
import { ATTRS, MODES } from '@bindings'
import type { CursorInfo, HostId, RowInfo, SpanInfo } from '@bindings'
import { colorSgr } from './palette'
import { getSessionScreen, screenInfoOf, subscribePaint, type PaintEvent } from './screen'

const ESC = '\x1b'
const CSI = `${ESC}[`

/** DECSET numbers for each mode bit xterm must mirror. */
const DEC_MODES: Array<[number, number]> = [
  [MODES.APP_CURSOR, 1],
  [MODES.MOUSE_CLICK, 1000],
  [MODES.MOUSE_DRAG, 1002],
  [MODES.MOUSE_MOTION, 1003],
  [MODES.UTF8_MOUSE, 1005],
  [MODES.SGR_MOUSE, 1006],
  [MODES.ALTERNATE_SCROLL, 1007],
  [MODES.FOCUS_IN_OUT, 1004],
  [MODES.BRACKETED_PASTE, 2004],
]

export function spanSgr(span: SpanInfo): string {
  const p: string[] = ['0']
  const a = span.attrs
  if (a & ATTRS.BOLD) p.push('1')
  if (a & ATTRS.DIM) p.push('2')
  if (a & ATTRS.ITALIC) p.push('3')
  if (a & ATTRS.UNDERCURL) p.push('4:3')
  else if (a & ATTRS.DOUBLE_UNDERLINE) p.push('4:2')
  else if (a & ATTRS.UNDERLINE) p.push('4')
  if (a & ATTRS.INVERSE) p.push('7')
  if (a & ATTRS.HIDDEN) p.push('8')
  if (a & ATTRS.STRIKE) p.push('9')
  if (span.fg >= 0) p.push(colorSgr(span.fg, false))
  if (span.bg >= 0) p.push(colorSgr(span.bg, true))
  return `${CSI}${p.join(';')}m`
}

/** One row, written at `index` (0-based) and cleared to the right. */
export function rowSequence(index: number, row: RowInfo): string {
  let s = `${CSI}${index + 1};1H`
  for (const span of row.spans) {
    s += spanSgr(span)
    if (span.link) s += `${ESC}]8;;${span.link}${ESC}\\${span.text}${ESC}]8;;${ESC}\\`
    else s += span.text
  }
  s += `${CSI}0m${CSI}K`
  return s
}

/** Transition xterm's DEC modes from `prev` to `next`. */
export function modesSequence(prev: number, next: number): string {
  let s = ''
  const changed = prev ^ next
  if (!changed) return s
  // Alt screen first: the rows that follow belong to the new buffer.
  if (changed & MODES.ALT_SCREEN) s += `${CSI}?1049${next & MODES.ALT_SCREEN ? 'h' : 'l'}`
  for (const [bit, dec] of DEC_MODES) {
    if (changed & bit) s += `${CSI}?${dec}${next & bit ? 'h' : 'l'}`
  }
  if (changed & MODES.APP_KEYPAD) s += next & MODES.APP_KEYPAD ? `${ESC}=` : `${ESC}>`
  return s
}

export function cursorSequence(c: CursorInfo): string {
  const shape = c.shape === 'beam' ? 5 : c.shape === 'underline' ? 3 : 1
  const style = c.blink ? shape : shape + 1
  return `${CSI}${c.row + 1};${c.col + 1}H${CSI}${style} q${CSI}?25${c.visible ? 'h' : 'l'}`
}

/**
 * Modes, a scroll of `scroll` rows off the top, the given rows, then
 * the cursor. The cursor is hidden while rows land so a partial frame
 * never flashes it mid-row.
 */
export function diffSequence(
  rows: ReadonlyArray<{ index: number; row: RowInfo }>,
  cursor: CursorInfo,
  modes: number,
  prevModes: number,
  scroll = 0,
): string {
  let s = `${CSI}?25l${modesSequence(prevModes, modes)}`
  if (scroll > 0) s += `${CSI}${scroll}S`
  for (const { index, row } of rows) s += rowSequence(index, row)
  return s + cursorSequence(cursor)
}

/** Every row: a full repaint. */
export function screenSequence(
  lines: readonly RowInfo[],
  cursor: CursorInfo,
  modes: number,
  prevModes: number,
): string {
  return diffSequence(
    lines.map((row, index) => ({ index, row })),
    cursor,
    modes,
    prevModes,
  )
}

export interface Painter {
  /** Catch up after being hidden, if anything was missed. */
  repaintIfDirty(): void
  dispose(): void
}

/**
 * Keep `term` showing the session's grid. Paints the current screen on
 * attach and every screen / diff after, skipping while `visible()` is
 * false (a hidden session catches up with one repaint when shown). Rows
 * past xterm's own height — a resize round-trip in flight — are
 * dropped rather than piled onto its last row; a full frame follows
 * the resize.
 */
export function attachPainter(
  term: Terminal,
  hostId: HostId,
  sessionId: string,
  visible: () => boolean,
): Painter {
  // What xterm's modes are right now; each paint transitions them.
  let modes = 0
  let dirty = false
  const paint = (ev: PaintEvent) => {
    if (!visible()) {
      dirty = true
      return
    }
    if (ev.kind === 'screen') {
      const { lines, cursor, modes: next } = ev.screen
      term.write(screenSequence(lines.slice(0, term.rows), cursor, next, modes))
      modes = next
    } else {
      const rows = ev.rows.filter((r) => r.index < term.rows)
      term.write(diffSequence(rows, ev.cursor, ev.modes, modes, ev.scroll))
      modes = ev.modes
    }
  }
  const unsubscribe = subscribePaint(hostId, sessionId, paint)
  return {
    repaintIfDirty() {
      if (!dirty) return
      dirty = false
      const s = getSessionScreen(hostId, sessionId)
      if (s.loaded) paint({ kind: 'screen', screen: screenInfoOf(s) })
    },
    dispose: unsubscribe,
  }
}

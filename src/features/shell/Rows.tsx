/**
 * Terminal rows as DOM.
 *
 * Rows come from the daemon's model (`lib/session/screen.ts`): styled
 * spans per physical row plus a soft-wrap flag. Wrapped rows join into
 * one logical line so the browser reflows them at the session's current
 * width — history resizes the way a terminal reflows, without anyone
 * re-wrapping anything.
 *
 * `RowsView` renders a line range as fixed chunks keyed by absolute
 * line. Each chunk subscribes to its own rows, so a history append only
 * re-renders the chunk it lands in and prepending older history never
 * re-keys what's already on screen; `content-visibility: auto` keeps
 * off-screen chunks free.
 */

import { memo, useMemo, type CSSProperties } from 'react'
import type { DomCursor } from '@lib/session/screen'
import { ATTRS } from '@bindings'
import type { HostId, RowInfo, SpanInfo } from '@bindings'
import { commands } from '@lib/ipc'
import { linkifySpans } from '@lib/session/links'
import { colorCss } from '@lib/session/palette'
import { useRows } from '@lib/session/screen'

/** Rows per chunk; chunk `c` covers lines `[c * CHUNK, (c + 1) * CHUNK)`.
 * Exported: the render window quantizes its floor to this grid so the
 * boundary chunk's `from` is stable across frames. */
export const CHUNK = 256

export interface LogicalLine {
  /** Absolute line of the first physical row. */
  line: number
  spans: SpanInfo[]
  /** Text length of each physical row joined into this line — maps a
   * (physical line, column) cursor to a char offset in the joined text. */
  rowLens: number[]
}

export function joinWrapped(rows: ReadonlyArray<[number, RowInfo]>): LogicalLine[] {
  const out: LogicalLine[] = []
  let cur: LogicalLine | null = null
  let prevLine = -1
  for (const [line, row] of rows) {
    let len = 0
    for (const sp of row.spans) len += sp.text.length
    if (cur && line === prevLine + 1) {
      cur.spans.push(...row.spans)
      cur.rowLens.push(len)
    } else {
      if (cur) out.push(cur)
      cur = { line, spans: [...row.spans], rowLens: [len] }
    }
    prevLine = line
    if (!row.wrapped) {
      out.push(cur)
      cur = null
    }
  }
  if (cur) out.push(cur)
  return out
}

export function rowsToText(rows: ReadonlyArray<[number, RowInfo]>): string {
  return joinWrapped(rows)
    .map((l) => l.spans.map((s) => s.text).join(''))
    .join('\n')
}

export interface RowsViewProps {
  hostId: HostId
  sessionId: string
  /** Absolute line range `[from, to)`. */
  from: number
  to: number
  /** Terminal cursor to draw inline, when its line falls in range. */
  cursor?: DomCursor
}

export const RowsView = memo(function RowsView({ hostId, sessionId, from, to, cursor }: RowsViewProps) {
  // A range must be finite: an open-ended `to` (the grid's first line
  // while the grid is hidden is Infinity) would loop forever here.
  if (!Number.isFinite(from) || !Number.isFinite(to) || to <= from) return null
  const first = Math.floor(from / CHUNK)
  const last = Math.floor((to - 1) / CHUNK)
  const chunks = []
  for (let c = first; c <= last; c++) {
    const lo = Math.max(from, c * CHUNK)
    const hi = Math.min(to, (c + 1) * CHUNK)
    chunks.push(
      <RowsChunk
        key={c}
        hostId={hostId}
        sessionId={sessionId}
        from={lo}
        to={hi}
        cursor={cursor && cursor.visible && cursor.line >= lo && cursor.line < hi ? cursor : undefined}
      />,
    )
  }
  return <>{chunks}</>
})

const RowsChunk = memo(function RowsChunk({ hostId, sessionId, from, to, cursor }: RowsViewProps) {
  const rows = useRows(hostId, sessionId, from, to)
  const lines = useMemo(() => joinWrapped(rows), [rows])
  if (lines.length === 0) return null
  return (
    <div
      className="helm-rows-chunk"
      style={{ containIntrinsicSize: `auto calc(var(--helm-line-px) * ${lines.length})` }}
    >
      {lines.map((l) => (
        <Line key={l.line} line={l} caret={caretFor(l, cursor)} />
      ))}
    </div>
  )
})

/** The caret's character offset within a joined logical line, or null
 * when the cursor is elsewhere. A wrapped row is full-width by
 * definition, so the offset is the sum of the preceding physical rows'
 * lengths plus the column. Computed in the parent so `Line` stays
 * memoized: a cursor move re-renders only the lines gaining or losing
 * the caret, not the whole chunk. */
function caretFor(l: LogicalLine, cursor: DomCursor | undefined): CaretProps | null {
  if (!cursor || cursor.line < l.line || cursor.line >= l.line + l.rowLens.length) return null
  let offset = cursor.col
  for (let r = 0; r < cursor.line - l.line; r++) offset += l.rowLens[r]
  return { offset, shape: cursor.shape, blink: cursor.blink }
}

interface CaretProps {
  offset: number
  shape: DomCursor['shape']
  blink: boolean
}

const Line = memo(function Line({ line: l, caret }: { line: LogicalLine; caret: CaretProps | null }) {
  // Plain-text URL detection runs on the joined logical line, so a
  // link that soft-wraps across physical rows is still one target.
  const spans = useMemo(() => linkifySpans(l.spans), [l.spans])
  return (
    <div className="helm-line" data-line={l.line}>
      {lineContent(spans, caret)}
    </div>
  )
})

function lineContent(spans: SpanInfo[], caret: CaretProps | null) {
  if (!caret) {
    return spans.length === 0 ? ' ' : spans.map((s, j) => <SpanView key={j} span={s} />)
  }
  const { offset } = caret
  const out: React.ReactNode[] = []
  let seen = 0
  let placed = false
  for (let j = 0; j < spans.length; j++) {
    const s = spans[j]
    const end = seen + s.text.length
    if (!placed && offset >= seen && offset < end) {
      const cut = offset - seen
      if (cut > 0) out.push(<SpanView key={`${j}a`} span={{ ...s, text: s.text.slice(0, cut) }} />)
      out.push(<Caret key="caret" {...caret} />)
      out.push(<SpanView key={`${j}b`} span={{ ...s, text: s.text.slice(cut) }} />)
      placed = true
    } else {
      out.push(<SpanView key={j} span={s} />)
    }
    seen = end
  }
  if (!placed) {
    // Cursor past the text: pad with spaces so the caret sits at its column.
    if (offset > seen) out.push(<span key="pad">{' '.repeat(offset - seen)}</span>)
    out.push(<Caret key="caret" {...caret} />)
  }
  return out
}

/** The terminal cursor, honouring the shape/blink the application set
 * via DECSCUSR — same contract the alt-screen xterm follows. */
function Caret({ shape, blink }: CaretProps) {
  return <span className="helm-caret" data-shape={shape} data-blink={blink || undefined} aria-hidden />
}

function SpanView({ span }: { span: SpanInfo }) {
  const css = spanStyle(span)
  if (span.link) {
    const href = span.link
    return (
      <a
        href={href}
        className="helm-link"
        // OSC 8 links can label the URL with arbitrary text — surface
        // the real target on hover. A plain detected URL is its own label.
        title={span.text === href ? undefined : href}
        style={css}
        onClick={(e) => {
          e.preventDefault()
          // A drag-selection that ends on the link is a selection, not
          // a click.
          if (window.getSelection()?.isCollapsed === false) return
          void commands.openUrl(href)
        }}
      >
        {span.text}
      </a>
    )
  }
  return <span style={css}>{span.text}</span>
}

function spanStyle(s: SpanInfo): CSSProperties | undefined {
  let fg = colorCss(s.fg)
  let bg = colorCss(s.bg)
  const a = s.attrs
  if (a & ATTRS.INVERSE) {
    const t = fg
    fg = bg ?? 'var(--terminal-bg)'
    bg = t ?? 'var(--terminal-fg)'
  }
  const css: CSSProperties = {}
  if (fg) css.color = fg
  if (bg) css.backgroundColor = bg
  if (a & ATTRS.BOLD) css.fontWeight = 600
  if (a & ATTRS.DIM) css.opacity = 0.6
  if (a & ATTRS.ITALIC) css.fontStyle = 'italic'
  if (a & (ATTRS.UNDERLINE | ATTRS.STRIKE)) {
    css.textDecoration = [a & ATTRS.UNDERLINE ? 'underline' : '', a & ATTRS.STRIKE ? 'line-through' : '']
      .filter(Boolean)
      .join(' ')
    if (a & ATTRS.UNDERCURL) css.textDecorationStyle = 'wavy'
    else if (a & ATTRS.DOUBLE_UNDERLINE) css.textDecorationStyle = 'double'
  }
  if (a & ATTRS.HIDDEN) css.visibility = 'hidden'
  return Object.keys(css).length ? css : undefined
}

/**
 * Terminal rows as DOM.
 *
 * Rows come from the daemon's model (`lib/session/screen.ts`): styled
 * spans per physical row plus a soft-wrap flag. Wrapped rows join into
 * one logical line so the browser reflows them at the pane's current
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
import { ATTRS } from '@bindings'
import type { HostId, RowInfo, SpanInfo } from '@bindings'
import { commands } from '@lib/ipc'
import { colorCss } from '@lib/session/palette'
import { useRows } from '@lib/session/screen'

/** Rows per chunk; chunk `c` covers lines `[c * CHUNK, (c + 1) * CHUNK)`. */
const CHUNK = 256

export interface LogicalLine {
  /** Absolute line of the first physical row. */
  line: number
  spans: SpanInfo[]
}

export function joinWrapped(rows: ReadonlyArray<[number, RowInfo]>): LogicalLine[] {
  const out: LogicalLine[] = []
  let cur: LogicalLine | null = null
  let prevLine = -1
  for (const [line, row] of rows) {
    if (cur && line === prevLine + 1) {
      cur.spans.push(...row.spans)
    } else {
      if (cur) out.push(cur)
      cur = { line, spans: [...row.spans] }
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
  paneId: string
  /** Absolute line range `[from, to)`. */
  from: number
  to: number
}

export const RowsView = memo(function RowsView({ hostId, paneId, from, to }: RowsViewProps) {
  // A range must be finite: an open-ended `to` (the grid's first line
  // while the grid is hidden is Infinity) would loop forever here.
  if (!Number.isFinite(from) || !Number.isFinite(to) || to <= from) return null
  const first = Math.floor(from / CHUNK)
  const last = Math.floor((to - 1) / CHUNK)
  const chunks = []
  for (let c = first; c <= last; c++) {
    chunks.push(
      <RowsChunk
        key={c}
        hostId={hostId}
        paneId={paneId}
        from={Math.max(from, c * CHUNK)}
        to={Math.min(to, (c + 1) * CHUNK)}
      />,
    )
  }
  return <>{chunks}</>
})

const RowsChunk = memo(function RowsChunk({ hostId, paneId, from, to }: RowsViewProps) {
  const rows = useRows(hostId, paneId, from, to)
  const lines = useMemo(() => joinWrapped(rows), [rows])
  if (lines.length === 0) return null
  return (
    <div
      className="helm-rows-chunk"
      style={{ containIntrinsicSize: `auto calc(var(--helm-line-px) * ${lines.length})` }}
    >
      {lines.map((l) => (
        <div key={l.line} className="helm-line" data-line={l.line}>
          {l.spans.length === 0 ? ' ' : l.spans.map((s, j) => <SpanView key={j} span={s} />)}
        </div>
      ))}
    </div>
  )
})

function SpanView({ span }: { span: SpanInfo }) {
  const css = spanStyle(span)
  if (span.link) {
    const href = span.link
    return (
      <a
        href={href}
        style={{ ...css, textDecoration: 'underline' }}
        onClick={(e) => {
          e.preventDefault()
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

/**
 * One finished command block, rendered as plain DOM from the pane's
 * byte stream. Warp's block chrome: a header row (prompt glyph +
 * command line + right-aligned duration / exit), the output below,
 * failed commands washed red with a flag-pole stripe flush left, and a
 * hover toolbar (copy command / copy output). No edge borders on
 * ordinary blocks — separators live in the list.
 */

import { memo, useMemo, type CSSProperties } from 'react'
import type { BlockInfo, HostId } from '@bindings'
import { commands } from '@lib/ipc'
import * as stream from '@lib/session/stream'
import { bodyStartSeq } from '@lib/session/blocks'
import { linesToText, renderAnsi, type Line, type Style } from '@lib/session/ansi'

const decoder = new TextDecoder()

interface BlockProps {
  hostId: HostId
  paneId: string
  block: BlockInfo
}

export const Block = memo(function Block({ hostId, paneId, block }: BlockProps) {
  const bodyStart = bodyStartSeq(block)
  const end = block.end_seq ?? bodyStart
  const lines = useMemo<Line[] | null>(() => {
    const bytes = stream.slice(hostId, paneId, bodyStart, end)
    if (!bytes) return null
    if (bytes.length === 0) return []
    return renderAnsi(decoder.decode(bytes))
    // Finished blocks are immutable: key on identity + span.
  }, [hostId, paneId, block.id, bodyStart, end])

  const failed = block.exit_code !== null && block.exit_code !== 0
  const meta = formatMeta(block)

  return (
    <div
      data-block-id={block.id}
      className={`helm-block group relative ${failed ? 'helm-block-failed' : ''}`}
    >
      {failed && (
        <div className="absolute bottom-0 left-0 top-0 w-[4px] bg-[var(--terminal-failed)]" />
      )}
      <div className="helm-block-header sticky top-0 z-10 flex items-start gap-3 px-5 pt-2.5 pb-1">
        <span className="select-none font-mono text-[13px] font-semibold leading-5 text-accent">❯</span>
        <span className="min-w-0 flex-1 whitespace-pre-wrap break-words font-mono text-[13px] leading-5 text-text-primary">
          {block.cmdline ?? ''}
        </span>
        <span
          className={`shrink-0 select-none font-mono text-[11px] leading-5 ${
            failed ? 'text-[var(--terminal-failed)]' : 'text-text-tertiary'
          }`}
        >
          {meta}
        </span>
        <div className="absolute right-3 top-1.5 flex gap-0.5 rounded-md border border-white/[0.08] bg-elevated p-0.5 opacity-0 transition-opacity group-hover:opacity-100">
          <ToolbarButton
            title="Copy command"
            onClick={() => void navigator.clipboard.writeText(block.cmdline ?? '')}
          >
            ⌘
          </ToolbarButton>
          <ToolbarButton
            title="Copy output"
            onClick={() => void navigator.clipboard.writeText(lines ? linesToText(lines) : '')}
          >
            ⧉
          </ToolbarButton>
        </div>
      </div>
      {lines === null ? (
        <div className="px-5 pb-3 font-mono text-[11px] text-text-disabled">
          output no longer in buffer
        </div>
      ) : lines.length > 0 ? (
        <pre className="helm-block-output">
          {lines.map((line, i) => (
            <div key={i} className="helm-line">
              {line.length === 0 ? ' ' : line.map((span, j) => <SpanView key={j} text={span.text} style={span.style} />)}
            </div>
          ))}
        </pre>
      ) : null}
    </div>
  )
})

function SpanView({ text, style }: { text: string; style: Style }) {
  const css = spanStyle(style)
  if (style.href) {
    const href = style.href
    return (
      <a
        href={href}
        style={{ ...css, textDecoration: 'underline' }}
        onClick={(e) => {
          e.preventDefault()
          void commands.openUrl(href)
        }}
      >
        {text}
      </a>
    )
  }
  return <span style={css}>{text}</span>
}

function spanStyle(s: Style): CSSProperties | undefined {
  let fg = s.fg
  let bg = s.bg
  if (s.inverse) {
    const t = fg
    fg = bg ?? 'var(--terminal-bg)'
    bg = t ?? 'var(--terminal-fg)'
  }
  const css: CSSProperties = {}
  if (fg) css.color = fg
  if (bg) css.backgroundColor = bg
  if (s.bold) css.fontWeight = 600
  if (s.dim) css.opacity = 0.6
  if (s.italic) css.fontStyle = 'italic'
  if (s.underline || s.strike) {
    css.textDecoration = [s.underline ? 'underline' : '', s.strike ? 'line-through' : '']
      .filter(Boolean)
      .join(' ')
  }
  return Object.keys(css).length ? css : undefined
}

function ToolbarButton({
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
      className="flex h-6 w-6 items-center justify-center rounded text-[12px] text-text-tertiary hover:bg-white/[0.06] hover:text-text-primary"
    >
      {children}
    </button>
  )
}

function formatMeta(b: BlockInfo): string {
  const parts: string[] = []
  if (b.exit_code !== null && b.exit_code !== 0) parts.push(`exit ${b.exit_code}`)
  else if (b.exit_code === 0) parts.push('✓')
  const dur = formatDuration(b)
  if (dur) parts.push(dur)
  return parts.join(' · ')
}

function formatDuration(b: BlockInfo): string | null {
  if (b.started_at_ms === null || b.finished_at_ms === null) return null
  const ms = Math.max(0, b.finished_at_ms - b.started_at_ms)
  if (ms < 1000) return `${ms}ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`
  const m = Math.floor(ms / 60_000)
  const s = Math.round((ms % 60_000) / 1000)
  return `${m}m ${s.toString().padStart(2, '0')}s`
}

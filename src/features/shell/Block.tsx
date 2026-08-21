/**
 * One finished command block, rendered as plain DOM from the pane's
 * byte stream. Warp's block: a quiet prompt row (`cwd on branch`, the
 * duration right-aligned), the command line, the output; failed
 * commands washed red; a hover belt (copy command / copy output) at
 * the top-right. No edge borders — hairlines live in the list.
 */

import { memo, useMemo, type CSSProperties } from 'react'
import type { BlockInfo, HostId } from '@bindings'
import { commands } from '@lib/ipc'
import * as stream from '@lib/session/stream'
import { bodyStartSeq } from '@lib/session/blocks'
import { linesToText, renderAnsi, type Line, type Style } from '@lib/session/ansi'
import { homeRelative } from '@lib/path'
import { CopyIcon, TerminalIcon } from '@features/sessions/icons'

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
  const duration = formatDuration(block)
  const cwd = homeRelative(block.cwd)

  return (
    <div
      data-block-id={block.id}
      className={`helm-block group relative ${failed ? 'helm-block-failed' : ''}`}
    >
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
          <BeltButton
            title="Copy output"
            onClick={() => void navigator.clipboard.writeText(lines ? linesToText(lines) : '')}
          >
            <CopyIcon size={14} />
          </BeltButton>
        </div>
      </div>
      {lines === null ? (
        <div className="px-4 pb-4 font-mono text-[11px] text-text-disabled">
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
      ) : (
        <div className="h-4" />
      )}
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
      className="flex h-6 w-6 items-center justify-center rounded text-text-tertiary hover:bg-[var(--stroke-default)] hover:text-text-primary"
    >
      {children}
    </button>
  )
}

export function formatDuration(b: BlockInfo): string {
  if (b.started_at_ms === null || b.finished_at_ms === null) return ''
  const ms = Math.max(0, b.finished_at_ms - b.started_at_ms)
  if (ms < 1000) return `${ms}ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`
  const m = Math.floor(ms / 60_000)
  const s = Math.round((ms % 60_000) / 1000)
  return `${m}m ${s.toString().padStart(2, '0')}s`
}

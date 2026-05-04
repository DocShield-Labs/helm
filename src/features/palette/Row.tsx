/**
 * Single result row in the palette list.
 *
 * Visual model from Figma frame 27:6 (Empty + Recents):
 *   - 40px tall, rounded-md background
 *   - 12px gap, leading 20px icon slot
 *   - label (14px, primary), then a monospace muted sublabel
 *   - trailing kbd hint (↵, ⌘K, etc.)
 *   - selected row: `--accent-muted` background plus a 2×24 accent left
 *     bar rendered as an absolute child so row geometry stays stable
 */

import type { ReactNode } from 'react'

export interface RowProps {
  label: string
  sublabel?: string
  icon?: string
  kbd?: ReactNode
  selected?: boolean
  onMouseEnter?: () => void
  onClick?: () => void
}

export function Row({
  label,
  sublabel,
  icon,
  kbd,
  selected,
  onMouseEnter,
  onClick,
}: RowProps) {
  return (
    <button
      type="button"
      onMouseEnter={onMouseEnter}
      onClick={onClick}
      className="relative flex h-10 w-full items-center gap-3 overflow-hidden rounded-md px-3 text-left"
      style={{
        background: selected ? 'var(--accent-muted)' : 'transparent',
      }}
    >
      {selected && (
        <span
          className="pointer-events-none absolute -left-2 top-1/2 h-6 w-[2px] -translate-y-1/2 rounded-[1px]"
          style={{ background: 'var(--accent-default)' }}
        />
      )}
      <span
        className="flex size-5 shrink-0 items-center justify-center font-mono text-[14px]"
        style={{ color: selected ? 'var(--accent-text)' : 'var(--text-secondary)' }}
      >
        {icon ?? '·'}
      </span>
      <span className="flex flex-1 items-center gap-2 overflow-hidden whitespace-nowrap">
        <span
          className={`text-[14px] ${selected ? 'font-medium' : 'font-normal'}`}
          style={{ color: 'var(--text-primary)' }}
        >
          {label}
        </span>
        {sublabel && (
          <span className="font-mono text-[12px]" style={{ color: 'var(--text-tertiary)' }}>
            {sublabel}
          </span>
        )}
      </span>
      {kbd && <span className="flex shrink-0 items-center gap-1">{kbd}</span>}
    </button>
  )
}

/** Tiny pill matching the Figma kbd chip style. Use 1+ inside a Row's
 * `kbd` prop, e.g. `<><Kbd>⌘</Kbd><Kbd>k</Kbd></>`. */
export function Kbd({ children }: { children: ReactNode }) {
  return (
    <span
      className="inline-flex h-5 min-w-[20px] items-center justify-center rounded px-[5px] font-mono text-[10px]"
      style={{
        background: 'rgba(255,255,255,0.06)',
        color: 'var(--text-secondary)',
      }}
    >
      {children}
    </span>
  )
}

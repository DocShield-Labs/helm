/**
 * Palette — Cmd+K overlay shell.
 *
 * Pure presentational: owns the floating sheet, the leading ⌘ glyph,
 * the optional sub-mode chip, the input, the trailing "esc" affordance,
 * and the body + footer slots. State (open, query, results, selection,
 * keyboard nav) lives in `features/palette/PaletteHost.tsx`.
 */

import type { ReactNode } from 'react'

export interface PaletteProps {
  open: boolean
  query: string
  onQueryChange?: (q: string) => void
  /** Filter chips rendered between the ⌘ glyph and the input. Each entry
   * is a sub-mode label (`@workspaces`, `#windows`, `$hosts`) or, when
   * the user has drilled into a row, a single `↳ <parent>` breadcrumb. */
  chips?: readonly string[]
  /** Body — rendered between the header and footer. Scrolls when long. */
  children?: ReactNode
  /** Optional footer content (the `↑↓ navigate · ↵ run · esc close` row). */
  footer?: ReactNode
  onClose?: () => void
  /** Forwarded to the input so the host can intercept ↑↓/↵/Esc/Tab/etc.
   * at the point where focus lives. */
  onInputKeyDown?: (e: React.KeyboardEvent<HTMLInputElement>) => void
}

export function Palette({
  open,
  query,
  onQueryChange,
  chips,
  children,
  footer,
  onClose,
  onInputKeyDown,
}: PaletteProps) {
  if (!open) return null
  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/45 pt-[18vh]"
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal
        style={{ width: 560, boxShadow: 'var(--elevation-2)' }}
        className="overflow-hidden rounded-xl border border-white/10 bg-elevated"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex h-14 items-center gap-3 px-[18px]">
          <span className="font-mono text-[18px] text-text-tertiary">⌘</span>
          {chips && chips.length > 0 && (
            <div className="flex items-center gap-1">
              {chips.map((c, i) => (
                <span
                  key={`${i}.${c}`}
                  className="inline-flex items-center rounded-md border border-accent-border bg-accent-muted px-2 py-1 font-mono text-[12px] font-medium"
                  style={{ color: 'var(--accent-text)' }}
                >
                  {c}
                </span>
              ))}
            </div>
          )}
          <input
            autoFocus
            value={query}
            onChange={(e) => onQueryChange?.(e.target.value)}
            onKeyDown={onInputKeyDown}
            placeholder="Type a command…"
            className="flex-1 bg-transparent text-[16px] text-text-primary placeholder:text-text-disabled focus:outline-none"
          />
          <button
            type="button"
            onClick={onClose}
            className="font-mono text-[10px] text-text-tertiary hover:text-text-secondary"
          >
            esc
          </button>
        </div>
        <div className="border-t border-white/[0.06]" />
        <div className="max-h-[60vh] overflow-y-auto p-2">{children}</div>
        {footer}
      </div>
    </div>
  )
}

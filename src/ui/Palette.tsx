/**
 * Palette — Cmd+K overlay shell.
 * Real fuzzy filtering, sub-modes (@workspaces / #windows / $hosts), and
 * keyboard navigation land in feature/palette.
 */

import type { ReactNode } from 'react'

export interface PaletteProps {
  open: boolean
  query: string
  onQueryChange?: (q: string) => void
  chip?: string
  children?: ReactNode
  onClose?: () => void
}

export function Palette({ open, query, onQueryChange, chip, children, onClose }: PaletteProps) {
  if (!open) return null
  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center bg-black/45 pt-[18vh]">
      <div
        role="dialog"
        aria-modal
        style={{ width: 560, boxShadow: 'var(--elevation-2)' }}
        className="overflow-hidden rounded-xl border border-white/10 bg-elevated"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex h-14 items-center gap-3 px-[18px]">
          <span className="font-mono text-[18px] text-text-tertiary">⌘</span>
          {chip && (
            <span
              className="inline-flex items-center rounded-md border border-accent-border bg-accent-muted px-2 py-1 font-mono text-[12px] font-medium"
              style={{ color: 'var(--accent-text)' }}
            >
              {chip}
            </span>
          )}
          <input
            autoFocus
            value={query}
            onChange={(e) => onQueryChange?.(e.target.value)}
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
      </div>
    </div>
  )
}

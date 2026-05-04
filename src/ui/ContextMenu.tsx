/**
 * ContextMenu — right-click popover used across the sidebar.
 *
 * Fixed-positioned so it escapes parent overflow (sidebar's
 * overflow-y-auto would clip otherwise). Closes on:
 *   - clicking any item
 *   - clicking outside
 *   - Escape
 *   - the window scrolling (matches macOS native menus)
 */

import { useEffect, useRef } from 'react'

export interface ContextMenuItem {
  /** Unique id within the menu — used as React key. */
  id: string
  label: string
  /** Optional leading glyph rendered in mono. */
  icon?: string
  /** Optional trailing keyboard hint, e.g. "⌘W". */
  shortcut?: string
  onClick: () => void
  /** Renders the label in red (used for kill-style actions). */
  destructive?: boolean
  /** Disabled items are not clickable and rendered dimmed. */
  disabled?: boolean
}

export interface ContextMenuProps {
  open: boolean
  /** Viewport coordinates from the right-click event. */
  x: number
  y: number
  items: Array<ContextMenuItem | 'separator'>
  onClose: () => void
}

export function ContextMenu({ open, x, y, items, onClose }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    const onDocMouseDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    const onScroll = () => onClose()
    document.addEventListener('mousedown', onDocMouseDown, true)
    document.addEventListener('keydown', onKey, true)
    window.addEventListener('scroll', onScroll, true)
    return () => {
      document.removeEventListener('mousedown', onDocMouseDown, true)
      document.removeEventListener('keydown', onKey, true)
      window.removeEventListener('scroll', onScroll, true)
    }
  }, [open, onClose])

  if (!open) return null

  // Clamp into viewport so the menu never opens off-screen.
  const W = 220
  const ESTIMATED_H = items.length * 28 + 8
  const left = Math.min(x, window.innerWidth - W - 8)
  const top = Math.min(y, window.innerHeight - ESTIMATED_H - 8)

  return (
    <div
      ref={ref}
      role="menu"
      onContextMenu={(e) => e.preventDefault()}
      className="fixed z-[60] min-w-[200px] overflow-hidden rounded-lg border border-white/[0.06] bg-elevated py-1"
      style={{ left, top, boxShadow: 'var(--elevation-2)' }}
    >
      {items.map((item, idx) => {
        if (item === 'separator') {
          return <div key={`sep-${idx}`} className="my-1 h-px bg-white/[0.06]" />
        }
        return (
          <button
            key={item.id}
            type="button"
            role="menuitem"
            disabled={item.disabled}
            onClick={(e) => {
              e.stopPropagation()
              if (item.disabled) return
              item.onClick()
              onClose()
            }}
            className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-[12px] ${
              item.disabled
                ? 'cursor-default text-text-disabled'
                : item.destructive
                  ? 'text-status-error hover:bg-white/[0.04]'
                  : 'text-text-secondary hover:bg-white/[0.04] hover:text-text-primary'
            }`}
          >
            {item.icon && (
              <span className="font-mono text-[12px] text-text-tertiary">{item.icon}</span>
            )}
            <span className="flex-1">{item.label}</span>
            {item.shortcut && (
              <span className="font-mono text-[10px] text-text-tertiary">{item.shortcut}</span>
            )}
          </button>
        )
      })}
    </div>
  )
}

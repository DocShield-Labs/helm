/**
 * SidebarWindowRow — third-level row, the deepest in the tree.
 * Focused state gets the only 2px coral accent line in the system.
 * Double-click on the name enters inline rename mode.
 */

import { useEffect, useRef, useState } from 'react'
import { ActivityDot, type ActivityDotState } from './ActivityDot'
import { ContextMenu, type ContextMenuItem } from './ContextMenu'

export type SidebarWindowRowState = 'rest' | 'hover' | 'focused'

export interface SidebarWindowRowProps {
  name: string
  /** Secondary label rendered under the name in the tertiary slot.
   * Currently used to surface the active pane's working directory; when
   * empty the slot collapses. The full string is exposed via the
   * `<span title>` so long paths stay readable on hover. */
  label: string
  activity?: ActivityDotState
  state?: SidebarWindowRowState
  onClick?: () => void
  /** Called when user finishes renaming (Enter). No-op on Esc / blur. */
  onRename?: (next: string) => void
  /** Hover-revealed × button + Kill menu item — kill this window. */
  onKill?: () => void
  /** Whether this window is currently in the user's pinned list. Drives
   * whether the context menu shows "Pin" or "Unpin". */
  isPinned?: boolean
  /** Pin this window. Omit (alongside onUnpin) to hide the menu entry. */
  onPin?: () => void
  /** Remove this window from pins. */
  onUnpin?: () => void
}

export function SidebarWindowRow({
  name,
  label,
  activity = 'none',
  state = 'rest',
  onClick,
  onRename,
  onKill,
  isPinned = false,
  onPin,
  onUnpin,
}: SidebarWindowRowProps) {
  const focused = state === 'focused'
  const bg = focused ? 'bg-accent-muted' : state === 'hover' ? 'bg-white/[0.025]' : ''

  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(name)
  const inputRef = useRef<HTMLInputElement>(null)
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)

  // Build the context menu lazily so we don't recompute on every render.
  // Items list adapts to which callbacks the parent provided — pin
  // entries vanish when no onPin/onUnpin is wired.
  const menuItems: Array<ContextMenuItem | 'separator'> = []
  if (isPinned && onUnpin) {
    menuItems.push({ id: 'unpin', label: 'Unpin from sidebar', icon: '☆', onClick: onUnpin })
  } else if (!isPinned && onPin) {
    menuItems.push({ id: 'pin', label: 'Pin to sidebar', icon: '★', onClick: onPin })
  }
  if (onRename) {
    menuItems.push({
      id: 'rename', label: 'Rename window', icon: 'A', shortcut: '⏎⏎',
      onClick: () => setEditing(true),
    })
  }
  if (onKill) {
    if (menuItems.length > 0) menuItems.push('separator')
    menuItems.push({
      id: 'kill', label: 'Kill window', icon: '×', shortcut: '⌘W',
      destructive: true, onClick: onKill,
    })
  }

  // Reset draft when entering edit mode or when the underlying name changes.
  useEffect(() => {
    if (editing) {
      setDraft(name)
      // defer focus so the input is mounted
      requestAnimationFrame(() => {
        inputRef.current?.focus()
        inputRef.current?.select()
      })
    }
  }, [editing, name])

  const commit = () => {
    setEditing(false)
    const trimmed = draft.trim()
    if (trimmed && trimmed !== name) onRename?.(trimmed)
  }
  const cancel = () => setEditing(false)

  return (
    <>
    <button
      type="button"
      onClick={() => {
        if (!editing) onClick?.()
      }}
      onDoubleClick={(e) => {
        if (onRename) {
          e.stopPropagation()
          setEditing(true)
        }
      }}
      onContextMenu={(e) => {
        if (menuItems.length === 0) return
        e.preventDefault()
        e.stopPropagation()
        setMenu({ x: e.clientX, y: e.clientY })
      }}
      className={`group relative flex h-[26px] w-full items-center gap-2 rounded-md pl-[54px] pr-2 ${bg} ${focused ? '' : 'hover:bg-white/[0.025]'}`}
    >
      <span
        className="font-mono text-[11px] leading-none"
        style={{ color: focused ? 'var(--accent-text)' : 'var(--text-tertiary)' }}
      >
        ▢
      </span>
      {editing ? (
        <input
          ref={inputRef}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          // Blur (click-out, Tab) commits — Enter also commits, Esc cancels.
          // This matches the convention used by Linear, Things, etc. where
          // text inputs treat focus loss as "I'm done."
          onBlur={commit}
          onKeyDown={(e) => {
            e.stopPropagation()
            if (e.key === 'Enter') {
              e.preventDefault()
              commit()
            } else if (e.key === 'Escape') {
              e.preventDefault()
              cancel()
            }
          }}
          onClick={(e) => e.stopPropagation()}
          className="flex-1 rounded-sm bg-canvas px-1 text-[12px] text-text-primary outline-none ring-1 ring-accent focus:outline-none"
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
        />
      ) : (
        <>
          <span
            className={`text-[12px] ${focused ? 'font-medium text-text-primary' : 'text-text-secondary'}`}
          >
            {name}
          </span>
          <span
            className="flex-1 truncate text-left font-mono text-[10px] text-text-tertiary"
            title={label || undefined}
          >
            {label}
          </span>
          {onKill && (
            <span
              role="button"
              aria-label="Kill window"
              title="Kill window (⌘W)"
              onClick={(e) => {
                e.stopPropagation()
                onKill()
              }}
              className="cursor-pointer rounded-sm px-1 font-mono text-[12px] leading-none text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100 hover:bg-white/[0.06] hover:text-text-secondary"
            >
              ×
            </span>
          )}
          <ActivityDot state={activity} />
        </>
      )}
    </button>
    {menu && (
      <ContextMenu
        open
        x={menu.x}
        y={menu.y}
        items={menuItems}
        onClose={() => setMenu(null)}
      />
    )}
    </>
  )
}

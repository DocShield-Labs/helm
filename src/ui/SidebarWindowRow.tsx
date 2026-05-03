/**
 * SidebarWindowRow — third-level row, the deepest in the tree.
 * Focused state gets the only 2px coral accent line in the system.
 * Double-click on the name enters inline rename mode.
 */

import { useEffect, useRef, useState } from 'react'
import { ActivityDot, type ActivityDotState } from './ActivityDot'

export type SidebarWindowRowState = 'rest' | 'hover' | 'focused'

export interface SidebarWindowRowProps {
  name: string
  command: string
  activity?: ActivityDotState
  state?: SidebarWindowRowState
  onClick?: () => void
  /** Called when user finishes renaming (Enter). No-op on Esc / blur. */
  onRename?: (next: string) => void
  /** Hover-revealed × button — kill this window. */
  onKill?: () => void
}

export function SidebarWindowRow({
  name,
  command,
  activity = 'none',
  state = 'rest',
  onClick,
  onRename,
  onKill,
}: SidebarWindowRowProps) {
  const focused = state === 'focused'
  const bg = focused ? 'bg-accent-muted' : state === 'hover' ? 'bg-white/[0.025]' : ''

  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(name)
  const inputRef = useRef<HTMLInputElement>(null)

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
      className={`group relative flex h-[26px] w-full items-center gap-2 rounded-md pl-[54px] pr-2 ${bg} ${focused ? '' : 'hover:bg-white/[0.025]'}`}
    >
      {focused && (
        <span
          className="absolute left-0 top-[6px] block h-[14px] w-[2px] rounded-[1px]"
          style={{ background: 'var(--accent-default)' }}
        />
      )}
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
          <span className="flex-1 truncate text-left font-mono text-[10px] text-text-tertiary">
            {command}
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
  )
}

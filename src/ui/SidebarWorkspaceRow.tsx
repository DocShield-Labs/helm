/**
 * SidebarWorkspaceRow — second-level row.
 * Indented one notch from host rows. Double-click renames the workspace
 * (= rename-session); right-click triggers the parent's kill flow.
 */

import { useEffect, useRef, useState, type ReactNode } from 'react'
import { ActivityDot, type ActivityDotState } from './ActivityDot'

export type SidebarWorkspaceRowState = 'rest' | 'hover' | 'active'

export interface SidebarWorkspaceRowProps {
  name: string
  activity?: ActivityDotState
  state?: SidebarWorkspaceRowState
  expanded?: boolean
  onClick?: () => void
  onToggleExpand?: () => void
  /** Inline rename handler — fired on Enter / blur with trimmed value. */
  onRename?: (next: string) => void
  /** Right-click handler — parent owns the confirm + kill dispatch. */
  onKill?: () => void
  /** Hover-revealed + button — spawn a new window in this workspace. */
  onAddWindow?: () => void
  /** Glyph or icon node rendered before the name. Defaults to the
   * workspace glyph (◫); the folder-view variant passes a folder
   * SVG instead. Accepts any ReactNode so callers aren't constrained
   * to a single Unicode character. */
  icon?: ReactNode
  /** Hover title for the icon/label — used by the folder variant to
   * surface the full cwd path on hover. */
  title?: string
}

export function SidebarWorkspaceRow({
  name,
  activity = 'none',
  state = 'rest',
  expanded = false,
  onClick,
  onToggleExpand,
  onRename,
  onKill,
  onAddWindow,
  icon = '◫',
  title,
}: SidebarWorkspaceRowProps) {
  const isActive = state === 'active'
  // Workspaces get the lightest tier (5%) so a selected workspace
  // doesn't visually outweigh a selected window inside it.
  const bg = isActive ? 'bg-accent-muted-workspace' : state === 'hover' ? 'bg-white/[0.025]' : ''

  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(name)
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (editing) {
      setDraft(name)
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
      title={title}
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
        if (onKill) {
          e.preventDefault()
          onKill()
        }
      }}
      className={`group flex h-[32px] w-full items-center gap-2 rounded-md pl-[22px] pr-2 ${bg}
                  hover:bg-white/[0.025]`}
    >
      {/* Chevron is its own click target — toggles expansion without
          changing which workspace is active. Clicking the rest of the
          row sets active and leaves expansion alone. */}
      <span
        role="button"
        aria-label={expanded ? 'Collapse workspace' : 'Expand workspace'}
        onClick={(e) => {
          e.stopPropagation()
          onToggleExpand?.()
        }}
        className="flex h-5 w-5 shrink-0 cursor-pointer items-center justify-center rounded-sm text-text-tertiary hover:bg-white/[0.06] hover:text-text-secondary"
      >
        <span
          className="font-mono text-[10px] leading-none transition-transform duration-150 ease-out"
          style={{ transform: expanded ? 'rotate(90deg)' : 'none' }}
        >
          ▸
        </span>
      </span>
      <span
        className="font-mono text-[12px]"
        style={{ color: isActive ? 'var(--accent-text)' : 'var(--text-secondary)' }}
      >
        {icon}
      </span>
      {editing ? (
        <input
          ref={inputRef}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
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
          className="flex-1 rounded-sm bg-canvas px-1 text-[14px] text-text-primary outline-none ring-1 ring-accent focus:outline-none"
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
        />
      ) : (
        <span
          className={`flex-1 truncate text-left text-[14px] ${isActive ? 'font-medium text-text-primary' : 'text-text-secondary'}`}
        >
          {name}
        </span>
      )}
      {onAddWindow && (
        <span
          role="button"
          aria-label="New window in workspace"
          title="New window (⌘T)"
          onClick={(e) => {
            e.stopPropagation()
            onAddWindow()
          }}
          className="cursor-pointer rounded-sm px-1 font-mono text-[12px] leading-none text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100 hover:bg-white/[0.06] hover:text-text-secondary"
        >
          +
        </span>
      )}
      {onKill && (
        <span
          role="button"
          aria-label="Kill workspace"
          title="Kill workspace (⌘⇧W)"
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
    </button>
  )
}

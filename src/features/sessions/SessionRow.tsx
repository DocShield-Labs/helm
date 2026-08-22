/**
 * One session in the sidebar: icon · title / subtitle · unread dot.
 * The only indicator is the dot (unread activity); the icon carries
 * what the session is (shell / agent) and whether it's busy (accent).
 * Double-click renames; right-click for rename / kill.
 */

import { useEffect, useRef, useState } from 'react'
import { ContextMenu, type ContextMenuItem } from '@ui'
import { SparkIcon, TerminalIcon, XIcon } from './icons'
import type { PaneKind } from '@lib/session/paneState'

export interface SessionRowProps {
  kind: PaneKind
  running: boolean
  title: string
  subtitle: string
  unread: boolean
  selected: boolean
  onClick: () => void
  onRename: (name: string) => void
  onKill: () => void
}

export function SessionRow({
  kind,
  running,
  title,
  subtitle,
  unread,
  selected,
  onClick,
  onRename,
  onKill,
}: SessionRowProps) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(title)
  const inputRef = useRef<HTMLInputElement>(null)
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)

  useEffect(() => {
    if (editing) {
      setDraft(title)
      requestAnimationFrame(() => {
        inputRef.current?.focus()
        inputRef.current?.select()
      })
    }
  }, [editing, title])

  const commit = () => {
    setEditing(false)
    const next = draft.trim()
    if (next && next !== title) onRename(next)
  }

  const items: Array<ContextMenuItem | 'separator'> = [
    { id: 'rename', label: 'Rename', icon: 'A', onClick: () => setEditing(true) },
    'separator',
    { id: 'kill', label: 'Close session', icon: '×', shortcut: '⌘W', destructive: true, onClick: onKill },
  ]

  const iconColor =
    kind === 'agent'
      ? 'text-[var(--terminal-claude,#D97757)]'
      : running
        ? 'text-accent'
        : 'text-text-secondary'

  return (
    <>
      <div
        role="button"
        tabIndex={0}
        onClick={onClick}
        onDoubleClick={() => setEditing(true)}
        onContextMenu={(e) => {
          e.preventDefault()
          setMenu({ x: e.clientX, y: e.clientY })
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !editing) onClick()
        }}
        className={`helm-row group ${selected ? 'helm-row-selected' : ''}`}
      >
        <span className={`flex size-6 shrink-0 items-center justify-center rounded-full bg-[var(--stroke-default)] ${iconColor}`}>
          {kind === 'agent' ? <SparkIcon size={14} /> : <TerminalIcon size={14} />}
        </span>
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="flex min-w-0 items-center gap-1.5">
            {editing ? (
              <input
                ref={inputRef}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onBlur={commit}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') commit()
                  if (e.key === 'Escape') setEditing(false)
                  e.stopPropagation()
                }}
                onClick={(e) => e.stopPropagation()}
                // Same box as the title it replaces (16px line, no inner
                // padding) so the row doesn't grow while editing; the
                // 4px side padding is pulled back with a negative margin
                // so the text stays where the title was.
                className="-mx-1 h-4 min-w-0 flex-1 rounded-sm bg-[var(--stroke-default)] px-1 py-0 text-[12px] leading-4 text-text-primary outline-none"
              />
            ) : (
              <span className="truncate text-[12px] leading-4 text-text-primary" title={title}>
                {title}
              </span>
            )}
            {unread && !editing && (
              <span className="size-2 shrink-0 rounded-full bg-accent" aria-label="unread" />
            )}
          </span>
          <span className="truncate font-mono text-[10px] leading-[14px] text-text-tertiary" title={subtitle}>
            {subtitle}
          </span>
        </span>
        <button
          type="button"
          aria-label="Close session"
          title="Close session"
          onClick={(e) => {
            e.stopPropagation()
            onKill()
          }}
          className="flex size-5 shrink-0 items-center justify-center rounded text-text-tertiary opacity-0 hover:bg-[var(--stroke-default)] hover:text-text-primary group-hover:opacity-100"
        >
          <XIcon size={12} />
        </button>
      </div>
      {menu && <ContextMenu open x={menu.x} y={menu.y} items={items} onClose={() => setMenu(null)} />}
    </>
  )
}

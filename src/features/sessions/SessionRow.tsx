/** A sidebar session row with rename, close, context menu, and hover details. */

import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { ContextMenu, type ContextMenuItem } from '@ui'
import { BranchIcon, FolderIcon, SparkIcon, TerminalIcon, XIcon } from './icons'
import type { SessionKind } from '@lib/session/sessionState'

export interface SessionRowProps {
  kind: SessionKind
  running: boolean
  /** The command (running or last), agent prompt, or name. */
  title: string
  /** The underlying command, which may differ from a renamed title. */
  detail: string
  /** Working directory (home-relative) — shown in the hover card. */
  dir: string
  /** Git branch — shown in the hover card. */
  branch: string
  unread: boolean
  selected: boolean
  onClick: () => void
  onRename: (name: string) => void
  onKill: () => void
}

/** Gap between the sidebar's right edge and the hover card. */
const HOVER_GAP = 8
/** Keep short hover cards comfortably readable. */
const HOVER_MIN_WIDTH = 220
/** Keep long hover cards inside the app window. */
const HOVER_VIEWPORT_GAP = 12
/** How long the cursor must rest on a card before the panel appears. */
const HOVER_DELAY = 260

export function SessionRow({
  kind,
  running,
  title,
  detail,
  dir,
  branch,
  unread,
  selected,
  onClick,
  onRename,
  onKill,
}: SessionRowProps) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(title)
  const inputRef = useRef<HTMLInputElement>(null)
  const rowRef = useRef<HTMLDivElement>(null)
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)
  // Hover card anchored just past the sidebar, centred on this card.
  const [hover, setHover] = useState<{ left: number; top: number } | null>(null)
  const hoverTimer = useRef(0)

  useEffect(() => {
    if (editing) {
      setDraft(title)
      requestAnimationFrame(() => {
        inputRef.current?.focus()
        inputRef.current?.select()
      })
    }
  }, [editing, title])

  useEffect(() => () => window.clearTimeout(hoverTimer.current), [])

  const openHover = () => {
    if (editing) return
    window.clearTimeout(hoverTimer.current)
    hoverTimer.current = window.setTimeout(() => {
      const row = rowRef.current
      if (!row) return
      const rect = row.getBoundingClientRect()
      // Anchor to the sidebar's right divider, not the card (whose right
      // edge sits inside the padding), so the panel clears the border.
      const sidebarRight = row.closest('aside')?.getBoundingClientRect().right ?? rect.right
      setHover({ left: sidebarRight + HOVER_GAP, top: rect.top + rect.height / 2 })
    }, HOVER_DELAY)
  }
  const closeHover = () => {
    window.clearTimeout(hoverTimer.current)
    setHover(null)
  }

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

  const iconColor = kind === 'agent' ? 'text-accent' : 'text-text-tertiary'
  const titleColor = running ? 'text-text-primary font-medium' : 'text-text-secondary'

  return (
    <>
      <div
        ref={rowRef}
        role="button"
        tabIndex={0}
        onClick={onClick}
        onDoubleClick={() => setEditing(true)}
        onContextMenu={(e) => {
          e.preventDefault()
          closeHover()
          setMenu({ x: e.clientX, y: e.clientY })
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !editing) onClick()
        }}
        onMouseEnter={openHover}
        onMouseLeave={closeHover}
        className={`helm-row helm-row-session group relative ${selected ? 'helm-row-selected' : ''}`}
      >
        <span className={`flex size-6 shrink-0 items-center justify-center rounded-full bg-[var(--stroke-default)] ${iconColor}`}>
          {kind === 'agent' ? <SparkIcon size={14} /> : <TerminalIcon size={14} />}
        </span>
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
            // Same face, size, line-height and box as the title span it
            // replaces — no background, no fixed height, no padding — so
            // entering edit mode moves nothing, not even by a pixel. The
            // row's `user-select: none` inherits into the input and stops
            // WebKit from painting the text-selection highlight, so it is
            // explicitly re-enabled here.
            className="min-w-0 flex-1 select-text bg-transparent p-0 font-mono text-[12px] text-text-primary outline-none"
          />
        ) : (
          <span className={`min-w-0 flex-1 truncate pr-5 font-mono text-[12px] ${titleColor}`}>{title}</span>
        )}
        {unread && !editing && (
          <span className="size-2 shrink-0 rounded-full bg-accent" aria-label="unread" />
        )}
        <button
          type="button"
          aria-label="Close session"
          title="Close session"
          onClick={(e) => {
            e.stopPropagation()
            onKill()
          }}
          // With an unread dot in the corner, hover slides the ×
          // one slot left so the two never stack.
          className={`absolute top-1/2 flex size-5 shrink-0 -translate-y-1/2 items-center justify-center rounded text-text-tertiary opacity-0 hover:bg-[var(--stroke-default)] hover:text-text-primary group-hover:opacity-100 ${unread && !editing ? 'right-7' : 'right-2'}`}
        >
          <XIcon size={12} />
        </button>
      </div>
      {hover &&
        !editing &&
        createPortal(
          <div
            className="pointer-events-none fixed z-50 w-max overflow-hidden -translate-y-1/2 rounded-lg border border-[var(--stroke-default)] bg-elevated px-3 py-2.5"
            style={{
              left: hover.left,
              top: hover.top,
              minWidth: HOVER_MIN_WIDTH,
              maxWidth: `calc(100vw - ${hover.left + HOVER_VIEWPORT_GAP}px)`,
              boxShadow: 'var(--elevation-2)',
            }}
          >
            <div className="truncate whitespace-nowrap font-mono text-[12px] text-text-primary">{detail}</div>
            <div className="mt-2 flex items-center gap-1.5 text-text-secondary">
              <FolderIcon size={12} className="shrink-0" />
              <span className="min-w-0 truncate whitespace-nowrap font-mono text-[11px]">{dir || '—'}</span>
            </div>
            {branch && (
              <div className="mt-1 flex items-center gap-1.5 text-text-tertiary">
                <BranchIcon size={12} className="shrink-0" />
                <span className="min-w-0 truncate whitespace-nowrap font-mono text-[11px]">{branch}</span>
              </div>
            )}
          </div>,
          document.body,
        )}
      {menu && <ContextMenu open x={menu.x} y={menu.y} items={items} onClose={() => setMenu(null)} />}
    </>
  )
}

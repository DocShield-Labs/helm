/**
 * SidebarHostRow — top-level row in the navigation tree.
 * Carries the StatusDot for the host.
 */

import { StatusDot, type StatusDotState } from './StatusDot'

export type SidebarHostRowState = 'rest' | 'hover' | 'active'

export interface SidebarHostRowProps {
  name: string
  status: StatusDotState
  state?: SidebarHostRowState
  expanded?: boolean
  dim?: boolean
  /** Number of pending inbox notifications belonging to this host.
   * Renders as a small badge to the right of the status dot when > 0. */
  notificationCount?: number
  onClick?: () => void
  onToggleExpand?: () => void
  onAddWorkspace?: () => void
  /** Double-click handler (used to open the host editor). */
  onEdit?: () => void
  /** Right-click handler (used to confirm + delete the host). */
  onContextMenu?: () => void
}

export function SidebarHostRow({
  name,
  status,
  state = 'rest',
  expanded = false,
  dim = false,
  notificationCount = 0,
  onClick,
  onEdit,
  onContextMenu,
}: SidebarHostRowProps) {
  const bg =
    state === 'active' ? 'bg-accent-muted' : state === 'hover' ? 'bg-white/[0.025]' : ''
  return (
    <button
      type="button"
      onClick={onClick}
      onDoubleClick={(e) => {
        if (onEdit) {
          e.stopPropagation()
          onEdit()
        }
      }}
      onContextMenu={(e) => {
        if (onContextMenu) {
          e.preventDefault()
          onContextMenu()
        }
      }}
      className={`flex h-[30px] w-full items-center gap-2 rounded-md px-2 ${bg}
                  ${dim ? 'opacity-50' : ''}
                  text-text-primary hover:bg-white/[0.025]`}
    >
      <span className="font-mono text-[10px] text-text-tertiary">{expanded ? '▾' : '▸'}</span>
      <StatusDot state={status} />
      <span className="flex-1 truncate text-left text-[13px] font-medium">{name}</span>
      {notificationCount > 0 && (
        <span
          className="rounded-full px-1.5 py-0.5 font-mono text-[9px] leading-none"
          style={{
            background: 'var(--activity-attention)',
            color: 'var(--text-inverse)',
          }}
          title={`${notificationCount} notification${notificationCount === 1 ? '' : 's'}`}
        >
          {notificationCount}
        </span>
      )}
    </button>
  )
}

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
    </button>
  )
}

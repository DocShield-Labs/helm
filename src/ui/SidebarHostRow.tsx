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
  /** Number of pending inbox notifications belonging to this host.
   * Renders as a small badge to the right of the status dot when > 0. */
  notificationCount?: number
  /** Click anywhere on the row body (excluding the chevron and the
   * status dot). Wired by callers to toggle expansion + set active. */
  onClick?: () => void
  /** Toggle this host's expanded/collapsed state. Used by the chevron;
   * the row body itself usually fires `onClick` which the parent wires
   * to the same handler so the whole row toggles on click. */
  onToggleExpand?: () => void
  /** Click on the status dot. Used to (re)connect a host without
   * affecting the row's expansion state — the dot is a dedicated
   * connect affordance, separate from the body click. */
  onConnect?: () => void
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
  notificationCount = 0,
  onClick,
  onToggleExpand,
  onConnect,
  onEdit,
  onContextMenu,
}: SidebarHostRowProps) {
  // Selected hosts get the lightest accent fill (6%) — they sit
  // visually outside the workspace/window selection so a stronger
  // tint would compete with a selected child below.
  const bg =
    state === 'active'
      ? 'bg-accent-muted-host'
      : state === 'hover'
        ? 'bg-white/[0.025]'
        : ''
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
      className={`flex h-[34px] w-full items-center gap-2 rounded-md px-2 ${bg}
                  text-text-primary hover:bg-white/[0.025]`}
    >
      <span
        role="button"
        aria-label={expanded ? 'Collapse host' : 'Expand host'}
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
        role={onConnect ? 'button' : undefined}
        aria-label={onConnect ? 'Connect host' : undefined}
        onClick={
          onConnect
            ? (e) => {
                e.stopPropagation()
                onConnect()
              }
            : undefined
        }
        className={onConnect ? 'cursor-pointer rounded-sm p-0.5 hover:bg-white/[0.04]' : 'p-0.5'}
        title={onConnect ? 'Connect' : undefined}
      >
        <StatusDot state={status} />
      </span>
      <span className="flex-1 truncate text-left text-[14px] font-medium">{name}</span>
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

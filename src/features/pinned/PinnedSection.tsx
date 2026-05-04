/**
 * PinnedSection — the user's curated working set, surfaced in the
 * sidebar's "Pinned" tab.
 *
 * Each pin is a window the user explicitly stuck to the sidebar via
 * right-click. Pins can cross hosts ("the api-server window on
 * localhost" alongside "the staging logs window on iad-prod-01") and
 * are the natural "what am I working on right now" surface.
 *
 * Resolution: a pin stores `{hostId, workspaceName, windowId}`. We
 * look up the host, find the session by name (resilient to session-id
 * churn after disconnect), then find the window by id.
 *
 * Pin state machine:
 *   - active: this pin is the currently-focused window
 *   - normal: window exists, click to jump
 *   - loading: host hasn't refetched its tree yet (e.g. just-launched
 *              app on localhost); show snapshot labels, no badge
 *   - offline: host is reachable in principle but not connected (remote
 *              that hasn't been clicked yet, or disconnected)
 *   - stale: host's tree is loaded but the workspace/window aren't
 *            there — user killed it elsewhere or tmux server restarted
 */

import { useMemo, useState } from 'react'
import { commands } from '@lib/ipc'
import {
  isWindowRunning,
  notificationsForWindow,
  pinnedKey,
  useStore,
  type PinnedWindow,
  type TmuxWindow,
  type TmuxWorkspace,
} from '@lib/store'
import { connectHost, selectWorkspace } from '@lib/host'
import { killWindow } from '@lib/actions/window'
import type { Host, HostStatus, Notification } from '@bindings'
import {
  ActivityDot,
  type ActivityDotState,
  ContextMenu,
  type ContextMenuItem,
  StatusDot,
} from '@ui'
import { prettyCwd } from '@lib/path'
import { activityFor, phaseFor, pinDotState, resolvePin } from './resolve'

export function PinnedSection() {
  const pinnedWindows = useStore((s) => s.pinnedWindows)
  const removePinnedWindow = useStore((s) => s.removePinnedWindow)
  const hosts = useStore((s) => s.hosts)
  const sessions = useStore((s) => s.sessions)
  const statuses = useStore((s) => s.statuses)
  const notifications = useStore((s) => s.notifications)
  const runningPanes = useStore((s) => s.runningPanes)
  const activeHostId = useStore((s) => s.activeHostId)

  const resolved = useMemo(
    () => pinnedWindows.map((pin) => resolvePin(pin, hosts, sessions)),
    [pinnedWindows, hosts, sessions],
  )

  if (pinnedWindows.length === 0) {
    return (
      <div className="px-3 py-6 text-center">
        <div className="font-mono text-[11px] text-text-tertiary">
          No pinned windows
        </div>
        <div className="mt-2 text-[11px] leading-relaxed text-text-tertiary">
          Right-click any window in the Hosts tab and pick{' '}
          <span className="text-text-secondary">Pin to sidebar</span>{' '}
          to keep it in your working set.
        </div>
      </div>
    )
  }

  return (
    <div className="flex flex-col gap-0.5 py-1">
      {resolved.map(({ pin, host, workspace, window, treeLoaded }) => (
        <PinnedRow
          key={pinnedKey(pin.hostId, pin.workspaceName, pin.windowId)}
          pin={pin}
          host={host}
          workspace={workspace}
          window={window}
          treeLoaded={treeLoaded}
          status={statuses.get(pin.hostId)}
          notifications={
            workspace && window
              ? notificationsForWindow(notifications, sessions.get(pin.hostId), pin.hostId, window.id)
              : []
          }
          running={
            workspace && window
              ? isWindowRunning(runningPanes, pin.hostId, workspace, window.id)
              : false
          }
          isActive={
            activeHostId === pin.hostId &&
            sessions.get(pin.hostId)?.activeWorkspaceId === workspace?.id &&
            window?.active === true
          }
          onRemove={() =>
            removePinnedWindow(pinnedKey(pin.hostId, pin.workspaceName, pin.windowId))
          }
        />
      ))}
    </div>
  )
}

interface PinnedRowProps {
  pin: PinnedWindow
  host: Host | undefined
  workspace: TmuxWorkspace | undefined
  window: TmuxWindow | undefined
  treeLoaded: boolean
  status: HostStatus | undefined
  notifications: Notification[]
  running: boolean
  isActive: boolean
  onRemove: () => void
}

function PinnedRow({
  pin,
  host,
  workspace,
  window,
  treeLoaded,
  status,
  notifications,
  running,
  isActive,
  onRemove,
}: PinnedRowProps) {
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)

  const phase = phaseFor(host, status, treeLoaded, !!workspace, !!window)
  const stale = phase === 'stale'
  const offline = phase === 'offline'

  const activity: ActivityDotState = activityFor(notifications, running)

  // Resolve the active pane's cwd for the subtitle. When the pin is
  // stale or offline we don't have a live tree, so fall back to the
  // pin-time workspace label so the user still has a recognisable
  // breadcrumb.
  const activePane =
    workspace && window
      ? [...workspace.panes.values()]
          .filter((p) => p.windowId === window.id)
          .find((p) => p.active) ?? null
      : null
  const cwd = activePane?.cwd ?? ''
  const subtitle = cwd
    ? `${pin.hostName} · ${prettyCwd(cwd)}`
    : `${pin.hostName} · ${pin.workspaceName}`

  const click = () => {
    if (!host) return
    // Offline → kick off a connect; the row will resolve once the tree
    // refetches and the user can click again to jump in.
    if (offline) {
      void connectHost(host.id)
      return
    }
    if (stale || !workspace || !window) return
    const store = useStore.getState()
    store.setActiveHost(host.id)
    store.setActiveWindow(host.id, workspace.id, window.id)
    void selectWorkspace(host.id, workspace.id)
    void commands.tmuxSelectWindow(host.id, window.id)
  }

  const menuItems: Array<ContextMenuItem | 'separator'> = [
    { id: 'unpin', label: 'Unpin from sidebar', icon: '☆', onClick: onRemove },
  ]
  if (phase === 'normal' && host && workspace && window) {
    menuItems.push('separator')
    menuItems.push({
      id: 'kill',
      label: 'Kill window',
      icon: '×',
      destructive: true,
      onClick: () => killWindow(host.id, workspace.id, window),
    })
  }

  const dotState = pinDotState(host, status, phase)

  return (
    <>
      <button
        type="button"
        onClick={click}
        onContextMenu={(e) => {
          if (menuItems.length === 0) return
          e.preventDefault()
          e.stopPropagation()
          setMenu({ x: e.clientX, y: e.clientY })
        }}
        className={`group relative flex h-[44px] w-full items-center gap-2.5 rounded-md px-2.5 ${
          isActive ? 'bg-accent-muted' : 'hover:bg-white/[0.025]'
        } ${stale ? 'opacity-55' : ''}`}
      >
        <StatusDot state={dotState} />
        {/* pt-0.5 nudges the name+subtitle bundle down so the name has
            a touch more breathing room above it. */}
        <div className="flex min-w-0 flex-1 flex-col items-start gap-px pt-0.5">
          <div className="flex w-full items-center gap-2">
            <span
              className={`flex-1 truncate text-left text-[12px] ${
                isActive ? 'font-medium text-text-primary' : 'text-text-secondary'
              }`}
            >
              {window?.name ?? pin.windowName}
            </span>
            {stale && (
              <span className="font-mono text-[9px] uppercase tracking-[0.08em] text-text-tertiary">
                stale
              </span>
            )}
            {offline && (
              <span className="font-mono text-[9px] uppercase tracking-[0.08em] text-status-disconnected">
                offline
              </span>
            )}
          </div>
          <span
            className="truncate text-left font-mono text-[10px] text-text-tertiary"
            title={cwd || undefined}
          >
            {subtitle}
          </span>
        </div>
        <ActivityDot state={activity} />
        {stale && (
          <span
            role="button"
            aria-label="Remove pin"
            title="Remove pin"
            onClick={(e) => {
              e.stopPropagation()
              onRemove()
            }}
            className="cursor-pointer rounded-sm px-1 font-mono text-[12px] leading-none text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100 hover:bg-white/[0.06] hover:text-text-secondary"
          >
            ×
          </span>
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


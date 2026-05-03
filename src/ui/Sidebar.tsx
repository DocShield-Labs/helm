/**
 * Sidebar — floating navigation rail.
 *
 * Two widths: 280px expanded (full host/workspace/window tree) and 48px
 * collapsed (just host status dots). Toggled with ⌘\ or the chevron at
 * the bottom of the rail. When collapsed, hovering a host icon reveals
 * its workspaces + windows in a popover so the user can switch without
 * re-expanding the sidebar.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import { commands } from '@lib/ipc'
import {
  notificationsForHost,
  notificationsForWindow,
  notificationsForWorkspace,
  useStore,
  type HostSessions,
  type TmuxWorkspace,
} from '@lib/store'
import type { Notification } from '@bindings'
import { connectHost, selectWorkspace } from '@lib/host'
import type { Host, HostId, HostStatus } from '@bindings'
import { StatusDot, type StatusDotState } from './StatusDot'
import { SidebarHostRow } from './SidebarHostRow'
import { SidebarWorkspaceRow } from './SidebarWorkspaceRow'
import { SidebarWindowRow } from './SidebarWindowRow'
import { InboxSection } from '@features/activity-feed/InboxSection'

export interface SidebarProps {
  /** Open the host editor for a brand-new host. */
  onAddHost: () => void
  /** Open the host editor pre-filled with this host. */
  onEditHost: (host: Host) => void
  /** Delete a remote host (caller handles confirm). */
  onDeleteHost: (host: Host) => void
  /** Create a fresh workspace on the given host. */
  onCreateWorkspace: (hostId: HostId) => void
  /** Kill a workspace (caller handles confirm). */
  onKillWorkspace: (hostId: HostId, w: TmuxWorkspace) => void
}

export function Sidebar(props: SidebarProps) {
  const collapsed = useStore((s) => s.sidebarCollapsed)
  const setSidebarCollapsed = useStore((s) => s.setSidebarCollapsed)

  return (
    <aside
      className="pointer-events-none absolute inset-y-2 left-2 z-20 flex"
      style={{
        width: collapsed ? 48 : 280,
        transition: 'width 180ms cubic-bezier(0.2, 0.7, 0.2, 1)',
      }}
    >
      <div
        className="pointer-events-auto flex h-full w-full flex-col overflow-hidden rounded-xl border border-white/[0.06] bg-elevated"
        style={{ boxShadow: 'var(--elevation-2)' }}
      >
        {collapsed ? (
          <CollapsedSidebar
            onExpand={() => setSidebarCollapsed(false)}
            {...props}
          />
        ) : (
          <ExpandedSidebar
            onCollapse={() => setSidebarCollapsed(true)}
            {...props}
          />
        )}
      </div>
    </aside>
  )
}

// ===========================================================================
// Expanded — full tree (the existing experience)
// ===========================================================================

interface ExpandedProps extends SidebarProps {
  onCollapse: () => void
}

function ExpandedSidebar({
  onAddHost,
  onEditHost,
  onDeleteHost,
  onCreateWorkspace,
  onKillWorkspace,
  onCollapse,
}: ExpandedProps) {
  const sortedHosts = useSortedHosts()
  const activeHostId = useStore((s) => s.activeHostId)
  const setActiveHost = useStore((s) => s.setActiveHost)
  const sessions = useStore((s) => s.sessions)
  const statuses = useStore((s) => s.statuses)
  const notifications = useStore((s) => s.notifications)
  const toggleWorkspaceCollapsed = useStore((s) => s.toggleWorkspaceCollapsed)

  return (
    <div className="flex flex-1 flex-col gap-1 overflow-y-auto p-3">
      <InboxSection />
      <div className="flex items-center justify-between pb-1 pt-1 pl-2 pr-1">
        <span className="text-[10px] font-medium tracking-[0.08em] text-text-tertiary">
          HOSTS
        </span>
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={onAddHost}
            className="rounded-sm px-1.5 font-mono text-[12px] leading-none text-text-tertiary hover:bg-white/[0.04] hover:text-text-secondary"
            title="Add host"
          >
            +
          </button>
          <button
            type="button"
            onClick={onCollapse}
            className="rounded-sm px-1.5 font-mono text-[12px] leading-none text-text-tertiary hover:bg-white/[0.04] hover:text-text-secondary"
            title="Collapse sidebar (⌘\)"
          >
            ⇤
          </button>
        </div>
      </div>
      {sortedHosts.map((h) => {
        const hs = sessions.get(h.id)
        const status = statuses.get(h.id) ?? 'disconnected'
        const isActive = activeHostId === h.id
        const hostNotifs = notificationsForHost(notifications, h.id)
        const workspaceList = hs
          ? [...hs.workspaces.values()].sort((a, b) => a.name.localeCompare(b.name))
          : []
        return (
          <div key={h.id} className="flex flex-col gap-1">
            <SidebarHostRow
              name={h.name}
              status={displayedHostStatus(h, status, hs?.detachedReason ?? null)}
              state={isActive ? 'active' : 'rest'}
              expanded
              notificationCount={hostNotifs.length}
              onClick={() => {
                setActiveHost(h.id)
                // Click acts as manual reconnect whenever the host
                // isn't actually live — covers a fresh remote
                // (`disconnected`), a remote stuck reconnecting, and
                // localhost whose tmux died. Connected/idle hosts
                // just get their selection updated.
                if (status !== 'connected' && status !== 'idle') {
                  void connectHost(h.id)
                }
              }}
              onEdit={h.port === 0 ? undefined : () => onEditHost(h)}
              onContextMenu={h.port === 0 ? undefined : () => onDeleteHost(h)}
            />
            {isActive &&
              workspaceList.map((w) => {
                const isActiveWs = hs?.activeWorkspaceId === w.id
                const expanded = !(hs?.collapsedWorkspaces.has(w.id) ?? false)
                const winsForWs = [...w.windows.values()].sort((a, b) =>
                  a.id.localeCompare(b.id),
                )
                const wsNotifs = notificationsForWorkspace(notifications, hs, h.id, w.id)
                return (
                  <div key={w.id} className="flex flex-col gap-1">
                    <SidebarWorkspaceRow
                      name={w.name}
                      activity={
                        wsNotifs.length > 0
                          ? rollupActivity(wsNotifs)
                          : w.windows.size > 0
                            ? 'running'
                            : 'idle'
                      }
                      state={isActiveWs ? 'active' : 'rest'}
                      expanded={expanded}
                      onClick={() => toggleWorkspaceCollapsed(h.id, w.id)}
                      onToggleExpand={() => toggleWorkspaceCollapsed(h.id, w.id)}
                      onRename={(newName) => {
                        if (newName && newName !== w.name) {
                          void commands.tmuxRenameSession(h.id, w.id, newName)
                        }
                      }}
                      onKill={() => onKillWorkspace(h.id, w)}
                      onAddWindow={() => {
                        // Match Cmd+T: spawn a new window in this
                        // workspace and pin selection on it.
                        useStore.getState().setActiveHost(h.id)
                        void selectWorkspace(h.id, w.id)
                        void commands.tmuxNewWindow(h.id, w.id, null)
                      }}
                    />
                    {expanded && (
                      <>
                        {winsForWs.length === 0 ? (
                          <div className="px-3 py-2 font-mono text-[11px] text-text-tertiary">
                            no windows
                          </div>
                        ) : (
                          winsForWs.map((win) => {
                            const isFocused =
                              activeHostId === h.id && isActiveWs && win.active
                            const winNotifs = notificationsForWindow(
                              notifications,
                              hs,
                              h.id,
                              win.id,
                            )
                            return (
                              <SidebarWindowRow
                                key={win.id}
                                name={win.name}
                                command={paneCommandFor(win.id, w)}
                                activity={
                                  winNotifs.length > 0
                                    ? rollupActivity(winNotifs)
                                    : win.active
                                      ? 'running'
                                      : 'idle'
                                }
                                state={isFocused ? 'focused' : 'rest'}
                                onClick={() => {
                                  const store = useStore.getState()
                                  store.setActiveHost(h.id)
                                  store.setActiveWindow(h.id, w.id, win.id)
                                  void selectWorkspace(h.id, w.id)
                                  void commands.tmuxSelectWindow(h.id, win.id)
                                }}
                                onRename={(newName) => {
                                  if (newName && newName !== win.name) {
                                    void commands.tmuxRenameWindow(h.id, win.id, newName)
                                  }
                                }}
                                onKill={() => void commands.tmuxKillWindow(h.id, win.id)}
                              />
                            )
                          })
                        )}
                      </>
                    )}
                  </div>
                )
              })}
            {isActive && (
              <button
                type="button"
                className="rounded-md px-2 py-1 text-left font-mono text-[11px] text-text-tertiary hover:bg-white/[0.04] hover:text-text-secondary"
                onClick={() => onCreateWorkspace(h.id)}
                title="Cmd+Shift+T"
              >
                + workspace
              </button>
            )}
          </div>
        )
      })}
    </div>
  )
}

// ===========================================================================
// Collapsed — 48px dot rail with hover-peek popover
// ===========================================================================

interface CollapsedProps extends SidebarProps {
  onExpand: () => void
}

function CollapsedSidebar({
  onAddHost,
  onCreateWorkspace,
  onKillWorkspace,
  onExpand,
}: CollapsedProps) {
  const sortedHosts = useSortedHosts()
  const sessions = useStore((s) => s.sessions)
  const statuses = useStore((s) => s.statuses)
  const activeHostId = useStore((s) => s.activeHostId)
  const setActiveHost = useStore((s) => s.setActiveHost)

  const [peekHostId, setPeekHostId] = useState<HostId | null>(null)
  const [peekAnchorTop, setPeekAnchorTop] = useState(0)
  const closeTimer = useRef<number | null>(null)

  const cancelClose = () => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current)
      closeTimer.current = null
    }
  }
  const scheduleClose = () => {
    cancelClose()
    closeTimer.current = window.setTimeout(() => {
      setPeekHostId(null)
      closeTimer.current = null
    }, 140)
  }
  useEffect(() => () => cancelClose(), [])

  const peekHost = peekHostId ? sortedHosts.find((h) => h.id === peekHostId) : null

  return (
    <div className="flex h-full flex-col items-center gap-1 px-2 pt-3 pb-2">
      {sortedHosts.map((h) => {
        const status = statuses.get(h.id) ?? 'disconnected'
        const isActive = activeHostId === h.id
        return (
          <button
            key={h.id}
            type="button"
            onClick={() => {
              setActiveHost(h.id)
              if (status !== 'connected' && status !== 'idle') {
                void connectHost(h.id)
              }
            }}
            onMouseEnter={(e) => {
              cancelClose()
              const rect = e.currentTarget.getBoundingClientRect()
              setPeekAnchorTop(rect.top)
              setPeekHostId(h.id)
            }}
            onMouseLeave={scheduleClose}
            title={h.name}
            className={`flex h-8 w-8 items-center justify-center rounded-md ${
              isActive ? 'bg-accent-muted' : 'hover:bg-white/[0.04]'
            }`}
          >
            <StatusDot state={displayedHostStatus(h, status, sessions.get(h.id)?.detachedReason ?? null)} />
          </button>
        )
      })}
      <button
        type="button"
        onClick={onAddHost}
        className="flex h-8 w-8 items-center justify-center rounded-md font-mono text-[14px] text-text-tertiary hover:bg-white/[0.04] hover:text-text-secondary"
        title="Add host"
      >
        +
      </button>

      <div className="flex-1" />

      <button
        type="button"
        onClick={onExpand}
        className="flex h-8 w-8 items-center justify-center rounded-md font-mono text-[12px] text-text-tertiary hover:bg-white/[0.04] hover:text-text-secondary"
        title="Expand sidebar (⌘\)"
      >
        ⇥
      </button>

      {peekHost && (
        <HostPeekPopover
          host={peekHost}
          hs={sessions.get(peekHost.id)}
          activeHostId={activeHostId}
          anchorTop={peekAnchorTop}
          onMouseEnter={cancelClose}
          onMouseLeave={scheduleClose}
          onCreateWorkspace={() => onCreateWorkspace(peekHost.id)}
          onKillWorkspace={(w) => onKillWorkspace(peekHost.id, w)}
          onClose={() => {
            cancelClose()
            setPeekHostId(null)
          }}
        />
      )}
    </div>
  )
}

// ===========================================================================
// Hover peek popover — shown next to the collapsed dot rail
// ===========================================================================

interface HostPeekPopoverProps {
  host: Host
  hs: HostSessions | undefined
  activeHostId: HostId | null
  anchorTop: number
  onMouseEnter: () => void
  onMouseLeave: () => void
  onCreateWorkspace: () => void
  onKillWorkspace: (w: TmuxWorkspace) => void
  onClose: () => void
}

function HostPeekPopover({
  host,
  hs,
  activeHostId,
  anchorTop,
  onMouseEnter,
  onMouseLeave,
  onCreateWorkspace,
  onKillWorkspace,
  onClose,
}: HostPeekPopoverProps) {
  const setActiveHost = useStore((s) => s.setActiveHost)
  const statuses = useStore((s) => s.statuses)
  const status = statuses.get(host.id) ?? 'disconnected'
  const displayed = displayedHostStatus(host, status, hs?.detachedReason ?? null)

  const workspaceList = useMemo(() => {
    if (!hs) return []
    return [...hs.workspaces.values()].sort((a, b) => a.name.localeCompare(b.name))
  }, [hs])

  const totalWindows = useMemo(
    () => workspaceList.reduce((sum, w) => sum + w.windows.size, 0),
    [workspaceList],
  )

  // Whether the host is in a state where we don't yet know its workspace
  // tree. We show a connect CTA instead of an empty "no workspaces"
  // message, since the host probably DOES have windows — we just haven't
  // fetched them yet.
  const offline = displayed === 'disconnected' || displayed === 'error'
  const connecting = displayed === 'connecting'

  // Position the popover next to the sidebar (left=2 + width=48 + gap=8 = 58),
  // vertically anchored near the hovered icon. We clamp to keep it on-screen.
  const top = Math.max(8, Math.min(anchorTop - 8, window.innerHeight - 240))

  return (
    <div
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
      className="pointer-events-auto fixed z-30 w-[244px] overflow-hidden rounded-xl border border-white/[0.06] bg-elevated"
      style={{
        left: 58,
        top,
        boxShadow: 'var(--elevation-2)',
      }}
      role="dialog"
    >
      {/* Host header */}
      <div className="flex items-center gap-2 px-3 pt-3 pb-2">
        <StatusDot state={displayed} />
        <span className="flex-1 truncate text-[13px] font-medium text-text-primary">
          {host.name}
        </span>
        <span className="font-mono text-[10px] text-text-tertiary">
          {connecting
            ? 'connecting…'
            : offline
              ? hs?.detachedReason ?? 'disconnected'
              : totalWindows === 1
                ? '1 window'
                : `${totalWindows} windows`}
        </span>
      </div>

      <div className="border-t border-white/[0.06]" />

      <div className="max-h-[360px] overflow-y-auto py-1">
        {offline ? (
          <div className="flex flex-col items-start gap-0.5 px-3 py-3">
            <span className="text-[12px] text-text-secondary">Click the dot to connect</span>
            <span className="font-mono text-[10px] text-text-tertiary">
              {host.user}@{host.hostname}:{host.port}
            </span>
          </div>
        ) : connecting ? (
          <div className="px-3 py-3 font-mono text-[11px] text-text-tertiary">
            connecting…
          </div>
        ) : workspaceList.length === 0 ? (
          <div className="px-3 py-3 font-mono text-[11px] text-text-tertiary">
            no workspaces
          </div>
        ) : (
          workspaceList.map((w) => {
            const winsForWs = [...w.windows.values()].sort((a, b) =>
              a.id.localeCompare(b.id),
            )
            return (
              <div key={w.id} className="pb-1">
                <div
                  className="flex items-center gap-2 px-3 pt-2 pb-1 text-[10px] font-medium tracking-[0.08em] text-text-tertiary"
                  onContextMenu={(e) => {
                    e.preventDefault()
                    onKillWorkspace(w)
                  }}
                >
                  <span className="truncate uppercase">{w.name}</span>
                  <span className="opacity-60">·</span>
                  <span className="opacity-60">
                    {w.windows.size === 0
                      ? 'empty'
                      : w.windows.size === 1
                        ? '1 window'
                        : `${w.windows.size} windows`}
                  </span>
                </div>
                {winsForWs.length === 0 ? null : (
                  winsForWs.map((win) => {
                    const isFocused =
                      activeHostId === host.id &&
                      hs?.activeWorkspaceId === w.id &&
                      win.active
                    return (
                      <button
                        key={win.id}
                        type="button"
                        onClick={() => {
                          const store = useStore.getState()
                          store.setActiveHost(host.id)
                          store.setActiveWindow(host.id, w.id, win.id)
                          void selectWorkspace(host.id, w.id)
                          void commands.tmuxSelectWindow(host.id, win.id)
                          onClose()
                        }}
                        className={`flex h-7 w-full items-center gap-2 px-3 ${
                          isFocused ? 'bg-accent-muted' : 'hover:bg-white/[0.04]'
                        }`}
                      >
                        <span
                          className="font-mono text-[11px] leading-none"
                          style={{
                            color: isFocused
                              ? 'var(--accent-text)'
                              : 'var(--text-tertiary)',
                          }}
                        >
                          ▢
                        </span>
                        <span
                          className={`text-[12px] ${
                            isFocused
                              ? 'font-medium text-text-primary'
                              : 'text-text-secondary'
                          }`}
                        >
                          {win.name}
                        </span>
                        <span className="flex-1 truncate text-left font-mono text-[10px] text-text-tertiary">
                          {paneCommandFor(win.id, w)}
                        </span>
                      </button>
                    )
                  })
                )}
              </div>
            )
          })
        )}
        {!offline && !connecting && (
          <button
            type="button"
            onClick={() => {
              setActiveHost(host.id)
              onCreateWorkspace()
            }}
            className="flex h-7 w-full items-center px-3 font-mono text-[11px] text-text-tertiary hover:bg-white/[0.04] hover:text-text-secondary"
            title="Cmd+Shift+T"
          >
            + workspace
          </button>
        )}
      </div>
    </div>
  )
}

// ===========================================================================
// Helpers
// ===========================================================================

function useSortedHosts(): Host[] {
  const hosts = useStore((s) => s.hosts)
  return useMemo(
    () =>
      [...hosts.values()].sort((a, b) => {
        if (a.port === 0 && b.port !== 0) return -1
        if (b.port === 0 && a.port !== 0) return 1
        return a.name.localeCompare(b.name)
      }),
    [hosts],
  )
}

function paneCommandFor(windowId: string, w: TmuxWorkspace): string {
  const inWindow = [...w.panes.values()].filter((p) => p.windowId === windowId)
  const active = inWindow.find((p) => p.active) ?? inWindow[0]
  return active?.command ?? ''
}

/** Highest-urgency notification kind across a list, mapped to an
 * ActivityDot state. Failed (non-zero exit) outranks attention (bell)
 * outranks running (success/done). Used for workspace + window dot
 * rollups. */
function rollupActivity(
  notifs: Notification[],
): 'failed' | 'attention' | 'running' {
  let hasFailed = false
  let hasBell = false
  for (const n of notifs) {
    if (
      n.kind.kind === 'command_done' &&
      n.kind.exit_code !== null &&
      n.kind.exit_code !== undefined &&
      n.kind.exit_code !== 0
    ) {
      hasFailed = true
    }
    if (n.kind.kind === 'bell') hasBell = true
  }
  if (hasFailed) return 'failed'
  if (hasBell) return 'attention'
  return 'running'
}

function displayedHostStatus(
  host: Host,
  status: HostStatus | undefined,
  detached: string | null,
): StatusDotState {
  // Localhost stays green for the steady state ("localhost is always
  // there") but surfaces real trouble — Reconnecting because tmux died
  // and we're respawning, Error because tmux can't be brought up at
  // all. Without this, a dead local tmux would show green forever and
  // the user has no signal that typing/shortcuts are silently failing.
  if (host.port === 0) {
    if (status === undefined || status === 'connected' || status === 'idle') {
      return 'connected'
    }
  }
  if (detached) return 'disconnected'
  if (status === 'connected' || status === 'idle') return 'connected'
  // `reconnecting` collapses to the amber `connecting` color — the
  // overlay tells the user *why*; the dot just signals "in flight".
  if (status === 'connecting' || status === 'reconnecting') return 'connecting'
  if (status === 'error') return 'error'
  return 'disconnected'
}

/**
 * Sidebar — the session list. One row per window across every host,
 * grouped by host and, within a host, by project (the git toplevel,
 * or the directory outside a repo — see sidebarGroups.ts); the inbox
 * as a single row above. Nothing else.
 *
 * Hidden entirely when collapsed (⌘\) — the top bar's toggle brings
 * it back. Width and rhythm follow Warp's vertical tabs panel.
 */

import { useMemo, useState } from 'react'
import { commands } from '@lib/ipc'
import {
  notificationsForWindow,
  sortById,
  useStore,
  type HostSessions,
  type TmuxWindow,
  type TmuxWorkspace,
} from '@lib/store'
import { connectHost, selectWindow } from '@lib/host'
import { displayedHostStatus } from '@lib/host-status'
import { homeRelative } from '@lib/path'
import { groupRows, relativeCwd } from '@lib/session/sidebarGroups'
import { agentPromptOf, commandName, isAgentCommand, type PaneKind } from '@lib/session/paneState'
import { killWindow } from '@lib/actions/window'
import { ContextMenu, type ContextMenuItem } from '@ui'
import { InboxSection } from '@features/activity-feed/InboxSection'
import type { Host, HostId } from '@bindings'
import { SessionRow } from './SessionRow'
import { ChevronDownIcon, InboxIcon, PlusIcon, SearchIcon } from './icons'

export interface SidebarProps {
  onAddHost: () => void
  onEditHost: (host: Host) => void
  onDeleteHost: (host: Host) => void
}

export function Sidebar({ onAddHost, onEditHost, onDeleteHost }: SidebarProps) {
  const collapsed = useStore((s) => s.sidebarCollapsed)
  const hosts = useStore((s) => s.hosts)
  const notifications = useStore((s) => s.notifications)
  const [filter, setFilter] = useState('')
  const [inboxOpen, setInboxOpen] = useState(false)

  const sortedHosts = useMemo(() => {
    const list = [...hosts.values()]
    list.sort((a, b) => {
      if ((a.port === 0) !== (b.port === 0)) return a.port === 0 ? -1 : 1
      return a.name.localeCompare(b.name)
    })
    return list
  }, [hosts])

  if (collapsed) return null

  const unread = notifications.size

  return (
    <aside className="flex h-full w-[248px] shrink-0 flex-col border-r border-[var(--stroke-default)] bg-sidebar">
      {/* Title-bar strip: traffic lights live in the first 80px. */}
      <div data-tauri-drag-region className="flex h-[38px] shrink-0 items-center gap-1 pl-[80px] pr-2">
        <label className="flex h-6 min-w-0 flex-1 items-center gap-1.5 rounded-md px-1.5 text-text-tertiary focus-within:bg-[var(--stroke-subtle)]">
          <SearchIcon size={12} />
          <input
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter sessions"
            spellCheck={false}
            className="min-w-0 flex-1 bg-transparent text-[11px] text-text-primary outline-none placeholder:text-text-disabled"
          />
        </label>
        <button
          type="button"
          onClick={newWindow}
          title="New session (⌘T)"
          className="flex size-6 shrink-0 items-center justify-center rounded-md text-text-tertiary hover:bg-[var(--stroke-subtle)] hover:text-text-primary"
        >
          <PlusIcon size={14} />
        </button>
      </div>

      <div className="helm-scroll flex min-h-0 flex-1 flex-col gap-px overflow-y-auto px-2 pb-2 pt-1">
        {unread > 0 && (
          <>
            <div
              role="button"
              onClick={() => setInboxOpen((v) => !v)}
              className={`helm-row ${inboxOpen ? 'helm-row-selected' : ''}`}
            >
              <span className="flex size-6 shrink-0 items-center justify-center rounded-full bg-[var(--stroke-default)] text-text-secondary">
                <InboxIcon size={14} />
              </span>
              <span className="flex-1 text-[12px] text-text-primary">Inbox</span>
              <span className="font-mono text-[10px] text-text-tertiary">
                {unread} unread
              </span>
              <ChevronDownIcon
                size={12}
                className={`text-text-disabled transition-transform ${inboxOpen ? 'rotate-180' : ''}`}
              />
            </div>
            {inboxOpen && (
              <div className="py-1">
                <InboxSection hideHeader />
              </div>
            )}
          </>
        )}

        {sortedHosts.map((h) => (
          <HostGroup
            key={h.id}
            host={h}
            filter={filter.trim().toLowerCase()}
            onEdit={h.port === 0 ? undefined : () => onEditHost(h)}
            onDelete={h.port === 0 ? undefined : () => onDeleteHost(h)}
          />
        ))}
      </div>

      <div className="flex shrink-0 px-2 pb-2">
        <button
          type="button"
          onClick={onAddHost}
          className="flex h-7 items-center gap-1.5 rounded-md px-2 text-[11px] text-text-tertiary hover:bg-[var(--stroke-subtle)] hover:text-text-primary"
        >
          <PlusIcon size={12} />
          Add host
        </button>
      </div>
    </aside>
  )
}

function newWindow() {
  const state = useStore.getState()
  const hostId = state.activeHostId
  if (!hostId) return
  const hs = state.sessions.get(hostId)
  const wsId = hs?.activeWorkspaceId ?? hs?.workspaces.keys().next().value
  if (!wsId) return
  void commands.windowNew(hostId, wsId, null, null, null).then((res) => {
    if (res.status === 'ok' && res.data.window_id) selectWindow(hostId, wsId, res.data.window_id)
  })
}

interface HostGroupProps {
  host: Host
  filter: string
  onEdit?: () => void
  onDelete?: () => void
}

function HostGroup({ host, filter, onEdit, onDelete }: HostGroupProps) {
  const hs = useStore((s) => s.sessions.get(host.id))
  const status = useStore((s) => s.statuses.get(host.id) ?? 'disconnected')
  const activeHostId = useStore((s) => s.activeHostId)
  const notifications = useStore((s) => s.notifications)
  const runningPanes = useStore((s) => s.runningPanes)
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)

  const displayed = displayedHostStatus(host, status)
  const connected = displayed === 'connected'
  const isActiveHost = activeHostId === host.id

  const rows = useMemo(() => buildRows(host.id, hs, runningPanes), [host.id, hs, runningPanes])
  const groups = useMemo(() => {
    const all = groupRows(rows)
    if (!filter) return all
    return all
      .map((g) => ({
        ...g,
        rows: g.rows.filter((r) => `${r.title} ${r.subtitle} ${g.label}`.toLowerCase().includes(filter)),
      }))
      .filter((g) => g.rows.length > 0)
  }, [rows, filter])

  const statusText =
    displayed === 'connected'
      ? null
      : displayed === 'connecting'
        ? status === 'reconnecting'
          ? 'reconnecting…'
          : 'connecting…'
        : displayed === 'error'
          ? 'error'
          : 'offline'

  const items: Array<ContextMenuItem | 'separator'> = []
  if (!connected) {
    items.push({ id: 'connect', label: 'Connect', icon: '⇄', onClick: () => void connectHost(host.id) })
  }
  if (onEdit) items.push({ id: 'edit', label: 'Edit host…', icon: 'A', onClick: onEdit })
  if (onDelete) {
    if (items.length) items.push('separator')
    items.push({ id: 'delete', label: 'Delete host', icon: '×', destructive: true, onClick: onDelete })
  }

  return (
    <div className="flex flex-col gap-px pt-3 first:pt-1">
      <div
        role="button"
        onClick={() => {
          useStore.getState().setActiveHost(host.id)
          if (!connected && displayed !== 'connecting') void connectHost(host.id)
        }}
        onContextMenu={(e) => {
          if (!items.length) return
          e.preventDefault()
          setMenu({ x: e.clientX, y: e.clientY })
        }}
        className="flex h-5 select-none items-center gap-2 px-2"
        title={connected ? host.name : `${host.name} — click to connect`}
      >
        <span className={`truncate text-[10px] ${connected ? 'text-text-tertiary' : 'text-text-disabled'}`}>
          {host.name}
        </span>
        <span className="flex-1" />
        {statusText && (
          <span className={`text-[10px] ${displayed === 'error' ? 'text-status-error' : 'text-text-disabled'}`}>
            {statusText}
          </span>
        )}
      </div>
      {groups.map((g, i) => (
        <div key={g.key || '\0unknown'} className={`flex flex-col gap-px ${i > 0 ? 'pt-2' : ''}`}>
          {g.label && (
            // The project header: as quiet as the host line above it.
            <div className="flex h-[18px] items-center px-2" title={g.key}>
              <span className="truncate font-mono text-[10px] text-text-tertiary">{g.label}</span>
            </div>
          )}
          {g.rows.map((r) => (
            <SessionRow
              key={r.key}
              kind={r.kind}
              running={r.running}
              title={r.title}
              subtitle={r.subtitle}
              unread={notificationsForWindow(notifications, hs, host.id, r.window.id).length > 0}
              selected={isActiveHost && hs?.activeWorkspaceId === r.workspace.id && r.window.active}
              onClick={() => selectWindow(host.id, r.workspace.id, r.window.id)}
              onRename={(name) => void commands.windowRename(host.id, r.window.id, name)}
              onKill={() => killWindow(host.id, r.workspace.id, r.window)}
            />
          ))}
        </div>
      ))}
      {connected && rows.length === 0 && (
        <div className="px-2 py-1.5 text-[11px] text-text-disabled">no sessions · ⌘T</div>
      )}
      {menu && <ContextMenu open x={menu.x} y={menu.y} items={items} onClose={() => setMenu(null)} />}
    </div>
  )
}

interface Row {
  key: string
  workspace: TmuxWorkspace
  window: TmuxWindow
  kind: PaneKind
  running: boolean
  title: string
  subtitle: string
  /** Grouping inputs (see sidebarGroups.ts). */
  root: string
  cwd: string
}

/** Every window on the host in a stable order: workspaces by name,
 * then windows by id (creation order) — grouping by project happens
 * on top of this and never reorders it. Labels come from what the
 * pane is doing — the running command, the agent's prompt — and where
 * it is *within its project*, since the group header already says
 * which project. */
function buildRows(
  hostId: HostId,
  hs: HostSessions | undefined,
  runningPanes: Map<string, { hostId: HostId; startedAt: number; command: string | null }>,
): Row[] {
  if (!hs) return []
  const out: Row[] = []
  const workspaces = [...hs.workspaces.values()].sort((a, b) => a.name.localeCompare(b.name))
  for (const ws of workspaces) {
    for (const win of sortById(ws.windows.values())) {
      const panes = [...ws.panes.values()].filter((p) => p.windowId === win.id)
      const pane = panes.find((p) => p.active) ?? panes[0]
      const run = pane ? runningPanes.get(`${hostId}::${pane.id}`) : undefined
      const program = run ? commandName(run.command) : (pane?.command || null)
      const kind: PaneKind = isAgentCommand(program) ? 'agent' : 'shell'
      const root = pane?.root ?? ''
      const cwd = pane?.cwd ?? ''
      // Where the session is inside its group: the subdirectory, or the
      // project's own name when it sits at the top (so a row never reads
      // as blank).
      const rel = relativeCwd({ root, cwd })
      const here = rel || basename(root || cwd)
      const branch = pane?.branch || null
      const renamed = !/^(zsh|bash|fish|sh|-?\w*sh|\d+|window \d+)$/i.test(win.name)
      let title: string
      let subtitle: string
      if (kind === 'agent') {
        title = (renamed ? win.name : agentPromptOf(run?.command)) || program || 'agent'
        subtitle = [program, rel || null].filter(Boolean).join(' · ')
      } else if (run) {
        title = renamed ? win.name : (run.command?.trim() || here || win.name)
        subtitle = renamed && run.command ? run.command : [rel || null, branch].filter(Boolean).join(' · ')
      } else {
        title = renamed ? win.name : here || win.name
        subtitle = [renamed ? rel || null : null, branch].filter(Boolean).join(' · ') || 'idle'
      }
      out.push({ key: `${ws.id}:${win.id}`, workspace: ws, window: win, kind, running: !!run, title, subtitle, root, cwd })
    }
  }
  return out
}


/** `/Users/x/Code/bento` → `bento`; `~` for a home directory. */
function basename(path: string): string {
  if (!path) return ''
  const rel = homeRelative(path)
  if (rel === '~') return '~'
  const trimmed = path.replace(/\/+$/, '')
  return trimmed.slice(trimmed.lastIndexOf('/') + 1) || '/'
}

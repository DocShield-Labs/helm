/**
 * Sidebar — the session list. One row per session across every host,
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
  notificationSessionIds,
  sortById,
  useStore,
  type HostSessions,
  type RunningSession,
  type Session,
} from '@lib/store'
import { activateAndConnectHost, selectSession } from '@lib/host'
import { displayedHostStatus } from '@lib/host-status'
import { homeRelative } from '@lib/path'
import { groupRows } from '@lib/session/sidebarGroups'
import { getSessionBlocks, lastFinished, useHostBlockLoadRevision } from '@lib/session/blocks'
import type { SessionKind } from '@lib/session/sessionState'
import { agentPromptOf, commandName, isAgentCommand } from '@lib/session/agents'
import { killSession, openSession } from '@lib/actions/session'
import { ContextMenu, type ContextMenuItem } from '@ui'
import { InboxSection } from '@features/activity-feed/InboxSection'
import type { Host, HostId } from '@bindings'
import { SessionRow } from './SessionRow'
import { ChevronDownIcon, InboxIcon, MachineIcon, MoreVerticalIcon, PlusIcon, SearchIcon } from './icons'

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
    // Retired daemon generations never get their own group — their
    // sessions render inside the parent host's group below.
    const list = [...hosts.values()].filter((h) => !h.retired)
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
      {/* The sidebar sits below the full-width top bar (which hosts the
          traffic lights and the collapse toggle), so it opens directly
          with its own content.
          Filter section: the field sits naked on the sidebar (no fill),
          with the new-session button on the same line, right-aligned and
          icon-only so it stays quiet. A hairline closes the section. */}
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-[var(--stroke-default)] px-3">
        <SearchIcon size={13} className="shrink-0 text-text-tertiary" />
        <input
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Search sessions..."
          spellCheck={false}
          className="min-w-0 flex-1 bg-transparent text-[12px] text-text-primary outline-none placeholder:text-text-disabled"
        />
        <button
          type="button"
          onClick={() => void openSession()}
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
            soleHost={sortedHosts.length === 1}
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

interface HostGroupProps {
  host: Host
  filter: string
  /** True when this is the only host — its header line is then noise. */
  soleHost: boolean
  onEdit?: () => void
  onDelete?: () => void
}

function HostGroup({ host, filter, soleHost, onEdit, onDelete }: HostGroupProps) {
  const allHosts = useStore((s) => s.hosts)
  const sessionsByHost = useStore((s) => s.sessions)
  const status = useStore((s) => s.statuses.get(host.id) ?? 'disconnected')
  const activeHostId = useStore((s) => s.activeHostId)
  const notifications = useStore((s) => s.notifications)
  const runningSessions = useStore((s) => s.runningSessions)
  const customAgentTemplate = useStore((s) => s.customAgentTemplate)
  const blockLoadRevision = useHostBlockLoadRevision(host.id)
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)

  const displayed = displayedHostStatus(host, status)
  const connected = displayed === 'connected'

  const isSelectedRow = (r: Row) =>
    activeHostId === r.hostId && sessionsByHost.get(r.hostId)?.activeSessionId === r.session.id

  // The group shows this host's sessions plus those of its retired
  // daemon generations (older generations first — they hold the older
  // sessions). Rows carry their own host id so selection, rename, and
  // kill route to the daemon that actually owns the session.
  const members = useMemo(() => {
    const retired = [...allHosts.values()]
      .filter((h) => h.retired?.parent === host.id)
      .sort((a, b) => (a.retired?.socket ?? '').localeCompare(b.retired?.socket ?? ''))
    return [...retired, host]
  }, [allHosts, host])

  const rows = useMemo(
    () =>
      members.flatMap((m) =>
        buildRows(m.id, sessionsByHost.get(m.id), runningSessions, customAgentTemplate),
      ),
    [members, sessionsByHost, runningSessions, customAgentTemplate, blockLoadRevision],
  )
  const unreadSessionIds = useMemo(() => {
    const set = new Set<string>()
    for (const m of members) {
      for (const sid of notificationSessionIds(notifications, m.id)) set.add(`${m.id}::${sid}`)
    }
    return set
  }, [notifications, members])
  const groups = useMemo(() => {
    const all = groupRows(rows)
    if (!filter) return all
    return all
      .map((g) => ({
        ...g,
        rows: g.rows.filter((r) =>
          `${r.title} ${r.detail} ${r.dir} ${g.label}`.toLowerCase().includes(filter),
        ),
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

  // Localhost has no transport to drop — its helmd is the app's own —
  // so connect/disconnect only apply to remotes.
  const isRemote = host.port !== 0
  const items: Array<ContextMenuItem | 'separator'> = []
  if (!isRemote || connected) {
    items.push({
      id: 'new-session',
      label: 'New session',
      icon: '+',
      onClick: () => void openSession(host.id),
    })
  }
  if (isRemote) {
    if (items.length) items.push('separator')
    if (connected || displayed === 'connecting') {
      // Drops the SSH transport and clears this host's notifications; the
      // remote helmd keeps running, so reconnecting finds the sessions
      // exactly as they were.
      items.push({
        id: 'disconnect',
        label: 'Disconnect',
        icon: '⏏',
        onClick: () => void commands.hostDisconnect(host.id),
      })
    } else {
      items.push({
        id: 'connect',
        label: 'Connect',
        icon: '⇄',
        onClick: () => void activateAndConnectHost(host.id).catch(() => {}),
      })
    }
  }
  if (onEdit) items.push({ id: 'edit', label: 'Edit host…', icon: 'A', onClick: onEdit })
  if (onDelete) {
    items.push('separator')
    items.push({ id: 'delete', label: 'Delete host', icon: '×', destructive: true, onClick: onDelete })
  }

  // Emphasis marks where you are, not merely what's connected: the host
  // and project holding the selected session step up a tier, everything
  // else recedes.
  const holdsSelection = groups.some((g) => g.rows.some(isSelectedRow))
  const hostTone = !connected
    ? 'text-text-disabled'
    : holdsSelection
      ? 'text-text-secondary'
      : 'text-text-tertiary'

  return (
    <div className="flex flex-col gap-px pt-3 first:pt-1">
      {/* With a single host there's nothing to distinguish, so the line
          is pure noise — the projects below are the real structure. It
          comes back the moment a second host exists, or if this one has
          something to report (connecting, offline, an error). */}
      {(!soleHost || statusText) && (
        <div
          role="button"
          onClick={() => {
            if (connected || displayed === 'connecting') useStore.getState().setActiveHost(host.id)
            else void activateAndConnectHost(host.id).catch(() => {})
          }}
          onContextMenu={(e) => {
            e.preventDefault()
            setMenu({ x: e.clientX, y: e.clientY })
          }}
          className="group/host flex h-6 select-none items-center gap-1.5 px-2"
          title={connected ? host.name : `${host.name} — click to connect`}
        >
          <MachineIcon
            size={12}
            className={`shrink-0 ${hostTone}`}
          />
          <span className={`truncate text-[11px] font-medium ${hostTone}`}>{host.name}</span>
          <span className="flex-1" />
          {statusText && (
            <span className={`text-[10px] ${displayed === 'error' ? 'text-status-error' : 'text-text-disabled'}`}>
              {statusText}
            </span>
          )}
          <button
            type="button"
            aria-label={`${host.name} options`}
            title="Host options"
            onClick={(e) => {
              e.stopPropagation()
              const r = e.currentTarget.getBoundingClientRect()
              setMenu({ x: r.right, y: r.bottom + 2 })
            }}
            className="-mr-1 flex size-5 shrink-0 items-center justify-center rounded text-text-tertiary opacity-0 hover:bg-[var(--stroke-default)] hover:text-text-primary focus-visible:opacity-100 group-hover/host:opacity-100"
          >
            <MoreVerticalIcon size={13} />
          </button>
        </div>
      )}
      {groups.map((g, i) => (
        <div key={g.key || '\0unknown'} className={`flex flex-col gap-1 ${i > 0 ? 'pt-2' : ''}`}>
          {g.label && (
            // The project header: just the folder path, as quiet as the
            // host line above it. The branch lives in each card's hover.
            <div className="flex h-[20px] items-center px-2" title={g.key}>
              <span
                className={`helm-truncate-start min-w-0 font-mono text-[11px] ${
                  g.rows.some(isSelectedRow) ? 'text-text-secondary' : 'text-text-tertiary'
                }`}
              >
                <span>{g.label}</span>
              </span>
            </div>
          )}
          {g.rows.map((r) => (
            <SessionRow
              key={r.key}
              kind={r.kind}
              running={r.running}
              title={r.title}
              detail={r.detail}
              dir={r.dir}
              branch={r.branch}
              unread={unreadSessionIds.has(`${r.hostId}::${r.session.id}`)}
              selected={isSelectedRow(r)}
              onClick={() => selectSession(r.hostId, r.session.id)}
              onRename={(name) => void commands.sessionRename(r.hostId, r.session.id, name)}
              onKill={() => killSession(r.hostId, r.session)}
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
  /** The host entry that owns the session — the parent host, or one of
   * its retired daemon generations. */
  hostId: HostId
  session: Session
  kind: SessionKind
  running: boolean
  /** Line one: the command (running or last), agent prompt, or name. */
  title: string
  /** Full command shown in hover details when the session is renamed. */
  detail: string
  /** Working directory (home-relative) — the hover tooltip and filter. */
  dir: string
  /** Git branch shown in the hover details. */
  branch: string
  /** Grouping inputs (see sidebarGroups.ts). */
  root: string
  cwd: string
}

/** The default, un-renamed session name (a shell program or a number). */
const DEFAULT_NAME = /^(zsh|bash|fish|sh|-?\w*sh|\d+|session \d+)$/i

/** Every session on the host in creation order. Grouping by project
 * happens on top of this and never reorders it. Labels come from what
 * the session is doing — the running command, the agent's prompt — and where
 * it is *within its project*, since the group header already says
 * which project. */
function buildRows(
  hostId: HostId,
  hs: HostSessions | undefined,
  runningSessions: Map<string, RunningSession>,
  customAgentTemplate: string,
): Row[] {
  if (!hs) return []
  const out: Row[] = []
  for (const session of sortById(hs.sessions.values())) {
      const run = runningSessions.get(`${hostId}::${session.id}`)
      const program = run ? commandName(run.command) : session.command || null
      const kind: SessionKind = run
        ? run.agentName !== null ? 'agent' : 'shell'
        : isAgentCommand(program, customAgentTemplate) ? 'agent' : 'shell'
      const root = session.root
      const cwd = session.cwd
      // The working directory isn't on the card — it's the hover tooltip.
      const dir = homeRelative(cwd)
      const renamed = !DEFAULT_NAME.test(session.name)

      const last = !run ? lastFinished(getSessionBlocks(hostId, session.id).blocks)?.cmdline : null
      const detail =
        kind === 'agent'
          ? run?.agentPrompt || agentPromptOf(session.command, customAgentTemplate) || program || 'agent'
          : run?.command?.trim() || last?.trim() || program || session.name
      const title = renamed ? session.name : detail

      out.push({
        key: `${hostId}::${session.id}`,
        hostId,
        session,
        kind,
        // An agent session is always live; a shell is running when a command
        // is in flight. The command's brightness follows this.
        running: kind === 'agent' || !!run,
        title,
        detail,
        dir,
        branch: session.branch,
        root,
        cwd,
      })
  }
  return out
}

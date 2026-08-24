/**
 * Dynamic action projections for sub-mode views.
 *
 * The static registry (`actions/index.ts`) holds verbs. Sub-modes
 * project store state into per-instance actions on demand: every
 * session and host becomes a selectable palette object. They share the same
 * Action shape so the palette renders + ranks them through the same
 * pipeline.
 *
 * Each projection optionally returns a `groups` map (action id →
 * header) so the renderer can drop section labels (`LOCALHOST · 2`,
 * `OFFLINE · 1`) between bucket boundaries without coupling grouping
 * into the Action type.
 */

import { commands } from '@lib/ipc'
import { activateAndConnectHost, deleteHost, selectSession } from '@lib/host'
import { useStore, sortById, type Session } from '@lib/store'
import {
  displayedHostStatus,
  STATUS_LABEL,
  STATUS_RANK,
  type HostDisplayStatus,
} from '@lib/host-status'
import type { Host } from '@bindings'
import type { Action } from './types'
import { killSession } from './session'

export interface GroupHeader {
  label: string
  count?: number
}

export interface SubModeResult {
  chip: string
  actions: Action[]
  /** Pre-computed group header per action id. The renderer emits a
   * `<SectionHeader>` the first time a new label appears in iteration
   * order, then the matching row. Absent ids skip the header. */
  groups?: Map<string, GroupHeader>
}

// ---------- #sessions ----------

function sessionSubActions(host: Host, session: Session): Action[] {
  return [
    {
      id: `session.${host.id}.${session.id}.jump`,
      kind: 'action',
      label: 'Jump to session',
      icon: '⏵',
      run: () => selectSession(host.id, session.id),
    },
    {
      id: `session.${host.id}.${session.id}.kill`,
      kind: 'action',
      label: 'Kill session',
      icon: '×',
      destructive: true,
      run: () => killSession(host.id, session),
    },
  ]
}

export function sessionsAsActions(): SubModeResult {
  const state = useStore.getState()
  const actions: Action[] = []
  for (const host of state.hosts.values()) {
    const hostSessions = state.sessions.get(host.id)
    if (!hostSessions) continue
    for (const session of sortById(hostSessions.sessions.values())) {
      actions.push({
        id: `session.${host.id}.${session.id}`,
        kind: 'session',
        label: session.name,
        sublabel: `· ${host.name}`,
        icon: '▢',
        run: () => selectSession(host.id, session.id),
        subActions: () => sessionSubActions(host, session),
      })
    }
  }
  return { chip: '#sessions', actions }
}

// ---------- $hosts ----------

function hostSubActions(host: Host, display: HostDisplayStatus): Action[] {
  const out: Action[] = []
  if (host.port !== 0) {
    if (display === 'connected' || display === 'connecting') {
      out.push({
        id: `host.${host.id}.disconnect`,
        kind: 'action',
        label: 'Disconnect',
        icon: '⏏',
        run: () => {
          void commands.hostDisconnect(host.id)
        },
      })
    } else {
      out.push({
        id: `host.${host.id}.connect`,
        kind: 'action',
        label: 'Connect',
        icon: '⏵',
        run: () => void activateAndConnectHost(host.id).catch(() => {}),
      })
    }
  }
  // Localhost (port 0) can't be removed from the registry.
  if (host.port !== 0) {
    out.push({
      id: `host.${host.id}.delete`,
      kind: 'action',
      label: 'Delete host',
      icon: '×',
      destructive: true,
      run: () => void deleteHost(host),
    })
  }
  return out
}

export function hostsAsActions(): SubModeResult {
  const state = useStore.getState()
  const actions: Action[] = []
  const groups = new Map<string, GroupHeader>()

  // Sort by status bucket, then by name within bucket. Counts per
  // bucket are computed in the same pass so each row's group header
  // can carry an accurate `count` without a second walk.
  const sorted = [...state.hosts.values()]
    .map((h) => {
      const status = state.statuses.get(h.id)
      return { host: h, display: displayedHostStatus(h, status) }
    })
    .sort((a, b) => {
      const r = STATUS_RANK[a.display] - STATUS_RANK[b.display]
      return r !== 0 ? r : a.host.name.localeCompare(b.host.name)
    })

  const counts = new Map<HostDisplayStatus, number>()
  for (const { display } of sorted) {
    counts.set(display, (counts.get(display) ?? 0) + 1)
  }
  const headerByDisplay = new Map<HostDisplayStatus, GroupHeader>()
  for (const [display, count] of counts) {
    headerByDisplay.set(display, { label: STATUS_LABEL[display], count })
  }

  for (const { host, display } of sorted) {
    const sublabel = host.port === 0 ? `· localhost` : `· ssh ${host.user}@${host.hostname}`
    const id = `host.${host.id}`
    actions.push({
      id,
      kind: 'host',
      label: host.name,
      sublabel,
      icon: '●',
      run: () => void activateAndConnectHost(host.id).catch(() => {}),
      subActions: () => hostSubActions(host, display),
    })
    const header = headerByDisplay.get(display)
    if (header) groups.set(id, header)
  }

  return { chip: '$hosts', actions, groups }
}

// ---------- dispatcher ----------

export type Sigil = '#' | '$'

/** Resolve a sigil to the matching projection. */
export function resolveSigil(sigil: Sigil): SubModeResult {
  switch (sigil) {
    case '#':
      return sessionsAsActions()
    case '$':
      return hostsAsActions()
  }
}

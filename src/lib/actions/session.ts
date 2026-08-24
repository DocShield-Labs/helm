import type { HostId } from '@bindings'
import { selectSession } from '@lib/host'
import { commands } from '@lib/ipc'
import { sortById, useStore, type HostSessions, type Session } from '@lib/store'
import type { Action } from './types'

export function activeHostId(): HostId | null {
  return useStore.getState().activeHostId
}

export function activeSession(): Session | undefined {
  const state = useStore.getState()
  const hostId = state.activeHostId
  if (!hostId) return undefined
  const hostSessions = state.sessions.get(hostId)
  if (!hostSessions?.activeSessionId) return undefined
  return hostSessions.sessions.get(hostSessions.activeSessionId)
}

function inheritedCwd(hostSessions: HostSessions | undefined): string | null {
  if (!hostSessions?.activeSessionId) return null
  return hostSessions.sessions.get(hostSessions.activeSessionId)?.cwd || null
}

function canOpenSession(state: ReturnType<typeof useStore.getState>, hostId: HostId | null): hostId is HostId {
  if (!hostId) return false
  const host = state.hosts.get(hostId)
  if (!host) return false
  const status = state.statuses.get(hostId)
  return host.port === 0 || status === 'connected' || status === 'idle'
}

function reportOpenError(hostId: HostId, message: string): void {
  useStore.getState().pushToast({
    id: `new-session-error::${hostId}`,
    message,
    durationMs: 8_000,
  })
}

export async function openSession(targetHostId?: HostId): Promise<void> {
  const initial = useStore.getState()
  const hostId = targetHostId ?? initial.activeHostId
  if (!canOpenSession(initial, hostId)) {
    if (hostId) reportOpenError(hostId, 'Connect to this host before opening a session.')
    return
  }
  const cwd = hostId === initial.activeHostId ? inheritedCwd(initial.sessions.get(hostId)) : null
  if (targetHostId && targetHostId !== initial.activeHostId) initial.setActiveHost(targetHostId)
  try {
    const result = await commands.sessionNew(hostId, null, cwd, null)
    if (result.status !== 'ok') {
      reportOpenError(hostId, `Couldn't open session: ${result.error}`)
      return
    }
    if (useStore.getState().activeHostId === hostId) {
      selectSession(hostId, result.data.session_id)
    }
  } catch (error) {
    reportOpenError(hostId, `Couldn't open session: ${String(error)}`)
  }
}

export function neighbourSessionId(
  hostSessions: HostSessions,
  currentId: string | null,
  direction: 1 | -1,
): string | undefined {
  const sessions = sortById(hostSessions.sessions.values())
  if (sessions.length < 2 || !currentId) return undefined
  const index = sessions.findIndex((session) => session.id === currentId)
  if (index < 0) return undefined
  return sessions[(index + direction + sessions.length) % sessions.length].id
}

export function killSession(hostId: HostId, session: Session): void {
  const state = useStore.getState()
  const key = `${hostId}::${session.id}`
  state.optimisticRemoveSession(hostId, session.id)
  state.pushToast({
    id: `kill-session::${key}`,
    message: `Killed session "${session.name}"`,
    durationMs: 5_000,
    deferredAction: () => {
      void commands.sessionKill(hostId, session.id)
      useStore.getState().commitPendingSessionKill(key)
    },
    action: {
      label: 'Undo',
      onClick: () => useStore.getState().restorePendingSessionKill(key),
    },
  })
}

function stepSession(direction: 1 | -1): void {
  const state = useStore.getState()
  const hostId = state.activeHostId
  if (!hostId) return
  const hostSessions = state.sessions.get(hostId)
  if (!hostSessions) return
  const next = neighbourSessionId(hostSessions, hostSessions.activeSessionId, direction)
  if (next) selectSession(hostId, next)
}

export const sessionActions: Action[] = [
  {
    id: 'session.new',
    kind: 'action',
    label: 'New session',
    icon: '▢',
    keybinding: 'Cmd+T',
    canRun: () => {
      const state = useStore.getState()
      return canOpenSession(state, state.activeHostId)
    },
    run: () => void openSession(),
  },
  {
    id: 'session.kill',
    kind: 'action',
    label: 'Kill session',
    icon: '×',
    keybinding: 'Cmd+W',
    destructive: true,
    canRun: () => activeHostId() !== null && activeSession() !== undefined,
    run: () => {
      const hostId = activeHostId()
      const session = activeSession()
      if (hostId && session) killSession(hostId, session)
    },
  },
  {
    id: 'session.next',
    kind: 'action',
    label: 'Next session',
    icon: '⏵',
    keybinding: ['Cmd+]', 'Cmd+ArrowRight'],
    canRun: () => activeHostId() !== null,
    run: () => stepSession(+1),
  },
  {
    id: 'session.previous',
    kind: 'action',
    label: 'Previous session',
    icon: '⏴',
    keybinding: ['Cmd+[', 'Cmd+ArrowLeft'],
    canRun: () => activeHostId() !== null,
    run: () => stepSession(-1),
  },
]

export function activeSessionSnapshot(): { hostId: HostId; session: Session } | null {
  const hostId = activeHostId()
  const session = activeSession()
  return hostId && session ? { hostId, session } : null
}

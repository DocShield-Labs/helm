/**
 * Composer mode, per session.
 *
 * Auto mode follows the session: a shell session composes for the shell, an
 * agent session composes for the agent. The user can override (the
 * Terminal | Agent control, ⌘I); the override is remembered with the
 * session kind it was made under so it lapses when the session changes
 * (Claude exits → back to the shell composer).
 */

import { useSyncExternalStore } from 'react'
import type { HostId } from '@bindings'
import type { SessionKind } from './sessionState'

export type ComposerMode = 'terminal' | 'agent'

interface Override {
  mode: ComposerMode
  kind: SessionKind
}

const overrides = new Map<string, Override>()
/** What each mounted session is currently showing — lets the ⌘I action
 * flip relative to the effective mode without knowing how it was derived. */
const effective = new Map<string, { mode: ComposerMode; kind: SessionKind }>()
const subs = new Set<() => void>()
const key = (h: HostId, p: string) => `${h}::${p}`

function notify() {
  for (const cb of subs) cb()
}

export function effectiveMode(hostId: HostId, sessionId: string, kind: SessionKind): ComposerMode {
  const o = overrides.get(key(hostId, sessionId))
  if (o && o.kind === kind) return o.mode
  return kind === 'agent' ? 'agent' : 'terminal'
}

export function useComposerMode(hostId: HostId, sessionId: string, kind: SessionKind): ComposerMode {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb)
      return () => subs.delete(cb)
    },
    () => effectiveMode(hostId, sessionId, kind),
    () => (kind === 'agent' ? 'agent' : 'terminal'),
  )
}

export function setComposerMode(hostId: HostId, sessionId: string, kind: SessionKind, mode: ComposerMode): void {
  overrides.set(key(hostId, sessionId), { mode, kind })
  notify()
}

export function reportEffective(hostId: HostId, sessionId: string, kind: SessionKind, mode: ComposerMode): void {
  effective.set(key(hostId, sessionId), { kind, mode })
}

export function toggleComposerMode(hostId: HostId, sessionId: string): void {
  const cur = effective.get(key(hostId, sessionId))
  if (!cur) return
  setComposerMode(hostId, sessionId, cur.kind, cur.mode === 'agent' ? 'terminal' : 'agent')
}

export function forgetSession(hostId: HostId, sessionId: string): void {
  overrides.delete(key(hostId, sessionId))
  effective.delete(key(hostId, sessionId))
}

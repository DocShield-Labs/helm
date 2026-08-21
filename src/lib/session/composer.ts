/**
 * Composer mode, per pane.
 *
 * Auto mode follows the pane: a shell pane composes for the shell, an
 * agent pane composes for the agent. The user can override (the
 * Terminal | Agent control, ⌘I); the override is remembered with the
 * pane kind it was made under so it lapses when the pane changes
 * (Claude exits → back to the shell composer).
 */

import { useSyncExternalStore } from 'react'
import type { HostId } from '@bindings'
import type { PaneKind } from './paneState'

export type ComposerMode = 'terminal' | 'agent'

interface Override {
  mode: ComposerMode
  kind: PaneKind
}

const overrides = new Map<string, Override>()
/** What each mounted pane is currently showing — lets the ⌘I action
 * flip relative to the effective mode without knowing how it was derived. */
const effective = new Map<string, { mode: ComposerMode; kind: PaneKind }>()
const subs = new Set<() => void>()
const key = (h: HostId, p: string) => `${h}::${p}`

function notify() {
  for (const cb of subs) cb()
}

export function effectiveMode(hostId: HostId, paneId: string, kind: PaneKind): ComposerMode {
  const o = overrides.get(key(hostId, paneId))
  if (o && o.kind === kind) return o.mode
  return kind === 'agent' ? 'agent' : 'terminal'
}

export function useComposerMode(hostId: HostId, paneId: string, kind: PaneKind): ComposerMode {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb)
      return () => subs.delete(cb)
    },
    () => effectiveMode(hostId, paneId, kind),
    () => (kind === 'agent' ? 'agent' : 'terminal'),
  )
}

export function setComposerMode(hostId: HostId, paneId: string, kind: PaneKind, mode: ComposerMode): void {
  overrides.set(key(hostId, paneId), { mode, kind })
  notify()
}

export function reportEffective(hostId: HostId, paneId: string, kind: PaneKind, mode: ComposerMode): void {
  effective.set(key(hostId, paneId), { kind, mode })
}

export function toggleComposerMode(hostId: HostId, paneId: string): void {
  const cur = effective.get(key(hostId, paneId))
  if (!cur) return
  setComposerMode(hostId, paneId, cur.kind, cur.mode === 'agent' ? 'terminal' : 'agent')
}

export function forgetPane(hostId: HostId, paneId: string): void {
  overrides.delete(key(hostId, paneId))
  effective.delete(key(hostId, paneId))
}

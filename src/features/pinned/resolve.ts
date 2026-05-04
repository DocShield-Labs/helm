/**
 * Pin resolution + phase helpers.
 *
 * Shared between the expanded PinnedSection and the collapsed-rail
 * pinned variant. Centralizing here means one source of truth for
 * "is this pin reachable / loading / stale".
 */

import type { Host, HostStatus, Notification } from '@bindings'
import type { PinnedWindow, TmuxWindow, TmuxWorkspace } from '@lib/store'
import type { StatusDotState } from '@ui'

export type PinPhase = 'normal' | 'loading' | 'stale' | 'offline'

export interface ResolvedPin {
  pin: PinnedWindow
  host: Host | undefined
  workspace: TmuxWorkspace | undefined
  window: TmuxWindow | undefined
  /** True once the host's tree has been refetched at least once.
   * Without this, every pin would look stale on app boot until the
   * first tree-fetch lands. */
  treeLoaded: boolean
}

export function resolvePin(
  pin: PinnedWindow,
  hosts: Map<string, Host>,
  sessions: Map<string, { workspaces: Map<string, TmuxWorkspace>; activeWorkspaceId: string | null } | undefined>,
): ResolvedPin {
  const host = hosts.get(pin.hostId)
  const hs = sessions.get(pin.hostId)
  // A connected host with zero workspaces would also resolve to stale —
  // technically correct, since there's nothing for the pin to point to.
  const treeLoaded = !!hs && hs.workspaces.size > 0
  let workspace: TmuxWorkspace | undefined
  if (hs) {
    for (const w of hs.workspaces.values()) {
      if (w.name === pin.workspaceName) { workspace = w; break }
    }
  }
  const window = workspace?.windows.get(pin.windowId)
  return { pin, host, workspace, window, treeLoaded }
}

/** Decides what visual state to render. We only call a pin stale when we
 * have evidence — host connected and tree fetched — that the
 * workspace/window aren't there. Without this guard, every pin would
 * read as stale on app launch. */
export function phaseFor(
  host: Host | undefined,
  status: HostStatus | undefined,
  treeLoaded: boolean,
  hasWorkspace: boolean,
  hasWindow: boolean,
): PinPhase {
  if (!host) return 'stale'
  const isLocal = host.port === 0
  // Localhost reads as connected even before the boot ping lands —
  // otherwise the rail flashes "offline" for a tick on launch.
  const connected =
    status === 'connected' || status === 'idle' || (isLocal && status === undefined)
  if (!connected) return 'offline'
  if (!treeLoaded) return 'loading'
  if (!hasWorkspace || !hasWindow) return 'stale'
  return 'normal'
}

export function pinDotState(
  host: Host | undefined,
  status: HostStatus | undefined,
  phase: PinPhase,
): StatusDotState {
  if (!host) return 'disconnected'
  if (phase === 'stale') return 'error'
  if (phase === 'loading') return 'connecting'
  if (phase === 'offline') {
    if (status === 'connecting') return 'connecting'
    if (status === 'error') return 'error'
    return 'disconnected'
  }
  return 'connected'
}

export function rollupActivity(notifs: Notification[]): 'failed' | 'attention' | 'running' {
  let hasFailed = false
  let hasBell = false
  for (const n of notifs) {
    if (
      n.kind.kind === 'command_done' &&
      n.kind.exit_code !== null &&
      n.kind.exit_code !== undefined &&
      n.kind.exit_code !== 0
    ) hasFailed = true
    if (n.kind.kind === 'bell') hasBell = true
  }
  if (hasFailed) return 'failed'
  if (hasBell) return 'attention'
  return 'running'
}

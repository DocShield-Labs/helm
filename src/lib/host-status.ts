/**
 * Shared host-status helpers.
 *
 * `hostRowStatus` collapses the raw HostStatus + detached reason into the
 * 4-state visual vocabulary the dot uses (connected / connecting /
 * disconnected / error). `displayedHostStatus` is the localhost-aware
 * wrapper: localhost stays green for the steady state since
 * network-style "disconnected" is meaningless for the local machine,
 * but real trouble (Reconnecting, Error) still surfaces so the user
 * has a path to recover.
 */

import type { Host, HostStatus } from '@bindings'

export type HostDisplayStatus = 'connected' | 'connecting' | 'disconnected' | 'error'

export function hostRowStatus(
  status: HostStatus,
  detached: string | null,
): HostDisplayStatus {
  if (detached) return 'disconnected'
  if (status === 'connected' || status === 'idle') return 'connected'
  // `reconnecting` collapses to the amber `connecting` color — the
  // overlay tells the user *why*; the dot just signals "in flight".
  if (status === 'connecting' || status === 'reconnecting') return 'connecting'
  if (status === 'error') return 'error'
  return 'disconnected'
}

export function displayedHostStatus(
  host: Host,
  status: HostStatus | undefined,
  detached: string | null,
): HostDisplayStatus {
  // Localhost: the dot stays green when the tmux client is healthy
  // (or we haven't heard otherwise yet). Real trouble (Reconnecting
  // because tmux died, Error because it can't be brought up) falls
  // through to the regular status mapping so the user can see + act.
  if (host.port === 0) {
    if (status === undefined || status === 'connected' || status === 'idle') {
      return 'connected'
    }
    return hostRowStatus(status, detached)
  }
  return hostRowStatus(status ?? 'disconnected', detached)
}

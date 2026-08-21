/**
 * Cross-host scrollback search (the `/` mode of the command palette).
 *
 * Runs `session_search` on every connected host — the daemon greps its
 * ring buffers server-side and returns matches with exact seq anchors
 * — and turns each hit into a palette action that jumps to the window
 * and scrolls to the block. Results are cached per term and fetched
 * with a short debounce; `useSearchVersion` lets the palette re-render
 * when a fetch lands.
 */

import { useSyncExternalStore } from 'react'
import { commands } from '@lib/ipc'
import { selectWindow } from '@lib/host'
import { locatePane, useStore } from '@lib/store'
import { requestJump } from '@lib/session/blocks'
import type { SearchHit } from '@bindings'
import type { Action } from './types'

const MIN_TERM = 2
const MAX_PER_HOST = 40
const DEBOUNCE_MS = 150

let version = 0
const listeners = new Set<() => void>()
const cache = new Map<string, Action[]>()
const inflight = new Set<string>()
let timer: number | null = null

export function useSearchVersion(): number {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb)
      return () => listeners.delete(cb)
    },
    () => version,
    () => version,
  )
}

/** Synchronous view of the results for `term` (possibly still empty
 * while the fetch is in flight). Kicks off the fetch as a side effect. */
export function buildSearchActions(term: string): Action[] {
  const t = term.trim()
  if (t.length < MIN_TERM) return []
  const hit = cache.get(t)
  if (!hit && !inflight.has(t)) schedule(t)
  return hit ?? []
}

function schedule(t: string) {
  if (timer !== null) window.clearTimeout(timer)
  timer = window.setTimeout(() => {
    timer = null
    void run(t)
  }, DEBOUNCE_MS)
}

async function run(t: string) {
  if (cache.has(t) || inflight.has(t)) return
  inflight.add(t)
  try {
    const s = useStore.getState()
    const hostIds = [...s.hosts.keys()].filter((id) => {
      const st = s.statuses.get(id)
      return st === 'connected' || st === 'idle'
    })
    const perHost = await Promise.all(
      hostIds.map(async (hostId) => {
        const res = await commands.sessionSearch(hostId, t, false, false, null, null, MAX_PER_HOST)
        return { hostId, hits: res.status === 'ok' ? res.data.matches : ([] as SearchHit[]) }
      }),
    )
    const out: Action[] = []
    const state = useStore.getState()
    for (const { hostId, hits } of perHost) {
      const host = state.hosts.get(hostId)
      const hs = state.sessions.get(hostId)
      if (!host || !hs) continue
      for (const hit of hits) {
        const located = locatePane(hs, hit.pane_id)
        if (!located) continue
        const { workspace, pane, window: win } = located
        const snippet = hit.line_text.trim().slice(0, 120)
        out.push({
          id: `search.${hostId}.${hit.pane_id}.${hit.line_seq}`,
          kind: 'window',
          label: snippet || '(blank match)',
          sublabel: `${host.name} · ${workspace.name} · ${win?.name ?? ''}`,
          icon: '⌕',
          run: () => {
            selectWindow(hostId, workspace.id, pane.windowId)
            // The pane consumes this once it's mounted and hydrated.
            requestJump({ hostId, paneId: hit.pane_id, blockId: hit.block_id, lineSeq: hit.line_seq })
          },
        })
      }
    }
    cache.set(t, out)
    version++
    for (const cb of listeners) cb()
  } finally {
    inflight.delete(t)
  }
}

/** Drop cached results (called when the palette closes so a reopened
 * search reflects new output). */
export function clearSearchCache(): void {
  cache.clear()
}

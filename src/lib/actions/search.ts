/**
 * Cross-window search (the `/` mode of the command palette).
 *
 * Greps the pre-hydrated pane captures across *every* window on *every*
 * connected host for the typed term and turns each hit into a palette
 * action that jumps to the originating window. This is the "where did I
 * see that?" companion to the per-pane Cmd+F find.
 *
 * Scope/limits (v1):
 *   - Searches `paneCaptures` (the snapshots taken on connect / refetch),
 *     not live xterm buffers — so the active pane's most recent lines may
 *     lag. The active pane is the one you're already looking at; Cmd+F
 *     covers it. Other windows' captures are fresh enough to locate.
 *   - Capped per-pane and overall so a broad term can't flood the list.
 *   - Jumps to the window; it does not yet scroll to the exact line.
 */

import { useStore } from '@lib/store'
import { selectWorkspace } from '@lib/host'
import { commands } from '@lib/ipc'
import type { Action } from './types'

// Strip CSI/SGR sequences and OSC strings so we match (and show) clean
// text rather than the escape-laden bytes `capture-pane -e` returns.
const ANSI =
  // eslint-disable-next-line no-control-regex
  /[\x1b\x9b][[\]()#;?]*(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-ntqry=><~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g

function stripAnsi(s: string): string {
  return s.replace(ANSI, '')
}

const MIN_TERM = 2
const MAX_RESULTS = 60
const MAX_PER_PANE = 5

/** Build palette actions for every line matching `term` across all
 * connected hosts' captured panes. Case-insensitive substring match.
 * Returns [] for terms shorter than MIN_TERM. */
export function buildSearchActions(term: string): Action[] {
  const trimmed = term.trim()
  if (trimmed.length < MIN_TERM) return []
  const needle = trimmed.toLowerCase()

  const { sessions, paneCaptures, hosts } = useStore.getState()
  const out: Action[] = []

  for (const [hostId, hs] of sessions) {
    const hostName = hosts.get(hostId)?.name ?? '?'
    for (const ws of hs.workspaces.values()) {
      for (const win of ws.windows.values()) {
        for (const [paneId, pane] of ws.panes) {
          if (pane.windowId !== win.id) continue
          const cap = paneCaptures.get(`${hostId}::${paneId}`)
          if (!cap || cap.data.length === 0) continue
          const lines = stripAnsi(cap.data).split('\n')
          let perPane = 0
          for (let li = 0; li < lines.length; li++) {
            if (!lines[li].toLowerCase().includes(needle)) continue
            const snippet = lines[li].trim().slice(0, 120)
            out.push({
              id: `search.${hostId}.${win.id}.${paneId}.${li}`,
              kind: 'window',
              label: snippet || '(blank match)',
              sublabel: `${hostName} · ${ws.name} · ${win.name}`,
              icon: '⌕',
              run: () => {
                const store = useStore.getState()
                store.setActiveHost(hostId)
                store.setActiveWindow(hostId, ws.id, win.id)
                void selectWorkspace(hostId, ws.id)
                void commands.tmuxSelectWindow(hostId, win.id)
              },
            })
            if (++perPane >= MAX_PER_PANE) break
            if (out.length >= MAX_RESULTS) return out
          }
        }
      }
    }
  }
  return out
}

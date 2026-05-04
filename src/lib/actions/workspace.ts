/**
 * Workspace-scoped actions and the helpers that back them.
 *
 * `createWorkspace` and `killWorkspace` are exported because the sidebar
 * also wires them onto its `+` button and right-click menu — they aren't
 * keymap-only verbs.
 */

import { commands } from '@lib/ipc'
import { connectHost, selectWorkspace } from '@lib/host'
import { useStore, type TmuxWorkspace } from '@lib/store'
import type { Action } from './types'
import type { HostId } from '@bindings'

/** Pick the lowest unused `workspace N` slot, connect the host if it
 * isn't already, and create the session. The bootstrap path passes the
 * intended name through so a fresh server doesn't get a stray `main`
 * session before our `workspace 1` shows up. */
export async function createWorkspace(hostId: HostId): Promise<void> {
  const state = useStore.getState()
  const hs = state.sessions.get(hostId)
  const used = new Set<number>()
  if (hs) {
    for (const w of hs.workspaces.values()) {
      const m = w.name.match(/^workspace (\d+)$/)
      if (m) used.add(parseInt(m[1], 10))
    }
  }
  let n = 1
  while (used.has(n)) n++
  const name = `workspace ${n}`

  const status = state.statuses.get(hostId)
  if (status !== 'connected' && status !== 'idle') {
    try {
      await connectHost(hostId, name)
    } catch (e) {
      console.error('connect failed:', e)
      return
    }
  }

  const post = useStore.getState().sessions.get(hostId)
  let workspaceId: string | undefined
  if (post) {
    for (const w of post.workspaces.values()) {
      if (w.name === name) {
        workspaceId = w.id
        break
      }
    }
  }
  if (!workspaceId) {
    const res = await commands.tmuxNewSession(hostId, name)
    if (res.status !== 'ok') {
      console.error('new-session failed:', res.error)
      return
    }
    workspaceId = res.data
  }
  await selectWorkspace(hostId, workspaceId)
}

/** Schedule a workspace kill with a 5s undo window. The toast carries
 * the actual kill as its deferred action; pressing the toast's Undo
 * button dismisses without firing. Cmd+Z (global undo handler) reaches
 * into the same toast queue. */
export async function killWorkspace(hostId: HostId, workspace: TmuxWorkspace): Promise<void> {
  const toastId = `kill-workspace::${hostId}::${workspace.id}`
  const { pushToast } = useStore.getState()
  pushToast({
    id: toastId,
    message: `Killing workspace "${workspace.name}"`,
    durationMs: 5_000,
    deferredAction: () => {
      void commands.tmuxKillSession(hostId, workspace.id)
    },
    action: { label: 'Undo', onClick: () => {} },
  })
}

/** Read-only helpers — used by the action `run` closures and by
 * dynamic projections in the palette. Centralised here so the
 * "what is the active workspace" answer is consistent. */
export function activeHostId(): HostId | null {
  return useStore.getState().activeHostId
}

export function activeWorkspace(): TmuxWorkspace | undefined {
  const s = useStore.getState()
  if (!s.activeHostId) return undefined
  const hs = s.sessions.get(s.activeHostId)
  if (!hs?.activeWorkspaceId) return undefined
  return hs.workspaces.get(hs.activeWorkspaceId)
}

export const workspaceActions: Action[] = [
  {
    id: 'workspace.new',
    kind: 'action',
    label: 'New workspace',
    icon: '◫',
    keybinding: 'Cmd+Shift+T',
    canRun: () => activeHostId() !== null,
    run: () => {
      const hostId = activeHostId()
      if (hostId) void createWorkspace(hostId)
    },
  },
  {
    id: 'workspace.kill',
    kind: 'action',
    label: 'Kill workspace',
    icon: '×',
    keybinding: 'Cmd+Shift+W',
    destructive: true,
    canRun: () => activeHostId() !== null && activeWorkspace() !== undefined,
    run: () => {
      const hostId = activeHostId()
      const ws = activeWorkspace()
      if (hostId && ws) void killWorkspace(hostId, ws)
    },
  },
]

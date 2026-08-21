/**
 * Workspace-scoped actions and the helpers that back them.
 *
 * `createWorkspace` and `killWorkspace` are exported because the sidebar
 * also wires them onto its `+` button and right-click menu — they aren't
 * keymap-only verbs.
 */

import { commands } from '@lib/ipc'
import { connectHost, selectWindow, selectWorkspace } from '@lib/host'
import { useStore, type TmuxWorkspace } from '@lib/store'
import type { Action } from './types'
import type { HostId } from '@bindings'

/** Pick the lowest unused `workspace N` slot, connect the host if it
 * isn't already, and create the workspace. The bootstrap path passes
 * the intended name through so a fresh daemon's default workspace IS
 * the one the user asked for. */
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
    const res = await commands.workspaceNew(hostId, name)
    if (res.status !== 'ok') {
      console.error('workspace_new failed:', res.error)
      return
    }
    if (res.data.window_id) {
      selectWindow(hostId, res.data.workspace_id, res.data.window_id)
      return
    }
    workspaceId = res.data.workspace_id
  }
  await selectWorkspace(hostId, workspaceId)
}

/** Folder-view's "+ window" path. Folder view hides workspaces, so
 * a new window can't pick a workspace from the UI — instead we route
 * every folder-view-created window into a single per-host workspace
 * named `folders`, lazily created on first use. When the user later
 * flips back to workspace view, those windows appear grouped under
 * that workspace, conveying "these were created in folder mode".
 *
 * Caveat: if the user renames the `folders` workspace from within
 * workspace view, this helper won't find it by name and will create
 * another. v1 accepts that — a follow-up could persist the workspace
 * id in localStorage if it matters in practice. */
export async function createFolderWindow(hostId: HostId): Promise<void> {
  const FOLDERS_NAME = 'folders'
  const state = useStore.getState()
  const hs = state.sessions.get(hostId)

  let workspaceId: string | undefined
  if (hs) {
    for (const w of hs.workspaces.values()) {
      if (w.name === FOLDERS_NAME) {
        workspaceId = w.id
        break
      }
    }
  }

  // A freshly created workspace comes with an initial window — that IS
  // the new window. Only a pre-existing workspace needs an explicit
  // `window_new`.
  const preExisting = workspaceId !== undefined

  if (!workspaceId) {
    // Same bootstrap path as createWorkspace — pass the intended name
    // to connectHost so a fresh server doesn't get a stray `main`
    // session before our `folders` shows up.
    const status = state.statuses.get(hostId)
    if (status !== 'connected' && status !== 'idle') {
      try {
        await connectHost(hostId, FOLDERS_NAME)
      } catch (e) {
        console.error('connect failed:', e)
        return
      }
    }
    const post = useStore.getState().sessions.get(hostId)
    if (post) {
      for (const w of post.workspaces.values()) {
        if (w.name === FOLDERS_NAME) {
          workspaceId = w.id
          break
        }
      }
    }
    if (!workspaceId) {
      const res = await commands.workspaceNew(hostId, FOLDERS_NAME)
      if (res.status !== 'ok') {
        console.error('workspace_new failed:', res.error)
        return
      }
      if (res.data.window_id) {
        selectWindow(hostId, res.data.workspace_id, res.data.window_id)
        return
      }
      workspaceId = res.data.workspace_id
    }
  }

  await selectWorkspace(hostId, workspaceId)
  if (preExisting) {
    const winRes = await commands.windowNew(hostId, workspaceId, null, null, null)
    if (winRes.status !== 'ok') {
      console.error('window_new failed:', winRes.error)
    } else if (winRes.data.window_id) {
      selectWindow(hostId, workspaceId, winRes.data.window_id)
    }
  }
}

/** Optimistic workspace kill with a 5s undo. Mirrors the `killWindow`
 * pattern in `window.ts` — the workspace (with its windows + panes) is
 * snapshotted into `pendingWorkspaceKills` and stripped from the live
 * sessions tree immediately so the sidebar collapses without waiting
 * for the deferred kill. The toast's Undo button (and the global
 * Cmd+Z handler) restores the snapshot; the deferred action fires the
 * real `workspace_kill` after the timer elapses. */
export async function killWorkspace(hostId: HostId, workspace: TmuxWorkspace): Promise<void> {
  const state = useStore.getState()
  const key = `${hostId}::${workspace.id}`
  const toastId = `kill-workspace::${key}`
  state.optimisticRemoveWorkspace(hostId, workspace.id)
  state.pushToast({
    id: toastId,
    message: `Killed workspace "${workspace.name}"`,
    durationMs: 5_000,
    deferredAction: () => {
      void commands.workspaceKill(hostId, workspace.id)
      useStore.getState().commitPendingWorkspaceKill(key)
    },
    action: {
      label: 'Undo',
      onClick: () => {
        useStore.getState().restorePendingWorkspaceKill(key)
      },
    },
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

/**
 * Window-scoped actions plus the small helpers behind window navigation.
 *
 * `cyclePinnedWindow` and `neighbourWindowId` are exported because the
 * Cmd+] / Cmd+[ next/prev verbs branch on whether the sidebar's Pinned
 * tab is active — same key, two behaviours, both selected from in here.
 */

import { commands } from '@lib/ipc'
import { selectWindow } from '@lib/host'
import {
  pinnedKey,
  selectedPane,
  sortById,
  useStore,
  type HostSessions,
  type TmuxWindow,
  type TmuxWorkspace,
} from '@lib/store'
import type { Action } from './types'
import type { HostId } from '@bindings'
import { activeHostId, activeWorkspace } from './workspace'

function activeWindow(): TmuxWindow | undefined {
  const ws = activeWorkspace()
  if (!ws) return undefined
  const list = sortById(ws.windows.values())
  return list.find((w) => w.active) ?? list[0]
}

function inheritedCwd(hostSessions: HostSessions | undefined): string | null {
  if (!hostSessions?.activeWorkspaceId) return null
  const ws = hostSessions.workspaces.get(hostSessions.activeWorkspaceId)
  if (!ws) return null
  const windows = sortById(ws.windows.values())
  const win = windows.find((window) => window.active) ?? windows[0]
  if (!win) return null
  const pane = selectedPane(ws, win.id)
  return pane?.cwd || null
}

function canOpenSession(
  state: ReturnType<typeof useStore.getState>,
  hostId: HostId | null,
): hostId is HostId {
  if (!hostId) return false
  const host = state.hosts.get(hostId)
  if (!host) return false
  const status = state.statuses.get(hostId)
  return host.port === 0 || status === 'connected' || status === 'idle'
}

function reportOpenSessionError(hostId: HostId, message: string): void {
  useStore.getState().pushToast({
    id: `new-session-error::${hostId}`,
    message,
    durationMs: 8_000,
  })
}

/** Open a session on the active host, inheriting the current session's
 * cwd. Killing the last window strips its (now empty) workspace from
 * the tree while the undo toast counts down, so there may be no
 * workspace to put this in — create one rather than making the user
 * wait out the timer. */
export async function openSession(targetHostId?: HostId): Promise<void> {
  const initial = useStore.getState()
  const hostId = targetHostId ?? initial.activeHostId
  if (!canOpenSession(initial, hostId)) {
    if (hostId) reportOpenSessionError(hostId, 'Connect to this host before opening a session.')
    return
  }

  const cwd = hostId === initial.activeHostId ? inheritedCwd(initial.sessions.get(hostId)) : null
  if (targetHostId && targetHostId !== initial.activeHostId) initial.setActiveHost(targetHostId)

  const hs = initial.sessions.get(hostId)
  let wsId = hs?.activeWorkspaceId ?? hs?.workspaces.keys().next().value ?? null
  try {
    if (!wsId) {
      const created = await commands.workspaceNew(hostId, null)
      if (created.status !== 'ok') {
        reportOpenSessionError(hostId, `Couldn't open session: ${created.error}`)
        return
      }
      if (created.data.window_id) {
        if (useStore.getState().activeHostId === hostId) {
          selectWindow(hostId, created.data.workspace_id, created.data.window_id)
        }
        return
      }
      wsId = created.data.workspace_id
    }

    const result = await commands.windowNew(hostId, wsId, null, cwd, null)
    if (result.status !== 'ok') {
      reportOpenSessionError(hostId, `Couldn't open session: ${result.error}`)
      return
    }
    if (result.data.window_id && useStore.getState().activeHostId === hostId) {
      selectWindow(hostId, wsId, result.data.window_id)
    }
  } catch (error) {
    reportOpenSessionError(hostId, `Couldn't open session: ${String(error)}`)
  }
}

export function neighbourWindowId(
  workspace: TmuxWorkspace,
  currentId: string | undefined,
  direction: 1 | -1,
): string | undefined {
  const list = sortById(workspace.windows.values())
  if (list.length < 2 || !currentId) return undefined
  const idx = list.findIndex((w) => w.id === currentId)
  if (idx < 0) return undefined
  return list[(idx + direction + list.length) % list.length].id
}

/** Pinned-tab variant: walk the user's working set across hosts,
 * skipping pins whose underlying window has gone stale. */
export async function cyclePinnedWindow(dir: 1 | -1): Promise<void> {
  const state = useStore.getState()
  const pins = state.pinnedWindows
  if (pins.length === 0) return

  const curIdx = pins.findIndex((p) => {
    if (p.hostId !== state.activeHostId) return false
    const hs = state.sessions.get(p.hostId)
    if (!hs) return false
    const ws = [...hs.workspaces.values()].find((w) => w.name === p.workspaceName)
    if (!ws || hs.activeWorkspaceId !== ws.id) return false
    const win = ws.windows.get(p.windowId)
    return !!win && win.active
  })

  const start = curIdx === -1 ? (dir > 0 ? 0 : pins.length - 1) : curIdx + dir
  for (let i = 0; i < pins.length; i++) {
    const idx = ((start + i * dir) % pins.length + pins.length) % pins.length
    const target = pins[idx]
    const hs = state.sessions.get(target.hostId)
    const ws = hs ? [...hs.workspaces.values()].find((w) => w.name === target.workspaceName) : undefined
    const win = ws?.windows.get(target.windowId)
    if (!ws || !win) continue

    selectWindow(target.hostId, ws.id, win.id)
    return
  }
}

/** Optimistic window kill with a 5s undo. Mirrors the kill-workspace
 * pattern in `workspace.ts` but also strips the window (and its panes)
 * from the local sessions tree immediately so the sidebar collapses
 * and the active window slides to a sibling. The toast's Undo button
 * (and the global Cmd+Z handler) restores the snapshot; the deferred
 * action fires the real `tmux_kill_window` after the timer elapses.
 *
 * The store filters `pendingWindowKills` out of every `setWorkspaces`
 * refetch in the meantime — without that, a `%window-add` from
 * elsewhere on the host would refetch the tree and resurrect the row,
 * breaking the optimistic illusion. */
export function killWindow(hostId: HostId, workspaceId: string, window: TmuxWindow): void {
  const state = useStore.getState()
  const key = `${hostId}::${window.id}`
  const toastId = `kill-window::${key}`
  state.optimisticRemoveWindow(hostId, workspaceId, window.id)
  state.pushToast({
    id: toastId,
    message: `Killed window "${window.name}"`,
    durationMs: 5_000,
    deferredAction: () => {
      void commands.windowKill(hostId, window.id)
      useStore.getState().commitPendingWindowKill(key)
    },
    action: {
      label: 'Undo',
      onClick: () => {
        useStore.getState().restorePendingWindowKill(key)
      },
    },
  })
}

/** Keymap dispatcher for next/prev — cycle within the user's pinned
 * working set if the active window is itself a pin, otherwise step
 * to the neighbour window inside the same workspace. The active
 * window's pin-membership is the natural signal now that the sidebar
 * has no Pinned/Hosts tab — the user is "in pinned mode" exactly when
 * they're sitting on a pinned row. */
async function stepWindow(direction: 1 | -1): Promise<void> {
  const state = useStore.getState()
  const hostId = state.activeHostId
  const ws = activeWorkspace()
  const win = activeWindow()
  const onPinned =
    !!hostId &&
    !!ws &&
    !!win &&
    state.isWindowPinned(hostId, ws.name, win.id)
  if (onPinned && state.pinnedWindows.length > 0) {
    await cyclePinnedWindow(direction)
    return
  }
  if (!hostId) return

  if (!ws) return
  const next = neighbourWindowId(ws, win?.id, direction)
  if (!next) return
  selectWindow(hostId, ws.id, next)
}

export const windowActions: Action[] = [
  {
    id: 'window.new',
    kind: 'action',
    label: 'New session',
    icon: '▢',
    keybinding: 'Cmd+T',
    // A workspace isn't required: openSession creates one when the last
    // window's kill has stripped it (undo toast still counting down).
    canRun: () => {
      const state = useStore.getState()
      return canOpenSession(state, state.activeHostId)
    },
    run: () => void openSession(),
  },
  {
    id: 'window.kill',
    kind: 'action',
    label: 'Kill window',
    icon: '×',
    keybinding: 'Cmd+W',
    destructive: true,
    canRun: () =>
      activeHostId() !== null && activeWorkspace() !== undefined && activeWindow() !== undefined,
    run: () => {
      const hostId = activeHostId()
      const ws = activeWorkspace()
      const win = activeWindow()
      if (hostId && ws && win) killWindow(hostId, ws.id, win)
    },
  },
  {
    id: 'window.next',
    kind: 'action',
    label: 'Next window',
    icon: '⏵',
    keybinding: ['Cmd+]', 'Cmd+ArrowRight'],
    canRun: () => activeHostId() !== null,
    run: () => {
      void stepWindow(+1)
    },
  },
  {
    id: 'window.prev',
    kind: 'action',
    label: 'Previous window',
    icon: '⏴',
    keybinding: ['Cmd+[', 'Cmd+ArrowLeft'],
    canRun: () => activeHostId() !== null,
    run: () => {
      void stepWindow(-1)
    },
  },
  // Two entries gated by `isWindowPinned` so the palette shows the
  // applicable verb at any moment — splits avoid a dynamic label that
  // would shift the row's identity between renders.
  {
    id: 'window.pin-current',
    kind: 'action',
    label: 'Pin current window',
    icon: '★',
    canRun: () => {
      const snap = activeWindowSnapshot()
      if (!snap) return false
      return !useStore.getState().isWindowPinned(snap.hostId, snap.workspace.name, snap.window.id)
    },
    run: () => {
      const snap = activeWindowSnapshot()
      if (!snap) return
      const host = useStore.getState().hosts.get(snap.hostId)
      if (!host) return
      useStore.getState().addPinnedWindow({
        hostId: snap.hostId,
        workspaceName: snap.workspace.name,
        windowId: snap.window.id,
        hostName: host.name,
        windowName: snap.window.name,
      })
    },
  },
  {
    id: 'window.unpin-current',
    kind: 'action',
    label: 'Unpin current window',
    icon: '☆',
    canRun: () => {
      const snap = activeWindowSnapshot()
      if (!snap) return false
      return useStore.getState().isWindowPinned(snap.hostId, snap.workspace.name, snap.window.id)
    },
    run: () => {
      const snap = activeWindowSnapshot()
      if (!snap) return
      useStore
        .getState()
        .removePinnedWindow(pinnedKey(snap.hostId, snap.workspace.name, snap.window.id))
    },
  },
]

/** Exposed for callers that want to identify the active window
 * without re-implementing the sort. Used by the inbox jump action
 * and by future drill-in projections. */
export function activeWindowSnapshot(): {
  hostId: HostId
  workspace: TmuxWorkspace
  window: TmuxWindow
} | null {
  const hostId = activeHostId()
  const ws = activeWorkspace()
  const win = activeWindow()
  if (!hostId || !ws || !win) return null
  return { hostId, workspace: ws, window: win }
}

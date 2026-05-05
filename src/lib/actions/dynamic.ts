/**
 * Dynamic action projections for sub-mode views.
 *
 * The static registry (`actions/index.ts`) holds verbs. Sub-modes
 * project store state into per-instance Actions on demand: every
 * workspace becomes a `workspace.<id>` action whose primary verb is
 * "switch to", every host becomes `host.<id>` etc. They share the same
 * Action shape so the palette renders + ranks them through the same
 * pipeline.
 *
 * Each projection optionally returns a `groups` map (action id →
 * header) so the renderer can drop section labels (`LOCALHOST · 2`,
 * `OFFLINE · 1`) between bucket boundaries without coupling grouping
 * into the Action type.
 */

import { commands } from '@lib/ipc'
import { connectHost, selectWorkspace } from '@lib/host'
import { useStore, pinnedKey, sortById, type TmuxWindow, type TmuxWorkspace } from '@lib/store'
import {
  displayedHostStatus,
  STATUS_LABEL,
  STATUS_RANK,
  type HostDisplayStatus,
} from '@lib/host-status'
import type { Host } from '@bindings'
import type { Action } from './types'
import { killWorkspace } from './workspace'
import { killWindow } from './window'

export interface GroupHeader {
  label: string
  count?: number
}

export interface SubModeResult {
  chip: string
  actions: Action[]
  /** Pre-computed group header per action id. The renderer emits a
   * `<SectionHeader>` the first time a new label appears in iteration
   * order, then the matching row. Absent ids skip the header. */
  groups?: Map<string, GroupHeader>
}

// ---------- @workspaces ----------

function workspaceSubActions(hostId: string, ws: TmuxWorkspace): Action[] {
  return [
    {
      id: `workspace.${hostId}.${ws.id}.switch`,
      kind: 'action',
      label: 'Switch to workspace',
      icon: '⏵',
      run: () => {
        useStore.getState().setActiveHost(hostId)
        void selectWorkspace(hostId, ws.id)
      },
    },
    {
      id: `workspace.${hostId}.${ws.id}.kill`,
      kind: 'action',
      label: 'Kill workspace',
      icon: '×',
      destructive: true,
      run: () => {
        void killWorkspace(hostId, ws)
      },
    },
  ]
}

export function workspacesAsActions(): SubModeResult {
  const state = useStore.getState()
  const actions: Action[] = []
  const groups = new Map<string, GroupHeader>()
  for (const host of state.hosts.values()) {
    const hs = state.sessions.get(host.id)
    if (!hs) continue
    const list = [...hs.workspaces.values()].sort((a, b) => a.name.localeCompare(b.name))
    const header: GroupHeader = { label: host.name.toUpperCase(), count: list.length }
    for (const ws of list) {
      const isActive = host.id === state.activeHostId && hs.activeWorkspaceId === ws.id
      const id = `workspace.${host.id}.${ws.id}`
      actions.push({
        id,
        kind: 'workspace',
        label: ws.name,
        sublabel: `· ${ws.windows.size} window${ws.windows.size === 1 ? '' : 's'}${isActive ? ' · active' : ''}`,
        icon: '◫',
        run: () => {
          state.setActiveHost(host.id)
          void selectWorkspace(host.id, ws.id)
        },
        subActions: () => workspaceSubActions(host.id, ws),
      })
      groups.set(id, header)
    }
  }
  return { chip: '@workspaces', actions, groups }
}

// ---------- #windows ----------

function windowSubActions(host: Host, ws: TmuxWorkspace, win: TmuxWindow): Action[] {
  const state = useStore.getState()
  const isPinned = state.isWindowPinned(host.id, ws.name, win.id)
  return [
    {
      id: `window.${host.id}.${ws.id}.${win.id}.jump`,
      kind: 'action',
      label: 'Jump to window',
      icon: '⏵',
      run: () => {
        state.setActiveHost(host.id)
        state.setActiveWindow(host.id, ws.id, win.id)
        void selectWorkspace(host.id, ws.id)
        void commands.tmuxSelectWindow(host.id, win.id)
      },
    },
    isPinned
      ? {
          id: `window.${host.id}.${ws.id}.${win.id}.unpin`,
          kind: 'action',
          label: 'Unpin from sidebar',
          icon: '☆',
          run: () => {
            state.removePinnedWindow(pinnedKey(host.id, ws.name, win.id))
          },
        }
      : {
          id: `window.${host.id}.${ws.id}.${win.id}.pin`,
          kind: 'action',
          label: 'Pin to sidebar',
          icon: '★',
          run: () => {
            state.addPinnedWindow({
              hostId: host.id,
              workspaceName: ws.name,
              windowId: win.id,
              hostName: host.name,
              windowName: win.name,
            })
          },
        },
    {
      id: `window.${host.id}.${ws.id}.${win.id}.kill`,
      kind: 'action',
      label: 'Kill window',
      icon: '×',
      destructive: true,
      run: () => {
        killWindow(host.id, ws.id, win)
      },
    },
  ]
}

export function windowsAsActions(): SubModeResult {
  const state = useStore.getState()
  const actions: Action[] = []
  // Pre-resolve the pinned set so each row knows whether to bubble up
  // and render with a star icon. This used to be a post-process step
  // in PaletteHost that parsed action ids back into (host, ws, win) —
  // doing it here at construction time keeps the id format private.
  const pins = state.pinnedWindows
  for (const host of state.hosts.values()) {
    const hs = state.sessions.get(host.id)
    if (!hs) continue
    for (const ws of hs.workspaces.values()) {
      for (const win of sortById(ws.windows.values())) {
        const isPinned = pins.some(
          (p) => p.hostId === host.id && p.workspaceName === ws.name && p.windowId === win.id,
        )
        actions.push({
          id: `window.${host.id}.${ws.id}.${win.id}`,
          kind: 'window',
          label: win.name,
          sublabel: `· ${host.name} → ${ws.name}`,
          icon: isPinned ? '★' : '▢',
          weight: isPinned ? 5 : 0,
          run: () => {
            state.setActiveHost(host.id)
            state.setActiveWindow(host.id, ws.id, win.id)
            void selectWorkspace(host.id, ws.id)
            void commands.tmuxSelectWindow(host.id, win.id)
          },
          subActions: () => windowSubActions(host, ws, win),
        })
      }
    }
  }
  return { chip: '#windows', actions }
}

// ---------- $hosts ----------

function hostSubActions(host: Host, display: HostDisplayStatus): Action[] {
  const out: Action[] = []
  if (display === 'connected' || display === 'connecting') {
    out.push({
      id: `host.${host.id}.disconnect`,
      kind: 'action',
      label: 'Disconnect',
      icon: '⏏',
      run: () => {
        void commands.hostDisconnect(host.id)
      },
    })
  } else {
    out.push({
      id: `host.${host.id}.connect`,
      kind: 'action',
      label: 'Connect',
      icon: '⏵',
      run: () => {
        useStore.getState().setActiveHost(host.id)
        void connectHost(host.id).catch(() => {})
      },
    })
  }
  // Localhost (port 0) can't be removed from the registry.
  if (host.port !== 0) {
    out.push({
      id: `host.${host.id}.delete`,
      kind: 'action',
      label: 'Delete host',
      icon: '×',
      destructive: true,
      run: () => {
        void (async () => {
          const ok = await useStore.getState().requestConfirm({
            title: `Delete host "${host.name}"?`,
            message:
              'This removes it from your saved list and clears any stored password. tmux sessions on the remote machine are unaffected.',
            confirmLabel: 'Delete',
            destructive: true,
          })
          if (!ok) return
          let res: Awaited<ReturnType<typeof commands.hostDelete>>
          try {
            res = await commands.hostDelete(host.id)
          } catch (e) {
            useStore.getState().pushToast({
              id: `host-delete-error::${host.id}`,
              message: `Delete threw: ${String(e)}`,
              durationMs: 8_000,
            })
            return
          }
          if (res.status !== 'ok') {
            useStore.getState().pushToast({
              id: `host-delete-error::${host.id}`,
              message: `Couldn't fully delete "${host.name}": ${res.error}`,
              durationMs: 8_000,
            })
          }
        })()
      },
    })
  }
  return out
}

export function hostsAsActions(): SubModeResult {
  const state = useStore.getState()
  const actions: Action[] = []
  const groups = new Map<string, GroupHeader>()

  // Sort by status bucket, then by name within bucket. Counts per
  // bucket are computed in the same pass so each row's group header
  // can carry an accurate `count` without a second walk.
  const sorted = [...state.hosts.values()]
    .map((h) => {
      const status = state.statuses.get(h.id)
      const detached = state.sessions.get(h.id)?.detachedReason ?? null
      return { host: h, display: displayedHostStatus(h, status, detached) }
    })
    .sort((a, b) => {
      const r = STATUS_RANK[a.display] - STATUS_RANK[b.display]
      return r !== 0 ? r : a.host.name.localeCompare(b.host.name)
    })

  const counts = new Map<HostDisplayStatus, number>()
  for (const { display } of sorted) {
    counts.set(display, (counts.get(display) ?? 0) + 1)
  }
  const headerByDisplay = new Map<HostDisplayStatus, GroupHeader>()
  for (const [display, count] of counts) {
    headerByDisplay.set(display, { label: STATUS_LABEL[display], count })
  }

  for (const { host, display } of sorted) {
    const sublabel = host.port === 0 ? `· localhost` : `· ssh ${host.user}@${host.hostname}`
    const id = `host.${host.id}`
    actions.push({
      id,
      kind: 'host',
      label: host.name,
      sublabel,
      icon: '●',
      run: () => {
        state.setActiveHost(host.id)
        if (display === 'disconnected' || display === 'error') {
          void connectHost(host.id).catch(() => {})
        }
      },
      subActions: () => hostSubActions(host, display),
    })
    const header = headerByDisplay.get(display)
    if (header) groups.set(id, header)
  }

  return { chip: '$hosts', actions, groups }
}

// ---------- dispatcher ----------

export type Sigil = '@' | '#' | '$'

/** Resolve a sigil to the matching projection. */
export function resolveSigil(sigil: Sigil): SubModeResult {
  switch (sigil) {
    case '@':
      return workspacesAsActions()
    case '#':
      return windowsAsActions()
    case '$':
      return hostsAsActions()
  }
}

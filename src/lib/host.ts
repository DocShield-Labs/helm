/**
 * Frontend host glue.
 *
 * Owns the single Tauri Channel that receives every `HostEvent`. Routes:
 *   - `session` events → per-pane streams / block tables / store tree
 *   - `status` events  → host status map (+ tree fetch on connect)
 *   - `host_added` / `host_removed` → registry mutations
 *   - notifications, schedules, tool suggestions → store
 *
 * Selection (which workspace/window/pane is shown) is purely frontend
 * state — the daemon has no notion of it. `selectWindow` is the one
 * entry point every "jump to window" path uses.
 */

import { Channel } from '@tauri-apps/api/core'
import { commands } from '@lib/ipc'
import { useStore, workspaceForWindow } from '@lib/store'
import { treeToWorkspaces } from '@lib/session/tree'
import * as stream from '@lib/session/stream'
import * as blocks from '@lib/session/blocks'
import type { HostEvent, HostId, SessionEvent } from '@bindings'

let subscribed = false

/**
 * Open the global event channel. Idempotent — calling twice is a no-op.
 * Throws if the underlying tauri command fails.
 */
export async function subscribeHostEvents(): Promise<void> {
  if (subscribed) return
  const channel = new Channel<HostEvent>()
  channel.onmessage = (evt) => {
    // host_added / host_removed are the only events that legitimately
    // arrive for a host id we don't yet (or no longer) track.
    if (
      evt.kind !== 'host_added' &&
      evt.kind !== 'host_removed' &&
      'host_id' in evt &&
      !useStore.getState().hosts.has(evt.host_id)
    ) {
      return
    }
    switch (evt.kind) {
      case 'session':
        handleSessionEvent(evt.host_id, evt.event)
        return
      case 'status': {
        const store = useStore.getState()
        const prev = store.statuses.get(evt.host_id)
        store.setHostStatus(evt.host_id, evt.status)
        if (evt.status === 'connected') {
          store.setHostError(evt.host_id, null)
        } else if (evt.error) {
          store.setHostError(evt.host_id, evt.error)
        }
        if (evt.status === 'connected' && prev !== 'connected') {
          // The pump ships a Tree event right after connect, but a
          // fetch here covers the webview-reload case too.
          void refetchTree(evt.host_id)
        }
        if (evt.status === 'disconnected' && prev === 'connected') {
          store.setWorkspaces(evt.host_id, [])
          store.clearRunningForHost(evt.host_id)
          stream.dropHost(evt.host_id)
          blocks.dropHost(evt.host_id)
        }
        // Reconnecting: keep the tree. helmd persists across transport
        // drops (and auto-respawns on localhost); the next Tree event
        // reconciles whatever changed, and panes resume from their
        // last seq — the whole point of seq-addressed output.
        return
      }
      case 'host_added':
        useStore.getState().addHost(evt.host)
        return
      case 'host_removed':
        stream.dropHost(evt.host_id)
        blocks.dropHost(evt.host_id)
        useStore.getState().removeHost(evt.host_id)
        return
      case 'host_key_prompt':
        useStore.getState().setHostKeyPrompt({
          hostId: evt.host_id,
          hostname: evt.hostname,
          port: evt.port,
          algorithm: evt.algorithm,
          fingerprint: evt.fingerprint,
          kind: evt.prompt,
        })
        return
      case 'notification':
        useStore.getState().upsertNotification(evt.notification)
        return
      case 'notification_dismissed':
        useStore.getState().removeNotification(evt.notification_id)
        return
      case 'tool_integration_suggested':
        useStore.getState().pushToolSuggestion({
          hostId: evt.host_id,
          integrationId: evt.integration_id,
          name: evt.name,
          description: evt.description,
          postInstallNote: evt.post_install_note,
        })
        return
      case 'schedule_upserted':
        useStore.getState().upsertSchedule(evt.schedule)
        return
      case 'schedule_removed':
        useStore.getState().removeSchedule(evt.schedule_id)
        return
      case 'schedule_fired': {
        // Manual fires jump to the new window — the user just clicked
        // "Run now" and expects to see it. Cron fires don't yank focus.
        if (!evt.manual) return
        const store = useStore.getState()
        const sched = store.schedules.get(evt.schedule_id)
        if (!sched) return
        const hs = store.sessions.get(sched.host_id)
        const ws = workspaceForWindow(hs, evt.window_id)
        if (ws) {
          selectWindow(sched.host_id, ws.id, evt.window_id)
        } else {
          // Tree may not have caught up yet — retry once it does.
          const windowId = evt.window_id
          const hostId = sched.host_id
          window.setTimeout(() => {
            const later = workspaceForWindow(useStore.getState().sessions.get(hostId), windowId)
            if (later) selectWindow(hostId, later.id, windowId)
          }, 300)
        }
        return
      }
    }
  }
  const res = await commands.hostSubscribe(channel)
  if (res.status !== 'ok') throw new Error(res.error)
  subscribed = true

  // Replay current notifications / schedules so a webview reload finds
  // the world the way it left it. Best-effort.
  try {
    const list = await commands.notificationsList()
    if (list.status === 'ok') {
      const upsert = useStore.getState().upsertNotification
      for (const n of list.data) upsert(n)
    }
  } catch {
    /* no-op */
  }
  try {
    const list = await commands.scheduleList()
    if (list.status === 'ok') useStore.getState().setSchedules(list.data)
  } catch {
    /* no-op */
  }
}

function handleSessionEvent(hostId: HostId, ev: SessionEvent): void {
  const store = useStore.getState()
  switch (ev.kind) {
    case 'output':
      stream.applyOutput(hostId, ev.pane_id, ev.seq, ev.data)
      return
    case 'replay_done':
      stream.onReplayDone(hostId, ev.pane_id)
      return
    case 'block': {
      const b = ev.block
      blocks.upsertBlock(hostId, ev.pane_id, b)
      if (blocks.isRunning(b)) {
        store.markPaneRunning(hostId, ev.pane_id, b.cmdline)
      } else if (b.end_seq !== null) {
        store.markPaneIdle(hostId, ev.pane_id)
      }
      if (b.cwd !== null) {
        store.updatePaneCwd(hostId, ev.pane_id, b.cwd, b.branch ?? '')
      }
      return
    }
    case 'mode_change':
      blocks.setAltScreen(hostId, ev.pane_id, ev.alt_screen)
      return
    case 'tree':
      store.setWorkspaces(hostId, treeToWorkspaces(ev.tree))
      return
    case 'pane_exited':
      blocks.setExited(hostId, ev.pane_id)
      store.markPaneIdle(hostId, ev.pane_id)
      return
    case 'bell':
      blocks.ringBell(hostId, ev.pane_id)
      return
  }
}

/**
 * Connect a host. The daemon's tree arrives via events; we also fetch
 * it here so callers can read the store right after this resolves.
 */
export async function connectHost(hostId: HostId, bootstrapWorkspace?: string): Promise<void> {
  const res = await commands.hostConnect(hostId, bootstrapWorkspace ?? null)
  if (res.status !== 'ok') throw new Error(res.error)
  await refetchTree(hostId)
}

/** Make `workspaceId` the active workspace for `hostId`. Local state. */
export async function selectWorkspace(hostId: HostId, workspaceId: string): Promise<void> {
  useStore.getState().setActiveWorkspace(hostId, workspaceId)
}

/** Focus a window: active host + workspace + window, all local state. */
export function selectWindow(hostId: HostId, workspaceId: string, windowId: string): void {
  const store = useStore.getState()
  store.setActiveHost(hostId)
  store.setActiveWorkspace(hostId, workspaceId)
  store.setActiveWindow(hostId, workspaceId, windowId)
}

/** Pull the daemon's current tree into the store. */
export async function refetchTree(hostId: HostId): Promise<void> {
  const res = await commands.sessionTree(hostId)
  if (res.status !== 'ok') return
  useStore.getState().setWorkspaces(hostId, treeToWorkspaces(res.data))
}

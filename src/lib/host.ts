/**
 * Frontend host glue.
 *
 * Owns the single Tauri Channel that receives every `HostEvent`. Routes:
 *   - `tmux` events  → per-pane output subscribers + per-host store actions
 *   - `status` events → host status map (+ tree refetch on connect)
 *   - `host_added` / `host_removed` → registry mutations
 *
 * Output subscribers are keyed by `${hostId}::${paneId}`. Output that
 * arrives before a pane has a subscriber is dropped — TmuxPane subscribes
 * synchronously on mount and captures the buffer separately
 * (`tmux_capture_pane`), so anything pre-subscribe predates the pane
 * existing in the UI anyway.
 */

import { Channel } from '@tauri-apps/api/core'
import { commands } from '@lib/ipc'
import { useStore, type TmuxWorkspace, type TmuxWindow, type TmuxPane } from '@lib/store'
import type { HostEvent, HostId, MarkerAt } from '@bindings'

type OutputListener = (bytes: number[], markers: MarkerAt[]) => void

const listeners = new Map<string, Set<OutputListener>>()
let subscribed = false
const subKey = (hostId: HostId, paneId: string) => `${hostId}::${paneId}`

// Per-host pre-hydration state. Each host gets at most one prehydrate
// pass per connect — without this guard, every refetchTree (which fires
// on every tmux event) would kick off another pass, flooding tmux's
// command queue with capture-pane requests and starving user input.
const prehydratingHosts = new Set<HostId>()
const prehydratedHosts = new Set<HostId>()

/** Cap on lines fetched for the "full history" pane capture. Tmux's
 * default `history-limit` is 2000, so this is enough to cover most
 * users' expectation of "scroll up to read context." Long-lived panes
 * with large history-limits can otherwise produce multi-MB responses,
 * which serialize over SSH and produce multi-second waits per pane. */
const FULL_CAPTURE_LINES = 2000

/**
 * Open the global event channel. Idempotent — calling twice is a no-op.
 * Throws if the underlying tauri command fails.
 */
export async function subscribeHostEvents(): Promise<void> {
  if (subscribed) return
  const channel = new Channel<HostEvent>()
  channel.onmessage = (evt) => {
    // host_added / host_removed are the only events that legitimately
    // arrive for a host id we don't yet (or no longer) track. Every
    // other event references a host that should already be in the
    // store. Drop late-arriving events for deleted hosts so a stuck
    // supervisor's tail-end emissions can't repopulate `statuses` /
    // `notifications` / `toolSuggestions` for a host the user just
    // removed.
    if (
      evt.kind !== 'host_added' &&
      evt.kind !== 'host_removed' &&
      'host_id' in evt &&
      !useStore.getState().hosts.has(evt.host_id)
    ) {
      return
    }
    if (evt.kind === 'status') {
      const store = useStore.getState()
      const prev = store.statuses.get(evt.host_id)
      store.setHostStatus(evt.host_id, evt.status)
      // Stash the supervisor's last connect error (or clear it on a
      // successful connect). The ReconnectingOverlay reads this so the
      // user can see *why* a reconnect is stuck — most useful for
      // localhost when tmux can't be respawned (binary missing, etc.).
      if (evt.status === 'connected') {
        store.setHostError(evt.host_id, null)
      } else if (evt.error) {
        store.setHostError(evt.host_id, evt.error)
      }
      // Bootstrap the workspace tree + kick off a pre-hydration pass on
      // the connect transition. tmux doesn't replay the world for
      // late-attaching control clients; without the refetch the sidebar
      // stays empty, and pre-hydration is what makes "click any pane,
      // see content instantly" work.
      if (evt.status === 'connected' && prev !== 'connected') {
        void (async () => {
          await refetchTree(evt.host_id)
          void prehydrateCaptures(evt.host_id)
        })()
      }
      // Clear stale tree on disconnect transition. Without this the
      // sidebar still shows the dead workspace/window/pane the user
      // just killed, and TmuxPane keeps the frozen capture buffer
      // visible — both very confusing. Capture cache is invalidated
      // here too — captures from the previous tmux server can't be
      // assumed valid for whatever we'll connect to next. The
      // prehydrate guards reset so the next connect re-hydrates.
      if (evt.status === 'disconnected' && prev === 'connected') {
        store.setWorkspaces(evt.host_id, [])
        store.clearPaneCapturesForHost(evt.host_id)
        store.clearRunningForHost(evt.host_id)
        prehydratingHosts.delete(evt.host_id)
        prehydratedHosts.delete(evt.host_id)
      }
      // Reconnecting on a remote: keep the tree + captures so the user
      // sees their last frame while the SSH ladder runs — tmux state
      // on the remote machine survives transport drops, and the
      // freeze-frame is the whole point of keep-alive panes.
      //
      // Reconnecting on localhost: tmux *server* state usually went
      // away with whatever killed it (process death, pkill, OS reaper),
      // so the tree we have is fossilized — its session/window/pane
      // ids point at things that no longer exist. Wipe so the user
      // doesn't type into a phantom pane that silently no-ops while
      // the supervisor respawns tmux. The next successful connect
      // refetches a fresh tree.
      if (evt.status === 'reconnecting' && prev === 'connected') {
        const host = store.hosts.get(evt.host_id)
        if (host && host.port === 0) {
          store.setWorkspaces(evt.host_id, [])
          store.clearPaneCapturesForHost(evt.host_id)
          store.clearRunningForHost(evt.host_id)
          prehydratingHosts.delete(evt.host_id)
          prehydratedHosts.delete(evt.host_id)
        }
      }
      return
    }
    if (evt.kind === 'host_added') {
      useStore.getState().addHost(evt.host)
      return
    }
    if (evt.kind === 'host_removed') {
      useStore.getState().removeHost(evt.host_id)
      return
    }
    if (evt.kind === 'host_key_prompt') {
      useStore.getState().setHostKeyPrompt({
        hostId: evt.host_id,
        hostname: evt.hostname,
        port: evt.port,
        algorithm: evt.algorithm,
        fingerprint: evt.fingerprint,
        kind: evt.prompt,
      })
      return
    }
    if (evt.kind === 'notification') {
      useStore.getState().upsertNotification(evt.notification)
      return
    }
    if (evt.kind === 'notification_dismissed') {
      useStore.getState().removeNotification(evt.notification_id)
      return
    }
    if (evt.kind === 'tool_integration_suggested') {
      useStore.getState().pushToolSuggestion({
        hostId: evt.host_id,
        integrationId: evt.integration_id,
        name: evt.name,
        description: evt.description,
        postInstallNote: evt.post_install_note,
      })
      return
    }
    // evt.kind === 'tmux'
    const { host_id, notification: n } = evt
    const store = useStore.getState()
    switch (n.kind) {
      case 'output':
        // Mirror the OSC 133 lifecycle into the store so the sidebar
        // dot can show a live spinner. BlockTracker still consumes the
        // same markers in-pane for block chrome — both readers run
        // independently. We walk in marker order so a chunk containing
        // `D` followed by a fresh `B` (one prompt cycle ending, the
        // next command starting) settles to "running".
        for (const m of n.markers) {
          if (m.marker.kind === 'command_start') {
            store.markPaneRunning(host_id, n.pane_id, m.marker.command)
          } else if (m.marker.kind === 'command_done') {
            store.markPaneIdle(host_id, n.pane_id)
          }
        }
        deliverOutput(host_id, n.pane_id, n.bytes, n.markers)
        return

      // Window-level changes — we don't know which session a window
      // belongs to from the notification alone (tmux's protocol omits
      // session id on window-add/close/renamed), so just refetch.
      case 'window_added':
      case 'window_closed':
        void refetchTree(host_id)
        return
      case 'window_renamed': {
        // Fast path: locate the window we already have and rename in-place.
        // If we've never heard of it, refetch fills in the gap.
        const hs = store.sessions.get(host_id)
        let renamed = false
        if (hs) {
          for (const ws of hs.workspaces.values()) {
            if (ws.windows.has(n.window_id)) {
              const win = ws.windows.get(n.window_id)!
              const nextWindows = new Map(ws.windows)
              nextWindows.set(n.window_id, { ...win, name: n.name })
              const nextWorkspaces = new Map(hs.workspaces)
              nextWorkspaces.set(ws.id, { ...ws, windows: nextWindows })
              useStore.setState({
                sessions: new Map(store.sessions).set(host_id, {
                  ...hs,
                  workspaces: nextWorkspaces,
                }),
              })
              renamed = true
              break
            }
          }
        }
        if (!renamed) void refetchTree(host_id)
        return
      }

      // Session-level changes — refetch (cheap; multi-session list is small).
      case 'session_changed':
        // The control client switched its current session. We use this
        // to seed activeWorkspaceId on the very first connect; otherwise
        // we let the user drive workspace selection.
        if (!store.sessions.get(host_id)?.activeWorkspaceId) {
          store.setActiveWorkspace(host_id, n.session_id)
        }
        void refetchTree(host_id)
        return
      case 'session_renamed':
      case 'sessions_changed':
        void refetchTree(host_id)
        return
      case 'session_window_changed':
        // tmux says session $X is now showing window @Y. Reflect it in
        // the active flag of the windows map for that workspace.
        store.setActiveWindow(host_id, n.session_id, n.window_id)
        return
      case 'window_pane_changed': {
        // We only know window_id + pane_id; find the workspace that
        // owns the window so we can demote the right siblings.
        const hs = store.sessions.get(host_id)
        if (!hs) return
        for (const ws of hs.workspaces.values()) {
          if (ws.windows.has(n.window_id)) {
            store.setActivePane(host_id, ws.id, n.window_id, n.pane_id)
            return
          }
        }
        return
      }

      case 'exit':
        store.markDetached(host_id, n.reason ?? null)
        return
      default:
        // layout_changed, pane_mode_changed, continue, pause,
        // client_detached, unknown — ignored for now.
        return
    }
  }
  const res = await commands.hostSubscribe(channel)
  if (res.status !== 'ok') throw new Error(res.error)
  subscribed = true

  // Replay current notifications so a webview reload (Cmd+R) finds the
  // inbox the way it left it. New events keep flowing through the
  // channel handler above. Best-effort — a failure here just means the
  // user starts with an empty inbox until the next event arrives.
  try {
    const list = await commands.notificationsList()
    if (list.status === 'ok') {
      const upsert = useStore.getState().upsertNotification
      for (const n of list.data) upsert(n)
    }
  } catch {
    /* no-op */
  }
}

/**
 * Connect a host and bootstrap its tmux tree once Connected. Returns
 * when the connect command resolves; the tree fills in via subsequent
 * events and the `connected` transition's refetchTree.
 *
 * `bootstrapWorkspace` overrides the host's `default_workspace` for the
 * connect attempt. The "+ workspace" button passes the new workspace's
 * name so the bootstrap creates exactly that session if no others exist
 * — avoiding a stray `main` workspace appearing alongside the user's
 * intended one.
 */
export async function connectHost(
  hostId: HostId,
  bootstrapWorkspace?: string,
): Promise<void> {
  const res = await commands.hostConnect(hostId, bootstrapWorkspace ?? null)
  if (res.status !== 'ok') throw new Error(res.error)
  await refetchTree(hostId)
}

/**
 * Make `workspaceId` the active workspace for `hostId`. Pure local-
 * state flip in the multi-client model — every session has its own
 * permanently-attached control client at the user's viewport size, so
 * there's no tmux-side switch to perform. Pre-multi-client we called
 * `tmux_switch_client` here to keep the single client attached to the
 * right session for sizing; that command is now a backend no-op.
 */
export async function selectWorkspace(hostId: HostId, workspaceId: string): Promise<void> {
  useStore.getState().setActiveWorkspace(hostId, workspaceId)
}

/**
 * Refetch the entire workspace+window+pane tree for `host` and write it
 * to the store. Run on every connect, and after any tree-mutating
 * notification where a delta isn't precise enough.
 *
 * tmux's list commands are cheap (microseconds locally, single-digit ms
 * over SSH), so we just re-list everything rather than maintaining
 * per-event deltas. Predictable correctness > minimum chatter.
 */
async function refetchTree(host: HostId): Promise<void> {
  const store = useStore.getState()

  // We use `|` as the field delimiter rather than `\t`. In theory the
  // terminal modes we send with `request_pty` (RAW_TERMINAL_MODES in
  // helm-ssh) should make `\t` round-trip cleanly. In practice, many
  // sshd configurations don't honor terminal modes on non-interactive
  // `exec` channels, and one of our requested modes (`IUCLC`) is
  // Linux-only and may make BSD/macOS sshd reject the whole list.
  // `|` round-trips on every server we've tested, so we use it; the
  // terminal modes still help with echo / canonical-mode side effects.
  const sessRes = await commands.tmuxListSessions(host, '#{session_id}|#{session_name}')
  if (sessRes.status !== 'ok') {
    // Don't clobber the workspace tree here. Two refetches can run
    // concurrently (one from the connected-status event handler, one
    // from connectHost's await), and one transiently failing while
    // the other succeeds would erase good data. Real disconnections
    // are handled by the disconnect-status transition explicitly.
    return
  }
  const sessions = sessRes.data
    .split('\n')
    .filter((line) => line.length > 0)
    .map((line) => {
      const [id, name] = line.split('|')
      return { id, name }
    })

  const winRes = await commands.tmuxListWindows(
    host,
    '#{session_id}|#{window_id}|#{window_name}|#{window_active}',
  )
  const windowRows = winRes.status === 'ok'
    ? winRes.data
        .split('\n')
        .filter((l) => l.length > 0)
        .map((l) => {
          const [sessionId, id, name, active] = l.split('|')
          return { sessionId, id, name, active: active === '1' }
        })
    : []

  const paneRes = await commands.tmuxListPanes(
    host,
    '#{session_id}|#{window_id}|#{pane_id}|#{pane_active}|#{pane_current_command}|#{pane_current_path}',
  )
  const paneRows = paneRes.status === 'ok'
    ? paneRes.data
        .split('\n')
        .filter((l) => l.length > 0)
        .map((l) => {
          // pane_current_path can theoretically contain `|` on some
          // exotic filesystems; rejoin everything from index 5 onward
          // so we don't truncate the path.
          const parts = l.split('|')
          const [sessionId, windowId, id, active, command] = parts
          const cwd = parts.slice(5).join('|')
          // Branch lives on TmuxPane but isn't fetched here — tmux's
          // `#(shell-cmd)` substitution is async-cached against
          // status-right and doesn't refresh on ad-hoc list-panes
          // calls, so we'd always get empty strings. Real branch
          // fetching needs a separate `host_run_shell` IPC; until then
          // the field stays empty and the footer segment stays hidden.
          return { sessionId, windowId, id, active: active === '1', command, branch: '', cwd }
        })
    : []

  // Group rows by session id and assemble TmuxWorkspace records.
  const workspaces: TmuxWorkspace[] = sessions.map((s) => {
    const windows = new Map<string, TmuxWindow>()
    for (const w of windowRows) {
      if (w.sessionId === s.id) {
        windows.set(w.id, { id: w.id, name: w.name, active: w.active })
      }
    }
    const panes = new Map<string, TmuxPane>()
    for (const p of paneRows) {
      if (p.sessionId === s.id) {
        panes.set(p.id, {
          id: p.id,
          windowId: p.windowId,
          active: p.active,
          command: p.command,
          cwd: p.cwd,
          branch: p.branch,
        })
      }
    }
    return { id: s.id, name: s.name, windows, panes }
  })

  store.setWorkspaces(host, workspaces)
}

/**
 * Walk every pane on `host` and warm `store.paneCaptures`. Runs at most
 * once per connect (guarded by `prehydratedHosts`). Subsequent refetches
 * don't re-run this — they just keep the workspace tree current.
 *
 * Two passes:
 *   1. **Fast pass.** Visible-buffer-only (`scrollback: false`) for every
 *      pane. Each capture is a few KB instead of hundreds, so bulk
 *      pre-hydrating 12+ remote panes finishes in ~hundreds of ms total
 *      without saturating tmux's command queue. After this pass, every
 *      pane has *some* capture — clicking any pane shows content
 *      instantly.
 *   2. **Upgrade pass.** Sequentially fetches full scrollback (`-S -`)
 *      for each pane and replaces the cache entry. Each capture costs a
 *      round-trip; serialized so user keystrokes interleave naturally.
 *      Once a pane's cache entry has `hasScrollback: true`, TmuxPane
 *      skips the extra fetch on mount and the user has full history to
 *      page through immediately.
 *
 * If the user clicks a pane before the upgrade pass reaches it, TmuxPane
 * does its own full-scrollback fetch. The two may race on the same pane
 * occasionally — both produce the same bytes, last-write-wins is fine.
 */
async function prehydrateCaptures(host: HostId): Promise<void> {
  if (prehydratingHosts.has(host) || prehydratedHosts.has(host)) return
  prehydratingHosts.add(host)
  try {
    const hs = useStore.getState().sessions.get(host)
    if (!hs) return

    // Pass 1: fast visible-buffer warm-up across every pane.
    for (const ws of hs.workspaces.values()) {
      for (const paneId of ws.panes.keys()) {
        const key = `${host}::${paneId}`
        if (useStore.getState().paneCaptures.has(key)) continue
        try {
          const cap = await commands.tmuxCapturePane(host, paneId, 0)
          if (cap.status === 'ok') {
            useStore.getState().setPaneCapture(host, paneId, cap.data, false)
          }
        } catch {
          // Best-effort. TmuxPane falls back to a live capture-pane on
          // mount if there's no cache entry.
        }
      }
    }
    prehydratedHosts.add(host)

    // Pass 2: upgrade every entry to bounded-history scrollback.
    // Sequential so we don't pile a dozen captures into tmux's queue at
    // once. Re-read sessions each iteration in case workspaces/panes
    // changed (creation, kill) while pass 1 was running.
    const upgraded = new Set<string>()
    while (true) {
      const cur = useStore.getState().sessions.get(host)
      if (!cur) break
      const target = pickPaneNeedingUpgrade(host, cur, upgraded)
      if (!target) break
      upgraded.add(target)
      try {
        const cap = await commands.tmuxCapturePane(host, target, FULL_CAPTURE_LINES)
        if (cap.status === 'ok') {
          useStore.getState().setPaneCapture(host, target, cap.data, true)
        }
      } catch {
        // Best-effort. TmuxPane will fall back if it mounts before
        // the upgrade succeeds.
      }
    }
  } finally {
    prehydratingHosts.delete(host)
  }
}

/** Find the next pane on `host` that doesn't yet have a full-scrollback
 * capture and hasn't been attempted yet in this upgrade pass. */
function pickPaneNeedingUpgrade(
  host: HostId,
  hs: { workspaces: Map<string, TmuxWorkspace> },
  attempted: Set<string>,
): string | null {
  const captures = useStore.getState().paneCaptures
  for (const ws of hs.workspaces.values()) {
    for (const paneId of ws.panes.keys()) {
      if (attempted.has(paneId)) continue
      const entry = captures.get(`${host}::${paneId}`)
      if (entry?.hasScrollback) continue
      return paneId
    }
  }
  return null
}

// ---------- per-pane output pub/sub ----------

function deliverOutput(
  hostId: HostId,
  paneId: string,
  bytes: number[],
  markers: MarkerAt[],
) {
  const subs = listeners.get(subKey(hostId, paneId))
  if (!subs) return
  for (const fn of subs) fn(bytes, markers)
}

/**
 * Subscribe to a specific pane's output stream. Returns an unsubscribe
 * function. Output that arrived before subscription is *not* delivered —
 * see the module-level docstring.
 */
export function subscribePaneOutput(
  hostId: HostId,
  paneId: string,
  fn: OutputListener,
): () => void {
  const key = subKey(hostId, paneId)
  let set = listeners.get(key)
  if (!set) {
    set = new Set()
    listeners.set(key, set)
  }
  set.add(fn)
  return () => {
    set?.delete(fn)
    if (set?.size === 0) listeners.delete(key)
  }
}

import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'
import './index.css'

// Expose the typed commands surface on `window` so devtools pastes can
// drive Tauri without enabling `withGlobalTauri`. Cheap; harmless in prod.
;(window as unknown as { helm?: typeof commands }).helm = commands

// Dev helpers — short names so single-line console pastes don't wrap.
;(window as unknown as { dbg?: unknown }).dbg = {
  async start() {
    // Save mac-studio (persists to hosts.json) + connect + dump tree
    // in one shot. Re-runnable — host_save upserts.
    const id = crypto.randomUUID()
    const add = await commands.hostSave({
      id,
      name: 'mac-studio',
      hostname: 'mac-studio.tail2ab8ae.ts.net',
      port: 22,
      user: 'azhar',
      auth: 'Agent',
      jump_host: null,
      tmux_integration: true,
      default_workspace: 'helm',
      startup_commands: [],
    })
    if (add.status !== 'ok') return { add }
    const conn = await commands.hostConnect(add.data, null)
    if (conn.status !== 'ok') return { add, conn }
    // Wait a beat for the post-connect events to settle.
    await new Promise((r) => setTimeout(r, 500))
    const w = await commands.tmuxListWindows(
      add.data,
      '#{window_id}|#{window_name}|#{window_active}',
    )
    const p = await commands.tmuxListPanes(
      add.data,
      '#{pane_id}|#{window_id}|#{pane_active}|#{pane_current_command}',
    )
    return { id: add.data, w, p }
  },
  async windows() {
    const list = await commands.hostList()
    if (list.status !== 'ok') return list
    const remote = list.data.find((h) => h.port !== 0)
    if (!remote) return 'no remote host'
    return commands.tmuxListWindows(remote.id, '#{window_id}|#{window_name}|#{window_active}')
  },
  /** Dump what the frontend store *thinks* is true. */
  state() {
    const s = useStore.getState()
    return {
      activeHostId: s.activeHostId,
      hosts: [...s.hosts.values()].map((h) => ({ id: h.id, name: h.name, port: h.port })),
      statuses: Object.fromEntries(s.statuses),
      sessions: Object.fromEntries(
        [...s.sessions.entries()].map(([hid, hs]) => [
          hid,
          {
            activeWorkspaceId: hs.activeWorkspaceId,
            detachedReason: hs.detachedReason,
            workspaces: [...hs.workspaces.values()].map((w) => ({
              id: w.id,
              name: w.name,
              windows: [...w.windows.values()],
              panes: [...w.panes.values()],
            })),
          },
        ]),
      ),
    }
  },
  /** Tab-delimited fetch — exercises the octal-decode path. */
  async tabwin() {
    const list = await commands.hostList()
    if (list.status !== 'ok') return list
    const remote = list.data.find((h) => h.port !== 0)
    if (!remote) return 'no remote host'
    const r = await commands.tmuxListWindows(
      remote.id,
      '#{window_id}\t#{window_name}\t#{window_active}',
    )
    if (r.status !== 'ok') return r
    // Show every char, including tabs as <TAB>, so we can confirm the
    // server returned literal tabs vs `\011`.
    return r.data.replace(/\t/g, '<TAB>').replace(/\\/g, '<BSL>')
  },
  async panes() {
    const list = await commands.hostList()
    if (list.status !== 'ok') return list
    const remote = list.data.find((h) => h.port !== 0)
    if (!remote) return 'no remote host'
    return commands.tmuxListPanes(
      remote.id,
      '#{pane_id}|#{window_id}|#{pane_active}|#{pane_current_command}',
    )
  },
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)

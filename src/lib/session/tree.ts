/**
 * `SessionTree` (helmd's snapshot, via the Rust bridge) → the store's
 * per-workspace projection. Pure; selection flags start false and the
 * store's `setWorkspaces` carries the user's current selection over.
 */

import type { SessionTree } from '@bindings'
import type { TmuxPane, TmuxWindow, TmuxWorkspace } from '@lib/store'

export function treeToWorkspaces(tree: SessionTree): TmuxWorkspace[] {
  return tree.workspaces.map((ws) => {
    const windows = new Map<string, TmuxWindow>()
    const panes = new Map<string, TmuxPane>()
    for (const w of ws.windows) {
      windows.set(w.id, { id: w.id, name: w.name, active: false })
      for (const p of w.panes) {
        panes.set(p.id, {
          id: p.id,
          windowId: w.id,
          active: false,
          command: p.command ?? '',
          cwd: p.cwd ?? '',
          branch: p.branch ?? '',
          root: p.root ?? '',
        })
      }
    }
    return { id: ws.id, name: ws.name, windows, panes }
  })
}

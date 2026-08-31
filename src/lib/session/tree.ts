/**
 * `SessionTree` (helmd's snapshot, via the Rust bridge) → store sessions.
 */

import type { SessionTree } from '@bindings'
import type { Session } from '@lib/store'

export function treeToSessions(tree: SessionTree): Session[] {
  return tree.sessions.map((session) => ({
    id: session.id,
    name: session.name,
    command: session.command ?? '',
    cols: session.cols,
    rows: session.rows,
    cwd: session.cwd ?? '',
    branch: session.branch ?? '',
    root: session.root ?? '',
  }))
}

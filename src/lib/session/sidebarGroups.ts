/**
 * Sidebar grouping: sessions by the project they're in.
 *
 * The group key is the session's git toplevel (so every shell and
 * agent in a repo — any subdirectory — sits together, and a worktree
 * is its own group), falling back to the cwd outside a repo. Groups
 * appear in the order they first appeared (the oldest session in each),
 * never alphabetically, so a new project appends at the bottom and
 * nothing above it moves; within a group sessions keep their own
 * order. Sessions whose directory isn't known yet (no integration,
 * exited) go last, under no header.
 */

import { homeRelative } from '@lib/path'

export interface Groupable {
  /** Git toplevel, '' when not in a repo or unknown. */
  root: string
  /** Current directory, '' when unknown. */
  cwd: string
}

export interface Group<T> {
  /** The directory the group is keyed on ('' for the unknown group). */
  key: string
  /** Home-relative label; '' for the unknown group (rendered headerless). */
  label: string
  rows: T[]
}

/** The directory a session is grouped under. */
export function groupKeyOf(p: Groupable): string {
  return p.root || p.cwd || ''
}

/** Where the session is inside its group: `cwd` relative to the group
 * directory ('' at the group directory itself). A cwd outside the
 * group — shouldn't happen, but a stale sample can — shows in full. */
export function relativeCwd(p: Groupable): string {
  const key = groupKeyOf(p)
  if (!p.cwd || !key || p.cwd === key) return ''
  if (p.cwd.startsWith(key + '/')) return p.cwd.slice(key.length + 1)
  return homeRelative(p.cwd)
}

/** Group `rows` (already in their own stable order) by directory. */
export function groupRows<T extends Groupable>(rows: readonly T[]): Group<T>[] {
  const groups = new Map<string, Group<T>>()
  for (const r of rows) {
    const key = groupKeyOf(r)
    let g = groups.get(key)
    if (!g) {
      g = { key, label: key ? homeRelative(key) : '', rows: [] }
      groups.set(key, g)
    }
    g.rows.push(r)
  }
  const out = [...groups.values()]
  const unknown = groups.get('')
  if (unknown) {
    out.splice(out.indexOf(unknown), 1)
    out.push(unknown)
  }
  return out
}

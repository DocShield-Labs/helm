/**
 * Folder-view projection.
 *
 * Folder view is a UI-only re-grouping of a host's windows, keyed by
 * each window's cwd path. The underlying tmux state (workspaces /
 * sessions) is unchanged — folders are synthetic labels computed at
 * render time. Two windows in different workspaces both sitting in
 * the same cwd land under one folder header.
 *
 * Grouping uses the **full cwd path** as the key (display label is
 * the basename via `prettyCwd`) so two unrelated dirs that share a
 * basename — `~/Code/foo` and `~/Other/foo` — stay separate. The
 * empty-cwd bucket ('') collects windows whose pane cwd hasn't been
 * sampled yet; it sorts to the bottom under a `(no cwd)` label.
 */

import type { HostSessions, TmuxWindow, TmuxWorkspace } from './store'
import { paneCwdFor, prettyCwd } from './path'

export interface FolderEntry {
  workspace: TmuxWorkspace
  window: TmuxWindow
  /** Full cwd path. '' for the unknown bucket. */
  cwd: string
}

export interface FolderGroup {
  /** Full cwd path, or '' for the unknown bucket. */
  path: string
  /** Display label — `prettyCwd(path)` or '(no cwd)' for ''. */
  label: string
  entries: FolderEntry[]
}

/** The empty-cwd bucket's display label. Pinned to the bottom of
 * the sorted list so groups with a known path read first. */
const NO_CWD_LABEL = '(no cwd)'

/** Group every window in a host's workspaces by its (active pane's)
 * cwd path. Entries within each group are sorted by window id to
 * match the existing sidebar order. */
export function groupHostByFolder(hs: HostSessions): FolderGroup[] {
  const buckets = new Map<string, FolderEntry[]>()
  for (const ws of hs.workspaces.values()) {
    for (const win of ws.windows.values()) {
      const cwd = paneCwdFor(win.id, ws)
      const list = buckets.get(cwd) ?? []
      list.push({ workspace: ws, window: win, cwd })
      buckets.set(cwd, list)
    }
  }
  const groups: FolderGroup[] = []
  for (const [path, entries] of buckets) {
    entries.sort((a, b) => a.window.id.localeCompare(b.window.id))
    groups.push({
      path,
      label: path === '' ? NO_CWD_LABEL : prettyCwd(path),
      entries,
    })
  }
  groups.sort((a, b) => {
    // Empty-cwd bucket pinned to the bottom; otherwise alphabetical
    // by display label so the visible order matches the rendered text.
    if (a.path === '' && b.path !== '') return 1
    if (b.path === '' && a.path !== '') return -1
    return a.label.localeCompare(b.label)
  })
  return groups
}

/** Find the folder a given window belongs to within a host. Used by
 * the sidebar's "is this folder active?" check — a folder is active
 * when its host is active and the focused window is among its
 * entries. */
export function folderForWindow(
  hs: HostSessions | undefined,
  windowId: string,
): FolderGroup | undefined {
  if (!hs) return undefined
  for (const group of groupHostByFolder(hs)) {
    if (group.entries.some((e) => e.window.id === windowId)) return group
  }
  return undefined
}

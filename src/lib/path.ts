/**
 * Path helpers shared across the sidebar / palette / footer.
 */

import type { TmuxWorkspace } from './store'

/** Trim an absolute path to its trailing folder name, prefixed with
 * `/` for readability — `/Users/azhar/Code/foo/bar` → `/bar`. The
 * caller's `title` attribute carries the full path for hover. The
 * root directory collapses to a single `/`. */
export function prettyCwd(cwd: string): string {
  if (!cwd) return ''
  // Strip any trailing slash so `/foo/bar/` still resolves to `/bar`.
  const trimmed = cwd.replace(/\/+$/, '')
  if (trimmed === '') return '/'
  const idx = trimmed.lastIndexOf('/')
  if (idx < 0) return `/${trimmed}`
  return `/${trimmed.slice(idx + 1)}`
}

/** Resolve the cwd to display for a window. We prefer the active
 * pane's cwd; if no pane is marked active (transient state during
 * tmux events) fall back to the first pane in the window. Returns
 * '' when the window has no panes yet. */
export function paneCwdFor(windowId: string, w: TmuxWorkspace): string {
  const inWindow = [...w.panes.values()].filter((p) => p.windowId === windowId)
  const active = inWindow.find((p) => p.active) ?? inWindow[0]
  return active?.cwd ?? ''
}

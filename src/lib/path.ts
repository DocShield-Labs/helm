/**
 * Path helpers shared across the sidebar / palette / footer.
 */

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

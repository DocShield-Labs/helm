/**
 * RemotePathPicker — directory picker for any host (local or remote).
 *
 * UX: a breadcrumb at the top showing the current path with each
 * segment clickable to jump back; a single column underneath listing
 * the directory's subdirectories. Clicking a directory navigates into
 * it; clicking the breadcrumb's parent walks up.
 *
 * Each navigation is a `fs_list_dir` round-trip. Local hits are
 * sub-millisecond; remote hits go over SSH (typical 50–200 ms). We
 * cache results in an in-memory LRU keyed by `${hostId}::${path}` so
 * back-clicks are instant — the cache is dropped on unmount.
 *
 * The picker is a controlled component: parent owns the selected path
 * and feeds it back via `value`. We treat null / empty value as
 * "default to $HOME on the next listing."
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import { commands } from '@lib/ipc'
import type { DirEntry, DirListing, HostId } from '@bindings'

export interface RemotePathPickerProps {
  hostId: HostId | null
  value: string
  onChange: (path: string) => void
  /** Show entries whose name starts with `.` (e.g. `.config`).
   * Defaults to false to match Finder's default behavior. */
  showHidden?: boolean
}

/** Tiny LRU keyed by `hostId::path`. Cleared by un-mounting the
 * picker or by manual invalidation (host disconnect, schedule save).
 * Cap is per-instance so the picker doesn't accumulate entries across
 * a long session. */
const LRU_MAX = 64
function makeCache() {
  const map = new Map<string, DirListing>()
  return {
    get: (key: string): DirListing | undefined => {
      const v = map.get(key)
      if (v) {
        // Touch to LRU end.
        map.delete(key)
        map.set(key, v)
      }
      return v
    },
    set: (key: string, value: DirListing) => {
      if (map.has(key)) map.delete(key)
      map.set(key, value)
      while (map.size > LRU_MAX) {
        const oldest = map.keys().next().value
        if (oldest === undefined) break
        map.delete(oldest)
      }
    },
  }
}

export function RemotePathPicker({
  hostId,
  value,
  onChange,
  showHidden = false,
}: RemotePathPickerProps) {
  const cache = useRef(makeCache())
  const [listing, setListing] = useState<DirListing | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  // Free-text override — collapsed by default. Useful for power users
  // who already know the absolute path and don't want to click.
  const [textOpen, setTextOpen] = useState(false)
  const [textDraft, setTextDraft] = useState(value)

  // Cancel stale fetches: each navigation increments a token and only
  // the latest token's response is allowed to commit. Without this a
  // user clicking through 3 dirs faster than the network can respond
  // would race their listings out of order.
  const fetchToken = useRef(0)

  useEffect(() => {
    setTextDraft(value)
  }, [value])

  // Fetch whenever hostId or value changes. Empty value resolves to
  // $HOME server-side; we still pass it through so the cache key is
  // stable per-host.
  useEffect(() => {
    if (!hostId) {
      setListing(null)
      return
    }
    const key = `${hostId}::${value}`
    const cached = cache.current.get(key)
    if (cached) {
      setListing(cached)
      setError(null)
      setLoading(false)
      // Sync onChange to the canonical path so the parent shows the
      // resolved-tilde version even on a cache hit.
      if (cached.path !== value) onChange(cached.path)
      return
    }

    const token = ++fetchToken.current
    setLoading(true)
    setError(null)
    void (async () => {
      const res = await commands.fsListDir(hostId, value || null)
      // Stale response — a newer navigation has fired. Drop it.
      if (token !== fetchToken.current) return
      setLoading(false)
      if (res.status === 'ok') {
        cache.current.set(key, res.data)
        setListing(res.data)
        // Canonicalize the path the parent holds. Without this the
        // next click would re-fetch the un-resolved form.
        if (res.data.path !== value) onChange(res.data.path)
      } else {
        setError(res.error)
      }
    })()
  }, [hostId, value, onChange])

  const breadcrumb = useMemo(() => buildBreadcrumb(listing?.path ?? value), [listing, value])

  const visibleEntries = useMemo(() => {
    if (!listing) return [] as DirEntry[]
    return listing.entries
      .filter((e) => e.is_dir)
      .filter((e) => showHidden || !e.name.startsWith('.'))
  }, [listing, showHidden])

  return (
    <div className="flex flex-col gap-2">
      {/* Breadcrumb. Each segment except the last is clickable. */}
      <div className="flex flex-wrap items-center gap-x-1 gap-y-1 font-mono text-[12px] text-text-secondary">
        {breadcrumb.map((seg, i) => {
          const isLast = i === breadcrumb.length - 1
          return (
            <span key={seg.path} className="flex items-center gap-1">
              {i > 0 && <span className="text-text-disabled">/</span>}
              <button
                type="button"
                disabled={isLast}
                onClick={() => onChange(seg.path)}
                className={
                  isLast
                    ? 'text-text-primary'
                    : 'rounded-sm px-1 hover:bg-white/[0.04] hover:text-text-primary'
                }
              >
                {seg.label}
              </button>
            </span>
          )
        })}
      </div>

      {/* Listing column. */}
      <div
        className="rounded-md border border-white/[0.08] bg-sidebar"
        style={{ minHeight: 220, maxHeight: 240, overflowY: 'auto' }}
      >
        {!hostId && (
          <div className="px-3 py-3 text-[12px] text-text-tertiary">Pick a host first.</div>
        )}
        {hostId && error && (
          <div className="flex flex-col gap-1 px-3 py-3">
            <span className="text-[12px] text-status-error">{friendlyError(error)}</span>
            <span className="text-[11px] text-text-tertiary">
              Try the typed-path field below — it skips the directory query.
            </span>
          </div>
        )}
        {hostId && !error && loading && !listing && (
          <div className="px-3 py-3 text-[12px] text-text-tertiary">loading…</div>
        )}
        {hostId && !error && listing && (
          <ul className="flex flex-col py-1">
            {/* Up-arrow row when we're not at root. */}
            {listing.parent && (
              <li>
                <button
                  type="button"
                  onClick={() => onChange(listing.parent!)}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-left
                             font-mono text-[12px] text-text-secondary
                             hover:bg-white/[0.04] hover:text-text-primary"
                >
                  <span className="w-3 text-text-tertiary">↑</span>
                  <span>..</span>
                </button>
              </li>
            )}
            {visibleEntries.length === 0 && (
              <li className="px-3 py-1.5 text-[12px] text-text-tertiary">
                {listing.entries.length === 0
                  ? 'empty directory'
                  : 'no subdirectories'}
              </li>
            )}
            {visibleEntries.map((e) => (
              <li key={e.name}>
                <button
                  type="button"
                  onClick={() => onChange(joinPath(listing.path, e.name))}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-left
                             font-mono text-[12px] text-text-primary
                             hover:bg-white/[0.04]"
                >
                  <span className="w-3 text-text-tertiary">▸</span>
                  <span className="truncate">{e.name}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>

      {/* Typed-path entry. Promoted (always visible, never collapsed)
          when the listing is in error state, since the picker isn't
          giving the user anything else to act on. Otherwise behaves
          as a power-user escape hatch the user can opt into. */}
      <div className="flex flex-col gap-1">
        {!error && (
          <button
            type="button"
            onClick={() => setTextOpen((v) => !v)}
            className="self-start text-[11px] uppercase tracking-[0.06em] text-text-tertiary hover:text-text-secondary"
          >
            {textOpen ? '— hide path entry' : '+ type a path'}
          </button>
        )}
        {(textOpen || !!error) && (
          <div className="flex gap-2">
            <input
              type="text"
              value={textDraft}
              onChange={(e) => setTextDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  onChange(textDraft.trim())
                }
              }}
              placeholder="/Users/azhar/Code"
              className="h-8 flex-1 rounded-md border border-white/[0.08] bg-sidebar px-2
                         font-mono text-[12px] text-text-primary
                         focus:border-accent focus:outline-none"
            />
            <button
              type="button"
              onClick={() => onChange(textDraft.trim())}
              className="h-8 rounded-md border border-white/[0.08] bg-white/[0.04] px-3 text-[12px] hover:bg-white/[0.08]"
            >
              Go
            </button>
          </div>
        )}
      </div>
    </div>
  )
}

/** Map raw transport errors to a one-line message a user can act on.
 * Most importantly, the SSH `ConnectFailed` reason — which usually
 * means the remote ran out of session channels (typically OpenSSH's
 * `MaxSessions` exhausted by tmux + concurrent helpers) — gets a hint
 * pointing the user at the typed-path fallback. */
function friendlyError(raw: string): string {
  const lower = raw.toLowerCase()
  if (lower.includes('connectfailed') || lower.includes('open channel')) {
    return "Couldn't open a new SSH channel — the remote may be at its session limit."
  }
  if (lower.includes('host not connected')) {
    return 'Host is not connected yet. Connect first or type the path manually.'
  }
  if (lower.includes('not a directory')) {
    return 'That path is a file, not a folder.'
  }
  if (lower.includes('permission denied')) {
    return "Permission denied — your account can't list this directory."
  }
  if (lower.includes('no such file')) {
    return 'Path does not exist on the host.'
  }
  return raw
}

interface BreadcrumbSegment {
  label: string
  path: string
}

/** Split an absolute POSIX path into its breadcrumb segments. The
 * leading slash is rendered as `/`; subsequent segments accumulate
 * paths so each is independently clickable. */
function buildBreadcrumb(path: string): BreadcrumbSegment[] {
  if (!path) return [{ label: '/', path: '/' }]
  const parts = path.split('/').filter((p) => p.length > 0)
  const out: BreadcrumbSegment[] = [{ label: '/', path: '/' }]
  let acc = ''
  for (const p of parts) {
    acc += `/${p}`
    out.push({ label: p, path: acc })
  }
  return out
}

/** POSIX path join. The picker doesn't run on Windows hosts — Tauri
 * supports them but the helm tmux integration is unix-only. */
function joinPath(base: string, name: string): string {
  if (base.endsWith('/')) return `${base}${name}`
  return `${base}/${name}`
}

/**
 * Bounded ring of recently-invoked palette action ids, persisted to
 * `localStorage['helm.palette.recents']`. Pushed on every successful
 * palette run (not on keymap-driven runs — those don't go through the
 * palette's choice surface, so they shouldn't crowd the recents list).
 *
 * Storage shape: a JSON array of strings, most-recent first. Entries
 * are deduped on push (re-running an existing action moves it to the
 * head). The cap exists to keep the empty-state list compact and the
 * localStorage payload trivial.
 */

import { readJsonArray, writeJson } from '@lib/store'

const STORAGE_KEY = 'helm.palette.recents'
const MAX_ENTRIES = 12
/** How many recents the palette renders in its empty state. The Figma
 * mock shows ~4 rows; keeping the cap on what we *render* separate from
 * the cap on what we *store* lets the renderer change without churning
 * persisted data. */
export const RECENTS_DISPLAY_LIMIT = 4

const isString = (x: unknown): x is string => typeof x === 'string'

let cache: string[] | null = null

function read(): string[] {
  if (cache !== null) return cache
  cache = readJsonArray(STORAGE_KEY, isString).slice(0, MAX_ENTRIES)
  return cache
}

function write(ids: string[]) {
  cache = ids
  writeJson(STORAGE_KEY, ids)
}

export function getRecents(): readonly string[] {
  return read()
}

export function pushRecent(actionId: string): void {
  const cur = read()
  const next = [actionId, ...cur.filter((x) => x !== actionId)].slice(0, MAX_ENTRIES)
  write(next)
}

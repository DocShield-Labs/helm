/**
 * Per-session mirror of the daemon's terminal model.
 *
 * The daemon owns the grid and the line history (see PLAN.md, M8); this
 * module keeps the frontend's copy:
 *
 *   - the live grid (`grid`, `topLine`, cursor, modes), replaced by
 *     `Screen` events and patched by `ScreenDiff`s — the grid owns
 *     `topLine`;
 *   - history rows by absolute line, grown live by `HistoryAppend` and
 *     paged in backwards with `ensureHistory` as the user scrolls up;
 *     the loaded range `[loadedFrom, loadedTo)` is one contiguous run
 *     that `ensureHistory` keeps touching the grid;
 *   - "painters": whoever renders the live grid (the session's xterm)
 *     subscribes to receive each screen / diff in arrival order.
 *
 * Rows are immutable once they've scrolled out, which is what makes the
 * per-range caching in `useRows` safe. Everything lives outside zustand
 * so a 60 Hz diff stream re-renders only the components that asked for
 * the rows that changed.
 */

import { useCallback, useRef, useSyncExternalStore } from 'react'
import { commands } from '@lib/ipc'
import { MAX_HISTORY_PAGE, MODES } from '@bindings'
import type { CursorInfo, HistoryPage, HostId, RowAt, RowInfo, ScreenInfo } from '@bindings'
import { timed } from '@lib/perf'
import { addListener, notifyListeners } from './listeners'

export interface SessionScreen {
  /** A `Screen` has been applied at least once. */
  loaded: boolean
  cols: number
  rows: number
  /** Absolute line of grid row 0. */
  topLine: number
  /** Exactly `rows` entries once loaded. */
  grid: RowInfo[]
  cursor: CursorInfo
  modes: number
  /** Oldest line the daemon reported holding. */
  historyStart: number
  /** Loaded history rows by absolute line. */
  history: Map<number, RowInfo>
  /** Contiguous loaded range `[loadedFrom, loadedTo)`; null when empty. */
  loadedFrom: number | null
  loadedTo: number
  /** Bumped on any history change (page in, append, trim). */
  historyVersion: number
  /** Bumped on any grid change. */
  gridVersion: number
}

export type PaintEvent =
  | { kind: 'screen'; screen: ScreenInfo }
  | { kind: 'diff'; scroll: number; rows: RowAt[]; cursor: CursorInfo; modes: number }

const EMPTY_CURSOR: CursorInfo = { row: 0, col: 0, visible: true, shape: 'block', blink: false }
const EMPTY_ROW: RowInfo = { spans: [], wrapped: false }

/** Rows kept per session in the frontend before the oldest are dropped. */
const MAX_CLIENT_ROWS = 60_000

function empty(): SessionScreen {
  return {
    loaded: false,
    cols: 0,
    rows: 0,
    topLine: 0,
    grid: [],
    cursor: EMPTY_CURSOR,
    modes: 0,
    historyStart: 0,
    history: new Map(),
    loadedFrom: null,
    loadedTo: 0,
    historyVersion: 0,
    gridVersion: 0,
  }
}

const sessions = new Map<string, SessionScreen>()
const subs = new Map<string, Set<() => void>>()
const painters = new Map<string, Set<(ev: PaintEvent) => void>>()
const loading = new Map<string, Promise<void>>()
const key = (h: HostId, p: string) => `${h}::${p}`

function get(k: string): SessionScreen {
  let s = sessions.get(k)
  if (!s) {
    s = empty()
    sessions.set(k, s)
  }
  return s
}

// Notifications coalesce per session: heavy output delivers many diffs
// per frame, and re-rendering React row chunks for each one stalls
// input. The foreground session flushes at most every VISIBLE_MS
// (~30fps — plenty for streaming text, half the render/layout work of
// 60fps); everything else flushes every HIDDEN_MS — enough to keep
// previews honest without background sessions competing for the main
// thread. Which session is foreground is TOLD to this module
// (`setForeground`, called by the view that owns visibility) — the
// mirror layer deliberately knows nothing about the UI store.
// Subscribers read fresh state when they run, and a session becoming
// visible re-reads its snapshots on that very render, so batching
// delays paint, never data. (Synchronous where rAF doesn't exist:
// tests.)
const VISIBLE_MS = 33
const HIDDEN_MS = 250
const pendingNotify = new Set<string>()
let foregroundKey: string | null = null
let flushArmed = false
let lastVisibleFlush = 0
let lastAllFlush = 0

/** The session whose updates flush at full rate. */
export function setForeground(hostId: HostId, sessionId: string): void {
  foregroundKey = key(hostId, sessionId)
}

function armFlush(): void {
  if (flushArmed) return
  flushArmed = true
  requestAnimationFrame(() => {
    flushArmed = false
    const now = performance.now()
    const all = now - lastAllFlush >= HIDDEN_MS
    const visible = all || now - lastVisibleFlush >= VISIBLE_MS
    if (all) lastAllFlush = now
    if (visible) lastVisibleFlush = now
    for (const k of pendingNotify) {
      if (all || (visible && k === foregroundKey)) {
        pendingNotify.delete(k)
        timed(k === foregroundKey ? 'react-flush:visible' : 'react-flush:hidden', () =>
          notifyListeners(subs, k),
        )
      }
    }
    // Re-arm until drained; rAF suspends while the app is hidden, which
    // is fine — nobody is looking, and resume flushes immediately.
    if (pendingNotify.size > 0) armFlush()
  })
}

const notify = (k: string) => {
  if (typeof requestAnimationFrame !== 'function') {
    notifyListeners(subs, k)
    return
  }
  pendingNotify.add(k)
  armFlush()
}

// ---- inbound events (host.ts) ----

export function applyScreen(hostId: HostId, sessionId: string, screen: ScreenInfo): void {
  const k = key(hostId, sessionId)
  const s = get(k)
  s.loaded = true
  s.cols = screen.cols
  s.rows = screen.rows
  s.topLine = screen.top_line
  s.historyStart = screen.history_start
  s.grid = screen.lines
  s.cursor = screen.cursor
  s.modes = screen.modes
  s.gridVersion++
  for (const p of painters.get(k) ?? []) p({ kind: 'screen', screen })
  notify(k)
}

export function applyDiff(
  hostId: HostId,
  sessionId: string,
  topLine: number,
  scroll: number,
  rows: RowAt[],
  cursor: CursorInfo,
  modes: number,
): void {
  const k = key(hostId, sessionId)
  const s = get(k)
  if (!s.loaded) return // a full screen is on its way (or will be asked for)
  if (scroll > 0) {
    s.grid.splice(0, scroll)
    while (s.grid.length < s.rows) s.grid.push(EMPTY_ROW)
  }
  s.topLine = topLine
  for (const { index, row } of rows) {
    if (index < s.grid.length) s.grid[index] = row
  }
  s.cursor = cursor
  s.modes = modes
  s.gridVersion++
  for (const p of painters.get(k) ?? []) p({ kind: 'diff', scroll, rows, cursor, modes })
  notify(k)
}

/** Rows that left the grid. Extends the loaded range when contiguous
 * with (or overlapping) it, or starts one; a gap is left for
 * `ensureHistory` to fill. Overlap is real: a rows-grow pulls exported
 * rows back onto the grid, and when they scroll out again the daemon
 * re-exports them — possibly modified — starting below `loadedTo`.
 * Those rows upsert by line; `loadedTo` never moves backwards. */
export function applyHistoryAppend(
  hostId: HostId,
  sessionId: string,
  firstLine: number,
  rows: RowInfo[],
): void {
  const k = key(hostId, sessionId)
  const s = get(k)
  if (s.loadedFrom === null) {
    s.loadedFrom = firstLine
    s.loadedTo = firstLine
  }
  if (firstLine <= s.loadedTo && firstLine >= s.loadedFrom) {
    rows.forEach((r, i) => s.history.set(firstLine + i, r))
    s.loadedTo = Math.max(s.loadedTo, firstLine + rows.length)
    trim(s)
  }
  s.historyVersion++
  notify(k)
}

/** Drop the oldest loaded rows past the client cap. */
function trim(s: SessionScreen) {
  if (s.loadedFrom === null) return
  const excess = s.loadedTo - s.loadedFrom - MAX_CLIENT_ROWS
  if (excess <= 0) return
  for (let l = s.loadedFrom; l < s.loadedFrom + excess; l++) s.history.delete(l)
  s.loadedFrom += excess
}

// ---- reads ----

export function getSessionScreen(hostId: HostId, sessionId: string): SessionScreen {
  return get(key(hostId, sessionId))
}

export function screenInfoOf(s: SessionScreen): ScreenInfo {
  return {
    cols: s.cols,
    rows: s.rows,
    top_line: s.topLine,
    history_start: s.historyStart,
    lines: s.grid,
    cursor: s.cursor,
    modes: s.modes,
  }
}

export function subscribe(hostId: HostId, sessionId: string, cb: () => void): () => void {
  return addListener(subs, key(hostId, sessionId), cb)
}

/** The live grid's painter. Receives the current screen at once when
 * one is known, then every screen / diff in order. */
export function subscribePaint(
  hostId: HostId,
  sessionId: string,
  cb: (ev: PaintEvent) => void,
): () => void {
  const k = key(hostId, sessionId)
  const off = addListener(painters, k, cb)
  const s = get(k)
  if (s.loaded) cb({ kind: 'screen', screen: screenInfoOf(s) })
  return off
}

/** Row at an absolute line, from history or the grid. */
export function rowAt(s: SessionScreen, line: number): RowInfo | undefined {
  if (line >= s.topLine) return s.grid[line - s.topLine]
  return s.history.get(line)
}

/** Rows in `[from, to)` that are present; `[line, row]` pairs. */
export function rowsBetween(s: SessionScreen, from: number, to: number): Array<[number, RowInfo]> {
  const out: Array<[number, RowInfo]> = []
  for (let l = Math.max(0, from); l < to; l++) {
    const r = rowAt(s, l)
    if (r) out.push([l, r])
  }
  return out
}

/** Plain text of the last `n` rows (history + grid), trailing blank
 * grid rows skipped. */
export function tailText(s: SessionScreen, n: number): string {
  const lines: string[] = []
  let l = s.topLine + s.grid.length - 1
  while (l >= s.topLine && rowText(s.grid[l - s.topLine]).trim() === '') l--
  for (; l >= 0 && lines.length < n; l--) {
    const r = rowAt(s, l)
    if (!r) break
    lines.push(rowText(r))
  }
  return lines.reverse().join('\n')
}

export function rowText(r: RowInfo): string {
  let t = ''
  for (const sp of r.spans) t += sp.text
  return t
}

// ---- React hooks ----

/** Grid rows in use: through the last non-blank row (the cursor's row
 * when the grid is blank). What a session shows of the live grid; a bare
 * cursor on a blank row below the output is left out so the band a
 * running command occupies equals the rows its finished block gets. */
export function usedRows(s: SessionScreen): number {
  let last = s.grid.length - 1
  while (last >= 0 && s.grid[last].spans.length === 0) last--
  return last >= 0 ? last + 1 : Math.min(s.grid.length, s.cursor.row + 1)
}

/** Normal-screen agent extent. TUIs commonly repaint cleared rows as
 * styled spaces; those rows are layout noise, not visible content. */
export function agentUsedRows(s: SessionScreen): number {
  let last = s.grid.length - 1
  while (last >= 0 && !s.grid[last].spans.some((span) => /\S/.test(span.text))) last--
  return last >= 0 ? last + 1 : Math.min(s.grid.length, s.cursor.row + 1)
}

/** Absolute line just past the rendered document's last row: the used
 * extent, stretched to the cursor's row (a bare prompt still shows its
 * caret), clamped to the grid. The single definition — the live tail's
 * end, and the `clear` threshold, must agree or blocks vanish/linger. */
export function documentEnd(s: SessionScreen, agent: boolean): number {
  const used = agent ? agentUsedRows(s) : usedRows(s)
  return s.topLine + Math.max(used, Math.min(s.cursor.row + 1, s.rows))
}

/** Coarse subscription: re-renders when the grid's shape, its used
 * extent, or the history's loaded range changes — not on every cursor
 * move within the used rows. */
export interface ScreenMeta {
  loaded: boolean
  rows: number
  usedRows: number
  agentUsedRows: number
  topLine: number
  alt: boolean
  historyStart: number
  loadedFrom: number | null
  historyVersion: number
}

const metaCache = new Map<string, ScreenMeta>()

function metaOf(k: string): ScreenMeta {
  const s = get(k)
  const next: ScreenMeta = {
    loaded: s.loaded,
    rows: s.rows,
    usedRows: s.loaded ? usedRows(s) : 0,
    agentUsedRows: s.loaded ? agentUsedRows(s) : 0,
    topLine: s.topLine,
    alt: (s.modes & MODES.ALT_SCREEN) !== 0,
    historyStart: s.historyStart,
    loadedFrom: s.loadedFrom,
    historyVersion: s.historyVersion,
  }
  const prev = metaCache.get(k)
  if (prev && (Object.keys(next) as Array<keyof ScreenMeta>).every((f) => prev[f] === next[f])) {
    return prev
  }
  metaCache.set(k, next)
  return next
}

export function useScreenMeta(hostId: HostId, sessionId: string): ScreenMeta {
  const k = key(hostId, sessionId)
  const sub = useCallback((cb: () => void) => subscribe(hostId, sessionId, cb), [hostId, sessionId])
  return useSyncExternalStore(sub, () => metaOf(k))
}

/** The cursor as the DOM renders it: position by absolute line, plus
 * the shape/blink the application chose (DECSCUSR) so the DOM caret
 * honours them like the alt screen does. Cached, compared field-wise
 * BEFORE allocating — `getSnapshot` runs on every render. */
export interface DomCursor {
  /** Absolute line (`topLine + row`). */
  line: number
  col: number
  visible: boolean
  shape: CursorInfo['shape']
  blink: boolean
}

const cursorCache = new Map<string, DomCursor>()

function cursorOf(k: string): DomCursor {
  const s = get(k)
  const line = s.topLine + s.cursor.row
  const prev = cursorCache.get(k)
  if (
    prev &&
    prev.line === line &&
    prev.col === s.cursor.col &&
    prev.visible === s.cursor.visible &&
    prev.shape === s.cursor.shape &&
    prev.blink === s.cursor.blink
  ) {
    return prev
  }
  const next: DomCursor = {
    line,
    col: s.cursor.col,
    visible: s.cursor.visible,
    shape: s.cursor.shape,
    blink: s.cursor.blink,
  }
  cursorCache.set(k, next)
  return next
}

export function useCursor(hostId: HostId, sessionId: string): DomCursor {
  const k = key(hostId, sessionId)
  const sub = useCallback((cb: () => void) => subscribe(hostId, sessionId, cb), [hostId, sessionId])
  return useSyncExternalStore(sub, () => cursorOf(k))
}

/** Every change, including cursor moves — for the peek. */
export function useScreenVersion(hostId: HostId, sessionId: string): number {
  const k = key(hostId, sessionId)
  const sub = useCallback((cb: () => void) => subscribe(hostId, sessionId, cb), [hostId, sessionId])
  return useSyncExternalStore(sub, () => {
    const s = get(k)
    return s.gridVersion + s.historyVersion
  })
}

interface RowsCache {
  from: number
  to: number
  rows: Array<[number, RowInfo]>
  complete: boolean
  touchesGrid: boolean
  historyVersion: number
  gridVersion: number
}
const EMPTY_ROWS: Array<[number, RowInfo]> = []

/**
 * Rows in `[from, to)` as a referentially stable array while nothing in
 * the range changed. A complete range entirely in history is immutable
 * and never recomputes; ranges touching the grid follow the 60 Hz diff
 * stream. The cache lives with the component, so it dies with it.
 */
export function useRows(
  hostId: HostId,
  sessionId: string,
  from: number,
  to: number,
): Array<[number, RowInfo]> {
  const k = key(hostId, sessionId)
  const cache = useRef<RowsCache | null>(null)
  const sub = useCallback((cb: () => void) => subscribe(hostId, sessionId, cb), [hostId, sessionId])
  const snapshot = useCallback(() => {
    if (to <= from) return EMPTY_ROWS
    const s = get(k)
    const c = cache.current
    const touchesGrid = to > s.topLine
    if (c && c.from === from && c.to === to) {
      const frozen = c.complete && !touchesGrid && from >= (s.loadedFrom ?? Infinity)
      const fresh =
        c.historyVersion === s.historyVersion &&
        (c.gridVersion === s.gridVersion || (!touchesGrid && !c.touchesGrid))
      if (frozen || fresh) return c.rows
    }
    const rows = rowsBetween(s, from, to)
    cache.current = {
      from,
      to,
      rows,
      complete: rows.length === to - from,
      touchesGrid,
      historyVersion: s.historyVersion,
      gridVersion: s.gridVersion,
    }
    return rows
  }, [k, from, to])
  return useSyncExternalStore(sub, snapshot)
}

// ---- fetching ----

/** Make sure the session's grid is known (first paint after mount). */
export async function ensureScreen(hostId: HostId, sessionId: string): Promise<void> {
  const k = key(hostId, sessionId)
  if (get(k).loaded) return
  const lk = `${k}|screen`
  const inflight = loading.get(lk)
  if (inflight) return inflight
  const p = commands
    .sessionScreen(hostId, sessionId)
    .then((res) => {
      if (res.status === 'ok') applyScreen(hostId, sessionId, res.data)
    })
    .finally(() => loading.delete(lk))
  loading.set(lk, p)
  return p
}

/**
 * Page history in until rows from `fromLine` up to the grid are loaded
 * (or the daemon has nothing older). The gap touching the grid is
 * filled first, then older pages, so the rows nearest the viewport
 * arrive first. One fetch chain per session at a time; a call that finds
 * one running re-checks once it finishes.
 */
export function ensureHistory(hostId: HostId, sessionId: string, fromLine: number): Promise<void> {
  const k = key(hostId, sessionId)
  const inflight = loading.get(k)
  if (inflight) return inflight.then(() => ensureHistory(hostId, sessionId, fromLine))
  const p = fetchLoop(hostId, sessionId, fromLine).finally(() => loading.delete(k))
  loading.set(k, p)
  return p
}

/** The next unloaded range `[lo, hi)` to fetch, newest first. */
function nextGap(s: SessionScreen, fromLine: number): [number, number] | null {
  if (s.loadedFrom === null) {
    const lo = Math.max(0, fromLine)
    return s.topLine > lo ? [lo, s.topLine] : null
  }
  if (s.loadedTo < s.topLine) return [s.loadedTo, s.topLine]
  const floor = Math.max(fromLine, s.historyStart)
  if (s.loadedFrom > floor) return [Math.max(floor, s.loadedFrom - MAX_HISTORY_PAGE), s.loadedFrom]
  return null
}

/** Merge a page answering `[lo, hi)` into the loaded range. An empty
 * page still marks the asked-for range covered so paging converges. */
function insertPage(s: SessionScreen, lo: number, hi: number, page: HistoryPage): void {
  s.historyStart = page.history_start
  if (page.rows.length === 0) {
    if (s.loadedFrom === null) {
      s.loadedFrom = hi
      s.loadedTo = hi
    } else if (hi === s.loadedFrom) {
      s.loadedFrom = Math.min(s.loadedFrom, page.history_start)
    } else if (lo === s.loadedTo) {
      s.loadedTo = hi
    }
  } else {
    const first = page.from_line
    const end = first + page.rows.length
    page.rows.forEach((r, i) => s.history.set(first + i, r))
    if (s.loadedFrom === null || first > s.loadedTo) {
      s.loadedFrom = first
      s.loadedTo = end
    } else {
      s.loadedFrom = Math.min(s.loadedFrom, first)
      s.loadedTo = Math.max(s.loadedTo, end)
    }
    trim(s)
  }
  s.historyVersion++
}

async function fetchLoop(hostId: HostId, sessionId: string, fromLine: number): Promise<void> {
  const k = key(hostId, sessionId)
  for (let guard = 0; guard < 64; guard++) {
    const s = get(k)
    const gap = nextGap(s, fromLine)
    if (!gap) {
      if (s.loadedFrom === null) {
        // Nothing to page (the grid hasn't scrolled): an empty range at
        // the grid's top says "loaded, nothing there".
        s.loadedFrom = s.topLine
        s.loadedTo = s.topLine
        s.historyVersion++
        notify(k)
      }
      return
    }
    const [lo, hi] = gap
    const res = await commands.sessionHistory(hostId, sessionId, lo, hi)
    if (res.status !== 'ok') return
    insertPage(get(k), lo, hi, res.data)
    notify(k)
  }
}

// ---- lifecycle ----

/** Every per-session cache, so teardown can't miss one. A new derived
 * snapshot map must be added here or it leaks entries for dead sessions. */
function dropSession(k: string): void {
  sessions.delete(k)
  metaCache.delete(k)
  cursorCache.delete(k)
  pendingNotify.delete(k)
  notify(k)
}

export function dropHost(hostId: HostId): void {
  const prefix = `${hostId}::`
  for (const k of [...sessions.keys()]) {
    if (k.startsWith(prefix)) dropSession(k)
  }
}

/** Drop cached screens for sessions no longer present in a host tree. */
export function retainHostSessions(hostId: HostId, sessionIds: ReadonlySet<string>): void {
  const prefix = `${hostId}::`
  for (const k of [...sessions.keys()]) {
    if (!k.startsWith(prefix)) continue
    if (sessionIds.has(k.slice(prefix.length))) continue
    dropSession(k)
  }
}

/** Keystrokes / pasted text for a session. One place owns the wire
 * encoding (base64) for the outbound half. */
export function sendInput(hostId: HostId, sessionId: string, input: string | Uint8Array): Promise<void> {
  const bytes = typeof input === 'string' ? new TextEncoder().encode(input) : input
  return commands.sessionInput(hostId, sessionId, bytesToBase64(bytes)).then((res) => {
    if (res.status !== 'ok') console.warn('session_input failed:', res.error)
  })
}

function bytesToBase64(bytes: Uint8Array): string {
  let bin = ''
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000))
  }
  return btoa(bin)
}

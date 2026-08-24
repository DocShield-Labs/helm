/**
 * Per-session block tables, kept OUT of the main zustand store so block
 * churn (every command start/finish) re-renders only the session showing
 * it, never the sidebar.
 *
 * Fed by `SessionEvent.Block` / `ModeChange` / `PaneExited`; primed on
 * first view by `session_blocks` (historical blocks aren't streamed).
 */

import { useSyncExternalStore } from 'react'
import { commands } from '@lib/ipc'
import type { BlockInfo, HostId } from '@bindings'
import { addListener, notifyListeners } from './listeners'

export interface SessionBlocks {
  /** Sorted by start_line. */
  blocks: readonly BlockInfo[]
  altScreen: boolean
  exited: boolean
  /** `session_blocks` has been answered at least once. */
  loaded: boolean
  /** Count of bells the session has rung since the frontend started.
   * Consumers compare against the count they last acknowledged. */
  bells: number
  /** Blocks starting before this line are hidden (`clear`). */
  clearedBefore: number
}

const EMPTY: SessionBlocks = {
  blocks: [],
  altScreen: false,
  exited: false,
  loaded: false,
  bells: 0,
  clearedBefore: 0,
}

const sessions = new Map<string, SessionBlocks>()
const subs = new Map<string, Set<() => void>>()
const loading = new Map<string, Promise<void>>()
const hostLoadRevisions = new Map<string, number>()
const hostLoadSubs = new Map<string, Set<() => void>>()
const key = (h: HostId, p: string) => `${h}::${p}`

function update(k: string, f: (cur: SessionBlocks) => SessionBlocks): void {
  const cur = sessions.get(k) ?? EMPTY
  const next = f(cur)
  if (next === cur) return
  sessions.set(k, next)
  notifyListeners(subs, k)
}

function insertSorted(list: readonly BlockInfo[], block: BlockInfo): BlockInfo[] {
  const out = list.filter((b) => b.id !== block.id)
  let i = out.findIndex((b) => b.start_line > block.start_line)
  if (i < 0) i = out.length
  out.splice(i, 0, block)
  return out
}

export function upsertBlock(hostId: HostId, sessionId: string, block: BlockInfo): void {
  update(key(hostId, sessionId), (cur) => ({ ...cur, blocks: insertSorted(cur.blocks, block) }))
}

export function setAltScreen(hostId: HostId, sessionId: string, on: boolean): void {
  update(key(hostId, sessionId), (cur) => (cur.altScreen === on ? cur : { ...cur, altScreen: on }))
}

export function ringBell(hostId: HostId, sessionId: string): void {
  update(key(hostId, sessionId), (cur) => ({ ...cur, bells: cur.bells + 1 }))
}

/** `clear`: hide every block that started before `line`. The rows stay
 * in the daemon (search, history); only the list forgets them. */
export function clearBefore(hostId: HostId, sessionId: string, line: number): void {
  update(key(hostId, sessionId), (cur) => ({ ...cur, clearedBefore: Math.max(cur.clearedBefore, line) }))
}

export function setExited(hostId: HostId, sessionId: string): void {
  update(key(hostId, sessionId), (cur) => (cur.exited ? cur : { ...cur, exited: true }))
}

export function getSessionBlocks(hostId: HostId, sessionId: string): SessionBlocks {
  return sessions.get(key(hostId, sessionId)) ?? EMPTY
}

export function subscribe(hostId: HostId, sessionId: string, cb: () => void): () => void {
  return addListener(subs, key(hostId, sessionId), cb)
}

/** React hook: the session's block table, re-rendering on change. */
export function useSessionBlocks(hostId: HostId, sessionId: string): SessionBlocks {
  return useSyncExternalStore(
    (cb) => subscribe(hostId, sessionId, cb),
    () => getSessionBlocks(hostId, sessionId),
    () => EMPTY,
  )
}

/** Re-render host-level summaries when historical blocks finish loading.
 * Live block events already update the main store's running state. */
export function useHostBlockLoadRevision(hostId: HostId): number {
  return useSyncExternalStore(
    (cb) => addListener(hostLoadSubs, hostId, cb),
    () => hostLoadRevisions.get(hostId) ?? 0,
    () => 0,
  )
}

/**
 * Fetch the daemon's retained block table once per session. Live upserts
 * that raced ahead are preserved (merged by id).
 */
export function ensureLoaded(hostId: HostId, sessionId: string): Promise<void> {
  const k = key(hostId, sessionId)
  if (sessions.get(k)?.loaded) return Promise.resolve()
  const inflight = loading.get(k)
  if (inflight) return inflight
  const p = commands
    .sessionBlocks(hostId, sessionId)
    .then((res) => {
      update(k, (cur) => {
        if (res.status !== 'ok') return { ...cur, loaded: true }
        let blocks: BlockInfo[] = [...res.data].sort((a, b) => a.start_line - b.start_line)
        for (const live of cur.blocks) blocks = insertSorted(blocks, live)
        return { ...cur, blocks, loaded: true }
      })
      hostLoadRevisions.set(hostId, (hostLoadRevisions.get(hostId) ?? 0) + 1)
      notifyListeners(hostLoadSubs, hostId)
    })
    .finally(() => loading.delete(k))
  loading.set(k, p)
  return p
}

export function dropHost(hostId: HostId): void {
  const prefix = `${hostId}::`
  for (const k of [...sessions.keys()]) {
    if (k.startsWith(prefix)) {
      sessions.delete(k)
      notifyListeners(subs, k)
    }
  }
  hostLoadRevisions.delete(hostId)
  notifyListeners(hostLoadSubs, hostId)
}

/** Drop cached block tables for sessions no longer present in a host tree. */
export function retainHostSessions(hostId: HostId, sessionIds: ReadonlySet<string>): void {
  const prefix = `${hostId}::`
  for (const k of [...sessions.keys()]) {
    if (!k.startsWith(prefix)) continue
    const sessionId = k.slice(prefix.length)
    if (sessionIds.has(sessionId)) continue
    sessions.delete(k)
    loading.delete(k)
    notifyListeners(subs, k)
  }
}

/** A block whose command has been accepted but not finished. */
export function isRunning(b: BlockInfo): boolean {
  return b.cmd_line !== null && b.end_line === null
}

/** Where a block's *output* starts: the line after the prompt and the
 * echoed command when the markers were seen, else the block start. */
export function bodyStartLine(b: BlockInfo): number {
  return b.output_line ?? b.cmd_line ?? b.start_line
}

/** A block's output rows, `[bodyStart, end)`; empty while running. */
export function blockRange(b: BlockInfo): [number, number] {
  const start = bodyStartLine(b)
  return [start, b.end_line ?? start]
}

/** Finished blocks with something to show (a command line or output). */
export function isRenderable(b: BlockInfo): boolean {
  if (b.end_line === null) return false
  return (b.cmdline !== null && b.cmdline !== '') || b.end_line > bodyStartLine(b)
}

/** The most recently finished block, without copying the list. */
export function lastFinished(blocks: readonly BlockInfo[]): BlockInfo | undefined {
  for (let i = blocks.length - 1; i >= 0; i--) {
    if (blocks[i].end_line !== null) return blocks[i]
  }
  return undefined
}

// ---- jump-to-block (palette search → session) ----
//
// A pending jump lives here, not in a DOM event: the target session may
// not be mounted (or its blocks loaded) yet when the palette fires, so
// the session consumes it whenever it is ready.

export interface PendingJump {
  hostId: HostId
  sessionId: string
  blockId: string | null
  /** Absolute line to scroll to. */
  line: number
}

let pendingJump: PendingJump | null = null
const jumpSubs = new Set<() => void>()

export function requestJump(j: PendingJump): void {
  pendingJump = j
  for (const cb of jumpSubs) cb()
}

/** The pending jump for this session, or null. Re-renders when one arrives. */
export function usePendingJump(hostId: HostId, sessionId: string): PendingJump | null {
  return useSyncExternalStore(
    (cb) => {
      jumpSubs.add(cb)
      return () => jumpSubs.delete(cb)
    },
    () =>
      pendingJump && pendingJump.hostId === hostId && pendingJump.sessionId === sessionId
        ? pendingJump
        : null,
    () => null,
  )
}

export function consumeJump(j: PendingJump): void {
  if (pendingJump === j) {
    pendingJump = null
    for (const cb of jumpSubs) cb()
  }
}

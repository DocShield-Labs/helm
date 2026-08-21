/**
 * Per-pane block tables, kept OUT of the main zustand store so block
 * churn (every command start/finish) re-renders only the pane showing
 * it, never the sidebar.
 *
 * Fed by `SessionEvent.Block` / `ModeChange` / `PaneExited`; primed on
 * first view by `session_blocks` (historical blocks aren't streamed).
 */

import { useSyncExternalStore } from 'react'
import { commands } from '@lib/ipc'
import type { BlockInfo, HostId } from '@bindings'

export interface PaneBlocks {
  /** Sorted by start_seq. */
  blocks: readonly BlockInfo[]
  altScreen: boolean
  exited: boolean
  /** `session_blocks` has been answered at least once. */
  loaded: boolean
}

const EMPTY: PaneBlocks = { blocks: [], altScreen: false, exited: false, loaded: false }

const panes = new Map<string, PaneBlocks>()
const subs = new Map<string, Set<() => void>>()
const loading = new Map<string, Promise<void>>()
const key = (h: HostId, p: string) => `${h}::${p}`

function update(k: string, f: (cur: PaneBlocks) => PaneBlocks): void {
  const cur = panes.get(k) ?? EMPTY
  const next = f(cur)
  if (next === cur) return
  panes.set(k, next)
  const set = subs.get(k)
  if (set) for (const cb of set) cb()
}

function insertSorted(list: readonly BlockInfo[], block: BlockInfo): BlockInfo[] {
  const out = list.filter((b) => b.id !== block.id)
  let i = out.findIndex((b) => b.start_seq > block.start_seq)
  if (i < 0) i = out.length
  out.splice(i, 0, block)
  return out
}

export function upsertBlock(hostId: HostId, paneId: string, block: BlockInfo): void {
  update(key(hostId, paneId), (cur) => ({ ...cur, blocks: insertSorted(cur.blocks, block) }))
}

export function setAltScreen(hostId: HostId, paneId: string, on: boolean): void {
  update(key(hostId, paneId), (cur) => (cur.altScreen === on ? cur : { ...cur, altScreen: on }))
}

export function setExited(hostId: HostId, paneId: string): void {
  update(key(hostId, paneId), (cur) => (cur.exited ? cur : { ...cur, exited: true }))
}

export function getPaneBlocks(hostId: HostId, paneId: string): PaneBlocks {
  return panes.get(key(hostId, paneId)) ?? EMPTY
}

export function subscribe(hostId: HostId, paneId: string, cb: () => void): () => void {
  const k = key(hostId, paneId)
  let set = subs.get(k)
  if (!set) {
    set = new Set()
    subs.set(k, set)
  }
  set.add(cb)
  return () => {
    set?.delete(cb)
    if (set?.size === 0) subs.delete(k)
  }
}

/** React hook: the pane's block table, re-rendering on change. */
export function usePaneBlocks(hostId: HostId, paneId: string): PaneBlocks {
  return useSyncExternalStore(
    (cb) => subscribe(hostId, paneId, cb),
    () => getPaneBlocks(hostId, paneId),
    () => EMPTY,
  )
}

/**
 * Fetch the daemon's retained block table once per pane. Live upserts
 * that raced ahead are preserved (merged by id).
 */
export function ensureLoaded(hostId: HostId, paneId: string): Promise<void> {
  const k = key(hostId, paneId)
  if (panes.get(k)?.loaded) return Promise.resolve()
  const inflight = loading.get(k)
  if (inflight) return inflight
  const p = commands
    .sessionBlocks(hostId, paneId)
    .then((res) => {
      update(k, (cur) => {
        if (res.status !== 'ok') return { ...cur, loaded: true }
        let blocks: BlockInfo[] = [...res.data].sort((a, b) => a.start_seq - b.start_seq)
        for (const live of cur.blocks) blocks = insertSorted(blocks, live)
        return { ...cur, blocks, loaded: true }
      })
    })
    .finally(() => loading.delete(k))
  loading.set(k, p)
  return p
}

export function dropHost(hostId: HostId): void {
  const prefix = `${hostId}::`
  for (const k of [...panes.keys()]) {
    if (k.startsWith(prefix)) {
      panes.delete(k)
      const set = subs.get(k)
      if (set) for (const cb of set) cb()
    }
  }
}

/** A block whose command has been accepted but not finished. */
export function isRunning(b: BlockInfo): boolean {
  return b.cmd_seq !== null && b.end_seq === null
}

/** Where a block's *output* starts in the stream: after the prompt and
 * the echoed command when the markers were seen, else the block start. */
export function bodyStartSeq(b: BlockInfo): number {
  return b.output_seq ?? b.cmd_seq ?? b.start_seq
}

/** Finished blocks with something to show (a command line or output). */
export function isRenderable(b: BlockInfo): boolean {
  if (b.end_seq === null) return false
  return (b.cmdline !== null && b.cmdline !== '') || b.end_seq > bodyStartSeq(b)
}

/** The most recently finished block, without copying the list. */
export function lastFinished(blocks: readonly BlockInfo[]): BlockInfo | undefined {
  for (let i = blocks.length - 1; i >= 0; i--) {
    if (blocks[i].end_seq !== null) return blocks[i]
  }
  return undefined
}

// ---- jump-to-block (palette search → pane) ----
//
// A pending jump lives here, not in a DOM event: the target pane may
// not be mounted (or its blocks loaded) yet when the palette fires, so
// the pane consumes it whenever it is ready.

export interface PendingJump {
  hostId: HostId
  paneId: string
  blockId: string | null
  lineSeq: number
}

let pendingJump: PendingJump | null = null
const jumpSubs = new Set<() => void>()

export function requestJump(j: PendingJump): void {
  pendingJump = j
  for (const cb of jumpSubs) cb()
}

/** The pending jump for this pane, or null. Re-renders when one arrives. */
export function usePendingJump(hostId: HostId, paneId: string): PendingJump | null {
  return useSyncExternalStore(
    (cb) => {
      jumpSubs.add(cb)
      return () => jumpSubs.delete(cb)
    },
    () =>
      pendingJump && pendingJump.hostId === hostId && pendingJump.paneId === paneId
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


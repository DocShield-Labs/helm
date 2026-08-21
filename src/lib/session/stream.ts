/**
 * Per-pane output streams: the bridge between `SessionEvent.Output`
 * frames and everything that renders bytes.
 *
 * Each pane owns a `SeqBuffer`. Frames are applied in seq order; gaps
 * trigger a replay request from the last contiguous point. Consumers:
 *
 *   - the live tail (xterm) subscribes and receives bytes as the
 *     contiguous head advances;
 *   - finished blocks read `[start, end)` slices on demand;
 *   - the notification peek reads the last few KB.
 *
 * History for a pane is fetched once (`ensureHistory`) — the daemon
 * replays the most recent window of scrollback, which lands here like
 * any other frame.
 */

import { commands } from '@lib/ipc'
import type { HostId } from '@bindings'
import { base64ToBytes, bytesToBase64 } from './bytes'
import { SeqBuffer } from './seqbuffer'

export type TailListener = (bytes: Uint8Array, seq: number) => void

interface PaneStream {
  buf: SeqBuffer
  historyRequested: boolean
  replayDone: boolean
  replayWaiters: Array<() => void>
  pendingGap: number | null
  gapTimer: number | null
  tail: Set<TailListener>
}

/** How much history to pull when a pane is first shown. */
const HISTORY_BYTES = 512 * 1024

const streams = new Map<string, PaneStream>()
const key = (h: HostId, p: string) => `${h}::${p}`

function get(hostId: HostId, paneId: string): PaneStream {
  const k = key(hostId, paneId)
  let s = streams.get(k)
  if (!s) {
    s = {
      buf: new SeqBuffer(),
      historyRequested: false,
      replayDone: false,
      replayWaiters: [],
      pendingGap: null,
      gapTimer: null,
      tail: new Set(),
    }
    streams.set(k, s)
  }
  return s
}

/** Apply one `Output` frame. */
export function applyOutput(hostId: HostId, paneId: string, seq: number, dataB64: string): void {
  const s = get(hostId, paneId)
  const bytes = base64ToBytes(dataB64)
  const r = s.buf.apply(seq, bytes)
  if (r.appended && r.appended.length > 0) {
    const startSeq = s.buf.head - r.appended.length
    for (const fn of s.tail) fn(r.appended, startSeq)
  }
  if (r.gapFrom !== null && s.pendingGap === null) {
    s.pendingGap = r.gapFrom
    // Coalesce: a burst of out-of-order frames should cost one replay.
    s.gapTimer = window.setTimeout(() => {
      s.gapTimer = null
      const from = s.pendingGap
      if (from === null) return
      void commands.sessionReplay(hostId, paneId, from, null)
    }, 20)
  }
}

/** `ReplayDone` — the daemon finished a replay burst for this pane. */
export function onReplayDone(hostId: HostId, paneId: string): void {
  const s = get(hostId, paneId)
  s.replayDone = true
  s.pendingGap = null
  const waiters = s.replayWaiters
  s.replayWaiters = []
  for (const w of waiters) w()
}

/**
 * Make sure the pane's recent history has been requested; resolves
 * once the first replay completes (or immediately if it already has).
 */
export function ensureHistory(hostId: HostId, paneId: string): Promise<void> {
  const s = get(hostId, paneId)
  if (s.replayDone) return Promise.resolve()
  const p = new Promise<void>((resolve) => s.replayWaiters.push(resolve))
  if (!s.historyRequested) {
    s.historyRequested = true
    void commands.sessionReplay(hostId, paneId, null, HISTORY_BYTES).then((res) => {
      if (res.status !== 'ok') {
        // Nothing will ever come — don't leave callers hanging.
        onReplayDone(hostId, paneId)
      }
    })
  }
  return p
}

export function subscribeTail(hostId: HostId, paneId: string, fn: TailListener): () => void {
  const s = get(hostId, paneId)
  s.tail.add(fn)
  return () => {
    s.tail.delete(fn)
  }
}

/** Bytes in `[from, to)` if retained and contiguous, else null. */
export function slice(hostId: HostId, paneId: string, from: number, to: number): Uint8Array | null {
  return get(hostId, paneId).buf.slice(from, to)
}

export function head(hostId: HostId, paneId: string): number {
  return get(hostId, paneId).buf.head
}

export function start(hostId: HostId, paneId: string): number {
  return get(hostId, paneId).buf.start
}

/** Forget every pane on a host (disconnect / removal). */
export function dropHost(hostId: HostId): void {
  const prefix = `${hostId}::`
  for (const [k, s] of streams) {
    if (!k.startsWith(prefix)) continue
    if (s.gapTimer !== null) window.clearTimeout(s.gapTimer)
    streams.delete(k)
  }
}

/** Keystrokes / pasted text for a pane. One place owns the wire
 * encoding (base64) for the outbound half, as `applyOutput` does for
 * the inbound half. */
export function sendInput(hostId: HostId, paneId: string, input: string | Uint8Array): Promise<void> {
  const bytes = typeof input === 'string' ? new TextEncoder().encode(input) : input
  return commands.sessionInput(hostId, paneId, bytesToBase64(bytes)).then((res) => {
    if (res.status !== 'ok') console.warn('session_input failed:', res.error)
  })
}

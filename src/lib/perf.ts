/**
 * Dev-only main-thread telemetry.
 *
 * WKWebView has no attachable profiler from the CLI, so the frontend
 * measures itself: named sections accumulate into buckets, a rAF
 * watchdog counts long frames, and every couple of seconds the summary
 * ships to the dev process stdout via the `perf_report` command. Inert
 * in release builds (`import.meta.env.DEV` gates everything).
 *
 * `timed('react-flush', …)` around a store notification captures the
 * React render + commit + layout effects it triggers — React 18 runs
 * external-store re-renders synchronously inside the callback.
 */

import { commands } from '@lib/ipc'

const enabled = import.meta.env.DEV && typeof window !== 'undefined'

interface Bucket {
  total: number
  count: number
  max: number
}

const buckets = new Map<string, Bucket>()
let longFrames = 0
let veryLongFrames = 0
let worstGap = 0
let depth = 0

export function timed<T>(name: string, fn: () => T): T {
  if (!enabled) return fn()
  // Nested sections would double-count into the frame budget; only the
  // outermost owns the wall clock, inner ones still get their bucket.
  const t0 = performance.now()
  depth++
  try {
    return fn()
  } finally {
    depth--
    const dt = performance.now() - t0
    const b = buckets.get(name) ?? { total: 0, count: 0, max: 0 }
    b.total += dt
    b.count++
    b.max = Math.max(b.max, dt)
    buckets.set(name, b)
    if (depth === 0) schedule()
  }
}

if (enabled && typeof requestAnimationFrame === 'function') {
  let last = performance.now()
  const tick = () => {
    const now = performance.now()
    const gap = now - last
    // rAF suspends while the window is hidden; a resume gap is not a
    // stall. 10s sanity bound for sleeps/occlusion the event misses.
    if (gap > 50 && gap < 10_000 && document.visibilityState === 'visible') {
      longFrames++
      worstGap = Math.max(worstGap, gap)
      if (gap > 200) veryLongFrames++
      schedule()
    }
    last = now
    requestAnimationFrame(tick)
  }
  requestAnimationFrame(tick)
  document.addEventListener('visibilitychange', () => {
    last = performance.now()
  })
}

let timer: number | null = null

function schedule(): void {
  if (timer !== null) return
  timer = window.setTimeout(flush, 2000)
}

function flush(): void {
  timer = null
  if (buckets.size === 0 && longFrames === 0) return
  const parts: string[] = []
  for (const [name, b] of [...buckets.entries()].sort((a, z) => z[1].total - a[1].total)) {
    parts.push(`${name}=${b.total.toFixed(0)}ms/${b.count}x max${b.max.toFixed(0)}`)
  }
  parts.push(`frames>50ms=${longFrames} >200ms=${veryLongFrames} worst=${worstGap.toFixed(0)}ms`)
  buckets.clear()
  longFrames = 0
  veryLongFrames = 0
  worstGap = 0
  void commands.perfReport(parts.join(' | ')).catch(() => {})
}

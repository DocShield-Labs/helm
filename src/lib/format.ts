/** Format a duration with one-decimal precision under a minute, then
 * coarser units above. Sub-minute is where users care about tenths
 * (e.g. did the test take 0.4s or 4.0s); past a minute the extra digit
 * is just noise. */
export function formatDuration(ms: number): string {
  const safe = Math.max(0, ms)
  if (safe < 60_000) return `${(safe / 1000).toFixed(1)}s`
  const sec = Math.floor(safe / 1000)
  const m = Math.floor(sec / 60)
  if (m < 60) {
    const s = sec % 60
    return `${m}m ${s}s`
  }
  const h = Math.floor(m / 60)
  const mm = m % 60
  return `${h}h ${mm}m`
}

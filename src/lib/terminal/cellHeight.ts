/**
 * Making xterm's rows exactly as tall as the DOM rows.
 *
 * The live band (xterm) sits in the same column as finished blocks
 * (DOM rows, `--helm-line-px` tall), and SessionView relies on the two
 * agreeing: a command that finishes turns its band into block rows
 * without anything moving. xterm's `lineHeight` option can't be set
 * to hit that directly — it multiplies the font's *measured* natural
 * height, not `fontSize`, and then rounds in device pixels:
 *
 *     charDev = ceil(naturalHeight · dpr)
 *     cellDev = floor(charDev · lineHeight)
 *     cell    = cellDev / dpr                      (CSS px)
 *
 * A 13px monospace font is ~16px tall, so the old `20 / 13` produced
 * 24.5px cells at 2× DPR. Rather than replicate the measurement, which
 * depends on the font being loaded, we measure the cell xterm actually
 * produced, recover `charDev` from it, and solve for the `lineHeight`
 * that lands `cellDev` on the target. One correction converges.
 */

/**
 * The `lineHeight` that makes xterm's cell `targetPx` tall, given the
 * cell it currently renders (`measuredPx`) under `lineHeight`. `null`
 * when it already does, or when the inputs can't be trusted (nothing
 * rendered yet, hidden session).
 */
export function correctedLineHeight(
  measuredPx: number,
  lineHeight: number,
  dpr: number,
  targetPx: number,
): number | null {
  if (!(measuredPx > 0) || !(lineHeight > 0) || !(dpr > 0) || !(targetPx > 0)) return null
  const cellDev = Math.round(measuredPx * dpr)
  // cellDev = floor(charDev · lineHeight) ⇒ charDev ∈ [cellDev/lh, (cellDev+1)/lh);
  // for lineHeight > 1 that interval holds exactly one integer.
  const charDev = Math.ceil(cellDev / lineHeight - 1e-6)
  if (charDev <= 0) return null
  const targetDev = Math.round(targetPx * dpr)
  if (targetDev === cellDev) return null
  // A hair above the exact ratio so the floor can't fall one short.
  const next = (targetDev + 1e-3) / charDev
  return Math.abs(next - lineHeight) < 1e-6 ? null : next
}

/** `--helm-line-px` as a number (the DOM row height), `fallback` if unset. */
export function domLinePx(fallback = 20): number {
  if (typeof document === 'undefined') return fallback
  const v = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--helm-line-px'))
  return Number.isFinite(v) && v > 0 ? v : fallback
}

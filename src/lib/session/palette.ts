/**
 * Colour resolution for rows rendered as DOM. The daemon packs a cell
 * colour into one number (see `helm-domain::SpanInfo`): -1 = default,
 * 0..=255 = indexed, >= TRUECOLOR_FLAG = truecolor
 * `TRUECOLOR_FLAG | (r<<16) | (g<<8) | b`.
 *
 * The 16 ANSI slots resolve through the `--ansi-N` CSS variables the
 * theme picker sets (`applyThemeCssVars`), so DOM rows and the xterm
 * grid always agree on a theme's palette; the 240-colour cube and
 * greys are fixed by the standard.
 */

import { TRUECOLOR_FLAG } from '@bindings'

function unpackRgb(packed: number): [number, number, number] {
  const v = packed - TRUECOLOR_FLAG
  return [(v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff]
}

/** CSS colour for a packed colour, or null for the default. */
export function colorCss(packed: number): string | null {
  if (packed < 0) return null
  if (packed < 16) return `var(--ansi-${packed})`
  if (packed < 232) {
    const i = packed - 16
    const r = Math.floor(i / 36)
    const g = Math.floor((i % 36) / 6)
    const b = i % 6
    const v = (c: number) => (c === 0 ? 0 : 55 + c * 40)
    return `rgb(${v(r)},${v(g)},${v(b)})`
  }
  if (packed < 256) {
    const gray = 8 + (packed - 232) * 10
    return `rgb(${gray},${gray},${gray})`
  }
  const [r, g, b] = unpackRgb(packed)
  return `rgb(${r},${g},${b})`
}

/** SGR parameter list that reproduces a packed colour in a terminal. */
export function colorSgr(packed: number, bg: boolean): string {
  const base = bg ? 48 : 38
  if (packed < 0) return bg ? '49' : '39'
  if (packed < 256) return `${base};5;${packed}`
  const [r, g, b] = unpackRgb(packed)
  return `${base};2;${r};${g};${b}`
}

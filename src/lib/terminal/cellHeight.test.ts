import { describe, expect, test } from 'bun:test'
import { correctedLineHeight } from './cellHeight'

/** xterm's cell arithmetic (see cellHeight.ts), in CSS px. */
function xtermCell(naturalHeight: number, lineHeight: number, dpr: number): number {
  const charDev = Math.ceil(naturalHeight * dpr)
  return Math.floor(charDev * lineHeight) / dpr
}

describe('correctedLineHeight', () => {
  test('the trace: 24.5px cells at 2× from lineHeight 20/13 become 20px', () => {
    const before = xtermCell(15.6, 20 / 13, 2)
    expect(before).toBe(24.5)
    const lh = correctedLineHeight(before, 20 / 13, 2, 20)
    expect(lh).not.toBeNull()
    expect(xtermCell(15.6, lh!, 2)).toBe(20)
  })

  test('converges: a second pass changes nothing', () => {
    const lh = correctedLineHeight(24.5, 20 / 13, 2, 20)!
    expect(correctedLineHeight(xtermCell(15.6, lh, 2), lh, 2, 20)).toBeNull()
  })

  test('works at 1× and for other font heights', () => {
    for (const [natural, dpr] of [
      [15.6, 1],
      [15.0, 2],
      [17.2, 2],
      [16.0, 1.5],
    ] as const) {
      const start = 20 / 13
      const lh = correctedLineHeight(xtermCell(natural, start, dpr), start, dpr, 20)
      expect(lh).not.toBeNull()
      expect(xtermCell(natural, lh!, dpr)).toBe(20)
    }
  })

  test('nothing to do when the cell is already on target', () => {
    expect(correctedLineHeight(20, 1.25, 2, 20)).toBeNull()
  })

  test('ignores an unrendered (0px) or hidden measurement', () => {
    expect(correctedLineHeight(0, 1.25, 2, 20)).toBeNull()
    expect(correctedLineHeight(NaN, 1.25, 2, 20)).toBeNull()
  })
})

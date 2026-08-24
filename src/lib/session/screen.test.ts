import { describe, expect, test } from 'bun:test'
import type { RowInfo } from '@bindings'
import {
  applyDiff,
  applyHistoryAppend,
  applyScreen,
  agentUsedRows,
  getSessionScreen,
  rowAt,
  rowsBetween,
  subscribePaint,
  tailText,
  usedRows,
} from './screen'
import { joinWrapped, rowsToText } from '@features/shell/Rows'

const H = 'host-a'

function r(text: string, wrapped = false): RowInfo {
  return { spans: text ? [{ text, fg: -1, bg: -1, attrs: 0, link: null }] : [], wrapped }
}

function screenOf(topLine: number, lines: string[]) {
  return {
    cols: 10,
    rows: lines.length,
    top_line: topLine,
    history_start: 0,
    lines: lines.map((t) => r(t)),
    cursor: { row: 0, col: 0, visible: true, shape: 'block' as const, blink: false },
    modes: 0,
  }
}

const cursor = { row: 2, col: 1, visible: true, shape: 'block' as const, blink: false }

function texts(s: ReturnType<typeof getSessionScreen>, from: number, to: number) {
  return rowsBetween(s, from, to).map(([l, row]) => [l, row.spans[0]?.text ?? ''])
}

describe('session screen mirror', () => {
  test('agent rows ignore styled whitespace left below normal-screen content', () => {
    const p = 'used-rows'
    applyScreen(H, p, screenOf(0, ['output', 'status', '          ', '']))
    const s = getSessionScreen(H, p)
    s.cursor = { ...s.cursor, row: 3 }

    expect(agentUsedRows(s)).toBe(2)
    expect(usedRows(s)).toBe(3)
  })

  test('a wholly blank grid still includes its cursor row', () => {
    const p = 'blank-grid'
    applyScreen(H, p, screenOf(0, ['   ', '', '']))
    const s = getSessionScreen(H, p)
    s.cursor = { ...s.cursor, row: 1 }

    expect(agentUsedRows(s)).toBe(2)
    expect(usedRows(s)).toBe(1)
  })

  test('a scrolling diff shifts the grid; appends fill history', () => {
    const p = 'p1'
    applyScreen(H, p, screenOf(0, ['a', 'b', '']))
    let s = getSessionScreen(H, p)
    expect(s.loaded).toBe(true)
    expect(rowAt(s, 1)?.spans[0].text).toBe('b')

    // Two rows scroll out: history gets them, the grid shifts up by two
    // and only the rows that differ from the shifted frame arrive.
    applyHistoryAppend(H, p, 0, [r('a'), r('b')])
    applyDiff(H, p, 2, 2, [{ index: 1, row: r('c') }, { index: 2, row: r('d') }], cursor, 0)
    s = getSessionScreen(H, p)
    expect(s.topLine).toBe(2)
    expect([s.loadedFrom, s.loadedTo]).toEqual([0, 2])
    expect(texts(s, 0, 5)).toEqual([
      [0, 'a'],
      [1, 'b'],
      [2, ''],
      [3, 'c'],
      [4, 'd'],
    ])
    expect(tailText(s, 3)).toBe('\nc\nd')

    // An unscrolled diff patches in place.
    applyDiff(H, p, 2, 0, [{ index: 0, row: r('x') }], cursor, 0)
    expect(texts(getSessionScreen(H, p), 2, 3)).toEqual([[2, 'x']])
  })

  test('an agent transcript is stable as live rows cross into history', () => {
    const p = 'agent-resize'
    applyHistoryAppend(H, p, 0, [r('notice')])
    applyScreen(H, p, screenOf(1, ['first', 'second', 'footer']))
    const before = getSessionScreen(H, p)
    expect(texts(before, 0, before.topLine + agentUsedRows(before))).toEqual([
      [0, 'notice'],
      [1, 'first'],
      [2, 'second'],
      [3, 'footer'],
    ])

    applyHistoryAppend(H, p, 1, [r('first')])
    applyScreen(H, p, screenOf(2, ['second', 'footer']))
    const after = getSessionScreen(H, p)
    expect(texts(after, 0, after.topLine + agentUsedRows(after))).toEqual([
      [0, 'notice'],
      [1, 'first'],
      [2, 'second'],
      [3, 'footer'],
    ])
  })

  test('an append with a gap does not extend the loaded range, nor move the grid', () => {
    const p = 'p2'
    applyScreen(H, p, screenOf(5, ['', '']))
    applyHistoryAppend(H, p, 3, [r('x'), r('y')])
    applyHistoryAppend(H, p, 20, [r('z')])
    const s = getSessionScreen(H, p)
    expect([s.loadedFrom, s.loadedTo]).toEqual([3, 5])
    expect(s.topLine).toBe(5)
    expect(rowAt(s, 20)).toBeUndefined()
  })

  test('diffs before any screen are ignored; a painter gets the screen at once', () => {
    const p = 'p3'
    applyDiff(H, p, 0, 0, [{ index: 0, row: r('q') }], cursor, 0)
    expect(getSessionScreen(H, p).loaded).toBe(false)
    const seen: string[] = []
    const off = subscribePaint(H, p, (ev) => seen.push(ev.kind))
    expect(seen).toEqual([])
    applyScreen(H, p, screenOf(0, ['a']))
    const off2 = subscribePaint(H, p, (ev) => seen.push(`late:${ev.kind}`))
    applyDiff(H, p, 0, 0, [{ index: 0, row: r('b') }], cursor, 0)
    expect(seen).toEqual(['screen', 'late:screen', 'diff', 'late:diff'])
    off()
    off2()
  })
})

describe('rows as logical lines', () => {
  test('wrapped rows join, gaps and hard breaks split', () => {
    const rows: Array<[number, RowInfo]> = [
      [0, r('abcd', true)],
      [1, r('ef')],
      [2, r('g', true)],
      [4, r('h')],
    ]
    const lines = joinWrapped(rows)
    expect(lines.map((l) => [l.line, l.spans.map((s) => s.text).join('')])).toEqual([
      [0, 'abcdef'],
      [2, 'g'],
      [4, 'h'],
    ])
    expect(rowsToText(rows)).toBe('abcdef\ng\nh')
    // Joining never mutates the source rows.
    expect(rows[0][1].spans.length).toBe(1)
  })
})

describe('RowsView', () => {
  test('refuses an open-ended range instead of looping', async () => {
    const { RowsView } = await import('@features/shell/Rows')
    // A memo component's render function; called directly to keep the
    // test free of a DOM renderer.
    const render = (RowsView as unknown as { type: (p: unknown) => unknown }).type
    expect(render({ hostId: H, sessionId: 'p', from: 3, to: Infinity })).toBeNull()
    expect(render({ hostId: H, sessionId: 'p', from: 5, to: 5 })).toBeNull()
  })
})

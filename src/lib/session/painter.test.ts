import { describe, expect, test } from 'bun:test'
import { ATTRS, MODES, TRUECOLOR_FLAG } from '@bindings'
import type { RowInfo } from '@bindings'
import type { Terminal } from '@xterm/xterm'
import { applyDiff, applyScreen } from './screen'
import {
  attachPainter,
  clampCursor,
  cursorSequence,
  diffSequence,
  modesSequence,
  rowSequence,
  screenSequence,
  spanSgr,
} from './painter'

const ESC = '\x1b'

function row(spans: RowInfo['spans'], wrapped = false): RowInfo {
  return { spans, wrapped }
}

describe('painter', () => {
  test('spanSgr resets then applies attributes and colours', () => {
    expect(spanSgr({ text: 'x', fg: -1, bg: -1, attrs: 0, link: null })).toBe(`${ESC}[0m`)
    expect(
      spanSgr({ text: 'x', fg: 1, bg: 4, attrs: ATTRS.BOLD | ATTRS.UNDERLINE | ATTRS.INVERSE, link: null }),
    ).toBe(`${ESC}[0;1;4;7;38;5;1;48;5;4m`)
    const tc = TRUECOLOR_FLAG | (0x10 << 16) | (0x20 << 8) | 0x30
    expect(spanSgr({ text: 'x', fg: tc, bg: -1, attrs: ATTRS.UNDERCURL, link: null })).toBe(
      `${ESC}[0;4:3;38;2;16;32;48m`,
    )
  })

  test('rowSequence positions, writes spans, wraps links, clears to EOL', () => {
    const r = row([
      { text: 'ab', fg: 2, bg: -1, attrs: 0, link: null },
      { text: 'link', fg: -1, bg: -1, attrs: 0, link: 'https://x.dev' },
    ])
    expect(rowSequence(3, r)).toBe(
      `${ESC}[4;1H${ESC}[0;38;5;2mab${ESC}[0m${ESC}]8;;https://x.dev${ESC}\\link${ESC}]8;;${ESC}\\${ESC}[0m${ESC}[K`,
    )
    expect(rowSequence(0, row([]))).toBe(`${ESC}[1;1H${ESC}[0m${ESC}[K`)
  })

  test('modesSequence emits only transitions, alt screen first', () => {
    expect(modesSequence(0, 0)).toBe('')
    const next = MODES.BRACKETED_PASTE | MODES.ALT_SCREEN | MODES.APP_KEYPAD
    expect(modesSequence(0, next)).toBe(`${ESC}[?1049h${ESC}[?2004h${ESC}=`)
    expect(modesSequence(next, MODES.BRACKETED_PASTE)).toBe(`${ESC}[?1049l${ESC}>`)
    expect(modesSequence(MODES.MOUSE_CLICK | MODES.SGR_MOUSE, 0)).toBe(`${ESC}[?1000l${ESC}[?1006l`)
  })

  test('cursorSequence sets position, shape and visibility', () => {
    expect(cursorSequence({ row: 2, col: 5, visible: true, shape: 'beam', blink: true })).toBe(
      `${ESC}[3;6H${ESC}[5 q${ESC}[?25h`,
    )
    expect(cursorSequence({ row: 0, col: 0, visible: false, shape: 'block', blink: false })).toBe(
      `${ESC}[1;1H${ESC}[2 q${ESC}[?25l`,
    )
  })

  test('clampCursor keeps stale resize frames inside the current grid', () => {
    const cursor = { row: 37, col: 143, visible: true, shape: 'beam' as const, blink: true }
    expect(clampCursor(cursor, 120, 28)).toEqual({ ...cursor, row: 27, col: 119 })
    expect(clampCursor(cursor, 144, 38)).toEqual(cursor)
  })

  test('screenSequence paints every row then the cursor', () => {
    const s = screenSequence(
      [row([{ text: 'hi', fg: -1, bg: -1, attrs: 0, link: null }]), row([])],
      { row: 1, col: 0, visible: true, shape: 'block', blink: false },
      MODES.FOCUS_IN_OUT,
      0,
    )
    expect(s.startsWith(`${ESC}[?25l${ESC}[?1004h${ESC}[1;1H`)).toBe(true)
    expect(s.endsWith(`${ESC}[2;1H${ESC}[2 q${ESC}[?25h`)).toBe(true)
    expect(s.includes('hi')).toBe(true)
  })

  test('diffSequence scrolls first, then paints only the given rows', () => {
    const s = diffSequence(
      [{ index: 5, row: row([{ text: 'z', fg: -1, bg: -1, attrs: 0, link: null }]) }],
      { row: 5, col: 1, visible: true, shape: 'underline', blink: false },
      0,
      0,
      2,
    )
    expect(s).toBe(
      `${ESC}[?25l${ESC}[2S${ESC}[6;1H${ESC}[0mz${ESC}[0m${ESC}[K${ESC}[6;2H${ESC}[4 q${ESC}[?25h`,
    )
  })

  test('a skipped diff recovers with an authoritative full repaint', () => {
    const writes: string[] = []
    const term = {
      cols: 4,
      rows: 2,
      write(data: string, callback?: () => void) {
        writes.push(data)
        callback?.()
      },
    } as unknown as Terminal
    let visible = false
    applyScreen('paint-host', 'paint-session', {
      cols: 4,
      rows: 2,
      top_line: 0,
      history_start: 0,
      lines: [row([{ text: 'old', fg: -1, bg: -1, attrs: 0, link: null }]), row([])],
      cursor: { row: 1, col: 0, visible: true, shape: 'block', blink: false },
      modes: 0,
    })
    const painter = attachPainter(term, 'paint-host', 'paint-session', () => visible)
    visible = true
    applyDiff(
      'paint-host',
      'paint-session',
      0,
      0,
      [{ index: 1, row: row([{ text: 'new', fg: -1, bg: -1, attrs: 0, link: null }]) }],
      { row: 1, col: 3, visible: true, shape: 'block', blink: false },
      0,
    )
    expect(writes).toHaveLength(1)
    expect(writes[0]).toContain('old')
    expect(writes[0]).toContain('new')
    painter.dispose()
  })

  test('a queued resize cannot run after painter disposal', () => {
    const callbacks: Array<() => void> = []
    const term = {
      cols: 4,
      rows: 2,
      write(_data: string, callback?: () => void) {
        if (callback) callbacks.push(callback)
      },
    } as unknown as Terminal
    const painter = attachPainter(term, 'dispose-host', 'dispose-session', () => false)
    let resized = false
    painter.resizeAndRepaint(() => {
      resized = true
    })
    painter.dispose()
    callbacks.forEach((callback) => callback())
    expect(resized).toBe(false)
  })
})

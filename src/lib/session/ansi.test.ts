import { describe, expect, test } from 'bun:test';
import { applySgr, lastLines, linesToText, renderAnsi } from './ansi';
import { base64ToBytes, bytesToBase64 } from './bytes';

describe('renderAnsi', () => {
  test('plain lines', () => {
    const lines = renderAnsi('hello\nworld\n');
    expect(linesToText(lines)).toBe('hello\nworld');
  });

  test('sgr colors split spans and reset', () => {
    const lines = renderAnsi('\x1b[31mred\x1b[0m plain \x1b[1;32mbold green\x1b[m');
    expect(lines).toHaveLength(1);
    expect(lines[0].map((s) => s.text)).toEqual(['red', ' plain ', 'bold green']);
    expect(lines[0][0].style.fg).toBe('#E0564A');
    expect(lines[0][1].style.fg).toBeNull();
    expect(lines[0][2].style.bold).toBe(true);
  });

  test('carriage return overwrites (progress bar collapses)', () => {
    const lines = renderAnsi('[=>   ] 20%\r[===> ] 60%\r[=====] 100%\n');
    expect(linesToText(lines)).toBe('[=====] 100%');
  });

  test('erase-to-end-of-line after CR truncates leftovers', () => {
    const lines = renderAnsi('Downloading 12345 bytes\rDone\x1b[K\n');
    expect(linesToText(lines)).toBe('Done');
  });

  test('cursor up redraws a multi-line progress frame', () => {
    const frame1 = 'a: 10%\nb: 10%\n';
    const redraw = '\x1b[2A\x1b[2Ka: 100%\n\x1b[2Kb: 100%\n';
    expect(linesToText(renderAnsi(frame1 + redraw))).toBe('a: 100%\nb: 100%');
  });

  test('osc 8 hyperlinks set href and clear', () => {
    const lines = renderAnsi('see \x1b]8;;https://x.dev\x07docs\x1b]8;;\x07 now');
    expect(lines[0].map((s) => [s.text, s.style.href])).toEqual([
      ['see ', null], ['docs', 'https://x.dev'], [' now', null],
    ]);
  });

  test('256 and truecolor', () => {
    const s = applySgr([38, 5, 196, 48, 2, 1, 2, 3], { ...renderAnsi('')[0]?.[0]?.style ?? {
      fg: null, bg: null, bold: false, dim: false, italic: false, underline: false, inverse: false, strike: false, href: null } });
    expect(s.fg).toBe('rgb(255,0,0)');
    expect(s.bg).toBe('rgb(1,2,3)');
  });

  test('unknown OSC and DCS are dropped; titles do not leak', () => {
    const lines = renderAnsi('\x1b]0;title\x07visible\x1bPq#0\x1b\\ tail');
    expect(linesToText(lines)).toBe('visible tail');
  });

  test('truncated escape at end is dropped without throwing', () => {
    expect(linesToText(renderAnsi('ok\x1b[31'))).toBe('ok');
  });

  test('tabs and backspace', () => {
    expect(linesToText(renderAnsi('a\tb'))).toBe('a       b');
    expect(linesToText(renderAnsi('abc\b\bX'))).toBe('aXc');
  });

  test('astral characters stay whole', () => {
    const lines = renderAnsi('✻ 🚀 done');
    expect(linesToText(lines)).toBe('✻ 🚀 done');
  });

  test('trailing blank lines trimmed, interior kept', () => {
    expect(linesToText(renderAnsi('a\n\nb\n\n\n'))).toBe('a\n\nb');
  });
});

describe('lastLines', () => {
  test('drops trailing blanks and tails', () => {
    const lines = renderAnsi('a\nb\nc\n\n\n');
    expect(linesToText(lastLines(lines, 2))).toBe('b\nc');
  });
});

describe('base64 helpers', () => {
  test('roundtrip bytes', () => {
    const bytes = new Uint8Array([0, 1, 27, 91, 255, 10]);
    expect(Array.from(base64ToBytes(bytesToBase64(bytes)))).toEqual(Array.from(bytes));
  });
});

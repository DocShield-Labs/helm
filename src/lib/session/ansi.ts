/**
 * ANSI/VT → styled spans, for rendering *finished* command blocks as
 * plain DOM (selectable, searchable, no xterm instance per block).
 *
 * This is deliberately not a terminal emulator. Finished output is a
 * static transcript; what we need is faithful handling of the things
 * shells and CLIs actually do inline:
 *
 *   - SGR styling (16/256/truecolor, bold/dim/italic/underline/
 *     inverse/strike)
 *   - `\r` overwrite and `ESC[K` erase (progress bars, spinners)
 *   - cursor up/down/column moves so multi-line progress (cargo, npm,
 *     docker pulls) collapses to its final frame instead of stacking
 *   - OSC 8 hyperlinks (kept as `href` on the span)
 *   - everything else (other OSC/DCS, private modes) dropped silently
 *
 * The model is a width-unbounded grid: an array of lines, each an array
 * of styled cells, plus a cursor. No wrapping — wrapping is CSS's job.
 * Alt-screen TUIs never come through here (they render in xterm).
 */

export interface Style {
  fg: string | null;      // CSS color or null for default
  bg: string | null;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  inverse: boolean;
  strike: boolean;
  href: string | null;
}

export interface Span {
  text: string;
  style: Style;
}

export type Line = Span[];

const DEFAULT_STYLE: Style = {
  fg: null, bg: null, bold: false, dim: false, italic: false,
  underline: false, inverse: false, strike: false, href: null,
};

/** Standard 16-color palette (Helm 2.0 tokens; Warp-style pastels on near-black). */
export const ANSI16: string[] = [
  '#3A3A3E', '#E0564A', '#3DBA7E', '#E8B04B', '#4B8BF5', '#C678DD', '#56B6C2', '#D7D6D2',
  '#6B6B70', '#FF7B70', '#5ED39A', '#F5C56B', '#7AA8FF', '#D98FE8', '#7BD0DA', '#F5F4F0',
];

function color256(n: number): string {
  if (n < 16) return ANSI16[n];
  if (n < 232) {
    const i = n - 16;
    const r = Math.floor(i / 36), g = Math.floor((i % 36) / 6), b = i % 6;
    const v = (c: number) => (c === 0 ? 0 : 55 + c * 40);
    return `rgb(${v(r)},${v(g)},${v(b)})`;
  }
  const gray = 8 + (n - 232) * 10;
  return `rgb(${gray},${gray},${gray})`;
}

interface Cell { ch: string; style: Style }

/** Apply an SGR parameter list to a style, returning the new style. */
export function applySgr(params: number[], style: Style): Style {
  let s = { ...style };
  if (params.length === 0) params = [0];
  for (let i = 0; i < params.length; i++) {
    const p = params[i];
    switch (p) {
      case 0: s = { ...DEFAULT_STYLE, href: s.href }; break;
      case 1: s.bold = true; break;
      case 2: s.dim = true; break;
      case 3: s.italic = true; break;
      case 4: s.underline = true; break;
      case 7: s.inverse = true; break;
      case 9: s.strike = true; break;
      case 22: s.bold = false; s.dim = false; break;
      case 23: s.italic = false; break;
      case 24: s.underline = false; break;
      case 27: s.inverse = false; break;
      case 29: s.strike = false; break;
      case 38: case 48: {
        const target = p === 38 ? 'fg' : 'bg';
        if (params[i + 1] === 5 && i + 2 < params.length) {
          s[target] = color256(params[i + 2]); i += 2;
        } else if (params[i + 1] === 2 && i + 4 < params.length) {
          s[target] = `rgb(${params[i + 2]},${params[i + 3]},${params[i + 4]})`; i += 4;
        }
        break;
      }
      case 39: s.fg = null; break;
      case 49: s.bg = null; break;
      default:
        if (p >= 30 && p <= 37) s.fg = ANSI16[p - 30];
        else if (p >= 40 && p <= 47) s.bg = ANSI16[p - 40];
        else if (p >= 90 && p <= 97) s.fg = ANSI16[p - 90 + 8];
        else if (p >= 100 && p <= 107) s.bg = ANSI16[p - 100 + 8];
    }
  }
  return s;
}

function sameStyle(a: Style, b: Style): boolean {
  return a.fg === b.fg && a.bg === b.bg && a.bold === b.bold && a.dim === b.dim &&
    a.italic === b.italic && a.underline === b.underline && a.inverse === b.inverse &&
    a.strike === b.strike && a.href === b.href;
}

/**
 * Render a finished block's bytes (already UTF-8 decoded) into lines of
 * merged spans. Trailing blank lines are trimmed; interior structure is
 * preserved exactly.
 */
export function renderAnsi(text: string): Line[] {
  const grid: Cell[][] = [[]];
  let row = 0, col = 0;
  let style: Style = { ...DEFAULT_STYLE };

  const ensureRow = (r: number) => { while (grid.length <= r) grid.push([]); };
  const put = (ch: string) => {
    ensureRow(row);
    const line = grid[row];
    while (line.length < col) line.push({ ch: ' ', style: DEFAULT_STYLE });
    line[col] = { ch, style };
    col++;
  };

  let i = 0;
  const n = text.length;
  while (i < n) {
    const c = text[i];
    if (c === '\x1b') {
      const next = text[i + 1];
      if (next === '[') {
        // CSI: params until final byte 0x40–0x7E
        let j = i + 2;
        while (j < n && !(text.charCodeAt(j) >= 0x40 && text.charCodeAt(j) <= 0x7e)) j++;
        if (j >= n) break; // truncated sequence at end — drop it
        const final = text[j];
        const raw = text.slice(i + 2, j);
        const priv = raw.startsWith('?');
        const params = (priv ? raw.slice(1) : raw)
          .split(/[;:]/)
          .map((p) => (p === '' ? 0 : parseInt(p, 10)))
          .map((p) => (Number.isNaN(p) ? 0 : p));
        if (!priv) {
          const a = params[0] || 0;
          switch (final) {
            case 'm': style = applySgr(raw === '' ? [] : params, style); break;
            case 'K': {
              ensureRow(row);
              const line = grid[row];
              if (a === 0) line.length = Math.min(line.length, col);
              else if (a === 1) for (let k = 0; k < Math.min(col + 1, line.length); k++) line[k] = { ch: ' ', style: DEFAULT_STYLE };
              else if (a === 2) line.length = 0;
              break;
            }
            case 'J': {
              if (a === 0) { ensureRow(row); grid[row].length = Math.min(grid[row].length, col); grid.length = row + 1; }
              else if (a === 2 || a === 3) { grid.length = 0; grid.push([]); row = 0; col = 0; }
              break;
            }
            case 'A': row = Math.max(0, row - (a || 1)); break;
            case 'B': row = row + (a || 1); ensureRow(row); break;
            case 'C': col = col + (a || 1); break;
            case 'D': col = Math.max(0, col - (a || 1)); break;
            case 'E': row = row + (a || 1); col = 0; ensureRow(row); break;
            case 'F': row = Math.max(0, row - (a || 1)); col = 0; break;
            case 'G': col = Math.max(0, (a || 1) - 1); break;
            case 'H': case 'f': {
              // Absolute positioning is rare inline; honor it relative
              // to the transcript's first line.
              row = Math.max(0, (params[0] || 1) - 1); col = Math.max(0, (params[1] || 1) - 1); ensureRow(row); break;
            }
            default: break; // unsupported CSI: ignore
          }
        }
        i = j + 1;
        continue;
      }
      if (next === ']') {
        // OSC: until BEL or ESC \
        let j = i + 2;
        let end = -1, skip = 0;
        while (j < n) {
          if (text[j] === '\x07') { end = j; skip = 1; break; }
          if (text[j] === '\x1b' && text[j + 1] === '\\') { end = j; skip = 2; break; }
          j++;
        }
        if (end < 0) break;
        const body = text.slice(i + 2, end);
        if (body.startsWith('8;')) {
          // OSC 8 ; params ; URI
          const uri = body.slice(2).split(';').slice(1).join(';');
          style = { ...style, href: uri === '' ? null : uri };
        }
        i = end + skip;
        continue;
      }
      if (next === 'P' || next === 'X' || next === '^' || next === '_') {
        const end = text.indexOf('\x1b\\', i + 2);
        if (end < 0) break;
        i = end + 2;
        continue;
      }
      // Two-byte escape (ESC 7, ESC =, charset, …): drop both.
      i += 2;
      continue;
    }
    if (c === '\n') { row++; col = 0; ensureRow(row); i++; continue; }
    if (c === '\r') { col = 0; i++; continue; }
    if (c === '\b') { col = Math.max(0, col - 1); i++; continue; }
    if (c === '\t') { col = (Math.floor(col / 8) + 1) * 8; i++; continue; }
    if (c === '\x07' || (c < ' ' && c !== '\t')) { i++; continue; }
    // Surrogate pairs: keep astral chars whole in one cell.
    const code = text.codePointAt(i)!;
    const ch = String.fromCodePoint(code);
    put(ch);
    i += ch.length;
  }

  // Trim trailing empty lines.
  while (grid.length > 1 && grid[grid.length - 1].length === 0) grid.pop();

  return grid.map((cells) => {
    const spans: Span[] = [];
    for (const cell of cells) {
      const last = spans[spans.length - 1];
      if (last && sameStyle(last.style, cell.style)) last.text += cell.ch;
      else spans.push({ text: cell.ch, style: cell.style });
    }
    return spans;
  });
}

/** Plain text of rendered lines (for search / copy). */
export function linesToText(lines: Line[]): string {
  return lines.map((l) => l.map((s) => s.text).join('')).join('\n');
}

/** The last `n` lines, ignoring trailing blank ones. */
export function lastLines(lines: Line[], n: number): Line[] {
  let end = lines.length;
  while (end > 0 && lines[end - 1].every((s) => s.text.trim() === '')) end--;
  return lines.slice(Math.max(0, end - n), end);
}

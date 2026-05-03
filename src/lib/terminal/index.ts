/**
 * xterm.js + WebGL renderer wrapper. Thin and unopinionated:
 * the consumer creates a div, hands it to `attachTerminal`, and we wire up
 * the renderer + addons. Backpressure (`xon`/`xoff` via Terminal.write())
 * happens implicitly — see crates/helm-pty for the producer-side batching.
 */

import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'

// We deliberately don't load `@xterm/addon-webgl`. Browsers cap WebGL
// contexts per page (Chromium ~16, Safari similar) and contexts release
// on GC, not synchronously on `dispose()`. With multiple workspaces +
// rapid window switching + React strict-mode's double-mount in dev, we
// blow past the cap, see "too many active WebGL contexts" errors, and
// xterm starts throwing `_renderer.value.dimensions is undefined` as
// the GPU process evicts old contexts. The built-in canvas renderer
// has no such cap, doesn't crash under churn, and the perf delta for
// our usage (typing + reading output, not full-screen 60fps redraws)
// is invisible. Phase 5 can revisit if profiling shows it matters.

export interface HelmTerminal {
  term: Terminal
  fit: FitAddon
  dispose: () => void
}

export interface AttachOptions {
  fontSize?: number
  lineHeight?: number
  fontFamily?: string
}

export function attachTerminal(host: HTMLElement, opts: AttachOptions = {}): HelmTerminal {
  const term = new Terminal({
    fontFamily:
      opts.fontFamily ??
      '"Berkeley Mono", "JetBrains Mono", "SF Mono", ui-monospace, monospace',
    fontSize: opts.fontSize ?? 12,
    lineHeight: opts.lineHeight ?? 1.2,
    cursorBlink: true,
    cursorStyle: 'block',
    // Helps Powerlevel10k / starship chevrons render at the correct width
    // instead of overlapping the next column.
    rescaleOverlappingGlyphs: true,
    theme: {
      background: '#0A0B0D',
      foreground: '#ECEDEE',
      cursor: '#F4856B',
      cursorAccent: '#0A0B0D',
      selectionBackground: 'rgba(244, 133, 107, 0.25)',
    },
    allowProposedApi: true,
    // tmux's protocol output uses bare LF, not CRLF — xterm needs to translate
    // so the cursor returns to column 0 on each newline. Disable this and
    // every line drifts right by the previous line's length.
    convertEol: true,
    scrollback: 10_000,
  })

  const fit = new FitAddon()
  term.loadAddon(fit)

  // Cmd-prefixed shortcuts are app shortcuts (new window, switch, palette,
  // etc.); never forward them to the shell. xterm calls this before
  // emitting onData / interpreting keys, so returning false vetoes its
  // handling and the document-level keydown listener gets a clean shot.
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.metaKey) return false
    return true
  })

  term.open(host)
  try {
    fit.fit()
  } catch {
    /* terminal not visible yet */
  }

  return {
    term,
    fit,
    dispose: () => {
      term.dispose()
    },
  }
}

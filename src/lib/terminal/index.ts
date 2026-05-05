/**
 * xterm.js + WebGL renderer wrapper. Thin and unopinionated:
 * the consumer creates a div, hands it to `attachTerminal`, and we wire up
 * the renderer + addons. Backpressure (`xon`/`xoff` via Terminal.write())
 * happens implicitly — see crates/helm-pty for the producer-side batching.
 */

import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { WebLinksAddon } from '@xterm/addon-web-links'
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
  /** Click handler for URLs / file:line links rendered by xterm. Receives
   * the matched URI; resolution to a real file (or `open` invocation) is
   * left to the caller. Falls back to `window.open` for full URLs when
   * unset. */
  onLinkClick?: (uri: string) => void
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
      cursor: '#7AA2F7',
      cursorAccent: '#0A0B0D',
      // Selection: same accent blue as the cursor / chevron / running
      // chip, at the alpha that gives a clear-but-not-loud highlight.
      selectionBackground: 'rgba(122, 162, 247, 0.28)',
      selectionForeground: '#ECEDEE',
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

  // WebLinks: detect http(s) URLs in output and make them clickable.
  // Phase 4F polish: a custom matcher for `path:line` references is
  // layered on top in the consuming pane (TmuxPane) so we keep this
  // wrapper generic.
  const links = new WebLinksAddon((_event, uri) => {
    if (opts.onLinkClick) {
      opts.onLinkClick(uri)
      return
    }
    // No handler supplied — best-effort fallback. Tauri webview blocks
    // window.open for unknown protocols, so this is a no-op there but
    // works in dev / web previews.
    window.open(uri, '_blank', 'noopener,noreferrer')
  })
  term.loadAddon(links)

  // Translate a few macOS-standard editing chords into the bytes
  // readline-style line editors (zsh, bash, most TUIs) actually
  // understand:
  //
  //   Cmd+Left   → ^A   (go to beginning of line)
  //   Cmd+Right  → ^E   (go to end of line)
  //   Shift+Enter → LF  (literal newline; CRLF-aware CLIs treat the
  //                       CR-less LF as multi-line input — matches
  //                       iTerm2 / Terminal.app default)
  //
  // We have to fully claim these events: returning false from the
  // xterm handler only stops xterm's own translation, the
  // KeyboardEvent still propagates to the document and OS. Without
  // preventDefault + stopPropagation, macOS (or the Tauri window
  // chrome) will eat Cmd+Left as a window-management shortcut.
  //
  // Other Cmd-prefixed chords (palette, switcher, block actions)
  // pass through to the document-level keymap unchanged — we only
  // veto xterm's data emission for them.
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type !== 'keydown') return true
    const onlyMeta =
      ev.metaKey && !ev.shiftKey && !ev.altKey && !ev.ctrlKey
    if (onlyMeta && ev.key === 'ArrowLeft') {
      ev.preventDefault()
      ev.stopPropagation()
      term.input('\x01', true)
      return false
    }
    if (onlyMeta && ev.key === 'ArrowRight') {
      ev.preventDefault()
      ev.stopPropagation()
      term.input('\x05', true)
      return false
    }
    if (
      ev.shiftKey &&
      !ev.metaKey &&
      !ev.altKey &&
      !ev.ctrlKey &&
      ev.key === 'Enter'
    ) {
      ev.preventDefault()
      ev.stopPropagation()
      term.input('\n', true)
      return false
    }
    if (ev.metaKey) return false
    return true
  })

  term.open(host)
  try {
    fit.fit()
  } catch {
    /* terminal not visible yet */
  }

  // Native-feeling scroll for trackpads. xterm's built-in viewport
  // scrolls integer rows per wheel notch, which on a high-DPI macOS
  // trackpad shows up as one row per ~13px gesture — distinctly
  // step-y. We replace that with our own handler:
  //
  //   - Listen on the host in capture phase + stopPropagation so
  //     xterm's own wheel listener (registered on `.xterm-viewport`
  //     during `term.open`) never fires.
  //   - Translate `deltaY` to row-fraction units based on `deltaMode`:
  //       0 (PIXEL) → divide by measured cell height
  //       1 (LINE)  → pass through directly
  //       2 (PAGE)  → multiply by viewport rows
  //     macOS trackpads emit pixel mode; classic mouse wheels emit
  //     line mode; our behaviour matches each modality's intent.
  //   - Accumulate fractional rows; only call `term.scrollLines(int)`
  //     when the integer part crosses a boundary. Fractions persist
  //     between events so a slow trackpad gesture eventually scrolls
  //     a full row instead of being rounded away.
  //   - Cmd / Ctrl + wheel are left to the browser (zoom).
  let scrollAccum = 0
  let cachedCellH = 17
  const measureCellHeight = () => {
    const row = host.querySelector('.xterm-rows > div') as HTMLElement | null
    if (row) {
      const h = row.getBoundingClientRect().height
      if (h > 0) cachedCellH = h
    }
  }
  measureCellHeight()
  const onWheel = (e: WheelEvent) => {
    if (e.ctrlKey || e.metaKey) return
    e.preventDefault()
    e.stopPropagation()

    let rowDelta: number
    switch (e.deltaMode) {
      case 1: // LINE
        rowDelta = e.deltaY
        break
      case 2: // PAGE
        rowDelta = e.deltaY * Math.max(1, term.rows - 1)
        break
      case 0: // PIXEL
      default:
        rowDelta = e.deltaY / cachedCellH
        break
    }

    scrollAccum += rowDelta
    const intRows = Math.trunc(scrollAccum)
    if (intRows !== 0) {
      scrollAccum -= intRows
      term.scrollLines(intRows)
    }
  }
  // capture phase + passive:false so we can preventDefault. Without
  // capture, xterm's own listener (which is bubble-phase on a child
  // element) fires first and we'd double-scroll.
  host.addEventListener('wheel', onWheel, { capture: true, passive: false })

  // Re-measure cell height when the host resizes (font-size or DPR
  // changes). Cheap; wheel handler reads from the cached value so the
  // hot path stays free of layout thrash.
  const ro = new ResizeObserver(measureCellHeight)
  ro.observe(host)

  return {
    term,
    fit,
    dispose: () => {
      host.removeEventListener('wheel', onWheel, { capture: true } as EventListenerOptions)
      ro.disconnect()
      term.dispose()
    },
  }
}

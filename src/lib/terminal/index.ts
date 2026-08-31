/**
 * xterm.js + WebGL renderer wrapper. Thin and unopinionated:
 * the consumer creates a div, hands it to `attachTerminal`, and we wire up
 * the renderer + addons.
 *
 * Since M8 xterm is a painter for the daemon's grid and an input
 * encoder — it has no scrollback (history is DOM above it, see
 * lib/session/screen.ts) and never sees application bytes.
 */

import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { correctedLineHeight, domLinePx } from './cellHeight'
import { SearchAddon } from '@xterm/addon-search'
import { WebLinksAddon } from '@xterm/addon-web-links'
import { WebglAddon } from '@xterm/addon-webgl'
import { commands } from '@lib/ipc'
import { getTheme, xtermThemeFor, type Theme } from './themes'
import '@xterm/xterm/css/xterm.css'

export { THEMES, DEFAULT_THEME_NAME, applyThemeCssVars, getTheme } from './themes'
export type { Theme } from './themes'

// WebGL renderer with `onContextLoss` fallback. The atlas-cached glyphs
// look noticeably crisper than the DOM renderer, especially at small
// font sizes. The historical concern was browser per-page context
// caps (Chromium ~16) blowing up under session churn — fixed here by
// disposing the addon when a context is lost, which makes xterm fall
// back to its built-in DOM renderer for the affected session. Construction
// is wrapped in try/catch so headless or no-GPU environments degrade
// silently instead of throwing.

export interface HelmTerminal {
  term: Terminal
  fit: FitAddon
  /** In-session find (Cmd+F). The SearchOverlay drives findNext/findPrevious
   * and reads match counts via `onDidChangeResults`. Decorations are
   * passed per-call by the overlay so highlight colours track the theme. */
  search: SearchAddon
  /** Pixel size of one xterm cell, measured from `.xterm-screen / rows`
   * to bypass CSS line-height rounding. Returns the latest value cached
   * by the internal ResizeObserver — single source of truth shared
   * between the wheel handler, the block overlay, and hover hit-tests. */
  /** Live-swap the xterm theme. Mutates `term.options.theme`, which
   * triggers a redraw with the new palette (no reload, no remount). */
  setTheme(theme: Theme): void
  dispose: () => void
}

export interface AttachOptions {
  /** Owner-driven sizing: called whenever the terminal should re-fit
   * its host (first render, line-height corrections). Lets the owner
   * define "how many columns fit" once — SessionView derives cols from
   * the DOM glyph advance, not xterm's cell width. Defaults to
   * FitAddon.fit(). */
  refit?: () => void
  fontSize?: number
  lineHeight?: number
  fontFamily?: string
  /** Initial theme. The new terminal also auto-registers for
   * `setThemeForAllTerminals` — pass the current store value here for
   * first paint, then a single subscriber at the app level handles
   * subsequent swaps. */
  theme?: Theme
  /** Click handler for URLs / file:line links rendered by xterm. Receives
   * the matched URI; resolution to a real file (or `open` invocation) is
   * left to the caller. Falls back to `window.open` for full URLs when
   * unset. */
  onLinkClick?: (uri: string) => void
}

/** Registry of every currently-attached terminal. The theme picker
 * fans out via `setThemeForAllTerminals` so we don't need each session
 * to maintain its own store subscription — relevant when many sessions
 * are mounted at once and the user is rapidly cycling preview themes
 * (one xterm atlas rebuild per session per keypress otherwise). */
const attached = new Set<HelmTerminal>()

/** Push a theme into every live xterm. Safe to call from anywhere;
 * disposed terminals deregister themselves. */
export function setThemeForAllTerminals(theme: Theme): void {
  for (const helm of attached) helm.setTheme(theme)
}

export function attachTerminal(host: HTMLElement, opts: AttachOptions = {}): HelmTerminal {
  const initialTheme = opts.theme ?? getTheme(undefined)

  // Shared open path for both link mechanisms below (plain-text URLs via
  // WebLinksAddon, and OSC 8 escape-sequence hyperlinks via linkHandler).
  // Tauri's webview blocks window.open, so everything routes through the
  // Rust open_url command unless the caller overrides.
  const openLink = (uri: string) => {
    if (opts.onLinkClick) {
      opts.onLinkClick(uri)
      return
    }
    commands.openUrl(uri).then(
      (res) => {
        if (res.status !== 'ok') console.error('[helm] open_url rejected:', res.error)
      },
      (err) => console.error('[helm] open_url threw:', err),
    )
  }

  const term = new Terminal({
    // Lifted from Warp's defaults (app/src/settings/font.rs:11-13):
    //   font: Hack, size: 13, line-height ratio: 1.2.
    // We prefer Hack first, fall back to Berkeley Mono / JetBrains Mono
    // / SF Mono so the look is right when Hack isn't installed.
    fontFamily:
      opts.fontFamily ??
      '"Hack", "Berkeley Mono", "JetBrains Mono", "SF Mono", ui-monospace, monospace',
    fontSize: opts.fontSize ?? 13,
    // Cells must be `--helm-line-px` (20px) tall — the rows finished
    // blocks render as DOM — so the live band and the block it turns
    // into line up. xterm's `lineHeight` multiplies the font's measured
    // natural height (~1.2× the size), not `fontSize`, so this is only
    // a first guess; `measureCellSize` corrects it from the rendered
    // cell (see cellHeight.ts).
    lineHeight: opts.lineHeight ?? 1.25,
    letterSpacing: 0,
    fontWeight: 400,
    fontWeightBold: 600,
    cursorBlink: true,
    cursorStyle: 'bar',
    cursorInactiveStyle: 'outline',
    cursorWidth: 2,
    rescaleOverlappingGlyphs: true,
    theme: xtermThemeFor(initialTheme),
    allowProposedApi: true,
    // OSC 8 hyperlinks (`ESC ] 8 ; ; URL ST  label  ESC ] 8 ; ; ST`).
    // Programs like Claude Code emit links this way — a styled label with
    // the URL carried in the escape sequence rather than printed as raw
    // text. xterm parses them but only makes them clickable when a
    // linkHandler is set; without this they render as inert styled text.
    // (WebLinksAddon below is the *other* path: plain-text URLs like the
    // raw PR link `gh` prints.)
    linkHandler: {
      activate: (_event, uri) => openLink(uri),
    },
    // helmd forwards raw PTY bytes (real CRLF from the line discipline),
    // so no LF→CRLF translation: doing it would double-space output
    // from programs that already emit CRLF.
    convertEol: false,
    // The grid only: history lives in the daemon and renders as DOM.
    scrollback: 0,
    // When a TUI like Claude Code or vim turns on mouse capture
    // (DECSET 1000/1006), xterm forwards every click to the app and
    // link clicks no longer fire. Enabling this lets the user hold
    // Option (Alt) and click to force-select / activate a link
    // anyway — matching iTerm2 / Terminal.app's behaviour.
    macOptionClickForcesSelection: true,
  })

  const fit = new FitAddon()
  const refit =
    opts.refit ??
    (() => {
      try {
        fit.fit()
      } catch {
        /* not laid out yet */
      }
    })
  term.loadAddon(fit)

  // In-session find. Loaded eagerly (cheap) so Cmd+F is instant; the
  // overlay UI (SearchOverlay) is what's lazily mounted on demand.
  const search = new SearchAddon()
  term.loadAddon(search)

  // WebLinks: detect http(s) URLs in output and make them clickable.
  //
  // The default handler routes through the Rust `open_url` command —
  // Tauri's webview blocks `window.open`, so without this, clicking
  // a link in the terminal would silently no-op. Callers can override
  // by passing `onLinkClick`.
  //
  // Inside TUIs that capture mouse (Claude Code, vim, htop) plain
  // clicks are forwarded to the app and never reach this handler —
  // see `macOptionClickForcesSelection` above; user holds Option and
  // clicks to bypass.
  const links = new WebLinksAddon((_event, uri) => openLink(uri))
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
  // WebGL renderer must be loaded *after* `term.open` so it can read
  // the host's dimensions. If construction fails (no WebGL2, blocked
  // context) or the context is lost later (GPU process churn,
  // browser-cap eviction), we dispose the addon and xterm reverts to
  // its built-in DOM renderer for this session. No reload, no crash.
  let webgl: WebglAddon | null = null
  try {
    const addon = new WebglAddon()
    addon.onContextLoss(() => addon.dispose())
    term.loadAddon(addon)
    webgl = addon
  } catch {
    /* no WebGL2 — xterm falls back to its DOM renderer automatically */
  }
  // The first fit waits for the first render (below): fit() → term.resize
  // → syncScrollArea reads `renderer.dimensions`, which is undefined until
  // the renderer has painted — and loading the WebGL addon above swaps the
  // renderer, so a fit here throws an (async, uncatchable) TypeError.

  // Cached cell dimensions in pixels. Measure from `.xterm-screen / rows`
  // (and cols) — that's xterm's internal layout, not CSS line-height,
  // so we sidestep rounding drift between the two. The session's wheel
  // handler reads it to turn line-mode wheel deltas into pixels, and
  // SessionView sizes the live band from it. (The grid has no scrollback,
  // so xterm never scrolls itself: the session decides what a wheel over
  // the grid means — see SessionView.)
  //
  // The measurement also closes the loop on `lineHeight`: when the
  // rendered cell isn't the DOM row height, set the lineHeight that
  // makes it so and refit. xterm resizes `.xterm-screen` in response,
  // which re-measures and finds nothing left to correct.
  const linePx = domLinePx()
  let cachedCellH = linePx
  // Pure measurement: read the rendered cell into the cache. NEVER
  // mutates layout — an observer that resized what it observes would
  // loop. Returns the measured cell height, or 0 if nothing's laid out.
  const measureCellSize = (): number => {
    const screen = host.querySelector('.xterm-screen') as HTMLElement | null
    if (screen && term.rows > 0 && term.cols > 0) {
      const rect = screen.getBoundingClientRect()
      if (rect.height > 0) {
        cachedCellH = rect.height / term.rows
        return cachedCellH
      }
    }
    const row = host.querySelector('.xterm-rows > div') as HTMLElement | null
    if (row) {
      const rect = row.getBoundingClientRect()
      if (rect.height > 0) cachedCellH = rect.height
      return cachedCellH
    }
    return 0
  }

  // Make xterm's cell exactly `--helm-line-px` tall (see cellHeight.ts).
  // A bounded one-shot, decoupled from the ResizeObserver: each pass
  // measures, and if the cell is off, sets the lineHeight that fixes it
  // and refits — which resizes the screen, so we re-check on the next
  // frame. Capped so a font that never lands on an exact device pixel
  // can't loop; whatever cell we have after that is what the band uses.
  let corrections = 0
  const correctLineHeight = () => {
    const measured = measureCellSize()
    if (measured <= 0) {
      if (corrections < 8) {
        corrections++
        requestAnimationFrame(correctLineHeight)
      }
      return
    }
    const lh = correctedLineHeight(measured, term.options.lineHeight ?? 1, window.devicePixelRatio || 1, linePx)
    if (lh === null || corrections >= 4) return
    corrections++
    term.options.lineHeight = lh
    refit()
    requestAnimationFrame(correctLineHeight)
  }

  // Re-measure (cache only) on host resize — font-size or DPR changes.
  const ro = new ResizeObserver(() => measureCellSize())
  ro.observe(host)
  // Webfonts landing AFTER the correction passes change the font's
  // natural height, silently invalidating the lineHeight we solved
  // for — the cell drifts off `--helm-line-px`, the PTY gets more (or
  // fewer) rows than the DOM renders in the same pixels, and a TUI's
  // bottom-anchored chrome draws past the pane. The advance cache
  // already resets on fonts.ready (cellHeight.ts); re-arm the height
  // correction on the same signal.
  let disposed = false
  void document.fonts?.ready.then(() => {
    if (disposed) return
    corrections = 0
    correctLineHeight()
  })
  // Start the lineHeight correction only once xterm has actually
  // rendered: fit()/syncScrollArea read `renderer.dimensions`, which is
  // undefined until the first paint (throws otherwise), and the screen
  // element only settles a tick after `term.open` anyway.
  let firstRender: { dispose(): void } | null = term.onRender(() => {
    // Renderer has painted now — `dimensions` is populated, so a fit is
    // safe. Size to the viewport, then correct the cell height.
    refit()
    correctLineHeight()
    firstRender?.dispose()
    firstRender = null
  })

  const helm: HelmTerminal = {
    term,
    fit,
    search,
    setTheme: (theme: Theme) => {
      term.options.theme = xtermThemeFor(theme)
    },
    dispose: () => {
      disposed = true
      attached.delete(helm)
      ro.disconnect()
      firstRender?.dispose()
      webgl?.dispose()
      term.dispose()
    },
  }
  attached.add(helm)
  return helm
}

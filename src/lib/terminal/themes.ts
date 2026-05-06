/**
 * Terminal themes — colour sets ported from Warp's bundled themes
 * (`app/src/themes/default_themes.rs` in warpdotdev/warp). Each theme
 * supplies the pane background, foreground, accent (cursor / selection),
 * and the 16 ANSI slots xterm reads to colour shell output.
 *
 * The pane chrome (block dividers, status chips, action card) reads from
 * the `--terminal-*` CSS variables set in `tokens.css`. `applyTheme()`
 * pushes the active theme's values into those variables so the chrome
 * stays in sync with whatever xterm is rendering.
 */
import type { ITheme } from '@xterm/xterm'

export interface ThemeAnsi {
  black: string
  red: string
  green: string
  yellow: string
  blue: string
  magenta: string
  cyan: string
  white: string
  brightBlack: string
  brightRed: string
  brightGreen: string
  brightYellow: string
  brightBlue: string
  brightMagenta: string
  brightCyan: string
  brightWhite: string
}

export interface Theme {
  name: string
  bg: string
  fg: string
  accent: string
  ansi: ThemeAnsi
}

export const THEMES: readonly Theme[] = [
  {
    name: 'Phenomenon',
    bg: '#121212',
    fg: '#faf9f6',
    accent: '#2e5d9e',
    ansi: {
      black: '#121212', red: '#d22d1e', green: '#1ca05a', yellow: '#e5a01a',
      blue: '#3780e9', magenta: '#bf409d', cyan: '#799c92', white: '#faf9f6',
      brightBlack: '#292929', brightRed: '#ae756f', brightGreen: '#789b88', brightYellow: '#bd9f65',
      brightBlue: '#6f839f', brightMagenta: '#a57899', brightCyan: '#bfc5c3', brightWhite: '#ffffff',
    },
  },
  {
    name: 'Dark Default',
    bg: '#000000',
    fg: '#ffffff',
    accent: '#19aad8',
    ansi: {
      black: '#616161', red: '#ff8272', green: '#b4fa72', yellow: '#fefdc2',
      blue: '#a5d5fe', magenta: '#ff8ffd', cyan: '#d0d1fe', white: '#f1f1f1',
      brightBlack: '#8e8e8e', brightRed: '#ffc4bd', brightGreen: '#d6fcb9', brightYellow: '#fefdd5',
      brightBlue: '#c1e3fe', brightMagenta: '#ffb1fe', brightCyan: '#e5e6fe', brightWhite: '#feffff',
    },
  },
  {
    name: 'Light Default',
    bg: '#ffffff',
    fg: '#111111',
    accent: '#00c2ff',
    ansi: {
      black: '#212121', red: '#c30771', green: '#10a778', yellow: '#a89c14',
      blue: '#008ec4', magenta: '#523c79', cyan: '#20a5ba', white: '#e0e0e0',
      brightBlack: '#212121', brightRed: '#fb007a', brightGreen: '#5fd7af', brightYellow: '#f3e430',
      brightBlue: '#20bbfc', brightMagenta: '#6855de', brightCyan: '#4fb8cc', brightWhite: '#f1f1f1',
    },
  },
  {
    name: 'Dracula',
    bg: '#282a36',
    fg: '#f8f8f2',
    accent: '#ff79c6',
    ansi: {
      black: '#000000', red: '#ff5555', green: '#50fa7b', yellow: '#f1fa8c',
      blue: '#bd93f9', magenta: '#ff79c6', cyan: '#8be9fd', white: '#bbbbbb',
      brightBlack: '#555555', brightRed: '#ff5555', brightGreen: '#50fa7b', brightYellow: '#f1fa8c',
      brightBlue: '#caa9fa', brightMagenta: '#ff79c6', brightCyan: '#8be9fd', brightWhite: '#ffffff',
    },
  },
  {
    name: 'Solarized Dark',
    bg: '#002b36',
    fg: '#eee8d5',
    accent: '#cb4b16',
    ansi: {
      black: '#073642', red: '#dc322f', green: '#859900', yellow: '#b58900',
      blue: '#268bd2', magenta: '#d33682', cyan: '#2aa198', white: '#eee8d5',
      brightBlack: '#002b36', brightRed: '#cb4b16', brightGreen: '#586e75', brightYellow: '#657b83',
      brightBlue: '#839496', brightMagenta: '#6c71c4', brightCyan: '#93a1a1', brightWhite: '#fdf6e3',
    },
  },
  {
    name: 'Solarized Light',
    bg: '#fdf6e3',
    fg: '#586e75',
    accent: '#66b5a9',
    ansi: {
      black: '#073642', red: '#dc322f', green: '#859900', yellow: '#b58900',
      blue: '#268bd2', magenta: '#d33682', cyan: '#2aa198', white: '#eee8d5',
      brightBlack: '#002b36', brightRed: '#cb4b16', brightGreen: '#586e75', brightYellow: '#657b83',
      brightBlue: '#839496', brightMagenta: '#6c71c4', brightCyan: '#93a1a1', brightWhite: '#fdf6e3',
    },
  },
  {
    name: 'Gruvbox Dark',
    bg: '#282828',
    fg: '#ebdbb2',
    accent: '#fc802d',
    ansi: {
      black: '#282828', red: '#cc241d', green: '#98971a', yellow: '#d79921',
      blue: '#458588', magenta: '#b16286', cyan: '#689d6a', white: '#a89984',
      brightBlack: '#928374', brightRed: '#fb4934', brightGreen: '#b8bb26', brightYellow: '#fabd2f',
      brightBlue: '#83a598', brightMagenta: '#d3869b', brightCyan: '#8ec07c', brightWhite: '#ebdbb2',
    },
  },
  {
    name: 'Gruvbox Light',
    bg: '#fbf1c7',
    fg: '#3c3836',
    accent: '#ad3b14',
    ansi: {
      black: '#fbf1c7', red: '#cc241d', green: '#98971a', yellow: '#d79921',
      blue: '#458588', magenta: '#b16286', cyan: '#689d6a', white: '#7c6f64',
      brightBlack: '#928374', brightRed: '#9d0006', brightGreen: '#79740e', brightYellow: '#b57614',
      brightBlue: '#076678', brightMagenta: '#8f3f71', brightCyan: '#427b58', brightWhite: '#3c3836',
    },
  },
  {
    name: 'Adeberry',
    bg: '#1d2022',
    fg: '#e4eef5',
    accent: '#6c96b4',
    ansi: {
      black: '#121212', red: '#c76156', green: '#57c78a', yellow: '#c8a35a',
      blue: '#5785c7', magenta: '#c756a9', cyan: '#57c7c3', white: '#eeedeb',
      brightBlack: '#292929', brightRed: '#d22d1e', brightGreen: '#1ca05a', brightYellow: '#e5a01a',
      brightBlue: '#1458b8', brightMagenta: '#a43787', brightCyan: '#4d9989', brightWhite: '#ffffff',
    },
  },
]

export const DEFAULT_THEME_NAME = 'Phenomenon'

export function getTheme(name: string | undefined): Theme {
  if (name) {
    const found = THEMES.find((t) => t.name === name)
    if (found) return found
  }
  return THEMES[0]
}

/** Convert a hex `#rrggbb` to `rgba(r, g, b, a)`. xterm.js accepts both
 * forms but our chrome uses rgba for the fractional opacities. */
function alpha(hex: string, a: number): string {
  const v = hex.replace('#', '')
  const r = parseInt(v.slice(0, 2), 16)
  const g = parseInt(v.slice(2, 4), 16)
  const b = parseInt(v.slice(4, 6), 16)
  return `rgba(${r}, ${g}, ${b}, ${a})`
}

/** Build the xterm `ITheme` from a Helm theme. `ThemeAnsi`'s keys are
 * 1:1 with `ITheme`'s ANSI fields so the palette spreads in directly. */
export function xtermThemeFor(theme: Theme): ITheme {
  return {
    background: theme.bg,
    foreground: theme.fg,
    cursor: theme.accent,
    cursorAccent: theme.bg,
    selectionBackground: alpha(theme.accent, 0.22),
    selectionForeground: theme.fg,
    ...theme.ansi,
  }
}

/** Perceived brightness (relative luminance, simple sRGB approximation).
 * 0 = black, 1 = white. Used to decide whether a theme should drive the
 * app in dark or light mode — controls whether elevated surfaces lighten
 * or darken on top of the canvas. */
function luminance(hex: string): number {
  const v = hex.replace('#', '')
  const r = parseInt(v.slice(0, 2), 16) / 255
  const g = parseInt(v.slice(2, 4), 16) / 255
  const b = parseInt(v.slice(4, 6), 16) / 255
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

export function isLightTheme(theme: Theme): boolean {
  return luminance(theme.bg) > 0.55
}

/** Write the theme's base colours to `:root` and stamp
 * `data-theme-mode` so `tokens.css` flips its surface-mix direction
 * (lighten on dark themes, darken on light). All chrome derives from
 * those bases via `color-mix`, so this one call re-skins the app. */
export function applyThemeCssVars(theme: Theme): void {
  if (typeof document === 'undefined') return
  const root = document.documentElement
  root.style.setProperty('--terminal-bg', theme.bg)
  root.style.setProperty('--terminal-fg', theme.fg)
  root.style.setProperty('--terminal-accent', theme.accent)
  root.style.setProperty('--terminal-failed', theme.ansi.red)
  root.style.setProperty('--terminal-success', theme.ansi.green)
  root.style.setProperty('--terminal-warning', theme.ansi.yellow)
  root.dataset.themeMode = isLightTheme(theme) ? 'light' : 'dark'
}

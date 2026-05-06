/**
 * Action registry — the single source of truth for every user-facing
 * verb in the app. The keymap engine, the command palette, and (later)
 * the help cheatsheet all read from this surface.
 *
 * Two flavours of Action exist:
 *
 *   - **Static actions** declared in `actions/<area>.ts` modules and
 *     gathered into `STATIC_ACTIONS`. These are verbs (`window.kill`,
 *     `chrome.toggle-sidebar`) that don't depend on instance data —
 *     they look up "what window is active" themselves at run time.
 *
 *   - **Dynamic objects** projected from store state at palette open
 *     time. Each workspace, window, host, pin, etc. is materialised as
 *     an Action with the same shape but a `kind` that tags it as an
 *     object. Their `run` and `subActions` close over the specific
 *     instance.
 *
 * Both flow through the same fuzzy filter, the same Row renderer, and
 * the same drill-in handler. The kind discriminator only changes
 * cosmetic things (icon defaults, section grouping in the palette).
 */

export type ActionSource = 'palette' | 'keymap' | 'menu'

export interface ActionContext {
  /** Where the action was invoked from. Lets the action behave
   * differently depending on entry point (e.g. close the palette
   * after firing). */
  source: ActionSource
  /** Provided when invoked from the palette; the action calls this
   * to dismiss the palette before its own side effect. Undefined
   * when triggered via keymap or menu. */
  closePalette?: () => void
}

export type ActionKind =
  | 'action'    // verb (kill window, toggle sidebar)
  | 'workspace' // object — primary action is "switch to"
  | 'window'    // object — primary action is "jump to"
  | 'host'      // object — primary action is "make active"
  | 'pin'       // object — primary action is "jump to pinned window"

export interface Action {
  /** Stable id, dotted by area. Used as the key in the keymap override
   * map and in recents storage. Never user-visible. */
  id: string
  kind: ActionKind
  /** Primary text shown in the palette row. */
  label: string
  /** Muted, monospace tail rendered after the label, e.g.
   * "· iad-prod-01 · 2 windows". */
  sublabel?: string
  /** Single-character glyph rendered in the leading icon slot. Strings
   * keep the type free of React; if we later need real icons we'll
   * switch to ReactNode. */
  icon?: string
  /** Which sub-mode this action lives in, for the sigil-prefixed views
   * (`@workspaces`, `#windows`, `$hosts`). Static action verbs set this
   * to undefined — they appear in the no-sigil mode. */
  sigil?: '@' | '#' | '$'
  /** Default keyboard binding(s), in the form `Cmd+K`, `Cmd+Shift+W`,
   * `Cmd+]`. Pass an array when an action accepts aliases — e.g.
   * `['Cmd+]', 'Cmd+ArrowRight']` for next-window. The engine parses
   * each into a normalized combo at boot. User overrides from
   * `localStorage['helm.keymap']` replace the whole binding entry. */
  keybinding?: string | readonly string[]
  run: (ctx: ActionContext) => void | Promise<void>
  /** Returns false when the action shouldn't be offered (e.g. no active
   * host). Hides the row in the palette and ignores the key combo. */
  canRun?: () => boolean
  /** Destructive actions don't get a confirm step — they fire and push
   * an undo toast (Cmd+Z within ~5s reverses). The flag is here so the
   * palette can render a subtle warning glyph if we want to. */
  destructive?: boolean
  /** Bias for the fuzzy ranker. Positive boosts the row; defaults to 0.
   * Pinned windows in the quick switcher get +5, recents get +10, etc. */
  weight?: number
  /** When provided, drilling in (→ / Cmd+Enter) on this row opens an
   * inline list of these actions instead of running the primary. */
  subActions?: () => Action[]
  /** Treat plain Enter as drill-in. By default Enter runs the primary
   * action; set this true on actions whose primary purpose IS the
   * sub-list (e.g. the theme picker — there's nothing to "run" at
   * the top level, only a list to choose from). */
  drillOnEnter?: boolean
  /** Fires when this row becomes the highlighted one in the palette
   * (via ↑↓ or mouse hover). Used for live previews — e.g. the theme
   * picker applies a transient theme as you scroll through rows so
   * you see what you're picking before committing. The palette clears
   * any preview state on close, so this hook only needs to push
   * forward; it doesn't have to manage rollback. */
  onHighlight?: () => void
}

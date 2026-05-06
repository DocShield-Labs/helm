/**
 * Theme picker — one parent action ("Theme") that drills into a list of
 * the bundled palettes. Selecting a row swaps the active theme via the
 * store; xterm and the pane chrome both subscribe to `themeName` and
 * live-update without a remount.
 */

import { useStore } from '@lib/store'
import { THEMES } from '@lib/terminal'
import type { Action } from './types'

const slug = (name: string) => name.toLowerCase().replace(/[^a-z0-9]+/g, '-')

export const themeActions: Action[] = [
  {
    id: 'theme.picker',
    kind: 'action',
    label: 'Theme',
    icon: '◐',
    // The sublabel is computed at palette open time (when the static
    // list is materialised) so it always reflects the current
    // selection. A getter keeps the value reactive without needing the
    // palette to subscribe to the store directly.
    get sublabel() {
      return `· ${useStore.getState().themeName}`
    },
    drillOnEnter: true,
    run: () => {
      /* no-op; drill-in is the primary affordance */
    },
    subActions: () => {
      const current = useStore.getState().themeName
      return THEMES.map(
        (t): Action => ({
          id: `theme.set.${slug(t.name)}`,
          kind: 'action',
          label: t.name,
          icon: t.name === current ? '✓' : '·',
          // Cycle preview: highlighting a row applies the theme
          // transiently, so the user sees the palette change live as
          // they ↑↓. Esc out → palette closes → PaletteHost clears the
          // preview → app snaps back to the persisted theme.
          onHighlight: () => {
            useStore.getState().setPreviewThemeName(t.name)
          },
          // Enter persists. The preview is still set when run fires,
          // but App's effect uses `preview ?? themeName` so the
          // visual result is the same; the close-effect then wipes
          // the now-redundant preview state.
          run: () => {
            useStore.getState().setThemeName(t.name)
          },
        }),
      )
    },
  },
]

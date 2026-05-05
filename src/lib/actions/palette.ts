/**
 * Palette entry actions. Both keybindings open the same palette overlay;
 * they differ only in the initial query passed to it. Cmd+K boots empty
 * (default actions view). Cmd+P boots with `@#` so the palette opens
 * with the workspace + window filter chips already applied — the user
 * can backspace them off to land back in the default palette.
 */

import { useStore } from '@lib/store'
import type { Action } from './types'

export const paletteActions: Action[] = [
  {
    id: 'palette.open',
    kind: 'action',
    label: 'Command palette',
    icon: '⌘',
    keybinding: 'Cmd+k',
    run: () => {
      useStore.getState().openPalette()
    },
  },
  {
    id: 'switcher.open',
    kind: 'action',
    label: 'Quick switcher',
    sublabel: '· workspaces and windows',
    icon: '◫',
    keybinding: 'Cmd+p',
    run: () => {
      useStore.getState().openPalette('@#')
    },
  },
]

/**
 * Palette + quick-switcher entry actions. Both flip `paletteOpen` in
 * the store; PaletteHost subscribes and renders. Sub-modes (sigil
 * chips) are derived from the input value at runtime — they're not
 * actions of their own.
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
      useStore.getState().openPalette('actions')
    },
  },
  {
    id: 'switcher.open',
    kind: 'action',
    label: 'Quick switcher',
    icon: '◫',
    keybinding: 'Cmd+p',
    run: () => {
      useStore.getState().openPalette('switcher')
    },
  },
]

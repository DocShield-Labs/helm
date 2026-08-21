/**
 * App-chrome actions — verbs that don't belong to any single
 * host/workspace/window/palette: sidebar, undo, etc.
 */

import { useStore } from '@lib/store'
import type { Action } from './types'

export const chromeActions: Action[] = [
  {
    id: 'chrome.toggle-sidebar',
    kind: 'action',
    label: 'Toggle sidebar',
    icon: '⏵',
    keybinding: 'Cmd+\\',
    run: () => {
      useStore.getState().toggleSidebar()
    },
  },
  {
    id: 'chrome.undo',
    kind: 'action',
    label: 'Undo last action',
    icon: '↶',
    keybinding: 'Cmd+z',
    canRun: () => {
      // Only enabled when there's an in-flight toast carrying a
      // deferredAction. Dismissing the toast cancels the timer — the
      // ToastHost cleanup effect handles that on its own.
      return useStore.getState().toasts.some((t) => t.deferredAction)
    },
    run: () => {
      const state = useStore.getState()
      for (let i = state.toasts.length - 1; i >= 0; i--) {
        const t = state.toasts[i]
        if (!t.deferredAction) continue
        try {
          t.action?.onClick()
        } catch {
          /* ignore */
        }
        state.dismissToast(t.id)
        return
      }
    },
  },
]

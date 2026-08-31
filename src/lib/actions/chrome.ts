/**
 * App-chrome actions — verbs that don't belong to any single
 * host/session/palette: sidebar, undo, etc.
 */

import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'
import type { Action } from './types'

export const chromeActions: Action[] = [
  {
    id: 'chrome.copy-diagnostics',
    kind: 'action',
    label: 'Copy diagnostics',
    icon: '⚕',
    run: () => {
      void (async () => {
        const push = useStore.getState().pushToast
        try {
          const res = await commands.diagnostics()
          if (res.status !== 'ok') throw new Error(res.error)
          await navigator.clipboard.writeText(res.data)
          push({
            id: 'diagnostics',
            message: 'Diagnostics copied — paste into a bug report.',
            durationMs: 5_000,
          })
        } catch (error) {
          push({
            id: 'diagnostics',
            message: `Couldn't gather diagnostics: ${String(error)}`,
            durationMs: 8_000,
          })
        }
      })()
    },
  },
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

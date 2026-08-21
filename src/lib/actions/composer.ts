/**
 * Composer actions — the Terminal ⇄ Agent mode switch for the active
 * pane. The pane reports its effective mode to the composer store, so
 * this needs no knowledge of how the mode was derived.
 */

import { useStore } from '@lib/store'
import { toggleComposerMode } from '@lib/session/composer'
import { activeWindowSnapshot } from './window'
import type { Action } from './types'

function activePaneId(): { hostId: string; paneId: string } | null {
  const snap = activeWindowSnapshot()
  if (!snap) return null
  const panes = [...snap.workspace.panes.values()].filter((p) => p.windowId === snap.window.id)
  const pane = panes.find((p) => p.active) ?? panes[0]
  return pane ? { hostId: snap.hostId, paneId: pane.id } : null
}

export const composerActions: Action[] = [
  {
    id: 'composer.toggle-mode',
    kind: 'action',
    label: 'Toggle composer mode (Terminal ⇄ Agent)',
    icon: '⇄',
    keybinding: 'Cmd+i',
    canRun: () => useStore.getState().activeHostId !== null && activePaneId() !== null,
    run: () => {
      const p = activePaneId()
      if (p) toggleComposerMode(p.hostId, p.paneId)
    },
  },
]

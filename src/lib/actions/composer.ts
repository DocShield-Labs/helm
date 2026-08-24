/**
 * Composer actions — the Terminal ⇄ Agent mode switch for the active
 * session. The session reports its effective mode to the composer store, so
 * this needs no knowledge of how the mode was derived.
 */

import { toggleComposerMode } from '@lib/session/composer'
import { activeSessionSnapshot } from './session'
import type { Action } from './types'

export const composerActions: Action[] = [
  {
    id: 'composer.toggle-mode',
    kind: 'action',
    label: 'Toggle composer mode (Terminal ⇄ Agent)',
    icon: '⇄',
    keybinding: 'Cmd+i',
    canRun: () => activeSessionSnapshot() !== null,
    run: () => {
      const snapshot = activeSessionSnapshot()
      if (snapshot) toggleComposerMode(snapshot.hostId, snapshot.session.id)
    },
  },
]

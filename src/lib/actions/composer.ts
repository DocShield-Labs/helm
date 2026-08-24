/**
 * Composer actions — the Terminal ⇄ Agent mode switch for the active
 * session. The session reports its effective mode to the composer store, so
 * this needs no knowledge of how the mode was derived.
 */

import { useStore } from '@lib/store'
import { toggleComposerMode } from '@lib/session/composer'
import type { Action } from './types'

function activeSessionId(): { hostId: string; sessionId: string } | null {
  const state = useStore.getState()
  if (!state.activeHostId) return null
  const sessionId = state.sessions.get(state.activeHostId)?.activeSessionId
  return sessionId ? { hostId: state.activeHostId, sessionId } : null
}

export const composerActions: Action[] = [
  {
    id: 'composer.toggle-mode',
    kind: 'action',
    label: 'Toggle composer mode (Terminal ⇄ Agent)',
    icon: '⇄',
    keybinding: 'Cmd+i',
    canRun: () => activeSessionId() !== null,
    run: () => {
      const session = activeSessionId()
      if (session) toggleComposerMode(session.hostId, session.sessionId)
    },
  },
]

/**
 * Inbox-scoped actions. Currently just the keymap-driven "jump to oldest"
 * verb migrated from App.tsx. Per-notification rows in the palette are
 * deliberately out of scope this phase.
 */

import { selectSession } from '@lib/host'
import { useStore } from '@lib/store'
import type { Action } from './types'

/** Cmd+Shift+I — switch host if needed, then focus the session owning
 * the oldest live notification. Doesn't dismiss the notification;
 * dismissal happens when the user types into the focused session (handled
 * inside SessionView). */
export function jumpToOldestNotification(): void {
  const state = useStore.getState()
  let oldest: { hostId: string; sessionId: string; createdAt: number } | null = null
  for (const n of state.notifications.values()) {
    if (oldest === null || n.created_at < oldest.createdAt) {
      oldest = { hostId: n.host_id, sessionId: n.session_id, createdAt: n.created_at }
    }
  }
  if (!oldest) return
  if (state.sessions.get(oldest.hostId)?.sessions.has(oldest.sessionId)) {
    selectSession(oldest.hostId, oldest.sessionId)
  } else {
    state.setActiveHost(oldest.hostId)
  }
}

export const inboxActions: Action[] = [
  {
    id: 'inbox.jump-to-oldest',
    kind: 'action',
    label: 'Jump to oldest notification',
    icon: '⊙',
    keybinding: 'Cmd+Shift+I',
    canRun: () => useStore.getState().notifications.size > 0,
    run: jumpToOldestNotification,
  },
]

/**
 * Registry barrel. Re-exports types and assembles the static action list.
 * Dynamic projections (sessions and hosts) live in their
 * own modules and are pulled in by the palette at open time, not here.
 */

import type { Action } from './types'
import { chromeActions } from './chrome'
import { paletteActions } from './palette'
import { sessionActions } from './session'
import { inboxActions } from './inbox'
import { themeActions } from './theme'
import { composerActions } from './composer'
import { agentActions } from './agent'

export type { Action, ActionContext, ActionKind, ActionSource } from './types'

export const STATIC_ACTIONS: Action[] = [
  ...chromeActions,
  ...paletteActions,
  ...sessionActions,
  ...inboxActions,
  ...themeActions,
  ...agentActions,
  ...composerActions,
]

export function findActionById(id: string): Action | undefined {
  return STATIC_ACTIONS.find((a) => a.id === id)
}

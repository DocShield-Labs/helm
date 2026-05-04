/**
 * Registry barrel. Re-exports types and assembles the static action list.
 * Dynamic projections (workspaces, windows, hosts, pins) live in their
 * own modules and are pulled in by the palette at open time, not here.
 */

import type { Action } from './types'
import { chromeActions } from './chrome'
import { paletteActions } from './palette'
import { workspaceActions } from './workspace'
import { windowActions } from './window'
import { inboxActions } from './inbox'

export type { Action, ActionContext, ActionKind, ActionSource } from './types'

export const STATIC_ACTIONS: Action[] = [
  ...chromeActions,
  ...paletteActions,
  ...workspaceActions,
  ...windowActions,
  ...inboxActions,
]

export function findActionById(id: string): Action | undefined {
  return STATIC_ACTIONS.find((a) => a.id === id)
}

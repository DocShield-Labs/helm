/**
 * Global keymap engine.
 *
 * Subscribes a single document-level keydown listener at the App root
 * and routes matching events into actions from the registry. Combos are
 * normalized to lowercase strings of the form `cmd+shift+t`,
 * `cmd+]`, `cmd+arrowright`. User overrides come from
 * `localStorage['helm.keymap']` (mapping `actionId → string | string[]`)
 * and replace the action's default keybinding entry wholesale — there's
 * no merging, so a user can rebind without inheriting the default.
 *
 * The engine fires only when the event is a Cmd+ combo (`metaKey &&
 * !altKey && !ctrlKey`). This matches the existing app convention:
 * xterm vetoes Cmd+ keys at the terminal layer (see
 * `src/lib/terminal/index.ts:69-71`), so they're guaranteed to reach
 * us. Non-Cmd shortcuts can be added later by relaxing this gate; until
 * then, the engine and the terminal stay in lockstep.
 */

import { useEffect, useMemo } from 'react'
import { STATIC_ACTIONS, type Action } from '@lib/actions'

const OVERRIDES_KEY = 'helm.keymap'

type ComboTable = Map<string, Action>

function asArray(b: string | readonly string[] | undefined): readonly string[] {
  if (!b) return []
  return typeof b === 'string' ? [b] : b
}

function normalizeCombo(s: string): string {
  return s.trim().toLowerCase()
}

function readOverrides(): Map<string, string[]> {
  try {
    const raw = localStorage.getItem(OVERRIDES_KEY)
    if (!raw) return new Map()
    const parsed = JSON.parse(raw)
    if (typeof parsed !== 'object' || parsed === null) return new Map()
    const out = new Map<string, string[]>()
    for (const [id, val] of Object.entries(parsed)) {
      if (typeof val === 'string') out.set(id, [val])
      else if (Array.isArray(val) && val.every((x) => typeof x === 'string')) {
        out.set(id, val as string[])
      }
    }
    return out
  } catch {
    return new Map()
  }
}

function buildComboTable(actions: readonly Action[]): ComboTable {
  const overrides = readOverrides()
  const table: ComboTable = new Map()
  for (const action of actions) {
    const bindings = overrides.get(action.id) ?? asArray(action.keybinding)
    for (const raw of bindings) {
      const combo = normalizeCombo(raw)
      if (!combo) continue
      // First-write-wins on conflicts: later actions silently lose.
      // Matches how the old switch statement would have been
      // ordered — first case clause wins.
      if (!table.has(combo)) table.set(combo, action)
    }
  }
  return table
}

/** Build the canonical combo string for a keyboard event. Only fires
 * for cmd-modified events (the engine's gate); returns empty string
 * for plain modifiers like just Shift or just Cmd held alone. */
function comboFromEvent(e: KeyboardEvent): string {
  if (!e.metaKey || e.altKey || e.ctrlKey) return ''
  const key = e.key.toLowerCase()
  // Bare modifier presses arrive with key === 'meta' / 'shift' / etc.
  // Don't emit a combo for those — the user hasn't actually triggered
  // a binding yet, they're mid-chord.
  if (key === 'meta' || key === 'shift' || key === 'alt' || key === 'control') {
    return ''
  }
  const parts: string[] = ['cmd']
  if (e.shiftKey) parts.push('shift')
  parts.push(key)
  return parts.join('+')
}

/** Attach the global handler. Returns nothing — the cleanup happens in
 * the effect's teardown automatically. Called once at App root. */
export function useGlobalKeymap(): void {
  // Build the table once per process. Static action keybindings don't
  // change at runtime; user overrides only change on rebind, which
  // (until the Settings tab ships) requires a manual reload anyway.
  const table = useMemo(() => buildComboTable(STATIC_ACTIONS), [])

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const combo = comboFromEvent(e)
      if (!combo) return
      const action = table.get(combo)
      if (!action) return
      if (action.canRun && !action.canRun()) return
      e.preventDefault()
      void action.run({ source: 'keymap' })
    }
    document.addEventListener('keydown', handler, true)
    return () => document.removeEventListener('keydown', handler, true)
  }, [table])
}

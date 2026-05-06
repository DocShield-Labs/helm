/**
 * Command palette orchestrator.
 *
 * Owns transient input state (query, selected index, drilled-in object),
 * parses the run of leading `@` / `#` / `$` sigils into an ordered list
 * of sub-mode chips, picks a result source (static actions, the union
 * of the active projections, the drilled object's subActions, or
 * recents+actions for the empty state), runs them through the fuzzy
 * ranker, and dispatches the chosen action. Open/close state lives in
 * the store so the keymap engine can flip it via `palette.open`. A
 * paired `paletteInitialQuery` lets entry actions seed the input —
 * Cmd+P opens with `'@#'` so the workspace + window chips are already
 * applied.
 *
 * Esc walks back one navigation step at a time: drill-out if drilled
 * in, otherwise peel the rightmost chip off. Once there's nothing
 * left to pop, Esc closes the palette. Backspace-on-empty does the
 * same step-pop (just doesn't close at the bottom).
 *
 * The renderer treats the matched list as a flat sequence of items
 * carrying optional section headers — that way ↑↓ navigation only ever
 * moves through `actions` (skipping headers) and `selected` is a simple
 * index into that array.
 */

import { useEffect, useMemo, useState } from 'react'
import { useStore } from '@lib/store'
import { STATIC_ACTIONS, findActionById, type Action } from '@lib/actions'
import {
  resolveSigil,
  type Sigil,
  type SubModeResult,
} from '@lib/actions/dynamic'
import { Palette } from '@ui'
import { Row, Kbd } from './Row'
import { SectionHeader } from './SectionHeader'
import { Footer } from './Footer'
import { fuzzyMatch } from './fuzzy'
import { getRecents, pushRecent, RECENTS_DISPLAY_LIMIT } from './recents'

interface ScoredAction {
  action: Action
  score: number
}

/** One row in the rendered list. A `header` carries no action and
 * doesn't participate in keyboard navigation; an `item` does. */
type ListEntry =
  | { type: 'header'; label: string; count?: number }
  | { type: 'item'; action: Action }

const SIGILS = ['@', '#', '$'] as const

function rankActions(query: string, actions: readonly Action[]): ScoredAction[] {
  const out: ScoredAction[] = []
  for (const action of actions) {
    if (action.canRun && !action.canRun()) continue
    const labelMatch = fuzzyMatch(query, action.label)
    const subMatch = action.sublabel ? fuzzyMatch(query, action.sublabel) : null
    const m = labelMatch ?? subMatch
    if (!m) continue
    const base = labelMatch?.score ?? (subMatch ? subMatch.score - 2 : 0)
    out.push({ action, score: base + (action.weight ?? 0) })
  }
  out.sort((a, b) => b.score - a.score)
  return out
}

/** Consume every leading sigil in `raw` into an ordered list of
 * sub-mode projections. The query string remains the source of truth
 * for which chips are active — `'@#term'` resolves to `[@workspaces,
 * #windows]` + residual `'term'`. Repeats are deduped on character so
 * `'@@foo'` doesn't render two identical chips. */
function parseInput(raw: string): { subs: SubModeResult[]; residual: string } {
  const subs: SubModeResult[] = []
  const seen = new Set<string>()
  let i = 0
  while (i < raw.length && (SIGILS as readonly string[]).includes(raw[i])) {
    const c = raw[i] as Sigil
    if (!seen.has(c)) {
      seen.add(c)
      subs.push(resolveSigil(c))
    }
    i++
  }
  let residual = raw.slice(i)
  if (residual.startsWith(' ')) residual = residual.slice(1)
  return { subs, residual }
}

/** Count the leading sigil characters (including duplicates) so
 * Backspace can strip exactly one off the rightmost end of the query. */
function leadingSigilCount(raw: string): number {
  let i = 0
  while (i < raw.length && (SIGILS as readonly string[]).includes(raw[i])) i++
  return i
}

function renderBinding(binding: string | readonly string[] | undefined) {
  if (!binding) return null
  const first = typeof binding === 'string' ? binding : binding[0]
  if (!first) return null
  const parts = first.split('+').map((p) => p.trim())
  return (
    <>
      {parts.map((p, i) => {
        const lower = p.toLowerCase()
        const glyph =
          lower === 'cmd' ? '⌘'
          : lower === 'shift' ? '⇧'
          : lower === 'alt' ? '⌥'
          : lower === 'ctrl' ? '⌃'
          : p
        return <Kbd key={i}>{glyph}</Kbd>
      })}
    </>
  )
}

interface ComputeEntriesArgs {
  drilled: Action | null
  subs: readonly SubModeResult[]
  residual: string
  ranked: readonly ScoredAction[]
}

/** Produce the rendered (header + item) sequence for the current
 * palette state. Pure: no React, no store reads. The caller wraps this
 * in `useMemo` with the appropriate dep array. */
function computeEntries({ drilled, subs, residual, ranked }: ComputeEntriesArgs): ListEntry[] {
  // Drilled in: just the subActions list, no headers.
  if (drilled) {
    return ranked.map(({ action }) => ({ type: 'item', action }))
  }

  // Single sigil sub-mode with grouping (e.g. @workspaces, $hosts).
  // We only keep group headers when exactly one sub is active —
  // mixing groups across two unrelated projections (e.g. workspaces
  // grouped by host alongside ungrouped windows) tends to look noisy,
  // so the multi-chip case falls through to a flat list.
  if (subs.length === 1 && subs[0].groups) {
    const groups = subs[0].groups
    const out: ListEntry[] = []
    let lastLabel: string | null = null
    for (const { action } of ranked) {
      const group = groups.get(action.id)
      if (group && group.label !== lastLabel) {
        out.push({ type: 'header', label: group.label, count: group.count })
        lastLabel = group.label
      }
      out.push({ type: 'item', action })
    }
    return out
  }

  // Empty palette (no chips, no typed text) → RECENTS section then ACTIONS.
  if (subs.length === 0 && residual.length === 0) {
    const recents: Action[] = []
    for (const id of getRecents()) {
      const a = findActionById(id)
      if (!a) continue
      if (a.canRun && !a.canRun()) continue
      recents.push(a)
      if (recents.length >= RECENTS_DISPLAY_LIMIT) break
    }
    const recentIds = new Set(recents.map((a) => a.id))
    const tail = ranked.filter(({ action }) => !recentIds.has(action.id))
    const out: ListEntry[] = []
    if (recents.length > 0) {
      out.push({ type: 'header', label: 'Recents' })
      for (const a of recents) out.push({ type: 'item', action: a })
    }
    if (tail.length > 0) {
      if (recents.length > 0) out.push({ type: 'header', label: 'Actions' })
      for (const { action } of tail) out.push({ type: 'item', action })
    }
    return out
  }

  // Flat list — typed actions-mode, sigil mode without groups, etc.
  return ranked.map(({ action }) => ({ type: 'item', action }))
}

export function PaletteHost() {
  const open = useStore((s) => s.paletteOpen)
  const initialQuery = useStore((s) => s.paletteInitialQuery)
  const close = useStore((s) => s.closePalette)
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState(0)
  /** Object the user has drilled into (→ / Cmd+Enter on a result). The
   * palette swaps its source list for `drilled.subActions()`; Esc
   * pops back to the parent view, and Backspace-on-empty does the
   * same for keyboard-only navigation. */
  const [drilled, setDrilled] = useState<Action | null>(null)

  // Seed the query from `paletteInitialQuery` on each open. Cmd+K
  // passes empty; Cmd+P passes `'@#'` so the palette boots with the
  // workspace + window chips already applied.
  useEffect(() => {
    if (open) {
      setQuery(initialQuery)
      setSelected(0)
      setDrilled(null)
    }
  }, [open, initialQuery])

  const { subs, residual } = useMemo(
    () => (open ? parseInput(query) : { subs: [] as SubModeResult[], residual: '' }),
    [open, query],
  )

  // Source list:
  //   - drill-in → the drilled object's subActions
  //   - one or more sigil chips → union of those projections (deduped
  //     by action id; first-seen wins so the chip order controls
  //     iteration order — `@#` lists workspaces first, `#@` lists
  //     windows first)
  //   - otherwise → the static registry (default Cmd+K view)
  const sourceActions = useMemo<readonly Action[]>(() => {
    if (drilled?.subActions) return drilled.subActions()
    if (subs.length > 0) {
      const seen = new Set<string>()
      const out: Action[] = []
      for (const s of subs) {
        for (const a of s.actions) {
          if (seen.has(a.id)) continue
          seen.add(a.id)
          out.push(a)
        }
      }
      return out
    }
    return STATIC_ACTIONS
  }, [drilled, subs])

  const ranked = useMemo(
    () => (open ? rankActions(residual, sourceActions) : []),
    [open, residual, sourceActions],
  )

  const entries = useMemo<ListEntry[]>(
    () => (open ? computeEntries({ drilled, subs, residual, ranked }) : []),
    [open, drilled, subs, residual, ranked],
  )

  // Index map: position in `entries` of the i-th item-row, used to map
  // `selected` (a 0-based item index) back to the right ListEntry.
  const itemPositions = useMemo(() => {
    const out: number[] = []
    entries.forEach((e, i) => {
      if (e.type === 'item') out.push(i)
    })
    return out
  }, [entries])

  const itemCount = itemPositions.length

  useEffect(() => {
    if (selected > 0 && selected >= itemCount) {
      setSelected(Math.max(0, itemCount - 1))
    }
  }, [itemCount, selected])

  const itemAt = (idx: number): Action | undefined => {
    const pos = itemPositions[idx]
    if (pos === undefined) return undefined
    const e = entries[pos]
    return e.type === 'item' ? e.action : undefined
  }

  // Live preview hook. When the highlighted row changes, fire its
  // `onHighlight` callback. Used by the theme picker to apply a
  // transient theme as the user scrolls through rows. When NO row is
  // highlighted (filter empties the list, etc.), drop any pending
  // preview — otherwise typing a non-matching query while a theme is
  // previewed would leave the preview stuck until palette close.
  const highlightedAction = itemAt(selected)
  useEffect(() => {
    if (!open) return
    if (highlightedAction) {
      highlightedAction.onHighlight?.()
    } else {
      useStore.getState().setPreviewThemeName(null)
    }
    // Depending only on the action id keeps the effect quiet during
    // unrelated re-renders. The eslint warning would have us depend
    // on `highlightedAction` directly, but action objects are
    // re-created every ranking pass even when the row is the same.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, highlightedAction?.id])

  // Clear any transient preview state when the palette closes (Esc,
  // outside-click, after running an action). The theme picker relies
  // on this — Esc reverts to the persisted theme; Enter persists via
  // the action's `run`, then close fires here and the redundant
  // preview is wiped. Other onHighlight users get the same lifecycle.
  useEffect(() => {
    if (open) return
    useStore.getState().setPreviewThemeName(null)
  }, [open])

  const run = (action: Action) => {
    pushRecent(action.id)
    close()
    void action.run({ source: 'palette', closePalette: close })
  }

  const popDrill = () => {
    setDrilled(null)
    setQuery('')
    setSelected(0)
  }

  const drillInto = (action: Action) => {
    if (!action.subActions) return
    setDrilled(action)
    setQuery('')
    setSelected(0)
  }

  /** Pop one navigation step. Drilled view → its parent. Otherwise
   * peel the rightmost chip off. Returns false when there's nothing
   * left to pop (caller closes). */
  const popOneStep = (): boolean => {
    if (drilled) {
      popDrill()
      return true
    }
    const sigilCount = leadingSigilCount(query)
    if (sigilCount > 0) {
      const next = query.slice(0, sigilCount - 1) + query.slice(sigilCount)
      setQuery(next)
      return true
    }
    return false
  }

  const onInputKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    // Esc and Backspace-on-empty both step backwards through the
    // nav stack: drill-out, then peel chips one at a time, then
    // (Esc only) close once we hit the bottom.
    if (e.key === 'Escape') {
      e.preventDefault()
      if (!popOneStep()) close()
      return
    }
    if (e.key === 'Backspace' && residual.length === 0) {
      if (popOneStep()) e.preventDefault()
      return
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setSelected((s) => Math.min(itemCount - 1, s + 1))
      return
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault()
      setSelected((s) => Math.max(0, s - 1))
      return
    }
    // → and Cmd+Enter always drill into a result with subActions.
    // Plain Enter normally runs the primary, but actions whose whole
    // purpose is the sub-list (e.g. the theme picker) opt into
    // drill-on-enter so users don't have to learn Cmd+Enter for them.
    if (e.key === 'ArrowRight' || (e.key === 'Enter' && e.metaKey)) {
      const target = itemAt(selected)
      if (target?.subActions) {
        e.preventDefault()
        drillInto(target)
        return
      }
    }
    if (e.key === 'Enter') {
      const target = itemAt(selected)
      if (!target) return
      e.preventDefault()
      if (target.subActions && target.drillOnEnter) {
        drillInto(target)
      } else {
        run(target)
      }
    }
  }

  let body: React.ReactNode
  if (entries.length === 0) {
    body = (
      <div
        className="px-3 py-6 text-center text-[12px]"
        style={{ color: 'var(--text-tertiary)' }}
      >
        No matches.
      </div>
    )
  } else {
    let itemIdx = 0
    body = entries.map((e, i) => {
      if (e.type === 'header') {
        return <SectionHeader key={`hdr.${i}.${e.label}`} label={e.label} count={e.count} />
      }
      const idx = itemIdx++
      return (
        <Row
          key={e.action.id}
          label={e.action.label}
          sublabel={e.action.sublabel}
          icon={e.action.icon}
          kbd={renderBinding(e.action.keybinding)}
          selected={idx === selected}
          onMouseEnter={() => setSelected(idx)}
          onClick={() => run(e.action)}
        />
      )
    })
  }

  // Chip rules:
  //   - drilled in → single "↳ <parent label>" breadcrumb chip,
  //     overrides any sigil chips
  //   - one or more sigils → one chip per active sub, in the order
  //     they appear in the query
  //   - otherwise → no chips
  const chips = drilled ? [`↳ ${drilled.label}`] : subs.map((s) => s.chip)

  // Reconstruct the query when the user types. The visible input
  // value is just the residual; we have to re-prepend the sigil run
  // so backing state stays consistent.
  const sigilPrefix = drilled ? '' : query.slice(0, leadingSigilCount(query))

  return (
    <Palette
      open={open}
      query={residual}
      onQueryChange={(v) => {
        if (drilled) setQuery(v)
        else setQuery(sigilPrefix + v)
      }}
      onClose={close}
      onInputKeyDown={onInputKeyDown}
      chips={chips}
      footer={<Footer />}
    >
      {body}
    </Palette>
  )
}

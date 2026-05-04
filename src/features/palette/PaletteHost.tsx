/**
 * Command palette orchestrator.
 *
 * Owns transient input state (query, selected index, drilled-in object),
 * parses the leading `@` / `#` / `$` sigil into a sub-mode chip, picks a
 * result source (static actions, dynamic projection, or recents+actions
 * for the empty state), runs them through the fuzzy ranker, and
 * dispatches the chosen action. Open/close state lives in the store so
 * the keymap engine can flip it via the `palette.open` action.
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
  workspacesAsActions,
  windowsAsActions,
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

function parseInput(raw: string): { sub: SubModeResult | null; residual: string } {
  const first = raw[0] as Sigil | undefined
  if (first && (SIGILS as readonly string[]).includes(first)) {
    const sub = resolveSigil(first)
    let residual = raw.slice(1)
    if (residual.startsWith(' ')) residual = residual.slice(1)
    return { sub, residual }
  }
  return { sub: null, residual: raw }
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
  sub: SubModeResult | null
  mode: 'actions' | 'switcher'
  residual: string
  ranked: readonly ScoredAction[]
}

/** Produce the rendered (header + item) sequence for the current
 * palette state. Pure: no React, no store reads. The caller wraps this
 * in `useMemo` with the appropriate dep array. */
function computeEntries({ drilled, sub, mode, residual, ranked }: ComputeEntriesArgs): ListEntry[] {
  // Drilled in: just the subActions list, no headers.
  if (drilled) {
    return ranked.map(({ action }) => ({ type: 'item', action }))
  }

  // Sigil sub-mode with grouping (e.g. @workspaces, $hosts).
  if (sub?.groups) {
    const out: ListEntry[] = []
    let lastLabel: string | null = null
    for (const { action } of ranked) {
      const group = sub.groups.get(action.id)
      if (group && group.label !== lastLabel) {
        out.push({ type: 'header', label: group.label, count: group.count })
        lastLabel = group.label
      }
      out.push({ type: 'item', action })
    }
    return out
  }

  // Empty actions-mode → RECENTS section then ACTIONS.
  if (!sub && mode === 'actions' && residual.length === 0) {
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
  const mode = useStore((s) => s.paletteMode)
  const close = useStore((s) => s.closePalette)
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState(0)
  /** Object the user has drilled into (→ / Cmd+Enter on a result). The
   * palette swaps its source list for `drilled.subActions()`; Esc and
   * Backspace-on-empty pop back to the parent view. */
  const [drilled, setDrilled] = useState<Action | null>(null)

  useEffect(() => {
    if (open) {
      setQuery('')
      setSelected(0)
      setDrilled(null)
    }
  }, [open])

  const { sub, residual } = useMemo(
    () => (open ? parseInput(query) : { sub: null, residual: '' }),
    [open, query],
  )

  // Source list:
  //   - drill-in → the drilled object's subActions
  //   - sigil chip → that sub-mode's projection
  //   - switcher mode → workspaces ∪ windows (windows already carry
  //     pin-aware weight/icon via `windowsAsActions`)
  //   - actions mode → the static registry
  const sourceActions = useMemo<readonly Action[]>(() => {
    if (drilled?.subActions) return drilled.subActions()
    if (sub) return sub.actions
    if (mode === 'switcher') {
      return [...workspacesAsActions().actions, ...windowsAsActions().actions]
    }
    return STATIC_ACTIONS
  }, [drilled, sub, mode])

  const ranked = useMemo(
    () => (open ? rankActions(residual, sourceActions) : []),
    [open, residual, sourceActions],
  )

  const entries = useMemo<ListEntry[]>(
    () => (open ? computeEntries({ drilled, sub, mode, residual, ranked }) : []),
    [open, drilled, sub, mode, residual, ranked],
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

  const onInputKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      if (drilled) popDrill()
      else close()
      return
    }
    if (e.key === 'Backspace' && residual.length === 0) {
      if (drilled) {
        e.preventDefault()
        popDrill()
        return
      }
      if (sub) {
        e.preventDefault()
        setQuery('')
        return
      }
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
    // → and Cmd+Enter drill into the highlighted object.
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
      if (target) {
        e.preventDefault()
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
  //   - drilled in → "↳ <parent label>", overrides any sigil chip
  //   - sigil active → that sub-mode's chip
  //   - otherwise → no chip
  const chip = drilled ? `↳ ${drilled.label}` : sub?.chip

  return (
    <Palette
      open={open}
      query={residual}
      onQueryChange={(v) => {
        // While drilled, query is a free-text fuzzy filter over the
        // subActions — no sigil prefix to preserve.
        if (drilled) setQuery(v)
        else setQuery(sub ? `${sub.chip[0]}${v}` : v)
      }}
      onClose={close}
      onInputKeyDown={onInputKeyDown}
      chip={chip}
      footer={<Footer />}
    >
      {body}
    </Palette>
  )
}

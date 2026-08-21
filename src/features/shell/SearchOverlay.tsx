/**
 * SearchOverlay — in-pane find (Cmd+F).
 *
 * A compact floating bar pinned to the pane's top-right (browser-find /
 * VS Code style). Searches two surfaces as one: the finished blocks
 * (DOM, via the CSS Custom Highlight API) and the live xterm tail (via
 * SearchAddon). Matches are ordered oldest→newest: DOM matches first,
 * then the tail. Enter / Shift+Enter step through them with a live n/m
 * counter; case-sensitive + regex toggles; Esc (or ×) clears and hands
 * focus back to the terminal.
 *
 * Highlight colours are read from the live theme tokens so matches read
 * well on whatever palette is active.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import type { ISearchOptions } from '@xterm/addon-search'
import type { HelmTerminal } from '@lib/terminal'
import { applyHighlights, clearHighlights, findInBlocks, scrollRangeIntoView } from './domFind'

interface SearchOverlayProps {
  helm: HelmTerminal
  /** Scroll container holding the DOM blocks (searched alongside xterm). */
  container?: HTMLElement | null
  onClose: () => void
}

export function SearchOverlay({ helm, container, onClose }: SearchOverlayProps) {
  const [query, setQuery] = useState('')
  const [caseSensitive, setCaseSensitive] = useState(false)
  const [regex, setRegex] = useState(false)
  const [result, setResult] = useState<{ index: number; count: number }>({ index: -1, count: 0 })
  // DOM-block matches + which one is active (-1 = none; the tail owns
  // the active match when domIndex === -1 and xterm has results).
  const domRanges = useRef<Range[]>([])
  const [domCount, setDomCount] = useState(0)
  const [domIndex, setDomIndex] = useState(-1)
  const inputRef = useRef<HTMLInputElement>(null)

  // Match-highlight colours from theme tokens: the active match takes the
  // warning/amber swatch (high salience), other matches a translucent
  // accent. Computed once on mount.
  const decorations = useMemo<ISearchOptions['decorations']>(() => {
    const root = getComputedStyle(document.documentElement)
    const accent = root.getPropertyValue('--terminal-accent').trim() || '#3780e9'
    const warning = root.getPropertyValue('--terminal-warning').trim() || '#e5a01a'
    return {
      matchBackground: `${accent}59`, // ~35% accent
      matchOverviewRuler: accent,
      activeMatchBackground: warning,
      activeMatchColorOverviewRuler: warning,
    }
  }, [])

  const options = useMemo<ISearchOptions>(
    () => ({ caseSensitive, regex, decorations }),
    [caseSensitive, regex, decorations],
  )

  // Live match count for the n/m readout.
  useEffect(() => {
    const sub = helm.search.onDidChangeResults((r) => {
      setResult({ index: r.resultIndex, count: r.resultCount })
    })
    return () => sub.dispose()
  }, [helm.search])

  useEffect(() => {
    requestAnimationFrame(() => {
      inputRef.current?.focus()
      inputRef.current?.select()
    })
  }, [])

  // Run the search whenever the query or a toggle changes, always landing
  // on the most recent (last) match — for a terminal the newest output is
  // what you're usually after. clearDecorations() resets the selection so
  // findPrevious starts from the bottom of the buffer; doing this on every
  // change also makes toggles deterministic (the active match can't drift
  // by one on each flip the way it would if we re-issued from the current
  // selection). Enter / Shift+Enter step from there.
  // Debounced: a TreeWalker over every block plus an xterm buffer scan
  // per keystroke is too much to run synchronously in the input handler.
  useEffect(() => {
    if (query === '') {
      domRanges.current = []
      setDomCount(0)
      setDomIndex(-1)
      clearHighlights()
      helm.search.clearDecorations()
      setResult({ index: -1, count: 0 })
      return
    }
    const id = window.setTimeout(() => {
      const ranges = container ? findInBlocks(container, query, { caseSensitive, regex }) : []
      domRanges.current = ranges
      setDomCount(ranges.length)
      setDomIndex(-1)
      // Newest match first: the tail if it has one, else the last block hit.
      helm.search.clearDecorations()
      const found = helm.search.findPrevious(query, options)
      if (!found && ranges.length > 0) {
        setDomIndex(ranges.length - 1)
        applyHighlights(ranges, ranges.length - 1)
        scrollRangeIntoView(ranges[ranges.length - 1])
      } else {
        applyHighlights(ranges, -1)
      }
    }, 60)
    return () => {
      window.clearTimeout(id)
      clearHighlights()
    }
  }, [query, options, caseSensitive, regex, helm.search, container])

  // Unified stepping: [dom 0..n-1] then [tail 0..m-1].
  const total = domCount + result.count
  const unifiedIndex =
    domIndex >= 0 ? domIndex : result.count > 0 ? domCount + Math.max(0, result.index) : -1

  const goTo = (idx: number) => {
    const ranges = domRanges.current
    if (total === 0) return
    const i = ((idx % total) + total) % total
    if (i < ranges.length) {
      helm.search.clearDecorations()
      setDomIndex(i)
      applyHighlights(ranges, i)
      scrollRangeIntoView(ranges[i])
    } else {
      // The addon steps from its current selection; coming in from the
      // blocks, a cleared selection makes findNext/findPrevious land on
      // the tail's first/last match respectively.
      const wasInTail = domIndex < 0 && result.count > 0
      setDomIndex(-1)
      applyHighlights(ranges, -1)
      if (!wasInTail) helm.search.clearDecorations()
      if (idx > unifiedIndex) helm.search.findNext(query, options)
      else helm.search.findPrevious(query, options)
      helm.term.element?.scrollIntoView({ block: 'nearest' })
    }
  }
  const next = () => {
    if (!query) return
    goTo(unifiedIndex + 1)
  }
  const prev = () => {
    if (!query) return
    goTo(unifiedIndex - 1)
  }
  const close = () => {
    clearHighlights()
    helm.search.clearDecorations()
    helm.term.focus()
    onClose()
  }

  const counter =
    total === 0 ? (query ? 'no results' : '') : `${unifiedIndex + 1}/${total}`

  return (
    <div
      className="absolute right-3 top-3 z-20 flex w-[420px] max-w-[calc(100%-1.5rem)] items-center gap-1 rounded-lg border border-white/[0.08] bg-elevated px-1.5 py-1"
      style={{ boxShadow: 'var(--elevation-2)' }}
      onClick={(e) => e.stopPropagation()}
    >
      <input
        ref={inputRef}
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          e.stopPropagation()
          if (e.key === 'Enter') {
            e.preventDefault()
            if (e.shiftKey) prev()
            else next()
          } else if (e.key === 'Escape') {
            e.preventDefault()
            close()
          }
        }}
        placeholder="Find"
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        className="min-w-0 flex-1 rounded-md bg-canvas px-2 py-1 text-[13px] text-text-primary outline-none placeholder:text-text-tertiary"
      />
      <span className="min-w-[46px] px-1 text-center font-mono text-[11px] tabular-nums text-text-tertiary">
        {counter}
      </span>
      <ToggleButton active={caseSensitive} onClick={() => setCaseSensitive((v) => !v)} title="Match case">
        Aa
      </ToggleButton>
      <ToggleButton active={regex} onClick={() => setRegex((v) => !v)} title="Use regular expression">
        .*
      </ToggleButton>
      <IconButton onClick={prev} title="Previous match (⇧⏎)">
        ↑
      </IconButton>
      <IconButton onClick={next} title="Next match (⏎)">
        ↓
      </IconButton>
      <IconButton onClick={close} title="Close (Esc)">
        ×
      </IconButton>
    </div>
  )
}

function ToggleButton({
  active,
  onClick,
  title,
  children,
}: {
  active: boolean
  onClick: () => void
  title: string
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className={`flex h-6 min-w-6 items-center justify-center rounded-md px-1 font-mono text-[11px] leading-none ${
        active
          ? 'bg-accent-muted text-text-primary'
          : 'text-text-tertiary hover:bg-white/[0.06] hover:text-text-secondary'
      }`}
    >
      {children}
    </button>
  )
}

function IconButton({
  onClick,
  title,
  children,
}: {
  onClick: () => void
  title: string
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className="flex h-6 w-6 items-center justify-center rounded-md font-mono text-[13px] leading-none text-text-tertiary hover:bg-white/[0.06] hover:text-text-secondary"
    >
      {children}
    </button>
  )
}

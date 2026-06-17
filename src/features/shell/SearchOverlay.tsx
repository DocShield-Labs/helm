/**
 * SearchOverlay — in-pane find (Cmd+F).
 *
 * A compact floating bar pinned to the pane's top-right (browser-find /
 * VS Code style). Drives xterm's SearchAddon: incremental highlight as
 * you type, Enter / Shift+Enter to step through matches, a live n/m
 * counter, and case-sensitive + regex toggles. Esc (or ×) clears the
 * highlights and hands focus back to the terminal.
 *
 * Highlight colours are read from the live theme tokens so matches read
 * well on whatever palette is active.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import type { ISearchOptions } from '@xterm/addon-search'
import type { HelmTerminal } from '@lib/terminal'

interface SearchOverlayProps {
  helm: HelmTerminal
  onClose: () => void
}

export function SearchOverlay({ helm, onClose }: SearchOverlayProps) {
  const [query, setQuery] = useState('')
  const [caseSensitive, setCaseSensitive] = useState(false)
  const [regex, setRegex] = useState(false)
  const [result, setResult] = useState<{ index: number; count: number }>({ index: -1, count: 0 })
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
  useEffect(() => {
    if (query === '') {
      helm.search.clearDecorations()
      setResult({ index: -1, count: 0 })
      return
    }
    helm.search.clearDecorations()
    helm.search.findPrevious(query, options)
  }, [query, options, helm.search])

  const next = () => {
    if (query) helm.search.findNext(query, options)
  }
  const prev = () => {
    if (query) helm.search.findPrevious(query, options)
  }
  const close = () => {
    helm.search.clearDecorations()
    helm.term.focus()
    onClose()
  }

  const counter =
    result.count === 0 ? (query ? 'no results' : '') : `${result.index + 1}/${result.count}`

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

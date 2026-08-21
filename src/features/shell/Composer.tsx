/**
 * The composer — Helm's input. Warp's universal input, in our palette:
 * a bordered box above the bottom edge with context chips, a growing
 * mono editor, and a Terminal | Agent control that says where ⏎ sends
 * the text.
 *
 *   Terminal  → the shell (`cmd⏎`; multi-line as a bracketed paste)
 *   Agent     → the agent running in this pane, or — from a shell —
 *               launches one with the text as its prompt
 *
 * The editor is a textarea (native selection, paste, undo, IME); the
 * few terminal keys that make sense on an empty editor (^C ^D ^L, Esc
 * for an agent) pass straight through to the pane.
 */

import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from 'react'
import { homeRelative } from '@lib/path'
import type { ComposerMode } from '@lib/session/composer'
import type { PaneKind } from '@lib/session/paneState'
import { BranchIcon, FolderIcon } from '@features/sessions/icons'

export interface ComposerProps {
  mode: ComposerMode
  kind: PaneKind
  cwd: string | null | undefined
  branch: string | null | undefined
  /** Previous command lines, oldest first (Terminal mode ↑/↓). */
  history: readonly string[]
  /** Label of the agent the Agent mode talks to ("claude"). */
  agentName: string
  onModeChange: (mode: ComposerMode) => void
  onSend: (text: string) => void
  /** Raw bytes for the pane (control keys on an empty editor). */
  onRaw: (bytes: string) => void
  /** Bumps to pull focus (pane became visible, mode re-opened). */
  focusKey: number
}

const MAX_ROWS = 8

export function Composer({
  mode,
  kind,
  cwd,
  branch,
  history,
  agentName,
  onModeChange,
  onSend,
  onRaw,
  focusKey,
}: ComposerProps) {
  const taRef = useRef<HTMLTextAreaElement>(null)
  const [text, setText] = useState('')
  const [focused, setFocused] = useState(false)
  // History cursor: -1 = editing a fresh line; otherwise index into
  // `history` counted from the end. The fresh line is stashed so
  // ↓ past the newest entry restores it.
  const histRef = useRef<{ idx: number; stash: string }>({ idx: -1, stash: '' })

  useEffect(() => {
    taRef.current?.focus()
  }, [focusKey])

  // Grow with content up to MAX_ROWS, then scroll.
  useLayoutEffect(() => {
    const ta = taRef.current
    if (!ta) return
    ta.style.height = '0px'
    const line = 20
    const max = line * MAX_ROWS
    ta.style.height = `${Math.min(max, Math.max(line, ta.scrollHeight))}px`
    ta.style.overflowY = ta.scrollHeight > max ? 'auto' : 'hidden'
  }, [text])

  const send = () => {
    const t = text.replace(/\s+$/, '')
    if (t === '') {
      // Bare ⏎ at a shell prompt is a fresh prompt; to an agent it's a no-op.
      if (mode === 'terminal') onRaw('\r')
      return
    }
    onSend(t)
    setText('')
    histRef.current = { idx: -1, stash: '' }
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    const ta = e.currentTarget
    if (e.key === 'Enter' && !e.shiftKey && !e.altKey && !e.metaKey && !e.ctrlKey) {
      e.preventDefault()
      send()
      return
    }
    if (e.ctrlKey && !e.metaKey && !e.altKey && text === '') {
      const ctrl: Record<string, string> = { c: '\x03', d: '\x04', l: '\x0c', z: '\x1a' }
      const b = ctrl[e.key.toLowerCase()]
      if (b) {
        e.preventDefault()
        onRaw(b)
        return
      }
    }
    if (e.key === 'Escape' && mode === 'agent' && kind === 'agent') {
      e.preventDefault()
      onRaw('\x1b')
      return
    }
    if (mode !== 'terminal' || history.length === 0) return
    const caret = ta.selectionStart
    const onFirstLine = !text.slice(0, caret).includes('\n')
    const onLastLine = !text.slice(caret).includes('\n')
    const h = histRef.current
    if (e.key === 'ArrowUp' && onFirstLine && h.idx < history.length - 1) {
      e.preventDefault()
      if (h.idx === -1) h.stash = text
      h.idx += 1
      const v = history[history.length - 1 - h.idx]
      setText(v)
      requestAnimationFrame(() => ta.setSelectionRange(v.length, v.length))
    } else if (e.key === 'ArrowDown' && onLastLine && h.idx >= 0) {
      e.preventDefault()
      h.idx -= 1
      const v = h.idx === -1 ? h.stash : history[history.length - 1 - h.idx]
      setText(v)
      requestAnimationFrame(() => ta.setSelectionRange(v.length, v.length))
    }
  }

  const placeholder =
    mode === 'agent'
      ? kind === 'agent'
        ? `Message ${agentName}…`
        : `Start ${agentName} with a prompt…`
      : 'Type a command…'

  const hints =
    mode === 'agent' ? '⏎ send · ⇧⏎ newline' : '⏎ run · ⇧⏎ newline · ↑ history'

  return (
    <div
      className={`helm-composer ${focused ? 'helm-composer-focused' : ''}`}
      onMouseDown={(e) => {
        // Clicks on the chrome land in the editor.
        if (e.target === e.currentTarget) {
          e.preventDefault()
          taRef.current?.focus()
        }
      }}
    >
      {(cwd || branch) && (
        <div className="flex items-center gap-1.5 px-3 pt-2.5">
          {cwd && (
            <Chip title={cwd}>
              <FolderIcon size={12} />
              {homeRelative(cwd)}
            </Chip>
          )}
          {branch && (
            <Chip>
              <BranchIcon size={12} />
              {branch}
            </Chip>
          )}
        </div>
      )}
      <textarea
        ref={taRef}
        value={text}
        rows={1}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        placeholder={placeholder}
        onChange={(e) => {
          setText(e.target.value)
          histRef.current.idx = -1
        }}
        onKeyDown={onKeyDown}
        onFocus={() => setFocused(true)}
        onBlur={() => setFocused(false)}
        className="helm-composer-editor"
      />
      <div className="flex items-center gap-3 px-2 pb-2 pt-1">
        <Segmented value={mode} onChange={onModeChange} />
        <span className="flex-1" />
        <span className="select-none font-mono text-[11px] text-text-disabled">{hints}</span>
      </div>
    </div>
  )
}

function Chip({ title, children }: { title?: string; children: React.ReactNode }) {
  return (
    <span
      title={title}
      className="inline-flex h-[22px] select-none items-center gap-1 rounded border border-[var(--stroke-default)] bg-[var(--terminal-chip-bg)] px-1.5 font-mono text-[11px] font-semibold text-text-secondary"
    >
      {children}
    </span>
  )
}

function Segmented({
  value,
  onChange,
}: {
  value: ComposerMode
  onChange: (m: ComposerMode) => void
}) {
  const opts: Array<[ComposerMode, string]> = [
    ['terminal', 'Terminal'],
    ['agent', 'Agent'],
  ]
  return (
    <div
      role="radiogroup"
      className="flex h-[22px] select-none items-center rounded-md bg-[var(--stroke-subtle)] p-[2px]"
    >
      {opts.map(([m, label]) => (
        <button
          key={m}
          type="button"
          role="radio"
          aria-checked={value === m}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => onChange(m)}
          title={`${label} mode (⌘I toggles)`}
          className={`h-full rounded-[4px] px-2 text-[11px] font-medium leading-none ${
            value === m ? 'bg-accent text-white' : 'text-text-secondary hover:text-text-primary'
          }`}
        >
          {label}
        </button>
      ))}
    </div>
  )
}

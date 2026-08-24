/**
 * The composer — Helm's input. Warp's universal input, in our palette:
 * a bordered box above the bottom edge with one row of context — the
 * Terminal | Agent control that says where ⏎ sends the text, then the
 * cwd and branch chips — over a growing mono editor.
 *
 *   Terminal  → the shell (`cmd⏎`; multi-line as a bracketed paste)
 *   Agent     → the agent running in this pane, or — from a shell —
 *               launches one with the text as its prompt
 *
 * The editor is a textarea (native selection, paste, undo, IME). An
 * EMPTY editor is transparent to the agent: arrows, Tab, ⏎, Home/End,
 * PageUp/Down go to the TUI, so Claude's menus (the --resume picker,
 * option lists) work without leaving the composer; typing anything
 * makes the same keys edit the draft. ^C ^D ^L ^Z pass through on an
 * empty editor; Esc also passes through to an active agent.
 */

/** Keys an empty agent composer forwards, as the TUI expects them. */
const PASSTHROUGH: Record<string, string> = {
  ArrowUp: '\x1b[A',
  ArrowDown: '\x1b[B',
  ArrowRight: '\x1b[C',
  ArrowLeft: '\x1b[D',
  Home: '\x1b[H',
  End: '\x1b[F',
  PageUp: '\x1b[5~',
  PageDown: '\x1b[6~',
  Tab: '\t',
  Enter: '\r',
}

import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent } from 'react'
import { createPortal } from 'react-dom'
import type { PathCompletion, PathCompletionResult } from '@bindings'
import { homeRelative } from '@lib/path'
import type { ComposerMode } from '@lib/session/composer'
import type { PaneKind } from '@lib/session/paneState'
import { initialHistoryCursor, navigateHistory } from '@lib/session/historyNavigation'
import {
  applyPathCompletion,
  commonPathPrefix,
  pathCompletionLabel,
  pathCompletionContext,
  replacePathCompletion,
  type PathCompletionContext,
} from '@lib/session/pathCompletion'
import { BranchIcon, FolderIcon } from '@features/sessions/icons'
import { textareaCaretRect } from '@lib/textareaCaret'

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
  onPathComplete: (path: string, directoriesOnly: boolean) => Promise<PathCompletionResult>
  /** Bumps to pull focus (pane became visible, mode re-opened). */
  focusKey: number
}

const MAX_ROWS = 8

interface CompletionMenu {
  context: PathCompletionContext
  candidates: PathCompletion[]
  selected: number
  truncated: boolean
}

interface CompletionMenuPosition {
  left: number
  width: number
  maxHeight: number
  top?: number
  bottom?: number
}

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
  onPathComplete,
  focusKey,
}: ComposerProps) {
  const completionMenuId = useId()
  const taRef = useRef<HTMLTextAreaElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)
  const [text, setText] = useState('')
  const [focused, setFocused] = useState(false)
  const [completion, setCompletion] = useState<CompletionMenu | null>(null)
  const [completionPosition, setCompletionPosition] = useState<CompletionMenuPosition | null>(null)
  const completionRequestRef = useRef(0)
  const completionCaretRef = useRef<number | null>(null)
  // History cursor: -1 = editing a fresh line; otherwise index into
  // `history` counted from the end. The fresh line is stashed so
  // ↓ past the newest entry restores it.
  const histRef = useRef(initialHistoryCursor())

  useEffect(() => {
    taRef.current?.focus()
  }, [focusKey])

  const cancelCompletion = useCallback(() => {
    completionRequestRef.current += 1
    completionCaretRef.current = null
    setCompletion(null)
  }, [])

  useEffect(cancelCompletion, [cancelCompletion, mode])

  useEffect(() => {
    if (!completion || completion.selected < 0 || !menuRef.current) return
    menuRef.current
      .querySelector<HTMLElement>(`[data-completion-index="${completion.selected}"]`)
      ?.scrollIntoView({ block: 'nearest' })
  }, [completion])

  const completionContext = completion?.context
  const completionCandidates = completion?.candidates
  useLayoutEffect(() => {
    const textarea = taRef.current
    if (!completionContext || !completionCandidates || !textarea) {
      setCompletionPosition(null)
      return
    }
    const position = () => {
      const start = textareaCaretRect(textarea, completionContext.start)
      const end = textareaCaretRect(textarea, completionContext.end)
      const textareaRect = textarea.getBoundingClientRect()
      const anchor =
        Math.abs(start.top - end.top) < 1 && start.left >= textareaRect.left
          ? start
          : end
      const measuringContext = document.createElement('canvas').getContext('2d')
      if (measuringContext) {
        measuringContext.font = `12px ${window.getComputedStyle(textarea).fontFamily}`
      }
      const longest = completionCandidates.reduce((width, candidate) => {
        const label = pathCompletionLabel(candidate.value)
        return Math.max(width, measuringContext?.measureText(label).width ?? label.length * 8)
      }, 0)
      const width = Math.min(Math.max(240, Math.ceil(longest + 116)), 560, window.innerWidth - 16)
      const left = Math.min(Math.max(8, anchor.left), window.innerWidth - width - 8)
      const above = Math.max(0, anchor.top - 12)
      const below = Math.max(0, window.innerHeight - anchor.bottom - 12)
      const openAbove = above >= 120 || above >= below
      setCompletionPosition({
        left,
        width,
        maxHeight: Math.min(240, Math.max(72, openAbove ? above : below)),
        ...(openAbove
          ? { bottom: window.innerHeight - anchor.top + 6 }
          : { top: anchor.bottom + 6 }),
      })
    }
    position()
    window.addEventListener('resize', position)
    return () => window.removeEventListener('resize', position)
  }, [completionCandidates, completionContext, text])

  // Grow with content up to MAX_ROWS, then scroll. Measuring means
  // collapsing the textarea to read its scrollHeight; the box is held
  // at its current height meanwhile, or the pane above would grow for
  // a layout pass and WebKit would clamp a bottom-pinned scroll
  // position off the bottom.
  useLayoutEffect(() => {
    const ta = taRef.current
    const box = ta?.parentElement
    if (!ta || !box) return
    box.style.height = `${box.offsetHeight}px`
    ta.style.height = '0px'
    const line = 20
    const max = line * MAX_ROWS
    ta.style.height = `${Math.min(max, Math.max(line, ta.scrollHeight))}px`
    ta.style.overflowY = ta.scrollHeight > max ? 'auto' : 'hidden'
    box.style.height = ''
  }, [text])

  const send = () => {
    const t = text.replace(/\s+$/, '')
    if (t === '') {
      // Bare ⏎: a fresh prompt from a shell, "confirm" to an agent's menu.
      onRaw('\r')
      return
    }
    onSend(t)
    setText('')
    histRef.current = initialHistoryCursor()
    cancelCompletion()
  }

  const setDraft = (value: string, caret: number) => {
    setText(value)
    histRef.current = initialHistoryCursor()
    requestAnimationFrame(() => taRef.current?.setSelectionRange(caret, caret))
  }

  const acceptCompletion = (candidate: PathCompletion) => {
    if (!completion) return
    const applied = applyPathCompletion(text, completion.context, candidate)
    setDraft(applied.text, applied.caret)
    cancelCompletion()
  }

  const requestCompletion = (ta: HTMLTextAreaElement) => {
    const context = pathCompletionContext(text, ta.selectionStart)
    if (!context) return
    const request = ++completionRequestRef.current
    completionCaretRef.current = context.end
    const requestedText = text
    setCompletion(null)
    void onPathComplete(context.path, context.directoriesOnly)
      .then((result) => {
        if (completionRequestRef.current !== request) return
        if (result.candidates.length === 0) {
          completionCaretRef.current = null
          return
        }
        if (result.candidates.length === 1) {
          const applied = applyPathCompletion(requestedText, context, result.candidates[0])
          setDraft(applied.text, applied.caret)
          completionCaretRef.current = null
          return
        }
        const common = commonPathPrefix(
          result.candidates.map((candidate) => candidate.value),
          context.path,
        )
        const replaced = common !== context.path
          ? replacePathCompletion(requestedText, context, common)
          : { text: requestedText, caret: context.end, context }
        if (replaced.text !== requestedText) setDraft(replaced.text, replaced.caret)
        completionCaretRef.current = replaced.caret
        setCompletion({
          context: replaced.context,
          candidates: result.candidates,
          selected: -1,
          truncated: result.truncated,
        })
      })
      .catch(() => {
        if (completionRequestRef.current === request) completionCaretRef.current = null
      })
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    const ta = e.currentTarget
    if (!completion && completionCaretRef.current !== null && e.key === 'Escape') {
      e.preventDefault()
      cancelCompletion()
      return
    }
    if (completion) {
      if (e.key === 'Escape') {
        e.preventDefault()
        cancelCompletion()
        return
      }
      if (e.key === 'Enter') {
        e.preventDefault()
        if (completion.selected < 0) {
          send()
        } else {
          acceptCompletion(completion.candidates[completion.selected])
        }
        return
      }
      if (e.key === 'Tab' || e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault()
        const direction = e.key === 'ArrowUp' || (e.key === 'Tab' && e.shiftKey) ? -1 : 1
        setCompletion((current) => current && ({
          ...current,
          selected: current.selected < 0
            ? direction > 0 ? 0 : current.candidates.length - 1
            : (current.selected + direction + current.candidates.length) % current.candidates.length,
        }))
        return
      }
    }
    if (mode === 'terminal' && e.key === 'Tab' && !e.metaKey && !e.ctrlKey && !e.altKey) {
      e.preventDefault()
      if (completionCaretRef.current !== null) return
      requestCompletion(ta)
      return
    }
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
    if (mode === 'agent' && kind === 'agent' && text === '' && !e.metaKey && !e.ctrlKey && !e.altKey) {
      const seq = e.key === 'Tab' && e.shiftKey ? '\x1b[Z' : PASSTHROUGH[e.key]
      if (seq) {
        e.preventDefault()
        onRaw(seq)
        return
      }
    }
    if (mode !== 'terminal' || (e.key !== 'ArrowUp' && e.key !== 'ArrowDown')) return
    const direction = e.key === 'ArrowUp' ? 'older' : 'newer'
    const next = navigateHistory(history, histRef.current, direction, text)
    if (!next) return
    e.preventDefault()
    histRef.current = next.cursor
    setText(next.value)
    requestAnimationFrame(() => ta.setSelectionRange(next.value.length, next.value.length))
  }

  const placeholder =
    mode === 'agent'
      ? kind === 'agent'
        ? `Message ${agentName}…`
        : `Start ${agentName} with a prompt…`
      : 'Type a command…'

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
      {completion && completionPosition && createPortal(
        <div
          id={completionMenuId}
          ref={menuRef}
          className="helm-completion-menu"
          role="listbox"
          style={completionPosition}
        >
          {completion.candidates.map((candidate, index) => (
            <button
              key={`${candidate.kind}:${candidate.value}`}
              id={`${completionMenuId}-option-${index}`}
              type="button"
              role="option"
              aria-selected={index === completion.selected}
              data-completion-index={index}
              className={`helm-completion-option ${index === completion.selected ? 'helm-completion-option-selected' : ''}`}
              onMouseDown={(event) => {
                event.preventDefault()
                acceptCompletion(candidate)
              }}
            >
              <span className="truncate" title={candidate.value}>
                {pathCompletionLabel(candidate.value)}
              </span>
              <span className="helm-completion-kind">{candidate.kind}</span>
            </button>
          ))}
          {completion.truncated && (
            <div className="helm-completion-status">More matches not shown</div>
          )}
        </div>,
        document.body,
      )}
      <div className="flex items-center gap-1.5 px-2 pt-2">
        <Segmented value={mode} onChange={onModeChange} />
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
      <textarea
        ref={taRef}
        value={text}
        rows={1}
        spellCheck={mode === 'agent'}
        autoCapitalize={mode === 'agent' ? 'sentences' : 'off'}
        autoCorrect={mode === 'agent' ? 'on' : 'off'}
        autoComplete={mode === 'agent' ? 'on' : 'off'}
        aria-autocomplete={mode === 'terminal' ? 'list' : undefined}
        aria-controls={completion ? completionMenuId : undefined}
        aria-activedescendant={
          completion && completion.selected >= 0
            ? `${completionMenuId}-option-${completion.selected}`
            : undefined
        }
        aria-expanded={mode === 'terminal' ? completion !== null : undefined}
        placeholder={placeholder}
        onChange={(e) => {
          setText(e.target.value)
          histRef.current = initialHistoryCursor()
          cancelCompletion()
        }}
        onSelect={(event) => {
          const textarea = event.currentTarget
          const expected = completionCaretRef.current
          if (
            expected !== null &&
            (textarea.selectionStart !== expected || textarea.selectionEnd !== expected)
          ) {
            cancelCompletion()
          }
        }}
        onKeyDown={onKeyDown}
        onFocus={() => setFocused(true)}
        onBlur={() => {
          setFocused(false)
          cancelCompletion()
        }}
        className="helm-composer-editor"
      />
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

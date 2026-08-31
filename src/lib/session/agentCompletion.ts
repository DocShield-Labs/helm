/**
 * Agent-composer autocomplete triggers — Claude Code's `@file` and
 * `/command` affordances, recreated in the composer.
 *
 *   `/…` — only as the very first characters of the message, matching
 *          the CLI: it only interprets a leading slash (mid-sentence a
 *          slash is punctuation, and `/model` mid-text does nothing);
 *   `@…` — any whitespace-delimited token starting with `@`, completed
 *          against the session's cwd on the daemon.
 *
 * Pure text math; fetching and menus live in the composer.
 */

import { fuzzyMatch } from '@lib/fuzzy'

export interface AgentTrigger {
  kind: 'file' | 'command'
  /** Token span `[start, end)` in the text, including the trigger char. */
  start: number
  end: number
  /** What the user typed after the trigger char. */
  query: string
}

/** The active trigger at `caret`, or null. */
export function agentTrigger(text: string, caret: number): AgentTrigger | null {
  if (caret < 0 || caret > text.length) return null
  // A command: everything before the caret is exactly `/name-so-far`.
  const head = text.slice(0, caret)
  if (/^\/[A-Za-z0-9:_-]*$/.test(head)) {
    return { kind: 'command', start: 0, end: caret, query: head.slice(1) }
  }
  // A file token: scan back to whitespace; the token starts with `@`.
  let start = caret
  while (start > 0 && !/\s/.test(text[start - 1])) start--
  if (text[start] !== '@' || caret === start) return null
  return { kind: 'file', start, end: caret, query: text.slice(start + 1, caret) }
}

/** Replace the trigger's WHOLE token (caret may sit mid-token — the
 * untyped tail is stale, not content to preserve) with the chosen
 * completion. Commands and files get a trailing space so typing
 * continues naturally — unless one already follows, or the value is a
 * directory: a directory leaves the token open so the next keystroke
 * keeps completing inside it. */
export function applyAgentCompletion(
  text: string,
  trigger: AgentTrigger,
  value: string,
): { text: string; caret: number } {
  let tokenEnd = trigger.end
  while (tokenEnd < text.length && !/\s/.test(text[tokenEnd])) tokenEnd++
  const isDir = trigger.kind === 'file' && value.endsWith('/')
  const core = trigger.kind === 'command' ? `/${value}` : `@${value}`
  const spaceFollows = text[tokenEnd] === ' '
  const inserted = isDir || spaceFollows ? core : `${core} `
  const next = text.slice(0, trigger.start) + inserted + text.slice(tokenEnd)
  // Land after the space either way, so typing continues past it.
  const caret = trigger.start + inserted.length + (spaceFollows && !isDir ? 1 : 0)
  return { text: next, caret }
}

/** Rank the command list against what's typed, palette-style: the same
 * subsequence matcher, so `/cr` finds `code-review` here exactly like
 * it does in the palette. Pure — the composer just renders rows. */
export function filterAgentCommands<T extends { name: string }>(
  all: readonly T[],
  query: string,
  limit: number,
): T[] {
  const ranked: Array<{ cmd: T; score: number }> = []
  for (const cmd of all) {
    const match = fuzzyMatch(query, cmd.name)
    if (match) ranked.push({ cmd, score: match.score })
  }
  ranked.sort((a, b) => b.score - a.score || a.cmd.name.localeCompare(b.cmd.name))
  return ranked.slice(0, limit).map((r) => r.cmd)
}

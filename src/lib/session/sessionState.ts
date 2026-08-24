/**
 * What a session is doing right now, derived from its block table.
 *
 * The composer-as-input model hangs off this: the shell's own prompt
 * is never typed into — at a prompt the xterm is hidden and the
 * composer is the input; while a command runs the xterm shows its
 * output; TUIs take the grid. The markers come from the shell
 * integration (OSC 133 A = prompt, B = command accepted, D = done), so
 * a session without integration degrades to a plain terminal (`raw`).
 */

import type { BlockInfo } from '@bindings'
import type { SessionBlocks } from './blocks'

export type SessionPhase =
  /** Shell is at its prompt; the composer is the input. */
  | 'prompt'
  /** A command is running; the xterm shows it. */
  | 'running'
  /** Alternate screen (full-screen TUI). */
  | 'alt'
  /** No integration markers (yet) or the process exited: plain terminal. */
  | 'raw'

export type SessionKind = 'shell' | 'agent'

export interface SessionState {
  phase: SessionPhase
  kind: SessionKind
  /** The in-flight block while `running`. */
  current: BlockInfo | null
}

/** Coding agents whose TUI the agent composer talks to. The first is
 * what Agent mode launches from a shell. */
export const AGENT_COMMANDS = ['claude', 'codex', 'gemini', 'aider'] as const
export const AGENT_LAUNCH_COMMAND = AGENT_COMMANDS[0]

/** Program name of a command line: skips `FOO=bar` assignments and
 * `sudo`/`env`, strips any path. `null` when empty. */
export function commandName(cmdline: string | null | undefined): string | null {
  if (!cmdline) return null
  const words = cmdline.trim().split(/\s+/)
  for (const w of words) {
    if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(w)) continue
    if (w === 'sudo' || w === 'env' || w === 'exec' || w === 'nohup') continue
    if (w.startsWith('-')) continue
    const slash = w.lastIndexOf('/')
    return slash >= 0 ? w.slice(slash + 1) : w
  }
  return null
}

export function isAgentCommand(name: string | null): boolean {
  return name !== null && (AGENT_COMMANDS as readonly string[]).includes(name)
}

/** The prompt an agent was launched with (`claude "fix the tests"`),
 * for labelling the session; null when it was started bare. */
export function agentPromptOf(cmdline: string | null | undefined): string | null {
  if (!cmdline) return null
  const m = /^\s*(?:\S+=\S+\s+)*(?:claude|codex|gemini|aider)\b\s*(.*)$/.exec(cmdline)
  if (!m || !m[1]) return null
  const rest = m[1].trim()
  const q = /^(?:-\S+\s+)*(?:"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)'|(\S.*))$/.exec(rest)
  if (!q) return null
  const text = q[1] ?? q[2] ?? q[3] ?? ''
  return text.startsWith('-') ? null : text || null
}

export function deriveSessionState(pb: SessionBlocks, spawnedCommand: string | null): SessionState {
  const last = pb.blocks.length > 0 ? pb.blocks[pb.blocks.length - 1] : undefined
  const running = last && last.cmd_line !== null && last.end_line === null ? last : null
  const program = running ? commandName(running.cmdline) : spawnedCommand
  const kind: SessionKind = isAgentCommand(program) ? 'agent' : 'shell'
  if (pb.altScreen) return { phase: 'alt', kind, current: running }
  if (!pb.loaded || pb.exited || !last) return { phase: 'raw', kind, current: null }
  if (last.end_line === null && last.cmd_line === null) {
    return { phase: 'prompt', kind: 'shell', current: null }
  }
  return { phase: 'running', kind, current: running }
}

/** Shell-quote for a single-quoted argument. */
export function shellQuote(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`
}

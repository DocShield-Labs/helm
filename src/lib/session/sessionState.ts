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
import { commandName, isAgentCommand } from './agents'

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

export function deriveSessionState(
  pb: SessionBlocks,
  spawnedCommand: string | null,
  customAgentTemplate = '',
  runningAgentName?: string | null,
): SessionState {
  const last = pb.blocks.length > 0 ? pb.blocks[pb.blocks.length - 1] : undefined
  const running = last && last.cmd_line !== null && last.end_line === null ? last : null
  const program = running ? commandName(running.cmdline) : spawnedCommand
  const isAgent = running && runningAgentName !== undefined
    ? runningAgentName !== null
    : isAgentCommand(program, customAgentTemplate)
  const kind: SessionKind = isAgent ? 'agent' : 'shell'
  if (pb.altScreen) return { phase: 'alt', kind, current: running }
  if (!pb.loaded || pb.exited || !last) return { phase: 'raw', kind, current: null }
  if (last.end_line === null && last.cmd_line === null) {
    return { phase: 'prompt', kind: 'shell', current: null }
  }
  return { phase: 'running', kind, current: running }
}

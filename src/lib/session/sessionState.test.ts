import { describe, expect, test } from 'bun:test'
import type { BlockInfo } from '@bindings'
import type { SessionBlocks } from './blocks'
import { deriveSessionState } from './sessionState'

function runningBlocks(command: string): SessionBlocks {
  const block: BlockInfo = {
    id: 'block-1',
    start_line: 0,
    cmd_line: 1,
    output_line: 2,
    end_line: null,
    cmdline: command,
    cwd: null,
    branch: null,
    root: null,
    exit_code: null,
    started_at_ms: null,
    finished_at_ms: null,
  }
  return {
    blocks: [block],
    altScreen: false,
    exited: false,
    loaded: true,
    bells: 0,
    clearedBefore: 0,
  }
}

describe('session state', () => {
  test('captured identity survives a custom-agent preference change', () => {
    const blocks = runningBlocks("old-agent 'keep working'")

    expect(deriveSessionState(blocks, null, 'new-agent {prompt}', 'old-agent').kind).toBe('agent')
    expect(deriveSessionState(blocks, null, 'new-agent {prompt}', null).kind).toBe('shell')
  })
})

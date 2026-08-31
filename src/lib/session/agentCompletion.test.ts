import { describe, expect, test } from 'bun:test'
import { agentTrigger, applyAgentCompletion, filterAgentCommands } from './agentCompletion'

describe('agentTrigger', () => {
  test('slash at message start, and only there — matching the CLI', () => {
    expect(agentTrigger('/', 1)).toEqual({ kind: 'command', start: 0, end: 1, query: '' })
    expect(agentTrigger('/sim', 4)).toEqual({ kind: 'command', start: 0, end: 4, query: 'sim' })
    // Mid-sentence slashes are punctuation, not commands.
    expect(agentTrigger('fix a/b', 7)).toBeNull()
    expect(agentTrigger('hey /help', 9)).toBeNull()
    // Once an argument begins, the menu stands down.
    expect(agentTrigger('/help me', 8)).toBeNull()
  })

  test('at-token anywhere, caret past the @', () => {
    expect(agentTrigger('look at @src/ma', 15)).toEqual({
      kind: 'file',
      start: 8,
      end: 15,
      query: 'src/ma',
    })
    // A bare @ opens the menu on the cwd, like Claude Code.
    expect(agentTrigger('@', 1)).toEqual({ kind: 'file', start: 0, end: 1, query: '' })
    expect(agentTrigger('@x', 2)).toEqual({ kind: 'file', start: 0, end: 2, query: 'x' })
    // An email-ish token still triggers only when @ starts the token.
    expect(agentTrigger('user@host', 9)).toBeNull()
  })

  test('caret inside a token completes the typed half', () => {
    const t = agentTrigger('see @src/app now', 8)
    expect(t).toEqual({ kind: 'file', start: 4, end: 8, query: 'src' })
  })
})

describe('applyAgentCompletion', () => {
  test('command inserts with trailing space', () => {
    const r = applyAgentCompletion('/sim', { kind: 'command', start: 0, end: 4, query: 'sim' }, 'simplify')
    expect(r).toEqual({ text: '/simplify ', caret: 10 })
  })

  test('file replaces the token; a following space is reused, not doubled', () => {
    const t = { kind: 'file' as const, start: 4, end: 8, query: 'src' }
    expect(applyAgentCompletion('see @src tail', t, 'src/')).toEqual({
      text: 'see @src/ tail',
      caret: 9,
    })
    // The existing space pads the insertion; the caret hops past it.
    expect(applyAgentCompletion('see @src tail', t, 'src/main.ts')).toEqual({
      text: 'see @src/main.ts tail',
      caret: 17,
    })
    // At end of text there is no space to reuse — one is added.
    expect(applyAgentCompletion('see @src', t, 'src/main.ts')).toEqual({
      text: 'see @src/main.ts ',
      caret: 17,
    })
  })

  test('caret mid-token replaces the whole token, not just the typed half', () => {
    const t = agentTrigger('see @src/app now', 8)!
    expect(applyAgentCompletion('see @src/app now', t, 'src/main.ts')).toEqual({
      text: 'see @src/main.ts now',
      caret: 17,
    })
  })
})

describe('filterAgentCommands', () => {
  const all = [{ name: 'code-review' }, { name: 'clear' }, { name: 'compact' }]

  test('subsequence matching, palette-style', () => {
    expect(filterAgentCommands(all, 'cr', 10).map((c) => c.name)).toContain('code-review')
  })

  test('empty query returns everything up to the limit', () => {
    expect(filterAgentCommands(all, '', 2)).toHaveLength(2)
  })
})

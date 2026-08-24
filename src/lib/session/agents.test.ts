import { describe, expect, test } from 'bun:test'
import {
  agentNameForCommand,
  agentPromptOf,
  buildAgentCommand,
  commandName,
  isAgentCommand,
} from './agents'

describe('agent commands', () => {
  test('builds each preset with a safely quoted prompt', () => {
    expect(buildAgentCommand('claude', '', "fix azhar's test")).toBe("claude 'fix azhar'\\''s test'")
    expect(buildAgentCommand('codex', '', 'review this')).toBe("codex 'review this'")
    expect(buildAgentCommand('opencode', '', 'review this')).toBe("opencode --prompt 'review this'")
  })

  test('supports custom placeholders and append-by-default templates', () => {
    expect(buildAgentCommand('custom', 'agent --prompt {prompt} --mode work', 'ship it')).toBe(
      "agent --prompt 'ship it' --mode work",
    )
    expect(buildAgentCommand('custom', 'agent --mode work', 'ship it')).toBe(
      "agent --mode work 'ship it'",
    )
    expect(agentPromptOf("agent --profile work 'ship it'", 'agent --profile work {prompt}')).toBe('ship it')
  })

  test('recognises built-in, legacy, and configured custom processes', () => {
    expect(isAgentCommand('claude')).toBe(true)
    expect(isAgentCommand('claude-code')).toBe(true)
    expect(isAgentCommand('codex')).toBe(true)
    expect(isAgentCommand('opencode')).toBe(true)
    expect(isAgentCommand('gemini')).toBe(true)
    expect(isAgentCommand('my-agent', 'env my-agent --profile work {prompt}')).toBe(true)
    expect(isAgentCommand('cargo', 'my-agent {prompt}')).toBe(false)
  })

  test('derives provider labels from the running command', () => {
    expect(agentNameForCommand('codex --model gpt-5')).toBe('Codex')
    expect(agentNameForCommand('claude-code --resume session-id')).toBe('Claude Code')
    expect(agentNameForCommand('opencode --prompt hello')).toBe('OpenCode')
    expect(agentNameForCommand('my-agent hello', 'my-agent {prompt}')).toBe('my-agent')
    expect(agentNameForCommand('cargo test')).toBeNull()
  })

  test('extracts executable names through common wrappers', () => {
    expect(commandName('FOO=1 sudo env /opt/bin/codex hello')).toBe('codex')
    expect(commandName('sudo -u root env FOO=1 codex hello')).toBe('codex')
  })
})

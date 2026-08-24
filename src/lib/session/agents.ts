export type AgentId = 'claude' | 'codex' | 'opencode' | 'custom'

export interface AgentPreset {
  id: Exclude<AgentId, 'custom'>
  name: string
  commandTemplate: string
  processNames: readonly string[]
}

export const AGENT_PRESETS: readonly AgentPreset[] = [
  {
    id: 'claude',
    name: 'Claude Code',
    commandTemplate: 'claude {prompt}',
    processNames: ['claude', 'claude-code'],
  },
  {
    id: 'codex',
    name: 'Codex',
    commandTemplate: 'codex {prompt}',
    processNames: ['codex'],
  },
  {
    id: 'opencode',
    name: 'OpenCode',
    commandTemplate: 'opencode --prompt {prompt}',
    processNames: ['opencode', 'opencode2'],
  },
]

export const DEFAULT_AGENT_ID: AgentId = 'claude'
export const DEFAULT_CUSTOM_AGENT_TEMPLATE = ''

const LEGACY_AGENT_PROCESSES = ['gemini', 'aider'] as const
const WRAPPERS = new Set(['sudo', 'env', 'exec', 'nohup'])
const WRAPPER_OPTIONS_WITH_VALUES = new Set([
  '-u', '--user', '-g', '--group', '-h', '--host', '-p', '--prompt',
  '-C', '--chdir', '-S', '--close-from', '--unset',
])

export function isAgentId(value: string): value is AgentId {
  return value === 'claude' || value === 'codex' || value === 'opencode' || value === 'custom'
}

export function commandName(commandLine: string | null | undefined): string | null {
  if (!commandLine) return null
  const words = commandLine.trim().split(/\s+/)
  let index = 0
  while (index < words.length) {
    const word = words[index]
    if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(word)) {
      index++
      continue
    }
    const slash = word.lastIndexOf('/')
    const name = slash >= 0 ? word.slice(slash + 1) : word
    if (WRAPPERS.has(name)) {
      index++
      while (index < words.length && words[index].startsWith('-')) {
        index += WRAPPER_OPTIONS_WITH_VALUES.has(words[index]) ? 2 : 1
      }
      continue
    }
    if (word.startsWith('-')) {
      index++
      continue
    }
    return name
  }
  return null
}

export function shellQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`
}

export function agentName(id: AgentId, customTemplate: string): string {
  if (id === 'custom') return commandName(customTemplate) || 'Custom'
  return AGENT_PRESETS.find((preset) => preset.id === id)?.name ?? 'Agent'
}

export function agentTemplate(id: AgentId, customTemplate: string): string {
  if (id === 'custom') return customTemplate.trim()
  return AGENT_PRESETS.find((preset) => preset.id === id)?.commandTemplate ?? 'claude {prompt}'
}

export function buildAgentCommand(id: AgentId, customTemplate: string, prompt: string): string {
  const template = agentTemplate(id, customTemplate)
  const quotedPrompt = shellQuote(prompt)
  if (template.includes('{prompt}')) return template.replaceAll('{prompt}', quotedPrompt)
  return `${template} ${quotedPrompt}`.trim()
}

export function isAgentCommand(name: string | null, customTemplate = ''): boolean {
  if (!name) return false
  if (AGENT_PRESETS.some((preset) => preset.processNames.includes(name))) return true
  if ((LEGACY_AGENT_PROCESSES as readonly string[]).includes(name)) return true
  return commandName(customTemplate) === name
}

export function agentNameForCommand(commandLine: string | null | undefined, customTemplate = ''): string | null {
  const program = commandName(commandLine)
  if (!program) return null
  const preset = AGENT_PRESETS.find((candidate) => candidate.processNames.includes(program))
  if (preset) return preset.name
  if ((LEGACY_AGENT_PROCESSES as readonly string[]).includes(program)) {
    return program[0].toUpperCase() + program.slice(1)
  }
  return commandName(customTemplate) === program ? agentName('custom', customTemplate) : null
}

/** Prompt used to launch an agent, for session labels. Custom templates
 * are matched before the general fallback so valued flags remain part
 * of the command rather than being mistaken for prompt text. */
export function agentPromptOf(commandLine: string | null | undefined, customTemplate = ''): string | null {
  if (!commandLine || !isAgentCommand(commandName(commandLine), customTemplate)) return null
  const customPrompt = promptFromTemplate(commandLine.trim(), customTemplate.trim())
  if (customPrompt !== null) return customPrompt
  const match = /^\s*(?:\S+=\S+\s+)*(?:sudo\s+|env\s+|exec\s+|nohup\s+)*(?:\S+)\s*(.*)$/.exec(commandLine)
  if (!match?.[1]) return null
  const rest = match[1].trim()
  const quoted = /^(?:-\S+\s+)*(?:"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)'|(\S.*))$/.exec(rest)
  if (!quoted) return null
  const text = quoted[1] ?? quoted[2] ?? quoted[3] ?? ''
  return text.startsWith('-') ? null : text || null
}

function promptFromTemplate(commandLine: string, template: string): string | null {
  if (!template || commandName(commandLine) !== commandName(template)) return null
  const marker = '{prompt}'
  const markerIndex = template.indexOf(marker)
  let raw: string
  if (markerIndex >= 0) {
    const before = template.slice(0, markerIndex)
    const after = template.slice(markerIndex + marker.length)
    if (!commandLine.startsWith(before) || !commandLine.endsWith(after)) return null
    raw = commandLine.slice(before.length, commandLine.length - after.length)
  } else {
    if (!commandLine.startsWith(`${template} `)) return null
    raw = commandLine.slice(template.length + 1)
  }
  return unquoteShellArgument(raw.trim())
}

function unquoteShellArgument(value: string): string | null {
  if (value.length < 2) return value || null
  if (value.startsWith("'") && value.endsWith("'")) {
    return value.slice(1, -1).replace(/'\\''/g, "'")
  }
  if (value.startsWith('"') && value.endsWith('"')) return value.slice(1, -1)
  return value
}

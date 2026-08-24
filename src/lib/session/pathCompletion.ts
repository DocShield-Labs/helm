export type CompletionQuote = 'unquoted' | 'single' | 'double'

export interface PathCompletionContext {
  start: number
  end: number
  path: string
  quote: CompletionQuote
  directoriesOnly: boolean
}

export interface PathCompletionCandidate {
  value: string
  kind: 'file' | 'directory'
}

const SHELL_BOUNDARY = /[\s;&|()<>]/

export function pathCompletionContext(
  text: string,
  caret: number,
): PathCompletionContext | null {
  if (caret < 0 || caret > text.length) return null
  if (caret < text.length && !SHELL_BOUNDARY.test(text[caret])) return null

  let start = 0
  let quote: "'" | '"' | null = null
  for (let index = 0; index < caret; index += 1) {
    const char = text[index]
    if (quote === "'") {
      if (char === "'") quote = null
      continue
    }
    if (quote === '"') {
      if (char === '\\') index += 1
      else if (char === '"') quote = null
      continue
    }
    if (char === '\\') {
      index += 1
    } else if (char === "'" || char === '"') {
      quote = char
    } else if (SHELL_BOUNDARY.test(char)) {
      start = index + 1
    }
  }

  const raw = text.slice(start, caret)
  if (/[$`*?\[]/.test(raw)) return null
  const path = decodeShellWord(raw)
  const before = text.slice(0, start)
  const segment = before.split(/[;\n|&]+/).at(-1) ?? ''
  const command = firstShellWord(segment)
  const isCommandWord = command === null
  const pathLikeCommand = /^(?:\.{0,2}\/|~\/|\/)/.test(path) || path.includes('/')
  if (isCommandWord && !pathLikeCommand) return null

  const style: CompletionQuote = raw.startsWith("'")
    ? 'single'
    : raw.startsWith('"')
      ? 'double'
      : 'unquoted'
  const commandName = command?.split('/').at(-1)
  return { start, end: caret, path, quote: style, directoriesOnly: commandName === 'cd' }
}

export function replacePathCompletion(
  text: string,
  context: PathCompletionContext,
  value: string,
): { text: string; caret: number; context: PathCompletionContext } {
  const inserted = quotePath(value, context.quote)
  const next = `${text.slice(0, context.start)}${inserted}${text.slice(context.end)}`
  const end = context.start + inserted.length
  return {
    text: next,
    caret: end,
    context: { ...context, end, path: value },
  }
}

export function applyPathCompletion(
  text: string,
  context: PathCompletionContext,
  candidate: PathCompletionCandidate,
): { text: string; caret: number } {
  const replaced = replacePathCompletion(text, context, candidate.value)
  const suffix = candidate.kind === 'file' && !/^\s/.test(replaced.text.slice(replaced.caret))
    ? ' '
    : ''
  return {
    text: `${replaced.text.slice(0, replaced.caret)}${suffix}${replaced.text.slice(replaced.caret)}`,
    caret: replaced.caret + suffix.length,
  }
}

export function commonPathPrefix(values: readonly string[], existing = ''): string {
  if (values.length === 0) return ''
  let prefix = values[0]
  for (const value of values.slice(1)) {
    let length = 0
    while (length < prefix.length && length < value.length && prefix[length] === value[length]) {
      length += 1
    }
    prefix = prefix.slice(0, length)
    if (prefix === '') break
  }
  return prefix.length >= existing.length ? prefix : existing
}

/** The menu is already anchored to the token being completed, so the
 * shared parent path is redundant. Keep the full value for insertion. */
export function pathCompletionLabel(value: string): string {
  const directory = value.endsWith('/')
  const withoutSlash = directory ? value.slice(0, -1) : value
  const name = withoutSlash.slice(withoutSlash.lastIndexOf('/') + 1)
  return `${name}${directory ? '/' : ''}`
}

function firstShellWord(segment: string): string | null {
  const match = /\S+/.exec(segment)
  return match ? decodeShellWord(match[0]) : null
}

function decodeShellWord(raw: string): string {
  let result = ''
  let quote: "'" | '"' | null = null
  for (let index = 0; index < raw.length; index += 1) {
    const char = raw[index]
    if (quote === "'") {
      if (char === "'") quote = null
      else result += char
    } else if (quote === '"') {
      if (char === '"') quote = null
      else if (char === '\\' && index + 1 < raw.length) result += raw[++index]
      else result += char
    } else if (char === "'" || char === '"') {
      quote = char
    } else if (char === '\\' && index + 1 < raw.length) {
      result += raw[++index]
    } else {
      result += char
    }
  }
  return result
}

function quotePath(value: string, quote: CompletionQuote): string {
  if (quote === 'single') return `'${value.replaceAll("'", "'\\''")}'`
  if (quote === 'double') return `"${value.replace(/["\\$`!]/g, '\\$&')}"`
  return value.replace(/[\\\s'"`$&;|<>()\[\]{}*?!#]/g, '\\$&')
}

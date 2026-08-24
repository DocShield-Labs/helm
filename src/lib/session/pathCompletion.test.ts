import { describe, expect, test } from 'bun:test'
import {
  applyPathCompletion,
  commonPathPrefix,
  pathCompletionLabel,
  pathCompletionContext,
  replacePathCompletion,
} from './pathCompletion'

describe('path completion context', () => {
  test('recognises cd as directory-only and decodes escaped spaces', () => {
    expect(pathCompletionContext('cd My\\ Fo', 9)).toEqual({
      start: 3,
      end: 9,
      path: 'My Fo',
      quote: 'unquoted',
      directoriesOnly: true,
    })
    expect(pathCompletionContext('cd ', 3)?.directoriesOnly).toBe(true)
  })

  test('allows quoted arguments and path-like command words', () => {
    expect(pathCompletionContext('cat "notes f', 12)?.path).toBe('notes f')
    expect(pathCompletionContext('./scr', 5)?.path).toBe('./scr')
    expect(pathCompletionContext('car', 3)).toBeNull()
  })

  test('does not complete in the middle of a token or shell expansions', () => {
    expect(pathCompletionContext('cat file', 5)).toBeNull()
    expect(pathCompletionContext('cat $HOME', 9)).toBeNull()
  })
})

describe('path insertion', () => {
  test('escapes an unquoted candidate', () => {
    const context = pathCompletionContext('cat notes', 9)!
    expect(replacePathCompletion('cat notes', context, 'notes file.txt').text).toBe(
      'cat notes\\ file.txt',
    )
  })

  test('applies file spacing once without doubling existing whitespace', () => {
    const context = pathCompletionContext('cat rea', 7)!
    expect(applyPathCompletion('cat rea', context, { value: 'read me.txt', kind: 'file' })).toEqual({
      text: 'cat read\\ me.txt ',
      caret: 17,
    })
    const beforeSpace = pathCompletionContext('cat rea next', 7)!
    expect(applyPathCompletion('cat rea next', beforeSpace, { value: 'readme', kind: 'file' }).text)
      .toBe('cat readme next')
  })

  test('preserves the selected quote style', () => {
    const context = pathCompletionContext("cd 'My Fo", 9)!
    expect(replacePathCompletion("cd 'My Fo", context, 'My Folder/').text).toBe(
      "cd 'My Folder/'",
    )
  })

  test('finds the shared canonical prefix', () => {
    expect(commonPathPrefix(['src/components/', 'src/commands/'])).toBe('src/com')
    expect(commonPathPrefix(['config.toml', 'Code/'], 'co')).toBe('co')
    expect(commonPathPrefix(['Codebase/', 'Codex/'], 'co')).toBe('Code')
    expect(commonPathPrefix(['Code/', 'code/'], 'co')).toBe('co')
  })

  test('shows only the segment being completed', () => {
    expect(pathCompletionLabel('Code/docshield-workspaces/')).toBe('docshield-workspaces/')
    expect(pathCompletionLabel('../notes file.txt')).toBe('notes file.txt')
  })
})

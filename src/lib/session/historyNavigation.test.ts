import { describe, expect, test } from 'bun:test'
import { initialHistoryCursor, navigateHistory } from './historyNavigation'

describe('history navigation', () => {
  test('walks backward and restores the fresh draft', () => {
    const history = ['one', 'two']
    const first = navigateHistory(history, initialHistoryCursor(), 'older', '')!
    expect(first.value).toBe('two')
    const second = navigateHistory(history, first.cursor, 'older', first.value)!
    expect(second.value).toBe('one')
    const newer = navigateHistory(history, second.cursor, 'newer', second.value)!
    const fresh = navigateHistory(history, newer.cursor, 'newer', newer.value)!
    expect(fresh.value).toBe('')
  })

  test('leaves non-empty fresh drafts alone', () => {
    expect(navigateHistory(['one'], initialHistoryCursor(), 'older', 'draft')).toBeNull()
  })
})

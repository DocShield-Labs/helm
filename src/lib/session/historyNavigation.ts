export interface HistoryCursor {
  index: number
}

export function initialHistoryCursor(): HistoryCursor {
  return { index: -1 }
}

export function navigateHistory(
  history: readonly string[],
  cursor: HistoryCursor,
  direction: 'older' | 'newer',
  current: string,
): { value: string; cursor: HistoryCursor } | null {
  if (history.length === 0 || (cursor.index === -1 && current !== '')) return null
  if (direction === 'older') {
    if (cursor.index >= history.length - 1) return null
    const index = cursor.index + 1
    return {
      value: history[history.length - 1 - index],
      cursor: { index },
    }
  }
  if (cursor.index < 0) return null
  const index = cursor.index - 1
  return {
    value: index === -1 ? '' : history[history.length - 1 - index],
    cursor: { index },
  }
}

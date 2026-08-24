/**
 * Find-in-blocks: text search over the rendered DOM blocks of a session,
 * highlighted with the CSS Custom Highlight API (no DOM mutation, so
 * selections and React reconciliation are untouched).
 */

export interface DomFindOptions {
  caseSensitive: boolean
  regex: boolean
}

const HIGHLIGHT_ALL = 'helm-find'
const HIGHLIGHT_ACTIVE = 'helm-find-active'

export function highlightsSupported(): boolean {
  return typeof Highlight === 'function' && typeof CSS !== 'undefined' && !!CSS.highlights
}

/** Build the matcher once per query; null when the regex is invalid. */
function compile(query: string, opts: DomFindOptions): RegExp | null {
  if (query === '') return null
  const flags = opts.caseSensitive ? 'g' : 'gi'
  try {
    return new RegExp(opts.regex ? query : query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), flags)
  } catch {
    return null
  }
}

/** All matches of `query` inside `root`'s block output, document order. */
export function findInBlocks(root: HTMLElement, query: string, opts: DomFindOptions): Range[] {
  const re = compile(query, opts)
  if (!re) return []
  const out: Range[] = []
  // Walk only block bodies and headers — everything under them is a
  // candidate, so no per-node ancestor checks.
  for (const body of root.querySelectorAll<HTMLElement>('.helm-block-output, .helm-block-header')) {
    const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT)
    let node: Node | null
    while ((node = walker.nextNode())) {
      const text = node.textContent ?? ''
      re.lastIndex = 0
      let m: RegExpExecArray | null
      while ((m = re.exec(text))) {
        if (m[0].length === 0) {
          re.lastIndex++
          continue
        }
        const r = document.createRange()
        r.setStart(node, m.index)
        r.setEnd(node, m.index + m[0].length)
        out.push(r)
        if (out.length >= MAX_MATCHES) return out
      }
    }
  }
  return out
}

/** Cap so a pathological query can't allocate unbounded Ranges. */
export const MAX_MATCHES = 2000

export function applyHighlights(ranges: Range[], activeIndex: number): void {
  if (!highlightsSupported()) return
  CSS.highlights.set(HIGHLIGHT_ALL, new Highlight(...ranges))
  const active = activeIndex >= 0 && activeIndex < ranges.length ? [ranges[activeIndex]] : []
  CSS.highlights.set(HIGHLIGHT_ACTIVE, new Highlight(...active))
}

export function clearHighlights(): void {
  CSS.highlights?.delete(HIGHLIGHT_ALL)
  CSS.highlights?.delete(HIGHLIGHT_ACTIVE)
}

export function scrollRangeIntoView(r: Range): void {
  const el = r.startContainer.parentElement
  el?.scrollIntoView({ block: 'center' })
}

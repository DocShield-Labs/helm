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

/** Matches of `query` inside `root`'s block output, document order.
 *
 * Collected NEWEST-first and capped: in a session that has scrolled a
 * lot, a short query matches everywhere, and painting thousands of
 * highlight ranges across a `content-visibility` document stalls the
 * main thread for seconds. The newest `MAX_MATCHES` are the ones a
 * terminal search wants anyway (stepping starts from the bottom).
 * Sub-`MIN_QUERY` queries skip the DOM entirely — every single letter
 * would hit the cap and the highlights would be pure noise. */
export function findInBlocks(root: HTMLElement, query: string, opts: DomFindOptions): Range[] {
  if (query.length < MIN_QUERY) return []
  const re = compile(query, opts)
  if (!re) return []
  // Bodies newest-first so the cap keeps the matches a terminal search
  // wants (stepping starts from the bottom); each body walks forward —
  // no node buffering — and its matches are appended newest-first, then
  // the whole list flips back to document order.
  const bodies = root.querySelectorAll<HTMLElement>('.helm-block-output, .helm-block-header')
  const out: Range[] = []
  const inBody: Range[] = []
  for (let i = bodies.length - 1; i >= 0 && out.length < MAX_MATCHES; i--) {
    inBody.length = 0
    const walker = document.createTreeWalker(bodies[i], NodeFilter.SHOW_TEXT)
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
        inBody.push(r)
      }
    }
    for (let j = inBody.length - 1; j >= 0 && out.length < MAX_MATCHES; j--) {
      out.push(inBody[j])
    }
  }
  return out.reverse()
}

/** Highlight-range cap — newest matches win; see `findInBlocks`. */
export const MAX_MATCHES = 500

/** Queries shorter than this skip the DOM search. */
export const MIN_QUERY = 2

// One persistent Highlight per name, registered once and MUTATED —
// never replaced or deleted. WebKit's repaint invalidation tracks
// mutations of a registered Highlight; wholesale `set`/`delete` of the
// registry entry can leave the old ranges' pixels painted (the stale
// yellow active-match that survived closing the search bar).
let allMatches: Highlight | null = null
let activeMatch: Highlight | null = null

function ensureRegistered(): boolean {
  if (!highlightsSupported()) return false
  if (!allMatches) {
    allMatches = new Highlight()
    activeMatch = new Highlight()
    CSS.highlights.set(HIGHLIGHT_ALL, allMatches)
    CSS.highlights.set(HIGHLIGHT_ACTIVE, activeMatch)
  }
  return true
}

export function applyHighlights(ranges: Range[], activeIndex: number): void {
  if (!ensureRegistered()) return
  allMatches!.clear()
  for (const r of ranges) allMatches!.add(r)
  activeMatch!.clear()
  if (activeIndex >= 0 && activeIndex < ranges.length) activeMatch!.add(ranges[activeIndex])
}

export function clearHighlights(): void {
  allMatches?.clear()
  activeMatch?.clear()
}

export function scrollRangeIntoView(r: Range): void {
  const el = r.startContainer.parentElement
  el?.scrollIntoView({ block: 'center' })
}

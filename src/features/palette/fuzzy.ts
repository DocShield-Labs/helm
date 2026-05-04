/**
 * Tiny fuzzy-subsequence scorer.
 *
 * Returns null when the query characters can't be threaded through the
 * candidate text in order; otherwise returns a score and the matched
 * indices (so the row renderer can later highlight them).
 *
 * Scoring rules — tuned by feel, easy to tweak:
 *   +1 per matched char (the baseline)
 *   +3 when the match continues a run (consecutive char in the candidate)
 *   +4 when the match lands at a word boundary (start of string, or
 *      preceded by space / `-` / `_` / `.` / `/` / `·`)
 *   −0.01 × candidate.length so shorter targets break ties
 *
 * The empty query returns score 0 with no indices — callers use this as
 * the "show everything; sort by weight/recency" signal.
 *
 * Hand-rolled rather than depending on `fzf-for-js`: the action set is
 * < 200 items, the scorer fits in 30 lines, and we avoid pulling 30 KB
 * of bundle for one screen.
 */

export interface FuzzyMatch {
  score: number
  /** Indices into the original candidate string where each query char
   * matched. Same length as the query. */
  indices: number[]
}

export function fuzzyMatch(query: string, text: string): FuzzyMatch | null {
  if (!query) return { score: 0, indices: [] }
  const q = query.toLowerCase()
  const t = text.toLowerCase()
  const indices: number[] = []
  let score = 0
  let lastMatch = -2
  let qi = 0
  for (let ti = 0; ti < t.length && qi < q.length; ti++) {
    if (t.charCodeAt(ti) !== q.charCodeAt(qi)) continue
    indices.push(ti)
    score += 1
    if (ti === 0 || /[\s\-_./·:]/.test(text[ti - 1])) score += 4
    if (lastMatch + 1 === ti) score += 3
    lastMatch = ti
    qi++
  }
  if (qi < q.length) return null
  score -= text.length * 0.01
  return { score, indices }
}

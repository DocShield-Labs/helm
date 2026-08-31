/**
 * Plain-text URL detection for the DOM renderer — the WebLinksAddon
 * we left behind with xterm. Runs over a *logical line's* spans (after
 * soft-wrap joining), so a URL that wraps across physical rows is one
 * link. Detected segments get their `link` field set and render
 * through the same anchor path OSC 8 hyperlinks already use; explicit
 * OSC 8 spans are never overridden.
 */

import type { SpanInfo } from '@bindings'

const URL_RE = /https?:\/\/[^\s<>"'`]+/g

/** Split spans so every detected URL becomes its own span(s) with
 * `link` set. Returns the input array untouched (same identity) when
 * the line holds no URL. */
export function linkifySpans(spans: SpanInfo[]): SpanInfo[] {
  let text = ''
  for (const s of spans) text += s.text
  if (!text.includes('http')) return spans
  const ranges = findUrls(text)
  if (ranges.length === 0) return spans

  const out: SpanInfo[] = []
  let ri = 0
  let pos = 0
  for (const s of spans) {
    const start = pos
    const end = pos + s.text.length
    pos = end
    if (s.link !== null || s.text.length === 0) {
      out.push(s)
      continue
    }
    let cursor = start
    while (cursor < end) {
      while (ri < ranges.length && ranges[ri].end <= cursor) ri++
      const r = ri < ranges.length ? ranges[ri] : null
      if (!r || r.start >= end) {
        out.push(cursor === start ? s : { ...s, text: s.text.slice(cursor - start) })
        break
      }
      if (r.start > cursor) {
        out.push({ ...s, text: s.text.slice(cursor - start, r.start - start) })
        cursor = r.start
      }
      const stop = Math.min(r.end, end)
      out.push({ ...s, text: s.text.slice(cursor - start, stop - start), link: r.url })
      cursor = stop
    }
  }
  return out
}

interface UrlRange {
  start: number
  end: number
  url: string
}

function findUrls(text: string): UrlRange[] {
  const out: UrlRange[] = []
  for (const m of text.matchAll(URL_RE)) {
    const url = trimTrailing(m[0])
    // A scheme alone (or scheme + separator noise) is not a link.
    if (!/^https?:\/\/[^/]/.test(url)) continue
    out.push({ start: m.index, end: m.index + url.length, url })
  }
  return out
}

/** Strip punctuation that belongs to the sentence, not the URL: a
 * trailing `.` or `),` etc. A closing bracket only comes off when the
 * URL doesn't contain its opener (so wiki-style `…/Foo_(bar)` stays
 * whole). */
function trimTrailing(raw: string): string {
  let url = raw
  for (;;) {
    const last = url[url.length - 1]
    if (last === undefined) break
    if ('.,;:!?'.includes(last)) {
      url = url.slice(0, -1)
      continue
    }
    const open = last === ')' ? '(' : last === ']' ? '[' : last === '}' ? '{' : null
    if (open && count(url, open) < count(url, last)) {
      url = url.slice(0, -1)
      continue
    }
    break
  }
  return url
}

function count(s: string, ch: string): number {
  let n = 0
  for (const c of s) if (c === ch) n++
  return n
}

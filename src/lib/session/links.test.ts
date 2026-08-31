import { describe, expect, test } from 'bun:test'
import type { SpanInfo } from '@bindings'
import { linkifySpans } from './links'

function span(text: string, link: string | null = null): SpanInfo {
  return { text, fg: -1, bg: -1, attrs: 0, link }
}

function shape(spans: SpanInfo[]): Array<[string, string | null]> {
  return spans.map((s) => [s.text, s.link])
}

describe('linkifySpans', () => {
  test('a line without URLs comes back untouched, same identity', () => {
    const spans = [span('nothing to see here')]
    expect(linkifySpans(spans)).toBe(spans)
    const httpButNoUrl = [span('the http protocol, in prose')]
    expect(linkifySpans(httpButNoUrl)).toBe(httpButNoUrl)
  })

  test('a URL mid-span splits into pre, link, post', () => {
    const out = linkifySpans([span('see https://github.com/DocShield-Labs/helm/pull/5 merged')])
    expect(shape(out)).toEqual([
      ['see ', null],
      ['https://github.com/DocShield-Labs/helm/pull/5', 'https://github.com/DocShield-Labs/helm/pull/5'],
      [' merged', null],
    ])
  })

  test('sentence punctuation is not part of the URL; wiki parens are', () => {
    const out = linkifySpans([span('read (https://example.com/a).')])
    expect(shape(out)).toEqual([
      ['read (', null],
      ['https://example.com/a', 'https://example.com/a'],
      [').', null],
    ])
    const wiki = linkifySpans([span('https://en.wikipedia.org/wiki/Foo_(bar) rocks')])
    expect(wiki[0].link).toBe('https://en.wikipedia.org/wiki/Foo_(bar)')
  })

  test('a URL spanning styled spans links every piece to the full URL', () => {
    // e.g. a color change mid-URL, or a soft-wrapped row boundary after
    // joinWrapped — the text is split, the target is whole.
    const out = linkifySpans([span('go https://exam'), span('ple.com/path now')])
    expect(shape(out)).toEqual([
      ['go ', null],
      ['https://exam', 'https://example.com/path'],
      ['ple.com/path', 'https://example.com/path'],
      [' now', null],
    ])
  })

  test('explicit OSC 8 spans are never overridden', () => {
    const out = linkifySpans([span('https://plain.example '), span('label', 'https://osc8.example')])
    expect(shape(out)).toEqual([
      ['https://plain.example', 'https://plain.example'],
      [' ', null],
      ['label', 'https://osc8.example'],
    ])
  })

  test('a bare scheme is not a link', () => {
    const spans = [span('https:// is how URLs start')]
    expect(linkifySpans(spans)).toBe(spans)
  })
})

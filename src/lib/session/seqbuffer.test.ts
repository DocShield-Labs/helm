import { describe, expect, test } from 'bun:test';
import { SeqBuffer } from './seqbuffer';

const b = (s: string) => new TextEncoder().encode(s);
const t = (u: Uint8Array | null) => (u ? new TextDecoder().decode(u) : null);

describe('SeqBuffer', () => {
  test('in-order frames extend the head', () => {
    const buf = new SeqBuffer();
    expect(buf.apply(0, b('hello ')).appended).toEqual(b('hello '));
    expect(buf.apply(6, b('world')).appended).toEqual(b('world'));
    expect(buf.head).toBe(11);
    expect(t(buf.slice(0, 11))).toBe('hello world');
    expect(t(buf.slice(6, 11))).toBe('world');
  });

  test('duplicate and overlapping frames are trimmed', () => {
    const buf = new SeqBuffer();
    buf.apply(0, b('abcdef'));
    expect(buf.apply(0, b('abc')).appended).toBeNull();
    const r = buf.apply(4, b('efGH'));
    expect(t(r.appended)).toBe('GH');
    expect(t(buf.slice(0, 8))).toBe('abcdefGH');
  });

  test('gap is reported and healed by replay', () => {
    const buf = new SeqBuffer();
    buf.apply(0, b('0123'));
    const r = buf.apply(8, b('89'));
    expect(r.gapFrom).toBe(4);
    expect(r.appended).toBeNull();
    expect(buf.contiguous).toBe(false);
    expect(buf.slice(0, 10)).toBeNull();
    // Replay fills the hole (and overlaps a little on both ends). The
    // bytes handed to the live renderer must include the bridged
    // detached chunk, not just the replay frame.
    const healed = buf.apply(2, b('234567'));
    expect(t(healed.appended)).toBe('456789');
    expect(healed.gapFrom).toBeNull();
    expect(buf.contiguous).toBe(true);
    expect(buf.head).toBe(10);
    expect(t(buf.slice(0, 10))).toBe('0123456789');
  });

  test('first frame at any seq is the origin, not a gap', () => {
    const buf = new SeqBuffer();
    const r = buf.apply(5000, b('live'));
    expect(r.gapFrom).toBeNull();
    expect(t(r.appended)).toBe('live');
    expect(buf.start).toBe(5000);
    expect(buf.head).toBe(5004);
  });

  test('history replayed after a live frame is kept (prepend)', () => {
    const buf = new SeqBuffer();
    buf.apply(5000, b('live'));
    const r = buf.apply(4990, b('history...'));
    expect(r.appended).toBeNull(); // not new tail bytes
    expect(buf.start).toBe(4990);
    expect(t(buf.slice(4990, 5004))).toBe('history...live');
  });

  test('evicted range is never re-accepted', () => {
    const buf = new SeqBuffer();
    buf.apply(0, b('abcdef'));
    buf.evictBefore(4);
    expect(buf.apply(0, b('ab')).appended).toBeNull();
    expect(buf.start).toBe(4);
  });

  test('many in-order frames stay one chunk', () => {
    const buf = new SeqBuffer();
    for (let i = 0; i < 1000; i++) buf.apply(i * 3, b('abc'));
    expect(buf.head).toBe(3000);
    expect(buf.size).toBe(3000);
    expect(t(buf.slice(2997, 3000))).toBe('abc');
  });

  test('evictBefore drops rendered bytes', () => {
    const buf = new SeqBuffer();
    buf.apply(0, b('aaaa'));
    buf.apply(4, b('bbbb'));
    buf.evictBefore(6);
    expect(buf.start).toBe(6);
    expect(buf.slice(0, 4)).toBeNull();
    expect(t(buf.slice(6, 8))).toBe('bb');
  });

  test('capacity evicts from the front', () => {
    const buf = new SeqBuffer(10);
    buf.apply(0, b('12345'));
    buf.apply(5, b('67890'));
    buf.apply(10, b('ABCDE'));
    expect(buf.size).toBeLessThanOrEqual(10);
    expect(buf.head).toBe(15);
    expect(buf.slice(0, 5)).toBeNull();
    expect(t(buf.slice(10, 15))).toBe('ABCDE');
  });
});

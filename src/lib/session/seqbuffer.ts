/**
 * Per-pane byte stream assembler keyed by absolute sequence number.
 *
 * helmd stamps every `Output` frame with `seq` = offset of its first
 * byte since pane creation. The frontend's job is simple but has to be
 * exact: apply frames in order, notice gaps (a dropped frame, a
 * reconnect), ask for a replay from the last contiguous point, and
 * hand out byte ranges by seq so finished blocks can be rendered from
 * `[start_seq, end_seq)`.
 *
 * Storage is a list of contiguous chunks; the common case (frames in
 * order) appends to the tail. Out-of-order frames (replay overlapping
 * live) are merged by seq; duplicates are ignored byte-for-byte.
 */

export interface Chunk {
  seq: number;
  bytes: Uint8Array;
}

export interface ApplyResult {
  /** Bytes that extended the contiguous head (to feed a live renderer). */
  appended: Uint8Array | null;
  /** A gap was detected before this frame: replay needed from `head`. */
  gapFrom: number | null;
}

export class SeqBuffer {
  /** Sorted by seq, non-overlapping after normalization. Chunks past a
   *  hole are "detached" until a replay bridges them. */
  private chunks: Chunk[] = []
  /** Bytes below this seq were evicted (capacity or `evictBefore`) and
   *  are never accepted again. Distinct from "oldest retained": an
   *  empty, never-evicted buffer accepts its first frame at any seq —
   *  the origin is simply unknown until then, not zero. */
  private floor = 0
  private totalSize = 0
  /** End of the contiguous prefix, maintained incrementally. */
  private contiguousHead = 0
  constructor(private readonly capacity = 16 * 1024 * 1024) {}

  /** Seq just past the last byte of the *contiguous prefix*. */
  get head(): number {
    return this.chunks.length ? this.contiguousHead : this.floor
  }

  /** Oldest retained seq. */
  get start(): number {
    return this.chunks[0]?.seq ?? this.floor
  }

  /** Total retained bytes (including detached chunks). */
  get size(): number {
    return this.totalSize
  }

  /** True when no hole exists between retained chunks. */
  get contiguous(): boolean {
    const last = this.chunks[this.chunks.length - 1]
    return !last || last.seq + last.bytes.length === this.contiguousHead
  }

  /**
   * Apply one frame. Returns the bytes that newly became contiguous
   * with the head (which may include previously-detached chunks this
   * frame bridged to) and, when the frame landed past the head without
   * bridging, the seq a replay must start from.
   */
  apply(seq: number, bytes: Uint8Array): ApplyResult {
    if (bytes.length === 0) return { appended: null, gapFrom: null }
    if (seq + bytes.length <= this.floor) return { appended: null, gapFrom: null } // evicted range

    const last = this.chunks[this.chunks.length - 1]
    if (!last) {
      // First frame ever: the buffer's origin is wherever it lands.
      this.chunks.push({ seq, bytes })
      this.totalSize = bytes.length
      this.contiguousHead = seq + bytes.length
      this.enforceCapacity()
      return { appended: bytes, gapFrom: null }
    }
    // Fast path: the common in-order frame — extend the tail chunk.
    if (this.contiguous && seq === this.contiguousHead) {
      this.appendToTail(last, bytes)
      this.enforceCapacity()
      return { appended: bytes, gapFrom: null }
    }

    const oldHead = this.head
    if (seq + bytes.length <= oldHead && seq >= this.start) {
      return { appended: null, gapFrom: null } // fully known already
    }
    this.insert(seq, bytes)
    const newHead = this.head
    const appended = newHead > oldHead ? this.slice(oldHead, newHead) : null
    const gapFrom = seq > oldHead && newHead === oldHead ? oldHead : null
    return { appended, gapFrom }
  }

  /** Bytes in `[from, to)` if fully retained and contiguous, else null. */
  slice(from: number, to: number): Uint8Array | null {
    if (to <= from) return new Uint8Array(0)
    if (from < this.start) return null
    const out = new Uint8Array(to - from)
    let filled = 0
    for (const c of this.chunks) {
      const cEnd = c.seq + c.bytes.length
      if (cEnd <= from || c.seq >= to) continue
      const s = Math.max(from, c.seq), e = Math.min(to, cEnd)
      out.set(c.bytes.subarray(s - c.seq, e - c.seq), s - from)
      filled += e - s
    }
    return filled === to - from ? out : null
  }

  /** Drop everything before `seq` (e.g. bytes of blocks already rendered). */
  evictBefore(seq: number): void {
    this.trimFront(seq)
  }

  /** Grow the tail chunk in place. Tail chunks are allocated with spare
   *  capacity so back-to-back frames reuse the same backing buffer. */
  private appendToTail(last: Chunk, bytes: Uint8Array): void {
    const needed = last.bytes.length + bytes.length
    const backing = last.bytes.buffer as ArrayBuffer
    if (last.bytes.byteOffset === 0 && needed <= backing.byteLength) {
      const grown = new Uint8Array(backing, 0, needed)
      grown.set(bytes, last.bytes.length)
      last.bytes = grown
    } else {
      const cap = Math.max(needed * 2, 64 * 1024)
      const fresh = new Uint8Array(new ArrayBuffer(cap), 0, needed)
      fresh.set(last.bytes, 0)
      fresh.set(bytes, last.bytes.length)
      last.bytes = fresh
    }
    this.totalSize += bytes.length
    this.contiguousHead += bytes.length
  }

  private insert(seq: number, bytes: Uint8Array): void {
    let idx = this.chunks.findIndex((c) => c.seq > seq)
    if (idx < 0) idx = this.chunks.length
    this.chunks.splice(idx, 0, { seq, bytes })
    this.normalize()
    this.enforceCapacity()
  }

  /** Resolve overlaps (earlier chunk wins — bytes at a seq never change),
   *  merge adjacent small chunks, and recompute the cached totals. */
  private normalize(): void {
    for (let i = 0; i + 1 < this.chunks.length; i++) {
      const a = this.chunks[i], b = this.chunks[i + 1]
      const aEnd = a.seq + a.bytes.length
      if (b.seq < aEnd) {
        const overlap = aEnd - b.seq
        if (overlap >= b.bytes.length) { this.chunks.splice(i + 1, 1); i--; continue }
        b.bytes = b.bytes.subarray(overlap)
        b.seq = aEnd
      }
      if (b.seq === aEnd && a.bytes.length + b.bytes.length <= 64 * 1024) {
        const merged = new Uint8Array(a.bytes.length + b.bytes.length)
        merged.set(a.bytes, 0)
        merged.set(b.bytes, a.bytes.length)
        a.bytes = merged
        this.chunks.splice(i + 1, 1)
        i--
      }
    }
    this.recount()
  }

  private recount(): void {
    let size = 0
    let head = this.chunks[0]?.seq ?? this.floor
    let broken = false
    for (const c of this.chunks) {
      size += c.bytes.length
      if (!broken && c.seq === head) head += c.bytes.length
      else broken = true
    }
    this.totalSize = size
    this.contiguousHead = head
  }

  private trimFront(seq: number): void {
    while (this.chunks.length && this.chunks[0].seq + this.chunks[0].bytes.length <= seq) {
      this.chunks.shift()
    }
    const first = this.chunks[0]
    if (first && first.seq < seq) {
      first.bytes = first.bytes.subarray(seq - first.seq)
      first.seq = seq
    }
    this.floor = Math.max(this.floor, seq)
    this.recount()
  }

  private enforceCapacity(): void {
    const overflow = this.totalSize - this.capacity
    if (overflow > 0) this.trimFront(this.start + overflow)
  }
}

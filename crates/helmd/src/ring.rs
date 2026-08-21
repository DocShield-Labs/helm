//! Per-pane scrollback: a bounded byte ring addressed by absolute
//! sequence numbers.
//!
//! `seq` is the offset of a byte since pane creation and never resets or
//! wraps (u64). The ring holds the most recent `capacity` bytes; replay
//! requests below `start_seq()` are clamped — the client learns it got a
//! truncated tail and renders from there. This replaces tmux's
//! `capture-pane` entirely: reattach is "give me bytes from seq N",
//! which is exact, needs no re-wrap, and has no cursor-restoration step.

use std::collections::VecDeque;

pub struct RingBuffer {
    data: VecDeque<u8>,
    capacity: usize,
    /// Seq of the next byte to be appended (== total bytes ever seen).
    head_seq: u64,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            data: VecDeque::with_capacity(capacity.min(64 * 1024)),
            capacity,
            head_seq: 0,
        }
    }

    /// Seq of the next byte to append.
    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    /// Seq of the oldest byte still retained.
    pub fn start_seq(&self) -> u64 {
        self.head_seq - self.data.len() as u64
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn append(&mut self, bytes: &[u8]) {
        // head_seq counts *offered* bytes, including any truncated away by
        // an oversized write — seq is a stream position, not a storage one.
        let offered = bytes.len() as u64;
        // A single write larger than the whole ring: only the tail survives.
        let bytes = if bytes.len() > self.capacity {
            &bytes[bytes.len() - self.capacity..]
        } else {
            bytes
        };
        let overflow = (self.data.len() + bytes.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.data.drain(..overflow);
        }
        self.data.reserve(bytes.len());
        self.data.extend(bytes.iter().copied());
        self.head_seq += offered;
    }

    /// Bytes from `seq` to head. Clamps to what's retained; returns the
    /// actual start seq of the returned bytes alongside them. `None` when
    /// `seq` is at or past head (nothing to replay).
    pub fn slice_from(&self, seq: u64) -> Option<(u64, Vec<u8>)> {
        if seq >= self.head_seq {
            return None;
        }
        let start = self.start_seq();
        let effective = seq.max(start);
        Some((effective, self.copy_from((effective - start) as usize)))
    }

    /// The most recent `n` bytes (first paint of an unseen pane).
    pub fn last_bytes(&self, n: u64) -> (u64, Vec<u8>) {
        let n = (n as usize).min(self.data.len());
        (self.head_seq - n as u64, self.copy_from(self.data.len() - n))
    }

    /// Copy `data[skip..]` out as two memcpys over the ring's halves
    /// (a byte-wise iterator copy is ~10× slower at MB sizes).
    fn copy_from(&self, skip: usize) -> Vec<u8> {
        let (a, b) = self.data.as_slices();
        let mut out = Vec::with_capacity(self.data.len() - skip);
        if skip < a.len() {
            out.extend_from_slice(&a[skip..]);
            out.extend_from_slice(b);
        } else {
            out.extend_from_slice(&b[skip - a.len()..]);
        }
        out
    }

    /// Borrow the retained bytes as (at most) two contiguous slices, for
    /// scans that don't need an owned copy (search).
    pub fn as_slices(&self) -> (&[u8], &[u8]) {
        self.data.as_slices()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_replay() {
        let mut r = RingBuffer::new(8);
        r.append(b"abcd");
        assert_eq!(r.head_seq(), 4);
        assert_eq!(r.start_seq(), 0);
        let (seq, bytes) = r.slice_from(1).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(bytes, b"bcd");
    }

    #[test]
    fn eviction_keeps_tail_and_advances_start() {
        let mut r = RingBuffer::new(4);
        r.append(b"abcdef"); // over capacity in one write
        assert_eq!(r.head_seq(), 6);
        assert_eq!(r.start_seq(), 2);
        let (seq, bytes) = r.slice_from(0).unwrap(); // clamped
        assert_eq!(seq, 2);
        assert_eq!(bytes, b"cdef");

        r.append(b"gh");
        assert_eq!(r.head_seq(), 8);
        assert_eq!(r.start_seq(), 4);
        assert_eq!(r.slice_from(4).unwrap().1, b"efgh");
    }

    #[test]
    fn replay_at_head_is_none() {
        let mut r = RingBuffer::new(8);
        r.append(b"xy");
        assert!(r.slice_from(2).is_none());
        assert!(r.slice_from(99).is_none());
    }

    #[test]
    fn last_bytes_clamps() {
        let mut r = RingBuffer::new(8);
        r.append(b"abcdefgh");
        let (seq, bytes) = r.last_bytes(3);
        assert_eq!(seq, 5);
        assert_eq!(bytes, b"fgh");
        let (seq, bytes) = r.last_bytes(100);
        assert_eq!(seq, 0);
        assert_eq!(bytes, b"abcdefgh");
    }
}

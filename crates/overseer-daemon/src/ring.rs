//! Fixed-capacity byte ring that keeps the newest bytes.

use std::collections::VecDeque;

use crate::vt::ModeTracker;

pub struct RingBuffer {
    cap: usize,
    buf: VecDeque<u8>,
    /// Fed every byte the ring drops, so it holds the terminal modes in
    /// force at the oldest retained byte.
    evicted: ModeTracker,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: VecDeque::new(),
            evicted: ModeTracker::new(),
        }
    }

    /// Append bytes, dropping the oldest ones when over capacity.
    pub fn push(&mut self, data: &[u8]) {
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        let from_buf = overflow.min(self.buf.len());
        if from_buf > 0 {
            let (a, b) = self.buf.as_slices();
            let a_n = from_buf.min(a.len());
            self.evicted.feed(&a[..a_n]);
            self.evicted.feed(&b[..from_buf - a_n]);
            self.buf.drain(..from_buf);
        }
        let from_data = overflow - from_buf;
        self.evicted.feed(&data[..from_data]);
        self.buf.extend(&data[from_data..]);
    }

    pub fn contents(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    /// What a new client replays: a preamble restoring the terminal modes
    /// set by bytes the ring has dropped, then the retained bytes. Replaying
    /// it leaves a fresh terminal in the modes the whole stream would.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = self.evicted.preamble();
        out.reserve(self.buf.len());
        out.extend(self.buf.iter().copied());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_newest_when_full() {
        let mut r = RingBuffer::new(8);
        r.push(b"abcdef");
        r.push(b"ghij");
        assert_eq!(r.contents(), b"cdefghij");
        r.push(b"0123456789");
        assert_eq!(r.contents(), b"23456789");
    }

    #[test]
    fn snapshot_restores_modes_from_dropped_bytes() {
        let mut r = RingBuffer::new(16);
        r.push(b"\x1b[?2004h");
        assert_eq!(r.snapshot(), b"\x1b[?2004h", "nothing dropped yet");
        // Split the sequence across the eviction boundary of two pushes.
        r.push(b"0123456789");
        r.push(b"abcdefghij");
        assert_eq!(r.contents(), b"456789abcdefghij");
        assert_eq!(r.snapshot(), b"\x1b[?2004h456789abcdefghij");
        // A later reset in the dropped part wins.
        r.push(b"\x1b[?2004l0123456789abcdef");
        assert_eq!(r.snapshot(), b"\x1b[?2004l0123456789abcdef");
    }
}

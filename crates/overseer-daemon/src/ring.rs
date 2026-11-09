//! Fixed-capacity byte ring that keeps the newest bytes.

use std::collections::VecDeque;

pub struct RingBuffer {
    cap: usize,
    buf: VecDeque<u8>,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: VecDeque::new(),
        }
    }

    /// Append bytes, dropping the oldest ones when over capacity.
    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(data);
    }

    pub fn contents(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
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
    }
}

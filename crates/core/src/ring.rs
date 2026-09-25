//! The most recent output of a session, by `seq`, so a subscriber that knows where it left
//! off can be given exactly what it missed while the bytes are still held.

use std::collections::VecDeque;

pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    /// The `seq` of the first byte held.
    start: u64,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
            start: 0,
        }
    }

    /// The `seq` just past the last byte held.
    pub fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.cap {
            self.start += (self.buf.len() + bytes.len() - self.cap) as u64;
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let over = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        if over > 0 {
            self.buf.drain(..over);
            self.start += over as u64;
        }
        self.buf.extend(bytes);
    }

    /// Everything from `seq` on, when it is still held.
    pub fn since(&self, seq: u64) -> Option<Vec<u8>> {
        if seq < self.start || seq > self.end() {
            return None;
        }
        let from = (seq - self.start) as usize;
        Some(self.buf.range(from..).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_the_tail_by_seq() {
        let mut r = Ring::new(8);
        r.push(b"abcd");
        r.push(b"efgh");
        assert_eq!(r.since(0).unwrap(), b"abcdefgh");
        r.push(b"ij");
        assert_eq!(r.end(), 10);
        assert_eq!(r.since(0), None);
        assert_eq!(r.since(2).unwrap(), b"cdefghij");
        assert_eq!(r.since(10).unwrap(), b"");
        assert_eq!(r.since(11), None);
        r.push(b"0123456789");
        assert_eq!(r.end(), 20);
        assert_eq!(r.since(12).unwrap(), b"23456789");
    }
}

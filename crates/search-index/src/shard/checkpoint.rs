//! Sequence numbers and the local checkpoint (Elasticsearch's `LocalCheckpointTracker`).
//!
//! The checkpoint is the highest sequence number below which every operation has been
//! processed. Operations complete out of order (concurrent writers, WAL replay in log order),
//! so processed numbers above the checkpoint are kept in a bitmap of 64-bit words starting at
//! a word-aligned base; the checkpoint advances through set bits and drops words it passes.
//! A number absurdly far ahead (more than 2^26 above the base) goes to a small ordered set
//! instead, so a corrupt or hostile sequence number can't size an allocation.

use std::collections::{BTreeSet, VecDeque};

const FAR: i64 = 1 << 26;

#[derive(Debug)]
pub struct Tracker {
    checkpoint: i64,
    /// Bit `i` of word `w` is seq no `base + 64 w + i`.
    base: i64,
    words: VecDeque<u64>,
    far: BTreeSet<i64>,
}

impl Tracker {
    /// `checkpoint`: every seq no `<= checkpoint` is processed (`-1`: none).
    pub fn new(checkpoint: i64) -> Tracker {
        let next = checkpoint + 1;
        Tracker { checkpoint, base: next - next.rem_euclid(64), words: VecDeque::new(), far: BTreeSet::new() }
    }

    pub fn checkpoint(&self) -> i64 {
        self.checkpoint
    }

    /// Marks `seq` processed.
    pub fn mark(&mut self, seq: i64) {
        if seq <= self.checkpoint {
            return;
        }
        let off = seq - self.base;
        if off >= FAR {
            self.far.insert(seq);
            return;
        }
        let w = (off / 64) as usize;
        if self.words.len() <= w {
            self.words.resize(w + 1, 0);
        }
        self.words[w] |= 1 << (off % 64);
        self.advance();
    }

    fn advance(&mut self) {
        loop {
            let next = self.checkpoint + 1;
            let off = next - self.base;
            let w = (off / 64) as usize;
            match self.words.get(w) {
                Some(&word) if word >> (off % 64) & 1 != 0 => {
                    self.checkpoint = next;
                    if (next + 1 - self.base) % 64 == 0 && w == 0 {
                        self.words.pop_front();
                        self.base += 64;
                    }
                }
                _ => break,
            }
            // Pull far entries in once the window reaches them.
            while let Some(&f) = self.far.first() {
                if f <= self.checkpoint {
                    self.far.pop_first();
                } else if f - self.base < FAR {
                    self.far.pop_first();
                    let o = f - self.base;
                    let w = (o / 64) as usize;
                    if self.words.len() <= w {
                        self.words.resize(w + 1, 0);
                    }
                    self.words[w] |= 1 << (o % 64);
                } else {
                    break;
                }
            }
        }
    }

    /// Heap bytes held.
    pub fn ram_bytes(&self) -> usize {
        self.words.capacity() * 8 + self.far.len() * 48
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::seq::SliceRandom;
    use rand::{SeedableRng, rngs::StdRng};

    #[test]
    fn out_of_order() {
        let mut t = Tracker::new(-1);
        t.mark(1);
        assert_eq!(t.checkpoint(), -1);
        t.mark(0);
        assert_eq!(t.checkpoint(), 1);
        let mut v: Vec<i64> = (2..5000).collect();
        v.shuffle(&mut StdRng::seed_from_u64(3));
        let max_missing = 4321;
        for &s in &v {
            if s != max_missing {
                t.mark(s);
            }
        }
        assert_eq!(t.checkpoint(), max_missing - 1);
        t.mark(max_missing);
        assert_eq!(t.checkpoint(), 4999);
        t.mark(10);
        assert_eq!(t.checkpoint(), 4999);
    }

    #[test]
    fn far_ahead_is_bounded() {
        let mut t = Tracker::new(99);
        t.mark(1 << 40);
        assert!(t.ram_bytes() < 1024);
        assert_eq!(t.checkpoint(), 99);
        t.mark(100);
        assert_eq!(t.checkpoint(), 100);
    }
}

//! Blocked Bloom filters over a field's terms, for fields looked up by exact value across many
//! segments (`_id`: every update, delete and realtime get probes each segment).
//!
//! Elasticsearch puts a Bloom filter in front of the `_id` terms
//! (`ES87BloomFilterPostingsFormat`) because most of those probes miss. A miss in the term
//! dictionary costs an index search plus a block scan (~300 ns, several cache misses); the filter answers most
//! misses with one cache line.
//!
//! The filter is *blocked* (Putze, Sanders, Singler: *Cache-, Hash- and Space-Efficient Bloom
//! Filters*): a key hashes to one 512-bit block (one cache line) and sets 7 bits inside it.
//! At 10 bits per key that gives a false-positive rate of about 1% — slightly above a classic
//! Bloom filter's 0.8%, for one memory access instead of seven. The filter is sized for one term
//! per doc of the segment (the `_id` shape); a field with more than twice as many terms gets no
//! filter rather than a useless one.
//!
//! ```text
//! num_blocks u64 | added u64 | blocks: num_blocks × 8 × u64 (little-endian)
//! ```

use crate::error::{Result, corrupt};
use crate::slice::Slice;

const BITS_PER_KEY: u64 = 10;
const BLOCK_WORDS: usize = 8;
const BLOCK_BYTES: usize = BLOCK_WORDS * 8;

const P0: u64 = 0xa076_1d64_78bd_642f;
const P1: u64 = 0xe703_7ed1_a0b4_28db;

#[inline]
fn mum(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

/// A stable 64-bit hash of `b` (wyhash-style multiply-mix with a murmur3 finalizer). Part of
/// the on-disk format: it must never change.
#[inline]
pub fn hash64(b: &[u8]) -> u64 {
    let mut h = (b.len() as u64).wrapping_mul(P0) ^ 0x2d35_8dcc_aa6c_78a5;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        h = mum(h ^ u64::from_le_bytes(c.try_into().unwrap()), P1);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut t = [0u8; 8];
        t[..rem.len()].copy_from_slice(rem);
        h = mum(h ^ u64::from_le_bytes(t), P1 ^ rem.len() as u64);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// The block of `h` and the 7 bit positions inside it (9 bits each).
#[inline]
fn probe(h: u64, num_blocks: u64) -> (usize, u64) {
    let block = ((h as u128 * num_blocks as u128) >> 64) as usize;
    (block, mum(h, P0))
}

/// Builds a filter while terms are written.
pub(crate) struct BloomBuilder {
    blocks: Vec<[u64; BLOCK_WORDS]>,
    expected: u64,
    added: u64,
}

impl BloomBuilder {
    /// A filter for about `expected_terms` terms.
    pub(crate) fn new(expected_terms: u64) -> BloomBuilder {
        let n = (expected_terms.max(1) * BITS_PER_KEY).div_ceil(512).max(1);
        BloomBuilder { blocks: vec![[0; BLOCK_WORDS]; n as usize], expected: expected_terms.max(1), added: 0 }
    }

    #[inline]
    pub(crate) fn add(&mut self, term: &[u8]) {
        let (b, mut bits) = probe(hash64(term), self.blocks.len() as u64);
        let block = &mut self.blocks[b];
        for _ in 0..7 {
            let pos = (bits & 511) as usize;
            block[pos >> 6] |= 1 << (pos & 63);
            bits >>= 9;
        }
        self.added += 1;
    }

    /// The section bytes, or `None` when far more terms were added than the filter was sized
    /// for (its false-positive rate would make it a cost).
    pub(crate) fn finish(self) -> Option<Vec<u8>> {
        if self.added > self.expected * 2 {
            return None;
        }
        let mut out = Vec::with_capacity(16 + self.blocks.len() * BLOCK_BYTES);
        out.extend_from_slice(&(self.blocks.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.added.to_le_bytes());
        for b in &self.blocks {
            for w in b {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
        Some(out)
    }
}

/// A filter read from a segment. Cheap to clone.
#[derive(Clone, Debug)]
pub struct BloomFilter {
    data: Slice,
    num_blocks: u64,
    added: u64,
}

impl BloomFilter {
    pub(crate) fn open(data: Slice) -> Result<BloomFilter> {
        if data.len() < 16 {
            return Err(corrupt("bloom filter truncated"));
        }
        let num_blocks = u64::from_le_bytes(data[..8].try_into().unwrap());
        let added = u64::from_le_bytes(data[8..16].try_into().unwrap());
        if num_blocks == 0 || num_blocks.checked_mul(BLOCK_BYTES as u64).is_none_or(|n| n + 16 != data.len() as u64) {
            return Err(corrupt("bad bloom filter size"));
        }
        Ok(BloomFilter { data: data.slice(16..data.len()), num_blocks, added })
    }

    /// False when `term` is certainly absent.
    #[inline]
    pub fn may_contain(&self, term: &[u8]) -> bool {
        self.may_contain_hash(hash64(term))
    }

    /// [`may_contain`](Self::may_contain) with a precomputed [`hash64`], so a lookup that
    /// probes many segments hashes once.
    #[inline]
    pub fn may_contain_hash(&self, h: u64) -> bool {
        let (b, mut bits) = probe(h, self.num_blocks);
        let block = &self.data[b * BLOCK_BYTES..(b + 1) * BLOCK_BYTES];
        let mut words = [0u64; BLOCK_WORDS];
        for (w, c) in words.iter_mut().zip(block.chunks_exact(8)) {
            *w = u64::from_le_bytes(c.try_into().unwrap());
        }
        let mut ok = true;
        for _ in 0..7 {
            let pos = (bits & 511) as usize;
            ok &= words[pos >> 6] >> (pos & 63) & 1 != 0;
            bits >>= 9;
        }
        ok
    }

    /// Terms added when the filter was built.
    pub fn len(&self) -> u64 {
        self.added
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0
    }

    /// Bytes of the filter.
    pub fn size_in_bytes(&self) -> usize {
        self.data.len() + 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(keys: impl Iterator<Item = Vec<u8>>, expected: u64) -> Option<BloomFilter> {
        let mut b = BloomBuilder::new(expected);
        for k in keys {
            b.add(&k);
        }
        b.finish().map(|v| BloomFilter::open(Slice::from(v)).unwrap())
    }

    #[test]
    fn no_false_negatives_and_low_false_positive_rate() {
        for n in [0u64, 1, 2, 63, 1000, 100_000] {
            let keys = |i: u64| format!("doc-{i:x}-{}", i.wrapping_mul(0x9E37_79B9)).into_bytes();
            let f = build((0..n).map(keys), n).unwrap();
            assert_eq!(f.len(), n);
            for i in 0..n {
                assert!(f.may_contain(&keys(i)), "false negative at {i} of {n}");
            }
            if n >= 1000 {
                let probes = 200_000u64;
                let fp = (n..n + probes).filter(|&i| f.may_contain(&keys(i))).count();
                let rate = fp as f64 / probes as f64;
                assert!(rate < 0.015, "n {n}: false-positive rate {rate}");
            }
        }
        // Short, empty and binary keys.
        let ks: Vec<Vec<u8>> = vec![vec![], vec![0], vec![0, 0], vec![255; 9], b"abcdefgh".to_vec(), b"abcdefgh\0".to_vec()];
        let f = build(ks.iter().cloned(), 100).unwrap();
        assert!(ks.iter().all(|k| f.may_contain(k)));
    }

    #[test]
    fn hash_is_stable_and_spreads() {
        // On-disk format: these values must never change.
        let fixed = [hash64(b""), hash64(b"a"), hash64(b"abcdefgh"), hash64(b"abcdefgh\0")];
        assert_eq!(fixed, [0x33605fe82fa60161, 0x55cbc9e4c53595df, 0x433d4a5ae8e1a42e, 0x2c11c0288109c6b0]);
        assert_ne!(hash64(b"a"), hash64(b"a\0"));
        assert_ne!(hash64(b""), hash64(b"\0"));
        // Bit balance over sequential keys.
        let mut ones = [0u32; 64];
        for i in 0..10_000u32 {
            let h = hash64(&i.to_le_bytes());
            for (b, o) in ones.iter_mut().enumerate() {
                *o += (h >> b & 1) as u32;
            }
        }
        assert!(ones.iter().all(|&o| (4500..5500).contains(&o)), "{ones:?}");
    }

    #[test]
    fn overfull_filters_are_dropped_and_corruption_detected() {
        assert!(build((0..1000u32).map(|i| i.to_le_bytes().to_vec()), 100).is_none());
        let mut b = BloomBuilder::new(10);
        b.add(b"x");
        let bytes = b.finish().unwrap();
        assert!(BloomFilter::open(Slice::from(bytes[..bytes.len() - 1].to_vec())).is_err());
        assert!(BloomFilter::open(Slice::from(vec![0u8; 16])).is_err());
        assert!(BloomFilter::open(Slice::from(vec![0u8; 3])).is_err());
    }
}

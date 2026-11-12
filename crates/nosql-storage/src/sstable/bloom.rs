//! The partition-key bloom filter of an SSTable (`bloom_filter_fp_chance`, Cassandra's default
//! 0.01). Keys hash once with XXH3-128; the two halves drive Kirsch–Mitzenmacher double hashing.

use crate::codec::{Reader, put_uvarint};
use anyhow::{Result, bail};
use xxhash_rust::xxh3::xxh3_128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bloom {
    k: u32,
    bits: Vec<u64>,
}

fn hashes(key: &[u8]) -> (u64, u64) {
    let h = xxh3_128(key);
    (h as u64, (h >> 64) as u64 | 1)
}

impl Bloom {
    /// A filter for `n` keys at false-positive chance `fp` (an `fp` >= 1 disables filtering).
    pub fn new(n: u64, fp: f64) -> Self {
        if fp >= 1.0 {
            return Self { k: 0, bits: Vec::new() };
        }
        let fp = fp.clamp(1e-6, 0.5);
        let n = n.max(1) as f64;
        let ln2 = std::f64::consts::LN_2;
        let m = (-(n * fp.ln()) / (ln2 * ln2)).ceil().max(64.0);
        // Cap at 1 GiB of bits per SSTable; `n` comes from estimates.
        let words = ((m / 64.0).ceil() as usize).min(1 << 24);
        let k = ((words as f64 * 64.0 / n) * ln2).round().clamp(1.0, 20.0) as u32;
        Self { k, bits: vec![0; words] }
    }

    pub fn insert(&mut self, key: &[u8]) {
        if self.k == 0 {
            return;
        }
        let (h1, h2) = hashes(key);
        let m = self.bits.len() as u64 * 64;
        for i in 0..self.k as u64 {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % m;
            self.bits[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }

    pub fn may_contain(&self, key: &[u8]) -> bool {
        if self.k == 0 {
            return true;
        }
        let (h1, h2) = hashes(key);
        let m = self.bits.len() as u64 * 64;
        (0..self.k as u64).all(|i| {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % m;
            self.bits[(bit / 64) as usize] & (1 << (bit % 64)) != 0
        })
    }

    pub fn size_bytes(&self) -> usize {
        self.bits.len() * 8
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        put_uvarint(out, self.k as u64);
        put_uvarint(out, self.bits.len() as u64);
        for w in &self.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Self> {
        let k = r.uvarint()?;
        let n = r.count(8)?;
        if k > 64 || (k > 0 && n == 0) {
            bail!("corrupt bloom filter");
        }
        let mut bits = Vec::with_capacity(n);
        for _ in 0..n {
            bits.push(r.u64_le()?);
        }
        Ok(Self { k: k as u32, bits })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives_and_bounded_false_positives() {
        let mut b = Bloom::new(10_000, 0.01);
        for i in 0..10_000u32 {
            b.insert(&i.to_be_bytes());
        }
        assert!((0..10_000u32).all(|i| b.may_contain(&i.to_be_bytes())));
        let fp = (10_000..110_000u32).filter(|i| b.may_contain(&i.to_be_bytes())).count();
        assert!(fp < 2_000, "false positives: {fp} / 100000");
        let mut out = Vec::new();
        b.encode(&mut out);
        assert_eq!(Bloom::decode(&mut Reader::new(&out)).unwrap(), b);
        assert!(Bloom::decode(&mut Reader::new(&out[..out.len() - 1])).is_err());
    }

    #[test]
    fn disabled_filter_matches_everything() {
        let b = Bloom::new(10, 1.0);
        assert!(b.may_contain(b"anything"));
    }
}

//! The executor's in-memory hash tables (hash joins' and aggregates' keys, DISTINCT,
//! subquery caches): a word-at-a-time multiply-rotate hasher with a final avalanche, seeded
//! once per process, in place of SipHash. Nothing observable depends on these tables'
//! iteration order (Postgres's own hash orders are modelled apart, `pghash`); the seed keeps
//! their layout from being predictable across processes.

use std::hash::{BuildHasher, Hasher};

pub type HashMap<K, V> = std::collections::HashMap<K, V, FastState>;
pub type HashSet<K> = std::collections::HashSet<K, FastState>;

static SEED: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b) | 1
});

#[derive(Clone, Copy)]
pub struct FastState(u64);

impl Default for FastState {
    fn default() -> Self {
        FastState(*SEED)
    }
}

impl BuildHasher for FastState {
    type Hasher = FastHasher;

    fn build_hasher(&self) -> FastHasher {
        FastHasher(self.0)
    }
}

pub struct FastHasher(u64);

const K: u64 = 0x517c_c1b7_2722_0a95;

impl FastHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(K);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.add(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(w) ^ ((rest.len() as u64) << 59));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // fmix64: every input bit reaches the top bits hashbrown's control bytes take.
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Row keys (`table | index | row id`, big-endian) spread over the buckets: bucket
    /// index bits and the control byte's top bits both take every key's differing bytes.
    #[test]
    fn row_keys_spread() {
        let s = FastState::default();
        let n = 1u64 << 20;
        let mut low = vec![0u32; 1 << 20];
        let mut top = [0u32; 128];
        for i in 0..n {
            let mut k = Vec::with_capacity(16);
            k.extend_from_slice(&16384u32.to_be_bytes());
            k.extend_from_slice(&0u32.to_be_bytes());
            k.extend_from_slice(&i.to_be_bytes());
            let h = s.hash_one(&k);
            low[(h & (n - 1)) as usize] += 1;
            top[(h >> 57) as usize] += 1;
        }
        let max = *low.iter().max().unwrap();
        assert!(max <= 12, "max bucket load {max}");
        let (lo, hi) = (*top.iter().min().unwrap(), *top.iter().max().unwrap());
        assert!(hi < lo * 2, "control bytes {lo}..{hi}");
    }
}

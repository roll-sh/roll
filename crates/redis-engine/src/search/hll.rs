//! The HyperLogLog behind `COUNT_DISTINCTISH`, `HLL` and `HLL_SUM` (`hll/hll.c`): `bits` index
//! bits, ranks capped at `32 - bits` (+1), the classic estimate with the small-range (linear
//! counting) and large-range corrections, truncated to an integer.

#[derive(Clone, Debug)]
pub struct Hll {
    pub bits: u8,
    pub registers: Vec<u8>,
}

impl Hll {
    /// `hll_init` (4..=20 bits).
    pub fn new(bits: u8) -> Option<Hll> {
        if !(4..=20).contains(&bits) {
            return None;
        }
        Some(Hll { bits, registers: vec![0; 1 << bits] })
    }

    /// `hll_load`: registers of a power-of-two length.
    pub fn load(regs: &[u8]) -> Option<Hll> {
        if !regs.len().is_power_of_two() {
            return None;
        }
        let mut h = Hll::new(regs.len().trailing_zeros() as u8)?;
        h.registers.copy_from_slice(regs);
        Some(h)
    }

    fn rank_bits(&self) -> u32 {
        32 - self.bits as u32
    }

    /// `hll_add_hash`.
    pub fn add_hash(&mut self, hash: u32) {
        let rb = self.rank_bits();
        let index = (hash.checked_shr(rb).unwrap_or(0)) as usize;
        let rank = if hash == 0 { 32 } else { hash.trailing_zeros() };
        let rank = rank.min(rb) as u8 + 1;
        if rank > self.registers[index] {
            self.registers[index] = rank;
        }
    }

    /// `hll_merge`: the larger register of each pair (sizes must match).
    pub fn merge(&mut self, other: &[u8]) -> bool {
        if other.len() != self.registers.len() {
            return false;
        }
        for (d, &s) in self.registers.iter_mut().zip(other) {
            if *d < s {
                *d = s;
            }
        }
        true
    }

    /// `hll_count`.
    pub fn count(&self) -> u64 {
        let size = self.registers.len() as f64;
        let alpha = match self.bits {
            4 => 0.673,
            5 => 0.697,
            6 => 0.709,
            _ => 0.7213 / (1.0 + 1.079 / size),
        };
        let n = self.registers.len() as u32;
        let alpha_mm = alpha * (n.wrapping_mul(n)) as f64;
        let mut sum = 0.0;
        for &r in &self.registers {
            sum += 1.0 / ((1u32 << r) as f64);
        }
        let mut estimate = alpha_mm / sum;
        if estimate <= 5.0 / 2.0 * size {
            let zeros = self.registers.iter().filter(|&&r| r == 0).count();
            if zeros > 0 {
                estimate = size * (size / zeros as f64).ln();
            }
        } else if estimate > (1.0 / 30.0) * 4294967296.0 {
            estimate = -4294967296.0 * (1.0 - estimate / 4294967296.0).ln();
        }
        // `(size_t)estimate`: saturating like aarch64's conversion.
        estimate as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_counts() {
        let mut h = Hll::new(8).unwrap();
        assert_eq!(h.count(), 0);
        // splitmix64's finalizer: well-mixed 32-bit hashes.
        for i in 0..100u64 {
            let mut z = i.wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            h.add_hash((z ^ (z >> 31)) as u32);
        }
        let c = h.count();
        assert!((80..=120).contains(&c), "{c}");
        let mut g = Hll::load(&h.registers).unwrap();
        assert!(g.merge(&h.registers));
        assert_eq!(g.count(), c);
        assert!(Hll::load(&[0; 3]).is_none());
    }

    #[test]
    fn rank_caps() {
        let mut h = Hll::new(8).unwrap();
        h.add_hash(0);
        assert_eq!(h.registers[0], 25);
        h.add_hash(0x0100_0000);
        assert_eq!(h.registers[1], 25);
        h.add_hash(0x0200_0001);
        assert_eq!(h.registers[2], 1);
    }
}

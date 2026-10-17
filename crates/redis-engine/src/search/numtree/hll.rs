//! The range cardinality estimator: `hyperloglog::HyperLogLog6<NumericValue, WyHasher>`, 64
//! one-byte registers fed by `wyhash` 0.5 (seed 0, truncated to 32 bits) over each value's
//! `Hash` (an 8-byte length prefix, then the native-endian bytes).

const P0: u64 = 0xa076_1d64_78bd_642f;
const P1: u64 = 0xe703_7ed1_a0b4_28db;
const P5: u64 = 0xeb44_acca_b455_d165;

/// Register index bits.
pub const BITS: u32 = 6;
/// Number of registers (`1 << BITS`).
pub const SIZE: usize = 1 << BITS;
/// `RANK_BITS`: the hash bits left for the rank.
const RANK_BITS: u32 = 32 - BITS;

fn wymum(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    ((r >> 64) ^ r) as u64
}

fn read32(d: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes([d[0], d[1], d[2], d[3]]))
}

/// `wyhash_core` for an 8-byte write: the seed mixed with `P0`, then one `read_rest` round.
fn core8(bytes: [u8; 8], seed: u64) -> u64 {
    let swapped = (read32(&bytes) << 32) | read32(&bytes[4..]);
    wymum(seed ^ P0, swapped ^ P1)
}

/// The 32-bit hash `HyperLogLog::add` computes for a value: `WyHash::with_seed(0)` fed
/// `[u8; 8]::hash` (`write_usize(8)` then the bytes), finished and truncated.
pub fn hash_value(value: f64) -> u32 {
    let h = core8(8u64.to_le_bytes(), 0);
    let h = core8(value.to_le_bytes(), h);
    wymum(h, 16 ^ P5) as u32
}

/// `rank`: trailing zeros capped at `RANK_BITS`, plus one.
const fn rank(hash: u32) -> u8 {
    let r = hash.trailing_zeros();
    (if r > RANK_BITS { RANK_BITS } else { r }) as u8 + 1
}

/// `HyperLogLog6`: register `hash >> 26` keeps the highest rank seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hll {
    registers: [u8; SIZE],
}

impl Default for Hll {
    fn default() -> Self {
        Self::new()
    }
}

impl Hll {
    pub const fn new() -> Self {
        Self { registers: [0; SIZE] }
    }

    /// `HyperLogLog::add` for a numeric value (raw bits, so `-0.0` and `0.0` differ).
    pub fn add(&mut self, value: f64) {
        self.add_hash(hash_value(value));
    }

    /// `add_precomputed_hash`.
    pub fn add_hash(&mut self, hash: u32) {
        let i = (hash >> RANK_BITS) as usize;
        let r = rank(hash);
        if r > self.registers[i] {
            self.registers[i] = r;
        }
    }

    /// `HyperLogLog::merge`.
    pub fn merge(&mut self, other: &Self) {
        for (a, b) in self.registers.iter_mut().zip(other.registers) {
            *a = (*a).max(b);
        }
    }

    pub const fn registers(&self) -> &[u8; SIZE] {
        &self.registers
    }

    pub const fn set_registers(&mut self, registers: [u8; SIZE]) {
        self.registers = registers;
    }

    pub fn clear(&mut self) {
        self.registers = [0; SIZE];
    }

    /// `HyperLogLog::count` (`compute_estimate`): the raw estimate with alpha 0.709, linear
    /// counting while it is at most `2.5 * SIZE` and some register is zero, the large range
    /// correction above `2^32 / 30`, truncated.
    pub fn count(&self) -> usize {
        let m = SIZE as f64;
        let alpha_mm = 0.709 * m * m;
        let sum: f64 = self.registers.iter().map(|&r| 1.0 / ((1u32 << r) as f64)).sum();
        let mut estimate = alpha_mm / sum;
        if estimate <= 2.5 * m {
            let zeros = self.registers.iter().filter(|&&r| r == 0).count();
            if zeros > 0 {
                estimate = m * (m / zeros as f64).ln();
            }
        } else if estimate > (1.0 / 30.0) * 4_294_967_296.0 {
            estimate = -4_294_967_296.0 * (1.0 - (estimate / 4_294_967_296.0)).ln();
        }
        estimate as usize
    }
}

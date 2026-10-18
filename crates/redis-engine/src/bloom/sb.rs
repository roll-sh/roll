//! Scalable bloom filters (RedisBloom's `SBChain`): a chain of bloom filters, each new link
//! larger by the growth factor and with half the error rate of the one before.

use super::hash::{murmur2, murmur64a};

/// Don't round the bit count up to a power of two (every filter BF.* creates).
pub const NOROUND: u32 = 1;
/// The capacity given is a bit count exponent.
pub const ENTS_IS_BITS: u32 = 2;
/// 64-bit hashing (every filter BF.* creates).
pub const FORCE64: u32 = 4;
/// Refuse to add a link once the last one is full.
pub const NO_SCALING: u32 = 8;

/// The module's own (truncated) ln 2.
#[allow(clippy::approx_constant)]
const LN2: f64 = 0.693147180559945;
const TIGHTENING: f64 = 0.5;

/// The serialized header: chain size, link count, options, growth.
const HEADER: usize = 8 + 4 + 4 + 4;
/// A serialized link: bytes, bits, size, error, bpe, hashes, entries, n2.
const LINK: usize = 8 + 8 + 8 + 8 + 8 + 4 + 8 + 1;

/// Largest filter allocation granted (a failed `RedisModule_TryCalloc` past it).
pub const MAX_ALLOC: u64 = 1 << 34;

pub const BAD_DATA: &str = "ERR received bad data";

/// One bloom filter of the chain (`struct bloom` and the link's item count).
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub hashes: u32,
    pub force64: bool,
    pub n2: u8,
    pub entries: u64,
    pub error: f64,
    pub bpe: f64,
    pub bits: u64,
    pub bf: Vec<u8>,
    /// Items added to this link.
    pub size: u64,
}

#[derive(Clone, Copy)]
struct Hash {
    a: u64,
    b: u64,
}

fn calc_bpe(error: f64) -> f64 {
    const DENOM: f64 = 0.480453013918201; // ln(2)^2
    (-(crate::musl_math::log(error) / DENOM)).abs()
}

/// `logb`: the binary exponent of `x` as a double.
fn logb(x: f64) -> f64 {
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if !x.is_finite() {
        return x.abs();
    }
    let bits = x.abs().to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i64;
    if exp == 0 {
        // Subnormal: the exponent of its leading bit.
        let mant = bits & ((1 << 52) - 1);
        return (-1022 - (mant.leading_zeros() as i64 - 12)) as f64;
    }
    (exp - 1023) as f64
}

/// Zeroed filter bytes, or `None` when the allocation would fail.
pub fn alloc(bytes: u64) -> Option<Vec<u8>> {
    if bytes > MAX_ALLOC {
        return None;
    }
    let mut v = Vec::new();
    v.try_reserve_exact(bytes as usize).ok()?;
    v.resize(bytes as usize, 0);
    Some(v)
}

pub enum InitError {
    Invalid,
    Oom,
}

impl Link {
    /// `bloom_init`.
    pub fn new(entries: u64, error: f64, options: u32) -> Result<Link, InitError> {
        if entries < 1 || error <= 0.0 || error >= 1.0 || error.is_nan() {
            return Err(InitError::Invalid);
        }
        let bpe = calc_bpe(error);
        let mut entries_out = entries;
        let n2;
        let bits;
        if options & ENTS_IS_BITS != 0 {
            if entries > 64 {
                return Err(InitError::Invalid);
            }
            n2 = entries as u8;
            bits = if entries == 64 { 0 } else { 1u64 << entries };
            entries_out = (bits as f64 / bpe) as u64;
        } else if options & NOROUND != 0 {
            let b = (entries as f64 * bpe) as u64;
            bits = b.max(1);
            n2 = 0;
        } else {
            let bn2 = logb(entries as f64 * bpe);
            if bn2 > 63.0 || bn2 == f64::INFINITY {
                return Err(InitError::Invalid);
            }
            n2 = (bn2 + 1.0) as u8;
            bits = if n2 >= 64 { 0 } else { 1u64 << n2 };
            let bit_diff = (bits as f64 - entries as f64 * bpe) as u64;
            let item_diff = (bit_diff as f64 / bpe) as u64;
            entries_out = entries_out.wrapping_add(item_diff);
        }
        let bytes = if bits % 64 != 0 { (bits / 64 + 1) * 8 } else { bits / 8 };
        let bf = alloc(bytes).ok_or(InitError::Oom)?;
        Ok(Link {
            hashes: hashes_for(bpe),
            force64: options & FORCE64 != 0,
            n2,
            entries: entries_out,
            error,
            bpe,
            bits: bytes.wrapping_mul(8),
            bf,
            size: 0,
        })
    }

    pub fn bytes(&self) -> u64 {
        self.bf.len() as u64
    }

    /// `bloom_validate_integrity`.
    pub fn valid(&self) -> bool {
        !(self.error <= 0.0
            || self.error >= 1.0
            || self.error.is_nan()
            || self.n2 > 63
            || (self.n2 != 0 && self.bits < (1u64 << self.n2))
            || self.bits == 0
            || self.bits != self.bytes().wrapping_mul(8)
            || self.hashes != hashes_for(self.bpe))
    }

    /// The bit positions of a hash (`bloom_check_add32/64/compat`: modulo `2^n2`, or the bit
    /// count for filters not sized in powers of two).
    fn positions(&self, h: Hash) -> impl Iterator<Item = u64> + use<> {
        let modulo = if self.n2 > 0 { 1u64 << self.n2 } else { self.bits };
        (0..self.hashes).map(move |i| h.a.wrapping_add((i as u64).wrapping_mul(h.b)) % modulo)
    }

    /// Set the item's bits; whether all of them were set already.
    fn add(&mut self, h: Hash) -> bool {
        let mut found_unset = false;
        for x in self.positions(h) {
            let (byte, mask) = ((x >> 3) as usize, 1u8 << (x % 8));
            if self.bf[byte] & mask == 0 {
                self.bf[byte] |= mask;
                found_unset = true;
            }
        }
        !found_unset
    }

    fn check(&self, h: Hash) -> bool {
        self.positions(h).all(|x| self.bf[(x >> 3) as usize] & (1u8 << (x % 8)) != 0)
    }
}

/// `(int)ceil(LN2 * bpe)` (aarch64 conversions saturate, like `as`).
fn hashes_for(bpe: f64) -> u32 {
    libm::ceil(LN2 * bpe) as i32 as u32
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chain {
    pub links: Vec<Link>,
    /// Items added to all links.
    pub size: u64,
    pub options: u32,
    pub growth: u32,
}

pub enum AddError {
    /// A non-scaling filter is full.
    Full,
    /// A new link couldn't be created.
    Failed,
}

impl Chain {
    /// `SB_NewChain`.
    pub fn new(capacity: u64, error: f64, options: u32, growth: u32) -> Result<Chain, InitError> {
        if capacity == 0 || error == 0.0 || error >= 1.0 {
            return Err(InitError::Invalid);
        }
        let tightening = if options & NO_SCALING != 0 { 1.0 } else { TIGHTENING };
        let link = Link::new(capacity, error * tightening, options)?;
        Ok(Chain { links: vec![link], size: 0, options, growth })
    }

    fn hash(&self, item: &[u8]) -> Hash {
        if self.options & FORCE64 != 0 {
            let a = murmur64a(item, 0xc6a4a7935bd1e995);
            Hash { a, b: murmur64a(item, a) }
        } else {
            let a = murmur2(item, 0x9747b28c) as u64;
            Hash { a, b: murmur2(item, a as u32) as u64 }
        }
    }

    /// `SBChain_Add`: whether the item was new.
    pub fn add(&mut self, item: &[u8]) -> Result<bool, AddError> {
        let h = self.hash(item);
        if self.links.iter().rev().any(|l| l.check(h)) {
            return Ok(false);
        }
        let cur = self.links.last().expect("a chain has a link");
        if cur.size >= cur.entries {
            if self.options & NO_SCALING != 0 {
                return Err(AddError::Full);
            }
            let error = cur.error * TIGHTENING;
            let entries = cur.entries.wrapping_mul(self.growth as u64);
            let link = Link::new(entries, error, self.options).map_err(|_| AddError::Failed)?;
            self.links.push(link);
        }
        let cur = self.links.last_mut().expect("a chain has a link");
        if cur.add(h) {
            return Ok(false);
        }
        cur.size += 1;
        self.size += 1;
        Ok(true)
    }

    /// `SBChain_Check`.
    pub fn check(&self, item: &[u8]) -> bool {
        let h = self.hash(item);
        self.links.iter().rev().any(|l| l.check(h))
    }

    /// BF.INFO CAPACITY: the links' entries.
    pub fn capacity(&self) -> u64 {
        self.links.iter().fold(0u64, |a, l| a.wrapping_add(l.entries))
    }

    /// `BFMemUsage`: the chain, its link array and the bit arrays.
    pub fn mem_usage(&self) -> u64 {
        32 + 64 * self.links.len() as u64 + self.links.iter().map(Link::bytes).sum::<u64>()
    }

    /// `SBChain_GetEncodedHeader`.
    pub fn header(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + LINK * self.links.len());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&(self.links.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.options.to_le_bytes());
        out.extend_from_slice(&self.growth.to_le_bytes());
        for l in &self.links {
            out.extend_from_slice(&l.bytes().to_le_bytes());
            out.extend_from_slice(&l.bits.to_le_bytes());
            out.extend_from_slice(&l.size.to_le_bytes());
            out.extend_from_slice(&l.error.to_le_bytes());
            out.extend_from_slice(&l.bpe.to_le_bytes());
            out.extend_from_slice(&l.hashes.to_le_bytes());
            out.extend_from_slice(&l.entries.to_le_bytes());
            out.push(l.n2);
        }
        out
    }

    /// `SB_NewChainFromHeader`.
    pub fn from_header(buf: &[u8]) -> Option<Chain> {
        if buf.len() < HEADER {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().expect("8 bytes"));
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().expect("4 bytes"));
        let n = u32_at(8) as usize;
        if n == 0 || buf.len() != HEADER + LINK * n {
            return None;
        }
        let options = u32_at(12);
        let mut chain = Chain { links: Vec::with_capacity(n), size: u64_at(0), options, growth: u32_at(16) };
        for i in 0..n {
            let o = HEADER + LINK * i;
            let bytes = u64_at(o);
            let mut link = Link {
                bits: u64_at(o + 8),
                size: u64_at(o + 16),
                error: f64::from_bits(u64_at(o + 24)),
                bpe: f64::from_bits(u64_at(o + 32)),
                hashes: u32_at(o + 40),
                entries: u64_at(o + 44),
                n2: buf[o + 52],
                force64: options & FORCE64 != 0,
                bf: Vec::new(),
            };
            // Validate with the claimed size before allocating it.
            if !(link.bits == bytes.wrapping_mul(8)) {
                return None;
            }
            link.bf = alloc(bytes)?;
            if !link.valid() {
                return None;
            }
            chain.links.push(link);
        }
        chain.valid().then_some(chain)
    }

    /// `SB_ValidateIntegrity`.
    pub fn valid(&self) -> bool {
        if self.options & !(NOROUND | ENTS_IS_BITS | FORCE64 | NO_SCALING) != 0 {
            return false;
        }
        let mut total = 0u64;
        for l in &self.links {
            match total.checked_add(l.size) {
                Some(t) => total = t,
                None => return false,
            }
        }
        total == self.size
    }

    /// `getLinkPos`: the link holding byte `iter - 1` of the concatenated bit arrays, and the
    /// offset into it.
    fn link_pos(&self, iter: i64) -> Option<(usize, u64)> {
        if iter < 1 {
            return None;
        }
        let cur = (iter - 1) as u64;
        let mut seek = 0u64;
        for (i, l) in self.links.iter().enumerate() {
            if seek + l.bytes() > cur {
                return Some((i, cur - seek));
            }
            seek += l.bytes();
        }
        None
    }

    /// `SBChain_GetEncodedChunk`: the next iterator and the chunk (`None` past the end).
    pub fn chunk(&self, iter: i64, max: u64) -> Option<(i64, &[u8])> {
        let (i, offset) = self.link_pos(iter)?;
        let l = &self.links[i];
        let len = (l.bytes() - offset).min(max);
        Some((iter + len as i64, &l.bf[offset as usize..(offset + len) as usize]))
    }

    /// `SBChain_LoadEncodedChunk`.
    pub fn load_chunk(&mut self, iter: i64, buf: &[u8]) -> Result<(), &'static str> {
        if iter <= 0 || (iter as u64) < buf.len() as u64 {
            return Err(BAD_DATA);
        }
        let iter = iter - buf.len() as i64;
        let (i, offset) = self.link_pos(iter).ok_or("ERR invalid offset - no link found")?;
        let l = &mut self.links[i];
        if buf.len() as u64 > l.bytes() - offset {
            return Err("ERR invalid chunk - Too big for current filter");
        }
        l.bf[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizing_like_bloom_init() {
        let c = Chain::new(100, 0.01, FORCE64 | NOROUND, 2).ok().unwrap();
        let l = &c.links[0];
        // error 0.005: bpe 11.03, 1103 bits rounded up to 64-bit words.
        assert_eq!(l.bits, 1152);
        assert_eq!(l.bytes(), 144);
        assert_eq!(l.hashes, 8);
        assert_eq!(l.entries, 100);
        assert_eq!(c.mem_usage(), 32 + 64 + 144);
    }

    #[test]
    fn adds_and_scales() {
        let mut c = Chain::new(2, 0.01, FORCE64 | NOROUND, 2).ok().unwrap();
        for i in 0..10 {
            let item = format!("item{i}");
            assert!(matches!(c.add(item.as_bytes()), Ok(true)));
            assert!(c.check(item.as_bytes()));
        }
        assert!(matches!(c.add(b"item3"), Ok(false)));
        assert_eq!(c.size, 10);
        assert_eq!(c.links.len(), 3);
        assert_eq!(c.capacity(), 2 + 4 + 8);
    }

    #[test]
    fn dump_round_trip() {
        let mut c = Chain::new(50, 0.001, FORCE64 | NOROUND, 4).ok().unwrap();
        for i in 0..200 {
            let _ = c.add(format!("{i}").as_bytes());
        }
        let mut copy = Chain::from_header(&c.header()).unwrap();
        let mut iter = 1;
        while let Some((next, data)) = c.chunk(iter, 7) {
            copy.load_chunk(next, data).unwrap();
            iter = next;
        }
        assert_eq!(copy, c);
    }
}

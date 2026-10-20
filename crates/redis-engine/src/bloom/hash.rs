//! The MurmurHash2 variants RedisBloom hashes items with, byte for byte (little-endian reads,
//! the C `int` length folded into the seed).

/// `MurmurHash2` (32-bit): bloom filters without FORCE64, count-min sketches and top-k.
pub fn murmur2(key: &[u8], seed: u32) -> u32 {
    const M: u32 = 0x5bd1e995;
    const R: u32 = 24;
    let mut h = seed ^ key.len() as u32;
    let (chunks, rest) = key.as_chunks::<4>();
    for c in chunks {
        let mut k = u32::from_le_bytes(*c);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M);
        h ^= k;
    }
    if rest.len() >= 3 {
        h ^= (rest[2] as u32) << 16;
    }
    if rest.len() >= 2 {
        h ^= (rest[1] as u32) << 8;
    }
    if !rest.is_empty() {
        h ^= rest[0] as u32;
        h = h.wrapping_mul(M);
    }
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h
}

/// `MurmurHash64A_Bloom`: bloom filters with FORCE64 and cuckoo filters.
pub fn murmur64a(key: &[u8], seed: u64) -> u64 {
    const M: u64 = 0xc6a4a7935bd1e995;
    const R: u32 = 47;
    let mut h = seed ^ (key.len() as i32 as i64 as u64).wrapping_mul(M);
    let (chunks, rest) = key.as_chunks::<8>();
    for c in chunks {
        let mut k = u64::from_le_bytes(*c);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h ^= k;
        h = h.wrapping_mul(M);
    }
    if !rest.is_empty() {
        for (i, b) in rest.iter().enumerate().rev() {
            h ^= (*b as u64) << (8 * i);
        }
        h = h.wrapping_mul(M);
    }
    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^= h >> R;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        assert_eq!(murmur2(b"", 0), 0);
        assert_eq!(murmur64a(b"", 0), 0);
    }
}

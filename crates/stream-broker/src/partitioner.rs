//! The Java client's default partitioner, so keyed records sent over HTTP land on the same
//! partition as when a Kafka producer sends them.

/// Kafka's `Utils.murmur2`.
pub fn murmur2(data: &[u8]) -> i32 {
    const SEED: u32 = 0x9747_b28c;
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;
    let len = data.len();
    let mut h = SEED ^ len as u32;
    let (chunks, rest) = data.as_chunks::<4>();
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
    h as i32
}

/// The partition for a key: `toPositive(murmur2(key)) % partitions`.
pub fn partition_for_key(key: &[u8], partitions: i32) -> i32 {
    (murmur2(key) & 0x7fff_ffff) % partitions.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_kafka_utils_test() {
        // From Kafka's UtilsTest.testMurmur2.
        let cases: [(&[u8], i32); 6] = [
            (b"21", -973_932_308),
            (b"foobar", -790_332_482),
            (b"a-little-bit-long-string", -985_981_536),
            (b"a-little-bit-longer-string", -1_486_304_829),
            (b"lkjh234lh9fiuh90y23oiuhsafujhadof229phr9h19h89h8", -58_897_971),
            (b"abc", 479_470_107),
        ];
        for (input, want) in cases {
            assert_eq!(murmur2(input), want, "{}", String::from_utf8_lossy(input));
        }
    }

    #[test]
    fn partitions_are_in_range() {
        for i in 0..1000 {
            let p = partition_for_key(format!("key-{i}").as_bytes(), 7);
            assert!((0..7).contains(&p));
        }
    }
}

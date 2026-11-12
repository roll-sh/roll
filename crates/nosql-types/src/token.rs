//! Partition tokens and partition-key encoding.
//!
//! `token` is `org.apache.cassandra.dht.Murmur3Partitioner.getToken` over
//! `utils.MurmurHash.hash3_x64_128` (Apache-2.0): the first 64 bits of MurmurHash3 x64-128 with
//! seed 0, except that tail bytes are *sign-extended* before shifting (a bug Cassandra keeps for
//! compatibility), `Long.MIN_VALUE` is mapped to `Long.MAX_VALUE`, and the empty key is the
//! minimum token. ScyllaDB computes identical tokens (verified on both images).
//!
//! A multi-column partition key is serialised as `CompositeType` does: per component a 2-byte
//! big-endian length, the bytes and an end-of-component byte `0x00`. A single-column key is the
//! column's bytes as they are.

use crate::MarshalError;

/// The token of the empty key and the lower bound of the ring.
pub const MIN_TOKEN: i64 = i64::MIN;
/// The largest token.
pub const MAX_TOKEN: i64 = i64::MAX;

#[inline]
fn fmix(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

/// Cassandra's `hash3_x64_128` (seed 0): both 64-bit halves.
pub fn murmur3_cassandra(key: &[u8]) -> (i64, i64) {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    let len = key.len();
    let nblocks = len / 16;
    let (mut h1, mut h2) = (0u64, 0u64);
    for i in 0..nblocks {
        let b = &key[i * 16..i * 16 + 16];
        let mut k1 = u64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]));
        let mut k2 = u64::from_le_bytes(b[8..].try_into().unwrap_or([0; 8]));
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1
            .rotate_left(27)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x52dc_e729);
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2
            .rotate_left(31)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x3849_5ab5);
    }
    let tail = &key[nblocks * 16..];
    // Sign-extended tail bytes: `((long) key.get(i)) << n`.
    let sx = |i: usize| tail[i] as i8 as i64 as u64;
    let (mut k1, mut k2) = (0u64, 0u64);
    let rem = len & 15;
    if rem > 8 {
        for i in (8..rem).rev() {
            k2 ^= sx(i) << (8 * (i - 8));
        }
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 ^= k2;
    }
    if rem > 0 {
        for i in (0..rem.min(8)).rev() {
            k1 ^= sx(i) << (8 * i);
        }
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 ^= k1;
    }
    h1 ^= len as u64;
    h2 ^= len as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1 as i64, h2 as i64)
}

/// The Murmur3Partitioner token of a serialised partition key.
pub fn token(partition_key: &[u8]) -> i64 {
    if partition_key.is_empty() {
        return MIN_TOKEN;
    }
    match murmur3_cassandra(partition_key).0 {
        i64::MIN => i64::MAX,
        t => t,
    }
}

/// Serialises a multi-column partition key (`CompositeType.build`). A component longer than
/// 65535 bytes cannot be encoded.
pub fn composite_key(components: &[&[u8]]) -> Result<Vec<u8>, MarshalError> {
    let mut out = Vec::with_capacity(components.iter().map(|c| c.len() + 3).sum());
    for c in components {
        let len = u16::try_from(c.len()).map_err(|_| {
            MarshalError::new(format!(
                "Key length of {} is longer than maximum of 65535",
                c.len()
            ))
        })?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(c);
        out.push(0);
    }
    Ok(out)
}

/// The partition key bytes for its column values: the value itself for one column, the
/// composite encoding otherwise.
pub fn partition_key(components: &[&[u8]]) -> Result<Vec<u8>, MarshalError> {
    match components {
        [one] => Ok(one.to_vec()),
        many => composite_key(many),
    }
}

/// Splits a composite key into its components. `None` if malformed (truncated, or an
/// end-of-component byte other than 0 before the last component).
pub fn split_composite(key: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut pos = 0;
    // A static-column marker (0xFFFF) is not part of a partition key.
    while pos < key.len() {
        let len = u16::from_be_bytes([*key.get(pos)?, *key.get(pos + 1)?]) as usize;
        pos += 2;
        let v = key.get(pos..pos + len)?;
        pos += len;
        let eoc = *key.get(pos)?;
        pos += 1;
        if eoc != 0 && pos != key.len() {
            return None;
        }
        out.push(v);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_both_references() {
        // `SELECT token(k)` on cassandra:5.0.9 and scylladb/scylla:2026.3.3 (identical).
        for (key, t) in [
            (&b"a"[..], -8_839_064_797_231_613_815i64),
            (b"abcdefghijklmno", -8_449_275_918_290_243_589),
            (b"abcdefghijklmnop", -4_266_531_025_627_334_877),
            (b"abcdefghijklmnopq", 8_459_014_091_212_432_983),
            ("ÿÿÿÿÿÿÿ".as_bytes(), 3_230_710_917_636_395_204),
            (
                "héllo wörld ünïcode ☃ 0123456789".as_bytes(),
                3_807_475_239_202_371_191,
            ),
            (&[0x80], -5_284_281_814_142_962_636),
            (&[0xff], -4_442_228_696_663_692_417),
            (
                &[0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87],
                -6_062_263_109_623_760_877,
            ),
            (
                &[
                    0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc,
                    0xfd, 0xfe,
                ],
                -7_737_243_313_669_454_511,
            ),
            (
                &[
                    0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc,
                    0xfd, 0xfe, 0xff, 0x00,
                ],
                2_962_097_511_150_331_042,
            ),
            (&[0x00], 5_048_724_184_180_415_669),
        ] {
            assert_eq!(token(key), t, "{key:02x?}");
        }
        for (k, t) in [
            (0i64, 2_945_182_322_382_062_539i64),
            (1, 6_292_367_497_774_912_474),
            (-1, 7_071_048_584_287_372_947),
            (-128, 3_704_606_467_661_385_153),
            (i64::MAX, -1_722_304_415_079_482_439),
        ] {
            assert_eq!(token(&k.to_be_bytes()), t, "{k}");
        }
        assert_eq!(token(&[]), MIN_TOKEN);
    }

    #[test]
    fn composite_keys() {
        // PRIMARY KEY ((a int, b text)) on both images.
        for (a, b, t) in [
            (1i32, "x", 746_584_927_563_629_270i64),
            (-1, "", 6_025_991_999_186_018_540),
            (
                2_147_483_647,
                "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
                -7_575_561_770_193_362_196,
            ),
        ] {
            let ab = a.to_be_bytes();
            let key = partition_key(&[&ab, b.as_bytes()]).unwrap();
            assert_eq!(token(&key), t, "{a} {b}");
            assert_eq!(split_composite(&key).unwrap(), vec![&ab[..], b.as_bytes()]);
        }
        assert_eq!(
            composite_key(&[b"ab", b""]).unwrap(),
            vec![0, 2, b'a', b'b', 0, 0, 0, 0]
        );
        assert_eq!(partition_key(&[b"ab"]).unwrap(), b"ab".to_vec());
        let big = vec![0u8; 70_000];
        assert_eq!(
            composite_key(&[&big]).unwrap_err().message,
            "Key length of 70000 is longer than maximum of 65535"
        );
        assert_eq!(split_composite(&[0, 5, 1]), None);
        assert_eq!(split_composite(&[0, 1, 1]), None);
        assert_eq!(split_composite(&[0]), None);
        assert_eq!(split_composite(&[0, 1, 1, 1, 0, 0, 0]), None);
        assert_eq!(split_composite(&[0, 1, 1, 1]), Some(vec![&[1u8][..]]));
        assert_eq!(split_composite(&[]), Some(vec![]));
    }

    #[test]
    fn murmur_halves() {
        // The second half is also exposed (ScyllaDB's shard and Cassandra's bloom filters use it).
        let (h1, h2) = murmur3_cassandra(b"abcdefghijklmnopq");
        assert_eq!(h1, 8_459_014_091_212_432_983);
        assert_ne!(h2, 0);
    }
}

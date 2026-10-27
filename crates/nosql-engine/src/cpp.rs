//! Behaviours of the C++ standard library (libstdc++) that ScyllaDB's answers depend on: the
//! iteration order of `std::unordered_map<sstring, ...>` decides, for example, the order of the
//! dropped columns in `DESCRIBE ... WITH INTERNALS` and which bad index option is reported
//! first.

/// libstdc++'s `std::_Hash_bytes` (64-bit MurmurHash2 variant), the hash of `std::string_view`
/// with its default seed.
pub fn hash_bytes(b: &[u8]) -> u64 {
    const MUL: u64 = (0xc6a4_a793_u64 << 32) + 0x5bd1_e995;
    let shift_mix = |v: u64| v ^ (v >> 47);
    let mut h = 0xc70f_6907_u64 ^ (b.len() as u64).wrapping_mul(MUL);
    let (chunks, rest) = b.as_chunks::<8>();
    for c in chunks {
        let d = shift_mix(u64::from_le_bytes(*c).wrapping_mul(MUL)).wrapping_mul(MUL);
        h = (h ^ d).wrapping_mul(MUL);
    }
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(MUL);
    }
    shift_mix(shift_mix(h).wrapping_mul(MUL))
}

/// The iteration order of a libstdc++ `std::unordered_map` with string keys after inserting
/// `keys` in the given order: the `_Prime_rehash_policy` bucket counts (13 at the first
/// insertion, then the next prime of at least twice as many), each new node at the front of its
/// bucket, or of the whole list when its bucket is empty; a rehash re-inserts in list order.
/// Names past the bucket counts modelled here keep their insertion order.
pub fn unordered_map_order<'a>(keys: &[&'a str]) -> Vec<&'a str> {
    const PRIMES: &[u64] = &[
        13, 29, 59, 127, 257, 541, 1109, 2357, 5087, 10273, 20753, 42043, 85229,
    ];
    let mut order: Vec<&str> = Vec::with_capacity(keys.len());
    let mut buckets: u64 = 0;
    let mut capacity: usize = 0;
    let place = |order: &mut Vec<&'a str>, k: &'a str, n: u64| {
        let b = hash_bytes(k.as_bytes()) % n;
        match order.iter().position(|x| hash_bytes(x.as_bytes()) % n == b) {
            Some(i) => order.insert(i, k),
            None => order.insert(0, k),
        }
    };
    for &k in keys {
        if order.len() + 1 > capacity {
            let Some(&n) = PRIMES
                .iter()
                .find(|&&p| p > buckets.max(1) && p as usize > order.len())
            else {
                order.push(k);
                continue;
            };
            buckets = n;
            capacity = n as usize;
            let old = std::mem::take(&mut order);
            for x in old {
                place(&mut order, x, n);
            }
        }
        place(&mut order, k, buckets);
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Orders the reference lists dropped columns in (`DESC TABLE ... WITH INTERNALS`) and
    /// validates index options in.
    #[test]
    fn dropped_column_order() {
        for (names, want) in [
            (&["a", "s"][..], "s a"),
            (&["a", "b", "c", "d", "e", "f", "g"][..], "g f e c b d a"),
            (&["a", "c", "e"][..], "e c a"),
            (
                &[
                    "a",
                    "c",
                    "cdc$deleted_a",
                    "cdc$deleted_c",
                    "cdc$deleted_e",
                    "e",
                    "v",
                ][..],
                "v e cdc$deleted_e cdc$deleted_c cdc$deleted_a c a",
            ),
            (&["a", "cdc$deleted_a", "v"][..], "v cdc$deleted_a a"),
        ] {
            assert_eq!(unordered_map_order(names).join(" "), want);
        }
        let many: Vec<String> = (0..40).map(|i| format!("c{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(unordered_map_order(&refs).len(), 40);
    }
}

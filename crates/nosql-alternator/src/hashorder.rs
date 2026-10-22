//! The iteration order of a libstdc++ `std::unordered_set<std::string>`.
//!
//! ScyllaDB prints some sets of names straight from such containers (DescribeTable's
//! AttributeDefinitions, the spurious AttributeDefinitions error, the write-isolation values),
//! so the order clients see is libstdc++'s: `std::hash<std::string>` (`_Hash_bytes`, a 64-bit
//! Murmur2 variant with seed `0xc70f6907`), prime bucket counts starting at 13, and each new
//! element linked in front of its bucket's run (or at the very front for an empty bucket).

const MUL: u64 = (0xc6a4_a793u64 << 32) + 0x5bd1_e995;

fn shift_mix(v: u64) -> u64 {
    v ^ (v >> 47)
}

/// libstdc++ `std::_Hash_bytes(data, len, 0xc70f6907)` for 64-bit `size_t`.
pub fn hash_bytes(b: &[u8]) -> u64 {
    let len = b.len();
    let aligned = len & !7;
    let mut h = 0xc70f_6907u64 ^ (len as u64).wrapping_mul(MUL);
    for chunk in b[..aligned].chunks_exact(8) {
        let mut w = [0u8; 8];
        w.copy_from_slice(chunk);
        let d = shift_mix(u64::from_le_bytes(w).wrapping_mul(MUL)).wrapping_mul(MUL);
        h ^= d;
        h = h.wrapping_mul(MUL);
    }
    if len & 7 != 0 {
        let mut w = [0u8; 8];
        w[..len & 7].copy_from_slice(&b[aligned..]);
        h ^= u64::from_le_bytes(w);
        h = h.wrapping_mul(MUL);
    }
    h = shift_mix(h).wrapping_mul(MUL);
    shift_mix(h)
}

const FAST_BUCKETS: [usize; 14] = [2, 2, 2, 3, 5, 5, 7, 7, 11, 11, 11, 11, 13, 13];

fn is_prime(n: usize) -> bool {
    n >= 2
        && (2..)
            .take_while(|d| d * d <= n)
            .all(|d| !n.is_multiple_of(d))
}

/// `_Prime_rehash_policy::_M_next_bkt`: the smallest listed prime ≥ n (libstdc++'s table holds
/// every prime in the range that matters here).
fn next_bucket(n: usize) -> usize {
    if n < FAST_BUCKETS.len() {
        return if n == 0 { 1 } else { FAST_BUCKETS[n] };
    }
    (n..).find(|&p| is_prime(p)).unwrap_or(n)
}

/// The order in which an `unordered_set<string>` built by inserting `items` (in order,
/// duplicates ignored) iterates them.
pub fn unordered_set_order<S: AsRef<str>>(items: &[S]) -> Vec<String> {
    let mut list: Vec<(String, u64)> = Vec::new();
    let mut buckets = 1usize;
    let mut next_resize = 0usize;
    let place = |list: &mut Vec<(String, u64)>, buckets: usize, name: String, h: u64| {
        let b = h % buckets as u64;
        match list.iter().position(|(_, x)| x % buckets as u64 == b) {
            Some(i) => list.insert(i, (name, h)),
            None => list.insert(0, (name, h)),
        }
    };
    for s in items {
        let s = s.as_ref();
        if list.iter().any(|(x, _)| x == s) {
            continue;
        }
        let n = list.len();
        if n + 1 > next_resize {
            let min = (n + 1).max(if next_resize == 0 { 11 } else { 0 });
            if min >= buckets {
                buckets = next_bucket((min + 1).max(buckets * 2));
                next_resize = buckets;
                let old = std::mem::take(&mut list);
                for (name, h) in old {
                    place(&mut list, buckets, name, h);
                }
            } else {
                next_resize = buckets;
            }
        }
        let h = hash_bytes(s.as_bytes());
        place(&mut list, buckets, s.to_string(), h);
    }
    list.into_iter().map(|(s, _)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_scylla_outputs() {
        // DescribeTable of a table keyed (p, c) with an index on x.
        assert_eq!(unordered_set_order(&["p", "c", "x"]), ["x", "c", "p"]);
        assert_eq!(unordered_set_order(&["p", "c"]), ["c", "p"]);
        // "AttributeDefinitions defines spurious attributes ...: {"d", "c"}"
        assert_eq!(unordered_set_order(&["c", "d"]), ["d", "c"]);
    }
}

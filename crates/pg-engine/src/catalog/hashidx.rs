//! Hash indexes' sizes (access/hash): the 32-bit hash codes Postgres computes for a key
//! (common/hashfn.c's hash_bytes and the type's hash support function), and the bucket
//! layout those codes get as they are inserted (_hash_init, _hash_doinsert,
//! _hash_expandtable with its splitpoint phases, overflow pages and their reuse).

use crate::types::{DataType, Value};

fn rot(x: u32, k: u32) -> u32 {
    x.rotate_left(k)
}

fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= rot(*c, 4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= rot(*a, 6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= rot(*b, 8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= rot(*c, 16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= rot(*a, 19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= rot(*b, 4);
    *b = b.wrapping_add(*a);
}

fn fin(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(rot(*b, 14));
    *a ^= *c;
    *a = a.wrapping_sub(rot(*c, 11));
    *b ^= *a;
    *b = b.wrapping_sub(rot(*a, 25));
    *c ^= *b;
    *c = c.wrapping_sub(rot(*b, 16));
    *a ^= *c;
    *a = a.wrapping_sub(rot(*c, 4));
    *b ^= *a;
    *b = b.wrapping_sub(rot(*a, 14));
    *c ^= *b;
    *c = c.wrapping_sub(rot(*b, 24));
}

/// hash_bytes (hash_any).
pub fn hash_bytes(k: &[u8]) -> u32 {
    let mut a = 0x9e37_79b9u32
        .wrapping_add(k.len() as u32)
        .wrapping_add(3_923_095);
    let (mut b, mut c) = (a, a);
    let mut k = k;
    let w = |s: &[u8], i: usize| u32::from_le_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);
    while k.len() >= 12 {
        a = a.wrapping_add(w(k, 0));
        b = b.wrapping_add(w(k, 4));
        c = c.wrapping_add(w(k, 8));
        mix(&mut a, &mut b, &mut c);
        k = &k[12..];
    }
    let n = k.len();
    let byte = |i: usize, sh: u32| (k[i] as u32) << sh;
    if n >= 11 {
        c = c.wrapping_add(byte(10, 24));
    }
    if n >= 10 {
        c = c.wrapping_add(byte(9, 16));
    }
    if n >= 9 {
        c = c.wrapping_add(byte(8, 8));
    }
    if n >= 8 {
        b = b.wrapping_add(byte(7, 24));
    }
    if n >= 7 {
        b = b.wrapping_add(byte(6, 16));
    }
    if n >= 6 {
        b = b.wrapping_add(byte(5, 8));
    }
    if n >= 5 {
        b = b.wrapping_add(byte(4, 0));
    }
    if n >= 4 {
        a = a.wrapping_add(byte(3, 24));
    }
    if n >= 3 {
        a = a.wrapping_add(byte(2, 16));
    }
    if n >= 2 {
        a = a.wrapping_add(byte(1, 8));
    }
    if n >= 1 {
        a = a.wrapping_add(byte(0, 0));
    }
    fin(&mut a, &mut b, &mut c);
    c
}

/// hash_bytes_uint32 (hash_uint32).
pub fn hash_uint32(k: u32) -> u32 {
    let mut a = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let (mut b, mut c) = (a, a);
    a = a.wrapping_add(k);
    fin(&mut a, &mut b, &mut c);
    c
}

fn hash_int8(v: i64) -> u32 {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    hash_uint32(lo ^ if v >= 0 { hi } else { !hi })
}

/// The hash support function's code for a key (None: a type not modelled).
pub fn hash_value(v: &Value, ty: &DataType) -> Option<u32> {
    use DataType as T;
    Some(match v {
        Value::Int2(x) => hash_uint32(*x as i32 as u32),
        Value::Int4(x) | Value::Date(x) => hash_uint32(*x as u32),
        Value::Int8(x) | Value::Timestamp(x) | Value::Timestamptz(x) | Value::Time(x) => {
            hash_int8(*x)
        }
        Value::Oid(x) => hash_uint32(*x),
        Value::Bool(b) => hash_uint32(*b as u32),
        Value::Char(c) => hash_uint32(*c as i8 as i32 as u32),
        Value::Float8(f) => {
            if *f == 0.0 {
                0
            } else if f.is_nan() {
                hash_bytes(&f64::NAN.to_le_bytes())
            } else {
                hash_bytes(&f.to_le_bytes())
            }
        }
        Value::Float4(f) => {
            // hashfloat4 hashes the value widened to float8.
            let d = *f as f64;
            if d == 0.0 {
                0
            } else if d.is_nan() {
                hash_bytes(&f64::NAN.to_le_bytes())
            } else {
                hash_bytes(&d.to_le_bytes())
            }
        }
        Value::Uuid(u) => hash_bytes(&u.to_be_bytes()),
        Value::Text(s) => match ty {
            T::Bpchar(_) => hash_bytes(s.trim_end_matches(' ').as_bytes()),
            _ => hash_bytes(s.as_bytes()),
        },
        Value::Bytea(b) => hash_bytes(b),
        _ => return None,
    })
}

const SPLITPOINT_PHASE_BITS: u32 = 2;
const SPLITPOINT_PHASE_MASK: u32 = 3;
const GROUPS_WITH_ONE_PHASE: u32 = 10;
/// Index tuples of a 4-byte hash code (16 bytes) and a line pointer per page:
/// (8192 - page header 24 - special 16) / 20.
const PER_PAGE: usize = 407;

fn ceil_log2(n: u32) -> u32 {
    if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() }
}

/// _hash_spareindex.
fn spareindex(num_bucket: u32) -> u32 {
    let group = ceil_log2(num_bucket);
    if group < GROUPS_WITH_ONE_PHASE {
        return group;
    }
    let mut phases = GROUPS_WITH_ONE_PHASE;
    phases += (group - GROUPS_WITH_ONE_PHASE) << SPLITPOINT_PHASE_BITS;
    phases += ((num_bucket - 1) >> (group - (SPLITPOINT_PHASE_BITS + 1))) & SPLITPOINT_PHASE_MASK;
    phases
}

/// _hash_get_totalbuckets.
fn totalbuckets(phase: u32) -> u32 {
    if phase < GROUPS_WITH_ONE_PHASE {
        return 1 << phase;
    }
    let group = GROUPS_WITH_ONE_PHASE + ((phase - GROUPS_WITH_ONE_PHASE) >> SPLITPOINT_PHASE_BITS);
    let mut total = 1u32 << (group - 1);
    let within = ((phase - GROUPS_WITH_ONE_PHASE) & SPLITPOINT_PHASE_MASK) + 1;
    total += ((1u32 << (group - 1)) >> SPLITPOINT_PHASE_BITS) * within;
    total
}

/// The pages of a hash index built expecting `est_tuples` (estimate_rel_size's) with
/// `fillfactor`, after the codes `codes` went in, in order.
pub fn index_pages(est_tuples: f64, fillfactor: usize, codes: impl Iterator<Item = u32>) -> u64 {
    simulate(est_tuples, fillfactor, codes, None).0
}

/// The bucket hash code `code` goes to once `codes` went in (what its SIRead locks and
/// insertions' conflict checks name: the bucket's primary page).
pub fn bucket_of_code(
    est_tuples: f64,
    fillfactor: usize,
    codes: impl Iterator<Item = u32>,
    code: u32,
) -> u32 {
    simulate(est_tuples, fillfactor, codes, Some(code)).1
}

fn simulate(
    est_tuples: f64,
    fillfactor: usize,
    codes: impl Iterator<Item = u32>,
    probe: Option<u32>,
) -> (u64, u32) {
    // _hash_init: the split threshold per bucket, and the initial buckets.
    let item_width = 8 + 8 + 4;
    let ffactor = ((8192 * fillfactor / 100) / item_width).max(10) as f64;
    let dbuckets = est_tuples / ffactor;
    let nbuckets: u32 = if dbuckets <= 2.0 {
        2
    } else if dbuckets >= 1_073_741_824.0 {
        1 << 30
    } else {
        totalbuckets(spareindex(dbuckets as u32))
    };
    let mut ovflpoint = spareindex(nbuckets);
    let mut maxbucket = nbuckets - 1;
    let mut highmask = (nbuckets + 1).next_power_of_two() - 1;
    let mut lowmask = highmask >> 1;
    // Per bucket, its codes; overflow pages in use and free; bucket pages allocated.
    let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); nbuckets as usize];
    let pages_of = |n: usize| n.div_ceil(PER_PAGE).max(1) - 1;
    let mut ovfl_used = 0usize;
    // (The bitmap page is the first overflow page.)
    let mut ovfl_alloc = 1usize;
    let mut bucket_pages = nbuckets as usize;
    let mut ntuples = 0f64;
    let bucket_of = |h: u32, maxbucket: u32, highmask: u32, lowmask: u32| {
        let b = h & highmask;
        if b > maxbucket { b & lowmask } else { b }
    };
    let take_ovfl = |n: usize, used: &mut usize, alloc: &mut usize| {
        *used += n;
        if *used + 1 > *alloc {
            *alloc = *used + 1;
        }
    };
    for h in codes {
        let b = bucket_of(h, maxbucket, highmask, lowmask) as usize;
        let before = pages_of(buckets[b].len());
        buckets[b].push(h);
        let after = pages_of(buckets[b].len());
        if after > before {
            take_ovfl(after - before, &mut ovfl_used, &mut ovfl_alloc);
        }
        ntuples += 1.0;
        // _hash_expandtable.
        if ntuples > ffactor * (maxbucket as f64 + 1.0) {
            let new_bucket = maxbucket + 1;
            let old_bucket = new_bucket & lowmask;
            maxbucket = new_bucket;
            if new_bucket > highmask {
                lowmask = highmask;
                highmask = new_bucket | lowmask;
            }
            let spare = spareindex(new_bucket + 1);
            if spare > ovflpoint {
                bucket_pages = totalbuckets(spare) as usize;
                ovflpoint = spare;
            }
            let old = std::mem::take(&mut buckets[old_bucket as usize]);
            let old_pages = pages_of(old.len());
            let (moved, kept): (Vec<u32>, Vec<u32>) =
                old.into_iter().partition(|h| h & highmask == new_bucket);
            let new_pages = pages_of(moved.len());
            take_ovfl(new_pages, &mut ovfl_used, &mut ovfl_alloc);
            // The old bucket squeezed: its spare overflow pages are free again.
            ovfl_used -= old_pages - pages_of(kept.len());
            buckets[old_bucket as usize] = kept;
            buckets.push(moved);
        }
    }
    let probed = probe.map_or(0, |h| bucket_of(h, maxbucket, highmask, lowmask));
    (1 + bucket_pages as u64 + ovfl_alloc as u64, probed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_postgres() {
        assert_eq!(hash_uint32(1) as i32, -1905060026);
        assert_eq!(hash_uint32(-5i32 as u32) as i32, 1412886042);
        assert_eq!(hash_int8(5) as i32, -1330264708);
        assert_eq!(hash_int8(-7) as i32, -82204417);
        assert_eq!(hash_bytes(b"abc") as i32, -785388649);
        assert_eq!(hash_bytes(b"") as i32, -1477818771);
        assert_eq!(
            hash_bytes(b"hello world, this is longer than twelve") as i32,
            -1245504862
        );
        assert_eq!(hash_uint32(16384) as i32, -1726903597);
        assert_eq!(hash_bytes(&1.5f64.to_le_bytes()) as i32, 630860146);
        assert_eq!(hash_uint32(3) as i32, -28094569);
        assert_eq!(hash_uint32(b'a' as u32) as i32, 463612535);
        assert_eq!(hash_bytes(b"ab") as i32, 1718550461);
        let u: u128 = 0xa0eebc999c0b4ef8bb6d6bb9bd380a11;
        assert_eq!(hash_bytes(&u.to_be_bytes()) as i32, 1154666245);
    }
}

#[cfg(test)]
mod sim_tests {
    #[test]
    fn grows_like_postgres() {
        let codes = (1..=20000u32).map(super::hash_uint32);
        assert_eq!(super::index_pages(1000.0, 75, codes), 149);
    }
}

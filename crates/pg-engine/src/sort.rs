//! Postgres's `qsort` (`pg_qsort`, from lib/sort_template.h): Bentley & McIlroy's
//! quicksort. It isn't stable, so code whose output depends on the order of equal elements
//! sorts with it to match.

use std::cmp::Ordering;

fn med3<T>(
    v: &[T],
    a: usize,
    b: usize,
    c: usize,
    cmp: &mut impl FnMut(&T, &T) -> Ordering,
) -> usize {
    if cmp(&v[a], &v[b]).is_lt() {
        if cmp(&v[b], &v[c]).is_lt() {
            b
        } else if cmp(&v[a], &v[c]).is_lt() {
            c
        } else {
            a
        }
    } else if cmp(&v[b], &v[c]).is_gt() {
        b
    } else if cmp(&v[a], &v[c]).is_lt() {
        a
    } else {
        c
    }
}

fn swapn<T>(v: &mut [T], a: usize, b: usize, n: usize) {
    for i in 0..n {
        v.swap(a + i, b + i);
    }
}

/// Sort `v` as `pg_qsort` would.
pub fn pg_qsort<T>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    sort(v, &mut cmp);
}

fn sort<T>(v: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    // `a` and `n` of the C code: the part still being sorted.
    let (mut a, mut n) = (0usize, v.len());
    loop {
        if n < 7 {
            for pm in a + 1..a + n {
                let mut pl = pm;
                while pl > a && cmp(&v[pl - 1], &v[pl]).is_gt() {
                    v.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return;
        }
        if (a + 1..a + n).all(|pm| !cmp(&v[pm - 1], &v[pm]).is_gt()) {
            return;
        }
        let mut pm = a + n / 2;
        if n > 7 {
            let mut pl = a;
            let mut pn = a + n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(v, pl, pl + d, pl + 2 * d, cmp);
                pm = med3(v, pm - d, pm, pm + d, cmp);
                pn = med3(v, pn - 2 * d, pn - d, pn, cmp);
            }
            pm = med3(v, pl, pm, pn, cmp);
        }
        v.swap(a, pm);
        let (mut pa, mut pb) = (a + 1, a + 1);
        // pc and pd may step below pb, past `a` (as pointers that are never dereferenced
        // there): signed.
        let (mut pc, mut pd) = ((a + n - 1) as isize, (a + n - 1) as isize);
        loop {
            while pb as isize <= pc {
                let r = cmp(&v[pb], &v[a]);
                if r.is_gt() {
                    break;
                }
                if r.is_eq() {
                    v.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb as isize <= pc {
                let r = cmp(&v[pc as usize], &v[a]);
                if r.is_lt() {
                    break;
                }
                if r.is_eq() {
                    v.swap(pc as usize, pd as usize);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb as isize > pc {
                break;
            }
            v.swap(pb, pc as usize);
            pb += 1;
            pc -= 1;
        }
        let pn = a + n;
        let d1 = (pa - a).min(pb - pa);
        swapn(v, a, pb - d1, d1);
        let d1 = ((pd - pc) as usize).min(pn - pd as usize - 1);
        swapn(v, pb, pn - d1, d1);
        let d1 = pb - pa;
        let d2 = (pd - pc) as usize;
        if d1 <= d2 {
            if d1 > 1 {
                sort(&mut v[a..a + d1], cmp);
            }
            if d2 > 1 {
                a = pn - d2;
                n = d2;
                continue;
            }
        } else {
            if d2 > 1 {
                sort(&mut v[pn - d2..pn], cmp);
            }
            if d1 > 1 {
                n = d1;
                continue;
            }
        }
        return;
    }
}

/// Slices this short are sorted by comparisons (`radix_sort_by`).
const RADIX_SMALL: usize = 64;

/// Sort `v` as `cmp` orders it, for a `cmp` that orders elements by `key` first: 16 bytes as
/// two words, the first the more significant, each word's high byte first (`u64::from_be_bytes`
/// of the bytes). A byte at a time from the most significant (MSD radix, in place: American
/// flag sort), the bytes every element shares passed over; slices of a few elements, and
/// elements whose 16 bytes are all equal, by `cmp`. Not stable. Elements already in order
/// (an index's entries read in the table's order) are left as they are after one pass, as a
/// comparison sort's pattern checks leave them.
///
/// A comparison sort of a million elements makes about 20 comparisons and moves an element,
/// with a branch mispredicted half the time (pdqsort's partitions: 370-480 instructions an
/// element sorting an index build's entries); this reads each element about twice per byte
/// that still splits its slice, two or three bytes for a million keys.
pub fn radix_sort_by<T>(v: &mut [T], key: impl Fn(&T) -> [u64; 2] + Copy, cmp: impl Fn(&T, &T) -> Ordering + Copy) {
    if v.len() <= RADIX_SMALL {
        v.sort_unstable_by(cmp);
        return;
    }
    if v.is_sorted_by(|a, b| cmp(a, b).is_le()) {
        return;
    }
    // (The bytes before the first one two elements differ in: none of them splits anything.)
    let k0 = key(&v[0]);
    let mut diff = [0u64; 2];
    for x in v.iter() {
        let k = key(x);
        diff[0] |= k[0] ^ k0[0];
        diff[1] |= k[1] ^ k0[1];
    }
    let d = match diff {
        [0, 0] => 16,
        [0, w] => 8 + (w.leading_zeros() / 8) as usize,
        [w, _] => (w.leading_zeros() / 8) as usize,
    };
    flag_sort(v, d, key, cmp);
}

/// `radix_sort_by` from byte `d` of the key on (the bytes before it equal in all of `v`).
fn flag_sort<T>(v: &mut [T], mut d: usize, key: impl Fn(&T) -> [u64; 2] + Copy, cmp: impl Fn(&T, &T) -> Ordering + Copy) {
    let n = v.len();
    let byte = |x: &T, d: usize| (key(x)[d >> 3] >> (56 - 8 * (d & 7))) as u8 as usize;
    let mut count = [0usize; 256];
    loop {
        if n <= RADIX_SMALL || d >= 16 {
            v.sort_unstable_by(cmp);
            return;
        }
        count.fill(0);
        for x in v.iter() {
            count[byte(x, d)] += 1;
        }
        if count[byte(&v[0], d)] < n {
            break;
        }
        d += 1;
    }
    // (Each bucket's next place and its end; each swap puts an element in its bucket for good.)
    let (mut next, mut end) = ([0usize; 256], [0usize; 256]);
    let mut at = 0;
    for b in 0..256 {
        next[b] = at;
        at += count[b];
        end[b] = at;
    }
    for b in 0..256 {
        while next[b] < end[b] {
            let i = next[b];
            let mut x = byte(&v[i], d);
            while x != b {
                v.swap(i, next[x]);
                next[x] += 1;
                x = byte(&v[i], d);
            }
            next[b] += 1;
        }
    }
    let mut at = 0;
    for &c in &count {
        if c > 1 {
            flag_sort(&mut v[at..at + c], d + 1, key, cmp);
        }
        at += c;
    }
}

#[cfg(test)]
mod tests {
    use super::pg_qsort;

    #[test]
    fn sorts() {
        let mut seed = 12345u64;
        for len in [0usize, 1, 5, 7, 8, 39, 41, 200, 1000] {
            let mut v: Vec<u32> = (0..len)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (seed >> 33) as u32 % 50
                })
                .collect();
            let mut w = v.clone();
            pg_qsort(&mut v, |a, b| a.cmp(b));
            w.sort();
            assert_eq!(v, w);
        }
    }

    /// Keys sharing leading bytes, equal in all 16 bytes with the rest deciding, duplicates,
    /// as many as a few hundred thousand: the order a comparison sort gives.
    #[test]
    fn radix_sorts_as_the_comparison() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for (len, mask) in [(0usize, !0u64), (65, !0), (3000, 0xff), (3000, 0xffff_0000_00ff), (200_000, 0xff_ffff), (200_000, !0), (5000, 0)] {
            // (Two words, then a tiebreak the key doesn't hold.)
            let mut v: Vec<([u64; 2], u32)> = (0..len)
                .map(|_| {
                    let hi = 0x8000_0001_0000_0000 | (next() & mask);
                    let lo = if next() % 4 == 0 { 0 } else { next() & mask };
                    ([hi, lo], (next() % 5) as u32)
                })
                .collect();
            let mut w = v.clone();
            w.sort();
            super::radix_sort_by(&mut v, |e| e.0, |a, b| a.cmp(b));
            assert_eq!(v, w, "{len} {mask:x}");
        }
    }
}

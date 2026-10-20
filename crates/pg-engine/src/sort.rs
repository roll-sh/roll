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
}

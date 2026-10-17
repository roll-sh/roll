//! musl 1.2.5's `qsort()` (smoothsort, by Valentin Ochs, MIT licensed),
//! ported element-wise.
//!
//! TRE sorts the items of a negated bracket expression with `qsort()` keyed on
//! the range start only, and its complement construction is order-sensitive
//! when two items share a start (see `parse::Parser::bracket`). `qsort()` is
//! not stable, so to get the same complement as the reference build (Redis on
//! Alpine/musl, aarch64) the exact permutation matters. Shift and
//! count-trailing-zeros edge cases follow aarch64 semantics (`ctz(0) == 64`,
//! shift amounts taken mod 64), which is what musl's C compiles to there.

use std::cmp::Ordering;

const BITS: u32 = usize::BITS;

fn ntz(x: usize) -> u32 {
    x.trailing_zeros()
}

fn pntz(p: &[usize; 2]) -> u32 {
    let r = ntz(p[0].wrapping_sub(1));
    if r != 0 {
        return r;
    }
    let r = BITS + ntz(p[1]);
    if r != BITS { r } else { 0 }
}

fn shl(p: &mut [usize; 2], mut n: u32) {
    if n >= BITS {
        n -= BITS;
        p[1] = p[0];
        p[0] = 0;
    }
    p[1] = p[1].wrapping_shl(n);
    p[1] |= p[0].wrapping_shr(BITS.wrapping_sub(n));
    p[0] = p[0].wrapping_shl(n);
}

fn shr(p: &mut [usize; 2], mut n: u32) {
    if n >= BITS {
        n -= BITS;
        p[0] = p[1];
        p[1] = 0;
    }
    p[0] = p[0].wrapping_shr(n);
    p[0] |= p[1].wrapping_shl(BITS.wrapping_sub(n));
    p[1] = p[1].wrapping_shr(n);
}

struct Sorter<'a, T, F> {
    v: &'a mut [T],
    cmp: F,
    lp: Vec<usize>,
}

impl<T: Copy, F: FnMut(&T, &T) -> Ordering> Sorter<'_, T, F> {
    fn c(&mut self, a: usize, b: usize) -> Ordering {
        (self.cmp)(&self.v[a], &self.v[b])
    }

    /// `cycle()`: rotate the values at `ar[0..n]` one step to the left.
    fn cycle(&mut self, ar: &[usize]) {
        if ar.len() < 2 {
            return;
        }
        let tmp = self.v[ar[0]];
        for i in 0..ar.len() - 1 {
            self.v[ar[i]] = self.v[ar[i + 1]];
        }
        self.v[ar[ar.len() - 1]] = tmp;
    }

    fn sift(&mut self, mut head: usize, mut pshift: i32) {
        let mut ar = vec![head];
        while pshift > 1 {
            let rt = head - 1;
            let lf = head - 1 - self.lp[(pshift - 2) as usize];
            if self.c(ar[0], lf).is_ge() && self.c(ar[0], rt).is_ge() {
                break;
            }
            if self.c(lf, rt).is_ge() {
                ar.push(lf);
                head = lf;
                pshift -= 1;
            } else {
                ar.push(rt);
                head = rt;
                pshift -= 2;
            }
        }
        self.cycle(&ar);
    }

    fn trinkle(&mut self, mut head: usize, pp: [usize; 2], mut pshift: i32, mut trusty: bool) {
        let mut p = pp;
        let mut ar = vec![head];
        while p[0] != 1 || p[1] != 0 {
            let stepson = head - self.lp[pshift as usize];
            if self.c(stepson, ar[0]).is_le() {
                break;
            }
            if !trusty && pshift > 1 {
                let rt = head - 1;
                let lf = head - 1 - self.lp[(pshift - 2) as usize];
                if self.c(rt, stepson).is_ge() || self.c(lf, stepson).is_ge() {
                    break;
                }
            }
            ar.push(stepson);
            head = stepson;
            let trail = pntz(&p);
            shr(&mut p, trail);
            pshift += trail as i32;
            trusty = false;
        }
        if !trusty {
            self.cycle(&ar);
            self.sift(head, pshift);
        }
    }
}

/// `qsort(v, n, sizeof *v, cmp)` with musl's exact element permutation.
pub(crate) fn qsort<T: Copy, F: FnMut(&T, &T) -> Ordering>(v: &mut [T], cmp: F) {
    let size = v.len();
    if size == 0 {
        return;
    }
    let mut lp = vec![1usize, 1];
    loop {
        let i = lp.len();
        let next = lp[i - 2] + lp[i - 1] + 1;
        lp.push(next);
        if next >= size {
            break;
        }
    }
    // musl keeps a fixed 96-entry table; extend so any index the algorithm
    // could form stays in bounds.
    while lp.len() < 96 {
        let i = lp.len();
        lp.push(lp[i - 2].saturating_add(lp[i - 1]).saturating_add(1));
    }
    let mut s = Sorter { v, cmp, lp };
    let mut head = 0usize;
    let high = size - 1;
    let mut p: [usize; 2] = [1, 0];
    let mut pshift: i32 = 1;

    while head < high {
        if (p[0] & 3) == 3 {
            s.sift(head, pshift);
            shr(&mut p, 2);
            pshift += 2;
        } else {
            if s.lp[(pshift - 1) as usize] >= high - head {
                s.trinkle(head, p, pshift, false);
            } else {
                s.sift(head, pshift);
            }
            if pshift == 1 {
                shl(&mut p, 1);
                pshift = 0;
            } else {
                shl(&mut p, (pshift - 1) as u32);
                pshift = 1;
            }
        }
        p[0] |= 1;
        head += 1;
    }

    s.trinkle(head, p, pshift, false);

    while pshift != 1 || p[0] != 1 || p[1] != 0 {
        if pshift <= 1 {
            let trail = pntz(&p);
            shr(&mut p, trail);
            pshift += trail as i32;
        } else {
            shl(&mut p, 2);
            pshift -= 2;
            p[0] ^= 7;
            shr(&mut p, 1);
            s.trinkle(head - s.lp[pshift as usize] - 1, p, pshift + 1, true);
            shl(&mut p, 1);
            p[0] |= 1;
            s.trinkle(head - 1, p, pshift, true);
        }
        head -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::qsort;

    #[test]
    fn sorts() {
        let mut seed = 0x1234_5678u64;
        for n in 0..200usize {
            let mut v: Vec<u32> = (0..n)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (seed >> 40) as u32 % 50
                })
                .collect();
            let mut want = v.clone();
            want.sort();
            qsort(&mut v, |a, b| a.cmp(b));
            assert_eq!(v, want, "n={n}");
        }
    }
}

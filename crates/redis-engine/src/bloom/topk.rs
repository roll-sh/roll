//! Top-K (RedisBloom's HeavyKeeper `TopK`): `depth` rows of `width` fingerprint counters that
//! decay on collisions, and a min-heap of the `k` heaviest items.

use super::hash::murmur2;
use super::sb::MAX_ALLOC;

const GA: u32 = 1919;
pub const DECAY_TABLE: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bucket {
    pub fp: u32,
    pub count: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeapBucket {
    pub fp: u32,
    pub itemlen: u32,
    /// The item's bytes (`None` for an unused slot).
    pub item: Option<Box<[u8]>>,
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopK {
    pub k: u32,
    pub width: u32,
    pub depth: u32,
    pub decay: f64,
    pub data: Vec<Bucket>,
    pub heap: Vec<HeapBucket>,
    pub lookup: Box<[f64; DECAY_TABLE]>,
}

/// musl's `rand()`: a process-wide LCG seeded like Redis' `srand(time ^ pid)`.
pub mod crand {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEED: AtomicU64 = AtomicU64::new(0);
    pub const RAND_MAX: i32 = 0x7fff_ffff;

    pub fn srand(s: u32) {
        SEED.store((s as u64).wrapping_sub(1), Ordering::Relaxed);
    }

    pub fn rand() -> i32 {
        let next = SEED.load(Ordering::Relaxed).wrapping_mul(6364136223846793005).wrapping_add(1);
        SEED.store(next, Ordering::Relaxed);
        (next >> 33) as i32
    }
}

fn heapify_down(heap: &mut [HeapBucket], start: usize) {
    let len = heap.len();
    let mut start = start;
    let mut child = start;
    if len < 2 || (len - 2) / 2 < child {
        return;
    }
    child = 2 * child + 1;
    if child + 1 < len && heap[child].count > heap[child + 1].count {
        child += 1;
    }
    if heap[child].count > heap[start].count {
        return;
    }
    let top = std::mem::take(&mut heap[start]);
    loop {
        heap[start] = std::mem::take(&mut heap[child]);
        start = child;
        if (len - 2) / 2 < child {
            break;
        }
        child = 2 * child + 1;
        if child + 1 < len && heap[child].count > heap[child + 1].count {
            child += 1;
        }
        if heap[child].count >= top.count {
            break;
        }
    }
    heap[start] = top;
}

impl TopK {
    /// `TopK_Create`; `None` when the counters can't be allocated.
    pub fn new(k: u32, width: u32, depth: u32, decay: f64) -> Option<TopK> {
        let cells = width as u64 * depth as u64;
        if cells * 8 > MAX_ALLOC || k as u64 * 24 > MAX_ALLOC {
            return None;
        }
        let mut lookup = Box::new([0.0; DECAY_TABLE]);
        for (i, v) in lookup.iter_mut().enumerate() {
            *v = crate::musl_math::pow(decay, i as f64);
        }
        Some(TopK {
            k,
            width,
            depth,
            decay,
            data: vec![Bucket::default(); cells as usize],
            heap: vec![HeapBucket::default(); k as usize],
            lookup,
        })
    }

    /// The decay table as RDB loading rebuilds it (running products).
    pub fn rebuild_lookup(&mut self) {
        for i in 0..DECAY_TABLE {
            self.lookup[i] = if i == 0 { 1.0 } else { self.lookup[i - 1] * self.decay };
        }
    }

    fn find(&self, item: &[u8]) -> Option<usize> {
        let fp = murmur2(item, GA);
        (0..self.heap.len()).rev().find(|&i| {
            let h = &self.heap[i];
            // `memcmp(NULL, item, 0)`: a slot loaded without its item (an empty one, `""` once
            // restored) matches the empty item.
            h.fp == fp && h.itemlen as usize == item.len() && h.item.as_deref().unwrap_or_default() == item
        })
    }

    /// `TopK_Add`: the item expelled from the heap, if any.
    pub fn add(&mut self, item: &[u8], increment: u32) -> Option<Box<[u8]>> {
        let fp = murmur2(item, GA);
        let heap_min = self.heap[0].count;
        let mut max_count = 0u32;
        for row in 0..self.depth {
            let loc = murmur2(item, row) % self.width;
            let b = &mut self.data[row.wrapping_mul(self.width).wrapping_add(loc) as usize];
            if b.count == 0 {
                b.fp = fp;
                b.count = increment;
                max_count = max_count.max(b.count);
            } else if b.fp == fp {
                b.count = b.count.wrapping_add(increment);
                max_count = max_count.max(b.count);
            } else {
                let mut local = increment;
                while local > 0 {
                    let c = b.count as usize;
                    let decay = if c < DECAY_TABLE {
                        self.lookup[c]
                    } else {
                        crate::musl_math::pow(self.lookup[DECAY_TABLE - 1], (c / (DECAY_TABLE - 1)) as f64)
                            * self.lookup[c % (DECAY_TABLE - 1)]
                    };
                    let chance = crand::rand() as f64 / crand::RAND_MAX as f64;
                    if chance < decay {
                        b.count -= 1;
                        if b.count == 0 {
                            b.fp = fp;
                            b.count = local;
                            max_count = max_count.max(b.count);
                            break;
                        }
                    }
                    local -= 1;
                }
            }
        }
        if max_count < heap_min {
            return None;
        }
        if let Some(i) = self.find(item) {
            self.heap[i].count = max_count;
            heapify_down(&mut self.heap, i);
            return None;
        }
        let expelled = self.heap[0].item.take();
        self.heap[0] = HeapBucket { fp, itemlen: item.len() as u32, item: Some(item.into()), count: max_count };
        heapify_down(&mut self.heap, 0);
        expelled
    }

    pub fn query(&self, item: &[u8]) -> bool {
        self.find(item).is_some()
    }

    /// `TopK_Count`.
    pub fn count(&self, item: &[u8]) -> u32 {
        let fp = murmur2(item, GA);
        let heap_min = self.heap[0].count;
        let in_heap = self.find(item).is_some();
        let mut res = 0;
        for row in 0..self.depth {
            let loc = murmur2(item, row) % self.width;
            let b = &self.data[row.wrapping_mul(self.width).wrapping_add(loc) as usize];
            if b.fp == fp && (!in_heap || b.count >= heap_min) {
                res = res.max(b.count);
            }
        }
        res
    }

    /// `TopK_List`: the heap sorted by count, highest first (musl's `qsort`).
    pub fn list(&self) -> Vec<HeapBucket> {
        let mut out = self.heap.clone();
        smoothsort(&mut out, |a, b| b.count.cmp(&a.count) as i32);
        out
    }

    /// `TopKMemUsage`.
    pub fn mem_usage(&self) -> u64 {
        2088 + 8 * self.width as u64 * self.depth as u64 + 24 * self.k as u64
    }
}

/// musl's `qsort` (smoothsort), over a slice: ties keep musl's order.
pub fn smoothsort<T>(v: &mut [T], cmp: impl Fn(&T, &T) -> i32) {
    let n = v.len();
    if n == 0 {
        return;
    }
    // Leonardo numbers in elements.
    let mut lp = vec![1usize, 1];
    while {
        let next = lp[lp.len() - 2] + lp[lp.len() - 1] + 1;
        lp.push(next);
        next < n
    } {}
    let mut p = [1usize, 0];
    let mut pshift: i32 = 1;
    let mut head = 0usize;
    let high = n - 1;
    let s = Sorter { cmp: &cmp, lp: &lp };
    while head < high {
        if p[0] & 3 == 3 {
            s.sift(v, head, pshift);
            shr(&mut p, 2);
            pshift += 2;
        } else {
            if lp[(pshift - 1) as usize] >= high - head {
                s.trinkle(v, head, p, pshift, false);
            } else {
                s.sift(v, head, pshift);
            }
            if pshift == 1 {
                shl(&mut p, 1);
                pshift = 0;
            } else {
                shl(&mut p, pshift - 1);
                pshift = 1;
            }
        }
        p[0] |= 1;
        head += 1;
    }
    s.trinkle(v, head, p, pshift, false);
    while pshift != 1 || p[0] != 1 || p[1] != 0 {
        if pshift <= 1 {
            let trail = pntz(p);
            shr(&mut p, trail);
            pshift += trail;
        } else {
            shl(&mut p, 2);
            pshift -= 2;
            p[0] ^= 7;
            shr(&mut p, 1);
            s.trinkle(v, head - lp[pshift as usize] - 1, p, pshift + 1, true);
            shl(&mut p, 1);
            p[0] |= 1;
            s.trinkle(v, head - 1, p, pshift, true);
        }
        head -= 1;
    }
}

const BITS: i32 = usize::BITS as i32;

/// aarch64's count of trailing zeros (64 for zero).
fn ntz(x: usize) -> i32 {
    x.trailing_zeros() as i32
}

fn pntz(p: [usize; 2]) -> i32 {
    let r = ntz(p[0].wrapping_sub(1));
    if r != 0 {
        return r;
    }
    let r = BITS + ntz(p[1]);
    if r != BITS { r } else { 0 }
}

fn shl(p: &mut [usize; 2], n: i32) {
    let mut n = n;
    if n >= BITS {
        n -= BITS;
        p[1] = p[0];
        p[0] = 0;
    }
    p[1] = p[1].wrapping_shl(n as u32);
    p[1] |= p[0].wrapping_shr((BITS - n) as u32);
    p[0] = p[0].wrapping_shl(n as u32);
}

fn shr(p: &mut [usize; 2], n: i32) {
    let mut n = n;
    if n >= BITS {
        n -= BITS;
        p[0] = p[1];
        p[1] = 0;
    }
    p[0] = p[0].wrapping_shr(n as u32);
    p[0] |= p[1].wrapping_shl((BITS - n) as u32);
    p[1] = p[1].wrapping_shr(n as u32);
}

struct Sorter<'a, F> {
    cmp: &'a F,
    lp: &'a [usize],
}

impl<F> Sorter<'_, F> {
    /// `cycle`: rotate the values along the index path left by one.
    fn cycle<T>(v: &mut [T], ar: &[usize]) {
        for w in ar.windows(2) {
            v.swap(w[0], w[1]);
        }
    }

    fn sift<T>(&self, v: &mut [T], head: usize, pshift: i32)
    where
        F: Fn(&T, &T) -> i32,
    {
        let mut ar = vec![head];
        let mut head = head;
        let mut pshift = pshift;
        while pshift > 1 {
            let rt = head - 1;
            let lf = head - 1 - self.lp[(pshift - 2) as usize];
            if (self.cmp)(&v[ar[0]], &v[lf]) >= 0 && (self.cmp)(&v[ar[0]], &v[rt]) >= 0 {
                break;
            }
            if (self.cmp)(&v[lf], &v[rt]) >= 0 {
                ar.push(lf);
                head = lf;
                pshift -= 1;
            } else {
                ar.push(rt);
                head = rt;
                pshift -= 2;
            }
        }
        Self::cycle(v, &ar);
    }

    fn trinkle<T>(&self, v: &mut [T], head: usize, pp: [usize; 2], pshift: i32, trusty: bool)
    where
        F: Fn(&T, &T) -> i32,
    {
        let mut p = pp;
        let mut ar = vec![head];
        let mut head = head;
        let mut pshift = pshift;
        let mut trusty = trusty;
        while p[0] != 1 || p[1] != 0 {
            let stepson = head - self.lp[pshift as usize];
            if (self.cmp)(&v[stepson], &v[ar[0]]) <= 0 {
                break;
            }
            if !trusty && pshift > 1 {
                let rt = head - 1;
                let lf = head - 1 - self.lp[(pshift - 2) as usize];
                if (self.cmp)(&v[rt], &v[stepson]) >= 0 || (self.cmp)(&v[lf], &v[stepson]) >= 0 {
                    break;
                }
            }
            ar.push(stepson);
            head = stepson;
            let trail = pntz(p);
            shr(&mut p, trail);
            pshift += trail;
            trusty = false;
        }
        if !trusty {
            Self::cycle(v, &ar);
            self.sift(v, head, pshift);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothsort_sorts() {
        for n in 0..200usize {
            let mut v: Vec<u64> = (0..n as u64).map(|i| (i * 7919) % 97).collect();
            let mut want = v.clone();
            want.sort();
            smoothsort(&mut v, |a, b| a.cmp(b) as i32);
            assert_eq!(v, want, "n={n}");
        }
    }

    #[test]
    fn keeps_heavy_hitters() {
        let mut t = TopK::new(3, 50, 4, 0.9).unwrap();
        for (item, n) in [("a", 10), ("b", 5), ("c", 3), ("d", 1)] {
            for _ in 0..n {
                t.add(item.as_bytes(), 1);
            }
        }
        let names: Vec<_> = t.list().iter().filter(|h| h.count > 0).map(|h| h.item.clone().unwrap()).collect();
        assert_eq!(names, vec![b"a"[..].into(), b"b"[..].into(), b"c"[..].into()]);
        assert!(t.query(b"a"));
        assert!(!t.query(b"d"));
        assert_eq!(t.count(b"a"), 10);
    }
}

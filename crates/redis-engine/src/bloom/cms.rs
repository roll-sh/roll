//! Count-min sketches (RedisBloom's `CMSketch`): `depth` rows of `width` 32-bit counters.

use super::hash::murmur2;
use super::sb::MAX_ALLOC;

#[derive(Clone, Debug, PartialEq)]
pub struct Cms {
    pub width: u64,
    pub depth: u64,
    pub counter: u64,
    pub array: Vec<u32>,
}

impl Cms {
    /// `NewCMSketch`; `None` when the counters can't be allocated.
    pub fn new(width: u64, depth: u64) -> Option<Cms> {
        let cells = width.checked_mul(depth)?;
        let bytes = cells.checked_mul(4)?;
        if bytes > MAX_ALLOC {
            return None;
        }
        let mut array = Vec::new();
        array.try_reserve_exact(cells as usize).ok()?;
        array.resize(cells as usize, 0);
        Some(Cms { width, depth, counter: 0, array })
    }

    /// `CMS_DimFromProb`: width and depth for an overestimation error and a probability
    /// (single-precision logarithms, as the module computes them).
    pub fn dims(error: f64, prob: f64) -> (u64, u64) {
        let width = (2.0 / error).ceil() as u64;
        let depth = (libm::log10f(prob as f32) / libm::log10f(0.5)).ceil() as u64;
        (width, depth)
    }

    fn loc(&self, item: &[u8], row: u64) -> usize {
        let h = murmur2(item, row as u32) as u64;
        (h % self.width + row * self.width) as usize
    }

    /// `CMS_IncrBy`: the item's minimum count afterwards (`u32::MAX` once saturated).
    pub fn incr_by(&mut self, item: &[u8], value: u64) -> u64 {
        let mut min = u64::MAX;
        for row in 0..self.depth {
            let at = self.loc(item, row);
            let v = (self.array[at] as u64).wrapping_add(value) as u32;
            self.array[at] = if (v as u64) < value { u32::MAX } else { v };
            min = min.min(self.array[at] as u64);
        }
        self.counter = self.counter.wrapping_add(value);
        min
    }

    pub fn query(&self, item: &[u8]) -> u64 {
        (0..self.depth).map(|row| self.array[self.loc(item, row)] as u64).min().unwrap_or(u64::MAX)
    }

    /// `CMS_Merge` into `self` from same-sized sketches with weights; `false` (and no change)
    /// on overflow. `None` in `srcs` stands for `self`.
    pub fn merge(&mut self, srcs: &[(Option<&Cms>, i64)]) -> bool {
        // `__builtin_mul_overflow`/`__builtin_add_overflow` into int64_t: exact products and
        // sums that must fit.
        fn acc(total: i64, v: u64, w: i64) -> Option<i64> {
            let m = i64::try_from(v as i128 * w as i128).ok()?;
            total.checked_add(m)
        }
        let cell = |s: Option<&Cms>, i: usize, me: &Cms| s.map_or(me.array[i], |c| c.array[i]) as u64;
        let counter = |s: Option<&Cms>, me: &Cms| s.map_or(me.counter, |c| c.counter);
        let mut out = vec![0u32; self.array.len()];
        for (i, slot) in out.iter_mut().enumerate() {
            let mut total = 0i64;
            for &(s, w) in srcs {
                let Some(t) = acc(total, cell(s, i, self), w) else { return false };
                total = t;
            }
            if total < 0 || total > u32::MAX as i64 {
                return false;
            }
            *slot = total as u32;
        }
        let mut count = 0i64;
        for &(s, w) in srcs {
            let Some(t) = acc(count, counter(s, self), w) else { return false };
            count = t;
        }
        if count < 0 {
            return false;
        }
        self.array = out;
        self.counter = count as u64;
        true
    }

    /// `CMSMemUsage`.
    pub fn mem_usage(&self) -> u64 {
        32 + 4 * self.width * self.depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        let mut c = Cms::new(100, 5).unwrap();
        assert_eq!(c.incr_by(b"a", 3), 3);
        assert_eq!(c.incr_by(b"a", 2), 5);
        assert_eq!(c.query(b"a"), 5);
        assert_eq!(c.query(b"zz"), 0);
        assert_eq!(c.counter, 5);
        assert_eq!(Cms::dims(0.001, 0.01), (2000, 7));
    }

    #[test]
    fn merges_with_weights() {
        let mut a = Cms::new(10, 2).unwrap();
        let mut b = Cms::new(10, 2).unwrap();
        a.incr_by(b"x", 2);
        b.incr_by(b"x", 5);
        let mut d = Cms::new(10, 2).unwrap();
        assert!(d.merge(&[(Some(&a), 1), (Some(&b), 3)]));
        assert_eq!(d.query(b"x"), 17);
        assert_eq!(d.counter, 17);
        assert!(!d.merge(&[(Some(&a), -1)]));
        assert_eq!(d.query(b"x"), 17);
    }
}

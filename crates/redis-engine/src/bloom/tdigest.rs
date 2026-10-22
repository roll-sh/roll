//! t-digests (the `t-digest-c` histogram RedisBloom links): centroids (mean, weight) buffered
//! unmerged and merged by compression, with quantile/CDF/rank estimates.

use super::sb::MAX_ALLOC;

use std::f64::consts::PI;
const INSORT_THRESHOLD: i32 = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct TDigest {
    pub compression: f64,
    pub min: f64,
    pub max: f64,
    pub cap: i32,
    pub merged_nodes: i32,
    pub unmerged_nodes: i32,
    pub total_compressions: i64,
    pub merged_weight: i64,
    pub unmerged_weight: i64,
    /// The used node slots (merged then unmerged); the rest of `cap` is zeros.
    pub means: Vec<f64>,
    pub weights: Vec<i64>,
}

/// An add or merge that would overflow.
#[derive(Debug)]
pub struct Overflow;

fn lt(a: f64, b: f64) -> bool {
    a < b
}

fn swap(m: &mut [f64], w: &mut [i64], i: usize, j: usize) {
    m.swap(i, j);
    w.swap(i, j);
}

fn insertion_sort(m: &mut [f64], w: &mut [i64], lo: i32, hi: i32) {
    for i in lo + 1..=hi {
        let (mv, wv) = (m[i as usize], w[i as usize]);
        let mut j = i - 1;
        while j >= lo && lt(mv, m[j as usize]) {
            m[(j + 1) as usize] = m[j as usize];
            w[(j + 1) as usize] = w[j as usize];
            j -= 1;
        }
        m[(j + 1) as usize] = mv;
        w[(j + 1) as usize] = wv;
    }
}

fn sift_down(m: &mut [f64], w: &mut [i64], lo: i32, mut i: i32, n: i32) {
    while i < n / 2 {
        let mut child = 2 * i + 1;
        if child + 1 < n && lt(m[(lo + child) as usize], m[(lo + child + 1) as usize]) {
            child += 1;
        }
        if !lt(m[(lo + i) as usize], m[(lo + child) as usize]) {
            break;
        }
        swap(m, w, (lo + i) as usize, (lo + child) as usize);
        i = child;
    }
}

fn heap_sort(m: &mut [f64], w: &mut [i64], lo: i32, hi: i32) {
    let n = hi - lo + 1;
    let mut i = n / 2 - 1;
    while i >= 0 {
        sift_down(m, w, lo, i, n);
        i -= 1;
    }
    let mut end = n - 1;
    while end > 0 {
        swap(m, w, lo as usize, (lo + end) as usize);
        sift_down(m, w, lo, 0, end);
        end -= 1;
    }
}

fn median3(m: &mut [f64], w: &mut [i64], lo: i32, mid: i32, hi: i32) -> f64 {
    let (lo, mid, hi) = (lo as usize, mid as usize, hi as usize);
    if lt(m[mid], m[lo]) {
        swap(m, w, lo, mid);
    }
    if lt(m[hi], m[lo]) {
        swap(m, w, lo, hi);
    }
    if lt(m[hi], m[mid]) {
        swap(m, w, mid, hi);
    }
    m[mid]
}

/// `td_introsort`: 3-way quicksort, heapsort past the depth limit, insertion sort for small
/// ranges (the order of equal means is the library's).
fn introsort(m: &mut [f64], w: &mut [i64], mut lo: i32, mut hi: i32, mut depth: i32) {
    while hi - lo > INSORT_THRESHOLD {
        if depth == 0 {
            heap_sort(m, w, lo, hi);
            return;
        }
        depth -= 1;
        let mid = lo + (hi - lo) / 2;
        let pivot = median3(m, w, lo, mid, hi);
        let (mut l, mut i, mut g) = (lo, lo, hi);
        while i <= g {
            let v = m[i as usize];
            if lt(v, pivot) {
                swap(m, w, i as usize, l as usize);
                l += 1;
                i += 1;
            } else if lt(pivot, v) {
                swap(m, w, i as usize, g as usize);
                g -= 1;
            } else {
                i += 1;
            }
        }
        let (left, right) = (l - lo, hi - g);
        if left < right {
            if left > 1 {
                introsort(m, w, lo, l - 1, depth);
            }
            lo = g + 1;
        } else {
            if right > 1 {
                introsort(m, w, g + 1, hi, depth);
            }
            hi = l - 1;
        }
    }
    insertion_sort(m, w, lo, hi);
}

fn sort(m: &mut [f64], w: &mut [i64], lo: i32, hi: i32) {
    if lo >= hi {
        return;
    }
    let mut depth = 0;
    let mut t = hi - lo + 1;
    while t > 1 {
        depth += 2;
        t >>= 1;
    }
    introsort(m, w, lo, hi, depth);
}

fn weighted_average_sorted(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    let x = (x1 * w1 + x2 * w2) / (w1 + w2);
    let lo = if x < x2 { x } else { x2 };
    if x1 > lo { x1 } else { lo }
}

fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    if x1 <= x2 { weighted_average_sorted(x1, w1, x2, w2) } else { weighted_average_sorted(x2, w2, x1, w1) }
}

fn max(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

fn min(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

fn overflows(unmerged: f64, total: f64) -> bool {
    unmerged == f64::INFINITY || total == f64::INFINITY || 2.0 * PI * total * crate::musl_math::log(total) == f64::INFINITY
}

/// `a + b` fits a `long long` (`_tdigest_long_long_add_safe`).
fn add_safe(a: i64, b: i64) -> bool {
    if b < 0 { a >= i64::MAX.wrapping_sub(b) } else { a <= i64::MAX - b }
}

impl TDigest {
    /// `td_init`: `None` for a compression it rejects or can't allocate for.
    pub fn new(compression: f64) -> Option<TDigest> {
        if !compression.is_finite() || compression <= 0.0 || compression > i32::MAX as f64 {
            return None;
        }
        let cap = 6u64 * compression as u64 + 10;
        if cap > i32::MAX as u64 || cap * 8 > MAX_ALLOC {
            return None;
        }
        let mut t = TDigest {
            compression,
            min: 0.0,
            max: 0.0,
            cap: cap as i32,
            merged_nodes: 0,
            unmerged_nodes: 0,
            total_compressions: 0,
            merged_weight: 0,
            unmerged_weight: 0,
            means: Vec::new(),
            weights: Vec::new(),
        };
        t.reset();
        Some(t)
    }

    /// `td_reset` (the node arrays keep their old contents past the counts).
    pub fn reset(&mut self) {
        self.min = f64::MAX;
        self.max = -self.min;
        self.merged_nodes = 0;
        self.merged_weight = 0;
        self.unmerged_nodes = 0;
        self.unmerged_weight = 0;
        self.total_compressions = 0;
        self.means.clear();
        self.weights.clear();
    }

    fn nodes(&self) -> i32 {
        self.merged_nodes + self.unmerged_nodes
    }

    pub fn size(&self) -> i64 {
        self.merged_weight.wrapping_add(self.unmerged_weight)
    }

    /// `td_add`.
    pub fn add(&mut self, mean: f64, weight: i64) -> Result<(), Overflow> {
        if !mean.is_finite() {
            return Err(Overflow);
        }
        if self.nodes() >= self.cap - 1 {
            self.compress()?;
        }
        let pos = self.nodes();
        if pos >= self.cap {
            return Err(Overflow);
        }
        if !add_safe(self.unmerged_weight, weight) {
            return Err(Overflow);
        }
        let new_unmerged = self.unmerged_weight + weight;
        if !add_safe(new_unmerged, self.merged_weight) {
            return Err(Overflow);
        }
        let new_total = new_unmerged + self.merged_weight;
        if overflows(new_unmerged as f64, new_total as f64) {
            return Err(Overflow);
        }
        if mean < self.min {
            self.min = mean;
        }
        if mean > self.max {
            self.max = mean;
        }
        self.means.truncate(pos as usize);
        self.weights.truncate(pos as usize);
        self.means.push(mean);
        self.weights.push(weight);
        self.unmerged_nodes += 1;
        self.unmerged_weight = new_unmerged;
        Ok(())
    }

    /// `td_compress`.
    pub fn compress(&mut self) -> Result<(), Overflow> {
        if self.unmerged_nodes == 0 {
            return Ok(());
        }
        let n = self.nodes();
        sort(&mut self.means, &mut self.weights, 0, n - 1);
        let total = self.merged_weight as f64 + self.unmerged_weight as f64;
        if overflows(self.unmerged_weight as f64, total) {
            return Err(Overflow);
        }
        if total <= 1.0 {
            self.merged_nodes += self.unmerged_nodes;
            self.merged_weight = total as i64;
            self.unmerged_nodes = 0;
            self.unmerged_weight = 0;
            self.total_compressions += 1;
            return Ok(());
        }
        let denom = 2.0 * PI * total * crate::musl_math::log(total);
        if denom == f64::INFINITY {
            return Err(Overflow);
        }
        let normalizer = self.compression / denom;
        if normalizer == f64::INFINITY {
            return Err(Overflow);
        }
        let (m, w) = (&mut self.means, &mut self.weights);
        let mut cur = 0usize;
        let mut so_far = 0.0;
        for i in 1..n as usize {
            let proposed = w[cur] as f64 + w[i] as f64;
            let z = proposed * normalizer;
            let q0 = so_far / total;
            let q2 = (so_far + proposed) / total;
            if z <= q0 * (1.0 - q0) && z <= q2 * (1.0 - q2) {
                w[cur] = w[cur].wrapping_add(w[i]);
                let delta = m[i] - m[cur];
                m[cur] += (delta * w[i] as f64) / w[cur] as f64;
            } else {
                so_far += w[cur] as f64;
                cur += 1;
                w[cur] = w[i];
                m[cur] = m[i];
            }
        }
        m.truncate(cur + 1);
        w.truncate(cur + 1);
        self.merged_nodes = cur as i32 + 1;
        self.merged_weight = total as i64;
        self.unmerged_nodes = 0;
        self.unmerged_weight = 0;
        self.total_compressions += 1;
        Ok(())
    }

    /// `td_merge`: compress both, then add `from`'s centroids.
    pub fn merge_from(&mut self, from: &mut TDigest) -> Result<(), Overflow> {
        self.compress()?;
        from.compress()?;
        for i in 0..from.nodes() as usize {
            self.add(from.means[i], from.weights[i])?;
        }
        Ok(())
    }

    /// `td_cdf`.
    pub fn cdf(&mut self, val: f64) -> f64 {
        let _ = self.compress();
        if self.merged_nodes == 0 {
            return f64::NAN;
        }
        if val < self.min {
            return 0.0;
        }
        if val > self.max {
            return 1.0;
        }
        if self.merged_nodes == 1 {
            let width = self.max - self.min;
            return if val - self.min <= width { 0.5 } else { (val - self.min) / width };
        }
        let n = self.merged_nodes as usize;
        let (m, w) = (&self.means, &self.weights);
        let mw = self.merged_weight as f64;
        let (left_mean, left_weight) = (m[0], w[0] as f64);
        if val < left_mean {
            let width = left_mean - self.min;
            if width > 0.0 {
                if val == self.min {
                    return 0.5 / mw;
                }
                return (1.0 + (val - self.min) / width * (left_weight / 2.0 - 1.0)) / mw;
            }
            return 0.0;
        }
        let (right_mean, right_weight) = (m[n - 1], w[n - 1] as f64);
        if val > right_mean {
            let width = self.max - right_mean;
            if width > 0.0 {
                if val == self.max {
                    return 1.0 - 0.5 / mw;
                }
                let dq = (1.0 + (self.max - val) / width * (right_weight / 2.0 - 1.0)) / mw;
                return 1.0 - dq;
            }
            return 1.0;
        }
        let mut so_far = 0.0;
        let mut it = 0;
        while it < n - 1 {
            if m[it] == val {
                let mut dw = 0.0;
                while it < n && m[it] == val {
                    dw += w[it] as f64;
                    it += 1;
                }
                return (so_far + dw / 2.0) / self.merged_weight as f64;
            } else if m[it] <= val && val < m[it + 1] {
                let (nw, nw_next) = (w[it] as f64, w[it + 1] as f64);
                let (nm, nm_next) = (m[it], m[it + 1]);
                if nm_next - nm > 0.0 {
                    let mut left_excluded = 0.0;
                    let mut right_excluded = 0.0;
                    if nw == 1.0 {
                        if nw_next == 1.0 {
                            return (so_far + 1.0) / mw;
                        }
                        left_excluded = 0.5;
                    } else if nw_next == 1.0 {
                        right_excluded = 0.5;
                    }
                    let dw = (nw + nw_next) / 2.0;
                    let dw_no_singleton = dw - left_excluded - right_excluded;
                    let base = so_far + nw / 2.0 + left_excluded;
                    return (base + dw_no_singleton * (val - nm) / (nm_next - nm)) / mw;
                }
                let dw = (nw + nw_next) / 2.0;
                return (so_far + dw) / mw;
            } else {
                so_far += w[it] as f64;
            }
            it += 1;
        }
        1.0 - 0.5 / mw
    }

    fn to_index(&self, index: f64, left_weight: f64, total: usize, so_far: &mut f64, pos: &mut usize) -> f64 {
        let (m, w) = (&self.means, &self.weights);
        if left_weight > 1.0 && index < left_weight / 2.0 {
            return self.min + (index - 1.0) / (left_weight / 2.0 - 1.0) * (m[0] - self.min);
        }
        if index > (self.merged_weight - 1) as f64 {
            return self.max;
        }
        let right_weight = w[total - 1] as f64;
        let right_mean = m[total - 1];
        if right_weight > 1.0 && self.merged_weight as f64 - index <= right_weight / 2.0 {
            return self.max
                - (self.merged_weight as f64 - index - 1.0) / (right_weight / 2.0 - 1.0) * (self.max - right_mean);
        }
        while *pos < total - 1 {
            let i = *pos;
            let (nw, nw_next) = (w[i] as f64, w[i + 1] as f64);
            let (nm, nm_next) = (m[i], m[i + 1]);
            let dw = (nw + nw_next) / 2.0;
            if *so_far + dw > index {
                let mut left_unit = 0.0;
                if nw == 1.0 {
                    if index - *so_far < 0.5 {
                        return nm;
                    }
                    left_unit = 0.5;
                }
                let mut right_unit = 0.0;
                if nw_next == 1.0 {
                    if *so_far + dw - index <= 0.5 {
                        return nm_next;
                    }
                    right_unit = 0.5;
                }
                let z1 = index - *so_far - left_unit;
                let z2 = *so_far + dw - index - right_unit;
                return weighted_average(nm, z2, nm_next, z1);
            }
            *so_far += dw;
            *pos += 1;
        }
        let z1 = index - self.merged_weight as f64 - right_weight / 2.0;
        let z2 = right_weight / 2.0 - z1;
        weighted_average(right_mean, z1, self.max, z2)
    }

    /// `td_quantile`.
    // NaN passes these bound checks, as in RedisBloom; `!(0.0..=1.0).contains(&q)` would reject it.
    #[allow(clippy::manual_range_contains)]
    pub fn quantile(&mut self, q: f64) -> f64 {
        let _ = self.compress();
        if q < 0.0 || q > 1.0 || self.merged_nodes == 0 {
            return f64::NAN;
        }
        if self.merged_nodes == 1 {
            return self.means[0];
        }
        let index = q * self.merged_weight as f64;
        if index < 1.0 {
            return self.min;
        }
        let left = self.weights[0] as f64;
        let mut so_far = left / 2.0;
        let mut pos = 0;
        self.to_index(index, left, self.merged_nodes as usize, &mut so_far, &mut pos)
    }

    /// `td_quantiles` over an ascending run of quantiles.
    // NaN passes these bound checks, as in RedisBloom; `!(0.0..=1.0).contains(&q)` would reject it.
    #[allow(clippy::manual_range_contains)]
    pub fn quantiles(&mut self, qs: &[f64]) -> Vec<f64> {
        let _ = self.compress();
        let n = self.merged_nodes;
        if n == 0 {
            return vec![f64::NAN; qs.len()];
        }
        if n == 1 {
            return qs.iter().map(|&q| if q < 0.0 || q > 1.0 { f64::NAN } else { self.means[0] }).collect();
        }
        let left = self.weights[0] as f64;
        let mut so_far = left / 2.0;
        let mut pos = 0;
        qs.iter()
            .map(|&q| self.to_index(q * self.merged_weight as f64, left, n as usize, &mut so_far, &mut pos))
            .collect()
    }

    fn trimmed(&self, leftmost: f64, rightmost: f64) -> f64 {
        let mut done = 0.0;
        let mut sum = 0.0;
        let mut count = 0.0;
        for i in 0..self.merged_nodes as usize {
            let nw = self.weights[i] as f64;
            let mut add = nw;
            add -= min(max(0.0, leftmost - done), add);
            add = min(max(0.0, rightmost - done), add);
            done += nw;
            sum += self.means[i] * add;
            count += add;
            if done >= rightmost {
                break;
            }
        }
        sum / count
    }

    /// `td_trimmed_mean`.
    // NaN passes these bound checks, as in RedisBloom; `!(0.0..=1.0).contains(&q)` would reject it.
    #[allow(clippy::manual_range_contains)]
    pub fn trimmed_mean(&mut self, low: f64, high: f64) -> f64 {
        let _ = self.compress();
        if self.merged_nodes == 0 || low < 0.0 || low > 1.0 || high < 0.0 || high > 1.0 {
            return f64::NAN;
        }
        if self.merged_nodes == 1 {
            return self.means[0];
        }
        let leftmost = libm::floor(self.merged_weight as f64 * low);
        let rightmost = libm::ceil(self.merged_weight as f64 * high);
        self.trimmed(leftmost, rightmost)
    }

    /// `TDigestMemUsage`: the histogram and both node arrays at full capacity.
    pub fn mem_usage(&self) -> u64 {
        80 + 16 * self.cap as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_and_cdf() {
        let mut t = TDigest::new(100.0).unwrap();
        assert_eq!(t.cap, 610);
        for i in 1..=10 {
            t.add(i as f64, 1).unwrap();
        }
        assert_eq!(t.quantile(0.5), 6.0);
        assert_eq!(t.quantile(0.0), 1.0);
        assert_eq!(t.quantile(1.0), 10.0);
        assert_eq!(t.cdf(5.0), 0.45);
        assert_eq!(t.merged_nodes, 10);
        assert_eq!(t.total_compressions, 1);
        assert_eq!(t.trimmed_mean(0.1, 0.9), 5.5);
    }

    #[test]
    fn sorts_large_inputs() {
        let mut m: Vec<f64> = (0..500).map(|i| ((i * 7919) % 211) as f64).collect();
        let mut w: Vec<i64> = vec![1; 500];
        let n = m.len() as i32;
        sort(&mut m, &mut w, 0, n - 1);
        assert!(m.windows(2).all(|p| p[0] <= p[1]));
    }

    #[test]
    fn compresses_when_full() {
        let mut t = TDigest::new(1.0).unwrap();
        for i in 0..100 {
            t.add(i as f64, 1).unwrap();
        }
        assert_eq!(t.size(), 100);
        assert!(t.total_compressions > 0);
    }
}

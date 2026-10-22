//! t-digests as Elasticsearch builds them (`TDigestState` over its `org.elasticsearch.tdigest`
//! library), so `percentiles`, `percentile_ranks`, `median_absolute_deviation` and `boxplot`
//! return the same numbers.
//!
//! * [`TDigest::Hybrid`] (the default): exact sorted values until `round(compression) * 20`
//!   samples, then a merging digest seeded with them.
//! * [`MergingDigest`]: the buffered, alternating-direction merge with the `K_2` scale function
//!   and weight limit, two-level compression (internal compression `sqrt(scale) *
//!   compression`, the public one on `compress()`).
//! * [`AvlTreeDigest`] (`execution_hint: high_accuracy`): nearest-centroid insertion with
//!   reservoir tie-breaking and shuffled recompression. Elasticsearch seeds its generator
//!   randomly, so its output isn't reproducible; this one uses a fixed seed.
//!
//! Every digest keeps its state exactly as Elasticsearch's does (array sizes, merge counters,
//! the order centroids are re-added in) because quantiles depend on all of it.

use crate::error::{AggError, Result};
use crate::metrics::simple::{java_max, java_min};

/// `TDigest.checkValue`.
#[inline]
fn check_value(x: f64) -> Result<()> {
    if x.is_finite() { Ok(()) } else { Err(AggError::illegal_argument(format!("Invalid value: {}", crate::json::java_double(x)))) }
}

/// `AbstractTDigest.weightedAverage`.
#[inline]
fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    let (x1, w1, x2, w2) = if x1 <= x2 { (x1, w1, x2, w2) } else { (x2, w2, x1, w1) };
    let x = (x1 * w1 + x2 * w2) / (w1 + w2);
    java_max(x1, java_min(x, x2))
}

/// `AbstractTDigest.interpolate`.
#[inline]
fn interpolate(x: f64, x0: f64, x1: f64) -> f64 {
    (x - x0) / (x1 - x0)
}

/// `Dist.cdf` over sorted samples.
fn dist_cdf(x: f64, v: &[f64]) -> f64 {
    let n = v.len();
    if n == 0 {
        return f64::NAN;
    }
    if n == 1 {
        return if x < v[0] {
            0.0
        } else if x > v[0] {
            1.0
        } else {
            0.5
        };
    }
    if x.total_cmp(&v[0]).is_lt() {
        return 0.0;
    }
    if x.total_cmp(&v[0]).is_eq() {
        let dw = v.iter().take_while(|e| e.total_cmp(&x).is_eq()).count() as f64;
        return dw / 2.0 / n as f64;
    }
    if x > v[n - 1] {
        return 1.0;
    }
    if x == v[n - 1] {
        let dw = v.iter().rev().take_while(|e| e.total_cmp(&x).is_eq()).count() as f64;
        return (n as f64 - dw / 2.0) / n as f64;
    }
    let mut left = (v[1] - v[0]) / 2.0;
    let mut so_far = 0.0;
    for i in 0..n - 1 {
        let right = (v[i + 1] - v[i]) / 2.0;
        if x < v[i] + right {
            let value = (so_far + interpolate(x, v[i] - left, v[i] + right)) / n as f64;
            return value.max(0.0);
        }
        so_far += 1.0;
        left = right;
    }
    let last = n - 1;
    let right = (v[last] - v[last - 1]) / 2.0;
    if x < v[last] + right {
        return (so_far + interpolate(x, v[last] - right, v[last] + right)) / n as f64;
    }
    1.0
}

/// `Dist.quantile` over sorted samples.
fn dist_quantile(q: f64, v: &[f64]) -> f64 {
    let n = v.len();
    if n == 0 {
        return f64::NAN;
    }
    let index = q * (n - 1) as f64;
    let mut low = index.floor() as i64;
    let mut high = low + 1;
    let mut weight = index - low as f64;
    if index <= 0.0 {
        low = 0;
        high = 0;
        weight = 0.0;
    }
    if index >= (n - 1) as f64 {
        low = n as i64 - 1;
        high = n as i64 - 1;
        weight = 0.0;
    }
    let lv = v[low as usize];
    let hv = v[high as usize];
    lv + weight * (hv - lv)
}

/// `SortingDigest`: every sample, sorted on demand.
#[derive(Clone, Debug, Default)]
pub struct SortingDigest {
    values: Vec<f64>,
    sorted: bool,
    min: f64,
    max: f64,
}

impl SortingDigest {
    fn new() -> SortingDigest {
        SortingDigest { values: Vec::new(), sorted: true, min: f64::INFINITY, max: f64::NEG_INFINITY }
    }

    fn add(&mut self, x: f64, w: u64) -> Result<()> {
        check_value(x)?;
        self.sorted = self.sorted && self.values.last().is_none_or(|&l| l <= x);
        for _ in 0..w {
            self.values.push(x);
        }
        self.max = java_max(self.max, x);
        self.min = java_min(self.min, x);
        Ok(())
    }

    fn compress(&mut self) {
        if !self.sorted {
            self.values.sort_unstable_by(f64::total_cmp);
            self.sorted = true;
        }
    }
}

/// `MergingDigest` (`K_2`, weight limit, alternating sort, two-level compression).
#[derive(Clone, Debug)]
pub struct MergingDigest {
    merge_count: u64,
    public_compression: f64,
    compression: f64,
    last_used_cell: usize,
    total_weight: f64,
    weight: Vec<f64>,
    mean: Vec<f64>,
    unmerged_weight: f64,
    temp_used: usize,
    temp_weight: Vec<f64>,
    temp_mean: Vec<f64>,
    order: Vec<u32>,
    min: f64,
    max: f64,
}

/// `ScaleFunction.K_2` normalizer (`compression / Z(compression, n)`).
#[inline]
fn k2_normalizer(compression: f64, n: f64) -> f64 {
    compression / (4.0 * (n / compression).ln() + 24.0)
}

impl MergingDigest {
    pub fn new(compression: f64) -> MergingDigest {
        let compression = if compression < 10.0 { 10.0 } else { compression };
        let size_fudge = 10.0;
        let mut size = (compression + size_fudge).max(-1.0) as i64;
        let mut buffer_size: i64 = -1;
        if buffer_size < 5 * size {
            buffer_size = 5 * size;
        }
        // int division, as in Java.
        let scale = ((buffer_size / size - 1) as f64).max(1.0);
        let internal = scale.sqrt() * compression;
        if (size as f64) < internal + size_fudge {
            size = (internal + size_fudge).ceil() as i64;
        }
        if buffer_size <= 2 * size {
            buffer_size = 2 * size;
        }
        let (size, buffer_size) = (size as usize, buffer_size as usize);
        MergingDigest {
            merge_count: 0,
            public_compression: compression,
            compression: internal,
            last_used_cell: 0,
            total_weight: 0.0,
            weight: vec![0.0; size],
            mean: vec![0.0; size],
            unmerged_weight: 0.0,
            temp_used: 0,
            temp_weight: vec![0.0; buffer_size],
            temp_mean: vec![0.0; buffer_size],
            order: vec![0; buffer_size],
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    pub fn add(&mut self, x: f64, w: u64) -> Result<()> {
        check_value(x)?;
        if self.temp_used + self.last_used_cell + 1 >= self.temp_weight.len() {
            self.merge_new_values(self.compression);
        }
        let at = self.temp_used;
        self.temp_used += 1;
        self.temp_weight[at] = w as f64;
        self.temp_mean[at] = x;
        self.unmerged_weight += w as f64;
        if x < self.min {
            self.min = x;
        }
        if x > self.max {
            self.max = x;
        }
        Ok(())
    }

    fn merge_new_values(&mut self, compression: f64) {
        if self.total_weight == 0.0 && self.unmerged_weight == 0.0 {
            return;
        }
        if self.unmerged_weight > 0.0 {
            let backwards = self.merge_count % 2 == 1;
            self.merge(backwards, compression);
            self.merge_count += 1;
            self.temp_used = 0;
            self.unmerged_weight = 0.0;
        }
    }

    fn merge(&mut self, run_backwards: bool, compression: f64) {
        let last = self.last_used_cell;
        let start = self.temp_used;
        self.temp_mean[start..start + last].copy_from_slice(&self.mean[..last]);
        self.temp_weight[start..start + last].copy_from_slice(&self.weight[..last]);
        let n = start + last;
        // Sort.stableSort: by value, ties by position.
        for (i, o) in self.order[..n].iter_mut().enumerate() {
            *o = i as u32;
        }
        let tm = &self.temp_mean;
        self.order[..n].sort_unstable_by(|&a, &b| tm[a as usize].total_cmp(&tm[b as usize]).then(a.cmp(&b)));
        self.total_weight += self.unmerged_weight;
        if run_backwards {
            self.order[..n].reverse();
        }
        let total = self.total_weight;
        let mut cell = 0usize;
        let first = self.order[0] as usize;
        self.mean[0] = self.temp_mean[first];
        self.weight[0] = self.temp_weight[first];
        let mut w_so_far = 0.0;
        let normalizer = k2_normalizer(compression, total);
        let max_of = |q: f64| q * (1.0 - q) / normalizer;
        let cap = self.mean.len() - 1;
        for i in 1..n {
            let ix = self.order[i] as usize;
            let proposed = self.weight[cell] + self.temp_weight[ix];
            let q0 = w_so_far / total;
            let q2 = (w_so_far + proposed) / total;
            let mut add_this = proposed <= total * java_min(max_of(q0), max_of(q2));
            if i == 1 || i == n - 1 {
                add_this = false;
            }
            if cell == cap {
                add_this = true;
            }
            if add_this {
                let m = weighted_average(self.mean[cell], self.weight[cell], self.temp_mean[ix], self.temp_weight[ix]);
                self.weight[cell] += self.temp_weight[ix];
                self.mean[cell] = m;
            } else {
                w_so_far += self.weight[cell];
                cell += 1;
                self.mean[cell] = self.temp_mean[ix];
                self.weight[cell] = self.temp_weight[ix];
            }
            self.temp_weight[ix] = 0.0;
        }
        cell += 1;
        self.last_used_cell = cell;
        if run_backwards {
            self.mean[..cell].reverse();
            self.weight[..cell].reverse();
        }
        if total > 0.0 {
            self.min = java_min(self.min, self.mean[0]);
            self.max = java_max(self.max, self.mean[cell - 1]);
        }
    }

    /// `compress()`: merges with the public compression.
    pub fn compress(&mut self) {
        self.merge_new_values(self.public_compression);
    }

    pub fn size(&self) -> u64 {
        (self.total_weight + self.unmerged_weight) as u64
    }

    pub fn cdf(&mut self, x: f64) -> Result<f64> {
        check_value(x)?;
        self.merge_new_values(self.compression);
        Ok(self.cdf_merged(x))
    }

    /// `cdf` once every sample is merged.
    fn cdf_merged(&self, x: f64) -> f64 {
        let n = self.last_used_cell;
        let (mean, weight) = (&self.mean, &self.weight);
        let size = self.size() as f64;
        if n == 0 {
            return f64::NAN;
        }
        if n == 1 {
            return if x < self.min {
                0.0
            } else if x > self.max {
                1.0
            } else {
                0.5
            };
        }
        if x < self.min {
            return 0.0;
        }
        if x.total_cmp(&self.min).is_eq() {
            let mut dw = 0.0;
            for i in 0..n {
                if !mean[i].total_cmp(&x).is_eq() {
                    break;
                }
                dw += weight[i];
            }
            return dw / 2.0 / size;
        }
        if x > self.max {
            return 1.0;
        }
        if x == self.max {
            let mut dw = 0.0;
            for i in (0..n).rev() {
                if !mean[i].total_cmp(&x).is_eq() {
                    break;
                }
                dw += weight[i];
            }
            return (size - dw / 2.0) / size;
        }
        let mut left = (mean[1] - mean[0]) / 2.0;
        let mut so_far = 0.0;
        for i in 0..n - 1 {
            let right = (mean[i + 1] - mean[i]) / 2.0;
            if x < mean[i] + right {
                let value = (so_far + weight[i] * interpolate(x, mean[i] - left, mean[i] + right)) / size;
                return value.max(0.0);
            }
            so_far += weight[i];
            left = right;
        }
        let last = n - 1;
        let right = (mean[last] - mean[last - 1]) / 2.0;
        if x < mean[last] + right {
            return (so_far + weight[last] * interpolate(x, mean[last] - right, mean[last] + right)) / size;
        }
        1.0
    }

    pub fn quantile(&mut self, q: f64) -> f64 {
        self.merge_new_values(self.compression);
        self.quantile_merged(q)
    }

    /// `quantile` once every sample is merged.
    fn quantile_merged(&self, q: f64) -> f64 {
        let n = self.last_used_cell;
        if n == 0 {
            return f64::NAN;
        } else if n == 1 {
            return self.mean[0];
        }
        let (mean, weight, total) = (&self.mean, &self.weight, self.total_weight);
        let index = q * total;
        if index < 0.0 {
            return self.min;
        }
        if index >= total {
            return self.max;
        }
        let mut so_far = weight[0] / 2.0;
        if weight[0] > 1.0 && index < so_far {
            return weighted_average(self.min, so_far - index, mean[0], index);
        }
        if weight[n - 1] > 1.0 && total - index <= weight[n - 1] / 2.0 {
            return self.max - (total - index - 1.0) / (weight[n - 1] / 2.0 - 1.0) * (self.max - mean[n - 1]);
        }
        for i in 0..n - 1 {
            let dw = (weight[i] + weight[i + 1]) / 2.0;
            if so_far + dw > index {
                let z1 = index - so_far;
                let z2 = so_far + dw - index;
                return weighted_average(mean[i], z2, mean[i + 1], z1);
            }
            so_far += dw;
        }
        let z1 = index - so_far;
        let z2 = weight[n - 1] / 2.0 - z1;
        weighted_average(mean[n - 1], z1, self.max, z2)
    }

    /// `centroids()` (merges pending samples first).
    pub fn centroids(&mut self) -> Vec<(f64, u64)> {
        self.merge_new_values(self.compression);
        (0..self.last_used_cell).map(|i| (self.mean[i], self.weight[i] as u64)).collect()
    }

    fn ram_bytes(&self) -> usize {
        8 * (self.mean.len() + self.weight.len() + self.temp_mean.len() + self.temp_weight.len()) + 4 * self.order.len() + 128
    }
}

/// `java.util.Random` (48-bit LCG), for the AVL tree digest.
#[derive(Clone, Debug)]
struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    fn new(seed: u64) -> JavaRandom {
        JavaRandom { seed: (seed ^ 0x5DEECE66D) & ((1 << 48) - 1) }
    }

    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(0x5DEECE66D).wrapping_add(0xB)) & ((1 << 48) - 1);
        (self.seed >> (48 - bits)) as i64 as i32
    }

    fn next_double(&mut self) -> f64 {
        ((((self.next(26) as i64) << 27) + self.next(27) as i64) as f64) * (1.0 / (1u64 << 53) as f64)
    }

    fn next_int(&mut self, bound: i32) -> i32 {
        let mut r = self.next(31);
        let m = bound - 1;
        if bound & m == 0 {
            return ((bound as i64 * r as i64) >> 31) as i32;
        }
        let mut u = r;
        loop {
            r = u % bound;
            if u - r + m >= 0 {
                return r;
            }
            u = self.next(31);
        }
    }
}

/// `AVLTreeDigest`, with the tree as a sorted array (same order: equal means insert after
/// existing ones, in-place updates when the order allows).
#[derive(Clone, Debug)]
pub struct AvlTreeDigest {
    compression: f64,
    means: Vec<f64>,
    counts: Vec<u64>,
    count: u64,
    needs_compression: bool,
    rng: JavaRandom,
    min: f64,
    max: f64,
}

impl AvlTreeDigest {
    pub fn new(compression: f64) -> AvlTreeDigest {
        AvlTreeDigest {
            compression,
            means: Vec::new(),
            counts: Vec::new(),
            count: 0,
            needs_compression: false,
            rng: JavaRandom::new(0x2545_F491_4F6C_DD1D),
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Inserts after every centroid whose mean is `<=` it (`AVLGroupTree` comparison).
    fn insert(&mut self, x: f64, w: u64) {
        let at = self.means.partition_point(|&m| m <= x);
        self.means.insert(at, x);
        self.counts.insert(at, w);
    }

    pub fn add(&mut self, x: f64, w: u64) -> Result<()> {
        check_value(x)?;
        self.add_unchecked(x, w);
        Ok(())
    }

    fn add_unchecked(&mut self, x: f64, w: u64) {
        self.needs_compression = true;
        if x < self.min {
            self.min = x;
        }
        if x > self.max {
            self.max = x;
        }
        if self.means.is_empty() {
            self.insert(x, w);
            self.count = w;
            return;
        }
        // floor(x): the last centroid with mean < x, else the first.
        let floor = self.means.partition_point(|&m| m < x);
        let first = floor.saturating_sub(1);
        let mut start = first;
        let mut min_distance = f64::MAX;
        let mut last_neighbor = self.means.len();
        for nb in first..self.means.len() {
            let z = (self.means[nb] - x).abs();
            if z < min_distance {
                start = nb;
                min_distance = z;
            } else if z > min_distance {
                last_neighbor = nb;
                break;
            }
        }
        let mut closest = usize::MAX;
        let mut n = 0.0f64;
        let mut sum: u64 = self.counts[..start].iter().sum();
        for nb in start..last_neighbor {
            let c = self.counts[nb];
            let q = if self.count == 1 { 0.5 } else { (sum as f64 + (c as f64 - 1.0) / 2.0) / (self.count - 1) as f64 };
            let k = 4.0 * self.count as f64 * q * (1.0 - q) / self.compression;
            if (c + w) as f64 <= k {
                n += 1.0;
                if self.rng.next_double() < 1.0 / n {
                    closest = nb;
                }
            }
            sum += c;
        }
        if closest == usize::MAX {
            self.insert(x, w);
        } else {
            let centroid = weighted_average(self.means[closest], self.counts[closest] as f64, x, w as f64);
            let count = self.counts[closest] + w;
            self.update(closest, centroid, count);
        }
        self.count += w;
        if self.means.len() as f64 > 20.0 * self.compression {
            self.compress();
        }
    }

    /// `IntAVLTree.update`: in place when the new mean still sorts there, else re-inserted.
    fn update(&mut self, node: usize, mean: f64, count: u64) {
        let prev_ok = node == 0 || mean >= self.means[node - 1];
        let next_ok = node + 1 == self.means.len() || mean < self.means[node + 1];
        if prev_ok && next_ok {
            self.means[node] = mean;
            self.counts[node] = count;
        } else {
            self.means.remove(node);
            self.counts.remove(node);
            self.insert(mean, count);
        }
    }

    pub fn compress(&mut self) {
        if !self.needs_compression {
            return;
        }
        self.needs_compression = false;
        let means = std::mem::take(&mut self.means);
        let counts = std::mem::take(&mut self.counts);
        let mut nodes: Vec<usize> = (0..means.len()).collect();
        for i in (1..nodes.len()).rev() {
            let other = self.rng.next_int(i as i32 + 1) as usize;
            nodes.swap(other, i);
        }
        self.count = 0;
        for i in nodes {
            self.add_unchecked(means[i], counts[i]);
        }
    }

    pub fn cdf(&self, x: f64) -> f64 {
        let (m, c) = (&self.means, &self.counts);
        if m.is_empty() {
            return f64::NAN;
        }
        if m.len() == 1 {
            return if x < m[0] {
                0.0
            } else if x > m[0] {
                1.0
            } else {
                0.5
            };
        }
        if x < self.min {
            return 0.0;
        }
        let size = self.count as f64;
        if x.total_cmp(&self.min).is_eq() {
            let dw: u64 = m.iter().zip(c).take_while(|(v, _)| v.total_cmp(&x).is_eq()).map(|(_, c)| *c).sum();
            return dw as f64 / 2.0 / size;
        }
        if x > self.max {
            return 1.0;
        }
        if x.total_cmp(&self.max).is_eq() {
            let dw: u64 = m.iter().zip(c).rev().take_while(|(v, _)| v.total_cmp(&x).is_eq()).map(|(_, c)| *c).sum();
            return (size - dw as f64 / 2.0) / size;
        }
        let (mut a, mut b) = (0usize, 1usize);
        let mut left = (m[b] - m[a]) / 2.0;
        let mut right = left;
        let mut r = 0.0f64;
        while b + 1 < m.len() {
            if x < m[a] + right {
                let value = (r + c[a] as f64 * interpolate(x, m[a] - left, m[a] + right)) / size;
                return value.max(0.0);
            }
            r += c[a] as f64;
            a = b;
            left = right;
            b += 1;
            right = (m[b] - m[a]) / 2.0;
        }
        if x < m[a] + right {
            return (r + c[a] as f64 * interpolate(x, m[a] - left, m[a] + right)) / size;
        }
        r += c[a] as f64;
        if x < m[b] + right {
            return (r + c[b] as f64 * interpolate(x, m[b] - right, m[b] + right)) / size;
        }
        1.0
    }

    pub fn quantile(&self, q: f64) -> f64 {
        let (m, c) = (&self.means, &self.counts);
        if m.is_empty() {
            return f64::NAN;
        } else if m.len() == 1 {
            return m[0];
        }
        let count = self.count as f64;
        let index = q * count;
        if index <= 0.0 {
            return self.min;
        }
        if index >= count {
            return self.max;
        }
        let mut cur = 0usize;
        let mut cur_w = c[0];
        let mut so_far = cur_w as f64 / 2.0;
        if index <= so_far && so_far > 1.0 {
            return weighted_average(self.min, so_far - index, m[0], index);
        }
        for _ in 0..m.len() - 1 {
            let next = cur + 1;
            let next_w = c[next];
            let dw = (cur_w + next_w) as f64 / 2.0;
            if index < so_far + dw {
                let w1 = index - so_far;
                let w2 = so_far + dw - index;
                return weighted_average(m[cur], w2, m[next], w1);
            }
            so_far += dw;
            cur = next;
            cur_w = next_w;
        }
        let w1 = index - so_far;
        let w2 = cur_w as f64 / 2.0 - w1;
        weighted_average(m[cur], w2, self.max, w1)
    }
}

/// `HybridDigest`: sorted samples, switching to a merging digest at `round(compression) * 20`.
#[derive(Clone, Debug)]
pub struct HybridDigest {
    compression: f64,
    max_sorting_size: u64,
    sorting: Option<SortingDigest>,
    merging: Option<Box<MergingDigest>>,
}

impl HybridDigest {
    pub fn new(compression: f64) -> HybridDigest {
        // Math.round(double) * 20.
        let max_sorting_size = ((compression + 0.5).floor() as i64).saturating_mul(20).max(0) as u64;
        HybridDigest { compression, max_sorting_size, sorting: Some(SortingDigest::new()), merging: None }
    }

    fn reserve(&mut self, size: u64) -> Result<()> {
        if self.merging.is_some() {
            return Ok(());
        }
        let s = self.sorting.as_ref().map_or(0, |s| s.values.len() as u64);
        if s + size >= self.max_sorting_size {
            let mut m = Box::new(MergingDigest::new(self.compression));
            if let Some(sorting) = self.sorting.take() {
                for &v in &sorting.values {
                    m.add(v, 1)?;
                }
            }
            self.merging = Some(m);
        }
        Ok(())
    }

    pub fn add(&mut self, x: f64, w: u64) -> Result<()> {
        self.reserve(w)?;
        match (&mut self.merging, &mut self.sorting) {
            (Some(m), _) => m.add(x, w),
            (None, Some(s)) => s.add(x, w),
            (None, None) => Ok(()),
        }
    }

    /// `add(TDigestReadView)`: one reservation for the whole digest, then its centroids.
    fn add_centroids(&mut self, size: u64, centroids: &[(f64, u64)]) -> Result<()> {
        self.reserve(size)?;
        for &(x, w) in centroids {
            match (&mut self.merging, &mut self.sorting) {
                (Some(m), _) => m.add(x, w)?,
                (None, Some(s)) => s.add(x, w)?,
                (None, None) => {}
            }
        }
        Ok(())
    }

    pub fn compress(&mut self) {
        match (&mut self.merging, &mut self.sorting) {
            (Some(m), _) => m.compress(),
            (None, Some(s)) => s.compress(),
            (None, None) => {}
        }
    }
}

/// `TDigestState`: the digest behind a percentiles state.
#[derive(Clone, Debug)]
pub enum TDigest {
    Hybrid(HybridDigest),
    Avl(Box<AvlTreeDigest>),
}

impl TDigest {
    /// `TDigestState.create(compression, hint)`.
    pub fn new(compression: f64, high_accuracy: bool) -> TDigest {
        if high_accuracy { TDigest::Avl(Box::new(AvlTreeDigest::new(compression))) } else { TDigest::Hybrid(HybridDigest::new(compression)) }
    }

    /// An empty digest of the same type and compression (`createUsingParamsFrom`).
    pub fn empty_like(&self) -> TDigest {
        match self {
            TDigest::Hybrid(h) => TDigest::Hybrid(HybridDigest::new(h.compression)),
            TDigest::Avl(a) => TDigest::Avl(Box::new(AvlTreeDigest::new(a.compression))),
        }
    }

    pub fn compression(&self) -> f64 {
        match self {
            TDigest::Hybrid(h) => h.compression,
            TDigest::Avl(a) => a.compression,
        }
    }

    #[inline]
    pub fn add(&mut self, x: f64) -> Result<()> {
        self.add_weighted(x, 1)
    }

    pub fn add_weighted(&mut self, x: f64, w: u64) -> Result<()> {
        match self {
            TDigest::Hybrid(h) => h.add(x, w),
            TDigest::Avl(a) => a.add(x, w),
        }
    }

    /// `add(TDigestState other)`.
    pub fn merge(&mut self, other: &mut TDigest) -> Result<()> {
        let size = other.size();
        let centroids = other.centroids();
        match self {
            TDigest::Hybrid(h) => h.add_centroids(size, &centroids),
            TDigest::Avl(a) => {
                for (x, w) in centroids {
                    a.add(x, w)?;
                }
                Ok(())
            }
        }
    }

    pub fn compress(&mut self) {
        match self {
            TDigest::Hybrid(h) => h.compress(),
            TDigest::Avl(a) => a.compress(),
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            TDigest::Hybrid(h) => match (&h.merging, &h.sorting) {
                (Some(m), _) => m.size(),
                (None, Some(s)) => s.values.len() as u64,
                _ => 0,
            },
            TDigest::Avl(a) => a.count,
        }
    }

    /// `centroids()` (sorting digests sort, merging digests merge pending samples).
    pub fn centroids(&mut self) -> Vec<(f64, u64)> {
        match self {
            TDigest::Hybrid(h) => match (&mut h.merging, &mut h.sorting) {
                (Some(m), _) => m.centroids(),
                (None, Some(s)) => {
                    s.compress();
                    s.values.iter().map(|&v| (v, 1)).collect()
                }
                _ => Vec::new(),
            },
            TDigest::Avl(a) => a.means.iter().copied().zip(a.counts.iter().copied()).collect(),
        }
    }

    pub fn quantile(&mut self, q: f64) -> f64 {
        match self {
            TDigest::Hybrid(h) => match (&mut h.merging, &mut h.sorting) {
                (Some(m), _) => m.quantile(q),
                (None, Some(s)) => {
                    s.compress();
                    dist_quantile(q, &s.values)
                }
                _ => f64::NAN,
            },
            TDigest::Avl(a) => a.quantile(q),
        }
    }

    pub fn cdf(&mut self, x: f64) -> Result<f64> {
        match self {
            TDigest::Hybrid(h) => match (&mut h.merging, &mut h.sorting) {
                (Some(m), _) => m.cdf(x),
                (None, Some(s)) => {
                    s.compress();
                    Ok(dist_cdf(x, &s.values))
                }
                _ => Ok(f64::NAN),
            },
            TDigest::Avl(a) => Ok(a.cdf(x)),
        }
    }

    /// `quantile` without merging or sorting: for compressed digests (shard results and
    /// reduced states), whose reads don't change them. Anything still pending is read from a
    /// compressed copy.
    pub fn quantile_compressed(&self, q: f64) -> f64 {
        match self {
            TDigest::Hybrid(h) => match (&h.merging, &h.sorting) {
                (Some(m), _) if m.unmerged_weight == 0.0 => m.quantile_merged(q),
                (None, Some(s)) if s.sorted => dist_quantile(q, &s.values),
                _ => self.clone().quantile(q),
            },
            TDigest::Avl(a) => a.quantile(q),
        }
    }

    /// `centroids()` of a compressed digest, without changing it.
    pub fn centroids_compressed(&self) -> Vec<(f64, u64)> {
        match self {
            TDigest::Hybrid(h) => match (&h.merging, &h.sorting) {
                (Some(m), _) if m.unmerged_weight == 0.0 => (0..m.last_used_cell).map(|i| (m.mean[i], m.weight[i] as u64)).collect(),
                (None, Some(s)) if s.sorted => s.values.iter().map(|&v| (v, 1)).collect(),
                _ => self.clone().centroids(),
            },
            TDigest::Avl(a) => a.means.iter().copied().zip(a.counts.iter().copied()).collect(),
        }
    }

    /// `cdf` for compressed digests (see [`TDigest::quantile_compressed`]).
    pub fn cdf_compressed(&self, x: f64) -> Result<f64> {
        check_value(x)?;
        match self {
            TDigest::Hybrid(h) => match (&h.merging, &h.sorting) {
                (Some(m), _) if m.unmerged_weight == 0.0 => Ok(m.cdf_merged(x)),
                (None, Some(s)) if s.sorted => Ok(dist_cdf(x, &s.values)),
                _ => self.clone().cdf(x),
            },
            TDigest::Avl(a) => Ok(a.cdf(x)),
        }
    }

    pub fn min(&self) -> f64 {
        match self {
            TDigest::Hybrid(h) => match (&h.merging, &h.sorting) {
                (Some(m), _) => m.min,
                (None, Some(s)) => s.min,
                _ => f64::INFINITY,
            },
            TDigest::Avl(a) => a.min,
        }
    }

    pub fn max(&self) -> f64 {
        match self {
            TDigest::Hybrid(h) => match (&h.merging, &h.sorting) {
                (Some(m), _) => m.max,
                (None, Some(s)) => s.max,
                _ => f64::NEG_INFINITY,
            },
            TDigest::Avl(a) => a.max,
        }
    }

    /// Approximate heap bytes.
    pub fn ram_bytes(&self) -> usize {
        match self {
            TDigest::Hybrid(h) => 64 + h.sorting.as_ref().map_or(0, |s| s.values.capacity() * 8) + h.merging.as_ref().map_or(0, |m| m.ram_bytes()),
            TDigest::Avl(a) => 64 + a.means.capacity() * 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorting_phase_is_exact() {
        let mut d = TDigest::new(100.0, false);
        for v in [5.0, 1.0, 3.0, 2.0] {
            d.add(v).unwrap();
        }
        d.compress();
        assert_eq!(d.quantile(0.5), 2.5);
        assert_eq!(d.quantile(0.0), 1.0);
        assert_eq!(d.quantile(1.0), 5.0);
        assert_eq!(d.cdf(3.0).unwrap(), 0.5833333333333334);
    }

    #[test]
    fn merging_sizes_follow_elasticsearch() {
        let m = MergingDigest::new(100.0);
        assert_eq!(m.compression, 200.0);
        assert_eq!(m.mean.len(), 210);
        assert_eq!(m.temp_mean.len(), 550);
        let mut d = TDigest::new(100.0, false);
        for i in 0..10_000 {
            d.add(i as f64).unwrap();
        }
        d.compress();
        assert_eq!(d.size(), 10_000);
        let median = d.quantile(0.5);
        assert!((median - 4999.5).abs() < 5.0, "{median}");
        assert_eq!(d.quantile(0.0), 0.0);
        assert_eq!(d.quantile(1.0), 9999.0);
    }

    #[test]
    fn java_random_matches_jdk() {
        // new Random(42).nextInt(100) == 30, then nextDouble() == 0.05466526274716077 (JDK 21).
        let mut r = JavaRandom::new(42);
        assert_eq!(r.next_int(100), 30);
        assert_eq!(r.next_double(), 0.05466526274716077);
    }
}

//! `exponential_histogram` fields (OpenTelemetry base-2 exponential histograms) as
//! Elasticsearch's `libs/exponential-histogram` merges and queries them.
//!
//! A histogram has a `scale`: bucket `i` holds the values in `(base^i, base^(i+1)]` with
//! `base = 2^(2^-scale)`, separately for positive and negative values (by absolute value), plus
//! a zero bucket for `|v| <= zero_threshold`, a `sum`, `min` and `max`. Aggregations read:
//!
//! * `sum`, `avg`, `min`, `max`, `value_count`: the stored summary only (no bucket decoding);
//! * `percentiles`, `percentile_ranks`, `boxplot`: every doc's histogram merged with
//!   [`Merger`] (at most [`DEFAULT_MAX_BUCKETS`] buckets: on overflow the scale drops by just as
//!   much as needed, `DownscaleStats`), then [`quantile`] / [`cdf`], which interpolate between
//!   the buckets' points of least relative error clamped to `[min, max]`;
//! * `histogram`, `range`: each bucket's point of least relative error (clamped) with the
//!   bucket's count ([`ExpHistogram::centers`]).
//!
//! Index math follows `Base2ExponentialHistogramIndexer` (exact for scale <= 0, `log` based
//! above) and `ExponentialScaleUtils`; `exp`/`ln`/`powf` are the platform's correctly rounded
//! functions, as HotSpot's intrinsics are (checked against the JDK in
//! `tests/exp_histogram.rs`).
//!
//! The doc value is one binary value per doc ([`encode`]): scale, flags, zero threshold, value
//! count, sum, optional min and max, then the negative and positive buckets delta-encoded the
//! way `ExponentialHistogramState` serializes them (`ZLong` first index and `VLong` count, then
//! a count per following bucket, preceded by a negative `ZLong` index delta when the next index
//! is not the previous plus one). The zero bucket's count is the value count minus the buckets'
//! counts, as in `CompressedExponentialHistogram`.

use crate::error::{AggError, Result};

/// `ExponentialHistogram.MAX_SCALE`.
pub const MAX_SCALE: i32 = 38;
/// `ExponentialHistogram.MIN_SCALE`.
pub const MIN_SCALE: i32 = -11;
/// `ExponentialHistogram.MAX_INDEX_BITS`.
pub const MAX_INDEX_BITS: u32 = 62;
/// `ExponentialHistogram.MAX_INDEX`.
pub const MAX_INDEX: i64 = (1 << MAX_INDEX_BITS) - 1;
/// `ExponentialHistogram.MIN_INDEX`.
pub const MIN_INDEX: i64 = -MAX_INDEX;
/// `ExponentialHistogramMerger.DEFAULT_MAX_HISTOGRAM_BUCKETS`.
pub const DEFAULT_MAX_BUCKETS: usize = 320;

const LN_2: f64 = std::f64::consts::LN_2;

/// `Math.scalb` for the small exponents the index math uses (exact: one rounding at most).
#[inline]
fn scalb(x: f64, n: i32) -> f64 {
    debug_assert!((-1022..=1023).contains(&n));
    x * f64::from_bits(((1023 + n) as u64) << 52)
}

/// `Base2ExponentialHistogramIndexer.computeIndex`: the bucket of `|value|` at `scale`.
pub fn compute_index(value: f64, scale: i32) -> i64 {
    let abs = value.abs();
    if scale > 0 {
        let log2 = abs.ln() * (1.0 / LN_2);
        return scalb(log2, scale).ceil() as i64 - 1;
    }
    let zero = index_scale_zero(abs);
    if scale == 0 { zero } else { zero >> -scale }
}

/// `mapToIndexScaleZero`: from the IEEE exponent, exact.
fn index_scale_zero(value: f64) -> i64 {
    let bits = value.to_bits() as i64;
    let mut raw_exponent = (bits & 0x7FF0_0000_0000_0000) >> 52;
    let significand = bits & 0x000F_FFFF_FFFF_FFFF;
    if raw_exponent == 0 {
        raw_exponent -= (significand.wrapping_sub(1) as u64).leading_zeros() as i64 - 11 - 1;
    }
    let ieee_exponent = (raw_exponent - 1023) as i32 as i64;
    if significand == 0 { ieee_exponent - 1 } else { ieee_exponent }
}

/// `exponentiallyScaledToDoubleValue`: `base^index`.
#[inline]
pub fn scaled_to_double(index: i64, scale: i32) -> f64 {
    (scalb(LN_2, -scale) * index as f64).exp()
}

/// `getLowerBucketBoundary`.
pub fn lower_boundary(index: i64, scale: i32) -> f64 {
    scaled_to_double(index, scale)
}

/// `getUpperBucketBoundary`.
pub fn upper_boundary(index: i64, scale: i32) -> f64 {
    scaled_to_double(index + 1, scale)
}

/// `getPointOfLeastRelativeError`: the bucket's representative value.
pub fn point_of_least_relative_error(index: i64, scale: i32) -> f64 {
    let base = 2f64.powf(scalb(1.0, -scale));
    if base.is_finite() {
        2.0 / (base + 1.0) * upper_boundary(index, scale)
    } else if index >= 0 {
        f64::INFINITY
    } else {
        0.0
    }
}

/// `compareExponentiallyScaledValues`: compares `base_a^idx_a` with `base_b^idx_b` exactly.
pub fn compare_scaled(idx_a: i64, scale_a: i32, idx_b: i64, scale_b: i32) -> std::cmp::Ordering {
    if scale_a > scale_b {
        return compare_scaled(idx_b, scale_b, idx_a, scale_a).reverse();
    }
    let shifts = (scale_b - scale_a) as u32;
    let down_b = idx_b >> shifts;
    match idx_a.cmp(&down_b) {
        std::cmp::Ordering::Equal => {
            let shifted_away = idx_b & ((1i64 << shifts) - 1);
            if shifted_away > 0 { std::cmp::Ordering::Less } else { std::cmp::Ordering::Equal }
        }
        o => o,
    }
}

/// `Math.clamp(value, min, max)` (`min <= max`): NaN stays NaN.
#[inline]
fn clamp(value: f64, min: f64, max: f64) -> f64 {
    crate::metrics::simple::java_min(max, crate::metrics::simple::java_max(value, min))
}

/// `ZeroBucket`: the count of values with `|v| <= threshold`. The threshold is either a real
/// number or an exponentially scaled index (after collapsing buckets into it); both forms are
/// kept since comparisons use the index and estimates the value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZeroBucket {
    scale: i32,
    index: i64,
    threshold: f64,
    index_based: bool,
    pub count: u64,
}

impl ZeroBucket {
    /// `ZeroBucket.minimalEmpty()`.
    pub fn minimal_empty() -> ZeroBucket {
        ZeroBucket::minimal_with_count(0)
    }

    /// `minimalWithCount`.
    pub fn minimal_with_count(count: u64) -> ZeroBucket {
        ZeroBucket { scale: MIN_SCALE, index: MIN_INDEX, threshold: scaled_to_double(MIN_INDEX, MIN_SCALE), index_based: true, count }
    }

    /// `create(zeroThreshold, count)`.
    pub fn with_threshold(threshold: f64, count: u64) -> ZeroBucket {
        if threshold == 0.0 {
            return ZeroBucket::minimal_with_count(count);
        }
        ZeroBucket { scale: MAX_SCALE, index: compute_index(threshold, MAX_SCALE) + 1, threshold, index_based: false, count }
    }

    /// `create(index, scale, count)`.
    pub fn with_index(index: i64, scale: i32, count: u64) -> ZeroBucket {
        if index == MIN_INDEX && scale == MIN_SCALE {
            return ZeroBucket::minimal_with_count(count);
        }
        ZeroBucket { scale, index, threshold: scaled_to_double(index, scale), index_based: true, count }
    }

    pub fn zero_threshold(&self) -> f64 {
        self.threshold
    }

    pub fn index(&self) -> i64 {
        self.index
    }

    pub fn scale(&self) -> i32 {
        self.scale
    }

    pub fn is_index_based(&self) -> bool {
        self.index_based
    }

    /// `withCount`.
    pub fn with_count(&self, count: u64) -> ZeroBucket {
        ZeroBucket { count, ..*self }
    }

    /// `compareZeroThreshold`.
    pub fn compare_threshold(&self, other: &ZeroBucket) -> std::cmp::Ordering {
        compare_scaled(self.index, self.scale, other.index, other.scale)
    }

    /// `merge`: the larger threshold with both counts.
    pub fn merge(&self, other: &ZeroBucket) -> ZeroBucket {
        if other.count == 0 {
            *self
        } else if self.count == 0 {
            *other
        } else {
            let total = self.count + other.count;
            if self.compare_threshold(other).is_ge() { self.with_count(total) } else { other.with_count(total) }
        }
    }

    /// `collapseOverlappingBuckets`: absorbs the iterator's buckets below the threshold
    /// (consuming them), raising the threshold to the last absorbed bucket's upper bound.
    pub fn collapse_overlapping<I: BucketIter>(&self, buckets: &mut I) -> ZeroBucket {
        let mut collapsed = 0u64;
        let mut highest = 0i64;
        while buckets.has_next() && compare_scaled(buckets.peek_index(), buckets.scale(), self.index, self.scale).is_lt() {
            highest = buckets.peek_index();
            collapsed += buckets.peek_count();
            buckets.advance();
        }
        if collapsed == 0 {
            return *self;
        }
        let count = self.count + collapsed;
        let upper = highest + 1;
        if compare_scaled(self.index, self.scale, upper, buckets.scale()).is_ge() {
            self.with_count(count)
        } else {
            ZeroBucket::with_index(upper, buckets.scale(), count)
        }
    }

    /// `collapseOverlappingBucketsForAll` over the negative and positive iterators, until the
    /// threshold stops moving.
    pub fn collapse_all<A: BucketIter, B: BucketIter>(&self, a: &mut A, b: &mut B) -> ZeroBucket {
        let mut current = *self;
        loop {
            let previous = current;
            current = current.collapse_overlapping(a);
            current = current.collapse_overlapping(b);
            if previous.compare_threshold(&current).is_eq() {
                return current;
            }
        }
    }
}

/// `BucketIterator`.
pub trait BucketIter {
    fn has_next(&self) -> bool;
    fn peek_index(&self) -> i64;
    fn peek_count(&self) -> u64;
    fn advance(&mut self);
    fn scale(&self) -> i32;
}

/// Buckets in ascending index order.
#[derive(Clone, Copy, Debug)]
pub struct SliceIter<'a> {
    buckets: &'a [(i64, u64)],
    pos: usize,
    scale: i32,
}

impl<'a> SliceIter<'a> {
    pub fn new(buckets: &'a [(i64, u64)], scale: i32) -> SliceIter<'a> {
        SliceIter { buckets, pos: 0, scale }
    }
}

impl BucketIter for SliceIter<'_> {
    #[inline]
    fn has_next(&self) -> bool {
        self.pos < self.buckets.len()
    }
    #[inline]
    fn peek_index(&self) -> i64 {
        self.buckets[self.pos].0
    }
    #[inline]
    fn peek_count(&self) -> u64 {
        self.buckets[self.pos].1
    }
    #[inline]
    fn advance(&mut self) {
        self.pos += 1;
    }
    #[inline]
    fn scale(&self) -> i32 {
        self.scale
    }
}

/// `ScaleReducingBucketIterator`: merges buckets that fall together at a lower scale.
#[derive(Clone, Copy, Debug)]
struct ScaleReducing<I> {
    inner: I,
    reduction: u32,
    index: i64,
    count: u64,
    has: bool,
}

impl<I: BucketIter> ScaleReducing<I> {
    fn new(inner: I, target_scale: i32) -> ScaleReducing<I> {
        let reduction = (inner.scale() - target_scale) as u32;
        let mut s = ScaleReducing { inner, reduction, index: 0, count: 0, has: true };
        s.advance();
        s
    }
}

impl<I: BucketIter> BucketIter for ScaleReducing<I> {
    fn has_next(&self) -> bool {
        self.has
    }
    fn peek_index(&self) -> i64 {
        self.index
    }
    fn peek_count(&self) -> u64 {
        self.count
    }
    fn advance(&mut self) {
        self.has = self.inner.has_next();
        if !self.has {
            return;
        }
        self.index = self.inner.peek_index() >> self.reduction;
        self.count = self.inner.peek_count();
        self.inner.advance();
        while self.inner.has_next() && self.inner.peek_index() >> self.reduction == self.index {
            self.count += self.inner.peek_count();
            self.inner.advance();
        }
    }
    fn scale(&self) -> i32 {
        self.inner.scale() - self.reduction as i32
    }
}

/// `MergingBucketIterator` with a count combination (`Long::sum`, or the signed difference
/// `estimateSum` uses).
#[derive(Clone, Copy, Debug)]
struct Merging<A, B> {
    a: ScaleReducing<A>,
    b: ScaleReducing<B>,
    end: bool,
    index: i64,
    count: i128,
    subtract: bool,
}

impl<A: BucketIter, B: BucketIter> Merging<A, B> {
    fn new(a: A, b: B, target_scale: i32, subtract: bool) -> Merging<A, B> {
        let mut m = Merging { a: ScaleReducing::new(a, target_scale), b: ScaleReducing::new(b, target_scale), end: false, index: 0, count: 0, subtract };
        m.advance();
        m
    }

    fn signed_count(&self) -> i128 {
        self.count
    }
}

impl<A: BucketIter, B: BucketIter> BucketIter for Merging<A, B> {
    fn has_next(&self) -> bool {
        !self.end
    }
    fn peek_index(&self) -> i64 {
        self.index
    }
    fn peek_count(&self) -> u64 {
        self.count as u64
    }
    fn advance(&mut self) {
        let (ha, hb) = (self.a.has_next(), self.b.has_next());
        self.end = !ha && !hb;
        if self.end {
            return;
        }
        let ia = if ha { self.a.peek_index() } else { 0 };
        let ib = if hb { self.b.peek_index() } else { 0 };
        let adv_a = ha && (!hb || ia <= ib);
        let adv_b = hb && (!ha || ib <= ia);
        let (mut ca, mut cb) = (0u64, 0u64);
        if adv_a {
            self.index = ia;
            ca = self.a.peek_count();
            self.a.advance();
        }
        if adv_b {
            self.index = ib;
            cb = self.b.peek_count();
            self.b.advance();
        }
        self.count = if self.subtract { ca as i128 - cb as i128 } else { ca as i128 + cb as i128 };
    }
    fn scale(&self) -> i32 {
        self.a.scale()
    }
}

/// An exponential histogram (`ExponentialHistogram`): buckets ascending by index.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpHistogram {
    pub scale: i32,
    pub zero: ZeroBucket,
    pub negative: Vec<(i64, u64)>,
    pub positive: Vec<(i64, u64)>,
    pub sum: f64,
    /// NaN when unknown (an empty histogram).
    pub min: f64,
    pub max: f64,
}

impl Default for ExpHistogram {
    fn default() -> Self {
        ExpHistogram::empty()
    }
}

impl ExpHistogram {
    /// `ExponentialHistogram.empty()`.
    pub fn empty() -> ExpHistogram {
        ExpHistogram {
            scale: MAX_SCALE,
            zero: ZeroBucket::minimal_empty(),
            negative: Vec::new(),
            positive: Vec::new(),
            sum: 0.0,
            min: f64::NAN,
            max: f64::NAN,
        }
    }

    fn clear(&mut self) {
        self.scale = MAX_SCALE;
        self.zero = ZeroBucket::minimal_empty();
        self.negative.clear();
        self.positive.clear();
        self.sum = 0.0;
        self.min = f64::NAN;
        self.max = f64::NAN;
    }

    pub fn negative_count(&self) -> u64 {
        self.negative.iter().map(|b| b.1).sum()
    }

    pub fn positive_count(&self) -> u64 {
        self.positive.iter().map(|b| b.1).sum()
    }

    /// `valueCount()`.
    pub fn value_count(&self) -> u64 {
        self.zero.count + self.negative_count() + self.positive_count()
    }

    /// `isEmpty()`.
    pub fn is_empty(&self) -> bool {
        self.negative.is_empty() && self.positive.is_empty() && self.zero.count == 0
    }

    pub fn bucket_count(&self) -> usize {
        self.negative.len() + self.positive.len()
    }

    fn neg_iter(&self) -> SliceIter<'_> {
        SliceIter::new(&self.negative, self.scale)
    }

    fn pos_iter(&self) -> SliceIter<'_> {
        SliceIter::new(&self.positive, self.scale)
    }

    /// The bucket centers `histogram` and `range` aggregate (`forEachBucketCenter`): negative
    /// buckets (descending values), the zero bucket, positive buckets; each center clamped to
    /// `[min, max]`, with the bucket's count.
    pub fn centers(&self, mut f: impl FnMut(f64, u64)) {
        for &(i, c) in &self.negative {
            f(clamp(-point_of_least_relative_error(i, self.scale), self.min, self.max), c);
        }
        if self.zero.count > 0 {
            f(0.0, self.zero.count);
        }
        for &(i, c) in &self.positive {
            f(clamp(point_of_least_relative_error(i, self.scale), self.min, self.max), c);
        }
    }

    /// `ExponentialHistogramState.centroids()`: unclamped bucket centers, ascending.
    pub fn centroids(&self) -> Vec<(f64, u64)> {
        let mut out = Vec::with_capacity(self.bucket_count() + 1);
        for &(i, c) in self.negative.iter().rev() {
            out.push((-point_of_least_relative_error(i, self.scale), c));
        }
        if self.zero.count > 0 {
            out.push((0.0, self.zero.count));
        }
        for &(i, c) in &self.positive {
            out.push((point_of_least_relative_error(i, self.scale), c));
        }
        out
    }

    /// Bytes held (for memory accounting).
    pub fn ram_bytes(&self) -> usize {
        96 + (self.negative.capacity() + self.positive.capacity()) * 16
    }
}

/// `ExponentialHistogramUtils.estimateSum`: bucket centers times the signed counts.
pub fn estimate_sum(negative: &[(i64, u64)], positive: &[(i64, u64)], scale: i32) -> f64 {
    let mut it = Merging::new(SliceIter::new(positive, scale), SliceIter::new(negative, scale), scale, true);
    let mut sum = 0.0;
    while it.has_next() {
        let c = it.signed_count();
        let mid = point_of_least_relative_error(it.peek_index(), it.scale());
        if c != 0 {
            let add = mid * c as i64 as f64;
            if add.is_finite() {
                sum += add;
            } else {
                sum = add;
            }
        }
        it.advance();
    }
    sum
}

/// `ExponentialHistogramUtils.estimateMin`.
pub fn estimate_min(zero: &ZeroBucket, negative: &[(i64, u64)], positive: &[(i64, u64)], scale: i32) -> Option<f64> {
    if let Some(&(i, _)) = negative.last() {
        return Some(-upper_boundary(i, scale));
    }
    if zero.count > 0 {
        return Some(if zero.zero_threshold() == 0.0 { 0.0 } else { -zero.zero_threshold() });
    }
    positive.first().map(|&(i, _)| lower_boundary(i, scale))
}

/// `ExponentialHistogramUtils.estimateMax`.
pub fn estimate_max(zero: &ZeroBucket, negative: &[(i64, u64)], positive: &[(i64, u64)], scale: i32) -> Option<f64> {
    if let Some(&(i, _)) = positive.last() {
        return Some(upper_boundary(i, scale));
    }
    if zero.count > 0 {
        return Some(zero.zero_threshold());
    }
    negative.first().map(|&(i, _)| -lower_boundary(i, scale))
}

/// `ExponentialHistogramMerger`: merges histograms into one of at most `limit` buckets,
/// lowering the scale as little as possible when they don't fit. Buffers are reused.
#[derive(Clone, Debug)]
pub struct Merger {
    limit: usize,
    max_scale: i32,
    result: Option<ExpHistogram>,
    spare: ExpHistogram,
    collapsed: [u32; MAX_INDEX_BITS as usize],
}

static EMPTY: std::sync::LazyLock<ExpHistogram> = std::sync::LazyLock::new(ExpHistogram::empty);

impl Merger {
    /// `ExponentialHistogramMerger.create(bucketLimit)` (`bucketLimit >= 4`).
    pub fn new(limit: usize) -> Merger {
        Merger::with_max_scale(limit, MAX_SCALE)
    }

    pub fn with_max_scale(limit: usize, max_scale: i32) -> Merger {
        assert!(limit >= 4, "The bucket limit must be at least 4");
        Merger { limit, max_scale, result: None, spare: ExpHistogram::empty(), collapsed: [0; MAX_INDEX_BITS as usize] }
    }

    /// `get()`.
    pub fn get(&self) -> &ExpHistogram {
        self.result.as_ref().unwrap_or(&EMPTY)
    }

    /// Whether nothing was added.
    pub fn is_empty(&self) -> bool {
        self.result.is_none()
    }

    pub fn ram_bytes(&self) -> usize {
        64 + self.spare.ram_bytes() + self.result.as_ref().map_or(0, |r| r.ram_bytes())
    }

    /// `add(histogram)`.
    pub fn add(&mut self, b: &ExpHistogram) {
        if b.is_empty() {
            return;
        }
        let a = self.result.take();
        let mut out = std::mem::take(&mut self.spare);
        {
            let a_ref = a.as_ref().unwrap_or(&EMPTY);
            self.merge_into(a_ref, b, &mut out);
        }
        self.result = Some(out);
        if let Some(mut old) = a {
            old.clear();
            self.spare = old;
        }
    }

    /// `setToMerged(a, b)` into `out`.
    fn merge_into(&mut self, a: &ExpHistogram, b: &ExpHistogram, out: &mut ExpHistogram) {
        let zero = a.zero.merge(&b.zero);
        out.sum = a.sum + b.sum;
        out.min = nan_aware(a.min, b.min, crate::metrics::simple::java_min);
        out.max = nan_aware(a.max, b.max, crate::metrics::simple::java_max);
        let mut target = self.max_scale.min(a.scale.min(b.scale));

        let mut pos = Merging::new(a.pos_iter(), b.pos_iter(), target, false);
        let mut neg = Merging::new(a.neg_iter(), b.neg_iter(), target, false);
        out.zero = zero.collapse_all(&mut pos, &mut neg);
        out.scale = target;
        out.negative.clear();
        out.positive.clear();
        self.collapsed = [0; MAX_INDEX_BITS as usize];
        let mut overflow = put_buckets(out, &mut neg, false, self.limit, Some(&mut self.collapsed));
        overflow += put_buckets(out, &mut pos, true, self.limit, Some(&mut self.collapsed));
        if overflow > 0 {
            target -= required_reduction(&self.collapsed, overflow);
            out.scale = target;
            out.negative.clear();
            out.positive.clear();
            let mut pos = Merging::new(a.pos_iter(), b.pos_iter(), target, false);
            let mut neg = Merging::new(a.neg_iter(), b.neg_iter(), target, false);
            out.zero = zero.collapse_all(&mut pos, &mut neg);
            let overflow = put_buckets(out, &mut neg, false, self.limit, None) + put_buckets(out, &mut pos, true, self.limit, None);
            debug_assert_eq!(overflow, 0);
        }
    }
}

fn nan_aware(a: f64, b: f64, f: fn(f64, f64) -> f64) -> f64 {
    if a.is_nan() {
        b
    } else if b.is_nan() {
        a
    } else {
        f(a, b)
    }
}

/// `putBuckets`: adds what fits in `limit` slots (negatives first), counting the rest, and
/// records for each pair of neighbours the scale reduction that would merge them.
fn put_buckets<I: BucketIter>(out: &mut ExpHistogram, it: &mut I, positive: bool, limit: usize, mut stats: Option<&mut [u32; 62]>) -> usize {
    let mut collect_next = false;
    let mut prev = 0i64;
    let mut overflow = 0;
    while it.has_next() {
        let count = it.peek_count();
        if count == 0 {
            it.advance();
            continue;
        }
        let idx = it.peek_index();
        if collect_next {
            if let Some(s) = stats.as_deref_mut() {
                // DownscaleStats.add
                let xor = prev ^ idx;
                let lz = xor.leading_zeros();
                if lz > 0 {
                    s[(64 - lz - 1) as usize] += 1;
                }
            }
        } else {
            collect_next = stats.is_some();
        }
        if out.negative.len() + out.positive.len() >= limit {
            overflow += 1;
        } else if positive {
            out.positive.push((idx, count));
        } else {
            out.negative.push((idx, count));
        }
        prev = idx;
        it.advance();
    }
    overflow
}

/// `DownscaleStats.getRequiredScaleReductionToReduceBucketCountBy`.
fn required_reduction(collapsed: &[u32; 62], desired: usize) -> i32 {
    let mut total = 0usize;
    for (i, &c) in collapsed.iter().enumerate() {
        total += c as usize;
        if total >= desired {
            return i as i32 + 1;
        }
    }
    // Unreachable for valid indices: one more bit than any index has merges everything.
    MAX_INDEX_BITS as i32
}

/// `ExponentialHistogramQuantile.getQuantile`.
pub fn quantile(h: &ExpHistogram, q: f64) -> f64 {
    let zero = h.zero.count;
    let neg = h.negative_count();
    let pos = h.positive_count();
    let total = zero + neg + pos;
    if total == 0 {
        return f64::NAN;
    }
    let exact = q * (total - 1) as f64;
    let lower = exact.floor() as i64;
    let upper = exact.ceil() as i64;
    let factor = exact - lower as f64;
    let (prev, at) = element_at_rank(h, upper.max(0) as u64, neg, zero);
    let (prev, at) = (clamp(prev, h.min, h.max), clamp(at, h.min, h.max));
    let r = if lower == upper { at } else { prev + (at - prev) * factor };
    if r == 0.0 { 0.0 } else { r }
}

/// `getElementAtRank`: (value at rank - 1, value at rank).
fn element_at_rank(h: &ExpHistogram, rank: u64, neg: u64, zero: u64) -> (f64, f64) {
    let s = h.scale;
    let first_neg = || h.negative.first().map_or(f64::NAN, |&(i, _)| point_of_least_relative_error(i, s));
    if rank < neg {
        if rank == 0 {
            let last = h.negative.last().map_or(f64::NAN, |&(i, _)| point_of_least_relative_error(i, s));
            return (f64::NAN, -last);
        }
        let (p, v) = midpoint_for_rank(&h.negative, s, neg - rank);
        return (-v, -p);
    }
    if rank < neg + zero {
        if rank == neg {
            return (-first_neg(), 0.0);
        }
        return (0.0, 0.0);
    }
    let (p, v) = midpoint_for_rank(&h.positive, s, rank - neg - zero);
    if rank.wrapping_sub(1) < neg && rank > 0 {
        (-first_neg(), v)
    } else if rank > 0 && rank - 1 < neg + zero {
        (0.0, v)
    } else {
        (p, v)
    }
}

/// `getBucketMidpointForRank`: (previous center, center) of the element at `rank`.
fn midpoint_for_rank(buckets: &[(i64, u64)], scale: i32, rank: u64) -> (f64, f64) {
    let mut prev_index = i64::MIN;
    let mut seen = 0u64;
    for &(i, c) in buckets {
        seen += c;
        if rank < seen {
            let center = point_of_least_relative_error(i, scale);
            let prev = if rank > 0 {
                if rank > seen - c { center } else { point_of_least_relative_error(prev_index, scale) }
            } else {
                f64::NAN
            };
            return (prev, center);
        }
        prev_index = i;
    }
    // Ranks come from the histogram's own counts.
    (f64::NAN, f64::NAN)
}

/// `ExponentialHistogramQuantile.estimateRank`: values below (or at, `inclusive`) `value`.
pub fn estimate_rank(h: &ExpHistogram, value: f64, inclusive: bool) -> u64 {
    if h.min.is_nan() || value < h.min {
        return 0;
    }
    if value > h.max {
        return h.value_count();
    }
    if value >= 0.0 {
        let mut rank = h.negative_count();
        if value > 0.0 || inclusive {
            rank += h.zero.count;
        }
        rank + rank_in(&h.positive, h.scale, value, inclusive, h.min, h.max)
    } else {
        let greater = rank_in(&h.negative, h.scale, -value, !inclusive, -h.max, -h.min);
        h.negative_count() - greater
    }
}

fn rank_in(buckets: &[(i64, u64)], scale: i32, value: f64, inclusive: bool, min: f64, max: f64) -> u64 {
    let mut rank = 0;
    for &(i, c) in buckets {
        let mid = clamp(point_of_least_relative_error(i, scale), min, max);
        if mid < value || (inclusive && mid == value) {
            rank += c;
        } else {
            break;
        }
    }
    rank
}

/// `ExponentialHistogramState.cdf`: equal values get half credit.
pub fn cdf(h: &ExpHistogram, x: f64) -> f64 {
    let less = estimate_rank(h, x, false);
    let less_eq = estimate_rank(h, x, true);
    let equal = less_eq - less;
    (less as f64 + equal as f64 / 2.0) / h.value_count() as f64
}

/// `TDigestToExponentialHistogramConverter.convert` (+ `ExponentialHistogramBuilder.build`):
/// ascending `(mean, count)` centroids into scale-38 buckets, zero centroids into the zero
/// bucket, sum/min/max estimated from the buckets.
pub fn from_centroids(centroids: &[(f64, u64)]) -> ExpHistogram {
    let scale = MAX_SCALE;
    let first_non_negative = centroids.iter().position(|c| c.0 >= 0.0 || c.0.is_nan()).unwrap_or(centroids.len());
    let mut zero_count = 0;
    let mut k = first_non_negative;
    while k < centroids.len() && centroids[k].0 == 0.0 {
        zero_count += centroids[k].1;
        k += 1;
    }
    let group = |iter: &mut dyn Iterator<Item = &(f64, u64)>, out: &mut Vec<(i64, u64)>| {
        for &(v, c) in iter {
            if c == 0 {
                continue;
            }
            let idx = compute_index(v, scale);
            match out.last_mut() {
                Some(last) if last.0 == idx => last.1 += c,
                _ => out.push((idx, c)),
            }
        }
    };
    let mut h = ExpHistogram::empty();
    group(&mut centroids[..first_non_negative].iter().rev(), &mut h.negative);
    group(&mut centroids[k..].iter(), &mut h.positive);
    h.scale = scale;
    h.zero = ZeroBucket::with_threshold(0.0, zero_count);
    h.sum = estimate_sum(&h.negative, &h.positive, scale);
    h.min = estimate_min(&h.zero, &h.negative, &h.positive, scale).unwrap_or(f64::NAN);
    h.max = estimate_max(&h.zero, &h.negative, &h.positive, scale).unwrap_or(f64::NAN);
    h
}

// ------------------------------------------------------------------ doc values

/// What `search-es` indexes for one `exponential_histogram` value (validated as
/// Elasticsearch's `ExponentialHistogramParser` does: scale in range, non-negative finite zero
/// threshold, positive counts, unique indices in `[MIN_INDEX, MAX_INDEX]`).
#[derive(Clone, Debug, Default)]
pub struct ExpHistogramValue {
    pub scale: i32,
    pub zero_threshold: f64,
    pub zero_count: u64,
    pub negative: Vec<(i64, u64)>,
    pub positive: Vec<(i64, u64)>,
    /// `None`: estimated from the buckets (`validateOrEstimateSum`).
    pub sum: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

const HAS_MIN: u8 = 1;
const HAS_MAX: u8 = 2;

fn put_vlong(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_zlong(out: &mut Vec<u8>, v: i64) {
    put_vlong(out, ((v << 1) ^ (v >> 63)) as u64);
}

fn put_buckets_bytes(out: &mut Vec<u8>, buckets: &[(i64, u64)]) {
    let mut prev = 0i64;
    for (k, &(i, c)) in buckets.iter().enumerate() {
        if k == 0 {
            put_zlong(out, i);
            put_vlong(out, c);
        } else {
            let delta = i - prev;
            if delta > 1 {
                put_zlong(out, -delta);
            }
            put_zlong(out, c as i64);
        }
        prev = i;
    }
}

/// The binary doc value of an `exponential_histogram` field. Buckets are sorted by index;
/// absent `sum` / `min` / `max` are estimated as Elasticsearch does when indexing (an empty
/// histogram has sum 0 and no min or max).
pub fn encode(v: &ExpHistogramValue) -> Vec<u8> {
    let mut neg = v.negative.clone();
    let mut pos = v.positive.clone();
    neg.sort_by_key(|b| b.0);
    pos.sort_by_key(|b| b.0);
    let zero = ZeroBucket::with_threshold(v.zero_threshold, v.zero_count);
    let count = v.zero_count + neg.iter().map(|b| b.1).sum::<u64>() + pos.iter().map(|b| b.1).sum::<u64>();
    let sum = v.sum.unwrap_or_else(|| estimate_sum(&neg, &pos, v.scale));
    let min = v.min.or_else(|| estimate_min(&zero, &neg, &pos, v.scale));
    let max = v.max.or_else(|| estimate_max(&zero, &neg, &pos, v.scale));
    let mut out = Vec::with_capacity(32 + (neg.len() + pos.len()) * 3);
    out.push(v.scale as i8 as u8);
    out.push(if min.is_some() { HAS_MIN } else { 0 } | if max.is_some() { HAS_MAX } else { 0 });
    out.extend_from_slice(&v.zero_threshold.to_le_bytes());
    put_vlong(&mut out, count);
    out.extend_from_slice(&(if count > 0 { sum } else { 0.0 }).to_le_bytes());
    if let Some(m) = min {
        out.extend_from_slice(&m.to_le_bytes());
    }
    if let Some(m) = max {
        out.extend_from_slice(&m.to_le_bytes());
    }
    put_vlong(&mut out, neg.len() as u64);
    put_vlong(&mut out, pos.len() as u64);
    put_buckets_bytes(&mut out, &neg);
    put_buckets_bytes(&mut out, &pos);
    out
}

fn corrupt() -> AggError {
    AggError::new(500, "illegal_state_exception", "Malformed exponential histogram bytes")
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn byte(&mut self) -> Result<u8> {
        let x = *self.b.get(self.pos).ok_or_else(corrupt)?;
        self.pos += 1;
        Ok(x)
    }
    fn f64(&mut self) -> Result<f64> {
        let s = self.b.get(self.pos..self.pos + 8).ok_or_else(corrupt)?;
        self.pos += 8;
        Ok(f64::from_le_bytes(s.try_into().map_err(|_| corrupt())?))
    }
    fn vlong(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let x = self.byte()?;
            if shift > 63 {
                return Err(corrupt());
            }
            v |= ((x & 0x7f) as u64) << shift;
            if x & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }
    fn zlong(&mut self) -> Result<i64> {
        let v = self.vlong()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }
}

/// The stored summary of a doc's histogram (what `sum`/`avg`/`min`/`max`/`value_count` read).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub value_count: u64,
    pub sum: f64,
    /// NaN when absent (empty histogram).
    pub min: f64,
    pub max: f64,
}

struct Header {
    scale: i32,
    zero_threshold: f64,
    summary: Summary,
    neg: usize,
    pos: usize,
}

fn header(c: &mut Cursor<'_>) -> Result<Header> {
    let scale = c.byte()? as i8 as i32;
    if !(MIN_SCALE..=MAX_SCALE).contains(&scale) {
        return Err(corrupt());
    }
    let flags = c.byte()?;
    let zero_threshold = c.f64()?;
    let value_count = c.vlong()?;
    let sum = c.f64()?;
    let min = if flags & HAS_MIN != 0 { c.f64()? } else { f64::NAN };
    let max = if flags & HAS_MAX != 0 { c.f64()? } else { f64::NAN };
    let neg = c.vlong()? as usize;
    let pos = c.vlong()? as usize;
    // Every bucket takes at least two bytes.
    if neg.saturating_add(pos) > c.b.len() {
        return Err(corrupt());
    }
    Ok(Header { scale, zero_threshold, summary: Summary { value_count, sum, min, max }, neg, pos })
}

/// The stored summary of an encoded histogram.
pub fn decode_summary(b: &[u8]) -> Result<Summary> {
    Ok(header(&mut Cursor { b, pos: 0 })?.summary)
}

fn read_buckets(c: &mut Cursor<'_>, n: usize, out: &mut Vec<(i64, u64)>) -> Result<()> {
    out.clear();
    if n == 0 {
        return Ok(());
    }
    let mut index = c.zlong()?;
    let mut count = c.vlong()?;
    out.push((index, count));
    for _ in 1..n {
        let d = c.zlong()?;
        if d < 0 {
            index = index.checked_add(d.checked_neg().ok_or_else(corrupt)?).ok_or_else(corrupt)?;
            count = c.zlong()? as u64;
        } else {
            index = index.checked_add(1).ok_or_else(corrupt)?;
            count = d as u64;
        }
        out.push((index, count));
    }
    Ok(())
}

/// Decodes an encoded histogram into `h` (buffers reused).
pub fn decode_into(b: &[u8], h: &mut ExpHistogram) -> Result<()> {
    let mut c = Cursor { b, pos: 0 };
    let hd = header(&mut c)?;
    read_buckets(&mut c, hd.neg, &mut h.negative)?;
    read_buckets(&mut c, hd.pos, &mut h.positive)?;
    h.scale = hd.scale;
    h.sum = hd.summary.sum;
    h.min = hd.summary.min;
    h.max = hd.summary.max;
    let buckets = h.negative_count().wrapping_add(h.positive_count());
    h.zero = ZeroBucket::with_threshold(hd.zero_threshold, hd.summary.value_count.wrapping_sub(buckets));
    Ok(())
}

/// A segment's `exponential_histogram` doc values.
pub struct ExpHistogramReader {
    dv: Option<search_codec::BinaryDocValues>,
    cur: ExpHistogram,
}

impl ExpHistogramReader {
    pub fn open(seg: &search_index::SegmentContext, field: Option<u32>) -> ExpHistogramReader {
        ExpHistogramReader { dv: field.and_then(|f| seg.reader.binary(f).cloned()), cur: ExpHistogram::empty() }
    }

    /// The doc's summary, without decoding buckets.
    pub fn summary(&self, doc: u32) -> Result<Option<Summary>> {
        match self.dv.as_ref().and_then(|d| d.get(doc)) {
            Some(b) => Ok(Some(decode_summary(b)?)),
            None => Ok(None),
        }
    }

    /// The doc's histogram.
    pub fn histogram(&mut self, doc: u32) -> Result<Option<&ExpHistogram>> {
        match self.dv.as_ref().and_then(|d| d.get(doc)) {
            Some(b) => {
                decode_into(b, &mut self.cur)?;
                Ok(Some(&self.cur))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(scale: i32, zero: (f64, u64), neg: &[(i64, u64)], pos: &[(i64, u64)], sum: Option<f64>, min: Option<f64>, max: Option<f64>) -> ExpHistogram {
        let v = ExpHistogramValue { scale, zero_threshold: zero.0, zero_count: zero.1, negative: neg.to_vec(), positive: pos.to_vec(), sum, min, max };
        let mut h = ExpHistogram::empty();
        decode_into(&encode(&v), &mut h).unwrap();
        h
    }

    #[test]
    fn index_math() {
        // Scale 0: exact powers of two are the upper bounds of their bucket.
        assert_eq!(compute_index(1.0, 0), -1);
        assert_eq!(compute_index(1.5, 0), 0);
        assert_eq!(compute_index(2.0, 0), 0);
        assert_eq!(compute_index(2.0000001, 0), 1);
        assert_eq!(compute_index(-3.0, 0), 1);
        assert_eq!(compute_index(f64::MIN_POSITIVE, 0), -1023);
        assert_eq!(compute_index(f64::MIN_POSITIVE / 4.0, 0), -1025);
        assert_eq!(compute_index(8.0, -1), 1);
        assert_eq!(compute_index(1.5, 2), 2);
        assert_eq!(lower_boundary(0, 0), 1.0);
        assert_eq!(upper_boundary(0, 0), 2.0);
        assert_eq!(point_of_least_relative_error(0, 0), 4.0 / 3.0);
        assert_eq!(point_of_least_relative_error(5, -11), f64::INFINITY);
        assert_eq!(point_of_least_relative_error(-5, -11), 0.0);
        use std::cmp::Ordering::*;
        assert_eq!(compare_scaled(1, 0, 2, 1), Equal);
        assert_eq!(compare_scaled(1, 0, 3, 1), Less);
        assert_eq!(compare_scaled(3, 1, 1, 0), Greater);
        assert_eq!(compare_scaled(-1, 0, -3, 1), Greater);
    }

    #[test]
    fn doc_value_round_trip_and_estimates() {
        let h = hist(2, (0.0, 1), &[], &[(3, 3), (0, 2)], Some(10.0), Some(1.0), Some(5.0));
        assert_eq!(h.positive, [(0, 2), (3, 3)]);
        assert_eq!((h.value_count(), h.zero.count, h.sum, h.min, h.max), (6, 1, 10.0, 1.0, 5.0));
        // The reference node: percentiles 1/50/99 of this doc.
        assert_eq!(quantile(&h, 0.01), 1.0043213616862945);
        assert_eq!(quantile(&h, 0.5), 1.4567863831370553);
        assert_eq!(quantile(&h, 0.99), 1.8271455325482207);
        assert_eq!(cdf(&h, 1.0) * 100.0, 16.666666666666664);
        let e = hist(0, (0.5, 0), &[], &[], None, None, None);
        assert!(e.is_empty() && e.min.is_nan() && e.max.is_nan() && e.sum == 0.0);
        let est = hist(0, (0.0, 0), &[(1, 1)], &[(0, 2), (2, 1)], None, None, None);
        // Bounds come from exp(ln 2 * index): 2^3 is 7.999999999999998, as in Java.
        assert_eq!(est.min, -upper_boundary(1, 0));
        assert_eq!(est.max, upper_boundary(2, 0));
        assert!((est.sum - (2.0 * 4.0 / 3.0 + 16.0 / 3.0 - 8.0 / 3.0)).abs() < 1e-12);
        assert!(decode_into(&[2, 0, 1], &mut ExpHistogram::empty()).is_err());
        assert!(decode_summary(&[99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn merge_downscales_to_fit() {
        let a = hist(3, (0.0, 0), &[], &(0..6).map(|i| (i * 2, 1)).collect::<Vec<_>>(), None, None, None);
        let b = hist(3, (0.0, 0), &[], &(0..6).map(|i| (i * 2 + 1, 1)).collect::<Vec<_>>(), None, None, None);
        let mut m = Merger::new(6);
        m.add(&a);
        m.add(&b);
        let r = m.get();
        // 12 neighbours at scale 3 → scale 2 merges pairs: 6 buckets.
        assert_eq!(r.scale, 2);
        assert_eq!(r.positive, (0..6).map(|i| (i, 2)).collect::<Vec<_>>());
        assert_eq!(r.value_count(), 12);
        assert_eq!(r.min, a.min.min(b.min));
    }

    #[test]
    fn zero_bucket_collapses_overlapping_buckets() {
        let a = hist(0, (3.0, 2), &[(0, 1)], &[(0, 4), (1, 1), (5, 1)], Some(1.0), None, None);
        let mut m = Merger::new(320);
        m.add(&a);
        let r = m.get();
        // |v| <= 3 swallows (1,2] and (2,4]: the threshold becomes the bucket bound 4.
        assert_eq!(r.zero.count, 2 + 1 + 4 + 1);
        assert_eq!(r.positive, [(5, 1)]);
        assert!(r.negative.is_empty());
        assert_eq!(r.zero.zero_threshold(), 4.0);
    }

    #[test]
    fn converts_centroids() {
        let h = from_centroids(&[(-3.0, 2), (-1.0, 1), (0.0, 4), (1.0, 1), (1.0000000001, 3), (7.0, 0)]);
        assert_eq!(h.zero.count, 4);
        assert_eq!(h.negative.len(), 2);
        assert_eq!(h.positive.len(), 2);
        assert_eq!(h.value_count(), 11);
        assert!(h.min <= -3.0 && h.max >= 1.0000000001);
    }
}

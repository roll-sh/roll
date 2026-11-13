//! HdrHistogram's `DoubleHistogram` (version 2.1.9, the one Elasticsearch ships) for the `hdr`
//! method of `percentiles` and `percentile_ranks`.
//!
//! A double histogram is an integer histogram (log-linear buckets with `10^digits` precision)
//! plus a conversion ratio that slides as values arrive: the covered range starts empty
//! (`[2^800, 2^803)`) and is shifted by whole binary orders of magnitude, shifting the recorded
//! counts with it (a rotation of the counts array through the normalizing index offset), and the
//! integer histogram grows when a shift would overflow. Every step mirrors the Java library,
//! including its quirks, because percentiles read the bucket boundaries directly.

use crate::error::{AggError, Result};

/// The integer counts histogram (`org.HdrHistogram.Histogram`).
#[derive(Clone, Debug)]
struct IntHistogram {
    highest_trackable_value: i64,
    digits: u32,
    sub_bucket_half_count_magnitude: u32,
    sub_bucket_count: i32,
    sub_bucket_half_count: i32,
    sub_bucket_mask: i64,
    leading_zero_count_base: i32,
    counts_array_length: i32,
    bucket_count: i32,
    counts: Vec<i64>,
    normalizing_index_offset: i32,
    total_count: i64,
    max_value: i64,
    min_non_zero_value: i64,
}

type Aioobe = String;

/// `AbstractHistogram.numberOfSubbuckets`.
fn number_of_subbuckets(digits: u32) -> i64 {
    let largest = 2 * 10i64.pow(digits);
    let mag = ((largest as f64).ln() / 2f64.ln()).ceil() as u32;
    1i64 << mag
}

impl IntHistogram {
    /// `Histogram(1, highestTrackableValue, digits)`.
    fn new(highest_trackable_value: i64, digits: u32) -> IntHistogram {
        let largest = 2 * 10i64.pow(digits);
        let sub_bucket_count_magnitude = ((largest as f64).ln() / 2f64.ln()).ceil() as u32;
        let half_mag = sub_bucket_count_magnitude.max(1) - 1;
        let sub_bucket_count = 1i32 << (half_mag + 1);
        let mut h = IntHistogram {
            highest_trackable_value,
            digits,
            sub_bucket_half_count_magnitude: half_mag,
            sub_bucket_count,
            sub_bucket_half_count: sub_bucket_count / 2,
            sub_bucket_mask: (sub_bucket_count as i64) - 1,
            leading_zero_count_base: 64 - half_mag as i32 - 1,
            counts_array_length: 0,
            bucket_count: 0,
            counts: Vec::new(),
            normalizing_index_offset: 0,
            total_count: 0,
            max_value: 0,
            min_non_zero_value: i64::MAX,
        };
        h.establish_size(highest_trackable_value);
        h.counts = vec![0; h.counts_array_length as usize];
        h
    }

    fn establish_size(&mut self, highest: i64) {
        self.bucket_count = self.buckets_needed(highest);
        self.counts_array_length = (self.bucket_count + 1) * (self.sub_bucket_count / 2);
        self.highest_trackable_value = highest;
    }

    fn buckets_needed(&self, value: i64) -> i32 {
        let mut smallest_untrackable = self.sub_bucket_count as i64;
        let mut needed = 1;
        while smallest_untrackable <= value {
            if smallest_untrackable > i64::MAX / 2 {
                return needed + 1;
            }
            smallest_untrackable <<= 1;
            needed += 1;
        }
        needed
    }

    #[inline]
    fn bucket_index(&self, value: i64) -> i32 {
        self.leading_zero_count_base - (value | self.sub_bucket_mask).leading_zeros() as i32
    }

    #[inline]
    fn sub_bucket_index(value: i64, bucket: i32) -> i32 {
        ((value as u64) >> bucket) as i32
    }

    #[inline]
    fn counts_index(&self, value: i64) -> i32 {
        let b = self.bucket_index(value);
        let s = Self::sub_bucket_index(value, b);
        ((b + 1) << self.sub_bucket_half_count_magnitude) + (s - self.sub_bucket_half_count)
    }

    #[inline]
    fn normalize(&self, index: i32) -> std::result::Result<usize, Aioobe> {
        if self.normalizing_index_offset == 0 {
            return Ok(index as usize);
        }
        let len = self.counts_array_length;
        if index > len || index < 0 {
            return Err("index out of covered value range".into());
        }
        let mut n = index - self.normalizing_index_offset;
        if n < 0 {
            n += len;
        } else if n >= len {
            n -= len;
        }
        Ok(n as usize)
    }

    fn count_at(&self, index: i32) -> std::result::Result<i64, Aioobe> {
        let n = self.normalize(index)?;
        self.counts.get(n).copied().ok_or_else(|| format!("Index {index} out of bounds for length {}", self.counts.len()))
    }

    fn add_at(&mut self, index: i32, v: i64) -> std::result::Result<(), Aioobe> {
        let n = self.normalize(index)?;
        let len = self.counts.len();
        match self.counts.get_mut(n) {
            Some(c) => {
                *c += v;
                Ok(())
            }
            None => Err(format!("Index {index} out of bounds for length {len}")),
        }
    }

    fn set_at(&mut self, index: i32, v: i64) -> std::result::Result<(), Aioobe> {
        let n = self.normalize(index)?;
        self.counts[n] = v;
        Ok(())
    }

    fn value_from_parts(bucket: i32, sub: i32) -> i64 {
        (sub as i64) << bucket
    }

    fn value_from_index(&self, index: i32) -> i64 {
        let mut bucket = (index >> self.sub_bucket_half_count_magnitude) - 1;
        let mut sub = (index & (self.sub_bucket_half_count - 1)) + self.sub_bucket_half_count;
        if bucket < 0 {
            sub -= self.sub_bucket_half_count;
            bucket = 0;
        }
        Self::value_from_parts(bucket, sub)
    }

    fn size_of_equivalent_range(&self, value: i64) -> i64 {
        let b = self.bucket_index(value);
        let s = Self::sub_bucket_index(value, b);
        1i64 << (if s >= self.sub_bucket_count { b + 1 } else { b })
    }

    fn lowest_equivalent(&self, value: i64) -> i64 {
        let b = self.bucket_index(value);
        let s = Self::sub_bucket_index(value, b);
        Self::value_from_parts(b, s)
    }

    fn highest_equivalent(&self, value: i64) -> i64 {
        self.lowest_equivalent(value) + self.size_of_equivalent_range(value) - 1
    }

    fn update_min_max(&mut self, value: i64) {
        if value > self.max_value {
            self.max_value = value;
        }
        if value < self.min_non_zero_value && value != 0 && value > 0 {
            self.min_non_zero_value = value;
        }
    }

    fn max(&self) -> i64 {
        if self.max_value == 0 { 0 } else { self.highest_equivalent(self.max_value) }
    }

    fn min_non_zero(&self) -> i64 {
        if self.min_non_zero_value == i64::MAX { i64::MAX } else { self.lowest_equivalent(self.min_non_zero_value) }
    }

    fn record(&mut self, value: i64, count: i64) -> std::result::Result<(), Aioobe> {
        if value < 0 {
            return Err("Histogram recorded value cannot be negative.".into());
        }
        let idx = self.counts_index(value);
        if let Err(e) = self.add_at(idx, count) {
            // The internal histogram doesn't auto-resize.
            return Err(format!("value outside of histogram covered range. Caused by: java.lang.ArrayIndexOutOfBoundsException: {e}"));
        }
        self.update_min_max(value);
        self.total_count += count;
        Ok(())
    }

    /// `Histogram.resize`.
    fn resize(&mut self, new_highest: i64) {
        let old_zero = self.normalize(0).unwrap_or(0);
        self.establish_size(new_highest);
        let len = self.counts_array_length as usize;
        let delta = len - self.counts.len();
        self.counts.resize(len, 0);
        if old_zero != 0 {
            let new_zero = old_zero + delta;
            let length_to_copy = (len - delta) - old_zero;
            self.counts.copy_within(old_zero..old_zero + length_to_copy, new_zero);
            self.counts[old_zero..new_zero].fill(0);
        }
    }

    fn shift_left(&mut self, n: u32) -> std::result::Result<(), Aioobe> {
        if n == 0 || self.total_count == self.count_at(0)? {
            return Ok(());
        }
        let shift = (n << self.sub_bucket_half_count_magnitude) as i32;
        let max_index = self.counts_index(self.max());
        if max_index >= self.counts_array_length - shift {
            return Err("Operation would overflow, would discard recorded value counts".into());
        }
        let max_before = std::mem::replace(&mut self.max_value, 0);
        let min_before = std::mem::replace(&mut self.min_non_zero_value, i64::MAX);
        let lowest_half_populated = min_before < self.sub_bucket_half_count as i64;
        // nonConcurrentNormalizingIndexShift
        let zero_count = self.count_at(0)?;
        self.set_at(0, 0)?;
        self.normalizing_index_offset += shift;
        if lowest_half_populated {
            let orders = shift >> self.sub_bucket_half_count_magnitude;
            for from in 1..self.sub_bucket_half_count {
                let to_value = self.value_from_index(from) << orders;
                let to = self.counts_index(to_value);
                let c = self.counts[from as usize];
                self.set_at(to, c)?;
                self.counts[from as usize] = 0;
            }
        }
        self.set_at(0, zero_count)?;
        self.update_min_max(max_before << n);
        if min_before < i64::MAX {
            self.update_min_max(min_before << n);
        }
        Ok(())
    }

    fn shift_right(&mut self, n: u32) -> std::result::Result<(), Aioobe> {
        if n == 0 || self.total_count == self.count_at(0)? {
            return Ok(());
        }
        let shift = self.sub_bucket_half_count * n as i32;
        let min_index = self.counts_index(self.min_non_zero());
        if min_index < shift + self.sub_bucket_half_count {
            return Err("Operation would underflow and lose precision of already recorded value counts".into());
        }
        let max_before = std::mem::replace(&mut self.max_value, 0);
        let min_before = std::mem::replace(&mut self.min_non_zero_value, i64::MAX);
        let zero_count = self.count_at(0)?;
        self.set_at(0, 0)?;
        self.normalizing_index_offset -= shift;
        self.set_at(0, zero_count)?;
        self.update_min_max(max_before >> n);
        if min_before < i64::MAX {
            self.update_min_max(min_before >> n);
        }
        Ok(())
    }

    fn value_at_percentile(&self, percentile: f64) -> i64 {
        let requested = percentile.min(100.0);
        let at = ((requested / 100.0) * self.total_count as f64 + 0.5) as i64;
        let at = at.max(1);
        let mut total = 0i64;
        for i in 0..self.counts_array_length {
            total += self.count_at(i).unwrap_or(0);
            if total >= at {
                let v = self.value_from_index(i);
                return if percentile == 0.0 { self.lowest_equivalent(v) } else { self.highest_equivalent(v) };
            }
        }
        0
    }

    fn percentile_at_or_below(&self, value: i64) -> f64 {
        if self.total_count == 0 {
            return 100.0;
        }
        let target = self.counts_index(value).min(self.counts_array_length - 1);
        let mut total = 0i64;
        for i in 0..=target {
            total += self.count_at(i).unwrap_or(0);
        }
        (100.0 * total as f64) / self.total_count as f64
    }
}

/// `org.HdrHistogram.DoubleHistogram` with auto-resize.
#[derive(Clone, Debug)]
pub struct DoubleHistogram {
    configured_ratio: i64,
    lowest_in_range: f64,
    highest_limit_in_range: f64,
    ints: IntHistogram,
    d2i: f64,
    i2d: f64,
}

/// `highestAllowedValueEver`.
fn highest_allowed_value_ever() -> f64 {
    let mut v = 1.0f64;
    while v < f64::MAX / 4.0 {
        v *= 2.0;
    }
    v
}

fn containing_order_long(n: i64) -> u32 {
    64 - n.leading_zeros()
}

fn containing_order_double(d: f64) -> u32 {
    containing_order_long(d.ceil() as i64)
}

/// `Math.ulp(double)` for finite values.
fn ulp(v: f64) -> f64 {
    let a = v.abs();
    if a == f64::MAX {
        return 2f64.powi(971);
    }
    f64::from_bits(a.to_bits() + 1) - a
}

impl DoubleHistogram {
    /// `new DoubleHistogram(digits)` (`setAutoResize(true)`).
    pub fn new(digits: u32) -> DoubleHistogram {
        let ratio = 2i64;
        let internal_ratio = 1i64 << (containing_order_long(ratio) + 1);
        let range = (number_of_subbuckets(digits) / 2) * internal_ratio;
        let ints = IntHistogram::new(range - 1, digits);
        let lowest = 2f64.powi(800);
        let mut h = DoubleHistogram { configured_ratio: ratio, lowest_in_range: 0.0, highest_limit_in_range: 0.0, ints, d2i: 0.0, i2d: 0.0 };
        h.set_range(lowest, lowest * internal_ratio as f64);
        h
    }

    /// `new DoubleHistogram(source)`: the source's configuration and covered range, no counts.
    pub fn empty_like(source: &DoubleHistogram) -> DoubleHistogram {
        let ints = IntHistogram::new(source.ints.highest_trackable_value, source.ints.digits);
        let mut h = DoubleHistogram { configured_ratio: source.configured_ratio, lowest_in_range: 0.0, highest_limit_in_range: 0.0, ints, d2i: 0.0, i2d: 0.0 };
        h.set_range(source.lowest_in_range, source.highest_limit_in_range);
        h
    }

    pub fn digits(&self) -> u32 {
        self.ints.digits
    }

    fn set_range(&mut self, lowest: f64, highest: f64) {
        self.lowest_in_range = lowest;
        self.highest_limit_in_range = highest;
        self.i2d = lowest / self.ints.sub_bucket_half_count as f64;
        self.d2i = 1.0 / self.i2d;
    }

    pub fn total_count(&self) -> i64 {
        self.ints.total_count
    }

    /// `recordValue`.
    pub fn record(&mut self, value: f64) -> Result<()> {
        self.record_count(value, 1)
    }

    /// `recordValueWithCount`.
    pub fn record_count(&mut self, value: f64, count: i64) -> Result<()> {
        if value < self.lowest_in_range || value >= self.highest_limit_in_range {
            self.auto_adjust(value).map_err(|m| AggError::new(500, "array_index_out_of_bounds_exception", m))?;
        }
        let iv = (value * self.d2i) as i64;
        self.ints.record(iv, count).map_err(|m| AggError::new(500, "array_index_out_of_bounds_exception", m))
    }

    fn auto_adjust(&mut self, value: f64) -> std::result::Result<(), Aioobe> {
        if value == 0.0 {
            return Ok(());
        }
        let r = (|| {
            if value < self.lowest_in_range {
                if value < 0.0 {
                    return Err("Negative values cannot be recorded".to_string());
                }
                loop {
                    let n = self.capped_order((self.lowest_in_range / value).ceil() - 1.0);
                    self.shift_range_right(n)?;
                    if value >= self.lowest_in_range {
                        break;
                    }
                }
            } else if value >= self.highest_limit_in_range {
                let max = highest_allowed_value_ever();
                if value > max {
                    return Err(format!("Values above {} cannot be recorded", crate::json::java_double(max)));
                }
                loop {
                    let n = self.capped_order(((value + ulp(value)) / self.highest_limit_in_range).ceil() - 1.0);
                    self.shift_range_left(n)?;
                    if value < self.highest_limit_in_range {
                        break;
                    }
                }
            }
            Ok(())
        })();
        r.map_err(|e| {
            format!(
                "The value {} is out of bounds for histogram, current covered range [{}, {}) cannot be extended any further.\nCaused by: java.lang.ArrayIndexOutOfBoundsException: {e}",
                crate::json::java_double(value),
                crate::json::java_double(self.lowest_in_range),
                crate::json::java_double(self.highest_limit_in_range)
            )
        })
    }

    fn capped_order(&self, d: f64) -> u32 {
        if d > self.configured_ratio as f64 {
            return ((self.configured_ratio as f64).ln() / 2f64.ln()) as u32;
        }
        if d > 2f64.powi(50) {
            return 50;
        }
        containing_order_double(d)
    }

    fn shift_range_right(&mut self, n: u32) -> std::result::Result<(), Aioobe> {
        let mut new_lowest = self.lowest_in_range;
        let mut new_highest = self.highest_limit_in_range;
        let mult = 1.0 / (1i64 << n) as f64;
        let r = (|| {
            if self.ints.total_count > self.ints.count_at(0)?
                && let Err(e) = self.ints.shift_left(n)
            {
                self.handle_shift_exception(n, e)?;
                new_highest /= mult;
                self.ints.shift_left(n)?;
            }
            new_lowest *= mult;
            new_highest *= mult;
            Ok(())
        })();
        self.set_range(new_lowest, new_highest);
        r
    }

    fn shift_range_left(&mut self, n: u32) -> std::result::Result<(), Aioobe> {
        let mut new_lowest = self.lowest_in_range;
        let mut new_highest = self.highest_limit_in_range;
        let mult = (1i64 << n) as f64;
        let r = (|| {
            if self.ints.total_count > self.ints.count_at(0)? {
                match self.ints.shift_right(n) {
                    Ok(()) => {
                        new_lowest *= mult;
                        new_highest *= mult;
                    }
                    Err(e) => {
                        self.handle_shift_exception(n, e)?;
                        new_lowest /= mult;
                    }
                }
            }
            new_lowest *= mult;
            new_highest *= mult;
            Ok(())
        })();
        self.set_range(new_lowest, new_highest);
        r
    }

    fn handle_shift_exception(&mut self, n: u32, e: Aioobe) -> std::result::Result<(), Aioobe> {
        let current = containing_order_long(self.ints.highest_trackable_value);
        let new_order = n + current;
        if new_order > 63 {
            return Err(format!(
                "Cannot resize histogram covered range beyond (1L << 63) / (1L << {}) - 1.\nCaused by: java.lang.ArrayIndexOutOfBoundsException: {e}",
                self.ints.sub_bucket_half_count_magnitude
            ));
        }
        let new_highest = ((1u64 << new_order) - 1) as i64;
        self.ints.resize(new_highest);
        self.ints.highest_trackable_value = new_highest;
        self.configured_ratio <<= n;
        Ok(())
    }

    /// `add(DoubleHistogram)`: every non-empty bucket re-recorded at its integer value.
    pub fn add(&mut self, other: &DoubleHistogram) -> Result<()> {
        for i in 0..other.ints.counts_array_length {
            let c = other.ints.count_at(i).unwrap_or(0);
            if c > 0 {
                self.record_count(other.ints.value_from_index(i) as f64 * other.i2d, c)?;
            }
        }
        Ok(())
    }

    /// `getValueAtPercentile`.
    pub fn value_at_percentile(&self, percentile: f64) -> f64 {
        self.ints.value_at_percentile(percentile) as f64 * self.i2d
    }

    /// `getPercentileAtOrBelowValue`.
    pub fn percentile_at_or_below(&self, value: f64) -> f64 {
        self.ints.percentile_at_or_below((value * self.d2i) as i64)
    }

    pub fn min_value(&self) -> f64 {
        let m = if self.ints.count_at(0).unwrap_or(0) > 0 || self.ints.total_count == 0 { 0 } else { self.ints.min_non_zero() };
        m as f64 * self.i2d
    }

    pub fn max_value(&self) -> f64 {
        self.ints.max() as f64 * self.i2d
    }

    pub fn ram_bytes(&self) -> usize {
        64 + self.ints.counts.len() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values() {
        let mut h = DoubleHistogram::new(3);
        for v in [1.0, 2.0, 3.0, 5.0] {
            h.record(v).unwrap();
        }
        // Captured from Elasticsearch: percentiles with hdr digits 3 over [1, 2, 3, 5].
        assert_eq!(h.value_at_percentile(50.0), 2.0009765625);
        assert_eq!(h.value_at_percentile(75.0), 3.0009765625);
        assert_eq!(h.value_at_percentile(99.0), 5.0029296875);
        assert_eq!(h.value_at_percentile(1.0), 1.0);
    }
}

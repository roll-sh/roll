//! Histogram metrics (`time_series_metric: histogram`) as the downsampler aggregates them.
//!
//! * `exponential_histogram` (`ExponentialHistogramFieldDownsampler.AggregateHistogram`): with
//!   delta temporality (the default for histograms) a bucket's histograms are merged into one
//!   of at most [`DEFAULT_MAX_BUCKETS`] buckets; cumulative ones keep the bucket's oldest
//!   histogram and record reset points like cumulative counters, a reset being a histogram
//!   that can't have grown into the newer one ([`exp_histogram::is_cumulative_successor`]).
//! * `histogram` and `tdigest` (`TDigestHistogramFieldDownsampler.Aggregate`): every value and
//!   count goes into a merging t-digest (compression 100), written as its unique centroids.

use crate::engine::synthetic::exponential_json;
use crate::json::Value;
use search_aggs::exp_histogram::{self, DEFAULT_MAX_BUCKETS, ExpHistogram, ExpHistogramValue, Merger};
use search_aggs::metrics::tdigest::MergingDigest;

/// An `exponential_histogram` value as `_source` holds it, normalized as indexing does
/// (buckets sorted, missing `sum`/`min`/`max` estimated).
pub(super) fn parse_exponential(v: &Value) -> Option<ExpHistogram> {
    let m = v.as_object()?;
    let buckets = |k: &str| -> Option<Vec<(i64, u64)>> {
        let Some(b) = m.get(k) else { return Some(Vec::new()) };
        let indices = b.get("indices").and_then(Value::as_array)?;
        let counts = b.get("counts").and_then(Value::as_array)?;
        indices.iter().zip(counts).map(|(i, c)| Some((i.as_i64()?, c.as_i64()? as u64))).collect()
    };
    let zero = m.get("zero");
    let value = ExpHistogramValue {
        scale: m.get("scale").and_then(Value::as_i64)? as i32,
        zero_threshold: zero.and_then(|z| z.get("threshold")).and_then(Value::as_f64).unwrap_or(0.0),
        zero_count: zero.and_then(|z| z.get("count")).and_then(Value::as_i64).unwrap_or(0) as u64,
        negative: buckets("negative")?,
        positive: buckets("positive")?,
        sum: m.get("sum").and_then(Value::as_f64),
        min: m.get("min").and_then(Value::as_f64),
        max: m.get("max").and_then(Value::as_f64),
    };
    let mut h = ExpHistogram::empty();
    exp_histogram::decode_into(&exp_histogram::encode(&value), &mut h).ok()?;
    Some(h)
}

/// `ExponentialHistogram.equals` (NaN-aware `min`/`max`).
fn same(a: &ExpHistogram, b: &ExpHistogram) -> bool {
    let eq = |x: f64, y: f64| x == y || (x.is_nan() && y.is_nan());
    a.scale == b.scale
        && a.zero.count == b.zero.count
        && a.zero.compare_threshold(&b.zero).is_eq()
        && a.negative == b.negative
        && a.positive == b.positive
        && a.sum == b.sum
        && eq(a.min, b.min)
        && eq(a.max, b.max)
}

/// A cumulative exponential histogram (`AggregateHistogram.CumulativeCollector`), fed newest
/// first.
#[derive(Clone, Debug, Default)]
pub(super) struct Cumulative {
    stack: Vec<(i64, ExpHistogram)>,
    downsampled: Option<ExpHistogram>,
    last_ts: i64,
    previous: Option<ExpHistogram>,
    previous_bucket: Option<ExpHistogram>,
}

impl Cumulative {
    pub fn new() -> Cumulative {
        Cumulative { last_ts: -1, ..Cumulative::default() }
    }

    pub fn collect(&mut self, v: ExpHistogram, ts: i64) {
        let Some(previous) = &self.previous else {
            (self.previous, self.downsampled, self.last_ts) = (Some(v.clone()), Some(v), ts);
            return;
        };
        let count = v.value_count();
        if count > previous.value_count() || !exp_histogram::is_cumulative_successor(previous, &v, DEFAULT_MAX_BUCKETS) {
            if self.last_ts > 0 && self.stack.last().is_none_or(|p| p.0 != self.last_ts) {
                let persisted = self.stack.last().map(|p| &p.1).or(self.previous_bucket.as_ref());
                if persisted.is_none_or(|p| count < p.value_count()) {
                    self.stack.push((self.last_ts, previous.clone()));
                }
            }
            self.stack.push((ts, v.clone()));
        }
        (self.downsampled, self.previous, self.last_ts) = (Some(v.clone()), Some(v), ts);
    }

    pub fn downsampled(&self) -> Option<&ExpHistogram> {
        self.downsampled.as_ref()
    }

    pub fn reset(&mut self) {
        self.previous_bucket = self.downsampled.take();
        self.last_ts = -1;
        self.stack.clear();
    }

    /// The bucket's reset points, the last pushed first; the oldest is left out when it is the
    /// value the bucket is written with.
    pub fn reset_points(&mut self, mut add: impl FnMut(i64, Value)) {
        let Some(first) = self.stack.pop() else { return };
        if self.downsampled.as_ref().is_none_or(|d| !same(&first.1, d)) {
            add(first.0, exponential_json(&first.1));
        }
        while let Some((ts, h)) = self.stack.pop() {
            add(ts, exponential_json(&h));
        }
    }
}

/// How an exponential histogram metric is aggregated in the current series.
#[derive(Clone, Debug)]
pub(super) enum Exponential {
    /// No value yet in this series: the temporality is picked by its first value.
    Unset,
    Delta(Option<Box<Merger>>),
    Cumulative(Box<Cumulative>),
}

impl Exponential {
    pub fn collect(&mut self, v: ExpHistogram, ts: i64, temporality: Option<&str>) {
        if matches!(self, Exponential::Unset) {
            *self = if temporality == Some("cumulative") { Exponential::Cumulative(Box::new(Cumulative::new())) } else { Exponential::Delta(None) };
        }
        match self {
            Exponential::Delta(m) => m.get_or_insert_with(|| Box::new(Merger::new(DEFAULT_MAX_BUCKETS))).add(&v),
            Exponential::Cumulative(c) => c.collect(v, ts),
            Exponential::Unset => unreachable!(),
        }
    }

    pub fn reset(&mut self) {
        match self {
            Exponential::Delta(m) => *m = None,
            Exponential::Cumulative(c) => c.reset(),
            Exponential::Unset => {}
        }
    }

    pub fn value(&self) -> Option<Value> {
        match self {
            Exponential::Delta(Some(m)) => Some(exponential_json(m.get())),
            Exponential::Cumulative(c) => c.downsampled().map(exponential_json),
            _ => None,
        }
    }
}

/// A `histogram` / `tdigest` metric's bucket (`TDigestState` of type `MERGING`, compression 100).
pub(super) fn add_to_digest(digest: &mut Option<Box<MergingDigest>>, v: &Value, values_key: &str) {
    let (Some(values), Some(counts)) = (v.get(values_key).and_then(Value::as_array), v.get("counts").and_then(Value::as_array)) else { return };
    let d = match digest {
        Some(d) => d,
        None => match MergingDigest::new(100.0) {
            Ok(d) => digest.insert(Box::new(d)),
            Err(_) => return,
        },
    };
    for (x, c) in values.iter().zip(counts) {
        if let (Some(x), Some(c)) = (x.as_f64(), c.as_i64()) {
            let _ = d.add(x, c.max(0) as u64);
        }
    }
}

/// The digest written as `{counts, <values>}` of its unique centroids
/// (`TDigestState.uniqueCentroids`: neighbours with the same mean are one).
pub(super) fn digest_json(digest: &mut MergingDigest, values_key: &str) -> Value {
    let mut unique: Vec<(f64, u64)> = Vec::new();
    for (mean, count) in digest.centroids() {
        match unique.last_mut() {
            Some(last) if last.0 == mean => last.1 += count,
            _ => unique.push((mean, count)),
        }
    }
    let counts: Vec<Value> = unique.iter().map(|c| Value::from(c.1 as i64)).collect();
    let values: Vec<Value> = unique.iter().map(|c| Value::from(c.0)).collect();
    crate::obj! { "counts" => Value::Arr(counts), values_key => Value::Arr(values) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> ExpHistogram {
        parse_exponential(&crate::json::parse(s.as_bytes()).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn exponential_histograms_round_trip() {
        let src = r#"{"scale":0,"sum":4.0,"min":1.0,"max":3.0,"zero":{"count":2,"threshold":0.5},"positive":{"indices":[0,1],"counts":[1,1]}}"#;
        assert_eq!(exponential_json(&h(src)).to_json(), src);
    }

    /// The suite's cumulative series, newest first: the drop from 5 values to 1 between 18:10
    /// and 18:15 is a reset, the oldest point is the bucket's value.
    #[test]
    fn cumulative_histograms_record_resets() {
        let mut c = Cumulative::new();
        let docs = [
            (5, r#"{"scale":0,"sum":17.0,"min":7.0,"max":10.0,"positive":{"indices":[2,3],"counts":[1,1]}}"#),
            (4, r#"{"scale":0,"sum":7.0,"min":7.0,"max":7.0,"positive":{"indices":[2],"counts":[1]}}"#),
            (3, r#"{"scale":0,"sum":9.0,"min":1.0,"max":5.0,"positive":{"indices":[0,1,2],"counts":[1,1,1]}}"#),
            (2, r#"{"scale":0,"sum":4.0,"min":1.0,"max":3.0,"positive":{"indices":[0,1],"counts":[1,1]}}"#),
            (1, r#"{"scale":0,"sum":1.0,"min":1.0,"max":1.0,"positive":{"indices":[0],"counts":[1]}}"#),
        ];
        for (ts, d) in docs {
            c.collect(h(d), ts);
        }
        assert_eq!(c.downsampled().map(|d| d.sum), Some(1.0));
        let mut points = Vec::new();
        c.reset_points(|ts, v| points.push((ts, v.get("sum").and_then(Value::as_f64))));
        assert_eq!(points, vec![(3, Some(9.0)), (4, Some(7.0))]);
    }
}

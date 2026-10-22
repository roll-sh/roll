//! `rate` on a counter field of a `time_series` index (Elasticsearch's
//! `TimeSeriesRateAggregator`): the per-second (or per-`unit`) increase of a monotonic counter,
//! compensating for counter resets.
//!
//! Elasticsearch runs such requests in `_tsid` order, newest sample first within a series, and
//! its aggregator is a state machine over that sequence: a bucket's span is the first and last
//! sample it saw, a sample larger than the previous (older-in-time) start value means the
//! counter was reset, and moving to a new bucket of the same series closes the previous bucket
//! with the new sample. Collection here is per segment, so samples `(bucket, series, time,
//! value)` are buffered and replayed in Elasticsearch's order at build time; the per-bucket
//! arrays model `BigArrays` growth so an owning bucket never collected into fails exactly like
//! Elasticsearch (an out-of-bounds read) or yields its zeroed `NaN` rate. Partial results
//! merge by start time, adding the reset seen between consecutive spans.

use std::cmp::Ordering;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, single_value_validate_sort};
use crate::error::{AggError, Result};
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx, single_value_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::rounding::DateUnit;
use crate::util::BigArraySize;
use crate::values::{BoundVs, DvNum, DvOrds, NumReader};

/// `rateUnit.getField().getBaseUnit().getDuration().toSeconds()`.
pub fn unit_seconds(unit: Option<DateUnit>) -> i64 {
    match unit {
        None | Some(DateUnit::Second) => 1,
        Some(DateUnit::Minute) => 60,
        Some(DateUnit::Hour) => 3600,
        Some(DateUnit::Day) => 86400,
        Some(DateUnit::Week) => 604800,
        Some(DateUnit::Month) => 2629746,
        Some(DateUnit::Quarter) => 7889238,
        Some(DateUnit::Year) => 31556952,
    }
}

/// Parent aggregations Elasticsearch implements as `DeferableBucketAggregator`s.
const DEFERABLE: [&str; 9] = [
    "terms",
    "multi_terms",
    "rare_terms",
    "significant_terms",
    "significant_text",
    "sampler",
    "diversified_sampler",
    "auto_date_histogram",
    "variable_width_histogram",
];

/// Binds a counter `rate` (`vs` is a mapped counter field).
pub fn create(def: &AggDef, vs: BoundVs, unit: Option<DateUnit>, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    if ctx.parents.iter().any(|p| DEFERABLE.contains(p)) {
        return Err(AggError::illegal_argument(
            "Wrapping a time-series rate aggregation within a DeferableBucketAggregator is not supported. Consider using an alternative outer aggregation type to avoid this, e.g. date_histogram instead of auto_date_histogram.",
        ));
    }
    let info = ctx.lookup.time_series();
    Ok(Box::new(TsRateAgg {
        vs,
        unit_seconds: unit_seconds(unit),
        ordered: ctx.time_series_request,
        tsid_field: info.as_ref().and_then(|i| i.tsid_field),
        timestamp_field: info.as_ref().and_then(|i| i.timestamp_field),
        reader: None,
        tsid: DvOrds::None,
        keys: crate::bucket::time_series::SeriesKeys::new(info.as_ref().map(|i| i.dimensions.as_slice()).unwrap_or(&[])),
        timestamps: DvNum::None,
        series: hashbrown::HashMap::new(),
        series_keys: Vec::new(),
        samples: Vec::new(),
        state: None,
        tracker: ctx.tracker(def),
    }))
}

/// One collected sample.
#[derive(Clone, Copy)]
struct Sample {
    ord: u32,
    series: u32,
    time: i64,
    value: f64,
}

/// The replayed per-bucket arrays.
struct State {
    size: BigArraySize,
    start_values: Vec<f64>,
    end_values: Vec<f64>,
    start_times: Vec<i64>,
    end_times: Vec<i64>,
    resets: Vec<f64>,
}

struct TsRateAgg {
    vs: BoundVs,
    unit_seconds: i64,
    /// The request runs in `_tsid` order (it has a `time_series` aggregation).
    ordered: bool,
    tsid_field: Option<u32>,
    timestamp_field: Option<u32>,
    reader: Option<NumReader>,
    tsid: DvOrds,
    /// Series identity from the dimensions when there is no `_tsid` field.
    keys: crate::bucket::time_series::SeriesKeys,
    timestamps: DvNum,
    /// `_tsid` bytes → series id.
    series: hashbrown::HashMap<Vec<u8>, u32>,
    series_keys: Vec<Vec<u8>>,
    samples: Vec<Sample>,
    state: Option<State>,
    tracker: MemTracker,
}

impl TsRateAgg {
    /// Replays the samples in Elasticsearch's order (`TimeSeriesRateAggregator.collect`).
    fn replay(&mut self) -> &State {
        if self.state.is_none() {
            let mut order: Vec<usize> = (0..self.samples.len()).collect();
            if self.tsid_field.is_some() {
                let (keys, samples) = (&self.series_keys, &self.samples);
                order.sort_by(|&a, &b| {
                    let (x, y) = (&samples[a], &samples[b]);
                    keys[x.series as usize].cmp(&keys[y.series as usize]).then(y.time.cmp(&x.time)).then(a.cmp(&b))
                });
            }
            let mut st = State { size: BigArraySize::new(), start_values: Vec::new(), end_values: Vec::new(), start_times: Vec::new(), end_times: Vec::new(), resets: Vec::new() };
            let mut cur_bucket: Option<u32> = None;
            let mut cur_series: Option<u32> = None;
            let (mut start_time, mut end_time) = (-1i64, -1i64);
            let (mut start_value, mut end_value) = (-1.0f64, -1.0f64);
            let mut reset = 0.0f64;
            let close = |st: &mut State, bucket: &mut Option<u32>, sv: f64, ev: f64, stt: i64, et: i64, r: f64| {
                if let Some(b) = bucket.take() {
                    let b = b as usize;
                    if st.start_values.len() <= b {
                        st.start_values.resize(b + 1, 0.0);
                        st.end_values.resize(b + 1, 0.0);
                        st.start_times.resize(b + 1, 0);
                        st.end_times.resize(b + 1, 0);
                        st.resets.resize(b + 1, 0.0);
                    }
                    st.start_values[b] = sv;
                    st.end_values[b] = ev;
                    st.start_times[b] = stt;
                    st.end_times[b] = et;
                    st.resets[b] = r;
                }
            };
            for &i in &order {
                let s = self.samples[i];
                let latest = s.value;
                if Some(s.ord) != cur_bucket {
                    st.size.grow(s.ord as u64 + 1);
                    if Some(s.series) != cur_series {
                        close(&mut st, &mut cur_bucket, start_value, end_value, start_time, end_time, reset);
                        cur_series = Some(s.series);
                    } else {
                        start_time = s.time;
                        if latest > start_value {
                            reset += end_value;
                            end_value = latest;
                        }
                        start_value = latest;
                        close(&mut st, &mut cur_bucket, start_value, end_value, start_time, end_time, reset);
                    }
                    cur_bucket = Some(s.ord);
                    start_time = s.time;
                    end_time = s.time;
                    start_value = latest;
                    end_value = latest;
                    reset = 0.0;
                } else {
                    start_time = s.time;
                    if latest > start_value {
                        reset += end_value;
                        end_value = latest;
                    }
                    start_value = latest;
                }
            }
            close(&mut st, &mut cur_bucket, start_value, end_value, start_time, end_time, reset);
            self.state = Some(st);
        }
        self.state.as_ref().expect("replayed")
    }

    fn rate(&mut self, ord: u32) -> Result<ResetTrackingRate> {
        let unit_seconds = self.unit_seconds;
        let st = self.replay();
        // `startValues.get(owningBucketOrd)`.
        st.size.check(ord as u64)?;
        let o = ord as usize;
        let get = |v: &Vec<f64>| v.get(o).copied().unwrap_or(0.0);
        Ok(ResetTrackingRate {
            start_value: get(&st.start_values),
            end_value: get(&st.end_values),
            start_time: st.start_times.get(o).copied().unwrap_or(0),
            end_time: st.end_times.get(o).copied().unwrap_or(0),
            reset: get(&st.resets),
            unit_seconds,
        })
    }
}

impl Aggregator for TsRateAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.reader = Some(self.vs.numeric_reader(seg)?);
        self.tsid = match self.tsid_field {
            Some(f) => DvOrds::open(seg, f),
            None => DvOrds::None,
        };
        self.keys.set_segment(seg);
        self.timestamps = match self.timestamp_field {
            Some(f) => DvNum::open(seg, f),
            None => DvNum::None,
        };
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if b.is_empty() {
            return Ok(());
        }
        if !self.ordered {
            // `AggregationExecutionContext.getTsidHashOrd()` outside a time-series search.
            return Err(AggError::illegal_argument(
                "Aggregation on a time-series field is misconfigured, likely due to lack of wrapping a metric aggregation within a `time-series` aggregation",
            ));
        }
        let Some(mut reader) = self.reader.take() else { return Ok(()) };
        let mut key = Vec::new();
        let res = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                // `MultiValueMode.MAX`; docs without a value are skipped.
                let Some(&value) = reader.doubles(doc, b.score(i))?.last() else { continue };
                let mut time = None;
                self.timestamps.for_each(doc, |t| {
                    if time.is_none() {
                        time = Some(t);
                    }
                });
                let mut tsid_ord = None;
                self.tsid.for_each(doc, |o| {
                    if tsid_ord.is_none() {
                        tsid_ord = Some(o);
                    }
                });
                key.clear();
                if let Some(o) = tsid_ord {
                    self.tsid.lookup(o, &mut key);
                } else if self.tsid_field.is_none() {
                    key.extend_from_slice(self.keys.build(doc)?);
                }
                let series = match self.series.get(&key) {
                    Some(&s) => s,
                    None => {
                        let s = self.series_keys.len() as u32;
                        self.series.insert(key.clone(), s);
                        self.series_keys.push(key.clone());
                        s
                    }
                };
                self.samples.push(Sample { ord: b.ords[i], series, time: time.unwrap_or(0), value });
            }
            self.tracker.ensure(self.samples.len() * 32 + self.series_keys.len() * 64)
        })();
        self.reader = Some(reader);
        res
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        owning.iter().map(|&o| Ok(AggValue::ResetTrackingRate(Box::new(self.rate(o)?)))).collect()
    }

    fn build_empty(&self) -> AggValue {
        AggValue::ResetTrackingRate(Box::new(ResetTrackingRate { start_value: 0.0, end_value: 0.0, start_time: 0, end_time: 0, reset: 0.0, unit_seconds: 1 }))
    }

    fn post_collection(&mut self) -> Result<()> {
        self.replay();
        Ok(())
    }

    fn needs_scores(&self) -> bool {
        false
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_value_validate_sort(name, key, rest)
    }

    fn sort_value(&self, _key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        // `metric(ord)`: the replay ran in `post_collection`.
        let Some(st) = &self.state else { return f64::NAN };
        let o = ord as usize;
        let get = |v: &Vec<f64>| v.get(o).copied().unwrap_or(0.0);
        ResetTrackingRate {
            start_value: get(&st.start_values),
            end_value: get(&st.end_values),
            start_time: st.start_times.get(o).copied().unwrap_or(0),
            end_time: st.end_times.get(o).copied().unwrap_or(0),
            reset: get(&st.resets),
            unit_seconds: self.unit_seconds,
        }
        .value()
    }
}

/// `InternalResetTrackingRate`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResetTrackingRate {
    pub start_value: f64,
    pub end_value: f64,
    pub start_time: i64,
    pub end_time: i64,
    pub reset: f64,
    pub unit_seconds: i64,
}

impl ResetTrackingRate {
    pub fn value(&self) -> f64 {
        (self.end_value - self.start_value + self.reset) / (self.end_time - self.start_time) as f64 * 1000.0 * self.unit_seconds as f64
    }
}

impl Internal for ResetTrackingRate {
    fn type_name(&self) -> &'static str {
        "rate_with_resets"
    }

    fn class_name(&self) -> &'static str {
        "InternalResetTrackingRate"
    }

    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut sorted = list;
        sorted.sort_by(|a, b| a.start_time.cmp(&b.start_time).then(Ordering::Equal));
        let first = &sorted[0];
        let (mut reset, start_value, mut end_value) = (first.reset, first.start_value, first.end_value);
        for r in &sorted[1..] {
            reset += r.reset;
            if end_value > r.start_value {
                reset += end_value;
            }
            end_value = r.end_value;
        }
        let last = sorted.last().expect("non-empty");
        Ok(ResetTrackingRate { start_value, end_value, start_time: first.start_time, end_time: last.end_time, reset, unit_seconds: first.unit_seconds })
    }

    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        // A raw double: NaN and infinities print as strings.
        out.push("value", Json::Float(self.value()));
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value(), path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_and_reduce() {
        // Elasticsearch: samples 100, 160, 30, 90 a minute apart → 0.8333… per second.
        let r = ResetTrackingRate { start_value: 100.0, end_value: 160.0, start_time: 0, end_time: 180_000, reset: 90.0, unit_seconds: 1 };
        assert_eq!(r.value(), 0.8333333333333334);
        assert_eq!(ResetTrackingRate { unit_seconds: 2629746, ..r.clone() }.value(), 2191455.0);
        let a = ResetTrackingRate { start_value: 10.0, end_value: 50.0, start_time: 0, end_time: 10, reset: 0.0, unit_seconds: 1 };
        let b = ResetTrackingRate { start_value: 5.0, end_value: 20.0, start_time: 20, end_time: 30, reset: 0.0, unit_seconds: 1 };
        let opts = crate::internal::ReduceOptions::default();
        let mut ctx = ReduceCtx::new(&opts);
        let m = ResetTrackingRate::reduce(vec![b, a], &mut ctx).unwrap();
        assert_eq!((m.start_value, m.end_value, m.reset, m.start_time, m.end_time), (10.0, 20.0, 50.0, 0, 30));
    }
}

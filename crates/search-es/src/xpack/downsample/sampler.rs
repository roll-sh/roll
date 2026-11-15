//! The downsampling state machine (`DownsampleShardIndexer.TimeSeriesBucketCollector`): fed the
//! source documents in the index sort's order (`_tsid` ascending, `@timestamp` descending), it
//! emits each time series' bucket as soon as the next document belongs elsewhere. It holds one
//! bucket of one series at a time, so memory doesn't grow with the source index.
//!
//! Per bucket it writes one document (`@timestamp` = the bucket, `_doc_count`, dimensions,
//! metrics, labels). Cumulative counters keep the bucket's oldest value, and when a counter
//! resets inside the bucket the points needed to rebuild its rate are written as extra
//! documents at their own timestamps (`ResetDataPoints`: dimensions and the counters that reset
//! there, no `_doc_count`); the bucket's `_doc_count` leaves them out, so downsampling the
//! result again counts every source document once.

use super::fields::{Field, Fields, GAUGE_METRICS, Metric, Role};
use super::histograms::{self, Exponential};
use crate::json::{Map, Value};
use search_aggs::metrics::tdigest::MergingDigest;
use crate::time::{DateFormatter, Instant, Zone};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Method {
    Aggregate,
    LastValue,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Aggregate => "aggregate",
            Method::LastValue => "last_value",
        }
    }
}

/// A source document's values, aligned with [`Fields::fields`].
pub(super) struct Doc {
    pub tsid: String,
    /// `@timestamp` in the field's resolution (milliseconds, or nanoseconds for `date_nanos`).
    pub timestamp: i64,
    pub doc_count: i64,
    /// Doc values (always an array) or the `_source` value of each field; `None` when missing.
    pub values: Vec<Option<Value>>,
}

/// How the timestamps are stored and printed.
pub(super) struct Clock {
    pub nanos: bool,
    pub format: DateFormatter,
}

impl Clock {
    pub fn millis(&self, raw: i64) -> i64 {
        if self.nanos { raw.div_euclid(1_000_000) } else { raw }
    }

    fn print_raw(&self, raw: i64) -> String {
        let i = if self.nanos { Instant { secs: raw.div_euclid(1_000_000_000), nanos: raw.rem_euclid(1_000_000_000) as u32 } } else { Instant::from_millis(raw) };
        self.format.format(i, &Zone::UTC)
    }

    fn print_millis(&self, ms: i64) -> String {
        self.format.format(Instant::from_millis(ms), &Zone::UTC)
    }
}

/// Elasticsearch's `CompensatedSum` (Kahan summation).
#[derive(Clone, Copy, Default, Debug)]
struct Kahan {
    value: f64,
    delta: f64,
}

impl Kahan {
    fn add(&mut self, v: f64) {
        if !v.is_finite() {
            self.value += v;
        }
        if self.value.is_finite() {
            let corrected = v + self.delta;
            let updated = self.value + corrected;
            self.delta = corrected - (updated - self.value);
            self.value = updated;
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Gauge {
    min: f64,
    max: f64,
    sum: Kahan,
    count: i64,
}

impl Gauge {
    const EMPTY: Gauge = Gauge { min: f64::MAX, max: -f64::MAX, sum: Kahan { value: 0.0, delta: 0.0 }, count: 0 };

    fn add(&mut self, x: f64) {
        self.min = self.min.min(x);
        self.max = self.max.max(x);
        self.sum.add(x);
        self.count += 1;
    }
}

/// One sub-metric of a pre-aggregated gauge.
#[derive(Clone, Debug)]
enum Sub {
    Min(Option<f64>),
    Max(Option<f64>),
    Sum(Option<Kahan>),
    Count(Option<i64>),
    /// `last_value`: the newest document's value.
    Last(&'static str, Option<f64>),
}

impl Sub {
    fn new(name: &str, method: Method) -> Sub {
        let name = GAUGE_METRICS.into_iter().find(|m| *m == name).unwrap_or("max");
        match (method, name) {
            (Method::LastValue, n) => Sub::Last(n, None),
            (_, "min") => Sub::Min(None),
            (_, "sum") => Sub::Sum(None),
            (_, "value_count") => Sub::Count(None),
            _ => Sub::Max(None),
        }
    }

    fn add(&mut self, x: f64) {
        match self {
            Sub::Min(m) => *m = Some(m.map_or(x, |m| m.min(x))),
            Sub::Max(m) => *m = Some(m.map_or(x, |m| m.max(x))),
            Sub::Sum(s) => s.get_or_insert_with(Kahan::default).add(x),
            Sub::Count(c) => *c = Some(c.unwrap_or(0) + x.round() as i64),
            Sub::Last(_, l) => {
                if l.is_none() {
                    *l = Some(x)
                }
            }
        }
    }

    fn reset(&mut self) {
        match self {
            Sub::Min(m) | Sub::Max(m) | Sub::Last(_, m) => *m = None,
            Sub::Sum(s) => *s = None,
            Sub::Count(c) => *c = None,
        }
    }

    fn write(&self, out: &mut Map) {
        match self {
            Sub::Min(Some(v)) => out.insert("min".into(), Value::from(*v)),
            Sub::Max(Some(v)) => out.insert("max".into(), Value::from(*v)),
            Sub::Sum(Some(s)) => out.insert("sum".into(), Value::from(s.value)),
            Sub::Count(Some(c)) => out.insert("value_count".into(), Value::from(*c)),
            Sub::Last(n, Some(v)) => out.insert((*n).into(), if *n == "value_count" { Value::from(v.round() as i64) } else { Value::from(*v) }),
            _ => None,
        };
    }
}

/// A cumulative counter (`AggregateCounter.CumulativeCollector`), fed newest first.
#[derive(Clone, Debug)]
struct Cumulative {
    /// Reset points of this bucket, the last pushed on top.
    stack: Vec<(i64, f64)>,
    downsampled: f64,
    /// The timestamp of the previous value in this bucket; -1 at the start of a bucket.
    last_ts: i64,
    /// The previous value of the series (across buckets).
    previous: f64,
    /// The value the previous bucket of the series was written with.
    previous_bucket: f64,
}

impl Cumulative {
    const NEW: Cumulative = Cumulative { stack: Vec::new(), downsampled: f64::NAN, last_ts: -1, previous: f64::NAN, previous_bucket: f64::NAN };

    fn collect(&mut self, v: f64, ts: i64) {
        if self.previous.is_nan() {
            (self.downsampled, self.previous, self.last_ts) = (v, v, ts);
            return;
        }
        // Going back in time, a larger older value means the counter reset in between.
        if v > self.previous {
            if self.last_ts > 0 && self.stack.last().is_none_or(|p| p.0 != self.last_ts) {
                let persisted = match self.stack.last() {
                    Some(p) => p.1,
                    None => self.previous_bucket,
                };
                if persisted.is_nan() || v < persisted {
                    self.stack.push((self.last_ts, self.previous));
                }
            }
            self.stack.push((ts, v));
        }
        (self.downsampled, self.previous, self.last_ts) = (v, v, ts);
    }

    fn reset(&mut self) {
        self.previous_bucket = self.downsampled;
        self.downsampled = f64::NAN;
        self.last_ts = -1;
        self.stack.clear();
    }

    fn reset_points(&mut self, field: usize, points: &mut ResetPoints) {
        let Some(first) = self.stack.pop() else { return };
        if first.1 != self.downsampled {
            points.add(field, first.0, Value::from(first.1));
        }
        while let Some((ts, v)) = self.stack.pop() {
            points.add(field, ts, Value::from(v));
        }
    }
}

#[derive(Clone, Debug)]
enum Counter {
    /// No value yet in this series: the temporality is picked by its first value.
    Unset,
    Delta(Kahan),
    Cumulative(Cumulative),
}

/// The extra documents of a bucket: by timestamp, the counters and histograms (field index,
/// value) that reset there.
#[derive(Default)]
struct ResetPoints(BTreeMap<i64, Vec<(usize, Value)>>);

impl ResetPoints {
    fn add(&mut self, field: usize, ts: i64, v: Value) {
        let at = self.0.entry(ts).or_default();
        if !at.iter().any(|(f, _)| *f == field) {
            at.push((field, v));
        }
    }
}

#[derive(Clone, Debug)]
enum State {
    Dimension(Option<Value>),
    Gauge(Gauge),
    Counter { counter: Counter, collected: bool },
    Exponential { agg: Exponential, collected: bool },
    /// A `histogram` / `tdigest` metric's t-digest, and the name of its values array.
    Digest(Option<Box<MergingDigest>>, &'static str),
    Aggregate(Vec<Sub>),
    /// Labels and `last_value` metrics: the newest value of the bucket.
    Last(Option<Value>),
}

pub(super) struct Sampler<'a> {
    fields: &'a Fields,
    method: Method,
    clock: &'a Clock,
    interval_ms: i64,
    /// `index.time_series.start_time`: no bucket starts before it.
    start_ms: i64,
    /// The dimension holding each series' temporality (`index.time_series.temporality_field`).
    temporality: Option<usize>,
    tsid: Option<String>,
    bucket_ms: i64,
    /// The bucket of the last document (`lastHistoTimestamp`).
    histo_ms: i64,
    doc_count: i64,
    dims_collected: bool,
    states: Vec<State>,
}

impl<'a> Sampler<'a> {
    pub fn new(fields: &'a Fields, method: Method, clock: &'a Clock, interval_ms: i64, start_ms: i64, temporality_field: Option<&str>) -> Sampler<'a> {
        let states = fields
            .fields
            .iter()
            .map(|f| match (&f.role, method) {
                (Role::Dimension, _) => State::Dimension(None),
                (Role::Metric(Metric::Aggregate(subs)), m) => State::Aggregate(subs.iter().map(|s| Sub::new(s, m)).collect()),
                (Role::Metric(Metric::Gauge), Method::Aggregate) => State::Gauge(Gauge::EMPTY),
                (Role::Metric(Metric::Counter), Method::Aggregate) => State::Counter { counter: Counter::Unset, collected: false },
                (Role::Metric(Metric::ExponentialHistogram), Method::Aggregate) => State::Exponential { agg: Exponential::Unset, collected: false },
                (Role::Metric(Metric::TDigest(key)), Method::Aggregate) => State::Digest(None, key),
                _ => State::Last(None),
            })
            .collect();
        let temporality = temporality_field.and_then(|t| fields.fields.iter().position(|f| f.role == Role::Dimension && f.name == t));
        Sampler { fields, method, clock, interval_ms, start_ms, temporality, tsid: None, bucket_ms: 0, histo_ms: i64::MAX, doc_count: 0, dims_collected: false, states }
    }

    /// Feeds the next document; the documents of every bucket it closes go to `out`.
    pub fn push(&mut self, doc: Doc, out: &mut Vec<Map>) {
        let ms = self.clock.millis(doc.timestamp);
        let tsid_changed = self.tsid.as_deref() != Some(doc.tsid.as_str());
        if tsid_changed || ms < self.histo_ms {
            self.histo_ms = (ms.div_euclid(self.interval_ms) * self.interval_ms).max(self.start_ms);
        }
        if tsid_changed || self.bucket_ms != self.histo_ms {
            self.flush(out);
            self.reset_bucket(self.histo_ms);
            if tsid_changed {
                self.reset_series(doc.tsid.clone());
            }
        }
        self.doc_count += doc.doc_count;
        if !self.dims_collected {
            for (s, v) in self.states.iter_mut().zip(&doc.values) {
                if let State::Dimension(d) = s {
                    *d = v.as_ref().map(collapse);
                }
            }
            self.dims_collected = true;
        }
        let temporality = self.temporality.and_then(|i| match &self.states[i] {
            State::Dimension(Some(Value::Str(s))) => Some(s.clone()),
            _ => None,
        });
        for (state, value) in self.states.iter_mut().zip(doc.values) {
            let Some(value) = value else { continue };
            match state {
                State::Dimension(_) => {}
                State::Gauge(g) => numbers(&value).for_each(|x| g.add(x)),
                State::Counter { counter, collected } => {
                    let Some(x) = numbers(&value).next() else { continue };
                    if matches!(counter, Counter::Unset) {
                        *counter = if temporality.as_deref() == Some("delta") { Counter::Delta(Kahan::default()) } else { Counter::Cumulative(Cumulative::NEW) };
                    }
                    match counter {
                        Counter::Delta(sum) => sum.add(x),
                        Counter::Cumulative(c) => c.collect(x, doc.timestamp),
                        Counter::Unset => unreachable!(),
                    }
                    *collected = true;
                }
                State::Exponential { agg, collected } => {
                    if let Some(h) = histograms::parse_exponential(&value) {
                        agg.collect(h, doc.timestamp, temporality.as_deref());
                        *collected = true;
                    }
                }
                State::Digest(digest, key) => histograms::add_to_digest(digest, &value, key),
                State::Aggregate(subs) => {
                    if let Value::Obj(m) = &value {
                        for s in subs.iter_mut() {
                            let key = match s {
                                Sub::Min(_) => "min",
                                Sub::Max(_) => "max",
                                Sub::Sum(_) => "sum",
                                Sub::Count(_) => "value_count",
                                Sub::Last(n, _) => n,
                            };
                            if let Some(x) = m.get(key).and_then(Value::as_f64) {
                                s.add(x);
                            }
                        }
                    }
                }
                State::Last(last) => {
                    if last.is_none() {
                        *last = Some(value);
                    }
                }
            }
        }
    }

    /// Emits the last open bucket.
    pub fn finish(&mut self, out: &mut Vec<Map>) {
        self.flush(out);
    }

    fn reset_bucket(&mut self, bucket_ms: i64) {
        self.bucket_ms = bucket_ms;
        self.doc_count = 0;
        for s in &mut self.states {
            match s {
                State::Dimension(_) => {}
                State::Gauge(g) => *g = Gauge::EMPTY,
                State::Counter { counter, collected } => {
                    *collected = false;
                    match counter {
                        Counter::Delta(sum) => *sum = Kahan::default(),
                        Counter::Cumulative(c) => c.reset(),
                        Counter::Unset => {}
                    }
                }
                State::Exponential { agg, collected } => {
                    *collected = false;
                    agg.reset();
                }
                State::Digest(d, _) => *d = None,
                State::Aggregate(subs) => subs.iter_mut().for_each(Sub::reset),
                State::Last(l) => *l = None,
            }
        }
    }

    fn reset_series(&mut self, tsid: String) {
        self.tsid = Some(tsid);
        self.dims_collected = false;
        for s in &mut self.states {
            match s {
                State::Dimension(d) => *d = None,
                State::Counter { counter, .. } => *counter = Counter::Unset,
                State::Exponential { agg, .. } => *agg = Exponential::Unset,
                _ => {}
            }
        }
    }

    fn flush(&mut self, out: &mut Vec<Map>) {
        // `DownsampleBucketBuilder.isEmpty`.
        if self.tsid.is_none() || self.bucket_ms == 0 || self.doc_count == 0 {
            return;
        }
        let mut points = ResetPoints::default();
        for (i, s) in self.states.iter_mut().enumerate() {
            match s {
                State::Counter { counter: Counter::Cumulative(c), .. } => c.reset_points(i, &mut points),
                State::Exponential { agg: Exponential::Cumulative(c), .. } => c.reset_points(|ts, v| points.add(i, ts, v)),
                _ => {}
            }
        }
        let mut doc = Map::new();
        doc.insert("@timestamp".into(), Value::Str(self.clock.print_millis(self.bucket_ms)));
        doc.insert("_doc_count".into(), Value::from(self.doc_count - points.0.len() as i64));
        let fields = &self.fields.fields;
        for pass in [Pass::Metrics, Pass::Labels, Pass::Dimensions] {
            for (f, s) in fields.iter().zip(self.states.iter_mut()).filter(|(f, _)| pass.takes(f)) {
                if let Some(v) = write(self.method, f, s) {
                    doc.insert(f.name.clone(), v);
                }
            }
        }
        out.push(doc);
        for (ts, counters) in points.0 {
            let mut doc = Map::new();
            doc.insert("@timestamp".into(), Value::Str(self.clock.print_raw(ts)));
            for (f, s) in fields.iter().zip(&self.states) {
                if let State::Dimension(Some(v)) = s {
                    doc.insert(f.name.clone(), v.clone());
                }
            }
            for (i, v) in counters {
                doc.insert(fields[i].name.clone(), v);
            }
            out.push(doc);
        }
    }

}

/// A field's value in the bucket's document.
fn write(method: Method, f: &Field, s: &mut State) -> Option<Value> {
    match s {
        State::Dimension(v) => v.clone(),
        State::Gauge(g) => (g.count > 0).then(|| crate::obj! { "min" => g.min, "max" => g.max, "sum" => g.sum.value, "value_count" => g.count }),
        State::Counter { collected: false, .. } | State::Counter { counter: Counter::Unset, .. } => None,
        State::Counter { counter: Counter::Delta(sum), .. } => Some(Value::from(sum.value)),
        State::Counter { counter: Counter::Cumulative(c), .. } => Some(Value::from(c.downsampled)),
        State::Exponential { collected: false, .. } => None,
        State::Exponential { agg, .. } => agg.value(),
        State::Digest(d, key) => d.as_mut().map(|d| histograms::digest_json(d, key)),
        State::Aggregate(subs) => {
            let mut m = Map::new();
            subs.iter().for_each(|s| s.write(&mut m));
            (!m.is_empty()).then_some(Value::Obj(m))
        }
        // A numeric metric's last value is its first (smallest) doc value, as a double.
        State::Last(v) if matches!(f.role, Role::Metric(Metric::Gauge | Metric::Counter)) && method == Method::LastValue => {
            v.as_ref().and_then(|v| numbers(v).next()).map(Value::from)
        }
        State::Last(v) => v.as_ref().map(collapse),
    }
}

#[derive(Clone, Copy)]
enum Pass {
    Metrics,
    Labels,
    Dimensions,
}

impl Pass {
    fn takes(self, f: &Field) -> bool {
        matches!((self, &f.role), (Pass::Metrics, Role::Metric(_)) | (Pass::Labels, Role::Label) | (Pass::Dimensions, Role::Dimension))
    }
}

/// A doc value array as written: one value as itself, several as an array.
fn collapse(v: &Value) -> Value {
    match v {
        Value::Arr(a) if a.len() == 1 => a[0].clone(),
        v => v.clone(),
    }
}

/// The numbers of a doc value array (or a single number).
fn numbers(v: &Value) -> impl Iterator<Item = f64> + '_ {
    let items: &[Value] = match v {
        Value::Arr(a) => a,
        v => std::slice::from_ref(v),
    };
    items.iter().filter_map(|x| match x {
        Value::Num(n) => Some(n.as_f64()),
        Value::Str(s) => s.parse().ok(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::fields::{Field, Read};
    use super::*;

    fn fields() -> Fields {
        let f = |name: &str, role| Field { name: name.into(), role, read: Read::DocValues(name.into()) };
        Fields { fields: vec![f("uid", Role::Dimension), f("c", Role::Metric(Metric::Counter)), f("g", Role::Metric(Metric::Gauge)), f("l", Role::Label)] }
    }

    fn run(docs: &[(&str, i64, f64, f64)]) -> Vec<String> {
        let fields = fields();
        let clock = Clock { nanos: false, format: DateFormatter::new("strict_date_optional_time").unwrap() };
        let mut s = Sampler::new(&fields, Method::Aggregate, &clock, 3_600_000, 1, None);
        let mut out = Vec::new();
        for (tsid, ts, c, g) in docs {
            let values = vec![Some(Value::from(vec![*tsid])), Some(Value::from(vec![*c])), Some(Value::from(vec![*g])), Some(Value::from(vec![format!("{ts}")]))];
            s.push(Doc { tsid: tsid.to_string(), timestamp: *ts, doc_count: 1, values }, &mut out);
        }
        s.finish(&mut out);
        out.into_iter().map(|d| Value::Obj(d).to_json()).collect()
    }

    /// The `dog` series of the suite's `test` index (multi-counter), as Elasticsearch 9.5.5
    /// downsampled it: each bucket keeps its oldest value, and the resets get documents.
    #[test]
    fn cumulative_counters_keep_reset_points() {
        // 19:51:03.142, 19:50:53.142 (bucket 19:00), 18:50:23.142, 18:50:03.142 (bucket 18:00).
        let got = run(&[
            ("dog", 1_619_639_463_142, 76.0, 95.0),
            ("dog", 1_619_639_453_142, 1000.0, 99.0),
            ("dog", 1_619_635_823_142, 0.0, 101.0),
            ("dog", 1_619_635_803_142, 7.0, 100.0),
        ]);
        assert_eq!(
            got,
            vec![
                r#"{"@timestamp":"2021-04-28T19:00:00.000Z","_doc_count":1,"c":1000.0,"g":{"min":95.0,"max":99.0,"sum":194.0,"value_count":2},"l":"1619639463142","uid":"dog"}"#,
                r#"{"@timestamp":"2021-04-28T19:51:03.142Z","uid":"dog","c":76.0}"#,
                r#"{"@timestamp":"2021-04-28T18:00:00.000Z","_doc_count":1,"c":7.0,"g":{"min":100.0,"max":101.0,"sum":201.0,"value_count":2},"l":"1619635823142","uid":"dog"}"#,
                r#"{"@timestamp":"2021-04-28T18:50:23.142Z","uid":"dog","c":0.0}"#,
            ]
        );
    }

    #[test]
    fn kahan_matches_compensated_sum() {
        let mut k = Kahan::default();
        for _ in 0..10 {
            k.add(0.1);
        }
        assert_eq!(k.value, 1.0);
    }
}

//! OTLP metrics (`ExportMetricsServiceRequest`, protobuf or JSON) as the documents Elastic's
//! exporter writes in its `otel` mapping mode, for the built-in `metrics-otel@template` (a time
//! series data stream whose `metrics-otel@mappings` holds the dynamic templates named below):
//!
//! One document per resource, scope, data point attributes, `@timestamp`, `start_timestamp`,
//! `unit` and `temporality`, holding each of those data points' values under
//! `metrics.<name>`: `@timestamp` and `start_timestamp` (epoch milliseconds; no start when the
//! point has none), `unit`, `temporality` (`delta` on histograms), `data_stream.*`,
//! `attributes`, `resource.*`, `scope.*`, `metrics.*`, and `_metric_names_hash` (xxHash64 of
//! the sorted metric names, in hex: a dimension, so two documents of one series and timestamp
//! holding other metrics don't collide). Each `create` line names each value's dynamic
//! template (`dynamic_templates`) and its unit (`dynamic_template_params`, the templates'
//! `{{unit}}`):
//!
//! - gauge: `gauge_double` or `gauge_long`;
//! - sum: `counter_double` or `counter_long` when monotonic and cumulative, else `gauge_*`;
//! - histogram (delta): `histogram`, `{counts, values}` with each non-empty bucket at its
//!   midpoint (the first bucket at half its upper bound when that's positive, the overflow
//!   bucket at the last bound; no bounds: one bucket at `sum / count`), as the exporter's
//!   default (`TDigest`) mapping converts it;
//! - exponential histogram (delta): `histogram` the same way, each bucket at the midpoint of
//!   its boundaries, the zero bucket at 0;
//! - summary: `summary`, `{sum, value_count}` (quantiles aren't kept, as the exporter's).
//!
//! Cumulative histograms and exponential histograms are rejected, as the exporter does under
//! that mapping (a `histogram` field can't hold a running total): SDKs send delta ones with
//! `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=delta`.
//!
//! Routing is logs' ([`super::logs`]) into `metrics-<dataset>.otel-<namespace>`, plus the
//! exporter's scope routing when no `data_stream.dataset` is set: a Collector's own telemetry
//! goes to `collectortelemetry`, a scope with an `encoding.format` attribute to that, and a
//! receiver's or connector's scope (`…/receiver/hostmetricsreceiver/…`) to its name. Data point
//! attributes are written as the exporter writes dimensions: the routing ones and
//! `elasticsearch.mapping.hints` left out, a key-value list as its JSON text (resource and
//! scope attributes too). Mapping hints aren't read.
//!
//! Each data point's values are charged as they're built ([`super::logs::Budget`]; at most
//! [`super::logs::MAX_VALUES`] a point, buckets included) and held until its scope's documents
//! are written. A point without a value, a non-finite one, a duplicate metric name in one
//! document, or a histogram whose buckets don't fit its bounds is rejected (counted in the
//! answer's `rejectedDataPoints`), as the exporter's validation does.

use super::logs::{
    Batch, Budget, Context, Fail, Rejected, Rendered, Step, VALUE_COST, add_attribute, attributes_json, context_json_in, context_or, context_pb, dataset_base,
    dimensions, drop_rendered, get, key_value_pb, push_line, render_as, str_json, target_from, text, uint_json,
};
use super::wire::{Fields, Wire, varint};
use crate::json::{Map, Value};
use crate::obj;
use std::collections::HashMap;

/// A Collector's own telemetry scopes, routed to `collectortelemetry`.
const SELF_TELEMETRY: [&str; 5] = [
    "go.opentelemetry.io/collector/receiver/receiverhelper",
    "go.opentelemetry.io/collector/scraper/scraperhelper",
    "go.opentelemetry.io/collector/processor/processorhelper",
    "go.opentelemetry.io/collector/exporter/exporterhelper",
    "go.opentelemetry.io/collector/service",
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Gauge,
    Sum,
    Histogram,
    Exponential,
    Summary,
}

/// `AggregationTemporality`.
const DELTA: u64 = 1;
const CUMULATIVE: u64 = 2;

/// What a metric gives each of its points.
struct Metric {
    name: Option<String>,
    unit: String,
    shape: Shape,
    temporality: u64,
    monotonic: bool,
}

impl Metric {
    /// Why every point of the metric is rejected.
    fn refused(&self) -> Option<String> {
        let Some(name) = &self.name else { return Some("a metric without a name".into()) };
        match self.shape {
            Shape::Histogram if self.temporality == CUMULATIVE => Some(format!("dropping cumulative temporality histogram {name:?} (send delta histograms)")),
            Shape::Exponential if self.temporality == CUMULATIVE => {
                Some(format!("dropping cumulative temporality exponential histogram {name:?} (send delta histograms)"))
            }
            _ => None,
        }
    }

    fn temporality(&self) -> Option<&'static str> {
        match (self.shape, self.temporality) {
            (Shape::Histogram | Shape::Exponential, DELTA) => Some("delta"),
            (Shape::Histogram | Shape::Exponential, CUMULATIVE) => Some("cumulative"),
            _ => None,
        }
    }
}

/// A data point as decoded, before its value is made.
#[derive(Debug, Default)]
struct Raw {
    time: u64,
    start: u64,
    attributes: Map,
    dropped: u64,
    double: Option<f64>,
    int: Option<i64>,
    count: u64,
    sum: Option<f64>,
    buckets: Vec<u64>,
    bounds: Vec<f64>,
    scale: i64,
    zero: u64,
    positive: (i64, Vec<u64>),
    negative: (i64, Vec<u64>),
}

/// One document being built.
struct Group {
    index: String,
    dataset: String,
    namespace: String,
    time: u64,
    start: u64,
    unit: String,
    temporality: Option<&'static str>,
    attributes: Map,
    metrics: Map,
    templates: Map,
}

/// A scope's documents, in the order their first points came.
#[derive(Default)]
struct Groups {
    list: Vec<Group>,
    by_key: HashMap<String, usize>,
}

fn clamp(n: u64) -> i64 {
    n.min(i64::MAX as u64) as i64
}

/// The exporter's scope routing: a dataset (made valid) for a scope without one of its own.
fn scope_route(c: &Context) -> Option<String> {
    let name = c.name.as_deref().unwrap_or_default();
    let raw = if SELF_TELEMETRY.contains(&name) {
        Some("collectortelemetry".to_string())
    } else if let Some(f) = c.attributes.get("encoding.format").and_then(Value::as_str).filter(|f| !f.is_empty()) {
        Some(f.to_string())
    } else {
        component(name, "/receiver/", "receiver").or_else(|| component(name, "/connector/", "connector"))
    };
    raw.map(|r| dataset_base(&r)).filter(|d| !d.is_empty())
}

/// `/receiver/(\w+receiver)` (or a connector's), leftmost and greedy as the exporter's regex.
fn component(name: &str, marker: &str, suffix: &str) -> Option<String> {
    let mut from = 0;
    while let Some(at) = name[from..].find(marker) {
        let rest = &name[from + at + marker.len()..];
        let run = &rest[..rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len())];
        if let Some(end) = (suffix.len() + 1..=run.len()).rev().find(|&e| run[..e].ends_with(suffix)) {
            return Some(run[..end].to_string());
        }
        from += at + 1;
    }
    None
}

/// The exporter's explicit-bucket midpoint for bucket `i` (`bounds` non-empty, `i <= bounds.len()`).
fn midpoint(bounds: &[f64], i: usize) -> f64 {
    if i == 0 {
        let v = bounds[0];
        if v > 0.0 { v / 2.0 } else { v }
    } else if i >= bounds.len() {
        bounds[i - 1]
    } else {
        bounds[i - 1] + (bounds[i] - bounds[i - 1]) / 2.0
    }
}

/// An exponential histogram bucket's lower boundary (OpenTelemetry's data model).
fn lower_boundary(index: i64, scale: i64) -> f64 {
    if scale <= 0 {
        let exp = index.saturating_mul(1 << -scale).clamp(-2000, 2000);
        return 2f64.powi(exp as i32);
    }
    let inverse = std::f64::consts::LN_2 / (1u64 << scale) as f64;
    2.0 * ((index - (1i64 << scale)) as f64 * inverse).exp()
}

/// A point's value and its dynamic template, or why it's rejected.
fn value_of(raw: &Raw, m: &Metric) -> Result<(Value, &'static str), String> {
    let name = m.name.as_deref().unwrap_or_default();
    let (mut counts, mut values) = (Vec::new(), Vec::new());
    match m.shape {
        Shape::Gauge | Shape::Sum => {
            let counter = m.shape == Shape::Sum && m.monotonic && m.temporality == CUMULATIVE;
            return match (raw.double, raw.int) {
                (Some(d), _) if !d.is_finite() => Err(format!("invalid double value in number data point {name:?}, either NaN or Inf")),
                (Some(d), _) => Ok((Value::from(d), if counter { "counter_double" } else { "gauge_double" })),
                (None, Some(i)) => Ok((Value::from(i), if counter { "counter_long" } else { "gauge_long" })),
                (None, None) => Err(format!("invalid number data point {name:?}, wrong ValueType Empty")),
            };
        }
        Shape::Summary => {
            let sum = raw.sum.unwrap_or(0.0);
            if !sum.is_finite() {
                return Err(format!("summary {name:?} has a sum that isn't finite"));
            }
            return Ok((obj! { "sum" => sum, "value_count" => clamp(raw.count) }, "summary"));
        }
        Shape::Histogram => {
            let (bc, eb) = (&raw.buckets, &raw.bounds);
            if !bc.is_empty() && bc.len() != eb.len() + 1 {
                return Err(format!("histogram {name:?} bucket count must be one greater than explicit bounds count, ignoring"));
            }
            if eb.windows(2).any(|w| !(w[0] < w[1])) {
                return Err(format!("histogram {name:?} bounds are not sorted or are not unique, ignoring"));
            }
            if eb.is_empty() {
                if raw.count > 0 {
                    counts.push(clamp(raw.count));
                    values.push(raw.sum.unwrap_or(0.0) / raw.count as f64);
                }
            } else {
                for (i, &c) in bc.iter().enumerate().filter(|(_, c)| **c > 0) {
                    counts.push(clamp(c));
                    values.push(midpoint(eb, i));
                }
            }
        }
        Shape::Exponential => {
            if !(-10..=20).contains(&raw.scale) {
                return Err(format!("exponential histogram {name:?} has a scale outside -10..20"));
            }
            let (offset, neg) = (&raw.negative.0, &raw.negative.1);
            for (i, &c) in neg.iter().enumerate().rev().filter(|(_, c)| **c > 0) {
                let (lb, ub) = (-lower_boundary(offset + i as i64 + 1, raw.scale), -lower_boundary(offset + i as i64, raw.scale));
                counts.push(clamp(c));
                values.push(lb + (ub - lb) / 2.0);
            }
            if raw.zero > 0 {
                counts.push(clamp(raw.zero));
                values.push(0.0);
            }
            let (offset, pos) = (&raw.positive.0, &raw.positive.1);
            for (i, &c) in pos.iter().enumerate().filter(|(_, c)| **c > 0) {
                let (lb, ub) = (lower_boundary(offset + i as i64, raw.scale), lower_boundary(offset + i as i64 + 1, raw.scale));
                counts.push(clamp(c));
                values.push(lb + (ub - lb) / 2.0);
            }
        }
    }
    if values.iter().any(|v| !v.is_finite()) {
        return Err(format!("histogram {name:?} has a bucket whose value isn't finite"));
    }
    let v = obj! { "counts" => Value::Arr(counts.into_iter().map(Value::from).collect()), "values" => Value::Arr(values.into_iter().map(Value::from).collect()) };
    Ok((v, "histogram"))
}

/// Adds a decoded point to its scope's documents (or rejects it).
#[allow(clippy::too_many_arguments)]
fn add(out: &mut Batch, budget: &mut Budget, groups: &mut Groups, m: &Metric, mut raw: Raw, scope: &Rendered, resource: &Rendered, route: Option<&str>, now_ms: i64) -> Result<(), Fail> {
    let (value, template) = match value_of(&raw, m) {
        Ok(v) => v,
        Err(why) => {
            out.reject(why);
            return Ok(());
        }
    };
    if let Value::Obj(o) = &value {
        let entries: usize = o.values().map(|v| v.as_array().map_or(1, Vec::len)).sum();
        budget.text(entries * VALUE_COST)?;
    }
    let (mut dataset, namespace) = target_from(&raw.attributes, scope, resource);
    let explicit = raw.attributes.get("data_stream.dataset").and_then(Value::as_str).is_some_and(|d| !dataset_base(d).is_empty()) || scope.dataset.is_some() || resource.dataset.is_some();
    if let (false, Some(r)) = (explicit, route) {
        dataset = format!("{r}.otel");
    }
    let Some(index) = super::logs::admit(out, "metrics", &dataset, &namespace) else { return Ok(()) };
    dimensions(&mut raw.attributes);
    let time = if raw.time > 0 { raw.time } else { (now_ms.max(0) as u64) * 1_000_000 };
    let temporality = m.temporality();
    let mut key = format!("{index}\n{time}\n{}\n{}\n{}\n", raw.start, m.unit, temporality.unwrap_or_default());
    let mut pairs: Vec<_> = raw.attributes.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in pairs {
        key.push_str(&Value::from(k.as_str()).to_json());
        key.push('=');
        key.push_str(&v.to_json());
        key.push('\n');
    }
    budget.text(key.len())?;
    let at = match groups.by_key.get(&key) {
        Some(&at) => at,
        None => {
            groups.list.push(Group {
                index,
                dataset,
                namespace,
                time,
                start: raw.start,
                unit: m.unit.clone(),
                temporality,
                attributes: std::mem::take(&mut raw.attributes),
                metrics: Map::new(),
                templates: Map::new(),
            });
            groups.by_key.insert(key, groups.list.len() - 1);
            groups.list.len() - 1
        }
    };
    let g = &mut groups.list[at];
    let name = m.name.clone().unwrap_or_default();
    if g.metrics.contains_key(&name) {
        out.reject(format!("metric with name '{name}' has already been serialized in document with timestamp {}", super::logs::timestamp(time)));
        return Ok(());
    }
    g.templates.insert(format!("metrics.{name}"), Value::from(template));
    g.metrics.insert(name, value);
    Ok(())
}

/// Writes a scope's documents.
fn flush(out: &mut Batch, budget: &mut Budget, groups: Groups, scope: &Rendered, resource: &Rendered) -> Result<(), Fail> {
    for g in groups.list {
        let mut names: Vec<&String> = g.metrics.keys().collect();
        names.sort();
        let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
        for n in &names {
            hasher.update(n.as_bytes());
        }
        let hash = format!("{:x}", hasher.digest());
        let weight = g.metrics.len() as u64;
        let mut create = Map::new();
        create.insert("_index".into(), Value::from(g.index));
        if !g.unit.is_empty() {
            let params: Map = g.templates.keys().map(|k| (k.clone(), obj! { "unit" => g.unit.as_str() })).collect();
            create.insert("dynamic_template_params".into(), Value::Obj(params));
        }
        create.insert("dynamic_templates".into(), Value::Obj(g.templates));
        let action = obj! { "create" => Value::Obj(create) }.to_json();
        let mut d = Map::new();
        d.insert("@timestamp".into(), Value::from(g.time / 1_000_000));
        if g.start != 0 {
            d.insert("start_timestamp".into(), Value::from(g.start / 1_000_000));
        }
        if !g.unit.is_empty() {
            d.insert("unit".into(), Value::from(g.unit));
        }
        if let Some(t) = g.temporality {
            d.insert("temporality".into(), Value::from(t));
        }
        d.insert("data_stream".into(), obj! { "type" => "metrics", "dataset" => g.dataset, "namespace" => g.namespace });
        if !g.attributes.is_empty() {
            d.insert("attributes".into(), Value::Obj(g.attributes));
        }
        d.insert("metrics".into(), Value::Obj(g.metrics));
        d.insert("_metric_names_hash".into(), Value::from(hash));
        push_line(out, budget, action, Value::Obj(d), scope, resource)?;
        out.weights.push(weight);
    }
    Ok(())
}

/// A decoded context rendered for metrics (with its schema URL), or its rejection.
fn finish(ctx: Result<Context, String>, schema: Option<String>, scope: bool, budget: &mut Budget) -> Result<Result<Rendered, String>, Fail> {
    match ctx {
        Ok(mut c) => {
            if schema.is_some() {
                c.schema_url = schema;
            }
            Ok(Ok(render_as(c, scope, true, budget)?))
        }
        Err(why) => {
            budget.record_done();
            Ok(Err(why))
        }
    }
}

/// Where a scope's points go: its documents, and what they're added with.
struct Scoped<'a> {
    resource: &'a Result<Rendered, String>,
    scope: &'a Result<Rendered, String>,
    route: Option<&'a str>,
    groups: Groups,
    now_ms: i64,
}

impl Scoped<'_> {
    fn take(&mut self, out: &mut Batch, budget: &mut Budget, m: &Metric, raw: Step<Raw>) -> Result<(), Fail> {
        match (raw, self.resource, self.scope) {
            (Step::Stop(f), _, _) => return Err(f),
            (_, Err(why), _) | (_, _, Err(why)) => out.reject(why.clone()),
            (raw, Ok(res), Ok(sc)) => match (m.refused(), raw) {
                (Some(why), _) | (None, Step::Reject(Rejected(why))) => out.reject(why),
                (None, Step::Ok(raw)) => add(out, budget, &mut self.groups, m, raw, sc, res, self.route, self.now_ms)?,
                (None, Step::Stop(f)) => return Err(f),
            },
        }
        budget.point_done();
        Ok(())
    }

    fn flush(self, out: &mut Batch, budget: &mut Budget) -> Result<(), Fail> {
        if let (Ok(res), Ok(sc)) = (self.resource, self.scope) {
            flush(out, budget, self.groups, sc, res)?;
        }
        budget.record_done();
        Ok(())
    }
}

// --- protobuf ---------------------------------------------------------------------------------

macro_rules! wire_m {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(m) => return Step::Stop(Fail::from(m)),
        }
    };
}

macro_rules! step_m {
    ($e:expr) => {
        match $e {
            Step::Ok(v) => v,
            Step::Reject(r) => return Step::Reject(r),
            Step::Stop(f) => return Step::Stop(f),
        }
    };
}

/// A repeated `fixed64` or `double`, packed or not, each entry charged.
fn fixed64s(w: Wire<'_>, out: &mut Vec<u64>, budget: &mut Budget) -> Step<()> {
    match w {
        Wire::Bytes(b) => {
            if b.len() % 8 != 0 {
                return Step::Stop(Fail::Malformed("a packed fixed64 field isn't a multiple of 8 bytes".into()));
            }
            for c in b.chunks_exact(8) {
                step_m!(budget.value(0));
                out.push(u64::from_le_bytes(c.try_into().unwrap_or_default()));
            }
        }
        w => {
            step_m!(budget.value(0));
            out.push(wire_m!(w.uint()));
        }
    }
    Step::Ok(())
}

/// A repeated `uint64`, packed or not, each entry charged.
fn varints(w: Wire<'_>, out: &mut Vec<u64>, budget: &mut Budget) -> Step<()> {
    match w {
        Wire::Bytes(mut b) => {
            while !b.is_empty() {
                step_m!(budget.value(0));
                out.push(wire_m!(varint(&mut b)));
            }
        }
        w => {
            step_m!(budget.value(0));
            out.push(wire_m!(w.uint()));
        }
    }
    Step::Ok(())
}

fn sint32(v: u64) -> i64 {
    let v = v as u32;
    i64::from((v >> 1) as i32 ^ -((v & 1) as i32))
}

/// `Buckets { sint32 offset = 1; repeated uint64 bucket_counts = 2; }`.
fn buckets_pb(b: &[u8], budget: &mut Budget) -> Step<(i64, Vec<u64>)> {
    let mut out = (0, Vec::new());
    for f in Fields::new(b) {
        let (n, w) = wire_m!(f);
        match n {
            1 => out.0 = sint32(wire_m!(w.uint())),
            2 => step_m!(varints(w, &mut out.1, budget)),
            _ => {}
        }
    }
    Step::Ok(out)
}

/// A `NumberDataPoint`, `HistogramDataPoint`, `ExponentialHistogramDataPoint` or
/// `SummaryDataPoint` (each has `start_time_unix_nano = 2`, `time_unix_nano = 3`; the rest by
/// shape, below).
fn point_pb(b: &[u8], shape: Shape, budget: &mut Budget, batch: &mut Batch) -> Step<Raw> {
    let mut r = Raw::default();
    let attributes = match shape {
        Shape::Gauge | Shape::Sum | Shape::Summary => 7,
        Shape::Histogram => 9,
        Shape::Exponential => 1,
    };
    for f in Fields::new(b) {
        let (n, w) = wire_m!(f);
        match (shape, n) {
            (_, n) if n == attributes => {
                if let Some((k, v)) = step_m!(key_value_pb(wire_m!(w.bytes()), 1, budget, batch)) {
                    add_attribute(&mut r.attributes, &mut r.dropped, k, v);
                }
            }
            (_, 2) => r.start = wire_m!(w.uint()),
            (_, 3) => r.time = wire_m!(w.uint()),
            (Shape::Gauge | Shape::Sum, 4) => (r.double, r.int) = (Some(f64::from_bits(wire_m!(w.uint()))), None),
            (Shape::Gauge | Shape::Sum, 6) => (r.double, r.int) = (None, Some(wire_m!(w.uint()) as i64)),
            (Shape::Histogram | Shape::Exponential | Shape::Summary, 4) => r.count = wire_m!(w.uint()),
            (Shape::Histogram | Shape::Exponential | Shape::Summary, 5) => r.sum = Some(f64::from_bits(wire_m!(w.uint()))),
            (Shape::Histogram, 6) => step_m!(fixed64s(w, &mut r.buckets, budget)),
            (Shape::Histogram, 7) => {
                let mut bits = Vec::new();
                step_m!(fixed64s(w, &mut bits, budget));
                r.bounds.extend(bits.into_iter().map(f64::from_bits));
            }
            (Shape::Exponential, 6) => r.scale = sint32(wire_m!(w.uint())),
            (Shape::Exponential, 7) => r.zero = wire_m!(w.uint()),
            (Shape::Exponential, 8) => r.positive = step_m!(buckets_pb(wire_m!(w.bytes()), budget)),
            (Shape::Exponential, 9) => r.negative = step_m!(buckets_pb(wire_m!(w.bytes()), budget)),
            _ => {}
        }
    }
    Step::Ok(r)
}

/// A `Metric` (`name = 1`, `unit = 3`; its data: `gauge = 5`, `sum = 7`, `histogram = 9`,
/// `exponential_histogram = 10`, `summary = 11`, each with `data_points = 1`, and
/// `aggregation_temporality = 2` and (a sum's) `is_monotonic = 3`).
fn metric_pb(b: &[u8], out: &mut Batch, budget: &mut Budget, at: &mut Scoped<'_>) -> Result<(), Fail> {
    let (mut name, mut unit) = (None, String::new());
    for f in Fields::new(b) {
        let (n, w) = f?;
        match n {
            1 => name = Some(text(w.string()?, budget, out)?).filter(|n| !n.is_empty()),
            3 => unit = text(w.string()?, budget, out)?,
            _ => {}
        }
    }
    for f in Fields::new(b) {
        let (n, w) = f?;
        let shape = match n {
            5 => Shape::Gauge,
            7 => Shape::Sum,
            9 => Shape::Histogram,
            10 => Shape::Exponential,
            11 => Shape::Summary,
            _ => continue,
        };
        let data = w.bytes()?;
        let mut m = Metric { name: name.clone(), unit: unit.clone(), shape, temporality: 0, monotonic: false };
        for g in Fields::new(data) {
            let (gn, gw) = g?;
            match gn {
                2 if shape != Shape::Gauge && shape != Shape::Summary => m.temporality = gw.uint()?,
                3 if shape == Shape::Sum => m.monotonic = gw.uint()? != 0,
                _ => {}
            }
        }
        for g in Fields::new(data) {
            let (gn, gw) = g?;
            if gn == 1 {
                let raw = point_pb(gw.bytes()?, shape, budget, out);
                at.take(out, budget, &m, raw)?;
            }
        }
    }
    Ok(())
}

/// The documents of an `ExportMetricsServiceRequest` in protobuf (`resource_metrics = 1`; each
/// `ResourceMetrics`: `resource = 1`, `scope_metrics = 2`, `schema_url = 3`; each
/// `ScopeMetrics`: `scope = 1`, `metrics = 2`, `schema_url = 3`), a scope at a time.
pub fn from_protobuf(body: &[u8], now_ms: i64, budget: &mut Budget) -> Result<Batch, Fail> {
    let mut out = Batch::default();
    for f in Fields::new(body) {
        let (n, w) = f?;
        if n != 1 {
            continue;
        }
        let rm = w.bytes()?;
        let mut ctx: Result<Context, String> = Ok(Context::default());
        let mut schema = None;
        for g in Fields::new(rm) {
            let (gn, gw) = g?;
            match gn {
                1 => ctx = context_or(context_pb(gw.bytes()?, false, budget, &mut out))?,
                3 => schema = Some(text(gw.string()?, budget, &mut out)?),
                _ => {}
            }
        }
        let resource = finish(ctx, schema, false, budget)?;
        for g in Fields::new(rm) {
            let (gn, gw) = g?;
            if gn != 2 {
                continue;
            }
            let sm = gw.bytes()?;
            let mut ctx: Result<Context, String> = Ok(Context::default());
            let mut schema = None;
            for h in Fields::new(sm) {
                let (hn, hw) = h?;
                match hn {
                    1 => ctx = context_or(context_pb(hw.bytes()?, true, budget, &mut out))?,
                    3 => schema = Some(text(hw.string()?, budget, &mut out)?),
                    _ => {}
                }
            }
            let route = ctx.as_ref().ok().and_then(scope_route);
            let scope = finish(ctx, schema, true, budget)?;
            let mut at = Scoped { resource: &resource, scope: &scope, route: route.as_deref(), groups: Groups::default(), now_ms };
            for h in Fields::new(sm) {
                let (hn, hw) = h?;
                if hn == 2 {
                    metric_pb(hw.bytes()?, &mut out, budget, &mut at)?;
                }
            }
            at.flush(&mut out, budget)?;
            drop_rendered(&scope, budget);
        }
        drop_rendered(&resource, budget);
    }
    Ok(out.finish())
}

// --- JSON -----------------------------------------------------------------------------------

/// A JSON number, or the strings OTLP/JSON writes for the rest (`"NaN"`, `"Infinity"`).
fn f64_json(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
}

fn enum_json(v: Option<&Value>) -> u64 {
    match v.and_then(Value::as_str) {
        Some(s) if s.ends_with("DELTA") => DELTA,
        Some(s) if s.ends_with("CUMULATIVE") => CUMULATIVE,
        Some(s) => s.parse().unwrap_or(0),
        None => uint_json(v),
    }
}

fn uints_json(v: Option<&Value>, out: &mut Vec<u64>, budget: &mut Budget) -> Step<()> {
    for x in v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        step_m!(budget.value(0));
        out.push(uint_json(Some(x)));
    }
    Step::Ok(())
}

fn buckets_json(v: Option<&Value>, budget: &mut Budget) -> Step<(i64, Vec<u64>)> {
    let Some(v) = v else { return Step::Ok((0, Vec::new())) };
    let offset = v.get("offset").and_then(|o| o.as_i64().or_else(|| o.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
    let mut counts = Vec::new();
    step_m!(uints_json(get(v, "bucketCounts", "bucket_counts"), &mut counts, budget));
    Step::Ok((offset.clamp(i64::from(i32::MIN), i64::from(i32::MAX)), counts))
}

fn point_json(v: &Value, budget: &mut Budget, batch: &mut Batch) -> Step<Raw> {
    let mut r = Raw {
        time: uint_json(get(v, "timeUnixNano", "time_unix_nano")),
        start: uint_json(get(v, "startTimeUnixNano", "start_time_unix_nano")),
        count: uint_json(v.get("count")),
        sum: f64_json(v.get("sum")),
        zero: uint_json(get(v, "zeroCount", "zero_count")),
        scale: v.get("scale").and_then(Value::as_i64).unwrap_or(0),
        ..Raw::default()
    };
    if let Some(d) = get(v, "asDouble", "as_double") {
        r.double = Some(f64_json(Some(d)).unwrap_or(f64::NAN));
    } else if let Some(i) = get(v, "asInt", "as_int") {
        r.int = i.as_i64().or_else(|| i.as_str().and_then(|s| s.parse().ok()));
    }
    step_m!(attributes_json(v.get("attributes"), &mut r.attributes, &mut r.dropped, budget, batch));
    step_m!(uints_json(get(v, "bucketCounts", "bucket_counts"), &mut r.buckets, budget));
    for b in get(v, "explicitBounds", "explicit_bounds").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        step_m!(budget.value(0));
        r.bounds.push(f64_json(Some(b)).unwrap_or(f64::NAN));
    }
    r.positive = step_m!(buckets_json(v.get("positive"), budget));
    r.negative = step_m!(buckets_json(v.get("negative"), budget));
    Step::Ok(r)
}

fn metric_json(v: &Value, out: &mut Batch, budget: &mut Budget, at: &mut Scoped<'_>) -> Result<(), Fail> {
    let name = str_json(v.get("name"), budget, out)?.filter(|n| !n.is_empty());
    let unit = str_json(v.get("unit"), budget, out)?.unwrap_or_default();
    let shapes = [
        ("gauge", "gauge", Shape::Gauge),
        ("sum", "sum", Shape::Sum),
        ("histogram", "histogram", Shape::Histogram),
        ("exponentialHistogram", "exponential_histogram", Shape::Exponential),
        ("summary", "summary", Shape::Summary),
    ];
    for (camel, snake, shape) in shapes {
        let Some(data) = get(v, camel, snake) else { continue };
        let m = Metric {
            name: name.clone(),
            unit: unit.clone(),
            shape,
            temporality: if matches!(shape, Shape::Gauge | Shape::Summary) { 0 } else { enum_json(get(data, "aggregationTemporality", "aggregation_temporality")) },
            monotonic: shape == Shape::Sum && get(data, "isMonotonic", "is_monotonic").and_then(Value::as_bool).unwrap_or(false),
        };
        for p in get(data, "dataPoints", "data_points").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            let raw = point_json(p, budget, out);
            at.take(out, budget, &m, raw)?;
        }
    }
    Ok(())
}

/// The documents of an `ExportMetricsServiceRequest` in OTLP/JSON, from its parsed tree.
pub fn from_json(body: &Value, now_ms: i64, budget: &mut Budget) -> Result<Batch, Fail> {
    let mut out = Batch::default();
    let Some(list) = get(body, "resourceMetrics", "resource_metrics") else { return Ok(out.finish()) };
    let Some(list) = list.as_array() else { return Err(Fail::Malformed("resourceMetrics isn't a list".into())) };
    for rm in list {
        let ctx = context_or(context_json_in(rm.get("resource"), get(rm, "schemaUrl", "schema_url"), false, budget, &mut out))?;
        let resource = finish(ctx, None, false, budget)?;
        for sm in get(rm, "scopeMetrics", "scope_metrics").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            let ctx = context_or(context_json_in(sm.get("scope"), get(sm, "schemaUrl", "schema_url"), true, budget, &mut out))?;
            let route = ctx.as_ref().ok().and_then(scope_route);
            let scope = finish(ctx, None, true, budget)?;
            let mut at = Scoped { resource: &resource, scope: &scope, route: route.as_deref(), groups: Groups::default(), now_ms };
            for m in sm.get("metrics").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                metric_json(m, &mut out, budget, &mut at)?;
            }
            at.flush(&mut out, budget)?;
            drop_rendered(&scope, budget);
        }
        drop_rendered(&resource, budget);
    }
    Ok(out.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::{MemoryBudget, Reservation};

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }
    fn field(n: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(n) << 3 | 2, &mut out);
        varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }
    fn fixed64(n: u32, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(n) << 3 | 1, &mut out);
        out.extend_from_slice(&v.to_le_bytes());
        out
    }
    fn uint(n: u32, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(n) << 3, &mut out);
        varint(v, &mut out);
        out
    }
    fn string_kv(k: &str, v: &str) -> Vec<u8> {
        [field(1, k.as_bytes()), field(2, &field(1, v.as_bytes()))].concat()
    }
    fn budget() -> Budget {
        Budget::new(Reservation::new(MemoryBudget::new(u64::MAX)))
    }
    fn docs(b: &Batch) -> Vec<(Value, Value)> {
        let all: String = b.chunks.concat();
        let lines: Vec<&str> = all.lines().collect();
        lines.chunks(2).map(|c| (crate::json::parse(c[0].as_bytes()).unwrap().unwrap(), crate::json::parse(c[1].as_bytes()).unwrap().unwrap())).collect()
    }
    fn request(scope_name: &str, metrics: &[Vec<u8>]) -> Vec<u8> {
        let mut sm = field(1, &field(1, scope_name.as_bytes()));
        for m in metrics {
            sm.extend(field(2, m));
        }
        field(1, &[field(1, &field(1, &string_kv("service.name", "checkout"))), field(2, &sm)].concat())
    }

    const T: u64 = 1_700_000_000_000_000_000;

    #[test]
    fn protobuf_and_json_points_become_the_same_grouped_documents() {
        let attr = field(7, &string_kv("host", "a"));
        let gauge = [field(1, b"cpu"), field(3, b"1"), field(5, &field(1, &[fixed64(3, T), fixed64(4, 0.5f64.to_bits()), attr.clone()].concat()))].concat();
        let sum = [field(1, b"requests"), field(3, b"1"), field(7, &[field(1, &[fixed64(3, T), fixed64(6, 7), attr].concat()), uint(2, 2), uint(3, 1)].concat())].concat();
        let bounds: Vec<u8> = [1.0f64, 10.0].iter().flat_map(|b| b.to_le_bytes()).collect();
        let counts: Vec<u8> = [2u64, 0, 3].iter().flat_map(|c| c.to_le_bytes()).collect();
        let hist = [
            field(1, b"latency"),
            field(3, b"ms"),
            field(9, &[field(1, &[fixed64(2, T - 1_000_000_000), fixed64(3, T), fixed64(4, 5), fixed64(5, 40f64.to_bits()), field(6, &counts), field(7, &bounds)].concat()), uint(2, 1)].concat()),
        ]
        .concat();
        let pb = from_protobuf(&request("github.com/open-telemetry/opentelemetry-collector-contrib/receiver/hostmetricsreceiver/internal/scraper/cpuscraper", &[gauge, sum, hist]), 0, &mut budget()).unwrap();
        let json = crate::json::parse(
            br#"{"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},"scopeMetrics":[{
                "scope":{"name":"github.com/open-telemetry/opentelemetry-collector-contrib/receiver/hostmetricsreceiver/internal/scraper/cpuscraper"},"metrics":[
                {"name":"cpu","unit":"1","gauge":{"dataPoints":[{"timeUnixNano":"1700000000000000000","asDouble":0.5,"attributes":[{"key":"host","value":{"stringValue":"a"}}]}]}},
                {"name":"requests","unit":"1","sum":{"aggregationTemporality":"AGGREGATION_TEMPORALITY_CUMULATIVE","isMonotonic":true,"dataPoints":[{"timeUnixNano":"1700000000000000000","asInt":"7","attributes":[{"key":"host","value":{"stringValue":"a"}}]}]}},
                {"name":"latency","unit":"ms","histogram":{"aggregationTemporality":1,"dataPoints":[{"startTimeUnixNano":"1699999999000000000","timeUnixNano":"1700000000000000000","count":"5","sum":40,"bucketCounts":["2","0","3"],"explicitBounds":[1,10]}]}}]}]}]}"#,
        )
        .unwrap()
        .unwrap();
        let js = from_json(&json, 0, &mut budget()).unwrap();
        let (a, b) = (docs(&pb), docs(&js));
        assert_eq!(a, b);
        // The gauge and the counter share a document (same attributes, timestamp and unit).
        assert_eq!((a.len(), pb.weights.clone(), pb.rejected), (2, vec![2, 1], 0));
        let (action, doc) = &a[0];
        assert_eq!(action.pointer(&["create", "_index"]).and_then(Value::as_str), Some("metrics-hostmetricsreceiver.otel-default"));
        assert_eq!(action.pointer(&["create", "dynamic_templates", "metrics.cpu"]).and_then(Value::as_str), Some("gauge_double"));
        assert_eq!(action.pointer(&["create", "dynamic_templates", "metrics.requests"]).and_then(Value::as_str), Some("counter_long"));
        assert_eq!(action.pointer(&["create", "dynamic_template_params", "metrics.cpu", "unit"]).and_then(Value::as_str), Some("1"));
        assert_eq!(doc.get("@timestamp").and_then(Value::as_i64), Some(1_700_000_000_000));
        assert_eq!(doc.pointer(&["metrics", "requests"]).and_then(Value::as_i64), Some(7));
        assert_eq!(doc.pointer(&["attributes", "host"]).and_then(Value::as_str), Some("a"));
        let mut h = xxhash_rust::xxh64::Xxh64::new(0);
        h.update(b"cpurequests");
        assert_eq!(doc.get("_metric_names_hash").and_then(Value::as_str), Some(format!("{:x}", h.digest()).as_str()));
        let (action, doc) = &a[1];
        assert_eq!(action.pointer(&["create", "dynamic_templates", "metrics.latency"]).and_then(Value::as_str), Some("histogram"));
        let floats = |k: &str| doc.pointer(&["metrics", "latency", k]).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>());
        assert_eq!((floats("counts"), floats("values")), (Some(vec![2.0, 3.0]), Some(vec![0.5, 10.0])));
        assert_eq!((doc.get("start_timestamp").and_then(Value::as_i64), doc.get("temporality").and_then(Value::as_str)), (Some(1_699_999_999_000), Some("delta")));
    }

    #[test]
    fn points_that_cant_be_written_are_counted_not_dropped() {
        // A cumulative histogram, a NaN gauge, a duplicate name in one document, a point past
        // MAX_VALUES buckets: each rejected and counted; the good point still lands.
        let cumulative = [field(1, b"h"), field(9, &[field(1, &fixed64(3, T)), uint(2, 2)].concat())].concat();
        let nan = [field(1, b"g"), field(5, &field(1, &[fixed64(3, T), fixed64(4, f64::NAN.to_bits())].concat()))].concat();
        let ok = [field(1, b"x"), field(5, &field(1, &[fixed64(3, T), fixed64(6, 1)].concat()))].concat();
        let wide: Vec<u8> = (0..super::super::logs::MAX_VALUES + 1).flat_map(|_| 1u64.to_le_bytes()).collect();
        let wide = [field(1, b"w"), field(9, &[field(1, &[fixed64(3, T), field(6, &wide)].concat()), uint(2, 1)].concat())].concat();
        let out = from_protobuf(&request("lib", &[cumulative, nan, ok.clone(), ok, wide]), 0, &mut budget()).unwrap();
        assert_eq!((out.accepted, out.rejected), (1, 4));
        assert!(out.reason.unwrap().contains("cumulative"));
    }

    #[test]
    fn exponential_buckets_sit_at_their_midpoints() {
        // Scale 0: bucket i is (2^i, 2^(i+1)]; offset 1 puts the two buckets at (2,4] and (4,8].
        let raw = Raw { positive: (1, vec![1, 2]), negative: (0, vec![4]), zero: 3, count: 10, ..Raw::default() };
        let m = Metric { name: Some("e".into()), unit: String::new(), shape: Shape::Exponential, temporality: DELTA, monotonic: false };
        let (v, t) = value_of(&raw, &m).unwrap();
        assert_eq!(t, "histogram");
        assert_eq!(v.get("values").map(Value::to_json), Some(Value::Arr([-1.5, 0.0, 3.0, 6.0].map(Value::from).to_vec()).to_json()));
        assert_eq!(v.get("counts").map(Value::to_json).as_deref(), Some("[4,3,1,2]"));
    }
}

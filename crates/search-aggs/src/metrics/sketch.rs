//! t-digest backed single metrics: `median_absolute_deviation` and `boxplot`.
//!
//! Both keep one [`TDigest`] per bucket, like `percentiles`. The median absolute deviation is
//! computed when a result is built (shard or reduce): the median of the values sketch, then the
//! median of a second sketch fed each centroid's distance to it with the centroid's weight
//! (`InternalMedianAbsoluteDeviation.computeMedianAbsoluteDeviation`, which reads the sketch
//! without compressing it first). The boxplot reports min, max, quartiles and the whiskers (the
//! extreme centroid means within 1.5 IQR of the quartiles).

use std::cell::RefCell;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, multi_value_validate_sort, single_value_validate_sort};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx, single_value_property};
use crate::json::{Json, java_double};
use crate::memory::MemTracker;
use crate::metrics::simple::multi_value_property;
use crate::exp_histogram::ExpHistogramReader;
use crate::metrics::tdigest::TDigest;
use crate::metrics::union_state::UnionState;
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// Which sketch metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SketchKind {
    Mad,
    Boxplot,
}

impl SketchKind {
    fn type_name(self) -> &'static str {
        match self {
            SketchKind::Mad => "median_absolute_deviation",
            SketchKind::Boxplot => "boxplot",
        }
    }
}

/// A parsed `median_absolute_deviation` / `boxplot`.
#[derive(Clone, Debug)]
pub struct SketchSpec {
    pub kind: SketchKind,
    pub vs: VsSpec,
    pub compression: f64,
    pub high_accuracy: bool,
}

/// Parses `median_absolute_deviation` (compression 1000) or `boxplot` (compression 100).
pub fn parse(p: &mut XParser<'_>, kind: SketchKind, name: &str) -> Result<SketchSpec> {
    let opts = VsOpts::new(true, true, false);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new("compression", Tokens::FLOAT));
    fields.push(Field::new("execution_hint", Tokens::STRING));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut vs = VsSpec::default();
    let mut compression = if kind == SketchKind::Mad { 1000.0 } else { 100.0 };
    let mut high_accuracy = false;
    p.object(kind.type_name(), &fields, |p, i| {
        let f = names[i];
        if vs.parse_field(p, f)? {
            return Ok(());
        }
        if f == "compression" {
            let c = p.double_value()?;
            match kind {
                SketchKind::Mad if c <= 0.0 => {
                    return Err(AggError::illegal_argument(format!("[compression] must be greater than 0. Found [{}] in [{name}]", java_double(c))));
                }
                SketchKind::Boxplot if c < 0.0 => {
                    return Err(AggError::illegal_argument(format!(
                        "[compression] must be greater than or equal to 0. Found [{}] in [{name}]",
                        java_double(c)
                    )));
                }
                _ => {}
            }
            compression = c;
        } else {
            let h = p.text()?;
            high_accuracy = match h.as_str() {
                "high_accuracy" | "HIGH_ACCURACY" => true,
                "default" | "" | "DEFAULT" => false,
                _ => return Err(AggError::illegal_argument(format!("Invalid execution_hint [{h}], valid values are [default, high_accuracy]"))),
            };
        }
        Ok(())
    })?;
    vs.check_required(opts)?;
    Ok(SketchSpec { kind, vs, compression, high_accuracy })
}

/// `computeMedianAbsoluteDeviation` (reads, and so merges/sorts, the sketch).
fn median_absolute_deviation(sketch: &mut TDigest) -> f64 {
    if sketch.size() == 0 {
        return f64::NAN;
    }
    let median = sketch.quantile(0.5);
    let mut deviations = sketch.empty_like();
    for (mean, count) in sketch.centroids() {
        // Deviations of finite values are finite: adding can't fail.
        let _ = deviations.add_weighted((median - mean).abs(), count);
    }
    deviations.quantile(0.5)
}

/// `InternalMedianAbsoluteDeviation`.
#[derive(Clone, Debug)]
pub struct MedianAbsoluteDeviation {
    pub sketch: TDigest,
    pub value: f64,
    pub format: DocValueFormat,
}

impl MedianAbsoluteDeviation {
    fn new(mut sketch: TDigest, format: DocValueFormat) -> MedianAbsoluteDeviation {
        let value = median_absolute_deviation(&mut sketch);
        MedianAbsoluteDeviation { sketch, value, format }
    }
}

impl Internal for MedianAbsoluteDeviation {
    fn type_name(&self) -> &'static str {
        "median_absolute_deviation"
    }
    fn class_name(&self) -> &'static str {
        "InternalMedianAbsoluteDeviation"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut merged = list[0].sketch.empty_like();
        let format = list[0].format.clone();
        for m in &mut list {
            merged.merge(&mut m.sketch)?;
        }
        Ok(MedianAbsoluteDeviation::new(merged, format))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let any = self.sketch.size() > 0;
        out.push("value", if any { Json::Float(self.value) } else { Json::Null });
        if !self.format.is_raw() && any {
            out.push("value_as_string", self.format.double_json(self.value));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value, path)
    }
    fn ram_bytes(&self) -> usize {
        64 + self.sketch.ram_bytes()
    }
}

/// `InternalBoxplot.Metrics`.
const BOXPLOT_METRICS: [&str; 7] = ["min", "max", "q1", "q2", "q3", "lower", "upper"];

/// The seven boxplot values of a digest (`Metrics.value(state)`); `None` is an absent state.
fn boxplot_values(state: Option<&mut TDigest>) -> [f64; 7] {
    let Some(t) = state else {
        return [f64::NEG_INFINITY, f64::INFINITY, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN];
    };
    let q1 = t.quantile(0.25);
    let q2 = t.quantile(0.5);
    let q3 = t.quantile(0.75);
    let (lower, upper) = whiskers(q1, q3, &t.centroids(), t.max());
    [t.min(), t.max(), q1, q2, q3, lower, upper]
}

/// The seven values of a union state (`Metrics.value(state)`).
fn union_values(u: &UnionState) -> [f64; 7] {
    let q1 = u.quantile(0.25);
    let q2 = u.quantile(0.5);
    let q3 = u.quantile(0.75);
    let (lower, upper) = whiskers(q1, q3, &u.centroids(), u.max());
    [u.min(), u.max(), q1, q2, q3, lower, upper]
}

/// `InternalBoxplot.whiskers`.
fn whiskers(q1: f64, q3: f64, centroids: &[(f64, u64)], max: f64) -> (f64, f64) {
    let iqr = q3 - q1;
    let upper = q3 + 1.5 * iqr;
    let lower = q1 - 1.5 * iqr;
    let (mut lo, mut hi) = (f64::NAN, f64::NAN);
    let mut prev: Option<f64> = None;
    for &(mean, _) in centroids {
        if lo.is_nan() && mean > lower {
            lo = mean;
        }
        if mean > upper {
            // A first centroid above the upper fence leaves `prev` null in Java (an NPE);
            // the fence is above q3, so some centroid precedes it.
            hi = prev.unwrap_or(f64::NAN);
            break;
        }
        prev = Some(mean);
    }
    if hi.is_nan() {
        hi = max;
    }
    (lo, hi)
}

/// A boxplot's sketch: a digest, or a union state once exponential histograms take part.
#[derive(Clone, Debug)]
pub enum BoxState {
    TDigest(TDigest),
    Union(Box<UnionState>),
}

impl BoxState {
    fn compress(&mut self) {
        match self {
            BoxState::TDigest(t) => t.compress(),
            BoxState::Union(u) => u.compress(),
        }
    }

    fn ram_bytes(&self) -> usize {
        match self {
            BoxState::TDigest(t) => t.ram_bytes(),
            BoxState::Union(u) => u.ram_bytes(),
        }
    }
}

/// `InternalBoxplot`.
#[derive(Clone, Debug)]
pub struct Boxplot {
    pub state: BoxState,
    pub format: DocValueFormat,
}

impl Boxplot {
    fn new(mut state: BoxState, format: DocValueFormat) -> Boxplot {
        state.compress();
        Boxplot { state, format }
    }

    /// `min, max, q1, q2, q3, lower, upper` of the (compressed) state.
    pub fn values(&self) -> [f64; 7] {
        match &self.state {
            BoxState::TDigest(t) => {
                let q1 = t.quantile_compressed(0.25);
                let q2 = t.quantile_compressed(0.5);
                let q3 = t.quantile_compressed(0.75);
                let (lower, upper) = whiskers(q1, q3, &t.centroids_compressed(), t.max());
                [t.min(), t.max(), q1, q2, q3, lower, upper]
            }
            BoxState::Union(u) => union_values(u),
        }
    }

    /// `value(name)`: `Metrics.resolve(name)`.
    pub fn value(&self, name: &str) -> Result<f64> {
        let lower = name.to_lowercase();
        match BOXPLOT_METRICS.iter().position(|m| *m == lower) {
            Some(i) => Ok(self.values()[i]),
            None => Err(AggError::illegal_argument(format!(
                "No enum constant org.elasticsearch.xpack.analytics.boxplot.InternalBoxplot.Metrics.{}",
                name.to_uppercase()
            ))),
        }
    }
}

impl Internal for Boxplot {
    fn type_name(&self) -> &'static str {
        "boxplot"
    }
    fn class_name(&self) -> &'static str {
        "InternalBoxplot"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let format = list[0].format.clone();
        if list.iter().all(|b| matches!(b.state, BoxState::TDigest(_))) {
            let BoxState::TDigest(first) = &list[0].state else { unreachable!() };
            let mut merged = first.empty_like();
            for b in &mut list {
                if let BoxState::TDigest(t) = &mut b.state {
                    merged.merge(t)?;
                }
            }
            return Ok(Boxplot::new(BoxState::TDigest(merged), format));
        }
        // `createUsingParamsFrom(first)`, then every state added.
        let mut merged: Option<UnionState> = None;
        for b in list {
            let mut u = match b.state {
                BoxState::TDigest(t) => UnionState::wrap(t),
                BoxState::Union(u) => *u,
            };
            merged.get_or_insert_with(|| UnionState::like(&u)).add(&mut u)?;
        }
        Ok(Boxplot::new(BoxState::Union(Box::new(merged.expect("non-empty reduce"))), format))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let v = self.values();
        for (name, x) in BOXPLOT_METRICS.iter().zip(v) {
            out.push(*name, Json::Float(x));
        }
        if !self.format.is_raw() {
            for (name, x) in BOXPLOT_METRICS.iter().zip(v) {
                out.push(format!("{name}_as_string"), self.format.format_double(x));
            }
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_value_property(name, path, |k| self.value(k))
    }
    fn ram_bytes(&self) -> usize {
        64 + self.state.ram_bytes()
    }
}

/// Binds `median_absolute_deviation` / `boxplot`.
pub fn create(def: &AggDef, spec: &SketchSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Numeric, false)?;
    // `boxplot` also reads pre-aggregated `histogram` fields.
    let histogram = spec.kind == SketchKind::Boxplot && matches!(vs.vs_type, VsType::Histogram | VsType::ExponentialHistogram);
    if !vs.unmapped && !vs.vs_type.is_numeric() && !histogram {
        return Err(vs.unsupported(spec.kind.type_name()));
    }
    Ok(Box::new(SketchAgg {
        spec: spec.clone(),
        format: vs.format.clone(),
        histograms: None,
        exp: None,
        exp_states: Vec::new(),
        reader: None,
        vs,
        states: RefCell::new(Vec::new()),
        bytes: 0,
        tracker: ctx.tracker(def),
    }))
}

struct SketchAgg {
    spec: SketchSpec,
    format: DocValueFormat,
    vs: BoundVs,
    histograms: Option<crate::histogram_field::HistogramReader>,
    /// `exponential_histogram` fields (`ExponentialHistogramBoxplotAggregator`): union states.
    exp: Option<ExpHistogramReader>,
    exp_states: Vec<Option<UnionState>>,
    reader: Option<NumReader>,
    states: RefCell<Vec<Option<TDigest>>>,
    bytes: usize,
    tracker: MemTracker,
}

impl SketchAgg {
    fn new_state(&self) -> TDigest {
        TDigest::new(self.spec.compression, self.spec.high_accuracy)
    }

    fn result(&self, state: Option<TDigest>) -> AggValue {
        let state = state.unwrap_or_else(|| self.new_state());
        match self.spec.kind {
            SketchKind::Mad => AggValue::MedianAbsoluteDeviation(Box::new(MedianAbsoluteDeviation::new(state, self.format.clone()))),
            SketchKind::Boxplot => AggValue::Boxplot(Box::new(Boxplot::new(BoxState::TDigest(state), self.format.clone()))),
        }
    }

    fn is_exp(&self) -> bool {
        self.vs.vs_type == VsType::ExponentialHistogram && !self.vs.unmapped
    }

    fn exp_result(&self, state: Option<UnionState>) -> AggValue {
        let state = state.unwrap_or_else(|| UnionState::new(self.spec.compression, self.spec.high_accuracy));
        AggValue::Boxplot(Box::new(Boxplot::new(BoxState::Union(Box::new(state)), self.format.clone())))
    }

    fn collect_exp(&mut self, b: Batch<'_>, r: &mut ExpHistogramReader) -> Result<()> {
        for i in 0..b.len() {
            let Some(h) = r.histogram(b.docs[i])? else { continue };
            let ord = b.ords[i] as usize;
            if self.exp_states.len() <= ord {
                self.exp_states.resize_with(ord + 1, || None);
            }
            let (c, ha) = (self.spec.compression, self.spec.high_accuracy);
            let state = self.exp_states[ord].get_or_insert_with(|| UnionState::new(c, ha));
            let before = state.ram_bytes();
            state.add_exp(h);
            let after = state.ram_bytes();
            if after > before {
                self.bytes += after - before;
                self.tracker.ensure(self.bytes + self.exp_states.len() * 8)?;
            }
        }
        Ok(())
    }
}

impl Aggregator for SketchAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if self.is_exp() {
            self.exp = Some(ExpHistogramReader::open(seg, self.vs.field.as_ref().map(|f| f.number)));
            return Ok(());
        }
        if self.vs.vs_type == VsType::Histogram && !self.vs.unmapped {
            self.histograms = Some(crate::histogram_field::HistogramReader::open(seg, self.vs.field.as_ref().map(|f| f.number)));
            return Ok(());
        }
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(self.vs.numeric_reader(seg)?) };
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if let Some(mut r) = self.exp.take() {
            let res = self.collect_exp(b, &mut r);
            self.exp = Some(r);
            return res;
        }
        if let Some(mut hist) = self.histograms.take() {
            let r = (|| {
                let states = self.states.get_mut();
                for i in 0..b.len() {
                    let centroids = hist.centroids(b.docs[i])?;
                    if centroids.is_empty() {
                        continue;
                    }
                    let ord = b.ords[i] as usize;
                    if states.len() <= ord {
                        states.resize_with(ord + 1, || None);
                    }
                    let (compression, high_accuracy) = (self.spec.compression, self.spec.high_accuracy);
                    let state = states[ord].get_or_insert_with(|| TDigest::new(compression, high_accuracy));
                    let before = state.ram_bytes();
                    for &(v, c) in centroids {
                        state.add_weighted(v, c)?;
                    }
                    let after = state.ram_bytes();
                    if after > before {
                        self.bytes += after - before;
                        self.tracker.ensure(self.bytes + states.len() * 8)?;
                    }
                }
                Ok(())
            })();
            self.histograms = Some(hist);
            return r;
        }
        let Some(reader) = self.reader.as_mut() else { return Ok(()) };
        let states = self.states.get_mut();
        for i in 0..b.len() {
            let vals = reader.doubles(b.docs[i], b.score(i))?;
            if vals.is_empty() {
                continue;
            }
            let ord = b.ords[i] as usize;
            if states.len() <= ord {
                states.resize_with(ord + 1, || None);
            }
            let (compression, high_accuracy) = (self.spec.compression, self.spec.high_accuracy);
            let state = states[ord].get_or_insert_with(|| TDigest::new(compression, high_accuracy));
            let before = state.ram_bytes();
            for &v in vals {
                state.add(v)?;
            }
            let after = state.ram_bytes();
            if after > before {
                self.bytes += after - before;
                self.tracker.ensure(self.bytes + states.len() * 8)?;
            }
        }
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut out = Vec::with_capacity(owning.len());
        for &o in owning {
            if self.is_exp() {
                let state = self.exp_states.get_mut(o as usize).and_then(Option::take);
                out.push(self.exp_result(state));
                continue;
            }
            let state = self.states.get_mut().get_mut(o as usize).and_then(Option::take);
            out.push(self.result(state));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        if self.is_exp() {
            return self.exp_result(None);
        }
        self.result(None)
    }

    fn needs_scores(&self) -> bool {
        self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        match self.spec.kind {
            SketchKind::Mad => single_value_validate_sort(name, key, rest),
            SketchKind::Boxplot => multi_value_validate_sort(name, key, rest, |k| BOXPLOT_METRICS.contains(&k.to_lowercase().as_str())),
        }
    }

    fn sort_value(&self, key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        if self.is_exp() {
            let i = key.and_then(|k| BOXPLOT_METRICS.iter().position(|m| *m == k.to_lowercase())).unwrap_or(0);
            return match self.exp_states.get(ord as usize).and_then(Option::as_ref) {
                Some(u) => union_values(u)[i],
                None => boxplot_values(None)[i],
            };
        }
        let mut states = self.states.borrow_mut();
        let state = states.get_mut(ord as usize).and_then(Option::as_mut);
        match self.spec.kind {
            SketchKind::Mad => state.map_or(f64::NAN, median_absolute_deviation),
            SketchKind::Boxplot => {
                let i = key.and_then(|k| BOXPLOT_METRICS.iter().position(|m| *m == k.to_lowercase())).unwrap_or(0);
                boxplot_values(state)[i]
            }
        }
    }
}

//! `percentiles` and `percentile_ranks`, with the `tdigest` (default) or `hdr` method.
//!
//! Each bucket keeps a [`TDigest`] (hybrid sorted/merging digest, or the AVL tree digest for
//! `execution_hint: high_accuracy`) or a [`DoubleHistogram`]. Shard results compress their
//! digest (`AbstractInternalTDigestPercentiles` does it in its constructor); reduces build a
//! fresh digest of the first state's type and add every state's centroids, which is what makes
//! a single shard inside a bucket differ from the same shard at the top level — Elasticsearch
//! behaves the same way. Ordering terms by a percentile reads the digest before it's compressed
//! (with the internal compression), and the shard result then keeps that merge.

use std::cell::RefCell;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, multi_value_validate_sort};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx};
use crate::json::{Json, java_double};
use crate::memory::MemTracker;
use crate::metrics::hdr::DoubleHistogram;
use crate::metrics::simple::multi_value_property;
use crate::exp_histogram::ExpHistogramReader;
use crate::metrics::tdigest::TDigest;
use crate::metrics::union_state::UnionState;
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Token, Tokens, XParser, java_parse_double};

/// `PercentilesAggregationBuilder.DEFAULT_PERCENTS`.
pub const DEFAULT_PERCENTS: [f64; 7] = [1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0];

/// `PercentilesConfig`.
#[derive(Clone, Debug, PartialEq)]
pub enum Method {
    TDigest { compression: f64, high_accuracy: bool },
    Hdr { digits: u32 },
}

impl Default for Method {
    fn default() -> Self {
        Method::TDigest { compression: 100.0, high_accuracy: false }
    }
}

/// A parsed `percentiles` / `percentile_ranks`.
#[derive(Clone, Debug)]
pub struct PercentilesSpec {
    pub vs: VsSpec,
    pub ranks: bool,
    /// Sorted percents (or rank values).
    pub keys: Vec<f64>,
    pub keyed: bool,
    pub method: Method,
}

/// `PercentilesMethod.TDIGEST_PARSER`.
fn parse_tdigest(p: &mut XParser<'_>) -> Result<Method> {
    let fields = [Field::new("compression", Tokens::FLOAT), Field::new("execution_hint", Tokens::STRING)];
    let (mut compression, mut high_accuracy) = (100.0, false);
    p.object("tdigest", &fields, |p, i| {
        if i == 0 {
            let c = p.double_value()?;
            if c < 0.0 {
                return Err(AggError::illegal_argument(format!("[compression] must be greater than or equal to 0. Found [{}]", java_double(c))));
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
    Ok(Method::TDigest { compression, high_accuracy })
}

/// `PercentilesMethod.HDR_PARSER`.
fn parse_hdr(p: &mut XParser<'_>) -> Result<Method> {
    let fields = [Field::new("number_of_significant_value_digits", Tokens::INT)];
    let mut digits = 3u32;
    p.object("hdr", &fields, |p, _| {
        let d = p.int_value()?;
        if !(0..=5).contains(&d) {
            return Err(AggError::illegal_argument("[numberOfSignificantValueDigits] must be between 0 and 5"));
        }
        digits = d as u32;
        Ok(())
    })?;
    Ok(Method::Hdr { digits })
}

/// Parses `percentiles` (`ranks: false`) or `percentile_ranks` (a `ConstructingObjectParser`:
/// the percents/values and method are checked once the whole object is read).
pub fn parse(p: &mut XParser<'_>, ranks: bool, name: &str) -> Result<PercentilesSpec> {
    let type_name = if ranks { "percentile_ranks" } else { "percentiles" };
    let values_field = if ranks { "values" } else { "percents" };
    let opts = VsOpts::new(true, true, false);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new(values_field, Tokens::FLOAT_ARRAY));
    fields.push(Field::new("keyed", Tokens::BOOLEAN));
    fields.push(Field::with_deprecated("tdigest", &["TDigest", "TDIGEST"], Tokens::OBJECT));
    fields.push(Field::with_deprecated("hdr", &["HDR"], Tokens::OBJECT));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut vs = VsSpec::default();
    let mut values: Option<Vec<f64>> = None;
    let mut keyed = true;
    let (mut tdigest, mut hdr): (Option<Method>, Option<Method>) = (None, None);
    p.object(type_name, &fields, |p, i| {
        let f = names[i];
        if vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "keyed" => keyed = p.boolean_value()?,
            "tdigest" => tdigest = Some(parse_tdigest(p)?),
            "hdr" => hdr = Some(parse_hdr(p)?),
            _ => {
                if p.token() == Token::Null {
                    return Err(p.xcontent_error(format!("[{type_name}] {f} doesn't support values of type: VALUE_NULL")));
                }
                values = Some(p.array_items(|p| p.double_value())?);
            }
        }
        Ok(())
    })?;
    let built = (|| {
        let method = match (tdigest, hdr) {
            (Some(_), Some(_)) => return Err(AggError::illegal_argument("Only one percentiles method should be declared.")),
            (Some(m), None) | (None, Some(m)) => m,
            (None, None) => Method::default(),
        };
        let keys = if ranks {
            let Some(mut v) = values else {
                return Err(AggError::illegal_argument(format!("[values] must not be null: [{name}]")));
            };
            if v.is_empty() {
                return Err(AggError::illegal_argument(format!("[values] must not be an empty array: [{name}]")));
            }
            v.sort_by(f64::total_cmp);
            v
        } else {
            match values {
                None => DEFAULT_PERCENTS.to_vec(),
                Some(mut v) => {
                    if v.is_empty() {
                        return Err(AggError::illegal_argument(format!("[percents] must not be empty: [{name}]")));
                    }
                    v.sort_by(f64::total_cmp);
                    let mut prev = -1.0;
                    for &pc in &v {
                        if !(0.0..=100.0).contains(&pc) {
                            return Err(AggError::illegal_argument(format!("percent must be in [0,100], got [{}]: [{name}]", java_double(pc))));
                        }
                        if pc == prev {
                            return Err(AggError::illegal_argument(format!("percent [{}] has been specified twice: [{name}]", java_double(pc))));
                        }
                        prev = pc;
                    }
                    v
                }
            }
        };
        Ok((method, keys))
    })();
    let (method, keys) = built.map_err(|e| {
        AggError::new(400, "x_content_parse_exception", format!("Failed to build [{type_name}] after last required field arrived")).caused_by(e)
    })?;
    vs.check_required(opts)?;
    Ok(PercentilesSpec { vs, ranks, keys, keyed, method })
}

/// One bucket's state.
#[derive(Clone, Debug)]
pub enum PctState {
    TDigest(TDigest),
    Hdr(DoubleHistogram),
    /// `exponential_histogram` data (with or without a digest): `HistogramUnionState`.
    Union(Box<UnionState>),
}

impl PctState {
    fn new(method: &Method) -> PctState {
        match method {
            Method::TDigest { compression, high_accuracy } => PctState::TDigest(TDigest::new(*compression, *high_accuracy)),
            Method::Hdr { digits } => PctState::Hdr(DoubleHistogram::new(*digits)),
        }
    }

    fn ram_bytes(&self) -> usize {
        match self {
            PctState::TDigest(t) => t.ram_bytes(),
            PctState::Hdr(h) => h.ram_bytes(),
            PctState::Union(u) => u.ram_bytes(),
        }
    }

    /// `size()` / `getTotalCount()`.
    fn size(&self) -> u64 {
        match self {
            PctState::TDigest(t) => t.size(),
            PctState::Hdr(h) => h.total_count().max(0) as u64,
            PctState::Union(u) => u.size(),
        }
    }

    /// The percentile (`ranks: false`) or percentile rank of `key` on a live state
    /// (shard-side ordering: merging digests merge their pending samples first).
    fn value(&mut self, ranks: bool, key: f64) -> f64 {
        match self {
            PctState::TDigest(t) => {
                if ranks {
                    clamp_rank(t.cdf(key).unwrap_or(f64::NAN))
                } else {
                    t.quantile(key / 100.0)
                }
            }
            PctState::Hdr(h) => hdr_value(h, ranks, key),
            PctState::Union(u) => {
                if ranks {
                    clamp_rank(u.cdf_live(key).unwrap_or(f64::NAN))
                } else {
                    u.quantile_live(key / 100.0)
                }
            }
        }
    }

    /// [`PctState::value`] on a compressed result state, without changing it.
    fn value_compressed(&self, ranks: bool, key: f64) -> f64 {
        match self {
            PctState::TDigest(t) => {
                if ranks {
                    clamp_rank(t.cdf_compressed(key).unwrap_or(f64::NAN))
                } else {
                    t.quantile_compressed(key / 100.0)
                }
            }
            PctState::Hdr(h) => hdr_value(h, ranks, key),
            PctState::Union(u) => {
                if ranks {
                    clamp_rank(u.cdf(key).unwrap_or(f64::NAN))
                } else {
                    u.quantile(key / 100.0)
                }
            }
        }
    }
}

/// `InternalHDRPercentiles.percentile` / `InternalHDRPercentileRanks.percentileRank`.
fn hdr_value(h: &DoubleHistogram, ranks: bool, key: f64) -> f64 {
    if h.total_count() == 0 {
        return f64::NAN;
    }
    if ranks { h.percentile_at_or_below(key).clamp(0.0, 100.0) } else { h.value_at_percentile(key) }
}

/// `percentileRank`: the CDF clamped to [0, 1], times 100.
fn clamp_rank(r: f64) -> f64 {
    // NaN stays NaN (`clamp` keeps it too).
    r.clamp(0.0, 1.0) * 100.0
}

/// Which result family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PctKind {
    TDigest,
    Hdr,
}

/// `InternalTDigestPercentiles`, `InternalTDigestPercentileRanks`, `InternalHDRPercentiles`,
/// `InternalHDRPercentileRanks`.
#[derive(Clone, Debug)]
pub struct Percentiles {
    pub kind: PctKind,
    pub ranks: bool,
    pub keys: Vec<f64>,
    pub keyed: bool,
    pub format: DocValueFormat,
    /// `None` for empty percentiles (ranks keep an empty digest).
    pub state: Option<PctState>,
    /// The digest's configuration, for the empty state.
    pub method: Method,
}

impl Percentiles {
    fn size(&self) -> u64 {
        self.state.as_ref().map_or(0, |s| s.size())
    }

    /// `value(double key)` on the (compressed) state.
    pub fn value(&self, key: f64) -> f64 {
        match &self.state {
            None => f64::NAN,
            Some(s) => s.value_compressed(self.ranks, key),
        }
    }

    /// `value(String name)`.
    pub fn value_by_name(&self, name: &str, agg_name: &str) -> Result<f64> {
        if self.keys.len() == 1 && agg_name == name {
            return Ok(self.value(self.keys[0]));
        }
        Ok(self.value(java_parse_double(name)?))
    }

    fn values(&self) -> Vec<f64> {
        match &self.state {
            None => vec![f64::NAN; self.keys.len()],
            Some(s) => self.keys.iter().map(|&k| s.value_compressed(self.ranks, k)).collect(),
        }
    }
}

impl Internal for Percentiles {
    fn type_name(&self) -> &'static str {
        match (self.kind, self.ranks) {
            (PctKind::TDigest, false) => "tdigest_percentiles",
            (PctKind::TDigest, true) => "tdigest_percentile_ranks",
            (PctKind::Hdr, false) => "hdr_percentiles",
            (PctKind::Hdr, true) => "hdr_percentile_ranks",
        }
    }

    fn class_name(&self) -> &'static str {
        match (self.kind, self.ranks) {
            (PctKind::TDigest, false) => "InternalTDigestPercentiles",
            (PctKind::TDigest, true) => "InternalTDigestPercentileRanks",
            (PctKind::Hdr, false) => "InternalHDRPercentiles",
            (PctKind::Hdr, true) => "InternalHDRPercentileRanks",
        }
    }

    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        if list.iter().any(|p| matches!(p.state, Some(PctState::Union(_)))) {
            return Percentiles::reduce_union(list);
        }
        let first = list[0].clone_shell();
        let mut merged: Option<PctState> = None;
        for p in &mut list {
            let Some(state) = p.state.as_mut() else { continue };
            match state {
                PctState::TDigest(t) => {
                    let replace = match &merged {
                        Some(PctState::TDigest(m)) => t.compression() > m.compression(),
                        _ => true,
                    };
                    if replace {
                        let previous = merged.take();
                        let mut fresh = t.empty_like();
                        if let Some(PctState::TDigest(mut prev)) = previous {
                            fresh.merge(&mut prev)?;
                        }
                        merged = Some(PctState::TDigest(fresh));
                    }
                    if let Some(PctState::TDigest(m)) = &mut merged {
                        m.merge(t)?;
                    }
                }
                PctState::Union(_) => unreachable!("reduced by reduce_union"),
                PctState::Hdr(h) => {
                    let m = match merged.take() {
                        Some(PctState::Hdr(m)) => m,
                        _ => DoubleHistogram::empty_like(h),
                    };
                    // merge(): the histogram with more digits absorbs the other.
                    let out = if h.digits() > m.digits() {
                        let mut more = h.clone();
                        more.add(&m)?;
                        more
                    } else {
                        let mut more = m;
                        more.add(h)?;
                        more
                    };
                    merged = Some(PctState::Hdr(out));
                }
            }
        }
        let mut out = first;
        if let Some(PctState::TDigest(t)) = &mut merged {
            t.compress();
        }
        out.state = match (merged, out.kind) {
            (Some(s), _) => Some(s),
            // Reduced with no state: Elasticsearch's EMPTY state (size 0).
            (None, PctKind::TDigest) => Some(PctState::TDigest(TDigest::new(100.0, false))),
            (None, PctKind::Hdr) => None,
        };
        Ok(out)
    }

    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let size = self.size();
        let values = self.values();
        if self.keyed {
            let mut o = Json::obj();
            for (&k, &v) in self.keys.iter().zip(&values) {
                let key = java_double(k);
                o.push(key.clone(), if size == 0 { Json::Null } else { Json::Float(v) });
                if !self.format.is_raw() && size > 0 {
                    o.push(format!("{key}_as_string"), self.format.double_json(v));
                }
            }
            out.push("values", o);
        } else {
            let mut arr = Vec::with_capacity(self.keys.len());
            for (&k, &v) in self.keys.iter().zip(&values) {
                let mut o = Json::obj();
                o.push("key", Json::Float(k));
                o.push("value", if size == 0 { Json::Null } else { Json::Float(v) });
                if !self.format.is_raw() && size > 0 {
                    o.push("value_as_string", self.format.double_json(v));
                }
                arr.push(o);
            }
            out.push("values", Json::Arr(arr));
        }
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_value_property(name, path, |k| self.value_by_name(k, name))
    }

    fn ram_bytes(&self) -> usize {
        64 + self.keys.len() * 8 + self.state.as_ref().map_or(0, |s| s.ram_bytes())
    }
}

impl Percentiles {
    /// The reduce once exponential histograms take part (`AbstractInternalTDigestPercentiles`'s
    /// leader reducer over `HistogramUnionState`s): re-seeded whenever a state has a larger
    /// compression, every state added.
    fn reduce_union(mut list: Vec<Self>) -> Result<Self> {
        let mut out = list[0].clone_shell();
        let mut merged: Option<UnionState> = None;
        for p in &mut list {
            let mut state = match p.state.take() {
                Some(PctState::Union(u)) => *u,
                Some(PctState::TDigest(t)) => UnionState::wrap(t),
                Some(PctState::Hdr(_)) | None => continue,
            };
            if merged.as_ref().is_none_or(|m| state.compression() > m.compression()) {
                let mut fresh = UnionState::like(&state);
                if let Some(mut prev) = merged.take() {
                    fresh.add(&mut prev)?;
                }
                merged = Some(fresh);
            }
            if let Some(m) = merged.as_mut() {
                m.add(&mut state)?;
            }
        }
        let mut m = merged.unwrap_or_else(|| match &out.method {
            Method::TDigest { compression, high_accuracy } => UnionState::new(*compression, *high_accuracy),
            Method::Hdr { .. } => UnionState::new(100.0, false),
        });
        m.compress();
        out.state = Some(PctState::Union(Box::new(m)));
        Ok(out)
    }

    fn clone_shell(&self) -> Percentiles {
        Percentiles {
            kind: self.kind,
            ranks: self.ranks,
            keys: self.keys.clone(),
            keyed: self.keyed,
            format: self.format.clone(),
            state: None,
            method: self.method.clone(),
        }
    }
}

/// Binds `percentiles` / `percentile_ranks`.
pub fn create(def: &AggDef, spec: &PercentilesSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Numeric, false)?;
    let type_name = if spec.ranks { "percentile_ranks" } else { "percentiles" };
    let exp = vs.vs_type == VsType::ExponentialHistogram && !vs.unmapped;
    if !vs.unmapped && !vs.vs_type.is_numeric() && vs.vs_type != VsType::Histogram && !exp {
        return Err(vs.unsupported(type_name));
    }
    if exp && matches!(spec.method, Method::Hdr { .. }) {
        return Err(AggError::illegal_argument("Percentiles algorithm hdr must not be used with exponential_histogram field"));
    }
    Ok(Box::new(PercentilesAgg {
        spec: spec.clone(),
        format: vs.format.clone(),
        histograms: None,
        exp: None,
        reader: None,
        vs,
        states: RefCell::new(Vec::new()),
        bytes: 0,
        tracker: ctx.tracker(def),
    }))
}

struct PercentilesAgg {
    spec: PercentilesSpec,
    format: DocValueFormat,
    vs: BoundVs,
    /// A `histogram` field: centroids weighted by count (`HistoBacked*PercentilesAggregator`).
    histograms: Option<crate::histogram_field::HistogramReader>,
    /// An `exponential_histogram` field: each doc's histogram merged into a union state.
    exp: Option<ExpHistogramReader>,
    reader: Option<NumReader>,
    states: RefCell<Vec<Option<PctState>>>,
    bytes: usize,
    tracker: MemTracker,
}

impl PercentilesAgg {
    fn kind(&self) -> PctKind {
        match self.spec.method {
            Method::TDigest { .. } => PctKind::TDigest,
            Method::Hdr { .. } => PctKind::Hdr,
        }
    }

    fn result(&self, state: Option<PctState>) -> AggValue {
        let state = match state {
            Some(PctState::TDigest(mut t)) => {
                t.compress();
                Some(PctState::TDigest(t))
            }
            Some(PctState::Union(mut u)) => {
                u.compress();
                Some(PctState::Union(u))
            }
            Some(s) => Some(s),
            None if self.spec.ranks => match &self.spec.method {
                // InternalTDigestPercentileRanks.empty keeps an empty digest.
                Method::TDigest { compression, high_accuracy } => Some(PctState::TDigest(TDigest::new(*compression, *high_accuracy))),
                Method::Hdr { .. } => None,
            },
            None => None,
        };
        AggValue::Percentiles(Box::new(Percentiles {
            kind: self.kind(),
            ranks: self.spec.ranks,
            keys: self.spec.keys.clone(),
            keyed: self.spec.keyed,
            format: self.format.clone(),
            state,
            method: self.spec.method.clone(),
        }))
    }
}

impl PercentilesAgg {
    /// Every doc gets a state (an empty one without a value), then each centroid is added with
    /// its count as weight.
    fn collect_histograms(&mut self, b: Batch<'_>, hist: &mut crate::histogram_field::HistogramReader) -> Result<()> {
        let states = self.states.get_mut();
        for i in 0..b.len() {
            let ord = b.ords[i] as usize;
            if states.len() <= ord {
                states.resize_with(ord + 1, || None);
            }
            let state = states[ord].get_or_insert_with(|| PctState::new(&self.spec.method));
            let before = state.ram_bytes();
            for &(v, c) in hist.centroids(b.docs[i])? {
                match state {
                    PctState::TDigest(t) => t.add_weighted(v, c)?,
                    PctState::Hdr(h) => h.record_count(v, c as i64)?,
                    // Only exponential histogram fields build union states.
                    PctState::Union(_) => {}
                }
            }
            let after = state.ram_bytes();
            if after != before {
                self.bytes = self.bytes + after - before.min(after);
                self.tracker.ensure(self.bytes + states.len() * 8)?;
            }
        }
        Ok(())
    }
}

impl PercentilesAgg {
    /// `mergingExponentialHistogramCollector`: every collected doc gets a state, its histogram
    /// (if any) merged in.
    fn collect_exp(&mut self, b: Batch<'_>, r: &mut ExpHistogramReader) -> Result<()> {
        let (compression, high_accuracy) = match self.spec.method {
            Method::TDigest { compression, high_accuracy } => (compression, high_accuracy),
            Method::Hdr { .. } => (100.0, false),
        };
        let states = self.states.get_mut();
        for i in 0..b.len() {
            let ord = b.ords[i] as usize;
            if states.len() <= ord {
                states.resize_with(ord + 1, || None);
            }
            let state = states[ord].get_or_insert_with(|| PctState::Union(Box::new(UnionState::new(compression, high_accuracy))));
            let Some(h) = r.histogram(b.docs[i])? else { continue };
            let before = state.ram_bytes();
            if let PctState::Union(u) = state {
                u.add_exp(h);
            }
            let after = state.ram_bytes();
            if after > before {
                self.bytes += after - before;
                self.tracker.ensure(self.bytes + states.len() * 8)?;
            }
        }
        Ok(())
    }
}

impl Aggregator for PercentilesAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if self.vs.vs_type == VsType::ExponentialHistogram && !self.vs.unmapped {
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
            let r = self.collect_histograms(b, &mut hist);
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
            let state = states[ord].get_or_insert_with(|| PctState::new(&self.spec.method));
            let before = state.ram_bytes();
            match state {
                PctState::TDigest(t) => {
                    for &v in vals {
                        t.add(v)?;
                    }
                }
                PctState::Hdr(h) => {
                    for &v in vals {
                        if v < 0.0 {
                            return Err(AggError::illegal_argument("Negative values are not supported by HDR aggregation"));
                        }
                        h.record(v)?;
                    }
                }
                // Only exponential histogram fields build union states.
                PctState::Union(_) => {}
            }
            let after = state.ram_bytes();
            if after != before {
                self.bytes = self.bytes + after - before.min(after);
                self.tracker.ensure(self.bytes + states.len() * 8)?;
            }
        }
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut out = Vec::with_capacity(owning.len());
        for &o in owning {
            let state = self.states.get_mut().get_mut(o as usize).and_then(Option::take);
            out.push(self.result(state));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        self.result(None)
    }

    fn needs_scores(&self) -> bool {
        self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        multi_value_validate_sort(name, key, rest, |k| java_parse_double(k).is_ok_and(|v| self.spec.keys.contains(&v)))
    }

    fn sort_value(&self, key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        let Some(k) = key.and_then(|k| java_parse_double(k).ok()) else { return f64::NAN };
        let mut states = self.states.borrow_mut();
        match states.get_mut(ord as usize).and_then(Option::as_mut) {
            // metric(name, ord) reads the live state (merging pending samples).
            Some(s) => s.value(self.spec.ranks, k),
            None => f64::NAN,
        }
    }
}

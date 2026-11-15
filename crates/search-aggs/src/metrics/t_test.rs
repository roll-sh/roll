//! `t_test` (x-pack analytics): Student's t-test between two populations, reported as a
//! p-value.
//!
//! Shard side, each owning bucket keeps a count and Kahan-compensated sums of values and of
//! squares: one set of the per-doc differences `a - b` for `paired` tests (one value of each
//! per doc), or one set per population for `homoscedastic` / `heteroscedastic` (Welch) tests,
//! where each population may have its own `filter`. Partial results merge the sums
//! (compensated again, in shard order); the p-value is computed only when rendering or
//! sorting, through Commons Math's t-distribution ([`crate::commons_math`]) so it matches
//! Elasticsearch bit for bit — including its quirks (the paired variance divides by `n`, the
//! unpaired by `n - 1`).

use search_index::{QueryRef, ScoreMode, Scorer, SegmentContext, Weight};

use crate::collect::{Aggregator, Batch, BindCtx, grow, single_value_validate_sort};
use crate::commons_math;
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, CompensatedSum, Internal, Prop, ReduceCtx, RenderCtx, single_value_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::simple::{MvsField, add_value_as_string, parse_value_type_hint};
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, NumReader, VsType, bind, resolve_format};
use crate::xparser::{Field, Tokens, XParser};

/// `TTestType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TTestType {
    Paired,
    Homoscedastic,
    Heteroscedastic,
}

/// One population of a `t_test` (`MultiValuesSourceFieldConfig` with a `filter`).
#[derive(Clone, Debug)]
pub struct Population {
    pub vs: MvsField,
    pub filter: Option<Json>,
}

/// A parsed `t_test`.
#[derive(Clone, Debug)]
pub struct TTestSpec {
    pub a: Option<Population>,
    pub b: Option<Population>,
    pub kind: TTestType,
    pub tails: u8,
    pub format: Option<String>,
    pub value_type: Option<crate::values::ValueType>,
}

/// `MultiValuesSourceFieldConfig.parserBuilder(scriptable, timezoneAware = false, filtered)`.
fn parse_population(p: &mut XParser<'_>, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<Population> {
    let fields = [
        Field::new("field", Tokens::STRING),
        Field::new("missing", Tokens::VALUE),
        Field::new("script", Tokens::OBJECT_OR_STRING),
        Field::new("filter", Tokens::OBJECT),
    ];
    let mut vs = MvsField::default();
    let mut filter = None;
    p.object("field_config", &fields, |p, i| {
        match i {
            0 => vs.field = Some(p.text()?),
            1 => vs.missing = p.object_text()?,
            2 => vs.script = Some(crate::script::Script::parse(p)?),
            _ => {
                check_query(p)?;
                filter = Some(p.take_value());
            }
        }
        Ok(())
    })?;
    vs.check()?;
    Ok(Population { vs, filter })
}

/// `TTestAggregationBuilder.PARSER`.
pub fn parse(p: &mut XParser<'_>, name: &str, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<TTestSpec> {
    let fields = [
        Field::with_deprecated("value_type", &["valueType"], Tokens::STRING),
        Field::new("format", Tokens::STRING),
        Field::new("a", Tokens::OBJECT),
        Field::new("b", Tokens::OBJECT),
        Field::new("type", Tokens::STRING),
        Field::new("tails", Tokens::INT),
    ];
    let mut spec = TTestSpec { a: None, b: None, kind: TTestType::Heteroscedastic, tails: 2, format: None, value_type: None };
    p.object("t_test", &fields, |p, i| {
        match i {
            0 => spec.value_type = Some(parse_value_type_hint(p, "t_test")?),
            1 => spec.format = Some(p.text()?),
            2 => spec.a = Some(parse_population(p, check_query)?),
            3 => spec.b = Some(parse_population(p, check_query)?),
            4 => {
                let t = p.text()?;
                spec.kind = match t.to_uppercase().as_str() {
                    "PAIRED" => TTestType::Paired,
                    "HOMOSCEDASTIC" => TTestType::Homoscedastic,
                    "HETEROSCEDASTIC" => TTestType::Heteroscedastic,
                    other => {
                        return Err(AggError::illegal_argument(format!("No enum constant org.elasticsearch.xpack.analytics.ttest.TTestType.{other}")));
                    }
                };
            }
            _ => {
                let tails = p.int_value()?;
                if !(1..=2).contains(&tails) {
                    return Err(AggError::illegal_argument(format!("[tails] must be 1 or 2. Found [{tails}] in [{name}]")));
                }
                spec.tails = tails as u8;
            }
        }
        Ok(())
    })?;
    Ok(spec)
}

/// `TTestAggregationBuilder.innerBuild` + `TTestAggregatorFactory`.
pub fn create(def: &AggDef, spec: &TTestSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let filter_a = spec.a.as_ref().and_then(|p| p.filter.as_ref());
    let filter_b = spec.b.as_ref().and_then(|p| p.filter.as_ref());
    let (Some(a), Some(b)) = (&spec.a, &spec.b) else {
        // `configs.get("a").fieldContext()` on a missing population.
        return Err(AggError::new(
            500,
            "null_pointer_exception",
            "Cannot invoke \"org.elasticsearch.search.aggregations.support.ValuesSourceConfig.fieldContext()\" because the return value of \"java.util.Map.get(Object)\" is null",
        ));
    };
    let va = bind(&a.vs.to_vs(), ctx.lookup, VsType::Numeric, false)?;
    let vb = bind(&b.vs.to_vs(), ctx.lookup, VsType::Numeric, false)?;
    if filter_a.is_none()
        && filter_b.is_none()
        && let (Some(fa), Some(fb)) = (&va.field, &vb.field)
        && fa.name == fb.name
    {
        return Err(AggError::illegal_argument(format!("The same field [{}] is used for both population but no filters are specified.", fa.name)));
    }
    let query = |f: Option<&Json>| -> Result<Option<QueryRef>> { f.map(|q| ctx.lookup.query(q)).transpose() };
    let (qa, qb) = (query(filter_a)?, query(filter_b)?);
    let vs_type = spec.value_type.map(|v| v.source()).unwrap_or(VsType::Numeric);
    let format = resolve_format(spec.format.as_deref(), vs_type, None, None, ctx.lookup)?;
    // `NumericMultiValuesSource`: both sources numeric (dates and booleans are).
    for v in [&va, &vb] {
        if let Some(f) = &v.field
            && !v.vs_type.is_numeric()
        {
            return Err(AggError::illegal_argument(format!("Expected numeric type on field [{}], but got [{}]", f.name, f.type_name)));
        }
    }
    // `areValuesSourcesEmpty()` is never true (unmapped fields get empty sources), so the
    // paired check applies to unmapped fields too.
    let empty = !va.has_values() && !vb.has_values();
    if spec.kind == TTestType::Paired && (qa.is_some() || qb.is_some()) {
        return Err(AggError::illegal_argument("Paired t-test doesn't support filters"));
    }
    let weight = |q: Option<QueryRef>| -> Result<Option<Box<dyn Weight>>> { Ok(q.map(|q| ctx.searcher.create_weight(&q, ScoreMode::CompleteNoScores)).transpose()?) };
    let filters = if spec.kind == TTestType::Paired { [None, None] } else { [weight(qa)?, weight(qb)?] };
    Ok(Box::new(TTestAgg {
        kind: spec.kind,
        tails: spec.tails,
        format,
        empty,
        sources: [va, vb],
        readers: None,
        filters,
        scorers: [None, None],
        last: [(u32::MAX, false); 2],
        stats: [Stats::default(), Stats::default()],
        tracker: ctx.tracker(def),
    }))
}

/// Per-owning-bucket compensated sums of one population (`TTestStatsBuilder`).
#[derive(Default)]
struct Stats {
    counts: Vec<u64>,
    sums: Vec<f64>,
    sum_comps: Vec<f64>,
    sqs: Vec<f64>,
    sq_comps: Vec<f64>,
}

impl Stats {
    #[inline]
    fn add(&mut self, ord: u32, v: f64) {
        let o = ord as usize;
        self.counts[o] += 1;
        let mut s = CompensatedSum::new(self.sums[o], self.sum_comps[o]);
        s.add(v);
        self.sums[o] = s.value;
        self.sum_comps[o] = s.delta;
        let mut q = CompensatedSum::new(self.sqs[o], self.sq_comps[o]);
        q.add(v * v);
        self.sqs[o] = q.value;
        self.sq_comps[o] = q.delta;
    }

    fn grow(&mut self, ord: u32) {
        grow(&mut self.counts, ord, 0);
        grow(&mut self.sums, ord, 0.0);
        grow(&mut self.sum_comps, ord, 0.0);
        grow(&mut self.sqs, ord, 0.0);
        grow(&mut self.sq_comps, ord, 0.0);
    }

    fn get(&self, ord: u32) -> TTestStats {
        let o = ord as usize;
        if o < self.counts.len() { TTestStats { count: self.counts[o], sum: self.sums[o], sum_of_sqrs: self.sqs[o] } } else { TTestStats::default() }
    }
}

struct TTestAgg {
    kind: TTestType,
    tails: u8,
    format: DocValueFormat,
    /// Neither population has values (`createUnmapped`).
    empty: bool,
    sources: [BoundVs; 2],
    readers: Option<[NumReader; 2]>,
    filters: [Option<Box<dyn Weight>>; 2],
    scorers: [Option<Box<dyn Scorer>>; 2],
    /// Per population: the last doc checked against its filter and the answer.
    last: [(u32, bool); 2],
    /// Paired: the differences in `stats[0]`. Unpaired: populations a and b.
    stats: [Stats; 2],
    tracker: MemTracker,
}

impl TTestAgg {
    /// Whether `doc` passes population `i`'s filter (docs arrive in order within a segment).
    fn accepts(&mut self, i: usize, doc: u32) -> Result<bool> {
        if self.filters[i].is_none() {
            return Ok(true);
        }
        if self.last[i].0 == doc {
            return Ok(self.last[i].1);
        }
        let Some(sc) = self.scorers[i].as_mut() else { return Ok(false) };
        let mut d = sc.doc();
        if d == u32::MAX || d < doc {
            d = sc.advance(doc);
        }
        let hit = d == doc;
        self.last[i] = (doc, hit);
        Ok(hit)
    }

    fn state(&self, ord: u32) -> TTestState {
        match self.kind {
            TTestType::Paired => TTestState::Paired { stats: self.stats[0].get(ord), tails: self.tails },
            _ => TTestState::Unpaired { a: self.stats[0].get(ord), b: self.stats[1].get(ord), homoscedastic: self.kind == TTestType::Homoscedastic, tails: self.tails },
        }
    }

    fn empty_state(&self) -> TTestState {
        match self.kind {
            TTestType::Paired => TTestState::Paired { stats: TTestStats::default(), tails: self.tails },
            _ => TTestState::Unpaired { a: TTestStats::default(), b: TTestStats::default(), homoscedastic: self.kind == TTestType::Homoscedastic, tails: self.tails },
        }
    }
}

impl Aggregator for TTestAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.readers = if self.empty { None } else { Some([self.sources[0].numeric_reader(seg)?, self.sources[1].numeric_reader(seg)?]) };
        for i in 0..2 {
            self.scorers[i] = match &self.filters[i] {
                Some(w) => w.scorer(seg)?,
                None => None,
            };
            self.last[i] = (u32::MAX, false);
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some(mut readers) = self.readers.take() else { return Ok(()) };
        let res = (|| {
            for i in 0..b.len() {
                let (doc, ord, score) = (b.docs[i], b.ords[i], b.score(i));
                if self.kind == TTestType::Paired {
                    let [ra, rb] = &mut readers;
                    let va = ra.doubles(doc, score)?;
                    if va.is_empty() {
                        continue;
                    }
                    let vb = rb.doubles(doc, score)?;
                    if vb.is_empty() {
                        continue;
                    }
                    if va.len() > 1 || vb.len() > 1 {
                        return Err(AggError::illegal_argument(
                            "Encountered more than one value for a single document. Use a script to combine multiple values per doc into a single value.",
                        ));
                    }
                    let diff = va[0] - vb[0];
                    self.stats[0].grow(ord);
                    self.stats[0].add(ord, diff);
                } else {
                    for (p, reader) in readers.iter_mut().enumerate() {
                        if !self.accepts(p, doc)? {
                            continue;
                        }
                        let vals = reader.doubles(doc, score)?;
                        if vals.is_empty() {
                            continue;
                        }
                        self.stats[p].grow(ord);
                        for &v in vals {
                            self.stats[p].add(ord, v);
                        }
                    }
                }
            }
            let cells = self.stats[0].counts.len() + self.stats[1].counts.len();
            self.tracker.ensure(cells * 40)
        })();
        self.readers = Some(readers);
        res
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning.iter().map(|&o| AggValue::TTest(Box::new(TTest { state: if self.empty { self.empty_state() } else { self.state(o) }, format: self.format.clone() }))).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::TTest(Box::new(TTest { state: self.empty_state(), format: self.format.clone() }))
    }

    fn needs_scores(&self) -> bool {
        self.sources.iter().any(|v| v.script.as_ref().is_some_and(|s| s.needs_score()))
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_value_validate_sort(name, key, rest)
    }

    fn sort_value(&self, _key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        self.state(ord).value()
    }
}

/// `TTestStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TTestStats {
    pub count: u64,
    pub sum: f64,
    pub sum_of_sqrs: f64,
}

impl TTestStats {
    fn variance(&self) -> f64 {
        let n = self.count as f64;
        let v = (self.sum_of_sqrs - ((self.sum * self.sum) / n)) / (n - 1.0);
        if v < 0.0 { 0.0 } else { v }
    }

    fn average(&self) -> f64 {
        self.sum / self.count as f64
    }
}

/// `TTestStats.Reducer`: counts add up, sums are compensated in list order.
fn reduce_stats(list: impl Iterator<Item = TTestStats>) -> TTestStats {
    let (mut count, mut sum, mut sqs) = (0u64, CompensatedSum::default(), CompensatedSum::default());
    for s in list {
        count += s.count;
        sum.add(s.sum);
        sqs.add(s.sum_of_sqrs);
    }
    TTestStats { count, sum: sum.value, sum_of_sqrs: sqs.value }
}

/// `PairedTTestState` / `UnpairedTTestState`.
#[derive(Clone, Debug, PartialEq)]
pub enum TTestState {
    Paired { stats: TTestStats, tails: u8 },
    Unpaired { a: TTestStats, b: TTestStats, homoscedastic: bool, tails: u8 },
}

impl TTestState {
    /// `getValue()`: the p-value (NaN without enough data).
    pub fn value(&self) -> f64 {
        match *self {
            TTestState::Paired { stats, tails } => {
                if stats.count < 2 {
                    return f64::NAN;
                }
                let n = stats.count - 1;
                let count = stats.count as f64;
                let mean_diff = stats.sum / count;
                let variance = (stats.sum_of_sqrs - ((stats.sum * stats.sum) / count)) / count;
                if variance <= 0.0 {
                    return if mean_diff == 0.0 { f64::NAN } else { 0.0 };
                }
                let std_dev = variance.sqrt();
                let std_err = std_dev / (n as f64).sqrt();
                let t = (mean_diff / std_err).abs();
                commons_math::t_cdf(n as f64, -t) * tails as f64
            }
            TTestState::Unpaired { a, b, homoscedastic, tails } => {
                if a.count < 2 || b.count < 2 {
                    return f64::NAN;
                }
                if homoscedastic {
                    let n = a.count + b.count - 2;
                    let variance = ((a.count - 1) as f64 * a.variance() + (b.count - 1) as f64 * b.variance()) / n as f64;
                    let nn = 1.0 / a.count as f64 + 1.0 / b.count as f64;
                    unpaired_p(&a, &b, tails, variance * nn, n as f64)
                } else {
                    let s2an = a.variance() / a.count as f64;
                    let s2bn = b.variance() / b.count as f64;
                    let variance = s2an + s2bn;
                    let dof = variance * variance / (s2an * s2an / (a.count - 1) as f64 + s2bn * s2bn / (b.count - 1) as f64);
                    unpaired_p(&a, &b, tails, variance, dof)
                }
            }
        }
    }
}

/// `UnpairedTTestState.p`.
fn unpaired_p(a: &TTestStats, b: &TTestStats, tails: u8, sd2: f64, dof: f64) -> f64 {
    if dof < 0.0 {
        return f64::NAN;
    }
    let sd = sd2.sqrt();
    let mean_diff = a.average() - b.average();
    let t = (mean_diff / sd).abs();
    commons_math::t_cdf(dof, -t) * tails as f64
}

/// `InternalTTest`.
#[derive(Clone, Debug)]
pub struct TTest {
    pub state: TTestState,
    pub format: DocValueFormat,
}

impl TTest {
    pub fn value(&self) -> f64 {
        self.state.value()
    }
}

impl Internal for TTest {
    fn type_name(&self) -> &'static str {
        "t_test"
    }

    fn class_name(&self) -> &'static str {
        "InternalTTest"
    }

    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let first = list[0].clone();
        let state = match &first.state {
            TTestState::Paired { tails, .. } => {
                let mut stats = Vec::with_capacity(list.len());
                for t in &list {
                    match &t.state {
                        TTestState::Paired { stats: s, tails: other } => {
                            if other != tails {
                                return Err(incompatible("tails value", other, tails));
                            }
                            stats.push(*s);
                        }
                        _ => return Err(crate::internal::reduce_type_mismatch("t_test")),
                    }
                }
                TTestState::Paired { stats: reduce_stats(stats.into_iter()), tails: *tails }
            }
            TTestState::Unpaired { homoscedastic, tails, .. } => {
                let (mut sa, mut sb) = (Vec::with_capacity(list.len()), Vec::with_capacity(list.len()));
                for t in &list {
                    match &t.state {
                        TTestState::Unpaired { a, b, homoscedastic: h, tails: other } => {
                            if h != homoscedastic {
                                return Err(incompatible("homoscedastic mode", h, homoscedastic));
                            }
                            if other != tails {
                                return Err(incompatible("tails value", other, tails));
                            }
                            sa.push(*a);
                            sb.push(*b);
                        }
                        _ => return Err(crate::internal::reduce_type_mismatch("t_test")),
                    }
                }
                TTestState::Unpaired { a: reduce_stats(sa.into_iter()), b: reduce_stats(sb.into_iter()), homoscedastic: *homoscedastic, tails: *tails }
            }
        };
        Ok(TTest { state, format: first.format })
    }

    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let v = self.value();
        if v.is_nan() {
            out.push("value", Json::Null);
        } else {
            out.push("value", Json::Float(v));
            add_value_as_string(out, "value_as_string", &self.format, v);
        }
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value(), path)
    }
}

fn incompatible(what: &str, got: &dyn std::fmt::Display, want: &dyn std::fmt::Display) -> AggError {
    AggError::new(500, "illegal_state_exception", format!("Incompatible {what} in the reduce. Expected {got} reduced with {want}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(vals: &[f64]) -> TTestStats {
        let mut s = Stats::default();
        for &v in vals {
            s.grow(0);
            s.add(0, v);
        }
        s.get(0)
    }

    /// p-values from Elasticsearch 9.5.5 (`aggs-tt`: a = [1, 3, 5, 2], b = [2.5, 1.5, 4]).
    #[test]
    fn p_values_match_elasticsearch() {
        let a = stats(&[1.0, 3.0, 5.0, 2.0]);
        let b = stats(&[2.5, 1.5, 4.0]);
        let welch = TTestState::Unpaired { a, b, homoscedastic: false, tails: 2 };
        assert_eq!(welch.value(), 0.9436349629797879);
        let paired = TTestState::Paired { stats: stats(&[1.0 - 2.5, 3.0 - 1.5, 5.0 - 4.0]), tails: 2 };
        assert_eq!(paired.value(), 0.7538170180413346);
        // With `missing`: a = [1, 3, 5, 2], b = [2.5, 1.5, 4, 0]; one tail.
        let homo = TTestState::Unpaired { a, b: stats(&[2.5, 1.5, 4.0, 0.0]), homoscedastic: true, tails: 1 };
        assert_eq!(homo.value(), 0.27732321161096873);
    }

    #[test]
    fn degenerate_inputs() {
        let one = stats(&[1.0]);
        let two = stats(&[1.0, 2.0]);
        assert!(TTestState::Unpaired { a: one, b: two, homoscedastic: false, tails: 2 }.value().is_nan());
        assert!(TTestState::Paired { stats: stats(&[0.0, 0.0]), tails: 2 }.value().is_nan());
        assert_eq!(TTestState::Paired { stats: stats(&[1.0, 1.0]), tails: 2 }.value(), 0.0);
        // Identical populations: t = 0, p = 1 (two tails).
        assert_eq!(TTestState::Unpaired { a: two, b: two, homoscedastic: true, tails: 2 }.value(), 1.0);
    }
}

//! OpenSearch's hybrid search (the neural-search plugin every distribution bundles): the
//! `hybrid` query and the search pipelines' phase-results processors that fuse its sub-query
//! results, `normalization-processor` and `score-ranker-processor`.
//!
//! A `hybrid` query holds up to five sub-queries (a `filter` is pushed into each of them, the
//! way `QueryBuilder.filter` does it: a `bool` gains a filter clause, a `knn` query its own
//! filter, anything else is wrapped in `bool {must, filter}`). On a shard each sub-query
//! collects its own top documents (`pagination_depth` of them, else `size`), and the shard
//! matches every document any sub-query matches (totals and aggregations see that union).
//!
//! **With a phase-results processor** the per-shard lists are fused between the query and the
//! fetch phase ([`fuse`], `NormalizationProcessorWorkflow`): each sub-query's scores are
//! normalized over all shards (`min_max` with optional bounds, `l2`, `z_score`, or rank-based
//! `rrf`), then combined per document (`arithmetic_mean`, `geometric_mean`, `harmonic_mean`
//! with optional weights, or `rrf`'s sum), a document missing from a sub-query counting as
//! 0. Each shard orders its documents by combined score (equal scores keep the iteration
//! order of the plugin's `HashMap`s of doc ids), the shards' lists merge like Lucene's
//! `TopDocs.merge` (equal scores: lower shard index first) and `from`/`size` page the result.
//! A field `sort` orders the union instead.
//!
//! **Without one** OpenSearch returns the raw per-shard format the processor would consume:
//! a start marker, then per sub-query a delimiter and its hits, then a stop marker, the
//! markers carrying "magic" negative scores and the first hit's document ([`raw`]).
//!
//! Execution reuses the compound-retriever machinery: each (shard, sub-query) runs as a
//! query phase of its own on a pinned searcher, and one final search over rank docs
//! ([`crate::search::fusion`]) fetches the page's documents and computes the totals and
//! aggregations over the union; the page is then laid out in the fused order with the fused
//! scores. Texts, defaults and float arithmetic follow the plugin (version 3.9.0.0).

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::query::{Common, Query, Token as Tk, XParser, parse_inner};
use crate::search::fusion::{Child, Fused, Node};
use crate::search::rank_docs::RankKind;
use crate::search::retriever::FusedDoc;
use crate::search::{SearchOutcome, SearchRequest, Target};
use search_index::collector::top_docs::TotalRelation;
use crate::search_source::{Sort, SortOrder, TrackTotalHits};
use search_index::collector::top_field::SortValue;
use std::collections::HashMap;
use std::sync::Arc;

/// `HybridQueryBuilder.MAX_NUMBER_OF_SUB_QUERIES`.
pub const MAX_SUB_QUERIES: usize = 5;
/// `HybridSearchResultFormatUtil.MAGIC_NUMBER_START_STOP`.
pub const MAGIC_START_STOP: f32 = -9_549_511_920.488_16;
/// `HybridSearchResultFormatUtil.MAGIC_NUMBER_DELIMITER`.
pub const MAGIC_DELIMITER: f32 = -4_422_440_593.979_12;

/// A parsed `hybrid` query.
#[derive(Clone, Debug, PartialEq)]
pub struct Hybrid {
    /// The sub-queries, the `filter` already pushed into each.
    pub queries: Vec<Query>,
    pub pagination_depth: Option<i32>,
    pub common: Common,
    /// The search pipeline's phase-results processor, attached per search (never parsed).
    pub phase: Option<Arc<Phase>>,
}

fn unsupported(p: &XParser<'_>) -> EsError {
    p.parsing_error("Field is not supported by [hybrid] query")
}

fn filter_not_object(p: &XParser<'_>) -> EsError {
    p.parsing_error("[hybrid] query's [filter] field must be a query object")
}

impl Hybrid {
    /// `HybridQueryBuilder.fromXContent`.
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Hybrid> {
        let mut queries: Vec<Query> = Vec::new();
        let mut filter: Option<Query> = None;
        let mut common = Common::default();
        let mut depth: Option<i32> = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Tk::EndObject => break,
                Tk::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Tk::StartObject => match current.as_str() {
                    "queries" => queries.push(parse_inner(p)?),
                    "filter" => filter = Some(parse_inner(p)?),
                    _ => return Err(unsupported(p)),
                },
                Tk::StartArray => match current.as_str() {
                    "queries" => {
                        // The loop starts on the array token itself: `parseInnerQueryBuilder`
                        // moves to the first element (an empty array fails there).
                        let mut tok = Some(Tk::StartArray);
                        while tok.is_some_and(|t| t != Tk::EndArray) {
                            if queries.len() == MAX_SUB_QUERIES {
                                return Err(p.parsing_error("Number of sub-queries exceeds maximum supported by [hybrid] query"));
                            }
                            queries.push(parse_inner(p)?);
                            tok = p.next_token();
                        }
                    }
                    "filter" => return Err(filter_not_object(p)),
                    _ => return Err(unsupported(p)),
                },
                _ => match current.as_str() {
                    "boost" => {
                        let boost = p.float_value()?;
                        // Scores are shaped by the normalization, never by a boost.
                        if boost != 1.0 {
                            return Err(p.parsing_error("[hybrid] query does not support [boost]"));
                        }
                        common.boost = boost;
                    }
                    "_name" => common.name = Some(p.text()?),
                    "pagination_depth" => depth = Some(p.int_value()?),
                    "filter" => return Err(filter_not_object(p)),
                    _ => return Err(unsupported(p)),
                },
            }
        }
        if queries.is_empty() {
            return Err(p.parsing_error("[hybrid] requires 'queries' field with at least one clause"));
        }
        if let Some(f) = &filter {
            queries = queries.into_iter().map(|q| push_filter(q, f)).collect();
        }
        Ok(Hybrid { queries, pagination_depth: depth, common, phase: None })
    }

    /// `HybridQueryBuilder.doXContent` (the body under `hybrid`).
    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("queries".into(), Value::Arr(self.queries.iter().map(Query::to_value).collect()));
        if let Some(d) = self.pagination_depth {
            m.insert("pagination_depth".into(), Value::from(i64::from(d)));
        }
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// The search's hybrid query when it runs as one: the top-level query, or the only `must`
/// clause of a `bool` that has nothing else (Lucene rewrites such a `bool` to its clause).
pub fn top_level(q: &Query) -> Option<&Hybrid> {
    match q {
        Query::OsHybrid(h) => Some(h),
        Query::Bool(b) if lone_must(b) => top_level(&b.must[0]),
        _ => None,
    }
}

/// [`top_level`], mutable (to attach the pipeline's processor).
pub fn top_level_mut(q: &mut Query) -> Option<&mut Hybrid> {
    match q {
        Query::OsHybrid(h) => Some(h),
        Query::Bool(b) if lone_must(b) => top_level_mut(&mut b.must[0]),
        _ => None,
    }
}

/// Whether a hybrid query sits inside one of the compound queries the plugin refuses to run it
/// in (`HybridQueryPhaseSearcher.containsHybridQuery`: bool, dis_max, function_score,
/// boosting, constant_score, script_score).
pub fn nested_in_compound(q: &Query) -> bool {
    fn contains(q: &Query) -> bool {
        match q {
            Query::OsHybrid(_) => true,
            other => crate::stack::maybe_grow(|| inside(other)),
        }
    }
    fn inside(q: &Query) -> bool {
        match q {
            Query::Bool(b) => b.must.iter().chain(&b.filter).chain(&b.should).chain(&b.must_not).any(contains),
            Query::DisMax(d) => d.queries.iter().any(contains),
            Query::FunctionScore(f) => contains(&f.query),
            Query::ScriptScore(s) => contains(&s.query),
            Query::ConstantScore(c) => contains(&c.filter),
            Query::Boosting(b) => contains(&b.positive) || contains(&b.negative),
            _ => false,
        }
    }
    top_level(q).is_none() && inside(q)
}

/// The shard failure of a search whose hybrid query is wrapped in another query.
pub fn nested_error() -> EsError {
    EsError::illegal_argument(
        "hybrid query must be a top level query and cannot be wrapped into other queries. To use scoring wrapper queries (function_score, script_score, etc.) with hybrid sub-queries, replace the hybrid clause with a bool query using should clauses containing the same sub-queries",
    )
}

fn lone_must(b: &crate::query::compound::Bool) -> bool {
    b.must.len() == 1 && b.filter.is_empty() && b.should.is_empty() && b.must_not.is_empty() && b.common.boost == 1.0 && b.common.name.is_none()
}

/// `QueryBuilder.filter(filter)`: a `bool` gains a filter clause, the k-NN query filters its
/// own search, anything else becomes `bool {must: [q], filter: [f]}`.
pub fn push_filter(q: Query, f: &Query) -> Query {
    match q {
        Query::Bool(mut b) => {
            b.filter.push(f.clone());
            Query::Bool(b)
        }
        Query::OsKnn(mut k) => {
            k.filter = Some(match k.filter.take() {
                None => f.clone(),
                Some(existing) => push_filter(existing, f),
            });
            Query::OsKnn(k)
        }
        other => {
            let b = crate::query::compound::Bool { must: vec![other], filter: vec![f.clone()], ..Default::default() };
            Query::Bool(Box::new(b))
        }
    }
}

// ---- techniques ------------------------------------------------------------------------

/// `BoundMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundMode {
    Apply,
    Clip,
    Ignore,
}

impl BoundMode {
    fn name(self) -> &'static str {
        match self {
            BoundMode::Apply => "apply",
            BoundMode::Clip => "clip",
            BoundMode::Ignore => "ignore",
        }
    }

    /// `BoundMode.fromString`: empty is the default (`apply`).
    fn parse(s: &str) -> EsResult<BoundMode> {
        match s.trim().to_ascii_uppercase().as_str() {
            "" | "APPLY" => Ok(BoundMode::Apply),
            "CLIP" => Ok(BoundMode::Clip),
            "IGNORE" => Ok(BoundMode::Ignore),
            _ => Err(EsError::illegal_argument(format!("invalid mode: {s}, valid values are: apply, clip, ignore"))),
        }
    }
}

/// One sub-query's `lower_bounds` / `upper_bounds` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Bound {
    pub mode: BoundMode,
    pub score: f32,
    /// The score as configured (`Objects.toString`), for the technique's description.
    pub text: String,
}

/// A score normalization technique.
#[derive(Clone, Debug, PartialEq)]
pub enum Normalization {
    MinMax { lower: Option<Vec<Bound>>, upper: Option<Vec<Bound>> },
    L2,
    ZScore,
    Rrf { rank_constant: i32 },
}

impl Normalization {
    /// `describe()`: the name with its parameters, as explanations print it.
    pub fn describe(&self) -> String {
        match self {
            Normalization::MinMax { lower, upper } => {
                let mut s = String::from("min_max");
                for (label, bounds) in [("lower", lower), ("upper", upper)] {
                    if let Some(b) = bounds {
                        let parts: Vec<String> = b.iter().map(|b| format!("({}, {})", b.mode.name(), b.text)).collect();
                        s.push_str(&format!(", {label} bounds [{}]", parts.join(", ")));
                    }
                }
                s
            }
            Normalization::Rrf { rank_constant } => format!("rrf, rank_constant [{rank_constant}]"),
            other => other.name().to_string(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Normalization::MinMax { .. } => "min_max",
            Normalization::L2 => "l2",
            Normalization::ZScore => "z_score",
            Normalization::Rrf { .. } => "rrf",
        }
    }
}

/// A score combination technique.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CombinationKind {
    ArithmeticMean,
    GeometricMean,
    HarmonicMean,
    Rrf,
}

impl CombinationKind {
    pub fn name(self) -> &'static str {
        match self {
            CombinationKind::ArithmeticMean => "arithmetic_mean",
            CombinationKind::GeometricMean => "geometric_mean",
            CombinationKind::HarmonicMean => "harmonic_mean",
            CombinationKind::Rrf => "rrf",
        }
    }
}

/// A phase-results processor: how sub-query scores are normalized and combined.
#[derive(Clone, Debug, PartialEq)]
pub struct Phase {
    pub normalization: Normalization,
    pub combination: CombinationKind,
    /// Per sub-query weights (empty: 1 each).
    pub weights: Vec<f32>,
    /// The pipeline also has a `hybrid_score_explanation` response processor.
    pub explain_response: bool,
}

impl Phase {
    /// `ExplanationUtils.describeCombinationTechnique` (rrf never lists its weights).
    fn describe_combination(&self) -> String {
        if self.weights.is_empty() || self.combination == CombinationKind::Rrf {
            return self.combination.name().to_string();
        }
        format!("{}, weights {}", self.combination.name(), floats_string(&self.weights))
    }
}

/// `ScoreBound.MIN_BOUND_SCORE` / `MAX_BOUND_SCORE`.
const BOUND_LIMIT: f32 = 10_000.0;
/// `MinMaxScoreNormalizationTechnique.MIN_SCORE` (also z-score's floor).
const MIN_MAX_FLOOR: f32 = 0.001;

/// `String.valueOf(value)` of a configuration value.
fn java_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        other => other.to_es_string(),
    }
}

/// `ScoreNormalizationUtil.validateParameters`.
fn validate_norm_params(params: &Map, top: &[&str], nested: &[(&str, &[&str])]) -> EsResult<()> {
    let unknown = || EsError::illegal_argument("unrecognized parameters in normalization technique");
    let check_nested = |name: &str, m: &Map| -> EsResult<()> {
        let allowed = nested.iter().find(|(n, _)| *n == name).map(|(_, a)| *a);
        if let Some(allowed) = allowed
            && m.keys().any(|k| !allowed.contains(&k.as_str()))
        {
            return Err(unknown());
        }
        Ok(())
    };
    let mut has_unknown = false;
    for (name, value) in params {
        if !top.contains(&name.as_str()) {
            has_unknown = true;
            continue;
        }
        match value {
            Value::Obj(m) => check_nested(name, m)?,
            Value::Arr(items) => {
                for item in items {
                    match item {
                        Value::Obj(m) => check_nested(name, m)?,
                        _ => has_unknown = true,
                    }
                }
            }
            _ if nested.is_empty() => {}
            _ => has_unknown = true,
        }
    }
    if has_unknown { Err(unknown()) } else { Ok(()) }
}

/// `MinMaxScoreNormalizationTechnique.getBoundsParams`.
fn bounds(params: &Map, name: &str, score_key: &str, default: f32) -> EsResult<Option<Vec<Bound>>> {
    if !params.contains_key(name) {
        return Ok(None);
    }
    let Some(Value::Arr(items)) = params.get(name) else {
        return Err(EsError::illegal_argument(format!("{name} must be a List")));
    };
    if items.len() > MAX_SUB_QUERIES {
        return Err(EsError::illegal_argument(format!("{name} size {} should be less than or equal to {MAX_SUB_QUERIES}", items.len())));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Value::Obj(m) = item else { return Err(EsError::illegal_argument("each bound must be a map")) };
        let mode = match m.get("mode") {
            None | Some(Value::Null) => BoundMode::Apply,
            Some(v) => BoundMode::parse(&java_string(v))?,
        };
        let text = match m.get(score_key) {
            None | Some(Value::Null) => format!("{default:.1}"),
            Some(v) => java_string(v),
        };
        let score = match m.get(score_key) {
            None | Some(Value::Null) => default,
            Some(v) => {
                let s = java_string(v);
                let f = crate::query::xparser::java_parse_float(s.trim())
                    .map_err(|_| EsError::illegal_argument(format!("invalid format for {score_key}: must be a valid float value")))?;
                if !(-BOUND_LIMIT..=BOUND_LIMIT).contains(&f) {
                    return Err(EsError::illegal_argument(format!(
                        "{score_key} must be a valid finite number between {:.6} and {:.6}",
                        -BOUND_LIMIT, BOUND_LIMIT
                    )));
                }
                f
            }
        };
        out.push(Bound { mode, score, text });
    }
    Ok(Some(out))
}

/// `Integer.decode` (what `NumberUtils.createInteger` uses): decimal, `0x`/`#` hex, `0` octal.
fn java_decode_int(s: &str) -> Option<i32> {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (radix, digits) = if let Some(h) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")).or_else(|| rest.strip_prefix('#')) {
        (16, h)
    } else if rest.len() > 1 && rest.starts_with('0') {
        (8, &rest[1..])
    } else {
        (10, rest)
    };
    if digits.is_empty() || digits.starts_with(['-', '+']) {
        return None;
    }
    let v = i64::from_str_radix(digits, radix).ok()?;
    i32::try_from(if neg { -v } else { v }).ok()
}

/// `ScoreNormalizationFactory.createNormalization`.
pub fn normalization(name: &str, params: Option<&Map>) -> EsResult<Normalization> {
    let empty = Map::new();
    let params = params.unwrap_or(&empty);
    Ok(match name {
        "min_max" => {
            validate_norm_params(params, &["lower_bounds", "upper_bounds"], &[("lower_bounds", &["mode", "min_score"]), ("upper_bounds", &["mode", "max_score"])])?;
            Normalization::MinMax { lower: bounds(params, "lower_bounds", "min_score", 0.0)?, upper: bounds(params, "upper_bounds", "max_score", 1.0)? }
        }
        "l2" => {
            validate_norm_params(params, &[], &[])?;
            Normalization::L2
        }
        // `new ZScoreNormalizationTechnique()`: parameters are ignored.
        "z_score" => Normalization::ZScore,
        "rrf" => {
            validate_norm_params(params, &["rank_constant"], &[])?;
            Normalization::Rrf { rank_constant: rank_constant(params.get("rank_constant"))? }
        }
        _ => return Err(EsError::illegal_argument("provided normalization technique is not supported")),
    })
}

/// `RRFNormalizationTechnique.getRankConstant` (60 when absent).
pub fn rank_constant(v: Option<&Value>) -> EsResult<i32> {
    let Some(v) = v else { return Ok(60) };
    let c = java_decode_int(&java_string(v)).ok_or_else(|| EsError::illegal_argument("parameter [rank_constant] must be an integer"))?;
    if !(1..=10_000).contains(&c) {
        return Err(EsError::illegal_argument(format!("rank constant must be in the interval between 1 and 10000, submitted rank constant: {c}")));
    }
    Ok(c)
}

/// `class X cannot be cast to class java.lang.Double`, as the plugin's unchecked
/// `(List<Double>)` weights cast fails on anything but a decimal number.
fn cast_error(v: &Value) -> EsError {
    let class = match v {
        Value::Obj(_) => "java.util.HashMap",
        Value::Arr(_) => "java.util.ArrayList",
        Value::Str(_) => "java.lang.String",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Num(Number::Int(i)) if i32::try_from(*i).is_ok() => "java.lang.Integer",
        Value::Num(Number::Int(_)) => "java.lang.Long",
        Value::Num(_) => "java.math.BigInteger",
        Value::Null => {
            return EsError::java(
                500,
                "null_pointer_exception",
                "Cannot invoke \"java.lang.Double.floatValue()\" because the return value of \"java.util.Iterator.next()\" is null",
            );
        }
    };
    EsError::java(
        500,
        "class_cast_exception",
        format!("class {class} cannot be cast to class java.lang.Double ({class} and java.lang.Double are in module java.base of loader 'bootstrap')"),
    )
}

/// `Arrays.toString(Float[])`.
fn floats_string(w: &[f32]) -> String {
    let parts: Vec<String> = w.iter().map(|f| search_index::util::java::float_to_string(*f)).collect();
    format!("[{}]", parts.join(", "))
}

/// `ScoreCombinationFactory.createCombination`: the technique and its weights.
pub fn combination(name: &str, params: Option<&Map>) -> EsResult<(CombinationKind, Vec<f32>)> {
    let kind = match name {
        "arithmetic_mean" => CombinationKind::ArithmeticMean,
        "geometric_mean" => CombinationKind::GeometricMean,
        "harmonic_mean" => CombinationKind::HarmonicMean,
        "rrf" => CombinationKind::Rrf,
        _ => return Err(EsError::illegal_argument("provided combination technique is not supported")),
    };
    let Some(params) = params.filter(|p| !p.is_empty()) else { return Ok((kind, Vec::new())) };
    // `ScoreCombinationUtil.validateParams`.
    if params.keys().any(|k| k != "weights") {
        return Err(EsError::illegal_argument("provided parameter for combination technique is not supported. supported parameters are [weights]"));
    }
    let mut weights = Vec::new();
    match params.get("weights") {
        Some(Value::Arr(items)) => {
            for item in items {
                match item {
                    Value::Num(Number::Float(f)) => weights.push(*f as f32),
                    other => return Err(cast_error(other)),
                }
            }
        }
        Some(_) => return Err(EsError::illegal_argument("parameter [weights] must be a collection of numbers")),
        None => {}
    }
    if params.contains_key("weights") {
        // `ScoreCombinationUtil.validateWeights`.
        if weights.iter().any(|w| !(0.0..=1.0).contains(w)) {
            return Err(EsError::illegal_argument(format!("all weights must be in range [0.0 ... 1.0], submitted weights: {}", floats_string(&weights))));
        }
        let sum: f32 = weights.iter().fold(0.0f32, |a, w| a + w);
        if (1.0 - f64::from(sum)).abs() > 0.01 {
            return Err(EsError::illegal_argument(format!("sum of weights for combination must be equal to 1.0, submitted weights: {}", floats_string(&weights))));
        }
    }
    Ok((kind, weights))
}

/// `ScoreNormalizationFactory.isTechniquesCompatible`.
pub fn check_compatible(n: &Normalization, c: CombinationKind) -> EsResult<()> {
    let supported: &[CombinationKind] = match n {
        Normalization::ZScore => &[CombinationKind::ArithmeticMean],
        _ => &[CombinationKind::ArithmeticMean, CombinationKind::GeometricMean, CombinationKind::HarmonicMean],
    };
    if supported.contains(&c) {
        return Ok(());
    }
    let names: Vec<&str> = supported.iter().map(|k| k.name()).collect();
    Err(EsError::illegal_argument(format!(
        "provided combination technique {} is not supported for normalization technique {}. Supported techniques are: {}",
        c.name(),
        n.name(),
        names.join(", ")
    )))
}

// ---- normalization and combination ------------------------------------------------------

/// One shard's sub-query results: per sub-query its (doc, score) list, best first.
pub type ShardLists = Vec<Vec<(u32, f32)>>;

/// The bound-aware min-max normalization of one score (`normalizeSingleScore`).
fn min_max_score(score: f32, min: f32, max: f32, lower: Option<&Bound>, upper: Option<&Bound>) -> f32 {
    if max == min && max == score {
        return 1.0;
    }
    let eff_min = match lower {
        None => min,
        Some(b) => match b.mode {
            BoundMode::Apply if max > b.score && score > b.score => b.score,
            BoundMode::Apply | BoundMode::Ignore => min,
            BoundMode::Clip if max < b.score => min,
            BoundMode::Clip => b.score,
        },
    };
    let eff_max = match upper {
        None => max,
        Some(b) => match b.mode {
            BoundMode::Apply if min < b.score && score < b.score => b.score,
            BoundMode::Apply | BoundMode::Ignore => max,
            BoundMode::Clip if min > b.score => max,
            BoundMode::Clip => b.score,
        },
    };
    if lower.is_some_and(|b| b.mode == BoundMode::Clip) && score < eff_min {
        return MIN_MAX_FLOOR;
    }
    if upper.is_some_and(|b| b.mode == BoundMode::Clip) && score > eff_max {
        return 1.0;
    }
    if eff_max == eff_min {
        return 1.0;
    }
    let n = (score - eff_min) / (eff_max - eff_min);
    if n == 0.0 { MIN_MAX_FLOOR } else { n }
}

/// Commons-math `Mean.evaluate` (with its correction term) and the bias-corrected standard
/// deviation (`DescriptiveStatistics`).
fn mean_and_std(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let n = values.len() as f64;
    let sum: f64 = values.iter().sum();
    let xbar = sum / n;
    let correction: f64 = values.iter().map(|v| v - xbar).sum();
    let mean = xbar + correction / n;
    if values.len() == 1 {
        return (mean, 0.0);
    }
    let (mut accum, mut accum2) = (0.0f64, 0.0f64);
    for v in values {
        let dev = v - mean;
        accum += dev * dev;
        accum2 += dev;
    }
    let var = (accum - accum2 * accum2 / n) / (n - 1.0);
    (mean, var.sqrt())
}

/// Normalizes every shard's sub-query scores in place (`ScoreNormalizer.normalizeScores`).
/// `shards` holds the answering shards in shard-index order; `single` is a one-shard search.
pub fn normalize(n: &Normalization, shards: &mut [ShardLists], single: bool) -> EsResult<()> {
    let m = shards.iter().map(Vec::len).max().unwrap_or(0);
    if m == 0 {
        return Ok(());
    }
    match n {
        Normalization::MinMax { lower, upper } => {
            for b in [lower, upper].into_iter().flatten() {
                if b.len() != m {
                    return Err(EsError::illegal_argument(format!(
                        "expected bounds array to contain {m} elements matching the number of sub-queries, but found a mismatch"
                    )));
                }
            }
            // `Float.MAX_VALUE` / `Float.MIN_VALUE` (the smallest positive float).
            let mut mins = vec![f32::MAX; m];
            let mut maxs = vec![f32::from_bits(1); m];
            for s in shards.iter() {
                for (j, list) in s.iter().enumerate() {
                    for &(_, score) in list {
                        mins[j] = mins[j].min(score);
                        maxs[j] = maxs[j].max(score);
                    }
                }
            }
            for s in shards.iter_mut() {
                for (j, list) in s.iter_mut().enumerate() {
                    let lb = lower.as_ref().map(|b| &b[j]);
                    let ub = upper.as_ref().map(|b| &b[j]);
                    for e in list.iter_mut() {
                        e.1 = min_max_score(e.1, mins[j], maxs[j], lb, ub);
                    }
                }
            }
        }
        Normalization::L2 => {
            let mut norms = vec![0.0f32; m];
            for s in shards.iter() {
                for (j, list) in s.iter().enumerate() {
                    for &(_, score) in list {
                        norms[j] += score * score;
                    }
                }
            }
            for x in norms.iter_mut() {
                *x = f64::from(*x).sqrt() as f32;
            }
            for s in shards.iter_mut() {
                for (j, list) in s.iter_mut().enumerate() {
                    for e in list.iter_mut() {
                        e.1 = if norms[j] == 0.0 { 0.0 } else { e.1 / norms[j] };
                    }
                }
            }
        }
        Normalization::ZScore => {
            let mut stats = Vec::with_capacity(m);
            for j in 0..m {
                let values: Vec<f64> = shards.iter().filter_map(|s| s.get(j)).flat_map(|l| l.iter().map(|e| f64::from(e.1))).collect();
                let (mean, std) = mean_and_std(&values);
                let max = values.iter().copied().reduce(f64::max).unwrap_or(f64::NAN);
                let min = values.iter().copied().reduce(f64::min).unwrap_or(f64::NAN);
                stats.push((mean as f32, std as f32, max as f32, min as f32));
            }
            for s in shards.iter_mut() {
                for (j, list) in s.iter_mut().enumerate() {
                    let (mean, std, max, min) = stats[j];
                    for e in list.iter_mut() {
                        e.1 = if mean == e.1 {
                            max
                        } else if std == 0.0 {
                            min
                        } else {
                            let z = (e.1 - mean) / std;
                            if z <= 0.0 { MIN_MAX_FLOOR } else { z }
                        };
                    }
                }
            }
        }
        Normalization::Rrf { rank_constant } => {
            // Ranks are global over the shards (`ScoreDoc.COMPARATOR`, then shard), except on a
            // single shard where the position is the rank.
            let mut global: Vec<HashMap<(usize, u32), usize>> = vec![HashMap::new(); m];
            if !single {
                for (j, ranks) in global.iter_mut().enumerate() {
                    let mut all: Vec<(f32, u32, usize)> = Vec::new();
                    for (si, s) in shards.iter().enumerate() {
                        if let Some(list) = s.get(j) {
                            all.extend(list.iter().map(|&(d, sc)| (sc, d, si)));
                        }
                    }
                    all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
                    for (rank, (_, d, si)) in all.into_iter().enumerate() {
                        ranks.insert((si, d), rank);
                    }
                }
            }
            for (si, s) in shards.iter_mut().enumerate() {
                for (j, list) in s.iter_mut().enumerate() {
                    for (pos, e) in list.iter_mut().enumerate() {
                        let rank = if single { pos } else { global[j].get(&(si, e.0)).copied().unwrap_or(pos) };
                        e.1 = rrf_score(*rank_constant, rank);
                    }
                }
            }
        }
    }
    Ok(())
}

/// `1 / (rank_constant + rank + 1)` rounded half-up to 10 decimals, as a float.
fn rrf_score(rank_constant: i32, rank: usize) -> f32 {
    let d = i64::from(rank_constant) + rank as i64 + 1;
    let num = (2 * 10_000_000_000i64 + d) / (2 * d);
    (num as f64 / 1.0e10) as f32
}

/// `ScoreCombinationTechnique.combine` over one document's normalized scores (0 where a
/// sub-query didn't return it).
pub fn combine(kind: CombinationKind, weights: &[f32], scores: &[f32]) -> EsResult<f32> {
    if !weights.is_empty() && weights.len() != scores.len() {
        return Err(EsError::illegal_argument(format!(
            "number of weights [{}] must match number of sub-queries [{}] in hybrid query",
            weights.len(),
            scores.len()
        )));
    }
    let w = |i: usize| weights.get(i).copied().unwrap_or(1.0);
    Ok(match kind {
        CombinationKind::ArithmeticMean | CombinationKind::Rrf => {
            let (mut sum, mut sum_w) = (0.0f32, 0.0f32);
            for (i, &s) in scores.iter().enumerate() {
                if s >= 0.0 {
                    sum += s * w(i);
                    sum_w += w(i);
                }
            }
            if sum_w == 0.0 {
                0.0
            } else if kind == CombinationKind::Rrf {
                sum
            } else {
                sum / sum_w
            }
        }
        CombinationKind::GeometricMean => {
            let (mut ln_sum, mut sum_w) = (0.0f32, 0.0f32);
            for (i, &s) in scores.iter().enumerate() {
                if s <= 0.0 {
                    continue;
                }
                sum_w += w(i);
                // `float += double`: computed in double, narrowed.
                ln_sum = (f64::from(ln_sum) + f64::from(w(i)) * f64::from(s).ln()) as f32;
            }
            if sum_w == 0.0 { 0.0 } else { f64::from(ln_sum / sum_w).exp() as f32 }
        }
        CombinationKind::HarmonicMean => {
            let (mut sum_w, mut harmonics) = (0.0f32, 0.0f32);
            for (i, &s) in scores.iter().enumerate() {
                if s <= 0.0 {
                    continue;
                }
                sum_w += w(i);
                harmonics += w(i) / s;
            }
            if harmonics > 0.0 { sum_w / harmonics } else { 0.0 }
        }
    })
}

/// The iteration order of a `java.util.HashMap<Integer, _>` that got `keys` inserted in order
/// (default capacity, doubling past 0.75 load): by bucket, then insertion.
fn int_hash_order(keys: &[u32]) -> Vec<usize> {
    let mut cap = 16usize;
    while keys.len() > cap * 3 / 4 {
        cap *= 2;
    }
    let mask = (cap - 1) as u32;
    let mut idx: Vec<usize> = (0..keys.len()).collect();
    idx.sort_by_key(|&i| {
        let h = keys[i];
        ((h ^ (h >> 16)) & mask, i)
    });
    idx
}

/// One shard's fused documents, (doc, combined score) in the shard's final order, and how many
/// documents its sub-queries returned.
fn combine_shard(phase: &Phase, lists: &ShardLists, min_score: Option<f32>) -> EsResult<(Vec<(u32, f32)>, usize)> {
    let m = lists.len();
    // `getNormalizedScoresPerDocument`: a HashMap filled sub-query by sub-query.
    let mut order: Vec<u32> = Vec::new();
    let mut scores: HashMap<u32, Vec<f32>> = HashMap::new();
    for (j, list) in lists.iter().enumerate() {
        for &(doc, s) in list {
            scores.entry(doc).or_insert_with(|| {
                order.push(doc);
                vec![0.0; m]
            })[j] = s;
        }
    }
    let collected = order.len();
    let mut combined: Vec<(u32, f32)> = Vec::with_capacity(order.len());
    for i in int_hash_order(&order) {
        let doc = order[i];
        let c = combine(phase.combination, &phase.weights, &scores[&doc])?;
        if min_score.is_none_or(|ms| c >= ms) {
            combined.push((doc, c));
        }
    }
    // `Collectors.toMap` re-hashes into a fresh map, then a stable sort by score.
    let keys: Vec<u32> = combined.iter().map(|e| e.0).collect();
    let mut out: Vec<(u32, f32)> = int_hash_order(&keys).into_iter().map(|i| combined[i]).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok((out, collected))
}

/// `TopDocs.merge` over per-shard lists, each `(shard index, entries)` and read in its own
/// order: repeatedly the head with the highest score, equal scores from the lower shard index.
/// Returns (list position, entry, score).
fn merge_lists<T: Copy>(lists: &[(usize, Vec<(T, f32)>)], take: usize) -> Vec<(usize, T, f32)> {
    let mut pos = vec![0usize; lists.len()];
    let mut out = Vec::new();
    while out.len() < take {
        let mut best: Option<usize> = None;
        for (i, (rank, l)) in lists.iter().enumerate() {
            let Some(&(_, s)) = l.get(pos[i]) else { continue };
            best = match best {
                Some(b) => {
                    let (brank, bl) = &lists[b];
                    let bs = bl[pos[b]].1;
                    if s > bs || (s == bs && rank < brank) { Some(i) } else { Some(b) }
                }
                None => Some(i),
            };
        }
        let Some(b) = best else { break };
        let (t, s) = lists[b].1[pos[b]];
        out.push((b, t, s));
        pos[b] += 1;
    }
    out
}

/// The fused (normalized and combined) ranking of the answering shards.
pub struct Fusion {
    /// Per shard: its fused docs, in order.
    pub shards: Vec<Vec<(u32, f32)>>,
    /// Per shard: the documents its sub-queries returned (before `min_score`).
    pub collected: Vec<usize>,
    /// With `explain`: per shard, its docs in the explain pass's order with their normalized
    /// scores per sub-query (`ScoreCombiner.explain`, `ScoreNormalizer.explain`).
    pub explained: Vec<Vec<(u32, Vec<f32>)>>,
}

/// `NormalizationProcessorWorkflow.execute` up to the shards' fused lists. With `explain` the
/// workflow's explain pass normalizes the scores in place first, so the final scores are
/// normalized twice, as OpenSearch's are.
pub fn fuse(phase: &Phase, mut shards: Vec<ShardLists>, single: bool, min_score: Option<f32>, explain: bool) -> EsResult<Fusion> {
    let mut explained = Vec::new();
    if explain {
        normalize(&phase.normalization, &mut shards, single)?;
        for lists in &shards {
            let (docs, _) = combine_shard(phase, lists, min_score)?;
            let m = lists.len();
            let per_doc: Vec<(u32, Vec<f32>)> = docs
                .iter()
                .map(|(doc, _)| {
                    let scores = (0..m).map(|j| lists[j].iter().find(|e| e.0 == *doc).map_or(0.0, |e| e.1)).collect();
                    (*doc, scores)
                })
                .collect();
            explained.push(per_doc);
        }
    }
    normalize(&phase.normalization, &mut shards, single)?;
    let mut out = Fusion { shards: Vec::with_capacity(shards.len()), collected: Vec::with_capacity(shards.len()), explained };
    for lists in &shards {
        let (docs, collected) = combine_shard(phase, lists, min_score)?;
        out.shards.push(docs);
        out.collected.push(collected);
    }
    Ok(out)
}

/// One shard's raw hybrid result list (`HybridSearchCollectorResultUtil.getNewTopDocs`):
/// start, per sub-query a delimiter and its hits, stop; the markers carry the first hit's doc.
pub fn raw(lists: &ShardLists) -> Vec<(u32, f32)> {
    let Some(marker) = lists.iter().find_map(|l| l.first().map(|e| e.0)) else { return Vec::new() };
    let mut out = vec![(marker, MAGIC_START_STOP)];
    for l in lists {
        out.push((marker, MAGIC_DELIMITER));
        out.extend_from_slice(l);
    }
    out.push((marker, MAGIC_START_STOP));
    out
}

// ---- execution -------------------------------------------------------------------------

/// `SearchPhaseExecutionException("query", "The phase has failed", …)`: a processor failure.
fn phase_failed(e: EsError) -> EsError {
    EsError::new(e.status, "search_phase_execution_exception", "The phase has failed")
        .with_meta("phase", "query")
        .with_meta("grouped", true)
        .with_meta("failed_shards", Value::Arr(Vec::new()))
        .caused_by(e)
        .with_root_causes(Vec::new())
}

/// `ShardId` order (shard number, index name, uuid): the shard index of each target.
fn shard_indices(targets: &[Target]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..targets.len()).collect();
    order.sort_by_cached_key(|&i| (targets[i].shard.id, targets[i].idx.name.clone(), targets[i].idx.uuid.clone()));
    let mut out = vec![0; targets.len()];
    for (rank, t) in order.into_iter().enumerate() {
        out[t] = rank;
    }
    out
}

/// `HybridCollectorManager.validateSortCriteria`.
fn check_sort(src: &crate::search_source::SearchSource) -> EsResult<()> {
    if src.sort.is_empty() {
        return Ok(());
    }
    let has_score = src.sort.iter().any(|s| matches!(s, Sort::Score { .. }));
    let has_field = src.sort.iter().any(|s| !matches!(s, Sort::Score { .. }));
    let arg = EsError::illegal_argument;
    if has_score && has_field {
        return Err(arg("_score sort criteria cannot be applied with any other criteria. Please select one sort criteria out of them."));
    }
    if src.track_scores && has_field {
        return Err(arg("Hybrid search results when sorted by any field, docId or _id, track_scores must be set to false."));
    }
    if src.track_scores && has_score {
        return Err(arg("Hybrid search results are by default sorted by _score, track_scores must be set to false."));
    }
    if src.min_score.is_some() && has_field {
        return Err(arg("Hybrid search results when sorted by any field, docId or _id, min_score cannot be set."));
    }
    Ok(())
}

/// The checks each shard runs as it builds the hybrid query and its collector.
fn shard_checks(t: &Target, h: &Hybrid, req: &SearchRequest, from: i64, size: i64) -> EsResult<()> {
    if let Some(d) = h.pagination_depth {
        let qse = |m: &str| crate::search::query::shard_exception(&t.idx, EsError::illegal_argument(m));
        if d < 0 {
            return Err(qse("pagination_depth should be greater than 0"));
        }
        let window = t.idx.settings.get("index.max_result_window").and_then(|v| v.parse::<i64>().ok()).unwrap_or(10_000);
        if i64::from(d) > window {
            return Err(qse("pagination_depth should be less than or equal to index.max_result_window setting"));
        }
    }
    if req.scroll.is_some() {
        return Err(EsError::illegal_argument("Scroll operation is not supported in hybrid query"));
    }
    if h.pagination_depth.is_none() && from > 0 {
        return Err(EsError::illegal_argument("pagination_depth param is missing in the search request"));
    }
    check_sort(&req.source)?;
    crate::search::check_window(&t.idx, from, size, false)
}

/// A sub-query's top `n` (doc, score, collapse group) on one shard. With `collapse` each
/// sub-query collects its best document per group.
fn sub_search(cluster: &Cluster, req: &SearchRequest, t: &Target, ti: usize, shards: usize, q: &Query, n: usize) -> EsResult<Vec<(u32, f32, Option<SortValue>)>> {
    let mut creq = req.clone();
    let src = &mut creq.source;
    src.query = Some(q.clone());
    src.from = Some(0);
    src.size = Some(n.min(i32::MAX as usize) as i32);
    src.aggs = None;
    src.track_total_hits = Some(TrackTotalHits::Disabled);
    src.min_score = None;
    // Sorted sub-queries still score: the processor normalizes their scores.
    src.track_scores = true;
    src.explain = None;
    src.profile = false;
    src.highlight = None;
    src.suggest = None;
    src.rescore.clear();
    if let Some(c) = &mut src.collapse {
        c.inner_hits.clear();
    }
    src.knn.clear();
    creq.fused = None;
    creq.rank_only = true;
    creq.scroll = None;
    let st = cluster.state();
    let after = creq.source.search_after.clone();
    let r = crate::search::query_phase(cluster, &st, &creq, t, ti, shards, n, after.as_deref(), None, &[], None)?;
    Ok(r.hits.iter().map(|h| (h.doc, h.score, h.group.clone())).collect())
}

/// `hybrid_score_explanation`'s inputs: per target its explained docs, then the normalization
/// and the combination descriptions.
type Explained = (HashMap<usize, Vec<(u32, Vec<f32>)>>, String, String);

/// How the page is laid out after fusing.
enum Layout {
    /// The page in order: (target, doc, score).
    Page(Vec<(usize, u32, f32)>),
    /// Every fused doc, ordered by the request's sort in the final phase.
    Sorted(Vec<(usize, u32, f32)>),
}

/// Runs a search whose query is a top-level `hybrid` query.
pub fn execute(cluster: &Cluster, req: &SearchRequest, targets: &[Target], h: &Hybrid) -> EsResult<SearchOutcome> {
    let started = std::time::Instant::now();
    if req.dfs {
        return Err(EsError::illegal_argument("hybrid query does not support search_type [dfs_query_then_fetch]"));
    }
    let src = &req.source;
    let from = i64::from(src.from.unwrap_or(0).max(0));
    let size = i64::from(src.size.unwrap_or(10).max(0));
    // Pin each shard's searcher: the final phase must see the doc ids the sub-queries saw.
    let mut pinned: Vec<Target> = Vec::with_capacity(targets.len());
    for t in targets {
        let mut t = t.clone();
        if t.searcher.is_none() {
            t.searcher = Some(t.searcher()?);
        }
        pinned.push(t);
    }
    let shard_index = shard_indices(&pinned);
    let mut failures: Vec<(usize, EsError)> = Vec::new();
    // Per answering shard: (target, its sub-query lists).
    let mut answered: Vec<(usize, ShardLists)> = Vec::new();
    let mut raw_max: Option<f32> = None;
    // `collapse`: each collected doc's group.
    let mut groups: HashMap<(usize, u32), SortValue> = HashMap::new();
    for (ti, t) in pinned.iter().enumerate() {
        if let Err(e) = shard_checks(t, h, req, from, size) {
            failures.push((ti, e));
            continue;
        }
        let wanted = match h.pagination_depth {
            Some(d) => d.max(0) as usize,
            None => size as usize,
        };
        let n = wanted.min(t.searcher()?.num_docs() as usize);
        let mut lists = Vec::with_capacity(h.queries.len());
        let mut failed = None;
        for q in &h.queries {
            // With nothing to collect, the shard still reports its best score.
            match sub_search(cluster, req, t, ti, pinned.len(), q, n.max(1)) {
                Ok(mut hits) => {
                    if let Some(&(_, s, _)) = hits.first()
                        && !s.is_nan()
                    {
                        raw_max = Some(raw_max.map_or(s, |m| m.max(s)));
                    }
                    hits.truncate(n);
                    for (d, _, g) in &mut hits {
                        if let Some(g) = g.take() {
                            groups.insert((ti, *d), g);
                        }
                    }
                    lists.push(hits.into_iter().map(|(d, s, _)| (d, s)).collect());
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        match failed {
            Some(e) => failures.push((ti, e)),
            None => answered.push((ti, lists)),
        }
    }
    if answered.is_empty() && !failures.is_empty() {
        return Err(crate::search::all_shards_failed(&cluster.node.id, &pinned, &failures, "all shards failed"));
    }
    // Shard index order: how the plugin sees the query results.
    answered.sort_by_key(|(ti, _)| shard_index[*ti]);
    let has_hits = answered.iter().any(|(_, l)| l.iter().any(|x| !x.is_empty()));
    let sorted = !src.sort.is_empty() && !src.sort.iter().all(|s| matches!(s, Sort::Score { order: SortOrder::Desc }));
    let window = (from + size) as usize;
    let targets_of: Vec<usize> = answered.iter().map(|(ti, _)| *ti).collect();
    let mut min_score_total: Option<(usize, usize)> = None;
    let max_score: Option<f32>;
    // `hybrid_score_explanation`: per target its explained docs, the normalization and the
    // combination descriptions.
    let mut explanation: Option<Explained> = None;
    let layout = match &h.phase {
        Some(phase) if has_hits => {
            let lists: Vec<ShardLists> = answered.iter().map(|(_, l)| l.clone()).collect();
            let min_score = if sorted { None } else { src.min_score };
            let fused = fuse(phase, lists, pinned.len() == 1, min_score, src.explain == Some(true)).map_err(phase_failed)?;
            if phase.explain_response && !fused.explained.is_empty() {
                let description = format!(
                    "{} combination of{}:",
                    phase.describe_combination(),
                    min_score.map(|m| format!(" [filtered by min_score: {m:.4}]")).unwrap_or_default()
                );
                explanation = Some((targets_of.iter().copied().zip(fused.explained.clone()).collect::<HashMap<_, _>>(), format!("{} normalization of:", phase.normalization.describe()), description));
            }
            if min_score.is_some() {
                min_score_total = Some((fused.shards.iter().map(Vec::len).sum(), fused.collected.iter().sum()));
            }
            let total_docs: usize = fused.shards.iter().map(Vec::len).sum();
            if from as usize > total_docs {
                return Err(phase_failed(EsError::illegal_argument("Reached end of search result, increase pagination_depth value to see more results")));
            }
            let per_shard: Vec<(usize, Vec<(u32, f32)>)> = targets_of.iter().zip(fused.shards).map(|(ti, docs)| (shard_index[*ti], docs)).collect();
            if sorted {
                // The combined scores still make the max score, except on a single shard,
                // whose sorted docs carry no score (`NaN`).
                max_score = if pinned.len() == 1 { None } else { per_shard.iter().flat_map(|(_, d)| d.iter().map(|e| e.1)).reduce(f32::max) };
                Layout::Sorted(per_shard.iter().enumerate().flat_map(|(i, (_, docs))| docs.iter().map(move |&(d, s)| (i, d, s))).map(|(i, d, s)| (targets_of[i], d, s)).collect())
            } else {
                max_score = per_shard.iter().filter_map(|(_, d)| d.first().map(|e| e.1)).reduce(f32::max);
                let merged = if src.collapse.is_some() {
                    // `CollapseExecutor`: the best combined document of each group, groups in
                    // combined-score order.
                    let mut all = merge_lists(&per_shard, usize::MAX);
                    let mut seen: Vec<Option<SortValue>> = Vec::new();
                    all.retain(|(i, d, _)| {
                        let g = groups.get(&(targets_of[*i], *d)).cloned();
                        if seen.contains(&g) {
                            false
                        } else {
                            seen.push(g);
                            true
                        }
                    });
                    all.truncate(window);
                    all
                } else {
                    merge_lists(&per_shard, window)
                };
                Layout::Page(merged.into_iter().skip(from as usize).map(|(i, d, s)| (targets_of[i], d, s)).collect())
            }
        }
        // Nothing collected: the processor has nothing to fuse.
        Some(_) => {
            max_score = Some(raw_max.unwrap_or(0.0).max(0.0));
            Layout::Page(Vec::new())
        }
        None => {
            let per_shard: Vec<(usize, Vec<(u32, f32)>)> = answered.iter().map(|(ti, l)| (shard_index[*ti], raw(l))).collect();
            max_score = Some(raw_max.unwrap_or(0.0).max(0.0));
            Layout::Page(merge_lists(&per_shard, window).into_iter().skip(from as usize).map(|(i, d, s)| (targets_of[i], d, s)).collect())
        }
    };
    // The final phase runs on the answering shards: it fetches the page's documents and
    // computes totals and aggregations over the union of the sub-queries' matches.
    let local: HashMap<usize, usize> = targets_of.iter().enumerate().map(|(i, ti)| (*ti, i)).collect();
    let surviving: Vec<Target> = targets_of.iter().map(|ti| pinned[*ti].clone()).collect();
    let (docs, sorted_layout) = match layout {
        Layout::Page(p) => (p, false),
        Layout::Sorted(all) => (all, true),
    };
    let mut unique: Vec<(usize, u32, f32)> = Vec::new();
    for &(t, d, s) in &docs {
        if !unique.iter().any(|u| u.0 == t && u.1 == d) {
            unique.push((t, d, s));
        }
    }
    let n = unique.len();
    // A page's rank docs score by position, so the final phase keeps the fused order.
    let fused_docs: Vec<FusedDoc<(usize, u32)>> = unique
        .iter()
        .enumerate()
        .map(|(i, &(t, d, s))| FusedDoc { key: (local[&t], d), score: if sorted_layout { s } else { (n - i) as f32 }, per_child: Vec::new() })
        .collect();
    let node = Node { kind: RankKind::Hybrid, names: vec![None; h.queries.len()], fused: fused_docs, children: h.queries.iter().map(|q| Child::Parsed(q.clone())).collect() };
    let mut freq = req.clone();
    freq.fused = Some(Arc::new(Fused { cap: n, node: Arc::new(node) }));
    freq.dfs = false;
    freq.scroll = None;
    let fs = &mut freq.source;
    fs.os_knn = src.query.as_ref().is_some_and(super::profile::knn_decides);
    fs.query = None;
    fs.knn.clear();
    fs.min_score = None;
    fs.from = Some(0);
    // At least one: with nothing to collect the shard would count the rank docs alone.
    fs.size = Some(n.clamp(1, i32::MAX as usize) as i32);
    if !sorted_layout {
        fs.sort.clear();
    }
    let mut out = crate::search::execute_on(cluster, &freq, &surviving)?;
    if !sorted_layout {
        let rendered: HashMap<(usize, u32), Value> = out.ranked.iter().zip(out.hits.iter()).map(|(r, hit)| ((targets_of[r.0], r.1), hit.clone())).collect();
        let mut hits = Vec::with_capacity(docs.len());
        let mut ranked = Vec::with_capacity(docs.len());
        for &(t, d, s) in docs.iter().take(size as usize) {
            let Some(mut hit) = rendered.get(&(t, d)).cloned() else { continue };
            if let Value::Obj(m) = &mut hit
                && let Some(slot) = m.get_mut("_score")
            {
                *slot = Value::Num(Number::Float(crate::fetch::java_float_as_double(s)));
            }
            hits.push(hit);
            ranked.push((t, d, s));
        }
        if let Some((per_target, norm, comb)) = &explanation {
            explain_hits(&mut hits, &ranked, per_target, norm, comb)?;
        }
        out.hits = hits;
        out.ranked = ranked;
        out.max_score = Some(max_score.unwrap_or(0.0));
    } else {
        // Every fused doc came back in sort order. Equal sort values order as the plugin's
        // merges leave them: by shard index, then (`SORTING_TIE_BREAKER`) lower combined score,
        // then doc.
        let combined: HashMap<(usize, u32), f32> = docs.iter().map(|&(t, d, s)| ((t, d), s)).collect();
        let mut rows: Vec<(Value, (usize, u32, f32))> = Vec::with_capacity(out.hits.len());
        for (r, hit) in out.ranked.iter().zip(out.hits.drain(..)) {
            let t = targets_of[r.0];
            rows.push((hit, (t, r.1, combined.get(&(t, r.1)).copied().unwrap_or(0.0))));
        }
        let mut start = 0;
        while start < rows.len() {
            let key = rows[start].0.get("sort").cloned();
            let mut end = start + 1;
            while end < rows.len() && rows[end].0.get("sort").cloned() == key {
                end += 1;
            }
            rows[start..end].sort_by(|a, b| shard_index[a.1.0].cmp(&shard_index[b.1.0]).then(a.1.2.total_cmp(&b.1.2)).then(a.1.1.cmp(&b.1.1)));
            start = end;
        }
        let page: Vec<(Value, (usize, u32, f32))> = rows.into_iter().skip(from as usize).take(size as usize).collect();
        out.ranked = page.iter().map(|(_, r)| *r).collect();
        out.hits = page.into_iter().map(|(h, _)| h).collect();
        out.max_score = max_score;
    }
    // `ScoreCombiner.getTotalHits`: with `min_score`, the documents left above it.
    if let (Some((kept, collected)), Some((_, relation))) = (min_score_total, out.total) {
        out.total = Some((kept as u64, if kept < collected { TotalRelation::EqualTo } else { relation }));
    }
    if let Some(p) = &mut out.profile {
        hybrid_profile(p);
    }
    // Shards that failed their checks report as shard failures.
    let mut rendered_failures: Vec<Value> = failures.iter().map(|(i, e)| crate::search::shard_failure(&cluster.node.id, &pinned[*i], e)).collect();
    rendered_failures.append(&mut out.failures);
    out.failures = rendered_failures;
    out.shards_total = pinned.len();
    out.took_ms = started.elapsed().as_millis() as u64;
    Ok(out)
}

/// The final phase's profile as the plugin's query phase reports it: the hybrid query (with
/// the sub-queries the rank docs' tail ran) where the rank-docs query was, and the hybrid
/// collectors.
fn hybrid_profile(profile: &mut Value) {
    let Value::Obj(p) = profile else { return };
    let Some(Value::Arr(shards)) = p.get_mut("shards") else { return };
    for shard in shards {
        let Value::Obj(shard) = shard else { continue };
        let Some(Value::Arr(searches)) = shard.get_mut("searches") else { continue };
        let Some(Value::Obj(search)) = searches.first_mut() else { continue };
        if let Some(Value::Arr(queries)) = search.get_mut("query")
            && let Some(top) = queries.first_mut()
            && top.get("type").and_then(Value::as_str) == Some("RankDocsQuery")
            && let Some(hybrid) = top.get("children").and_then(Value::as_array).and_then(|c| c.iter().find(|n| n.get("type").and_then(Value::as_str) == Some("HybridQuery")))
        {
            *top = hybrid.clone();
        }
        if let Some(Value::Arr(collectors)) = search.get_mut("collector") {
            for c in collectors.iter_mut() {
                rename_collectors(c);
            }
        }
    }
}

fn rename_collectors(c: &mut Value) {
    let Value::Obj(m) = c else { return };
    let renamed = match m.get("name").and_then(Value::as_str) {
        Some("TopScoreDocCollector") => Some("HybridTopScoreDocCollector"),
        Some("SimpleTopDocsCollectorManager") => Some("HybridCollectorManager"),
        Some("SimpleFieldCollector") | Some("PagingFieldCollector") => Some("HybridTopFieldDocSortCollector"),
        _ => None,
    };
    if let Some(n) = renamed {
        m.insert("name".into(), Value::from(n));
    }
    if let Some(Value::Arr(children)) = m.get_mut("children") {
        for ch in children.iter_mut() {
            rename_collectors(ch);
        }
    }
}

/// `ExplanationResponseProcessor.processResponse`: each hit's explanation becomes the
/// combination over its normalized sub-query explanations. A shard's k-th hit takes the
/// shard's k-th explained document's normalized scores (the plugin pairs them by position).
fn explain_hits(hits: &mut [Value], ranked: &[(usize, u32, f32)], per_target: &HashMap<usize, Vec<(u32, Vec<f32>)>>, norm: &str, comb: &str) -> EsResult<()> {
    fn hybrid_node(e: &Value) -> Option<&Value> {
        if e.get("description").and_then(Value::as_str) == Some("combined score of:") {
            return Some(e);
        }
        e.get("details").and_then(Value::as_array)?.iter().find_map(hybrid_node)
    }
    let float = |f: f32| Value::Num(Number::Float(crate::fetch::java_float_as_double(f)));
    let mut seen: HashMap<usize, usize> = HashMap::new();
    for (hit, &(t, _, score)) in hits.iter_mut().zip(ranked) {
        let k = seen.entry(t).or_insert(0);
        let Some(scores) = per_target.get(&t).and_then(|l| l.get(*k)).map(|e| &e.1) else { continue };
        *k += 1;
        let Value::Obj(m) = hit else { continue };
        let Some(query_level) = m.get("_explanation").cloned() else { continue };
        let node = hybrid_node(&query_level).cloned().unwrap_or(query_level);
        let details = node.get("details").and_then(Value::as_array).cloned().unwrap_or_default();
        if details.len() != scores.len() {
            return Err(EsError::illegal_state("mismatch in number of query level explanations and normalization explanations"));
        }
        let mut normalized = Vec::new();
        for (q, s) in details.into_iter().zip(scores) {
            if q.get("value").and_then(Value::as_f64).is_some_and(|v| v as f32 > 0.0) {
                normalized.push(crate::obj! { "value" => float(*s), "description" => norm, "details" => Value::Arr(vec![q]) });
            }
        }
        let final_score = if score.is_nan() { 0.0 } else { score };
        m.insert("_explanation".into(), crate::obj! { "value" => float(final_score), "description" => comb, "details" => Value::Arr(normalized) });
    }
    Ok(())
}

/// The shard query of a `hybrid` query that isn't the search's top-level query (inside
/// another query, a count, a delete-by-query): the disjunction of its sub-queries.
pub fn to_query(ctx: &crate::search::query::QueryCtx, h: &Hybrid) -> EsResult<search_index::QueryRef> {
    let mut subs = Vec::with_capacity(h.queries.len());
    for q in &h.queries {
        subs.push(ctx.to_query(q)?);
    }
    Ok(union(subs))
}

/// What any of `subs` matches, as the plugin's `HybridQuery` (also the tail of a hybrid
/// search's final phase, so its profile shows the sub-queries as given).
pub fn union(subs: Vec<search_index::QueryRef>) -> search_index::QueryRef {
    let mut b = search_index::query::BooleanQuery::new();
    for q in &subs {
        b = b.add(search_index::query::Occur::Should, q.clone());
    }
    Arc::new(HybridQuery { inner: Arc::new(b), subs })
}

/// The shard query of a hybrid query run as a plain query: what any sub-query matches, printed
/// as `HybridQuery.toString` (`(a | (b c))`).
#[derive(Debug)]
struct HybridQuery {
    inner: search_index::QueryRef,
    subs: Vec<search_index::QueryRef>,
}

impl search_index::Query for HybridQuery {
    fn weight(&self, s: &search_index::Searcher, mode: search_index::ScoreMode, boost: f32) -> search_index::Result<Box<dyn search_index::Weight>> {
        self.inner.weight(s, mode, boost)
    }

    fn rewrite(&self, s: &search_index::Searcher) -> search_index::Result<Option<search_index::QueryRef>> {
        let mut changed = false;
        let mut subs = Vec::with_capacity(self.subs.len());
        for q in &self.subs {
            let r = s.rewrite(q)?;
            changed |= !Arc::ptr_eq(&r, q);
            subs.push(r);
        }
        if !changed {
            return Ok(None);
        }
        let mut b = search_index::query::BooleanQuery::new();
        for q in &subs {
            b = b.add(search_index::query::Occur::Should, q.clone());
        }
        Ok(Some(Arc::new(HybridQuery { inner: Arc::new(b), subs })))
    }

    fn lucene_string(&self, schema: &search_index::Schema) -> String {
        let parts: Vec<String> = self
            .subs
            .iter()
            .map(|q| {
                let s = q.lucene_string(schema);
                if q.as_any().is::<search_index::query::BooleanQuery>() { format!("({s})") } else { s }
            })
            .collect();
        format!("({})", parts.join(" | "))
    }

    fn visit(&self, v: &mut dyn search_index::search::QueryVisitor) {
        self.inner.visit(v);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(n: Normalization, c: CombinationKind, w: &[f32]) -> Phase {
        Phase { normalization: n, combination: c, weights: w.to_vec(), explain_response: false }
    }

    #[test]
    fn min_max_arithmetic_matches_opensearch() {
        // OpenSearch 3.9.0: match "quick" scores docs 0 and 2 equally; knn k=3 gives
        // 0: 0.5, 1 and 3: 0.33333334; weights [0.3, 0.7].
        let p = phase(Normalization::MinMax { lower: None, upper: None }, CombinationKind::ArithmeticMean, &[0.3, 0.7]);
        let lists = vec![vec![vec![(0, 0.29123834), (2, 0.29123834)], vec![(0, 0.5), (1, 0.33333334), (3, 0.33333334)]]];
        let f = fuse(&p, lists, true, None, false).unwrap();
        let docs = &f.shards[0];
        assert_eq!(docs[0], (0, 1.0));
        assert_eq!(docs[1], (2, 0.3));
        assert_eq!(docs[2], (1, 7.0000003E-4));
        assert_eq!(docs[3], (3, 7.0000003E-4));
        assert_eq!(f.collected, vec![4]);
    }

    #[test]
    fn rrf_scores_round_half_up() {
        assert_eq!(rrf_score(60, 0), 0.016393442);
        assert_eq!(rrf_score(60, 1), 0.016129032);
        let p = phase(Normalization::Rrf { rank_constant: 60 }, CombinationKind::Rrf, &[]);
        let lists = vec![vec![vec![(0, 0.29), (2, 0.29)], vec![(0, 0.5), (1, 0.33), (3, 0.33)]]];
        let f = fuse(&p, lists, true, None, false).unwrap();
        // OpenSearch 3.9.0: 0.032786883, then 0.016129032 twice, then 0.015873017.
        assert_eq!(f.shards[0][0], (0, 0.032786883));
        assert_eq!(f.shards[0][1].1, 0.016129032);
        assert_eq!(f.shards[0][3], (3, 0.015873017));
    }

    #[test]
    fn rrf_ranks_globally_across_shards() {
        let p = phase(Normalization::Rrf { rank_constant: 60 }, CombinationKind::Rrf, &[]);
        let lists = vec![vec![vec![(0, 0.5)]], vec![vec![(0, 0.9)]]];
        let f = fuse(&p, lists, false, None, false).unwrap();
        assert_eq!(f.shards[1][0].1, rrf_score(60, 0));
        assert_eq!(f.shards[0][0].1, rrf_score(60, 1));
    }

    #[test]
    fn combinations() {
        assert_eq!(combine(CombinationKind::ArithmeticMean, &[], &[1.0, 0.0]).unwrap(), 0.5);
        assert_eq!(combine(CombinationKind::GeometricMean, &[], &[1.0, 0.0]).unwrap(), 1.0);
        assert_eq!(combine(CombinationKind::HarmonicMean, &[], &[0.5, 0.25]).unwrap(), 2.0 / 6.0);
        assert_eq!(combine(CombinationKind::Rrf, &[], &[0.25, 0.5]).unwrap(), 0.75);
        let e = combine(CombinationKind::ArithmeticMean, &[0.5, 0.5], &[1.0]).unwrap_err();
        assert_eq!(e.reason(), "number of weights [2] must match number of sub-queries [1] in hybrid query");
    }

    #[test]
    fn bounds_clip_and_apply() {
        let clip = Bound { mode: BoundMode::Clip, score: 0.2, text: "0.2".into() };
        assert_eq!(min_max_score(0.1, 0.0, 1.0, Some(&clip), None), MIN_MAX_FLOOR);
        assert_eq!(min_max_score(0.6, 0.0, 1.0, Some(&clip), None), (0.6f32 - 0.2) / (1.0 - 0.2));
        let apply = Bound { mode: BoundMode::Apply, score: 2.0, text: "2.0".into() };
        assert_eq!(min_max_score(0.5, 0.0, 1.0, None, Some(&apply)), 0.25);
        assert_eq!(min_max_score(0.5, 0.5, 0.5, None, None), 1.0);
    }

    #[test]
    fn z_score_and_l2() {
        let mut shards = vec![vec![vec![(0, 3.0f32), (1, 1.0), (2, 2.0)]]];
        normalize(&Normalization::ZScore, &mut shards, true).unwrap();
        // mean 2, sample std 1: 3 -> 1.0, 1 -> the floor, 2 (the mean) -> the max.
        assert_eq!(shards[0][0], vec![(0, 1.0), (1, MIN_MAX_FLOOR), (2, 3.0)]);
        let mut shards = vec![vec![vec![(0, 3.0f32), (1, 4.0)]]];
        normalize(&Normalization::L2, &mut shards, true).unwrap();
        assert_eq!(shards[0][0], vec![(0, 0.6), (1, 0.8)]);
    }

    #[test]
    fn raw_layout_and_merge() {
        let lists = vec![vec![(5, 0.3), (7, 0.2)], vec![(9, 0.5)]];
        let r = raw(&lists);
        assert_eq!(r, vec![(5, MAGIC_START_STOP), (5, MAGIC_DELIMITER), (5, 0.3), (7, 0.2), (5, MAGIC_DELIMITER), (9, 0.5), (5, MAGIC_START_STOP)]);
        assert!(raw(&vec![vec![], vec![]]).is_empty());
        let merged = merge_lists(&[(1, vec![(1u32, 0.5f32), (2, 0.4)]), (0, vec![(3u32, 0.5f32)])], 3);
        assert_eq!(merged, vec![(1, 3, 0.5), (0, 1, 0.5), (0, 2, 0.4)]);
    }

    #[test]
    fn hash_order_of_doc_ids() {
        // Buckets of a 16-slot table: 18 -> 2, 5 -> 5.
        let keys = [5u32, 18];
        let order: Vec<u32> = int_hash_order(&keys).into_iter().map(|i| keys[i]).collect();
        assert_eq!(order, vec![18, 5]);
    }

    #[test]
    fn processor_configuration() {
        assert_eq!(combination("bogus", None).unwrap_err().reason(), "provided combination technique is not supported");
        let Some(Value::Obj(w)) = crate::json::parse(br#"{"weights":[0.5,0.6]}"#).unwrap() else { unreachable!() };
        assert_eq!(combination("arithmetic_mean", Some(&w)).unwrap_err().reason(), "sum of weights for combination must be equal to 1.0, submitted weights: [0.5, 0.6]");
        let Some(Value::Obj(w)) = crate::json::parse(br#"{"weights":[1,0]}"#).unwrap() else { unreachable!() };
        assert_eq!(combination("arithmetic_mean", Some(&w)).unwrap_err().kind, "class_cast_exception");
        assert_eq!(rank_constant(Some(&Value::from(0))).unwrap_err().reason(), "rank constant must be in the interval between 1 and 10000, submitted rank constant: 0");
        assert_eq!(rank_constant(Some(&Value::from("0x10"))).unwrap(), 16);
        assert_eq!(rank_constant(Some(&Value::from("abc"))).unwrap_err().reason(), "parameter [rank_constant] must be an integer");
        assert_eq!(
            check_compatible(&Normalization::ZScore, CombinationKind::HarmonicMean).unwrap_err().reason(),
            "provided combination technique harmonic_mean is not supported for normalization technique z_score. Supported techniques are: arithmetic_mean"
        );
        let Some(Value::Obj(b)) = crate::json::parse(br#"{"lower_bounds":[{"mode":"clip","min_score":20000}]}"#).unwrap() else { unreachable!() };
        assert_eq!(normalization("min_max", Some(&b)).unwrap_err().reason(), "min_score must be a valid finite number between -10000.000000 and 10000.000000");
        let Some(Value::Obj(b)) = crate::json::parse(br#"{"x":1}"#).unwrap() else { unreachable!() };
        assert_eq!(normalization("l2", Some(&b)).unwrap_err().reason(), "unrecognized parameters in normalization technique");
        assert_eq!(normalization("z_score", Some(&b)).unwrap(), Normalization::ZScore);
    }
}

//! `function_score` and `script_score` with Elasticsearch's semantics and explanations.
//!
//! A function-score query scores its inner query's matches, computes the functions (weight,
//! `field_value_factor`, decay `gauss`/`exp`/`linear` on numbers, dates and geo points,
//! `random_score`, `script_score`), each optionally under a filter, combines them with
//! `score_mode` (first, avg, max, sum, min, multiply), caps them at `max_boost`, combines with
//! the query score by `boost_mode` and drops docs under `min_score`. Arithmetic follows
//! Elasticsearch's (doubles, cast to float at the same points) so scores and explanation
//! values match. Scripts plug in through [`ScoreScript`] (implemented with Painless by
//! `search-es`).

use std::any::Any;
use std::sync::Arc;

use search_codec::{DocValuesReader, NO_MORE_DOCS};

use crate::error::{Error, Result};
use crate::query::nested::segment_bits;
use crate::schema::Schema;
use crate::search::{
    BATCH, BulkScorer, DefaultBulkScorer, DocScoreBuf, Explanation, LeafCollector, Query, QueryRef, QueryVisitor, Scorable,
    ScoreMode, Scorer, Searcher, SegmentContext, Weight, defer_error,
};
use crate::util::bitset::FixedBitSet;
use crate::util::geo;
use crate::util::java::{double_to_string, float_to_string};

/// A score script (Painless in `search-es`).
pub trait ScoreScript: Send + Sync {
    /// Whether the script reads `_score`.
    fn needs_score(&self) -> bool;
    /// An instance bound to a segment.
    fn leaf(&self, seg: &SegmentContext) -> Result<Box<dyn LeafScoreScript>>;
    /// The script's `toString` (`Script{type=inline, lang='painless', idOrCode='…', …}`).
    fn describe(&self) -> String;
}

/// A score script bound to a segment.
pub trait LeafScoreScript: Send {
    fn score(&mut self, doc: u32, sub_score: f32) -> Result<f64>;
    /// Scores a batch: `out[i] = score(docs[i], subs[i])` (`docs` ascending). On a failure,
    /// returns the index of the failing doc and its error; `out` before it is filled, after it
    /// unspecified. Scripts that can evaluate a block at once (doc values decoded per block,
    /// one dispatch per operation) override it; the default scores a doc at a time.
    fn score_block(&mut self, docs: &[u32], subs: &[f32], out: &mut [f64]) -> std::result::Result<(), (usize, Error)> {
        for (i, (&d, &sub)) in docs.iter().zip(subs).enumerate() {
            out[i] = self.score(d, sub).map_err(|e| (i, e))?;
        }
        Ok(())
    }
    /// A custom explanation description the script produced (`explanation.set(…)`).
    fn explanation(&mut self) -> Option<String> {
        None
    }
}

/// How numeric doc values decode to doubles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumDecode {
    Long,
    Double,
    Float,
    HalfFloat,
    /// `scaled_float` with its scaling factor.
    Scaled(f64),
    /// `unsigned_long` (stored with the sign bit flipped).
    UnsignedLong,
}

impl NumDecode {
    fn decode(self, v: i64) -> f64 {
        use search_codec::points::encode;
        match self {
            NumDecode::Long => v as f64,
            NumDecode::Double => encode::sortable_f64(v),
            NumDecode::Float => encode::sortable_f32(v as i32) as f64,
            NumDecode::HalfFloat => encode::sortable_f16(v as i16) as f64,
            // `ScaledFloatLeafFieldData`: times the inverse, which rounds unlike a division.
            NumDecode::Scaled(f) => v as f64 * (1.0 / f),
            NumDecode::UnsignedLong => ((v as u64) ^ (1u64 << 63)) as f64,
        }
    }
}

/// `field_value_factor` modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    None,
    Log,
    Log1p,
    Log2p,
    Ln,
    Ln1p,
    Ln2p,
    Square,
    Sqrt,
    Reciprocal,
}

impl Modifier {
    fn apply(self, n: f64) -> f64 {
        match self {
            Modifier::None => n,
            Modifier::Log => n.log10(),
            Modifier::Log1p => (n + 1.0).log10(),
            Modifier::Log2p => (n + 2.0).log10(),
            Modifier::Ln => n.ln(),
            Modifier::Ln1p => n.ln_1p(),
            Modifier::Ln2p => (n + 1.0).ln_1p(),
            Modifier::Square => n.powi(2),
            Modifier::Sqrt => n.sqrt(),
            Modifier::Reciprocal => 1.0 / n,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Modifier::None => "none",
            Modifier::Log => "log",
            Modifier::Log1p => "log1p",
            Modifier::Log2p => "log2p",
            Modifier::Ln => "ln",
            Modifier::Ln1p => "ln1p",
            Modifier::Ln2p => "ln2p",
            Modifier::Square => "square",
            Modifier::Sqrt => "sqrt",
            Modifier::Reciprocal => "reciprocal",
        }
    }
}

/// Decay shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecayFn {
    Gauss,
    Exp,
    Linear,
}

impl DecayFn {
    /// Elasticsearch's `processScale`.
    fn process_scale(self, scale: f64, decay: f64) -> f64 {
        match self {
            DecayFn::Gauss => 0.5 * scale.powi(2) / decay.ln(),
            DecayFn::Exp => decay.ln() / scale,
            DecayFn::Linear => scale / (1.0 - decay),
        }
    }

    fn evaluate(self, v: f64, s: f64) -> f64 {
        match self {
            DecayFn::Gauss => (0.5 * v.powi(2) / s).exp(),
            DecayFn::Exp => (s * v).exp(),
            DecayFn::Linear => 0f64.max((s - v) / s),
        }
    }

    fn explain(self, value_expl: &str, v: f64, s: f64) -> Explanation {
        let desc = match self {
            DecayFn::Gauss => format!("exp(-0.5*pow({value_expl},2.0)/{})", double_to_string(-s)),
            DecayFn::Exp => format!("exp(- {value_expl} * {})", double_to_string(-s)),
            DecayFn::Linear => format!("max(0.0, (({} - {value_expl})/{})", double_to_string(s), double_to_string(s)),
        };
        Explanation::leaf(self.evaluate(v, s) as f32, desc)
    }
}

/// `multi_value_mode` of decay functions (Elasticsearch's `MultiValueMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiValueMode {
    Min,
    Max,
    Avg,
    Sum,
    Median,
}

impl MultiValueMode {
    fn select(self, sorted: &[f64]) -> f64 {
        let n = sorted.len();
        match self {
            MultiValueMode::Min => sorted[0],
            MultiValueMode::Max => sorted[n - 1],
            MultiValueMode::Sum => sorted.iter().sum(),
            MultiValueMode::Avg => sorted.iter().sum::<f64>() / n as f64,
            MultiValueMode::Median => {
                if n.is_multiple_of(2) {
                    (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
                } else {
                    sorted[n / 2]
                }
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            MultiValueMode::Min => "MIN",
            MultiValueMode::Max => "MAX",
            MultiValueMode::Avg => "AVG",
            MultiValueMode::Sum => "SUM",
            MultiValueMode::Median => "MEDIAN",
        }
    }
}

/// The origin of a decay function.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DecayOrigin {
    /// Numbers and dates (millis).
    Number(f64, NumDecode),
    /// A geo point; distances in meters (`arc`).
    Geo { lat: f64, lon: f64 },
}

/// How `random_score` reads its field.
#[derive(Clone, Debug, PartialEq)]
pub enum RandomField {
    /// Keyword-like: hashes the first term.
    Bytes { field: u32, name: String },
    /// Numeric: hashes the decimal text of the first value (Elasticsearch's numeric field data
    /// as bytes).
    Number { field: u32, name: String, decode: NumDecode },
}

/// One score function.
#[derive(Clone)]
pub enum ScoreFunction {
    /// `weight` alone.
    Weight,
    FieldValueFactor { field: u32, name: String, factor: f32, modifier: Modifier, missing: Option<f64>, decode: NumDecode },
    Decay { field: u32, name: String, func: DecayFn, origin: DecayOrigin, scale: f64, decay: f64, offset: f64, mode: MultiValueMode },
    /// `random_score`: `seed` and `salt` (Elasticsearch salts with the shard id).
    Random { seed: i32, salt: i32, field: Option<RandomField> },
    Script(Arc<dyn ScoreScript>),
}

impl std::fmt::Debug for ScoreFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScoreFunction::Weight => write!(f, "Weight"),
            ScoreFunction::FieldValueFactor { name, .. } => write!(f, "FieldValueFactor({name})"),
            ScoreFunction::Decay { name, func, .. } => write!(f, "Decay({func:?} {name})"),
            ScoreFunction::Random { seed, .. } => write!(f, "Random({seed})"),
            ScoreFunction::Script(s) => write!(f, "Script({})", s.describe()),
        }
    }
}

/// A function with its optional filter and weight.
#[derive(Clone, Debug)]
pub struct FunctionEntry {
    pub filter: Option<QueryRef>,
    pub function: ScoreFunction,
    pub weight: Option<f32>,
}

impl FunctionEntry {
    /// The `toString` of the `ScoreFunction` Elasticsearch builds for the entry: a filter wraps
    /// it in a `FilterScoreFunction`, a weight in a `WeightFactorFunction`. Only script
    /// functions override `Object.toString`; the others print their class and identity hash,
    /// which differs by nature (a hash of the entry stands in for it).
    pub fn java_string(&self) -> String {
        const LUCENE: &str = "org.elasticsearch.common.lucene.search.function.";
        const DECAY: &str = "org.elasticsearch.index.query.functionscore.DecayFunctionBuilder$";
        let class = if self.filter.is_some() {
            format!("{LUCENE}FunctionScoreQuery$FilterScoreFunction")
        } else if self.weight.is_some() || matches!(self.function, ScoreFunction::Weight) {
            format!("{LUCENE}WeightFactorFunction")
        } else {
            match &self.function {
                ScoreFunction::Script(sc) => return format!("script{}", sc.describe()),
                ScoreFunction::FieldValueFactor { .. } => format!("{LUCENE}FieldValueFactorFunction"),
                ScoreFunction::Decay { origin: DecayOrigin::Geo { .. }, .. } => format!("{DECAY}GeoFieldDataScoreFunction"),
                ScoreFunction::Decay { .. } => format!("{DECAY}NumericFieldDataScoreFunction"),
                ScoreFunction::Random { .. } => format!("{LUCENE}RandomScoreFunction"),
                ScoreFunction::Weight => unreachable!("weight functions print as WeightFactorFunction"),
            }
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&format!("{:?}{:?}", self.function, self.weight), &mut h);
        format!("{class}@{:x}", std::hash::Hasher::finish(&h) as u32)
    }
}

/// `score_mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionScoreMode {
    First,
    Avg,
    Max,
    Sum,
    Min,
    Multiply,
}

/// `boost_mode` (Elasticsearch's `CombineFunction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CombineFunction {
    Multiply,
    Replace,
    Sum,
    Avg,
    Min,
    Max,
}

/// `Math.min(double, double)`: NaN if either is NaN, and -0.0 below 0.0.
fn java_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}

/// `Math.max(double, double)`: NaN if either is NaN, and 0.0 above -0.0.
fn java_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() { b } else { a }
    } else {
        a.max(b)
    }
}

impl CombineFunction {
    fn combine(self, q: f64, f: f64, max: f64) -> f32 {
        // Java's `Math.min`/`Math.max`: a NaN function value stays NaN (and fails the query
        // as an invalid score) instead of becoming `max_boost`.
        let f = java_min(f, max);
        match self {
            CombineFunction::Multiply => (q * f) as f32,
            CombineFunction::Replace => f as f32,
            CombineFunction::Sum => (q + f) as f32,
            CombineFunction::Avg => ((f + q) / 2.0) as f32,
            CombineFunction::Min => java_min(q, f) as f32,
            CombineFunction::Max => java_max(q, f) as f32,
        }
    }

    fn explain(self, q: Explanation, f: Explanation, max: f32) -> Explanation {
        let fv = f.value();
        let qv = q.value();
        let min_e = || Explanation::matched(fv.min(max), "min of:", vec![f.clone(), Explanation::leaf(max, "maxBoost")]);
        match self {
            CombineFunction::Multiply => {
                let m = min_e();
                Explanation::matched(qv * m.value(), "function score, product of:", vec![q, m])
            }
            CombineFunction::Replace => min_e(),
            CombineFunction::Sum => Explanation::matched(fv.min(max) + qv, "sum of", vec![q, min_e()]),
            CombineFunction::Avg => {
                Explanation::matched(((fv.min(max) + qv) as f64 / 2.0) as f32, "avg of", vec![q, min_e()])
            }
            CombineFunction::Min => Explanation::matched(fv.min(max).min(qv), "min of", vec![q, min_e()]),
            CombineFunction::Max => Explanation::matched(fv.min(max).max(qv), "max of:", vec![q, min_e()]),
        }
    }
}

/// Elasticsearch's `function_score` query.
#[derive(Clone, Debug)]
pub struct FunctionScoreQuery {
    pub query: QueryRef,
    pub functions: Vec<FunctionEntry>,
    pub score_mode: FunctionScoreMode,
    pub boost_mode: CombineFunction,
    pub max_boost: f32,
    pub min_score: Option<f32>,
}

impl FunctionScoreQuery {
    pub fn new(query: QueryRef, functions: Vec<FunctionEntry>) -> FunctionScoreQuery {
        FunctionScoreQuery {
            query,
            functions,
            score_mode: FunctionScoreMode::Multiply,
            boost_mode: CombineFunction::Multiply,
            max_boost: f32::MAX,
            min_score: None,
        }
    }

    fn needs_scores(&self) -> bool {
        self.boost_mode != CombineFunction::Replace
            || self.functions.iter().any(|f| matches!(&f.function, ScoreFunction::Script(s) if s.needs_score()))
    }
}

/// Values of one function in a segment.
enum LeafFn {
    Weight,
    Fvf { vals: Option<search_codec::SortedNumericDocValues>, num: Option<search_codec::NumericDocValues>, f: ScoreFunction },
    Decay {
        vals: Option<search_codec::SortedNumericDocValues>,
        num: Option<search_codec::NumericDocValues>,
        f: ScoreFunction,
        scale: f64,
        /// The geo origin's precomputed half of the distance formula.
        from: Option<geo::HaversinFrom>,
    },
    Random { seed: i32, base: u32, field: Option<(RandomField, Option<DocValuesReader>)> },
    Script(Box<dyn LeafScoreScript>),
}

fn mix32(mut k: i32) -> i32 {
    k = (k ^ ((k as u32) >> 16) as i32).wrapping_mul(0x85eb_ca6bu32 as i32);
    k = (k ^ ((k as u32) >> 13) as i32).wrapping_mul(0xc2b2_ae35u32 as i32);
    k ^ ((k as u32) >> 16) as i32
}

/// MurmurHash3 x86 32 (Lucene `StringHelper.murmurhash3_x86_32`).
pub fn murmur3_32(data: &[u8], seed: i32) -> i32 {
    let (c1, c2) = (0xcc9e_2d51u32, 0x1b87_3593u32);
    let mut h1 = seed as u32;
    let rounded = data.len() & !3;
    for i in (0..rounded).step_by(4) {
        let mut k1 = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        k1 = k1.wrapping_mul(c1).rotate_left(15).wrapping_mul(c2);
        h1 ^= k1;
        h1 = h1.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let mut k1 = 0u32;
    let tail = &data[rounded..];
    if tail.len() == 3 {
        k1 = (tail[2] as u32) << 16;
    }
    if tail.len() >= 2 {
        k1 |= (tail[1] as u32) << 8;
    }
    if !tail.is_empty() {
        k1 |= tail[0] as u32;
        k1 = k1.wrapping_mul(c1).rotate_left(15).wrapping_mul(c2);
        h1 ^= k1;
    }
    h1 ^= data.len() as u32;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85eb_ca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2_ae35);
    h1 ^= h1 >> 16;
    h1 as i32
}

/// The value of `doc` when the field has at most one per doc: `Some(None)` for none, `None`
/// when the field is multi-valued (then [`values_of`] reads them all). Saves the scratch
/// vector and the value iterator on every document of a scoring loop.
#[inline]
fn single_value(sn: &Option<search_codec::SortedNumericDocValues>, num: &Option<search_codec::NumericDocValues>, doc: u32) -> Option<Option<i64>> {
    match (sn, num) {
        (Some(v), _) => {
            let vals = v.singleton_values()?;
            Some(v.docs().rank(doc).map(|i| vals.get(i as u64)))
        }
        (None, Some(v)) => Some(v.get(doc)),
        (None, None) => Some(None),
    }
}

fn values_of(sn: &Option<search_codec::SortedNumericDocValues>, num: &Option<search_codec::NumericDocValues>, doc: u32, out: &mut Vec<i64>) {
    out.clear();
    if let Some(v) = sn {
        out.extend(v.values(doc));
    } else if let Some(v) = num
        && let Some(x) = v.get(doc)
    {
        out.push(x);
    }
}

impl LeafFn {
    fn new(f: &ScoreFunction, seg: &SegmentContext) -> Result<LeafFn> {
        let dv = |field: u32| -> (Option<search_codec::SortedNumericDocValues>, Option<search_codec::NumericDocValues>) {
            match seg.reader.doc_values(field) {
                Some(DocValuesReader::SortedNumeric(v)) => (Some(v.clone()), None),
                Some(DocValuesReader::Numeric(v)) => (None, Some(v.clone())),
                _ => (None, None),
            }
        };
        Ok(match f {
            ScoreFunction::Weight => LeafFn::Weight,
            ScoreFunction::FieldValueFactor { field, .. } => {
                let (vals, num) = dv(*field);
                LeafFn::Fvf { vals, num, f: f.clone() }
            }
            ScoreFunction::Decay { field, func, scale, decay, .. } => {
                let (vals, num) = dv(*field);
                let from = match f {
                    ScoreFunction::Decay { origin: DecayOrigin::Geo { lat, lon }, .. } => Some(geo::HaversinFrom::new(*lat, *lon)),
                    _ => None,
                };
                LeafFn::Decay { vals, num, f: f.clone(), scale: func.process_scale(*scale, *decay), from }
            }
            ScoreFunction::Random { seed, salt, field } => {
                let salted = mix32(seed ^ salt);
                let field = field.as_ref().map(|rf| {
                    let n = match rf {
                        RandomField::Bytes { field, .. } | RandomField::Number { field, .. } => *field,
                    };
                    (rf.clone(), seg.reader.doc_values(n).cloned())
                });
                LeafFn::Random { seed: salted, base: seg.base, field }
            }
            ScoreFunction::Script(s) => LeafFn::Script(s.leaf(seg)?),
        })
    }

    fn score(&mut self, doc: u32, sub: f32, scratch: &mut Vec<i64>) -> Result<f64> {
        match self {
            LeafFn::Weight => Ok(1.0),
            LeafFn::Fvf { vals, num, f } => {
                let ScoreFunction::FieldValueFactor { name, factor, modifier, missing, decode, .. } = f else { unreachable!() };
                let found = match single_value(vals, num, doc) {
                    Some(v) => v.map(|v| decode.decode(v)),
                    None => {
                        values_of(vals, num, doc, scratch);
                        scratch.iter().map(|&v| decode.decode(v)).reduce(f64::min)
                    }
                };
                let value = match found {
                    Some(v) => v,
                    None => match missing {
                        Some(m) => *m,
                        None => return Err(Error::illegal(format!("Missing value for field [{name}]"))),
                    },
                };
                let val = value * *factor as f64;
                let r = modifier.apply(val);
                if r < 0.0 {
                    let mut msg = format!(
                        "field value function must not produce negative scores, but got: [{}] for field value: [{}]",
                        double_to_string(r),
                        double_to_string(value)
                    );
                    if *modifier == Modifier::Ln {
                        msg.push_str("; consider using ln1p or ln2p instead of ln to avoid negative scores");
                    } else if *modifier == Modifier::Log {
                        msg.push_str("; consider using log1p or log2p instead of log to avoid negative scores");
                    }
                    return Err(Error::illegal(msg));
                }
                Ok(r)
            }
            LeafFn::Decay { vals, num, f, scale, from } => {
                let d = decay_distance(f, from.as_ref(), vals, num, doc, scratch);
                let ScoreFunction::Decay { func, .. } = f else { unreachable!() };
                Ok(func.evaluate(d, *scale))
            }
            LeafFn::Random { seed, base, field } => {
                let hash = match field {
                    None => mix32((*base + doc) as i32 ^ *seed),
                    Some((rf, dv)) => match random_bytes(rf, dv.as_ref(), doc) {
                        Some(b) => murmur3_32(&b, *seed),
                        None => *seed,
                    },
                };
                Ok(((hash & 0x00FF_FFFF) as f32 / (1u32 << 24) as f32) as f64)
            }
            LeafFn::Script(s) => s.score(doc, sub),
        }
    }

    /// [`score`](Self::score) for a block of docs: `out[i]` for every doc `need` selects (all
    /// without a mask); a doc whose function fails gets `bad[i]` (its exact error comes from
    /// [`score`](Self::score) if the doc's score is ever asked for). The same arithmetic as
    /// `score`, with the doc values of the block decoded in one batch.
    fn score_block(
        &mut self,
        docs: &[u32],
        subs: &[f32],
        need: Option<&[bool]>,
        out: &mut [f64],
        bad: &mut [bool],
        b: &mut BlockScratch,
    ) {
        let needed = |i: usize| need.is_none_or(|m| m[i]);
        match self {
            LeafFn::Weight => out.fill(1.0),
            LeafFn::Fvf { vals, num, f } => {
                let ScoreFunction::FieldValueFactor { factor, modifier, missing, decode, .. } = f else { unreachable!() };
                let (factor, modifier, missing, decode) = (*factor as f64, *modifier, *missing, *decode);
                if b.fetch_single(vals, num, docs) {
                    let n = docs.len();
                    decode_block(decode, &b.single[..n], out);
                    for i in 0..n {
                        if !b.has[i] {
                            match missing {
                                Some(m) => out[i] = m,
                                None => {
                                    bad[i] |= needed(i);
                                    out[i] = 0.0;
                                }
                            }
                        }
                    }
                    modify_block(modifier, factor, out);
                    for i in 0..n {
                        if out[i] < 0.0 {
                            bad[i] |= needed(i);
                        }
                    }
                    return;
                }
                b.fetch(vals, num, docs);
                let mut c = BlockCursor::default();
                for (i, &d) in docs.iter().enumerate() {
                    let found = c.values(b, d).iter().map(|&v| decode.decode(v)).reduce(f64::min);
                    let value = match found.or(missing) {
                        Some(v) => v,
                        None => {
                            bad[i] |= needed(i);
                            continue;
                        }
                    };
                    let r = modifier.apply(value * factor);
                    if r < 0.0 {
                        bad[i] |= needed(i);
                    }
                    out[i] = r;
                }
            }
            LeafFn::Decay { vals, num, f, scale, from } => {
                let ScoreFunction::Decay { func, origin, offset, mode, .. } = f else { unreachable!() };
                let (func, offset, mode, scale, from) = (*func, *offset, *mode, *scale, from.as_ref());
                if b.fetch_single(vals, num, docs) {
                    let n = docs.len();
                    let (raw, has) = (&b.single[..n], &b.has[..n]);
                    match (&*origin, from) {
                        (DecayOrigin::Geo { .. }, Some(h)) => {
                            for i in 0..n {
                                out[i] = if has[i] {
                                    let (a, bb) = geo::decode_geo_dv(raw[i]);
                                    0f64.max(h.meters(a, bb) - offset)
                                } else {
                                    0.0
                                };
                            }
                        }
                        _ => {
                            for i in 0..n {
                                out[i] = if has[i] { raw_distance(origin, from, offset, raw[i]) } else { 0.0 };
                            }
                        }
                    }
                    match func {
                        DecayFn::Gauss => out.iter_mut().for_each(|v| *v = DecayFn::Gauss.evaluate(*v, scale)),
                        DecayFn::Exp => out.iter_mut().for_each(|v| *v = DecayFn::Exp.evaluate(*v, scale)),
                        DecayFn::Linear => out.iter_mut().for_each(|v| *v = DecayFn::Linear.evaluate(*v, scale)),
                    }
                    return;
                }
                b.fetch(vals, num, docs);
                let mut c = BlockCursor::default();
                for (i, &d) in docs.iter().enumerate() {
                    let dist = match c.values(b, d) {
                        [] => 0.0,
                        [v] => raw_distance(origin, from, offset, *v),
                        vs => select_distance(mode, vs.iter().map(|&v| raw_distance(origin, from, offset, v))),
                    };
                    out[i] = func.evaluate(dist, scale);
                }
            }
            LeafFn::Random { .. } => {
                for (i, (&d, &sub)) in docs.iter().zip(subs).enumerate() {
                    if needed(i) {
                        match self.score(d, sub, &mut b.one) {
                            Ok(v) => out[i] = v,
                            Err(_) => bad[i] = true,
                        }
                    }
                }
            }
            LeafFn::Script(s) => match need {
                None => {
                    let mut start = 0;
                    while start < docs.len() {
                        match s.score_block(&docs[start..], &subs[start..], &mut out[start..]) {
                            Ok(()) => break,
                            Err((k, _)) => {
                                bad[start + k] = true;
                                start += k + 1;
                            }
                        }
                    }
                }
                Some(m) => {
                    for (i, (&d, &sub)) in docs.iter().zip(subs).enumerate() {
                        if m[i] {
                            match s.score(d, sub) {
                                Ok(v) => out[i] = v,
                                Err(_) => bad[i] = true,
                            }
                        }
                    }
                }
            },
        }
    }

    fn explain(&mut self, doc: u32, sub: &Explanation, scratch: &mut Vec<i64>) -> Result<Explanation> {
        Ok(match self {
            LeafFn::Weight => Explanation::leaf(1.0f32, "constant score 1.0 - no function provided"),
            LeafFn::Fvf { f, .. } => {
                let ScoreFunction::FieldValueFactor { name, factor, modifier, missing, .. } = f.clone() else { unreachable!() };
                let s = self.score(doc, sub.value(), scratch)?;
                let def = missing.map_or(String::new(), |m| format!("?:{}", double_to_string(m)));
                Explanation::leaf(
                    s as f32,
                    format!("field value function: {}(doc['{name}'].value{def} * factor={})", modifier.name(), float_to_string(factor)),
                )
            }
            LeafFn::Decay { vals, num, f, scale, from } => {
                let d = decay_distance(f, from.as_ref(), vals, num, doc, scratch);
                let ScoreFunction::Decay { func, name, origin, offset, mode, .. } = f.clone() else { unreachable!() };
                values_of(vals, num, doc, scratch);
                let mut ds = String::from(mode.name());
                match origin {
                    DecayOrigin::Number(o, dec) => {
                        ds.push('[');
                        if scratch.is_empty() {
                            ds.push_str("0.0");
                        }
                        for (i, &v) in scratch.iter().enumerate() {
                            ds.push_str(&format!(
                                "Math.max(Math.abs({}(=doc value) - {}(=origin))) - {}(=offset), 0)",
                                double_to_string(dec.decode(v)),
                                double_to_string(o),
                                double_to_string(offset)
                            ));
                            if i + 1 != scratch.len() {
                                ds.push_str(", ");
                            }
                        }
                    }
                    DecayOrigin::Geo { lat, lon } => {
                        ds.push_str(" of: [");
                        if scratch.is_empty() {
                            ds.push_str("0.0");
                        }
                        for (i, &v) in scratch.iter().enumerate() {
                            let (a, b) = geo::decode_geo_dv(v);
                            ds.push_str(&format!(
                                "Math.max(arcDistance({}, {}(=doc value),{}, {}(=origin)) - {}(=offset), 0)",
                                double_to_string(a),
                                double_to_string(b),
                                double_to_string(lat),
                                double_to_string(lon),
                                double_to_string(offset)
                            ));
                            if i + 1 != scratch.len() {
                                ds.push_str(", ");
                            }
                        }
                    }
                }
                ds.push(']');
                Explanation::matched(
                    func.evaluate(d, *scale) as f32,
                    format!("Function for field {name}:"),
                    vec![func.explain(&ds, d, *scale)],
                )
            }
            LeafFn::Random { .. } => {
                let s = self.score(doc, sub.value(), scratch)?;
                let LeafFn::Random { .. } = self else { unreachable!() };
                Explanation::leaf(s as f32, "random score function".to_string())
            }
            LeafFn::Script(s) => {
                let v = s.score(doc, sub.value())?;
                let se = Explanation::matched(sub.value, "_score: ", vec![sub.clone()]);
                match s.explanation() {
                    Some(d) => Explanation::matched(v as f32, d, vec![se]),
                    None => Explanation::matched(v as f32, "script score function, computed with script:".to_string(), vec![se]),
                }
            }
        })
    }
}

/// Reused buffers of block scoring: the doc values of one block of docs, flattened.
#[derive(Default)]
struct BlockScratch {
    found: Vec<u32>,
    counts: Vec<u32>,
    values: Vec<i64>,
    /// Single-valued fields: the value of each doc of the block, if `has` it.
    single: Vec<i64>,
    has: Vec<bool>,
    /// Scratch of the per-doc path ([`LeafFn::score`]).
    one: Vec<i64>,
}

impl BlockScratch {
    /// For a field with at most one value per doc, fills `single` / `has` for `docs`
    /// (ascending) and returns true; false (nothing filled) for multi-valued fields. A dense
    /// field read over a run of consecutive docs (`match_all`) decodes the run sequentially.
    fn fetch_single(&mut self, sn: &Option<search_codec::SortedNumericDocValues>, num: &Option<search_codec::NumericDocValues>, docs: &[u32]) -> bool {
        let n = docs.len();
        let (set, vals) = match (sn, num) {
            (Some(v), _) => match v.singleton_values() {
                Some(vals) => (v.docs(), vals),
                None => return false,
            },
            (None, Some(v)) => (v.docs(), v.values()),
            (None, None) => {
                self.has.clear();
                self.has.resize(n, false);
                self.single.clear();
                self.single.resize(n, 0);
                return true;
            }
        };
        self.single.clear();
        self.single.resize(n, 0);
        self.has.clear();
        if n > 0 && set.is_all() && docs[n - 1] < set.max_doc() && docs[n - 1] - docs[0] == (n - 1) as u32 {
            vals.get_range(docs[0] as u64, &mut self.single);
            self.has.resize(n, true);
            return true;
        }
        self.has.resize(n, false);
        self.found.clear();
        self.counts.clear();
        self.values.clear();
        match (sn, num) {
            (Some(v), _) => v.get_batch(docs, &mut self.found, &mut self.counts, &mut self.values),
            (None, Some(v)) => v.get_batch(docs, &mut self.found, &mut self.values),
            (None, None) => {}
        }
        let mut j = 0;
        for (i, &d) in docs.iter().enumerate() {
            if self.found.get(j) == Some(&d) {
                self.has[i] = true;
                self.single[i] = self.values[j];
                j += 1;
            }
        }
        true
    }

    /// Decodes the values of `docs` (ascending) in one batch; read them back in doc order
    /// with a [`BlockCursor`].
    fn fetch(&mut self, sn: &Option<search_codec::SortedNumericDocValues>, num: &Option<search_codec::NumericDocValues>, docs: &[u32]) {
        self.found.clear();
        self.counts.clear();
        self.values.clear();
        match (sn, num) {
            (Some(v), _) => v.get_batch(docs, &mut self.found, &mut self.counts, &mut self.values),
            (None, Some(v)) => {
                v.get_batch(docs, &mut self.found, &mut self.values);
                self.counts.resize(self.found.len(), 1);
            }
            (None, None) => {}
        }
    }
}

/// `out[i] = d.decode(raw[i])`, the decoding chosen once per block.
fn decode_block(d: NumDecode, raw: &[i64], out: &mut [f64]) {
    match d {
        NumDecode::Long => out.iter_mut().zip(raw).for_each(|(o, &v)| *o = NumDecode::Long.decode(v)),
        NumDecode::Double => out.iter_mut().zip(raw).for_each(|(o, &v)| *o = NumDecode::Double.decode(v)),
        _ => out.iter_mut().zip(raw).for_each(|(o, &v)| *o = d.decode(v)),
    }
}

/// `x[i] = m.apply(x[i] * factor)`, the modifier chosen once per block. Logarithms reuse the
/// previous doc's result for an equal input (fields like `population` repeat values a lot).
fn modify_block(m: Modifier, factor: f64, x: &mut [f64]) {
    match m {
        Modifier::None => x.iter_mut().for_each(|v| *v = Modifier::None.apply(*v * factor)),
        Modifier::Square => x.iter_mut().for_each(|v| *v = Modifier::Square.apply(*v * factor)),
        Modifier::Sqrt => x.iter_mut().for_each(|v| *v = Modifier::Sqrt.apply(*v * factor)),
        Modifier::Reciprocal => x.iter_mut().for_each(|v| *v = Modifier::Reciprocal.apply(*v * factor)),
        _ => {
            let mut last: Option<(u64, f64)> = None;
            for v in x.iter_mut() {
                let a = *v * factor;
                *v = match last {
                    Some((bits, r)) if bits == a.to_bits() => r,
                    _ => {
                        let r = m.apply(a);
                        last = Some((a.to_bits(), r));
                        r
                    }
                };
            }
        }
    }
}

/// Walks a [`BlockScratch::fetch`] result in doc order.
#[derive(Default)]
struct BlockCursor {
    next: usize,
    pos: usize,
}

impl BlockCursor {
    /// The values (ascending) of `doc`, the next doc of the fetched block.
    #[inline]
    fn values<'a>(&mut self, b: &'a BlockScratch, doc: u32) -> &'a [i64] {
        if b.found.get(self.next) != Some(&doc) {
            return &[];
        }
        let n = b.counts[self.next] as usize;
        self.next += 1;
        let start = self.pos;
        self.pos += n;
        &b.values[start..start + n]
    }
}

/// The per-block state of [`FsBulkScorer`].
struct FsBlock {
    subs: [f32; BATCH],
    vals: [f64; BATCH],
    factor: [f64; BATCH],
    acc: [f64; BATCH],
    wsum: [f64; BATCH],
    scores: [f32; BATCH],
    bad: [bool; BATCH],
    need: [bool; BATCH],
    done: [bool; BATCH],
}

/// A score handed to collectors by block scorers, recording the minimum competitive score
/// they announce (docs below it are not collected at all).
struct BlockScore {
    score: f32,
    min: f32,
}

impl Scorable for BlockScore {
    #[inline]
    fn score(&mut self) -> f32 {
        self.score
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        if m > self.min {
            self.min = m;
        }
    }
}

/// A score computed only when a collector asks for it (block scorers hand docs whose block
/// computation failed to collectors this way, so the per-doc path raises the exact error at
/// the same point it would without blocks).
struct LazyScore<F: FnMut() -> f32> {
    f: F,
    cur: Option<f32>,
}

impl<F: FnMut() -> f32> Scorable for LazyScore<F> {
    fn score(&mut self) -> f32 {
        if let Some(c) = self.cur {
            return c;
        }
        let v = (self.f)();
        self.cur = Some(v);
        v
    }
}

fn random_bytes(rf: &RandomField, dv: Option<&DocValuesReader>, doc: u32) -> Option<Vec<u8>> {
    match (rf, dv?) {
        (RandomField::Bytes { .. }, DocValuesReader::Sorted(v)) => {
            let o = v.ord(doc)?;
            let mut b = Vec::new();
            v.lookup_ord(o, &mut b);
            Some(b)
        }
        (RandomField::Bytes { .. }, DocValuesReader::SortedSet(v)) => {
            let o = v.ords(doc).next()?;
            let mut b = Vec::new();
            v.lookup_ord(o, &mut b);
            Some(b)
        }
        (RandomField::Bytes { .. }, DocValuesReader::Binary(v)) => v.get(doc).map(|b| b.to_vec()),
        (RandomField::Number { decode, .. }, DocValuesReader::SortedNumeric(v)) => {
            let x = v.values(doc).next()?;
            Some(num_text(*decode, x).into_bytes())
        }
        (RandomField::Number { decode, .. }, DocValuesReader::Numeric(v)) => {
            let x = v.get(doc)?;
            Some(num_text(*decode, x).into_bytes())
        }
        _ => None,
    }
}

fn num_text(d: NumDecode, v: i64) -> String {
    match d {
        NumDecode::Long => v.to_string(),
        NumDecode::UnsignedLong => ((v as u64) ^ (1 << 63)).to_string(),
        _ => double_to_string(d.decode(v)),
    }
}

/// The selected distance of a doc (0 without values, as Elasticsearch's `replaceMissing`).
fn decay_distance(
    f: &ScoreFunction,
    from: Option<&geo::HaversinFrom>,
    vals: &Option<search_codec::SortedNumericDocValues>,
    num: &Option<search_codec::NumericDocValues>,
    doc: u32,
    scratch: &mut Vec<i64>,
) -> f64 {
    let ScoreFunction::Decay { origin, offset, mode, .. } = f else { return 0.0 };
    let distance = |v: i64| raw_distance(origin, from, *offset, v);
    // One value: every mode selects it.
    if let Some(v) = single_value(vals, num, doc) {
        return v.map_or(0.0, distance);
    }
    values_of(vals, num, doc, scratch);
    select_distance(*mode, scratch.iter().map(|&v| distance(v)))
}

/// The distance of one doc value from a decay origin, less the offset (at least 0).
#[inline]
fn raw_distance(origin: &DecayOrigin, from: Option<&geo::HaversinFrom>, offset: f64, v: i64) -> f64 {
    match origin {
        DecayOrigin::Number(o, dec) => 0f64.max((dec.decode(v) - o).abs() - offset),
        DecayOrigin::Geo { lat, lon } => {
            let (a, b) = geo::decode_geo_dv(v);
            let m = match from {
                Some(h) => h.meters(a, b),
                None => geo::haversin_meters(*lat, *lon, a, b),
            };
            0f64.max(m - offset)
        }
    }
}

/// `mode` over the distances of a multi-valued doc (0 without any).
fn select_distance(mode: MultiValueMode, ds: impl Iterator<Item = f64>) -> f64 {
    let mut ds: smallvec::SmallVec<[f64; 4]> = ds.collect();
    if ds.is_empty() {
        return 0.0;
    }
    ds.sort_by(|a, b| a.total_cmp(b));
    mode.select(&ds)
}

struct FsWeight {
    sub: Box<dyn Weight>,
    q: FunctionScoreQuery,
    filters: Vec<Option<QueryRef>>,
    filter_descs: Vec<String>,
    searcher: Searcher,
    needs_scores: bool,
}

struct FsScorer {
    inner: Box<dyn Scorer>,
    fns: Vec<LeafFn>,
    sets: Vec<Option<Arc<FixedBitSet>>>,
    weights: Vec<f32>,
    mode: FunctionScoreMode,
    combine: CombineFunction,
    max_boost: f32,
    min_score: Option<f32>,
    needs_scores: bool,
    cur: f32,
    scratch: Vec<i64>,
    failed: bool,
    /// Whether matching needs a second phase (`min_score`, or the inner query's); without
    /// one, iteration is the inner scorer's alone.
    two_phase: bool,
}

impl FsScorer {
    fn factor(&mut self, doc: u32, sub: f32) -> Result<f64> {
        let mut factor = 1f64;
        let n = self.fns.len();
        let applies = |i: usize, sets: &[Option<Arc<FixedBitSet>>]| sets[i].as_ref().is_none_or(|s| s.get(doc));
        // One function without a filter: its score is the factor, whatever `score_mode`
        // (Elasticsearch builds a single-function query then).
        let mode = if n == 1 && self.sets[0].is_none() { FunctionScoreMode::First } else { self.mode };
        match mode {
            FunctionScoreMode::First => {
                for i in 0..n {
                    if applies(i, &self.sets) {
                        factor = self.fns[i].score(doc, sub, &mut self.scratch)? * self.weights[i] as f64;
                        break;
                    }
                }
            }
            FunctionScoreMode::Max => {
                let mut m = f64::NEG_INFINITY;
                for i in 0..n {
                    if applies(i, &self.sets) {
                        m = java_max(m, self.fns[i].score(doc, sub, &mut self.scratch)? * self.weights[i] as f64);
                    }
                }
                if m != f64::NEG_INFINITY {
                    factor = m;
                }
            }
            FunctionScoreMode::Min => {
                let mut m = f64::INFINITY;
                for i in 0..n {
                    if applies(i, &self.sets) {
                        m = java_min(m, self.fns[i].score(doc, sub, &mut self.scratch)? * self.weights[i] as f64);
                    }
                }
                if m != f64::INFINITY {
                    factor = m;
                }
            }
            FunctionScoreMode::Multiply => {
                for i in 0..n {
                    if applies(i, &self.sets) {
                        factor *= self.fns[i].score(doc, sub, &mut self.scratch)? * self.weights[i] as f64;
                    }
                }
            }
            FunctionScoreMode::Avg | FunctionScoreMode::Sum => {
                let mut total = 0f64;
                let mut wsum = 0f64;
                for i in 0..n {
                    if applies(i, &self.sets) {
                        total += self.fns[i].score(doc, sub, &mut self.scratch)? * self.weights[i] as f64;
                        wsum += self.weights[i] as f64;
                    }
                }
                if wsum != 0.0 {
                    factor = total;
                    if self.mode == FunctionScoreMode::Avg {
                        factor /= wsum;
                    }
                }
            }
        }
        Ok(factor)
    }

    fn compute(&mut self) -> Result<f32> {
        let doc = self.inner.doc();
        let sub = if self.needs_scores { self.inner.score() } else { 0.0 };
        self.compute_at(doc, sub)
    }

    fn compute_at(&mut self, doc: u32, sub: f32) -> Result<f32> {
        if self.fns.is_empty() {
            return Ok(sub);
        }
        let f = self.factor(doc, sub)?;
        let s = self.combine.combine(sub as f64, f, self.max_boost as f64);
        if s < 0.0 || s.is_nan() {
            return Err(Error::illegal(format!(
                "function score query returned an invalid score: {} for doc: {doc}; score must be a non-negative real number",
                float_to_string(s)
            )));
        }
        Ok(s)
    }

    /// [`compute_at`](Self::compute_at) for the docs of a block, their query scores in
    /// `blk.subs`: final scores in `blk.scores`, `blk.bad` for docs whose computation fails
    /// (scored again a doc at a time if asked, for the exact error). Functions run a block at
    /// a time and combine in the per-doc order of operations, so scores are bit-identical.
    fn score_block(&mut self, docs: &[u32], blk: &mut FsBlock, scratch: &mut BlockScratch) {
        let n = docs.len();
        let nf = self.fns.len();
        let mode = if nf == 1 && self.sets[0].is_none() { FunctionScoreMode::First } else { self.mode };
        let FsBlock { subs, vals, factor, acc, wsum, scores, bad, need, done } = blk;
        let (subs, vals, factor, acc, wsum, scores, bad, need, done) = (
            &subs[..n],
            &mut vals[..n],
            &mut factor[..n],
            &mut acc[..n],
            &mut wsum[..n],
            &mut scores[..n],
            &mut bad[..n],
            &mut need[..n],
            &mut done[..n],
        );
        bad.fill(false);
        factor.fill(1.0);
        match mode {
            FunctionScoreMode::First => done.fill(false),
            FunctionScoreMode::Max => acc.fill(f64::NEG_INFINITY),
            FunctionScoreMode::Min => acc.fill(f64::INFINITY),
            FunctionScoreMode::Multiply => {}
            FunctionScoreMode::Avg | FunctionScoreMode::Sum => {
                acc.fill(0.0);
                wsum.fill(0.0);
            }
        }
        for i in 0..nf {
            let set = self.sets[i].as_deref();
            let all = set.is_none() && (mode != FunctionScoreMode::First || i == 0);
            if !all {
                for (k, &d) in docs.iter().enumerate() {
                    need[k] = set.is_none_or(|s| s.get(d)) && !(mode == FunctionScoreMode::First && done[k]);
                }
            }
            let mask = if all { None } else { Some(&need[..]) };
            self.fns[i].score_block(docs, subs, mask, vals, bad, scratch);
            let w = self.weights[i] as f64;
            if all && mode == FunctionScoreMode::First {
                // The lone (or first unfiltered) function: its value is the factor.
                for k in 0..n {
                    factor[k] = vals[k] * w;
                    done[k] = true;
                }
                continue;
            }
            for k in 0..n {
                if !all && !need[k] {
                    continue;
                }
                let v = vals[k] * w;
                match mode {
                    FunctionScoreMode::First => {
                        factor[k] = v;
                        done[k] = true;
                    }
                    FunctionScoreMode::Max => acc[k] = java_max(acc[k], v),
                    FunctionScoreMode::Min => acc[k] = java_min(acc[k], v),
                    FunctionScoreMode::Multiply => factor[k] *= v,
                    FunctionScoreMode::Avg | FunctionScoreMode::Sum => {
                        acc[k] += v;
                        wsum[k] += w;
                    }
                }
            }
        }
        let max_boost = self.max_boost as f64;
        if !matches!(mode, FunctionScoreMode::First | FunctionScoreMode::Multiply) {
            for k in 0..n {
                match mode {
                    FunctionScoreMode::Max if acc[k] != f64::NEG_INFINITY => factor[k] = acc[k],
                    FunctionScoreMode::Min if acc[k] != f64::INFINITY => factor[k] = acc[k],
                    FunctionScoreMode::Avg | FunctionScoreMode::Sum if wsum[k] != 0.0 => {
                        factor[k] = acc[k];
                        if self.mode == FunctionScoreMode::Avg {
                            factor[k] /= wsum[k];
                        }
                    }
                    _ => {}
                }
            }
        }
        // `combine` with the boost mode chosen once per block.
        let finish = |scores: &mut [f32], bad: &mut [bool], c: CombineFunction| {
            for k in 0..n {
                let s = c.combine(subs[k] as f64, factor[k], max_boost);
                if s < 0.0 || s.is_nan() {
                    bad[k] = true;
                }
                scores[k] = s;
            }
        };
        match self.combine {
            CombineFunction::Multiply => finish(scores, bad, CombineFunction::Multiply),
            CombineFunction::Replace => finish(scores, bad, CombineFunction::Replace),
            CombineFunction::Sum => finish(scores, bad, CombineFunction::Sum),
            c => finish(scores, bad, c),
        }
    }

    fn check(&mut self) -> bool {
        match self.compute() {
            Ok(s) => {
                self.cur = s;
                self.min_score.is_none_or(|m| s >= m)
            }
            Err(e) => {
                defer_error(e);
                self.failed = true;
                false
            }
        }
    }
}

impl Scorable for FsScorer {
    fn score(&mut self) -> f32 {
        if self.min_score.is_none() {
            match self.compute() {
                Ok(s) => self.cur = s,
                Err(e) => {
                    defer_error(e);
                    self.failed = true;
                    self.cur = 0.0;
                }
            }
        }
        self.cur
    }
}

impl Scorer for FsScorer {
    fn doc(&self) -> u32 {
        if self.failed { NO_MORE_DOCS } else { self.inner.doc() }
    }
    fn next(&mut self) -> u32 {
        if !self.two_phase {
            return if self.failed { NO_MORE_DOCS } else { self.inner.next() };
        }
        loop {
            let d = self.approx_next();
            if d == NO_MORE_DOCS || self.matches() {
                return if self.failed { NO_MORE_DOCS } else { d };
            }
        }
    }
    fn advance(&mut self, t: u32) -> u32 {
        if !self.two_phase {
            return if self.failed { NO_MORE_DOCS } else { self.inner.advance(t) };
        }
        let d = self.approx_advance(t);
        if d == NO_MORE_DOCS || self.matches() {
            return if self.failed { NO_MORE_DOCS } else { d };
        }
        self.next()
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
    fn has_two_phase(&self) -> bool {
        self.two_phase
    }
    fn approx_next(&mut self) -> u32 {
        if self.failed {
            return NO_MORE_DOCS;
        }
        self.inner.approx_next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.failed {
            return NO_MORE_DOCS;
        }
        self.inner.approx_advance(t)
    }
    fn matches(&mut self) -> bool {
        if self.inner.has_two_phase() && !self.inner.matches() {
            return false;
        }
        if self.min_score.is_some() {
            return self.check();
        }
        true
    }
    fn match_cost(&self) -> f32 {
        self.inner.match_cost() + if self.min_score.is_some() { 1000.0 } else { 0.0 }
    }
}

/// Scores the inner query's matches a block at a time (Lucene scores function queries a doc
/// at a time): the inner scorer fills a batch of docs and scores, each function decodes the
/// doc values of the whole batch and evaluates them in a tight loop, and collectors get the
/// docs with their final scores. Used when matching is the inner query's alone (no
/// `min_score`, no two-phase inner query).
struct FsBulkScorer {
    sc: FsScorer,
    buf: Box<DocScoreBuf>,
    blk: Box<FsBlock>,
    scratch: BlockScratch,
    /// The minimum competitive score the collector announced.
    min: f32,
}

impl BulkScorer for FsBulkScorer {
    fn score(&mut self, collector: &mut dyn LeafCollector, live: Option<&search_codec::LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let s = &mut self.sc;
        let mut d = s.inner.doc();
        if d == u32::MAX || d < min {
            d = s.inner.advance(min);
        }
        if collector.skips() {
            return crate::search::score_skipping(s, collector, live, d, max);
        }
        let mut b = BlockScore { score: 0.0, min: self.min };
        loop {
            if s.failed {
                return Ok(NO_MORE_DOCS);
            }
            s.inner.next_docs_and_scores(max, live, &mut self.buf);
            let n = self.buf.len;
            if n == 0 {
                break;
            }
            let docs = &self.buf.docs[..n];
            if s.needs_scores {
                self.blk.subs[..n].copy_from_slice(&self.buf.scores[..n]);
            } else {
                self.blk.subs[..n].fill(0.0);
            }
            s.score_block(docs, &mut self.blk, &mut self.scratch);
            for (i, &doc) in docs.iter().enumerate() {
                if self.blk.bad[i] {
                    let sub = self.blk.subs[i];
                    let mut l = LazyScore {
                        f: || match s.compute_at(doc, sub) {
                            Ok(v) => v,
                            Err(e) => {
                                defer_error(e);
                                s.failed = true;
                                0.0
                            }
                        },
                        cur: None,
                    };
                    collector.collect(doc, &mut l)?;
                    if s.failed {
                        self.min = b.min;
                        return Ok(NO_MORE_DOCS);
                    }
                } else {
                    let score = self.blk.scores[i];
                    if b.min <= 0.0 || score >= b.min {
                        b.score = score;
                        collector.collect(doc, &mut b)?;
                    }
                }
            }
        }
        self.min = b.min;
        Ok(s.inner.doc())
    }

    fn cost(&self) -> u64 {
        self.sc.inner.cost()
    }
}

impl FsWeight {
    fn fs_scorer(&self, seg: &SegmentContext) -> Result<Option<FsScorer>> {
        let Some(inner) = self.sub.scorer(seg)? else { return Ok(None) };
        let mut fns = Vec::with_capacity(self.q.functions.len());
        let mut sets = Vec::with_capacity(self.q.functions.len());
        let mut weights = Vec::with_capacity(self.q.functions.len());
        for (i, f) in self.q.functions.iter().enumerate() {
            fns.push(LeafFn::new(&f.function, seg)?);
            sets.push(match &self.filters[i] {
                Some(q) => Some(segment_bits(&self.searcher, seg, q)?),
                None => None,
            });
            weights.push(f.weight.unwrap_or(1.0));
        }
        let two_phase = self.q.min_score.is_some() || inner.has_two_phase();
        Ok(Some(FsScorer {
            two_phase,
            inner,
            fns,
            sets,
            weights,
            mode: self.q.score_mode,
            combine: self.q.boost_mode,
            max_boost: self.q.max_boost,
            min_score: self.q.min_score,
            needs_scores: self.needs_scores,
            cur: 0.0,
            scratch: Vec::new(),
            failed: false,
        }))
    }
}

impl Weight for FsWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.fs_scorer(seg)?.map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        let Some(sc) = self.fs_scorer(seg)? else { return Ok(None) };
        if sc.two_phase || sc.fns.is_empty() {
            return Ok(Some(Box::new(DefaultBulkScorer::new(Box::new(sc)))));
        }
        Ok(Some(Box::new(FsBulkScorer {
            sc,
            buf: DocScoreBuf::new(),
            blk: Box::new(FsBlock {
                subs: [0.0; BATCH],
                vals: [0.0; BATCH],
                factor: [0.0; BATCH],
                acc: [0.0; BATCH],
                wsum: [0.0; BATCH],
                scores: [0.0; BATCH],
                bad: [false; BATCH],
                need: [false; BATCH],
                done: [false; BATCH],
            }),
            scratch: BlockScratch::default(),
            min: 0.0,
        })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let mut expl = self.sub.explain(seg, doc)?;
        if !expl.is_match() {
            return Ok(expl);
        }
        let fs = &self.q.functions;
        let single = fs.len() == 1 && self.filters[0].is_none();
        if !fs.is_empty() {
            let mut fexpls = Vec::new();
            let mut scratch = Vec::new();
            for (i, f) in fs.iter().enumerate() {
                if let Some(q) = &self.filters[i]
                    && !segment_bits(&self.searcher, seg, q)?.get(doc)
                {
                    continue;
                }
                let mut lf = LeafFn::new(&f.function, seg)?;
                let mut e = lf.explain(doc, &expl, &mut scratch)?;
                if let ScoreFunction::Random { seed, field, .. } = &f.function {
                    let name = match field {
                        None => "null".to_string(),
                        Some(RandomField::Bytes { name, .. } | RandomField::Number { name, .. }) => name.clone(),
                    };
                    e.description = format!("random score function (seed: {seed}, field: {name})");
                }
                if let ScoreFunction::Script(s) = &f.function
                    && e.description == "script score function, computed with script:"
                {
                    e.description = format!("script score function, computed with script:\"{}\"", s.describe());
                }
                if let Some(w) = f.weight {
                    e = Explanation::matched(e.value() * w, "product of:", vec![e, Explanation::leaf(w, "weight")]);
                }
                if self.filters[i].is_some() {
                    e = Explanation::matched(
                        e.value(),
                        "function score, product of:",
                        vec![Explanation::leaf(1.0f32, format!("match filter: {}", self.filter_descs[i])), e],
                    );
                }
                fexpls.push(e);
            }
            let factor = if fexpls.is_empty() {
                Explanation::leaf(1.0f32, "No function matched")
            } else if single && fexpls.len() == 1 {
                fexpls.pop().unwrap()
            } else {
                let mut sc = self.fs_scorer(seg)?.ok_or_else(|| Error::illegal_state("no scorer"))?;
                sc.inner.advance(doc);
                let v = sc.factor(doc, expl.value())?;
                let mode = format!("{:?}", self.q.score_mode).to_lowercase();
                Explanation::matched(v as f32, format!("function score, score mode [{mode}]"), fexpls)
            };
            expl = self.q.boost_mode.explain(expl, factor, self.q.max_boost);
        }
        if let Some(m) = self.q.min_score
            && m > expl.value()
        {
            return Ok(Explanation::no_match(
                format!("Score value is too low, expected at least {} but got {}", float_to_string(m), expl.value.to_java_string()),
                vec![expl],
            ));
        }
        Ok(expl)
    }
}

impl Query for FunctionScoreQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        if mode == ScoreMode::CompleteNoScores && self.min_score.is_none() {
            return s.rewrite(&self.query)?.weight(s, mode, boost);
        }
        let needs = self.needs_scores();
        let sub_mode = if needs { ScoreMode::Complete } else { ScoreMode::CompleteNoScores };
        let sub = s.weight_of(&s.rewrite(&self.query)?, sub_mode, boost)?;
        let mut filters = Vec::new();
        let mut descs = Vec::new();
        let single = self.functions.len() == 1;
        for f in &self.functions {
            match &f.filter {
                // A lone function filtered by match_all is a plain function (Elasticsearch
                // builds its single-function query then).
                Some(q) if single && s.rewrite(q)?.as_any().is::<crate::query::basic::MatchAllQuery>() => {
                    descs.push(String::new());
                    filters.push(None);
                }
                Some(q) => {
                    let r = s.rewrite(q)?;
                    descs.push(r.lucene_string(s.schema()));
                    filters.push(Some(r));
                }
                None => {
                    descs.push(String::new());
                    filters.push(None);
                }
            }
        }
        Ok(Box::new(FsWeight { sub, q: self.clone(), filters, filter_descs: descs, searcher: s.clone(), needs_scores: needs }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let r = s.rewrite(&self.query)?;
        if !Arc::ptr_eq(&r, &self.query) {
            let mut q = self.clone();
            q.query = r;
            return Ok(Some(Arc::new(q)));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let mut b = format!("function score ({}, functions: [", self.query.lucene_string(schema));
        for f in &self.functions {
            b.push('{');
            b.push_str(&f.java_string());
            b.push('}');
        }
        b.push_str("])");
        b
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.query.visit(v)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Elasticsearch's `script_score` query: the script's value is the score.
#[derive(Clone)]
pub struct ScriptScoreQuery {
    pub query: QueryRef,
    pub script: Arc<dyn ScoreScript>,
    pub min_score: Option<f32>,
}

impl std::fmt::Debug for ScriptScoreQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptScoreQuery({:?}, {})", self.query, self.script.describe())
    }
}

struct SsWeight {
    sub: Box<dyn Weight>,
    script: Arc<dyn ScoreScript>,
    min_score: Option<f32>,
    boost: f32,
    needs_score: bool,
}

struct SsScorer {
    inner: Box<dyn Scorer>,
    leaf: Box<dyn LeafScoreScript>,
    boost: f32,
    min_score: Option<f32>,
    needs_score: bool,
    cur: f32,
    failed: bool,
}

impl SsScorer {
    fn compute(&mut self) -> Result<f32> {
        let doc = self.inner.doc();
        let sub = if self.needs_score { self.inner.score() } else { 0.0 };
        self.compute_at(doc, sub)
    }

    fn compute_at(&mut self, doc: u32, sub: f32) -> Result<f32> {
        let v = self.leaf.score(doc, sub)? as f32;
        if v < 0.0 || v.is_nan() {
            return Err(Error::illegal(format!(
                "script_score script returned an invalid score [{}] for doc [{doc}]. Must be a non-negative score!",
                float_to_string(v)
            )));
        }
        Ok(v * self.boost)
    }
}

/// [`FsBulkScorer`] for `script_score`: the script scores a block of the inner query's
/// matches at a time ([`LeafScoreScript::score_block`]).
struct SsBulkScorer {
    sc: SsScorer,
    buf: Box<DocScoreBuf>,
    subs: Box<[f32; BATCH]>,
    out: Box<[f64; BATCH]>,
    bad: Box<[bool; BATCH]>,
    min: f32,
}

impl BulkScorer for SsBulkScorer {
    fn score(&mut self, collector: &mut dyn LeafCollector, live: Option<&search_codec::LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let s = &mut self.sc;
        let mut d = s.inner.doc();
        if d == u32::MAX || d < min {
            d = s.inner.advance(min);
        }
        if collector.skips() {
            return crate::search::score_skipping(s, collector, live, d, max);
        }
        let mut b = BlockScore { score: 0.0, min: self.min };
        loop {
            if s.failed {
                return Ok(NO_MORE_DOCS);
            }
            s.inner.next_docs_and_scores(max, live, &mut self.buf);
            let n = self.buf.len;
            if n == 0 {
                break;
            }
            let docs = &self.buf.docs[..n];
            let subs = &mut self.subs[..n];
            if s.needs_score {
                subs.copy_from_slice(&self.buf.scores[..n]);
            } else {
                subs.fill(0.0);
            }
            let (out, bad) = (&mut self.out[..n], &mut self.bad[..n]);
            bad.fill(false);
            let mut start = 0;
            while start < n {
                match s.leaf.score_block(&docs[start..], &subs[start..], &mut out[start..]) {
                    Ok(()) => break,
                    Err((k, _)) => {
                        bad[start + k] = true;
                        start += k + 1;
                    }
                }
            }
            for (i, &doc) in docs.iter().enumerate() {
                let v = out[i] as f32;
                if bad[i] || v < 0.0 || v.is_nan() {
                    let sub = subs[i];
                    let mut l = LazyScore {
                        f: || match s.compute_at(doc, sub) {
                            Ok(v) => v,
                            Err(e) => {
                                defer_error(e);
                                s.failed = true;
                                0.0
                            }
                        },
                        cur: None,
                    };
                    collector.collect(doc, &mut l)?;
                    if s.failed {
                        self.min = b.min;
                        return Ok(NO_MORE_DOCS);
                    }
                } else {
                    let score = v * s.boost;
                    if b.min <= 0.0 || score >= b.min {
                        b.score = score;
                        collector.collect(doc, &mut b)?;
                    }
                }
            }
        }
        self.min = b.min;
        Ok(s.inner.doc())
    }

    fn cost(&self) -> u64 {
        self.sc.inner.cost()
    }
}

impl Scorable for SsScorer {
    fn score(&mut self) -> f32 {
        if self.min_score.is_none() {
            match self.compute() {
                Ok(s) => self.cur = s,
                Err(e) => {
                    defer_error(e);
                    self.failed = true;
                    self.cur = 0.0;
                }
            }
        }
        self.cur
    }
}

impl Scorer for SsScorer {
    fn doc(&self) -> u32 {
        if self.failed { NO_MORE_DOCS } else { self.inner.doc() }
    }
    fn next(&mut self) -> u32 {
        loop {
            let d = self.approx_next();
            if d == NO_MORE_DOCS || self.matches() {
                return if self.failed { NO_MORE_DOCS } else { d };
            }
        }
    }
    fn advance(&mut self, t: u32) -> u32 {
        let d = self.approx_advance(t);
        if d == NO_MORE_DOCS || self.matches() {
            return if self.failed { NO_MORE_DOCS } else { d };
        }
        self.next()
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
    fn has_two_phase(&self) -> bool {
        self.min_score.is_some() || self.inner.has_two_phase()
    }
    fn approx_next(&mut self) -> u32 {
        if self.failed {
            return NO_MORE_DOCS;
        }
        self.inner.approx_next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.failed {
            return NO_MORE_DOCS;
        }
        self.inner.approx_advance(t)
    }
    fn matches(&mut self) -> bool {
        if self.inner.has_two_phase() && !self.inner.matches() {
            return false;
        }
        if let Some(m) = self.min_score {
            return match self.compute() {
                Ok(s) => {
                    self.cur = s;
                    s >= m
                }
                Err(e) => {
                    defer_error(e);
                    self.failed = true;
                    false
                }
            };
        }
        true
    }
    fn match_cost(&self) -> f32 {
        self.inner.match_cost() + if self.min_score.is_some() { 1000.0 } else { 0.0 }
    }
}

impl SsWeight {
    fn ss_scorer(&self, seg: &SegmentContext) -> Result<Option<SsScorer>> {
        let Some(inner) = self.sub.scorer(seg)? else { return Ok(None) };
        Ok(Some(SsScorer {
            inner,
            leaf: self.script.leaf(seg)?,
            boost: self.boost,
            min_score: self.min_score,
            needs_score: self.needs_score,
            cur: 0.0,
            failed: false,
        }))
    }
}

impl Weight for SsWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.ss_scorer(seg)?.map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        let Some(sc) = self.ss_scorer(seg)? else { return Ok(None) };
        if sc.has_two_phase() {
            return Ok(Some(Box::new(DefaultBulkScorer::new(Box::new(sc)))));
        }
        Ok(Some(Box::new(SsBulkScorer {
            sc,
            buf: DocScoreBuf::new(),
            subs: Box::new([0.0; BATCH]),
            out: Box::new([0.0; BATCH]),
            bad: Box::new([false; BATCH]),
            min: 0.0,
        })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let sub = self.sub.explain(seg, doc)?;
        if !sub.is_match() {
            return Ok(sub);
        }
        let mut leaf = self.script.leaf(seg)?;
        let v = leaf.score(doc, sub.value())? as f32;
        let mut e = match leaf.explanation() {
            Some(d) => {
                if self.needs_score {
                    Explanation::matched(v, d, vec![sub.clone()])
                } else {
                    Explanation::leaf(v, d)
                }
            }
            None => {
                let desc = format!("script score function, computed with script:\"{}\"", self.script.describe());
                if self.needs_score {
                    Explanation::matched(v, desc, vec![Explanation::matched(sub.value, "_score: ", vec![sub.clone()])])
                } else {
                    Explanation::leaf(v, desc)
                }
            }
        };
        if self.boost != 1.0 {
            e = Explanation::matched(self.boost * e.value(), "Boosted score, product of:", vec![Explanation::leaf(self.boost, "boost"), e]);
        }
        if let Some(m) = self.min_score
            && m > e.value()
        {
            return Ok(Explanation::no_match(
                format!("Score value is too low, expected at least {} but got {}", float_to_string(m), e.value.to_java_string()),
                vec![e],
            ));
        }
        Ok(e)
    }
}

impl Query for ScriptScoreQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        if mode == ScoreMode::CompleteNoScores && self.min_score.is_none() {
            return s.rewrite(&self.query)?.weight(s, mode, boost);
        }
        let needs = self.script.needs_score();
        let sub_mode = if needs { ScoreMode::Complete } else { ScoreMode::CompleteNoScores };
        let sub = s.weight_of(&s.rewrite(&self.query)?, sub_mode, 1.0)?;
        Ok(Box::new(SsWeight { sub, script: self.script.clone(), min_score: self.min_score, boost, needs_score: needs }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let r = s.rewrite(&self.query)?;
        if !Arc::ptr_eq(&r, &self.query) {
            return Ok(Some(Arc::new(ScriptScoreQuery { query: r, script: self.script.clone(), min_score: self.min_score })));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("script_score ({}, script: {{{}}}", self.query.lucene_string(schema), self.script.describe())
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.query.visit(v)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_keeps_nan_like_java() {
        // `ln1p` of a negative factor is NaN; Java's Math.min keeps it, so the query fails
        // with an invalid score instead of scoring `max_boost`.
        let s = CombineFunction::Multiply.combine(1.0, f64::NAN, f32::MAX as f64);
        assert!(s.is_nan());
        assert!(CombineFunction::Max.combine(f64::NAN, 1.0, 10.0).is_nan());
        assert_eq!(CombineFunction::Replace.combine(1.0, 20.0, 10.0), 10.0);
        assert!(java_min(-0.0, 0.0).is_sign_negative());
        assert!(!java_max(-0.0, 0.0).is_sign_negative());
    }

    #[test]
    fn murmur_and_mix_match_java() {
        // Values from Lucene's StringHelper / HPPC BitMixer.
        assert_eq!(murmur3_32(b"", 0), 0);
        assert_eq!(murmur3_32(b"hello", 0), 613153351);
        assert_eq!(murmur3_32(b"hello, world", 0), 345750399);
        assert_eq!(murmur3_32(b"abc", 42), 1313807976);
        assert_eq!(mix32(0), 0);
        assert_eq!(mix32(1), 1364076727);
    }

    #[test]
    fn decay_shapes() {
        let s = DecayFn::Gauss.process_scale(10.0, 0.5);
        assert!((DecayFn::Gauss.evaluate(10.0, s) - 0.5).abs() < 1e-12);
        let s = DecayFn::Exp.process_scale(10.0, 0.5);
        assert!((DecayFn::Exp.evaluate(10.0, s) - 0.5).abs() < 1e-12);
        let s = DecayFn::Linear.process_scale(10.0, 0.5);
        assert!((DecayFn::Linear.evaluate(10.0, s) - 0.5).abs() < 1e-12);
        assert_eq!(MultiValueMode::Median.select(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }
}

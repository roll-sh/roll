//! `significant_terms`: terms unusually frequent in a bucket compared with a background set.
//!
//! The subset is the documents collected into the owning bucket (`doc_count`); the superset is
//! the shard's documents (root documents, as Elasticsearch filters out nested ones) or those
//! matching `background_filter` (`bg_count`). Each term's background frequency is a count of the
//! superset documents holding it. A heuristic (JLH by default, or mutual information,
//! chi-square, GND, percentage, a script) scores every term; shards keep their `shard_size` best
//! by score in a Lucene priority queue fed in Elasticsearch's term order (global-ordinal order
//! for the dense keyword case, first-seen order otherwise), so ties resolve the same way. The
//! reduce sums subset and superset counts per term — iterating terms in the order of
//! Elasticsearch's paged hash map — rescores and keeps the `size` best with a score above zero
//! and at least `min_doc_count` documents.

use std::sync::Arc;

use search_index::query::boolean::{BooleanQuery, Occur};
use search_index::query::range::{DocValuesRangeQuery, DocValuesTermsQuery, DvRange};
use search_index::{QueryRef, Searcher, SegmentContext};

use crate::bucket::include::{IncludeExclude, LongFilter, StringFilter};
use crate::bucket::terms::{TermKey, suggest_shard_size};
use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::script::{HeuristicScript, Script, ScriptHost};
use crate::util::{LucenePq, java_string_hash_code, paged_hash_map_order};
use crate::values::{BoundVs, BytesReader, DocCounter, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Token, Tokens, XParser};

/// A significance heuristic.
#[derive(Clone, Debug, PartialEq)]
pub enum Heuristic {
    Jlh,
    MutualInformation { include_negatives: bool, background_is_superset: bool },
    ChiSquare { include_negatives: bool, background_is_superset: bool },
    Gnd { background_is_superset: bool },
    Percentage,
    Script(Script),
}

/// The names of the heuristics (named objects in the aggregation body).
pub const HEURISTICS: &[&str] = &["jlh", "mutual_information", "chi_square", "gnd", "percentage", "script_heuristic"];

/// Parses a heuristic's object (positioned on its `START_OBJECT`).
pub fn parse_heuristic(p: &mut XParser<'_>, name: &str) -> Result<Heuristic> {
    match name {
        "jlh" | "percentage" => {
            p.object(name, &[], |_, _| Ok(()))?;
            Ok(if name == "jlh" { Heuristic::Jlh } else { Heuristic::Percentage })
        }
        "mutual_information" | "chi_square" => {
            let fields = [Field::new("include_negatives", Tokens::BOOLEAN), Field::new("background_is_superset", Tokens::BOOLEAN)];
            let (mut neg, mut sup) = (false, true);
            p.object(name, &fields, |p, i| {
                if i == 0 {
                    neg = p.boolean_value()?;
                } else {
                    sup = p.boolean_value()?;
                }
                Ok(())
            })?;
            Ok(if name == "chi_square" {
                Heuristic::ChiSquare { include_negatives: neg, background_is_superset: sup }
            } else {
                Heuristic::MutualInformation { include_negatives: neg, background_is_superset: sup }
            })
        }
        "gnd" => {
            let fields = [Field::new("background_is_superset", Tokens::BOOLEAN)];
            let mut sup = true;
            p.object("gnd", &fields, |p, _| {
                sup = p.boolean_value()?;
                Ok(())
            })?;
            Ok(Heuristic::Gnd { background_is_superset: sup })
        }
        _ => {
            let fields = [Field::new("script", Tokens::OBJECT_OR_STRING)];
            let mut script = None;
            p.object("script_heuristic", &fields, |p, _| {
                script = Some(Script::parse(p)?);
                Ok(())
            })?;
            match script {
                Some(s) => Ok(Heuristic::Script(s)),
                None => Err(AggError::illegal_argument("Required [script]")),
            }
        }
    }
}

const SCORE_ERROR: &str = ", does your background filter not include all documents in the bucket? If so and it is intentional, set \"background_is_superset\": false\n";

fn check_validity(sub_f: i64, sub_s: i64, sup_f: i64, sup_s: i64, name: &str) -> Result<()> {
    if sub_f < 0 || sub_s < 0 || sup_f < 0 || sup_s < 0 {
        return Err(AggError::illegal_argument(format!("Frequencies of subset and superset must be positive in {name}.getScore()")));
    }
    if sub_f > sub_s {
        return Err(AggError::illegal_argument(format!("subsetFreq > subsetSize, in {name}")));
    }
    if sup_f > sup_s {
        return Err(AggError::illegal_argument(format!("supersetFreq > supersetSize, in {name}")));
    }
    Ok(())
}

/// `NXYSignificanceHeuristic.Frequencies`: `[n00, n01, n10, n11, n0_, n1_, n_0, n_1, n]`.
fn nxys(sub_f: i64, sub_s: i64, sup_f: i64, sup_s: i64, superset: bool, name: &str) -> Result<[f64; 9]> {
    check_validity(sub_f, sub_s, sup_f, sup_s, name)?;
    if superset {
        if sub_f > sup_f {
            return Err(AggError::illegal_argument(format!("subsetFreq > supersetFreq{SCORE_ERROR}")));
        }
        if sub_s > sup_s {
            return Err(AggError::illegal_argument(format!("subsetSize > supersetSize{SCORE_ERROR}")));
        }
        if sup_f - sub_f > sup_s - sub_s {
            return Err(AggError::illegal_argument(format!("supersetFreq - subsetFreq > supersetSize - subsetSize{SCORE_ERROR}")));
        }
        Ok([
            (sup_s - sup_f - (sub_s - sub_f)) as f64,
            (sub_s - sub_f) as f64,
            (sup_f - sub_f) as f64,
            sub_f as f64,
            (sup_s - sup_f) as f64,
            sup_f as f64,
            (sup_s - sub_s) as f64,
            sub_s as f64,
            sup_s as f64,
        ])
    } else {
        Ok([
            (sup_s - sup_f) as f64,
            (sub_s - sub_f) as f64,
            sup_f as f64,
            sub_f as f64,
            (sup_s - sup_f + sub_s - sub_f) as f64,
            (sup_f + sub_f) as f64,
            sup_s as f64,
            sub_s as f64,
            (sup_s + sub_s) as f64,
        ])
    }
}

/// `MutualInformation.getMITerm`.
fn mi_term(nxy: f64, nx: f64, ny: f64, n: f64) -> f64 {
    let numerator = (n * nxy).abs();
    let denominator = (nx * ny).abs();
    let factor = (nxy / n).abs();
    if numerator < 1e-7 && factor < 1e-7 { 0.0 } else { factor * (numerator / denominator).ln() }
}

/// A heuristic ready to score (scripts compiled).
#[derive(Clone)]
pub enum Scorer {
    Native(Heuristic),
    Script(Arc<dyn HeuristicScript>),
}

impl Scorer {
    fn new(h: &Heuristic, host: Option<&Arc<dyn ScriptHost>>) -> Result<Scorer> {
        match h {
            Heuristic::Script(s) => match host {
                Some(host) => Ok(Scorer::Script(host.heuristic(s)?)),
                None => Err(AggError::illegal_argument("scripts are not supported here")),
            },
            other => Ok(Scorer::Native(other.clone())),
        }
    }

    /// `getScore(subsetFreq, subsetSize, supersetFreq, supersetSize)`.
    pub fn score(&self, sub_f: i64, sub_s: i64, sup_f: i64, sup_s: i64) -> Result<f64> {
        let h = match self {
            Scorer::Script(s) => return s.score(sub_f, sub_s, sup_f, sup_s),
            Scorer::Native(h) => h,
        };
        match h {
            Heuristic::Jlh => {
                check_validity(sub_f, sub_s, sup_f, sup_s, "JLHScore")?;
                if sub_s == 0 || sup_s == 0 {
                    return Ok(0.0);
                }
                let sup_f = if sup_f == 0 { 1 } else { sup_f };
                let sub_p = sub_f as f64 / sub_s as f64;
                let sup_p = sup_f as f64 / sup_s as f64;
                let abs_change = sub_p - sup_p;
                if abs_change <= 0.0 {
                    return Ok(0.0);
                }
                Ok(abs_change * (sub_p / sup_p))
            }
            Heuristic::Percentage => {
                check_validity(sub_f, sub_s, sup_f, sup_s, "PercentageScore")?;
                if sup_f == 0 {
                    return Ok(0.0);
                }
                Ok(sub_f as f64 / sup_f as f64)
            }
            Heuristic::MutualInformation { include_negatives, background_is_superset } => {
                let [n00, n01, n10, n11, n0_, n1_, n_0, n_1, n] = nxys(sub_f, sub_s, sup_f, sup_s, *background_is_superset, "MutualInformation")?;
                let mut score =
                    (mi_term(n00, n0_, n_0, n) + mi_term(n01, n0_, n_1, n) + mi_term(n10, n1_, n_0, n) + mi_term(n11, n1_, n_1, n)) / 2f64.ln();
                if score.is_nan() {
                    score = f64::NEG_INFINITY;
                }
                if !include_negatives && n11 / n_1 < n10 / n_0 {
                    score = f64::NEG_INFINITY;
                }
                Ok(score)
            }
            Heuristic::ChiSquare { include_negatives, background_is_superset } => {
                let [n00, n01, n10, n11, n0_, n1_, n_0, n_1, n] = nxys(sub_f, sub_s, sup_f, sup_s, *background_is_superset, "ChiSquare")?;
                if !include_negatives && n11 / n_1 < n10 / n_0 {
                    return Ok(f64::NEG_INFINITY);
                }
                let d = n11 * n00 - n01 * n10;
                Ok(n * (d * d) / (n_1 * n1_ * n0_ * n_0))
            }
            Heuristic::Gnd { background_is_superset } => {
                let [_, _, _, n11, _, n1_, _, n_1, n] = nxys(sub_f, sub_s, sup_f, sup_s, *background_is_superset, "GND")?;
                let (fx, fy, fxy) = (n1_, n_1, n11);
                if fxy == 0.0 {
                    return Ok(0.0);
                }
                if fx == fy && fx == fxy {
                    return Ok(1.0);
                }
                let score = (fx.ln().max(fy.ln()) - fxy.ln()) / (n.ln() - fx.ln().min(fy.ln()));
                Ok((-score).exp())
            }
            Heuristic::Script(_) => unreachable!("compiled above"),
        }
    }
}

/// A parsed `significant_terms`.
#[derive(Clone, Debug)]
pub struct SignificantSpec {
    pub vs: VsSpec,
    pub size: usize,
    pub shard_size: Option<usize>,
    pub min_doc_count: u64,
    pub shard_min_doc_count: u64,
    pub execution_hint: Option<String>,
    pub background_filter: Option<Json>,
    pub include_exclude: Option<IncludeExclude>,
    pub heuristic: Heuristic,
}

/// `SignificantTermsAggregationBuilder.PARSER` (unknown object fields are heuristics).
pub fn parse(p: &mut XParser<'_>, name: &str, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<SignificantSpec> {
    let opts = VsOpts::new(true, true, false);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("shard_size", Tokens::INT),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("shard_min_doc_count", Tokens::LONG),
        Field::new("size", Tokens::INT),
        Field::new("execution_hint", Tokens::STRING),
        Field::new("background_filter", Tokens::OBJECT),
        Field::new("include", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("exclude", Tokens::STRING_ARRAY),
    ]);
    for h in HEURISTICS {
        fields.push(Field::new(h, Tokens::OBJECT));
    }
    let mut spec = SignificantSpec {
        vs: VsSpec::default(),
        size: 10,
        shard_size: None,
        min_doc_count: 3,
        shard_min_doc_count: 0,
        execution_hint: None,
        background_filter: None,
        include_exclude: None,
        heuristic: Heuristic::Jlh,
    };
    parse_object_with_heuristics(p, "significant_terms", &fields, |p, f| {
        if spec.vs.parse_field(p, f)? {
            return Ok(());
        }
        parse_common(p, f, name, &mut spec, check_query)
    })?;
    spec.vs.check_required(opts)?;
    Ok(spec)
}

/// Shared fields of `significant_terms` and `significant_text`.
fn parse_common(p: &mut XParser<'_>, f: &str, name: &str, spec: &mut SignificantSpec, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<()> {
    match f {
        "shard_size" => {
            let v = p.int_value()?;
            if v <= 0 {
                return Err(AggError::illegal_argument(format!("[shardSize] must be greater than  0. Found [{v}] in [{name}]")));
            }
            spec.shard_size = Some(v as usize);
        }
        "min_doc_count" => {
            let v = p.long_value()?;
            if v < 0 {
                return Err(AggError::illegal_argument(format!("[minDocCount] must be greater than or equal to 0. Found [{v}] in [{name}]")));
            }
            spec.min_doc_count = v as u64;
        }
        "shard_min_doc_count" => {
            let v = p.long_value()?;
            if v < 0 {
                return Err(AggError::illegal_argument(format!("[shardMinDocCount] must be greater than or equal to 0. Found [{v}] in [{name}]")));
            }
            spec.shard_min_doc_count = v as u64;
        }
        "size" => {
            let v = p.int_value()?;
            if v <= 0 {
                return Err(AggError::illegal_argument(format!("[size] must be greater than 0. Found [{v}] in [{name}]")));
            }
            spec.size = v as usize;
        }
        "execution_hint" => spec.execution_hint = Some(p.text()?),
        "background_filter" => {
            check_query(p)?;
            spec.background_filter = Some(p.take_value());
        }
        "include" => {
            let inc = IncludeExclude::parse_include(p)?;
            spec.include_exclude = IncludeExclude::merge(Some(inc), spec.include_exclude.take())?;
        }
        "exclude" => {
            let exc = IncludeExclude::parse_exclude(p)?;
            spec.include_exclude = IncludeExclude::merge(spec.include_exclude.take(), Some(exc))?;
        }
        h => spec.heuristic = parse_heuristic(p, h)?,
    }
    Ok(())
}

/// `ObjectParser` with a named-object fallback: unknown fields holding objects are looked up
/// as heuristics (`named_object_not_found_exception` when there's none of that name).
fn parse_object_with_heuristics(
    p: &mut XParser<'_>,
    parser_name: &str,
    fields: &[Field],
    mut on: impl FnMut(&mut XParser<'_>, &str) -> Result<()>,
) -> Result<()> {
    let known: Vec<Field> = fields.iter().copied().filter(|f| !HEURISTICS.contains(&f.name)).collect();
    if p.current_token() != Some(Token::StartObject) {
        p.next_token();
    }
    // Scan field by field so unknown names get the named-object error, not "unknown field".
    while let Some(t) = p.next_token() {
        if t == Token::EndObject {
            break;
        }
        let name = p.current_name().unwrap_or("").to_string();
        let name_loc = p.token_location();
        let vt = p.next_token().unwrap_or(Token::EndObject);
        if let Some(field) = known.iter().find(|f| f.name == name || f.deprecated.contains(&name.as_str())) {
            if !field.tokens.contains(vt) {
                return Err(p.xcontent_error(format!("[{parser_name}] {name} doesn't support values of type: {vt}")));
            }
            if let Err(e) = on(p, field.name) {
                return Err(p.xcontent_error(format!("[{parser_name}] failed to parse field [{name}]")).caused_by(e));
            }
            continue;
        }
        if HEURISTICS.contains(&name.as_str()) && vt == Token::StartObject {
            if let Err(e) = on(p, &name) {
                return Err(p.xcontent_error(format!("[{parser_name}] failed to parse field [{name}]")).caused_by(e));
            }
            continue;
        }
        let value_loc = p.token_location();
        return Err(AggError::x_content(name_loc, format!("[{parser_name}] unknown field [{name}]")).caused_by(AggError::new(
            400,
            "named_object_not_found_exception",
            format!("[{}:{}] unknown field [{name}]", value_loc.0, value_loc.1),
        )));
    }
    Ok(())
}

/// A parsed `significant_text`.
#[derive(Clone, Debug)]
pub struct SigTextSpec {
    pub field: Option<String>,
    pub source_fields: Option<Vec<String>>,
    pub filter_duplicate_text: bool,
    pub common: SignificantSpec,
}

/// `SignificantTextAggregationBuilder.PARSER`.
pub fn parse_text(p: &mut XParser<'_>, name: &str, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<SigTextSpec> {
    let mut fields = vec![
        Field::new("shard_size", Tokens::INT),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("shard_min_doc_count", Tokens::LONG),
        Field::new("size", Tokens::INT),
        Field::new("field", Tokens::STRING),
        Field::new("source_fields", Tokens::STRING_ARRAY),
        Field::new("filter_duplicate_text", Tokens::BOOLEAN),
        Field::new("background_filter", Tokens::OBJECT),
        Field::new("include", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("exclude", Tokens::STRING_ARRAY),
    ];
    for h in HEURISTICS {
        fields.push(Field::new(h, Tokens::OBJECT));
    }
    let mut spec = SigTextSpec {
        field: None,
        source_fields: None,
        filter_duplicate_text: false,
        common: SignificantSpec {
            vs: VsSpec::default(),
            size: 10,
            shard_size: None,
            min_doc_count: 3,
            shard_min_doc_count: 0,
            execution_hint: None,
            background_filter: None,
            include_exclude: None,
            heuristic: Heuristic::Jlh,
        },
    };
    parse_object_with_heuristics(p, "significant_text", &fields, |p, f| match f {
        "field" => {
            spec.field = Some(p.text()?);
            Ok(())
        }
        "source_fields" => {
            spec.source_fields = Some(p.array_items(|p| p.text())?);
            Ok(())
        }
        "filter_duplicate_text" => {
            spec.filter_duplicate_text = p.boolean_value()?;
            Ok(())
        }
        other => parse_common(p, other, name, &mut spec.common, check_query),
    })?;
    Ok(spec)
}

/// Elasticsearch's duplicate-text detection (`filter_duplicate_text`), per owning bucket: every
/// token is hashed to a byte (the low byte of its 128-bit Murmur3 `h1`), and each run of six
/// consecutive token bytes in a document is counted; when a run was seen before, its six
/// tokens are dropped from the document's value.
#[derive(Default)]
struct DupSpotter {
    seen: hashbrown::HashMap<[u8; 6], u8>,
    window: [u8; 6],
    filled: usize,
    next: usize,
}

impl DupSpotter {
    fn start_new_sequence(&mut self) {
        self.filled = 0;
        self.next = 0;
    }

    /// Adds a token byte; returns how often the last six bytes were seen before (0 until six
    /// bytes of the current document were added).
    fn add(&mut self, b: u8) -> u16 {
        self.window[self.next] = b;
        self.next = (self.next + 1) % 6;
        if self.filled < 6 {
            self.filled += 1;
        }
        if self.filled < 6 {
            return 0;
        }
        let mut key = [0u8; 6];
        for (i, k) in key.iter_mut().enumerate() {
            *k = self.window[(self.next + i) % 6];
        }
        let c = self.seen.entry(key).or_insert(0);
        let prior = *c;
        *c = c.saturating_add(1);
        prior as u16
    }

    /// The tokens of one value that survive (`DeDuplicatingTokenFilter`).
    fn filter(&mut self, tokens: &[Vec<u8>]) -> Vec<usize> {
        let mut dup = vec![0u16; tokens.len()];
        for (i, t) in tokens.iter().enumerate() {
            let byte = (crate::util::murmur3_x64_128(t, 0).0 & 0xFF) as u8;
            let n = self.add(byte);
            if n >= 1 {
                for d in dup[i.saturating_sub(5)..=i].iter_mut() {
                    *d = (*d).max(n);
                }
            }
        }
        (0..tokens.len()).filter(|&i| dup[i] < 1).collect()
    }
}

/// Which result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigKind {
    Str,
    Long,
    Unmapped,
}

/// One significant term.
#[derive(Clone, Debug)]
pub struct SigBucket {
    pub key: TermKey,
    pub subset_df: u64,
    pub superset_df: u64,
    pub score: f64,
    pub aggs: Aggs,
}

/// `SignificantStringTerms` / `SignificantLongTerms` / `UnmappedSignificantTerms`.
#[derive(Clone, Debug)]
pub struct SignificantTerms {
    pub kind: SigKind,
    pub size: usize,
    pub min_doc_count: u64,
    pub format: DocValueFormat,
    pub subset_size: u64,
    pub superset_size: u64,
    pub heuristic: Heuristic,
    pub buckets: Vec<SigBucket>,
}

impl SignificantTerms {
    /// `getKeyAsString()`.
    fn key_string(&self, k: &TermKey) -> String {
        match k {
            TermKey::Bytes(b) => match self.format.format_bytes(b) {
                Json::Str(s) => s,
                o => o.to_java_string(),
            },
            TermKey::Long(l) => self.format.long_string(*l),
            TermKey::Double(d) => self.format.double_string(*d),
            TermKey::Multi(_) => String::new(),
        }
    }
}

/// `lessThan`: by score only.
fn score_less(a: &SigBucket, b: &SigBucket) -> bool {
    a.score < b.score
}

impl Internal for SignificantTerms {
    fn type_name(&self) -> &'static str {
        match self.kind {
            SigKind::Str | SigKind::Unmapped => "sigsterms",
            SigKind::Long => "siglterms",
        }
    }
    fn class_name(&self) -> &'static str {
        match self.kind {
            SigKind::Str => "SignificantStringTerms",
            SigKind::Long => "SignificantLongTerms",
            SigKind::Unmapped => "UnmappedSignificantTerms",
        }
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn can_lead(&self) -> bool {
        self.kind != SigKind::Unmapped
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let Some(lead) = list.iter().position(|t| t.kind != SigKind::Unmapped) else {
            return Ok(list.into_iter().next().expect("one"));
        };
        let mut out = list[lead].clone();
        let initial = out.buckets.len();
        let mut subset_size = 0u64;
        let mut superset_size = 0u64;
        // Per key string: subset/superset sums and the buckets' aggregations.
        let mut index: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
        let mut groups: Vec<(String, SigBucket, Vec<Aggs>)> = Vec::new();
        for t in list {
            if t.kind == SigKind::Unmapped {
                continue;
            }
            if t.kind != out.kind {
                return Err(crate::internal::reduce_type_mismatch(""));
            }
            subset_size += t.subset_size;
            superset_size += t.superset_size;
            let mut t = t;
            let buckets = std::mem::take(&mut t.buckets);
            for b in buckets {
                let ks = t.key_string(&b.key);
                match index.get(&ks) {
                    Some(&i) => {
                        groups[i].1.subset_df += b.subset_df;
                        groups[i].1.superset_df += b.superset_df;
                        groups[i].2.push(b.aggs);
                    }
                    None => {
                        index.insert(ks.clone(), groups.len());
                        let SigBucket { key, subset_df, superset_df, score, aggs } = b;
                        groups.push((ks, SigBucket { key, subset_df, superset_df, score, aggs: Aggs::default() }, vec![aggs]));
                    }
                }
            }
        }
        let scorer = Scorer::new(&out.heuristic, ctx.opts.scripts.as_ref())?;
        let hashes: Vec<i32> = groups.iter().map(|g| java_string_hash_code(&g.0)).collect();
        let order = paged_hash_map_order(&hashes, initial);
        let is_final = ctx.is_final();
        let size = if is_final { out.size.min(groups.len()) } else { groups.len() };
        let mut slots: Vec<Option<(String, SigBucket, Vec<Aggs>)>> = groups.into_iter().map(Some).collect();
        let mut pq = LucenePq::new(size);
        for i in order {
            let (_, mut b, aggs) = slots[i].take().expect("group");
            b.aggs = Aggs::reduce_hashed(aggs, ctx)?;
            b.score = scorer.score(b.subset_df as i64, subset_size as i64, b.superset_df as i64, superset_size as i64)?;
            if ((b.score > 0.0 && b.subset_df >= out.min_doc_count) || !is_final) && pq.insert_with_overflow(b, &score_less).is_none() {
                ctx.consume_buckets(1)?;
            }
        }
        let mut buckets = pq.drain_ascending(&score_less);
        buckets.reverse();
        out.buckets = buckets;
        out.subset_size = subset_size;
        out.superset_size = superset_size;
        Ok(out)
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        if self.kind != SigKind::Unmapped {
            out.push("doc_count", Json::from(self.subset_size));
            out.push("bg_count", Json::from(self.superset_size));
        }
        let buckets = self
            .buckets
            .iter()
            .filter(|b| b.subset_df >= self.min_doc_count)
            .map(|b| {
                let mut o = Json::obj();
                match &b.key {
                    TermKey::Long(l) => {
                        o.push("key", Json::Int(*l));
                        if !self.format.is_raw() {
                            o.push("key_as_string", self.format.format_long(*l));
                        }
                    }
                    k => o.push("key", Json::Str(self.key_string(k))),
                }
                o.push("doc_count", Json::from(b.subset_df));
                o.push("score", Json::Float(b.score));
                o.push("bg_count", Json::from(b.superset_df));
                b.aggs.render_into(&mut o, rc);
                o
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
    fn ram_bytes(&self) -> usize {
        64 + self.buckets.iter().map(|b| 64 + b.aggs.ram_bytes()).sum::<usize>()
    }
}

impl MultiBuckets for SignificantTerms {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].subset_df
    }
    fn key(&self, i: usize) -> Json {
        match &self.buckets[i].key {
            TermKey::Long(l) => Json::Int(*l),
            k => Json::Str(self.key_string(k)),
        }
    }
    fn key_string(&self, i: usize) -> String {
        self.key_string(&self.buckets[i].key)
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<SigBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

/// Binds `significant_terms`.
/// Under a sampler, counts can't be compared with thresholds above 1 (`min_doc_count` and
/// `shard_min_doc_count` other than the default 3).
fn check_sampled_thresholds(name: &str, min: u64, shard_min: u64, ctx: &BindCtx<'_>) -> Result<()> {
    if ctx.sampling.is_some() && ((min != 3 && min > 1) || (shard_min != 3 && shard_min > 1)) {
        return Err(AggError::new(
            400,
            "status_exception",
            format!(
                "aggregation [{name}] is within a sampling context; min_doc_count, provided [{min}], and min_shard_doc_count, provided [{shard_min}], cannot be greater than 1"
            ),
        ));
    }
    Ok(())
}

pub fn create(def: &AggDef, spec: &SignificantSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Keyword, false)?;
    if vs.is_counter() {
        return Err(vs.unsupported("significant_terms"));
    }
    check_sampled_thresholds(&def.name, spec.min_doc_count, spec.shard_min_doc_count, ctx)?;
    let subs = Subs::bind(def, ctx)?;
    let mut shard_size = spec.shard_size.unwrap_or_else(|| 2 * suggest_shard_size(spec.size));
    if shard_size < spec.size {
        shard_size = spec.size;
    }
    let shard_min_doc_count = spec.shard_min_doc_count.min(spec.min_doc_count);
    let base = |kind| SignificantTerms {
        kind,
        size: spec.size,
        min_doc_count: spec.min_doc_count,
        format: vs.format.clone(),
        subset_size: 0,
        superset_size: 0,
        heuristic: spec.heuristic.clone(),
        buckets: Vec::new(),
    };
    if vs.unmapped && vs.missing.is_none() {
        return Ok(Box::new(UnmappedSig { proto: base(SigKind::Unmapped) }));
    }
    let many = !ctx.cardinality.is_one();
    let (kind, sfilter, lfilter, dense) = match vs.vs_type {
        VsType::Keyword | VsType::Ip => {
            if let Some(h) = &spec.execution_hint
                && h != "map"
                && h != "global_ordinals"
            {
                return Err(AggError::illegal_argument(format!("Unknown `execution_hint`: [{h}], expected any of [map, global_ordinals]")));
            }
            if let Some(ie) = &spec.include_exclude
                && ie.is_regex_based()
                && !vs.format.is_raw()
            {
                return Err(AggError::illegal_argument(format!(
                    "Aggregation [{}] cannot support regular expression style include/exclude settings as they can only be applied to string fields. Use an array of values for include/exclude clauses",
                    def.name
                )));
            }
            let filter = match &spec.include_exclude {
                Some(ie) => Some(ie.string_filter(&vs.format)?),
                None => None,
            };
            let ords = vs.has_ordinals() && vs.missing.is_none() && spec.execution_hint.as_deref() != Some("map");
            let dense = ords && !many && subs.is_empty() && spec.include_exclude.is_none();
            (SigKind::Str, filter, None, dense)
        }
        VsType::Numeric | VsType::Date | VsType::Boolean => {
            if !vs.is_integral() {
                return Err(AggError::new(500, "unsupported_operation_exception", "No support for examining floating point numerics"));
            }
            if let Some(ie) = &spec.include_exclude
                && ie.is_regex_based()
            {
                return Err(AggError::illegal_argument(format!(
                    "Aggregation [{}] cannot support regular expression style include/exclude settings as they can only be applied to string fields. Use an array of numeric values for include/exclude clauses used to filter numeric fields",
                    def.name
                )));
            }
            let filter = match &spec.include_exclude {
                Some(ie) => Some(ie.long_filter(&vs.format)?),
                None => None,
            };
            (SigKind::Long, None, filter, false)
        }
        _ => return Err(vs.unsupported("significant_terms")),
    };
    // The superset: the shard's root documents (plus alias filters), or the background filter.
    let superset_query: QueryRef = match &spec.background_filter {
        Some(q) => Arc::new(BooleanQuery::new().add(Occur::Filter, ctx.lookup.query(q)?).add(Occur::Filter, ctx.lookup.global_query())),
        None => ctx.lookup.global_query(),
    };
    let superset_size = match &ctx.sampling {
        Some(s) => s.count(ctx.searcher, &superset_query)?,
        None => ctx.searcher.count(&superset_query)?,
    };
    // Under a sampler the background is sampled too (term frequencies count the sampled
    // superset).
    let bg_filter = if spec.background_filter.is_some() || ctx.sampling.is_some() { Some(superset_query.clone()) } else { None };
    let scorer = Scorer::new(&spec.heuristic, ctx.lookup.scripts().as_ref())?;
    let field = vs.field.as_ref().map(|f| f.number);
    Ok(Box::new(SigAgg {
        proto: SignificantTerms { superset_size, ..base(kind) },
        text: None,
        vs: Some(vs),
        sfilter,
        lfilter,
        dense,
        shard_size,
        shard_min_doc_count,
        scorer: Some(scorer),
        field,
        bg_filter,
        sampling: ctx.sampling,
        searcher: ctx.searcher.clone(),
        bytes: None,
        nums: None,
        index: hashbrown::HashMap::new(),
        keys: Vec::new(),
        counts: Vec::new(),
        subset_sizes: Vec::new(),
        bg_cache: hashbrown::HashMap::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
    }))
}

/// The `significant_text` source of a [`SigAgg`].
struct TextSource {
    hook: Arc<dyn crate::field::TextTerms>,
    field: String,
    source_fields: Vec<String>,
    spotters: Option<Vec<Option<DupSpotter>>>,
    seg: Option<SegmentContext>,
}

struct SigAgg {
    proto: SignificantTerms,
    text: Option<TextSource>,
    /// The values source (`None` for `significant_text`).
    vs: Option<BoundVs>,
    sfilter: Option<StringFilter>,
    lfilter: Option<LongFilter>,
    /// Elasticsearch iterates dense global ordinals in term order (else first-seen order).
    dense: bool,
    shard_size: usize,
    shard_min_doc_count: u64,
    scorer: Option<Scorer>,
    field: Option<u32>,
    bg_filter: Option<QueryRef>,
    /// The enclosing sampler's sampling (background counts sample too).
    sampling: Option<crate::bucket::random_sampler::Sampling>,
    searcher: Searcher,
    bytes: Option<BytesReader>,
    nums: Option<NumReader>,
    index: hashbrown::HashMap<(u32, Vec<u8>), u32>,
    keys: Vec<(u32, TermKey)>,
    counts: Vec<u64>,
    subset_sizes: Vec<u64>,
    bg_cache: hashbrown::HashMap<Vec<u8>, u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
}

impl SigAgg {
    /// The background frequency of a term (a count over the superset).
    fn bg_freq(&mut self, key: &TermKey) -> Result<u64> {
        let mut id = Vec::new();
        key.identity(&mut id);
        if let Some(&f) = self.bg_cache.get(&id) {
            return Ok(f);
        }
        if let Some(t) = &self.text {
            let TermKey::Bytes(b) = key else { return Ok(0) };
            let n = match &self.bg_filter {
                None => t.hook.doc_freq(&self.searcher, &t.field, b)?,
                Some(f) => {
                    let q: QueryRef = Arc::new(BooleanQuery::new().add(Occur::Filter, t.hook.term_query(&t.field, b)?).add(Occur::Filter, f.clone()));
                    match &self.sampling {
                        Some(s) => s.count(&self.searcher, &q)?,
                        None => self.searcher.count(&q)?,
                    }
                }
            };
            self.bg_cache.insert(id, n);
            return Ok(n);
        }
        let Some(field) = self.field else { return Ok(0) };
        let term: QueryRef = match key {
            TermKey::Bytes(b) => Arc::new(DocValuesTermsQuery::new(field, [b.clone()])),
            TermKey::Long(l) => Arc::new(DocValuesRangeQuery { field, range: DvRange::Long { lower: *l, upper: *l } }),
            _ => return Ok(0),
        };
        let q: QueryRef = match &self.bg_filter {
            Some(f) => Arc::new(BooleanQuery::new().add(Occur::Filter, term).add(Occur::Filter, f.clone())),
            None => term,
        };
        let n = match &self.sampling {
            Some(s) => s.count(&self.searcher, &q)?,
            None => self.searcher.count(&q)?,
        };
        self.bg_cache.insert(id, n);
        Ok(n)
    }

    fn add(&mut self, owning: u32, key: TermKey, doc: u32, score: f32) -> Result<()> {
        let mut id = Vec::new();
        key.identity(&mut id);
        let ord = match self.index.get(&(owning, id.clone())) {
            Some(&o) => o,
            None => {
                let o = self.keys.len() as u32;
                self.index.insert((owning, id), o);
                self.keys.push((owning, key));
                grow(&mut self.counts, o, 0);
                self.tracker.ensure(self.keys.len() * 96)?;
                o
            }
        };
        self.counts[ord as usize] += self.doc_counter.count(doc);
        self.subs.push(doc, ord, score)
    }
}

impl Aggregator for SigAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if let Some(t) = &mut self.text {
            t.seg = Some(seg.clone());
            self.doc_counter = DocCounter::open(seg, self.doc_count_field);
            return self.subs.set_segment(seg);
        }
        if let Some(vs) = &self.vs {
            match self.proto.kind {
                SigKind::Str => self.bytes = Some(vs.bytes_reader(seg)?),
                SigKind::Long => self.nums = Some(vs.numeric_reader(seg)?),
                SigKind::Unmapped => {}
            }
        }
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        for i in 0..b.len() {
            let (doc, score, owning) = (b.docs[i], b.score(i), b.ords[i]);
            grow(&mut self.subset_sizes, owning, 0);
            self.subset_sizes[owning as usize] += 1;
            let mut keys: smallvec::SmallVec<[TermKey; 2]> = smallvec::SmallVec::new();
            if let Some(t) = self.text.as_mut() {
                let seg = t.seg.as_ref().expect("segment");
                let values = t.hook.tokens(seg, doc, &t.field, &t.source_fields)?;
                let mut in_doc: hashbrown::HashSet<Vec<u8>> = hashbrown::HashSet::new();
                let mut spotter = match &mut t.spotters {
                    Some(sp) => {
                        if sp.len() <= owning as usize {
                            sp.resize_with(owning as usize + 1, || None);
                        }
                        Some(sp[owning as usize].get_or_insert_with(DupSpotter::default))
                    }
                    None => None,
                };
                for tokens in values {
                    let kept: Vec<usize> = match spotter.as_mut() {
                        Some(sp) => sp.filter(&tokens),
                        None => (0..tokens.len()).collect(),
                    };
                    for i in kept {
                        let term = &tokens[i];
                        if self.sfilter.as_ref().is_some_and(|f| !f.accept(term)) {
                            continue;
                        }
                        if in_doc.insert(term.clone()) {
                            keys.push(TermKey::Bytes(term.clone()));
                        }
                    }
                }
                if let Some(sp) = spotter {
                    sp.start_new_sequence();
                }
            } else if let Some(r) = self.bytes.as_mut() {
                let n = r.load(doc, score)?;
                for j in 0..n {
                    let v = r.value(j);
                    if j > 0 && r.value(j - 1) == v {
                        continue;
                    }
                    if self.sfilter.as_ref().is_some_and(|f| !f.accept(v)) {
                        continue;
                    }
                    keys.push(TermKey::Bytes(v.to_vec()));
                }
            } else if let Some(r) = self.nums.as_mut() {
                let mut prev = None;
                for &v in r.longs(doc, score)? {
                    if prev == Some(v) {
                        continue;
                    }
                    prev = Some(v);
                    if self.lfilter.as_ref().is_some_and(|f| !f.accept(v)) {
                        continue;
                    }
                    keys.push(TermKey::Long(v));
                }
            }
            for k in keys {
                self.add(owning, k, doc, score)?;
            }
        }
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let pos: hashbrown::HashMap<u32, usize> = owning.iter().enumerate().map(|(i, &o)| (o, i)).collect();
        let mut per: Vec<Vec<u32>> = vec![Vec::new(); owning.len()];
        for (ord, (o, _)) in self.keys.iter().enumerate() {
            if let Some(&i) = pos.get(o) {
                per[i].push(ord as u32);
            }
        }
        if self.dense {
            for list in &mut per {
                list.sort_by(|&a, &b| self.keys[a as usize].1.cmp_key(&self.keys[b as usize].1));
            }
        }
        let scorer = self.scorer.clone().expect("mapped");
        let mut selected: Vec<(u32, Vec<(u32, SigBucket)>)> = Vec::with_capacity(owning.len());
        for (i, list) in per.into_iter().enumerate() {
            let own = owning[i];
            let subset_size = self.subset_sizes.get(own as usize).copied().unwrap_or(0);
            let less = |a: &(u32, SigBucket), b: &(u32, SigBucket)| a.1.score < b.1.score;
            let mut pq = LucenePq::new(list.len().min(self.shard_size));
            for ord in list {
                let doc_count = self.counts[ord as usize];
                if doc_count < self.shard_min_doc_count {
                    continue;
                }
                let key = self.keys[ord as usize].1.clone();
                let superset_df = self.bg_freq(&key)?;
                let score = scorer.score(doc_count as i64, subset_size as i64, superset_df as i64, self.proto.superset_size as i64)?;
                pq.insert_with_overflow((ord, SigBucket { key, subset_df: doc_count, superset_df, score, aggs: Aggs::default() }), &less);
            }
            let mut kept = pq.drain_ascending(&less);
            kept.reverse();
            selected.push((own, kept));
        }
        let all: Vec<u32> = selected.iter().flat_map(|(_, k)| k.iter().map(|x| x.0)).collect();
        let mut aggs = self.subs.build(&all)?.into_iter();
        let mut out = Vec::with_capacity(owning.len());
        for (own, kept) in selected {
            let buckets = kept
                .into_iter()
                .map(|(_, mut b)| {
                    b.aggs = aggs.next().unwrap_or_default();
                    b
                })
                .collect();
            out.push(AggValue::SignificantTerms(Box::new(SignificantTerms {
                subset_size: self.subset_sizes.get(own as usize).copied().unwrap_or(0),
                buckets,
                ..self.proto.clone()
            })));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        AggValue::SignificantTerms(Box::new(self.proto.clone()))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.vs.as_ref().and_then(|v| v.script.as_ref()).is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}

/// An unmapped field: no buckets (`UnmappedSignificantTerms`).
struct UnmappedSig {
    proto: SignificantTerms,
}

impl Aggregator for UnmappedSig {
    fn set_segment(&mut self, _seg: &SegmentContext) -> Result<()> {
        Ok(())
    }

    fn collect(&mut self, _b: Batch<'_>) -> Result<()> {
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning.iter().map(|_| self.build_empty()).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::SignificantTerms(Box::new(self.proto.clone()))
    }
}

/// Binds `significant_text`.
pub fn create_text(def: &AggDef, spec: &SigTextSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let c = &spec.common;
    check_sampled_thresholds(&def.name, c.min_doc_count, c.shard_min_doc_count, ctx)?;
    let base = |kind| SignificantTerms {
        kind,
        size: c.size,
        min_doc_count: c.min_doc_count,
        format: DocValueFormat::Raw,
        subset_size: 0,
        superset_size: 0,
        heuristic: c.heuristic.clone(),
        buckets: Vec::new(),
    };
    let Some(field) = spec.field.clone() else {
        return Ok(Box::new(UnmappedSig { proto: base(SigKind::Unmapped) }));
    };
    if ctx.lookup.field(&field)?.is_none() {
        return Ok(Box::new(UnmappedSig { proto: base(SigKind::Unmapped) }));
    }
    let hook = ctx.lookup.text_terms().ok_or_else(|| AggError::illegal_argument("[significant_text] needs the index's text analysis"))?;
    if !hook.analyzed(&field) {
        return Err(AggError::illegal_argument(format!("Field [{field}] has no analyzer, but SignificantText requires an analyzed field")));
    }
    let subs = Subs::bind(def, ctx)?;
    let mut shard_size = c.shard_size.unwrap_or_else(|| 2 * suggest_shard_size(c.size));
    if shard_size < c.size {
        shard_size = c.size;
    }
    let sfilter = match &c.include_exclude {
        Some(ie) => Some(ie.string_filter(&DocValueFormat::Raw)?),
        None => None,
    };
    let superset_query: QueryRef = match &c.background_filter {
        Some(q) => Arc::new(BooleanQuery::new().add(Occur::Filter, ctx.lookup.query(q)?).add(Occur::Filter, ctx.lookup.global_query())),
        None => ctx.lookup.global_query(),
    };
    let superset_size = match &ctx.sampling {
        Some(s) => s.count(ctx.searcher, &superset_query)?,
        None => ctx.searcher.count(&superset_query)?,
    };
    let bg_filter = if c.background_filter.is_some() || ctx.sampling.is_some() { Some(superset_query.clone()) } else { None };
    let scorer = Scorer::new(&c.heuristic, ctx.lookup.scripts().as_ref())?;
    let source_fields = spec.source_fields.clone().unwrap_or_else(|| vec![field.clone()]);
    Ok(Box::new(SigAgg {
        proto: SignificantTerms { superset_size, ..base(SigKind::Str) },
        text: Some(TextSource { hook, field, source_fields, spotters: spec.filter_duplicate_text.then(Vec::new), seg: None }),
        vs: None,
        sfilter,
        lfilter: None,
        dense: false,
        shard_size,
        shard_min_doc_count: c.shard_min_doc_count.min(c.min_doc_count),
        scorer: Some(scorer),
        field: None,
        bg_filter,
        sampling: ctx.sampling,
        searcher: ctx.searcher.clone(),
        bytes: None,
        nums: None,
        index: hashbrown::HashMap::new(),
        keys: Vec::new(),
        counts: Vec::new(),
        subset_sizes: Vec::new(),
        bg_cache: hashbrown::HashMap::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_runs_are_dropped() {
        let doc: Vec<Vec<u8>> = ["a", "b", "c", "d", "e", "f", "g"].iter().map(|s| s.as_bytes().to_vec()).collect();
        let mut sp = DupSpotter::default();
        assert_eq!(sp.filter(&doc).len(), 7);
        sp.start_new_sequence();
        // The same text again: every six-token run was seen, so nothing survives.
        assert!(sp.filter(&doc).is_empty());
        sp.start_new_sequence();
        let other: Vec<Vec<u8>> = ["x", "y", "z"].iter().map(|s| s.as_bytes().to_vec()).collect();
        assert_eq!(sp.filter(&other).len(), 3);
    }

    #[test]
    fn heuristics_score_like_elasticsearch() {
        let jlh = Scorer::Native(Heuristic::Jlh);
        // subset 10/100, superset 20/1000: (0.1 - 0.02) * (0.1 / 0.02).
        assert!((jlh.score(10, 100, 20, 1000).unwrap() - 0.4).abs() < 1e-12);
        let pct = Scorer::Native(Heuristic::Percentage);
        assert_eq!(pct.score(3, 10, 6, 100).unwrap(), 0.5);
        let chi = Scorer::Native(Heuristic::ChiSquare { include_negatives: false, background_is_superset: true });
        assert_eq!(
            chi.score(5, 2, 10, 20).unwrap_err().reason,
            "subsetFreq > subsetSize, in ChiSquare"
        );
    }
}

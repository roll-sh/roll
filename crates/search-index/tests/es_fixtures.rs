//! Scores, hit order and explanation trees against Elasticsearch 9.5.5, and highlighting
//! output, from fixtures captured by `tests/fixtures/gen_es_fixtures.py` on the reference
//! cluster. The same corpus is indexed into one segment, so Lucene doc ids (which appear in
//! explanations as `in N`) line up.

mod common;

use std::sync::Arc;

use search_analysis::{Analysis, Analyzer};
use search_codec::{DocValuesType, IndexOptions, PointsSpec};
use search_index::collector::TopScoreCollector;
use search_index::doc::{DocValue, IndexableDoc, IndexableField};
use search_index::highlight::{BoundaryScanner, Encoder, Fragmenter, HighlightOptions, HighlighterType};
use search_index::query::function_score::{
    CombineFunction, DecayFn, DecayOrigin, FunctionEntry, FunctionScoreMode, FunctionScoreQuery, Modifier, MultiValueMode,
    NumDecode, RandomField, ScoreFunction,
};
use search_index::query::intervals::IntervalsSource;
use search_index::query::mlt::{MoreLikeThis, MoreLikeThisQuery};
use search_index::query::{
    CoveringQuery, IntervalQuery, MinMatch,
    BooleanQuery, BoostQuery, ConstantScoreQuery, DisMaxQuery, DocValuesRangeQuery, DvRange, FieldExistsQuery,
    IndexOrDocValuesQuery, MatchAllQuery, MultiTermQuery, Occur, PhraseQuery, PointRangeQuery, SpanNode, SpanQuery,
    TermQuery,
};
use search_index::schema::FieldSchema;
use search_index::search::{ExplValue, Explanation, Searcher};
use search_index::similarity::{AfterEffect, BasicModel, Distribution, Independence, Lambda, Normalization, Similarity};
use search_index::{IndexOp, QueryRef, Schema, Shard};
use serde_json::Value;

const DOCS: &[&str] = &[
    "the quick brown fox jumps over the lazy dog",
    "quick quick fox",
    "lazy",
    "the fox and the hound are friends of the quick brown dog",
    "brown dogs and brown foxes jump quickly over lazy cats",
    "a quick movement of the enemy will jeopardize six gunboats",
    "fox fox fox fox fox",
    "the the the quick",
    "dog brown quick fox lazy",
    "over the hills and far away the quick fox ran",
];
const NUMS: &[i64] = &[5, 12, 7, 30, 1, 18, 25, 3, 9, 14];
const TAGS: &[&[&str]] = &[&["a"], &["b"], &["a", "c"], &[], &["c"], &["b", "d"], &["a"], &["d"], &["e"], &["a", "b"]];

const T: u32 = 10;
const K: u32 = 11;
const N: u32 = 12;
const G: u32 = 13;
const M: u32 = 14;
const MINS: &[Option<i64>] = &[Some(1), Some(2), Some(1), Some(0), Some(1), Some(2), None, Some(1), Some(3), Some(2)];
const GROUPS: &[Option<&str>] = &[Some("x"), Some("y"), Some("x"), Some("z"), Some("y"), Some("x"), None, Some("z"), Some("y"), Some("x")];

/// The fixture's Painless similarity scripts, in Rust.
struct Scripted(bool);

impl search_index::similarity::ScriptedSimilarity for Scripted {
    fn weight(&self, st: &search_index::similarity::ScriptStats) -> Option<f64> {
        self.0.then(|| (st.boost as f64) * (((st.doc_count as f64 + 1.0) / (st.doc_freq as f64 + 1.0)).ln() + 1.0))
    }
    fn score(&self, weight: f64, st: &search_index::similarity::ScriptStats, len: u32, freq: f32) -> f64 {
        let tf = (freq as f64).sqrt();
        let norm = 1.0 / (len as f64).sqrt();
        if self.0 {
            weight * tf * norm
        } else {
            let idf = ((st.doc_count as f64 + 1.0) / (st.doc_freq as f64 + 1.0)).ln() + 1.0;
            st.boost as f64 * tf * idf * norm
        }
    }
    fn describe(&self) -> String {
        let script = |s: &str| format!("Script{{type=inline, lang='painless', idOrCode='{s}', options={{}}, params={{}}}}");
        if self.0 {
            format!(
                "ScriptedSimilarity(weightScript=[{}], script=[{}])",
                script("double idf = Math.log((field.docCount+1.0)/(term.docFreq+1.0)) + 1.0; return query.boost * idf;"),
                script("double tf = Math.sqrt(doc.freq); double norm = 1/Math.sqrt(doc.length); return weight * tf * norm;")
            )
        } else {
            format!(
                "ScriptedSimilarity(weightScript=[null], script=[{}])",
                script("double tf = Math.sqrt(doc.freq); double idf = Math.log((field.docCount+1.0)/(term.docFreq+1.0)) + 1.0; double norm = 1/Math.sqrt(doc.length); return query.boost * tf * idf * norm;")
            )
        }
    }
}

fn sims() -> Vec<(&'static str, Similarity)> {
    let d = true;
    vec![
        ("bm25b", Similarity::Bm25 { k1: 1.6, b: 0.3, discount_overlaps: d, legacy: true }),
        ("dfr", Similarity::Dfr { basic_model: BasicModel::G, after_effect: AfterEffect::L, normalization: Normalization::H2(3.0), discount_overlaps: d }),
        ("dfr2", Similarity::Dfr { basic_model: BasicModel::IF, after_effect: AfterEffect::B, normalization: Normalization::H1(1.0), discount_overlaps: d }),
        ("dfr3", Similarity::Dfr { basic_model: BasicModel::In, after_effect: AfterEffect::L, normalization: Normalization::Z(0.3), discount_overlaps: d }),
        ("dfr4", Similarity::Dfr { basic_model: BasicModel::Ine, after_effect: AfterEffect::B, normalization: Normalization::H3(900.0), discount_overlaps: d }),
        ("ib", Similarity::Ib { distribution: Distribution::LL, lambda: Lambda::DF, normalization: Normalization::Z(0.25), discount_overlaps: d }),
        ("ib2", Similarity::Ib { distribution: Distribution::SPL, lambda: Lambda::TTF, normalization: Normalization::H2(1.0), discount_overlaps: d }),
        ("dfi", Similarity::Dfi { independence: Independence::ChiSquared, discount_overlaps: d }),
        ("dfi2", Similarity::Dfi { independence: Independence::Standardized, discount_overlaps: d }),
        ("lmd", Similarity::LmDirichlet { mu: 3.0, discount_overlaps: d }),
        ("lmj", Similarity::LmJelinekMercer { lambda: 0.4, discount_overlaps: d }),
        ("script", Similarity::Scripted(Arc::new(Scripted(false)))),
        ("script2", Similarity::Scripted(Arc::new(Scripted(true)))),
        ("boolean", Similarity::Boolean),
    ]
}

struct Env {
    searcher: Searcher,
    schema: Schema,
}

fn env() -> Env {
    env_impl(false)
}

fn env_with_groups() -> Env {
    env_impl(true)
}

fn env_impl(groups: bool) -> Env {
    let mut schema = Schema::new()
        .with_field(
            FieldSchema::new(T, "t")
                .indexed(IndexOptions::DocsAndFreqsAndPositions, true)
                .with_term_vectors(search_index::TermVectorOptions { positions: true, offsets: true, payloads: false }),
        )
        .unwrap()
        .with_field(FieldSchema::new(K, "k").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet))
        .unwrap()
        .with_field(
            FieldSchema::new(N, "n").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric),
        )
        .unwrap()
        .with_field(FieldSchema::new(G, "g").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet))
        .unwrap()
        .with_field(FieldSchema::new(M, "m").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap();
    for (i, (name, sim)) in sims().into_iter().enumerate() {
        schema
            .add_field(
                FieldSchema::new(20 + i as u32, format!("t_{name}"))
                    .indexed(IndexOptions::DocsAndFreqsAndPositions, true)
                    .with_similarity(sim),
            )
            .unwrap();
    }
    let shard = Shard::in_memory(schema.clone());
    let a = Analysis::global().analyzer("standard").unwrap();
    for (i, text) in DOCS.iter().enumerate() {
        let mut d = IndexableDoc::new(i.to_string().into_bytes()).text(T, &a, *text);
        for j in 0..sims().len() {
            d = d.text(20 + j as u32, &a, *text);
        }
        for t in TAGS[i] {
            d.fields.push(IndexableField::Term { field: K, term: t.as_bytes().to_vec().into() });
            d.fields.push(IndexableField::DocValue { field: K, value: DocValue::SortedSet(t.as_bytes().to_vec().into()) });
        }
        d.fields.push(IndexableField::Point { field: N, packed: search_codec::points::encode::i64_bytes(NUMS[i]).to_vec() });
        d.fields.push(IndexableField::DocValue { field: N, value: DocValue::SortedNumeric(NUMS[i]) });
        if groups && let Some(m) = MINS[i] {
            d.fields.push(IndexableField::Point { field: M, packed: search_codec::points::encode::i64_bytes(m).to_vec() });
            d.fields.push(IndexableField::DocValue { field: M, value: DocValue::SortedNumeric(m) });
        }
        if groups && let Some(g) = GROUPS[i] {
            d.fields.push(IndexableField::Term { field: G, term: g.as_bytes().to_vec().into() });
            d.fields.push(IndexableField::DocValue { field: G, value: DocValue::SortedSet(g.as_bytes().to_vec().into()) });
        }
        shard.index(IndexOp::new(d)).unwrap();
    }
    shard.refresh().unwrap();
    Env { searcher: shard.searcher(), schema }
}

fn field(s: &Schema, name: &str) -> u32 {
    s.by_name(name).unwrap_or_else(|| panic!("field {name}")).number
}

fn build(e: &Env, q: &Value) -> QueryRef {
    let o = q.as_object().unwrap();
    let (k, v) = o.iter().next().unwrap();
    let a = v.as_array();
    match k.as_str() {
        "term" => {
            let a = a.unwrap();
            Arc::new(TermQuery::new(field(&e.schema, a[0].as_str().unwrap()), a[1].as_str().unwrap().as_bytes().to_vec()))
        }
        "or" | "and" => {
            let mut b = BooleanQuery::new();
            for c in a.unwrap() {
                b = b.add(if k == "or" { Occur::Should } else { Occur::Must }, build(e, c));
            }
            Arc::new(b)
        }
        "msm" => {
            let a = a.unwrap();
            let mut b = BooleanQuery::new().with_msm(a[1].as_u64().unwrap() as usize);
            for c in a[0].as_array().unwrap() {
                b = b.should(build(e, c));
            }
            Arc::new(b)
        }
        "bool" => {
            let mut b = BooleanQuery::new();
            // Elasticsearch's clause order: must, must_not, should, filter.
            for (occ, key) in [(Occur::Must, "must"), (Occur::MustNot, "must_not"), (Occur::Should, "should"), (Occur::Filter, "filter")] {
                if let Some(cs) = v.get(key) {
                    for c in cs.as_array().unwrap() {
                        b = b.add(occ, build(e, c));
                    }
                }
            }
            if v.get("should").is_some() && v.get("filter").is_some() {
                b = b.with_msm(1);
            }
            Arc::new(b)
        }
        "phrase" => {
            let a = a.unwrap();
            let terms = a[1].as_array().unwrap().iter().map(|t| t.as_str().unwrap().as_bytes().to_vec()).collect();
            Arc::new(PhraseQuery::new(field(&e.schema, a[0].as_str().unwrap()), terms, a[2].as_u64().unwrap() as u32))
        }
        "boost" => {
            let a = a.unwrap();
            Arc::new(BoostQuery::new(build(e, &a[0]), a[1].as_f64().unwrap() as f32))
        }
        "const" => {
            let a = a.unwrap();
            Arc::new(BoostQuery::new(Arc::new(ConstantScoreQuery::new(build(e, &a[0]))), a[1].as_f64().unwrap() as f32))
        }
        "dismax" => {
            let a = a.unwrap();
            Arc::new(DisMaxQuery::new(a[0].as_array().unwrap().iter().map(|c| build(e, c)).collect(), a[1].as_f64().unwrap() as f32))
        }
        "prefix" => {
            let a = a.unwrap();
            Arc::new(MultiTermQuery::prefix(field(&e.schema, a[0].as_str().unwrap()), a[1].as_str().unwrap().as_bytes().to_vec()))
        }
        "wildcard" => {
            let a = a.unwrap();
            Arc::new(MultiTermQuery::wildcard(field(&e.schema, a[0].as_str().unwrap()), a[1].as_str().unwrap(), false).unwrap())
        }
        "fuzzy" => {
            let a = a.unwrap();
            Arc::new(MultiTermQuery::fuzzy(
                field(&e.schema, a[0].as_str().unwrap()),
                a[1].as_str().unwrap(),
                a[2].as_u64().unwrap() as u32,
                0,
                50,
                true,
            ))
        }
        "range" => {
            let a = a.unwrap();
            let f = field(&e.schema, a[0].as_str().unwrap());
            let (lo, hi) = (a[1].as_i64().unwrap(), a[2].as_i64().unwrap());
            Arc::new(IndexOrDocValuesQuery {
                index: Arc::new(PointRangeQuery::long(f, lo, hi)),
                dv: Arc::new(DocValuesRangeQuery { field: f, range: DvRange::Long { lower: lo, upper: hi } }),
            })
        }
        "set" => {
            let a = a.unwrap();
            Arc::new(MultiTermQuery::set(
                field(&e.schema, a[0].as_str().unwrap()),
                a[1].as_array().unwrap().iter().map(|t| t.as_str().unwrap().as_bytes().to_vec()),
            ))
        }
        "exists" => Arc::new(ConstantScoreQuery::new(Arc::new(FieldExistsQuery { field: field(&e.schema, v.as_str().unwrap()) }))),
        "all" => Arc::new(MatchAllQuery),
        "span" => Arc::new(SpanQuery::new(span(v)).unwrap()),
        "fs" => {
            let a = a.unwrap();
            let inner = build(e, &a[0]);
            let mut fns = Vec::new();
            for f in a[1].as_array().unwrap() {
                let filter = f.get("filter").map(|x| build(e, x));
                let weight = f.get("weight").map(|w| w.as_f64().unwrap() as f32);
                let function = if let Some(x) = f.get("fvf") {
                    let x = x.as_array().unwrap();
                    let modifier = match x[2].as_str().unwrap() {
                        "log1p" => Modifier::Log1p,
                        "none" => Modifier::None,
                        m => panic!("{m}"),
                    };
                    ScoreFunction::FieldValueFactor {
                        field: field(&e.schema, x[0].as_str().unwrap()),
                        name: x[0].as_str().unwrap().into(),
                        factor: x[1].as_f64().unwrap() as f32,
                        modifier,
                        missing: x[3].as_f64(),
                        decode: NumDecode::Long,
                    }
                } else if let Some(x) = f.get("decay") {
                    let x = x.as_array().unwrap();
                    let func = match x[0].as_str().unwrap() {
                        "gauss" => DecayFn::Gauss,
                        "exp" => DecayFn::Exp,
                        _ => DecayFn::Linear,
                    };
                    ScoreFunction::Decay {
                        field: field(&e.schema, x[1].as_str().unwrap()),
                        name: x[1].as_str().unwrap().into(),
                        func,
                        origin: DecayOrigin::Number(x[2].as_f64().unwrap(), NumDecode::Long),
                        scale: x[3].as_f64().unwrap(),
                        decay: x[4].as_f64().unwrap(),
                        offset: x[5].as_f64().unwrap(),
                        mode: MultiValueMode::Min,
                    }
                } else if let Some(x) = f.get("random") {
                    let x = x.as_array().unwrap();
                    let name = x[1].as_str().unwrap();
                    ScoreFunction::Random {
                        seed: x[0].as_i64().unwrap() as i32,
                        // Elasticsearch salts with (index name hash << 10) | shard id.
                        salt: java_hash("index-fx-main") << 10,
                        field: Some(RandomField::Number { field: field(&e.schema, name), name: name.into(), decode: NumDecode::Long }),
                    }
                } else {
                    ScoreFunction::Weight
                };
                fns.push(FunctionEntry { filter, function, weight });
            }
            let mut q = FunctionScoreQuery::new(inner, fns);
            q.score_mode = match a[2].as_str().unwrap() {
                "sum" => FunctionScoreMode::Sum,
                "avg" => FunctionScoreMode::Avg,
                _ => FunctionScoreMode::Multiply,
            };
            q.boost_mode = match a[3].as_str().unwrap() {
                "sum" => CombineFunction::Sum,
                "replace" => CombineFunction::Replace,
                _ => CombineFunction::Multiply,
            };
            q.min_score = a.get(4).and_then(|m| m.as_f64()).map(|m| m as f32);
            Arc::new(q)
        }
        "iv" => {
            let a = a.unwrap();
            let f = field(&e.schema, a[0].as_str().unwrap());
            Arc::new(IntervalQuery::new(f, iv(f, &a[1])))
        }
        "mlt" => mlt(e, v),
        "tset" => {
            let a = a.unwrap();
            let f = field(&e.schema, a[0].as_str().unwrap());
            Arc::new(CoveringQuery {
                queries: a[1].as_array().unwrap().iter().map(|t| Arc::new(TermQuery::new(f, t.as_str().unwrap().as_bytes().to_vec())) as QueryRef).collect(),
                min: MinMatch::Field { field: field(&e.schema, a[2].as_str().unwrap()), name: a[2].as_str().unwrap().into() },
            })
        }
        other => panic!("unknown query {other}"),
    }
}

fn analyze(text: &str) -> Vec<(Vec<u8>, u32, u32)> {
    let a = Analysis::global().analyzer("standard").unwrap();
    let mut out = Vec::new();
    a.analyze_values(&[text], &mut |t| out.push((t.term.as_bytes().to_vec(), t.position_increment, 1)));
    out
}

/// Elasticsearch's intervals rules over the fixture's `iv` description.
fn iv(f: u32, d: &Value) -> IntervalsSource {
    let (k, v) = d.as_object().unwrap().iter().next().unwrap();
    let a = v.as_array();
    match k.as_str() {
        "m" => {
            let a = a.unwrap();
            IntervalsSource::analyzed(&analyze(a[0].as_str().unwrap()), a[1].as_i64().unwrap() as i32, a[2].as_bool().unwrap()).unwrap()
        }
        "all_of" => {
            let a = a.unwrap();
            let subs: Vec<IntervalsSource> = a[0].as_array().unwrap().iter().map(|x| iv(f, x)).collect();
            let inner = if a[1].as_bool().unwrap() { IntervalsSource::ordered(subs) } else { IntervalsSource::unordered(subs) };
            match a[2].as_i64().unwrap() {
                -1 => inner,
                g => IntervalsSource::max_gaps(g as i32, inner),
            }
        }
        "any_of" => IntervalsSource::or(a.unwrap().iter().map(|x| iv(f, x)).collect()),
        "prefix" => {
            let p = v.as_str().unwrap();
            IntervalsSource::multi_term(MultiTermQuery::prefix(f, p.as_bytes().to_vec()), 128, format!("{p}*"))
        }
        "wildcard" => {
            let p = v.as_str().unwrap();
            IntervalsSource::multi_term(MultiTermQuery::wildcard(f, p, false).unwrap(), 128, p)
        }
        "fuzzy" => {
            let a = a.unwrap();
            let t = a[0].as_str().unwrap();
            IntervalsSource::multi_term(MultiTermQuery::fuzzy(f, t, a[1].as_u64().unwrap() as u32, 0, 128, true), 128, t)
        }
        "filter" => {
            let a = a.unwrap();
            let (input, flt) = (iv(f, &a[1]), iv(f, &a[2]));
            match a[0].as_str().unwrap() {
                "containing" => IntervalsSource::containing(input, flt),
                "contained_by" => IntervalsSource::contained_by(input, flt),
                "not_containing" => IntervalsSource::not_containing(input, flt),
                "not_contained_by" => IntervalsSource::not_contained_by(input, flt),
                "overlapping" => IntervalsSource::overlapping(input, flt),
                "not_overlapping" => IntervalsSource::non_overlapping(input, flt),
                "before" => IntervalsSource::before(input, flt),
                "after" => IntervalsSource::after(input, flt),
                x => panic!("{x}"),
            }
        }
        x => panic!("iv {x}"),
    }
}

/// `more_like_this` from the fixture's request body (like docs: their analyzed source).
fn mlt(e: &Env, v: &Value) -> QueryRef {
    let mut m = MoreLikeThis { fields: v["fields"].as_array().unwrap().iter().map(|f| field(&e.schema, f.as_str().unwrap())).collect(), ..Default::default() };
    if let Some(x) = v.get("min_term_freq") {
        m.min_term_freq = x.as_u64().unwrap() as u32;
    }
    if let Some(x) = v.get("min_doc_freq") {
        m.min_doc_freq = x.as_u64().unwrap();
    }
    if let Some(x) = v.get("max_doc_freq") {
        m.max_doc_freq = x.as_u64().unwrap();
    }
    if let Some(x) = v.get("max_query_terms") {
        m.max_query_terms = x.as_u64().unwrap() as usize;
    }
    if let Some(x) = v.get("boost_terms") {
        m.boost_terms = x.as_f64().unwrap() as f32;
    }
    if let Some(x) = v.get("minimum_should_match") {
        m.minimum_should_match = Some(x.as_str().unwrap().to_string());
    }
    let mut q = MoreLikeThisQuery { mlt: m, ..Default::default() };
    let likes: Vec<Value> = match &v["like"] {
        Value::Array(a) => a.clone(),
        x => vec![x.clone()],
    };
    let mut ids = Vec::new();
    for l in likes {
        match l {
            Value::String(t) => q.like_texts.push(analyze(&t).into_iter().map(|t| t.0).collect()),
            Value::Object(o) => {
                let id = o["_id"].as_str().unwrap().to_string();
                let mut tf: std::collections::BTreeMap<Vec<u8>, u32> = Default::default();
                for (t, ..) in analyze(DOCS[id.parse::<usize>().unwrap()]) {
                    *tf.entry(t).or_default() += 1;
                }
                q.like_docs.push(vec![(T, tf.into_iter().collect())]);
                ids.push(id);
            }
            _ => panic!(),
        }
    }
    if let Some(u) = v.get("unlike") {
        for (t, ..) in analyze(u.as_str().unwrap()) {
            for &f in &q.mlt.fields {
                q.skip.insert((f, t.clone()));
            }
        }
    }
    let mq: QueryRef = Arc::new(q);
    if ids.is_empty() || v.get("include").and_then(|x| x.as_bool()) == Some(true) {
        return mq;
    }
    let mut b = BooleanQuery::new().should(mq);
    for id in ids {
        b = b.must_not(Arc::new(TermQuery::new(search_index::schema::ID_FIELD, id.into_bytes())));
    }
    Arc::new(b)
}

fn java_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

fn span(v: &Value) -> SpanNode {
    let o = v.as_object().unwrap();
    let (k, x) = o.iter().next().unwrap();
    match k.as_str() {
        "t" => SpanNode::Term { field: T, term: x.as_str().unwrap().as_bytes().to_vec() },
        "near" => {
            let a = x.as_array().unwrap();
            SpanNode::Near {
                clauses: a[0].as_array().unwrap().iter().map(span).collect(),
                slop: a[1].as_u64().unwrap() as u32,
                in_order: a[2].as_bool().unwrap(),
            }
        }
        "or" => SpanNode::Or(x.as_array().unwrap().iter().map(span).collect()),
        "first" => {
            let a = x.as_array().unwrap();
            SpanNode::First { inner: Box::new(span(&a[0])), end: a[1].as_u64().unwrap() as u32 }
        }
        "not" => {
            let a = x.as_array().unwrap();
            SpanNode::Not {
                include: Box::new(span(&a[0])),
                exclude: Box::new(span(&a[1])),
                pre: a[2].as_u64().unwrap() as u32,
                post: a[3].as_u64().unwrap() as u32,
            }
        }
        "containing" => {
            let a = x.as_array().unwrap();
            SpanNode::Containing { big: Box::new(span(&a[0])), little: Box::new(span(&a[1])) }
        }
        "within" => {
            let a = x.as_array().unwrap();
            SpanNode::Within { big: Box::new(span(&a[0])), little: Box::new(span(&a[1])) }
        }
        other => panic!("span {other}"),
    }
}

/// Compares an explanation with Elasticsearch's JSON (`idf, sum of:` children in any order:
/// Elasticsearch orders them by Java hash order).
fn same_expl(ours: &Explanation, es: &Value, path: &str) -> Result<(), String> {
    let ev = es["value"].as_f64().unwrap();
    let ours_v = ours.value.as_f64();
    let integral = es["value"].is_i64() || es["value"].is_u64();
    let value_ok = if integral { ours.value.is_integral() && ours_v == ev } else { ours.value.as_f32() == ev as f32 };
    let desc = es["description"].as_str().unwrap();
    if !value_ok || ours.description != desc {
        return Err(format!(
            "{path}: ours {} = {:?} (integral {}) vs es {ev} = {desc:?}",
            ours.value.to_java_string(),
            ours.description,
            matches!(ours.value, ExplValue::Int(_) | ExplValue::Long(_))
        ));
    }
    let ed = es["details"].as_array().unwrap();
    if ed.len() != ours.details.len() {
        return Err(format!("{path} ({desc}): {} details vs es {}:\n{}", ours.details.len(), ed.len(), ours.to_text()));
    }
    if desc == "idf, sum of:" || desc.ends_with("required matches, sum of:") {
        let mut used = vec![false; ed.len()];
        for (i, d) in ours.details.iter().enumerate() {
            let j = (0..ed.len()).find(|&j| !used[j] && same_expl(d, &ed[j], "").is_ok());
            match j {
                Some(j) => used[j] = true,
                None => return Err(format!("{path}/{i}: idf detail without a match: {}", d.to_text())),
            }
        }
        return Ok(());
    }
    for (i, (d, x)) in ours.details.iter().zip(ed).enumerate() {
        same_expl(d, x, &format!("{path}/{i}"))?;
    }
    Ok(())
}

#[test]
fn search_scores_and_explanations_match_elasticsearch() {
    let e = env();
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/search.json")).unwrap();
    let ids: std::collections::HashMap<u32, String> = common::ids(&e.searcher);
    let mut failures = Vec::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let q = build(&e, &c["q"]);
        let mut col = TopScoreCollector::new(20, u64::MAX);
        e.searcher.search(&q, &mut col).unwrap();
        let td = col.top_docs();
        let want = c["hits"].as_array().unwrap();
        if td.total_hits != c["total"].as_u64().unwrap() {
            let ours: Vec<&String> = td.hits.iter().map(|h| &ids[&h.doc]).collect();
            let es: Vec<&str> = want.iter().map(|w| w["id"].as_str().unwrap()).collect();
            failures.push(format!("{name}: total {} vs {} (ours {ours:?}, es {es:?})", td.total_hits, c["total"]));
            continue;
        }
        for (i, (h, w)) in td.hits.iter().zip(want).enumerate() {
            let wid = w["id"].as_str().unwrap();
            let wscore = w["score"].as_f64().unwrap() as f32;
            if ids[&h.doc] != wid || h.score != wscore {
                failures.push(format!("{name}: hit {i} ours {}@{} vs es {wid}@{wscore}", ids[&h.doc], h.score));
                break;
            }
            let ex = e.searcher.explain(&q, h.doc).unwrap();
            if let Err(m) = same_expl(&ex, &w["explanation"], "") {
                failures.push(format!("{name} doc {wid}: {m}"));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn highlighting_matches_elasticsearch() {
    let _ = env();
    let a: Analyzer = Analysis::global().analyzer("standard").unwrap();
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/highlight.json")).unwrap();
    let mut failures = Vec::new();
    for c in &cases {
        let values: Vec<String> = c["values"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let refs: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
        let qd = &c["q"];
        let q = hl_query(qd);
        let o = &c["opts"];
        let opts = hl_opts(o);
        // The fixture runs the highlight query against one doc; a doc the query doesn't
        // match still gets no_match_size summaries, so compare highlight output directly.
        let got = search_index::highlight::highlight(&q, &refs, Some(&a), T, &opts).unwrap();
        let want: Vec<String> = match c["fragments"].as_array() {
            Some(f) => f.iter().map(|v| v.as_str().unwrap().to_string()).collect(),
            None => Vec::new(),
        };
        if got != want {
            failures.push(format!("{:?} q={qd} opts={o}\n  ours {got:?}\n  es   {want:?}", values));
        }
    }
    assert!(failures.is_empty(), "{} of {} highlight cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn phrase_highlighting_matches_elasticsearch() {
    // Sloppy and repeated-term phrases next to other clauses: the unified highlighter shows
    // only real phrase matches, the plain one Lucene's span-near reading of the phrase.
    let _ = env();
    let a: Analyzer = Analysis::global().analyzer("standard").unwrap();
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/highlight_phrases.json")).unwrap();
    let mut failures = Vec::new();
    for c in &cases {
        let values: Vec<String> = c["values"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let refs: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
        let (qd, o) = (&c["q"], &c["opts"]);
        let got = search_index::highlight::highlight(&hl_query(qd), &refs, Some(&a), T, &hl_opts(o)).unwrap();
        let want: Vec<String> = match c["fragments"].as_array() {
            Some(f) => f.iter().map(|v| v.as_str().unwrap().to_string()).collect(),
            None => Vec::new(),
        };
        if got != want {
            failures.push(format!("{values:?} q={qd} opts={o}\n  ours {got:?}\n  es   {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{} of {} phrase highlight cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

/// Rebuilds a highlight fixture query (`highlight_query` on field `t`).
fn hl_query(qd: &Value) -> QueryRef {
    let tq = |t: &Value| -> QueryRef { Arc::new(TermQuery::new(T, t.as_str().unwrap().as_bytes().to_vec())) };
    let terms = |ts: &Value| -> Vec<Vec<u8>> { ts.as_array().unwrap().iter().map(|t| t.as_str().unwrap().as_bytes().to_vec()).collect() };
    if let Some(cs) = qd.get("should") {
        let mut b = BooleanQuery::new();
        for c in cs.as_array().unwrap() {
            b = match c.get("term") {
                Some(t) => b.should(tq(t)),
                None => b.should(Arc::new(PhraseQuery::new(T, terms(&c["phrase"]), c["slop"].as_u64().unwrap() as u32))),
            };
        }
        Arc::new(b)
    } else if let Some(ts) = qd.get("terms") {
        let mut b = BooleanQuery::new();
        for t in ts.as_array().unwrap() {
            b = b.should(tq(t));
        }
        Arc::new(b)
    } else if let Some(ts) = qd.get("boosted") {
        let mut b = BooleanQuery::new();
        for t in ts.as_array().unwrap() {
            b = b.should(Arc::new(BoostQuery::new(tq(&t[0]), t[1].as_f64().unwrap() as f32)));
        }
        Arc::new(b)
    } else if let Some(ps) = qd.get("phrases") {
        let mut b = BooleanQuery::new();
        for p in ps.as_array().unwrap() {
            b = b.should(Arc::new(PhraseQuery::new(T, terms(p), 0)));
        }
        Arc::new(b)
    } else if let Some(ts) = qd.get("phrase") {
        let slop = qd.get("slop").and_then(|s| s.as_u64()).unwrap_or(0) as u32;
        Arc::new(PhraseQuery::new(T, terms(ts), slop))
    } else if let Some(f) = qd.get("fuzzy") {
        Arc::new(MultiTermQuery::fuzzy(T, f[0].as_str().unwrap(), f[1].as_u64().unwrap() as u32, 0, 50, true))
    } else if let Some(w) = qd.get("wildcard") {
        Arc::new(MultiTermQuery::wildcard(T, w.as_str().unwrap(), false).unwrap())
    } else {
        Arc::new(MultiTermQuery::prefix(T, qd["prefix"].as_str().unwrap().as_bytes().to_vec()))
    }
}

/// Rebuilds highlight options from the fixture's Elasticsearch field options.
fn hl_opts(o: &Value) -> HighlightOptions {
    let mut opts = HighlightOptions::default();
    let n = |k: &str| o.get(k).and_then(|v| v.as_u64()).map(|v| v as usize);
    if let Some(v) = n("fragment_size") {
        opts.fragment_size = v;
    }
    if let Some(v) = n("number_of_fragments") {
        opts.number_of_fragments = v;
    }
    if let Some(v) = n("no_match_size") {
        opts.no_match_size = v;
    }
    if let Some(v) = n("fragment_offset") {
        opts.fragment_offset = Some(v);
    }
    if let Some(v) = n("phrase_limit") {
        opts.phrase_limit = v;
    }
    if let Some(v) = n("boundary_max_scan") {
        opts.boundary_max_scan = v;
    }
    if let Some(v) = o.get("boundary_chars") {
        opts.boundary_chars = v.as_str().unwrap().into();
    }
    if o.get("order").is_some() {
        opts.order_by_score = true;
    }
    opts.boundary_scanner = match o.get("boundary_scanner").and_then(|v| v.as_str()) {
        Some("word") => Some(BoundaryScanner::Word),
        Some("sentence") => Some(BoundaryScanner::Sentence),
        Some("chars") => Some(BoundaryScanner::Chars),
        _ => None,
    };
    if o.get("encoder").is_some() {
        opts.encoder = Encoder::Html;
    }
    let tags = |v: &Value| -> Vec<String> { v.as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect() };
    if let Some(v) = o.get("pre_tags") {
        opts.pre_tags = tags(v);
        opts.post_tags = tags(&o["post_tags"]);
    }
    if o.get("tags_schema").is_some() {
        (opts.pre_tags, opts.post_tags) = HighlightOptions::styled_tags();
    }
    if o.get("type").is_some() {
        opts.kind = HighlighterType::Plain;
    }
    if o.get("fragmenter").is_some() {
        opts.fragmenter = Fragmenter::Simple;
    }
    opts
}

#[test]
fn fvh_matches_elasticsearch() {
    let _ = env();
    let a: Analyzer = Analysis::global().analyzer("standard").unwrap();
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/fvh.json")).unwrap();
    // The fixture index held one doc per distinct value list; multi-term queries expand
    // against its terms (which numbers clauses for styled tags).
    let shard = Shard::in_memory(
        Schema::new()
            .with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
            .unwrap(),
    );
    let mut seen: Vec<&Value> = Vec::new();
    for c in &cases {
        if !seen.contains(&&c["values"]) {
            seen.push(&c["values"]);
            let mut d = IndexableDoc::new(seen.len().to_string().into_bytes());
            for v in c["values"].as_array().unwrap() {
                d = d.text(T, &a, v.as_str().unwrap());
            }
            shard.index(IndexOp::new(d)).unwrap();
        }
    }
    shard.refresh().unwrap();
    let searcher = shard.searcher();
    let mut failures = Vec::new();
    for c in &cases {
        let values: Vec<String> = c["values"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let refs: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
        let (qd, o) = (&c["q"], &c["opts"]);
        let q = hl_query(qd);
        let mut opts = hl_opts(o);
        opts.kind = HighlighterType::Fvh;
        opts.term_vectors_with_offsets = true;
        opts.field_name = "t".into();
        let got = search_index::highlight::highlight_in(&searcher, &q, &refs, Some(&a), T, &opts);
        let ok = match (&got, c.get("error")) {
            (Err(search_index::Error::Es { kind, reason, .. }), Some(e)) => kind == e[0].as_str().unwrap() && reason == e[1].as_str().unwrap(),
            (Ok(got), None) => {
                let want: Vec<String> = match c["fragments"].as_array() {
                    Some(f) => f.iter().map(|v| v.as_str().unwrap().to_string()).collect(),
                    None => Vec::new(),
                };
                *got == want
            }
            _ => false,
        };
        if !ok {
            failures.push(format!("{:?} q={qd} opts={o}\n  ours {got:?}\n  es   {}", values, c.get("fragments").or(c.get("error")).unwrap()));
        }
    }
    assert!(failures.is_empty(), "{} of {} fvh cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn rescore_collapse_and_terminate_after_match_elasticsearch() {
    use search_index::collector::{CollapseCollector, CollapseField, FieldSort, Missing, QueryRescorer, RescoreMode, SortKey, SortMode,
        SortType, SortValue, TerminateAfter};
    let e = env_with_groups();
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/extra.json")).unwrap();
    let ids = common::ids(&e.searcher);
    let mut failures = Vec::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let want = c["hits"].as_array().unwrap();
        let q = build(&e, &c["q"]);
        match c["kind"].as_str().unwrap() {
            "rescore" => {
                let mut col = TopScoreCollector::new(20.max(c["window"].as_u64().unwrap() as usize), u64::MAX);
                e.searcher.search(&q, &mut col).unwrap();
                let first = col.top_docs();
                let mut r = QueryRescorer::new(build(&e, &c["rq"]));
                r.window_size = c["window"].as_u64().unwrap() as usize;
                r.query_weight = c["qw"].as_f64().unwrap() as f32;
                r.rescore_query_weight = c["rqw"].as_f64().unwrap() as f32;
                r.mode = RescoreMode::parse(c["mode"].as_str().unwrap()).unwrap();
                let window: Vec<u32> = first.hits.iter().take(r.window_size).map(|h| h.doc).collect();
                let td = r.rescore(&e.searcher, first).unwrap();
                for (i, (h, w)) in td.hits.iter().zip(want).enumerate() {
                    let (wid, ws) = (w["id"].as_str().unwrap(), w["score"].as_f64().unwrap() as f32);
                    if ids[&h.doc] != wid || h.score != ws {
                        failures.push(format!("{name}: hit {i} ours {}@{} vs es {wid}@{ws}", ids[&h.doc], h.score));
                        break;
                    }
                    let ex = r.explain(&e.searcher, h.doc, e.searcher.explain(&q, h.doc).unwrap(), window.contains(&h.doc)).unwrap();
                    if let Err(m) = same_expl(&ex, &w["explanation"], "") {
                        failures.push(format!("{name} doc {wid}: {m}"));
                        break;
                    }
                }
            }
            "collapse" => {
                let keys = match c["sort"].as_str() {
                    None => Vec::new(),
                    Some(s) => vec![SortKey::Field(FieldSort {
                        field: N,
                        kind: SortType::Long,
                        reverse: s.starts_with('-'),
                        missing: Missing::Last,
                        mode: if s.starts_with('-') { SortMode::Max } else { SortMode::Min },
                        nested: None,
                    })],
                };
                let size = c.get("size").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let mut col = CollapseCollector::new(CollapseField::Keyword(G), keys, size, 10_000, false);
                e.searcher.search(&q, &mut col).unwrap();
                let td = col.top_docs();
                if td.total_hits != c["total"].as_u64().unwrap() || td.hits.len() != want.len() {
                    failures.push(format!("{name}: total {} / {} hits vs {} / {}", td.total_hits, td.hits.len(), c["total"], want.len()));
                    continue;
                }
                for (i, (h, w)) in td.hits.iter().zip(want).enumerate() {
                    let wid = w["id"].as_str().unwrap();
                    let group = match &w["fields"]["g"][0] {
                        Value::String(g) => SortValue::Bytes(g.as_bytes().to_vec()),
                        _ => SortValue::Null,
                    };
                    let score_ok = w["score"].as_f64().is_none_or(|s| s as f32 == h.score);
                    let sort_ok = w["sort"].as_array().is_none_or(|s| h.values == vec![SortValue::Long(s[0].as_i64().unwrap())]);
                    if ids[&h.doc] != wid || !score_ok || !sort_ok || h.group != group {
                        failures.push(format!("{name}: hit {i} ours {}@{} {:?} {:?} vs es {w}", ids[&h.doc], h.score, h.values, h.group));
                        break;
                    }
                }
            }
            "terminate" => {
                let mut col = TerminateAfter::new(TopScoreCollector::new(20, 10_000), c["max"].as_u64().unwrap());
                e.searcher.search(&q, &mut col).unwrap();
                let terminated = col.terminated_early;
                let td = col.inner.top_docs();
                // Elasticsearch reports the cheap count as the total when it has one.
                let total = e.searcher.count_if_cheap(&q).unwrap().unwrap_or(td.total_hits);
                let got: Vec<&str> = td.hits.iter().map(|h| ids[&h.doc].as_str()).collect();
                let exp: Vec<&str> = want.iter().map(|w| w["id"].as_str().unwrap()).collect();
                if got != exp || total != c["total"].as_u64().unwrap() || terminated != c["terminated_early"].as_bool().unwrap() {
                    failures.push(format!("{name}: ours {got:?} total {total} terminated {terminated} vs es {exp:?} {} {}", c["total"], c["terminated_early"]));
                }
            }
            "search" => {
                let mut col = TopScoreCollector::new(20, u64::MAX);
                e.searcher.search(&q, &mut col).unwrap();
                let td = col.top_docs();
                let rewritten = e.searcher.rewrite(&q).unwrap().lucene_string(&e.schema);
                let want_rw = c["rewritten"].as_str().unwrap_or("");
                let cut = |s: &str| s.split(" -_id:").next().unwrap().to_string();
                if cut(&rewritten) != cut(want_rw) {
                    failures.push(format!("{name}: rewritten ours {rewritten:?} vs es {want_rw:?}"));
                }
                if td.total_hits != c["total"].as_u64().unwrap() {
                    let ours: Vec<&String> = td.hits.iter().map(|h| &ids[&h.doc]).collect();
                    failures.push(format!("{name}: total {} vs {} (ours {ours:?})", td.total_hits, c["total"]));
                    continue;
                }
                for (i, (h, w)) in td.hits.iter().zip(want).enumerate() {
                    let (wid, ws) = (w["id"].as_str().unwrap(), w["score"].as_f64().unwrap() as f32);
                    if ids[&h.doc] != wid || h.score != ws {
                        failures.push(format!("{name}: hit {i} ours {}@{} vs es {wid}@{ws}", ids[&h.doc], h.score));
                        break;
                    }
                    let ex = e.searcher.explain(&q, h.doc).unwrap();
                    if let Err(m) = same_expl(&ex, &w["explanation"], "") {
                        failures.push(format!("{name} doc {wid}: {m}"));
                        break;
                    }
                }
            }
            k => panic!("{k}"),
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn term_vectors_and_statistics_match_elasticsearch() {
    let e = env();
    let ids = common::ids(&e.searcher);
    let by_id: std::collections::HashMap<&str, u32> = ids.iter().map(|(d, i)| (i.as_str(), *d)).collect();
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/termvectors.json")).unwrap();
    for c in &cases {
        let doc = by_id[c["id"].as_str().unwrap()];
        let tvs = e.searcher.term_vectors(doc, Some(&[T]), true, true).unwrap();
        assert_eq!(tvs.len(), 1);
        let tv = &tvs[0];
        let want = &c["tv"];
        let fs = tv.field_stats.unwrap();
        assert_eq!(fs.sum_doc_freq, want["field_statistics"]["sum_doc_freq"].as_u64().unwrap());
        assert_eq!(fs.doc_count, want["field_statistics"]["doc_count"].as_u64().unwrap());
        assert_eq!(fs.sum_total_term_freq, want["field_statistics"]["sum_ttf"].as_u64().unwrap());
        let terms = want["terms"].as_object().unwrap();
        assert_eq!(tv.vector.terms.len(), terms.len());
        for (t, st) in tv.vector.terms.iter().zip(&tv.term_stats) {
            let w = &terms[std::str::from_utf8(&t.term).unwrap()];
            assert_eq!(st.doc_freq, w["doc_freq"].as_u64().unwrap());
            assert_eq!(st.total_term_freq, w["ttf"].as_u64().unwrap());
            assert_eq!(t.freq as u64, w["term_freq"].as_u64().unwrap());
            let toks: Vec<(u32, u32, u32)> = t.tokens.iter().map(|k| (k.position.unwrap(), k.start_offset.unwrap(), k.end_offset.unwrap())).collect();
            let wt: Vec<(u32, u32, u32)> = w["tokens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| (k["position"].as_u64().unwrap() as u32, k["start_offset"].as_u64().unwrap() as u32, k["end_offset"].as_u64().unwrap() as u32))
                .collect();
            assert_eq!(toks, wt);
        }
    }
}

fn es_sort_keys(e: &Env, sort: &Value) -> Vec<search_index::collector::SortKey> {
    use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, SortValue};
    sort.as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let (name, spec) = s.as_object().unwrap().iter().next().unwrap();
            let (order, mode, missing) = match spec {
                Value::String(o) => (o.as_str(), None, None),
                o => (o["order"].as_str().unwrap_or("asc"), o.get("mode").and_then(|m| m.as_str()), o.get("missing")),
            };
            let reverse = order == "desc";
            if name == "_score" {
                return SortKey::Score { reverse: !reverse };
            }
            let f = field(&e.schema, name);
            let kind = if name == "k" { SortType::Keyword } else { SortType::Long };
            let missing = match missing {
                None => Missing::Last,
                Some(Value::String(m)) if m == "_first" => Missing::First,
                Some(Value::String(_)) => Missing::Last,
                Some(v) => Missing::Value(SortValue::Long(v.as_i64().unwrap())),
            };
            let mode = match mode {
                Some("max") => SortMode::Max,
                Some("min") => SortMode::Min,
                Some("avg") => SortMode::Avg,
                Some("sum") => SortMode::Sum,
                Some("median") => SortMode::Median,
                _ => if reverse { SortMode::Max } else { SortMode::Min },
            };
            SortKey::Field(FieldSort { field: f, kind, reverse, missing, mode, nested: None })
        })
        .collect()
}

fn es_value(v: &search_index::collector::SortValue) -> Value {
    use search_index::collector::SortValue;
    match v {
        SortValue::Long(x) => Value::from(*x),
        SortValue::Double(x) => Value::from(*x),
        SortValue::Float(x) => Value::from(*x as f64),
        SortValue::Bytes(b) => Value::from(String::from_utf8_lossy(b).into_owned()),
        SortValue::Null => Value::Null,
    }
}

#[test]
fn sorts_and_search_after_match_elasticsearch() {
    use search_index::collector::TopFieldCollector;
    let e = env_with_groups();
    let ids = common::ids(&e.searcher);
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/sorts.json")).unwrap();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let keys = es_sort_keys(&e, &c["sort"]);
        let q: QueryRef = if c["q"].get("match_all").is_some() {
            Arc::new(MatchAllQuery)
        } else {
            let mut b = BooleanQuery::new();
            for w in ["quick", "fox", "the"] {
                b = b.should(Arc::new(TermQuery::new(T, w.as_bytes().to_vec())));
            }
            Arc::new(b)
        };
        let mut col = TopFieldCollector::new(keys.clone(), 20, u64::MAX, true).with_searcher(&e.searcher);
        e.searcher.search(&q, &mut col).unwrap();
        let got = col.top_docs();
        let want = c["hits"].as_array().unwrap();
        assert_eq!(got.hits.len(), want.len(), "{name}");
        for (h, w) in got.hits.iter().zip(want) {
            assert_eq!(ids[&h.doc], w["id"].as_str().unwrap(), "{name}");
            let vals: Vec<Value> = h.values.iter().map(es_value).collect();
            let wv = w["sort"].as_array().unwrap();
            for (a, b) in vals.iter().zip(wv) {
                let same = match (a.as_f64(), b.as_f64()) {
                    (Some(x), Some(y)) => (x as f32) == (y as f32) || x == y,
                    _ => a == b,
                };
                assert!(same, "{name}: sort values {vals:?} vs {wv:?}");
            }
        }
        if let Some(after) = c["after"].as_array() {
            let values: Vec<search_index::collector::SortValue> = got.hits[3].values.clone();
            assert_eq!(values.iter().map(es_value).map(|v| v.as_f64().map(|x| x as f32)).collect::<Vec<_>>(),
                after.iter().map(|v| v.as_f64().map(|x| x as f32)).collect::<Vec<_>>(), "{name}: after values");
            let mut col = TopFieldCollector::new(keys, 20, u64::MAX, true).with_searcher(&e.searcher).with_after(values, None);
            e.searcher.search(&q, &mut col).unwrap();
            let got: Vec<&str> = col.top_docs().hits.iter().map(|h| ids[&h.doc].as_str()).collect();
            let want: Vec<&str> = c["after_hits"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
            assert_eq!(got, want, "{name}: search_after");
        }
    }
}

#[test]
fn nested_queries_and_sorts_match_elasticsearch() {
    use search_index::collector::{FieldSort, Missing, NestedSort, SortKey, SortMode, SortType, SortValue, TopFieldCollector};
    use search_index::query::{JoinScoreMode, ToParentQuery, nested::root_filter};
    const TITLE: u32 = 10;
    const CTEXT: u32 = 11;
    const CSTARS: u32 = 12;
    let docs: &[&[(&str, i64)]] = &[
        &[("great quick product", 5), ("slow delivery", 2)],
        &[("quick and nice", 4)],
        &[],
        &[("terrible", 1), ("quick quick fix", 3), ("nice nice", 5)],
        &[("nice", 2), ("ok", 3)],
        &[("quick", 1)],
    ];
    let schema = Schema::new()
        .with_field(FieldSchema::new(TITLE, "title").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet))
        .unwrap()
        .with_field(FieldSchema::new(CTEXT, "comments.text").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap()
        .with_field(FieldSchema::new(CSTARS, "comments.stars").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap();
    let shard = Shard::in_memory(schema);
    let a = Analysis::global().analyzer("standard").unwrap();
    for (i, cs) in docs.iter().enumerate() {
        let mut d = IndexableDoc::new(i.to_string().into_bytes()).term(TITLE, format!("doc{i}").into_bytes());
        d = d.doc_value(TITLE, DocValue::SortedSet(format!("doc{i}").into_bytes().into()));
        for (t, s) in cs.iter() {
            let c = IndexableDoc::new(Vec::<u8>::new())
                .text(CTEXT, &a, *t)
                .point(CSTARS, search_codec::points::encode::i64_bytes(*s).to_vec())
                .doc_value(CSTARS, DocValue::SortedNumeric(*s));
            d = d.child(c);
        }
        shard.index(IndexOp::new(d)).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let ids: std::collections::HashMap<u32, String> = {
        let q: QueryRef = root_filter();
        let mut c = TopScoreCollector::new(100, u64::MAX);
        s.search(&q, &mut c).unwrap();
        c.top_docs()
            .hits
            .iter()
            .map(|h| {
                let f = s.stored(h.doc).unwrap();
                let id = f.iter().find(|(n, _)| *n == search_index::schema::ID_FIELD).unwrap();
                let search_codec::stored::OwnedStoredValue::Bytes(b) = &id.1 else { panic!() };
                (h.doc, String::from_utf8(b.clone()).unwrap())
            })
            .collect()
    };
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/nested.json")).unwrap();
    let child_q = || -> QueryRef {
        Arc::new(BooleanQuery::new().should(Arc::new(TermQuery::new(CTEXT, b"quick".to_vec()))).should(Arc::new(TermQuery::new(CTEXT, b"nice".to_vec()))))
    };
    let mut failures = Vec::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let want = c["hits"].as_array().unwrap();
        if let Some(mode) = c["mode"].as_str() {
            let mode = match mode {
                "avg" => JoinScoreMode::Avg,
                "max" => JoinScoreMode::Max,
                "min" => JoinScoreMode::Min,
                "sum" => JoinScoreMode::Total,
                _ => JoinScoreMode::None,
            };
            let q: QueryRef = Arc::new(ToParentQuery { child: child_q(), parent_filter: root_filter(), score_mode: mode });
            let mut col = TopScoreCollector::new(20, u64::MAX);
            s.search(&q, &mut col).unwrap();
            let td = col.top_docs();
            assert_eq!(td.total_hits, c["total"].as_u64().unwrap(), "{name}");
            for (h, w) in td.hits.iter().zip(want) {
                let (wid, ws) = (w["id"].as_str().unwrap(), w["score"].as_f64().unwrap() as f32);
                if ids[&h.doc] != wid || h.score != ws {
                    failures.push(format!("{name}: ours {}@{} vs es {wid}@{ws}", ids[&h.doc], h.score));
                    break;
                }
                if let Err(m) = same_expl(&s.explain(&q, h.doc).unwrap(), &w["explanation"], "") {
                    failures.push(format!("{name} doc {wid}: {m}"));
                    break;
                }
            }
        } else {
            let spec = &c["sort"][0]["comments.stars"];
            let reverse = spec["order"].as_str() == Some("desc");
            let mode = match spec["mode"].as_str().unwrap() {
                "min" => SortMode::Min,
                "max" => SortMode::Max,
                "avg" => SortMode::Avg,
                _ => SortMode::Sum,
            };
            let missing = match spec.get("missing") {
                Some(v) => Missing::Value(SortValue::Long(v.as_i64().unwrap())),
                None => Missing::Last,
            };
            let child_filter: QueryRef = match spec["nested"].get("filter") {
                Some(_) => Arc::new(TermQuery::new(CTEXT, b"quick".to_vec())),
                None => Arc::new(search_index::query::FieldExistsQuery { field: CSTARS }),
            };
            let key = SortKey::Field(FieldSort {
                field: CSTARS,
                kind: SortType::Long,
                reverse,
                missing,
                mode,
                nested: Some(NestedSort { parent_filter: root_filter(), child_filter, max_children: None }),
            });
            let q: QueryRef = root_filter();
            let mut col = TopFieldCollector::new(vec![key], 20, u64::MAX, false).with_searcher(&s);
            s.search(&q, &mut col).unwrap();
            let got: Vec<(String, Value)> = col.top_docs().hits.iter().map(|h| (ids[&h.doc].clone(), es_value(&h.values[0]))).collect();
            let exp: Vec<(String, Value)> = want.iter().map(|w| (w["id"].as_str().unwrap().to_string(), w["sort"][0].clone())).collect();
            if got != exp {
                failures.push(format!("{name}: ours {got:?} vs es {exp:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Intervals `match` rules analyzed with a synonym_graph analyzer (token graphs split at
/// articulation points, side paths as disjunctions of phrases): Lucene's query string, hits
/// and scores.
#[test]
fn interval_token_graphs_match_elasticsearch() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/ivgraph.json")).unwrap();
    let analysis = Analysis::from_settings(&fx["analysis"], &Default::default()).unwrap();
    let sg = analysis.analyzer("sg").unwrap();
    let std_a = Analysis::global().analyzer("standard").unwrap();
    let schema = Schema::new().with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqsAndPositions, true)).unwrap();
    let shard = Shard::in_memory(schema.clone());
    for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
        shard.index(IndexOp::new(IndexableDoc::new(i.to_string().into_bytes()).text(T, &std_a, d.as_str().unwrap()))).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let ids = common::ids(&s);
    let mut failures = Vec::new();
    for c in fx["cases"].as_array().unwrap() {
        let text = c["text"].as_str().unwrap();
        let mut toks = Vec::new();
        sg.analyze_values(&[text], &mut |t| toks.push((t.term.as_bytes().to_vec(), t.position_increment, t.position_length)));
        let src = IntervalsSource::analyzed(&toks, c["max_gaps"].as_i64().unwrap() as i32, c["ordered"].as_bool().unwrap()).unwrap();
        let q: QueryRef = Arc::new(IntervalQuery::new(T, src));
        let lucene = q.lucene_string(&schema);
        let mut col = TopScoreCollector::new(20, u64::MAX);
        s.search(&q, &mut col).unwrap();
        let got: Vec<(String, f32)> = col.top_docs().hits.iter().map(|h| (ids[&h.doc].clone(), h.score)).collect();
        let want: Vec<(String, f32)> =
            c["hits"].as_array().unwrap().iter().map(|h| (h["id"].as_str().unwrap().to_string(), h["score"].as_f64().unwrap() as f32)).collect();
        if lucene != c["lucene"].as_str().unwrap() || got != want {
            failures.push(format!("{text:?}: ours {lucene} {got:?}\n  es {} {want:?}", c["lucene"]));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Feature queries (`rank_feature`, `sparse_vector`) and BM25F (`combined_fields`).
mod features {
    use super::*;
    use search_index::doc::PreToken;
    use search_index::query::feature::{FeatureQuery, feature_token};
    use search_index::query::CombinedFieldQuery;

    const T: u32 = 10;
    const T2: u32 = 11;
    const FEATURE: u32 = 12;
    const TOPICS: u32 = 13;
    const SV: u32 = 14;

    fn feature_fields(tokens: Vec<PreToken>, field: u32) -> Option<IndexableField> {
        (!tokens.is_empty()).then_some(IndexableField::Tokens { field, tokens })
    }

    fn env(docs: &[Value]) -> (Searcher, Schema) {
        let freqs = |n, name: &str| FieldSchema::new(n, name).indexed(IndexOptions::DocsAndFreqs, false);
        let schema = Schema::new()
            .with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
            .unwrap()
            .with_field(FieldSchema::new(T2, "t2").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
            .unwrap()
            .with_field(freqs(FEATURE, "_feature"))
            .unwrap()
            .with_field(freqs(TOPICS, "topics"))
            .unwrap()
            .with_field(freqs(SV, "sv"))
            .unwrap();
        let shard = Shard::in_memory(schema.clone());
        let a = Analysis::global().analyzer("standard").unwrap();
        for (i, d) in docs.iter().enumerate() {
            let mut doc = IndexableDoc::new(es_numeric_id(&i.to_string()));
            for (f, name) in [(T, "t"), (T2, "t2")] {
                if let Some(v) = d[name].as_str() {
                    doc = doc.text(f, &a, v);
                }
            }
            // rank_feature: one `_feature` term per field; `len` has a negative score impact
            // (Elasticsearch indexes 1 / value).
            let mut feats = Vec::new();
            if let Some(v) = d["pr"].as_f64() {
                feats.push(feature_token("_feature", "pr", v as f32).unwrap());
            }
            if let Some(v) = d["len"].as_f64() {
                feats.push(feature_token("_feature", "len", 1f32 / v as f32).unwrap());
            }
            let map = |name: &str| -> Vec<PreToken> {
                d[name].as_object().map_or_else(Vec::new, |m| {
                    m.iter().map(|(k, v)| feature_token(name, k.clone().into_bytes(), v.as_f64().unwrap() as f32).unwrap()).collect()
                })
            };
            for f in [feature_fields(feats, FEATURE), feature_fields(map("topics"), TOPICS), feature_fields(map("sv"), SV)]
                .into_iter()
                .flatten()
            {
                doc.fields.push(f);
            }
            shard.index(IndexOp::new(doc)).unwrap();
        }
        shard.refresh().unwrap();
        (shard.searcher(), schema)
    }

    fn f32_of(v: &Value) -> f32 {
        v.as_f64().unwrap() as f32
    }

    /// Elasticsearch's `Uid.encodeId` for numeric ids: a marker byte, then two digits per byte
    /// (`0xf` ends an odd count).
    fn es_numeric_id(id: &str) -> Vec<u8> {
        let d: Vec<u8> = id.bytes().map(|c| c - b'0').collect();
        let mut out = vec![0xfe];
        for pair in d.chunks(2) {
            out.push(pair[0] << 4 | pair.get(1).copied().unwrap_or(0x0f));
        }
        out
    }

    /// Elasticsearch's query builders, for the fixture's queries.
    fn build(s: &Searcher, q: &Value) -> QueryRef {
        let (kind, body) = q.as_object().unwrap().iter().next().unwrap();
        let boost = body.get("boost").map_or(1.0, f32_of);
        let wrap = |q: QueryRef| -> QueryRef { if boost != 1.0 { Arc::new(BoostQuery::new(q, boost)) } else { q } };
        match kind.as_str() {
            "rank_feature" => {
                let name = body["field"].as_str().unwrap();
                let (field, feature) = match name.split_once('.') {
                    Some((_, feat)) => (TOPICS, feat.to_string()),
                    None => (FEATURE, name.to_string()),
                };
                let q = if let Some(p) = body.get("saturation") {
                    FeatureQuery::saturation(field, feature, 1.0, p.get("pivot").map(f32_of))
                } else if let Some(p) = body.get("log") {
                    FeatureQuery::log(field, feature, 1.0, f32_of(&p["scaling_factor"]))
                } else if let Some(p) = body.get("sigmoid") {
                    FeatureQuery::sigmoid(field, feature, 1.0, f32_of(&p["pivot"]), f32_of(&p["exponent"]))
                } else if body.get("linear").is_some() {
                    FeatureQuery::linear(field, feature, 1.0)
                } else {
                    FeatureQuery::saturation(field, feature, 1.0, None)
                };
                wrap(q.unwrap())
            }
            "sparse_vector" => {
                let mut b = BooleanQuery::new().with_msm(1);
                for (tok, w) in body["query_vector"].as_object().unwrap() {
                    // Elasticsearch drops tokens the index doesn't have.
                    if s.term_stats(SV, tok.as_bytes()).doc_freq == 0 {
                        continue;
                    }
                    b = b.should(Arc::new(BoostQuery::new(FeatureQuery::linear(SV, tok.clone().into_bytes(), 1.0).unwrap(), f32_of(w))));
                }
                wrap(Arc::new(b))
            }
            "combined_fields" => {
                let fields: Vec<(u32, f32)> = body["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|f| {
                        let f = f.as_str().unwrap();
                        let (n, w) = f.split_once('^').map_or((f, 1.0), |(n, w)| (n, w.parse().unwrap()));
                        (if n == "t" { T } else { T2 }, w)
                    })
                    .collect();
                let terms: Vec<Vec<u8>> = analyze(body["query"].as_str().unwrap()).into_iter().map(|t| t.0).collect();
                let and = body.get("operator").is_some_and(|o| o == "and");
                let mut qs: Vec<QueryRef> =
                    terms.iter().map(|t| Arc::new(CombinedFieldQuery::new(&fields, t.clone()).unwrap()) as QueryRef).collect();
                let q: QueryRef = if qs.len() == 1 {
                    qs.pop().unwrap()
                } else {
                    Arc::new(qs.into_iter().fold(BooleanQuery::new(), |b, q| if and { b.must(q) } else { b.should(q) }))
                };
                wrap(q)
            }
            "match_phrase_prefix" => {
                let (fname, v) = body.as_object().unwrap().iter().next().unwrap();
                let field = if fname == "t" { T } else { T2 };
                let (text, slop, max) = match v {
                    Value::String(t) => (t.as_str(), 0, 50),
                    o => (
                        o["query"].as_str().unwrap(),
                        o.get("slop").map_or(0, |x| x.as_u64().unwrap() as u32),
                        o.get("max_expansions").map_or(50, |x| x.as_u64().unwrap() as usize),
                    ),
                };
                let toks = analyze(text);
                let mut pos = Vec::new();
                let mut p = -1i64;
                for t in &toks {
                    p += t.1 as i64;
                    pos.push(p as u32);
                }
                let terms = toks.into_iter().map(|t| vec![t.0]).collect();
                Arc::new(search_index::query::MultiPhrasePrefixQuery::new(field, terms, pos, slop, max).unwrap())
            }
            "pinned" => {
                use search_index::query::CappedScoreQuery;
                // Elasticsearch's PinnedQueryBuilder: pins score above any capped organic score.
                let max_organic = f32::from_bits(0xfe << 23) - 1.0;
                let organic: QueryRef = {
                    let t = body["organic"]["match"]["t"].as_str().unwrap();
                    Arc::new(TermQuery::new(T, t.as_bytes().to_vec()))
                };
                let capped: QueryRef = Arc::new(CappedScoreQuery::new(organic, max_organic).unwrap());
                let ids = body["ids"].as_array().unwrap();
                if ids.is_empty() {
                    return capped;
                }
                let mut boost_num = max_organic.to_bits() as i32 + 1 + ids.len() as i32;
                let mut pins: Vec<QueryRef> = Vec::new();
                for id in ids {
                    let pin = f32::from_bits(boost_num as u32);
                    boost_num -= 1;
                    let idq: QueryRef = Arc::new(MultiTermQuery::set(search_index::schema::ID_FIELD, [es_numeric_id(id.as_str().unwrap())]));
                    pins.push(Arc::new(BoostQuery::new(Arc::new(ConstantScoreQuery::new(idq)), pin)));
                }
                Arc::new(DisMaxQuery::new(vec![Arc::new(DisMaxQuery::new(pins, 0.0)) as QueryRef, capped], 0.0))
            }
            "match" => {
                let (f, v) = body.as_object().unwrap().iter().next().unwrap();
                Arc::new(TermQuery::new(if f == "t" { T } else { T2 }, v.as_str().unwrap().as_bytes().to_vec()))
            }
            "boosting" => wrap(Arc::new(
                search_index::query::BoostingQuery::new(build(s, &body["positive"]), build(s, &body["negative"]), f32_of(&body["negative_boost"])).unwrap(),
            )),
            "bool" => {
                let mut b = BooleanQuery::new();
                for m in body["must"].as_array().unwrap() {
                    let t = m["match"]["t"].as_str().unwrap();
                    b = b.must(Arc::new(TermQuery::new(T, t.as_bytes().to_vec())));
                }
                for sq in body["should"].as_array().unwrap() {
                    b = b.should(build(s, sq));
                }
                Arc::new(b)
            }
            k => panic!("unknown query {k}"),
        }
    }

    #[test]
    fn feature_and_combined_fields_queries_match_elasticsearch() {
        let fx: Value = serde_json::from_str(include_str!("fixtures/features.json")).unwrap();
        let (s, schema) = env(fx["docs"].as_array().unwrap());
        // One segment in insertion order: doc n is `_id` n.
        let ids: std::collections::HashMap<u32, String> = (0..s.max_doc()).map(|d| (d, d.to_string())).collect();
        let mut failures = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let q = build(&s, &c["es"]);
            let name = c["lucene"].as_str().unwrap();
            if q.lucene_string(&schema) != name {
                failures.push(format!("query string {:?} vs es {name:?}", q.lucene_string(&schema)));
            }
            let mut col = TopScoreCollector::new(20, u64::MAX);
            s.search(&q, &mut col).unwrap();
            let td = col.top_docs();
            let want = c["hits"].as_array().unwrap();
            if td.total_hits != c["total"].as_u64().unwrap() {
                failures.push(format!("{name}: total {} vs {}", td.total_hits, c["total"]));
                continue;
            }
            for (i, (h, w)) in td.hits.iter().zip(want).enumerate() {
                let (wid, wscore) = (w["id"].as_str().unwrap(), f32_of(&w["score"]));
                if ids[&h.doc] != wid || h.score != wscore {
                    failures.push(format!("{name}: hit {i} ours {}@{} vs es {wid}@{wscore}", ids[&h.doc], h.score));
                    break;
                }
                let ex = s.explain(&q, h.doc).unwrap();
                if let Err(m) = same_expl(&ex, &w["explanation"], "") {
                    failures.push(format!("{name} doc {wid}: {m}"));
                    break;
                }
            }
        }
        assert!(failures.is_empty(), "{} cases differ:\n{}", failures.len(), failures.join("\n"));
    }

    const D: u32 = 20;
    const GEO: u32 = 21;

    /// `distance_feature` on a date (long) and a geo_point field, multi-valued docs included.
    #[test]
    fn distance_feature_queries_match_elasticsearch() {
        use search_index::query::DistanceFeatureQuery;
        use search_index::util::geo::encode_geo_dv;
        let fx: Value = serde_json::from_str(include_str!("fixtures/features.json")).unwrap();
        let fx = &fx["distance"];
        let schema = Schema::new()
            .with_field(FieldSchema::new(D, "d").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
            .unwrap()
            .with_field(FieldSchema::new(GEO, "g").with_points(PointsSpec::new(2, 4)).with_doc_values(DocValuesType::SortedNumeric))
            .unwrap();
        let shard = Shard::in_memory(schema.clone());
        // One value or an array of them; a geo point is a `[lon, lat]` array.
        let many = |v: &Value, point: bool| -> Vec<Value> {
            match v {
                Value::Null => vec![],
                Value::Array(a) if !point || a[0].is_array() => a.clone(),
                v => vec![v.clone()],
            }
        };
        for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
            let mut doc = IndexableDoc::new(i.to_string().into_bytes());
            for v in many(&d["d"], false) {
                let ms = v.as_i64().unwrap();
                doc = doc.point(D, search_codec::points::encode::i64_bytes(ms).to_vec()).doc_value(D, DocValue::SortedNumeric(ms));
            }
            for p in many(&d["g"], true) {
                let (lon, lat) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap());
                let enc = encode_geo_dv(lat, lon);
                let mut packed = search_codec::points::encode::i32_bytes((enc >> 32) as i32).to_vec();
                packed.extend_from_slice(&search_codec::points::encode::i32_bytes(enc as i32));
                doc = doc.point(GEO, packed).doc_value(GEO, DocValue::SortedNumeric(enc));
            }
            shard.index(IndexOp::new(doc)).unwrap();
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let ids = common::ids(&s);
        let mut failures = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let body = &c["es"]["distance_feature"];
            let boost = body.get("boost").map_or(1.0, f32_of);
            let p = &c["params"];
            let q = if let Some(l) = p.get("long") {
                DistanceFeatureQuery::long(D, boost, l[0].as_i64().unwrap(), l[1].as_i64().unwrap())
            } else {
                let g = &p["geo"];
                DistanceFeatureQuery::geo(GEO, boost, g[0].as_f64().unwrap(), g[1].as_f64().unwrap(), g[2].as_f64().unwrap())
            }
            .unwrap();
            let name = c["lucene"].as_str().unwrap();
            if q.lucene_string(&schema) != name {
                failures.push(format!("query string {:?} vs es {name:?}", q.lucene_string(&schema)));
            }
            let mut col = TopScoreCollector::new(20, u64::MAX);
            s.search(&q, &mut col).unwrap();
            let td = col.top_docs();
            if td.total_hits != c["total"].as_u64().unwrap() {
                failures.push(format!("{name}: total {} vs {}", td.total_hits, c["total"]));
                continue;
            }
            for (i, (h, w)) in td.hits.iter().zip(c["hits"].as_array().unwrap()).enumerate() {
                let (wid, wscore) = (w["id"].as_str().unwrap(), f32_of(&w["score"]));
                if ids[&h.doc] != wid || h.score != wscore {
                    failures.push(format!("{name}: hit {i} ours {}@{} vs es {wid}@{wscore}", ids[&h.doc], h.score));
                    break;
                }
                if let Err(m) = same_expl(&s.explain(&q, h.doc).unwrap(), &w["explanation"], "") {
                    failures.push(format!("{name} doc {wid}: {m}"));
                    break;
                }
            }
        }
        assert!(failures.is_empty(), "{} cases differ:\n{}", failures.len(), failures.join("\n"));
    }

    const JOIN: u32 = 22;
    const JQ: u32 = 23;

    /// `has_child` / `has_parent` (global ordinals joins): every score mode, children counts,
    /// boosts, explanations and the rewritten query strings.
    #[test]
    fn parent_child_joins_match_elasticsearch() {
        use search_index::query::{JoinQuery, JoinScoreMode};
        let fx: Value = serde_json::from_str(include_str!("fixtures/features.json")).unwrap();
        let fx = &fx["join"];
        let schema = Schema::new()
            .with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
            .unwrap()
            .with_field(FieldSchema::new(JOIN, "join").indexed(IndexOptions::Docs, false))
            .unwrap()
            .with_field(FieldSchema::new(JQ, "join#question").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::Sorted))
            .unwrap();
        let shard = Shard::in_memory(schema.clone());
        let a = Analysis::global().analyzer("standard").unwrap();
        let mut ids = std::collections::HashMap::new();
        for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
            let id = d[0].as_str().unwrap();
            ids.insert(i as u32, id.to_string());
            let body = &d[2];
            let (kind, parent) = match &body["join"] {
                Value::String(k) => (k.as_str(), id),
                o => (o["name"].as_str().unwrap(), o["parent"].as_str().unwrap()),
            };
            let doc = IndexableDoc::new(id.as_bytes().to_vec())
                .text(T, &a, body["t"].as_str().unwrap())
                .term(JOIN, kind.as_bytes().to_vec())
                .term(JQ, parent.as_bytes().to_vec())
                .doc_value(JQ, DocValue::Sorted(parent.as_bytes().to_vec().into()));
            shard.index(IndexOp::new(doc)).unwrap();
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let term = |f: u32, t: &str| -> QueryRef { Arc::new(TermQuery::new(f, t.as_bytes().to_vec())) };
        let mut failures = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let (kind, body) = c["es"].as_object().unwrap().iter().next().unwrap();
            let m = body["query"]["match"]["t"].as_str().unwrap();
            let boost = body.get("boost").map_or(1.0, f32_of);
            let q = if kind == "has_child" {
                let mode = match body.get("score_mode").and_then(|v| v.as_str()) {
                    Some("sum") => JoinScoreMode::Total,
                    Some("max") => JoinScoreMode::Max,
                    Some("min") => JoinScoreMode::Min,
                    Some("avg") => JoinScoreMode::Avg,
                    _ => JoinScoreMode::None,
                };
                let from: QueryRef = Arc::new(BooleanQuery::new().must(term(T, m)).filter(term(JOIN, "answer")));
                let mut j = JoinQuery::new(JQ, from, term(JOIN, "question"), mode);
                j.min = body.get("min_children").map_or(1, |v| v.as_u64().unwrap() as u32);
                j.max = body.get("max_children").map_or(i32::MAX as u32, |v| v.as_u64().unwrap() as u32);
                j
            } else {
                let mode = if body.get("score").is_some_and(|v| v == true) { JoinScoreMode::Max } else { JoinScoreMode::None };
                let from: QueryRef = Arc::new(BooleanQuery::new().must(term(T, m)).filter(term(JOIN, "question")));
                JoinQuery::new(JQ, from, term(JOIN, "answer"), mode)
            };
            let q: QueryRef = if boost != 1.0 { Arc::new(BoostQuery::new(Arc::new(q), boost)) } else { Arc::new(q) };
            let name = c["lucene"].as_str().unwrap();
            if q.lucene_string(&schema) != name {
                failures.push(format!("query string {:?} vs es {name:?}", q.lucene_string(&schema)));
            }
            let rw = s.rewrite(&q).unwrap().lucene_string(&schema);
            if rw != c["rewritten"].as_str().unwrap() {
                failures.push(format!("rewritten {rw:?} vs es {:?}", c["rewritten"]));
            }
            let mut col = TopScoreCollector::new(20, u64::MAX);
            s.search(&q, &mut col).unwrap();
            let td = col.top_docs();
            if td.total_hits != c["total"].as_u64().unwrap() {
                failures.push(format!("{rw}: total {} vs {}", td.total_hits, c["total"]));
                continue;
            }
            for (i, (h, w)) in td.hits.iter().zip(c["hits"].as_array().unwrap()).enumerate() {
                let (wid, wscore) = (w["id"].as_str().unwrap(), f32_of(&w["score"]));
                if ids[&h.doc] != wid || h.score != wscore {
                    failures.push(format!("{rw}: hit {i} ours {}@{} vs es {wid}@{wscore}", ids[&h.doc], h.score));
                    break;
                }
                if let Err(m) = same_expl(&s.explain(&q, h.doc).unwrap(), &w["explanation"], "") {
                    failures.push(format!("{rw} doc {wid}: {m}"));
                    break;
                }
            }
        }
        assert!(failures.is_empty(), "{} cases differ:\n{}", failures.len(), failures.join("\n"));
    }

    const GS: u32 = 24;
    const GN: u32 = 25;

    /// `n * 2.5`, or -1 without a value (the fixture's Painless sort script).
    struct NTimes;
    struct NTimesLeaf(Option<search_codec::SortedNumericDocValues>);
    impl search_index::collector::SortScript for NTimes {
        fn leaf(&self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn search_index::collector::LeafSortScript>> {
            let v = match seg.reader.doc_values(GN) {
                Some(search_codec::DocValuesReader::SortedNumeric(v)) => Some(v.clone()),
                _ => None,
            };
            Ok(Box::new(NTimesLeaf(v)))
        }
    }
    impl search_index::collector::LeafSortScript for NTimesLeaf {
        fn number(&mut self, doc: u32) -> search_index::Result<f64> {
            Ok(self.0.as_ref().and_then(|v| v.values(doc).next()).map_or(-1.0, |n| n as f64 * 2.5))
        }
    }

    /// `_geo_distance` (arc / plane, units, several origins, modes, descending with missing
    /// first) and `_script` sorts, with `search_after`.
    #[test]
    fn geo_distance_and_script_sorts_match_elasticsearch() {
        use search_index::collector::{FieldSort, GeoDistanceSort, GeoDistanceType, Missing, ScriptSort, SortKey, SortMode, SortType, SortValue, TopFieldCollector};
        use search_index::util::geo::encode_geo_dv;
        let fx: Value = serde_json::from_str(include_str!("fixtures/features.json")).unwrap();
        let fx = &fx["geosort"];
        let schema = Schema::new()
            .with_field(FieldSchema::new(GS, "g").with_points(PointsSpec::new(2, 4)).with_doc_values(DocValuesType::SortedNumeric))
            .unwrap()
            .with_field(FieldSchema::new(GN, "n").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
            .unwrap();
        let shard = Shard::in_memory(schema);
        for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
            let mut doc = IndexableDoc::new(i.to_string().into_bytes());
            let pts: Vec<Value> = match &d["g"] {
                Value::Null => vec![],
                Value::Array(a) if a[0].is_array() => a.clone(),
                v => vec![v.clone()],
            };
            for p in pts {
                doc = doc.doc_value(GS, DocValue::SortedNumeric(encode_geo_dv(p[1].as_f64().unwrap(), p[0].as_f64().unwrap())));
            }
            if let Some(n) = d["n"].as_i64() {
                doc = doc.doc_value(GN, DocValue::SortedNumeric(n));
            }
            shard.index(IndexOp::new(doc)).unwrap();
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let ids = common::ids(&s);
        let es_value = |v: &Value| -> SortValue {
            match v {
                Value::String(x) if x == "Infinity" => SortValue::Double(f64::INFINITY),
                Value::String(x) if x == "-Infinity" => SortValue::Double(f64::NEG_INFINITY),
                v if v.is_i64() => SortValue::Long(v.as_i64().unwrap()),
                v => SortValue::Double(v.as_f64().unwrap()),
            }
        };
        let mut failures = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let mut keys = Vec::new();
            for k in c["sort"].as_array().unwrap() {
                let (name, body) = k.as_object().unwrap().iter().next().unwrap();
                match name.as_str() {
                    "_geo_distance" => {
                        let g = &body["g"];
                        let points: Vec<(f64, f64)> = if g[0].is_array() {
                            g.as_array().unwrap().iter().map(|p| (p[1].as_f64().unwrap(), p[0].as_f64().unwrap())).collect()
                        } else {
                            vec![(g[1].as_f64().unwrap(), g[0].as_f64().unwrap())]
                        };
                        let unit_meters = match body.get("unit").and_then(|u| u.as_str()) {
                            Some("km") => 1000.0,
                            Some("mi") => 1609.344,
                            _ => 1.0,
                        };
                        let mode = body.get("mode").map(|m| match m.as_str().unwrap() {
                            "max" => SortMode::Max,
                            "avg" => SortMode::Avg,
                            "median" => SortMode::Median,
                            _ => SortMode::Min,
                        });
                        keys.push(SortKey::GeoDistance(GeoDistanceSort {
                            field: GS,
                            points,
                            distance_type: if body.get("distance_type").is_some_and(|d| d == "plane") { GeoDistanceType::Plane } else { GeoDistanceType::Arc },
                            unit_meters,
                            reverse: body.get("order").is_some_and(|o| o == "desc"),
                            mode,
                            nested: None,
                        }));
                    }
                    "_script" => keys.push(SortKey::Script(ScriptSort {
                        script: Arc::new(NTimes),
                        reverse: body.get("order").is_some_and(|o| o == "desc"),
                        mode: SortMode::Min,
                        nested: None,
                    })),
                    _ => keys.push(SortKey::Field(FieldSort {
                        field: GN,
                        kind: SortType::Long,
                        reverse: body == "desc",
                        missing: Missing::Last,
                        mode: SortMode::Min,
                        nested: None,
                    })),
                }
            }
            let q: QueryRef = Arc::new(MatchAllQuery);
            let mut col = TopFieldCollector::new(keys.clone(), 20, u64::MAX, false).with_searcher(&s);
            s.search(&q, &mut col).unwrap();
            let got: Vec<(String, Vec<SortValue>)> = col.top_docs().hits.into_iter().map(|h| (ids[&h.doc].clone(), h.values)).collect();
            let want: Vec<(String, Vec<SortValue>)> = c["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| (h["id"].as_str().unwrap().to_string(), h["sort"].as_array().unwrap().iter().map(es_value).collect()))
                .collect();
            if got != want {
                failures.push(format!("{}: ours {got:?}\n  es {want:?}", c["sort"]));
                continue;
            }
            if let Some(after) = c["after"].as_array() {
                let after: Vec<SortValue> = after.iter().map(es_value).collect();
                let mut col = TopFieldCollector::new(keys, 20, u64::MAX, false).with_searcher(&s).with_after(after, None);
                s.search(&q, &mut col).unwrap();
                let got: Vec<String> = col.top_docs().hits.iter().map(|h| ids[&h.doc].clone()).collect();
                let want: Vec<String> = c["after_hits"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
                if got != want {
                    failures.push(format!("{} after: ours {got:?} es {want:?}", c["sort"]));
                }
            }
        }
        assert!(failures.is_empty(), "{} cases differ:\n{}", failures.len(), failures.join("\n"));
    }

    /// The `term` suggester (Lucene's `DirectSpellChecker`): every option, string distance,
    /// mode, sort and threshold.
    #[test]
    fn term_suggestions_match_elasticsearch() {
        use search_index::spell::{SpellOptions, StringDistance, SuggestMode, SuggestSort};
        let fx: Value = serde_json::from_str(include_str!("fixtures/features.json")).unwrap();
        let fx = &fx["spell"];
        let schema = Schema::new().with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqsAndPositions, true)).unwrap();
        let shard = Shard::in_memory(schema);
        let a = Analysis::global().analyzer("standard").unwrap();
        for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
            shard.index(IndexOp::new(IndexableDoc::new(i.to_string().into_bytes()).text(T, &a, d.as_str().unwrap()))).unwrap();
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let mut failures = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let o = &c["case"]["term"];
            let mut opts = SpellOptions::default();
            if let Some(v) = o.get("suggest_mode") {
                opts.suggest_mode = match v.as_str().unwrap() {
                    "always" => SuggestMode::Always,
                    "popular" => SuggestMode::Popular,
                    _ => SuggestMode::Missing,
                };
            }
            if o.get("sort").is_some_and(|v| v == "frequency") {
                opts.sort = SuggestSort::Frequency;
            }
            if let Some(v) = o.get("string_distance") {
                opts.string_distance = match v.as_str().unwrap() {
                    "damerau_levenshtein" => StringDistance::DamerauLevenshtein,
                    "levenshtein" => StringDistance::Levenshtein,
                    "jaro_winkler" => StringDistance::JaroWinkler,
                    "ngram" => StringDistance::NGram,
                    _ => StringDistance::Internal,
                };
            }
            if let Some(v) = o.get("max_edits") {
                opts.max_edits = v.as_u64().unwrap() as u32;
            }
            if let Some(v) = o.get("prefix_length") {
                opts.prefix_length = v.as_u64().unwrap() as usize;
            }
            if let Some(v) = o.get("min_word_length") {
                opts.min_word_length = v.as_u64().unwrap() as usize;
            }
            if let Some(v) = o.get("min_doc_freq") {
                opts.min_doc_freq = f32_of(v);
            }
            if let Some(v) = o.get("max_term_freq") {
                opts.max_term_freq = f32_of(v);
            }
            if let Some(v) = o.get("accuracy") {
                opts.accuracy = f32_of(v);
            }
            if let Some(v) = o.get("max_inspections") {
                opts.max_inspections = v.as_u64().unwrap() as u32;
            }
            let size = o.get("size").map_or(5, |v| v.as_u64().unwrap() as usize);
            let shard_size = size.max(5);
            let tokens = analyze(c["case"]["text"].as_str().unwrap());
            for (tok, e) in tokens.iter().zip(c["entries"].as_array().unwrap()) {
                let text = String::from_utf8(tok.0.clone()).unwrap();
                let mut got = s.suggest_similar(T, &text, shard_size, &opts).unwrap();
                got.truncate(size);
                let got: Vec<(String, f32, u32)> = got.into_iter().map(|w| (w.term, w.score, w.freq)).collect();
                let want: Vec<(String, f32, u32)> = e["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|o| (o[0].as_str().unwrap().to_string(), f32_of(&o[1]), o[2].as_u64().unwrap() as u32))
                    .collect();
                if got != want {
                    failures.push(format!("{}: ours {got:?} es {want:?}", c["case"]));
                }
            }
        }
        assert!(failures.is_empty(), "{} cases differ:\n{}", failures.len(), failures.join("\n"));
    }

    /// The `phrase` suggester: generators (with reverse pre/post filters), smoothing models,
    /// gram sizes, confidence, max errors and highlighting.
    #[test]
    fn phrase_suggestions_match_elasticsearch() {
        use search_index::phrase_suggest::{DirectGenerator, PhraseSuggestOptions, PhraseToken, Smoothing};
        use search_index::spell::{SpellOptions, StringDistance, SuggestMode};
        let fx: Value = serde_json::from_str(include_str!("fixtures/features.json")).unwrap();
        let fx = &fx["phrase"];
        let analysis = Analysis::from_settings(&fx["analysis"], &search_analysis::IndexOptions::default()).unwrap();
        // field name -> (number, analyzer name, shingle max size)
        let fields: Vec<(&str, u32, &str, usize)> =
            vec![("body", 30, "body", 1), ("body.bigram", 31, "bigram", 2), ("body.ngram", 32, "ngram", 2), ("body.trigram", 33, "trigram", 3), ("body.reverse", 34, "reverse", 1)];
        let mut schema = Schema::new();
        for (name, n, _, _) in &fields {
            schema.add_field(FieldSchema::new(*n, *name).indexed(IndexOptions::DocsAndFreqsAndPositions, true)).unwrap();
        }
        let shard = Shard::in_memory(schema);
        for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
            let mut doc = IndexableDoc::new(i.to_string().into_bytes());
            for (_, n, a, _) in &fields {
                doc = doc.text(*n, &analysis.analyzer(a).unwrap(), d.as_str().unwrap());
            }
            shard.index(IndexOp::new(doc)).unwrap();
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let field = |name: &str| fields.iter().find(|f| f.0 == name).copied().unwrap();
        let filter = |name: &str| -> Box<search_index::phrase_suggest::TermFilter> {
            let a = analysis.analyzer(name).unwrap();
            Box::new(move |t: &[u8]| {
                let mut out = Vec::new();
                a.analyze(std::str::from_utf8(t).unwrap(), &mut |tok| out.push((tok.term.as_bytes().to_vec(), tok.position_increment)));
                out
            })
        };
        let mut failures = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let p = &c["case"]["phrase"];
            let (_, fnum, fan, shingles) = field(p["field"].as_str().unwrap());
            let analyzer = analysis.analyzer(p.get("analyzer").and_then(|a| a.as_str()).unwrap_or(fan)).unwrap();
            let mut tokens = Vec::new();
            analyzer.analyze(c["case"]["text"].as_str().unwrap(), &mut |t| {
                tokens.push(PhraseToken {
                    term: t.term.as_bytes().to_vec(),
                    position_increment: t.position_increment,
                    shingle: t.kind == "shingle",
                    synonym: t.kind == "SYNONYM",
                })
            });
            let size = p.get("size").map_or(5, |v| v.as_u64().unwrap() as usize);
            let smoothing = match p.get("smoothing").and_then(|v| v.as_object()).and_then(|m| m.iter().next()) {
                None => Smoothing::StupidBackoff { discount: 0.4f32 as f64 },
                Some((k, v)) if k == "laplace" => Smoothing::Laplace { alpha: v["alpha"].as_f64().unwrap() },
                Some((k, v)) if k == "linear" => Smoothing::LinearInterpolation {
                    trigram: v["trigram_lambda"].as_f64().unwrap(),
                    bigram: v["bigram_lambda"].as_f64().unwrap(),
                    unigram: v["unigram_lambda"].as_f64().unwrap(),
                },
                Some((_, v)) => Smoothing::StupidBackoff { discount: v["discount"].as_f64().unwrap() },
            };
            let generators = p["direct_generator"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| {
                    let mut spell = SpellOptions {
                        min_word_length: g["min_word_length"].as_u64().unwrap() as usize,
                        suggest_mode: match g["suggest_mode"].as_str().unwrap() {
                            "always" => SuggestMode::Always,
                            "popular" => SuggestMode::Popular,
                            _ => SuggestMode::Missing,
                        },
                        ..Default::default()
                    };
                    if let Some(m) = g.get("max_edits") {
                        spell.max_edits = m.as_u64().unwrap() as u32;
                    }
                    if g.get("string_distance").is_some_and(|d| d == "jaro_winkler") {
                        spell.string_distance = StringDistance::JaroWinkler;
                    }
                    DirectGenerator {
                        field: field(g["field"].as_str().unwrap()).1,
                        spell,
                        size: g.get("size").map_or(5, |v| v.as_u64().unwrap() as usize),
                        pre_filter: g.get("pre_filter").map(|f| filter(f.as_str().unwrap())),
                        post_filter: g.get("post_filter").map(|f| filter(f.as_str().unwrap())),
                    }
                })
                .collect();
            let opts = PhraseSuggestOptions {
                field: fnum,
                real_word_error_likelihood: p.get("real_word_error_likelihood").map_or(0.95, f32_of),
                confidence: p.get("confidence").map_or(1.0, f32_of),
                max_errors: p.get("max_errors").map_or(0.5, f32_of),
                separator: b" ".to_vec(),
                shard_size: size.max(5),
                gram_size: p.get("gram_size").map_or(shingles, |v| v.as_u64().unwrap() as usize),
                require_unigram: p.get("force_unigrams").is_none_or(|v| v == true),
                token_limit: 10,
                smoothing,
                generators,
            };
            let tags = p.get("highlight").map(|h| (h["pre_tag"].as_str().unwrap().as_bytes(), h["post_tag"].as_str().unwrap().as_bytes()));
            let r = s.phrase_suggest(&tokens, &opts).unwrap();
            let got: Vec<(String, Option<String>, f32)> = r
                .corrections
                .iter()
                .take(size)
                .map(|c| {
                    (
                        String::from_utf8(c.join(b" ", None)).unwrap(),
                        tags.map(|t| String::from_utf8(c.join(b" ", Some(t))).unwrap()),
                        c.score as f32,
                    )
                })
                .collect();
            let want: Vec<(String, Option<String>, f32)> = c["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| (o[0].as_str().unwrap().to_string(), o[1].as_str().map(|x| x.to_string()), f32_of(&o[2])))
                .collect();
            if got != want {
                failures.push(format!("{} {}:\n  ours {got:?}\n  es   {want:?}", c["case"]["text"], p));
            }
        }
        assert!(failures.is_empty(), "{} cases differ:\n{}", failures.len(), failures.join("\n"));
    }

    /// Elasticsearch's `Uid.encodeId`: numeric, URL-safe base64 (unpadded) or marked UTF-8.
    fn es_uid(id: &str) -> Vec<u8> {
        if !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()) {
            return es_numeric_id(id);
        }
        let b64 = |c: u8| -> Option<u32> {
            Some(match c {
                b'A'..=b'Z' => (c - b'A') as u32,
                b'a'..=b'z' => (c - b'a') as u32 + 26,
                b'0'..=b'9' => (c - b'0') as u32 + 52,
                b'-' => 62,
                b'_' => 63,
                _ => return None,
            })
        };
        let last_ok = match id.len() & 3 {
            0 => true,
            1 => false,
            2 => matches!(id.as_bytes()[id.len() - 1], b'A' | b'Q' | b'g' | b'w'),
            _ => matches!(id.as_bytes()[id.len() - 1], b'A' | b'E' | b'I' | b'M' | b'Q' | b'U' | b'Y' | b'c' | b'g' | b'k' | b'o' | b's' | b'w' | b'0' | b'4' | b'8'),
        };
        if last_ok && id.bytes().all(|c| b64(c).is_some()) {
            let mut bits = 0u32;
            let mut n = 0;
            let mut out = Vec::new();
            for c in id.bytes() {
                bits = bits << 6 | b64(c).unwrap();
                n += 6;
                if n >= 8 {
                    n -= 8;
                    out.push((bits >> n) as u8);
                    bits &= (1 << n) - 1;
                }
            }
            if out[0] >= 0xfd {
                out.insert(0, 0xfd);
            }
            return out;
        }
        let mut out = vec![0xff];
        out.extend_from_slice(id.as_bytes());
        out
    }

    /// Scroll (`_id` terms), field (numeric doc values) and point-in-time (doc id ranges)
    /// slices.
    #[test]
    fn slices_match_elasticsearch() {
        use search_index::query::{SliceBy, SliceQuery};
        const SN: u32 = 26;
        let fx: Value = serde_json::from_str(include_str!("fixtures/features.json")).unwrap();
        let fx = &fx["slices"];
        let schema = Schema::new()
            .with_field(FieldSchema::new(SN, "n").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
            .unwrap();
        let shard = Shard::in_memory(schema);
        let docs = fx["docs"].as_array().unwrap();
        for d in docs {
            let mut doc = IndexableDoc::new(es_uid(d["id"].as_str().unwrap()));
            for v in d["n"].as_array().into_iter().flatten() {
                doc = doc.doc_value(SN, DocValue::SortedNumeric(v.as_i64().unwrap()));
            }
            shard.index(IndexOp::new(doc)).unwrap();
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let mut failures = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let by = match c["kind"].as_str().unwrap() {
                "scroll" => SliceBy::Terms { field: search_index::schema::ID_FIELD, sliced_id_only: false },
                "field" => SliceBy::DocValues { field: SN },
                _ => SliceBy::DocId,
            };
            let q: QueryRef = Arc::new(SliceQuery::new(by, c["id"].as_u64().unwrap() as u32, c["max"].as_u64().unwrap() as u32).unwrap());
            let mut col = TopScoreCollector::new(100, u64::MAX);
            s.search(&q, &mut col).unwrap();
            let mut got: Vec<String> = col.top_docs().hits.iter().map(|h| docs[h.doc as usize]["id"].as_str().unwrap().to_string()).collect();
            got.sort();
            let want: Vec<String> = c["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            if got != want {
                failures.push(format!("{} {}/{}: ours {got:?} es {want:?}", c["kind"], c["id"], c["max"]));
            }
        }
        assert!(failures.is_empty(), "{} cases differ:\n{}", failures.len(), failures.join("\n"));
    }
}

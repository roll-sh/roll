//! Block scoring of `function_score` and `script_score` (their bulk scorers score a batch of
//! the inner query's matches at a time) against the per-doc scorer path: for random queries
//! over a corpus with deletes, many segments, missing and multi-valued fields, every function
//! kind, filters, every `score_mode` and `boost_mode`, the bulk path must collect the same docs
//! with bit-identical scores, fail with the same error when a function fails on some doc, and
//! give the same top hits when the collector prunes by minimum competitive score.
//! `FN_SEEDS=<n>` sweeps more corpora.

mod common;

use std::sync::Arc;

use rand::Rng;
use rand::rngs::StdRng;
use search_codec::{DocValuesType, IndexOptions, PointsSpec};
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken};
use search_index::schema::FieldSchema;
use search_index::collector::TopScoreCollector;
use search_index::query::function_score::{
    CombineFunction, DecayFn, DecayOrigin, FunctionEntry, FunctionScoreMode, FunctionScoreQuery, LeafScoreScript, Modifier, MultiValueMode,
    NumDecode, RandomField, ScoreFunction, ScoreScript, ScriptScoreQuery,
};
use search_index::query::{BooleanQuery, MatchAllQuery, TermQuery};
use search_index::search::{Collector, LeafCollector, Scorable, ScoreMode, SearchControl, Searcher, SegmentContext, take_deferred_error};
use search_index::{DeleteOp, IndexOp, QueryRef, Schema, Shard, ShardConfig};

const BODY: u32 = 10;
/// 0–2 values per doc (multi-valued).
const NUM: u32 = 12;
/// Exactly one value per doc (dense, single-valued sorted numeric).
const DENSE: u32 = 13;
/// `Numeric` doc values, missing on some docs.
const SPARSE: u32 = 14;
/// Encoded geo points, one per doc.
const GEO: u32 = 15;

fn schema() -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(BODY, "body").indexed(IndexOptions::DocsAndFreqs, true))
        .unwrap()
        .with_field(FieldSchema::new(NUM, "num").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(DENSE, "dense").with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(SPARSE, "sparse").with_doc_values(DocValuesType::Numeric))
        .unwrap()
        .with_field(FieldSchema::new(GEO, "geo").with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
}

fn random_doc(rng: &mut StdRng, i: usize) -> IndexableDoc {
    let mut d = IndexableDoc::new(format!("d{i}").into_bytes());
    let words: Vec<PreToken> = (0..rng.gen_range(0..12))
        .map(|_| PreToken::new(common::word(common::zipf(rng, common::VOCAB)), 1, 0, 0))
        .collect();
    if !words.is_empty() {
        d.fields.push(IndexableField::Tokens { field: BODY, tokens: words });
    }
    for _ in 0..rng.gen_range(0..3) {
        d.fields.push(IndexableField::DocValue { field: NUM, value: DocValue::SortedNumeric(rng.gen_range(-50..50)) });
    }
    // Repeated values (as `population` in geonames) exercise the logarithm reuse.
    let dense = if rng.gen_bool(0.5) { 0 } else { rng.gen_range(-5..100_000) };
    d.fields.push(IndexableField::DocValue { field: DENSE, value: DocValue::SortedNumeric(dense) });
    if rng.gen_bool(0.8) {
        d.fields.push(IndexableField::DocValue { field: SPARSE, value: DocValue::Numeric(rng.gen_range(0..1000)) });
    }
    let (lat, lon) = (rng.gen_range(40.0..60.0), rng.gen_range(-10.0..20.0));
    d.fields.push(IndexableField::DocValue { field: GEO, value: DocValue::SortedNumeric(search_index::util::geo::encode_geo_dv(lat, lon)) });
    d
}

/// A shard of `n` random operations (updates and deletes included), refreshed now and then.
fn build(seed: u64, n: usize) -> Shard {
    let mut rng = common::rng(seed);
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    let mut next = 0;
    for _ in 0..n {
        let r: f64 = rng.r#gen();
        if r < 0.85 || next == 0 {
            shard.index(IndexOp::new(random_doc(&mut rng, next))).unwrap();
            next += 1;
        } else if r < 0.93 {
            let k = rng.gen_range(0..next);
            shard.index(IndexOp::new(random_doc(&mut rng, k))).unwrap();
        } else {
            shard.delete(DeleteOp::new(format!("d{}", rng.gen_range(0..next)).into_bytes())).unwrap();
        }
        if rng.gen_bool(0.004) {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    if seed.is_multiple_of(3) {
        shard.force_merge(Some(1), false).unwrap();
    }
    shard
}

fn num_field(rng: &mut StdRng) -> u32 {
    [NUM, DENSE, DENSE, SPARSE][rng.gen_range(0..4)]
}

/// A test script: `sub * a + (doc % 7)`, failing on docs `% 53 == 11` when `fail`.
struct TestScript {
    a: f64,
    fail: bool,
    needs: bool,
}

struct TestLeaf {
    a: f64,
    fail: bool,
    base: u32,
}

impl LeafScoreScript for TestLeaf {
    fn score(&mut self, doc: u32, sub: f32) -> search_index::error::Result<f64> {
        let g = self.base + doc;
        if self.fail && g % 53 == 11 {
            return Err(search_index::error::Error::illegal(format!("script failed on {g}")));
        }
        Ok(sub as f64 * self.a + (g % 7) as f64)
    }
}

impl ScoreScript for TestScript {
    fn needs_score(&self) -> bool {
        self.needs
    }
    fn leaf(&self, seg: &SegmentContext) -> search_index::error::Result<Box<dyn LeafScoreScript>> {
        Ok(Box::new(TestLeaf { a: self.a, fail: self.fail, base: seg.base }))
    }
    fn describe(&self) -> String {
        "test".into()
    }
}

fn term(w: usize) -> QueryRef {
    Arc::new(TermQuery::new(BODY, common::word(w).into_bytes()))
}

fn inner(rng: &mut StdRng) -> QueryRef {
    match rng.gen_range(0..4) {
        0 => Arc::new(MatchAllQuery),
        1 => term(common::zipf(rng, common::VOCAB)),
        2 => {
            let mut b = BooleanQuery::new();
            b = b.should(term(common::zipf(rng, common::VOCAB))).should(term(common::zipf(rng, common::VOCAB)));
            Arc::new(b)
        }
        _ => term(rng.gen_range(0..3)),
    }
}

fn function(rng: &mut StdRng, errors: bool) -> ScoreFunction {
    let modes = [MultiValueMode::Min, MultiValueMode::Max, MultiValueMode::Avg, MultiValueMode::Sum, MultiValueMode::Median];
    match rng.gen_range(0..7) {
        0 => ScoreFunction::Weight,
        1 | 2 => {
            let safe = [Modifier::None, Modifier::Square, Modifier::Reciprocal];
            let all = [Modifier::None, Modifier::Square, Modifier::Reciprocal, Modifier::Log1p, Modifier::Ln, Modifier::Sqrt, Modifier::Log2p];
            let modifier = if errors { all[rng.gen_range(0..all.len())] } else { safe[rng.gen_range(0..safe.len())] };
            let missing = if errors && rng.gen_bool(0.5) { None } else { Some(rng.gen_range(-2.0..5.0)) };
            let decode = if rng.gen_bool(0.7) { NumDecode::Long } else { NumDecode::Scaled(rng.gen_range(1.0..100.0)) };
            ScoreFunction::FieldValueFactor { field: num_field(rng), name: "num".into(), factor: rng.gen_range(0.1f32..3.0), modifier, missing, decode }
        }
        3 => ScoreFunction::Decay {
            field: num_field(rng),
            name: "num".into(),
            func: [DecayFn::Gauss, DecayFn::Exp, DecayFn::Linear][rng.gen_range(0..3)],
            origin: DecayOrigin::Number(rng.gen_range(-30.0..30.0), NumDecode::Long),
            scale: rng.gen_range(1.0..40.0),
            decay: rng.gen_range(0.1..0.9),
            offset: rng.gen_range(0.0..5.0),
            mode: modes[rng.gen_range(0..modes.len())],
        },
        4 => ScoreFunction::Decay {
            // NUM: raw longs read as encoded points, tiny but exactly computed distances.
            field: if rng.gen_bool(0.7) { GEO } else { NUM },
            name: "geo".into(),
            func: [DecayFn::Gauss, DecayFn::Exp, DecayFn::Linear][rng.gen_range(0..3)],
            origin: DecayOrigin::Geo { lat: rng.gen_range(40.0..60.0), lon: rng.gen_range(-10.0..20.0) },
            scale: rng.gen_range(1_000.0..500_000.0),
            decay: rng.gen_range(0.1..0.9),
            offset: rng.gen_range(0.0..20_000.0),
            mode: modes[rng.gen_range(0..modes.len())],
        },
        5 => ScoreFunction::Random {
            seed: rng.r#gen(),
            salt: 3,
            field: rng.gen_bool(0.5).then(|| RandomField::Number { field: NUM, name: "num".into(), decode: NumDecode::Long }),
        },
        _ => ScoreFunction::Script(Arc::new(TestScript { a: rng.gen_range(0.5..2.0), fail: errors && rng.gen_bool(0.5), needs: rng.gen_bool(0.7) })),
    }
}

fn random_query(rng: &mut StdRng) -> QueryRef {
    let errors = rng.gen_bool(0.25);
    if rng.gen_bool(0.2) {
        let script = Arc::new(TestScript { a: rng.gen_range(0.5..2.0), fail: errors, needs: rng.gen_bool(0.7) });
        return Arc::new(ScriptScoreQuery { query: inner(rng), script, min_score: None });
    }
    let n = if rng.gen_bool(0.5) { 1 } else { rng.gen_range(2..5) };
    let fns = (0..n)
        .map(|_| FunctionEntry {
            filter: rng.gen_bool(0.35).then(|| term(common::zipf(rng, common::VOCAB))),
            function: function(rng, errors),
            weight: rng.gen_bool(0.4).then(|| rng.gen_range(0.25f32..4.0)),
        })
        .collect();
    let mut q = FunctionScoreQuery::new(inner(rng), fns);
    let sm = [
        FunctionScoreMode::First,
        FunctionScoreMode::Avg,
        FunctionScoreMode::Max,
        FunctionScoreMode::Sum,
        FunctionScoreMode::Min,
        FunctionScoreMode::Multiply,
    ];
    q.score_mode = sm[rng.gen_range(0..sm.len())];
    let bm = [CombineFunction::Multiply, CombineFunction::Replace, CombineFunction::Sum, CombineFunction::Avg, CombineFunction::Min, CombineFunction::Max];
    q.boost_mode = bm[rng.gen_range(0..bm.len())];
    if rng.gen_bool(0.2) {
        q.max_boost = rng.gen_range(0.5f32..5.0);
    }
    Arc::new(q)
}

/// Every match with its score bits, through the per-doc scorer API.
fn per_doc(s: &Searcher, q: &QueryRef) -> Result<Vec<(u32, u32)>, String> {
    let w = s.create_weight(q, ScoreMode::Complete).map_err(|e| e.to_string())?;
    take_deferred_error();
    let mut out = Vec::new();
    for seg in s.segments() {
        let Some(mut sc) = w.scorer(seg).map_err(|e| e.to_string())? else { continue };
        let mut d = sc.next();
        while d != search_codec::NO_MORE_DOCS {
            if seg.live.as_ref().is_none_or(|l| l.is_live(d)) {
                let score = sc.score();
                if let Some(e) = take_deferred_error() {
                    return Err(e.to_string());
                }
                out.push((seg.base + d, score.to_bits()));
            }
            d = sc.next();
        }
    }
    Ok(out)
}

/// Records every collected doc and its score.
struct All(Vec<(u32, u32)>);

struct AllLeaf<'a> {
    out: &'a mut Vec<(u32, u32)>,
    base: u32,
}

impl LeafCollector for AllLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> search_index::error::Result<()> {
        self.out.push((self.base + doc, scorer.score().to_bits()));
        Ok(())
    }
}

impl Collector for All {
    fn score_mode(&self) -> ScoreMode {
        ScoreMode::Complete
    }
    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> search_index::error::Result<Box<dyn LeafCollector + 'a>> {
        Ok(Box::new(AllLeaf { out: &mut self.0, base: seg.base }))
    }
}

fn bulk(s: &Searcher, q: &QueryRef) -> Result<Vec<(u32, u32)>, String> {
    let w = s.create_weight(q, ScoreMode::Complete).map_err(|e| e.to_string())?;
    let mut c = All(Vec::new());
    s.search_weight(w.as_ref(), &mut c, &SearchControl::default()).map_err(|e| e.to_string())?;
    Ok(c.0)
}

#[test]
fn block_scoring_equals_per_doc_scoring() {
    let seeds: u64 = std::env::var("FN_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(6);
    let (mut checked, mut failed) = (0, 0);
    for seed in 0..seeds {
        let shard = build(4000 + seed, 1500 + 400 * seed as usize);
        let s = shard.searcher();
        let mut rng = common::rng(seed * 31 + 7);
        for qi in 0..150 {
            let q = random_query(&mut rng);
            let want = per_doc(&s, &q);
            let got = bulk(&s, &q);
            assert_eq!(got, want, "seed {seed} query {qi}: {q:?}");
            checked += 1;
            if want.is_err() {
                failed += 1;
                continue;
            }
            // Top hits with pruning by minimum competitive score (small total-hits threshold).
            let want = want.unwrap();
            let mut sorted: Vec<(u32, f32)> = want.iter().map(|&(d, b)| (d, f32::from_bits(b))).collect();
            sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            sorted.truncate(10);
            let mut c = TopScoreCollector::new(10, rng.gen_range(0..20));
            s.search(&q, &mut c).unwrap();
            let top: Vec<(u32, f32)> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
            assert_eq!(top, sorted, "seed {seed} query {qi} top 10: {q:?}");
        }
    }
    assert!(checked > 0 && failed > 0 && failed < checked, "checked {checked}, failed {failed}");
}

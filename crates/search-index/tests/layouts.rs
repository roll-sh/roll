//! Randomized checks of every query family over a rich schema (text with positions, keywords,
//! numbers, geo points, dense vectors, nested children):
//!
//! - layout invariance: the same documents in many segments (random refreshes, partial merges,
//!   BP reordering) and in one segment give the same hits and scores (up to the last bits a
//!   different scorer structure can change), counts and explanations;
//! - brute-force models for what has simple semantics: geo distance and boxes (on the
//!   quantized coordinates), exact kNN, nested joins (parents of matching children with every
//!   score mode, children of matching parents) and span / interval near conditions;
//! - pruned top-k equals exhaustive top-k.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use rand::Rng;
use rand::rngs::StdRng;
use search_codec::points::encode;
use search_codec::{DocValuesType, HnswParams, IndexOptions, PointsSpec, VectorElement, VectorQuantization, VectorSimilarity, VectorSpec};
use search_index::collector::TopScoreCollector;
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken, StoredField};
use search_index::query::function_score::{
    CombineFunction, DecayFn, DecayOrigin, FunctionEntry, FunctionScoreMode, FunctionScoreQuery, Modifier, MultiValueMode, NumDecode,
    RandomField, ScoreFunction,
};
use search_index::query::intervals::IntervalsSource;
use search_index::query::mlt::{MoreLikeThis, MoreLikeThisQuery};
use search_index::query::nested::root_filter;
use search_index::query::{
    BooleanQuery, BoostQuery, BoostingQuery, CappedScoreQuery, CombinedFieldQuery, ConstantScoreQuery, DistanceFeatureQuery, DocValuesTermsQuery,
    MultiPhrasePrefixQuery, PointInSetQuery, PointType, SynonymQuery, CoveringQuery, MinMatch, DisMaxQuery, DocValuesRangeQuery, DvRange, FieldExistsQuery, GeoDistanceQuery,
    GeoPointShapeQuery, GeoShape, IndexOrDocValuesQuery, IntervalQuery, JoinScoreMode, KnnQuery, MultiPhraseQuery, MultiTermQuery,
    PhraseQuery, PointRangeQuery, QueryVector, RewriteMethod, ShapeRelation, SpanNode, SpanQuery, ToChildQuery, ToParentQuery,
    TermQuery,
};
use search_index::schema::FieldSchema;
use search_index::search::Searcher;
use search_index::{IndexOp, QueryRef, Schema, Shard, ShardConfig};

const BODY: u32 = 10;
const TAG: u32 = 11;
const NUM: u32 = 12;
const GEO: u32 = 14;
const VEC: u32 = 15;
const CBODY: u32 = 16;
const CNUM: u32 = 17;
const KEY: u32 = 18;
const DIMS: usize = 8;

fn schema() -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(BODY, "body").indexed(IndexOptions::DocsAndFreqsAndPositionsAndOffsets, true))
        .unwrap()
        .with_field(FieldSchema::new(TAG, "tag").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet))
        .unwrap()
        .with_field(FieldSchema::new(NUM, "num").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric).with_skip_index())
        .unwrap()
        .with_field(FieldSchema::new(GEO, "geo").with_points(PointsSpec::new(2, 4)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(VEC, "vec").with_vectors(VectorSpec {
            dims: DIMS as u32,
            element: VectorElement::Float32,
            similarity: VectorSimilarity::Cosine,
            hnsw: Some(HnswParams::default()),
            quantization: VectorQuantization::None,
        }))
        .unwrap()
        .with_field(FieldSchema::new(CBODY, "comments.body").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap()
        .with_field(FieldSchema::new(CNUM, "comments.num").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
}

#[derive(Clone, Debug)]
struct RDoc {
    id: String,
    body: Vec<String>,
    tags: Vec<String>,
    nums: Vec<i64>,
    geo: Option<(f64, f64)>,
    vec: Option<Vec<f32>>,
    children: Vec<(Vec<String>, i64)>,
}

fn word(i: usize) -> String {
    format!("w{i}")
}

fn random_doc(rng: &mut StdRng, i: usize) -> RDoc {
    let body = (0..rng.gen_range(0..25)).map(|_| word(common::zipf(rng, 40))).collect();
    let tags = (0..rng.gen_range(0..3)).map(|_| format!("t{}", rng.gen_range(0..6))).collect();
    let nums = (0..rng.gen_range(0..3)).map(|_| rng.gen_range(-40..40)).collect();
    let geo = rng.gen_bool(0.85).then(|| (rng.gen_range(40.0..50.0), rng.gen_range(-5.0..15.0)));
    let vec = rng.gen_bool(0.9).then(|| (0..DIMS).map(|_| rng.gen_range(-1.0f32..1.0)).collect());
    let children = (0..rng.gen_range(0..4))
        .map(|_| ((0..rng.gen_range(1..6)).map(|_| word(common::zipf(rng, 12))).collect(), rng.gen_range(0..20)))
        .collect();
    RDoc { id: format!("r{i}"), body, tags, nums, geo, vec, children }
}

fn tokens(words: &[String]) -> Vec<PreToken> {
    let mut off = 0u32;
    words
        .iter()
        .map(|w| {
            let t = PreToken::new(w.clone().into_bytes(), 1, off, off + w.len() as u32);
            off += w.len() as u32 + 1;
            t
        })
        .collect()
}

fn key_field(s: String) -> StoredField {
    StoredField { field: KEY, value: search_codec::stored::OwnedStoredValue::Str(s) }
}

fn to_indexable(d: &RDoc) -> IndexableDoc {
    let mut doc = IndexableDoc::new(d.id.clone().into_bytes());
    if !d.body.is_empty() {
        doc.fields.push(IndexableField::Tokens { field: BODY, tokens: tokens(&d.body) });
    }
    for t in &d.tags {
        doc.fields.push(IndexableField::Term { field: TAG, term: t.clone().into_bytes().into() });
        doc.fields.push(IndexableField::DocValue { field: TAG, value: DocValue::SortedSet(t.clone().into_bytes().into()) });
    }
    for &n in &d.nums {
        doc.fields.push(IndexableField::Point { field: NUM, packed: encode::i64_bytes(n).to_vec() });
        doc.fields.push(IndexableField::DocValue { field: NUM, value: DocValue::SortedNumeric(n) });
    }
    if let Some((lat, lon)) = d.geo {
        let mut p = encode::i32_bytes(encode::encode_latitude(lat)).to_vec();
        p.extend_from_slice(&encode::i32_bytes(encode::encode_longitude(lon)));
        doc.fields.push(IndexableField::Point { field: GEO, packed: p });
        let dv = ((encode::encode_latitude(lat) as i64) << 32) | (encode::encode_longitude(lon) as u32 as i64);
        doc.fields.push(IndexableField::DocValue { field: GEO, value: DocValue::SortedNumeric(dv) });
    }
    if let Some(v) = &d.vec {
        let b: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        doc.fields.push(IndexableField::Vector { field: VEC, value: b.into() });
    }
    doc.stored.push(key_field(d.id.clone()));
    for (i, (words, n)) in d.children.iter().enumerate() {
        let mut c = IndexableDoc::new(Vec::<u8>::new());
        c.fields.push(IndexableField::Tokens { field: CBODY, tokens: tokens(words) });
        c.fields.push(IndexableField::Point { field: CNUM, packed: encode::i64_bytes(*n).to_vec() });
        c.fields.push(IndexableField::DocValue { field: CNUM, value: DocValue::SortedNumeric(*n) });
        c.stored.push(key_field(format!("{}/{i}", d.id)));
        doc = doc.child(c);
    }
    doc
}

/// Two shards with the same docs: many segments vs one.
fn shards(seed: u64, n: usize) -> (Shard, Shard, Vec<RDoc>) {
    let mut rng = common::rng(seed);
    let docs: Vec<RDoc> = (0..n).map(|i| random_doc(&mut rng, i)).collect();
    let cfg = || ShardConfig { merge_threads: 0, ..Default::default() };
    let a = Shard::open(cfg(), schema(), Vec::new()).unwrap();
    let b = Shard::open(cfg(), schema(), Vec::new()).unwrap();
    for d in &docs {
        a.index(IndexOp::new(to_indexable(d))).unwrap();
        b.index(IndexOp::new(to_indexable(d))).unwrap();
        if rng.gen_bool(0.03) {
            a.refresh().unwrap();
        }
        if rng.gen_bool(0.003) {
            a.force_merge(Some(rng.gen_range(3..6)), false).unwrap();
        }
    }
    a.refresh().unwrap();
    b.refresh().unwrap();
    b.force_merge(Some(1), false).unwrap();
    (a, b, docs)
}

/// Global doc → key (`r12` for roots, `r12/1` for children).
fn keys(s: &Searcher) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for seg in s.segments() {
        for d in 0..seg.reader.max_doc() {
            if seg.live.as_ref().is_some_and(|l| !l.is_live(d)) {
                continue;
            }
            for (f, v) in seg.reader.stored().document(d).unwrap() {
                if f == KEY
                    && let search_codec::stored::OwnedStoredValue::Str(k) = v
                {
                    out.insert(seg.base + d, k);
                }
            }
        }
    }
    out
}

fn close(a: f32, b: f32) -> bool {
    // Subnormal scores (decays far from the origin) keep only a few significant bits.
    a == b
        || (a - b).abs() <= 4.0 * f32::EPSILON * a.abs().max(b.abs())
        || (a.abs().max(b.abs()) < f32::MIN_POSITIVE && (a - b).abs() <= 1e-44)
        || (a.is_nan() && b.is_nan())
}

/// All hits as key → score.
fn hits(s: &Searcher, k: &HashMap<u32, String>, q: &QueryRef) -> HashMap<String, f32> {
    let mut c = TopScoreCollector::new(k.len() + 1, u64::MAX);
    s.search(q, &mut c).unwrap();
    let td = c.top_docs();
    assert_eq!(td.total_hits as usize, td.hits.len());
    td.hits.iter().map(|h| (k[&h.doc].clone(), h.score)).collect()
}

/// Layout invariance + count + pruned top-k + explanations for one query.
fn check(name: &str, a: &Searcher, ka: &HashMap<u32, String>, b: &Searcher, kb: &HashMap<u32, String>, q: &QueryRef, rng: &mut StdRng) -> HashMap<String, f32> {
    let ha = hits(a, ka, q);
    let hb = hits(b, kb, q);
    let desc = || q.lucene_string(a.schema());
    assert_eq!(ha.len(), hb.len(), "{name}: hit counts differ for {}", desc());
    for (k, sa) in &ha {
        let sb = hb.get(k).unwrap_or_else(|| panic!("{name}: {k} only in the multi-segment view for {}", desc()));
        assert!(close(*sa, *sb), "{name}: {k} scores {sa} vs {sb} for {}", desc());
    }
    for (s, h, keys) in [(a, &ha, ka), (b, &hb, kb)] {
        assert_eq!(s.count(q).unwrap() as usize, h.len(), "{name}: count for {}", desc());
        let mut exhaustive: Vec<(u32, f32)> = {
            let mut c = TopScoreCollector::new(keys.len() + 1, u64::MAX);
            s.search(q, &mut c).unwrap();
            c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect()
        };
        exhaustive.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
        for k in [1usize, 4, 10] {
            let mut c = TopScoreCollector::new(k, k as u64);
            s.search(q, &mut c).unwrap();
            let got: Vec<(u32, f32)> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
            let want: Vec<(u32, f32)> = exhaustive.iter().take(k).copied().collect();
            assert_eq!(got, want, "{name}: pruned top-{k} for {}", desc());
        }
        let docs: Vec<u32> = keys.keys().copied().collect();
        for _ in 0..3 {
            let d = docs[rng.gen_range(0..docs.len())];
            let e = s.explain(q, d).unwrap();
            match h.get(&keys[&d]) {
                Some(sc) => {
                    assert!(e.is_match(), "{name}: explain of {} doesn't match for {} ({q:?}): {}", keys[&d], desc(), e.to_text());
                    assert!(close(e.value(), *sc), "{name}: explain {} != {sc} for {} ({q:?}): {}", e.value(), desc(), e.to_text());
                }
                None => assert!(!e.is_match(), "{name}: explain of non-hit {} matches for {}", keys[&d], desc()),
            }
        }
    }
    ha
}

fn term(f: u32, w: usize) -> QueryRef {
    Arc::new(TermQuery::new(f, word(w).into_bytes()))
}

fn span_term(w: usize) -> SpanNode {
    SpanNode::Term { field: BODY, term: word(w).into_bytes() }
}

fn iv_term(w: usize) -> IntervalsSource {
    IntervalsSource::term(word(w).into_bytes())
}

fn rewrite_method(rng: &mut StdRng) -> RewriteMethod {
    match rng.gen_range(0..7) {
        0 => RewriteMethod::ConstantScore,
        1 => RewriteMethod::ConstantScoreBoolean,
        2 => RewriteMethod::ScoringBoolean,
        3 => RewriteMethod::TopTerms(rng.gen_range(1..5)),
        4 => RewriteMethod::TopTermsBoost(rng.gen_range(1..5)),
        5 => RewriteMethod::TopTermsBlendedFreqs(rng.gen_range(1..5)),
        _ => RewriteMethod::ConstantScoreBlended,
    }
}

/// A random query of every family; `depth` bounds nesting.
fn random_query(rng: &mut StdRng, depth: u32) -> QueryRef {
    let w = |rng: &mut StdRng| common::zipf(rng, 40);
    match rng.gen_range(0..34) {
        0 => term(BODY, w(rng)),
        1 => {
            let n = rng.gen_range(2..4);
            Arc::new(PhraseQuery::new(BODY, (0..n).map(|_| word(w(rng)).into_bytes()).collect(), rng.gen_range(0..4)))
        }
        2 => {
            let slots: Vec<Vec<Vec<u8>>> = (0..rng.gen_range(2..4)).map(|_| (0..rng.gen_range(1..3)).map(|_| word(w(rng)).into_bytes()).collect()).collect();
            let positions = (0..slots.len() as u32).collect();
            Arc::new(MultiPhraseQuery::new(BODY, slots, positions, rng.gen_range(0..3)).unwrap())
        }
        3 => {
            let p = format!("w{}", rng.gen_range(1..4));
            Arc::new(MultiTermQuery::prefix(BODY, p.into_bytes()).with_rewrite(rewrite_method(rng)))
        }
        4 => Arc::new(MultiTermQuery::wildcard(BODY, &format!("w?{}", rng.gen_range(0..10)), false).unwrap().with_rewrite(rewrite_method(rng))),
        5 => Arc::new(MultiTermQuery::fuzzy(BODY, &word(w(rng) + 3), rng.gen_range(1..3), 1, 50, true)),
        6 => Arc::new(MultiTermQuery::regexp(BODY, "w[1-3][0-9]?", 0xff, false, 10_000).unwrap().with_rewrite(rewrite_method(rng))),
        7 => {
            let lo = rng.gen_range(-40..30);
            let hi = lo + rng.gen_range(0..30);
            match rng.gen_range(0..3) {
                0 => Arc::new(PointRangeQuery::long(NUM, lo, hi)),
                1 => Arc::new(DocValuesRangeQuery { field: NUM, range: DvRange::Long { lower: lo, upper: hi } }),
                _ => Arc::new(IndexOrDocValuesQuery {
                    index: Arc::new(PointRangeQuery::long(NUM, lo, hi)),
                    dv: Arc::new(DocValuesRangeQuery { field: NUM, range: DvRange::Long { lower: lo, upper: hi } }),
                }),
            }
        }
        8 => Arc::new(MultiTermQuery::set(TAG, (0..rng.gen_range(1..4)).map(|_| format!("t{}", rng.gen_range(0..7)).into_bytes()))),
        9 => Arc::new(SpanQuery::new(random_span(rng, 2)).unwrap()),
        10 => Arc::new(IntervalQuery::new(BODY, random_intervals(rng, 2))),
        11 => {
            let (lat, lon) = (rng.gen_range(40.0..50.0), rng.gen_range(-5.0..15.0));
            Arc::new(GeoDistanceQuery::new(GEO, lat, lon, rng.gen_range(1_000.0..400_000.0)).unwrap())
        }
        12 => {
            let lat = rng.gen_range(40.0..48.0);
            let lon = rng.gen_range(-5.0..10.0);
            let shape = GeoShape::Rectangle(lat, lat + rng.gen_range(0.5..3.0), lon, lon + rng.gen_range(0.5..5.0));
            let relation = [ShapeRelation::Intersects, ShapeRelation::Within, ShapeRelation::Disjoint][rng.gen_range(0..3)];
            Arc::new(GeoPointShapeQuery { field: GEO, shapes: vec![shape], relation })
        }
        13 => {
            let v: Vec<f32> = (0..DIMS).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
            let mut k = KnnQuery::new(VEC, QueryVector::Float(v), rng.gen_range(1..15), 40);
            k.exact = true;
            if rng.gen_bool(0.3) {
                k.filter = Some(term(BODY, w(rng)));
            }
            Arc::new(k)
        }
        14 => {
            let child = if depth > 0 && rng.gen_bool(0.3) { random_child(rng) } else { term(CBODY, common::zipf(rng, 12)) };
            let mode = [JoinScoreMode::None, JoinScoreMode::Avg, JoinScoreMode::Max, JoinScoreMode::Total, JoinScoreMode::Min][rng.gen_range(0..5)];
            Arc::new(ToParentQuery { child, parent_filter: root_filter(), score_mode: mode })
        }
        15 => Arc::new(FieldExistsQuery { field: [BODY, TAG, NUM, GEO][rng.gen_range(0..4)] }),
        16 => {
            let f = FunctionEntry {
                filter: rng.gen_bool(0.4).then(|| term(BODY, w(rng))),
                function: match rng.gen_range(0..4) {
                    0 => ScoreFunction::Weight,
                    1 => ScoreFunction::FieldValueFactor {
                        field: NUM,
                        name: "num".into(),
                        factor: 0.5,
                        // Values are negative too: square them (log1p would fail, as in ES).
                        modifier: Modifier::Square,
                        missing: Some(1.0),
                        decode: NumDecode::Long,
                    },
                    2 => ScoreFunction::Decay {
                        field: NUM,
                        name: "num".into(),
                        func: [DecayFn::Gauss, DecayFn::Exp, DecayFn::Linear][rng.gen_range(0..3)],
                        origin: DecayOrigin::Number(rng.gen_range(-10.0..10.0), NumDecode::Long),
                        scale: rng.gen_range(1.0..20.0),
                        decay: 0.5,
                        offset: rng.gen_range(0.0..3.0),
                        mode: MultiValueMode::Min,
                    },
                    _ => ScoreFunction::Random {
                        seed: rng.r#gen(),
                        salt: 7,
                        field: Some(RandomField::Number { field: NUM, name: "num".into(), decode: NumDecode::Long }),
                    },
                },
                weight: rng.gen_bool(0.5).then(|| rng.gen_range(0.5f32..3.0)),
            };
            let inner = if depth > 0 { random_query(rng, depth - 1) } else { term(BODY, w(rng)) };
            let mut q = FunctionScoreQuery::new(inner, vec![f]);
            q.score_mode = [FunctionScoreMode::Multiply, FunctionScoreMode::Sum, FunctionScoreMode::Avg, FunctionScoreMode::Max][rng.gen_range(0..4)];
            q.boost_mode = [CombineFunction::Multiply, CombineFunction::Sum, CombineFunction::Replace, CombineFunction::Max][rng.gen_range(0..4)];
            Arc::new(q)
        }
        17 => {
            let mut m = MoreLikeThis { fields: vec![BODY], min_doc_freq: 1, min_term_freq: 1, ..Default::default() };
            m.max_query_terms = rng.gen_range(2..8);
            let text: Vec<Vec<u8>> = (0..rng.gen_range(3..12)).map(|_| word(w(rng)).into_bytes()).collect();
            Arc::new(MoreLikeThisQuery { mlt: m, like_texts: vec![text], ..Default::default() })
        }
        18 if depth > 0 => Arc::new(DisMaxQuery::new((0..rng.gen_range(1..4)).map(|_| random_query(rng, depth - 1)).collect(), rng.gen_range(0.0..1.0))),
        19 if depth > 0 => Arc::new(ConstantScoreQuery::new(random_query(rng, depth - 1))),
        20 if depth > 0 => Arc::new(BoostQuery::new(random_query(rng, depth - 1), rng.gen_range(0.1..4.0))),
        21 if depth > 0 => Arc::new(ToChildQuery { parent: random_query(rng, 0), parent_filter: root_filter() }),
        22 => {
            let queries: Vec<QueryRef> = (0..rng.gen_range(1..5)).map(|_| term(BODY, w(rng))).collect();
            let min = if rng.gen_bool(0.5) { MinMatch::Field { field: NUM, name: "num".into() } } else { MinMatch::Constant(rng.gen_range(0..4)) };
            Arc::new(CoveringQuery { queries, min })
        }
        23 => {
            let terms = (0..rng.gen_range(1..4)).map(|_| (word(w(rng)).into_bytes(), [1.0f32, 0.5, 0.25][rng.gen_range(0..3)])).collect();
            Arc::new(SynonymQuery { field: BODY, terms })
        }
        24 => Arc::new(CombinedFieldQuery::new(&[(BODY, rng.gen_range(1.0..3.0))], word(w(rng))).unwrap()),
        25 if depth > 0 => Arc::new(BoostingQuery::new(random_query(rng, depth - 1), term(BODY, w(rng)), rng.gen_range(0.0..1.0)).unwrap()),
        26 if depth > 0 => Arc::new(CappedScoreQuery::new(random_query(rng, depth - 1), rng.gen_range(0.2..3.0)).unwrap()),
        27 => DistanceFeatureQuery::long(NUM, rng.gen_range(0.5..3.0), rng.gen_range(-30..30), rng.gen_range(1..20)).unwrap(),
        28 => {
            let (lat, lon) = (rng.gen_range(40.0..50.0), rng.gen_range(-5.0..15.0));
            DistanceFeatureQuery::geo(GEO, rng.gen_range(0.5..3.0), lat, lon, rng.gen_range(1_000.0..200_000.0)).unwrap()
        }
        29 => {
            let slots: Vec<Vec<Vec<u8>>> = (0..rng.gen_range(1..3)).map(|_| vec![word(w(rng)).into_bytes()]).chain([vec![b"w1".to_vec()]]).collect();
            let positions = (0..slots.len() as u32).collect();
            Arc::new(MultiPhrasePrefixQuery::new(BODY, slots, positions, rng.gen_range(0..2), rng.gen_range(1..20)).unwrap())
        }
        30 => {
            let tags: Vec<Vec<u8>> = (0..rng.gen_range(1..4)).map(|_| format!("t{}", rng.gen_range(0..7)).into_bytes()).collect();
            if rng.gen_bool(0.5) {
                Arc::new(DocValuesTermsQuery::new(TAG, tags))
            } else {
                let pts: Vec<Vec<u8>> = (0..rng.gen_range(1..6)).map(|_| encode::i64_bytes(rng.gen_range(-40..40)).to_vec()).collect();
                Arc::new(PointInSetQuery::new(NUM, pts, PointType::Long))
            }
        }
        31 => {
            let lat = rng.gen_range(40.0..48.0);
            let lon = rng.gen_range(-5.0..10.0);
            search_index::query::geo::bounding_box(GEO, lat, lat + rng.gen_range(0.5..4.0), lon, lon + rng.gen_range(0.5..6.0)).unwrap()
        }
        _ if depth > 0 => {
            let mut b = BooleanQuery::new();
            for _ in 0..rng.gen_range(1..5) {
                let c = random_query(rng, depth - 1);
                b = match rng.gen_range(0..4) {
                    0 => b.must(c),
                    1 => b.should(c),
                    2 => b.filter(c),
                    _ => b.must_not(c),
                };
            }
            if rng.gen_bool(0.3) {
                b = b.should(term(BODY, w(rng))).should(term(BODY, w(rng))).with_msm(rng.gen_range(1..3));
            }
            Arc::new(b)
        }
        _ => term(BODY, w(rng)),
    }
}

fn random_child(rng: &mut StdRng) -> QueryRef {
    match rng.gen_range(0..3) {
        0 => Arc::new(PointRangeQuery::long(CNUM, rng.gen_range(0..10), rng.gen_range(10..20))),
        1 => Arc::new(BooleanQuery::new().should(term(CBODY, common::zipf(rng, 12))).should(term(CBODY, common::zipf(rng, 12)))),
        _ => Arc::new(PhraseQuery::new(CBODY, vec![word(common::zipf(rng, 12)).into_bytes(), word(common::zipf(rng, 12)).into_bytes()], 1)),
    }
}

fn random_span(rng: &mut StdRng, depth: u32) -> SpanNode {
    let w = |rng: &mut StdRng| common::zipf(rng, 20);
    if depth == 0 {
        return span_term(w(rng));
    }
    match rng.gen_range(0..7) {
        0 => span_term(w(rng)),
        1 => SpanNode::Near {
            clauses: (0..rng.gen_range(2..4)).map(|_| random_span(rng, depth - 1)).collect(),
            slop: rng.gen_range(0..4),
            in_order: rng.gen_bool(0.5),
        },
        2 => SpanNode::Or((0..rng.gen_range(1..4)).map(|_| random_span(rng, depth - 1)).collect()),
        3 => SpanNode::Not {
            include: Box::new(random_span(rng, depth - 1)),
            exclude: Box::new(random_span(rng, depth - 1)),
            pre: rng.gen_range(0..2),
            post: rng.gen_range(0..2),
        },
        4 => SpanNode::First { inner: Box::new(random_span(rng, depth - 1)), end: rng.gen_range(1..10) },
        5 => SpanNode::Containing { big: Box::new(random_span(rng, depth - 1)), little: Box::new(random_span(rng, depth - 1)) },
        _ => SpanNode::Within { big: Box::new(random_span(rng, depth - 1)), little: Box::new(random_span(rng, depth - 1)) },
    }
}

fn random_intervals(rng: &mut StdRng, depth: u32) -> IntervalsSource {
    let w = |rng: &mut StdRng| common::zipf(rng, 20);
    if depth == 0 {
        return iv_term(w(rng));
    }
    let sub = |rng: &mut StdRng| random_intervals(rng, depth - 1);
    match rng.gen_range(0..12) {
        0 => iv_term(w(rng)),
        1 => IntervalsSource::ordered((0..rng.gen_range(2..4)).map(|_| sub(rng)).collect()),
        2 => IntervalsSource::unordered((0..rng.gen_range(2..4)).map(|_| sub(rng)).collect()),
        3 => IntervalsSource::phrase((0..rng.gen_range(2..4)).map(|_| sub(rng)).collect()),
        4 => IntervalsSource::or((0..rng.gen_range(2..4)).map(|_| sub(rng)).collect()),
        5 => IntervalsSource::max_gaps(rng.gen_range(0..4), sub(rng)),
        6 => IntervalsSource::containing(sub(rng), sub(rng)),
        7 => IntervalsSource::not_containing(sub(rng), sub(rng)),
        8 => IntervalsSource::contained_by(sub(rng), sub(rng)),
        9 => IntervalsSource::before(sub(rng), sub(rng)),
        10 => IntervalsSource::after(sub(rng), sub(rng)),
        _ => IntervalsSource::at_least(2, (0..3).map(|_| sub(rng)).collect()),
    }
}

#[test]
fn every_query_family_is_layout_invariant() {
    let seeds: u64 = std::env::var("LAYOUT_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    let per: usize = std::env::var("LAYOUT_QUERIES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    for seed in 0..seeds {
        let (a, b, _) = shards(seed, 700);
        let (sa, sb) = (a.searcher(), b.searcher());
        if sa.segments().len() < 2 {
            // The merge policy folded every refresh into one segment (it may, with 8+ small
            // segments): nothing to compare for this seed.
            continue;
        }
        assert_eq!(sb.segments().len(), 1);
        let (ka, kb) = (keys(&sa), keys(&sb));
        let mut rng = common::rng(seed * 31 + 5);
        for i in 0..per {
            let q = random_query(&mut rng, 2);
            check(&format!("seed {seed} q{i}"), &sa, &ka, &sb, &kb, &q, &mut rng);
        }
    }
}

/// Positions of a word in a token list.
fn positions(words: &[String], w: &str) -> Vec<i64> {
    words.iter().enumerate().filter(|(_, x)| x.as_str() == w).map(|(i, _)| i as i64).collect()
}

#[test]
fn brute_force_models_agree() {
    for seed in 0..4u64 {
        let (a, b, docs) = shards(seed + 50, 600);
        let (sa, sb) = (a.searcher(), b.searcher());
        let (ka, kb) = (keys(&sa), keys(&sb));
        let mut rng = common::rng(seed + 900);
        for i in 0..40 {
            let name = format!("seed {seed} model {i}");
            // Geo distance on the quantized coordinates.
            let (lat, lon, r) = (rng.gen_range(40.0..50.0), rng.gen_range(-5.0..15.0), rng.gen_range(10_000.0..300_000.0));
            let q: QueryRef = Arc::new(GeoDistanceQuery::new(GEO, lat, lon, r).unwrap());
            let h = check(&name, &sa, &ka, &sb, &kb, &q, &mut rng);
            for d in &docs {
                let Some((dlat, dlon)) = d.geo else {
                    assert!(!h.contains_key(&d.id));
                    continue;
                };
                let (qlat, qlon) = (
                    encode::decode_latitude(encode::encode_latitude(dlat)),
                    encode::decode_longitude(encode::encode_longitude(dlon)),
                );
                let dist = haversine(lat, lon, qlat, qlon);
                if (dist - r).abs() > r * 1e-6 {
                    assert_eq!(h.contains_key(&d.id), dist <= r, "{name}: geo {} at {dist} m, radius {r}", d.id);
                }
            }
            // Exact kNN = brute-force cosine top k.
            let v: Vec<f32> = (0..DIMS).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
            let k = rng.gen_range(1..20);
            let mut kq = KnnQuery::new(VEC, QueryVector::Float(v.clone()), k, 50);
            kq.exact = true;
            let q: QueryRef = Arc::new(kq);
            let h = check(&name, &sa, &ka, &sb, &kb, &q, &mut rng);
            let mut want: Vec<(f32, &str)> = docs.iter().filter_map(|d| d.vec.as_ref().map(|x| (cosine_score(&v, x), d.id.as_str()))).collect();
            want.sort_by(|x, y| y.0.total_cmp(&x.0));
            assert_eq!(h.len(), k.min(want.len()), "{name}: knn hits");
            let kth = want[k.min(want.len()) - 1].0;
            for (s, id) in want.iter().take(k) {
                if *s > kth + 1e-5 {
                    let got = h.get(*id).unwrap_or_else(|| panic!("{name}: knn missing {id}"));
                    assert!((got - s).abs() < 1e-5, "{name}: knn score {got} vs {s}");
                }
            }
            // Approximate kNN recall.
            let q: QueryRef = Arc::new(KnnQuery::new(VEC, QueryVector::Float(v.clone()), 10, 60));
            let ha = hits(&sa, &ka, &q);
            let top: Vec<&str> = want.iter().take(10).map(|x| x.1).collect();
            let found = top.iter().filter(|id| ha.contains_key(**id)).count();
            assert!(found >= 8, "{name}: approximate knn recall {found}/10");
            // Nested: parents of children with a word, every score mode.
            let cw = word(common::zipf(&mut rng, 12));
            for mode in [JoinScoreMode::None, JoinScoreMode::Avg, JoinScoreMode::Max, JoinScoreMode::Total, JoinScoreMode::Min] {
                let child: QueryRef = Arc::new(TermQuery::new(CBODY, cw.clone().into_bytes()));
                let q: QueryRef = Arc::new(ToParentQuery { child: child.clone(), parent_filter: root_filter(), score_mode: mode });
                let h = check(&name, &sa, &ka, &sb, &kb, &q, &mut rng);
                let child_hits = hits(&sa, &ka, &child);
                for d in &docs {
                    let scores: Vec<f32> = (0..d.children.len()).filter_map(|i| child_hits.get(&format!("{}/{i}", d.id)).copied()).collect();
                    assert_eq!(h.contains_key(&d.id), !scores.is_empty(), "{name}: nested parent {}", d.id);
                    if let Some(got) = h.get(&d.id) {
                        let want = match mode {
                            JoinScoreMode::None => 0.0,
                            JoinScoreMode::Max => scores.iter().copied().fold(f32::MIN, f32::max),
                            JoinScoreMode::Min => scores.iter().copied().fold(f32::MAX, f32::min),
                            JoinScoreMode::Total => scores.iter().sum(),
                            JoinScoreMode::Avg => scores.iter().sum::<f32>() / scores.len() as f32,
                        };
                        assert!(close(*got, want) || (got - want).abs() < 1e-6, "{name}: nested {mode:?} score of {} {got} vs {want}", d.id);
                    }
                }
            }
            // Children of parents with a body word.
            let pw = common::zipf(&mut rng, 40);
            let q: QueryRef = Arc::new(ToChildQuery { parent: term(BODY, pw), parent_filter: root_filter() });
            let h = check(&name, &sa, &ka, &sb, &kb, &q, &mut rng);
            for d in &docs {
                for i in 0..d.children.len() {
                    assert_eq!(h.contains_key(&format!("{}/{i}", d.id)), d.body.contains(&word(pw)), "{name}: to-child {}/{i}", d.id);
                }
            }
            // Ordered span near / ordered intervals: some window of the words in order.
            let (w1, w2) = (word(common::zipf(&mut rng, 10)), word(common::zipf(&mut rng, 10)));
            let slop = rng.gen_range(0..4i64);
            let near = SpanNode::Near {
                clauses: vec![SpanNode::Term { field: BODY, term: w1.clone().into_bytes() }, SpanNode::Term { field: BODY, term: w2.clone().into_bytes() }],
                slop: slop as u32,
                in_order: true,
            };
            let q: QueryRef = Arc::new(SpanQuery::new(near).unwrap());
            let hs = check(&name, &sa, &ka, &sb, &kb, &q, &mut rng);
            let iq: QueryRef = Arc::new(IntervalQuery::new(
                BODY,
                IntervalsSource::max_gaps(slop as i32, IntervalsSource::ordered(vec![IntervalsSource::term(w1.clone().into_bytes()), IntervalsSource::term(w2.clone().into_bytes())])),
            ));
            let hi = check(&name, &sa, &ka, &sb, &kb, &iq, &mut rng);
            for d in &docs {
                let (p1, p2) = (positions(&d.body, &w1), positions(&d.body, &w2));
                let model = p1.iter().any(|&x| p2.iter().any(|&y| y > x && y - x - 1 <= slop));
                assert_eq!(hs.contains_key(&d.id), model, "{name}: span near {w1} {w2}~{slop} in {}: {:?}", d.id, d.body);
                assert_eq!(hi.contains_key(&d.id), model, "{name}: intervals {w1} {w2}~{slop} in {}: {:?}", d.id, d.body);
            }
        }
    }
}

fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let dp = p2 - p1;
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.771_4 * a.sqrt().asin()
}

fn cosine_score(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    (1.0 + dot / (na * nb)) / 2.0
}

/// The doc-values geo queries (`LatLonDocValuesQuery`, `LatLonDocValuesBoxQuery`) match the
/// same docs as their point-index counterparts for every relation and shape, alone and as the
/// `dvQuery` of an `IndexOrDocValuesQuery` led by a cheaper clause.
#[test]
fn geo_doc_values_queries_equal_point_queries() {
    use search_index::query::geo::{Polygon, bounding_box};
    use search_index::query::{GeoPointDvQuery, ShapeRelation};
    for seed in 0..3u64 {
        let (a, _, docs) = shards(seed + 70, 600);
        let s = a.searcher();
        let k = keys(&s);
        let mut rng = common::rng(seed + 31);
        let set = |q: &QueryRef| -> Vec<String> {
            let mut v: Vec<String> = hits(&s, &k, q).into_keys().collect();
            v.sort();
            v
        };
        for i in 0..60 {
            let (lat, lon) = (rng.gen_range(40.0..48.0), rng.gen_range(-5.0..12.0));
            let (dlat, dlon) = (rng.gen_range(0.5..4.0), rng.gen_range(0.5..6.0));
            let shape = match rng.gen_range(0..4) {
                0 => GeoShape::Rectangle(lat, lat + dlat, lon, lon + dlon),
                1 => GeoShape::Circle { lat, lon, radius: rng.gen_range(10_000.0..400_000.0) },
                2 => GeoShape::Polygon(Polygon {
                    outer: vec![(lat, lon), (lat, lon + dlon), (lat + dlat, lon + dlon * 0.5), (lat, lon)],
                    holes: vec![],
                }),
                _ => {
                    // A doc's own (quantized) point.
                    let (plat, plon) = docs.iter().filter_map(|d| d.geo).nth(rng.gen_range(0..50)).unwrap();
                    GeoShape::Point(
                        encode::decode_latitude(encode::encode_latitude(plat)),
                        encode::decode_longitude(encode::encode_longitude(plon)),
                    )
                }
            };
            for relation in [ShapeRelation::Intersects, ShapeRelation::Within, ShapeRelation::Disjoint] {
                let what = format!("seed {seed} #{i}: {} {relation:?}", shape.lucene_string());
                let index: QueryRef = match (&shape, relation) {
                    (GeoShape::Rectangle(a, b, c, d), ShapeRelation::Intersects) => bounding_box(GEO, *a, *b, *c, *d).unwrap(),
                    (GeoShape::Circle { lat, lon, radius }, ShapeRelation::Intersects) => {
                        Arc::new(GeoDistanceQuery::new(GEO, *lat, *lon, *radius).unwrap())
                    }
                    _ => Arc::new(GeoPointShapeQuery { field: GEO, shapes: vec![shape.clone()], relation }),
                };
                let dv = GeoPointDvQuery::geometry(GEO, relation, vec![shape.clone()]).unwrap();
                let want = set(&index);
                assert_eq!(set(&dv), want, "{what}");
                // Led by a selective clause, the doc-values side runs.
                let lead: QueryRef = Arc::new(TermQuery::new(BODY, word(common::zipf(&mut rng, 40)).into_bytes()));
                let iodv: QueryRef = Arc::new(IndexOrDocValuesQuery { index: index.clone(), dv: dv.clone() });
                let both: QueryRef = Arc::new(BooleanQuery::new().must(lead.clone()).filter(iodv));
                let only_index: QueryRef = Arc::new(BooleanQuery::new().must(lead).filter(index));
                assert_eq!(set(&both), set(&only_index), "{what} led");
            }
        }
    }
}

/// Lucene's strings for the geo queries, as Elasticsearch 9.5.5 profiles them.
#[test]
fn geo_query_strings_match_elasticsearch() {
    use search_index::Query;
    use search_index::query::geo::Polygon;
    use search_index::query::{GeoPointDvQuery, ShapeRelation};
    let s = Shard::in_memory(schema()).searcher();
    let sc = s.schema();
    let (q52, q53, q13, q14) = (51.99999995995313, 52.999999984167516, 12.999999979510903, 13.999999919906259);
    let circle = GeoShape::Circle { lat: 52.399999969638884, lon: 12.999999979510903, radius: 100000.0 };
    assert_eq!(
        GeoPointDvQuery::geometry(GEO, ShapeRelation::Intersects, vec![circle]).unwrap().lucene_string(sc),
        "geo:INTERSECTS:geometries([Circle([52.399999969638884,12.999999979510903] radius = 100000.0 meters)])"
    );
    let poly = GeoShape::Polygon(Polygon { outer: vec![(52.0, 13.0), (52.0, 14.0), (53.0, 14.0), (53.0, 13.0)], holes: vec![] });
    assert_eq!(
        GeoPointShapeQuery { field: GEO, shapes: vec![poly.clone()], relation: ShapeRelation::Intersects }.lucene_string(sc),
        "LatLonPointQuery: field=geo:[Polygon[52.0, 13.0] [52.0, 14.0] [53.0, 14.0] [53.0, 13.0] [52.0, 13.0] ,]"
    );
    assert_eq!(
        GeoPointDvQuery::geometry(GEO, ShapeRelation::Within, vec![poly]).unwrap().lucene_string(sc),
        "geo:WITHIN:geometries([Polygon[52.0, 13.0] [52.0, 14.0] [53.0, 14.0] [53.0, 13.0] [52.0, 13.0] ])"
    );
    let pt = GeoShape::Point(52.49999997206032, 13.399999989196658);
    assert_eq!(
        GeoPointShapeQuery { field: GEO, shapes: vec![pt.clone()], relation: ShapeRelation::Intersects }.lucene_string(sc),
        "LatLonPointQuery: field=geo:[Point(13.399999989196658,52.49999997206032),]"
    );
    assert_eq!(
        GeoPointDvQuery::geometry(GEO, ShapeRelation::Contains, vec![pt]).unwrap().lucene_string(sc),
        "geo:CONTAINS:geometries([Point(13.399999989196658,52.49999997206032)])"
    );
    let line = GeoShape::Line(vec![(q52, q13), (q53, q14)]);
    assert_eq!(
        GeoPointShapeQuery { field: GEO, shapes: vec![line], relation: ShapeRelation::Intersects }.lucene_string(sc),
        "LatLonPointQuery: field=geo:[Line([12.999999979510903, 51.99999995995313][13.999999919906259, 52.999999984167516]),]"
    );
    let pts = vec![GeoShape::Point(q52, q13), GeoShape::Point(q53, q14)];
    assert_eq!(
        GeoPointDvQuery::geometry(GEO, ShapeRelation::Disjoint, pts).unwrap().lucene_string(sc),
        "geo:DISJOINT:geometries([Point(12.999999979510903,51.99999995995313), Point(13.999999919906259,52.999999984167516)])"
    );
    assert_eq!(
        GeoPointDvQuery::bounding_box(GEO, q52, q53, q13, q14).unwrap().lucene_string(sc),
        "geo:box(minLat=51.99999995995313, maxLat=52.999999984167516, minLon=12.999999979510903, maxLon=13.999999919906259)"
    );
    assert_eq!(
        GeoPointDvQuery::geometry(GEO, ShapeRelation::Contains, vec![GeoShape::Rectangle(1.0, 2.0, 3.0, 4.0)]).unwrap().lucene_string(sc),
        "MatchNoDocsQuery(\"Contains LatLonDocValuesField.newSlowGeometryQuery with non-point geometries\")"
    );
    // Lucene's geometry checks.
    let err = |g: GeoShape| GeoPointDvQuery::geometry(GEO, ShapeRelation::Intersects, vec![g]).unwrap_err().to_string();
    assert_eq!(err(GeoShape::Circle { lat: 1.0, lon: 2.0, radius: f64::NAN }), "radiusMeters: 'NaN' is invalid");
    assert_eq!(err(GeoShape::Circle { lat: 91.0, lon: 2.0, radius: 1.0 }), "invalid latitude 91.0; must be between -90.0 and 90.0");
    assert_eq!(err(GeoShape::Line(vec![(1.0, 2.0)])), "at least 2 line points required");
    assert_eq!(err(GeoShape::Point(0.0, 181.0)), "invalid longitude 181.0; must be between -180.0 and 180.0");
    assert!(GeoPointDvQuery::geometry(GEO, ShapeRelation::Within, vec![GeoShape::Line(vec![(1.0, 2.0), (2.0, 3.0)])]).is_err());
    let bad: QueryRef = Arc::new(GeoPointShapeQuery { field: GEO, shapes: vec![GeoShape::Point(95.0, 0.0)], relation: ShapeRelation::Intersects });
    assert!(s.count(&bad).is_err());
}

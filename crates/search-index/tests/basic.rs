//! End-to-end basics: index, refresh, term queries, scores against Elasticsearch.

use std::sync::Arc;

use search_analysis::Analysis;
use search_codec::IndexOptions;
use search_index::collector::TopScoreCollector;
use search_index::query::{MatchAllQuery, TermQuery};
use search_index::schema::FieldSchema;
use search_index::{IndexOp, IndexableDoc, OpResult, Schema, Shard};

const T: u32 = 10;

fn schema() -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap()
}

#[test]
fn bm25_scores_match_elasticsearch() {
    let shard = Shard::in_memory(schema());
    let a = Analysis::global().analyzer("standard").unwrap();
    for (id, text) in [("1", "the quick brown fox jumps over the lazy dog"), ("2", "quick quick fox"), ("3", "lazy")] {
        let r = shard.index(IndexOp::new(IndexableDoc::new(id.as_bytes().to_vec()).text(T, &a, text))).unwrap();
        assert!(matches!(r, OpResult::Indexed { created: true, .. }), "{r:?}");
    }
    assert!(shard.refresh().unwrap());
    let s = shard.searcher();
    assert_eq!(s.num_docs(), 3);
    let q = Arc::new(TermQuery::new(T, "quick")) as search_index::QueryRef;
    let mut c = TopScoreCollector::new(10, u64::MAX);
    s.search(&q, &mut c).unwrap();
    let td = c.top_docs();
    assert_eq!(td.total_hits, 2);
    assert_eq!(td.hits[0].doc, 1);
    assert_eq!(td.hits[0].score, 0.7074792);
    let e = s.explain(&q, 1).unwrap();
    assert_eq!(e.description, "weight(t:quick in 1) [PerFieldSimilarity], result of:");
    assert_eq!(e.value(), 0.7074792);
    let q = Arc::new(TermQuery::new(T, "fox")) as search_index::QueryRef;
    let mut c = TopScoreCollector::new(10, u64::MAX);
    s.search(&q, &mut c).unwrap();
    assert_eq!(c.top_docs().hits.iter().find(|h| h.doc == 1).unwrap().score, 0.5376842);
    assert_eq!(s.count(&(Arc::new(MatchAllQuery) as search_index::QueryRef)).unwrap(), 3);
    // Realtime get and update.
    let g = shard.get(b"2", true).unwrap().unwrap();
    assert_eq!(g.version, 1);
    let r = shard.index(IndexOp::new(IndexableDoc::new(b"2".to_vec()).text(T, &a, "slow"))).unwrap();
    assert!(matches!(r, OpResult::Indexed { created: false, version: 2, .. }), "{r:?}");
    assert_eq!(shard.get(b"2", true).unwrap().unwrap().version, 2);
    assert_eq!(shard.get(b"2", false).unwrap().unwrap().version, 1);
    shard.refresh().unwrap();
    let s = shard.searcher();
    assert_eq!(s.num_docs(), 3);
    let q = Arc::new(TermQuery::new(T, "quick")) as search_index::QueryRef;
    assert_eq!(s.count(&q).unwrap(), 1);
}

/// The `script` query: the script filters, other clauses lead, failures surface.
#[test]
fn script_query_filters_and_reports_failures() {
    use search_index::query::{BooleanQuery, FilterScript, LeafFilterScript, ScriptQuery};
    use search_index::search::SegmentContext;

    struct Even {
        fail_at: Option<u32>,
        calls: Arc<std::sync::atomic::AtomicU32>,
    }
    struct Leaf {
        base: u32,
        fail_at: Option<u32>,
        calls: Arc<std::sync::atomic::AtomicU32>,
    }
    impl FilterScript for Even {
        fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafFilterScript>> {
            Ok(Box::new(Leaf { base: seg.base, fail_at: self.fail_at, calls: self.calls.clone() }))
        }
        fn describe(&self) -> String {
            "Script{type=inline, lang='painless', idOrCode='even', options={}, params={}}".into()
        }
    }
    impl LeafFilterScript for Leaf {
        fn matches(&mut self, doc: u32) -> search_index::Result<bool> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let d = self.base + doc;
            if Some(d) == self.fail_at {
                return Err(search_index::Error::illegal("boom"));
            }
            Ok(d.is_multiple_of(2))
        }
    }

    let shard = Shard::in_memory(schema());
    let a = Analysis::global().analyzer("standard").unwrap();
    for i in 0..100 {
        let text = if i % 10 == 0 { "rare word" } else { "common word" };
        shard.index(IndexOp::new(IndexableDoc::new(format!("{i}").into_bytes()).text(T, &a, text))).unwrap();
        if i == 50 {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let script: search_index::QueryRef = Arc::new(ScriptQuery { script: Arc::new(Even { fail_at: None, calls: calls.clone() }) });
    assert_eq!(s.count(&script).unwrap(), 50);
    let mut c = TopScoreCollector::new(100, u64::MAX);
    s.search(&script, &mut c).unwrap();
    assert!(c.top_docs().hits.iter().all(|h| h.doc % 2 == 0 && h.score == 1.0));
    // In a conjunction the term leads: the script runs on its 10 candidates only.
    calls.store(0, std::sync::atomic::Ordering::Relaxed);
    let q: search_index::QueryRef = Arc::new(BooleanQuery::new().must(Arc::new(TermQuery::new(T, "rare"))).filter(script.clone()));
    assert_eq!(s.count(&q).unwrap(), 10);
    assert!(calls.load(std::sync::atomic::Ordering::Relaxed) <= 10);
    let e = s.explain(&script, 2).unwrap();
    assert!(e.is_match());
    assert!(e.description.starts_with("ScriptQuery(Script{type=inline"), "{}", e.description);
    assert!(!s.explain(&script, 3).unwrap().is_match());
    let failing: search_index::QueryRef = Arc::new(ScriptQuery { script: Arc::new(Even { fail_at: Some(77), calls }) });
    let mut c = TopScoreCollector::new(10, u64::MAX);
    assert_eq!(s.search(&failing, &mut c).unwrap_err().to_string(), "boom");
}

/// `CappedScoreQuery`: scores and explanations capped, top-k over capped scores intact.
#[test]
fn capped_scores() {
    use search_index::query::{BoostQuery, CappedScoreQuery};
    let shard = Shard::in_memory(schema());
    let a = Analysis::global().analyzer("standard").unwrap();
    for i in 0..300 {
        let text = format!("fox {}", "fox ".repeat(i % 7)) + &"pad ".repeat(i % 13);
        shard.index(IndexOp::new(IndexableDoc::new(format!("{i}").into_bytes()).text(T, &a, text))).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let raw: search_index::QueryRef = Arc::new(TermQuery::new(T, "fox"));
    let mut all = TopScoreCollector::new(300, u64::MAX);
    s.search(&raw, &mut all).unwrap();
    let all = all.top_docs();
    let cap = all.hits[150].score;
    // A boost under the cap is dropped by the rewrite (Elasticsearch).
    let q: search_index::QueryRef = Arc::new(CappedScoreQuery::new(Arc::new(BoostQuery::new(raw.clone(), 3.0)), cap).unwrap());
    let mut want: Vec<(u32, f32)> = all.hits.iter().map(|h| (h.doc, h.score.min(cap))).collect();
    want.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
    for k in [1usize, 10, 200] {
        let mut c = TopScoreCollector::new(k, k as u64);
        s.search(&q, &mut c).unwrap();
        let got: Vec<(u32, f32)> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
        assert_eq!(got, want[..k], "top {k}");
    }
    let top = all.hits[0].doc;
    let e = s.explain(&q, top).unwrap();
    assert_eq!(e.value(), cap);
    assert_eq!(e.description, "Capped score of t:fox, max of");
    assert_eq!(e.details[1].description, "maximum score");
    let low = all.hits[299].doc;
    assert!(s.explain(&q, low).unwrap().description.starts_with("weight(t:fox"));
    assert_eq!(q.lucene_string(s.schema()), "CappedScore((t:fox)^3.0)");
    assert_eq!(
        CappedScoreQuery::new(raw, 0.0).unwrap_err().to_string(),
        "org.elasticsearch.xpack.searchbusinessrules.CappedScoreQuery maxScore must be >0, 0.0 supplied."
    );
}

/// `BinaryDvConfirmedQuery` (the `wildcard` field): n-gram approximation confirmed against
/// the stored values.
#[test]
fn binary_dv_confirmed_query_filters_candidates() {
    use search_codec::DocValuesType;
    use search_index::doc::DocValue;
    use search_index::query::{BinaryDvConfirmedQuery, BooleanQuery};
    const W: u32 = 11;
    const NG: u32 = 12;
    let schema = schema()
        .with_field(FieldSchema::new(W, "w").with_doc_values(DocValuesType::Binary))
        .unwrap()
        .with_field(FieldSchema::new(NG, "w._ngram").indexed(IndexOptions::Docs, false))
        .unwrap();
    let shard = Shard::in_memory(schema);
    let values = ["foobar", "barfoo", "fobar", "xfooz", "oof"];
    for (i, v) in values.iter().enumerate() {
        let mut d = IndexableDoc::new(format!("{i}").into_bytes()).doc_value(W, DocValue::Binary(v.as_bytes().to_vec().into()));
        let b = v.as_bytes();
        for g in b.windows(2) {
            d = d.term(NG, g.to_vec());
        }
        shard.index(IndexOp::new(d)).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    // `*foo*`: candidates have the grams "fo" and "oo"; confirmation needs "foo" in order.
    let approx: search_index::QueryRef = Arc::new(BooleanQuery::new().filter(Arc::new(TermQuery::new(NG, "fo"))).filter(Arc::new(TermQuery::new(NG, "oo"))));
    let q: search_index::QueryRef = Arc::new(BinaryDvConfirmedQuery {
        approx,
        field: W,
        matcher: Arc::new(|v: &[u8]| v.windows(3).any(|w| w == b"foo")),
        description: ":*foo*".into(),
    });
    let mut c = TopScoreCollector::new(10, u64::MAX);
    s.search(&q, &mut c).unwrap();
    let mut docs: Vec<u32> = c.top_docs().hits.iter().map(|h| h.doc).collect();
    docs.sort();
    assert_eq!(docs, vec![0, 1, 3]);
    assert_eq!(s.count(&q).unwrap(), 3);
    assert!(s.explain(&q, 0).unwrap().is_match());
    assert!(!s.explain(&q, 2).unwrap().is_match());
    assert_eq!(q.lucene_string(s.schema()), ":*foo*");
}

/// Point bounds across segments, per dimension (Lucene's `PointValues.getMin/MaxPackedValue`).
#[test]
fn point_bounds_span_segments_per_dimension() {
    use search_codec::PointsSpec;
    use search_codec::points::encode::{i32_bytes, i64_bytes};
    const N: u32 = 11;
    const P: u32 = 12;
    let schema = Schema::new()
        .with_field(FieldSchema::new(N, "n").with_points(PointsSpec::new(1, 8)))
        .unwrap()
        .with_field(FieldSchema::new(P, "p").with_points(PointsSpec::new(2, 4)))
        .unwrap();
    let shard = Shard::in_memory(schema);
    assert_eq!(shard.searcher().point_bounds(N), None);
    let pt = |a: i32, b: i32| [i32_bytes(a), i32_bytes(b)].concat();
    for (i, (n, p)) in [(5i64, (3, -7)), (-2, (9, 1)), (40, (-4, 0))].into_iter().enumerate() {
        let d = IndexableDoc::new(format!("{i}").into_bytes()).point(N, i64_bytes(n).to_vec()).point(P, pt(p.0, p.1));
        shard.index(IndexOp::new(d)).unwrap();
        shard.refresh().unwrap();
    }
    shard.index(IndexOp::new(IndexableDoc::new(b"none".to_vec()))).unwrap();
    shard.refresh().unwrap();
    let s = shard.searcher();
    assert!(s.segments().len() > 1);
    let b = s.point_bounds(N).unwrap();
    assert_eq!((b.min, b.max, b.doc_count), (i64_bytes(-2).to_vec(), i64_bytes(40).to_vec(), 3));
    let b = s.point_bounds(P).unwrap();
    assert_eq!((b.min, b.max), (pt(-4, -7), pt(9, 1)));
}

/// Lucene 10.5's range rewrites by the field's global bounds, as Elasticsearch 9.5.5 profiles
/// them: no overlap → `MatchNoDocsQuery("")`, covering every value → `*:*` when every doc has a
/// point, else `FieldExistsQuery`; `IndexOrDocValuesQuery` follows either side.
#[test]
fn range_rewrites_follow_lucene() {
    use search_codec::{DocValuesType, PointsSpec};
    use search_index::doc::DocValue;
    use search_index::query::{DocValuesRangeQuery, DvRange, IndexOrDocValuesQuery, PointRangeQuery};
    use search_index::QueryRef;
    const N: u32 = 11;
    let schema = Schema::new()
        .with_field(FieldSchema::new(N, "n").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap();
    let shard = Shard::in_memory(schema);
    for v in [5i64, 7, 9] {
        let d = IndexableDoc::new(format!("{v}").into_bytes())
            .point(N, search_codec::points::encode::i64_bytes(v).to_vec())
            .doc_value(N, DocValue::SortedNumeric(v));
        shard.index(IndexOp::new(d)).unwrap();
    }
    shard.refresh().unwrap();
    let rw = |s: &search_index::Searcher, q: QueryRef| s.rewrite(&q).unwrap().lucene_string(s.schema());
    let iodv = |lo: i64, hi: i64| -> QueryRef {
        Arc::new(IndexOrDocValuesQuery {
            index: Arc::new(PointRangeQuery::long(N, lo, hi)),
            dv: Arc::new(DocValuesRangeQuery { field: N, range: DvRange::Long { lower: lo, upper: hi } }),
        })
    };
    let s = shard.searcher();
    assert_eq!(rw(&s, iodv(1, 100)), "*:*");
    assert_eq!(rw(&s, iodv(10, 100)), "MatchNoDocsQuery(\"\")");
    assert_eq!(rw(&s, iodv(6, 100)), "IndexOrDocValuesQuery(indexQuery=n:[6 TO 100], dvQuery=n:[6 TO 100])");
    assert_eq!(rw(&s, Arc::new(DocValuesRangeQuery { field: N, range: DvRange::Long { lower: i64::MIN, upper: i64::MAX } })), "*:*");
    // A doc without the field: covering ranges become exists queries.
    shard.index(IndexOp::new(IndexableDoc::new(b"none".to_vec()))).unwrap();
    shard.refresh().unwrap();
    let s = shard.searcher();
    assert_eq!(rw(&s, Arc::new(PointRangeQuery::long(N, 1, 100))), "FieldExistsQuery [field=n]");
    assert_eq!(rw(&s, iodv(1, 100)), "IndexOrDocValuesQuery(indexQuery=FieldExistsQuery [field=n], dvQuery=n:[1 TO 100])");
    let mut c = TopScoreCollector::new(10, u64::MAX);
    s.search(&iodv(1, 100), &mut c).unwrap();
    assert_eq!(c.top_docs().total_hits, 3);
    // Points of another shape are an error, as in Lucene.
    let bad: QueryRef = Arc::new(PointRangeQuery::int(N, 1, 2));
    assert_eq!(
        s.rewrite(&bad).unwrap_err().to_string(),
        "field=\"n\" was indexed with bytesPerDim=8 but this query has bytesPerDim=4"
    );
}

/// `ConstantScoreQuery` rewrites its boolean without scoring (Lucene's `rewriteNoScoring`):
/// musts become filters and optional shoulds go. Elasticsearch 9.5.5 explains
/// `constant_score { bool { must: [a:x, b:y], should: [a:z] } }` as `ConstantScore(#a:x #b:y)^2.0`.
#[test]
fn constant_score_rewrites_its_boolean_without_scoring() {
    use search_index::query::{BooleanQuery, BoostQuery, ConstantScoreQuery};
    const A: u32 = 11;
    const B: u32 = 12;
    let schema = Schema::new()
        .with_field(FieldSchema::new(A, "a").indexed(IndexOptions::Docs, false))
        .unwrap()
        .with_field(FieldSchema::new(B, "b").indexed(IndexOptions::Docs, false))
        .unwrap();
    let shard = Shard::in_memory(schema);
    shard.index(IndexOp::new(IndexableDoc::new(b"1".to_vec()).term(A, "x").term(B, "y"))).unwrap();
    shard.refresh().unwrap();
    let s = shard.searcher();
    let t = |f: u32, v: &str| Arc::new(TermQuery::new(f, v)) as search_index::QueryRef;
    let b = BooleanQuery::new().must(t(A, "x")).must(t(B, "y")).should(t(A, "z"));
    let q: search_index::QueryRef = Arc::new(BoostQuery::new(Arc::new(ConstantScoreQuery::new(Arc::new(b))), 2.0));
    let e = s.explain(&q, 0).unwrap();
    assert_eq!(e.description, "ConstantScore(#a:x #b:y)^2.0");
    assert_eq!(e.value(), 2.0);
    // Shoulds stay when nothing else is required, or with minimum_should_match.
    let b = BooleanQuery::new().should(t(A, "x")).should(t(B, "y")).must_not(t(A, "z"));
    let q: search_index::QueryRef = Arc::new(ConstantScoreQuery::new(Arc::new(b)));
    assert_eq!(s.rewrite(&q).unwrap().lucene_string(s.schema()), "ConstantScore(a:x b:y -a:z)");
}

/// A 0-boosted match-none (a filter clause that matches nothing, nested a few levels deep)
/// rewrites once to match-none: it used to ping-pong with its constant-score wrapper, and the
/// nested rewrites multiplied that into a hang.
#[test]
fn zero_boosted_match_none_rewrites_once() {
    use search_index::query::{BooleanQuery, BoostQuery, ConstantScoreQuery, MatchNoneQuery, Occur};
    let shard = Shard::in_memory(schema());
    let a = Analysis::global().analyzer("standard").unwrap();
    shard.index(IndexOp::new(IndexableDoc::new(b"1".to_vec()).text(T, &a, "quick fox"))).unwrap();
    assert!(shard.refresh().unwrap());
    let s = shard.searcher();
    let mut q: search_index::QueryRef = Arc::new(BoostQuery { query: Arc::new(MatchNoneQuery::new("empty BooleanQuery")), boost: 0.0 });
    for _ in 0..8 {
        q = Arc::new(ConstantScoreQuery::new(Arc::new(BooleanQuery::new().add(Occur::Filter, q).add(Occur::Should, Arc::new(TermQuery::new(T, "fox"))))));
    }
    let started = std::time::Instant::now();
    let r = s.rewrite(&q).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "rewrite took {:?}", started.elapsed());
    assert!(r.as_any().is::<MatchNoneQuery>(), "{}", r.lucene_string(s.schema()));
    let mut c = TopScoreCollector::new(10, u64::MAX);
    s.search(&q, &mut c).unwrap();
    assert_eq!(c.top_docs().total_hits, 0);
}

/// Lucene 10.5's `DisjunctionMaxQuery.rewrite` drops disjuncts that rewrite to match-none, then
/// collapses what is left: one disjunct is that disjunct, none is match-none.
#[test]
fn dismax_drops_match_none_disjuncts() {
    use search_index::query::{DisMaxQuery, MatchNoneQuery};
    let shard = Shard::in_memory(schema());
    let a = Analysis::global().analyzer("standard").unwrap();
    shard.index(IndexOp::new(IndexableDoc::new(b"1".to_vec()).text(T, &a, "quick fox"))).unwrap();
    assert!(shard.refresh().unwrap());
    let s = shard.searcher();
    let none = || -> search_index::QueryRef { Arc::new(MatchNoneQuery::new("no")) };
    let inner: search_index::QueryRef = Arc::new(DisMaxQuery::new(vec![none()], 0.0));
    let q: search_index::QueryRef = Arc::new(DisMaxQuery::new(vec![Arc::new(TermQuery::new(T, "fox")), inner], 0.0));
    let r = s.rewrite(&q).unwrap();
    assert!(r.as_any().is::<TermQuery>(), "{}", r.lucene_string(s.schema()));
    let q: search_index::QueryRef = Arc::new(DisMaxQuery::new(vec![none(), Arc::new(DisMaxQuery::new(vec![none()], 0.0))], 0.3));
    let r = s.rewrite(&q).unwrap();
    assert!(r.as_any().is::<MatchNoneQuery>(), "{}", r.lucene_string(s.schema()));
    // Nothing to drop: the query stays a disjunction (of its rewritten clauses).
    let q: search_index::QueryRef = Arc::new(DisMaxQuery::new(vec![Arc::new(TermQuery::new(T, "fox")), Arc::new(TermQuery::new(T, "quick"))], 0.3));
    assert!(s.rewrite(&q).unwrap().as_any().is::<DisMaxQuery>());
}

/// Lucene's `WildcardQuery.toAutomaton("")` concatenates no automata: the empty language. The
/// empty pattern matches no term, not even the empty one.
#[test]
fn empty_wildcard_pattern_matches_nothing() {
    use search_index::query::MultiTermQuery;
    const K: u32 = 11;
    let schema = schema().with_field(FieldSchema::new(K, "k").indexed(IndexOptions::Docs, false)).unwrap();
    let shard = Shard::in_memory(schema);
    shard.index(IndexOp::new(IndexableDoc::new(b"1".to_vec()).term(K, ""))).unwrap();
    shard.index(IndexOp::new(IndexableDoc::new(b"2".to_vec()).term(K, "a"))).unwrap();
    assert!(shard.refresh().unwrap());
    let s = shard.searcher();
    let count = |pattern: &str, ci: bool| {
        let q: search_index::QueryRef = Arc::new(MultiTermQuery::wildcard(K, pattern, ci).unwrap());
        let mut c = TopScoreCollector::new(10, u64::MAX);
        s.search(&q, &mut c).unwrap();
        c.top_docs().total_hits
    };
    assert_eq!(count("", false), 0);
    assert_eq!(count("", true), 0);
    assert_eq!(count("*", false), 2);
    assert_eq!(count("?", false), 1);
}

/// Point ranges over segments where every doc has one value take the inverse walk (clear the
/// docs outside from a full set) when most docs match; constant-score top hits past the hit
/// threshold end a segment once its score can't compete, and sorted collection keeps its
/// competitive iterator under `constant_score`. Every result equals brute force.
#[test]
fn dense_point_ranges_and_constant_score_pruning_match_brute_force() {
    use rand::{Rng, SeedableRng};
    use search_codec::points::encode::{i32_bytes, i64_bytes};
    use search_codec::{DocValuesType, PointsSpec};
    use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, TopFieldCollector, TotalRelation};
    use search_index::doc::DocValue;
    use search_index::query::{BooleanQuery, ConstantScoreQuery, DocValuesRangeQuery, DvRange, PointRangeQuery};
    use search_index::{DeleteOp, QueryRef, ShardConfig};
    const N: u32 = 11;
    const P: u32 = 12;
    // The two dimensions of P as doc values: the brute-force side of every check.
    const P0: u32 = 13;
    const P1: u32 = 14;
    let schema = Schema::new()
        .with_field(FieldSchema::new(N, "n").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(P, "p").with_points(PointsSpec::new(2, 4)))
        .unwrap()
        .with_field(FieldSchema::new(P0, "p0").with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(P1, "p1").with_doc_values(DocValuesType::SortedNumeric))
        .unwrap();
    for seed in 0..4u64 {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema.clone(), Vec::new()).unwrap();
        let mut docs: Vec<Option<(i64, (i32, i32))>> = Vec::new();
        for i in 0..rng.gen_range(3000..9000u32) {
            // Mostly increasing values (time series: whole leaves inside or outside), some noise.
            let n = i as i64 * 3 + rng.gen_range(-50..50);
            let p = (rng.gen_range(-1000..1000), rng.gen_range(-1000..1000));
            let mut d = IndexableDoc::new(format!("{i}").into_bytes()).point(N, i64_bytes(n).to_vec()).doc_value(N, DocValue::SortedNumeric(n));
            d = d.point(P, [i32_bytes(p.0), i32_bytes(p.1)].concat());
            d = d.doc_value(P0, DocValue::SortedNumeric(p.0 as i64)).doc_value(P1, DocValue::SortedNumeric(p.1 as i64));
            shard.index(IndexOp::new(d)).unwrap();
            docs.push(Some((n, p)));
            if rng.gen_bool(0.0008) {
                shard.refresh().unwrap();
            }
        }
        // One segment with a doc missing the points (not every doc valued: the plain walk).
        if seed % 2 == 1 {
            shard.refresh().unwrap();
            shard.index(IndexOp::new(IndexableDoc::new(b"bare".to_vec()))).unwrap();
            docs.push(None);
            for _ in 0..50 {
                let i = docs.len() as u32;
                let n = i as i64 * 3;
                let d = IndexableDoc::new(format!("{i}").into_bytes()).point(N, i64_bytes(n).to_vec()).doc_value(N, DocValue::SortedNumeric(n)).point(P, [i32_bytes(0), i32_bytes(0)].concat()).doc_value(P0, DocValue::SortedNumeric(0)).doc_value(P1, DocValue::SortedNumeric(0));
                shard.index(IndexOp::new(d)).unwrap();
                docs.push(Some((n, (0, 0))));
            }
        }
        shard.refresh().unwrap();
        if seed >= 2 {
            for _ in 0..40 {
                let i = rng.gen_range(0..docs.len());
                let id = if docs[i].is_none() { "bare".to_string() } else { format!("{i}") };
                shard.delete(DeleteOp::new(id.into_bytes())).unwrap();
                docs[i] = None;
            }
            shard.refresh().unwrap();
        }
        let s = shard.searcher();
        let max = docs.len() as i64 * 3;
        let ids = |q: &QueryRef| -> Vec<u32> {
            let mut c = TopScoreCollector::new(docs.len() + 10, u64::MAX);
            s.search(q, &mut c).unwrap();
            let mut v: Vec<u32> = c.top_docs().hits.iter().map(|h| h.doc).collect();
            v.sort();
            v
        };
        let dv = |f: u32, lower: i64, upper: i64| -> QueryRef { Arc::new(DocValuesRangeQuery { field: f, range: DvRange::Long { lower, upper } }) };
        for round in 0..30 {
            let (lo, hi) = match round % 3 {
                0 => (rng.gen_range(-100..max / 4), rng.gen_range(max / 2..max + 100)),
                1 => (i64::MIN, rng.gen_range(0..max)),
                _ => {
                    let a = rng.gen_range(-100..max + 100);
                    let b = rng.gen_range(-100..max + 100);
                    (a.min(b), a.max(b))
                }
            };
            let q: QueryRef = Arc::new(PointRangeQuery::long(N, lo, hi));
            let want = ids(&dv(N, lo, hi));
            assert_eq!(want.len(), docs.iter().filter(|d| d.is_some_and(|(n, _)| n >= lo && n <= hi)).count());
            assert_eq!(ids(&q), want, "seed {seed} long [{lo}, {hi}]");
            // Two dimensions: the inverse walk checks both on crossing cells.
            let (a0, a1) = (rng.gen_range(-1100..0), rng.gen_range(0..1100));
            let (b0, b1) = (rng.gen_range(-1100..-500), rng.gen_range(-200..1100));
            let q2: QueryRef = Arc::new(PointRangeQuery::new(P, [i32_bytes(a0), i32_bytes(b0)].concat(), [i32_bytes(a1), i32_bytes(b1)].concat(), 4, search_index::query::PointType::Int).unwrap());
            let want2 = ids(&(Arc::new(BooleanQuery::new().filter(dv(P0, a0 as i64, a1 as i64)).filter(dv(P1, b0 as i64, b1 as i64))) as QueryRef));
            assert_eq!(want2.len(), docs.iter().filter(|d| d.is_some_and(|(_, p)| p.0 >= a0 && p.0 <= a1 && p.1 >= b0 && p.1 <= b1)).count());
            assert_eq!(ids(&q2), want2, "seed {seed} 2-d [{a0}..{a1}] x [{b0}..{b1}]");
            // Constant-score top hits past the threshold: the first `k` matching docs, a total
            // that is a lower bound of the matches.
            let cq: QueryRef = Arc::new(ConstantScoreQuery::new(q.clone()));
            for (k, threshold) in [(10usize, 100u64), (1, 0), (500, 1000)] {
                let full = {
                    let mut c = TopScoreCollector::new(k, u64::MAX);
                    s.search(&cq, &mut c).unwrap();
                    c.top_docs()
                };
                for qq in [&q, &cq] {
                    let mut c = TopScoreCollector::new(k, threshold);
                    s.search(qq, &mut c).unwrap();
                    let got = c.top_docs();
                    let pairs = |t: &search_index::collector::TopDocs| t.hits.iter().map(|h| (h.doc, h.score.to_bits())).collect::<Vec<_>>();
                    if Arc::ptr_eq(qq, &cq) {
                        assert_eq!(pairs(&got), pairs(&full), "seed {seed} k={k} t={threshold} [{lo}, {hi}]");
                    } else {
                        assert_eq!(got.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), full.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), "seed {seed} [{lo}, {hi}]");
                    }
                    if got.relation == TotalRelation::EqualTo {
                        assert_eq!(got.total_hits, full.total_hits, "seed {seed} k={k} t={threshold}");
                    } else {
                        assert!(got.total_hits > threshold.min(full.total_hits.saturating_sub(1)) && got.total_hits <= full.total_hits, "seed {seed} k={k} t={threshold}: {} of {}", got.total_hits, full.total_hits);
                    }
                }
                // Sorted on the field under constant_score: pruned equals exhaustive.
                let key = SortKey::Field(FieldSort { field: N, kind: SortType::Long, reverse: round % 2 == 0, missing: Missing::Last, mode: SortMode::Min, nested: None });
                let sorted = |t: u64| {
                    let mut c = TopFieldCollector::new(vec![key.clone()], k, t, false).with_searcher(&s);
                    s.search(&cq, &mut c).unwrap();
                    c.top_docs().hits.iter().map(|h| h.doc).collect::<Vec<_>>()
                };
                assert_eq!(sorted(threshold), sorted(u64::MAX), "seed {seed} sorted k={k} t={threshold}");
            }
        }
    }
}

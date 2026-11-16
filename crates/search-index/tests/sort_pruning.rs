//! Randomized check that sorted top hits with a small total-hits threshold — where the
//! collector prunes with points (numeric competitive iterators), keyword ordinal bottoms and
//! index-sort early termination — equal exhaustive sorting, for every numeric type, keywords,
//! multi-key sorts with `_score` / `_doc`, missing values (`_first`, `_last`, a value), every
//! multi-value mode, `search_after`, deletes, updates and many segments, with and without an
//! index sort. `SORT_SEEDS=<n>` sweeps more corpora.

mod common;

use std::sync::Arc;

use rand::Rng;
use rand::rngs::StdRng;
use search_codec::points::encode;
use search_codec::{DocValuesType, IndexOptions, IndexSort, PointsSpec, SortField, SortKind, SortMissing, SortSelector};
use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, SortValue, TopFieldCollector, TotalRelation};
use search_index::doc::{DocValue, IndexableDoc, PreToken};
use search_index::query::{BooleanQuery, MatchAllQuery, TermQuery};
use search_index::schema::FieldSchema;
use search_index::collector::TopFieldManager;
use search_index::search::SearchLimits;
use search_index::{DeleteOp, IndexOp, QueryRef, Schema, SearchControl, Shard, ShardConfig};

const L: u32 = 20;
const I: u32 = 21;
const F: u32 = 22;
const D: u32 = 23;
const K: u32 = 24;
const W: u32 = 25;

fn schema(sort: Option<IndexSort>) -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(L, "l").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(I, "i").with_points(PointsSpec::new(1, 4)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(F, "f").with_points(PointsSpec::new(1, 4)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(D, "d").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(K, "k").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet))
        .unwrap()
        .with_field(FieldSchema::new(W, "w").indexed(IndexOptions::DocsAndFreqs, true))
        .unwrap()
        .with_index_sort(sort)
}

/// Value shapes: few distinct values (ties), increasing with noise (time series), uniform.
#[derive(Clone, Copy)]
enum Shape {
    Ties,
    Increasing,
    Uniform,
}

fn value(rng: &mut StdRng, shape: Shape, i: u32) -> i64 {
    match shape {
        Shape::Ties => rng.gen_range(-3..4),
        Shape::Increasing => i as i64 * 7 + rng.gen_range(-200..200),
        Shape::Uniform => rng.gen_range(-1_000_000..1_000_000),
    }
}

fn doc(rng: &mut StdRng, shape: Shape, i: u32, missing: f64, multi: f64) -> IndexableDoc {
    let mut d = IndexableDoc::new(format!("d{i}").into_bytes());
    let n = if rng.gen_bool(missing) { 0 } else if rng.gen_bool(multi) { rng.gen_range(2..4) } else { 1 };
    for _ in 0..n {
        let v = value(rng, shape, i);
        d = d.point(L, encode::i64_bytes(v).to_vec()).doc_value(L, DocValue::SortedNumeric(v));
        let iv = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        d = d.point(I, encode::i32_bytes(iv).to_vec()).doc_value(I, DocValue::SortedNumeric(iv as i64));
        let fv = v as f32 / 8.0;
        d = d.point(F, encode::f32_bytes(fv).to_vec()).doc_value(F, DocValue::SortedNumeric(encode::f32_sortable(fv) as i64));
        let dv = v as f64 / 3.0;
        d = d.point(D, encode::f64_bytes(dv).to_vec()).doc_value(D, DocValue::SortedNumeric(encode::f64_sortable(dv)));
        let k = format!("k{:03}", v.rem_euclid(500));
        d = d.term(K, k.clone().into_bytes()).doc_value(K, DocValue::SortedSet(k.into_bytes().into()));
    }
    let words: Vec<PreToken> = (0..rng.gen_range(1..6))
        .map(|p| {
            let w = format!("w{}", common::zipf(rng, 8));
            PreToken::new(w.into_bytes(), 1, p * 3, p * 3 + 2)
        })
        .collect();
    d.field(search_index::doc::IndexableField::Tokens { field: W, tokens: words })
}

fn random_key(rng: &mut StdRng) -> SortKey {
    match rng.gen_range(0..10) {
        0 => SortKey::Score { reverse: rng.gen_bool(0.2) },
        1 => SortKey::Doc { reverse: rng.gen_bool(0.3) },
        _ => {
            let (field, kind) = [(L, SortType::Long), (I, SortType::Int), (F, SortType::Float), (D, SortType::Double), (K, SortType::Keyword)]
                [rng.gen_range(0..5)];
            let mode = if kind == SortType::Keyword || rng.gen_bool(0.6) {
                [SortMode::Min, SortMode::Max][rng.gen_range(0..2)]
            } else {
                [SortMode::Sum, SortMode::Avg, SortMode::Median][rng.gen_range(0..3)]
            };
            let missing = match rng.gen_range(0..4) {
                0 => Missing::First,
                1 if kind != SortType::Keyword => Missing::Value(match kind {
                    SortType::Long | SortType::Int => SortValue::Long(rng.gen_range(-5..5)),
                    SortType::Float => SortValue::Float(rng.gen_range(-1.0..1.0)),
                    _ => SortValue::Double(rng.gen_range(-1.0..1.0)),
                }),
                _ => Missing::Last,
            };
            SortKey::Field(FieldSort { field, kind, reverse: rng.gen_bool(0.5), missing, mode, nested: None })
        }
    }
}

/// Runs `c` in one pass per segment, as a collector wrapped in another collects (bands off,
/// see `top_field::Band`): the reference banded collection must equal.
fn one_pass(s: &search_index::Searcher, q: &QueryRef, c: &mut TopFieldCollector) {
    use search_index::search::{Collector, LeafCollector, Scorable, ScoreMode, SegmentContext};
    struct OnePass<'c>(&'c mut TopFieldCollector);
    struct Leaf<'a>(Box<dyn LeafCollector + 'a>);
    impl Collector for OnePass<'_> {
        fn score_mode(&self) -> ScoreMode {
            self.0.score_mode()
        }
        fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafCollector + 'a>> {
            Ok(Box::new(Leaf(self.0.leaf(seg)?)))
        }
    }
    impl LeafCollector for Leaf<'_> {
        fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> search_index::Result<()> {
            self.0.collect(doc, scorer)
        }
        fn skips(&self) -> bool {
            self.0.skips()
        }
        fn competitive_from(&mut self, doc: u32) -> u32 {
            self.0.competitive_from(doc)
        }
    }
    s.search(q, &mut OnePass(c)).unwrap();
}

fn hits(td: &search_index::collector::TopFieldDocs) -> Vec<(u32, Vec<SortValue>)> {
    td.hits.iter().map(|h| (h.doc, h.values.clone())).collect()
}

#[test]
fn pruned_sorts_equal_exhaustive_sorts() {
    let seeds: u64 = std::env::var("SORT_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    for seed in 0..seeds {
        let mut rng = common::rng(seed * 101 + 3);
        let shape = [Shape::Ties, Shape::Increasing, Shape::Uniform][rng.gen_range(0..3)];
        let index_sort = match rng.gen_range(0..3) {
            0 => Some(IndexSort {
                fields: vec![SortField {
                    field: L,
                    kind: SortKind::Long,
                    reverse: rng.gen_bool(0.5),
                    missing: if rng.gen_bool(0.5) { SortMissing::First } else { SortMissing::Last },
                    selector: if rng.gen_bool(0.5) { SortSelector::Min } else { SortSelector::Max },
                }],
            }),
            1 => Some(IndexSort {
                fields: vec![SortField { field: K, kind: SortKind::String, reverse: rng.gen_bool(0.5), missing: SortMissing::Last, selector: SortSelector::Min }],
            }),
            _ => None,
        };
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(index_sort.clone()), Vec::new()).unwrap();
        let n = rng.gen_range(500..6000u32);
        let (missing, multi) = (rng.gen_range(0.0..0.3), rng.gen_range(0.0..0.3));
        for i in 0..n {
            shard.index(IndexOp::new(doc(&mut rng, shape, i, missing, multi))).unwrap();
            if rng.gen_bool(0.002) {
                shard.refresh().unwrap();
            }
            if rng.gen_bool(0.05) {
                let victim = format!("d{}", rng.gen_range(0..=i));
                if rng.gen_bool(0.5) {
                    shard.delete(DeleteOp::new(victim.into_bytes())).unwrap();
                } else {
                    let j: u32 = victim[1..].parse().unwrap();
                    shard.index(IndexOp::new(doc(&mut rng, shape, j, missing, multi))).unwrap();
                }
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let queries: Vec<QueryRef> = vec![
            Arc::new(MatchAllQuery),
            Arc::new(TermQuery::new(W, "w1")),
            Arc::new(BooleanQuery::new().should(Arc::new(TermQuery::new(W, "w0"))).should(Arc::new(TermQuery::new(W, "w3")))),
        ];
        for round in 0..40 {
            let q = &queries[rng.gen_range(0..queries.len())];
            let keys: Vec<SortKey> = (0..rng.gen_range(1..4)).map(|_| random_key(&mut rng)).collect();
            let k = [1usize, 3, 10, 40][rng.gen_range(0..4)];
            let threshold = [1u64, k as u64, 50, 1000][rng.gen_range(0..4)];
            let scores = rng.gen_bool(0.3);
            let what = format!("seed {seed} round {round}: k={k} threshold={threshold} keys={keys:?} index_sort={index_sort:?}");
            let full = {
                let mut c = TopFieldCollector::new(keys.clone(), k, u64::MAX, scores).with_searcher(&s);
                one_pass(&s, q, &mut c);
                c.top_docs()
            };
            let mut c = TopFieldCollector::new(keys.clone(), k, threshold, scores).with_searcher(&s);
            s.search(q, &mut c).unwrap();
            let got = c.top_docs();
            assert_eq!(hits(&got), hits(&full), "{what}");
            // Segments on their own threads, results merged.
            let par = s.with_limits(SearchLimits { parallel_min_docs: 16, parallelism: 3, ..SearchLimits::default() });
            let m = TopFieldManager { keys: keys.clone(), n: k, threshold, track_scores: scores, after: None, searcher: par.clone() };
            let conc = par.search_concurrent(q, &m, &SearchControl::default()).unwrap();
            assert_eq!(hits(&conc), hits(&full), "concurrent: {what}");
            if got.relation == TotalRelation::EqualTo {
                assert_eq!(got.total_hits, full.total_hits, "{what}");
            } else {
                assert!(got.total_hits <= full.total_hits && got.total_hits >= threshold.min(full.total_hits), "{what}");
            }
            // search_after a hit of the exhaustive order continues it.
            if full.hits.len() > 1 {
                let all = {
                    let mut c = TopFieldCollector::new(keys.clone(), k * 3, u64::MAX, false).with_searcher(&s);
                    one_pass(&s, q, &mut c);
                    c.top_docs()
                };
                let at = rng.gen_range(0..all.hits.len().min(k));
                let after = &all.hits[at];
                let mut c = TopFieldCollector::new(keys.clone(), k, threshold, false).with_searcher(&s).with_after(after.values.clone(), Some(after.doc));
                s.search(q, &mut c).unwrap();
                let want: Vec<u32> = all.hits.iter().skip(at + 1).take(k).map(|h| h.doc).collect();
                let have: Vec<u32> = c.top_docs().hits.iter().map(|h| h.doc).collect();
                assert_eq!(have, want, "after #{at}: {what}");
            }
        }
    }
}

/// Elasticsearch averages (sums, medians) float values in double and compares the result
/// cast to float: `[1.1, 1.2]` averages to 1.1500001, `[1.0, 1.3]` to 1.15, tying with a
/// plain 1.15 (checked against Elasticsearch 9.5.5).
#[test]
fn float_keys_compare_at_float_precision() {
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
    for (id, vals) in [("1", vec![1.1f32, 1.2]), ("2", vec![1.15]), ("3", vec![1.0, 1.3])] {
        let mut d = IndexableDoc::new(id.as_bytes().to_vec());
        for v in vals {
            d = d.point(F, encode::f32_bytes(v).to_vec()).doc_value(F, DocValue::SortedNumeric(encode::f32_sortable(v) as i64));
        }
        shard.index(IndexOp::new(d)).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let q: QueryRef = Arc::new(MatchAllQuery);
    for (reverse, want) in [(false, vec![(1u32, 1.15f32), (2, 1.15), (0, 1.1500001)]), (true, vec![(0, 1.1500001), (1, 1.15), (2, 1.15)])] {
        let key = SortKey::Field(FieldSort { field: F, kind: SortType::Float, reverse, missing: Missing::Last, mode: SortMode::Avg, nested: None });
        let mut c = TopFieldCollector::new(vec![key], 10, u64::MAX, false).with_searcher(&s);
        s.search(&q, &mut c).unwrap();
        let got: Vec<(u32, Vec<SortValue>)> = hits(&c.top_docs());
        let want: Vec<(u32, Vec<SortValue>)> = want.into_iter().map(|(d, v)| (d, vec![SortValue::Float(v)])).collect();
        assert_eq!(got, want, "reverse={reverse}");
    }
}

/// `IndexSortRangeQuery` on index-sorted segments (both directions, missing first or last,
/// deletes, several segments, nested blocks falling back) matches the plain range query, and
/// counts sorted segments without visiting docs.
#[test]
fn index_sort_ranges_equal_plain_ranges() {
    use search_index::query::{DocValuesRangeQuery, DvRange, IndexOrDocValuesQuery, IndexSortRangeQuery, PointRangeQuery};
    let mut cheap_counts = 0;
    for seed in 0..24u64 {
        let mut rng = common::rng(seed * 7 + 1);
        let sort = IndexSort {
            fields: vec![SortField {
                field: L,
                kind: SortKind::Long,
                reverse: rng.gen_bool(0.5),
                missing: if rng.gen_bool(0.5) { SortMissing::First } else { SortMissing::Last },
                selector: SortSelector::Min,
            }],
        };
        let nested = seed % 4 == 3;
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(Some(sort.clone())), Vec::new()).unwrap();
        let n = rng.gen_range(200..3000u32);
        for i in 0..n {
            let mut d = IndexableDoc::new(format!("d{i}").into_bytes());
            if !rng.gen_bool(0.1) {
                let v = rng.gen_range(-50..50i64);
                d = d.point(L, encode::i64_bytes(v).to_vec()).doc_value(L, DocValue::SortedNumeric(v));
            }
            if nested && rng.gen_bool(0.3) {
                d = d.child(IndexableDoc::new(Vec::<u8>::new()).term(K, b"child".to_vec()));
            }
            shard.index(IndexOp::new(d)).unwrap();
            if rng.gen_bool(0.003) {
                shard.refresh().unwrap();
            }
            if seed % 3 != 0 && rng.gen_bool(0.03) {
                shard.delete(DeleteOp::new(format!("d{}", rng.gen_range(0..=i)).into_bytes())).unwrap();
            }
        }
        shard.refresh().unwrap();
        if rng.gen_bool(0.5) {
            shard.force_merge(Some(1), false).unwrap();
        }
        let s = shard.searcher();
        for _ in 0..30 {
            let lo = rng.gen_range(-60..60i64);
            let hi = lo + rng.gen_range(-5..40);
            let plain: QueryRef = Arc::new(IndexOrDocValuesQuery {
                index: Arc::new(PointRangeQuery::long(L, lo, hi)),
                dv: Arc::new(DocValuesRangeQuery { field: L, range: DvRange::Long { lower: lo, upper: hi } }),
            });
            let sorted: QueryRef = Arc::new(IndexSortRangeQuery { field: L, lower: lo, upper: hi, fallback: plain.clone() });
            let docs = |q: &QueryRef| {
                let mut c = search_index::collector::TopScoreCollector::new(100_000, u64::MAX);
                s.search(q, &mut c).unwrap();
                let mut v: Vec<u32> = c.top_docs().hits.iter().map(|h| h.doc).collect();
                v.sort();
                v
            };
            let want = docs(&plain);
            assert_eq!(docs(&sorted), want, "seed {seed} [{lo} TO {hi}] {sort:?} nested={nested}");
            assert_eq!(s.count(&sorted).unwrap(), want.len() as u64);
            let led: QueryRef = Arc::new(BooleanQuery::new().must(Arc::new(TermQuery::new(W, "w1"))).filter(sorted.clone()));
            let led_plain: QueryRef = Arc::new(BooleanQuery::new().must(Arc::new(TermQuery::new(W, "w1"))).filter(plain.clone()));
            assert_eq!(docs(&led), docs(&led_plain));
            assert_eq!(sorted.lucene_string(s.schema()), format!("l:[{lo} TO {hi}]"));
            if !nested && !s.segments().iter().any(|g| g.has_deletions()) {
                // Sorted segments answer counts from the doc id range alone.
                assert_eq!(s.count_if_cheap(&sorted).unwrap(), Some(want.len() as u64));
                cheap_counts += 1;
            }
        }
    }
    assert!(cheap_counts > 0);
}

/// A queue whose bottom changes on almost every doc (a descending sort over increasing
/// values): the pruner samples its updates after the first 256 and retries the latest bottom
/// later, which must not lose a hit; with `search_after` on both sides of the values.
#[test]
fn constantly_changing_bottoms_prune_exactly() {
    let mut rng = common::rng(77);
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
    // Increasing values first (the queue changes on every doc for a descending sort), then
    // middling values with a top value every 997 docs, earlier ones better (found only through
    // the points once the bottom settled); mirrored for ascending sorts every 991 docs.
    let n = 60_000u32;
    for i in 0..n {
        let v: i64 = if i < 12_000 {
            i as i64 * 10
        } else if i % 997 == 0 {
            // The earliest of these are the best: a pruner that skipped docs loses them.
            300_000 - i as i64
        } else if i % 991 == 0 {
            -300_000 + i as i64
        } else {
            rng.gen_range(0..100_000)
        };
        let d = IndexableDoc::new(format!("d{i}").into_bytes()).point(L, encode::i64_bytes(v).to_vec()).doc_value(L, DocValue::SortedNumeric(v));
        shard.index(IndexOp::new(d)).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let q: QueryRef = Arc::new(MatchAllQuery);
    for reverse in [true, false] {
        for field in [L] {
            let kind = SortType::Long;
            let key = SortKey::Field(FieldSort { field, kind, reverse, missing: Missing::Last, mode: SortMode::Min, nested: None });
            let all = {
                let mut c = TopFieldCollector::new(vec![key.clone()], 200, u64::MAX, false).with_searcher(&s);
                one_pass(&s, &q, &mut c);
                c.top_docs()
            };
            for k in [1usize, 10, 40] {
                let full = {
                    let mut c = TopFieldCollector::new(vec![key.clone()], k, u64::MAX, false).with_searcher(&s);
                    one_pass(&s, &q, &mut c);
                    c.top_docs()
                };
                let mut c = TopFieldCollector::new(vec![key.clone()], k, 1, false).with_searcher(&s);
                s.search(&q, &mut c).unwrap();
                assert_eq!(hits(&c.top_docs()), hits(&full), "reverse {reverse} field {field} k {k}");
                for at in [0usize, 5, 30] {
                    let after = &all.hits[at];
                    let mut c = TopFieldCollector::new(vec![key.clone()], k, 1, false).with_searcher(&s).with_after(after.values.clone(), Some(after.doc));
                    s.search(&q, &mut c).unwrap();
                    let want: Vec<u32> = all.hits.iter().skip(at + 1).take(k).map(|h| h.doc).collect();
                    let have: Vec<u32> = c.top_docs().hits.iter().map(|h| h.doc).collect();
                    assert_eq!(have, want, "after #{at}: reverse {reverse} field {field} k {k}");
                }
            }
        }
    }
}

/// `_doc` ascending as the first key (a scroll's `sort=_doc`) ends a segment once the queue is
/// full and the count passed the threshold, and with an exact total counts the rest of a
/// constant-score block at once: hits, totals and `max_score` must equal an exhaustive sort
/// (a queue too large to fill), with `search_after` (also a tie on `_doc` decided by a second
/// key), scores, deletes, many segments and concurrent slices.
#[test]
fn doc_order_sorts_stop_once_the_queue_is_full() {
    for seed in 0..4u64 {
        let mut rng = common::rng(seed * 31 + 7);
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
        let n = 4000u32;
        for i in 0..n {
            shard.index(IndexOp::new(doc(&mut rng, Shape::Uniform, i, 0.1, 0.1))).unwrap();
            if i % 700 == 699 {
                shard.refresh().unwrap();
            }
            if rng.gen_bool(0.04) {
                shard.delete(DeleteOp::new(format!("d{}", rng.gen_range(0..=i)).into_bytes())).unwrap();
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        assert!(s.segments().len() >= 3, "{} segments", s.segments().len());
        let par = s.with_limits(SearchLimits { parallel_min_docs: 16, parallelism: 3, ..SearchLimits::default() });
        let queries: Vec<QueryRef> = vec![Arc::new(MatchAllQuery), Arc::new(TermQuery::new(W, "w1"))];
        let long = SortKey::Field(FieldSort { field: L, kind: SortType::Long, reverse: false, missing: Missing::Last, mode: SortMode::Min, nested: None });
        let key_sets = [vec![SortKey::Doc { reverse: false }], vec![SortKey::Doc { reverse: false }, long.clone()]];
        for q in &queries {
            for keys in &key_sets {
                for scores in [false, true] {
                    let full = {
                        let mut c = TopFieldCollector::new(keys.clone(), n as usize + 1, u64::MAX, scores).with_searcher(&s);
                        s.search(q, &mut c).unwrap();
                        c.top_docs()
                    };
                    let all = hits(&full);
                    let mut afters: Vec<Option<(Vec<SortValue>, Option<u32>)>> = vec![None];
                    for at in [0usize, all.len() / 3, all.len().saturating_sub(2)] {
                        if let Some(h) = full.hits.get(at) {
                            afters.push(Some((h.values.clone(), Some(h.doc))));
                            if keys.len() == 2 {
                                // Same `_doc`, second key below the doc's own value: the doc itself follows.
                                let mut v = h.values.clone();
                                v[1] = SortValue::Long(i64::MIN);
                                afters.push(Some((v, None)));
                            }
                        }
                    }
                    for k in [1usize, 10, 700] {
                        for threshold in [0u64, 1, 100, u64::MAX] {
                            for after in &afters {
                                let want: Vec<(u32, Vec<SortValue>)> = match after {
                                    None => all.iter().take(k).cloned().collect(),
                                    Some((v, d)) => {
                                        let SortValue::Long(ad) = v[0] else { panic!("{v:?}") };
                                        let start = all.iter().position(|(doc, vals)| {
                                            i64::from(*doc) > ad || (i64::from(*doc) == ad && (vals.len() == 1 && d.is_some_and(|d| *doc > d) || vals.len() == 2 && vals[1] > v[1]))
                                        });
                                        start.map_or(Vec::new(), |p| all[p..].iter().take(k).cloned().collect())
                                    }
                                };
                                let what = format!("seed {seed} k={k} threshold={threshold} keys={keys:?} scores={scores} after={after:?}");
                                let mut c = TopFieldCollector::new(keys.clone(), k, threshold, scores).with_searcher(&s);
                                if let Some((v, d)) = after {
                                    c = c.with_after(v.clone(), *d);
                                }
                                s.search(q, &mut c).unwrap();
                                let got = c.top_docs();
                                assert_eq!(hits(&got), want, "{what}");
                                if threshold == u64::MAX {
                                    assert_eq!((got.total_hits, got.relation), (full.total_hits, TotalRelation::EqualTo), "{what}");
                                    assert_eq!(got.max_score.map(f32::to_bits), full.max_score.map(f32::to_bits), "{what}");
                                } else {
                                    assert!(got.total_hits <= full.total_hits && got.total_hits >= (threshold + 1).min(full.total_hits), "{what}");
                                }
                                let m = TopFieldManager { keys: keys.clone(), n: k, threshold, track_scores: scores, after: after.clone(), searcher: par.clone() };
                                let conc = par.search_concurrent(q, &m, &SearchControl::default()).unwrap();
                                assert_eq!(hits(&conc), want, "concurrent: {what}");
                                if threshold == u64::MAX {
                                    assert_eq!((conc.total_hits, conc.relation), (full.total_hits, TotalRelation::EqualTo), "concurrent: {what}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Segments where the sort field has no value at all (every doc sorts as missing: such a
/// segment ends once the queue is full, as Lucene's comparators do) and `search_after` bounds
/// past every value (pruning starts on the doc that passes the hit threshold even when that doc
/// sorts before the bound) give the exhaustive order, with every `missing` and tie-break.
#[test]
fn absent_fields_and_exhausted_afters_prune_exactly() {
    const UNMAPPED: u32 = 99;
    for seed in 0..6u64 {
        let mut rng = common::rng(seed * 17 + 5);
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
        let mut i = 0u32;
        // Segments alternate between valued (some docs missing) and fieldless.
        for chunk in 0..6 {
            let valued = chunk % 2 == seed as usize % 2 || rng.gen_bool(0.3);
            for _ in 0..rng.gen_range(50..700) {
                let missing = if valued { 0.2 } else { 1.0 };
                shard.index(IndexOp::new(doc(&mut rng, Shape::Ties, i, missing, 0.1))).unwrap();
                i += 1;
            }
            shard.refresh().unwrap();
        }
        let s = shard.searcher();
        let par = s.with_limits(SearchLimits { parallel_min_docs: 16, parallelism: 3, ..SearchLimits::default() });
        let queries: Vec<QueryRef> = vec![Arc::new(MatchAllQuery), Arc::new(TermQuery::new(W, "w1"))];
        for round in 0..60 {
            let q = &queries[rng.gen_range(0..queries.len())];
            let (field, kind) = [(L, SortType::Long), (I, SortType::Int), (D, SortType::Double), (K, SortType::Keyword), (UNMAPPED, SortType::Long), (UNMAPPED, SortType::Keyword)]
                [rng.gen_range(0..6)];
            let missing = match rng.gen_range(0..5) {
                0 => Missing::First,
                1 if kind != SortType::Keyword => Missing::Value(SortValue::Long(rng.gen_range(-4..5))),
                _ => Missing::Last,
            };
            let first = SortKey::Field(FieldSort { field, kind, reverse: rng.gen_bool(0.5), missing, mode: SortMode::Min, nested: None });
            let mut keys = vec![first];
            match rng.gen_range(0..4) {
                0 => keys.push(SortKey::Doc { reverse: true }),
                1 => keys.push(SortKey::Score { reverse: false }),
                2 => keys.push(random_key(&mut rng)),
                _ => {}
            }
            let k = [1usize, 3, 10, 40][rng.gen_range(0..4)];
            let threshold = [0u64, 1, k as u64, 200][rng.gen_range(0..4)];
            let scores = rng.gen_bool(0.3);
            let what = format!("seed {seed} round {round}: k={k} threshold={threshold} keys={keys:?}");
            let full = {
                let mut c = TopFieldCollector::new(keys.clone(), k, u64::MAX, scores).with_searcher(&s);
                one_pass(&s, q, &mut c);
                c.top_docs()
            };
            let mut c = TopFieldCollector::new(keys.clone(), k, threshold, scores).with_searcher(&s);
            s.search(q, &mut c).unwrap();
            let got = c.top_docs();
            assert_eq!(hits(&got), hits(&full), "{what}");
            assert!(got.total_hits <= full.total_hits && got.total_hits >= (threshold + 1).min(full.total_hits), "{what}");
            let m = TopFieldManager { keys: keys.clone(), n: k, threshold, track_scores: scores, after: None, searcher: par.clone() };
            let conc = par.search_concurrent(q, &m, &SearchControl::default()).unwrap();
            assert_eq!(hits(&conc), hits(&full), "concurrent: {what}");
            // `search_after` a value past (or before) every one, and after a hit of the full order.
            let mut afters: Vec<(Vec<SortValue>, Option<u32>)> = Vec::new();
            if kind != SortType::Keyword && keys.len() == 1 {
                afters.push((vec![SortValue::Long(1_000)], None));
                afters.push((vec![SortValue::Long(-1_000)], None));
            }
            let all = {
                let mut c = TopFieldCollector::new(keys.clone(), k * 3, u64::MAX, false).with_searcher(&s);
                one_pass(&s, q, &mut c);
                c.top_docs()
            };
            if let Some(h) = all.hits.get(rng.gen_range(0..all.hits.len().clamp(1, k))) {
                afters.push((h.values.clone(), Some(h.doc)));
            }
            for (v, d) in afters {
                let want = {
                    let mut c = TopFieldCollector::new(keys.clone(), k, u64::MAX, false).with_searcher(&s).with_after(v.clone(), d);
                    s.search(q, &mut c).unwrap();
                    hits(&c.top_docs())
                };
                let mut c = TopFieldCollector::new(keys.clone(), k, threshold, false).with_searcher(&s).with_after(v.clone(), d);
                s.search(q, &mut c).unwrap();
                assert_eq!(hits(&c.top_docs()), want, "after {v:?}/{d:?}: {what}");
                let m = TopFieldManager { keys: keys.clone(), n: k, threshold, track_scores: false, after: Some((v.clone(), d)), searcher: par.clone() };
                let conc = par.search_concurrent(q, &m, &SearchControl::default()).unwrap();
                assert_eq!(hits(&conc), want, "concurrent after {v:?}/{d:?}: {what}");
            }
        }
    }
}

/// Keyword sorts with a custom `missing` value order missing docs against every value exactly,
/// also when the missing value and a value of another segment (the queue's worst entry, a
/// `search_after`) fall between the same two terms of a segment, or the segment has no value
/// of the field at all. Expected orders from Elasticsearch 9.5.5 (one shard, the segments as
/// below).
#[test]
fn keyword_missing_values_order_like_elasticsearch() {
    fn kdoc(id: &str, v: Option<&str>) -> IndexableDoc {
        let d = IndexableDoc::new(id.as_bytes().to_vec());
        match v {
            Some(v) => d.term(K, v.as_bytes().to_vec()).doc_value(K, DocValue::SortedSet(v.as_bytes().to_vec().into())),
            None => d,
        }
    }
    let run = |segments: &[&[(&str, Option<&str>)]], missing: &str, reverse: bool, k: usize, threshold: u64| {
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
        for seg in segments {
            for (id, v) in seg.iter() {
                shard.index(IndexOp::new(kdoc(id, *v))).unwrap();
            }
            shard.refresh().unwrap();
        }
        let s = shard.searcher();
        let ids = common::ids(&s);
        let key = SortKey::Field(FieldSort {
            field: K,
            kind: SortType::Keyword,
            reverse,
            missing: Missing::Value(SortValue::Bytes(missing.as_bytes().to_vec())),
            mode: SortMode::Min,
            nested: None,
        });
        let mut c = TopFieldCollector::new(vec![key], k, threshold, false).with_searcher(&s);
        s.search(&(Arc::new(MatchAllQuery) as QueryRef), &mut c).unwrap();
        c.top_docs()
            .hits
            .iter()
            .map(|h| {
                let SortValue::Bytes(b) = &h.values[0] else { panic!("{:?}", h.values) };
                format!("{}:{}", ids[&h.doc], String::from_utf8_lossy(b))
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let n: Vec<(String, Option<&str>)> = (0..5).map(|i| (format!("n{i}"), Some("n"))).collect();
    let n: Vec<(&str, Option<&str>)> = n.iter().map(|(a, b)| (a.as_str(), *b)).collect();
    let amz: &[(&str, Option<&str>)] =
        &[("a0", Some("a")), ("a1", Some("a")), ("m0", None), ("m1", None), ("m2", None), ("z0", Some("z")), ("z1", Some("z"))];
    let two = [n.as_slice(), amz];
    for threshold in [u64::MAX, 0, 3] {
        assert_eq!(run(&two, "m", false, 6, threshold), "a0:a a1:a m0:m m1:m m2:m n0:n", "threshold {threshold}");
        assert_eq!(run(&two, "m", true, 6, threshold), "z0:z z1:z n0:n n1:n n2:n n3:n");
        assert_eq!(run(&two, "o", false, 9, threshold), "a0:a a1:a n0:n n1:n n2:n n3:n n4:n m0:o m1:o");
        assert_eq!(run(&two, "7", false, 4, threshold), "m0:7 m1:7 m2:7 a0:a");
        assert_eq!(run(&two, "zz", true, 4, threshold), "m0:zz m1:zz m2:zz z0:z");
        let valued: &[(&str, Option<&str>)] = &[("v0", Some("b")), ("v1", Some("c"))];
        let empty: &[(&str, Option<&str>)] = &[("e0", None), ("e1", None)];
        assert_eq!(run(&[valued, empty], "7", false, 2, threshold), "e0:7 e1:7");
        assert_eq!(run(&[valued, empty], "bb", false, 2, threshold), "v0:b e0:bb");
        assert_eq!(run(&[valued, empty], "zz", true, 2, threshold), "e0:zz e1:zz");
    }
}

/// Randomized keyword sorts (custom, first and last `missing`, min and max, both directions,
/// `search_after`, a second key) over segments with sparse, disjoint and absent terms, checked
/// against a brute-force model rather than the collector itself.
#[test]
fn keyword_sorts_with_missing_values_match_brute_force() {
    use std::cmp::Ordering;
    const ALPHABET: [&str; 8] = ["b", "d", "f", "h", "j", "l", "n", "p"];
    for seed in 0..8u64 {
        let mut rng = common::rng(seed * 31 + 7);
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
        let mut model: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        let mut i = 0;
        for _ in 0..rng.gen_range(2..7) {
            // Each segment draws from its own subset of the alphabet (or none at all).
            let subset: Vec<&str> = ALPHABET.iter().copied().filter(|_| rng.gen_bool(0.4)).collect();
            let missing = if subset.is_empty() { 1.0 } else { rng.gen_range(0.0..0.6) };
            // Single-valued segments read ordinals through the block-decoded fast path.
            let most = if rng.gen_bool(0.5) { 2 } else { 3 };
            for _ in 0..rng.gen_range(5..120) {
                let id = format!("d{i}");
                i += 1;
                let mut vals = Vec::new();
                if !rng.gen_bool(missing) {
                    for _ in 0..rng.gen_range(1..most) {
                        vals.push(subset[rng.gen_range(0..subset.len())].to_string());
                    }
                }
                let mut d = IndexableDoc::new(id.clone().into_bytes());
                for v in &vals {
                    d = d.term(K, v.clone().into_bytes()).doc_value(K, DocValue::SortedSet(v.clone().into_bytes().into()));
                }
                shard.index(IndexOp::new(d)).unwrap();
                model.insert(id, vals);
            }
            shard.refresh().unwrap();
        }
        let s = shard.searcher();
        let ids = common::ids(&s);
        let par = s.with_limits(SearchLimits { parallel_min_docs: 4, parallelism: 3, ..SearchLimits::default() });
        let q: QueryRef = Arc::new(MatchAllQuery);
        for round in 0..80 {
            let reverse = rng.gen_bool(0.5);
            let max = rng.gen_bool(0.4);
            let missing = match rng.gen_range(0..6) {
                0 => Missing::First,
                1 => Missing::Last,
                _ => Missing::Value(SortValue::Bytes(["a", "c", "d", "g", "o", "q", "", "dd"][rng.gen_range(0..8)].as_bytes().to_vec())),
            };
            let second = rng.gen_bool(0.3);
            let mut keys = vec![SortKey::Field(FieldSort {
                field: K,
                kind: SortType::Keyword,
                reverse,
                missing: missing.clone(),
                mode: if max { SortMode::Max } else { SortMode::Min },
                nested: None,
            })];
            if second {
                keys.push(SortKey::Doc { reverse: true });
            }
            // Model: (global doc, key) in the expected order.
            let key_of = |vals: &Vec<String>| -> Option<Vec<u8>> {
                let v = if max { vals.iter().max() } else { vals.iter().min() };
                match (v, &missing) {
                    (Some(v), _) => Some(v.clone().into_bytes()),
                    (None, Missing::Value(SortValue::Bytes(b))) => Some(b.clone()),
                    _ => None,
                }
            };
            let first = matches!(missing, Missing::First);
            let order = |a: &(u32, Option<Vec<u8>>), b: &(u32, Option<Vec<u8>>)| -> Ordering {
                let o = match (&a.1, &b.1) {
                    (None, None) => Ordering::Equal,
                    (None, _) => if first { Ordering::Less } else { Ordering::Greater },
                    (_, None) => if first { Ordering::Greater } else { Ordering::Less },
                    (Some(x), Some(y)) => if reverse { y.cmp(x) } else { x.cmp(y) },
                };
                o.then_with(|| if second { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) })
            };
            let mut exp: Vec<(u32, Option<Vec<u8>>)> = ids.iter().map(|(g, id)| (*g, key_of(&model[id]))).collect();
            exp.sort_by(order);
            let k = [1usize, 2, 5, 17, 60][rng.gen_range(0..5)];
            let threshold = [0u64, 3, u64::MAX][rng.gen_range(0..3)];
            let what = format!("seed {seed} round {round}: k={k} threshold={threshold} keys={keys:?}");
            let shown = |v: &Option<Vec<u8>>| match v {
                Some(b) => SortValue::Bytes(b.clone()),
                None => SortValue::Null,
            };
            let want: Vec<(u32, SortValue)> = exp.iter().take(k).map(|(g, v)| (*g, shown(v))).collect();
            let got = |td: search_index::collector::TopFieldDocs| -> Vec<(u32, SortValue)> { td.hits.iter().map(|h| (h.doc, h.values[0].clone())).collect() };
            let mut c = TopFieldCollector::new(keys.clone(), k, threshold, false).with_searcher(&s);
            s.search(&q, &mut c).unwrap();
            assert_eq!(got(c.top_docs()), want, "{what}");
            let m = TopFieldManager { keys: keys.clone(), n: k, threshold, track_scores: false, after: None, searcher: par.clone() };
            assert_eq!(got(par.search_concurrent(&q, &m, &SearchControl::default()).unwrap()), want, "concurrent: {what}");
            // search_after a hit of the model's order continues it.
            let at = rng.gen_range(0..exp.len());
            let (ad, av) = &exp[at];
            let mut after = vec![shown(av)];
            if second {
                after.push(SortValue::Long(*ad as i64));
            }
            let want: Vec<(u32, SortValue)> = exp.iter().skip(at + 1).take(k).map(|(g, v)| (*g, shown(v))).collect();
            let mut c = TopFieldCollector::new(keys.clone(), k, threshold, false).with_searcher(&s).with_after(after.clone(), Some(*ad));
            s.search(&q, &mut c).unwrap();
            assert_eq!(got(c.top_docs()), want, "after #{at} {after:?}: {what}");
        }
    }
}

/// Sorts on one single-valued field (numeric, sorted-numeric, sorted and sorted-set doc
/// values, which read values block by block and compare them with the queue's worst entry
/// directly) with `_first`, `_last` and custom missing values, checked against a brute-force
/// model, also when the worst queued hit is missing its value.
#[test]
fn single_valued_sorts_match_brute_force() {
    use std::cmp::Ordering;
    const NUMERIC: u32 = 40;
    const SNUM: u32 = 41;
    const SORTED: u32 = 42;
    const SSET: u32 = 43;
    let schema = Schema::new()
        .with_field(FieldSchema::new(NUMERIC, "numeric").with_doc_values(DocValuesType::Numeric))
        .unwrap()
        .with_field(FieldSchema::new(SNUM, "snum").with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(SORTED, "sorted").with_doc_values(DocValuesType::Sorted))
        .unwrap()
        .with_field(FieldSchema::new(SSET, "sset").with_doc_values(DocValuesType::SortedSet))
        .unwrap();
    let term = |v: i64| format!("k{v:02}").into_bytes();
    for seed in 0..6u64 {
        let mut rng = common::rng(seed * 13 + 3);
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema.clone(), Vec::new()).unwrap();
        let mut model: Vec<Option<i64>> = Vec::new();
        for _ in 0..rng.gen_range(1..6) {
            // Each segment has its own value range and share of docs without a value (all, at
            // times).
            let lo = rng.gen_range(0..20i64);
            let hi = lo + rng.gen_range(1..12);
            let missing = [0.0, 0.3, 0.8, 1.0][rng.gen_range(0..4)];
            for _ in 0..rng.gen_range(5..150) {
                let id = format!("d{}", model.len());
                let v = (!rng.gen_bool(missing)).then(|| rng.gen_range(lo..hi));
                let mut d = IndexableDoc::new(id.into_bytes());
                if let Some(v) = v {
                    d = d
                        .doc_value(NUMERIC, DocValue::Numeric(v))
                        .doc_value(SNUM, DocValue::SortedNumeric(v))
                        .doc_value(SORTED, DocValue::Sorted(term(v).into()))
                        .doc_value(SSET, DocValue::SortedSet(term(v).into()));
                }
                shard.index(IndexOp::new(d)).unwrap();
                model.push(v);
            }
            shard.refresh().unwrap();
        }
        let s = shard.searcher();
        let ids = common::ids(&s);
        let q: QueryRef = Arc::new(MatchAllQuery);
        for round in 0..60 {
            let (field, kind) = [(NUMERIC, SortType::Long), (SNUM, SortType::Long), (SORTED, SortType::Keyword), (SSET, SortType::Keyword)][rng.gen_range(0..4)];
            let reverse = rng.gen_bool(0.5);
            // A custom value between or beside the indexed ones (odd: never indexed as a term).
            let custom = rng.gen_range(-1..33i64);
            let missing = match rng.gen_range(0..3) {
                0 => Missing::First,
                1 => Missing::Last,
                _ if kind == SortType::Keyword => Missing::Value(SortValue::Bytes(format!("k{custom:02}x").into_bytes())),
                _ => Missing::Value(SortValue::Long(custom)),
            };
            let first = matches!(missing, Missing::First);
            // Keys as comparable numbers: a custom keyword `k<c>x` sorts just after `k<c>`.
            let key = |v: Option<i64>| -> Option<(i64, u8)> {
                match (v, &missing) {
                    (Some(v), _) => Some((v, 0)),
                    (None, Missing::Value(_)) => Some((custom, u8::from(kind == SortType::Keyword))),
                    _ => None,
                }
            };
            let mut exp: Vec<(u32, Option<(i64, u8)>)> = ids.iter().map(|(g, id)| (*g, key(model[id[1..].parse::<usize>().unwrap()]))).collect();
            exp.sort_by(|a, b| {
                let o = match (a.1, b.1) {
                    (None, None) => Ordering::Equal,
                    (None, _) => if first { Ordering::Less } else { Ordering::Greater },
                    (_, None) => if first { Ordering::Greater } else { Ordering::Less },
                    (Some(x), Some(y)) => if reverse { y.cmp(&x) } else { x.cmp(&y) },
                };
                o.then(a.0.cmp(&b.0))
            });
            let k = [1usize, 2, 7, 30][rng.gen_range(0..4)];
            let want: Vec<u32> = exp.iter().take(k).map(|e| e.0).collect();
            for threshold in [0u64, u64::MAX] {
                let keys = vec![SortKey::Field(FieldSort { field, kind, reverse, missing: missing.clone(), mode: SortMode::Min, nested: None })];
                let mut c = TopFieldCollector::new(keys.clone(), k, threshold, false).with_searcher(&s);
                s.search(&q, &mut c).unwrap();
                let got: Vec<u32> = c.top_docs().hits.iter().map(|h| h.doc).collect();
                assert_eq!(got, want, "seed {seed} round {round}: k={k} threshold={threshold} keys={keys:?}");
            }
        }
    }
}

/// Sorts against the docs' order (descending over values that grow with doc ids, the log case,
/// and ascending over falling ones), where the collector takes the first key's values in bands
/// and runs a segment again for the next band when the first came up short: exact hits, sort
/// values and tie order by doc id, for longs, ints, floats and doubles, ties, missing values
/// (`_last` and `_first`), filters of every density, `search_after`, a second key, deletes,
/// several segments, and totals exact up to the threshold.
#[test]
fn sorts_against_doc_order_equal_exhaustive_sorts() {
    use search_index::query::{DocValuesRangeQuery, DvRange, IndexOrDocValuesQuery, PointRangeQuery};
    for seed in 0..4u64 {
        let mut rng = common::rng(seed * 13 + 5);
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
        let n = 30_000u32;
        let falling = seed % 2 == 1;
        for i in 0..n {
            let mut d = IndexableDoc::new(format!("d{i}").into_bytes());
            // Every 11th doc has no value; values tie in threes.
            if i % 11 != 5 {
                let t = (i / 3) as i64;
                let v = if falling { 1_000_000 - t } else { t };
                let iv = v as i32;
                let fv = v as f32 / 4.0;
                let dv = v as f64 / 3.0;
                d = d
                    .point(L, encode::i64_bytes(v).to_vec())
                    .doc_value(L, DocValue::SortedNumeric(v))
                    .point(I, encode::i32_bytes(iv).to_vec())
                    .doc_value(I, DocValue::SortedNumeric(iv as i64))
                    .point(F, encode::f32_bytes(fv).to_vec())
                    .doc_value(F, DocValue::SortedNumeric(encode::f32_sortable(fv) as i64))
                    .point(D, encode::f64_bytes(dv).to_vec())
                    .doc_value(D, DocValue::SortedNumeric(encode::f64_sortable(dv)));
            }
            // Words of every density: every doc, one in 10, one in 1000, docs before a third.
            let mut words = vec!["all".to_string(), format!("m{}", i % 10), format!("r{}", i % 1000)];
            if i < n / 3 {
                words.push("early".into());
            }
            let tokens: Vec<PreToken> = words.iter().enumerate().map(|(p, w)| PreToken::new(w.clone().into_bytes(), 1, p as u32 * 8, p as u32 * 8 + 4)).collect();
            shard.index(IndexOp::new(d.field(search_index::doc::IndexableField::Tokens { field: W, tokens }))).unwrap();
            if seed >= 2 && rng.gen_bool(0.0002) {
                shard.refresh().unwrap();
            }
            if seed == 3 && rng.gen_bool(0.01) {
                shard.delete(DeleteOp::new(format!("d{}", rng.gen_range(0..=i)).into_bytes())).unwrap();
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let queries: Vec<(&str, QueryRef)> = vec![
            ("all", Arc::new(MatchAllQuery)),
            ("m3", Arc::new(TermQuery::new(W, "m3"))),
            ("r7", Arc::new(TermQuery::new(W, "r7"))),
            ("early", Arc::new(TermQuery::new(W, "early"))),
            ("filtered", Arc::new(BooleanQuery::new().must(Arc::new(TermQuery::new(W, "all"))).filter(Arc::new(TermQuery::new(W, "m7"))))),
        ];
        for (field, kind) in [(L, SortType::Long), (I, SortType::Int), (F, SortType::Float), (D, SortType::Double)] {
            // A range on the sort field over the middle values, as Elasticsearch's range query
            // builds it (the band starts inside it).
            let (a, b) = if falling { (1_000_000 - 6000, 1_000_000 - 4000) } else { (4000i64, 6000) };
            let (point, dv): (QueryRef, DvRange) = match kind {
                SortType::Long => (Arc::new(PointRangeQuery::long(field, a, b)), DvRange::Long { lower: a, upper: b }),
                SortType::Int => (Arc::new(PointRangeQuery::int(field, a as i32, b as i32)), DvRange::Long { lower: a, upper: b }),
                SortType::Float => {
                    let (x, y) = (a as f32 / 4.0, b as f32 / 4.0);
                    (Arc::new(PointRangeQuery::float(field, x, y)), DvRange::Long { lower: encode::f32_sortable(x) as i64, upper: encode::f32_sortable(y) as i64 })
                }
                _ => {
                    let (x, y) = (a as f64 / 3.0, b as f64 / 3.0);
                    (Arc::new(PointRangeQuery::double(field, x, y)), DvRange::Long { lower: encode::f64_sortable(x), upper: encode::f64_sortable(y) })
                }
            };
            let range: QueryRef = Arc::new(IndexOrDocValuesQuery { index: point, dv: Arc::new(DocValuesRangeQuery { field, range: dv }) });
            let mut queries = queries.clone();
            queries.push(("range", range.clone()));
            queries.push(("m3 in range", Arc::new(BooleanQuery::new().must(Arc::new(TermQuery::new(W, "m3"))).filter(range))));
            for (qn, q) in &queries {
                // Against the doc order: descending over rising values, ascending over falling.
                let reverse = !falling;
                let missing = if rng.gen_bool(0.25) { Missing::First } else { Missing::Last };
                let mut keys = vec![SortKey::Field(FieldSort { field, kind, reverse, missing, mode: SortMode::Min, nested: None })];
                if rng.gen_bool(0.3) {
                    keys.push(SortKey::Doc { reverse: true });
                }
                let k = [1usize, 10, 40][rng.gen_range(0..3)];
                let threshold = [1u64, 100, 1000, 10_000, u64::MAX][rng.gen_range(0..5)];
                let what = format!("seed {seed} field {field} query {qn} k={k} threshold={threshold} keys={keys:?}");
                let full = {
                    let mut c = TopFieldCollector::new(keys.clone(), k, u64::MAX, false).with_searcher(&s);
                    one_pass(&s, q, &mut c);
                    c.top_docs()
                };
                let mut c = TopFieldCollector::new(keys.clone(), k, threshold, false).with_searcher(&s).with_query(q);
                s.search(q, &mut c).unwrap();
                let got = c.top_docs();
                assert_eq!(hits(&got), hits(&full), "{what}");
                if got.relation == TotalRelation::EqualTo {
                    assert_eq!(got.total_hits, full.total_hits, "{what}");
                } else {
                    assert!(full.total_hits > threshold && got.total_hits > threshold && got.total_hits <= full.total_hits, "{what}");
                }
                // search_after a hit of the exhaustive order continues it.
                let all = {
                    let mut c = TopFieldCollector::new(keys.clone(), 300, u64::MAX, false).with_searcher(&s);
                    one_pass(&s, q, &mut c);
                    c.top_docs()
                };
                for at in [0usize, 7, 250] {
                    let Some(after) = all.hits.get(at) else { continue };
                    let mut c = TopFieldCollector::new(keys.clone(), k, threshold, false).with_searcher(&s).with_query(q).with_after(after.values.clone(), Some(after.doc));
                    s.search(q, &mut c).unwrap();
                    let want: Vec<(u32, Vec<SortValue>)> = all.hits.iter().skip(at + 1).take(k).map(|h| (h.doc, h.values.clone())).collect();
                    assert_eq!(hits(&c.top_docs())[..want.len().min(k)], want[..], "after #{at}: {what}");
                }
            }
        }
    }
}

/// A range on a multi-valued sort field: a hit may match through one value and sort by another
/// outside the range, so the competitive docs aren't bounded by the range. Values rise with doc
/// ids inside the range (a descending sort bands), and every 1000th doc also has a value above it
/// (sorted by the max, those are the top hits, whatever their value in the range).
#[test]
fn ranges_on_multi_valued_sort_fields_equal_exhaustive_sorts() {
    use search_index::query::{DocValuesRangeQuery, DvRange, IndexOrDocValuesQuery, PointRangeQuery};
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(None), Vec::new()).unwrap();
    for i in 0..30_000i64 {
        let mut values = vec![10_000 + i / 3];
        if i % 1000 == 999 {
            values.push(500_000 + i);
        }
        let mut d = IndexableDoc::new(format!("d{i}").into_bytes());
        for v in values {
            d = d.point(L, encode::i64_bytes(v).to_vec()).doc_value(L, DocValue::SortedNumeric(v));
        }
        shard.index(IndexOp::new(d)).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let range: QueryRef = Arc::new(IndexOrDocValuesQuery {
        index: Arc::new(PointRangeQuery::long(L, 10_000, 20_000)),
        dv: Arc::new(DocValuesRangeQuery { field: L, range: DvRange::Long { lower: 10_000, upper: 20_000 } }),
    });
    for (reverse, mode) in [(true, SortMode::Max), (true, SortMode::Min), (false, SortMode::Min), (false, SortMode::Max)] {
        let keys = vec![SortKey::Field(FieldSort { field: L, kind: SortType::Long, reverse, missing: Missing::Last, mode, nested: None })];
        for threshold in [10u64, 1000, u64::MAX] {
            let mut full = TopFieldCollector::new(keys.clone(), 10, u64::MAX, false).with_searcher(&s);
            one_pass(&s, &range, &mut full);
            let mut c = TopFieldCollector::new(keys.clone(), 10, threshold, false).with_searcher(&s).with_query(&range);
            s.search(&range, &mut c).unwrap();
            assert_eq!(hits(&c.top_docs()), hits(&full.top_docs()), "reverse {reverse} mode {mode:?} threshold {threshold}");
        }
    }
}

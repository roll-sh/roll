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
                s.search(q, &mut c).unwrap();
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
                    s.search(q, &mut c).unwrap();
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

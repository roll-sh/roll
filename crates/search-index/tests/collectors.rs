//! Collector composition, concurrent segment search and collapsing against sequential runs
//! and brute-force models, on random multi-segment corpora.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::{BODY, NUM, TAG, build, word};
use rand::Rng;
use search_codec::{DocValuesType, IndexOptions};
use search_index::collector::{
    CollapseCollector, CollapseField, CountManager, FieldSort, Missing, MultiCollector, SortKey, SortMode, SortType, SortValue,
    TerminateAfter, TopFieldCollector, TopFieldManager, TopScoreCollector, TopScoreManager, TotalHitCountCollector,
};
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken};
use search_index::query::{BooleanQuery, MatchAllQuery, PhraseQuery, TermQuery};
use search_index::schema::FieldSchema;
use search_index::search::{SearchControl, SearchLimits};
use search_index::{Collector, IndexOp, QueryRef, Schema, Shard, ShardConfig};

fn queries() -> Vec<QueryRef> {
    vec![
        Arc::new(TermQuery::new(BODY, word(0).into_bytes())),
        Arc::new(TermQuery::new(BODY, word(7).into_bytes())),
        Arc::new(
            BooleanQuery::new()
                .should(Arc::new(TermQuery::new(BODY, word(1).into_bytes())))
                .should(Arc::new(TermQuery::new(BODY, word(3).into_bytes())))
                .should(Arc::new(TermQuery::new(BODY, word(20).into_bytes()))),
        ),
        Arc::new(
            BooleanQuery::new()
                .must(Arc::new(TermQuery::new(BODY, word(0).into_bytes())))
                .should(Arc::new(TermQuery::new(BODY, word(2).into_bytes())))
                .filter(Arc::new(TermQuery::new(TAG, b"t1".to_vec()))),
        ),
        Arc::new(MatchAllQuery),
        // Pure conjunctions: bounded batches when every match is visited.
        Arc::new(
            BooleanQuery::new()
                .must(Arc::new(TermQuery::new(BODY, word(0).into_bytes())))
                .must(Arc::new(TermQuery::new(BODY, word(1).into_bytes())))
                .filter(Arc::new(TermQuery::new(TAG, b"t1".to_vec())))
                .must_not(Arc::new(TermQuery::new(BODY, word(4).into_bytes()))),
        ),
        Arc::new(
            BooleanQuery::new()
                .filter(Arc::new(TermQuery::new(BODY, word(2).into_bytes())))
                .filter(Arc::new(TermQuery::new(BODY, word(3).into_bytes()))),
        ),
        Arc::new(
            BooleanQuery::new()
                .must(Arc::new(TermQuery::new(BODY, word(0).into_bytes())))
                .must(Arc::new(PhraseQuery::new(BODY, vec![word(1).into_bytes(), word(0).into_bytes()], 1))),
        ),
        // Two-phase: windows and partitions confirm only their own candidates.
        Arc::new(PhraseQuery::new(BODY, vec![word(0).into_bytes(), word(1).into_bytes()], 0)),
        Arc::new(PhraseQuery::new(BODY, vec![word(2).into_bytes(), word(0).into_bytes(), word(1).into_bytes()], 2)),
        Arc::new(
            BooleanQuery::new()
                .should(Arc::new(PhraseQuery::new(BODY, vec![word(0).into_bytes(), word(0).into_bytes()], 0)))
                .filter(Arc::new(TermQuery::new(TAG, b"t2".to_vec())))
                .must_not(Arc::new(PhraseQuery::new(BODY, vec![word(1).into_bytes(), word(2).into_bytes()], 1))),
        ),
    ]
}

/// Searches with a deadline score in growing windows (to check the clock between them); the
/// results equal unwindowed ones, two-phase queries included.
#[test]
fn windowed_scoring_equals_plain() {
    for seed in 0..3u64 {
        let corpus = build(seed + 50, 1500, ShardConfig { merge_threads: 0, ..Default::default() });
        let s = corpus.shard.searcher();
        let ctl = SearchControl { deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(3600)), cancelled: None };
        for (qi, q) in queries().iter().enumerate() {
            for (n, threshold) in [(10usize, u64::MAX), (3, 5), (100, u64::MAX)] {
                let mut a = TopScoreCollector::new(n, threshold);
                s.search(q, &mut a).unwrap();
                let mut b = TopScoreCollector::new(n, threshold);
                s.search_with(q, &mut b, &ctl).unwrap();
                let (a, b) = (a.top_docs(), b.top_docs());
                assert_eq!(a.hits, b.hits, "seed {seed} q{qi} n {n}");
                if threshold == u64::MAX {
                    assert_eq!(a.total_hits, b.total_hits, "seed {seed} q{qi}");
                }
            }
            let mut c = TotalHitCountCollector::default();
            s.search_with(q, &mut c, &ctl).unwrap();
            assert_eq!(c.total, s.count(q).unwrap(), "seed {seed} q{qi}");
        }
    }
}

fn num_sort(reverse: bool) -> SortKey {
    SortKey::Field(FieldSort {
        field: NUM,
        kind: SortType::Long,
        reverse,
        missing: Missing::Last,
        mode: if reverse { SortMode::Max } else { SortMode::Min },
        nested: None,
    })
}

#[test]
fn concurrent_search_equals_sequential() {
    for seed in 0..std::env::var("SWEEP_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6u64) {
        let corpus = build(seed, 1500, ShardConfig { merge_threads: 0, ..Default::default() });
        if seed % 3 == 2 {
            // One segment: the slices are partitions of it.
            corpus.shard.force_merge(Some(1), false).unwrap();
        }
        let s = corpus.shard.searcher();
        let par = s.with_limits(SearchLimits { parallel_min_docs: 64, parallelism: 4, ..SearchLimits::default() });
        let slices = par.slices();
        assert!(slices.len() > 1, "seed {seed}: {slices:?}");
        // Every doc of every segment is in exactly one partition, and no slice holds two
        // partitions of one segment.
        for seg in s.segments() {
            let mut ranges: Vec<(u32, u32)> =
                slices.iter().flatten().filter(|p| p.seg.ord == seg.ord).map(|p| (p.min, p.max.min(seg.max_doc()))).collect();
            ranges.sort();
            assert_eq!(ranges.first().map(|r| r.0), Some(0));
            assert!(ranges.windows(2).all(|w| w[0].1 == w[1].0), "seed {seed}: {ranges:?}");
            assert_eq!(ranges.last().map(|r| r.1), Some(seg.max_doc()));
        }
        for sl in &slices {
            let mut ords: Vec<u32> = sl.iter().map(|p| p.seg.ord).collect();
            ords.dedup();
            assert_eq!(ords.len(), sl.len(), "seed {seed}: {sl:?}");
        }
        let ctl = SearchControl::default();
        for (qi, q) in queries().iter().enumerate() {
            for (n, threshold) in [(10usize, u64::MAX), (5, 20), (0, u64::MAX), (50, 100)] {
                let mut c = TopScoreCollector::new(n, threshold);
                s.search(q, &mut c).unwrap();
                let seq = c.top_docs();
                let m = TopScoreManager { n, threshold, after: None, min_score: None };
                let got = par.search_concurrent(q, &m, &ctl).unwrap();
                assert_eq!(got.hits, seq.hits, "seed {seed} q{qi} n {n} threshold {threshold}");
                if threshold == u64::MAX {
                    assert_eq!(got.total_hits, seq.total_hits);
                } else {
                    assert!(got.total_hits >= threshold.min(seq.total_hits));
                }
            }
            for reverse in [false, true] {
                let keys = vec![num_sort(reverse), SortKey::Score { reverse: false }];
                let mut c = TopFieldCollector::new(keys.clone(), 15, u64::MAX, true).with_searcher(&s);
                s.search(q, &mut c).unwrap();
                let seq = c.top_docs();
                let m = TopFieldManager { keys, n: 15, threshold: u64::MAX, track_scores: true, after: None, searcher: par.clone() };
                let got = par.search_concurrent(q, &m, &ctl).unwrap();
                assert_eq!(got.hits, seq.hits, "seed {seed} q{qi} sorted reverse {reverse}");
                assert_eq!(got.total_hits, seq.total_hits);
                assert_eq!(got.max_score, seq.max_score);
            }
            let count = s.count(q).unwrap();
            assert_eq!(par.search_concurrent(q, &CountManager, &ctl).unwrap(), count);
            assert_eq!(par.count(q).unwrap(), count, "seed {seed} q{qi} concurrent count");
        }
    }
}

#[test]
fn multi_collector_matches_separate_runs() {
    let corpus = build(42, 1200, ShardConfig { merge_threads: 0, ..Default::default() });
    let s = corpus.shard.searcher();
    for q in queries() {
        let mut a = TopScoreCollector::new(10, 50);
        let mut b = TotalHitCountCollector::default();
        let mut f = TopFieldCollector::new(vec![num_sort(false)], 7, u64::MAX, false).with_searcher(&s);
        {
            let mut m = MultiCollector::new(vec![&mut a as &mut dyn Collector, &mut b, &mut f]);
            s.search(&q, &mut m).unwrap();
        }
        let mut a1 = TopScoreCollector::new(10, 50);
        s.search(&q, &mut a1).unwrap();
        let mut f1 = TopFieldCollector::new(vec![num_sort(false)], 7, u64::MAX, false).with_searcher(&s);
        s.search(&q, &mut f1).unwrap();
        assert_eq!(a.top_docs().hits, a1.top_docs().hits);
        assert_eq!(b.total, s.count(&q).unwrap());
        let dv = |h: Vec<search_index::collector::FieldDoc>| h.into_iter().map(|h| (h.doc, h.values)).collect::<Vec<_>>();
        assert_eq!(dv(f.top_docs().hits), dv(f1.top_docs().hits));
        // Two top-score collectors: pruning uses the lower bar, both stay exact.
        let mut x = TopScoreCollector::new(3, 0);
        let mut y = TopScoreCollector::new(12, 0);
        {
            let mut m = MultiCollector::new(vec![&mut x as &mut dyn Collector, &mut y]);
            s.search(&q, &mut m).unwrap();
        }
        let mut x1 = TopScoreCollector::new(3, u64::MAX);
        s.search(&q, &mut x1).unwrap();
        let mut y1 = TopScoreCollector::new(12, u64::MAX);
        s.search(&q, &mut y1).unwrap();
        assert_eq!(x.top_docs().hits, x1.top_docs().hits);
        assert_eq!(y.top_docs().hits, y1.top_docs().hits);
    }
}

#[test]
fn terminate_after_stops_at_the_limit() {
    let corpus = build(7, 900, ShardConfig { merge_threads: 0, ..Default::default() });
    let s = corpus.shard.searcher();
    let q: QueryRef = Arc::new(MatchAllQuery);
    let total = s.count(&q).unwrap();
    for max in [0u64, 1, 5, 64, total, total + 10] {
        let mut c = TerminateAfter::new(TotalHitCountCollector::default(), max);
        s.search(&q, &mut c).unwrap();
        assert_eq!(c.count(), max.min(total));
        assert_eq!(c.inner.total, max.min(total));
        assert_eq!(c.terminated_early, max < total, "max {max} total {total}");
    }
}

const GRP: u32 = 30;

/// Per `_id`: words, group and number.
type Model = HashMap<String, (Vec<String>, Option<String>, i64)>;

fn collapse_shard(seed: u64) -> (Shard, Model) {
    let schema = Schema::new()
        .with_field(FieldSchema::new(BODY, "body").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap()
        .with_field(FieldSchema::new(GRP, "grp").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::Sorted))
        .unwrap()
        .with_field(FieldSchema::new(NUM, "num").with_doc_values(DocValuesType::Numeric))
        .unwrap();
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    let mut rng = common::rng(seed);
    let mut model = HashMap::new();
    for i in 0..700 {
        let id = format!("c{}", rng.gen_range(0..500));
        let words: Vec<String> = (0..rng.gen_range(1..12)).map(|_| word(common::zipf(&mut rng, 15))).collect();
        let grp = rng.gen_bool(0.9).then(|| format!("g{}", rng.gen_range(0..25)));
        let num = rng.gen_range(-100..100);
        let mut d = IndexableDoc::new(id.clone().into_bytes());
        let mut off = 0u32;
        let toks = words
            .iter()
            .map(|w| {
                let t = PreToken::new(w.clone().into_bytes(), 1, off, off + w.len() as u32);
                off += w.len() as u32 + 1;
                t
            })
            .collect();
        d.fields.push(IndexableField::Tokens { field: BODY, tokens: toks });
        if let Some(g) = &grp {
            d.fields.push(IndexableField::DocValue { field: GRP, value: DocValue::Sorted(g.clone().into_bytes().into()) });
        }
        d.fields.push(IndexableField::DocValue { field: NUM, value: DocValue::Numeric(num) });
        shard.index(IndexOp::new(d)).unwrap();
        model.insert(id, (words, grp, num));
        if i % 97 == 0 {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    (shard, model)
}

#[test]
fn collapse_matches_brute_force() {
    for seed in 0..std::env::var("SWEEP_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(4u64) {
        let (shard, model) = collapse_shard(seed);
        let s = shard.searcher();
        let ids = common::ids(&s);
        for w in [0usize, 1, 4] {
            let q: QueryRef = Arc::new(TermQuery::new(BODY, word(w).into_bytes()));
            let mut all = TopScoreCollector::new(s.num_docs() as usize, u64::MAX);
            s.search(&q, &mut all).unwrap();
            let all = all.top_docs();
            for by_num in [false, true] {
                for n in [1usize, 5, 30] {
                    let keys = if by_num { vec![num_sort(false)] } else { Vec::new() };
                    let mut c = CollapseCollector::new(CollapseField::Keyword(GRP), keys, n, u64::MAX, false);
                    s.search(&q, &mut c).unwrap();
                    let got = c.top_docs();
                    assert_eq!(got.total_hits, all.total_hits);
                    // Model: the best doc per group, then the best n groups.
                    let num = |d: u32| model[&ids[&d]].2;
                    let better = |a: (u32, f32), b: (u32, f32)| -> bool {
                        if by_num {
                            let (x, y) = (num(a.0), num(b.0));
                            x < y || (x == y && a.0 < b.0)
                        } else {
                            a.1 > b.1 || (a.1 == b.1 && a.0 < b.0)
                        }
                    };
                    let mut best: HashMap<Option<String>, (u32, f32)> = HashMap::new();
                    for h in &all.hits {
                        let g = model[&ids[&h.doc]].1.clone();
                        let cand = (h.doc, h.score);
                        match best.get(&g) {
                            Some(&cur) if !better(cand, cur) => {}
                            _ => {
                                best.insert(g, cand);
                            }
                        }
                    }
                    let mut heads: Vec<(Option<String>, (u32, f32))> = best.into_iter().collect();
                    heads.sort_by(|a, b| if better(a.1, b.1) { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater });
                    heads.truncate(n);
                    let want: Vec<(u32, SortValue)> = heads
                        .into_iter()
                        .map(|(g, (d, _))| (d, g.map_or(SortValue::Null, |g| SortValue::Bytes(g.into_bytes()))))
                        .collect();
                    let have: Vec<(u32, SortValue)> = got.hits.iter().map(|h| (h.doc, h.group.clone())).collect();
                    assert_eq!(have, want, "seed {seed} word {w} by_num {by_num} n {n}");
                    if by_num {
                        for h in &got.hits {
                            assert_eq!(h.values, vec![SortValue::Long(num(h.doc))]);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn filter_cache_reuses_doc_sets_and_stays_correct() {
    use search_index::DeleteOp;
    use search_index::query::{PointRangeQuery, TermQuery};
    let mut rng = common::rng(77);
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, common::schema(), Vec::new()).unwrap();
    let mut ids = Vec::new();
    for i in 0..30_000 {
        let d = common::random_doc(&mut rng, i);
        ids.push(d.id.clone());
        s.index(IndexOp::new(common::to_indexable(&d))).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let q = |lo: i64, hi: i64| -> QueryRef {
        Arc::new(
            BooleanQuery::new()
                .must(Arc::new(TermQuery::new(BODY, word(1).into_bytes())))
                .filter(Arc::new(PointRangeQuery::long(NUM, lo, hi)))
                .must_not(Arc::new(search_index::query::MultiTermQuery::prefix(TAG, b"t7".to_vec()))),
        )
    };
    let run = |se: &search_index::Searcher, q: &QueryRef| {
        let mut c = TopScoreCollector::new(50, u64::MAX);
        se.search(q, &mut c).unwrap();
        let td = c.top_docs();
        (td.total_hits, td.hits)
    };
    let before = s.stats().segment_heap_bytes;
    let se = s.searcher();
    let plain = se.with_query_cache(None);
    for round in 0..4 {
        for (lo, hi) in [(-20, 10), (0, 49), (-50, -10)] {
            let a = run(&se, &q(lo, hi));
            let b = run(&plain, &q(lo, hi));
            assert_eq!(a, b, "round {round} range {lo}..{hi}");
            assert_eq!(se.count(&q(lo, hi)).unwrap(), plain.count(&q(lo, hi)).unwrap());
        }
    }
    assert!(s.stats().segment_heap_bytes > before, "nothing was cached");
    // Deletions after caching are applied on top of the cached sets.
    for id in ids.iter().step_by(3) {
        s.delete(DeleteOp::new(id.clone().into_bytes())).unwrap();
    }
    s.refresh().unwrap();
    let se = s.searcher();
    let plain = se.with_query_cache(None);
    for (lo, hi) in [(-20, 10), (0, 49), (-50, -10)] {
        assert_eq!(run(&se, &q(lo, hi)), run(&plain, &q(lo, hi)));
        assert_eq!(se.count(&q(lo, hi)).unwrap(), plain.count(&q(lo, hi)).unwrap());
    }
}

/// Compound filters (bool over a phrase, dis_max) are cached after recurring like Lucene's
/// policy says (four uses for compounds), and cached results equal uncached ones, with
/// scoring clauses next to them and deletions applied afterwards.
#[test]
fn filter_cache_handles_compound_filters() {
    use search_index::DeleteOp;
    use search_index::query::{DisMaxQuery, PhraseQuery, PointRangeQuery};
    let mut rng = common::rng(91);
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, common::schema(), Vec::new()).unwrap();
    let mut ids = Vec::new();
    for i in 0..30_000 {
        let d = common::random_doc(&mut rng, i);
        ids.push(d.id.clone());
        s.index(IndexOp::new(common::to_indexable(&d))).unwrap();
    }
    s.refresh().unwrap();
    s.force_merge(Some(1), false).unwrap();
    let filter = |k: usize| -> QueryRef {
        let ph: QueryRef = Arc::new(PhraseQuery::new(BODY, vec![word(k).into_bytes(), word(k + 1).into_bytes()], 1));
        let dm: QueryRef = Arc::new(DisMaxQuery::new(
            vec![Arc::new(TermQuery::new(BODY, word(k + 2).into_bytes())), Arc::new(PointRangeQuery::long(NUM, -10, 10))],
            0.0,
        ));
        Arc::new(BooleanQuery::new().should(ph).should(dm))
    };
    let q = |k: usize| -> QueryRef { Arc::new(BooleanQuery::new().should(Arc::new(TermQuery::new(BODY, word(3).into_bytes()))).filter(filter(k))) };
    let run = |se: &search_index::Searcher, q: &QueryRef| {
        let mut c = TopScoreCollector::new(30, u64::MAX);
        se.search(q, &mut c).unwrap();
        let td = c.top_docs();
        (td.total_hits, td.hits)
    };
    let before = s.stats().segment_heap_bytes;
    let se = s.searcher();
    let plain = se.with_query_cache(None);
    for round in 0..3 {
        for k in [0, 1, 2] {
            assert_eq!(run(&se, &q(k)), run(&plain, &q(k)), "round {round} k {k}");
        }
    }
    // Three uses each: below the compound threshold.
    assert_eq!(s.stats().segment_heap_bytes, before, "cached too early");
    for k in [0, 1, 2] {
        assert_eq!(run(&se, &q(k)), run(&plain, &q(k)));
        assert_eq!(se.count(&q(k)).unwrap(), plain.count(&q(k)).unwrap());
    }
    assert!(s.stats().segment_heap_bytes > before, "nothing was cached");
    for id in ids.iter().step_by(4) {
        s.delete(DeleteOp::new(id.clone().into_bytes())).unwrap();
    }
    s.refresh().unwrap();
    let se = s.searcher();
    let plain = se.with_query_cache(None);
    for k in [0, 1, 2] {
        assert_eq!(run(&se, &q(k)), run(&plain, &q(k)));
        assert_eq!(se.count(&q(k)).unwrap(), plain.count(&q(k)).unwrap());
    }
}

/// `post_filter`: top hits of `q AND filter` (scores of `q` alone) while a counter beside it
/// sees every match of `q`.
#[test]
fn post_filter_limits_only_the_wrapped_collector() {
    use search_index::collector::PostFilter;
    use search_index::query::PhraseQuery;
    let corpus = build(11, 1500, ShardConfig { merge_threads: 0, ..Default::default() });
    let s = corpus.shard.searcher();
    let filters: Vec<QueryRef> = vec![
        Arc::new(TermQuery::new(TAG, "t1")),
        Arc::new(PhraseQuery::new(BODY, vec![word(1).into_bytes(), word(2).into_bytes()], 1)),
        Arc::new(BooleanQuery::new().should(Arc::new(TermQuery::new(TAG, "t0"))).should(Arc::new(TermQuery::new(TAG, "t2")))),
    ];
    for q in queries() {
        for f in &filters {
            let mut count = TotalHitCountCollector::default();
            let mut top = PostFilter::new(&s, f, TopScoreCollector::new(15, u64::MAX)).unwrap();
            {
                let mut m = MultiCollector::new(vec![&mut count as &mut dyn Collector, &mut top]);
                s.search(&q, &mut m).unwrap();
            }
            assert_eq!(count.total, s.count(&q).unwrap());
            // Expected: hits of q whose docs match the filter, with q's scores.
            let mut all = TopScoreCollector::new(s.num_docs() as usize + 1, u64::MAX);
            s.search(&q, &mut all).unwrap();
            let both: QueryRef = Arc::new(BooleanQuery::new().must(q.clone()).filter(f.clone()));
            let mut fb = TopScoreCollector::new(s.num_docs() as usize + 1, u64::MAX);
            s.search(&both, &mut fb).unwrap();
            let keep: std::collections::HashSet<u32> = fb.top_docs().hits.iter().map(|h| h.doc).collect();
            let want: Vec<(u32, f32)> = all.top_docs().hits.into_iter().filter(|h| keep.contains(&h.doc)).take(15).map(|h| (h.doc, h.score)).collect();
            let td = top.inner.top_docs();
            assert_eq!(td.total_hits as usize, keep.len());
            assert_eq!(td.hits.iter().map(|h| (h.doc, h.score)).collect::<Vec<_>>(), want);
        }
    }
}

/// Profiling: the query tree of the rewritten query with Lucene's class names, timings that
/// add up, and unchanged results.
#[test]
fn profiled_searches_report_the_query_tree() {
    use search_index::query::{ConstantScoreQuery, DisMaxQuery, MultiTermQuery};
    use search_index::search::profile::{ProfileCollector, Profiler};
    let corpus = build(5, 600, ShardConfig { merge_threads: 0, ..Default::default() });
    let s = corpus.shard.searcher();
    let q: QueryRef = Arc::new(
        BooleanQuery::new()
            .must(Arc::new(TermQuery::new(BODY, word(1))))
            .should(Arc::new(DisMaxQuery::new(vec![Arc::new(TermQuery::new(BODY, word(2))), Arc::new(MultiTermQuery::prefix(BODY, b"w1".to_vec()))], 0.1)))
            .filter(Arc::new(ConstantScoreQuery::new(Arc::new(TermQuery::new(TAG, "t1"))))),
    );
    let p = Profiler::new();
    let ps = s.with_profiler(Some(p.clone()));
    let mut c = ProfileCollector::new(TopScoreCollector::new(10, u64::MAX));
    ps.search(&q, &mut c).unwrap();
    let mut plain = TopScoreCollector::new(10, u64::MAX);
    s.search(&q, &mut plain).unwrap();
    assert!(c.time_in_nanos() > 0);
    assert_eq!(c.inner.top_docs().hits, plain.top_docs().hits, "profiling changes no result");
    let tree = p.tree();
    assert_eq!(tree.len(), 1);
    let root = &tree[0];
    assert_eq!(root.kind, "BooleanQuery");
    // The tree describes the rewritten query: the filter's constant score is gone.
    assert_eq!(root.description, s.rewrite(&q).unwrap().lucene_string(s.schema()));
    assert_eq!(root.description, "+body:w1 (body:w2 | body:w1*)~0.1 #tag:t1");
    let kinds: Vec<&str> = root.children.iter().map(|n| n.kind.as_str()).collect();
    assert_eq!(kinds, ["TermQuery", "DisjunctionMaxQuery", "TermQuery"]);
    let dm = &root.children[1];
    assert_eq!(dm.children.iter().map(|n| n.kind.as_str()).collect::<Vec<_>>(), ["TermQuery", "MultiTermQueryConstantScoreBlendedWrapper"]);
    let get = |n: &search_index::search::profile::ProfileNode, k: &str| *n.breakdown.iter().find(|b| b.0 == k).unwrap();
    assert_eq!(get(root, "create_weight").2, 1);
    assert!(get(root, "build_scorer").2 >= 1);
    assert!(get(root, "next_doc").2 + get(root, "advance").2 > 0);
    assert_eq!(root.time_in_nanos, root.breakdown.iter().map(|b| b.1).sum::<u64>());
    assert_eq!(root.breakdown.len(), 10);
    // An unprofiled view of the same searcher records nothing more.
    s.search(&q, &mut TopScoreCollector::new(10, u64::MAX)).unwrap();
    assert_eq!(p.tree().len(), 1);
}

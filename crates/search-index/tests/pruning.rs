//! Dynamic pruning on corpora large enough for multi-block postings: top-k with a low
//! total-hits threshold (block-max term skipping, batch scoring, score-first block-max
//! conjunctions, MaxScore with required non-essential clauses, WAND) must return exactly the
//! exhaustive top-k — same docs, same scores, same order — with and without deletions.

mod common;

use std::sync::Arc;

use common::{BODY, TAG, TITLE, schema, to_indexable};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use search_index::collector::{TopScoreCollector, TotalRelation};
use search_index::query::{BooleanQuery, MultiPhraseQuery, PhraseQuery, TermQuery};
use search_index::{DeleteOp, IndexOp, QueryRef, Shard, ShardConfig};

fn word(i: usize) -> Vec<u8> {
    format!("p{i}").into_bytes()
}

/// Zipf(1)-like rank over `n` words.
fn zipf(rng: &mut StdRng, n: usize) -> usize {
    let u: f64 = rng.r#gen();
    (((n as f64).powf(u)) as usize).saturating_sub(1).min(n - 1)
}

fn corpus(seed: u64, n: usize, deletes: bool) -> Shard {
    corpus_in(seed, n, deletes, 3)
}

/// `n` docs refreshed into about `parts` segments (merges may fold some).
fn corpus_in(seed: u64, n: usize, deletes: bool, parts: usize) -> Shard {
    let mut rng = StdRng::seed_from_u64(seed);
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for i in 0..n {
        let len = rng.gen_range(1..60);
        let body = (0..len).map(|_| String::from_utf8(word(zipf(&mut rng, 400))).unwrap()).collect();
        let title = (0..rng.gen_range(0..4)).map(|_| String::from_utf8(word(zipf(&mut rng, 30))).unwrap()).collect();
        let tags = (0..rng.gen_range(0..3)).map(|_| format!("t{}", rng.gen_range(0..4))).collect();
        let d = common::MDoc { id: format!("d{i}"), body, title, tags, nums: Vec::new() };
        s.index(IndexOp::new(to_indexable(&d))).unwrap();
        if i % (n / parts) == n / parts - 1 {
            s.refresh().unwrap();
        }
    }
    if deletes {
        for _ in 0..n / 8 {
            let k = rng.gen_range(0..n);
            s.delete(DeleteOp::new(format!("d{k}").into_bytes())).unwrap();
        }
    }
    s.refresh().unwrap();
    s
}

fn tq(f: u32, t: Vec<u8>) -> QueryRef {
    Arc::new(TermQuery::new(f, t))
}

fn queries(rng: &mut StdRng) -> Vec<QueryRef> {
    let mut out: Vec<QueryRef> = Vec::new();
    for _ in 0..6 {
        out.push(tq(BODY, word(zipf(rng, 60))));
    }
    for _ in 0..6 {
        out.push(Arc::new(BooleanQuery::new().must(tq(BODY, word(zipf(rng, 40)))).must(tq(BODY, word(zipf(rng, 200))))));
    }
    for _ in 0..4 {
        out.push(Arc::new(
            BooleanQuery::new()
                .must(tq(BODY, word(zipf(rng, 20))))
                .must(tq(BODY, word(zipf(rng, 60))))
                .must(tq(TITLE, word(zipf(rng, 10)))),
        ));
    }
    for _ in 0..4 {
        // Filters join the score-first conjunction as zero-scoring clauses.
        out.push(Arc::new(
            BooleanQuery::new()
                .must(tq(BODY, word(zipf(rng, 30))))
                .must(tq(BODY, word(zipf(rng, 100))))
                .filter(tq(TAG, format!("t{}", rng.gen_range(0..4)).into_bytes())),
        ));
    }
    for _ in 0..3 {
        out.push(Arc::new(
            BooleanQuery::new()
                .must(tq(BODY, word(zipf(rng, 30))))
                .must(tq(BODY, word(zipf(rng, 30))))
                .must_not(tq(BODY, word(zipf(rng, 100)))),
        ));
    }
    for n in [2usize, 2, 3, 4, 6] {
        let mut b = BooleanQuery::new();
        for _ in 0..n {
            b = b.should(tq(BODY, word(zipf(rng, 300))));
        }
        out.push(Arc::new(b));
    }
    for _ in 0..3 {
        out.push(Arc::new(
            BooleanQuery::new()
                .should(tq(BODY, word(zipf(rng, 50))))
                .should(tq(BODY, word(zipf(rng, 200))))
                .filter(tq(TAG, format!("t{}", rng.gen_range(0..4)).into_bytes())),
        ));
    }
    for _ in 0..2 {
        out.push(Arc::new(
            BooleanQuery::new()
                .should(tq(BODY, word(zipf(rng, 50))))
                .should(tq(BODY, word(zipf(rng, 100))))
                .should(tq(BODY, word(zipf(rng, 200))))
                .with_msm(2),
        ));
    }
    for _ in 0..3 {
        out.push(Arc::new(
            BooleanQuery::new().must(tq(BODY, word(zipf(rng, 30)))).should(tq(BODY, word(zipf(rng, 100)))),
        ));
    }
    for _ in 0..3 {
        out.push(Arc::new(PhraseQuery::new(BODY, vec![word(zipf(rng, 20)), word(zipf(rng, 20))], rng.gen_range(0..3))));
    }
    // Exact phrases prune with the merged impacts of their terms.
    for _ in 0..6 {
        out.push(Arc::new(PhraseQuery::new(BODY, vec![word(zipf(rng, 8)), word(zipf(rng, 40))], 0)));
    }
    for _ in 0..3 {
        out.push(Arc::new(PhraseQuery::new(BODY, vec![word(zipf(rng, 6)), word(zipf(rng, 6)), word(zipf(rng, 12))], 0)));
    }
    for _ in 0..2 {
        let w = word(zipf(rng, 4));
        out.push(Arc::new(PhraseQuery::new(BODY, vec![w.clone(), w], 0)));
    }
    for _ in 0..3 {
        let slot = |rng: &mut StdRng, n: usize| (0..n).map(|_| word(zipf(rng, 10))).collect::<Vec<_>>();
        let (na, nb) = (1 + rng.gen_range(0..2), 1 + rng.gen_range(0..3));
        let (a, b) = (slot(rng, na), slot(rng, nb));
        out.push(Arc::new(MultiPhraseQuery::new(BODY, vec![a, b], vec![0, 1], 0).unwrap()));
    }
    for _ in 0..3 {
        out.push(Arc::new(
            BooleanQuery::new()
                .should(Arc::new(PhraseQuery::new(BODY, vec![word(zipf(rng, 8)), word(zipf(rng, 8))], 0)) as QueryRef)
                .should(tq(BODY, word(zipf(rng, 100)))),
        ));
    }
    out
}

#[test]
fn pruned_top_k_equals_exhaustive() {
    // PRUNING_SEEDS=<n> sweeps more corpora (every third without deletes).
    let n: u64 = std::env::var("PRUNING_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    for (seed, deletes) in (1..=n).map(|s| (s, s % 3 != 1)) {
        let s = corpus(seed, 12_000, deletes).searcher();
        let mut rng = StdRng::seed_from_u64(seed + 100);
        for q in queries(&mut rng) {
            let mut all = TopScoreCollector::new(s.num_docs() as usize + 1, u64::MAX);
            s.search(&q, &mut all).unwrap();
            let all = all.top_docs();
            assert_eq!(all.relation, TotalRelation::EqualTo);
            for k in [1usize, 10, 100] {
                let mut c = TopScoreCollector::new(k, k as u64);
                s.search(&q, &mut c).unwrap();
                let got = c.top_docs();
                let want: Vec<(u32, f32)> = all.hits.iter().take(k).map(|h| (h.doc, h.score)).collect();
                let have: Vec<(u32, f32)> = got.hits.iter().map(|h| (h.doc, h.score)).collect();
                assert_eq!(have, want, "seed {seed} top-{k} of {}", q.lucene_string(s.schema()));
                assert!(got.total_hits <= all.total_hits);
                if got.relation == TotalRelation::EqualTo {
                    assert_eq!(got.total_hits, all.total_hits, "exact total of {}", q.lucene_string(s.schema()));
                } else {
                    assert!(got.total_hits > k as u64);
                }
            }
        }
    }
}

/// `distance_feature` swaps its iterator for the docs near the origin once a minimum
/// competitive score is known: the top hits must not change.
#[test]
fn distance_feature_top_k_equals_exhaustive() {
    use search_codec::{DocValuesType, PointsSpec};
    use search_index::doc::{DocValue, IndexableDoc};
    use search_index::query::DistanceFeatureQuery;
    use search_index::schema::FieldSchema;
    use search_index::util::geo::encode_geo_dv;
    const D: u32 = 30;
    const G: u32 = 31;
    let schema = search_index::Schema::new()
        .with_field(FieldSchema::new(D, "d").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(G, "g").with_points(PointsSpec::new(2, 4)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap();
    let mut rng = StdRng::seed_from_u64(77);
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    let n = 30_000;
    for i in 0..n {
        let mut doc = IndexableDoc::new(format!("d{i}").into_bytes());
        for _ in 0..rng.gen_range(0..3) {
            let v: i64 = rng.gen_range(-1_000_000..1_000_000);
            doc = doc.point(D, search_codec::points::encode::i64_bytes(v).to_vec()).doc_value(D, DocValue::SortedNumeric(v));
        }
        for _ in 0..rng.gen_range(0..3) {
            let (lat, lon) = (rng.gen_range(-60.0..60.0), rng.gen_range(-180.0..180.0));
            let enc = encode_geo_dv(lat, lon);
            let mut packed = search_codec::points::encode::i32_bytes((enc >> 32) as i32).to_vec();
            packed.extend_from_slice(&search_codec::points::encode::i32_bytes(enc as i32));
            doc = doc.point(G, packed).doc_value(G, DocValue::SortedNumeric(enc));
        }
        s.index(IndexOp::new(doc)).unwrap();
        if i % 9000 == 8999 {
            s.refresh().unwrap();
        }
        if i % 7 == 3 {
            s.delete(DeleteOp::new(format!("d{}", rng.gen_range(0..=i)).into_bytes())).unwrap();
        }
    }
    s.refresh().unwrap();
    let se = s.searcher();
    let queries: Vec<QueryRef> = vec![
        DistanceFeatureQuery::long(D, 1.0, 0, 1000).unwrap(),
        DistanceFeatureQuery::long(D, 2.0, 999_000, 50).unwrap(),
        DistanceFeatureQuery::long(D, 1.0, i64::MIN, 1).unwrap(),
        DistanceFeatureQuery::geo(G, 1.0, 10.0, 20.0, 50_000.0).unwrap(),
        DistanceFeatureQuery::geo(G, 1.5, 0.0, 179.9, 10_000.0).unwrap(),
    ];
    let mut pruned = 0;
    for q in queries {
        let mut all = TopScoreCollector::new(n, u64::MAX);
        se.search(&q, &mut all).unwrap();
        let all = all.top_docs();
        for k in [1usize, 10, 100] {
            let mut c = TopScoreCollector::new(k, k as u64);
            se.search(&q, &mut c).unwrap();
            let got = c.top_docs();
            let want: Vec<(u32, f32)> = all.hits.iter().take(k).map(|h| (h.doc, h.score)).collect();
            let have: Vec<(u32, f32)> = got.hits.iter().map(|h| (h.doc, h.score)).collect();
            assert_eq!(have, want, "top-{k} of {}", q.lucene_string(se.schema()));
            assert!(got.total_hits <= all.total_hits);
            pruned += (got.total_hits < all.total_hits / 2) as usize;
        }
    }
    // The near-origin iterators did kick in.
    assert!(pruned >= 5, "{pruned}");
}

/// Concurrent top-k: slices share the hit count and the best score bar, prune as soon as the
/// whole search may, and still return exactly the exhaustive top-k.
#[test]
fn concurrent_pruned_top_k_equals_exhaustive() {
    use search_index::SearchControl;
    use search_index::collector::TopScoreManager;
    use search_index::search::SearchLimits;
    for (seed, deletes) in [(11u64, false), (12, true)] {
        let s = corpus_in(seed, 12_000, deletes, 7).searcher();
        assert!(s.segments().len() >= 3, "{} segments", s.segments().len());
        let par = s.with_limits(SearchLimits { parallel_min_docs: 500, parallelism: 4, ..SearchLimits::default() });
        let mut rng = StdRng::seed_from_u64(seed + 100);
        for q in queries(&mut rng) {
            let mut all = TopScoreCollector::new(s.num_docs() as usize + 1, u64::MAX);
            s.search(&q, &mut all).unwrap();
            let all = all.top_docs();
            for (k, threshold) in [(1usize, 1u64), (10, 10), (10, 1000), (100, 100)] {
                let m = TopScoreManager { n: k, threshold, after: None, min_score: None };
                let got = par.search_concurrent(&q, &m, &SearchControl::default()).unwrap();
                let want: Vec<(u32, f32)> = all.hits.iter().take(k).map(|h| (h.doc, h.score)).collect();
                let have: Vec<(u32, f32)> = got.hits.iter().map(|h| (h.doc, h.score)).collect();
                assert_eq!(have, want, "seed {seed} top-{k} threshold {threshold} of {}", q.lucene_string(s.schema()));
                assert!(got.total_hits <= all.total_hits);
                if got.relation == TotalRelation::EqualTo {
                    assert_eq!(got.total_hits, all.total_hits, "exact total of {}", q.lucene_string(s.schema()));
                } else {
                    assert!(got.total_hits > threshold, "{} <= {threshold}", got.total_hits);
                }
            }
        }
    }
}

//! Global ordinals and parent/child joins across segment layouts (several segments, deletes,
//! merges) against brute-force models.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rand::Rng;
use search_codec::{DocValuesType, IndexOptions};
use search_index::collector::TopScoreCollector;
use search_index::doc::DocValue;
use search_index::query::{BooleanQuery, JoinQuery, JoinScoreMode, TermQuery};
use search_index::schema::FieldSchema;
use search_index::{DeleteOp, IndexOp, IndexableDoc, QueryRef, Schema, Searcher, Shard, ShardConfig};

const T: u32 = 10;
const KIND: u32 = 11;
const JQ: u32 = 12;
const TAGS: u32 = 13;

fn schema() -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqs, true))
        .unwrap()
        .with_field(FieldSchema::new(KIND, "join").indexed(IndexOptions::Docs, false))
        .unwrap()
        .with_field(FieldSchema::new(JQ, "join#p").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::Sorted))
        .unwrap()
        .with_field(FieldSchema::new(TAGS, "tags").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet))
        .unwrap()
}

fn term(f: u32, t: &str) -> QueryRef {
    Arc::new(TermQuery::new(f, t.as_bytes().to_vec()))
}

/// Global doc → (join value, kind) of every live doc.
fn model(s: &Searcher) -> HashMap<u32, (String, String)> {
    let mut out = HashMap::new();
    for seg in s.segments() {
        let Some(search_codec::DocValuesReader::Sorted(v)) = seg.reader.doc_values(JQ) else { continue };
        for d in 0..seg.reader.max_doc() {
            if seg.live.as_ref().is_some_and(|l| !l.is_live(d)) {
                continue;
            }
            let mut b = Vec::new();
            v.lookup_ord(v.ord(d).unwrap(), &mut b);
            let kind = if is_kind(s, seg.base + d, "p") { "p" } else { "c" };
            out.insert(seg.base + d, (String::from_utf8(b).unwrap(), kind.to_string()));
        }
    }
    out
}

fn is_kind(s: &Searcher, doc: u32, kind: &str) -> bool {
    let q = term(KIND, kind);
    s.explain(&q, doc).unwrap().is_match()
}

#[test]
fn global_ordinals_follow_term_order_across_segments() {
    let mut rng = common::rng(3);
    // Many small segments: no merging.
    let mut cfg = ShardConfig { merge_threads: 0, ..Default::default() };
    cfg.merge_policy.segments_per_tier = 1000.0;
    let shard = Shard::open(cfg, schema(), Vec::new()).unwrap();
    for i in 0..3000 {
        let mut d = IndexableDoc::new(format!("d{i}").into_bytes());
        for _ in 0..rng.gen_range(0..4) {
            let tag = format!("tag{}", rng.gen_range(0..500));
            d = d.doc_value(TAGS, DocValue::SortedSet(tag.into_bytes().into()));
        }
        shard.index(IndexOp::new(d)).unwrap();
        if i % 300 == 299 {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    assert!(s.segments().len() > 3, "{} segments", s.segments().len());
    let g = s.global_ordinals(TAGS).unwrap();
    let mut all: Vec<Vec<u8>> = Vec::new();
    for seg in s.segments() {
        if let Some(search_codec::DocValuesReader::SortedSet(v)) = seg.reader.doc_values(TAGS) {
            for o in 0..v.value_count() {
                let mut b = Vec::new();
                v.lookup_ord(o, &mut b);
                // Each segment ordinal maps to the global ordinal of its term's rank.
                all.push(b.clone());
                let gl = g.global(seg.ord, o);
                let mut back = Vec::new();
                g.lookup(&s, TAGS, gl, &mut back);
                assert_eq!(back, b);
            }
        }
    }
    all.sort();
    all.dedup();
    assert_eq!(g.value_count(), all.len() as u64);
    for (i, t) in all.iter().enumerate() {
        let mut back = Vec::new();
        g.lookup(&s, TAGS, i as u64, &mut back);
        assert_eq!(&back, t);
    }
    assert!(Arc::ptr_eq(&g, &s.global_ordinals(TAGS).unwrap()), "cached per searcher");
    assert!(s.global_ordinals(T).is_none());
}

#[test]
fn joins_match_brute_force_over_layouts() {
    for seed in 0..std::env::var("SWEEP_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6u64) {
        let mut rng = common::rng(100 + seed);
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
        let parents = 60;
        let words = ["a", "b", "c", "d"];
        for i in 0..400 {
            let (id, kind, jv) = if i < parents || rng.gen_bool(0.1) {
                let id = format!("p{i}");
                (id.clone(), "p", id)
            } else {
                (format!("c{i}"), "c", format!("p{}", rng.gen_range(0..parents)))
            };
            let text: Vec<&str> = (0..rng.gen_range(1..5)).map(|_| words[rng.gen_range(0..words.len())]).collect();
            let d = IndexableDoc::new(id.into_bytes())
                .term(KIND, kind.as_bytes().to_vec())
                .term(JQ, jv.clone().into_bytes())
                .doc_value(JQ, DocValue::Sorted(jv.into_bytes().into()));
            let mut d = d;
            for w in &text {
                d = d.term(T, w.as_bytes().to_vec());
            }
            shard.index(IndexOp::new(d)).unwrap();
            if rng.gen_bool(0.02) {
                shard.refresh().unwrap();
            }
            if rng.gen_bool(0.05) {
                let victim = if rng.gen_bool(0.5) { format!("p{}", rng.gen_range(0..i + 1)) } else { format!("c{}", rng.gen_range(0..i + 1)) };
                shard.delete(DeleteOp::new(victim.into_bytes())).unwrap();
            }
        }
        shard.refresh().unwrap();
        if seed % 3 == 2 {
            shard.force_merge(Some(2), false).unwrap();
        }
        let s = shard.searcher();
        let m = model(&s);
        for (w, mode, min, max) in [
            ("a", JoinScoreMode::None, 1, i32::MAX as u32),
            ("b", JoinScoreMode::Total, 1, i32::MAX as u32),
            ("c", JoinScoreMode::Max, 2, i32::MAX as u32),
            ("a", JoinScoreMode::Min, 1, 3),
            ("d", JoinScoreMode::Avg, 1, i32::MAX as u32),
            ("b", JoinScoreMode::None, 2, 4),
        ] {
            // has_child: parents by their children's scores.
            let from: QueryRef = Arc::new(BooleanQuery::new().must(term(T, w)).filter(term(KIND, "c")));
            let mut child = TopScoreCollector::new(1000, u64::MAX);
            s.search(&from, &mut child).unwrap();
            let mut hits: Vec<_> = child.top_docs().hits;
            hits.sort_by_key(|h| h.doc);
            // Per join value: collected scores in doc order.
            let mut agg: BTreeMap<String, Vec<f32>> = BTreeMap::new();
            for h in &hits {
                agg.entry(m[&h.doc].0.clone()).or_default().push(h.score);
            }
            let q: QueryRef = Arc::new(JoinQuery { join_field: JQ, from, to: term(KIND, "p"), score_mode: mode, min, max });
            let mut c = TopScoreCollector::new(1000, u64::MAX);
            s.search(&q, &mut c).unwrap();
            let got: HashMap<u32, f32> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
            for (&doc, (jv, kind)) in &m {
                let scores = if kind == "p" { agg.get(jv) } else { None };
                let want = scores.filter(|v| v.len() as u32 >= min && v.len() as u32 <= max).map(|v| match mode {
                    JoinScoreMode::None => 1.0,
                    JoinScoreMode::Total => v.iter().fold(0f32, |a, b| a + b),
                    JoinScoreMode::Max => v.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b)),
                    JoinScoreMode::Min => v.iter().fold(f32::INFINITY, |a, &b| a.min(b)),
                    JoinScoreMode::Avg => v.iter().fold(0f32, |a, b| a + b) / v.len() as f32,
                });
                assert_eq!(got.get(&doc).copied(), want, "seed {seed} {w} {mode:?} {min}..{max}: doc {doc} ({jv}, {kind})");
            }
            // has_parent (score: max of the one parent's score).
            let pfrom: QueryRef = Arc::new(BooleanQuery::new().must(term(T, w)).filter(term(KIND, "p")));
            let mut parent = TopScoreCollector::new(1000, u64::MAX);
            s.search(&pfrom, &mut parent).unwrap();
            let pscore: HashMap<String, f32> = parent.top_docs().hits.iter().map(|h| (m[&h.doc].0.clone(), h.score)).collect();
            let q: QueryRef = Arc::new(JoinQuery::new(JQ, pfrom, term(KIND, "c"), JoinScoreMode::Max));
            let mut c = TopScoreCollector::new(1000, u64::MAX);
            s.search(&q, &mut c).unwrap();
            let got: HashMap<u32, f32> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
            for (&doc, (jv, kind)) in &m {
                let want = if kind == "c" { pscore.get(jv).copied() } else { None };
                assert_eq!(got.get(&doc).copied(), want, "seed {seed} has_parent {w}: doc {doc} ({jv})");
            }
        }
    }
}

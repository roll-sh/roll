//! Completion suggestions against Elasticsearch 9.5.5 (fixture `completion.json`: three
//! segments, duplicates, a deleted doc; prefix, fuzzy and skip_duplicates requests), plus a
//! brute-force model over random corpora and segment layouts.

mod common;

use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use search_codec::automaton::dfa::Dfa;
use search_codec::automaton::{Automaton, Prefix};
use search_index::doc::{IndexableDoc, IndexableField};
use search_index::schema::FieldSchema;
use search_index::suggest::{BoostFn, CompletionQuery, completion_score, fuzzy_prefix_boost};
use search_index::{DeleteOp, IndexOp, Schema, Searcher, Shard, ShardConfig};
use serde_json::Value;

const S: u32 = 10;

fn schema() -> Schema {
    Schema::new().with_field(FieldSchema::new(S, "s").with_suggest()).unwrap()
}

/// The `simple` analyzer (lowercased letter runs) joined with Lucene's
/// `ConcatenateGraphFilter.SEP_LABEL` (0x1f), as Elasticsearch encodes completion inputs
/// (`preserve_separators`).
fn analyze(text: &str) -> Vec<u8> {
    let toks: Vec<String> =
        text.split(|c: char| !c.is_alphabetic()).filter(|t| !t.is_empty()).map(|t| t.to_lowercase()).collect();
    toks.join("\u{1f}").into_bytes()
}

fn doc(id: &str, inputs: &[&str], weight: u64) -> IndexableDoc {
    let mut d = IndexableDoc::new(id.as_bytes().to_vec());
    for i in inputs {
        d = d.field(IndexableField::Suggest { field: S, key: analyze(i).into(), weight, payload: i.as_bytes().to_vec().into() });
    }
    d
}

fn ids(s: &Searcher) -> HashMap<u32, String> {
    common::ids(s)
}

#[test]
fn completion_matches_elasticsearch() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/completion.json")).unwrap();
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
    for batch in fx["batches"].as_array().unwrap() {
        for d in batch.as_array().unwrap() {
            let inputs: Vec<&str> = d[1].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
            shard.index(IndexOp::new(doc(d[0].as_str().unwrap(), &inputs, d[2].as_u64().unwrap()))).unwrap();
        }
        shard.refresh().unwrap();
    }
    for id in fx["delete"].as_array().unwrap() {
        shard.delete(DeleteOp::new(id.as_str().unwrap().as_bytes().to_vec())).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    assert!(s.segments().len() >= 3);
    let ids = ids(&s);
    let mut failures = Vec::new();
    for c in fx["cases"].as_array().unwrap() {
        let q = &c["q"];
        let text = analyze(q["prefix"].as_str().unwrap());
        let size = q["size"].as_u64().unwrap() as usize;
        let skip = q.get("skip_duplicates").and_then(|v| v.as_bool()).unwrap_or(false);
        let (aut, boost): (Box<dyn Automaton>, Option<Box<BoostFn>>) = match q.get("fuzzy") {
            Some(f) => {
                let originals = vec![text.clone()];
                let t = String::from_utf8(text.clone()).unwrap();
                // min_length 3, prefix_length 1, transpositions.
                let aut: Box<dyn Automaton> = if t.len() < 3 {
                    Box::new(Prefix::new(text.clone()))
                } else {
                    Box::new(Dfa::levenshtein(&t, f.as_u64().unwrap() as u32, 1, true))
                };
                (aut, Some(Box::new(move |p: &[u8]| fuzzy_prefix_boost(p, &originals))))
            }
            None => (Box::new(Prefix::new(text.clone())), None),
        };
        let cq = CompletionQuery { field: S, automaton: aut.as_ref(), boost: boost.as_deref(), size, skip_duplicates: skip, one_per_doc: true };
        let got: Vec<(String, String, f32)> = s
            .complete(&cq)
            .unwrap()
            .into_iter()
            .map(|h| (String::from_utf8(h.payload).unwrap(), ids[&h.doc].clone(), h.score))
            .collect();
        let want: Vec<(String, String, f32)> = c["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| (o["text"].as_str().unwrap().to_string(), o["id"].as_str().unwrap().to_string(), o["score"].as_f64().unwrap() as f32))
            .collect();
        if got != want {
            failures.push(format!("{q}\n  ours {got:?}\n  es   {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One entry of the model.
#[derive(Clone)]
struct Entry {
    id: usize,
    key: Vec<u8>,
    payload: Vec<u8>,
    weight: u64,
}

/// The suggestions a single segment holding every live entry gives without deduplication or
/// grouping: every matching entry by (score desc, payload, doc) — the model for layouts where
/// the per-segment cut-offs cannot bite (size ≥ entries).
fn model(entries: &[Entry], live: &[bool], docs: &HashMap<usize, u32>, prefix: &[u8], boost: f32) -> Vec<(usize, Vec<u8>, f32)> {
    let mut v: Vec<(usize, Vec<u8>, f32)> = entries
        .iter()
        .filter(|e| live[e.id] && e.key.starts_with(prefix))
        .map(|e| (e.id, e.payload.clone(), completion_score(e.weight as f32, boost)))
        .collect();
    v.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap().then_with(|| a.1.cmp(&b.1)).then(docs[&a.0].cmp(&docs[&b.0])));
    v
}

#[test]
fn completion_is_layout_independent_and_complete() {
    for seed in 0..12u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let words = ["ab", "abc", "abd", "b", "ba", "bab", "c"];
        let n = 60;
        let mut entries = Vec::new();
        let mut docs = Vec::new();
        for id in 0..n {
            let k = rng.gen_range(1..3);
            let mut inputs = Vec::new();
            for _ in 0..k {
                let w = words[rng.gen_range(0..words.len())];
                let w2 = if rng.gen_bool(0.3) { format!("{w} {}", words[rng.gen_range(0..words.len())]) } else { w.to_string() };
                inputs.push(w2);
            }
            let weight = rng.gen_range(0..20u64);
            for i in &inputs {
                entries.push(Entry { id, key: analyze(i), payload: i.as_bytes().to_vec(), weight });
            }
            docs.push((inputs, weight));
        }
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema(), Vec::new()).unwrap();
        for (id, (inputs, w)) in docs.iter().enumerate() {
            let refs: Vec<&str> = inputs.iter().map(|s| s.as_str()).collect();
            shard.index(IndexOp::new(doc(&id.to_string(), &refs, *w))).unwrap();
            if rng.gen_bool(0.1) {
                shard.refresh().unwrap();
            }
        }
        let mut live = vec![true; n];
        for (id, l) in live.iter_mut().enumerate() {
            if rng.gen_bool(0.15) {
                *l = false;
                shard.delete(DeleteOp::new(id.to_string().into_bytes())).unwrap();
            }
        }
        shard.refresh().unwrap();
        if seed % 3 == 0 {
            shard.force_merge(Some(1), false).unwrap();
        }
        let s = shard.searcher();
        let idmap = common::ids(&s);
        let docs: HashMap<usize, u32> = idmap.iter().map(|(d, id)| (id.parse().unwrap(), *d)).collect();
        for prefix in ["a", "ab", "b", "ba", "c", "ab\u{1f}", "x"] {
            for boost in [0.0f32, 2.5] {
                let p = prefix.as_bytes().to_vec();
                let f = move |_: &[u8]| boost;
                let aut = Prefix::new(p.clone());
                let all = entries.len() + 1;
                let q = CompletionQuery {
                    field: S,
                    automaton: &aut,
                    boost: if boost == 0.0 { None } else { Some(&f) },
                    size: all,
                    skip_duplicates: false,
                    one_per_doc: false,
                };
                let got: Vec<(usize, Vec<u8>, f32)> =
                    s.complete(&q).unwrap().into_iter().map(|h| (idmap[&h.doc].parse().unwrap(), h.payload, h.score)).collect();
                let want = model(&entries, &live, &docs, &p, boost);
                assert_eq!(got, want, "seed {seed} prefix {prefix:?} boost {boost}");
                // Deduplicated: the best of each surface form, still in order.
                let q = CompletionQuery { skip_duplicates: true, ..q };
                let got: Vec<(Vec<u8>, f32)> = s.complete(&q).unwrap().into_iter().map(|h| (h.payload, h.score)).collect();
                let mut seen = std::collections::HashSet::new();
                let want: Vec<(Vec<u8>, f32)> = want.iter().filter(|w| seen.insert(w.1.clone())).map(|w| (w.1.clone(), w.2)).collect();
                assert_eq!(got, want, "dedup seed {seed} prefix {prefix:?}");
                // Small sizes return a prefix of the full ranking.
                for size in [1, 3] {
                    let q = CompletionQuery { size, skip_duplicates: false, ..q };
                    let got: Vec<(Vec<u8>, f32)> = s.complete(&q).unwrap().into_iter().map(|h| (h.payload, h.score)).collect();
                    let full = model(&entries, &live, &docs, &p, boost);
                    let want: Vec<(Vec<u8>, f32)> = full.iter().take(size).map(|w| (w.1.clone(), w.2)).collect();
                    assert_eq!(got, want, "size {size} seed {seed} prefix {prefix:?}");
                }
            }
        }
    }
}

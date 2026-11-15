//! `match_only_text`: phrase and phrase-prefix queries confirmed against the docs' values
//! (`SourceConfirmedQuery`) on a field indexed with doc ids only, against Elasticsearch 9.5.5
//! (fixture: `tests/fixtures/gen_mot.py`): hits, constant scores and explanation texts,
//! multi-valued fields with the position increment gap, slop, and phrase-prefix expansion.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use search_analysis::{Analysis, Analyzer};
use search_codec::IndexOptions;
use search_codec::stored::OwnedStoredValue;
use search_index::collector::TopScoreCollector;
use search_index::doc::{IndexableDoc, IndexableField, StoredField};
use search_index::query::source_confirmed::{LeafSourceValues, SourceValues};
use search_index::query::{BooleanQuery, BoostQuery, ConstantScoreQuery, MultiPhrasePrefixQuery, PhraseQuery, SourceConfirmedQuery, TermQuery};
use search_index::schema::FieldSchema;
use search_index::{Explanation, IndexOp, QueryRef, Schema, SegmentContext, Shard, ShardConfig};
use serde_json::Value;

const M: u32 = 10;
const SRC: u32 = 11;
const ID: u32 = 12;

/// Values from a stored field standing in for `_source`.
struct Stored;

struct LeafStored(search_codec::SegmentReader);

impl SourceValues for Stored {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafSourceValues>> {
        Ok(Box::new(LeafStored(seg.reader.clone())))
    }
}

impl LeafSourceValues for LeafStored {
    fn values(&mut self, doc: u32) -> search_index::Result<Vec<String>> {
        let mut out = Vec::new();
        for (f, v) in self.0.stored().document(doc)? {
            if f == SRC
                && let OwnedStoredValue::Str(s) = v
            {
                out.push(s);
            }
        }
        Ok(out)
    }
}

fn values(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
        Value::String(s) => vec![s.clone()],
        _ => vec![],
    }
}

fn confirmed(a: &Analyzer, q: QueryRef) -> QueryRef {
    Arc::new(ConstantScoreQuery::new(Arc::new(SourceConfirmedQuery::new(q, M, a.clone(), Arc::new(Stored)))))
}

/// The query Elasticsearch builds for one fixture query on the `match_only_text` field.
fn build(a: &Analyzer, q: &Value) -> QueryRef {
    let (kind, body) = q.as_object().unwrap().iter().next().unwrap();
    if kind == "bool" {
        let mut b = BooleanQuery::new();
        for c in body["must"].as_array().unwrap() {
            b = b.must(build(a, c));
        }
        for c in body["filter"].as_array().unwrap() {
            // A filter keeps the term query unwrapped (Elasticsearch's non-scoring shortcut).
            let text = c["match"]["m"].as_str().unwrap();
            b = b.filter(Arc::new(TermQuery::new(M, a.terms(text)[0].clone().into_bytes())));
        }
        return Arc::new(b);
    }
    let spec = &body["m"];
    let (text, slop, boost) = match spec {
        Value::String(s) => (s.clone(), 0, 1.0),
        o => (
            o["query"].as_str().unwrap().to_string(),
            o.get("slop").and_then(Value::as_u64).unwrap_or(0) as u32,
            o.get("boost").and_then(Value::as_f64).unwrap_or(1.0) as f32,
        ),
    };
    let terms: Vec<Vec<u8>> = a.terms(&text).into_iter().map(String::into_bytes).collect();
    let q: QueryRef = match kind.as_str() {
        "match_phrase" => confirmed(a, Arc::new(PhraseQuery::new(M, terms, slop))),
        "match_phrase_prefix" => {
            let positions = (0..terms.len() as u32).collect();
            let slots = terms.into_iter().map(|t| vec![t]).collect();
            confirmed(a, Arc::new(MultiPhrasePrefixQuery::new(M, slots, positions, slop, 50).unwrap()))
        }
        _ => {
            let and = spec.get("operator").and_then(Value::as_str) == Some("and");
            let clauses: Vec<QueryRef> =
                terms.into_iter().map(|t| Arc::new(ConstantScoreQuery::new(Arc::new(TermQuery::new(M, t)))) as QueryRef).collect();
            if clauses.len() == 1 {
                clauses[0].clone()
            } else {
                Arc::new(clauses.into_iter().fold(BooleanQuery::new(), |b, c| if and { b.must(c) } else { b.should(c) }))
            }
        }
    };
    if boost != 1.0 { Arc::new(BoostQuery::new(q, boost)) } else { q }
}

/// Phrase-prefix expansions come out of a Java `HashSet<Term>` whose order depends on a
/// per-JVM hash seed: we list them sorted, so their order isn't compared.
fn same(ours: &Explanation, es: &Value) -> bool {
    let words = |s: &str| {
        let mut v: Vec<String> = s.replace(['(', ')'], " ").split_whitespace().map(str::to_string).collect();
        v.sort();
        v
    };
    let es_desc = es["description"].as_str().unwrap();
    (ours.value() - es["value"].as_f64().unwrap() as f32).abs() < 1e-6
        && (ours.description == es_desc || words(&ours.description) == words(es_desc))
}

#[test]
fn match_only_text_queries_match_elasticsearch() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/mot.json")).unwrap();
    let a = Analysis::global().analyzer("standard").unwrap();
    let schema = Schema::new().with_field(FieldSchema::new(M, "m").indexed(IndexOptions::Docs, false)).unwrap();
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
        let vals = values(d);
        let mut doc = IndexableDoc::new(i.to_string().into_bytes())
            .field(IndexableField::Text { field: M, analyzer: a.clone(), values: vals.clone() });
        for v in vals {
            doc.stored.push(StoredField { field: SRC, value: OwnedStoredValue::Str(v) });
        }
        doc.stored.push(StoredField { field: ID, value: OwnedStoredValue::Str(i.to_string()) });
        shard.index(IndexOp::new(doc)).unwrap();
        if i % 5 == 4 {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let mut ids: HashMap<u32, String> = HashMap::new();
    for seg in s.segments() {
        for d in 0..seg.reader.max_doc() {
            for (f, v) in seg.reader.stored().document(d).unwrap() {
                if f == ID
                    && let OwnedStoredValue::Str(x) = v
                {
                    ids.insert(seg.base + d, x);
                }
            }
        }
    }
    let by_id: HashMap<String, u32> = ids.iter().map(|(d, i)| (i.clone(), *d)).collect();
    for c in fx["cases"].as_array().unwrap() {
        let q = build(&a, &c["query"]);
        let what = c["query"].to_string();
        let mut col = TopScoreCollector::new(100, u64::MAX);
        s.search(&q, &mut col).unwrap();
        let td = col.top_docs();
        assert_eq!(td.total_hits, c["total"].as_u64().unwrap(), "{what}");
        let mut got: Vec<(String, f32)> = td.hits.iter().map(|h| (ids[&h.doc].clone(), h.score)).collect();
        let mut want: Vec<(String, f32)> =
            c["hits"].as_array().unwrap().iter().map(|h| (h["id"].as_str().unwrap().to_string(), h["score"].as_f64().unwrap() as f32)).collect();
        got.sort_by(|x, y| x.0.cmp(&y.0));
        want.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(got, want, "{what}");
        assert_eq!(s.count(&q).unwrap(), want.len() as u64, "count {what}");
        if let Some(h) = c["hits"].as_array().unwrap().first() {
            let e = s.explain(&q, by_id[h["id"].as_str().unwrap()]).unwrap();
            let es = &h["explanation"];
            assert!(same(&e, es), "{what}: explanation {}\nElasticsearch {es}", e.to_text());
            for (o, x) in e.details.iter().zip(es["details"].as_array().unwrap()) {
                assert!(same(o, x), "{what}: detail {}\nElasticsearch {x}", o.to_text());
            }
        }
        // Docs Elasticsearch doesn't return don't match in explain either.
        for (id, &d) in &by_id {
            if !c["hits"].as_array().unwrap().iter().any(|h| h["id"].as_str() == Some(id)) {
                assert!(!s.explain(&q, d).unwrap().is_match(), "{what}: {id} explains as a match");
            }
        }
    }
}

/// Phrases confirmed from values (fast path) and through a one-doc segment (any query) equal
/// the same phrases on a positions-indexed copy of the field: exact and sloppy, repeated
/// terms, multi-phrases, multi-valued docs with the position gap, deletes, several segments.
#[test]
fn confirmed_phrases_equal_positional_phrases() {
    use rand::Rng;
    use search_index::query::MultiPhraseQuery;
    const P: u32 = 13;
    let a = Analysis::global().analyzer("standard").unwrap();
    for seed in 0..6u64 {
        let mut rng = common::rng(seed + 40);
        let schema = Schema::new()
            .with_field(FieldSchema::new(M, "m").indexed(IndexOptions::Docs, false))
            .unwrap()
            .with_field(FieldSchema::new(P, "p").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
            .unwrap();
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
        let word = |rng: &mut rand::rngs::StdRng| format!("w{}", common::zipf(rng, 8));
        for i in 0..400 {
            let vals: Vec<String> = (0..rng.gen_range(1..3)).map(|_| (0..rng.gen_range(0..12)).map(|_| word(&mut rng)).collect::<Vec<_>>().join(" ")).collect();
            let mut doc = IndexableDoc::new(format!("d{i}").into_bytes())
                .field(IndexableField::Text { field: M, analyzer: a.clone(), values: vals.clone() })
                .field(IndexableField::Text { field: P, analyzer: a.clone(), values: vals.clone() });
            for v in vals {
                doc.stored.push(StoredField { field: SRC, value: OwnedStoredValue::Str(v) });
            }
            shard.index(IndexOp::new(doc)).unwrap();
            if rng.gen_bool(0.01) {
                shard.refresh().unwrap();
            }
            if rng.gen_bool(0.05) {
                shard.delete(search_index::DeleteOp::new(format!("d{}", rng.gen_range(0..=i)).into_bytes())).unwrap();
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let docs = |q: &QueryRef| {
            let mut c = TopScoreCollector::new(10_000, u64::MAX);
            s.search(q, &mut c).unwrap();
            let mut v: Vec<u32> = c.top_docs().hits.iter().map(|h| h.doc).collect();
            v.sort();
            v
        };
        for qi in 0..60 {
            let n = rng.gen_range(2..4);
            let slop = rng.gen_range(0..4);
            let multi = rng.gen_bool(0.3);
            let slots: Vec<Vec<Vec<u8>>> =
                (0..n).map(|_| (0..if multi { rng.gen_range(1..3) } else { 1 }).map(|_| word(&mut rng).into_bytes()).collect()).collect();
            let positions: Vec<u32> = (0..n as u32).collect();
            let (on_m, on_p): (QueryRef, QueryRef) = if multi {
                (
                    Arc::new(MultiPhraseQuery::new(M, slots.clone(), positions.clone(), slop).unwrap()),
                    Arc::new(MultiPhraseQuery::new(P, slots.clone(), positions.clone(), slop).unwrap()),
                )
            } else {
                let t: Vec<Vec<u8>> = slots.iter().map(|x| x[0].clone()).collect();
                (Arc::new(PhraseQuery::new(M, t.clone(), slop)), Arc::new(PhraseQuery::new(P, t, slop)))
            };
            let want = docs(&on_p);
            let fast: QueryRef = Arc::new(SourceConfirmedQuery::new(on_m.clone(), M, a.clone(), Arc::new(Stored)));
            assert_eq!(docs(&fast), want, "seed {seed}: fast {}", on_p.lucene_string(s.schema()));
            if qi % 6 != 0 {
                continue;
            }
            // Not a plain phrase: confirmed through a one-doc segment (every doc a candidate).
            let not_phrase = BooleanQuery::new().must(on_m).should(Arc::new(TermQuery::new(M, b"absent".to_vec())));
            let slow: QueryRef = Arc::new(SourceConfirmedQuery::new(Arc::new(not_phrase), M, a.clone(), Arc::new(Stored)));
            assert_eq!(docs(&slow), want, "seed {seed}: slow {}", on_p.lucene_string(s.schema()));
        }
    }
}

/// Interval queries on `match_only_text` (Elasticsearch's `SourceIntervalsSource`): matches and
/// the intervals' own saturation scores and explanations, from the docs' values.
#[test]
fn match_only_text_intervals_match_elasticsearch() {
    use search_index::query::intervals::{IntervalQuery, IntervalsSource};
    use search_index::query::MultiTermQuery;
    let fx: Value = serde_json::from_str(include_str!("fixtures/mot.json")).unwrap();
    let a = Analysis::global().analyzer("standard").unwrap();
    let schema = Schema::new().with_field(FieldSchema::new(M, "m").indexed(IndexOptions::Docs, false)).unwrap();
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    for (i, d) in fx["iv_docs"].as_array().unwrap().iter().enumerate() {
        let vals = values(d);
        let mut doc = IndexableDoc::new(i.to_string().into_bytes());
        if !vals.is_empty() {
            doc = doc.field(IndexableField::Text { field: M, analyzer: a.clone(), values: vals.clone() });
        }
        for v in vals {
            doc.stored.push(StoredField { field: SRC, value: OwnedStoredValue::Str(v) });
        }
        doc.stored.push(StoredField { field: ID, value: OwnedStoredValue::Str(i.to_string()) });
        shard.index(IndexOp::new(doc)).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let ids: HashMap<u32, String> = (0..s.max_doc())
        .map(|d| {
            let seg = &s.segments()[0];
            let id = seg.reader.stored().document(d).unwrap().into_iter().find_map(|(f, v)| match (f, v) {
                (ID, OwnedStoredValue::Str(x)) => Some(x),
                _ => None,
            });
            (d, id.unwrap())
        })
        .collect();
    for c in fx["iv_cases"].as_array().unwrap() {
        let what = c["query"].to_string();
        let body = &c["query"]["intervals"]["m"];
        let (rule, spec) = body.as_object().unwrap().iter().find(|(k, _)| k.as_str() != "boost").unwrap();
        let src = match rule.as_str() {
            "match" => {
                let mut toks = Vec::new();
                a.analyze(spec["query"].as_str().unwrap(), &mut |t| toks.push((t.term.as_bytes().to_vec(), t.position_increment, t.position_length)));
                let gaps = spec.get("max_gaps").and_then(Value::as_i64).unwrap_or(-1) as i32;
                IntervalsSource::analyzed(&toks, gaps, spec.get("ordered").and_then(Value::as_bool).unwrap_or(false)).unwrap()
            }
            "prefix" => {
                let p = spec["prefix"].as_str().unwrap();
                IntervalsSource::multi_term(MultiTermQuery::prefix(M, p.as_bytes().to_vec()), 128, format!("{p}*"))
            }
            "wildcard" => {
                let p = spec["pattern"].as_str().unwrap();
                IntervalsSource::multi_term(MultiTermQuery::wildcard(M, p, false).unwrap(), 128, p)
            }
            _ => {
                let t = spec["term"].as_str().unwrap();
                IntervalsSource::multi_term(MultiTermQuery::fuzzy(M, t, 2, 0, 128, true), 128, t)
            }
        };
        let mut q: QueryRef = Arc::new(SourceConfirmedQuery::new(Arc::new(IntervalQuery::new(M, src)), M, a.clone(), Arc::new(Stored)));
        if let Some(b) = body.get("boost").and_then(Value::as_f64) {
            q = Arc::new(BoostQuery::new(q, b as f32));
        }
        let mut col = TopScoreCollector::new(100, u64::MAX);
        s.search(&q, &mut col).unwrap();
        let td = col.top_docs();
        assert_eq!(td.total_hits, c["total"].as_u64().unwrap(), "{what}");
        let got: Vec<(String, f32)> = td.hits.iter().map(|h| (ids[&h.doc].clone(), h.score)).collect();
        let want: Vec<(String, f32)> =
            c["hits"].as_array().unwrap().iter().map(|h| (h["id"].as_str().unwrap().to_string(), h["score"].as_f64().unwrap() as f32)).collect();
        assert_eq!(got, want, "{what}");
        for h in c["hits"].as_array().unwrap() {
            let d = *ids.iter().find(|(_, v)| v.as_str() == h["id"].as_str().unwrap()).unwrap().0;
            let e = s.explain(&q, d).unwrap();
            let es = &h["explanation"];
            assert!(same(&e, es), "{what}: {}\nElasticsearch {es}", e.to_text());
            for (o, x) in e.details.iter().zip(es["details"].as_array().unwrap()) {
                assert!(same(o, x), "{what}: detail {}\nElasticsearch {x}", o.to_text());
            }
        }
    }
}

/// Intervals on a `match_only_text` field whose sources partly read a positional `text` field
/// (`use_field`): the field's own sources are confirmed from `_source`, the other field's
/// read the index (`fixtures/mot_use_field.json`, from `fixtures/gen_mot_use_field.py`).
#[test]
fn match_only_text_intervals_with_use_field_match_elasticsearch() {
    use search_index::query::MultiTermQuery;
    use search_index::query::intervals::{IntervalQuery, IntervalsSource as I};
    const T: u32 = 13;
    let fx: Value = serde_json::from_str(include_str!("fixtures/mot_use_field.json")).unwrap();
    let a = Analysis::global().analyzer("standard").unwrap();
    let schema = Schema::new()
        .with_field(FieldSchema::new(M, "m").indexed(IndexOptions::Docs, false))
        .unwrap()
        .with_field(FieldSchema::new(T, "t").indexed(IndexOptions::DocsAndFreqsAndPositions, true))
        .unwrap();
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    for (i, d) in fx["docs"].as_array().unwrap().iter().enumerate() {
        let vals = values(&d["m"]);
        let mut doc = IndexableDoc::new(i.to_string().into_bytes());
        if !vals.is_empty() {
            doc = doc.field(IndexableField::Text { field: M, analyzer: a.clone(), values: vals.clone() });
        }
        doc = doc.field(IndexableField::Text { field: T, analyzer: a.clone(), values: values(&d["t"]) });
        for v in vals {
            doc.stored.push(StoredField { field: SRC, value: OwnedStoredValue::Str(v) });
        }
        doc.stored.push(StoredField { field: ID, value: OwnedStoredValue::Str(i.to_string()) });
        shard.index(IndexOp::new(doc)).unwrap();
        if i % 17 == 16 {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let mut ids: HashMap<u32, String> = HashMap::new();
    for seg in s.segments() {
        for d in 0..seg.max_doc() {
            let id = seg.reader.stored().document(d).unwrap().into_iter().find_map(|(f, v)| match (f, v) {
                (ID, OwnedStoredValue::Str(x)) => Some(x),
                _ => None,
            });
            ids.insert(seg.base + d, id.unwrap());
        }
    }
    let mm = |q: &str, gaps: i32, ordered: bool| {
        let mut toks = Vec::new();
        a.analyze(q, &mut |t| toks.push((t.term.as_bytes().to_vec(), t.position_increment, t.position_length)));
        I::analyzed(&toks, gaps, ordered).unwrap()
    };
    let uf = |s: I| I::fix_field(T, s);
    // `QUERIES` of the generator, in order.
    let cases: Vec<I> = vec![
        I::unordered(vec![mm("quick", -1, false), uf(mm("fox", -1, false))]),
        I::ordered(vec![mm("brown", -1, false), uf(mm("dog", -1, false))]),
        I::or(vec![mm("lazy cat", -1, true), uf(mm("red fox", 1, false))]),
        uf(mm("quick fox", -1, false)),
        uf(mm("the dog", -1, true)),
        I::unordered(vec![mm("fox dog", 2, false), uf(mm("jumps", -1, false))]),
        I::or(vec![
            I::multi_term(MultiTermQuery::prefix(M, b"laz".to_vec()), 128, "laz*"),
            uf(I::multi_term(MultiTermQuery::prefix(T, b"re".to_vec()), 128, "re*")),
        ]),
        I::not_containing(
            I::unordered(vec![mm("cat", -1, false), uf(I::multi_term(MultiTermQuery::wildcard(T, "j*s", false).unwrap(), 128, "j*s"))]),
            mm("over", -1, false),
        ),
    ];
    let fcases = fx["cases"].as_array().unwrap();
    assert_eq!(cases.len(), fcases.len());
    for (src, c) in cases.into_iter().zip(fcases) {
        let what = c["query"].to_string();
        let q: QueryRef = Arc::new(SourceConfirmedQuery::new(Arc::new(IntervalQuery::new(M, src)), M, a.clone(), Arc::new(Stored)));
        let mut col = TopScoreCollector::new(100, u64::MAX);
        s.search(&q, &mut col).unwrap();
        let td = col.top_docs();
        assert_eq!(td.total_hits, c["total"].as_u64().unwrap(), "{what}");
        let mut got: Vec<(String, f32)> = td.hits.iter().map(|h| (ids[&h.doc].clone(), h.score)).collect();
        let mut want: Vec<(String, f32)> =
            c["hits"].as_array().unwrap().iter().map(|h| (h["id"].as_str().unwrap().to_string(), h["score"].as_f64().unwrap() as f32)).collect();
        // Equal scores tie by doc id, which differs between the two indices' segment layouts.
        let key = |x: &(String, f32)| (std::cmp::Reverse(x.1.to_bits()), x.0.parse::<u32>().unwrap());
        got.sort_by_key(key);
        want.sort_by_key(key);
        assert_eq!(got, want, "{what}");
        for h in c["hits"].as_array().unwrap() {
            let d = *ids.iter().find(|(_, v)| v.as_str() == h["id"].as_str().unwrap()).unwrap().0;
            let e = s.explain(&q, d).unwrap();
            let es = &h["explanation"];
            assert!(same(&e, es), "{what}: {}\nElasticsearch {es}", e.to_text());
            for (o, x) in e.details.iter().zip(es["details"].as_array().unwrap()) {
                assert!(same(o, x), "{what}: detail {}\nElasticsearch {x}", o.to_text());
            }
        }
    }
}

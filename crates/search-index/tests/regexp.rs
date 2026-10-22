//! `regexp` queries against Elasticsearch 9.5.5: Lucene's regular expression syntax with every
//! flag combination Elasticsearch exposes, `case_insensitive`, numeric intervals, complements,
//! and the parse errors' reasons. Fixture: `tests/fixtures/gen_regexp.py`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use search_codec::IndexOptions;
use search_codec::automaton::regexp;
use search_index::collector::TopScoreCollector;
use search_index::doc::{IndexableDoc, IndexableField, StoredField};
use search_index::query::MultiTermQuery;
use search_index::schema::FieldSchema;
use search_index::{IndexOp, QueryRef, Schema, Shard, ShardConfig};
use serde_json::Value;

const K: u32 = 10;
const ID: u32 = 11;

fn flags(s: &str) -> u32 {
    s.split('|')
        .map(|f| match f {
            "ALL" => regexp::ALL,
            "NONE" => regexp::NONE,
            "INTERSECTION" => regexp::INTERSECTION,
            "COMPLEMENT" => regexp::COMPLEMENT,
            "EMPTY" => regexp::EMPTY,
            "ANYSTRING" => regexp::ANYSTRING,
            "INTERVAL" => regexp::INTERVAL,
            other => panic!("flag {other}"),
        })
        .fold(0, |a, b| a | b)
}

#[test]
fn regexps_match_elasticsearch() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/regexp.json")).unwrap();
    let schema = Schema::new().with_field(FieldSchema::new(K, "k").indexed(IndexOptions::Docs, false)).unwrap();
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    for (i, v) in fx["values"].as_array().unwrap().iter().enumerate() {
        let mut d = IndexableDoc::new(i.to_string().into_bytes());
        d.fields.push(IndexableField::Term { field: K, term: v.as_str().unwrap().as_bytes().to_vec().into() });
        d.stored.push(StoredField { field: ID, value: search_codec::stored::OwnedStoredValue::Long(i as i64) });
        shard.index(IndexOp::new(d)).unwrap();
        if i % 97 == 96 {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let mut ids: HashMap<u32, i64> = HashMap::new();
    for seg in s.segments() {
        for d in 0..seg.reader.max_doc() {
            for (f, v) in seg.reader.stored().document(d).unwrap() {
                if f == ID
                    && let search_codec::stored::OwnedStoredValue::Long(x) = v
                {
                    ids.insert(seg.base + d, x);
                }
            }
        }
    }
    let mut failures = Vec::new();
    for c in fx["cases"].as_array().unwrap() {
        let pattern = c["pattern"].as_str().unwrap();
        let fl = c["flags"].as_str().unwrap();
        let ci = c["case_insensitive"].as_bool().unwrap();
        let what = format!("/{pattern}/ flags={fl} ci={ci}");
        let q = MultiTermQuery::regexp(K, pattern, flags(fl), ci, 10_000);
        match (&c["error"], q) {
            (Value::Object(e), Err(err)) => {
                let reason = err.to_string();
                if !reason.contains(e["reason"].as_str().unwrap()) {
                    failures.push(format!("{what}: error {reason:?}, Elasticsearch {:?}", e["reason"]));
                }
            }
            (Value::Object(e), Ok(_)) => failures.push(format!("{what}: accepted, Elasticsearch fails with {:?}", e["reason"])),
            (_, Err(err)) => failures.push(format!("{what}: {err}")),
            (_, Ok(q)) => {
                let q: QueryRef = Arc::new(q);
                let mut col = TopScoreCollector::new(10_000, u64::MAX);
                s.search(&q, &mut col).unwrap();
                let mut got: Vec<i64> = col.top_docs().hits.iter().map(|h| ids[&h.doc]).collect();
                got.sort();
                let want: Vec<i64> = c["ids"].as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
                if got != want {
                    failures.push(format!("{what}: ids {got:?}, Elasticsearch {want:?}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), fx["cases"].as_array().unwrap().len(), failures.join("\n"));
}

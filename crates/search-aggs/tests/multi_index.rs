//! Searches over several indices whose mappings differ (`tests/fixtures/aggs-mi2.json`,
//! captured from Elasticsearch 9.5.5 by `gen_fixtures.py mi2`): a field mapped in one index is
//! unmapped or of another type in the other, so the reduce has to choose its leader, formats and
//! result types exactly as Elasticsearch's `canLeadReduction` / `getReducer` do. Shards reduce in
//! index-name order, as Elasticsearch orders them.

mod common;

use common::{Lookup, TestIndex, diff, top_query};
use search_aggs::Json;

#[test]
fn reduce_across_mappings() {
    let path = format!("{}/tests/fixtures/aggs-mi2.json", env!("CARGO_MANIFEST_DIR"));
    let fx = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut indices = Vec::new();
    for spec in fx.get("indices").unwrap().as_array().unwrap() {
        let mut index = TestIndex::from_mapping(spec.get("mapping").unwrap());
        index.name = spec.get("name").and_then(Json::as_str).unwrap().to_string();
        let docs: Vec<(String, Json)> =
            spec.get("docs").unwrap().as_array().unwrap().iter().enumerate().map(|(i, d)| (i.to_string(), d.clone())).collect();
        let shard = index.shard(&docs, 1);
        indices.push((index, shard));
    }
    let mut failures = Vec::new();
    for case in fx.get("cases").unwrap().as_array().unwrap() {
        let name = case.get("name").and_then(Json::as_str).unwrap();
        let body_text = case.get("body").and_then(Json::as_str).unwrap();
        let typed = case.get("typed_keys").and_then(Json::as_bool).unwrap_or(false);
        let body = Json::parse(body_text).unwrap();
        let req = search_aggs::parse(body.get("aggs").unwrap(), &search_aggs::ParseOptions::default()).unwrap();
        let mut parts = Vec::new();
        for (index, shard) in &indices {
            let lookup = Lookup { index, now: 1_700_000_000_000, scripts: None };
            let q = top_query(&lookup, &body);
            parts.push(req.run(&lookup, &shard.searcher(), &q, &search_aggs::ShardOptions::default()).unwrap());
        }
        let got = search_aggs::reduce(parts, &req, &search_aggs::ReduceOptions::default()).unwrap().to_json(typed);
        let want = case.get("response").expect("a response");
        if let Some(d) = diff(&got, want, "aggregations") {
            failures.push(format!("[{name}] {d}\n  got: {got}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

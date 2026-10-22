//! `profile: true`: the per-shard aggregation profile tree (types, descriptions, timed phases
//! with counts, debug maps, children) as Elasticsearch's `AggregationProfileShardResult`
//! renders it.

mod common;

use common::{Lookup, TestIndex, top_query};
use search_aggs::Json;

#[test]
fn profile_tree() {
    let mapping = Json::parse(r#"{"properties":{"k":{"type":"keyword"},"n":{"type":"long"},"d":{"type":"double"},"t":{"type":"date"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let docs: Vec<(String, Json)> = (0..300)
        .map(|i| (i.to_string(), Json::parse(&format!(r#"{{"k":"k{}","n":{},"d":{}.5,"t":{}}}"#, i % 40, i % 7, i, 1_600_000_000_000i64 + i * 3_600_000)).unwrap()))
        .collect();
    let shard = index.shard(&docs, 3);
    let searcher = shard.searcher();
    let lookup = Lookup { index: &index, now: 0, scripts: None };
    let body = Json::obj();
    let query = top_query(&lookup, &body);
    let aggs = r#"{"str_terms":{"terms":{"field":"k","size":2},"aggs":{"max_number":{"max":{"field":"n"}}}},
                   "c":{"cardinality":{"field":"d"}},"cs":{"cardinality":{"field":"k"}},"cd":{"cardinality":{"field":"k","execution_hint":"direct"}},
                   "h":{"date_histogram":{"field":"t","calendar_interval":"day"}},"n_terms":{"terms":{"field":"n"}}}"#;
    let req = search_aggs::parse(&Json::parse(aggs).unwrap(), &search_aggs::ParseOptions::default()).unwrap();
    let profiler = search_aggs::profile::Profiler::new();
    let opts = search_aggs::ShardOptions { profiler: Some(profiler.clone()), ..Default::default() };
    req.run(&lookup, &searcher, &query, &opts).unwrap();
    let p = profiler.to_json();
    let list = p.as_array().unwrap();
    assert_eq!(list.len(), 6);
    let by = |name: &str| list.iter().find(|n| n.get("description").and_then(Json::as_str) == Some(name)).unwrap().clone();
    let terms = by("str_terms");
    assert_eq!(terms.get("type").and_then(Json::as_str), Some("GlobalOrdinalsStringTermsAggregator"));
    let b = terms.get("breakdown").unwrap();
    for phase in ["initialize", "build_leaf_collector", "collect", "post_collection", "build_aggregation"] {
        assert!(b.get(phase).and_then(Json::as_i64).unwrap() > 0, "{phase}");
    }
    assert_eq!(b.get("collect_count").and_then(Json::as_i64), Some(300));
    assert_eq!(b.get("build_leaf_collector_count").and_then(Json::as_i64), Some(3));
    let debug = terms.get("debug").unwrap();
    assert_eq!(debug.get("result_strategy").and_then(Json::as_str), Some("terms"));
    assert_eq!(debug.get("has_filter"), Some(&Json::Bool(false)));
    assert_eq!(debug.get("total_buckets").and_then(Json::as_i64), Some(40));
    // 40 terms > shard_size 13: breadth-first, the max deferred.
    assert_eq!(debug.get("deferred_aggregators"), Some(&Json::Arr(vec![Json::Str("max_number".into())])));
    let child = &terms.get("children").unwrap().as_array().unwrap()[0];
    assert_eq!(child.get("type").and_then(Json::as_str), Some("MaxAggregator"));
    assert_eq!(child.get("description").and_then(Json::as_str), Some("max_number"));
    // Replayed for the kept buckets only.
    let kept = child.get("breakdown").unwrap().get("collect_count").and_then(Json::as_i64).unwrap();
    assert!(kept > 0 && kept < 300, "{kept}");
    let c = by("c");
    assert_eq!(c.get("type").and_then(Json::as_str), Some("CardinalityAggregator"));
    assert_eq!(c.get("debug").unwrap().get("numeric_collectors_used").and_then(Json::as_i64), Some(3));
    assert_eq!(by("cs").get("type").and_then(Json::as_str), Some("GlobalOrdCardinalityAggregator"));
    let cd = by("cd");
    assert_eq!(cd.get("type").and_then(Json::as_str), Some("CardinalityAggregator"));
    assert_eq!(cd.get("debug").unwrap().get("string_hashing_collectors_used").and_then(Json::as_i64), Some(3));
    assert_eq!(by("h").get("type").and_then(Json::as_str), Some("DateHistogramAggregator"));
    let n = by("n_terms");
    assert_eq!(n.get("type").and_then(Json::as_str), Some("NumericTermsAggregator"));
    assert_eq!(n.get("debug").unwrap().get("result_strategy").and_then(Json::as_str), Some("long_terms"));
}

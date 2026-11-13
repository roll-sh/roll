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

/// Filter-by-filter execution (`search.aggs.rewrite_to_filter_by_filter`): a top-level
/// low-cardinality keyword `terms` runs as `StringTermsAggregatorFromFilters` and a top-level
/// `filters` as `FilterByFilterAggregator`, with the same results as the native aggregators,
/// no docs collected by the aggregator itself, and sub-aggregations fed filter by filter
/// (Elasticsearch 9.5.5's profile shape).
#[test]
fn filter_by_filter() {
    let mapping = Json::parse(r#"{"properties":{"str":{"type":"keyword"},"number":{"type":"long"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let docs: Vec<(String, Json)> =
        (0..90).map(|i| (i.to_string(), Json::parse(&format!(r#"{{"str":"{}","number":{}}}"#, ["sheep", "cow", "pig"][i % 3], i)).unwrap())).collect();
    let shard = index.shard(&docs, 3);
    let searcher = shard.searcher();
    let lookup = Lookup { index: &index, now: 0, scripts: None };
    let query = top_query(&lookup, &Json::obj());
    let aggs = r#"{"t":{"terms":{"field":"str"},"aggs":{"m":{"max":{"field":"number"}}}},"t2":{"terms":{"field":"str","min_doc_count":0}},
                   "f":{"filters":{"filters":{"a":{"term":{"str":"cow"}},"b":{"term":{"str":"pig"}}}}},
                   "fo":{"filters":{"other_bucket":true,"filters":{"a":{"term":{"str":"cow"}}}}}}"#;
    let req = search_aggs::parse(&Json::parse(aggs).unwrap(), &search_aggs::ParseOptions::default()).unwrap();
    let run = |fbf: bool| {
        let profiler = search_aggs::profile::Profiler::new();
        let opts = search_aggs::ShardOptions { profiler: Some(profiler.clone()), filter_by_filter: fbf, matches_all: true, ..Default::default() };
        let out = req.run(&lookup, &searcher, &query, &opts).unwrap();
        let reduced = search_aggs::reduce(vec![out], &req, &search_aggs::ReduceOptions::default()).unwrap();
        (reduced.to_json(false), profiler.to_json())
    };
    let (native, native_profile) = run(false);
    let (fbf, p) = run(true);
    assert_eq!(native.to_string(), fbf.to_string());
    let types = |p: &Json| p.as_array().unwrap().iter().map(|n| n.get("type").and_then(Json::as_str).unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(types(&native_profile), ["GlobalOrdinalsStringTermsAggregator", "GlobalOrdinalsStringTermsAggregator", "FiltersAggregator.Compatible", "FiltersAggregator.Compatible"]);
    assert_eq!(types(&p), ["StringTermsAggregatorFromFilters", "StringTermsAggregatorFromFilters", "FilterByFilterAggregator", "FiltersAggregator.Compatible"]);
    let list = p.as_array().unwrap();
    let t = &list[0];
    assert_eq!(t.get("breakdown").unwrap().get("collect_count").and_then(Json::as_i64), Some(0));
    let debug = t.get("debug").unwrap();
    assert_eq!(debug.get("delegate").and_then(Json::as_str), Some("FilterByFilterAggregator"));
    let dd = debug.get("delegate_debug").unwrap();
    assert_eq!(dd.get("segments_collected").and_then(Json::as_i64), Some(3));
    assert_eq!(dd.get("segments_counted").and_then(Json::as_i64), Some(0));
    let filters = dd.get("filters").unwrap().as_array().unwrap();
    assert_eq!(filters.len(), 3);
    // One term filter per global ordinal, in term order (the test lookup builds doc-values
    // term queries, so only the term is checked; search-es prints `str:cow`).
    assert!(filters[0].get("query").and_then(Json::as_str).unwrap().contains("cow"));
    assert!(filters[2].get("query").and_then(Json::as_str).unwrap().contains("sheep"));
    // The max under it sees every doc once, one leaf per filter and segment.
    let child = &t.get("children").unwrap().as_array().unwrap()[0];
    assert_eq!(child.get("breakdown").unwrap().get("collect_count").and_then(Json::as_i64), Some(90));
    assert_eq!(child.get("breakdown").unwrap().get("build_leaf_collector_count").and_then(Json::as_i64), Some(9));
    let f = &list[2];
    assert_eq!(f.get("breakdown").unwrap().get("collect_count").and_then(Json::as_i64), Some(0));
    assert_eq!(f.get("debug").unwrap().get("segments_counted").and_then(Json::as_i64), Some(3));
    // The other bucket keeps the compatible implementation, which collects the docs.
    assert_eq!(list[3].get("breakdown").unwrap().get("collect_count").and_then(Json::as_i64), Some(90));
}

/// The opensearch dialect's profile (OpenSearch 3.9's `collectDebugInfo` maps): no
/// `built_buckets`, no `debug` when an aggregator has nothing to say, OpenSearch's aggregator
/// classes and its terms, cardinality and date histogram collector entries — the values
/// OpenSearch 3.9.0 printed for this data (its 20_terms and 170_cardinality_metric YAML
/// tests). The test index has no points, so the filter rewrite counts (which need them) are
/// covered by `filter_rewrite`'s unit tests and the `os_compat` REST fixture instead.
#[test]
fn opensearch_profile() {
    let mapping = Json::parse(r#"{"properties":{"str":{"type":"keyword"},"number":{"type":"integer"},"double":{"type":"double"},"date":{"type":"date"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    // 2016-01-01, 2016-01-02, 2016-02-01 and 2016-03-01 (UTC).
    let rows = [(1_451_606_400_000i64, "sheep", 1, 42.0), (1_451_692_800_000, "sheep", 3, 100.0), (1_454_284_800_000, "cow", 1, 50.0), (1_456_790_400_000, "pig", 1, 7.0)];
    let docs: Vec<(String, Json)> = rows
        .iter()
        .enumerate()
        .map(|(i, (d, s, n, x))| (i.to_string(), Json::parse(&format!(r#"{{"date":{d},"str":"{s}","number":{n},"double":{x}}}"#)).unwrap()))
        .collect();
    let shard = index.shard(&docs, 1);
    let searcher = shard.searcher();
    let lookup = Lookup { index: &index, now: 0, scripts: None };
    let query = top_query(&lookup, &Json::obj());
    let aggs = r#"{"h":{"date_histogram":{"field":"date","calendar_interval":"month"}},
                   "hs":{"date_histogram":{"field":"date","calendar_interval":"month"},"aggs":{"m":{"max":{"field":"number"}}}},
                   "r":{"range":{"field":"double","ranges":[{"to":50},{"from":50,"to":150},{"from":150}]}},
                   "c":{"composite":{"sources":[{"d":{"date_histogram":{"field":"date","calendar_interval":"1d"}}}]}},
                   "a":{"auto_date_histogram":{"field":"date","buckets":2}},
                   "card":{"cardinality":{"field":"str"}},
                   "t":{"terms":{"field":"str","collect_mode":"breadth_first","execution_hint":"global_ordinals"},"aggs":{"mx":{"max":{"field":"number"}}}}}"#;
    let req = search_aggs::parse(&Json::parse(aggs).unwrap(), &search_aggs::ParseOptions::default()).unwrap();
    let profiler = search_aggs::profile::Profiler::new();
    let opts = search_aggs::ShardOptions { profiler: Some(profiler.clone()), ..Default::default() };
    search_aggs::dialect::with(true, || req.run(&lookup, &searcher, &query, &opts)).unwrap();
    let p = profiler.to_json();
    let list = p.as_array().unwrap();
    let by = |name: &str| list.iter().find(|n| n.get("description").and_then(Json::as_str) == Some(name)).unwrap().clone();
    let num = |n: &Json, k: &str| n.get("debug").and_then(|d| d.get(k)).and_then(Json::as_i64);
    for name in ["h", "hs", "a", "card", "t"] {
        assert!(by(name).get("debug").unwrap().get("built_buckets").is_none(), "{name}");
    }
    assert_eq!(num(&by("h"), "total_buckets"), Some(3));
    // A sub-aggregation on a small segment: collected the usual way.
    let hs = by("hs");
    assert_eq!(num(&hs, "optimized_segments"), None);
    assert_eq!(num(&hs, "single_valued_collectors_used"), Some(1));
    let max = &hs.get("children").unwrap().as_array().unwrap()[0];
    assert!(max.get("debug").is_none());
    assert_eq!(by("r").get("type").and_then(Json::as_str), Some("RangeAggregator"));
    assert_eq!(by("c").get("type").and_then(Json::as_str), Some("CompositeAggregator"));
    assert_eq!(num(&by("a"), "skiplist_collectors_used"), Some(0));
    let card = by("card");
    assert_eq!(card.get("type").and_then(Json::as_str), Some("CardinalityAggregator"));
    assert_eq!(num(&card, "hybrid_collectors_used"), Some(1));
    assert_eq!(num(&card, "ordinals_collectors_used"), Some(0));
    assert_eq!(num(&card, "string_hashing_collectors_used"), Some(0));
    let t = by("t");
    assert_eq!(t.get("debug").unwrap().get("collection_strategy").and_then(Json::as_str), Some("dense"));
    assert_eq!(t.get("debug").unwrap().get("result_selection_strategy").and_then(Json::as_str), Some("select_all"));
    assert!(t.get("debug").unwrap().get("total_buckets").is_none());
}

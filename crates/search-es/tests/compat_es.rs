//! Behaviour checked against Elasticsearch 9.5.5 by hand this round (the shared reference
//! cluster was read-only, past its disk flood stage, so these are assertions rather than
//! captured fixtures): `knn` lookup vector builders inside the query DSL, `exists` on an empty
//! `sparse_vector` object, scrolls whose shards the can-match phase skips, `_slice` routing,
//! the `@timestamp` segment order, cross-cluster search over a loopback remote, and the
//! `_msearch` request breaker. Round 2 adds percolator candidate pre-selection and scripts in
//! aggregations. (`rrf`, `linear` and `pinned` retrievers are pinned by the `rank-rrf` and
//! `search-business-rules` YAML suites.)

use search_es::json::{Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::Arc;

async fn cluster() -> Cluster {
    Cluster::open(ClusterConfig { unsealed_secrets: true, ..ClusterConfig::new("compat-es") }, Arc::new(MemoryStore::new())).await.unwrap()
}

async fn call(c: &Cluster, method: &str, uri: &str, body: Option<&str>) -> (u16, Value) {
    let mut r = EsRequest::new(method, uri);
    if let Some(b) = body {
        let ct = if uri.contains("_bulk") || uri.contains("_msearch") { "application/x-ndjson" } else { "application/json" };
        r.headers.push(("content-type".into(), ct.into()));
        r.body = b.to_string().into();
    }
    let resp = search_es::handle(c, r).await;
    let v = parse(&resp.body).ok().flatten().unwrap_or(Value::Str(String::from_utf8_lossy(&resp.body).into_owned()));
    (resp.status, v)
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    let mut cur = v;
    for p in path.split('.') {
        cur = match cur {
            Value::Arr(a) => p.parse::<usize>().ok().and_then(|i| a.get(i)).unwrap_or(&Value::Null),
            other => other.get(p).unwrap_or(&Value::Null),
        };
    }
    cur
}

fn ids(v: &Value) -> Vec<String> {
    at(v, "hits.hits").as_array().map(|a| a.iter().map(|h| at(h, "_id").to_es_string()).collect()).unwrap_or_default()
}

#[tokio::test]
async fn knn_query_lookup_builder_and_sparse_exists() {
    let c = cluster().await;
    call(&c, "PUT", "/v", Some(r#"{"mappings":{"properties":{"vec":{"type":"dense_vector","dims":2,"similarity":"l2_norm"},"sp":{"type":"sparse_vector"}}}}"#)).await;
    call(&c, "PUT", "/v/_doc/1?refresh=true", Some(r#"{"vec":[1,2],"sp":{"a":1.5}}"#)).await;
    call(&c, "PUT", "/v/_doc/2?refresh=true", Some(r#"{"vec":[3,4],"sp":{}}"#)).await;
    call(&c, "PUT", "/v/_doc/3?refresh=true", Some(r#"{"vec":[0,1],"sp":[]}"#)).await;
    // The lookup resolves on the coordinator, also nested in other queries.
    let (s, v) = call(&c, "POST", "/v/_search", Some(r#"{"_source":false,"query":{"knn":{"field":"vec","k":3,"query_vector_builder":{"lookup":{"index":"v","id":"1","path":"vec"}}}}}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(ids(&v), ["1", "3", "2"]);
    assert_eq!(at(&v, "hits.hits.0._score").as_f64(), Some(1.0));
    let (s, v) = call(&c, "POST", "/v/_search", Some(r#"{"_source":false,"query":{"bool":{"filter":[{"knn":{"field":"vec","k":1,"query_vector_builder":{"lookup":{"index":"v","id":"3","path":"vec"}}}}]}}}"#)).await;
    assert_eq!((s, ids(&v)), (200, vec!["3".to_string()]));
    let (s, v) = call(&c, "POST", "/v/_search", Some(r#"{"query":{"knn":{"field":"vec","k":3,"query_vector_builder":{"lookup":{"index":"v","id":"9","path":"vec"}}}}}"#)).await;
    assert_eq!(s, 404);
    assert_eq!(at(&v, "error.reason").as_str(), Some("Document with id [9] not found in index [v] for lookup query vector builder."));
    // An empty object makes the field exist; an empty array doesn't.
    let (_, v) = call(&c, "POST", "/v/_search", Some(r#"{"_source":false,"sort":["_doc"],"query":{"exists":{"field":"sp"}}}"#)).await;
    assert_eq!(ids(&v), ["1", "2"]);
}

#[tokio::test]
async fn scroll_skipped_by_can_match() {
    let c = cluster().await;
    call(&c, "PUT", "/s", Some(r#"{"settings":{"number_of_shards":3},"mappings":{"properties":{"d":{"type":"date","format":"yyyy-MM-dd"}}}}"#)).await;
    let bulk = (1..=3).map(|i| format!("{{\"index\":{{\"_index\":\"s\",\"_id\":\"{i}\"}}}}\n{{\"d\":\"2020-01-0{i}\"}}\n")).collect::<String>();
    call(&c, "POST", "/_bulk?refresh=true", Some(&bulk)).await;
    let (s, v) = call(&c, "POST", "/s/_search?scroll=1m&pre_filter_shard_size=1", Some(r#"{"size":1,"query":{"range":{"d":{"gte":"2030-01-01","lte":"2030-01-02"}}}}"#)).await;
    assert_eq!(s, 200);
    assert_eq!((at(&v, "_shards.total").as_i64(), at(&v, "_shards.skipped").as_i64(), at(&v, "hits.max_score").as_f64()), (Some(3), Some(3), Some(0.0)));
    let id = at(&v, "_scroll_id").as_str().unwrap().to_string();
    let (s, v) = call(&c, "POST", "/_search/scroll", Some(&format!(r#"{{"scroll":"1m","scroll_id":"{id}"}}"#))).await;
    assert_eq!(s, 200);
    assert_eq!(at(&v, "num_reduce_phases").as_i64(), Some(0));
    assert_eq!((at(&v, "_shards.total").as_i64(), at(&v, "hits.max_score")), (Some(0), &Value::Null));
    // Partly skipped: only the matching shard keeps a context.
    let (_, v) = call(&c, "POST", "/s/_search?scroll=1m&pre_filter_shard_size=1", Some(r#"{"size":1,"query":{"range":{"d":{"gte":"2020-01-02","lte":"2020-01-02"}}}}"#)).await;
    let skipped = at(&v, "_shards.skipped").as_i64().unwrap();
    assert_eq!((at(&v, "_shards.total").as_i64(), ids(&v)), (Some(3), vec!["2".to_string()]));
    assert!((1..3).contains(&skipped), "{skipped}");
    let id = at(&v, "_scroll_id").as_str().unwrap().to_string();
    let (_, v) = call(&c, "POST", "/_search/scroll", Some(&format!(r#"{{"scroll":"1m","scroll_id":"{id}"}}"#))).await;
    assert_eq!(at(&v, "_shards.total").as_i64(), Some(3 - skipped));
}

#[tokio::test]
async fn slice_routing_and_timestamp_segment_order() {
    let c = cluster().await;
    // `_slice` is `routing` under another name, never both.
    let (s, _) = call(&c, "PUT", "/t/_doc/1?_slice=abc&refresh=true", Some(r#"{"@timestamp":"2021-08-01"}"#)).await;
    assert_eq!(s, 201);
    let (_, v) = call(&c, "GET", "/t/_doc/1?_slice=abc", None).await;
    assert_eq!(at(&v, "_routing").as_str(), Some("abc"));
    let (s, v) = call(&c, "GET", "/t/_doc/1?_slice=abc&routing=abc", None).await;
    assert_eq!((s, at(&v, "error.reason").as_str()), (400, Some("[routing] is not allowed together with [_slice]")));
    let (_, v) = call(&c, "GET", "/t/_doc/1?_slice=a%20b", None).await;
    assert_eq!(at(&v, "error.reason").as_str(), Some("invalid [_slice] value [a b]: only [a-zA-Z0-9._:-] are allowed and max length is [128]"));
    // `@timestamp` added dynamically: segments stay in order until the index opens again.
    call(&c, "PUT", "/t/_doc/2?_slice=abc&refresh=true", Some(r#"{"@timestamp":"2021-08-02"}"#)).await;
    let (_, v) = call(&c, "POST", "/t/_search", None).await;
    assert_eq!(ids(&v), ["1", "2"]);
    call(&c, "POST", "/t/_close", None).await;
    call(&c, "POST", "/t/_open", None).await;
    let (_, v) = call(&c, "POST", "/t/_search", None).await;
    assert_eq!(ids(&v), ["2", "1"]);
}

#[tokio::test]
async fn loopback_cross_cluster_search() {
    let c = cluster().await;
    let me = c.node.transport_address.clone();
    let settings = format!(r#"{{"persistent":{{"cluster":{{"remote":{{"one":{{"seeds":["{me}"]}},"far":{{"seeds":["10.0.0.1:9300"],"skip_unavailable":false}}}}}}}}}}"#);
    call(&c, "PUT", "/_cluster/settings", Some(&settings)).await;
    call(&c, "PUT", "/test/_doc/1?refresh=true", Some(r#"{"foo":"bar"}"#)).await;
    let (s, v) = call(&c, "POST", "/test,one:test/_search", Some(r#"{"query":{"match":{"foo":"bar"}}}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    let mut indices: Vec<String> = at(&v, "hits.hits").as_array().unwrap().iter().map(|h| at(h, "_index").to_es_string()).collect();
    indices.sort();
    assert_eq!(indices, ["one:test", "test"]);
    assert_eq!((at(&v, "_clusters.total").as_i64(), at(&v, "_clusters.details.one.status").as_str()), (Some(2), Some("successful")));
    let (s, v) = call(&c, "POST", "/far:test/_search", None).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (500, Some("connect_transport_exception")));
    let (s, v) = call(&c, "POST", "/nope:test/_search", None).await;
    assert_eq!((s, at(&v, "error.reason").as_str()), (404, Some("no such remote cluster: [nope]")));
    let (_, v) = call(&c, "GET", "/_remote/info", None).await;
    assert_eq!((at(&v, "one.connected"), at(&v, "far.connected")), (&Value::Bool(true), &Value::Bool(false)));
    let (_, v) = call(&c, "GET", "/_cluster/stats?include_remotes=true", None).await;
    assert_eq!(at(&v, "ccs._search.total").as_i64(), Some(1));
    assert_eq!(at(&v, "ccs.clusters.one.mode").as_str(), Some("sniff"));
    let (_, v) = call(&c, "GET", "/_cluster/stats", None).await;
    assert!(at(&v, "ccs.clusters").is_null());
}

#[tokio::test]
async fn msearch_request_breaker() {
    let c = cluster().await;
    let bulk = (1..=80).map(|i| format!("{{\"index\":{{\"_index\":\"m\",\"_id\":\"{i}\"}}}}\n{{}}\n")).collect::<String>();
    call(&c, "POST", "/_bulk?refresh=true", Some(&bulk)).await;
    call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"indices.breaker.request.limit":"63000b"}}"#)).await;
    let body = "{\"index\":\"m\"}\n{\"size\":80}\n{\"index\":\"m\"}\n{\"size\":80}\n";
    let (s, v) = call(&c, "POST", "/_msearch?max_concurrent_searches=1", Some(body)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (429, Some("circuit_breaking_exception")));
    assert!(at(&v, "error.reason").as_str().unwrap().starts_with("[request] Data too large, data for [<msearch_response>] would be ["));
    // Released afterwards: one search fits.
    let (s, _) = call(&c, "POST", "/_msearch", Some("{\"index\":\"m\"}\n{\"size\":80}\n")).await;
    assert_eq!(s, 200);
    let (_, v) = call(&c, "GET", "/_nodes/stats/breaker", None).await;
    let node = at(&v, "nodes").as_object().unwrap().values().next().unwrap().clone();
    assert_eq!(at(&node, "breakers.request.limit_size_in_bytes").as_i64(), Some(63000));
    assert_eq!(at(&node, "breakers.request.estimated_size_in_bytes").as_i64(), Some(0));
    assert_eq!(at(&node, "breakers.request.tripped").as_i64(), Some(1));
}

/// Percolation verifies only the stored queries whose extracted terms the documents contain
/// (plus those without extractable terms); the results must equal checking every query.
#[tokio::test]
async fn percolate_candidate_preselection() {
    let c = cluster().await;
    let (s, v) = call(&c, "PUT", "/p", Some(r#"{"settings":{"number_of_shards":2},"mappings":{"properties":{"q":{"type":"percolator"},"msg":{"type":"text"},"title":{"type":"text"},"tag":{"type":"keyword"},"num":{"type":"long"}}}}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    let queries = [
        (1, r#"{"term":{"msg":"fox"}}"#),
        (2, r#"{"match":{"msg":"quick brown"}}"#),
        (3, r#"{"match_phrase":{"msg":"lazy dog"}}"#),
        (4, r#"{"bool":{"must":[{"term":{"tag":"a"}}],"must_not":[{"term":{"msg":"cat"}}]}}"#),
        (5, r#"{"range":{"num":{"gte":10}}}"#),
        (6, r#"{"prefix":{"msg":"jum"}}"#),
        (7, r#"{"terms":{"tag":["b","c"]}}"#),
        (8, r#"{"match":{"msg":{"query":"quick cat","operator":"and"}}}"#),
        (9, r#"{"bool":{"should":[{"term":{"msg":"zebra"}},{"range":{"num":{"lt":0}}}]}}"#),
        (10, r#"{"match_none":{}}"#),
        (11, r#"{"query_string":{"query":"msg:(elephant OR fox)"}}"#),
        (12, r#"{"multi_match":{"query":"dog","fields":["msg","title"]}}"#),
        (13, r#"{"bool":{"filter":[{"term":{"tag":"a"}},{"match":{"msg":"elephant"}}]}}"#),
        (14, r#"{"match":{"title":"x"}}"#),
        (15, r#"{"constant_score":{"filter":{"match_phrase_prefix":{"msg":"brown fo"}},"boost":2}}"#),
        (16, r#"{"dis_max":{"queries":[{"term":{"title":"zebra"}},{"match":{"msg":"over"}}]}}"#),
    ];
    let mut bulk = String::new();
    for (id, q) in queries {
        bulk.push_str(&format!("{{\"index\":{{\"_index\":\"p\",\"_id\":\"{id}\"}}}}\n{{\"q\":{q}}}\n"));
    }
    let (s, v) = call(&c, "POST", "/_bulk?refresh=true", Some(&bulk)).await;
    assert_eq!((s, at(&v, "errors").as_bool()), (200, Some(false)), "{v:?}");
    let percolate = |doc: &'static str| {
        let c = &c;
        async move {
            let body = format!(r#"{{"size":100,"_source":false,"query":{{"percolate":{{"field":"q","document":{doc}}}}}}}"#);
            let (s, v) = call(c, "POST", "/p/_search", Some(&body)).await;
            assert_eq!(s, 200, "{v:?}");
            let mut got: Vec<u32> = ids(&v).iter().map(|i| i.parse().unwrap()).collect();
            got.sort_unstable();
            got
        }
    };
    // Mapped fields only: candidates come from the extracted terms.
    assert_eq!(percolate(r#"{"msg":"the quick brown fox jumps over the lazy dog","tag":"a","num":5}"#).await, [1, 2, 3, 4, 6, 11, 12, 15, 16]);
    assert_eq!(percolate(r#"{"msg":"a quick cat","tag":"b","num":12}"#).await, [2, 5, 7, 8]);
    // A field the mapping lacks: every stored query is verified.
    assert_eq!(percolate(r#"{"msg":"elephant","tag":"c","num":-1,"title":"x","extra":"y"}"#).await, [7, 9, 11, 14]);
    // Changing the stored queries (and the mapping) invalidates the per-segment cache.
    call(&c, "PUT", "/p/_doc/1?refresh=true", Some(r#"{"q":{"term":{"msg":"cat"}}}"#)).await;
    call(&c, "PUT", "/p/_mapping", Some(r#"{"properties":{"extra":{"type":"keyword"}}}"#)).await;
    assert_eq!(percolate(r#"{"msg":"a quick cat","tag":"b","num":12}"#).await, [1, 2, 5, 7, 8]);
    assert_eq!(percolate(r#"{"msg":"elephant","title":"x"}"#).await, [11, 14]);
}

/// Scripts in aggregations: the field API reads a `text` field from `_source`
/// (`FielddataOperation.SCRIPT`), `$()` returns a vector field's `DenseVector`, and a script
/// failure keeps its `script_stack` / `caused_by` as in other search contexts.
#[tokio::test]
async fn aggregation_scripts_field_api_and_errors() {
    let c = cluster().await;
    call(&c, "PUT", "/a", Some(r#"{"mappings":{"properties":{"text":{"type":"text"},"v":{"type":"dense_vector","dims":1,"index":true,"similarity":"l2_norm"}}}}"#)).await;
    call(&c, "PUT", "/a/_doc/1?refresh=true", Some(r#"{"text":"term 5","v":[2.5]}"#)).await;
    let body = r#"{"size":0,"aggs":{"s":{"scripted_metric":{"init_script":"state.l = []","map_script":"state.l.add(Integer.parseInt($('text', null).substring(5)) + $('v', null).getVector()[0])","combine_script":"state.l","reduce_script":"states"}}}}"#;
    let (s, v) = call(&c, "POST", "/a/_search", Some(body)).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(at(&v, "aggregations.s.value.0.0").as_f64(), Some(7.5));
    let body = r#"{"size":0,"aggs":{"s":{"scripted_metric":{"init_script":"state.l = []","map_script":"state.l.add(doc['text'].value)","combine_script":"state.l","reduce_script":"states"}}}}"#;
    let (s, v) = call(&c, "POST", "/a/_search", Some(body)).await;
    assert_eq!(s, 400);
    let root = at(&v, "error.root_cause.0");
    assert_eq!(at(root, "type").as_str(), Some("script_exception"));
    assert_eq!(at(root, "script").as_str(), Some("state.l.add(doc['text'].value)"));
    assert_eq!(at(root, "position.offset").as_i64(), Some(16));
    assert!(at(&v, "error.failed_shards.0.reason.caused_by.reason").as_str().unwrap_or("").starts_with("Fielddata is disabled"), "{v:?}");
}

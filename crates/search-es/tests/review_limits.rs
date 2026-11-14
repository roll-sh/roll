//! Behaviour the differential fixtures can't pin down: requests that must stay bounded (sizes
//! and counts come from the client; Elasticsearch itself falls over on some of them, so they
//! can't be captured), node-level statistics on a cluster of our own, and the field table under
//! concurrent mapping updates.

use search_es::json::{Value, parse};
use search_es::{Cluster, EsRequest};
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let mut r = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    if uri.contains("_bulk") {
        r.headers = vec![("content-type".into(), "application/x-ndjson".into())];
    }
    let resp = search_es::handle(c, r).await;
    (resp.status, parse(&resp.body).ok().flatten().unwrap_or(Value::Null))
}

fn at<'v>(v: &'v Value, path: &[&str]) -> &'v Value {
    static NULL: Value = Value::Null;
    path.iter().try_fold(v, |v, k| match v {
        Value::Arr(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
        other => other.get(k),
    })
    .unwrap_or(&NULL)
}

#[tokio::test]
async fn huge_slice_counts_fail_without_allocating() {
    let c = Cluster::in_memory("limits").await;
    call(&c, "PUT", "/idx/_doc/1?refresh=true", r#"{"a":1}"#).await;
    for uri in ["/idx/_update_by_query?slices=2147483647", "/idx/_delete_by_query?slices=2147483647", "/idx/_update_by_query?slices=100000"] {
        let t = Instant::now();
        let body = if uri.contains("delete") { r#"{"query":{"match_all":{}}}"# } else { "" };
        let (status, v) = call(&c, "POST", uri, body).await;
        assert_eq!(status, 400, "{uri}: {}", v.to_json());
        assert!(at(&v, &["error", "root_cause", "0", "reason"]).as_str().unwrap().starts_with("The number of slices ["), "{}", v.to_json());
        // Suppressed copies of the slice failure are bounded.
        let suppressed = at(&v, &["error", "suppressed"]).as_array().map_or(0, Vec::len);
        assert!(suppressed <= 4096, "{suppressed}");
        assert!(t.elapsed() < Duration::from_secs(20), "{uri} took {:?}", t.elapsed());
    }
    // Nothing was written or deleted, and no task was left registered.
    let (_, v) = call(&c, "GET", "/idx/_count", "").await;
    assert_eq!(at(&v, &["count"]).as_i64(), Some(1));
    let (_, v) = call(&c, "GET", "/_tasks?actions=*byquery*", "").await;
    assert_eq!(at(&v, &["nodes"]).as_object().map_or(0, |m| m.len()), 0, "{}", v.to_json());
}

#[tokio::test]
async fn huge_suggest_sizes_stay_bounded() {
    let c = Cluster::in_memory("limits").await;
    call(&c, "PUT", "/idx/_doc/1?refresh=true", r#"{"t":"hello world"}"#).await;
    let (status, v) = call(&c, "POST", "/idx/_search", r#"{"suggest":{"s":{"text":"helo","term":{"field":"t","size":2147483647,"shard_size":2147483647}}}}"#).await;
    assert_eq!(status, 200, "{}", v.to_json());
    assert_eq!(at(&v, &["suggest", "s", "0", "options", "0", "text"]).as_str(), Some("hello"));
}

#[tokio::test]
async fn node_and_cat_stats_report_documents() {
    let c = Cluster::in_memory("stats").await;
    call(&c, "PUT", "/a", r#"{"settings":{"number_of_shards":2}}"#).await;
    let mut bulk = String::new();
    for i in 0..5 {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"t\":\"doc {i}\"}}\n"));
    }
    call(&c, "POST", "/a/_bulk?refresh=true", &bulk).await;
    call(&c, "PUT", "/b/_doc/1?refresh=true", r#"{"t":"x"}"#).await;
    call(&c, "DELETE", "/a/_doc/0?refresh=true", "").await;

    let (_, v) = call(&c, "GET", "/_cat/indices/a?format=json&h=docs.count,docs.deleted,store.size,pri.store.size,indexing.index_total,segments.count&bytes=b", "").await;
    let row = at(&v, &["0"]);
    assert_eq!(at(row, &["docs.count"]).as_str(), Some("4"), "{}", v.to_json());
    assert_eq!(at(row, &["indexing.index_total"]).as_str(), Some("5"), "{}", v.to_json());
    let store: i64 = at(row, &["store.size"]).as_str().unwrap().parse().unwrap();
    assert!(store > 0, "{}", v.to_json());
    assert_eq!(at(row, &["pri.store.size"]), at(row, &["store.size"]));

    let (_, v) = call(&c, "GET", "/_nodes/stats/indices/docs,store,indexing", "").await;
    let node = at(&v, &["nodes"]).as_object().unwrap().values().next().unwrap().clone();
    assert_eq!(at(&node, &["indices", "docs", "count"]).as_i64(), Some(5), "{}", node.to_json());
    assert!(at(&node, &["indices", "store", "size_in_bytes"]).as_i64().unwrap() > 0);
    assert_eq!(at(&node, &["indices", "indexing", "index_total"]).as_i64(), Some(6));
    let (_, v) = call(&c, "GET", "/_nodes/stats/indices/docs?level=indices", "").await;
    let node = at(&v, &["nodes"]).as_object().unwrap().values().next().unwrap().clone();
    assert_eq!(at(&node, &["indices", "indices", "a", "docs", "count"]).as_i64(), Some(4), "{}", node.to_json());
    assert_eq!(at(&node, &["indices", "indices", "b", "docs", "count"]).as_i64(), Some(1));
    let (_, v) = call(&c, "GET", "/_nodes/stats/indices/docs?level=shards", "").await;
    let node = at(&v, &["nodes"]).as_object().unwrap().values().next().unwrap().clone();
    let shards = at(&node, &["indices", "shards", "a"]).as_array().unwrap();
    let per_shard: i64 = shards.iter().enumerate().map(|(i, s)| at(s, &[&i.to_string(), "docs", "count"]).as_i64().unwrap()).sum();
    assert_eq!(per_shard, 4);
}

/// A request that resolved its index before a concurrent mapping update must not roll the
/// field table back: fields mapped by the update would silently stop being indexed.
#[tokio::test]
async fn stale_metadata_never_rolls_the_field_table_back() {
    let c = Cluster::in_memory("tables").await;
    call(&c, "PUT", "/idx", r#"{"mappings":{"properties":{"base":{"type":"keyword"}}}}"#).await;
    let old_state = c.state();
    let old = old_state.metadata.indices.get("idx").unwrap().clone();
    let (status, _) = call(&c, "PUT", "/idx/_doc/1", r#"{"f1":"v"}"#).await;
    assert_eq!(status, 201);
    let engine = c.engine.loaded(&old.uuid).unwrap();
    assert!(engine.table().get("f1").is_some());
    // The stale request syncs with the old metadata…
    c.engine.index(&c, &old_state, &old).unwrap();
    // …and the table keeps the newer mapping.
    assert!(engine.table().get("f1").is_some());
    assert!(engine.table().get("f1.keyword").is_some());
    call(&c, "PUT", "/idx/_doc/2?refresh=true", r#"{"f1":"v"}"#).await;
    let (_, v) = call(&c, "GET", "/idx/_count?q=f1.keyword:v", "").await;
    assert_eq!(at(&v, &["count"]).as_i64(), Some(2), "{}", v.to_json());
}

/// Many writers adding fields at once: every document stays searchable by its new field.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_dynamic_fields_are_all_indexed() {
    let c = Arc::new(Cluster::in_memory("race").await);
    call(&c, "PUT", "/idx", r#"{"settings":{"number_of_shards":1}}"#).await;
    let n = 200;
    let mut handles = Vec::new();
    for i in 0..n {
        let c = c.clone();
        handles.push(tokio::spawn(async move {
            let body = if i % 2 == 0 { format!("{{\"f{i}\":\"v\"}}") } else { r#"{"base":"v"}"#.to_string() };
            let (status, v) = call(&c, "PUT", &format!("/idx/_doc/n{i}"), &body).await;
            assert!(status == 201 || status == 200, "{}", v.to_json());
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    call(&c, "POST", "/idx/_refresh", "").await;
    for i in (0..n).step_by(2) {
        let (_, v) = call(&c, "GET", &format!("/idx/_count?q=f{i}.keyword:v"), "").await;
        assert_eq!(at(&v, &["count"]).as_i64(), Some(1), "f{i}: {}", v.to_json());
    }
}

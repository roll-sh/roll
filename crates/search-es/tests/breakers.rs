//! Circuit breakers against Elasticsearch 9.5.5 captures (`tests/fixtures/breakers.json`):
//! `inflight_requests` charges every request body (with its overhead of 2) and refuses one past
//! its limit with Elasticsearch's text and byte counts, except on the handlers Elasticsearch
//! lets through; the `parent` breaker checks each charge; `_nodes/stats` lists the breakers in
//! Elasticsearch's order with live numbers. ES|QL charges what it builds to `request` and to the
//! node's budget, and gives it back when the query ends.

use search_es::json::{Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::Arc;

async fn call(c: &Cluster, method: &str, uri: &str, body: Option<&[u8]>) -> (u16, Value) {
    let mut r = EsRequest::new(method, uri);
    if let Some(b) = body {
        r.headers.push(("content-type".into(), "application/json".into()));
        r.body = b.to_vec().into();
    }
    let resp = search_es::handle(c, r).await;
    let v = parse(&resp.body).ok().flatten().unwrap_or(Value::Str(String::from_utf8_lossy(&resp.body).into_owned()));
    (resp.status, v)
}

fn captured(key: &str) -> (u16, Value) {
    let all = parse(include_bytes!("fixtures/breakers.json")).unwrap().unwrap();
    match all.get(key).cloned() {
        Some(Value::Arr(mut a)) if a.len() == 2 => {
            let body = a.pop().unwrap();
            (a[0].as_i64().unwrap() as u16, body)
        }
        other => panic!("{key}: {other:?}"),
    }
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    let mut cur = v;
    for p in path.split('.') {
        cur = match cur {
            Value::Arr(a) => p.parse::<usize>().ok().and_then(|i| a.get(i)).unwrap_or(&Value::Null),
            Value::Obj(m) if p == "*" => m.values().next().unwrap_or(&Value::Null),
            other => other.get(p).unwrap_or(&Value::Null),
        };
    }
    cur
}

/// Breaker names in order, with the keys each reports.
fn names_and_keys(breakers: &Value) -> Vec<(String, Vec<String>)> {
    match breakers {
        Value::Obj(m) => m.iter().map(|(k, v)| (k.clone(), v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default())).collect(),
        _ => Vec::new(),
    }
}

/// Every number (and size unit after it) in `s` replaced by `N`: what differs between two heaps.
fn numbers_masked(s: &str) -> String {
    let mut out = digits_masked(s);
    for unit in ["Npb", "Ntb", "Ngb", "Nmb", "Nkb", "Nb"] {
        out = out.replace(unit, "N");
    }
    out
}

fn digits_masked(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_num = false;
    for c in s.chars() {
        if c.is_ascii_digit() || (in_num && c == '.') {
            if !in_num {
                out.push('N');
                in_num = true;
            }
        } else {
            in_num = false;
            out.push(c);
        }
    }
    out
}

#[tokio::test]
async fn in_flight_and_parent_breakers_trip_like_elasticsearch() {
    let c = Cluster::open(ClusterConfig::new("breakers"), Arc::new(MemoryStore::new())).await.unwrap();
    call(&c, "PUT", "/brk", Some(br#"{"settings":{"number_of_shards":1,"number_of_replicas":0}}"#)).await;
    let (_, stats) = call(&c, "GET", "/_nodes/stats/breaker?filter_path=nodes.*.breakers", None).await;
    assert_eq!(names_and_keys(at(&stats, "nodes.*.breakers")), names_and_keys(at(&captured("stats_default").1, "nodes.*.breakers")));

    let set = |limit: &str| format!(r#"{{"persistent":{{"network.breaker.inflight_requests.limit":{limit}}}}}"#);
    let (s, _) = call(&c, "PUT", "/_cluster/settings", Some(set(r#""100b""#).as_bytes())).await;
    assert_eq!(s, 200);
    let doc = format!(r#"{{"field":"{}"}}"#, "a".repeat(105));
    assert_eq!(call(&c, "POST", "/brk/_doc", Some(doc.as_bytes())).await, captured("in_flight_document"));
    let search = br#"{"query":{"match_all":{}},"size":1,"from":0,"track_total_hits":true,"_source":true,"timeout":"1m"}"#;
    assert_eq!(call(&c, "POST", "/brk/_search", Some(search)).await, captured("in_flight_search"));
    // Health and node stats are never refused.
    let (s, _) = call(&c, "GET", "/_cluster/health/brk", None).await;
    assert_eq!(s, captured("in_flight_exempt_health").0);
    let (_, v) = call(&c, "GET", "/_nodes/stats/breaker?filter_path=nodes.*.breakers.inflight_requests", None).await;
    let es = captured("in_flight_stats").1;
    for k in ["limit_size_in_bytes", "limit_size", "estimated_size_in_bytes", "overhead"] {
        assert_eq!(at(&v, &format!("nodes.*.breakers.inflight_requests.{k}")), at(&es, &format!("nodes.*.breakers.inflight_requests.{k}")), "{k}");
    }
    // (The reference node had tripped twice before the capture.)
    assert_eq!(at(&v, "nodes.*.breakers.inflight_requests.tripped").as_i64(), Some(2));
    // Small bodies pass, and their charge is released once answered.
    call(&c, "PUT", "/_cluster/settings", Some(set("null").as_bytes())).await;
    let (s, _) = call(&c, "POST", "/brk/_doc?refresh=true", Some(br#"{"f":"short"}"#)).await;
    assert_eq!(s, 201);
    let (_, v) = call(&c, "GET", "/_nodes/stats/breaker", None).await;
    assert_eq!(at(&v, "nodes.*.breakers.inflight_requests.estimated_size_in_bytes").as_i64(), Some(0));

    // The parent sums the children: a body over `indices.breaker.total.limit` trips it.
    call(&c, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.total.limit":"1b"}}"#)).await;
    let (s, v) = call(&c, "POST", "/brk/_doc", Some(br#"{"f":"short"}"#)).await;
    let (es_status, es) = captured("parent_cat");
    assert_eq!(s, es_status);
    assert_eq!(at(&v, "error.type"), at(&es, "error.type"));
    assert_eq!(numbers_masked(at(&v, "error.reason").as_str().unwrap()), numbers_masked(at(&es, "error.reason").as_str().unwrap()));
    assert_eq!(
        at(&v, "error.reason").as_str().unwrap().split(", real usage").next(),
        Some("[parent] Data too large, data for [<http_request>] would be [26/26b], which is larger than the limit of [1/1b]")
    );
    assert_eq!((at(&v, "error.bytes_wanted").as_i64(), at(&v, "error.bytes_limit").as_i64(), at(&v, "error.durability").as_str()), (Some(26), Some(1), Some("TRANSIENT")));
    let (s, _) = call(&c, "GET", "/_cluster/settings", None).await;
    assert_eq!(s, 200, "settings can still be fixed");
    let (_, v) = call(&c, "GET", "/_nodes/stats/breaker", None).await;
    assert_eq!(at(&v, "nodes.*.breakers.parent.tripped").as_i64(), Some(1));
    call(&c, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.total.limit":null}}"#)).await;
    let (s, _) = call(&c, "POST", "/brk/_doc", Some(br#"{"f":"short"}"#)).await;
    assert_eq!(s, 201);
}

/// 10,000 documents whose messages `VALUES` collects into about 2 MB, as in the capture.
async fn esql_logs(request_memory: Option<Arc<search_es::breaker::MemoryBudget>>) -> Cluster {
    let mut cfg = ClusterConfig::new("esql-breakers");
    cfg.request_memory = request_memory;
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    call(&c, "PUT", "/logs", Some(br#"{"mappings":{"properties":{"host":{"type":"keyword"},"message":{"type":"keyword"}}}}"#)).await;
    let mut bulk = String::new();
    for i in 0..10_000 {
        bulk.push_str(&format!("{{\"index\":{{}}}}\n{{\"host\":\"h{}\",\"message\":\"message number {i} {}\"}}\n", i % 10, "x".repeat(150)));
    }
    let (s, _) = call(&c, "POST", "/logs/_bulk?refresh=true", Some(bulk.as_bytes())).await;
    assert_eq!(s, 200);
    c
}

const VALUES_BY: &[u8] = br#"{"query":"FROM logs | STATS v = VALUES(message) BY host | LIMIT 1"}"#;

/// Elasticsearch's refusal with its numbers and the label of the structure that crossed the
/// limit masked (both depend on where the query was when it did).
fn refusal_masked(v: &Value) -> (String, String, Value, Value) {
    let reason = at(v, "error.reason").as_str().unwrap_or_default();
    let label = reason.split_once("data for [").and_then(|(_, r)| r.split_once("] would be")).map_or(String::new(), |(l, _)| l.to_string());
    (at(v, "error.root_cause.0.type").to_es_string(), numbers_masked(&reason.replace(&format!("[{label}]"), "[L]")), at(v, "error.durability").clone(), Value::Bool(at(v, "error.bytes_wanted").as_i64() > at(v, "error.bytes_limit").as_i64()))
}

#[tokio::test]
async fn esql_trips_the_request_breaker_and_gives_its_memory_back() {
    let c = esql_logs(None).await;
    let (s, _) = call(&c, "POST", "/_query", Some(VALUES_BY)).await;
    assert_eq!(s, 200, "under the default limit");
    call(&c, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.request.limit":"1mb"}}"#)).await;
    let (s, v) = call(&c, "POST", "/_query", Some(VALUES_BY)).await;
    let (es_status, es) = captured("esql_values_by");
    assert_eq!(s, es_status);
    assert_eq!(refusal_masked(&v), refusal_masked(&es), "{v:?}");
    let (_, stats) = call(&c, "GET", "/_nodes/stats/breaker?filter_path=nodes.*.breakers.request", None).await;
    assert_eq!(at(&stats, "nodes.*.breakers.request.estimated_size_in_bytes").as_i64(), Some(0), "{stats:?}");
}

#[tokio::test]
async fn esql_is_held_to_the_node_budget_whatever_the_tenant_sets() {
    let node = search_es::breaker::MemoryBudget::new(1 << 20);
    let c = esql_logs(Some(node.clone())).await;
    call(&c, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.request.limit":"-1"}}"#)).await;
    let (s, v) = call(&c, "POST", "/_query", Some(VALUES_BY)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (429, Some("circuit_breaking_exception")), "{v:?}");
    assert_eq!(at(&v, "error.bytes_limit").as_i64(), Some(1 << 20));
    assert_eq!(node.used(), 0);
}

//! Circuit breakers against Elasticsearch 9.5.5 captures (`tests/fixtures/breakers.json`):
//! `inflight_requests` charges every request body (with its overhead of 2) and refuses one past
//! its limit with Elasticsearch's text and byte counts, except on the handlers Elasticsearch
//! lets through; the `parent` breaker checks each charge; `_nodes/stats` lists the breakers in
//! Elasticsearch's order with live numbers. ES|QL and aggregations charge what they build to
//! `request` and through it to the node's budget, and give it back when the request ends; a
//! tenant's `request` limit is held to the operator's maximum.

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
async fn esql_logs(request_memory: Option<Arc<search_es::breaker::MemoryBudget>>, node_settings: &[(&str, &str)]) -> Cluster {
    let mut cfg = ClusterConfig::new("esql-breakers");
    cfg.request_memory = request_memory;
    cfg.node_settings = node_settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
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
    let c = esql_logs(None, &[]).await;
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

/// A `-1` request limit on a cluster of a node with `node`'s budget and no maximum on the
/// tenant's limit (only the node's budget can refuse), with 50,000 distinct keywords in `ids`.
async fn ids_cluster(name: &str, node: &Arc<search_es::breaker::MemoryBudget>) -> Cluster {
    let mut cfg = ClusterConfig::new(name);
    cfg.request_memory = Some(node.clone());
    cfg.node_settings = vec![(search_es::breaker::REQUEST_MAX_LIMIT.to_string(), "-1".to_string())];
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    call(&c, "PUT", "/ids", Some(br#"{"mappings":{"properties":{"id":{"type":"keyword"},"cat":{"type":"keyword"}}}}"#)).await;
    let mut bulk = String::new();
    for i in 0..50_000 {
        bulk.push_str(&format!("{{\"index\":{{}}}}\n{{\"id\":\"id-{i:08}\",\"cat\":\"c{}\"}}\n", i % 10));
    }
    assert_eq!(call(&c, "POST", "/ids/_bulk?refresh=true", Some(bulk.as_bytes())).await.0, 200);
    assert_eq!(call(&c, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.request.limit":"-1"}}"#)).await.0, 200);
    c
}

const TERMS_IDS: &[u8] = br#"{"size":0,"aggs":{"ids":{"terms":{"field":"id","size":50000}}}}"#;

/// The `[parent]` refusal a search gets when the node's budget refuses its aggregation (the
/// reduce refuses it, so it's the search phase's cause).
fn parent_refusal(s: u16, v: &Value) -> &Value {
    let cause = if at(v, "error.type").as_str() == Some("circuit_breaking_exception") { at(v, "error") } else { at(v, "error.caused_by") };
    assert_eq!((s, at(cause, "type").as_str(), at(cause, "durability").as_str()), (429, Some("circuit_breaking_exception"), Some("TRANSIENT")), "{v:?}");
    assert!(at(cause, "reason").as_str().unwrap().starts_with("[parent] Data too large, data for ["), "{v:?}");
    cause
}

#[tokio::test]
async fn aggregations_are_held_to_the_node_budget_whatever_the_tenant_sets() {
    let node = search_es::breaker::MemoryBudget::new(2 << 20);
    let c = ids_cluster("agg-node", &node).await;
    let (s, v) = call(&c, "POST", "/ids/_search", Some(TERMS_IDS)).await;
    let cause = parent_refusal(s, &v);
    // In the cluster's own numbers: at most what the node could have given it.
    assert!(at(cause, "bytes_limit").as_i64().unwrap() <= 2 << 20, "{v:?}");
    let (_, stats) = call(&c, "GET", "/_nodes/stats/breaker?filter_path=nodes.*.breakers", None).await;
    assert_eq!(at(&stats, "nodes.*.breakers.request.estimated_size_in_bytes").as_i64(), Some(0));
    assert_eq!(at(&stats, "nodes.*.breakers.parent.tripped").as_i64(), Some(1));
    assert_eq!(node.used(), 0);
}

#[tokio::test]
async fn the_tenants_request_limit_is_held_to_the_operators_maximum() {
    let mut cfg = ClusterConfig::new("clamp");
    cfg.request_memory = Some(search_es::breaker::MemoryBudget::new(4 << 20));
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    assert_eq!(call(&c, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.request.limit":"-1"}}"#)).await.0, 200);
    let (_, settings) = call(&c, "GET", "/_cluster/settings", None).await;
    assert_eq!(at(&settings, "persistent.indices.breaker.request.limit").as_str(), Some("-1"), "the setting reads as written");
    let (_, stats) = call(&c, "GET", "/_nodes/stats/breaker?filter_path=nodes.*.breakers.request", None).await;
    // A quarter of the node's budget by default.
    assert_eq!(at(&stats, "nodes.*.breakers.request.limit_size_in_bytes").as_i64(), Some(1 << 20));
}

#[tokio::test]
async fn one_tenants_kept_results_leave_room_for_anothers_aggregation() {
    let node = search_es::breaker::MemoryBudget::new(4 << 20);
    let a = esql_logs(Some(node.clone()), &[(search_es::breaker::REQUEST_MAX_LIMIT, "-1")]).await;
    call(&a, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.request.limit":"-1"}}"#)).await;
    let b = ids_cluster("tenant-b", &node).await;
    // A keeps async results until the node refuses it, as Elasticsearch's parent breaker, in
    // A's own numbers; alone, it still leaves an eighth of the node.
    let keep = br#"{"query":"FROM logs | KEEP message | LIMIT 1000","keep_on_completion":true}"#;
    let (mut ids, mut refused) = (Vec::new(), None);
    for _ in 0..60 {
        let (s, v) = call(&a, "POST", "/_query/async", Some(keep)).await;
        if s != 200 {
            refused = Some((s, v));
            break;
        }
        ids.push(at(&v, "id").as_str().unwrap().to_string());
    }
    let (s, v) = refused.expect("the node refuses A before 60 results");
    parent_refusal(s, &v);
    assert!(node.used() <= (4 << 20) - (4 << 20) / 8, "A leaves an eighth: {}", node.used());
    // B's small aggregation runs in what is left...
    let (s, v) = call(&b, "POST", "/ids/_search", Some(br#"{"size":0,"aggs":{"c":{"terms":{"field":"cat"}}}}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    // ...and a large one is refused in B's own numbers, never the node's.
    let (s, v) = call(&b, "POST", "/ids/_search", Some(TERMS_IDS)).await;
    let cause = parent_refusal(s, &v);
    assert!(at(cause, "bytes_limit").as_i64().unwrap() < (4 << 20) / 2, "{v:?}");
    // Deleting A's results gives their memory back.
    for id in ids {
        assert_eq!(call(&a, "DELETE", &format!("/_query/async/{id}"), None).await.0, 200);
    }
    assert_eq!(node.used(), 0);
}

/// `(status, body, warnings)` of an ES|QL query, the implicit limit's warning left out.
async fn esql_with_warnings(c: &Cluster, query: &str) -> (u16, Value, Vec<String>) {
    let mut r = EsRequest::new("POST", "/_query");
    r.headers.push(("content-type".into(), "application/json".into()));
    r.body = Value::Obj([("query".to_string(), Value::from(query))].into_iter().collect()).to_json().into_bytes().into();
    let resp = search_es::handle(c, r).await;
    let warnings = resp.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("warning")).filter_map(|(_, v)| v.split_once('"').and_then(|(_, rest)| rest.rsplit_once('"')).map(|(m, _)| m.to_string())).filter(|w| !w.contains("No limit defined")).collect();
    (resp.status, parse(&resp.body).ok().flatten().unwrap_or(Value::Null), warnings)
}

#[tokio::test]
async fn esql_builders_stop_at_1mib_like_elasticsearch() {
    let c = Cluster::open(ClusterConfig::new("builders"), Arc::new(MemoryStore::new())).await.unwrap();
    call(&c, "PUT", "/cc", Some(br#"{"mappings":{"properties":{"f":{"type":"keyword"}}}}"#)).await;
    call(&c, "POST", "/cc/_doc?refresh=true", Some(br#"{"f":"x"}"#)).await;
    let all = parse(include_bytes!("fixtures/breakers.json")).unwrap().unwrap();
    // CONCAT past 1 MiB fails the query; REPLACE past it is null with Elasticsearch's warnings.
    let concat = all.get("esql_concat_over_1mib").unwrap();
    let (s, v, _) = esql_with_warnings(&c, at(concat, "2").as_str().unwrap()).await;
    assert_eq!((s as i64, v), (at(concat, "0").as_i64().unwrap(), at(concat, "1").clone()));
    let replace = all.get("esql_replace_over_1mib").unwrap();
    let (s, v, warnings) = esql_with_warnings(&c, at(replace, "2").as_str().unwrap()).await;
    assert_eq!(s as i64, at(replace, "0").as_i64().unwrap());
    assert_eq!(at(&v, "values"), at(replace, "1.values"));
    let es: Vec<String> = match at(replace, "1.warnings") {
        Value::Arr(a) => a.iter().map(|w| w.as_str().unwrap().to_string()).collect(),
        _ => Vec::new(),
    };
    assert_eq!(warnings, es);
}

#[tokio::test]
async fn esql_eval_output_is_charged_as_each_row_is_built() {
    let c = esql_logs(None, &[]).await;
    call(&c, "PUT", "/_cluster/settings", Some(br#"{"persistent":{"indices.breaker.request.limit":"1mb"}}"#)).await;
    // Each row grows to about 1 KiB; one page of them is past the limit, so the breaker refuses
    // the EVAL itself.
    let q = br#"{"query":"FROM logs | EVAL a = CONCAT(message, message), b = CONCAT(a, a) | KEEP b | LIMIT 10000"}"#;
    let (s, v) = call(&c, "POST", "/_query", Some(q)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (429, Some("circuit_breaking_exception")), "{v:?}");
    let (_, stats) = call(&c, "GET", "/_nodes/stats/breaker?filter_path=nodes.*.breakers.request", None).await;
    assert_eq!(at(&stats, "nodes.*.breakers.request.estimated_size_in_bytes").as_i64(), Some(0));
}

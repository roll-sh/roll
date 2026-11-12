//! Operations safety against captures from Elasticsearch 9.5.5 (`fixtures/operations.json`):
//! searches as tasks (listing, get, cancel, the cancelled search's error), full thread pools
//! refusing work with `es_rejected_execution_exception`, a graceful stop that cancels searches
//! still running after the drain period, and HTTP edges (`Expect`, absolute-form targets,
//! malformed request heads).

mod common;

use common::*;
use search_es::json::Value;
use search_server::{ListenOptions, NodeConfig};
use std::sync::Arc;
use std::time::Duration;

/// A search that runs until cancelled (search scripts poll the task instead of counting loops).
const ENDLESS: &str = "long s=0; for (long i=0;i<100000000000L;i++){s+=i;} return s % 7;";
/// The script of the captured search.
const CAPTURED: &str = "long s=0; for (int i=0;i<900000;i++){s+=i;} return s % 7;";

fn fixture(key: &str) -> Value {
    let all = search_es::json::parse(include_bytes!("fixtures/operations.json")).unwrap().unwrap();
    all.get(key).cloned().unwrap_or_else(|| panic!("fixture {key}"))
}

/// The value at a `/`-separated path (array items by index).
fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('/').filter(|p| !p.is_empty()).try_fold(v, |v, p| match v {
        Value::Arr(a) => a.get(p.parse::<usize>().ok()?),
        Value::Obj(m) => m.get(p),
        _ => None,
    })
}

fn at_mut<'a>(v: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('/').filter(|p| !p.is_empty()).try_fold(v, |v, p| match v {
        Value::Arr(a) => a.get_mut(p.parse::<usize>().ok()?),
        Value::Obj(m) => m.get_mut(p),
        _ => None,
    })
}

fn json(r: &Resp) -> Value {
    search_es::json::parse(&r.body).unwrap_or_else(|e| panic!("{e}: {}", r.text())).unwrap()
}

/// Masks what differs between runs and nodes: node ids (`node`, keys and ids `<node>:<n>`),
/// times, and the run-specific parts of executor descriptions.
fn masked(v: &Value, node: &str) -> Value {
    match v {
        Value::Obj(m) => {
            let mut out = search_es::json::Map::new();
            for (k, x) in m {
                let k = k.replace(node, "NODE");
                let x = match k.as_str() {
                    "start_time_in_millis" | "running_time_in_nanos" | "id" | "took" => Value::from(0),
                    _ => masked(x, node),
                };
                out.insert(mask_ids(&k), x);
            }
            Value::Obj(out)
        }
        Value::Arr(a) => Value::Arr(a.iter().map(|x| masked(x, node)).collect()),
        Value::Str(s) => Value::Str(mask_reason(&mask_ids(&s.replace(node, "NODE")))),
        other => other.clone(),
    }
}

/// `NODE:123` -> `NODE:N`.
fn mask_ids(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("NODE:") {
        out.push_str(&rest[..i + 5]);
        rest = rest[i + 5..].trim_start_matches(|c: char| c.is_ascii_digit());
        out.push('N');
    }
    out.push_str(rest);
    out
}

/// Replaces what follows each marker up to a terminator with `*`.
fn mask_after(s: &str, marker: &str, stop: &[char]) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(marker) {
        out.push_str(&rest[..i + marker.len()]);
        rest = &rest[i + marker.len()..];
        let end = rest.find(|c| stop.contains(&c)).unwrap_or(rest.len());
        out.push('*');
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn mask_reason(s: &str) -> String {
    let mut s = s.to_string();
    for (marker, stop) in [
        ("@", &[']', ',', '['][..]),
        ("creationTimeNanos=", &[','][..]),
        ("EWMA = ", &[','][..]),
        ("total task execution time = ", &[','][..]),
        ("completed tasks = ", &[']'][..]),
        ("name = ", &['/'][..]),
        ("task=", &['}'][..]),
        ("req=", &['}'][..]),
        ("Lambda/", &['@'][..]),
    ] {
        s = mask_after(&s, marker, stop);
    }
    s
}

/// A node's `tasks` map as its masked tasks ordered by action (masked ids collide as keys).
fn sorted_tasks(tasks: &Value, node: &str) -> String {
    let mut v: Vec<String> = tasks.as_object().unwrap().values().map(|t| masked(t, node).to_json()).collect();
    v.sort();
    v.join("\n")
}

async fn slow_index(addr: std::net::SocketAddr, docs: usize) {
    let r = send(addr, "PUT", "/slow", &[("Content-Type", "application/json")], br#"{"settings":{"number_of_shards":1}}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let mut bulk = String::new();
    for i in 0..docs {
        bulk.push_str(&format!("{{\"index\":{{}}}}\n{{\"n\":{i}}}\n"));
    }
    let r = send(addr, "POST", "/slow/_bulk?refresh=true", &[("Content-Type", "application/x-ndjson")], bulk.as_bytes()).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

fn script_search(source: &str) -> String {
    format!(r#"{{"query":{{"script_score":{{"query":{{"match_all":{{}}}},"script":{{"source":"{source}"}}}}}}}}"#)
}

/// Waits for a task of `action` and returns the `_tasks?detailed` listing with it.
async fn wait_for_task(addr: std::net::SocketAddr, action: &str) -> Value {
    for _ in 0..500 {
        let r = send(addr, "GET", &format!("/_tasks?detailed=true&actions={action}*"), &[], b"").await;
        let v = json(&r);
        let found = v.get("nodes").and_then(Value::as_object).is_some_and(|n| n.values().any(|n| n.get("tasks").and_then(Value::as_object).is_some_and(|t| t.values().any(|t| t.get("action").and_then(Value::as_str) == Some(action)))));
        if found {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no {action} task appeared");
}

fn node_id(listing: &Value) -> String {
    listing.get("nodes").and_then(Value::as_object).and_then(|n| n.keys().next().cloned()).expect("a node")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn searches_are_tasks_and_cancel_like_elasticsearch() {
    let t = start_dev().await;
    slow_index(t.addr, 5).await;
    let addr = t.addr;
    let search = tokio::spawn(async move { send(addr, "GET", "/slow/_search", &[("Content-Type", "application/json"), ("X-Opaque-Id", "opq1")], script_search(ENDLESS).as_bytes()).await });
    // The shard's task starts once the request has been parsed and its shards resolved.
    wait_for_task(t.addr, "indices:data/read/search[phase/query]").await;
    let listing = json(&send(t.addr, "GET", "/_tasks?detailed=true&actions=indices:data/read/search*", &[], b"").await);
    let node = node_id(&listing);
    // The search and its shard's query phase, as Elasticsearch lists them.
    let tasks = listing.get("nodes").and_then(|n| n.get(&node)).and_then(|n| n.get("tasks")).cloned().unwrap();
    let ours = sorted_tasks(&tasks, &node).replace(ENDLESS, CAPTURED);
    let es_node = "WVzIze1MQxq6OakrtfZpmQ";
    assert_eq!(ours, sorted_tasks(&fixture("tasks_detailed_while_searching"), es_node));
    let id = tasks.as_object().unwrap().iter().find(|(_, v)| v.get("action").and_then(Value::as_str) == Some("indices:data/read/search")).map(|(k, _)| k.clone()).unwrap();
    // `GET _tasks/<id>` of a running search.
    let got = json(&send(t.addr, "GET", &format!("/_tasks/{id}"), &[], b"").await);
    let mut want = masked(&fixture("task_get_running"), es_node);
    // The captured search had no `X-Opaque-Id`.
    if let Some(h) = at_mut(&mut want, "task/headers").and_then(Value::as_object_mut) {
        h.insert("X-Opaque-Id".into(), "opq1".into());
    }
    assert_eq!(masked(&got, &node).to_json().replace(ENDLESS, CAPTURED), want.to_json());
    // Cancelling it answers with the task, now cancelled.
    let cancel = send(t.addr, "POST", &format!("/_tasks/{id}/_cancel"), &[], b"").await;
    let mut cancel = json(&cancel);
    for n in at_mut(&mut cancel, "nodes").and_then(Value::as_object_mut).unwrap().values_mut() {
        let n = n.as_object_mut().unwrap();
        for k in ["attributes", "roles", "name", "host", "ip", "transport_address"] {
            n.shift_remove(k);
        }
    }
    let mut want = masked(&fixture("cancel_by_id"), es_node);
    if let Some(h) = at_mut(&mut want, "nodes/NODE/tasks/NODE:N/headers").and_then(Value::as_object_mut) {
        h.insert("X-Opaque-Id".into(), "opq1".into());
    }
    assert_eq!(masked(&cancel, &node).to_json(), want.to_json());
    // The search stops with Elasticsearch's error.
    let r = tokio::time::timeout(Duration::from_secs(30), search).await.expect("the cancelled search answers").unwrap();
    assert_eq!(r.status, 400);
    assert_eq!(masked(&json(&r), &node).to_json(), masked(&fixture("cancelled_search"), es_node).to_json());
    // Nothing of it stays registered.
    let after = json(&send(t.addr, "GET", "/_tasks?actions=indices:data/read/search*", &[], b"").await);
    assert_eq!(after.to_json(), r#"{"nodes":{}}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn msearch_children_and_non_cancellable_templates() {
    let t = start_dev().await;
    slow_index(t.addr, 2).await;
    let addr = t.addr;
    let body = format!("{{\"index\":\"slow\"}}\n{}\n", script_search(ENDLESS));
    let ms = tokio::spawn(async move { send(addr, "POST", "/_msearch", &[("Content-Type", "application/x-ndjson")], body.as_bytes()).await });
    wait_for_task(t.addr, "indices:data/read/search[phase/query]").await;
    let listing = json(&send(t.addr, "GET", "/_tasks?detailed=true", &[], b"").await);
    let node = node_id(&listing);
    let tasks = listing.get("nodes").and_then(|n| n.get(&node)).and_then(|n| n.get("tasks")).and_then(Value::as_object).unwrap().clone();
    let by_action = |a: &str| tasks.iter().find(|(_, v)| v.get("action").and_then(Value::as_str) == Some(a)).map(|(k, v)| (k.clone(), v.clone())).unwrap();
    let (msearch_id, msearch) = by_action("indices:data/read/msearch");
    let (search_id, search) = by_action("indices:data/read/search");
    let (_, shard) = by_action("indices:data/read/search[phase/query]");
    assert_eq!(search.get("parent_task_id").and_then(Value::as_str), Some(msearch_id.as_str()));
    assert_eq!(shard.get("parent_task_id").and_then(Value::as_str), Some(search_id.as_str()));
    let desc = msearch.get("description").and_then(Value::as_str).unwrap();
    assert!(desc.starts_with("requests[1]: indices[slow], search_type[QUERY_THEN_FETCH], source[{\"query\":{\"script_score\""), "{desc}");
    // Cancelling the parent cancels the item's search (a ban on its children).
    let r = send(t.addr, "POST", "/_tasks/_cancel?actions=indices:data/read/msearch", &[], b"").await;
    assert_eq!(r.status, 200);
    let r = tokio::time::timeout(Duration::from_secs(30), ms).await.expect("the msearch answers").unwrap();
    let v = json(&r);
    assert_eq!(at(&v, "responses/0/status").and_then(Value::as_i64), Some(400), "{}", r.text());
    // A search template's own task can't be cancelled.
    let addr = t.addr;
    let tmpl = format!(r#"{{"source":{}}}"#, script_search(ENDLESS));
    let st = tokio::spawn(async move { send(addr, "GET", "/slow/_search/template", &[("Content-Type", "application/json")], tmpl.as_bytes()).await });
    wait_for_task(t.addr, "indices:data/read/search[phase/query]").await;
    let listing = json(&send(t.addr, "GET", "/_tasks?detailed=true", &[], b"").await);
    let tasks = listing.get("nodes").and_then(|n| n.get(&node)).and_then(|n| n.get("tasks")).and_then(Value::as_object).unwrap().clone();
    let tid = tasks.iter().find(|(_, v)| v.get("action").and_then(Value::as_str) == Some("indices:data/read/search/template")).map(|(k, _)| k.clone()).unwrap();
    let r = json(&send(t.addr, "POST", &format!("/_tasks/{tid}/_cancel"), &[], b"").await);
    assert_eq!(masked(&r, &node).to_json(), masked(&fixture("cancel_not_cancellable"), "WVzIze1MQxq6OakrtfZpmQ").to_json());
    let r = send(t.addr, "POST", "/_tasks/_cancel?actions=indices:data/read/search", &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(tokio::time::timeout(Duration::from_secs(30), st).await.expect("the template search answers").unwrap().status, 400);
}

/// A dev node whose pools are sized by node settings.
async fn pooled(settings: &[(&str, &str)]) -> TestNode {
    let node_settings = settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    start(NodeConfig { default_cluster: Some("dev".into()), node_settings, ..NodeConfig::default() }, ListenOptions::default()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_search_pool_queues_then_rejects() {
    let t = pooled(&[("thread_pool.search.size", "1"), ("thread_pool.search.queue_size", "1")]).await;
    slow_index(t.addr, 3).await;
    let addr = t.addr;
    let running = tokio::spawn(async move { send(addr, "GET", "/slow/_search", &[("Content-Type", "application/json")], script_search(ENDLESS).as_bytes()).await });
    wait_for_task(t.addr, "indices:data/read/search").await;
    // The second waits in the queue, the third is refused.
    let queued = tokio::spawn(async move { send(addr, "GET", "/slow/_search?size=0", &[], b"").await });
    for _ in 0..500 {
        let r = send(t.addr, "GET", "/_cat/thread_pool/search?h=active,queue", &[], b"").await;
        if r.text().trim() == "1 1" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let r = send(t.addr, "GET", "/slow/_search?size=0", &[], b"").await;
    assert_eq!(r.status, 429, "{}", r.text());
    let v = json(&r);
    let node = at(&v, "error/failed_shards/0/node").and_then(Value::as_str).unwrap().to_string();
    let es = masked(&fixture("search_rejected"), "WVzIze1MQxq6OakrtfZpmQ").to_json().replace("queue capacity = 2", "queue capacity = 1").replace("pool size = 2, active threads = 2, queued tasks = 2", "pool size = 1, active threads = 1, queued tasks = 1");
    assert_eq!(masked(&v, &node).to_json(), es);
    let stats = send(t.addr, "GET", "/_nodes/stats/thread_pool?filter_path=nodes.*.thread_pool.search", &[], b"").await;
    assert!(stats.text().contains(r#""search":{"threads":1,"queue":1,"active":1,"rejected":1,"largest":1,"completed":"#), "{}", stats.text());
    // Once the running search is cancelled, the queued one runs.
    send(t.addr, "POST", "/_tasks/_cancel?actions=indices:data/read/search", &[], b"").await;
    assert_eq!(tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().status, 400);
    assert_eq!(tokio::time::timeout(Duration::from_secs(30), queued).await.unwrap().unwrap().status, 200);
    let r = send(t.addr, "GET", "/_cat/thread_pool/search?h=name,active,queue,rejected,size,queue_size", &[], b"").await;
    assert_eq!(r.text(), "search 0 0 1 1 1\n");
}

/// A request waiting for its thread pool holds no node-wide running slot: with two running
/// slots, one search running and one queued behind it, the tenant's other requests still run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_queued_for_a_pool_hold_no_running_slot() {
    let node_settings = vec![("thread_pool.search.size".to_string(), "1".to_string()), ("thread_pool.search.queue_size".to_string(), "5".to_string())];
    let t = start(NodeConfig { default_cluster: Some("dev".into()), node_settings, max_running: 2, ..NodeConfig::default() }, ListenOptions::default()).await;
    slow_index(t.addr, 3).await;
    let addr = t.addr;
    let running = tokio::spawn(async move { send(addr, "GET", "/slow/_search", &[("Content-Type", "application/json")], script_search(ENDLESS).as_bytes()).await });
    wait_for_task(t.addr, "indices:data/read/search").await;
    let queued = tokio::spawn(async move { send(addr, "GET", "/slow/_search?size=0", &[], b"").await });
    for _ in 0..500 {
        let r = send(t.addr, "GET", "/_cat/thread_pool/search?h=active,queue", &[], b"").await;
        if r.text().trim() == "1 1" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Both running slots would be taken if the queued search held one.
    for _ in 0..3 {
        let r = tokio::time::timeout(Duration::from_secs(10), send(t.addr, "GET", "/_cluster/health", &[], b"")).await.expect("health runs beside the queued search");
        assert_eq!(r.status, 200);
        let r = tokio::time::timeout(Duration::from_secs(10), send(t.addr, "GET", "/slow/_doc/missing", &[], b"")).await.expect("a get runs beside it too");
        assert_eq!(r.status, 404);
    }
    send(t.addr, "POST", "/_tasks/_cancel?actions=indices:data/read/search", &[], b"").await;
    assert_eq!(tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().status, 400);
    assert_eq!(tokio::time::timeout(Duration::from_secs(30), queued).await.unwrap().unwrap().status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_write_pool_refuses_every_bulk_item() {
    let t = pooled(&[("thread_pool.write.size", "1"), ("thread_pool.write.queue_size", "0")]).await;
    let r = send(t.addr, "PUT", "/w1", &[], b"").await;
    assert_eq!(r.status, 200);
    let cluster = t.node.get_loaded("dev").unwrap().cluster.clone();
    // Holds the only write slot.
    let held = cluster.thread_pools.pool(search_es::thread_pool::PoolName::Write).admit().await.ok().unwrap();
    let r = send(t.addr, "POST", "/w1/_bulk", &[("Content-Type", "application/x-ndjson")], b"{\"index\":{\"_id\":\"a\"}}\n{\"n\":1}\n").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = json(&r);
    let node = cluster.node.id.clone();
    let mut want = masked(&fixture("bulk_rejected"), "WVzIze1MQxq6OakrtfZpmQ");
    // The captured bulk ran an ingest pipeline.
    want.as_object_mut().unwrap().shift_remove("ingest_took");
    if let Some(item) = at_mut(&mut want, "items/0/index") {
        item.as_object_mut().unwrap().insert("_id".into(), "a".into());
    }
    let ours = masked(&v, &node).to_json();
    let want = want.to_json().replace("queue capacity = 1", "queue capacity = 0").replace("queued tasks = 1", "queued tasks = 0");
    assert_eq!(ours, want);
    // A single-document write fails with the rejection itself.
    let r = send(t.addr, "PUT", "/w1/_doc/b", &[("Content-Type", "application/json")], b"{}").await;
    assert_eq!(r.status, 429);
    assert_eq!(at(&json(&r), "error/type").cloned().and_then(|v| v.as_str().map(str::to_string)).as_deref(), Some("es_rejected_execution_exception"));
    drop(held);
    assert_eq!(send(t.addr, "PUT", "/w1/_doc/b", &[("Content-Type", "application/json")], b"{}").await.status, 201);
    let r = send(t.addr, "GET", "/_cat/thread_pool/write?h=rejected,completed", &[], b"").await;
    assert_eq!(r.text(), "2 2\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_stop_cancels_searches_after_the_drain_period() {
    let catalog = Arc::new(search_server::StaticCatalog::new());
    catalog.insert(search_server::ClusterSpec::new("dev"));
    let node = search_server::Node::new(catalog, Arc::new(search_server::MemoryStores::new()), NodeConfig { default_cluster: Some("dev".into()), ..NodeConfig::default() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = search_server::Shutdown::new();
    let opts = ListenOptions { drain_timeout: Some(Duration::from_millis(300)), ..ListenOptions::default() };
    let server = tokio::spawn(search_server::serve_until(listener, node.clone(), opts, shutdown.clone()));
    slow_index(addr, 2).await;
    let search = tokio::spawn(async move { send(addr, "GET", "/slow/_search", &[("Content-Type", "application/json")], script_search(ENDLESS).as_bytes()).await });
    wait_for_task(addr, "indices:data/read/search").await;
    let stopping = std::time::Instant::now();
    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(10), server).await.expect("the stop doesn't wait for the search").unwrap().unwrap();
    assert!(stopping.elapsed() >= Duration::from_millis(300), "requests get the drain period");
    let r = search.await.unwrap();
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("task_cancelled_exception"), "{}", r.text());
    node.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn expectations_and_absolute_targets_like_elasticsearch() {
    let t = start_dev().await;
    let http = fixture("http");
    let mut s = tokio::net::TcpStream::connect(t.addr).await.unwrap();
    use tokio::io::AsyncWriteExt;
    s.write_all(b"POST /_search HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nExpect: something-else\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
    let mut buf = Vec::new();
    let r = read_response(&mut s, &mut buf, false).await;
    assert_eq!(r.status, 417);
    assert!(r.body.is_empty());
    assert_eq!(r.header("connection"), Some("close"));
    assert!(http.get("expect_bad").and_then(Value::as_str).unwrap().starts_with("b'HTTP/1.1 417 Expectation Failed | content-length: 0 | connection: close"));
    let mut s = tokio::net::TcpStream::connect(t.addr).await.unwrap();
    s.write_all(b"GET http://x/?filter_path=tagline HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut buf = Vec::new();
    let r = read_response(&mut s, &mut buf, false).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.text(), r#"{"error":"no handler found for uri [http://x/?filter_path=tagline] and method [GET]"}"#);
    assert!(http.get("abs_uri").and_then(Value::as_str).unwrap().contains(&r.text()));
}

/// Malformed request heads get Netty's 400 with the reason Elasticsearch names (or, for a
/// `Content-Length` that isn't a number, a closed connection), byte for byte. Elasticsearch
/// keeps some of these connections open after answering; roll closes them.
#[tokio::test]
async fn malformed_heads_are_answered_like_netty() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let t = start_dev().await;
    let heads = fixture("http_heads");
    let Value::Obj(cases) = &heads else { panic!("no http_heads") };
    for (name, case) in cases.iter().filter(|(k, _)| !k.starts_with('_')) {
        let request = case.get("request").and_then(Value::as_str).unwrap();
        let expected = case.get("response").and_then(Value::as_str).unwrap().replace("<open>", "<EOF>");
        let mut s = tokio::net::TcpStream::connect(t.addr).await.unwrap();
        s.write_all(&request.chars().map(|c| c as u8).collect::<Vec<u8>>()).await.unwrap();
        let mut got = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut got)).await;
        assert!(read.is_ok(), "{name}: the connection stays open");
        let got: String = got.iter().map(|&b| b as char).collect::<String>() + "<EOF>";
        assert_eq!(got, expected, "{name}");
    }
}

/// A by-query run scans with a search of its own: an `indices:data/read/search` child of the
/// run's task (with a `[phase/query]` child per shard), and cancelling the run stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn by_query_scans_are_child_searches_of_the_run() {
    let t = start_dev().await;
    slow_index(t.addr, 3).await;
    let addr = t.addr;
    let body = format!(r#"{{"query":{{"script":{{"script":{{"source":"{} return true;"}}}}}}}}"#, ENDLESS.trim_end_matches(" return s % 7;"));
    let run = tokio::spawn(async move { send(addr, "POST", "/slow/_delete_by_query", &[("Content-Type", "application/json"), ("X-Opaque-Id", "dbq1")], body.as_bytes()).await });
    let listing = wait_for_task(t.addr, "indices:data/read/search[phase/query]").await;
    let node = node_id(&listing);
    let tasks = at(&listing, &format!("nodes/{node}/tasks")).and_then(Value::as_object).unwrap().clone();
    let find = |action: &str| tasks.iter().find(|(_, v)| v.get("action").and_then(Value::as_str) == Some(action)).map(|(k, v)| (k.clone(), v.clone())).unwrap_or_else(|| panic!("no {action} in {tasks:?}"));
    let (run_id, _) = find("indices:data/write/delete/byquery");
    let (search_id, search) = find("indices:data/read/search");
    let (_, shard) = find("indices:data/read/search[phase/query]");
    assert_eq!(search.get("parent_task_id").and_then(Value::as_str), Some(run_id.as_str()));
    assert_eq!(shard.get("parent_task_id").and_then(Value::as_str), Some(search_id.as_str()));
    assert_eq!(shard.get("description").and_then(Value::as_str), Some("shardId[[slow][0]]"));
    assert_eq!(at(&search, "headers/X-Opaque-Id").and_then(Value::as_str), Some("dbq1"));
    assert!(search.get("description").and_then(Value::as_str).is_some_and(|d| d.starts_with("indices[slow], search_type[QUERY_THEN_FETCH]")), "{search:?}");
    let r = send(t.addr, "POST", &format!("/_tasks/{run_id}/_cancel"), &[], b"").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let done = tokio::time::timeout(Duration::from_secs(30), run).await.expect("the cancelled run ends").unwrap();
    assert!(done.status >= 400 || done.text().contains("canceled"), "{}", done.text());
    let r = send(t.addr, "GET", "/_tasks?actions=indices:data/read/search*", &[], b"").await;
    assert_eq!(json(&r).get("nodes").and_then(Value::as_object).map(|n| n.len()), Some(0), "{}", r.text());
}

//! Behaviour the differential fixtures can't pin down: requests that must stay bounded (sizes
//! and counts come from the client; Elasticsearch itself falls over on some of them, so they
//! can't be captured), node-level statistics on a cluster of our own, and the field table under
//! concurrent mapping updates.

use search_es::json::{Value, parse};
use search_es::{Cluster, EsRequest};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// Painless executions keep their memory accounts through `Counting`, as in rolld. Under it, a
/// cap on what the whole process holds: a script that got past its limit aborts this test
/// instead of taking the machine's memory.
#[global_allocator]
static ALLOC: search_painless::Counting<Capped> = search_painless::Counting(Capped);

struct Capped;

static LIVE: AtomicI64 = AtomicI64::new(0);
const PROCESS_CAP: i64 = 8 << 30;

// SAFETY: forwards to the system allocator, or fails an allocation past the cap.
unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if LIVE.fetch_add(l.size() as i64, Ordering::Relaxed) + l.size() as i64 > PROCESS_CAP {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as i64, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

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

/// Points in time are bounded per cluster: each keeps its searchers for up to a day on a node
/// other tenants share.
#[tokio::test]
async fn open_points_in_time_are_bounded() {
    let c = Cluster::in_memory("limits").await;
    call(&c, "PUT", "/idx/_doc/1?refresh=true", r#"{"a":1}"#).await;
    let mut first = String::new();
    for i in 0..1000 {
        let (status, v) = call(&c, "POST", "/idx/_pit?keep_alive=1h", "").await;
        assert_eq!(status, 200, "#{i}: {}", v.to_json());
        if i == 0 {
            first = at(&v, &["id"]).as_str().unwrap().to_string();
        }
    }
    let (status, v) = call(&c, "POST", "/idx/_pit?keep_alive=1h", "").await;
    assert_eq!(status, 429, "{}", v.to_json());
    assert_eq!(at(&v, &["error", "type"]).as_str(), Some("rejected_execution_exception"));
    assert_eq!(at(&v, &["error", "reason"]).as_str(), Some("Trying to create too many Point In Time contexts. Must be less than or equal to: [1000]."));
    // Closing one makes room.
    assert_eq!(call(&c, "DELETE", "/_pit", &format!(r#"{{"id":"{first}"}}"#)).await.0, 200);
    assert_eq!(call(&c, "POST", "/idx/_pit?keep_alive=1h", "").await.0, 200);
}

/// A MaxSim over one document with many vectors (query vectors × document vectors × dims of
/// work: 268M multiply-adds, seconds in a debug build) stops at the request's `timeout`. There
/// is only one document, so only the score's own polling of the deadline can stop it; the
/// search answers with what it has, `timed_out`.
#[tokio::test]
async fn max_sim_stops_at_the_request_timeout() {
    let c = Cluster::in_memory("limits").await;
    let (status, v) = call(&c, "PUT", "/rv", r#"{"mappings":{"properties":{"v":{"type":"rank_vectors","dims":32}}}}"#).await;
    assert_eq!(status, 200, "{}", v.to_json());
    let vectors = |n: usize| format!("[{}]", vec![format!("[{}]", vec!["1"; 32].join(",")); n].join(","));
    let (status, v) = call(&c, "PUT", "/rv/_doc/1?refresh=true", &format!(r#"{{"v":{}}}"#, vectors(4096))).await;
    assert_eq!(status, 201, "{}", v.to_json());
    let query = format!(
        r#"{{"timeout":"100ms","query":{{"script_score":{{"query":{{"match_all":{{}}}},"script":{{"source":"maxSimDotProduct(params.q, 'v')","params":{{"q":{}}}}}}}}}}}"#,
        vectors(2048)
    );
    let (status, v) = call(&c, "POST", "/rv/_search", &query).await;
    assert_eq!(status, 200, "{}", v.to_json());
    assert_eq!(at(&v, &["timed_out"]).as_bool(), Some(true), "{}", v.to_json());
}

/// The script from the security review of `21708aef1`: each copy is under the per-object cap,
/// together they are 125 GiB. Elasticsearch 9.5.5 runs out of heap and exits on it; with its
/// per-execution guard on (`script.painless.max_allocation_bytes.context.<context>.limit:
/// 64mb`) it answers the responses below. roll's node limit (64 MiB hosted) answers the same,
/// in `_execute` and in the other contexts, with its own byte counts in the brackets.
#[tokio::test]
async fn painless_memory_is_bounded_per_execution() {
    let c = Cluster::in_memory("limits").await;
    call(&c, "PUT", "/i/_doc/1?refresh=true", r#"{"a":1}"#).await;
    let unknown = r#""script_stack":["<<< unknown portion of script >>>"]"#;
    let guard = r#"{"type":"painless_error","reason":"script allocation limit exceeded: allocation of [33554464] bytes brings the running total to [67109628] bytes, over the limit of [67108864] bytes"}"#;
    let script_exception = |src: &str| format!(r#"{{"root_cause":[{guard}],"type":"script_exception","reason":"runtime error",{unknown},"script":{},"lang":"painless","caused_by":{guard}}}"#, serde_json::json!(src));
    let body = |src: &str| serde_json::json!({ "script": { "source": src } }).to_string();
    let exploit = |end: &str| format!("String s = \"x\"; while (s.length() < 128 * 1024 * 1024) {{ s = s + s; }} List l = new ArrayList(); for (int i = 0; i < 1000; i++) {{ l.add(s + i); }} {end}");

    let src = exploit("return l.size();");
    let (status, v) = call(&c, "POST", "/_scripts/painless/_execute", &body(&src)).await;
    assert_eq!(status, 400);
    assert_like_es(&v, &format!(r#"{{"error":{},"status":400}}"#, script_exception(&src)));

    let src = exploit("ctx._source.n = l.size();");
    let (status, v) = call(&c, "POST", "/i/_update/1", &body(&src)).await;
    assert_eq!(status, 400);
    let cause = script_exception(&src).replacen(&format!(r#""root_cause":[{guard}],"#), "", 1);
    assert_like_es(&v, &format!(r#"{{"error":{{"root_cause":[{{"type":"illegal_argument_exception","reason":"failed to execute script"}}],"type":"illegal_argument_exception","reason":"failed to execute script","caused_by":{cause}}},"status":400}}"#));

    let src = exploit("ctx.n = l.size();");
    let pipeline = serde_json::json!({ "pipeline": { "processors": [{ "script": { "source": src } }] }, "docs": [{ "_source": { "a": 1 } }] });
    let (status, v) = call(&c, "POST", "/_ingest/pipeline/_simulate", &pipeline.to_string()).await;
    assert_eq!(status, 200);
    assert_like_es(&v, &format!(r#"{{"docs":[{{"error":{}}}]}}"#, script_exception(&src)));
    // the node is fine afterwards
    let (status, _) = call(&c, "POST", "/_scripts/painless/_execute", r#"{"script":{"source":"return 1"}}"#).await;
    assert_eq!(status, 200);
}

/// `got` equals Elasticsearch's response `es` but for the numbers in brackets.
fn assert_like_es(got: &Value, es: &str) {
    fn blank(s: &str) -> serde_json::Value {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('[') {
            out.push_str(&rest[..=i]);
            rest = &rest[i + 1..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && rest[digits..].starts_with(']') {
                out.push('N');
                rest = &rest[digits..];
            }
        }
        out.push_str(rest);
        serde_json::from_str(&out).unwrap()
    }
    assert_eq!(blank(&got.to_json()), blank(es), "{}", got.to_json());
}

/// A scripted metric's state outlives each segment's script host, and so does what its map
/// script keeps on the `request` breaker: 2.5 MiB a segment fits a 4 MiB breaker, both segments'
/// together don't (roll's own; Elasticsearch charges scripts to no breaker).
#[tokio::test]
async fn scripted_metric_state_stays_charged_across_segments() {
    let c = Cluster::in_memory("limits").await;
    call(&c, "PUT", "/sm", r#"{"settings":{"number_of_shards":1,"number_of_replicas":0}}"#).await;
    for batch in 0..2 {
        let bulk: String = (0..40).map(|i| format!("{{\"index\":{{\"_id\":\"{batch}-{i}\"}}}}\n{{\"n\":{i}}}\n")).collect();
        let (status, v) = call(&c, "POST", "/sm/_bulk?refresh=true", &bulk).await;
        assert_eq!(status, 200, "{}", v.to_json());
    }
    let (_, v) = call(&c, "GET", "/sm/_stats/segments", "").await;
    assert!(at(&v, &["indices", "sm", "primaries", "segments", "count"]).as_i64().unwrap_or(0) >= 2, "{}", v.to_json());
    let (status, _) = call(&c, "PUT", "/_cluster/settings", r#"{"persistent":{"indices.breaker.request.limit":"4mb"}}"#).await;
    assert_eq!(status, 200);
    let agg = r#"{"size":0,"aggs":{"kept":{"scripted_metric":{"init_script":"state.l = []","map_script":"state.l.add(new int[16384])","combine_script":"return state.l.size()","reduce_script":"return states.size()"}}}}"#;
    let (_, v) = call(&c, "POST", "/sm/_search", agg).await;
    let body = v.to_json();
    assert!(body.contains("circuit_breaking_exception") && body.contains("data for [<script>]"), "{body}");
}

//! The operation-level WAL: what a write leaves behind (one record, no segment), what a wake
//! replays (every acknowledged operation, with its `_seq_no`, `_version` and `_primary_term`,
//! idempotently), what a commit keeps (segments, recent delete tombstones), index close/open,
//! point-in-time branches, and stored task results and async results across unloads.
//!
//! Object storage is S3 when `ROLL_E2E_S3_BUCKET` is set (`search_store::test_objects`), a
//! temp directory otherwise.

mod common;

use search_es::EsRequest;
use search_server::{ClusterSpec, MemoryStores, Node, NodeConfig, ObjectStores, StaticCatalog};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn req(cluster: &str, method: &str, uri: &str, body: &str) -> EsRequest {
    let mut r = EsRequest::json(method, uri, body.as_bytes().to_vec());
    r.headers.push(("authorization".into(), common::basic(cluster, "pw")));
    r
}

async fn call(node: &Arc<Node>, cluster: &str, method: &str, uri: &str, body: &str) -> (u16, Value) {
    // On a worker thread, as the HTTP server runs requests (a fresh stack: debug builds of the
    // deeper APIs, SQL among them, need most of one).
    let (n, r) = (node.clone(), req(cluster, method, uri, body));
    let r = tokio::spawn(async move { n.handle(r).await }).await.unwrap();
    (r.status, serde_json::from_slice(&r.body).unwrap_or(Value::Null))
}

async fn ok(node: &Arc<Node>, cluster: &str, method: &str, uri: &str, body: &str) -> Value {
    let (s, b) = call(node, cluster, method, uri, body).await;
    assert!(s < 300, "{method} {uri} -> {s}: {b}");
    b
}

fn cache_dir(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    search_store::test_dir().join(format!("server-durable-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

async fn open_store(objects: search_store::Objects, node: &str, cache: &std::path::Path, retain_ms: u64) -> search_store::Store {
    let cfg = search_store::StoreConfig {
        node: node.to_string(),
        verify: Some(search_server::persist::segment_verifier()),
        manifest_retain_ms: retain_ms,
        ..Default::default()
    };
    search_store::Store::open(objects, cache, cfg).await.unwrap()
}

fn catalog() -> Arc<StaticCatalog> {
    let catalog = Arc::new(StaticCatalog::new());
    for c in ["c1", "c2", "child"] {
        catalog.insert(ClusterSpec::new(c).with_password("pw"));
    }
    catalog
}

fn node_on(store: &search_store::Store) -> Arc<Node> {
    Node::new(catalog(), Arc::new(ObjectStores::new(store.clone())), NodeConfig::default())
}

fn count(v: &Value) -> u64 {
    v["count"].as_u64().unwrap_or_else(|| panic!("no count in {v}"))
}

/// `(seq_no, version, primary_term)` of every document of `index`, by id (realtime GETs).
async fn versions(n: &Arc<Node>, cluster: &str, index: &str, ids: impl IntoIterator<Item = String>) -> BTreeMap<String, Option<(i64, i64, i64)>> {
    let mut out = BTreeMap::new();
    for id in ids {
        let (s, v) = call(n, cluster, "GET", &format!("/{index}/_doc/{id}"), "").await;
        let meta = match s {
            200 => Some((v["_seq_no"].as_i64().unwrap(), v["_version"].as_i64().unwrap(), v["_primary_term"].as_i64().unwrap())),
            404 => None,
            _ => panic!("GET {id}: {s} {v}"),
        };
        out.insert(id, meta);
    }
    out
}

/// Elasticsearch flushes a closing index and reopens it on that commit: documents indexed
/// without a refresh are all there after `_open` (finding 9 of the review, ES 9.5.5: 4), in
/// memory and durable, and after a wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_flushes_and_open_reopens_on_the_commit() {
    let mem = Node::new(catalog(), Arc::new(MemoryStores::new()), NodeConfig { unload_memory_only: true, ..NodeConfig::default() });
    let store = open_store(search_store::test_objects("server-close"), "n1", &cache_dir("close"), 0).await;
    let durable = node_on(&store);
    for n in [&mem, &durable] {
        ok(n, "c1", "PUT", "/i", r#"{"settings":{"number_of_shards":2}}"#).await;
        for d in 0..4 {
            ok(n, "c1", "PUT", &format!("/i/_doc/{d}"), r#"{"f":"v"}"#).await;
        }
        let closed = ok(n, "c1", "POST", "/i/_close", "").await;
        assert_eq!(closed["indices"]["i"]["closed"], true, "{closed}");
        let (s, v) = call(n, "c1", "GET", "/i/_count", "").await;
        assert_eq!(s, 400, "closed: {v}");
        assert_eq!(v["error"]["type"], "index_closed_exception", "{v}");
        ok(n, "c1", "POST", "/i/_open", "").await;
        assert_eq!(count(&ok(n, "c1", "GET", "/i/_count", "").await), 4);
    }
    // The durable close committed both shards: a wake replays nothing.
    let ms = store.read_latest_manifests("c1").await.unwrap();
    assert_eq!(ms.iter().map(|(_, m)| m.max_seq_no + 1).sum::<i64>(), 4, "{ms:?}");
    assert!(ms.iter().all(|(_, m)| m.local_checkpoint == m.max_seq_no));
    assert!(durable.unload("c1").await);
    assert_eq!(count(&ok(&durable, "c1", "GET", "/i/_count", "").await), 4);
    assert_eq!(durable.get_loaded("c1").unwrap().recovery.records, 0);
}

/// A write appends one WAL record and never cuts a segment: segment layouts follow refreshes
/// as in Elasticsearch (50 writes, one refresh: one segment), and nothing is committed to
/// object storage until a flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_log_operations_and_never_cut_segments() {
    let store = open_store(search_store::test_objects("server-oplog"), "n1", &cache_dir("oplog"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/i", r#"{"settings":{"refresh_interval":"-1"}}"#).await;
    let manifests = store.stats().manifests_written;
    let l = n.get_loaded("c1").unwrap();
    let d = l.data.clone().unwrap();
    for i in 0..50 {
        ok(&n, "c1", "PUT", &format!("/i/_doc/{i}"), &format!(r#"{{"n":{i},"t":"doc {i}"}}"#)).await;
    }
    ok(&n, "c1", "POST", "/i/_bulk", "{\"delete\":{\"_id\":\"0\"}}\n{\"index\":{\"_id\":\"x\"}}\n{\"n\":-1}\n").await;
    assert_eq!(d.records.load(Ordering::Relaxed), 52, "one record per operation");
    assert_eq!(d.full_commits.load(Ordering::Relaxed), 0);
    assert_eq!(store.stats().manifests_written - manifests, 1, "only the shard's attach wrote a manifest");
    // The translog stats count the logged operations until a commit, as Elasticsearch's do.
    let tl = ok(&n, "c1", "GET", "/i/_stats/translog", "").await["indices"]["i"]["primaries"]["translog"].clone();
    assert_eq!((tl["operations"].as_u64(), tl["uncommitted_operations"].as_u64()), (Some(52), Some(52)), "{tl}");
    assert!(tl["size_in_bytes"].as_u64().unwrap() > 55 * 52, "{tl}");
    ok(&n, "c1", "POST", "/i/_refresh", "").await;
    let segs = ok(&n, "c1", "GET", "/i/_segments", "").await;
    let shard = &segs["indices"]["i"]["shards"]["0"][0];
    assert_eq!(shard["num_search_segments"], 1, "{segs}");
    assert_eq!(count(&ok(&n, "c1", "GET", "/i/_count", "").await), 50);
    // `_flush` commits: segments to object storage, the WAL trimmed.
    ok(&n, "c1", "POST", "/i/_flush", "").await;
    assert_eq!(d.full_commits.load(Ordering::Relaxed), 1);
    assert_eq!(d.unflushed_log_bytes(), 0);
    let tl = ok(&n, "c1", "GET", "/i/_stats/translog", "").await["indices"]["i"]["primaries"]["translog"].clone();
    assert_eq!((tl["operations"].as_u64(), tl["size_in_bytes"].as_u64()), (Some(0), Some(55)), "{tl}");
    drop(l);
    drop(d);
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/i/_count", "").await), 50);
    assert_eq!(n.get_loaded("c1").unwrap().recovery.records, 0);
}

/// `_flush` commits only the shards it targets, as Elasticsearch flushes only those: another
/// index's operations stay in the WAL (and its translog stats), and `_flush` on every index
/// commits the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flush_commits_only_the_targeted_shards() {
    let store = open_store(search_store::test_objects("server-flush1"), "n1", &cache_dir("flush1"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/a", r#"{"settings":{"number_of_shards":2}}"#).await;
    ok(&n, "c1", "PUT", "/b", "").await;
    for i in 0..6 {
        ok(&n, "c1", "PUT", &format!("/a/_doc/{i}"), r#"{"f":1}"#).await;
        ok(&n, "c1", "PUT", &format!("/b/_doc/{i}"), r#"{"f":1}"#).await;
    }
    let d = n.get_loaded("c1").unwrap().data.clone().unwrap();
    let translog_ops = |v: &Value, i: &str| v["indices"][i]["primaries"]["translog"]["uncommitted_operations"].as_u64();
    ok(&n, "c1", "POST", "/a/_flush", "").await;
    assert_eq!(d.full_commits.load(Ordering::Relaxed), 2, "both shards of a, not b");
    let st = ok(&n, "c1", "GET", "/_stats/translog", "").await;
    assert_eq!((translog_ops(&st, "a"), translog_ops(&st, "b")), (Some(0), Some(6)), "{st}");
    assert!(d.unflushed_log_bytes() > 0);
    // A flushing force merge commits its shards too.
    ok(&n, "c1", "POST", "/b/_forcemerge?max_num_segments=1", "").await;
    assert_eq!(d.full_commits.load(Ordering::Relaxed), 3);
    assert_eq!(d.unflushed_log_bytes(), 0);
    let ms = store.read_latest_manifests("c1").await.unwrap();
    assert_eq!(ms.iter().map(|(_, m)| m.max_seq_no + 1).sum::<i64>(), 12, "{ms:?}");
    // GET _flush with nothing new commits nothing more.
    ok(&n, "c1", "GET", "/_flush", "").await;
    assert_eq!(d.full_commits.load(Ordering::Relaxed), 3);
}

/// A dropped cluster (no flush, as after a crash or a lost lease) wakes from its WAL with every
/// acknowledged operation exactly as acknowledged; dropping it again right after the wake
/// replays the same history again (idempotent), and new writes continue the sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_restores_seq_nos_versions_and_terms_idempotently() {
    let store = open_store(search_store::test_objects("server-replay"), "n1", &cache_dir("replay"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/r", r#"{"settings":{"number_of_shards":3},"mappings":{"properties":{"k":{"type":"keyword"}}}}"#).await;
    let mut ids = Vec::new();
    for i in 0..30 {
        let id = format!("d{i}");
        ok(&n, "c1", "PUT", &format!("/r/_doc/{id}?routing=x{}", i % 4), &format!(r#"{{"k":"k{i}","n":{i}}}"#)).await;
        ids.push(id);
    }
    // Updates, deletes, a dynamic field, external versions, a create conflict (no seq no) and
    // a bulk with a document that fails to parse (its seq no is not used either).
    for i in 0..10 {
        ok(&n, "c1", "POST", &format!("/r/_update/d{i}?routing=x{}", i % 4), r#"{"doc":{"extra":"yes"}}"#).await;
    }
    for i in 10..15 {
        ok(&n, "c1", "DELETE", &format!("/r/_doc/d{i}?routing=x{}", i % 4), "").await;
    }
    ok(&n, "c1", "PUT", "/r/_doc/ext?version=42&version_type=external", r#"{"k":"ext"}"#).await;
    ids.push("ext".into());
    let (s, _) = call(&n, "c1", "PUT", "/r/_create/ext", r#"{"k":"again"}"#).await;
    assert_eq!(s, 409);
    let bulk = ok(&n, "c1", "POST", "/r/_bulk", "{\"index\":{\"_id\":\"b1\"}}\n{\"n\":\"not a number\"}\n{\"index\":{\"_id\":\"b2\"}}\n{\"n\":7}\n").await;
    assert_eq!(bulk["errors"], true, "{bulk}");
    ids.push("b1".into());
    ids.push("b2".into());
    let route = |id: &str| id.strip_prefix('d').and_then(|n| n.parse::<usize>().ok()).map(|n| format!("?routing=x{}", n % 4)).unwrap_or_default();
    let get_all = |n: Arc<Node>, ids: Vec<String>| async move {
        let mut out = BTreeMap::new();
        for id in ids {
            let r = route(&id);
            let v = versions(&n, "c1", "r", [format!("{id}{r}")]).await;
            out.extend(v);
        }
        out
    };
    let before = get_all(n.clone(), ids.clone()).await;
    ok(&n, "c1", "POST", "/r/_refresh", "").await;
    let hits_before = ok(&n, "c1", "POST", "/r/_search?size=100", r#"{"query":{"exists":{"field":"extra"}},"sort":["n"]}"#).await;
    let stats_before = ok(&n, "c1", "GET", "/r/_stats/docs", "").await["_all"]["primaries"]["docs"].clone();

    for round in 0..2 {
        assert!(n.force_unload("c1").await, "round {round}");
        // 30 documents, 5 deleted, `ext` and `b2`.
        assert_eq!(count(&ok(&n, "c1", "GET", "/r/_count", "").await), 27, "round {round}");
        let l = n.get_loaded("c1").unwrap();
        // 30 creates, 10 updates, 5 deletes, `ext` and `b2` (failed writes used no seq no).
        assert_eq!(l.recovery.records, 47, "round {round}: {:?}", l.recovery);
        assert_eq!(l.recovery.failed_records, 0, "{:?}", l.recovery);
        drop(l);
        assert_eq!(get_all(n.clone(), ids.clone()).await, before, "round {round}");
        let hits = ok(&n, "c1", "POST", "/r/_search?size=100", r#"{"query":{"exists":{"field":"extra"}},"sort":["n"]}"#).await;
        assert_eq!(hits["hits"]["hits"], hits_before["hits"]["hits"], "round {round}");
        assert_eq!(ok(&n, "c1", "GET", "/r/_stats/docs", "").await["_all"]["primaries"]["docs"]["count"], stats_before["count"]);
    }
    // The external version and the delete tombstones came back too.
    let (s, v) = call(&n, "c1", "PUT", "/r/_doc/ext?version=41&version_type=external", r#"{"k":"old"}"#).await;
    assert_eq!(s, 409, "{v}");
    // New writes continue each shard's sequence.
    // (Documents routed with `x1` share a shard.)
    let max_seq: i64 = before.iter().filter(|(id, _)| id.ends_with("routing=x1")).filter_map(|(_, m)| m.map(|m| m.0)).max().unwrap();
    let w = ok(&n, "c1", "PUT", "/r/_doc/new?routing=x1", r#"{"k":"new"}"#).await;
    assert!(w["_seq_no"].as_i64().unwrap() > max_seq && w["_primary_term"] == 1, "{w}");
    let again = ok(&n, "c1", "PUT", "/r/_doc/d1?routing=x1", r#"{"k":"k1"}"#).await;
    assert_eq!(again["_version"].as_i64().unwrap(), before["d1?routing=x1"].unwrap().1 + 1);
    // A clean unload commits: the next wake replays nothing and has the same documents.
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/r/_count", "").await), 28);
    assert_eq!(n.get_loaded("c1").unwrap().recovery.records, 0);
}

/// Delete tombstones are part of a commit for `index.gc_deletes`: an external version older
/// than a delete is refused after an unload (as within one Elasticsearch process), and
/// accepted once `gc_deletes` passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_tombstones_survive_unload_until_gc_deletes() {
    let store = open_store(search_store::test_objects("server-tomb"), "n1", &cache_dir("tomb"), 0).await;
    let n = node_on(&store);
    // Long enough that a slow (loaded, S3) unload and wake stay within it; shortened below.
    ok(&n, "c1", "PUT", "/t", r#"{"settings":{"index.gc_deletes":"60s"}}"#).await;
    ok(&n, "c1", "PUT", "/t/_doc/1?version=10&version_type=external", r#"{"a":1}"#).await;
    ok(&n, "c1", "PUT", "/t/_doc/2?version=10&version_type=external", r#"{"a":2}"#).await;
    let del = ok(&n, "c1", "DELETE", "/t/_doc/1?version=11&version_type=external", "").await;
    assert_eq!(del["result"], "deleted");
    ok(&n, "c1", "DELETE", "/t/_doc/2?version=12&version_type=external", "").await;
    // Committed (tombstones in the manifest), then unloaded.
    ok(&n, "c1", "POST", "/t/_flush", "").await;
    let ms = store.read_latest_manifests("c1").await.unwrap();
    assert!(ms[0].1.user_data.contains_key(search_server::persist::TOMBSTONES_KEY), "{:?}", ms[0].1.user_data);
    assert!(n.unload("c1").await);
    let (s, v) = call(&n, "c1", "PUT", "/t/_doc/1?version=5&version_type=external", r#"{"a":3}"#).await;
    assert_eq!(s, 409, "{v}");
    assert_eq!(v["error"]["reason"], "[1]: version conflict, current version [11] is higher or equal to the one provided [5]");
    let (s, v) = call(&n, "c1", "PUT", "/t/_doc/2?version=12&version_type=external_gte", r#"{"a":3}"#).await;
    assert_eq!(s, 201, "external_gte equal to the delete's version: {v}");
    // Past gc_deletes (a dynamic setting) the tombstone is forgotten, and a commit no longer
    // keeps it either.
    // The shards take the change at once: the unload right after it commits with it.
    ok(&n, "c1", "PUT", "/t/_settings", r#"{"index.gc_deletes":"50ms"}"#).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(n.unload("c1").await);
    let ms = store.read_latest_manifests("c1").await.unwrap();
    assert!(!ms[0].1.user_data.contains_key(search_server::persist::TOMBSTONES_KEY), "{:?}", ms[0].1.user_data);
    let (s, v) = call(&n, "c1", "PUT", "/t/_doc/1?version=5&version_type=external", r#"{"a":4}"#).await;
    assert_eq!(s, 201, "{v}");
}

/// A point-in-time branch replays the parent's WAL up to the branch point: operations before
/// it are in the child with their seq nos, later ones only in the parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn point_in_time_branches_replay_up_to_the_branch_point() {
    let store = open_store(search_store::test_objects("server-pitb"), "n1", &cache_dir("pitb"), 3_600_000).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/p", r#"{"settings":{"number_of_shards":2}}"#).await;
    for i in 0..10 {
        ok(&n, "c1", "PUT", &format!("/p/_doc/a{i}"), r#"{"when":"before"}"#).await;
    }
    ok(&n, "c1", "DELETE", "/p/_doc/a0", "").await;
    // Ten documents and a delete, all only in the WAL (no flush yet).
    let a_ids: Vec<String> = (0..10).map(|i| format!("a{i}")).collect();
    let b_ids: Vec<String> = (0..5).map(|i| format!("b{i}")).collect();
    let at_branch = versions(&n, "c1", "p", a_ids.clone()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    tokio::time::sleep(Duration::from_millis(20)).await;
    for id in &b_ids {
        ok(&n, "c1", "PUT", &format!("/p/_doc/{id}"), r#"{"when":"after"}"#).await;
    }
    ok(&n, "c1", "DELETE", "/p/_doc/a1", "").await;
    n.branch_at("c1", "child", Some(at)).await.unwrap();
    assert_eq!(count(&ok(&n, "child", "GET", "/p/_count", "").await), 9);
    assert_eq!(versions(&n, "child", "p", a_ids.clone()).await, at_branch);
    assert!(versions(&n, "child", "p", b_ids.clone()).await.values().all(Option::is_none));
    ok(&n, "c1", "POST", "/p/_refresh", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/p/_count", "").await), 13);
    // The child is a cluster of its own from here on.
    ok(&n, "child", "PUT", "/p/_doc/c0?refresh=true", r#"{"when":"child"}"#).await;
    assert_eq!(count(&ok(&n, "child", "GET", "/p/_count", "").await), 10);
    assert!(n.unload("child").await);
    assert_eq!(count(&ok(&n, "child", "GET", "/p/_count", "").await), 10);
    assert_eq!(count(&ok(&n, "c1", "GET", "/p/_count", "").await), 13);
}

/// A `wait_for_completion=false` task's result lives in the `.tasks` index, so it outlives
/// the cluster's unload (Elasticsearch: a node restart).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stored_task_results_survive_unload() {
    let store = open_store(search_store::test_objects("server-tasks"), "n1", &cache_dir("tasks"), 0).await;
    let n = node_on(&store);
    let mut bulk = String::new();
    for i in 0..20 {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"n\":{i}}}\n"));
    }
    ok(&n, "c1", "POST", "/q/_bulk?refresh=true", &bulk).await;
    let t = ok(&n, "c1", "POST", "/q/_delete_by_query?wait_for_completion=false", r#"{"query":{"range":{"n":{"lt":5}}}}"#).await;
    let task = t["task"].as_str().unwrap().to_string();
    let done = ok(&n, "c1", "GET", &format!("/_tasks/{task}?wait_for_completion=true&timeout=10s"), "").await;
    assert_eq!(done["completed"], true, "{done}");
    assert_eq!(done["response"]["deleted"], 5, "{done}");
    // The result reached `.tasks` before the cluster may idle out.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !n.unload("c1").await {
        assert!(Instant::now() < deadline, "the cluster unloads once its task ended");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let after = ok(&n, "c1", "GET", &format!("/_tasks/{task}"), "").await;
    assert_eq!(after["completed"], true, "{after}");
    assert_eq!(after["response"]["deleted"], 5, "{after}");
    assert_eq!(after["task"]["action"], done["task"]["action"]);
    assert_eq!(count(&ok(&n, "c1", "GET", "/q/_count", "").await), 15);
}

/// Async results (async search, EQL, ES|QL) and SQL cursors outlive the cluster's instance,
/// as on Elasticsearch, where they live in `.async-search`: stored before the request that
/// made them is acknowledged (a drop without a flush, like a crash, keeps them: WAL replay),
/// deletions too, and every later wake reads them back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn async_results_survive_unload_and_crash() {
    let store = open_store(search_store::test_objects("server-async"), "n1", &cache_dir("async"), 0).await;
    let n = node_on(&store);
    let mut bulk = String::new();
    for i in 0..12 {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"n\":{i},\"@timestamp\":{}}}\n", 1_700_000_000_000i64 + i));
    }
    ok(&n, "c1", "POST", "/q/_bulk?refresh=true", &bulk).await;
    let search = ok(&n, "c1", "POST", "/q/_async_search?keep_on_completion=true&wait_for_completion_timeout=10s", r#"{"query":{"range":{"n":{"gte":4}}}}"#).await;
    let search_id = search["id"].as_str().unwrap_or_else(|| panic!("{search}")).to_string();
    let gone = ok(&n, "c1", "POST", "/q/_async_search?keep_on_completion=true&wait_for_completion_timeout=10s", "{}").await;
    let gone_id = gone["id"].as_str().unwrap().to_string();
    ok(&n, "c1", "DELETE", &format!("/_async_search/{gone_id}"), "").await;
    let eql = ok(&n, "c1", "POST", "/q/_eql/search?keep_on_completion=true&wait_for_completion_timeout=10s", r#"{"query":"any where n < 3"}"#).await;
    let eql_id = eql["id"].as_str().unwrap_or_else(|| panic!("{eql}")).to_string();
    let esql = ok(&n, "c1", "POST", "/_query/async", r#"{"query":"FROM q | STATS c = COUNT(*)","keep_on_completion":true,"wait_for_completion_timeout":"10s"}"#).await;
    let esql_id = esql["id"].as_str().unwrap_or_else(|| panic!("{esql}")).to_string();
    let sql = ok(&n, "c1", "POST", "/_sql?format=json", r#"{"query":"SELECT n FROM q ORDER BY n","fetch_size":5}"#).await;
    let cursor = sql["cursor"].as_str().unwrap_or_else(|| panic!("{sql}")).to_string();
    assert_eq!(sql["rows"].as_array().map(Vec::len), Some(5), "{sql}");

    // Dropped without a flush (a crash): the WAL replays what was acknowledged.
    assert!(n.force_unload("c1").await);
    let s = ok(&n, "c1", "GET", &format!("/_async_search/{search_id}"), "").await;
    assert_eq!(s["response"]["hits"]["total"]["value"], 8, "{s}");
    assert_eq!(s["is_running"], false);
    assert_eq!(call(&n, "c1", "GET", &format!("/_async_search/{gone_id}"), "").await.0, 404, "a deleted result stays deleted");
    let e = ok(&n, "c1", "GET", &format!("/_eql/search/{eql_id}"), "").await;
    assert_eq!(e["hits"]["total"]["value"], 3, "{e}");
    let q = ok(&n, "c1", "GET", &format!("/_query/async/{esql_id}"), "").await;
    assert_eq!(q["values"][0][0], 12, "{q}");
    let page = ok(&n, "c1", "POST", "/_sql?format=json", &format!(r#"{{"cursor":"{cursor}"}}"#)).await;
    assert_eq!(page["rows"][0][0], 5, "{page}");

    // A graceful unload and wake keep them too (and what this instance changed: the cursor
    // moved on, the ES|QL result was deleted).
    ok(&n, "c1", "DELETE", &format!("/_query/async/{esql_id}"), "").await;
    let next = page["cursor"].as_str().unwrap_or_else(|| panic!("{page}")).to_string();
    assert!(n.unload("c1").await);
    assert_eq!(ok(&n, "c1", "GET", &format!("/_async_search/{search_id}"), "").await["response"]["hits"]["total"]["value"], 8);
    assert_eq!(call(&n, "c1", "GET", &format!("/_query/async/{esql_id}"), "").await.0, 404);
    let last = ok(&n, "c1", "POST", "/_sql?format=json", &format!(r#"{{"cursor":"{next}"}}"#)).await;
    assert_eq!(last["rows"][0][0], 10, "{last}");
    assert_eq!(ok(&n, "c1", "GET", &format!("/_eql/search/status/{eql_id}"), "").await["is_running"], false);
}

/// Every mapping parameter survives the cluster state's round trip through storage: a
/// document written after a wake is indexed exactly like one written before it. (A custom
/// normalizer named `lowercase` makes `normalizer_skip_store_original_value: true` a
/// non-default that must be persisted; synthetic source then rebuilds sorted values.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn documents_written_after_a_wake_are_indexed_like_before() {
    let store = open_store(search_store::test_objects("server-mapwake"), "n1", &cache_dir("mapwake"), 0).await;
    let n = node_on(&store);
    let mapping = r#"{"settings":{"analysis":{"normalizer":{"lowercase":{"type":"custom","filter":["lowercase"]}}},"index":{"mapping.source.mode":"synthetic"}},
        "mappings":{"properties":{"k":{"type":"keyword","normalizer":"lowercase","normalizer_skip_store_original_value":true},
        "i":{"type":"keyword","normalizer":"lowercase","normalizer_skip_store_original_value":true,"ignore_above":10}}}}"#;
    ok(&n, "c1", "PUT", "/s", mapping).await;
    let doc = r#"{"k":["may the force be with you!","do or do not"],"i":["May the FORCE be with You!","Do or Do Not"]}"#;
    ok(&n, "c1", "PUT", "/s/_doc/1", doc).await;
    let before = ok(&n, "c1", "GET", "/s/_doc/1", "").await["_source"].clone();
    assert_eq!(before["k"], serde_json::json!(["do or do not", "may the force be with you!"]), "{before}");
    let m = ok(&n, "c1", "GET", "/s/_mapping", "").await;
    assert_eq!(m["s"]["mappings"]["properties"]["k"]["normalizer_skip_store_original_value"], true, "{m}");
    assert!(n.unload("c1").await);
    ok(&n, "c1", "PUT", "/s/_doc/2", doc).await;
    assert_eq!(ok(&n, "c1", "GET", "/s/_doc/2", "").await["_source"], before);
    assert!(n.unload("c1").await);
    assert_eq!(ok(&n, "c1", "GET", "/s/_doc/1", "").await["_source"], before);
    assert_eq!(ok(&n, "c1", "GET", "/s/_doc/2", "").await["_source"], before);
}

/// An idle unload is invisible: writes nobody refreshed stay unsearchable across it (the
/// commit keeps what the searcher showed), realtime reads see them, and they become
/// searchable when Elasticsearch would have made them so: on `_refresh` with refreshes
/// disabled, on the next search otherwise. A crash still replays into a refreshed shard, as
/// an Elasticsearch restart does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unload_keeps_unrefreshed_writes_unsearchable() {
    let objects = search_store::test_objects("server-unrefreshed");
    let store = open_store(objects.clone(), "n1", &cache_dir("unrefreshed"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/off", r#"{"settings":{"number_of_shards":2,"refresh_interval":"-1"}}"#).await;
    ok(&n, "c1", "PUT", "/on", r#"{"settings":{"number_of_shards":2}}"#).await;
    for index in ["off", "on"] {
        for d in 0..6 {
            ok(&n, "c1", "PUT", &format!("/{index}/_doc/{d}"), r#"{"f":"old"}"#).await;
        }
        ok(&n, "c1", "POST", &format!("/{index}/_refresh"), "").await;
        // Not refreshed: two new docs, a delete and an update.
        ok(&n, "c1", "PUT", &format!("/{index}/_doc/new1"), r#"{"f":"new"}"#).await;
        ok(&n, "c1", "PUT", &format!("/{index}/_doc/new2"), r#"{"f":"new"}"#).await;
        ok(&n, "c1", "DELETE", &format!("/{index}/_doc/1"), "").await;
        ok(&n, "c1", "PUT", &format!("/{index}/_doc/2"), r#"{"f":"new"}"#).await;
    }
    let searchable = |index: &'static str| {
        let n = n.clone();
        async move {
            let all = count(&ok(&n, "c1", "GET", &format!("/{index}/_count"), "").await);
            let new = count(&ok(&n, "c1", "GET", &format!("/{index}/_count?q=f:new"), "").await);
            let (_, g) = call(&n, "c1", "GET", &format!("/{index}/_doc/new1?realtime=false"), "").await;
            let (_, rt) = call(&n, "c1", "GET", &format!("/{index}/_doc/new1"), "").await;
            assert_eq!(rt["found"], true, "realtime reads see every write: {rt}");
            (all, new, g["found"] == true)
        }
    };
    assert_eq!(searchable("off").await, (6, 0, false));
    for round in 0..2 {
        assert!(n.unload("c1").await);
        assert_eq!(searchable("off").await, (6, 0, false), "round {round}: refreshes disabled");
        assert_eq!(n.get_loaded("c1").unwrap().recovery.records, 0);
    }
    // The next search refreshes a shard whose writes a scheduled refresh would have published.
    assert_eq!(searchable("on").await, (7, 3, true));
    ok(&n, "c1", "POST", "/off/_refresh", "").await;
    assert_eq!(searchable("off").await, (7, 3, true));
    // The refresh changed what is searchable, not the commit's contents: the unload still
    // records it.
    assert!(n.unload("c1").await);
    assert_eq!(searchable("off").await, (7, 3, true));

    // Unrefreshed writes after the last commit, then a crash (no unload): the WAL replays
    // into a refreshed shard.
    ok(&n, "c1", "PUT", "/off/_doc/new3", r#"{"f":"new"}"#).await;
    assert_eq!(searchable("off").await, (7, 3, true));
    assert!(n.force_unload("c1").await);
    assert_eq!(searchable("off").await, (8, 4, true));
    let st = ok(&n, "c1", "GET", "/off/_stats/translog", "").await;
    // The replayed write is in the translog again, after the ten no flush trimmed (the unloads'
    // commits are none Elasticsearch would have made).
    assert_eq!(st["indices"]["off"]["total"]["translog"]["uncommitted_operations"], 11, "{st}");
    // Closing commits everything as searchable: the index reopens as a new shard copy would.
    ok(&n, "c1", "PUT", "/off/_doc/new4", r#"{"f":"new"}"#).await;
    ok(&n, "c1", "POST", "/off/_close", "").await;
    assert!(n.unload("c1").await);
    ok(&n, "c1", "POST", "/off/_open", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/off/_count?q=f:new", "").await), 5);
}

/// Trained model deployments are cluster state on Elasticsearch, restarted after a restart:
/// a deployment started before a crash runs again after the wake (counters start over, as in
/// a restarted process); a graceful unload also keeps the counters (the wake is invisible);
/// one stopped stays stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trained_model_deployments_survive_unload_and_crash() {
    let store = open_store(search_store::test_objects("server-ml"), "n1", &cache_dir("ml"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/_ml/trained_models/m1", r#"{"model_type":"pytorch","inference_config":{"pass_through":{}}}"#).await;
    ok(&n, "c1", "PUT", "/_ml/trained_models/m1/vocabulary", r#"{"vocabulary":["[PAD]","[UNK]","a"]}"#).await;
    ok(&n, "c1", "PUT", "/_ml/trained_models/m1/definition/0", r#"{"total_definition_length":4,"definition":"UEsDBA==","total_parts":1}"#).await;
    let started = ok(&n, "c1", "POST", "/_ml/trained_models/m1/deployment/_start?deployment_id=d1&number_of_allocations=1", "").await;
    assert_eq!(started["assignment"]["task_parameters"]["deployment_id"], "d1", "{started}");
    ok(&n, "c1", "POST", "/_ml/trained_models/d1/_infer", r#"{"docs":[{"text_field":"a"}]}"#).await;
    let stats = |n: Arc<Node>| async move {
        let s = ok(&n, "c1", "GET", "/_ml/trained_models/m1/_stats", "").await;
        s["trained_model_stats"][0]["deployment_stats"].clone()
    };
    let before = stats(n.clone()).await;
    assert_eq!(before["inference_count"], 1, "{before}");

    // A crash: the assignment was saved with the request that started it.
    assert!(n.force_unload("c1").await);
    let after = stats(n.clone()).await;
    assert_eq!(after["deployment_id"], "d1", "the deployment runs again: {after}");
    assert_eq!(after["state"], "started");
    assert_eq!(after["start_time"], before["start_time"]);
    assert_eq!(after["inference_count"], 0, "a new process");
    ok(&n, "c1", "POST", "/_ml/trained_models/d1/_infer", r#"{"docs":[{"text_field":"a"}]}"#).await;
    ok(&n, "c1", "POST", "/_ml/trained_models/d1/_infer", r#"{"docs":[{"text_field":"a"}]}"#).await;
    assert!(n.unload("c1").await);
    let kept = stats(n.clone()).await;
    assert_eq!((kept["inference_count"].clone(), kept["nodes"][0]["inference_cache_hit_count"].clone()), (2.into(), 1.into()), "{kept}");

    // Stopped, then a graceful unload: it stays stopped.
    ok(&n, "c1", "POST", "/_ml/trained_models/d1/deployment/_stop", "").await;
    assert!(n.unload("c1").await);
    assert_eq!(stats(n.clone()).await, Value::Null);
    let (s, v) = call(&n, "c1", "POST", "/_ml/trained_models/d1/_infer", r#"{"docs":[{"text_field":"a"}]}"#).await;
    assert!(s >= 400, "{s} {v}");
}

/// An unload doesn't reset `_stats` (an Elasticsearch node that never restarted keeps
/// counting, and keeps field data loaded): indexing, get and search counters, search groups,
/// field usage and loaded field data carry over, written with the unload's commit even when
/// only reads happened since the last one. Branches start without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shard_statistics_survive_unload() {
    let store = open_store(search_store::test_objects("server-stats"), "n1", &cache_dir("stats"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/s", r#"{"settings":{"number_of_shards":1},"mappings":{"properties":{"name":{"type":"text","fielddata":true}}}}"#).await;
    for d in 0..3 {
        ok(&n, "c1", "PUT", &format!("/s/_doc/{d}"), r#"{"name":"the quick fox"}"#).await;
    }
    ok(&n, "c1", "DELETE", "/s/_doc/2", "").await;
    ok(&n, "c1", "GET", "/s/_doc/0", "").await;
    ok(&n, "c1", "POST", "/s/_flush", "").await;
    // Only reads after the last commit.
    ok(&n, "c1", "POST", "/s/_refresh", "").await;
    ok(&n, "c1", "POST", "/s/_search", r#"{"stats":["g1"],"query":{"match":{"name":"quick"}},"aggs":{"t":{"terms":{"field":"name"}}}}"#).await;
    let read = |n: Arc<Node>| async move {
        let s = ok(&n, "c1", "GET", "/s/_stats?groups=g1&fielddata_fields=name", "").await;
        let t = &s["indices"]["s"]["total"];
        assert!(t["fielddata"]["fields"]["name"]["memory_size_in_bytes"].as_i64().unwrap_or(0) > 0, "{s}");
        let u = ok(&n, "c1", "GET", "/s/_field_usage_stats", "").await;
        let u = &u["s"]["shards"][0];
        (
            [
                &t["indexing"]["index_total"],
                &t["indexing"]["delete_total"],
                &t["get"]["total"],
                &t["search"]["query_total"],
                &t["search"]["groups"]["g1"]["query_total"],
                &t["flush"]["total"],
                &t["translog"]["operations"],
            ]
                .map(|v| v.as_i64().unwrap_or(-1)),
            u["tracking_id"].clone(),
            u["stats"]["fields"]["name"]["any"].clone(),
        )
    };
    let before = read(n.clone()).await;
    assert_eq!(before.0, [3, 1, 1, 1, 1, 1, 0], "{before:?}");
    assert_eq!(before.2, 1, "{before:?}");
    assert!(n.unload("c1").await);
    let after = read(n.clone()).await;
    assert_eq!(after, before, "the stats calls themselves count nothing");
    ok(&n, "c1", "PUT", "/s/_doc/9", r#"{"name":"x"}"#).await;
    assert_eq!(read(n.clone()).await.0[0], 4, "counting goes on");
    // A branch has the parent's documents, not its history.
    n.branch("c1", "child").await.unwrap();
    let s = ok(&n, "child", "GET", "/s/_stats", "").await;
    let t = &s["indices"]["s"]["total"];
    assert_eq!((t["docs"]["count"].as_i64(), t["indexing"]["index_total"].as_i64()), (Some(3), Some(0)), "{s}");
}

/// Snapshot contents are durable: a snapshot pins each shard's commit point in object storage
/// (no copy), so restoring it after a graceful unload, a crash, the source index's deletion and
/// merges, garbage collection, or in a branch gets every document back with its `_seq_no`,
/// `_version` and `_primary_term`. Deleting a snapshot drops its pins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_contents_survive_unload_crash_gc_and_branches() {
    let repo_dir = cache_dir("snap-repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let store = open_store(search_store::test_objects("server-snap"), "n1", &cache_dir("snap"), 0).await;
    let cfg = NodeConfig { node_settings: vec![("path.repo".into(), repo_dir.to_string_lossy().into_owned())], ..NodeConfig::default() };
    let n = Node::new(catalog(), Arc::new(ObjectStores::new(store.clone())), cfg);
    ok(&n, "c1", "PUT", "/s", r#"{"settings":{"number_of_shards":2}}"#).await;
    ok(&n, "c1", "PUT", "/empty", r#"{"settings":{"number_of_shards":1}}"#).await;
    // A closed index is snapshotted from its last commit.
    ok(&n, "c1", "PUT", "/closed/_doc/1", r#"{"f":"c"}"#).await;
    ok(&n, "c1", "PUT", "/closed/_doc/2", r#"{"f":"c"}"#).await;
    ok(&n, "c1", "POST", "/closed/_close", "").await;
    let ids: Vec<String> = (0..12).map(|i| format!("d{i}")).collect();
    for id in &ids {
        ok(&n, "c1", "PUT", &format!("/s/_doc/{id}"), r#"{"f":"v1"}"#).await;
    }
    ok(&n, "c1", "PUT", "/s/_doc/d0", r#"{"f":"v2"}"#).await;
    ok(&n, "c1", "DELETE", "/s/_doc/d1", "").await;
    let at_snapshot = versions(&n, "c1", "s", ids.clone()).await;
    ok(&n, "c1", "PUT", "/_snapshot/r", r#"{"type":"fs","settings":{"location":"r"}}"#).await;
    let s1 = ok(&n, "c1", "PUT", "/_snapshot/r/s1?wait_for_completion=true", r#"{"indices":"s,empty,closed"}"#).await;
    assert_eq!(s1["snapshot"]["state"], "SUCCESS", "{s1}");
    // A pin per non-empty shard; the empty index needs none.
    let pins = store.list_pins("c1").await.unwrap();
    assert_eq!(pins.len(), 3, "{pins:?}");

    // The index goes on: more writes, a merge, then it is deleted, and the cluster unloads.
    for i in 0..5 {
        ok(&n, "c1", "PUT", &format!("/s/_doc/x{i}"), r#"{"f":"later"}"#).await;
    }
    ok(&n, "c1", "DELETE", "/s/_doc/d2", "").await;
    ok(&n, "c1", "POST", "/s/_forcemerge?max_num_segments=1", "").await;
    ok(&n, "c1", "DELETE", "/s", "").await;
    assert!(n.unload("c1").await);
    ok(&n, "c1", "DELETE", "/closed", "").await;
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    assert_eq!(gc.pins, 3, "{gc:?}");
    ok(&n, "c1", "POST", "/_snapshot/r/s1/_restore?wait_for_completion=true", r#"{"indices":"closed"}"#).await;
    ok(&n, "c1", "POST", "/closed/_refresh", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/closed/_count", "").await), 2);

    let restore = ok(&n, "c1", "POST", "/_snapshot/r/s1/_restore?wait_for_completion=true", r#"{"indices":"s"}"#).await;
    assert_eq!(restore["snapshot"]["shards"]["successful"], 2, "{restore}");
    ok(&n, "c1", "POST", "/s/_refresh", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/s/_count", "").await), 11);
    assert_eq!(versions(&n, "c1", "s", ids.clone()).await, at_snapshot);
    assert_eq!(ok(&n, "c1", "GET", "/s/_doc/d0", "").await["_source"]["f"], "v2");

    // A second snapshot, then a crash (no flush): the restored data and the snapshot are both
    // durable.
    ok(&n, "c1", "PUT", "/s/_doc/y0", r#"{"f":"after-restore"}"#).await;
    ok(&n, "c1", "PUT", "/_snapshot/r/s2?wait_for_completion=true", r#"{"indices":"s"}"#).await;
    assert!(n.force_unload("c1").await);
    ok(&n, "c1", "POST", "/_snapshot/r/s2/_restore?wait_for_completion=true", r#"{"indices":"s","rename_pattern":"s","rename_replacement":"s2"}"#).await;
    ok(&n, "c1", "POST", "/s2/_refresh", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/s2/_count", "").await), 12);
    assert_eq!(count(&ok(&n, "c1", "GET", "/s/_count", "").await), 12, "the restore and the write after it were acknowledged");

    // A clone shares the contents; a branch restores the parent's snapshots.
    ok(&n, "c1", "PUT", "/_snapshot/r/s1/_clone/s1c", r#"{"indices":"s"}"#).await;
    n.branch_at("c1", "child", None).await.unwrap();
    ok(&n, "child", "DELETE", "/s", "").await;
    ok(&n, "child", "POST", "/_snapshot/r/s1c/_restore?wait_for_completion=true", r#"{"indices":"s"}"#).await;
    ok(&n, "child", "POST", "/s/_refresh", "").await;
    assert_eq!(count(&ok(&n, "child", "GET", "/s/_count", "").await), 11);
    assert_eq!(versions(&n, "child", "s", ids.clone()).await, at_snapshot);

    // Deleting snapshots drops their pins (only this cluster's: the branch has its own copies).
    ok(&n, "c1", "DELETE", "/_snapshot/r/s1,s1c", "").await;
    let left = store.list_pins("c1").await.unwrap();
    assert_eq!(left.len(), 2, "s2's: {left:?}");
    assert_eq!(store.list_pins("child").await.unwrap().len(), 7);
    ok(&n, "c1", "DELETE", "/_snapshot/r/s2", "").await;
    assert!(store.list_pins("c1").await.unwrap().is_empty());
}

/// Analyzers keep the synonym rules they loaded until something reloads them (a set changed
/// with `refresh=false`, or replaced by rules the chain can't build with `lenient: false`):
/// an unload doesn't reload them, as an Elasticsearch node that never restarted wouldn't;
/// closing and opening the index does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_synonym_rules_survive_unload_until_a_reload() {
    let store = open_store(search_store::test_objects("server-syn"), "n1", &cache_dir("syn"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/_synonyms/set1", r#"{"synonyms_set":[{"synonyms":"foo => bar"}]}"#).await;
    let tk = "tokenizer";
    let index = format!(
        r#"{{"settings":{{"analysis":{{"filter":{{"syn":{{"type":"synonym","synonyms_set":"set1","updateable":true,"lenient":false}},"stop":{{"type":"stop","stopwords":["baz"]}}}},"analyzer":{{"a":{{"type":"custom","{tk}":"whitespace","filter":["lowercase","stop","syn"]}}}}}}}},"mappings":{{"properties":{{"t":{{"type":"text","search_analyzer":"a"}}}}}}}}"#
    );
    ok(&n, "c1", "PUT", "/i", &index).await;
    ok(&n, "c1", "PUT", "/i/_doc/1", r#"{"t":"x"}"#).await;
    let token = |v: Value| v["tokens"][0]["token"].as_str().unwrap_or_else(|| panic!("{v}")).to_string();
    let analyze = r#"{"analyzer":"a","text":"foo"}"#;
    assert_eq!(token(ok(&n, "c1", "POST", "/i/_analyze", analyze).await), "bar");
    // Stored without a reload: the analyzer keeps "bar", across unloads.
    ok(&n, "c1", "PUT", "/_synonyms/set1?refresh=false", r#"{"synonyms_set":[{"synonyms":"foo => qux"}]}"#).await;
    assert_eq!(token(ok(&n, "c1", "POST", "/i/_analyze", analyze).await), "bar");
    assert!(n.unload("c1").await);
    assert_eq!(token(ok(&n, "c1", "POST", "/i/_analyze", analyze).await), "bar", "a wake is no reload");
    assert!(n.unload("c1").await);
    assert_eq!(token(ok(&n, "c1", "POST", "/i/_analyze", analyze).await), "bar");
    ok(&n, "c1", "POST", "/i/_reload_search_analyzers", "").await;
    assert_eq!(token(ok(&n, "c1", "POST", "/i/_analyze", analyze).await), "qux");
    assert!(n.unload("c1").await);
    assert_eq!(token(ok(&n, "c1", "POST", "/i/_analyze", analyze).await), "qux");
    // Rules the chain can't build ("baz" is a stop word): the reload fails, the index keeps
    // its analyzer and its shards, across an unload too.
    ok(&n, "c1", "PUT", "/_synonyms/set1", r#"{"synonyms_set":[{"synonyms":"foo => bar, baz"}]}"#).await;
    assert!(n.unload("c1").await);
    let stats = ok(&n, "c1", "GET", "/i/_stats", "").await;
    assert_eq!(stats["indices"]["i"]["health"], "yellow", "started (one replica, unassigned): {stats}");
    assert_eq!(token(ok(&n, "c1", "POST", "/i/_analyze", analyze).await), "qux");
    assert_eq!(count(&ok(&n, "c1", "GET", "/i/_count", "").await), 1);
    // The same for an index without documents (its shards open only for the unload).
    ok(&n, "c1", "PUT", "/_synonyms/set1", r#"{"synonyms_set":[{"synonyms":"foo => bar"}]}"#).await;
    ok(&n, "c1", "PUT", "/e", &index).await;
    assert_eq!(token(ok(&n, "c1", "POST", "/e/_analyze", analyze).await), "bar");
    ok(&n, "c1", "PUT", "/_synonyms/set1", r#"{"synonyms_set":[{"synonyms":"foo => bar, baz"}]}"#).await;
    assert!(n.unload("c1").await);
    assert_eq!(token(ok(&n, "c1", "POST", "/e/_analyze", analyze).await), "bar", "the empty index keeps its rules");
    // Reopening the index builds from the sets: it can't start.
    ok(&n, "c1", "POST", "/i/_close", "").await;
    ok(&n, "c1", "POST", "/i/_open", "").await;
    assert!(ok(&n, "c1", "GET", "/i/_stats", "").await["indices"].as_object().is_some_and(|m| m.is_empty()));
    assert!(n.unload("c1").await);
    assert!(ok(&n, "c1", "GET", "/i/_stats", "").await["indices"].as_object().is_some_and(|m| m.is_empty()));
}

/// What a running Elasticsearch node holds without a restart survives an unload too: the
/// translog figures (the unload's commit is no flush Elasticsearch would make) and the request
/// cache (its entries are rebuilt on use and count as the hits they are).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn translog_and_request_cache_figures_survive_unload() {
    let store = open_store(search_store::test_objects("server-inst"), "n1", &cache_dir("inst"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/r", r#"{"settings":{"number_of_shards":1}}"#).await;
    for d in 0..3 {
        ok(&n, "c1", "PUT", &format!("/r/_doc/{d}"), r#"{"k":"a"}"#).await;
    }
    ok(&n, "c1", "POST", "/r/_refresh", "").await;
    let agg = r#"{"size":0,"aggs":{"k":{"terms":{"field":"k.keyword"}}}}"#;
    let first = ok(&n, "c1", "POST", "/r/_search?request_cache=true", agg).await;
    ok(&n, "c1", "POST", "/r/_search?request_cache=true", agg).await;
    let figures = |n: Arc<Node>| async move {
        let s = ok(&n, "c1", "GET", "/r/_stats/translog,request_cache", "").await;
        let t = &s["indices"]["r"]["primaries"];
        [
            &t["translog"]["operations"],
            &t["translog"]["uncommitted_operations"],
            &t["request_cache"]["hit_count"],
            &t["request_cache"]["miss_count"],
            &t["request_cache"]["memory_size_in_bytes"],
        ]
        .map(|v| v.as_i64().unwrap_or(-1))
    };
    let before = figures(n.clone()).await;
    assert_eq!(before[..4], [3, 3, 1, 1], "{before:?}");
    assert!(before[4] > 0, "{before:?}");
    assert!(n.unload("c1").await);
    assert_eq!(figures(n.clone()).await, before);
    let again = ok(&n, "c1", "POST", "/r/_search?request_cache=true", agg).await;
    assert_eq!(again["aggregations"], first["aggregations"]);
    let after = figures(n.clone()).await;
    assert_eq!(after, [3, 3, 2, 1, before[4]], "a hit, and the same memory");
    // A refresh that changes the searcher drops the cache; a flush trims the translog.
    ok(&n, "c1", "PUT", "/r/_doc/9", r#"{"k":"b"}"#).await;
    ok(&n, "c1", "POST", "/r/_refresh", "").await;
    ok(&n, "c1", "POST", "/r/_flush", "").await;
    ok(&n, "c1", "POST", "/r/_search?request_cache=true", agg).await;
    assert_eq!(figures(n.clone()).await[..4], [0, 0, 2, 2]);
    assert!(n.unload("c1").await);
    assert_eq!(figures(n.clone()).await[..4], [0, 0, 2, 2]);
}

/// An index without documents has nothing to commit, but its unload still keeps what a running
/// node would report: a `_flush`'s count (its shards opened only for the unload) and search
/// counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_index_keeps_its_statistics_across_unload() {
    let store = open_store(search_store::test_objects("server-empty"), "n1", &cache_dir("empty"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/e", r#"{"settings":{"number_of_shards":2}}"#).await;
    ok(&n, "c1", "PUT", "/q", r#"{"settings":{"number_of_shards":1}}"#).await;
    ok(&n, "c1", "POST", "/e/_flush", "").await;
    ok(&n, "c1", "POST", "/q/_search", "{}").await;
    let figures = |n: Arc<Node>| async move {
        let s = ok(&n, "c1", "GET", "/_stats/flush,search", "").await;
        let i = &s["indices"];
        [&i["e"]["primaries"]["flush"]["total"], &i["q"]["primaries"]["search"]["query_total"]].map(|v| v.as_i64().unwrap_or(-1))
    };
    assert_eq!(figures(n.clone()).await, [2, 1]);
    assert!(n.unload("c1").await);
    assert_eq!(figures(n.clone()).await, [2, 1]);
    assert!(n.unload("c1").await);
    assert_eq!(figures(n.clone()).await, [2, 1]);
    assert_eq!(count(&ok(&n, "c1", "GET", "/e/_count", "").await), 0);
}

/// A merge installed after the last refresh doesn't make unrefreshed writes searchable at the
/// next wake: the commit traces the merged segment back to the segments the searcher showed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrefreshed_writes_stay_unsearchable_across_merges_and_unload() {
    let store = open_store(search_store::test_objects("server-mergeview"), "n1", &cache_dir("mergeview"), 0).await;
    let n = node_on(&store);
    ok(&n, "c1", "PUT", "/m", r#"{"settings":{"number_of_shards":1,"refresh_interval":"-1"}}"#).await;
    // Refresh after each write until a merge finishes after the last refresh: the merged
    // segment is not searchable yet, the searcher still shows the segments it replaced.
    let unpublished_merge = |n: Arc<Node>| async move {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let s = ok(&n, "c1", "GET", "/m/_stats/merge", "").await;
            if s["indices"]["m"]["primaries"]["merges"]["current"].as_i64() == Some(0) {
                break;
            }
            assert!(Instant::now() < deadline, "the merge doesn't finish");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let segs = ok(&n, "c1", "GET", "/m/_segments", "").await;
        let segs = segs["indices"]["m"]["shards"]["0"][0]["segments"].as_object().cloned().unwrap_or_default();
        segs.values().any(|s| s["search"] == false)
    };
    let mut docs = 0;
    loop {
        assert!(docs < 300, "no merge finished after a refresh");
        ok(&n, "c1", "PUT", &format!("/m/_doc/{docs}"), r#"{"f":"v"}"#).await;
        ok(&n, "c1", "POST", "/m/_refresh", "").await;
        docs += 1;
        if unpublished_merge(n.clone()).await {
            break;
        }
    }
    ok(&n, "c1", "PUT", "/m/_doc/late", r#"{"f":"late"}"#).await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/m/_count", "").await), docs);
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/m/_count", "").await), docs, "the late write stays unsearchable");
    ok(&n, "c1", "POST", "/m/_refresh", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/m/_count", "").await), docs + 1);
}

/// Writes `docs` documents to each of `indices` (`shards` shards, unrefreshed: taking a
/// snapshot then has to commit them).
async fn fill(n: &Arc<Node>, cluster: &str, indices: &[String], shards: u32, docs: usize) {
    for idx in indices {
        ok(n, cluster, "PUT", &format!("/{idx}"), &format!(r#"{{"settings":{{"number_of_shards":{shards}}}}}"#)).await;
        let mut bulk = String::new();
        for i in 0..docs {
            bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"f\":\"v{i}\",\"n\":{i}}}\n"));
        }
        let (s, b) = call(n, cluster, "POST", &format!("/{idx}/_bulk"), &bulk).await;
        assert!(s == 200 && b["errors"] == false, "{s}: {b}");
    }
}

/// A snapshot's copies are written before it is recorded: snapshot deletions (and failed
/// snapshots) running meanwhile must not take them for garbage, and a snapshot can't be deleted
/// while it is being restored. Without that, the big snapshot is recorded SUCCESS but restores
/// with documents missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_snapshot_deletes_keep_the_copies_of_snapshots_in_progress() {
    let repo_dir = cache_dir("race-repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let store = open_store(search_store::test_objects("server-snap-race"), "n1", &cache_dir("race"), 0).await;
    let cfg = NodeConfig { node_settings: vec![("path.repo".into(), repo_dir.to_string_lossy().into_owned())], ..NodeConfig::default() };
    let n = Node::new(catalog(), Arc::new(ObjectStores::new(store.clone())), cfg);
    let indices: Vec<String> = (0..10).map(|i| format!("big-{i}")).collect();
    let (shards, docs) = (2, 300);
    fill(&n, "c1", &indices, shards, docs).await;
    ok(&n, "c1", "PUT", "/small/_doc/1", r#"{"f":"x"}"#).await;
    ok(&n, "c1", "PUT", "/_snapshot/r", r#"{"type":"fs","settings":{"location":"r"}}"#).await;
    let total = (indices.len() * docs) as u64;

    for round in 0..3 {
        // Small snapshots taken and deleted over and over while the big one is being taken.
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn = {
            let (n, done) = (n.clone(), done.clone());
            tokio::spawn(async move {
                let mut deletes = 0;
                for i in 0.. {
                    if done.load(Ordering::SeqCst) && deletes > 0 {
                        break;
                    }
                    ok(&n, "c1", "PUT", &format!("/_snapshot/r/tmp-{round}-{i}?wait_for_completion=true"), r#"{"indices":"small"}"#).await;
                    ok(&n, "c1", "DELETE", &format!("/_snapshot/r/tmp-{round}-{i}"), "").await;
                    // A failed snapshot (the name is taken) drops its own copies.
                    let (s, _) = call(&n, "c1", "PUT", &format!("/_snapshot/r/big-{round}?wait_for_completion=true"), r#"{"indices":"small"}"#).await;
                    assert!(s == 200 || s == 400, "{s}");
                    if s == 200 {
                        // It won the name: the big one fails below; take it again under another.
                        ok(&n, "c1", "DELETE", &format!("/_snapshot/r/big-{round}"), "").await;
                    }
                    deletes += 1;
                }
                deletes
            })
        };
        let mut name = format!("big-{round}");
        loop {
            let (s, b) = call(&n, "c1", "PUT", &format!("/_snapshot/r/{name}?wait_for_completion=true"), r#"{"indices":"big-*"}"#).await;
            if s == 200 {
                assert_eq!(b["snapshot"]["state"], "SUCCESS", "{b}");
                break;
            }
            assert_eq!(b["error"]["type"], "invalid_snapshot_name_exception", "{b}");
            name.push('x');
        }
        done.store(true, Ordering::SeqCst);
        assert!(churn.await.unwrap() > 0);

        // Restore it over the deleted indices while deletions of it are attempted: they are
        // refused until the restore finished.
        ok(&n, "c1", "DELETE", &format!("/{}", indices.join(",")), "").await;
        let restore = {
            let (n, name) = (n.clone(), name.clone());
            tokio::spawn(async move { call(&n, "c1", "POST", &format!("/_snapshot/r/{name}/_restore?wait_for_completion=true"), r#"{"indices":"big-*"}"#).await })
        };
        let mut refused = 0;
        if round == 2 {
            loop {
                let (s, b) = call(&n, "c1", "DELETE", &format!("/_snapshot/r/{name}"), "").await;
                if s == 200 {
                    break;
                }
                assert_eq!((s, b["error"]["type"].as_str()), (503, Some("concurrent_snapshot_execution_exception")), "{b}");
                refused += 1;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        let (s, b) = restore.await.unwrap();
        if s == 404 {
            // The deletion came first.
            assert!(round == 2 && refused == 0, "{b}");
            fill(&n, "c1", &indices, shards, docs).await;
            continue;
        }
        assert_eq!(s, 200, "{b}");
        ok(&n, "c1", "POST", "/big-*/_refresh", "").await;
        assert_eq!(count(&ok(&n, "c1", "GET", "/big-*/_count", "").await), total, "round {round}: every document of the snapshot comes back");
    }
}

/// Copies no recorded snapshot uses — a crash between capturing a snapshot and recording it,
/// an index deleted while it was being captured — are dropped by a sweep once they are older
/// than the grace period (after a wake, and after snapshot operations); young ones, and every
/// copy a recorded snapshot uses, stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leftover_snapshot_copies_are_swept_after_the_grace_period() {
    let repo_dir = cache_dir("sweep-repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let store = open_store(search_store::test_objects("server-snap-sweep"), "n1", &cache_dir("sweep"), 0).await;
    let settings = vec![("path.repo".to_string(), repo_dir.to_string_lossy().into_owned())];
    let n = Node::new(catalog(), Arc::new(ObjectStores::new(store.clone())), NodeConfig { node_settings: settings.clone(), ..NodeConfig::default() });
    ok(&n, "c1", "PUT", "/s/_doc/1", r#"{"f":"a"}"#).await;
    ok(&n, "c1", "PUT", "/_snapshot/r", r#"{"type":"fs","settings":{"location":"r"}}"#).await;
    let s1 = ok(&n, "c1", "PUT", "/_snapshot/r/s1?wait_for_completion=true", r#"{"indices":"s"}"#).await;
    let s1_uuid = s1["snapshot"]["uuid"].as_str().unwrap().to_string();
    let pins = store.list_pins("c1").await.unwrap();
    assert_eq!(pins.len(), 1, "{pins:?}");
    let m = store.read_pin("c1", &pins[0]).await.unwrap().unwrap();
    let index_uuid = pins[0].index_uuid.rsplit('.').next().unwrap().to_string();
    // What a crash leaves: a snapshot that was never recorded, and an index a recorded
    // snapshot doesn't include.
    let crashed = search_store::ShardId::new(format!("snap.crashedsnapshotuuid0.{index_uuid}"), 0);
    let stray = search_store::ShardId::new(format!("snap.{s1_uuid}.deletedindexuuid000000"), 0);
    store.write_pin("c1", &crashed, &m).await.unwrap();
    store.write_pin("c1", &stray, &m).await.unwrap();

    // Young leftovers stay through snapshot operations and a wake.
    ok(&n, "c1", "PUT", "/_snapshot/r/s2?wait_for_completion=true", r#"{"indices":"s"}"#).await;
    ok(&n, "c1", "PUT", "/_snapshot/r/tmp?wait_for_completion=true", r#"{"indices":"s"}"#).await;
    ok(&n, "c1", "DELETE", "/_snapshot/r/tmp", "").await;
    assert!(n.unload("c1").await);
    ok(&n, "c1", "GET", "/s/_count", "").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(store.list_pins("c1").await.unwrap().len(), 4, "s1, s2 and both leftovers");
    assert!(n.unload("c1").await);

    // Past the grace period, the sweep after a wake drops them.
    let n2 = Node::new(catalog(), Arc::new(ObjectStores::new(store.clone())), NodeConfig { node_settings: settings, snapshot_leftover_grace: Duration::ZERO, ..NodeConfig::default() });
    ok(&n2, "c1", "GET", "/s/_count", "").await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let left = loop {
        let left = store.list_pins("c1").await.unwrap();
        if left.len() <= 2 || Instant::now() > deadline {
            break left;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(!left.contains(&crashed) && !left.contains(&stray), "{left:?}");
    // The recorded snapshots still restore.
    ok(&n2, "c1", "DELETE", "/s", "").await;
    ok(&n2, "c1", "POST", "/_snapshot/r/s1/_restore?wait_for_completion=true", "").await;
    ok(&n2, "c1", "POST", "/s/_refresh", "").await;
    assert_eq!(count(&ok(&n2, "c1", "GET", "/s/_count", "").await), 1);
}

//! Durable clusters: documents survive unloading, restarts and crashes; branches are isolated;
//! deleted indices leave nothing behind; an idle node does no work.
//!
//! Object storage is S3 when `ROLL_E2E_S3_BUCKET` is set (`search_store::test_objects`), a
//! temp directory otherwise.

mod common;

use search_es::EsRequest;
use search_server::{ClusterSpec, Node, NodeConfig, ObjectStores, StaticCatalog};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn req(cluster: &str, method: &str, uri: &str, body: &str) -> EsRequest {
    let mut r = EsRequest::json(method, uri, body.as_bytes().to_vec());
    r.headers.push(("authorization".into(), common::basic(cluster, "pw")));
    r
}

async fn call(node: &Arc<Node>, cluster: &str, method: &str, uri: &str, body: &str) -> (u16, String) {
    let r = node.handle(req(cluster, method, uri, body)).await;
    (r.status, String::from_utf8_lossy(&r.body).into_owned())
}

async fn ok(node: &Arc<Node>, cluster: &str, method: &str, uri: &str, body: &str) -> serde_json::Value {
    let (s, b) = call(node, cluster, method, uri, body).await;
    assert!(s < 300, "{method} {uri} -> {s}: {b}");
    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null)
}

fn cache_dir(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    search_store::test_dir().join(format!("server-persist-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

async fn open_store(objects: search_store::Objects, node: &str, cache: &std::path::Path, sync_objects: bool) -> search_store::Store {
    let mut cfg = search_store::StoreConfig { node: node.to_string(), verify: Some(search_server::persist::segment_verifier()), ..Default::default() };
    cfg.wal.sync_objects = sync_objects;
    search_store::Store::open(objects, cache, cfg).await.unwrap()
}

fn node_on(store: &search_store::Store, cfg: NodeConfig) -> Arc<Node> {
    let catalog = Arc::new(StaticCatalog::new());
    for c in ["c1", "c2", "prod", "preview", "pit"] {
        catalog.insert(ClusterSpec::new(c).with_password("pw"));
    }
    Node::new(catalog, Arc::new(ObjectStores::new(store.clone())), cfg)
}

fn count(v: &serde_json::Value) -> u64 {
    v["count"].as_u64().unwrap_or_else(|| panic!("no count in {v}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn documents_survive_unload_and_restart() {
    let objects = search_store::test_objects("server-docs");
    let cache = cache_dir("docs");
    let store = open_store(objects.clone(), "n1", &cache, false).await;
    let n = node_on(&store, NodeConfig::default());

    // Field numbers allocate in mapping-growth order: `zeta` before `alpha`, the reverse of
    // the order a fresh walk of the final mapping would give them.
    ok(&n, "c1", "PUT", "/books", r#"{"mappings":{"properties":{"zeta":{"type":"keyword"}}}}"#).await;
    ok(&n, "c1", "PUT", "/books/_doc/1", r#"{"zeta":"z1","title":"rust in action"}"#).await;
    ok(&n, "c1", "PUT", "/books/_doc/2", r#"{"zeta":"z2","alpha":7,"title":"the rust book"}"#).await;
    ok(&n, "c1", "POST", "/books/_bulk", "{\"index\":{\"_id\":\"3\"}}\n{\"alpha\":9,\"title\":\"zig\"}\n{\"delete\":{\"_id\":\"1\"}}\n").await;
    ok(&n, "c1", "POST", "/books/_update/2", r#"{"doc":{"alpha":8}}"#).await;
    let before = ok(&n, "c1", "GET", "/books/_doc/2", "").await;
    assert_eq!(before["_version"], 2);

    for round in 0..3 {
        assert!(n.unload("c1").await, "round {round}");
        assert!(n.loaded().is_empty());
        let after = ok(&n, "c1", "GET", "/books/_doc/2", "").await;
        assert_eq!(after, before, "round {round}");
        assert_eq!(count(&ok(&n, "c1", "GET", "/books/_count", "").await), 2 + round);
        let hits = ok(&n, "c1", "POST", "/books/_search", r#"{"query":{"term":{"zeta":"z2"}}}"#).await;
        assert_eq!(hits["hits"]["total"]["value"], 1, "{hits}");
        let hits = ok(&n, "c1", "POST", "/books/_search", r#"{"query":{"range":{"alpha":{"gte":9}}}}"#).await;
        assert_eq!(hits["hits"]["hits"][0]["_id"], "3", "{hits}");
        let hits = ok(&n, "c1", "POST", "/books/_search", r#"{"query":{"match":{"title":"rust"}}}"#).await;
        assert_eq!(hits["hits"]["total"]["value"], 1, "{hits}");
        // Versions and seq nos continue where they were.
        let r = ok(&n, "c1", "PUT", &format!("/books/_doc/r{round}"), r#"{"alpha":1}"#).await;
        assert_eq!(r["result"], "created");
    }
    // Optimistic concurrency sees the persisted seq no.
    let seq = before["_seq_no"].as_i64().unwrap();
    let (s, b) = call(&n, "c1", "PUT", &format!("/books/_doc/2?if_seq_no={}&if_primary_term=1", seq + 100), r#"{"alpha":1}"#).await;
    assert_eq!(s, 409, "{b}");
    let r = ok(&n, "c1", "PUT", &format!("/books/_doc/2?if_seq_no={seq}&if_primary_term=1"), r#"{"zeta":"z2","alpha":8,"title":"the rust book"}"#).await;
    assert_eq!(r["_version"], 3);

    // A restart of the node (clean shutdown) and another node on the same bucket.
    n.shutdown(Duration::from_secs(5)).await.unwrap();
    drop(n);
    drop(store);
    let store = open_store(objects.clone(), "n1", &cache, false).await;
    let n = node_on(&store, NodeConfig::default());
    assert_eq!(count(&ok(&n, "c1", "GET", "/books/_count", "").await), 5);
    n.shutdown(Duration::from_secs(5)).await.unwrap();
    let other = open_store(objects, "n2", &cache_dir("docs-n2"), false).await;
    let n2 = node_on(&other, NodeConfig::default());
    assert_eq!(count(&ok(&n2, "c1", "GET", "/books/_count", "").await), 5);
    let doc = ok(&n2, "c1", "GET", "/books/_doc/3", "").await;
    assert_eq!(doc["_source"]["title"], "zig");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_are_logged_then_committed() {
    let objects = search_store::test_objects("server-log");
    let store = open_store(objects.clone(), "n1", &cache_dir("log"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/i", r#"{"settings":{"number_of_shards":2}}"#).await;
    let m0 = store.stats().manifests_written;
    for i in 0..20 {
        ok(&n, "c1", "PUT", &format!("/i/_doc/{i}"), r#"{"f":"v"}"#).await;
    }
    let l = n.get_loaded("c1").unwrap();
    let d = l.data.as_ref().unwrap();
    // Every acknowledged write went through the WAL; only attaching wrote manifests.
    assert!(d.records.load(Ordering::Relaxed) >= 20);
    assert_eq!(store.stats().manifests_written - m0, 2, "one attach per shard");
    assert!(store.stats().wal.durable_lsn >= store.stats().wal.last_lsn);
    // `_flush` commits them to object storage.
    ok(&n, "c1", "POST", "/i/_flush", "").await;
    assert_eq!(d.unflushed_log_bytes(), 0);
    let ms = store.read_latest_manifests("c1").await.unwrap();
    assert_eq!(ms.len(), 2);
    assert_eq!(ms.iter().map(|(_, m)| m.segments.iter().map(|s| (s.max_doc - s.del_count) as u64).sum::<u64>()).sum::<u64>(), 20);
    drop(l);
    // Waking replays nothing past those commit points.
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/i/_count", "").await), 20);
    let l = n.get_loaded("c1").unwrap();
    assert_eq!(l.recovery.records, 0);
    assert_eq!(l.recovery.shards, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn async_durability_acknowledges_before_the_commit() {
    let store = open_store(search_store::test_objects("server-async"), "n1", &cache_dir("async"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/a", r#"{"settings":{"index.translog.durability":"async","index.translog.sync_interval":"200ms"}}"#).await;
    // The first write attaches the shard to the WAL; time the next ones.
    ok(&n, "c1", "PUT", "/a/_doc/0", r#"{"f":0}"#).await;
    let l = n.get_loaded("c1").unwrap();
    let d = l.data.clone().unwrap();
    for i in 1..=20 {
        ok(&n, "c1", "PUT", &format!("/a/_doc/{i}"), r#"{"f":1}"#).await;
    }
    // Logged at once (no syncer needed) and acknowledged without waiting for the WAL.
    assert_eq!(d.records.load(Ordering::Relaxed), 21);
    assert!(n.background_tasks() <= 1, "the reaper only: nothing to sync in the background");
    // The WAL commits them within its window anyway.
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.stats().wal.durable_lsn < store.stats().wal.last_lsn {
        assert!(Instant::now() < deadline, "the WAL commits async records");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(l);
    drop(d);
    // Dropped without a flush, the writes come back from the WAL.
    assert!(n.force_unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/a/_count", "").await), 21);
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/a/_count", "").await), 21);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_node_runs_nothing() {
    let store = open_store(search_store::test_objects("server-idle"), "n1", &cache_dir("idle"), false).await;
    let cfg = NodeConfig { idle_unload: Some(Duration::from_millis(100)), ..NodeConfig::default() };
    let n = node_on(&store, cfg);
    let metrics = tokio::runtime::Handle::current().metrics();
    let baseline = metrics.num_alive_tasks();
    for c in ["c1", "c2"] {
        ok(&n, c, "PUT", "/i", r#"{"settings":{"index.translog.durability":"async","index.translog.sync_interval":"100ms"}}"#).await;
        ok(&n, c, "POST", "/i/_bulk?refresh=true", "{\"index\":{}}\n{\"a\":1}\n{\"index\":{}}\n{\"a\":2}\n").await;
    }
    assert!(n.background_tasks() > 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !n.loaded().is_empty() || n.background_tasks() > 0 {
        assert!(Instant::now() < deadline, "clusters unload, tasks end: {} loaded, {} tasks", n.loaded().len(), n.background_tasks());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Give exiting tasks a moment to be reaped by the runtime.
    tokio::time::sleep(Duration::from_millis(50)).await;
    if std::env::var_os("ROLL_E2E_S3_BUCKET").is_none() {
        assert_eq!(metrics.num_alive_tasks(), baseline, "no task outlives the clusters");
    } else {
        // On S3 the object store's HTTP client keeps the connections it opened meanwhile in
        // its pool (one task per idle socket, closed after the pool's idle timeout): sockets,
        // not work. Nothing of ours is among them, and the count holds still.
        let alive = metrics.num_alive_tasks();
        assert!(alive <= baseline + 16, "{alive} tasks alive, {baseline} before");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(metrics.num_alive_tasks() <= alive);
    }
    // The WAL is quiet too: nothing commits while nothing is loaded.
    let commits = store.stats().wal.commits;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(store.stats().wal.commits, commits);
    assert_eq!(n.unloads.load(Ordering::Relaxed), 2);
    // Everything was committed on unload: the staging directory holds nothing of theirs.
    assert_eq!(store.stats().staging_files, 0);
    assert_eq!(count(&ok(&n, "c2", "GET", "/i/_count", "").await), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wake_and_unload_cycles_under_load() {
    let store = open_store(search_store::test_objects("server-cycles"), "n1", &cache_dir("cycles"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/load", r#"{"settings":{"number_of_shards":3}}"#).await;
    let acked = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut writers = Vec::new();
    for w in 0..4 {
        let (n, acked, stop) = (n.clone(), acked.clone(), stop.clone());
        writers.push(tokio::spawn(async move {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let body = if i.is_multiple_of(5) {
                    // A bulk of three.
                    format!(
                        "{{\"index\":{{\"_id\":\"{w}-{i}-a\"}}}}\n{{\"v\":{i}}}\n{{\"index\":{{\"_id\":\"{w}-{i}-b\"}}}}\n{{\"v\":{i}}}\n{{\"index\":{{\"_id\":\"{w}-{i}-c\"}}}}\n{{\"v\":{i}}}\n"
                    )
                } else {
                    String::new()
                };
                let (s, b) = if body.is_empty() {
                    call(&n, "c1", "PUT", &format!("/load/_doc/{w}-{i}"), &format!(r#"{{"v":{i}}}"#)).await
                } else {
                    call(&n, "c1", "POST", "/load/_bulk", &body).await
                };
                if s < 300 {
                    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                    acked.fetch_add(if body.is_empty() { 1 } else { v["items"].as_array().unwrap().len() as u64 }, Ordering::Relaxed);
                } else {
                    panic!("write failed: {s} {b}");
                }
                i += 1;
            }
        }));
    }
    let mut unloads = 0;
    let started = Instant::now();
    // At least 3 s and 50 acknowledged writes (a loaded machine or a slow store is slow).
    while started.elapsed() < Duration::from_secs(3) || (acked.load(Ordering::Relaxed) <= 50 && started.elapsed() < Duration::from_secs(60)) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if n.unload("c1").await {
            unloads += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.await.unwrap();
    }
    assert!(unloads > 0 || n.wakes.load(Ordering::Relaxed) > 0);
    let total = acked.load(Ordering::Relaxed);
    assert!(total > 50, "only {total} writes");
    ok(&n, "c1", "POST", "/load/_refresh", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/load/_count", "").await), total);
    while !n.unload("c1").await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(count(&ok(&n, "c1", "GET", "/load/_count", "").await), total);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_running_when_a_cluster_is_dropped_are_never_acknowledged() {
    let objects = search_store::test_objects("server-dropped");
    let store = open_store(objects, "n1", &cache_dir("dropped"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/i/_doc/0", r#"{"f":0}"#).await;
    // Admitted, then the cluster is dropped under it (lost lease, failed storage, a shutdown
    // that timed out) before it runs: it must not be acknowledged.
    let dropped = |method: &'static str, uri: &'static str, body: &'static str| {
        let n = n.clone();
        async move {
            let r = req("c1", method, uri, body);
            let lease = n.admit(&r, None).await.unwrap();
            assert!(n.force_unload("c1").await);
            let resp = lease.handle(r).await;
            (resp.status, String::from_utf8_lossy(&resp.body).into_owned())
        }
    };
    let (s, b) = dropped("PUT", "/i/_doc/1", r#"{"f":1}"#).await;
    assert_eq!(s, 503, "{b}");
    assert!(b.contains("cluster_block_exception") && b.contains("unloaded"), "{b}");
    let (s, b) = dropped("POST", "/i/_bulk", "{\"index\":{\"_id\":\"2\"}}\n{\"f\":2}\n").await;
    assert!(s >= 500 || b.contains("\"errors\":true"), "{s} {b}");
    let (s, b) = dropped("POST", "/i/_flush", "").await;
    assert!(s >= 500, "{s} {b}");
    // A state change on the dropped instance: 503 like a cluster that is not ready, not 500.
    let (s, b) = dropped("PUT", "/j", "{}").await;
    assert_eq!(s, 503, "{b}");
    assert!(b.contains("cluster_block_exception"), "{b}");
    // Nothing of it is there after the next wake; what was acknowledged is.
    assert_eq!(call(&n, "c1", "GET", "/i/_doc/0", "").await.0, 200);
    assert_eq!(call(&n, "c1", "GET", "/i/_doc/1", "").await.0, 404);
    assert_eq!(call(&n, "c1", "GET", "/i/_doc/2", "").await.0, 404);
    assert_eq!(call(&n, "c1", "HEAD", "/j", "").await.0, 404);
    // A write whose record is in the log before the drop is acknowledged by it.
    ok(&n, "c1", "PUT", "/i/_doc/3", r#"{"f":3}"#).await;
    assert!(n.force_unload("c1").await);
    assert_eq!(call(&n, "c1", "GET", "/i/_doc/3", "").await.0, 200);
    n.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_are_isolated() {
    let store = open_store(search_store::test_objects("server-branch"), "n1", &cache_dir("branch"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "prod", "PUT", "/p", r#"{"mappings":{"properties":{"k":{"type":"keyword"}}}}"#).await;
    ok(&n, "prod", "POST", "/p/_bulk", "{\"index\":{\"_id\":\"1\"}}\n{\"k\":\"a\"}\n{\"index\":{\"_id\":\"2\"}}\n{\"k\":\"b\"}\n{\"index\":{\"_id\":\"3\"}}\n{\"k\":\"c\"}\n").await;
    ok(&n, "prod", "DELETE", "/p/_doc/3", "").await;
    // The parent is loaded and holds writes only in its WAL: branching commits it first.
    n.branch("prod", "preview").await.unwrap();
    assert_eq!(count(&ok(&n, "preview", "GET", "/p/_count", "").await), 2);
    // Each side writes on its own.
    ok(&n, "preview", "DELETE", "/p/_doc/1", "").await;
    ok(&n, "preview", "PUT", "/p/_doc/9", r#"{"k":"preview"}"#).await;
    ok(&n, "prod", "PUT", "/p/_doc/2", r#"{"k":"b2"}"#).await;
    ok(&n, "prod", "PUT", "/p/_doc/7", r#"{"k":"prod"}"#).await;
    for _ in 0..2 {
        let ids = |v: serde_json::Value| -> Vec<String> {
            let mut ids: Vec<String> = v["hits"]["hits"].as_array().unwrap().iter().map(|h| format!("{}={}", h["_id"].as_str().unwrap(), h["_source"]["k"].as_str().unwrap())).collect();
            ids.sort();
            ids
        };
        let q = r#"{"query":{"match_all":{}},"size":20}"#;
        ok(&n, "prod", "POST", "/p/_refresh", "").await;
        ok(&n, "preview", "POST", "/p/_refresh", "").await;
        assert_eq!(ids(ok(&n, "prod", "POST", "/p/_search", q).await), vec!["1=a", "2=b2", "7=prod"]);
        assert_eq!(ids(ok(&n, "preview", "POST", "/p/_search", q).await), vec!["2=b", "9=preview"]);
        // And again after both slept.
        assert!(n.unload("prod").await);
        assert!(n.unload("preview").await);
    }
    // Deleting the branch leaves the parent's (shared) segments alone.
    n.delete_cluster("preview").await.unwrap();
    assert_eq!(count(&ok(&n, "prod", "GET", "/p/_count", "").await), 3);
    let (s, _) = call(&n, "preview", "GET", "/p/_count", "").await;
    assert_eq!(s, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_indices_leave_nothing_behind() {
    let store = open_store(search_store::test_objects("server-delidx"), "n1", &cache_dir("delidx"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/gone/_doc/1", r#"{"a":1}"#).await;
    ok(&n, "c1", "PUT", "/kept/_doc/1", r#"{"a":1}"#).await;
    assert_eq!(store.list_shards("c1").await.unwrap().len(), 2);
    ok(&n, "c1", "DELETE", "/gone", "").await;
    assert_eq!(store.list_shards("c1").await.unwrap().len(), 1);
    // Re-created under the same name: a new, empty index.
    ok(&n, "c1", "PUT", "/gone", "{}").await;
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/gone/_count", "").await), 0);
    assert_eq!(count(&ok(&n, "c1", "GET", "/kept/_count", "").await), 1);
}

/// A change the engine accepts but couldn't load again is refused before it reaches object
/// storage: the cluster keeps waking, with its documents and last good mapping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unloadable_states_are_never_saved() {
    let store = open_store(search_store::test_objects("server-unloadable"), "n1", &cache_dir("unloadable"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/v", r#"{"mappings":{"properties":{"v":{"type":"dense_vector","similarity":"l2_norm","index_options":{"type":"int4_hnsw"}}}}}"#).await;
    ok(&n, "c1", "PUT", "/v/_doc/1", r#"{"other":"x"}"#).await;
    // Odd dims for int4_hnsw, taken from the document.
    let (s, b) = call(&n, "c1", "PUT", "/v/_doc/2", r#"{"v":[1,2,3]}"#).await;
    assert!(s >= 400, "{s} {b}");
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/v/_count", "").await), 1);
    let m = ok(&n, "c1", "GET", "/v/_mapping", "").await;
    assert!(m["v"]["mappings"]["properties"]["v"].get("dims").is_none(), "{m}");
    ok(&n, "c1", "PUT", "/v/_doc/3?refresh=true", r#"{"v":[1,2,3,4]}"#).await;
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/v/_count", "").await), 2);
    n.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tenants_are_isolated() {
    let store = open_store(search_store::test_objects("server-tenants"), "n1", &cache_dir("tenants"), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/same/_doc/1?refresh=true", r#"{"owner":"c1"}"#).await;
    ok(&n, "c2", "PUT", "/same/_doc/2?refresh=true", r#"{"owner":"c2"}"#).await;
    for _ in 0..2 {
        let a = ok(&n, "c1", "POST", "/same/_search", "{}").await;
        let b = ok(&n, "c2", "POST", "/same/_search", "{}").await;
        assert_eq!(a["hits"]["hits"].as_array().unwrap().len(), 1);
        assert_eq!(a["hits"]["hits"][0]["_source"]["owner"], "c1");
        assert_eq!(b["hits"]["hits"][0]["_source"]["owner"], "c2");
        let (s, _) = call(&n, "c1", "GET", "/same/_doc/2", "").await;
        assert_eq!(s, 404);
        assert!(n.unload("c1").await && n.unload("c2").await);
    }
    // A wrong password for c2 doesn't reach c2's data (and looks like any failed login).
    let mut r = EsRequest::new("GET", "/same/_doc/2");
    r.headers.push(("authorization".into(), common::basic("c2", "pw-of-c1")));
    assert_eq!(n.handle(r).await.status, 401);
    // Usage is per tenant.
    let u1 = n.usage("c1").await.unwrap();
    assert!(!u1.loaded);
    assert_eq!(u1.docs, 1, "{u1:?} {:?}", store.read_latest_manifests("c1").await.unwrap());
    ok(&n, "c1", "GET", "/", "").await;
    let u1 = n.usage("c1").await.unwrap();
    assert!(u1.loaded && u1.docs == 1 && u1.store_bytes > 0 && u1.requests >= 1, "{u1:?}");
}

/// A node with an empty cache wakes a cluster whose segment is larger than one fetch part
/// without waiting for the whole download: the shard opens on the segment's metadata and
/// queries, gets and writes wait only for what they read.
/// Writes an index with one segment larger than a fetch part on a first node, then returns the
/// objects, the doc count and one document as written.
async fn big_index(tag: &str) -> (search_store::Objects, usize, serde_json::Value) {
    let objects = search_store::test_objects(&format!("server-{tag}"));
    let store = open_store(objects.clone(), "n1", &cache_dir(tag), false).await;
    let n = node_on(&store, NodeConfig::default());
    ok(&n, "c1", "PUT", "/big", r#"{"settings":{"number_of_shards":1},"mappings":{"properties":{"tag":{"type":"keyword"},"n":{"type":"long"},"blob":{"type":"keyword","index":false,"doc_values":false}}}}"#).await;
    // ~14 MB of stored source at 6 bits per character (~10 MiB compressed), in a few bulks.
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let docs = 14_000;
    for chunk in 0..7 {
        let mut body = String::new();
        for i in chunk * (docs / 7)..(chunk + 1) * (docs / 7) {
            let blob: String = (0..1000).map(|_| ALPHABET[(next() >> 58) as usize] as char).collect();
            body.push_str(&format!("{{\"index\":{{\"_id\":\"d{i}\"}}}}\n{{\"tag\":\"t{}\",\"n\":{i},\"title\":\"doc number {i}\",\"blob\":\"{blob}\"}}\n", i % 10));
        }
        ok(&n, "c1", "POST", "/big/_bulk", &body).await;
    }
    ok(&n, "c1", "POST", "/big/_forcemerge?max_num_segments=1", "").await;
    let want = ok(&n, "c1", "GET", "/big/_doc/d4321", "").await;
    n.shutdown(Duration::from_secs(10)).await.unwrap();
    drop(n);
    let segs = ok_usage(&store).await;
    assert!(segs > search_store::cache::FETCH_PART, "the index must have a segment over one fetch part: {segs} bytes");
    drop(store);
    (objects, docs, want)
}

/// A second node with an empty cache wakes the index while its big segment downloads.
async fn wake_while_downloading(tag: &str, objects: search_store::Objects, docs: usize, want: serde_json::Value) {
    let other = open_store(objects, "n2", &cache_dir(&format!("{tag}-n2")), false).await;
    let n2 = node_on(&other, NodeConfig::default());
    assert_eq!(count(&ok(&n2, "c1", "GET", "/big/_count", "").await), docs as u64);
    let loaded = n2.get_loaded("c1").unwrap();
    assert!(loaded.recovery.fetching >= 1, "{:?}", loaded.recovery);
    // These read postings and stored fields that may still be on their way.
    let hits = ok(&n2, "c1", "POST", "/big/_search", r#"{"query":{"term":{"tag":"t3"}},"size":0}"#).await;
    assert_eq!(hits["hits"]["total"]["value"], docs / 10);
    let hits = ok(&n2, "c1", "POST", "/big/_search", r#"{"query":{"match":{"title":"4321"}}}"#).await;
    assert_eq!(hits["hits"]["hits"][0]["_id"], "d4321", "{hits}");
    assert_eq!(ok(&n2, "c1", "GET", "/big/_doc/d4321", "").await, want);
    // Writes, a flush and a wake from the commit work on top of it.
    ok(&n2, "c1", "DELETE", "/big/_doc/d0", "").await;
    ok(&n2, "c1", "PUT", "/big/_doc/new", r#"{"tag":"t3","n":-1}"#).await;
    ok(&n2, "c1", "POST", "/big/_flush", "").await;
    assert!(n2.unload("c1").await);
    assert_eq!(count(&ok(&n2, "c1", "GET", "/big/_count?q=tag:t3", "").await), docs as u64 / 10 + 1);
    assert_eq!(count(&ok(&n2, "c1", "GET", "/big/_count", "").await), docs as u64);
    n2.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_segments_open_while_downloading() {
    let (objects, docs, want) = big_index("progressive").await;
    wake_while_downloading("progressive", objects, docs, want).await;
}

async fn ok_usage(store: &search_store::Store) -> u64 {
    store.cluster_usage("c1").await.unwrap().segment_bytes
}

/// A cluster over its own `max_resident_bytes` is committed at the next check: its indexing
/// buffers become segment files, and nothing is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clusters_over_their_memory_budget_are_committed() {
    let objects = search_store::test_objects("server-membudget");
    let store = open_store(objects, "n1", &cache_dir("membudget"), false).await;
    let catalog = Arc::new(StaticCatalog::new());
    let mut spec = ClusterSpec::new("c1").with_password("pw");
    // 3000 small documents take about 60 KiB in indexing buffers.
    spec.max_resident_bytes = Some(16 << 10);
    catalog.insert(spec);
    let n = Node::new(catalog, Arc::new(ObjectStores::new(store.clone())), NodeConfig::default());
    // Writes stay in indexing buffers (only the WAL record is durable) until a commit.
    ok(&n, "c1", "PUT", "/m", r#"{"settings":{"number_of_shards":1,"index.translog.durability":"async","index.translog.sync_interval":"60s"}}"#).await;
    let mut body = String::new();
    for i in 0..3000 {
        body.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"title\":\"some words for document {i}\",\"n\":{i}}}\n"));
    }
    ok(&n, "c1", "POST", "/m/_bulk", &body).await;
    let before = n.usage("c1").await.unwrap();
    assert!(before.resident_bytes > 16 << 10, "{before:?}");
    let commits = n.get_loaded("c1").unwrap().data.as_ref().unwrap().full_commits.load(Ordering::Relaxed);
    assert!(n.enforce_memory(0).await.is_empty(), "no node budget: nothing unloaded");
    let after = n.usage("c1").await.unwrap();
    assert!(after.resident_bytes < before.resident_bytes, "{before:?} -> {after:?}");
    assert!(n.get_loaded("c1").unwrap().data.as_ref().unwrap().full_commits.load(Ordering::Relaxed) > commits);
    // Committing doesn't publish (no refresh happened yet); after one, everything is there.
    ok(&n, "c1", "POST", "/m/_refresh", "").await;
    assert_eq!(count(&ok(&n, "c1", "GET", "/m/_count", "").await), 3000);
    assert!(n.unload("c1").await);
    assert_eq!(count(&ok(&n, "c1", "GET", "/m/_count", "").await), 3000);
    n.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_budgeted_cluster_is_checked_soon_even_when_the_reaper_idles_slowly() {
    let objects = search_store::test_objects("server-membudget-kick");
    let store = open_store(objects, "n1", &cache_dir("membudget-kick"), false).await;
    let catalog = Arc::new(StaticCatalog::new());
    catalog.insert(ClusterSpec::new("plain").with_password("pw"));
    let mut spec = ClusterSpec::new("c1").with_password("pw");
    spec.max_resident_bytes = Some(16 << 10);
    catalog.insert(spec);
    // The default idle timeout (300 s) makes the reaper tick every 15 s.
    let n = Node::new(catalog, Arc::new(ObjectStores::new(store.clone())), NodeConfig::default());
    ok(&n, "plain", "GET", "/", "").await;
    assert!(n.reaper_running());
    ok(&n, "c1", "PUT", "/m", r#"{"settings":{"number_of_shards":1,"index.translog.durability":"async","index.translog.sync_interval":"60s"}}"#).await;
    let mut body = String::new();
    for i in 0..3000 {
        body.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"title\":\"some words for document {i}\",\"n\":{i}}}\n"));
    }
    ok(&n, "c1", "POST", "/m/_bulk", &body).await;
    let data = n.get_loaded("c1").unwrap().data.clone().unwrap();
    let commits = data.full_commits.load(Ordering::Relaxed);
    // Checked within the memory-check period (2 s), not at the next 15 s idle tick.
    tokio::time::timeout(Duration::from_secs(6), async {
        while data.full_commits.load(Ordering::Relaxed) == commits {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the reaper re-planned its tick for the budgeted cluster");
    n.shutdown(Duration::from_secs(5)).await.unwrap();
}

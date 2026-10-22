//! Measurements (ignored by default; run with
//! `cargo test --release -p search-server --test bench -- --ignored --nocapture --test-threads=1`):
//! cold wake latency of a small cluster with its segments cached and uncached, RSS of an idle
//! node (a `search-dev --data-dir` process) and RSS per loaded small cluster.

mod common;

use search_es::EsRequest;
use search_server::{ClusterSpec, Node, NodeConfig, ObjectStores, StaticCatalog};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn req(cluster: &str, method: &str, uri: &str, body: &str) -> EsRequest {
    let mut r = EsRequest::json(method, uri, body.as_bytes().to_vec());
    r.headers.push(("authorization".into(), common::basic(cluster, "pw")));
    r
}

async fn ok(n: &Arc<Node>, cluster: &str, method: &str, uri: &str, body: &str) {
    let r = n.handle(req(cluster, method, uri, body)).await;
    assert!(r.status < 300, "{method} {uri}: {} {}", r.status, String::from_utf8_lossy(&r.body));
}

fn rss_kib(pid: &str) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)).unwrap().parse().unwrap()
}

async fn store(objects: &search_store::Objects, cache: &std::path::Path) -> search_store::Store {
    let cfg = search_store::StoreConfig { verify: Some(search_server::persist::segment_verifier()), ..Default::default() };
    search_store::Store::open(objects.clone(), cache, cfg).await.unwrap()
}

fn node(store: &search_store::Store, clusters: usize) -> Arc<Node> {
    let catalog = Arc::new(StaticCatalog::new());
    for i in 0..clusters {
        catalog.insert(ClusterSpec::new(format!("c{i}")).with_password("pw"));
    }
    Node::new(catalog, Arc::new(ObjectStores::new(store.clone())), NodeConfig { idle_unload: None, ..NodeConfig::default() })
}

fn bulk(docs: usize) -> String {
    let mut b = String::new();
    for i in 0..docs {
        b.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"title\":\"small document number {i}\",\"n\":{i},\"tag\":\"t{}\"}}\n", i % 10));
    }
    b
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cold_wake_latency() {
    let dir = tempfile::tempdir().unwrap();
    let objects = search_store::Objects::new(&search_store::StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let s = store(&objects, &dir.path().join("cache-0")).await;
    let n = node(&s, 1);
    ok(&n, "c0", "PUT", "/small", r#"{"settings":{"number_of_shards":1},"mappings":{"properties":{"tag":{"type":"keyword"}}}}"#).await;
    ok(&n, "c0", "POST", "/small/_bulk", &bulk(1000)).await;
    ok(&n, "c0", "PUT", "/other", "{}").await;
    ok(&n, "c0", "POST", "/other/_bulk", &bulk(100)).await;
    assert!(n.unload("c0").await);
    // Cached: segments in this node's cache.
    let mut cached = Vec::new();
    let mut engine = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        ok(&n, "c0", "GET", "/small/_count", "").await;
        cached.push(t.elapsed());
        engine.push(n.get_loaded("c0").unwrap().wake_time);
        assert!(n.unload("c0").await);
    }
    // Uncached: a node with an empty cache (a new or replaced node) downloads every segment.
    n.shutdown(Duration::from_secs(5)).await.unwrap();
    drop(n);
    drop(s);
    let mut uncached = Vec::new();
    for i in 0..10 {
        let s = store(&objects, &dir.path().join(format!("cache-u{i}"))).await;
        let n = node(&s, 1);
        let t = Instant::now();
        ok(&n, "c0", "GET", "/small/_count", "").await;
        uncached.push(t.elapsed());
        n.shutdown(Duration::from_secs(5)).await.unwrap();
    }
    println!(
        "cold wake (2 indices, 1100 docs, local objects): cached median {:?} (wake alone {:?}), uncached median {:?}",
        median(cached),
        median(engine),
        median(uncached)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn rss_per_loaded_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let objects = search_store::Objects::new(&search_store::StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let s = store(&objects, &dir.path().join("cache")).await;
    const K: usize = 50;
    let n = node(&s, K);
    let body = bulk(200);
    for i in 0..K {
        let c = format!("c{i}");
        ok(&n, &c, "PUT", "/docs", "{}").await;
        ok(&n, &c, "POST", "/docs/_bulk?refresh=true", &body).await;
        assert!(n.unload(&c).await);
    }
    unsafe { libmimalloc_sys_collect() };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = rss_kib("self");
    for i in 0..K {
        let c = format!("c{i}");
        ok(&n, &c, "POST", "/docs/_search", r#"{"query":{"match":{"title":"document"}}}"#).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = rss_kib("self");
    let mut resident = 0;
    for i in 0..K {
        resident += n.usage(&format!("c{i}")).await.unwrap().resident_bytes;
    }
    println!(
        "RSS per loaded small cluster (1 index, 200 docs, searched once): {} KiB (engine-resident {} KiB)",
        (after.saturating_sub(before)) / K as u64,
        resident / 1024 / K as u64
    );
    for i in 0..K {
        assert!(n.unload(&format!("c{i}")).await);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("RSS after unloading them all: {} KiB (before loading: {before} KiB)", rss_kib("self"));
}

unsafe fn libmimalloc_sys_collect() {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn idle_node_rss() {
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_search-dev"))
        .args(["--listen", &format!("127.0.0.1:{port}"), "--idle-unload", "1", "--data-dir"])
        .arg(dir.path())
        .env("RUST_LOG", "warn")
        .spawn()
        .unwrap();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let pid = child.id().to_string();
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let fresh = rss_kib(&pid);
    // Load the cluster with a little data, then let it unload.
    let body = bulk(1000);
    let r = common::send(addr, "POST", "/d/_bulk", &[("Content-Type", "application/x-ndjson")], body.as_bytes()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = common::send(addr, "POST", "/d/_search", &[("Content-Type", "application/json")], br#"{"query":{"match":{"title":"small"}}}"#).await;
    assert_eq!(r.status, 200);
    let loaded = rss_kib(&pid);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let idle = rss_kib(&pid);
    println!("search-dev RSS: started {fresh} KiB, with a loaded cluster {loaded} KiB, after idle unload {idle} KiB");
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Indexing throughput with every write durable (WAL cut records) against the same node in
/// memory: single-document writes from 1 and 16 clients, and 1000-document bulks.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn durable_indexing_throughput() {
    async fn run(n: &Arc<Node>, label: &str) {
        let c = "c0";
        ok(n, c, "PUT", "/t", r#"{"mappings":{"properties":{"tag":{"type":"keyword"}}}}"#).await;
        let docs = 2000;
        let t = Instant::now();
        for i in 0..docs {
            ok(n, c, "PUT", &format!("/t/_doc/s{i}"), &format!(r#"{{"title":"doc {i}","tag":"x","n":{i}}}"#)).await;
        }
        let one = docs as f64 / t.elapsed().as_secs_f64();
        let t = Instant::now();
        let mut tasks = Vec::new();
        for w in 0..16 {
            let n = n.clone();
            tasks.push(tokio::spawn(async move {
                for i in 0..500 {
                    ok(&n, "c0", "PUT", &format!("/t/_doc/p{w}-{i}"), &format!(r#"{{"title":"doc {i}","tag":"y","n":{i}}}"#)).await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let sixteen = 8000.0 / t.elapsed().as_secs_f64();
        let body = bulk(1000);
        let t = Instant::now();
        for _ in 0..20 {
            ok(n, c, "POST", "/t/_bulk", &body).await;
        }
        let bulk_rate = 20_000.0 / t.elapsed().as_secs_f64();
        println!("{label}: single docs {one:.0}/s (1 client), {sixteen:.0}/s (16 clients); bulk {bulk_rate:.0} docs/s");
    }
    let dir = tempfile::tempdir().unwrap();
    let objects = search_store::Objects::new(&search_store::StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let s = store(&objects, &dir.path().join("cache")).await;
    run(&node(&s, 1), "durable (WAL fsync per group commit)").await;
    let catalog = Arc::new(StaticCatalog::new());
    catalog.insert(ClusterSpec::new("c0").with_password("pw"));
    let mem = Node::new(catalog, Arc::new(search_server::MemoryStores::new()), NodeConfig::default());
    run(&mem, "in memory").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn store_open_memory() {
    let dir = tempfile::tempdir().unwrap();
    let before = rss_kib("self");
    let objects = search_store::Objects::new(&search_store::StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let after_objects = rss_kib("self");
    let s = store(&objects, &dir.path().join("cache")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_store = rss_kib("self");
    let n = node(&s, 1);
    ok(&n, "c0", "PUT", "/x/_doc/1", r#"{"a":1}"#).await;
    assert!(n.unload("c0").await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_cycle = rss_kib("self");
    println!("RSS KiB: start {before}, objects +{}, store +{}, one load/unload cycle +{}", after_objects - before, after_store - after_objects, after_cycle.saturating_sub(after_store));
}

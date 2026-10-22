//! Two nodes on one bucket, ownership through leases: one owner at a time, the other is told
//! who owns the cluster; when the owner loses contact it stops acknowledging writes before the
//! lease can pass, the next owner recovers everything acknowledged, and a deposed node that
//! still believes it owns the cluster can't commit (epoch fencing in search-store).

mod common;

use async_trait::async_trait;
use search_es::EsRequest;
use search_server::lease::{Leases, MemoryLeases, Owner};
use search_server::{ClusterSpec, Node, NodeConfig, ObjectStores, StaticCatalog};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A lease store whose renewals can be cut off (a partitioned node) or faked (a node that
/// wrongly believes it still owns the cluster).
struct Flaky {
    inner: Arc<MemoryLeases>,
    cut: AtomicBool,
    lie: AtomicBool,
}

#[async_trait]
impl Leases for Flaky {
    async fn acquire(&self, cluster: &str, node: &str, ttl: Duration) -> anyhow::Result<Result<u64, Owner>> {
        anyhow::ensure!(!self.cut.load(Ordering::SeqCst), "partitioned");
        self.inner.acquire(cluster, node, ttl).await
    }
    async fn renew(&self, cluster: &str, node: &str, epoch: u64, ttl: Duration) -> anyhow::Result<bool> {
        if self.lie.load(Ordering::SeqCst) {
            return Ok(true);
        }
        anyhow::ensure!(!self.cut.load(Ordering::SeqCst), "partitioned");
        self.inner.renew(cluster, node, epoch, ttl).await
    }
    async fn release(&self, cluster: &str, node: &str, epoch: u64) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cut.load(Ordering::SeqCst), "partitioned");
        self.inner.release(cluster, node, epoch).await
    }
    async fn owner(&self, cluster: &str) -> anyhow::Result<Option<Owner>> {
        self.inner.owner(cluster).await
    }
}

fn cache_dir(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    search_store::test_dir().join(format!("server-fence-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

async fn node(objects: &search_store::Objects, id: &str, leases: Arc<dyn Leases>, ttl: Duration) -> Arc<Node> {
    let mut cfg = search_store::StoreConfig { node: id.to_string(), ..Default::default() };
    // Ephemeral disks: acknowledge once the WAL is in object storage, so the next owner sees it.
    cfg.wal.sync_objects = true;
    let store = search_store::Store::open(objects.clone(), cache_dir(id), cfg).await.unwrap();
    let catalog = Arc::new(StaticCatalog::new());
    catalog.insert(ClusterSpec::new("shop").with_password("pw"));
    let n = Node::new(catalog, Arc::new(ObjectStores::new(store)), NodeConfig { node_name: id.into(), lease_ttl: ttl, ..NodeConfig::default() });
    n.set_leases(leases);
    n
}

async fn call(n: &Arc<Node>, method: &str, uri: &str, body: &str) -> (u16, serde_json::Value, Vec<(String, String)>) {
    let mut r = EsRequest::json(method, uri, body.as_bytes().to_vec());
    r.headers.push(("authorization".into(), common::basic("shop", "pw")));
    let resp = n.handle(r).await;
    (resp.status, serde_json::from_slice(&resp.body).unwrap_or(serde_json::Value::Null), resp.headers)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_owner_at_a_time_and_deposed_nodes_cannot_commit() {
    let objects = search_store::test_objects("server-fencing");
    let shared = MemoryLeases::new();
    let ttl = Duration::from_millis(900);
    let flaky = Arc::new(Flaky { inner: shared.clone(), cut: AtomicBool::new(false), lie: AtomicBool::new(false) });
    let a = node(&objects, "node-a", flaky.clone(), ttl).await;
    let b = node(&objects, "node-b", shared.clone(), ttl).await;

    let (s, v, _) = call(&a, "PUT", "/orders", r#"{"settings":{"number_of_shards":2}}"#).await;
    assert_eq!(s, 200, "{v}");
    let mut acked = 0;
    for i in 0..10 {
        let (s, v, _) = call(&a, "PUT", &format!("/orders/_doc/{i}"), r#"{"total":1}"#).await;
        assert_eq!(s, 201, "{v}");
        acked += 1;
    }
    assert_eq!(a.get_loaded("shop").unwrap().epoch(), 1);
    // B is refused and told the owner.
    let (s, v, h) = call(&b, "GET", "/orders/_count", "").await;
    assert_eq!(s, 503, "{v}");
    assert!(h.iter().any(|(k, v)| k == "X-Roll-Search-Owner" && v == "node-a"), "{h:?}");

    // A is partitioned from the lease store: within 0.8 × ttl it stops acknowledging writes.
    flaky.cut.store(true, Ordering::SeqCst);
    let cut_at = Instant::now();
    let mut refused = None;
    while cut_at.elapsed() < ttl * 2 {
        let (s, _, _) = call(&a, "PUT", "/orders/_doc/late", r#"{"total":2}"#).await;
        if s >= 500 {
            refused = Some(cut_at.elapsed());
            break;
        }
        assert!(cut_at.elapsed() < ttl, "acknowledged a write after the lease could have passed");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(refused.is_some_and(|t| t < ttl), "A must stop acknowledging before the lease expires: {refused:?}");

    // Once the lease has expired, B takes over with a newer epoch and has every acknowledged doc.
    tokio::time::sleep(ttl).await;
    let (s, v, _) = call(&b, "POST", "/orders/_refresh", "").await;
    assert_eq!(s, 200, "{v}");
    let (s, v, _) = call(&b, "GET", "/orders/_count", "").await;
    assert_eq!(s, 200, "{v}");
    assert!(v["count"].as_u64().unwrap() >= acked, "{v}");
    assert_eq!(b.get_loaded("shop").unwrap().epoch(), 2);
    for i in 0..10 {
        let (s, _, _) = call(&b, "GET", &format!("/orders/_doc/{i}"), "").await;
        assert_eq!(s, 200, "doc {i}");
    }
    let (s, _, _) = call(&b, "PUT", "/orders/_doc/b1", r#"{"total":3}"#).await;
    assert_eq!(s, 201);
    let (s, _, _) = call(&b, "POST", "/orders/_flush", "").await;
    assert_eq!(s, 200);
    // A dropped the cluster when it couldn't acknowledge.
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.get_loaded("shop").is_some() {
        assert!(Instant::now() < deadline, "A unloads the cluster it lost");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_wrongly_believes_it_owns_a_cluster_is_fenced() {
    let objects = search_store::test_objects("server-fencing-split");
    let shared = MemoryLeases::new();
    let ttl = Duration::from_millis(600);
    let flaky = Arc::new(Flaky { inner: shared.clone(), cut: AtomicBool::new(false), lie: AtomicBool::new(false) });
    let a = node(&objects, "node-a", flaky.clone(), ttl).await;
    let b = node(&objects, "node-b", shared.clone(), ttl).await;
    let (s, _, _) = call(&a, "PUT", "/t/_doc/1", r#"{"x":1}"#).await;
    assert_eq!(s, 201);
    // A's renewals stop reaching the store but it is told they succeed (split brain).
    flaky.lie.store(true, Ordering::SeqCst);
    // Make A's lease look lapsed to the store: release it behind A's back.
    shared.release("shop", "node-a", 1).await.unwrap();
    let (s, v, _) = call(&b, "PUT", "/t/_doc/2", r#"{"x":2}"#).await;
    assert_eq!(s, 201, "{v}");
    let (s, _, _) = call(&b, "POST", "/t/_flush", "").await;
    assert_eq!(s, 200);
    // A's commit point would overwrite B's: the store refuses it, and A drops the cluster.
    let (s, v, _) = call(&a, "POST", "/t/_flush", "").await;
    assert!(s >= 500, "a deposed owner's flush must fail: {s} {v}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.get_loaded("shop").is_some() {
        assert!(Instant::now() < deadline, "A unloads the fenced cluster");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // B's data is intact.
    let (s, v, _) = call(&b, "GET", "/t/_doc/2", "").await;
    assert_eq!(s, 200, "{v}");
    let (s, _, _) = call(&b, "GET", "/t/_doc/1", "").await;
    assert_eq!(s, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deposed_owner_cannot_acknowledge_writes() {
    let objects = search_store::test_objects("server-fencing-acks");
    let shared = MemoryLeases::new();
    let ttl = Duration::from_millis(600);
    let flaky = Arc::new(Flaky { inner: shared.clone(), cut: AtomicBool::new(false), lie: AtomicBool::new(false) });
    let a = node(&objects, "node-a", flaky.clone(), ttl).await;
    let b = node(&objects, "node-b", shared.clone(), ttl).await;
    let (s, _, _) = call(&a, "PUT", "/shop-items/_doc/1", r#"{"x":1}"#).await;
    assert_eq!(s, 201);
    // A's lease store lies (renewals "succeed"), and its lease is released behind its back.
    flaky.lie.store(true, Ordering::SeqCst);
    shared.release("shop", "node-a", 1).await.unwrap();
    // B wakes the cluster and only reads: it writes no commit point or state of its own.
    let (s, v, _) = call(&b, "GET", "/shop-items/_doc/1", "").await;
    assert_eq!(s, 200, "{v}");
    // A still believes it owns the cluster, and its lease deadline keeps being renewed: its
    // WAL append succeeds, but the ownership check before the acknowledgement refuses it.
    let (s, v, _) = call(&a, "PUT", "/shop-items/_doc/3", r#"{"x":3}"#).await;
    assert_eq!(s, 503, "a deposed owner must not acknowledge: {v}");
    assert_eq!(v["error"]["type"], "cluster_block_exception", "{v}");
    assert!(v["error"]["reason"].as_str().unwrap().contains("owned under epoch 2"), "{v}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.get_loaded("shop").is_some() {
        assert!(Instant::now() < deadline, "A unloads the cluster it lost");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // B writes on; A's later attempts (a reload under its stale view) are refused at wake.
    let (s, v, _) = call(&b, "PUT", "/shop-items/_doc/2", r#"{"x":2}"#).await;
    assert_eq!(s, 201, "{v}");
    let (s, _, _) = call(&a, "PUT", "/shop-items/_doc/4", r#"{"x":4}"#).await;
    assert!(s >= 500, "{s}");
    // Everything B acknowledged survives B's unload and wake; A's refused write is absent.
    assert!(b.unload("shop").await);
    for (id, want) in [(1, 200), (2, 200), (3, 404), (4, 404)] {
        let (s, v, _) = call(&b, "GET", &format!("/shop-items/_doc/{id}"), "").await;
        assert_eq!(s, want, "doc {id}: {v}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_hands_clusters_over() {
    let objects = search_store::test_objects("server-handoff");
    let shared = MemoryLeases::new();
    let ttl = Duration::from_secs(30);
    let a = node(&objects, "node-a", shared.clone(), ttl).await;
    let b = node(&objects, "node-b", shared.clone(), ttl).await;
    let (s, _, _) = call(&a, "PUT", "/h/_doc/1", r#"{"x":1}"#).await;
    assert_eq!(s, 201);
    // SIGTERM: flush and release; B takes over at once, without waiting out the 30 s lease.
    a.shutdown(Duration::from_secs(5)).await.unwrap();
    let started = Instant::now();
    let (s, v, _) = call(&b, "GET", "/h/_doc/1", "").await;
    assert_eq!(s, 200, "{v}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(b.get_loaded("shop").unwrap().epoch(), 2);
    assert_eq!(b.get_loaded("shop").unwrap().recovery.records, 0, "A flushed before handing over");
    // A refuses new work.
    let (s, _, _) = call(&a, "GET", "/h/_doc/1", "").await;
    assert_eq!(s, 503);
}

//! Streams on the fleet: several brokers in one process, one catalog and one object store, with
//! leases on the memory backend and on DynamoDB Local (`ROLL_CLUSTER_TEST_DYNAMODB` names its
//! endpoint, else `amazon/dynamodb-local` is started in Docker as `roll-cloud-streams-ddb`; the
//! DynamoDB tests skip without either). Connections a node doesn't own are spliced to the owner
//! in process, as the edge does over its internal hop.
//!
//! - Kafka connections and HTTP requests through any node reach the stream's owner.
//! - The owner dies: another node takes over within the lease, and every acknowledged record is
//!   there.
//! - A drain hands the stream off: an idempotent producer and a consumer see nothing but a
//!   reconnect, and nothing is lost or written twice.
//! - Two nodes believing they own a stream: the one that claimed it last is the only writer.
//! - A branch of records acknowledged only in the write-ahead log has every one of them.

mod common;

use common::{init_tracing, temp_dir, test_config};
use futures::future::BoxFuture;
use rdkafka::ClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::message::Message;
use rdkafka::producer::{BaseRecord, DeliveryResult, Producer, ProducerContext, ThreadedProducer};
use roll_cluster::{Cluster, ClusterConfig, DynamoDb, DynamoDbConfig, Memory, MemoryStore, Node, placement};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};
use stream_broker::fleet::{self, Hop, Io, Splicer};
use stream_broker::store::Settings;
use stream_broker::{Broker, server};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const SECRET: &[u8] = b"test cluster secret, 32 bytes!!";
const T: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
enum Kind {
    Memory,
    DynamoDb,
}

macro_rules! on_every_backend {
    ($($name:ident),* $(,)?) => {
        mod memory { $(#[tokio::test(flavor = "multi_thread", worker_threads = 6)] async fn $name() { super::$name(super::Kind::Memory).await })* }
        mod dynamodb { $(#[tokio::test(flavor = "multi_thread", worker_threads = 6)] async fn $name() { super::$name(super::Kind::DynamoDb).await })* }
    };
}

on_every_backend!(
    connections_and_requests_reach_the_owner,
    the_owner_dies_and_no_acknowledged_record_is_lost,
    a_drain_hands_off_with_only_a_reconnect,
    a_stream_claimed_elsewhere_has_one_writer,
);

type Registry = Arc<RwLock<HashMap<String, Arc<Broker>>>>;

/// The edge's splice, in process: the owner serves the connection as if it had accepted it.
struct InProcess(Registry);

impl Splicer for InProcess {
    fn splice(&self, to: &Node, io: Box<dyn Io>, hop: Hop) -> BoxFuture<'static, std::io::Result<()>> {
        let owner = self.0.read().unwrap().get(&to.id).cloned();
        Box::pin(async move {
            let owner = owner.ok_or_else(|| std::io::Error::other(format!("node {} is gone", hop.stream)))?;
            server::serve_spliced(owner, io, hop.first, hop.peer, &hop.stream, hop.advertised).await;
            Ok(())
        })
    }
}

enum Leases {
    Memory(MemoryStore),
    Dynamo { endpoint: String, table: String },
}

/// Nodes sharing a catalog, object storage and a lease backend.
struct Fleet {
    leases: Leases,
    scale: u32,
    db: code_sql::Db,
    dir: PathBuf,
    storage: pg_storage::StorageConfig,
    registry: Registry,
    /// Upload chunks only after a long while: acknowledged records are in the write-ahead log
    /// alone.
    lazy_uploads: bool,
}

struct TestNode {
    id: String,
    broker: Arc<Broker>,
    cluster: Arc<Cluster>,
    kafka: String,
    http: String,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Fleet {
    fn new(kind: Kind, name: &str) -> Option<Fleet> {
        init_tracing();
        let (leases, scale) = match kind {
            Kind::Memory => (Leases::Memory(MemoryStore::new()), 1),
            Kind::DynamoDb => (Leases::Dynamo { endpoint: dynamodb_endpoint()?, table: format!("t-streams-{name}-{:016x}", random()) }, 2),
        };
        let dir = temp_dir(&format!("fleet-{name}"));
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.join("catalog.db")), "streams").unwrap();
        // One object store for every node (S3 under one prefix with ROLL_E2E_S3_BUCKET).
        let storage = test_config(&dir).storage;
        Some(Fleet { leases, scale, db, dir, storage, registry: Registry::default(), lazy_uploads: false })
    }

    fn timings(&self, id: &str, url: &str) -> ClusterConfig {
        let mut c = ClusterConfig::for_tests(id, url, SECRET);
        for d in [&mut c.heartbeat, &mut c.node_ttl, &mut c.lease_ttl, &mut c.renew_every, &mut c.lease_margin, &mut c.sweep_every, &mut c.rebalance_every, &mut c.handoff_timeout] {
            *d *= self.scale;
        }
        // Handoffs flush and upload before letting go.
        c.handoff_timeout *= 2;
        c
    }

    async fn join(&self, id: &str) -> TestNode {
        let kafka = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (kafka_addr, http_url) = (kafka.local_addr().unwrap(), format!("http://{}", http.local_addr().unwrap()));
        let cfg = self.timings(id, &http_url);
        let backend: Box<dyn roll_cluster::Backend> = match &self.leases {
            Leases::Memory(m) => Box::new(Memory::new(m.clone())),
            Leases::Dynamo { endpoint, table } => {
                let mut c = DynamoDbConfig::new(table.clone());
                c.endpoint = Some(endpoint.clone());
                c.create_table = true;
                let c2 = c.clone();
                Box::new(tokio::task::spawn_blocking(move || DynamoDb::connect(c2)).await.unwrap().unwrap())
            }
        };
        let cluster = tokio::task::spawn_blocking(move || Cluster::join(backend, cfg)).await.unwrap().unwrap();
        let mut config = test_config(&self.dir.join(id));
        config.storage = self.storage.clone();
        config.listen = kafka_addr;
        config.advertised_port = kafka_addr.port();
        config.http_url = http_url.clone();
        config.default_stream = Some("test".into());
        if self.lazy_uploads {
            config.upload.min_bytes = 64 << 20;
            config.upload.max_age_ms = 600_000;
        }
        let mut f = fleet::Fleet::new(cluster.clone(), Arc::new(InProcess(self.registry.clone())));
        f.linger = Duration::from_millis(5);
        let broker = Broker::on_fleet(config, self.db.clone(), f).await.unwrap();
        self.registry.write().unwrap().insert(id.to_string(), broker.clone());
        let mut tasks = vec![];
        let b = broker.clone();
        tasks.push(tokio::spawn(async move {
            let _ = server::serve_on(b, kafka).await;
        }));
        tasks.push(tokio::spawn(broker.clone().run()));
        let router = stream_broker::http::router(broker.clone());
        tasks.push(tokio::spawn(async move {
            let _ = axum::serve(http, router).await;
        }));
        TestNode { id: id.to_string(), broker, cluster, kafka: kafka_addr.to_string(), http: http_url, tasks }
    }

    async fn nodes(&self, ids: &[&str]) -> Vec<TestNode> {
        let mut nodes = vec![];
        for id in ids {
            nodes.push(self.join(id).await);
        }
        let deadline = Instant::now() + T;
        while nodes.iter().any(|n| n.cluster.nodes().len() != ids.len()) {
            assert!(Instant::now() < deadline, "membership never converged");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let first = &nodes[0].broker;
        if first.find_stream("test").is_none() {
            first.create_stream("test", Settings::default()).unwrap();
        }
        nodes
    }

    /// Crash a node: its leases lapse, its memory is gone, nothing reaches it any more.
    fn kill(&self, n: &TestNode) {
        self.registry.write().unwrap().remove(&n.id);
        n.cluster.fault_kill();
        for t in &n.tasks {
            t.abort();
        }
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stream_id(n: &TestNode) -> String {
    n.broker.find_stream("test").unwrap().id.clone()
}

/// The node that should own the test stream among `nodes`, and one that shouldn't.
fn owner_and_other(nodes: &[TestNode]) -> (&TestNode, &TestNode) {
    let key = fleet::key(&stream_id(&nodes[0]));
    let want = placement::rendezvous(&key, nodes.iter().map(|n| n.id.as_str())).unwrap();
    let owner = nodes.iter().find(|n| n.id == want).unwrap();
    (owner, nodes.iter().find(|n| n.id != want).unwrap())
}

/// Wait until `f` holds.
async fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    while !f() {
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    started.elapsed()
}

fn serving(n: &TestNode, stream: &str) -> bool {
    n.broker.stream(stream).is_some_and(|s| matches!(s.tenure.serving(), fleet::Serving::Yes { .. }))
}

#[derive(Default)]
struct Deliveries {
    ok: AtomicUsize,
    failed: AtomicUsize,
}

impl ClientContext for Deliveries {}

impl ProducerContext for Deliveries {
    type DeliveryOpaque = ();
    fn delivery(&self, r: &DeliveryResult<'_>, _: ()) {
        match r {
            Ok(_) => self.ok.fetch_add(1, Ordering::Relaxed),
            Err((e, _)) => {
                eprintln!("delivery failed: {e}");
                self.failed.fetch_add(1, Ordering::Relaxed)
            }
        };
    }
}

fn client(addr: &str) -> ClientConfig {
    let mut c = ClientConfig::new();
    c.set("bootstrap.servers", addr).set("broker.address.family", "v4").set("reconnect.backoff.ms", "50").set("reconnect.backoff.max.ms", "500");
    if std::env::var("RDKAFKA_DEBUG").is_ok() {
        c.set("debug", "broker,protocol,topic,msg,eos");
    }
    c
}

fn producer(addr: &str) -> ThreadedProducer<Deliveries> {
    client(addr)
        .set("enable.idempotence", "true")
        .set("linger.ms", "2")
        .set("message.timeout.ms", "60000")
        .create_with_context(Deliveries::default())
        .expect("producer")
}

fn consumer(addr: &str) -> BaseConsumer {
    client(addr).set("group.id", "readers").set("auto.offset.reset", "earliest").set("enable.auto.commit", "false").create().expect("consumer")
}

fn send(p: &ThreadedProducer<Deliveries>, topic: &str, i: usize) {
    let v = i.to_string();
    while let Err((e, _)) = p.send(BaseRecord::to(topic).key(&v).payload(&v)) {
        assert!(matches!(e, rdkafka::error::KafkaError::MessageProduction(rdkafka::types::RDKafkaErrorCode::QueueFull)), "send: {e}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Every record of a one-partition topic from the start: `(offset, value)`.
fn read_all(c: &BaseConsumer, topic: &str, n: usize) -> Vec<(i64, usize)> {
    let mut tpl = rdkafka::TopicPartitionList::new();
    tpl.add_partition_offset(topic, 0, rdkafka::Offset::Beginning).unwrap();
    c.assign(&tpl).unwrap();
    let mut out = vec![];
    let deadline = Instant::now() + T;
    while out.len() < n {
        assert!(Instant::now() < deadline, "timed out with {} of {n} records", out.len());
        if let Some(Ok(m)) = c.poll(Duration::from_millis(200)) {
            out.push((m.offset(), std::str::from_utf8(m.payload().unwrap()).unwrap().parse().unwrap()));
        }
    }
    // Nothing more.
    let quiet = Instant::now() + Duration::from_millis(500);
    while Instant::now() < quiet {
        if let Some(Ok(m)) = c.poll(Duration::from_millis(100)) {
            panic!("an extra record at offset {}", m.offset());
        }
    }
    out
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.expect("client thread panicked")
}

async fn http_produce(base: &str, topic: &str, value: &str) -> reqwest::Response {
    reqwest::Client::new().post(format!("{base}/v1/streams/test/topics/{topic}/records")).json(&serde_json::json!({ "value": value })).send().await.unwrap()
}

async fn http_read(base: &str, topic: &str) -> Vec<String> {
    let v: serde_json::Value = reqwest::get(format!("{base}/v1/streams/test/topics/{topic}/partitions/0/records?offset=0&limit=1000")).await.unwrap().json().await.unwrap();
    v["records"].as_array().unwrap_or_else(|| panic!("no records in {v}")).iter().map(|r| r["value"].as_str().unwrap().to_string()).collect()
}

/// Whichever node a client reaches, its connection and its HTTP requests run on the stream's
/// owner, and only the owner opens the stream's partitions.
async fn connections_and_requests_reach_the_owner(kind: Kind) {
    let Some(f) = Fleet::new(kind, "route") else { return };
    let nodes = f.nodes(&["a", "b", "c"]).await;
    let (owner, via) = owner_and_other(&nodes);
    let (addr, http) = (via.kafka.clone(), via.http.clone());
    let delivered = blocking(move || {
        let p = producer(&addr);
        for i in 0..50 {
            send(&p, "orders", i);
        }
        p.flush(T).unwrap();
        let c = consumer(&addr);
        let got = read_all(&c, "orders", 50);
        assert_eq!(got, (0..50).map(|i| (i as i64, i)).collect::<Vec<_>>());
        p.context().ok.load(Ordering::Relaxed)
    })
    .await;
    assert_eq!(delivered, 50);
    assert!(serving(owner, &stream_id(owner)), "the preferred node took the stream");
    // HTTP through the other non-owner reaches the same log.
    let third = nodes.iter().find(|n| n.id != owner.id && n.id != via.id).unwrap();
    assert!(http_produce(&third.http, "orders", "50").await.status().is_success());
    assert_eq!(http_read(&http, "orders").await.len(), 51);
    for n in &nodes {
        let s = n.broker.find_stream("test").unwrap();
        let open = s.topic("orders").and_then(|t| t.partition(0)).and_then(|p| p.loaded()).is_some();
        assert_eq!(open, n.id == owner.id, "node {}: only the owner opens partitions", n.id);
    }
}

/// The owner dies with acknowledged records it hadn't uploaded as chunks yet: another node takes
/// the stream over within the lease and serves every one of them, at the same offsets.
async fn the_owner_dies_and_no_acknowledged_record_is_lost(kind: Kind) {
    let Some(mut f) = Fleet::new(kind, "crash") else { return };
    f.lazy_uploads = true;
    let nodes = f.nodes(&["a", "b", "c"]).await;
    let (owner, via) = owner_and_other(&nodes);
    let id = stream_id(owner);
    let addr = via.kafka.clone();
    let delivered = blocking(move || {
        let p = producer(&addr);
        for i in 0..200 {
            send(&p, "events", i);
            // Several write-ahead log objects.
            if i % 50 == 49 {
                p.flush(T).unwrap();
            }
        }
        p.flush(T).unwrap();
        p.context().ok.load(Ordering::Relaxed)
    })
    .await;
    assert_eq!(delivered, 200);
    let s = owner.broker.stream(&id).unwrap();
    assert!(s.topic("events").unwrap().partition(0).unwrap().loaded().unwrap().stats().pending_upload_bytes > 0, "records are only in the write-ahead log");
    owner.broker.halt().await;
    f.kill(owner);
    let lease = owner.cluster.config().lease_ttl;
    let took = eventually("a takeover", lease * 4 + Duration::from_secs(2), || nodes.iter().any(|n| n.id != owner.id && serving(n, &id))).await;
    eprintln!("taken over {took:?} after the owner died (lease {lease:?})");
    assert!(took < lease * 3, "taken over in {took:?} (lease {lease:?})");
    let addr = via.kafka.clone();
    blocking(move || {
        let got = read_all(&consumer(&addr), "events", 200);
        assert_eq!(got, (0..200).map(|i| (i as i64, i)).collect::<Vec<_>>());
        let p = producer(&addr);
        send(&p, "events", 200);
        p.flush(T).unwrap();
        assert_eq!(p.context().ok.load(Ordering::Relaxed), 1);
    })
    .await;
}

/// Draining the owner hands the stream to another node while an idempotent producer and a
/// consumer keep going: they reconnect, nothing fails, nothing is lost or written twice.
async fn a_drain_hands_off_with_only_a_reconnect(kind: Kind) {
    let Some(f) = Fleet::new(kind, "drain") else { return };
    let nodes = f.nodes(&["a", "b", "c"]).await;
    let (owner, via) = owner_and_other(&nodes);
    let id = stream_id(owner);
    let addr = via.kafka.clone();
    let n = 600;
    let p = Arc::new(blocking(move || producer(&addr)).await);
    let p2 = p.clone();
    let producing = tokio::task::spawn_blocking(move || {
        for i in 0..n {
            send(&p2, "steady", i);
            std::thread::sleep(Duration::from_millis(3));
        }
        p2.flush(T).unwrap();
        (p2.context().ok.load(Ordering::Relaxed), p2.context().failed.load(Ordering::Relaxed))
    });
    // Drain in the middle of the traffic.
    eventually("records flowing", T, || p.context().ok.load(Ordering::Relaxed) >= 150).await;
    owner.cluster.drain(Duration::from_secs(20)).await;
    assert!(p.context().ok.load(Ordering::Relaxed) < n, "the drain happened while producing");
    eventually("another owner", T, || nodes.iter().any(|n| n.id != owner.id && serving(n, &id))).await;
    let (ok, failed) = producing.await.unwrap();
    assert_eq!((ok, failed), (n, 0), "every record delivered, none failed");
    let addr = via.kafka.clone();
    blocking(move || {
        let got = read_all(&consumer(&addr), "steady", n);
        assert_eq!(got, (0..n).map(|i| (i as i64, i)).collect::<Vec<_>>(), "in order, exactly once");
    })
    .await;
}

/// Two nodes both believe they own the stream (a lease moved under a frozen owner): the one that
/// claimed it last is the only one that can acknowledge anything. The other's write fails and
/// is never visible; the stream carries on with an owner that claims it properly.
async fn a_stream_claimed_elsewhere_has_one_writer(kind: Kind) {
    let Some(f) = Fleet::new(kind, "fence") else { return };
    let nodes = f.nodes(&["a", "b"]).await;
    let (owner, other) = owner_and_other(&nodes);
    let id = stream_id(owner);
    for i in 0..10 {
        assert!(http_produce(&owner.http, "ledger", &i.to_string()).await.status().is_success());
    }
    // The other node claims the stream, as a new owner would.
    let (b, sid) = (other.broker.clone(), id.clone());
    tokio::task::spawn_blocking(move || b.store.claim(&sid, "b")).await.unwrap().unwrap();
    let r = http_produce(&owner.http, "ledger", "lost").await;
    assert_eq!(r.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE, "{:?}", r.text().await);
    // The stream gets an owner again (whose claim fences the other node's), and the write that
    // failed never appears.
    eventually("an owner again", T, || nodes.iter().any(|n| serving(n, &id))).await;
    assert!(http_produce(&other.http, "ledger", "10").await.status().is_success());
    let values = http_read(&other.http, "ledger").await;
    assert_eq!(values, (0..=10).map(|i| i.to_string()).collect::<Vec<_>>());
}

// ── DynamoDB Local ──

const CONTAINER: &str = "roll-cloud-streams-ddb";
/// DynamoDB Local, pinned by digest (the multi-arch index), as every suite pins its images.
const DYNAMODB_IMAGE: &str = "amazon/dynamodb-local:3.3.2@sha256:6ada631ec7aa5fc83446855eb17233e3dad184fec47477e8140c6c569cfe0c86";

fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    h.finish()
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

extern "C" fn remove_container() {
    let _ = Command::new("docker").args(["rm", "-f", CONTAINER]).output();
}

fn answers_http(addr: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(addr) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 12];
    s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_ok() && s.read_exact(&mut buf).is_ok() && buf.starts_with(b"HTTP/")
}

/// DynamoDB Local's endpoint for this run, or `None` (the DynamoDB tests then skip, unless
/// `ROLL_CLUSTER_DYNAMODB_REQUIRED=1`).
fn dynamodb_endpoint() -> Option<String> {
    static EP: OnceLock<Option<String>> = OnceLock::new();
    let ep = EP.get_or_init(|| {
        if let Ok(e) = std::env::var("ROLL_CLUSTER_TEST_DYNAMODB")
            && !e.is_empty()
        {
            return Some(e);
        }
        let port = |s: &str| s.lines().next().and_then(|l| l.rsplit(':').next()).map(str::to_string);
        let p = match docker(&["port", CONTAINER, "8000/tcp"]).ok().and_then(|s| port(&s)) {
            Some(p) => p,
            None => {
                if let Err(e) = docker(&["run", "-d", "--rm", "--name", CONTAINER, "-p", "127.0.0.1::8000", DYNAMODB_IMAGE]) {
                    eprintln!("skipping the DynamoDB tests: Docker can't start amazon/dynamodb-local ({e})");
                    return None;
                }
                // SAFETY: registering a plain extern "C" function to run at exit.
                unsafe { libc::atexit(remove_container) };
                port(&docker(&["port", CONTAINER, "8000/tcp"]).ok()?)?
            }
        };
        let addr = format!("127.0.0.1:{p}");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !answers_http(&addr) {
            if Instant::now() > deadline {
                eprintln!("skipping the DynamoDB tests: DynamoDB Local never answered on {addr}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Some(format!("http://{addr}"))
    });
    if ep.is_none() && std::env::var("ROLL_CLUSTER_DYNAMODB_REQUIRED").is_ok_and(|v| v == "1") {
        panic!("DynamoDB Local is required (ROLL_CLUSTER_DYNAMODB_REQUIRED=1) but isn't available");
    }
    ep.clone()
}


async fn http_records(base: &str, stream: &str, topic: &str) -> Vec<(i64, String)> {
    let v: serde_json::Value = reqwest::get(format!("{base}/v1/streams/{stream}/topics/{topic}/partitions/0/records?offset=0&limit=1000")).await.unwrap().json().await.unwrap();
    v["records"].as_array().unwrap_or_else(|| panic!("no records in {v}")).iter().map(|r| (r["offset"].as_i64().unwrap(), r["value"].as_str().unwrap().to_string())).collect()
}

/// A branch asked of a node that doesn't own the parent, whose records are acknowledged only in
/// the write-ahead log: the parent's owner uploads them and cuts the branch there, and the branch
/// (owned on its own) serves every one of them at the same offsets, then goes its own way.
/// Leases don't matter here, so this runs on the memory backend only.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_branch_has_every_acknowledged_record() {
    let Some(mut f) = Fleet::new(Kind::Memory, "branch") else { return };
    f.lazy_uploads = true;
    let nodes = f.nodes(&["a", "b", "c"]).await;
    let (owner, via) = owner_and_other(&nodes);
    let http = reqwest::Client::new();
    let r = http.post(format!("{}/v1/streams/test/topics", via.http)).json(&serde_json::json!({ "name": "orders" })).send().await.unwrap();
    assert!(r.status().is_success());
    for i in 0..20 {
        assert!(http_produce(&via.http, "orders", &i.to_string()).await.status().is_success());
    }
    let p = owner.broker.find_stream("test").unwrap().topic("orders").unwrap().partition(0).unwrap();
    assert!(p.loaded().unwrap().stats().pending_upload_bytes > 0, "records are only in the write-ahead log");

    let r = http.post(format!("{}/v1/streams/test/branches", via.http)).json(&serde_json::json!({ "name": "pv" })).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::CREATED);
    let id = r.json::<serde_json::Value>().await.unwrap()["id"].as_str().unwrap().to_string();
    let want: Vec<(i64, String)> = (0..20).map(|i| (i, i.to_string())).collect();
    for n in &nodes {
        assert_eq!(http_records(&n.http, &id, "orders").await, want, "through {}", n.id);
    }
    assert_eq!(nodes.iter().filter(|n| serving(n, &id)).count(), 1, "the branch has one owner");

    let r = http.post(format!("{}/v1/streams/{id}/topics/orders/records", via.http)).json(&serde_json::json!({ "value": "branch" })).send().await.unwrap();
    assert!(r.status().is_success());
    assert!(http_produce(&via.http, "orders", "parent").await.status().is_success());
    assert_eq!(http_records(&via.http, &id, "orders").await.last().unwrap(), &(20, "branch".to_string()));
    assert_eq!(http_records(&via.http, "test", "orders").await.last().unwrap(), &(20, "parent".to_string()));
}

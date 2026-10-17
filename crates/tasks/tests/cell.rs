//! A log store cell: a leader and a standby sharing one bucket, a proxy in front. Leadership
//! moves on a crash or a hand-over without losing anything acknowledged, and requests sent
//! during the move are held until the new leader answers.

mod common;

use common::*;
use object_store::ObjectStore;
use object_store::memory::InMemory;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tasks::cluster::{Exit, NodeConfig, Proxy, Stop, run_node};
use tasks::store::log::LogConfig;
use tokio::sync::watch;

/// An empty bucket: in memory, or under a fresh prefix of `ROLL_TASKS_E2E_LOG` (real S3 …).
fn bucket() -> Arc<dyn ObjectStore> {
    match std::env::var("ROLL_TASKS_E2E_LOG") {
        Ok(url) => {
            let (objects, prefix) = tasks::store::log::objects_from_url(&url).unwrap();
            let unique = format!("{prefix}/cell-{}", tasks::ids::ulid());
            Arc::new(object_store::prefix::PrefixStore::new(objects, unique.as_str()))
        }
        Err(_) => Arc::new(InMemory::new()),
    }
}

struct CellNode {
    id: String,
    base: String,
    stop: watch::Sender<Stop>,
    done: tokio::task::JoinHandle<anyhow::Result<Exit>>,
    _dir: tempfile::TempDir,
}

async fn start_node(objects: Arc<dyn ObjectStore>, id: &str) -> CellNode {
    start_node_with(objects, id, None).await
}

async fn start_node_with(objects: Arc<dyn ObjectStore>, id: &str, token: Option<&str>) -> CellNode {
    let dir = tempfile::tempdir().unwrap();
    let mut log = LogConfig::new(objects, "cell", dir.path().to_path_buf());
    log.node = id.into();
    log.shards = 4;
    log.relaxed_ms = 100;
    let mut tasks = fast_config();
    tasks.token = token.map(str::to_string);
    let mut cfg = NodeConfig::new(log, tasks);
    cfg.health_ms = 100;
    cfg.fail_after = 3;
    cfg.lease_check_ms = 200;
    cfg.forward_wait_ms = 10_000;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, rx) = watch::channel(Stop::Run);
    let done = tokio::spawn(run_node(listener, cfg, rx));
    CellNode { id: id.into(), base, stop, done, _dir: dir }
}

async fn start_proxy(objects: Arc<dyn ObjectStore>) -> String {
    let mut proxy = Proxy::new(objects, "cell");
    proxy.wait = Duration::from_secs(10);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, Arc::new(proxy).router()).await.unwrap() });
    base
}

async fn leader(objects: &dyn ObjectStore) -> Option<tasks::cluster::Lease> {
    tasks::cluster::read_lease(objects, "cell").await.unwrap().map(|(l, _)| l).filter(|l| !l.released)
}

async fn call(http: &reqwest::Client, base: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut r = http.request(method, format!("{base}/v1/tasks{path}"));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn publish(http: &reqwest::Client, base: &str, body: Value) -> String {
    let (s, v) = call(http, base, reqwest::Method::POST, "/publish", Some(body)).await;
    assert!(s.is_success(), "publish: {s} {v}");
    v["message_id"].as_str().unwrap().to_string()
}

async fn wait_leader(objects: &dyn ObjectStore, not: Option<&str>) -> tasks::cluster::Lease {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(l) = leader(objects).await
            && Some(l.node.as_str()) != not
        {
            return l;
        }
        assert!(std::time::Instant::now() < deadline, "no new leader");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn delivered(recv: &Receiver, path: &str) -> HashMap<String, usize> {
    let mut seen = HashMap::new();
    for h in recv.hits(path) {
        *seen.entry(h.header("roll-message-id").unwrap()).or_insert(0) += 1;
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_leader_is_replaced_and_nothing_acknowledged_is_lost() {
    let objects = bucket();
    let a = start_node(objects.clone(), "node-a").await;
    let first = wait_leader(&*objects, None).await;
    assert_eq!(first.node, "node-a");
    let b = start_node(objects.clone(), "node-b").await;
    let proxy = start_proxy(objects.clone()).await;
    let http = reqwest::Client::new();
    let recv = Receiver::start().await;

    // Due later than the crash: only the new leader can deliver these.
    let mut ids = Vec::new();
    for i in 0..30 {
        ids.push(publish(&http, &proxy, json!({ "url": recv.url("/later"), "body": format!("{i}"), "delay": "1500ms" })).await);
    }
    let (s, _) = call(&http, &proxy, reqwest::Method::PUT, "/queues/kept", Some(json!({ "parallelism": 2 }))).await;
    assert!(s.is_success());

    a.stop.send(Stop::Crash).unwrap();
    assert_eq!(a.done.await.unwrap().unwrap(), Exit::Stopped);

    // Sent while nobody leads: held by the proxy, then served by the new leader.
    let during = publish(&http, &proxy, json!({ "url": recv.url("/during"), "body": "x" })).await;
    let second = wait_leader(&*objects, Some("node-a")).await;
    assert_eq!(second.node, b.id);
    assert!(second.epoch > first.epoch);

    recv.wait("/later", 30, 15_000).await;
    recv.wait("/during", 1, 5_000).await;
    let seen = delivered(&recv, "/later");
    for id in &ids {
        assert!(seen.contains_key(id), "lost {id}");
    }
    let (s, q) = call(&http, &proxy, reqwest::Method::GET, "/queues/kept", None).await;
    assert!(s.is_success(), "{q}");
    assert_eq!(q["parallelism"], 2);
    let (_, m) = call(&http, &proxy, reqwest::Method::GET, &format!("/messages/{during}"), None).await;
    assert!(m["state"] == "delivered" || m["state"] == "active", "{m}");

    b.stop.send(Stop::Graceful).unwrap();
    assert_eq!(b.done.await.unwrap().unwrap(), Exit::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_graceful_hand_over_is_quick_and_delivers_everything_once() {
    let objects = bucket();
    let a = start_node(objects.clone(), "node-a").await;
    wait_leader(&*objects, None).await;
    let b = start_node(objects.clone(), "node-b").await;
    let proxy = start_proxy(objects.clone()).await;
    let http = reqwest::Client::new();
    let recv = Receiver::start().await;

    let mut ids = Vec::new();
    for i in 0..40 {
        ids.push(publish(&http, &proxy, json!({ "url": recv.url("/m"), "body": format!("{i}"), "delay": format!("{}ms", i * 25) })).await);
    }
    let started = std::time::Instant::now();
    a.stop.send(Stop::Graceful).unwrap();
    // Keep publishing through the hand-over.
    for i in 40..60 {
        ids.push(publish(&http, &proxy, json!({ "url": recv.url("/m"), "body": format!("{i}") })).await);
    }
    assert_eq!(a.done.await.unwrap().unwrap(), Exit::Stopped);
    let l = wait_leader(&*objects, Some("node-a")).await;
    assert_eq!(l.node, "node-b");
    assert!(started.elapsed() < Duration::from_secs(8), "hand-over took {:?}", started.elapsed());

    recv.wait("/m", ids.len(), 15_000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = delivered(&recv, "/m");
    assert_eq!(seen.len(), ids.len());
    assert!(seen.values().all(|n| *n == 1), "a graceful hand-over shouldn't redeliver: {seen:?}");

    b.stop.send(Stop::Graceful).unwrap();
    b.done.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standbys_forward_requests_to_the_leader() {
    let objects = bucket();
    let a = start_node(objects.clone(), "node-a").await;
    let lease = wait_leader(&*objects, None).await;
    let b = start_node(objects.clone(), "node-b").await;
    let http = reqwest::Client::new();
    let leader_base = format!("http://{}", lease.addr);
    assert_eq!(leader_base, a.base);
    eventually(5_000, "the leader to finish starting", || async {
        let st: Value = http.get(format!("{leader_base}/_roll/node")).send().await.unwrap().json().await.unwrap();
        st["role"] == "leader"
    })
    .await;

    // A standby answers API calls by forwarding them.
    let standby = b.base.clone();
    let st: Value = http.get(format!("{standby}/_roll/node")).send().await.unwrap().json().await.unwrap();
    assert_eq!(st["role"], "standby");
    let id = publish(&http, &standby, json!({ "url": "http://example.test/x", "delay": "1h" })).await;
    let (s, m) = call(&http, &leader_base, reqwest::Method::GET, &format!("/messages/{id}"), None).await;
    assert_eq!(s, StatusCode::OK, "{m}");
    // Errors come back as they are.
    let (s, _) = call(&http, &standby, reqwest::Method::GET, "/messages/msg_nope", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    for n in [a, b] {
        n.stop.send(Stop::Graceful).unwrap();
        n.done.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deposed_leader_steps_down() {
    let objects = bucket();
    let a = start_node(objects.clone(), "node-a").await;
    let first = wait_leader(&*objects, None).await;
    // Another node opens the log behind its back (a partition, a confused operator): the old
    // leader is fenced on its next commit or sees the lease change, whichever comes first.
    let (lease, version) = tasks::cluster::read_lease(&*objects, "cell").await.unwrap().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut log = LogConfig::new(objects.clone(), "cell", dir.path().to_path_buf());
    log.shards = 4;
    let usurper = tasks::store::log::LogStore::open(log).await.unwrap();
    let taken = tasks::cluster::Lease { node: "usurper".into(), epoch: first.epoch + 1, ..lease };
    assert!(tasks::cluster::write_lease(&*objects, "cell", &taken, Some(version)).await.unwrap());
    let exit = tokio::time::timeout(Duration::from_secs(5), a.done).await.expect("old leader kept going").unwrap().unwrap();
    assert!(matches!(exit, Exit::Deposed | Exit::Fenced(_)), "{exit:?}");
    usurper.close().await;
}

/// What the log costs: S3 requests per message at S3-like latency. Prints a table; run with
/// `cargo test -p tasks --test cell --release -- --ignored --nocapture cost_profile`.
/// A raw request with an optional bearer token.
async fn req(http: &reqwest::Client, method: reqwest::Method, url: String, auth: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut r = http.request(method, url);
    if let Some(a) = auth {
        r = r.bearer_auth(a);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// One cell hosts many tenants: each has its own data and keys, the node's token administers
/// them, and they move with the cell when leadership does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_hosts_isolated_tenants_that_survive_a_hand_over() {
    use reqwest::Method;
    const ADMIN: &str = "admin-token";
    let objects = bucket();
    let _ = tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into())).with_test_writer().try_init();
    let a = start_node_with(objects.clone(), "node-a", Some(ADMIN)).await;
    wait_leader(&*objects, None).await;
    let proxy = start_proxy(objects.clone()).await;
    let http = reqwest::Client::new();
    let recv = Receiver::start().await;
    let tenants = format!("{proxy}/_roll/tenants");

    // Only the node's token administers tenants.
    let (s, _) = req(&http, Method::POST, tenants.clone(), None, Some(json!({ "id": "acme" }))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = req(&http, Method::POST, tenants.clone(), Some(ADMIN), Some(json!({ "id": "Not_Valid" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, acme) = req(&http, Method::POST, tenants.clone(), Some(ADMIN), Some(json!({ "id": "acme" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{acme}");
    assert_eq!(acme["path"], "/t/acme/v1/tasks");
    let acme_key = acme["token"].as_str().unwrap().to_string();
    let (s, _) = req(&http, Method::POST, tenants.clone(), Some(ADMIN), Some(json!({ "id": "acme" }))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, globex) = req(&http, Method::POST, tenants.clone(), Some(ADMIN), Some(json!({ "id": "globex" }))).await;
    assert_eq!(s, StatusCode::CREATED);
    let globex_key = globex["token"].as_str().unwrap().to_string();
    let (s, list) = req(&http, Method::GET, tenants.clone(), Some(ADMIN), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["tenants"], json!(["acme", "default", "globex"]));

    // A tenant's API takes its own keys only: not none, not the admin token, not another's.
    let acme_api = format!("{proxy}/t/acme/v1/tasks");
    let publish_body = json!({ "url": recv.url("/acme"), "body": "hello" });
    for auth in [None, Some(ADMIN), Some(globex_key.as_str())] {
        let (s, _) = req(&http, Method::POST, format!("{acme_api}/publish"), auth, Some(publish_body.clone())).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{auth:?}");
    }
    let (s, v) = req(&http, Method::POST, format!("{acme_api}/publish?x=1"), Some(&acme_key), Some(publish_body.clone())).await;
    assert!(s.is_success(), "{s} {v}");
    let acme_msg = v["message_id"].as_str().unwrap().to_string();
    recv.wait("/acme", 1, 5_000).await;

    // Nobody else sees it: not the default tenant (admin token), not globex.
    let (s, _) = req(&http, Method::GET, format!("{proxy}/v1/tasks/messages/{acme_msg}"), Some(ADMIN), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, v) = req(&http, Method::GET, format!("{proxy}/t/globex/v1/tasks/messages/{acme_msg}"), Some(&globex_key), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, _) = req(&http, Method::GET, format!("{proxy}/v1/tasks/messages"), Some(&acme_key), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "a tenant key works on the default tenant");
    let (s, _) = req(&http, Method::GET, format!("{proxy}/t/nope/v1/tasks/messages"), Some(&acme_key), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Hand over: tenants, their keys and their data move with the cell.
    let b = start_node_with(objects.clone(), "node-b", Some(ADMIN)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.stop.send(Stop::Graceful).unwrap();
    assert_eq!(a.done.await.unwrap().unwrap(), Exit::Stopped);
    assert_eq!(wait_leader(&*objects, Some("node-a")).await.node, b.id);
    let (s, m) = req(&http, Method::GET, format!("{acme_api}/messages/{acme_msg}"), Some(&acme_key), None).await;
    assert_eq!(s, StatusCode::OK, "{m}");
    let (s, k) = req(&http, Method::POST, format!("{tenants}/globex/keys"), Some(ADMIN), Some(json!({ "name": "ci" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{k}");
    let (s, _) = req(&http, Method::POST, format!("{proxy}/t/globex/v1/tasks/publish"), k["token"].as_str(), Some(json!({ "url": recv.url("/globex"), "body": "x" }))).await;
    assert!(s.is_success());
    recv.wait("/globex", 1, 5_000).await;

    // Deleting a tenant deletes its data; the id can be taken again, by somebody new.
    let (s, _) = req(&http, Method::DELETE, format!("{tenants}/default"), Some(ADMIN), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = req(&http, Method::DELETE, format!("{tenants}/acme"), Some(ADMIN), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = req(&http, Method::DELETE, format!("{tenants}/acme"), Some(ADMIN), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = req(&http, Method::GET, format!("{acme_api}/messages/{acme_msg}"), Some(&acme_key), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, again) = req(&http, Method::POST, tenants.clone(), Some(ADMIN), Some(json!({ "id": "acme" }))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = req(&http, Method::GET, format!("{acme_api}/messages/{acme_msg}"), Some(&acme_key), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "an old key opens a new tenant");
    let (s, _) = req(&http, Method::GET, format!("{acme_api}/messages/{acme_msg}"), again["token"].as_str(), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    b.stop.send(Stop::Graceful).unwrap();
    assert_eq!(b.done.await.unwrap().unwrap(), Exit::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cost_profile() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    for (clients, n) in [(1usize, 200usize), (8, 1_000), (64, 4_000)] {
        let objects: Arc<dyn ObjectStore> = Arc::new(ThrottledStore::new(
            InMemory::new(),
            ThrottleConfig { wait_put_per_call: Duration::from_millis(30), wait_get_per_call: Duration::from_millis(15), ..Default::default() },
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut log = LogConfig::new(objects, "cost", dir.path().to_path_buf());
        log.shards = 16;
        let store = tasks::store::log::LogStore::open(log).await.unwrap();
        let mut cfg = fast_config();
        cfg.max_inflight = 512;
        let tasks = tasks::Tasks::new(cfg, store.clone()).await.unwrap();
        let worker = tasks::start(tasks.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, tasks::api::router(tasks.clone())).into_future());
        let recv = Receiver::start().await;
        let http = reqwest::Client::new();
        let before = store.stats().commits;
        let started = std::time::Instant::now();
        let per = n / clients;
        let publishers = (0..clients).map(|_| {
            let (http, api, url) = (http.clone(), api.clone(), recv.url("/c"));
            tokio::spawn(async move {
                for i in 0..per {
                    publish(&http, &api, json!({ "url": url, "body": format!("{i}") })).await;
                }
            })
        });
        futures::future::join_all(publishers).await;
        let published = started.elapsed();
        recv.wait("/c", per * clients, 120_000).await;
        // Let the last delivery outcomes commit.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let commits = store.stats().commits - before;
        let msgs = (per * clients) as f64;
        println!(
            "clients {clients:>3}  messages {:>5}  publish {:>6.0}/s  S3 PUTs {commits:>5}  PUTs/message {:.3}  $/million messages {:.2}",
            msgs,
            msgs / published.as_secs_f64(),
            commits as f64 / msgs,
            commits as f64 / msgs * 5.0
        );
        worker.shutdown().await;
        store.close().await;
    }
}

/// CPU time this process has used so far.
fn cpu_time() -> Duration {
    let out = std::process::Command::new("ps").args(["-o", "time=", "-p", &std::process::id().to_string()]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // [[dd-]hh:]mm:ss[.cc]
    let secs = text.trim().rsplit(['-', ':']).zip([1.0, 60.0, 3600.0, 86400.0]).map(|(v, m)| v.parse::<f64>().unwrap_or(0.0) * m).sum::<f64>();
    Duration::from_secs_f64(secs)
}

/// The shared platform: many small tenants in one cell share its commits. Prints S3 PUTs per
/// message and the idle CPU of the hosted tenants. `cargo test -p tasks --test cell
/// multi_tenant_cost_profile -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn multi_tenant_cost_profile() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    let tenants = 200usize;
    let seconds = 10usize;
    let objects: Arc<dyn ObjectStore> = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig { wait_put_per_call: Duration::from_millis(30), wait_get_per_call: Duration::from_millis(15), ..Default::default() },
    ));
    let dir = tempfile::tempdir().unwrap();
    let log = tasks::store::log::Log::open(LogConfig::new(objects, "mt", dir.path().to_path_buf())).await.unwrap();
    let cfg = tasks::Config { max_inflight: 512, ..Default::default() };
    let cell = tasks::tenants::Cell::start(log.clone(), cfg).await.unwrap();
    let mut keys = Vec::new();
    for i in 0..tenants {
        keys.push((format!("t{i}"), cell.create(&format!("t{i}")).await.unwrap()));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, cell.clone().router()).into_future());
    let recv = Receiver::start().await;
    let http = reqwest::Client::new();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let (cpu0, wall0) = (cpu_time(), std::time::Instant::now());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let idle_cpu = (cpu_time() - cpu0).as_secs_f64() / wall0.elapsed().as_secs_f64();

    // Every tenant publishes once a second, unsynchronised.
    let before = log.stats().commits;
    let started = std::time::Instant::now();
    let jobs = keys.iter().enumerate().map(|(i, (id, key))| {
        let (http, url, target, key) = (http.clone(), format!("{api}/t/{id}/v1/tasks/publish"), recv.url("/mt"), key.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis((i * 1000 / tenants) as u64)).await;
            let mut took = Vec::new();
            for n in 0..seconds {
                let at = std::time::Instant::now();
                let r = http.post(&url).bearer_auth(&key).json(&json!({ "url": target, "body": format!("{n}") })).send().await.unwrap();
                assert!(r.status().is_success());
                took.push(at.elapsed());
                tokio::time::sleep(Duration::from_secs(1).saturating_sub(at.elapsed())).await;
            }
            took
        })
    });
    let mut took: Vec<Duration> = futures::future::join_all(jobs).await.into_iter().flat_map(|r| r.unwrap()).collect();
    let published = started.elapsed();
    took.sort();
    let pct = |p: usize| took[(took.len() * p / 100).min(took.len() - 1)].as_millis();
    let msgs = tenants * seconds;
    recv.wait("/mt", msgs, 60_000).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let commits = log.stats().commits - before;
    println!(
        "tenants {tenants}  messages {msgs} published in {:.1}s  S3 PUTs {commits}  PUTs/message {:.4}  $/million messages {:.3}  idle CPU {:.1}% of a core  publish p50 {}ms p99 {}ms max {}ms",
        published.as_secs_f64(),
        commits as f64 / msgs as f64,
        commits as f64 / msgs as f64 * 5.0,
        idle_cpu * 100.0,
        pct(50),
        pct(99),
        took.last().unwrap().as_millis()
    );
    cell.shutdown().await;
    log.close().await;
}

/// At rest, a cell costs nothing in S3 writes: heartbeats and lane scans stay on the node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_leader_writes_nothing_to_s3() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = LogConfig::new(bucket(), "idle", dir.path().to_path_buf());
    log.shards = 4;
    log.relaxed_ms = 100;
    let store = tasks::store::log::LogStore::open(log).await.unwrap();
    let tasks = tasks::Tasks::new(fast_config(), store.clone()).await.unwrap();
    let worker = tasks::start(tasks.clone());
    // A schedule far in the future and a queue: config, but nothing due.
    tasks.put_queue("q", Default::default()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let before = store.stats().commits;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let during = store.stats().commits - before;
    assert_eq!(during, 0, "an idle node committed {during} times in 2s");
    worker.shutdown().await;
    store.close().await;
}

//! The proxy against fake nodes: routing, owner redirects, holding and waking from zero (with a
//! fake ECS), retries across a shutdown handoff, dead nodes, TLS with SNI, the PROXY protocol,
//! and nodes whose records pin a certificate (no request to anything else at their address).
//! The fleet with real search nodes is tested in `crates/search-control/tests/fleet.rs`.

use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use parking_lot::Mutex;
use search_proxy::fleet::{Beat, Members, MemoryMembers};
use search_proxy::proxy::{ListenOptions, Port};
use search_proxy::wake::Credentials;
use search_proxy::{Directory, EcsWaker, Proxy, ProxyConfig, Waker};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Leases of the fake fleet: cluster → owning node.
type Leases = Arc<Mutex<HashMap<String, String>>>;

struct FakeNode {
    name: String,
    addr: SocketAddr,
    hits: AtomicUsize,
    /// Answers `node_closed_exception` for clusters it doesn't hold (shutting down).
    closed: AtomicBool,
    /// Drops connections without answering.
    broken: AtomicBool,
    /// Answers every request with a 503 whose body is this many bytes (no owner header).
    big_503: AtomicUsize,
}

fn cluster_of(req: &Request<Incoming>) -> Option<String> {
    let h = req.headers().get("authorization")?.to_str().ok()?;
    let b = base64::engine::general_purpose::STANDARD.decode(h.strip_prefix("Basic ")?).ok()?;
    let s = String::from_utf8(b).ok()?;
    Some(s.split_once(':')?.0.to_string())
}

fn password_of(req: &Request<Incoming>) -> Option<String> {
    let h = req.headers().get("authorization")?.to_str().ok()?;
    let b = base64::engine::general_purpose::STANDARD.decode(h.strip_prefix("Basic ")?).ok()?;
    let s = String::from_utf8(b).ok()?;
    Some(s.split_once(':')?.1.to_string())
}

/// A node that answers with its name, takes free clusters' leases, and refuses clusters another
/// node holds the way search-server does (503 + `X-Roll-Search-Owner`).
async fn fake_node(name: &str, leases: Leases) -> Arc<FakeNode> {
    fake_node_with(name, leases, None).await
}

/// [`fake_node`] serving TLS with `tls` (both its REST and API port).
async fn fake_node_with(name: &str, leases: Leases, tls: Option<tokio_rustls::TlsAcceptor>) -> Arc<FakeNode> {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = Arc::new(FakeNode { name: name.into(), addr: l.local_addr().unwrap(), hits: AtomicUsize::new(0), closed: AtomicBool::new(false), broken: AtomicBool::new(false), big_503: AtomicUsize::new(0) });
    let n = node.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            let (n, leases, tls) = (n.clone(), leases.clone(), tls.clone());
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let (n, leases) = (n.clone(), leases.clone());
                    async move {
                        if n.broken.load(Ordering::SeqCst) {
                            return Err(std::io::Error::other("dropped"));
                        }
                        n.hits.fetch_add(1, Ordering::SeqCst);
                        let cluster = cluster_of(&req);
                        let headers: HashMap<String, String> = req.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
                        let (method, path) = (req.method().to_string(), req.uri().to_string());
                        // Like search-server: credentials are checked first (password `bad`
                        // fails), before any lease.
                        if password_of(&req).as_deref() == Some("bad") {
                            let e = json!({"error": {"type": "security_exception", "reason": "unable to authenticate user"}, "status": 401});
                            return Ok(Response::builder().status(401).body(Full::new(Bytes::from(e.to_string()))).unwrap());
                        }
                        let big = n.big_503.load(Ordering::SeqCst);
                        if big > 0 {
                            let reason = "x".repeat(big);
                            let e = json!({"error": {"type": "search_phase_execution_exception", "reason": reason}, "status": 503});
                            return Ok(Response::builder().status(503).header("content-type", "application/json").body(Full::new(Bytes::from(e.to_string()))).unwrap());
                        }
                        // Like search-server: refusals come before the body is read.
                        if let Some(c) = &cluster {
                            let mut l = leases.lock();
                            let owner = l.get(c).cloned();
                            match owner {
                                Some(o) if o != n.name => {
                                    let e = json!({"error": {"type": "cluster_block_exception", "reason": format!("cluster [{c}] is served by node [{o}]")}, "status": 503});
                                    return Ok(Response::builder().status(503).header("X-Roll-Search-Owner", o).body(Full::new(Bytes::from(e.to_string()))).unwrap());
                                }
                                None if n.closed.load(Ordering::SeqCst) => {
                                    let e = json!({"error": {"type": "node_closed_exception", "reason": "node is shutting down"}, "status": 503});
                                    return Ok(Response::builder().status(503).body(Full::new(Bytes::from(e.to_string()))).unwrap());
                                }
                                None => {
                                    l.insert(c.clone(), n.name.clone());
                                }
                                _ => {}
                            }
                        }
                        let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                        let v = json!({ "node": n.name, "cluster": cluster, "method": method, "path": path, "body_len": body.len(), "headers": headers });
                        Ok::<_, std::io::Error>(Response::builder().header("content-type", "application/json").body(Full::new(Bytes::from(v.to_string()))).unwrap())
                    }
                });
                match tls {
                    Some(acceptor) => {
                        let Ok(s) = acceptor.accept(s).await else { return };
                        let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(s), svc).await;
                    }
                    None => {
                        let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(s), svc).await;
                    }
                }
            });
        }
    });
    node
}

fn beat(n: &FakeNode, az: &str) -> Beat {
    Beat { id: n.name.clone(), http: n.addr.to_string(), api: Some(n.addr.to_string()), az: Some(az.into()), ..Beat::default() }
}

struct Env {
    proxy: Arc<Proxy>,
    url: String,
    _members: Arc<MemoryMembers>,
    http: reqwest::Client,
}

async fn proxy(members: Arc<MemoryMembers>, az: &str, waker: Option<Arc<dyn Waker>>, f: impl FnOnce(&mut ProxyConfig)) -> Env {
    let mut cfg = ProxyConfig { az: Some(az.into()), refresh: Duration::from_millis(50), hold_timeout: Duration::from_secs(10), ..ProxyConfig::default() };
    f(&mut cfg);
    let d = Directory::new(members.clone(), cfg);
    d.refresh().await.unwrap();
    d.spawn_refresh();
    let p = Proxy::new(d, waker);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let (_tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(p.clone().serve(l, ListenOptions::plain(Port::Rest), rx));
    // Keep the sender alive for the test's duration.
    std::mem::forget(_tx);
    Env { proxy: p, url, _members: members, http: reqwest::Client::new() }
}

impl Env {
    async fn get(&self, cluster: &str, path: &str) -> (u16, Value) {
        let r = self.http.get(format!("{}{path}", self.url)).basic_auth(cluster, Some("pw")).send().await.unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, cluster: &str, path: &str, body: &str) -> (u16, Value) {
        let r = self.http.post(format!("{}{path}", self.url)).basic_auth(cluster, Some("pw")).body(body.to_string()).send().await.unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn routes_by_rendezvous_and_follows_owners() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let nodes = [fake_node("a", leases.clone()).await, fake_node("b", leases.clone()).await, fake_node("c", leases.clone()).await];
    for n in &nodes {
        m.beat(&beat(n, "az1")).await.unwrap();
    }
    let e = proxy(m, "az1", None, |_| {}).await;
    // A cluster always reaches the same node.
    let (s, v) = e.get("c1", "/").await;
    assert_eq!(s, 200);
    let first = v["node"].as_str().unwrap().to_string();
    for _ in 0..5 {
        assert_eq!(e.get("c1", "/").await.1["node"], first.as_str());
    }
    // Many clusters spread over the nodes.
    let mut seen = std::collections::HashSet::new();
    for i in 0..40 {
        seen.insert(e.get(&format!("k{i}"), "/").await.1["node"].as_str().unwrap().to_string());
    }
    assert_eq!(seen.len(), 3, "{seen:?}");
    // A cluster held by another node (loaded there before this proxy's view changed) is
    // followed there through the refusal, and remembered.
    let rendezvous = e.get("moved", "/").await.1["node"].as_str().unwrap().to_string();
    let other = if rendezvous == "a" { "b" } else { "a" };
    leases.lock().insert("held".into(), other.into());
    let (s, v) = e.get("held", "/").await;
    assert_eq!((s, v["node"].as_str()), (200, Some(other)));
    assert_eq!(e.proxy.directory.owner_of("held").as_deref(), Some(other));
    let redirects = e.proxy.stats.redirects.load(Ordering::SeqCst);
    assert!(redirects <= 1);
    for _ in 0..5 {
        assert_eq!(e.get("held", "/").await.1["node"], other);
    }
    assert_eq!(e.proxy.stats.redirects.load(Ordering::SeqCst), redirects, "remembered: no more redirects");
    // Headers reach the node; hop-by-hop ones don't; X-Forwarded-For is set.
    let r = e.http.get(format!("{}/x?y=1", e.url)).basic_auth("c1", Some("pw")).header("keep-alive", "timeout=1").header("x-opaque-id", "7").send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["path"], "/x?y=1");
    assert_eq!(v["headers"]["x-opaque-id"], "7");
    assert!(v["headers"].get("keep-alive").is_none());
    assert_eq!(v["headers"]["x-forwarded-for"], "127.0.0.1");
    // Bodies arrive whole, buffered or streamed.
    assert_eq!(e.post("c1", "/i/_doc", &"x".repeat(1000)).await.1["body_len"], 1000);
}

/// A body over the resend bound can't be sent again after a refusal: the client gets the refusal,
/// and its retry goes straight to the owner the refusal named.
#[tokio::test]
async fn a_refused_streamed_body_teaches_the_owner() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let nodes = [fake_node("a", leases.clone()).await, fake_node("b", leases.clone()).await];
    for n in &nodes {
        m.beat(&beat(n, "az1")).await.unwrap();
    }
    let e = proxy(m, "az1", None, |c| c.retry_body_bytes = 100).await;
    let rendezvous = e.get("probe", "/").await.1["node"].as_str().unwrap().to_string();
    leases.lock().remove("probe");
    let other = if rendezvous == "a" { "b" } else { "a" };
    leases.lock().insert("probe".into(), other.into());
    let big = "x".repeat(1000);
    let (s, v) = e.post("probe", "/i/_bulk", &big).await;
    assert_eq!(s, 503, "{v}");
    assert_eq!(e.proxy.directory.owner_of("probe").as_deref(), Some(other));
    let (s, v) = e.post("probe", "/i/_bulk", &big).await;
    assert_eq!((s, v["node"].as_str(), v["body_len"].as_u64()), (200, Some(other), Some(1000)));
}

/// Clusters `a` serves by rendezvous, with their lease moved to `b` (as after a failover the
/// proxy hasn't seen yet): the proxy's first attempt is refused.
async fn moved_away(e: &Env, leases: &Leases, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let c = format!("mv{i}");
        let on = e.get(&c, "/").await.1["node"].as_str().unwrap().to_string();
        leases.lock().insert(c.clone(), if on == "a" { "b" } else { "a" }.into());
        e.proxy.directory.forget_owner(&c);
        out.push(c);
        if out.len() == n {
            return out;
        }
    }
    unreachable!()
}

/// A refusal can come while the client is still uploading (the node refuses before reading
/// the body): the proxy resends what was sent so far to the owner and streams the rest there.
#[tokio::test]
async fn a_body_refused_mid_upload_is_resent_whole() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let nodes = [fake_node("a", leases.clone()).await, fake_node("b", leases.clone()).await];
    for n in &nodes {
        m.beat(&beat(n, "az1")).await.unwrap();
    }
    let e = proxy(m, "az1", None, |_| {}).await;
    let moved = moved_away(&e, &leases, 2).await;
    let owner = |c: &str| leases.lock()[c].clone();
    // Chunked, with a pause after the first chunk so the refusal arrives mid-upload.
    let addr = e.url.strip_prefix("http://").unwrap().to_string();
    let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:pw", moved[0]));
    let head = format!("POST /i/_bulk HTTP/1.1\r\nhost: x\r\nauthorization: Basic {auth}\r\ntransfer-encoding: chunked\r\ncontent-type: application/x-ndjson\r\n\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    let line = "{\"index\":{}}\n{\"f\":1}\n";
    s.write_all(format!("{:x}\r\n{line}\r\n", line.len()).as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    for _ in 0..3 {
        s.write_all(format!("{:x}\r\n{line}\r\n", line.len()).as_bytes()).await.unwrap();
    }
    s.write_all(b"0\r\n\r\n").await.unwrap();
    let mut resp = Vec::new();
    let mut buf = [0u8; 4096];
    // Read until the head and the whole Content-Length body are in (a response may arrive in
    // several reads).
    let complete = |r: &[u8]| {
        let t = String::from_utf8_lossy(r);
        let Some(end) = t.find("\r\n\r\n") else { return false };
        let len = t[..end].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()));
        len.is_some_and(|n| r.len() >= end + 4 + n)
    };
    while !complete(&resp) {
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await.unwrap().unwrap();
        assert!(n > 0, "{}", String::from_utf8_lossy(&resp));
        resp.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    let v: Value = serde_json::from_str(&text[text.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!((v["node"].as_str(), v["body_len"].as_u64()), (Some(owner(&moved[0]).as_str()), Some(4 * line.len() as u64)), "{v}");
    assert!(e.proxy.stats.redirects.load(Ordering::SeqCst) >= 1);
    // A declared body larger than the old store-and-forward limit (16 MiB) is resent too.
    let big = "x".repeat(20 << 20);
    let (st, v) = e.post(&moved[1], "/i/_bulk", &big).await;
    assert_eq!((st, v["node"].as_str(), v["body_len"].as_u64()), (200, Some(owner(&moved[1]).as_str()), Some(20 << 20)));
    assert_eq!(e.proxy.budget.used(), 0, "nothing stays kept after the requests");
}

#[tokio::test]
async fn own_az_first_then_any() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let a = fake_node("a1", leases.clone()).await;
    let b = fake_node("b1", leases.clone()).await;
    m.beat(&beat(&a, "az-a")).await.unwrap();
    m.beat(&beat(&b, "az-b")).await.unwrap();
    let e = proxy(m.clone(), "az-b", None, |_| {}).await;
    for i in 0..10 {
        assert_eq!(e.get(&format!("c{i}"), "/").await.1["node"], "b1");
    }
    // Draining nodes take no new cluster; the other AZ does.
    let mut d = beat(&b, "az-b");
    d.draining = true;
    m.beat(&d).await.unwrap();
    e.proxy.directory.refresh().await.unwrap();
    assert_eq!(e.get("fresh", "/").await.1["node"], "a1");
    // A draining owner still serves its clusters.
    assert_eq!(e.get("c1", "/").await.1["node"], "b1");
}

/// A fake ECS API: answers DescribeServices with the current desired count and, on
/// UpdateService, "starts" a node that heartbeats.
async fn fake_ecs(on_update: impl Fn(u64) + Send + Sync + 'static) -> (SocketAddr, Arc<Mutex<Vec<(String, Value, String)>>>) {
    let calls: Arc<Mutex<Vec<(String, Value, String)>>> = Arc::default();
    let desired = Arc::new(Mutex::new(0u64));
    let on_update = Arc::new(on_update);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let c = calls.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            let (c, desired, on_update) = (c.clone(), desired.clone(), on_update.clone());
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let (c, desired, on_update) = (c.clone(), desired.clone(), on_update.clone());
                    async move {
                        let target = req.headers().get("x-amz-target").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
                        let auth = req.headers().get("authorization").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
                        let body: Value = serde_json::from_slice(&req.into_body().collect().await.unwrap().to_bytes()).unwrap();
                        c.lock().push((target.clone(), body.clone(), auth));
                        let out = if target.ends_with("DescribeServices") {
                            json!({"services": [{"serviceName": body["services"][0], "desiredCount": *desired.lock()}], "failures": []})
                        } else {
                            let n = body["desiredCount"].as_u64().unwrap();
                            *desired.lock() = n;
                            on_update(n);
                            json!({"service": {"desiredCount": n}})
                        };
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(out.to_string()))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(s), svc).await;
            });
        }
    });
    (addr, calls)
}

#[tokio::test]
async fn holds_requests_and_wakes_the_fleet_from_zero() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let started = Arc::new(AtomicUsize::new(0));
    let (m2, leases2, started2) = (m.clone(), leases.clone(), started.clone());
    let (ecs, calls) = fake_ecs(move |n| {
        assert_eq!(n, 2);
        let (m, leases, started) = (m2.clone(), leases2.clone(), started2.clone());
        // The task takes a while to boot, then heartbeats.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let node = fake_node("woken", leases).await;
            m.beat(&beat(&node, "az1")).await.unwrap();
            started.fetch_add(1, Ordering::SeqCst);
            std::mem::forget(node);
        });
    })
    .await;
    let waker = EcsWaker::new("roll".into(), "search-nodes".into(), 2, "eu-west-1".into())
        .with_endpoint(format!("http://{ecs}"))
        .with_credentials(Credentials { key: "AKIDEXAMPLE".into(), secret: "secret".into(), token: Some("tok".into()) });
    let e = proxy(m, "az1", Some(Arc::new(waker)), |_| {}).await;
    // Several requests arrive while nothing runs: all are held, the fleet is woken once.
    let started_at = std::time::Instant::now();
    let reqs: Vec<_> = (0..5)
        .map(|i| {
            let (http, url) = (e.http.clone(), e.url.clone());
            tokio::spawn(async move { http.get(format!("{url}/")).basic_auth(format!("c{i}"), Some("pw")).send().await.unwrap().json::<Value>().await.unwrap() })
        })
        .collect();
    for r in reqs {
        assert_eq!(r.await.unwrap()["node"], "woken");
    }
    assert!(started_at.elapsed() >= Duration::from_millis(400));
    assert_eq!(started.load(Ordering::SeqCst), 1);
    let calls = calls.lock().clone();
    let targets: Vec<&str> = calls.iter().map(|(t, _, _)| t.as_str()).collect();
    assert_eq!(targets, ["AmazonEC2ContainerServiceV20141113.DescribeServices", "AmazonEC2ContainerServiceV20141113.UpdateService"], "woken once");
    assert_eq!(calls[1].1, json!({"cluster": "roll", "service": "search-nodes", "desiredCount": 2}));
    assert!(calls[1].2.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"), "{}", calls[1].2);
    assert!(calls[1].2.contains("/eu-west-1/ecs/aws4_request"));
    assert!(e.proxy.stats.held.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn a_wake_never_lowers_the_desired_count() {
    let (ecs, calls) = fake_ecs(|_| {}).await;
    let w = EcsWaker::new("roll".into(), "svc".into(), 1, "us-east-1".into())
        .with_endpoint(format!("http://{ecs}"))
        .with_credentials(Credentials { key: "k".into(), secret: "s".into(), token: None });
    w.wake().await.unwrap();
    assert_eq!(calls.lock().len(), 2, "0 → 1");
    w.wake().await.unwrap();
    assert_eq!(calls.lock().len(), 3, "already 1: describe only");
}

#[tokio::test]
async fn gives_up_after_the_hold_timeout() {
    let e = proxy(MemoryMembers::new(), "az1", None, |c| c.hold_timeout = Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    let r = e.http.get(format!("{}/", e.url)).basic_auth("c", Some("pw")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 503);
    assert_eq!(r.headers()["x-elastic-product"], "Elasticsearch");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "master_not_discovered_exception");
    assert_eq!(v["status"], 503);
    assert!(t.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn retries_across_a_shutdown_handoff() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let a = fake_node("a", leases.clone()).await;
    let b = fake_node("b", leases.clone()).await;
    m.beat(&beat(&a, "az1")).await.unwrap();
    m.beat(&beat(&b, "az1")).await.unwrap();
    let e = proxy(m.clone(), "az1", None, |_| {}).await;
    // Find a cluster whose rendezvous node is `a`, and let `a` load it.
    let mut c = String::new();
    for i in 0.. {
        c = format!("h{i}");
        if e.get(&c, "/").await.1["node"] == "a" {
            break;
        }
        leases.lock().clear();
    }
    // `a` gets SIGTERM: it drains (no new clusters), still holds `c` while it flushes, then
    // hands the lease back. Writes sent meanwhile are refused before running and resent.
    a.closed.store(true, Ordering::SeqCst);
    let mut d = beat(&a, "az1");
    d.draining = true;
    m.beat(&d).await.unwrap();
    let writes: Vec<_> = (0..4)
        .map(|i| {
            let (http, url, c) = (e.http.clone(), e.url.clone(), c.clone());
            tokio::spawn(async move {
                let r = http.post(format!("{url}/idx/_doc/{i}")).basic_auth(&c, Some("pw")).body(r#"{"f":1}"#).send().await.unwrap();
                (r.status().as_u16(), r.json::<Value>().await.unwrap())
            })
        })
        .collect();
    // While `a` still holds it, its own requests are served by it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The handoff: `a` releases the lease after flushing; `b` takes over.
    let l2 = leases.clone();
    let cc = c.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        l2.lock().remove(&cc);
    });
    for w in writes {
        let (s, v) = w.await.unwrap();
        assert_eq!(s, 200, "{v}");
        assert!(v["node"] == "a" || v["node"] == "b", "{v}");
    }
    // After the handoff everything lands on `b`, `a` is gone.
    tokio::time::sleep(Duration::from_millis(350)).await;
    m.leave("a").await.unwrap();
    let (s, v) = e.post(&c, "/idx/_doc/9", "{}").await;
    assert_eq!((s, v["node"].as_str()), (200, Some("b")), "{v}");
}

#[tokio::test]
async fn dead_and_broken_nodes() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let b = fake_node("b", leases.clone()).await;
    m.beat(&beat(&b, "az1")).await.unwrap();
    // A node record whose process is gone (connection refused).
    let gone = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    m.beat(&Beat { id: "a".into(), http: gone.to_string(), api: None, az: Some("az1".into()), ..Beat::default() }).await.unwrap();
    let e = proxy(m.clone(), "az1", None, |_| {}).await;
    for i in 0..10 {
        let (s, v) = e.post(&format!("x{i}"), "/i/_doc", "{}").await;
        assert_eq!((s, v["node"].as_str()), (200, Some("b")), "unsent requests are resent: {v}");
    }
    // A node that drops connections after reading a request: writes get a 502 (they may have
    // run), reads are resent.
    let c = fake_node("c", leases.clone()).await;
    m.leave("a").await.unwrap();
    m.beat(&beat(&c, "az1")).await.unwrap();
    e.proxy.directory.refresh().await.unwrap();
    let mut on_c = Vec::new();
    for i in 0.. {
        let name = format!("y{i}");
        if e.get(&name, "/").await.1["node"] == "c" {
            on_c.push(name);
            if on_c.len() == 2 {
                break;
            }
        }
    }
    leases.lock().clear();
    c.broken.store(true, Ordering::SeqCst);
    let r = e.http.put(format!("{}/i/_doc/1", e.url)).basic_auth(&on_c[0], Some("pw")).body("{}").send().await.unwrap();
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap();
    assert_eq!(status, 502, "{v}");
    assert_eq!(v["error"]["type"], "node_disconnected_exception", "{v}");
    // A broken connection doesn't take `c` out of rotation (a client can provoke one); a read
    // reaching it is resent to `b`.
    assert_eq!(e.proxy.directory.live(), ["b", "c"]);
    let (s, v) = e.get(&on_c[1], "/i/_search").await;
    assert_eq!((s, v["node"].as_str()), (200, Some("b")), "{v}");
    assert!(e.proxy.stats.retries.load(Ordering::SeqCst) >= 1);
    assert!(c.hits.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn tls_with_sni_routing_and_the_proxy_protocol() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let n = fake_node("n", leases.clone()).await;
    m.beat(&beat(&n, "az1")).await.unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["*.search.test".to_string()]).unwrap();
    let acceptor = search_proxy::proxy::tls_acceptor(cert.cert.pem().as_bytes(), cert.signing_key.serialize_pem().as_bytes()).unwrap();
    let cfg = ProxyConfig { az: Some("az1".into()), domain: Some("search.test".into()), refresh: Duration::from_millis(50), ..ProxyConfig::default() };
    let d = Directory::new(m.clone(), cfg);
    d.refresh().await.unwrap();
    let p = Proxy::new(d, None);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let tl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taddr = tl.local_addr().unwrap();
    tokio::spawn(p.clone().serve(tl, ListenOptions { port: Port::Rest, tls: Some(acceptor), proxy_protocol: false, drain: Duration::from_secs(1) }, rx.clone()));
    let http = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert.cert.pem().as_bytes()).unwrap())
        .resolve("app.search.test", taddr)
        .build()
        .unwrap();
    // No credentials: the TLS name picks the cluster, and the node sees it in `Host`.
    let v: Value = http.get(format!("https://app.search.test:{}/", taddr.port())).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["node"], "n");
    assert_eq!(v["headers"]["host"], format!("app.search.test:{}", taddr.port()));
    assert_eq!(v["headers"]["x-forwarded-proto"], "https");
    assert_eq!(leases.lock().get("app").map(String::as_str), None, "the fake node reads Basic only");
    // PROXY protocol: the client address reaches X-Forwarded-For.
    let pl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let paddr = pl.local_addr().unwrap();
    tokio::spawn(p.clone().serve(pl, ListenOptions { port: Port::Rest, tls: None, proxy_protocol: true, drain: Duration::from_secs(1) }, rx));
    let mut s = tokio::net::TcpStream::connect(paddr).await.unwrap();
    s.write_all(b"PROXY TCP4 203.0.113.50 10.0.0.1 40000 9200\r\nGET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\nauthorization: Basic YXBwOnB3\r\n\r\n").await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("\"x-forwarded-for\":\"203.0.113.50\""), "{text}");
    // A connection without the header is refused.
    let mut s = tokio::net::TcpStream::connect(paddr).await.unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n").await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut out)).await;
    assert!(out.is_empty());
}

#[tokio::test]
async fn graceful_shutdown_finishes_requests_in_progress() {
    // A slow node: answers after 300 ms.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(|_req: Request<Incoming>| async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from_static(b"{\"ok\":true}"))))
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(s), svc).await;
            });
        }
    });
    let m = MemoryMembers::new();
    m.beat(&Beat { id: "slow".into(), http: addr.to_string(), api: None, az: None, ..Beat::default() }).await.unwrap();
    let d = Directory::new(m, ProxyConfig::default());
    d.refresh().await.unwrap();
    let p = Proxy::new(d, None);
    let pl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", pl.local_addr().unwrap());
    let (tx, rx) = tokio::sync::watch::channel(false);
    let served = tokio::spawn(p.serve(pl, ListenOptions::plain(Port::Rest), rx));
    let http = reqwest::Client::new();
    let inflight = tokio::spawn(async move { http.get(format!("{url}/")).send().await.unwrap().status().as_u16() });
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send(true).unwrap();
    assert_eq!(inflight.await.unwrap(), 200, "the request in progress finished");
    tokio::time::timeout(Duration::from_secs(5), served).await.unwrap().unwrap().unwrap();
}

/// A record with a certificate pin is reached only over TLS that presents that certificate: a
/// plain-HTTP process at the address (an impostor, or a node record gone stale) gets no request,
/// which goes to another node instead; a record with a malformed pin is ignored altogether.
#[tokio::test]
async fn pinned_nodes_get_requests_only_over_matching_tls() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let (b, c) = (fake_node("b", leases.clone()).await, fake_node("c", leases.clone()).await);
    let pin = search_proxy::upstream::self_signed(&[]).unwrap().fingerprint;
    m.beat(&Beat { tls: Some(pin), ..beat(&b, "az1") }).await.unwrap();
    m.beat(&beat(&c, "az1")).await.unwrap();
    let bad = fake_node("bad", leases.clone()).await;
    m.beat(&Beat { tls: Some("not-a-pin".into()), ..beat(&bad, "az1") }).await.unwrap();
    let e = proxy(m.clone(), "az1", None, |_| {}).await;
    assert_eq!(e.proxy.directory.live(), ["b", "c"], "the malformed pin's node is skipped");
    assert!(e.proxy.directory.tls(&b.addr) && !e.proxy.directory.tls(&c.addr));
    for i in 0..10 {
        let (s, v) = e.post(&format!("p{i}"), "/i/_doc", "{}").await;
        assert_eq!((s, v["node"].as_str()), (200, Some("c")), "{v}");
    }
    assert_eq!((b.hits.load(Ordering::SeqCst), bad.hits.load(Ordering::SeqCst)), (0, 0));
}

/// A node whose record says its API port serves TLS too is reached there only over TLS with the
/// pinned certificate, like its REST port; the API listener of the proxy stays what it is.
#[tokio::test]
async fn the_api_port_reaches_pinned_nodes_over_tls() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let cert = search_proxy::upstream::self_signed(&[]).unwrap();
    let acceptor = search_proxy::proxy::tls_acceptor(cert.cert_pem.as_bytes(), cert.key_pem.as_bytes()).unwrap();
    let t = fake_node_with("t", leases.clone(), Some(acceptor)).await;
    m.beat(&Beat { tls: Some(cert.fingerprint.clone()), api_tls: true, ..beat(&t, "az1") }).await.unwrap();
    let d = Directory::new(m.clone(), ProxyConfig { az: Some("az1".into()), hold_timeout: Duration::from_secs(2), ..ProxyConfig::default() });
    d.refresh().await.unwrap();
    assert!(d.tls(&t.addr));
    let p = Proxy::new(d.clone(), None);
    let (rest, api) = (TcpListener::bind("127.0.0.1:0").await.unwrap(), TcpListener::bind("127.0.0.1:0").await.unwrap());
    let (rest_url, api_url) = (format!("http://{}", rest.local_addr().unwrap()), format!("http://{}", api.local_addr().unwrap()));
    let (_tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(p.clone().serve(rest, ListenOptions::plain(Port::Rest), rx.clone()));
    tokio::spawn(p.clone().serve(api, ListenOptions::plain(Port::Api), rx));
    let http = reqwest::Client::new();
    let r = http.get(format!("{api_url}/v1/search/clusters/shop/indices")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!((v["node"].as_str(), v["path"].as_str()), (Some("t"), Some("/v1/search/clusters/shop/indices")));
    let r = http.get(format!("{rest_url}/")).basic_auth("shop", Some("pw")).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["node"], "t");
    // A proxy that finds another certificate's pin in the record (connections are pooled per
    // verified peer, so this is a fresh one): its API port sends the node nothing.
    let before = t.hits.load(Ordering::SeqCst);
    let other = search_proxy::upstream::self_signed(&[]).unwrap().fingerprint;
    m.beat(&Beat { tls: Some(other), api_tls: true, ..beat(&t, "az1") }).await.unwrap();
    let d2 = Directory::new(m.clone(), ProxyConfig { az: Some("az1".into()), hold_timeout: Duration::from_secs(1), ..ProxyConfig::default() });
    d2.refresh().await.unwrap();
    let api2 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let api2_url = format!("http://{}", api2.local_addr().unwrap());
    let (_tx2, rx2) = tokio::sync::watch::channel(false);
    tokio::spawn(Proxy::new(d2, None).serve(api2, ListenOptions::plain(Port::Api), rx2));
    let r = http.get(format!("{api2_url}/v1/search/clusters/shop/indices")).send().await.unwrap();
    assert!(r.status().is_server_error(), "{}", r.status());
    assert_eq!(t.hits.load(Ordering::SeqCst), before, "nothing reached a node presenting another certificate");
}

/// Sends a request head declaring a 100,000-byte body, five bytes of it, then hangs up.
async fn abandon_upload(e: &Env, cluster: &str, path: &str) {
    let addr = e.url.strip_prefix("http://").unwrap().to_string();
    let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{cluster}:pw"));
    let head = format!("POST {path} HTTP/1.1\r\nhost: x\r\nauthorization: Basic {auth}\r\ncontent-type: application/json\r\ncontent-length: 100000\r\n\r\n{{\"a\":");
    s.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(s);
}

/// A client that disconnects mid-upload breaks the proxy's connection to the node too, but the
/// node did nothing wrong: it stays in rotation for everyone else, and nothing is resent (not
/// even a read-only `_search`, whose body is gone).
#[tokio::test]
async fn a_client_abandoning_its_upload_blames_no_node() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let nodes = [fake_node("a", leases.clone()).await, fake_node("b", leases.clone()).await];
    for n in &nodes {
        m.beat(&beat(n, "az1")).await.unwrap();
    }
    let e = proxy(m, "az1", None, |_| {}).await;
    abandon_upload(&e, "app", "/idx/_doc").await;
    abandon_upload(&e, "app", "/idx/_search").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(e.proxy.stats.requests.load(Ordering::SeqCst), 2);
    assert_eq!(e.proxy.directory.live(), ["a", "b"], "no node is marked bad");
    assert_eq!(e.proxy.stats.retries.load(Ordering::SeqCst), 0, "nothing is resent");
    // Another client's request goes straight through, without waiting for any node.
    let started = std::time::Instant::now();
    let (s, v) = e.post("app", "/idx/_doc", "{}").await;
    assert_eq!(s, 200, "{v}");
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
}

/// A read-only request whose connection breaks is resent once, to another node, not walked
/// through the fleet (a request that crashes whichever node runs it would hit every node).
#[tokio::test]
async fn a_broken_read_is_resent_once() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let nodes = [fake_node("a", leases.clone()).await, fake_node("b", leases.clone()).await, fake_node("c", leases.clone()).await];
    for n in &nodes {
        n.broken.store(true, Ordering::SeqCst);
        m.beat(&beat(n, "az1")).await.unwrap();
    }
    let e = proxy(m, "az1", None, |_| {}).await;
    let (s, v) = e.get("app", "/idx/_search").await;
    assert_eq!((s, v["error"]["type"].as_str()), (502, Some("node_disconnected_exception")), "{v}");
    assert_eq!(e.proxy.stats.retries.load(Ordering::SeqCst), 1);
    assert_eq!(e.proxy.directory.live(), ["a", "b", "c"], "broken connections mark no node bad");
}

/// A 503 too large to be a refusal (a search_phase_execution_exception listing many shard
/// failures) reaches the client as the node sent it.
#[tokio::test]
async fn a_large_503_reaches_the_client_whole() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let a = fake_node("a", leases.clone()).await;
    m.beat(&beat(&a, "az1")).await.unwrap();
    let e = proxy(m, "az1", None, |_| {}).await;
    for size in [100, 100_000, 3 << 20] {
        a.big_503.store(size, Ordering::SeqCst);
        let r = e.http.post(format!("{}/idx/_search", e.url)).basic_auth("app", Some("pw")).body("{}").send().await.unwrap();
        assert_eq!(r.status(), 503);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["type"], "search_phase_execution_exception", "{size}");
        assert_eq!(v["error"]["reason"].as_str().map(str::len), Some(size));
    }
    assert_eq!(e.proxy.stats.retries.load(Ordering::SeqCst), 0);
}

/// Only answers that show the node holds the cluster teach the proxy its owner: not a 401 for
/// bad credentials (checked before leases), and not the control API (whose node forwards to
/// the owner itself).
#[tokio::test]
async fn bad_credentials_and_api_answers_never_move_the_owner() {
    let leases = Leases::default();
    let m = MemoryMembers::new();
    let nodes = [fake_node("a", leases.clone()).await, fake_node("b", leases.clone()).await];
    for n in &nodes {
        m.beat(&beat(n, "az1")).await.unwrap();
    }
    let e = proxy(m, "az1", None, |_| {}).await;
    let api = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let api_url = format!("http://{}", api.local_addr().unwrap());
    let (_tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(e.proxy.clone().serve(api, ListenOptions::plain(Port::Api), rx));
    // `shop` lives on the node that isn't its rendezvous node.
    let rendezvous = e.get("shop", "/").await.1["node"].as_str().unwrap().to_string();
    let owner = if rendezvous == "a" { "b" } else { "a" };
    leases.lock().insert("shop".into(), owner.into());
    e.proxy.directory.forget_owner("shop");
    // Wrong password: the rendezvous node answers 401 before looking at leases.
    let r = e.http.get(format!("{}/", e.url)).basic_auth("shop", Some("bad")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(e.proxy.directory.owner_of("shop"), None, "a 401 says nothing about the owner");
    let (s, v) = e.get("shop", "/").await;
    assert_eq!((s, v["node"].as_str()), (200, Some(owner)));
    assert_eq!(e.proxy.directory.owner_of("shop").as_deref(), Some(owner));
    // With the owner cached, bad credentials reach the owner and leave the entry as it is.
    for _ in 0..3 {
        let r = e.http.get(format!("{}/", e.url)).basic_auth("shop", Some("bad")).send().await.unwrap();
        assert_eq!(r.status(), 401);
    }
    assert_eq!(e.proxy.directory.owner_of("shop").as_deref(), Some(owner));
    // The control API: answered by whichever node took it.
    e.proxy.directory.forget_owner("shop");
    let r = e.http.get(format!("{api_url}/v1/search/clusters/shop/indices")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["node"], rendezvous.as_str());
    assert_eq!(e.proxy.directory.owner_of("shop"), None, "API answers don't name the owner");
}

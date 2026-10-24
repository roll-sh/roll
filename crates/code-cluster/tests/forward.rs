//! Forwarding end to end, with real axum servers on random ports: methods, headers, queries,
//! large streamed bodies, streamed responses, retry after Unavailable, the hop limit, and
//! signature checks.

mod common;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use code_cluster::{Cluster, ClusterConfig, Forwarded, Guard, MAX_HOPS, Node, accept_forwarded, hops, unavailable};
use common::*;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

struct TestNode {
    cluster: Arc<Cluster>,
    addr: SocketAddr,
    flaky: Arc<AtomicUsize>,
    loops: Arc<AtomicUsize>,
    peer: Arc<OnceLock<Node>>,
}

impl TestNode {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

#[derive(Clone)]
struct AppState {
    cluster: Arc<Cluster>,
    flaky: Arc<AtomicUsize>,
    loops: Arc<AtomicUsize>,
    peer: Arc<OnceLock<Node>>,
}

async fn routed(State(s): State<AppState>, Path((key, _rest)): Path<(String, String)>, req: Request) -> Response {
    let st = s.clone();
    s.cluster.dispatch(&key, req, move |req, guard| handle(st, req, guard)).await
}

/// The application, run where the key lives.
async fn handle(s: AppState, req: Request, guard: Guard) -> Response {
    let me = s.cluster.node_id().to_string();
    let path = req.uri().path().to_string();
    let mut resp = if path.ends_with("/echo") {
        let fwd = req.extensions().get::<Forwarded>().cloned();
        let mut lines = vec![
            format!("node={me}"),
            format!("epoch={}", guard.epoch()),
            format!("method={}", req.method()),
            format!("pq={}", req.uri().path_and_query().unwrap()),
            format!("from={}", fwd.as_ref().map_or("-".into(), |f| f.node.clone())),
            format!("hops={}", fwd.as_ref().map_or(0, |f| f.hops)),
            format!("client={}", fwd.as_ref().and_then(|f| f.client.clone()).unwrap_or("-".into())),
        ];
        for h in ["x-custom", "x-drop", "host", "transfer-encoding", "content-length"] {
            let v: Vec<&str> = req.headers().get_all(h).iter().map(|v| v.to_str().unwrap()).collect();
            lines.push(format!("{h}={}", v.join(",")));
        }
        let body = req.into_body().collect().await.unwrap().to_bytes();
        lines.push(format!("body={}", String::from_utf8_lossy(&body)));
        lines.join("\n").into_response()
    } else if path.ends_with("/upload") {
        // Read it all, then send it back: both directions stream through the forwarder.
        let body = req.into_body().collect().await.unwrap().to_bytes();
        Response::new(Body::from(body))
    } else if path.ends_with("/events") {
        let stream = futures::stream::unfold(0, |i| async move {
            if i == 5 {
                return None;
            }
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Some((Ok::<_, std::convert::Infallible>(Bytes::from(format!("data: {i}\n\n"))), i + 1))
        });
        let mut r = Response::new(Body::from_stream(stream));
        r.headers_mut().insert("content-type", HeaderValue::from_static("text/event-stream"));
        r
    } else if path.ends_with("/flaky") {
        // Read the body first, so a client still sending it sees the answer, not a broken pipe.
        let body = req.into_body().collect().await.unwrap().to_bytes();
        if s.flaky.fetch_add(1, Ordering::SeqCst) == 0 {
            return unavailable(Duration::from_millis(100));
        }
        format!("ok {} bytes", body.len()).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    };
    resp.headers_mut().insert("x-echo-node", HeaderValue::from_str(&me).unwrap());
    resp
}

/// Forwards everything to the peer node, whatever the key: a loop.
async fn looping(State(s): State<AppState>, req: Request) -> Response {
    s.loops.fetch_add(1, Ordering::SeqCst);
    let hops = hops(&req);
    let peer = s.peer.get().unwrap().clone();
    s.cluster.forward(&peer, req, hops).await
}

async fn start(env: &Env, id: &str) -> TestNode {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = env.scaled(ClusterConfig::for_tests(id, format!("http://{addr}"), SECRET));
    let db = env.node_db();
    let cluster = tokio::task::spawn_blocking(move || Cluster::join(db, cfg).unwrap()).await.unwrap();
    let state = AppState { cluster: cluster.clone(), flaky: Arc::default(), loops: Arc::default(), peer: Arc::default() };
    let app = Router::new()
        .route("/r/{key}/{*rest}", any(routed))
        .route("/loop/{*rest}", any(looping))
        .with_state(state.clone())
        .merge(cluster.router())
        .layer(axum::middleware::from_fn_with_state(cluster.clone(), accept_forwarded));
    tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap() });
    TestNode { cluster, addr, flaky: state.flaky, loops: state.loops, peer: state.peer }
}

async fn pair(name: &str) -> (Env, TestNode, TestNode) {
    let env = Env::new(name);
    let a = start(&env, "a").await;
    let b = start(&env, "b").await;
    let _ = a.peer.set(b.cluster.node());
    let _ = b.peer.set(a.cluster.node());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(a.cluster.nodes().len() == 2 && b.cluster.nodes().len() == 2) {
        assert!(Instant::now() < deadline, "views never converged");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (env, a, b)
}

fn parse(text: &str) -> HashMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwards_methods_headers_and_queries_to_the_owner() {
    let (_env, a, b) = pair("fwd_basic").await;
    let key = key_for(&["a", "b"], "b", "repo");
    let http = client();
    for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let pq = format!("/r/{key}/echo?x=1&y=a%20b&z=%2F");
        let body = if method == Method::GET { "" } else { "payload" };
        let resp = http
            .request(method.clone(), a.url(&pq))
            .header("x-custom", "v1")
            .header("connection", "x-drop")
            .header("x-drop", "must not pass")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{method}");
        assert_eq!(resp.headers()["x-echo-node"], "b", "response headers come back");
        let m = parse(&resp.text().await.unwrap());
        assert_eq!(m["node"], "b", "{method}: ran on the owner");
        assert_eq!(m["method"], method.as_str());
        assert_eq!(m["pq"], pq, "path and query arrive unchanged");
        assert_eq!(m["from"], "a");
        assert_eq!(m["hops"], "1");
        assert_eq!(m["client"], "127.0.0.1");
        assert_eq!(m["x-custom"], "v1");
        assert_eq!(m["x-drop"], "", "headers named in Connection are hop-by-hop");
        assert_eq!(m["host"], format!("{}", a.addr), "the original Host is kept");
        assert_eq!(m["body"], body);
        assert_eq!(m["epoch"], "1");
    }
    // Straight to the owner: runs locally, not forwarded.
    let m = parse(&http.get(b.url(&format!("/r/{key}/echo"))).send().await.unwrap().text().await.unwrap());
    assert_eq!((m["node"].as_str(), m["from"].as_str(), m["hops"].as_str()), ("b", "-", "0"));
    // And a key a owns runs on a.
    let mine = key_for(&["a", "b"], "a", "repo");
    let m = parse(&http.get(a.url(&format!("/r/{mine}/echo"))).send().await.unwrap().text().await.unwrap());
    assert_eq!(m["node"], "a");
    assert!(b.cluster.owns(&key) && a.cluster.owns(&mine));
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_large_bodies_both_ways() {
    let (_env, a, _b) = pair("fwd_large").await;
    let key = key_for(&["a", "b"], "b", "repo");
    let http = client();
    let url = a.url(&format!("/r/{key}/upload"));
    // Sized and bigger than the buffer (streamed), unsized (chunked, streamed), and small
    // (buffered).
    for (name, size, chunked) in [("16 MiB", 16 << 20, false), ("12 MiB chunked", 12 << 20, true), ("1 MiB", 1 << 20, false)] {
        let data = pattern(size);
        let want = Sha256::digest(&data);
        let body = if chunked {
            let chunks: Vec<Result<Vec<u8>, std::io::Error>> = data.chunks(64 << 10).map(|c| Ok(c.to_vec())).collect();
            reqwest::Body::wrap_stream(futures::stream::iter(chunks))
        } else {
            reqwest::Body::from(data.clone())
        };
        let resp = http.post(&url).body(body).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{name}");
        assert_eq!(resp.headers()["x-echo-node"], "b");
        let mut stream = resp.bytes_stream();
        let (mut got, mut n, mut chunks) = (Sha256::new(), 0usize, 0usize);
        while let Some(c) = stream.next().await {
            let c = c.unwrap();
            n += c.len();
            chunks += 1;
            got.update(&c);
        }
        assert_eq!(n, size, "{name}");
        assert_eq!(got.finalize(), want, "{name}: the body came back intact");
        eprintln!("{name}: {chunks} chunks back");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_responses_incrementally() {
    let (_env, a, _b) = pair("fwd_sse").await;
    let key = key_for(&["a", "b"], "b", "repo");
    let started = Instant::now();
    let resp = client().get(a.url(&format!("/r/{key}/events"))).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut stream = resp.bytes_stream();
    let mut arrivals = Vec::new();
    let mut text = String::new();
    while let Some(c) = stream.next().await {
        text.push_str(std::str::from_utf8(&c.unwrap()).unwrap());
        arrivals.push(started.elapsed());
    }
    assert_eq!(text, (0..5).map(|i| format!("data: {i}\n\n")).collect::<String>());
    let (first, last) = (arrivals[0], *arrivals.last().unwrap());
    eprintln!("first event after {first:?}, last after {last:?}, {} chunks", arrivals.len());
    assert!(arrivals.len() >= 3, "events arrived in one lump: {arrivals:?}");
    assert!(last - first >= Duration::from_millis(700), "events weren't streamed as they were sent: {arrivals:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_once_after_the_owner_answers_unavailable() {
    let (_env, a, b) = pair("fwd_retry").await;
    let key = key_for(&["a", "b"], "b", "repo");
    let http = client();
    let resp = http.post(a.url(&format!("/r/{key}/flaky"))).body("hello").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok 5 bytes");
    assert_eq!(b.flaky.load(Ordering::SeqCst), 2, "the forwarder routed again and retried once");

    // A body too big to buffer is streamed once: the Unavailable answer reaches the client.
    b.flaky.store(0, Ordering::SeqCst);
    let resp = http.post(a.url(&format!("/r/{key}/flaky"))).body(pattern(8 << 20)).send().await.unwrap();
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("retry-after"));
    assert!(resp.headers().contains_key("x-roll-code-unavailable"));
    assert_eq!(b.flaky.load(Ordering::SeqCst), 1);

    // A node that can't be reached: 503 (retryable), after one retry.
    let gone = Node { id: "gone".into(), url: "http://127.0.0.1:1".into() };
    let req = Request::builder().uri("/r/x/echo").body(Body::from("x")).unwrap();
    let resp = a.cluster.forward(&gone, req, 0).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("x-roll-code-unavailable"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refuses_to_forward_past_the_hop_limit() {
    let (_env, a, b) = pair("fwd_hops").await;
    let resp = client().get(a.url("/loop/x")).send().await.unwrap();
    assert_eq!(resp.status(), 508);
    // a (0) → b (1) → a (2) → b (3), which refuses.
    assert_eq!(a.loops.load(Ordering::SeqCst) + b.loops.load(Ordering::SeqCst), MAX_HOPS as usize + 1);
    let req = Request::builder().uri("/r/x/echo").body(Body::empty()).unwrap();
    assert_eq!(a.cluster.forward(&b.cluster.node(), req, MAX_HOPS).await.status(), 508);
}

fn manual_sig(secret: &[u8], node: &str, ts: i64, method: &str, pq: &str, hop: &str, client: &str) -> HeaderMap {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret).unwrap();
    m.update(format!("roll-code-sig-v1\n{node}\n{ts}\n{method}\n{pq}\n{hop}\n{client}").as_bytes());
    let mut h = HeaderMap::new();
    h.insert("x-roll-code-node", HeaderValue::from_str(node).unwrap());
    h.insert("x-roll-code-ts", HeaderValue::from(ts));
    h.insert("x-roll-code-sig", HeaderValue::from_str(&hex::encode(m.finalize().into_bytes())).unwrap());
    if !hop.is_empty() {
        h.insert("x-roll-code-hop", HeaderValue::from_str(hop).unwrap());
    }
    if !client.is_empty() {
        h.insert("x-roll-code-client", HeaderValue::from_str(client).unwrap());
    }
    h
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_unsigned_and_tampered_internal_requests() {
    let (_env, a, b) = pair("fwd_auth").await;
    let key = key_for(&["a", "b"], "b", "repo");
    let http = client();
    let pq = format!("/r/{key}/echo");
    let now = code_sql::now_ms();
    let status = |h: HeaderMap, path: String| {
        let http = http.clone();
        let url = b.url(&path);
        async move { http.get(url).headers(h).send().await.unwrap().status() }
    };

    // Claiming to be forwarded without a signature.
    let mut h = HeaderMap::new();
    h.insert("x-roll-code-hop", HeaderValue::from_static("1"));
    assert_eq!(status(h, pq.clone()).await, 401);
    let mut h = HeaderMap::new();
    h.insert("x-roll-code-client", HeaderValue::from_static("6.6.6.6"));
    assert_eq!(status(h, pq.clone()).await, 401, "a client can't claim another address");

    // Properly signed (as a forward would be) is accepted.
    let good = manual_sig(SECRET, "a", now, "GET", &pq, "1", "10.0.0.1");
    let resp = http.get(b.url(&pq)).headers(good.clone()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let m = parse(&resp.text().await.unwrap());
    assert_eq!((m["from"].as_str(), m["hops"].as_str(), m["client"].as_str()), ("a", "1", "10.0.0.1"));

    // Signed for another path, method, hop count or client.
    assert_eq!(status(good.clone(), format!("{pq}?evil=1")).await, 401);
    assert_eq!(http.post(b.url(&pq)).headers(good.clone()).send().await.unwrap().status(), 401);
    let mut h = good.clone();
    h.insert("x-roll-code-hop", HeaderValue::from_static("0"));
    assert_eq!(status(h, pq.clone()).await, 401);
    let mut h = good.clone();
    h.insert("x-roll-code-client", HeaderValue::from_static("6.6.6.6"));
    assert_eq!(status(h, pq.clone()).await, 401);
    // The wrong secret, a stale timestamp, a garbled signature.
    assert_eq!(status(manual_sig(b"some other cluster's secret!!!!!", "a", now, "GET", &pq, "1", ""), pq.clone()).await, 401);
    assert_eq!(status(manual_sig(SECRET, "a", now - 120_000, "GET", &pq, "1", ""), pq.clone()).await, 401);
    let mut h = good.clone();
    h.insert("x-roll-code-sig", HeaderValue::from_static("zz"));
    assert_eq!(status(h, pq.clone()).await, 401);

    // The internal ping only answers signed requests.
    assert_eq!(http.get(b.url("/internal/code/ping")).send().await.unwrap().status(), 401);
    let resp = http.get(b.url("/internal/code/ping")).headers(a.cluster.sign(&Method::GET, "/internal/code/ping")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "b");
    assert_eq!(a.cluster.ping(&b.cluster.node()).await.unwrap(), "b");
    assert_eq!(b.cluster.ping(&a.cluster.node()).await.unwrap(), "a");
}

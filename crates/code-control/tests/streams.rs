//! Limits on open event streams: per caller (a token, an anonymous client address) and in
//! total; a slot frees when its stream is dropped; MCP's event-stream answers count too.

mod common;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use common::{App, app_with};
use serde_json::{Value, json};
use std::net::SocketAddr;

const ROOT: &str = "root-token";

fn events(token: Option<&str>, peer: Option<&str>, forwarded: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri("/v1/code/repos/acme/api/events");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(f) = forwarded {
        b = b.header("x-forwarded-for", f);
    }
    let mut req = b.body(Body::empty()).unwrap();
    if let Some(p) = peer {
        req.extensions_mut().insert(ConnectInfo(p.parse::<SocketAddr>().unwrap()));
    }
    req
}

async fn setup(f: impl FnOnce(&mut code_control::Config)) -> (App, String) {
    let a = app_with(|c| {
        c.admin_token = Some(ROOT.into());
        f(c)
    });
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some(ROOT)).await;
    assert!(s.is_success());
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api", "visibility": "public" })), Some(ROOT)).await;
    assert!(s.is_success());
    let (_, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "ci" })), Some(ROOT)).await;
    (a, t["token"].as_str().unwrap().to_string())
}

async fn body(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn each_token_gets_a_bounded_number_of_streams() {
    let (a, acme) = setup(|c| c.max_streams_per_caller = 2).await;
    let s1 = a.send(events(Some(&acme), None, None)).await;
    let s2 = a.send(events(Some(&acme), None, None)).await;
    assert_eq!(s1.status(), StatusCode::OK);
    assert_eq!(s2.status(), StatusCode::OK);
    assert_eq!(s1.headers()[header::CONTENT_TYPE], "text/event-stream");
    // A third for the same token: 429 with Retry-After and a JSON error.
    let s3 = a.send(events(Some(&acme), None, None)).await;
    assert_eq!(s3.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(s3.headers()[header::RETRY_AFTER], "5");
    assert!(body(s3).await["error"].as_str().unwrap().contains("at most 2"));
    // Another token isn't affected.
    let root = a.send(events(Some(ROOT), None, None)).await;
    assert_eq!(root.status(), StatusCode::OK);
    // Closing a stream frees its slot.
    drop(s1);
    let s4 = a.send(events(Some(&acme), None, None)).await;
    assert_eq!(s4.status(), StatusCode::OK);
    drop((s2, s4, root));
    assert_eq!(a.code.streams.open("anonymous"), 0);
    // A refused request (private repo, 404) takes no slot.
    let (s, _) = a.req("PATCH", "/v1/code/repos/acme/api", Some(json!({ "visibility": "private" })), Some(ROOT)).await;
    assert!(s.is_success());
    for _ in 0..3 {
        assert_eq!(a.send(events(None, Some("10.0.0.9:1"), None)).await.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(a.code.streams.open("ip:10.0.0.9"), 0);
}

#[tokio::test]
async fn anonymous_streams_count_per_client_address_and_in_total() {
    let (a, _) = setup(|c| {
        c.max_streams_per_caller = 2;
        c.max_streams = 5;
    })
    .await;
    // Per address.
    let x1 = a.send(events(None, Some("203.0.113.1:5000"), None)).await;
    let x2 = a.send(events(None, Some("203.0.113.1:5001"), None)).await;
    assert_eq!((x1.status(), x2.status()), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.send(events(None, Some("203.0.113.1:5002"), None)).await.status(), StatusCode::TOO_MANY_REQUESTS);
    let y1 = a.send(events(None, Some("203.0.113.2:5000"), None)).await;
    assert_eq!(y1.status(), StatusCode::OK);
    assert_eq!(a.code.streams.open("ip:203.0.113.1"), 2);
    // X-Forwarded-For is ignored unless trusted: it can't pick a fresh bucket.
    assert_eq!(a.send(events(None, Some("203.0.113.1:5003"), Some("198.51.100.7"))).await.status(), StatusCode::TOO_MANY_REQUESTS);
    // Without a known address, anonymous callers share one bucket bounded by the total.
    let z1 = a.send(events(None, None, None)).await;
    let z2 = a.send(events(None, None, None)).await;
    assert_eq!((z1.status(), z2.status()), (StatusCode::OK, StatusCode::OK));
    // Five open in total: everyone gets the server-wide 429 now.
    let full = a.send(events(None, Some("203.0.113.3:1"), None)).await;
    assert_eq!(full.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(body(full).await["error"].as_str().unwrap().contains("the server has 5"));
    drop((x1, x2, y1, z1, z2));
    assert_eq!(a.send(events(None, Some("203.0.113.3:1"), None)).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_trusted_proxy_keys_anonymous_streams_by_forwarded_address() {
    let (a, _) = setup(|c| {
        c.max_streams_per_caller = 1;
        c.trust_forwarded_for = true;
    })
    .await;
    // All from the proxy's address, but different clients behind it.
    let p1 = a.send(events(None, Some("10.0.0.1:1"), Some("198.51.100.1, 10.0.0.1"))).await;
    let p2 = a.send(events(None, Some("10.0.0.1:2"), Some("198.51.100.2"))).await;
    assert_eq!((p1.status(), p2.status()), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.send(events(None, Some("10.0.0.1:3"), Some("198.51.100.1"))).await.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(a.code.streams.open("ip:198.51.100.1"), 1);
}

#[tokio::test]
async fn open_daemons_count_everyone_by_address() {
    // No admin token: every caller is admin, so streams are counted per address, not shared.
    let a = app_with(|c| c.max_streams_per_caller = 1);
    a.repo("acme", "api").await;
    let s1 = a.send(events(None, Some("127.0.0.1:1"), None)).await;
    let s2 = a.send(events(None, Some("127.0.0.2:1"), None)).await;
    assert_eq!((s1.status(), s2.status()), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.send(events(None, Some("127.0.0.1:2"), None)).await.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn mcp_event_stream_answers_share_the_callers_slots() {
    let (a, acme) = setup(|c| c.max_streams_per_caller = 1).await;
    let mcp = |body: Value, sse: bool, session: Option<String>| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/code/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {acme}"))
            .header("accept", if sse { "text/event-stream" } else { "application/json, text/event-stream" });
        if let Some(s) = session {
            b = b.header("mcp-session-id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };
    let init = a
        .send(mcp(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }), false, None))
        .await;
    let session = init.headers()["mcp-session-id"].to_str().unwrap().to_string();
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
    // A slot free: the answer streams.
    let ok = a.send(mcp(ping.clone(), true, Some(session.clone()))).await;
    assert_eq!(ok.status(), StatusCode::OK);
    let text = String::from_utf8(axum::body::to_bytes(ok.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    assert!(text.contains("event: message"), "{text}");
    // The token's one slot taken by /events: the MCP event stream gets the 429.
    let held = a.send(events(Some(&acme), None, None)).await;
    assert_eq!(held.status(), StatusCode::OK);
    let refused = a.send(mcp(ping.clone(), true, Some(session.clone()))).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    // JSON answers don't hold a stream slot.
    assert_eq!(a.send(mcp(ping, false, Some(session))).await.status(), StatusCode::OK);
}

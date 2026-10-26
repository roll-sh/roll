//! A Lambda function URL in front of the fleet: each invocation (function URL payload format
//! 2.0) is forwarded over HTTP to `upstream` (the stack's internal NLB in front of the proxies)
//! and its answer returned. The function URL gives HTTPS on an AWS domain without a certificate
//! or a load balancer listener of your own; the proxies still do routing, holding and wakes.
//!
//! The control API (`/v1/search…`) goes to `api_upstream` when one is given (the proxies' API
//! listener), so one function URL serves both. An upstream that can't be reached answers 502
//! in Elasticsearch's error shape rather than as a failed invocation (which a function URL
//! turns into an opaque 502).
//!
//! Lambda limits apply: request and response payloads of at most 6 MB each (bulk requests
//! beyond that need the NLB), and the function's timeout bounds a cold wake. Text responses
//! are returned as text (most Elasticsearch answers are JSON); anything else is base64.
//!
//! This is a custom runtime: the binary talks to the Runtime API (`AWS_LAMBDA_RUNTIME_API`)
//! itself, so the same image runs as a Fargate proxy and as the function.
//!
//! **TLS to the proxies.** With a seed (`ROLL_SEARCH_LAMBDA_TLS_SEED`, the secret the stack
//! also gives the proxies as `ROLL_SEARCH_PROXY_TLS_SEED`), `https://` upstreams are reached
//! over TLS that accepts only the certificate the proxies derive from it
//! ([`crate::upstream::seeded`]); the internal NLB passes the connection through.

use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use crate::upstream::{Connector, Pins};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Largest response body returned (Lambda's synchronous payload limit is 6 MB, JSON envelope
/// included).
const MAX_RESPONSE: usize = 6 * 1000 * 1000 - 16 * 1024;

type HttpClient = Client<Connector, Full<Bytes>>;

/// Plain HTTP, and `https://` pinned to `pin` (no pin: `https://` upstreams fail).
fn client(pin: Option<crate::upstream::CertPin>) -> HttpClient {
    let mut c = HttpConnector::new();
    c.set_nodelay(true);
    c.set_connect_timeout(Some(Duration::from_secs(5)));
    let pins = Arc::new(pin.map(Pins::any).unwrap_or_default());
    Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(60)).build(Connector::new(c, pins, Duration::from_secs(5)))
}

/// Runs the Lambda runtime loop until the process is stopped. `tls_seed`: the proxies'
/// certificate seed, for `https://` upstreams.
pub async fn run(runtime_api: &str, upstream: &str, api_upstream: Option<&str>, tls_seed: Option<&[u8]>) -> anyhow::Result<()> {
    let pin = match tls_seed {
        Some(seed) => Some(crate::upstream::parse_pin(&crate::upstream::seeded(seed)?.fingerprint).expect("a fingerprint is a pin")),
        None => None,
    };
    anyhow::ensure!(pin.is_some() || !upstream.starts_with("https://"), "an https:// upstream needs ROLL_SEARCH_LAMBDA_TLS_SEED");
    let http = client(pin);
    let base = format!("http://{runtime_api}/2018-06-01/runtime/invocation");
    loop {
        let next = http.request(Request::get(format!("{base}/next")).body(Full::new(Bytes::new()))?).await?;
        let id = next.headers().get("lambda-runtime-aws-request-id").and_then(|v| v.to_str().ok()).map(str::to_string);
        let event = next.into_body().collect().await?.to_bytes();
        let Some(id) = id else {
            tracing::warn!("invocation without a request id");
            continue;
        };
        let (path, body) = match handle(&http, upstream, api_upstream, &event).await {
            Ok(r) => (format!("{base}/{id}/response"), r.to_string()),
            Err(e) => (format!("{base}/{id}/error"), json!({ "errorMessage": e.to_string(), "errorType": "ProxyError" }).to_string()),
        };
        let r = Request::post(path).header("content-type", "application/json").body(Full::new(Bytes::from(body)))?;
        if let Err(e) = http.request(r).await {
            tracing::warn!(error = %e, "posting the invocation result failed");
        }
    }
}

/// A request as an invocation describes it: method, `path?query`, headers, body.
pub type Invocation = (String, String, Vec<(String, String)>, Bytes);

/// The request an invocation describes.
pub fn request_of(event: &Value) -> anyhow::Result<Invocation> {
    let method = event.pointer("/requestContext/http/method").and_then(Value::as_str).unwrap_or("GET").to_string();
    let mut path = event["rawPath"].as_str().unwrap_or("/").to_string();
    if let Some(q) = event["rawQueryString"].as_str().filter(|q| !q.is_empty()) {
        path.push('?');
        path.push_str(q);
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(h) = event["headers"].as_object() {
        for (k, v) in h {
            if let Some(v) = v.as_str() {
                headers.push((k.clone(), v.to_string()));
            }
        }
    }
    if let Some(c) = event["cookies"].as_array().filter(|c| !c.is_empty()) {
        let joined: Vec<&str> = c.iter().filter_map(Value::as_str).collect();
        headers.push(("cookie".into(), joined.join("; ")));
    }
    if let Some(ip) = event.pointer("/requestContext/http/sourceIp").and_then(Value::as_str) {
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case("x-forwarded-for"));
        headers.push(("x-forwarded-for".into(), ip.to_string()));
    }
    let body = match event["body"].as_str() {
        None => Bytes::new(),
        Some(b) if event["isBase64Encoded"].as_bool().unwrap_or(false) => Bytes::from(base64::engine::general_purpose::STANDARD.decode(b)?),
        Some(b) => Bytes::from(b.to_string()),
    };
    Ok((method, path, headers, body))
}

/// The function URL response for an upstream answer.
pub fn response_of(status: u16, headers: &[(String, String)], body: &[u8]) -> Value {
    if body.len() > MAX_RESPONSE {
        return too_large(&body.len().to_string());
    }
    let mut h = Map::new();
    let mut cookies = Vec::new();
    for (k, v) in headers {
        let k = k.to_ascii_lowercase();
        if k == "set-cookie" {
            cookies.push(Value::String(v.clone()));
            continue;
        }
        if matches!(k.as_str(), "connection" | "keep-alive" | "transfer-encoding" | "content-length") {
            continue;
        }
        match h.get_mut(&k) {
            Some(Value::String(prev)) => {
                prev.push_str(", ");
                prev.push_str(v);
            }
            _ => {
                h.insert(k, Value::String(v.clone()));
            }
        }
    }
    let mut out = json!({ "statusCode": status, "headers": h });
    if !cookies.is_empty() {
        out["cookies"] = Value::Array(cookies);
    }
    match std::str::from_utf8(body) {
        Ok(text) => {
            out["body"] = Value::String(text.to_string());
            out["isBase64Encoded"] = Value::Bool(false);
        }
        Err(_) => {
            out["body"] = Value::String(base64::engine::general_purpose::STANDARD.encode(body));
            out["isBase64Encoded"] = Value::Bool(true);
        }
    }
    out
}

/// Whether `path` (with its query) is the control API's.
fn is_api(path: &str) -> bool {
    path.strip_prefix("/v1/search").is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?']))
}

/// An Elasticsearch-shaped error answer.
fn error_response(status: u16, kind: &str, reason: &str) -> Value {
    let body = json!({ "error": { "root_cause": [{ "type": kind, "reason": reason }], "type": kind, "reason": reason }, "status": status });
    json!({ "statusCode": status, "headers": { "content-type": "application/json" }, "body": body.to_string(), "isBase64Encoded": false })
}

async fn handle(http: &HttpClient, upstream: &str, api_upstream: Option<&str>, event: &[u8]) -> anyhow::Result<Value> {
    let event: Value = serde_json::from_slice(event)?;
    let (method, path, headers, body) = request_of(&event)?;
    let upstream = match api_upstream {
        Some(api) if is_api(&path) => api,
        _ => upstream,
    };
    match forward(http, upstream, method, path, headers, body).await {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(error = %e, "forwarding an invocation failed");
            Ok(error_response(502, "proxy_exception", &format!("the search proxies could not be reached: {e}")))
        }
    }
}

async fn forward(http: &HttpClient, upstream: &str, method: String, path: String, headers: Vec<(String, String)>, body: Bytes) -> anyhow::Result<Value> {
    let mut req = Request::builder().method(method.as_str()).uri(format!("{}{path}", upstream.trim_end_matches('/')));
    for (k, v) in &headers {
        if matches!(k.to_ascii_lowercase().as_str(), "connection" | "content-length" | "transfer-encoding") {
            continue;
        }
        req = req.header(k.as_str(), v.as_str());
    }
    req = req.header("x-forwarded-proto", "https");
    let resp = http.request(req.body(Full::new(body))?).await?;
    let status = resp.status().as_u16();
    let rheaders: Vec<(String, String)> =
        resp.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
    // Bounded: a response past the limit can't be returned anyway.
    let body = match http_body_util::Limited::new(resp.into_body(), MAX_RESPONSE + 1).collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => return Ok(too_large(&format!("more than {MAX_RESPONSE}"))),
        Err(e) => anyhow::bail!("reading the response: {e}"),
    };
    Ok(response_of(status, &rheaders, &body))
}

/// The answer for a response a function URL can't carry (`size` describes its bytes).
fn too_large(size: &str) -> Value {
    let reason = format!("the response ({size} bytes) is larger than a Lambda function URL can return; use the load balancer endpoint");
    error_response(502, "proxy_exception", &reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_url_events() {
        let e = json!({
            "version": "2.0",
            "rawPath": "/logs/_search",
            "rawQueryString": "size=1&q=a",
            "cookies": ["a=1", "b=2"],
            "headers": { "authorization": "Basic YXBwOnB3", "content-type": "application/json", "x-forwarded-for": "1.1.1.1" },
            "requestContext": { "http": { "method": "POST", "sourceIp": "203.0.113.5" } },
            "body": "eyJxdWVyeSI6e319",
            "isBase64Encoded": true,
        });
        let (m, p, h, b) = request_of(&e).unwrap();
        assert_eq!((m.as_str(), p.as_str()), ("POST", "/logs/_search?size=1&q=a"));
        assert_eq!(&b[..], br#"{"query":{}}"#);
        assert!(h.contains(&("cookie".into(), "a=1; b=2".into())));
        assert!(h.contains(&("x-forwarded-for".into(), "203.0.113.5".into())));
        assert_eq!(h.iter().filter(|(k, _)| k == "x-forwarded-for").count(), 1);
        let (m, p, _, b) = request_of(&json!({ "rawPath": "/", "requestContext": { "http": { "method": "GET" } } })).unwrap();
        assert_eq!((m.as_str(), p.as_str(), b.len()), ("GET", "/", 0));
        let (_, _, _, b) = request_of(&json!({ "rawPath": "/", "body": "{\"a\":1}" })).unwrap();
        assert_eq!(&b[..], b"{\"a\":1}");
        assert!(request_of(&json!({ "body": "!!", "isBase64Encoded": true })).is_err());
    }

    #[test]
    fn function_url_responses() {
        let h = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Elastic-Product".into(), "Elasticsearch".into()),
            ("set-cookie".into(), "s=1".into()),
            ("warning".into(), "299 a".into()),
            ("warning".into(), "299 b".into()),
            ("content-length".into(), "2".into()),
        ];
        let r = response_of(200, &h, b"{}");
        assert_eq!(r["statusCode"], 200);
        assert_eq!(r["body"], "{}");
        assert_eq!(r["isBase64Encoded"], false);
        assert_eq!(r["headers"]["content-type"], "application/json");
        assert_eq!(r["headers"]["warning"], "299 a, 299 b");
        assert!(r["headers"].get("content-length").is_none());
        assert_eq!(r["cookies"], json!(["s=1"]));
        let bin = response_of(200, &[], &[0xff, 0x00, 0x80]);
        assert_eq!((bin["body"].as_str().unwrap(), &bin["isBase64Encoded"]), ("/wCA", &json!(true)));
        let big = response_of(200, &[], &vec![b'a'; MAX_RESPONSE + 1]);
        assert_eq!(big["statusCode"], 502);
    }

    /// One invocation end to end: a fake Runtime API and a fake upstream.
    #[tokio::test]
    async fn invocation_round_trip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = up.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.starts_with("GET /_cluster/health?pretty HTTP/1.1"), "{req}");
            assert!(req.to_ascii_lowercase().contains("x-forwarded-proto: https"), "{req}");
            let body = r#"{"status":"green"}"#;
            s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let http = client(None);
        let event = json!({ "rawPath": "/_cluster/health", "rawQueryString": "pretty", "requestContext": { "http": { "method": "GET" } } });
        let r = handle(&http, &format!("http://{up_addr}/"), None, event.to_string().as_bytes()).await.unwrap();
        assert_eq!(r["statusCode"], 200);
        assert_eq!(r["body"], r#"{"status":"green"}"#);
    }

    /// A response past the payload limit is reported as too large, not as an unreachable
    /// upstream, and the proxy stops reading it at the limit.
    #[tokio::test]
    async fn oversized_responses() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = up.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await.unwrap();
            let len = MAX_RESPONSE + 4096;
            s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {len}\r\n\r\n").as_bytes()).await.unwrap();
            let chunk = vec![b'a'; 64 << 10];
            let mut sent = 0;
            while sent < len {
                let n = chunk.len().min(len - sent);
                if s.write_all(&chunk[..n]).await.is_err() {
                    return;
                }
                sent += n;
            }
        });
        let event = json!({ "rawPath": "/big/_search", "requestContext": { "http": { "method": "GET" } } });
        let r = handle(&client(None), &format!("http://{up_addr}"), None, event.to_string().as_bytes()).await.unwrap();
        assert_eq!(r["statusCode"], 502);
        let body: Value = serde_json::from_str(r["body"].as_str().unwrap()).unwrap();
        assert!(body["error"]["reason"].as_str().unwrap().contains("larger than a Lambda function URL can return"), "{body}");
    }

    #[test]
    fn api_paths() {
        assert!(is_api("/v1/search"));
        assert!(is_api("/v1/search/clusters/a"));
        assert!(is_api("/v1/search?x=1"));
        assert!(!is_api("/v1/searchx"));
        assert!(!is_api("/v1/search-logs/_search"));
        assert!(!is_api("/_search"));
    }

    /// API paths go to the API upstream; an unreachable upstream answers 502 in the
    /// Elasticsearch error shape.
    #[tokio::test]
    async fn api_routing_and_unreachable_upstreams() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = api.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = api.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /v1/search/clusters HTTP/1.1"));
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n[]").await.unwrap();
        });
        // Nothing listens on the REST upstream.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let http = client(None);
        let (rest, api) = (format!("http://{dead}"), format!("http://{api_addr}"));
        let ev = json!({ "rawPath": "/v1/search/clusters", "requestContext": { "http": { "method": "GET" } } });
        let r = handle(&http, &rest, Some(&api), ev.to_string().as_bytes()).await.unwrap();
        assert_eq!((r["statusCode"].as_u64(), r["body"].as_str()), (Some(200), Some("[]")));
        let ev = json!({ "rawPath": "/_search", "requestContext": { "http": { "method": "GET" } } });
        let r = handle(&http, &rest, Some(&api), ev.to_string().as_bytes()).await.unwrap();
        assert_eq!(r["statusCode"], 502);
        let body: Value = serde_json::from_str(r["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["error"]["type"], "proxy_exception");
    }

    /// With a seed, `https://` upstreams are proxies serving the seeded certificate (as
    /// `search-proxy --tls-seed` does): reached over TLS, and a proxy with another certificate
    /// isn't sent the request.
    #[tokio::test]
    async fn tls_upstreams_are_pinned_to_the_seeded_certificate() {
        use crate::proxy::{ListenOptions, Port, tls_acceptor};
        let seed = b"the stack's proxy TLS seed, 32 b";
        let cert = crate::upstream::seeded(seed).unwrap();
        // A real proxy over an empty fleet answers 503 at once: enough to see TLS worked.
        let members = crate::fleet::MemoryMembers::new();
        let d = crate::proxy::Directory::new(members, crate::proxy::ProxyConfig { hold_timeout: Duration::from_millis(100), ..Default::default() });
        let p = crate::proxy::Proxy::new(d, None);
        let l = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let opts = ListenOptions { tls: Some(tls_acceptor(cert.cert_pem.as_bytes(), cert.key_pem.as_bytes()).unwrap()), ..ListenOptions::plain(Port::Rest) };
        tokio::spawn(p.serve(l, opts, rx));
        let pin = crate::upstream::parse_pin(&cert.fingerprint).unwrap();
        let ev = json!({ "rawPath": "/", "requestContext": { "http": { "method": "GET" } } });
        let up = format!("https://localhost:{port}");
        let r = handle(&client(Some(pin)), &up, None, ev.to_string().as_bytes()).await.unwrap();
        assert_eq!(r["statusCode"], 503, "{r}");
        assert!(r["body"].as_str().unwrap().contains("master_not_discovered_exception"), "{r}");
        let wrong = crate::upstream::parse_pin(&crate::upstream::seeded(b"some other stack's seed, 32 byte").unwrap().fingerprint).unwrap();
        let r = handle(&client(Some(wrong)), &up, None, ev.to_string().as_bytes()).await.unwrap();
        assert_eq!(r["statusCode"], 502, "{r}");
        // Plain HTTP to a TLS proxy gets nowhere either.
        let r = handle(&client(None), &format!("http://localhost:{port}"), None, ev.to_string().as_bytes()).await.unwrap();
        assert_ne!(r["statusCode"], 200, "{r}");
    }
}

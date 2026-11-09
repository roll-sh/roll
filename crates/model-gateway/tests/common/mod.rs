//! Mock upstreams (axum on loopback) and a gateway in a temp dir, for the proxy's tests.

#![allow(dead_code)]

use agent_proto::{GatewayRun, GatewaySettings, LogEntry, RouteSpec, UpstreamKind, UpstreamSpec};
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use model_gateway::{Gateway, Secret};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A request a mock saw.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: Method,
    /// The path and query, as sent.
    pub uri: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

type Handler = Arc<dyn Fn(&Seen) -> Response + Send + Sync>;

/// A mock upstream: answers every request with `handler`, and keeps what it saw.
pub struct Mock {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    pub async fn start(handler: impl Fn(&Seen) -> Response + Send + Sync + 'static) -> Mock {
        Mock::delayed(Duration::ZERO, handler).await
    }

    /// A mock that answers (headers and all) only after `delay`.
    pub async fn delayed(delay: Duration, handler: impl Fn(&Seen) -> Response + Send + Sync + 'static) -> Mock {
        let handler: Handler = Arc::new(handler);
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = (handler, seen.clone());
        let app = axum::Router::new().fallback(move |method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
            let (handler, seen) = state.clone();
            async move {
                let s = Seen { method, uri: uri.to_string(), headers, body };
                seen.lock().unwrap().push(s.clone());
                tokio::time::sleep(delay).await;
                handler(&s)
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Mock { addr, seen }
    }

    /// `http://127.0.0.1:<port>` + `path`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// The same server by the name `localhost` (another host to the gateway).
    pub fn url_by_name(&self, path: &str) -> String {
        format!("http://localhost:{}{path}", self.addr.port())
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// A JSON answer.
pub fn json(status: u16, v: serde_json::Value) -> Response {
    Response::builder().status(status).header("content-type", "application/json").body(Body::from(v.to_string())).unwrap()
}

/// An answer of exactly these bytes.
pub fn bytes(status: u16, content_type: &str, b: impl Into<Bytes>) -> Response {
    Response::builder().status(status).header("content-type", content_type).body(Body::from(b.into())).unwrap()
}

/// A streamed answer: each chunk after `gap`; an `Err` breaks the connection there.
pub fn stream(content_type: &str, gap: Duration, chunks: Vec<Result<Bytes, &'static str>>) -> Response {
    let s = futures_util::stream::unfold(chunks.into_iter(), move |mut it| async move {
        let next = it.next()?;
        tokio::time::sleep(gap).await;
        Some((next.map_err(std::io::Error::other), it))
    });
    Response::builder().status(200).header("content-type", content_type).body(Body::from_stream(s)).unwrap()
}

pub fn status(code: u16) -> Response {
    json(code, serde_json::json!({ "type": "error", "error": { "type": "api_error", "message": format!("mock says {code}") } }))
}

/// An upstream on a mock (private addresses allowed: it's on loopback) serving every model
/// (`*`: mocks don't list models; tests of listing clear `models`).
pub fn upstream(id: &str, kind: UpstreamKind, base_url: &str) -> UpstreamSpec {
    UpstreamSpec {
        id: id.into(),
        name: id.into(),
        kind,
        base_url: Some(base_url.into()),
        region: None,
        project: None,
        models: vec!["*".into()],
        responses: false,
        headers: vec![],
        allow_private: true,
        key_ref: None,
        disabled: false,
    }
}

/// A port nothing listens on now.
pub fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    l.local_addr().unwrap().port()
}

/// A running gateway with a client key.
pub struct Gw {
    pub gw: Gateway,
    pub dir: tempfile::TempDir,
    pub port: u16,
    pub key: String,
    pub http: reqwest::Client,
}

pub fn settings(port: u16, upstreams: Vec<UpstreamSpec>, routes: Vec<RouteSpec>) -> GatewaySettings {
    GatewaySettings { enabled: true, port, upstreams, routes, ..Default::default() }
}

pub fn keys(pairs: &[(&str, &str)]) -> HashMap<String, Secret> {
    pairs.iter().map(|(k, v)| (k.to_string(), Secret::new(v.to_string()))).collect()
}

impl Gw {
    pub async fn start(upstreams: Vec<UpstreamSpec>, routes: Vec<RouteSpec>, upstream_keys: &[(&str, &str)]) -> Gw {
        Gw::with(|s| s, upstreams, routes, upstream_keys).await
    }

    pub async fn with(edit: impl FnOnce(GatewaySettings) -> GatewaySettings, upstreams: Vec<UpstreamSpec>, routes: Vec<RouteSpec>, upstream_keys: &[(&str, &str)]) -> Gw {
        let dir = tempfile::tempdir().unwrap();
        let gw = Gateway::open(dir.path()).await.unwrap();
        let port = free_port();
        let run = gw.apply(edit(settings(port, upstreams, routes)), keys(upstream_keys)).await;
        assert!(matches!(run, GatewayRun::Running { .. }), "{run:?}");
        let (_, key) = gw.issue_client("test", None).unwrap();
        Gw { gw, dir, port, key: key.expose().to_string(), http: reqwest::Client::new() }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", Ipv4Addr::LOCALHOST, self.port)
    }

    /// POSTs `body` with the client key.
    pub async fn post(&self, path: &str, body: &serde_json::Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&self.key).json(body).send().await.unwrap()
    }

    /// The newest log entry, once the recorder has written `n` of them.
    pub async fn logged(&self, n: usize) -> Vec<LogEntry> {
        for _ in 0..200 {
            let log = self.gw.log(None, None, 200, false).unwrap();
            if log.len() >= n {
                return log;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the gateway never recorded {n} requests");
    }
}

/// A Messages request.
pub fn messages(model: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({ "model": model, "max_tokens": 64, "stream": stream, "messages": [{ "role": "user", "content": "hi" }] })
}

/// A Messages answer.
pub fn messages_answer(model: &str, input: u64, output: u64) -> serde_json::Value {
    serde_json::json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": model,
        "content": [{ "type": "text", "text": "hello" }],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": { "input_tokens": input, "output_tokens": output, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 }
    })
}

/// A Messages stream, as Anthropic sends it.
pub fn messages_stream(model: &str) -> Vec<String> {
    vec![
        format!("event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"{model}\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":12,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":1}}}}}}\n\n"),
        "event: ping\ndata: {\"type\": \"ping\"}\n\n".into(),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".into(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n".into(),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\n".into(),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
    ]
}

pub fn sse(events: &[String]) -> Response {
    bytes(200, "text/event-stream", events.concat())
}

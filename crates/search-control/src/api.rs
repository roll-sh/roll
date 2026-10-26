//! Control-plane HTTP API for roll search (`/v1/search/...`), merged into rolld's API.
//!
//! Errors are `{"error": "..."}` with 400 (invalid), 404 (no such cluster), 409 (name taken,
//! busy), 413 (a console request over the cluster's body limit) or 500, like the other control
//! planes. A cluster's password appears only in the
//! responses that create it (create, branch) or change it (rotate). Creating a cluster answers
//! in the shape `search-yaml run --cluster-hook` expects (`url`, `username`, `password`, `id`).
//!
//! With [`crate::SearchConfig::api_token`] every route wants `Authorization: Bearer <token>`
//! (compared in constant time); without one the API is open, like rolld's local API. In a fleet,
//! calls about a cluster another node holds (console requests, indices, unload, branching it)
//! are forwarded to that node's API, once (`x-roll-search-forwarded`), over TLS pinned by the
//! node's record when it serves its API with TLS.
//!
//! [`TlsListener`] serves the API over TLS (fleet nodes behind search-proxy, whose own API
//! port carries nothing else): handshakes run on their own tasks, so a slow or silent client
//! never holds up accepting the next connection.

use crate::control::{ClusterOptions, ConsoleRequest, ConsoleResponse, Control, ControlError, Usage};
use crate::store::SearchCluster;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

type AppState = Arc<Control>;
type ApiResult<T> = Result<Json<T>, ControlError>;

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let status = match &self {
            ControlError::NotFound(_) => StatusCode::NOT_FOUND,
            ControlError::Invalid(_) => StatusCode::BAD_REQUEST,
            ControlError::Conflict(_) => StatusCode::CONFLICT,
            ControlError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ControlError::Internal(e) => {
                tracing::warn!(error = %e, "search control error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

pub fn router(control: Arc<Control>) -> Router {
    let authed = control.clone();
    Router::new()
        .route("/v1/search", get(info))
        .route("/v1/search/clusters", get(list).post(create))
        .route("/v1/search/clusters/{id}", get(show).patch(update).delete(remove))
        .route("/v1/search/clusters/{id}/branches", get(branches).post(branch))
        .route("/v1/search/clusters/{id}/password", post(rotate))
        .route("/v1/search/clusters/{id}/unload", post(unload))
        .route("/v1/search/clusters/{id}/indices", get(indices))
        .route("/v1/search/clusters/{id}/request", post(request))
        .route("/v1/search/clusters/{id}/usage", get(usage))
        .layer(axum::middleware::from_fn(move |req: Request, next: Next| {
            let c = authed.clone();
            async move { authorize(&c, req, next).await }
        }))
        // The dashboard calls it from the browser, like the other control planes.
        .layer(CorsLayer::permissive())
        .with_state(control)
}

/// Constant-time byte comparison (the length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn authorize(c: &Control, req: Request, next: Next) -> Response {
    let Some(token) = c.cfg.api_token.as_deref().filter(|t| !t.is_empty()) else { return next.run(req).await };
    // Browsers send CORS preflights without credentials.
    if req.method() == Method::OPTIONS {
        return next.run(req).await;
    }
    let presented = req.headers().get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
    match presented {
        Some(p) if ct_eq(p.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, [(axum::http::header::WWW_AUTHENTICATE, "Bearer")], Json(json!({ "error": "missing or wrong API token" }))).into_response(),
    }
}

/// How long a forwarded call may take, answer included (a console request runs a search).
const FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Forwards an API call to the node holding `name`, when that is another node of the fleet.
async fn forward(c: &Control, name: &str, method: Method, path: &str, headers: &HeaderMap, body: bytes::Bytes) -> Option<Response> {
    if headers.contains_key(crate::control::FORWARDED_HEADER) {
        return None;
    }
    let base = c.owner_api(name).await?;
    let http = c.forwarder()?;
    let mut r = axum::http::Request::builder().method(method).uri(format!("{base}{path}")).header(crate::control::FORWARDED_HEADER, "1");
    for (k, v) in headers {
        if matches!(k.as_str(), "authorization" | "content-type" | "accept") {
            r = r.header(k, v);
        }
    }
    let failed = |e: String| ControlError::Internal(anyhow::anyhow!("forwarding to the cluster's node failed: {e}")).into_response();
    let req = match r.body(http_body_util::Full::new(body)) {
        Ok(r) => r,
        Err(e) => return Some(failed(e.to_string())),
    };
    let call = async {
        use http_body_util::BodyExt;
        let resp = http.request(req).await.map_err(|e| format!("{e:?}"))?;
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.map_err(|e| e.to_string())?.to_bytes();
        Ok::<_, String>((parts, bytes))
    };
    Some(match tokio::time::timeout(FORWARD_TIMEOUT, call).await {
        Ok(Ok((parts, bytes))) => {
            let ct = parts.headers.get("content-type").cloned().unwrap_or_else(|| axum::http::HeaderValue::from_static("application/json"));
            (parts.status, [(axum::http::header::CONTENT_TYPE, ct)], bytes).into_response()
        }
        Ok(Err(e)) => failed(e),
        Err(_) => failed("timed out".into()),
    })
}

/// A TLS listener for [`axum::serve`]: accepts TCP connections and completes each handshake
/// on its own task (10 s at most), handing out only connections that finished it.
pub struct TlsListener {
    rx: tokio::sync::mpsc::Receiver<(tokio_rustls::server::TlsStream<tokio::net::TcpStream>, std::net::SocketAddr)>,
    local: std::net::SocketAddr,
    accepting: tokio::task::JoinHandle<()>,
}

/// Handshakes finished but not yet served; past it new handshakes wait.
const TLS_BACKLOG: usize = 256;

impl TlsListener {
    pub fn new(listener: tokio::net::TcpListener, acceptor: tokio_rustls::TlsAcceptor) -> std::io::Result<TlsListener> {
        let local = listener.local_addr()?;
        let (tx, rx) = tokio::sync::mpsc::channel(TLS_BACKLOG);
        let accepting = tokio::spawn(async move {
            loop {
                let (tcp, peer) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        // Out of file descriptors and the like: back off rather than spin.
                        tracing::warn!(error = %e, "accepting a search API connection failed");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if tx.is_closed() {
                    return;
                }
                let _ = tcp.set_nodelay(true);
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(tcp)).await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, peer)).await;
                        }
                        Ok(Err(e)) => tracing::debug!(%peer, error = %e, "search API TLS handshake failed"),
                        Err(_) => tracing::debug!(%peer, "search API TLS handshake timed out"),
                    }
                });
            }
        });
        Ok(TlsListener { rx, local, accepting })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            // The accept loop only ends when this listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

async fn info(State(c): State<AppState>) -> Json<Value> {
    let stats = c.data.stats();
    Json(json!({
        "version": search_es::version::ES_VERSION,
        "opensearch_version": search_es::version::OPENSEARCH_VERSION,
        "lucene_version": search_es::version::LUCENE_VERSION,
        "region": c.cfg.region,
        "http": format!("{}:{}", c.cfg.public_host, c.cfg.http_port),
        "tls": c.cfg.tls,
        "domain": c.cfg.domain,
        "storage": c.objects().description,
        "history_retention_s": c.cfg.history_retention.as_secs(),
        "defaults": c.cfg.defaults,
        "pricing": c.cfg.pricing,
        "loaded": c.node.loaded().len(),
        "cache": { "used_bytes": stats.cache.used, "budget_bytes": stats.cache.budget },
    }))
}

#[derive(Serialize)]
struct BranchRef {
    id: String,
    name: String,
    parent_at: Option<i64>,
    created_at: i64,
}

#[derive(Serialize)]
struct ClusterView {
    #[serde(flatten)]
    cluster: SearchCluster,
    #[serde(flatten)]
    connection: Value,
    status: Value,
    /// Name of the parent (`parent_id` stays when the parent is deleted; this doesn't).
    parent_name: Option<String>,
    branches: Vec<BranchRef>,
    /// Only when created or rotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

async fn view(c: &Control, d: SearchCluster, with_password: bool) -> Result<ClusterView, ControlError> {
    let branches = c
        .store
        .children(&d.id)
        .await?
        .into_iter()
        .map(|b| BranchRef { id: b.id, name: b.name, parent_at: b.parent_at, created_at: b.created_at })
        .collect();
    let parent_name = match &d.parent_id {
        Some(p) => c.store.get(p).await?.map(|p| p.name),
        None => None,
    };
    Ok(ClusterView {
        connection: c.connection(&d),
        status: c.status(&d).await,
        parent_name,
        branches,
        password: with_password.then(|| d.password.clone()),
        cluster: d,
    })
}

async fn views(c: &Control, list: Vec<SearchCluster>) -> Result<Vec<ClusterView>, ControlError> {
    let mut out = Vec::with_capacity(list.len());
    for d in list {
        out.push(view(c, d, false).await?);
    }
    Ok(out)
}

async fn list(State(c): State<AppState>) -> ApiResult<Vec<ClusterView>> {
    let list = c.list().await?;
    Ok(Json(views(&c, list).await?))
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    /// Branch of this cluster (id or name)…
    parent: Option<String>,
    /// …as of this Unix second (default now).
    at: Option<i64>,
    #[serde(flatten)]
    opts: ClusterOptions,
}

async fn create(State(c): State<AppState>, method: Method, headers: HeaderMap, body: bytes::Bytes) -> Result<Response, ControlError> {
    let r: CreateReq = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    let d = match &r.parent {
        Some(p) => {
            // A branch is cut on the node holding the parent, which commits it first.
            let parent = c.get(p).await?;
            if let Some(resp) = forward(&c, &parent.name, method, "/v1/search/clusters", &headers, body.clone()).await {
                return Ok(resp);
            }
            c.branch(p, &r.name, r.at, r.opts).await?
        }
        None if r.at.is_some() => return Err(ControlError::Invalid("at needs a parent".into())),
        None => c.create(&r.name, r.opts).await?,
    };
    Ok(Json(view(&c, d, true).await?).into_response())
}

async fn show(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ClusterView> {
    let d = c.get(&id).await?;
    Ok(Json(view(&c, d, false).await?))
}

async fn update(State(c): State<AppState>, Path(id): Path<String>, Json(o): Json<ClusterOptions>) -> ApiResult<ClusterView> {
    let d = c.update(&id, o).await?;
    Ok(Json(view(&c, d, false).await?))
}

async fn remove(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let d = c.get(&id).await?;
    c.delete(&d.id).await?;
    Ok(Json(json!({ "deleted": d.id, "name": d.name })))
}

#[derive(Deserialize)]
struct BranchReq {
    name: String,
    /// Unix seconds; default now.
    at: Option<i64>,
    #[serde(flatten)]
    opts: ClusterOptions,
}

async fn branch(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap, body: bytes::Bytes) -> Result<Response, ControlError> {
    let r: BranchReq = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    let parent = c.get(&id).await?;
    if let Some(resp) = forward(&c, &parent.name, method, &format!("/v1/search/clusters/{id}/branches"), &headers, body.clone()).await {
        return Ok(resp);
    }
    let d = c.branch(&id, &r.name, r.at, r.opts).await?;
    Ok(Json(view(&c, d, true).await?).into_response())
}

async fn branches(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Vec<ClusterView>> {
    let p = c.get(&id).await?;
    let list = c.store.children(&p.id).await?;
    Ok(Json(views(&c, list).await?))
}

async fn rotate(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ClusterView> {
    let d = c.rotate_password(&id).await?;
    Ok(Json(view(&c, d, true).await?))
}

async fn unload(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap) -> Result<Response, ControlError> {
    let d = c.get(&id).await?;
    if let Some(resp) = forward(&c, &d.name, method, &format!("/v1/search/clusters/{id}/unload"), &headers, bytes::Bytes::new()).await {
        return Ok(resp);
    }
    Ok(Json(json!({ "unloaded": c.unload(&id).await? })).into_response())
}

async fn indices(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap) -> Result<Response, ControlError> {
    let d = c.get(&id).await?;
    if let Some(resp) = forward(&c, &d.name, method, &format!("/v1/search/clusters/{id}/indices"), &headers, bytes::Bytes::new()).await {
        return Ok(resp);
    }
    Ok(Json(c.indices(&id).await?).into_response())
}

/// The console request is read up to the cluster's own body limit (`max_body_mb`, like a
/// client talking to it directly) plus room for its JSON envelope, instead of axum's 2 MB
/// default: a body sent as a JSON string grows by its escapes. The cluster then enforces its
/// limit exactly, with Elasticsearch's 413.
async fn request(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
    let cluster = c.get(&id).await?;
    let limit = console_limit(cluster.max_body_mb);
    let bytes = read_limited(body, limit).await.map_err(|e| match e {
        Some(e) => ControlError::Invalid(format!("reading the request failed: {e}")),
        None => ControlError::TooLarge(format!("the request is larger than cluster {}'s body limit allows ({} MiB, max_body_mb)", cluster.name, cluster.max_body_mb)),
    })?;
    let r: ConsoleRequest = serde_json::from_slice(&bytes).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    if let Some(resp) = forward(&c, &cluster.name, method, &format!("/v1/search/clusters/{id}/request"), &headers, bytes.clone()).await {
        return Ok(resp);
    }
    let out: ConsoleResponse = c.request(&cluster.id, r).await?;
    Ok(Json(out).into_response())
}

/// Bytes past the limit read (and dropped) before answering 413, and for how long.
const DRAIN_BYTES: usize = 64 << 20;
const DRAIN_FOR: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads `body` up to `limit` bytes; `Err(None)` when it is longer. An oversized body is still
/// read to its end (dropping the rest, within [`DRAIN_BYTES`] and [`DRAIN_FOR`]) before the
/// 413 goes out: answering while the client is still uploading closes the connection under it,
/// and the client then sees a reset instead of the reason.
async fn read_limited(body: Body, limit: usize) -> Result<bytes::Bytes, Option<String>> {
    use http_body_util::BodyExt;
    let mut body = body;
    let mut buf = bytes::BytesMut::new();
    let mut over = 0usize;
    let deadline = tokio::time::Instant::now() + DRAIN_FOR;
    loop {
        let frame = if over == 0 {
            body.frame().await
        } else {
            match tokio::time::timeout_at(deadline, body.frame()).await {
                Ok(f) => f,
                Err(_) => return Err(None),
            }
        };
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|e| Some(e.to_string()))?;
        let Ok(data) = frame.into_data() else { continue };
        if over == 0 && buf.len() + data.len() <= limit {
            buf.extend_from_slice(&data);
            continue;
        }
        if over == 0 {
            buf = bytes::BytesMut::new();
        }
        over += data.len();
        if over > DRAIN_BYTES {
            return Err(None);
        }
    }
    if over > 0 { Err(None) } else { Ok(buf.freeze()) }
}

/// Bytes of a console request for a cluster whose bodies may be `max_body_mb` MiB: twice that
/// (escaped quotes and newlines double in a JSON string) plus 1 MiB for the rest.
pub fn console_limit(max_body_mb: i64) -> usize {
    ((max_body_mb.max(1) as usize) << 21).saturating_add(1 << 20)
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
}

async fn usage(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<UsageQuery>) -> ApiResult<Usage> {
    Ok(Json(c.usage(&id, q.days.unwrap_or(30).clamp(1, 366)).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A streamed body of `n` chunks of `size` bytes; `sent` counts the chunks taken from it.
    fn chunks(n: usize, size: usize, sent: Arc<AtomicUsize>) -> Body {
        let s = futures_util::stream::iter((0..n).map(move |_| {
            sent.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; size]))
        }));
        Body::from_stream(s)
    }

    #[tokio::test]
    async fn bodies_within_the_limit_are_read_whole() {
        let sent = Arc::new(AtomicUsize::new(0));
        let b = read_limited(chunks(4, 1000, sent.clone()), 4000).await.unwrap();
        assert_eq!(b.len(), 4000);
        assert_eq!(read_limited(Body::empty(), 0).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn an_oversized_body_is_read_to_its_end_before_the_refusal() {
        let sent = Arc::new(AtomicUsize::new(0));
        assert_eq!(read_limited(chunks(10, 1000, sent.clone()), 4500).await.unwrap_err(), None);
        assert_eq!(sent.load(Ordering::SeqCst), 10, "the rest was drained, so the client sees the 413");
        // Past the drain allowance it stops reading (the connection then closes).
        let sent = Arc::new(AtomicUsize::new(0));
        let n = (DRAIN_BYTES >> 20) + 50;
        assert_eq!(read_limited(chunks(n, 1 << 20, sent.clone()), 1 << 20).await.unwrap_err(), None);
        assert!(sent.load(Ordering::SeqCst) < n);
    }

    /// The API over [`TlsListener`]: a client that connects and never says anything doesn't
    /// stop the next one, which is served over TLS pinned to the listener's certificate.
    #[tokio::test]
    async fn tls_listener_serves_while_a_handshake_stalls() {
        use http_body_util::BodyExt;
        let cert = search_proxy::upstream::self_signed(&[]).unwrap();
        let acceptor = search_server::net::tls_acceptor(cert.cert_pem.as_bytes(), cert.key_pem.as_bytes()).unwrap();
        let l = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        let tls = TlsListener::new(l, acceptor).unwrap();
        assert_eq!(axum::serve::Listener::local_addr(&tls).unwrap(), addr);
        let app = Router::new().route("/v1/search", get(|| async { "ok" }));
        tokio::spawn(async move { axum::serve(tls, app).await });
        let _silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        let pins = Arc::new(search_proxy::upstream::Pins::default());
        pins.replace(std::collections::HashMap::from([(addr, search_proxy::upstream::parse_pin(&cert.fingerprint).unwrap())]));
        let conn = search_proxy::upstream::Connector::new(hyper_util::client::legacy::connect::HttpConnector::new(), pins, std::time::Duration::from_secs(5));
        let client: crate::control::Forwarder = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(conn);
        let req = axum::http::Request::get(format!("https://{addr}/v1/search")).body(http_body_util::Full::new(bytes::Bytes::new())).unwrap();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), client.request(req)).await.unwrap().unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.into_body().collect().await.unwrap().to_bytes(), "ok");
        // Plain HTTP gets no answer from it.
        let plain = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http::<http_body_util::Full<bytes::Bytes>>();
        let req = axum::http::Request::get(format!("http://{addr}/v1/search")).body(http_body_util::Full::new(bytes::Bytes::new())).unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(5), plain.request(req)).await.unwrap().is_err());
    }
}

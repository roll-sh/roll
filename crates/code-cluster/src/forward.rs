//! Forwarding requests to a key's owner, and accepting forwarded requests.

use crate::cluster::{Cluster, Guard, Node, Route};
use crate::sign::{self, CLIENT_HEADER, HEADERS, HOP_HEADER, UNAVAILABLE_HEADER, VerifyError};
use crate::{Error, Result};
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http_body_util::BodyExt;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Forwards a request may take at most; the next one answers 508 Loop Detected.
pub const MAX_HOPS: u8 = 3;

/// Set on requests that arrived signed from another node (by [`accept_forwarded`] or
/// [`require_signed`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forwarded {
    /// The node that sent it (verified).
    pub node: String,
    /// Forwards so far (1 after the first).
    pub hops: u8,
    /// The original client's address, as the first node saw it.
    pub client: Option<String>,
}

/// The client address to pass on when forwarding, if the application knows better than the TCP
/// peer (e.g. from a trusted load balancer's `X-Forwarded-For`). Insert it as a request
/// extension before [`Cluster::dispatch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientAddr(pub String);

/// The original client's address: from a verified forward, else [`ClientAddr`], else the TCP
/// peer (`ConnectInfo<SocketAddr>`).
pub fn client_addr(req: &Request) -> Option<String> {
    client_from(req.extensions())
}

fn client_from(ext: &axum::http::Extensions) -> Option<String> {
    if let Some(f) = ext.get::<Forwarded>() {
        return f.client.clone();
    }
    if let Some(c) = ext.get::<ClientAddr>() {
        return Some(c.0.clone());
    }
    ext.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string())
}

/// Forwards the request has taken so far (0 if it came from a client).
pub fn hops(req: &Request) -> u8 {
    req.extensions().get::<Forwarded>().map_or(0, |f| f.hops)
}

/// The answer for a key this node can't serve right now: 503 with `Retry-After` (whole
/// seconds) and `x-roll-code-unavailable` (milliseconds), which tells a forwarding node to
/// route again rather than pass the 503 on.
pub fn unavailable(retry_after: Duration) -> Response {
    let ms = retry_after.as_millis().max(1) as u64;
    let secs = ms.div_ceil(1000);
    let mut r = (StatusCode::SERVICE_UNAVAILABLE, "this repository is moving between nodes; retry shortly\n").into_response();
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
    r.headers_mut().insert(UNAVAILABLE_HEADER, HeaderValue::from(ms));
    r
}

fn plain(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, msg.into()).into_response()
}

fn path_and_query(uri: &axum::http::Uri, ext: &axum::http::Extensions) -> String {
    let uri = ext.get::<OriginalUri>().map_or(uri, |o| &o.0);
    uri.path_and_query().map_or("/", |p| p.as_str()).to_string()
}

const HOP_BY_HOP: [&str; 10] =
    ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade", "expect"];

/// Headers to pass on: everything but hop-by-hop headers (and those `Connection` names), and,
/// on requests, our own headers (re-added, signed).
fn copy_headers(src: &HeaderMap, dst: &mut HeaderMap, request: bool, drop_length: bool) {
    let listed: Vec<HeaderName> = src
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for (name, value) in src {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || listed.contains(name) || (request && HEADERS.contains(&n)) || (drop_length && name == header::CONTENT_LENGTH) {
            continue;
        }
        dst.append(name.clone(), value.clone());
    }
}

enum Body2 {
    /// Not read yet.
    Fresh(Body),
    /// Read into memory: can be sent again.
    Buffered(Bytes),
    /// Streamed to a node: gone.
    Spent,
}

enum Failed {
    /// Never reached the node: safe to send again.
    Connect(String),
    /// The node answered Unavailable (and did nothing).
    Unavailable(Duration, Box<Response>),
}

impl Cluster {
    /// Send `req` to `to` and stream its answer back (see the crate docs, "Forwarding").
    /// `hops`: forwards the request has taken so far ([`hops`]). Retries once on a connection
    /// error or an Unavailable answer if the body was small enough to buffer.
    pub async fn forward(&self, to: &Node, req: Request, hops: u8) -> Response {
        if hops >= MAX_HOPS {
            return loop_detected();
        }
        let client = client_addr(&req);
        let (parts, body) = req.into_parts();
        let pq = path_and_query(&parts.uri, &parts.extensions);
        let mut body = Body2::Fresh(body);
        for attempt in 0..2 {
            let (rb, replayable) = match self.body_for_send(&mut body).await {
                Ok(b) => b,
                Err(r) => return *r,
            };
            let wait = match self.send(to, &parts, &pq, rb, replayable, hops, client.as_deref()).await {
                Ok(r) => return r,
                Err(Failed::Connect(e)) if attempt == 0 && replayable => {
                    tracing::debug!(to = %to.id, error = %e, "forward failed to connect; retrying");
                    Duration::from_millis(50)
                }
                Err(Failed::Connect(e)) => return node_unreachable(to, &e),
                Err(Failed::Unavailable(d, _)) if attempt == 0 && replayable => d,
                Err(Failed::Unavailable(_, r)) => return *r,
            };
            tokio::time::sleep(wait.min(self.inner.cfg.max_retry_wait)).await;
        }
        unavailable(self.inner.cfg.sweep_every)
    }

    /// Run a request for `key` where it belongs: here (calling `local` with a [`Guard`]) if
    /// this node owns the key, else forwarded to the owner (or to the node that should take
    /// it), else 503 [`unavailable`]. On a connection error or an Unavailable answer (local or
    /// remote) it routes again once, after a short wait, if the body is replayable. `local` is
    /// called at most once.
    pub async fn dispatch<F, Fut>(&self, key: &str, req: Request, local: F) -> Response
    where
        F: FnOnce(Request, Guard) -> Fut,
        Fut: Future<Output = Response>,
    {
        let hops = hops(&req);
        let client = client_addr(&req);
        let (parts, body) = req.into_parts();
        let pq = path_and_query(&parts.uri, &parts.extensions);
        let mut body = Body2::Fresh(body);
        let mut parts = Some(parts);
        let mut local = Some(local);
        for attempt in 0..2 {
            let last = attempt == 1;
            let route = match self.route_async(key).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(key, error = %e, "routing failed");
                    return unavailable(Duration::from_secs(1));
                }
            };
            let wait = match route {
                Route::Local(guard) => {
                    let b = match std::mem::replace(&mut body, Body2::Spent) {
                        Body2::Fresh(b) => b,
                        Body2::Buffered(b) => Body::from(b),
                        Body2::Spent => return unavailable(self.inner.cfg.sweep_every),
                    };
                    let (Some(p), Some(f)) = (parts.take(), local.take()) else { return unavailable(self.inner.cfg.sweep_every) };
                    return f(Request::from_parts(p, b), guard).await;
                }
                Route::Unavailable { retry_after } => {
                    if last || matches!(body, Body2::Spent) {
                        return unavailable(retry_after);
                    }
                    retry_after
                }
                Route::Remote(node) => {
                    if hops >= MAX_HOPS {
                        return loop_detected();
                    }
                    let (rb, replayable) = match self.body_for_send(&mut body).await {
                        Ok(b) => b,
                        Err(r) => return *r,
                    };
                    let p = parts.as_ref().expect("parts are taken only on return");
                    match self.send(&node, p, &pq, rb, replayable, hops, client.as_deref()).await {
                        Ok(r) => return r,
                        Err(Failed::Connect(e)) => {
                            self.forget(key);
                            if last || !replayable {
                                return node_unreachable(&node, &e);
                            }
                            Duration::from_millis(50)
                        }
                        Err(Failed::Unavailable(d, r)) => {
                            self.forget(key);
                            if last || !replayable {
                                return *r;
                            }
                            d
                        }
                    }
                }
            };
            tokio::time::sleep(wait.min(self.inner.cfg.max_retry_wait)).await;
        }
        unavailable(self.inner.cfg.sweep_every)
    }

    /// [`Cluster::route`] without blocking the async runtime: answered from memory when it can,
    /// else on the blocking pool.
    pub async fn route_async(&self, key: &str) -> Result<Route> {
        if let Some(r) = self.inner.route_fast(key) {
            return Ok(r);
        }
        let inner = self.inner.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || inner.route(&key)).await.map_err(|e| Error::Internal(format!("routing task: {e}")))?
    }

    /// The body to send this attempt, and whether it can be sent again.
    async fn body_for_send(&self, body: &mut Body2) -> std::result::Result<(reqwest::Body, bool), Box<Response>> {
        match std::mem::replace(body, Body2::Spent) {
            Body2::Buffered(b) => {
                *body = Body2::Buffered(b.clone());
                Ok((reqwest::Body::from(b), true))
            }
            Body2::Fresh(b) => match b.size_hint().exact() {
                Some(n) if n <= self.inner.cfg.forward_buffer as u64 => match b.collect().await {
                    Ok(c) => {
                        let b = c.to_bytes();
                        *body = Body2::Buffered(b.clone());
                        Ok((reqwest::Body::from(b), true))
                    }
                    Err(e) => Err(Box::new(plain(StatusCode::BAD_REQUEST, format!("reading the request body failed: {e}\n")))),
                },
                // Big or of unknown size: stream it, once.
                _ => Ok((reqwest::Body::wrap_stream(b.into_data_stream()), false)),
            },
            Body2::Spent => Err(Box::new(unavailable(self.inner.cfg.sweep_every))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        to: &Node,
        parts: &Parts,
        pq: &str,
        body: reqwest::Body,
        buffered: bool,
        hops: u8,
        client: Option<&str>,
    ) -> std::result::Result<Response, Failed> {
        let url = match reqwest::Url::parse(&format!("{}{}", to.url.trim_end_matches('/'), pq)) {
            Ok(u) => u,
            Err(e) => return Ok(plain(StatusCode::BAD_GATEWAY, format!("bad node url {}: {e}\n", to.url))),
        };
        // Sign what will actually be on the wire (the URL parser may normalize the path).
        let sent_pq = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        };
        let mut headers = HeaderMap::new();
        copy_headers(&parts.headers, &mut headers, true, buffered);
        if !headers.contains_key(header::HOST)
            && let Some(a) = parts.uri.authority().and_then(|a| HeaderValue::from_str(a.as_str()).ok())
        {
            headers.insert(header::HOST, a);
        }
        let me = &self.inner.cfg;
        headers.extend(sign::sign(&me.secret, &me.node_id, &parts.method, &sent_pq, Some(hops + 1), client));
        let req = self.inner.http.request(parts.method.clone(), url).headers(headers).body(body);
        match req.send().await {
            Ok(resp) => {
                let unavailable_ms = (resp.status() == StatusCode::SERVICE_UNAVAILABLE)
                    .then(|| resp.headers().get(UNAVAILABLE_HEADER))
                    .flatten()
                    .map(|v| v.to_str().ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(1000));
                let r = into_response(resp);
                match unavailable_ms {
                    Some(ms) => Err(Failed::Unavailable(Duration::from_millis(ms), Box::new(r))),
                    None => Ok(r),
                }
            }
            Err(e) if e.is_connect() => Err(Failed::Connect(e.to_string())),
            Err(e) => {
                tracing::warn!(to = %to.id, error = %e, "forwarded request failed");
                Ok(plain(StatusCode::BAD_GATEWAY, format!("forwarding to node {} failed: {e}\n", to.id)))
            }
        }
    }

    /// The internal router: `GET /internal/code/ping` (signed) answers this node's id. Merge it
    /// into the API router.
    pub fn router(self: &Arc<Self>) -> axum::Router {
        axum::Router::new()
            .route("/internal/code/ping", axum::routing::get(ping))
            .route_layer(axum::middleware::from_fn_with_state(self.clone(), require_signed))
            .with_state(self.clone())
    }

    /// Ask `node` for its id over the signed ping route.
    pub async fn ping(&self, node: &Node) -> Result<String> {
        let pq = "/internal/code/ping";
        let resp = self
            .inner
            .http
            .get(format!("{}{pq}", node.url.trim_end_matches('/')))
            .headers(self.sign(&Method::GET, pq))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| Error::Internal(format!("ping {}: {e}", node.id)))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Error::Internal(format!("ping {}: {e}", node.id)))?;
        if !status.is_success() {
            return Err(Error::Internal(format!("ping {}: {status}: {text}", node.id)));
        }
        Ok(text)
    }
}

async fn ping(State(c): State<Arc<Cluster>>) -> String {
    c.node_id().to_string()
}

fn into_response(resp: reqwest::Response) -> Response {
    let mut out = Response::new(Body::empty());
    *out.status_mut() = resp.status();
    copy_headers(resp.headers(), out.headers_mut(), false, false);
    *out.body_mut() = Body::from_stream(resp.bytes_stream());
    out
}

fn loop_detected() -> Response {
    plain(StatusCode::LOOP_DETECTED, format!("the request was forwarded {MAX_HOPS} times without reaching its owner\n"))
}

fn node_unreachable(to: &Node, e: &str) -> Response {
    tracing::warn!(to = %to.id, error = %e, "node unreachable");
    unavailable(Duration::from_secs(1))
}

fn unauthorized(e: VerifyError) -> Response {
    plain(StatusCode::UNAUTHORIZED, format!("bad internal request signature: {e}\n"))
}

fn verify_request(cluster: &Cluster, req: &Request) -> std::result::Result<Forwarded, VerifyError> {
    let pq = path_and_query(req.uri(), req.extensions());
    let node = cluster.verify(req.headers(), req.method(), &pq)?;
    let get = |h: &str| req.headers().get(h).and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty());
    let hops = get(HOP_HEADER).map_or(Ok(0), |h| h.parse::<u8>()).map_err(|_| VerifyError::Malformed(HOP_HEADER))?;
    Ok(Forwarded { node, hops, client: get(CLIENT_HEADER).map(str::to_string) })
}

/// Middleware for the API router (`axum::middleware::from_fn_with_state(cluster,
/// accept_forwarded)`): a request carrying any `x-roll-code-*` header must be signed by a node
/// of this cluster, else 401; a verified one gets a [`Forwarded`] extension (node, hop count,
/// client address). Requests without those headers pass through untouched.
pub async fn accept_forwarded(State(cluster): State<Arc<Cluster>>, mut req: Request, next: Next) -> Response {
    if !HEADERS.iter().any(|h| req.headers().contains_key(*h)) {
        return next.run(req).await;
    }
    match verify_request(&cluster, &req) {
        Ok(f) => {
            req.extensions_mut().insert(f);
            next.run(req).await
        }
        Err(e) => unauthorized(e),
    }
}

/// Middleware for internal routes: only signed requests get through (401 otherwise).
pub async fn require_signed(State(cluster): State<Arc<Cluster>>, mut req: Request, next: Next) -> Response {
    if req.extensions().get::<Forwarded>().is_none() {
        match verify_request(&cluster, &req) {
            Ok(f) => {
                req.extensions_mut().insert(f);
            }
            Err(e) => return unauthorized(e),
        }
    }
    next.run(req).await
}

//! The HTTPS listener (node port 8443, the NLB's 443): HTTP/1.1 (what Cloudflare speaks to
//! origins), routed by `Host` to an in-process router or a reverse proxy (the dashboard, the
//! WebSocket listeners). Bodies stream both ways, and upgrades (WebSockets) pass through.
//!
//! Every request reaches its handler with the client's address as `ConnectInfo<SocketAddr>` and
//! in `X-Forwarded-For` (whatever the client sent there is dropped): the PROXY header's source,
//! or, for a hostname Cloudflare proxies and a peer that is Cloudflare, `CF-Connecting-IP`; and
//! with a [`ViaEdge`] extension. Every response, the listener's own included, passes through
//! [`HttpsConfig::observe`] (access logs, metrics).

use crate::acme::Http01;
use crate::drain::Drain;
use crate::hosts::{Host, Hostnames};
use crate::listener::{self, Accepted, ListenOptions};
use crate::trusted::TrustedProxies;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, header};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_rustls::rustls::ServerConfig;
use tower::ServiceExt as _;
use tracing::debug;

/// Where a request goes.
#[derive(Clone)]
pub enum Target {
    Router(axum::Router),
    /// The router, with `prefix` put in front of the path (`git.roll.sh/o/r.git/info/refs`
    /// is the API's `/git/o/r.git/info/refs`).
    Prefixed(&'static str, axum::Router),
    Proxy(Upstream),
    NotFound,
}

/// Picks the target for a request: by its host, and where one host serves several things (git
/// paths on the dashboard's host, the two WebSocket protocols on a queue's), its path or headers.
pub trait Routes: Send + Sync + 'static {
    fn target(&self, host: &Host, uri: &Uri, headers: &HeaderMap) -> Target;
}

impl<F: Fn(&Host, &Uri, &HeaderMap) -> Target + Send + Sync + 'static> Routes for F {
    fn target(&self, host: &Host, uri: &Uri, headers: &HeaderMap) -> Target {
        self(host, uri, headers)
    }
}

/// Marks a request that came in through the HTTPS listener (a request extension), so a router's
/// own access logging can leave the request to [`Observe`].
#[derive(Clone, Copy, Debug)]
pub struct ViaEdge;

/// Sees every response the listener sends (it may wrap the body, e.g. to log when it ends).
pub trait Observe: Send + Sync + 'static {
    fn observe(&self, host: &Host, method: &Method, started: Instant, resp: Response<Body>) -> Response<Body>;
}

pub struct HttpsConfig {
    pub hosts: Hostnames,
    /// Whose `CF-Connecting-IP` to believe.
    pub cloudflare: TrustedProxies,
    pub listen: ListenOptions,
    /// Authenticated origin pulls: the TLS config (requiring Cloudflare's client certificate)
    /// for the hostnames Cloudflare proxies. Requests for those hostnames on a connection
    /// without that certificate are refused, whatever its server name was.
    pub origin_pull: Option<Arc<ServerConfig>>,
    pub observe: Option<Arc<dyn Observe>>,
}

/// The ALPN the HTTPS listener offers.
pub const ALPN: &[&[u8]] = &[b"http/1.1"];

/// Serve HTTPS on `listener` until the drain stops accepting; then keep-alive connections are
/// closed after their request in flight, so clients reconnect through another node. Requests in
/// flight (a clone, a forwarded push) aren't cut by the edge: they finish, or end with the process.
pub async fn serve(listener: TcpListener, tls: Arc<ServerConfig>, cfg: Arc<HttpsConfig>, routes: Arc<dyn Routes>, drain: Drain) -> std::io::Result<()> {
    let opts = cfg.listen;
    let pick = {
        let cfg = cfg.clone();
        Arc::new(move |sni: Option<&str>| match (&cfg.origin_pull, sni) {
            (Some(pull), Some(name)) if cfg.hosts.classify(name).proxied() => pull.clone(),
            _ => tls.clone(),
        })
    };
    listener::serve_tcp(listener, opts, drain.clone(), move |sock, peer| {
        let (cfg, routes, drain, pick) = (cfg.clone(), routes.clone(), drain.clone(), pick.clone());
        async move {
            let (stream, accepted) = match tokio::time::timeout(opts.handshake_timeout, listener::handshake_by_name(sock, peer, pick.as_ref())).await {
                Ok(Ok(x)) => x,
                Ok(Err(e)) => return debug!(%peer, error = %e, "edge: TLS handshake failed"),
                Err(_) => return debug!(%peer, "edge: TLS handshake timed out"),
            };
            let svc = hyper::service::service_fn(move |req| {
                let (cfg, routes, accepted) = (cfg.clone(), routes.clone(), accepted.clone());
                async move { Ok::<_, Infallible>(handle(req, &accepted, &cfg, routes.as_ref()).await) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(30))
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades();
            tokio::pin!(conn);
            let result = tokio::select! {
                r = conn.as_mut() => r,
                _ = drain.refusing() => {
                    conn.as_mut().graceful_shutdown();
                    conn.await
                }
            };
            if let Err(e) = result {
                debug!(%peer, error = %e, "edge: HTTP connection ended with an error");
            }
        }
    })
    .await
}

/// Where ACME HTTP-01 tokens are asked for.
pub const ACME_CHALLENGE_PATH: &str = "/.well-known/acme-challenge/";

/// The plain-HTTP listener (port 80 on a node alone): `GET /.well-known/acme-challenge/<token>`
/// is answered from `challenges` for any host, before anything else routes (an unknown token, or
/// anything else under that path, is a 404); every other request is redirected to HTTPS. Nothing
/// is served in the clear.
pub async fn serve_plain(listener: TcpListener, opts: ListenOptions, challenges: Arc<Http01>, drain: Drain) -> std::io::Result<()> {
    listener::serve_tcp(listener, opts, drain.clone(), move |sock, peer| {
        let (challenges, drain) = (challenges.clone(), drain.clone());
        async move {
            let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                let challenges = challenges.clone();
                async move { Ok::<_, Infallible>(plain(&req, &challenges)) }
            });
            let conn = hyper::server::conn::http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10)).serve_connection(TokioIo::new(sock), svc);
            tokio::pin!(conn);
            let result = tokio::select! {
                r = conn.as_mut() => r,
                _ = drain.refusing() => {
                    conn.as_mut().graceful_shutdown();
                    conn.await
                }
            };
            if let Err(e) = result {
                debug!(%peer, error = %e, "edge: plain HTTP connection ended with an error");
            }
        }
    })
    .await
}

/// One request on the plain-HTTP listener.
pub fn plain<B>(req: &Request<B>, challenges: &Http01) -> Response<Body> {
    if let Some(token) = req.uri().path().strip_prefix(ACME_CHALLENGE_PATH) {
        let well_formed = (1..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let answer = well_formed.then(|| challenges.answer(token)).flatten();
        return match (req.method(), answer) {
            (&Method::GET | &Method::HEAD, Some(key_authorization)) => {
                let mut r = Response::new(Body::from(key_authorization));
                r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
                r
            }
            _ => text(StatusCode::NOT_FOUND, "no such challenge\n"),
        };
    }
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(crate::hosts::normalize);
    let host = host.filter(|h| !h.is_empty() && h.len() <= 253 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'));
    let Some(host) = host else { return text(StatusCode::BAD_REQUEST, "no Host header\n") };
    let pq = req.uri().path_and_query().map_or("/", |p| p.as_str());
    match HeaderValue::from_str(&format!("https://{host}{pq}")) {
        Ok(location) => {
            let mut r = text(StatusCode::PERMANENT_REDIRECT, "");
            r.headers_mut().insert(header::LOCATION, location);
            r
        }
        Err(_) => text(StatusCode::BAD_REQUEST, "bad path\n"),
    }
}

/// The client's address for a request (see the module docs).
pub fn client_addr(accepted: &Accepted, host: &Host, headers: &HeaderMap, cloudflare: &TrustedProxies) -> SocketAddr {
    if host.proxied()
        && cloudflare.contains(accepted.peer.ip())
        && let Some(ip) = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return SocketAddr::new(ip, 0);
    }
    accepted.peer
}

fn text(status: StatusCode, msg: &'static str) -> Response<Body> {
    let mut r = Response::new(Body::from(msg));
    *r.status_mut() = status;
    r
}

const FORWARDING_HEADERS: &[&str] = &["x-forwarded-for", "x-forwarded-proto", "x-forwarded-host", "x-real-ip", "forwarded"];

async fn handle(req: Request<Incoming>, accepted: &Accepted, cfg: &HttpsConfig, routes: &dyn Routes) -> Response<Body> {
    let started = Instant::now();
    let method = req.method().clone();
    let host_name = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string).or_else(|| req.uri().authority().map(|a| a.to_string()));
    let host = host_name.as_deref().map_or(Host::Unknown, |h| cfg.hosts.classify(h));
    let resp = match host_name {
        Some(name) => route(req, accepted, cfg, routes, &host, &name).await,
        None => text(StatusCode::BAD_REQUEST, "no Host header\n"),
    };
    match &cfg.observe {
        Some(o) => o.observe(&host, &method, started, resp),
        None => resp,
    }
}

async fn route(mut req: Request<Incoming>, accepted: &Accepted, cfg: &HttpsConfig, routes: &dyn Routes, host: &Host, host_name: &str) -> Response<Body> {
    if cfg.origin_pull.is_some() && host.proxied() && !accepted.client_verified {
        return text(StatusCode::FORBIDDEN, "this hostname is served only through Cloudflare\n");
    }
    let client = client_addr(accepted, host, req.headers(), &cfg.cloudflare);
    let headers = req.headers_mut();
    for h in FORWARDING_HEADERS {
        headers.remove(*h);
    }
    if client == accepted.peer {
        // Not believed: nothing downstream should believe it either.
        headers.remove("cf-connecting-ip");
    }
    if let Ok(v) = HeaderValue::from_str(&client.ip().to_string()) {
        headers.insert(HeaderName::from_static("x-forwarded-for"), v);
    }
    headers.insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_static("https"));
    if let Ok(v) = HeaderValue::from_str(host_name) {
        headers.insert(HeaderName::from_static("x-forwarded-host"), v);
    }
    req.extensions_mut().insert(ConnectInfo(client));
    req.extensions_mut().insert(ViaEdge);
    let req = req.map(Body::new);
    match routes.target(host, req.uri(), req.headers()) {
        Target::Router(r) => r.oneshot(req).await.unwrap_or_else(|e| match e {}),
        Target::Prefixed(prefix, r) => {
            let (mut parts, body) = req.into_parts();
            let pq = parts.uri.path_and_query().map_or("/", |p| p.as_str());
            match format!("{prefix}{pq}").parse::<Uri>() {
                Ok(uri) => parts.uri = uri,
                Err(_) => return text(StatusCode::BAD_REQUEST, "bad path\n"),
            }
            r.oneshot(Request::from_parts(parts, body)).await.unwrap_or_else(|e| match e {})
        }
        Target::Proxy(u) => u.forward(req).await,
        Target::NotFound => text(StatusCode::NOT_FOUND, "no such host\n"),
    }
}

/// A reverse proxy to an HTTP/1.1 server (`http://127.0.0.1:3000`), WebSockets included.
#[derive(Clone)]
pub struct Upstream {
    base: Arc<str>,
    client: Arc<Client<HttpConnector, Body>>,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Upstream").field(&self.base).finish()
    }
}

/// Hop-by-hop headers (RFC 9110 §7.6.1), not forwarded.
const HOP_HEADERS: &[&str] = &["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade"];

fn strip_hop_headers(h: &mut HeaderMap) {
    let listed: Vec<HeaderName> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|s| s.trim().to_ascii_lowercase()).collect::<Vec<_>>())
        .filter_map(|n| HeaderName::from_bytes(n.as_bytes()).ok())
        .collect();
    for n in listed {
        h.remove(n);
    }
    for n in HOP_HEADERS {
        h.remove(*n);
    }
}

impl Upstream {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let uri: Uri = base.parse().map_err(|_| anyhow::anyhow!("{base} isn't a URL"))?;
        anyhow::ensure!(uri.scheme_str() == Some("http") && uri.host().is_some(), "{base}: an upstream is http://host:port");
        let client = Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build_http();
        Ok(Self { base: base.into(), client: Arc::new(client) })
    }

    pub async fn forward(&self, mut req: Request<Body>) -> Response<Body> {
        let wants_upgrade = req.headers().get(header::CONNECTION).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")));
        let upgrade = req.headers().get(header::UPGRADE).cloned().filter(|_| wants_upgrade);
        let pq = req.uri().path_and_query().map_or("/", |p| p.as_str());
        match format!("{}{pq}", self.base).parse::<Uri>() {
            Ok(uri) => *req.uri_mut() = uri,
            Err(_) => return text(StatusCode::BAD_REQUEST, "bad path\n"),
        }
        strip_hop_headers(req.headers_mut());
        let client_side = upgrade.map(|u| {
            req.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
            req.headers_mut().insert(header::UPGRADE, u);
            hyper::upgrade::on(&mut req)
        });
        let mut resp = match self.client.request(req).await {
            Ok(r) => r,
            Err(e) => {
                debug!(upstream = %self.base, error = %e, "edge: upstream request failed");
                return text(StatusCode::BAD_GATEWAY, "upstream unavailable\n");
            }
        };
        if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
            if let Some(client_side) = client_side {
                let upstream_side = hyper::upgrade::on(&mut resp);
                tokio::spawn(async move {
                    if let (Ok(c), Ok(u)) = tokio::join!(client_side, upstream_side) {
                        let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(u)).await;
                    }
                });
            }
        } else {
            strip_hop_headers(resp.headers_mut());
        }
        resp.map(Body::new)
    }
}

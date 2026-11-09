//! The DynamoDB-compatible API (Alternator), served by `nosql-alternator` on the engine.
//!
//! - **Routing.** A request names its database by its SigV4 access key (`role@db`: the role
//!   signs, the database serves) or by its `Host` (`<db>.alternator.<domain>`), else it goes
//!   to the directory's default database (single mode). Unrouted API requests get DynamoDB's
//!   `UnrecognizedClientException`.
//! - **Scale to zero.** Every request holds a [`Lease`] while it runs: it wakes an unloaded
//!   database and keeps it loaded, and its end counts as activity for the idle timeout, like a
//!   CQL connection's.
//! - **Per database.** The first request builds the database's [`Alternator`]: tables in the
//!   engine ([`EngineBackend`]), SigV4 enforced when the database authenticates its clients
//!   (`PasswordAuthenticator`) with the roles' salted hashes as secrets, and the TTL
//!   expiration scanner, which stops when the database is unloaded.
//! - **Metrics.** API requests count in the database's [`Metrics`] (`requests`,
//!   `alternator_requests`, `alternator_errors`, bytes in and out).
//! - **HTTPS.** [`serve_tls`] terminates TLS (ScyllaDB's `alternator_https_port`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{ConnectInfo, Request};
use axum::response::Response;
use nosql_alternator::auth::{Credentials, RoleSecret};
use nosql_alternator::engine::EngineBackend;
use nosql_alternator::engine::auth::RoleCredentials;
use nosql_alternator::http::{HttpResponse, Route};
use nosql_alternator::server::{read_request, to_axum};
use nosql_alternator::{Alternator, Config, Error, ErrorKind};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::directory::{Directory, Lease, Loaded, Metrics, OpenError};

/// How a directory's databases serve Alternator.
#[derive(Clone, Debug)]
pub struct AlternatorOptions {
    /// What every database's Alternator starts from (`enforce_authorization` follows the
    /// database's authenticator).
    pub config: Config,
    /// `alternator_ttl_period_in_seconds`: how often expired items are looked for.
    pub ttl_period: Duration,
}

impl Default for AlternatorOptions {
    fn default() -> Self {
        AlternatorOptions { config: Config::default(), ttl_period: Duration::from_secs(86_400) }
    }
}

/// Role secrets for access keys that name their database (`role@db`).
struct KeyCredentials(RoleCredentials);

#[async_trait]
impl Credentials for KeyCredentials {
    async fn secret(&self, access_key: &str) -> nosql_alternator::Result<Option<RoleSecret>> {
        self.0.secret(&self.role_of(access_key)).await
    }

    fn role_of(&self, access_key: &str) -> String {
        access_key.rsplit_once('@').map_or(access_key, |(r, _)| r).to_string()
    }
}

/// The Alternator of a loaded database (built on its first request).
pub fn alternator_of(l: &Loaded, opts: &AlternatorOptions) -> Arc<Alternator> {
    l.alternator
        .get_or_init(|| {
            let backend = Arc::new(EngineBackend::new(l.db.clone()));
            let mut config = opts.config.clone();
            config.enforce_authorization = l.spec.requires_auth();
            let creds = Arc::new(KeyCredentials(RoleCredentials(l.db.clone())));
            let alt = Alternator::new(backend.clone(), config).with_credentials(creds);
            backend.spawn_expiration(opts.ttl_period, Some(alt.metrics.clone()));
            let alt = Arc::new(alt);
            // `system.config` shows Alternator's actual flags (a test reading an option and
            // writing it back restores it).
            let c = alt.config();
            for (name, value) in [
                ("alternator_enforce_authorization", c.enforce_authorization.to_string()),
                ("alternator_warn_authorization", c.warn_authorization.to_string()),
                ("alternator_allow_system_table_write", c.allow_system_table_write.to_string()),
                ("alternator_force_read_before_write", c.force_read_before_write.to_string()),
                ("alternator_streams_increased_compatibility", c.streams_increased_compatibility.to_string()),
                ("alternator_max_items_in_batch_write", c.max_items_in_batch_write.to_string()),
                ("alternator_streams_time_window_s", (c.streams_time_window_ms / 1000).to_string()),
            ] {
                l.db.show_option(name, &value);
            }
            // Live options set with CQL (`UPDATE system.config`) apply to Alternator too: those
            // set before it existed, then every later change.
            let settings = l.db.settings();
            for (name, value) in &settings.scylla.values {
                if settings.scylla.cql_sourced.contains(name) {
                    alt.follow_option(name, value);
                }
            }
            let weak = Arc::downgrade(&alt);
            l.db.on_option_set(Arc::new(move |name: &str, value: &str| {
                if let Some(alt) = weak.upgrade() {
                    alt.follow_option(name, value);
                }
            }));
            alt
        })
        .clone()
}

/// A Prometheus endpoint (`GET /metrics`) of the directory's default database's Alternator
/// metrics (ScyllaDB's port 9180; single mode).
pub fn metrics_router(dir: Arc<dyn Directory>, opts: AlternatorOptions) -> axum::Router {
    let state = Arc::new((dir, opts));
    axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let state = state.clone();
            async move {
                let (dir, opts) = &*state;
                let Some(spec) = dir.route_alternator(None, None).await else {
                    return String::new();
                };
                match dir.open(&spec).await {
                    Ok(l) => alternator_of(&l, opts).metrics.render(),
                    Err(_) => String::new(),
                }
            }
        }),
    )
}

/// The engine backend of a loaded database's Alternator (tests, the TTL scanner).
pub fn backend_of(l: &Loaded, opts: &AlternatorOptions) -> Arc<dyn nosql_alternator::Backend> {
    alternator_of(l, opts).backend.clone()
}

/// The peer of a connection (plain or TLS).
#[derive(Clone, Copy, Debug)]
pub struct Peer(pub SocketAddr);

/// A connection's row in its database's `system.clients` (`client_type` `alternator`),
/// registered by its first request and dropped with the connection.
#[derive(Clone, Default)]
struct ConnSlot(Arc<parking_lot::Mutex<Option<(String, nosql_engine::system::clients::ClientHandle)>>>);

impl ConnSlot {
    fn note(&self, lease: &Lease, peer: Option<SocketAddr>, user_agent: Option<String>, tls: bool) {
        let mut slot = self.0.lock();
        if slot.as_ref().is_some_and(|(id, _)| id == lease.id()) {
            if let Some((_, h)) = slot.as_ref() {
                h.request();
            }
            return;
        }
        let info = nosql_engine::ClientInfo {
            address: peer,
            driver_name: user_agent,
            client_type: Some("alternator".into()),
            ..Default::default()
        };
        let h = lease.db().register_client(info);
        h.ready();
        if tls {
            h.set_tls(Some(nosql_engine::system::clients::Tls { protocol: "TLSv1.3".into(), cipher_suite: String::new() }));
        }
        h.request();
        *slot = Some((lease.id().to_string(), h));
    }
}


/// An axum router serving every database of `dir`.
pub fn router(dir: Arc<dyn Directory>, opts: AlternatorOptions) -> axum::Router {
    let state = Arc::new((dir, opts));
    axum::Router::new().fallback(move |req: Request| {
        let state = state.clone();
        async move { handle(&state.0, &state.1, req).await }
    })
}

fn unrouted(r: &nosql_alternator::http::HttpRequest) -> Response {
    let resp = match nosql_alternator::http::route(&r.method, &r.path) {
        Route::Api => HttpResponse::error(&Error::new(ErrorKind::UnrecognizedClient, "The security token included in the request is invalid.")),
        Route::Health | Route::Options => {
            HttpResponse::new(200, "text/plain", format!("healthy: {}", r.header("host").unwrap_or_default()))
        }
        _ => nosql_alternator::http::not_found(),
    };
    to_axum(resp)
}

fn unavailable(e: &OpenError) -> Response {
    // Closing is momentary: a throttling error makes SDKs retry.
    let (status, ty) = match e {
        OpenError::Closing => (400, "com.amazon.coral.availability#ThrottlingException"),
        OpenError::Unavailable(_) => (500, "com.amazonaws.dynamodb.v20120810#InternalServerError"),
    };
    let body = format!(r#"{{"__type":"{ty}","message":{}}}"#, serde_json::Value::String(e.to_string()));
    to_axum(HttpResponse::new(status, nosql_alternator::http::JSON_CONTENT_TYPE, body))
}

async fn handle(dir: &Arc<dyn Directory>, opts: &AlternatorOptions, req: Request) -> Response {
    let peer = req.extensions().get::<ConnectInfo<Peer>>().map(|c| c.0.0);
    let conn = req.extensions().get::<ConnSlot>().cloned();
    let tls = req.extensions().get::<Tls>().is_some();
    let mut r = match read_request(req).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    r.peer = r.peer.or(peer);
    let host = r.header("host");
    let key = nosql_alternator::auth::access_key_id(&r);
    let Some(spec) = dir.route_alternator(host.as_deref(), key.as_deref()).await else {
        return unrouted(&r);
    };
    let lease: Lease = match dir.open(&spec).await {
        Ok(l) => l,
        Err(e) => return unavailable(&e),
    };
    let alt = alternator_of(&lease, opts);
    if let Some(c) = &conn {
        c.note(&lease, r.peer, r.header("user-agent"), tls);
    }
    let api = matches!(nosql_alternator::http::route(&r.method, &r.path), Route::Api);
    let m = &lease.metrics;
    Metrics::add(&m.bytes_in, r.body.len() as u64);
    if api {
        Metrics::add(&m.requests, 1);
        Metrics::add(&m.alternator_requests, 1);
    }
    let resp = alt.handle(r).await;
    Metrics::add(&m.bytes_out, resp.body.len() as u64);
    if api && resp.status >= 400 {
        Metrics::add(&m.errors, 1);
        Metrics::add(&m.alternator_errors, 1);
    }
    drop(lease);
    to_axum(resp)
}

/// Marks requests that came over TLS.
#[derive(Clone, Copy)]
struct Tls;

/// One HTTP/1.1 connection. hyper's own `Date` header is off: Alternator writes it (unless
/// `alternator_http_response_disable_date_header`).
async fn serve_connection<I>(io: I, peer: SocketAddr, app: axum::Router, tls: bool)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tower::ServiceExt;
    let slot = ConnSlot::default();
    let svc = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        let app = app.clone();
        req.extensions_mut().insert(ConnectInfo(Peer(peer)));
        req.extensions_mut().insert(slot.clone());
        if tls {
            req.extensions_mut().insert(Tls);
        }
        async move { app.oneshot(req.map(axum::body::Body::new)).await }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .auto_date_header(false)
        .keep_alive(true)
        .serve_connection(hyper_util::rt::TokioIo::new(io), svc);
    if let Err(e) = conn.await {
        tracing::debug!(error = %e, %peer, "alternator connection ended");
    }
}

/// Serves Alternator over HTTP on `listener` until it fails.
pub async fn serve(listener: TcpListener, dir: Arc<dyn Directory>, opts: AlternatorOptions) -> std::io::Result<()> {
    let app = router(dir, opts);
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = %e, "alternator accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        tokio::spawn(serve_connection(sock, peer, app.clone(), false));
    }
}

/// Serves Alternator over HTTPS on `listener` until it fails.
pub async fn serve_tls(listener: TcpListener, tls: TlsAcceptor, dir: Arc<dyn Directory>, opts: AlternatorOptions) -> std::io::Result<()> {
    use axum::serve::Listener as _;
    let app = router(dir, opts);
    let mut listener = TlsListener::new(listener, tls);
    loop {
        let (io, peer) = listener.accept().await;
        tokio::spawn(serve_connection(io, peer, app.clone(), true));
    }
}

/// Binds `addr` and serves Alternator (HTTP) in the background; returns the bound address.
pub async fn start(dir: Arc<dyn Directory>, opts: AlternatorOptions, addr: SocketAddr) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(e) = serve(listener, dir, opts).await {
            tracing::warn!(error = %e, "alternator listener failed");
        }
    });
    Ok(bound)
}

/// TLS connections: handshakes run concurrently (a slow client doesn't hold up others) and
/// finished ones are handed to the server.
pub struct TlsListener {
    local: SocketAddr,
    rx: tokio::sync::mpsc::Receiver<(tokio_rustls::server::TlsStream<tokio::net::TcpStream>, SocketAddr)>,
}

impl TlsListener {
    pub fn new(listener: TcpListener, tls: TlsAcceptor) -> TlsListener {
        let local = listener.local_addr().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let (sock, peer) = match listener.accept().await {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::debug!(error = %e, "alternator accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if tx.is_closed() {
                    return;
                }
                let (tls, tx) = (tls.clone(), tx.clone());
                tokio::spawn(async move {
                    if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(10), tls.accept(sock)).await {
                        let _ = tx.send((s, peer)).await;
                    }
                });
            }
        });
        TlsListener { local, rx }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if let Some(c) = self.rx.recv().await {
                return c;
            }
            // The acceptor stopped: nothing more will arrive.
            std::future::pending::<()>().await;
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

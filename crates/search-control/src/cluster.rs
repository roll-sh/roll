//! roll search on a roll-cluster fleet (docs/search/architecture.md, "On the fleet"): every
//! cluster has one owner node at a time, the holder of the lease `search:cluster:<name>`, taken
//! from rolld's roll-cluster handle.
//!
//! - **Loading** ([`ClusterLeases`], search-server's [`Leases`]): a cluster is loaded only under a
//!   guard of its lease, kept for as long as it stays loaded, and unloaded when the guard's tenure
//!   ends (a handoff, a drain, the lease lost). Acknowledgements check the guard
//!   ([`Leases::validity`]). Unloading drops the guard and gives the idle lease up
//!   ([`Cluster::release_if_idle`]), so a sleeping cluster belongs to nobody.
//! - **Epochs** are search's own, from the ownership markers in object storage (search-store's
//!   `claim_cluster`): roll-cluster's restart at 1 once a lease row is deleted, so they only order
//!   the holders of one tenure.
//! - **Requests** for a cluster run on its owner ([`dispatch`]): here, or forwarded to the owner's
//!   `/internal/search/rest/…` ([`REST_FORWARDED`]) and served there as if they had come in on its
//!   own REST port. The control plane's own requests to a cluster (a link's credential, which
//!   rolld's apps glue asks any node for) go the same way ([`to_owner`]).
//! - **Client addresses** (a cluster's IP allowlist, docs/search/architecture.md, "Client
//!   addresses and IP allowlists"): the node a client reached decides its address (the edge's,
//!   or [`serve_front`]'s) and a forward carries it signed (`x-roll-code-client`); the owner
//!   believes only that ([`origin_of`]). The internal route takes signed requests only, so a
//!   client can't come in there with an address of its choosing. The control plane's own
//!   forwards carry [`INTERNAL_CLIENT`] instead, which no address parses as.

use async_trait::async_trait;
use axum::extract::ConnectInfo;
use search_server::Origin;
use std::net::{IpAddr, SocketAddr};
use parking_lot::Mutex;
use roll_cluster::{Cluster, Guard, Route};
use search_server::lease::{Leases, Owner};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// Lease keys of clusters: `search:cluster:<name>` (a cluster's name is unique and immutable).
pub const KEY_PREFIX: &str = "search:cluster:";

/// The internal route REST requests other nodes forward come in on.
pub const REST_FORWARDED: &str = "/internal/search/rest";

/// The client a forward of the control plane's own request names (signed, like a client's
/// address): roll itself, which no IP allowlist filters. It isn't an address, so nothing a
/// listener decides can be it.
pub const INTERNAL_CLIENT: &str = "roll-search-control";

pub fn key(name: &str) -> String {
    format!("{KEY_PREFIX}{name}")
}

/// How long a request or a wake waits for a cluster that's moving: a lease, two sweeps and a
/// margin.
pub fn ride_out(cluster: &Cluster) -> Duration {
    let c = cluster.config();
    c.lease_ttl + c.sweep_every * 2 + Duration::from_secs(1)
}

/// Unloads a cluster whose guard ended (called with its name; true once it is unloaded).
pub type Unload = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

/// search-server's leases over roll-cluster: the guard of every loaded cluster.
pub struct ClusterLeases {
    cluster: Arc<Cluster>,
    held: Arc<Mutex<HashMap<String, Guard>>>,
    unload: std::sync::OnceLock<Unload>,
    me: Weak<ClusterLeases>,
}

impl ClusterLeases {
    pub fn new(cluster: Arc<Cluster>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self { cluster, held: Arc::default(), unload: std::sync::OnceLock::new(), me: me.clone() })
    }

    /// What unloads a cluster when its guard ends (the control plane sets it once).
    pub fn on_ending(&self, unload: Unload) {
        let _ = self.unload.set(unload);
    }

    pub fn cluster(&self) -> &Arc<Cluster> {
        &self.cluster
    }

    /// Unload `name` as soon as no request runs on it, once its guard starts ending; give up
    /// waiting (and unload anyway: the guard is no longer valid, so nothing more is
    /// acknowledged) after the handoff timeout.
    fn watch(&self, name: String, ending: impl std::future::Future<Output = ()> + Send + 'static) {
        let me = self.me.clone();
        tokio::spawn(async move {
            ending.await;
            let Some(me) = Weak::upgrade(&me) else { return };
            let Some(unload) = me.unload.get().cloned() else { return };
            let deadline = tokio::time::Instant::now() + me.cluster.config().handoff_timeout;
            while me.held.lock().contains_key(&name) {
                if unload(name.clone()).await || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
    }
}

#[async_trait]
impl Leases for ClusterLeases {
    async fn acquire(&self, name: &str, _node: &str, _ttl: Duration) -> anyhow::Result<Result<u64, Owner>> {
        let key = key(name);
        let deadline = tokio::time::Instant::now() + ride_out(&self.cluster);
        loop {
            let wait = match self.cluster.route_async(&key).await {
                Ok(Route::Local(g)) => {
                    let epoch = g.epoch().max(1) as u64;
                    let ending = g.ending();
                    self.held.lock().insert(name.to_string(), g);
                    self.watch(name.to_string(), ending);
                    return Ok(Ok(epoch));
                }
                Ok(Route::Forward(n)) => return Ok(Err(Owner { node: n.id, epoch: 0, remaining_ms: 0 })),
                Ok(Route::Unavailable { retry_after }) => retry_after,
                Err(e) => {
                    tracing::warn!(cluster = name, error = %e, "search: routing a cluster failed");
                    Duration::from_millis(500)
                }
            };
            if tokio::time::Instant::now() + wait >= deadline {
                anyhow::bail!("cluster [{name}] is moving between nodes; try again");
            }
            tokio::time::sleep(wait.clamp(Duration::from_millis(20), self.cluster.config().max_retry_wait)).await;
        }
    }

    async fn renew(&self, name: &str, _node: &str, _epoch: u64, _ttl: Duration) -> anyhow::Result<bool> {
        Ok(self.held.lock().get(name).is_some_and(Guard::is_valid))
    }

    async fn release(&self, name: &str, _node: &str, _epoch: u64) -> anyhow::Result<()> {
        let guard = self.held.lock().remove(name);
        drop(guard);
        // An unloaded cluster belongs to nobody: the next request takes it where it's placed.
        // The request that unloaded it may still hold a guard; try again as it finishes (the
        // idle sweep gets it otherwise), unless the cluster was loaded again meanwhile.
        let (c, key, held, name) = (self.cluster.clone(), key(name), Arc::downgrade(&self.held), name.to_string());
        tokio::spawn(async move {
            for _ in 0..40 {
                let (c2, key2, held, name) = (c.clone(), key.clone(), held.clone(), name.clone());
                let gone = move || held.upgrade().is_none_or(|h| !h.lock().contains_key(&name));
                let released = tokio::task::spawn_blocking(move || c2.release_if_idle(&key2, Duration::ZERO, gone)).await.unwrap_or(false);
                if released || !c.owns(&key) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        Ok(())
    }

    async fn owner(&self, name: &str) -> anyhow::Result<Option<Owner>> {
        let (c, key) = (self.cluster.clone(), key(name));
        let rec = tokio::task::spawn_blocking(move || c.lease_record(&key)).await??;
        Ok(rec.map(|(node, epoch)| Owner { node, epoch: epoch.max(0) as u64, remaining_ms: 0 }))
    }

    fn store_epochs(&self) -> bool {
        true
    }

    fn validity(&self, name: &str) -> Option<search_server::lease::Validity> {
        let held = Arc::downgrade(&self.held);
        let name = name.to_string();
        Some(Arc::new(move || held.upgrade().is_some_and(|h| h.lock().get(&name).is_some_and(Guard::is_valid))))
    }
}

/// Gives up the leases of clusters that aren't loaded and had no request for a while: API
/// calls about a sleeping cluster take its lease (they run on its owner) without loading it.
pub(crate) async fn release_idle(control: Weak<crate::Control>) {
    const IDLE: Duration = Duration::from_secs(30);
    loop {
        tokio::time::sleep(IDLE / 3).await;
        let Some(c) = control.upgrade() else { return };
        let Some(cluster) = c.cluster_leases().map(|l| l.cluster().clone()) else { return };
        for key in cluster.idle(IDLE) {
            let Some(name) = key.strip_prefix(KEY_PREFIX).map(str::to_string) else { continue };
            let (cl, node) = (cluster.clone(), c.node.clone());
            let _ = tokio::task::spawn_blocking(move || cl.release_if_idle(&key, IDLE, || !node.loaded().contains(&name))).await;
        }
    }
}

/// Runs a `/v1/search/clusters/{id}/…` call about cluster `name` on its owner (a cluster's
/// settings, credentials, indices and console reach it where it's loaded); here off a fleet. The
/// API resolves the cluster and checks the caller first (`crate::api`).
pub(crate) async fn on_owner(c: &crate::Control, name: &str, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let Some(cluster) = c.cluster_leases().map(|l| l.cluster().clone()) else { return next.run(req).await };
    cluster
        .dispatch(&key(name), req, |req, guard| async move {
            let r = next.run(req).await;
            drop(guard);
            r
        })
        .await
}

/// The Elasticsearch REST API over axum, for a fleet node's REST port and the edge's
/// `<cluster>.search.<region>.<domain>`: each request runs on its cluster's owner. A request
/// that names no cluster (no credentials, a host outside the domain) is answered here, as the
/// node answers it (401, `GET /` without a cluster).
pub fn rest_router(control: Arc<crate::Control>) -> axum::Router {
    axum::Router::new().fallback(move |req: axum::extract::Request| {
        let c = control.clone();
        async move { rest(c, req).await }
    })
}

/// The internal API's part: REST requests other nodes forwarded here ([`REST_FORWARDED`]),
/// signed by a node of the cluster or refused (401): the client address a forward carries is
/// believed only because a node signed it.
pub fn internal_router(control: Arc<crate::Control>, cluster: Arc<Cluster>) -> axum::Router {
    axum::Router::new()
        .nest_service(REST_FORWARDED, rest_router(control))
        .layer(axum::middleware::from_fn_with_state(cluster, roll_cluster::require_signed))
}

/// Who a REST request comes from, for the cluster's IP allowlist: a forward's client, signed by
/// the node that took it (an address, or [`INTERNAL_CLIENT`]); else the address the listener
/// that took it decided (`ConnectInfo`: the edge's, or [`serve_front`]'s); else nobody anyone
/// vouches for.
pub fn origin_of(ext: &axum::http::Extensions) -> Origin {
    if let Some(f) = ext.get::<roll_cluster::Forwarded>() {
        return match f.client.as_deref() {
            Some(INTERNAL_CLIENT) => Origin::Internal,
            Some(c) => c.parse::<IpAddr>().map_or(Origin::Unknown, Origin::Client),
            None => Origin::Unknown,
        };
    }
    ext.get::<ConnectInfo<SocketAddr>>().map_or(Origin::Unknown, |c| Origin::Client(c.0.ip()))
}

/// The address [`FrontListener`] took a connection's requests from: the socket's peer, or the
/// PROXY header's source.
#[derive(Clone, Copy, Debug)]
struct Peer(SocketAddr);

/// A fleet node's REST port as an axum listener: each connection's PROXY header (when the
/// listener expects one) is read on a task of its own, so a slow one holds up nobody else.
struct FrontListener {
    local: SocketAddr,
    ready: tokio::sync::mpsc::Receiver<(tokio::net::TcpStream, Peer)>,
}

impl FrontListener {
    fn new(listener: tokio::net::TcpListener, proxy_protocol: bool) -> std::io::Result<FrontListener> {
        let local = listener.local_addr()?;
        let (tx, ready) = tokio::sync::mpsc::channel(256);
        tokio::spawn(async move {
            loop {
                let (mut sock, addr) = tokio::select! {
                    a = listener.accept() => match a {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::warn!(error = %e, "accept failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    },
                    _ = tx.closed() => return,
                };
                let _ = sock.set_nodelay(true);
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Some(ip) = search_server::http::connection_peer(&mut sock, addr.ip(), proxy_protocol).await {
                        let _ = tx.send((sock, Peer(SocketAddr::new(ip, addr.port())))).await;
                    }
                });
            }
        });
        Ok(FrontListener { local, ready })
    }
}

impl axum::serve::Listener for FrontListener {
    type Io = tokio::net::TcpStream;
    type Addr = Peer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(c) => c,
            // The accept task only ends once this listener is gone.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(Peer(self.local))
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, FrontListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, FrontListener>) -> Peer {
        *stream.remote_addr()
    }
}

/// Serves a fleet node's Elasticsearch REST port until `stop` resolves: [`rest_router`] with
/// each request's client address decided as the node's own listener decides it (the PROXY
/// header's source with `opts.proxy_protocol`, a trusted proxy's header per `opts.client`), so
/// a forward carries it to the cluster's owner.
pub async fn serve_front(listener: tokio::net::TcpListener, control: Arc<crate::Control>, opts: &search_server::ListenOptions, stop: impl std::future::Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    let client = opts.client.clone();
    let front = rest_router(control).layer(axum::middleware::from_fn(move |mut req: axum::extract::Request, next: axum::middleware::Next| {
        let client = client.clone();
        async move {
            match req.extensions().get::<ConnectInfo<Peer>>().map(|c| c.0.0) {
                Some(peer) => {
                    let ip = client.client(peer.ip(), req.headers());
                    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ip, peer.port())));
                }
                // Not from this listener: vouch for nobody.
                None => {
                    req.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
                }
            }
            next.run(req).await
        }
    }));
    let listener = FrontListener::new(listener, opts.proxy_protocol)?;
    axum::serve(listener, front.into_make_service_with_connect_info::<Peer>()).with_graceful_shutdown(stop).await
}

async fn rest(c: Arc<crate::Control>, req: axum::extract::Request) -> axum::response::Response {
    let node = c.node.clone();
    let fleet = c.cluster_leases().map(|l| l.cluster().clone());
    let name = fleet.as_ref().and_then(|_| search_server::auth::cluster_named(req.headers(), node.domain()));
    let (Some(cluster), Some(name)) = (fleet, name) else { return local(node, req).await };
    let original = req.uri().clone();
    let pq = original.path_and_query().map_or("/", |p| p.as_str());
    // A forward goes to the owner's internal route (roll-cluster forwards the `OriginalUri`).
    let Ok(forwarded) = format!("{REST_FORWARDED}{pq}").parse::<axum::http::Uri>() else { return local(node, req).await };
    let key = key(&name);
    // While a cluster moves (a drain, a takeover) nodes can briefly disagree on its owner: a
    // request whose body is small enough to keep is sent again for up to a lease, as a client
    // would; a larger one goes once.
    let (parts, body) = req.into_parts();
    let small = match parts.headers.get(axum::http::header::CONTENT_LENGTH) {
        Some(v) => v.to_str().ok().and_then(|v| v.parse::<usize>().ok()).is_some_and(|n| n <= REPLAY_BYTES),
        // No length and not chunked: no body.
        None => !parts.headers.contains_key(axum::http::header::TRANSFER_ENCODING),
    };
    let body = match small {
        true => match axum::body::to_bytes(body, REPLAY_BYTES).await {
            Ok(b) => Some(b),
            Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        },
        false => {
            let req = axum::http::Request::from_parts(parts, body);
            return send(&cluster, &key, req, forwarded, original, node).await;
        }
    };
    let deadline = tokio::time::Instant::now() + ride_out(&cluster);
    let mut wait = Duration::from_millis(20);
    loop {
        let req = axum::http::Request::from_parts(parts.clone(), axum::body::Body::from(body.clone().unwrap_or_default()));
        let r = send(&cluster, &key, req, forwarded.clone(), original.clone(), node.clone()).await;
        let moving = r.status() == axum::http::StatusCode::LOOP_DETECTED || r.headers().contains_key("x-roll-code-unavailable");
        if !moving || tokio::time::Instant::now() + wait >= deadline {
            return r;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(cluster.config().max_retry_wait);
    }
}

/// Bodies up to this size are kept so a request can be sent again while its cluster moves.
const REPLAY_BYTES: usize = 4 << 20;

/// The most of an owner's answer [`to_owner`] reads (Elasticsearch's `http.max_content_length`).
const ANSWER_BYTES: usize = 100 << 20;

/// Sends one of the control plane's own REST requests about cluster `name` to the cluster's owner
/// when that's another node of the fleet, as that node's REST front would take it (`None`: serve
/// it here, where it's owned or nobody's).
pub(crate) async fn to_owner(c: &crate::Control, name: &str, req: &search_es::EsRequest) -> Option<search_es::EsResponse> {
    let cluster = c.cluster_leases()?.cluster().clone();
    let Ok(Route::Forward(owner)) = cluster.route_async(&key(name)).await else { return None };
    let mut r = axum::http::Request::builder().method(req.method.as_str()).uri(format!("{REST_FORWARDED}{}", req.uri));
    for (k, v) in &req.headers {
        r = r.header(k, v);
    }
    let mut r = r.body(axum::body::Body::from(req.body.clone())).ok()?;
    // roll's own request: the owner admits it whatever the cluster's IP allowlist says.
    r.extensions_mut().insert(roll_cluster::ClientAddr(INTERNAL_CLIENT.into()));
    let resp = cluster.forward(&owner, r, 0).await;
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp.headers().iter().filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string()))).collect();
    // A streamed answer stays one (server-sent events, as the owner sends them).
    if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.starts_with("text/event-stream")) {
        use futures::StreamExt;
        let pieces = resp.into_body().into_data_stream().take_while(|r| std::future::ready(r.is_ok())).filter_map(|r| std::future::ready(r.ok()));
        let stream = search_es::rest::Streamed::new(Box::pin(pieces));
        return Some(search_es::EsResponse { status, headers, body: bytes::Bytes::new(), head: false, stream: Some(stream) });
    }
    let body = axum::body::to_bytes(resp.into_body(), ANSWER_BYTES).await.unwrap_or_default();
    Some(search_es::EsResponse { status, headers, body, head: false, stream: None })
}

async fn send(cluster: &Cluster, key: &str, mut req: axum::extract::Request, forwarded: axum::http::Uri, original: axum::http::Uri, node: Arc<search_server::Node>) -> axum::response::Response {
    req.extensions_mut().insert(axum::extract::OriginalUri(forwarded.clone()));
    *req.uri_mut() = forwarded;
    cluster
        .dispatch(key, req, |mut req, guard| async move {
            *req.uri_mut() = original.clone();
            req.extensions_mut().insert(axum::extract::OriginalUri(original));
            let r = local(node, req).await;
            drop(guard);
            r
        })
        .await
}

/// Serves a request on this node, as its own REST port does.
async fn local(node: Arc<search_server::Node>, req: axum::extract::Request) -> axum::response::Response {
    let origin = origin_of(req.extensions());
    match search_server::http::serve_request(node, req, origin).await {
        Ok(r) => r.map(axum::body::Body::new),
        // The client went away or sent a body that can't be read: nobody to answer.
        Err(_) => axum::http::StatusCode::BAD_REQUEST.into_response(),
    }
}

use axum::response::IntoResponse;

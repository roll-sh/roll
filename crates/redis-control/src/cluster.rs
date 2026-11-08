//! Redis on many nodes (docs/cloud/README.md; the cluster layer is docs/code/cluster.md).
//!
//! - **One owner per database.** A database is served by the holder of its lease,
//!   `redis:db:<id>`; a read replica's is `redis:db:<primary>#<replica>`, which the cluster places
//!   as a standby of its primary (another node, another zone when one is live). The node holds a
//!   guard for as long as the database is loaded ([`ClusterOwner`]), so a handoff waits for it to
//!   unload. Leases of databases that aren't loaded are given back after a while.
//! - **Any node accepts any connection** ([`Front`]): the TLS server name names the database (the
//!   connection is spliced to the owner right after the handshake), else the first AUTH or HELLO
//!   … AUTH does (`redis_server::preauth`). A database that is moving is waited for (about a
//!   lease's length) instead of failing.
//! - **REST and the control API** (`/v1/redis/databases/{id}/…`) are forwarded to the owner by
//!   the cluster layer ([`owner_routes`], [`rest_routes`]).
//! - **Handoff** (a drain, rebalancing): the old owner closes the database's connections over
//!   `handoff_spread`, each at its own moment (commands in flight are answered first), flushes
//!   everything to object storage, writes the final manifest, and only then lets the lease go;
//!   nothing acknowledged is lost, `async` durability included. Clients reconnect, through any
//!   node, to the new owner.
//! - **Crash**: the lease lapses; the next owner fences the old owner's log and replays it
//!   (`redis_server::persist`), so storage, not the lease, decides what the history is. `async`
//!   databases can lose what their owner hadn't flushed (up to a second); `sync` ones lose
//!   nothing.
//! - Garbage collection runs on one node at a time (the `redis-gc` singleton).
//!
//! Without a cluster none of this runs: a node serves every database it's asked for.

use crate::control::{Control, spec_for};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bytes::BytesMut;
use futures::future::BoxFuture;
use redis_server::conn::{Fixed, Portable};
use redis_server::directory::{AuthRequest, Directory};
use redis_server::node::{Owner, Refused, Tenure};
use redis_server::preauth::{self, FirstAuth};
use roll_cluster::{Cluster, Guard, Node, Route};
use roll_edge::splice::{Hop, Signer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// The product name spliced connections carry (`roll_edge::splice::Hop::product`).
pub const PRODUCT: &str = "redis";
const KEY_PREFIX: &str = "redis:db:";
/// Splices a connection may take from the node it reached to its database (a move under way
/// can need a second).
pub const MAX_SPLICES: u8 = 3;
/// How long a connection may take to name its database.
const FIRST_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// A database's lease key: `redis:db:<id>`, or for a read replica `redis:db:<primary>#<id>`.
pub fn db_key(db: &str, primary: Option<&str>) -> String {
    match primary {
        Some(p) => format!("{KEY_PREFIX}{p}#{db}"),
        None => format!("{KEY_PREFIX}{db}"),
    }
}

/// The database a lease key is for.
fn key_db(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(KEY_PREFIX)?;
    Some(rest.split_once('#').map_or(rest, |(_, replica)| replica))
}

/// How long a request for a moving database waits for its next owner: about a lease's length.
fn ride_out(c: &Cluster) -> Duration {
    c.config().lease_ttl + c.config().lease_margin
}

/// The node's [`Owner`]: a database is served here while this node holds its lease.
pub struct ClusterOwner(pub Arc<Cluster>);

struct Held(Guard);

impl Tenure for Held {
    fn ending(&self) -> BoxFuture<'static, ()> {
        Box::pin(self.0.ending())
    }
    fn is_ending(&self) -> bool {
        self.0.is_ending()
    }
}

/// Where a database is served right now, waiting out a move for up to `wait`.
async fn locate(cluster: &Cluster, key: &str, wait: Duration) -> Result<Route, String> {
    let deadline = Instant::now() + wait;
    loop {
        match cluster.route_async(key).await {
            Ok(Route::Unavailable { retry_after }) if Instant::now() < deadline => tokio::time::sleep(retry_after.min(Duration::from_millis(200))).await,
            Ok(r) => return Ok(r),
            Err(e) => return Err(e.to_string()),
        }
    }
}

impl Owner for ClusterOwner {
    /// A database that is moving is waited for (a lease's length). One another node serves (this
    /// node just handed it over, while a connection for it was on its way here) is refused: the
    /// connection closes, and the client's reconnect finds the owner.
    fn claim<'a>(&'a self, db: &'a str, primary: Option<&'a str>) -> BoxFuture<'a, Result<Box<dyn Tenure>, Refused>> {
        Box::pin(async move {
            match locate(&self.0, &db_key(db, primary), ride_out(&self.0)).await {
                Ok(Route::Local(g)) => Ok(Box::new(Held(g)) as Box<dyn Tenure>),
                Ok(Route::Forward(n)) => Err(Refused::Elsewhere(n.id)),
                Ok(Route::Unavailable { .. }) | Err(_) => Err(Refused::Moving(db.to_string())),
            }
        })
    }
}

impl Control {
    /// Serve databases only while this node holds their leases on `cluster`. Call once, before
    /// any connection; on a node of its own there is no cluster to join.
    pub fn join(self: &Arc<Self>, cluster: Arc<Cluster>) {
        if self.cluster.set(cluster.clone()).is_err() {
            return;
        }
        self.node.set_owner(Arc::new(ClusterOwner(cluster)));
        tokio::spawn(release_idle_leases(Arc::downgrade(self)));
    }

    pub fn cluster(&self) -> Option<&Arc<Cluster>> {
        self.cluster.get()
    }

    /// Whether this node runs the cluster-wide job `name` (always, on a node of its own).
    pub async fn singleton(&self, name: &str) -> bool {
        let Some(c) = self.cluster.get().cloned() else { return true };
        let name = name.to_string();
        tokio::task::spawn_blocking(move || c.singleton(&name)).await.unwrap_or(false)
    }

    /// The lease key of the database or replica `id`, if it exists.
    pub(crate) fn key_of(&self, id: &str) -> Option<String> {
        let spec = spec_for(&self.store, id).ok().flatten()?;
        Some(db_key(&spec.id, spec.replica_of.as_deref()))
    }
}

/// Leases this node holds for databases it doesn't have loaded (taken by a sweep, or for a
/// control request) go back after a while, so the lease table stays the size of what's in use.
async fn release_idle_leases(control: std::sync::Weak<Control>) {
    const IDLE: Duration = Duration::from_secs(30);
    loop {
        tokio::time::sleep(IDLE / 3).await;
        let Some(control) = control.upgrade() else { return };
        let Some(cluster) = control.cluster().cloned() else { return };
        for key in cluster.idle(IDLE) {
            let Some(db) = key_db(&key).map(str::to_string) else { continue };
            let (c, node) = (cluster.clone(), control.node.clone());
            let _ = tokio::task::spawn_blocking(move || c.release_if_idle(&key, IDLE, || node.get_loaded(&db).is_none())).await;
        }
    }
}

/// Every RESP connection that reaches this node: served here when this node owns its database,
/// else spliced to the owner (`roll_edge::splice`).
pub struct Front {
    pub control: Arc<Control>,
    signer: Signer,
    /// The address shown for spliced connections (`CLIENT LIST`'s `laddr`).
    pub public: SocketAddr,
}

impl Front {
    /// `secret` is the cluster's (it signs the hop to the owner).
    pub fn new(control: Arc<Control>, secret: &str, public: SocketAddr) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self { control, signer: Signer::new(secret)?, public }))
    }

    fn dir(&self) -> Arc<dyn Directory> {
        self.control.node.clone()
    }

    /// Serve a client connection (TLS done; `sni` is its server name).
    pub async fn serve<S: Portable>(self: Arc<Self>, mut stream: S, peer: SocketAddr, local: SocketAddr, sni: Option<String>, fd: i64) {
        let Some(cluster) = self.control.cluster().cloned() else {
            return redis_server::conn::serve_with(stream, peer, local, self.dir(), sni, fd).await;
        };
        let domain = self.control.node.cfg.domain.clone();
        let (db, first) = match sni.as_deref().and_then(|s| redis_server::node::sni_db(s, domain.as_deref())) {
            Some(db) => (Some(db), BytesMut::new()),
            None => match preauth::read_auth(&mut stream, domain.as_deref(), FIRST_AUTH_TIMEOUT).await {
                FirstAuth::Names { db, read } => (Some(db), read),
                FirstAuth::Unknown { read } => (None, read),
                FirstAuth::Closed => return,
            },
        };
        // Unknown databases are answered here (WRONGPASS), and so is one that is still moving
        // after a lease's length (LOADING).
        let hop = match db.as_deref().and_then(|db| Some((db, self.control.key_of(db)?))) {
            Some((db, key)) => self.hop_to_owner(&cluster, &key, Hop { product: PRODUCT.into(), tenant: db.to_string(), client: peer, sni: sni.clone(), hops: 0 }).await,
            None => None,
        };
        match hop {
            Some(mut owner) => {
                let r = async {
                    owner.write_all(&first).await?;
                    tokio::io::copy_bidirectional(&mut stream, &mut owner).await
                };
                if let Err(e) = r.await {
                    tracing::debug!(error = %e, "redis: spliced connection ended");
                }
            }
            None if first.is_empty() => redis_server::conn::serve_with(stream, peer, local, self.dir(), sni, fd).await,
            None => redis_server::conn::serve_with(Fixed(roll_edge::stream::Prefixed::new(first.freeze(), stream)), peer, local, self.dir(), sni, fd).await,
        }
    }

    /// A stream to the owner of `key`, routing again (up to a lease's length) when the node it
    /// was routed to can't be reached (it just died). `None`: serve it here (this node owns it,
    /// or nobody does yet).
    async fn hop_to_owner(&self, cluster: &Cluster, key: &str, hop: Hop) -> Option<impl AsyncRead + AsyncWrite + Unpin + use<>> {
        let deadline = Instant::now() + ride_out(cluster);
        loop {
            let node: Node = match locate(cluster, key, deadline.saturating_duration_since(Instant::now())).await {
                Ok(Route::Forward(node)) => node,
                r => {
                    tracing::debug!(key, route = ?r, "redis: serving the connection here");
                    return None;
                }
            };
            match roll_edge::splice::connect(&node.url, &self.signer, &hop).await {
                Ok(owner) => return Some(owner),
                Err(e) if Instant::now() < deadline => {
                    tracing::debug!(key, node = node.id, error = %e, "redis: owner unreachable; routing again");
                    cluster.forget(key);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => return None,
            }
        }
    }

    /// The owner's side of a splice: serve the stream as if the client had connected here. If the
    /// database moved on while the connection was on its way (this node just handed it over),
    /// pass it on to the new owner, at most [`MAX_SPLICES`] times in all.
    pub fn handler(self: &Arc<Self>) -> roll_edge::splice::Handler {
        let me = self.clone();
        Arc::new(move |io, hop| Box::pin(me.clone().serve_spliced(io, hop)))
    }

    async fn serve_spliced(self: Arc<Self>, mut io: Box<dyn roll_edge::Io>, hop: Hop) {
        if let (Some(cluster), Some(key)) = (self.control.cluster().cloned(), self.control.key_of(&hop.tenant))
            && hop.hops + 1 < MAX_SPLICES
        {
            let next = Hop { hops: hop.hops + 1, ..hop.clone() };
            if let Some(mut owner) = self.hop_to_owner(&cluster, &key, next).await {
                // What the first node read is still unread here: it flows on with the rest.
                if let Err(e) = tokio::io::copy_bidirectional(&mut io, &mut owner).await {
                    tracing::debug!(error = %e, "redis: spliced connection ended");
                }
                return;
            }
        }
        redis_server::conn::serve_with(Fixed(io), hop.client, self.public, self.dir(), hop.sni, -1).await
    }

    /// Accept RESP connections forever (TLS and the PROXY protocol as `opts` says), each through
    /// [`Front::serve`].
    pub async fn serve_listener(self: Arc<Self>, listener: tokio::net::TcpListener, opts: redis_server::net::ListenOptions) -> anyhow::Result<()> {
        let local = listener.local_addr()?;
        loop {
            let (mut sock, mut peer) = listener.accept().await?;
            let _ = sock.set_nodelay(true);
            let (me, opts) = (self.clone(), opts.clone());
            tokio::spawn(async move {
                if opts.proxy_protocol {
                    match tokio::time::timeout(Duration::from_secs(5), redis_server::net::read_proxy_header(&mut sock)).await {
                        Ok(Ok(Some(src))) => peer = src,
                        Ok(Ok(None)) => {}
                        _ => return,
                    }
                }
                let fd = raw_fd(&sock);
                match opts.tls {
                    Some(acceptor) => {
                        let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(sock)).await else { return };
                        let sni = tls.get_ref().1.server_name().map(str::to_ascii_lowercase);
                        me.serve(tls, peer, local, sni, fd).await
                    }
                    None => me.serve(sock, peer, local, None, fd).await,
                }
            });
        }
    }
}

fn raw_fd(sock: &tokio::net::TcpStream) -> i64 {
    use std::os::fd::AsRawFd;
    sock.as_raw_fd() as i64
}

/// Where REST requests forwarded from another node arrive on the owner's internal API (the
/// original path follows).
pub const REST_FORWARDED: &str = "/internal/redis/rest";

/// The REST data plane (`redis_server::http`, Upstash's protocol): a request runs on the owner
/// of the database its token (`<db>_<secret>`) names.
pub fn rest_router(control: Arc<Control>) -> axum::Router {
    redis_server::http::router(control.node.clone()).layer(axum::middleware::from_fn_with_state(control, rest_routes))
}

/// The internal API's part: REST requests other nodes forwarded here ([`REST_FORWARDED`]),
/// checked as signed by a node of the cluster.
pub fn internal_router(control: Arc<Control>) -> axum::Router {
    let r = axum::Router::new().nest_service(REST_FORWARDED, rest_router(control.clone()));
    match control.cluster() {
        Some(c) => r.layer(axum::middleware::from_fn_with_state(c.clone(), roll_cluster::accept_forwarded)),
        None => r,
    }
}

async fn rest_routes(State(control): State<Arc<Control>>, mut req: Request, next: Next) -> Response {
    let Some(cluster) = control.cluster().cloned() else { return next.run(req).await };
    let token = redis_server::http::token(req.headers(), req.uri()).unwrap_or_default();
    let auth = AuthRequest { user: None, password: token.as_bytes(), sni: None, client: None, command: b"auth" };
    let Some(key) = redis_server::node::route(&auth, None).and_then(|(db, _, _)| control.key_of(&db)) else { return next.run(req).await };
    // Sent on, it lands on the owner's internal API, under REST_FORWARDED.
    let original = req.uri().clone();
    let pq = original.path_and_query().map_or("/", |p| p.as_str());
    let Ok(forwarded) = format!("{REST_FORWARDED}{pq}").parse::<axum::http::Uri>() else { return next.run(req).await };
    *req.uri_mut() = forwarded.clone();
    req.extensions_mut().insert(axum::extract::OriginalUri(forwarded));
    cluster
        .dispatch(&key, req, |mut req, guard| async move {
            *req.uri_mut() = original.clone();
            req.extensions_mut().insert(axum::extract::OriginalUri(original));
            let r = next.run(req).await;
            drop(guard);
            r
        })
        .await
}

/// Middleware for the control API: `/v1/redis/databases/{id or name}/…` and
/// `/v1/redis/replicas/{id}` run on the database's owner (they act on it loaded: settings,
/// restores, branches, deletes, the console, its status).
pub async fn owner_routes(State(control): State<Arc<Control>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let named = path
        .strip_prefix("/v1/redis/databases/")
        .or_else(|| path.strip_prefix("/v1/redis/replicas/"))
        .and_then(|rest| rest.split('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let key = named.and_then(|n| {
        let id = control.get(&n).map(|d| d.id).unwrap_or(n);
        control.key_of(&id)
    });
    dispatch(&control, key, req, next).await
}

async fn dispatch(control: &Control, key: Option<String>, req: Request, next: Next) -> Response {
    match (control.cluster(), key) {
        (Some(c), Some(key)) => {
            c.dispatch(&key, req, |req, guard| async move {
                let r = next.run(req).await;
                drop(guard);
                r
            })
            .await
        }
        _ => next.run(req).await,
    }
}

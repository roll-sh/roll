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
use redis_server::node::{Owner, Tenure};
use redis_server::preauth::{self, FirstAuth};
use roll_cluster::{Cluster, Guard, Node, Route};
use roll_edge::splice::{Hop, Signer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The product name spliced connections carry (`roll_edge::splice::Hop::product`).
pub const PRODUCT: &str = "redis";
const KEY_PREFIX: &str = "redis:db:";
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
    fn claim<'a>(&'a self, db: &'a str, primary: Option<&'a str>) -> BoxFuture<'a, Result<Box<dyn Tenure>, String>> {
        Box::pin(async move {
            match locate(&self.0, &db_key(db, primary), ride_out(&self.0)).await? {
                Route::Local(g) => Ok(Box::new(Held(g)) as Box<dyn Tenure>),
                Route::Forward(n) => Err(format!("database {db} is served by node {}; reconnect", n.id)),
                Route::Unavailable { .. } => Err(format!("database {db} is moving between nodes; retry")),
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
    fn key_of(&self, id: &str) -> Option<String> {
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
        let owner = match db.as_deref().and_then(|db| Some((db, self.control.key_of(db)?))) {
            Some((db, key)) => match locate(&cluster, &key, ride_out(&cluster)).await {
                Ok(Route::Forward(node)) => Some((db.to_string(), node)),
                _ => None,
            },
            // Unknown databases are answered here (WRONGPASS).
            None => None,
        };
        match owner {
            Some((db, node)) => self.splice(stream, &node, Hop { product: PRODUCT.into(), tenant: db, client: peer, sni }, &first).await,
            None if first.is_empty() => redis_server::conn::serve_with(stream, peer, local, self.dir(), sni, fd).await,
            None => redis_server::conn::serve_with(Fixed(roll_edge::stream::Prefixed::new(first.freeze(), stream)), peer, local, self.dir(), sni, fd).await,
        }
    }

    async fn splice<S: Portable>(&self, stream: S, node: &Node, hop: Hop, first: &[u8]) {
        if let Err(e) = roll_edge::splice::splice(stream, &node.url, &self.signer, &hop, first).await {
            tracing::debug!(db = hop.tenant, node = node.id, error = %e, "redis: spliced connection ended");
        }
    }

    /// The owner's side of a splice: serve the stream as if the client had connected here.
    pub fn handler(self: &Arc<Self>) -> roll_edge::splice::Handler {
        let me = self.clone();
        Arc::new(move |io, hop| {
            let me = me.clone();
            Box::pin(async move { redis_server::conn::serve_with(Fixed(io), hop.client, me.public, me.dir(), hop.sni, -1).await })
        })
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

/// The REST data plane (`redis_server::http`, Upstash's protocol): a request runs on the owner
/// of the database its token (`<db>_<secret>`) names.
pub fn rest_router(control: Arc<Control>) -> axum::Router {
    redis_server::http::router(control.node.clone()).layer(axum::middleware::from_fn_with_state(control, rest_routes))
}

async fn rest_routes(State(control): State<Arc<Control>>, req: Request, next: Next) -> Response {
    let token = redis_server::http::token(req.headers(), req.uri()).unwrap_or_default();
    let auth = AuthRequest { user: None, password: token.as_bytes(), sni: None, client: None, command: b"auth" };
    let key = redis_server::node::route(&auth, None).and_then(|(db, _, _)| control.key_of(&db));
    dispatch(&control, key, req, next).await
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

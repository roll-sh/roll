//! roll nosql on many nodes (docs/nosql/architecture.md, "On a fleet"; docs/code/cluster.md for
//! the lease semantics): every database has one owner node at a time, holding the lease
//! `nosql:db:<id>`, and any node takes any request or connection and gets it there.
//!
//! - **Connections** ([`NosqlFleet`], the server's [`nosql_server::Fleet`]): a database is loaded
//!   only under a guard of its lease, and released when the guard's tenure ends (handoff, or the
//!   lease lost). A CQL connection reaching another node is spliced to the owner over roll-edge's
//!   signed hop (product `cql`; [`splice_handler`] is the owner's side).
//! - **HTTP**: `/v1/nosql/databases/{id}/…` runs on the database's owner ([`route`]), retried for
//!   about a lease while the database moves; Alternator requests (named by `Host` or by the SigV4
//!   access key `role@db`) too ([`alternator`]), forwarded to the owner's
//!   `/internal/nosql/alternator/…`.
//! - **Background work** runs on owners: a database's imports on its owner (or, when nobody owns
//!   it, on the node it belongs on, which takes it), expired branches deleted by their owner the
//!   same way, garbage collection on one node (`singleton:nosql-gc`). An owner gives an idle
//!   database's lease up ([`Cluster::release_if_idle`]) once it's unloaded and nothing runs for it.
//! - Storage fences what leases can't: a node that opens a database swaps its manifest, and the
//!   previous owner's writes fail from then on (`nosql_storage::manifest::Fenced`).

use crate::control::Control;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::future::BoxFuture;
use nosql_server::conn::{Accepted, Io};
use nosql_server::{Located, Node, Residence};
use roll_cluster::{Cluster, Guard, Ownership, Route};
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Lease keys of databases: `nosql:db:<id>`.
pub const KEY_PREFIX: &str = "nosql:db:";

pub fn key(id: &str) -> String {
    format!("{KEY_PREFIX}{id}")
}

/// What a spliced CQL connection tells the owner.
#[derive(Clone, Debug)]
pub struct HopTo {
    /// The owner's internal URL.
    pub node: String,
    pub db: String,
    pub client: SocketAddr,
    pub sni: Option<String>,
}

/// Opens a signed hop to a database's owner for a CQL client (roll-edge's splice in rolld).
pub type Connector = Arc<dyn Fn(HopTo) -> BoxFuture<'static, std::io::Result<Box<dyn Io>>> + Send + Sync>;

/// The cluster as the nosql server sees it.
pub struct NosqlFleet {
    cluster: Arc<Cluster>,
    connect: Connector,
}

impl NosqlFleet {
    pub fn new(cluster: Arc<Cluster>, connect: Connector) -> Arc<Self> {
        Arc::new(Self { cluster, connect })
    }
}

/// A guard of a database's lease, kept while it's loaded.
struct Held(Guard);

impl Residence for Held {
    fn ending(&self) -> BoxFuture<'static, ()> {
        Box::pin(self.0.ending())
    }
}

/// How long a request waits for a database that's moving: a lease, two sweeps and a margin.
fn ride_out(cluster: &Cluster) -> Duration {
    let c = cluster.config();
    c.lease_ttl + c.sweep_every * 2 + Duration::from_secs(1)
}

impl nosql_server::Fleet for NosqlFleet {
    fn locate<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Located> {
        Box::pin(async move {
            let key = key(id);
            let deadline = tokio::time::Instant::now() + ride_out(&self.cluster);
            loop {
                let wait = match self.cluster.route_async(&key).await {
                    Ok(Route::Local(g)) => return Located::Here(Box::new(Held(g))),
                    Ok(Route::Forward(n)) => return Located::Elsewhere(n.url),
                    Ok(Route::Unavailable { retry_after }) => retry_after,
                    Err(e) => {
                        tracing::warn!(db = id, error = %e, "nosql: routing a database failed");
                        Duration::from_secs(1)
                    }
                };
                if tokio::time::Instant::now() + wait >= deadline {
                    return Located::Unavailable;
                }
                tokio::time::sleep(wait.min(self.cluster.config().max_retry_wait).max(Duration::from_millis(20))).await;
            }
        })
    }

    fn hand_over(&self, node: String, id: String, mut client: Box<dyn Io>, peer: SocketAddr, sni: Option<String>, first: Bytes, skip: usize) -> BoxFuture<'static, ()> {
        let connect = self.connect.clone();
        Box::pin(async move {
            let mut owner = match connect(HopTo { node: node.clone(), db: id.clone(), client: peer, sni }).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::debug!(db = id, node, error = %e, "nosql: handing a connection to the owner failed");
                    return;
                }
            };
            if owner.write_all(&first).await.is_err() || owner.flush().await.is_err() {
                return;
            }
            if let Err(e) = nosql_server::fleet::skip_frames(&mut owner, &mut client, skip).await {
                tracing::debug!(db = id, error = %e, "nosql: handed-over connection ended early");
                return;
            }
            let _ = tokio::io::copy_bidirectional(&mut client, &mut owner).await;
        })
    }
}

/// The owner's side of a spliced CQL connection: served as if the client had connected here.
pub fn serve_spliced(node: Arc<Node>, io: Box<dyn Io>, client: SocketAddr, sni: Option<String>) -> BoxFuture<'static, ()> {
    let accepted = Accepted { peer: client, sni, shard_aware: false, tls: None, certificate: None };
    Box::pin(nosql_server::conn::serve(io, accepted, node))
}

impl Control {
    /// Take part in a cluster: databases are served by their owners from now on. Call once,
    /// before serving.
    pub fn join(self: &Arc<Self>, cluster: Arc<Cluster>, connect: Connector) {
        if self.cluster.set(cluster.clone()).is_err() {
            return;
        }
        self.node.set_fleet(NosqlFleet::new(cluster.clone(), connect));
        tokio::spawn(follow_ownership(Arc::downgrade(self), cluster.subscribe_ownership()));
        tokio::spawn(release_idle(Arc::downgrade(self)));
        tracing::info!(node = cluster.node_id(), backend = cluster.backend_name(), "roll nosql: databases are served by their owners");
    }

    /// The cluster this node serves databases with (none on a single node).
    pub fn cluster(&self) -> Option<&Arc<Cluster>> {
        self.cluster.get()
    }

    /// The name this node's work is recorded under (import claims).
    pub(crate) fn node_name(&self) -> String {
        match self.cluster() {
            Some(c) => c.node_id().to_string(),
            None => self.cfg.node_name.clone(),
        }
    }

    /// Whether work for database `id` belongs on this node: always on a single node; in a
    /// cluster when this node owns it, or nobody does and it belongs here (it's then taken).
    /// The guard is held while the work runs.
    pub(crate) async fn take_for_work(&self, id: &str) -> Option<Option<Guard>> {
        let Some(c) = self.cluster() else { return Some(None) };
        let key = key(id);
        if let Some(g) = c.guard(&key) {
            return Some(Some(g));
        }
        if c.preferred(&key).is_none_or(|n| n.id != c.node_id()) {
            return None;
        }
        match c.lease(&key).await {
            Ok(Some(g)) => Some(Some(g)),
            _ => None,
        }
    }

    /// Whether `node` (an import's claim) may be taken over by this node: it isn't live.
    pub(crate) fn node_is_gone(&self, node: &str) -> bool {
        self.cluster().is_none_or(|c| !c.nodes().iter().any(|n| n.id == node))
    }

    /// A cluster-wide singleton (always this node's on its own).
    pub(crate) async fn singleton(&self, name: &str) -> bool {
        let Some(c) = self.cluster().cloned() else { return true };
        let name = name.to_string();
        tokio::task::spawn_blocking(move || c.singleton(&name)).await.unwrap_or(false)
    }

    /// The database was deleted: give its lease up for good once this request is done.
    pub(crate) fn retire(&self, id: &str) {
        if let Some(c) = self.cluster() {
            c.retire(&key(id));
        }
    }
}

/// On `Lost`, a database still loaded here is released (its residence normally does it first);
/// on `Acquired`, its imports are resumed here.
async fn follow_ownership(control: Weak<Control>, mut rx: tokio::sync::mpsc::UnboundedReceiver<Ownership>) {
    while let Some(ev) = rx.recv().await {
        let Some(c) = control.upgrade() else { return };
        match ev {
            Ownership::Lost { key } => {
                if let Some(id) = key.strip_prefix(KEY_PREFIX)
                    && c.node.get_loaded(id).is_some()
                {
                    let (node, id) = (c.node.clone(), id.to_string());
                    tokio::spawn(async move { node.release(&id).await });
                }
            }
            Ownership::Acquired { key, .. } => {
                if let Some(id) = key.strip_prefix(KEY_PREFIX) {
                    c.resume_imports(id);
                }
            }
        }
    }
}

/// How long a database's lease stays with its owner after the database was last used and
/// unloaded.
const IDLE_RELEASE: Duration = Duration::from_secs(120);

/// Give up the leases of databases that are neither loaded here nor importing, so the lease
/// table stays the size of the databases in use. The next connection takes the lease again.
async fn release_idle(control: Weak<Control>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let Some(c) = control.upgrade() else { return };
        let Some(cluster) = c.cluster().cloned() else { return };
        for key in cluster.idle(IDLE_RELEASE).into_iter().filter(|k| k.starts_with(KEY_PREFIX)) {
            let (cl, me) = (cluster.clone(), c.clone());
            let _ = tokio::task::spawn_blocking(move || {
                let id = key[KEY_PREFIX.len()..].to_string();
                cl.release_if_idle(&key, IDLE_RELEASE, || me.node.get_loaded(&id).is_none() && !me.importing(&id))
            })
            .await;
        }
    }
}

/// The database a `/v1/nosql/databases/{id}/…` path names (an id or a name).
fn db_in_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v1/nosql/databases/")?;
    let id = rest.split('/').next()?;
    (!id.is_empty()).then_some(id)
}

/// Request bodies up to this size are kept so a request can be routed again.
const RETRY_BODY: usize = 4 << 20;

/// How long to wait before routing again, if `resp` says nothing ran.
fn unavailable_for(resp: &Response) -> Option<Duration> {
    if resp.status() != StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }
    let ms = resp.headers().get("x-roll-code-unavailable")?.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_millis(ms))
}

/// Middleware on `/v1/nosql` (in a cluster): requests about one database run on its owner.
/// Global routes (the list, creating, info) run where they arrive, as does a database that
/// doesn't exist (its 404).
pub async fn route(State(control): State<Arc<Control>>, req: Request, next: Next) -> Response {
    let Some(cluster) = control.cluster().cloned() else { return next.run(req).await };
    let Some(named) = db_in_path(req.uri().path()) else { return next.run(req).await };
    let Ok(d) = control.get(&urlencoding::decode(named).map(|s| s.into_owned()).unwrap_or_else(|_| named.to_string())) else { return next.run(req).await };
    dispatch(&cluster, &key(&d.id), req, move |req| next.run(req)).await
}

/// Run `req` on the owner of `key`, riding out moves for about a lease when its body is small
/// enough to send again. `local` runs it here.
async fn dispatch<F, Fut>(cluster: &Arc<Cluster>, key: &str, req: Request, local: F) -> Response
where
    F: FnOnce(Request) -> Fut + Clone,
    Fut: Future<Output = Response>,
{
    if let Some(from) = req.extensions().get::<roll_cluster::Forwarded>().map(|f| f.node.clone()) {
        // Never back to where it came from: the two nodes disagree for a moment.
        if let Ok(Route::Forward(n)) = cluster.route_async(key).await
            && n.id == from
        {
            cluster.forget(key);
            return roll_cluster::unavailable(Duration::from_millis(50));
        }
    }
    let run = |local: F| move |req: Request, guard: Guard| async move {
        let resp = local(req).await;
        drop(guard);
        resp
    };
    let small = http_body::Body::size_hint(req.body()).exact().is_some_and(|n| n <= RETRY_BODY as u64);
    if roll_cluster::hops(&req) > 0 || !small {
        return cluster.dispatch(key, req, run(local)).await;
    }
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, RETRY_BODY).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "error": format!("reading the request body: {e}") }))).into_response(),
    };
    let deadline = tokio::time::Instant::now() + ride_out(cluster);
    loop {
        let req = Request::from_parts(parts.clone(), Body::from(body.clone()));
        let resp = cluster.dispatch(key, req, run(local.clone())).await;
        let wait = match unavailable_for(&resp) {
            Some(w) => w,
            None if resp.status() == StatusCode::LOOP_DETECTED => {
                cluster.forget(key);
                Duration::from_millis(50)
            }
            None => return resp,
        };
        if tokio::time::Instant::now() + wait >= deadline {
            return resp;
        }
        tokio::time::sleep(wait.min(cluster.config().max_retry_wait).max(Duration::from_millis(20))).await;
    }
}

/// Where forwarded Alternator requests arrive on the owner.
pub const ALTERNATOR_INTERNAL: &str = "/internal/nosql/alternator";

/// Alternator (the DynamoDB API) for `control`'s databases: in a cluster, each request runs on
/// its database's owner (named by its SigV4 access key `role@db`, else its `Host`).
pub fn alternator(control: Arc<Control>, opts: nosql_server::alternator::AlternatorOptions) -> axum::Router {
    let base = nosql_server::alternator::router(control.node.clone(), opts);
    base.layer(axum::middleware::from_fn_with_state(control, route_alternator))
}

/// Forwarded Alternator requests, on the internal API (signed by the forwarding node).
pub fn internal_routes(control: Arc<Control>, opts: nosql_server::alternator::AlternatorOptions) -> axum::Router {
    let Some(cluster) = control.cluster().cloned() else { return axum::Router::new() };
    axum::Router::new()
        .nest(ALTERNATOR_INTERNAL, nosql_server::alternator::router(control.node.clone(), opts))
        .layer(axum::middleware::from_fn_with_state(cluster, roll_cluster::require_signed))
}

async fn route_alternator(State(control): State<Arc<Control>>, req: Request, next: Next) -> Response {
    let Some(cluster) = control.cluster().cloned() else { return next.run(req).await };
    let host = req.headers().get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string);
    let access_key = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|a| a.split("Credential=").nth(1))
        .and_then(|c| c.split('/').next())
        .map(str::to_string);
    let dir: &dyn nosql_server::Directory = control.node.as_ref();
    let Some(spec) = dir.route_alternator(host.as_deref(), access_key.as_deref()).await else { return next.run(req).await };
    let original = req.uri().clone();
    // Forwarded to the owner's internal route (the path comes back off there); run here as is.
    let (mut parts, body) = req.into_parts();
    // `/` is the nest's own path (`/internal/nosql/alternator`, no trailing slash).
    let path = Some(original.path()).filter(|p| *p != "/").unwrap_or("");
    let query = original.query().map(|q| format!("?{q}")).unwrap_or_default();
    parts.uri = format!("{ALTERNATOR_INTERNAL}{path}{query}").parse().unwrap_or(original.clone());
    parts.extensions.remove::<axum::extract::OriginalUri>();
    let req = Request::from_parts(parts, body);
    dispatch(&cluster, &key(&spec.id), req, move |mut req: Request| {
        *req.uri_mut() = original;
        next.run(req)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_paths() {
        assert_eq!(db_in_path("/v1/nosql/databases/n123/cql"), Some("n123"));
        assert_eq!(db_in_path("/v1/nosql/databases/app"), Some("app"));
        assert_eq!(db_in_path("/v1/nosql/databases"), None);
        assert_eq!(db_in_path("/v1/nosql/databases/"), None);
        assert_eq!(db_in_path("/v1/nosql"), None);
        assert_eq!(key("n1"), "nosql:db:n1");
    }
}

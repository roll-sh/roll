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
//!   own REST port.

use async_trait::async_trait;
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

/// Runs a `/v1/search/clusters/{id}/…` call on the cluster's owner (a cluster's settings,
/// credentials, indices and console reach it where it's loaded); other calls run here.
pub(crate) async fn on_owner(c: Arc<crate::Control>, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let Some(cluster) = c.cluster_leases().map(|l| l.cluster().clone()) else { return next.run(req).await };
    let id = req.uri().path().strip_prefix("/v1/search/clusters/").and_then(|r| r.split('/').next()).filter(|s| !s.is_empty()).map(str::to_string);
    let Some(id) = id.and_then(|id| urlencoding::decode(&id).ok().map(|s| s.into_owned())) else { return next.run(req).await };
    // An unknown cluster is answered here (404).
    let Ok(d) = c.get(&id).await else { return next.run(req).await };
    cluster
        .dispatch(&key(&d.name), req, |req, guard| async move {
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
/// checked as signed by a node of the cluster.
pub fn internal_router(control: Arc<crate::Control>, cluster: Arc<Cluster>) -> axum::Router {
    axum::Router::new()
        .nest_service(REST_FORWARDED, rest_router(control))
        .layer(axum::middleware::from_fn_with_state(cluster, roll_cluster::accept_forwarded))
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
    match search_server::http::serve_request(node, req).await {
        Ok(r) => r.map(axum::body::Body::new),
        // The client went away or sent a body that can't be read: nobody to answer.
        Err(_) => axum::http::StatusCode::BAD_REQUEST.into_response(),
    }
}

use axum::response::IntoResponse;

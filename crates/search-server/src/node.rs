//! A multi-tenant node: many clusters in one process, loaded on the first request and unloaded
//! when idle.
//!
//! - **Routing and auth** ([`crate::auth`]): every request names its cluster through its
//!   credentials or host name; the [`Catalog`] says whether it exists and which secrets it
//!   accepts. In dev mode ([`NodeConfig::default_cluster`]) everything goes to one cluster
//!   without authentication.
//! - **Wake on request.** A cluster that isn't loaded is opened by the first request;
//!   concurrent first requests share one open. With leases ([`Node::set_leases`]) the node
//!   first takes the cluster's lease and writes under its epoch; a cluster another node owns
//!   is refused with 503 and `X-Roll-Search-Owner`, which the front proxy follows. Then the
//!   metadata is read, and — when the [`StoreProvider`] keeps clusters in search-store — every
//!   shard's commit point, its segments (from the cache) and the WAL past it ([`persist`]).
//! - **Durability.** After each request the node makes its writes durable as the index's
//!   `index.translog.durability` asks before the response leaves; a failure turns the
//!   response into the error and, if memory may now be ahead of storage, unloads the cluster.
//! - **Limits.** Each cluster has a request rate, a budget of body bytes in flight, a body size
//!   limit and a cap on requests in flight with a queue of the same size behind it; beyond them
//!   requests get 429. Requests then take a node-wide running slot, granted round-robin across
//!   clusters ([`FairScheduler`]), so a noisy tenant can't starve quiet ones. Admission has two
//!   steps: [`Node::enter`] authenticates and pins the cluster (it can't be unloaded while a
//!   lease exists), then [`Lease::acquire`] takes the permits. The HTTP layer reads the body in
//!   between, so a slow client never holds a permit.
//! - **Residency protocol.** A request counts itself in `in_flight` and only then checks
//!   `closed`; unload sets `closed` and only then checks `in_flight`, both under the map lock
//!   for unload. With sequentially consistent atomics one side always sees the other: either
//!   unload backs off, or the request retries against a fresh load. No request ever runs on a
//!   cluster instance that has been unloaded (two live instances would overwrite each other).
//! - **Idle unload.** A cluster with no request for its idle timeout (its own
//!   [`ClusterSpec::idle_unload`], else the node's) is unloaded: its shards are
//!   committed to object storage and detached from the WAL, its lease released, its memory
//!   dropped and returned to the OS. A cluster holding what lives only in memory (an open
//!   scroll or point in time, until the last is closed or expires; a running task) isn't idle. Woken
//!   clusters keep their node id ([`stable_node_id`]) and `cluster_uuid`, so ids clients hold
//!   stay valid. Every background task (idle reaper, lease renewal, async
//!   WAL sync, flushes) exists only while a cluster that needs it is loaded, so a node with no
//!   loaded cluster runs no timer at all ([`Node::background_tasks`]). A cluster whose documents
//!   live only in memory (the [`StoreProvider`] has no [`StoreProvider::data`]) is never unloaded
//!   automatically while it holds any, unless [`NodeConfig::unload_memory_only`] says losing
//!   them is fine: a provider wrapper that forgot to forward `data()` keeps its tenants' data
//!   resident instead of silently dropping it.
//! - **Admission fast path.** A request for a loaded cluster takes one read lock of the cluster
//!   map, checks its credentials against the instance's shared description (no copy) and
//!   counts itself in flight; only a wake goes through the catalog and the transition lock.
//! - **Memory budgets** ([`Node::enforce_memory`], run by the reaper every 2 s and before every
//!   wake): a cluster past its own `max_resident_bytes` is committed (buffers become segment
//!   files); past the node's `memory_budget`, the least recently used clusters with nothing in
//!   flight are unloaded.
//! - **Metering** ([`Node::take_request_counts`]): requests admitted per cluster since the last
//!   call, counted per instance and carried over when an instance unloads.
//! - **Live spec changes** ([`Node::update_spec`]): limits and credentials apply to the loaded
//!   instance at once, and to one still waking (the wake re-reads the catalog before it
//!   publishes, and updates made after that wait in `pending_specs` for the publishing opener).
//! - **Shutdown** ([`Node::shutdown`]): refuses new loads, unloads every cluster (flushing and
//!   handing the leases back, so another node can take over at once) and closes the WAL.

use crate::auth::{self, Presented};
use crate::catalog::{Catalog, ClusterSpec, StoreProvider};
use crate::lease::Leases;
use crate::limits::{ByteBudget, FairPermit, FairScheduler, RateLimiter, Reservation};
use crate::persist::{self, ClusterData, TaskCount, TaskGuard};
use async_trait::async_trait;
use parking_lot::Mutex;
use search_es::{Cluster, ClusterConfig, Dialect, EsError, EsRequest, EsResponse};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, Semaphore};

/// `http.max_content_length` default.
pub const DEFAULT_MAX_BODY: usize = 100 * 1024 * 1024;
/// Default lease time to live.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(10);
/// Memory set aside for a cluster being woken, when the node has a memory budget.
const WAKE_ROOM: u64 = 1 << 20;
/// How often memory budgets are checked while clusters are loaded.
const MEMORY_CHECK: Duration = Duration::from_secs(2);
/// Response header naming the node that owns a cluster this node refused.
pub const OWNER_HEADER: &str = "X-Roll-Search-Owner";

#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// The name the node reports (`GET /` `name`).
    pub node_name: String,
    /// The node's identity for ownership leases (default: `node_name`); unique per node.
    pub node_id: Option<String>,
    /// DNS suffix of cluster host names, for SNI/`Host` routing (`search.example.com`).
    pub domain: Option<String>,
    /// Unload clusters idle this long (`None`: never).
    pub idle_unload: Option<Duration>,
    /// `http.max_content_length`.
    pub max_body_bytes: usize,
    /// Dev mode: serve this cluster to every request, without authentication.
    pub default_cluster: Option<String>,
    /// Node settings every cluster reports (Elasticsearch's `-E`): `node.attr.*`,
    /// `node.processors`.
    pub node_settings: Vec<(String, String)>,
    /// Publish Elasticsearch's test features (for the YAML REST suites).
    pub test_features: bool,
    /// Let node stats report the host's real numbers. Only for nodes serving a single tenant
    /// (the dev server): on shared nodes they would leak other tenants' load and the host's
    /// disks, so clusters report synthetic figures.
    pub host_metrics: bool,
    /// Requests running at once on the node, granted fairly across clusters (0: twice the
    /// cores; `usize::MAX`: no node-wide cap).
    pub max_running: usize,
    /// Lease time to live, with [`Node::set_leases`].
    pub lease_ttl: Duration,
    /// Memory all loaded clusters together may hold (indexing buffers, in-heap segments,
    /// version maps; [`Usage::resident_bytes`]). Past it, the least recently used clusters with
    /// no request in flight are unloaded, and waking another cluster first makes room.
    /// `None`: no node budget.
    pub memory_budget: Option<u64>,
    /// Let idle and memory-budget unloads drop clusters whose documents live only in memory
    /// (a [`StoreProvider`] without [`StoreProvider::data`]): they lose every document and keep
    /// only their metadata. For tests and throwaway nodes. Off: such a cluster stays loaded
    /// while it holds documents ([`Node::unload`] still unloads it when asked to).
    pub unload_memory_only: bool,
    /// How old a durable snapshot copy no recorded snapshot uses (a crash between capturing
    /// and recording a snapshot) must be before the sweep after a wake drops it
    /// ([`search_es::api::snapshots::sweep_leftover_copies`]).
    pub snapshot_leftover_grace: Duration,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            node_name: "roll-search".into(),
            node_id: None,
            domain: None,
            idle_unload: Some(Duration::from_secs(300)),
            max_body_bytes: DEFAULT_MAX_BODY,
            default_cluster: None,
            node_settings: Vec::new(),
            test_features: false,
            host_metrics: false,
            max_running: 0,
            lease_ttl: DEFAULT_LEASE_TTL,
            memory_budget: None,
            unload_memory_only: false,
            snapshot_leftover_grace: search_es::api::snapshots::LEFTOVER_GRACE,
        }
    }
}

/// Engine hooks around a cluster's residency (warm-up on wake, metering).
#[async_trait]
pub trait LifecycleHooks: Send + Sync + 'static {
    async fn loaded(&self, _cluster: &Arc<Cluster>) {}
    async fn unloading(&self, _cluster: &Arc<Cluster>) {}
}

struct NoHooks;
impl LifecycleHooks for NoHooks {}

/// The lease a loaded cluster is held under.
#[derive(Clone, Copy, Debug)]
struct HeldLease {
    epoch: u64,
}

/// A loaded cluster.
pub struct Loaded {
    pub cluster: Arc<Cluster>,
    /// Durable shards (`None`: the cluster lives in memory).
    pub data: Option<Arc<ClusterData>>,
    /// The cluster's metadata store (closed when it unloads).
    meta: Arc<crate::meta_guard::CheckedMeta>,
    /// The catalog's description, refreshed with [`Node::update_spec`] (credential rotation).
    spec: parking_lot::RwLock<Arc<ClusterSpec>>,
    /// Milliseconds since the node started, at the last request.
    last_used: AtomicU64,
    in_flight: AtomicUsize,
    waiting: AtomicUsize,
    permits: Arc<Semaphore>,
    /// Permits to retire as they come back (`max_in_flight` lowered while they were in use).
    permit_debt: AtomicUsize,
    closed: AtomicBool,
    rate: Mutex<Option<Arc<RateLimiter>>>,
    bytes: Arc<ByteBudget>,
    requests: AtomicU64,
    /// `requests` as last reported by [`Node::take_request_counts`].
    metered: AtomicU64,
    lease: Option<HeldLease>,
    name: Arc<str>,
    /// The node's body limit, for specs without their own.
    default_max_body: usize,
    /// An automatic unload was refused because its documents live only in memory (logged once).
    kept_volatile: AtomicBool,
    /// How long waking took.
    pub wake_time: Duration,
    pub recovery: persist::RecoveryStats,
}

impl Loaded {
    pub fn spec(&self) -> Arc<ClusterSpec> {
        self.spec.read().clone()
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Requests served since the cluster loaded.
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Requests not reported yet (each is reported once, by whoever swaps `metered` past it).
    fn unmetered(&self) -> u64 {
        let now = self.requests.load(Ordering::SeqCst);
        now.saturating_sub(self.metered.swap(now, Ordering::SeqCst))
    }

    /// The lease epoch the cluster is written under (0: no leases).
    pub fn epoch(&self) -> u64 {
        self.lease.map_or(0, |l| l.epoch)
    }

    /// Request body bytes in flight.
    pub fn bytes_in_flight(&self) -> usize {
        self.bytes.used()
    }

    /// Whether state that lives only in memory still needs the cluster loaded: an open scroll
    /// or point in time not yet expired, or a running task.
    fn pinned(&self) -> bool {
        // Open contexts' searchers live only in memory: pinned exactly until the last one is
        // closed or expires.
        // Running OpenSearch jobs (ISM steps) write through the cluster like requests do.
        self.cluster.contexts.latest_expiry().is_some() || !self.cluster.tasks.is_empty() || self.cluster.jobs.busy()
    }
}

type Slot = Arc<OnceCell<Arc<Loaded>>>;

/// What a cluster costs (metering, the dashboard).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub loaded: bool,
    /// Live documents (Lucene-level, nested included) and deleted ones.
    pub docs: u64,
    pub deleted_docs: u64,
    /// Segment bytes (stored).
    pub store_bytes: u64,
    /// Memory held while loaded (indexing buffers, in-heap segments, version maps).
    pub resident_bytes: u64,
    pub requests: u64,
}

pub struct Node {
    pub catalog: Arc<dyn Catalog>,
    pub stores: Arc<dyn StoreProvider>,
    pub cfg: NodeConfig,
    hooks: Arc<dyn LifecycleHooks>,
    /// Read on every request, written only by wakes and unloads.
    clusters: parking_lot::RwLock<HashMap<String, Slot>>,
    reaper: AtomicBool,
    /// A loaded cluster has an idle timeout or memory budget of its own: the reaper must run
    /// even when the node has neither. Cleared when the reaper stops (nothing loaded).
    reaper_wanted: AtomicBool,
    /// Notified on every unload: lets the reaper and renewer exit as soon as nothing is loaded.
    residency: Arc<tokio::sync::Notify>,
    renewer: AtomicBool,
    epoch: Instant,
    /// Serializes loads and unloads of the same cluster. Entries live only while someone
    /// holds or waits for them ([`Transition`] removes the last one).
    transitions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    fair: Option<Arc<FairScheduler>>,
    leases: OnceLock<Arc<dyn Leases>>,
    /// Spec updates that arrived while their cluster was loading ([`Node::update_spec`]).
    pending_specs: Mutex<HashMap<String, ClusterSpec>>,
    /// Requests of unloaded instances not reported yet ([`Node::take_request_counts`]).
    unmetered: Mutex<HashMap<String, u64>>,
    /// Makes a running reaper re-plan its tick (a cluster with a memory budget loaded).
    reaper_kick: Arc<tokio::sync::Notify>,
    tasks: Arc<TaskCount>,
    shutting_down: AtomicBool,
    /// Wakes and unloads since start.
    pub wakes: AtomicU64,
    pub unloads: AtomicU64,
}

/// Holds a cluster's transition lock; drops the map entry when nobody else wants it.
struct Transition<'a> {
    node: &'a Node,
    name: &'a str,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for Transition<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut map = self.node.transitions.lock();
        // Only the map's own reference left: no holder, no waiter (waiters clone it under this lock).
        if map.get(self.name).is_some_and(|m| Arc::strong_count(m) == 1) {
            map.remove(self.name);
        }
    }
}

/// A request admitted to a cluster: holds its place in the in-flight count (which keeps the
/// cluster loaded) until dropped, and the permits once [`Lease::acquire`]d.
pub struct Lease {
    pub loaded: Arc<Loaded>,
    node: Arc<Node>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    /// The node-wide running slot; given back as soon as the cluster has answered (waiting
    /// for durability needs no CPU).
    fair: Mutex<Option<FairPermit>>,
    /// The request's thread pool slot (or refusal), taken before the running slot so a request
    /// queued for its pool doesn't hold one; handed to search-es with the request.
    admitted: Mutex<Option<search_es::rest::Admitted>>,
    bytes: Reservation,
    dialect: Dialect,
    /// Who the node authenticated with the cluster's own credentials (the owner, `elastic`, or
    /// a credential's user); search-es trusts it ([`search_es::xpack::security::AUTHENTICATED_HEADER`]).
    user: Option<Arc<str>>,
    /// Credentials the cluster's own security verified instead (a native user, API key or
    /// access token, [`auth::native_authorization`]): the `Authorization` header the request
    /// runs with, so search-es authenticates and authorizes it as that principal.
    native_auth: Option<String>,
}

/// Counts a request in the cluster's queue for as long as it waits, however the wait ends
/// (granted, closed, or the request future dropped because the client went away).
struct Queued<'a>(&'a AtomicUsize);

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(p) = self.permit.take() {
            // `max_in_flight` went down while this permit was out: retire it instead.
            let debt = &self.loaded.permit_debt;
            if debt.load(Ordering::SeqCst) > 0 && debt.try_update(Ordering::SeqCst, Ordering::SeqCst, |d| d.checked_sub(1)).is_ok() {
                p.forget();
            }
        }
        self.loaded.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Lease {
    pub fn max_body_bytes(&self, node: &Node) -> usize {
        self.loaded.spec().max_body_bytes.unwrap_or(node.cfg.max_body_bytes)
    }

    /// Reserves `bytes` of the cluster's in-flight body budget (from `Content-Length`, before
    /// the body is read); 429 like Elasticsearch's indexing pressure past it.
    pub fn reserve_body(&mut self, req: &EsRequest, bytes: usize) -> Result<(), Rejected> {
        self.reserve_more(req, bytes)
    }

    /// Adds `bytes` to the request's reservation (a chunked body's next frame, or a compressed
    /// body's output as it inflates); 429 past the budget, as [`Lease::reserve_body`].
    pub fn reserve_more(&mut self, req: &EsRequest, bytes: usize) -> Result<(), Rejected> {
        match self.loaded.bytes.grow(&mut self.bytes, bytes) {
            Ok(()) => Ok(()),
            Err(used) => Err(self.over_budget(req, used, self.bytes.bytes() + bytes)),
        }
    }

    /// The 429 for a body past the cluster's in-flight byte budget.
    pub fn over_budget(&self, req: &EsRequest, used: usize, bytes: usize) -> Rejected {
        Rejected {
            error: EsError::too_many_requests(format!(
                "rejected execution of coordinating operation [coordinating_and_primary_bytes={used}, replica_bytes=0, all_bytes={used}, coordinating_operation_bytes={bytes}, max_coordinating_and_primary_bytes={}] for REST request [{} {}]",
                self.loaded.bytes.limit(),
                req.method,
                req.uri
            )),
            dialect: self.dialect,
        }
    }

    /// Moves the body reservation out (to grow it off the async task, e.g. while inflating
    /// on a blocking thread), with the budget it belongs to; give it back with
    /// [`Lease::put_reservation`].
    pub(crate) fn take_reservation(&mut self) -> (Reservation, Arc<ByteBudget>) {
        (std::mem::replace(&mut self.bytes, Reservation::none()), self.loaded.bytes.clone())
    }

    pub(crate) fn put_reservation(&mut self, r: Reservation) {
        self.bytes = r;
    }

    /// Runs the request on its cluster and makes its writes durable before answering.
    pub async fn handle(&self, mut req: EsRequest) -> EsResponse {
        use search_es::xpack::security::AUTHENTICATED_HEADER;
        // Only the node may say who authenticated.
        req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(AUTHENTICATED_HEADER));
        if let Some(u) = &self.user {
            req.headers.push((AUTHENTICATED_HEADER.to_string(), u.to_string()));
        } else if let Some(h) = &self.native_auth {
            req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
            req.headers.push(("authorization".to_string(), h.clone()));
        }
        self.run(req).await
    }

    /// Writes the async results a request changed to `.async-search` (a durable cluster): they
    /// must outlive an unload, which an Elasticsearch node never does. One atomic load when
    /// nothing changed. A failure is logged and retried with the next request or the unload.
    async fn sync_async_results(&self) {
        let cluster = &self.loaded.cluster;
        if cluster.xpack.results.has_changes()
            && let Err(e) = cluster.sync_async_results().await
        {
            tracing::warn!(cluster = &*self.loaded.name, error = %e.reason(), "storing async results failed");
        }
    }

    async fn run(&self, req: EsRequest) -> EsResponse {
        let loaded = &self.loaded;
        loaded.requests.fetch_add(1, Ordering::Relaxed);
        let admitted = self.admitted.lock().take();
        let Some(data) = &loaded.data else {
            return search_es::rest::handle_shared_admitted(&loaded.cluster, req, admitted).await;
        };
        if let Err(e) = data.ready(&loaded.cluster).await {
            // A shard whose WAL didn't replay must not be read or written yet.
            tracing::warn!(cluster = &*loaded.name, error = ?e, "recovering shards failed");
            return search_es::rest::error_response(&e.to_es(&loaded.name), &req, loaded.spec().dialect);
        }
        let flush = is_flush(&req);
        // Reads can't write (background tasks are the syncer's): nothing to make durable, and
        // a search never waits for the fsync of someone else's write.
        if !flush && (matches!(req.method.as_str(), "GET" | "HEAD") || is_read_only_post(&req)) {
            // Only reads of async results can change them; only those keep what an error
            // response needs (no copy on the search path).
            let err_req = reads_async_results(&req.uri)
                .then(|| EsRequest { method: req.method.clone(), uri: req.uri.clone(), headers: req.headers.clone(), body: bytes::Bytes::new() });
            let resp = search_es::rest::handle_shared_admitted(&loaded.cluster, req, admitted).await;
            // A read that kept an async result longer (`keep_alive`) or opened a SQL cursor:
            // Elasticsearch answers once the new expiration is stored, so the read is
            // acknowledged like a write, and fails like one when that can't be made durable.
            if loaded.cluster.xpack.results.has_changes() {
                drop(self.fair.lock().take());
                let result = match loaded.cluster.sync_async_results().await {
                    Ok(()) => data.after_request(&loaded.cluster).await.map_err(|e| e.to_es(&loaded.name)),
                    Err(e) => Err(e),
                };
                if data.failed().is_some() {
                    self.node.spawn_force_unload(loaded);
                }
                if let Err(e) = result {
                    tracing::warn!(cluster = &*loaded.name, error = %e.reason(), "storing async results not acknowledged");
                    // A change of background work (an async search finishing) is retried by
                    // the next request or the unload; it doesn't fail an unrelated read.
                    if resp.status < 300
                        && let Some(r) = &err_req
                    {
                        return search_es::rest::error_response(&e, r, loaded.spec().dialect);
                    }
                }
            }
            return resp;
        }
        let err_req = EsRequest { method: req.method.clone(), uri: req.uri.clone(), headers: req.headers.clone(), body: bytes::Bytes::new() };
        let resp = search_es::rest::handle_shared_admitted(&loaded.cluster, req, admitted).await;
        drop(self.fair.lock().take());
        // Async results the request stored go to `.async-search`, acknowledged with it.
        self.sync_async_results().await;
        // Deployment assignments it started or stopped go to the cluster state (durable once
        // saved), as on Elasticsearch.
        let assignments = loaded.cluster.sync_ml_deployments(false).await;
        let mut result = data.after_request(&loaded.cluster).await;
        if result.is_ok() && flush && resp.status < 300 {
            // Only the shards this `_flush` targeted (search-es asked for them).
            result = data.commit_flushed(&loaded.cluster).await.map_err(|e| data.classify(e));
        }
        if data.failed().is_some() {
            self.node.spawn_force_unload(loaded);
        }
        match (result, assignments) {
            (Ok(()), Ok(())) => resp,
            (Ok(()), Err(e)) => {
                tracing::warn!(cluster = &*loaded.name, error = %e.reason(), "storing deployment assignments failed");
                if resp.status < 300 {
                    search_es::rest::error_response(&e, &err_req, loaded.spec().dialect)
                } else {
                    resp
                }
            }
            (Err(e), _) => {
                tracing::warn!(cluster = &*loaded.name, error = ?e, "write not acknowledged");
                search_es::rest::error_response(&e.to_es(&loaded.name), &err_req, loaded.spec().dialect)
            }
        }
    }

    /// Whether the concurrency permit is held.
    pub fn has_permit(&self) -> bool {
        self.permit.is_some()
    }

    /// Takes a concurrency permit, waiting in the cluster's queue if all are in use (429 when
    /// the queue is full too), then the slot of the thread pool the request runs on (queued
    /// there FIFO; a refusal is answered once the request is authenticated), then a node-wide
    /// running slot. Idempotent.
    pub async fn acquire(&mut self, req: &EsRequest) -> Result<(), Rejected> {
        if self.permit.is_some() {
            return Ok(());
        }
        let loaded = self.loaded.clone();
        let sem = loaded.permits.clone();
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let max = loaded.spec().max_in_flight.max(1);
                let queued = Queued(&loaded.waiting);
                if loaded.waiting.fetch_add(1, Ordering::SeqCst) >= max {
                    drop(queued);
                    return Err(Rejected {
                        error: EsError::too_many_requests(format!(
                            "rejected execution of REST request [{} {}]: cluster [{}] has [{max}] requests in flight and [{max}] queued",
                            req.method,
                            req.uri,
                            loaded.spec().name
                        )),
                        dialect: self.dialect,
                    });
                }
                let p = sem.acquire_owned().await;
                drop(queued);
                p.map_err(|_| Rejected { error: EsError::internal("cluster closed"), dialect: self.dialect })?
            }
        };
        if let Some(pool) = search_es::thread_pool::pool_for(&loaded.cluster, &req.method, &req.uri) {
            let admitted = pool.admit().await;
            *self.admitted.lock() = Some(admitted);
        }
        if let Some(f) = &self.node.fair {
            let p = f.acquire(&loaded.name).await;
            *self.fair.lock() = Some(p);
        }
        self.permit = Some(permit);
        Ok(())
    }
}

/// A cluster's node id: stable across wakes (Elasticsearch keeps it in the data path), so ids
/// clients hold (task ids, `_nodes/<id>`) stay valid; derived from the cluster name, as tenants
/// see one node whichever host serves them.
pub fn stable_node_id(cluster: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    let h = sha2::Sha256::new().chain_update(b"roll-search-node\0").chain_update(cluster.as_bytes()).finalize();
    let mut b: [u8; 16] = h[..16].try_into().expect("32-byte digest");
    // A version-4 UUID's layout, like Elasticsearch's `UUIDs.randomBase64UUID`.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn is_flush(req: &EsRequest) -> bool {
    let path = req.uri.split('?').next().unwrap_or("");
    let path = path.trim_end_matches('/');
    matches!(req.method.as_str(), "POST" | "GET") && (path == "/_flush" || path.ends_with("/_flush"))
}

/// A `POST` that only reads (searches with a body): `_search`, `_msearch`, `_count`, `_mget`,
/// `_field_caps`, `_termvectors`, `_mtermvectors`, `_explain/<id>`, `_validate/query`,
/// `_search/scroll`, `_search/template`, `_msearch/template`, `_rank_eval`. None of them logs an
/// operation or changes the cluster state, so none needs an acknowledgement from the WAL.
fn is_read_only_post(req: &EsRequest) -> bool {
    if req.method != "POST" {
        return false;
    }
    let path = req.uri.split('?').next().unwrap_or("").trim_end_matches('/');
    // `POST /i/_doc/_search` indexes a document with the id `_search`.
    if path.split('/').any(|s| matches!(s, "_doc" | "_create" | "_update" | "_bulk")) {
        return false;
    }
    let mut segs = path.rsplit('/');
    let last = segs.next().unwrap_or("");
    let prev = segs.next().unwrap_or("");
    matches!(last, "_search" | "_msearch" | "_count" | "_mget" | "_field_caps" | "_termvectors" | "_mtermvectors" | "_rank_eval")
        || matches!((prev, last), ("_search", "scroll" | "template") | ("_msearch", "template") | ("_validate", "query"))
        || matches!(prev, "_explain" | "_termvectors")
}

/// Whether a read may change stored async results: async search, EQL, ES|QL and SQL reads
/// (a `keep_alive` extension, a new SQL cursor).
fn reads_async_results(uri: &str) -> bool {
    let path = uri.split('?').next().unwrap_or("");
    path.split('/').any(|s| matches!(s, "_async_search" | "_eql" | "_query" | "_sql"))
}

/// Why a request was turned away before reaching its cluster.
#[derive(Debug)]
pub struct Rejected {
    pub error: EsError,
    pub dialect: Dialect,
}

const WWW_AUTHENTICATE: [&str; 3] = ["Basic realm=\"security\", charset=\"UTF-8\"", "Bearer realm=\"security\"", "ApiKey"];

/// How often a held lease is renewed. Acknowledgements stop `0.8 × ttl` after a renewal was
/// sent; a cluster that joins the renewer between two of its rounds waits up to one period
/// for its first, so renewing every `ttl / 6` leaves a renewal `0.47 × ttl` to complete even
/// then (a slow store, a loaded machine).
pub fn lease_renew_period(ttl: Duration) -> Duration {
    ttl / 6
}

/// The deadline a renewal sent at `sent` gives acknowledgements.
fn lease_deadline(sent: Instant, ttl: Duration) -> Instant {
    sent + ttl.mul_f64(0.8)
}

/// Renews `cluster`'s lease once, moving `deadline` on; an error once the lease is lost (a
/// failing store is only logged: the deadline then runs out on its own).
async fn renew_lease(leases: &dyn Leases, cluster: &str, node: &str, epoch: u64, ttl: Duration, deadline: &Mutex<Option<Instant>>) -> Result<(), anyhow::Error> {
    let sent = Instant::now();
    match leases.renew(cluster, node, epoch, ttl).await {
        Ok(true) => {
            *deadline.lock() = Some(lease_deadline(sent, ttl));
            Ok(())
        }
        Ok(false) => {
            let e = EsError::new(503, "cluster_block_exception", format!("cluster [{cluster}] lost its lease while waking up"));
            Err(anyhow::Error::new(OpenError { error: e, source: anyhow::anyhow!("cluster [{cluster}]: lease (epoch {epoch}) lost while loading") }))
        }
        Err(e) => {
            tracing::warn!(cluster, error = %e, "renewing the lease of a cluster waking up failed");
            Ok(())
        }
    }
}

/// Keeps `cluster`'s lease renewed while a wake runs (every [`lease_renew_period`] after the
/// last renewal sent), recording each new deadline in `deadline`. Returns only once the lease
/// is lost.
async fn keep_lease(leases: &dyn Leases, cluster: &str, node: &str, epoch: u64, ttl: Duration, deadline: &Mutex<Option<Instant>>) -> anyhow::Error {
    loop {
        let last_sent = deadline.lock().map_or_else(Instant::now, |d| d - ttl.mul_f64(0.8));
        tokio::time::sleep_until((last_sent + lease_renew_period(ttl)).into()).await;
        if let Err(e) = renew_lease(leases, cluster, node, epoch, ttl, deadline).await {
            return e;
        }
    }
}

/// The user a verified claim authenticates: its credential's user, else the cluster's owner.
fn authenticated_user(c: &auth::Claim) -> Arc<str> {
    Arc::from(c.user.as_deref().unwrap_or("elastic"))
}

fn unauthorized(reason: String) -> EsError {
    EsError::security(401, reason).with_header_values("WWW-Authenticate", WWW_AUTHENTICATE.iter().map(|s| s.to_string()).collect())
}

const ANONYMOUS_REFUSED: &str = "unable to authenticate with provided credentials and anonymous access is not allowed for this request";

/// Elasticsearch's 401 for a malformed `Authorization` header.
fn malformed(m: auth::Malformed) -> EsError {
    match m {
        auth::Malformed::BasicEncoding(why) => unauthorized("invalid basic authentication header encoding".into()).caused_by(EsError::illegal_argument(why)),
        auth::Malformed::BasicValue => unauthorized("invalid basic authentication header value".into()),
        auth::Malformed::ApiKey(why) => unauthorized(ANONYMOUS_REFUSED.into()).with_meta("additional_unsuccessful_credentials", format!("API key: {why}")),
    }
}

/// A failed wake, with what the client should see.
#[derive(Debug)]
pub struct OpenError {
    pub error: EsError,
    pub source: anyhow::Error,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.source)
    }
}

impl std::error::Error for OpenError {}

/// Hands freed memory back to the OS (when mimalloc is the allocator, as in rolld and
/// search-dev; harmless otherwise).
fn release_memory() {
    unsafe { libmimalloc_sys::mi_collect(true) };
}

impl Node {
    pub fn new(catalog: Arc<dyn Catalog>, stores: Arc<dyn StoreProvider>, cfg: NodeConfig) -> Arc<Node> {
        Node::with_hooks(catalog, stores, cfg, Arc::new(NoHooks))
    }

    pub fn with_hooks(catalog: Arc<dyn Catalog>, stores: Arc<dyn StoreProvider>, cfg: NodeConfig, hooks: Arc<dyn LifecycleHooks>) -> Arc<Node> {
        if stores.data().is_none() && !cfg.unload_memory_only && (cfg.idle_unload.is_some() || cfg.memory_budget.is_some()) {
            tracing::warn!(
                "the store provider keeps documents only in memory (StoreProvider::data is None): clusters holding documents will not be unloaded when idle or over the memory budget"
            );
        }
        let running = match cfg.max_running {
            0 => std::thread::available_parallelism().map_or(2, |n| n.get() * 2),
            n => n,
        };
        Arc::new(Node {
            catalog,
            stores,
            fair: (running != usize::MAX).then(|| FairScheduler::new(running)),
            cfg,
            hooks,
            clusters: parking_lot::RwLock::new(HashMap::new()),
            reaper: AtomicBool::new(false),
            reaper_wanted: AtomicBool::new(false),
            residency: Arc::new(tokio::sync::Notify::new()),
            renewer: AtomicBool::new(false),
            epoch: Instant::now(),
            transitions: Mutex::new(HashMap::new()),
            leases: OnceLock::new(),
            pending_specs: Mutex::new(HashMap::new()),
            unmetered: Mutex::new(HashMap::new()),
            reaper_kick: Arc::new(tokio::sync::Notify::new()),
            tasks: Arc::new(TaskCount::default()),
            shutting_down: AtomicBool::new(false),
            wakes: AtomicU64::new(0),
            unloads: AtomicU64::new(0),
        })
    }

    /// Clusters are owned through `leases` (multi-node). Set before the first request.
    pub fn set_leases(&self, leases: Arc<dyn Leases>) {
        let _ = self.leases.set(leases);
    }

    /// The node's identity in leases.
    pub fn node_id(&self) -> &str {
        self.cfg.node_id.as_deref().unwrap_or(&self.cfg.node_name)
    }

    /// The node-wide fair scheduler, if any.
    pub fn scheduler(&self) -> Option<&Arc<FairScheduler>> {
        self.fair.as_ref()
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    async fn transition<'a>(&'a self, name: &'a str) -> Transition<'a> {
        let m = self.transitions.lock().entry(name.to_string()).or_default().clone();
        let mut t = Transition { node: self, name, guard: None };
        t.guard = Some(m.lock_owned().await);
        t
    }

    /// Number of transition locks held or awaited (they are dropped when free).
    pub fn transition_locks(&self) -> usize {
        self.transitions.lock().len()
    }

    /// Background tasks running for the node's clusters (idle reaper, lease renewal, WAL
    /// syncers, flushes). Zero whenever no cluster is loaded.
    pub fn background_tasks(&self) -> usize {
        self.tasks.get()
    }

    /// Names of the loaded clusters.
    pub fn loaded(&self) -> Vec<String> {
        let map = self.clusters.read();
        map.iter().filter(|(_, s)| s.initialized()).map(|(k, _)| k.clone()).collect()
    }

    /// Cancels every cancellable task of every loaded cluster (running searches, by-query
    /// runs): what a stopping node does with work it won't wait for. Returns how many it
    /// cancelled.
    pub fn cancel_tasks(&self) -> usize {
        self.clusters.read().values().filter_map(|s| s.get()).map(|l| l.cluster.tasks.cancel_all()).sum()
    }

    pub fn get_loaded(&self, name: &str) -> Option<Arc<Loaded>> {
        self.clusters.read().get(name).and_then(|s| s.get().cloned())
    }

    /// Loads (or returns the loaded) cluster described by `spec`.
    pub async fn open(self: &Arc<Self>, spec: ClusterSpec) -> anyhow::Result<Arc<Loaded>> {
        let name = spec.name.clone();
        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                anyhow::bail!(OpenError { error: EsError::new(503, "node_closed_exception", "node is shutting down"), source: anyhow::anyhow!("node is shutting down") });
            }
            let existing = self.clusters.read().get(&name).cloned();
            let slot = match existing {
                Some(s) => s,
                None => self.clusters.write().entry(name.clone()).or_default().clone(),
            };
            let node = self.clone();
            let spec2 = spec.clone();
            let mut initialized_here = false;
            let loaded = slot
                .get_or_try_init(|| async {
                    initialized_here = true;
                    let cluster_name = spec2.name.clone();
                    let _t = node.transition(&cluster_name).await;
                    node.load(spec2).await
                })
                .await;
            let loaded = match loaded {
                Ok(l) => l.clone(),
                Err(e) => {
                    // Don't leave an empty slot behind, unless another opener is still on it.
                    let mut map = self.clusters.write();
                    if map.get(&name).is_some_and(|s| Arc::ptr_eq(s, &slot) && !s.initialized() && Arc::strong_count(s) == 2) {
                        map.remove(&name);
                        self.pending_specs.lock().remove(&name);
                    }
                    return Err(e);
                }
            };
            if initialized_here {
                // Only the opener that published the instance: updates that saw the slot
                // still loading are in `pending_specs` by now (both sides hold the map lock).
                self.apply_pending_spec(&loaded);
                let spec = loaded.spec();
                if spec.max_resident_bytes.is_some() || spec.idle_unload.is_some() {
                    // After publishing: a reaper stopping meanwhile saw the slot loaded or
                    // cleared this before it is set again.
                    self.reaper_wanted.store(true, Ordering::SeqCst);
                    // A reaper on the long idle tick re-plans for this budget or timeout.
                    self.reaper_kick.notify_waiters();
                }
            }
            if loaded.closed.load(Ordering::SeqCst) {
                // Being unloaded: `unload` flips `closed` and removes the slot in one critical
                // section of the map lock (or backs off and clears it), so waiting for the lock
                // and looking again finds either a fresh slot or this one reopened.
                drop(self.clusters.read());
                tokio::task::yield_now().await;
                continue;
            }
            if initialized_here {
                // Pairs with the reaper's and renewer's stop decisions (taken under the write lock).
                drop(self.clusters.read());
            }
            self.ensure_reaper();
            if loaded.lease.is_some() {
                self.ensure_renewer();
            }
            return Ok(loaded);
        }
    }

    /// Wakes a cluster (under its transition lock).
    async fn load(self: &Arc<Self>, spec: ClusterSpec) -> anyhow::Result<Arc<Loaded>> {
        let started = Instant::now();
        if self.cfg.memory_budget.is_some() {
            // Room for the newcomer: least recently used clusters go first.
            // (No per-cluster commits here: they'd delay the wake; the reaper does them.)
            self.enforce_memory_with(WAKE_ROOM, false).await;
        }
        let name = spec.name.clone();
        let data_store = self.stores.data();
        // Ownership first: everything below writes under the lease's epoch.
        let mut held = None;
        let mut deadline = None;
        if let Some(leases) = self.leases.get() {
            let ttl = self.cfg.lease_ttl;
            let sent = Instant::now();
            match leases.acquire(&name, self.node_id(), ttl).await? {
                Ok(epoch) => {
                    held = Some(HeldLease { epoch });
                    deadline = Some(lease_deadline(sent, ttl));
                }
                Err(owner) => {
                    let e = EsError::new(503, "cluster_block_exception", format!("cluster [{name}] is served by node [{}]", owner.node))
                        .with_header_values(OWNER_HEADER, vec![owner.node.clone()]);
                    anyhow::bail!(OpenError { error: e, source: anyhow::anyhow!("cluster [{name}] is owned by node [{}] (epoch {})", owner.node, owner.epoch) });
                }
            }
            if let Some(store) = &data_store {
                store.set_epoch(&name, held.map_or(0, |h| h.epoch));
            }
        }
        let release_lease = |node: Arc<Node>, name: String, held: Option<HeldLease>| async move {
            if let (Some(l), Some(h)) = (node.leases.get(), held) {
                let _ = l.release(&name, node.node_id(), h.epoch).await;
            }
        };
        let r = match (self.leases.get(), held) {
            (Some(leases), Some(h)) => {
                // A wake can outlast the lease (a long WAL replay): keep it renewed meanwhile,
                // so the cluster comes up with a fresh deadline instead of losing its lease on
                // its first acknowledgement.
                let renewed = Mutex::new(deadline);
                let load = self.load_inner(&spec, data_store, deadline, h.epoch);
                let ttl = self.cfg.lease_ttl;
                let keep = keep_lease(leases.as_ref(), &name, self.node_id(), h.epoch, ttl, &renewed);
                tokio::pin!(load, keep);
                let lost = tokio::select! {
                    r = &mut load => Ok(r),
                    lost = &mut keep => Err(lost),
                };
                let mut r = match lost {
                    Ok(r) => r,
                    // Finish the load (it isn't cancellation safe) and drop what it built.
                    Err(lost) => load.await.and_then(|_| Err(lost)),
                };
                // The renewer may run its next round only a period from now: a lease last
                // renewed longer ago than that is renewed before the cluster serves anything.
                let stale = renewed.lock().is_some_and(|d| d - ttl.mul_f64(0.8) + lease_renew_period(ttl) <= Instant::now());
                if r.is_ok()
                    && stale
                    && let Err(lost) = renew_lease(leases.as_ref(), &name, self.node_id(), h.epoch, ttl, &renewed).await
                {
                    r = Err(lost);
                }
                if let Ok((_, _, Some(data), _)) = &r {
                    data.set_lease_deadline(*renewed.lock());
                }
                r
            }
            _ => self.load_inner(&spec, data_store, deadline, 0).await,
        };
        // The catalog may have changed since the request that started this wake read it (a
        // password rotated, limits changed): take the current description. Later changes find
        // the slot loading and wait in `pending_specs`.
        let spec = match (&r, self.cfg.default_cluster.is_none()) {
            (Ok(_), true) => match self.catalog.lookup(&name).await {
                Some(fresh) => ClusterSpec { dialect: spec.dialect, destructive_requires_name: spec.destructive_requires_name, ..fresh },
                None => spec,
            },
            _ => spec,
        };
        let (cluster, meta, data, recovery) = match r {
            Ok(x) => x,
            Err(e) => {
                release_lease(self.clone(), name.clone(), held).await;
                self.stores.release(&name).await;
                return Err(e);
            }
        };
        self.hooks.loaded(&cluster).await;
        let permits = spec.max_in_flight.max(1);
        let max_body = spec.max_body_bytes.unwrap_or(self.cfg.max_body_bytes);
        let bytes = ByteBudget::new(spec.max_in_flight_bytes.unwrap_or(max_body.saturating_mul(2)));
        let rate = spec.max_requests_per_second.map(|r| Arc::new(RateLimiter::new(r, r.max(1.0))));
        let wake_time = started.elapsed();
        tracing::debug!(cluster = name, ms = wake_time.as_secs_f64() * 1e3, shards = recovery.shards, records = recovery.records, "cluster loaded");
        self.wakes.fetch_add(1, Ordering::Relaxed);
        let loaded = Arc::new(Loaded {
            cluster,
            data,
            meta,
            name: name.as_str().into(),
            spec: parking_lot::RwLock::new(Arc::new(spec)),
            last_used: AtomicU64::new(self.now_ms()),
            in_flight: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            permits: Arc::new(Semaphore::new(permits)),
            permit_debt: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            rate: Mutex::new(rate),
            bytes,
            requests: AtomicU64::new(0),
            metered: AtomicU64::new(0),
            lease: held,
            kept_volatile: AtomicBool::new(false),
            default_max_body: self.cfg.max_body_bytes,
            wake_time,
            recovery,
        });
        Ok(loaded)
    }

    async fn load_inner(
        &self,
        spec: &ClusterSpec,
        data_store: Option<search_store::Store>,
        deadline: Option<Instant>,
        epoch: u64,
    ) -> anyhow::Result<(Arc<Cluster>, Arc<crate::meta_guard::CheckedMeta>, Option<Arc<ClusterData>>, persist::RecoveryStats)> {
        if let Some(ds) = &data_store
            && epoch > 0
        {
            // Before reading anything: an older owner's later writes and acknowledgements then
            // find this claim and are refused (`persist`, ownership).
            ds.claim_cluster(&spec.name).await.map_err(|e| e.context(format!("claiming cluster [{}] under epoch {epoch}", spec.name)))?;
        }
        // Only states that load again are saved: a cluster must never be left unwakeable.
        let meta = Arc::new(crate::meta_guard::CheckedMeta::new(self.stores.meta_store(&spec.name).await?, spec.dialect));
        let store: Arc<dyn search_es::MetaStore> = meta.clone();
        let mut cfg = ClusterConfig::new(&spec.name);
        cfg.dialect = spec.dialect;
        cfg.node_name = self.cfg.node_name.clone();
        cfg.destructive_requires_name = spec.destructive_requires_name;
        cfg.node_settings = self.cfg.node_settings.clone();
        cfg.test_features = self.cfg.test_features;
        cfg.host_metrics = self.cfg.host_metrics;
        let mut cluster = Cluster::open(cfg, store).await?;
        cluster.node.id = stable_node_id(&spec.name);
        if cluster.state().version <= 1 {
            // A cluster that never wrote its state got a fresh `cluster_uuid`: keep it, so it
            // stays the same across wakes (clients and monitoring key on it). Once per cluster.
            cluster.update(|_| Ok(())).await.map_err(|e| anyhow::anyhow!("saving the new cluster's state: {}", e.reason()))?;
        }
        let mut stats = persist::RecoveryStats::default();
        let data = match data_store {
            Some(ds) => {
                let verify_owner = epoch > 0 && ds.wal().config().sync_objects;
                let data = ClusterData::new(ds, &spec.name, self.tasks.clone());
                data.set_lease_deadline(deadline);
                data.set_verify_owner(verify_owner);
                cluster.engine = search_es::engine::Engine::with_storage(data.clone());
                stats = data.recover(&cluster).await?;
                // Async results an earlier instance stored; their changes are stored from now on.
                if let Err(e) = cluster.restore_async_results() {
                    tracing::warn!(cluster = spec.name, error = %e.reason(), "reading stored async results failed");
                }
                // Deployments it had running start again, as after an Elasticsearch restart.
                cluster.restore_ml_deployments();
                Some(data)
            }
            None => None,
        };
        let cluster = Arc::new(cluster);
        // `wait_for_completion=false` by-query tasks run on after their response.
        cluster.enable_background_tasks();
        if data.is_some() {
            // Snapshot copies a crash left unrecorded; off the wake path (it lists the pins).
            let (c, grace) = (Arc::downgrade(&cluster), self.cfg.snapshot_leftover_grace);
            tokio::spawn(async move {
                let Some(c) = c.upgrade() else { return };
                if let Err(e) = search_es::api::snapshots::sweep_leftover_copies(&c, grace).await {
                    tracing::debug!(cluster = c.name, error = %e.reason(), "sweeping leftover snapshot copies failed");
                }
            });
        }
        Ok((cluster, meta, data, stats))
    }

    /// Clusters idle (no request in flight, none for `older_than`, nothing pinning them).
    ///
    /// A pinned cluster counts as used while pinned, so its idle time starts when the pin ends
    /// (checked every reaper tick): a task that ran past the idle timeout keeps its stored result
    /// for a whole timeout afterwards, for the client that polls `_tasks/<id>`.
    ///
    /// A cluster's own [`ClusterSpec::idle_unload`] overrides `older_than`. Clusters holding
    /// documents only in memory are left out (see [`NodeConfig::unload_memory_only`]).
    pub fn idle(&self, older_than: Duration) -> Vec<String> {
        self.idle_with(Some(older_than))
    }

    /// [`Node::idle`] with the node's default timeout optional: clusters without a timeout of
    /// their own never idle when it is `None`.
    fn idle_with(&self, default: Option<Duration>) -> Vec<String> {
        let now = self.now_ms();
        let map = self.clusters.read();
        map.iter()
            .filter_map(|(k, s)| s.get().map(|l| (k, l)))
            .filter(|(_, l)| {
                if l.pinned() {
                    l.last_used.fetch_max(now, Ordering::SeqCst);
                    return false;
                }
                let Some(limit) = l.spec().idle_unload.or(default) else { return false };
                l.in_flight() == 0 && now.saturating_sub(l.last_used.load(Ordering::SeqCst)) >= limit.as_millis() as u64 && self.may_drop(l, "idle")
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Whether an automatic unload may drop `l`: its documents are durable, it holds none, or
    /// the node accepts losing them ([`NodeConfig::unload_memory_only`]).
    fn may_drop(&self, l: &Loaded, why: &str) -> bool {
        if l.data.is_some() || self.cfg.unload_memory_only {
            return true;
        }
        let holds_docs = l.cluster.engine.indices_snapshot().iter().any(|ie| ie.shards.iter().any(|se| se.shard.max_seq_no() >= 0));
        if holds_docs && !l.kept_volatile.swap(true, Ordering::Relaxed) {
            tracing::warn!(cluster = &*l.name, reason = why, "not unloading a cluster whose documents live only in memory (the store provider has no data store)");
        }
        !holds_docs
    }

    /// Unloads a cluster unless requests are in flight: commits its shards, detaches them from
    /// the WAL, releases its lease and memory. Returns whether it was unloaded.
    pub async fn unload(&self, name: &str) -> bool {
        self.unload_with(name, true, None).await
    }

    /// Unloads `name`; with `only`, just that instance (a reload since is left alone).
    async fn unload_with(&self, name: &str, flush: bool, only: Option<&Arc<Loaded>>) -> bool {
        let _t = self.transition(name).await;
        let loaded = {
            let mut map = self.clusters.write();
            let Some(l) = map.get(name).and_then(|s| s.get().cloned()) else { return false };
            if only.is_some_and(|o| !Arc::ptr_eq(o, &l)) {
                return false;
            }
            // Pairs with `enter`, which counts itself in flight before checking `closed`: mark
            // closed, then check in-flight (see the module docs).
            l.closed.store(true, Ordering::SeqCst);
            if flush && l.in_flight() > 0 {
                l.closed.store(false, Ordering::SeqCst);
                return false;
            }
            map.remove(name);
            l
        };
        self.hooks.unloading(&loaded.cluster).await;
        if flush
            && loaded.data.is_some()
            && let Err(e) = loaded.cluster.sync_async_results().await
        {
            tracing::warn!(cluster = name, error = %e.reason(), "storing async results on unload failed");
        }
        if flush
            && loaded.data.is_some()
            && let Err(e) = loaded.cluster.sync_ml_deployments(true).await
        {
            tracing::warn!(cluster = name, error = %e.reason(), "storing deployment assignments on unload failed");
        }
        // Requests still running on this instance (a forced unload) can't change it any more.
        loaded.meta.close();
        if !flush
            && loaded.lease.is_some()
            && let Some(d) = &loaded.data
        {
            // Nor acknowledge anything: once the lease is released below, the next owner may
            // replay the WAL before a write still in flight here is durable.
            d.revoke_lease();
        }
        if let Some(d) = &loaded.data {
            let flush = flush && d.failed().is_none();
            if let Err(e) = d.close(&loaded.cluster, flush).await {
                // Everything acknowledged is in the WAL (and stays replayable): only the commit
                // that would have made the next wake faster failed.
                tracing::warn!(cluster = name, error = %e, "flushing on unload failed");
            }
        }
        if let (Some(leases), Some(h)) = (self.leases.get(), loaded.lease)
            && let Err(e) = leases.release(name, self.node_id(), h.epoch).await
        {
            tracing::warn!(cluster = name, error = %e, "releasing the cluster's lease failed");
        }
        self.stores.release(name).await;
        let left = loaded.unmetered();
        if left > 0 {
            *self.unmetered.lock().entry(name.to_string()).or_default() += left;
        }
        drop(loaded);
        self.unloads.fetch_add(1, Ordering::Relaxed);
        self.residency.notify_waiters();
        release_memory();
        tracing::debug!(cluster = name, flush, "cluster unloaded");
        true
    }

    /// Drops a cluster at once, without writing (it lost its lease, or its storage failed):
    /// requests in flight finish on the old instance, whose writes can no longer be
    /// acknowledged.
    pub async fn force_unload(&self, name: &str) -> bool {
        self.unload_with(name, false, None).await
    }

    fn spawn_force_unload(self: &Arc<Self>, loaded: &Arc<Loaded>) {
        let me = self.clone();
        let loaded = loaded.clone();
        let guard = TaskGuard::new(&self.tasks);
        tokio::spawn(async move {
            let _g = guard;
            me.unload_with(&loaded.name.clone(), false, Some(&loaded)).await;
        });
    }

    /// Replaces a loaded cluster's description (new credentials, limits), at once: the request
    /// rate, the concurrency cap (permits are added, or retired as they come back) and the
    /// byte budget included. A cluster being woken gets it as soon as its load finishes (its
    /// catalog read may predate the change); unloaded clusters read the catalog when they wake.
    /// What the engine fixed at load (dialect, `destructive_requires_name`) needs a reload.
    pub fn update_spec(&self, spec: ClusterSpec) {
        let loaded = {
            let map = self.clusters.read();
            match map.get(&spec.name) {
                Some(slot) => match slot.get() {
                    Some(l) => l.clone(),
                    None => {
                        // Loading: `open` applies it right after the instance is published.
                        self.pending_specs.lock().insert(spec.name.clone(), spec);
                        return;
                    }
                },
                None => return,
            }
        };
        // A pending update is older than this one (it saw the slot loading): drop it so the
        // publishing opener can't apply it after this.
        let mut pending = self.pending_specs.lock();
        pending.remove(&spec.name);
        let wants_reaper = spec.idle_unload.is_some() || spec.max_resident_bytes.is_some();
        let replan = wants_reaper || loaded.spec().idle_unload.is_some();
        Self::apply_spec(&loaded, spec);
        drop(pending);
        if wants_reaper {
            // The next request starts a reaper if none runs.
            self.reaper_wanted.store(true, Ordering::SeqCst);
        }
        if replan {
            self.reaper_kick.notify_waiters();
        }
    }

    fn apply_spec(l: &Loaded, spec: ClusterSpec) {
        let cur = l.spec();
        if cur.max_requests_per_second != spec.max_requests_per_second {
            *l.rate.lock() = spec.max_requests_per_second.map(|r| Arc::new(RateLimiter::new(r, r.max(1.0))));
        }
        let (old, new) = (cur.max_in_flight.max(1), spec.max_in_flight.max(1));
        if new > old {
            // Cancel retirements still owed first, then add the rest.
            let mut add = new - old;
            while add > 0 {
                let d = l.permit_debt.load(Ordering::SeqCst);
                if d == 0 {
                    break;
                }
                let take = d.min(add);
                if l.permit_debt.compare_exchange(d, d - take, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    add -= take;
                }
            }
            l.permits.add_permits(add);
        } else if new < old {
            let gone = l.permits.forget_permits(old - new);
            l.permit_debt.fetch_add(old - new - gone, Ordering::SeqCst);
        }
        let max_body = spec.max_body_bytes.unwrap_or(l.default_max_body);
        l.bytes.set_limit(spec.max_in_flight_bytes.unwrap_or(max_body.saturating_mul(2)));
        *l.spec.write() = Arc::new(spec);
    }

    /// Applies an update that raced the cluster's load (see [`Node::update_spec`]).
    fn apply_pending_spec(&self, l: &Loaded) {
        let _map = self.clusters.write();
        // Applied under the pending lock, like direct updates: they apply in the order made.
        let mut pending = self.pending_specs.lock();
        if let Some(spec) = pending.remove(&*l.name) {
            Self::apply_spec(l, spec);
        }
    }

    /// Makes `child` a branch of `parent` (now).
    pub async fn branch(&self, parent: &str, child: &str) -> anyhow::Result<()> {
        self.branch_at(parent, child, None).await
    }

    /// Makes `child` a branch of `parent` as of `at_ms` (`None`: now). A loaded parent is
    /// committed first, so the child sees everything acknowledged; segments are shared, not
    /// copied.
    pub async fn branch_at(&self, parent: &str, child: &str, at_ms: Option<u64>) -> anyhow::Result<()> {
        anyhow::ensure!(self.get_loaded(child).is_none(), "cluster [{child}] is loaded");
        if let Some(l) = self.get_loaded(parent)
            && let Some(d) = &l.data
        {
            d.flush_all(&l.cluster, false).await?;
        }
        self.stores.branch_at(parent, child, at_ms).await
    }

    /// Deletes a cluster's data (unloading it first, without flushing).
    pub async fn delete_cluster(&self, name: &str) -> anyhow::Result<()> {
        self.force_unload(name).await;
        self.stores.delete(name).await
    }

    /// Flushes every shard of a loaded cluster to object storage.
    pub async fn flush(&self, name: &str) -> anyhow::Result<()> {
        if let Some(l) = self.get_loaded(name)
            && let Some(d) = &l.data
        {
            d.flush_all(&l.cluster, false).await?;
        }
        Ok(())
    }

    /// Docs, bytes and loaded memory of a cluster (from the engine when loaded, else from its
    /// commit points).
    pub async fn usage(&self, name: &str) -> anyhow::Result<Usage> {
        if let Some(l) = self.get_loaded(name) {
            let mut u = Usage { loaded: true, requests: l.requests(), ..Usage::default() };
            for ie in l.cluster.engine.indices_snapshot() {
                for se in &ie.shards {
                    let s = se.shard.stats();
                    u.docs += s.num_docs;
                    u.deleted_docs += s.deleted_docs;
                    u.store_bytes += s.segment_bytes;
                }
            }
            u.resident_bytes = persist::resident_bytes(&l.cluster);
            return Ok(u);
        }
        match self.stores.data() {
            Some(store) => {
                let cu = store.cluster_usage(name).await?;
                Ok(Usage { loaded: false, docs: cu.docs, deleted_docs: cu.deleted_docs, store_bytes: cu.segment_bytes, ..Usage::default() })
            }
            None => Ok(Usage::default()),
        }
    }

    /// Requests served per cluster since the previous call (metering): loaded clusters and
    /// those unloaded meanwhile, each request counted once. Only requests admitted to their
    /// cluster count (not ones refused by authentication or limits).
    pub fn take_request_counts(&self) -> Vec<(String, u64)> {
        let loaded: Vec<Arc<Loaded>> = self.clusters.read().values().filter_map(|s| s.get().cloned()).collect();
        let mut out: HashMap<String, u64> = std::mem::take(&mut *self.unmetered.lock());
        for l in loaded {
            let n = l.unmetered();
            if n > 0 {
                *out.entry(l.name.to_string()).or_default() += n;
            }
        }
        let mut v: Vec<(String, u64)> = out.into_iter().collect();
        v.sort_unstable();
        v
    }

    /// Applies memory budgets: commits clusters over their own `max_resident_bytes` (moving
    /// buffers and in-heap segments to files), then, while the node holds more than its
    /// `memory_budget` minus `room` (memory a wake is about to need), unloads the least recently
    /// used clusters with no request in flight. Returns the clusters unloaded.
    pub async fn enforce_memory(&self, room: u64) -> Vec<String> {
        self.enforce_memory_with(room, true).await
    }

    async fn enforce_memory_with(&self, room: u64, commit: bool) -> Vec<String> {
        let loaded: Vec<Arc<Loaded>> = self.clusters.read().values().filter_map(|s| s.get().cloned()).collect();
        if loaded.is_empty() {
            return Vec::new();
        }
        let mut sizes: Vec<(Arc<Loaded>, u64)> = loaded.into_iter().map(|l| {
            let r = persist::resident_bytes(&l.cluster);
            (l, r)
        }).collect();
        for (l, r) in sizes.iter_mut().filter(|_| commit) {
            if let (Some(max), Some(d)) = (l.spec().max_resident_bytes, &l.data)
                && *r > max
            {
                match d.flush_all(&l.cluster, false).await {
                    Ok(()) => *r = persist::resident_bytes(&l.cluster),
                    Err(e) => tracing::warn!(cluster = &*l.name, error = %e, "committing a cluster over its memory budget failed"),
                }
            }
        }
        let mut evicted = Vec::new();
        let Some(budget) = self.cfg.memory_budget else { return evicted };
        let mut total: u64 = sizes.iter().map(|(_, r)| *r).sum();
        if total.saturating_add(room) <= budget {
            return evicted;
        }
        // Clusters holding scrolls or points in time go last.
        sizes.sort_by_key(|(l, _)| (l.pinned(), l.last_used.load(Ordering::SeqCst)));
        for (l, r) in sizes {
            if total.saturating_add(room) <= budget {
                break;
            }
            if l.in_flight() > 0 || !self.may_drop(&l, "memory budget") {
                continue;
            }
            if self.unload_with(&l.name, true, Some(&l)).await {
                tracing::info!(cluster = &*l.name, resident = r, total, budget, "unloaded a cluster to stay within the node's memory budget");
                total = total.saturating_sub(r);
                evicted.push(l.name.to_string());
            }
        }
        evicted
    }

    /// Unloads every cluster (flushing, handing leases back) and closes the WAL; new loads are
    /// refused from now on. For SIGTERM: deploys and Spot reclaims.
    pub async fn shutdown(&self, timeout: Duration) -> anyhow::Result<()> {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Searches and background tasks stop at their next cancellation check instead of
        // holding the unload until the deadline.
        self.cancel_tasks();
        let deadline = Instant::now() + timeout;
        loop {
            let names = self.loaded();
            if names.is_empty() {
                break;
            }
            for n in names {
                if !self.unload(&n).await && Instant::now() >= deadline {
                    // Requests still running: their writes are in the WAL; don't wait forever.
                    self.force_unload(&n).await;
                }
            }
            if !self.loaded().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        if let Some(store) = self.stores.data() {
            store.close().await?;
        }
        Ok(())
    }

    /// What the reaper has to watch this round: whether memory budgets apply (a node budget,
    /// or a loaded cluster with its own), and the shortest idle timeout of a loaded cluster.
    fn reaper_plan(&self) -> (bool, Option<Duration>) {
        let mut budgets = self.cfg.memory_budget.is_some();
        let mut idle = None::<Duration>;
        for l in self.clusters.read().values().filter_map(|s| s.get()) {
            let spec = l.spec();
            budgets |= spec.max_resident_bytes.is_some();
            if let Some(t) = spec.idle_unload.or(self.cfg.idle_unload) {
                idle = Some(idle.map_or(t, |i| i.min(t)));
            }
        }
        (budgets, idle)
    }

    fn ensure_reaper(self: &Arc<Self>) {
        // Called on every request: the common case is one load.
        if self.reaper.load(Ordering::SeqCst) {
            return;
        }
        if self.cfg.idle_unload.is_none() && self.cfg.memory_budget.is_none() && !self.reaper_wanted.load(Ordering::SeqCst) {
            return;
        }
        if self.reaper.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        // Memory budgets are checked at least every few seconds.
        let tick = move |(needs_budgets, idle): (bool, Option<Duration>)| match idle {
            Some(idle) if !needs_budgets => (idle / 4).clamp(Duration::from_millis(20), Duration::from_secs(15)),
            Some(idle) => (idle / 4).clamp(Duration::from_millis(20), MEMORY_CHECK),
            None => MEMORY_CHECK,
        };
        let guard = TaskGuard::new(&self.tasks);
        tokio::spawn(async move {
            let _guard = guard;
            loop {
                {
                    let Some(node) = weak.upgrade() else { return };
                    let residency = node.residency.clone();
                    let kick = node.reaper_kick.clone();
                    let mut kicked = std::pin::pin!(kick.notified());
                    // Registered before the budget check below, so a load after it is seen.
                    kicked.as_mut().enable();
                    // Re-planned every round: the short tick only while a budget applies.
                    let plan = node.reaper_plan();
                    drop(node);
                    let unloaded = residency.notified();
                    tokio::select! {
                        _ = tokio::time::sleep(tick(plan)) => {}
                        _ = unloaded => {}
                        // A budgeted cluster loaded: re-plan the tick.
                        _ = kicked => continue,
                    }
                }
                let Some(node) = weak.upgrade() else { return };
                for name in node.idle_with(node.cfg.idle_unload) {
                    node.unload(&name).await;
                }
                node.enforce_memory(0).await;
                // Stop when nothing is loaded; the next load starts a new reaper. (Write lock: an
                // opener passes through the map lock after publishing and before checking
                // `reaper`, so it either is seen here or sees `false`.)
                let map = node.clusters.write();
                if map.values().all(|s| !s.initialized()) {
                    node.reaper_wanted.store(false, Ordering::SeqCst);
                    node.reaper.store(false, Ordering::SeqCst);
                    return;
                }
            }
        });
    }

    /// Renews the leases of loaded clusters every [`lease_renew_period`]; a cluster whose
    /// lease is lost is dropped. Runs only while a leased cluster is loaded.
    fn ensure_renewer(self: &Arc<Self>) {
        // Called on every request of a leased cluster: a load, not a write, when running.
        if self.renewer.load(Ordering::SeqCst) || self.renewer.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        let guard = TaskGuard::new(&self.tasks);
        tokio::spawn(async move {
            let _guard = guard;
            loop {
                let ttl = {
                    let Some(n) = weak.upgrade() else { return };
                    let ttl = n.cfg.lease_ttl;
                    let residency = n.residency.clone();
                    drop(n);
                    let unloaded = residency.notified();
                    tokio::select! {
                        _ = tokio::time::sleep(lease_renew_period(ttl)) => {}
                        _ = unloaded => {}
                    }
                    ttl
                };
                let Some(node) = weak.upgrade() else { return };
                let Some(leases) = node.leases.get().cloned() else { return };
                let held: Vec<Arc<Loaded>> = {
                    let map = node.clusters.write();
                    let held: Vec<Arc<Loaded>> = map.values().filter_map(|s| s.get().cloned()).filter(|l| l.lease.is_some()).collect();
                    if held.is_empty() {
                        // Under the map lock: a load finishing after this sees `false` and
                        // starts a new renewer (`open` calls `ensure_renewer` once loaded).
                        node.renewer.store(false, Ordering::SeqCst);
                        return;
                    }
                    held
                };
                // One round trip for every lease the node holds (a fleet node may hold thousands).
                let batch: Vec<(String, u64)> = held.iter().map(|l| (l.name.to_string(), l.epoch())).collect();
                let sent = Instant::now();
                match leases.renew_many(&batch, node.node_id(), ttl).await {
                    Ok(renewed) => {
                        for (l, ok) in held.iter().zip(renewed) {
                            if ok {
                                if let Some(d) = &l.data {
                                    d.set_lease_deadline(Some(lease_deadline(sent, ttl)));
                                }
                            } else {
                                tracing::warn!(cluster = &*l.name, epoch = l.epoch(), "lost the cluster's lease; dropping it");
                                // Only this instance: one reloaded since holds a new lease.
                                node.unload_with(&l.name, false, Some(l)).await;
                            }
                        }
                    }
                    Err(e) => {
                        // The deadline stops acknowledgements on its own; keep trying.
                        tracing::warn!(clusters = held.len(), error = %e, "renewing the node's leases failed");
                    }
                }
            }
        });
    }

    /// Whether the idle reaper is running (it only runs while clusters are loaded).
    pub fn reaper_running(&self) -> bool {
        self.reaper.load(Ordering::SeqCst)
    }

    /// Finds, authenticates and admits a request, permits included: the returned lease routes
    /// it to its cluster.
    pub async fn admit(self: &Arc<Self>, req: &EsRequest, sni: Option<&str>) -> Result<Lease, Rejected> {
        let mut lease = self.enter(req, sni).await?;
        lease.acquire(req).await?;
        Ok(lease)
    }

    /// Finds and authenticates a request and pins its cluster (loading it if needed), without
    /// taking a concurrency permit yet ([`Lease::acquire`]). Applies the request rate.
    pub async fn enter(self: &Arc<Self>, req: &EsRequest, sni: Option<&str>) -> Result<Lease, Rejected> {
        crate::process::note_activity();
        let reject = |error: EsError| Rejected { error, dialect: Dialect::Elasticsearch };
        let presented;
        let mut host_cluster = None;
        let claim = match &self.cfg.default_cluster {
            Some(_) => {
                presented = Presented::None;
                None
            }
            None => {
                presented = auth::parse_authorization(req.header("authorization")).map_err(|m| reject(malformed(m)))?;
                host_cluster = sni
                    .and_then(|s| auth::host_label(s, self.cfg.domain.as_deref()))
                    .or_else(|| req.header("host").and_then(|h| auth::host_label(h, self.cfg.domain.as_deref())));
                Some(auth::claim(&presented, host_cluster))
            }
        };
        let failed = || {
            let e = match &presented {
                Presented::None => unauthorized(format!("missing authentication credentials for REST request [{}]", req.uri)),
                // Elasticsearch says "invalid credentials for API key [id]" for a known id with a
                // wrong secret; one answer for both keeps cluster and key names unprobeable.
                Presented::ApiKey { id, .. } => {
                    unauthorized(ANONYMOUS_REFUSED.into()).with_meta("additional_unsuccessful_credentials", format!("API key: unable to find apikey with id {id}"))
                }
                Presented::Basic { user, .. } => unauthorized(format!("unable to authenticate user [{user}] for REST request [{}]", req.uri)),
                Presented::Bearer(_) => unauthorized(ANONYMOUS_REFUSED.into()).with_meta("additional_unsuccessful_credentials", "oauth2 token: invalid token"),
            };
            reject(e)
        };
        let claim = match claim {
            None => None,
            Some(Some(c)) => Some(c),
            Some(None) => return Err(failed()),
        };
        let name = match (&claim, &self.cfg.default_cluster) {
            (Some(c), _) => c.cluster.as_str(),
            (None, Some(name)) => name.as_str(),
            (None, None) => return Err(failed()),
        };
        // Credentials that aren't the cluster's own may be a principal of the cluster's
        // security (native user, API key, access token): checked once the cluster is loaded.
        let native = claim.as_ref().and_then(|c| auth::native_authorization(&presented, c, host_cluster));
        let native_ok = |loaded: &Loaded| native.as_deref().is_some_and(|h| search_es::xpack::security::authenticate_native(&loaded.cluster, h).is_some());
        // Fast path: a loaded cluster checks against its shared description, no catalog read.
        if let Some(loaded) = self.get_loaded(name) {
            let spec = loaded.spec();
            let mut via_native = false;
            if let Some(c) = &claim
                && !auth::verify(&spec, c)
            {
                if !native_ok(&loaded) {
                    return Err(failed());
                }
                via_native = true;
            }
            // Draining (SIGTERM): nothing new starts, so the requests in flight finish and the
            // cluster flushes and hands its lease over instead of being starved by a steady
            // stream of requests. Refused before running: the proxy resends it to the next owner.
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err(Rejected { error: EsError::new(503, "node_closed_exception", "node is shutting down"), dialect: spec.dialect });
            }
            // Otherwise it is being unloaded: wake it again below.
            if let Ok(mut lease) = self.pin(loaded, spec.dialect) {
                if via_native {
                    lease.native_auth = native.clone();
                } else {
                    lease.user = claim.as_ref().map(authenticated_user);
                }
                return self.admitted(lease, req);
            }
        }
        let spec = match self.catalog.lookup(name).await {
            Some(spec) => spec,
            None if self.cfg.default_cluster.is_some() => ClusterSpec::new(name.to_string()),
            None => return Err(failed()),
        };
        let mut via_native = false;
        if let Some(c) = &claim
            && !auth::verify(&spec, c)
        {
            if native.is_none() {
                return Err(failed());
            }
            via_native = true;
        }
        let dialect = spec.dialect;
        loop {
            let loaded = self.open(spec.clone()).await.map_err(|e| {
                let error = match e.downcast_ref::<OpenError>() {
                    Some(o) => o.error.clone(),
                    None => {
                        tracing::warn!(cluster = spec.name, error = %format!("{e:#}"), "cluster could not be loaded");
                        EsError::new(503, "cluster_block_exception", format!("cluster could not be loaded: {e:#}"))
                    }
                };
                Rejected { error, dialect }
            })?;
            if via_native && !native_ok(&loaded) {
                return Err(failed());
            }
            match self.pin(loaded, dialect) {
                Ok(mut lease) => {
                    if via_native {
                        lease.native_auth = native.clone();
                    } else {
                        lease.user = claim.as_ref().map(authenticated_user);
                    }
                    return self.admitted(lease, req);
                }
                Err(()) => tokio::task::yield_now().await,
            }
        }
    }

    /// Counts a request in flight on `loaded`, unless it is being unloaded (the residency
    /// protocol in the module docs: count first, then check `closed`).
    fn pin(self: &Arc<Self>, loaded: Arc<Loaded>, dialect: Dialect) -> Result<Lease, ()> {
        loaded.in_flight.fetch_add(1, Ordering::SeqCst);
        let lease = Lease {
            loaded,
            node: self.clone(),
            permit: None,
            fair: Mutex::new(None),
            admitted: Mutex::new(None),
            bytes: Reservation::none(),
            dialect,
            user: None,
            native_auth: None,
        };
        if lease.loaded.closed.load(Ordering::SeqCst) {
            // Unloaded (or being unloaded) under us: the lease's drop uncounts it.
            return Err(());
        }
        lease.loaded.last_used.store(self.now_ms(), Ordering::SeqCst);
        // One atomic load each when running (a lost reaper or renewer is restarted here).
        self.ensure_reaper();
        if lease.loaded.lease.is_some() {
            self.ensure_renewer();
        }
        Ok(lease)
    }

    /// Applies the cluster's request rate to a pinned request.
    fn admitted(&self, lease: Lease, req: &EsRequest) -> Result<Lease, Rejected> {
        let rate = lease.loaded.rate.lock();
        if let Some(r) = &*rate
            && !r.try_take()
        {
            return Err(Rejected {
                error: EsError::too_many_requests(format!(
                    "rejected execution of REST request [{} {}]: cluster [{}] exceeds its rate of [{}] requests per second",
                    req.method,
                    req.uri,
                    lease.loaded.name,
                    r.rate()
                )),
                dialect: lease.dialect,
            });
        }
        drop(rate);
        Ok(lease)
    }

    /// Handles a complete request (tests and embedders without the HTTP layer).
    pub async fn handle(self: &Arc<Self>, req: EsRequest) -> EsResponse {
        match self.enter(&req, None).await {
            Ok(mut lease) => {
                let max = lease.max_body_bytes(self);
                if req.body.len() > max {
                    return search_es::rest::error_response(&EsError::content_too_long(req.body.len() as u64, max as u64), &req, lease.loaded.spec().dialect);
                }
                if let Err(r) = lease.reserve_body(&req, req.body.len()) {
                    return search_es::rest::error_response(&r.error, &req, r.dialect);
                }
                if let Err(r) = lease.acquire(&req).await {
                    return search_es::rest::error_response(&r.error, &req, r.dialect);
                }
                lease.handle(req).await
            }
            Err(r) => search_es::rest::error_response(&r.error, &req, r.dialect),
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.clusters.get_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{MemoryStores, StaticCatalog};
    use base64::Engine;

    fn node(cfg: NodeConfig) -> (Arc<Node>, Arc<StaticCatalog>) {
        let catalog = Arc::new(StaticCatalog::new());
        catalog.insert(ClusterSpec::new("c1").with_password("pw1"));
        catalog.insert(ClusterSpec::new("c2").with_password("pw2"));
        (Node::new(catalog.clone(), Arc::new(MemoryStores::new()), cfg), catalog)
    }

    fn basic(u: &str, p: &str) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))
    }

    fn req(uri: &str, auth: Option<String>) -> EsRequest {
        let mut r = EsRequest::new("GET", uri);
        if let Some(a) = auth {
            r.headers.push(("Authorization".into(), a));
        }
        r
    }

    #[tokio::test]
    async fn routes_by_credentials() {
        let (n, _) = node(NodeConfig::default());
        let r = n.handle(req("/", Some(basic("c1", "pw1")))).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body_json().get("cluster_name").and_then(|v| v.as_str()), Some("c1"));
        let r = n.handle(req("/", Some("Bearer c2_pw2".into()))).await;
        assert_eq!(r.body_json().get("cluster_name").and_then(|v| v.as_str()), Some("c2"));
        let r = n.handle(req("/", Some(basic("c1", "nope")))).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body_json().pointer(&["error", "reason"]).and_then(|v| v.as_str()), Some("unable to authenticate user [c1] for REST request [/]"));
        assert!(r.headers.iter().any(|(k, v)| k == "WWW-Authenticate" && v == "ApiKey"));
        let r = n.handle(req("/_cluster/health", None)).await;
        assert_eq!(r.status, 401);
        assert_eq!(
            r.body_json().pointer(&["error", "reason"]).and_then(|v| v.as_str()),
            Some("missing authentication credentials for REST request [/_cluster/health]")
        );
        // Unknown clusters fail like wrong passwords.
        let r = n.handle(req("/", Some(basic("zz", "pw1")))).await;
        assert_eq!(r.status, 401);
    }

    #[tokio::test]
    async fn idle_unload_and_wake() {
        let cfg = NodeConfig { idle_unload: Some(Duration::from_millis(80)), ..NodeConfig::default() };
        let (n, _) = node(cfg);
        assert!(!n.reaper_running());
        let mut r = EsRequest::json("PUT", "/idx", "{}");
        r.headers.push(("Authorization".into(), basic("c1", "pw1")));
        assert_eq!(n.handle(r).await.status, 200);
        assert_eq!(n.loaded(), vec!["c1".to_string()]);
        assert!(n.reaper_running());
        assert_eq!(n.background_tasks(), 1);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !n.loaded().is_empty() || n.reaper_running() || n.background_tasks() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cluster unloads and the reaper stops");
        // Waking keeps the metadata.
        let r = n.handle(req("/idx", Some(basic("c1", "pw1")))).await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn credential_rotation_reaches_loaded_clusters() {
        let (n, _) = node(NodeConfig::default());
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 200);
        n.update_spec(ClusterSpec::new("c1").with_password("rotated"));
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 401);
        assert_eq!(n.handle(req("/", Some(basic("c1", "rotated")))).await.status, 200);
    }

    #[tokio::test]
    async fn dev_mode_needs_no_auth() {
        let cfg = NodeConfig { default_cluster: Some("dev".into()), ..NodeConfig::default() };
        let (n, _) = node(cfg);
        let r = n.handle(req("/", None)).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body_json().get("cluster_name").and_then(|v| v.as_str()), Some("dev"));
    }

    #[tokio::test]
    async fn in_flight_limit() {
        let catalog = Arc::new(StaticCatalog::new());
        let mut spec = ClusterSpec::new("c");
        spec.max_in_flight = 1;
        catalog.insert(spec);
        let n = Node::new(catalog, Arc::new(MemoryStores::new()), NodeConfig { default_cluster: Some("c".into()), ..NodeConfig::default() });
        let r = req("/", None);
        let a = n.admit(&r, None).await.unwrap();
        // One may wait…
        let n2 = n.clone();
        let waiting = tokio::spawn(async move { n2.admit(&req("/", None), None).await.is_ok() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        // …the next is rejected.
        let e = n.admit(&r, None).await.err().unwrap();
        assert_eq!(e.error.status, 429);
        drop(a);
        assert!(waiting.await.unwrap());
    }

    #[tokio::test]
    async fn request_rate_limit() {
        let catalog = Arc::new(StaticCatalog::new());
        let mut spec = ClusterSpec::new("c");
        spec.max_requests_per_second = Some(2.0);
        catalog.insert(spec);
        let n = Node::new(catalog, Arc::new(MemoryStores::new()), NodeConfig { default_cluster: Some("c".into()), ..NodeConfig::default() });
        assert_eq!(n.handle(req("/", None)).await.status, 200);
        assert_eq!(n.handle(req("/", None)).await.status, 200);
        let r = n.handle(req("/", None)).await;
        assert_eq!(r.status, 429);
        assert_eq!(r.body_json().pointer(&["error", "type"]).and_then(|v| v.as_str()), Some("es_rejected_execution_exception"));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(n.handle(req("/", None)).await.status, 200);
    }

    fn limited(max: usize) -> Arc<Node> {
        let catalog = Arc::new(StaticCatalog::new());
        let mut spec = ClusterSpec::new("c");
        spec.max_in_flight = max;
        catalog.insert(spec);
        Node::new(catalog, Arc::new(MemoryStores::new()), NodeConfig { default_cluster: Some("c".into()), ..NodeConfig::default() })
    }

    #[tokio::test]
    async fn cancelled_waiters_leave_the_queue() {
        let n = limited(2);
        let r = req("/", None);
        let held = [n.admit(&r, None).await.unwrap(), n.admit(&r, None).await.unwrap()];
        // Two requests queue, then their clients go away (the futures are dropped).
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let n2 = n.clone();
            waiters.push(tokio::spawn(async move { n2.admit(&req("/", None), None).await.is_ok() }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let loaded = n.get_loaded("c").unwrap();
        assert_eq!(loaded.waiting.load(Ordering::SeqCst), 2);
        for w in &waiters {
            w.abort();
        }
        for w in waiters {
            let _ = w.await;
        }
        assert_eq!(loaded.waiting.load(Ordering::SeqCst), 0);
        assert_eq!(loaded.in_flight(), 2);
        // The queue has room again.
        let n2 = n.clone();
        let queued = tokio::spawn(async move { n2.admit(&req("/", None), None).await.is_ok() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        assert!(queued.await.unwrap());
        assert_eq!(loaded.waiting.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_never_run_on_an_unloaded_cluster() {
        let n = limited(64);
        let r = req("/", None);
        for _ in 0..300 {
            drop(n.admit(&r, None).await.unwrap());
            let n2 = n.clone();
            let unload = tokio::spawn(async move { n2.unload("c").await });
            let n3 = n.clone();
            let enter = tokio::spawn(async move {
                let lease = n3.enter(&req("/", None), None).await.unwrap();
                // While the lease lives, its cluster is the loaded one and stays open.
                tokio::task::yield_now().await;
                let current = n3.get_loaded("c");
                let ok = !lease.loaded.closed.load(Ordering::SeqCst) && current.is_some_and(|c| Arc::ptr_eq(&c, &lease.loaded));
                drop(lease);
                ok
            });
            let _ = unload.await.unwrap();
            assert!(enter.await.unwrap());
        }
        // Transition locks don't accumulate.
        assert_eq!(n.transition_locks(), 0);
    }

    #[tokio::test]
    async fn max_in_flight_changes_apply_live() {
        let n = limited(2);
        let r = req("/", None);
        let a = n.admit(&r, None).await.unwrap();
        let b = n.admit(&r, None).await.unwrap();
        let loaded = n.get_loaded("c").unwrap();
        assert_eq!(loaded.permits.available_permits(), 0);
        // Raised: a third runs at once.
        let mut spec = (*loaded.spec()).clone();
        spec.max_in_flight = 3;
        n.update_spec(spec.clone());
        let c = n.admit(&r, None).await.unwrap();
        // Lowered to 1 with three out: they are retired as they come back, until one is left.
        spec.max_in_flight = 1;
        n.update_spec(spec.clone());
        assert_eq!(loaded.permit_debt.load(Ordering::SeqCst), 2);
        drop(a);
        drop(b);
        assert_eq!(loaded.permits.available_permits(), 0);
        assert_eq!(loaded.permit_debt.load(Ordering::SeqCst), 0);
        drop(c);
        assert_eq!(loaded.permits.available_permits(), 1);
        // Raised again while a debt is owed: the debt is cancelled first.
        let a = n.admit(&r, None).await.unwrap();
        spec.max_in_flight = 0; // treated as 1: no change
        n.update_spec(spec.clone());
        assert_eq!(loaded.permits.available_permits(), 0);
        drop(a);
        assert_eq!(loaded.permits.available_permits(), 1);
        // Same instance throughout: no reload happened.
        assert!(Arc::ptr_eq(&loaded, &n.get_loaded("c").unwrap()));
        assert_eq!(n.wakes.load(Ordering::Relaxed), 1);
    }

    /// A store whose metadata read waits until released, to hold a load in progress.
    struct SlowStores {
        inner: MemoryStores,
        gate: Arc<tokio::sync::Notify>,
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl StoreProvider for SlowStores {
        async fn meta_store(&self, cluster: &str) -> anyhow::Result<Arc<dyn search_es::MetaStore>> {
            self.started.notify_one();
            self.gate.notified().await;
            self.inner.meta_store(cluster).await
        }
        async fn branch(&self, _parent: &str, _child: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete(&self, _cluster: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn spec_updates_reach_a_load_in_progress() {
        let catalog = Arc::new(StaticCatalog::new());
        catalog.insert(ClusterSpec::new("c1").with_password("old"));
        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let stores = Arc::new(SlowStores { inner: MemoryStores::new(), gate: gate.clone(), started: started.clone() });
        let n = Node::new(catalog.clone(), stores, NodeConfig::default());
        let n2 = n.clone();
        let first = tokio::spawn(async move { n2.handle(req("/", Some(basic("c1", "old")))).await.status });
        started.notified().await;
        // The wake read the catalog already; the password changes now.
        catalog.insert(ClusterSpec::new("c1").with_password("new"));
        n.update_spec(ClusterSpec::new("c1").with_password("new"));
        gate.notify_one();
        // The request that started the wake was checked against the old password.
        assert_eq!(first.await.unwrap(), 200);
        assert_eq!(n.handle(req("/", Some(basic("c1", "old")))).await.status, 401);
        assert_eq!(n.handle(req("/", Some(basic("c1", "new")))).await.status, 200);
        assert!(n.pending_specs.lock().is_empty());
        // Updates of clusters that aren't loaded or loading leave nothing behind.
        n.update_spec(ClusterSpec::new("c2"));
        assert!(n.pending_specs.lock().is_empty());
        // A change that reaches the catalog before the wake finishes (with nobody calling
        // update_spec) is picked up too: the wake reads the catalog again before publishing.
        assert!(n.unload("c1").await);
        let n2 = n.clone();
        let first = tokio::spawn(async move { n2.handle(req("/", Some(basic("c1", "new")))).await.status });
        started.notified().await;
        let mut changed = ClusterSpec::new("c1").with_password("newer");
        changed.max_requests_per_second = Some(1000.0);
        catalog.insert(changed);
        gate.notify_one();
        assert_eq!(first.await.unwrap(), 200);
        assert_eq!(n.handle(req("/", Some(basic("c1", "new")))).await.status, 401);
        assert_eq!(n.handle(req("/", Some(basic("c1", "newer")))).await.status, 200);
        assert_eq!(n.get_loaded("c1").unwrap().spec().max_requests_per_second, Some(1000.0));
    }

    #[tokio::test]
    async fn request_counts_survive_unloads_and_are_reported_once() {
        let (n, _) = node(NodeConfig { idle_unload: None, ..NodeConfig::default() });
        for _ in 0..3 {
            assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 200);
        }
        assert_eq!(n.handle(req("/", Some(basic("c2", "pw2")))).await.status, 200);
        // Refused requests never reach a cluster.
        assert_eq!(n.handle(req("/", Some(basic("c1", "wrong")))).await.status, 401);
        assert_eq!(n.take_request_counts(), vec![("c1".to_string(), 3), ("c2".to_string(), 1)]);
        assert!(n.take_request_counts().is_empty());
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 200);
        assert!(n.unload("c1").await);
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 200);
        // One request from the unloaded instance, one from the new one.
        assert_eq!(n.take_request_counts(), vec![("c1".to_string(), 2)]);
        assert!(n.take_request_counts().is_empty());
    }

    #[tokio::test]
    async fn open_scrolls_keep_their_cluster_loaded() {
        let (n, _) = node(NodeConfig { idle_unload: Some(Duration::from_millis(40)), unload_memory_only: true, ..NodeConfig::default() });
        let send = |method: &str, uri: &str, body: &str, who: (&str, &str)| {
            let mut r = EsRequest::json(method, uri, body.to_string());
            r.headers.push(("Authorization".into(), basic(who.0, who.1)));
            let n = n.clone();
            async move { n.handle(r).await }
        };
        let c1 = ("c1", "pw1");
        assert_eq!(send("PUT", "/i/_doc/1?refresh=true", r#"{"f":1}"#, c1).await.status, 201);
        assert_eq!(send("PUT", "/i/_doc/2?refresh=true", r#"{"f":2}"#, c1).await.status, 201);
        let r = send("GET", "/i/_search?scroll=1m&size=1", "", c1).await;
        assert_eq!(r.status, 200);
        let id = r.body_json().get("_scroll_id").and_then(|v| v.as_str()).unwrap().to_string();
        // c2 has nothing open: it unloads; c1 holds a scroll for a minute.
        assert_eq!(send("GET", "/", "", ("c2", "pw2")).await.status, 200);
        tokio::time::timeout(Duration::from_secs(5), async {
            while n.get_loaded("c2").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("c2 unloads");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(n.get_loaded("c1").is_some(), "a cluster with an open scroll stays loaded");
        let r = send("POST", "/_search/scroll", &format!(r#"{{"scroll":"1m","scroll_id":"{id}"}}"#), c1).await;
        assert_eq!(r.status, 200, "{:?}", r.body_json());
        assert_eq!(r.body_json().pointer(&["hits", "hits"]).and_then(|v| v.as_array()).map(|a| a.len()), Some(1));
        // Once its last scroll is closed, the cluster idles out as usual (no need to wait for
        // the keep-alive).
        let r = send("DELETE", "/_search/scroll", &format!(r#"{{"scroll_id":"{id}"}}"#), c1).await;
        assert_eq!(r.status, 200, "{:?}", r.body_json());
        tokio::time::timeout(Duration::from_secs(5), async {
            while n.get_loaded("c1").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("c1 unloads once its scroll expired");
    }

    /// A point in time pins its cluster until it is closed or its keep alive passes, whichever
    /// comes first; nothing waits for the longest keep alive ever given.
    #[tokio::test]
    async fn points_in_time_pin_until_closed_or_expired() {
        let (n, _) = node(NodeConfig { idle_unload: Some(Duration::from_millis(40)), unload_memory_only: true, ..NodeConfig::default() });
        let send = |method: &str, uri: &str, body: &str| {
            let mut r = EsRequest::json(method, uri, body.to_string());
            r.headers.push(("Authorization".into(), basic("c1", "pw1")));
            let n = n.clone();
            async move { n.handle(r).await }
        };
        let gone = |n: Arc<Node>| async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while n.get_loaded("c1").is_some() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok()
        };
        assert_eq!(send("PUT", "/i/_doc/1?refresh=true", r#"{"f":1}"#).await.status, 201);
        // Closed explicitly: unloads right after, though the keep alive is an hour.
        let r = send("POST", "/i/_pit?keep_alive=1h", "").await;
        assert_eq!(r.status, 200);
        let id = r.body_json().get("id").and_then(|v| v.as_str()).unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(n.get_loaded("c1").is_some(), "an open point in time keeps its cluster loaded");
        assert_eq!(send("DELETE", "/_pit", &format!(r#"{{"id":"{id}"}}"#)).await.status, 200);
        assert!(gone(n.clone()).await, "unloads once the point in time is closed");
        // Expired: unloads once the (short) keep alive passed, without a close.
        assert_eq!(send("POST", "/i/_pit?keep_alive=300ms", "").await.status, 200);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(n.get_loaded("c1").is_some());
        assert!(gone(n.clone()).await, "unloads once the keep alive passed");
    }

    #[tokio::test]
    async fn idle_time_starts_when_a_task_ends() {
        let (n, _) = node(NodeConfig { idle_unload: None, ..NodeConfig::default() });
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 200);
        let l = n.get_loaded("c1").unwrap();
        let idle = Duration::from_millis(60);
        {
            let _task = l.cluster.tasks.register("indices:data/write/reindex", "long", None, Vec::new());
            tokio::time::sleep(idle * 2).await;
            assert!(n.idle(idle).is_empty(), "a running task pins its cluster");
        }
        // The task ran for longer than the idle timeout: its result still gets a whole timeout.
        assert!(n.idle(idle).is_empty());
        tokio::time::sleep(idle / 2).await;
        assert!(n.idle(idle).is_empty());
        tokio::time::sleep(idle).await;
        assert_eq!(n.idle(idle), vec!["c1".to_string()]);
    }

    #[tokio::test]
    async fn node_and_cluster_ids_are_stable_across_wakes() {
        let (n, _) = node(NodeConfig { idle_unload: None, ..NodeConfig::default() });
        let id = |n: &Arc<Node>, who: (&'static str, &'static str)| {
            let n = n.clone();
            async move {
                let r = n.handle(req("/_nodes/_local?filter_path=nodes.*.name", Some(basic(who.0, who.1)))).await;
                r.body_json().get("nodes").and_then(|v| v.as_object()).and_then(|m| m.keys().next().cloned()).unwrap()
            }
        };
        let uuid = |n: &Arc<Node>| {
            let n = n.clone();
            async move { n.handle(req("/", Some(basic("c1", "pw1")))).await.body_json().get("cluster_uuid").and_then(|v| v.as_str()).unwrap().to_string() }
        };
        let first = id(&n, ("c1", "pw1")).await;
        assert_eq!(first, stable_node_id("c1"));
        assert_eq!(first.len(), 22);
        // A cluster that never wrote anything keeps its uuid too.
        let cluster_uuid = uuid(&n).await;
        assert!(n.unload("c1").await);
        assert_eq!(id(&n, ("c1", "pw1")).await, first);
        assert_eq!(uuid(&n).await, cluster_uuid);
        assert_ne!(id(&n, ("c2", "pw2")).await, first);
    }

    /// Every state a cluster accepted must wake again: a cluster whose saved state fails to load
    /// can never be served. (search-es once took a dense_vector's dims from the first document
    /// without the index_options checks; [`crate::meta_guard`] refuses any such state.)
    #[tokio::test]
    async fn accepted_mappings_always_wake() {
        let (n, _) = node(NodeConfig { idle_unload: None, ..NodeConfig::default() });
        let send = |method: &str, uri: &str, body: &str| {
            let mut r = EsRequest::json(method, uri, body.to_string());
            r.headers.push(("Authorization".into(), basic("c1", "pw1")));
            let n = n.clone();
            async move { n.handle(r).await }
        };
        let mapping = r#"{"mappings":{"properties":{"v":{"type":"dense_vector","index":true,"similarity":"l2_norm","index_options":{"type":"int4_hnsw"}}}}}"#;
        assert_eq!(send("PUT", "/dd", mapping).await.status, 200);
        // Elasticsearch refuses the document (odd dims for int4_hnsw) and keeps the mapping.
        let r = send("POST", "/dd/_doc", r#"{"v":[1,2,3,4,5]}"#).await;
        assert!(r.status >= 400, "{:?}", r.body_json());
        let r = send("GET", "/dd/_mapping", "").await;
        assert!(r.body_json().pointer(&["dd", "mappings", "properties", "v", "dims"]).is_none(), "{:?}", r.body_json());
        // Even dims are fine, and stick.
        assert_eq!(send("POST", "/dd/_doc", r#"{"v":[1,2,3,4]}"#).await.status, 201);
        assert!(n.unload("c1").await);
        let r = send("GET", "/dd/_mapping", "").await;
        assert_eq!(r.status, 200, "{:?}", r.body_json());
        assert_eq!(r.body_json().pointer(&["dd", "mappings", "properties", "v", "dims"]).and_then(|v| v.as_i64()), Some(4));
    }

    #[tokio::test]
    async fn memory_budget_unloads_least_recently_used() {
        let catalog = Arc::new(StaticCatalog::new());
        for c in ["a", "b", "c"] {
            catalog.insert(ClusterSpec::new(c).with_password("pw"));
        }
        let cfg = NodeConfig { idle_unload: None, memory_budget: Some(1), unload_memory_only: true, ..NodeConfig::default() };
        let n = Node::new(catalog, Arc::new(MemoryStores::new()), cfg);
        let write = |c: &str| {
            let mut r = EsRequest::json("PUT", "/i/_doc/1", r#"{"f":"some text"}"#);
            r.headers.push(("Authorization".into(), basic(c, "pw")));
            r
        };
        assert_eq!(n.handle(write("a")).await.status, 201);
        assert!(n.usage("a").await.unwrap().resident_bytes > 1);
        assert_eq!(n.handle(write("b")).await.status, 201);
        // Waking b made room: a (least recently used, idle) went.
        assert_eq!(n.loaded(), vec!["b".to_string()]);
        // A cluster with a request in flight is never evicted.
        let pinned = n.enter(&req("/", Some(basic("b", "pw"))), None).await.unwrap();
        assert_eq!(n.handle(write("c")).await.status, 201);
        let mut loaded = n.loaded();
        loaded.sort();
        assert_eq!(loaded, vec!["b".to_string(), "c".to_string()]);
        drop(pinned);
        // The periodic check: both are over the budget once nothing is in flight.
        let mut evicted = n.enforce_memory(0).await;
        evicted.sort();
        assert_eq!(evicted, vec!["b".to_string(), "c".to_string()]);
        assert!(n.loaded().is_empty());
        // Without a budget nothing is evicted.
        let n = Node::new(Arc::new(StaticCatalog::new()), Arc::new(MemoryStores::new()), NodeConfig { default_cluster: Some("d".into()), ..NodeConfig::default() });
        assert_eq!(n.handle(EsRequest::json("PUT", "/i/_doc/1", r#"{"f":1}"#)).await.status, 201);
        assert!(n.enforce_memory(u64::MAX).await.is_empty());
    }

    #[tokio::test]
    async fn memory_only_documents_are_never_dropped_automatically() {
        // A provider without a data store (a wrapper that didn't forward `data()`): documents
        // live only in memory, so idle and budget unloads must leave such clusters loaded.
        let catalog = Arc::new(StaticCatalog::new());
        for c in ["docs", "empty"] {
            catalog.insert(ClusterSpec::new(c).with_password("pw"));
        }
        let cfg = NodeConfig { idle_unload: Some(Duration::from_millis(30)), memory_budget: Some(1), ..NodeConfig::default() };
        let n = Node::new(catalog, Arc::new(MemoryStores::new()), cfg);
        let send = |c: &str, method: &str, uri: &str, body: &str| {
            let mut r = EsRequest::json(method, uri, body.to_string());
            r.headers.push(("Authorization".into(), basic(c, "pw")));
            let n = n.clone();
            async move { n.handle(r).await.status }
        };
        assert_eq!(send("docs", "PUT", "/i/_doc/1", r#"{"f":1}"#).await, 201);
        assert_eq!(send("empty", "PUT", "/only-metadata", "{}").await, 200);
        // The empty cluster idles out (its metadata is kept); the one with documents stays.
        tokio::time::timeout(Duration::from_secs(5), async {
            while n.get_loaded("empty").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a cluster without documents unloads");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(n.idle(Duration::ZERO).is_empty());
        assert!(n.enforce_memory(0).await.is_empty());
        assert_eq!(n.loaded(), vec!["docs".to_string()]);
        assert_eq!(send("docs", "GET", "/i/_doc/1", "").await, 200);
        assert_eq!(send("empty", "HEAD", "/only-metadata", "").await, 200);
        // An explicit unload still unloads (the caller decides).
        assert!(n.unload("docs").await);
        assert_eq!(send("docs", "GET", "/i/_doc/1", "").await, 404);
    }

    #[tokio::test]
    async fn a_clusters_own_idle_timeout_applies_without_a_node_default() {
        let catalog = Arc::new(StaticCatalog::new());
        let mut quick = ClusterSpec::new("quick").with_password("pw");
        quick.idle_unload = Some(Duration::from_millis(40));
        catalog.insert(quick);
        catalog.insert(ClusterSpec::new("forever").with_password("pw"));
        let n = Node::new(catalog, Arc::new(MemoryStores::new()), NodeConfig { idle_unload: None, ..NodeConfig::default() });
        for c in ["forever", "quick"] {
            assert_eq!(n.handle(req("/", Some(basic(c, "pw")))).await.status, 200);
        }
        assert!(n.reaper_running(), "a cluster with its own timeout needs the reaper");
        tokio::time::timeout(Duration::from_secs(5), async {
            while n.get_loaded("quick").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the cluster's own idle timeout unloads it");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(n.loaded(), vec!["forever".to_string()], "no node default: the other stays");
        // A timeout given to a loaded cluster later applies too.
        let mut spec = (*n.get_loaded("forever").unwrap().spec()).clone();
        spec.idle_unload = Some(Duration::from_millis(40));
        n.update_spec(spec);
        assert_eq!(n.handle(req("/", Some(basic("forever", "pw")))).await.status, 200);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !n.loaded().is_empty() || n.reaper_running() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("unloads, then the reaper stops");
    }

    #[tokio::test]
    async fn loaded_clusters_are_admitted_on_the_fast_path() {
        let (n, catalog) = node(NodeConfig::default());
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 200);
        // The catalog isn't asked again while the cluster is loaded: removing it changes nothing.
        catalog.remove("c1");
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 200);
        assert_eq!(n.handle(req("/", Some(basic("c1", "wrong")))).await.status, 401);
        assert_eq!(n.wakes.load(Ordering::Relaxed), 1);
        // Once unloaded, it has to be in the catalog to wake.
        assert!(n.unload("c1").await);
        assert_eq!(n.handle(req("/", Some(basic("c1", "pw1")))).await.status, 401);
    }

    struct FailingStores;
    #[async_trait]
    impl StoreProvider for FailingStores {
        async fn meta_store(&self, _cluster: &str) -> anyhow::Result<Arc<dyn search_es::MetaStore>> {
            anyhow::bail!("store unavailable")
        }
        async fn branch(&self, _parent: &str, _child: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete(&self, _cluster: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn read_only_posts() {
        let post = |uri: &str| is_read_only_post(&EsRequest::json("POST", uri, Vec::new()));
        for uri in [
            "/_search",
            "/i/_search?q=x",
            "/i,j/_msearch",
            "/_count",
            "/i/_mget",
            "/i/_field_caps?fields=*",
            "/i/_termvectors/1",
            "/_mtermvectors",
            "/i/_explain/1",
            "/i/_validate/query",
            "/_search/scroll",
            "/i/_search/template",
            "/_msearch/template",
            "/i/_rank_eval",
        ] {
            assert!(post(uri), "{uri}");
        }
        for uri in ["/i/_doc", "/i/_bulk", "/i/_update/1", "/i/_delete_by_query", "/i/_update_by_query", "/_reindex", "/i/_refresh", "/i/_flush", "/i/_pit", "/_async_search", "/i/_doc/_search", "/i/_create/_count"] {
            assert!(!post(uri), "{uri}");
        }
        assert!(!is_read_only_post(&EsRequest::json("PUT", "/i/_search", Vec::new())));
    }

    #[tokio::test]
    async fn failed_opens_leave_nothing_behind() {
        let catalog = Arc::new(StaticCatalog::new());
        catalog.insert(ClusterSpec::new("c"));
        let n = Node::new(catalog, Arc::new(FailingStores), NodeConfig { default_cluster: Some("c".into()), ..NodeConfig::default() });
        let r = n.handle(req("/", None)).await;
        assert_eq!(r.status, 503);
        assert!(n.clusters.read().is_empty());
        assert_eq!(n.transition_locks(), 0);
    }

    #[test]
    fn flush_requests() {
        assert!(is_flush(&EsRequest::new("POST", "/_flush")));
        assert!(is_flush(&EsRequest::new("POST", "/a,b/_flush?wait_if_ongoing=true")));
        assert!(is_flush(&EsRequest::new("GET", "/a/_flush/")));
        assert!(!is_flush(&EsRequest::new("PUT", "/_flush")));
        assert!(!is_flush(&EsRequest::new("POST", "/a/_refresh")));
    }
}

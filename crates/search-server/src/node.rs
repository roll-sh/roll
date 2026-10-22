//! A multi-tenant node: many clusters in one process, loaded on the first request and unloaded
//! when idle.
//!
//! - **Routing and auth** ([`crate::auth`]): every request names its cluster through its
//!   credentials or host name; the [`Catalog`] says whether it exists and which secrets it
//!   accepts. In dev mode ([`NodeConfig::default_cluster`]) everything goes to one cluster
//!   without authentication.
//! - **Wake on request.** A cluster that isn't loaded is opened from its [`StoreProvider`] by the
//!   first request; concurrent first requests share one open.
//! - **Limits.** Each cluster has a body size limit and a cap on requests in flight, with a
//!   queue of the same size behind it; beyond that requests get 429, so one tenant can't
//!   starve the node. Admission has two steps: [`Node::enter`] authenticates and pins the
//!   cluster (it can't be unloaded while a lease exists), then [`Lease::acquire`] takes the
//!   concurrency permit. The HTTP layer reads the body in between, so a slow client never holds
//!   a permit.
//! - **Residency protocol.** A request counts itself in `in_flight` and only then checks
//!   `closed`; unload sets `closed` and only then checks `in_flight`, both under the map lock
//!   for unload. With sequentially consistent atomics one side always sees the other: either
//!   unload backs off, or the request retries against a fresh load. No request ever runs on a
//!   cluster instance that has been unloaded (two live instances would overwrite each other's
//!   metadata).
//! - **Idle unload.** A cluster with no request for its idle timeout is unloaded (its metadata
//!   is already durable: every change is persisted before it is acknowledged). The reaper task
//!   exists only while some cluster is loaded, so an idle node runs no timer at all.
//!   [`LifecycleHooks`] let the engine flush on unload and warm up on wake.

use crate::auth::{self, Presented};
use crate::catalog::{Catalog, ClusterSpec, StoreProvider};
use async_trait::async_trait;
use parking_lot::Mutex;
use search_es::{Cluster, ClusterConfig, Dialect, EsError, EsRequest, EsResponse};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, Semaphore};

/// `http.max_content_length` default.
pub const DEFAULT_MAX_BODY: usize = 100 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// The name the node reports (`GET /` `name`).
    pub node_name: String,
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
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            node_name: "roll-search".into(),
            domain: None,
            idle_unload: Some(Duration::from_secs(300)),
            max_body_bytes: DEFAULT_MAX_BODY,
            default_cluster: None,
            node_settings: Vec::new(),
            test_features: false,
            host_metrics: false,
        }
    }
}

/// Engine hooks around a cluster's residency (flush on unload, warm-up on wake).
#[async_trait]
pub trait LifecycleHooks: Send + Sync + 'static {
    async fn loaded(&self, _cluster: &Arc<Cluster>) {}
    async fn unloading(&self, _cluster: &Arc<Cluster>) {}
}

struct NoHooks;
impl LifecycleHooks for NoHooks {}

/// A loaded cluster.
pub struct Loaded {
    pub cluster: Arc<Cluster>,
    /// The catalog's description, refreshed with [`Node::update_spec`] (credential rotation).
    spec: parking_lot::RwLock<Arc<ClusterSpec>>,
    /// Milliseconds since the node started, at the last request.
    last_used: AtomicU64,
    in_flight: AtomicUsize,
    waiting: AtomicUsize,
    permits: Arc<Semaphore>,
    closed: AtomicBool,
}

impl Loaded {
    pub fn spec(&self) -> Arc<ClusterSpec> {
        self.spec.read().clone()
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

type Slot = Arc<OnceCell<Arc<Loaded>>>;

pub struct Node {
    pub catalog: Arc<dyn Catalog>,
    pub stores: Arc<dyn StoreProvider>,
    pub cfg: NodeConfig,
    hooks: Arc<dyn LifecycleHooks>,
    clusters: Mutex<HashMap<String, Slot>>,
    reaper: AtomicBool,
    epoch: Instant,
    /// Serializes loads and unloads of the same cluster. Entries live only while someone
    /// holds or waits for them ([`Transition`] removes the last one).
    transitions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
/// cluster loaded) until dropped, and the concurrency permit once [`Lease::acquire`]d.
pub struct Lease {
    pub loaded: Arc<Loaded>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    dialect: Dialect,
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
        self.loaded.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Lease {
    pub fn max_body_bytes(&self, node: &Node) -> usize {
        self.loaded.spec().max_body_bytes.unwrap_or(node.cfg.max_body_bytes)
    }

    pub async fn handle(&self, req: EsRequest) -> EsResponse {
        search_es::handle(&self.loaded.cluster, req).await
    }

    /// Whether the concurrency permit is held.
    pub fn has_permit(&self) -> bool {
        self.permit.is_some()
    }

    /// Takes a concurrency permit, waiting in the cluster's queue if all are in use; 429 when
    /// the queue is full too. Idempotent.
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
        self.permit = Some(permit);
        Ok(())
    }
}

/// Why a request was turned away before reaching its cluster.
#[derive(Debug)]
pub struct Rejected {
    pub error: EsError,
    pub dialect: Dialect,
}

const WWW_AUTHENTICATE: [&str; 3] = ["Basic realm=\"security\", charset=\"UTF-8\"", "Bearer realm=\"security\"", "ApiKey"];

fn unauthorized(reason: String) -> EsError {
    EsError::security(401, reason).with_header_values("WWW-Authenticate", WWW_AUTHENTICATE.iter().map(|s| s.to_string()).collect())
}

impl Node {
    pub fn new(catalog: Arc<dyn Catalog>, stores: Arc<dyn StoreProvider>, cfg: NodeConfig) -> Arc<Node> {
        Node::with_hooks(catalog, stores, cfg, Arc::new(NoHooks))
    }

    pub fn with_hooks(catalog: Arc<dyn Catalog>, stores: Arc<dyn StoreProvider>, cfg: NodeConfig, hooks: Arc<dyn LifecycleHooks>) -> Arc<Node> {
        Arc::new(Node {
            catalog,
            stores,
            cfg,
            hooks,
            clusters: Mutex::new(HashMap::new()),
            reaper: AtomicBool::new(false),
            epoch: Instant::now(),
            transitions: Mutex::new(HashMap::new()),
        })
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

    /// Names of the loaded clusters.
    pub fn loaded(&self) -> Vec<String> {
        let map = self.clusters.lock();
        map.iter().filter(|(_, s)| s.initialized()).map(|(k, _)| k.clone()).collect()
    }

    pub fn get_loaded(&self, name: &str) -> Option<Arc<Loaded>> {
        self.clusters.lock().get(name).and_then(|s| s.get().cloned())
    }

    /// Loads (or returns the loaded) cluster described by `spec`.
    pub async fn open(self: &Arc<Self>, spec: ClusterSpec) -> anyhow::Result<Arc<Loaded>> {
        let name = spec.name.clone();
        loop {
            let slot = self.clusters.lock().entry(name.clone()).or_default().clone();
            let node = self.clone();
            let spec2 = spec.clone();
            let loaded = slot
                .get_or_try_init(|| async move {
                    let cluster_name = spec2.name.clone();
                    let _t = node.transition(&cluster_name).await;
                    let store = node.stores.meta_store(&spec2.name).await?;
                    let mut cfg = ClusterConfig::new(&spec2.name);
                    cfg.dialect = spec2.dialect;
                    cfg.node_name = node.cfg.node_name.clone();
                    cfg.destructive_requires_name = spec2.destructive_requires_name;
                    cfg.node_settings = node.cfg.node_settings.clone();
                    cfg.test_features = node.cfg.test_features;
                    cfg.host_metrics = node.cfg.host_metrics;
                    let cluster = Arc::new(Cluster::open(cfg, store).await?);
                    node.hooks.loaded(&cluster).await;
                    tracing::debug!(cluster = spec2.name, "cluster loaded");
                    let permits = spec2.max_in_flight.max(1);
                    anyhow::Ok(Arc::new(Loaded {
                        cluster,
                        spec: parking_lot::RwLock::new(Arc::new(spec2)),
                        last_used: AtomicU64::new(node.now_ms()),
                        in_flight: AtomicUsize::new(0),
                        waiting: AtomicUsize::new(0),
                        permits: Arc::new(Semaphore::new(permits)),
                        closed: AtomicBool::new(false),
                    }))
                })
                .await;
            let loaded = match loaded {
                Ok(l) => l.clone(),
                Err(e) => {
                    // Don't leave an empty slot behind, unless another opener is still on it.
                    let mut map = self.clusters.lock();
                    if map.get(&name).is_some_and(|s| Arc::ptr_eq(s, &slot) && !s.initialized() && Arc::strong_count(s) == 2) {
                        map.remove(&name);
                    }
                    return Err(e);
                }
            };
            if loaded.closed.load(Ordering::SeqCst) {
                // Being unloaded: `unload` flips `closed` and removes the slot in one critical
                // section of the map lock (or backs off and clears it), so waiting for the lock
                // and looking again finds either a fresh slot or this one reopened.
                drop(self.clusters.lock());
                tokio::task::yield_now().await;
                continue;
            }
            self.ensure_reaper();
            return Ok(loaded);
        }
    }

    /// Clusters idle (no request in flight, none for `older_than`).
    pub fn idle(&self, older_than: Duration) -> Vec<String> {
        let now = self.now_ms();
        let map = self.clusters.lock();
        map.iter()
            .filter_map(|(k, s)| s.get().map(|l| (k, l)))
            .filter(|(_, l)| {
                let limit = l.spec().idle_unload.unwrap_or(older_than).as_millis() as u64;
                l.in_flight() == 0 && now.saturating_sub(l.last_used.load(Ordering::SeqCst)) >= limit
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Unloads a cluster unless requests are in flight. Returns whether it was unloaded.
    pub async fn unload(&self, name: &str) -> bool {
        let _t = self.transition(name).await;
        let loaded = {
            let mut map = self.clusters.lock();
            let Some(l) = map.get(name).and_then(|s| s.get().cloned()) else { return false };
            // Pairs with `enter`, which counts itself in flight before checking `closed`: mark
            // closed, then check in-flight (see the module docs).
            l.closed.store(true, Ordering::SeqCst);
            if l.in_flight() > 0 {
                l.closed.store(false, Ordering::SeqCst);
                return false;
            }
            map.remove(name);
            l
        };
        self.hooks.unloading(&loaded.cluster).await;
        self.stores.release(name).await;
        tracing::debug!(cluster = name, "cluster unloaded");
        true
    }

    /// Replaces a loaded cluster's description (new credentials, limits); unloaded clusters
    /// read the catalog when they wake anyway.
    pub fn update_spec(&self, spec: ClusterSpec) {
        if let Some(l) = self.get_loaded(&spec.name) {
            *l.spec.write() = Arc::new(spec);
        }
    }

    /// Makes `child` a branch of `parent` (metadata copy; segments are shared by the store).
    pub async fn branch(&self, parent: &str, child: &str) -> anyhow::Result<()> {
        self.stores.branch(parent, child).await
    }

    fn ensure_reaper(self: &Arc<Self>) {
        let Some(idle) = self.cfg.idle_unload else { return };
        if self.reaper.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        let tick = (idle / 4).clamp(Duration::from_millis(20), Duration::from_secs(15));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(node) = weak.upgrade() else { return };
                for name in node.idle(idle) {
                    node.unload(&name).await;
                }
                // Stop when nothing is loaded; the next load starts a new reaper.
                let map = node.clusters.lock();
                if map.values().all(|s| !s.initialized()) {
                    node.reaper.store(false, Ordering::SeqCst);
                    return;
                }
            }
        });
    }

    /// Whether the idle reaper is running (it only runs while clusters are loaded).
    pub fn reaper_running(&self) -> bool {
        self.reaper.load(Ordering::SeqCst)
    }

    /// Finds, authenticates and admits a request, permit included: the returned lease routes it
    /// to its cluster.
    pub async fn admit(self: &Arc<Self>, req: &EsRequest, sni: Option<&str>) -> Result<Lease, Rejected> {
        let mut lease = self.enter(req, sni).await?;
        lease.acquire(req).await?;
        Ok(lease)
    }

    /// Finds and authenticates a request and pins its cluster (loading it if needed), without
    /// taking a concurrency permit yet ([`Lease::acquire`]).
    pub async fn enter(self: &Arc<Self>, req: &EsRequest, sni: Option<&str>) -> Result<Lease, Rejected> {
        let reject = |error: EsError| Rejected { error, dialect: Dialect::Elasticsearch };
        let spec = if let Some(name) = &self.cfg.default_cluster {
            match self.get_loaded(name) {
                Some(l) => (*l.spec()).clone(),
                None => self.catalog.lookup(name).await.unwrap_or_else(|| ClusterSpec::new(name.clone())),
            }
        } else {
            let presented = auth::parse_authorization(req.header("authorization"))
                .map_err(|_| reject(unauthorized(format!("missing authentication credentials for REST request [{}]", req.uri))))?;
            let host_cluster = sni
                .and_then(|s| auth::host_label(s, self.cfg.domain.as_deref()))
                .or_else(|| req.header("host").and_then(|h| auth::host_label(h, self.cfg.domain.as_deref())))
                .map(str::to_string);
            let Some(claim) = auth::claim(&presented, host_cluster.as_deref()) else {
                return Err(reject(unauthorized(format!("missing authentication credentials for REST request [{}]", req.uri))));
            };
            let failed = || {
                let msg = match (&presented, presented.principal()) {
                    (Presented::None, _) => format!("missing authentication credentials for REST request [{}]", req.uri),
                    (Presented::ApiKey { .. }, _) => "unable to authenticate with provided credentials and anonymous access is not allowed for this request".to_string(),
                    (_, Some(p)) => format!("unable to authenticate user [{p}] for REST request [{}]", req.uri),
                    (_, None) => "unable to authenticate with provided credentials and anonymous access is not allowed for this request".to_string(),
                };
                reject(unauthorized(msg))
            };
            // A loaded cluster checks against its cached description; the catalog is only
            // asked when the cluster has to be woken.
            let spec = match self.get_loaded(&claim.cluster) {
                Some(l) => (*l.spec()).clone(),
                None => self.catalog.lookup(&claim.cluster).await.ok_or_else(failed)?,
            };
            if !auth::verify(&spec, &claim) {
                return Err(failed());
            }
            spec
        };
        let dialect = spec.dialect;
        loop {
            let loaded = self.open(spec.clone()).await.map_err(|e| Rejected {
                error: EsError::new(503, "cluster_block_exception", format!("cluster could not be loaded: {e}")),
                dialect,
            })?;
            // Count first, then check `closed` (the residency protocol in the module docs).
            loaded.in_flight.fetch_add(1, Ordering::SeqCst);
            let lease = Lease { loaded, permit: None, dialect };
            if lease.loaded.closed.load(Ordering::SeqCst) {
                // Unloaded (or being unloaded) under us: the lease's drop uncounts it; retry.
                drop(lease);
                tokio::task::yield_now().await;
                continue;
            }
            lease.loaded.last_used.store(self.now_ms(), Ordering::SeqCst);
            return Ok(lease);
        }
    }

    /// Handles a complete request (tests and embedders without the HTTP layer).
    pub async fn handle(self: &Arc<Self>, req: EsRequest) -> EsResponse {
        match self.admit(&req, None).await {
            Ok(lease) => {
                let max = lease.max_body_bytes(self);
                if req.body.len() > max {
                    return search_es::rest::error_response(&EsError::content_too_long(req.body.len() as u64, max as u64), &req, lease.loaded.spec().dialect);
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
        tokio::time::timeout(Duration::from_secs(5), async {
            while !n.loaded().is_empty() || n.reaper_running() {
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

    #[tokio::test]
    async fn failed_opens_leave_nothing_behind() {
        let catalog = Arc::new(StaticCatalog::new());
        catalog.insert(ClusterSpec::new("c"));
        let n = Node::new(catalog, Arc::new(FailingStores), NodeConfig { default_cluster: Some("c".into()), ..NodeConfig::default() });
        let r = n.handle(req("/", None)).await;
        assert_eq!(r.status, 503);
        assert!(n.clusters.lock().is_empty());
        assert_eq!(n.transition_locks(), 0);
    }
}

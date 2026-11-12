//! roll storage on many nodes (docs/storage/architecture.md, "Many nodes").
//!
//! One node at a time owns the deployment's metadata: it holds the lease [`KEY`] (roll-cluster),
//! writes the metadata log and serves every request. Every node accepts S3, developer-API and
//! control-plane requests and hands them to the owner through roll-cluster, streaming bodies
//! both ways; while the metadata moves, the node a client reached keeps a request whose body it
//! could buffer and sends it again, and answers the others with a 503 SDKs retry.
//!
//! - **Standbys.** The nodes most likely to own the metadata next (the next one by placement,
//!   and the one [`Cluster::standby`] places in another zone) follow the log
//!   ([`s3_store::Follower`]), so taking over reads only what's new.
//! - **A tenure** starts when this node's lease is granted: the store takes over the log from the
//!   standby's copy (or from a snapshot), the product's services are built on it ([`Open`]), and
//!   requests are served. It ends when the lease starts moving (a handoff: requests in flight
//!   finish, the store commits everything and writes its handoff entry, then the lease goes) or
//!   is lost (fenced, or another node took it).
//! - **Fencing.** Correctness never depends on the lease: the log's entries are create-only, so a
//!   deposed owner's next commit fails and it stops serving (`s3_store::Store::take_over`).
//!
//! On a node alone (the memory backend) the node owns the metadata from the start and nothing is
//! forwarded or followed: rolld behaves as it always did.

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, OriginalUri, Request};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures::future::BoxFuture;
use roll_cluster::{Cluster, Forwarded, Guard, Route, placement};
use s3_store::{Backend, Follower, Store, StoreConfig};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use tower::ServiceExt;
use tracing::{info, warn};

/// The lease of the deployment's metadata.
pub const KEY: &str = "storage:meta";

/// Where a forwarded request was going: its path and query, as the client sent them. The hop
/// itself goes to a fixed internal path, so nothing on the way normalizes what S3 signed.
const URI_HEADER: &str = "x-roll-storage-uri";

/// The client's `Content-Length` (S3 signs it; a forwarder may send a buffered body without
/// one, an empty body above all).
const LENGTH_HEADER: &str = "x-roll-storage-content-length";

/// Request bodies up to this size are kept so the request can be sent again while the metadata
/// moves.
const RETRY_BODY: u64 = 4 << 20;

/// What the metadata's owner serves, built on the store it took over.
pub struct Services {
    /// The S3 API.
    pub s3: Router,
    /// The developer API (`/storage/v1`).
    pub api: Option<Router>,
    /// The control plane (`/v1/storage`).
    pub control: Option<Router>,
}

/// Builds a tenure's [`Services`] once its store is open.
pub type Open = Arc<dyn Fn(Arc<Store>) -> BoxFuture<'static, anyhow::Result<Services>> + Send + Sync>;

/// Which of storage's APIs a request is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    S3,
    Api,
    Control,
}

impl Surface {
    fn internal_path(self) -> &'static str {
        match self {
            Surface::S3 => "/internal/storage/s3",
            Surface::Api => "/internal/storage/api",
            Surface::Control => "/internal/storage/control",
        }
    }
}

#[derive(Clone)]
pub struct NodeConfig {
    /// The store's settings; `dir` is where this node keeps its copies of the metadata.
    pub store: StoreConfig,
    /// How often a standby reads the log's next entry while nothing is new.
    pub follow_every: Duration,
    /// How often it also checks the log's pruned mark, gaps and snapshots.
    pub check_every: Duration,
    /// How long a request that reached the owner waits for its store to open.
    pub open_wait: Duration,
    /// This node is alone (rolld without a cluster): it owns the metadata from the start, and
    /// failing to open it fails [`StorageNode::start`].
    pub single: bool,
}

impl NodeConfig {
    pub fn new(store: StoreConfig) -> Self {
        Self { store, follow_every: Duration::from_millis(500), check_every: Duration::from_secs(60), open_wait: Duration::from_secs(10), single: false }
    }
}

/// What this node is doing for the metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct NodeStatus {
    /// This node owns the metadata and serves it.
    pub owner: bool,
    /// The lease epoch it serves under.
    pub epoch: Option<i64>,
    /// As a standby: the last log entry its copy holds.
    pub following: Option<u64>,
    /// Tenures this node started.
    pub tenures: u64,
}

/// This node's part in serving storage. See the module docs.
pub struct StorageNode {
    cluster: Arc<Cluster>,
    backend: Arc<dyn Backend>,
    cfg: NodeConfig,
    open: Open,
    live: watch::Sender<Option<Arc<Tenure>>>,
    status: Mutex<NodeStatus>,
    wake: Notify,
    stopped: AtomicBool,
    copies: AtomicU64,
}

/// One tenure as the metadata's owner.
struct Tenure {
    epoch: i64,
    store: Arc<Store>,
    services: Services,
    inflight: AtomicUsize,
    closing: AtomicBool,
    idle: Notify,
}

impl Tenure {
    /// Count a request in, unless the tenure is closing (the closer either sees it or it sees
    /// the closer: both sides use sequentially consistent operations).
    fn enter(self: &Arc<Self>) -> Option<Entered> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        let entered = Entered(self.clone());
        (!self.closing.load(Ordering::SeqCst) && self.store.is_open()).then_some(entered)
    }

    /// Stop taking requests and wait (up to `limit`) for the ones in flight.
    async fn quiesce(&self, limit: Duration) {
        self.closing.store(true, Ordering::SeqCst);
        let until = tokio::time::Instant::now() + limit;
        loop {
            let idle = self.idle.notified();
            if self.inflight.load(Ordering::SeqCst) == 0 {
                return;
            }
            if tokio::time::timeout_at(until, idle).await.is_err() {
                warn!(inflight = self.inflight.load(Ordering::SeqCst), "storage: requests still running as the metadata moves");
                return;
            }
        }
    }
}

struct Entered(Arc<Tenure>);

impl Drop for Entered {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl StorageNode {
    /// Take part in serving storage on `cluster`. A node alone ([`NodeConfig::single`]) opens
    /// the store before this returns, and fails if it can't, as a single node always did; in a
    /// cluster the store opens whenever this node gets the lease.
    pub async fn start(cluster: Arc<Cluster>, backend: Arc<dyn Backend>, cfg: NodeConfig, open: Open) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.store.dir)?;
        // Copies a previous process left (the local database is only a cache).
        for name in ["meta.db", "meta.db-wal", "meta.db-shm", "meta.db-journal"] {
            let _ = std::fs::remove_file(cfg.store.dir.join(name));
        }
        let _ = std::fs::remove_dir_all(cfg.store.dir.join("snapshots"));
        let node = Arc::new(Self {
            cluster,
            backend,
            cfg,
            open,
            live: watch::channel(None).0,
            status: Mutex::new(NodeStatus::default()),
            wake: Notify::new(),
            stopped: AtomicBool::new(false),
            copies: AtomicU64::new(0),
        });
        if node.cfg.single {
            let guard = node.cluster.lease(KEY).await?.ok_or_else(|| anyhow::anyhow!("storage: this node can't take the metadata's lease"))?;
            node.open_tenure(guard, None).await?;
        }
        tokio::spawn(node.clone().run());
        Ok(node)
    }

    pub fn status(&self) -> NodeStatus {
        self.status.lock().unwrap().clone()
    }

    /// The store, while this node owns the metadata.
    pub fn store(&self) -> Option<Arc<Store>> {
        self.live.borrow().as_ref().map(|t| t.store.clone())
    }

    /// A router answering `surface`'s requests on any node: they run on the metadata's owner.
    pub fn router(self: &Arc<Self>, surface: Surface) -> Router {
        let node = self.clone();
        Router::new().fallback(move |req: Request| {
            let node = node.clone();
            async move { node.front(surface, req).await }
        })
    }

    /// The control plane's routes (`/v1/storage` and under it), to merge into the API router.
    pub fn control_routes(self: &Arc<Self>) -> Router {
        let h = |node: Arc<Self>| {
            any(move |req: Request| {
                let node = node.clone();
                async move { node.front(Surface::Control, req).await }
            })
        };
        // Nested, not a `{*rest}` route: a param the outer route matched stays in the request, and
        // the control router's own `Path` extractors then see one too many (every
        // `/v1/storage/buckets/{name}/…` answered 500). Axum leaves a nest's tail out.
        Router::new().nest_service("/v1/storage", h(self.clone()))
    }

    /// `/internal/storage/{s3,api,control}`: requests other nodes forwarded to this one
    /// (signed), to merge into the API router other nodes reach.
    pub fn internal_routes(self: &Arc<Self>) -> Router {
        let h = |node: Arc<Self>, surface: Surface| {
            any(move |req: Request| {
                let node = node.clone();
                async move { node.forwarded(surface, req).await }
            })
        };
        Router::new()
            .route(Surface::S3.internal_path(), h(self.clone(), Surface::S3))
            .route(Surface::Api.internal_path(), h(self.clone(), Surface::Api))
            .route(Surface::Control.internal_path(), h(self.clone(), Surface::Control))
            .route_layer(axum::middleware::from_fn_with_state(self.cluster.clone(), roll_cluster::require_signed))
    }

    /// Stop: commit and close the store if this node still owns the metadata (a cluster node
    /// has handed it off by now, in its drain).
    pub async fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        let tenure = self.live.send_replace(None);
        if let Some(t) = tenure {
            t.quiesce(Duration::from_secs(10)).await;
            if self.cfg.single {
                t.store.close().await;
            } else {
                t.store.hand_off().await;
            }
        }
    }

    /// Test hook: stop as a killed process would, mid-tenure (the store isn't closed; it keeps
    /// committing what's in flight until a new owner's takeover fences it).
    #[doc(hidden)]
    pub fn fault_kill(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        self.live.send_replace(None);
        *self.status.lock().unwrap() = NodeStatus::default();
    }

    // ── ownership ──

    async fn run(self: Arc<Self>) {
        let mut events = self.cluster.subscribe_ownership();
        let mut follower: Option<(Follower, PathBuf)> = None;
        let mut last_check = Instant::now();
        let mut retry_at = Instant::now();
        while !self.stopped.load(Ordering::SeqCst) {
            let owner = self.live.borrow().is_some();
            if !owner && Instant::now() >= retry_at {
                if self.cluster.owns(KEY) {
                    if let Some(g) = self.cluster.guard(KEY) {
                        if let Err(e) = self.open_tenure(g, follower.take()).await {
                            warn!(error = %e, "storage: opening the metadata failed; trying again");
                            retry_at = Instant::now() + Duration::from_secs(1);
                        }
                        continue;
                    }
                } else if self.cluster.preferred(KEY).is_some_and(|n| n.id == self.cluster.node_id()) {
                    // Nobody may hold it yet (a new deployment, or every node restarted): take it.
                    if let Ok(Route::Local(_)) = self.cluster.route_async(KEY).await {
                        continue;
                    }
                }
            }
            if self.live.borrow().is_none() {
                if self.should_follow() {
                    self.follow(&mut follower, &mut last_check).await;
                } else if follower.take().is_some() {
                    info!("storage: no longer a standby of the metadata");
                    self.status.lock().unwrap().following = None;
                }
            }
            tokio::select! {
                Some(_) = events.recv() => {}
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.cfg.follow_every) => {}
            }
        }
    }

    /// Whether this node keeps a copy of the metadata current: it's among the two nodes placement
    /// prefers for the lease, or the standby in another zone.
    fn should_follow(&self) -> bool {
        let me = self.cluster.node_id();
        let mut nodes = self.cluster.nodes();
        if nodes.len() < 2 {
            return false;
        }
        nodes.sort_by_key(|n| std::cmp::Reverse((placement::weight(KEY, &n.id), n.id.clone())));
        nodes.iter().take(2).any(|n| n.id == me) || self.cluster.standby(KEY).is_some_and(|n| n.id == me)
    }

    /// A fresh directory for a copy of the metadata; the others go.
    fn new_dir(&self) -> PathBuf {
        let n = self.copies.fetch_add(1, Ordering::SeqCst);
        let base = &self.cfg.store.dir;
        let name = format!("meta-{n}");
        for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
            let old = e.file_name().to_string_lossy().into_owned();
            if old.starts_with("meta-") && old != name {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        base.join(name)
    }

    fn store_config(&self, dir: PathBuf) -> StoreConfig {
        StoreConfig { dir, ..self.cfg.store.clone() }
    }

    async fn follow(&self, follower: &mut Option<(Follower, PathBuf)>, last_check: &mut Instant) {
        match follower {
            None => {
                let dir = self.new_dir();
                match Follower::start(&self.store_config(dir.clone()), self.backend.clone()).await {
                    Ok(f) => {
                        info!(seq = f.seq(), "storage: standby of the metadata");
                        self.status.lock().unwrap().following = Some(f.seq());
                        *last_check = Instant::now();
                        *follower = Some((f, dir));
                    }
                    Err(e) => warn!(error = %e, "storage: copying the metadata for a standby failed"),
                }
            }
            Some((f, _)) => {
                let check = last_check.elapsed() >= self.cfg.check_every;
                if check {
                    *last_check = Instant::now();
                }
                match f.poll(check).await {
                    Ok(_) => self.status.lock().unwrap().following = Some(f.seq()),
                    Err(e) => {
                        warn!(error = %e, "storage: the standby's copy of the metadata can't follow the log; copying it again");
                        *follower = None;
                    }
                }
            }
        }
    }

    async fn open_tenure(self: &Arc<Self>, guard: Guard, follower: Option<(Follower, PathBuf)>) -> anyhow::Result<()> {
        let started = Instant::now();
        let hot = follower.is_some();
        let (follower, dir) = match follower {
            Some((f, d)) => (Some(f), d),
            None => (None, self.new_dir()),
        };
        self.status.lock().unwrap().following = None;
        let store = Store::take_over(self.store_config(dir), self.backend.clone(), follower).await?;
        let services = match (self.open)(store.clone()).await {
            Ok(s) => s,
            Err(e) => {
                store.hand_off().await;
                return Err(e);
            }
        };
        if !guard.is_valid() {
            store.hand_off().await;
            anyhow::bail!("the metadata's lease moved while its store opened");
        }
        let tenure = Arc::new(Tenure { epoch: guard.epoch(), store, services, inflight: AtomicUsize::new(0), closing: AtomicBool::new(false), idle: Notify::new() });
        self.live.send_replace(Some(tenure.clone()));
        {
            let mut s = self.status.lock().unwrap();
            s.owner = true;
            s.epoch = Some(tenure.epoch);
            s.tenures += 1;
        }
        info!(epoch = tenure.epoch, standby = hot, took_ms = started.elapsed().as_millis() as u64, "storage: this node owns the metadata");
        tokio::spawn(self.clone().hold(tenure, guard));
        Ok(())
    }

    /// Hold the lease's guard for the tenure, so a handoff waits for the store to close. Ends
    /// when the lease starts moving or is lost, or the store stops serving (fenced).
    async fn hold(self: Arc<Self>, t: Arc<Tenure>, guard: Guard) {
        let ending = guard.ending();
        tokio::pin!(ending);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let fenced = loop {
            tokio::select! {
                _ = &mut ending => break false,
                _ = tick.tick() => if !t.store.is_open() { break true },
            }
        };
        // Within the handoff's patience, so the store has closed before the lease moves.
        t.quiesce(self.cluster.config().handoff_timeout.mul_f32(0.6)).await;
        let clean = !fenced && t.store.hand_off().await;
        self.live.send_if_modified(|l| {
            let mine = l.as_ref().is_some_and(|x| Arc::ptr_eq(x, &t));
            if mine {
                *l = None;
            }
            mine
        });
        {
            let mut s = self.status.lock().unwrap();
            s.owner = false;
            s.epoch = None;
        }
        info!(epoch = t.epoch, clean, fenced, "storage: this node gave the metadata up");
        drop(guard);
        self.wake.notify_one();
    }

    /// The tenure serving under `epoch`, waiting a little for it to open.
    async fn tenure_for(&self, epoch: i64) -> Option<Arc<Tenure>> {
        let mut rx = self.live.subscribe();
        let until = tokio::time::Instant::now() + self.cfg.open_wait;
        loop {
            let current = rx.borrow_and_update().clone();
            if let Some(t) = current.filter(|t| t.epoch == epoch) {
                return Some(t);
            }
            match tokio::time::timeout_at(until, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    // ── requests ──

    /// A request from a client: run it on the owner, riding out a move.
    async fn front(self: Arc<Self>, surface: Surface, req: Request) -> Response {
        let req = match internal(surface, req) {
            Ok(r) => r,
            Err(r) => return *r,
        };
        let resp = self.ride(surface, req).await;
        if unavailable_for(&resp).is_some() {
            return retry_later(surface, &resp);
        }
        if resp.headers().contains_key(roll_cluster::FORWARD_FAILED) {
            return outcome_unknown(surface);
        }
        resp
    }

    /// The node a client reached keeps a request whose body it could buffer and routes it
    /// again while the metadata has no owner that can serve it (a handoff, or an owner that
    /// died and whose lease hasn't run out), for about a lease's length. A request with a bigger
    /// body waits until there's an owner to send it to, and gets one try.
    async fn ride(self: &Arc<Self>, surface: Surface, req: Request) -> Response {
        let cfg = self.cluster.config();
        let deadline = Instant::now() + cfg.lease_ttl + cfg.sweep_every * 2 + Duration::from_secs(1);
        let pause = |w: Duration| w.min(cfg.max_retry_wait).max(Duration::from_millis(20));
        let small = http_body::Body::size_hint(req.body()).exact().is_some_and(|n| n <= RETRY_BODY);
        if !small {
            loop {
                match self.cluster.route_async(KEY).await {
                    Ok(Route::Unavailable { retry_after }) if Instant::now() + retry_after < deadline => tokio::time::sleep(pause(retry_after)).await,
                    _ => break,
                }
            }
            return self.dispatch(surface, req).await;
        }
        let (parts, body) = req.into_parts();
        let body = match axum::body::to_bytes(body, RETRY_BODY as usize).await {
            Ok(b) => b,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("reading the request body failed: {e}\n")).into_response(),
        };
        loop {
            let resp = self.dispatch(surface, Request::from_parts(parts.clone(), Body::from(body.clone()))).await;
            let wait = match unavailable_for(&resp) {
                Some(w) => w,
                // Two nodes disagreeing for a moment about where the metadata belongs.
                None if resp.status() == StatusCode::LOOP_DETECTED => {
                    self.cluster.forget(KEY);
                    Duration::from_millis(50)
                }
                None => return resp,
            };
            if Instant::now() + wait >= deadline {
                return resp;
            }
            tokio::time::sleep(pause(wait)).await;
        }
    }

    async fn dispatch(self: &Arc<Self>, surface: Surface, req: Request) -> Response {
        let node = self.clone();
        self.cluster.dispatch(KEY, req, move |req, guard| node.local(surface, req, guard)).await
    }

    /// A request another node forwarded: run it here, or pass it on if the metadata moved.
    async fn forwarded(self: Arc<Self>, surface: Surface, req: Request) -> Response {
        if let Some(from) = req.extensions().get::<Forwarded>().map(|f| f.node.clone())
            && let Ok(Route::Forward(n)) = self.cluster.route_async(KEY).await
            && n.id == from
        {
            // Never back where it came from: the two nodes disagree right now, and the first
            // one tries again shortly.
            self.cluster.forget(KEY);
            return roll_cluster::unavailable(Duration::from_millis(50));
        }
        self.dispatch(surface, req).await
    }

    /// Serve a request here, as the owner (`guard` holds the lease while it runs).
    async fn local(self: Arc<Self>, surface: Surface, req: Request, guard: Guard) -> Response {
        let retry = self.cluster.config().sweep_every;
        let Some(t) = self.tenure_for(guard.epoch()).await else { return roll_cluster::unavailable(Duration::from_secs(1)) };
        let Some(entered) = t.enter() else { return roll_cluster::unavailable(retry) };
        if !guard.is_valid() {
            return roll_cluster::unavailable(retry);
        }
        let router = match surface {
            Surface::S3 => Some(t.services.s3.clone()),
            Surface::Api => t.services.api.clone(),
            Surface::Control => t.services.control.clone(),
        };
        let Some(router) = router else { return StatusCode::NOT_FOUND.into_response() };
        let req = match external(req) {
            Ok(r) => r,
            Err(r) => return *r,
        };
        let resp = match router.oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        // The response's head is ready: whatever its body still streams (an object's bytes,
        // from an immutable blob) needs neither the lease nor the log.
        drop(entered);
        drop(guard);
        resp
    }
}

fn bad_request(msg: &str) -> Box<Response> {
    Box::new((StatusCode::BAD_REQUEST, format!("{msg}\n")).into_response())
}

/// Address a client's request to the internal route of its surface; its own path and query
/// travel in a header.
fn internal(surface: Surface, mut req: Request) -> Result<Request, Box<Response>> {
    let original = req.extensions().get::<OriginalUri>().map_or(req.uri(), |o| &o.0).clone();
    let pq = original.path_and_query().map_or("/", |p| p.as_str());
    let value = HeaderValue::from_str(pq).map_err(|_| bad_request("bad request path"))?;
    // HTTP/2 names the host in the URI only.
    if !req.headers().contains_key(header::HOST)
        && let Some(a) = original.authority().and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    {
        req.headers_mut().insert(header::HOST, a);
    }
    req.headers_mut().insert(URI_HEADER, value);
    match req.headers().get(header::CONTENT_LENGTH).cloned() {
        Some(n) => req.headers_mut().insert(LENGTH_HEADER, n),
        None => req.headers_mut().remove(LENGTH_HEADER),
    };
    req.extensions_mut().remove::<OriginalUri>();
    *req.uri_mut() = Uri::from_static(surface.internal_path());
    Ok(req)
}

/// Undo [`internal`] on the owner, with the client's address as the first node saw it.
fn external(mut req: Request) -> Result<Request, Box<Response>> {
    let pq = req.headers_mut().remove(URI_HEADER).ok_or_else(|| bad_request("a storage request without its path"))?;
    let uri: Uri = pq.to_str().ok().and_then(|s| s.parse().ok()).ok_or_else(|| bad_request("bad request path"))?;
    *req.uri_mut() = uri;
    if let Some(n) = req.headers_mut().remove(LENGTH_HEADER) {
        req.headers_mut().insert(header::CONTENT_LENGTH, n);
    }
    req.extensions_mut().remove::<OriginalUri>();
    let client = req.extensions().get::<Forwarded>().map(|f| f.client.clone());
    if let Some(client) = client {
        match client.and_then(|c| c.parse::<IpAddr>().ok()) {
            Some(ip) => {
                req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ip, 0)));
            }
            None => {
                req.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
            }
        }
    }
    Ok(req)
}

/// How long to wait before routing again, if `resp` says nothing ran because the metadata
/// can't be served right now.
fn unavailable_for(resp: &Response) -> Option<Duration> {
    if resp.status() != StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }
    let ms = resp.headers().get("x-roll-code-unavailable")?.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_millis(ms))
}

/// The answer a client gets when the metadata stayed unavailable: a 503 its SDK retries, in the
/// surface's own error format.
fn retry_later(surface: Surface, resp: &Response) -> Response {
    let after = resp.headers().get(header::RETRY_AFTER).cloned().unwrap_or(HeaderValue::from_static("1"));
    let message = "storage is moving between nodes; retry shortly";
    let (content_type, body) = match surface {
        Surface::S3 => {
            let mut id = [0u8; 8];
            let _ = getrandom::fill(&mut id);
            ("application/xml", s3_proto::error::S3Error::new(s3_proto::error::Code::ServiceUnavailable).to_xml(&hex::encode_upper(id)))
        }
        Surface::Api => ("application/json", serde_json::json!({ "statusCode": "503", "code": "ServiceUnavailable", "error": "Service Unavailable", "message": message }).to_string()),
        Surface::Control => ("application/json", serde_json::json!({ "error": message }).to_string()),
    };
    let mut r = (StatusCode::SERVICE_UNAVAILABLE, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r.headers_mut().insert(header::RETRY_AFTER, after);
    r
}

/// The answer a client gets when its request reached the owner and the owner went away before
/// answering (it may have run): S3's `InternalError`, which every SDK retries (storage's writes are
/// safe to retry: a retried PUT writes the object again), in the surface's own error format.
fn outcome_unknown(surface: Surface) -> Response {
    let message = "storage's node went away while serving the request; retry it";
    let (status, content_type, body) = match surface {
        Surface::S3 => {
            let mut id = [0u8; 8];
            let _ = getrandom::fill(&mut id);
            (StatusCode::INTERNAL_SERVER_ERROR, "application/xml", s3_proto::error::S3Error::new(s3_proto::error::Code::InternalError).to_xml(&hex::encode_upper(id)))
        }
        Surface::Api => (StatusCode::INTERNAL_SERVER_ERROR, "application/json", serde_json::json!({ "statusCode": "500", "code": "InternalError", "error": "Internal Server Error", "message": message }).to_string()),
        Surface::Control => (StatusCode::BAD_GATEWAY, "application/json", serde_json::json!({ "error": message }).to_string()),
    };
    let mut r = (status, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_whose_owner_went_away_gets_the_s3_error_sdks_retry() {
        let r = outcome_unknown(Surface::S3);
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/xml");
    }

    #[test]
    fn requests_travel_with_their_own_path_and_host() {
        let req = Request::builder().method("PUT").uri("http://b.s3.example/a/../b%20c?uploads&x=1").header("content-length", "0").body(Body::empty()).unwrap();
        let mut hop = internal(Surface::S3, req).unwrap();
        assert_eq!(hop.uri(), "/internal/storage/s3");
        assert_eq!(hop.headers()[header::HOST], "b.s3.example");
        // What a forwarder may do to an empty body.
        hop.headers_mut().remove(header::CONTENT_LENGTH);
        let back = external(hop).unwrap();
        assert_eq!(back.uri(), "/a/../b%20c?uploads&x=1");
        assert_eq!(back.headers()[header::CONTENT_LENGTH], "0");
        assert!(!back.headers().contains_key(URI_HEADER) && !back.headers().contains_key(LENGTH_HEADER));
        assert!(external(Request::new(Body::empty())).is_err());
    }
}

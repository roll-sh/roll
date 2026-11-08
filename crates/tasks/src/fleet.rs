//! roll tasks on a roll-cluster fleet (rolld; docs/cloud/README.md, "Ownership keys"): the
//! log-store cell of [`crate::cluster`], with its leader, standbys and forwarding taken from the
//! node's cluster instead of the cell's own S3 lease and health checks.
//!
//! - **The leader** holds the lease [`KEY`]. A tenure starts when this node is granted it: the
//!   log is taken over (from this node's copy when it's a standby, else rebuilt from snapshots),
//!   every tenant's engine starts ([`Cell`]) and requests are served. It ends when the lease
//!   starts moving (a handoff) or is lost (fenced, or another node took it).
//! - **Standbys.** The two nodes placement prefers for the lease, and the one
//!   [`Cluster::standby`] places in another zone, keep a copy of every tenant current
//!   ([`Follower`]), so taking over reads only what's new.
//! - **Requests.** Every node accepts tasks requests (rolld's API router, the edge's tasks host)
//!   and hands them to the leader through roll-cluster. While the cell moves, the node a client
//!   reached holds the request and sends it again, for up to [`NodeConfig::ride`]. Only answers
//!   that say nothing ran are retried: roll-cluster's "unavailable", and the engine's "stepped
//!   down" (a write the log refused because this node stopped leading; nothing was committed).
//! - **Handoff** (a drain, rebalancing): roll-cluster stops granting guards, so new requests
//!   wait at the node they reached; long polls and run waits are sent on; deliveries in flight
//!   finish (up to the engine's `drain_ms`) or are handed back; usage is recorded; the log
//!   commits everything, relaxed writes included, and closes; then the lease goes. Nothing
//!   acknowledged is lost and no request fails.
//! - **Crash.** The lease runs out and a standby takes over from its copy. Everything the dead
//!   leader acknowledged is there. Its relaxed bookkeeping of the last `relaxed_ms` (delivery
//!   outcomes, leases, workflow steps, schedule ticks, usage) is lost, so those deliveries
//!   happen again (at-least-once).
//! - **Fencing.** Correctness never depends on the lease: log entries are create-only and a new
//!   leader writes a takeover entry before serving, so a deposed leader's next commit fails and
//!   it stops ([`crate::store::log`]).
//!
//! On a node alone (the memory backend) the node leads from the start and nothing is forwarded
//! or followed.

use crate::engine::Config;
use crate::store::log::{Follower, Log, LogConfig};
use crate::tenants::Cell;
use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, Request};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use roll_cluster::{Cluster, Forwarded, Guard, Route, placement};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use tower::ServiceExt;
use tracing::{info, warn};

/// The lease of the cell (one per deployment for now; docs/cloud/README.md says what spreading
/// tenants over cells needs).
pub const KEY: &str = "tasks:cell:default";

/// The client's path and query on a forwarded request (the hop goes to [`INTERNAL`]).
const URI_HEADER: &str = "x-roll-tasks-uri";
const INTERNAL: &str = "/internal/tasks";

#[derive(Clone)]
pub struct NodeConfig {
    /// The log; `dir` is where this node keeps its copies of the cell's data (a cache).
    pub log: LogConfig,
    /// Every tenant's engine settings. Keep `drain_ms` well under the cluster's
    /// `handoff_timeout`: a handoff waits for deliveries in flight that long.
    pub tasks: Config,
    /// How often a standby looks for new log entries.
    pub follow_every: Duration,
    /// How often it also checks the log wasn't pruned past its copy.
    pub check_every: Duration,
    /// How long a request that reached the leader waits for its tenure to open.
    pub open_wait: Duration,
    /// How long the node a client reached holds a request while the cell has no leader that
    /// can serve it (a handoff, or a leader that died and whose lease hasn't run out).
    pub ride: Duration,
    /// Largest request accepted (bodies are kept so a request can be sent again).
    pub max_request: usize,
    /// The operator's token (rolld's `ROLL_ADMIN_TOKEN`): requests carrying it act as the
    /// operator (`roll_auth::Operator`) on the leader, wherever they came in.
    pub operator: Option<roll_auth::AdminToken>,
    /// This node is alone (rolld without a cluster): it leads from the start, and failing to
    /// open the log fails [`TasksNode::start`].
    pub single: bool,
}

impl NodeConfig {
    pub fn new(log: LogConfig, tasks: Config) -> Self {
        let max_request = tasks.max_body_bytes + (1 << 20);
        Self {
            log,
            tasks,
            follow_every: Duration::from_millis(500),
            check_every: Duration::from_secs(30),
            open_wait: Duration::from_secs(10),
            ride: Duration::from_secs(25),
            max_request,
            operator: None,
            single: false,
        }
    }
}

/// What this node is doing for the cell.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct NodeStatus {
    /// This node leads the cell and serves it.
    pub leader: bool,
    /// The lease epoch it leads under.
    pub epoch: Option<i64>,
    /// As a standby: the last log entry its copy holds.
    pub following: Option<u64>,
    /// Tenures this node started.
    pub tenures: u64,
}

/// This node's part in serving roll tasks. See the module docs.
pub struct TasksNode {
    cluster: Arc<Cluster>,
    cfg: NodeConfig,
    live: watch::Sender<Option<Arc<Tenure>>>,
    status: Mutex<NodeStatus>,
    wake: Notify,
    stopped: AtomicBool,
    copies: AtomicU64,
}

/// One tenure as the cell's leader.
struct Tenure {
    epoch: i64,
    log: Arc<Log>,
    cell: Arc<Cell>,
    router: Router,
    inflight: AtomicUsize,
    closing: AtomicBool,
    idle: Notify,
    ended: tokio::sync::OnceCell<()>,
}

impl Tenure {
    /// Count a request in, unless the tenure is closing (the closer either sees it or it sees
    /// the closer: both sides use sequentially consistent operations).
    fn enter(self: &Arc<Self>) -> Option<Entered> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        let entered = Entered(self.clone());
        (!self.closing.load(Ordering::SeqCst) && self.log.is_open()).then_some(entered)
    }

    /// Wait (up to `limit`) for the requests in flight.
    async fn quiesce(&self, limit: Duration) {
        let until = tokio::time::Instant::now() + limit;
        loop {
            let idle = self.idle.notified();
            if self.inflight.load(Ordering::SeqCst) == 0 {
                return;
            }
            if tokio::time::timeout_at(until, idle).await.is_err() {
                warn!(inflight = self.inflight.load(Ordering::SeqCst), "tasks: requests still running as the cell moves");
                return;
            }
        }
    }

    /// End the tenure, once: stop taking requests, send long waits on, let deliveries and
    /// requests in flight finish, then commit everything and close the log. A fenced tenure
    /// (another node took the log) just stops.
    async fn end(&self, fenced: bool, limit: Duration) {
        self.ended
            .get_or_init(|| async {
                self.closing.store(true, Ordering::SeqCst);
                self.cell.stop_waiting();
                if fenced {
                    self.cell.kill();
                    return;
                }
                futures::future::join(self.cell.shutdown(), self.quiesce(limit)).await;
                self.cell.flush_usage().await;
                self.log.close().await;
            })
            .await;
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

impl TasksNode {
    /// Take part in serving roll tasks on `cluster`. A node alone ([`NodeConfig::single`])
    /// opens the log before this returns, and fails if it can't; in a cluster it opens whenever
    /// this node gets the lease.
    pub async fn start(cluster: Arc<Cluster>, cfg: NodeConfig) -> anyhow::Result<Arc<Self>> {
        // Copies a previous process left (only a cache).
        let _ = std::fs::remove_dir_all(&cfg.log.dir);
        std::fs::create_dir_all(&cfg.log.dir)?;
        let node = Arc::new(Self {
            cluster,
            cfg,
            live: watch::channel(None).0,
            status: Mutex::new(NodeStatus::default()),
            wake: Notify::new(),
            stopped: AtomicBool::new(false),
            copies: AtomicU64::new(0),
        });
        if node.cfg.single {
            let guard = node.cluster.lease(KEY).await?.ok_or_else(|| anyhow::anyhow!("tasks: this node can't take the cell's lease"))?;
            node.open_tenure(guard, None).await?;
        }
        tokio::spawn(node.clone().run());
        Ok(node)
    }

    pub fn status(&self) -> NodeStatus {
        self.status.lock().unwrap().clone()
    }

    /// The cell, while this node leads it.
    pub fn cell(&self) -> Option<Arc<Cell>> {
        self.live.borrow().as_ref().map(|t| t.cell.clone())
    }

    /// The log's numbers, while this node leads the cell.
    pub fn log_stats(&self) -> Option<crate::store::log::LogStats> {
        self.live.borrow().as_ref().map(|t| t.log.stats())
    }

    /// Every tasks request on any node (`/v1/tasks/…`, `/t/<tenant>/v1/tasks/…`,
    /// `/_roll/tenants…`), run on the leader: for the edge's tasks host. Real routes rather than
    /// a fallback, so an access gate in front sees which API a request is for.
    pub fn router(self: &Arc<Self>) -> Router {
        let h = |node: Arc<Self>| {
            any(move |req: Request| {
                let node = node.clone();
                async move { node.front(req).await }
            })
        };
        let mut r = Router::new();
        for path in ["/v1/tasks/health", "/v1/tasks/{*rest}", "/t/{tenant}/v1/tasks/{*rest}", "/_roll/tenants", "/_roll/tenants/{*rest}"] {
            r = r.route(path, h(self.clone()));
        }
        r
    }

    /// The same routes plus the one other nodes forward requests to (signed): for rolld's API
    /// router.
    pub fn api_routes(self: &Arc<Self>) -> Router {
        self.router().merge(self.internal_routes())
    }

    /// `/internal/tasks`: requests other nodes forwarded to this one (signed).
    pub fn internal_routes(self: &Arc<Self>) -> Router {
        let node = self.clone();
        Router::new()
            .route(
                INTERNAL,
                any(move |req: Request| {
                    let node = node.clone();
                    async move { node.forwarded(req).await }
                }),
            )
            .route_layer(axum::middleware::from_fn_with_state(self.cluster.clone(), roll_cluster::require_signed))
    }

    /// Stop: drain deliveries, commit and close the log if this node still leads (a cluster
    /// node has handed the cell off by now, in its drain).
    pub async fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        let tenure = self.live.send_replace(None);
        if let Some(t) = tenure {
            t.end(false, Duration::from_secs(10)).await;
        }
    }

    /// Test hook: stop as a killed process would, mid-tenure (nothing is drained or committed;
    /// the log keeps its last commits only).
    #[doc(hidden)]
    pub fn fault_kill(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        if let Some(t) = self.live.send_replace(None) {
            t.closing.store(true, Ordering::SeqCst);
            t.log.fault_kill();
            t.cell.kill();
        }
        *self.status.lock().unwrap() = NodeStatus::default();
    }

    // ── leadership ──

    async fn run(self: Arc<Self>) {
        let mut events = self.cluster.subscribe_ownership();
        let mut follower: Option<Follower> = None;
        let mut last_check = Instant::now();
        let mut retry_at = Instant::now();
        while !self.stopped.load(Ordering::SeqCst) {
            let leader = self.live.borrow().is_some();
            if !leader && Instant::now() >= retry_at {
                if self.cluster.owns(KEY) {
                    if let Some(g) = self.cluster.guard(KEY) {
                        if let Err(e) = self.open_tenure(g, follower.take()).await {
                            warn!(error = format!("{e:#}"), "tasks: opening the cell failed; trying again");
                            retry_at = Instant::now() + Duration::from_secs(1);
                        }
                        continue;
                    }
                } else if self.cluster.preferred(KEY).is_some_and(|n| n.id == self.cluster.node_id()) {
                    // Nobody may hold it yet (a new deployment, or every node restarted): take
                    // it. The cell's background work needs a leader without any request.
                    if let Ok(Route::Local(_)) = self.cluster.route_async(KEY).await {
                        continue;
                    }
                }
            }
            if self.live.borrow().is_none() {
                if self.should_follow() {
                    self.follow(&mut follower, &mut last_check).await;
                } else if follower.take().is_some() {
                    info!("tasks: no longer a standby of the cell");
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

    /// Whether this node keeps a copy of the cell current: it's among the two nodes placement
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

    /// A fresh directory for a copy of the cell's data; the others go.
    fn new_dir(&self) -> PathBuf {
        let n = self.copies.fetch_add(1, Ordering::SeqCst);
        let base = &self.cfg.log.dir;
        let name = format!("copy-{n}");
        for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
            let old = e.file_name().to_string_lossy().into_owned();
            if old.starts_with("copy-") && old != name {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        base.join(name)
    }

    fn log_config(&self, dir: PathBuf) -> LogConfig {
        let mut c = self.cfg.log.clone();
        c.dir = dir;
        c.node = self.cluster.node_id().to_string();
        c
    }

    async fn follow(&self, follower: &mut Option<Follower>, last_check: &mut Instant) {
        match follower {
            None => match Follower::start(self.log_config(self.new_dir())).await {
                Ok(f) => {
                    info!(seq = f.seq(), "tasks: standby of the cell");
                    self.status.lock().unwrap().following = Some(f.seq());
                    *last_check = Instant::now();
                    *follower = Some(f);
                }
                Err(e) => warn!(error = format!("{e:#}"), "tasks: copying the cell for a standby failed"),
            },
            Some(f) => {
                let check = last_check.elapsed() >= self.cfg.check_every;
                if check {
                    *last_check = Instant::now();
                }
                match f.poll(check).await {
                    Ok(_) => self.status.lock().unwrap().following = Some(f.seq()),
                    Err(e) => {
                        warn!(error = format!("{e:#}"), "tasks: the standby's copy can't follow the log; copying it again");
                        *follower = None;
                    }
                }
            }
        }
    }

    async fn open_tenure(self: &Arc<Self>, guard: Guard, follower: Option<Follower>) -> anyhow::Result<()> {
        let started = Instant::now();
        let hot = follower.is_some();
        self.status.lock().unwrap().following = None;
        let log = match follower {
            Some(f) => match Log::take_over(f).await {
                Ok(l) => l,
                Err(e) => {
                    warn!(error = format!("{e:#}"), "tasks: taking over from the standby's copy failed; rebuilding it");
                    Log::open(self.log_config(self.new_dir())).await?
                }
            },
            None => Log::open(self.log_config(self.new_dir())).await?,
        };
        let mut tasks = self.cfg.tasks.clone();
        tasks.node_id = self.cluster.node_id().to_string();
        let cell = match Cell::start(log.clone(), tasks).await {
            Ok(c) => c,
            Err(e) => {
                log.close().await;
                return Err(e.context("starting the cell's tenants"));
            }
        };
        if !guard.is_valid() {
            cell.kill();
            log.close().await;
            anyhow::bail!("the cell's lease moved while it opened");
        }
        let tenure = Arc::new(Tenure {
            epoch: guard.epoch(),
            router: cell.clone().router(),
            log,
            cell,
            inflight: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
            idle: Notify::new(),
            ended: tokio::sync::OnceCell::new(),
        });
        self.live.send_replace(Some(tenure.clone()));
        {
            let mut s = self.status.lock().unwrap();
            s.leader = true;
            s.epoch = Some(tenure.epoch);
            s.tenures += 1;
        }
        info!(epoch = tenure.epoch, standby = hot, took_ms = started.elapsed().as_millis() as u64, stats = ?tenure.log.stats(), "tasks: this node leads the cell");
        tokio::spawn(self.clone().hold(tenure, guard));
        Ok(())
    }

    /// Hold the lease's guard for the tenure, so a handoff waits for the log to close. Ends
    /// when the lease starts moving or is lost, or the log stops committing (fenced).
    async fn hold(self: Arc<Self>, t: Arc<Tenure>, guard: Guard) {
        let ending = guard.ending();
        let fenced = tokio::select! {
            _ = ending => false,
            reason = t.log.fenced() => {
                // "closed": this node stopped (`shutdown`), which ended the tenure itself.
                let fenced = reason != "closed";
                if fenced {
                    warn!(%reason, "tasks: the cell's log stopped taking commits");
                }
                fenced
            }
        };
        // Within the handoff's patience, so the log has closed before the lease moves.
        t.end(fenced, self.cluster.config().handoff_timeout.mul_f32(0.6)).await;
        self.live.send_if_modified(|l| {
            let mine = l.as_ref().is_some_and(|x| Arc::ptr_eq(x, &t));
            if mine {
                *l = None;
            }
            mine
        });
        {
            let mut s = self.status.lock().unwrap();
            s.leader = false;
            s.epoch = None;
        }
        info!(epoch = t.epoch, fenced, "tasks: this node gave the cell up");
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

    /// A request from a client: run it on the leader, riding out a move. The node it reached
    /// keeps its body and routes it again while the cell has no leader that can serve it.
    async fn front(self: Arc<Self>, req: Request) -> Response {
        let req = match internal(req) {
            Ok(r) => r,
            Err(r) => return *r,
        };
        let (parts, body) = req.into_parts();
        let body = match axum::body::to_bytes(body, self.cfg.max_request).await {
            Ok(b) => b,
            Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        };
        let cfg = self.cluster.config();
        let deadline = Instant::now() + self.cfg.ride;
        loop {
            let resp = self.dispatch(Request::from_parts(parts.clone(), Body::from(body.clone()))).await;
            let wait = match unavailable_for(&resp) {
                Some(w) => w,
                // Two nodes disagreeing for a moment about where the cell belongs.
                None if resp.status() == StatusCode::LOOP_DETECTED => {
                    self.cluster.forget(KEY);
                    Duration::from_millis(50)
                }
                None => return resp,
            };
            if Instant::now() + wait >= deadline {
                let after = resp.headers().get(header::RETRY_AFTER).cloned().unwrap_or(HeaderValue::from_static("1"));
                let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "roll tasks is moving between nodes; retry shortly");
                r.headers_mut().insert(header::RETRY_AFTER, after);
                return r;
            }
            tokio::time::sleep(wait.min(cfg.max_retry_wait).max(Duration::from_millis(20))).await;
        }
    }

    async fn dispatch(self: &Arc<Self>, req: Request) -> Response {
        let node = self.clone();
        self.cluster.dispatch(KEY, req, move |req, guard| node.local(req, guard)).await
    }

    /// A request another node forwarded: run it here, or pass it on if the cell moved.
    async fn forwarded(self: Arc<Self>, req: Request) -> Response {
        if let Some(from) = req.extensions().get::<Forwarded>().map(|f| f.node.clone())
            && let Ok(Route::Forward(n)) = self.cluster.route_async(KEY).await
            && n.id == from
        {
            // Never back where it came from: the two nodes disagree right now, and the first
            // one tries again shortly.
            self.cluster.forget(KEY);
            return roll_cluster::unavailable(Duration::from_millis(50));
        }
        self.dispatch(req).await
    }

    /// Serve a request here, as the leader (`guard` holds the lease while it runs).
    async fn local(self: Arc<Self>, req: Request, guard: Guard) -> Response {
        let retry = self.cluster.config().sweep_every;
        let Some(t) = self.tenure_for(guard.epoch()).await else { return roll_cluster::unavailable(Duration::from_secs(1)) };
        let Some(entered) = t.enter() else { return roll_cluster::unavailable(retry) };
        if !guard.is_valid() {
            return roll_cluster::unavailable(retry);
        }
        let mut req = match external(req) {
            Ok(r) => r,
            Err(r) => return *r,
        };
        // The gate's mark doesn't travel with a forwarded request: check the token here.
        if let Some(op) = &self.cfg.operator
            && roll_auth::bearer(req.headers()).is_some_and(|b| op.matches(b))
        {
            req.extensions_mut().insert(roll_auth::Operator);
        }
        let resp = match t.router.clone().oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        // The engine refused a write because the log stopped taking them (a handoff, or this
        // node was fenced): nothing was committed, so the request may run again elsewhere.
        if resp.headers().contains_key(crate::cluster::NOT_LEADER) {
            return roll_cluster::unavailable(retry);
        }
        drop(entered);
        drop(guard);
        resp
    }
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": msg }))).into_response()
}

/// `req` at `uri`, without what routing attached to it (the matched route's path parameters
/// must not reach the cell's router).
fn fresh(req: Request, uri: Uri) -> Request {
    let (parts, body) = req.into_parts();
    let mut out = Request::new(body);
    *out.method_mut() = parts.method;
    *out.uri_mut() = uri;
    *out.version_mut() = parts.version;
    *out.headers_mut() = parts.headers;
    if let Some(f) = parts.extensions.get::<Forwarded>() {
        out.extensions_mut().insert(f.clone());
    }
    out
}

/// Address a client's request to [`INTERNAL`]; its own path and query travel in a header.
fn internal(req: Request) -> Result<Request, Box<Response>> {
    let original = req.extensions().get::<OriginalUri>().map_or(req.uri(), |o| &o.0).clone();
    let pq = original.path_and_query().map_or("/", |p| p.as_str());
    let value = HeaderValue::from_str(pq).map_err(|_| Box::new(error(StatusCode::BAD_REQUEST, "bad request path")))?;
    let mut req = fresh(req, Uri::from_static(INTERNAL));
    req.headers_mut().insert(URI_HEADER, value);
    Ok(req)
}

/// Undo [`internal`] on the leader.
fn external(mut req: Request) -> Result<Request, Box<Response>> {
    let bad = || Box::new(error(StatusCode::BAD_REQUEST, "a tasks request without its path"));
    let pq = req.headers_mut().remove(URI_HEADER).ok_or_else(bad)?;
    let uri = pq.to_str().ok().and_then(|s| s.parse::<Uri>().ok()).ok_or_else(bad)?;
    Ok(fresh(req, uri))
}

/// How long to wait before routing again, if `resp` says nothing ran because the cell can't be
/// served right now.
fn unavailable_for(resp: &Response) -> Option<Duration> {
    if resp.status() != StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }
    let ms = resp.headers().get("x-roll-code-unavailable")?.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_travel_with_their_own_path() {
        let req = Request::builder().method("POST").uri("/t/acme/v1/tasks/publish/https://x.example/a?b=1").body(Body::empty()).unwrap();
        let hop = internal(req).unwrap();
        assert_eq!(hop.uri(), INTERNAL);
        let back = external(hop).unwrap();
        assert_eq!(back.uri(), "/t/acme/v1/tasks/publish/https://x.example/a?b=1");
        assert!(!back.headers().contains_key(URI_HEADER));
        assert!(external(Request::new(Body::empty())).is_err());
    }
}

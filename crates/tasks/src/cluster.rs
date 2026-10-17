//! A roll tasks cell on the log store: one leader that owns the data, hot standbys, and
//! stateless proxies in front.
//!
//! - **Lease.** `<prefix>/lease` names the leader and its private address. It's only written on
//!   a change of leader, with compare-and-swap, so it costs nothing at rest.
//! - **Standbys** check the leader's health directly over the private network (free) every
//!   `health_ms`. When it stops answering (`fail_after` times in a row) or hands over, one of
//!   them wins the lease, rebuilds the data from the log and serves. Correctness never depends
//!   on this timing: the log fences out a previous leader by itself (see [`crate::store::log`]).
//! - **Leaders** look at the lease now and then and stop if they've been replaced. On SIGTERM
//!   (a deploy, a Spot interruption) they drain deliveries, commit everything and release the
//!   lease, so the hand-over takes about one health check.
//! - **Proxies** (the API on Lambda, or any node that isn't the leader) forward requests to the
//!   leader. During a hand-over they hold requests and retry until a leader answers.

use crate::engine::Config;
use crate::model::{Ms, now_ms};
use crate::store::log::{Log, LogConfig};
use crate::tenants::Cell;
use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutMode, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tower::ServiceExt;
use tracing::{info, warn};

/// Set on forwarded requests, so a node that isn't the leader never forwards twice.
const FORWARDED: &str = "roll-forwarded";
/// Set on the answer of a node that got a request meant for the leader.
pub(crate) const NOT_LEADER: &str = "roll-not-leader";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lease {
    pub node: String,
    /// Where the leader serves, `host:port` on the private network.
    pub addr: String,
    /// Bumped on every change of leader.
    pub epoch: u64,
    /// The leader handed over: take over without waiting.
    #[serde(default)]
    pub released: bool,
    pub at: Ms,
}

fn lease_path(prefix: &str) -> ObjPath {
    ObjPath::from(format!("{prefix}/lease"))
}

pub async fn read_lease(objects: &dyn ObjectStore, prefix: &str) -> Result<Option<(Lease, UpdateVersion)>> {
    match objects.get_opts(&lease_path(prefix), Default::default()).await {
        Ok(got) => {
            let version = UpdateVersion { e_tag: got.meta.e_tag.clone(), version: got.meta.version.clone() };
            let lease = serde_json::from_slice(&got.bytes().await?)?;
            Ok(Some((lease, version)))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write the lease if it's still `prev` (or absent). `false`: someone else changed it first.
pub async fn write_lease(objects: &dyn ObjectStore, prefix: &str, lease: &Lease, prev: Option<UpdateVersion>) -> Result<bool> {
    let payload = PutPayload::from(serde_json::to_vec(lease)?);
    let mode = match &prev {
        Some(v) => PutMode::Update(v.clone()),
        None => PutMode::Create,
    };
    match objects.put_opts(&lease_path(prefix), payload.clone(), mode.into()).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }) => Ok(false),
        // A local directory can't compare-and-swap; it serves one host, where it doesn't matter.
        Err(object_store::Error::NotImplemented { .. }) => {
            objects.put_opts(&lease_path(prefix), payload, PutMode::Overwrite.into()).await?;
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

/// Forwards requests to the leader named in the lease, holding them through a hand-over.
pub struct Proxy {
    objects: Arc<dyn ObjectStore>,
    prefix: String,
    http: reqwest::Client,
    /// How long a request may wait for a leader.
    pub wait: Duration,
    /// Largest request body accepted for forwarding.
    pub max_body: usize,
    cached: RwLock<Option<(String, Instant)>>,
}

impl Proxy {
    pub fn new(objects: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(1_000))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client");
        Self { objects, prefix: prefix.into(), http, wait: Duration::from_secs(25), max_body: 8 << 20, cached: RwLock::new(None) }
    }

    /// Everything goes to the leader.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new().fallback(|State(p): State<Arc<Proxy>>, req: Request| async move { p.forward(req).await }).with_state(self)
    }

    async fn leader(&self, fresh: bool) -> Option<String> {
        if !fresh && let Some((addr, at)) = &*self.cached.read().unwrap() && at.elapsed() < Duration::from_secs(10) {
            return Some(addr.clone());
        }
        let found = match read_lease(&*self.objects, &self.prefix).await {
            Ok(Some((lease, _))) if !lease.released => Some(lease.addr),
            Ok(_) => None,
            Err(e) => {
                warn!(error = %e, "reading the lease");
                None
            }
        };
        *self.cached.write().unwrap() = found.clone().map(|a| (a, Instant::now()));
        found
    }

    pub async fn forward(&self, req: Request) -> Response {
        let (parts, body) = req.into_parts();
        let Ok(body) = axum::body::to_bytes(body, self.max_body).await else {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        };
        let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let deadline = Instant::now() + self.wait;
        let mut fresh = false;
        let mut delay = Duration::from_millis(50);
        loop {
            if let Some(addr) = self.leader(fresh).await {
                let mut rb = self.http.request(parts.method.clone(), format!("http://{addr}{path}")).body(body.clone());
                for (k, v) in &parts.headers {
                    if !hop_by_hop(k) {
                        rb = rb.header(k, v);
                    }
                }
                match rb.header(FORWARDED, "1").send().await {
                    Ok(resp) if !resp.headers().contains_key(NOT_LEADER) => return relay(resp),
                    // Not (or no longer) the leader.
                    Ok(_) => {}
                    // Nobody there (yet): the request never left, so it's safe to try again.
                    Err(e) if e.is_connect() => {}
                    Err(e) => {
                        warn!(error = %e, "forwarding to the leader");
                        return error(StatusCode::BAD_GATEWAY, "the leader didn't answer; the request may or may not have been applied");
                    }
                }
            }
            if Instant::now() + delay >= deadline {
                return error(StatusCode::SERVICE_UNAVAILABLE, "no leader available; try again");
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(1_000));
            fresh = true;
        }
    }
}

fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "te" | "trailer" | "upgrade" | "content-length"
    ) || name.as_str() == FORWARDED
}

fn relay(resp: reqwest::Response) -> Response {
    let mut out = Response::builder().status(resp.status().as_u16());
    for (k, v) in resp.headers() {
        if !hop_by_hop(k) {
            out = out.header(k, v);
        }
    }
    out.body(Body::from_stream(resp.bytes_stream())).unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "bad response from the leader"))
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": msg }))).into_response()
}

#[derive(Clone)]
pub struct NodeConfig {
    pub log: LogConfig,
    pub tasks: Config,
    /// This node's address for the others, `host:port`; default: the listening address.
    pub advertise: Option<String>,
    /// Standbys check the leader this often …
    pub health_ms: u64,
    /// … and take over after this many failed checks in a row.
    pub fail_after: u32,
    /// Leaders make sure they still hold the lease this often.
    pub lease_check_ms: u64,
    /// Standbys re-read the lease at least this often even while the leader is healthy.
    pub lease_refresh_ms: u64,
    /// How long a request may wait for a leader when forwarded.
    pub forward_wait_ms: u64,
    /// After stepping down, keep answering this long ("not the leader", or forwarding to the new
    /// one) before closing the listener. Proxies holding a connection to this node then move to
    /// the new leader on a definite answer, instead of a request cut off by the close, whose
    /// fate they couldn't know.
    pub linger_ms: u64,
}

impl NodeConfig {
    pub fn new(log: LogConfig, tasks: Config) -> Self {
        Self { log, tasks, advertise: None, health_ms: 1_000, fail_after: 3, lease_check_ms: 5_000, lease_refresh_ms: 30_000, forward_wait_ms: 25_000, linger_ms: 3_000 }
    }
}

/// How to stop a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Run,
    /// Drain, commit and hand over.
    Graceful,
    /// Vanish as a crash would (tests): nothing is flushed, the lease isn't released.
    Crash,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Stopped,
    /// Another node took over the log (or it became unwritable).
    Fenced(String),
    /// Another node took the lease.
    Deposed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Standby,
    Starting,
    Leader,
    Stopping,
}

struct Shared {
    node: String,
    role: Mutex<Role>,
    epoch: AtomicU64,
    api: RwLock<Option<Router>>,
    /// API requests being served by this node as leader.
    serving: AtomicUsize,
    proxy: Arc<Proxy>,
}

impl Shared {
    fn set_role(&self, role: Role) {
        *self.role.lock().unwrap() = role;
    }
}

#[derive(Deserialize)]
struct Status {
    node: String,
    role: Role,
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "starting" => Role::Starting,
            "leader" => Role::Leader,
            "stopping" => Role::Stopping,
            _ => Role::Standby,
        })
    }
}

async fn status(State(s): State<Arc<Shared>>) -> Response {
    let role = *s.role.lock().unwrap();
    axum::Json(json!({ "node": s.node, "role": role, "epoch": s.epoch.load(Ordering::Relaxed) })).into_response()
}

/// A node on its way out tells clients not to reuse their connection to it.
async fn close_when_stopping(State(s): State<Arc<Shared>>, req: Request, next: axum::middleware::Next) -> Response {
    let mut res = next.run(req).await;
    if *s.role.lock().unwrap() == Role::Stopping {
        res.headers_mut().insert(header::CONNECTION, header::HeaderValue::from_static("close"));
    }
    res
}

async fn handle(State(s): State<Arc<Shared>>, req: Request) -> Response {
    let api = s.api.read().unwrap().clone();
    if let Some(api) = api {
        struct Serving<'a>(&'a AtomicUsize);
        impl Drop for Serving<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        s.serving.fetch_add(1, Ordering::SeqCst);
        let _serving = Serving(&s.serving);
        return api.oneshot(req).await.into_response();
    }
    if req.headers().contains_key(FORWARDED) {
        let mut res = error(StatusCode::SERVICE_UNAVAILABLE, "not the leader");
        res.headers_mut().insert(NOT_LEADER, header::HeaderValue::from_static("1"));
        return res;
    }
    s.proxy.forward(req).await
}

/// Run a node until it's stopped, deposed or fenced: serve as standby (forwarding requests),
/// win the lease when the leader goes, then serve as leader.
pub async fn run_node(listener: tokio::net::TcpListener, cfg: NodeConfig, mut stop: watch::Receiver<Stop>) -> Result<Exit> {
    let objects = cfg.log.objects.clone();
    let prefix = cfg.log.prefix.clone();
    let me = cfg.log.node.clone();
    let addr = match &cfg.advertise {
        Some(a) => a.clone(),
        None => listener.local_addr()?.to_string(),
    };
    let mut proxy = Proxy::new(objects.clone(), prefix.clone());
    proxy.wait = Duration::from_millis(cfg.forward_wait_ms);
    proxy.max_body = cfg.tasks.max_body_bytes + (1 << 20);
    let shared = Arc::new(Shared {
        node: me.clone(),
        role: Mutex::new(Role::Standby),
        epoch: AtomicU64::new(0),
        api: RwLock::new(None),
        serving: AtomicUsize::new(0),
        proxy: Arc::new(proxy),
    });
    let app = Router::new()
        .route("/_roll/node", get(status))
        .fallback(handle)
        .layer(axum::middleware::from_fn_with_state(shared.clone(), close_when_stopping))
        .with_state(shared.clone());
    let (server_stop, server_stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = server_stopped.await;
            })
            .await;
    });
    let health = reqwest::Client::builder().timeout(Duration::from_millis(cfg.health_ms.clamp(200, 2_000))).build()?;
    info!(node = me, addr, "roll tasks node (standby)");

    // --- standby: wait for the lease ---
    let mut known: Option<(Lease, UpdateVersion, Instant)> = None;
    let mut failures = 0u32;
    let won = loop {
        if *stop.borrow() != Stop::Run {
            let _ = server_stop.send(());
            if *stop.borrow() == Stop::Crash {
                server.abort();
            }
            let _ = server.await;
            return Ok(Exit::Stopped);
        }
        let stale = known.as_ref().is_none_or(|(_, _, at)| failures > 0 || at.elapsed() > Duration::from_millis(cfg.lease_refresh_ms));
        if stale {
            match read_lease(&*objects, &prefix).await {
                Ok(found) => known = found.map(|(l, v)| (l, v, Instant::now())),
                Err(e) => {
                    warn!(error = %e, "reading the lease");
                    tokio::time::sleep(Duration::from_millis(cfg.health_ms)).await;
                    continue;
                }
            }
        }
        let take = match &known {
            None => Some((1, None)),
            Some((l, v, _)) if l.released || l.node == me => Some((l.epoch + 1, Some(v.clone()))),
            Some((l, v, _)) => {
                let ok = match health.get(format!("http://{}/_roll/node", l.addr)).send().await {
                    Ok(r) => r.json::<Status>().await.is_ok_and(|s| s.node == l.node && matches!(s.role, Role::Starting | Role::Leader)),
                    Err(_) => false,
                };
                failures = if ok { 0 } else { failures + 1 };
                (failures >= cfg.fail_after).then(|| (l.epoch + 1, Some(v.clone())))
            }
        };
        if let Some((epoch, prev)) = take {
            let lease = Lease { node: me.clone(), addr: addr.clone(), epoch, released: false, at: now_ms() };
            match write_lease(&*objects, &prefix, &lease, prev).await {
                Ok(true) => break lease,
                Ok(false) => {
                    // Someone else won; follow them.
                    known = None;
                    failures = 0;
                }
                Err(e) => warn!(error = %e, "writing the lease"),
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(cfg.health_ms)) => {}
            _ = stop.changed() => {}
        }
    };

    // --- leader ---
    shared.epoch.store(won.epoch, Ordering::Relaxed);
    shared.set_role(Role::Starting);
    info!(node = me, epoch = won.epoch, "won the lease; recovering");
    let log = match Log::open(cfg.log.clone()).await {
        Ok(l) => l,
        Err(e) => {
            let _ = server_stop.send(());
            return Err(e.context("recovering the log"));
        }
    };
    let mut tasks_cfg = cfg.tasks.clone();
    tasks_cfg.node_id = me.clone();
    let cell = match Cell::start(log.clone(), tasks_cfg).await {
        Ok(c) => c,
        Err(e) => {
            let _ = server_stop.send(());
            let _ = server.await;
            // Replaced while starting: not an error, just over.
            if let Some(reason) = futures::FutureExt::now_or_never(log.fenced()) {
                return Ok(Exit::Fenced(reason));
            }
            return Err(e.context("starting"));
        }
    };
    *shared.api.write().unwrap() = Some(cell.clone().router());
    shared.set_role(Role::Leader);
    info!(node = me, epoch = won.epoch, stats = ?log.stats(), "leading");

    let deposed = async {
        loop {
            tokio::time::sleep(Duration::from_millis(cfg.lease_check_ms)).await;
            match read_lease(&*objects, &prefix).await {
                Ok(Some((l, _))) if l.node != me => return,
                Ok(None) => return,
                Ok(_) => {}
                Err(e) => warn!(error = %e, "checking the lease"),
            }
        }
    };
    let stopped = async {
        loop {
            let s = *stop.borrow_and_update();
            if s != Stop::Run {
                return s;
            }
            if stop.changed().await.is_err() {
                return Stop::Graceful;
            }
        }
    };
    let exit = tokio::select! {
        how = stopped => match how {
            Stop::Crash => {
                *shared.api.write().unwrap() = None;
                server.abort();
                cell.kill();
                return Ok(Exit::Stopped);
            }
            _ => Exit::Stopped,
        },
        reason = log.fenced() => Exit::Fenced(reason),
        _ = deposed => Exit::Deposed,
    };

    if exit == Exit::Stopped {
        // Keep serving while deliveries drain, then hand over.
        cell.shutdown().await;
        shared.set_role(Role::Stopping);
        *shared.api.write().unwrap() = None;
        // New requests go to the next leader; let the ones under way finish (long polls get
        // cut short: their writes, if any, fail as retryable).
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.serving.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cell.flush_usage().await;
        log.close().await;
        match read_lease(&*objects, &prefix).await {
            Ok(Some((l, v))) if l.node == me => {
                let released = Lease { released: true, at: now_ms(), ..l };
                if let Err(e) = write_lease(&*objects, &prefix, &released, Some(v)).await {
                    warn!(error = %e, "releasing the lease");
                }
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "releasing the lease"),
        }
        info!(node = me, "handed over");
    } else {
        warn!(node = me, ?exit, "stepping down");
        shared.set_role(Role::Stopping);
        *shared.api.write().unwrap() = None;
        cell.kill();
    }
    // Answer stragglers for a while (see `linger_ms`), then close.
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(cfg.linger_ms)) => {}
        _ = async {
            loop {
                if *stop.borrow_and_update() == Stop::Crash {
                    return;
                }
                if stop.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        } => {}
    }
    let _ = server_stop.send(());
    let _ = server.await;
    Ok(exit)
}

/// This task's private address on ECS (awsvpc), from the task metadata endpoint.
pub async fn ecs_address() -> Option<String> {
    let uri = std::env::var("ECS_CONTAINER_METADATA_URI_V4").ok()?;
    let meta: serde_json::Value = reqwest::Client::new().get(uri).timeout(Duration::from_secs(2)).send().await.ok()?.json().await.ok()?;
    meta["Networks"][0]["IPv4Addresses"][0].as_str().map(str::to_string)
}

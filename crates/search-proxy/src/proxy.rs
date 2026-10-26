//! The proxy: an HTTP/1.1 reverse proxy in front of the search nodes.
//!
//! **Routing.** Each request's cluster ([`crate::route::cluster_of`]) goes to the node that
//! last served it (a small cache, refreshed by every answer), else to its rendezvous node among
//! live, non-draining nodes of the proxy's AZ ([`crate::route::pick`]). A node that doesn't own
//! the cluster refuses it with 503 and `X-Roll-Search-Owner: <node>` before running anything,
//! and the proxy follows the header. Requests with no cluster go to any live node.
//!
//! **Holding.** With no live node (the fleet scaled to zero), a request waits: the proxy runs
//! its [`Waker`] (at most every `wake_every`), polls membership quickly while anyone waits, and
//! forwards as soon as a node heartbeats. The same wait covers handoffs: when the owner named
//! by a refusal is draining, closed or dead, the request backs off and retries until the lease
//! moves (released at once by a draining owner, or expired after the lease TTL for a dead
//! one). Everything is bounded by `hold_timeout`; then the client gets a 503.
//!
//! **Retries.** A request is resent to another node only when the node provably didn't run it:
//! the connection couldn't be made, the node refused the cluster (`X-Roll-Search-Owner`), or it
//! answered `node_closed_exception` (it is shutting down and opens nothing new). A request whose
//! connection broke after it was sent is resent only if it is read-only (GET/HEAD and the search
//! family of POSTs), and only once, to another node if there is one; otherwise the client gets a
//! 502 and decides, as it would with a direct connection. A connection that broke because the
//! *client's* upload failed (it disconnected mid-body) isn't resent at all. Only a failed connect
//! takes a node out of rotation (for 3 s): a broken connection is something a client can provoke,
//! and no client may take healthy nodes away from the others.
//! Bodies stream straight through and are kept as they pass ([`crate::replay`]), up
//! to `retry_body_bytes` per request and `retry_buffer_bytes` per proxy, so a refused request
//! is resent whole even when the refusal came mid-upload; a body over those bounds can't be,
//! and its refusal goes to the client (whose retry then goes straight to the owner).
//!
//! **Owner cache.** Only answers that show the node holds the cluster teach the proxy its
//! owner: REST answers past the node's credential check (not 401/403), and refusals naming the
//! owner. Control API answers never do (the node there forwards to the owner itself).
//!
//! **Node TLS.** A node whose record carries a certificate pin is reached over TLS that accepts
//! only that certificate ([`crate::upstream`]); others over plain HTTP.
//!
//! **Listeners.** Plain TCP or TLS (rustls; the SNI host name can select the cluster, exactly
//! like the node's `<cluster>.<domain>` routing), optionally behind the PROXY protocol (an NLB
//! with `proxy_protocol_v2.enabled`: the client's address reaches `X-Forwarded-For`). A
//! separate health listener answers load balancer checks without touching nodes, so health
//! checks never wake a fleet that scaled to zero. The control API (`/v1/search`) is proxied the
//! same way on its own listener; its nodes forward cluster-specific calls to the owner
//! themselves.

use crate::fleet::{Members, NodeRecord};
use crate::replay::{Budget, Replay, SendBody};
use crate::route::{self, Candidate};
use crate::upstream::{Connector, Pins};
use crate::wake::Waker;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tracing::{debug, info, warn};

pub type Body = BoxBody<Bytes, hyper::Error>;

/// The header a node names a cluster's owner with when it refuses it (search-server's
/// `node::OWNER_HEADER`).
pub const OWNER_HEADER: &str = "x-roll-search-owner";

/// Bytes of a 503 body read to decide whether it can be retried (refusals and
/// `node_closed_exception` are far smaller; a larger 503 is passed on as it is).
const PEEK_503: usize = 64 << 10;
/// Resends of a read-only request after a node broke its connection mid-request.
const MAX_BROKEN_RETRIES: u32 = 1;
/// Owners remembered per proxy; past it the cache starts over (owners are re-learned in one
/// request each).
const OWNER_CACHE: usize = 200_000;
/// How long a remembered owner is trusted without being confirmed.
const OWNER_TTL: Duration = Duration::from_secs(120);
/// A node whose connection failed is skipped this long.
const BAD_FOR: Duration = Duration::from_secs(3);
/// Membership polling while requests wait for a node.
const FAST_REFRESH: Duration = Duration::from_millis(200);

fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|never| match never {}).boxed()
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// This proxy's availability zone: nodes there are preferred.
    pub az: Option<String>,
    /// DNS suffix of cluster host names (`<cluster>.<domain>`), as the nodes have it.
    pub domain: Option<String>,
    /// A node is gone when its heartbeat is older than this.
    pub node_ttl: Duration,
    /// How often membership is re-read when nobody waits.
    pub refresh: Duration,
    /// Longest a request waits for a node (a wake, a handoff, a failover).
    pub hold_timeout: Duration,
    /// Run the waker at most this often.
    pub wake_every: Duration,
    pub connect_timeout: Duration,
    /// Request bodies up to this are kept as they stream through, so they can be resent.
    pub retry_body_bytes: usize,
    /// Bytes kept for resends across all requests in flight.
    pub retry_buffer_bytes: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            az: None,
            domain: None,
            node_ttl: Duration::from_secs(5),
            refresh: Duration::from_secs(1),
            hold_timeout: Duration::from_secs(60),
            wake_every: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(2),
            // A node's default `http.max_content_length`.
            retry_body_bytes: 100 << 20,
            retry_buffer_bytes: 128 << 20,
        }
    }
}

#[derive(Clone, Debug)]
struct Backend {
    http: SocketAddr,
    api: Option<SocketAddr>,
    az: Option<String>,
    draining: bool,
    /// Age at the last refresh, and when that was.
    age_ms: u64,
    seen: Instant,
    /// Connecting failed: skipped until then.
    bad_until: Option<Instant>,
    /// It answered `node_closed_exception`: it opens no cluster any more.
    closed: bool,
}

/// The live view of the fleet.
pub struct Directory {
    members: Arc<dyn Members>,
    cfg: ProxyConfig,
    nodes: RwLock<HashMap<String, Backend>>,
    owners: Mutex<HashMap<String, (String, Instant)>>,
    /// Certificate pins of nodes that serve TLS, by REST address.
    pins: Arc<Pins>,
    changed: Notify,
    waiters: AtomicUsize,
    kick: Arc<Notify>,
    refreshes: AtomicU64,
}

/// Which listener a request came in on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    /// Elasticsearch REST.
    Rest,
    /// The control API (`/v1/search`).
    Api,
}

impl Directory {
    pub fn new(members: Arc<dyn Members>, cfg: ProxyConfig) -> Arc<Directory> {
        Arc::new(Directory {
            members,
            cfg,
            nodes: RwLock::default(),
            owners: Mutex::default(),
            pins: Arc::default(),
            changed: Notify::new(),
            waiters: AtomicUsize::new(0),
            kick: Arc::new(Notify::new()),
            refreshes: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> &ProxyConfig {
        &self.cfg
    }

    /// Re-reads node records.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let records = self.members.list().await?;
        self.apply(records);
        Ok(())
    }

    fn apply(&self, records: Vec<NodeRecord>) {
        let now = Instant::now();
        let mut nodes = self.nodes.write();
        let mut next = HashMap::with_capacity(records.len());
        let mut pins = HashMap::new();
        for r in records {
            let Ok(http) = r.beat.http.parse::<SocketAddr>() else {
                warn!(node = %r.beat.id, http = %r.beat.http, "node record with a bad address");
                continue;
            };
            // A node that asks for TLS is never reached without it (REST, and the API port
            // when that serves TLS too).
            match r.beat.pins() {
                Ok(p) => pins.extend(p),
                Err(e) => {
                    warn!(node = %r.beat.id, error = %e, "node record with a bad certificate pin");
                    continue;
                }
            }
            let old = nodes.remove(&r.beat.id);
            let b = Backend {
                http,
                api: r.beat.api.as_deref().and_then(|a| a.parse().ok()),
                az: r.beat.az.clone(),
                draining: r.beat.draining,
                age_ms: r.age_ms,
                seen: now,
                bad_until: old.as_ref().and_then(|o| o.bad_until).filter(|t| *t > now),
                // A restarted node (new address) opens clusters again.
                closed: old.as_ref().is_some_and(|o| o.closed && o.http == http),
            };
            next.insert(r.beat.id, b);
        }
        *nodes = next;
        self.pins.replace(pins);
        drop(nodes);
        self.refreshes.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    /// Follows membership until the directory is dropped: every `refresh`, or every
    /// [`FAST_REFRESH`] while requests wait for a node.
    pub fn spawn_refresh(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(d) = me.upgrade() else { return };
                if let Err(e) = d.refresh().await {
                    warn!(error = %e, "reading search node records failed");
                }
                let period = if d.waiters.load(Ordering::SeqCst) > 0 { FAST_REFRESH } else { d.cfg.refresh };
                let kick = d.kick.clone();
                drop(d);
                let _ = tokio::time::timeout(period, kick.notified()).await;
            }
        });
    }

    fn alive(&self, b: &Backend, now: Instant) -> bool {
        (b.age_ms as u128 + b.seen.elapsed().as_millis()) < self.cfg.node_ttl.as_millis() && b.bad_until.is_none_or(|t| t <= now)
    }

    /// Live, accepting new clusters.
    fn open_for_new(&self, b: &Backend, now: Instant) -> bool {
        self.alive(b, now) && !b.draining && !b.closed
    }

    /// Whether the node at `addr` is reached over TLS.
    pub fn tls(&self, addr: &SocketAddr) -> bool {
        self.pins.contains(addr)
    }

    /// Ids of nodes that are alive (for status and tests).
    pub fn live(&self) -> Vec<String> {
        let now = Instant::now();
        let mut v: Vec<String> = self.nodes.read().iter().filter(|(_, b)| self.alive(b, now)).map(|(k, _)| k.clone()).collect();
        v.sort();
        v
    }

    /// The node for `cluster` (`None`: any) and its address on `port`, other than `avoid` when
    /// another node can take it.
    fn choose(&self, cluster: Option<&str>, port: Port, avoid: Option<&str>) -> Option<(String, SocketAddr)> {
        let pick = self.choose_from(cluster, port, avoid);
        if pick.is_none() && avoid.is_some() { self.choose_from(cluster, port, None) } else { pick }
    }

    fn choose_from(&self, cluster: Option<&str>, port: Port, avoid: Option<&str>) -> Option<(String, SocketAddr)> {
        let now = Instant::now();
        let nodes = self.nodes.read();
        let addr_of = |b: &Backend| match port {
            Port::Rest => Some(b.http),
            Port::Api => b.api,
        };
        if let Some(c) = cluster {
            let cached = self.owners.lock().get(c).filter(|(_, at)| at.elapsed() < OWNER_TTL).map(|(n, _)| n.clone());
            // The owner serves its clusters while it drains; only a closed one is skipped.
            if let Some(n) = cached
                && avoid != Some(n.as_str())
                && let Some(b) = nodes.get(&n)
                && self.alive(b, now)
                && !b.closed
                && let Some(a) = addr_of(b)
            {
                return Some((n, a));
            }
        }
        let cands: Vec<Candidate> =
            nodes.iter().filter(|(id, b)| avoid != Some(id.as_str()) && self.open_for_new(b, now) && addr_of(b).is_some()).map(|(id, b)| Candidate { id, az: b.az.as_deref() }).collect();
        let id = route::pick(cluster.unwrap_or(""), &cands, self.cfg.az.as_deref())?.to_string();
        let a = addr_of(&nodes[&id])?;
        Some((id, a))
    }

    /// Where a refusal says the cluster lives, if that node can take it now.
    fn owner_addr(&self, node: &str, port: Port) -> Option<SocketAddr> {
        let now = Instant::now();
        let nodes = self.nodes.read();
        let b = nodes.get(node).filter(|b| self.alive(b, now) && !b.closed)?;
        match port {
            Port::Rest => Some(b.http),
            Port::Api => b.api,
        }
    }

    pub fn remember_owner(&self, cluster: &str, node: &str) {
        let mut o = self.owners.lock();
        if let Some((n, at)) = o.get_mut(cluster) {
            if n != node {
                *n = node.to_string();
            }
            *at = Instant::now();
            return;
        }
        if o.len() >= OWNER_CACHE {
            o.clear();
        }
        o.insert(cluster.to_string(), (node.to_string(), Instant::now()));
    }

    pub fn forget_owner(&self, cluster: &str) {
        self.owners.lock().remove(cluster);
    }

    pub fn owner_of(&self, cluster: &str) -> Option<String> {
        self.owners.lock().get(cluster).map(|(n, _)| n.clone())
    }

    fn mark_bad(&self, node: &str) {
        if let Some(b) = self.nodes.write().get_mut(node) {
            b.bad_until = Some(Instant::now() + BAD_FOR);
        }
    }

    fn mark_closed(&self, node: &str) {
        if let Some(b) = self.nodes.write().get_mut(node) {
            b.closed = true;
        }
    }

    /// Waits for the next membership change (at most `max`), polling fast meanwhile.
    async fn wait_change(&self, max: Duration) {
        let changed = self.changed.notified();
        self.waiters.fetch_add(1, Ordering::SeqCst);
        self.kick.notify_one();
        let _ = tokio::time::timeout(max, changed).await;
        self.waiters.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Where a request came from.
#[derive(Clone, Debug, Default)]
pub struct ClientInfo {
    pub addr: Option<SocketAddr>,
    /// TLS server name the client asked for.
    pub sni: Option<String>,
    pub tls: bool,
}

pub struct Proxy {
    pub directory: Arc<Directory>,
    client: Client<Connector, SendBody>,
    /// Request bytes kept for resends.
    pub budget: Arc<Budget>,
    waker: Option<Arc<dyn Waker>>,
    last_wake: Mutex<Option<Instant>>,
    /// Requests answered, retried, held (for status and tests).
    pub stats: Stats,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub requests: AtomicU64,
    pub retries: AtomicU64,
    pub redirects: AtomicU64,
    pub held: AtomicU64,
    pub wakes: AtomicU64,
}

enum Outcome {
    Response(Response<Incoming>),
    /// The node never received the request.
    NotSent(String),
    /// It may have run it.
    Broken(String),
}

impl Proxy {
    pub fn new(directory: Arc<Directory>, waker: Option<Arc<dyn Waker>>) -> Arc<Proxy> {
        let mut conn = HttpConnector::new();
        conn.set_nodelay(true);
        conn.set_connect_timeout(Some(directory.cfg.connect_timeout));
        conn.set_keepalive(Some(Duration::from_secs(60)));
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(256)
            .build(Connector::new(conn, directory.pins.clone(), directory.cfg.connect_timeout * 2));
        let budget = Budget::new(directory.cfg.retry_buffer_bytes);
        Arc::new(Proxy { directory, client, budget, waker, last_wake: Mutex::new(None), stats: Stats::default() })
    }

    fn wake(&self) {
        let Some(w) = self.waker.clone() else { return };
        {
            let mut last = self.last_wake.lock();
            if last.is_some_and(|t| t.elapsed() < self.directory.cfg.wake_every) {
                return;
            }
            *last = Some(Instant::now());
        }
        self.stats.wakes.fetch_add(1, Ordering::Relaxed);
        info!("no search node running; waking the fleet");
        tokio::spawn(async move {
            if let Err(e) = w.wake().await {
                warn!(error = %e, "waking search nodes failed");
            }
        });
    }

    /// Forwards one request (any body), with routing, holding and retries as described in the
    /// module docs.
    pub async fn forward<B>(&self, req: Request<B>, client: &ClientInfo, port: Port) -> Response<Body>
    where
        B: hyper::body::Body<Data = Bytes, Error = hyper::Error> + Unpin + Send + 'static,
    {
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let cfg = &self.directory.cfg;
        let deadline = Instant::now() + cfg.hold_timeout;
        let (parts, body) = req.into_parts();
        let cluster = match port {
            Port::Rest => {
                let auth = parts.headers.get(hyper::header::AUTHORIZATION).and_then(|v| v.to_str().ok());
                let host = parts.headers.get(hyper::header::HOST).and_then(|v| v.to_str().ok());
                let host_cluster = client
                    .sni
                    .as_deref()
                    .and_then(|s| route::host_label(s, cfg.domain.as_deref()))
                    .or_else(|| host.and_then(|h| route::host_label(h, cfg.domain.as_deref())));
                route::cluster_of(auth, host_cluster)
            }
            Port::Api => api_cluster(parts.uri.path()),
        };
        let declared = parts.headers.get(hyper::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        let no_body = declared == Some(0) || (declared.is_none() && !parts.headers.contains_key(hyper::header::TRANSFER_ENCODING));
        let replay = (!no_body).then(|| Replay::new(body, declared, cfg.retry_body_bytes, self.budget.clone()));
        // Whether the whole body can still be sent again (never once the client's own upload
        // failed: there is no whole body any more).
        let resendable = || replay.as_ref().is_none_or(|r| r.replayable() && !r.client_failed());
        let client_failed = || replay.as_ref().is_some_and(Replay::client_failed);
        let headers = forward_headers(&parts.headers, client, cfg.domain.as_deref());
        let path = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        let read_only = read_only(&parts.method, parts.uri.path());
        let mut redirect: Option<(String, SocketAddr)> = None;
        let mut backoff = Duration::from_millis(25);
        let mut held = false;
        let mut broken = 0u32;
        // The node whose connection broke: a resend goes elsewhere if it can.
        let mut avoid: Option<String> = None;
        let mut last_503: Option<Response<Body>> = None;
        loop {
            let target = match redirect.take() {
                Some(t) => Some(t),
                None => self.directory.choose(cluster.as_deref(), port, avoid.as_deref()),
            };
            let Some((node, addr)) = target else {
                if Instant::now() >= deadline {
                    break;
                }
                if !held {
                    held = true;
                    self.stats.held.fetch_add(1, Ordering::Relaxed);
                }
                self.wake();
                self.directory.wait_change(FAST_REFRESH.min(deadline.saturating_duration_since(Instant::now()))).await;
                continue;
            };
            let body = match &replay {
                None => http_body_util::Empty::new().map_err(|never| match never {}).boxed(),
                Some(r) => match r.attempt() {
                    Some(b) => b,
                    None => break,
                },
            };
            let scheme = if self.directory.tls(&addr) { "https" } else { "http" };
            let mut req = Request::builder().method(parts.method.clone()).uri(format!("{scheme}://{addr}{path}")).version(hyper::Version::HTTP_11);
            if let Some(h) = req.headers_mut() {
                *h = headers.clone();
            }
            let req = match req.body(body) {
                Ok(r) => r,
                Err(e) => return error(port, StatusCode::BAD_REQUEST, "bad_request", &e.to_string()),
            };
            let outcome = self.send(req).await;
            if !matches!(outcome, Outcome::Response(_)) && client_failed() {
                // The client disconnected (or stalled out) mid-upload: hyper reports that as a
                // failed request, but the node did nothing wrong. Blaming it would take a
                // healthy node out of rotation for every tenant, and retrying would only fail
                // the same way.
                debug!(%node, "the client's request body failed");
                return error(port, StatusCode::BAD_REQUEST, "parse_exception", "the request body ended before it was complete");
            }
            match outcome {
                Outcome::NotSent(e) => {
                    debug!(%node, %addr, error = %e, "search node unreachable");
                    self.directory.mark_bad(&node);
                    if let Some(c) = &cluster {
                        self.directory.forget_owner(c);
                    }
                    if !resendable() {
                        return error(port, StatusCode::BAD_GATEWAY, "node_disconnected_exception", &format!("search node [{node}] is unreachable: {e}"));
                    }
                    self.stats.retries.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Outcome::Broken(e) => {
                    // Not marked bad: the node is up (it took the connection) and a broken
                    // connection says little about it. A client can provoke one (a corrupt gzip
                    // body the node drops the connection over), and so can a kept-alive
                    // connection the node closed just as the request went out. A node that
                    // really died fails to connect next, and that marks it.
                    avoid = Some(node.clone());
                    if let Some(c) = &cluster {
                        self.directory.forget_owner(c);
                    }
                    // Once: a request that breaks every connection it is sent on (one that crashes
                    // the node's handler) must not walk through the whole fleet.
                    broken += 1;
                    if read_only && broken <= MAX_BROKEN_RETRIES && resendable() && Instant::now() < deadline {
                        self.stats.retries.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    return error(port, StatusCode::BAD_GATEWAY, "node_disconnected_exception", &format!("search node [{node}] closed the connection: {e}"));
                }
                Outcome::Response(resp) if resp.status() == StatusCode::SERVICE_UNAVAILABLE && resendable() => {
                    let owner = resp.headers().get(OWNER_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
                    let (mut rparts, rbody) = resp.into_parts();
                    strip_hop_by_hop(&mut rparts.headers);
                    let bytes = match peek(rbody, PEEK_503).await {
                        Ok(Peeked::Whole(b)) => b,
                        // Larger than any refusal or `node_closed_exception`: an ordinary error
                        // (say a search_phase_execution_exception with many shard failures),
                        // which goes to the client whole.
                        Ok(Peeked::Partial(head, rest)) if owner.is_none() => {
                            return Response::from_parts(rparts, Prefixed { head: Some(head), rest }.boxed());
                        }
                        // A refusal whose body we don't need (only its header counts).
                        Ok(Peeked::Partial(..)) => Bytes::new(),
                        Err(_) => return error(port, StatusCode::SERVICE_UNAVAILABLE, "node_disconnected_exception", "search node answered 503"),
                    };
                    let closed = !bytes.is_empty() && error_type(&bytes).as_deref() == Some("node_closed_exception");
                    if owner.is_none() && !closed {
                        return Response::from_parts(rparts, full(bytes));
                    }
                    if bytes.is_empty() {
                        rparts.headers.remove(hyper::header::CONTENT_LENGTH);
                    }
                    last_503 = Some(Response::from_parts(rparts, full(bytes)));
                    if Instant::now() >= deadline {
                        break;
                    }
                    self.stats.retries.fetch_add(1, Ordering::Relaxed);
                    if closed {
                        // Shutting down: it opens nothing new; another node takes the cluster
                        // once this one hands the lease back.
                        self.directory.mark_closed(&node);
                        if let Some(c) = &cluster {
                            self.directory.forget_owner(c);
                        }
                        continue;
                    }
                    let owner = owner.unwrap_or_default();
                    match self.directory.owner_addr(&owner, port).filter(|_| owner != node) {
                        Some(a) => {
                            self.stats.redirects.fetch_add(1, Ordering::Relaxed);
                            if let Some(c) = &cluster {
                                self.directory.remember_owner(c, &owner);
                            }
                            redirect = Some((owner, a));
                        }
                        None => {
                            // The owner can't take it (draining away, dead, unknown yet): wait
                            // for its lease to move.
                            if let Some(c) = &cluster {
                                self.directory.forget_owner(c);
                            }
                            tokio::time::sleep(backoff.min(deadline.saturating_duration_since(Instant::now()))).await;
                            backoff = (backoff * 2).min(Duration::from_millis(500));
                        }
                    }
                    continue;
                }
                Outcome::Response(resp) => {
                    if let Some(c) = &cluster
                        && port == Port::Rest
                    {
                        if confirms_owner(resp.status()) {
                            self.directory.remember_owner(c, &node);
                        } else if resp.status() == StatusCode::SERVICE_UNAVAILABLE
                            && let Some(owner) = resp.headers().get(OWNER_HEADER).and_then(|v| v.to_str().ok())
                            && owner != node
                            && self.directory.owner_addr(owner, port).is_some()
                        {
                            // A body over the resend bounds can't be sent again, so the client
                            // gets the refusal; its retry goes straight to the owner.
                            self.directory.remember_owner(c, owner);
                        }
                    }
                    let (mut rparts, rbody) = resp.into_parts();
                    strip_hop_by_hop(&mut rparts.headers);
                    return Response::from_parts(rparts, rbody.boxed());
                }
            }
        }
        if let Some(r) = last_503 {
            return r;
        }
        let waited = cfg.hold_timeout.as_secs();
        error(port, StatusCode::SERVICE_UNAVAILABLE, "master_not_discovered_exception", &format!("no search node became available within {waited}s"))
    }

    async fn send(&self, req: Request<SendBody>) -> Outcome {
        match self.client.request(req).await {
            Ok(r) => Outcome::Response(r),
            Err(e) if e.is_connect() => Outcome::NotSent(e.to_string()),
            Err(e) => {
                // The pool hands out a kept-alive connection the node had just closed: hyper
                // reports the request as canceled before anything was written.
                let canceled = std::error::Error::source(&e).and_then(|s| s.downcast_ref::<hyper::Error>()).is_some_and(|h| h.is_canceled());
                if canceled { Outcome::NotSent(e.to_string()) } else { Outcome::Broken(e.to_string()) }
            }
        }
    }

    /// Serves `listener` until `shutdown` flips to `true`; then stops accepting and lets
    /// requests in progress finish (up to `drain`).
    pub async fn serve(self: Arc<Self>, listener: TcpListener, opts: ListenOptions, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        let open = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(Notify::new());
        loop {
            let (socket, peer) = tokio::select! {
                r = listener.accept() => match r {
                    Ok(x) => x,
                    Err(e) => {
                        warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                },
                _ = stopped(&mut shutdown) => break,
            };
            let me = self.clone();
            let opts = opts.clone();
            let shutdown = shutdown.clone();
            let (open, done) = (open.clone(), done.clone());
            open.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Err(e) = me.connection(socket, peer, &opts, shutdown).await {
                    debug!(%peer, error = %e, "proxy connection ended");
                }
                if open.fetch_sub(1, Ordering::SeqCst) == 1 {
                    done.notify_waiters();
                }
            });
        }
        let deadline = Instant::now() + opts.drain;
        while open.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            let _ = tokio::time::timeout(Duration::from_millis(100), done.notified()).await;
        }
        Ok(())
    }

    async fn connection(self: Arc<Self>, socket: TcpStream, peer: SocketAddr, opts: &ListenOptions, shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        socket.set_nodelay(true)?;
        let mut socket = socket;
        let mut info = ClientInfo { addr: Some(peer), sni: None, tls: false };
        if opts.proxy_protocol {
            match tokio::time::timeout(Duration::from_secs(5), crate::proxy_protocol::read_header(&mut socket)).await {
                Ok(Ok(Some(a))) => info.addr = Some(a),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => anyhow::bail!("PROXY header: {e}"),
                Err(_) => anyhow::bail!("PROXY header timed out"),
            }
        }
        match &opts.tls {
            Some(acceptor) => {
                let tls = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket)).await.map_err(|_| anyhow::anyhow!("TLS handshake timed out"))??;
                info.sni = tls.get_ref().1.server_name().map(str::to_string);
                info.tls = true;
                self.http(TokioIo::new(tls), info, opts.port, shutdown).await
            }
            None => self.http(TokioIo::new(socket), info, opts.port, shutdown).await,
        }
    }

    async fn http<I>(self: Arc<Self>, io: I, info: ClientInfo, port: Port, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()>
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let info = Arc::new(info);
        let me = self.clone();
        let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
            let (me, info) = (me.clone(), info.clone());
            async move { Ok::<_, std::convert::Infallible>(me.forward(req, &info, port).await) }
        });
        let conn = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(30))
            .keep_alive(true)
            .pipeline_flush(true)
            .serve_connection(io, svc);
        let mut conn = std::pin::pin!(conn);
        tokio::select! {
            r = conn.as_mut() => return r.map_err(Into::into),
            _ = stopped(&mut shutdown) => {}
        }
        conn.as_mut().graceful_shutdown();
        conn.await.map_err(Into::into)
    }
}

#[derive(Clone)]
pub struct ListenOptions {
    pub port: Port,
    pub tls: Option<tokio_rustls::TlsAcceptor>,
    pub proxy_protocol: bool,
    /// How long requests in progress may finish after shutdown.
    pub drain: Duration,
}

impl ListenOptions {
    pub fn plain(port: Port) -> ListenOptions {
        ListenOptions { port, tls: None, proxy_protocol: false, drain: Duration::from_secs(30) }
    }
}

/// Resolves once `rx` holds `true` (never, if its sender is gone while `false`).
async fn stopped(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A TLS acceptor for a PEM certificate chain and key (HTTP/1.1 over ALPN).
pub fn tls_acceptor(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(!certs.is_empty(), "no certificate in the PEM");
    let key = PrivateKeyDer::from_pem_slice(key_pem)?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut cfg = ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_no_client_auth().with_single_cert(certs, key)?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

/// The cluster an API path names (`/v1/search/clusters/{id}/…`), for routing only.
fn api_cluster(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/v1/search/clusters/")?;
    let id = rest.split('/').next()?;
    (!id.is_empty()).then(|| id.to_string())
}

const HOP_BY_HOP: [&str; 8] = ["connection", "keep-alive", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade", "proxy-authorization"];

fn strip_hop_by_hop(h: &mut HeaderMap) {
    // Headers the client listed in `Connection` are hop-by-hop too.
    let listed: Vec<HeaderName> = h
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
        .collect();
    for n in listed {
        h.remove(n);
    }
    for n in HOP_BY_HOP {
        h.remove(n);
    }
}

fn forward_headers(src: &HeaderMap, client: &ClientInfo, domain: Option<&str>) -> HeaderMap {
    let mut h = src.clone();
    strip_hop_by_hop(&mut h);
    if let Some(addr) = client.addr {
        let ip = addr.ip().to_string();
        let xff = match h.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            Some(prev) if !prev.is_empty() => format!("{prev}, {ip}"),
            _ => ip,
        };
        if let Ok(v) = HeaderValue::from_str(&xff) {
            h.insert("x-forwarded-for", v);
        }
    }
    h.insert("x-forwarded-proto", HeaderValue::from_static(if client.tls { "https" } else { "http" }));
    // The cluster the TLS name selected must survive the plain hop to the node, which routes by
    // `Host` there.
    if let Some(sni) = &client.sni
        && route::host_label(sni, domain).is_some()
        && h.get(hyper::header::HOST).and_then(|v| v.to_str().ok()).and_then(|v| route::host_label(v, domain)).is_none()
        && let Ok(v) = HeaderValue::from_str(sni)
    {
        h.insert(hyper::header::HOST, v);
    }
    h
}

/// Whether resending a request that may have run is harmless: reads, and POSTs of the search
/// family (`_search`, `_msearch`, `_count`, `_mget`, …). Scroll continuations and anything
/// that creates state (point in time, async search) are not.
pub fn read_only(method: &Method, path: &str) -> bool {
    // A scroll page advances the cursor, whatever the method.
    if path.contains("/_search/scroll") {
        return false;
    }
    match *method {
        Method::GET | Method::HEAD | Method::OPTIONS => return true,
        Method::POST => {}
        _ => return false,
    }
    if path.contains("_async_search") || path.ends_with("/_pit") {
        return false;
    }
    const READS: [&str; 15] = [
        "_search",
        "_msearch",
        "_count",
        "_mget",
        "_field_caps",
        "_analyze",
        "_validate",
        "_explain",
        "_terms_enum",
        "_render",
        "_search_shards",
        "_rank_eval",
        "_termvectors",
        "_mtermvectors",
        "_knn_search",
    ];
    path.split('/').any(|seg| READS.contains(&seg))
}

/// `error.type` of an Elasticsearch error body.
fn error_type(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.pointer("/error/type").and_then(|t| t.as_str()).map(str::to_string)
}

/// Whether an answer shows the node that gave it holds the request's cluster. A node checks
/// credentials before it looks at leases, so a 401 (wrong password, unknown cluster, malformed
/// header) or a 403 says nothing about where the cluster lives: remembering it would let anyone
/// with bad credentials move another tenant's cached owner and add a hop to its traffic.
fn confirms_owner(status: StatusCode) -> bool {
    !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE)
}

/// The start of a body, read up to a bound.
enum Peeked {
    /// All of it (trailers dropped).
    Whole(Bytes),
    /// More than the bound: what was read, and the rest still to come.
    Partial(Bytes, Incoming),
}

async fn peek(mut body: Incoming, limit: usize) -> Result<Peeked, hyper::Error> {
    let mut buf = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame?.into_data() {
            buf.extend_from_slice(&data);
            if buf.len() > limit {
                return Ok(Peeked::Partial(buf.freeze(), body));
            }
        }
    }
    Ok(Peeked::Whole(buf.freeze()))
}

/// A response body whose first bytes were already read ([`peek`]): those, then the rest.
struct Prefixed {
    head: Option<Bytes>,
    rest: Incoming,
}

impl hyper::body::Body for Prefixed {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        if let Some(h) = self.head.take() {
            return std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(h))));
        }
        std::pin::Pin::new(&mut self.rest).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.head.is_none() && self.rest.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        let head = self.head.as_ref().map_or(0, |h| h.len() as u64);
        let rest = self.rest.size_hint();
        let mut hint = hyper::body::SizeHint::new();
        hint.set_lower(rest.lower() + head);
        if let Some(u) = rest.upper() {
            hint.set_upper(u + head);
        }
        hint
    }
}

/// An error answered by the proxy itself: Elasticsearch-shaped on the REST port (clients parse
/// and retry it), `{"error": …}` like the control API on the API port.
fn error(port: Port, status: StatusCode, kind: &str, reason: &str) -> Response<Body> {
    let body = match port {
        Port::Rest => serde_json::json!({
            "error": { "root_cause": [{ "type": kind, "reason": reason }], "type": kind, "reason": reason },
            "status": status.as_u16(),
        }),
        Port::Api => serde_json::json!({ "error": reason }),
    };
    let mut r = Response::new(full(body.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if port == Port::Rest {
        r.headers_mut().insert("x-elastic-product", HeaderValue::from_static("Elasticsearch"));
    }
    r
}

/// Answers every connection with `200 ok` (load balancer health checks: never touches nodes,
/// so checks don't wake a fleet that scaled to zero).
pub async fn serve_health(listener: TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut s, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = tokio::time::timeout(Duration::from_millis(500), s.read(&mut buf)).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_requests() {
        let get = Method::GET;
        let post = Method::POST;
        assert!(read_only(&get, "/idx/_doc/1"));
        assert!(read_only(&Method::HEAD, "/idx"));
        assert!(read_only(&post, "/idx/_search"));
        assert!(read_only(&post, "/_msearch"));
        assert!(read_only(&post, "/a,b/_count"));
        assert!(read_only(&post, "/_mget"));
        assert!(read_only(&post, "/idx/_search/template"));
        assert!(read_only(&post, "/idx/_explain/1"));
        assert!(!read_only(&post, "/_search/scroll"));
        assert!(!read_only(&get, "/_search/scroll?scroll_id=x"));
        assert!(!read_only(&post, "/idx/_pit"));
        assert!(!read_only(&post, "/idx/_async_search"));
        assert!(!read_only(&post, "/idx/_doc"));
        assert!(!read_only(&post, "/_bulk"));
        assert!(!read_only(&Method::PUT, "/idx/_doc/1"));
        assert!(!read_only(&Method::DELETE, "/idx"));
        assert!(!read_only(&post, "/idx/_update/1"));
        // An index named like an API is still just an index.
        assert!(!read_only(&post, "/searches/_doc"));
    }

    #[test]
    fn hop_by_hop_headers_go() {
        let mut h = HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("keep-alive, x-secret-hop"));
        h.insert("x-secret-hop", HeaderValue::from_static("1"));
        h.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        h.insert("authorization", HeaderValue::from_static("Basic eA=="));
        h.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1"));
        let info = ClientInfo { addr: Some("203.0.113.9:1234".parse().unwrap()), sni: Some("app.s.example.com".into()), tls: true };
        let out = forward_headers(&h, &info, Some("s.example.com"));
        assert!(out.get("connection").is_none() && out.get("x-secret-hop").is_none() && out.get("keep-alive").is_none());
        assert_eq!(out["authorization"], "Basic eA==");
        assert_eq!(out["x-forwarded-for"], "198.51.100.1, 203.0.113.9");
        assert_eq!(out["x-forwarded-proto"], "https");
        assert_eq!(out["host"], "app.s.example.com", "the SNI cluster survives the plain hop");
        // A Host that already names a cluster is kept.
        h.insert("host", HeaderValue::from_static("other.s.example.com"));
        assert_eq!(forward_headers(&h, &info, Some("s.example.com"))["host"], "other.s.example.com");
    }

    #[test]
    fn api_paths() {
        assert_eq!(api_cluster("/v1/search/clusters/app/request").as_deref(), Some("app"));
        assert_eq!(api_cluster("/v1/search/clusters/srch_x").as_deref(), Some("srch_x"));
        assert_eq!(api_cluster("/v1/search/clusters"), None);
        assert_eq!(api_cluster("/v1/search/clusters/"), None);
        assert_eq!(api_cluster("/v1/search"), None);
    }

    #[test]
    fn errors_have_the_port_shape() {
        assert_eq!(error_type(br#"{"error":{"type":"node_closed_exception","reason":"x"},"status":503}"#).as_deref(), Some("node_closed_exception"));
        assert_eq!(error_type(b"not json"), None);
        let r = error(Port::Rest, StatusCode::SERVICE_UNAVAILABLE, "master_not_discovered_exception", "none");
        assert_eq!(r.status(), 503);
        assert_eq!(r.headers()["x-elastic-product"], "Elasticsearch");
        assert!(error(Port::Api, StatusCode::BAD_GATEWAY, "x", "y").headers().get("x-elastic-product").is_none());
    }
}

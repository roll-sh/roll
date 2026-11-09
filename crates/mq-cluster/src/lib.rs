//! roll queues on many nodes: every node of a roll fleet acting as one RabbitMQ node.
//!
//! - **Shared state.** Topology lives in one metadata store (the system database, through
//!   [`pg::PgMeta`]) and message logs in shared storage (`mq-storage`: S3 on the fleet). Every
//!   node follows the store's change feed and applies it ([`Broker::apply_change`]).
//! - **Membership and ownership** are roll-cluster's (docs/cloud/README.md): each durable queue
//!   partition has one owner, the holder of the lease [`queue_key`] (`mq:<vhost>/<queue>`).
//!   The owner opens the partition's log with the lease's epoch, which fences every older owner
//!   out of the log store: correctness never depends on the lease.
//! - **Routing.** Every node has a [`router`] per queue that forwards to the owner, in process
//!   or over [`rpc`] (an upgraded, signed connection on the internal API). Ownership moves
//!   without client connections moving: routers retarget and re-register their consumers.
//! - **Moves.** roll-cluster hands leases off (rebalancing, a drain) and takes over a dead
//!   node's. A handoff ends the owner's guard: the router pauses deliveries, lets in-flight acks
//!   land, stops the actor (every write flushed, the log closed), and only then lets go of the
//!   lease. A dead owner's queues come back on whichever node takes its leases, replayed from
//!   the log; confirmed messages are durable there, so none are lost.

pub mod pg;
pub use pg::PgMeta;
mod router;
pub mod rpc;

use async_trait::async_trait;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use mq_core::cluster::{Placement, QueueStart};
use mq_core::log::LogStore;
use mq_core::meta::MetaStore;
use mq_core::queue::stream::{ChunkSubscription, NativePublish};
use mq_core::queue::{ConfirmTicket, ConsumerSpec, Outcome, PublishResult, QueueCmd, QueueRef};
use mq_core::{AmqpError, Broker, FieldTable, Message};
use parking_lot::{Mutex, RwLock};
use roll_cluster::{Guard, Ownership, Route};
use router::{Ctl, RemoteChunkSink, RemoteSink, Router, RouterRef};
use rpc::{Frame, Peer, Push, Req};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// How queue leases start: `mq:<vhost>/<queue>` (docs/cloud/README.md, "Ownership keys").
pub const KEY_PREFIX: &str = "mq:";

#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// Client-facing addresses by protocol (`amqp`, `stream`, …): stream clients are sent to a
    /// stream's owner by them.
    pub listeners: BTreeMap<String, SocketAddr>,
    /// How often to read the metadata change feed.
    pub follow: Duration,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig { listeners: BTreeMap::new(), follow: Duration::from_millis(100) }
    }
}

/// A queue actor running on this node.
pub(crate) struct OwnedQueue {
    pub inner: QueueRef,
    /// Set during handoff: consumers stop receiving.
    pub paused: Arc<AtomicBool>,
}

pub(crate) enum Decision {
    /// This node owns the queue: run it under the lease's guard.
    Local(Guard),
    Remote(Arc<Peer>),
    Wait,
}

pub fn outcome_code(o: Outcome) -> u8 {
    match o {
        Outcome::Ack => 0,
        Outcome::Requeue => 1,
        Outcome::Reject => 2,
    }
}

fn outcome_of(c: u8) -> Outcome {
    match c {
        1 => Outcome::Requeue,
        2 => Outcome::Reject,
        _ => Outcome::Ack,
    }
}

/// `%`, `/` and `#` (and control characters) escaped: they separate the key's parts.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '/' | '#') || c.is_control() {
            let mut b = [0u8; 4];
            for byte in c.encode_utf8(&mut b).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The lease of a queue partition: `mq:<vhost>/<queue>`, then `#<n>` for partitions after the
/// first, with `%`, `/` and `#` in names escaped (the default vhost is `%2F`).
pub fn queue_key(vhost: &str, name: &str, partition: u32) -> String {
    match partition {
        0 => format!("{KEY_PREFIX}{}/{}", escape(vhost), escape(name)),
        p => format!("{KEY_PREFIX}{}/{}#{p}", escape(vhost), escape(name)),
    }
}

/// `(vhost, queue, partition)` of a [`queue_key`].
pub fn parse_key(key: &str) -> Option<(String, String, u32)> {
    let rest = key.strip_prefix(KEY_PREFIX)?;
    let (vhost, rest) = rest.split_once('/')?;
    let (name, partition) = match rest.split_once('#') {
        Some((n, p)) => (n, p.parse().ok()?),
        None => (rest, 0),
    };
    Some((unescape(vhost)?, unescape(name)?, partition))
}

pub struct Cluster {
    /// The node's membership and leases (shared with every product on the node).
    pub nodes: Arc<roll_cluster::Cluster>,
    pub config: ClusterConfig,
    pub meta: Arc<dyn MetaStore>,
    logs: Arc<dyn LogStore>,
    /// Namespaces this process's channel and connection ids at owners on other nodes.
    num: u16,
    broker: OnceLock<Weak<Broker>>,
    pub(crate) owned: RwLock<HashMap<String, Arc<OwnedQueue>>>,
    pub(crate) routers: RwLock<HashMap<String, Arc<RouterRef>>>,
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    connecting: tokio::sync::Mutex<()>,
    conns: Mutex<Vec<Weak<ServerConn>>>,
    next_sub: AtomicU64,
    stopped: AtomicBool,
    version: AtomicU64,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Cluster {
    /// Take part in serving queues on `nodes`. Then start the broker with
    /// [`Cluster::placement`], call [`Cluster::attach`], and serve [`Cluster::routes`] on the
    /// node's internal API (where `nodes` advertises it).
    pub async fn join(nodes: Arc<roll_cluster::Cluster>, config: ClusterConfig, meta: Arc<dyn MetaStore>, logs: Arc<dyn LogStore>) -> anyhow::Result<Arc<Cluster>> {
        let (_, version) = meta.changes_since(i64::MAX).await?;
        let c = Arc::new(Cluster {
            nodes,
            config,
            meta,
            logs,
            num: ((mq_core::rand_u64() % 0xfffe) + 1) as u16,
            broker: OnceLock::new(),
            owned: RwLock::new(HashMap::new()),
            routers: RwLock::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            connecting: tokio::sync::Mutex::new(()),
            conns: Mutex::new(Vec::new()),
            next_sub: AtomicU64::new(1),
            stopped: AtomicBool::new(false),
            version: AtomicU64::new(version.max(0) as u64),
            tasks: Mutex::new(Vec::new()),
        });
        let cc = c.clone();
        c.tasks.lock().push(tokio::spawn(async move { cc.stats_loop().await }));
        info!(node = %c.nodes.node_id(), "queues: cluster node");
        Ok(c)
    }

    /// The placement to start the broker with.
    pub fn placement(self: &Arc<Self>) -> Arc<dyn Placement> {
        Arc::new(ClusterPlacement(self.clone()))
    }

    /// Follow the change feed into `broker`, and start queues as this node takes their leases.
    pub fn attach(self: &Arc<Self>, broker: &Arc<Broker>) {
        let _ = self.broker.set(Arc::downgrade(broker));
        // Subscribe now: leases taken from here on reach the loop.
        let events = self.nodes.subscribe_ownership();
        let (c, c2) = (self.clone(), self.clone());
        let mut tasks = self.tasks.lock();
        tasks.push(tokio::spawn(async move { c.follow().await }));
        tasks.push(tokio::spawn(async move { c2.ownership(events).await }));
    }

    /// `/internal/queues/rpc`, for the node's internal API: other nodes open their RPC
    /// connection to this one there (signed).
    pub fn routes(self: &Arc<Self>) -> axum::Router {
        let c = self.clone();
        axum::Router::new()
            .route(
                rpc::PATH,
                axum::routing::get(move |req: Request| {
                    let c = c.clone();
                    async move { c.accept(req) }
                }),
            )
            .route_layer(axum::middleware::from_fn_with_state(self.nodes.clone(), roll_cluster::require_signed))
    }

    fn broker(&self) -> Option<Arc<Broker>> {
        self.broker.get().and_then(|w| w.upgrade())
    }

    pub(crate) fn broker_logs(&self) -> Arc<dyn LogStore> {
        self.logs.clone()
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(crate) fn next_sub(&self) -> u64 {
        self.next_sub.fetch_add(1, Ordering::Relaxed)
    }

    /// Channel/connection ids are per process; namespace them before sending to an owner.
    pub(crate) fn ns(&self, id: u64) -> u64 {
        ((self.num as u64) << 48) | (id & ((1 << 48) - 1))
    }

    /// Queues this node runs.
    pub fn owned_count(&self) -> usize {
        self.owned.read().len()
    }

    pub fn owned_keys(&self) -> Vec<String> {
        self.owned.read().keys().cloned().collect()
    }

    /// Where a queue runs: here (with the lease's guard), on another node, or nowhere yet (the
    /// lease is moving, or its holder died and the lease hasn't lapsed).
    pub(crate) async fn decide(&self, key: &str) -> Decision {
        match self.nodes.route_async(key).await {
            Ok(Route::Local(g)) => Decision::Local(g),
            Ok(Route::Forward(node)) => match self.peer(&node).await {
                Some(p) => Decision::Remote(p),
                None => {
                    self.nodes.forget(key);
                    Decision::Wait
                }
            },
            Ok(Route::Unavailable { .. }) => Decision::Wait,
            Err(e) => {
                debug!(queue = %key, error = %e, "routing a queue failed");
                Decision::Wait
            }
        }
    }

    pub(crate) async fn peer(&self, node: &roll_cluster::Node) -> Option<Arc<Peer>> {
        if let Some(p) = self.peers.lock().get(&node.id).filter(|p| !p.is_closed()) {
            return Some(p.clone());
        }
        let _g = self.connecting.lock().await;
        if let Some(p) = self.peers.lock().get(&node.id).filter(|p| !p.is_closed()) {
            return Some(p.clone());
        }
        match Peer::connect(node, &self.nodes).await {
            Ok(p) => {
                self.peers.lock().insert(node.id.clone(), p.clone());
                Some(p)
            }
            Err(e) => {
                debug!(node = %node.id, error = format!("{e:#}"), "can't reach node");
                None
            }
        }
    }

    /// Get `key`'s actor running if this node should own it: its router routes again, or (no
    /// router: the vhost is asleep here) the vhost wakes and starts its routers. A lease taken
    /// for a queue that no longer exists is given up.
    fn kick(self: &Arc<Self>, key: &str) {
        if let Some(r) = self.routers.read().get(key) {
            let _ = r.ctl.send(Ctl::Retarget);
            return;
        }
        let Some((vhost, name, _)) = parse_key(key) else { return };
        let Some(broker) = self.broker() else { return };
        let (c, key) = (self.clone(), key.to_string());
        tokio::spawn(async move {
            broker.open_vhost(&vhost).await;
            if c.routers.read().contains_key(&key) || !c.nodes.owns(&key) {
                return;
            }
            // Deleted queues only: a queue's log refuses lease epochs older than its last owner's,
            // and a deleted lease starts over at 1 (a new queue of that name has a new log).
            if let Ok(None) = c.meta.get(mq_core::meta::kind::QUEUE, &mq_core::meta::key(&[&vhost, &name])).await {
                debug!(queue = %key, "giving up the lease of a deleted queue");
                c.nodes.retire(&key);
            }
        });
    }

    /// Tell consumers subscribed from other nodes that `key` moved.
    pub(crate) fn notify_moved(&self, key: &str) {
        let conns: Vec<Arc<ServerConn>> = self.conns.lock().iter().filter_map(|w| w.upgrade()).collect();
        for c in conns {
            let subs: Vec<u64> = c.subs.lock().iter().filter(|(_, s)| s.key == key).map(|(id, _)| *id).collect();
            for sub in subs {
                c.subs.lock().remove(&sub);
                c.push(Push::Moved { sub });
            }
            let subs: Vec<u64> = c.chunk_subs.lock().iter().filter(|(_, s)| s.key == key).map(|(id, _)| *id).collect();
            for sub in subs {
                c.chunk_subs.lock().remove(&sub);
                c.push(Push::Moved { sub });
            }
        }
    }

    /// Stop background work (after the node's leases were handed off, or it was killed).
    pub async fn leave(&self) {
        self.stopped.store(true, Ordering::Release);
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
    }

    // ── background loops ──

    async fn follow(self: Arc<Self>) {
        loop {
            tokio::time::sleep(self.config.follow).await;
            if self.stopped() {
                return;
            }
            let from = self.version.load(Ordering::Acquire) as i64;
            let (changes, latest) = match self.meta.changes_since(from).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "change feed read failed");
                    continue;
                }
            };
            let Some(broker) = self.broker() else { return };
            let mut seen = HashSet::new();
            for ch in changes {
                // Reconcile each key once per batch; it reads the current value anyway.
                if seen.insert((ch.kind.clone(), ch.key.clone()))
                    && let Err(e) = broker.apply_change(&ch.kind, &ch.key).await
                {
                    warn!(kind = %ch.kind, key = %ch.key, error = %e, "failed to apply metadata change");
                }
            }
            self.version.store(latest.max(from) as u64, Ordering::Release);
        }
    }

    /// Start queues whose leases this node takes (a dead node's, or one handed to it), and stop
    /// the ones it loses.
    async fn ownership(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Ownership>) {
        while let Some(ev) = events.recv().await {
            if self.stopped() {
                return;
            }
            match ev {
                Ownership::Acquired { key, .. } if key.starts_with(KEY_PREFIX) => self.kick(&key),
                Ownership::Lost { key } if key.starts_with(KEY_PREFIX) => {
                    if let Some(r) = self.routers.read().get(&key) {
                        let _ = r.ctl.send(Ctl::Retarget);
                    }
                }
                _ => {}
            }
        }
    }

    /// Refresh queue counters on routers whose queue runs elsewhere.
    async fn stats_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if self.stopped() {
                return;
            }
            type Queues = Vec<(String, Weak<mq_core::queue::QueueHandle>)>;
            let mut by_node: HashMap<String, (Arc<Peer>, Queues)> = HashMap::new();
            for r in self.routers.read().values() {
                if let Some(p) = r.remote.lock().clone() {
                    by_node.entry(p.node.clone()).or_insert_with(|| (p, Vec::new())).1.push((r.key.clone(), r.handle.clone()));
                }
            }
            for (_, (peer, qs)) in by_node {
                if peer.is_closed() {
                    continue;
                }
                let keys = qs.iter().map(|q| q.0.clone()).collect();
                if let rpc::Reply::Ok(v, _) = peer.call(&Req::Stats { qs: keys }, vec![]).await {
                    for ((_, h), s) in qs.iter().zip(v.as_array().cloned().unwrap_or_default()) {
                        if let (Some(h), Some(arr)) = (h.upgrade(), s.as_array()) {
                            let vals: Vec<u64> = arr.iter().map(|x| x.as_u64().unwrap_or(0)).collect();
                            router::apply_stats(&h.shared, &vals);
                        }
                    }
                }
            }
        }
    }

    // ── RPC server ──

    fn accept(self: Arc<Self>, mut req: Request) -> Response {
        let upgrade = req.headers().get(header::UPGRADE).and_then(|v| v.to_str().ok());
        if !upgrade.is_some_and(|u| u.eq_ignore_ascii_case(rpc::PROTOCOL)) {
            return (StatusCode::BAD_REQUEST, "expected an upgrade\n").into_response();
        }
        let on = hyper::upgrade::on(&mut req);
        let c = self.clone();
        tokio::spawn(async move {
            match on.await {
                Ok(up) => c.serve_conn(hyper_util::rt::TokioIo::new(up)).await,
                Err(e) => debug!(error = %e, "queues rpc: upgrade failed"),
            }
        });
        let mut resp = StatusCode::SWITCHING_PROTOCOLS.into_response();
        let h = resp.headers_mut();
        h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(header::UPGRADE, HeaderValue::from_static(rpc::PROTOCOL));
        if let Ok(v) = HeaderValue::from_str(&serde_json::to_string(&self.config.listeners).unwrap_or_default()) {
            h.insert(rpc::LISTENERS_HEADER, v);
        }
        resp
    }

    async fn serve_conn<S: AsyncRead + AsyncWrite + Send + 'static>(self: Arc<Self>, stream: S) {
        let (r, mut w) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<Frame>(16384);
        tokio::spawn(async move {
            let mut buf = BytesMut::with_capacity(64 << 10);
            while let Some(f) = rx.recv().await {
                f.encode(&mut buf);
                while buf.len() < (1 << 20) {
                    match rx.try_recv() {
                        Ok(f) => f.encode(&mut buf),
                        Err(_) => break,
                    }
                }
                if w.write_all(&buf).await.is_err() || w.flush().await.is_err() {
                    break;
                }
                buf.clear();
            }
        });
        let conn = Arc::new(ServerConn { tx, subs: Mutex::new(HashMap::new()), chunk_subs: Mutex::new(HashMap::new()), channels: Mutex::new(HashSet::new()) });
        self.conns.lock().push(Arc::downgrade(&conn));
        let mut r = BufReader::with_capacity(256 << 10, r);
        // Once the other node pings, a connection silent for longer than it pings is dead.
        let mut pinged = false;
        loop {
            let f = tokio::select! {
                f = Frame::read(&mut r) => match f {
                    Ok(Some(f)) => f,
                    _ => break,
                },
                _ = tokio::time::sleep(rpc::DEAD_AFTER), if pinged => {
                    warn!("queues rpc: a node stopped pinging; dropping its connection");
                    break;
                }
            };
            if f.kind != rpc::REQ {
                continue;
            }
            let Ok(req) = serde_json::from_value::<Req>(f.head) else {
                conn.reply(rpc::err_frame(f.id, &AmqpError::command_invalid("bad request")));
                continue;
            };
            if let Req::Ping = req {
                pinged = true;
                conn.reply(rpc::ok_frame(f.id, Value::Null, vec![]));
                continue;
            }
            self.handle(&conn, f.id, req, f.blobs).await;
        }
        // The other node is gone: drop its stream subscriptions, and requeue what its channels held.
        let chunk_subs: Vec<ChunkServerSub> = conn.chunk_subs.lock().drain().map(|(_, s)| s).collect();
        for s in chunk_subs {
            s.queue.stream_unsubscribe(s.reg).await;
        }
        let chans: Vec<(String, u64)> = conn.channels.lock().drain().collect();
        for (key, ch) in chans {
            let o = self.owned.read().get(&key).cloned();
            if let Some(o) = o {
                o.inner.channel_closed(ch).await;
            }
        }
        self.conns.lock().retain(|w| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(&conn)));
    }

    async fn handle(self: &Arc<Self>, conn: &Arc<ServerConn>, id: u64, req: Req, blobs: Vec<Bytes>) {
        let key = match &req {
            Req::Credit { sub, n } => {
                if let Some(s) = conn.subs.lock().get(sub) {
                    s.sink.credit.fetch_add(*n, Ordering::AcqRel);
                    s.queue.wake();
                } else if let Some(s) = conn.chunk_subs.lock().get(sub) {
                    s.sink.credit.fetch_add(*n, Ordering::AcqRel);
                    s.queue.wake();
                }
                return;
            }
            Req::Ping => return,
            Req::Stats { qs } => {
                let owned = self.owned.read();
                let v: Vec<Value> = qs.iter().map(|k| owned.get(k).map(|o| json!(router::stats_of(&o.inner.shared))).unwrap_or(Value::Null)).collect();
                conn.reply(rpc::ok_frame(id, Value::Array(v), vec![]));
                return;
            }
            Req::StreamUnsubscribe { sub, .. } => {
                let s = conn.chunk_subs.lock().remove(sub);
                if let Some(s) = s {
                    s.queue.stream_unsubscribe(s.reg).await;
                }
                return;
            }
            Req::Publish { q, .. }
            | Req::Consume { q, .. }
            | Req::Cancel { q, .. }
            | Req::Get { q, .. }
            | Req::Settle { q, .. }
            | Req::Release { q, .. }
            | Req::Modify { q, .. }
            | Req::ChannelClosed { q, .. }
            | Req::Recover { q, .. }
            | Req::Purge { q }
            | Req::Delete { q, .. }
            | Req::Lease { q, .. }
            | Req::SettleLease { q, .. }
            | Req::ExtendLease { q, .. }
            | Req::Peek { q, .. }
            | Req::Touch { q }
            | Req::Consumers { q }
            | Req::StreamPublish { q, .. }
            | Req::StreamSubscribe { q, .. }
            | Req::StreamResolve { q, .. }
            | Req::StreamWriterSequence { q, .. }
            | Req::StreamStats { q } => q.clone(),
        };
        let Some(owned) = self.owned.read().get(&key).cloned() else {
            // Not running here: the sender routes again, and this node starts the queue if it
            // should own it now (a free lease placement gives it, or one it just took).
            self.kick(&key);
            conn.reply(rpc::moved_frame(id));
            return;
        };
        let q = owned.inner.clone();
        let reply = conn.clone();
        match req {
            Req::Publish { wait, confirm, .. } => {
                let Some(mut b) = blobs.into_iter().next() else { return };
                let Ok(msg) = Message::decode(&mut b) else {
                    conn.reply(rpc::err_frame(id, &AmqpError::command_invalid("bad message")));
                    return;
                };
                let (ctx, crx) = oneshot::channel();
                let ticket = confirm.then(|| {
                    ConfirmTicket::new(move |ok| {
                        let _ = ctx.send(ok);
                    })
                });
                let (rtx, rrx) = oneshot::channel();
                if q.send(QueueCmd::Publish { msg: Arc::new(msg), confirm: ticket, reply: wait.then_some(rtx) }).await.is_err() {
                    conn.reply(rpc::moved_frame(id));
                    return;
                }
                if !confirm && !wait {
                    conn.reply(rpc::ok_frame(id, json!({}), vec![]));
                    return;
                }
                tokio::spawn(async move {
                    let confirmed = if confirm { crx.await.unwrap_or(false) } else { true };
                    let result = if wait {
                        match rrx.await {
                            Ok(PublishResult::Rejected) => "rejected",
                            Ok(PublishResult::Duplicate) => "duplicate",
                            _ => "accepted",
                        }
                    } else {
                        "accepted"
                    };
                    reply.reply(rpc::ok_frame(id, json!({ "confirmed": confirmed, "result": result }), vec![]));
                });
            }
            Req::Consume { sub, channel, conn: cid, tag, no_ack, exclusive, priority, credit, .. } => {
                let args = blobs.into_iter().next().and_then(|mut b| FieldTable::decode(&mut b).ok()).unwrap_or_default();
                let sink = Arc::new(RemoteSink { sub, tx: conn.tx.clone(), credit: AtomicU32::new(credit), paused: owned.paused.clone() });
                let spec = ConsumerSpec { tag: Arc::from(tag.as_str()), channel, conn: cid, no_ack, exclusive, priority, args, sink: sink.clone() };
                conn.channels.lock().insert((key.clone(), channel));
                match q.consume(spec).await {
                    Ok(()) => {
                        conn.subs.lock().insert(sub, ServerSub { key, channel, queue: q.clone(), sink });
                        conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
                        q.wake();
                    }
                    Err(e) => conn.reply(rpc::err_frame(id, &e)),
                }
            }
            Req::Cancel { sub, channel, tag, .. } => {
                conn.subs.lock().remove(&sub);
                tokio::spawn(async move {
                    let ok = q.cancel(channel, Arc::from(tag.as_str())).await;
                    reply.reply(rpc::ok_frame(id, json!(ok), vec![]));
                });
            }
            Req::Get { channel, no_ack, .. } => {
                conn.channels.lock().insert((key, channel));
                tokio::spawn(async move {
                    match q.get(channel, no_ack).await {
                        Ok(Some(d)) => {
                            let head = json!({ "seq": d.seq, "redelivered": d.redelivered, "delivery_count": d.delivery_count, "remaining": d.remaining });
                            reply.reply(rpc::ok_frame(id, head, vec![encode_msg(&d.msg)]));
                        }
                        Ok(None) => reply.reply(rpc::ok_frame(id, Value::Null, vec![])),
                        Err(e) => reply.reply(rpc::err_frame(id, &e)),
                    }
                });
            }
            Req::Settle { channel, seqs, outcome, .. } => {
                let _ = q.send(QueueCmd::Settle { channel, seqs, outcome: outcome_of(outcome) }).await;
                conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
            }
            Req::Release { channel, seqs, .. } => {
                let _ = q.send(QueueCmd::Release { channel, seqs }).await;
                conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
            }
            Req::Modify { channel, seqs, failed, undeliverable, .. } => {
                let annotations = blobs.into_iter().next().and_then(|mut b| FieldTable::decode(&mut b).ok()).unwrap_or_default();
                let modification = mq_core::queue::Modification { annotations, delivery_failed: failed, undeliverable_here: undeliverable };
                let _ = q.send(QueueCmd::Modify { channel, seqs, modification: Box::new(modification) }).await;
                conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
            }
            Req::ChannelClosed { channel, .. } => {
                conn.channels.lock().remove(&(key.clone(), channel));
                conn.subs.lock().retain(|_, s| !(s.key == key && s.channel == channel));
                q.channel_closed(channel).await;
                conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
            }
            Req::Recover { channel, .. } => {
                let _ = q.send(QueueCmd::Recover { channel }).await;
                conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
            }
            Req::Touch { .. } => {
                let _ = q.send(QueueCmd::Touch).await;
                conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
            }
            Req::Purge { .. } => {
                tokio::spawn(async move {
                    let n = q.purge().await.unwrap_or(0);
                    reply.reply(rpc::ok_frame(id, json!(n), vec![]));
                });
            }
            Req::Delete { if_unused, if_empty, .. } => {
                tokio::spawn(async move {
                    let (tx, rx) = oneshot::channel();
                    let r = match q.send(QueueCmd::Delete { if_unused, if_empty, reply: tx }).await {
                        Ok(()) => rx.await.unwrap_or_else(|_| Err(AmqpError::not_found("queue gone"))),
                        Err(e) => Err(e),
                    };
                    match r {
                        Ok(n) => reply.reply(rpc::ok_frame(id, json!(n), vec![])),
                        Err(e) => reply.reply(rpc::err_frame(id, &e)),
                    }
                });
            }
            Req::Lease { max, visibility_ms, .. } => {
                tokio::spawn(async move {
                    let ls = q.lease(max, visibility_ms).await.unwrap_or_default();
                    let head: Vec<Value> = ls.iter().map(|l| json!({ "seq": l.seq, "delivery_count": l.delivery_count, "redelivered": l.redelivered, "deadline_ms": l.deadline_ms })).collect();
                    let blobs = ls.iter().map(|l| encode_msg(&l.msg)).collect();
                    reply.reply(rpc::ok_frame(id, Value::Array(head), blobs));
                });
            }
            Req::SettleLease { seq, delivery_count, outcome, .. } => {
                tokio::spawn(async move {
                    let ok = q.settle_lease(seq, delivery_count, outcome_of(outcome)).await.unwrap_or(false);
                    reply.reply(rpc::ok_frame(id, json!(ok), vec![]));
                });
            }
            Req::ExtendLease { seq, delivery_count, visibility_ms, .. } => {
                tokio::spawn(async move {
                    let ok = q.extend_lease(seq, delivery_count, visibility_ms).await.unwrap_or(false);
                    reply.reply(rpc::ok_frame(id, json!(ok), vec![]));
                });
            }
            Req::Peek { offset, count, .. } => {
                tokio::spawn(async move {
                    let ps = q.peek(offset, count).await.unwrap_or_default();
                    let head: Vec<Value> = ps.iter().map(|p| json!({ "seq": p.seq, "redelivered": p.redelivered, "delivery_count": p.delivery_count })).collect();
                    let blobs = ps.iter().map(|p| encode_msg(&p.msg)).collect();
                    reply.reply(rpc::ok_frame(id, Value::Array(head), blobs));
                });
            }
            Req::Consumers { .. } => {
                tokio::spawn(async move {
                    let cs = q.consumers().await;
                    reply.reply(rpc::ok_frame(id, serde_json::to_value(cs).unwrap_or(Value::Null), vec![]));
                });
            }
            Req::StreamPublish { writer, ids, .. } => {
                if ids.len() != blobs.len() {
                    conn.reply(rpc::err_frame(id, &AmqpError::command_invalid("bad stream publish")));
                    return;
                }
                let (ctx, crx) = oneshot::channel();
                let confirm = ConfirmTicket::new(move |ok| {
                    let _ = ctx.send(ok);
                });
                let entries = ids.into_iter().zip(blobs).collect();
                if q.stream_publish(NativePublish { writer: writer.map(Arc::from), entries, confirm }).await.is_err() {
                    conn.reply(rpc::moved_frame(id));
                    return;
                }
                tokio::spawn(async move {
                    let confirmed = crx.await.unwrap_or(false);
                    reply.reply(rpc::ok_frame(id, json!({ "confirmed": confirmed }), vec![]));
                });
            }
            Req::StreamSubscribe { sub, conn: cid, tag, offset, filtered, match_unfiltered, credit, max_bytes, .. } => {
                let mut blobs = blobs.into_iter();
                let properties = blobs.next().and_then(|mut b| FieldTable::decode(&mut b).ok()).unwrap_or_default();
                let filters = filtered.then(|| blobs.collect());
                let Some(broker) = self.broker() else { return };
                let reg = broker.next_id();
                let sink = Arc::new(RemoteChunkSink { sub, tx: conn.tx.clone(), credit: AtomicU32::new(credit), max_bytes, paused: owned.paused.clone() });
                let cs = ChunkSubscription { id: reg, conn: cid, tag: Arc::from(tag.as_str()), offset: offset.into(), filters, match_unfiltered, properties, sink: sink.clone() };
                match q.stream_subscribe(cs).await {
                    Ok(next) => {
                        conn.chunk_subs.lock().insert(sub, ChunkServerSub { key, queue: q.clone(), reg, sink });
                        conn.reply(rpc::ok_frame(id, json!({ "next": next }), vec![]));
                        q.wake();
                    }
                    Err(_) => conn.reply(rpc::moved_frame(id)),
                }
            }
            Req::StreamResolve { offset, .. } => {
                tokio::spawn(async move {
                    match q.stream_resolve(offset.into()).await {
                        Ok(n) => reply.reply(rpc::ok_frame(id, json!(n), vec![])),
                        Err(_) => reply.reply(rpc::moved_frame(id)),
                    }
                });
            }
            Req::StreamWriterSequence { writer, seed, .. } => {
                tokio::spawn(async move {
                    match q.stream_writer_sequence(&writer, seed).await {
                        Ok(n) => reply.reply(rpc::ok_frame(id, json!(n), vec![])),
                        Err(_) => reply.reply(rpc::moved_frame(id)),
                    }
                });
            }
            Req::StreamStats { .. } => {
                tokio::spawn(async move {
                    match q.stream_stats().await {
                        Ok(s) => reply.reply(rpc::ok_frame(id, json!({ "first": s.first, "next": s.next }), vec![])),
                        Err(_) => reply.reply(rpc::moved_frame(id)),
                    }
                });
            }
            Req::Credit { .. } | Req::Stats { .. } | Req::StreamUnsubscribe { .. } | Req::Ping => {}
        }
    }
}

fn encode_msg(m: &Message) -> Bytes {
    let mut b = BytesMut::with_capacity(m.encoded_len());
    m.encode(&mut b);
    b.freeze()
}

struct ServerSub {
    key: String,
    channel: u64,
    queue: QueueRef,
    sink: Arc<RemoteSink>,
}

/// A stream-protocol subscription from another node, registered with the stream's actor.
struct ChunkServerSub {
    key: String,
    queue: QueueRef,
    /// Its id at the actor.
    reg: u64,
    sink: Arc<RemoteChunkSink>,
}

/// A connection from another node.
struct ServerConn {
    tx: mpsc::Sender<Frame>,
    subs: Mutex<HashMap<u64, ServerSub>>,
    chunk_subs: Mutex<HashMap<u64, ChunkServerSub>>,
    channels: Mutex<HashSet<(String, u64)>>,
}

impl ServerConn {
    fn reply(&self, f: Frame) {
        let tx = self.tx.clone();
        if let Err(mpsc::error::TrySendError::Full(f)) = tx.try_send(f) {
            tokio::spawn(async move {
                let _ = tx.send(f).await;
            });
        }
    }

    fn push(&self, ev: Push) {
        let head = serde_json::to_value(ev).unwrap_or(Value::Null);
        self.reply(Frame { kind: rpc::PUSH, id: 0, head, blobs: vec![] });
    }
}

struct ClusterPlacement(Arc<Cluster>);

#[async_trait]
impl Placement for ClusterPlacement {
    async fn start(&self, start: QueueStart) -> mq_core::Result<QueueRef> {
        let c = &self.0;
        let key = queue_key(&start.meta.vhost, &start.meta.name, start.partition);
        if let Some(old) = c.routers.write().remove(&key) {
            let _ = old.ctl.send(Ctl::Forget { deleted: false });
        }
        Router::spawn(c, key, start).await
    }

    fn forget(&self, vhost: &str, name: &str) {
        let key = queue_key(vhost, name, 0);
        let partitions = format!("{key}#");
        let gone: Vec<Arc<RouterRef>> = {
            let mut r = self.0.routers.write();
            let keys: Vec<String> = r.keys().filter(|k| **k == key || k.starts_with(&partitions)).cloned().collect();
            keys.iter().filter_map(|k| r.remove(k)).collect()
        };
        for r in gone {
            let _ = r.ctl.send(Ctl::Forget { deleted: true });
        }
    }

    fn locate(&self, vhost: &str, name: &str) -> Option<mq_core::cluster::Located> {
        let c = &self.0;
        let key = queue_key(vhost, name, 0);
        let remote = c.routers.read().get(&key)?.remote.lock().clone();
        if let Some(o) = c.owned.read().get(&key) {
            return Some(mq_core::cluster::Located::Local(o.inner.clone()));
        }
        let p = remote?;
        Some(mq_core::cluster::Located::Remote { node: p.node.clone(), listeners: p.listeners.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_keys_round_trip_whatever_the_names() {
        assert_eq!(queue_key("/", "orders", 0), "mq:%2F/orders");
        assert_eq!(queue_key("shop", "a/b#c%", 2), "mq:shop/a%2Fb%23c%25#2");
        for (v, n, p) in [("/", "orders", 0), ("a/b", "c", 0), ("a", "b/c", 3), ("ü", "%41\n", 1)] {
            assert_eq!(parse_key(&queue_key(v, n, p)), Some((v.to_string(), n.to_string(), p)));
        }
        assert_ne!(queue_key("a/b", "c", 0), queue_key("a", "b/c", 0));
        assert_eq!(parse_key("storage:meta"), None);
    }
}

//! roll queues clustering: many stateless brokers acting as one RabbitMQ node.
//!
//! - **Shared state.** Topology lives in one metadata store (roll postgres via [`pg::PgMeta`])
//!   and message logs in shared, replicated storage (`mq-storage`). Every node follows the
//!   store's change feed and applies it ([`Broker::apply_change`]).
//! - **Membership.** Nodes heartbeat a `mq_node` record; a node whose heartbeat stops changing
//!   for `node_ttl` is dead. One record per node, so liveness costs nothing per queue.
//! - **Ownership.** Each queue partition has a `mq_owner` record `{node, epoch}`, valid while that
//!   node is alive. Claims and moves are compare-and-set; the new owner opens the log with a
//!   higher epoch, which fences the old one out of the storage layer.
//! - **Routing.** Every node has a [`router`] per queue that forwards to the owner (in process or
//!   over [`rpc`]). Ownership moves without client connections moving: routers retarget and
//!   re-register consumers.
//! - **Failover.** When an owner dies, the surviving node that wins rendezvous hashing for the
//!   queue claims it and replays its log; confirmed messages are on the storage quorum, so none
//!   are lost. **Rebalance** moves queues to their rendezvous node as nodes join; **drain**
//!   hands everything off before a node leaves (rolling upgrades).
//! - **AZ affinity.** Traffic between availability zones is billed, so each queue has a home
//!   AZ (in its owner record) and lives on the rendezvous node among that AZ's nodes. The home
//!   starts as the AZ where the queue was first used and follows its clients: the owner counts
//!   where each queue's publishes and deliveries come from, and when another AZ clearly
//!   dominates for a while, the home moves there and the queue follows (see
//!   [`Cluster::place_by_traffic`]). A home AZ without live nodes falls back to any node.

pub mod aws;
pub mod pg;
pub use pg::PgMeta;
mod router;
pub mod rpc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use mq_core::cluster::{Placement, QueueStart};
use mq_core::log::LogStore;
use mq_core::meta::MetaStore;
use mq_core::queue::{ConfirmTicket, ConsumerSpec, Outcome, PublishResult, QueueCmd, QueueRef};
use mq_core::transport::{Stream, Transport};
use mq_core::{AmqpError, Broker, FieldTable, Message};
use parking_lot::{Mutex, RwLock};
use router::{Ctl, RemoteSink, Router, RouterRef};
use rpc::{Frame, Peer, Push, Req};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

pub const NODE: &str = "mq_node";
pub const OWNER: &str = "mq_owner";

#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// Unique and stable per node (a hostname or instance id).
    pub node_id: String,
    /// Internal RPC listener.
    pub listen: SocketAddr,
    /// Address other nodes dial (defaults to `listen`).
    pub advertise: Option<SocketAddr>,
    pub az: Option<String>,
    pub heartbeat: Duration,
    /// A node is dead after its heartbeat hasn't changed for this long.
    pub node_ttl: Duration,
    /// How often to read the metadata change feed.
    pub follow: Duration,
    /// Move queues to their rendezvous node this often (`None`: never). Each pass also
    /// reconsiders home AZs from the traffic since the last one.
    pub rebalance: Option<Duration>,
    /// Operations (publishes and deliveries) a queue needs in one rebalance period before its
    /// traffic can move its home AZ.
    pub place_min_ops: u64,
    /// Client-facing addresses by protocol (`amqp`, `mqtt`, …), published for `mq-proxy`.
    pub listeners: std::collections::BTreeMap<String, SocketAddr>,
    /// TLS and the shared secret between nodes (see [`mq_core::transport`]).
    pub transport: Arc<Transport>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            node_id: std::env::var("HOSTNAME").unwrap_or_else(|_| format!("node-{:x}", mq_core::rand_u64())),
            listen: "127.0.0.1:0".parse().expect("addr"),
            advertise: None,
            az: None,
            heartbeat: Duration::from_millis(1000),
            node_ttl: Duration::from_millis(5000),
            follow: Duration::from_millis(100),
            rebalance: Some(Duration::from_secs(10)),
            place_min_ops: PLACE_MIN_OPS,
            listeners: Default::default(),
            transport: Transport::plain("cluster"),
        }
    }
}

/// A queue actor running on this node.
pub(crate) struct OwnedQueue {
    pub inner: QueueRef,
    /// Set during handoff: consumers stop receiving.
    pub paused: Arc<std::sync::atomic::AtomicBool>,
    #[allow(dead_code)]
    pub epoch: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Owner {
    pub node: String,
    pub epoch: u64,
    /// The queue's home AZ (see the module docs).
    pub az: Option<String>,
}

/// Default for [`ClusterConfig::place_min_ops`].
const PLACE_MIN_OPS: u64 = 1000;
/// Share of a queue's traffic another AZ needs before the queue moves there...
const PLACE_SHARE: f64 = 2.0 / 3.0;
/// ...in this many rebalance periods in a row.
const PLACE_STREAK: u32 = 2;

/// Operations on one queue, by the AZ they came from.
#[derive(Default)]
struct Traffic {
    /// Publishes and deliveries for other nodes, by AZ (shared with their sinks).
    remote: HashMap<String, Arc<AtomicU64>>,
    /// Totals at the end of the last period: all operations, and the remote ones by AZ.
    last_total: Option<u64>,
    last_remote: HashMap<String, u64>,
    /// An AZ that has dominated recent periods, and for how many.
    streak: Option<(String, u32)>,
}

#[derive(Clone, Debug)]
struct NodeState {
    addr: SocketAddr,
    listeners: std::collections::BTreeMap<String, SocketAddr>,
    az: Option<String>,
    draining: bool,
    hb: u64,
    last_seen: Instant,
}

pub(crate) enum Decision {
    Local(u64),
    Remote(String),
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

/// Cluster key of a queue partition.
pub fn queue_key(vhost: &str, name: &str, partition: u32) -> String {
    mq_core::meta::key(&[vhost, name, &partition.to_string()])
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The node a queue belongs on: highest rendezvous score.
fn rendezvous<'a>(key: &str, nodes: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    nodes.max_by_key(|n| fnv(&format!("{key}\u{1f}{n}")))
}

pub struct Cluster {
    pub node_id: String,
    pub config: ClusterConfig,
    pub meta: Arc<dyn MetaStore>,
    logs: Arc<dyn LogStore>,
    num: u16,
    advertise: SocketAddr,
    broker: OnceLock<Weak<Broker>>,
    nodes: RwLock<HashMap<String, NodeState>>,
    owners: RwLock<HashMap<String, Owner>>,
    /// Where the traffic of owned queues comes from (AZ affinity).
    traffic: Mutex<HashMap<String, Traffic>>,
    pub(crate) owned: RwLock<HashMap<String, Arc<OwnedQueue>>>,
    pub(crate) routers: RwLock<HashMap<String, Arc<RouterRef>>>,
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    connecting: tokio::sync::Mutex<()>,
    conns: Mutex<Vec<Weak<ServerConn>>>,
    next_sub: AtomicU64,
    draining: AtomicBool,
    stopped: AtomicBool,
    hb: AtomicU64,
    version: AtomicU64,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Cluster {
    /// Join the cluster: bind the RPC listener, load membership and ownership, start
    /// heartbeating. Then start the broker with [`Cluster::placement`] and call
    /// [`Cluster::attach`].
    pub async fn join(config: ClusterConfig, meta: Arc<dyn MetaStore>, logs: Arc<dyn LogStore>) -> anyhow::Result<Arc<Cluster>> {
        let listener = TcpListener::bind(config.listen).await?;
        let bound = listener.local_addr()?;
        let advertise = config.advertise.unwrap_or(bound);
        let (_, version) = meta.changes_since(i64::MAX).await?;
        let num = ((mq_core::rand_u64() % 0xfffe) + 1) as u16;
        let c = Arc::new(Cluster {
            node_id: config.node_id.clone(),
            config: config.clone(),
            meta: meta.clone(),
            logs,
            num,
            advertise,
            broker: OnceLock::new(),
            nodes: RwLock::new(HashMap::new()),
            owners: RwLock::new(HashMap::new()),
            traffic: Mutex::new(HashMap::new()),
            owned: RwLock::new(HashMap::new()),
            routers: RwLock::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            connecting: tokio::sync::Mutex::new(()),
            conns: Mutex::new(Vec::new()),
            next_sub: AtomicU64::new(1),
            draining: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            hb: AtomicU64::new(0),
            version: AtomicU64::new(version.max(0) as u64),
            tasks: Mutex::new(Vec::new()),
        });
        for (k, v) in meta.list(NODE).await? {
            if let Some(n) = node_state(&v)
                && k != c.node_id
            {
                c.nodes.write().insert(k, n);
            }
        }
        for (k, v) in meta.list(OWNER).await? {
            if let Some(o) = owner_of(&v) {
                c.owners.write().insert(k, o);
            }
        }
        c.heartbeat().await?;
        let mut tasks = vec![];
        let cc = c.clone();
        tasks.push(tokio::spawn(async move { cc.serve(listener).await }));
        let cc = c.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(cc.config.heartbeat).await;
                if cc.stopped() {
                    return;
                }
                if let Err(e) = cc.heartbeat().await {
                    warn!(error = %e, "cluster heartbeat failed");
                }
            }
        }));
        let cc = c.clone();
        tasks.push(tokio::spawn(async move { cc.monitor().await }));
        let cc = c.clone();
        tasks.push(tokio::spawn(async move { cc.stats_loop().await }));
        *c.tasks.lock() = tasks;
        info!(node = %c.node_id, rpc = %advertise, "joined cluster");
        Ok(c)
    }

    /// The placement to start the broker with.
    pub fn placement(self: &Arc<Self>) -> Arc<dyn Placement> {
        Arc::new(ClusterPlacement(self.clone()))
    }

    /// Follow the change feed into `broker` and start rebalancing.
    pub fn attach(self: &Arc<Self>, broker: &Arc<Broker>) {
        let _ = self.broker.set(Arc::downgrade(broker));
        let c = self.clone();
        let follow = tokio::spawn(async move { c.follow().await });
        self.tasks.lock().push(follow);
        if let Some(every) = self.config.rebalance {
            let c = self.clone();
            let t = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    if c.stopped() {
                        return;
                    }
                    c.place_by_traffic().await;
                    c.rebalance(64).await;
                }
            });
            self.tasks.lock().push(t);
        }
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

    /// Live nodes (including this one).
    pub fn members(&self) -> Vec<Value> {
        let mut out = vec![json!({ "node": self.node_id, "addr": self.advertise.to_string(), "az": self.config.az, "alive": true, "draining": self.draining.load(Ordering::Relaxed), "owned": self.owned_count(), "me": true })];
        for (id, n) in self.nodes.read().iter() {
            out.push(json!({ "node": id, "addr": n.addr.to_string(), "az": n.az, "alive": n.last_seen.elapsed() < self.config.node_ttl, "draining": n.draining, "me": false }));
        }
        out
    }

    async fn heartbeat(&self) -> anyhow::Result<()> {
        let hb = self.hb.fetch_add(1, Ordering::Relaxed) + 1;
        let v = json!({ "addr": self.advertise.to_string(), "az": self.config.az, "num": self.num, "draining": self.draining.load(Ordering::Relaxed), "hb": hb, "listeners": self.config.listeners });
        self.meta.put(NODE, &self.node_id, &v).await?;
        Ok(())
    }

    pub(crate) fn alive(&self, node: &str) -> bool {
        node == self.node_id || self.nodes.read().get(node).is_some_and(|n| n.last_seen.elapsed() < self.config.node_ttl)
    }

    /// Nodes that should own queues: alive and not draining.
    fn candidates(&self) -> Vec<String> {
        let ttl = self.config.node_ttl;
        let mut v: Vec<String> = self.nodes.read().iter().filter(|(_, n)| n.last_seen.elapsed() < ttl && !n.draining).map(|(k, _)| k.clone()).collect();
        if !self.draining.load(Ordering::Relaxed) {
            v.push(self.node_id.clone());
        }
        v
    }

    pub(crate) fn owner_cached(&self, key: &str) -> Option<Owner> {
        self.owners.read().get(key).cloned()
    }

    /// Claim `key` for this node. `home` is the queue's home AZ (this node's for a new queue).
    async fn claim(&self, key: &str, expected: Option<i64>, epoch: u64, home: Option<String>) -> anyhow::Result<bool> {
        let az = home.or_else(|| self.config.az.clone());
        let v = owner_json(&self.node_id, epoch, az.as_deref());
        let won = self.meta.cas(OWNER, key, expected, &v).await?.is_some();
        if won {
            self.owners.write().insert(key.into(), Owner { node: self.node_id.clone(), epoch, az });
        }
        Ok(won)
    }

    fn node_az(&self, node: &str) -> Option<String> {
        if node == self.node_id {
            return self.config.az.clone();
        }
        self.nodes.read().get(node).and_then(|n| n.az.clone())
    }

    /// The node `key` belongs on: the rendezvous winner among the live nodes of its home AZ,
    /// or among all live nodes when that AZ has none (or the queue has no home).
    fn target(&self, key: &str, candidates: &[String]) -> Option<String> {
        let home = self.owner_cached(key).and_then(|o| o.az);
        if let Some(home) = home {
            let local: Vec<&String> = candidates.iter().filter(|n| self.node_az(n).as_deref() == Some(home.as_str())).collect();
            if let Some(n) = rendezvous(key, local.into_iter()) {
                return Some(n.clone());
            }
        }
        rendezvous(key, candidates.iter()).cloned()
    }

    pub(crate) async fn owner_decision(&self, key: &str, waited: Duration) -> anyhow::Result<Decision> {
        match self.meta.get_versioned(OWNER, key).await? {
            None => {
                if self.draining.load(Ordering::Relaxed)
                    && let Some(to) = rendezvous(key, self.candidates().iter())
                    && to != &self.node_id
                {
                    // Don't take new queues while draining; the rendezvous node will.
                    return Ok(if waited > self.config.node_ttl { Decision::Local(1) } else { Decision::Wait });
                }
                Ok(if self.claim(key, None, 1, None).await? { Decision::Local(1) } else { Decision::Wait })
            }
            Some((v, ver)) => {
                let Some(o) = owner_of(&v) else { return Ok(Decision::Wait) };
                self.owners.write().insert(key.into(), o.clone());
                if o.node == self.node_id {
                    // Ours on record (adopted, or from before a restart): bump the epoch to fence
                    // any older incarnation, then run it.
                    let epoch = o.epoch + 1;
                    return Ok(if self.claim(key, Some(ver), epoch, o.az).await? { Decision::Local(epoch) } else { Decision::Wait });
                }
                if self.alive(&o.node) {
                    return Ok(Decision::Remote(o.node));
                }
                // The dead node's successor stays in the queue's home AZ when it can.
                let winner = self.target(key, &self.candidates());
                if winner.as_deref() == Some(self.node_id.as_str()) || waited > self.config.node_ttl * 2 {
                    let epoch = o.epoch + 1;
                    if self.claim(key, Some(ver), epoch, o.az.clone()).await? {
                        info!(queue = %key, from = %o.node, epoch, "took over queue from dead node");
                        return Ok(Decision::Local(epoch));
                    }
                }
                Ok(Decision::Wait)
            }
        }
    }

    /// Give up a claim we couldn't act on.
    pub(crate) async fn release(&self, key: &str) {
        if let Ok(Some((v, ver))) = self.meta.get_versioned(OWNER, key).await
            && owner_of(&v).is_some_and(|o| o.node == self.node_id)
        {
            let gone = owner_json("", v["epoch"].as_u64().unwrap_or(0), v["az"].as_str());
            let _ = self.meta.cas(OWNER, key, Some(ver), &gone).await;
        }
    }

    /// Pass ownership of a stopped queue to `to`.
    pub(crate) async fn transfer(&self, key: &str, to: &str) -> bool {
        let Ok(Some((v, ver))) = self.meta.get_versioned(OWNER, key).await else { return false };
        let Some(o) = owner_of(&v) else { return false };
        if o.node != self.node_id {
            return false;
        }
        let next = owner_json(to, o.epoch + 1, o.az.as_deref());
        if !matches!(self.meta.cas(OWNER, key, Some(ver), &next).await, Ok(Some(_))) {
            return false;
        }
        self.owners.write().insert(key.into(), Owner { node: to.into(), epoch: o.epoch + 1, az: o.az });
        if let Some(p) = self.peer(to).await {
            let _ = p.call(&Req::Adopt { q: key.into() }, vec![]).await;
        }
        true
    }

    pub(crate) async fn peer(&self, node: &str) -> Option<Arc<Peer>> {
        if let Some(p) = self.peers.lock().get(node).filter(|p| !p.is_closed()) {
            return Some(p.clone());
        }
        let _g = self.connecting.lock().await;
        if let Some(p) = self.peers.lock().get(node).filter(|p| !p.is_closed()) {
            return Some(p.clone());
        }
        let addr = self.nodes.read().get(node).map(|n| n.addr)?;
        match Peer::connect(node, addr, &self.config.transport, &self.node_id, self.num).await {
            Ok(p) => {
                self.peers.lock().insert(node.into(), p.clone());
                Some(p)
            }
            Err(e) => {
                let e = format!("{e:#}");
                // A secret mismatch is a misconfiguration, not a node going away.
                if e.contains("secret") {
                    warn!(%node, error = %e, "node refused the cluster handshake");
                } else {
                    debug!(%node, error = %e, "can't reach node");
                }
                None
            }
        }
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
                match ch.kind.as_str() {
                    NODE => {
                        if ch.key == self.node_id {
                            continue;
                        }
                        match ch.value.as_ref().and_then(node_state) {
                            Some(n) => {
                                let mut nodes = self.nodes.write();
                                let changed = nodes.get(&ch.key).is_none_or(|o| o.hb != n.hb || o.draining != n.draining);
                                if changed {
                                    nodes.insert(ch.key.clone(), n);
                                }
                            }
                            None => {
                                self.nodes.write().remove(&ch.key);
                            }
                        }
                    }
                    OWNER => {
                        if let Some(o) = ch.value.as_ref().and_then(owner_of) {
                            self.owners.write().insert(ch.key.clone(), o);
                        }
                        if let Some(r) = self.routers.read().get(&ch.key) {
                            let _ = r.ctl.send(Ctl::Retarget);
                        }
                    }
                    kind => {
                        // Reconcile each key once per batch; it reads the current value anyway.
                        if seen.insert((kind.to_string(), ch.key.clone()))
                            && let Err(e) = broker.apply_change(kind, &ch.key).await
                        {
                            warn!(kind, key = %ch.key, error = %e, "failed to apply metadata change");
                        }
                    }
                }
            }
            self.version.store(latest.max(from) as u64, Ordering::Release);
        }
    }

    /// Notice dead nodes and wake the routers of their queues.
    async fn monitor(self: Arc<Self>) {
        let mut dead: HashSet<String> = HashSet::new();
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if self.stopped() {
                return;
            }
            let now_dead: HashSet<String> = self.nodes.read().iter().filter(|(_, n)| n.last_seen.elapsed() >= self.config.node_ttl).map(|(k, _)| k.clone()).collect();
            for n in now_dead.difference(&dead) {
                warn!(node = %n, "node is gone; taking over its queues");
                self.peers.lock().remove(n);
                let keys: Vec<String> = self.owners.read().iter().filter(|(_, o)| &o.node == n).map(|(k, _)| k.clone()).collect();
                for k in keys {
                    if let Some(r) = self.routers.read().get(&k) {
                        let _ = r.ctl.send(Ctl::Retarget);
                    }
                }
            }
            dead = now_dead;
        }
    }

    /// Refresh queue counters on routers whose queue runs elsewhere.
    async fn stats_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if self.stopped() {
                return;
            }
            let mut by_node: HashMap<String, Vec<(String, Weak<mq_core::queue::QueueHandle>)>> = HashMap::new();
            for r in self.routers.read().values() {
                if let Some(n) = r.remote_node.lock().clone() {
                    by_node.entry(n).or_default().push((r.key.clone(), r.handle.clone()));
                }
            }
            for (node, qs) in by_node {
                let Some(peer) = self.peer(&node).await else { continue };
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

    /// Counter of operations on `key` for clients of `peer` (`None` without AZs).
    fn remote_ops(&self, key: &str, peer: Option<&str>) -> Option<Arc<AtomicU64>> {
        self.config.az.as_ref()?;
        let az = self.node_az(peer?)?;
        Some(self.traffic.lock().entry(key.into()).or_default().remote.entry(az).or_default().clone())
    }

    /// Reconsider owned queues' home AZs from the traffic since the last call: a queue whose
    /// operations came mostly (two thirds) from another AZ for [`PLACE_STREAK`] periods in a row
    /// gets that AZ as its home, and the next rebalance moves it there.
    pub async fn place_by_traffic(&self) {
        let Some(my_az) = self.config.az.clone() else { return };
        let owned: HashMap<String, Arc<OwnedQueue>> = self.owned.read().iter().map(|(k, o)| (k.clone(), o.clone())).collect();
        let mut moves = Vec::new();
        {
            let mut traffic = self.traffic.lock();
            traffic.retain(|k, _| owned.contains_key(k));
            for (key, q) in &owned {
                let s = &q.inner.shared;
                let total = s.published.load(Ordering::Relaxed) + s.delivered.load(Ordering::Relaxed);
                let t = traffic.entry(key.clone()).or_default();
                let remote: HashMap<String, u64> = t.remote.iter().map(|(az, n)| (az.clone(), n.load(Ordering::Relaxed))).collect();
                let last_remote = std::mem::replace(&mut t.last_remote, remote.clone());
                let Some(last) = t.last_total.replace(total) else { continue };
                // Everything that didn't come through another node came from clients here.
                let mut by_az: HashMap<String, u64> = HashMap::new();
                for (az, n) in remote {
                    *by_az.entry(az.clone()).or_default() += n.saturating_sub(last_remote.get(&az).copied().unwrap_or(0));
                }
                let from_others: u64 = by_az.values().sum();
                *by_az.entry(my_az.clone()).or_default() += total.saturating_sub(last).saturating_sub(from_others);
                let sum: u64 = by_az.values().sum();
                let home = self.owner_cached(key).and_then(|o| o.az);
                match by_az.into_iter().max_by_key(|(_, n)| *n) {
                    Some((az, n)) if sum >= self.config.place_min_ops && n as f64 >= sum as f64 * PLACE_SHARE && home.as_deref() != Some(az.as_str()) => {
                        let streak = match t.streak.take() {
                            Some((a, k)) if a == az => k + 1,
                            _ => 1,
                        };
                        if streak >= PLACE_STREAK {
                            moves.push((key.clone(), az));
                        } else {
                            t.streak = Some((az, streak));
                        }
                    }
                    _ => t.streak = None,
                }
            }
        }
        let candidates = self.candidates();
        for (key, az) in moves {
            if candidates.iter().any(|n| self.node_az(n).as_deref() == Some(az.as_str())) {
                self.set_home(&key, &az).await;
            }
        }
    }

    /// Give an owned queue a new home AZ.
    async fn set_home(&self, key: &str, az: &str) -> bool {
        let Ok(Some((v, ver))) = self.meta.get_versioned(OWNER, key).await else { return false };
        let Some(o) = owner_of(&v) else { return false };
        if o.node != self.node_id {
            return false;
        }
        if !matches!(self.meta.cas(OWNER, key, Some(ver), &owner_json(&o.node, o.epoch, Some(az))).await, Ok(Some(_))) {
            return false;
        }
        info!(queue = %key, from = o.az.as_deref().unwrap_or("-"), to = %az, "queue follows its clients to another AZ");
        self.owners.write().insert(key.into(), Owner { az: Some(az.into()), ..o });
        true
    }

    /// The home AZ of a queue partition (`None` without AZs, or for queues nobody owns yet).
    pub fn home_az(&self, key: &str) -> Option<String> {
        self.owner_cached(key).and_then(|o| o.az)
    }

    /// Move up to `max` owned queues to the node rendezvous hashing assigns them.
    pub async fn rebalance(&self, max: usize) -> usize {
        let nodes = self.candidates();
        if nodes.is_empty() {
            return 0;
        }
        let mut moves = Vec::new();
        for key in self.owned_keys() {
            if let Some(to) = self.target(&key, &nodes)
                && to != self.node_id
            {
                moves.push((key, to));
                if moves.len() >= max {
                    break;
                }
            }
        }
        let mut waits = Vec::new();
        for (key, to) in moves {
            if let Some(r) = self.routers.read().get(&key) {
                let (tx, rx) = oneshot::channel();
                if r.ctl.send(Ctl::Handoff { to, reply: tx }).is_ok() {
                    waits.push(rx);
                }
            }
        }
        let mut n = 0;
        for w in waits {
            if w.await.unwrap_or(false) {
                n += 1;
            }
        }
        if n > 0 {
            info!(moved = n, "rebalanced queues");
        }
        n
    }

    /// Stop taking queues and hand every owned queue to other nodes (rolling upgrade). Returns
    /// how many queues are still here after `timeout`.
    pub async fn drain(&self, timeout: Duration) -> usize {
        self.draining.store(true, Ordering::Release);
        let _ = self.heartbeat().await;
        let deadline = Instant::now() + timeout;
        while self.owned_count() > 0 && Instant::now() < deadline {
            if self.candidates().is_empty() {
                warn!("no other node to drain to");
                break;
            }
            if self.rebalance(256).await == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        self.owned_count()
    }

    /// Leave the cluster: stop background work and remove our membership record so others take
    /// over at once.
    pub async fn leave(&self) {
        self.stopped.store(true, Ordering::Release);
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
        let _ = self.meta.delete(NODE, &self.node_id).await;
    }

    // ── RPC server ──

    async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "rpc accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let c = self.clone();
            tokio::spawn(async move {
                match c.config.transport.accept(stream).await {
                    Ok(s) => c.serve_conn(s).await,
                    Err(e) => warn!(%peer, error = format!("{e:#}"), "rpc connection refused"),
                }
            });
        }
    }

    async fn serve_conn(self: Arc<Self>, stream: Stream) {
        let (r, mut w) = stream.into_split();
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
        let conn = Arc::new(ServerConn { tx, peer: Mutex::new(None), subs: Mutex::new(HashMap::new()), channels: Mutex::new(HashSet::new()) });
        self.conns.lock().push(Arc::downgrade(&conn));
        let mut r = BufReader::with_capacity(256 << 10, r);
        while let Ok(Some(f)) = Frame::read(&mut r).await {
            if f.kind != rpc::REQ {
                continue;
            }
            let Ok(req) = serde_json::from_value::<Req>(f.head) else {
                conn.reply(rpc::err_frame(f.id, &AmqpError::command_invalid("bad request")));
                continue;
            };
            self.handle(&conn, f.id, req, f.blobs).await;
        }
        // The other node is gone: requeue what its channels held.
        let chans: Vec<(String, u64)> = conn.channels.lock().drain().collect();
        for (key, ch) in chans {
            let o = self.owned.read().get(&key).cloned();
            if let Some(o) = o {
                o.inner.channel_closed(ch).await;
            }
        }
        self.conns.lock().retain(|w| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(&conn)));
    }

    async fn handle(&self, conn: &Arc<ServerConn>, id: u64, req: Req, blobs: Vec<Bytes>) {
        let key = match &req {
            Req::Hello { node, .. } => {
                *conn.peer.lock() = Some(node.clone());
                conn.reply(rpc::ok_frame(id, Value::Null, vec![]));
                return;
            }
            Req::Credit { sub, n } => {
                if let Some(s) = conn.subs.lock().get(sub) {
                    s.sink.credit.fetch_add(*n, Ordering::AcqRel);
                    s.queue.wake();
                }
                return;
            }
            Req::Stats { qs } => {
                let owned = self.owned.read();
                let v: Vec<Value> = qs.iter().map(|k| owned.get(k).map(|o| json!(router::stats_of(&o.inner.shared))).unwrap_or(Value::Null)).collect();
                conn.reply(rpc::ok_frame(id, Value::Array(v), vec![]));
                return;
            }
            Req::Adopt { q } => {
                if let Some(r) = self.routers.read().get(q) {
                    let _ = r.ctl.send(Ctl::Retarget);
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                while !self.owned.read().contains_key(q) && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                conn.reply(rpc::ok_frame(id, json!(self.owned.read().contains_key(q)), vec![]));
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
            | Req::Consumers { q } => q.clone(),
        };
        let Some(owned) = self.owned.read().get(&key).cloned() else {
            conn.reply(rpc::moved_frame(id));
            return;
        };
        let q = owned.inner.clone();
        let reply = conn.clone();
        match req {
            Req::Publish { wait, confirm, .. } => {
                let Some(mut b) = blobs.into_iter().next() else { return };
                if let Some(n) = self.remote_ops(&key, conn.peer.lock().as_deref()) {
                    n.fetch_add(1, Ordering::Relaxed);
                }
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
                let ops = self.remote_ops(&key, conn.peer.lock().as_deref());
                let sink = Arc::new(RemoteSink { sub, tx: conn.tx.clone(), credit: AtomicU32::new(credit), paused: owned.paused.clone(), ops });
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
            Req::Hello { .. } | Req::Credit { .. } | Req::Stats { .. } | Req::Adopt { .. } => {}
        }
    }
}

fn encode_msg(m: &Message) -> Bytes {
    let mut b = BytesMut::with_capacity(m.encoded_len());
    m.encode(&mut b);
    b.freeze()
}

fn node_state(v: &Value) -> Option<NodeState> {
    Some(NodeState {
        addr: v["addr"].as_str()?.parse().ok()?,
        listeners: v["listeners"].as_object().map(|m| m.iter().filter_map(|(k, a)| Some((k.clone(), a.as_str()?.parse().ok()?))).collect()).unwrap_or_default(),
        az: v["az"].as_str().map(String::from),
        draining: v["draining"].as_bool().unwrap_or(false),
        hb: v["hb"].as_u64().unwrap_or(0),
        last_seen: Instant::now(),
    })
}

fn owner_of(v: &Value) -> Option<Owner> {
    let node = v["node"].as_str()?;
    if node.is_empty() {
        return None;
    }
    Some(Owner { node: node.into(), epoch: v["epoch"].as_u64().unwrap_or(0), az: v["az"].as_str().filter(|a| !a.is_empty()).map(String::from) })
}

fn owner_json(node: &str, epoch: u64, az: Option<&str>) -> Value {
    match az {
        Some(az) => json!({ "node": node, "epoch": epoch, "az": az }),
        None => json!({ "node": node, "epoch": epoch }),
    }
}

struct ServerSub {
    key: String,
    channel: u64,
    queue: QueueRef,
    sink: Arc<RemoteSink>,
}

/// A connection from another node.
struct ServerConn {
    tx: mpsc::Sender<Frame>,
    /// The node on the other end, from its hello.
    peer: Mutex<Option<String>>,
    subs: Mutex<HashMap<u64, ServerSub>>,
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
            let _ = old.ctl.send(Ctl::Forget);
        }
        let (handle, rref) = Router::spawn(c, key.clone(), start).await?;
        c.routers.write().insert(key, rref);
        Ok(handle)
    }

    fn forget(&self, vhost: &str, name: &str) {
        let prefix = format!("{}\u{1f}", mq_core::meta::key(&[vhost, name]));
        let gone: Vec<Arc<RouterRef>> = {
            let mut r = self.0.routers.write();
            let keys: Vec<String> = r.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            keys.iter().filter_map(|k| r.remove(k)).collect()
        };
        for r in gone {
            let _ = r.ctl.send(Ctl::Forget);
        }
    }

    fn locate(&self, vhost: &str, name: &str) -> Option<mq_core::cluster::Located> {
        let c = &self.0;
        let key = queue_key(vhost, name, 0);
        if !c.routers.read().contains_key(&key) {
            return None;
        }
        if let Some(o) = c.owned.read().get(&key) {
            return Some(mq_core::cluster::Located::Local(o.inner.clone()));
        }
        let owner = c.owner_cached(&key)?;
        if owner.node == c.node_id {
            return None;
        }
        let listeners = c.nodes.read().get(&owner.node).map(|n| n.listeners.clone()).unwrap_or_default();
        Some(mq_core::cluster::Located::Remote { node: owner.node, listeners })
    }
}

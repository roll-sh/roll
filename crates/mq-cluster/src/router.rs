//! A router is what channels on every node hold for a clustered queue. It forwards commands to
//! the queue's actor, wherever it runs: in this process (the node holds the queue's lease) or on
//! another node over RPC. When ownership moves (rebalance, drain, failover) the router retargets
//! and re-registers its consumers, so client connections never notice.

use crate::rpc::{Peer, Push, PushHandler, Reply, Req};
use crate::{Cluster, Decision, OwnedQueue, outcome_code};
use bytes::{Bytes, BytesMut};
use mq_core::cluster::QueueStart;
use mq_core::queue::engine::QueueHost;
use mq_core::queue::*;
use mq_core::{AmqpError, FieldTable, Message};
use parking_lot::Mutex;
use roll_cluster::Guard;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub(crate) enum Ctl {
    /// Ownership may have changed: find the owner again.
    Retarget,
    /// The lease of the tenure `epoch` is moving (a handoff, a drain) or was lost: stop the
    /// actor, then let go of it.
    Release { epoch: i64 },
    /// The router is no longer needed: the queue was deleted cluster-wide (`deleted`, so its
    /// lease goes too), or a new router replaced it.
    Forget { deleted: bool },
    /// The owner's connection dropped or the queue moved: re-subscribe consumers.
    Resubscribe,
}

#[derive(Clone)]
pub(crate) enum Target {
    None,
    Local(Arc<OwnedQueue>),
    Remote(Arc<Peer>),
}

/// The router's own view, shared with the cluster (stats, retargeting).
pub(crate) struct RouterRef {
    pub key: String,
    pub handle: Weak<QueueHandle>,
    pub ctl: mpsc::UnboundedSender<Ctl>,
    /// The owner's node, while the queue runs elsewhere.
    pub remote: Mutex<Option<Arc<Peer>>>,
}

/// This node's tenure as the queue's owner: the lease's guard (a handoff waits for it), and the
/// task that tells the router when the lease starts moving.
struct Tenure {
    guard: Guard,
    watcher: tokio::task::JoinHandle<()>,
}

impl Drop for Tenure {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

/// One consumer registered through this router.
struct Reg {
    spec: ConsumerSpec,
    me: Weak<QueueHandle>,
    /// Remote subscriptions: deliveries waiting for channel credit, and where to grant more.
    buf: Mutex<VecDeque<Delivery>>,
    sub: Mutex<Option<(u64, Weak<Peer>)>>,
    ctl: mpsc::UnboundedSender<Ctl>,
}

const REMOTE_CREDIT: u32 = 64;

impl Reg {
    /// Hand buffered deliveries to the channel while it has credit; grant the owner as many.
    fn drain(&self) {
        let mut n = 0;
        loop {
            if !self.spec.sink.has_credit() {
                break;
            }
            let Some(d) = self.buf.lock().pop_front() else { break };
            self.spec.sink.deliver(d);
            n += 1;
        }
        if n > 0
            && let Some((sub, peer)) = self.sub.lock().clone()
            && let Some(p) = peer.upgrade()
        {
            p.send(&Req::Credit { sub, n });
        }
    }
}

impl PushHandler for Reg {
    fn push(&self, ev: Push, blobs: Vec<Bytes>) {
        match ev {
            Push::Deliver { seq, redelivered, delivery_count, remaining, .. } => {
                let Some(me) = self.me.upgrade() else { return };
                let Some(mut b) = blobs.into_iter().next() else { return };
                let Ok(msg) = Message::decode(&mut b) else { return };
                let d = Delivery { queue: me, consumer_tag: self.spec.tag.clone(), seq, msg: Arc::new(msg), redelivered, delivery_count, remaining };
                self.buf.lock().push_back(d);
                self.drain();
            }
            Push::Cancelled { .. } => self.spec.sink.cancelled(&self.spec.tag),
            Push::Activated { active, .. } => self.spec.sink.activated(&self.spec.tag, active),
            Push::TimedOut { timeout_ms, .. } => self.spec.sink.timed_out(&self.spec.tag, timeout_ms),
            Push::Moved { .. } => {
                let _ = self.ctl.send(Ctl::Resubscribe);
            }
        }
    }

    fn disconnected(&self) {
        let _ = self.ctl.send(Ctl::Resubscribe);
    }
}

/// Sink for consumers of a locally owned queue: names the router in deliveries and stops
/// delivering while the queue is being handed off.
pub(crate) struct LocalSink {
    pub inner: Arc<dyn ConsumerSink>,
    pub me: Weak<QueueHandle>,
    pub paused: Arc<AtomicBool>,
}

impl ConsumerSink for LocalSink {
    fn has_credit(&self) -> bool {
        !self.paused.load(Ordering::Acquire) && self.inner.has_credit()
    }
    fn deliver(&self, mut d: Delivery) {
        if let Some(me) = self.me.upgrade() {
            d.queue = me;
        }
        self.inner.deliver(d);
    }
    fn cancelled(&self, tag: &str) {
        // Paused means the queue is moving: the router re-registers the consumer elsewhere.
        if !self.paused.load(Ordering::Acquire) {
            self.inner.cancelled(tag);
        }
    }
    fn activated(&self, tag: &str, active: bool) {
        self.inner.activated(tag, active);
    }
    fn timed_out(&self, tag: &str, ms: u64) {
        self.inner.timed_out(tag, ms);
    }
}

/// Host for the local actor: marks the router deleted before the vhost forgets it.
struct RouterHost {
    inner: Arc<dyn QueueHost>,
    shared: Arc<QueueShared>,
}

impl QueueHost for RouterHost {
    fn republish(&self, exchange: String, msg: Message, from_queue: String) {
        self.inner.republish(exchange, msg, from_queue);
    }
    fn queue_removed(&self, name: &str) {
        self.shared.deleted.store(true, Ordering::Release);
        self.inner.queue_removed(name);
    }
}

pub(crate) struct Router {
    pub key: String,
    start: QueueStart,
    host: Arc<dyn QueueHost>,
    cluster: Weak<Cluster>,
    me: Weak<QueueHandle>,
    shared: Arc<QueueShared>,
    target: Target,
    tenure: Option<Tenure>,
    regs: HashMap<(ChannelId, Arc<str>), Arc<Reg>>,
    ctl_tx: mpsc::UnboundedSender<Ctl>,
    rref: Arc<RouterRef>,
}

fn encode_msg(m: &Message) -> Bytes {
    let mut b = BytesMut::with_capacity(m.encoded_len());
    m.encode(&mut b);
    b.freeze()
}

fn table_bytes(t: &FieldTable) -> Bytes {
    let mut b = BytesMut::new();
    t.encode(&mut b);
    b.freeze()
}

impl Router {
    /// Create the router and find (or claim) the owner before returning.
    pub(crate) async fn spawn(cluster: &Arc<Cluster>, key: String, start: QueueStart) -> mq_core::Result<QueueRef> {
        let (tx, rx) = mpsc::channel(4096);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(QueueShared::default());
        let handle = Arc::new(QueueHandle::new(start.meta.clone(), start.kind, shared.clone(), tx));
        let rref = Arc::new(RouterRef { key: key.clone(), handle: Arc::downgrade(&handle), ctl: ctl_tx.clone(), remote: Mutex::new(None) });
        let host = start.host.clone();
        let mut r = Router {
            key,
            start,
            host,
            cluster: Arc::downgrade(cluster),
            me: Arc::downgrade(&handle),
            shared,
            target: Target::None,
            tenure: None,
            regs: HashMap::new(),
            ctl_tx,
            rref: rref.clone(),
        };
        // Registered before it routes: taking the lease announces it (`Acquired`), and the
        // cluster must find the router starting rather than a queue it doesn't know.
        cluster.routers.write().insert(r.key.clone(), rref.clone());
        if let Err(e) = r.resolve(true).await {
            let mut routers = cluster.routers.write();
            if routers.get(&r.key).is_some_and(|x| Arc::ptr_eq(x, &rref)) {
                routers.remove(&r.key);
            }
            return Err(e);
        }
        tokio::spawn(r.run(rx, ctl_rx));
        Ok(handle)
    }

    fn cluster(&self) -> Option<Arc<Cluster>> {
        self.cluster.upgrade()
    }

    /// Find the owner, taking the queue's lease if placement gives it to this node. With
    /// `first`, errors from starting the actor are returned (declare fails like it would on one
    /// node).
    async fn resolve(&mut self, first: bool) -> mq_core::Result<()> {
        let Some(c) = self.cluster() else { return Err(AmqpError::internal("cluster stopped")) };
        loop {
            match c.decide(&self.key).await {
                Decision::Local(guard) => match self.start_local(&c, guard).await {
                    Ok(()) => {
                        self.resubscribe().await;
                        return Ok(());
                    }
                    // The lease stays held (the guard is gone): routing finds it here again.
                    Err(e) if first => return Err(e),
                    Err(e) => warn!(queue = %self.key, error = %e, "failed to start queue; retrying"),
                },
                Decision::Remote(peer) => {
                    *self.rref.remote.lock() = Some(peer.clone());
                    self.target = Target::Remote(peer);
                    self.resubscribe().await;
                    return Ok(());
                }
                Decision::Wait => {}
            }
            if c.stopped() {
                return Err(AmqpError::internal("cluster stopped"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Run the actor here under `guard`, opening the log with the lease's epoch: that fences
    /// every earlier owner out of the log store.
    async fn start_local(&mut self, c: &Arc<Cluster>, guard: Guard) -> mq_core::Result<()> {
        let epoch = guard.epoch();
        let mut s = self.start.clone();
        s.host = Arc::new(RouterHost { inner: self.host.clone(), shared: self.shared.clone() });
        let inner = s.spawn_local(c.broker_logs().as_ref(), epoch.max(0) as u64).await?;
        if !guard.is_valid() {
            inner.stop().await;
            return Err(AmqpError::internal(format!("queue '{}' moved while it started", self.start.meta.name)));
        }
        let owned = Arc::new(OwnedQueue { inner, paused: Arc::new(AtomicBool::new(false)) });
        c.owned.write().insert(self.key.clone(), owned.clone());
        *self.rref.remote.lock() = None;
        let (ending, ctl) = (guard.ending(), self.ctl_tx.clone());
        let watcher = tokio::spawn(async move {
            ending.await;
            let _ = ctl.send(Ctl::Release { epoch });
        });
        self.tenure = Some(Tenure { guard, watcher });
        self.target = Target::Local(owned);
        debug!(queue = %self.key, epoch, "queue owned here");
        Ok(())
    }

    /// Register every consumer with the current target (after a move or failover).
    async fn resubscribe(&mut self) {
        let regs: Vec<Arc<Reg>> = self.regs.values().cloned().collect();
        let ns = self.cluster().map(|c| c.ns(0)).unwrap_or(0);
        // Cancel at the old owners first and wait for their answers (together, bounded).
        // Deliveries sent before a cancel arrive first on the same connection and land in the
        // buffer, to be released below; unsubscribing first would drop them, leaving them
        // unacked at the old owner for good.
        let channel_of = |reg: &Reg| ns | (reg.spec.channel & ((1 << 48) - 1));
        let cancels: Vec<_> = regs
            .iter()
            .filter_map(|reg| {
                let (sub, peer) = reg.sub.lock().clone()?;
                let p = peer.upgrade().filter(|p| !p.is_closed())?;
                let req = Req::Cancel { q: self.key.clone(), sub, channel: channel_of(reg), tag: reg.spec.tag.to_string() };
                Some(async move { p.call(&req, vec![]).await })
            })
            .collect();
        if !cancels.is_empty() {
            let _ = tokio::time::timeout(Duration::from_secs(2), futures_util::future::join_all(cancels)).await;
        }
        for reg in regs {
            let channel = channel_of(&reg);
            let held: Vec<u64> = reg.buf.lock().drain(..).map(|d| d.seq).collect();
            if let Some((sub, peer)) = reg.sub.lock().take()
                && let Some(p) = peer.upgrade()
            {
                p.unsubscribe(sub);
                // Deliveries the channel never saw go back to the old owner (if it still has
                // the queue; otherwise its log replay already made them ready again).
                if !held.is_empty() {
                    p.send(&Req::Release { q: self.key.clone(), channel, seqs: held });
                }
            }
            if let Err(e) = self.register(&reg).await {
                warn!(queue = %self.key, tag = %reg.spec.tag, error = %e, "consumer lost on failover");
                reg.spec.sink.cancelled(&reg.spec.tag);
                self.regs.remove(&(reg.spec.channel, reg.spec.tag.clone()));
            }
        }
    }

    async fn register(&self, reg: &Arc<Reg>) -> mq_core::Result<()> {
        let s = &reg.spec;
        match &self.target {
            Target::Local(o) => {
                let mut spec = s.clone();
                spec.sink = Arc::new(LocalSink { inner: s.sink.clone(), me: self.me.clone(), paused: o.paused.clone() });
                o.inner.consume(spec).await?;
                o.inner.wake();
                Ok(())
            }
            Target::Remote(peer) => {
                let Some(c) = self.cluster() else { return Err(AmqpError::internal("cluster stopped")) };
                let sub = c.next_sub();
                *reg.sub.lock() = Some((sub, Arc::downgrade(peer)));
                peer.subscribe(sub, reg.clone());
                let req = Req::Consume {
                    q: self.key.clone(),
                    sub,
                    channel: c.ns(s.channel),
                    conn: c.ns(s.conn),
                    tag: s.tag.to_string(),
                    no_ack: s.no_ack,
                    exclusive: s.exclusive,
                    priority: s.priority,
                    credit: REMOTE_CREDIT,
                };
                match peer.call(&req, vec![table_bytes(&s.args)]).await {
                    Reply::Ok(..) => Ok(()),
                    Reply::Err(e) => {
                        peer.unsubscribe(sub);
                        Err(e)
                    }
                    Reply::Moved => {
                        peer.unsubscribe(sub);
                        let _ = self.ctl_tx.send(Ctl::Resubscribe);
                        Ok(())
                    }
                }
            }
            Target::None => Ok(()),
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<QueueCmd>, mut ctl: mpsc::UnboundedReceiver<Ctl>) {
        loop {
            tokio::select! {
                biased;
                c = ctl.recv() => match c {
                    Some(Ctl::Forget { deleted }) => {
                        self.shutdown(deleted).await;
                        return;
                    }
                    None => break,
                    Some(Ctl::Retarget) => self.retarget().await,
                    Some(Ctl::Resubscribe) => {
                        // The owner we talked to is gone or no longer has the queue: ask the
                        // store again (the move may still be committing).
                        if matches!(self.target, Target::Remote(_)) {
                            self.target = Target::None;
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            let _ = self.resolve(false).await;
                        }
                    }
                    Some(Ctl::Release { epoch }) => {
                        if self.tenure.as_ref().is_some_and(|t| t.guard.epoch() == epoch) {
                            self.release().await;
                        }
                    }
                },
                cmd = rx.recv() => match cmd {
                    Some(cmd) => {
                        if self.handle(cmd).await {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        self.shutdown(false).await;
    }

    async fn shutdown(&mut self, deleted: bool) {
        for reg in self.regs.values() {
            if let Some((sub, peer)) = reg.sub.lock().take()
                && let Some(p) = peer.upgrade()
            {
                p.unsubscribe(sub);
            }
            reg.spec.sink.cancelled(&reg.spec.tag);
        }
        self.regs.clear();
        if let Some(c) = self.cluster() {
            let mine = c.routers.read().get(&self.key).is_some_and(|r| Arc::ptr_eq(r, &self.rref));
            if mine {
                c.routers.write().remove(&self.key);
            }
            if let Target::Local(o) = &self.target {
                c.owned.write().remove(&self.key);
                o.inner.stop().await;
            }
            // Only a deleted queue gives its lease up for good: the log of a queue that still
            // exists refuses lease epochs older than its last owner's, and a deleted lease starts
            // over at 1. (A new queue of the same name has a new log.)
            let deleted = deleted || self.shared.deleted.load(Ordering::Acquire);
            if self.tenure.take().is_some() && deleted {
                c.nodes.retire(&self.key);
            }
        }
        self.target = Target::None;
        self.shared.deleted.store(true, Ordering::Release);
    }

    /// Route again; switch target if the owner changed.
    async fn retarget(&mut self) {
        let Some(c) = self.cluster() else { return };
        match &self.target {
            // Still ours (a handoff ends the guard and comes as `Release`).
            Target::Local(_) if self.tenure.as_ref().is_some_and(|t| t.guard.is_valid()) => return,
            Target::Local(o) => {
                // Lost without a handoff (fenced: the backend was out of reach for too long):
                // stop serving at once. The log store fences whatever this actor still writes.
                warn!(queue = %self.key, "lost the queue's lease; stopping its actor");
                o.paused.store(true, Ordering::Release);
                c.owned.write().remove(&self.key);
                o.inner.stop().await;
                self.tenure = None;
            }
            Target::Remote(p) if !p.is_closed() => match c.decide(&self.key).await {
                Decision::Remote(q) if q.node == p.node => return,
                Decision::Local(g) => {
                    if self.start_local(&c, g).await.is_ok() {
                        self.resubscribe().await;
                        return;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        *self.rref.remote.lock() = None;
        self.target = Target::None;
        let _ = self.resolve(false).await;
    }

    /// The lease is moving: pause deliveries, let in-flight acks land, stop the actor (its log
    /// flushed and closed), then let go of the lease, so the next owner starts from everything
    /// this one confirmed. Consumers on other nodes are told to subscribe again.
    async fn release(&mut self) {
        let Some(c) = self.cluster() else { return };
        let Target::Local(o) = self.target.clone() else {
            self.tenure = None;
            return;
        };
        o.paused.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while o.inner.shared.unacked.load(Ordering::Relaxed) > 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        c.owned.write().remove(&self.key);
        c.notify_moved(&self.key);
        o.inner.stop().await;
        if self.start.durable {
            let _ = tokio::time::timeout(Duration::from_secs(10), c.broker_logs().released(&self.start.log_key)).await;
        }
        self.tenure = None;
        info!(queue = %self.key, "queue handed off");
        self.target = Target::None;
        let _ = self.resolve(false).await;
    }

    /// Ask the owner, retrying through the router mailbox if the queue moved.
    fn remote(&self, peer: &Arc<Peer>, req: Req, blobs: Vec<Bytes>, done: impl FnOnce(Reply) -> Option<QueueCmd> + Send + 'static) -> impl std::future::Future<Output = ()> + Send + 'static {
        let peer = peer.clone();
        let me = self.me.clone();
        let ctl = self.ctl_tx.clone();
        async move {
            let rx = peer.start(&req, blobs).await;
            tokio::spawn(async move {
                let reply = Peer::finish(rx).await;
                let moved = matches!(reply, Reply::Moved);
                if let Some(retry) = done(reply)
                    && moved
                    && let Some(me) = me.upgrade()
                {
                    let _ = ctl.send(Ctl::Resubscribe);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _ = me.send(retry).await;
                }
            });
        }
    }

    /// Returns true when the router should stop.
    async fn handle(&mut self, cmd: QueueCmd) -> bool {
        if matches!(self.target, Target::None) {
            if let QueueCmd::Wake = cmd {
                if let Some(me) = self.me.upgrade() {
                    me.woke();
                }
                return false;
            }
            let _ = self.resolve(false).await;
        }
        let Some(c) = self.cluster() else { return true };
        let target = self.target.clone();
        match (target, cmd) {
            (_, QueueCmd::Wake) => {
                if let Some(me) = self.me.upgrade() {
                    me.woke();
                }
                match &self.target {
                    Target::Local(o) => o.inner.wake(),
                    _ => self.regs.values().for_each(|r| r.drain()),
                }
            }
            (_, QueueCmd::Consume { spec, reply }) => {
                let reg = Arc::new(Reg { spec: (*spec).clone(), me: self.me.clone(), buf: Mutex::default(), sub: Mutex::default(), ctl: self.ctl_tx.clone() });
                let r = self.register(&reg).await;
                if r.is_ok() {
                    self.regs.insert((spec.channel, spec.tag.clone()), reg);
                }
                let _ = reply.send(r);
            }
            (target, QueueCmd::Cancel { channel, tag, reply }) => {
                let reg = self.regs.remove(&(channel, tag.clone()));
                match target {
                    Target::Local(o) => {
                        let r = o.inner.cancel(channel, tag).await;
                        if let Some(reply) = reply {
                            let _ = reply.send(r || reg.is_some());
                        }
                    }
                    Target::Remote(peer) => {
                        let sub = reg.as_ref().and_then(|r| r.sub.lock().take()).map(|s| s.0).unwrap_or(0);
                        peer.unsubscribe(sub);
                        let req = Req::Cancel { q: self.key.clone(), sub, channel: c.ns(channel), tag: tag.to_string() };
                        let fut = self.remote(&peer, req, vec![], move |rep| {
                            if let Some(reply) = reply {
                                let _ = reply.send(matches!(rep, Reply::Ok(..)));
                            }
                            None
                        });
                        fut.await;
                    }
                    Target::None => {}
                }
            }
            (Target::Local(o), QueueCmd::Get { channel, no_ack, reply }) => {
                let me = self.me.clone();
                let inner = o.inner.clone();
                tokio::spawn(async move {
                    let r = inner.get(channel, no_ack).await.map(|d| {
                        d.map(|mut d| {
                            if let Some(me) = me.upgrade() {
                                d.queue = me;
                            }
                            d
                        })
                    });
                    let _ = reply.send(r);
                });
            }
            (Target::Local(o), QueueCmd::Delete { if_unused, if_empty, reply }) => {
                let _ = o.inner.send(QueueCmd::Delete { if_unused, if_empty, reply }).await;
            }
            (Target::Local(o), QueueCmd::Stop { reply }) => {
                c.owned.write().remove(&self.key);
                o.inner.stop().await;
                self.tenure = None;
                let _ = reply.send(());
                return true;
            }
            (_, QueueCmd::Stop { reply }) => {
                let _ = reply.send(());
                return true;
            }
            (_, QueueCmd::Reconfigure { config }) => {
                self.start.config = (*config).clone();
                if let Target::Local(o) = &self.target {
                    let _ = o.inner.send(QueueCmd::Reconfigure { config }).await;
                }
            }
            (Target::Local(o), QueueCmd::ChannelClosed { channel }) => {
                self.regs.retain(|(ch, _), _| *ch != channel);
                o.inner.channel_closed(channel).await;
            }
            (Target::Local(o), cmd) => {
                let settle = matches!(cmd, QueueCmd::Settle { .. } | QueueCmd::Release { .. } | QueueCmd::Modify { .. });
                let _ = o.inner.send(cmd).await;
                if settle {
                    o.inner.wake();
                }
            }
            (Target::Remote(peer), cmd) => self.forward(&c, &peer, cmd).await,
            (Target::None, cmd) => drop_cmd(cmd, &self.start.meta),
        }
        false
    }

    async fn forward(&mut self, c: &Arc<Cluster>, peer: &Arc<Peer>, cmd: QueueCmd) {
        let q = self.key.clone();
        match cmd {
            QueueCmd::Publish { msg, confirm, reply } => {
                let req = Req::Publish { q, wait: reply.is_some(), confirm: confirm.is_some() };
                let blob = encode_msg(&msg);
                let fut = self.remote(peer, req, vec![blob], move |rep| match rep {
                    Reply::Ok(v, _) => {
                        if let Some(t) = confirm {
                            t.settle(v["confirmed"].as_bool().unwrap_or(true));
                        }
                        if let Some(r) = reply {
                            let res = match v["result"].as_str() {
                                Some("rejected") => PublishResult::Rejected,
                                Some("duplicate") => PublishResult::Duplicate,
                                _ => PublishResult::Accepted,
                            };
                            let _ = r.send(res);
                        }
                        None
                    }
                    Reply::Moved => Some(QueueCmd::Publish { msg, confirm, reply }),
                    Reply::Err(_) => None,
                });
                fut.await;
            }
            QueueCmd::Get { channel, no_ack, reply } => {
                let me = self.me.clone();
                let req = Req::Get { q, channel: c.ns(channel), no_ack };
                let fut = self.remote(peer, req, vec![], move |rep| match rep {
                    Reply::Ok(v, blobs) => {
                        let d = match (v.is_object(), blobs.into_iter().next(), me.upgrade()) {
                            (true, Some(mut b), Some(me)) => Message::decode(&mut b).ok().map(|m| Delivery {
                                queue: me,
                                consumer_tag: Arc::from(""),
                                seq: v["seq"].as_u64().unwrap_or(0),
                                msg: Arc::new(m),
                                redelivered: v["redelivered"].as_bool().unwrap_or(false),
                                delivery_count: v["delivery_count"].as_u64().unwrap_or(0) as u32,
                                remaining: v["remaining"].as_u64().unwrap_or(0) as u32,
                            }),
                            _ => None,
                        };
                        let _ = reply.send(Ok(d));
                        None
                    }
                    Reply::Err(e) => {
                        let _ = reply.send(Err(e));
                        None
                    }
                    Reply::Moved => Some(QueueCmd::Get { channel, no_ack, reply }),
                });
                fut.await;
            }
            QueueCmd::Settle { channel, seqs, outcome } => {
                let req = Req::Settle { q, channel: c.ns(channel), seqs, outcome: outcome_code(outcome) };
                self.remote(peer, req, vec![], |_| None).await;
                self.regs.values().for_each(|r| r.drain());
            }
            QueueCmd::Release { channel, seqs } => {
                let req = Req::Release { q, channel: c.ns(channel), seqs };
                self.remote(peer, req, vec![], |_| None).await;
                self.regs.values().for_each(|r| r.drain());
            }
            QueueCmd::Modify { channel, seqs, modification: m } => {
                let req = Req::Modify { q, channel: c.ns(channel), seqs, failed: m.delivery_failed, undeliverable: m.undeliverable_here };
                self.remote(peer, req, vec![table_bytes(&m.annotations)], |_| None).await;
                self.regs.values().for_each(|r| r.drain());
            }
            QueueCmd::ChannelClosed { channel } => {
                for ((ch, _), reg) in self.regs.iter() {
                    if *ch == channel
                        && let Some((sub, _)) = reg.sub.lock().take()
                    {
                        peer.unsubscribe(sub);
                    }
                }
                self.regs.retain(|(ch, _), _| *ch != channel);
                self.remote(peer, Req::ChannelClosed { q, channel: c.ns(channel) }, vec![], |_| None).await;
            }
            QueueCmd::Recover { channel } => self.remote(peer, Req::Recover { q, channel: c.ns(channel) }, vec![], |_| None).await,
            QueueCmd::Touch => self.remote(peer, Req::Touch { q }, vec![], |_| None).await,
            QueueCmd::Purge { reply } => {
                let fut = self.remote(peer, Req::Purge { q }, vec![], move |rep| match rep {
                    Reply::Ok(v, _) => {
                        let _ = reply.send(v.as_u64().unwrap_or(0));
                        None
                    }
                    Reply::Moved => Some(QueueCmd::Purge { reply }),
                    Reply::Err(_) => {
                        let _ = reply.send(0);
                        None
                    }
                });
                fut.await;
            }
            QueueCmd::Delete { if_unused, if_empty, reply } => {
                let shared = self.shared.clone();
                let host = self.host.clone();
                let name = self.start.meta.name.clone();
                let fut = self.remote(peer, Req::Delete { q, if_unused, if_empty }, vec![], move |rep| match rep {
                    Reply::Ok(v, _) => {
                        shared.deleted.store(true, Ordering::Release);
                        host.queue_removed(&name);
                        let _ = reply.send(Ok(v.as_u64().unwrap_or(0)));
                        None
                    }
                    Reply::Err(e) => {
                        let _ = reply.send(Err(e));
                        None
                    }
                    Reply::Moved => Some(QueueCmd::Delete { if_unused, if_empty, reply }),
                });
                fut.await;
            }
            QueueCmd::Lease { max, visibility_ms, reply } => {
                let fut = self.remote(peer, Req::Lease { q, max, visibility_ms }, vec![], move |rep| match rep {
                    Reply::Ok(v, blobs) => {
                        let mut out = Vec::new();
                        for (meta, mut b) in v.as_array().cloned().unwrap_or_default().into_iter().zip(blobs) {
                            if let Ok(m) = Message::decode(&mut b) {
                                out.push(Leased {
                                    seq: meta["seq"].as_u64().unwrap_or(0),
                                    msg: Arc::new(m),
                                    delivery_count: meta["delivery_count"].as_u64().unwrap_or(0) as u32,
                                    redelivered: meta["redelivered"].as_bool().unwrap_or(false),
                                    deadline_ms: meta["deadline_ms"].as_u64().unwrap_or(0),
                                });
                            }
                        }
                        let _ = reply.send(out);
                        None
                    }
                    Reply::Moved => Some(QueueCmd::Lease { max, visibility_ms, reply }),
                    Reply::Err(_) => {
                        let _ = reply.send(vec![]);
                        None
                    }
                });
                fut.await;
            }
            QueueCmd::SettleLease { seq, delivery_count, outcome, reply } => {
                let req = Req::SettleLease { q, seq, delivery_count, outcome: outcome_code(outcome) };
                let fut = self.remote(peer, req, vec![], move |rep| {
                    let _ = reply.send(matches!(&rep, Reply::Ok(v, _) if v.as_bool() == Some(true)));
                    None
                });
                fut.await;
            }
            QueueCmd::ExtendLease { seq, delivery_count, visibility_ms, reply } => {
                let req = Req::ExtendLease { q, seq, delivery_count, visibility_ms };
                let fut = self.remote(peer, req, vec![], move |rep| {
                    let _ = reply.send(matches!(&rep, Reply::Ok(v, _) if v.as_bool() == Some(true)));
                    None
                });
                fut.await;
            }
            QueueCmd::Peek { offset, count, reply } => {
                let fut = self.remote(peer, Req::Peek { q, offset, count }, vec![], move |rep| match rep {
                    Reply::Ok(v, blobs) => {
                        let mut out = Vec::new();
                        for (meta, mut b) in v.as_array().cloned().unwrap_or_default().into_iter().zip(blobs) {
                            if let Ok(m) = Message::decode(&mut b) {
                                out.push(Peeked {
                                    seq: meta["seq"].as_u64().unwrap_or(0),
                                    msg: Arc::new(m),
                                    redelivered: meta["redelivered"].as_bool().unwrap_or(false),
                                    delivery_count: meta["delivery_count"].as_u64().unwrap_or(0) as u32,
                                });
                            }
                        }
                        let _ = reply.send(out);
                        None
                    }
                    Reply::Moved => Some(QueueCmd::Peek { offset, count, reply }),
                    Reply::Err(_) => {
                        let _ = reply.send(vec![]);
                        None
                    }
                });
                fut.await;
            }
            QueueCmd::Consumers { reply } => {
                let fut = self.remote(peer, Req::Consumers { q }, vec![], move |rep| {
                    let v = match rep {
                        Reply::Ok(v, _) => serde_json::from_value::<Vec<Value>>(v).unwrap_or_default(),
                        _ => vec![],
                    };
                    let out = v
                        .iter()
                        .map(|c| ConsumerInfo {
                            consumer_tag: c["consumer_tag"].as_str().unwrap_or_default().into(),
                            channel: c["channel"].as_u64().unwrap_or(0),
                            connection: c["connection"].as_u64().unwrap_or(0),
                            ack_required: c["ack_required"].as_bool().unwrap_or(true),
                            exclusive: c["exclusive"].as_bool().unwrap_or(false),
                            priority: c["priority"].as_i64().unwrap_or(0) as i32,
                            active: c["active"].as_bool().unwrap_or(true),
                            arguments: FieldTable::from_json(&c["arguments"]),
                        })
                        .collect();
                    let _ = reply.send(out);
                    None
                });
                fut.await;
            }
            cmd => drop_cmd(cmd, &self.start.meta),
        }
    }
}

/// Answer a command that can't be served (no owner reachable).
fn drop_cmd(cmd: QueueCmd, meta: &QueueMeta) {
    let gone = || AmqpError::internal(format!("queue '{}' in vhost '{}' is unavailable (owner unreachable)", meta.name, meta.vhost));
    match cmd {
        QueueCmd::Publish { reply: Some(r), .. } => {
            let _ = r.send(PublishResult::Rejected);
        }
        QueueCmd::Consume { reply, .. } => {
            let _ = reply.send(Err(gone()));
        }
        QueueCmd::Get { reply, .. } => {
            let _ = reply.send(Err(gone()));
        }
        QueueCmd::Delete { reply, .. } => {
            let _ = reply.send(Err(gone()));
        }
        QueueCmd::Purge { reply } => {
            let _ = reply.send(0);
        }
        QueueCmd::Lease { reply, .. } => {
            let _ = reply.send(vec![]);
        }
        QueueCmd::Peek { reply, .. } => {
            let _ = reply.send(vec![]);
        }
        QueueCmd::Consumers { reply } => {
            let _ = reply.send(vec![]);
        }
        QueueCmd::Stop { reply } => {
            let _ = reply.send(());
        }
        _ => {}
    }
}

/// Counters a stats reply carries, in order.
pub(crate) const STATS: usize = 11;

pub(crate) fn stats_of(s: &QueueShared) -> [u64; STATS] {
    [
        s.ready.load(Ordering::Relaxed),
        s.unacked.load(Ordering::Relaxed),
        s.consumers.load(Ordering::Relaxed),
        s.published.load(Ordering::Relaxed),
        s.delivered.load(Ordering::Relaxed),
        s.acked.load(Ordering::Relaxed),
        s.redelivered.load(Ordering::Relaxed),
        s.ready_bytes.load(Ordering::Relaxed),
        s.unacked_bytes.load(Ordering::Relaxed),
        s.log_bytes.load(Ordering::Relaxed),
        s.dead_lettered.load(Ordering::Relaxed),
    ]
}

pub(crate) fn apply_stats(s: &QueueShared, v: &[u64]) {
    if v.len() < STATS {
        return;
    }
    let was_empty = s.ready.load(Ordering::Relaxed) == 0;
    s.ready.store(v[0], Ordering::Relaxed);
    s.unacked.store(v[1], Ordering::Relaxed);
    s.consumers.store(v[2], Ordering::Relaxed);
    s.published.store(v[3], Ordering::Relaxed);
    s.delivered.store(v[4], Ordering::Relaxed);
    s.acked.store(v[5], Ordering::Relaxed);
    s.redelivered.store(v[6], Ordering::Relaxed);
    s.ready_bytes.store(v[7], Ordering::Relaxed);
    s.unacked_bytes.store(v[8], Ordering::Relaxed);
    s.log_bytes.store(v[9], Ordering::Relaxed);
    s.dead_lettered.store(v[10], Ordering::Relaxed);
    if was_empty && v[0] > 0 {
        s.available.notify_waiters();
    }
}

/// Sink the owner registers for a consumer on another node: sends deliveries over RPC within
/// the credit that node grants.
pub(crate) struct RemoteSink {
    pub sub: u64,
    pub tx: mpsc::Sender<crate::rpc::Frame>,
    pub credit: AtomicU32,
    pub paused: Arc<AtomicBool>,
}

impl RemoteSink {
    fn push(&self, ev: Push, blobs: Vec<Bytes>) {
        let head = serde_json::to_value(ev).unwrap_or(Value::Null);
        let _ = self.tx.try_send(crate::rpc::Frame { kind: crate::rpc::PUSH, id: 0, head, blobs });
    }
}

impl ConsumerSink for RemoteSink {
    fn has_credit(&self) -> bool {
        !self.paused.load(Ordering::Acquire) && self.credit.load(Ordering::Acquire) > 0 && self.tx.capacity() > 16
    }
    fn deliver(&self, d: Delivery) {
        let _ = self.credit.try_update(Ordering::AcqRel, Ordering::Acquire, |c| c.checked_sub(1));
        let ev = Push::Deliver { sub: self.sub, seq: d.seq, redelivered: d.redelivered, delivery_count: d.delivery_count, remaining: d.remaining };
        self.push(ev, vec![encode_msg(&d.msg)]);
    }
    fn cancelled(&self, _tag: &str) {
        if !self.paused.load(Ordering::Acquire) {
            self.push(Push::Cancelled { sub: self.sub }, vec![]);
        }
    }
    fn activated(&self, _tag: &str, active: bool) {
        self.push(Push::Activated { sub: self.sub, active }, vec![]);
    }
    fn timed_out(&self, _tag: &str, timeout_ms: u64) {
        self.push(Push::TimedOut { sub: self.sub, timeout_ms }, vec![]);
    }
}

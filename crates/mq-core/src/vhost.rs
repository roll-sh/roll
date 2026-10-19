//! A virtual host: its exchanges, queues and bindings, routing and publishing.

use crate::broker::Broker;
use crate::deadletter;
use crate::delayed::Delayed;
use crate::error::{ACCESS_REFUSED, AmqpError, NOT_FOUND, PRECONDITION_FAILED, Result};
use crate::exchange::{Binding, Dest, DestKind, Exchange, ExchangeKind, ExchangeRef};
use crate::message::{Message, MessageRef};
use crate::meta::{self, kind};
use crate::policy::{self, Target};
use crate::queue::config::{QueueConfig, QueueType, declared_type};
use crate::queue::engine::QueueHost;
use crate::queue::{ConfirmTicket, ConnId, QueueMeta, QueueRef};
use crate::value::{FieldTable, FieldValue};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tracing::{debug, warn};

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct VHostInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub default_queue_type: Option<String>,
    #[serde(default)]
    pub tracing: bool,
    /// `max-connections`, `max-queues`, and roll's `max-publish-rate` (messages/s) and
    /// `max-publish-bytes-rate` (negative or absent: unlimited, or the broker's default budget).
    #[serde(default)]
    pub limits: HashMap<String, i64>,
}

#[derive(Default, Debug)]
pub struct VHostStats {
    pub published: AtomicU64,
    pub published_bytes: AtomicU64,
    pub routed: AtomicU64,
    pub unroutable: AtomicU64,
    pub returned: AtomicU64,
    pub confirmed: AtomicU64,
    pub delivered: AtomicU64,
    pub delivered_bytes: AtomicU64,
    pub acked: AtomicU64,
    pub connections: AtomicU64,
    pub channels: AtomicU64,
    /// Connection-seconds accumulated by closed connections (live ones are added on read).
    pub connection_ms_closed: AtomicU64,
    /// Unix ms of the last client activity (connect, publish, deliver). Drives scale-to-zero.
    pub last_active_ms: AtomicU64,
    /// Time publishers spent waiting for the vhost's publish budget, in ms.
    pub throttled_ms: AtomicU64,
}

/// A vhost's publish budget: a token bucket per rate (one second's worth deep) that publishers
/// may overdraw, then wait off. Waiting holds up the publisher's channel and, through TCP, the
/// client, like RabbitMQ's flow control, so a busy tenant slows itself down, not its neighbours.
#[derive(Default)]
struct Budget {
    msgs: AtomicU64,
    bytes: AtomicU64,
    bucket: Mutex<Bucket>,
}

#[derive(Default)]
struct Bucket {
    at: Option<std::time::Instant>,
    msgs: f64,
    bytes: f64,
}

/// The longest a publisher waits off one overdraft (a huge message against a small budget).
const MAX_THROTTLE_S: f64 = 5.0;

impl Bucket {
    /// Take `n` from a bucket refilled at `rate`/s since `dt`; the wait it now owes, in seconds.
    fn take(level: &mut f64, rate: u64, dt: f64, n: u64) -> f64 {
        if rate == 0 {
            return 0.0;
        }
        let r = rate as f64;
        *level = ((*level + dt * r).min(r) - n as f64).max(-MAX_THROTTLE_S * r);
        (-*level / r).max(0.0)
    }
}

pub struct DeclareQueue {
    pub name: String,
    pub durable: bool,
    pub exclusive_owner: Option<ConnId>,
    pub auto_delete: bool,
    pub args: FieldTable,
    pub passive: bool,
}

#[derive(Debug)]
pub struct DeclareOk {
    pub queue: QueueRef,
    pub name: String,
    pub messages: u32,
    pub consumers: u32,
}

/// Result of routing one message.
pub struct PublishOutcome {
    /// Queues the message went to (0 = unroutable).
    pub routed: usize,
    /// The caller's ticket, handed back so a `basic.return` can go out before the ack.
    pub confirm: Option<ConfirmTicket>,
}

pub struct VHost {
    pub name: String,
    broker: Weak<Broker>,
    me: Weak<VHost>,
    pub info: RwLock<VHostInfo>,
    exchanges: RwLock<HashMap<String, ExchangeRef>>,
    queues: RwLock<HashMap<String, QueueRef>>,
    /// destination → sources bound to it (for cleanup when the destination goes away).
    reverse: Mutex<HashMap<Dest, HashSet<String>>>,
    /// Serialises topology changes (declares, deletes, binds) within the vhost.
    topo: tokio::sync::Mutex<()>,
    delayed: Mutex<HashMap<String, Arc<Delayed>>>,
    /// Exchange-level dedup caches (`x-message-deduplication` exchanges).
    dedup: Mutex<HashMap<String, DedupCache>>,
    budget: Budget,
    pub stats: VHostStats,
}

#[derive(Default)]
struct DedupCache {
    seen: HashMap<String, u64>,
    order: VecDeque<String>,
}

impl std::fmt::Debug for VHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VHost").field("name", &self.name).finish()
    }
}

struct Host(Weak<VHost>);

impl QueueHost for Host {
    fn republish(&self, exchange: String, msg: Message, from_queue: String) {
        let Some(vh) = self.0.upgrade() else { return };
        tokio::spawn(async move {
            if let Err(e) = vh.publish_dead_letter(&exchange, msg, &from_queue).await {
                debug!(vhost = %vh.name, %exchange, error = %e, "dead letter dropped");
            }
        });
    }

    fn queue_removed(&self, name: &str) {
        let Some(vh) = self.0.upgrade() else { return };
        let name = name.to_string();
        // Only drop the map entry if it's still the queue that deleted itself.
        let removed = {
            let mut qs = vh.queues.write();
            match qs.get(&name) {
                Some(q) if q.is_deleted() => qs.remove(&name),
                _ => None,
            }
        };
        if let Some(q) = removed {
            tokio::spawn(async move { vh.after_queue_removed(&q).await });
        }
    }
}

pub const SYSTEM_EXCHANGES: &[(&str, &str)] =
    &[("amq.direct", "direct"), ("amq.fanout", "fanout"), ("amq.topic", "topic"), ("amq.headers", "headers"), ("amq.match", "headers")];

pub fn generate_queue_name() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut b = [0u8; 22];
    getrandom::fill(&mut b).expect("random");
    let s: String = b.iter().map(|x| ALPHABET[(*x & 63) as usize] as char).collect();
    format!("amq.gen-{s}")
}

fn inequivalent(arg: &str, what: &str, name: &str, vhost: &str, received: impl std::fmt::Display, current: impl std::fmt::Display) -> AmqpError {
    AmqpError::new(
        PRECONDITION_FAILED,
        format!("inequivalent arg '{arg}' for {what} '{name}' in vhost '{vhost}': received '{received}' but current is '{current}'"),
    )
}

/// Queue arguments RabbitMQ compares on redeclaration.
const CHECKED_QUEUE_ARGS: &[&str] = &[
    "x-expires",
    "x-message-ttl",
    "x-dead-letter-exchange",
    "x-dead-letter-routing-key",
    "x-max-length",
    "x-max-length-bytes",
    "x-max-priority",
    "x-overflow",
    "x-queue-mode",
    "x-single-active-consumer",
    "x-delivery-limit",
    "x-max-age",
];

fn describe_arg(v: Option<&FieldValue>) -> String {
    match v {
        None => "none".into(),
        Some(v) => format!("the value '{}'", v.to_json().to_string().trim_matches('"')),
    }
}

impl VHost {
    pub(crate) fn new(broker: Weak<Broker>, info: VHostInfo) -> Arc<Self> {
        let vh = Arc::new_cyclic(|me| {
            let mut exchanges = HashMap::new();
            exchanges.insert(String::new(), Arc::new(Exchange::new("", ExchangeKind::Direct, true, false, false, FieldTable::new())));
            for (n, k) in SYSTEM_EXCHANGES {
                let kind = ExchangeKind::parse(k, &FieldTable::new()).unwrap();
                exchanges.insert(n.to_string(), Arc::new(Exchange::new(*n, kind, true, false, false, FieldTable::new())));
            }
            exchanges.insert(
                "amq.rabbitmq.trace".into(),
                Arc::new(Exchange::new("amq.rabbitmq.trace", ExchangeKind::Topic, true, false, true, FieldTable::new())),
            );
            VHost {
                name: info.name.clone(),
                broker,
                me: me.clone(),
                info: RwLock::new(info),
                exchanges: RwLock::new(exchanges),
                queues: RwLock::default(),
                reverse: Mutex::default(),
                topo: tokio::sync::Mutex::new(()),
                delayed: Mutex::default(),
                dedup: Mutex::default(),
                // Idle since it loaded: a vhost woken by a connecting client must not look idle
                // to the reaper before that client has registered.
                budget: Budget::default(),
                stats: VHostStats { last_active_ms: AtomicU64::new(crate::now_ms()), ..Default::default() },
            }
        });
        vh.refresh_limits();
        vh
    }

    /// Re-read the publish budget after the vhost's limits changed.
    pub(crate) fn refresh_limits(&self) {
        let (rate, bytes) = self.broker.upgrade().map(|b| (b.config.vhost_publish_rate, b.config.vhost_publish_bytes_rate)).unwrap_or_default();
        let limits = &self.info.read().limits;
        let get = |k: &str, default: u64| limits.get(k).map(|v| (*v).max(0) as u64).unwrap_or(default);
        self.budget.msgs.store(get("max-publish-rate", rate), Ordering::Relaxed);
        self.budget.bytes.store(get("max-publish-bytes-rate", bytes), Ordering::Relaxed);
    }

    /// Wait until the vhost's publish budget covers `msgs` messages of `bytes` bytes (see
    /// [`Budget`]). Free without a budget. Every publish path goes through here.
    pub async fn throttle(&self, msgs: u64, bytes: u64) {
        let (mr, br) = (self.budget.msgs.load(Ordering::Relaxed), self.budget.bytes.load(Ordering::Relaxed));
        if mr == 0 && br == 0 {
            return;
        }
        let wait = {
            let mut b = self.budget.bucket.lock();
            let now = std::time::Instant::now();
            // A fresh bucket starts full.
            let dt = b.at.map_or(1.0, |t| now.duration_since(t).as_secs_f64());
            b.at = Some(now);
            let b = &mut *b;
            Bucket::take(&mut b.msgs, mr, dt, msgs).max(Bucket::take(&mut b.bytes, br, dt, bytes))
        };
        if wait > 0.0 {
            self.stats.throttled_ms.fetch_add((wait * 1000.0) as u64, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
        }
    }

    pub fn broker(&self) -> Arc<Broker> {
        self.broker.upgrade().expect("broker dropped")
    }

    fn host(&self) -> Arc<dyn QueueHost> {
        Arc::new(Host(self.me.clone()))
    }

    pub fn touch(&self) {
        self.stats.last_active_ms.store(crate::now_ms(), Ordering::Relaxed);
    }

    pub fn default_queue_type(&self) -> QueueType {
        let dqt = self.info.read().default_queue_type.clone();
        dqt.as_deref().and_then(QueueType::parse).or_else(|| QueueType::parse(&self.broker().config.default_queue_type)).unwrap_or_default()
    }

    // ── lookups ──

    pub fn exchange(&self, name: &str) -> Option<ExchangeRef> {
        self.exchanges.read().get(name).cloned()
    }

    pub fn exchanges(&self) -> Vec<ExchangeRef> {
        let mut v: Vec<_> = self.exchanges.read().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn queue(&self, name: &str) -> Option<QueueRef> {
        self.queues.read().get(name).cloned()
    }

    pub fn queues(&self) -> Vec<QueueRef> {
        let mut v: Vec<_> = self.queues.read().values().cloned().collect();
        v.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        v
    }

    /// Visit every queue without collecting or sorting them (samplers walk them all often).
    pub fn for_each_queue(&self, mut f: impl FnMut(&QueueRef)) {
        for q in self.queues.read().values() {
            f(q);
        }
    }

    /// Visit every exchange without collecting or sorting them.
    pub fn for_each_exchange(&self, mut f: impl FnMut(&ExchangeRef)) {
        for x in self.exchanges.read().values() {
            f(x);
        }
    }

    pub fn queue_count(&self) -> usize {
        self.queues.read().len()
    }

    pub fn exchange_or_404(&self, name: &str) -> Result<ExchangeRef> {
        self.exchange(name).ok_or_else(|| AmqpError::not_found(format!("exchange '{name}' in vhost '{}'", self.name)))
    }

    /// A queue the connection may use (exclusive queues belong to their connection).
    pub fn queue_for(&self, name: &str, conn: Option<ConnId>) -> Result<QueueRef> {
        let q = self.queue(name).ok_or_else(|| AmqpError::not_found(format!("queue '{name}' in vhost '{}'", self.name)))?;
        self.check_exclusive(&q, conn)?;
        Ok(q)
    }

    fn check_exclusive(&self, q: &QueueRef, conn: Option<ConnId>) -> Result<()> {
        if let Some(owner) = q.meta.exclusive_owner
            && Some(owner) != conn
            && conn.is_some()
        {
            return Err(AmqpError::locked(format!(
                "cannot obtain exclusive access to locked queue '{}' in vhost '{}'. It could be originally declared on another connection or the exclusive property value does not match that of the original declaration.",
                q.meta.name, self.name
            )));
        }
        Ok(())
    }

    pub fn bindings(&self) -> Vec<Binding> {
        let mut out = Vec::new();
        // The default exchange's implicit bindings, as the management API lists them.
        for q in self.queues() {
            out.push(Binding { source: String::new(), dest: Dest::queue(&q.meta.name), routing_key: q.meta.name.clone(), args: FieldTable::new() });
        }
        for ex in self.exchanges() {
            if !ex.is_default() {
                out.extend(ex.bindings());
            }
        }
        out
    }

    // ── exchanges ──

    #[allow(clippy::too_many_arguments)]
    pub async fn declare_exchange(
        &self,
        name: &str,
        kind_name: &str,
        durable: bool,
        auto_delete: bool,
        internal: bool,
        args: FieldTable,
        passive: bool,
    ) -> Result<ExchangeRef> {
        if passive {
            return self.exchange_or_404(name);
        }
        if name.is_empty() {
            return Err(AmqpError::access_refused("operation not permitted on the default exchange"));
        }
        let kind = ExchangeKind::parse(kind_name, &args)?;
        let _g = self.topo.lock().await;
        if let Some(ex) = self.exchange(name) {
            let (vh, what) = (&self.name, "exchange");
            if ex.kind.name() != kind.name() {
                return Err(inequivalent("type", what, name, vh, kind.name(), ex.kind.name()));
            }
            if ex.durable != durable {
                return Err(inequivalent("durable", what, name, vh, durable, ex.durable));
            }
            if ex.auto_delete != auto_delete {
                return Err(inequivalent("auto_delete", what, name, vh, auto_delete, ex.auto_delete));
            }
            if ex.internal != internal {
                return Err(inequivalent("internal", what, name, vh, internal, ex.internal));
            }
            let (a, b) = (args.get("alternate-exchange"), ex.args.get("alternate-exchange"));
            if a.map(|v| v.as_str()) != b.map(|v| v.as_str()) {
                return Err(AmqpError::precondition(format!(
                    "inequivalent arg 'alternate-exchange' for exchange '{name}' in vhost '{vh}': received {} but current is {}",
                    describe_arg(a),
                    describe_arg(b)
                )));
            }
            return Ok(ex);
        }
        if name.starts_with("amq.") {
            return Err(AmqpError::access_refused(format!("exchange name '{name}' contains reserved prefix 'amq.*'")));
        }
        let ex = Arc::new(Exchange::new(name, kind.clone(), durable, auto_delete, internal, args.clone()));
        if let ExchangeKind::Delayed(_) = kind {
            let d = Delayed::start(self.me.clone(), name, durable).await?;
            self.delayed.lock().insert(name.to_string(), d);
        }
        if durable {
            let v = json!({
                "name": name, "vhost": self.name, "type": kind_name, "durable": durable,
                "auto_delete": auto_delete, "internal": internal, "arguments": args.to_json(),
            });
            self.broker().meta.put(kind::EXCHANGE, &meta::key(&[&self.name, name]), &v).await?;
        }
        self.exchanges.write().insert(name.to_string(), ex.clone());
        Ok(ex)
    }

    pub async fn delete_exchange(&self, name: &str, if_unused: bool) -> Result<()> {
        if name.is_empty() {
            return Err(AmqpError::access_refused("operation not permitted on the default exchange"));
        }
        if name.starts_with("amq.") {
            return Err(AmqpError::access_refused(format!("deletion of system exchange '{name}' in vhost '{}' is not allowed", self.name)));
        }
        let _g = self.topo.lock().await;
        self.delete_exchange_locked(name, if_unused).await
    }

    async fn delete_exchange_locked(&self, name: &str, if_unused: bool) -> Result<()> {
        let ex = self.exchange_or_404(name)?;
        let dest = Dest::exchange(name);
        let has_dest_bindings = self.reverse.lock().get(&dest).is_some_and(|s| !s.is_empty());
        if if_unused && (ex.binding_count() > 0 || has_dest_bindings) {
            return Err(AmqpError::precondition(format!("exchange '{name}' in vhost '{}' in use", self.name)));
        }
        self.exchanges.write().remove(name);
        // Bindings from this exchange.
        for b in ex.bindings() {
            self.forget_binding(&b).await;
        }
        // Bindings to this exchange.
        let sources = self.reverse.lock().remove(&dest).unwrap_or_default();
        for s in sources {
            if let Some(src) = self.exchange(&s) {
                for b in src.bindings().into_iter().filter(|b| b.dest == dest) {
                    src.unbind(&b.dest, &b.routing_key, &b.args);
                    self.delete_binding_meta(&b).await;
                }
                self.maybe_auto_delete_exchange(&src).await;
            }
        }
        if let Some(d) = self.delayed.lock().remove(name) {
            d.destroy();
        }
        self.dedup.lock().remove(name);
        if ex.durable {
            self.broker().meta.delete(kind::EXCHANGE, &meta::key(&[&self.name, name])).await?;
        }
        Ok(())
    }

    async fn maybe_auto_delete_exchange(&self, ex: &ExchangeRef) {
        if ex.auto_delete && ex.binding_count() == 0 && !ex.is_default() && self.exchange(&ex.name).is_some() {
            let _ = Box::pin(self.delete_exchange_locked(&ex.name, false)).await;
        }
    }

    // ── queues ──

    pub async fn declare_queue(&self, d: DeclareQueue) -> Result<DeclareOk> {
        let name = if d.name.is_empty() { generate_queue_name() } else { d.name.clone() };
        if let Some(q) = self.queue(&name) {
            return self.redeclare(q, &d, d.exclusive_owner).await;
        }
        if d.passive {
            return Err(AmqpError::new(NOT_FOUND, format!("no queue '{name}' in vhost '{}'", self.name)));
        }
        if name.starts_with("amq.") && !d.name.is_empty() {
            return Err(AmqpError::new(ACCESS_REFUSED, format!("queue name '{name}' contains reserved prefix 'amq.*'")));
        }
        let _g = self.topo.lock().await;
        if let Some(q) = self.queue(&name) {
            return self.redeclare(q, &d, d.exclusive_owner).await;
        }
        if let Some(max) = self.info.read().limits.get("max-queues").copied().filter(|v| *v >= 0)
            && self.queue_count() as i64 >= max
        {
            return Err(AmqpError::precondition(format!("cannot declare queue '{name}': queue limit in vhost '{}' ({max}) is reached", self.name)));
        }
        let meta = QueueMeta {
            name: name.clone(),
            vhost: self.name.clone(),
            durable: d.durable,
            auto_delete: d.auto_delete,
            exclusive_owner: d.exclusive_owner,
            arguments: d.args.clone(),
            created_at: crate::now_ms() as i64 / 1000,
        };
        let generation = format!("{:016x}", crate::rand_u64());
        let q = self.start_queue(meta, &generation).await?;
        if d.durable && d.exclusive_owner.is_none() {
            let v = json!({
                "name": name, "vhost": self.name, "durable": true, "auto_delete": d.auto_delete,
                "arguments": d.args.to_json(), "type": q.kind.name(), "generation": generation,
                "created_at": q.meta.created_at,
            });
            self.broker().meta.put(kind::QUEUE, &meta::key(&[&self.name, &name]), &v).await?;
        }
        self.queues.write().insert(name.clone(), q.clone());
        Ok(DeclareOk { queue: q, name, messages: 0, consumers: 0 })
    }

    /// Start a queue actor (new, or recovered from the metadata store).
    pub(crate) async fn start_queue(&self, meta: QueueMeta, generation: &str) -> Result<QueueRef> {
        let (name, vh) = (meta.name.clone(), self.name.clone());
        let kind = declared_type(&meta.arguments, &name, &vh, self.default_queue_type())?;
        if kind != QueueType::Classic {
            let bad = if !meta.durable {
                Some("non-durable")
            } else if meta.exclusive() {
                Some("exclusive")
            } else if meta.auto_delete {
                Some("auto-delete")
            } else {
                None
            };
            if let Some(b) = bad {
                return Err(AmqpError::precondition(format!("invalid property '{b}' for queue '{name}' in vhost '{vh}'")));
            }
        }
        let config = self.queue_config(kind, &meta.arguments, &name)?;
        let broker = self.broker();
        let durable = meta.durable && meta.exclusive_owner.is_none();
        let log_key = format!("q/{}/{}/{}", self.name, name, generation);
        let partitions = if kind == QueueType::Stream { 1 } else { config.partitions.max(1) };
        let start = crate::cluster::QueueStart { meta, kind, config, log_key, durable, host: self.host(), partition: 0 };
        let placement = broker.placement();
        let place = async |s: crate::cluster::QueueStart| match &placement {
            Some(p) if s.placeable() => p.start(s).await,
            _ => s.spawn_local(broker.logs.as_ref(), 0).await,
        };
        if partitions == 1 {
            return place(start).await;
        }
        let m = &start.meta;
        if !durable || m.auto_delete || start.config.expires.is_some() {
            return Err(AmqpError::precondition(format!("invalid property 'x-partitions' for queue '{name}' in vhost '{vh}': partitioned queues must be durable, not exclusive, not auto-delete and without x-expires")));
        }
        let mut parts = Vec::with_capacity(partitions as usize);
        for i in 0..partitions {
            let mut s = start.clone();
            s.partition = i;
            s.log_key = format!("{}/p{i}", start.log_key);
            parts.push(place(s).await?);
        }
        Ok(crate::queue::partitioned::spawn(start.meta, kind, parts))
    }

    pub(crate) fn queue_config(&self, kind: QueueType, args: &FieldTable, name: &str) -> Result<QueueConfig> {
        let broker = self.broker();
        let policies = broker.policies.read();
        let ops = broker.operator_policies.read();
        let re = &broker.regex;
        let p = policy::pick(&policies, &self.name, Target::Queue(kind), name, re).map(|p| (p.name.as_str(), &p.definition));
        let o = policy::pick(&ops, &self.name, Target::Queue(kind), name, re).map(|p| (p.name.as_str(), &p.definition));
        QueueConfig::resolve(kind, args, name, &self.name, p, o)
    }

    async fn redeclare(&self, q: QueueRef, d: &DeclareQueue, conn: Option<ConnId>) -> Result<DeclareOk> {
        self.check_exclusive(&q, conn)?;
        let name = q.meta.name.clone();
        let vh = &self.name;
        if !d.passive {
            if q.meta.durable != d.durable {
                return Err(inequivalent("durable", "queue", &name, vh, d.durable, q.meta.durable));
            }
            if q.meta.auto_delete != d.auto_delete {
                return Err(inequivalent("auto_delete", "queue", &name, vh, d.auto_delete, q.meta.auto_delete));
            }
            if q.meta.exclusive() != d.exclusive_owner.is_some() {
                return Err(inequivalent("exclusive", "queue", &name, vh, d.exclusive_owner.is_some(), q.meta.exclusive()));
            }
            if let Some(t) = d.args.get_str("x-queue-type")
                && t != q.kind.name()
            {
                return Err(AmqpError::precondition(format!(
                    "inequivalent arg 'x-queue-type' for queue '{name}' in vhost '{vh}': received the value '{t}' of type 'longstr' but current is the value '{}' of type 'longstr'",
                    q.kind.name()
                )));
            }
            for a in CHECKED_QUEUE_ARGS {
                let (new, cur) = (d.args.get(a), q.meta.arguments.get(a));
                let same = match (new, cur) {
                    (None, None) => true,
                    (Some(x), Some(y)) => x.equivalent(y),
                    _ => false,
                };
                if !same {
                    return Err(AmqpError::precondition(format!(
                        "inequivalent arg '{a}' for queue '{name}' in vhost '{vh}': received {} but current is {}",
                        describe_arg(new),
                        describe_arg(cur)
                    )));
                }
            }
        }
        let _ = q.send(crate::queue::QueueCmd::Touch).await;
        // A round trip through the actor, so the counts include everything this connection
        // published before the declare (RabbitMQ answers declare-ok from the queue process).
        q.consumers().await;
        let s = &q.shared;
        Ok(DeclareOk {
            messages: s.ready.load(Ordering::Relaxed) as u32,
            consumers: s.consumers.load(Ordering::Relaxed) as u32,
            name,
            queue: q,
        })
    }

    pub async fn delete_queue(&self, name: &str, if_unused: bool, if_empty: bool, conn: Option<ConnId>) -> Result<u64> {
        let q = self.queue_for(name, conn)?;
        let (reply, rx) = tokio::sync::oneshot::channel();
        q.send(crate::queue::QueueCmd::Delete { if_unused, if_empty, reply }).await?;
        let n = rx.await.map_err(|_| AmqpError::not_found(format!("queue '{name}' in vhost '{}'", self.name)))??;
        Ok(n)
    }

    /// Cleanup after a queue is gone: bindings, metadata, auto-delete exchanges.
    async fn after_queue_removed(&self, q: &QueueRef) {
        let _g = self.topo.lock().await;
        let dest = Dest::queue(&q.meta.name);
        let sources = self.reverse.lock().remove(&dest).unwrap_or_default();
        for s in sources {
            if let Some(src) = self.exchange(&s) {
                for b in src.bindings().into_iter().filter(|b| b.dest == dest) {
                    src.unbind(&b.dest, &b.routing_key, &b.args);
                    self.delete_binding_meta(&b).await;
                }
                self.maybe_auto_delete_exchange(&src).await;
            }
        }
        if q.meta.durable && q.meta.exclusive_owner.is_none() {
            let broker = self.broker();
            if let Err(e) = broker.meta.delete(kind::QUEUE, &meta::key(&[&self.name, &q.meta.name])).await {
                warn!(queue = %q.meta.name, error = %e, "failed to delete queue metadata");
            }
            broker.on_queue_deleted(&self.name, &q.meta.name).await;
        }
    }

    /// Delete the exclusive queues of a closed connection.
    pub async fn connection_closed(&self, conn: ConnId) {
        let owned: Vec<QueueRef> = self.queues.read().values().filter(|q| q.meta.exclusive_owner == Some(conn)).cloned().collect();
        for q in owned {
            let _ = self.delete_queue(&q.meta.name.clone(), false, false, Some(conn)).await;
        }
    }

    // ── bindings ──

    fn binding_key(b: &Binding, vhost: &str) -> String {
        let kind = match b.dest.kind {
            DestKind::Queue => "q",
            DestKind::Exchange => "e",
        };
        meta::key(&[vhost, &b.source, kind, &b.dest.name, &b.routing_key, &b.args.to_json().to_string()])
    }

    fn dest_durable(&self, dest: &Dest) -> bool {
        match dest.kind {
            DestKind::Queue => self.queue(&dest.name).is_some_and(|q| q.meta.durable && q.meta.exclusive_owner.is_none()),
            DestKind::Exchange => self.exchange(&dest.name).is_some_and(|e| e.durable),
        }
    }

    pub async fn bind(&self, source: &str, dest: Dest, routing_key: &str, args: FieldTable, conn: Option<ConnId>) -> Result<()> {
        if source.is_empty() || (dest.kind == DestKind::Exchange && dest.name.is_empty()) {
            return Err(AmqpError::access_refused("operation not permitted on the default exchange"));
        }
        let _g = self.topo.lock().await;
        let src = self.exchange_or_404(source)?;
        match dest.kind {
            DestKind::Queue => {
                self.queue_for(&dest.name, conn)?;
            }
            DestKind::Exchange => {
                self.exchange_or_404(&dest.name)?;
            }
        }
        if src.kind == ExchangeKind::Headers {
            // RabbitMQ validates x-match.
            if let Some(m) = args.get("x-match")
                && !matches!(m.as_str(), Some("all" | "any" | "all-with-x" | "any-with-x"))
            {
                return Err(AmqpError::precondition(format!("Invalid x-match field value {}; expected all, any, all-with-x, or any-with-x", m.to_json())));
            }
        }
        if src.bind(dest.clone(), routing_key, args.clone()) {
            self.reverse.lock().entry(dest.clone()).or_default().insert(source.to_string());
            if src.durable && self.dest_durable(&dest) {
                let b = Binding { source: source.into(), dest: dest.clone(), routing_key: routing_key.into(), args: args.clone() };
                let v = json!({
                    "source": source, "vhost": self.name, "destination": dest.name,
                    "destination_type": match dest.kind { DestKind::Queue => "queue", DestKind::Exchange => "exchange" },
                    "routing_key": routing_key, "arguments": args.to_json(),
                });
                self.broker().meta.put(kind::BINDING, &Self::binding_key(&b, &self.name), &v).await?;
            }
        }
        Ok(())
    }

    pub async fn unbind(&self, source: &str, dest: Dest, routing_key: &str, args: FieldTable, conn: Option<ConnId>) -> Result<()> {
        if source.is_empty() {
            return Err(AmqpError::access_refused("operation not permitted on the default exchange"));
        }
        let _g = self.topo.lock().await;
        let src = self.exchange_or_404(source)?;
        if dest.kind == DestKind::Queue {
            self.queue_for(&dest.name, conn)?;
        } else {
            self.exchange_or_404(&dest.name)?;
        }
        if src.unbind(&dest, routing_key, &args) {
            let b = Binding { source: source.into(), dest: dest.clone(), routing_key: routing_key.into(), args };
            self.after_unbind(&src, &b).await;
        }
        // Unbinding something that doesn't exist is fine (like RabbitMQ).
        Ok(())
    }

    async fn after_unbind(&self, src: &ExchangeRef, b: &Binding) {
        if !src.bindings().iter().any(|x| x.dest == b.dest)
            && let Some(set) = self.reverse.lock().get_mut(&b.dest)
        {
            set.remove(&src.name);
        }
        self.delete_binding_meta(b).await;
        self.maybe_auto_delete_exchange(src).await;
    }

    async fn forget_binding(&self, b: &Binding) {
        if let Some(set) = self.reverse.lock().get_mut(&b.dest) {
            set.remove(&b.source);
        }
        self.delete_binding_meta(b).await;
    }

    async fn delete_binding_meta(&self, b: &Binding) {
        let _ = self.broker().meta.delete(kind::BINDING, &Self::binding_key(b, &self.name)).await;
    }

    /// Restore a binding at startup without touching the metadata store.
    pub(crate) fn restore_binding(&self, b: Binding) {
        if let Some(src) = self.exchange(&b.source)
            && src.bind(b.dest.clone(), &b.routing_key, b.args)
        {
            self.reverse.lock().entry(b.dest).or_default().insert(b.source);
        }
    }

    pub(crate) fn restore_exchange(&self, ex: Exchange) {
        self.exchanges.write().insert(ex.name.clone(), Arc::new(ex));
    }

    pub(crate) async fn restore_delayed(&self, name: &str) -> anyhow::Result<()> {
        let d = Delayed::start(self.me.clone(), name, true).await?;
        self.delayed.lock().insert(name.to_string(), d);
        Ok(())
    }

    pub(crate) fn restore_queue(&self, q: QueueRef) {
        self.queues.write().insert(q.meta.name.clone(), q);
    }

    /// Re-resolve queue settings after a policy change.
    pub async fn apply_policies(&self) {
        for q in self.queues() {
            if let Ok(config) = self.queue_config(q.kind, &q.meta.arguments, &q.meta.name) {
                let _ = q.send(crate::queue::QueueCmd::Reconfigure { config: Box::new(config) }).await;
            }
        }
    }

    // ── routing & publishing ──

    fn alternate_exchange(&self, ex: &Exchange) -> Option<String> {
        if let Some(ae) = ex.args.get_str("alternate-exchange") {
            return Some(ae.to_string());
        }
        let broker = self.broker();
        let policies = broker.policies.read();
        policy::pick(&policies, &self.name, Target::Exchange, &ex.name, &broker.regex)
            .and_then(|p| p.definition.get("alternate-exchange"))
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    /// Queues a message published to `ex` reaches (exchange-to-exchange bindings, CC/BCC and
    /// alternate exchanges included).
    pub fn route(&self, ex: &ExchangeRef, msg: &Message) -> Vec<String> {
        let mut keys: Vec<String> = vec![msg.routing_key.clone()];
        for h in ["CC", "BCC"] {
            if let Some(a) = msg.props.header(h).and_then(|v| v.as_array()) {
                keys.extend(a.iter().filter_map(|v| v.as_str().map(String::from)));
            }
        }
        let mut queues: Vec<String> = Vec::new();
        let mut seen_q: HashSet<String> = HashSet::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut current = Some(ex.clone());
        while let Some(start) = current.take() {
            let mut stack = vec![start.clone()];
            visited.insert(start.name.clone());
            let mut dests = Vec::new();
            while let Some(x) = stack.pop() {
                for k in &keys {
                    dests.clear();
                    if x.is_default() {
                        if self.queues.read().contains_key(k) {
                            dests.push(Dest::queue(k.clone()));
                        }
                    } else {
                        x.route(msg, k, &mut dests);
                    }
                    for d in dests.drain(..) {
                        match d.kind {
                            DestKind::Queue => {
                                if seen_q.insert(d.name.clone()) {
                                    queues.push(d.name);
                                }
                            }
                            DestKind::Exchange => {
                                if visited.insert(d.name.clone())
                                    && let Some(next) = self.exchange(&d.name)
                                {
                                    stack.push(next);
                                }
                            }
                        }
                    }
                }
            }
            if queues.is_empty()
                && let Some(ae) = self.alternate_exchange(&start)
                && !visited.contains(&ae)
            {
                current = self.exchange(&ae);
            }
        }
        queues
    }

    /// Publish to an exchange. Routing failures that RabbitMQ reports as errors (missing or
    /// internal exchange) come back as `Err`; unroutable messages come back with `routed == 0`.
    pub async fn publish(&self, exchange: &str, msg: Message, confirm: Option<ConfirmTicket>) -> Result<PublishOutcome> {
        let ex = self.exchange_or_404(exchange)?;
        if ex.internal {
            return Err(AmqpError::access_refused(format!("cannot publish to internal exchange '{exchange}' in vhost '{}'", self.name)));
        }
        self.touch();
        self.throttle(1, msg.size() as u64).await;
        self.stats.published.fetch_add(1, Ordering::Relaxed);
        self.stats.published_bytes.fetch_add(msg.size() as u64, Ordering::Relaxed);
        ex.stats.publish_in.fetch_add(1, Ordering::Relaxed);

        if let ExchangeKind::Delayed(_) = ex.kind
            && let Some(delay) = msg.props.header("x-delay").and_then(|v| v.as_i64())
            && delay > 0
        {
            let d = self.delayed.lock().get(exchange).cloned();
            if let Some(d) = d {
                d.schedule(msg, delay as u64, confirm).await;
                return Ok(PublishOutcome { routed: 1, confirm: None });
            }
        }
        if ex.kind == ExchangeKind::Dedup && self.exchange_duplicate(&ex, &msg) {
            return Ok(PublishOutcome { routed: 1, confirm });
        }
        self.publish_routed(&ex, msg, confirm).await
    }

    fn exchange_duplicate(&self, ex: &Exchange, msg: &Message) -> bool {
        let Some(v) = msg.props.header("x-deduplication-header") else { return false };
        let key = v.as_str().map(String::from).unwrap_or_else(|| v.to_json().to_string());
        let now = crate::now_ms();
        let ttl = msg.props.header("x-cache-ttl").and_then(|v| v.as_i64()).or_else(|| ex.args.get_i64("x-cache-ttl")).map(|v| v as u64);
        let size = ex.args.get_i64("x-cache-size").unwrap_or(100_000).max(1) as usize;
        let mut caches = self.dedup.lock();
        let c = caches.entry(ex.name.clone()).or_default();
        if let Some(&expires) = c.seen.get(&key)
            && expires > now
        {
            return true;
        }
        c.seen.insert(key.clone(), ttl.map(|t| now + t).unwrap_or(u64::MAX));
        c.order.push_back(key);
        while c.order.len() > size {
            if let Some(old) = c.order.pop_front() {
                c.seen.remove(&old);
            }
        }
        false
    }

    /// Route and enqueue (also used by delayed exchanges when a message is due).
    pub(crate) async fn publish_routed(&self, ex: &ExchangeRef, msg: Message, confirm: Option<ConfirmTicket>) -> Result<PublishOutcome> {
        let names = self.route(ex, &msg);
        if names.is_empty() {
            ex.stats.unroutable.fetch_add(1, Ordering::Relaxed);
            self.stats.unroutable.fetch_add(1, Ordering::Relaxed);
            return Ok(PublishOutcome { routed: 0, confirm });
        }
        // BCC recipients must not see the header.
        let msg = if msg.props.header("BCC").is_some() {
            let mut p = msg.props.clone();
            if let Some(h) = p.headers.as_mut() {
                h.remove("BCC");
            }
            Arc::new(msg.with_props(&msg.exchange, &msg.routing_key, p))
        } else {
            Arc::new(msg)
        };
        let mut routed = 0;
        for name in names {
            let Some(q) = self.queue(&name) else { continue };
            let ticket = confirm.as_ref().map(|c| c.share());
            if q.publish(msg.clone(), ticket).await.is_ok() {
                routed += 1;
            }
        }
        ex.stats.publish_out.fetch_add(routed as u64, Ordering::Relaxed);
        self.stats.routed.fetch_add(1, Ordering::Relaxed);
        Ok(PublishOutcome { routed, confirm })
    }

    async fn publish_dead_letter(&self, exchange: &str, msg: Message, from_queue: &str) -> Result<()> {
        let ex = self.exchange_or_404(exchange)?;
        // `from_queue` is in the history already, so cycles back to it are caught here too.
        let _ = from_queue;
        let names: Vec<String> = self.route(&ex, &msg).into_iter().filter(|q| !deadletter::is_cycle(&msg, q)).collect();
        let msg = Arc::new(msg);
        for name in names {
            if let Some(q) = self.queue(&name) {
                q.publish(msg.clone(), None).await?;
            }
        }
        Ok(())
    }

    /// Publish straight to a queue by name (HTTP API, shovels, replay) via the default exchange.
    pub async fn publish_to_queue(&self, queue: &str, msg: MessageRef, confirm: Option<ConfirmTicket>) -> Result<()> {
        let q = self.queue(queue).ok_or_else(|| AmqpError::not_found(format!("queue '{queue}' in vhost '{}'", self.name)))?;
        self.throttle(1, msg.size() as u64).await;
        q.publish(msg, confirm).await
    }

    pub async fn stop(&self) {
        for q in self.queues() {
            q.stop().await;
        }
        for d in self.delayed.lock().drain() {
            d.1.stop();
        }
    }

    /// Delete everything (the vhost is being deleted).
    pub async fn destroy(&self) {
        for q in self.queues() {
            let _ = self.delete_queue(&q.meta.name.clone(), false, false, None).await;
        }
        for d in self.delayed.lock().drain() {
            d.1.destroy();
        }
    }

    /// Nothing is waiting on a timer (delayed messages, TTLs, expiring queues), so the vhost can
    /// be unloaded without changing behaviour.
    pub fn can_sleep(&self) -> bool {
        self.delayed.lock().values().all(|d| d.is_idle())
            && self.queues.read().values().all(|q| {
                let s = &q.shared;
                let has_msgs = s.messages() > 0;
                let timed = q.meta.arguments.get("x-message-ttl").is_some() || q.meta.arguments.get("x-expires").is_some();
                s.consumers.load(Ordering::Relaxed) == 0 && !(timed && (has_msgs || q.meta.arguments.get("x-expires").is_some()))
                    && q.meta.exclusive_owner.is_none()
            })
    }

    pub fn messages(&self) -> (u64, u64) {
        self.queues.read().values().fold((0, 0), |(r, u), q| (r + q.shared.ready.load(Ordering::Relaxed), u + q.shared.unacked.load(Ordering::Relaxed)))
    }

    pub fn bytes_stored(&self) -> u64 {
        self.queues.read().values().map(|q| q.shared.log_bytes.load(Ordering::Relaxed)).sum::<u64>()
            + self.delayed.lock().values().map(|d| d.bytes()).sum::<u64>()
    }

    pub fn info_json(&self) -> Value {
        serde_json::to_value(&*self.info.read()).unwrap_or(Value::Null)
    }
}

// ── cluster: apply changes made on other nodes (in memory only) ──

impl VHost {
    /// Drop an exchange another node deleted.
    pub(crate) fn forget_exchange(&self, name: &str) {
        let Some(ex) = self.exchanges.write().remove(name) else { return };
        let dest = Dest::exchange(name);
        let sources = self.reverse.lock().remove(&dest).unwrap_or_default();
        for s in sources {
            if let Some(src) = self.exchange(&s) {
                src.unbind_dest(&dest);
            }
        }
        for b in ex.bindings() {
            if let Some(set) = self.reverse.lock().get_mut(&b.dest) {
                set.remove(name);
            }
        }
        if let Some(d) = self.delayed.lock().remove(name) {
            d.stop();
        }
        self.dedup.lock().remove(name);
    }

    /// Drop a queue another node deleted; returns the local handle (a cluster router).
    pub(crate) fn forget_queue(&self, name: &str) -> Option<QueueRef> {
        let q = self.queues.write().remove(name)?;
        let dest = Dest::queue(name);
        let sources = self.reverse.lock().remove(&dest).unwrap_or_default();
        for s in sources {
            if let Some(src) = self.exchange(&s) {
                src.unbind_dest(&dest);
            }
        }
        Some(q)
    }

    /// Remove a binding another node deleted.
    pub(crate) fn forget_binding_local(&self, b: &Binding) {
        if let Some(src) = self.exchange(&b.source)
            && src.unbind(&b.dest, &b.routing_key, &b.args)
            && !src.bindings().iter().any(|x| x.dest == b.dest)
            && let Some(set) = self.reverse.lock().get_mut(&b.dest)
        {
            set.remove(&src.name);
        }
    }
}

impl VHost {
    /// Where a queue's actor runs: here (the actor itself) or on another cluster node.
    pub fn locate_queue(&self, name: &str) -> Option<crate::cluster::Located> {
        let q = self.queue(name)?;
        let placed = self.broker().placement().and_then(|p| p.locate(&self.name, name));
        Some(placed.unwrap_or(crate::cluster::Located::Local(q)))
    }
}

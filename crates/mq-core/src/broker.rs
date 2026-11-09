//! The broker: vhosts, users, permissions, policies, parameters and the connection registry,
//! persisted to the metadata store and recovered on start.

use crate::auth::{Access, Permission, RegexCache, TopicPermission, User, expand_vars};
use crate::error::{ACCESS_REFUSED, AmqpError, INTERNAL_ERROR, NOT_ALLOWED, Result};
use crate::exchange::{Binding, Dest, DestKind, Exchange, ExchangeKind};
use crate::log::LogStore;
use crate::meta::{self, MetaStore, kind};
use crate::policy::{self, Policy};
use crate::queue::{ChannelId, ConnId, QueueMeta};
use crate::value::FieldTable;
use crate::vhost::{VHost, VHostInfo};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Notify, broadcast};
use tracing::{info, warn};

/// The RabbitMQ version we present (clients and tools gate features on it).
pub const RABBITMQ_VERSION: &str = "4.1.0";
pub const PRODUCT: &str = "RabbitMQ";

#[derive(Clone, Debug)]
pub struct BrokerConfig {
    pub node_name: String,
    pub cluster_name: String,
    pub default_user: String,
    pub default_pass: String,
    pub default_vhost: String,
    pub default_queue_type: String,
    pub frame_max: u32,
    pub channel_max: u16,
    pub heartbeat: u16,
    /// Largest message body accepted (RabbitMQ 4 defaults to 16 MiB).
    pub max_message_size: u64,
    /// Users that may only connect from loopback (RabbitMQ restricts `guest`).
    pub loopback_users: Vec<String>,
    /// Availability zone of this node (AZ-aware placement and routing).
    pub az: Option<String>,
    /// Unload vhosts idle for this long (0 = never). Scale-to-zero.
    pub vhost_idle_suspend_ms: u64,
    /// Publish budget of each vhost without its own `max-publish-rate` limit, in messages per
    /// second (0 = unlimited). Keeps one tenant from taking a shared broker from the others.
    pub vhost_publish_rate: u64,
    /// The same in message bytes per second (`max-publish-bytes-rate`).
    pub vhost_publish_bytes_rate: u64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        let host = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "localhost".into());
        BrokerConfig {
            node_name: format!("rabbit@{host}"),
            cluster_name: "roll".into(),
            default_user: "guest".into(),
            default_pass: "guest".into(),
            default_vhost: "/".into(),
            default_queue_type: "classic".into(),
            frame_max: 131_072,
            channel_max: 2047,
            heartbeat: 60,
            max_message_size: 16 << 20,
            loopback_users: vec![],
            az: None,
            vhost_idle_suspend_ms: 0,
            vhost_publish_rate: 0,
            vhost_publish_bytes_rate: 0,
        }
    }
}

/// Queues of a waking vhost that open their logs at once.
const RECOVER_PARALLEL: usize = 32;

/// Count (and restore) one recovered queue.
fn recovered(vh: &Arc<VHost>, r: Option<std::result::Result<(Result<crate::queue::QueueRef>, String), tokio::task::JoinError>>) -> usize {
    match r {
        Some(Ok((Ok(q), _))) => {
            vh.restore_queue(q);
            1
        }
        Some(Ok((Err(e), queue))) => {
            warn!(vhost = %vh.name, %queue, error = %e, "failed to recover queue");
            0
        }
        Some(Err(e)) => {
            warn!(vhost = %vh.name, error = %e, "queue recovery panicked");
            0
        }
        None => 0,
    }
}

/// Broker-wide events for the control plane (shovels, federation, cluster sync, dashboards).
#[derive(Clone, Debug)]
pub enum Event {
    PoliciesChanged { vhost: String },
    ParameterChanged { component: String, vhost: String, name: String },
    QueueDeleted { vhost: String, name: String },
    VHostDeleted { vhost: String },
    VHostCreated { vhost: String },
    /// Another node changed a key of a kind the broker doesn't own (webhooks); its owner
    /// reconciles against the store.
    MetaChanged { kind: String, key: String },
}

/// A live client connection, for the management API and limits.
pub struct ConnectionInfo {
    pub id: ConnId,
    pub name: String,
    pub protocol: String,
    pub vhost: String,
    pub user: String,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub client_properties: Value,
    pub connected_at_ms: u64,
    pub tls: bool,
    pub auth_mechanism: String,
    pub frame_max: AtomicU32,
    pub channel_max: AtomicU32,
    pub heartbeat: AtomicU32,
    pub recv_oct: AtomicU64,
    pub send_oct: AtomicU64,
    pub recv_cnt: AtomicU64,
    pub send_cnt: AtomicU64,
    pub channels: AtomicU64,
    pub blocked: AtomicBool,
    /// Set to ask the connection to close itself (management API, draining).
    pub close_reason: Mutex<Option<String>>,
    pub close: Notify,
}

impl ConnectionInfo {
    pub fn request_close(&self, reason: impl Into<String>) {
        *self.close_reason.lock() = Some(reason.into());
        self.close.notify_waiters();
        self.close.notify_one();
    }

    pub fn to_json(&self, node: &str) -> Value {
        let props = &self.client_properties;
        json!({
            "name": self.name,
            "node": node,
            "type": "network",
            "protocol": self.protocol,
            "vhost": self.vhost,
            "user": self.user,
            "user_who_performed_action": self.user,
            "host": self.local.ip().to_string(),
            "port": self.local.port(),
            "peer_host": self.peer.ip().to_string(),
            "peer_port": self.peer.port(),
            "ssl": self.tls,
            "auth_mechanism": self.auth_mechanism,
            "state": if self.blocked.load(Ordering::Relaxed) { "blocked" } else { "running" },
            "channels": self.channels.load(Ordering::Relaxed),
            "channel_max": self.channel_max.load(Ordering::Relaxed),
            "frame_max": self.frame_max.load(Ordering::Relaxed),
            "timeout": self.heartbeat.load(Ordering::Relaxed),
            "recv_oct": self.recv_oct.load(Ordering::Relaxed),
            "send_oct": self.send_oct.load(Ordering::Relaxed),
            "recv_cnt": self.recv_cnt.load(Ordering::Relaxed),
            "send_cnt": self.send_cnt.load(Ordering::Relaxed),
            "connected_at": self.connected_at_ms,
            "client_properties": props,
            "id": self.id,
        })
    }
}

/// A live channel (AMQP channel, MQTT/STOMP session, …).
pub struct ChannelInfo {
    pub id: ChannelId,
    pub conn: ConnId,
    pub number: u16,
    pub conn_name: String,
    pub vhost: String,
    pub user: String,
    pub prefetch_count: AtomicU32,
    pub global_prefetch_count: AtomicU32,
    pub unacked: AtomicU64,
    pub consumers: AtomicU64,
    pub confirm: AtomicBool,
    pub transactional: AtomicBool,
    pub published: AtomicU64,
    pub delivered: AtomicU64,
    pub acked: AtomicU64,
    pub confirmed: AtomicU64,
    pub returned: AtomicU64,
    pub redelivered: AtomicU64,
}

impl ChannelInfo {
    pub fn to_json(&self, node: &str) -> Value {
        json!({
            "name": format!("{} ({})", self.conn_name, self.number),
            "node": node,
            "number": self.number,
            "vhost": self.vhost,
            "user": self.user,
            "connection_details": {"name": self.conn_name},
            "prefetch_count": self.prefetch_count.load(Ordering::Relaxed),
            "global_prefetch_count": self.global_prefetch_count.load(Ordering::Relaxed),
            "messages_unacknowledged": self.unacked.load(Ordering::Relaxed),
            "messages_unconfirmed": 0,
            "messages_uncommitted": 0,
            "acks_uncommitted": 0,
            "consumer_count": self.consumers.load(Ordering::Relaxed),
            "confirm": self.confirm.load(Ordering::Relaxed),
            "transactional": self.transactional.load(Ordering::Relaxed),
            "state": "running",
            "message_stats": {
                "publish": self.published.load(Ordering::Relaxed),
                "deliver_get": self.delivered.load(Ordering::Relaxed),
                "ack": self.acked.load(Ordering::Relaxed),
                "confirm": self.confirmed.load(Ordering::Relaxed),
                "return_unroutable": self.returned.load(Ordering::Relaxed),
                "redeliver": self.redelivered.load(Ordering::Relaxed),
            },
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Listener {
    pub protocol: String,
    pub port: u16,
    pub ip_address: String,
}

pub struct Broker {
    pub config: BrokerConfig,
    pub meta: Arc<dyn MetaStore>,
    pub logs: Arc<dyn LogStore>,
    vhosts: RwLock<HashMap<String, Arc<VHost>>>,
    /// Known vhosts that are currently unloaded (scaled to zero); loaded on first use.
    asleep: RwLock<HashMap<String, VHostInfo>>,
    /// One lock per vhost name: waking (or suspending) one vhost never waits for another's.
    wake_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    users: RwLock<HashMap<String, User>>,
    permissions: RwLock<HashMap<(String, String), Permission>>,
    topic_permissions: RwLock<HashMap<(String, String), Vec<TopicPermission>>>,
    pub policies: RwLock<Vec<Policy>>,
    pub operator_policies: RwLock<Vec<Policy>>,
    parameters: RwLock<BTreeMap<(String, String, String), Value>>,
    global_parameters: RwLock<BTreeMap<String, Value>>,
    pub regex: RegexCache,
    connections: RwLock<HashMap<ConnId, Arc<ConnectionInfo>>>,
    channels: RwLock<HashMap<ChannelId, Arc<ChannelInfo>>>,
    next_id: AtomicU64,
    pub started_ms: u64,
    pub listeners: RwLock<Vec<Listener>>,
    pub events: broadcast::Sender<Event>,
    /// Memory/disk alarm: publishers are blocked while set (either of the two below, or set
    /// directly).
    pub alarm: AtomicBool,
    pub alarm_cleared: Notify,
    pub mem_alarm: AtomicBool,
    pub disk_alarm: AtomicBool,
    /// Draining for shutdown or a rolling upgrade: refuse new connections.
    pub draining: AtomicBool,
    /// Signalled when draining starts.
    pub drain_started: Notify,
    /// Where queues run in a cluster (`None`: all local).
    placement: RwLock<Option<Arc<dyn crate::cluster::Placement>>>,
    /// Background loops park on this while the broker is quiet (see [`Broker::is_quiet`]).
    activity: Arc<Notify>,
    /// Live [`AwakeGuard`]s.
    awake: AtomicU64,
    /// Unix ms of the last request that holds no connection (HTTP APIs).
    last_request_ms: AtomicU64,
}

/// Requests (the HTTP APIs) keep the broker out of quiet mode for this long.
pub const QUIET_AFTER_MS: u64 = 10_000;

/// Keeps the broker out of quiet mode while held: for work that moves messages without a
/// client connection, such as shovels, federation links and webhooks.
pub struct AwakeGuard {
    broker: Weak<Broker>,
}

impl Drop for AwakeGuard {
    fn drop(&mut self) {
        if let Some(b) = self.broker.upgrade() {
            b.awake.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        // Parked loops see the broker gone and exit.
        self.activity.notify_waiters();
    }
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker").field("node", &self.config.node_name).finish()
    }
}

fn perm_key(vhost: &str, user: &str) -> String {
    meta::key(&[vhost, user])
}

impl Broker {
    /// Open the broker: load definitions from the metadata store and recover every queue.
    pub async fn start(config: BrokerConfig, meta: Arc<dyn MetaStore>, logs: Arc<dyn LogStore>) -> anyhow::Result<Arc<Self>> {
        Self::start_with_placement(config, meta, logs, None).await
    }

    /// Start as a cluster member: queues (including recovered ones) go through `placement`.
    pub async fn start_with_placement(
        config: BrokerConfig,
        meta: Arc<dyn MetaStore>,
        logs: Arc<dyn LogStore>,
        placement: Option<Arc<dyn crate::cluster::Placement>>,
    ) -> anyhow::Result<Arc<Self>> {
        let broker = Arc::new(Broker {
            config,
            meta,
            logs,
            vhosts: RwLock::default(),
            asleep: RwLock::default(),
            wake_locks: Mutex::default(),
            users: RwLock::default(),
            permissions: RwLock::default(),
            topic_permissions: RwLock::default(),
            policies: RwLock::default(),
            operator_policies: RwLock::default(),
            parameters: RwLock::default(),
            global_parameters: RwLock::default(),
            regex: RegexCache::default(),
            connections: RwLock::default(),
            channels: RwLock::default(),
            next_id: AtomicU64::new(1),
            started_ms: crate::now_ms(),
            listeners: RwLock::default(),
            events: broadcast::channel(1024).0,
            alarm: AtomicBool::new(false),
            alarm_cleared: Notify::new(),
            mem_alarm: AtomicBool::new(false),
            disk_alarm: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            drain_started: Notify::new(),
            placement: RwLock::new(placement),
            activity: Arc::new(Notify::new()),
            awake: AtomicU64::new(0),
            last_request_ms: AtomicU64::new(0),
        });
        broker.load().await?;
        Ok(broker)
    }

    /// Quiet: no client connections, no request in the last [`QUIET_AFTER_MS`] and no
    /// [`AwakeGuard`]. Nothing can publish or consume then, so background loops (samplers,
    /// metering, alarms) stop until [`Broker::wait_while_quiet`] returns: an idle broker uses no
    /// CPU at all.
    pub fn is_quiet(&self) -> bool {
        self.awake.load(Ordering::Acquire) == 0
            && self.connections.read().is_empty()
            && crate::now_ms().saturating_sub(self.last_request_ms.load(Ordering::Relaxed)) >= QUIET_AFTER_MS
    }

    /// Note a request that holds no connection (HTTP API, dashboard, management API).
    pub fn note_request(&self) {
        let now = crate::now_ms();
        if now.saturating_sub(self.last_request_ms.swap(now, Ordering::Relaxed)) >= QUIET_AFTER_MS {
            self.activity.notify_waiters();
        }
    }

    /// Hold the broker out of quiet mode until the guard drops.
    pub fn awake_guard(self: &Arc<Self>) -> AwakeGuard {
        if self.awake.fetch_add(1, Ordering::AcqRel) == 0 {
            self.activity.notify_waiters();
        }
        AwakeGuard { broker: Arc::downgrade(self) }
    }

    /// Return once the broker is not quiet (at once if it isn't). False when the broker is
    /// gone. Holds no reference to the broker while waiting.
    pub async fn wait_while_quiet(broker: &Weak<Broker>) -> bool {
        loop {
            let Some(b) = broker.upgrade() else { return false };
            let activity = b.activity.clone();
            let woken = activity.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if !b.is_quiet() {
                return true;
            }
            drop(b);
            woken.await;
        }
    }

    /// Raise or clear the memory or disk alarm. Publishers block while either is raised.
    pub fn set_alarm(&self, disk: bool, on: bool) {
        let flag = if disk { &self.disk_alarm } else { &self.mem_alarm };
        if flag.swap(on, Ordering::AcqRel) == on {
            return;
        }
        let any = self.mem_alarm.load(Ordering::Acquire) || self.disk_alarm.load(Ordering::Acquire);
        self.alarm.store(any, Ordering::Release);
        if !any {
            self.alarm_cleared.notify_waiters();
        }
    }

    async fn load(self: &Arc<Self>) -> anyhow::Result<()> {
        let m = &self.meta;
        for (_, v) in m.list(kind::USER).await? {
            let u: User = serde_json::from_value(v)?;
            self.users.write().insert(u.name.clone(), u);
        }
        for (k, v) in m.list(kind::PERMISSION).await? {
            let parts = meta::split_key(&k);
            if parts.len() == 2 {
                self.permissions.write().insert((parts[0].into(), parts[1].into()), serde_json::from_value(v)?);
            }
        }
        for (k, v) in m.list(kind::TOPIC_PERMISSION).await? {
            let parts = meta::split_key(&k);
            if parts.len() == 2 {
                self.topic_permissions.write().insert((parts[0].into(), parts[1].into()), serde_json::from_value(v)?);
            }
        }
        for (_, v) in m.list(kind::POLICY).await? {
            self.policies.write().push(serde_json::from_value(v)?);
        }
        for (_, v) in m.list(kind::OPERATOR_POLICY).await? {
            self.operator_policies.write().push(serde_json::from_value(v)?);
        }
        for (k, v) in m.list(kind::PARAMETER).await? {
            let p = meta::split_key(&k);
            if p.len() == 3 {
                self.parameters.write().insert((p[0].into(), p[1].into(), p[2].into()), v);
            }
        }
        for (k, v) in m.list(kind::GLOBAL_PARAMETER).await? {
            self.global_parameters.write().insert(k, v);
        }
        let vhosts = m.list(kind::VHOST).await?;
        if vhosts.is_empty() && self.users.read().is_empty() {
            info!(user = %self.config.default_user, vhost = %self.config.default_vhost, "first start: creating default vhost and user");
            let vh = self.config.default_vhost.clone();
            self.create_vhost(VHostInfo { name: vh.clone(), description: "Default virtual host".into(), ..Default::default() }).await?;
            let (u, p) = (self.config.default_user.clone(), self.config.default_pass.clone());
            self.put_user(User::new(&u, &p, vec!["administrator".into()])).await?;
            self.set_permission(&vh, &u, Permission { configure: ".*".into(), write: ".*".into(), read: ".*".into() }).await?;
        } else {
            for (_, v) in vhosts {
                let info: VHostInfo = serde_json::from_value(v)?;
                if self.config.vhost_idle_suspend_ms > 0 {
                    // Scale-to-zero: vhosts load on first use.
                    self.asleep.write().insert(info.name.clone(), info);
                } else {
                    let name = info.name.clone();
                    let vh = VHost::new(Arc::downgrade(self), info);
                    self.recover_vhost(&vh).await?;
                    self.vhosts.write().insert(name, vh);
                }
            }
        }
        Ok(())
    }

    /// Recreate a vhost's exchanges, queues (replaying their logs) and bindings.
    async fn recover_vhost(&self, vh: &Arc<VHost>) -> anyhow::Result<()> {
        let m = &self.meta;
        let mut delayed = Vec::new();
        for (_, v) in m.list_vhost(kind::EXCHANGE, &vh.name).await? {
            let args = FieldTable::from_json(&v["arguments"]);
            let name = v["name"].as_str().unwrap_or_default();
            let kind_name = v["type"].as_str().unwrap_or("direct");
            let Ok(kind) = ExchangeKind::parse(kind_name, &args) else { continue };
            if matches!(kind, ExchangeKind::Delayed(_)) {
                delayed.push(name.to_string());
            }
            vh.restore_exchange(Exchange::new(
                name,
                kind,
                true,
                v["auto_delete"].as_bool().unwrap_or(false),
                v["internal"].as_bool().unwrap_or(false),
                args,
            ));
        }
        for name in delayed {
            vh.restore_delayed(&name).await?;
        }
        // Queues open their logs in parallel: waking a vhost costs about one log open, not one
        // per queue.
        let mut n = 0;
        let mut starting = tokio::task::JoinSet::new();
        for (_, v) in m.list_vhost(kind::QUEUE, &vh.name).await? {
            let meta = QueueMeta {
                name: v["name"].as_str().unwrap_or_default().into(),
                vhost: vh.name.clone(),
                durable: true,
                auto_delete: v["auto_delete"].as_bool().unwrap_or(false),
                exclusive_owner: None,
                arguments: FieldTable::from_json(&v["arguments"]),
                created_at: v["created_at"].as_i64().unwrap_or(0),
            };
            let generation = v["generation"].as_str().unwrap_or("0").to_string();
            if starting.len() >= RECOVER_PARALLEL {
                n += recovered(vh, starting.join_next().await);
            }
            let vh = vh.clone();
            starting.spawn(async move { (vh.start_queue(meta.clone(), &generation).await, meta.name) });
        }
        while let Some(r) = starting.join_next().await {
            n += recovered(vh, Some(r));
        }
        for (_, v) in m.list_vhost(kind::BINDING, &vh.name).await? {
            let dest_name = v["destination"].as_str().unwrap_or_default();
            let dest = match v["destination_type"].as_str() {
                Some("exchange") => Dest::exchange(dest_name),
                _ => Dest::queue(dest_name),
            };
            vh.restore_binding(Binding {
                source: v["source"].as_str().unwrap_or_default().into(),
                dest,
                routing_key: v["routing_key"].as_str().unwrap_or_default().into(),
                args: FieldTable::from_json(&v["arguments"]),
            });
        }
        info!(vhost = %vh.name, queues = n, "vhost recovered");
        Ok(())
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    // ── vhosts ──

    /// A loaded vhost (doesn't wake sleeping ones).
    pub fn vhost(&self, name: &str) -> Option<Arc<VHost>> {
        self.vhosts.read().get(name).cloned()
    }

    async fn wake_lock(&self, name: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.wake_locks.lock().entry(name.to_string()).or_default().clone();
        lock.lock_owned().await
    }

    /// A vhost, waking it if it was scaled to zero.
    pub async fn open_vhost(self: &Arc<Self>, name: &str) -> Option<Arc<VHost>> {
        if let Some(v) = self.vhost(name) {
            return Some(v);
        }
        let _g = self.wake_lock(name).await;
        if let Some(v) = self.vhost(name) {
            return Some(v);
        }
        let info = self.asleep.read().get(name).cloned()?;
        let t = std::time::Instant::now();
        let vh = VHost::new(Arc::downgrade(self), info);
        if let Err(e) = self.recover_vhost(&vh).await {
            warn!(vhost = name, error = %e, "failed to wake vhost");
            return None;
        }
        vh.touch();
        self.asleep.write().remove(name);
        self.vhosts.write().insert(name.to_string(), vh.clone());
        info!(vhost = name, took_ms = t.elapsed().as_millis() as u64, "vhost woke up");
        Some(vh)
    }

    /// Unload a vhost (scale to zero). Its queues flush and stop; state stays in the log.
    pub async fn suspend_vhost(&self, name: &str) -> bool {
        let Some(vh) = self.vhost(name) else { return false };
        if vh.stats.connections.load(Ordering::Relaxed) > 0 || !vh.can_sleep() {
            return false;
        }
        let _g = self.wake_lock(name).await;
        self.vhosts.write().remove(name);
        vh.stop().await;
        self.asleep.write().insert(name.to_string(), vh.info.read().clone());
        info!(vhost = name, "vhost scaled to zero");
        true
    }

    pub fn is_asleep(&self, name: &str) -> bool {
        self.asleep.read().contains_key(name)
    }

    /// Every vhost, loaded or not.
    pub fn vhost_infos(&self) -> Vec<(VHostInfo, Option<Arc<VHost>>)> {
        let mut v: Vec<_> = self.vhosts.read().values().map(|vh| (vh.info.read().clone(), Some(vh.clone()))).collect();
        v.extend(self.asleep.read().values().map(|i| (i.clone(), None)));
        v.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        v
    }

    pub fn vhosts(&self) -> Vec<Arc<VHost>> {
        let mut v: Vec<_> = self.vhosts.read().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn vhost_exists(&self, name: &str) -> bool {
        self.vhosts.read().contains_key(name) || self.asleep.read().contains_key(name)
    }

    pub async fn create_vhost(self: &Arc<Self>, info: VHostInfo) -> anyhow::Result<Arc<VHost>> {
        if let Some(vh) = self.open_vhost(&info.name).await {
            // Updating metadata of an existing vhost.
            let limits = vh.info.read().limits.clone();
            let updated = VHostInfo { limits, ..info.clone() };
            *vh.info.write() = updated.clone();
            vh.refresh_limits();
            self.meta.put(kind::VHOST, &info.name, &serde_json::to_value(&updated)?).await?;
            return Ok(vh);
        }
        let vh = VHost::new(Arc::downgrade(self), info.clone());
        self.meta.put(kind::VHOST, &info.name, &serde_json::to_value(&info)?).await?;
        self.vhosts.write().insert(info.name.clone(), vh.clone());
        self.emit(Event::VHostCreated { vhost: info.name.clone() });
        Ok(vh)
    }

    pub async fn delete_vhost(self: &Arc<Self>, name: &str) -> Result<()> {
        let vh = self.open_vhost(name).await.ok_or_else(|| AmqpError::not_found(format!("vhost '{name}'")))?;
        for c in self.connections().into_iter().filter(|c| c.vhost == name) {
            c.request_close("vhost deleted");
        }
        vh.destroy().await;
        self.vhosts.write().remove(name);
        let m = &self.meta;
        let prefix = format!("{name}\u{1f}");
        for k in [kind::EXCHANGE, kind::QUEUE, kind::BINDING, kind::PERMISSION, kind::TOPIC_PERMISSION, kind::POLICY, kind::OPERATOR_POLICY] {
            for (key, _) in m.list(k).await? {
                if key.starts_with(&prefix) {
                    m.delete(k, &key).await?;
                }
            }
        }
        for (key, _) in m.list(kind::PARAMETER).await? {
            if meta::split_key(&key).get(1) == Some(&name) {
                m.delete(kind::PARAMETER, &key).await?;
            }
        }
        m.delete(kind::VHOST, name).await?;
        self.permissions.write().retain(|(v, _), _| v != name);
        self.topic_permissions.write().retain(|(v, _), _| v != name);
        self.policies.write().retain(|p| p.vhost != name);
        self.operator_policies.write().retain(|p| p.vhost != name);
        self.parameters.write().retain(|(_, v, _), _| v != name);
        self.emit(Event::VHostDeleted { vhost: name.into() });
        Ok(())
    }

    pub async fn set_vhost_limits(self: &Arc<Self>, name: &str, limits: HashMap<String, i64>) -> Result<()> {
        let vh = self.open_vhost(name).await.ok_or_else(|| AmqpError::not_found(format!("vhost '{name}'")))?;
        vh.info.write().limits = limits;
        vh.refresh_limits();
        let v = serde_json::to_value(&*vh.info.read()).map_err(AmqpError::internal)?;
        self.meta.put(kind::VHOST, name, &v).await?;
        Ok(())
    }

    // ── users & permissions ──

    pub fn user(&self, name: &str) -> Option<User> {
        self.users.read().get(name).cloned()
    }

    pub fn users(&self) -> Vec<User> {
        let mut v: Vec<_> = self.users.read().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub async fn put_user(&self, user: User) -> anyhow::Result<()> {
        self.meta.put(kind::USER, &user.name, &serde_json::to_value(&user)?).await?;
        self.users.write().insert(user.name.clone(), user);
        Ok(())
    }

    pub async fn delete_user(&self, name: &str) -> anyhow::Result<bool> {
        let existed = self.users.write().remove(name).is_some();
        if existed {
            self.meta.delete(kind::USER, name).await?;
            let perms: Vec<(String, String)> = self.permissions.read().keys().filter(|(_, u)| u == name).cloned().collect();
            for (vh, u) in perms {
                self.clear_permission(&vh, &u).await?;
            }
            let tps: Vec<(String, String)> = self.topic_permissions.read().keys().filter(|(_, u)| u == name).cloned().collect();
            for (vh, u) in tps {
                self.topic_permissions.write().remove(&(vh.clone(), u.clone()));
                self.meta.delete(kind::TOPIC_PERMISSION, &perm_key(&vh, &u)).await?;
            }
            for c in self.connections().into_iter().filter(|c| c.user == name) {
                c.request_close("user deleted");
            }
        }
        Ok(existed)
    }

    /// Check credentials (PLAIN/AMQPLAIN). Errors are ACCESS_REFUSED, like RabbitMQ.
    pub fn authenticate(&self, username: &str, password: &str, peer: Option<SocketAddr>) -> Result<User> {
        let refuse = || AmqpError::new(ACCESS_REFUSED, "Login was refused using authentication mechanism PLAIN. For details see the broker logfile.");
        let user = self.user(username).ok_or_else(refuse)?;
        if user.password_hash.is_empty() || !user.check_password(password) {
            return Err(refuse());
        }
        if let Some(p) = peer
            && !p.ip().is_loopback()
            && self.config.loopback_users.iter().any(|u| u == username)
        {
            return Err(AmqpError::new(ACCESS_REFUSED, format!("user '{username}' can only connect via localhost")));
        }
        Ok(user)
    }

    pub fn permission(&self, vhost: &str, user: &str) -> Option<Permission> {
        self.permissions.read().get(&(vhost.to_string(), user.to_string())).cloned()
    }

    pub fn permissions(&self) -> Vec<(String, String, Permission)> {
        let mut v: Vec<_> = self.permissions.read().iter().map(|((vh, u), p)| (vh.clone(), u.clone(), p.clone())).collect();
        v.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        v
    }

    pub async fn set_permission(&self, vhost: &str, user: &str, p: Permission) -> anyhow::Result<()> {
        self.meta.put(kind::PERMISSION, &perm_key(vhost, user), &serde_json::to_value(&p)?).await?;
        self.permissions.write().insert((vhost.into(), user.into()), p);
        Ok(())
    }

    pub async fn clear_permission(&self, vhost: &str, user: &str) -> anyhow::Result<()> {
        self.permissions.write().remove(&(vhost.to_string(), user.to_string()));
        self.meta.delete(kind::PERMISSION, &perm_key(vhost, user)).await?;
        Ok(())
    }

    pub fn topic_permissions(&self) -> Vec<(String, String, TopicPermission)> {
        let mut v = Vec::new();
        for ((vh, u), list) in self.topic_permissions.read().iter() {
            for p in list {
                v.push((vh.clone(), u.clone(), p.clone()));
            }
        }
        v.sort_by(|a, b| (&a.0, &a.1, &a.2.exchange).cmp(&(&b.0, &b.1, &b.2.exchange)));
        v
    }

    pub async fn set_topic_permission(&self, vhost: &str, user: &str, p: TopicPermission) -> anyhow::Result<()> {
        let list = {
            let mut tp = self.topic_permissions.write();
            let list = tp.entry((vhost.into(), user.into())).or_default();
            list.retain(|x| x.exchange != p.exchange);
            list.push(p);
            list.clone()
        };
        self.meta.put(kind::TOPIC_PERMISSION, &perm_key(vhost, user), &serde_json::to_value(&list)?).await?;
        Ok(())
    }

    pub async fn clear_topic_permission(&self, vhost: &str, user: &str, exchange: Option<&str>) -> anyhow::Result<()> {
        let list = {
            let mut tp = self.topic_permissions.write();
            let key = (vhost.to_string(), user.to_string());
            match exchange {
                Some(x) => {
                    if let Some(l) = tp.get_mut(&key) {
                        l.retain(|p| p.exchange != x);
                    }
                }
                None => {
                    tp.remove(&key);
                }
            }
            tp.get(&key).cloned().unwrap_or_default()
        };
        if list.is_empty() {
            self.meta.delete(kind::TOPIC_PERMISSION, &perm_key(vhost, user)).await?;
        } else {
            self.meta.put(kind::TOPIC_PERMISSION, &perm_key(vhost, user), &serde_json::to_value(&list)?).await?;
        }
        Ok(())
    }

    /// Connection-level vhost access check (NOT_ALLOWED on failure, like `connection.open`).
    pub fn check_vhost_access(&self, user: &User, vhost: &str) -> Result<()> {
        if !self.vhost_exists(vhost) || self.permission(vhost, &user.name).is_none() {
            return Err(AmqpError::new(NOT_ALLOWED, format!("access to vhost '{vhost}' refused for user '{}'", user.name)));
        }
        Ok(())
    }

    /// Resource permission check (`configure`/`write`/`read` regexes).
    pub fn check(&self, user: &User, vhost: &str, access: Access, kind: &str, name: &str) -> Result<()> {
        let ok = self.permission(vhost, &user.name).is_some_and(|p| {
            let pattern = match access {
                Access::Configure => &p.configure,
                Access::Write => &p.write,
                Access::Read => &p.read,
            };
            self.regex.matches(pattern, name)
        });
        if ok {
            Ok(())
        } else {
            Err(AmqpError::access_refused(format!(
                "{} access to {kind} '{name}' in vhost '{vhost}' refused for user '{}'",
                access.as_str(),
                user.name
            )))
        }
    }

    /// Topic permission check for topic exchanges (no topic permissions = allowed).
    pub fn check_topic(&self, user: &User, vhost: &str, exchange: &str, routing_key: &str, access: Access, client_id: Option<&str>) -> Result<()> {
        let tp = self.topic_permissions.read();
        let Some(list) = tp.get(&(vhost.to_string(), user.name.clone())) else { return Ok(()) };
        let Some(p) = list.iter().find(|p| p.exchange == exchange) else { return Ok(()) };
        let pattern = match access {
            Access::Write => &p.write,
            _ => &p.read,
        };
        let pattern = expand_vars(pattern, &user.name, vhost, client_id);
        if self.regex.matches(&pattern, routing_key) {
            Ok(())
        } else {
            Err(AmqpError::access_refused(format!(
                "{} access to topic '{routing_key}' in exchange '{exchange}' in vhost '{vhost}' refused for user '{}'",
                access.as_str(),
                user.name
            )))
        }
    }

    // ── policies & parameters ──

    pub async fn put_policy(&self, p: Policy, operator: bool) -> Result<()> {
        policy::validate_definition(&p.definition, operator).map_err(AmqpError::precondition)?;
        regex::Regex::new(&p.pattern).map_err(|e| AmqpError::precondition(format!("invalid pattern: {e}")))?;
        let k = if operator { kind::OPERATOR_POLICY } else { kind::POLICY };
        self.meta.put(k, &meta::key(&[&p.vhost, &p.name]), &serde_json::to_value(&p).map_err(AmqpError::internal)?).await?;
        {
            let list = if operator { &self.operator_policies } else { &self.policies };
            let mut l = list.write();
            l.retain(|x| !(x.vhost == p.vhost && x.name == p.name));
            l.push(p.clone());
        }
        self.policies_changed(&p.vhost).await;
        Ok(())
    }

    pub async fn delete_policy(&self, vhost: &str, name: &str, operator: bool) -> Result<bool> {
        let list = if operator { &self.operator_policies } else { &self.policies };
        let existed = {
            let mut l = list.write();
            let before = l.len();
            l.retain(|x| !(x.vhost == vhost && x.name == name));
            before != l.len()
        };
        if existed {
            let k = if operator { kind::OPERATOR_POLICY } else { kind::POLICY };
            self.meta.delete(k, &meta::key(&[vhost, name])).await?;
            self.policies_changed(vhost).await;
        }
        Ok(existed)
    }

    async fn policies_changed(&self, vhost: &str) {
        if let Some(vh) = self.vhost(vhost) {
            vh.apply_policies().await;
        }
        self.emit(Event::PoliciesChanged { vhost: vhost.into() });
    }

    pub fn parameter(&self, component: &str, vhost: &str, name: &str) -> Option<Value> {
        self.parameters.read().get(&(component.into(), vhost.into(), name.into())).cloned()
    }

    pub fn parameters(&self, component: Option<&str>, vhost: Option<&str>) -> Vec<(String, String, String, Value)> {
        self.parameters
            .read()
            .iter()
            .filter(|((c, v, _), _)| component.is_none_or(|x| x == c) && vhost.is_none_or(|x| x == v))
            .map(|((c, v, n), val)| (c.clone(), v.clone(), n.clone(), val.clone()))
            .collect()
    }

    pub async fn put_parameter(&self, component: &str, vhost: &str, name: &str, value: Value) -> anyhow::Result<()> {
        self.meta.put(kind::PARAMETER, &meta::key(&[component, vhost, name]), &value).await?;
        self.parameters.write().insert((component.into(), vhost.into(), name.into()), value);
        self.emit(Event::ParameterChanged { component: component.into(), vhost: vhost.into(), name: name.into() });
        Ok(())
    }

    pub async fn delete_parameter(&self, component: &str, vhost: &str, name: &str) -> anyhow::Result<bool> {
        let existed = self.parameters.write().remove(&(component.into(), vhost.into(), name.into())).is_some();
        if existed {
            self.meta.delete(kind::PARAMETER, &meta::key(&[component, vhost, name])).await?;
            self.emit(Event::ParameterChanged { component: component.into(), vhost: vhost.into(), name: name.into() });
        }
        Ok(existed)
    }

    pub fn global_parameters(&self) -> Vec<(String, Value)> {
        self.global_parameters.read().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub async fn put_global_parameter(&self, name: &str, value: Value) -> anyhow::Result<()> {
        self.meta.put(kind::GLOBAL_PARAMETER, name, &value).await?;
        self.global_parameters.write().insert(name.into(), value);
        Ok(())
    }

    pub async fn delete_global_parameter(&self, name: &str) -> anyhow::Result<bool> {
        let existed = self.global_parameters.write().remove(name).is_some();
        if existed {
            self.meta.delete(kind::GLOBAL_PARAMETER, name).await?;
        }
        Ok(existed)
    }

    pub(crate) async fn on_queue_deleted(&self, vhost: &str, name: &str) {
        self.emit(Event::QueueDeleted { vhost: vhost.into(), name: name.into() });
    }

    // ── connections ──

    pub fn register_connection(&self, info: Arc<ConnectionInfo>) {
        if let Some(vh) = self.vhost(&info.vhost) {
            vh.stats.connections.fetch_add(1, Ordering::Relaxed);
            vh.touch();
        }
        let first = {
            let mut c = self.connections.write();
            c.insert(info.id, info);
            c.len() == 1
        };
        if first {
            self.activity.notify_waiters();
        }
    }

    /// Drop a connection: its exclusive queues go with it.
    pub async fn connection_closed(&self, id: ConnId) {
        let Some(info) = self.connections.write().remove(&id) else { return };
        self.channels.write().retain(|_, c| c.conn != id);
        if let Some(vh) = self.vhost(&info.vhost) {
            vh.stats.connections.fetch_sub(1, Ordering::Relaxed);
            vh.stats.connection_ms_closed.fetch_add(crate::now_ms().saturating_sub(info.connected_at_ms), Ordering::Relaxed);
            vh.touch();
            vh.connection_closed(id).await;
        }
    }

    pub fn connections(&self) -> Vec<Arc<ConnectionInfo>> {
        let mut v: Vec<_> = self.connections.read().values().cloned().collect();
        v.sort_by_key(|c| c.id);
        v
    }

    pub fn connection(&self, id: ConnId) -> Option<Arc<ConnectionInfo>> {
        self.connections.read().get(&id).cloned()
    }

    pub fn connection_by_name(&self, name: &str) -> Option<Arc<ConnectionInfo>> {
        self.connections.read().values().find(|c| c.name == name).cloned()
    }

    pub fn user_connections(&self, user: &str) -> usize {
        self.connections.read().values().filter(|c| c.user == user).count()
    }

    pub fn vhost_connections(&self, vhost: &str) -> usize {
        self.connections.read().values().filter(|c| c.vhost == vhost).count()
    }

    /// Connection limits for `user` on `vhost` (`max-connections`).
    pub fn check_connection_limits(&self, user: &User, vhost: &str) -> Result<()> {
        if let Some(vh) = self.vhost(vhost)
            && let Some(max) = vh.info.read().limits.get("max-connections").copied().filter(|v| *v >= 0)
            && self.vhost_connections(vhost) as i64 >= max
        {
            return Err(AmqpError::new(NOT_ALLOWED, format!("access to vhost '{vhost}' refused for user '{}': connection limit ({max}) is reached", user.name)));
        }
        if let Some(max) = user.limit("max-connections")
            && self.user_connections(&user.name) as i64 >= max
        {
            return Err(AmqpError::new(NOT_ALLOWED, format!("Connection refused for user '{}'. User connection limit ({max}) is reached", user.name)));
        }
        Ok(())
    }

    pub fn register_channel(&self, info: Arc<ChannelInfo>) {
        if let Some(vh) = self.vhost(&info.vhost) {
            vh.stats.channels.fetch_add(1, Ordering::Relaxed);
        }
        self.channels.write().insert(info.id, info);
    }

    pub fn channel_closed(&self, id: ChannelId) {
        if let Some(info) = self.channels.write().remove(&id)
            && let Some(vh) = self.vhost(&info.vhost)
        {
            vh.stats.channels.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn channels(&self) -> Vec<Arc<ChannelInfo>> {
        let mut v: Vec<_> = self.channels.read().values().cloned().collect();
        v.sort_by_key(|c| (c.conn, c.number));
        v
    }

    pub fn user_channels(&self, user: &str) -> usize {
        self.channels.read().values().filter(|c| c.user == user).count()
    }

    /// Stop accepting connections and ask existing ones to close, spread over `window` so
    /// clients don't reconnect all at once (rolling upgrades).
    pub async fn drain(&self, window: std::time::Duration) {
        self.draining.store(true, Ordering::SeqCst);
        self.drain_started.notify_waiters();
        let conns = self.connections();
        if conns.is_empty() {
            return;
        }
        let step = window / conns.len() as u32;
        for c in conns {
            c.request_close("broker is shutting down for an upgrade; reconnect");
            tokio::time::sleep(step).await;
        }
    }

    /// Flush and stop every queue (shutdown).
    pub async fn stop(&self) {
        for vh in self.vhosts() {
            vh.stop().await;
        }
    }

    // ── definitions (RabbitMQ JSON format) ──

    pub async fn export_definitions(self: &Arc<Self>, only_vhost: Option<&str>) -> anyhow::Result<Value> {
        let in_scope = |vh: &str| only_vhost.is_none_or(|x| x == vh);
        let m = &self.meta;
        let mut exchanges = Vec::new();
        for (_, v) in m.list(kind::EXCHANGE).await? {
            if in_scope(v["vhost"].as_str().unwrap_or("")) {
                exchanges.push(v);
            }
        }
        let mut queues = Vec::new();
        for (_, mut v) in m.list(kind::QUEUE).await? {
            if in_scope(v["vhost"].as_str().unwrap_or("")) {
                if let Some(o) = v.as_object_mut() {
                    o.remove("generation");
                    o.remove("type");
                    o.remove("created_at");
                }
                queues.push(v);
            }
        }
        let mut bindings = Vec::new();
        for (_, v) in m.list(kind::BINDING).await? {
            if in_scope(v["vhost"].as_str().unwrap_or("")) {
                bindings.push(v);
            }
        }
        let policies: Vec<Value> = self.policies.read().iter().filter(|p| in_scope(&p.vhost)).map(|p| serde_json::to_value(p).unwrap()).collect();
        let parameters: Vec<Value> = self
            .parameters(None, None)
            .into_iter()
            .filter(|(_, v, _, _)| in_scope(v))
            .map(|(c, v, n, val)| json!({"component": c, "vhost": v, "name": n, "value": val}))
            .collect();
        let mut out = json!({
            "rabbit_version": RABBITMQ_VERSION,
            "rabbitmq_version": RABBITMQ_VERSION,
            "product_name": PRODUCT,
            "product_version": RABBITMQ_VERSION,
            "policies": policies,
            "queues": queues,
            "exchanges": exchanges,
            "bindings": bindings,
            "parameters": parameters,
        });
        if only_vhost.is_none() {
            out["users"] = Value::Array(
                self.users()
                    .into_iter()
                    .map(|u| json!({"name": u.name, "password_hash": u.password_hash, "hashing_algorithm": u.hashing_algorithm, "tags": u.tags, "limits": u.limits}))
                    .collect(),
            );
            out["vhosts"] = Value::Array(
                self.vhost_infos()
                    .into_iter()
                    .map(|(i, _)| json!({"name": i.name, "description": i.description, "tags": i.tags, "default_queue_type": i.default_queue_type, "metadata": {"description": i.description, "tags": i.tags, "default_queue_type": i.default_queue_type}, "limits": i.limits}))
                    .collect(),
            );
            out["permissions"] = Value::Array(
                self.permissions().into_iter().map(|(vh, u, p)| json!({"user": u, "vhost": vh, "configure": p.configure, "write": p.write, "read": p.read})).collect(),
            );
            out["topic_permissions"] = Value::Array(
                self.topic_permissions()
                    .into_iter()
                    .map(|(vh, u, p)| json!({"user": u, "vhost": vh, "exchange": p.exchange, "write": p.write, "read": p.read}))
                    .collect(),
            );
            out["global_parameters"] = Value::Array(self.global_parameters().into_iter().map(|(n, v)| json!({"name": n, "value": v})).collect());
        }
        Ok(out)
    }

    /// Import definitions exported from RabbitMQ (or roll). Existing objects are kept;
    /// equivalent redeclarations are no-ops. See [`Broker::import_definitions_with`].
    pub async fn import_definitions(self: &Arc<Self>, defs: &Value, only_vhost: Option<&str>) -> Result<ImportReport> {
        self.import_definitions_with(defs, only_vhost, &ImportOptions::default()).await
    }

    /// Import definitions, skipping what roll can't take instead of stopping partway: invalid
    /// or unknown policy settings, patterns the `regex` crate can't compile, objects the broker
    /// rejects (and bindings to them). Classic-mirroring keys (`ha-*`) and
    /// `queue-master-locator` are dropped from policies, which RabbitMQ 4 no longer has. Every
    /// skipped or changed item is in the report. Only metadata-store failures abort.
    pub async fn import_definitions_with(self: &Arc<Self>, defs: &Value, only_vhost: Option<&str>, opts: &ImportOptions) -> Result<ImportReport> {
        let arr = |k: &str| defs.get(k).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let vh_of = |v: &Value| only_vhost.map(String::from).unwrap_or_else(|| v["vhost"].as_str().unwrap_or("/").to_string());
        let ie = |e: anyhow::Error| AmqpError::internal(format!("{e:#}"));
        let mut report = ImportReport::default();
        let invalid_re = |re: &str| regex::Regex::new(re).err().map(|e| format!("pattern '{re}' isn't supported: {}", last_line(&e.to_string())));
        for u in arr("users") {
            let name = u["name"].as_str().unwrap_or_default().to_string();
            if name.is_empty() {
                report.skip("user", None, "", "the user has no name");
                continue;
            }
            let tags = match &u["tags"] {
                Value::Array(a) => a.iter().filter_map(|t| t.as_str().map(String::from)).collect(),
                Value::String(s) => s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
                _ => vec![],
            };
            let user = match u.get("password") {
                Some(Value::String(p)) => User::new(&name, p, tags),
                _ => User {
                    name: name.clone(),
                    password_hash: u["password_hash"].as_str().unwrap_or_default().into(),
                    hashing_algorithm: u["hashing_algorithm"].as_str().unwrap_or("rabbit_password_hashing_sha256").into(),
                    tags,
                    limits: serde_json::from_value(u["limits"].clone()).unwrap_or_default(),
                },
            };
            self.put_user(user).await.map_err(ie)?;
        }
        for v in arr("vhosts") {
            let name = v["name"].as_str().unwrap_or("/").to_string();
            let md = v.get("metadata").cloned().unwrap_or(Value::Null);
            let info = VHostInfo {
                name: name.clone(),
                description: v["description"].as_str().or(md["description"].as_str()).unwrap_or("").into(),
                tags: serde_json::from_value(v.get("tags").cloned().or(md.get("tags").cloned()).unwrap_or(Value::Null)).unwrap_or_default(),
                default_queue_type: v["default_queue_type"].as_str().or(md["default_queue_type"].as_str()).map(String::from),
                tracing: false,
                limits: v.get("limits").and_then(|l| serde_json::from_value(l.clone()).ok()).unwrap_or_default(),
            };
            self.create_vhost(info).await.map_err(ie)?;
        }
        if let Some(vh) = only_vhost
            && !self.vhost_exists(vh)
        {
            self.create_vhost(VHostInfo { name: vh.into(), ..Default::default() }).await.map_err(ie)?;
        }
        for p in arr("permissions") {
            let (vhost, user) = (vh_of(&p), p["user"].as_str().unwrap_or_default().to_string());
            let perm = Permission {
                configure: p["configure"].as_str().unwrap_or("").into(),
                write: p["write"].as_str().unwrap_or("").into(),
                read: p["read"].as_str().unwrap_or("").into(),
            };
            let bad = [&perm.configure, &perm.write, &perm.read].into_iter().find_map(|re| invalid_re(re));
            let reason = if self.user(&user).is_none() { Some(format!("no user '{user}'")) } else { bad };
            if let Some(reason) = reason {
                report.skip("permission", Some(&vhost), &user, reason);
                continue;
            }
            self.set_permission(&vhost, &user, perm).await.map_err(ie)?;
        }
        for p in arr("topic_permissions") {
            let (vhost, user) = (vh_of(&p), p["user"].as_str().unwrap_or_default().to_string());
            let tp = TopicPermission {
                exchange: p["exchange"].as_str().unwrap_or_default().into(),
                write: p["write"].as_str().unwrap_or("").into(),
                read: p["read"].as_str().unwrap_or("").into(),
            };
            let bad = [&tp.write, &tp.read].into_iter().find_map(|re| invalid_re(re));
            let reason = if self.user(&user).is_none() { Some(format!("no user '{user}'")) } else { bad };
            if let Some(reason) = reason {
                report.skip("topic_permission", Some(&vhost), &format!("{user} on {}", tp.exchange), reason);
                continue;
            }
            self.set_topic_permission(&vhost, &user, tp).await.map_err(ie)?;
        }
        for p in arr("global_parameters") {
            self.put_global_parameter(p["name"].as_str().unwrap_or_default(), p["value"].clone()).await.map_err(ie)?;
        }
        for p in arr("parameters") {
            let (component, vhost, name) = (p["component"].as_str().unwrap_or_default(), vh_of(&p), p["name"].as_str().unwrap_or_default());
            if component.is_empty() || name.is_empty() {
                report.skip("parameter", Some(&vhost), &format!("{component}/{name}"), "a parameter needs a component and a name");
                continue;
            }
            if let Some(validate) = opts.validate_parameter
                && let Err(e) = validate(component, &p["value"])
            {
                report.skip("parameter", Some(&vhost), &format!("{component}/{name}"), e);
                continue;
            }
            self.put_parameter(component, &vhost, name, p["value"].clone()).await.map_err(ie)?;
        }
        for p in arr("policies") {
            let mut pol = Policy {
                vhost: vh_of(&p),
                name: p["name"].as_str().unwrap_or_default().into(),
                pattern: p["pattern"].as_str().unwrap_or_default().into(),
                apply_to: p["apply-to"].as_str().unwrap_or("all").into(),
                definition: p["definition"].clone(),
                priority: p["priority"].as_i64().unwrap_or(0),
            };
            let dropped = policy::strip_obsolete(&mut pol.definition);
            let why_dropped = || {
                format!(
                    "{} {} obsolete in RabbitMQ 4 (classic queue mirroring was removed; replicate with quorum queues)",
                    dropped.join(", "),
                    if dropped.len() == 1 { "is" } else { "are" }
                )
            };
            if !dropped.is_empty() && pol.definition.as_object().is_some_and(|o| o.is_empty()) {
                report.skip("policy", Some(&pol.vhost), &pol.name, format!("nothing left to import: {}", why_dropped()));
                continue;
            }
            let invalid = if pol.name.is_empty() {
                Some("the policy has no name".to_string())
            } else {
                policy::validate_definition(&pol.definition, false).err().map(|e| e.replace("Validation failed", "").trim().to_string()).or_else(|| invalid_re(&pol.pattern))
            };
            if let Some(reason) = invalid {
                report.skip("policy", Some(&pol.vhost), &pol.name, reason);
                continue;
            }
            let (vhost, name) = (pol.vhost.clone(), pol.name.clone());
            match self.put_policy(pol, false).await {
                Ok(()) if !dropped.is_empty() => report.partial("policy", Some(&vhost), &name, format!("dropped {}", why_dropped())),
                Ok(()) => {}
                Err(e) if e.code == INTERNAL_ERROR => return Err(e),
                Err(e) => report.skip("policy", Some(&vhost), &name, e.reply_text()),
            }
        }
        for x in arr("exchanges") {
            let name = x["name"].as_str().unwrap_or_default();
            if name.is_empty() || name.starts_with("amq.") {
                continue;
            }
            let vhost = vh_of(&x);
            let Some(vh) = self.open_vhost(&vhost).await else {
                report.skip("exchange", Some(&vhost), name, format!("no vhost '{vhost}'"));
                continue;
            };
            let r = vh
                .declare_exchange(
                    name,
                    x["type"].as_str().unwrap_or("direct"),
                    x["durable"].as_bool().unwrap_or(true),
                    x["auto_delete"].as_bool().unwrap_or(false),
                    x["internal"].as_bool().unwrap_or(false),
                    FieldTable::from_json(&x["arguments"]),
                    false,
                )
                .await;
            match r {
                Ok(_) => {}
                Err(e) if e.code == INTERNAL_ERROR => return Err(e),
                Err(e) => report.skip("exchange", Some(&vhost), name, e.reply_text()),
            }
        }
        for q in arr("queues") {
            let name = q["name"].as_str().unwrap_or_default().to_string();
            let vhost = vh_of(&q);
            let Some(vh) = self.open_vhost(&vhost).await else {
                report.skip("queue", Some(&vhost), &name, format!("no vhost '{vhost}'"));
                continue;
            };
            let r = vh
                .declare_queue(crate::vhost::DeclareQueue {
                    name: name.clone(),
                    durable: q["durable"].as_bool().unwrap_or(true),
                    exclusive_owner: None,
                    auto_delete: q["auto_delete"].as_bool().unwrap_or(false),
                    args: FieldTable::from_json(&q["arguments"]),
                    passive: false,
                })
                .await;
            match r {
                Ok(_) => {}
                Err(e) if e.code == INTERNAL_ERROR => return Err(e),
                Err(e) => report.skip("queue", Some(&vhost), &name, e.reply_text()),
            }
        }
        for b in arr("bindings") {
            let source = b["source"].as_str().unwrap_or_default();
            if source.is_empty() {
                continue;
            }
            let dname = b["destination"].as_str().unwrap_or_default();
            let rk = b["routing_key"].as_str().unwrap_or_default();
            let what = format!("{source} -> {dname} [{rk}]");
            let vhost = vh_of(&b);
            let Some(vh) = self.open_vhost(&vhost).await else {
                report.skip("binding", Some(&vhost), &what, format!("no vhost '{vhost}'"));
                continue;
            };
            let dest = match b["destination_type"].as_str() {
                Some("exchange") => Dest { kind: DestKind::Exchange, name: dname.into() },
                _ => Dest { kind: DestKind::Queue, name: dname.into() },
            };
            match vh.bind(source, dest, rk, FieldTable::from_json(&b["arguments"]), None).await {
                Ok(()) => {}
                Err(e) if e.code == INTERNAL_ERROR => return Err(e),
                Err(e) => report.skip("binding", Some(&vhost), &what, e.reply_text()),
            }
        }
        if !report.skipped.is_empty() {
            warn!(skipped = report.skipped.len(), "definitions imported with items skipped or changed");
        }
        Ok(report)
    }
}

fn last_line(s: &str) -> &str {
    s.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or(s)
}

/// Something [`Broker::import_definitions`] left out or changed, and why.
#[derive(Clone, Debug, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Skipped {
    /// `user`, `permission`, `topic_permission`, `parameter`, `policy`, `exchange`, `queue`,
    /// `binding` (or, from migration, `shovel`).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vhost: Option<String>,
    pub name: String,
    pub reason: String,
    /// The item was imported, without what `reason` describes (e.g. obsolete policy keys).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

/// What an import skipped or changed.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ImportReport {
    pub skipped: Vec<Skipped>,
}

impl ImportReport {
    pub fn skip(&mut self, kind: &str, vhost: Option<&str>, name: &str, reason: impl Into<String>) {
        self.skipped.push(Skipped { kind: kind.into(), vhost: vhost.map(String::from), name: name.into(), reason: reason.into(), partial: false });
    }

    pub fn partial(&mut self, kind: &str, vhost: Option<&str>, name: &str, reason: impl Into<String>) {
        self.skipped.push(Skipped { kind: kind.into(), vhost: vhost.map(String::from), name: name.into(), reason: reason.into(), partial: true });
    }

    /// Items of `kind` left out entirely.
    pub fn count(&self, kind: &str) -> usize {
        self.skipped.iter().filter(|s| s.kind == kind && !s.partial).count()
    }
}

/// Checks a runtime parameter value (`component`, `value`).
pub type ParameterValidator = fn(&str, &Value) -> std::result::Result<(), String>;

/// Options for [`Broker::import_definitions_with`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportOptions {
    /// Checks runtime parameter values (`component`, `value`); invalid ones are skipped.
    pub validate_parameter: Option<ParameterValidator>,
}

// ── cluster ──

impl Broker {
    /// Run queues through a cluster placement (see [`crate::cluster`]). Set before serving.
    pub fn set_placement(&self, p: Arc<dyn crate::cluster::Placement>) {
        *self.placement.write() = Some(p);
    }

    pub fn placement(&self) -> Option<Arc<dyn crate::cluster::Placement>> {
        self.placement.read().clone()
    }

    /// Bring this node's in-memory state in line with the metadata store for one key, after
    /// another node changed it (the cluster change feed). Idempotent and order-insensitive:
    /// it reconciles against the store's current value, not the change itself.
    pub async fn apply_change(self: &Arc<Self>, kind_name: &str, key: &str) -> anyhow::Result<()> {
        let current = self.meta.get(kind_name, key).await?;
        let parts = meta::split_key(key);
        match kind_name {
            kind::VHOST => match current {
                Some(v) => {
                    let info: VHostInfo = serde_json::from_value(v)?;
                    let name = info.name.clone();
                    if let Some(vh) = self.vhost(&name) {
                        *vh.info.write() = info;
                        vh.refresh_limits();
                    } else if self.asleep.read().contains_key(&name) || self.config.vhost_idle_suspend_ms > 0 {
                        self.asleep.write().insert(name, info);
                    } else {
                        let _g = self.wake_lock(&name).await;
                        if self.vhost(&name).is_none() {
                            let vh = VHost::new(Arc::downgrade(self), info);
                            self.recover_vhost(&vh).await?;
                            self.vhosts.write().insert(name.clone(), vh);
                            self.emit(Event::VHostCreated { vhost: name });
                        }
                    }
                }
                None => {
                    self.asleep.write().remove(key);
                    let removed = self.vhosts.write().remove(key);
                    if let Some(vh) = removed {
                        for c in self.connections().into_iter().filter(|c| c.vhost == key) {
                            c.request_close("vhost deleted");
                        }
                        let placement = self.placement();
                        for q in vh.queues() {
                            if let Some(p) = &placement {
                                p.forget(key, &q.meta.name);
                            }
                        }
                        self.permissions.write().retain(|(v, _), _| v != key);
                        self.topic_permissions.write().retain(|(v, _), _| v != key);
                        self.policies.write().retain(|p| p.vhost != key);
                        self.operator_policies.write().retain(|p| p.vhost != key);
                        self.parameters.write().retain(|(_, v, _), _| v != key);
                        self.emit(Event::VHostDeleted { vhost: key.into() });
                    }
                }
            },
            kind::USER => match current {
                Some(v) => {
                    let u: User = serde_json::from_value(v)?;
                    self.users.write().insert(u.name.clone(), u);
                }
                None => {
                    self.users.write().remove(key);
                }
            },
            kind::PERMISSION if parts.len() == 2 => {
                let k = (parts[0].to_string(), parts[1].to_string());
                match current {
                    Some(v) => {
                        self.permissions.write().insert(k, serde_json::from_value(v)?);
                    }
                    None => {
                        self.permissions.write().remove(&k);
                    }
                }
            }
            kind::TOPIC_PERMISSION if parts.len() == 2 => {
                let k = (parts[0].to_string(), parts[1].to_string());
                match current {
                    Some(v) => {
                        self.topic_permissions.write().insert(k, serde_json::from_value(v)?);
                    }
                    None => {
                        self.topic_permissions.write().remove(&k);
                    }
                }
            }
            kind::POLICY | kind::OPERATOR_POLICY if parts.len() == 2 => {
                let list = if kind_name == kind::POLICY { &self.policies } else { &self.operator_policies };
                {
                    let mut l = list.write();
                    l.retain(|p| !(p.vhost == parts[0] && p.name == parts[1]));
                    if let Some(v) = current {
                        l.push(serde_json::from_value(v)?);
                    }
                }
                if let Some(vh) = self.vhost(parts[0]) {
                    vh.apply_policies().await;
                }
                self.emit(Event::PoliciesChanged { vhost: parts[0].into() });
            }
            kind::PARAMETER if parts.len() == 3 => {
                let k = (parts[0].to_string(), parts[1].to_string(), parts[2].to_string());
                match current {
                    Some(v) => {
                        self.parameters.write().insert(k, v);
                    }
                    None => {
                        self.parameters.write().remove(&k);
                    }
                }
                self.emit(Event::ParameterChanged { component: parts[0].into(), vhost: parts[1].into(), name: parts[2].into() });
            }
            kind::GLOBAL_PARAMETER => match current {
                Some(v) => {
                    self.global_parameters.write().insert(key.into(), v);
                }
                None => {
                    self.global_parameters.write().remove(key);
                }
            },
            kind::EXCHANGE if parts.len() == 2 => {
                let Some(vh) = self.vhost(parts[0]) else { return Ok(()) };
                let name = parts[1];
                match current {
                    Some(v) if vh.exchange(name).is_none() => {
                        let args = FieldTable::from_json(&v["arguments"]);
                        let kind = ExchangeKind::parse(v["type"].as_str().unwrap_or("direct"), &args)?;
                        let delayed = matches!(kind, ExchangeKind::Delayed(_));
                        let auto_delete = v["auto_delete"].as_bool().unwrap_or(false);
                        let internal = v["internal"].as_bool().unwrap_or(false);
                        vh.restore_exchange(Exchange::new(name, kind, true, auto_delete, internal, args));
                        if delayed {
                            vh.restore_delayed(name).await?;
                        }
                    }
                    Some(_) => {}
                    None => vh.forget_exchange(name),
                }
            }
            kind::QUEUE if parts.len() == 2 => {
                let Some(vh) = self.vhost(parts[0]) else { return Ok(()) };
                let name = parts[1];
                match current {
                    Some(v) => {
                        let created_at = v["created_at"].as_i64().unwrap_or(0);
                        let existing = vh.queue(name).filter(|q| !q.is_deleted());
                        if existing.as_ref().is_some_and(|q| q.meta.created_at == created_at) {
                            return Ok(());
                        }
                        if existing.is_some() {
                            // Deleted and declared again elsewhere: replace our handle.
                            vh.forget_queue(name);
                            if let Some(p) = self.placement() {
                                p.forget(parts[0], name);
                            }
                        }
                        let meta = QueueMeta {
                            name: name.into(),
                            vhost: vh.name.clone(),
                            durable: true,
                            auto_delete: v["auto_delete"].as_bool().unwrap_or(false),
                            exclusive_owner: None,
                            arguments: FieldTable::from_json(&v["arguments"]),
                            created_at,
                        };
                        let generation = v["generation"].as_str().unwrap_or("0").to_string();
                        let q = vh.start_queue(meta, &generation).await?;
                        vh.restore_queue(q);
                        // Bindings declared before the queue reached us.
                        let prefix = format!("{}\u{1f}", vh.name);
                        for (k, b) in self.meta.list(kind::BINDING).await? {
                            if k.starts_with(&prefix) && b["destination_type"] == "queue" && b["destination"] == name {
                                vh.restore_binding(binding_from_json(&b));
                            }
                        }
                    }
                    None => {
                        if vh.forget_queue(name).is_some() {
                            if let Some(p) = self.placement() {
                                p.forget(parts[0], name);
                            }
                            self.emit(Event::QueueDeleted { vhost: parts[0].into(), name: name.into() });
                        }
                    }
                }
            }
            kind::BINDING if parts.len() == 6 => {
                let Some(vh) = self.vhost(parts[0]) else { return Ok(()) };
                let b = Binding {
                    source: parts[1].into(),
                    dest: if parts[2] == "e" { Dest::exchange(parts[3]) } else { Dest::queue(parts[3]) },
                    routing_key: parts[4].into(),
                    args: serde_json::from_str::<Value>(parts[5]).map(|v| FieldTable::from_json(&v)).unwrap_or_default(),
                };
                match current {
                    Some(_) => vh.restore_binding(b),
                    None => vh.forget_binding_local(&b),
                }
            }
            _ => self.emit(Event::MetaChanged { kind: kind_name.to_string(), key: key.to_string() }),
        }
        Ok(())
    }
}

fn binding_from_json(v: &Value) -> Binding {
    let dest_name = v["destination"].as_str().unwrap_or_default();
    Binding {
        source: v["source"].as_str().unwrap_or_default().into(),
        dest: if v["destination_type"] == "exchange" { Dest::exchange(dest_name) } else { Dest::queue(dest_name) },
        routing_key: v["routing_key"].as_str().unwrap_or_default().into(),
        args: FieldTable::from_json(&v["arguments"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::MemStore;
    use crate::meta::MemMeta;

    #[tokio::test]
    async fn quiet_brokers_park_background_loops_until_a_client_comes() {
        let b = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        assert!(b.is_quiet(), "no clients, no requests");
        let weak = Arc::downgrade(&b);
        let parked = tokio::spawn(async move { Broker::wait_while_quiet(&weak).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!parked.is_finished(), "parked while quiet");

        let guard = b.awake_guard();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(1), parked).await.unwrap().unwrap(), "a guard wakes it");
        assert!(!b.is_quiet());
        drop(guard);
        assert!(b.is_quiet());

        b.note_request();
        assert!(!b.is_quiet(), "requests keep it awake for a while");
        b.last_request_ms.store(0, Ordering::Relaxed);
        assert!(b.is_quiet());

        // The broker going away releases parked loops.
        let weak = Arc::downgrade(&b);
        let parked = tokio::spawn(async move { Broker::wait_while_quiet(&weak).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(b);
        assert!(!tokio::time::timeout(std::time::Duration::from_secs(1), parked).await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn a_vhost_over_its_publish_budget_waits_and_its_neighbours_do_not() {
        let config = BrokerConfig { vhost_publish_rate: 100, ..BrokerConfig::default() };
        let b = Broker::start(config, MemMeta::new(), MemStore::new()).await.unwrap();
        let busy = b.create_vhost(VHostInfo { name: "busy".into(), ..Default::default() }).await.unwrap();
        let quiet = b.create_vhost(VHostInfo { name: "quiet".into(), ..Default::default() }).await.unwrap();
        let free = b.create_vhost(VHostInfo { name: "free".into(), ..Default::default() }).await.unwrap();
        b.set_vhost_limits("free", HashMap::from([("max-publish-rate".to_string(), -1)])).await.unwrap();
        let publish = |vh: Arc<VHost>, n: usize| async move {
            let t = std::time::Instant::now();
            for _ in 0..n {
                vh.publish("amq.direct", crate::message::Message::new("amq.direct", "k", Default::default(), bytes::Bytes::from_static(b"x")), None).await.unwrap();
            }
            t.elapsed()
        };
        // A second's worth goes straight through, then the bucket pays out 100/s.
        let (busy_took, quiet_took) = tokio::join!(publish(busy.clone(), 150), publish(quiet.clone(), 50));
        assert!(busy_took >= std::time::Duration::from_millis(400), "{busy_took:?}");
        assert!(quiet_took < std::time::Duration::from_millis(100), "{quiet_took:?}");
        assert!(busy.stats.throttled_ms.load(Ordering::Relaxed) > 0);
        assert!(publish(free, 1000).await < std::time::Duration::from_millis(500), "a negative limit lifts the default");
    }

    /// Every log open takes a storage round trip.
    struct SlowLogs(Arc<MemStore>);

    #[async_trait::async_trait]
    impl LogStore for SlowLogs {
        async fn open(&self, key: &str, epoch: u64) -> anyhow::Result<Arc<dyn crate::log::Log>> {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.0.open(key, epoch).await
        }
        fn describe(&self) -> String {
            "slow".into()
        }
    }

    #[tokio::test]
    async fn sleeping_vhosts_wake_in_about_one_log_open_and_independently() {
        let config = BrokerConfig { vhost_idle_suspend_ms: 1, ..BrokerConfig::default() };
        let b = Broker::start(config, MemMeta::new(), Arc::new(SlowLogs(MemStore::new()))).await.unwrap();
        for (t, n) in [("a", 64), ("b", 64), ("c", 1)] {
            let vh = b.create_vhost(VHostInfo { name: t.into(), ..Default::default() }).await.unwrap();
            for i in 0..n {
                let d = crate::vhost::DeclareQueue { name: format!("q{i}"), durable: true, exclusive_owner: None, auto_delete: false, args: FieldTable::new(), passive: false };
                vh.declare_queue(d).await.unwrap();
            }
            assert!(b.suspend_vhost(t).await);
        }
        let t = std::time::Instant::now();
        let (a, bb) = tokio::join!(b.open_vhost("a"), b.open_vhost("b"));
        let took = t.elapsed();
        assert_eq!(a.unwrap().queue_count(), 64);
        assert_eq!(bb.unwrap().queue_count(), 64);
        // One queue at a time, one vhost at a time: 2 × 64 × 20 ms.
        assert!(took < std::time::Duration::from_millis(800), "{took:?}");
        assert!(b.is_asleep("c"));
    }

    #[tokio::test]
    async fn import_skips_what_it_cannot_take_and_keeps_going() {
        let b = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        let defs = json!({
            "users": [{"name": "app", "password_hash": "", "tags": ""}],
            "vhosts": [{"name": "/"}],
            "permissions": [
                {"user": "app", "vhost": "/", "configure": "(?!amq\\.).*", "write": ".*", "read": ".*"},
                {"user": "ghost", "vhost": "/", "configure": ".*", "write": ".*", "read": ".*"},
                {"user": "app", "vhost": "/", "configure": "", "write": ".*", "read": ".*"},
            ],
            "policies": [
                {"vhost": "/", "name": "ha-all", "pattern": ".*", "apply-to": "queues", "definition": {"ha-mode": "all", "ha-sync-mode": "automatic"}, "priority": 0},
                {"vhost": "/", "name": "mirrored-ttl", "pattern": "^orders", "apply-to": "queues", "definition": {"ha-mode": "exactly", "ha-params": 2, "ha-sync-batch-size": 5, "queue-master-locator": "min-masters", "message-ttl": 60000}, "priority": 1},
                {"vhost": "/", "name": "lookahead", "pattern": "^(?!amq\\.).*", "apply-to": "all", "definition": {"max-length": 10}, "priority": 0},
                {"vhost": "/", "name": "plugin", "pattern": ".*", "apply-to": "all", "definition": {"some-plugin-key": 1}, "priority": 0},
            ],
            "exchanges": [
                {"vhost": "/", "name": "orders.x", "type": "topic", "durable": true, "auto_delete": false, "internal": false, "arguments": {}},
                {"vhost": "/", "name": "weird.x", "type": "x-no-such-type", "durable": true, "auto_delete": false, "internal": false, "arguments": {}},
            ],
            "queues": [{"vhost": "/", "name": "orders", "durable": true, "auto_delete": false, "arguments": {}}],
            "bindings": [
                {"vhost": "/", "source": "weird.x", "destination": "orders", "destination_type": "queue", "routing_key": "k", "arguments": {}},
                {"vhost": "/", "source": "orders.x", "destination": "orders", "destination_type": "queue", "routing_key": "order.#", "arguments": {}},
            ],
        });
        let report = b.import_definitions(&defs, None).await.expect("import keeps going");
        let find = |kind: &str, name: &str| report.skipped.iter().find(|s| s.kind == kind && s.name == name).cloned();

        // Obsolete keys are dropped; a policy with nothing else is skipped, others are kept.
        let ha = find("policy", "ha-all").expect("mirroring-only policy skipped");
        assert!(!ha.partial && ha.reason.contains("ha-mode") && ha.reason.contains("RabbitMQ 4"), "{ha:?}");
        let ttl = find("policy", "mirrored-ttl").expect("noted");
        assert!(ttl.partial && ttl.reason.contains("ha-sync-batch-size") && ttl.reason.contains("queue-master-locator"), "{ttl:?}");
        let kept = b.policies.read().iter().find(|p| p.name == "mirrored-ttl").cloned().expect("imported");
        assert_eq!(kept.definition, json!({"message-ttl": 60000}));
        assert!(find("policy", "lookahead").unwrap().reason.contains("look-around"));
        assert!(find("policy", "plugin").unwrap().reason.contains("some-plugin-key"));

        // Bad permissions are skipped; the valid one for the same user lands.
        assert_eq!(report.count("permission"), 2, "{:?}", report.skipped);
        assert_eq!(b.permissions().iter().find(|(v, u, _)| v == "/" && u == "app").map(|p| p.2.write.clone()), Some(".*".into()));

        // The bad exchange and its binding are skipped; everything else is in.
        assert!(find("exchange", "weird.x").is_some());
        assert!(find("binding", "weird.x -> orders [k]").is_some());
        let vh = b.vhost("/").unwrap();
        assert!(vh.exchange("orders.x").is_some() && vh.queue("orders").is_some());
        assert!(vh.exchange("orders.x").unwrap().bindings().iter().any(|x| x.routing_key == "order.#"));
        assert_eq!(report.skipped.len(), 8, "{:#?}", report.skipped);

        // Parameters can be validated by the caller.
        let defs = json!({"parameters": [
            {"component": "shovel", "vhost": "/", "name": "bad", "value": {}},
            {"component": "shovel", "vhost": "/", "name": "good", "value": {"ok": true}},
        ]});
        let opts = ImportOptions { validate_parameter: Some(|_, v| if v["ok"] == true { Ok(()) } else { Err("not ok".into()) }) };
        let report = b.import_definitions_with(&defs, None, &opts).await.unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert_eq!((report.skipped[0].name.as_str(), report.skipped[0].reason.as_str()), ("shovel/bad", "not ok"));
        assert!(b.parameter("shovel", "/", "good").is_some() && b.parameter("shovel", "/", "bad").is_none());
    }
}

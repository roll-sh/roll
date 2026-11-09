//! The broker: streams (tenants), their topics and partitions, and the background work that keeps
//! logs durable and bottomless — group-commit fsync, uploads to object storage, retention, and
//! unloading idle partitions so an idle stream costs nothing but storage.

use crate::auth::{self, PlainCache};
use crate::config::{self, Config};
use crate::error::KafkaError;
use crate::groups::Groups;
use crate::ids::{new_id, new_uuid, now, now_ms};
use crate::store::{Credential, Settings, StorageSample, Store, StreamRow, TopicRow, UsageRow};
use crate::topic_config::{self, Effective};
use crate::txn::Txns;
use bytes::Bytes;
use futures::StreamExt;
use kafka_protocol::ResponseError;
use pg_storage::Objects;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use stream_log::{Chunk, CompactionPolicy, Log, LogConfig, Recovered, Retention, TailBudget, batch};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub struct Broker {
    pub config: Config,
    pub store: Store,
    pub objects: Objects,
    streams: RwLock<HashMap<String, Arc<Stream>>>,
    pub groups: Groups,
    pub txns: Txns,
    pub plain: PlainCache,
    pub(crate) remote: RemoteCache,
    /// Memory for the tail caches of every open partition.
    pub tail_budget: Arc<TailBudget>,
    flush_tx: mpsc::UnboundedSender<FlushJob>,
    flush_rx: Mutex<Option<mpsc::UnboundedReceiver<FlushJob>>>,
    producer_ids: Mutex<(i64, i64)>,
    pub started: Instant,
}

/// A tenant: what a Confluent cluster or MSK cluster is, minus the brokers.
pub struct Stream {
    pub id: String,
    pub cluster_id: String,
    pub row: RwLock<StreamRow>,
    topics: RwLock<TopicMap>,
    pub usage: Usage,
    pub ingress: Bucket,
    pub egress: Bucket,
}

#[derive(Default)]
struct TopicMap {
    by_name: BTreeMap<String, Arc<Topic>>,
    by_id: HashMap<Uuid, Arc<Topic>>,
}

pub struct Topic {
    pub id: Uuid,
    pub name: String,
    pub stream_id: String,
    pub created_at: i64,
    configs: RwLock<BTreeMap<String, String>>,
    effective: RwLock<Effective>,
    partitions: RwLock<Vec<Arc<Partition>>>,
    pub deleted: AtomicBool,
}

pub struct Partition {
    pub topic_id: Uuid,
    pub stream_id: String,
    pub index: i32,
    log: Mutex<Option<Arc<Log>>>,
    last_used_ms: AtomicI64,
    flush_queued: AtomicBool,
    uploading: AtomicBool,
    pub(crate) compacting: AtomicBool,
    /// While closed, the partition is left alone until this time (ms): nothing can expire or
    /// become compactable before it.
    next_clean_ms: AtomicI64,
}

#[derive(Default)]
pub struct Usage {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub records_in: AtomicU64,
    pub requests: AtomicU64,
    pub puts: AtomicU64,
    pub gets: AtomicU64,
    /// Totals since start, for live throughput.
    pub total_in: AtomicU64,
    pub total_out: AtomicU64,
}

/// Token bucket for byte-rate quotas; returns how long to throttle the client.
pub struct Bucket(Mutex<(f64, Instant)>);

impl Default for Bucket {
    fn default() -> Self {
        Self(Mutex::new((0.0, Instant::now())))
    }
}

impl Bucket {
    /// Charge `bytes` against `rate` bytes/s (0 = unlimited) with one second of burst.
    pub fn charge(&self, bytes: u64, rate: u64) -> u32 {
        if rate == 0 {
            return 0;
        }
        let mut g = self.0.lock().unwrap();
        let now = Instant::now();
        let rate = rate as f64;
        g.0 = (g.0 + now.duration_since(g.1).as_secs_f64() * rate).min(rate);
        g.1 = now;
        g.0 -= bytes as f64;
        if g.0 >= 0.0 { 0 } else { ((-g.0 / rate) * 1000.0).ceil().min(30_000.0) as u32 }
    }
}

impl Usage {
    pub fn record_in(&self, bytes: u64, records: u64) {
        self.bytes_in.fetch_add(bytes, Ordering::Relaxed);
        self.total_in.fetch_add(bytes, Ordering::Relaxed);
        self.records_in.fetch_add(records, Ordering::Relaxed);
    }
    pub fn record_out(&self, bytes: u64) {
        self.bytes_out.fetch_add(bytes, Ordering::Relaxed);
        self.total_out.fetch_add(bytes, Ordering::Relaxed);
    }
    fn take(&self) -> UsageRow {
        UsageRow {
            hour: now() / 3600 * 3600,
            bytes_in: self.bytes_in.swap(0, Ordering::Relaxed) as i64,
            bytes_out: self.bytes_out.swap(0, Ordering::Relaxed) as i64,
            records_in: self.records_in.swap(0, Ordering::Relaxed) as i64,
            requests: self.requests.swap(0, Ordering::Relaxed) as i64,
            puts: self.puts.swap(0, Ordering::Relaxed) as i64,
            gets: self.gets.swap(0, Ordering::Relaxed) as i64,
        }
    }
}

impl Topic {
    pub fn effective(&self) -> Effective {
        self.effective.read().unwrap().clone()
    }
    pub fn configs(&self) -> BTreeMap<String, String> {
        self.configs.read().unwrap().clone()
    }
    pub fn partition(&self, i: i32) -> Option<Arc<Partition>> {
        usize::try_from(i).ok().and_then(|i| self.partitions.read().unwrap().get(i).cloned())
    }
    pub fn partitions(&self) -> Vec<Arc<Partition>> {
        self.partitions.read().unwrap().clone()
    }
    pub fn partition_count(&self) -> i32 {
        self.partitions.read().unwrap().len() as i32
    }
}

impl Partition {
    fn new(topic: &Topic, index: i32) -> Arc<Self> {
        Arc::new(Self {
            topic_id: topic.id,
            stream_id: topic.stream_id.clone(),
            index,
            log: Mutex::new(None),
            last_used_ms: AtomicI64::new(now_ms()),
            flush_queued: AtomicBool::new(false),
            uploading: AtomicBool::new(false),
            compacting: AtomicBool::new(false),
            next_clean_ms: AtomicI64::new(0),
        })
    }

    /// The log, if it's open.
    pub fn loaded(&self) -> Option<Arc<Log>> {
        self.log.lock().unwrap().clone()
    }
}

impl Stream {
    pub fn topic(&self, name: &str) -> Option<Arc<Topic>> {
        self.topics.read().unwrap().by_name.get(name).cloned()
    }
    pub fn topic_by_id(&self, id: &Uuid) -> Option<Arc<Topic>> {
        self.topics.read().unwrap().by_id.get(id).cloned()
    }
    pub fn topics(&self) -> Vec<Arc<Topic>> {
        self.topics.read().unwrap().by_name.values().cloned().collect()
    }
    pub fn settings(&self) -> Settings {
        self.row.read().unwrap().settings.clone()
    }
    pub fn name(&self) -> String {
        self.row.read().unwrap().name.clone()
    }
    fn partition_total(&self) -> i32 {
        self.topics.read().unwrap().by_name.values().map(|t| t.partition_count()).sum()
    }
}

/// Kafka's legal topic names.
pub fn validate_topic_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("Topic name \"{name}\" is illegal"));
    }
    if name.len() > 249 {
        return Err("Topic name is longer than 249 characters".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err(format!(
            "Topic name \"{name}\" is illegal, it contains a character other than ASCII alphanumerics, '.', '_' and '-'"
        ));
    }
    Ok(())
}

fn cluster_id_for(stream_id: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    let h = sha2::Sha256::digest(stream_id.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&h[..16])
}

/// Chunks recently read from object storage.
pub(crate) struct RemoteCache {
    max: usize,
    inner: Mutex<RemoteInner>,
}

#[derive(Default)]
struct RemoteInner {
    map: HashMap<String, (Bytes, u64)>,
    bytes: usize,
    tick: u64,
}

impl RemoteCache {
    pub(crate) fn get(&self, key: &str) -> Option<Bytes> {
        let mut g = self.inner.lock().unwrap();
        g.tick += 1;
        let t = g.tick;
        g.map.get_mut(key).map(|(b, used)| {
            *used = t;
            b.clone()
        })
    }
    pub(crate) fn put(&self, key: &str, data: Bytes) {
        if data.len() > self.max {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        g.tick += 1;
        let t = g.tick;
        g.bytes += data.len();
        if let Some((old, _)) = g.map.insert(key.to_string(), (data, t)) {
            g.bytes -= old.len();
        }
        while g.bytes > self.max {
            let Some(k) = g.map.iter().min_by_key(|(_, (_, used))| *used).map(|(k, _)| k.clone()) else { break };
            if let Some((b, _)) = g.map.remove(&k) {
                g.bytes -= b.len();
            }
        }
    }
    pub(crate) fn remove(&self, key: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some((b, _)) = g.map.remove(key) {
            g.bytes -= b.len();
        }
    }
}

type FlushJob = (Arc<Partition>, Arc<Log>);
type Loaded = (Arc<Stream>, Arc<Topic>, Arc<Partition>, Arc<Log>);

pub type Result<T> = std::result::Result<T, KafkaError>;

impl Broker {
    pub async fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(config.home.join("data"))?;
        let store = Store::open(&config.home.join("stream.db"))?;
        let objects = Objects::new(&config.storage)?;
        let (flush_tx, flush_rx) = mpsc::unbounded_channel();
        let broker = Arc::new(Self {
            remote: RemoteCache { max: config.remote_cache_bytes, inner: Mutex::default() },
            tail_budget: TailBudget::new(config.tail_cache_total_bytes),
            groups: Groups::default(),
            txns: Txns::default(),
            plain: PlainCache::default(),
            streams: RwLock::default(),
            flush_tx,
            flush_rx: Mutex::new(Some(flush_rx)),
            producer_ids: Mutex::new((0, 0)),
            started: Instant::now(),
            config,
            store,
            objects,
        });
        broker.load()?;
        Ok(broker)
    }

    fn load(&self) -> anyhow::Result<()> {
        let mut streams = HashMap::new();
        for row in self.store.streams()? {
            streams.insert(row.id.clone(), Arc::new(self.stream_from(row)));
        }
        for t in self.store.topics()? {
            if let Some(s) = streams.get(&t.stream_id) {
                let topic = self.topic_from(t);
                let mut m = s.topics.write().unwrap();
                m.by_id.insert(topic.id, topic.clone());
                m.by_name.insert(topic.name.clone(), topic);
            }
        }
        info!(streams = streams.len(), "stream catalog loaded");
        *self.streams.write().unwrap() = streams;
        Ok(())
    }

    fn stream_from(&self, row: StreamRow) -> Stream {
        Stream {
            id: row.id.clone(),
            cluster_id: cluster_id_for(&row.id),
            row: RwLock::new(row),
            topics: RwLock::default(),
            usage: Usage::default(),
            ingress: Bucket::default(),
            egress: Bucket::default(),
        }
    }

    fn topic_from(&self, t: TopicRow) -> Arc<Topic> {
        let effective = topic_config::effective(&t.configs, &self.config.topics);
        let topic = Arc::new(Topic {
            id: t.id,
            name: t.name,
            stream_id: t.stream_id,
            created_at: t.created_at,
            configs: RwLock::new(t.configs),
            effective: RwLock::new(effective),
            partitions: RwLock::new(Vec::new()),
            deleted: AtomicBool::new(false),
        });
        let parts = (0..t.partitions).map(|i| Partition::new(&topic, i)).collect();
        *topic.partitions.write().unwrap() = parts;
        topic
    }

    // ── streams ──

    pub fn streams(&self) -> Vec<Arc<Stream>> {
        let mut v: Vec<_> = self.streams.read().unwrap().values().cloned().collect();
        v.sort_by_key(|s| s.row.read().unwrap().created_at);
        v
    }

    pub fn stream(&self, id: &str) -> Option<Arc<Stream>> {
        self.streams.read().unwrap().get(id).cloned()
    }

    /// By id or name.
    pub fn find_stream(&self, key: &str) -> Option<Arc<Stream>> {
        self.stream(key).or_else(|| self.streams.read().unwrap().values().find(|s| s.name() == key).cloned())
    }

    /// Create a stream and its first API key: `(stream, credential, secret)`.
    pub fn create_stream(&self, name: &str, settings: Settings) -> Result<(Arc<Stream>, Credential, String)> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(KafkaError::invalid_request("stream names are 1-64 letters, digits, '-' or '_'"));
        }
        if self.find_stream(name).is_some() {
            return Err(KafkaError::new(ResponseError::DuplicateResource, format!("stream {name} already exists")));
        }
        let row = StreamRow { id: new_id("st"), name: name.into(), region: "local".into(), created_at: now(), settings };
        self.store.insert_stream(&row).map_err(KafkaError::internal)?;
        let (cred, secret) = auth::new_credential(&row.id, "default");
        self.store.insert_credential(&cred).map_err(KafkaError::internal)?;
        let stream = Arc::new(self.stream_from(row));
        self.streams.write().unwrap().insert(stream.id.clone(), stream.clone());
        info!(stream = %stream.id, name, "stream created");
        Ok((stream, cred, secret))
    }

    pub fn update_settings(&self, stream: &Stream, f: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let mut row = stream.row.write().unwrap();
        let mut s = row.settings.clone();
        f(&mut s);
        if s.default_partitions < 1 || s.max_partitions < 1 {
            return Err(KafkaError::invalid_request("partition counts must be positive"));
        }
        self.store.update_settings(&stream.id, &s).map_err(KafkaError::internal)?;
        row.settings = s.clone();
        Ok(s)
    }

    pub async fn delete_stream(&self, id: &str) -> Result<()> {
        let stream = self.stream(id).ok_or_else(|| KafkaError::not_found(format!("stream {id}")))?;
        for t in stream.topics() {
            self.delete_topic(&stream, &t).await?;
        }
        self.groups.drop_stream(&stream.id);
        self.txns.drop_stream(&stream.id);
        self.store.delete_stream(&stream.id).map_err(KafkaError::internal)?;
        self.streams.write().unwrap().remove(&stream.id);
        let objects = self.objects.clone();
        let prefix = config::stream_prefix(&stream.id);
        tokio::spawn(async move {
            if let Err(e) = objects.delete_prefix(&prefix).await {
                warn!(%prefix, error = %e, "deleting stream objects");
            }
        });
        info!(stream = %stream.id, "stream deleted");
        Ok(())
    }

    pub fn create_key(&self, stream: &Stream, label: &str) -> Result<(Credential, String)> {
        let (cred, secret) = auth::new_credential(&stream.id, label);
        self.store.insert_credential(&cred).map_err(KafkaError::internal)?;
        Ok((cred, secret))
    }

    /// The stream an authenticated SASL user belongs to.
    pub fn credential(&self, username: &str) -> Option<Credential> {
        self.store.credential(username).ok().flatten()
    }

    // ── topics ──

    /// Create a topic (`partitions`/`replication` of -1 mean the defaults).
    pub fn create_topic(
        &self,
        stream: &Stream,
        name: &str,
        partitions: i32,
        replication: i16,
        configs: BTreeMap<String, String>,
        validate_only: bool,
    ) -> Result<Option<Arc<Topic>>> {
        validate_topic_name(name).map_err(|m| KafkaError::new(ResponseError::InvalidTopicException, m))?;
        let settings = stream.settings();
        let partitions = if partitions == -1 { settings.default_partitions } else { partitions };
        if partitions < 1 {
            return Err(KafkaError::new(ResponseError::InvalidPartitions, "Number of partitions must be larger than 0."));
        }
        // Durability comes from fsync and object storage; any replication factor a managed
        // Kafka accepts is fine and reported as a single replica.
        if !(replication == -1 || (1..=3).contains(&replication)) {
            return Err(KafkaError::new(
                ResponseError::InvalidReplicationFactor,
                "Replication factor must be between 1 and 3 (data is replicated by object storage).",
            ));
        }
        let mut configs = topic_config::validate_all(&configs).map_err(|m| KafkaError::new(ResponseError::InvalidConfig, m))?;
        if let Some(ms) = settings.retention_ms
            && !configs.contains_key("retention.ms")
        {
            configs.insert("retention.ms".into(), ms.to_string());
        }
        {
            let m = stream.topics.read().unwrap();
            if m.by_name.contains_key(name) {
                return Err(KafkaError::new(ResponseError::TopicAlreadyExists, format!("Topic '{name}' already exists.")));
            }
            let collides = name.replace('.', "_");
            if let Some(other) = m.by_name.keys().find(|k| k.replace('.', "_") == collides) {
                return Err(KafkaError::new(
                    ResponseError::InvalidTopicException,
                    format!("Topic '{name}' collides with existing topic: {other}"),
                ));
            }
        }
        if stream.partition_total() + partitions > settings.max_partitions {
            return Err(KafkaError::new(
                ResponseError::PolicyViolation,
                format!("the stream is limited to {} partitions", settings.max_partitions),
            ));
        }
        if validate_only {
            return Ok(None);
        }
        let row = TopicRow { id: new_uuid(), stream_id: stream.id.clone(), name: name.into(), partitions, configs, created_at: now() };
        self.store.insert_topic(&row).map_err(KafkaError::internal)?;
        let topic = self.topic_from(row);
        let mut m = stream.topics.write().unwrap();
        m.by_id.insert(topic.id, topic.clone());
        m.by_name.insert(topic.name.clone(), topic.clone());
        debug!(stream = %stream.id, topic = name, partitions, "topic created");
        Ok(Some(topic))
    }

    /// Auto-create a topic on first use, if the stream allows it.
    pub fn auto_create(&self, stream: &Stream, name: &str) -> Option<Arc<Topic>> {
        if !stream.settings().auto_create_topics {
            return None;
        }
        match self.create_topic(stream, name, -1, -1, BTreeMap::new(), false) {
            Ok(t) => t,
            Err(_) => stream.topic(name),
        }
    }

    pub async fn delete_topic(&self, stream: &Stream, topic: &Arc<Topic>) -> Result<()> {
        if topic.deleted.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut m = stream.topics.write().unwrap();
            m.by_name.remove(&topic.name);
            m.by_id.remove(&topic.id);
        }
        self.store.delete_topic(&topic.id).map_err(KafkaError::internal)?;
        self.store.delete_topic_offsets(&stream.id, &topic.name).map_err(KafkaError::internal)?;
        self.groups.topic_deleted(&stream.id, &topic.name);
        for p in topic.partitions() {
            p.log.lock().unwrap().take();
        }
        let dir = self.config.topic_dir(&topic.id);
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        let objects = self.objects.clone();
        let prefix = config::topic_prefix(&stream.id, &topic.id);
        tokio::spawn(async move {
            if let Err(e) = objects.delete_prefix(&prefix).await {
                warn!(%prefix, error = %e, "deleting topic objects");
            }
        });
        debug!(stream = %stream.id, topic = %topic.name, "topic deleted");
        Ok(())
    }

    pub fn add_partitions(&self, stream: &Stream, topic: &Topic, count: i32, validate_only: bool) -> Result<()> {
        let current = topic.partition_count();
        if count <= current {
            return Err(KafkaError::new(
                ResponseError::InvalidPartitions,
                format!("Topic currently has {current} partitions, which is higher than the requested {count}."),
            ));
        }
        let max = stream.settings().max_partitions;
        if stream.partition_total() + count - current > max {
            return Err(KafkaError::new(ResponseError::PolicyViolation, format!("the stream is limited to {max} partitions")));
        }
        if validate_only {
            return Ok(());
        }
        self.store.set_partitions(&topic.id, count).map_err(KafkaError::internal)?;
        let mut parts = topic.partitions.write().unwrap();
        for i in current..count {
            parts.push(Partition::new(topic, i));
        }
        Ok(())
    }

    /// Replace a topic's config overrides.
    pub fn set_topic_configs(&self, topic: &Topic, configs: BTreeMap<String, String>, validate_only: bool) -> Result<()> {
        let configs = topic_config::validate_all(&configs).map_err(|m| KafkaError::new(ResponseError::InvalidConfig, m))?;
        if validate_only {
            return Ok(());
        }
        self.store.set_configs(&topic.id, &configs).map_err(KafkaError::internal)?;
        let effective = topic_config::effective(&configs, &self.config.topics);
        let log_config = self.log_config(&effective);
        *topic.configs.write().unwrap() = configs;
        *topic.effective.write().unwrap() = effective;
        for p in topic.partitions() {
            // New retention or compaction settings may make a closed partition due now.
            p.next_clean_ms.store(0, Ordering::Relaxed);
            if let Some(log) = p.loaded() {
                log.set_config(log_config.clone());
            }
        }
        Ok(())
    }

    // ── partitions ──

    fn log_config(&self, e: &Effective) -> LogConfig {
        LogConfig {
            segment_bytes: e.segment_bytes,
            segment_ms: e.segment_ms,
            index_interval_bytes: e.index_interval_bytes,
            fsync: self.config.fsync,
            tail_cache_bytes: self.config.tail_cache_bytes,
            tail_budget: Some(self.tail_budget.clone()),
            leader_epoch: 0,
        }
    }

    /// Open a partition's log (recovering it on first use).
    pub fn log(&self, topic: &Topic, p: &Partition) -> Result<Arc<Log>> {
        p.last_used_ms.store(now_ms(), Ordering::Relaxed);
        self.open(topic, p)
    }

    fn open(&self, topic: &Topic, p: &Partition) -> Result<Arc<Log>> {
        let mut slot = p.log.lock().unwrap();
        if let Some(log) = &*slot {
            return Ok(log.clone());
        }
        if topic.deleted.load(Ordering::SeqCst) {
            return Err(KafkaError::code(ResponseError::UnknownTopicOrPartition));
        }
        let recovered = (|| -> anyhow::Result<Recovered> {
            Ok(Recovered {
                log_start: self.store.log_start(&topic.id, p.index)?,
                chunks: self.store.chunks(&topic.id, p.index)?,
                aborted: self.store.aborted(&topic.id, p.index)?,
            })
        })()
        .map_err(KafkaError::storage)?;
        let dir = self.config.partition_dir(&topic.id, p.index);
        let cleaned_to = crate::compaction::cleaned_to(&recovered.chunks, recovered.log_start);
        let log = Log::open(dir, self.log_config(&topic.effective()), recovered, now_ms()).map_err(KafkaError::storage)?;
        log.set_cleaned_to(cleaned_to);
        let log = Arc::new(log);
        *slot = Some(log.clone());
        Ok(log)
    }

    /// Queue a group-commit fsync for a log.
    pub fn request_flush(&self, p: &Arc<Partition>, log: &Arc<Log>) {
        if self.config.fsync && !p.flush_queued.swap(true, Ordering::AcqRel) {
            let _ = self.flush_tx.send((p.clone(), log.clone()));
        }
    }

    /// Bytes of a chunk, from the cache or object storage.
    pub async fn chunk(&self, stream: &Stream, chunk: &Chunk) -> anyhow::Result<Bytes> {
        if let Some(b) = self.remote.get(&chunk.key) {
            return Ok(b);
        }
        stream.usage.gets.fetch_add(1, Ordering::Relaxed);
        let data = self.objects.get(&chunk.key).await?.ok_or_else(|| anyhow::anyhow!("chunk {} is missing", chunk.key))?;
        self.remote.put(&chunk.key, data.clone());
        Ok(data)
    }

    /// Bytes of a chunk for a background scan: from the cache if present, without filling it.
    pub async fn chunk_uncached(&self, stream: &Stream, chunk: &Chunk) -> anyhow::Result<Bytes> {
        if let Some(b) = self.remote.get(&chunk.key) {
            return Ok(b);
        }
        stream.usage.gets.fetch_add(1, Ordering::Relaxed);
        self.objects.get(&chunk.key).await?.ok_or_else(|| anyhow::anyhow!("chunk {} is missing", chunk.key))
    }

    /// Allocate a producer id (in blocks, so the catalog is touched rarely).
    pub fn next_producer_id(&self) -> anyhow::Result<i64> {
        let mut g = self.producer_ids.lock().unwrap();
        if g.0 >= g.1 {
            let start = self.store.allocate_producer_ids(1000)?;
            *g = (start, start + 1000);
        }
        g.0 += 1;
        Ok(g.0 - 1)
    }

    pub fn retention(&self, e: &Effective) -> Retention {
        Retention {
            retention_ms: if e.delete { e.retention_ms } else { -1 },
            retention_bytes: if e.delete { e.retention_bytes } else { -1 },
            local_retention_ms: e.local_retention_ms,
            local_retention_bytes: e.local_retention_bytes,
        }
    }

    /// Apply retention to one partition, optionally truncating its head (`DeleteRecords`).
    pub async fn clean_partition(&self, stream: &Stream, topic: &Topic, p: &Partition, truncate_to: Option<i64>) -> Result<i64> {
        let log = match truncate_to {
            Some(_) => self.log(topic, p)?,
            None => self.open(topic, p)?,
        };
        let policy = self.retention(&topic.effective());
        let cleaned = log.clean(now_ms(), policy, truncate_to);
        if cleaned.start_moved {
            self.store.set_log_start(&topic.id, p.index, cleaned.log_start).map_err(KafkaError::storage)?;
        }
        if !cleaned.removed_chunks.is_empty() {
            let firsts: Vec<i64> = cleaned.removed_chunks.iter().map(|c| c.first).collect();
            self.store.delete_chunks(&topic.id, p.index, &firsts).map_err(KafkaError::storage)?;
            for c in &cleaned.removed_chunks {
                self.remote.remove(&c.key);
                if let Err(e) = self.objects.delete(&c.key).await {
                    warn!(key = %c.key, error = %e, "deleting expired chunk");
                }
            }
            debug!(stream = %stream.id, topic = %topic.name, partition = p.index, chunks = firsts.len(), "retention removed chunks");
        }
        Ok(cleaned.log_start)
    }

    // ── background work ──

    /// Run the background loops forever.
    pub async fn run(self: Arc<Self>) {
        let rx = self.flush_rx.lock().unwrap().take().expect("run once");
        tokio::join!(
            self.clone().flusher(rx),
            self.clone().uploader(),
            self.clone().cleaner(),
            self.clone().meter(),
            crate::groups::run(self.clone()),
            crate::txn::run(self.clone()),
        );
    }

    async fn flusher(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<(Arc<Partition>, Arc<Log>)>) {
        let sem = Arc::new(Semaphore::new(64));
        while let Some((p, log)) = rx.recv().await {
            let permit = sem.clone().acquire_owned().await.expect("semaphore");
            tokio::spawn(async move {
                // Appends after this point queue a new flush.
                p.flush_queued.store(false, Ordering::Release);
                match tokio::task::spawn_blocking(move || log.flush()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => warn!(topic = %p.topic_id, partition = p.index, error = %e, "fsync failed"),
                    Err(e) => warn!(error = %e, "flush task panicked"),
                }
                drop(permit);
            });
        }
    }

    fn loaded_partitions(&self) -> Vec<Loaded> {
        let mut out = Vec::new();
        for s in self.streams() {
            for t in s.topics() {
                for p in t.partitions() {
                    if let Some(log) = p.loaded() {
                        out.push((s.clone(), t.clone(), p.clone(), log));
                    }
                }
            }
        }
        out
    }

    async fn uploader(self: Arc<Self>) {
        let cfg = self.config.upload.clone();
        let mut tick = tokio::time::interval(Duration::from_millis(cfg.interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let _ = self.upload_once(false).await;
        }
    }

    /// Upload everything that's due (or everything durable, with `force`). Returns chunks written.
    pub async fn upload_once(&self, force: bool) -> usize {
        let cfg = &self.config.upload;
        let jobs = self.loaded_partitions().into_iter().filter(|(_, _, p, _)| !p.uploading.load(Ordering::Acquire));
        let (min, age) = if force { (1, 0) } else { (cfg.min_bytes, cfg.max_age_ms) };
        let futs = jobs.map(|(s, t, p, log)| async move {
            p.uploading.store(true, Ordering::Release);
            let mut n = 0;
            while let Some(plan) = log.upload_plan(min, cfg.max_bytes, age, now_ms()) {
                match self.upload(&s, &t, &p, &log, &plan).await {
                    Ok(()) => n += 1,
                    Err(e) => {
                        warn!(topic = %t.name, partition = p.index, error = %e, "upload failed; will retry");
                        break;
                    }
                }
            }
            p.uploading.store(false, Ordering::Release);
            n
        });
        futures::stream::iter(futs).buffer_unordered(cfg.concurrency.max(1)).collect::<Vec<_>>().await.into_iter().sum()
    }

    async fn upload(&self, s: &Stream, t: &Topic, p: &Partition, log: &Log, plan: &stream_log::UploadPlan) -> anyhow::Result<()> {
        let plan_c = plan.clone();
        let data = tokio::task::spawn_blocking(move || Log::read_upload(&plan_c)).await??;
        let span = batch::summarize(&data).ok_or_else(|| anyhow::anyhow!("nothing to upload"))?;
        let key = config::chunk_key(&s.id, &t.id, p.index, span.first, span.last);
        let chunk = Chunk { first: span.first, last: span.last, max_timestamp: span.max_timestamp, bytes: data.len() as u64, key };
        self.objects.put(&chunk.key, data).await?;
        s.usage.puts.fetch_add(1, Ordering::Relaxed);
        if t.deleted.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.store.insert_chunk(&t.id, p.index, &chunk, now())?;
        log.uploaded(plan, chunk, now_ms());
        Ok(())
    }

    async fn cleaner(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(self.config.clean_interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.clean_once().await;
        }
    }

    /// Retention and compaction for every partition with data, then unload idle partitions.
    /// Closed partitions are skipped until something in them can be due, so an idle broker
    /// touches neither disk nor object storage.
    pub async fn clean_once(&self) {
        let now = now_ms();
        let cutoff = now - self.config.idle_unload_ms;
        let producer_cutoff = now_ms() - self.config.txn.producer_id_expiration_ms;
        for s in self.streams() {
            for t in s.topics() {
                for p in t.partitions() {
                    let had = p.loaded().is_some();
                    if !had && now < p.next_clean_ms.load(Ordering::Relaxed) {
                        continue;
                    }
                    let has_data = had || self.config.partition_dir(&t.id, p.index).exists();
                    if !has_data {
                        continue;
                    }
                    if let Err(e) = self.clean_partition(&s, &t, &p, None).await {
                        warn!(topic = %t.name, partition = p.index, error = %e.message(), "retention failed");
                    }
                    if let Some(log) = p.loaded() {
                        log.expire_producers(producer_cutoff);
                        if let Err(e) = self.compact_partition(&s, &t, &p, &log).await {
                            warn!(topic = %t.name, partition = p.index, error = %e, "compaction failed");
                        }
                    }
                    if !had || p.last_used_ms.load(Ordering::Relaxed) < cutoff {
                        self.unload(&t, &p);
                    }
                }
            }
        }
        self.enforce_disk_budget();
    }

    /// Keep open partitions' local data under `local_disk_bytes` by dropping the oldest segments
    /// object storage already has, across all partitions.
    fn enforce_disk_budget(&self) {
        let budget = self.config.local_disk_bytes;
        if budget == 0 {
            return;
        }
        let logs: Vec<Arc<Log>> = self.loaded_partitions().into_iter().map(|(_, _, _, log)| log).collect();
        let mut used: u64 = logs.iter().map(|l| l.stats().local_bytes).sum();
        if used <= budget {
            return;
        }
        // (largest timestamp, log, bytes): segments are evicted per log in order, so pick the
        // globally oldest and count how many to take from each.
        let mut candidates: Vec<(i64, usize, u64)> =
            logs.iter().enumerate().flat_map(|(i, l)| l.evictable().into_iter().map(move |(ts, b)| (ts, i, b))).collect();
        candidates.sort_unstable_by_key(|c| c.0);
        let mut take = vec![0usize; logs.len()];
        for (_, i, bytes) in candidates {
            if used <= budget {
                break;
            }
            take[i] += 1;
            used = used.saturating_sub(bytes);
        }
        for (log, n) in logs.iter().zip(take) {
            if n > 0 {
                log.evict(n);
            }
        }
        if used > budget {
            debug!(used, budget, "local data over budget; the rest isn't in object storage yet");
        }
    }

    /// Close an idle partition once it's durable and fully uploaded. It's marked clean so the
    /// next open skips validation, and remembers when cleanup next has anything to do.
    fn unload(&self, t: &Topic, p: &Partition) {
        let mut slot = p.log.lock().unwrap();
        if let Some(log) = &*slot
            && Arc::strong_count(log) == 1
            && !log.needs_flush()
            && log.stats().pending_upload_bytes == 0
            && !p.uploading.load(Ordering::Acquire)
        {
            let e = t.effective();
            let compaction = e.compact.then_some(CompactionPolicy {
                min_lag_ms: e.min_compaction_lag_ms,
                max_lag_ms: e.max_compaction_lag_ms,
                dirty_ratio: e.min_cleanable_dirty_ratio,
            });
            let now = now_ms();
            // Re-check at least hourly, never more often than the cleaner runs.
            let due = log.next_due_ms(self.retention(&e), compaction).min(now + 3_600_000).max(now + self.config.clean_interval_ms as i64);
            p.next_clean_ms.store(due, Ordering::Relaxed);
            if self.config.evict_on_unload
                && let Err(e) = log.evict_all(now)
            {
                debug!(topic = %t.name, partition = p.index, error = %e, "dropping local copies of a closed log");
            }
            if let Err(e) = log.mark_clean() {
                debug!(topic = %t.name, partition = p.index, error = %e, "marking a closed log clean");
            }
            *slot = None;
        }
    }

    async fn meter(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await;
        let mut n = 0u64;
        loop {
            tick.tick().await;
            self.flush_usage();
            if n.is_multiple_of(5) {
                self.sample_storage().await;
            }
            n += 1;
        }
    }

    pub fn flush_usage(&self) {
        for s in self.streams() {
            let u = s.usage.take();
            if (u.bytes_in | u.bytes_out | u.requests | u.puts | u.gets) != 0
                && let Err(e) = self.store.add_usage(&s.id, &u)
            {
                warn!(error = %e, "recording usage");
            }
        }
    }

    pub async fn sample_storage(&self) {
        let remote = self.store.remote_bytes().unwrap_or_default();
        for s in self.streams() {
            let dirs: Vec<_> = s.topics().iter().map(|t| self.config.topic_dir(&t.id)).collect();
            let local = tokio::task::spawn_blocking(move || dirs.iter().map(|d| dir_size(d)).sum::<u64>()).await.unwrap_or(0);
            let sample =
                StorageSample { at: now(), local_bytes: local as i64, remote_bytes: remote.get(&s.id).copied().unwrap_or(0) };
            if let Err(e) = self.store.add_storage_sample(&s.id, &sample) {
                warn!(error = %e, "recording storage");
            }
        }
    }

    /// Make everything durable and uploaded (shutdown, tests).
    pub async fn sync_all(&self) {
        for (_, _, _, log) in self.loaded_partitions() {
            let _ = tokio::task::spawn_blocking(move || {
                log.flush()?;
                log.mark_clean()
            })
            .await;
        }
        self.upload_once(true).await;
        self.flush_usage();
    }
}

pub fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

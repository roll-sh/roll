//! `Cluster`: one loaded tenant — its identity, its published metadata state and the store the
//! state is persisted to.
//!
//! Reads take the current `Arc<ClusterState>` (lock held only to clone the `Arc`). Writes are
//! serialized by an async mutex: [`Cluster::update`] clones the state, applies the change,
//! persists the new state through the [`MetaStore`] and only then publishes it, so a failed
//! write leaves the cluster unchanged and nothing is acknowledged before it is durable.

use crate::error::{EsError, EsResult};
use crate::json::{self, Value};
use crate::meta::ClusterState;
use crate::meta::names::random_uuid;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Where a cluster's metadata lives. `search-store` implements this over object storage;
/// [`MemoryStore`] keeps it in memory (tests, the dev server).
#[async_trait]
pub trait MetaStore: Send + Sync {
    /// The last saved state, if any.
    async fn load(&self) -> anyhow::Result<Option<Bytes>>;
    /// Saves a new state; `version` increases with every save.
    async fn save(&self, version: u64, state: Bytes) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct MemoryStore {
    data: parking_lot::Mutex<Option<Bytes>>,
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }
}

#[async_trait]
impl MetaStore for MemoryStore {
    async fn load(&self) -> anyhow::Result<Option<Bytes>> {
        Ok(self.data.lock().clone())
    }

    async fn save(&self, _version: u64, state: Bytes) -> anyhow::Result<()> {
        *self.data.lock() = Some(state);
        Ok(())
    }
}

/// The REST dialect a cluster speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Elasticsearch,
    OpenSearch,
}

/// The single node a cluster reports.
#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub name: String,
    pub id: String,
    pub ephemeral_id: String,
    pub host: String,
    pub ip: String,
    pub transport_address: String,
    pub http_address: String,
}

impl NodeInfo {
    pub fn new(name: &str) -> NodeInfo {
        NodeInfo {
            name: name.to_string(),
            id: random_uuid(),
            ephemeral_id: random_uuid(),
            host: "127.0.0.1".into(),
            ip: "127.0.0.1".into(),
            transport_address: "127.0.0.1:9300".into(),
            http_address: "127.0.0.1:9200".into(),
        }
    }
}

/// How to open a cluster.
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    pub name: String,
    pub dialect: Dialect,
    pub node_name: String,
    /// `action.destructive_requires_name`.
    pub destructive_requires_name: bool,
    /// Node settings (Elasticsearch's `elasticsearch.yml` / `-E`), e.g. `node.attr.rack` (node
    /// attributes) or `node.processors`. They show in node info, `_cat/nodeattrs` and the
    /// cluster settings defaults as on Elasticsearch.
    pub node_settings: Vec<(String, String)>,
    /// Publish Elasticsearch's test cluster features too (`-Dtests.testfeatures.enabled`, as
    /// the clusters the YAML REST suites run against do).
    pub test_features: bool,
    /// Report the host's numbers (memory, load, file descriptors, disk) in node stats. Off on
    /// shared nodes, where they would reveal other tenants' activity and the host's layout: the
    /// node then reports fixed synthetic figures.
    pub host_metrics: bool,
}

impl ClusterConfig {
    pub fn new(name: &str) -> ClusterConfig {
        ClusterConfig {
            name: name.to_string(),
            dialect: Dialect::Elasticsearch,
            node_name: name.to_string(),
            destructive_requires_name: true,
            node_settings: Vec::new(),
            test_features: false,
            host_metrics: false,
        }
    }

    /// A node setting's value.
    pub fn node_setting(&self, key: &str) -> Option<&str> {
        self.node_settings.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// `node.processors` (allocated processors), when set.
    pub fn processors(&self) -> Option<usize> {
        self.node_setting("node.processors").and_then(|v| v.parse::<f64>().ok()).filter(|p| *p > 0.0).map(|p| p.ceil() as usize)
    }
}

pub struct Cluster {
    pub name: String,
    pub dialect: Dialect,
    pub node: NodeInfo,
    pub config: ClusterConfig,
    state: RwLock<Arc<ClusterState>>,
    write: tokio::sync::Mutex<()>,
    store: Arc<dyn MetaStore>,
    started: std::time::Instant,
    /// The published state version, for requests waiting on a change (`wait_for_status`).
    changes: tokio::sync::watch::Sender<u64>,
    /// Built analysis chains of this cluster's indices.
    pub analysis: crate::analysis::AnalysisCache,
    /// Running tasks (`_tasks`).
    pub tasks: TaskRegistry,
    /// Shard operation counters for `_stats` (`_flush`, `_refresh`).
    pub ops: ShardOps,
}

/// Why an index's primaries are unassigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unassigned {
    /// The shard failed to start (and allocation gave up retrying).
    AllocationFailed,
    IndexAllocationDisabled,
    ClusterAllocationDisabled,
}

/// Counts of one shard's flushes and refreshes since it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpCounts {
    pub flush: u64,
    pub refresh: u64,
    pub external_refresh: u64,
}

impl Default for OpCounts {
    /// A started shard has refreshed twice (once internally and once for searchers), as
    /// Elasticsearch's shards report.
    fn default() -> Self {
        OpCounts { flush: 0, refresh: 2, external_refresh: 2 }
    }
}

/// Per-shard operation counters by index uuid. Only shards that ran an operation have an entry;
/// closing an index forgets its counters (its shards restart on open).
#[derive(Default)]
pub struct ShardOps {
    inner: parking_lot::Mutex<HashMap<String, Vec<OpCounts>>>,
}

impl ShardOps {
    pub fn get(&self, uuid: &str, shard: usize) -> OpCounts {
        self.inner.lock().get(uuid).and_then(|v| v.get(shard).copied()).unwrap_or_default()
    }

    /// Applies `f` to every shard of an index of `shards` shards.
    pub fn record(&self, uuid: &str, shards: usize, f: impl Fn(&mut OpCounts)) {
        let mut g = self.inner.lock();
        let v = g.entry(uuid.to_string()).or_default();
        if v.len() < shards {
            v.resize(shards, OpCounts::default());
        }
        v.iter_mut().for_each(f);
    }

    pub fn forget(&self, uuid: &str) {
        self.inner.lock().remove(uuid);
    }

    /// Drops the entries of indices no longer in `live` (uuids).
    pub fn retain(&self, live: impl Fn(&str) -> bool) {
        self.inner.lock().retain(|k, _| live(k));
    }
}

/// A running task, as `_tasks` reports it.
#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub id: u64,
    pub action: String,
    pub description: String,
    pub start_ms: i64,
    pub started: std::time::Instant,
    pub cancellable: bool,
    pub parent: Option<u64>,
    /// Request headers tasks carry (`X-Opaque-Id`, `traceparent`).
    pub headers: Vec<(String, String)>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
}

/// The tasks running on the node for this cluster. Ids increase from 1 like Elasticsearch's;
/// a task is registered for as long as its guard lives. Tasks started with
/// `wait_for_completion=false` keep their outcome afterwards ([`TaskRegistry::store_result`]),
/// as Elasticsearch keeps them in the `.tasks` index, so `GET _tasks/<id>` can report it.
#[derive(Default, Debug)]
pub struct TaskRegistry {
    next: std::sync::atomic::AtomicU64,
    running: parking_lot::Mutex<std::collections::BTreeMap<u64, TaskInfo>>,
    results: parking_lot::Mutex<std::collections::BTreeMap<u64, StoredTask>>,
}

/// A finished task's record: the task as it ran and its response or failure.
#[derive(Clone, Debug)]
pub struct StoredTask {
    pub info: TaskInfo,
    pub running_nanos: i64,
    pub outcome: Result<Value, crate::error::EsError>,
}

/// Stored results kept per cluster (oldest dropped first), bounding memory.
const MAX_STORED_TASKS: usize = 1000;

/// Unregisters its task when dropped.
pub struct TaskGuard<'a> {
    registry: &'a TaskRegistry,
    pub id: u64,
}

impl Drop for TaskGuard<'_> {
    fn drop(&mut self) {
        self.registry.running.lock().remove(&self.id);
    }
}

impl TaskRegistry {
    pub fn register(&self, action: &str, description: &str, parent: Option<u64>, headers: Vec<(String, String)>) -> TaskGuard<'_> {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let info = TaskInfo {
            id,
            action: action.to_string(),
            description: description.to_string(),
            start_ms: crate::time::now_millis(),
            started: std::time::Instant::now(),
            cancellable: true,
            parent,
            headers,
            cancelled: Default::default(),
        };
        self.running.lock().insert(id, info);
        TaskGuard { registry: self, id }
    }

    /// A snapshot of the running tasks, by id.
    pub fn list(&self) -> Vec<TaskInfo> {
        self.running.lock().values().cloned().collect()
    }

    pub fn get(&self, id: u64) -> Option<TaskInfo> {
        self.running.lock().get(&id).cloned()
    }

    /// Runs `f` as a non-cancellable task and keeps its outcome; returns the task id.
    pub fn run_stored(&self, action: &str, description: &str, headers: Vec<(String, String)>, f: impl FnOnce() -> Result<Value, crate::error::EsError>) -> u64 {
        let guard = self.start_stored(action, description, headers);
        let outcome = f();
        self.finish(guard, outcome)
    }

    /// Registers a non-cancellable task whose outcome [`TaskRegistry::finish`] keeps.
    pub fn start_stored(&self, action: &str, description: &str, headers: Vec<(String, String)>) -> TaskGuard<'_> {
        let guard = self.register(action, description, None, headers);
        if let Some(t) = self.running.lock().get_mut(&guard.id) {
            t.cancellable = false;
        }
        guard
    }

    pub fn finish(&self, guard: TaskGuard<'_>, outcome: Result<Value, crate::error::EsError>) -> u64 {
        let id = guard.id;
        let info = self.get(id).expect("registered");
        drop(guard);
        self.store_result(StoredTask { running_nanos: info.started.elapsed().as_nanos() as i64, info, outcome });
        id
    }

    pub fn store_result(&self, t: StoredTask) {
        let mut r = self.results.lock();
        r.insert(t.info.id, t);
        while r.len() > MAX_STORED_TASKS {
            r.pop_first();
        }
    }

    pub fn stored(&self, id: u64) -> Option<StoredTask> {
        self.results.lock().get(&id).cloned()
    }
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster").field("name", &self.name).finish()
    }
}

impl Cluster {
    /// Loads the cluster's state from `store`, or starts a new one.
    pub async fn open(config: ClusterConfig, store: Arc<dyn MetaStore>) -> anyhow::Result<Cluster> {
        let state = match store.load().await? {
            Some(bytes) => {
                let v = json::parse(&bytes).map_err(|e| anyhow::anyhow!("corrupt cluster state: {e}"))?.unwrap_or(Value::Null);
                ClusterState::from_json_in(&v, config.dialect).map_err(|e| anyhow::anyhow!("corrupt cluster state: {e}"))?
            }
            None => ClusterState::new(&random_uuid()),
        };
        let mut state = ClusterState { dialect: config.dialect, ..state };
        state.metadata.dialect = config.dialect;
        let (changes, _) = tokio::sync::watch::channel(state.version);
        Ok(Cluster {
            changes,
            name: config.name.clone(),
            dialect: config.dialect,
            node: NodeInfo::new(&config.node_name),
            config,
            state: RwLock::new(Arc::new(state)),
            write: tokio::sync::Mutex::new(()),
            store,
            started: std::time::Instant::now(),
            analysis: Default::default(),
            ops: Default::default(),
            tasks: Default::default(),
        })
    }

    /// A cluster with an in-memory store (tests, the dev server).
    pub async fn in_memory(name: &str) -> Cluster {
        Cluster::open(ClusterConfig::new(name), Arc::new(MemoryStore::new())).await.expect("memory store can't fail")
    }

    pub fn uuid(&self) -> String {
        self.state.read().metadata.cluster_uuid.clone()
    }

    /// The current published state.
    pub fn state(&self) -> Arc<ClusterState> {
        self.state.read().clone()
    }

    /// A receiver that wakes on every published state change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// `action.destructive_requires_name` (cluster setting, default `true`; the config gives
    /// the default for clusters that never set it).
    pub fn destructive_requires_name(&self) -> bool {
        let st = self.state();
        let md = &st.metadata;
        match md.transient.get("action.destructive_requires_name").or_else(|| md.persistent.get("action.destructive_requires_name")) {
            Some(v) => v == "true",
            None => self.config.destructive_requires_name,
        }
    }

    /// The analysis chain of an index of `st` (built once per analysis settings change).
    pub fn index_analysis(&self, st: &ClusterState, idx: &crate::meta::IndexMeta) -> EsResult<search_analysis::Analysis> {
        let live = || st.metadata.indices.values().map(|i| i.uuid.clone()).collect();
        let md = &st.metadata;
        let synonyms: Arc<dyn search_analysis::SynonymSets> = Arc::new(crate::meta::synonyms::StateSynonyms(md.synonyms.clone()));
        self.analysis.get(&idx.uuid, &idx.name, &idx.settings, md.synonyms_version, Some(synonyms), md.indices.len(), live)
    }

    /// Why the primaries of `idx` can't start, if they can't: allocation disabled for the index
    /// (`index.routing.allocation.enable: none`) or the cluster, or — roll's only shard start
    /// failure, as in Elasticsearch — an analysis chain that loads a synonyms set when the shard
    /// starts (`synonyms_set`, not validated at create time) and doesn't build (`lenient: false`
    /// with a rule the chain rejects). Such an index is red, has no active shard and is left
    /// out of stats.
    pub fn unassigned_reason(&self, st: &ClusterState, idx: &crate::meta::IndexMeta) -> Option<Unassigned> {
        if idx.is_closed() {
            return None;
        }
        if idx.settings.get("index.routing.allocation.enable").is_some_and(|v| v.eq_ignore_ascii_case("none")) {
            return Some(Unassigned::IndexAllocationDisabled);
        }
        if st.metadata.cluster_setting("cluster.routing.allocation.enable").is_some_and(|v| v.eq_ignore_ascii_case("none")) {
            return Some(Unassigned::ClusterAllocationDisabled);
        }
        if idx.settings.iter().any(|(k, _)| k.starts_with("index.analysis.") && k.ends_with(".synonyms_set")) && self.index_analysis(st, idx).is_err() {
            return Some(Unassigned::AllocationFailed);
        }
        None
    }

    /// Whether the primaries of `idx` are unassigned (see [`Cluster::unassigned_reason`]).
    pub fn shards_failed(&self, st: &ClusterState, idx: &crate::meta::IndexMeta) -> bool {
        self.unassigned_reason(st, idx).is_some()
    }

    pub fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Applies a metadata change: on success the new state is persisted, then published.
    pub async fn update<R>(&self, f: impl FnOnce(&mut ClusterState) -> EsResult<R>) -> EsResult<R> {
        let _guard = self.write.lock().await;
        let mut next = (*self.state()).clone();
        let r = f(&mut next)?;
        next.version += 1;
        next.metadata.version += 1;
        next.state_uuid = random_uuid();
        let bytes = Bytes::from(next.to_json().to_json());
        self.store.save(next.version, bytes).await.map_err(|e| EsError::internal(format!("failed to persist cluster state: {e}")))?;
        let version = next.version;
        *self.state.write() = Arc::new(next);
        self.changes.send_replace(version);
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn updates_persist_and_reload() {
        let store: Arc<dyn MetaStore> = Arc::new(MemoryStore::new());
        let c = Cluster::open(ClusterConfig::new("c"), store.clone()).await.unwrap();
        let uuid = c.uuid();
        c.update(|st| {
            st.metadata.persistent.set("cluster.routing.allocation.enable", "all");
            Ok(())
        })
        .await
        .unwrap();
        let err = c.update(|_| -> EsResult<()> { Err(EsError::illegal_argument("no")) }).await;
        assert!(err.is_err());
        assert_eq!(c.state().version, 2);
        let reopened = Cluster::open(ClusterConfig::new("c"), store).await.unwrap();
        assert_eq!(reopened.uuid(), uuid);
        assert_eq!(reopened.state().metadata.persistent.get("cluster.routing.allocation.enable"), Some("all"));
    }
}

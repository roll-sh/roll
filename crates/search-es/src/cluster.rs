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
    /// Saves a new state; `version` increases with every save. An error that is (or wraps) an
    /// [`EsError`] is what the client gets; any other is a 500 `failed to persist cluster state`.
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
    /// Seals the secrets the cluster stores ([`crate::seal`]); `None`: stored as given.
    pub sealer: Option<std::sync::Arc<crate::seal::Sealer>>,
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
            sealer: None,
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
    /// The indices' shards and field tables.
    pub engine: crate::engine::Engine,
    /// Compiled scripts (Painless, Mustache).
    pub scripts: Arc<crate::scripting::ScriptService>,
    /// Open scrolls and points in time.
    pub contexts: crate::search::contexts::Contexts,
    /// Compiled ingest pipelines.
    pub ingest: crate::ingest::pipeline::Cache,
    /// In-memory state of the x-pack APIs (async results, verified credentials).
    pub xpack: crate::xpack::Runtime,
    /// The circuit breakers (`indices.breaker.*`).
    pub breakers: crate::breaker::Breakers,
    /// The request thread pools (`search`, `write`, `get`, `analyze`): admission and stats.
    pub thread_pools: crate::thread_pool::ThreadPools,
    /// Shard contents of the snapshots this process took.
    pub snapshot_data: crate::api::snapshots::SnapshotData,
    /// Cross-cluster search usage (`_cluster/stats` `ccs`).
    pub ccs: crate::remote::CcsStats,
    /// The k-NN plugin's counters and graph-cache ledger (opensearch dialect).
    pub knn: std::sync::Arc<crate::opensearch::knn_api::KnnRuntime>,
    /// The OpenSearch job scheduler (Index State Management jobs; idle without jobs).
    pub jobs: crate::opensearch::jobs::Jobs,
    /// The cluster's own handle, once its owner allowed background work
    /// ([`Cluster::enable_background_tasks`]).
    background: std::sync::OnceLock<std::sync::Weak<Cluster>>,
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
    pub action: std::borrow::Cow<'static, str>,
    pub description: String,
    pub start_ms: i64,
    pub started: std::time::Instant,
    pub cancellable: bool,
    pub parent: Option<u64>,
    /// Request headers tasks carry (`X-Opaque-Id`, `traceparent`).
    pub headers: Vec<(String, String)>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// A running by-query task's rate and progress (`BulkByScrollTask`).
    pub live: Option<Arc<crate::api::by_query::Live>>,
    /// Builds the description when a listing asks for it (Elasticsearch's description
    /// `Supplier`): a search renders its request only if someone looks. Overrides
    /// `description`.
    pub describe: Option<Describe>,
}

/// A task description rendered on demand ([`TaskInfo::describe`]).
#[derive(Clone)]
pub struct Describe(pub Arc<dyn Fn(&Cluster) -> String + Send + Sync>);

impl std::fmt::Debug for Describe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Describe(..)")
    }
}

impl TaskInfo {
    /// The task's description as `_tasks?detailed` shows it.
    pub fn description_in(&self, cluster: &Cluster) -> String {
        match &self.describe {
            Some(d) => (d.0)(cluster),
            None => self.description.clone(),
        }
    }
}

/// What [`TaskRegistry::start`] registers.
pub struct NewTask {
    pub action: std::borrow::Cow<'static, str>,
    pub description: String,
    pub describe: Option<Describe>,
    pub parent: Option<u64>,
    pub headers: Vec<(String, String)>,
    pub cancellable: bool,
    /// The cancellation flag to share (a search's [`search_index::search::SearchControl`]
    /// flag, or a parent's, whose cancellation bans its children); a new one when `None`.
    pub cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
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
    /// Woken whenever a task ends (`wait_for_completion` on the task APIs).
    ended: tokio::sync::Notify,
    /// Callers in [`TaskRegistry::wait_until`]: ending a task wakes `ended` only when there
    /// are any (a search ends several tasks).
    waiters: std::sync::atomic::AtomicUsize,
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
        self.registry.end(self.id);
    }
}

impl TaskGuard<'_> {
    /// Leaves the task registered without a guard (it is handed to a background run, which
    /// takes it back with [`TaskRegistry::adopt`]).
    pub fn detach(self) -> u64 {
        let id = self.id;
        std::mem::forget(self);
        id
    }
}

impl TaskRegistry {
    pub fn register(&self, action: &str, description: &str, parent: Option<u64>, headers: Vec<(String, String)>) -> TaskGuard<'_> {
        self.start(NewTask { action: action.to_string().into(), description: description.to_string(), describe: None, parent, headers, cancellable: true, cancelled: None })
    }

    /// Registers a task; it runs until the guard drops.
    pub fn start(&self, t: NewTask) -> TaskGuard<'_> {
        let id = self.start_detached(t);
        TaskGuard { registry: self, id }
    }

    /// [`TaskRegistry::start`] without a guard: the caller ends it with [`TaskRegistry::end`]
    /// (guards that must not borrow the registry).
    pub fn start_detached(&self, t: NewTask) -> u64 {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let info = TaskInfo {
            id,
            action: t.action,
            description: t.description,
            start_ms: crate::time::now_millis(),
            started: std::time::Instant::now(),
            cancellable: t.cancellable,
            parent: t.parent,
            headers: t.headers,
            cancelled: t.cancelled.unwrap_or_default(),
            live: None,
            describe: t.describe,
        };
        self.running.lock().insert(id, info);
        id
    }

    /// Ends a task started with [`TaskRegistry::start_detached`].
    pub fn end(&self, id: u64) {
        self.running.lock().remove(&id);
        self.notify_ended();
    }

    fn notify_ended(&self) {
        // A waiter counts itself before it checks the registry, so it either sees this task
        // gone or is counted here.
        if self.waiters.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.ended.notify_waiters();
        }
    }

    /// Ends several tasks at once.
    pub fn end_all(&self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        let mut running = self.running.lock();
        for id in ids {
            running.remove(id);
        }
        drop(running);
        self.notify_ended();
    }

    /// Whether any task runs.
    pub fn is_empty(&self) -> bool {
        self.running.lock().is_empty()
    }

    /// Cancels every cancellable task (a node shutting down); returns how many it cancelled.
    pub fn cancel_all(&self) -> usize {
        let g = self.running.lock();
        let mut n = 0;
        for t in g.values().filter(|t| t.cancellable) {
            if !t.cancelled.swap(true, std::sync::atomic::Ordering::Relaxed) {
                n += 1;
            }
        }
        n
    }

    /// Attaches a by-query task's live state.
    pub fn set_live(&self, id: u64, live: Arc<crate::api::by_query::Live>) {
        if let Some(t) = self.running.lock().get_mut(&id) {
            t.live = Some(live);
        }
    }

    /// The guard of a task [`TaskGuard::detach`]ed earlier.
    pub fn adopt(&self, id: u64) -> TaskGuard<'_> {
        TaskGuard { registry: self, id }
    }

    /// Waits until `done()` holds, re-checking whenever a task ends, at most `timeout`.
    /// Returns whether it held.
    pub async fn wait_until(&self, timeout: std::time::Duration, mut done: impl FnMut() -> bool) -> bool {
        struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.waiters.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _waiting = Waiting(&self.waiters);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let ended = self.ended.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            if done() {
                return true;
            }
            if tokio::time::timeout_at(deadline, ended).await.is_err() {
                return done();
            }
        }
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

    /// Keeps a task's outcome and ends it (the result is visible before the task stops
    /// running, so a waiter never misses both).
    pub fn finish(&self, guard: TaskGuard<'_>, outcome: Result<Value, crate::error::EsError>) -> u64 {
        let id = guard.id;
        self.record(&guard, outcome);
        drop(guard);
        id
    }

    /// Keeps a running task's outcome; the task ends when its guard drops.
    pub fn record(&self, guard: &TaskGuard<'_>, outcome: Result<Value, crate::error::EsError>) {
        let Some(info) = self.get(guard.id) else { return };
        self.store_result(StoredTask { running_nanos: info.started.elapsed().as_nanos() as i64, info, outcome });
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
        // x-pack's stack templates (`logs-*-*` and friends); OpenSearch has none.
        if config.dialect == Dialect::Elasticsearch {
            let on = crate::meta::stack_templates::Registries::of(&config);
            crate::meta::stack_templates::install_missing(&mut state.metadata, on, crate::time::now_millis());
        }
        let (changes, _) = tokio::sync::watch::channel(state.version);
        let thread_pools = crate::thread_pool::ThreadPools::new(&config);
        // Runtime-field grok runs inside Painless, which reads the static node setting from here.
        if config.node_setting("runtime_fields.grok.watchdog.max_execution_time").is_some() {
            let ms = config
                .node_setting("runtime_fields.grok.watchdog.max_execution_time")
                .and_then(|v| crate::units::TimeValue::parse(v, "runtime_fields.grok.watchdog.max_execution_time").ok())
                .map_or(1000, |t| t.as_millis());
            search_painless::es::extract::set_runtime_grok_watchdog((ms > 0).then(|| std::time::Duration::from_millis(ms as u64)));
        }
        Ok(Cluster {
            changes,
            name: config.name.clone(),
            dialect: config.dialect,
            node: {
                let mut n = NodeInfo::new(&config.node_name);
                // `http.publish_host` / `http.publish_port`: the address node info advertises
                // (reindex from remote and clients connect to it).
                if let Some(port) = config.node_setting("http.publish_port") {
                    let host = config.node_setting("http.publish_host").unwrap_or("127.0.0.1");
                    n.http_address = format!("{host}:{port}");
                }
                n
            },
            config,
            state: RwLock::new(Arc::new(state)),
            write: tokio::sync::Mutex::new(()),
            store,
            started: std::time::Instant::now(),
            analysis: Default::default(),
            ops: Default::default(),
            tasks: Default::default(),
            engine: Default::default(),
            scripts: Default::default(),
            contexts: Default::default(),
            ingest: Default::default(),
            xpack: Default::default(),
            breakers: Default::default(),
            thread_pools,
            snapshot_data: Default::default(),
            ccs: Default::default(),
            knn: Default::default(),
            jobs: Default::default(),
            background: Default::default(),
        })
    }

    /// Lets requests run on after their response (`wait_for_completion=false` by-query
    /// tasks): they hold a strong handle while they run. Without it, such tasks run to their
    /// end before the response is sent.
    pub fn enable_background_tasks(self: &Arc<Self>) {
        let _ = self.background.set(Arc::downgrade(self));
        // Jobs due while the cluster was unloaded (Index State Management steps, started
        // transforms' triggers) run now.
        self.jobs.kick(self);
    }

    /// A strong handle for background work, when enabled.
    pub fn handle(&self) -> Option<Arc<Cluster>> {
        self.background.get()?.upgrade()
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
        // OpenSearch's ISM coordinator reacts to every cluster state change (indices created
        // or deleted by any API).
        if next.dialect == Dialect::OpenSearch {
            crate::opensearch::ism::reconcile(&self.state(), &mut next, crate::time::now_millis());
        }
        // x-pack's template registry re-adds a deleted built-in on the next cluster state change
        // (a `DELETE /_index_template/*` doesn't remove `logs` for good); roll re-adds it in the
        // same update, which no client can tell apart.
        if next.dialect == Dialect::Elasticsearch {
            let on = crate::meta::stack_templates::Registries::of(&self.config);
            crate::meta::stack_templates::install_missing(&mut next.metadata, on, crate::time::now_millis());
            // ILM initializes an index that became managed right after the change
            // (`IndexLifecycleService.triggerPolicies` on the applied state).
            crate::xpack::ilm_run::reconcile(&self.state(), &mut next, crate::time::now_millis());
        }
        next.version += 1;
        next.metadata.version += 1;
        next.state_uuid = random_uuid();
        let bytes = Bytes::from(next.to_json().to_json());
        // A store may refuse with an Elasticsearch error of its own (a cluster unloading: 503).
        self.store.save(next.version, bytes).await.map_err(|e| match e.downcast_ref::<EsError>() {
            Some(es) => es.clone(),
            None => EsError::internal(format!("failed to persist cluster state: {e}")),
        })?;
        let version = next.version;
        *self.state.write() = Arc::new(next);
        self.changes.send_replace(version);
        self.jobs.kick(self);
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

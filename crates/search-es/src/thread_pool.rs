//! Elasticsearch's request thread pools as observable behaviour: admission, queueing,
//! rejection and the counters `_cat/thread_pool` and `_nodes/stats` report.
//!
//! roll runs a request's work on its own threads (a search fans out over the shared search
//! pool, writes run where they arrive), so these pools don't own threads. They gate requests
//! the way Elasticsearch's fixed executors gate the work items requests turn into: a pool of
//! `size` slots and a bounded queue in front of it. A request of a pooled API takes a slot for
//! as long as its handler runs; when all are taken it waits in the queue (FIFO), and when the
//! queue is full too it is refused with Elasticsearch's 429 `es_rejected_execution_exception`
//! and the executor's own description as the reason (`EsAbortPolicy`). Each pool keeps the
//! counters Elasticsearch reports: active, queued, largest, completed, rejected, and the task
//! execution time and its EWMA that appear in the rejection text.
//!
//! Sizes follow Elasticsearch 9.5.5 for the node's allocated processors (`node.processors`,
//! else what the node reports), and `thread_pool.<name>.size` / `.queue_size` node settings
//! override them as they do on Elasticsearch. Pools gate one request per slot (Elasticsearch
//! takes one per shard and phase), so a single request never queues behind itself.
//!
//! Cost when a slot is free: one uncontended semaphore acquire and a few relaxed atomics.

use crate::cluster::{ClusterConfig, Dialect};
use crate::error::EsError;
use crate::routes::Api;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// The pools that gate requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolName {
    Search,
    Write,
    Get,
    Analyze,
}

impl PoolName {
    pub fn as_str(self) -> &'static str {
        match self {
            PoolName::Search => "search",
            PoolName::Write => "write",
            PoolName::Get => "get",
            PoolName::Analyze => "analyze",
        }
    }
}

/// The pool a REST API's work runs on (`None`: not gated). Only APIs that hold their pool
/// for a bounded time are gated: an API that waits on something else (a cluster state change,
/// a task, an async search) never ties up a slot meanwhile.
pub fn pool_of(api: Api) -> Option<PoolName> {
    Some(match api {
        Api::Search
        | Api::Msearch
        | Api::Count
        | Api::Scroll
        | Api::SearchTemplate
        | Api::MsearchTemplate
        | Api::KnnSearch
        | Api::OpenPointInTime
        | Api::RankEval
        | Api::SearchMvt
        | Api::TermsEnum
        | Api::FleetSearch
        | Api::FleetMsearch
        | Api::EqlSearch
        | Api::SqlQuery => PoolName::Search,
        Api::Bulk | Api::Index | Api::Create | Api::Update | Api::Delete => PoolName::Write,
        Api::Get | Api::GetSource | Api::Exists | Api::ExistsSource | Api::Mget | Api::Explain | Api::Termvectors | Api::Mtermvectors => PoolName::Get,
        Api::IndicesAnalyze => PoolName::Analyze,
        _ => return None,
    })
}

/// One fixed pool.
pub struct Pool {
    pub name: PoolName,
    /// `<node name>/<pool>`, as the executor names itself.
    executor: String,
    pub size: usize,
    /// `None`: unbounded (`-1`).
    pub queue_size: Option<usize>,
    slots: tokio::sync::Semaphore,
    active: AtomicUsize,
    queued: AtomicUsize,
    largest: AtomicUsize,
    completed: AtomicU64,
    rejected: AtomicU64,
    /// Whether the executor tracks task execution time (`TaskExecutionTimeTrackingEsThreadPoolExecutor`).
    tracked: bool,
    total_nanos: AtomicU64,
    /// The EWMA of execution time in nanoseconds, as `f64` bits.
    ewma: AtomicU64,
    /// The `Object.hashCode` the executor prints, fixed per pool.
    hash: u32,
}

/// What a pool reports (`ThreadPoolStats.Stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub threads: usize,
    pub queue: usize,
    pub active: usize,
    pub rejected: u64,
    pub largest: usize,
    pub completed: u64,
}

/// `TaskTrackingConfig.DEFAULT_EWMA_ALPHA`.
const EWMA_ALPHA: f64 = 0.3;

impl Pool {
    fn new(name: PoolName, node: &str, size: usize, queue_size: Option<usize>, tracked: bool) -> Arc<Pool> {
        let size = size.max(1);
        Arc::new(Pool {
            name,
            executor: format!("{node}/{}", name.as_str()),
            size,
            queue_size,
            slots: tokio::sync::Semaphore::new(size),
            active: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            largest: AtomicUsize::new(0),
            completed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            tracked,
            total_nanos: AtomicU64::new(0),
            ewma: AtomicU64::new(0f64.to_bits()),
            hash: crate::meta::names::random_uuid().bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193)),
        })
    }

    pub fn stats(&self) -> PoolStats {
        let largest = self.largest.load(Ordering::Relaxed);
        PoolStats {
            // A fixed executor keeps the threads it started.
            threads: largest,
            queue: self.queued.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            largest,
            completed: self.completed.load(Ordering::Relaxed),
        }
    }

    /// Takes a slot, waiting in the queue when all are busy; `Err` (counted as rejected) when
    /// the queue is full too.
    pub async fn admit(self: &Arc<Self>) -> Result<Admission, Rejected> {
        let permit = match self.slots.try_acquire() {
            Ok(p) => p,
            Err(_) => {
                let queued = self.queued.fetch_add(1, Ordering::SeqCst);
                if self.queue_size.is_some_and(|q| queued >= q) {
                    self.queued.fetch_sub(1, Ordering::SeqCst);
                    self.rejected.fetch_add(1, Ordering::Relaxed);
                    return Err(Rejected { pool: self.clone(), stats: self.stats_with_queue(queued) });
                }
                // Leaves the queue however the wait ends (the request may be dropped).
                struct Dequeue<'a>(&'a AtomicUsize);
                impl Drop for Dequeue<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                let _dequeue = Dequeue(&self.queued);
                self.slots.acquire().await.expect("pool semaphores are never closed")
            }
        };
        permit.forget();
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.largest.fetch_max(active, Ordering::Relaxed);
        Ok(Admission { pool: self.clone(), started: Instant::now() })
    }

    fn stats_with_queue(&self, queue: usize) -> PoolStats {
        PoolStats { queue, ..self.stats() }
    }

    fn record(&self, nanos: u64) {
        self.completed.fetch_add(1, Ordering::Relaxed);
        if !self.tracked {
            return;
        }
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        let _ = self.ewma.try_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
            let avg = f64::from_bits(bits);
            Some((avg + EWMA_ALPHA * (nanos as f64 - avg)).to_bits())
        });
    }

    /// `EsThreadPoolExecutor.toString()` as the rejection prints it.
    fn describe(&self, stats: PoolStats) -> String {
        let class = if self.tracked { "TaskExecutionTimeTrackingEsThreadPoolExecutor" } else { "EsThreadPoolExecutor" };
        let mut s = format!("{class}[name = {}, ", self.executor);
        if let Some(q) = self.queue_size {
            s.push_str(&format!("queue capacity = {q}, "));
        }
        if self.tracked {
            let ewma = f64::from_bits(self.ewma.load(Ordering::Relaxed)) as i64;
            let nanos = |n: i64| crate::units::TimeValue { duration: n, unit: crate::units::TimeUnit::Nanos }.human();
            s.push_str(&format!("task execution EWMA = {}, total task execution time = {}, ", nanos(ewma), nanos(self.total_nanos.load(Ordering::Relaxed) as i64)));
        }
        s.push_str(&format!(
            "org.elasticsearch.common.util.concurrent.{class}@{:x}[Running, pool size = {}, active threads = {}, queued tasks = {}, completed tasks = {}]]",
            self.hash, stats.threads, stats.active, stats.queue, stats.completed
        ));
        s
    }
}

/// A slot of a pool, given back when dropped.
pub struct Admission {
    pool: Arc<Pool>,
    started: Instant,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.pool.active.fetch_sub(1, Ordering::SeqCst);
        self.pool.record(self.started.elapsed().as_nanos() as u64);
        self.pool.slots.add_permits(1);
    }
}

/// A request a full pool refused.
pub struct Rejected {
    pub pool: Arc<Pool>,
    stats: PoolStats,
}

impl Rejected {
    /// The `es_rejected_execution_exception` (429) for the work item `runnable` describes
    /// (`TimedRunnable{original=…}` as the pool would have run it).
    pub fn error(&self, runnable: &str) -> EsError {
        let nanos = (crate::time::now_millis() as u64).wrapping_mul(1_000_000);
        EsError::new(
            429,
            "es_rejected_execution_exception",
            format!(
                "rejected execution of TimedRunnable{{original={runnable}, creationTimeNanos={nanos}, startTimeNanos=0, finishTimeNanos=-1, failedOrRejected=false}} on {}",
                self.pool.describe(self.stats)
            ),
        )
    }

    /// The rejection of a search's query phase on a shard.
    pub fn search_error(&self) -> EsError {
        self.error(&format!("ActionRunnable#wrap[org.elasticsearch.search.SearchService$$Lambda/0x000000003884e400@{:x}]", lambda_hash()))
    }

    /// The rejection of a bulk's shard-level write (`indices:data/write/bulk[s][p]`).
    pub fn write_error(&self) -> EsError {
        self.error(&format!(
            "org.elasticsearch.action.support.replication.TransportReplicationAction$AsyncPrimaryAction/org.elasticsearch.action.ActionListenerImplementations$RunBeforeActionListener/ChannelActionListener{{TaskTransportChannel{{task=0}}{{DirectResponseChannel{{req=0}}{{indices:data/write/bulk[s][p]}}}}}}/org.elasticsearch.action.support.replication.TransportReplicationAction$$Lambda/0x0000000038766400@{:x}",
            lambda_hash()
        ))
    }

    /// The rejection of a single-shard read (`get`, `mget`, `explain`, term vectors) or `_analyze`.
    pub fn shard_error(&self) -> EsError {
        self.error(&format!("org.elasticsearch.action.support.single.shard.TransportSingleShardAction$AsyncSingleAction$1@{:x}", lambda_hash()))
    }
}

/// A Java identity hash for a rejected work item.
fn lambda_hash() -> u32 {
    static NEXT: AtomicU64 = AtomicU64::new(0x68d2_442d);
    (NEXT.fetch_add(0x9e37_79b9, Ordering::Relaxed) & 0x7fff_ffff) as u32
}

/// The gated pools of a node.
pub struct ThreadPools {
    search: Arc<Pool>,
    write: Arc<Pool>,
    get: Arc<Pool>,
    analyze: Arc<Pool>,
}

impl std::fmt::Debug for ThreadPools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadPools").field("search", &self.search.size).field("write", &self.write.size).finish()
    }
}

/// The processors pools are sized for: `node.processors`, else what the node reports (the
/// host's when it reports host metrics, one otherwise).
pub fn allocated_processors(config: &ClusterConfig) -> usize {
    config.processors().unwrap_or_else(|| if config.host_metrics { search_index::search::pool::cores() } else { 1 })
}

/// `size` and `queue_size` of a gated pool for `p` processors (Elasticsearch 9.5.5; OpenSearch
/// 3.9 for its dialect), before node settings.
pub fn default_size(d: Dialect, name: PoolName, p: usize) -> (usize, Option<usize>) {
    let p = p.max(1);
    let search_or_get = p * 3 / 2 + 1;
    match (d, name) {
        (Dialect::Elasticsearch, PoolName::Search) => (search_or_get, Some(search_or_get * 1000)),
        (Dialect::OpenSearch, PoolName::Search) => (search_or_get, Some(1000)),
        (Dialect::Elasticsearch, PoolName::Write) => (p, Some((750 * p).max(10_000))),
        (Dialect::OpenSearch, PoolName::Write) => (p, Some(10_000)),
        (Dialect::Elasticsearch, PoolName::Get) => (search_or_get, Some(1000)),
        (Dialect::OpenSearch, PoolName::Get) => (p, Some(1000)),
        (_, PoolName::Analyze) => (1, Some(16)),
    }
}

impl ThreadPools {
    pub fn new(config: &ClusterConfig) -> ThreadPools {
        let p = allocated_processors(config);
        let pool = |name: PoolName, tracked: bool| {
            let (mut size, mut queue) = default_size(config.dialect, name, p);
            if let Some(s) = config.node_setting(&format!("thread_pool.{}.size", name.as_str())).and_then(|v| v.trim().parse::<usize>().ok()) {
                size = s.max(1);
            }
            if let Some(q) = config.node_setting(&format!("thread_pool.{}.queue_size", name.as_str())).and_then(|v| v.trim().parse::<i64>().ok()) {
                queue = usize::try_from(q).ok();
            }
            Pool::new(name, &config.node_name, size, queue, tracked)
        };
        ThreadPools {
            search: pool(PoolName::Search, true),
            write: pool(PoolName::Write, true),
            get: pool(PoolName::Get, false),
            analyze: pool(PoolName::Analyze, false),
        }
    }

    pub fn pool(&self, name: PoolName) -> &Arc<Pool> {
        match name {
            PoolName::Search => &self.search,
            PoolName::Write => &self.write,
            PoolName::Get => &self.get,
            PoolName::Analyze => &self.analyze,
        }
    }

    /// A gated pool by name (`_cat/thread_pool`, node stats).
    pub fn by_name(&self, name: &str) -> Option<&Arc<Pool>> {
        [&self.search, &self.write, &self.get, &self.analyze].into_iter().find(|p| p.name.as_str() == name)
    }
}

/// Takes the pool slot a REST request's API needs (none for ungated APIs). A refusal is the
/// reply Elasticsearch gives when that pool rejects the request's work: searches fail every
/// shard (`search_phase_execution_exception`, 429), `_msearch` and `_bulk` answer 200 with
/// each item refused, single-document APIs fail with the rejection itself.
pub(crate) async fn admit_request(ctx: &crate::rest::Ctx<'_>) -> Result<Option<Admission>, crate::error::EsResult<crate::rest::Reply>> {
    let Some(name) = pool_of(ctx.api) else { return Ok(None) };
    match ctx.cluster.thread_pools.pool(name).admit().await {
        Ok(a) => Ok(Some(a)),
        Err(r) => Err(rejection_reply(ctx, &r)),
    }
}

/// The pool a request will need, by its method and path alone (what the server queues for
/// before the request runs, [`crate::rest::handle_admitted`]).
pub fn pool_for<'a>(cluster: &'a crate::cluster::Cluster, method: &str, uri: &str) -> Option<&'a Arc<Pool>> {
    let path = uri.split_once('?').map_or(uri, |(p, _)| p);
    let method = crate::routes::Method::parse(&method.to_ascii_uppercase())?;
    match crate::routes::route_in(cluster.dialect, method, if path.is_empty() { "/" } else { path }) {
        crate::routes::Routed::Found(r) => pool_of(r.api).map(|name| cluster.thread_pools.pool(name)),
        _ => None,
    }
}

pub(crate) fn rejection_reply(ctx: &crate::rest::Ctx<'_>, r: &Rejected) -> crate::error::EsResult<crate::rest::Reply> {
    use crate::json::Value;
    use crate::rest::Reply;
    match ctx.api {
        Api::Msearch | Api::MsearchTemplate => {
            let items = crate::tasks::msearch_items(ctx);
            let responses = items.iter().map(|req| {
                let e = search_rejection(ctx.cluster, req, r);
                crate::obj! { "error" => e.to_value_with_root(false), "status" => 429 }
            });
            Reply::ok(crate::obj! { "took" => 0, "responses" => Value::Arr(responses.collect()) })
        }
        Api::Bulk => {
            let defaults = crate::write::WriteArgs::default();
            let items = crate::api::bulk::parse(&ctx.body, &defaults, ctx.path_param("index"), None, true)?;
            let out = items.iter().map(|it| {
                let id = it.args.id.clone().unwrap_or_else(crate::meta::names::time_based_id);
                let mut m = crate::json::Map::new();
                m.insert("_index".into(), it.index.clone().unwrap_or_default().into());
                m.insert("_id".into(), id.into());
                m.insert("status".into(), 429.into());
                m.insert("error".into(), r.write_error().to_value(false));
                let mut o = crate::json::Map::new();
                o.insert(it.action.name().into(), Value::Obj(m));
                Value::Obj(o)
            });
            Reply::ok(crate::obj! { "errors" => true, "took" => 0, "items" => Value::Arr(out.collect()) })
        }
        Api::Index | Api::Create | Api::Update | Api::Delete => Err(r.write_error()),
        _ if r.pool.name == PoolName::Search => {
            let body = ctx.body()?;
            let req = crate::search::parse_request(ctx, body.as_ref(), ctx.raw_json())?;
            Err(search_rejection(ctx.cluster, &req, r))
        }
        _ => Err(r.shard_error()),
    }
}

/// A search whose every shard's query phase the search pool refused.
fn search_rejection(cluster: &crate::cluster::Cluster, req: &crate::search::SearchRequest, r: &Rejected) -> EsError {
    use crate::json::Value;
    let st = cluster.state();
    let mut failures = Vec::new();
    let mut roots = Vec::new();
    for (idx, _, _) in crate::search::resolve_targets(cluster, &st, req).unwrap_or_default() {
        for shard in 0..idx.number_of_shards() {
            let e = r.search_error();
            failures.push(crate::obj! { "shard" => shard as i64, "index" => idx.name.clone(), "node" => cluster.node.id.clone(), "reason" => e.to_value(false) });
            roots.push(e);
        }
    }
    let first = roots.first().cloned().unwrap_or_else(|| r.search_error());
    EsError::new(429, "search_phase_execution_exception", "all shards failed")
        .with_meta("phase", "query")
        .with_meta("grouped", true)
        .with_meta("failed_shards", Value::Arr(failures))
        .caused_by(first.clone().caused_by(first))
        .with_root_causes(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(settings: &[(&str, &str)]) -> ClusterConfig {
        let mut c = ClusterConfig::new("n");
        c.node_settings = settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        c
    }

    /// `_cat/thread_pool` of Elasticsearch 9.5.5 with `node.processors` 1, 2, 16 and 36.
    #[test]
    fn sizes_match_elasticsearch() {
        let es = Dialect::Elasticsearch;
        for (p, search, write, get) in [(1, (2, 2000), (1, 10_000), (2, 1000)), (2, (4, 4000), (2, 10_000), (4, 1000)), (16, (25, 25_000), (16, 12_000), (25, 1000)), (36, (55, 55_000), (36, 27_000), (55, 1000))] {
            assert_eq!(default_size(es, PoolName::Search, p), (search.0, Some(search.1)), "search at {p}");
            assert_eq!(default_size(es, PoolName::Write, p), (write.0, Some(write.1)), "write at {p}");
            assert_eq!(default_size(es, PoolName::Get, p), (get.0, Some(get.1)), "get at {p}");
            assert_eq!(default_size(es, PoolName::Analyze, p), (1, Some(16)));
        }
        let pools = ThreadPools::new(&config(&[("node.processors", "2"), ("thread_pool.search.size", "3"), ("thread_pool.search.queue_size", "-1")]));
        assert_eq!((pools.search.size, pools.search.queue_size), (3, None));
        assert_eq!((pools.write.size, pools.write.queue_size), (2, Some(10_000)));
    }

    #[tokio::test]
    async fn queues_then_rejects_with_the_executor_text() {
        let pools = ThreadPools::new(&config(&[("thread_pool.search.size", "2"), ("thread_pool.search.queue_size", "1")]));
        let pool = pools.pool(PoolName::Search).clone();
        let a = pool.admit().await.ok().unwrap();
        let b = pool.admit().await.ok().unwrap();
        let p2 = pool.clone();
        let waiter = tokio::spawn(async move { p2.admit().await.map(|_| ()).is_ok() });
        while pool.stats().queue == 0 {
            tokio::task::yield_now().await;
        }
        let Err(r) = pool.admit().await else { panic!("a full queue must reject") };
        let e = r.search_error();
        assert_eq!((e.status, e.kind.as_ref()), (429, "es_rejected_execution_exception"));
        let reason = e.reason();
        assert!(reason.starts_with("rejected execution of TimedRunnable{original=ActionRunnable#wrap[org.elasticsearch.search.SearchService$$Lambda/"), "{reason}");
        assert!(reason.contains("on TaskExecutionTimeTrackingEsThreadPoolExecutor[name = n/search, queue capacity = 1, task execution EWMA = "), "{reason}");
        assert!(reason.ends_with("[Running, pool size = 2, active threads = 2, queued tasks = 1, completed tasks = 0]]"), "{reason}");
        assert_eq!(pool.stats(), PoolStats { threads: 2, queue: 1, active: 2, rejected: 1, largest: 2, completed: 0 });
        drop(a);
        assert!(waiter.await.unwrap(), "the queued request runs once a slot frees");
        drop(b);
        assert_eq!(pool.stats(), PoolStats { threads: 2, queue: 0, active: 0, rejected: 1, largest: 2, completed: 3 });
    }

    #[tokio::test]
    async fn a_dropped_waiter_leaves_the_queue() {
        let pools = ThreadPools::new(&config(&[("thread_pool.get.size", "1"), ("thread_pool.get.queue_size", "1")]));
        let pool = pools.pool(PoolName::Get).clone();
        let a = pool.admit().await.ok().unwrap();
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(20), pool.admit()).await;
        assert!(waiting.is_err());
        assert_eq!(pool.stats().queue, 0);
        drop(a);
        assert!(pool.admit().await.is_ok());
    }
}

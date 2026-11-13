//! `_delete_by_query`, `_update_by_query` and `_reindex` (`AbstractBulkByScrollRequest`):
//! the matching documents of a point-in-time snapshot are read in `_doc` order, in batches of
//! `scroll_size` (`source.size` for reindex), and written back through the shared write path
//! with compare-and-set on the version read (`if_seq_no` / `if_primary_term`), so concurrent
//! changes surface as version conflicts (`conflicts=abort|proceed`). Scripts run in the
//! `update_by_query` / `reindex` contexts (`ctx._source`, `ctx.op`, metadata). The response
//! (`BulkByScrollResponse`) and the task forms (`wait_for_completion=false`, `slices`) follow
//! Elasticsearch.

use crate::api::document;
use crate::api::Reply;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::meta::IndexMeta;
use crate::obj;
use crate::rest::Ctx;
use crate::search::query::QueryCtx;
use crate::write::{self, WriteArgs};
use bytes::Bytes;
use search_index::{Collector, LeafCollector, Scorable, ScoreMode, SegmentContext};
use search_painless::es::write::{CtxKind, ctx_of, ctx_value};
use search_painless::{Context, NoHost};
use std::sync::Arc;

/// A document read for a by-query operation.
pub struct ScanDoc {
    pub index: Arc<IndexMeta>,
    pub id: String,
    pub routing: Option<String>,
    pub seq_no: i64,
    pub primary_term: i64,
    pub version: i64,
    pub source: Option<Bytes>,
}

struct DocIds(Vec<u32>);

struct DocIdsLeaf<'a> {
    out: &'a mut Vec<u32>,
    base: u32,
}

impl LeafCollector for DocIdsLeaf<'_> {
    fn collect(&mut self, doc: u32, _s: &mut dyn Scorable) -> search_index::Result<()> {
        self.out.push(self.base + doc);
        Ok(())
    }
}

impl Collector for DocIds {
    fn score_mode(&self) -> ScoreMode {
        ScoreMode::CompleteNoScores
    }
    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafCollector + 'a>> {
        Ok(Box::new(DocIdsLeaf { out: &mut self.0, base: seg.base }))
    }
}

/// The matching documents of a by-query run, read lazily: each shard's matching doc ids are
/// collected up front on a point-in-time searcher (four bytes a document), the documents
/// themselves a batch at a time, so a run's memory is bounded by its batch size.
///
/// The shards interleave as Elasticsearch's `_doc`-sorted scroll merges them
/// (`SearchPhaseController` sorts the shards' top docs by doc id, ties by shard index, and
/// each shard resumes after its last emitted doc): a k-way merge on `(doc id, shard index)`.
/// Reindex reads the shards one after the other instead (Elasticsearch 9.5.5 hands a reindex
/// every match of the first shard, then of the next one, whatever the query; checked with
/// `max_docs` on two and three shards). With `max_docs` this decides which documents a run
/// touches.
pub struct Scanner {
    shards: Vec<ShardScan>,
    /// Each shard's next document: `(merge key, doc, shard index)`, smallest key first.
    heads: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u32, usize)>>,
    /// Shard by shard (reindex) rather than interleaved by doc id.
    by_shard: bool,
    /// Each shard's read position.
    pos: Vec<usize>,
    /// Documents still to read.
    left: u64,
    need_source: bool,
    /// Documents the run will read (`max_docs` applied).
    pub total: u64,
}

struct ShardScan {
    idx: Arc<IndexMeta>,
    searcher: search_index::Searcher,
    docs: Vec<u32>,
}

impl Scanner {
    /// The next `n` documents (empty when done).
    pub fn next_batch(&mut self, n: usize) -> EsResult<Vec<ScanDoc>> {
        let mut out = Vec::with_capacity(n.min(1024));
        while out.len() < n && self.left > 0 {
            let Some(std::cmp::Reverse((_, doc, si))) = self.heads.pop() else { break };
            self.left -= 1;
            let sh = &self.shards[si];
            self.pos[si] += 1;
            if let Some(&next) = sh.docs.get(self.pos[si]) {
                self.heads.push(std::cmp::Reverse((merge_key(self.by_shard, next, si), next, si)));
            }
            let Some((seg, local)) = sh.searcher.segment_of(doc) else { continue };
            seg.ensure_stored().map_err(crate::engine::convert::index_error)?;
            let stored = sh.searcher.stored(doc).map_err(crate::engine::convert::index_error)?;
            let id = crate::search::hits::stored_id(&stored).unwrap_or_default();
            let source = crate::fetch::source_bytes(&stored).map(Bytes::copy_from_slice);
            if self.need_source && source.is_none() {
                return Err(EsError::illegal_argument(format!("[{}][{id}] didn't store _source", sh.idx.name)));
            }
            out.push(ScanDoc {
                index: sh.idx.clone(),
                routing: crate::fetch::routing(&stored),
                seq_no: crate::search::hits::meta_dv(seg, local, search_index::schema::SEQ_NO_FIELD),
                primary_term: crate::search::hits::meta_dv(seg, local, search_index::schema::PRIMARY_TERM_FIELD),
                version: crate::search::hits::meta_dv(seg, local, search_index::schema::VERSION_FIELD),
                id,
                source,
            });
        }
        Ok(out)
    }
}

/// Where a shard's document `doc` falls in a run's order: by doc id then shard index, or by
/// shard index then doc id.
fn merge_key(by_shard: bool, doc: u32, shard: usize) -> u64 {
    let shard = shard as u64 & 0xFFFF_FFFF;
    if by_shard { shard << 32 | u64::from(doc) } else { u64::from(doc) << 32 | shard }
}

/// Finds the documents matching `query` (up to `max_docs`), shard by shard; `slice` keeps
/// one `_id` slice of every shard, as a sliced scroll does.
///
/// Shard failures fail the whole operation as Elasticsearch's scroll does
/// (`search_phase_execution_exception`, "all shards failed" / "Partial shards failure");
/// `need_source` (reindex, update by query) rejects documents without a stored `_source`;
/// `by_shard` (reindex) reads the shards one after the other (see [`Scanner`]). The search runs
/// as an `indices:data/read/search` child of task `parent` (with its per-shard children), as
/// the scroll an Elasticsearch by-query run opens does.
#[allow(clippy::too_many_arguments)]
pub fn scan(
    ctx: &Ctx,
    indices: &[String],
    query: Option<&crate::query::Query>,
    routing: Option<&str>,
    max_docs: Option<u64>,
    slice: Option<(u32, u32)>,
    need_source: bool,
    by_shard: bool,
    parent: u64,
) -> EsResult<Scanner> {
    let cluster = ctx.cluster;
    let st = cluster.state();
    let req = crate::search::SearchRequest {
        indices: indices.to_vec(),
        opts: ctx.indices_options(crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?,
        source: crate::search_source::SearchSource { query: query.cloned(), ..Default::default() },
        routing: routing.map(str::to_string),
        preference: None,
        dfs: false,
        request_cache: Some(false),
        total_as_int: false,
        typed_keys: false,
        allow_partial: false,
        scroll: None,
        include_named_queries_score: false,
        force_synthetic_source: false,
        fused: None,
        rank_only: false,
        security: crate::xpack::security::dls::ReadSecurity::of(ctx),
        now_ms: ctx.now_ms,
    };
    let targets = crate::search::shard_targets(cluster, &st, &req)?;
    let _tasks = crate::tasks::scan_started(ctx, parent, &req, &targets);
    let mut shards = Vec::new();
    let mut matched = 0u64;
    let mut failures: Vec<(usize, EsError)> = Vec::new();
    let mut succeeded = 0usize;
    // Every shard is scanned: which of them the first `max_docs` documents come from is only
    // known after the merge. No shard contributes more than `max_docs`.
    for (ti, t) in targets.iter().enumerate() {
        match scan_shard(ctx, &st, t, query, slice, max_docs) {
            Ok(sh) => {
                succeeded += 1;
                matched += sh.docs.len() as u64;
                shards.push(sh);
            }
            Err(e) => failures.push((ti, e)),
        }
    }
    if !failures.is_empty() {
        let reason = if succeeded == 0 { "all shards failed" } else { "Partial shards failure" };
        return Err(crate::search::all_shards_failed(&cluster.node.id, &targets, &failures, reason));
    }
    let total = max_docs.map_or(matched, |m| m.min(matched));
    let heads = shards.iter().enumerate().filter_map(|(si, sh)| sh.docs.first().map(|&d| std::cmp::Reverse((merge_key(by_shard, d, si), d, si)))).collect();
    let pos = vec![0; shards.len()];
    Ok(Scanner { shards, heads, by_shard, pos, left: total, need_source, total })
}

/// One shard of [`scan`]: its matching root documents in doc id order, at most `limit`.
fn scan_shard(
    ctx: &Ctx,
    st: &crate::meta::ClusterState,
    t: &crate::search::Target,
    query: Option<&crate::query::Query>,
    slice: Option<(u32, u32)>,
    limit: Option<u64>,
) -> EsResult<ShardScan> {
    use search_index::query::{BooleanQuery, MatchAllQuery, Occur};
    let cluster = ctx.cluster;
    // Each slice is a sliced scroll (`DefaultSearchContext.preProcess`).
    if let Some((_, max)) = slice {
        crate::search::check_slice_count(&t.idx, max as i64)?;
    }
    t.shard.before_search(crate::engine::IndexEngine::refresh_interval(&t.idx))?;
    let searcher = t.shard.searcher();
    let table = t.table();
    let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &searcher, ctx.now_ms);
    use crate::search::query::arc;
    let mut q = match query {
        Some(q) => qctx.to_filter(q).map_err(|e| crate::search::query::shard_exception(&t.idx, e))?,
        None => arc(MatchAllQuery),
    };
    let mut b = BooleanQuery::new().add(Occur::Must, q).add(Occur::Filter, search_index::query::nested::root_filter());
    if let Some(af) = &t.alias_filter {
        b = b.add(Occur::Filter, qctx.to_filter(af)?);
    }
    if let Some((sid, max)) = slice
        && max > 1
    {
        match crate::search::id_slice_query(&qctx, sid, max)? {
            Some(f) => b = b.add(Occur::Filter, f),
            None => return Ok(ShardScan { idx: t.idx.clone(), searcher, docs: Vec::new() }),
        }
    }
    q = arc(b);
    let mut ids = DocIds(Vec::new());
    searcher.search(&q, &mut ids).map_err(crate::engine::convert::index_error)?;
    let mut docs = ids.0;
    docs.sort_unstable();
    if let Some(l) = limit {
        docs.truncate(l.min(usize::MAX as u64) as usize);
    }
    Ok(ShardScan { idx: t.idx.clone(), searcher, docs })
}

/// What a by-query run counted (`BulkByScrollTask.Status`).
#[derive(Default, Clone, Debug)]
pub struct Status {
    pub slice_id: Option<u32>,
    pub total: u64,
    pub updated: u64,
    pub created: u64,
    pub deleted: u64,
    pub batches: u64,
    pub version_conflicts: u64,
    pub noops: u64,
    pub failures: Vec<Value>,
    pub failure_status: u16,
    pub requests_per_second: f64,
    pub throttled_millis: u64,
    /// Milliseconds until the next batch may start (a running, throttled task).
    pub throttled_until_millis: u64,
    /// `canceled`: why the run stopped early.
    pub canceled: Option<String>,
    pub slices: Option<Vec<Option<Status>>>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Delete,
    Update,
    Reindex,
}

/// `requests_per_second` as Elasticsearch prints it (a float; unthrottled is `-1.0`).
fn rps_value(rps: f64) -> Value {
    if rps.is_finite() && rps > 0.0 { Value::Num(Number::Float(crate::fetch::java_float_as_double(rps as f32))) } else { Value::Num(Number::Float(-1.0)) }
}

impl Status {
    /// `Status.innerXContent`: `kind` drops the counters its REST response leaves out (`None`:
    /// all of them, as task statuses and stored results print); `human` adds the readable
    /// `throttled` / `throttled_until` (stored task results).
    fn fields(&self, kind: Option<Kind>, human: bool, m: &mut Map) {
        if let Some(id) = self.slice_id {
            m.insert("slice_id".into(), Value::from(id as i64));
        }
        m.insert("total".into(), Value::from(self.total as i64));
        if kind != Some(Kind::Delete) {
            m.insert("updated".into(), Value::from(self.updated as i64));
        }
        if kind.is_none() || kind == Some(Kind::Reindex) {
            m.insert("created".into(), Value::from(self.created as i64));
        }
        m.insert("deleted".into(), Value::from(self.deleted as i64));
        m.insert("batches".into(), Value::from(self.batches as i64));
        m.insert("version_conflicts".into(), Value::from(self.version_conflicts as i64));
        m.insert("noops".into(), Value::from(self.noops as i64));
        m.insert("retries".into(), obj! { "bulk" => 0, "search" => 0 });
        if human {
            m.insert("throttled".into(), Value::from(crate::units::TimeValue::millis(self.throttled_millis as i64).human()));
        }
        m.insert("throttled_millis".into(), Value::from(self.throttled_millis as i64));
        m.insert("requests_per_second".into(), rps_value(self.requests_per_second));
        if human {
            m.insert("throttled_until".into(), Value::from(crate::units::TimeValue::millis(self.throttled_until_millis as i64).human()));
        }
        m.insert("throttled_until_millis".into(), Value::from(self.throttled_until_millis as i64));
        if let Some(c) = &self.canceled {
            m.insert("canceled".into(), Value::from(c.clone()));
        }
        if let Some(sl) = &self.slices {
            let v: Vec<Value> = sl
                .iter()
                .map(|s| match s {
                    Some(s) => {
                        let mut sm = Map::new();
                        s.fields(kind, human, &mut sm);
                        Value::Obj(sm)
                    }
                    None => Value::Null,
                })
                .collect();
            m.insert("slices".into(), Value::Arr(v));
        }
    }

    /// The task status (`BulkByScrollTask.Status`).
    pub fn status_value(&self) -> Value {
        let mut m = Map::new();
        self.fields(None, false, &mut m);
        Value::Obj(m)
    }

    /// The response (`BulkByScrollResponse`): as the REST API prints it, or as a stored task
    /// result (`full`: every counter and the readable times).
    pub fn response(&self, kind: Kind, took: u64, full: bool) -> Value {
        let mut m = Map::new();
        m.insert("took".into(), Value::from(took as i64));
        m.insert("timed_out".into(), Value::Bool(false));
        self.fields(if full { None } else { Some(kind) }, full, &mut m);
        m.insert("failures".into(), Value::Arr(self.failures.clone()));
        Value::Obj(m)
    }

    /// Adds a slice's counters (`BulkByScrollResponse` of the slices).
    fn add(&mut self, o: &Status) {
        self.total += o.total;
        self.updated += o.updated;
        self.created += o.created;
        self.deleted += o.deleted;
        self.batches += o.batches;
        self.version_conflicts += o.version_conflicts;
        self.noops += o.noops;
        self.throttled_millis += o.throttled_millis;
        self.throttled_until_millis = self.throttled_until_millis.max(o.throttled_until_millis);
        self.failures.extend(o.failures.iter().cloned());
        self.failure_status = self.failure_status.max(o.failure_status);
        if self.canceled.is_none() {
            self.canceled.clone_from(&o.canceled);
        }
    }
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live").field("slice_id", &self.slice_id).field("rps", &self.rate()).finish()
    }
}

/// A by-query task's live state (`BulkByScrollTask` / `WorkerBulkByScrollTaskState`): its
/// rate, which `_rethrottle` changes while it runs, its progress for `GET _tasks`, and the
/// states of its slices (child tasks).
pub struct Live {
    /// The task this run (or slice) reports as; its searches are its children.
    task: u64,
    slice_id: Option<u32>,
    /// Documents per second; not positive or not finite: unthrottled.
    rps: parking_lot::Mutex<f64>,
    changed: tokio::sync::Notify,
    status: parking_lot::Mutex<Status>,
    throttled_until: parking_lot::Mutex<Option<std::time::Instant>>,
    children: parking_lot::Mutex<Vec<Arc<Live>>>,
    /// The task's (and its parent's) cancellation flags.
    cancelled: Vec<Arc<std::sync::atomic::AtomicBool>>,
}

/// A slice's share of a rate.
fn per_slice(rps: f64, n: usize) -> f64 {
    if rps > 0.0 && rps.is_finite() && n > 0 { rps / n as f64 } else { rps }
}

impl Live {
    fn new(task: u64, slice_id: Option<u32>, rps: f64, cancelled: Vec<Arc<std::sync::atomic::AtomicBool>>) -> Arc<Live> {
        Arc::new(Live {
            task,
            slice_id,
            rps: parking_lot::Mutex::new(rps),
            changed: tokio::sync::Notify::new(),
            status: parking_lot::Mutex::new(Status { slice_id, requests_per_second: rps, ..Default::default() }),
            throttled_until: parking_lot::Mutex::new(None),
            children: parking_lot::Mutex::new(Vec::new()),
            cancelled,
        })
    }

    pub fn rate(&self) -> f64 {
        *self.rps.lock()
    }

    /// `_rethrottle`: a faster rate applies to the batch being waited for, a slower one from
    /// the next batch; slices share the rate equally.
    pub fn rethrottle(&self, rps: f64) {
        *self.rps.lock() = rps;
        if !(rps > 0.0 && rps.is_finite()) {
            *self.throttled_until.lock() = None;
        }
        let kids = self.children.lock().clone();
        for k in &kids {
            k.rethrottle(per_slice(rps, kids.len()));
        }
        self.changed.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.iter().any(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The current counters (a sliced task: the sum of its slices, each listed).
    pub fn snapshot(&self) -> Status {
        let kids = self.children.lock().clone();
        let mut s = if kids.is_empty() {
            let mut s = self.status.lock().clone();
            s.throttled_until_millis = self.throttled_until.lock().map(|t| t.saturating_duration_since(std::time::Instant::now()).as_millis() as u64).unwrap_or(0);
            s
        } else {
            let mut s = Status { slice_id: self.slice_id, ..Default::default() };
            let parts: Vec<Status> = kids.iter().map(|k| k.snapshot()).collect();
            for p in &parts {
                s.add(p);
            }
            s.failures.clear();
            s.slices = Some(parts.into_iter().map(Some).collect());
            s
        };
        s.requests_per_second = self.rate();
        s
    }

    /// The task status `GET _tasks` shows.
    pub fn status_value(&self) -> Value {
        self.snapshot().status_value()
    }
}

/// `requests_per_second` throttling (`WorkerBulkByScrollTaskState.throttleWaitTime`): a batch
/// starts no earlier than `last batch size / rps` after the previous one started.
struct Throttle {
    live: Arc<Live>,
    last_start: Option<std::time::Instant>,
    last_size: u64,
    throttled: std::time::Duration,
}

impl Throttle {
    fn new(live: Arc<Live>) -> Throttle {
        Throttle { live, last_start: None, last_size: 0, throttled: std::time::Duration::ZERO }
    }

    /// Waits until a batch of `size` documents may start; `false` when the task was cancelled.
    async fn before_batch(&mut self, size: usize) -> bool {
        if self.live.is_cancelled() {
            return false;
        }
        if let Some(start) = self.last_start {
            let mut deadline: Option<std::time::Instant> = None;
            loop {
                let notified = self.live.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let rps = self.live.rate();
                let want = (rps > 0.0 && rps.is_finite()).then(|| start + std::time::Duration::from_secs_f64((self.last_size as f64 / rps).min(1e10)));
                deadline = match (deadline, want) {
                    (Some(d), Some(w)) => Some(d.min(w)),
                    (_, w) => w,
                };
                let now = std::time::Instant::now();
                let Some(d) = deadline.filter(|d| *d > now) else { break };
                *self.live.throttled_until.lock() = Some(d);
                tokio::select! {
                    _ = tokio::time::sleep_until(d.into()) => {
                        self.throttled += d - now;
                        break;
                    }
                    // Rethrottled: the wait so far isn't counted (only completed delays are).
                    _ = notified => {
                        if self.live.is_cancelled() {
                            *self.live.throttled_until.lock() = None;
                            return false;
                        }
                    }
                }
            }
            *self.live.throttled_until.lock() = None;
        }
        self.last_start = Some(std::time::Instant::now());
        self.last_size = size as u64;
        !self.live.is_cancelled()
    }

    fn millis(&self) -> u64 {
        self.throttled.as_millis() as u64
    }
}

fn failure(index: &str, id: &str, e: &EsError) -> Value {
    obj! { "index" => index, "id" => id, "cause" => e.to_value(false), "status" => e.status as i64 }
}

/// The execution options of a parsed request.
#[derive(Clone, Copy)]
pub struct Opts {
    pub proceed: bool,
    pub max_docs: Option<u64>,
    pub batch: u64,
    pub rps: f64,
    pub slices: u32,
    pub refresh: bool,
}

impl Opts {
    fn of(r: &super::by_query_request::ByScroll) -> Opts {
        Opts { proceed: r.proceed, max_docs: r.limit(), batch: r.batch(), rps: r.rps, slices: r.slices, refresh: r.refresh }
    }
}

tokio::task_local! {
    /// Set while a `wait_for_completion=false` request runs on in the background: its task.
    static BACKGROUND: u64;
}

/// A slice's (or a whole unsliced run's) outcome: counters and the indices written.
type SliceRun = (Status, Vec<String>);

/// Runs a by-query request: now (the response), or as a stored task (`wait_for_completion=
/// false`: the task id now, the work in the background, its result kept in `.tasks`).
///
/// The background run re-dispatches the request on a spawned task holding the cluster; it
/// finds its registered task through [`BACKGROUND`]. Clusters without a background handle
/// ([`crate::Cluster::enable_background_tasks`]) run the task before answering.
async fn run_as_task<'c, F, Fut>(ctx: &'c Ctx<'c>, kind: Kind, rps: f64, action: &str, description: String, f: F) -> EsResult<Reply>
where
    F: FnOnce(u64, Arc<Live>) -> Fut,
    Fut: std::future::Future<Output = EsResult<SliceRun>>,
{
    let tasks = &ctx.cluster.tasks;
    let headers = ctx.request.header("x-opaque-id").map(|v| vec![("X-Opaque-Id".to_string(), v.to_string())]).unwrap_or_default();
    let new_live = |id: u64| {
        let flag = tasks.get(id).map(|t| t.cancelled).unwrap_or_default();
        let live = Live::new(id, None, rps, vec![flag]);
        tasks.set_live(id, live.clone());
        live
    };
    let finish = |live: &Arc<Live>, r: EsResult<SliceRun>, started: std::time::Instant| -> EsResult<(Status, u16)> {
        let (mut s, _) = r?;
        s.requests_per_second = live.rate();
        *live.status.lock() = Status { slices: None, failures: Vec::new(), ..s.clone() };
        let code = if s.failures.is_empty() { 200 } else { s.failure_status };
        let _ = started;
        Ok((s, code))
    };
    if !ctx.bool_param("wait_for_completion", true)? {
        let started = std::time::Instant::now();
        if let Ok(id) = BACKGROUND.try_with(|id| *id) {
            // The background run of a task registered by the request that returned its id.
            let guard = tasks.adopt(id);
            let live = tasks.get(id).and_then(|t| t.live).unwrap_or_else(|| new_live(id));
            let r = f(id, live.clone()).await;
            let outcome = finish(&live, r, started).map(|(s, _)| s.response(kind, started.elapsed().as_millis() as u64, true));
            // Stored in `.tasks` before the task ends: whoever waited for it finds the document.
            tasks.record(&guard, outcome);
            let persisted = super::tasks::persist_result(ctx, id).await;
            drop(guard);
            persisted?;
            return Reply::ok(obj! {});
        }
        let guard = tasks.register(action, &description, None, headers);
        let id = guard.id;
        let live = new_live(id);
        if let Some(cluster) = ctx.cluster.handle() {
            let id = guard.detach();
            let req = ctx.request.clone();
            tokio::spawn(background(cluster, req, id));
            return Reply::ok(obj! { "task" => format!("{}:{id}", ctx.cluster.node.id) });
        }
        let r = f(id, live.clone()).await;
        let outcome = finish(&live, r, started).map(|(s, _)| s.response(kind, started.elapsed().as_millis() as u64, true));
        tasks.record(&guard, outcome);
        let persisted = super::tasks::persist_result(ctx, id).await;
        drop(guard);
        persisted?;
        return Reply::ok(obj! { "task" => format!("{}:{id}", ctx.cluster.node.id) });
    }
    let started = std::time::Instant::now();
    let guard = tasks.register(action, &description, None, headers);
    let live = new_live(guard.id);
    let r = f(guard.id, live.clone()).await;
    let (s, code) = finish(&live, r, started)?;
    drop(guard);
    Ok(Reply::Body(code, s.response(kind, started.elapsed().as_millis() as u64, false)))
}

/// The background run of task `id`: the request dispatched again (boxed: the request handler
/// recurses into itself here).
fn background(cluster: Arc<crate::Cluster>, req: crate::rest::EsRequest, id: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(BACKGROUND.scope(id, async move {
        let _ = crate::rest::handle(&cluster, req).await;
        // A run that failed before reaching its task (the request no longer valid) still
        // ends it.
        if cluster.tasks.get(id).is_some() {
            let g = cluster.tasks.adopt(id);
            cluster.tasks.finish(g, Err(EsError::illegal_state("the background run of the task ended early")));
        }
    }))
}

/// Polls futures to completion together (the slices of a request), in order.
async fn join_all<F: std::future::Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut futs: Vec<std::pin::Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in futs.iter_mut().enumerate() {
            if out[i].is_some() {
                continue;
            }
            match f.as_mut().poll(cx) {
                std::task::Poll::Ready(v) => out[i] = Some(v),
                std::task::Poll::Pending => pending = true,
            }
        }
        if pending { std::task::Poll::Pending } else { std::task::Poll::Ready(()) }
    })
    .await;
    out.into_iter().map(|o| o.expect("polled to completion")).collect()
}

/// The most slice failures an over-sliced request lists as `suppressed`.
const MAX_SUPPRESSED_SLICES: usize = 4096;

/// `slices=auto`: one slice per shard of the source index with the fewest shards.
fn auto_slices(ctx: &Ctx, indices: &[String]) -> u32 {
    let st = ctx.state();
    let opts = ctx.indices_options(crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED).unwrap_or(crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED);
    let names = crate::meta::resolve::resolve(&st, indices, &opts, ctx.now_ms).unwrap_or_default();
    names.iter().filter_map(|n| st.metadata.indices.get(n)).map(|i| i.number_of_shards()).min().unwrap_or(1).max(1)
}

/// Runs `one` once (a manual `slice`, or a single slice) or once per slice as child tasks of
/// `task` (`BulkByScrollParallelizationHelper`): each slice reads its `_id` slice of every
/// shard, gets `max_docs / slices` documents and `rps / slices` documents a second, and the
/// slices' counters add up into the request's, listed per slice.
#[allow(clippy::too_many_arguments)]
async fn run_slices<'c, F, Fut>(
    ctx: &'c Ctx<'c>,
    opts: Opts,
    indices: &[String],
    manual: Option<(u32, u32)>,
    task: u64,
    parent: Arc<Live>,
    action: &str,
    description: &str,
    one: F,
) -> EsResult<SliceRun>
where
    F: Fn(Option<(u32, u32)>, Option<u64>, Arc<Live>) -> Fut,
    Fut: std::future::Future<Output = EsResult<SliceRun>>,
{
    let n = if manual.is_some() { 1 } else if opts.slices == 0 { auto_slices(ctx, indices) } else { opts.slices };
    if n <= 1 {
        return one(manual, opts.max_docs, parent).await;
    }
    // More slices than a source index allows: every slice's scroll fails on that index's
    // shards, so the first one's failure is the request's (and `n` is never allocated for).
    let st = ctx.state();
    let names = crate::meta::resolve::resolve(&st, indices, &ctx.indices_options(crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?, ctx.now_ms).unwrap_or_default();
    if names.iter().filter_map(|x| st.metadata.indices.get(x)).any(|i| i64::from(n) > crate::search::max_slices_per_scroll(i)) {
        let share0 = opts.max_docs.map(|m| m / n as u64 + u64::from(m % n as u64 > 0));
        return match one(Some((0, n)), share0, parent.clone()).await {
            // Every other slice fails the same way; Elasticsearch lists them as suppressed
            // (bounded here: a response can't hold billions).
            Err(e) => {
                let others = (n as usize - 1).min(MAX_SUPPRESSED_SLICES);
                Err(e.clone().with_suppressed(vec![e; others]))
            }
            Ok(_) => Err(EsError::illegal_state(format!("slice 0 of {n} did not fail"))),
        };
    }
    drop(st);
    let tasks = &ctx.cluster.tasks;
    let headers = ctx.request.header("x-opaque-id").map(|v| vec![("X-Opaque-Id".to_string(), v.to_string())]).unwrap_or_default();
    let parent_flag = tasks.get(task).map(|t| t.cancelled).unwrap_or_default();
    let mut guards = Vec::with_capacity(n as usize);
    let mut lives = Vec::with_capacity(n as usize);
    for i in 0..n {
        let g = tasks.register(action, description, Some(task), headers.clone());
        let flag = tasks.get(g.id).map(|t| t.cancelled).unwrap_or_default();
        let live = Live::new(g.id, Some(i), per_slice(parent.rate(), n as usize), vec![flag, parent_flag.clone()]);
        tasks.set_live(g.id, live.clone());
        guards.push(g);
        lives.push(live);
    }
    *parent.children.lock() = lives.clone();
    // `max_docs` split evenly, the remainder going to the first slices.
    let share = |i: u32| opts.max_docs.map(|m| m / n as u64 + u64::from((i as u64) < m % n as u64));
    let futs: Vec<Fut> = (0..n).map(|i| one(Some((i, n)), share(i), lives[i as usize].clone())).collect();
    let results = join_all(futs).await;
    drop(guards);
    let mut total = Status::default();
    let mut parts = Vec::with_capacity(n as usize);
    let mut touched: Vec<String> = Vec::new();
    for (i, r) in results.into_iter().enumerate() {
        let (mut s, t) = r?;
        s.slice_id = Some(i as u32);
        s.requests_per_second = lives[i].rate();
        *lives[i].status.lock() = Status { failures: Vec::new(), ..s.clone() };
        total.add(&s);
        s.failures.clear();
        parts.push(Some(s));
        for u in t {
            if !touched.contains(&u) {
                touched.push(u);
            }
        }
    }
    total.slices = Some(parts);
    Ok((total, touched))
}

/// Records a written index's uuid (the indices `refresh` refreshes).
fn touch(touched: &mut Vec<String>, uuid: &str) {
    if !touched.iter().any(|u| u == uuid) {
        touched.push(uuid.to_string());
    }
}

/// Publishes a run's counters to its task, after each batch.
fn publish(live: &Live, s: &Status, throttle: &Throttle) {
    let mut g = live.status.lock();
    *g = Status { failures: Vec::new(), slices: None, throttled_millis: throttle.millis(), ..s.clone() };
}

/// Runs an update script over a document (`ctx.op`: index, noop, delete).
enum Op {
    /// New source, `_index`, `_id`, `_version`, `_routing` (a `None` metadata value was set
    /// to `null` by the script).
    Index(String, Option<String>, Option<String>, Option<i64>, Option<String>),
    Noop,
    /// `_index`, `_id`, `_routing` after the script.
    Delete(Option<String>, Option<String>, Option<String>),
}

/// A source document as a script sees it.
struct ScriptDoc<'a> {
    index: &'a str,
    id: &'a str,
    routing: Option<&'a str>,
    version: i64,
    source: Option<&'a Bytes>,
}

impl<'a> ScriptDoc<'a> {
    fn of(d: &'a ScanDoc) -> ScriptDoc<'a> {
        ScriptDoc { index: &d.index.name, id: &d.id, routing: d.routing.as_deref(), version: d.version, source: d.source.as_ref() }
    }
}

/// A document a reindex reads (from a local shard or a remote cluster).
struct SourceDoc {
    index: String,
    id: String,
    routing: Option<String>,
    version: i64,
    source: Option<Bytes>,
}

fn run_doc_script(ctx: &Ctx, bound: &crate::scripting::Bound, kind: CtxKind, d: &ScriptDoc) -> EsResult<Op> {
    let Some(sc) = bound.painless() else { return Err(EsError::illegal_argument("unsupported script language")) };
    let raw = d.source.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_else(|| "{}".into());
    let src = search_painless::xcontent::parse_source_unordered(&raw).map_err(|e| EsError::parse(format!("{e:?}")))?;
    use search_painless::Value as PV;
    let mut meta: Vec<(&str, PV)> = vec![
        ("_index", PV::str(d.index)),
        ("_id", PV::str(d.id)),
        ("_version", PV::Long(d.version)),
        ("_routing", d.routing.map(PV::str).unwrap_or(PV::Null)),
        ("op", PV::str("index")),
        ("_now", PV::Long(ctx.now_ms)),
    ];
    // `ReindexMetadata.metadataMap`: `_slice` mirrors `_routing` (slice indexing).
    if kind == CtxKind::Reindex {
        meta.insert(4, ("_slice", d.routing.map(PV::str).unwrap_or(PV::Null)));
    }
    let cv = ctx_value(kind, src, &meta, None).map_err(EsError::illegal_argument)?;
    let mut inst = sc.instance();
    inst.set_params(&bound.params);
    inst.set_var("ctx", cv.clone());
    inst.execute(&mut NoHost).map_err(crate::scripting::script_error)?;
    let c = ctx_of(&cv).ok_or_else(|| EsError::illegal_state("script lost its ctx"))?;
    match c.op().unwrap_or_default().as_str() {
        "noop" | "none" => Ok(Op::Noop),
        _ => {
            let get = |k: &str| -> Option<String> {
                match c.meta_get(k) {
                    PV::Null => None,
                    v => Some(v.to_java_string()),
                }
            };
            if c.op().as_deref() == Some("delete") {
                return Ok(Op::Delete(get("_index"), get("_id"), get("_routing")));
            }
            let s = search_painless::xcontent::to_xcontent(&c.source()).map_err(|e| EsError::illegal_argument(e.reason.clone()))?;
            let version = match c.meta_get("_version") {
                PV::Long(l) => Some(l),
                PV::Int(i) => Some(i as i64),
                _ => None,
            };
            Ok(Op::Index(s, get("_index"), get("_id"), version, get("_routing")))
        }
    }
}

/// `refresh`: the written indices, by uuid.
fn refresh_touched(ctx: &Ctx<'_>, refresh: bool, touched: &[String]) -> EsResult<()> {
    if refresh {
        for u in touched {
            if let Some(e) = ctx.cluster.engine.loaded(u) {
                e.refresh_all()?;
            }
        }
    }
    Ok(())
}

fn description(kind: Kind, indices: &[String], dest: Option<&str>) -> String {
    match kind {
        Kind::Delete => format!("delete-by-query [{}]", indices.join(", ")),
        Kind::Update => format!("update-by-query [{}]", indices.join(", ")),
        Kind::Reindex => format!("reindex from [{}] to [{}]", indices.join(", "), dest.unwrap_or("")),
    }
}

/// The outcome of one write: counted, or stop the slice.
enum Flow {
    Next,
    Stop,
}

/// Counts a failed write (`conflicts=proceed` skips version conflicts).
fn count_failure(s: &mut Status, proceed: bool, index: &str, id: &str, e: &EsError) -> Flow {
    if e.status == 409 {
        s.version_conflicts += 1;
        if proceed {
            return Flow::Next;
        }
        s.failures.push(failure(index, id, e));
        s.failure_status = s.failure_status.max(409);
        return Flow::Stop;
    }
    s.failure_status = s.failure_status.max(e.status);
    s.failures.push(failure(index, id, e));
    Flow::Stop
}

/// `wait_for_active_shards` of a by-query write: the shard's bulk request waits for its
/// copies. The failure names the request (`BulkShardRequest [[index][shard]] containing
/// [<item>]`, the item as `IndexRequest` / `DeleteRequest.toString` print it: `source` is
/// `None` for a delete) and, as a bulk item's cause, carries no index metadata.
async fn check_copies(ctx: &Ctx<'_>, idx: &IndexMeta, id: &str, routing: Option<&str>, source: Option<&[u8]>) -> EsResult<()> {
    let shard = crate::meta::routing::shard_for(idx, id, routing);
    let item = match source {
        None => format!("delete {{[{}][{id}]}}", idx.name),
        Some(src) => {
            // `IndexRequest.MAX_SOURCE_LENGTH_IN_TOSTRING`.
            let shown = if src.len() > 2048 {
                format!("n/a, actual length: [{}], max length: 2kb", crate::units::ByteSize(src.len() as i64).human())
            } else {
                match crate::json::parse(src) {
                    Ok(Some(v)) => v.to_json(),
                    _ => String::from_utf8_lossy(src).into_owned(),
                }
            };
            format!("index {{[{}][{id}], source[{shown}]}}", idx.name)
        }
    };
    let desc = format!("BulkShardRequest [[{}][{shard}]] containing [{item}]", idx.name);
    document::check_active_shards_for(ctx, idx, shard, Some(desc)).await.map_err(|e| EsError::new(e.status, e.kind.clone(), e.reason().to_string()))
}

/// `POST /{index}/_delete_by_query`.
pub async fn delete_by_query(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let req = super::by_query_request::parse_by_query(ctx, false)?;
    let opts = Opts::of(&req);
    let query = req.search.source.query.clone();
    let indices = req.search.indices.clone();
    let routing = req.search.routing.clone();
    let slice = req.slice();
    let action = "indices:data/write/delete/byquery";
    let desc = description(Kind::Delete, &indices, None);
    let d2 = desc.clone();
    run_as_task(ctx, Kind::Delete, opts.rps, action, desc, |task, live| async move {
        let one = |slice: Option<(u32, u32)>, max_docs: Option<u64>, live: Arc<Live>| {
            let (indices, query, routing) = (&indices, &query, &routing);
            async move {
                let mut scanner = scan(ctx, indices, query.as_ref(), routing.as_deref(), max_docs, slice, false, false, live.task)?;
                let mut s = Status { slice_id: live.slice_id, total: scanner.total, ..Default::default() };
                let mut touched = Vec::new();
                let mut throttle = Throttle::new(live.clone());
                let mut stop = false;
                loop {
                    let batch = scanner.next_batch(opts.batch as usize)?;
                    if batch.is_empty() {
                        break;
                    }
                    if !throttle.before_batch(batch.len()).await {
                        s.canceled = Some("by user request".into());
                        break;
                    }
                    s.batches += 1;
                    for d in &batch {
                        let args = WriteArgs { if_seq_no: Some(d.seq_no), if_primary_term: Some(d.primary_term), internal: true, ..Default::default() };
                        let r = match check_copies(ctx, &d.index, &d.id, d.routing.as_deref(), None).await {
                            Ok(()) => write::apply_delete(ctx.cluster, &d.index, &d.id, d.routing.as_deref(), &args),
                            Err(e) => Err(e),
                        };
                        match r {
                            Ok(o) if o.result == "deleted" => {
                                s.deleted += 1;
                                // Only indices written to are refreshed.
                                touch(&mut touched, &d.index.uuid);
                            }
                            Ok(_) => s.version_conflicts += 1,
                            Err(e) => {
                                if let Flow::Stop = count_failure(&mut s, opts.proceed, &d.index.name, &d.id, &e) {
                                    stop = true;
                                }
                            }
                        }
                    }
                    publish(&live, &s, &throttle);
                    // A batch's failures end the run once the whole batch was sent.
                    if stop {
                        break;
                    }
                }
                s.throttled_millis = throttle.millis();
                Ok((s, touched))
            }
        };
        let (s, touched) = run_slices(ctx, opts, &indices, slice, task, live, action, &d2, one).await?;
        refresh_touched(ctx, ctx.bool_param("refresh", false)?, &touched)?;
        Ok((s, touched))
    })
    .await
}

/// `POST /{index}/_update_by_query`.
pub async fn update_by_query(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let req = super::by_query_request::parse_by_query(ctx, true)?;
    let opts = Opts::of(&req);
    let query = req.search.source.query.clone();
    let script = req.script.clone();
    let indices = req.search.indices.clone();
    let routing = req.search.routing.clone();
    let pipeline = req.pipeline.clone();
    let slice = req.slice();
    // The scroll reads versions only when asked (`ctx._version` is -1 otherwise).
    let with_version = ctx.bool_param("version", false)?;
    let action = "indices:data/write/update/byquery";
    let desc = description(Kind::Update, &indices, None);
    let d2 = desc.clone();
    run_as_task(ctx, Kind::Update, opts.rps, action, desc, |task, live| async move {
        let st = ctx.state();
        let bound = match &script {
            Some(sc) => Some(ctx.cluster.scripts.compile(&st, sc, Context::UpdateByQuery)?),
            None => None,
        };
        let one = |slice: Option<(u32, u32)>, max_docs: Option<u64>, live: Arc<Live>| {
            let (indices, query, routing, bound, pipeline) = (&indices, &query, &routing, &bound, &pipeline);
            async move {
                let mut scanner = scan(ctx, indices, query.as_ref(), routing.as_deref(), max_docs, slice, true, false, live.task)?;
                let mut s = Status { slice_id: live.slice_id, total: scanner.total, ..Default::default() };
                let mut touched = Vec::new();
                let mut throttle = Throttle::new(live.clone());
                let mut stop = false;
                loop {
                    let batch = scanner.next_batch(opts.batch as usize)?;
                    if batch.is_empty() {
                        break;
                    }
                    if !throttle.before_batch(batch.len()).await {
                        s.canceled = Some("by user request".into());
                        break;
                    }
                    s.batches += 1;
                    for d in &batch {
                        let source = match bound {
                            Some(b) => match run_doc_script(ctx, b, CtxKind::UpdateByQuery, &ScriptDoc { version: if with_version { d.version } else { -1 }, ..ScriptDoc::of(d) })? {
                                Op::Noop => {
                                    s.noops += 1;
                                    continue;
                                }
                                Op::Delete(..) => {
                                    let args = WriteArgs { if_seq_no: Some(d.seq_no), if_primary_term: Some(d.primary_term), internal: true, ..Default::default() };
                                    match write::apply_delete(ctx.cluster, &d.index, &d.id, d.routing.as_deref(), &args) {
                                        Ok(_) => {
                                            s.deleted += 1;
                                            touch(&mut touched, &d.index.uuid);
                                        }
                                        Err(e) => {
                                            if let Flow::Stop = count_failure(&mut s, opts.proceed, &d.index.name, &d.id, &e) {
                                                stop = true;
                                            }
                                        }
                                    }
                                    continue;
                                }
                                Op::Index(src, _, _, _, _) => Bytes::from(src),
                            },
                            None => d.source.clone().unwrap_or_else(|| Bytes::from_static(b"{}")),
                        };
                        let args = WriteArgs {
                            id: Some(d.id.clone()),
                            routing: d.routing.clone(),
                            if_seq_no: Some(d.seq_no),
                            if_primary_term: Some(d.primary_term),
                            pipeline: pipeline.clone(),
                            internal: true,
                            ..Default::default()
                        };
                        let r = match check_copies(ctx, &d.index, &d.id, d.routing.as_deref(), Some(&source[..])).await {
                            Ok(()) => document::index_one(ctx, &d.index.name, args, source, false).await,
                            Err(e) => Err(e),
                        };
                        match r {
                            Ok(_) => {
                                s.updated += 1;
                                touch(&mut touched, &d.index.uuid);
                            }
                            Err(e) => {
                                if let Flow::Stop = count_failure(&mut s, opts.proceed, &d.index.name, &d.id, &e) {
                                    stop = true;
                                }
                            }
                        }
                    }
                    publish(&live, &s, &throttle);
                    // A batch's failures end the run once the whole batch was sent.
                    if stop {
                        break;
                    }
                }
                s.throttled_millis = throttle.millis();
                Ok((s, touched))
            }
        };
        let (s, touched) = run_slices(ctx, opts, &indices, slice, task, live, action, &d2, one).await?;
        refresh_touched(ctx, ctx.bool_param("refresh", false)?, &touched)?;
        Ok((s, touched))
    })
    .await
}

/// `Reindexer.destinationIndexMode`: the `index.mode` of the destination index, or of the
/// composable template that would create it.
fn destination_mode(st: &crate::meta::ClusterState, dest: &str) -> Option<String> {
    if let Some(idx) = write::resolve_existing(st, dest) {
        return idx.settings.get("index.mode").map(str::to_string);
    }
    let (_, t) = crate::meta::templates::find_composable(&st.metadata, dest, false)?;
    let composed = crate::meta::templates::compose(&st.metadata, t).ok()?;
    composed.settings.get("index.mode").map(str::to_string)
}

/// What a reindex writes besides the documents it reads.
struct ReindexPlan {
    dest_index: String,
    source_fields: Option<(Vec<String>, Vec<String>)>,
    bound: Option<crate::scripting::Bound>,
    op_create: bool,
    version_type: Option<search_index::VersionType>,
    dest_routing: Option<String>,
    pipeline: Option<String>,
    /// `IndexMode.idTransformerForReindex`: time series destinations generate their ids.
    dest_time_series: bool,
    proceed: bool,
}

/// The `_source` a reindex script sees: the document's, filtered by `source._source`.
fn reindex_source(p: &ReindexPlan, d: &SourceDoc) -> EsResult<Bytes> {
    let mut src = d.source.clone().unwrap_or_else(|| Bytes::from_static(b"{}"));
    if let Some((inc, exc)) = &p.source_fields {
        let v = crate::fetch::parse_source(&src)?;
        if let Value::Obj(m) = v {
            src = Bytes::from(Value::Obj(crate::engine::source::SourceFilter::new(inc, exc).apply(&m)).to_json());
        }
    }
    Ok(src)
}

/// `ctx.op = 'delete'` under external versioning: Elasticsearch's delete keeps the version but
/// not its type, so the bulk request's validation rejects it, and with it the whole batch before
/// anything is written. The scripts run once more here, only in that configuration.
fn reject_versioned_deletes<'a>(ctx: &Ctx<'_>, p: &ReindexPlan, docs: impl Iterator<Item = &'a SourceDoc>) -> EsResult<()> {
    let Some(b) = &p.bound else { return Ok(()) };
    if p.version_type.is_none_or(|vt| vt == search_index::VersionType::Internal) {
        return Ok(());
    }
    for d in docs {
        let src = reindex_source(p, d)?;
        let sd = ScriptDoc { index: &d.index, id: &d.id, routing: d.routing.as_deref(), version: d.version, source: Some(&src) };
        if let Op::Delete(..) = run_doc_script(ctx, b, CtxKind::Reindex, &sd)? {
            return Err(EsError::action_request_validation(&[
                "internal versioning can not be used for optimistic concurrency control. Please use `if_seq_no` and `if_primary_term` instead".into(),
            ]));
        }
    }
    Ok(())
}

/// Writes one document read by a reindex.
async fn reindex_doc(ctx: &Ctx<'_>, p: &ReindexPlan, d: &SourceDoc, s: &mut Status, touched: &mut Vec<String>) -> EsResult<Flow> {
    let mut src = reindex_source(p, d)?;
    let mut index = p.dest_index.clone();
    let mut id = d.id.clone();
    let mut routing = match p.dest_routing.as_deref() {
        None | Some("keep") => d.routing.clone(),
        Some("discard") => None,
        Some(r) => Some(r.strip_prefix('=').unwrap_or(r).to_string()),
    };
    let mut version = d.version;
    let mut version_cleared = false;
    if let Some(b) = &p.bound {
        // Versions are read only for external versioning (`needsSourceDocumentVersions`).
        let read_version = p.version_type.is_some_and(|vt| vt != search_index::VersionType::Internal);
        let sd = ScriptDoc { index: &d.index, id: &d.id, routing: d.routing.as_deref(), version: if read_version { d.version } else { -1 }, source: Some(&src) };
        match run_doc_script(ctx, b, CtxKind::Reindex, &sd)? {
            Op::Noop => {
                s.noops += 1;
                return Ok(Flow::Next);
            }
            Op::Delete(ni, nid, nr) => {
                // A delete of the destination document (`ctx.op = 'delete'`).
                let di = match ni {
                    Some(i) if i != d.index => i,
                    _ => p.dest_index.clone(),
                };
                let did = nid.unwrap_or_else(|| d.id.clone());
                let r = if nr != d.routing { nr } else { routing.clone() };
                let st = ctx.state();
                match write::resolve_existing(&st, &di) {
                    Some(meta) => {
                        let args = WriteArgs { internal: true, ..Default::default() };
                        match write::apply_delete(ctx.cluster, &meta, &did, r.as_deref(), &args) {
                            Ok(_) => s.deleted += 1,
                            Err(e) => {
                                s.failure_status = s.failure_status.max(e.status);
                                s.failures.push(failure(&di, &did, &e));
                                return Ok(Flow::Stop);
                            }
                        }
                        touch(touched, &meta.uuid);
                    }
                    None => s.deleted += 1,
                }
                return Ok(Flow::Next);
            }
            Op::Index(ns, ni, nid, nv, nr) => {
                src = Bytes::from(ns);
                if let Some(i) = ni
                    && i != d.index
                {
                    index = i;
                }
                // `ctx._id = null`: an automatic id.
                id = nid.unwrap_or_else(crate::meta::names::time_based_id);
                if nr != d.routing {
                    routing = nr;
                }
                // `ctx._version = null`: internal versioning, overwriting.
                version_cleared = nv.is_none();
                if let Some(v) = nv {
                    version = v;
                }
            }
        }
    }
    let given_id = if p.dest_time_series && id == d.id { None } else { Some(id.clone()) };
    let generated_id = given_id.is_none();
    let mut args = WriteArgs { id: given_id, routing, create: p.op_create, pipeline: p.pipeline.clone(), internal: true, ..Default::default() };
    if let Some(vt) = p.version_type
        && vt != search_index::VersionType::Internal
        && !version_cleared
    {
        args.version_type = Some(vt);
        args.version = Some(version);
    }
    let checked = match write::resolve_existing(&ctx.state(), &index) {
        Some(meta) => check_copies(ctx, &meta, &id, args.routing.as_deref(), Some(&src[..])).await,
        None => Ok(()),
    };
    let r = match checked {
        Ok(()) => document::index_one(ctx, &index, args, src, false).await,
        Err(e) => Err(e),
    };
    match r {
        Ok(o) => {
            if o.result == "created" {
                s.created += 1;
            } else {
                s.updated += 1;
            }
            touch(touched, &o.uuid);
            Ok(Flow::Next)
        }
        Err(e) if e.status == 409 => {
            s.version_conflicts += 1;
            if p.proceed {
                return Ok(Flow::Next);
            }
            s.failures.push(failure(&index, &id, &e));
            s.failure_status = s.failure_status.max(409);
            Ok(Flow::Stop)
        }
        // A destination that may not be auto-created fails the whole request.
        Err(e) if e.kind == "index_not_found_exception" => {
            Err(EsError::new(404, "index_not_found_exception", e.reason().to_string()).with_meta("index_uuid", "_na_").with_meta("index", index.as_str()))
        }
        Err(e) => {
            s.failure_status = s.failure_status.max(e.status);
            let mut f = failure(&index, &id, &e);
            if generated_id && let Value::Obj(m) = &mut f {
                m.shift_remove("id");
            }
            s.failures.push(f);
            Ok(Flow::Stop)
        }
    }
}

/// `POST /_reindex`.
pub async fn reindex(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let req = super::by_query_request::parse_reindex(ctx)?;
    if let Some(r) = &req.remote {
        super::remote::check_whitelist(ctx.cluster.config.node_setting("reindex.remote.whitelist"), r)?;
    }
    let remote = req.remote.clone();
    let remote_req = remote.is_some().then(|| req.clone());
    // `ReindexValidator.initialValidation`: a destination that can't be auto-created, or one of
    // the indices being read, fails before anything is read.
    if let Some(d) = &req.dest.index {
        let st = ctx.state();
        if let Err(e) = write::check_may_auto_create(&st, d) {
            return Err(EsError::new(404, "index_not_found_exception", e.reason().to_string()).with_meta("index_uuid", "_na_").with_meta("index", d.as_str()));
        }
        if remote.is_none()
            && let Some(target) = write::resolve_existing(&st, d)
            && crate::meta::resolve::resolve(&st, &req.search.indices, &req.search.opts, ctx.now_ms).is_ok_and(|src| src.iter().any(|n| *n == target.name))
        {
            return Err(EsError::action_request_validation(&[format!("reindex cannot write into an index its reading from [{}]", target.name)]));
        }
    }
    let opts = Opts::of(&req);
    let src_indices = req.search.indices.clone();
    let dest_index = req.dest.index.clone().unwrap_or_default();
    let query = req.search.source.query.clone();
    let batch = req.batch();
    let source_fields: Option<(Vec<String>, Vec<String>)> =
        req.search.source.source.as_ref().filter(|f| !f.includes.is_empty() || !f.excludes.is_empty()).map(|f| (f.includes.clone(), f.excludes.clone()));
    let script = req.script.clone();
    let slice = req.slice();
    let action = "indices:data/write/reindex";
    let desc = description(Kind::Reindex, &src_indices, Some(&dest_index));
    let d2 = desc.clone();
    run_as_task(ctx, Kind::Reindex, opts.rps, action, desc, |task, live| async move {
        let st = ctx.state();
        let plan = ReindexPlan {
            bound: match &script {
                Some(sc) => Some(ctx.cluster.scripts.compile(&st, sc, Context::Reindex)?),
                None => None,
            },
            dest_time_series: destination_mode(&st, &dest_index).as_deref() == Some("time_series"),
            dest_index: dest_index.clone(),
            source_fields,
            op_create: req.dest.op_type.as_deref() == Some("create"),
            version_type: req.dest.version_type,
            dest_routing: req.dest.routing.clone(),
            pipeline: req.dest.pipeline.clone(),
            proceed: opts.proceed,
        };
        let (s, touched) = match (&remote, &remote_req) {
            // A remote source is read as one scroll; its documents are written in batches.
            (Some(r), Some(rq)) => {
                let (docs, batches) = super::remote::fetch(rq, r, opts.max_docs).await?;
                let mut s = Status { total: docs.len() as u64, batches, ..Default::default() };
                let mut touched = Vec::new();
                let mut throttle = Throttle::new(live.clone());
                let mut stop = false;
                for chunk in docs.chunks(batch.max(1) as usize) {
                    if !throttle.before_batch(chunk.len()).await {
                        s.canceled = Some("by user request".into());
                        break;
                    }
                    let sds: Vec<SourceDoc> = chunk.iter().map(|d| SourceDoc { index: d.index.clone(), id: d.id.clone(), routing: d.routing.clone(), version: d.version, source: d.source.clone() }).collect();
                    reject_versioned_deletes(ctx, &plan, sds.iter())?;
                    for sd in &sds {
                        if let Flow::Stop = reindex_doc(ctx, &plan, sd, &mut s, &mut touched).await? {
                            stop = true;
                        }
                    }
                    publish(&live, &s, &throttle);
                    // A batch's failures end the run once the whole batch was sent.
                    if stop {
                        break;
                    }
                }
                s.throttled_millis = throttle.millis();
                (s, touched)
            }
            _ => {
                let one = |slice: Option<(u32, u32)>, max_docs: Option<u64>, live: Arc<Live>| {
                    let (src_indices, query, plan) = (&src_indices, &query, &plan);
                    async move {
                        let mut scanner = scan(ctx, src_indices, query.as_ref(), None, max_docs, slice, true, true, live.task)?;
                        let mut s = Status { slice_id: live.slice_id, total: scanner.total, ..Default::default() };
                        let mut touched = Vec::new();
                        let mut throttle = Throttle::new(live.clone());
                        let mut stop = false;
                loop {
                            let docs = scanner.next_batch(batch as usize)?;
                            if docs.is_empty() {
                                break;
                            }
                            if !throttle.before_batch(docs.len()).await {
                                s.canceled = Some("by user request".into());
                                break;
                            }
                            s.batches += 1;
                            let sds: Vec<SourceDoc> = docs.into_iter().map(|d| SourceDoc { index: d.index.name.clone(), id: d.id, routing: d.routing, version: d.version, source: d.source }).collect();
                            reject_versioned_deletes(ctx, plan, sds.iter())?;
                            for sd in &sds {
                                if let Flow::Stop = reindex_doc(ctx, plan, sd, &mut s, &mut touched).await? {
                                    stop = true;
                                }
                            }
                            publish(&live, &s, &throttle);
                            if stop {
                                break;
                            }
                        }
                        s.throttled_millis = throttle.millis();
                        Ok((s, touched))
                    }
                };
                run_slices(ctx, opts, &src_indices, slice, task, live, action, &d2, one).await?
            }
        };
        refresh_touched(ctx, opts.refresh, &touched)?;
        Ok((s, touched))
    })
    .await
}

/// `POST _{delete,update}_by_query/<task>/_rethrottle`, `POST _reindex/<task>/_rethrottle`:
/// changes a running task's rate (slices share it) and lists the task with its status.
pub fn rethrottle(ctx: &Ctx) -> EsResult<Reply> {
    let Some(rps) = super::by_query_request::parse_requests_per_second(ctx)? else {
        return Err(EsError::illegal_argument("requests_per_second is a required parameter"));
    };
    let raw = ctx.path_param("task_id").unwrap_or_default().to_string();
    let malformed = || EsError::illegal_argument(format!("malformed task id {raw}"));
    let (node, n) = raw.split_once(':').ok_or_else(malformed)?;
    let n: u64 = n.parse().map_err(|_| malformed())?;
    if node != ctx.cluster.node.id {
        return Reply::ok(super::tasks::node_failure(
            node,
            EsError::new(404, "no_such_node_exception", format!("No such node [{node}]")).with_meta("node_id", node),
        ));
    }
    let Some(t) = ctx.cluster.tasks.get(n) else {
        return Reply::ok(super::tasks::node_failure(node, EsError::resource_not_found(format!("task [{raw}] is missing"))));
    };
    let Some(live) = &t.live else {
        return Reply::ok(super::tasks::node_failure(
            node,
            EsError::illegal_argument(format!("task [{raw}] must be a BulkByScrollTask to be rethrottled but was a [{}]", t.action)),
        ));
    };
    if t.parent.is_some() {
        return Reply::ok(super::tasks::node_failure(node, EsError::illegal_argument(format!("task [{raw}] is a slice of another task and can't be rethrottled"))));
    }
    live.rethrottle(if rps > 0.0 { rps } else { f64::INFINITY });
    let t = ctx.cluster.tasks.get(n).unwrap_or(t);
    Reply::ok(super::tasks::by_node(ctx, &[t], true))
}

#[cfg(test)]
mod tests {
    use super::merge_key;

    /// The orders Elasticsearch 9.5.5 picked `max_docs` documents in (two shards holding
    /// docs 0..3 each): by-query runs interleave by doc id, reindex drains shard 0 first.
    #[test]
    fn merge_keys_interleave_or_go_shard_by_shard() {
        let mut docs: Vec<(u32, usize)> = (0..3).flat_map(|d| [(d, 0), (d, 1)]).collect();
        docs.sort_by_key(|&(d, s)| merge_key(false, d, s));
        assert_eq!(docs, [(0, 0), (0, 1), (1, 0), (1, 1), (2, 0), (2, 1)]);
        docs.sort_by_key(|&(d, s)| merge_key(true, d, s));
        assert_eq!(docs, [(0, 0), (1, 0), (2, 0), (0, 1), (1, 1), (2, 1)]);
        assert!(merge_key(false, u32::MAX, 0) > merge_key(false, 0, usize::MAX));
    }
}

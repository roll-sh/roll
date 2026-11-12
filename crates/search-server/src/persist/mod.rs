//! Durable shards: a loaded cluster's indices kept in `search-store` (WAL, segments, manifests).
//!
//! # Operations as the log
//!
//! search-es applies every write to its `search_index::Shard` and then logs the applied
//! operation through the shard's [`ShardLog`] ([`OpLog`]): an index operation with its source and
//! routing, a delete, or the no-op of a seq_no a failed write used up, each with its `_seq_no`,
//! `_primary_term` and `_version` ([`record`]). The append goes straight into the node's WAL
//! (one short lock: the WAL group-commits the records of every shard of every cluster on the
//! node and ships them to object storage), with no extra copy of the document and no
//! per-request work beyond it. Refresh, flush and merges are left to the engine as in
//! Elasticsearch: a write never cuts a segment, so segment layouts (and therefore scores) follow
//! Elasticsearch's refresh policy.
//!
//! After a request, [`ClusterData::after_request`] acknowledges per
//! `index.translog.durability`: `request` (default) waits for the WAL's ack of the request's
//! records (local fsync, or object storage with `sync_objects`); `async` doesn't wait (the
//! records are committed within the WAL's group-commit window anyway). Concurrent requests
//! share commits: a request waits for the newest LSN of the cluster's request-durability
//! shards, which is at most one group commit later than its own.
//!
//! A shard is *attached* to the WAL (search-store writes a manifest naming this node's WAL) on
//! its first write after a wake; operations logged before that are kept per shard and appended
//! by the next `after_request` (or the background syncer, for writes of background tasks).
//!
//! A **full commit** flushes the shard (`search_index::Shard::flush`: buffers become segment
//! files), uploads new segments and changed live docs, and writes the shard manifest, which trims
//! the WAL. It runs when the shard's log holds `index.translog.flush_threshold_size` or has been
//! uncommitted for `index.translog.flush_threshold_age`, when the shard holds more than
//! [`HEAP_COMMIT_BYTES`] in memory (Lucene flushing its RAM buffer), for the shards a `_flush`
//! (or a flushing force merge) targeted ([`ShardLog::flush_requested`]), when an index closes,
//! before branching and before unloading.
//!
//! # Recovery
//!
//! Waking a cluster reads every shard's latest manifest (one listing) and opens its segments
//! from the cache (fetching misses), then opens each index with its persisted field numbering
//! (`Engine::open_seeded`) and replays the WAL past the manifest's checkpoint into the shard's
//! buffer (`Engine::replay`, Elasticsearch's translog recovery: operations carry their seq_no,
//! term and version; stale or duplicate ones are skipped, so replay is idempotent and any order
//! works). Gaps left by seq_nos that never reached the log are filled with no-ops and the shard
//! is refreshed, so everything acknowledged is searchable after a wake, with the same `_seq_no`,
//! `_version` and `_primary_term`. Delete tombstones younger than `index.gc_deletes` travel in
//! the manifest ([`TOMBSTONES_KEY`]) and are seeded back, so external versions and creates
//! after a delete behave the same across crashes, evictions and handoffs.
//!
//! An uncached segment larger than one fetch part (a new or replaced node waking a big index)
//! isn't waited for: it opens while downloading ([`FetchGate`] over `Store::fetch_segment`), so
//! the shard starts on its metadata and term dictionaries and each query waits only for the
//! postings and stored fields it reads. Segment files written by search-index live in a
//! per-shard directory under the store's staging directory (same filesystem as the cache, so an
//! upload is a rename), which the store empties when it opens; the directory goes when the
//! cluster unloads.
//!
//! # Snapshot contents
//!
//! `ClusterData` is also the engine's [`SnapshotStorage`]: a snapshot commits each shard of its
//! indices and pins that commit point (`Store::write_pin`, `snap.<snapshot uuid>.<index uuid>`),
//! which keeps its segments alive without copying them; a restore opens the pinned segments
//! read-only and indexes their documents with their seq_nos, versions and terms; deleting a
//! snapshot drops its pins. Every shard gets a pin, an empty one for an empty shard, so a pin
//! lost some other way fails the restore instead of restoring the shard empty (snapshots from
//! before this pinned only shards with documents, and still restore). Which pins are garbage is decided by `search_es::api::snapshots`
//! (pins are written before their snapshot is recorded, so an unrecorded pin may belong to a
//! snapshot still being taken): those of a crash between capture and record go once older than
//! a grace period, swept after snapshot operations and in the background after a wake
//! ([`crate::NodeConfig::snapshot_leftover_grace`]). Waking itself never reads pins.
//!
//! # Ownership
//!
//! With leases, acknowledgements also need the cluster's lease to be valid
//! ([`ClusterData::set_lease_deadline`]); the store fences manifests and cluster states by
//! epoch, and a new owner freezes the WAL ranges of the previous one at what exists when it
//! replays them, so a deposed owner's later records never become part of the shard's history.
//! Any failure that leaves memory ahead of what is durable marks the cluster
//! [`ClusterData::failed`]; the node then unloads it.
//!
//! A new owner claims the cluster in object storage before reading it
//! (`Store::claim_cluster`). When acknowledgements wait for object storage (`sync_objects`),
//! each one also needs an ownership check that started after its record was there
//! ([`ClusterData::set_verify_owner`]; concurrent writers share one check): a deposed owner
//! that still believes its lease (a wrong lease store, a paused process) then gets
//! [`PersistError::Lost`] instead of acknowledging a write the new owner can't see. Without
//! `sync_objects` a record lives on local disk until shipped, where no other node could see it
//! anyway: there the lease deadline is the guard, and a check would cost an object-store round
//! trip per local fsync.
//!
//! A cluster closed under requests still running (lease lost, persistence failed, shutdown
//! timed out) acknowledges only writes the WAL already holds: the rest get
//! [`PersistError::Lost`] (503), never a 2xx for a write no log holds.

pub mod record;

use anyhow::Context;
use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use search_codec::{LiveDocs, Slice};
use search_es::engine::{IndexEngine, LoggedOp, ShardEngine, ShardLog, ShardStorage, SnapshotCopy, SnapshotStorage};
use search_es::meta::snapshots::ShardFiles;
use search_es::{Cluster, EsError, EsResult};
use search_index::shard::gate::SegmentGate;
use search_index::{PublishedView, SegmentHandle, Shard, ShardConfig};
use search_store::{SegmentId, SegmentRef, ShardId, ShardManifest, ShardRecovery, Store, WalShard};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Manifest `user_data` key holding the index's field numbering (JSON `[[key, number], ..]`).
pub const NUMBERS_KEY: &str = "roll.field_numbers";
/// Manifest `user_data` key holding the shard's recent delete tombstones (JSON
/// `[[id, seq_no, version, deleted_at_unix_ms], ..]`).
pub const TOMBSTONES_KEY: &str = "roll.tombstones";
/// Most tombstones a manifest keeps (the newest): a delete storm must not make every commit
/// point huge. Older ones are forgotten as if `gc_deletes` had passed.
pub const MAX_TOMBSTONES: usize = 100_000;
/// Manifest `user_data` key holding what the shard's searcher showed of the commit when it was
/// taken, if not everything (search-index's `PublishedView` as JSON `[[hidden position, ..],
/// [[position, [doc deleted since, ..]], ..], [[merged position, [doc not shown, ..]], ..]]`).
/// Instance data: a branch's shards open fully searchable, as new shard copies do.
pub const PUBLISHED_KEY: &str = "roll.instance.published";
/// Manifest `user_data` key holding the shard's cumulative statistics and its index's loaded
/// field data (search-es `IndexEngine::persisted_stats`), so an unload doesn't reset `_stats` as
/// a restart would. Not inherited by branches (`search_store::INSTANCE_DATA_PREFIX`).
pub const STATS_KEY: &str = "roll.instance.stats";
/// Manifest `user_data` key holding the shard's soft-deletes retention lease (JSON `[first
/// retained seq_no, next position, unix ms the next position was taken]`), as Elasticsearch keeps
/// its retention leases in the commit: a reopened shard keeps retaining the deletes it retained,
/// rather than letting merges reclaim them at once.
pub const RETENTION_KEY: &str = "roll.retention";
/// Manifest `user_data` key (shard 0 only, written at unload) holding the synonym rules the
/// index's analysis chain had loaded when they differ from the sets' current ones
/// (`Cluster::loaded_synonyms_json`): the next wake builds with them, as the analyzers of an
/// Elasticsearch node that never restarted still would.
pub const SYNONYMS_KEY: &str = "roll.instance.synonyms";

/// A shard holding more than this in indexing buffers and in-memory segments is committed (its
/// segments become files and go to object storage), like Lucene flushing its RAM buffer.
pub const HEAP_COMMIT_BYTES: u64 = 64 << 20;
/// Commit triggers are checked each time a shard logged this much more…
const CHECK_EVERY: u64 = 8 << 20;
/// …and the log's age at most this often.
const AGE_SCAN_MS: u64 = 1000;

/// `index.translog.flush_threshold_size` default (Elasticsearch 9).
pub const DEFAULT_FLUSH_THRESHOLD: u64 = 10 << 30;
/// `index.translog.flush_threshold_age` default.
pub const DEFAULT_FLUSH_AGE: Duration = Duration::from_secs(60);
/// `index.translog.sync_interval` default.
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(5);

type Key = (String, u32);

/// `index.translog.*` of one index.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Policy {
    request: bool,
    sync_interval: Duration,
    flush_threshold: u64,
    flush_age: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { request: true, sync_interval: DEFAULT_SYNC_INTERVAL, flush_threshold: DEFAULT_FLUSH_THRESHOLD, flush_age: DEFAULT_FLUSH_AGE }
    }
}

impl Policy {
    fn of(idx: &search_es::meta::IndexMeta) -> Policy {
        let s = &idx.settings;
        let mut p = Policy::default();
        if let Some(v) = s.get("index.translog.durability") {
            p.request = !v.eq_ignore_ascii_case("async");
        }
        let time = |k: &str| s.get(k).and_then(|v| search_es::units::TimeValue::parse(v, k).ok()).map(|t| t.as_millis());
        if let Some(ms) = time("index.translog.sync_interval").filter(|ms| *ms > 0) {
            p.sync_interval = Duration::from_millis(ms as u64);
        }
        if let Some(ms) = time("index.translog.flush_threshold_age").filter(|ms| *ms > 0) {
            p.flush_age = Duration::from_millis(ms as u64);
        }
        if let Some(b) = s.get("index.translog.flush_threshold_size").and_then(|v| search_es::units::ByteSize::parse(v, "index.translog.flush_threshold_size").ok())
            && b.bytes() > 0
        {
            p.flush_threshold = b.bytes() as u64;
        }
        p
    }
}

/// Why making writes durable failed.
#[derive(Debug)]
pub enum PersistError {
    /// Object storage is behind (`WalFull`): transient, answer 429.
    Busy(String),
    /// Another node owns the cluster now (fenced, or the lease lapsed): unload without writing.
    Lost(String),
    /// Anything else.
    Failed(anyhow::Error),
}

/// The cluster was unloaded (closed) on this node: nothing more is made durable for it.
#[derive(Debug, Clone, Copy)]
pub struct Unloaded;

impl std::fmt::Display for Unloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the cluster was unloaded from this node")
    }
}

impl std::error::Error for Unloaded {}

fn has<T: std::error::Error + Send + Sync + 'static>(e: &anyhow::Error) -> bool {
    e.downcast_ref::<T>().is_some()
        || e.chain().any(|c| c.downcast_ref::<T>().is_some() || c.downcast_ref::<Shared>().is_some_and(|s| has::<T>(&s.0)))
}

impl PersistError {
    fn from_anyhow(e: anyhow::Error) -> PersistError {
        if has::<Unloaded>(&e) || has::<search_store::Fenced>(&e) {
            PersistError::Lost(format!("{e:#}"))
        } else if has::<search_store::WalFull>(&e) {
            PersistError::Busy(format!("{e:#}"))
        } else {
            PersistError::Failed(e)
        }
    }

    /// The error a client gets.
    pub fn to_es(&self, cluster: &str) -> EsError {
        match self {
            PersistError::Busy(m) => EsError::too_many_requests(format!("rejected execution of write: the write-ahead log is full ({m})")),
            PersistError::Lost(m) => EsError::new(503, "cluster_block_exception", format!("cluster [{cluster}] is no longer owned by this node: {m}")),
            PersistError::Failed(e) => EsError::new(500, "translog_exception", format!("failed to make the write durable: {e:#}")),
        }
    }
}

struct Commit {
    /// Recovered at wake, not attached to the WAL yet (attached on the first write).
    recovery: Option<ShardRecovery>,
    /// The latest commit point.
    manifest: Option<ShardManifest>,
    /// Segments in object storage, as the latest manifest references them.
    uploaded: HashMap<[u8; 16], SegmentRef>,
    /// Field numbers the latest manifest holds.
    numbers_committed: usize,
    /// Operations replayed at wake and not yet in a manifest.
    replayed: u64,
    deleted: bool,
    detached: bool,
}

impl Commit {
    fn empty() -> Commit {
        Commit { recovery: None, manifest: None, uploaded: HashMap::new(), numbers_committed: 0, replayed: 0, deleted: false, detached: false }
    }
}

/// One open shard's persistence state.
pub struct ShardState {
    id: ShardId,
    dir: PathBuf,
    commit: tokio::sync::Mutex<Commit>,
    /// The shard's WAL registration once attached.
    wal: RwLock<Option<WalShard>>,
    /// Operations logged while not attached (or refused by the WAL), appended later.
    pending: Mutex<Vec<(i64, Vec<u8>)>>,
    /// LSN of the last record appended (0: none this run).
    lsn: AtomicU64,
    /// Record bytes logged since the last full commit…
    log_bytes: AtomicU64,
    /// …when the first of them was (ms since the cluster's epoch + 1; 0: none)…
    log_since: AtomicU64,
    /// …and the `log_bytes` at which commit triggers are checked next.
    next_check: AtomicU64,
    /// `index.translog.durability: request` (refreshed when the cluster state changes).
    request: AtomicBool,
    /// Opened at wake, WAL not replayed yet.
    needs_replay: AtomicBool,
    /// A background full commit is queued or running.
    committing: AtomicBool,
    /// The engine flushed the shard (`_flush`): commit it before the request is answered.
    flush_wanted: AtomicBool,
}

impl ShardState {
    fn new(id: ShardId, dir: PathBuf, commit: Commit) -> ShardState {
        ShardState {
            id,
            dir,
            commit: tokio::sync::Mutex::new(commit),
            wal: RwLock::new(None),
            pending: Mutex::new(Vec::new()),
            lsn: AtomicU64::new(0),
            log_bytes: AtomicU64::new(0),
            log_since: AtomicU64::new(0),
            next_check: AtomicU64::new(CHECK_EVERY),
            request: AtomicBool::new(true),
            needs_replay: AtomicBool::new(false),
            committing: AtomicBool::new(false),
            flush_wanted: AtomicBool::new(false),
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }
}

/// A shard recovered at wake, waiting for the engine to open it.
struct Prepared {
    handles: Vec<SegmentHandle>,
    checkpoint: i64,
    primary_term: i64,
    numbers: Vec<(String, u32)>,
    tombstones: Vec<(Bytes, i64, i64, Duration)>,
    /// What the shard's searcher showed of the commit ([`PUBLISHED_KEY`]).
    published: Option<PublishedView>,
    /// The synonym rules the index's analysis chain had loaded ([`SYNONYMS_KEY`]).
    synonyms: Option<String>,
    /// The soft-deletes retention lease ([`RETENTION_KEY`]): from, next, age.
    retention: Option<(i64, i64, Duration)>,
    commit: Commit,
    /// Segments opened while still downloading.
    fetching: usize,
}

/// A segment as a waking shard gets it.
enum Opened {
    Whole(search_store::SegmentData),
    Fetching(search_store::SegmentFetch),
}

/// Whether a shard may open segments that are still downloading: its reads then block on the
/// download, which only a multi-threaded runtime keeps running meanwhile.
fn progressive_fetch_possible() -> bool {
    tokio::runtime::Handle::try_current().is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
}

/// search-index's [`SegmentGate`] over search-store's [`search_store::SegmentFetch`].
///
/// Reads that need bytes still downloading block their thread: on a tokio worker through
/// `block_in_place` (the worker's other tasks move elsewhere, and the download task keeps
/// running), on any other thread (merges, blocking pools) by blocking on the runtime handle.
pub struct FetchGate {
    fetch: search_store::SegmentFetch,
    complete: AtomicBool,
    rt: tokio::runtime::Handle,
}

impl FetchGate {
    fn new(fetch: search_store::SegmentFetch) -> FetchGate {
        FetchGate { fetch, complete: AtomicBool::new(false), rt: tokio::runtime::Handle::current() }
    }
}

impl SegmentGate for FetchGate {
    fn is_ready(&self, range: std::ops::Range<u64>) -> bool {
        self.is_complete() || self.fetch.is_ready(range)
    }

    fn prioritize(&self, ranges: &[std::ops::Range<u64>]) {
        self.fetch.prioritize(ranges);
    }

    fn wait_ready(&self, ranges: &[std::ops::Range<u64>]) -> std::io::Result<()> {
        if ranges.iter().all(|r| self.is_ready(r.clone())) {
            return Ok(());
        }
        let wait = || self.rt.block_on(self.fetch.wait_ready(ranges));
        let r = match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(wait),
            // Inside a current-thread runtime blocking would stall the download itself.
            Ok(_) => return Err(std::io::Error::other("a segment still downloading can't be read on a current-thread runtime")),
            Err(_) => wait(),
        };
        r.map_err(|e| std::io::Error::other(format!("{e:#}")))
    }

    fn is_complete(&self) -> bool {
        if self.complete.load(Ordering::Relaxed) {
            return true;
        }
        let done = self.fetch.data().is_some();
        if done {
            self.complete.store(true, Ordering::Relaxed);
        }
        done
    }
}

/// Counts the cluster's background tasks (the idle node runs none).
#[derive(Default)]
pub struct TaskCount(AtomicUsize);

impl TaskCount {
    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

pub(crate) struct TaskGuard(Arc<TaskCount>);

impl TaskGuard {
    pub(crate) fn new(c: &Arc<TaskCount>) -> TaskGuard {
        c.0.fetch_add(1, Ordering::SeqCst);
        TaskGuard(c.clone())
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An ownership check in object storage, awaited by every writer it covers.
type OwnerCheck = futures::future::Shared<futures::future::BoxFuture<'static, Result<(), Arc<anyhow::Error>>>>;

/// Ownership checks of one cluster running at once.
const MAX_OWNER_CHECKS: usize = 4;

/// A shared error, kept in a chain so its type still classifies it ([`PersistError::from_anyhow`]).
#[derive(Debug)]
struct Shared(Arc<anyhow::Error>);

impl std::fmt::Display for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Shared {}

/// `lease_deadline` once the lease was revoked ([`ClusterData::revoke_lease`]).
const LEASE_REVOKED: u64 = u64::MAX;

/// A loaded cluster's durable storage; the engine's [`ShardStorage`].
pub struct ClusterData {
    me: Weak<ClusterData>,
    store: Store,
    cluster: String,
    dir: PathBuf,
    prepared: Mutex<HashMap<Key, Prepared>>,
    /// Whether `prepared` holds anything or a shard awaits replay (checked before every request).
    has_prepared: AtomicBool,
    /// Serializes opening prepared shards and their replay.
    opening: tokio::sync::Mutex<()>,
    /// Set while [`ClusterData::ready`] opens indices: only it may open a prepared shard.
    opening_now: AtomicBool,
    /// Open shards by index uuid, then shard number.
    shards: Mutex<HashMap<String, HashMap<u32, Arc<ShardState>>>>,
    /// The cluster state version whose deleted indices were last reconciled.
    reconciled: AtomicU64,
    /// The cluster state version whose policies (and closed indices) were last applied.
    policies_at: AtomicU64,
    /// Some index uses `index.translog.durability: async`.
    any_async: AtomicBool,
    /// Uuids of indices closed in the last state applied.
    closed_indices: Mutex<HashSet<String>>,
    /// Some shard holds pending operations.
    has_pending: AtomicBool,
    /// Some shard crossed a commit-check mark.
    check_wanted: AtomicBool,
    /// Some shard was flushed by a request ([`ShardLog::flush_requested`])…
    flush_wanted: AtomicBool,
    /// …and held while such shards commit, so a `_flush` whose shards another request took up
    /// still answers only once they are committed.
    flush_commit: tokio::sync::Mutex<()>,
    /// When logs' ages were last checked (ms since `epoch`).
    age_scan_at: AtomicU64,
    /// The newest LSN appended for this cluster, and the newest one acknowledged.
    last_lsn: AtomicU64,
    acked_lsn: AtomicU64,
    failed: Mutex<Option<String>>,
    closed: AtomicBool,
    /// Wakes background tasks when the cluster closes, so none outlives it.
    closing: tokio::sync::Notify,
    syncer: AtomicBool,
    /// Local deadline of the cluster's lease (ms since `epoch`; 0: no lease).
    lease_deadline: AtomicU64,
    /// Check ownership in object storage before acknowledging ([`ClusterData::set_verify_owner`]).
    verify_owner: AtomicBool,
    /// The newest LSN an ownership check that passed covers…
    owner_checked: AtomicU64,
    /// …and the checks running, with the LSN each covers ([`ClusterData::check_owner`]).
    owner_checks: Mutex<Vec<(u64, OwnerCheck)>>,
    epoch: Instant,
    tasks: Arc<TaskCount>,
    /// Statistics: operations logged, their bytes, full commits, operations replayed at wake.
    pub records: AtomicU64,
    pub record_bytes: AtomicU64,
    pub full_commits: AtomicU64,
    pub replayed: AtomicU64,
}

/// What [`ClusterData::recover`] found.
#[derive(Debug, Default, Clone, Copy)]
pub struct RecoveryStats {
    pub shards: usize,
    pub segments: usize,
    /// Segments opened while still downloading (uncached and larger than one fetch part).
    pub fetching: usize,
    /// Operations replayed from the WAL.
    pub records: u64,
    /// Replayed operations the engine refused (logged; their seq_nos count as no-ops).
    pub failed_records: u64,
    pub orphans: usize,
}

fn es_internal(e: impl std::fmt::Display) -> EsError {
    EsError::internal(e.to_string())
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The shard's tombstones as a manifest keeps them (the newest [`MAX_TOMBSTONES`]).
fn encode_tombstones(mut t: Vec<(Bytes, i64, i64, Duration)>) -> Option<String> {
    if t.is_empty() {
        return None;
    }
    t.sort_by_key(|(_, _, _, age)| *age);
    t.truncate(MAX_TOMBSTONES);
    let now = unix_ms();
    let v: Vec<(String, i64, i64, u64)> = t
        .into_iter()
        .filter_map(|(id, seq, ver, age)| Some((String::from_utf8(id.to_vec()).ok()?, seq, ver, now.saturating_sub(age.as_millis() as u64))))
        .collect();
    Some(serde_json::to_string(&v).expect("tombstones serialize"))
}

fn encode_published(v: &PublishedView) -> String {
    serde_json::to_string(&(&v.hidden, &v.deleted, &v.merged)).expect("view serializes")
}

fn decode_published(m: &ShardManifest) -> anyhow::Result<Option<PublishedView>> {
    let Some(j) = m.user_data.get(PUBLISHED_KEY) else { return Ok(None) };
    type DocLists = Vec<(u32, Vec<u32>)>;
    let (hidden, deleted, merged): (Vec<u32>, DocLists, DocLists) = serde_json::from_str(j).context("published view in a manifest")?;
    Ok(Some(PublishedView { hidden, deleted, merged }))
}

fn decode_tombstones(m: &ShardManifest) -> anyhow::Result<Vec<(Bytes, i64, i64, Duration)>> {
    let Some(j) = m.user_data.get(TOMBSTONES_KEY) else { return Ok(Vec::new()) };
    let v: Vec<(String, i64, i64, u64)> = serde_json::from_str(j).context("tombstones in a manifest")?;
    let now = unix_ms();
    Ok(v.into_iter().map(|(id, seq, ver, at)| (Bytes::from(id), seq, ver, Duration::from_millis(now.saturating_sub(at)))).collect())
}

impl ClusterData {
    pub fn new(store: Store, cluster: &str, tasks: Arc<TaskCount>) -> Arc<ClusterData> {
        let dir = store.staging_dir().join(format!("c-{cluster}"));
        Arc::new_cyclic(|me| ClusterData {
            me: me.clone(),
            store,
            cluster: cluster.to_string(),
            dir,
            prepared: Mutex::new(HashMap::new()),
            has_prepared: AtomicBool::new(false),
            opening: tokio::sync::Mutex::new(()),
            opening_now: AtomicBool::new(false),
            shards: Mutex::new(HashMap::new()),
            reconciled: AtomicU64::new(u64::MAX),
            policies_at: AtomicU64::new(u64::MAX),
            any_async: AtomicBool::new(false),
            closed_indices: Mutex::new(HashSet::new()),
            has_pending: AtomicBool::new(false),
            check_wanted: AtomicBool::new(false),
            flush_wanted: AtomicBool::new(false),
            flush_commit: tokio::sync::Mutex::new(()),
            age_scan_at: AtomicU64::new(0),
            last_lsn: AtomicU64::new(0),
            acked_lsn: AtomicU64::new(0),
            failed: Mutex::new(None),
            closed: AtomicBool::new(false),
            closing: tokio::sync::Notify::new(),
            syncer: AtomicBool::new(false),
            lease_deadline: AtomicU64::new(0),
            verify_owner: AtomicBool::new(false),
            owner_checked: AtomicU64::new(0),
            owner_checks: Mutex::new(Vec::new()),
            epoch: Instant::now(),
            tasks,
            records: AtomicU64::new(0),
            record_bytes: AtomicU64::new(0),
            full_commits: AtomicU64::new(0),
            replayed: AtomicU64::new(0),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn shard_dir(&self, uuid: &str, shard: u32) -> PathBuf {
        self.dir.join(format!("{uuid}-{shard}"))
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    /// Why the cluster must unload, if it must.
    pub fn failed(&self) -> Option<String> {
        self.failed.lock().clone()
    }

    fn fail(&self, why: String) {
        tracing::warn!(cluster = self.cluster, reason = why, "cluster persistence failed; unloading it");
        self.failed.lock().get_or_insert(why);
    }

    /// Acknowledgements need the lease valid until `deadline` (from now; `None`: no lease).
    /// No effect once [`ClusterData::revoke_lease`] ran.
    pub fn set_lease_deadline(&self, deadline: Option<Instant>) {
        let ms = deadline.map_or(0, |d| d.saturating_duration_since(self.epoch).as_millis().max(1) as u64);
        let mut cur = self.lease_deadline.load(Ordering::SeqCst);
        while cur != LEASE_REVOKED {
            match self.lease_deadline.compare_exchange_weak(cur, ms, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(now) => cur = now,
            }
        }
    }

    /// Nothing more is acknowledged, for good (a renewal finishing later doesn't bring it back):
    /// the lease is about to be handed back while requests may still be running.
    pub fn revoke_lease(&self) {
        self.lease_deadline.store(LEASE_REVOKED, Ordering::SeqCst);
    }

    fn lease_valid(&self) -> bool {
        let d = self.lease_deadline.load(Ordering::SeqCst);
        d != LEASE_REVOKED && (d == 0 || (self.epoch.elapsed().as_millis() as u64) < d)
    }

    /// Acknowledge a write only after an ownership check in object storage that started once
    /// its record was durable (see the module docs). For leased clusters on `sync_objects` WALs.
    pub fn set_verify_owner(&self, on: bool) {
        self.verify_owner.store(on, Ordering::SeqCst);
    }

    /// Whether the cluster was closed (unloaded); its writes are no longer made durable.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    // ---------------------------------------------------------------- recovery

    /// Reads every shard's latest commit point and opens its segments, deletes the shards of
    /// indices no longer in `cluster`'s state, then opens every index that has data with its
    /// persisted field numbering and replays the WAL into it ([`ClusterData::ready`]).
    pub async fn recover(self: &Arc<Self>, cluster: &Cluster) -> anyhow::Result<RecoveryStats> {
        let st = cluster.state();
        let live: HashSet<&str> = st.metadata.indices.values().map(|i| i.uuid.as_str()).collect();
        let recs = self.store.recover_shards(&self.cluster).await?;
        let mut stats = RecoveryStats::default();
        let mut todo = Vec::new();
        for rec in recs {
            if !live.contains(rec.shard.index_uuid.as_str()) {
                // The index was deleted, and the node stopped before deleting its shards.
                self.store.delete_shard(&self.cluster, &rec.shard).await?;
                stats.orphans += 1;
                continue;
            }
            todo.push(rec);
        }
        let mut futs = Vec::with_capacity(todo.len());
        for rec in todo {
            futs.push(self.recover_shard(rec));
        }
        let opened = futures::future::join_all(futs).await;
        {
            let mut prepared = self.prepared.lock();
            for p in opened {
                let (key, p) = p?;
                stats.shards += 1;
                stats.segments += p.handles.len();
                stats.fetching += p.fetching;
                prepared.insert(key, p);
            }
            self.has_prepared.store(!prepared.is_empty(), Ordering::SeqCst);
        }
        let (records, failed) = self.ready(cluster).await.map_err(|e| match e {
            PersistError::Failed(e) => e,
            other => anyhow::anyhow!("{}", other.to_es(&self.cluster).reason()),
        })?;
        stats.records = records;
        stats.failed_records = failed;
        Ok(stats)
    }

    async fn recover_shard(&self, rec: ShardRecovery) -> anyhow::Result<(Key, Prepared)> {
        let id = rec.shard.clone();
        let dir = self.shard_dir(&id.index_uuid, id.shard);
        tokio::fs::create_dir_all(&dir).await?;
        let m = rec.manifest.clone().unwrap_or_default();
        let progressive = progressive_fetch_possible();
        let part = self.store.fetch_part();
        // (Futures built in a loop, not an iterator closure: those aren't provably `Send` in a
        // spawned request future.)
        let mut futs = Vec::with_capacity(m.segments.len());
        for r in &m.segments {
            // A segment within one fetch part comes in one request anyway.
            futs.push(self.open_for_wake(&r.id, progressive && r.bytes > part));
        }
        let datas: Vec<Opened> = {
            use futures::StreamExt;
            let opened: Vec<anyhow::Result<Opened>> = futures::stream::iter(futs).buffered(8).collect().await;
            opened.into_iter().collect::<anyhow::Result<_>>().with_context(|| format!("opening the segments of shard {id}"))?
        };
        let mut handles = Vec::with_capacity(datas.len());
        let mut fetching = 0usize;
        for (r, d) in m.segments.iter().zip(datas) {
            let mut h = match d {
                Opened::Whole(d) => {
                    // Already in the store: a placeholder path stops the shard from writing it again.
                    let mut h = SegmentHandle::new(Slice::from_owner(d));
                    h.path = Some(self.store.segment_path(&r.id));
                    h
                }
                Opened::Fetching(f) => {
                    fetching += 1;
                    // SAFETY: the shard reads only ranges the gate reported ready (search-index's
                    // `SegmentHandle::fetching` contract), which is `map_partial`'s requirement.
                    let map = unsafe { f.map_partial() }?;
                    SegmentHandle::fetching(Slice::from_owner(map), Arc::new(FetchGate::new(f)))
                }
            };
            if let Some(g) = r.live {
                let b = self.store.open_live_docs(&r.id, g).await?;
                h.live = Some(LiveDocs::decode(&b, r.id.0).with_context(|| format!("live docs of segment {}", r.id.to_hex()))?.0);
            }
            handles.push(h);
        }
        let numbers: Vec<(String, u32)> = match m.user_data.get(NUMBERS_KEY) {
            Some(j) => serde_json::from_str(j).with_context(|| format!("field numbers of shard {id}"))?,
            None => Vec::new(),
        };
        let tombstones = decode_tombstones(&m).with_context(|| format!("shard {id}"))?;
        let published = decode_published(&m).with_context(|| format!("shard {id}"))?;
        let synonyms = m.user_data.get(SYNONYMS_KEY).cloned();
        let retention = match m.user_data.get(RETENTION_KEY) {
            Some(j) => {
                let (from, next, taken): (i64, i64, u64) = serde_json::from_str(j).with_context(|| format!("retention lease of shard {id}"))?;
                Some((from, next, Duration::from_millis(unix_ms().saturating_sub(taken))))
            }
            None => None,
        };
        let uploaded: HashMap<[u8; 16], SegmentRef> = m.segments.iter().map(|r| (r.id.0, r.clone())).collect();
        let checkpoint = rec.checkpoint();
        let primary_term = m.primary_term.max(1) as i64;
        let commit = Commit { recovery: Some(rec), numbers_committed: numbers.len(), manifest: Some(m), uploaded, replayed: 0, deleted: false, detached: false };
        Ok(((id.index_uuid.clone(), id.shard), Prepared { handles, checkpoint, primary_term, numbers, tombstones, published, synonyms, retention, commit, fetching }))
    }

    /// A segment for a waking shard: whole when cached or not `progressive`, else possibly
    /// still downloading (the shard opens on its metadata and waits per field for the rest).
    async fn open_for_wake(&self, key: &SegmentId, progressive: bool) -> anyhow::Result<Opened> {
        if !progressive || self.store.is_cached(key) {
            return Ok(Opened::Whole(self.store.open_segment(key).await?));
        }
        let f = self.store.fetch_segment(key).await?;
        match f.data() {
            Some(d) => Ok(Opened::Whole(d)),
            None => Ok(Opened::Fetching(f)),
        }
    }

    /// Opens indices whose shards were recovered but not opened yet (with their numbering) and
    /// replays their WAL. Called at wake and before requests while anything is pending (an
    /// index whose analysis failed at wake opens once it builds). Returns the operations
    /// replayed and those the engine refused.
    pub async fn ready(&self, cluster: &Cluster) -> Result<(u64, u64), PersistError> {
        if !self.has_prepared.load(Ordering::SeqCst) {
            return Ok((0, 0));
        }
        let _g = self.opening.lock().await;
        if !self.has_prepared.load(Ordering::SeqCst) {
            return Ok((0, 0));
        }
        let st = cluster.state();
        let mut wanted: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for ((uuid, _), p) in self.prepared.lock().iter_mut() {
            let e = wanted.entry(uuid.clone()).or_default();
            if p.numbers.len() > e.len() {
                *e = p.numbers.clone();
            }
            // Before the index opens: opening builds its analysis chain.
            if let Some(j) = p.synonyms.take()
                && let Err(e) = cluster.restore_loaded_synonyms(uuid, &j)
            {
                tracing::warn!(cluster = self.cluster, index = uuid, error = ?e.reason, "ignoring the persisted synonym rules");
            }
        }
        self.opening_now.store(true, Ordering::SeqCst);
        for idx in st.metadata.indices.values() {
            let Some(numbers) = wanted.get(&idx.uuid) else { continue };
            if let Err(e) = cluster.engine.open_seeded(cluster, &st, idx, numbers) {
                tracing::debug!(cluster = self.cluster, index = idx.name, error = ?e.reason, "index not opened yet");
            }
        }
        self.opening_now.store(false, Ordering::SeqCst);
        let waiting = {
            let mut p = self.prepared.lock();
            // Shards beyond the index's shard count can't be opened: forget them.
            p.retain(|(uuid, n), _| st.metadata.indices.values().any(|i| &i.uuid == uuid && *n < i.number_of_shards()));
            !p.is_empty()
        };
        // Replay every opened shard that waits for it.
        let mut todo = Vec::new();
        for ie in cluster.engine.indices_snapshot() {
            let Some(idx) = st.metadata.indices.values().find(|i| i.uuid == ie.uuid).cloned() else { continue };
            let states: Vec<(u32, Arc<ShardState>)> = match self.shards.lock().get(&ie.uuid) {
                Some(m) => m.iter().filter(|(_, s)| s.needs_replay.load(Ordering::SeqCst)).map(|(n, s)| (*n, s.clone())).collect(),
                None => continue,
            };
            for (n, s) in states {
                todo.push((ie.clone(), idx.clone(), n, s));
            }
        }
        let mut futs = Vec::with_capacity(todo.len());
        for (ie, idx, n, s) in &todo {
            futs.push(self.replay_shard(cluster, ie, idx, *n, s));
        }
        let mut totals = (0, 0);
        for r in futures::future::join_all(futs).await {
            let (a, b) = r?;
            totals.0 += a;
            totals.1 += b;
        }
        self.has_prepared.store(waiting, Ordering::SeqCst);
        Ok(totals)
    }

    /// Replays a woken shard's WAL into it, then fills seq_no gaps and refreshes.
    async fn replay_shard(&self, cluster: &Cluster, ie: &IndexEngine, idx: &search_es::meta::IndexMeta, n: u32, st: &ShardState) -> Result<(u64, u64), PersistError> {
        let mut c = st.commit.lock().await;
        if !st.needs_replay.load(Ordering::SeqCst) {
            return Ok((0, 0));
        }
        let se: &ShardEngine = ie.shard(n);
        // Statistics an earlier instance counted (before replay, which counts nothing).
        if let Some(j) = c.manifest.as_ref().and_then(|m| m.user_data.get(STATS_KEY))
            && let Err(e) = ie.restore_stats(n, j, &cluster.ops, idx.number_of_shards() as usize)
        {
            tracing::warn!(shard = %st.id, error = %e.reason(), "restoring shard statistics failed");
        }
        let mut failed = 0u64;
        let mut records = 0u64;
        if let Some(mut rec) = c.recovery.take() {
            let r = {
                let (failed, records) = (&mut failed, &mut records);
                let engine = &cluster.engine;
                let id = &st.id;
                self.store
                    .replay(&mut rec, &mut |r| {
                        let op = record::decode(r.payload).with_context(|| format!("WAL record at seq_no {} of shard {id}", r.seq_no))?;
                        *records += 1;
                        // Uncommitted, so in the translog again (`translog` stats), as after
                        // an Elasticsearch translog recovery.
                        se.logged(&op);
                        if let Err(e) = engine.replay(idx, n, &op) {
                            // The operation was accepted once; refusing it now means the shard
                            // changed under it. Keep the rest of the history.
                            tracing::error!(shard = %id, seq_no = op.seq_no(), error = %e.reason(), "replaying an operation failed");
                            se.shard.mark_seq_no_processed(op.seq_no());
                            *failed += 1;
                        }
                        Ok(())
                    })
                    .await
            };
            c.recovery = Some(rec);
            r.map_err(|e| PersistError::Failed(e.context(format!("replaying the WAL of shard {}", st.id))))?;
        }
        let filled = se.shard.fill_seq_no_gaps();
        if records == 0 && filled == 0 && se.shard.has_pending_changes() {
            // A graceful unload's commit with writes nobody had refreshed (refreshes disabled
            // or not due yet): the shard opened with the searcher it had, so the wake is
            // invisible, and they become searchable when a refresh would have published them.
            se.reopened_unrefreshed();
        } else {
            // Searchable after a crash or a handoff, as after an Elasticsearch restart.
            if se.shard.refresh().map_err(|e| PersistError::Failed(anyhow::anyhow!("refreshing shard {}: {e}", st.id)))? {
                // Request cache entries carried over describe the searcher before the replay.
                se.clear_request_cache();
            }
        }
        c.replayed = records;
        st.needs_replay.store(false, Ordering::SeqCst);
        self.replayed.fetch_add(records, Ordering::Relaxed);
        Ok((records, failed))
    }

    fn shard_config(&self, dir: PathBuf, checkpoint: Option<i64>, primary_term: i64) -> ShardConfig {
        ShardConfig { dir: Some(dir), merge_threads: 1, local_checkpoint: checkpoint, primary_term, ..ShardConfig::default() }
    }

    // ---------------------------------------------------------------- write path

    /// Applies policies (durability per index) and dynamic shard settings (`index.gc_deletes`,
    /// which decides the tombstones the next commit keeps) and notes indices that closed, when
    /// the cluster state changed. Returns the uuids of indices closed since the previous state.
    fn refresh_policies(&self, cluster: &Cluster) -> Vec<String> {
        let st = cluster.state();
        if self.policies_at.swap(st.version, Ordering::SeqCst) == st.version {
            return Vec::new();
        }
        let mut any_async = false;
        let mut closed_now = HashSet::new();
        // Not under `shards`: opening an index locks the engine's index map, then `shards`.
        for idx in st.metadata.indices.values() {
            cluster.engine.apply_settings(idx);
        }
        {
            let shards = self.shards.lock();
            for idx in st.metadata.indices.values() {
                let p = Policy::of(idx);
                any_async |= !p.request;
                if let Some(m) = shards.get(&idx.uuid) {
                    for s in m.values() {
                        s.request.store(p.request, Ordering::Relaxed);
                    }
                }
                if idx.is_closed() {
                    closed_now.insert(idx.uuid.clone());
                }
            }
        }
        self.any_async.store(any_async, Ordering::SeqCst);
        let mut prev = self.closed_indices.lock();
        let newly: Vec<String> = closed_now.difference(&prev).cloned().collect();
        *prev = closed_now;
        newly
    }

    fn policy(&self, cluster: &Cluster, uuid: &str) -> Policy {
        let st = cluster.state();
        st.metadata.indices.values().find(|i| i.uuid == uuid).map(|i| Policy::of(i)).unwrap_or_default()
    }

    /// After a request: deletes the shards of deleted indices, commits indices that closed,
    /// appends pending operations, acknowledges the cluster's records as their indices'
    /// `index.translog.durability` asks, and starts background commits and the syncer.
    pub async fn after_request(self: &Arc<Self>, cluster: &Arc<Cluster>) -> Result<(), PersistError> {
        if self.closed.load(Ordering::SeqCst) {
            return self.after_close().await;
        }
        self.reconcile(cluster).await;
        let closed = self.refresh_policies(cluster);
        if self.has_pending.swap(false, Ordering::SeqCst) {
            self.drain_all().await?;
        }
        if !cluster.tasks.is_empty() {
            self.ensure_syncer(cluster);
        }
        let lsn = self.ack_target();
        if lsn > self.acked_lsn.load(Ordering::SeqCst) {
            self.acknowledge(lsn).await?;
        } else if !self.lease_valid() {
            self.fail("lease expired".into());
            return Err(PersistError::Lost("the cluster's lease expired before the write was acknowledged".into()));
        }
        if !closed.is_empty() {
            // Elasticsearch flushes an index as it closes: its commit holds every document.
            self.commit_indices(cluster, &closed).await.map_err(|e| self.classify(e))?;
        }
        if self.flush_wanted.load(Ordering::SeqCst) {
            self.commit_flushed(cluster).await.map_err(|e| self.classify(e))?;
        }
        self.maybe_commit(cluster);
        Ok(())
    }

    /// Full commits of the shards the engine flushed (`_flush`, a flushing force merge) since
    /// the last call: only those, as Elasticsearch flushes only the shards a request targets.
    /// Returns once every shard flushed before the call is committed, whoever commits it.
    pub async fn commit_flushed(&self, cluster: &Cluster) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Unloaded.into());
        }
        let _g = self.flush_commit.lock().await;
        if !self.flush_wanted.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let shards: Vec<_> = self.engines_and_states(cluster, None).into_iter().filter(|(_, _, st)| st.flush_wanted.swap(false, Ordering::SeqCst)).collect();
        let mut futs = Vec::with_capacity(shards.len());
        for (ie, n, st) in &shards {
            futs.push(self.full_commit(ie, *n, st, false));
        }
        let r = futures::future::join_all(futs).await.into_iter().collect::<anyhow::Result<Vec<()>>>();
        if r.is_err() {
            // The next request (or the syncer) tries again.
            for (_, _, st) in &shards {
                st.flush_wanted.store(true, Ordering::SeqCst);
            }
            self.flush_wanted.store(true, Ordering::SeqCst);
        }
        r.map(|_| ())
    }

    /// The LSN a request must wait for: the newest record of a request-durability shard.
    fn ack_target(&self) -> u64 {
        if !self.any_async.load(Ordering::SeqCst) {
            return self.last_lsn.load(Ordering::SeqCst);
        }
        self.all_shards().iter().filter(|s| s.request.load(Ordering::Relaxed)).map(|s| s.lsn.load(Ordering::SeqCst)).max().unwrap_or(0)
    }

    /// A request that finished after the cluster was closed under it: its writes may be
    /// acknowledged only when the WAL already holds them (nothing is appended for a closed
    /// cluster: it may belong to another node now).
    async fn after_close(&self) -> Result<(), PersistError> {
        if self.all_shards().iter().any(|s| s.request.load(Ordering::Relaxed) && s.has_pending()) {
            return Err(PersistError::Lost("the cluster was unloaded before the write was made durable".into()));
        }
        let lsn = self.ack_target();
        if lsn > self.acked_lsn.load(Ordering::SeqCst) { self.acknowledge(lsn).await } else { Ok(()) }
    }

    /// Waits until `lsn` may be acknowledged: the WAL's ack, a lease still valid, and, with
    /// [`ClusterData::set_verify_owner`], ownership checked since.
    async fn acknowledge(&self, lsn: u64) -> Result<(), PersistError> {
        self.store.wal().wait_ack(lsn).await.map_err(PersistError::from_anyhow)?;
        if !self.lease_valid() {
            self.fail("lease expired".into());
            return Err(PersistError::Lost("the cluster's lease expired before the write was acknowledged".into()));
        }
        if self.verify_owner.load(Ordering::SeqCst) {
            self.check_owner(lsn).await?;
        }
        self.acked_lsn.fetch_max(lsn, Ordering::SeqCst);
        Ok(())
    }

    /// Ownership checked by a listing that started once `lsn` was durable. Concurrent writers
    /// share checks as they share WAL commits: a writer joins a running check that started
    /// after its record was durable, or starts one. Checks overlap (up to
    /// [`MAX_OWNER_CHECKS`]), so a writer whose record became durable just after a check
    /// started waits for one listing, not for that one and then its own.
    async fn check_owner(&self, lsn: u64) -> Result<(), PersistError> {
        use futures::FutureExt;
        loop {
            if self.owner_checked.load(Ordering::SeqCst) >= lsn {
                return Ok(());
            }
            let (covers, check) = {
                let mut g = self.owner_checks.lock();
                g.retain(|(_, c)| c.peek().is_none());
                if let Some((c, f)) = g.iter().find(|(c, _)| *c >= lsn) {
                    (*c, f.clone())
                } else if g.len() >= MAX_OWNER_CHECKS {
                    // Wait for the newest one, then look again.
                    let (c, f) = g.last().expect("checks running").clone();
                    (c, f)
                } else {
                    // Everything durable now is covered by a listing that starts after this.
                    let wal = self.store.wal();
                    let covers = if wal.config().sync_objects { wal.remote_lsn() } else { wal.durable_lsn() }.max(lsn);
                    let (store, cluster) = (self.store.clone(), self.cluster.clone());
                    let f: OwnerCheck = async move { store.check_owner(&cluster).await.map_err(Arc::new) }.boxed().shared();
                    g.push((covers, f.clone()));
                    (covers, f)
                }
            };
            match check.await {
                Ok(()) => {
                    self.owner_checked.fetch_max(covers, Ordering::SeqCst);
                }
                Err(e) => {
                    return Err(match PersistError::from_anyhow(anyhow::Error::new(Shared(e))) {
                        PersistError::Lost(m) => {
                            self.fail(m.clone());
                            PersistError::Lost(m)
                        }
                        // Unknown ownership: the write is durable but can't be acknowledged as ours.
                        PersistError::Busy(m) => PersistError::Busy(m),
                        PersistError::Failed(e) => PersistError::Failed(e.context("checking the cluster's ownership")),
                    });
                }
            }
        }
    }

    /// Appends every shard's pending operations.
    async fn drain_all(&self) -> Result<(), PersistError> {
        let states: Vec<Arc<ShardState>> = self.all_shards().into_iter().filter(|s| s.has_pending()).collect();
        let mut futs = Vec::with_capacity(states.len());
        for s in &states {
            futs.push(self.drain(s));
        }
        let mut first = None;
        for r in futures::future::join_all(futs).await {
            if let Err(e) = r {
                first.get_or_insert(e);
            }
        }
        match first {
            Some(e) => {
                // Still pending: the next request (or the syncer) tries again.
                self.has_pending.store(true, Ordering::SeqCst);
                Err(e)
            }
            None => Ok(()),
        }
    }

    /// Attaches the shard if needed and appends its pending operations.
    async fn drain(&self, st: &ShardState) -> Result<(), PersistError> {
        let mut c = st.commit.lock().await;
        if c.deleted {
            st.pending.lock().clear();
            return Ok(());
        }
        if c.detached || self.closed.load(Ordering::SeqCst) {
            return Err(PersistError::Lost("the cluster was unloaded before the write was made durable".into()));
        }
        self.attach(&mut c, st).await.map_err(|e| self.classify(e))?;
        drop(c);
        self.append_pending(st)
    }

    fn append_pending(&self, st: &ShardState) -> Result<(), PersistError> {
        let ops = std::mem::take(&mut *st.pending.lock());
        if ops.is_empty() {
            return Ok(());
        }
        let g = st.wal.read();
        let w = g.as_ref().expect("attached");
        match w.append_many(&ops) {
            Ok(lsn) => {
                st.lsn.fetch_max(lsn, Ordering::SeqCst);
                self.last_lsn.fetch_max(lsn, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                drop(g);
                let mut p = st.pending.lock();
                let later = std::mem::replace(&mut *p, ops);
                p.extend(later);
                Err(self.classify(e))
            }
        }
    }

    /// Classifies a persistence error; "lost" ones fail the cluster.
    pub fn classify(&self, e: anyhow::Error) -> PersistError {
        let pe = PersistError::from_anyhow(e);
        if let PersistError::Lost(m) = &pe {
            self.fail(m.clone());
        }
        pe
    }

    async fn attach(&self, c: &mut Commit, st: &ShardState) -> anyhow::Result<()> {
        if st.wal.read().is_some() {
            return Ok(());
        }
        anyhow::ensure!(!st.needs_replay.load(Ordering::SeqCst), "shard {} is still recovering", st.id);
        let rec = match c.recovery.take() {
            Some(r) => r,
            None => {
                let rec = self.store.recover_shard(&self.cluster, &st.id).await?;
                // A shard the engine opened empty must not have history.
                anyhow::ensure!(
                    rec.manifest.as_ref().is_none_or(|m| m.segments.is_empty() && m.max_seq_no < 0),
                    "shard {} has data this node didn't load",
                    st.id
                );
                rec
            }
        };
        let (wal, m) = self.store.attach_shard(rec).await?;
        c.manifest = Some(m);
        *st.wal.write() = Some(wal);
        Ok(())
    }

    /// Logs one applied operation (the engine's [`ShardLog`]).
    fn log(&self, st: &ShardState, op: &LoggedOp<'_>) -> EsResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            // A request still running on an unloaded instance: nothing it writes is kept.
            return Err(EsError::new(503, "cluster_block_exception", format!("cluster [{}] was unloaded before the write was made durable", self.cluster)));
        }
        let seq_no = op.seq_no();
        let framed = record::header(op);
        let whole: Vec<u8>;
        let parts: [&[u8]; 2] = match &framed {
            Some((h, t)) => [h.as_bytes(), t],
            None => {
                whole = record::encode(op);
                [&whole[..], &[]]
            }
        };
        let bytes = (parts[0].len() + parts[1].len()) as u64;
        let g = st.wal.read();
        match g.as_ref() {
            Some(w) => match w.append_parts(seq_no, &parts) {
                Ok(lsn) => {
                    st.lsn.fetch_max(lsn, Ordering::SeqCst);
                    self.last_lsn.fetch_max(lsn, Ordering::SeqCst);
                }
                Err(e) => {
                    drop(g);
                    // Applied but not logged: keep it for the next append, and fail the write.
                    st.pending.lock().push((seq_no, parts.concat()));
                    self.has_pending.store(true, Ordering::SeqCst);
                    return Err(self.classify(e).to_es(&self.cluster));
                }
            },
            None => {
                drop(g);
                st.pending.lock().push((seq_no, parts.concat()));
                self.has_pending.store(true, Ordering::SeqCst);
            }
        }
        self.records.fetch_add(1, Ordering::Relaxed);
        self.record_bytes.fetch_add(bytes, Ordering::Relaxed);
        let total = st.log_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if st.log_since.load(Ordering::Relaxed) == 0 {
            let _ = st.log_since.compare_exchange(0, self.now_ms(), Ordering::Relaxed, Ordering::Relaxed);
        }
        if total >= st.next_check.load(Ordering::Relaxed) {
            self.check_wanted.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Starts background full commits of shards that crossed a trigger: the log's flush
    /// threshold or age, or [`HEAP_COMMIT_BYTES`] in memory.
    fn maybe_commit(self: &Arc<Self>, cluster: &Arc<Cluster>) {
        let check = self.check_wanted.swap(false, Ordering::Relaxed);
        let now = self.now_ms();
        let last_scan = self.age_scan_at.load(Ordering::Relaxed);
        let ages = now.saturating_sub(last_scan) >= AGE_SCAN_MS && self.age_scan_at.compare_exchange(last_scan, now, Ordering::Relaxed, Ordering::Relaxed).is_ok();
        if !check && !ages {
            return;
        }
        let mut todo = Vec::new();
        for ie in cluster.engine.indices_snapshot() {
            let states: Vec<(u32, Arc<ShardState>)> = match self.shards.lock().get(&ie.uuid) {
                Some(m) => m.iter().map(|(n, s)| (*n, s.clone())).collect(),
                None => continue,
            };
            let mut policy = None;
            for (n, s) in states {
                let since = s.log_since.load(Ordering::Relaxed);
                if since == 0 || s.committing.load(Ordering::Relaxed) {
                    continue;
                }
                let bytes = s.log_bytes.load(Ordering::Relaxed);
                let crossed = check && bytes >= s.next_check.load(Ordering::Relaxed);
                let p = *policy.get_or_insert_with(|| self.policy(cluster, &ie.uuid));
                let aged = ages && now.saturating_sub(since) >= p.flush_age.as_millis() as u64;
                if !crossed && !aged {
                    continue;
                }
                s.next_check.store(bytes + CHECK_EVERY, Ordering::Relaxed);
                let heap = {
                    let st = ie.shard(n).shard.stats();
                    st.buffer_bytes + st.segment_heap_bytes
                };
                if aged || bytes >= p.flush_threshold || heap >= HEAP_COMMIT_BYTES {
                    todo.push((ie.clone(), n, s));
                }
            }
        }
        for (ie, n, s) in todo {
            self.spawn_full_commit(cluster, ie, n, s);
        }
    }

    // ---------------------------------------------------------------- full commits

    /// Uploads the shard's new segments and changed live docs and writes its manifest.
    /// `unloading` (with the cluster): the last commit of this instance, which also writes when
    /// only the shard's statistics changed, flush and refresh counts included (other commits
    /// leave those to the next one).
    async fn full_commit_locked(&self, c: &mut Commit, ie: &IndexEngine, n: u32, st: &ShardState, force: bool, unloading: Option<&Cluster>) -> anyhow::Result<()> {
        let se = ie.shard(n);
        let logged_before = st.log_bytes.load(Ordering::SeqCst);
        let translog = se.translog_mark();
        let cut = se.shard.flush().map_err(|e| anyhow::anyhow!("flushing shard {}: {e}", st.id))?;
        // An index opens again as a new shard copy would (everything searchable), so a closed
        // one's commit keeps no view.
        let published = cut.published.as_ref().filter(|_| !self.closed_indices.lock().contains(&st.id.index_uuid)).map(encode_published);
        let m = c.manifest.as_ref();
        let same = m.is_some_and(|m| {
            m.max_seq_no == cut.max_seq_no
                && m.local_checkpoint == cut.local_checkpoint
                && m.segments.len() == cut.segments.len()
                && m.segments.iter().zip(&cut.segments).all(|(r, s)| r.id.0 == s.id && r.del_count == s.del_count)
                && m.user_data.get(PUBLISHED_KEY) == published.as_ref()
        }) && ie.field_numbers_allocated() <= c.numbers_committed;
        let stats = ie.persisted_stats(n, unloading.map(|cl| cl.ops.get(&ie.uuid, n as usize)));
        let synonyms = unloading.filter(|_| n == 0).and_then(|cl| cl.loaded_synonyms_json(&ie.uuid));
        let same = same
            && (unloading.is_none() || (m.and_then(|m| m.user_data.get(STATS_KEY)) == stats.as_ref() && m.and_then(|m| m.user_data.get(SYNONYMS_KEY)) == synonyms.as_ref()));
        let empty = m.is_none_or(|m| m.segments.is_empty() && m.max_seq_no < 0) && cut.segments.is_empty() && cut.max_seq_no < 0;
        // An empty shard has nothing to commit, unless its unload has statistics or loaded
        // synonym rules to keep (a `_flush`, searches, a failed synonyms reload).
        let instance = unloading.is_some() && (stats.is_some() || synonyms.is_some());
        if (same || (empty && !instance)) && c.replayed == 0 && !force {
            se.committed(translog);
            return Ok(());
        }
        // The manifest names the WAL this node appends to (and pending operations go there).
        self.attach(c, st).await?;
        if let Err(e) = self.append_pending(st) {
            anyhow::bail!("logging the pending operations of shard {}: {}", st.id, e.to_es(&self.cluster).reason());
        }
        let numbers = ie.field_numbers();
        let mut refs = Vec::with_capacity(cut.segments.len());
        for s in &cut.segments {
            let sid = SegmentId(s.id);
            let mut r = match c.uploaded.get(&s.id) {
                Some(r) => r.clone(),
                None => {
                    let name = sid.file_name();
                    let path = match s.path.clone().filter(|p| p.starts_with(&st.dir)) {
                        Some(p) if p.file_name().is_some_and(|f| f == name.as_str()) && p.exists() => p,
                        _ => {
                            let p = st.dir.join(&name);
                            let data = s.data.clone();
                            let p2 = p.clone();
                            tokio::task::spawn_blocking(move || std::fs::write(&p2, &data[..])).await??;
                            p
                        }
                    };
                    self.store.upload_segment(&path).await.with_context(|| format!("uploading segment {}", sid.to_hex()))?;
                    SegmentRef { id: sid, bytes: s.data.len() as u64, max_doc: s.max_doc, del_count: 0, live: None }
                }
            };
            if s.del_count != r.del_count || (s.del_count > 0 && r.live.is_none()) {
                r.live = match (&s.live_docs, s.del_count) {
                    (Some(b), d) if d > 0 => Some(self.store.write_live_docs(&sid, Bytes::copy_from_slice(b)).await?),
                    _ => None,
                };
                r.del_count = s.del_count;
            }
            refs.push(r);
        }
        let mut m = c.manifest.clone().unwrap_or_default();
        m.segments = refs;
        m.max_seq_no = cut.max_seq_no;
        m.local_checkpoint = cut.local_checkpoint;
        m.primary_term = cut.primary_term.max(1) as u64;
        m.user_data.insert(NUMBERS_KEY.to_string(), serde_json::to_string(&numbers).expect("numbers serialize"));
        match encode_tombstones(se.shard.tombstones(cut.local_checkpoint)) {
            Some(t) => m.user_data.insert(TOMBSTONES_KEY.to_string(), t),
            None => m.user_data.remove(TOMBSTONES_KEY),
        };
        match published {
            Some(p) => m.user_data.insert(PUBLISHED_KEY.to_string(), p),
            None => m.user_data.remove(PUBLISHED_KEY),
        };
        match stats {
            Some(s) => m.user_data.insert(STATS_KEY.to_string(), s),
            None => m.user_data.remove(STATS_KEY),
        };
        match synonyms {
            Some(s) => m.user_data.insert(SYNONYMS_KEY.to_string(), s),
            None => m.user_data.remove(SYNONYMS_KEY),
        };
        let (from, next, age) = se.shard.retention_lease();
        let taken = unix_ms().saturating_sub(age.as_millis() as u64);
        m.user_data.insert(RETENTION_KEY.to_string(), format!("[{from},{next},{taken}]"));
        let written = self.store.write_manifest(&self.cluster, &st.id, &m).await?;
        c.uploaded = written.segments.iter().map(|r| (r.id.0, r.clone())).collect();
        c.manifest = Some(written);
        c.numbers_committed = numbers.len();
        c.replayed = 0;
        // Commits of a shard are serialized (the commit lock) and only they shrink the count.
        let left = st.log_bytes.fetch_sub(logged_before, Ordering::SeqCst) - logged_before;
        st.log_since.store(if left == 0 { 0 } else { self.now_ms() }, Ordering::SeqCst);
        st.next_check.store(left + CHECK_EVERY, Ordering::SeqCst);
        se.committed(translog);
        self.full_commits.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Full commit of one shard if anything is not in its manifest yet.
    async fn full_commit(&self, ie: &IndexEngine, n: u32, st: &ShardState, force: bool) -> anyhow::Result<()> {
        let mut c = st.commit.lock().await;
        if c.deleted || c.detached || self.closed.load(Ordering::SeqCst) || st.needs_replay.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.full_commit_locked(&mut c, ie, n, st, force, None).await
    }

    fn spawn_full_commit(self: &Arc<Self>, cluster: &Arc<Cluster>, ie: Arc<IndexEngine>, n: u32, st: Arc<ShardState>) {
        if st.committing.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let cluster = Arc::downgrade(cluster);
        let guard = TaskGuard::new(&self.tasks);
        tokio::spawn(async move {
            let _guard = guard;
            let r = me.full_commit(&ie, n, &st, false).await;
            st.committing.store(false, Ordering::SeqCst);
            if let Err(e) = r
                && cluster.upgrade().is_some()
            {
                tracing::warn!(cluster = me.cluster, shard = %st.id, error = %e, "background commit failed");
                let _ = me.classify(e);
            }
        });
    }

    fn engines_and_states(&self, cluster: &Cluster, only: Option<&[String]>) -> Vec<(Arc<IndexEngine>, u32, Arc<ShardState>)> {
        // The engine's index map before `shards` (the order opening an index takes them in).
        let engines = cluster.engine.indices_snapshot();
        let g = self.shards.lock();
        engines
            .into_iter()
            .filter(|ie| only.is_none_or(|o| o.contains(&ie.uuid)))
            .flat_map(|ie| {
                let states = g.get(&ie.uuid);
                let v: Vec<_> = ie.shards.iter().filter_map(|se| states.and_then(|m| m.get(&se.id)).map(|st| (ie.clone(), se.id, st.clone()))).collect();
                v
            })
            .collect()
    }

    async fn commit_indices(&self, cluster: &Cluster, uuids: &[String]) -> anyhow::Result<()> {
        let shards = self.engines_and_states(cluster, Some(uuids));
        let mut futs = Vec::with_capacity(shards.len());
        for (ie, n, st) in &shards {
            futs.push(self.full_commit(ie, *n, st, false));
        }
        futures::future::join_all(futs).await.into_iter().collect::<anyhow::Result<Vec<()>>>()?;
        Ok(())
    }

    /// Full commits of every shard (`_flush`, branching, unloading). Fails with [`Unloaded`] once
    /// the cluster is closed.
    pub async fn flush_all(&self, cluster: &Cluster, force: bool) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Unloaded.into());
        }
        self.reconcile(cluster).await;
        // Settings changed by a request that wrote nothing still apply to this commit.
        self.refresh_policies(cluster);
        let shards = self.engines_and_states(cluster, None);
        let mut futs = Vec::with_capacity(shards.len());
        for (ie, n, st) in &shards {
            futs.push(self.full_commit(ie, *n, st, force));
        }
        let results = futures::future::join_all(futs).await;
        results.into_iter().collect::<anyhow::Result<Vec<()>>>()?;
        // A shard detached by a concurrent close was skipped, not committed.
        if self.closed.load(Ordering::SeqCst) {
            return Err(Unloaded.into());
        }
        Ok(())
    }

    /// Deletes the shards of indices no longer in the state.
    async fn reconcile(&self, cluster: &Cluster) {
        let st = cluster.state();
        // Indices only go away with a new state version.
        if self.reconciled.swap(st.version, Ordering::SeqCst) == st.version {
            return;
        }
        let gone: Vec<Arc<ShardState>> = {
            let mut g = self.shards.lock();
            let live: HashSet<&str> = st.metadata.indices.values().map(|i| i.uuid.as_str()).collect();
            let keys: Vec<String> = g.keys().filter(|u| !live.contains(u.as_str())).cloned().collect();
            keys.into_iter().filter_map(|k| g.remove(&k)).flat_map(|m| m.into_values()).collect()
        };
        {
            let mut p = self.prepared.lock();
            if !p.is_empty() {
                p.retain(|(u, _), _| st.metadata.indices.values().any(|i| &i.uuid == u));
            }
        }
        for s in gone {
            let mut c = s.commit.lock().await;
            c.deleted = true;
            *s.wal.write() = None;
            s.pending.lock().clear();
            c.recovery = None;
            if let Err(e) = self.store.delete_shard(&self.cluster, &s.id).await {
                tracing::warn!(cluster = self.cluster, shard = %s.id, error = %e, "deleting a shard of a deleted index failed");
            }
            let _ = tokio::fs::remove_dir_all(&s.dir).await;
        }
    }

    // ---------------------------------------------------------------- background sync

    /// While background tasks (by-query requests answered before they finish) write, appends
    /// their pending operations every `index.translog.sync_interval` (at most 5 s).
    fn ensure_syncer(self: &Arc<Self>, cluster: &Arc<Cluster>) {
        if self.syncer.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let weak = Arc::downgrade(cluster);
        let guard = TaskGuard::new(&self.tasks);
        tokio::spawn(async move {
            let _guard = guard;
            loop {
                let interval = {
                    let Some(cl) = weak.upgrade() else { break };
                    let st = cl.state();
                    st.metadata.indices.values().map(|i| Policy::of(i).sync_interval).min().unwrap_or(DEFAULT_SYNC_INTERVAL).min(DEFAULT_SYNC_INTERVAL)
                };
                let closing = me.closing.notified();
                if me.closed.load(Ordering::SeqCst) {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = closing => break,
                }
                if me.closed.load(Ordering::SeqCst) {
                    break;
                }
                let Some(cl) = weak.upgrade() else { break };
                if me.has_pending.swap(false, Ordering::SeqCst)
                    && let Err(e) = me.drain_all().await
                {
                    tracing::warn!(cluster = me.cluster, error = ?e, "background WAL sync failed");
                }
                if me.flush_wanted.load(Ordering::SeqCst)
                    && let Err(e) = me.commit_flushed(&cl).await
                    && !me.closed.load(Ordering::SeqCst)
                {
                    tracing::warn!(cluster = me.cluster, error = %e, "committing flushed shards failed");
                    let _ = me.classify(e);
                }
                me.maybe_commit(&cl);
                let busy = me.has_pending.load(Ordering::SeqCst) || !cl.tasks.is_empty();
                if !busy {
                    me.syncer.store(false, Ordering::SeqCst);
                    // A write that raced this decision may have seen the syncer still running:
                    // take the job back rather than leave it unlogged.
                    if (me.has_pending.load(Ordering::SeqCst) || !cl.tasks.is_empty()) && !me.syncer.swap(true, Ordering::SeqCst) {
                        continue;
                    }
                    return;
                }
            }
            me.syncer.store(false, Ordering::SeqCst);
        });
    }

    // ---------------------------------------------------------------- unload

    /// Unloads: with `flush`, full-commits every changed shard and detaches it from the WAL
    /// (the manifest stops naming this node's WAL); without (the cluster moved to another
    /// node), only forgets. Removes the cluster's segment files from the staging directory.
    pub async fn close(&self, cluster: &Cluster, flush: bool) -> anyhow::Result<()> {
        if flush && self.failed().is_none() {
            // An index this instance never opened (an empty one: shards open on first use)
            // whose flush or refresh counts changed, or whose analysis chain keeps synonym
            // rules the sets no longer have, opens now, so its unload commit keeps them.
            let st = cluster.state();
            for idx in st.metadata.indices.values() {
                if !idx.is_closed()
                    && cluster.engine.loaded(&idx.uuid).is_none()
                    && ((0..idx.number_of_shards() as usize).any(|n| cluster.ops.get(&idx.uuid, n) != search_es::cluster::OpCounts::default())
                        || cluster.loaded_synonyms_json(&idx.uuid).is_some())
                {
                    let _ = cluster.engine.index(cluster, &st, idx);
                }
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        self.closing.notify_waiters();
        let mut first_err = None;
        if flush && self.failed().is_none() {
            self.reconcile(cluster).await;
            self.refresh_policies(cluster);
            let engines: HashMap<String, Arc<IndexEngine>> = cluster.engine.indices_snapshot().into_iter().map(|ie| (ie.uuid.clone(), ie)).collect();
            let shards: Vec<Arc<ShardState>> = self.all_shards();
            let mut futs = Vec::with_capacity(shards.len());
            for st in &shards {
                futs.push(self.close_shard(cluster, engines.get(&st.id.index_uuid), st));
            }
            let results = futures::future::join_all(futs).await;
            for r in results {
                if let Err(e) = r {
                    first_err.get_or_insert(e);
                }
            }
        } else {
            for st in self.all_shards() {
                let mut c = st.commit.lock().await;
                c.detached = true;
                *st.wal.write() = None;
            }
        }
        self.prepared.lock().clear();
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn close_shard(&self, cluster: &Cluster, ie: Option<&Arc<IndexEngine>>, st: &ShardState) -> anyhow::Result<()> {
        let mut c = st.commit.lock().await;
        if c.deleted || c.detached {
            return Ok(());
        }
        if let Some(ie) = ie
            && !st.needs_replay.load(Ordering::SeqCst)
        {
            self.full_commit_locked(&mut c, ie, st.id.shard, st, false, Some(cluster)).await?;
        }
        c.detached = true;
        let w = st.wal.write().take();
        if let Some(w) = w {
            self.store.detach_shard(w).await?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- figures

    /// Number of open shards with persistence state.
    pub fn open_shards(&self) -> usize {
        self.shards.lock().values().map(HashMap::len).sum()
    }

    fn all_shards(&self) -> Vec<Arc<ShardState>> {
        self.shards.lock().values().flat_map(|m| m.values().cloned()).collect()
    }

    /// Bytes of the WAL records not yet in a manifest, over every shard.
    pub fn unflushed_log_bytes(&self) -> u64 {
        self.all_shards().iter().map(|s| s.log_bytes.load(Ordering::SeqCst)).sum()
    }
}

/// The engine's log of one shard: [`ClusterData::log`].
struct OpLog {
    data: Weak<ClusterData>,
    st: Arc<ShardState>,
}

impl OpLog {
    fn data(&self) -> EsResult<Arc<ClusterData>> {
        self.data.upgrade().ok_or_else(|| EsError::new(503, "cluster_block_exception", "the cluster was unloaded from this node"))
    }
}

impl ShardLog for OpLog {
    fn check(&self) -> EsResult<()> {
        let d = self.data()?;
        if d.closed.load(Ordering::SeqCst) {
            return Err(EsError::new(503, "cluster_block_exception", format!("cluster [{}] was unloaded from this node", d.cluster)));
        }
        d.store.wal().check_room().map_err(|e| PersistError::from_anyhow(e).to_es(&d.cluster))
    }

    fn append(&self, op: &LoggedOp<'_>) -> EsResult<()> {
        self.data()?.log(&self.st, op)
    }

    fn flush_requested(&self) {
        if let Ok(d) = self.data() {
            self.st.flush_wanted.store(true, Ordering::SeqCst);
            d.flush_wanted.store(true, Ordering::SeqCst);
        }
    }
}

impl ShardStorage for ClusterData {
    fn open(&self, index_uuid: &str, shard: u32, schema: search_index::Schema) -> EsResult<Shard> {
        if self.closed.load(Ordering::SeqCst) {
            // A request still running on an unloaded instance: nothing it writes is kept.
            return Err(EsError::new(503, "cluster_block_exception", format!("cluster [{}] was unloaded from this node", self.cluster)));
        }
        let key = (index_uuid.to_string(), shard);
        let dir = self.shard_dir(index_uuid, shard);
        std::fs::create_dir_all(&dir).map_err(es_internal)?;
        let prepared = {
            let mut p = self.prepared.lock();
            if p.contains_key(&key) && !self.opening_now.load(Ordering::SeqCst) {
                // Only the wake's replay may open a recovered shard: its WAL must be replayed
                // before anything else writes to it.
                return Err(EsError::new(503, "cluster_block_exception", format!("shard [{index_uuid}][{shard}] is recovering")));
            }
            p.remove(&key)
        };
        if prepared.is_none() && self.shards.lock().get(index_uuid).is_some_and(|m| m.contains_key(&shard)) {
            // The engine dropped an index that still exists: an empty shard would lose its data.
            return Err(EsError::internal(format!("shard [{index_uuid}][{shard}] is still open on this node")));
        }
        let (shard_obj, state) = match prepared {
            Some(p) => {
                let cfg = self.shard_config(dir.clone(), Some(p.checkpoint), p.primary_term);
                let s = Shard::open_published(cfg, schema, p.handles, p.published)
                    .map_err(|e| EsError::internal(format!("opening shard [{index_uuid}][{shard}]: {e}")))?;
                s.seed_tombstones(p.tombstones);
                if let Some((from, next, age)) = p.retention {
                    s.seed_retention_lease(from, next, age);
                }
                let st = ShardState::new(ShardId::new(index_uuid, shard), dir, p.commit);
                st.needs_replay.store(true, Ordering::SeqCst);
                self.has_prepared.store(true, Ordering::SeqCst);
                (s, st)
            }
            None => {
                let cfg = self.shard_config(dir.clone(), None, 1);
                let s = Shard::open(cfg, schema, Vec::new()).map_err(es_internal)?;
                (s, ShardState::new(ShardId::new(index_uuid, shard), dir, Commit::empty()))
            }
        };
        // The next request applies the policies again (to this shard too).
        self.policies_at.store(u64::MAX, Ordering::SeqCst);
        self.shards.lock().entry(key.0).or_default().insert(shard, Arc::new(state));
        Ok(shard_obj)
    }

    fn log(&self, index_uuid: &str, shard: u32) -> Option<Arc<dyn ShardLog>> {
        let st = self.shards.lock().get(index_uuid)?.get(&shard)?.clone();
        Some(Arc::new(OpLog { data: self.me.clone(), st }))
    }

    fn snapshots(&self) -> Option<Arc<dyn SnapshotStorage>> {
        self.me.upgrade().map(|d| d as Arc<dyn SnapshotStorage>)
    }
}

// ---------------------------------------------------------------- snapshot contents

/// Prefix of the pins ([`Store::write_pin`]) holding snapshot contents: `snap.<snapshot
/// uuid>.<index uuid>`, one per shard. Index and snapshot uuids never contain a dot.
const SNAPSHOT_PIN_PREFIX: &str = "snap.";

/// Commit data every snapshot pin carries since empty shards are pinned too: with it, a
/// shard of the index without a pin was lost (a restore fails) rather than empty.
const SNAPSHOT_PIN_FORMAT_KEY: &str = "roll.snapshot.pin_format";
const SNAPSHOT_PIN_FORMAT: &str = "2";

fn snapshot_pin(snapshot: &str, index_uuid: &str, shard: u32) -> ShardId {
    ShardId::new(format!("{SNAPSHOT_PIN_PREFIX}{snapshot}.{index_uuid}"), shard)
}

/// `(snapshot uuid, index uuid)` of a snapshot pin.
fn parse_snapshot_pin(pin: &ShardId) -> Option<(&str, &str)> {
    pin.index_uuid.strip_prefix(SNAPSHOT_PIN_PREFIX)?.split_once('.')
}

impl ClusterData {
    fn snapshot_error(&self, snapshot: &str, e: anyhow::Error) -> EsError {
        match self.classify(e) {
            PersistError::Failed(e) => EsError::new(500, "snapshot_exception", format!("[{snapshot}] failed to store the snapshot's shard contents: {e:#}")),
            other => other.to_es(&self.cluster),
        }
    }

    /// The latest commit point of a shard, committing it first when it is open here.
    async fn commit_for_snapshot(&self, engine: Option<&Arc<IndexEngine>>, index_uuid: &str, n: u32) -> anyhow::Result<Option<ShardManifest>> {
        let st = self.shards.lock().get(index_uuid).and_then(|m| m.get(&n).cloned());
        if let (Some(ie), Some(st)) = (engine, st)
            && (n as usize) < ie.shards.len()
        {
            let mut c = st.commit.lock().await;
            if self.closed.load(Ordering::SeqCst) || c.detached {
                return Err(Unloaded.into());
            }
            if !c.deleted && !st.needs_replay.load(Ordering::SeqCst) {
                self.full_commit_locked(&mut c, ie, n, &st, false, None).await?;
            }
            return Ok(c.manifest.clone());
        }
        // Not open here (a closed index): its last commit holds everything.
        self.store.read_manifest(&self.cluster, &ShardId::new(index_uuid, n)).await
    }

    /// A snapshot shard's segments, opened whole (a restore reads all of them).
    async fn open_pinned(&self, m: &ShardManifest) -> anyhow::Result<Vec<SegmentHandle>> {
        let mut out = Vec::with_capacity(m.segments.len());
        for r in &m.segments {
            let d = self.store.open_segment(&r.id).await?;
            let mut h = SegmentHandle::new(Slice::from_owner(d));
            h.path = Some(self.store.segment_path(&r.id));
            if let Some(g) = r.live {
                let b = self.store.open_live_docs(&r.id, g).await?;
                h.live = Some(LiveDocs::decode(&b, r.id.0).with_context(|| format!("live docs of segment {}", r.id.to_hex()))?.0);
            }
            out.push(h);
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl SnapshotStorage for ClusterData {
    async fn capture(&self, snapshot: &str, index_uuid: &str, n: u32, engine: Option<&Arc<IndexEngine>>, previous: Option<&str>) -> EsResult<ShardFiles> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(PersistError::Lost(Unloaded.to_string()).to_es(&self.cluster));
        }
        let m = self.commit_for_snapshot(engine, index_uuid, n).await.map_err(|e| self.snapshot_error(snapshot, e))?;
        // An empty shard gets an empty pin, so a missing pin is detectably lost.
        let mut m = m.filter(|m| !m.segments.is_empty()).unwrap_or_default();
        // The segments and how to read them, nothing of the index's running state.
        m.user_data.retain(|k, _| k == NUMBERS_KEY);
        m.user_data.insert(SNAPSHOT_PIN_FORMAT_KEY.into(), SNAPSHOT_PIN_FORMAT.into());
        let pin_bytes = self.store.write_pin(&self.cluster, &snapshot_pin(snapshot, index_uuid, n), &m).await.map_err(|e| self.snapshot_error(snapshot, e))?;
        let previous = match previous {
            Some(p) => self.store.read_pin(&self.cluster, &snapshot_pin(p, index_uuid, n)).await.map_err(|e| self.snapshot_error(snapshot, e))?,
            None => None,
        };
        Ok(pinned_files(&m, previous.as_ref(), pin_bytes))
    }

    async fn load(&self, snapshot: &str, index_uuid: &str, shards: u32) -> EsResult<Vec<search_es::engine::SnapshotShard>> {
        let mut pins = Vec::with_capacity(shards as usize);
        for n in 0..shards {
            pins.push(self.store.read_pin(&self.cluster, &snapshot_pin(snapshot, index_uuid, n)).await.map_err(|e| self.snapshot_error(snapshot, e))?);
        }
        // Snapshots pin every shard since the format key exists; older ones pinned only
        // shards with documents, so a shard without a pin there was empty.
        let every_shard_pinned = pins.iter().flatten().any(|m| m.user_data.contains_key(SNAPSHOT_PIN_FORMAT_KEY));
        let mut out = Vec::with_capacity(shards as usize);
        for (n, pin) in pins.into_iter().enumerate() {
            let Some(m) = pin else {
                if every_shard_pinned {
                    return Err(EsError::new(
                        500,
                        "snapshot_restore_exception",
                        format!("[{snapshot}] the contents of shard [{index_uuid}][{n}] are missing from the snapshot's storage"),
                    ));
                }
                out.push(search_es::engine::SnapshotShard { segments: Vec::new(), numbers: Vec::new() });
                continue;
            };
            let numbers: Vec<(String, u32)> = match m.user_data.get(NUMBERS_KEY) {
                Some(j) => serde_json::from_str(j).map_err(|e| self.snapshot_error(snapshot, anyhow::anyhow!("field numbers of snapshot shard [{index_uuid}][{n}]: {e}")))?,
                None => Vec::new(),
            };
            let segments = self.open_pinned(&m).await.map_err(|e| self.snapshot_error(snapshot, e))?;
            out.push(search_es::engine::SnapshotShard { segments, numbers });
        }
        Ok(out)
    }

    async fn clone_snapshot(&self, source: &str, target: &str, index_uuids: &[String]) -> EsResult<()> {
        let pins = self.store.list_pins(&self.cluster).await.map_err(|e| self.snapshot_error(target, e))?;
        for p in &pins {
            let Some((snap, uuid)) = parse_snapshot_pin(p) else { continue };
            if snap != source || !index_uuids.iter().any(|u| u == uuid) {
                continue;
            }
            let Some(m) = self.store.read_pin(&self.cluster, p).await.map_err(|e| self.snapshot_error(target, e))? else { continue };
            self.store.write_pin(&self.cluster, &snapshot_pin(target, uuid, p.shard), &m).await.map_err(|e| self.snapshot_error(target, e))?;
        }
        Ok(())
    }

    async fn copies(&self) -> EsResult<Vec<SnapshotCopy>> {
        let pins = self.store.list_pins(&self.cluster).await.map_err(|e| self.snapshot_error("_all", e))?;
        Ok(pins
            .iter()
            .filter_map(|p| parse_snapshot_pin(p).map(|(s, i)| SnapshotCopy { snapshot: s.to_string(), index_uuid: i.to_string(), shard: p.shard }))
            .collect())
    }

    async fn drop_copies(&self, copies: &[SnapshotCopy], older_than: Option<Duration>) -> EsResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(PersistError::Lost(Unloaded.to_string()).to_es(&self.cluster));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let mut doomed = Vec::with_capacity(copies.len());
        for c in copies {
            let pin = snapshot_pin(&c.snapshot, &c.index_uuid, c.shard);
            if let Some(age) = older_than {
                // Pins record when they were written (`ShardManifest::written_ms`, wall clock).
                let Some(m) = self.store.read_pin(&self.cluster, &pin).await.map_err(|e| self.snapshot_error(&c.snapshot, e))? else { continue };
                if now.saturating_sub(m.written_ms) < age.as_millis() as u64 {
                    continue;
                }
            }
            doomed.push(pin);
        }
        self.store.delete_pins(&self.cluster, &doomed).await.map_err(|e| self.snapshot_error("_all", e))
    }
}

/// The files a snapshot pin references: each segment, each live-docs generation and the pin
/// itself (the commit point). Those `previous` (the shard's pin in the latest earlier
/// snapshot) doesn't reference are incremental; the pin counts as incremental unless the
/// commit is the same, as Elasticsearch reuses an unchanged `segments_N`.
fn pinned_files(m: &ShardManifest, previous: Option<&ShardManifest>, pin_bytes: u64) -> ShardFiles {
    // `LiveDocs::encode`: a 44-byte header and crc around one bit per document.
    let live_bytes = |max_doc: u32| 44 + u64::from(max_doc).div_ceil(64) * 8;
    let mut f = ShardFiles { segments: m.segments.len() as u32, ..ShardFiles::default() };
    for r in &m.segments {
        let old = previous.and_then(|p| p.segments.iter().find(|o| o.id == r.id));
        f.file_count += 1;
        f.size_in_bytes += r.bytes;
        if old.is_none() {
            f.incremental_file_count += 1;
            f.incremental_size_in_bytes += r.bytes;
        }
        if r.live.is_some() {
            f.file_count += 1;
            f.size_in_bytes += live_bytes(r.max_doc);
            if old.is_none_or(|o| o.live != r.live) {
                f.incremental_file_count += 1;
                f.incremental_size_in_bytes += live_bytes(r.max_doc);
            }
        }
    }
    f.file_count += 1;
    f.size_in_bytes += pin_bytes;
    if previous.is_none_or(|p| p.segments != m.segments) {
        f.incremental_file_count += 1;
        f.incremental_size_in_bytes += pin_bytes;
    }
    f
}

/// The codec's whole-file check for downloaded segments (search-store's [`search_store::Verifier`]).
pub fn segment_verifier() -> search_store::Verifier {
    search_store::Verifier::new(|d: &search_store::SegmentData| {
        if d.name().ends_with(".seg") {
            let r = search_codec::container::ContainerReader::open(Slice::from_owner(d.clone())).map_err(|e| anyhow::anyhow!("segment {}: {e}", d.name()))?;
            r.verify().map_err(|e| anyhow::anyhow!("segment {}: {e}", d.name()))?;
        }
        Ok(())
    })
}

/// Memory a loaded cluster holds in its shards (buffers, in-heap segments, version maps).
pub fn resident_bytes(cluster: &Cluster) -> u64 {
    cluster
        .engine
        .indices_snapshot()
        .iter()
        .flat_map(|ie| ie.shards.iter())
        .map(|se| {
            let s = se.shard.stats();
            s.buffer_bytes + s.segment_memory_bytes + s.segment_heap_bytes + s.version_map_bytes
        })
        .sum()
}

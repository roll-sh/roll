//! Durable shards: a loaded cluster's indices kept in `search-store` (WAL, segments, manifests).
//!
//! # Cuts as the log
//!
//! search-es applies writes to `search_index::Shard`s directly; the shard can hand out a
//! *consistent cut* of itself at any time ([`Shard::flush`]: every segment, its live docs and
//! the max seq no, captured with no write in flight, nothing published to searchers). A write
//! is made durable by appending the shard's next cut to the node's WAL as a [`record`]: only
//! segments the log doesn't hold yet travel as bytes, and only changed deletions (as a delta).
//! The WAL group-commits records of every shard of every cluster on the node and ships them to
//! object storage; [`ClusterData::after_request`] waits per `index.translog.durability`
//! (`request`: the WAL's ack, i.e. local fsync or object storage with `sync_objects`; `async`:
//! a per-cluster syncer appends every `index.translog.sync_interval`). Concurrent writers of a
//! shard share one cut (group commit at the shard): a writer whose ops a cut already covers
//! only waits for that cut's LSN.
//!
//! A **full commit** uploads the cut's new segments and changed live docs and writes the shard
//! manifest, which trims the WAL. It runs when a record would be large (a big bulk, or a merge
//! produced a new segment: the merged bytes go to object storage once rather than through the
//! WAL), when the log holds `index.translog.flush_threshold_size` or has been uncommitted for
//! `index.translog.flush_threshold_age`, on `_flush`, before branching, and before unloading.
//!
//! # Recovery
//!
//! Waking a cluster reads every shard's latest manifest (one listing), opens its segments from
//! the cache (fetching misses), and replays the cut records past the manifest in log order, so
//! the shard reopens exactly as acknowledged: same segments, live docs, seq nos and versions.
//! An uncached segment larger than one fetch part (a new or replaced node waking a big index)
//! isn't waited for: it opens while downloading ([`FetchGate`] over `Store::fetch_segment`), so
//! the shard starts on its metadata and term dictionaries and each query waits only for the
//! postings and stored fields it reads.
//! Segment files written by search-index live in a per-shard directory under the store's
//! staging directory (same filesystem as the cache, so an upload is a rename), which the store
//! empties when it opens; the directory goes when the cluster unloads.
//!
//! Field numbers are part of a segment's meaning, so each index's numbering travels with its
//! records and manifests and the woken index is reopened with it
//! (`Engine::open_seeded`).
//!
//! # Ownership
//!
//! With leases, acknowledgements also need the cluster's lease to be valid
//! ([`ClusterData::set_lease_deadline`]); the store fences manifests and cluster states by
//! epoch. Any failure that leaves memory ahead of what is durable marks the cluster
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
//! timed out) acknowledges only writes a record already covers: the rest get
//! [`PersistError::Lost`] (503), never a 2xx for a write no log holds.

pub mod record;

use anyhow::Context;
use bytes::Bytes;
use parking_lot::Mutex;
use record::{Cut, CutSegment, LiveChange};
use search_codec::{LiveDocs, Slice};
use search_es::engine::{IndexEngine, ShardStorage};
use search_es::{Cluster, EsError, EsResult};
use search_index::shard::gate::SegmentGate;
use search_index::{FlushOutcome, SegmentHandle, Shard, ShardConfig};
use search_store::{SegmentId, SegmentRef, ShardId, ShardManifest, ShardRecovery, Store, WalShard};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Manifest `user_data` key holding the index's field numbering (JSON `[[key, number], ..]`).
pub const NUMBERS_KEY: &str = "roll.field_numbers";

/// Records larger than this become full commits instead.
pub const MAX_LOG_RECORD: usize = 8 << 20;

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

impl PersistError {
    fn from_anyhow(e: anyhow::Error) -> PersistError {
        if e.chain().any(|c| c.downcast_ref::<Unloaded>().is_some()) {
            return PersistError::Lost(format!("{e:#}"));
        }
        if e.downcast_ref::<search_store::Fenced>().is_some() || e.chain().any(|c| c.downcast_ref::<search_store::Fenced>().is_some()) {
            PersistError::Lost(format!("{e:#}"))
        } else if e.downcast_ref::<search_store::WalFull>().is_some() || e.chain().any(|c| c.downcast_ref::<search_store::WalFull>().is_some()) {
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

/// A segment the shard's log holds (in a manifest or a record since it).
struct Logged {
    del_count: u32,
    /// The deletions as last logged (`None`: none).
    live: Option<LiveDocs>,
    /// Our file of it in the shard directory, while not uploaded.
    file: Option<PathBuf>,
}

struct Commit {
    wal: Option<WalShard>,
    /// Recovered at wake, not attached to the WAL yet (attached on the first write).
    recovery: Option<ShardRecovery>,
    /// The latest commit point.
    manifest: Option<ShardManifest>,
    /// Segments in object storage, as the latest manifest references them.
    uploaded: HashMap<[u8; 16], SegmentRef>,
    logged: HashMap<[u8; 16], Logged>,
    numbers_logged: usize,
    /// Record bytes appended since the manifest, and when the first of them was.
    log_bytes: u64,
    log_since: Option<Instant>,
    /// Records replayed at wake and not yet in a manifest.
    replayed: u64,
    deleted: bool,
    detached: bool,
}

/// One open shard's persistence state.
pub struct ShardState {
    id: ShardId,
    dir: PathBuf,
    commit: tokio::sync::Mutex<Commit>,
    /// The max seq no the log covers (records or manifest).
    committed: AtomicI64,
    /// LSN of the last record appended (0: none this run).
    lsn: AtomicU64,
}

/// A shard recovered at wake, waiting for the engine to open it.
struct Prepared {
    handles: Vec<SegmentHandle>,
    max_seq_no: i64,
    primary_term: i64,
    numbers: Vec<(String, u32)>,
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

/// A loaded cluster's durable storage; the engine's [`ShardStorage`].
pub struct ClusterData {
    store: Store,
    cluster: String,
    dir: PathBuf,
    prepared: Mutex<HashMap<Key, Prepared>>,
    /// Whether `prepared` holds anything (checked before every request).
    has_prepared: AtomicBool,
    /// Open shards by index uuid, then shard number.
    shards: Mutex<HashMap<String, HashMap<u32, Arc<ShardState>>>>,
    /// The cluster state version whose deleted indices were last reconciled.
    reconciled: AtomicU64,
    policies: Mutex<(u64, HashMap<String, Policy>)>,
    failed: Mutex<Option<String>>,
    closed: AtomicBool,
    /// Wakes background tasks when the cluster closes, so none outlives it.
    closing: tokio::sync::Notify,
    syncer: AtomicBool,
    /// Local deadline of the cluster's lease (ms since `epoch`; 0: no lease).
    lease_deadline: AtomicU64,
    /// Check ownership in object storage before acknowledging ([`ClusterData::set_verify_owner`]).
    verify_owner: AtomicBool,
    /// When the last ownership check that passed started (group commit of checks).
    owner_checked: tokio::sync::Mutex<Option<Instant>>,
    epoch: Instant,
    tasks: Arc<TaskCount>,
    /// Statistics.
    pub records: AtomicU64,
    pub record_bytes: AtomicU64,
    pub full_commits: AtomicU64,
}

/// What [`ClusterData::recover`] found.
#[derive(Debug, Default, Clone, Copy)]
pub struct RecoveryStats {
    pub shards: usize,
    pub segments: usize,
    /// Segments opened while still downloading (uncached and larger than one fetch part).
    pub fetching: usize,
    pub records: u64,
    pub orphans: usize,
}

fn hex(id: &[u8; 16]) -> String {
    SegmentId(*id).to_hex()
}

fn es_internal(e: impl std::fmt::Display) -> EsError {
    EsError::internal(e.to_string())
}

impl ClusterData {
    pub fn new(store: Store, cluster: &str, tasks: Arc<TaskCount>) -> Arc<ClusterData> {
        let dir = store.staging_dir().join(format!("c-{cluster}"));
        Arc::new(ClusterData {
            store,
            cluster: cluster.to_string(),
            dir,
            prepared: Mutex::new(HashMap::new()),
            has_prepared: AtomicBool::new(false),
            shards: Mutex::new(HashMap::new()),
            reconciled: AtomicU64::new(u64::MAX),
            policies: Mutex::new((u64::MAX, HashMap::new())),
            failed: Mutex::new(None),
            closed: AtomicBool::new(false),
            closing: tokio::sync::Notify::new(),
            syncer: AtomicBool::new(false),
            lease_deadline: AtomicU64::new(0),
            verify_owner: AtomicBool::new(false),
            owner_checked: tokio::sync::Mutex::new(None),
            epoch: Instant::now(),
            tasks,
            records: AtomicU64::new(0),
            record_bytes: AtomicU64::new(0),
            full_commits: AtomicU64::new(0),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn shard_dir(&self, uuid: &str, shard: u32) -> PathBuf {
        self.dir.join(format!("{uuid}-{shard}"))
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
    pub fn set_lease_deadline(&self, deadline: Option<Instant>) {
        let ms = deadline.map_or(0, |d| d.saturating_duration_since(self.epoch).as_millis().max(1) as u64);
        self.lease_deadline.store(ms, Ordering::SeqCst);
    }

    fn lease_valid(&self) -> bool {
        let d = self.lease_deadline.load(Ordering::SeqCst);
        d == 0 || (self.epoch.elapsed().as_millis() as u64) < d
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

    /// Reads every shard's latest commit point and replays the WAL past it, preparing the
    /// shards for the engine; shards of indices no longer in `cluster`'s state are deleted.
    /// Then opens every index that has data with its persisted field numbering.
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
        let mut prepared = self.prepared.lock();
        for p in opened {
            let (key, p) = p?;
            stats.shards += 1;
            stats.segments += p.handles.len();
            stats.fetching += p.fetching;
            stats.records += p.commit.replayed;
            prepared.insert(key, p);
        }
        self.has_prepared.store(!prepared.is_empty(), Ordering::SeqCst);
        drop(prepared);
        self.open_prepared(cluster);
        Ok(stats)
    }

    async fn recover_shard(&self, mut rec: ShardRecovery) -> anyhow::Result<(Key, Prepared)> {
        let id = rec.shard.clone();
        let dir = self.shard_dir(&id.index_uuid, id.shard);
        tokio::fs::create_dir_all(&dir).await?;
        let m = rec.manifest.clone().unwrap_or_default();
        let progressive = progressive_fetch_possible();
        // (Futures built in a loop, not an iterator closure: those aren't provably `Send` in a
        // spawned request future.)
        let mut futs = Vec::with_capacity(m.segments.len());
        for r in &m.segments {
            // A segment within one fetch part comes in one request anyway.
            futs.push(self.open_for_wake(&r.id, progressive && r.bytes > search_store::cache::FETCH_PART));
        }
        let datas: Vec<Opened> = {
            use futures::StreamExt;
            let opened: Vec<anyhow::Result<Opened>> = futures::stream::iter(futs).buffered(8).collect().await;
            opened.into_iter().collect::<anyhow::Result<_>>().with_context(|| format!("opening the segments of shard {id}"))?
        };
        struct Seg {
            data: Option<Opened>,
            file: Option<PathBuf>,
            live: Option<LiveDocs>,
            max_doc: u32,
            del_count: u32,
        }
        let mut segs: HashMap<[u8; 16], Seg> = HashMap::new();
        for (r, d) in m.segments.iter().zip(datas) {
            let live = match r.live {
                Some(g) => {
                    let b = self.store.open_live_docs(&r.id, g).await?;
                    Some(LiveDocs::decode(&b, r.id.0).with_context(|| format!("live docs of segment {}", r.id.to_hex()))?.0)
                }
                None => None,
            };
            segs.insert(r.id.0, Seg { data: Some(d), file: None, live, max_doc: r.max_doc, del_count: r.del_count });
        }
        let mut order: Vec<[u8; 16]> = m.segments.iter().map(|s| s.id.0).collect();
        let mut numbers: Vec<(String, u32)> = match m.user_data.get(NUMBERS_KEY) {
            Some(j) => serde_json::from_str(j).with_context(|| format!("field numbers of shard {id}"))?,
            None => Vec::new(),
        };
        let mut max_seq_no = m.max_seq_no;
        let mut primary_term = m.primary_term.max(1) as i64;
        let mut records = 0u64;
        {
            let dir = dir.clone();
            let segs = &mut segs;
            let order = &mut order;
            let numbers = &mut numbers;
            let (max_seq_no, primary_term, records) = (&mut max_seq_no, &mut primary_term, &mut records);
            self.store
                .replay(&mut rec, &mut |r| {
                    let cut = record::decode(r.payload).with_context(|| format!("WAL record at seq_no {} of shard {}", r.seq_no, r.shard))?;
                    for s in &cut.segments {
                        if let Some(bytes) = s.data {
                            let path = dir.join(format!("{}.seg", hex(&s.id)));
                            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
                            segs.insert(s.id, Seg { data: None, file: Some(path), live: None, max_doc: s.max_doc, del_count: 0 });
                        }
                        let e = segs.get_mut(&s.id).ok_or_else(|| anyhow::anyhow!("WAL record names segment {} the log doesn't hold", hex(&s.id)))?;
                        match &s.live {
                            LiveChange::Unchanged => {}
                            LiveChange::Whole(b) => e.live = Some(LiveDocs::decode(b, s.id)?.0),
                            LiveChange::Delta(d) => record::apply_delta(e.live.get_or_insert_with(|| LiveDocs::all_live(e.max_doc)), d),
                        }
                        anyhow::ensure!(
                            e.live.as_ref().map_or(0, LiveDocs::num_deleted) == s.del_count,
                            "WAL record: segment {} should have {} deletions",
                            hex(&s.id),
                            s.del_count
                        );
                        e.del_count = s.del_count;
                    }
                    *order = cut.segments.iter().map(|s| s.id).collect();
                    if let Some(n) = cut.numbers {
                        *numbers = serde_json::from_slice(n).context("field numbers in a WAL record")?;
                    }
                    *max_seq_no = cut.max_seq_no;
                    *primary_term = cut.primary_term;
                    *records += 1;
                    Ok(())
                })
                .await?;
        }
        let in_cut: HashSet<[u8; 16]> = order.iter().copied().collect();
        for (sid, s) in &segs {
            if !in_cut.contains(sid)
                && let Some(f) = &s.file
            {
                let _ = std::fs::remove_file(f);
            }
        }
        let mut handles = Vec::with_capacity(order.len());
        let mut logged = HashMap::new();
        let mut stats_fetching = 0usize;
        for sid in &order {
            let s = segs.remove(sid).expect("listed");
            let mut h = match (&s.data, &s.file) {
                (Some(Opened::Whole(d)), _) => {
                    // Already in the store: a placeholder path stops the shard from writing it again.
                    let mut h = SegmentHandle::new(Slice::from_owner(d.clone()));
                    h.path = Some(self.store.segment_path(&SegmentId(*sid)));
                    h
                }
                (Some(Opened::Fetching(f)), _) => {
                    stats_fetching += 1;
                    // SAFETY: the shard reads only ranges the gate reported ready (search-index's
                    // `SegmentHandle::fetching` contract), which is `map_partial`'s requirement.
                    let map = unsafe { f.map_partial() }?;
                    SegmentHandle::fetching(Slice::from_owner(map), Arc::new(FetchGate::new(f.clone())))
                }
                (None, Some(f)) => SegmentHandle::from_file(f).map_err(|e| anyhow::anyhow!("{e}"))?,
                (None, None) => unreachable!("a segment comes from the manifest or a record"),
            };
            h.live = s.live.clone();
            handles.push(h);
            logged.insert(*sid, Logged { del_count: s.del_count, live: s.live, file: s.file });
        }
        let uploaded: HashMap<[u8; 16], SegmentRef> = m.segments.iter().map(|r| (r.id.0, r.clone())).collect();
        let commit = Commit {
            wal: None,
            recovery: Some(rec),
            manifest: Some(m),
            uploaded,
            logged,
            numbers_logged: numbers.len(),
            log_bytes: 0,
            log_since: None,
            replayed: records,
            deleted: false,
            detached: false,
        };
        Ok(((id.index_uuid.clone(), id.shard), Prepared { handles, max_seq_no, primary_term, numbers, commit, fetching: stats_fetching }))
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

    /// Opens indices whose shards were recovered but not opened yet (with their numbering).
    /// Called after wake and before requests while anything is pending (an index whose
    /// analysis failed at wake opens once it builds).
    pub fn open_prepared(&self, cluster: &Cluster) {
        if !self.has_prepared.load(Ordering::SeqCst) {
            return;
        }
        let st = cluster.state();
        let mut wanted: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for ((uuid, _), p) in self.prepared.lock().iter() {
            let e = wanted.entry(uuid.clone()).or_default();
            if p.numbers.len() > e.len() {
                *e = p.numbers.clone();
            }
        }
        for idx in st.metadata.indices.values() {
            let Some(numbers) = wanted.get(&idx.uuid) else { continue };
            if let Err(e) = cluster.engine.open_seeded(cluster, &st, idx, numbers) {
                tracing::debug!(cluster = self.cluster, index = idx.name, error = ?e.reason, "index not opened yet");
            }
        }
        let mut p = self.prepared.lock();
        // Shards beyond the index's shard count can't be opened: forget them.
        p.retain(|(uuid, n), _| st.metadata.indices.values().any(|i| &i.uuid == uuid && *n < i.number_of_shards()));
        self.has_prepared.store(!p.is_empty(), Ordering::SeqCst);
    }

    fn shard_config(&self, dir: PathBuf, checkpoint: Option<i64>, primary_term: i64) -> ShardConfig {
        ShardConfig { dir: Some(dir), merge_threads: 1, local_checkpoint: checkpoint, primary_term, ..ShardConfig::default() }
    }

    // ---------------------------------------------------------------- write path

    fn policy(&self, cluster: &Cluster, uuid: &str) -> Policy {
        let st = cluster.state();
        let mut g = self.policies.lock();
        if g.0 != st.version {
            g.1 = st.metadata.indices.values().map(|i| (i.uuid.clone(), Policy::of(i))).collect();
            g.0 = st.version;
        }
        g.1.get(uuid).copied().unwrap_or_default()
    }

    /// Shards (engine, state) with operations the log doesn't cover yet.
    fn dirty(&self, cluster: &Cluster) -> Vec<(Arc<IndexEngine>, u32, Arc<ShardState>)> {
        let mut out = Vec::new();
        let shards = self.shards.lock();
        for ie in cluster.engine.indices_snapshot() {
            let Some(states) = shards.get(&ie.uuid) else { continue };
            for se in &ie.shards {
                let Some(st) = states.get(&se.id) else { continue };
                if se.shard.max_seq_no() > st.committed.load(Ordering::SeqCst) {
                    out.push((ie.clone(), se.id, st.clone()));
                }
            }
        }
        out
    }

    /// After a request: deletes the shards of deleted indices, makes the request's writes
    /// durable as their indices' `index.translog.durability` asks, and starts the syncer for
    /// `async` indices and background tasks.
    pub async fn after_request(self: &Arc<Self>, cluster: &Arc<Cluster>) -> Result<(), PersistError> {
        if self.closed.load(Ordering::SeqCst) {
            return self.after_close(cluster).await;
        }
        self.reconcile(cluster).await;
        let dirty = self.dirty(cluster);
        let mut need_syncer = !cluster.tasks.list().is_empty();
        let mut sync = Vec::new();
        for (ie, n, st) in dirty {
            if self.policy(cluster, &ie.uuid).request {
                sync.push((ie, n, st));
            } else {
                need_syncer = true;
            }
        }
        if need_syncer {
            self.ensure_syncer(cluster);
        }
        if sync.is_empty() {
            return Ok(());
        }
        let mut futs = Vec::with_capacity(sync.len());
        for (ie, n, st) in &sync {
            futs.push(self.log_shard(cluster, ie, *n, st));
        }
        let results = futures::future::join_all(futs).await;
        let mut lsn = 0;
        for r in results {
            lsn = lsn.max(r?);
        }
        self.acknowledge(lsn).await
    }

    /// A request that finished after the cluster was closed under it: its writes may be
    /// acknowledged only when records already cover them (nothing is appended for a closed
    /// cluster: it may belong to another node now).
    async fn after_close(&self, cluster: &Cluster) -> Result<(), PersistError> {
        let pending = self.dirty(cluster).into_iter().any(|(ie, _, _)| self.policy(cluster, &ie.uuid).request);
        if pending {
            return Err(PersistError::Lost("the cluster was unloaded before the write was made durable".into()));
        }
        let lsn = self.all_shards().iter().map(|s| s.lsn.load(Ordering::SeqCst)).max().unwrap_or(0);
        self.acknowledge(lsn).await
    }

    /// Waits until `lsn` (0: nothing appended) may be acknowledged: the WAL's ack, a lease still
    /// valid, and, with [`ClusterData::set_verify_owner`], ownership checked since.
    async fn acknowledge(&self, lsn: u64) -> Result<(), PersistError> {
        if lsn > 0 {
            self.store.wal().wait_ack(lsn).await.map_err(PersistError::from_anyhow)?;
        }
        if !self.lease_valid() {
            let e = PersistError::Lost("the cluster's lease expired before the write was acknowledged".into());
            self.fail("lease expired".into());
            return Err(e);
        }
        if lsn > 0 && self.verify_owner.load(Ordering::SeqCst) {
            self.check_owner(Instant::now()).await?;
        }
        Ok(())
    }

    /// Ownership checked by a listing that started after `durable_at`. Writers queue on one
    /// lock: a check that started after their record was durable covers them, so concurrent
    /// acknowledgements share checks as they share WAL commits.
    async fn check_owner(&self, durable_at: Instant) -> Result<(), PersistError> {
        let mut last = self.owner_checked.lock().await;
        if last.is_some_and(|t| t > durable_at) {
            return Ok(());
        }
        let started = Instant::now();
        match self.store.check_owner(&self.cluster).await {
            Ok(()) => {
                *last = Some(started);
                Ok(())
            }
            Err(e) => match PersistError::from_anyhow(e) {
                PersistError::Lost(m) => {
                    self.fail(m.clone());
                    Err(PersistError::Lost(m))
                }
                // Unknown ownership: the write is durable but can't be acknowledged as ours.
                PersistError::Busy(m) => Err(PersistError::Busy(m)),
                PersistError::Failed(e) => Err(PersistError::Failed(e.context("checking the cluster's ownership"))),
            },
        }
    }

    /// Appends the shard's cut to the WAL unless the log already covers its ops; returns the
    /// LSN to wait for (0: nothing to wait for).
    async fn log_shard(self: &Arc<Self>, cluster: &Arc<Cluster>, ie: &Arc<IndexEngine>, n: u32, st: &Arc<ShardState>) -> Result<u64, PersistError> {
        let se = ie.shard(n).clone();
        let target = se.shard.max_seq_no();
        if target <= st.committed.load(Ordering::SeqCst) {
            return Ok(st.lsn.load(Ordering::SeqCst));
        }
        let mut c = st.commit.lock().await;
        if c.detached || self.closed.load(Ordering::SeqCst) {
            // Closed while this write ran (force unload): no record will cover it.
            return Err(PersistError::Lost("the cluster was unloaded before the write was made durable".into()));
        }
        if c.deleted {
            return Ok(0);
        }
        // Another writer's cut may cover us now (group commit).
        if target <= st.committed.load(Ordering::SeqCst) {
            return Ok(st.lsn.load(Ordering::SeqCst));
        }
        let cut = se.shard.flush().map_err(|e| PersistError::Failed(anyhow::anyhow!("flushing shard {}: {e}", st.id)))?;
        // Every cut leaves a small segment: let the merge policy fold them while the shard is
        // being written (merges run in the shard's background thread, only while there is work).
        if let Err(e) = se.shard.maybe_merge() {
            tracing::debug!(shard = %st.id, error = %e, "starting merges failed");
        }
        if cut.max_seq_no <= st.committed.load(Ordering::SeqCst) {
            return Ok(st.lsn.load(Ordering::SeqCst));
        }
        let numbers = (ie.field_numbers_allocated() != c.numbers_logged).then(|| ie.field_numbers());
        let r = self.append_cut(&mut c, st, &cut, numbers.as_deref()).await;
        let lsn = match r {
            Ok(Some(lsn)) => lsn,
            Ok(None) => {
                // Too large for the log: commit it to object storage now.
                let numbers = ie.field_numbers();
                self.full_commit_cut(&mut c, st, &cut, &numbers).await.map_err(|e| self.persist_failure(cluster, &st.id, e))?;
                return Ok(0);
            }
            Err(e) => return Err(self.persist_failure(cluster, &st.id, e)),
        };
        let p = self.policy(cluster, &st.id.index_uuid);
        if c.log_bytes >= p.flush_threshold || c.log_since.is_some_and(|t| t.elapsed() >= p.flush_age) {
            drop(c);
            self.spawn_full_commit(cluster, ie.clone(), n, st.clone());
        }
        Ok(lsn)
    }

    /// Classifies a persistence error; "lost" ones fail the cluster.
    fn persist_failure(&self, cluster: &Cluster, id: &ShardId, e: anyhow::Error) -> PersistError {
        // A flush racing `DELETE /index`: the index is gone, nothing to make durable.
        if format!("{e:#}").contains("was deleted") && !cluster.state().metadata.indices.values().any(|i| i.uuid == id.index_uuid) {
            return PersistError::Failed(e);
        }
        let pe = PersistError::from_anyhow(e);
        if let PersistError::Lost(m) = &pe {
            self.fail(m.clone());
        }
        pe
    }

    /// Classifies an error of [`ClusterData::flush_all`] (a lost cluster fails it).
    pub fn classify(&self, e: anyhow::Error) -> PersistError {
        let pe = PersistError::from_anyhow(e);
        if let PersistError::Lost(m) = &pe {
            self.fail(m.clone());
        }
        pe
    }

    async fn attach(&self, c: &mut Commit, id: &ShardId) -> anyhow::Result<()> {
        if c.wal.is_some() {
            return Ok(());
        }
        let rec = match c.recovery.take() {
            Some(r) => r,
            None => {
                let rec = self.store.recover_shard(&self.cluster, id).await?;
                // A shard the engine opened empty must not have history.
                anyhow::ensure!(rec.manifest.as_ref().is_none_or(|m| m.segments.is_empty() && m.max_seq_no < 0), "shard {id} has data this node didn't load");
                rec
            }
        };
        let (wal, m) = self.store.attach_shard(rec).await?;
        c.manifest = Some(m);
        c.wal = Some(wal);
        Ok(())
    }

    /// Appends a cut record; `Ok(None)` when it would be larger than [`MAX_LOG_RECORD`].
    async fn append_cut(&self, c: &mut Commit, st: &ShardState, cut: &FlushOutcome, numbers: Option<&[(String, u32)]>) -> anyhow::Result<Option<u64>> {
        let numbers_json = numbers.map(|n| serde_json::to_vec(n).expect("numbers serialize"));
        let mut decoded: Vec<Option<LiveDocs>> = Vec::with_capacity(cut.segments.len());
        let mut segments = Vec::with_capacity(cut.segments.len());
        let mut size = 64usize;
        for s in &cut.segments {
            let known = c.logged.get(&s.id);
            let data = known.is_none().then(|| &s.data[..]);
            size += data.map_or(0, <[u8]>::len);
            let prev_del = known.map_or(0, |k| k.del_count);
            let live_now = match (&s.live_docs, s.del_count) {
                (Some(b), d) if d > 0 => Some(LiveDocs::decode(b, s.id).map_err(|e| anyhow::anyhow!("live docs of segment {}: {e}", hex(&s.id)))?.0),
                _ => None,
            };
            let live = if s.del_count == prev_del {
                LiveChange::Unchanged
            } else {
                match (&live_now, s.live_docs.as_deref()) {
                    (Some(now), Some(whole)) => {
                        let before = known.and_then(|k| k.live.clone()).unwrap_or_else(|| LiveDocs::all_live(s.max_doc));
                        let delta = record::live_delta(before.words(), now.words());
                        if 4 + delta.len() * 12 < whole.len() { LiveChange::Delta(delta) } else { LiveChange::Whole(whole) }
                    }
                    _ => anyhow::bail!("segment {} lost deletions ({prev_del} before, {} now)", hex(&s.id), s.del_count),
                }
            };
            size += match &live {
                LiveChange::Unchanged => 0,
                LiveChange::Whole(b) => b.len(),
                LiveChange::Delta(d) => d.len() * 12,
            };
            decoded.push(live_now);
            segments.push(CutSegment { id: s.id, max_doc: s.max_doc, del_count: s.del_count, data, live });
        }
        if size > MAX_LOG_RECORD {
            return Ok(None);
        }
        let rec = Cut { max_seq_no: cut.max_seq_no, primary_term: cut.primary_term, numbers: numbers_json.as_deref(), segments };
        let payload = rec.encode();
        self.attach(c, &st.id).await?;
        let lsn = c.wal.as_ref().expect("attached").append(cut.max_seq_no, &payload)?;
        // The log has this cut now.
        let in_cut: HashSet<[u8; 16]> = cut.segments.iter().map(|s| s.id).collect();
        c.logged.retain(|id, l| {
            let keep = in_cut.contains(id);
            if !keep && let Some(f) = l.file.take() {
                let _ = std::fs::remove_file(f);
            }
            keep
        });
        for (s, live) in cut.segments.iter().zip(decoded) {
            match c.logged.get_mut(&s.id) {
                Some(l) => {
                    if l.del_count != s.del_count {
                        l.del_count = s.del_count;
                        l.live = live;
                    }
                }
                None => {
                    c.logged.insert(s.id, Logged { del_count: s.del_count, live, file: s.path.clone().filter(|p| p.starts_with(&st.dir)) });
                }
            }
        }
        if let Some(n) = numbers {
            c.numbers_logged = n.len();
        }
        c.log_bytes += payload.len() as u64;
        c.log_since.get_or_insert_with(Instant::now);
        st.committed.fetch_max(cut.max_seq_no, Ordering::SeqCst);
        st.lsn.fetch_max(lsn, Ordering::SeqCst);
        self.records.fetch_add(1, Ordering::Relaxed);
        self.record_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
        Ok(Some(lsn))
    }

    /// Uploads the cut's new segments and changed live docs and writes the shard's manifest.
    async fn full_commit_cut(&self, c: &mut Commit, st: &ShardState, cut: &FlushOutcome, numbers: &[(String, u32)]) -> anyhow::Result<()> {
        self.attach(c, &st.id).await?;
        let mut refs = Vec::with_capacity(cut.segments.len());
        for s in &cut.segments {
            let sid = SegmentId(s.id);
            let mut r = match c.uploaded.get(&s.id) {
                Some(r) => r.clone(),
                None => {
                    let name = sid.file_name();
                    let ours = c.logged.get(&s.id).and_then(|l| l.file.clone());
                    let path = match ours.or_else(|| s.path.clone().filter(|p| p.starts_with(&st.dir))) {
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
                    if let Some(l) = c.logged.get_mut(&s.id) {
                        l.file = None;
                    }
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
        // A cut holds every op up to its max seq no (an op that failed after getting one, a
        // Lucene-level rejection, never applies: as Elasticsearch's no-op, it counts as done).
        m.local_checkpoint = cut.max_seq_no;
        m.primary_term = cut.primary_term.max(1) as u64;
        m.user_data.insert(NUMBERS_KEY.to_string(), serde_json::to_string(numbers).expect("numbers serialize"));
        let written = self.store.write_manifest(&self.cluster, &st.id, &m).await?;
        c.uploaded = written.segments.iter().map(|r| (r.id.0, r.clone())).collect();
        c.manifest = Some(written);
        let in_cut: HashMap<[u8; 16], &search_index::FlushedSegment> = cut.segments.iter().map(|s| (s.id, s)).collect();
        let mut logged = HashMap::with_capacity(in_cut.len());
        for (id, mut l) in c.logged.drain() {
            match in_cut.get(&id) {
                Some(_) => {
                    l.file = None;
                    logged.insert(id, l);
                }
                None => {
                    if let Some(f) = l.file {
                        let _ = std::fs::remove_file(f);
                    }
                }
            }
        }
        for s in &cut.segments {
            let live = match (&s.live_docs, s.del_count) {
                (Some(b), d) if d > 0 => LiveDocs::decode(b, s.id).ok().map(|x| x.0),
                _ => None,
            };
            logged.entry(s.id).and_modify(|l: &mut Logged| {
                if l.del_count != s.del_count {
                    l.del_count = s.del_count;
                    l.live = live.clone();
                }
            });
            logged.entry(s.id).or_insert(Logged { del_count: s.del_count, live, file: None });
        }
        c.logged = logged;
        c.numbers_logged = numbers.len();
        c.log_bytes = 0;
        c.log_since = None;
        c.replayed = 0;
        st.committed.fetch_max(cut.max_seq_no, Ordering::SeqCst);
        self.full_commits.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Full commit of one shard if anything is not in its manifest yet.
    async fn full_commit(&self, ie: &IndexEngine, n: u32, st: &ShardState, force: bool) -> anyhow::Result<()> {
        let mut c = st.commit.lock().await;
        if c.deleted || c.detached || self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.full_commit_locked(&mut c, ie, n, st, force).await
    }

    async fn full_commit_locked(&self, c: &mut Commit, ie: &IndexEngine, n: u32, st: &ShardState, force: bool) -> anyhow::Result<()> {
        let se = ie.shard(n);
        let cut = se.shard.flush().map_err(|e| anyhow::anyhow!("flushing shard {}: {e}", st.id))?;
        let m = c.manifest.as_ref();
        let same = m.is_some_and(|m| {
            m.max_seq_no == cut.max_seq_no
                && m.segments.len() == cut.segments.len()
                && m.segments.iter().zip(&cut.segments).all(|(r, s)| r.id.0 == s.id && r.del_count == s.del_count)
        }) && ie.field_numbers_allocated() <= c.numbers_logged;
        let empty = m.is_none() && cut.segments.is_empty() && cut.max_seq_no < 0;
        if (same || empty) && c.replayed == 0 && !force {
            return Ok(());
        }
        let numbers = ie.field_numbers();
        self.full_commit_cut(c, st, &cut, &numbers).await
    }

    fn spawn_full_commit(self: &Arc<Self>, cluster: &Arc<Cluster>, ie: Arc<IndexEngine>, n: u32, st: Arc<ShardState>) {
        let me = self.clone();
        let cluster = Arc::downgrade(cluster);
        let guard = TaskGuard::new(&self.tasks);
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(e) = me.full_commit(&ie, n, &st, false).await
                && let Some(cl) = cluster.upgrade()
            {
                let _ = me.persist_failure(&cl, &st.id, e);
            }
        });
    }

    /// Full commits of every shard (`_flush`, branching, unloading). Fails with [`Unloaded`] once
    /// the cluster is closed.
    pub async fn flush_all(&self, cluster: &Cluster, force: bool) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Unloaded.into());
        }
        self.reconcile(cluster).await;
        let shards: Vec<(Arc<IndexEngine>, u32, Arc<ShardState>)> = {
            let g = self.shards.lock();
            cluster
                .engine
                .indices_snapshot()
                .into_iter()
                .flat_map(|ie| {
                    let states = g.get(&ie.uuid);
                    let v: Vec<_> = ie.shards.iter().filter_map(|se| states.and_then(|m| m.get(&se.id)).map(|st| (ie.clone(), se.id, st.clone()))).collect();
                    v
                })
                .collect()
        };
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
                self.has_prepared.store(!p.is_empty(), Ordering::SeqCst);
            }
        }
        for s in gone {
            let mut c = s.commit.lock().await;
            c.deleted = true;
            c.wal = None;
            c.recovery = None;
            if let Err(e) = self.store.delete_shard(&self.cluster, &s.id).await {
                tracing::warn!(cluster = self.cluster, shard = %s.id, error = %e, "deleting a shard of a deleted index failed");
            }
            let _ = tokio::fs::remove_dir_all(&s.dir).await;
        }
    }

    // ---------------------------------------------------------------- background sync

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
                    st.metadata.indices.values().map(|i| Policy::of(i)).filter(|p| !p.request).map(|p| p.sync_interval).min().unwrap_or(DEFAULT_SYNC_INTERVAL)
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
                let dirty = me.dirty(&cl);
                let busy = !dirty.is_empty() || !cl.tasks.list().is_empty();
                for (ie, n, st) in dirty {
                    if let Err(e) = me.log_shard(&cl, &ie, n, &st).await {
                        tracing::warn!(cluster = me.cluster, shard = %st.id, error = ?e, "background WAL sync failed");
                    }
                }
                if !busy {
                    me.syncer.store(false, Ordering::SeqCst);
                    // A write that raced this decision may have seen the syncer still running:
                    // take the job back rather than leave it unlogged.
                    if (!me.dirty(&cl).is_empty() || !cl.tasks.list().is_empty()) && !me.syncer.swap(true, Ordering::SeqCst) {
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
        self.closed.store(true, Ordering::SeqCst);
        self.closing.notify_waiters();
        let mut first_err = None;
        if flush && self.failed().is_none() {
            self.reconcile(cluster).await;
            let engines: HashMap<String, Arc<IndexEngine>> = cluster.engine.indices_snapshot().into_iter().map(|ie| (ie.uuid.clone(), ie)).collect();
            let shards: Vec<Arc<ShardState>> = self.all_shards();
            let mut futs = Vec::with_capacity(shards.len());
            for st in &shards {
                futs.push(self.close_shard(engines.get(&st.id.index_uuid), st));
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
                c.wal = None;
            }
        }
        self.prepared.lock().clear();
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn close_shard(&self, ie: Option<&Arc<IndexEngine>>, st: &ShardState) -> anyhow::Result<()> {
        let mut c = st.commit.lock().await;
        if c.deleted || c.detached {
            return Ok(());
        }
        if let Some(ie) = ie {
            self.full_commit_locked(&mut c, ie, st.id.shard, st, false).await?;
        }
        c.detached = true;
        if let Some(w) = c.wal.take() {
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
        let shards = self.all_shards();
        shards.iter().filter_map(|s| s.commit.try_lock().ok().map(|c| c.log_bytes)).sum()
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
        let prepared = self.prepared.lock().remove(&key);
        if prepared.is_none() && self.shards.lock().get(index_uuid).is_some_and(|m| m.contains_key(&shard)) {
            // The engine dropped an index that still exists: an empty shard would lose its data.
            return Err(EsError::internal(format!("shard [{index_uuid}][{shard}] is still open on this node")));
        }
        let (shard_obj, state) = match prepared {
            Some(p) => {
                let cfg = self.shard_config(dir.clone(), Some(p.max_seq_no), p.primary_term);
                let s = Shard::open(cfg, schema, p.handles).map_err(|e| EsError::internal(format!("opening shard [{index_uuid}][{shard}]: {e}")))?;
                let st = ShardState {
                    id: ShardId::new(index_uuid, shard),
                    dir,
                    commit: tokio::sync::Mutex::new(p.commit),
                    committed: AtomicI64::new(p.max_seq_no),
                    lsn: AtomicU64::new(0),
                };
                (s, st)
            }
            None => {
                let cfg = self.shard_config(dir.clone(), None, 1);
                let s = Shard::open(cfg, schema, Vec::new()).map_err(es_internal)?;
                let commit = Commit {
                    wal: None,
                    recovery: None,
                    manifest: None,
                    uploaded: HashMap::new(),
                    logged: HashMap::new(),
                    numbers_logged: 0,
                    log_bytes: 0,
                    log_since: None,
                    replayed: 0,
                    deleted: false,
                    detached: false,
                };
                let st = ShardState {
                    id: ShardId::new(index_uuid, shard),
                    dir,
                    commit: tokio::sync::Mutex::new(commit),
                    committed: AtomicI64::new(-1),
                    lsn: AtomicU64::new(0),
                };
                (s, st)
            }
        };
        self.shards.lock().entry(key.0).or_default().insert(shard, Arc::new(state));
        Ok(shard_obj)
    }
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

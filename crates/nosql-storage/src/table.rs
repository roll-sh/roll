//! A table: one LSM tree (memtables and SSTables) and its maintenance.
//!
//! The table state (active memtable, frozen memtables being flushed, SSTables) is an immutable
//! snapshot behind a lock held only to clone or swap it: reads work on a snapshot and never
//! wait for a flush or compaction. Flushes write a frozen memtable as an SSTable on the
//! blocking pool, upload it, then swap it in; compactions merge SSTables on the blocking pool,
//! upload the outputs, then swap them for their inputs. Every swap is followed by a manifest
//! version (through the owning database), so object storage always describes a consistent
//! state.

use crate::cache::BlockCache;
use crate::clip::{Item, Slices};
use crate::codec::{decode_update, encode_update};
use crate::compaction::{Plan, Task, TaskKind, fully_expired};
use crate::compare::{ClusteringComparator, compare_clustering, compare_paths};
use crate::manifest::{SstRef, TableId, new_sstable_id, sstable_key};
use crate::memory::{DbMemory, MemtableBudget};
use crate::memtable::{MemCursor, Memtable};
use crate::merge::{MemSource, Merger, Source, drive_blocking};
use crate::model::*;
use crate::options::TableOptions;
use crate::read::{Phase, PartitionRead, RangeRead, ReadMetrics, ReadState, ReadStream, slices_after};
use crate::reconcile::{GcPurger, Purger, merge_rows, purge_row, reconcile};
use crate::sstable::cursor::{KeyBound, SstCursor};
use crate::sstable::writer::{SstWriter, WriterOptions};
use crate::sstable::{BlockSource, Index, SsTable};
use crate::tombstones::purge_marker;
use anyhow::{Result, bail};
use bytes::Bytes;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use pg_storage::Objects;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AO};
use std::sync::{Arc, Weak};

/// Wall-clock milliseconds (injectable for tests).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64))
}

/// What every table on a node shares.
pub struct Env {
    pub objects: Objects,
    pub cache: Arc<BlockCache>,
    /// Scratch space for compaction outputs (in memory without one).
    pub tmp_dir: Option<PathBuf>,
    pub compactions: Arc<tokio::sync::Semaphore>,
    pub auto_compaction: bool,
    pub clock: Clock,
    /// Memtables of every database together ([`crate::memory`]).
    pub memory: Arc<MemtableBudget>,
    /// Memtables being written out at once.
    pub flushes: Arc<tokio::sync::Semaphore>,
    /// Decoded blocks one compaction reads ahead, over all its inputs.
    pub compaction_read_bytes: u64,
    /// How long a write waits for memory before it fails ([`crate::memory::Overloaded`]).
    pub write_timeout: std::time::Duration,
}

impl Env {
    /// A node environment with the default budgets (tests, tools); `Store::start` builds the
    /// node's from its configuration.
    pub fn new(objects: Objects, cache: Arc<BlockCache>, clock: Clock) -> Self {
        Self {
            objects,
            cache,
            tmp_dir: None,
            compactions: Arc::new(tokio::sync::Semaphore::new(2)),
            auto_compaction: false,
            clock,
            memory: MemtableBudget::new(512 << 20),
            flushes: Arc::new(tokio::sync::Semaphore::new(2)),
            compaction_read_bytes: 16 << 20,
            write_timeout: std::time::Duration::from_secs(2),
        }
    }

    pub fn now_ms(&self) -> i64 {
        (self.clock)()
    }
}

/// The owner of a table: logging, durability and manifests (the database).
pub(crate) trait TableHost: Send + Sync {
    /// Log a write: assign its position, append its frame. Returns `(lsn, generation)`.
    fn begin_write(&self, table: TableId, update: &[u8]) -> Result<(u64, u64)>;
    fn end_write(&self, lsn: u64);
    fn wait_durable(&self, generation: u64) -> BoxFuture<'_, Result<()>>;
    fn sync_durability(&self) -> bool;
    /// Every write below this has finished applying.
    fn low_watermark(&self) -> u64;
    /// Wait until memory (memtables, the commit log buffer) lets a write in
    /// ([`crate::memory::Overloaded`] after the write timeout).
    fn admit(&self) -> BoxFuture<'_, Result<()>>;
    /// SSTables or the flushed position changed: record a manifest version.
    fn table_changed(&self, table: TableId, change: Change) -> BoxFuture<'_, Result<()>>;
}

pub(crate) struct Change {
    pub sstables: Vec<SstRef>,
    pub flushed_lsn: Option<u64>,
    pub base_done: bool,
    pub truncated: bool,
}

#[derive(Clone)]
pub struct TableState {
    pub active: Arc<Memtable>,
    /// Oldest first.
    pub frozen: Vec<Arc<Memtable>>,
    pub sstables: Vec<Arc<SsTable>>,
}

/// One data source of a table ([`Table::sources`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataSource {
    /// The `n`-th memtable (0: the active one).
    Memtable(usize),
    /// An SSTable, by object key.
    SsTable(String),
}

impl TableState {
    fn memtables(&self) -> impl Iterator<Item = &Arc<Memtable>> {
        std::iter::once(&self.active).chain(self.frozen.iter())
    }
}

#[derive(Debug, Default)]
pub struct TableMetrics {
    pub writes: AtomicU64,
    pub reads: AtomicU64,
    pub range_reads: AtomicU64,
    pub flushes: AtomicU64,
    pub compactions: AtomicU64,
    pub bytes_flushed: AtomicU64,
    pub bytes_compacted: AtomicU64,
    pub expired_sstables_dropped: AtomicU64,
}

/// `nodetool tablestats`-style numbers.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct TableStats {
    pub sstables: usize,
    pub sstables_per_level: Vec<usize>,
    /// Bytes on object storage (data + index).
    pub space_used: u64,
    pub data_bytes: u64,
    pub index_bytes: u64,
    pub uncompressed_bytes: u64,
    /// Blocks of this table in the node's caches.
    pub cache_bytes: u64,
    pub memtable_bytes: u64,
    pub memtable_partitions: u64,
    pub memtable_rows: u64,
    pub partitions_estimate: u64,
    pub tombstones: u64,
    pub droppable_tombstones: u64,
    pub max_partition_bytes: u64,
    pub bloom_filter_bytes: u64,
    /// Point reads the SSTables' bloom filters let through for nothing.
    pub bloom_filter_false_positives: u64,
    pub writes: u64,
    pub reads: u64,
    pub range_reads: u64,
    pub flushes: u64,
    pub compactions: u64,
}

pub struct Table {
    pub id: TableId,
    /// The database whose key prefix new SSTables get.
    pub(crate) owner: String,
    pub(crate) env: Arc<Env>,
    opts: RwLock<TableOptions>,
    pub(crate) state: RwLock<Arc<TableState>>,
    host: Option<Weak<dyn TableHost>>,
    pub(crate) flush_lock: tokio::sync::Mutex<()>,
    pub(crate) compaction_lock: tokio::sync::Mutex<()>,
    flush_scheduled: AtomicBool,
    /// Another flush was asked for while one was scheduled.
    flush_requested: AtomicBool,
    compaction_scheduled: AtomicBool,
    /// Background compaction is off for this table (`nodetool disableautocompaction`).
    auto_compaction_off: AtomicBool,
    /// Wakes a compaction waiting for its turn when compactions stop.
    compactions_stop: tokio::sync::Notify,
    /// The database's memtable memory (`None`: unaccounted, standalone tables).
    memory: Option<Arc<DbMemory>>,
    dropped: AtomicBool,
    compactions_stopped: AtomicBool,
    weak_self: Weak<Table>,
    pub metrics: TableMetrics,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Table").field("id", &self.id).field("owner", &self.owner).finish()
    }
}

pub(crate) fn sst_ref(s: &SsTable) -> SstRef {
    SstRef {
        key: s.key.clone(),
        level: s.level,
        run: s.run,
        data_size: s.data_size(),
        index_size: s.index_size,
        created_ms: s.created_ms,
        partitions: s.stats().partitions,
        min_timestamp: s.stats().min_timestamp,
        max_timestamp: s.stats().max_timestamp,
    }
}

impl Table {
    pub(crate) fn new(
        id: TableId,
        owner: String,
        env: Arc<Env>,
        opts: TableOptions,
        sstables: Vec<Arc<SsTable>>,
        host: Option<Weak<dyn TableHost>>,
        memory: Option<Arc<DbMemory>>,
    ) -> Arc<Self> {
        let active = Arc::new(Memtable::accounted(opts.comparator.clone(), opts.reconcile, memory.clone()));
        Arc::new_cyclic(|weak_self| Self {
            id,
            owner,
            env,
            opts: RwLock::new(opts),
            state: RwLock::new(Arc::new(TableState { active, frozen: Vec::new(), sstables })),
            host,
            flush_lock: tokio::sync::Mutex::new(()),
            compaction_lock: tokio::sync::Mutex::new(()),
            flush_scheduled: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            compaction_scheduled: AtomicBool::new(false),
            auto_compaction_off: AtomicBool::new(false),
            compactions_stop: tokio::sync::Notify::new(),
            memory,
            dropped: AtomicBool::new(false),
            compactions_stopped: AtomicBool::new(false),
            weak_self: weak_self.clone(),
            metrics: TableMetrics::default(),
        })
    }

    /// A table without a database: no commit log, no manifests (tests, scratch tables).
    pub fn standalone(id: TableId, owner: &str, env: Arc<Env>, opts: TableOptions) -> Arc<Self> {
        Self::new(id, owner.to_string(), env, opts, Vec::new(), None, None)
    }

    pub fn options(&self) -> TableOptions {
        self.opts.read().clone()
    }

    /// Change options (`ALTER TABLE`). A new comparator applies to new memtables only and must
    /// order like the old one.
    pub fn set_options(&self, opts: TableOptions) {
        *self.opts.write() = opts;
    }

    pub fn state(&self) -> Arc<TableState> {
        self.state.read().clone()
    }

    pub(crate) fn mark_dropped(&self) {
        self.dropped.store(true, AO::SeqCst);
    }

    /// Stop compacting (the database unloads): a running compaction gives up at its next
    /// partition (its outputs are discarded), one waiting for its turn at once.
    pub(crate) async fn stop_compactions(&self) {
        self.compactions_stopped.store(true, AO::SeqCst);
        self.compactions_stop.notify_waiters();
        drop(self.compaction_lock.lock().await);
    }

    pub(crate) fn resume_compactions(&self) {
        self.compactions_stopped.store(false, AO::SeqCst);
    }

    fn compactions_allowed(&self) -> bool {
        !self.dropped.load(AO::SeqCst) && !self.compactions_stopped.load(AO::SeqCst)
    }

    fn host(&self) -> Option<Arc<dyn TableHost>> {
        self.host.as_ref().and_then(|w| w.upgrade())
    }

    /// Sort rows and columns, merge duplicates: the engine may build updates in any order.
    pub(crate) fn normalize(&self, u: &mut PartitionUpdate) {
        let opts = self.opts.read();
        let cmp = &*opts.comparator;
        let mode = opts.reconcile;
        if let Some(s) = u.static_row.take() {
            u.static_row = normalize_row(cmp, mode, s);
        }
        let mut rows: Vec<Row> = std::mem::take(&mut u.rows).into_iter().filter_map(|r| normalize_row(cmp, mode, r)).collect();
        if !rows.windows(2).all(|w| compare_clustering(cmp, &w[0].clustering, &w[1].clustering) == Ordering::Less) {
            rows.sort_by(|a, b| compare_clustering(cmp, &a.clustering, &b.clustering));
            let mut out: Vec<Row> = Vec::with_capacity(rows.len());
            for r in rows {
                match out.last_mut() {
                    Some(last) if compare_clustering(cmp, &last.clustering, &r.clustering) == Ordering::Equal => {
                        let prev = std::mem::take(last);
                        match merge_rows(cmp, mode, vec![prev, r], DeletionTime::LIVE) {
                            Some(m) => *last = m,
                            None => {
                                out.pop();
                            }
                        }
                    }
                    _ => out.push(r),
                }
            }
            rows = out;
        }
        u.rows = rows;
    }

    /// One row made canonical as [`Table::normalize`] does (`None`: nothing left).
    pub(crate) fn normalize_row(&self, row: Row) -> Option<Row> {
        let opts = self.opts.read();
        normalize_row(&*opts.comparator, opts.reconcile, row)
    }

    /// Apply a write, logging it when the table belongs to a database.
    pub async fn apply(&self, mut update: PartitionUpdate) -> Result<()> {
        if self.dropped.load(AO::SeqCst) {
            bail!("table {} was dropped", self.id);
        }
        self.normalize(&mut update);
        let Some(host) = self.host() else {
            self.apply_local(update, 0);
            return Ok(());
        };
        host.admit().await?;
        let buf = encode_update(&update);
        let (lsn, generation) = host.begin_write(self.id, &buf)?;
        // Memtable data references the encoded frame bytes: one allocation per write.
        let decoded = decode_update(&buf);
        match decoded {
            Ok(u) => self.apply_local(u, lsn),
            Err(e) => {
                host.end_write(lsn);
                return Err(e);
            }
        }
        host.end_write(lsn);
        if host.sync_durability() {
            host.wait_durable(generation).await?;
        }
        Ok(())
    }

    /// Merge into the active memtable (retrying if a flush froze it meanwhile).
    pub(crate) fn apply_local(&self, mut update: PartitionUpdate, lsn: u64) {
        loop {
            let st = self.state.read().clone();
            match st.active.apply(update, lsn) {
                Ok(()) => {
                    self.metrics.writes.fetch_add(1, AO::Relaxed);
                    if st.active.bytes() >= self.opts.read().memtable_flush_bytes {
                        self.schedule_flush();
                    }
                    return;
                }
                Err(u) => {
                    update = u;
                    std::thread::yield_now();
                }
            }
        }
    }

    fn schedule_flush(&self) {
        self.request_flush();
    }

    /// Flush in the background. Asked while a flush runs, another one follows it (the active
    /// memtable has filled meanwhile). Returns whether a new flush task started.
    pub(crate) fn request_flush(&self) -> bool {
        let Some(me) = self.self_arc() else { return false };
        self.flush_requested.store(true, AO::SeqCst);
        if self.flush_scheduled.swap(true, AO::SeqCst) {
            return false;
        }
        let Ok(h) = tokio::runtime::Handle::try_current() else {
            self.flush_scheduled.store(false, AO::SeqCst);
            return false;
        };
        h.spawn(async move {
            loop {
                while me.flush_requested.swap(false, AO::SeqCst) {
                    if let Err(e) = me.flush().await {
                        tracing::warn!(table = %me.id, error = %e, "memtable flush failed");
                        break;
                    }
                }
                me.flush_scheduled.store(false, AO::SeqCst);
                // A request between the last check and the store above.
                if !me.flush_requested.load(AO::SeqCst) || me.flush_scheduled.swap(true, AO::SeqCst) {
                    break;
                }
            }
        });
        true
    }

    fn self_arc(&self) -> Option<Arc<Table>> {
        self.weak_self.upgrade()
    }

    // ---- reads ----

    fn point_sources(&self, st: &TableState, cmp: &Arc<dyn ClusteringComparator>, key: &PartitionKey, slices: &Slices, want_static: bool) -> Vec<Source> {
        let mut sources = Vec::new();
        for mt in st.memtables() {
            if let Some(p) = mt.partition(key) {
                sources.push(Source::Mem(MemSource::point(p, mt.max_timestamp(), cmp.clone(), slices.clone())));
            }
        }
        for sst in &st.sstables {
            if !sst.index.may_contain(key) {
                continue;
            }
            let stats = sst.stats();
            let hits = match (&stats.min_clustering, &stats.max_clustering) {
                (Some(lo), Some(hi)) => slices.intersects(&**cmp, lo, hi),
                _ => false,
            };
            let cur = SstCursor::point(sst.clone(), cmp.clone(), key.clone(), slices.clone());
            if hits {
                sources.push(Source::Sst(cur));
            } else if stats.partition_deletions > 0 || (want_static && stats.static_rows > 0) {
                sources.push(Source::Sst(cur.header_only()));
            }
        }
        sources
    }

    /// Read one partition.
    pub fn read(&self, r: PartitionRead) -> ReadStream {
        self.read_in(&self.state(), r)
    }

    /// Read one partition from the SSTables only: what has been flushed (ScyllaDB's
    /// large-partition guardrail looks at on-disk partitions).
    pub fn read_flushed(&self, r: PartitionRead) -> ReadStream {
        let opts = self.opts.read().clone();
        let st = self.state();
        let flushed = TableState { active: Arc::new(Memtable::new(opts.comparator.clone(), opts.reconcile)), frozen: Vec::new(), sstables: st.sstables.clone() };
        self.read_in(&flushed, r)
    }

    fn read_in(&self, st: &TableState, r: PartitionRead) -> ReadStream {
        self.metrics.reads.fetch_add(1, AO::Relaxed);
        let opts = self.opts.read().clone();
        let cmp = opts.comparator.clone();
        let slices = r.clustering.to_slices(&*cmp, r.reversed);
        let sources = self.point_sources(st, &cmp, &r.key, &slices, !r.columns.skip_static);
        let merger = Merger::new(cmp.clone(), opts.reconcile, r.reversed, sources);
        let state = ReadState::new(
            vec![Phase { merger, returned_in_first: 0 }],
            self.env.cache.clone(),
            cmp,
            r.columns,
            r.limits,
            r.options,
            opts.strict_liveness,
            opts.gc.gc_before(r.options.now),
            Arc::new(ReadMetrics::default()),
        );
        ReadStream::new(state)
    }

    /// The data sources, each on its own (ScyllaDB's `MUTATION_FRAGMENTS()`): the memtables
    /// (active first) and the SSTables, oldest first.
    pub fn sources(&self) -> Vec<(DataSource, TableState)> {
        let opts = self.opts.read().clone();
        let st = self.state();
        let empty = || Arc::new(Memtable::new(opts.comparator.clone(), opts.reconcile));
        let mut out = Vec::new();
        for (i, mt) in st.memtables().enumerate() {
            out.push((DataSource::Memtable(i), TableState { active: mt.clone(), frozen: Vec::new(), sstables: Vec::new() }));
        }
        for sst in &st.sstables {
            out.push((DataSource::SsTable(sst.key.clone()), TableState { active: empty(), frozen: Vec::new(), sstables: vec![sst.clone()] }));
        }
        out
    }

    /// The SSTables merged (what a row cache would hold once they are read).
    pub fn flushed(&self) -> TableState {
        let opts = self.opts.read().clone();
        TableState { active: Arc::new(Memtable::new(opts.comparator.clone(), opts.reconcile)), frozen: Vec::new(), sstables: self.state().sstables.clone() }
    }

    /// [`Table::read`] of the given sources.
    pub fn read_from(&self, st: &TableState, r: PartitionRead) -> ReadStream {
        self.read_in(st, r)
    }

    /// [`Table::scan`] of the given sources.
    pub fn scan_from(&self, st: &TableState, r: RangeRead) -> ReadStream {
        self.scan_in(Arc::new(st.clone()), r)
    }

    /// Read partitions in token order.
    pub fn scan(&self, r: RangeRead) -> ReadStream {
        self.scan_in(self.state(), r)
    }

    fn scan_in(&self, st: Arc<TableState>, r: RangeRead) -> ReadStream {
        self.metrics.range_reads.fetch_add(1, AO::Relaxed);
        let opts = self.opts.read().clone();
        let cmp = opts.comparator.clone();
        let slices = r.clustering.to_slices(&*cmp, r.reversed);
        let mut phases = Vec::new();
        let mut start = r.start.clone();
        if let Some(res) = &r.resume {
            if let Some(after) = &res.after {
                let s = slices_after(&*cmp, &slices, after);
                let sources = self.point_sources(&st, &cmp, &res.key, &s, !r.columns.skip_static);
                phases.push(Phase { merger: Merger::new(cmp.clone(), opts.reconcile, r.reversed, sources), returned_in_first: res.returned_in_partition });
            }
            start = KeyBound::Key { key: res.key.clone(), inclusive: false };
        }
        let mut sources = Vec::new();
        for mt in st.memtables() {
            if !mt.is_empty() {
                sources.push(Source::Mem(MemSource::scan(mt.clone(), start.clone(), r.end.clone(), slices.clone())));
            }
        }
        for sst in &st.sstables {
            let s = sst.stats();
            if sst.index.blocks.is_empty() || !start.admits_from(&s.last_key) || !r.end.admits_to(&s.first_key) {
                continue;
            }
            let hits = match (&s.min_clustering, &s.max_clustering) {
                (Some(lo), Some(hi)) => slices.intersects(&*cmp, lo, hi),
                _ => false,
            };
            let cur = SstCursor::scan(sst.clone(), cmp.clone(), start.clone(), r.end.clone(), slices.clone(), 4);
            if hits {
                sources.push(Source::Sst(cur));
            } else if s.partition_deletions > 0 || s.static_rows > 0 {
                sources.push(Source::Sst(cur.header_only()));
            }
        }
        phases.push(Phase { merger: Merger::new(cmp.clone(), opts.reconcile, r.reversed, sources), returned_in_first: 0 });
        let state = ReadState::new(
            phases,
            self.env.cache.clone(),
            cmp,
            r.columns,
            r.limits,
            r.options,
            opts.strict_liveness,
            opts.gc.gc_before(r.options.now),
            Arc::new(ReadMetrics::default()),
        );
        ReadStream::new(state)
    }

    // ---- flush ----

    /// Write the memtables out as SSTables and record them.
    pub async fn flush(&self) -> Result<()> {
        let _g = self.flush_lock.lock().await;
        let opts = self.opts.read().clone();
        let frozen = {
            let mut st = self.state.write();
            if !st.active.is_empty() {
                let mut next = (**st).clone();
                next.frozen.push(st.active.clone());
                next.active = Arc::new(Memtable::accounted(opts.comparator.clone(), opts.reconcile, self.memory.clone()));
                *st = Arc::new(next);
            }
            st.frozen.clone()
        };
        if frozen.is_empty() {
            return Ok(());
        }
        for m in &frozen {
            m.freeze();
        }
        // Frozen first, so writers move on to the new memtable while this one waits its turn.
        let _permit = self.env.flushes.clone().acquire_owned().await?;
        let run = rand::random::<u64>();
        for m in frozen {
            let sst = self.write_memtable(&m, &opts, run).await?;
            {
                let mut st = self.state.write();
                let mut next = (**st).clone();
                next.frozen.retain(|f| !Arc::ptr_eq(f, &m));
                if let Some(s) = &sst {
                    self.metrics.bytes_flushed.fetch_add(s.data_size(), AO::Relaxed);
                    next.sstables.push(s.clone());
                }
                *st = Arc::new(next);
            }
            self.metrics.flushes.fetch_add(1, AO::Relaxed);
        }
        self.record(true, false).await?;
        self.schedule_compaction();
        Ok(())
    }

    /// The flushed position: writes below it are all in SSTables (or were discarded).
    fn flushed_lsn(&self, host: &dyn TableHost) -> u64 {
        let w = host.low_watermark();
        let st = self.state();
        let unflushed = st.memtables().map(|m| m.min_lsn()).min().unwrap_or(u64::MAX);
        w.min(unflushed).saturating_sub(1)
    }

    async fn record(&self, flushed: bool, truncated: bool) -> Result<()> {
        let Some(host) = self.host() else { return Ok(()) };
        let flushed_lsn = flushed.then(|| self.flushed_lsn(&*host));
        let sstables = self.state().sstables.iter().map(|s| sst_ref(s)).collect();
        host.table_changed(self.id, Change { sstables, flushed_lsn, base_done: flushed, truncated }).await
    }

    async fn write_memtable(&self, m: &Arc<Memtable>, opts: &TableOptions, run: u64) -> Result<Option<Arc<SsTable>>> {
        if m.is_empty() {
            return Ok(None);
        }
        let m2 = m.clone();
        let wopts = WriterOptions { block_size: opts.block_size, codec: opts.codec, bloom_fp_chance: opts.bloom_filter_fp_chance, expected_partitions: m.partitions() };
        let cmp = opts.comparator.clone();
        let (data, index) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Index)> {
            let mut w = SstWriter::new(Vec::with_capacity((m2.bytes() / 2) as usize), cmp.clone(), wopts);
            for p in m2.all() {
                let h = p.header();
                w.begin_partition(&h.key, h.deletion, h.static_row)?;
                let mut c = MemCursor::new(p, cmp.clone(), Slices::all(false));
                while let Some(item) = c.next_item() {
                    match item {
                        Item::Row(r) => w.add_row(&r)?,
                        Item::Marker(mk) => w.add_marker(&mk)?,
                    }
                }
                w.end_partition()?;
            }
            w.finish()
        })
        .await??;
        if index.blocks.is_empty() {
            return Ok(None);
        }
        let s = self.upload(Data::Mem(Bytes::from(data)), index, 0, run).await?;
        Ok(Some(s))
    }

    pub(crate) async fn upload(&self, data: Data, index: Index, level: u32, run: u64) -> Result<Arc<SsTable>> {
        let key = sstable_key(&self.owner, self.id, &new_sstable_id());
        let raw_index = index.encode()?;
        let index_size = raw_index.len() as u64;
        #[cfg(test)]
        crate::failpoint::check(&key)?;
        match data {
            Data::Mem(b) => self.env.objects.put(&format!("{key}.data"), b).await?,
            Data::File(path) => {
                let f = tokio::fs::File::open(&path).await?;
                let r = self.env.objects.upload(&format!("{key}.data"), f).await;
                let _ = tokio::fs::remove_file(&path).await;
                r?;
            }
        }
        self.env.objects.put(&format!("{key}.index"), Bytes::from(raw_index)).await?;
        let source: Arc<dyn BlockSource> = Arc::new(self.env.objects.clone());
        Ok(Arc::new(SsTable::new(key, index, index_size, level, self.env.now_ms(), source).with_run(run)))
    }

    // ---- truncate ----

    /// Discard everything (`TRUNCATE`). The replaced SSTables stay in older manifest versions,
    /// so point-in-time restore can still reach them.
    pub async fn truncate(&self) -> Result<()> {
        let _f = self.flush_lock.lock().await;
        let _c = self.compaction_lock.lock().await;
        let opts = self.opts.read().clone();
        let old = {
            let mut st = self.state.write();
            let old = (**st).clone();
            *st = Arc::new(TableState {
                active: Arc::new(Memtable::accounted(opts.comparator.clone(), opts.reconcile, self.memory.clone())),
                frozen: Vec::new(),
                sstables: Vec::new(),
            });
            old
        };
        for m in old.memtables() {
            m.freeze();
        }
        for s in &old.sstables {
            self.env.cache.forget(s.uid);
        }
        self.record(true, true).await
    }

    // ---- compaction ----

    /// Turns background compaction of this table on or off (major compactions still run).
    pub fn set_auto_compaction(&self, on: bool) {
        self.auto_compaction_off.store(!on, AO::SeqCst);
        if on {
            self.schedule_compaction();
        }
    }

    pub(crate) fn schedule_compaction(&self) {
        if !self.env.auto_compaction || self.auto_compaction_off.load(AO::SeqCst) {
            return;
        }
        let Some(me) = self.self_arc() else { return };
        if self.compaction_scheduled.swap(true, AO::SeqCst) {
            return;
        }
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                loop {
                    match me.compact().await {
                        Ok(true) => continue,
                        Ok(false) => break,
                        Err(e) => {
                            tracing::warn!(table = %me.id, error = %e, "compaction failed");
                            break;
                        }
                    }
                }
                me.compaction_scheduled.store(false, AO::SeqCst);
            });
        } else {
            self.compaction_scheduled.store(false, AO::SeqCst);
        }
    }

    fn plan_inputs(&self) -> (Arc<TableState>, i64, i64, i64) {
        let st = self.state();
        let now = self.env.now_ms() / 1000;
        let gc_before = self.opts.read().gc.gc_before(now);
        let mt_min = st.memtables().map(|m| m.min_timestamp()).min().unwrap_or(i64::MAX);
        (st, now, gc_before, mt_min)
    }

    /// Run the strategy's next background compaction, if any. Returns whether one ran.
    pub async fn compact(&self) -> Result<bool> {
        let _g = self.compaction_lock.lock().await;
        let strategy = self.opts.read().compaction.strategy();
        if !strategy.enabled() || !self.compactions_allowed() || self.auto_compaction_off.load(AO::SeqCst) {
            return Ok(false);
        }
        let (st, now, gc_before, mt_min) = self.plan_inputs();
        let plan = Plan { sstables: &st.sstables, now, gc_before, memtable_min_timestamp: mt_min };
        let Some(task) = strategy.next_task(&plan) else { return Ok(false) };
        self.run_task(task).await?;
        Ok(true)
    }

    /// Background compactions waiting (`compaction_manager/metrics/pending_tasks`): one when
    /// the strategy has a task to run, whether or not background compaction is on.
    pub fn pending_compactions(&self) -> u64 {
        let strategy = self.opts.read().compaction.strategy();
        let (st, now, gc_before, mt_min) = self.plan_inputs();
        let plan = Plan { sstables: &st.sstables, now, gc_before, memtable_min_timestamp: mt_min };
        u64::from(strategy.next_task(&plan).is_some())
    }

    /// Major compaction (`nodetool compact`): everything together.
    pub async fn compact_major(&self) -> Result<()> {
        let _g = self.compaction_lock.lock().await;
        if !self.compactions_allowed() {
            return Ok(());
        }
        let strategy = self.opts.read().compaction.strategy();
        let (st, now, gc_before, mt_min) = self.plan_inputs();
        let plan = Plan { sstables: &st.sstables, now, gc_before, memtable_min_timestamp: mt_min };
        if let Some(task) = strategy.major_task(&plan) {
            self.run_task(task).await?;
        }
        Ok(())
    }

    /// Compact specific SSTables (tests and tools).
    pub async fn compact_these(&self, inputs: Vec<Arc<SsTable>>, level: u32, max_output_bytes: u64) -> Result<()> {
        let _g = self.compaction_lock.lock().await;
        if !self.compactions_allowed() {
            return Ok(());
        }
        self.run_task(Task { inputs, level, max_output_bytes, kind: TaskKind::Minor }).await
    }

    async fn run_task(&self, task: Task) -> Result<()> {
        if task.inputs.is_empty() {
            return Ok(());
        }
        let _permit = {
            let stop = self.compactions_stop.notified();
            tokio::pin!(stop);
            stop.as_mut().enable();
            if !self.compactions_allowed() {
                return Ok(());
            }
            tokio::select! {
                p = self.env.compactions.clone().acquire_owned() => p?,
                _ = stop => return Ok(()),
            }
        };
        let opts = self.opts.read().clone();
        let (st, now, gc_before, mt_min) = self.plan_inputs();
        let is_input = |s: &Arc<SsTable>| task.inputs.iter().any(|i| i.uid == s.uid);
        let others: Vec<Arc<SsTable>> = st.sstables.iter().filter(|s| !is_input(s)).cloned().collect();
        let expired = if task.kind == TaskKind::Expired { task.inputs.clone() } else { fully_expired(&task.inputs, &others, gc_before, mt_min) };
        let rest: Vec<Arc<SsTable>> = task.inputs.iter().filter(|s| !expired.iter().any(|e| e.uid == s.uid)).cloned().collect();
        let run = rand::random::<u64>();
        let mut outputs = Vec::new();
        if !rest.is_empty() {
            let me = self.self_arc().ok_or_else(|| anyhow::anyhow!("table is gone"))?;
            let rt = tokio::runtime::Handle::current();
            let ctx = CompactCtx { inputs: rest.clone(), others, opts: opts.clone(), gc_before, now, max_output: task.max_output_bytes };
            let written = match tokio::task::spawn_blocking(move || me.compact_blocking(ctx, &rt)).await? {
                Ok(w) => w,
                Err(e) if e.is::<Stopped>() => return Ok(()),
                Err(e) => return Err(e),
            };
            for (data, index) in written {
                let s = self.upload(data, index, task.level, run).await?;
                self.metrics.bytes_compacted.fetch_add(s.data_size(), AO::Relaxed);
                outputs.push(s);
            }
        }
        {
            let mut st = self.state.write();
            if !task.inputs.iter().all(|i| st.sstables.iter().any(|s| s.uid == i.uid)) {
                // Truncated meanwhile: the outputs are orphans for garbage collection.
                return Ok(());
            }
            let mut next = (**st).clone();
            next.sstables.retain(|s| !is_input(s));
            next.sstables.extend(outputs);
            *st = Arc::new(next);
        }
        for s in &task.inputs {
            self.env.cache.forget(s.uid);
        }
        self.metrics.compactions.fetch_add(1, AO::Relaxed);
        self.metrics.expired_sstables_dropped.fetch_add(expired.len() as u64, AO::Relaxed);
        self.record(false, false).await
    }

    /// Merge, purge and write (on the blocking pool).
    fn compact_blocking(&self, ctx: CompactCtx, rt: &tokio::runtime::Handle) -> Result<Vec<(Data, Index)>> {
        let cmp = ctx.opts.comparator.clone();
        let readahead = compaction_readahead(self.env.compaction_read_bytes, ctx.inputs.len(), ctx.opts.block_size);
        let sources = ctx
            .inputs
            .iter()
            .map(|s| Source::Sst(SstCursor::scan(s.clone(), cmp.clone(), KeyBound::Unbounded, KeyBound::Unbounded, Slices::all(false), readahead)))
            .collect();
        let mut merger = Merger::new(cmp.clone(), ctx.opts.reconcile, false, sources);
        let expected: u64 = ctx.inputs.iter().map(|s| s.stats().partitions).sum();
        let wopts = WriterOptions { block_size: ctx.opts.block_size, codec: ctx.opts.codec, bloom_fp_chance: ctx.opts.bloom_filter_fp_chance, expected_partitions: expected.max(1) };
        let tmp = self.env.tmp_dir.clone();
        let new_writer = || -> Result<SstWriter<Sink>> { Ok(SstWriter::new(Sink::new(tmp.as_deref())?, cmp.clone(), wopts.clone())) };
        let mut out = Vec::new();
        let mut w = new_writer()?;
        let strict = ctx.opts.strict_liveness;
        let cache = &self.env.cache;
        while let Some(h) = drive_blocking(&mut merger, cache, rt, |m| m.next_partition())? {
            if !self.compactions_allowed() {
                w.abandon().discard();
                for (d, _) in out {
                    Data::discard(d);
                }
                return Err(Stopped.into());
            }
            // The oldest data elsewhere that a tombstone of this partition could shadow: in
            // the other SSTables, and the partition's live data in the memtables (tombstones
            // there shadow nothing compaction drops).
            let mut max_purgeable = i64::MAX;
            for o in &ctx.others {
                if o.index.may_contain(&h.key) {
                    max_purgeable = max_purgeable.min(o.stats().min_timestamp);
                }
            }
            let mut max_purgeable_shadowable = max_purgeable;
            for mt in self.state().memtables() {
                if let Some(p) = mt.partition(&h.key) {
                    let (live, marker) = p.min_live_timestamps(ctx.now);
                    max_purgeable = max_purgeable.min(live);
                    max_purgeable_shadowable = max_purgeable_shadowable.min(marker);
                }
            }
            let purger = GcPurger { gc_before: ctx.gc_before, max_purgeable, max_purgeable_shadowable, marker_dated_at_write: ctx.opts.reconcile == crate::Reconcile::Scylla };
            let deletion = if purger.should_purge_deletion(&h.deletion) { DeletionTime::LIVE } else { h.deletion };
            let static_row = h.static_row.and_then(|s| purge_row(s, &purger, ctx.now, false));
            w.begin_partition(&h.key, deletion, static_row)?;
            while let Some(item) = drive_blocking(&mut merger, cache, rt, |m| m.next_item())? {
                match item {
                    Item::Row(r) => {
                        if let Some(r) = purge_row(r, &purger, ctx.now, strict) {
                            w.add_row(&r)?;
                        }
                    }
                    Item::Marker(mk) => {
                        if let Some(mk) = purge_marker(mk, &purger) {
                            w.add_marker(&mk)?;
                        }
                    }
                }
            }
            w.end_partition()?;
            if w.bytes_written() >= ctx.max_output {
                let (sink, index) = std::mem::replace(&mut w, new_writer()?).finish()?;
                out.push((sink.done()?, index));
            }
        }
        if !w.is_empty() {
            let (sink, index) = w.finish()?;
            out.push((sink.done()?, index));
        } else {
            w.finish()?.0.discard();
        }
        Ok(out)
    }

    // ---- stats ----

    pub fn stats(&self) -> TableStats {
        let st = self.state();
        let now = self.env.now_ms() / 1000;
        let gc_before = self.opts.read().gc.gc_before(now);
        let mut s = TableStats { sstables: st.sstables.len(), ..Default::default() };
        for t in &st.sstables {
            let lvl = t.level as usize;
            if s.sstables_per_level.len() <= lvl {
                s.sstables_per_level.resize(lvl + 1, 0);
            }
            s.sstables_per_level[lvl] += 1;
            s.data_bytes += t.data_size();
            s.index_bytes += t.index_size;
            s.uncompressed_bytes += t.stats().uncompressed_bytes;
            s.cache_bytes += self.env.cache.sstable_bytes(t.uid);
            s.partitions_estimate += t.stats().partitions;
            s.tombstones += t.stats().tombstones;
            s.droppable_tombstones += t.stats().droppable_tombstones(gc_before);
            s.max_partition_bytes = s.max_partition_bytes.max(t.stats().max_partition_bytes);
            s.bloom_filter_bytes += t.index.bloom.size_bytes() as u64;
            s.bloom_filter_false_positives += t.bloom_false_positives.load(AO::Relaxed);
        }
        s.space_used = s.data_bytes + s.index_bytes;
        for m in st.memtables() {
            s.memtable_bytes += m.bytes();
            s.memtable_partitions += m.partitions();
            s.memtable_rows += m.rows();
        }
        s.partitions_estimate += st.active.partitions();
        s.writes = self.metrics.writes.load(AO::Relaxed);
        s.reads = self.metrics.reads.load(AO::Relaxed);
        s.range_reads = self.metrics.range_reads.load(AO::Relaxed);
        s.flushes = self.metrics.flushes.load(AO::Relaxed);
        s.compactions = self.metrics.compactions.load(AO::Relaxed);
        s
    }

    /// Bytes held by memtables (active and frozen).
    pub fn memtable_bytes(&self) -> u64 {
        self.state().memtables().map(|m| m.bytes()).sum()
    }

    /// Bytes of the active memtable (what a flush would free).
    pub fn active_bytes(&self) -> u64 {
        self.state().active.bytes()
    }

    /// Bytes of frozen memtables (being flushed).
    pub fn frozen_bytes(&self) -> u64 {
        self.state().frozen.iter().map(|m| m.bytes()).sum()
    }

    /// In-memory size of the SSTables' indexes and bloom filters.
    pub fn index_bytes(&self) -> u64 {
        self.state().sstables.iter().map(|s| s.index_size).sum()
    }

    pub(crate) fn memtables_empty(&self) -> bool {
        let st = self.state();
        st.frozen.is_empty() && st.active.is_empty()
    }

    pub(crate) fn active_age_ms(&self) -> u64 {
        self.state().active.created.elapsed().as_millis() as u64
    }
}

/// SSTable data being written: in memory, or in a scratch file (compaction outputs can be large).
pub(crate) enum Sink {
    Mem(Vec<u8>),
    File(std::io::BufWriter<std::fs::File>, PathBuf),
}

/// Written SSTable data, ready to upload.
pub(crate) enum Data {
    Mem(Bytes),
    File(PathBuf),
}

impl Data {
    fn discard(self) {
        if let Data::File(path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A compaction gave up because compactions stopped (unload, drop).
#[derive(Debug)]
struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("compaction stopped")
    }
}

impl std::error::Error for Stopped {}

/// Blocks each input of a compaction reads ahead: `budget` bytes of decoded blocks over all
/// inputs, between 1 and 64 blocks each.
pub(crate) fn compaction_readahead(budget: u64, inputs: usize, block_size: usize) -> usize {
    (budget / (inputs.max(1) as u64 * block_size.max(1) as u64)).clamp(1, 64) as usize
}

impl Sink {
    pub(crate) fn new(dir: Option<&std::path::Path>) -> Result<Self> {
        Ok(match dir {
            None => Sink::Mem(Vec::new()),
            Some(d) => {
                let path = d.join(format!("compaction-{}.data", new_sstable_id()));
                Sink::File(std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&path)?), path)
            }
        })
    }

    pub(crate) fn done(self) -> Result<Data> {
        Ok(match self {
            Sink::Mem(v) => Data::Mem(Bytes::from(v)),
            Sink::File(w, path) => {
                w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
                Data::File(path)
            }
        })
    }

    pub(crate) fn discard(self) {
        if let Sink::File(_, path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Mem(v) => v.write(buf),
            Sink::File(f, _) => f.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Mem(_) => Ok(()),
            Sink::File(f, _) => f.flush(),
        }
    }
}

struct CompactCtx {
    inputs: Vec<Arc<SsTable>>,
    others: Vec<Arc<SsTable>>,
    opts: TableOptions,
    gc_before: i64,
    now: i64,
    max_output: u64,
}

/// Sort a row's columns and complex cells; merge duplicates.
fn normalize_row(cmp: &dyn ClusteringComparator, mode: crate::reconcile::Reconcile, mut row: Row) -> Option<Row> {
    for c in &mut row.columns {
        if let ColumnData::Complex { column, cells, .. } = c {
            let column = *column;
            if !cells.windows(2).all(|w| compare_paths(cmp, column, w[0].path.as_deref(), w[1].path.as_deref()) == Ordering::Less) {
                cells.sort_by(|a, b| compare_paths(cmp, column, a.path.as_deref(), b.path.as_deref()));
                let mut out: Vec<Cell> = Vec::with_capacity(cells.len());
                for cell in cells.drain(..) {
                    match out.last_mut() {
                        Some(last) if compare_paths(cmp, column, last.path.as_deref(), cell.path.as_deref()) == Ordering::Equal => {
                            let prev = std::mem::replace(last, Cell::tombstone(0, 0));
                            *last = reconcile(mode, prev, cell);
                        }
                        _ => out.push(cell),
                    }
                }
                *cells = out;
            }
        }
    }
    let has_deletions = !row.deletion.is_live() || row.columns.iter().any(|c| matches!(c, ColumnData::Complex { deletion, .. } if !deletion.is_live()));
    if has_deletions || !row.columns.windows(2).all(|w| w[0].column() < w[1].column()) {
        // Duplicate or unsorted columns, or data its own deletions shadow: a merge sorts,
        // reconciles and applies them (rows are canonical from here on).
        let clustering = row.clustering.clone();
        return merge_rows(cmp, mode, vec![row, Row::new(clustering)], DeletionTime::LIVE);
    }
    if row.is_empty() { None } else { Some(row) }
}

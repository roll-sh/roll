//! The node's storage: every database it serves, the commit log shipper, the block cache, and
//! the lifecycle operations — open, unload, branch, point-in-time restore — plus background
//! maintenance (memory pressure flushes, periodic flushes, lazy manifests, history expiry).
//!
//! Branches and restores copy nothing. A branch's first manifest references its parent's
//! SSTables by key and the parent's commit log frames up to the branch point (a *base log*);
//! an in-place restore archives the current manifest under `<db>.<unix ms>` and rewinds to the
//! newest manifest version written before the target, replaying the database's own frames up
//! to it. Follows `redis_server::node` / `persist` of the Redis product.

use crate::cache::{BlockCache, CacheConfig};
use crate::db::{Database, DbConfig};
use crate::gc::{self, GcStats};
use crate::log::{LogConfig, Shipper};
use crate::memory::{MemoryStats, MemtableBudget};
use crate::manifest::{self, BaseLog, Manifest, Origin, SegRef, TableEntry};
use crate::table::{Clock, Env, system_clock};
use anyhow::{Result, bail};
use parking_lot::Mutex;
use pg_storage::Objects;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

#[derive(Clone)]
pub struct StoreConfig {
    /// This node's id: commit log segments go under `nosql/log/<node>/`.
    pub node: String,
    pub cache: CacheConfig,
    pub log: LogConfig,
    /// Scratch space for compaction (in memory without one).
    pub tmp_dir: Option<PathBuf>,
    /// Memtables of all databases together ([`crate::memory`]): past half of it the largest
    /// are flushed; at it, writes of the databases holding more than their share wait.
    pub memtable_budget_bytes: u64,
    /// A database's memtable cap unless its config sets one (`DbConfig::memtable_space_bytes`).
    pub memtable_db_bytes: u64,
    /// Memtables written out at once (Cassandra's `memtable_flush_writers`).
    pub flush_concurrency: usize,
    /// How long a write waits for memtable memory before it fails (`write_request_timeout`).
    pub write_timeout: Duration,
    pub maintenance_interval: Duration,
    /// How often changed manifests are rewritten (new segments, idle tables).
    pub manifest_interval: Duration,
    pub compaction_concurrency: usize,
    /// Decoded blocks one compaction reads ahead, over all its inputs.
    pub compaction_read_bytes: u64,
    pub auto_compaction: bool,
    pub clock: Option<Clock>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            node: "node-1".into(),
            cache: CacheConfig::default(),
            log: LogConfig::default(),
            tmp_dir: None,
            memtable_budget_bytes: 512 << 20,
            memtable_db_bytes: 256 << 20,
            flush_concurrency: 2,
            write_timeout: Duration::from_secs(2),
            maintenance_interval: Duration::from_secs(1),
            manifest_interval: Duration::from_secs(10),
            compaction_concurrency: 2,
            compaction_read_bytes: 16 << 20,
            auto_compaction: true,
            clock: None,
        }
    }
}

pub struct Store {
    pub env: Arc<Env>,
    pub shipper: Arc<Shipper>,
    cfg: StoreConfig,
    dbs: Mutex<HashMap<String, Arc<Database>>>,
    /// Serialises open / unload / branch / restore.
    transitions: tokio::sync::Mutex<()>,
}

impl Store {
    pub async fn start(objects: Objects, cfg: StoreConfig) -> Result<Arc<Self>> {
        let cache = BlockCache::new(cfg.cache.clone())?;
        if let Some(d) = &cfg.tmp_dir {
            std::fs::create_dir_all(d)?;
        }
        let env = Arc::new(Env {
            objects: objects.clone(),
            cache,
            tmp_dir: cfg.tmp_dir.clone(),
            compactions: Arc::new(tokio::sync::Semaphore::new(cfg.compaction_concurrency.max(1))),
            auto_compaction: cfg.auto_compaction,
            clock: cfg.clock.clone().unwrap_or_else(system_clock),
            memory: MemtableBudget::new(cfg.memtable_budget_bytes),
            flushes: Arc::new(tokio::sync::Semaphore::new(cfg.flush_concurrency.max(1))),
            compaction_read_bytes: cfg.compaction_read_bytes,
            write_timeout: cfg.write_timeout,
        });
        let shipper = Shipper::start(cfg.node.clone(), objects, cfg.log.clone()).await?;
        let store = Arc::new(Self { env, shipper, cfg, dbs: Mutex::new(HashMap::new()), transitions: tokio::sync::Mutex::new(()) });
        let weak = Arc::downgrade(&store);
        let interval = store.cfg.maintenance_interval;
        tokio::spawn(async move { maintenance_loop(weak, interval).await });
        let weak = Arc::downgrade(&store);
        let budget = store.env.memory.clone();
        tokio::spawn(async move {
            loop {
                budget.relief_requested(Duration::from_secs(1)).await;
                let Some(store) = weak.upgrade() else { return };
                store.relieve();
                drop(store);
                // Writers ask again with every write past a threshold: at most 100 passes a second.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        Ok(store)
    }

    pub fn config(&self) -> &StoreConfig {
        &self.cfg
    }

    /// The node's memory: budgets, what memtables, caches and indexes hold, the allocator.
    pub fn memory_stats(&self) -> MemoryStats {
        let budget = &self.env.memory;
        let m = &budget.metrics;
        let get = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
        let mut s = MemoryStats {
            memtable_budget_bytes: budget.limit(),
            memtable_db_bytes: self.cfg.memtable_db_bytes.min(budget.limit()),
            memtable_bytes: budget.used(),
            databases_with_memtables: budget.holders(),
            blocked_writes: get(&m.blocked_writes),
            blocked_writes_now: get(&m.blocked_now),
            write_timeouts: get(&m.write_timeouts),
            pressure_flushes: get(&m.pressure_flushes),
            flush_concurrency: self.cfg.flush_concurrency.max(1) as u64,
            flushes_running: (self.cfg.flush_concurrency.max(1) - self.env.flushes.available_permits()) as u64,
            compaction_concurrency: self.cfg.compaction_concurrency.max(1) as u64,
            compactions_running: (self.cfg.compaction_concurrency.max(1) - self.env.compactions.available_permits()) as u64,
            cache_memory_bytes: self.env.cache.memory_bytes(),
            cache_memory_budget_bytes: self.cfg.cache.memory_bytes,
            cache_disk_bytes: self.env.cache.disk_bytes(),
            cache_disk_budget_bytes: if self.cfg.cache.disk_dir.is_some() { self.cfg.cache.disk_bytes } else { 0 },
            log_pending_bytes: self.shipper.pending_bytes() as u64,
            log_pending_limit_bytes: self.shipper.max_pending_bytes() as u64,
            process: crate::memory::process_memory(),
            ..Default::default()
        };
        for d in self.loaded() {
            for t in d.tables() {
                s.memtable_flushing_bytes += t.frozen_bytes();
                s.sstable_index_bytes += t.index_bytes();
            }
        }
        s
    }

    /// Flush memtables that hold too much ([`crate::memory`]): a database's largest active
    /// memtables while its unflushed ones hold half its cap or more, then the node's largest
    /// while all unflushed ones hold half the budget or more. Flushes run in the background.
    pub fn relieve(&self) {
        let budget = &self.env.memory;
        let mut node_unflushed = 0u64;
        let mut candidates: Vec<(u64, Arc<crate::table::Table>)> = Vec::new();
        let mut started = 0u64;
        for d in self.loaded() {
            let mut active: Vec<(u64, Arc<crate::table::Table>)> = d.tables().into_iter().map(|t| (t.active_bytes(), t)).filter(|(b, _)| *b > 0).collect();
            active.sort_by_key(|(b, _)| std::cmp::Reverse(*b));
            let mut unflushed: u64 = active.iter().map(|(b, _)| *b).sum();
            let soft = d.memory().cap() / 2;
            for (b, t) in active {
                if unflushed >= soft && unflushed > 0 {
                    t.request_flush();
                    started += 1;
                    unflushed -= b;
                } else {
                    candidates.push((b, t));
                }
            }
            node_unflushed += unflushed;
        }
        candidates.sort_by_key(|(b, _)| std::cmp::Reverse(*b));
        for (b, t) in candidates {
            if node_unflushed < budget.limit() / 2 {
                break;
            }
            t.request_flush();
            started += 1;
            node_unflushed -= b;
        }
        budget.metrics.pressure_flushes.fetch_add(started, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn objects(&self) -> &Objects {
        &self.env.objects
    }

    pub fn now_ms(&self) -> i64 {
        self.env.now_ms()
    }

    /// Load a database (or return it if loaded).
    pub async fn open(&self, id: &str, cfg: DbConfig) -> Result<Arc<Database>> {
        if let Some(db) = self.get(id) {
            return Ok(db);
        }
        let _t = self.transitions.lock().await;
        if let Some(db) = self.get(id) {
            return Ok(db);
        }
        let db = Database::open(self.env.clone(), self.shipper.clone(), id, cfg, self.cfg.memtable_db_bytes).await?;
        self.dbs.lock().insert(id.to_string(), db.clone());
        tracing::info!(db = id, "nosql database loaded");
        Ok(db)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Database>> {
        self.dbs.lock().get(id).cloned()
    }

    pub fn loaded(&self) -> Vec<Arc<Database>> {
        self.dbs.lock().values().cloned().collect()
    }

    /// Unload a database: flush its memtables, write its manifest, release it. `false` when it
    /// wasn't loaded.
    pub async fn unload(&self, id: &str) -> Result<bool> {
        let _t = self.transitions.lock().await;
        self.unload_locked(id).await
    }

    async fn unload_locked(&self, id: &str) -> Result<bool> {
        let Some(db) = self.get(id) else { return Ok(false) };
        db.close().await?;
        self.dbs.lock().remove(id);
        drop(db);
        crate::memory::release_to_os();
        tracing::info!(db = id, "nosql database unloaded");
        Ok(true)
    }

    /// Ship buffered frames and rewrite a loaded database's manifest, so storage reflects it.
    async fn sync_to_storage(&self, id: &str) -> Result<()> {
        if let Some(db) = self.get(id) {
            self.shipper.flush().await?;
            db.write_manifest(false).await?;
        }
        Ok(())
    }

    /// Create `child` from `parent`'s state at `at_ms` (latest when `None`). Copies nothing.
    pub async fn branch(&self, parent: &str, child: &str, at_ms: Option<i64>) -> Result<()> {
        let _t = self.transitions.lock().await;
        let objects = self.objects();
        if manifest::read_manifest(objects, child).await?.is_some() {
            bail!("database {child} already exists");
        }
        self.sync_to_storage(parent).await?;
        let Some(current) = manifest::read_manifest(objects, parent).await? else { bail!("database {parent} does not exist") };
        let mut m = based_on(objects, parent, &current, at_ms, child).await?;
        m.origin = Some(Origin { parent: parent.to_string(), at_ms: at_ms.unwrap_or_else(|| self.now_ms()), restore: false });
        m.version = 1;
        m.written_ms = self.now_ms();
        manifest::write_manifest(objects, child, &m, true).await
    }

    /// Restore `id` to its state at `at_ms`. With `into`, as a new database (a branch);
    /// otherwise in place: clients must reconnect, and the replaced state is archived under the
    /// returned id (`<id>.<unix ms>`), from which it can be branched back.
    pub async fn restore_at(&self, id: &str, at_ms: i64, into: Option<&str>) -> Result<Option<String>> {
        if let Some(new) = into {
            self.branch(id, new, Some(at_ms)).await?;
            return Ok(None);
        }
        let _t = self.transitions.lock().await;
        self.unload_locked(id).await?;
        let objects = self.objects();
        let Some(current) = manifest::read_manifest(objects, id).await? else { bail!("database {id} does not exist") };
        let now = self.now_ms();
        let archive = format!("{id}.{now}");
        let archived = Manifest { node: None, ..current.clone() };
        manifest::write_manifest(objects, &archive, &archived, true).await?;
        let mut m = based_on(objects, id, &current, Some(at_ms), id).await?;
        // The database keeps its stream; frames of the replaced timeline are never replayed as
        // its own again.
        m.stream = current.stream.clone();
        m.replay_from = current.next_lsn;
        m.next_lsn = current.next_lsn;
        for e in m.tables.values_mut() {
            e.flushed_lsn = current.next_lsn.saturating_sub(1);
        }
        m.segments = current.segments.clone();
        m.history_floor = current.history_floor;
        m.version = current.version + 1;
        m.written_ms = now;
        m.origin = Some(Origin { parent: archive.clone(), at_ms, restore: true });
        manifest::write_manifest(objects, id, &m, true).await?;
        Ok(Some(archive))
    }

    /// Delete a database's manifests (control plane). Unloads it first.
    pub async fn delete_database(&self, id: &str) -> Result<usize> {
        let _t = self.transitions.lock().await;
        if let Some(db) = self.dbs.lock().remove(id) {
            self.shipper.unregister(&db.id);
        }
        gc::delete_database(self.objects(), id).await
    }

    pub async fn collect_garbage(&self, min_age: Duration) -> Result<GcStats> {
        gc::collect_garbage(self.objects(), min_age, self.now_ms()).await
    }

    /// One maintenance pass (the background loop runs this every `maintenance_interval`).
    pub async fn maintain(&self) -> Result<()> {
        let dbs = self.loaded();
        // Memory pressure (also relieved at once when writes cross a threshold).
        self.relieve();
        // memtable_flush_period_in_ms.
        for d in &dbs {
            for t in d.tables() {
                let period = t.options().memtable_flush_period_ms;
                if period > 0 && !t.memtables_empty() && t.active_age_ms() >= period {
                    t.flush().await?;
                }
            }
        }
        Ok(())
    }

    /// Rewrite manifests that changed, and expire old history.
    pub async fn write_manifests(&self, expire: bool) -> Result<()> {
        for d in self.loaded() {
            if expire {
                d.expire_history().await?;
            }
            if d.is_dirty() {
                d.write_manifest(false).await?;
            }
        }
        Ok(())
    }
}

async fn maintenance_loop(weak: Weak<Store>, interval: Duration) {
    let mut last_manifests = tokio::time::Instant::now();
    let mut last_expiry = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(interval).await;
        let Some(store) = weak.upgrade() else { return };
        if let Err(e) = store.maintain().await {
            tracing::warn!(error = %e, "nosql maintenance failed");
        }
        if last_manifests.elapsed() >= store.cfg.manifest_interval {
            last_manifests = tokio::time::Instant::now();
            let expire = last_expiry.elapsed() >= Duration::from_secs(600);
            if expire {
                last_expiry = tokio::time::Instant::now();
            }
            if let Err(e) = store.write_manifests(expire).await {
                tracing::warn!(error = %e, "nosql manifest write failed");
            }
        }
    }
}

/// The manifest of a new state equal to `source`'s at `at_ms`: `source`'s SSTables of the
/// newest version written by then, and its frames from there up to `at_ms` as a base log.
async fn based_on(objects: &Objects, source: &str, current: &Manifest, at_ms: Option<i64>, new_id: &str) -> Result<Manifest> {
    let v = match at_ms {
        None => current.clone(),
        Some(t) => match manifest::manifest_at(objects, source, current, t).await? {
            Some(v) => v,
            None => bail!("{source}: the requested point is older than the retained history"),
        },
    };
    let until_lsn = current.next_lsn.saturating_sub(1);
    // Segments the base needs: the current manifest's (complete within retention), plus any
    // the owning node shipped after it.
    let mut segments: Vec<SegRef> = current.segments.iter().filter(|s| s.last_lsn >= v.replay_from && s.first_lsn <= until_lsn).cloned().collect();
    if let Some(node) = &current.node {
        for (k, _) in objects.list(&format!("nosql/log/{node}/")).await? {
            if let Some(seq) = manifest::segment_seq(&k)
                && seq > current.after_seq
                && !segments.iter().any(|s| s.node == *node && s.seq == seq)
            {
                segments.push(SegRef { node: node.clone(), seq, first_lsn: 0, last_lsn: u64::MAX, last_ms: 0 });
            }
        }
    }
    let mut bases: Vec<BaseLog> = v
        .bases
        .iter()
        .map(|b| {
            let mut b = b.clone();
            for (t, e) in &v.tables {
                if e.base_done {
                    b.skip.insert(*t);
                }
            }
            b
        })
        .collect();
    bases.push(BaseLog {
        stream: v.stream.clone(),
        segments,
        min_lsn: v.replay_from,
        until_lsn,
        until_ms: at_ms.unwrap_or(i64::MAX),
        flushed: v.tables.iter().map(|(t, e)| (*t, e.flushed_lsn)).collect(),
        skip: Default::default(),
    });
    let tables: BTreeMap<_, _> = v.tables.iter().map(|(t, e)| (*t, TableEntry { sstables: e.sstables.clone(), flushed_lsn: 0, base_done: false, truncated_ms: e.truncated_ms })).collect();
    let mut m = Manifest::new(new_id);
    m.tables = tables;
    m.dropped = v.dropped.clone();
    m.bases = bases;
    Ok(m)
}

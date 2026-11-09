//! A database: its tables, its commit log stream and its manifest.
//!
//! Opening a database reads its manifest and the commit log frames not yet in SSTables (the
//! base logs of a branch or restore first, then its own, then whatever its last node shipped
//! after the manifest was written). Frames are kept per table until the engine attaches the
//! table with its comparator ([`Database::table`]): schema lives in the system tables, so the
//! engine attaches those first, reads the schema, then attaches the rest.
//!
//! A write takes the next log position, appends its frame to the node's shipper, applies to
//! the memtable and, with `durability = sync`, waits for its segment. Positions in flight are
//! tracked so a flush knows exactly which frames its SSTable covers (`flushed_lsn`).

use crate::codec::{decode_update, encode_update};
use crate::log::{Durability, SegmentSink, Shipper, decode_payload, read_segment};
use crate::manifest::{self, Manifest, SegRef, TableEntry, TableId};
use crate::memory::{DbMemory, DbMemoryStats};
use crate::model::PartitionUpdate;
use crate::options::TableOptions;
use crate::sstable::SsTable;
use crate::table::{Change, Env, Table, TableHost, TableStats};
use anyhow::{Result, bail};
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{StreamExt, TryStreamExt};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DbConfig {
    pub durability: Durability,
    /// How long older manifest versions and segments are kept for point-in-time restore.
    pub history_retention_ms: i64,
    /// Cap on this database's memtables (its plan; `None`: the node's per-database default).
    /// Never above the node's memtable budget.
    #[serde(default)]
    pub memtable_space_bytes: Option<u64>,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self { durability: Durability::Async, history_retention_ms: 7 * 86_400_000, memtable_space_bytes: None }
    }
}

struct LogState {
    next_lsn: u64,
    in_flight: BTreeSet<u64>,
}

struct Meta {
    m: Manifest,
    dirty: bool,
}

/// Segments fetched ahead while replaying.
const PREFETCH: usize = 8;

pub struct Database {
    pub id: String,
    /// The id this database's frames are written under.
    stream: String,
    pub(crate) env: Arc<Env>,
    pub(crate) shipper: Arc<Shipper>,
    cfg: RwLock<DbConfig>,
    log: Mutex<LogState>,
    meta: Mutex<Meta>,
    write_lock: tokio::sync::Mutex<()>,
    attach_lock: tokio::sync::Mutex<()>,
    tables: RwLock<HashMap<TableId, Arc<Table>>>,
    /// Replayed frames of tables not attached yet: `(lsn, update)`; base-log frames have lsn 0.
    pending: Mutex<HashMap<TableId, Vec<(u64, Bytes)>>>,
    closed: AtomicBool,
    weak: Weak<Database>,
    /// Its memtable memory ([`crate::memory`]).
    memory: Arc<DbMemory>,
    /// The node's per-database cap, for a config without one.
    default_memtable_space: u64,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").field("id", &self.id).finish()
    }
}

struct Replay {
    pending: HashMap<TableId, Vec<(u64, Bytes)>>,
    own_max: u64,
    found: Vec<SegRef>,
}

/// Read the frames a manifest says must be replayed.
async fn load_replay(objects: &pg_storage::Objects, m: &Manifest) -> Result<Replay> {
    let mut pending: HashMap<TableId, Vec<(u64, Bytes)>> = HashMap::new();
    for base in &m.bases {
        let low = m.tables.iter().filter(|(t, _)| !base.skip.contains(t)).map(|(t, _)| base.flushed.get(t).copied().unwrap_or(0)).min().unwrap_or(0);
        let segs: Vec<SegRef> = dedupe(base.segments.iter().filter(|s| s.last_lsn > low && s.first_lsn <= base.until_lsn).cloned().collect());
        let mut fetched = futures::stream::iter(segs.into_iter().map(|s| async move {
            read_segment(objects, &s.node, s.seq).await?.ok_or_else(|| anyhow::anyhow!("segment {} is missing", manifest::segment_key(&s.node, s.seq)))
        }))
        .buffered(PREFETCH);
        while let Some(frames) = fetched.try_next().await? {
            for f in frames {
                if f.db != base.stream || f.lsn < base.min_lsn || f.lsn > base.until_lsn || f.at_ms > base.until_ms {
                    continue;
                }
                for (t, u) in decode_payload(&f.payload)? {
                    if m.dropped.contains(&t) || base.skip.contains(&t) || m.tables.get(&t).is_some_and(|e| e.base_done) {
                        continue;
                    }
                    if f.lsn <= base.flushed.get(&t).copied().unwrap_or(0) {
                        continue;
                    }
                    pending.entry(t).or_default().push((0, u));
                }
            }
        }
    }
    // Own frames: listed segments, plus the last node's later ones (it may have crashed
    // before rewriting the manifest).
    let low = m.min_flushed().max(m.replay_from.saturating_sub(1));
    let mut segs: Vec<SegRef> = m.segments.iter().filter(|s| s.last_lsn > low).cloned().collect();
    let listed: HashSet<(String, u64)> = m.segments.iter().map(|s| (s.node.clone(), s.seq)).collect();
    let mut unlisted = HashSet::new();
    if let Some(node) = &m.node {
        for (k, _) in objects.list(&format!("nosql/log/{node}/")).await? {
            if let Some(seq) = manifest::segment_seq(&k)
                && seq > m.after_seq
                && !listed.contains(&(node.clone(), seq))
            {
                unlisted.insert((node.clone(), seq));
                segs.push(SegRef { node: node.clone(), seq, first_lsn: 0, last_lsn: u64::MAX, last_ms: 0 });
            }
        }
    }
    segs = dedupe(segs);
    let mut own_max = 0;
    let mut found = Vec::new();
    let mut fetched = futures::stream::iter(segs.into_iter().map(|s| async move {
        let frames = read_segment(objects, &s.node, s.seq).await?;
        anyhow::Ok((s, frames))
    }))
    .buffered(PREFETCH);
    while let Some((seg, frames)) = fetched.try_next().await? {
        let Some(frames) = frames else {
            if unlisted.contains(&(seg.node.clone(), seg.seq)) {
                continue;
            }
            bail!("segment {} is missing", manifest::segment_key(&seg.node, seg.seq));
        };
        let mut range: Option<(u64, u64, i64)> = None;
        for f in frames {
            if f.db != m.stream {
                continue;
            }
            let r = range.get_or_insert((f.lsn, f.lsn, f.at_ms));
            r.0 = r.0.min(f.lsn);
            r.1 = r.1.max(f.lsn);
            r.2 = r.2.max(f.at_ms);
            if f.lsn < m.replay_from {
                continue;
            }
            own_max = own_max.max(f.lsn);
            for (t, u) in decode_payload(&f.payload)? {
                if m.dropped.contains(&t) || f.lsn <= m.tables.get(&t).map_or(0, |e| e.flushed_lsn) {
                    continue;
                }
                pending.entry(t).or_default().push((f.lsn, u));
            }
        }
        if unlisted.contains(&(seg.node.clone(), seg.seq))
            && let Some((first, last, ms)) = range
        {
            found.push(SegRef { node: seg.node, seq: seg.seq, first_lsn: first, last_lsn: last, last_ms: ms });
        }
    }
    Ok(Replay { pending, own_max, found })
}

fn dedupe(mut segs: Vec<SegRef>) -> Vec<SegRef> {
    let mut seen = HashSet::new();
    segs.retain(|s| seen.insert((s.node.clone(), s.seq)));
    segs.sort_by(|a, b| (a.node.as_str(), a.seq).cmp(&(b.node.as_str(), b.seq)));
    segs
}

impl Database {
    /// Load a database (an absent manifest starts an empty one, written as the first numbered
    /// version so point-in-time branches and restores between creation and the first flush
    /// find the empty state).
    pub(crate) async fn open(env: Arc<Env>, shipper: Arc<Shipper>, id: &str, cfg: DbConfig, default_memtable_space: u64) -> Result<Arc<Self>> {
        let objects = env.objects.clone();
        let stored = manifest::read_manifest(&objects, id).await?;
        let first = stored.is_none();
        let mut m = stored.unwrap_or_else(|| Manifest::new(id));
        let replay = load_replay(&objects, &m).await?;
        m.segments.extend(replay.found);
        m.segments = dedupe(std::mem::take(&mut m.segments));
        m.next_lsn = m.next_lsn.max(replay.own_max + 1).max(1);
        m.node = Some(shipper.node.clone());
        let next_lsn = m.next_lsn;
        let stream = m.stream.clone();
        let memory = env.memory.account(cfg.memtable_space_bytes.unwrap_or(default_memtable_space));
        let db = Arc::new_cyclic(|weak| Database {
            id: id.to_string(),
            stream,
            env,
            shipper: shipper.clone(),
            cfg: RwLock::new(cfg),
            log: Mutex::new(LogState { next_lsn, in_flight: BTreeSet::new() }),
            meta: Mutex::new(Meta { m, dirty: true }),
            write_lock: tokio::sync::Mutex::new(()),
            attach_lock: tokio::sync::Mutex::new(()),
            tables: RwLock::new(HashMap::new()),
            pending: Mutex::new(replay.pending),
            closed: AtomicBool::new(false),
            weak: weak.clone(),
            memory,
            default_memtable_space,
        });
        let sink: Weak<dyn SegmentSink> = db.weak.clone();
        shipper.register(id, sink);
        // Record ownership now, so a crash before the next lazy write is recoverable.
        db.write_manifest(first).await?;
        Ok(db)
    }

    pub fn config(&self) -> DbConfig {
        *self.cfg.read()
    }

    pub fn set_config(&self, cfg: DbConfig) {
        self.memory.set_cap(cfg.memtable_space_bytes.unwrap_or(self.default_memtable_space));
        *self.cfg.write() = cfg;
    }

    /// Its memtable memory: bytes held and the cap.
    pub fn memory(&self) -> &Arc<DbMemory> {
        &self.memory
    }

    /// The node's block cache.
    pub fn cache(&self) -> &Arc<crate::cache::BlockCache> {
        &self.env.cache
    }

    /// Its memory now (`nodetool`-like stats, the control plane's activity).
    pub fn memory_stats(&self) -> DbMemoryStats {
        let mut s = DbMemoryStats { memtable_bytes: self.memory.used(), memtable_cap_bytes: self.memory.cap(), ..Default::default() };
        for t in self.tables() {
            s.sstable_index_bytes += t.index_bytes();
            s.cache_bytes += t.state().sstables.iter().map(|x| self.env.cache.sstable_bytes(x.uid)).sum::<u64>();
        }
        s
    }

    /// The current manifest (as it would be written now).
    pub fn manifest(&self) -> Manifest {
        self.meta.lock().m.clone()
    }

    /// Attach a table (or update its options): its SSTables from the manifest, its replayed
    /// frames into the memtable.
    pub async fn table(&self, id: TableId, opts: TableOptions) -> Result<Arc<Table>> {
        if let Some(t) = self.tables.read().get(&id) {
            t.set_options(opts);
            return Ok(t.clone());
        }
        let _g = self.attach_lock.lock().await;
        if let Some(t) = self.tables.read().get(&id) {
            t.set_options(opts);
            return Ok(t.clone());
        }
        let entry = {
            let meta = self.meta.lock();
            if meta.m.dropped.contains(&id) {
                bail!("table {id} was dropped");
            }
            meta.m.tables.get(&id).cloned().unwrap_or_default()
        };
        let objects = &self.env.objects;
        // Owned items: a closure taking `&SstRef` makes this future `Send` only for one lifetime
        // (rust-lang/rust#110338), which callers can't spawn.
        let ssts: Vec<Arc<SsTable>> = futures::stream::iter(entry.sstables.iter().cloned().map(|s| async move {
            let t = SsTable::open(objects, s.key.clone(), s.level, s.created_ms).await?;
            anyhow::Ok(Arc::new(t.with_run(s.run)))
        }))
        .buffered(16)
        .try_collect()
        .await?;
        let host: Weak<dyn TableHost> = self.weak.clone();
        let table = Table::new(id, self.id.clone(), self.env.clone(), opts, ssts, Some(host), Some(self.memory.clone()));
        let pending = self.pending.lock().remove(&id).unwrap_or_default();
        for (lsn, bytes) in pending {
            let u = decode_update(&bytes)?;
            table.apply_local(u, lsn);
        }
        let new = {
            let mut meta = self.meta.lock();
            let new = !meta.m.tables.contains_key(&id);
            meta.m.tables.entry(id).or_insert(entry);
            new
        };
        self.tables.write().insert(id, table.clone());
        if new {
            // Replay must know about the table before it has frames.
            self.write_manifest(false).await?;
        }
        Ok(table)
    }

    pub fn get_table(&self, id: TableId) -> Option<Arc<Table>> {
        self.tables.read().get(&id).cloned()
    }

    pub fn tables(&self) -> Vec<Arc<Table>> {
        self.tables.read().values().cloned().collect()
    }

    /// Table ids in the manifest (attached or not).
    pub fn table_ids(&self) -> Vec<TableId> {
        self.meta.lock().m.tables.keys().copied().collect()
    }

    /// Apply writes to several tables atomically (one frame: a logged batch).
    pub async fn apply_batch(&self, updates: Vec<(TableId, PartitionUpdate)>) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("database {} is unloaded", self.id);
        }
        TableHost::admit(self).await?;
        let mut items = Vec::with_capacity(updates.len());
        for (id, mut u) in updates {
            let Some(t) = self.get_table(id) else { bail!("table {id} is not attached") };
            t.normalize(&mut u);
            items.push((t, id, encode_update(&u)));
        }
        let payload = {
            let mut p = Vec::new();
            crate::codec::put_uvarint(&mut p, items.len() as u64);
            for (_, id, b) in &items {
                p.extend_from_slice(&id.0.to_be_bytes());
                crate::codec::put_bytes(&mut p, b);
            }
            p
        };
        let (lsn, generation) = self.log_frame(&payload);
        let mut result = Ok(());
        for (t, _, b) in &items {
            match decode_update(b) {
                Ok(u) => t.apply_local(u, lsn),
                Err(e) => result = Err(e),
            }
        }
        self.end_write(lsn);
        result?;
        if self.config().durability == Durability::Sync {
            self.shipper.wait_stored(generation).await?;
        }
        Ok(())
    }

    fn log_frame(&self, payload: &[u8]) -> (u64, u64) {
        self.log_frame_parts(&[payload])
    }

    fn log_frame_parts(&self, parts: &[&[u8]]) -> (u64, u64) {
        let now = self.env.now_ms();
        let mut log = self.log.lock();
        let lsn = log.next_lsn;
        log.next_lsn += 1;
        log.in_flight.insert(lsn);
        let generation = self.shipper.append_parts(&self.stream, lsn, now, parts);
        (lsn, generation)
    }

    /// Flush every attached table's memtables, then write the manifest.
    pub async fn flush(&self) -> Result<()> {
        self.flush_tables().await?;
        self.write_manifest(false).await
    }

    /// Flush every table, as many at once as the node's flush concurrency allows.
    async fn flush_tables(&self) -> Result<()> {
        let tables = self.tables();
        futures::future::try_join_all(tables.iter().map(|t| t.flush())).await?;
        Ok(())
    }

    /// Discard a table's data (`TRUNCATE`).
    pub async fn truncate(&self, id: TableId) -> Result<()> {
        let Some(t) = self.get_table(id) else { bail!("table {id} is not attached") };
        t.truncate().await
    }

    /// Forget a table (`DROP TABLE`). Its objects stay referenced by older manifest versions
    /// until they expire.
    pub async fn drop_table(&self, id: TableId) -> Result<()> {
        let t = self.tables.write().remove(&id);
        if let Some(t) = &t {
            t.mark_dropped();
            for s in &t.state().sstables {
                self.env.cache.forget(s.uid);
            }
        }
        self.pending.lock().remove(&id);
        {
            let mut meta = self.meta.lock();
            meta.m.tables.remove(&id);
            meta.m.dropped.insert(id);
        }
        self.write_manifest(true).await
    }

    fn low_watermark_now(&self) -> u64 {
        let log = self.log.lock();
        log.in_flight.first().copied().unwrap_or(log.next_lsn)
    }

    /// Write the manifest (with `history`, also as a numbered version).
    pub async fn write_manifest(&self, history: bool) -> Result<()> {
        let _w = self.write_lock.lock().await;
        let w = self.low_watermark_now();
        let next_lsn = self.log.lock().next_lsn;
        let tables = self.tables();
        let now = self.env.now_ms();
        let retention = self.config().history_retention_ms;
        let m = {
            let mut meta = self.meta.lock();
            meta.m.next_lsn = next_lsn;
            for t in &tables {
                if t.memtables_empty()
                    && let Some(e) = meta.m.tables.get_mut(&t.id)
                {
                    e.flushed_lsn = e.flushed_lsn.max(w.saturating_sub(1));
                }
            }
            if meta.m.node.is_some() {
                meta.m.after_seq = self.shipper.completed_seq();
            }
            meta.m.prune_segments(now - retention);
            if history {
                meta.m.version += 1;
            }
            meta.m.written_ms = now;
            meta.dirty = false;
            meta.m.clone()
        };
        let r = manifest::write_manifest(&self.env.objects, &self.id, &m, history).await;
        if r.is_err() {
            // Not stored: the lazy manifest writer must retry it (a dropped or truncated table
            // would otherwise come back after a crash).
            self.meta.lock().dirty = true;
        }
        r
    }

    pub(crate) fn is_dirty(&self) -> bool {
        let meta = self.meta.lock();
        meta.dirty || self.shipper.completed_seq().saturating_sub(meta.m.after_seq) >= 600
    }

    /// Unload: flush everything, record a clean manifest, release memory.
    pub(crate) async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        // No compaction may record a manifest once another node could own the database.
        for t in self.tables() {
            t.stop_compactions().await;
        }
        let done = async {
            self.flush_tables().await?;
            self.meta.lock().m.node = None;
            self.write_manifest(false).await
        }
        .await;
        if let Err(e) = done {
            self.meta.lock().m.node = Some(self.shipper.node.clone());
            for t in self.tables() {
                t.resume_compactions();
            }
            self.closed.store(false, Ordering::SeqCst);
            return Err(e);
        }
        self.shipper.unregister(&self.id);
        for t in self.tables() {
            for s in &t.state().sstables {
                self.env.cache.forget(s.uid);
            }
        }
        Ok(())
    }

    /// Drop manifest versions older than the retention (keeping the newest one before it, so
    /// a restore to the horizon still works), and let segments only they needed go.
    pub async fn expire_history(&self) -> Result<usize> {
        let horizon = self.env.now_ms() - self.config().history_retention_ms;
        let (deleted, floor) = crate::gc::expire_versions(&self.env.objects, &self.id, horizon).await?;
        if let Some(f) = floor {
            let mut meta = self.meta.lock();
            meta.m.history_floor = meta.m.history_floor.max(f);
            meta.dirty = true;
        }
        Ok(deleted)
    }

    /// A named snapshot: a copy of the current manifest (`nodetool snapshot`).
    pub async fn snapshot(&self, name: &str) -> Result<()> {
        self.shipper.flush().await?;
        self.write_manifest(false).await?;
        let m = self.manifest();
        let raw = Bytes::from(serde_json::to_vec(&m)?);
        self.env.objects.put(&manifest::snapshot_key(&self.id, name), raw).await
    }

    pub async fn clear_snapshot(&self, name: &str) -> Result<()> {
        self.env.objects.delete(&manifest::snapshot_key(&self.id, name)).await?;
        self.env.objects.delete(&manifest::snapshot_info_key(&self.id, name)).await
    }

    /// Keeps a caller's description beside the snapshot `name` (what the REST API lists after
    /// the database was unloaded); [`Database::clear_snapshot`] deletes it with the snapshot.
    pub async fn put_snapshot_info(&self, name: &str, info: Bytes) -> Result<()> {
        self.env.objects.put(&manifest::snapshot_info_key(&self.id, name), info).await
    }

    /// Every snapshot description, by snapshot name.
    pub async fn snapshot_infos(&self) -> Result<Vec<(String, Bytes)>> {
        let prefix = manifest::snapshot_info_key(&self.id, "");
        let prefix = prefix.trim_end_matches(".json");
        let mut out = Vec::new();
        for (key, _) in self.env.objects.list(prefix).await? {
            let Some(name) = key.strip_prefix(prefix).and_then(|k| k.strip_suffix(".json")) else { continue };
            if let Some(raw) = self.env.objects.get(&key).await? {
                out.push((name.to_string(), raw));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Record bulk-loaded tables' SSTables in one manifest version ([`crate::ingest`]): per
    /// table its new SSTable list and whether its earlier contents were replaced (then its
    /// frames so far are never replayed again).
    pub(crate) async fn record_ingest(&self, changes: Vec<(TableId, Vec<manifest::SstRef>, bool)>) -> Result<()> {
        let flushed = self.low_watermark_now().saturating_sub(1);
        {
            let mut meta = self.meta.lock();
            let now = self.env.now_ms();
            for (id, sstables, replaced) in changes {
                if meta.m.dropped.contains(&id) {
                    continue;
                }
                let e: &mut TableEntry = meta.m.tables.entry(id).or_default();
                e.sstables = sstables;
                if replaced {
                    e.flushed_lsn = e.flushed_lsn.max(flushed);
                    e.base_done = true;
                    e.truncated_ms = Some(now);
                }
            }
        }
        self.write_manifest(true).await
    }

    pub fn stats(&self) -> HashMap<TableId, TableStats> {
        self.tables().into_iter().map(|t| (t.id, t.stats())).collect()
    }

    /// The next log position (tests, diagnostics).
    pub fn next_lsn(&self) -> u64 {
        self.log.lock().next_lsn
    }
}

impl SegmentSink for Database {
    fn shipped(&self, seg: SegRef) {
        let mut meta = self.meta.lock();
        meta.m.segments.push(seg);
        meta.dirty = true;
    }
}

impl TableHost for Database {
    fn begin_write(&self, table: TableId, update: &[u8]) -> Result<(u64, u64)> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("database {} is unloaded", self.id);
        }
        let mut head = Vec::with_capacity(32);
        crate::codec::put_uvarint(&mut head, 1);
        head.extend_from_slice(&table.0.to_be_bytes());
        crate::codec::put_uvarint(&mut head, update.len() as u64);
        Ok(self.log_frame_parts(&[&head, update]))
    }

    fn end_write(&self, lsn: u64) {
        self.log.lock().in_flight.remove(&lsn);
    }

    fn wait_durable(&self, generation: u64) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.shipper.wait_stored(generation))
    }

    fn sync_durability(&self) -> bool {
        self.config().durability == Durability::Sync
    }

    fn low_watermark(&self) -> u64 {
        self.low_watermark_now()
    }

    fn admit(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let timeout = self.env.write_timeout;
            self.memory.admit(timeout).await?;
            self.shipper.admit(timeout).await?;
            Ok(())
        })
    }

    fn table_changed(&self, table: TableId, change: Change) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            {
                let mut meta = self.meta.lock();
                if meta.m.dropped.contains(&table) {
                    return Ok(());
                }
                let now = self.env.now_ms();
                let e: &mut TableEntry = meta.m.tables.entry(table).or_default();
                e.sstables = change.sstables;
                if let Some(f) = change.flushed_lsn {
                    e.flushed_lsn = e.flushed_lsn.max(f);
                }
                if change.base_done {
                    e.base_done = true;
                }
                if change.truncated {
                    e.truncated_ms = Some(now);
                }
            }
            self.write_manifest(true).await
        })
    }
}

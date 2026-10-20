//! Multi-version LSM store for one database.
//!
//! Every stored key is `user_key | !commit_ts` (big-endian), so versions of a key sit together,
//! newest first, and a snapshot read is a single seek. Sources are ordered by age (memtable,
//! frozen memtables, L0 newest first, then L1), so the first source holding a visible version
//! of a key has the newest one.

pub mod cache;
pub mod keys;
pub mod row;
pub mod sst;
pub mod wal;

use crate::error::{Result, internal};
use bytes::Bytes;
use cache::BlockCache;
use crossbeam_skiplist::SkipMap;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sst::{SstMeta, SstReader, SstWriter, key_ts, user_key};
use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tracing::{debug, info, warn};
use wal::WalWriter;

pub fn mvcc_key(user: &[u8], ts: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(user.len() + 8);
    k.extend_from_slice(user);
    k.extend_from_slice(&(!ts).to_be_bytes());
    k
}

#[derive(Clone)]
pub struct StoreOptions {
    pub memtable_bytes: usize,
    /// fsync the WAL on every commit (`synchronous_commit = on`).
    pub sync: bool,
    pub compress: bool,
    pub l0_compaction_trigger: usize,
    pub target_file_bytes: u64,
    pub cache: Arc<BlockCache>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            memtable_bytes: 32 << 20,
            sync: true,
            compress: true,
            l0_compaction_trigger: 4,
            target_file_bytes: 64 << 20,
            cache: BlockCache::new(256 << 20),
        }
    }
}

pub struct Memtable {
    map: SkipMap<Vec<u8>, Option<Bytes>>,
    size: AtomicUsize,
    max_ts: AtomicU64,
    /// WAL file holding this memtable's commits.
    wal_seq: u64,
}

impl Memtable {
    fn new(wal_seq: u64) -> Self {
        Memtable {
            map: SkipMap::new(),
            size: AtomicUsize::new(0),
            max_ts: AtomicU64::new(0),
            wal_seq,
        }
    }

    fn insert(&self, key: Vec<u8>, value: Option<Bytes>, ts: u64) {
        let n = key.len() + value.as_ref().map(|v| v.len()).unwrap_or(0) + 48;
        self.map.insert(key, value);
        self.size.fetch_add(n, Ordering::Relaxed);
        self.max_ts.fetch_max(ts, Ordering::Relaxed);
    }

    pub fn bytes(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct Manifest {
    next_id: u64,
    /// Every commit <= this is in SSTs.
    flushed_ts: u64,
    last_ts: u64,
    /// Newest first.
    l0: Vec<SstMeta>,
    /// Sorted by key, non-overlapping.
    l1: Vec<SstMeta>,
}

/// An immutable picture of the store's sources. Readers hold one for their whole scan.
pub struct View {
    mem: Arc<Memtable>,
    /// Newest first.
    imm: Vec<Arc<Memtable>>,
    l0: Vec<Arc<SstReader>>,
    l1: Vec<Arc<SstReader>>,
}

struct CommitState {
    wal: WalWriter,
}

pub struct Store {
    dir: PathBuf,
    opts: StoreOptions,
    view: RwLock<Arc<View>>,
    commit: Mutex<CommitState>,
    manifest: Mutex<Manifest>,
    visible: AtomicU64,
    next_id: AtomicU64,
    snapshots: Mutex<BTreeMap<u64, usize>>,
    bg: Mutex<()>,
    bg_running: AtomicBool,
    closed: AtomicBool,
}

/// Keeps versions visible at `ts` from being compacted away.
pub struct SnapshotPin {
    store: Arc<Store>,
    pub ts: u64,
}

impl Drop for SnapshotPin {
    fn drop(&mut self) {
        let mut s = self.store.snapshots.lock();
        if let Some(n) = s.get_mut(&self.ts) {
            *n -= 1;
            if *n == 0 {
                s.remove(&self.ts);
            }
        }
    }
}

fn write_manifest(dir: &Path, m: &Manifest) -> Result<()> {
    let tmp = dir.join("MANIFEST.tmp");
    let data = serde_json::to_vec(m).map_err(|e| internal(e.to_string()))?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, &data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join("MANIFEST"))?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

impl Store {
    pub fn open(dir: &Path, opts: StoreOptions) -> Result<Arc<Store>> {
        std::fs::create_dir_all(dir)?;
        let manifest: Manifest = match std::fs::read(dir.join("MANIFEST")) {
            Ok(b) => {
                serde_json::from_slice(&b).map_err(|e| internal(format!("bad MANIFEST: {e}")))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest {
                next_id: 1,
                ..Default::default()
            },
            Err(e) => return Err(e.into()),
        };
        // Leftovers from an interrupted flush or compaction.
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "tmp") {
                let _ = std::fs::remove_file(&p);
            }
        }
        let live: std::collections::HashSet<u64> = manifest
            .l0
            .iter()
            .chain(manifest.l1.iter())
            .map(|m| m.id)
            .collect();
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "sst")
                && let Some(id) = p
                    .file_stem()
                    .and_then(|s| u64::from_str_radix(&s.to_string_lossy(), 16).ok())
                && !live.contains(&id)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
        let l0 = manifest
            .l0
            .iter()
            .map(|m| SstReader::open(dir, m.clone(), opts.cache.clone()).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        let l1 = manifest
            .l1
            .iter()
            .map(|m| SstReader::open(dir, m.clone(), opts.cache.clone()).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;

        // Replay WAL files on top of the SSTs.
        let wal_seqs = wal::list(dir)?;
        let mut next_id = manifest
            .next_id
            .max(wal_seqs.last().map(|s| s + 1).unwrap_or(1));
        let replay_seq = next_id;
        next_id += 1;
        let mem = Arc::new(Memtable::new(replay_seq));
        let mut last_ts = manifest.last_ts.max(manifest.flushed_ts);
        let mut replayed = 0usize;
        for seq in &wal_seqs {
            let max = wal::replay(&wal::wal_path(dir, *seq), |ts, entries| {
                if ts <= manifest.flushed_ts {
                    return;
                }
                for (k, v) in entries {
                    mem.insert(mvcc_key(&k, ts), v, ts);
                }
                replayed += 1;
            })?;
            last_ts = last_ts.max(max);
        }
        let wal = WalWriter::create(dir, replay_seq)?;
        let store = Arc::new(Store {
            dir: dir.to_path_buf(),
            opts,
            view: RwLock::new(Arc::new(View {
                mem,
                imm: Vec::new(),
                l0,
                l1,
            })),
            commit: Mutex::new(CommitState { wal }),
            manifest: Mutex::new(Manifest {
                next_id,
                last_ts,
                ..manifest
            }),
            visible: AtomicU64::new(last_ts),
            next_id: AtomicU64::new(next_id),
            snapshots: Mutex::new(BTreeMap::new()),
            bg: Mutex::new(()),
            bg_running: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        if replayed > 0 {
            debug!(dir = %dir.display(), records = replayed, "replayed WAL");
            // Re-log the replayed commits into the fresh WAL, then drop the old files: simplest
            // is to flush them to an SST right away.
            store.rotate()?;
            store.flush_all()?;
        }
        // Older WAL files are fully covered now.
        for seq in wal_seqs {
            if seq != replay_seq {
                let _ = std::fs::remove_file(wal::wal_path(dir, seq));
            }
        }
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The newest commit timestamp; a snapshot at this ts sees every committed write.
    pub fn visible_ts(&self) -> u64 {
        self.visible.load(Ordering::Acquire)
    }

    pub fn pin(self: &Arc<Self>, ts: u64) -> SnapshotPin {
        *self.snapshots.lock().entry(ts).or_insert(0) += 1;
        SnapshotPin {
            store: self.clone(),
            ts,
        }
    }

    /// Oldest timestamp any reader may still need.
    pub fn gc_horizon(&self) -> u64 {
        let s = self.snapshots.lock();
        s.keys()
            .next()
            .copied()
            .unwrap_or_else(|| self.visible_ts())
    }

    pub fn view(&self) -> Arc<View> {
        self.view.read().clone()
    }

    /// The value of the newest version of `key` visible at `ts` (None if absent or deleted).
    pub fn get(&self, key: &[u8], ts: u64) -> Result<Option<Bytes>> {
        Ok(self.get_version(key, ts)?.and_then(|(_, v)| v))
    }

    /// (commit ts, value) of the newest version visible at `ts`; a tombstone has value None.
    pub fn get_version(&self, key: &[u8], ts: u64) -> Result<Option<(u64, Option<Bytes>)>> {
        let view = self.view();
        let seek = mvcc_key(key, ts);
        for m in std::iter::once(&view.mem).chain(view.imm.iter()) {
            if let Some(e) = m.map.range(seek.clone()..).next()
                && user_key(e.key()) == key
            {
                return Ok(Some((key_ts(e.key()), e.value().clone())));
            }
        }
        for sst in view.l0.iter() {
            if let Some(hit) = sst_get(sst, key, &seek)? {
                return Ok(Some(hit));
            }
        }
        let i = view
            .l1
            .partition_point(|s| user_key(&s.meta.last_key) < key);
        if let Some(sst) = view.l1.get(i)
            && let Some(hit) = sst_get(sst, key, &seek)?
        {
            return Ok(Some(hit));
        }
        Ok(None)
    }

    /// Commit ts of the newest version of `key`, committed or deleted (for conflict checks).
    pub fn latest_version_ts(&self, key: &[u8]) -> Result<Option<u64>> {
        Ok(self.get_version(key, u64::MAX >> 1)?.map(|(ts, _)| ts))
    }

    /// Ascending scan of user keys in [start, end) at snapshot `ts`.
    pub fn scan(&self, start: &[u8], end: &[u8], ts: u64) -> Scan {
        Scan::new(self.view(), start.to_vec(), end.to_vec(), ts)
    }

    /// Atomically apply `writes` at a new commit timestamp. `check` runs under the commit lock
    /// (for conflict detection) and may veto the commit.
    pub fn commit(
        self: &Arc<Self>,
        writes: Vec<(Vec<u8>, Option<Bytes>)>,
        check: impl FnOnce(&Store) -> Result<()>,
    ) -> Result<u64> {
        self.commit_with(writes, check, |_| {})
    }

    /// Like `commit`, and `after(ts)` runs under the commit lock once the writes are visible
    /// (used to publish a new catalog atomically with the data).
    pub fn commit_with(
        self: &Arc<Self>,
        writes: Vec<(Vec<u8>, Option<Bytes>)>,
        check: impl FnOnce(&Store) -> Result<()>,
        after: impl FnOnce(u64),
    ) -> Result<u64> {
        if self.closed.load(Ordering::Acquire) {
            return Err(internal("database is closed"));
        }
        let rotate;
        let ts;
        {
            let mut c = self.commit.lock();
            check(self)?;
            if writes.is_empty() {
                return Ok(self.visible_ts());
            }
            ts = self.visible.load(Ordering::Acquire) + 1;
            c.wal.append(ts, &writes)?;
            if self.opts.sync {
                c.wal.sync()?;
            }
            let mem = self.view.read().mem.clone();
            for (k, v) in writes {
                mem.insert(mvcc_key(&k, ts), v, ts);
            }
            self.visible.store(ts, Ordering::Release);
            after(ts);
            rotate = mem.bytes() >= self.opts.memtable_bytes;
        }
        if rotate {
            self.rotate()?;
            self.schedule_background();
        }
        Ok(ts)
    }

    /// Freeze the memtable and start a new one (with its own WAL file).
    fn rotate(&self) -> Result<()> {
        let mut c = self.commit.lock();
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        let new_wal = WalWriter::create(&self.dir, seq)?;
        c.wal = new_wal;
        let mut v = self.view.write();
        if v.mem.is_empty() {
            // Nothing to freeze; just switch WAL files.
            let mem = Arc::new(Memtable::new(seq));
            *v = Arc::new(View {
                mem,
                imm: v.imm.clone(),
                l0: v.l0.clone(),
                l1: v.l1.clone(),
            });
            return Ok(());
        }
        let mut imm = vec![v.mem.clone()];
        imm.extend(v.imm.iter().cloned());
        *v = Arc::new(View {
            mem: Arc::new(Memtable::new(seq)),
            imm,
            l0: v.l0.clone(),
            l1: v.l1.clone(),
        });
        Ok(())
    }

    fn schedule_background(self: &Arc<Self>) {
        if self.bg_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        std::thread::Builder::new()
            .name("roll-flush".into())
            .spawn(move || {
                if let Err(e) = this.flush_all().and_then(|_| this.maybe_compact()) {
                    warn!(dir = %this.dir.display(), error = %e, "background flush/compaction failed");
                }
                this.bg_running.store(false, Ordering::Release);
            })
            .ok();
    }

    /// Write every frozen memtable to L0, oldest first.
    pub fn flush_all(&self) -> Result<()> {
        let _g = self.bg.lock();
        loop {
            let oldest = { self.view.read().imm.last().cloned() };
            let Some(mem) = oldest else { return Ok(()) };
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let mut w = SstWriter::create(&self.dir, id, self.opts.compress)?;
            for e in mem.map.iter() {
                w.add(e.key(), e.value().as_deref())?;
            }
            let meta = w.finish()?;
            let reader = Arc::new(SstReader::open(
                &self.dir,
                meta.clone(),
                self.opts.cache.clone(),
            )?);
            let flushed_ts = mem.max_ts.load(Ordering::Relaxed);
            {
                let mut m = self.manifest.lock();
                m.l0.insert(0, meta);
                m.flushed_ts = m.flushed_ts.max(flushed_ts);
                m.last_ts = self.visible_ts();
                m.next_id = self.next_id.load(Ordering::Relaxed);
                write_manifest(&self.dir, &m)?;
                let mut v = self.view.write();
                let imm: Vec<_> = v
                    .imm
                    .iter()
                    .filter(|x| !Arc::ptr_eq(x, &mem))
                    .cloned()
                    .collect();
                let mut l0 = vec![reader];
                l0.extend(v.l0.iter().cloned());
                *v = Arc::new(View {
                    mem: v.mem.clone(),
                    imm,
                    l0,
                    l1: v.l1.clone(),
                });
            }
            let _ = std::fs::remove_file(wal::wal_path(&self.dir, mem.wal_seq));
            debug!(dir = %self.dir.display(), id, "flushed memtable");
        }
    }

    /// Merge L0 into L1 once L0 has enough files, dropping versions no snapshot can see.
    pub fn maybe_compact(&self) -> Result<()> {
        if self.view.read().l0.len() < self.opts.l0_compaction_trigger {
            return Ok(());
        }
        self.compact()
    }

    pub fn compact(&self) -> Result<()> {
        let _g = self.bg.lock();
        let view = self.view();
        if view.l0.is_empty() {
            return Ok(());
        }
        let inputs_l0 = view.l0.clone();
        let inputs_l1 = view.l1.clone();
        let horizon = self.gc_horizon();
        let mut sources: Vec<Cursor> = inputs_l0
            .iter()
            .map(|s| Cursor::level(vec![s.clone()], &[], &[]))
            .collect();
        sources.push(Cursor::level(inputs_l1.clone(), &[], &[]));
        let mut merge = Merge::new(sources, Vec::new());
        let mut outputs: Vec<SstMeta> = Vec::new();
        let mut writer: Option<SstWriter> = None;
        let mut cur_user: Option<Vec<u8>> = None;
        let mut kept_old = false;
        while let Some((k, v)) = merge.next_raw()? {
            let uk = user_key(&k);
            let ts = key_ts(&k);
            if cur_user.as_deref() != Some(uk) {
                cur_user = Some(uk.to_vec());
                kept_old = false;
            }
            if ts <= horizon {
                if kept_old {
                    continue;
                }
                kept_old = true;
                // L1 is the bottom level, so a tombstone nobody can see past can go.
                if v.is_none() {
                    continue;
                }
            }
            if writer
                .as_ref()
                .is_some_and(|w| w.approx_size() >= self.opts.target_file_bytes)
            {
                // Only split between user keys.
                let w = writer.take().unwrap();
                outputs.push(w.finish()?);
            }
            if writer.is_none() {
                writer = Some(SstWriter::create(
                    &self.dir,
                    self.next_id.fetch_add(1, Ordering::Relaxed),
                    self.opts.compress,
                )?);
            }
            writer.as_mut().unwrap().add(&k, v.as_deref())?;
        }
        if let Some(w) = writer {
            if w.entries() > 0 {
                outputs.push(w.finish()?);
            } else {
                w.abandon();
            }
        }
        let readers = outputs
            .iter()
            .map(|m| SstReader::open(&self.dir, m.clone(), self.opts.cache.clone()).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        {
            let mut m = self.manifest.lock();
            let input_ids: std::collections::HashSet<u64> = inputs_l0
                .iter()
                .chain(inputs_l1.iter())
                .map(|s| s.meta.id)
                .collect();
            m.l0.retain(|s| !input_ids.contains(&s.id));
            m.l1 = outputs.clone();
            m.next_id = self.next_id.load(Ordering::Relaxed);
            write_manifest(&self.dir, &m)?;
            let mut v = self.view.write();
            let l0: Vec<_> =
                v.l0.iter()
                    .filter(|s| !input_ids.contains(&s.meta.id))
                    .cloned()
                    .collect();
            *v = Arc::new(View {
                mem: v.mem.clone(),
                imm: v.imm.clone(),
                l0,
                l1: readers,
            });
        }
        for s in inputs_l0.iter().chain(inputs_l1.iter()) {
            s.mark_obsolete();
        }
        info!(dir = %self.dir.display(), files = outputs.len(), "compacted");
        Ok(())
    }

    /// Flush everything to SSTs and stop accepting commits. After this the store holds no
    /// memory beyond what the caller keeps alive.
    pub fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.rotate()?;
        self.flush_all()?;
        let mut m = self.manifest.lock();
        m.last_ts = self.visible_ts();
        write_manifest(&self.dir, &m)?;
        Ok(())
    }

    /// Total bytes in SSTs and memtables (for metering).
    pub fn size_bytes(&self) -> u64 {
        let v = self.view();
        let sst: u64 = v.l0.iter().chain(v.l1.iter()).map(|s| s.meta.size).sum();
        let mem: usize = std::iter::once(&v.mem)
            .chain(v.imm.iter())
            .map(|m| m.bytes())
            .sum();
        sst + mem as u64
    }

    pub fn memory_bytes(&self) -> usize {
        let v = self.view();
        std::iter::once(&v.mem)
            .chain(v.imm.iter())
            .map(|m| m.bytes())
            .sum()
    }
}

fn sst_get(sst: &SstReader, key: &[u8], seek: &[u8]) -> Result<Option<(u64, Option<Bytes>)>> {
    if key < user_key(&sst.meta.first_key)
        || key > user_key(&sst.meta.last_key)
        || !sst.may_contain(key)
    {
        return Ok(None);
    }
    let mut b = sst.find_block(seek);
    while b < sst.block_count() {
        let block = sst.block(b)?;
        let i = block.partition_point(|(k, _)| k.as_slice() < seek);
        if let Some((k, v)) = block.get(i) {
            if user_key(k) == key {
                return Ok(Some((key_ts(k), v.clone())));
            }
            return Ok(None);
        }
        b += 1;
    }
    Ok(None)
}

// ── merging iteration ──

type RawEntry = (Vec<u8>, Option<Bytes>);

const BATCH: usize = 256;

/// One ordered source of raw (storage key, value) entries.
enum Cursor {
    Mem {
        mem: Arc<Memtable>,
        buf: VecDeque<RawEntry>,
        from: Bound<Vec<u8>>,
        end: Vec<u8>,
        done: bool,
    },
    Level {
        ssts: Vec<Arc<SstReader>>,
        file: usize,
        block: Option<Arc<sst::Block>>,
        block_idx: usize,
        pos: usize,
        end: Vec<u8>,
        start: Vec<u8>,
        started: bool,
    },
}

impl Cursor {
    fn mem(mem: Arc<Memtable>, start: &[u8], end: &[u8]) -> Cursor {
        Cursor::Mem {
            mem,
            buf: VecDeque::new(),
            from: Bound::Included(start.to_vec()),
            end: end.to_vec(),
            done: false,
        }
    }

    /// Non-overlapping SSTs in key order. Empty start/end = unbounded.
    fn level(ssts: Vec<Arc<SstReader>>, start: &[u8], end: &[u8]) -> Cursor {
        Cursor::Level {
            ssts,
            file: 0,
            block: None,
            block_idx: 0,
            pos: 0,
            end: end.to_vec(),
            start: start.to_vec(),
            started: false,
        }
    }

    fn peek(&mut self) -> Result<Option<&RawEntry>> {
        match self {
            Cursor::Mem {
                mem,
                buf,
                from,
                end,
                done,
            } => {
                if buf.is_empty() && !*done {
                    let upper = if end.is_empty() {
                        Bound::Unbounded
                    } else {
                        Bound::Excluded(end.clone())
                    };
                    let lower = match from {
                        Bound::Included(k) => Bound::Included(k.clone()),
                        Bound::Excluded(k) => Bound::Excluded(k.clone()),
                        Bound::Unbounded => Bound::Unbounded,
                    };
                    for e in mem.map.range::<Vec<u8>, _>((lower, upper)).take(BATCH) {
                        buf.push_back((e.key().clone(), e.value().clone()));
                    }
                    if buf.len() < BATCH {
                        *done = true;
                    }
                    if let Some((k, _)) = buf.back() {
                        *from = Bound::Excluded(k.clone());
                    }
                }
                Ok(buf.front())
            }
            Cursor::Level {
                ssts,
                file,
                block,
                block_idx,
                pos,
                end,
                start,
                started,
            } => {
                loop {
                    if *file >= ssts.len() {
                        return Ok(None);
                    }
                    if !*started {
                        *started = true;
                        // Position at the first entry >= start.
                        if !start.is_empty() {
                            *file = ssts
                                .partition_point(|s| s.meta.last_key.as_slice() < start.as_slice());
                            if *file >= ssts.len() {
                                return Ok(None);
                            }
                            let sst = &ssts[*file];
                            *block_idx = sst.find_block(start);
                            if *block_idx < sst.block_count() {
                                let b = sst.block(*block_idx)?;
                                *pos = b.partition_point(|(k, _)| k.as_slice() < start.as_slice());
                                *block = Some(b);
                            }
                        }
                    }
                    let sst = &ssts[*file];
                    if block.is_none() {
                        if *block_idx >= sst.block_count() {
                            *file += 1;
                            *block_idx = 0;
                            *pos = 0;
                            continue;
                        }
                        *block = Some(sst.block(*block_idx)?);
                        *pos = 0;
                    }
                    let b = block.as_ref().unwrap();
                    if *pos >= b.len() {
                        *block = None;
                        *block_idx += 1;
                        continue;
                    }
                    let entry = &b[*pos];
                    if !end.is_empty() && entry.0.as_slice() >= end.as_slice() {
                        *file = ssts.len();
                        return Ok(None);
                    }
                    // Safe: the block Arc lives in self.
                    let b: &sst::Block = block.as_ref().unwrap();
                    return Ok(Some(&b[*pos]));
                }
            }
        }
    }

    /// The entry the last `peek` returned (None where it returned None), without
    /// reading further.
    fn head(&self) -> Option<&RawEntry> {
        match self {
            Cursor::Mem { buf, .. } => buf.front(),
            Cursor::Level {
                ssts,
                file,
                block,
                pos,
                end,
                ..
            } => {
                if *file >= ssts.len() {
                    return None;
                }
                let e = block.as_ref()?.get(*pos)?;
                (end.is_empty() || e.0.as_slice() < end.as_slice()).then_some(e)
            }
        }
    }

    fn advance(&mut self) {
        match self {
            Cursor::Mem { buf, .. } => {
                buf.pop_front();
            }
            Cursor::Level { pos, .. } => *pos += 1,
        }
    }
}

struct Merge {
    sources: Vec<Cursor>,
    end: Vec<u8>,
}

impl Merge {
    fn new(sources: Vec<Cursor>, end: Vec<u8>) -> Merge {
        Merge { sources, end }
    }

    /// Smallest raw entry across sources. Identical storage keys can't occur in two sources
    /// (a commit ts belongs to one memtable), so ties don't need breaking.
    fn next_raw(&mut self) -> Result<Option<RawEntry>> {
        // Each source's next entry, then the smallest (the first of equals), compared in
        // place.
        for c in &mut self.sources {
            c.peek()?;
        }
        let mut best: Option<(usize, &RawEntry)> = None;
        for (i, c) in self.sources.iter().enumerate() {
            let Some(e) = c.head() else {
                continue;
            };
            if best.is_none_or(|(_, b)| e.0 < b.0) {
                best = Some((i, e));
            }
        }
        let Some((b, e)) = best else { return Ok(None) };
        let e = e.clone();
        self.sources[b].advance();
        if !self.end.is_empty() && e.0 >= self.end {
            return Ok(None);
        }
        Ok(Some(e))
    }
}

/// Snapshot scan: yields (user key, value) for the newest visible, non-deleted version.
pub struct Scan {
    merge: Merge,
    ts: u64,
    decided: Option<Vec<u8>>,
    _view: Arc<View>,
}

impl Scan {
    fn new(view: Arc<View>, start: Vec<u8>, end: Vec<u8>, ts: u64) -> Scan {
        let mut sources = Vec::new();
        sources.push(Cursor::mem(view.mem.clone(), &start, &end));
        for m in &view.imm {
            sources.push(Cursor::mem(m.clone(), &start, &end));
        }
        for s in &view.l0 {
            sources.push(Cursor::level(vec![s.clone()], &start, &end));
        }
        if !view.l1.is_empty() {
            sources.push(Cursor::level(view.l1.clone(), &start, &end));
        }
        Scan {
            merge: Merge::new(sources, end),
            ts,
            decided: None,
            _view: view,
        }
    }
}

impl Iterator for Scan {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (k, v) = match self.merge.next_raw() {
                Ok(Some(e)) => e,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            let uk = user_key(&k);
            if self.decided.as_deref() == Some(uk) {
                continue;
            }
            if key_ts(&k) > self.ts {
                continue;
            }
            self.decided = Some(uk.to_vec());
            if let Some(v) = v {
                return Some(Ok((uk.to_vec(), v)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> StoreOptions {
        StoreOptions {
            memtable_bytes: 4096,
            sync: false,
            l0_compaction_trigger: 3,
            ..Default::default()
        }
    }

    fn put(s: &Arc<Store>, k: &str, v: Option<&str>) -> u64 {
        s.commit(
            vec![(k.as_bytes().to_vec(), v.map(|v| Bytes::from(v.to_string())))],
            |_| Ok(()),
        )
        .unwrap()
    }

    fn scan_all(s: &Store, ts: u64) -> Vec<(String, String)> {
        s.scan(b"", &[0xff], ts)
            .map(|r| r.unwrap())
            .map(|(k, v)| {
                (
                    String::from_utf8(k).unwrap(),
                    String::from_utf8(v.to_vec()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn mvcc_reads() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), opts()).unwrap();
        let t1 = put(&s, "a", Some("1"));
        let t2 = put(&s, "a", Some("2"));
        put(&s, "b", Some("x"));
        let t4 = put(&s, "a", None);
        assert_eq!(s.get(b"a", t1).unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(s.get(b"a", t2).unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(s.get(b"a", t4).unwrap(), None);
        assert_eq!(scan_all(&s, t2), vec![("a".into(), "2".into())]);
        assert_eq!(scan_all(&s, t4), vec![("b".into(), "x".into())]);
    }

    #[test]
    fn flush_compact_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut expect = std::collections::BTreeMap::new();
        {
            let s = Store::open(dir.path(), opts()).unwrap();
            for i in 0..2000 {
                let k = format!("k{:05}", i % 700);
                let v = format!("v{i}");
                put(&s, &k, Some(&v));
                expect.insert(k, v);
                if i % 3 == 0 {
                    let d = format!("k{:05}", (i * 7) % 700);
                    put(&s, &d, None);
                    expect.remove(&d);
                }
            }
            s.flush_all().unwrap();
            s.compact().unwrap();
            let got: std::collections::BTreeMap<_, _> =
                scan_all(&s, s.visible_ts()).into_iter().collect();
            assert_eq!(got, expect);
            // Leave some data only in the WAL.
            put(&s, "zzz", Some("wal-only"));
            expect.insert("zzz".into(), "wal-only".into());
        }
        let s = Store::open(dir.path(), opts()).unwrap();
        let got: std::collections::BTreeMap<_, _> =
            scan_all(&s, s.visible_ts()).into_iter().collect();
        assert_eq!(got, expect);
        assert_eq!(
            s.get(b"zzz", s.visible_ts()).unwrap().as_deref(),
            Some(&b"wal-only"[..])
        );
        s.close().unwrap();
        drop(s);
        let s = Store::open(dir.path(), opts()).unwrap();
        let got: std::collections::BTreeMap<_, _> =
            scan_all(&s, s.visible_ts()).into_iter().collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn snapshots_survive_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), opts()).unwrap();
        let t1 = put(&s, "a", Some("old"));
        let pin = s.pin(t1);
        for i in 0..500 {
            put(&s, &format!("f{i:04}"), Some("filler-filler-filler"));
        }
        put(&s, "a", Some("new"));
        s.rotate().unwrap();
        s.flush_all().unwrap();
        s.compact().unwrap();
        assert_eq!(s.get(b"a", t1).unwrap().as_deref(), Some(&b"old"[..]));
        drop(pin);
        put(&s, "b", Some("1"));
        s.rotate().unwrap();
        s.flush_all().unwrap();
        s.compact().unwrap();
        assert_eq!(
            s.get(b"a", s.visible_ts()).unwrap().as_deref(),
            Some(&b"new"[..])
        );
    }
}

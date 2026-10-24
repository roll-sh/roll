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
    /// The last commit timestamp handed out (under the commit lock). Commits past `visible`
    /// are in the WAL and the memtable, waiting for a WAL fsync (group commit).
    assigned: AtomicU64,
    /// Every commit up to this one is fsynced.
    durable: AtomicU64,
    /// Held by the commit fsyncing the WAL for every commit appended so far.
    sync_lock: Mutex<()>,
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
        let wal = WalWriter::create(dir, replay_seq, if opts.sync { wal::PREALLOC } else { 0 })?;
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
            assigned: AtomicU64::new(last_ts),
            durable: AtomicU64::new(last_ts),
            sync_lock: Mutex::new(()),
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

    /// A snapshot of every committed write, pinned: the timestamp is read under the lock
    /// `gc_horizon` reads it under, so no pruning or compaction can take what it sees
    /// between the two.
    pub fn pin_visible(self: &Arc<Self>) -> SnapshotPin {
        let mut s = self.snapshots.lock();
        let ts = self.visible_ts();
        *s.entry(ts).or_insert(0) += 1;
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
            if let Some(e) = m
                .map
                .range::<[u8], _>((Bound::Included(seek.as_slice()), Bound::Unbounded))
                .next()
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
        Scan::new(self.view(), start.to_vec(), end.to_vec(), ts, self.gc_horizon())
    }

    /// Atomically apply `writes` at a new commit timestamp. `check` runs under the commit lock
    /// (for conflict detection) and may veto the commit.
    ///
    /// Group commit: the commit's WAL record and memtable entries go in under the commit
    /// lock, then it waits (without the lock) for an fsync covering it, which one waiting
    /// commit does for all those appended so far. It becomes visible with that fsync (in
    /// timestamp order: every earlier commit is in the memtable by then), before this
    /// returns; snapshots taken meanwhile don't see it, as a transaction whose commit record
    /// isn't flushed yet isn't seen in Postgres.
    pub fn commit(
        self: &Arc<Self>,
        writes: Vec<(Vec<u8>, Option<Bytes>)>,
        check: impl FnOnce(&Store) -> Result<()>,
    ) -> Result<u64> {
        if !self.opts.sync {
            return self.commit_with(writes, |s, _| check(s), |_| {});
        }
        let rotate;
        let ts;
        {
            let mut c = self.commit.lock();
            if self.closed.load(Ordering::Acquire) {
                return Err(internal("database is closed"));
            }
            check(self)?;
            if writes.is_empty() {
                return Ok(self.visible_ts());
            }
            ts = self.assigned.load(Ordering::Acquire) + 1;
            c.wal.append(ts, &writes)?;
            self.assigned.store(ts, Ordering::Release);
            let mem = self.view.read().mem.clone();
            for (k, v) in writes {
                mem.insert(mvcc_key(&k, ts), v, ts);
            }
            rotate = mem.bytes() >= self.opts.memtable_bytes;
        }
        self.wait_durable(ts)?;
        if rotate {
            self.rotate()?;
            self.schedule_background();
        }
        Ok(ts)
    }

    /// Wait until commit `ts` is fsynced (and so visible), fsyncing the WAL for every commit
    /// appended so far when no other commit is doing it.
    fn wait_durable(&self, ts: u64) -> Result<()> {
        while self.durable.load(Ordering::Acquire) < ts {
            let _g = self.sync_lock.lock();
            if self.durable.load(Ordering::Acquire) >= ts {
                break;
            }
            let (file, upto) = {
                let c = self.commit.lock();
                (c.wal.handle(), self.assigned.load(Ordering::Acquire))
            };
            if let Err(e) = file.sync_data() {
                // The commits are in the memtable already and can't be taken back: the
                // store stops (Postgres PANICs on a failed WAL fsync).
                self.closed.store(true, Ordering::Release);
                return Err(e.into());
            }
            self.made_durable(upto);
        }
        Ok(())
    }

    /// Every commit up to `upto` is fsynced: it is visible.
    fn made_durable(&self, upto: u64) {
        self.durable.fetch_max(upto, Ordering::AcqRel);
        self.visible.fetch_max(upto, Ordering::AcqRel);
    }

    /// Like `commit`, and `after(ts)` runs under the commit lock once the writes are visible
    /// (used to publish a new catalog atomically with the data).
    /// (`check` may also change the writes: a catalog carried onto the current one.)
    pub fn commit_with(
        self: &Arc<Self>,
        mut writes: Vec<(Vec<u8>, Option<Bytes>)>,
        check: impl FnOnce(&Store, &mut Vec<(Vec<u8>, Option<Bytes>)>) -> Result<()>,
        after: impl FnOnce(u64),
    ) -> Result<u64> {
        let rotate;
        let ts;
        {
            let mut c = self.commit.lock();
            // (Checked under the commit lock: `close` rotates under it, so no commit lands
            // in a WAL or memtable after the store's last flush.)
            if self.closed.load(Ordering::Acquire) {
                return Err(internal("database is closed"));
            }
            check(self, &mut writes)?;
            if writes.is_empty() {
                return Ok(self.visible_ts());
            }
            ts = self.assigned.load(Ordering::Acquire) + 1;
            c.wal.append(ts, &writes)?;
            if self.opts.sync {
                // (Also makes every group commit appended before this one durable.)
                c.wal.sync()?;
            }
            self.assigned.store(ts, Ordering::Release);
            let mem = self.view.read().mem.clone();
            for (k, v) in writes {
                mem.insert(mvcc_key(&k, ts), v, ts);
            }
            self.made_durable(ts);
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
        self.rotate_inner(false)
    }

    /// `rotate`; but once the store is closed only `close`'s own rotation (`closing`) runs:
    /// the database's files may belong to a store opened after it.
    fn rotate_inner(&self, closing: bool) -> Result<()> {
        let mut c = self.commit.lock();
        if self.closed.load(Ordering::Acquire) && !closing {
            return Ok(());
        }
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        let new_wal = WalWriter::create(
            &self.dir,
            seq,
            if self.opts.sync { wal::PREALLOC } else { 0 },
        )?;
        // The memtable frozen here holds only durable commits (group commits waiting for an
        // fsync of the old file get it now).
        if self.opts.sync {
            c.wal.sync()?;
        }
        self.made_durable(self.assigned.load(Ordering::Acquire));
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
        // A closed store's files may belong to a store opened after it (`close` waits for a
        // compaction that already runs: it takes `bg` too).
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
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
        self.rotate_inner(true)?;
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
const MIN_BATCH: usize = 16;

/// One ordered source of raw (storage key, value) entries.
enum Cursor {
    Mem {
        mem: Arc<Memtable>,
        buf: VecDeque<RawEntry>,
        from: Bound<Vec<u8>>,
        end: Vec<u8>,
        done: bool,
        /// Entries read per batch: a few at first (and after a seek), more as the scan goes
        /// on (a row updated many times has as many versions here, mostly skipped).
        batch: usize,
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
            batch: MIN_BATCH,
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
                batch,
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
                    for e in mem.map.range::<Vec<u8>, _>((lower, upper)).take(*batch) {
                        buf.push_back((e.key().clone(), e.value().clone()));
                    }
                    if buf.len() < *batch {
                        *done = true;
                    }
                    *batch = (*batch * 2).min(BATCH);
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

    /// Move to the first entry >= `target` (when not there already).
    fn seek(&mut self, target: &[u8]) {
        match self {
            Cursor::Mem {
                buf,
                from,
                done,
                batch,
                ..
            } => {
                while buf.front().is_some_and(|e| e.0.as_slice() < target) {
                    buf.pop_front();
                }
                if buf.is_empty() && !*done {
                    *from = Bound::Included(target.to_vec());
                    *batch = MIN_BATCH;
                }
            }
            Cursor::Level {
                ssts,
                file,
                block,
                pos,
                start,
                started,
                ..
            } => {
                if !*started {
                    if target > start.as_slice() {
                        *start = target.to_vec();
                    }
                    return;
                }
                if *file >= ssts.len() {
                    return;
                }
                if let Some(b) = block.as_ref() {
                    if b.get(*pos).is_some_and(|e| e.0.as_slice() >= target) {
                        return;
                    }
                    if b.last().is_some_and(|e| e.0.as_slice() >= target) {
                        *pos = b.partition_point(|(k, _)| k.as_slice() < target);
                        return;
                    }
                }
                // Further on: positioned again as at the start (`peek`).
                *start = target.to_vec();
                *started = false;
                *block = None;
            }
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

    /// Every source on to its first entry >= `target`.
    fn seek(&mut self, target: &[u8]) {
        for c in &mut self.sources {
            c.seek(target);
        }
    }
}

/// Snapshot scan: yields (user key, value) for the newest visible, non-deleted version.
pub struct Scan {
    merge: Merge,
    ts: u64,
    decided: Option<Vec<u8>>,
    /// The commit ts of the version `decided` holds.
    decided_ts: u64,
    /// Versions of the decided key passed over so far.
    skipped: u32,
    /// The last key a version newer than the snapshot was passed over for.
    newer: Option<Vec<u8>>,
    /// No snapshot older than this can read (the store's gc horizon as the scan began).
    horizon: u64,
    _view: Arc<View>,
}

impl Scan {
    fn new(view: Arc<View>, start: Vec<u8>, end: Vec<u8>, ts: u64, horizon: u64) -> Scan {
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
            decided_ts: 0,
            skipped: 0,
            newer: None,
            horizon,
            _view: view,
        }
    }

    /// Drop the memtables' versions of `uk` older than the decided one, when that one is
    /// at or before the horizon: no snapshot can read them (a row updated many times keeps
    /// only what readers need, as heap pruning does in Postgres).
    fn prune(&self, uk: &[u8]) {
        if self.decided_ts > self.horizon {
            return;
        }
        let from = mvcc_key(uk, self.decided_ts);
        let mut to = uk.to_vec();
        to.extend_from_slice(&[0xFF; 9]);
        for m in std::iter::once(&self._view.mem).chain(self._view.imm.iter()) {
            let range = (Bound::Excluded(from.as_slice()), Bound::Excluded(to.as_slice()));
            for e in m.map.range::<[u8], _>(range) {
                e.remove();
            }
        }
    }
}

impl Iterator for Scan {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (mut k, v) = match self.merge.next_raw() {
                Ok(Some(e)) => e,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            let uk = user_key(&k);
            if self.decided.as_deref() == Some(uk) {
                // Another version of a key decided already. Past a second one, a seek past
                // all of them (with the dead ones pruned): a row updated many times. (User
                // keys are prefix-free, so no other key's entries lie before the target.)
                self.skipped += 1;
                if self.skipped >= 2 {
                    self.prune(uk);
                    let mut target = uk.to_vec();
                    target.extend_from_slice(&[0xFF; 9]);
                    self.merge.seek(&target);
                }
                continue;
            }
            if key_ts(&k) > self.ts {
                // Versions newer than the snapshot: past a second one, a seek to the newest
                // one it sees.
                if self.newer.as_deref() == Some(uk) {
                    let target = mvcc_key(uk, self.ts);
                    self.merge.seek(&target);
                } else {
                    self.newer = Some(uk.to_vec());
                }
                continue;
            }
            self.skipped = 0;
            // (The decided key's buffer is reused, and the entry's own key cut to the user
            // key: one allocation per row.)
            match &mut self.decided {
                Some(d) => {
                    d.clear();
                    d.extend_from_slice(uk);
                }
                None => self.decided = Some(uk.to_vec()),
            }
            self.decided_ts = key_ts(&k);
            if let Some(v) = v {
                k.truncate(k.len().saturating_sub(8));
                return Some(Ok((k, v)));
            }
        }
    }
}

// ── descending iteration ──

/// One source's raw entries below a bound, greatest first (a batch at a time).
enum RevCursor {
    Mem {
        mem: Arc<Memtable>,
        buf: VecDeque<RawEntry>,
        start: Vec<u8>,
        /// Exclusive upper bound of what's left (None: unbounded).
        to: Option<Vec<u8>>,
        done: bool,
    },
    Level {
        ssts: Vec<Arc<SstReader>>,
        start: Vec<u8>,
        end: Vec<u8>,
        /// The next file to read is `file - 1` (0: none left); within the current block,
        /// entries before `pos`.
        file: usize,
        block: Option<Arc<sst::Block>>,
        block_idx: usize,
        pos: usize,
        started: bool,
    },
}

impl RevCursor {
    fn peek(&mut self) -> Result<Option<&RawEntry>> {
        match self {
            RevCursor::Mem {
                mem,
                buf,
                start,
                to,
                done,
            } => {
                if buf.is_empty() && !*done {
                    let upper = match to {
                        Some(k) => Bound::Excluded(k.as_slice()),
                        None => Bound::Unbounded,
                    };
                    let lower = Bound::Included(start.as_slice());
                    for e in mem.map.range::<[u8], _>((lower, upper)).rev().take(BATCH) {
                        buf.push_back((e.key().clone(), e.value().clone()));
                    }
                    if buf.len() < BATCH {
                        *done = true;
                    }
                    if let Some((k, _)) = buf.back() {
                        *to = Some(k.clone());
                    }
                }
                Ok(buf.front())
            }
            RevCursor::Level {
                ssts,
                start,
                end,
                file,
                block,
                block_idx,
                pos,
                started,
            } => loop {
                if !*started {
                    *started = true;
                    // The last file holding a key below `end`, at its first block reaching
                    // `end` (or its last).
                    *file = if end.is_empty() {
                        ssts.len()
                    } else {
                        ssts.partition_point(|s| s.meta.first_key.as_slice() < end.as_slice())
                    };
                    if *file == 0 {
                        return Ok(None);
                    }
                    let sst = &ssts[*file - 1];
                    let n = sst.block_count();
                    *block_idx = if end.is_empty() {
                        n.saturating_sub(1)
                    } else {
                        sst.find_block(end).min(n.saturating_sub(1))
                    };
                    if n == 0 {
                        *file -= 1;
                        *started = false;
                        continue;
                    }
                    let b = sst.block(*block_idx)?;
                    *pos = if end.is_empty() {
                        b.len()
                    } else {
                        b.partition_point(|(k, _)| k.as_slice() < end.as_slice())
                    };
                    *block = Some(b);
                }
                if *file == 0 {
                    return Ok(None);
                }
                if block.is_none() {
                    if *block_idx == 0 {
                        // On to the previous file's last block.
                        *file -= 1;
                        if *file == 0 {
                            return Ok(None);
                        }
                        let sst = &ssts[*file - 1];
                        if sst.block_count() == 0 {
                            continue;
                        }
                        *block_idx = sst.block_count() - 1;
                    } else {
                        *block_idx -= 1;
                    }
                    let b = ssts[*file - 1].block(*block_idx)?;
                    *pos = b.len();
                    *block = Some(b);
                }
                if *pos == 0 {
                    *block = None;
                    continue;
                }
                let b: &sst::Block = block.as_ref().unwrap();
                let entry = &b[*pos - 1];
                if entry.0.as_slice() < start.as_slice() {
                    *file = 0;
                    return Ok(None);
                }
                return Ok(Some(entry));
            },
        }
    }

    fn head(&self) -> Option<&RawEntry> {
        match self {
            RevCursor::Mem { buf, .. } => buf.front(),
            RevCursor::Level {
                file,
                block,
                pos,
                start,
                ..
            } => {
                if *file == 0 || *pos == 0 {
                    return None;
                }
                let e = block.as_ref()?.get(*pos - 1)?;
                (e.0.as_slice() >= start.as_slice()).then_some(e)
            }
        }
    }

    fn advance(&mut self) {
        match self {
            RevCursor::Mem { buf, .. } => {
                buf.pop_front();
            }
            RevCursor::Level { pos, .. } => *pos -= 1,
        }
    }
}

/// Snapshot scan in descending key order: (user key, value) of the newest visible,
/// non-deleted version of each key in [start, end).
pub struct RevScan {
    sources: Vec<RevCursor>,
    ts: u64,
    /// The next user key's entries already read: its first (oldest) one is held here.
    pending: Option<RawEntry>,
    _view: Arc<View>,
}

impl RevScan {
    fn new(view: Arc<View>, start: Vec<u8>, end: Vec<u8>, ts: u64) -> RevScan {
        let mem = |m: &Arc<Memtable>| RevCursor::Mem {
            mem: m.clone(),
            buf: VecDeque::new(),
            start: start.clone(),
            to: (!end.is_empty()).then(|| end.clone()),
            done: false,
        };
        let level = |ssts: Vec<Arc<SstReader>>| RevCursor::Level {
            ssts,
            start: start.clone(),
            end: end.clone(),
            file: 0,
            block: None,
            block_idx: 0,
            pos: 0,
            started: false,
        };
        let mut sources = vec![mem(&view.mem)];
        sources.extend(view.imm.iter().map(mem));
        sources.extend(view.l0.iter().map(|s| level(vec![s.clone()])));
        if !view.l1.is_empty() {
            sources.push(level(view.l1.clone()));
        }
        RevScan {
            sources,
            ts,
            pending: None,
            _view: view,
        }
    }

    /// The greatest raw entry left across the sources.
    fn next_raw(&mut self) -> Result<Option<RawEntry>> {
        for c in &mut self.sources {
            c.peek()?;
        }
        let mut best: Option<(usize, &RawEntry)> = None;
        for (i, c) in self.sources.iter().enumerate() {
            let Some(e) = c.head() else { continue };
            if best.is_none_or(|(_, b)| e.0 > b.0) {
                best = Some((i, e));
            }
        }
        let Some((b, e)) = best else { return Ok(None) };
        let e = e.clone();
        self.sources[b].advance();
        Ok(Some(e))
    }
}

impl Iterator for RevScan {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let first = match self.pending.take() {
                Some(e) => e,
                None => match self.next_raw() {
                    Ok(Some(e)) => e,
                    Ok(None) => return None,
                    Err(e) => return Some(Err(e)),
                },
            };
            // A key's versions come oldest first: the newest one at or before `ts` wins.
            let uk = user_key(&first.0).to_vec();
            let mut chosen = (key_ts(&first.0) <= self.ts).then_some(first.1);
            loop {
                match self.next_raw() {
                    Ok(Some(e)) if user_key(&e.0) == uk.as_slice() => {
                        if key_ts(&e.0) <= self.ts {
                            chosen = Some(e.1);
                        }
                    }
                    Ok(Some(e)) => {
                        self.pending = Some(e);
                        break;
                    }
                    Ok(None) => break,
                    Err(e) => return Some(Err(e)),
                }
            }
            if let Some(Some(v)) = chosen {
                return Some(Ok((uk, v)));
            }
        }
    }
}

impl Store {
    /// Descending scan of user keys in [start, end) at snapshot `ts`.
    pub fn scan_rev(&self, start: &[u8], end: &[u8], ts: u64) -> RevScan {
        RevScan::new(self.view(), start.to_vec(), end.to_vec(), ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_scan_matches_forward() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                memtable_bytes: 4 << 10,
                sync: false,
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let mut snaps = Vec::new();
        let mut pins = Vec::new();
        // Every version written: key -> [(ts, value)].
        let mut model: BTreeMap<Vec<u8>, Vec<(u64, Option<Bytes>)>> = BTreeMap::new();
        for round in 0..6u32 {
            // A few hot keys get many versions (each its own commit).
            for i in 0..40u32 {
                let k = format!("k{:05}", (i % 3) * 300 + 1).into_bytes();
                let v = (i % 5 != 4).then(|| Bytes::from(format!("hot{round}-{i}")));
                let ts = store.commit(vec![(k.clone(), v.clone())], |_| Ok(())).unwrap();
                model.entry(k).or_default().push((ts, v));
                if i % 13 == 0 {
                    pins.push(store.pin(ts));
                    snaps.push(ts);
                }
            }
            let mut writes = Vec::new();
            for i in 0..300u32 {
                if (i + round) % 7 == 0 {
                    writes.push((format!("k{:05}", i * 3).into_bytes(), None));
                } else if (i * 5 + round) % 3 == 0 {
                    writes.push((
                        format!("k{:05}", i * 3).into_bytes(),
                        Some(Bytes::from(format!("v{round}-{i}"))),
                    ));
                }
            }
            let ts = store.commit(writes.clone(), |_| Ok(())).unwrap();
            for (k, v) in writes {
                model.entry(k).or_default().push((ts, v));
            }
            pins.push(store.pin(ts));
            snaps.push(ts);
            if round % 2 == 1 {
                store.rotate().unwrap();
                store.flush_all().unwrap();
            }
            if round == 4 {
                store.compact().unwrap();
            }
        }
        // (The pinned snapshots keep their versions through compaction.)
        snaps.push(store.visible_ts());
        for &ts in &snaps {
            let expect: Vec<(Vec<u8>, Bytes)> = model
                .iter()
                .filter_map(|(k, vs)| {
                    let v = vs.iter().rev().find(|(t, _)| *t <= ts)?.1.clone()?;
                    Some((k.clone(), v))
                })
                .collect();
            let all: Vec<_> = store.scan(b"", b"", ts).map(|r| r.unwrap()).collect();
            assert_eq!(all, expect, "ts {ts}");
            for (k, _) in expect.iter().step_by(7) {
                assert!(store.get(k, ts).unwrap().is_some());
            }
        }
        drop(pins);
        for &ts in &snaps {
            for (a, b) in [(&b""[..], &b""[..]), (b"k00100", b"k00700"), (b"k00101", b"k00102")] {
                let mut fwd: Vec<_> = store.scan(a, b, ts).map(|r| r.unwrap()).collect();
                fwd.reverse();
                let rev: Vec<_> = store.scan_rev(a, b, ts).map(|r| r.unwrap()).collect();
                assert_eq!(fwd, rev, "ts {ts}, range {a:?}..{b:?}");
            }
        }
    }

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

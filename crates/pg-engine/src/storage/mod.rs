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

/// The store directory's subdirectory for transactions' spilled writes.
const SPILL_DIR: &str = "spill";

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
    /// Flush and compact in a background thread (off: only when asked, for tests that
    /// need the store left as a crash would leave it).
    pub background: bool,
    /// A transaction's buffered writes past about this many bytes go to disk
    /// (`txn::spill`), so a transaction of any size runs in bounded memory. The default
    /// is 64 MB, or `ROLL_TXN_SPILL_BYTES` (tests set it tiny to spill all the time).
    pub txn_spill_bytes: usize,
}

/// `set_default_txn_spill_bytes`'s value (0: none set).
static DEFAULT_SPILL_BYTES: AtomicUsize = AtomicUsize::new(0);

/// The default `txn_spill_bytes` of stores opened from now on (tests; 0: the usual one).
pub fn set_default_txn_spill_bytes(n: usize) {
    DEFAULT_SPILL_BYTES.store(n, Ordering::Relaxed);
}

/// The default `StoreOptions::txn_spill_bytes`.
fn default_spill_bytes() -> usize {
    match DEFAULT_SPILL_BYTES.load(Ordering::Relaxed) {
        0 => std::env::var("ROLL_TXN_SPILL_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(64 << 20),
        n => n,
    }
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
            background: true,
            txn_spill_bytes: default_spill_bytes(),
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
    /// Newest first (by commit timestamp: each file holds one range of them, a flushed
    /// memtable's or an ingested commit's, and the ranges don't overlap).
    l0: Vec<SstMeta>,
    /// Sorted by key, non-overlapping.
    l1: Vec<SstMeta>,
}

/// Sort writes by key, the last write of a key winning (a transaction's come sorted).
fn sort_writes(writes: &mut Vec<(Vec<u8>, Option<Bytes>)>) {
    if !writes.windows(2).all(|w| w[0].0 < w[1].0) {
        writes.sort_by(|a, b| a.0.cmp(&b.0));
        writes.dedup_by(|b, a| {
            // (The last write of a key wins.)
            if a.0 == b.0 {
                std::mem::swap(a, b);
                true
            } else {
                false
            }
        });
    }
}

/// Where an L0 file goes in a list kept newest first.
fn l0_place(l0: &[SstMeta], meta: &SstMeta) -> usize {
    l0.partition_point(|m| m.max_ts > meta.max_ts)
}

/// A frozen source: a memtable waiting to be flushed, or a big commit written as an SST
/// (`Store::ingest_locked`), in the manifest's L0 already, waiting for the memtables
/// older than it to be flushed before it joins the view's L0.
#[derive(Clone)]
enum Frozen {
    Mem(Arc<Memtable>),
    Sst(Arc<SstReader>),
}

/// An immutable picture of the store's sources. Readers hold one for their whole scan.
pub struct View {
    mem: Arc<Memtable>,
    /// Newest first.
    imm: Vec<Frozen>,
    l0: Vec<Arc<SstReader>>,
    l1: Vec<Arc<SstReader>>,
}

impl View {
    /// The memtables, newest first.
    fn memtables(&self) -> impl Iterator<Item = &Arc<Memtable>> {
        std::iter::once(&self.mem).chain(self.imm.iter().filter_map(|f| match f {
            Frozen::Mem(m) => Some(m),
            Frozen::Sst(_) => None,
        }))
    }
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
        // Leftovers from an interrupted flush or compaction, and spilled writes of
        // transactions a crash ended.
        let _ = std::fs::remove_dir_all(dir.join(SPILL_DIR));
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

        // Replay WAL files on top of the SSTs. Commits ingested as SSTs (`ingest_locked`)
        // have no WAL records and split the others: those between two of them go to a
        // memtable of their own, so every L0 file still holds one range of commit
        // timestamps, in order.
        let wal_seqs = wal::list(dir)?;
        let mut next_id = manifest
            .next_id
            .max(wal_seqs.last().map(|s| s + 1).unwrap_or(1));
        let replay_seq = next_id;
        next_id += 1;
        let mut barriers: Vec<u64> = manifest
            .l0
            .iter()
            .map(|m| m.max_ts)
            .filter(|&t| t > manifest.flushed_ts)
            .collect();
        barriers.sort_unstable();
        let groups: Vec<Memtable> = (0..=barriers.len()).map(|_| Memtable::new(replay_seq)).collect();
        let mut last_ts = manifest.last_ts.max(manifest.flushed_ts);
        last_ts = last_ts.max(barriers.last().copied().unwrap_or(0));
        let mut replayed = 0usize;
        for seq in &wal_seqs {
            let max = wal::replay(&wal::wal_path(dir, *seq), |ts, entries| {
                if ts <= manifest.flushed_ts {
                    return;
                }
                let g = &groups[barriers.partition_point(|&b| b < ts)];
                for (k, v) in entries {
                    g.insert(mvcc_key(&k, ts), v, ts);
                }
                replayed += 1;
            })?;
            last_ts = last_ts.max(max);
        }
        let mem = Arc::new(Memtable::new(replay_seq));
        // (Frozen sources go newest first; flushing takes them oldest first.)
        let imm: Vec<Frozen> = groups
            .into_iter()
            .rev()
            .filter(|g| !g.is_empty())
            .map(|g| Frozen::Mem(Arc::new(g)))
            .collect();
        let wal = WalWriter::create(dir, replay_seq, if opts.sync { wal::PREALLOC } else { 0 })?;
        let store = Arc::new(Store {
            dir: dir.to_path_buf(),
            opts,
            view: RwLock::new(Arc::new(View { mem, imm, l0, l1 })),
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

    /// Where transactions spill their writes (`txn::spill`). Emptied when the store opens:
    /// what a crash left there belongs to transactions that are gone.
    pub fn spill_dir(&self) -> PathBuf {
        self.dir.join(SPILL_DIR)
    }

    /// `StoreOptions::txn_spill_bytes`.
    pub fn txn_spill_bytes(&self) -> usize {
        self.opts.txn_spill_bytes
    }

    /// The newest commit timestamp; a snapshot at this ts sees every committed write.
    pub fn visible_ts(&self) -> u64 {
        self.visible.load(Ordering::Acquire)
    }

    /// The last commit timestamp handed out: no version newer than it can be read
    /// (commits waiting for their WAL fsync included).
    pub fn last_assigned_ts(&self) -> u64 {
        self.assigned.load(Ordering::Acquire)
    }

    /// Run `f` with the commit lock held, so no commit takes a timestamp meanwhile, given
    /// the last one assigned (a logical slot's consistent point: `repl::sqlfn`).
    pub fn with_commit_lock<T>(&self, f: impl FnOnce(u64) -> T) -> T {
        let _c = self.commit.lock();
        f(self.assigned.load(Ordering::Acquire))
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
        self.view().get_version(key, ts)
    }

    /// Commit ts of the newest version of `key`, committed or deleted (for conflict checks).
    pub fn latest_version_ts(&self, key: &[u8]) -> Result<Option<u64>> {
        Ok(self.get_version(key, u64::MAX >> 1)?.map(|(ts, _)| ts))
    }
}

impl View {
    /// A view of `files` alone, newest first: a transaction's spilled writes
    /// (`txn::spill`), read with the store's own merging and version rules.
    pub fn of_files(files: Vec<Arc<SstReader>>) -> Arc<View> {
        Arc::new(View {
            mem: Arc::new(Memtable::new(0)),
            imm: Vec::new(),
            l0: files,
            l1: Vec::new(),
        })
    }

    /// Ascending scan of user keys in [start, end) at `ts`, nothing pruned.
    pub fn scan(self: &Arc<Self>, start: &[u8], end: &[u8], ts: u64) -> Scan {
        Scan::new(self.clone(), start.to_vec(), end.to_vec(), ts, 0)
    }

    /// Descending scan of user keys in [start, end) at `ts`.
    pub fn scan_rev(self: &Arc<Self>, start: &[u8], end: &[u8], ts: u64) -> RevScan {
        RevScan::new(self.clone(), start.to_vec(), end.to_vec(), ts)
    }

    /// (ts, value) of the newest version of `key` at or before `ts`; a tombstone has value None.
    pub fn get_version(&self, key: &[u8], ts: u64) -> Result<Option<(u64, Option<Bytes>)>> {
        let view = self;
        let seek = mvcc_key(key, ts);
        let mem_get = |m: &Memtable| {
            m.map
                .range::<[u8], _>((Bound::Included(seek.as_slice()), Bound::Unbounded))
                .next()
                .filter(|e| user_key(e.key()) == key)
                .map(|e| (key_ts(e.key()), e.value().clone()))
        };
        if let Some(hit) = mem_get(&view.mem) {
            return Ok(Some(hit));
        }
        for f in &view.imm {
            let hit = match f {
                Frozen::Mem(m) => mem_get(m),
                Frozen::Sst(s) => sst_get(s, key, &seek)?,
            };
            if hit.is_some() {
                return Ok(hit);
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
}

impl Store {
    /// `get_version` of each of `keys` (ascending, no duplicates), read together: one view
    /// of the sources, each file's blocks read once for the keys falling in them (an index
    /// scan's heap fetches, a run at a time).
    pub fn get_versions(
        &self,
        keys: &[&[u8]],
        ts: u64,
    ) -> Result<Vec<Option<(u64, Option<Bytes>)>>> {
        debug_assert!(keys.windows(2).all(|w| w[0] < w[1]));
        let view = self.view();
        let mut out: Vec<Option<(u64, Option<Bytes>)>> = vec![None; keys.len()];
        let seeks: Vec<Vec<u8>> = keys.iter().map(|k| mvcc_key(k, ts)).collect();
        // The keys no source has answered yet.
        let mut left: Vec<usize> = (0..keys.len()).collect();
        let mut found = Vec::new();
        let mem_get = |m: &Memtable, left: &mut Vec<usize>, out: &mut Vec<Option<(u64, Option<Bytes>)>>| {
            if m.is_empty() {
                return;
            }
            left.retain(|&i| {
                let hit = m
                    .map
                    .range::<[u8], _>((Bound::Included(seeks[i].as_slice()), Bound::Unbounded))
                    .next()
                    .filter(|e| user_key(e.key()) == keys[i]);
                match hit {
                    Some(e) => {
                        out[i] = Some((key_ts(e.key()), e.value().clone()));
                        false
                    }
                    None => true,
                }
            });
        };
        mem_get(&view.mem, &mut left, &mut out);
        // Files in age order: the frozen ones, then L0's, then L1 (one level).
        let files = view
            .imm
            .iter()
            .map(|f| match f {
                Frozen::Mem(m) => Err(m),
                Frozen::Sst(s) => Ok(std::slice::from_ref(s)),
            })
            .chain(view.l0.iter().map(|s| Ok(std::slice::from_ref(s))))
            .chain((!view.l1.is_empty()).then_some(Ok(view.l1.as_slice())));
        for f in files {
            if left.is_empty() {
                break;
            }
            match f {
                Err(m) => mem_get(m, &mut left, &mut out),
                Ok(ssts) => {
                    ssts_get(ssts, keys, &seeks, &left, &mut found)?;
                    for (i, hit) in found.drain(..) {
                        out[i] = Some(hit);
                    }
                    left.retain(|&i| out[i].is_none());
                }
            }
        }
        Ok(out)
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
        self.commit_grouped(writes, |s, _| check(s), |_| {}, |_| {})
    }

    /// Whether a commit of `writes` is big enough to go in as an SST of its own
    /// (`ingest_locked`): a quarter of a memtable.
    fn ingests(&self, writes: &[(Vec<u8>, Option<Bytes>)]) -> bool {
        let limit = self.opts.memtable_bytes / 4;
        let mut n = 0usize;
        for (k, v) in writes {
            n += k.len() + v.as_ref().map_or(0, |v| v.len()) + 16;
            if n >= limit {
                return true;
            }
        }
        false
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

    /// `commit_with` for a commit that publishes a new catalog, group-committed as `commit`
    /// is (round 19, agent 76): `appended(ts)` runs under the commit lock once the writes are
    /// in the WAL and memtable (the commit's catalog becomes the one the next commit checks
    /// against), then the fsync is waited for without the lock (one waiting commit does it
    /// for all appended so far), and `after(ts)` runs once they are durable and visible (it
    /// publishes the catalog). Concurrent DDL commits (`pg_restore -j`) each held the
    /// commit lock through their own fsync, so they took turns at the disk's latency.
    pub fn commit_grouped(
        self: &Arc<Self>,
        mut writes: Vec<(Vec<u8>, Option<Bytes>)>,
        check: impl FnOnce(&Store, &mut Vec<(Vec<u8>, Option<Bytes>)>) -> Result<()>,
        appended: impl FnOnce(u64),
        after: impl FnOnce(u64),
    ) -> Result<u64> {
        if !self.opts.sync || self.ingests(&writes) {
            return self.commit_with(writes, check, after);
        }
        let rotate;
        let ts;
        {
            let mut c = self.commit.lock();
            if self.closed.load(Ordering::Acquire) {
                return Err(internal("database is closed"));
            }
            check(self, &mut writes)?;
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
            appended(ts);
            rotate = mem.bytes() >= self.opts.memtable_bytes;
        }
        self.wait_durable(ts)?;
        after(ts);
        if rotate {
            self.rotate()?;
            self.schedule_background();
        }
        Ok(ts)
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
        let ingest = self.ingests(&writes);
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
            if ingest {
                self.ingest_locked(&mut c, ts, writes)?;
                after(ts);
                rotate = false;
            } else {
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
        }
        if rotate {
            self.rotate()?;
        }
        if rotate || ingest {
            self.schedule_background();
        }
        Ok(ts)
    }

    /// Commit writes too many to hold in memory (a transaction's spilled ones, `txn::spill`):
    /// `writes` come in key order (strictly ascending: each key's last write) and go in as
    /// an SST of their own (`ingest_locked`), merged with the writes `check` gives under the
    /// commit lock (the catalog's records: sorted here, winning over `writes`); `each` checks
    /// every key of `writes` under the lock as it is written. `keys`: about how many.
    pub fn commit_sorted(
        self: &Arc<Self>,
        keys: usize,
        writes: &mut dyn Iterator<Item = Result<(Vec<u8>, Option<Bytes>)>>,
        check: impl FnOnce(&Store, &mut Vec<(Vec<u8>, Option<Bytes>)>) -> Result<()>,
        each: &dyn Fn(&Store, &[u8]) -> Result<()>,
        after: impl FnOnce(u64),
    ) -> Result<u64> {
        let ts;
        {
            let mut c = self.commit.lock();
            if self.closed.load(Ordering::Acquire) {
                return Err(internal("database is closed"));
            }
            let mut extra = Vec::new();
            check(self, &mut extra)?;
            sort_writes(&mut extra);
            ts = self.assigned.load(Ordering::Acquire) + 1;
            self.ingest_stream_locked(&mut c, ts, keys, writes, extra, each)?;
            after(ts);
        }
        self.schedule_background();
        Ok(ts)
    }

    /// Commit `writes` at `ts` as an SST of their own instead of WAL records and memtable
    /// entries (a bulk load's, an index build's): written once, with no copy in memory. The
    /// memtable is frozen first, so every earlier commit is in an older source; the file is
    /// fsynced and in the manifest's L0 (durable) before the commit is visible, and joins
    /// the view's L0 when the memtables frozen before it have been flushed (`flush_all`).
    /// After a crash, the WAL's commits before and after it are replayed around it
    /// (`Store::open`).
    fn ingest_locked(
        &self,
        c: &mut CommitState,
        ts: u64,
        mut writes: Vec<(Vec<u8>, Option<Bytes>)>,
    ) -> Result<()> {
        // (A transaction's writes come sorted; others are sorted here.)
        sort_writes(&mut writes);
        let n = writes.len();
        self.ingest_stream_locked(c, ts, n, &mut writes.into_iter().map(Ok), Vec::new(), &|_, _| Ok(()))
    }

    /// `ingest_locked` of `writes` coming in key order (strictly ascending), merged with
    /// `extra` (sorted, winning over `writes` on a key); `each` checks each key of `writes`
    /// first and fails the commit (a serializable commit's conflict check). `keys` is about
    /// how many keys there are (the bloom filter's size).
    fn ingest_stream_locked(
        &self,
        c: &mut CommitState,
        ts: u64,
        keys: usize,
        writes: &mut dyn Iterator<Item = Result<(Vec<u8>, Option<Bytes>)>>,
        extra: Vec<(Vec<u8>, Option<Bytes>)>,
        each: &dyn Fn(&Store, &[u8]) -> Result<()>,
    ) -> Result<()> {
        self.rotate_locked(c)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // (Uncompressed: the commit waits for this file, and compaction, in the background,
        // compresses it when it moves to L1.)
        let mut w = SstWriter::create(&self.dir, id, false)?;
        w.expect_keys(keys + extra.len());
        let mut key = Vec::new();
        let mut add = |w: &mut SstWriter, k: &[u8], v: Option<&[u8]>| {
            key.clear();
            key.extend_from_slice(k);
            key.extend_from_slice(&(!ts).to_be_bytes());
            w.add(&key, v)
        };
        let mut extra = extra.into_iter().peekable();
        let mut fill = || -> Result<()> {
            for item in &mut *writes {
                let (k, v) = item?;
                each(self, &k)?;
                let mut shadowed = false;
                while let Some((ek, _)) = extra.peek()
                    && *ek <= k
                {
                    let (ek, ev) = extra.next().unwrap();
                    shadowed |= ek == k;
                    add(&mut w, &ek, ev.as_deref())?;
                }
                if !shadowed {
                    add(&mut w, &k, v.as_deref())?;
                }
            }
            for (ek, ev) in extra.by_ref() {
                add(&mut w, &ek, ev.as_deref())?;
            }
            Ok(())
        };
        if let Err(e) = fill() {
            w.abandon();
            return Err(e);
        }
        if w.entries() == 0 {
            // (Nothing to write after all: the commit takes its timestamp alone.)
            w.abandon();
            self.assigned.store(ts, Ordering::Release);
            self.made_durable(ts);
            return Ok(());
        }
        let meta = w.finish()?;
        let reader = match SstReader::open(&self.dir, meta.clone(), self.opts.cache.clone()) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                let _ = std::fs::remove_file(sst::sst_path(&self.dir, id));
                return Err(e);
            }
        };
        {
            let mut m = self.manifest.lock();
            let mut next = m.clone();
            next.l0.insert(l0_place(&next.l0, &meta), meta);
            next.last_ts = ts;
            next.next_id = self.next_id.load(Ordering::Relaxed);
            if let Err(e) = write_manifest(&self.dir, &next) {
                reader.mark_obsolete();
                return Err(e);
            }
            *m = next;
            // (Handed out before its versions can be read: `last_assigned_ts`.)
            self.assigned.store(ts, Ordering::Release);
            let mut v = self.view.write();
            let mut imm = vec![Frozen::Sst(reader)];
            imm.extend(v.imm.iter().cloned());
            *v = Arc::new(View {
                mem: v.mem.clone(),
                imm,
                l0: v.l0.clone(),
                l1: v.l1.clone(),
            });
        }
        self.made_durable(ts);
        Ok(())
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
        self.rotate_locked(&mut c)
    }

    /// `rotate` under the commit lock.
    fn rotate_locked(&self, c: &mut CommitState) -> Result<()> {
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
        let mut imm = vec![Frozen::Mem(v.mem.clone())];
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
        if !self.opts.background || self.bg_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        std::thread::Builder::new()
            .name("roll-flush".into())
            .spawn(move || {
                loop {
                    let r = this.flush_all().and_then(|_| this.maybe_compact());
                    this.bg_running.store(false, Ordering::Release);
                    if let Err(e) = r {
                        warn!(dir = %this.dir.display(), error = %e, "background flush/compaction failed");
                        break;
                    }
                    // (Sources frozen while it ran, which their scheduling left to it.)
                    if this.view.read().imm.is_empty()
                        || this.closed.load(Ordering::Acquire)
                        || this.bg_running.swap(true, Ordering::AcqRel)
                    {
                        break;
                    }
                }
            })
            .ok();
    }

    /// Write every frozen memtable to L0, oldest first (an ingested commit's SST, in the
    /// manifest's L0 already, just joins the view's when its turn comes).
    pub fn flush_all(&self) -> Result<()> {
        let _g = self.bg.lock();
        loop {
            let oldest = { self.view.read().imm.last().cloned() };
            let Some(frozen) = oldest else { return Ok(()) };
            let mem = match &frozen {
                Frozen::Mem(m) => m.clone(),
                Frozen::Sst(s) => {
                    let mut m = self.manifest.lock();
                    // (Every commit up to it is in SSTs now.)
                    m.flushed_ts = m.flushed_ts.max(s.meta.max_ts);
                    m.last_ts = self.visible_ts();
                    write_manifest(&self.dir, &m)?;
                    let mut v = self.view.write();
                    let imm: Vec<Frozen> = v
                        .imm
                        .iter()
                        .filter(|x| !matches!(x, Frozen::Sst(y) if Arc::ptr_eq(y, s)))
                        .cloned()
                        .collect();
                    let mut l0 = v.l0.clone();
                    let at = l0.partition_point(|x| x.meta.max_ts > s.meta.max_ts);
                    l0.insert(at, s.clone());
                    *v = Arc::new(View {
                        mem: v.mem.clone(),
                        imm,
                        l0,
                        l1: v.l1.clone(),
                    });
                    continue;
                }
            };
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
                let at = l0_place(&m.l0, &meta);
                m.l0.insert(at, meta);
                m.flushed_ts = m.flushed_ts.max(flushed_ts);
                m.last_ts = self.visible_ts();
                m.next_id = self.next_id.load(Ordering::Relaxed);
                write_manifest(&self.dir, &m)?;
                let mut v = self.view.write();
                let imm: Vec<Frozen> = v
                    .imm
                    .iter()
                    .filter(|x| !matches!(x, Frozen::Mem(y) if Arc::ptr_eq(y, &mem)))
                    .cloned()
                    .collect();
                let mut l0 = v.l0.clone();
                let at = l0.partition_point(|x| x.meta.max_ts > reader.meta.max_ts);
                l0.insert(at, reader);
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

    /// Merge L0 into L1: every L0 file with the L1 files whose user keys overlap theirs
    /// (the others stay as they are: a bulk load into a new table rewrites none of the
    /// database's other files).
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
        // (By user key: a user key's versions are all in one L1 file.)
        let lo = inputs_l0
            .iter()
            .map(|s| user_key(&s.meta.first_key))
            .min()
            .unwrap_or_default();
        let hi = inputs_l0
            .iter()
            .map(|s| user_key(&s.meta.last_key))
            .max()
            .unwrap_or_default();
        let first = view
            .l1
            .partition_point(|s| user_key(&s.meta.last_key) < lo);
        let last = view
            .l1
            .partition_point(|s| user_key(&s.meta.first_key) <= hi)
            .max(first);
        let inputs_l1: Vec<Arc<SstReader>> = view.l1[first..last].to_vec();
        let horizon = self.gc_horizon();
        let mut sources: Vec<Cursor> = inputs_l0
            .iter()
            .map(|s| Cursor::level_uncached(vec![s.clone()]))
            .collect();
        sources.push(Cursor::level_uncached(inputs_l1.clone()));
        let mut merge = Merge::new(sources, Vec::new());
        let mut outputs: Vec<SstMeta> = Vec::new();
        let mut writer: Option<SstWriter> = None;
        let mut cur_user: Vec<u8> = Vec::new();
        let mut started = false;
        let mut kept_old = false;
        while let Some(i) = merge.min()? {
            let (k, v) = merge.sources[i].head().expect("a head entry");
            let uk = user_key(k);
            let ts = key_ts(k);
            if !started || cur_user.as_slice() != uk {
                started = true;
                cur_user.clear();
                cur_user.extend_from_slice(uk);
                kept_old = false;
            }
            if ts <= horizon {
                if kept_old {
                    merge.sources[i].advance();
                    continue;
                }
                kept_old = true;
                // L1 is the bottom level, so a tombstone nobody can see past can go.
                if v.is_none() {
                    merge.sources[i].advance();
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
            writer.as_mut().unwrap().add(k, v)?;
            merge.sources[i].advance();
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
            // (Only compaction changes L1, under `bg`: the view's is the manifest's.)
            let mut l1_meta: Vec<SstMeta> = view.l1[..first].iter().map(|s| s.meta.clone()).collect();
            l1_meta.extend(outputs.iter().cloned());
            l1_meta.extend(view.l1[last..].iter().map(|s| s.meta.clone()));
            m.l1 = l1_meta;
            m.next_id = self.next_id.load(Ordering::Relaxed);
            write_manifest(&self.dir, &m)?;
            let mut v = self.view.write();
            let l0: Vec<_> =
                v.l0.iter()
                    .filter(|s| !input_ids.contains(&s.meta.id))
                    .cloned()
                    .collect();
            let mut l1: Vec<Arc<SstReader>> = view.l1[..first].to_vec();
            l1.extend(readers);
            l1.extend(view.l1[last..].iter().cloned());
            *v = Arc::new(View {
                mem: v.mem.clone(),
                imm: v.imm.clone(),
                l0,
                l1,
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
        let ingested = v.imm.iter().filter_map(|f| match f {
            Frozen::Sst(s) => Some(s),
            Frozen::Mem(_) => None,
        });
        let sst: u64 = v.l0.iter().chain(v.l1.iter()).chain(ingested).map(|s| s.meta.size).sum();
        let mem: usize = v.memtables().map(|m| m.bytes()).sum();
        sst + mem as u64
    }

    pub fn memory_bytes(&self) -> usize {
        self.view().memtables().map(|m| m.bytes()).sum()
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
        let i = block.seek(seek);
        if i < block.len() {
            let k = block.key(i);
            if user_key(k) == key {
                return Ok(Some((key_ts(k), block.value(i))));
            }
            return Ok(None);
        }
        b += 1;
    }
    Ok(None)
}

/// `sst_get` of the keys `left` picks (ascending) over non-overlapping files in key
/// order (one L0 file, or L1), each block read once for every key in it: the hits, by
/// key index, go to `found`.
fn ssts_get(
    ssts: &[Arc<SstReader>],
    keys: &[&[u8]],
    seeks: &[Vec<u8>],
    left: &[usize],
    found: &mut Vec<(usize, (u64, Option<Bytes>))>,
) -> Result<()> {
    let mut file = 0;
    let mut cur: Option<(usize, usize, Arc<sst::Block>)> = None;
    for &i in left {
        let (key, seek) = (keys[i], seeks[i].as_slice());
        while file < ssts.len() && user_key(&ssts[file].meta.last_key) < key {
            file += 1;
        }
        let Some(sst) = ssts.get(file) else { break };
        if key < user_key(&sst.meta.first_key) || !sst.may_contain(key) {
            continue;
        }
        let mut b = sst.find_block(seek);
        while b < sst.block_count() {
            let block = match &cur {
                Some((f, bi, blk)) if *f == file && *bi == b => blk.clone(),
                _ => {
                    let blk = sst.block(b)?;
                    cur = Some((file, b, blk.clone()));
                    blk
                }
            };
            let j = block.seek(seek);
            if j < block.len() {
                let k = block.key(j);
                if user_key(k) == key {
                    found.push((i, (key_ts(k), block.value(j))));
                }
                break;
            }
            b += 1;
        }
    }
    Ok(())
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
        /// Blocks read from the files go into the block cache (not for compaction).
        fill: bool,
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
            fill: true,
        }
    }

    /// `level` over every key, its blocks kept out of the cache (compaction's inputs).
    fn level_uncached(ssts: Vec<Arc<SstReader>>) -> Cursor {
        let mut c = Cursor::level(ssts, &[], &[]);
        if let Cursor::Level { fill, .. } = &mut c {
            *fill = false;
        }
        c
    }

    /// Position on the next entry (reading a batch or a block when needed); `head` then
    /// shows it.
    fn fill(&mut self) -> Result<()> {
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
                        Bound::Excluded(end.as_slice())
                    };
                    let lower = match from {
                        Bound::Included(k) => Bound::Included(k.as_slice()),
                        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
                        Bound::Unbounded => Bound::Unbounded,
                    };
                    for e in mem.map.range::<[u8], _>((lower, upper)).take(*batch) {
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
                Ok(())
            }
            Cursor::Level {
                ssts,
                file,
                block,
                block_idx,
                pos,
                end: _,
                start,
                started,
                fill,
            } => loop {
                if *file >= ssts.len() {
                    return Ok(());
                }
                if !*started {
                    *started = true;
                    // Position at the first entry >= start.
                    if !start.is_empty() {
                        *file =
                            ssts.partition_point(|s| s.meta.last_key.as_slice() < start.as_slice());
                        if *file >= ssts.len() {
                            return Ok(());
                        }
                        let sst = &ssts[*file];
                        *block_idx = sst.find_block(start);
                        if *block_idx < sst.block_count() {
                            let b = sst.block_with(*block_idx, *fill)?;
                            *pos = b.seek(start);
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
                    *block = Some(sst.block_with(*block_idx, *fill)?);
                    *pos = 0;
                }
                if *pos >= block.as_ref().unwrap().len() {
                    *block = None;
                    *block_idx += 1;
                    continue;
                }
                return Ok(());
            },
        }
    }

    /// The entry `fill` positioned on (None: none left), without reading further.
    fn head(&self) -> Option<(&[u8], Option<&[u8]>)> {
        match self {
            Cursor::Mem { buf, .. } => buf.front().map(|(k, v)| (k.as_slice(), v.as_deref())),
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
                let b = block.as_ref()?;
                if *pos >= b.len() {
                    return None;
                }
                let k = b.key(*pos);
                (end.is_empty() || k < end.as_slice()).then(|| (k, b.value_ref(*pos)))
            }
        }
    }

    /// The head entry, owned, and on past it.
    fn take(&mut self) -> RawEntry {
        match self {
            Cursor::Mem { buf, .. } => buf.pop_front().expect("a head entry"),
            Cursor::Level { block, pos, .. } => {
                let b = block.as_ref().expect("a head entry");
                let e = (b.key(*pos).to_vec(), b.value(*pos));
                *pos += 1;
                e
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
                    if *pos < b.len() && b.key(*pos) >= target {
                        return;
                    }
                    if b.last_key().is_some_and(|k| k >= target) {
                        *pos = b.seek(target);
                        return;
                    }
                }
                // Further on: positioned again as at the start (`fill`).
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

    /// The source holding the smallest raw entry left (None: none below the end). Identical
    /// storage keys can't occur in two sources (a commit ts belongs to one memtable), so
    /// ties don't need breaking.
    fn min(&mut self) -> Result<Option<usize>> {
        for c in &mut self.sources {
            c.fill()?;
        }
        let mut best: Option<(usize, &[u8])> = None;
        for (i, c) in self.sources.iter().enumerate() {
            let Some((k, _)) = c.head() else {
                continue;
            };
            if best.is_none_or(|(_, b)| k < b) {
                best = Some((i, k));
            }
        }
        let Some((i, k)) = best else { return Ok(None) };
        if !self.end.is_empty() && k >= self.end.as_slice() {
            return Ok(None);
        }
        Ok(Some(i))
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
        for f in &view.imm {
            sources.push(match f {
                Frozen::Mem(m) => Cursor::mem(m.clone(), &start, &end),
                Frozen::Sst(s) => Cursor::level(vec![s.clone()], &start, &end),
            });
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
        for m in self._view.memtables() {
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
            let i = match self.merge.min() {
                Ok(Some(i)) => i,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            // (Entries passed over are never copied.)
            let (k, v) = self.merge.sources[i].head().expect("a head entry");
            let uk = user_key(k);
            if self.decided.as_deref() == Some(uk) {
                // Another version of a key decided already. Past a second one, a seek past
                // all of them (with the dead ones pruned): a row updated many times. (User
                // keys are prefix-free, so no other key's entries lie before the target.)
                self.skipped += 1;
                if self.skipped >= 2 {
                    let mut target = uk.to_vec();
                    target.extend_from_slice(&[0xFF; 9]);
                    self.merge.sources[i].advance();
                    self.prune(&target[..target.len() - 9]);
                    self.merge.seek(&target);
                } else {
                    self.merge.sources[i].advance();
                }
                continue;
            }
            let ts = key_ts(k);
            if ts > self.ts {
                // Versions newer than the snapshot: past a second one, a seek to the newest
                // one it sees.
                if self.newer.as_deref() == Some(uk) {
                    let target = mvcc_key(uk, self.ts);
                    self.merge.sources[i].advance();
                    self.merge.seek(&target);
                } else {
                    self.newer = Some(uk.to_vec());
                    self.merge.sources[i].advance();
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
            self.decided_ts = ts;
            if v.is_none() {
                self.merge.sources[i].advance();
                continue;
            }
            let (mut k, v) = self.merge.sources[i].take();
            k.truncate(k.len().saturating_sub(8));
            return Some(Ok((k, v.expect("a value"))));
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
    fn fill(&mut self) -> Result<()> {
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
                Ok(())
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
                        return Ok(());
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
                    *pos = if end.is_empty() { b.len() } else { b.seek(end) };
                    *block = Some(b);
                }
                if *file == 0 {
                    return Ok(());
                }
                if block.is_none() {
                    if *block_idx == 0 {
                        // On to the previous file's last block.
                        *file -= 1;
                        if *file == 0 {
                            return Ok(());
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
                if block.as_ref().unwrap().key(*pos - 1) < start.as_slice() {
                    *file = 0;
                }
                return Ok(());
            },
        }
    }

    fn head(&self) -> Option<&[u8]> {
        match self {
            RevCursor::Mem { buf, .. } => buf.front().map(|e| e.0.as_slice()),
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
                let k = block.as_ref()?.key(*pos - 1);
                (k >= start.as_slice()).then_some(k)
            }
        }
    }

    fn take(&mut self) -> RawEntry {
        match self {
            RevCursor::Mem { buf, .. } => buf.pop_front().expect("a head entry"),
            RevCursor::Level { block, pos, .. } => {
                let b = block.as_ref().expect("a head entry");
                *pos -= 1;
                (b.key(*pos).to_vec(), b.value(*pos))
            }
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
        sources.extend(view.imm.iter().map(|f| match f {
            Frozen::Mem(m) => mem(m),
            Frozen::Sst(s) => level(vec![s.clone()]),
        }));
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
            c.fill()?;
        }
        let mut best: Option<(usize, &[u8])> = None;
        for (i, c) in self.sources.iter().enumerate() {
            let Some(k) = c.head() else { continue };
            if best.is_none_or(|(_, b)| k > b) {
                best = Some((i, k));
            }
        }
        let Some((b, _)) = best else { return Ok(None) };
        Ok(Some(self.sources[b].take()))
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

    /// Every version written: key -> [(ts, value)].
    type Model = BTreeMap<Vec<u8>, Vec<(u64, Option<Bytes>)>>;

    /// What a snapshot at `ts` sees in `model`.
    fn model_at(model: &Model, ts: u64) -> Vec<(Vec<u8>, Bytes)> {
        model
            .iter()
            .filter_map(|(k, vs)| {
                let v = vs.iter().rev().find(|(t, _)| *t <= ts)?.1.clone()?;
                Some((k.clone(), v))
            })
            .collect()
    }

    /// The store at `ts` read every way (scans both ways, point and batched reads) against
    /// the model.
    fn check_at(s: &Store, model: &Model, ts: u64, what: &str) {
        let expect = model_at(model, ts);
        let all: Vec<_> = s.scan(b"", b"", ts).map(|r| r.unwrap()).collect();
        assert_eq!(all, expect, "{what}: scan at {ts}");
        let mut rev: Vec<_> = s.scan_rev(b"", b"", ts).map(|r| r.unwrap()).collect();
        rev.reverse();
        assert_eq!(rev, expect, "{what}: reverse scan at {ts}");
        let keys: Vec<&[u8]> = model.keys().map(|k| k.as_slice()).collect();
        let got = s.get_versions(&keys, ts).unwrap();
        for (k, g) in keys.iter().zip(got) {
            let want = model_at(&BTreeMap::from([(k.to_vec(), model[*k].clone())]), ts)
                .pop()
                .map(|e| e.1);
            assert_eq!(g.and_then(|(_, v)| v), want, "{what}: get_versions {k:?} at {ts}");
            assert_eq!(s.get(k, ts).unwrap(), want, "{what}: get {k:?} at {ts}");
        }
    }

    /// Big commits go in as SSTs of their own; small ones before and after them, unflushed,
    /// are recovered around them after a crash (the store dropped unclosed, its background
    /// work off so nothing was flushed), and flushing and partial compactions keep every
    /// version a pinned snapshot needs.
    #[test]
    fn ingested_commits_and_crash_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let o = || StoreOptions {
            memtable_bytes: 16 << 10,
            sync: false,
            background: false,
            l0_compaction_trigger: 3,
            ..StoreOptions::default()
        };
        let mut s = Store::open(dir.path(), o()).unwrap();
        let mut model: Model = BTreeMap::new();
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut pins: Vec<SnapshotPin> = Vec::new();
        let mut around = 0;
        for round in 0..40u64 {
            // Small commits: a few keys each, under the ingest size.
            for _ in 0..rnd(6) {
                let writes: Vec<(Vec<u8>, Option<Bytes>)> = (0..1 + rnd(4))
                    .map(|_| {
                        let k = format!("k{:04}", rnd(600)).into_bytes();
                        let v = (rnd(5) != 0).then(|| Bytes::from(format!("s{round}-{}", rnd(1000))));
                        (k, v)
                    })
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect();
                assert!(!s.ingests(&writes));
                let ts = s.commit(writes.clone(), |_| Ok(())).unwrap();
                for (k, v) in writes {
                    model.entry(k).or_default().push((ts, v));
                }
            }
            // A big one now and then: a run of keys, some deleted.
            if rnd(3) == 0 {
                let from = rnd(500);
                let writes: Vec<(Vec<u8>, Option<Bytes>)> = (from..from + 100 + rnd(200))
                    .map(|i| {
                        let v = (i % 7 != 3).then(|| Bytes::from(format!("big{round}-{i}-{}", "x".repeat(40))));
                        (format!("k{i:04}").into_bytes(), v)
                    })
                    .collect();
                assert!(s.ingests(&writes));
                let ts = s.commit(writes.clone(), |_| Ok(())).unwrap();
                for (k, v) in writes {
                    model.entry(k).or_default().push((ts, v));
                }
            }
            match rnd(6) {
                0 => s.flush_all().unwrap(),
                1 => {
                    s.rotate().unwrap();
                    s.flush_all().unwrap();
                    s.maybe_compact().unwrap();
                }
                2 => {
                    // A crash: nothing closed or flushed; pinned snapshots are gone with it.
                    pins.clear();
                    let last = s.visible_ts();
                    // (The case to recover around: an ingested commit with older commits
                    // only in the WAL.)
                    let imm = s.view().imm.clone();
                    if imm.iter().position(|f| matches!(f, Frozen::Sst(_)))
                        .is_some_and(|i| imm[i..].iter().any(|f| matches!(f, Frozen::Mem(_))))
                    {
                        around += 1;
                    }
                    drop(imm);
                    drop(s);
                    s = Store::open(dir.path(), o()).unwrap();
                    assert_eq!(s.visible_ts(), last, "round {round}: recovered visible ts");
                    // (Recovery flushed every memtable: L0 holds one range of commits per
                    // file, in order.)
                    let v = s.view();
                    assert!(v.imm.is_empty());
                    assert!(v.l0.windows(2).all(|w| w[0].meta.min_ts > w[1].meta.max_ts));
                }
                _ => {}
            }
            if rnd(4) == 0 {
                pins.push(s.pin_visible());
            }
            let latest = s.visible_ts();
            check_at(&s, &model, latest, &format!("round {round}"));
            for p in &pins {
                check_at(&s, &model, p.ts, &format!("round {round}, pinned"));
            }
        }
        // A clean close leaves nothing to replay; reopened, everything reads the same.
        assert!(around > 0, "no crash with an ingested commit over unflushed ones");
        let last = s.visible_ts();
        s.close().unwrap();
        drop(s);
        let s = Store::open(dir.path(), o()).unwrap();
        assert_eq!(s.visible_ts(), last);
        check_at(&s, &model, last, "after close");
        assert!(wal::list(dir.path()).unwrap().iter().all(|seq| {
            std::fs::metadata(wal::wal_path(dir.path(), *seq)).map_or(true, |m| m.len() == 0)
        }));
    }

    /// Compaction rewrites only the L1 files overlapping L0's keys.
    #[test]
    fn compaction_leaves_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(
            dir.path(),
            StoreOptions {
                memtable_bytes: 64 << 10,
                sync: false,
                background: false,
                target_file_bytes: 8 << 10,
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let mut model: Model = BTreeMap::new();
        let mut write = |s: &Arc<Store>, keys: std::ops::Range<u32>, tag: &str| {
            let writes: Vec<_> = keys
                .map(|i| (format!("t{i:05}").into_bytes(), Some(Bytes::from(format!("{tag}-{i}-{}", "y".repeat(30))))))
                .collect();
            let ts = s.commit(writes.clone(), |_| Ok(())).unwrap();
            for (k, v) in writes {
                model.entry(k).or_default().push((ts, v));
            }
        };
        write(&s, 0..2000, "a");
        s.flush_all().unwrap();
        s.compact().unwrap();
        let before: Vec<u64> = s.view().l1.iter().map(|f| f.meta.id).collect();
        assert!(before.len() > 4);
        // New keys past the end (a new table's rows): no L1 file is rewritten.
        write(&s, 5000..5100, "b");
        s.rotate().unwrap();
        s.flush_all().unwrap();
        s.compact().unwrap();
        let after: Vec<u64> = s.view().l1.iter().map(|f| f.meta.id).collect();
        assert_eq!(&after[..before.len()], &before[..], "{before:?} -> {after:?}");
        // An update inside: the files around it are, the rest aren't.
        write(&s, 1000..1010, "c");
        s.rotate().unwrap();
        s.flush_all().unwrap();
        s.compact().unwrap();
        let again: Vec<u64> = s.view().l1.iter().map(|f| f.meta.id).collect();
        let kept = after.iter().filter(|id| again.contains(id)).count();
        assert!(kept >= after.len() - 2, "{after:?} -> {again:?}");
        let v = s.view();
        assert!(v.l0.is_empty());
        assert!(v.l1.windows(2).all(|w| user_key(&w[0].meta.last_key) < user_key(&w[1].meta.first_key)));
        drop(v);
        check_at(&s, &model, s.visible_ts(), "compacted");
    }
}

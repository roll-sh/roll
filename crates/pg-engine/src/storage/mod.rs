//! Multi-version LSM store for one database.
//!
//! Every stored key is `user_key | !commit_ts` (big-endian), so versions of a key sit together,
//! newest first, and a snapshot read is a single seek. Sources are ordered by age (memtable,
//! frozen memtables, L0 newest first, then L1), so the first source holding a visible version
//! of a key has the newest one.

pub mod batch;
pub mod cache;
pub mod cols;
mod dead;
pub mod discard;
pub mod durable;
pub mod fileio;
mod hot;
pub mod keys;
pub mod point;
#[cfg(feature = "powerloss")]
pub mod powerloss;
pub mod rdel;
pub mod row;
pub mod sst;
pub mod toast;
pub mod wal;

use crate::error::{Result, internal};
use bytes::Bytes;
use cache::BlockCache;
use crossbeam_skiplist::SkipMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sst::{SstMeta, SstReader, SstWriter, key_ts, user_key};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tracing::{debug, info, warn};
use wal::WalWriter;

/// The store directory's subdirectory for transactions' spilled writes.
const SPILL_DIR: &str = "spill";

/// Spilled runs linked into their commits as they were (`Store::link_run`; tests check).
pub static LINKED_RUNS: AtomicU64 = AtomicU64::new(0);

/// The table a key is of, for `Store::untouched`: its first 4 bytes, a range deletion's
/// marker's those of the range's start.
fn key_table(k: &[u8]) -> u32 {
    let k = if rdel::is_marker(k) { &k[rdel::PREFIX.len()..] } else { k };
    k.get(..4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Invalidate heap measures only for writes to rows or their out-of-line values.
/// Retiring a range marker after compaction changes no visible row; index-only writes
/// likewise leave the measured heap alone.
fn unwatch_written(watched: &mut HashMap<u32, u64>, key: &[u8], value: Option<&[u8]>) {
    if rdel::is_marker(key) {
        if let Some(end) = value {
            let start = &key[rdel::PREFIX.len()..];
            watched.retain(|table, _| {
                let rows = keys::index_prefix(*table, 0);
                let rows_end = keys::prefix_end(&rows);
                end <= rows.as_slice() || start >= rows_end.as_slice()
            });
        }
    } else if toast::is_row_key(key) || toast::is_value_key(key) {
        watched.remove(&key_table(key));
    }
}

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

thread_local! {
    /// `with_txn_spill_bytes`'s value on this thread (0: none set).
    static SPILL_BYTES_HERE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Run `f` with the stores and engines it opens on this thread taking `n` as their default
/// `txn_spill_bytes` (tests). An engine keeps the value it was opened with for every store
/// it opens later, on any thread. (A process-wide default set and reset around an open let
/// tests running at once open with each other's value.)
pub fn with_txn_spill_bytes<R>(n: usize, f: impl FnOnce() -> R) -> R {
    let _g = crate::scoped::set(&SPILL_BYTES_HERE, n, "storage::SPILL_BYTES_HERE");
    f()
}

/// The default `StoreOptions::txn_spill_bytes`.
pub(crate) fn default_spill_bytes() -> usize {
    match SPILL_BYTES_HERE.with(|c| c.get()) {
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

thread_local! {
    /// This thread is reading in bulk (`bulk_read`).
    static BULK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This thread reads what it won't read again (`read_once`).
    static ONCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` reading blocks it won't read again: those not in the block cache are used and
/// dropped, never cached (a one-pass read of the change stream: `repl::decode`).
pub fn read_once<R>(f: impl FnOnce() -> R) -> R {
    struct Back(bool);
    impl Drop for Back {
        fn drop(&mut self) {
            ONCE.with(|b| b.set(self.0));
        }
    }
    let _back = Back(ONCE.with(|b| b.replace(true)));
    f()
}

pub(crate) fn in_read_once() -> bool {
    ONCE.with(|b| b.get())
}

/// Run `f` as a bulk read: a block it reads from a file goes into the block cache only
/// where the cache has room (`BlockCache::insert_if_room`), evicting nothing; once the
/// cache is full, a bulk read's blocks are used and dropped. PostgreSQL reads a big
/// table's sequential scan through a small ring of buffers for the same reason: one pass
/// over it would otherwise evict every other relation's pages.
pub fn bulk_read<R>(f: impl FnOnce() -> R) -> R {
    struct Back(bool);
    impl Drop for Back {
        fn drop(&mut self) {
            BULK.with(|b| b.set(self.0));
        }
    }
    let _back = Back(BULK.with(|b| b.replace(true)));
    f()
}

pub(crate) fn in_bulk_read() -> bool {
    BULK.with(|b| b.get())
}

/// An immutable picture of the store's sources. Readers hold one for their whole scan.
pub struct View {
    mem: Arc<Memtable>,
    /// Newest first.
    imm: Vec<Frozen>,
    l0: Vec<Arc<SstReader>>,
    l1: Vec<Arc<SstReader>>,
    /// The committed range deletions (`rdel`), each hiding what it covers from readers at or
    /// past its ts.
    rdels: Arc<rdel::RangeDels>,
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
    view: crate::published::Published<View>,
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
    /// Commits waiting for another's fsync sleep here (`wait_durable`), woken when
    /// `durable` advances (`made_durable`): they used to queue on `sync_lock`, spinning.
    durable_wait: (Mutex<()>, parking_lot::Condvar),
    next_id: AtomicU64,
    snapshots: Mutex<BTreeMap<u64, usize>>,
    /// The sessions' own pins (`pin_slot`).
    slots: Mutex<Vec<Arc<PinSlot>>>,
    /// The visible timestamp as `gc_horizon` last read it before reading the slots: a
    /// slot's pin of a timestamp before it may have been missed (`pin_visible_in`).
    gc_floor: AtomicU64,
    /// The newest horizon `gc_horizon` found (`horizon_hint`).
    horizon_seen: AtomicU64,
    bg: Mutex<()>,
    bg_running: AtomicBool,
    closed: AtomicBool,
    /// Small ranges scanned whole again and again, kept as version chains (`hot`).
    hot: hot::Hot,
    /// Ranges scans found holding no live row (`dead`).
    dead: Arc<dead::DeadRanges>,
    /// Ranges no one reads any more, left out of flushes and compactions (`discard`).
    discard: discard::Discard,
    /// Tables (by the first 4 bytes of their keys) no commit has written since they were
    /// watched (`watch_untouched`), with the last commit ts assigned then.
    untouched: parking_lot::Mutex<HashMap<u32, u64>>,
    untouched_any: AtomicBool,
    /// The last commit that waits for its WAL fsync before it is visible (an asynchronous
    /// commit after it waits for that fsync too: `commit_async`).
    sync_assigned: AtomicU64,
    /// A WAL writer thread is running (`wal_writer`), and how long it sleeps (ms).
    wal_writer: AtomicBool,
    wal_writer_delay: AtomicU64,
}

/// Keeps versions visible at `ts` from being compacted away.
pub struct SnapshotPin {
    owner: PinOwner,
    pub ts: u64,
}

/// Where a pin is counted: the store's table every session shares, or a session's own
/// slot (`Store::pin_slot`), which no other session writes.
enum PinOwner {
    Store(Arc<Store>),
    Slot(Arc<PinSlot>),
}

impl Drop for SnapshotPin {
    fn drop(&mut self) {
        match &self.owner {
            PinOwner::Store(store) => {
                let mut s = store.snapshots.lock();
                if let Some(n) = s.get_mut(&self.ts) {
                    *n -= 1;
                    if *n == 0 {
                        s.remove(&self.ts);
                    }
                }
            }
            PinOwner::Slot(slot) => slot.remove(self.ts),
        }
    }
}

/// One session's snapshot pins (for the store's versions what PGPROC.xmin is for heap
/// tuples): counted under a mutex of its own, the oldest published in `min` for
/// `Store::gc_horizon`. Taking and dropping one writes only this session's cache lines,
/// where the store's shared table (`snapshots`) took a mutex every session shares. (Made by
/// its session's thread, among that thread's own allocations: no other session's slot shares
/// its cache line, and no line of padding is needed.)
pub struct PinSlot {
    pins: Mutex<Vec<(u64, usize)>>,
    /// The oldest pinned timestamp, u64::MAX for none.
    min: AtomicU64,
}

impl PinSlot {
    /// Count a pin of `ts`; true when it is the slot's oldest now (`min` moved back to it).
    fn add(&self, ts: u64) -> bool {
        let mut p = self.pins.lock();
        match p.iter_mut().find(|e| e.0 == ts) {
            Some(e) => e.1 += 1,
            None => p.push((ts, 1)),
        }
        if ts < self.min.load(Ordering::Relaxed) {
            self.min.store(ts, Ordering::SeqCst);
            return true;
        }
        false
    }

    fn remove(&self, ts: u64) {
        let mut p = self.pins.lock();
        if let Some(i) = p.iter().position(|e| e.0 == ts) {
            p[i].1 -= 1;
            if p[i].1 == 0 {
                p.swap_remove(i);
            }
        }
        let min = p.iter().map(|e| e.0).min().unwrap_or(u64::MAX);
        if min != self.min.load(Ordering::Relaxed) {
            self.min.store(min, Ordering::Release);
        }
    }
}

/// Replace the manifest durably. The directory is fsynced first: the SSTs it names were
/// renamed into it, and a power cut may otherwise keep the manifest's rename without theirs
/// (renames are only ordered by a directory fsync between them; PostgreSQL's durable_rename).
fn write_manifest(dir: &Path, m: &Manifest) -> Result<()> {
    let data = serde_json::to_vec(m).map_err(|e| internal(e.to_string()))?;
    fileio::sync(|| durable::sync_dir(dir))?;
    fileio::sync(|| durable::replace(&dir.join("MANIFEST"), &data, true))?;
    Ok(())
}

impl Store {
    pub fn open(dir: &Path, opts: StoreOptions) -> Result<Arc<Store>> {
        durable::create_dir_all_synced(dir)?;
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
        let _ = durable::remove_dir_all(&dir.join(SPILL_DIR));
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "tmp") {
                let _ = durable::remove_file(&p);
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
                let _ = durable::remove_file(&p);
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
            .chain(manifest.l1.iter().filter(|m| m.commit_ts.is_some()))
            .map(|m| m.max_ts)
            .filter(|&t| t > manifest.flushed_ts)
            .collect();
        barriers.sort_unstable();
        barriers.dedup();
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
        let wal = WalWriter::create(dir, replay_seq, opts.sync)?;
        let store = Arc::new(Store {
            dir: dir.to_path_buf(),
            opts,
            view: crate::published::Published::new(Arc::new(View { mem, imm, l0, l1, rdels: Arc::default() })),
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
            durable_wait: (Mutex::new(()), parking_lot::Condvar::new()),
            next_id: AtomicU64::new(next_id),
            snapshots: Mutex::new(BTreeMap::new()),
            slots: Mutex::new(Vec::new()),
            gc_floor: AtomicU64::new(0),
            horizon_seen: AtomicU64::new(last_ts),
            bg: Mutex::new(()),
            bg_running: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            hot: hot::Hot::default(),
            dead: Arc::default(),
            discard: Default::default(),
            untouched: Default::default(),
            untouched_any: AtomicBool::new(false),
            sync_assigned: AtomicU64::new(0),
            wal_writer: AtomicBool::new(false),
            wal_writer_delay: AtomicU64::new(200),
        });
        if replayed > 0 {
            debug!(dir = %dir.display(), records = replayed, "replayed WAL");
            // Re-log the replayed commits into the fresh WAL, then drop the old files: simplest
            // is to flush them to an SST right away.
            store.rotate()?;
            store.flush_all()?;
        }
        store.load_rdels()?;
        // (L0 at its trigger as it was left, a crash's or a build's before closes compacted
        // it: merged in the background now, not at the first memtable filled.)
        if store.view().l0.len() >= store.opts.l0_compaction_trigger {
            store.schedule_background();
        }
        // Older WAL files are fully covered now.
        for seq in wal_seqs {
            if seq != replay_seq {
                let _ = durable::remove_file(&wal::wal_path(dir, seq));
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

    /// Compression policy for fresh runs that may become store files unchanged.
    pub(crate) fn compress(&self) -> bool {
        self.opts.compress
    }

    /// `StoreOptions::txn_spill_bytes`.
    pub fn txn_spill_bytes(&self) -> usize {
        self.opts.txn_spill_bytes
    }

    /// The newest commit timestamp; a snapshot at this ts sees every committed write.
    pub fn visible_ts(&self) -> u64 {
        self.visible.load(Ordering::Acquire)
    }

    /// The last commit timestamp handed out: every commit that has returned is at or
    /// before it (an asynchronous one possibly not durable yet).
    pub fn assigned_ts(&self) -> u64 {
        self.assigned.load(Ordering::Acquire)
    }

    /// Every commit up to this timestamp is durable (GetFlushRecPtr): what an asynchronous
    /// commit's caller waits for before calling it flushed. Without fsync (`--no-fsync`)
    /// every commit counts as flushed, as PostgreSQL's fsync = off reports its writes.
    pub fn durable_ts(&self) -> u64 {
        if !self.opts.sync {
            return self.assigned.load(Ordering::Acquire);
        }
        self.durable.load(Ordering::Acquire)
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
            owner: PinOwner::Store(self.clone()),
            ts,
        }
    }

    /// A slot for one session's pins (`pin_in`, `pin_visible_in`), which `gc_horizon`
    /// reads for as long as a pin or its session holds it.
    pub fn pin_slot(&self) -> Arc<PinSlot> {
        let slot = Arc::new(PinSlot { pins: Mutex::new(Vec::new()), min: AtomicU64::new(u64::MAX) });
        let mut slots = self.slots.lock();
        // (Slots nothing else holds any more go.)
        slots.retain(|s| Arc::strong_count(s) > 1);
        slots.push(slot.clone());
        drop(slots);
        slot
    }

    /// `pin(ts)` counted in `slot`: `ts` is one another pin holds (no older than the
    /// horizon).
    pub fn pin_in(&self, slot: &Arc<PinSlot>, ts: u64) -> SnapshotPin {
        slot.add(ts);
        SnapshotPin {
            owner: PinOwner::Slot(slot.clone()),
            ts,
        }
    }

    /// `pin_visible` counted in `slot`. A `gc_horizon` running meanwhile either reads the
    /// slot's new oldest pin or has published a floor before it read the slots, which this
    /// then reads: a timestamp below the floor is given up for a newer one. (Both sides'
    /// store and load are sequentially consistent, so one of them sees the other's.)
    pub fn pin_visible_in(&self, slot: &Arc<PinSlot>) -> SnapshotPin {
        loop {
            let ts = self.visible_ts();
            // (When not the slot's oldest, an older pin of the slot holds it already.)
            if !slot.add(ts) || self.gc_floor.load(Ordering::SeqCst) <= ts {
                return SnapshotPin {
                    owner: PinOwner::Slot(slot.clone()),
                    ts,
                };
            }
            slot.remove(ts);
        }
    }

    /// The block cache's size (bytes): memory other caches size themselves by.
    pub fn cache_capacity(&self) -> usize {
        self.opts.cache.capacity_bytes()
    }

    /// A snapshot of every committed write, pinned: the timestamp is read under the lock
    /// `gc_horizon` reads it under, so no pruning or compaction can take what it sees
    /// between the two.
    pub fn pin_visible(self: &Arc<Self>) -> SnapshotPin {
        let mut s = self.snapshots.lock();
        let ts = self.visible_ts();
        *s.entry(ts).or_insert(0) += 1;
        SnapshotPin {
            owner: PinOwner::Store(self.clone()),
            ts,
        }
    }

    /// Oldest timestamp any reader may still need, the sessions' slots' pins counted. (It
    /// never moves back: a pin is taken of a timestamp another pin holds, or of the visible
    /// one, past `gc_floor` in a slot.)
    pub fn gc_horizon(&self) -> u64 {
        let s = self.snapshots.lock();
        let visible = self.visible_ts();
        // (Before the slots are read: `pin_visible_in`.)
        self.gc_floor.fetch_max(visible, Ordering::SeqCst);
        let mut h = s.keys().next().map_or(visible, |t| (*t).min(visible));
        for slot in self.slots.lock().iter() {
            h = h.min(slot.min.load(Ordering::SeqCst));
        }
        drop(s);
        self.horizon_seen.fetch_max(h, Ordering::AcqRel);
        h
    }

    /// A horizon `gc_horizon` found before, without reading the pins: no newer than the
    /// one it would find now, which is what a scan pruning the memtables' versions needs.
    fn horizon_hint(&self) -> u64 {
        self.horizon_seen.load(Ordering::Acquire)
    }

    pub fn view(&self) -> Arc<View> {
        self.view.load()
    }

    /// Whether reading all of [start, end) is a bulk read: its files' blocks over that
    /// range (decompressed, about `BLOCK_TARGET` each) are more than a quarter of the block
    /// cache, PostgreSQL's rule for a sequential scan's ring (GetAccessStrategy's
    /// BAS_BULKREAD for relations over NBuffers / 4). Such a read goes through
    /// `bulk_read`, so a full scan of a big table doesn't push out everything else.
    pub fn bulk_range(&self, start: &[u8], end: &[u8]) -> bool {
        let v = self.view();
        let ssts = v.l0.iter().chain(&v.l1).chain(v.imm.iter().filter_map(|f| match f {
            Frozen::Sst(s) => Some(s),
            Frozen::Mem(_) => None,
        }));
        let mut blocks = 0usize;
        for s in ssts {
            if s.meta.last_key.as_slice() < start || s.meta.first_key.as_slice() >= end {
                continue;
            }
            let (a, b) = (s.find_block(start), s.find_block(end).min(s.block_count().saturating_sub(1)));
            blocks += b.saturating_sub(a) + 1;
        }
        blocks.saturating_mul(sst::BLOCK_TARGET) > self.opts.cache.capacity_bytes() / 4
    }

    /// Watch `table` (the first 4 bytes of its keys): until a commit writes one of its keys
    /// (or links a run whose keys span it), `untouched` answers the
    /// last commit ts assigned now. Under the commit lock, with `f` given that ts and run
    /// there first (the table isn't watched when it says no): every commit up to it is in
    /// the memtables or files by then, so `changed_since` there sees it.
    pub fn watch_untouched(&self, table: u32, f: impl FnOnce(u64) -> bool) -> Option<u64> {
        let _c = self.commit.lock();
        let ts = self.assigned.load(Ordering::Acquire);
        if !f(ts) {
            return None;
        }
        self.untouched.lock().insert(table, ts);
        self.untouched_any.store(true, Ordering::Release);
        Some(ts)
    }

    /// The ts `watch_untouched` gave `table`, while no commit has written it since.
    pub fn untouched(&self, table: u32) -> Option<u64> {
        if !self.untouched_any.load(Ordering::Acquire) {
            return None;
        }
        self.untouched.lock().get(&table).copied()
    }

    /// A commit's writes (under the commit lock): the tables they write aren't untouched.
    fn note_untouched(&self, writes: &[(Vec<u8>, Option<Bytes>)]) {
        if !self.untouched_any.load(Ordering::Acquire) {
            return;
        }
        let mut u = self.untouched.lock();
        for (k, v) in writes {
            unwatch_written(&mut u, k, v.as_deref());
        }
        if u.is_empty() {
            self.untouched_any.store(false, Ordering::Release);
        }
    }

    /// Whether a version of a key in [start, end) may have been committed after `ts`: a
    /// memtable entry there, a file overlapping the range whose newest version is, or a range
    /// deletion over it. Never false when one was (a file overlapping it in part can say true
    /// for another range's versions). For VACUUM's skip of a table nothing changed since its
    /// last measure (`txn::unchanged`).
    pub fn changed_since(&self, start: &[u8], end: &[u8], ts: u64) -> bool {
        let v = self.view();
        if v.rdels.iter().any(|r| r.ts > ts && r.overlaps(start, end)) {
            return true;
        }
        for m in v.memtables() {
            if m.max_ts.load(Ordering::Acquire) > ts
                && m.map.range(start.to_vec()..end.to_vec()).any(|e| sst::key_ts(e.key()) > ts)
            {
                return true;
            }
        }
        let ssts = v.l0.iter().chain(&v.l1).chain(v.imm.iter().filter_map(|f| match f {
            Frozen::Sst(s) => Some(s),
            Frozen::Mem(_) => None,
        }));
        ssts.filter(|s| s.meta.last_key.as_slice() >= start && s.meta.first_key.as_slice() < end)
            .any(|s| s.meta.commit_ts.unwrap_or(s.meta.max_ts) > ts)
    }

    /// The value of the newest version of `key` visible at `ts` (None if absent or deleted).
    pub fn get(&self, key: &[u8], ts: u64) -> Result<Option<Bytes>> {
        Ok(self.get_version(key, ts)?.and_then(|(_, v)| v))
    }

    /// (commit ts, value) of the newest version visible at `ts`; a tombstone has value None.
    /// A row's out-of-line values are put back (`toast`).
    pub fn get_version(&self, key: &[u8], ts: u64) -> Result<Option<(u64, Option<Bytes>)>> {
        let view = self.view();
        match view.get_version(key, ts)? {
            Some((vts, Some(v))) => Ok(Some((vts, Some(toast::resolve(&view, key, v, ts)?)))),
            other => Ok(other),
        }
    }

    /// `get_version` with a row's value as it is stored (its out-of-line values' pointers).
    pub fn get_version_raw(&self, key: &[u8], ts: u64) -> Result<Option<(u64, Option<Bytes>)>> {
        self.view().get_version(key, ts)
    }

    /// Commit ts of the newest version of `key`, committed or deleted (for conflict checks).
    pub fn latest_version_ts(&self, key: &[u8]) -> Result<Option<u64>> {
        Ok(self.get_version_raw(key, u64::MAX >> 1)?.map(|(ts, _)| ts))
    }
}

impl View {
    /// A view of `files` alone, newest first: a transaction's spilled writes
    /// (`txn::spill`), read with the store's own merging and version rules.
    pub fn of_files(files: Vec<Arc<SstReader>>) -> Arc<View> {
        View::of_files_with(files, Vec::new())
    }

    /// `of_files`, with range deletions (a transaction's own, at their journal places).
    pub fn of_files_with(files: Vec<Arc<SstReader>>, rdels: Vec<rdel::RangeDel>) -> Arc<View> {
        Arc::new(View {
            mem: Arc::new(Memtable::new(0)),
            imm: Vec::new(),
            l0: files,
            l1: Vec::new(),
            rdels: Arc::new(rdel::RangeDels::new(rdels)),
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
        let seek = mvcc_key(key, ts);
        let hit = point::newest(self, key, &seek, true, &mut [], &mut sst::BlockPos::default())?;
        Ok(point::deleted_by_range(self, key, hit, ts))
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
        self.get_versions_with(keys, ts, true)
    }

    /// `get_versions` with rows as they are stored (their out-of-line values' pointers).
    pub fn get_versions_raw(&self, keys: &[&[u8]], ts: u64) -> Result<Vec<Option<(u64, Option<Bytes>)>>> {
        self.get_versions_with(keys, ts, false)
    }

    /// The rows visible at `ts` under `keys` (strictly ascending) into `batch`, those that
    /// exist, in their order, by reference (`Scan::fetch_sorted`); with their out-of-line
    /// values put back when `resolve`.
    pub fn get_sorted_into<K: AsRef<[u8]>>(&self, keys: &[K], ts: u64, resolve: bool, batch: &mut batch::RowBatch) -> Result<()> {
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
            return Ok(());
        };
        // (Past every version of the last key: storage keys end in their version.)
        let end = [last.as_ref(), &[0xFF; 9]].concat();
        let mut s = Scan::new(self.view(), first.as_ref().to_vec(), end, ts, self.horizon_hint());
        s.resolve = resolve;
        s.fetch_sorted(keys, batch)
    }

    fn get_versions_with(&self, keys: &[&[u8]], ts: u64, resolve: bool) -> Result<Vec<Option<(u64, Option<Bytes>)>>> {
        debug_assert!(keys.windows(2).all(|w| w[0] < w[1]));
        // (The keys are rows an index or a bitmap named: the last level holds the ones no
        // newer source had, so its bloom filters would only cost.)
        let mut r = self.point_reader(ts, false);
        keys.iter().map(|k| self.get_with(&mut r, k, resolve)).collect()
    }

    /// A reader of single keys at `ts` (`point::PointReader`: blocks kept from key to key),
    /// over the sources as they are now: made once the snapshot's commits are visible.
    /// `bloom_last`: keys that may not exist (the last level's bloom filters checked too).
    pub fn point_reader(&self, ts: u64, bloom_last: bool) -> point::PointReader {
        point::PointReader::new(self.view(), ts, bloom_last)
    }

    /// `get_version` of `key` through `r` (at its snapshot), the hot ranges first; with a
    /// row's out-of-line values put back when `resolve`.
    pub fn get_with(&self, r: &mut point::PointReader, key: &[u8], resolve: bool) -> Result<Option<(u64, Option<Bytes>)>> {
        let ts = r.ts();
        let hit = match self.hot.get(key, ts) {
            Some(hit) => hit,
            None => r.get_raw(key)?,
        };
        match hit {
            Some((vts, Some(v))) if resolve && toast::has_external(&v) => {
                Ok(Some((vts, Some(toast::resolve(r.view(), key, v, ts)?))))
            }
            other => Ok(other),
        }
    }

    /// The first row visible at `r`'s snapshot with a user key in [start, end) (an empty
    /// `end`: unbounded): an equality lookup's row (a unique index's, a primary key's), read
    /// without a merging scan; the hot ranges first. With its out-of-line values put back
    /// when `resolve`. `First::Unknown`: scan the range instead.
    pub fn first_with(&self, r: &mut point::PointReader, start: &[u8], end: &[u8], resolve: bool) -> Result<point::First> {
        let ts = r.ts();
        let first = match self.hot.scan(start, end, ts) {
            Some(rows) => match rows.into_iter().next() {
                Some((k, v)) => point::First::Row(k.to_vec(), v),
                None => point::First::None,
            },
            None => r.first_raw(start, end)?,
        };
        Ok(match first {
            point::First::Row(k, v) if resolve && toast::has_external(&v) => {
                let v = toast::resolve(r.view(), &k, v, ts)?;
                point::First::Row(k, v)
            }
            other => other,
        })
    }

    /// `first_with` (values as stored) for a lookup a scan would otherwise make: the hot
    /// ranges as `scan_raw` reads them, the probe counted toward keeping its range hot as a
    /// scan is.
    pub fn first_noted(&self, r: &mut point::PointReader, start: &[u8], end: &[u8]) -> Result<point::First> {
        let first = |rows: Vec<(Bytes, Bytes)>| match rows.into_iter().next() {
            Some((k, v)) => point::First::Row(k.to_vec(), v),
            None => point::First::None,
        };
        if let Some(rows) = self.hot.scan(start, end, r.ts()) {
            return Ok(first(rows));
        }
        if let Some(p) = hot::covering(start, end)
            && !self.discard.claims(&p)
            && self.hot.note_scan(p)
            // (Kept from now on; a failure to read it leaves it to the probe.)
            && self.keep_hot(p).is_ok()
            && let Some(rows) = self.hot.scan(start, end, r.ts())
        {
            return Ok(first(rows));
        }
        r.first_raw(start, end)
    }

    /// Ascending scan of user keys in [start, end) at snapshot `ts`, rows with their
    /// out-of-line values put back (`toast`).
    pub fn scan(&self, start: &[u8], end: &[u8], ts: u64) -> Scan {
        let mut s = self.scan_raw(start, end, ts);
        s.resolve = true;
        s
    }

    /// `scan` with rows as they are stored (their out-of-line values' pointers).
    pub fn scan_raw(&self, start: &[u8], end: &[u8], ts: u64) -> Scan {
        if let Some(rows) = self.hot.scan(start, end, ts) {
            return Scan::of_rows(self.view(), rows, ts);
        }
        if let Some(p) = hot::covering(start, end)
            && !self.discard.claims(&p)
            && self.hot.note_scan(p)
        {
            // (Kept from now on; a failure to read it leaves it to the merging scan.)
            if self.keep_hot(p).is_ok()
                && let Some(rows) = self.hot.scan(start, end, ts)
            {
                return Scan::of_rows(self.view(), rows, ts);
            }
        }
        // (A run of deleted keys a scan found before at a snapshot at or before this one,
        // nothing written into it since, is stepped over: `dead`.)
        let start = match self.dead.skip(start, ts) {
            Some(live) if !end.is_empty() && live.as_slice() >= end => return Scan::of_rows(self.view(), Vec::new(), ts),
            Some(live) => live,
            None => start.to_vec(),
        };
        let mut s = Scan::new(self.view(), start.clone(), end.to_vec(), ts, self.horizon_hint());
        s.dead = Some((self.dead.clone(), start));
        s
    }

    /// Keep relation range `p` as version chains (`hot`), read from the sources under the
    /// commit lock. (Not waited for long: a scan under the commit lock, or a busy one,
    /// leaves it to a later scan.)
    fn keep_hot(&self, p: hot::Prefix) -> Result<()> {
        let Some(_c) = self.commit.try_lock_for(std::time::Duration::from_millis(2)) else {
            return Err(internal("commit lock busy"));
        };
        let horizon = self.gc_horizon();
        let end = keys::prefix_end(&p);
        let view = self.view();
        // (Not while a range deletion covers some of it: chains don't apply one.)
        if view.rdels.iter().any(|r| r.overlaps(&p, &end)) {
            return Err(internal("a range deletion over the range"));
        }
        let mut merge = Merge::new(sources(&view, &p, &end), end);
        let mut entries = std::iter::from_fn(|| match merge.min() {
            Ok(Some(i)) => Some(Ok(merge.sources[i].take())),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        });
        self.hot.build(p, &mut entries, horizon)
    }

    /// A commit's writes at `ts` into the kept hot ranges (under the commit lock), the
    /// versions their chains drop pruned from the memtables of `view` as well.
    fn apply_hot(&self, ts: u64, writes: &[(Vec<u8>, Option<Bytes>)], view: &View) {
        // (The commit's span, before its writes go in: `dead`.)
        self.dead.note(ts, Some(&mut writes.iter().map(|w| w.0.as_slice())));
        self.hot.apply(ts, writes, || self.gc_horizon(), |k, t| {
            let key = mvcc_key(k, t);
            for m in view.memtables() {
                m.map.remove(key.as_slice());
            }
        });
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
            // Another commit is fsyncing: sleep until it is done (it may cover this one),
            // then sync if it didn't.
            let Some(_g) = self.sync_lock.try_lock() else {
                let mut w = self.durable_wait.0.lock();
                while self.durable.load(Ordering::Acquire) < ts && self.sync_lock.is_locked() {
                    // (A bound on the sleep: a store closed meanwhile wakes no one.)
                    self.durable_wait.1.wait_for(&mut w, std::time::Duration::from_millis(50));
                }
                continue;
            };
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
                drop(_g);
                self.wake_durable();
                return Err(e.into());
            }
            self.durable.fetch_max(upto, Ordering::AcqRel);
            self.visible.fetch_max(upto, Ordering::AcqRel);
            // (Woken once the fsync is over: a commit appended after `upto` syncs next.)
            drop(_g);
            self.wake_durable();
            self.refresh_horizon_hint();
        }
        Ok(())
    }

    /// Every commit up to `upto` is fsynced: it is visible.
    fn made_durable(&self, upto: u64) {
        self.durable.fetch_max(upto, Ordering::AcqRel);
        self.visible.fetch_max(upto, Ordering::AcqRel);
        self.wake_durable();
        self.refresh_horizon_hint();
    }

    /// `horizon_hint` brought up to date, as commits made visible move it on: once per
    /// group of commits fsynced together, not per scan.
    fn refresh_horizon_hint(&self) {
        self.gc_horizon();
    }

    /// Wake the commits sleeping in `wait_durable` (`durable` advanced, or the fsync ended).
    fn wake_durable(&self) {
        let _w = self.durable_wait.0.lock();
        self.durable_wait.1.notify_all();
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
        let ranged;
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
            self.sync_assigned.store(ts, Ordering::Release);
            ranged = self.note_rdels(writes.iter().map(|(k, v)| (k.as_slice(), v.as_deref())), ts);
            let view = self.view.load();
            self.apply_hot(ts, &writes, &view);
            self.note_untouched(&writes);
            let mem = view.mem.clone();
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
        }
        if rotate || ranged {
            self.schedule_background();
        }
        Ok(ts)
    }

    /// `commit` for `synchronous_commit = off` (PostgreSQL's asynchronous commit): the
    /// writes go into the WAL and the memtable under the commit lock as `commit_grouped`'s
    /// do, and are visible (and this returns) without waiting for the WAL fsync. The WAL
    /// writer fsyncs them within `wal_writer_delay_ms` (PostgreSQL: wal_writer_delay, at
    /// most three times it); a later synchronous commit's fsync covers them too. A crash
    /// before that loses them, and only a suffix of commits: the WAL is replayed in commit
    /// order up to its last whole record. (A synchronous commit before it still waiting for
    /// its fsync is waited for: commits become visible in timestamp order, and that one
    /// mustn't be seen before it is durable.)
    pub fn commit_async(
        self: &Arc<Self>,
        mut writes: Vec<(Vec<u8>, Option<Bytes>)>,
        check: impl FnOnce(&Store, &mut Vec<(Vec<u8>, Option<Bytes>)>) -> Result<()>,
        wal_writer_delay_ms: u64,
    ) -> Result<u64> {
        if !self.opts.sync || self.ingests(&writes) {
            return self.commit_with(writes, check, |_| {});
        }
        let (ts, before, rotate);
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
            before = self.sync_assigned.load(Ordering::Acquire);
            let view = self.view.load();
            self.apply_hot(ts, &writes, &view);
            self.note_untouched(&writes);
            let mem = view.mem.clone();
            for (k, v) in writes {
                mem.insert(mvcc_key(&k, ts), v, ts);
            }
            rotate = mem.bytes() >= self.opts.memtable_bytes;
        }
        if self.durable.load(Ordering::Acquire) < before {
            self.wait_durable(before)?;
        }
        self.visible.fetch_max(ts, Ordering::AcqRel);
        self.wal_writer_delay.store(wal_writer_delay_ms.max(1), Ordering::Relaxed);
        self.wal_writer();
        if rotate {
            self.rotate()?;
            self.schedule_background();
        }
        Ok(ts)
    }

    /// Make sure the WAL writer runs: a thread that fsyncs the WAL every
    /// `wal_writer_delay` while asynchronous commits wait for it, and ends when none do.
    fn wal_writer(self: &Arc<Self>) {
        if self.wal_writer.swap(true, Ordering::AcqRel) {
            return;
        }
        let store = Arc::downgrade(self);
        let spawned = std::thread::Builder::new().name("roll-walwriter".into()).spawn(move || {
            loop {
                let delay = match store.upgrade() {
                    Some(s) => s.wal_writer_delay.load(Ordering::Relaxed),
                    None => return,
                };
                std::thread::sleep(std::time::Duration::from_millis(delay));
                let Some(s) = store.upgrade() else { return };
                if s.closed.load(Ordering::Acquire) {
                    s.wal_writer.store(false, Ordering::Release);
                    return;
                }
                let upto = s.assigned.load(Ordering::Acquire);
                if s.durable.load(Ordering::Acquire) < upto {
                    // (A failed fsync closes the store: `wait_durable`.)
                    let _ = s.wait_durable(upto);
                    continue;
                }
                // Nothing to write: done, unless a commit came in meanwhile (it found the
                // flag still set and left the writing to this thread).
                s.wal_writer.store(false, Ordering::Release);
                if s.durable.load(Ordering::Acquire) >= s.assigned.load(Ordering::Acquire)
                    || s.wal_writer.swap(true, Ordering::AcqRel)
                {
                    return;
                }
            }
        });
        if spawned.is_err() {
            // (No thread: the commit waits for its own fsync instead.)
            self.wal_writer.store(false, Ordering::Release);
            let _ = self.wait_durable(self.assigned.load(Ordering::Acquire));
        }
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
        let mut ranged = false;
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
                self.note_untouched(&writes);
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
                ranged = self.note_rdels(writes.iter().map(|(k, v)| (k.as_slice(), v.as_deref())), ts);
                let view = self.view.load();
                self.apply_hot(ts, &writes, &view);
                self.note_untouched(&writes);
                let mem = view.mem.clone();
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
        if rotate || ingest || ranged {
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
        linked: Vec<Arc<SstReader>>,
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
            self.ingest_stream_locked(&mut c, ts, keys, writes, extra, each, linked)?;
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
        self.ingest_stream_locked(c, ts, n, &mut writes.into_iter().map(Ok), Vec::new(), &|_, _| Ok(()), Vec::new())
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
        linked: Vec<Arc<SstReader>>,
    ) -> Result<()> {
        // (Its writes don't go through `apply_hot`.)
        self.hot.clear();
        self.dead.note(ts, None);
        // (The tables it writes aren't untouched: its keys', and those its linked runs span.)
        let mut untouched = self.untouched_any.load(Ordering::Acquire).then(|| self.untouched.lock());
        if let Some(u) = untouched.as_mut() {
            for f in &linked {
                let (lo, hi) = (key_table(&f.meta.first_key), key_table(&f.meta.last_key));
                // (A run of one table's index entries, an index build's, leaves its rows and
                // out-of-line values alone.)
                let (first, last) = (&f.meta.first_key[..], &f.meta.last_key[..]);
                if lo == hi && first >= &keys::index_prefix(lo, 1)[..] && last < &toast::prefix(lo)[..] {
                    continue;
                }
                u.retain(|t, _| *t < lo || *t > hi);
            }
        }
        self.rotate_locked(c)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // (Uncompressed: the commit waits for this file, and compaction, in the background,
        // compresses it when it moves to L1.)
        let mut w = SstWriter::create(&self.dir, id, false)?;
        w.expect_keys(keys + extra.len());
        let mut key = Vec::new();
        let mut markers: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        let mut add = |w: &mut SstWriter, k: &[u8], v: Option<&[u8]>| {
            if rdel::is_marker(k) {
                markers.push((k.to_vec(), v.map(<[u8]>::to_vec)));
            }
            if let Some(u) = untouched.as_mut() {
                unwatch_written(u, k, v);
            }
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
        if let Some(u) = untouched.take()
            && u.is_empty()
        {
            self.untouched_any.store(false, Ordering::Release);
        }
        let mut files: Vec<Arc<SstReader>> = Vec::new();
        if w.entries() == 0 {
            w.abandon();
            if linked.is_empty() {
                // (Nothing to write after all: the commit takes its timestamp alone.)
                self.assigned.store(ts, Ordering::Release);
                self.made_durable(ts);
                return Ok(());
            }
        } else {
            let meta = w.finish()?;
            match SstReader::open(&self.dir, meta, self.opts.cache.clone()) {
                Ok(r) => files.push(Arc::new(r)),
                Err(e) => {
                    let _ = durable::remove_file(&sst::sst_path(&self.dir, id));
                    return Err(e);
                }
            }
        }
        // (On an error, files already made go with their readers.)
        let fail = |files: &[Arc<SstReader>], e| {
            files.iter().for_each(|f| f.mark_obsolete());
            Err(e)
        };
        for run in &linked {
            match self.link_run(run, ts) {
                Ok(r) => files.push(r),
                Err(e) => return fail(&files, e),
            }
        }
        // (Linked runs no other source holds keys of go to L1 at once: written once, never
        // compacted. L1 changes under `bg` only; when busy, they go to L0 as the others.)
        let bg = if linked.is_empty() { None } else { self.bg.try_lock() };
        {
            let mut m = self.manifest.lock();
            let mut next = m.clone();
            let cur = self.view.load();
            let mut bottom = Vec::with_capacity(files.len());
            for i in 0..files.len() {
                bottom.push(match bg.is_some() {
                    true => match self.alone_at_bottom(&cur, &files, i) {
                        Ok(b) => b,
                        Err(e) => return fail(&files, e),
                    },
                    false => false,
                });
            }
            for (f, b) in files.iter().zip(&bottom) {
                if *b {
                    let at = next.l1.partition_point(|s| s.last_key < f.meta.first_key);
                    next.l1.insert(at, f.meta.clone());
                } else {
                    next.l0.insert(l0_place(&next.l0, &f.meta), f.meta.clone());
                }
            }
            next.last_ts = ts;
            next.next_id = self.next_id.load(Ordering::Relaxed);
            if let Err(e) = write_manifest(&self.dir, &next) {
                return fail(&files, e);
            }
            *m = next;
            // (Handed out before its versions can be read: `last_assigned_ts`.)
            self.assigned.store(ts, Ordering::Release);
            let mut v = self.view.write();
            let mut l1 = v.l1.clone();
            let mut imm: Vec<Frozen> = Vec::new();
            for (f, b) in files.into_iter().zip(bottom) {
                if b {
                    let at = l1.partition_point(|s| s.meta.last_key < f.meta.first_key);
                    l1.insert(at, f);
                } else {
                    imm.push(Frozen::Sst(f));
                }
            }
            imm.extend(v.imm.iter().cloned());
            let rdels = rdel::noted(&v.rdels, markers.iter().map(|(k, v)| (k.as_slice(), v.as_deref())), ts)
                .map_or_else(|| v.rdels.clone(), Arc::new);
            *v = Arc::new(View {
                mem: v.mem.clone(),
                imm,
                l0: v.l0.clone(),
                l1,
                rdels,
            });
        }
        drop(bg);
        self.made_durable(ts);
        Ok(())
    }

    /// Whether `files[i]`, a commit's linked run, can go to L1 as it is: meeting the store's compression
    /// policy, not much bigger than compaction's files, and no other source of `view` nor other file
    /// of the commit holds a key in its range (so every older version of its keys, none,
    /// is below it, and a key's versions in L1 stay in one file). Under the commit lock and
    /// `bg`.
    fn alone_at_bottom(&self, view: &View, files: &[Arc<SstReader>], i: usize) -> Result<bool> {
        let f = &files[i];
        if f.meta.commit_ts.is_none() || f.meta.size > 2 * self.opts.target_file_bytes || (self.opts.compress && !f.first_block_compressed()?) {
            return Ok(false);
        }
        let (lo, hi) = (user_key(&f.meta.first_key), user_key(&f.meta.last_key));
        let meets = |s: &SstReader| user_key(&s.meta.last_key) >= lo && user_key(&s.meta.first_key) <= hi;
        let frozen = view.imm.iter().filter_map(|x| match x {
            Frozen::Sst(s) => Some(s),
            Frozen::Mem(_) => None,
        });
        if files.iter().enumerate().any(|(j, g)| j != i && meets(g))
            || frozen.chain(&view.l0).chain(&view.l1).any(|s| meets(s))
        {
            return Ok(false);
        }
        Ok(!view.memtables().any(|m| {
            m.map
                .range::<[u8], _>((Bound::Included(lo), Bound::Unbounded))
                .next()
                .is_some_and(|e| user_key(e.key()) <= hi)
        }))
    }

    /// A transaction's spilled run (`txn::spill`) made a file of its commit at `ts`: moved
    /// into the store's directory under a new id and fsynced (runs aren't), read as
    /// versions at `ts` (`SstMeta::commit_ts`) once the manifest lists it.
    fn link_run(&self, run: &SstReader, ts: u64) -> Result<Arc<SstReader>> {
        durable::sync_path(run.path())?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = sst::sst_path(&self.dir, id);
        durable::rename(run.path(), &path)?;
        let mut meta = run.meta.clone();
        meta.id = id;
        sst::set_ts(&mut meta.first_key, ts);
        sst::set_ts(&mut meta.last_key, ts);
        meta.min_ts = ts;
        meta.max_ts = ts;
        meta.commit_ts = Some(ts);
        match SstReader::open(&self.dir, meta, self.opts.cache.clone()) {
            Ok(r) => {
                LINKED_RUNS.fetch_add(1, Ordering::Relaxed);
                Ok(Arc::new(r))
            }
            Err(e) => {
                let _ = durable::remove_file(&path);
                Err(e)
            }
        }
    }

    /// Take the range deletions (`rdel`) among a commit's writes at `ts` into the view
    /// (under the commit lock, before the commit is visible). Whether there were any.
    fn note_rdels<'a>(&self, writes: impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)>, ts: u64) -> bool {
        // (Only commits change the list, and they hold the commit lock.)
        let cur = self.view.with(|v| v.rdels.clone());
        let Some(list) = rdel::noted(&cur, writes, ts) else {
            return false;
        };
        // (The hot ranges' chains know nothing of range deletions: rebuilt when scanned
        // again, once no deletion covers them, `keep_hot`.)
        self.hot.clear();
        let mut v = self.view.write();
        *v = Arc::new(View {
            mem: v.mem.clone(),
            imm: v.imm.clone(),
            l0: v.l0.clone(),
            l1: v.l1.clone(),
            rdels: Arc::new(list),
        });
        true
    }

    /// The range deletions whose markers are stored (`Store::open`): each at its newest
    /// version, which hides everything an older one did from every reader there is now.
    fn load_rdels(&self) -> Result<()> {
        let ts = self.visible_ts();
        let end = keys::prefix_end(rdel::PREFIX);
        let mut list = Vec::new();
        for e in self.scan(rdel::PREFIX, &end, ts) {
            let (k, v) = e?;
            let at = self.get_version(&k, ts)?.map_or(0, |x| x.0);
            list.push(rdel::RangeDel {
                start: k[rdel::PREFIX.len()..].to_vec(),
                end: v.to_vec(),
                ts: at,
            });
        }
        if !list.is_empty() {
            let mut v = self.view.write();
            *v = Arc::new(View {
                mem: v.mem.clone(),
                imm: v.imm.clone(),
                l0: v.l0.clone(),
                l1: v.l1.clone(),
                rdels: Arc::new(rdel::RangeDels::new(list)),
            });
        }
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
        let new_wal = WalWriter::create(&self.dir, seq, self.opts.sync)?;
        fileio::wal_added();
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
                rdels: v.rdels.clone(),
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
            rdels: v.rdels.clone(),
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
                    let r = this
                        .flush_all()
                        .and_then(|_| this.maybe_compact())
                        .and_then(|_| this.collect_ranges());
                    this.bg_running.store(false, Ordering::Release);
                    if let Err(e) = r {
                        warn!(dir = %this.dir.display(), error = %e, "background flush/compaction failed");
                        break;
                    }
                    // (Sources frozen while it ran, which their scheduling left to it.)
                    if this.view.with(|v| v.imm.is_empty())
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
            let oldest = self.view.with(|v| v.imm.last().cloned());
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
                        rdels: v.rdels.clone(),
                    });
                    continue;
                }
            };
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let mut w = SstWriter::create(&self.dir, id, self.opts.compress)?;
            let mut discarded = self.discard.filter();
            for e in mem.map.iter() {
                if !discarded.drops(user_key(e.key())) {
                    w.add(e.key(), e.value().as_deref())?;
                }
            }
            // (Every entry discarded: no file.)
            let reader = if w.entries() > 0 {
                let meta = w.finish()?;
                Some(Arc::new(SstReader::open(&self.dir, meta, self.opts.cache.clone())?))
            } else {
                w.abandon();
                None
            };
            let flushed_ts = mem.max_ts.load(Ordering::Relaxed);
            {
                let mut m = self.manifest.lock();
                if let Some(r) = &reader {
                    let at = l0_place(&m.l0, &r.meta);
                    m.l0.insert(at, r.meta.clone());
                }
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
                if let Some(reader) = reader {
                    let at = l0.partition_point(|x| x.meta.max_ts > reader.meta.max_ts);
                    l0.insert(at, reader);
                }
                *v = Arc::new(View {
                    mem: v.mem.clone(),
                    imm,
                    l0,
                    l1: v.l1.clone(),
                    rdels: v.rdels.clone(),
                });
            }
            if durable::remove_file(&wal::wal_path(&self.dir, mem.wal_seq)).is_ok() {
                fileio::wal_removed();
            }
            debug!(dir = %self.dir.display(), id, "flushed memtable");
        }
    }

    /// From now on nobody reads [start, end) (the range of `start` only grows): flushes and
    /// compactions leave its entries out (`discard`).
    pub fn discard(&self, start: &[u8], end: Vec<u8>) {
        self.discard.raise(start, end);
    }

    /// The discarded ranges and the files (L0, L1), for tests.
    #[cfg(test)]
    pub(crate) fn discarded(&self) -> (Vec<(Vec<u8>, Vec<u8>)>, usize, usize) {
        let v = self.view();
        (self.discard.ranges(), v.l0.len(), v.l1.len())
    }

    /// Freeze the memtable and write it, with every frozen one, to L0 (benches and tests
    /// that want the data in files).
    pub fn flush_memtables(&self) -> Result<()> {
        self.rotate()?;
        self.flush_all()
    }

    /// Merge L0 into L1 once L0 has enough files, dropping versions no snapshot can see.
    pub fn maybe_compact(&self) -> Result<()> {
        if self.view.with(|v| v.l0.len()) < self.opts.l0_compaction_trigger {
            return Ok(());
        }
        self.compact()
    }

    /// Merge L0 into L1: every L0 file with the L1 files whose user keys overlap one of theirs
    /// (the others stay as they are, also between L0 files' ranges: a new index's entries and
    /// a catalog change don't rewrite the table loaded between them, nor drop its cached
    /// blocks; the outputs end before each such file, so L1's files still don't overlap).
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
        let meets_l0 = |s: &SstReader| {
            let (a, b) = (user_key(&s.meta.first_key), user_key(&s.meta.last_key));
            inputs_l0.iter().any(|l| b >= user_key(&l.meta.first_key) && a <= user_key(&l.meta.last_key))
        };
        let (inputs_l1, kept): (Vec<Arc<SstReader>>, Vec<Arc<SstReader>>) =
            view.l1[first..last].iter().cloned().partition(|s| meets_l0(s));
        // (The next L1 file left in place that the outputs reach: one ends before it.)
        let mut next_kept = 0;
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
        let mut discarded = self.discard.filter();
        while let Some(i) = merge.min()? {
            let (k, v) = merge.sources[i].head().expect("a head entry");
            let uk = user_key(k);
            let ts = key_ts(k);
            let first_version = !started || cur_user.as_slice() != uk;
            if first_version {
                started = true;
                cur_user.clear();
                cur_user.extend_from_slice(uk);
                kept_old = false;
            }
            // (Discarded: no one reads it any more.)
            if discarded.drops(uk) {
                merge.sources[i].advance();
                continue;
            }
            // (Deleted with its range before anyone still reading: its older versions too.)
            if !view.rdels.is_empty() && view.rdels.covering(uk, ts, horizon).is_some() {
                merge.sources[i].advance();
                continue;
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
            // Only split between user keys: a key's versions all in one L1 file, as point
            // reads look for them (`point::newest` reads the one file whose range has the key).
            // (Past a file left in place: no key of an input is in its range, so `uk` is past
            // all of it.)
            let mut past_kept = false;
            while first_version && next_kept < kept.len() && user_key(&kept[next_kept].meta.first_key) < uk {
                past_kept = true;
                next_kept += 1;
            }
            if first_version
                && writer
                    .as_ref()
                    .is_some_and(|w| past_kept || w.approx_size() >= self.opts.target_file_bytes)
            {
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
        let mut readers = outputs
            .iter()
            .map(|m| SstReader::open(&self.dir, m.clone(), self.opts.cache.clone()).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        // (In key order with the files left in place between them.)
        readers.extend(kept);
        readers.sort_by(|a, b| a.meta.first_key.cmp(&b.meta.first_key));
        {
            let mut m = self.manifest.lock();
            let input_ids: std::collections::HashSet<u64> = inputs_l0
                .iter()
                .chain(inputs_l1.iter())
                .map(|s| s.meta.id)
                .collect();
            m.l0.retain(|s| !input_ids.contains(&s.id));
            let mut l1: Vec<Arc<SstReader>> = view.l1[..first].to_vec();
            l1.extend(readers);
            l1.extend(view.l1[last..].iter().cloned());
            // (Only compaction changes L1, under `bg`: the view's is the manifest's.)
            m.l1 = l1.iter().map(|s| s.meta.clone()).collect();
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
                l1,
                rdels: v.rdels.clone(),
            });
        }
        for s in inputs_l0.iter().chain(inputs_l1.iter()) {
            s.mark_obsolete();
        }
        info!(dir = %self.dir.display(), files = outputs.len(), "compacted");
        Ok(())
    }

    /// Rewrite linked L1 files individually on clean close. Each replacement is
    /// durable before publication; unrelated L1 files and concurrent frozen L0
    /// sources keep their identities. Only one replacement file is added at a time.
    fn rewrite_linked_l1_for_close(&self) -> Result<()> {
        let _bg = self.bg.lock();
        loop {
            let Some(f) = self.view().l1.iter().find(|s| s.meta.commit_ts.is_some()).cloned() else {
                return Ok(());
            };
            let outputs = self.rewrite_without(&f, &[], 0)?;
            let mut m = self.manifest.lock();
            let mut v = self.view.write();
            let mut l1 = Vec::with_capacity(v.l1.len());
            for s in &v.l1 {
                if s.meta.id == f.meta.id {
                    l1.extend(outputs.iter().cloned());
                } else {
                    l1.push(s.clone());
                }
            }
            let mut next = m.clone();
            next.l1 = l1.iter().map(|s| s.meta.clone()).collect();
            next.next_id = self.next_id.load(Ordering::Relaxed);
            write_manifest(&self.dir, &next)?;
            *m = next;
            *v = Arc::new(View {
                mem: v.mem.clone(), imm: v.imm.clone(), l0: v.l0.clone(), l1, rdels: v.rdels.clone(),
            });
            f.mark_obsolete();
        }
    }

    /// Flush everything to SSTs and stop accepting commits. After this the store holds no
    /// memory beyond what the caller keeps alive.
    pub fn close(self: &Arc<Self>) -> Result<()> {
        // Range deletions still pending are applied first (compacted, collected, their
        // markers retired), and runs linked into commits rewritten (compacted): a build from
        // before them reading the files after a clean shutdown would see deleted rows again,
        // or misread a linked run's versions.
        let linked = |v: &View| v.l0.iter().chain(&v.l1).any(|s| s.meta.commit_ts.is_some()) || v.imm.iter().any(|f| matches!(f, Frozen::Sst(s) if s.meta.commit_ts.is_some()));
        // (And L0 at its compaction trigger: only a memtable filled up runs compaction, so a
        // session of a few writes left its close's file to the next, and every restart added
        // one that nothing merged: 413 after 400 restarts, each in every point read.)
        let l0_full = |v: &View| v.l0.len() + v.imm.len() >= self.opts.l0_compaction_trigger;
        let pending = {
            let v = self.view();
            !v.rdels.is_empty() || linked(&v) || l0_full(&v)
        };
        if !self.closed.load(Ordering::Acquire) && pending {
            self.rotate()?;
            self.flush_all()?;
            self.compact()?;
            self.collect_ranges()?;
            self.rewrite_linked_l1_for_close()?;
        }
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
        pos: sst::BlockPos,
        end: Vec<u8>,
        start: Vec<u8>,
        started: bool,
        /// Blocks read from the files go into the block cache (not for compaction).
        fill: bool,
        /// Every key of `block` is below `end` (no comparison per entry).
        whole: bool,
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
            pos: sst::BlockPos::default(),
            end: end.to_vec(),
            start: start.to_vec(),
            started: false,
            fill: true,
            whole: false,
        }
    }

    /// The same source over [start, end) instead: a memtable's read again from `start`; a
    /// level's kept in its block when `start` falls in it past its first entry (the next of
    /// ascending lookups), else positioned afresh.
    fn reseek(&mut self, start: &[u8], end: &[u8]) {
        match self {
            Cursor::Mem { buf, from, end: e, done, batch, .. } => {
                buf.clear();
                *from = Bound::Included(start.to_vec());
                e.clear();
                e.extend_from_slice(end);
                *done = false;
                *batch = MIN_BATCH;
            }
            Cursor::Level { ssts, file, block, block_idx, pos, end: e, start: s, started, whole, .. } => {
                e.clear();
                e.extend_from_slice(end);
                // (`start` in the block, past its first entry: the first entry at or past it
                // is here, not in the block before. At the block's first entry the one before
                // could hold it: positioned afresh.)
                if *started
                    && *file < ssts.len()
                    && let Some(b) = block.as_ref()
                    && b.last_key().is_some_and(|k| k >= start)
                {
                    b.seek_on(start, pos);
                    if pos.index() > 0 {
                        *whole = end.is_empty() || b.last_key().is_none_or(|k| k < end);
                        return;
                    }
                }
                s.clear();
                s.extend_from_slice(start);
                *started = false;
                *block = None;
                *file = 0;
                *block_idx = 0;
            }
        }
    }

    /// The files a level cursor reads (None: a memtable's).
    fn files(&self) -> Option<&[Arc<SstReader>]> {
        match self {
            Cursor::Level { ssts, .. } => Some(ssts),
            Cursor::Mem { .. } => None,
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
                end,
                start,
                started,
                fill,
                whole,
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
                            b.seek(start, pos);
                            *whole = end.is_empty() || b.last_key().is_none_or(|k| k < end.as_slice());
                            *block = Some(b);
                        }
                    }
                }
                let sst = &ssts[*file];
                if block.is_none() {
                    if *block_idx >= sst.block_count() {
                        *file += 1;
                        *block_idx = 0;
                        continue;
                    }
                    let b = sst.block_with(*block_idx, *fill)?;
                    b.at(0, pos);
                    *whole = end.is_empty() || b.last_key().is_none_or(|k| k < end.as_slice());
                    *block = Some(b);
                }
                if pos.index() >= block.as_ref().unwrap().len() {
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
                whole,
                ..
            } => {
                if *file >= ssts.len() {
                    return None;
                }
                let b = block.as_ref()?;
                if pos.index() >= b.len() {
                    return None;
                }
                let k = pos.key();
                (*whole || end.is_empty() || k < end.as_slice()).then(|| (k, b.value_ref(pos)))
            }
        }
    }

    /// The head entry, owned, and on past it.
    fn take(&mut self) -> RawEntry {
        match self {
            Cursor::Mem { buf, .. } => buf.pop_front().expect("a head entry"),
            Cursor::Level { block, pos, .. } => {
                let b = block.as_ref().expect("a head entry");
                let e = (pos.key().to_vec(), b.value(pos));
                b.step(pos);
                e
            }
        }
    }

    /// The head entry (not a tombstone) into `batch` under its first `ulen` key bytes (the
    /// user key), by reference; the cursor stays on it.
    fn push_head(&self, ulen: usize, batch: &mut batch::RowBatch) {
        match self {
            Cursor::Mem { buf, .. } => {
                let (k, v) = buf.front().expect("a head entry");
                batch.push(&k[..ulen], v.clone().expect("a value"));
            }
            Cursor::Level { block, pos, .. } => {
                let b = block.as_ref().expect("a head entry");
                b.push_value(pos, &pos.key()[..ulen], batch);
            }
        }
    }

    /// Entries of the current block from the head on into `batch`, under their user keys,
    /// while each is what `Scan::position` would hand out next without looking at another
    /// source: below `bound` (the other sources' smallest head) and the end, the first entry
    /// of a key after the last row pushed, a version at or before `ts`, not a deletion, and
    /// (`resolve`) without out-of-line values. Up to `max`; how many, and the last version.
    fn run_into(&mut self, ts: u64, bound: Option<&[u8]>, resolve: bool, batch: &mut batch::RowBatch, max: usize) -> (usize, u64) {
        let Cursor::Level { block: Some(b), pos, end, whole, .. } = self else {
            return (0, 0);
        };
        let (mut n, mut last) = (0, 0);
        while n < max && pos.index() < b.len() {
            let k = pos.key();
            if (!*whole && !end.is_empty() && k >= end.as_slice()) || bound.is_some_and(|x| k >= x) || k.len() < 8 {
                break;
            }
            let (uk, kts) = (user_key(k), key_ts(k));
            // (The entry before is the last row pushed: the same user key when it shares
            // all of it, as user keys are prefix-free; compared at a restart.)
            let prev = batch.last_key().map_or(0, <[u8]>::len);
            let same = pos.shared() >= prev || (pos.shared() == 0 && batch.last_key() == Some(uk));
            if kts > ts || same {
                break;
            }
            let Some(v) = b.value_ref(pos) else {
                break;
            };
            if resolve && toast::has_external(v) {
                break;
            }
            b.push_value(pos, uk, batch);
            b.step(pos);
            n += 1;
            last = kts;
        }
        (n, last)
    }

    /// Past the entries at the head of the current block that `Scan::position` would step
    /// over one at a time, below `bound` (the other sources' smallest head) and the end:
    /// older versions of the key decided last (`decided`), and deletions at or before `ts`
    /// (each key's newest such version: `decided` and `decided_ts` follow them). How many
    /// deleted keys it passed. (A queue's head of deleted keys, a block at a time.)
    fn skip_deleted(&mut self, ts: u64, bound: Option<&[u8]>, decided: &mut Vec<u8>, decided_ts: &mut u64) -> u32 {
        let Cursor::Level { block: Some(b), pos, end, whole, .. } = self else {
            return 0;
        };
        let mut n = 0;
        while pos.index() < b.len() {
            let k = pos.key();
            if (!*whole && !end.is_empty() && k >= end.as_slice()) || bound.is_some_and(|x| k >= x) || k.len() < 8 {
                break;
            }
            let (uk, kts) = (user_key(k), key_ts(k));
            if uk != decided.as_slice() {
                if kts > ts || b.value_ref(pos).is_some() {
                    break;
                }
                decided.clear();
                decided.extend_from_slice(uk);
                *decided_ts = kts;
                n += 1;
            }
            b.step(pos);
        }
        n
    }

    fn advance(&mut self) {
        match self {
            Cursor::Mem { buf, .. } => {
                buf.pop_front();
            }
            Cursor::Level { block, pos, .. } => {
                if let Some(b) = block {
                    b.step(pos);
                }
            }
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
                    if pos.index() < b.len() && pos.key() >= target {
                        return;
                    }
                    if b.last_key().is_some_and(|k| k >= target) {
                        b.seek_on(target, pos);
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

/// How many of the ranked sources `rank` (heads ascending) have a head below `k`.
fn rank_of(sources: &[Cursor], rank: &[usize], k: &[u8]) -> usize {
    rank.iter().take_while(|&&j| sources[j].head().is_some_and(|(h, _)| h < k)).count()
}

struct Merge {
    sources: Vec<Cursor>,
    end: Vec<u8>,
    /// The source `min` chose last, while no other source has moved since: the others'
    /// smallest head is `bound` (`bounded` false: they have none left), so entries of `cur`
    /// below it come next without looking at the others (a range held by one file, or runs
    /// of keys from one source, cost a key comparison per entry).
    cur: Option<usize>,
    bound: Vec<u8>,
    bounded: bool,
    /// While `cur` is set: the sources with a head, by their heads ascending (`cur` first).
    /// Only `cur` moves before the next `min`, so once it passes the bound it goes back in
    /// its place among the others, whose heads are where they were, without filling and
    /// comparing them all again (two sources taking turns, a table's rows and an UPDATE's
    /// new versions of every other one, switch at every row).
    rank: Vec<usize>,
}

impl Merge {
    fn new(sources: Vec<Cursor>, end: Vec<u8>) -> Merge {
        Merge { sources, end, cur: None, bound: Vec::new(), bounded: false, rank: Vec::new() }
    }

    /// The source holding the smallest raw entry left (None: none below the end). Identical
    /// storage keys can't occur in two sources (a commit ts belongs to one memtable), so
    /// ties don't need breaking. Only the source chosen last may have moved since (`cur`).
    fn min(&mut self) -> Result<Option<usize>> {
        if let Some(i) = self.cur {
            self.sources[i].fill()?;
            // (A source's entries stop at the end: `head`, the memtables' ranges.)
            match self.sources[i].head() {
                Some((k, _)) if !self.bounded || k < self.bound.as_slice() => return Ok(Some(i)),
                None if !self.bounded => return Ok(None),
                _ => {}
            }
            // (Past the bound, the next source's head, or run out.)
            debug_assert_eq!(self.rank.first(), Some(&i));
            self.rank.remove(0);
            if let Some((k, _)) = self.sources[i].head() {
                let at = 1 + rank_of(&self.sources, &self.rank[1..], k);
                self.rank.insert(at, i);
            }
        } else {
            for c in &mut self.sources {
                c.fill()?;
            }
            self.rank.clear();
            for i in 0..self.sources.len() {
                if let Some((k, _)) = self.sources[i].head() {
                    let at = rank_of(&self.sources, &self.rank, k);
                    self.rank.insert(at, i);
                }
            }
        }
        let Some(&i) = self.rank.first() else {
            self.cur = None;
            return Ok(None);
        };
        let (k, _) = self.sources[i].head().expect("a ranked head");
        if !self.end.is_empty() && k >= self.end.as_slice() {
            self.cur = None;
            return Ok(None);
        }
        self.bound.clear();
        self.bounded = self.rank.len() > 1;
        if self.bounded {
            let (b, _) = self.sources[self.rank[1]].head().expect("a ranked head");
            self.bound.extend_from_slice(b);
        }
        self.cur = Some(i);
        Ok(Some(i))
    }

    /// Every source on to its first entry >= `target`.
    fn seek(&mut self, target: &[u8]) {
        self.cur = None;
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
    /// The range deletions that can hide something here (`RangeDels::relevant`).
    rdels: rdel::RangeDels,
    _view: Arc<View>,
    /// Rows come with their out-of-line values put back (`Store::scan`; `toast`).
    resolve: bool,
    /// The rows of a hot range's chains (`hot`), read already: no merge.
    rows: Option<Box<std::vec::IntoIter<(Bytes, Bytes)>>>,
    /// Where the scan started, while it has stepped over only deleted keys (`dead`: a hint
    /// is recorded at its first live key past `dead::MIN_DEAD` of them); None: not looked for.
    dead: Option<(Arc<dead::DeadRanges>, Vec<u8>)>,
    dead_run: u32,
}

/// A cursor per source of `view` over [start, end), newest source first.
fn sources(view: &View, start: &[u8], end: &[u8]) -> Vec<Cursor> {
    // (Files holding no key of the range aren't sources: each source is looked at whenever
    // the merge changes sources.)
    // (By user keys: a file's first key with its version appended can sort past an `end`
    // that its user key is a prefix of.)
    let overlaps = |s: &&Arc<SstReader>| s.overlaps(start, end);
    let mut sources = Vec::with_capacity(2 + view.imm.len() + view.l0.len());
    sources.push(Cursor::mem(view.mem.clone(), start, end));
    for f in &view.imm {
        match f {
            Frozen::Mem(m) => sources.push(Cursor::mem(m.clone(), start, end)),
            Frozen::Sst(s) if overlaps(&s) => sources.push(Cursor::level(vec![s.clone()], start, end)),
            Frozen::Sst(_) => {}
        }
    }
    for s in view.l0.iter().filter(overlaps) {
        sources.push(Cursor::level(vec![s.clone()], start, end));
    }
    let l1: Vec<_> = view.l1.iter().filter(overlaps).cloned().collect();
    if !l1.is_empty() {
        sources.push(Cursor::level(l1, start, end));
    }
    sources
}

/// Whether `cursors` are the sources `sources(view, start, end)` would make, in its order:
/// the same memtables and files (a reseek then moves them in place, nothing made).
fn same_sources(view: &View, cursors: &[Cursor], start: &[u8], end: &[u8]) -> bool {
    let overlaps = |s: &Arc<SstReader>| s.overlaps(start, end);
    let mut c = cursors.iter();
    let mem = |c: Option<&Cursor>, m: &Arc<Memtable>| matches!(c, Some(Cursor::Mem { mem, .. }) if Arc::ptr_eq(mem, m));
    let file = |c: Option<&Cursor>, s: &Arc<SstReader>| c.and_then(Cursor::files).is_some_and(|f| f.len() == 1 && Arc::ptr_eq(&f[0], s));
    if !mem(c.next(), &view.mem) {
        return false;
    }
    for f in &view.imm {
        let ok = match f {
            Frozen::Mem(m) => mem(c.next(), m),
            Frozen::Sst(s) if overlaps(s) => file(c.next(), s),
            Frozen::Sst(_) => true,
        };
        if !ok {
            return false;
        }
    }
    for s in view.l0.iter().filter(|s| overlaps(s)) {
        if !file(c.next(), s) {
            return false;
        }
    }
    let mut l1 = view.l1.iter().filter(|s| overlaps(s)).peekable();
    if l1.peek().is_some() {
        let Some(files) = c.next().and_then(Cursor::files) else {
            return false;
        };
        let mut n = 0;
        for s in l1 {
            if files.get(n).is_none_or(|f| !Arc::ptr_eq(f, s)) {
                return false;
            }
            n += 1;
        }
        if n != files.len() {
            return false;
        }
    }
    c.next().is_none()
}

impl Scan {
    fn of_rows(view: Arc<View>, rows: Vec<(Bytes, Bytes)>, ts: u64) -> Scan {
        Scan {
            merge: Merge::new(Vec::new(), Vec::new()),
            ts,
            decided: None,
            decided_ts: 0,
            skipped: 0,
            newer: None,
            horizon: 0,
            _view: view,
            resolve: false,
            rows: Some(Box::new(rows.into_iter())),
            rdels: Default::default(),
            dead: None,
            dead_run: 0,
        }
    }

    fn new(view: Arc<View>, start: Vec<u8>, end: Vec<u8>, ts: u64, horizon: u64) -> Scan {
        let sources = sources(&view, &start, &end);
        let rdels = rdel::RangeDels::new(view.rdels.relevant(&start, &end, ts));
        Scan {
            merge: Merge::new(sources, end),
            ts,
            decided: None,
            decided_ts: 0,
            skipped: 0,
            newer: None,
            horizon,
            rdels,
            _view: view,
            resolve: false,
            rows: None,
            dead: None,
            dead_run: 0,
        }
    }

    /// The scan with these ranges left out (a transaction's own range deletions).
    pub fn excluding(mut self, ranges: &[(Vec<u8>, Vec<u8>)]) -> Scan {
        if ranges.is_empty() {
            return self;
        }
        self.rdels = self.rdels.with(ranges.iter().map(|(s, e)| rdel::RangeDel {
            start: s.clone(),
            end: e.clone(),
            ts: rdel::ALL,
        }));
        self
    }

    /// The scan over [start, end) instead, at its snapshot `ts` (false: it can't be, a hot
    /// range's rows or another snapshot: make a new one). The sources the range needs are
    /// the ones a new scan would have; each it had already keeps its place (`Cursor::reseek`),
    /// so ascending lookups (a nested loop's inner index scan, keyset pages) read the next key
    /// in the block they are in. Range deletions are the new range's; dead-range hints aren't
    /// looked up again.
    pub fn reseek(&mut self, start: &[u8], end: &[u8], ts: u64) -> bool {
        if self.rows.is_some() || ts != self.ts || self.rdels.iter().any(|r| r.ts == rdel::ALL) {
            return false;
        }
        let mut old = std::mem::take(&mut self.merge.sources);
        let sources = if same_sources(&self._view, &old, start, end) {
            // (The same sources: moved in place, none made.)
            for c in &mut old {
                c.reseek(start, end);
            }
            old
        } else {
            let fresh = sources(&self._view, start, end);
            let same = |a: &Cursor, b: &Cursor| match (a, b) {
                (Cursor::Mem { mem: x, .. }, Cursor::Mem { mem: y, .. }) => Arc::ptr_eq(x, y),
                _ => match (a.files(), b.files()) {
                    (Some(x), Some(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| Arc::ptr_eq(p, q)),
                    _ => false,
                },
            };
            fresh
                .into_iter()
                .map(|f| match old.iter().position(|o| same(o, &f)) {
                    Some(i) => {
                        let mut c = old.swap_remove(i);
                        c.reseek(start, end);
                        c
                    }
                    None => f,
                })
                .collect()
        };
        let mut e = std::mem::take(&mut self.merge.end);
        e.clear();
        e.extend_from_slice(end);
        self.merge = Merge::new(sources, e);
        self.rdels = rdel::RangeDels::new(self._view.rdels.relevant(start, end, ts));
        self.decided = None;
        self.decided_ts = 0;
        self.skipped = 0;
        self.newer = None;
        self.dead = None;
        self.dead_run = 0;
        true
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

impl Scan {
    /// The source whose head is the next visible row (None: none left), the versions and
    /// deletions before it passed. The row stays at the head until taken or advanced.
    fn position(&mut self) -> Result<Option<usize>> {
        loop {
            let Some(i) = self.merge.min()? else {
                return Ok(None);
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
                // (A key not visible yet: not a deleted one.)
                self.dead = None;
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
            // (The decided key's buffer is reused: no allocation per row.)
            match &mut self.decided {
                Some(d) => {
                    d.clear();
                    d.extend_from_slice(uk);
                }
                None => self.decided = Some(uk.to_vec()),
            }
            self.decided_ts = ts;
            if v.is_none() {
                self.dead_run += 1;
                self.merge.sources[i].advance();
                // (The deletions after it in its block that need no other source: a run at
                // a time. Range deletions don't change them: they hide versions, and these
                // are hidden already.)
                let bound = self.merge.bounded.then_some(self.merge.bound.as_slice());
                let d = self.decided.as_mut().expect("decided");
                self.dead_run += self.merge.sources[i].skip_deleted(self.ts, bound, d, &mut self.decided_ts);
                continue;
            }
            if !self.rdels.is_empty()
                && let Some(r) = self.rdels.covering(uk, ts, self.ts)
            {
                // Deleted with its range; a range left out whole is passed at once. (One the
                // transaction left out itself isn't deleted for others: no hint from it.)
                if r.ts == rdel::ALL {
                    self.dead = None;
                    let end = r.end.clone();
                    self.merge.seek(&end);
                } else {
                    self.dead_run += 1;
                    self.merge.sources[i].advance();
                }
                continue;
            }
            // (The first live key past a run of deleted ones: `dead`.)
            if let Some((d, start)) = self.dead.take()
                && self.dead_run >= dead::MIN_DEAD
            {
                d.record(&start, uk, self.ts);
            }
            return Ok(Some(i));
        }
    }

    /// Up to `max` more rows into `batch` (none: the scan is done), by reference: the values
    /// stay in the blocks holding them. Rows with out-of-line values a resolving scan puts
    /// back, and a hot range's rows, are pushed owned.
    pub fn fill_batch(&mut self, batch: &mut batch::RowBatch, max: usize) -> Result<()> {
        let target = batch.len() + max;
        if let Some(rows) = &mut self.rows {
            // (A hot range's rows, their keys shared with its chains: none copied.)
            while batch.len() < target {
                let Some((k, v)) = rows.next() else { break };
                if self.rdels.holds(&k) {
                    continue;
                }
                let v = match self.resolve && toast::has_external(&v) {
                    true => toast::resolve(&self._view, &k, v, self.ts)?,
                    false => v,
                };
                batch.push(&k, v);
            }
            return Ok(());
        }
        while batch.len() < target {
            let Some(i) = self.position()? else {
                break;
            };
            let c = &mut self.merge.sources[i];
            let (k, v) = c.head().expect("a head entry");
            if self.resolve && v.is_some_and(toast::has_external) {
                let (mut k, v) = c.take();
                k.truncate(k.len().saturating_sub(8));
                let v = toast::resolve(&self._view, &k, v.expect("a value"), self.ts)?;
                batch.push(&k, v);
                continue;
            }
            c.push_head(k.len().saturating_sub(8), batch);
            c.advance();
            // (The rows after it in its block that need no other source: a block at a time;
            // under range deletions a row at a time, each checked.)
            if !self.rdels.is_empty() {
                continue;
            }
            let bound = self.merge.bounded.then_some(self.merge.bound.as_slice());
            let (n, last) = c.run_into(self.ts, bound, self.resolve, batch, target - batch.len());
            if n > 0 {
                let d = self.decided.get_or_insert_with(Vec::new);
                d.clear();
                d.extend_from_slice(batch.last_key().expect("a row"));
                self.decided_ts = last;
            }
        }
        Ok(())
    }
    /// The rows stored under `keys` (strictly ascending, within the scan's range) into
    /// `batch`, those it has, in their order: the merge sought forward from key to key, a
    /// source staying in its block while the next key is there (a sorted batch of point
    /// reads without a lookup each). Rows are by reference, as `fill_batch` gives them.
    pub fn fetch_sorted<K: AsRef<[u8]>>(&mut self, keys: &[K], batch: &mut batch::RowBatch) -> Result<()> {
        debug_assert!(self.rows.is_none(), "a merging scan");
        self.dead = None;
        for k in keys {
            let k = k.as_ref();
            // (A key's versions are all after it: user keys are prefix-free.)
            self.merge.seek(k);
            self.decided = None;
            self.newer = None;
            self.skipped = 0;
            let Some(i) = self.position()? else {
                return Ok(());
            };
            let c = &mut self.merge.sources[i];
            let (sk, v) = c.head().expect("a head entry");
            if user_key(sk) != k {
                continue;
            }
            if self.resolve && v.is_some_and(toast::has_external) {
                let (mut sk, v) = c.take();
                sk.truncate(sk.len().saturating_sub(8));
                let v = toast::resolve(&self._view, &sk, v.expect("a value"), self.ts)?;
                batch.push(&sk, v);
                continue;
            }
            c.push_head(sk.len().saturating_sub(8), batch);
        }
        Ok(())
    }
}

impl Iterator for Scan {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(rows) = &mut self.rows {
            // (A hot range's rows: committed range deletions drop the chains, so only the
            // reader's own, `excluding`, can cover one.)
            let (k, v) = loop {
                let (k, v) = rows.next()?;
                if !self.rdels.holds(&k) {
                    break (k, v);
                }
            };
            if self.resolve && toast::has_external(&v) {
                return Some(toast::resolve(&self._view, &k, v, self.ts).map(|v| (k.to_vec(), v)));
            }
            return Some(Ok((k.to_vec(), v)));
        }
        let i = match self.position() {
            Ok(Some(i)) => i,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };
        // (The entry's own key cut to the user key: one allocation per row.)
        let (mut k, v) = self.merge.sources[i].take();
        k.truncate(k.len().saturating_sub(8));
        let v = v.expect("a value");
        if self.resolve && toast::has_external(&v) {
            return Some(toast::resolve(&self._view, &k, v, self.ts).map(|v| (k, v)));
        }
        Some(Ok((k, v)))
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
        block: Option<sst::Expanded>,
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
                    let b = sst.block(*block_idx)?.expand();
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
                    let b = ssts[*file - 1].block(*block_idx)?.expand();
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
    rdels: rdel::RangeDels,
    _view: Arc<View>,
    /// Rows come with their out-of-line values put back (`Store::scan_rev`; `toast`).
    resolve: bool,
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
        let rdels = rdel::RangeDels::new(view.rdels.relevant(&start, &end, ts));
        RevScan {
            sources,
            ts,
            pending: None,
            rdels,
            _view: view,
            resolve: false,
        }
    }

    /// `Scan::excluding`.
    pub fn excluding(mut self, ranges: &[(Vec<u8>, Vec<u8>)]) -> RevScan {
        self.rdels = self.rdels.with(ranges.iter().map(|(s, e)| rdel::RangeDel {
            start: s.clone(),
            end: e.clone(),
            ts: rdel::ALL,
        }));
        self
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
            let ts0 = key_ts(&first.0);
            let mut chosen = (ts0 <= self.ts).then_some((ts0, first.1));
            loop {
                match self.next_raw() {
                    Ok(Some(e)) if user_key(&e.0) == uk.as_slice() => {
                        let t = key_ts(&e.0);
                        if t <= self.ts {
                            chosen = Some((t, e.1));
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
            if let Some((t, Some(v))) = chosen
                && self.rdels.covering(&uk, t, self.ts).is_none()
            {
                if self.resolve && toast::has_external(&v) {
                    return Some(toast::resolve(&self._view, &uk, v, self.ts).map(|v| (uk, v)));
                }
                return Some(Ok((uk, v)));
            }
        }
    }
}

impl Store {
    /// Descending scan of user keys in [start, end) at snapshot `ts`.
    pub fn scan_rev(&self, start: &[u8], end: &[u8], ts: u64) -> RevScan {
        let mut s = RevScan::new(self.view(), start.to_vec(), end.to_vec(), ts);
        s.resolve = true;
        s
    }

    /// `scan_rev` with rows as they are stored (their out-of-line values' pointers).
    pub fn scan_rev_raw(&self, start: &[u8], end: &[u8], ts: u64) -> RevScan {
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

    /// A relation range scanned whole again and again is read from its version chains
    /// (`hot`): what every snapshot sees stays what the merging scan and the model show,
    /// through hot-key updates, deletions, pruning under pinned snapshots, flushes,
    /// compactions and a commit ingested as an SST (which drops the kept ranges).
    #[test]
    fn hot_ranges_match_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions { memtable_bytes: 64 << 10, sync: false, ..StoreOptions::default() },
        )
        .unwrap();
        let rel = keys::index_prefix(16_384, 0);
        let end = keys::prefix_end(&rel);
        let key = |i: u32| {
            let mut k = rel.clone();
            k.extend_from_slice(&i.to_be_bytes());
            k
        };
        // A key of the next relation: never in the range's scans.
        let mut other = keys::index_prefix(16_384, 1);
        other.push(1);
        let mut model: BTreeMap<Vec<u8>, Vec<(u64, Option<Bytes>)>> = BTreeMap::new();
        let mut pins: Vec<SnapshotPin> = Vec::new();
        let check = |model: &BTreeMap<Vec<u8>, Vec<(u64, Option<Bytes>)>>, ts: u64, what: &str| {
            let expect: Vec<(Vec<u8>, Bytes)> = model
                .range(rel.clone()..end.clone())
                .filter_map(|(k, vs)| Some((k.clone(), vs.iter().rev().find(|(t, _)| *t <= ts)?.1.clone()?)))
                .collect();
            let merged: Vec<_> = store.view().scan(&rel, &end, ts).map(|r| r.unwrap()).collect();
            assert_eq!(merged, expect, "merging scan, {what}, ts {ts}");
            let hot: Vec<_> = store.scan(&rel, &end, ts).map(|r| r.unwrap()).collect();
            assert_eq!(hot, expect, "hot scan, {what}, ts {ts}");
            let (a, b) = (key(3), key(7));
            let part: Vec<_> = store.scan(&a, &b, ts).map(|r| r.unwrap()).collect();
            let want: Vec<_> = expect.iter().filter(|(k, _)| *k >= a && *k < b).cloned().collect();
            assert_eq!(part, want, "part of the range, {what}, ts {ts}");
            // A batch at a time (small batches: runs end mid-block), and sorted point reads
            // of every other key (some never written).
            let mut batched = Vec::new();
            let mut s = store.view().scan(&rel, &end, ts);
            loop {
                let mut b = batch::RowBatch::default();
                s.fill_batch(&mut b, 7).unwrap();
                if b.is_empty() {
                    break;
                }
                while let Some(r) = b.take() {
                    batched.push(r);
                }
            }
            assert_eq!(batched, expect, "batched scan, {what}, ts {ts}");
            let wanted: Vec<Vec<u8>> = (0..400u32).step_by(2).map(key).collect();
            let mut b = batch::RowBatch::default();
            store.get_sorted_into(&wanted, ts, false, &mut b).unwrap();
            let got: Vec<_> = std::iter::from_fn(|| b.take()).collect();
            let want: Vec<_> = expect.iter().filter(|(k, _)| wanted.contains(k)).cloned().collect();
            assert_eq!(got, want, "sorted point reads, {what}, ts {ts}");
            // Point reads: the newest version at `ts`. (A deletion every snapshot sees may
            // be gone, as compaction drops it: no version and a deletion read alike.)
            let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
            let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
            let got = store.get_versions(&refs, ts).unwrap();
            let live = |v: Option<(u64, Option<Bytes>)>| v.and_then(|(t, v)| Some((t, v?)));
            for (k, g) in keys.iter().zip(got) {
                let want = model[k].iter().rev().find(|(t, _)| *t <= ts).cloned();
                assert_eq!(live(g), live(want), "get_versions {k:?}, {what}, ts {ts}");
            }
        };
        for round in 0..40u32 {
            // Each commit updates a hot key or two (now and then deleting one), and a key
            // outside the range.
            let mut writes = vec![
                (key(round % 10), (round % 9 != 8).then(|| Bytes::from(format!("v{round}")))),
                (other.clone(), Some(Bytes::from_static(b"x"))),
            ];
            if round % 3 == 0 {
                writes.push((key(10 + round % 4), Some(Bytes::from(format!("w{round}")))));
            }
            writes.sort_by(|a, b| a.0.cmp(&b.0));
            let ts = store.commit(writes.clone(), |_| Ok(())).unwrap();
            for (k, v) in writes {
                model.entry(k).or_default().push((ts, v));
            }
            if round % 5 == 0 {
                pins.push(store.pin_visible());
            }
            if round % 11 == 10 {
                // An old snapshot ends: its versions may go.
                pins.remove(0);
            }
            match round {
                15 => {
                    store.rotate().unwrap();
                    store.flush_all().unwrap();
                }
                25 => store.compact().unwrap(),
                30 => {
                    // A commit big enough to be ingested as an SST of its own.
                    let big: Vec<_> =
                        (100..400u32).map(|i| (key(i), Some(Bytes::from(vec![b'b'; 64])))).collect();
                    let ts = store.commit(big.clone(), |_| Ok(())).unwrap();
                    for (k, v) in big {
                        model.entry(k).or_default().push((ts, v));
                    }
                }
                _ => {}
            }
            for p in &pins {
                check(&model, p.ts, &format!("round {round}"));
            }
            check(&model, store.visible_ts(), &format!("round {round}"));
        }
        assert!(store.hot.any.load(Ordering::Acquire), "the range is kept");
    }

    /// An asynchronous commit is visible as soon as it returns, and the WAL writer makes it
    /// durable within its delay; a synchronous commit after it covers it at once.
    #[test]
    fn async_commits_visible_then_durable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions { sync: true, ..StoreOptions::default() }).unwrap();
        let t1 = store.commit_async(vec![(b"a".to_vec(), Some(Bytes::from_static(b"1")))], |_, _| Ok(()), 50).unwrap();
        assert_eq!(store.visible_ts(), t1);
        assert_eq!(store.get(b"a", t1).unwrap().as_deref(), Some(&b"1"[..]));
        let start = std::time::Instant::now();
        while store.durable.load(Ordering::Acquire) < t1 {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "never made durable");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let t2 = store.commit_async(vec![(b"b".to_vec(), Some(Bytes::from_static(b"2")))], |_, _| Ok(()), 10_000).unwrap();
        let t3 = put(&store, "c", Some("3"));
        assert!(t3 > t2 && store.durable.load(Ordering::Acquire) >= t3);
        drop(store);
        let store = Store::open(dir.path(), StoreOptions::default()).unwrap();
        assert_eq!(scan_all(&store, store.visible_ts()).len(), 3);
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

    #[test]
    fn close_rewrite_preserves_unrelated_l1_and_pending_ingest() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { sync: false, background: false, ..Default::default() }).unwrap();
        put(&s, "m0000", Some("middle"));
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        let middle = s.view().l1[0].meta.id;
        let spill = s.spill_dir();
        std::fs::create_dir_all(&spill).unwrap();
        let mut runs = Vec::new();
        for (id, prefix) in [(1000, "a"), (1001, "z")] {
            let mut w = SstWriter::create_run(&spill, id, true).unwrap();
            for i in 0..256 {
                w.add(&mvcc_key(format!("{prefix}{i:04}").as_bytes(), 1), Some(&[1; 80])).unwrap();
            }
            runs.push(Arc::new(SstReader::open(&spill, w.finish().unwrap(), s.opts.cache.clone()).unwrap()));
        }
        s.commit_sorted(0, &mut std::iter::empty(), |_, _| Ok(()), &|_, _| Ok(()), |_| {}, runs).unwrap();
        assert_eq!(s.view().l1.iter().filter(|f| f.meta.commit_ts.is_some()).count(), 2);
        // The state a concurrent ingest can leave between close's flush and rewrite:
        // its manifest L0 entry is still a Frozen::Sst, absent from view.l0.
        s.commit_sorted(1, &mut std::iter::once(Ok((b"n0000".to_vec(), Some(Bytes::from_static(b"pending"))))), |_, _| Ok(()), &|_, _| Ok(()), |_| {}, Vec::new()).unwrap();
        let pending: Vec<_> = s.manifest.lock().l0.iter().map(|m| m.id).collect();
        assert_eq!(pending.len(), 1);
        assert!(s.view().l0.is_empty());
        assert!(s.view().imm.iter().any(|f| matches!(f, Frozen::Sst(_))));
        s.rewrite_linked_l1_for_close().unwrap();
        assert_eq!(s.manifest.lock().l0.iter().map(|m| m.id).collect::<Vec<_>>(), pending);
        assert!(s.view().l0.is_empty());
        assert!(s.view().l1.iter().any(|f| f.meta.id == middle), "the ordinary file between the linked ranges is untouched");
        assert!(s.view().l1.iter().all(|f| f.meta.commit_ts.is_none()));
        assert_eq!(s.get(b"n0000", s.visible_ts()).unwrap().as_deref(), Some(&b"pending"[..]));
        s.close().unwrap();
        assert!(s.view().l1.iter().any(|f| f.meta.id == middle));
        drop(s);
        let s = Store::open(dir.path(), StoreOptions { sync: false, background: false, ..Default::default() }).unwrap();
        assert_eq!(s.get(b"m0000", s.visible_ts()).unwrap().as_deref(), Some(&b"middle"[..]));
        assert_eq!(s.get(b"n0000", s.visible_ts()).unwrap().as_deref(), Some(&b"pending"[..]));
        assert_eq!(s.scan(b"a", b"b", s.visible_ts()).count(), 256);
        assert_eq!(s.scan(b"z", b"{", s.visible_ts()).count(), 256);
    }

    /// One point reader over every kind of source at once (memtable, a frozen memtable, an
    /// ingested commit's frozen file, L0, L1), keys read in order and out of it: `get_with` and
    /// `first_with` answer as a scan does, the blocks a reader keeps never another source's.
    #[test]
    fn point_reads_over_every_source() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { memtable_bytes: 64 << 10, sync: false, background: false, ..Default::default() }).unwrap();
        let key = |i: u32| format!("k{i:05}").into_bytes();
        let commit = |s: &Arc<Store>, keys: &mut dyn Iterator<Item = u32>, tag: &str| {
            let w: Vec<_> = keys.map(|i| (key(i), (i % 11 != 3).then(|| Bytes::from(format!("{tag}{i}"))))).collect();
            s.commit(w, |_| Ok(())).unwrap();
        };
        commit(&s, &mut (0..3000).step_by(2), "l1-");
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        commit(&s, &mut (1..3000).step_by(6), "l0-");
        s.flush_memtables().unwrap();
        commit(&s, &mut (2..3000).step_by(10), "frozen-");
        s.rotate().unwrap();
        // (Big enough to be its commit's file, ahead of the frozen memtable.)
        commit(&s, &mut (3..3000).step_by(4), "ingested-");
        commit(&s, &mut (5..3000).step_by(14), "mem-");
        let v = s.view();
        assert!(v.imm.iter().any(|f| matches!(f, Frozen::Mem(_))) && v.imm.iter().any(|f| matches!(f, Frozen::Sst(_))));
        assert!(!v.l0.is_empty() && !v.l1.is_empty());
        drop(v);
        let ts = s.visible_ts();
        let all: Vec<(Vec<u8>, Bytes)> = s.scan(b"", b"", ts).map(|r| r.unwrap()).collect();
        let order: Vec<u32> = (0..3000).chain((0..3000).rev().step_by(7)).collect();
        let mut r = s.point_reader(ts, true);
        for &i in &order {
            let k = key(i);
            let want = all.iter().find(|(x, _)| *x == k).map(|e| e.1.clone());
            assert_eq!(s.get_with(&mut r, &k, true).unwrap().and_then(|v| v.1), want, "get {i}");
            let end = key(i + 5);
            let first = all.iter().find(|(x, _)| *x >= k && *x < end).cloned();
            match s.first_with(&mut r, &k, &end, true).unwrap() {
                point::First::Row(fk, fv) => assert_eq!(Some((fk, fv)), first, "first from {i}"),
                point::First::None => assert_eq!(first, None, "first from {i}"),
                point::First::Unknown => {}
            }
        }
    }

    /// A linked run whose keys no other source holds goes to L1 as it is (written once);
    /// one overlapping a file's or a memtable's keys, or not compressed, goes to L0. Reads,
    /// older snapshots and a crash's recovery see them all as the commit's.
    #[test]
    fn lone_linked_runs_go_to_l1() {
        let dir = tempfile::tempdir().unwrap();
        let o = || StoreOptions { memtable_bytes: 1 << 20, sync: false, background: false, ..Default::default() };
        let mut s = Store::open(dir.path(), o()).unwrap();
        // (Two files: what lies between them is no file's.)
        put(&s, "a0005", Some("old"));
        s.flush_memtables().unwrap();
        put(&s, "c0001", Some("old"));
        s.flush_memtables().unwrap();
        put(&s, "d0001", Some("mem"));
        let before = s.visible_ts();
        let spill = dir.path().join(SPILL_DIR);
        std::fs::create_dir_all(&spill).unwrap();
        let val = |p: &str, i: u32| format!("{p}{i}-{}", "v".repeat(30));
        let mut id = 1000;
        let mut run = |p: &str, n: u32, compress: bool| -> Arc<SstReader> {
            id += 1;
            let mut w = if compress { SstWriter::create_run(&spill, id, true) } else { SstWriter::create_temp(&spill, id) }.unwrap();
            for i in 0..n {
                // (A run's value: tag 1, present; every 9th a deletion, tag 0.)
                let v: Vec<u8> = if i % 9 == 4 { vec![0] } else { [&[1u8][..], val(p, i).as_bytes()].concat() };
                w.add(&mvcc_key(format!("{p}{i:04}").as_bytes(), 3), Some(&v)).unwrap();
            }
            Arc::new(SstReader::open(&spill, w.finish().unwrap(), s.opts.cache.clone()).unwrap())
        };
        // b: alone; a: overlaps a file; d: overlaps the memtable; e: not compressed.
        let linked = vec![run("b", 3000, true), run("a", 50, true), run("d", 50, true), run("e", 50, false)];
        let ts = s
            .commit_sorted(0, &mut std::iter::empty::<Result<(Vec<u8>, Option<Bytes>)>>(), |_, _| Ok(()), &|_, _| Ok(()), |_| {}, linked)
            .unwrap();
        assert_eq!(s.view().l1.iter().filter(|f| f.meta.commit_ts == Some(ts)).count(), 1);
        let check = |s: &Store, compacted: bool| {
            for (p, n) in [("b", 3000u32), ("a", 50), ("d", 50), ("e", 50)] {
                for i in (0..n).filter(|i| i % 7 == 0 || *i < 6) {
                    let k = format!("{p}{i:04}");
                    let want = (i % 9 != 4).then(|| val(p, i));
                    let got = s.get(k.as_bytes(), ts).unwrap().map(|v| String::from_utf8(v.to_vec()).unwrap());
                    assert_eq!(got, want, "{k}");
                    let version = s.get_version(k.as_bytes(), ts).unwrap().map(|v| v.0);
                    if !compacted || want.is_some() {
                        assert_eq!(version, Some(ts), "{k}, compacted={compacted}");
                    } else {
                        assert!(version.is_none() || version == Some(ts), "{k}: {version:?}");
                    }
                }
            }
            assert_eq!(s.get(b"c0001", ts).unwrap().as_deref(), Some(&b"old"[..]));
            let bs = scan_all(s, ts).iter().filter(|(k, _)| k.starts_with('b')).count();
            assert_eq!(bs, (0..3000).filter(|i| i % 9 != 4).count());
        };
        check(&s, false);
        assert!(s.view().l1.iter().any(|f| f.meta.commit_ts == Some(ts) && f.meta.first_key.starts_with(b"b")));
        // (Older snapshots see none of it.)
        assert_eq!(s.get(b"b0001", before).unwrap(), None);
        assert_eq!(s.get(b"d0001", before).unwrap().as_deref(), Some(&b"mem"[..]));
        // A crash: the WAL's commits replayed around the run.
        drop(s);
        s = Store::open(dir.path(), o()).unwrap();
        check(&s, false);
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        check(&s, true);
    }

    /// A clean close leaves L0 below its trigger however many sessions of a few writes the
    /// store had: a close compacts an L0 at its trigger, as an open does in the background.
    #[test]
    fn restarts_keep_l0_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let o = || StoreOptions { sync: false, ..Default::default() };
        let trigger = o().l0_compaction_trigger;
        for i in 0..50 {
            let s = Store::open(dir.path(), o()).unwrap();
            put(&s, &format!("k{i:03}"), Some("v"));
            assert_eq!(s.get(b"k000", s.visible_ts()).unwrap().as_deref(), Some(&b"v"[..]));
            s.close().unwrap();
            let v = s.view();
            assert!(v.l0.len() <= trigger, "restart {i}: {} L0 files", v.l0.len());
        }
        let s = Store::open(dir.path(), o()).unwrap();
        assert_eq!(scan_all(&s, s.visible_ts()).len(), 50);
    }

    /// Compaction rewrites only the L1 files an L0 file's keys overlap: one between two L0
    /// files' ranges stays, the outputs around it, and L1 stays in order without overlaps.
    #[test]
    fn compaction_leaves_l1_files_between_l0_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let o = || StoreOptions { sync: false, background: false, ..Default::default() };
        let mut s = Store::open(dir.path(), o()).unwrap();
        let fill = |s: &Arc<Store>, p: &str, v: &str| {
            for i in 0..200 {
                put(s, &format!("{p}{i:04}"), Some(v));
            }
            s.flush_memtables().unwrap();
        };
        fill(&s, "m", "m1");
        s.compact().unwrap();
        let mid = s.view().l1[0].meta.id;
        fill(&s, "a", "a1");
        fill(&s, "x", "x1");
        s.compact().unwrap();
        let l1 = |s: &Arc<Store>| s.view().l1.iter().map(|f| (f.meta.id, user_key(&f.meta.first_key).to_vec(), user_key(&f.meta.last_key).to_vec())).collect::<Vec<_>>();
        let files = l1(&s);
        assert_eq!(files.len(), 3, "{files:?}");
        assert_eq!(files[1].0, mid, "the file between stays");
        assert!(files.windows(2).all(|w| w[0].2 < w[1].1), "{files:?}");
        // (A version of one of its keys rewrites it.)
        put(&s, "m0100", Some("m2"));
        fill(&s, "b", "b1");
        s.compact().unwrap();
        let files = l1(&s);
        assert!(files.iter().all(|f| f.0 != mid), "{files:?}");
        assert!(files.windows(2).all(|w| w[0].2 < w[1].1), "{files:?}");
        for _ in 0..2 {
            let ts = s.visible_ts();
            let rows = scan_all(&s, ts);
            assert_eq!(rows.len(), 800);
            for (p, v) in [("a", "a1"), ("b", "b1"), ("m", "m1"), ("x", "x1")] {
                assert!(rows.iter().filter(|(k, _)| k.starts_with(p) && k != "m0100").all(|(_, x)| x == v), "{p}");
            }
            assert_eq!(s.get(b"m0100", ts).unwrap().as_deref(), Some(&b"m2"[..]));
            drop(s);
            s = Store::open(dir.path(), o()).unwrap();
        }
    }

    #[test]
    fn point_fetch_ranges_include_mvcc_versions_cold_and_hot() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { sync: false, ..Default::default() }).unwrap();
        let prefix = keys::index_prefix(7, 0);
        let mut key = prefix.clone();
        key.extend_from_slice(&1u64.to_be_bytes());
        let value = Bytes::from_static(b"account-balance");
        let ts = s.commit(vec![(key.clone(), Some(value.clone()))], |_| Ok(())).unwrap();
        let _pin = s.pin(ts);
        assert_eq!(s.get_version_raw(&key, ts).unwrap(), Some((ts, Some(value.clone()))));
        let expected = vec![(key.clone(), value)];
        let mut narrow_end = key.clone();
        narrow_end.push(0);
        let mut narrow = s.scan_raw(&key, &narrow_end, ts);
        assert!(narrow.rows.is_none(), "cold scan must use merging cursors");
        let cold: Vec<_> = narrow.by_ref().map(Result::unwrap).collect();
        let mut correct = s.scan_raw(&key, &keys::prefix_end(&key), ts);
        assert!(correct.rows.is_none(), "correct cold scan must use merging cursors");
        assert_eq!(correct.by_ref().map(Result::unwrap).collect::<Vec<_>>(), expected);
        let whole_end = keys::prefix_end(&prefix);
        for _ in 0..8 {
            assert_eq!(s.scan_raw(&prefix, &whole_end, ts).map(Result::unwrap).collect::<Vec<_>>(), expected);
        }
        let mut promoted = s.scan_raw(&key, &narrow_end, ts);
        assert!(promoted.rows.is_some(), "promoted scan must use hot version chains");
        let hot: Vec<_> = promoted.by_ref().map(Result::unwrap).collect();
        assert_eq!(hot, expected);
        let mut correct = s.scan_raw(&key, &keys::prefix_end(&key), ts);
        assert!(correct.rows.is_some(), "correct promoted scan must use hot version chains");
        assert_eq!(correct.by_ref().map(Result::unwrap).collect::<Vec<_>>(), expected);
        assert_eq!(s.get_version_raw(&key, ts).unwrap().map(|r| r.0), Some(ts));
        eprintln!("endpoint proof ts={ts} cold={cold:?} hot={hot:?} merging_before=true hot_after=true");
        // User-key bounds with an appended zero exclude physical timestamp suffixes.
        // prefix_end must work on both paths; the narrower range is a negative control.
        assert!(cold.is_empty());
    }

    /// A scan sought to other ranges (`Scan::reseek`), forward, backward, across files and
    /// memtables, over deletions and newer versions, gives what a new scan of each gives.
    #[test]
    fn reseek_reads_as_a_new_scan() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { memtable_bytes: 1 << 20, sync: false, ..Default::default() }).unwrap();
        let key = |i: u32| format!("r{i:05}").into_bytes();
        let rows = |from: u32, to: u32, step: u32, v: Option<&'static str>| -> Vec<(Vec<u8>, Option<Bytes>)> {
            (from..to).step_by(step as usize).map(|i| (key(i), v.map(|s| Bytes::from_static(s.as_bytes())))).collect()
        };
        s.commit(rows(0, 6000, 1, Some("a")), |_| Ok(())).unwrap();
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        s.commit(rows(1000, 3000, 3, None), |_| Ok(())).unwrap();
        s.flush_memtables().unwrap();
        s.commit(rows(2000, 5000, 7, Some("b")), |_| Ok(())).unwrap();
        let ts = s.visible_ts();
        // (Written after the snapshot: not seen.)
        s.commit(rows(0, 6000, 11, Some("c")), |_| Ok(())).unwrap();
        let read = |it: &mut Scan| it.map(|r| r.unwrap()).map(|(k, v)| (k, v.to_vec())).collect::<Vec<_>>();
        let ranges: &[(u32, u32)] = &[(10, 20), (20, 25), (900, 1200), (1199, 1300), (2995, 3100), (500, 520), (4990, 6100), (0, 3), (5999, 7000), (3000, 2999)];
        let mut it = s.scan_raw(&key(0), &key(1), ts);
        read(&mut it);
        for &(a, b) in ranges {
            let want = read(&mut s.scan_raw(&key(a), &key(b), ts));
            assert!(it.reseek(&key(a), &key(b), ts));
            assert_eq!(read(&mut it), want, "[{a}, {b})");
            // (A range half read, then another.)
            assert!(it.reseek(&key(a), &key(b), ts));
            it.next();
        }
        assert!(!it.reseek(&key(0), &key(9), ts + 1), "another snapshot");
    }

    #[test]
    fn scans_step_over_deleted_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { memtable_bytes: 1 << 20, sync: false, ..Default::default() }).unwrap();
        let key = |i: u32| format!("q{i:05}");
        let all: Vec<_> = (0..2000).map(|i| (key(i).into_bytes(), Some(Bytes::from_static(b"v")))).collect();
        s.commit(all, |_| Ok(())).unwrap();
        let old = s.visible_ts();
        let del: Vec<_> = (0..1500).map(|i| (key(i).into_bytes(), None)).collect();
        s.commit(del, |_| Ok(())).unwrap();
        let first = |ts: u64| -> Option<String> {
            let r = s.scan_raw(b"q", b"r", ts).next()?.unwrap();
            Some(String::from_utf8(r.0).unwrap())
        };
        let ts = s.visible_ts();
        assert_eq!(first(ts).as_deref(), Some("q01500"));
        assert_eq!(s.dead.skip(b"q", ts).as_deref(), Some(&b"q01500"[..]), "a hint recorded");
        assert_eq!(first(ts).as_deref(), Some("q01500"));
        assert_eq!(first(old).as_deref(), Some("q00000"), "an older snapshot sees the rows");
        // A row put back into the run, then deleted again; the head deleted one by one.
        let t = put(&s, "q00300", Some("w"));
        assert_eq!(first(t).as_deref(), Some("q00300"));
        put(&s, "q00300", None);
        put(&s, "q01500", None);
        let t = put(&s, "q01501", None);
        assert_eq!(first(t).as_deref(), Some("q01502"));
        assert_eq!(s.scan_raw(b"q", b"r", t).count(), 498);
        // Through flushes and compaction.
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        let t = s.visible_ts();
        assert_eq!(first(t).as_deref(), Some("q01502"));
        assert_eq!(s.scan_raw(b"q01000", b"q01600", t).count(), 98);
    }

    #[test]
    fn deleted_sst_runs_preserve_snapshots_and_merge_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions {
            memtable_bytes: 16 << 20,
            background: false,
            sync: false,
            ..Default::default()
        }).unwrap();
        let key = |i: u32| format!("q{i:05}").into_bytes();
        let mut model: Model = BTreeMap::new();
        let mut pins = Vec::new();
        for writes in [
            (0..2000).map(|i| (key(i), Some(Bytes::from_static(b"old")))).collect::<Vec<_>>(),
            (0..1700).map(|i| (key(i), None)).collect(),
            vec![(key(300), Some(Bytes::from_static(b"new"))), (key(700), Some(Bytes::from_static(b"new"))), (key(1700), None)],
            vec![(key(300), None), (key(699), Some(Bytes::from_static(b"last"))), (key(700), None), (key(1800), Some(Bytes::from_static(b"last")))],
        ] {
            let ts = s.commit(writes.clone(), |_| Ok(())).unwrap();
            for (k, v) in writes {
                model.entry(k).or_default().push((ts, v));
            }
            pins.push(s.pin(ts));
            // Distinct SSTs hold the old rows, their deletions, then resurrected rows;
            // the last commit remains in memory until the first check.
            if pins.len() < 4 {
                s.flush_memtables().unwrap();
            }
        }
        for stage in ["memtable", "flushed", "compacted"] {
            if stage == "flushed" {
                s.flush_memtables().unwrap();
            } else if stage == "compacted" {
                s.compact().unwrap();
            }
            for pin in &pins {
                for (start, end) in [(b"q".to_vec(), b"r".to_vec()), (key(300), key(700)), (key(0), key(1699)), (key(1800), b"r".to_vec())] {
                    let want: Vec<_> = model_at(&model, pin.ts).into_iter().filter(|(k, _)| k >= &start && k < &end).collect();
                    let got: Vec<_> = s.scan_raw(&start, &end, pin.ts).map(Result::unwrap).collect();
                    assert_eq!(got, want, "{stage}: iterator at {}", pin.ts);
                    for size in [1, 31, 1024] {
                        let mut scan = s.scan_raw(&start, &end, pin.ts);
                        let mut batch = batch::RowBatch::default();
                        let mut got = Vec::new();
                        loop {
                            scan.fill_batch(&mut batch, size).unwrap();
                            if batch.is_empty() { break; }
                            while let Some(row) = batch.take() { got.push(row); }
                            batch.clear();
                        }
                        assert_eq!(got, want, "{stage}: batch {size} at {}", pin.ts);
                    }
                }
            }
        }
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

    /// Rows updated after their source was written have their versions in two sources (and
    /// deleted ones a tombstone): batched scans read each once, at every snapshot, the
    /// newer source's rows and the older's interleaved, in memtables and files.
    #[test]
    fn updated_rows_read_once_across_sources() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { background: false, ..opts() }).unwrap();
        let mut model = Model::new();
        let write = |s: &Arc<Store>, model: &mut Model, i: u32, v: Option<String>| {
            let k = format!("row{i:05}");
            let ts = put(s, &k, v.as_deref());
            model.entry(k.into_bytes()).or_default().push((ts, v.map(Bytes::from)));
            ts
        };
        for i in 0..400 {
            write(&s, &mut model, i, Some(format!("v0-{i}")));
        }
        s.rotate().unwrap();
        s.flush_all().unwrap();
        let t0 = s.visible_ts();
        // Every other row updated (one in seven deleted), into a second file, then every third
        // in the memtable.
        for i in (0..400).step_by(2) {
            write(&s, &mut model, i, (i % 7 != 0).then(|| format!("v1-{i}")));
        }
        s.rotate().unwrap();
        s.flush_all().unwrap();
        let t1 = s.visible_ts();
        for i in (0..400).step_by(3) {
            write(&s, &mut model, i, Some(format!("v2-{i}")));
        }
        let t2 = s.visible_ts();
        for (ts, what) in [(t0, "first file"), (t1, "two files"), (t2, "files and memtable")] {
            check_at(&s, &model, ts, what);
        }
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
        // Batched (`fill_batch`: runs of a source's block, older versions in the next source
        // passed in the run).
        for max in [1, 3, 64] {
            let mut sc = s.scan(b"", b"", ts);
            let (mut b, mut got) = (batch::RowBatch::default(), Vec::new());
            loop {
                b.clear();
                sc.fill_batch(&mut b, max).unwrap();
                if b.is_empty() {
                    break;
                }
                got.extend((0..b.len()).map(|i| (b.get(i).0.to_vec(), b.value_bytes(i))));
            }
            assert_eq!(got, expect, "{what}: batches of {max} at {ts}");
        }
        let keys: Vec<&[u8]> = model.keys().map(|k| k.as_slice()).collect();
        let got = s.get_versions(&keys, ts).unwrap();
        for (k, g) in keys.iter().zip(got) {
            let want = model_at(&BTreeMap::from([(k.to_vec(), model[*k].clone())]), ts)
                .pop()
                .map(|e| e.1);
            assert_eq!(g.and_then(|(_, v)| v), want, "{what}: get_versions {k:?} at {ts}");
            assert_eq!(s.get(k, ts).unwrap(), want, "{what}: get {k:?} at {ts}");
        }
        // The first row of ranges (one point reader, ranges ascending and not): the first
        // row of a scan, or `Unknown` past deleted keys.
        let mut r = s.point_reader(ts, false);
        let bounds: Vec<Vec<u8>> = model.keys().flat_map(|k| [k.clone(), [k.as_slice(), &[0]].concat()]).collect();
        for (i, start) in bounds.iter().enumerate().chain(bounds.iter().enumerate().rev().step_by(3)) {
            for end in [bounds.get(i + 1).cloned().unwrap_or_default(), bounds.get(i + 7).cloned().unwrap_or_default(), Vec::new()] {
                let want = expect.iter().find(|(k, _)| k >= start && (end.is_empty() || k < &end));
                match s.first_with(&mut r, start, &end, true).unwrap() {
                    point::First::Row(k, v) => assert_eq!(Some(&(k, v)), want, "{what}: first in {start:?}..{end:?} at {ts}"),
                    point::First::None => assert_eq!(want, None, "{what}: first in {start:?}..{end:?} at {ts}"),
                    point::First::Unknown => {}
                }
            }
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

    /// Range deletions (`rdel`) committed among small and ingested commits: every read at
    /// every pinned snapshot is the model's (each covered key deleted at the deletion's ts),
    /// through flushes, compactions, the deleted ranges' collection and marker retirement,
    /// and crashes; and what they cover leaves the files.
    #[test]
    fn range_deletions_and_crash_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let o = || StoreOptions {
            memtable_bytes: 16 << 10,
            sync: false,
            background: false,
            l0_compaction_trigger: 3,
            target_file_bytes: 8 << 10,
            ..StoreOptions::default()
        };
        let mut s = Store::open(dir.path(), o()).unwrap();
        let mut model: Model = BTreeMap::new();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut pins: Vec<SnapshotPin> = Vec::new();
        let mut deleted = 0;
        for round in 0..60u64 {
            let mut writes: BTreeMap<Vec<u8>, Option<Bytes>> = BTreeMap::new();
            let big = rnd(4) == 0;
            for _ in 0..if big { 300 } else { 1 + rnd(8) } {
                let k = format!("k{:04}", rnd(600)).into_bytes();
                writes.insert(k, (rnd(6) != 0).then(|| Bytes::from(format!("v{round}-{}", "x".repeat(30)))));
            }
            // A range deletion now and then, with writes of its own in the range.
            let range = (rnd(3) == 0).then(|| {
                let a = rnd(550);
                (format!("k{a:04}").into_bytes(), format!("k{:04}", a + 1 + rnd(120)).into_bytes())
            });
            if let Some((a, b)) = &range {
                writes.insert(rdel::marker_key(a), Some(Bytes::from(b.clone())));
            }
            let writes: Vec<_> = writes.into_iter().collect();
            let ts = s.commit(writes.clone(), |_| Ok(())).unwrap();
            if let Some((a, b)) = &range {
                deleted += 1;
                for (k, vs) in model.range_mut::<[u8], _>((Bound::Included(a.as_slice()), Bound::Excluded(b.as_slice()))) {
                    if !writes.iter().any(|w| &w.0 == k) {
                        vs.push((ts, None));
                    }
                }
            }
            for (k, v) in writes {
                model.entry(k).or_default().push((ts, v));
            }
            match rnd(6) {
                0 => s.flush_all().unwrap(),
                1 | 2 => {
                    s.rotate().unwrap();
                    s.flush_all().unwrap();
                    s.maybe_compact().unwrap();
                    s.collect_ranges().unwrap();
                    // Retired markers: deleted at a ts of their own.
                    for (k, vs) in model.iter_mut().filter(|(k, _)| rdel::is_marker(k)) {
                        if let Some((t, None)) = s.get_version(k, u64::MAX >> 1).unwrap()
                            && vs.last().is_some_and(|l| l.0 < t)
                        {
                            vs.push((t, None));
                        }
                    }
                }
                3 => {
                    pins.clear();
                    let last = s.visible_ts();
                    drop(s);
                    s = Store::open(dir.path(), o()).unwrap();
                    assert_eq!(s.visible_ts(), last, "round {round}: recovered visible ts");
                }
                _ => {}
            }
            if rnd(4) == 0 {
                pins.push(s.pin_visible());
            }
            check_at(&s, &model, s.visible_ts(), &format!("round {round}"));
            for p in &pins {
                check_at(&s, &model, p.ts, &format!("round {round}, pinned"));
            }
        }
        assert!(deleted > 5);
        // With no snapshot left, collected: no file keeps a covered version, the markers go.
        pins.clear();
        s.rotate().unwrap();
        s.flush_all().unwrap();
        s.compact().unwrap();
        s.collect_ranges().unwrap();
        s.rotate().unwrap();
        s.flush_all().unwrap();
        s.compact().unwrap();
        s.collect_ranges().unwrap();
        assert!(s.view().rdels.is_empty(), "markers left: {:?}", s.view().rdels);
        let last = s.visible_ts();
        for (_, vs) in model.iter_mut().filter(|(k, _)| rdel::is_marker(k)) {
            vs.push((last, None));
        }
        check_at(&s, &model, last, "collected");
        s.close().unwrap();
        drop(s);
        let s = Store::open(dir.path(), o()).unwrap();
        check_at(&s, &model, last, "reopened");
    }

    /// Compaction rewrites only the L1 files overlapping L0's keys.
    /// A discarded range's entries leave with the next flush or compaction, unread; the
    /// rest stay, and reads never consult the list.
    #[test]
    fn discarded_ranges_leave_flushes_and_compactions() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { memtable_bytes: 1 << 20, sync: false, background: false, ..Default::default() }).unwrap();
        let put_range = |s: &Arc<Store>, keys: std::ops::Range<u32>| {
            let w: Vec<_> = keys.map(|i| (format!("d{i:05}").into_bytes(), Some(Bytes::from_static(b"v")))).collect();
            s.commit(w, |_| Ok(())).unwrap();
        };
        let keys = |s: &Store| -> Vec<String> {
            s.scan(b"d", b"e", s.visible_ts()).map(|r| String::from_utf8(r.unwrap().0).unwrap()).collect()
        };
        // Some in a file, the rest in the memtable.
        put_range(&s, 0..100);
        s.flush_memtables().unwrap();
        put_range(&s, 100..200);
        s.discard(b"d", b"d00150".to_vec());
        assert_eq!(keys(&s).len(), 200, "reads don't look at the list");
        s.flush_memtables().unwrap();
        let k = keys(&s);
        assert_eq!((k.len(), k[0].as_str(), k[100].as_str()), (150, "d00000", "d00150"), "the flush left 100..150 out");
        s.compact().unwrap();
        assert_eq!(keys(&s).first().map(String::as_str), Some("d00150"), "compaction left the file's out");
        // Everything in a memtable discarded: no file.
        let files = s.view().l0.len() + s.view().l1.len();
        put_range(&s, 150..160);
        s.discard(b"d", b"d00160".to_vec());
        s.flush_memtables().unwrap();
        assert_eq!(s.view().l0.len() + s.view().l1.len(), files);
        // (The file's older versions of them go with its next compaction, here of a file
        // whose keys overlap its.)
        put_range(&s, 170..175);
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        assert_eq!(keys(&s).first().map(String::as_str), Some("d00160"));
        // A file whose every key is discarded goes unread.
        s.discard(b"d", b"e".to_vec());
        s.collect_ranges().unwrap();
        assert_eq!(s.view().l0.len() + s.view().l1.len(), 0);
        assert!(keys(&s).is_empty());
    }

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

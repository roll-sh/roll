//! The node WAL: one append stream shared by every shard of every cluster on the node.
//!
//! Appends copy the operation into an in-memory batch under one short lock and get an [`Lsn`]
//! back. A dedicated flusher thread group-commits the batch: one `write` and one `fdatasync`
//! for every operation of every tenant buffered since the previous commit, which is what lets
//! indexing outrun Elasticsearch's per-request translog fsync. Commits are on demand: as soon
//! as someone waits for a buffered record ([`Wal::durable`], [`Wal::wait_ack`]) the flusher
//! commits, back to back with the previous commit, so the batch is whatever arrived during the
//! last fsync. A lone client pays one fsync of latency, sixteen clients share each fsync, and a
//! bulk request's thousand records go out in one commit when it waits for the last one. While
//! fsyncs are fast (local NVMe, tmpfs) and no commit is running, a waiter commits the batch on
//! its own thread instead (leader commit): handing the batch to the flusher thread and being
//! woken by it again would cost more than the fsync itself. Records
//! nobody waits for (`translog.durability: async`) are committed `flush_delay` (5 ms) after the
//! first of them, or once `flush_bytes` (4 MiB) are buffered.
//!
//! The batches go into local files (`<seq:016>.wal`, see [`record`] for the format). A file is
//! sealed once it holds `file_bytes` (64 MiB) or has been open for `ship_interval` (1 s), and a
//! shipper task uploads sealed files to `search/wal/<node>/<seq>.zst`, in order, a few at a time.
//! [`Wal::durable_remote`] resolves once an LSN is in object storage. With `sync_objects`
//! (nodes on ephemeral disks) the flusher seals after every group commit whenever an upload
//! slot is free, so object-store latency itself becomes the group-commit window, and
//! [`Wal::wait_ack`] waits for object storage.
//!
//! Each shard registers once ([`Wal::shard`]) and appends through its [`WalShard`] handle, so
//! the hot path does no hashing. The WAL remembers, per shard, which files hold its operations
//! that are not yet in a flushed commit point; [`WalShard::trim`] (called by the store after a
//! manifest is written) forgets them, and local files that are shipped and needed by no shard
//! are deleted. Object-store copies are deleted by garbage collection once no manifest's WAL
//! range covers them.
//!
//! Disk: unshipped bytes are capped by `max_local_bytes`; past it appends fail with
//! [`WalFull`] (a long object-store outage must not fill the disk), and shipped files that shards
//! would like to keep for fast local replay are dropped early instead (replay falls back to the
//! objects).
//!
//! Fencing: when the store finds that a newer owner wrote a cluster, it fences the cluster here
//! and every append for it fails with [`Fenced`] until [`crate::Store::set_epoch`] moves this node
//! to an epoch at least that new.
//!
//! Idle cost: with nothing buffered and no unsealed data the flusher thread sleeps on a condvar
//! without a timeout and the shipper awaits its channel, so an idle node runs no timer.
//!
//! LSNs number appends within one process (starting at 1); they order acknowledgements, not
//! history. History across restarts and nodes is ordered by `(node, file seq)` and, per shard,
//! by `seq_no`.

pub mod object;
pub mod record;
mod replay;

pub(crate) use replay::{Listings, run_end};

pub use record::WalRecord;
pub use replay::ReplayStats;

use crate::error::{Fenced, WalFull};
use crate::keys::{self, ShardId};
use crate::now_ms;
use anyhow::{Context, bail};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use parking_lot::{Condvar, Mutex};
use pg_storage::Objects;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Position of an append in this process's WAL stream.
pub type Lsn = u64;

#[derive(Clone, Debug)]
pub struct WalConfig {
    /// The longest records nobody waits for stay buffered (they are committed this long after
    /// the first of them). Records someone waits for are committed at once. Zero commits every
    /// record at once (appends arriving during an fsync still share the next one).
    pub flush_delay: Duration,
    /// Commit early once this much is buffered.
    pub flush_bytes: usize,
    /// Seal a local file once it reaches this size…
    pub file_bytes: u64,
    /// …or has been open this long: the most acknowledged data a lost local disk can take with it.
    pub ship_interval: Duration,
    /// Acknowledge writes only once they are in object storage ([`Wal::wait_ack`]).
    pub sync_objects: bool,
    /// fsync each group commit. Off only for tests and throwaway nodes.
    pub fsync: bool,
    /// Let a waiter commit the batch itself while fsyncs are fast (see the module docs).
    pub leader_commits: bool,
    /// Appends block while this much is buffered and not yet written (backpressure).
    pub max_pending_bytes: usize,
    /// Uploads of sealed files in flight at once.
    pub ship_concurrency: usize,
    pub zstd_level: i32,
    /// WAL objects kept in memory while replays run, shared between them.
    pub replay_cache_bytes: usize,
    /// Local bytes not yet in object storage past which appends fail with [`WalFull`] (0: no
    /// limit). Also the most local disk shipped files may keep for fast replay.
    pub max_local_bytes: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            flush_delay: Duration::from_millis(5),
            flush_bytes: 4 << 20,
            file_bytes: 64 << 20,
            ship_interval: Duration::from_secs(1),
            sync_objects: false,
            fsync: true,
            leader_commits: true,
            max_pending_bytes: 64 << 20,
            ship_concurrency: 4,
            zstd_level: 1,
            replay_cache_bytes: 256 << 20,
            max_local_bytes: 8 << 30,
        }
    }
}

/// A run of one node's WAL files that may hold a shard's operations newer than its commit point.
/// Kept in shard manifests (maintained by the store, not by callers).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalSource {
    /// The node whose `search/wal/<node>/` files these are.
    pub node: String,
    /// The cluster name the records were written under (a branch replays its parent's records).
    pub cluster: String,
    /// First file seq to read.
    pub from: u64,
    /// Last file seq to read; open-ended while the node still writes the shard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<u64>,
    /// Ignore records appended after this Unix ms (point-in-time branches, and branches in
    /// general so they never see their parent's later writes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Ignore records appended before this Unix ms. A range opened by an attachment starts
    /// there, not at the start of its first file: that file may hold older records of the
    /// shard that its history doesn't include (a deposed owner's, appended before it noticed
    /// another node took over, with seq_nos the other node reused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// Highest seq_no of the shard found in the range, once it was replayed. A commit point at
    /// or past it makes the source unnecessary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seq_no: Option<i64>,
    /// Inclusive runs of file seqs in `from..=to` that did not exist when the range was frozen
    /// by another node. Never read, even if they show up later (a slow upload by a deposed
    /// owner): every replay of the range sees the same history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<[u64; 2]>,
    /// Every file in `from..=to` outside `skip` existed when the range was frozen (from a
    /// listing), so one missing at replay means acknowledged operations were lost: replay fails
    /// rather than leave a hole in the shard's history. Ranges closed without a listing (by
    /// the node that wrote them, or spanning a crash) may have legitimate gaps and aren't exact.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exact: bool,
    /// The cluster's ownership epoch the range was opened under: only records appended under
    /// it are the shard's history (a deposed owner's records carry its older epoch, whenever
    /// and wherever they land). `None`: no leases, every record counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
}

impl WalSource {
    /// A range of `node`'s files from `from` on, open-ended.
    pub(crate) fn open(node: &str, cluster: &str, from: u64) -> Self {
        Self { node: node.into(), cluster: cluster.into(), from, to: None, until_ms: None, since_ms: None, max_seq_no: None, skip: Vec::new(), exact: false, epoch: None }
    }

    pub(crate) fn skips(&self, seq: u64) -> bool {
        self.skip.iter().any(|&[a, b]| (a..=b).contains(&seq))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mark {
    lsn: Lsn,
    failed: bool,
}

/// Counters, all monotonic except the gauges noted.
#[derive(Default)]
pub(crate) struct WalCounters {
    pub commits: AtomicU64,
    pub commit_bytes: AtomicU64,
    pub fsync_nanos: AtomicU64,
    pub files_sealed: AtomicU64,
    pub files_shipped: AtomicU64,
    pub shipped_raw_bytes: AtomicU64,
    pub shipped_stored_bytes: AtomicU64,
    pub ship_retries: AtomicU64,
    pub files_trimmed: AtomicU64,
    pub replayed_records: AtomicU64,
    pub replayed_files: AtomicU64,
    pub full_rejections: AtomicU64,
}

/// A snapshot of the WAL's counters and gauges.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WalStats {
    pub appends: u64,
    pub append_bytes: u64,
    pub commits: u64,
    pub commit_bytes: u64,
    pub fsync_nanos: u64,
    pub files_sealed: u64,
    pub files_shipped: u64,
    pub shipped_raw_bytes: u64,
    pub shipped_stored_bytes: u64,
    pub ship_retries: u64,
    pub files_trimmed: u64,
    pub replayed_records: u64,
    pub replayed_files: u64,
    /// WAL objects downloaded by replays, and object reads served by another replay's download.
    pub replay_fetches: u64,
    pub replay_cache_hits: u64,
    /// Ranged reads of single shards' blocks in large WAL objects, and bytes replay downloaded.
    pub replay_block_fetches: u64,
    pub replay_fetched_bytes: u64,
    /// Gauges.
    pub replay_cache_bytes: u64,
    pub pending_bytes: u64,
    pub local_files: u64,
    pub local_bytes: u64,
    pub unshipped_files: u64,
    pub unshipped_bytes: u64,
    /// Appends refused because the local disk limit was reached.
    pub full_rejections: u64,
    pub shards: u64,
    pub durable_lsn: Lsn,
    pub remote_lsn: Lsn,
    pub last_lsn: Lsn,
}

/// What a cluster's attached shards hold in the WAL beyond their commit points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ClusterWalStats {
    pub shards: u64,
    pub uncommitted_ops: u64,
    pub uncommitted_bytes: u64,
    pub oldest_ms: Option<u64>,
}

/// A registered shard with operations no commit point covers yet: it keeps the node's WAL files
/// from `first_file` on (locally until they ship, and in object storage until it flushes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PinningShard {
    pub cluster: String,
    pub shard: ShardId,
    pub first_file: u64,
    pub stats: ShardWalStats,
}

/// What a shard has in the WAL that its last commit point doesn't cover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ShardWalStats {
    pub uncommitted_ops: u64,
    pub uncommitted_bytes: u64,
    /// Unix ms of the oldest uncommitted operation (Elasticsearch's `earliest_last_modified_age`
    /// is now minus this).
    pub oldest_ms: Option<u64>,
    /// WAL files the shard keeps (locally and for replay).
    pub files: u64,
}

#[derive(Debug)]
struct FileUse {
    file: u64,
    max_seq_no: i64,
    ops: u64,
    bytes: u64,
    first_ms: u64,
}

#[derive(Hash, PartialEq, Eq)]
struct SlotKey {
    cluster: Box<str>,
    uuid: Box<str>,
    shard: u32,
}

#[derive(Hash)]
struct KeyRef<'a> {
    cluster: &'a str,
    uuid: &'a str,
    shard: u32,
}

impl hashbrown::Equivalent<SlotKey> for KeyRef<'_> {
    fn equivalent(&self, k: &SlotKey) -> bool {
        *k.cluster == *self.cluster && *k.uuid == *self.uuid && k.shard == self.shard
    }
}

struct Slot {
    cluster: Box<str>,
    uuid: Box<str>,
    shard: u32,
    handles: usize,
    files: VecDeque<FileUse>,
    /// The cluster was fenced (see [`State::fenced`]).
    fenced: bool,
}

struct State {
    buf: Vec<u8>,
    spare: Vec<u8>,
    next_lsn: Lsn,
    batch_first_lsn: Lsn,
    /// When the first record of the buffered batch was appended.
    batch_started: Option<Instant>,
    /// The newest LSN someone waits to be on local disk ([`Wal::durable`]).
    wanted: Lsn,
    last_ms: u64,
    /// Something is buffered since the last commit (the flusher was woken for it).
    pending: bool,
    /// The file buffered records go to.
    cur_file: u64,
    cur_bytes: u64,
    /// When the current file got its first batch (age-based sealing).
    cur_opened: Option<Instant>,
    sealed_lsn: Lsn,
    seal: bool,
    flush_now: bool,
    closing: bool,
    abandon: bool,
    failed: Option<String>,
    ship_inflight: usize,
    /// Append counters live under the lock the append holds anyway (no shared atomics on the
    /// hot path).
    appends: u64,
    append_bytes: u64,
    slots: Vec<Option<Slot>>,
    free: Vec<usize>,
    index: hashbrown::HashMap<SlotKey, usize>,
    /// Fenced clusters: `(this node's epoch, the newer owner's)`.
    fenced: std::collections::HashMap<Box<str>, (u64, u64)>,
}

#[derive(Clone, Copy, Debug)]
struct LocalFile {
    /// Bytes on disk known to be intact (synced, or validated at startup).
    len: u64,
    sealed: bool,
    shipped: bool,
}

enum ShipMsg {
    Ship { seq: u64, last_lsn: Lsn, recovered: bool },
    Stop { drain: bool },
}

pub(crate) struct WalShared {
    pub(crate) node: String,
    pub(crate) objects: Objects,
    dir: PathBuf,
    cfg: WalConfig,
    st: Mutex<State>,
    /// The local file being written. Whoever commits a batch holds it from taking the batch out
    /// of [`State::buf`] until the batch is on disk, so batches reach disk in LSN order (lock
    /// order: `io`, then `st`).
    io: Mutex<Option<OpenFile>>,
    /// Recent fsync latency (EWMA, ns; 0: none measured yet). Waiters commit inline only while
    /// fsyncs are fast ([`INLINE_FSYNC_NANOS`]).
    fsync_ewma: AtomicU64,
    wake: Condvar,
    room: Condvar,
    durable: watch::Sender<Mark>,
    remote: watch::Sender<Mark>,
    local: Mutex<BTreeMap<u64, LocalFile>>,
    /// Bytes of local files (the one being written included) not yet in object storage.
    unshipped: AtomicU64,
    /// The first file seq this process writes: lower ones are from earlier runs.
    pub(crate) first_file: u64,
    pub(crate) counters: WalCounters,
    ship_tx: mpsc::UnboundedSender<ShipMsg>,
    replay_cache: replay::ReplayCache,
    flusher_done: watch::Sender<bool>,
    shipper_done: watch::Sender<bool>,
}

/// The node's WAL. Cheap to clone.
#[derive(Clone)]
pub struct Wal {
    pub(crate) sh: Arc<WalShared>,
}

/// A shard's registration with the WAL: appends go through it.
pub struct WalShard {
    sh: Arc<WalShared>,
    slot: usize,
    cluster: Arc<str>,
    shard: ShardId,
    /// Stamped into every record ([`record`]).
    epoch: u64,
}

thread_local! {
    /// Per-thread buffer appends encode into before taking the WAL lock.
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A scratch buffer grown past this by a large operation is freed rather than kept.
const SCRATCH_KEEP: usize = 1 << 20;

fn file_name(seq: u64) -> String {
    format!("{seq:016}.wal")
}

fn parse_file_name(name: &str) -> Option<u64> {
    let s = name.strip_suffix(".wal")?;
    if s.len() != 16 {
        return None;
    }
    s.parse().ok()
}

fn sync_dir(dir: &Path) {
    // Makes a new file's directory entry durable; failure only matters on exotic filesystems.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

impl Wal {
    /// Open the WAL in `dir`: validate and truncate torn tails of files left by an earlier run,
    /// queue them for shipping, and start the flusher thread and shipper task. Needs a Tokio
    /// runtime.
    pub(crate) async fn open(node: &str, objects: Objects, dir: PathBuf, cfg: WalConfig) -> anyhow::Result<Wal> {
        anyhow::ensure!(cfg.ship_concurrency > 0, "ship_concurrency must be positive");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let scan_dir = dir.clone();
        let recovered = tokio::task::spawn_blocking(move || recover_dir(&scan_dir)).await??;
        let (mut remote_max, mut last_run) = (0, 0);
        for (k, _) in objects.list(&keys::wal_prefix(node)).await? {
            match (keys::parse_wal_key(&k), keys::parse_run_key(&k)) {
                (Some((n, seq)), _) if n == node => remote_max = remote_max.max(seq),
                (_, Some((n, first))) if n == node => last_run = last_run.max(first),
                _ => {}
            }
        }
        let local_max = recovered.keys().next_back().copied().unwrap_or(0);
        // Past every earlier run's start too: once garbage collection deleted a run's files,
        // numbering could otherwise restart below an existing run marker, which would end this
        // run's own ranges early (markers must grow with time).
        let first_file = remote_max.max(local_max).max(last_run) + 1;
        // Mark where this run starts before appending anything: an earlier run that died
        // without closing its shards' ranges wrote only files below it, so garbage collection
        // and replay can end those ranges here instead of reading (and keeping) every file
        // this node writes from now on.
        let marker = serde_json::to_vec(&serde_json::json!({ "first_file": first_file, "started_ms": now_ms() }))?;
        objects.put(&keys::run_key(node, first_file), marker.into()).await.context("writing the WAL run marker")?;
        let (ship_tx, ship_rx) = mpsc::unbounded_channel();
        let replay_budget = cfg.replay_cache_bytes;
        let sh = Arc::new(WalShared {
            node: node.to_string(),
            objects,
            dir,
            st: Mutex::new(State {
                buf: Vec::with_capacity(cfg.flush_bytes.min(1 << 20)),
                spare: Vec::new(),
                next_lsn: 1,
                batch_first_lsn: 1,
                batch_started: None,
                wanted: 0,
                last_ms: 0,
                pending: false,
                cur_file: first_file,
                cur_bytes: 0,
                cur_opened: None,
                sealed_lsn: 0,
                seal: false,
                flush_now: false,
                closing: false,
                abandon: false,
                failed: None,
                ship_inflight: 0,
                appends: 0,
                append_bytes: 0,
                slots: Vec::new(),
                free: Vec::new(),
                index: hashbrown::HashMap::new(),
                fenced: Default::default(),
            }),
            cfg,
            io: Mutex::new(None),
            fsync_ewma: AtomicU64::new(0),
            wake: Condvar::new(),
            room: Condvar::new(),
            durable: watch::Sender::new(Mark::default()),
            remote: watch::Sender::new(Mark::default()),
            unshipped: AtomicU64::new(recovered.values().map(|f| f.len).sum()),
            local: Mutex::new(recovered.clone()),
            first_file,
            counters: WalCounters::default(),
            ship_tx,
            replay_cache: replay::ReplayCache::new(replay_budget),
            flusher_done: watch::Sender::new(false),
            shipper_done: watch::Sender::new(false),
        });
        for seq in recovered.keys() {
            let _ = sh.ship_tx.send(ShipMsg::Ship { seq: *seq, last_lsn: 0, recovered: true });
        }
        tokio::spawn(run_shipper(sh.clone(), ship_rx));
        let f = sh.clone();
        std::thread::Builder::new().name("search-wal".into()).spawn(move || run_flusher(f))?;
        Ok(Wal { sh })
    }

    pub fn node(&self) -> &str {
        &self.sh.node
    }

    pub fn config(&self) -> &WalConfig {
        &self.sh.cfg
    }

    /// Register a shard for appending (or get another handle to an existing registration).
    pub fn shard(&self, cluster: &str, shard: &ShardId) -> anyhow::Result<WalShard> {
        self.shard_at(cluster, shard, 0)
    }

    /// [`Wal::shard`] for a cluster owned at `epoch`: every record the handle appends carries
    /// it (see [`WalSource::epoch`]).
    pub fn shard_at(&self, cluster: &str, shard: &ShardId, epoch: u64) -> anyhow::Result<WalShard> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(shard)?;
        let mut st = self.sh.st.lock();
        let key = KeyRef { cluster, uuid: &shard.index_uuid, shard: shard.shard };
        let slot = match st.index.get(&key) {
            Some(&i) => i,
            None => {
                let fenced = st.fenced.contains_key(cluster);
                let s = Slot { cluster: cluster.into(), uuid: shard.index_uuid.as_str().into(), shard: shard.shard, handles: 0, files: VecDeque::new(), fenced };
                let i = match st.free.pop() {
                    Some(i) => {
                        st.slots[i] = Some(s);
                        i
                    }
                    None => {
                        st.slots.push(Some(s));
                        st.slots.len() - 1
                    }
                };
                st.index.insert(SlotKey { cluster: cluster.into(), uuid: shard.index_uuid.as_str().into(), shard: shard.shard }, i);
                i
            }
        };
        st.slots[slot].as_mut().expect("indexed slot").handles += 1;
        Ok(WalShard { sh: self.sh.clone(), slot, cluster: cluster.into(), shard: shard.clone(), epoch })
    }

    /// Whether a shard currently has a live handle.
    pub fn is_attached(&self, cluster: &str, shard: &ShardId) -> bool {
        let st = self.sh.st.lock();
        let key = KeyRef { cluster, uuid: &shard.index_uuid, shard: shard.shard };
        st.index.get(&key).and_then(|&i| st.slots[i].as_ref()).is_some_and(|s| s.handles > 0)
    }

    /// Append one operation for a shard (looks the shard up; prefer [`WalShard::append`]).
    pub fn append(&self, cluster: &str, shard: &ShardId, seq_no: i64, op: &[u8]) -> anyhow::Result<Lsn> {
        self.shard(cluster, shard)?.append(seq_no, op)
    }

    /// Fails with [`WalFull`] when appends would be refused for lack of room (object storage
    /// is behind): callers check before applying an operation they then must log.
    pub fn check_room(&self) -> anyhow::Result<()> {
        let cap = self.sh.cfg.max_local_bytes;
        let unshipped = self.sh.unshipped.load(Relaxed);
        if cap > 0 && unshipped >= cap {
            return Err(WalFull { unshipped_bytes: unshipped, limit: cap }.into());
        }
        Ok(())
    }

    /// The last LSN handed out (0 before the first append).
    pub fn last_lsn(&self) -> Lsn {
        self.sh.st.lock().next_lsn - 1
    }

    /// Everything up to this LSN is on local disk.
    pub fn durable_lsn(&self) -> Lsn {
        self.sh.durable.borrow().lsn
    }

    /// Everything up to this LSN is in object storage.
    pub fn remote_lsn(&self) -> Lsn {
        self.sh.remote.borrow().lsn
    }

    /// Resolves once `lsn` is on local disk (fsynced). A buffered record someone waits for is
    /// committed at once (see the module docs).
    pub async fn durable(&self, lsn: Lsn) -> anyhow::Result<()> {
        if self.sh.durable.borrow().lsn < lsn {
            if self.sh.commit_inline(lsn) {
                return wait_mark(&self.sh.durable, lsn, &self.sh).await;
            }
            let mut st = self.sh.st.lock();
            if lsn > st.wanted {
                st.wanted = lsn;
                // Not buffered any more: the flusher is writing it.
                if st.pending {
                    self.sh.wake.notify_one();
                }
            }
        }
        wait_mark(&self.sh.durable, lsn, &self.sh).await
    }

    /// Resolves once `lsn` is in object storage. Asks for the file holding it to be sealed now
    /// rather than at the next `ship_interval`.
    pub async fn durable_remote(&self, lsn: Lsn) -> anyhow::Result<()> {
        if self.sh.remote.borrow().lsn >= lsn {
            return Ok(());
        }
        {
            let mut st = self.sh.st.lock();
            anyhow::ensure!(lsn < st.next_lsn, "LSN {lsn} has not been appended yet");
            if lsn > st.sealed_lsn {
                st.seal = true;
                self.sh.wake.notify_one();
            }
        }
        wait_mark(&self.sh.remote, lsn, &self.sh).await
    }

    /// Resolves when an append is acknowledged under the configured mode: in object storage
    /// with `sync_objects`, on local disk otherwise. This is what `index.translog.durability:
    /// request` waits for.
    pub async fn wait_ack(&self, lsn: Lsn) -> anyhow::Result<()> {
        if self.sh.cfg.sync_objects { self.durable_remote(lsn).await } else { self.durable(lsn).await }
    }

    /// Commit what's buffered now instead of at the end of the group-commit window.
    pub fn flush_now(&self) {
        let mut st = self.sh.st.lock();
        if !st.buf.is_empty() {
            st.flush_now = true;
            self.sh.wake.notify_one();
        }
    }

    /// Commit, seal and ship everything appended so far; resolves once it's in object storage.
    pub async fn sync_remote(&self) -> anyhow::Result<()> {
        let lsn = self.last_lsn();
        if lsn == 0 {
            return Ok(());
        }
        self.durable_remote(lsn).await
    }

    /// A shard's operations up to `checkpoint` are in a commit point (see [`WalShard::trim`]).
    pub fn trim_shard(&self, cluster: &str, shard: &ShardId, checkpoint: i64) {
        {
            let mut st = self.sh.st.lock();
            let key = KeyRef { cluster, uuid: &shard.index_uuid, shard: shard.shard };
            let Some(&i) = st.index.get(&key) else { return };
            if let Some(slot) = st.slots[i].as_mut() {
                while slot.files.front().is_some_and(|f| f.max_seq_no <= checkpoint) {
                    slot.files.pop_front();
                }
            }
            release_slot_if_unused(&mut st, i);
        }
        self.sh.trim_local();
    }

    /// Refuse appends for `cluster`: a newer owner (`owner_epoch`) wrote it.
    pub(crate) fn fence_cluster(&self, cluster: &str, epoch: u64, owner_epoch: u64) {
        let mut guard = self.sh.st.lock();
        let st = &mut *guard;
        let e = st.fenced.entry(cluster.into()).or_insert((epoch, owner_epoch));
        *e = (epoch, e.1.max(owner_epoch));
        for s in st.slots.iter_mut().flatten().filter(|s| &*s.cluster == cluster) {
            s.fenced = true;
        }
    }

    /// Lift a fence once this node writes `cluster` under an epoch at least as new as the owner's.
    pub(crate) fn unfence_cluster(&self, cluster: &str, epoch: u64) {
        let mut guard = self.sh.st.lock();
        let st = &mut *guard;
        if st.fenced.get(cluster).is_some_and(|&(_, owner)| owner <= epoch) {
            st.fenced.remove(cluster);
            for s in st.slots.iter_mut().flatten().filter(|s| &*s.cluster == cluster) {
                s.fenced = false;
            }
        }
    }

    /// What `cluster`'s registered shards hold beyond their commit points (memory and replay
    /// accounting per tenant).
    pub fn cluster_stats(&self, cluster: &str) -> ClusterWalStats {
        let st = self.sh.st.lock();
        let mut out = ClusterWalStats::default();
        for s in st.slots.iter().flatten().filter(|s| &*s.cluster == cluster) {
            out.shards += 1;
            for f in &s.files {
                out.uncommitted_ops += f.ops;
                out.uncommitted_bytes += f.bytes;
                out.oldest_ms = Some(out.oldest_ms.map_or(f.first_ms, |m| m.min(f.first_ms)));
            }
        }
        out
    }

    /// Shards holding uncommitted operations, the ones pinning the oldest WAL file first: what
    /// search-server flushes when the WAL grows (`stats().local_bytes`) or ages, besides
    /// Elasticsearch's per-shard `index.translog.flush_threshold_size`.
    pub fn pinning_shards(&self) -> Vec<PinningShard> {
        let st = self.sh.st.lock();
        let mut out: Vec<PinningShard> = st
            .slots
            .iter()
            .flatten()
            .filter_map(|s| {
                let first_file = s.files.front()?.file;
                let mut stats = ShardWalStats { files: s.files.len() as u64, ..Default::default() };
                for f in &s.files {
                    stats.uncommitted_ops += f.ops;
                    stats.uncommitted_bytes += f.bytes;
                    stats.oldest_ms = Some(stats.oldest_ms.map_or(f.first_ms, |m: u64| m.min(f.first_ms)));
                }
                Some(PinningShard { cluster: s.cluster.to_string(), shard: ShardId::new(&*s.uuid, s.shard), first_file, stats })
            })
            .collect();
        drop(st);
        out.sort_by_key(|p| p.first_file);
        out
    }

    /// Start closing without waiting (the store was dropped).
    pub(crate) fn begin_close(&self) {
        let mut st = self.sh.st.lock();
        st.closing = true;
        self.sh.wake.notify_all();
        self.sh.room.notify_all();
    }

    /// Forget a shard's uncommitted operations (its index was deleted): they no longer keep
    /// local WAL files around.
    pub fn forget_shard(&self, cluster: &str, shard: &ShardId) {
        {
            let mut st = self.sh.st.lock();
            let key = KeyRef { cluster, uuid: &shard.index_uuid, shard: shard.shard };
            if let Some(&i) = st.index.get(&key) {
                release_slot(&mut st, i, true);
            }
        }
        self.sh.trim_local();
    }

    /// Forget every shard of a cluster (the cluster was deleted).
    pub fn forget_cluster(&self, cluster: &str) {
        {
            let mut st = self.sh.st.lock();
            let ids: Vec<usize> = st.index.iter().filter(|(k, _)| &*k.cluster == cluster).map(|(_, &i)| i).collect();
            for i in ids {
                release_slot(&mut st, i, true);
            }
        }
        self.sh.trim_local();
    }

    pub fn stats(&self) -> WalStats {
        let c = &self.sh.counters;
        let (pending, shards, last, appends, append_bytes) = {
            let st = self.sh.st.lock();
            (st.buf.len() as u64, st.index.len() as u64, st.next_lsn - 1, st.appends, st.append_bytes)
        };
        let (mut local_files, mut local_bytes, mut unshipped) = (0, 0, 0);
        for f in self.sh.local.lock().values() {
            local_files += 1;
            local_bytes += f.len;
            unshipped += u64::from(!f.shipped);
        }
        WalStats {
            appends,
            append_bytes,
            commits: c.commits.load(Relaxed),
            commit_bytes: c.commit_bytes.load(Relaxed),
            fsync_nanos: c.fsync_nanos.load(Relaxed),
            files_sealed: c.files_sealed.load(Relaxed),
            files_shipped: c.files_shipped.load(Relaxed),
            shipped_raw_bytes: c.shipped_raw_bytes.load(Relaxed),
            shipped_stored_bytes: c.shipped_stored_bytes.load(Relaxed),
            ship_retries: c.ship_retries.load(Relaxed),
            files_trimmed: c.files_trimmed.load(Relaxed),
            replayed_records: c.replayed_records.load(Relaxed),
            replayed_files: c.replayed_files.load(Relaxed),
            replay_fetches: self.sh.replay_cache.fetches.load(Relaxed),
            replay_cache_hits: self.sh.replay_cache.hits.load(Relaxed),
            replay_block_fetches: self.sh.replay_cache.block_fetches.load(Relaxed),
            replay_fetched_bytes: self.sh.replay_cache.fetched_bytes.load(Relaxed),
            replay_cache_bytes: self.sh.replay_cache.used() as u64,
            pending_bytes: pending,
            local_files,
            local_bytes,
            unshipped_files: unshipped,
            unshipped_bytes: self.sh.unshipped.load(Relaxed),
            full_rejections: c.full_rejections.load(Relaxed),
            shards,
            durable_lsn: self.durable_lsn(),
            remote_lsn: self.remote_lsn(),
            last_lsn: last,
        }
    }

    /// Commit and ship everything, then stop the flusher and shipper. Appends fail afterwards.
    pub async fn close(&self) -> anyhow::Result<()> {
        {
            let mut st = self.sh.st.lock();
            st.closing = true;
            self.sh.wake.notify_all();
            self.sh.room.notify_all();
        }
        let mut f = self.sh.flusher_done.subscribe();
        f.wait_for(|d| *d).await?;
        let mut s = self.sh.shipper_done.subscribe();
        s.wait_for(|d| *d).await?;
        if let Some(e) = &self.sh.st.lock().failed {
            bail!("WAL failed: {e}");
        }
        Ok(())
    }

    /// Simulate a crash: stop at once, dropping whatever isn't committed and leaving sealed files
    /// unshipped. For tests of recovery.
    #[doc(hidden)]
    pub async fn abandon(&self) {
        {
            let mut st = self.sh.st.lock();
            st.abandon = true;
            self.sh.wake.notify_all();
            self.sh.room.notify_all();
        }
        let mut f = self.sh.flusher_done.subscribe();
        let _ = f.wait_for(|d| *d).await;
        let mut s = self.sh.shipper_done.subscribe();
        let _ = s.wait_for(|d| *d).await;
    }

    /// Path of a local WAL file (tests).
    #[doc(hidden)]
    pub fn local_path(&self, seq: u64) -> PathBuf {
        self.sh.dir.join(file_name(seq))
    }

    /// Seqs of the local WAL files.
    #[doc(hidden)]
    pub fn local_files(&self) -> Vec<u64> {
        self.sh.local.lock().keys().copied().collect()
    }

    /// A Unix ms that every record appended from now on is stamped at or after, and every
    /// record appended before is stamped before (append times never decrease: a late clock is
    /// restamped under the lock).
    pub(crate) fn mark_ms(&self) -> u64 {
        let mut st = self.sh.st.lock();
        let mark = now_ms().max(st.last_ms + 1);
        st.last_ms = mark;
        mark
    }

    /// The file seq new appends go to.
    pub(crate) fn current_file(&self) -> u64 {
        self.sh.st.lock().cur_file
    }

    /// For a shard's manifest written with this checkpoint: the first file that may hold one of
    /// its operations newer than the checkpoint.
    pub(crate) fn replay_from(&self, cluster: &str, shard: &ShardId, checkpoint: i64) -> u64 {
        let st = self.sh.st.lock();
        let key = KeyRef { cluster, uuid: &shard.index_uuid, shard: shard.shard };
        st.index
            .get(&key)
            .and_then(|&i| st.slots[i].as_ref())
            .and_then(|s| s.files.iter().find(|f| f.max_seq_no > checkpoint).map(|f| f.file))
            .unwrap_or(st.cur_file)
    }
}

fn release_slot_if_unused(st: &mut State, i: usize) {
    if st.slots[i].as_ref().is_some_and(|s| s.handles == 0 && s.files.is_empty()) {
        let slot = st.slots[i].take().expect("slot");
        st.index.remove(&KeyRef { cluster: &slot.cluster, uuid: &slot.uuid, shard: slot.shard });
        st.free.push(i);
    }
}

/// Drop a slot's handle count (or all of it with `forget`), removing it once unused and empty.
fn release_slot(st: &mut State, i: usize, forget: bool) {
    let Some(slot) = st.slots[i].as_mut() else { return };
    if forget {
        slot.files.clear();
    } else {
        slot.handles = slot.handles.saturating_sub(1);
    }
    release_slot_if_unused(st, i);
}

async fn wait_mark(tx: &watch::Sender<Mark>, lsn: Lsn, sh: &WalShared) -> anyhow::Result<()> {
    let mut rx = tx.subscribe();
    let m = *rx.wait_for(|m| m.lsn >= lsn || m.failed).await.map_err(|_| anyhow::anyhow!("WAL stopped"))?;
    if m.lsn >= lsn {
        return Ok(());
    }
    let e = sh.st.lock().failed.clone().unwrap_or_else(|| "stopped".into());
    bail!("WAL failed: {e}")
}

impl WalShard {
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    pub fn shard_id(&self) -> &ShardId {
        &self.shard
    }

    /// Append one operation. Returns its LSN; await [`Wal::wait_ack`] (or [`Wal::durable`])
    /// before acknowledging it. Blocks briefly when `max_pending_bytes` are already buffered.
    /// Fails with [`WalFull`] or [`Fenced`] (see the module docs).
    #[inline]
    pub fn append(&self, seq_no: i64, op: &[u8]) -> anyhow::Result<Lsn> {
        self.append_many(&[(seq_no, op)])
    }

    /// Append several operations of this shard (a bulk request's) under one lock acquisition.
    /// They land in the same group commit and the same file, so they become durable together.
    /// Returns the LSN of the last one (0 for none). Either all are appended or none is.
    pub fn append_many<B: AsRef<[u8]>>(&self, ops: &[(i64, B)]) -> anyhow::Result<Lsn> {
        let sh = &*self.sh;
        let cap = sh.cfg.max_local_bytes;
        if cap > 0 {
            let unshipped = sh.unshipped.load(Relaxed);
            if unshipped >= cap {
                sh.counters.full_rejections.fetch_add(1, Relaxed);
                return Err(WalFull { unshipped_bytes: unshipped, limit: cap }.into());
            }
        }
        if ops.is_empty() {
            return Ok(0);
        }
        let (cluster, uuid, shard, epoch) = (&*self.cluster, self.shard.index_uuid.as_str(), self.shard.shard, self.epoch);
        for (_, op) in ops {
            record::check_lengths(cluster, uuid, op.as_ref())?;
        }
        // Encode and checksum outside the lock, so the critical section is a copy: appenders on
        // every core share this lock, and a large operation's CRC would stall them all.
        let clock = now_ms();
        let encode = |buf: &mut Vec<u8>, at: u64| {
            for (seq_no, op) in ops {
                record::encode(buf, at, *seq_no, epoch, cluster, uuid, shard, op.as_ref());
            }
        };
        let max_seq_no = ops.iter().map(|(s, _)| *s).max().unwrap_or(i64::MIN);
        self.encode_and_push(clock, ops.len() as u64, max_seq_no, &encode)
    }

    /// Append one operation whose payload is the concatenation of `parts` (framing plus a
    /// document, say) without first copying them into one buffer.
    pub fn append_parts(&self, seq_no: i64, parts: &[&[u8]]) -> anyhow::Result<Lsn> {
        let sh = &*self.sh;
        let cap = sh.cfg.max_local_bytes;
        if cap > 0 {
            let unshipped = sh.unshipped.load(Relaxed);
            if unshipped >= cap {
                sh.counters.full_rejections.fetch_add(1, Relaxed);
                return Err(WalFull { unshipped_bytes: unshipped, limit: cap }.into());
            }
        }
        let (cluster, uuid, shard, epoch) = (&*self.cluster, self.shard.index_uuid.as_str(), self.shard.shard, self.epoch);
        record::check_payload_len(cluster, uuid, parts.iter().map(|p| p.len()).sum())?;
        let encode = |buf: &mut Vec<u8>, at: u64| record::encode_parts(buf, at, seq_no, epoch, cluster, uuid, shard, parts);
        self.encode_and_push(now_ms(), 1, seq_no, &encode)
    }

    fn encode_and_push(&self, clock: u64, count: u64, max_seq_no: i64, encode: &dyn Fn(&mut Vec<u8>, u64)) -> anyhow::Result<Lsn> {
        SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            scratch.clear();
            encode(&mut scratch, clock);
            let r = self.push(&mut scratch, encode, count, clock, max_seq_no);
            if scratch.capacity() > SCRATCH_KEEP {
                *scratch = Vec::new();
            }
            r
        })
    }

    /// Add encoded records (stamped `clock`) to the batch under the lock; `encode` stamps them
    /// again if the clock must move forward.
    fn push(&self, encoded: &mut Vec<u8>, encode: &dyn Fn(&mut Vec<u8>, u64), count: u64, clock: u64, max_seq_no: i64) -> anyhow::Result<Lsn> {
        let sh = &*self.sh;
        let mut guard = sh.st.lock();
        while guard.buf.len() >= sh.cfg.max_pending_bytes && guard.failed.is_none() && !guard.closing && !guard.abandon {
            sh.wake.notify_one();
            sh.room.wait(&mut guard);
        }
        if let Some(e) = &guard.failed {
            bail!("WAL failed: {e}");
        }
        if guard.closing || guard.abandon {
            bail!("WAL is closed");
        }
        let st = &mut *guard;
        let slot = st.slots[self.slot].as_mut().expect("registered slot");
        if slot.fenced {
            let (epoch, owner_epoch) = st.fenced.get(&*slot.cluster).copied().unwrap_or_default();
            return Err(Fenced { cluster: slot.cluster.to_string(), epoch, owner_epoch }.into());
        }
        // Times are monotonic in log order, so point-in-time replay can stop at the first later
        // record. Another appender that read a later clock got in first: restamp (rare: only
        // across a millisecond boundary, or when the clock steps back).
        let now = clock.max(st.last_ms);
        if now != clock {
            encoded.clear();
            encode(encoded, now);
        }
        st.last_ms = now;
        let before = st.buf.len();
        st.buf.extend_from_slice(encoded);
        let n = encoded.len() as u64;
        match slot.files.back_mut() {
            Some(f) if f.file == st.cur_file => {
                f.max_seq_no = f.max_seq_no.max(max_seq_no);
                f.ops += count;
                f.bytes += n;
            }
            _ => slot.files.push_back(FileUse { file: st.cur_file, max_seq_no, ops: count, bytes: n, first_ms: now }),
        }
        if st.cur_bytes == 0 {
            st.cur_bytes = record::FILE_HEADER as u64;
        }
        st.cur_bytes += n;
        let first = st.next_lsn;
        st.next_lsn += count;
        if !st.pending {
            st.pending = true;
            st.batch_first_lsn = first;
            st.batch_started = Some(Instant::now());
            sh.wake.notify_one();
        } else if before < sh.cfg.flush_bytes && st.buf.len() >= sh.cfg.flush_bytes {
            sh.wake.notify_one();
        }
        st.appends += count;
        st.append_bytes += n;
        Ok(st.next_lsn - 1)
    }

    /// The shard's operations up to `checkpoint` are in a commit point in object storage:
    /// stop keeping WAL files for them.
    pub fn trim(&self, checkpoint: i64) {
        {
            let mut st = self.sh.st.lock();
            let slot = st.slots[self.slot].as_mut().expect("registered slot");
            while slot.files.front().is_some_and(|f| f.max_seq_no <= checkpoint) {
                slot.files.pop_front();
            }
        }
        self.sh.trim_local();
    }

    pub fn stats(&self) -> ShardWalStats {
        let st = self.sh.st.lock();
        let slot = st.slots[self.slot].as_ref().expect("registered slot");
        let mut s = ShardWalStats { files: slot.files.len() as u64, ..Default::default() };
        for f in &slot.files {
            s.uncommitted_ops += f.ops;
            s.uncommitted_bytes += f.bytes;
            s.oldest_ms = Some(s.oldest_ms.map_or(f.first_ms, |m: u64| m.min(f.first_ms)));
        }
        s
    }
}

impl Clone for WalShard {
    fn clone(&self) -> Self {
        self.sh.st.lock().slots[self.slot].as_mut().expect("registered slot").handles += 1;
        Self { sh: self.sh.clone(), slot: self.slot, cluster: self.cluster.clone(), shard: self.shard.clone(), epoch: self.epoch }
    }
}

impl Drop for WalShard {
    fn drop(&mut self) {
        let mut st = self.sh.st.lock();
        release_slot(&mut st, self.slot, false);
    }
}

impl WalShared {
    pub(crate) fn path(&self, seq: u64) -> PathBuf {
        self.dir.join(file_name(seq))
    }

    /// `(seq, intact length)` of local files in `[from, to]`.
    pub(crate) fn local_range(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        self.local.lock().range(from..=to).filter(|(_, f)| f.len > record::FILE_HEADER as u64).map(|(s, f)| (*s, f.len)).collect()
    }

    /// Delete local files that are shipped and hold nothing a registered shard still needs.
    fn trim_local(&self) {
        let keep_from = {
            let st = self.st.lock();
            st.slots.iter().flatten().filter_map(|s| s.files.front().map(|f| f.file)).min().unwrap_or(u64::MAX).min(st.cur_file)
        };
        let doomed: Vec<u64> = {
            let mut local = self.local.lock();
            let mut doomed: Vec<u64> = local.range(..keep_from).filter(|(_, f)| f.sealed && f.shipped).map(|(s, _)| *s).collect();
            let cap = self.cfg.max_local_bytes;
            if cap > 0 {
                // Over the disk limit, shipped files go even if a shard would rather replay them
                // locally: object storage has them.
                let mut total: u64 = local.values().map(|f| f.len).sum::<u64>() - doomed.iter().map(|s| local[s].len).sum::<u64>();
                for (seq, f) in local.range(keep_from..) {
                    if total <= cap {
                        break;
                    }
                    if f.sealed && f.shipped {
                        doomed.push(*seq);
                        total -= f.len;
                    }
                }
            }
            for s in &doomed {
                local.remove(s);
            }
            doomed
        };
        for seq in doomed {
            match std::fs::remove_file(self.path(seq)) {
                Ok(()) => {
                    self.counters.files_trimmed.fetch_add(1, Relaxed);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, seq, "removing a shipped WAL file failed"),
            }
        }
    }

    fn fail(&self, st: &mut State, msg: String) {
        tracing::error!(error = %msg, "search WAL failed; appends are refused until the node restarts");
        st.failed = Some(msg);
        self.durable.send_modify(|m| m.failed = true);
        self.remote.send_modify(|m| m.failed = true);
        self.room.notify_all();
    }
}

/// Validate the local files of an earlier run: drop empty ones, truncate torn tails.
fn recover_dir(dir: &Path) -> anyhow::Result<BTreeMap<u64, LocalFile>> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(seq) = name.to_str().and_then(parse_file_name) else { continue };
        let path = entry.path();
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        let (valid, torn) = match record::StreamRecords::open(std::io::BufReader::with_capacity(1 << 20, &file)) {
            Ok((h, mut r)) if h.seq == seq => {
                while r.next_record()?.is_some() {}
                (r.valid_len(), r.torn())
            }
            // A header that never made it to disk: the file got no committed batch.
            _ => (0, true),
        };
        if valid <= record::FILE_HEADER as u64 {
            drop(file);
            std::fs::remove_file(&path)?;
            continue;
        }
        if torn || file.metadata()?.len() != valid {
            tracing::warn!(file = %path.display(), valid, "truncating a torn WAL tail");
            file.set_len(valid)?;
            file.sync_all()?;
        }
        out.insert(seq, LocalFile { len: valid, sealed: true, shipped: false });
    }
    sync_dir(dir);
    Ok(out)
}

enum Step {
    Flush,
    Seal,
    Exit,
}

fn run_flusher(sh: Arc<WalShared>) {
    loop {
        let mut st = sh.st.lock();
        let step = loop {
            if st.abandon || st.failed.is_some() {
                break Step::Exit;
            }
            let now = Instant::now();
            let upload_slot = sh.cfg.sync_objects && st.ship_inflight < sh.cfg.ship_concurrency;
            if !st.buf.is_empty() {
                // Someone waits for a buffered record: commit now (the previous commit is done,
                // so this is back to back with it). Otherwise within `flush_delay`.
                let waited = st.wanted >= st.batch_first_lsn;
                let due = if waited { now } else { st.batch_started.map_or(now, |t| t + sh.cfg.flush_delay) };
                if st.flush_now || st.closing || st.seal || st.buf.len() >= sh.cfg.flush_bytes || now >= due {
                    break Step::Flush;
                }
                sh.wake.wait_until(&mut st, due);
                continue;
            }
            if let Some(opened) = st.cur_opened {
                let due = opened + sh.cfg.ship_interval;
                if st.seal || st.closing || upload_slot || now >= due {
                    break Step::Seal;
                }
                sh.wake.wait_until(&mut st, due);
                continue;
            }
            st.seal = false;
            if st.closing {
                break Step::Exit;
            }
            sh.wake.wait(&mut st);
        };
        drop(st);
        let mut io = sh.io.lock();
        let mut st = sh.st.lock();
        match step {
            Step::Exit => {
                let drain = !st.abandon && st.failed.is_none();
                drop(st);
                drop(io.take());
                drop(io);
                let _ = sh.ship_tx.send(ShipMsg::Stop { drain });
                sh.flusher_done.send_replace(true);
                return;
            }
            // A waiter committed inline in between: decide again.
            Step::Flush if st.buf.is_empty() => {}
            Step::Seal if !st.buf.is_empty() || st.cur_opened.is_none() => {}
            Step::Seal => {
                let last = st.next_lsn - 1;
                seal_locked(&mut st, last);
                drop(st);
                if let Some(f) = io.take() {
                    drop(f.file);
                    sh.sealed(f.seq, last);
                }
            }
            Step::Flush => sh.commit_batch(&mut io, st),
        }
    }
}

/// Fsyncs at most this slow (on average) let a waiter commit its batch itself instead of waking
/// the flusher thread and then being woken by it: two thread handoffs are most of a write's
/// latency on fast disks, while a slow fsync must not block an async worker.
const INLINE_FSYNC_NANOS: u64 = 250_000;

/// The local file being written: its seq, handle and length.
struct OpenFile {
    seq: u64,
    file: File,
    len: u64,
}

impl WalShared {
    /// Commits the buffered batch (non-empty) under `io`: takes it out of `st`, seals the file
    /// when it's time, writes and fsyncs, and publishes the durable mark.
    fn commit_batch(&self, io: &mut Option<OpenFile>, mut st: parking_lot::MutexGuard<'_, State>) {
        let spare = std::mem::take(&mut st.spare);
        let mut data = std::mem::replace(&mut st.buf, spare);
        let (first_lsn, last_lsn, seq) = (st.batch_first_lsn, st.next_lsn - 1, st.cur_file);
        st.pending = false;
        st.flush_now = false;
        st.batch_started = None;
        let now = Instant::now();
        let roll = st.cur_bytes >= self.cfg.file_bytes
            || st.seal
            || st.closing
            || (self.cfg.sync_objects && st.ship_inflight < self.cfg.ship_concurrency)
            || st.cur_opened.is_some_and(|t| now >= t + self.cfg.ship_interval);
        if roll {
            seal_locked(&mut st, last_lsn);
        } else if st.cur_opened.is_none() {
            st.cur_opened = Some(now);
        }
        self.room.notify_all();
        drop(st);
        if let Err(e) = write_batch(self, io, seq, first_lsn, &data) {
            let mut st = self.st.lock();
            self.fail(&mut st, format!("writing WAL file {}: {e}", self.path(seq).display()));
            return;
        }
        self.durable.send_modify(|m| m.lsn = m.lsn.max(last_lsn));
        if roll && let Some(f) = io.take() {
            drop(f.file);
            self.sealed(f.seq, last_lsn);
        }
        data.clear();
        if data.capacity() > self.cfg.flush_bytes * 2 {
            data.shrink_to(self.cfg.flush_bytes);
        }
        let mut st = self.st.lock();
        if st.spare.capacity() < data.capacity() {
            st.spare = data;
        }
    }

    /// A waiter for `lsn` commits the buffered batch on its own thread when fsyncs are fast and
    /// no other commit runs; whether `lsn` is durable afterwards.
    fn commit_inline(&self, lsn: Lsn) -> bool {
        if self.durable.borrow().lsn >= lsn {
            return true;
        }
        if !self.cfg.leader_commits {
            return false;
        }
        if self.cfg.fsync {
            let ewma = self.fsync_ewma.load(Relaxed);
            if ewma == 0 || ewma > INLINE_FSYNC_NANOS {
                return false;
            }
        }
        let Some(mut io) = self.io.try_lock() else { return false };
        let st = self.st.lock();
        if st.abandon || st.failed.is_some() || st.closing || lsn >= st.next_lsn {
            return false;
        }
        if self.durable.borrow().lsn >= lsn {
            return true;
        }
        // Batches leave the buffer only under `io`, which this thread holds: `lsn` is buffered.
        if st.buf.is_empty() {
            return false;
        }
        self.commit_batch(&mut io, st);
        self.durable.borrow().lsn >= lsn
    }
}

/// Switch new appends to the next file; the caller seals the old one once written.
fn seal_locked(st: &mut State, last_lsn: Lsn) {
    st.cur_file += 1;
    st.cur_bytes = 0;
    st.cur_opened = None;
    st.seal = false;
    st.sealed_lsn = st.sealed_lsn.max(last_lsn);
}

fn write_batch(sh: &WalShared, file: &mut Option<OpenFile>, seq: u64, first_lsn: Lsn, data: &[u8]) -> std::io::Result<()> {
    if file.as_ref().is_none_or(|f| f.seq != seq) {
        let path = sh.path(seq);
        let mut f = std::fs::OpenOptions::new().create_new(true).write(true).open(&path)?;
        let header = record::FileHeader { seq, first_lsn, created_ms: now_ms() }.encode();
        f.write_all(&header)?;
        sh.unshipped.fetch_add(header.len() as u64, Relaxed);
        *file = Some(OpenFile { seq, file: f, len: header.len() as u64 });
        sh.local.lock().insert(seq, LocalFile { len: 0, sealed: false, shipped: false });
        if sh.cfg.fsync {
            sync_dir(&sh.dir);
        }
    }
    let f = file.as_mut().expect("open file");
    sh.unshipped.fetch_add(data.len() as u64, Relaxed);
    f.file.write_all(data)?;
    f.len += data.len() as u64;
    if sh.cfg.fsync {
        let t = Instant::now();
        f.file.sync_data()?;
        let nanos = t.elapsed().as_nanos() as u64;
        sh.counters.fsync_nanos.fetch_add(nanos, Relaxed);
        let old = sh.fsync_ewma.load(Relaxed);
        let ewma = if old == 0 { nanos } else { (old * 7 + nanos) / 8 };
        sh.fsync_ewma.store(ewma.max(1), Relaxed);
    }
    if let Some(l) = sh.local.lock().get_mut(&seq) {
        l.len = f.len;
    }
    sh.counters.commits.fetch_add(1, Relaxed);
    sh.counters.commit_bytes.fetch_add(data.len() as u64, Relaxed);
    Ok(())
}

impl WalShared {
    fn sealed(&self, seq: u64, last_lsn: Lsn) {
        if let Some(l) = self.local.lock().get_mut(&seq) {
            l.sealed = true;
        }
        self.st.lock().ship_inflight += 1;
        self.counters.files_sealed.fetch_add(1, Relaxed);
        let _ = self.ship_tx.send(ShipMsg::Ship { seq, last_lsn, recovered: false });
    }
}

async fn run_shipper(sh: Arc<WalShared>, mut rx: mpsc::UnboundedReceiver<ShipMsg>) {
    // Files complete out of order with concurrent uploads; the remote mark only moves over a
    // completed prefix.
    let mut order: VecDeque<(u64, Lsn, bool)> = VecDeque::new();
    let mut waiting: VecDeque<(u64, bool)> = VecDeque::new();
    let mut inflight = FuturesUnordered::new();
    let mut stop: Option<bool> = None;
    loop {
        while inflight.len() < sh.cfg.ship_concurrency
            && let Some((seq, recovered)) = waiting.pop_front()
        {
            let s = sh.clone();
            inflight.push(async move { (seq, recovered, ship_one(&s, seq, recovered).await) });
        }
        match stop {
            Some(false) => break,
            Some(true) if inflight.is_empty() && waiting.is_empty() => break,
            _ => {}
        }
        tokio::select! {
            msg = rx.recv(), if stop.is_none() => match msg {
                Some(ShipMsg::Ship { seq, last_lsn, recovered }) => {
                    order.push_back((seq, last_lsn, false));
                    waiting.push_back((seq, recovered));
                }
                Some(ShipMsg::Stop { drain }) => stop = Some(drain),
                None => stop = Some(false),
            },
            Some((seq, recovered, ok)) = inflight.next(), if !inflight.is_empty() => {
                if !ok {
                    // Abandoned mid-upload.
                    break;
                }
                if let Some(l) = sh.local.lock().get_mut(&seq)
                    && !l.shipped
                {
                    l.shipped = true;
                    sh.unshipped.fetch_sub(l.len, Relaxed);
                }
                if let Some(e) = order.iter_mut().find(|e| e.0 == seq) {
                    e.2 = true;
                }
                let mut mark = 0;
                while order.front().is_some_and(|e| e.2) {
                    mark = mark.max(order.pop_front().expect("front").1);
                }
                if mark > 0 {
                    sh.remote.send_modify(|m| m.lsn = m.lsn.max(mark));
                }
                if !recovered {
                    let mut st = sh.st.lock();
                    st.ship_inflight = st.ship_inflight.saturating_sub(1);
                    if sh.cfg.sync_objects && st.cur_opened.is_some() {
                        sh.wake.notify_one();
                    }
                }
                sh.trim_local();
            }
        }
    }
    sh.shipper_done.send_replace(true);
}

/// Upload one sealed file, retrying until it succeeds. `false` if the WAL was abandoned.
async fn ship_one(sh: &Arc<WalShared>, seq: u64, recovered: bool) -> bool {
    let key = keys::wal_key(&sh.node, seq);
    let mut backoff = Duration::from_millis(50);
    loop {
        if sh.st.lock().abandon {
            return false;
        }
        match try_ship(sh, seq, &key, recovered).await {
            Ok(()) => return true,
            Err(e) => {
                sh.counters.ship_retries.fetch_add(1, Relaxed);
                tracing::warn!(error = %e, key, "shipping a WAL file failed, retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

async fn try_ship(sh: &Arc<WalShared>, seq: u64, key: &str, recovered: bool) -> anyhow::Result<()> {
    // An earlier run may have shipped it and crashed before deleting the local copy.
    if recovered && sh.objects.exists(key).await? {
        return Ok(());
    }
    let len = sh.local.lock().get(&seq).map(|f| f.len).ok_or_else(|| anyhow::anyhow!("WAL file {seq} is not tracked"))?;
    let path = sh.path(seq);
    let level = sh.cfg.zstd_level;
    let data = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        use std::io::Read;
        let mut raw = Vec::with_capacity(len as usize);
        File::open(&path)?.take(len).read_to_end(&mut raw)?;
        anyhow::ensure!(raw.len() as u64 == len, "WAL file {} is shorter than its committed length", path.display());
        object::encode(&raw, level)
    })
    .await??;
    let stored = data.len() as u64;
    sh.objects.put(key, bytes::Bytes::from(data)).await?;
    sh.counters.files_shipped.fetch_add(1, Relaxed);
    sh.counters.shipped_raw_bytes.fetch_add(len, Relaxed);
    sh.counters.shipped_stored_bytes.fetch_add(stored, Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_clock_is_restamped_to_keep_times_monotonic() {
        let objects = crate::test_objects("wal-unit-restamp");
        let dir = crate::test_dir().join(format!("wal-unit-{}", crate::random_u64()));
        let cfg = WalConfig { ship_interval: Duration::from_secs(3600), ..WalConfig::default() };
        let wal = Wal::open("n1", objects, dir.clone(), cfg).await.unwrap();
        let s = ShardId::new("idx", 0);
        let w = wal.shard("c1", &s).unwrap();
        w.append(0, b"first").unwrap();
        // An appender that read its clock long ago and lost the race for the lock.
        let ops: [(i64, &[u8]); 2] = [(1, b"late"), (2, b"late too")];
        let encode = |buf: &mut Vec<u8>, at: u64| {
            for (seq, op) in ops {
                record::encode(buf, at, seq, 0, "c1", "idx", 0, op);
            }
        };
        let mut encoded = Vec::new();
        encode(&mut encoded, 5);
        let lsn = w.push(&mut encoded, &encode, 2, 5, 2).unwrap();
        wal.durable(lsn).await.unwrap();
        let raw = std::fs::read(wal.local_path(wal.local_files()[0])).unwrap();
        let recs: Vec<_> = record::Records::new(&raw[record::FILE_HEADER..]).collect();
        assert_eq!(recs.len(), 3, "the restamped records still check out");
        assert!(recs.windows(2).all(|r| r[0].at_ms <= r[1].at_ms), "{recs:?}");
        assert!(recs[1].at_ms > 5);
        assert_eq!((recs[2].seq_no, recs[2].payload), (2, &b"late too"[..]));
        drop(w);
        wal.close().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Commits are on demand: a thousand appends then one wait are one commit, at once, however
    /// long the window for records nobody waits for; those still commit within the window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_commits_at_once_and_unwaited_records_within_the_window() {
        let objects = crate::test_objects("wal-unit-demand");
        let dir = crate::test_dir().join(format!("wal-unit-{}", crate::random_u64()));
        // A window much longer than one fsync, even on a loaded disk.
        let cfg = WalConfig { flush_delay: Duration::from_secs(3), ship_interval: Duration::from_secs(3600), ..WalConfig::default() };
        let wal = Wal::open("n1", objects, dir.clone(), cfg).await.unwrap();
        let w = wal.shard("c1", &ShardId::new("idx", 0)).unwrap();
        let t = Instant::now();
        let mut lsn = 0;
        for i in 0..1000 {
            lsn = w.append_parts(i, &[b"op ", b"payload"]).unwrap();
        }
        wal.durable(lsn).await.unwrap();
        assert!(t.elapsed() < Duration::from_millis(2000), "a waiter doesn't wait for the window: {:?}", t.elapsed());
        assert_eq!(wal.stats().commits, 1, "{:?}", wal.stats());
        // Nobody waits: committed within the window anyway.
        let lsn = w.append(1000, b"async").unwrap();
        let t = Instant::now();
        while wal.durable_lsn() < lsn {
            assert!(t.elapsed() < Duration::from_secs(15), "unwaited records commit within the window");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(t.elapsed() >= Duration::from_millis(2500), "{:?}", t.elapsed());
        assert_eq!(wal.stats().commits, 2);
        drop(w);
        wal.close().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A waiter commits inline only while fsyncs are known to be fast; then its record is
    /// durable before it ever awaits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiters_commit_inline_only_while_fsyncs_are_fast() {
        let objects = crate::test_objects("wal-unit-inline");
        let dir = crate::test_dir().join(format!("wal-unit-{}", crate::random_u64()));
        // A long window: only a waiter makes a record durable.
        let cfg = WalConfig { flush_delay: Duration::from_secs(3600), ship_interval: Duration::from_secs(3600), ..WalConfig::default() };
        let wal = Wal::open("n1", objects, dir.clone(), cfg).await.unwrap();
        let w = wal.shard("c1", &ShardId::new("idx", 0)).unwrap();
        // No fsync measured yet: hand off to the flusher (which measures one).
        let lsn = w.append(0, b"a").unwrap();
        assert!(!wal.sh.commit_inline(lsn));
        wal.durable(lsn).await.unwrap();
        assert!(wal.sh.fsync_ewma.load(Relaxed) > 0);
        // Slow fsyncs: still the flusher's job.
        wal.sh.fsync_ewma.store(INLINE_FSYNC_NANOS + 1, Relaxed);
        let lsn = w.append(1, b"b").unwrap();
        assert!(!wal.sh.commit_inline(lsn));
        assert!(wal.durable_lsn() < lsn);
        wal.durable(lsn).await.unwrap();
        // Fast ones: the waiter commits (everything buffered) on its own thread.
        wal.sh.fsync_ewma.store(1, Relaxed);
        w.append(2, b"c").unwrap();
        let lsn = w.append(3, b"d").unwrap();
        let commits = wal.stats().commits;
        // The flusher may still be finishing the previous commit (it holds the file until
        // then); nobody else commits meanwhile (the window is an hour).
        let t = Instant::now();
        while !wal.sh.commit_inline(lsn) {
            assert!(t.elapsed() < Duration::from_secs(10), "the waiter never committed inline");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(wal.durable_lsn(), lsn);
        assert_eq!(wal.stats().commits, commits + 1);
        // Already durable: nothing to do.
        assert!(wal.sh.commit_inline(lsn));
        assert_eq!(wal.stats().commits, commits + 1);
        let raw = std::fs::read(wal.local_path(wal.local_files()[0])).unwrap();
        let seqs: Vec<i64> = record::Records::new(&raw[record::FILE_HEADER..]).map(|r| r.seq_no).collect();
        assert_eq!(seqs, [0, 1, 2, 3]);
        drop(w);
        wal.close().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Inline commits racing each other and the flusher (small files: seals in between) keep
    /// every record exactly once and each shard's records in append order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_leader_commits_keep_log_order() {
        let objects = crate::test_objects("wal-unit-leaders");
        let dir = crate::test_dir().join(format!("wal-unit-{}", crate::random_u64()));
        let cfg = WalConfig { fsync: false, file_bytes: 16 << 10, ship_interval: Duration::from_secs(3600), ..WalConfig::default() };
        let wal = Wal::open("n1", objects, dir.clone(), cfg).await.unwrap();
        let (writers, ops) = (8u32, 300i64);
        let mut tasks = Vec::new();
        for t in 0..writers {
            let wal = wal.clone();
            tasks.push(tokio::spawn(async move {
                let w = wal.shard("c1", &ShardId::new("idx", t)).unwrap();
                for i in 0..ops {
                    let lsn = w.append_parts(i, &[b"payload of an operation ", &i.to_le_bytes()]).unwrap();
                    if i % 3 != 2 {
                        wal.durable(lsn).await.unwrap();
                    }
                }
                let lsn = w.append(ops, b"last").unwrap();
                wal.durable(lsn).await.unwrap();
                w
            }));
        }
        let mut handles = Vec::new();
        for t in tasks {
            handles.push(t.await.unwrap());
        }
        let mut next = vec![0i64; writers as usize];
        let files = wal.local_files();
        assert!(files.len() > 3, "small files were sealed: {files:?}");
        for f in files {
            let raw = std::fs::read(wal.local_path(f)).unwrap();
            for r in record::Records::new(&raw[record::FILE_HEADER..]) {
                let n = &mut next[r.shard as usize];
                assert_eq!(r.seq_no, *n, "shard {} out of order in file {f}", r.shard);
                *n += 1;
            }
        }
        assert!(next.iter().all(|n| *n == ops + 1), "{next:?}");
        drop(handles);
        wal.close().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}

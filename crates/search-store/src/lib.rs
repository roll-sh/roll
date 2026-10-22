//! roll search durability and storage: WAL group commit, segment upload, manifests, local cache,
//! branching and garbage collection. One [`Store`] per node.
//!
//! Object storage is the source of truth; local disk holds the WAL being written and a cache.
//!
//! - **WAL** ([`wal`]): every shard of every cluster on the node appends to one stream that is
//!   group-committed to local files (fsync) and shipped to `search/wal/<node>/` as zstd objects.
//! - **Segments**: immutable files uploaded once under a random id and shared by every branch
//!   that references them; read through memory maps of the local cache ([`SegmentData`]), or
//!   range by range while a large one is still downloading ([`SegmentFetch`]). Deletions live
//!   in small live-docs objects with unique generations.
//! - **Manifests** ([`manifest`]): a shard's commit point (segments, live docs, checkpoint, WAL
//!   ranges to replay) and a cluster's metadata, both written as new generations, never in place.
//! - **Branching** ([`Store::branch`]): a branch's manifests point at its parent's segments and
//!   WAL; nothing is copied. Point-in-time branches start from the newest retained commit point
//!   at or before the time and replay the parent's WAL up to it.
//! - **Garbage collection** ([`Store::collect_garbage`]): deletes segments, live docs and WAL
//!   objects that no retained manifest of any cluster references, after a grace period. What
//!   each manifest references is remembered (and persisted), so a pass reads only new manifests.
//! - **Ownership** ([`Store::set_epoch`]): manifests and cluster states carry the writer's lease
//!   epoch in their keys and "latest" orders by it, so a node that lost its lease can't make
//!   anything it writes current; it gets [`Fenced`] and its WAL refuses the cluster's appends.
//!
//! Recovering a shard (on restart, on another node, or a branch's first open):
//!
//! ```ignore
//! store.set_epoch("c1", lease.epoch);                              // optional: fencing
//! let mut rec = store.recover_shard("c1", &shard).await?;          // latest commit point
//! // open rec.manifest's segments with store.open_segment(..)
//! store.replay(&mut rec, &mut |r| engine.apply(r.seq_no, r.payload)).await?;
//! let (wal, manifest) = store.attach_shard(rec).await?;            // now appends may go to `wal`
//! let lsn = wal.append_many(&bulk_ops_for_this_shard)?; store.wal().wait_ack(lsn).await?;
//! // flush: upload segments, then
//! store.write_manifest("c1", &shard, &new_commit_point).await?;    // also trims the WAL
//! store.detach_shard(wal).await?;                                   // unload (after a flush)
//! ```
//!
//! Waking a whole cluster, [`Store::recover_shards`] reads every shard's commit point with one
//! listing. A shard only read until its first write can stay recovered-but-unattached: attaching
//! writes a commit point, which a read-only wake doesn't need.

pub mod cache;
pub mod error;
pub mod keys;
pub mod manifest;
pub mod wal;

mod branch;
mod gc;

pub use cache::{CacheStats, SegmentData, SegmentFetch, Verifier};
pub use error::{Fenced, WalFull};
pub use gc::GcStats;
pub use keys::{LiveGen, SegmentId, SegmentKey, ShardId, ShardVersion, Version};
pub use manifest::{BranchBase, ClusterState, NO_OPS_PERFORMED, SegmentRef, ShardManifest, WalHead};
pub use pg_storage::{Objects, StorageConfig};
pub use wal::{ClusterWalStats, Lsn, PinningShard, ReplayStats, ShardWalStats, Wal, WalConfig, WalRecord, WalShard, WalSource, WalStats};

use anyhow::{Context, bail};
use bytes::Bytes;
use cache::Cache;
use futures::StreamExt;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Reads of "the latest" generation that find it deleted since the listing look again this
/// many times (only a concurrent deletion of the shard or cluster does that).
const READ_ATTEMPTS: usize = 3;

/// One past the highest generation (1 for none).
fn next_generation(gens: impl Iterator<Item = u64>) -> anyhow::Result<u64> {
    gens.max().map_or(Ok(1), |g| g.checked_add(1).ok_or_else(|| anyhow::anyhow!("generation {g} can't grow any further")))
}

pub(crate) fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    u64::from_le_bytes(b)
}

/// Where [`test_objects`] puts its S3 prefixes.
const S3_TEST_ROOT: &str = "search-tests";
/// S3 test prefixes older than this are swept by later test runs (no test runs this long).
const S3_TEST_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Object storage for tests: S3 under a unique prefix when `ROLL_E2E_S3_BUCKET` is set (with
/// `ROLL_E2E_S3_ENDPOINT` and the usual AWS credentials), otherwise a fresh directory under
/// [`test_dir`]. On S3, the first call of a process also deletes, in the background, test
/// prefixes older than six hours, so the bucket doesn't fill up with past runs.
pub fn test_objects(name: &str) -> Objects {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let unique = format!("{name}-{}-{n}-{}", std::process::id(), NEXT.fetch_add(1, Relaxed));
    let s3 = |prefix: String| match std::env::var("ROLL_E2E_S3_BUCKET") {
        Ok(bucket) => Some(StorageConfig::S3 {
            bucket,
            region: std::env::var("AWS_REGION").ok(),
            endpoint: std::env::var("ROLL_E2E_S3_ENDPOINT").ok(),
            prefix: Some(prefix),
        }),
        Err(_) => None,
    };
    let cfg = match s3(format!("{S3_TEST_ROOT}/{unique}")) {
        Some(cfg) => {
            static SWEEP: std::sync::Once = std::sync::Once::new();
            SWEEP.call_once(|| {
                if let Some(root) = s3(S3_TEST_ROOT.to_string()) {
                    sweep_s3_tests(root);
                }
            });
            cfg
        }
        None => StorageConfig::Local(test_dir().join(format!("objects-{unique}"))),
    };
    Objects::new(&cfg).expect("test object storage")
}

/// When a [`test_objects`] prefix (`<name>-<pid>-<unix nanos>-<n>`) was made.
fn test_prefix_nanos(prefix: &str) -> Option<u128> {
    let mut it = prefix.rsplitn(4, '-');
    let (_n, nanos, _pid, _name) = (it.next()?.parse::<u64>().ok()?, it.next()?.parse::<u128>().ok()?, it.next()?.parse::<u32>().ok()?, it.next()?);
    Some(nanos)
}

/// Delete S3 test prefixes past [`S3_TEST_TTL`], on a thread of its own (best effort: a test
/// process that exits first leaves the rest to the next run).
fn sweep_s3_tests(root: StorageConfig) {
    let _ = std::thread::Builder::new().name("search-test-sweep".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async {
            let Ok(objects) = Objects::new(&root) else { return };
            let Ok(keys) = objects.list("").await else { return };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let ttl = S3_TEST_TTL.as_nanos();
            let doomed: Vec<String> = keys
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| k.split('/').next().and_then(test_prefix_nanos).is_some_and(|t| now.saturating_sub(t) > ttl))
                .collect();
            futures::stream::iter(doomed.iter().map(|k| objects.delete(k))).buffer_unordered(16).collect::<Vec<_>>().await;
        });
    });
}

/// This process's directory for test data (`<tmp>/roll-search-store-<pid>`). The first call
/// removes the directories of test processes that are gone, so runs don't pile up in a
/// RAM-backed /tmp.
pub fn test_dir() -> PathBuf {
    static SWEEP: std::sync::Once = std::sync::Once::new();
    let tmp = std::env::temp_dir();
    SWEEP.call_once(|| {
        // Only where liveness can be checked.
        if !Path::new("/proc/self").exists() {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&tmp) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("roll-search-store-")).and_then(|p| p.parse::<u32>().ok()) else { continue };
            if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    });
    let dir = tmp.join(format!("roll-search-store-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// This node's id: names its WAL objects. Must be unique among nodes sharing a bucket and
    /// stable across restarts (a restarted node continues its own WAL numbering).
    pub node: String,
    pub wal: WalConfig,
    /// Local cache budget for segments and live docs (`--search-cache-bytes`).
    pub cache_bytes: u64,
    /// Shard manifest generations kept per shard (at least 1)…
    pub manifest_retain: usize,
    /// …plus every generation (of manifests and cluster states) that was current at some
    /// moment in the last this-many ms: the point-in-time branching window. 0: none.
    pub manifest_retain_ms: u64,
    /// Cluster state generations kept per cluster (at least 1).
    pub state_retain: usize,
    /// Segments up to this size are uploaded with one PUT, larger ones as multipart uploads.
    pub single_put_bytes: u64,
    /// Checks every downloaded segment and live-docs file before the cache keeps it
    /// (search-server: the codec's `SegmentReader::verify`, a whole-file CRC). `None`: trust
    /// object storage.
    pub verify: Option<Verifier>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            node: "node-0".into(),
            wal: WalConfig::default(),
            cache_bytes: 10 << 30,
            manifest_retain: 4,
            manifest_retain_ms: 0,
            state_retain: 4,
            single_put_bytes: 8 << 20,
            verify: None,
        }
    }
}

#[derive(Default)]
struct StoreCounters {
    segments_uploaded: AtomicU64,
    segment_bytes_uploaded: AtomicU64,
    live_docs_written: AtomicU64,
    manifests_written: AtomicU64,
    manifests_deleted: AtomicU64,
    states_written: AtomicU64,
    branches: AtomicU64,
    clusters_deleted: AtomicU64,
    gc_runs: AtomicU64,
    gc_deleted: AtomicU64,
    gc_deleted_bytes: AtomicU64,
    fenced: AtomicU64,
}

/// Everything the store counts, for metrics and the dashboard.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StoreStats {
    pub wal: WalStats,
    pub cache: CacheStats,
    pub segments_uploaded: u64,
    pub segment_bytes_uploaded: u64,
    pub live_docs_written: u64,
    pub manifests_written: u64,
    pub manifests_deleted: u64,
    pub states_written: u64,
    pub branches: u64,
    pub clusters_deleted: u64,
    pub gc_runs: u64,
    pub gc_deleted: u64,
    pub gc_deleted_bytes: u64,
    /// Writes refused because a newer owner had written ([`Fenced`]).
    pub fenced: u64,
    /// Segment files written but not yet uploaded.
    pub staging_files: u64,
    pub staging_bytes: u64,
    /// Shards and clusters whose generations the store tracks in memory.
    pub tracked_shards: u64,
    pub tracked_clusters: u64,
}

/// Storage a cluster's latest commit points reference.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ClusterUsage {
    pub shards: u64,
    pub segments: u64,
    /// Bytes of referenced segments (segments shared with a parent or branch count in full).
    pub segment_bytes: u64,
    /// Of those, plus live docs, the bytes in this node's local cache now.
    pub cached_bytes: u64,
    pub docs: u64,
    pub deleted_docs: u64,
    pub state_generation: Option<u64>,
}

#[derive(Default)]
struct ShardTrack {
    /// The shard was deleted by this node: later writes (a flush racing the deletion) fail
    /// instead of bringing it back.
    deleted: bool,
    /// `written_ms` of retained generations seen (manifests are immutable: never stale).
    written: HashMap<Version, u64>,
    /// The WAL ranges the store maintains for the shard (`None` until attached).
    wal: Option<Vec<WalSource>>,
    /// The last commit point this node wrote (saves reading it back on detach).
    last: Option<ShardManifest>,
}

type ShardKey = (String, ShardId);

/// Per cluster: serializes state writes and remembers `written_ms` of retained state versions
/// (for windowed retention).
type StateTrack = Arc<tokio::sync::Mutex<HashMap<Version, u64>>>;

struct Inner {
    objects: Objects,
    cfg: StoreConfig,
    wal: Wal,
    cache: Arc<Cache>,
    staging: PathBuf,
    shards: Mutex<HashMap<ShardKey, Arc<tokio::sync::Mutex<ShardTrack>>>>,
    states: Mutex<HashMap<String, StateTrack>>,
    epochs: Mutex<HashMap<String, u64>>,
    gc: Mutex<gc::Memo>,
    counters: StoreCounters,
    _lock: std::fs::File,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Commit and ship what's buffered in the background; `close` waits for it instead.
        self.wal.begin_close();
    }
}

/// The node's storage. Cheap to clone.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

/// A shard being opened: its latest commit point and the WAL ranges to replay past it.
#[derive(Debug)]
pub struct ShardRecovery {
    pub cluster: String,
    pub shard: ShardId,
    /// The latest commit point (`None` for a new shard).
    pub manifest: Option<ShardManifest>,
    sources: Vec<WalSource>,
    replayed: bool,
    /// What [`Store::replay`] did.
    pub stats: ReplayStats,
}

impl ShardRecovery {
    /// Replay starts after this seq_no.
    pub fn checkpoint(&self) -> i64 {
        self.manifest.as_ref().map_or(NO_OPS_PERFORMED, |m| m.local_checkpoint)
    }

    /// The WAL ranges replay reads.
    pub fn sources(&self) -> &[WalSource] {
        &self.sources
    }
}

impl Store {
    /// Open the node's store: `cache_dir` holds the WAL (`wal/`), the cache (`seg/`), segments
    /// being written (`staging/`) and downloads (`tmp/`). WAL files of an earlier run are
    /// validated and shipped in the background. Needs a Tokio runtime.
    pub async fn open(objects: Objects, cache_dir: impl Into<PathBuf>, cfg: StoreConfig) -> anyhow::Result<Store> {
        Self::open_with_wal_objects(objects.clone(), objects, cache_dir, cfg).await
    }

    /// [`Store::open`] with the WAL in its own object store (`wal_objects`), e.g. an S3 Express
    /// One Zone directory bucket for single-digit-ms acknowledgements with `sync_objects`, while
    /// segments, manifests and cluster states stay in `objects` (S3 Standard). Every node sharing
    /// `objects` must use the same `wal_objects`: replay on another node and garbage collection
    /// read WAL objects there. Only keys under `search/wal/` go to it, and nothing relies on
    /// listings being sorted (directory buckets don't sort them).
    pub async fn open_with_wal_objects(objects: Objects, wal_objects: Objects, cache_dir: impl Into<PathBuf>, cfg: StoreConfig) -> anyhow::Result<Store> {
        let dir: PathBuf = cache_dir.into();
        anyhow::ensure!(
            !cfg.node.is_empty() && cfg.node.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
            "invalid node id [{}]",
            cfg.node
        );
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("LOCK"))?;
        lock.try_lock().map_err(|_| anyhow::anyhow!("{} is in use by another search store", dir.display()))?;
        let staging = dir.join("staging");
        // Segments an earlier run never finished uploading: no manifest references them.
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let cache = Cache::open(dir.join("seg"), dir.join("tmp"), cfg.cache_bytes, objects.clone(), cfg.verify.clone())?;
        let wal = Wal::open(&cfg.node, wal_objects, dir.join("wal"), cfg.wal.clone()).await?;
        Ok(Store {
            inner: Arc::new(Inner {
                objects,
                cfg,
                wal,
                cache,
                staging,
                shards: Mutex::new(HashMap::new()),
                states: Mutex::new(HashMap::new()),
                epochs: Mutex::new(HashMap::new()),
                gc: Mutex::new(gc::Memo::default()),
                counters: StoreCounters::default(),
                _lock: lock,
            }),
        })
    }

    pub fn wal(&self) -> &Wal {
        &self.inner.wal
    }

    pub fn objects(&self) -> &Objects {
        &self.inner.objects
    }

    /// Where WAL objects go (the same as [`Store::objects`] unless opened with
    /// [`Store::open_with_wal_objects`]).
    pub fn wal_objects(&self) -> &Objects {
        &self.inner.wal.sh.objects
    }

    pub fn node(&self) -> &str {
        &self.inner.cfg.node
    }

    pub fn config(&self) -> &StoreConfig {
        &self.inner.cfg
    }

    /// Commit and ship the WAL, then stop it.
    pub async fn close(&self) -> anyhow::Result<()> {
        self.inner.wal.close().await
    }

    pub fn set_cache_budget(&self, bytes: u64) {
        self.inner.cache.set_budget(bytes);
    }

    // ---- segments ----

    /// Where search-index should write a new segment before [`Store::upload_segment`] (same
    /// filesystem as the cache, so adopting it is a rename).
    pub fn segment_path(&self, id: &SegmentId) -> PathBuf {
        self.inner.staging.join(id.file_name())
    }

    /// Upload a finished segment file named `<segment id hex>.seg` and move it into the cache
    /// (the store owns the file afterwards). Large files go up as multipart uploads.
    pub async fn upload_segment(&self, seg: &Path) -> anyhow::Result<SegmentKey> {
        let name = seg.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow::anyhow!("bad segment path {}", seg.display()))?;
        let Some(keys::SegObject::Segment(id)) = keys::parse_seg_file(name) else {
            bail!("segment file name [{name}] is not <32 hex digits>.seg");
        };
        let size = tokio::fs::metadata(seg).await.with_context(|| format!("reading {}", seg.display()))?.len();
        anyhow::ensure!(size > 0, "segment file {} is empty", seg.display());
        let key = id.key();
        if size <= self.inner.cfg.single_put_bytes {
            let data = tokio::fs::read(seg).await?;
            self.inner.objects.put(&key, Bytes::from(data)).await?;
        } else {
            let f = tokio::fs::File::open(seg).await?;
            let n = self.inner.objects.upload(&key, f).await?;
            anyhow::ensure!(n == size, "segment {} changed while uploading ({n} of {size} bytes)", seg.display());
        }
        let cache = self.inner.cache.clone();
        // The canonical (lowercase) name, which lookups use, whatever case the file had.
        let (name, src) = (id.file_name(), seg.to_path_buf());
        tokio::task::spawn_blocking(move || cache.adopt(&name, &src)).await??;
        self.inner.counters.segments_uploaded.fetch_add(1, Relaxed);
        self.inner.counters.segment_bytes_uploaded.fetch_add(size, Relaxed);
        Ok(id)
    }

    /// Memory-map a segment, fetching it into the cache first when it isn't there. Concurrent
    /// opens of one missing segment share one download.
    pub async fn open_segment(&self, key: &SegmentKey) -> anyhow::Result<SegmentData> {
        self.inner.cache.get(&key.file_name(), &key.key()).await
    }

    /// Start using a segment before it is fully downloaded: resolves as soon as its size is
    /// known (one round trip, which also brings its last 8 MiB: footer and table of contents).
    /// Ranges are readable as their parts arrive ([`SegmentFetch::read`], which moves them to
    /// the front of the download); [`SegmentFetch::complete`] gives the memory map, as
    /// [`Store::open_segment`] would. A cached segment comes back complete. Shares the download
    /// with concurrent `open_segment` calls.
    pub async fn fetch_segment(&self, key: &SegmentKey) -> anyhow::Result<SegmentFetch> {
        self.inner.cache.fetch_progressive(&key.file_name(), &key.key()).await
    }

    /// Open many segments, fetching missing ones in parallel (waking a shard).
    pub async fn open_segments(&self, keys: &[SegmentKey]) -> anyhow::Result<Vec<SegmentData>> {
        futures::stream::iter(keys.iter().map(|k| self.open_segment(k))).buffered(8).collect::<Vec<_>>().await.into_iter().collect()
    }

    pub fn is_cached(&self, key: &SegmentKey) -> bool {
        self.inner.cache.contains(&key.file_name())
    }

    /// Store a new live-docs generation for a segment (the bytes are the codec's live-docs
    /// format). Returns the generation to record in the manifest.
    pub async fn write_live_docs(&self, segment: &SegmentId, data: Bytes) -> anyhow::Result<LiveGen> {
        anyhow::ensure!(!data.is_empty(), "live docs of segment {segment} are empty");
        let live = LiveGen::new(now_ms());
        self.inner.objects.put(&segment.live_key(live), data.clone()).await?;
        let cache = self.inner.cache.clone();
        let name = segment.live_file_name(live);
        tokio::task::spawn_blocking(move || cache.insert_bytes(&name, &data)).await??;
        self.inner.counters.live_docs_written.fetch_add(1, Relaxed);
        Ok(live)
    }

    pub async fn open_live_docs(&self, segment: &SegmentId, live: LiveGen) -> anyhow::Result<SegmentData> {
        self.inner.cache.get(&segment.live_file_name(live), &segment.live_key(live)).await
    }

    // ---- ownership ----

    /// Write `cluster` under ownership epoch `epoch` from now on: the epoch of the lease
    /// search-server holds for it (0, the default, when there are no leases). Manifests and
    /// cluster states carry it in their keys; once a higher epoch has written anything for the
    /// cluster, this node's writes fail with [`Fenced`] and its WAL refuses the cluster's appends.
    pub fn set_epoch(&self, cluster: &str, epoch: u64) {
        self.inner.epochs.lock().insert(cluster.to_string(), epoch);
        self.inner.wal.unfence_cluster(cluster, epoch);
    }

    /// The epoch this node writes `cluster` under.
    pub fn epoch(&self, cluster: &str) -> u64 {
        self.inner.epochs.lock().get(cluster).copied().unwrap_or(0)
    }

    /// Refuse to write when `top` (the newest version at a path) is from a newer owner.
    fn fence_check(&self, cluster: &str, top_epoch: Option<u64>) -> anyhow::Result<u64> {
        let epoch = self.epoch(cluster);
        if let Some(top) = top_epoch
            && top > epoch
        {
            self.inner.wal.fence_cluster(cluster, epoch, top);
            self.inner.counters.fenced.fetch_add(1, Relaxed);
            return Err(Fenced { cluster: cluster.to_string(), epoch, owner_epoch: top }.into());
        }
        Ok(epoch)
    }

    // ---- manifests ----

    fn shard_track(&self, cluster: &str, shard: &ShardId) -> Arc<tokio::sync::Mutex<ShardTrack>> {
        self.inner.shards.lock().entry((cluster.to_string(), shard.clone())).or_default().clone()
    }

    fn state_track(&self, cluster: &str) -> StateTrack {
        self.inner.states.lock().entry(cluster.to_string()).or_default().clone()
    }

    /// Versions under `prefix` (direct children ending in `suffix`), oldest first.
    async fn list_versions(&self, prefix: &str, suffix: &str) -> anyhow::Result<Vec<Version>> {
        let mut out: Vec<Version> = self
            .inner
            .objects
            .list(prefix)
            .await?
            .into_iter()
            .filter(|(k, _)| k.strip_prefix(prefix).is_some_and(|f| !f.contains('/')))
            .filter_map(|(k, _)| keys::parse_version(&k, suffix))
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    /// Generations of a shard's history (commit points and tombstones), oldest first.
    async fn list_shard_versions(&self, cluster: &str, shard: &ShardId) -> anyhow::Result<Vec<ShardVersion>> {
        let prefix = keys::manifest_prefix(cluster, shard);
        let mut out: Vec<ShardVersion> = self
            .inner
            .objects
            .list(&prefix)
            .await?
            .iter()
            .filter_map(|(k, _)| k.strip_prefix(prefix.as_str()).filter(|f| !f.contains('/')).and_then(keys::parse_shard_version))
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    /// Every shard of a cluster with any history (deleted ones included), with its generations
    /// oldest first: one listing.
    async fn list_shard_histories(&self, cluster: &str) -> anyhow::Result<BTreeMap<ShardId, Vec<ShardVersion>>> {
        let mut out: BTreeMap<ShardId, Vec<ShardVersion>> = BTreeMap::new();
        for (k, _) in self.inner.objects.list(&keys::shards_prefix(cluster)).await? {
            if let Some(keys::ClusterObject::Manifest { cluster: c, index_uuid, shard, version, deleted }) = keys::parse_cluster_key(&k)
                && c == cluster
            {
                out.entry(ShardId::new(index_uuid, shard)).or_default().push(ShardVersion { version, deleted });
            }
        }
        for v in out.values_mut() {
            v.sort_unstable();
        }
        Ok(out)
    }

    /// A generation's manifest (a tombstone reads as an empty one). `None` if it is gone.
    /// Unreadable or damaged is an error: never silently a different generation.
    async fn get_manifest(&self, cluster: &str, shard: &ShardId, v: ShardVersion) -> anyhow::Result<Option<ShardManifest>> {
        let key = keys::shard_version_key(cluster, shard, v);
        let raw = self.inner.objects.get(&key).await.with_context(|| format!("reading shard manifest {key}"))?;
        match raw {
            Some(raw) => {
                let mut m = ShardManifest::from_json(&raw).with_context(|| format!("shard manifest {key} is damaged"))?;
                // The key is authoritative (it is what ordering and fencing go by).
                (m.epoch, m.generation) = (v.version.epoch, v.version.generation);
                Ok(Some(m))
            }
            None => Ok(None),
        }
    }

    /// The latest commit point of a shard (`None` if it has none, or was deleted).
    ///
    /// Reads exactly the newest generation: if it can't be read (an object-store error, or it
    /// is damaged) this fails rather than fall back to an older one, as Elasticsearch fails a
    /// shard with a corrupt commit instead of opening an older commit: the next write would
    /// build on the older state and supersede the newer one for good.
    pub async fn read_manifest(&self, cluster: &str, shard: &ShardId) -> anyhow::Result<Option<ShardManifest>> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(shard)?;
        for _ in 0..READ_ATTEMPTS {
            let versions = self.list_shard_versions(cluster, shard).await?;
            let Some(&top) = versions.last() else { return Ok(None) };
            if top.deleted {
                return Ok(None);
            }
            match self.get_manifest(cluster, shard, top).await? {
                Some(m) => return Ok(Some(m)),
                // The newest is never deleted by retention: the shard or its cluster is being
                // deleted. Look again rather than return an older generation.
                None => continue,
            }
        }
        bail!("the latest commit point of shard {shard} of cluster [{cluster}] kept disappearing while being read")
    }

    /// Every retained commit point of a shard, oldest first (tombstones aside).
    pub async fn read_manifests(&self, cluster: &str, shard: &ShardId) -> anyhow::Result<Vec<ShardManifest>> {
        Ok(self.read_history(cluster, shard).await?.into_iter().filter(|(_, deleted)| !deleted).map(|(m, _)| m).collect())
    }

    /// Every retained generation of a shard with whether it is a tombstone, oldest first.
    pub(crate) async fn read_history(&self, cluster: &str, shard: &ShardId) -> anyhow::Result<Vec<(ShardManifest, bool)>> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(shard)?;
        let versions = self.list_shard_versions(cluster, shard).await?;
        let fetched =
            futures::stream::iter(versions.into_iter().map(|v| async move { self.get_manifest(cluster, shard, v).await.map(|m| m.map(|m| (m, v.deleted))) }))
                .buffered(8)
                .collect::<Vec<_>>()
                .await;
        let mut out = Vec::new();
        for m in fetched {
            out.extend(m?);
        }
        Ok(out)
    }

    /// The latest commit point of every shard of a cluster (deleted shards aside), from one
    /// listing and concurrent reads (rather than a listing per shard). Fails, as
    /// [`Store::read_manifest`] does, when a latest generation can't be read.
    pub async fn read_latest_manifests(&self, cluster: &str) -> anyhow::Result<Vec<(ShardId, ShardManifest)>> {
        keys::validate_cluster(cluster)?;
        let histories = self.list_shard_histories(cluster).await?;
        let reads = histories.into_iter().filter_map(|(shard, vs)| {
            let top = *vs.last()?;
            (!top.deleted).then_some((shard, top))
        });
        let reads = reads.map(|(shard, top)| async move {
            match self.get_manifest(cluster, &shard, top).await? {
                Some(m) => Ok(Some((shard, m))),
                // Gone since the listing: look at the shard again.
                None => self.read_manifest(cluster, &shard).await.map(|m| m.map(|m| (shard, m))),
            }
        });
        let fetched: Vec<anyhow::Result<Option<(ShardId, ShardManifest)>>> = futures::stream::iter(reads).buffered(16).collect().await;
        let mut out = Vec::with_capacity(fetched.len());
        for r in fetched {
            out.extend(r?);
        }
        Ok(out)
    }

    /// Shards of a cluster that have a commit point (deleted shards aside).
    pub async fn list_shards(&self, cluster: &str) -> anyhow::Result<Vec<ShardId>> {
        keys::validate_cluster(cluster)?;
        let histories = self.list_shard_histories(cluster).await?;
        Ok(histories.into_iter().filter(|(_, vs)| vs.last().is_some_and(|v| !v.deleted)).map(|(s, _)| s).collect())
    }

    /// Every shard of a cluster with retained history, deleted shards included (point-in-time
    /// branches may start before a deletion).
    pub(crate) async fn list_shards_with_history(&self, cluster: &str) -> anyhow::Result<Vec<ShardId>> {
        Ok(self.list_shard_histories(cluster).await?.into_keys().collect())
    }

    /// Write a new commit point for a shard and return it as written (generation, epoch, time
    /// and WAL ranges filled in). Upload its segments and live docs first. Afterwards the shard's
    /// WAL operations up to `m.local_checkpoint` are no longer kept. Fails with [`Fenced`] when a
    /// newer owner has written the shard.
    pub async fn write_manifest(&self, cluster: &str, shard: &ShardId, m: &ShardManifest) -> anyhow::Result<ShardManifest> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(shard)?;
        anyhow::ensure!(m.local_checkpoint <= m.max_seq_no, "local checkpoint {} is past max seq_no {}", m.local_checkpoint, m.max_seq_no);
        let track = self.shard_track(cluster, shard);
        let mut t = track.lock().await;
        let mut m = m.clone();
        let wal = &self.inner.wal;
        let mut sources = t.wal.clone().unwrap_or_else(|| m.wal.clone());
        // Ranges whose operations the new commit point covers are no longer needed.
        sources.retain(|s| wal.is_live(s) || !(s.to.is_some_and(|to| to < s.from) || s.max_seq_no.is_some_and(|x| x <= m.local_checkpoint)));
        for s in sources.iter_mut().filter(|s| wal.is_live(s)) {
            s.from = wal.replay_from(cluster, shard, m.local_checkpoint).max(s.from);
        }
        m.wal = sources.clone();
        let written = self.put_manifest(&mut t, cluster, shard, m, false).await?;
        if t.wal.is_some() {
            t.wal = Some(sources);
        }
        drop(t);
        wal.trim_shard(cluster, shard, written.local_checkpoint);
        Ok(written)
    }

    /// Write `m` as the next generation (a tombstone with `tombstone`) under this node's epoch
    /// and apply retention. Caller holds the shard's lock. Lists the prefix every time: that is
    /// what notices a newer owner, and a deletion.
    async fn put_manifest(&self, t: &mut ShardTrack, cluster: &str, shard: &ShardId, mut m: ShardManifest, tombstone: bool) -> anyhow::Result<ShardManifest> {
        if t.deleted {
            bail!("shard {shard} of cluster [{cluster}] was deleted");
        }
        let mut versions = self.list_shard_versions(cluster, shard).await?;
        m.epoch = self.fence_check(cluster, versions.last().map(|v| v.version.epoch))?;
        if versions.last().is_some_and(|v| v.deleted) {
            t.deleted = true;
            bail!("shard {shard} of cluster [{cluster}] was deleted");
        }
        m.generation = next_generation(versions.iter().map(|v| v.version.generation))?;
        m.written_ms = now_ms();
        m.wal_head = Some(WalHead { node: self.inner.cfg.node.clone(), file: self.inner.wal.current_file() });
        let v = ShardVersion { version: Version::fresh(m.epoch, m.generation), deleted: tombstone };
        self.inner.objects.put(&keys::shard_version_key(cluster, shard, v), Bytes::from(m.to_json())).await?;
        versions.push(v);
        t.written.insert(v.version, m.written_ms);
        t.last = (!tombstone).then(|| m.clone());
        self.inner.counters.manifests_written.fetch_add(1, Relaxed);
        // Retention: keep the newest N, and every commit point a point-in-time branch inside the
        // window may start from: the oldest one goes only once its successor is outside the
        // window too (a time inside the window may fall before that successor).
        let keep = self.inner.cfg.manifest_retain.max(1);
        let window = self.inner.cfg.manifest_retain_ms;
        let mut gone = 0;
        while versions.len() - gone > keep {
            let oldest = versions[gone];
            if window > 0 {
                let successor = versions[gone + 1];
                let written = match t.written.get(&successor.version) {
                    Some(&w) => w,
                    None => match self.get_manifest(cluster, shard, successor).await {
                        Ok(Some(next)) => next.written_ms,
                        // Unreadable or gone: keep history rather than guess.
                        Ok(None) | Err(_) => u64::MAX,
                    },
                };
                t.written.insert(successor.version, written);
                if written.saturating_add(window) > m.written_ms {
                    break;
                }
            }
            if let Err(e) = self.inner.objects.delete(&keys::shard_version_key(cluster, shard, oldest)).await {
                tracing::warn!(error = %e, cluster, %shard, version = %oldest.version, "deleting an old shard manifest failed");
                break;
            }
            t.written.remove(&oldest.version);
            gone += 1;
            self.inner.counters.manifests_deleted.fetch_add(1, Relaxed);
        }
        if t.written.len() > versions.len() - gone {
            let alive: HashSet<Version> = versions[gone..].iter().map(|v| v.version).collect();
            t.written.retain(|v, _| alive.contains(v));
        }
        Ok(m)
    }

    /// Delete a shard (its index was deleted). Its segments go with garbage collection.
    ///
    /// With a point-in-time window (`manifest_retain_ms`), the shard's commit points stay and a
    /// tombstone generation records the deletion: the shard is gone for every read
    /// ([`Store::read_manifest`], [`Store::list_shards`], waking), but a branch to a moment
    /// before the deletion still restores its data. Garbage collection removes the history once
    /// the tombstone is older than the window. Without a window the commit points are deleted
    /// at once. Either way, later writes of the shard on this node (a flush racing the
    /// deletion) fail instead of bringing it back.
    pub async fn delete_shard(&self, cluster: &str, shard: &ShardId) -> anyhow::Result<()> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(shard)?;
        let track = self.shard_track(cluster, shard);
        let mut t = track.lock().await;
        let versions = self.list_shard_versions(cluster, shard).await?;
        self.fence_check(cluster, versions.last().map(|v| v.version.epoch))?;
        self.inner.wal.forget_shard(cluster, shard);
        match versions.last() {
            None => {}
            Some(top) if top.deleted => {}
            Some(_) if self.inner.cfg.manifest_retain_ms > 0 => {
                t.deleted = false;
                self.put_manifest(&mut t, cluster, shard, ShardManifest::default(), true).await?;
            }
            Some(_) => {
                self.inner.objects.delete_prefix(&keys::manifest_prefix(cluster, shard)).await?;
            }
        }
        // The track stays (until the cluster is released) to refuse late writes.
        *t = ShardTrack { deleted: true, ..ShardTrack::default() };
        Ok(())
    }

    // ---- recovery ----

    /// Start opening a shard on this node: read its latest commit point.
    pub async fn recover_shard(&self, cluster: &str, shard: &ShardId) -> anyhow::Result<ShardRecovery> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(shard)?;
        if self.inner.wal.is_attached(cluster, shard) {
            bail!("shard {shard} of cluster [{cluster}] is already open on this node");
        }
        let manifest = self.read_manifest(cluster, shard).await?;
        self.fence_check(cluster, manifest.as_ref().map(|m| m.epoch))?;
        let sources = manifest.as_ref().map(|m| m.wal.clone()).unwrap_or_default();
        Ok(ShardRecovery { cluster: cluster.to_string(), shard: shard.clone(), manifest, sources, replayed: false, stats: ReplayStats::default() })
    }

    /// Start opening every shard of a cluster that has a commit point (waking it): one listing
    /// and concurrent reads instead of [`Store::recover_shard`] per shard.
    pub async fn recover_shards(&self, cluster: &str) -> anyhow::Result<Vec<ShardRecovery>> {
        let latest = self.read_latest_manifests(cluster).await?;
        let mut out = Vec::with_capacity(latest.len());
        for (shard, manifest) in latest {
            if self.inner.wal.is_attached(cluster, &shard) {
                bail!("shard {shard} of cluster [{cluster}] is already open on this node");
            }
            self.fence_check(cluster, Some(manifest.epoch))?;
            let sources = manifest.wal.clone();
            out.push(ShardRecovery { cluster: cluster.to_string(), shard, manifest: Some(manifest), sources, replayed: false, stats: ReplayStats::default() });
        }
        Ok(out)
    }

    /// Hand the shard's operations past its checkpoint to `f`, in log order. Operations between
    /// the local checkpoint and `max_seq_no` may already be in the segments (they are
    /// replayed again, as Elasticsearch replays its translog from the local checkpoint).
    pub async fn replay(
        &self,
        rec: &mut ShardRecovery,
        f: &mut (dyn FnMut(&WalRecord<'_>) -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<ReplayStats> {
        let after = rec.checkpoint();
        let stats = self.inner.wal.replay(&mut rec.sources, &rec.shard, after, f).await?;
        rec.replayed = true;
        rec.stats = stats;
        Ok(stats)
    }

    /// Finish opening a shard: record in a new commit point that this node now writes it
    /// (so a crash before its next flush is recoverable from anywhere), and register it with
    /// the WAL. Requires [`Store::replay`] first whenever there is WAL to replay.
    pub async fn attach_shard(&self, mut rec: ShardRecovery) -> anyhow::Result<(WalShard, ShardManifest)> {
        let wal = &self.inner.wal;
        if !rec.replayed && rec.sources.iter().any(|s| s.to.is_none_or(|to| to >= s.from) && s.max_seq_no.is_none()) {
            bail!("replay the WAL of shard {} of cluster [{}] before attaching it", rec.shard, rec.cluster);
        }
        let track = self.shard_track(&rec.cluster, &rec.shard);
        let mut t = track.lock().await;
        if wal.is_attached(&rec.cluster, &rec.shard) {
            bail!("shard {} of cluster [{}] is already open on this node", rec.shard, rec.cluster);
        }
        let handle = wal.shard(&rec.cluster, &rec.shard)?;
        let cur = wal.current_file();
        let checkpoint = rec.checkpoint();
        let mut sources = std::mem::take(&mut rec.sources);
        for s in sources.iter_mut().filter(|s| wal.is_live(s)) {
            // An earlier attachment in this process: its range ends where the new one starts.
            s.to = Some(cur - 1);
        }
        sources.retain(|s| s.to.is_none_or(|to| to >= s.from) && s.max_seq_no.is_none_or(|x| x > checkpoint));
        sources.push(WalSource::open(&self.inner.cfg.node, &rec.cluster, wal.replay_from(&rec.cluster, &rec.shard, checkpoint).min(cur)));
        let mut m = rec.manifest.take().unwrap_or_default();
        m.wal = sources.clone();
        let written = self.put_manifest(&mut t, &rec.cluster, &rec.shard, m, false).await?;
        t.wal = Some(sources);
        Ok((handle, written))
    }

    /// Stop writing a shard on this node (its cluster unloads): drops the WAL registration.
    /// Flush it first: with nothing uncommitted, a new commit point stops naming this node's
    /// WAL, so the node's later WAL files don't stay pinned by an idle shard. Unflushed
    /// operations are shipped instead and stay replayable from anywhere.
    pub async fn detach_shard(&self, wal: WalShard) -> anyhow::Result<()> {
        let (cluster, shard) = (wal.cluster().to_string(), wal.shard_id().clone());
        let uncommitted = wal.stats().uncommitted_ops;
        drop(wal);
        let w = &self.inner.wal;
        if w.is_attached(&cluster, &shard) {
            return Ok(());
        }
        let track = self.shard_track(&cluster, &shard);
        let mut t = track.lock().await;
        let Some(mut sources) = t.wal.take() else { return Ok(()) };
        if !sources.iter().any(|s| w.is_live(s)) {
            return Ok(());
        }
        if uncommitted > 0 {
            w.sync_remote().await?;
            let cur = w.current_file();
            for s in sources.iter_mut().filter(|s| w.is_live(s)) {
                s.to = Some(cur - 1);
            }
            // Shipped and named by the commit point: local copies need not wait for a flush.
            w.forget_shard(&cluster, &shard);
        } else {
            sources.retain(|s| !w.is_live(s));
        }
        // Writing the next generation lists the shard's manifests anyway, which is what notices
        // a newer owner: the copy kept from this node's last write is as good as reading it.
        let latest = match t.last.take() {
            Some(m) => Some(m),
            None => self.read_manifest(&cluster, &shard).await?,
        };
        let Some(mut m) = latest else { return Ok(()) };
        m.wal = sources;
        self.put_manifest(&mut t, &cluster, &shard, m, false).await?;
        Ok(())
    }

    // ---- cluster state ----

    pub async fn cluster_exists(&self, cluster: &str) -> anyhow::Result<bool> {
        keys::validate_cluster(cluster)?;
        Ok(!self.list_versions(&keys::state_prefix(cluster), ".json.zst").await?.is_empty())
    }

    /// The latest cluster state (`None` if the cluster has never written one). Reads exactly
    /// the newest generation: an unreadable or damaged one is an error, never silently an older
    /// state (the next write would build on it and lose the newer one for good).
    pub async fn read_cluster(&self, cluster: &str) -> anyhow::Result<Option<ClusterState>> {
        keys::validate_cluster(cluster)?;
        for _ in 0..READ_ATTEMPTS {
            let versions = self.list_versions(&keys::state_prefix(cluster), ".json.zst").await?;
            let Some(&top) = versions.last() else { return Ok(None) };
            match self.get_state(cluster, top).await? {
                Some(s) => return Ok(Some(s)),
                // Retention never deletes the newest: the cluster is being deleted. Look again.
                None => continue,
            }
        }
        bail!("the latest state of cluster [{cluster}] kept disappearing while being read")
    }

    /// Every retained cluster state, oldest first.
    pub async fn read_cluster_history(&self, cluster: &str) -> anyhow::Result<Vec<ClusterState>> {
        keys::validate_cluster(cluster)?;
        let versions = self.list_versions(&keys::state_prefix(cluster), ".json.zst").await?;
        let fetched = futures::stream::iter(versions.into_iter().map(|v| self.get_state(cluster, v))).buffered(8).collect::<Vec<_>>().await;
        let mut out = Vec::new();
        for s in fetched {
            out.extend(s?);
        }
        Ok(out)
    }

    async fn get_state(&self, cluster: &str, v: Version) -> anyhow::Result<Option<ClusterState>> {
        let key = keys::state_key(cluster, v);
        match self.inner.objects.get(&key).await.with_context(|| format!("reading cluster state {key}"))? {
            Some(raw) => {
                let mut s = ClusterState::decode(&raw).with_context(|| format!("cluster state {key} is damaged"))?;
                (s.epoch, s.generation) = (v.epoch, v.generation);
                Ok(Some(s))
            }
            None => Ok(None),
        }
    }

    /// Write a new generation of a cluster's metadata under this node's epoch. Returns the
    /// generation. Fails with [`Fenced`] when a newer owner has written the cluster's state.
    pub async fn write_cluster(&self, cluster: &str, state: &ClusterState) -> anyhow::Result<u64> {
        keys::validate_cluster(cluster)?;
        let track = self.state_track(cluster);
        let mut written = track.lock().await;
        let prefix = keys::state_prefix(cluster);
        let mut versions = self.list_versions(&prefix, ".json.zst").await?;
        let mut s = state.clone();
        s.epoch = self.fence_check(cluster, versions.last().map(|v| v.epoch))?;
        s.generation = next_generation(versions.iter().map(|v| v.generation))?;
        s.written_ms = now_ms();
        let data = s.encode(3)?;
        let v = Version::fresh(s.epoch, s.generation);
        self.inner.objects.put(&keys::state_key(cluster, v), Bytes::from(data)).await?;
        versions.push(v);
        written.insert(v, s.written_ms);
        self.inner.counters.states_written.fetch_add(1, Relaxed);
        // Retention as for manifests: the newest N, plus the state current at any moment of the
        // point-in-time window (an old state goes once its successor is outside the window).
        let keep = self.inner.cfg.state_retain.max(1);
        let window = self.inner.cfg.manifest_retain_ms;
        let mut gone = 0;
        while versions.len() - gone > keep {
            let oldest = versions[gone];
            if window > 0 {
                let successor = versions[gone + 1];
                let at = match written.get(&successor) {
                    Some(&w) => w,
                    None => match self.get_state(cluster, successor).await {
                        Ok(Some(next)) => next.written_ms,
                        Ok(None) | Err(_) => u64::MAX,
                    },
                };
                written.insert(successor, at);
                if at.saturating_add(window) > s.written_ms {
                    break;
                }
            }
            if let Err(e) = self.inner.objects.delete(&keys::state_key(cluster, oldest)).await {
                tracing::warn!(error = %e, cluster, version = %oldest, "deleting an old cluster state failed");
                break;
            }
            written.remove(&oldest);
            gone += 1;
        }
        if written.len() > versions.len() - gone {
            let alive: HashSet<Version> = versions[gone..].iter().copied().collect();
            written.retain(|v, _| alive.contains(v));
        }
        Ok(s.generation)
    }

    /// Delete a cluster's metadata and commit points. Segments, live docs and WAL objects only
    /// it referenced go with garbage collection (branches keep what they share).
    pub async fn delete_cluster(&self, cluster: &str) -> anyhow::Result<()> {
        keys::validate_cluster(cluster)?;
        let track = self.state_track(cluster);
        let _t = track.lock().await;
        self.inner.wal.forget_cluster(cluster);
        // State first: a cluster without state doesn't exist, even while manifests are deleted.
        self.inner.objects.delete_prefix(&keys::state_prefix(cluster)).await?;
        self.inner.objects.delete_prefix(&keys::cluster_prefix(cluster)).await?;
        self.release_cluster(cluster);
        self.inner.counters.clusters_deleted.fetch_add(1, Relaxed);
        Ok(())
    }

    /// Drop what the store remembers about a cluster (it was unloaded). Its shards must be
    /// detached (their `WalShard`s dropped) first.
    pub fn release_cluster(&self, cluster: &str) {
        self.inner.shards.lock().retain(|(c, _), _| c != cluster);
        self.inner.states.lock().remove(cluster);
        self.inner.epochs.lock().remove(cluster);
        self.inner.wal.unfence_cluster(cluster, u64::MAX);
    }

    /// What a cluster's latest commit points reference. (Its in-memory WAL footprint is
    /// [`Wal::cluster_stats`].)
    pub async fn cluster_usage(&self, cluster: &str) -> anyhow::Result<ClusterUsage> {
        let mut u = ClusterUsage::default();
        for (_, m) in self.read_latest_manifests(cluster).await? {
            u.shards += 1;
            u.segments += m.segments.len() as u64;
            u.segment_bytes += m.segment_bytes();
            for s in &m.segments {
                u.docs += u64::from(s.max_doc.saturating_sub(s.del_count));
                u.deleted_docs += u64::from(s.del_count);
                u.cached_bytes += self.inner.cache.size_of(&s.id.file_name()).unwrap_or(0);
                if let Some(live) = s.live {
                    u.cached_bytes += self.inner.cache.size_of(&s.id.live_file_name(live)).unwrap_or(0);
                }
            }
        }
        u.state_generation = self.list_versions(&keys::state_prefix(cluster), ".json.zst").await?.last().map(|v| v.generation);
        Ok(u)
    }

    pub fn stats(&self) -> StoreStats {
        let c = &self.inner.counters;
        let (mut staging_files, mut staging_bytes) = (0, 0);
        if let Ok(rd) = std::fs::read_dir(&self.inner.staging) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    staging_files += 1;
                    staging_bytes += m.len();
                }
            }
        }
        StoreStats {
            wal: self.inner.wal.stats(),
            cache: self.inner.cache.stats(),
            segments_uploaded: c.segments_uploaded.load(Relaxed),
            segment_bytes_uploaded: c.segment_bytes_uploaded.load(Relaxed),
            live_docs_written: c.live_docs_written.load(Relaxed),
            manifests_written: c.manifests_written.load(Relaxed),
            manifests_deleted: c.manifests_deleted.load(Relaxed),
            states_written: c.states_written.load(Relaxed),
            branches: c.branches.load(Relaxed),
            clusters_deleted: c.clusters_deleted.load(Relaxed),
            gc_runs: c.gc_runs.load(Relaxed),
            gc_deleted: c.gc_deleted.load(Relaxed),
            gc_deleted_bytes: c.gc_deleted_bytes.load(Relaxed),
            fenced: c.fenced.load(Relaxed),
            staging_files,
            staging_bytes,
            tracked_shards: self.inner.shards.lock().len() as u64,
            tracked_clusters: self.inner.states.lock().len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_prefixes_tell_their_age() {
        assert_eq!(super::test_prefix_nanos("seg-progressive-4242-1700000000123456789-7"), Some(1_700_000_000_123_456_789));
        assert_eq!(super::test_prefix_nanos("x-1-2-3"), Some(2));
        for bad in ["", "plain", "a-b-c-d", "name-1-2", "name-x-2-3", "name-1-2-x"] {
            assert_eq!(super::test_prefix_nanos(bad), None, "{bad}");
        }
    }
}

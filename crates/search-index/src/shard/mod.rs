//! One shard's engine: writes, realtime reads, refresh, flush and merges.
//!
//! # Concurrency
//!
//! Index and delete operations run concurrently. Each holds, for its whole duration, a shared
//! lock on the writer state (segments and buffers can't change under it) and the lock of the
//! version-map stripe of its `_id` (serializing operations on one id, as Elasticsearch's
//! per-uid lock does). It borrows a free indexing buffer from a pool, so threads write to
//! different buffers. Refresh, merge installation and buffer conversion take the state lock
//! exclusively only to swap lists; writing segments happens outside every lock, and a buffer
//! being written is only read (an `RwLock`), so gets and updates of its docs don't wait.
//!
//! # Versions and locations
//!
//! The live version map holds, per recently written id, its seq no / term / version and, for
//! docs still in an indexing buffer, where they are; deletes keep a tombstone (in a map of
//! their own) for `index.gc_deletes`, pruned by later deletes and refreshes. Both count in
//! the shard's memory figures; entries of buffered docs also count toward the buffer budget.
//! Ids not in the map are looked up in the segments (`_id` terms,
//! Bloom-filtered) with their `_seq_no`, `_version` and `_primary_term` doc values. When a
//! buffer becomes a segment its map entries are dropped (the segment answers from then on),
//! so the map only ever points into buffers.
//!
//! # Visibility and durability
//!
//! A refresh turns every buffer into an in-memory segment and publishes a new point-in-time
//! [`Searcher`]. Converting buffers because of memory pressure does not publish (Elasticsearch's
//! internal refresh). A flush writes segments as files into the shard directory (when one is
//! configured) and returns the commit for `search-store` to upload: segment files, live docs and
//! the local checkpoint covered, all captured in one critical section with no write in flight,
//! so a commit holds every operation whole or not at all. Callers apply operations here first
//! and then append them to the WAL with the returned sequence numbers (a write that fails
//! after getting one returns [`Error::Rejected`] with it, for a no-op record); replay uses
//! [`Origin::Replay`], which tolerates any order and duplicates.
//!
//! # Limits
//!
//! A shard holds at most [`ShardConfig::max_docs`] docs (Lucene's `IndexWriter.MAX_DOCS`,
//! deleted docs included until merged away), so global doc ids always fit a `u32`.
//!
//! # Cold segments
//!
//! A segment still downloading opens through [`gate`]: only what readers parse must be in;
//! postings and stored documents are awaited per field on first use, merges wait for the whole
//! file, and writes never wait.
//!
//! # Idle cost
//!
//! No thread or timer exists unless a merge is running: merge threads are spawned when the
//! policy finds work and exit when there is none.

pub mod checkpoint;
pub mod gate;
pub mod ops;

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::{Condvar, Mutex, RwLock};
use search_codec::stored::{OwnedStoredValue, StoredOptions, decode_doc};
use search_codec::{
    BpOptions, LiveDocs, MergeInput, MergeSource, PostingsFlags, RemappedSource, SegmentReader, Slice, SpillOptions,
    WriteOptions, bp_reorder, write_segment_with,
};

use crate::buffer::source::BufferSource;
use crate::buffer::{Buffer, DocMeta};
use crate::error::{Error, Result};
use crate::merge::policy::{MergePolicyConfig, SegInfo};
use crate::merge::sort::block_sort_map;
use crate::schema::{ID_FIELD, PRIMARY_TERM_FIELD, SEQ_NO_FIELD, Schema, VERSION_FIELD};
use crate::search::{SegmentCore, Searcher};
use crate::util::bitset::FixedBitSet;
use checkpoint::Tracker;
pub use gate::SegmentGate;
pub use ops::{
    ConflictKind, DeleteOp, IndexOp, MATCH_ANY, NOT_FOUND, OpResult, OpType, Origin, VersionConflict, VersionType,
};

const STRIPES: usize = 64;
/// Segment attribute marking segments that contain nested blocks.
pub const NESTED_ATTR: &str = "roll.nested";

/// Shard settings.
#[derive(Clone, Debug)]
pub struct ShardConfig {
    /// The primary term operations get (`_primary_term`).
    pub primary_term: i64,
    /// Where segment files (`<id hex>.seg`) are written by flushes and merges, and where
    /// writing spills; `None` keeps everything in memory.
    pub dir: Option<PathBuf>,
    /// Indexing buffer budget: past it, the largest buffer becomes a segment (unpublished).
    pub max_buffer_bytes: usize,
    pub merge_policy: MergePolicyConfig,
    /// Concurrent background merges (`index.merge.scheduler.max_thread_count`); 0 runs
    /// merges in the thread that triggers them.
    pub merge_threads: usize,
    /// `index.gc_deletes`: how long delete tombstones stay in the version map.
    pub gc_deletes: Duration,
    /// Checkpoint of the segments opened (from the manifest); default: their max seq no.
    pub local_checkpoint: Option<i64>,
    /// Reorder merged segments with BP when the index is not sorted.
    pub bp_reorder: bool,
    /// Train a zstd dictionary for stored fields of refreshed segments (merges always do).
    pub refresh_stored_dictionary: bool,
    /// Threads building HNSW graphs while writing a segment.
    pub vector_threads: usize,
    /// Cache doc sets of recurring costly filters (`index.queries.cache.enabled`).
    pub query_cache: bool,
    /// Most docs (nested children and deleted docs not yet merged away included) the shard
    /// holds: Lucene's `IndexWriter.MAX_DOCS` ([`MAX_DOCS`]), which keeps every global doc id
    /// in a `u32`. Lower only in tests.
    pub max_docs: u64,
}

/// Lucene's `IndexWriter.MAX_DOCS`: the most docs one shard holds.
pub const MAX_DOCS: u64 = search_codec::MAX_DOCS as u64;

/// `ReferenceDocs.LUCENE_MAX_DOCS_LIMIT` as Elasticsearch 9.5 renders it.
const MAX_DOCS_LIMIT_DOCS: &str = "https://www.elastic.co/docs/deploy-manage/production-guidance/optimize-performance/size-shards?version=9.5#troubleshooting-max-docs-limit";

impl Default for ShardConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        ShardConfig {
            primary_term: 1,
            dir: None,
            max_buffer_bytes: 64 << 20,
            merge_policy: MergePolicyConfig::default(),
            merge_threads: (cores / 2).clamp(1, 4),
            gc_deletes: Duration::from_secs(60),
            local_checkpoint: None,
            bp_reorder: true,
            refresh_stored_dictionary: false,
            vector_threads: cores.min(8),
            query_cache: true,
            max_docs: MAX_DOCS,
        }
    }
}

/// An existing segment to open the shard with.
#[derive(Clone)]
pub struct SegmentHandle {
    pub data: Slice,
    /// Live docs (`None`: no deletions).
    pub live: Option<LiveDocs>,
    /// The file backing `data`, if any.
    pub path: Option<PathBuf>,
    /// Set when `data` is still downloading ([`SegmentHandle::fetching`]).
    pub gate: Option<Arc<dyn gate::SegmentGate>>,
}

impl SegmentHandle {
    /// A segment over bytes already in memory or mapped.
    pub fn new(data: Slice) -> SegmentHandle {
        SegmentHandle { data, live: None, path: None, gate: None }
    }

    /// A memory-mapped segment file.
    pub fn from_file(path: impl Into<PathBuf>) -> Result<SegmentHandle> {
        let path = path.into();
        Ok(SegmentHandle { path: Some(path.clone()), ..SegmentHandle::new(Slice::map_file(&path)?) })
    }

    /// A segment whose file is still downloading: `data` maps the whole file (bytes not yet
    /// arrived read as zeros, as search-store's `SegmentFetch::map_partial` gives it) and
    /// `gate` tells which ranges are in. The shard opens once the parts a reader parses are
    /// in and waits for the rest per field as queries need it ([`gate`]). Such a segment is
    /// the store's already: flushes report it as not new and never copy its bytes.
    pub fn fetching(data: Slice, gate: Arc<dyn gate::SegmentGate>) -> SegmentHandle {
        SegmentHandle { gate: Some(gate), ..SegmentHandle::new(data) }
    }

    /// Adds encoded live docs (as returned in a [`FlushOutcome`]).
    pub fn with_live_bytes(mut self, bytes: &[u8]) -> Result<SegmentHandle> {
        if let Some(g) = &self.gate {
            g.wait_ready(std::slice::from_ref(&(0..search_codec::container::HEADER_LEN as u64)))?;
        }
        let id = segment_id_of(&self.data)?;
        self.live = Some(LiveDocs::decode(bytes, id)?.0);
        Ok(self)
    }
}

/// The segment id in a segment file's header.
fn segment_id_of(data: &[u8]) -> Result<[u8; 16]> {
    use search_codec::container::{HEADER_LEN, MAGIC};
    if data.len() < HEADER_LEN || data[..4] != MAGIC {
        return Err(search_codec::Error::Corrupt("bad segment header magic".into()).into());
    }
    Ok(data[8..24].try_into().unwrap())
}

/// A segment of a commit.
#[derive(Clone, Debug)]
pub struct FlushedSegment {
    pub id: [u8; 16],
    /// The segment file (`<id hex>.seg` in the shard directory), when one is configured.
    pub path: Option<PathBuf>,
    /// The segment bytes (zero-copy: a memory map or the in-memory buffer).
    pub data: Slice,
    pub max_doc: u32,
    pub del_count: u32,
    /// Live docs encoded with [`LiveDocs::encode`], when the segment has deletions.
    pub live_docs: Option<Vec<u8>>,
    /// Whether the live docs changed since the previous flush.
    pub live_changed: bool,
    /// Whether this flush is the first to report the segment.
    pub new: bool,
}

/// A commit point for the store.
#[derive(Clone, Debug)]
pub struct FlushOutcome {
    /// Every segment of the shard, in order.
    pub segments: Vec<FlushedSegment>,
    pub max_seq_no: i64,
    /// Every operation with a seq no up to this is in `segments`.
    pub local_checkpoint: i64,
    pub primary_term: i64,
}

/// A document read by id.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredDoc {
    pub seq_no: i64,
    pub primary_term: i64,
    pub version: i64,
    /// Stored fields (`_id` is field 0).
    pub fields: Vec<(u32, OwnedStoredValue)>,
}

/// Memory and size figures of a shard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShardStats {
    pub num_docs: u64,
    pub deleted_docs: u64,
    pub segments: usize,
    pub segment_bytes: u64,
    /// Segment bytes held in heap (refreshed but not yet flushed to files).
    pub segment_memory_bytes: u64,
    pub segment_heap_bytes: u64,
    pub buffer_bytes: u64,
    pub buffered_docs: u64,
    pub version_map_entries: u64,
    /// Estimated heap of the version map, delete tombstones included (Elasticsearch's
    /// `segments.version_map_memory_in_bytes`). Entries of buffered docs count toward the
    /// indexing buffer budget (a conversion frees them); tombstones live `gc_deletes`.
    pub version_map_bytes: u64,
    pub merges_running: usize,
    /// Docs (`max_doc` sum) and bytes of the segments being merged now.
    pub merges_current_docs: u64,
    pub merges_current_bytes: u64,
    /// Finished merges, with the docs and bytes of their input segments and their run time
    /// (Elasticsearch's `merges.total`, `total_docs`, `total_size_in_bytes`, `total_time`).
    pub merges_total: u64,
    pub merges_total_docs: u64,
    pub merges_total_bytes: u64,
    pub merges_total_time: Duration,
    /// Refreshes that published a new searcher.
    pub refreshes_total: u64,
    /// [`Shard::refresh`] calls, changed or not, and the time spent in them (Elasticsearch
    /// counts every refresh in `refresh.total` / `total_time_in_millis`).
    pub refresh_calls: u64,
    pub refresh_time: Duration,
    pub flushes_total: u64,
    pub flush_time: Duration,
    pub max_seq_no: i64,
    pub local_checkpoint: i64,
}

/// Info about one segment.
#[derive(Clone, Debug)]
pub struct SegmentStatus {
    pub id: [u8; 16],
    pub max_doc: u32,
    pub del_count: u32,
    pub size_bytes: u64,
    pub in_memory: bool,
    pub searchable: bool,
}

struct BufferSlot {
    uid: u64,
    /// Written only while checked out to one writer; a frozen buffer (being written as a
    /// segment) is only read, so realtime gets and deletes of its docs never wait for the
    /// segment write.
    buf: RwLock<Buffer>,
    deletes: Mutex<Vec<u32>>,
}

struct LiveState {
    live: Option<Arc<LiveDocs>>,
    lgen: u64,
    flushed_gen: u64,
}

struct SegEntry {
    uid: u64,
    id: [u8; 16],
    max_doc: u32,
    size: u64,
    core: RwLock<Arc<SegmentCore>>,
    path: Mutex<Option<PathBuf>>,
    /// A file we wrote that no flush has handed to the caller yet (removed when dropped).
    owned_file: Mutex<Option<PathBuf>>,
    live: Mutex<LiveState>,
    merging: AtomicBool,
    reported: AtomicBool,
    nested: bool,
    /// Opened while its file was downloading: the store's already (never persisted here).
    fetching: bool,
}

impl Drop for SegEntry {
    fn drop(&mut self) {
        if let Some(p) = self.owned_file.get_mut().take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl SegEntry {
    fn new(uid: u64, reader: SegmentReader, live: Option<LiveDocs>, path: Option<PathBuf>, owned: bool) -> SegEntry {
        SegEntry::with_gate(uid, reader, live, path, owned, None, false)
    }

    fn with_gate(
        uid: u64,
        reader: SegmentReader,
        live: Option<LiveDocs>,
        path: Option<PathBuf>,
        owned: bool,
        gate: Option<Arc<gate::Gate>>,
        fetching: bool,
    ) -> SegEntry {
        let live = live.filter(|l| l.num_deleted() > 0).map(Arc::new);
        let nested = reader.info().attributes.get(NESTED_ATTR).is_some_and(|v| v == "true");
        SegEntry {
            uid,
            id: reader.id(),
            max_doc: reader.max_doc(),
            size: reader.size_in_bytes(),
            fetching,
            core: RwLock::new(Arc::new(SegmentCore::with_gate(uid, reader, gate))),
            owned_file: Mutex::new(if owned { path.clone() } else { None }),
            path: Mutex::new(path),
            live: Mutex::new(LiveState { live, lgen: 0, flushed_gen: 0 }),
            merging: AtomicBool::new(false),
            reported: AtomicBool::new(false),
            nested,
        }
    }

    fn reader(&self) -> SegmentReader {
        self.core.read().reader.clone()
    }

    /// Whether every byte of the file is readable (merges read all of it).
    fn is_complete(&self) -> bool {
        self.core.read().is_complete()
    }

    /// Blocks until every byte of the file is readable.
    fn wait_complete(&self) -> Result<()> {
        let g = self.core.read().gate.clone();
        match g {
            Some(g) => g.wait_complete(),
            None => Ok(()),
        }
    }

    fn del_count(&self) -> u32 {
        self.live.lock().live.as_ref().map_or(0, |l| l.num_deleted())
    }


    /// Deletes docs; returns how many were live.
    fn delete(&self, docs: impl IntoIterator<Item = u32>) -> u32 {
        let mut st = self.live.lock();
        let max_doc = self.max_doc;
        let l = st.live.get_or_insert_with(|| Arc::new(LiveDocs::all_live(max_doc)));
        let l = Arc::make_mut(l);
        let mut n = 0;
        for d in docs {
            if d < max_doc && l.delete(d) {
                n += 1;
            }
        }
        if n > 0 {
            st.lgen += 1;
        }
        n
    }

    /// The doc range of the block whose root is `root`.
    fn block(&self, root: u32) -> std::ops::Range<u32> {
        if !self.nested {
            return root..root + 1;
        }
        let r = self.reader();
        let Some(pt) = r.numeric(PRIMARY_TERM_FIELD) else { return root..root + 1 };
        let docs = pt.docs();
        let start = match docs.rank(root) {
            Some(0) | None => 0,
            Some(k) => docs.select(k - 1).map_or(0, |p| p + 1),
        };
        start..root + 1
    }

    /// The live root doc with `_id == id` and its seq no, version and term.
    fn lookup(&self, id: &[u8], hash: u64) -> Option<(u32, i64, i64, i64)> {
        let core = self.core.read();
        let r = &core.reader;
        let terms = r.terms(ID_FIELD)?;
        let st = terms.seek_exact_hashed(id, hash)?;
        let mut pc = terms.postings(&st, PostingsFlags::DOCS);
        let live = self.live.lock();
        let mut found = None;
        loop {
            let d = pc.next();
            if d == search_codec::NO_MORE_DOCS {
                break;
            }
            if live.live.as_ref().is_none_or(|l| l.is_live(d)) {
                found = Some(d);
            }
        }
        drop(live);
        let d = found?;
        let get = |f: u32| r.numeric(f).and_then(|v| v.get(d)).unwrap_or(0);
        Some((d, get(SEQ_NO_FIELD), get(VERSION_FIELD), get(PRIMARY_TERM_FIELD)))
    }
}

/// A buffered (not yet in a segment) doc of an id.
struct VersionValue {
    seq_no: i64,
    term: i64,
    version: i64,
    slot: Arc<BufferSlot>,
    doc: u32,
}

/// A delete, remembered for `index.gc_deletes` (and until the checkpoint passes it).
struct Tombstone {
    seq_no: i64,
    version: i64,
    at: Instant,
}

/// Version-map memory per entry: key and value inline in the table, its control byte and
/// load-factor slack, the key's own allocation (the id is copied: a key must never keep a
/// whole request body alive). An estimate, as Elasticsearch's `RamUsageEstimator` figures are.
const fn entry_bytes<V>(id_len: usize) -> usize {
    (std::mem::size_of::<(Bytes, V)>() + 1) * 8 / 7 + id_len + 16
}

/// Live entries (buffered docs, dropped when their buffer becomes a segment) and tombstones
/// are separate maps: pruning tombstones walks only tombstones, and an id is in at most one.
#[derive(Default)]
struct Stripe {
    map: HashMap<Bytes, VersionValue>,
    tombstones: HashMap<Bytes, Tombstone>,
}

/// Where the current live doc of an id is.
enum Current {
    NotFound,
    Deleted { seq_no: i64, version: i64 },
    Live { seq_no: i64, term: i64, version: i64, loc: Loc },
}

enum Loc {
    Buffer(Arc<BufferSlot>, u32),
    Segment(Arc<SegEntry>, u32),
}

struct State {
    schema: Arc<Schema>,
    segments: Vec<Arc<SegEntry>>,
    buffers: Mutex<Vec<Arc<BufferSlot>>>,
    frozen: Vec<Arc<BufferSlot>>,
    query_cache: Option<Arc<crate::search::cache::QueryCachePolicy>>,
}

#[derive(Default)]
struct MergeState {
    running: usize,
    queue: std::collections::VecDeque<Vec<Arc<SegEntry>>>,
    total: u64,
    current_docs: u64,
    current_bytes: u64,
    total_docs: u64,
    total_bytes: u64,
    total_time: Duration,
    error: Option<String>,
}

struct Inner {
    cfg: ShardConfig,
    state: RwLock<State>,
    stripes: Box<[Mutex<Stripe>]>,
    free: Mutex<Vec<Arc<BufferSlot>>>,
    tracker: Mutex<Tracker>,
    next_seq: AtomicI64,
    max_seq: AtomicI64,
    refresh_lock: Mutex<()>,
    published: RwLock<Searcher>,
    dirty: AtomicBool,
    ram: AtomicUsize,
    uids: AtomicU64,
    generation: AtomicU64,
    merges: Mutex<MergeState>,
    merge_done: Condvar,
    closed: AtomicBool,
    refreshes: AtomicU64,
    refresh_calls: AtomicU64,
    refresh_nanos: AtomicU64,
    flushes: AtomicU64,
    flush_nanos: AtomicU64,
    /// Serializes flushes (each commit's live-docs generations build on the previous one's).
    flush_lock: Mutex<()>,
    /// Docs held: segments' `max_doc` plus buffered docs (Lucene's `pendingNumDocs`).
    pending_docs: AtomicU64,
    /// Version-map bytes of buffered docs' entries and of tombstones.
    vm_live_bytes: AtomicUsize,
    vm_tombstone_bytes: AtomicUsize,
    /// When tombstones were last pruned (ms since `epoch`).
    last_prune_ms: AtomicU64,
    epoch: Instant,
    forced: Mutex<Forced>,
    weak: Mutex<Weak<Inner>>,
}

/// The segments running force merges are to merge (Lucene's `segmentsToMerge`): those
/// present when a force merge started and whatever merges of them produce. Segments refreshed
/// later are not, so a force merge ends even under steady indexing.
#[derive(Default)]
struct Forced {
    active: usize,
    segs: HashMap<u64, Lineage>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lineage {
    /// Present when the force merge started (Lucene's `isOriginal`).
    Original,
    /// Present and already in a merge then: that merge's result counts as original too.
    OriginalMerging,
    /// Produced by a merge of tracked segments.
    Merged,
}

/// What a flush commits, captured in one critical section with no write in flight: every
/// segment with its live docs then, and the buffers being converted with how many of their
/// deletes were in. Writes after it (their deletes land in these segments' live docs before
/// the flush returns) are left out whole, so a commit never holds half of an update.
struct CommitView {
    segments: Vec<(Arc<SegEntry>, Option<Arc<LiveDocs>>, u64)>,
    buffers: HashMap<u64, usize>,
    converted: Vec<(Arc<SegEntry>, Option<Arc<LiveDocs>>, u64)>,
    checkpoint: i64,
    max_seq_no: i64,
}

/// One shard: the writer and its reader manager. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct Shard {
    inner: Arc<Inner>,
}

/// The version-map stripe of an id and the id's term hash (the codec's Bloom hash, so segment
/// lookups don't hash again).
fn stripe_of(id: &[u8]) -> (usize, u64) {
    let h = search_codec::bloom::hash64(id);
    (((h >> 32) as usize) % STRIPES, h)
}

/// A new random segment id.
pub fn new_segment_id() -> [u8; 16] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let s = foldhash::fast::RandomState::default();
    let a = s.hash_one((t, c, std::process::id()));
    let b = s.hash_one((c, t ^ 0x9e37_79b9, a));
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&a.to_le_bytes());
    id[8..].copy_from_slice(&b.to_le_bytes());
    id
}

/// Hex of a segment id (file names).
pub fn hex_id(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

impl Shard {
    /// Opens a shard over existing segments (empty for a new shard).
    pub fn open(cfg: ShardConfig, schema: Schema, segments: Vec<SegmentHandle>) -> Result<Shard> {
        let schema = Arc::new(schema);
        let mut entries = Vec::with_capacity(segments.len());
        let mut max_seq = -1i64;
        let mut uid = 1u64;
        for h in segments {
            let fetching = h.gate.is_some();
            let gate = match h.gate {
                Some(g) => gate::Gate::open(&h.data, g)?,
                None => None,
            };
            let reader = SegmentReader::open(h.data)?;
            if let Some(l) = &h.live
                && l.max_doc() != reader.max_doc()
            {
                return Err(Error::illegal_state("live docs do not match the segment"));
            }
            if let Some(p) = reader.points(SEQ_NO_FIELD) {
                let m = p.max_packed_value();
                if m.len() == 8 {
                    max_seq = max_seq.max(search_codec::points::encode::i64_from(m));
                }
            }
            entries.push(Arc::new(SegEntry::with_gate(uid, reader, h.live, h.path, false, gate, fetching)));
            entries.last().unwrap().reported.store(true, Ordering::Relaxed);
            uid += 1;
        }
        let checkpoint = cfg.local_checkpoint.unwrap_or(max_seq);
        let docs: u64 = entries.iter().map(|e| e.max_doc as u64).sum();
        if docs > cfg.max_docs {
            return Err(Error::illegal_state(format!(
                "Too many documents: an index cannot exceed {} but readers have total maxDoc={docs}",
                cfg.max_docs
            )));
        }
        let mut stripes = Vec::with_capacity(STRIPES);
        stripes.resize_with(STRIPES, || Mutex::new(Stripe::default()));
        let state = State {
            schema: schema.clone(),
            segments: entries,
            buffers: Mutex::new(Vec::new()),
            frozen: Vec::new(),
            query_cache: cfg.query_cache.then(|| Arc::new(crate::search::cache::QueryCachePolicy::default())),
        };
        let searcher = Self::make_searcher(&state, 0);
        let inner = Arc::new(Inner {
            cfg,
            state: RwLock::new(state),
            stripes: stripes.into_boxed_slice(),
            free: Mutex::new(Vec::new()),
            tracker: Mutex::new(Tracker::new(checkpoint)),
            next_seq: AtomicI64::new(max_seq.max(checkpoint) + 1),
            max_seq: AtomicI64::new(max_seq.max(checkpoint)),
            refresh_lock: Mutex::new(()),
            published: RwLock::new(searcher),
            dirty: AtomicBool::new(false),
            ram: AtomicUsize::new(0),
            uids: AtomicU64::new(uid),
            generation: AtomicU64::new(1),
            merges: Mutex::new(MergeState::default()),
            merge_done: Condvar::new(),
            closed: AtomicBool::new(false),
            refreshes: AtomicU64::new(0),
            refresh_calls: AtomicU64::new(0),
            refresh_nanos: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            flush_nanos: AtomicU64::new(0),
            flush_lock: Mutex::new(()),
            pending_docs: AtomicU64::new(docs),
            vm_live_bytes: AtomicUsize::new(0),
            vm_tombstone_bytes: AtomicUsize::new(0),
            last_prune_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            forced: Mutex::new(Forced::default()),
            weak: Mutex::new(Weak::new()),
        });
        *inner.weak.lock() = Arc::downgrade(&inner);
        Ok(Shard { inner })
    }

    /// A new empty in-memory shard.
    pub fn in_memory(schema: Schema) -> Shard {
        Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).expect("empty shard")
    }

    pub fn config(&self) -> &ShardConfig {
        &self.inner.cfg
    }

    fn make_searcher(st: &State, generation: u64) -> Searcher {
        let segs = st
            .segments
            .iter()
            .map(|e| (e.core.read().clone(), e.live.lock().live.clone()))
            .collect();
        let s = Searcher::new(segs, st.schema.clone(), generation);
        if st.query_cache.is_some() { s.with_query_cache(st.query_cache.clone()) } else { s }
    }

    /// The schema in use.
    pub fn schema(&self) -> Arc<Schema> {
        self.inner.state.read().schema.clone()
    }

    /// Replaces the schema (new fields from mapping updates). Existing field numbers must keep
    /// their indexing options.
    pub fn update_schema(&self, schema: Schema) -> Result<()> {
        let mut st = self.inner.state.write();
        for f in st.schema.fields() {
            if let Some(n) = schema.field(f.number)
                && (n.index != f.index || n.doc_values != f.doc_values || n.points != f.points || n.vectors != f.vectors)
            {
                return Err(Error::illegal(format!("cannot change how field [{}] is indexed", f.name)));
            }
        }
        st.schema = Arc::new(schema);
        let s = st.schema.clone();
        let mut p = self.inner.published.write();
        *p = p.with_schema(s);
        Ok(())
    }

    /// The last published point-in-time searcher.
    pub fn searcher(&self) -> Searcher {
        self.inner.published.read().clone()
    }

    fn check_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        Ok(())
    }

    fn resolve(st: &State, stripe: &Stripe, id: &[u8], hash: u64) -> Current {
        if let Some(v) = stripe.map.get(id) {
            return Current::Live {
                seq_no: v.seq_no,
                term: v.term,
                version: v.version,
                loc: Loc::Buffer(v.slot.clone(), v.doc),
            };
        }
        if let Some(t) = stripe.tombstones.get(id) {
            return Current::Deleted { seq_no: t.seq_no, version: t.version };
        }
        for e in st.segments.iter().rev() {
            if let Some((d, seq, ver, term)) = e.lookup(id, hash) {
                return Current::Live { seq_no: seq, term, version: ver, loc: Loc::Segment(e.clone(), d) };
            }
        }
        Current::NotFound
    }

    fn delete_loc(loc: &Loc) {
        match loc {
            Loc::Buffer(slot, root) => {
                let start = {
                    let b = slot.buf.read();
                    if b.has_nested {
                        b.roots.prev_set_bit(root.saturating_sub(1)).filter(|_| *root > 0).map_or(0, |p| p + 1)
                    } else {
                        *root
                    }
                };
                let mut d = slot.deletes.lock();
                d.extend(start..=*root);
            }
            Loc::Segment(e, root) => {
                e.delete(e.block(*root));
            }
        }
    }

    fn checkout(&self, st: &State) -> Arc<BufferSlot> {
        if let Some(s) = self.inner.free.lock().pop() {
            return s;
        }
        let uid = self.inner.uids.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(BufferSlot {
            uid,
            buf: RwLock::new(Buffer::new(uid, st.schema.clone())),
            deletes: Mutex::new(Vec::new()),
        });
        st.buffers.lock().push(slot.clone());
        slot
    }

    /// Adds a block whose docs were reserved ([`Shard::reserve_docs`]) and releases what the
    /// block did not use (docs of a failed block that were started stay, deleted, until a
    /// merge drops them).
    fn add_to_buffer(&self, st: &State, doc: &crate::doc::IndexableDoc, meta: DocMeta) -> Result<(Arc<BufferSlot>, u32)> {
        let slot = self.checkout(st);
        let r = {
            let mut b = slot.buf.write();
            if !Arc::ptr_eq(&b.schema, &st.schema) {
                b.set_schema(st.schema.clone());
            }
            let before = b.ram_bytes();
            let docs_before = b.max_doc();
            let r = b.add_block(doc, meta);
            let after = b.ram_bytes();
            if after >= before {
                self.inner.ram.fetch_add(after - before, Ordering::Relaxed);
            } else {
                self.inner.ram.fetch_sub(before - after, Ordering::Relaxed);
            }
            let unused = (doc.doc_count() as u64).saturating_sub((b.max_doc() - docs_before) as u64);
            if unused > 0 {
                self.inner.pending_docs.fetch_sub(unused, Ordering::AcqRel);
            }
            r
        };
        self.inner.free.lock().push(slot.clone());
        r.map(|d| (slot, d))
    }

    /// Reserves room for `n` more docs, or fails as Elasticsearch's `tryAcquireInFlightDocs`
    /// (before a sequence number is given, so the failure uses none).
    fn reserve_docs(&self, n: u64) -> Result<()> {
        let max = self.inner.cfg.max_docs;
        let total = self.inner.pending_docs.fetch_add(n, Ordering::AcqRel) + n;
        if total > max {
            self.inner.pending_docs.fetch_sub(n, Ordering::AcqRel);
            return Err(too_many_docs(max));
        }
        Ok(())
    }

    /// Records a version-map entry for a buffered doc, replacing whatever the id had.
    fn put_live(&self, stripe: &mut Stripe, id: &Bytes, v: VersionValue) {
        if !stripe.tombstones.is_empty() && stripe.tombstones.remove(&id[..]).is_some() {
            self.inner.vm_tombstone_bytes.fetch_sub(entry_bytes::<Tombstone>(id.len()), Ordering::Relaxed);
        }
        match stripe.map.get_mut(&id[..]) {
            Some(old) => *old = v,
            None => {
                stripe.map.insert(Bytes::copy_from_slice(id), v);
                self.inner.vm_live_bytes.fetch_add(entry_bytes::<VersionValue>(id.len()), Ordering::Relaxed);
            }
        }
    }

    /// Records a delete tombstone, replacing whatever the id had.
    fn put_tombstone(&self, stripe: &mut Stripe, id: &Bytes, t: Tombstone) {
        if !stripe.map.is_empty() && stripe.map.remove(&id[..]).is_some() {
            self.inner.vm_live_bytes.fetch_sub(entry_bytes::<VersionValue>(id.len()), Ordering::Relaxed);
        }
        match stripe.tombstones.get_mut(&id[..]) {
            Some(old) => *old = t,
            None => {
                stripe.tombstones.insert(Bytes::copy_from_slice(id), t);
                self.inner.vm_tombstone_bytes.fetch_add(entry_bytes::<Tombstone>(id.len()), Ordering::Relaxed);
            }
        }
    }

    /// Drops tombstones older than `gc_deletes` whose op the local checkpoint covers
    /// (Elasticsearch's `pruneDeletedTombstones`), and with `converted` the entries of buffers
    /// that became segments. Locks one stripe at a time.
    fn prune_version_map(&self, converted: Option<&HashSet<u64>>) {
        let inner = &self.inner;
        let now = Instant::now();
        let gc = inner.cfg.gc_deletes;
        let checkpoint = inner.tracker.lock().checkpoint();
        let (mut live, mut dead) = (0usize, 0usize);
        for s in inner.stripes.iter() {
            let mut s = s.lock();
            if let Some(c) = converted {
                s.map.retain(|id, v| {
                    let keep = !c.contains(&v.slot.uid);
                    if !keep {
                        live += entry_bytes::<VersionValue>(id.len());
                    }
                    keep
                });
            }
            s.tombstones.retain(|id, t| {
                let keep = now.duration_since(t.at) < gc || t.seq_no > checkpoint;
                if !keep {
                    dead += entry_bytes::<Tombstone>(id.len());
                }
                keep
            });
        }
        inner.vm_live_bytes.fetch_sub(live, Ordering::Relaxed);
        inner.vm_tombstone_bytes.fetch_sub(dead, Ordering::Relaxed);
        inner.last_prune_ms.store(now.duration_since(inner.epoch).as_millis() as u64, Ordering::Relaxed);
    }

    /// Prunes tombstones at most every quarter of `gc_deletes` (Elasticsearch's
    /// `maybePruneDeletes`, run after deletes) and never more often than every 100 ms (its
    /// clock's resolution is 200 ms): a delete-heavy shard that never refreshes keeps only
    /// `gc_deletes` worth of tombstones.
    fn maybe_prune_deletes(&self) {
        let inner = &self.inner;
        let every = (inner.cfg.gc_deletes / 4).max(Duration::from_millis(100)).as_millis() as u64;
        let now = inner.epoch.elapsed().as_millis() as u64;
        let last = inner.last_prune_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) > every
            && inner.last_prune_ms.compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        {
            self.prune_version_map(None);
        }
    }

    fn mark_processed(&self, seq: i64) {
        self.inner.tracker.lock().mark(seq);
        self.inner.max_seq.fetch_max(seq, Ordering::AcqRel);
    }

    /// Indexes (creates or replaces) a document.
    pub fn index(&self, op: IndexOp) -> Result<OpResult> {
        self.check_open()?;
        let id = op.doc.id.clone();
        if id.is_empty() {
            return Err(Error::illegal("if _id is specified it must not be empty"));
        }
        let (si, hash) = stripe_of(&id);
        let docs = op.doc.doc_count() as u64;
        let result = {
            let st = self.inner.state.read();
            crate::buffer::precheck(&st.schema, &op.doc)?;
            let mut stripe = self.inner.stripes[si].lock();
            let skip_lookup = op.auto_generated_id && matches!(op.origin, Origin::Primary);
            let cur = if skip_lookup { Current::NotFound } else { Self::resolve(&st, &stripe, &id, hash) };
            let (cur_seq, cur_version, deleted) = match &cur {
                Current::NotFound => (ops::UNASSIGNED_SEQ_NO, NOT_FOUND, true),
                Current::Deleted { seq_no, version } => (*seq_no, *version, true),
                Current::Live { seq_no, version, .. } => (*seq_no, *version, false),
            };
            let (seq_no, term, version) = match op.origin {
                Origin::Primary => {
                    if op.if_seq_no.is_some() || op.if_primary_term.is_some() {
                        let s = op.if_seq_no.unwrap_or(ops::UNASSIGNED_SEQ_NO);
                        let t = op.if_primary_term.unwrap_or(0);
                        match &cur {
                            Current::Live { seq_no, term, .. } if *seq_no == s && *term == t => {}
                            Current::Live { seq_no, term, .. } => {
                                return Ok(OpResult::Conflict(VersionConflict {
                                    id,
                                    kind: ConflictKind::SeqNo { if_seq_no: s, if_primary_term: t, current: Some((*seq_no, *term)) },
                                }));
                            }
                            _ => {
                                return Ok(OpResult::Conflict(VersionConflict {
                                    id,
                                    kind: ConflictKind::SeqNo { if_seq_no: s, if_primary_term: t, current: None },
                                }));
                            }
                        }
                    }
                    if op.op_type == OpType::Create && !deleted {
                        return Ok(OpResult::Conflict(VersionConflict {
                            id,
                            kind: ConflictKind::Exists { current: cur_version },
                        }));
                    }
                    let expected = op.version.unwrap_or(MATCH_ANY);
                    if ops::is_conflict(op.version_type, cur_version, expected, deleted) {
                        return Ok(OpResult::Conflict(VersionConflict {
                            id,
                            kind: ConflictKind::Version { version_type: op.version_type, current: cur_version, expected, deleted },
                        }));
                    }
                    let version = ops::update_version(op.version_type, cur_version, expected);
                    // Too many docs fails before a seq no is given, as in Elasticsearch.
                    self.reserve_docs(docs)?;
                    let seq = self.inner.next_seq.fetch_add(1, Ordering::AcqRel);
                    (seq, self.inner.cfg.primary_term, version)
                }
                Origin::Replay { seq_no, primary_term, version } => {
                    self.inner.next_seq.fetch_max(seq_no + 1, Ordering::AcqRel);
                    if cur_seq >= seq_no {
                        drop(stripe);
                        drop(st);
                        self.mark_processed(seq_no);
                        return Ok(OpResult::Stale { seq_no });
                    }
                    (seq_no, primary_term, version)
                }
            };
            let reserved = match op.origin {
                Origin::Primary => Ok(()),
                Origin::Replay { .. } => self.reserve_docs(docs),
            };
            let meta = DocMeta { seq_no, version, primary_term: term };
            match reserved.and_then(|()| self.add_to_buffer(&st, &op.doc, meta)) {
                Ok((slot, d)) => {
                    let created = !matches!(cur, Current::Live { .. });
                    if let Current::Live { loc, .. } = &cur {
                        Self::delete_loc(loc);
                    }
                    self.put_live(&mut stripe, &id, VersionValue { seq_no, term, version, slot, doc: d });
                    Ok(OpResult::Indexed { seq_no, primary_term: term, version, created })
                }
                Err(e) => Err((seq_no, term, e)),
            }
        };
        self.inner.dirty.store(true, Ordering::Release);
        match result {
            Ok(r) => {
                if let OpResult::Indexed { seq_no, .. } = r {
                    self.mark_processed(seq_no);
                }
                // The op is applied whatever happens here: a conversion that fails leaves its
                // buffers in place and fails the next refresh or flush instead.
                let _ = self.after_write();
                Ok(r)
            }
            Err((seq_no, primary_term, e)) => {
                // The seq no was used: processed here, and the caller logs a no-op for it.
                self.mark_processed(seq_no);
                Err(Error::Rejected { seq_no, primary_term, source: Box::new(e) })
            }
        }
    }

    /// Deletes a document by id.
    pub fn delete(&self, op: DeleteOp) -> Result<OpResult> {
        self.check_open()?;
        let id = op.id.clone();
        let (si, hash) = stripe_of(&id);
        let r = {
            let st = self.inner.state.read();
            let mut stripe = self.inner.stripes[si].lock();
            let cur = Self::resolve(&st, &stripe, &id, hash);
            let (cur_seq, cur_version, deleted) = match &cur {
                Current::NotFound => (ops::UNASSIGNED_SEQ_NO, NOT_FOUND, true),
                Current::Deleted { seq_no, version } => (*seq_no, *version, true),
                Current::Live { seq_no, version, .. } => (*seq_no, *version, false),
            };
            let (seq_no, term, version) = match op.origin {
                Origin::Primary => {
                    if op.if_seq_no.is_some() || op.if_primary_term.is_some() {
                        let s = op.if_seq_no.unwrap_or(ops::UNASSIGNED_SEQ_NO);
                        let t = op.if_primary_term.unwrap_or(0);
                        match &cur {
                            Current::Live { seq_no, term, .. } if *seq_no == s && *term == t => {}
                            Current::Live { seq_no, term, .. } => {
                                return Ok(OpResult::Conflict(VersionConflict {
                                    id,
                                    kind: ConflictKind::SeqNo { if_seq_no: s, if_primary_term: t, current: Some((*seq_no, *term)) },
                                }));
                            }
                            _ => {
                                return Ok(OpResult::Conflict(VersionConflict {
                                    id,
                                    kind: ConflictKind::SeqNo { if_seq_no: s, if_primary_term: t, current: None },
                                }));
                            }
                        }
                    }
                    let expected = op.version.unwrap_or(MATCH_ANY);
                    if ops::is_conflict(op.version_type, cur_version, expected, deleted) {
                        return Ok(OpResult::Conflict(VersionConflict {
                            id,
                            kind: ConflictKind::Version { version_type: op.version_type, current: cur_version, expected, deleted },
                        }));
                    }
                    let version = ops::update_version(op.version_type, cur_version, expected);
                    // Elasticsearch reserves a doc for the delete's tombstone; ours is no doc,
                    // but a full shard rejects deletes alike.
                    if self.inner.pending_docs.load(Ordering::Acquire) + 1 > self.inner.cfg.max_docs {
                        return Err(too_many_docs(self.inner.cfg.max_docs));
                    }
                    (self.inner.next_seq.fetch_add(1, Ordering::AcqRel), self.inner.cfg.primary_term, version)
                }
                Origin::Replay { seq_no, primary_term, version } => {
                    self.inner.next_seq.fetch_max(seq_no + 1, Ordering::AcqRel);
                    if cur_seq >= seq_no {
                        drop(stripe);
                        drop(st);
                        self.mark_processed(seq_no);
                        return Ok(OpResult::Stale { seq_no });
                    }
                    (seq_no, primary_term, version)
                }
            };
            let found = matches!(cur, Current::Live { .. });
            if let Current::Live { loc, .. } = &cur {
                Self::delete_loc(loc);
            }
            self.put_tombstone(&mut stripe, &id, Tombstone { seq_no, version, at: Instant::now() });
            OpResult::Deleted { seq_no, primary_term: term, version, found }
        };
        if let OpResult::Deleted { seq_no, .. } = r {
            self.mark_processed(seq_no);
        }
        self.inner.dirty.store(true, Ordering::Release);
        self.maybe_prune_deletes();
        Ok(r)
    }

    /// Marks a sequence number processed without an operation (a no-op from the WAL).
    pub fn mark_seq_no_processed(&self, seq_no: i64) {
        self.inner.next_seq.fetch_max(seq_no + 1, Ordering::AcqRel);
        self.mark_processed(seq_no);
    }

    /// Reads a document by id: realtime (latest written state, buffers included) or from
    /// the last published searcher.
    pub fn get(&self, id: &[u8], realtime: bool) -> Result<Option<StoredDoc>> {
        if !realtime {
            return get_from_searcher(&self.searcher(), id);
        }
        let (si, hash) = stripe_of(id);
        let st = self.inner.state.read();
        let stripe = self.inner.stripes[si].lock();
        match Self::resolve(&st, &stripe, id, hash) {
            Current::NotFound | Current::Deleted { .. } => Ok(None),
            Current::Live { seq_no, term, version, loc } => {
                drop(stripe);
                // A segment still downloading may block on its documents: not under the lock.
                drop(st);
                let fields = match loc {
                    Loc::Buffer(slot, d) => {
                        let mut bytes = Vec::new();
                        slot.buf.read().stored(d, &mut bytes);
                        let mut v: Vec<(u32, OwnedStoredValue)> = Vec::new();
                        decode_doc(&bytes, &mut v)?;
                        v
                    }
                    Loc::Segment(e, d) => {
                        let core = e.core.read().clone();
                        core.ensure_stored()?;
                        core.reader.stored().document(d)?
                    }
                };
                Ok(Some(StoredDoc { seq_no, primary_term: term, version, fields }))
            }
        }
    }

    /// The seq no / term / version of the current doc of `id` (realtime), without reading it.
    pub fn version_of(&self, id: &[u8]) -> Option<(i64, i64, i64)> {
        let (si, hash) = stripe_of(id);
        let st = self.inner.state.read();
        let stripe = self.inner.stripes[si].lock();
        match Self::resolve(&st, &stripe, id, hash) {
            Current::Live { seq_no, term, version, .. } => Some((seq_no, term, version)),
            _ => None,
        }
    }

    /// Indexing memory a conversion frees: the buffers and their docs' version-map entries
    /// (Elasticsearch's `getIndexBufferRAMBytesUsed`; tombstones outlive conversions).
    fn indexing_bytes(&self) -> usize {
        self.inner.ram.load(Ordering::Relaxed) + self.inner.vm_live_bytes.load(Ordering::Relaxed)
    }

    fn after_write(&self) -> Result<()> {
        if self.indexing_bytes() > self.inner.cfg.max_buffer_bytes {
            // Convert the largest buffer; if another conversion is running, writers continue
            // unless memory is far over budget (then they wait for it: stall control).
            let far = self.indexing_bytes() > self.inner.cfg.max_buffer_bytes.saturating_mul(2);
            if far {
                self.convert(false, false)?;
            } else if let Some(g) = self.inner.refresh_lock.try_lock() {
                self.convert_locked(g, false, false, None)?;
            }
        }
        Ok(())
    }

    /// Makes every write so far searchable: buffers become segments and a new searcher is
    /// published. Returns whether anything changed.
    pub fn refresh(&self) -> Result<bool> {
        self.check_open()?;
        let t = Instant::now();
        let r = self.convert(true, true);
        self.inner.refresh_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.refresh_nanos.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }

    /// Converts buffers to segments without publishing them (Elasticsearch's internal
    /// refresh, e.g. to free indexing memory).
    pub fn write_buffers(&self) -> Result<()> {
        self.convert(true, false).map(|_| ())
    }

    fn convert(&self, all: bool, publish: bool) -> Result<bool> {
        let g = self.inner.refresh_lock.lock();
        self.convert_locked(g, all, publish, None)
    }

    /// Turns buffers (all, or the largest) into segments, installs them and, with `publish`,
    /// a new searcher. With `commit`, also captures the [`CommitView`] of a flush (`all` only).
    fn convert_locked(
        &self,
        guard: parking_lot::MutexGuard<'_, ()>,
        all: bool,
        publish: bool,
        mut commit: Option<&mut Option<CommitView>>,
    ) -> Result<bool> {
        let inner = &self.inner;
        let (selected, schema) = {
            let mut st = inner.state.write();
            let selected: Vec<Arc<BufferSlot>> = {
                let mut bufs = st.buffers.lock();
                let mut free = inner.free.lock();
                let mut chosen: Vec<Arc<BufferSlot>> = Vec::new();
                if all {
                    chosen = bufs.iter().filter(|b| b.buf.read().max_doc() > 0).cloned().collect();
                } else if let Some(b) = bufs.iter().max_by_key(|b| b.buf.read().ram_bytes())
                    && b.buf.read().max_doc() > 0
                {
                    chosen.push(b.clone());
                }
                let uids: HashSet<u64> = chosen.iter().map(|b| b.uid).collect();
                bufs.retain(|b| !uids.contains(&b.uid));
                free.retain(|b| !uids.contains(&b.uid));
                chosen
            };
            st.frozen.extend(selected.iter().cloned());
            if let Some(c) = commit.as_deref_mut() {
                // No write is in flight under the exclusive lock: every op applied so far is
                // whole in a segment or a selected buffer, and the tracker's checkpoint covers
                // only ops applied (they are marked after they apply).
                let segments = st
                    .segments
                    .iter()
                    .map(|e| {
                        let l = e.live.lock();
                        (e.clone(), l.live.clone(), l.lgen)
                    })
                    .collect();
                let buffers = selected.iter().map(|b| (b.uid, b.deletes.lock().len())).collect();
                *c = Some(CommitView {
                    segments,
                    buffers,
                    converted: Vec::new(),
                    checkpoint: inner.tracker.lock().checkpoint(),
                    // Every seq no given or replayed so far; ops that applied but are not yet
                    // marked processed are in the commit too.
                    max_seq_no: inner.next_seq.load(Ordering::Acquire) - 1,
                });
            }
            (selected, st.schema.clone())
        };
        let written: Vec<Result<(SegmentReader, Option<search_codec::source::DocMap>)>> = if selected.len() > 1 {
            std::thread::scope(|s| {
                let hs: Vec<_> = selected
                    .iter()
                    .map(|slot| {
                        let schema = schema.clone();
                        s.spawn(move || self.write_buffer(&slot.buf.read(), &schema))
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(Error::illegal_state("refresh panicked")))).collect()
            })
        } else {
            selected.iter().map(|slot| self.write_buffer(&slot.buf.read(), &schema)).collect()
        };
        if let Some(i) = written.iter().position(|r| r.is_err()) {
            // Back to the writers: the docs stay buffered (and realtime-readable) and the next
            // conversion tries again.
            let mut st = inner.state.write();
            let uids: HashSet<u64> = selected.iter().map(|b| b.uid).collect();
            st.frozen.retain(|s| !uids.contains(&s.uid));
            st.buffers.lock().extend(selected.iter().cloned());
            inner.free.lock().extend(selected.iter().cloned());
            drop(st);
            drop(guard);
            if let Some(c) = commit {
                *c = None;
            }
            return Err(written.into_iter().nth(i).unwrap().unwrap_err());
        }
        // Install.
        let changed = {
            let mut st = inner.state.write();
            let mut freed = 0usize;
            let mut converted = HashSet::new();
            for (slot, r) in selected.iter().zip(written) {
                let (reader, map) = r.expect("checked above");
                let b = slot.buf.read();
                freed += b.ram_bytes();
                let dels = slot.deletes.lock().clone();
                let in_commit = commit.as_deref().and_then(|c| c.as_ref()).and_then(|c| c.buffers.get(&slot.uid).copied());
                let to_live = |dels: &[u32]| {
                    if dels.is_empty() && b.aborted.is_empty() {
                        return None;
                    }
                    let mut l = LiveDocs::all_live(reader.max_doc());
                    for &d in dels.iter().chain(b.aborted.iter()) {
                        let nd = match &map {
                            Some(m) => m.old_to_new(d),
                            None => Some(d),
                        };
                        if let Some(nd) = nd {
                            l.delete(nd);
                        }
                    }
                    Some(l)
                };
                // Deletes since the commit view (by ops it leaves out) are not in the commit.
                let commit_live = in_commit.filter(|&n| n < dels.len()).map(|n| to_live(&dels[..n]).map(Arc::new));
                let live = to_live(&dels);
                let delta = reader.max_doc() as i64 - b.max_doc() as i64;
                drop(b);
                if delta != 0 {
                    inner.pending_docs.fetch_add(delta as u64, Ordering::AcqRel);
                }
                let uid = inner.uids.fetch_add(1, Ordering::Relaxed);
                let entry = Arc::new(SegEntry::new(uid, reader, live, None, false));
                if let Some(c) = commit.as_deref_mut().and_then(|c| c.as_mut()) {
                    match commit_live {
                        // The live docs differ from the committed ones: a later flush reports them.
                        Some(cl) => {
                            entry.live.lock().lgen = 1;
                            c.converted.push((entry.clone(), cl, 0));
                        }
                        None => {
                            let l = entry.live.lock().live.clone();
                            c.converted.push((entry.clone(), l, 0));
                        }
                    }
                }
                st.segments.push(entry);
                converted.insert(slot.uid);
            }
            st.frozen.retain(|s| !converted.contains(&s.uid));
            inner.ram.fetch_sub(freed.min(inner.ram.load(Ordering::Relaxed)), Ordering::Relaxed);
            self.prune_version_map(Some(&converted));
            let dirty = inner.dirty.swap(false, Ordering::AcqRel) || !converted.is_empty();
            if publish {
                if dirty {
                    let g = inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
                    *inner.published.write() = Self::make_searcher(&st, g);
                    inner.refreshes.fetch_add(1, Ordering::Relaxed);
                }
                dirty
            } else {
                if dirty {
                    inner.dirty.store(true, Ordering::Release);
                }
                !converted.is_empty()
            }
        };
        drop(guard);
        self.maybe_merge()?;
        Ok(changed)
    }

    fn write_options(&self, refresh: bool) -> WriteOptions {
        let mut stored = StoredOptions::default();
        if refresh {
            stored.dictionary = self.inner.cfg.refresh_stored_dictionary;
        }
        WriteOptions {
            stored,
            vector_threads: self.inner.cfg.vector_threads.max(1),
            spill: SpillOptions { dir: self.inner.cfg.dir.clone(), ..SpillOptions::default() },
        }
    }

    fn write_buffer(&self, buf: &Buffer, schema: &Schema) -> Result<(SegmentReader, Option<search_codec::source::DocMap>)> {
        let mut attributes = std::collections::BTreeMap::new();
        if buf.has_nested {
            attributes.insert(NESTED_ATTR.to_string(), "true".to_string());
        }
        let src = BufferSource { buf, id: new_segment_id(), attributes };
        let opts = self.write_options(true);
        let mut out = Vec::with_capacity((buf.ram_bytes() / 3).max(4096));
        let map = if let Some(sort) = &schema.index_sort {
            let roots = buf.has_nested.then_some(&buf.roots);
            let map = block_sort_map(&src, sort, roots)?;
            if map.is_identity() {
                let r = RemappedSource::new(&src, map, Some(sort.clone()))?;
                write_segment_with(&r, &mut out, &opts)?;
                None
            } else {
                let r = RemappedSource::new(&src, map.clone(), Some(sort.clone()))?;
                write_segment_with(&r, &mut out, &opts)?;
                Some(map)
            }
        } else {
            write_segment_with(&src, &mut out, &opts)?;
            None
        };
        let reader = SegmentReader::open(Slice::from(out))?;
        Ok((reader, map))
    }

    /// Writes refreshed segments to files (when a directory is configured) and returns the
    /// commit point: segments, live docs and the checkpoint they cover.
    pub fn flush(&self) -> Result<FlushOutcome> {
        self.check_open()?;
        let t = Instant::now();
        let r = self.flush_inner();
        self.inner.flush_nanos.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }

    fn flush_inner(&self) -> Result<FlushOutcome> {
        let _serial = self.inner.flush_lock.lock();
        let mut view = None;
        {
            let g = self.inner.refresh_lock.lock();
            self.convert_locked(g, true, false, Some(&mut view))?;
        }
        let view = view.expect("captured by the conversion");
        let mut out = Vec::with_capacity(view.segments.len() + view.converted.len());
        // Segments merged away since the view are still the commit's (the caller keeps their
        // files until a later commit drops them).
        for (e, live, lgen) in view.segments.iter().chain(view.converted.iter()) {
            self.persist(e)?;
            let changed = {
                let mut l = e.live.lock();
                let changed = *lgen != l.flushed_gen;
                l.flushed_gen = *lgen;
                changed
            };
            let new = !e.reported.swap(true, Ordering::AcqRel);
            e.owned_file.lock().take();
            out.push(FlushedSegment {
                id: e.id,
                path: e.path.lock().clone(),
                data: e.reader().data().clone(),
                max_doc: e.max_doc,
                del_count: live.as_ref().map_or(0, |l| l.num_deleted()),
                live_docs: live.as_ref().map(|l| l.encode(e.id, *lgen)),
                live_changed: changed,
                new,
            });
        }
        self.inner.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(FlushOutcome {
            segments: out,
            max_seq_no: view.max_seq_no,
            local_checkpoint: view.checkpoint,
            primary_term: self.inner.cfg.primary_term,
        })
    }

    /// Writes an in-memory segment to `<dir>/<id>.seg` and maps it back (freeing heap).
    fn persist(&self, e: &SegEntry) -> Result<()> {
        let Some(dir) = &self.inner.cfg.dir else { return Ok(()) };
        if e.fetching || e.path.lock().is_some() {
            return Ok(());
        }
        let data = e.reader().data().clone();
        let path = write_file(dir, &e.id, &data)?;
        let mapped = Slice::map_file(&path)?;
        let reader = SegmentReader::open(mapped)?;
        *e.core.write() = Arc::new(SegmentCore::new(e.uid, reader));
        *e.path.lock() = Some(path.clone());
        *e.owned_file.lock() = Some(path);
        self.inner.dirty.store(true, Ordering::Release);
        Ok(())
    }

    // ---------------------------------------------------------------- merges

    fn seg_infos(st: &State) -> Vec<SegInfo> {
        st.segments
            .iter()
            .enumerate()
            .map(|(i, e)| SegInfo {
                id: i,
                size_bytes: e.size,
                max_doc: e.max_doc,
                del_count: e.del_count(),
                // A segment still downloading can't be merged yet (a merge reads every byte).
                merging: e.merging.load(Ordering::Acquire) || (e.fetching && !e.is_complete()),
                order: e.uid,
                original: true,
            })
            .collect()
    }

    /// Asks the merge policy for work and starts it (background threads, or inline with
    /// `merge_threads == 0`).
    pub fn maybe_merge(&self) -> Result<usize> {
        let groups = {
            let st = self.inner.state.read();
            let infos = Self::seg_infos(&st);
            let picks = self.inner.cfg.merge_policy.find_merges(&infos);
            let mut groups = Vec::new();
            for g in picks {
                let segs: Vec<Arc<SegEntry>> = g.iter().map(|&i| st.segments[i].clone()).collect();
                if !claim(&segs) {
                    continue;
                }
                groups.push(segs);
            }
            groups
        };
        let n = groups.len();
        if n == 0 {
            return Ok(0);
        }
        if self.inner.cfg.merge_threads == 0 {
            for g in groups {
                self.run_merge(g)?;
            }
            return Ok(n);
        }
        let mut m = self.inner.merges.lock();
        m.queue.extend(groups);
        while m.running < self.inner.cfg.merge_threads && !m.queue.is_empty() {
            m.running += 1;
            let weak = self.inner.weak.lock().clone();
            std::thread::Builder::new()
                .name("search-merge".into())
                .spawn(move || merge_worker(weak))
                .map_err(Error::Io)?;
        }
        Ok(n)
    }

    /// Waits until no merge is running or queued.
    pub fn wait_for_merges(&self) {
        let mut m = self.inner.merges.lock();
        while m.running > 0 || !m.queue.is_empty() {
            self.inner.merge_done.wait(&mut m);
        }
    }

    /// `_forcemerge`: merges down to `max_num_segments` (or only segments with deletions with
    /// `only_expunge_deletes`, or natural merges with neither), then flushes if asked.
    /// Down to `max_num_segments` covers the segments present at the call and what merging
    /// them produces, as Lucene's `forceMerge`: segments refreshed meanwhile are left to
    /// natural merges, so the call returns under steady indexing.
    pub fn force_merge(&self, max_num_segments: Option<usize>, only_expunge_deletes: bool) -> Result<()> {
        self.check_open()?;
        self.convert(true, false)?;
        let mut merged = false;
        let r = self.force_merge_loop(max_num_segments, only_expunge_deletes, &mut merged);
        // Elasticsearch's force merge flushes and refreshes: searchers see the result.
        if merged {
            self.convert(true, true)?;
        }
        r
    }

    fn force_merge_loop(&self, max_num_segments: Option<usize>, only_expunge_deletes: bool, merged: &mut bool) -> Result<()> {
        let fetching: Vec<Arc<SegEntry>> = self.inner.state.read().segments.iter().filter(|e| e.fetching).cloned().collect();
        for e in fetching {
            e.wait_complete()?;
        }
        let policy = &self.inner.cfg.merge_policy;
        let target = match max_num_segments {
            Some(n) if !only_expunge_deletes => n.max(1),
            _ => {
                // One round over every segment; merges already running are left alone (Lucene's
                // `forceMergeDeletes` waits only for its own).
                let groups = self.claim_groups(|infos| {
                    if only_expunge_deletes { policy.find_forced_deletes_merges(infos) } else { policy.find_merges(infos) }
                });
                for g in groups {
                    *merged = true;
                    self.run_merge(g)?;
                }
                return Ok(());
            }
        };
        let _tracking = ForcedGuard::begin(self);
        loop {
            self.wait_forced_merges();
            let groups = {
                let st = self.inner.state.read();
                let f = self.inner.forced.lock();
                let infos: Vec<SegInfo> = Self::seg_infos(&st)
                    .into_iter()
                    .filter_map(|mut i| {
                        let l = *f.segs.get(&st.segments[i.id].uid)?;
                        i.original = l != Lineage::Merged;
                        Some(i)
                    })
                    .collect();
                drop(f);
                let mut groups = Vec::new();
                for g in policy.find_forced_merges(&infos, target) {
                    let segs: Vec<Arc<SegEntry>> = g.iter().map(|&i| st.segments[i].clone()).collect();
                    if claim(&segs) {
                        groups.push(segs);
                    }
                }
                groups
            };
            if groups.is_empty() {
                // Nothing picked: done, unless a tracked segment just got claimed elsewhere.
                if !self.forced_merge_running() {
                    return Ok(());
                }
                continue;
            }
            for g in groups {
                *merged = true;
                self.run_merge(g)?;
            }
        }
    }

    /// Claims the groups `pick` chooses among the segments (those it can't claim are skipped).
    fn claim_groups(&self, pick: impl FnOnce(&[SegInfo]) -> Vec<Vec<usize>>) -> Vec<Vec<Arc<SegEntry>>> {
        let st = self.inner.state.read();
        let infos = Self::seg_infos(&st);
        let mut groups = Vec::new();
        for g in pick(&infos) {
            let segs: Vec<Arc<SegEntry>> = g.iter().map(|&i| st.segments[i].clone()).collect();
            if claim(&segs) {
                groups.push(segs);
            }
        }
        groups
    }

    /// Whether a segment a force merge tracks is in a merge (running or queued).
    fn forced_merge_running(&self) -> bool {
        let st = self.inner.state.read();
        let f = self.inner.forced.lock();
        st.segments.iter().any(|e| e.merging.load(Ordering::Acquire) && f.segs.contains_key(&e.uid))
    }

    /// Waits until no tracked segment is in a merge (Lucene's `maxNumSegmentsMergesPending`).
    /// Merges of other segments may go on: under steady indexing they always do.
    fn wait_forced_merges(&self) {
        while self.forced_merge_running() {
            // Background merges signal; inline ones (another thread's refresh) don't, hence
            // the timeout. Never wait holding the state lock (stats() takes both).
            let mut m = self.inner.merges.lock();
            self.inner.merge_done.wait_for(&mut m, Duration::from_millis(5));
        }
    }

    fn run_merge(&self, group: Vec<Arc<SegEntry>>) -> Result<()> {
        let docs: u64 = group.iter().map(|e| e.max_doc as u64).sum();
        let bytes: u64 = group.iter().map(|e| e.size).sum();
        {
            let mut m = self.inner.merges.lock();
            m.current_docs += docs;
            m.current_bytes += bytes;
        }
        let t = Instant::now();
        let r = self.do_merge(&group);
        let took = t.elapsed();
        for s in &group {
            s.merging.store(false, Ordering::Release);
        }
        let mut m = self.inner.merges.lock();
        m.current_docs -= docs;
        m.current_bytes -= bytes;
        m.total_time += took;
        if r.is_ok() {
            m.total_docs += docs;
            m.total_bytes += bytes;
        }
        m.total += 1;
        if let Err(e) = &r {
            m.error = Some(e.to_string());
        }
        drop(m);
        r
    }

    fn do_merge(&self, group: &[Arc<SegEntry>]) -> Result<()> {
        let inner = &self.inner;
        let snapshots: Vec<Option<Arc<LiveDocs>>> = group.iter().map(|e| e.live.lock().live.clone()).collect();
        let readers: Vec<SegmentReader> = group.iter().map(|e| e.reader()).collect();
        let inputs: Vec<MergeInput> = readers
            .iter()
            .zip(&snapshots)
            .map(|(r, l)| MergeInput { reader: r.clone(), live: l.clone() })
            .collect();
        let id = new_segment_id();
        let nested = group.iter().any(|e| e.nested);
        let schema = inner.state.read().schema.clone();
        let mut ms = MergeSource::new(inputs, id)?;
        let spill = SpillOptions { dir: inner.cfg.dir.clone(), ..SpillOptions::default() };
        ms.set_spill(spill.clone());
        if nested {
            let mut a = std::collections::BTreeMap::new();
            a.insert(NESTED_ATTR.to_string(), "true".to_string());
            ms.set_attributes(a);
        }
        let parents = if nested { Some(merged_roots(&ms, &readers, &snapshots)) } else { None };
        let opts = self.write_options(false);
        let (path, data, map) = {
            let mut sink = SegmentSink::new(inner.cfg.dir.as_deref(), &id)?;
            let map = if let Some(sort) = &schema.index_sort {
                if nested {
                    let m = block_sort_map(&ms, sort, parents.as_ref())?;
                    let r = RemappedSource::new(&ms, m.clone(), Some(sort.clone()))?;
                    write_segment_with(&r, &mut sink, &opts)?;
                    Some(m)
                } else {
                    ms.set_index_sort(Some(sort.clone()));
                    // The writer sorts by itself; compute the same map to carry deletes over.
                    let m = search_codec::sort_doc_map(&ms, sort)?;
                    let r = RemappedSource::new(&ms, m.clone(), Some(sort.clone()))?;
                    write_segment_with(&r, &mut sink, &opts)?;
                    Some(m)
                }
            } else if inner.cfg.bp_reorder && search_codec::SegmentSource::max_doc(&ms) >= 4096 * 4 {
                let bp = BpOptions { parents: parents.as_ref().map(|p| p.words().to_vec()), ..Default::default() };
                match bp_reorder(&ms, &bp)? {
                    Some(m) => {
                        let mut r = RemappedSource::new(&ms, m.clone(), None)?;
                        r.set_spill(spill.clone());
                        write_segment_with(&r, &mut sink, &opts)?;
                        Some(m)
                    }
                    None => {
                        write_segment_with(&ms, &mut sink, &opts)?;
                        None
                    }
                }
            } else {
                write_segment_with(&ms, &mut sink, &opts)?;
                None
            };
            let (path, data) = sink.finish()?;
            (path, data, map)
        };
        let reader = SegmentReader::open(data)?;
        // Install, carrying over deletions made while merging.
        let mut st = inner.state.write();
        let mut live: Option<LiveDocs> = None;
        for (i, e) in group.iter().enumerate() {
            let now = e.live.lock().live.clone();
            let Some(now) = now else { continue };
            let before = &snapshots[i];
            for d in 0..e.max_doc {
                if !now.is_live(d) && before.as_ref().is_none_or(|b| b.is_live(d)) {
                    let Some(nd) = ms.map_doc(i, d) else { continue };
                    let nd = match &map {
                        Some(m) => m.old_to_new(nd),
                        None => Some(nd),
                    };
                    if let Some(nd) = nd {
                        live.get_or_insert_with(|| LiveDocs::all_live(reader.max_doc())).delete(nd);
                    }
                }
            }
        }
        let uid = inner.uids.fetch_add(1, Ordering::Relaxed);
        let owned = path.is_some();
        // Deleted docs merged away free room under `max_docs`.
        let dropped = docs_of(group) - reader.max_doc() as u64;
        inner.pending_docs.fetch_sub(dropped, Ordering::AcqRel);
        let entry = Arc::new(SegEntry::new(uid, reader, live, path, owned));
        {
            // Cascade a running force merge (Lucene's `segmentsToMerge` update in commitMerge):
            // any merge of a tracked segment yields a tracked segment.
            let mut f = inner.forced.lock();
            if f.active > 0 {
                let mut lineage = None;
                for e in group {
                    if let Some(l) = f.segs.remove(&e.uid) {
                        lineage = Some(match (lineage, l) {
                            (Some(Lineage::Original), _) | (_, Lineage::OriginalMerging) => Lineage::Original,
                            _ => Lineage::Merged,
                        });
                    }
                }
                if let Some(l) = lineage {
                    f.segs.insert(uid, l);
                }
            }
        }
        let uids: HashSet<u64> = group.iter().map(|e| e.uid).collect();
        let pos = st.segments.iter().position(|e| uids.contains(&e.uid)).unwrap_or(st.segments.len());
        st.segments.retain(|e| !uids.contains(&e.uid));
        let at = pos.min(st.segments.len());
        st.segments.insert(at, entry);
        inner.dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Stops accepting writes and waits for merges.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.wait_for_merges();
    }

    /// Size and memory figures.
    pub fn stats(&self) -> ShardStats {
        let st = self.inner.state.read();
        let mut s = ShardStats { segments: st.segments.len(), ..Default::default() };
        for e in &st.segments {
            let del = e.del_count() as u64;
            s.num_docs += e.max_doc as u64 - del;
            s.deleted_docs += del;
            s.segment_bytes += e.size;
            if e.path.lock().is_none() {
                s.segment_memory_bytes += e.size;
            }
            let c = e.core.read();
            s.segment_heap_bytes += (c.reader.heap_bytes() + c.cache_bytes()) as u64;
        }
        for b in st.buffers.lock().iter().chain(st.frozen.iter()) {
            let b = b.buf.read();
            s.buffered_docs += b.max_doc() as u64;
        }
        s.buffer_bytes = self.inner.ram.load(Ordering::Relaxed) as u64;
        s.version_map_entries =
            self.inner.stripes.iter().map(|m| m.lock()).map(|m| (m.map.len() + m.tombstones.len()) as u64).sum();
        s.version_map_bytes = self.version_map_bytes() as u64;
        let m = self.inner.merges.lock();
        s.merges_running = m.running;
        s.merges_current_docs = m.current_docs;
        s.merges_current_bytes = m.current_bytes;
        s.merges_total = m.total;
        s.merges_total_docs = m.total_docs;
        s.merges_total_bytes = m.total_bytes;
        s.merges_total_time = m.total_time;
        s.refresh_calls = self.inner.refresh_calls.load(Ordering::Relaxed);
        s.refresh_time = Duration::from_nanos(self.inner.refresh_nanos.load(Ordering::Relaxed));
        s.flush_time = Duration::from_nanos(self.inner.flush_nanos.load(Ordering::Relaxed));
        s.refreshes_total = self.inner.refreshes.load(Ordering::Relaxed);
        s.flushes_total = self.inner.flushes.load(Ordering::Relaxed);
        s.max_seq_no = self.inner.max_seq.load(Ordering::Acquire);
        s.local_checkpoint = self.inner.tracker.lock().checkpoint();
        s
    }

    /// The segments of the shard (internal view, published or not).
    pub fn segments(&self) -> Vec<SegmentStatus> {
        let st = self.inner.state.read();
        let published: HashSet<u64> = self.searcher().segments().iter().map(|s| s.core.uid).collect();
        st.segments
            .iter()
            .map(|e| SegmentStatus {
                id: e.id,
                max_doc: e.max_doc,
                del_count: e.del_count(),
                size_bytes: e.size,
                in_memory: e.path.lock().is_none(),
                searchable: published.contains(&e.uid),
            })
            .collect()
    }

    /// Highest seq no assigned or replayed.
    pub fn max_seq_no(&self) -> i64 {
        self.inner.max_seq.load(Ordering::Acquire)
    }

    pub fn local_checkpoint(&self) -> i64 {
        self.inner.tracker.lock().checkpoint()
    }

    /// Bytes held by indexing buffers.
    pub fn buffer_bytes(&self) -> usize {
        self.inner.ram.load(Ordering::Relaxed)
    }

    /// Estimated bytes of the version map, tombstones included ([`ShardStats::version_map_bytes`]).
    pub fn version_map_bytes(&self) -> usize {
        self.inner.vm_live_bytes.load(Ordering::Relaxed) + self.inner.vm_tombstone_bytes.load(Ordering::Relaxed)
    }

    /// Docs the shard holds against [`ShardConfig::max_docs`]: segments' `max_doc` (deleted
    /// docs included until merged away) plus buffered docs.
    pub fn pending_docs(&self) -> u64 {
        self.inner.pending_docs.load(Ordering::Acquire)
    }

    /// Whether writes happened since the last published refresh.
    pub fn has_pending_changes(&self) -> bool {
        self.inner.dirty.load(Ordering::Acquire) || self.inner.ram.load(Ordering::Relaxed) > 0
    }
}

fn docs_of(group: &[Arc<SegEntry>]) -> u64 {
    group.iter().map(|e| e.max_doc as u64).sum()
}

/// Elasticsearch's `tryAcquireInFlightDocs` failure.
fn too_many_docs(max: u64) -> Error {
    Error::illegal(format!(
        "Number of documents in the shard cannot exceed [{max}]; for more information, see [{}]",
        MAX_DOCS_LIMIT_DOCS
    ))
}

/// Registers a force merge's segments for its whole run.
struct ForcedGuard<'a>(&'a Shard);

impl<'a> ForcedGuard<'a> {
    fn begin(shard: &'a Shard) -> ForcedGuard<'a> {
        let st = shard.inner.state.read();
        let mut f = shard.inner.forced.lock();
        f.active += 1;
        for e in &st.segments {
            let l = if e.merging.load(Ordering::Acquire) { Lineage::OriginalMerging } else { Lineage::Original };
            f.segs.insert(e.uid, l);
        }
        ForcedGuard(shard)
    }
}

impl Drop for ForcedGuard<'_> {
    fn drop(&mut self) {
        let mut f = self.0.inner.forced.lock();
        f.active -= 1;
        if f.active == 0 {
            f.segs.clear();
        }
    }
}

/// Marks every segment of a merge as merging; false (and nothing marked) when one already is.
fn claim(segs: &[Arc<SegEntry>]) -> bool {
    for (i, s) in segs.iter().enumerate() {
        if s.merging.swap(true, Ordering::AcqRel) {
            for t in &segs[..i] {
                t.merging.store(false, Ordering::Release);
            }
            return false;
        }
    }
    true
}

fn merge_worker(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let shard = Shard { inner };
        let job = {
            let mut m = shard.inner.merges.lock();
            match m.queue.pop_front() {
                Some(j) => j,
                None => {
                    m.running -= 1;
                    shard.inner.merge_done.notify_all();
                    return;
                }
            }
        };
        if shard.inner.closed.load(Ordering::Acquire) {
            for s in &job {
                s.merging.store(false, Ordering::Release);
            }
            continue;
        }
        let _ = shard.run_merge(job);
        // More work may have appeared (the merged segment can join a new tier).
        let _ = shard.queue_more_merges();
        shard.inner.merge_done.notify_all();
    }
}

impl Shard {
    fn queue_more_merges(&self) -> Result<()> {
        let groups = {
            let st = self.inner.state.read();
            let infos = Self::seg_infos(&st);
            let mut groups = Vec::new();
            for g in self.inner.cfg.merge_policy.find_merges(&infos) {
                let segs: Vec<Arc<SegEntry>> = g.iter().map(|&i| st.segments[i].clone()).collect();
                if !claim(&segs) {
                    continue;
                }
                groups.push(segs);
            }
            groups
        };
        self.inner.merges.lock().queue.extend(groups);
        Ok(())
    }
}

/// Root docs of a merge in its output numbering.
fn merged_roots(ms: &MergeSource, readers: &[SegmentReader], lives: &[Option<Arc<LiveDocs>>]) -> FixedBitSet {
    let mut b = FixedBitSet::new(search_codec::SegmentSource::max_doc(ms));
    for (i, r) in readers.iter().enumerate() {
        let Some(pt) = r.numeric(PRIMARY_TERM_FIELD) else { continue };
        let docs = pt.docs();
        let mut c = docs.cursor();
        loop {
            let d = c.next();
            if d == search_codec::NO_MORE_DOCS {
                break;
            }
            if lives[i].as_ref().is_some_and(|l| !l.is_live(d)) {
                continue;
            }
            if let Some(nd) = ms.map_doc(i, d) {
                b.set(nd);
            }
        }
    }
    b
}

/// Writes a segment to a file in the shard directory (atomically, fsynced) or to memory.
struct SegmentSink {
    file: Option<(std::io::BufWriter<std::fs::File>, PathBuf, PathBuf)>,
    mem: Vec<u8>,
}

impl SegmentSink {
    fn new(dir: Option<&std::path::Path>, id: &[u8; 16]) -> Result<SegmentSink> {
        match dir {
            Some(d) => {
                std::fs::create_dir_all(d)?;
                let fin = d.join(format!("{}.seg", hex_id(id)));
                let tmp = d.join(format!("{}.seg.tmp", hex_id(id)));
                let f = std::fs::File::create(&tmp)?;
                Ok(SegmentSink { file: Some((std::io::BufWriter::with_capacity(1 << 20, f), tmp, fin)), mem: Vec::new() })
            }
            None => Ok(SegmentSink { file: None, mem: Vec::new() }),
        }
    }

    fn finish(self) -> Result<(Option<PathBuf>, Slice)> {
        match self.file {
            Some((w, tmp, fin)) => {
                let f = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
                f.sync_all()?;
                drop(f);
                std::fs::rename(&tmp, &fin)?;
                if let Some(p) = fin.parent()
                    && let Ok(d) = std::fs::File::open(p)
                {
                    let _ = d.sync_all();
                }
                let data = Slice::map_file(&fin)?;
                Ok((Some(fin), data))
            }
            None => Ok((None, Slice::from(self.mem))),
        }
    }
}

impl std::io::Write for SegmentSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.file {
            Some((w, _, _)) => w.write(buf),
            None => {
                self.mem.extend_from_slice(buf);
                Ok(buf.len())
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.file {
            Some((w, _, _)) => w.flush(),
            None => Ok(()),
        }
    }
}

fn write_file(dir: &std::path::Path, id: &[u8; 16], data: &[u8]) -> Result<PathBuf> {
    let mut s = SegmentSink::new(Some(dir), id)?;
    std::io::Write::write_all(&mut s, data)?;
    Ok(s.finish()?.0.unwrap())
}

/// Reads a doc by id from a searcher (non-realtime get).
pub fn get_from_searcher(s: &Searcher, id: &[u8]) -> Result<Option<StoredDoc>> {
    let h = search_codec::bloom::hash64(id);
    for seg in s.segments().iter().rev() {
        let Some(terms) = seg.reader.terms(ID_FIELD) else { continue };
        let Some(st) = terms.seek_exact_hashed(id, h) else { continue };
        let mut pc = terms.postings(&st, PostingsFlags::DOCS);
        let mut found = None;
        loop {
            let d = pc.next();
            if d == search_codec::NO_MORE_DOCS {
                break;
            }
            if seg.is_live(d) {
                found = Some(d);
            }
        }
        if let Some(d) = found {
            seg.ensure_stored()?;
            let r = &seg.reader;
            let get = |f: u32| r.numeric(f).and_then(|v| v.get(d)).unwrap_or(0);
            return Ok(Some(StoredDoc {
                seq_no: get(SEQ_NO_FIELD),
                primary_term: get(PRIMARY_TERM_FIELD),
                version: get(VERSION_FIELD),
                fields: r.stored().document(d)?,
            }));
        }
    }
    Ok(None)
}

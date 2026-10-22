//! The local object cache: segments and live-docs files on local disk, under a byte budget.
//!
//! Local disk only ever caches objects that are already in object storage, so evicting one is
//! always safe: a segment being written or uploaded lives in the staging directory and enters the
//! cache only once its upload finished (`Cache::adopt`). Files are immutable once cached, which
//! is what makes handing out memory maps of them sound.
//!
//! Opening returns a [`SegmentData`]: a shared memory map. Every opener of a file shares one map
//! while any of them holds it, and an entry with a live map is never evicted (the map would keep
//! the disk blocks allocated anyway). Eviction picks the least recently opened unmapped files
//! once the budget would be exceeded, and then frees down to `LOW_WATER` (95%) of the budget so a
//! full cache doesn't rank every file again on every miss; if every file is mapped the cache goes
//! over budget rather than failing reads, and counts it.
//!
//! A miss downloads the object to a temporary file, fsyncs it (a crash must never leave a
//! renamed but empty file that a restart would trust) and renames it into place. A download
//! reserves its size in the budget as soon as the size is known (evicting for it then), so many
//! concurrent cold downloads can't fill the disk past the budget before any of them lands. Concurrent
//! misses for one file share a single download, run as its own task so a caller that gives up
//! doesn't cancel it for the others. Objects larger than [`FETCH_PART`] download as parallel
//! ranged reads written in place (one S3 connection tops out around 100 MB/s; a cold shard
//! wants its segments at disk speed), with at most [`FETCH_PARTS`] parts in memory node-wide.
//!
//! Parts are aligned to the object's *end*: the first request is a suffix read, which learns
//! the size and brings the last part, where a segment keeps its footer and table of contents.
//! A reader that can't wait for a large segment uses [`SegmentFetch`]: it reads any range as
//! soon as the parts holding it are in ([`SegmentFetch::read`] moves them to the front of the
//! queue), so a query can start on the term dictionary and skip data while the postings are
//! still arriving. The download file is sized to the object (sparse) before the first part is
//! written, so it can also be memory-mapped whole from the start
//! ([`SegmentFetch::map_partial`]): a reader built over the whole file, like the codec's, opens
//! on it as soon as the sections it parses are in ([`SegmentFetch::wait_ready`]). Reads and
//! that map share one mapping per download, which becomes the cache entry's when the download
//! completes, so the cache counts it as in use and never evicts a file a reader still maps.
//!
//! With a [`Verifier`] (search-server passes the codec's whole-file checksum), a completed
//! download is checked before it enters the cache: a damaged object fails the download for
//! everyone waiting on it and is never cached, so the next open fetches it again. Files the
//! node wrote itself (uploads, live docs it wrote) and files an earlier run cached (fsynced
//! before they were renamed into place) are trusted.

use bytes::Bytes;
use memmap2::Mmap;
use parking_lot::Mutex;
use pg_storage::Objects;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Weak};
use tokio::sync::watch;

/// Checks a downloaded object before the cache keeps it: called with the whole file, mapped
/// ([`SegmentData::name`] tells the kind: `<id>.seg`, or `<id>.<gen>.liv` for live docs); an
/// error fails the download. Return `Ok` for kinds it doesn't check. The map can be wrapped
/// without copying (`Slice::from_owner(data.clone())` for the codec); don't keep it past the
/// call. Runs on the blocking thread pool.
#[derive(Clone)]
pub struct Verifier(Arc<VerifyFn>);

type VerifyFn = dyn Fn(&SegmentData) -> anyhow::Result<()> + Send + Sync;

impl Verifier {
    pub fn new(f: impl Fn(&SegmentData) -> anyhow::Result<()> + Send + Sync + 'static) -> Self {
        Verifier(Arc::new(f))
    }

    pub fn check(&self, data: &SegmentData) -> anyhow::Result<()> {
        (self.0)(data)
    }
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Verifier")
    }
}

/// A memory-mapped cached file. Cheap to clone; keeps its cache entry from being evicted.
#[derive(Clone)]
pub struct SegmentData(Arc<Mapped>);

struct Mapped {
    mmap: Mmap,
    name: String,
}

impl SegmentData {
    /// The file name in the cache (`<id>.seg` or `<id>.<gen>.liv`).
    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0.mmap
    }

    /// A zero-copy `Bytes` over the map (keeps it alive).
    pub fn to_bytes(&self) -> Bytes {
        Bytes::from_owner(self.clone())
    }
}

impl std::ops::Deref for SegmentData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0.mmap
    }
}

impl AsRef<[u8]> for SegmentData {
    fn as_ref(&self) -> &[u8] {
        &self.0.mmap
    }
}

impl std::fmt::Debug for SegmentData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SegmentData({}, {} bytes)", self.0.name, self.0.mmap.len())
    }
}

struct Entry {
    size: u64,
    last_used: u64,
    map: Weak<Mapped>,
}

struct State {
    /// Keyed by file name (`Arc` so ranking candidates for eviction doesn't copy names).
    entries: HashMap<Arc<str>, Entry>,
    used: u64,
    /// Bytes reserved by downloads in progress (their temporary files), counted against the
    /// budget with `used`.
    downloading: u64,
    tick: u64,
}

type FetchResult = Option<Result<SegmentData, String>>;

/// One download in progress, shared by everyone waiting for the file.
struct Download {
    /// The file's name in the cache.
    name: String,
    /// Final result (`None` while running).
    result: watch::Sender<FetchResult>,
    /// Bumped whenever a part lands, the size becomes known, or the download ends.
    tick: watch::Sender<u64>,
    /// `None` until the first response told the size.
    parts: Mutex<Option<Parts>>,
    /// The file being written: sized to the object (sparse) before any part lands, so it can be
    /// mapped whole while parts are still arriving.
    file: std::sync::OnceLock<Arc<File>>,
    /// The memory map of `file` handed to readers, shared while any holds it. Becomes the cache
    /// entry's map when the download completes, so the cache sees it as in use.
    map: Mutex<Weak<Mapped>>,
    /// Bytes this download reserved in the cache budget (0 once handed to the entry or released).
    reserved: AtomicU64,
}

impl Download {
    fn new(name: &str) -> Self {
        Download {
            name: name.to_string(),
            result: watch::Sender::new(None),
            tick: watch::Sender::new(0),
            parts: Mutex::new(None),
            file: std::sync::OnceLock::new(),
            map: Mutex::new(Weak::new()),
            reserved: AtomicU64::new(0),
        }
    }

    /// The shared map of the file being downloaded (whole object; parts not yet in read as
    /// zeros). Only valid once the size is known.
    fn map(&self) -> anyhow::Result<Arc<Mapped>> {
        let mut g = self.map.lock();
        if let Some(m) = g.upgrade() {
            return Ok(m);
        }
        let file = self.file.get().ok_or_else(|| anyhow::anyhow!("fetching {} has no file yet", self.name))?;
        // SAFETY: the file is only ever written with positional writes of downloaded parts
        // (each byte once, before its part is marked done) and never truncated, so a byte reads
        // as zero until its part is in and never changes afterwards. Callers expose only
        // ranges whose parts are done, except through `SegmentFetch::map_partial`, whose caller
        // promises the same.
        let mmap = unsafe { Mmap::map(&**file)? };
        let m = Arc::new(Mapped { mmap, name: self.name.clone() });
        *g = Arc::downgrade(&m);
        Ok(m)
    }

    /// Wait until every range's parts are in (or the download is over).
    async fn wait(&self, ranges: &[std::ops::Range<u64>], key: &str) -> anyhow::Result<()> {
        let mut tick = self.tick.subscribe();
        loop {
            match &*self.result.borrow() {
                Some(Ok(_)) => return Ok(()),
                Some(Err(e)) => anyhow::bail!("fetching {key}: {e}"),
                None => {}
            }
            if self.parts.lock().as_ref().is_some_and(|p| ranges.iter().all(|r| r.is_empty() || p.complete(r))) {
                return Ok(());
            }
            tick.changed().await.map_err(|_| anyhow::anyhow!("fetching {key} stopped"))?;
        }
    }
}

/// Which parts of a download are in, taken, or wanted first.
struct Parts {
    size: u64,
    /// Length of part 0; every later part is [`FETCH_PART`] long, so the last one is the
    /// suffix the first request read.
    head: u64,
    done: Vec<bool>,
    taken: Vec<bool>,
    done_count: usize,
    done_bytes: u64,
    /// Parts readers are waiting for, served before the sequential cursor.
    urgent: std::collections::VecDeque<usize>,
    next: usize,
}

impl Parts {
    fn new(size: u64) -> Self {
        let n = size.div_ceil(FETCH_PART).max(1) as usize;
        let head = size - (n as u64 - 1) * FETCH_PART;
        Parts { size, head, done: vec![false; n], taken: vec![false; n], done_count: 0, done_bytes: 0, urgent: Default::default(), next: 0 }
    }

    fn count(&self) -> usize {
        self.done.len()
    }

    fn range(&self, k: usize) -> std::ops::Range<u64> {
        let end = self.head + k as u64 * FETCH_PART;
        if k == 0 { 0..self.head } else { end - FETCH_PART..end }
    }

    fn part_of(&self, offset: u64) -> usize {
        if offset < self.head { 0 } else { 1 + ((offset - self.head) / FETCH_PART) as usize }
    }

    /// Parts overlapping `range` (non-empty, inside the object).
    fn covering(&self, range: &std::ops::Range<u64>) -> std::ops::RangeInclusive<usize> {
        self.part_of(range.start)..=self.part_of(range.end - 1)
    }

    fn complete(&self, range: &std::ops::Range<u64>) -> bool {
        self.covering(range).all(|k| self.done[k])
    }

    /// The next part to download: wanted ones first, then in file order.
    fn take(&mut self) -> Option<usize> {
        while let Some(k) = self.urgent.pop_front() {
            if !self.taken[k] {
                self.taken[k] = true;
                return Some(k);
            }
        }
        while self.next < self.count() {
            let k = self.next;
            self.next += 1;
            if !self.taken[k] {
                self.taken[k] = true;
                return Some(k);
            }
        }
        None
    }

    fn finish(&mut self, k: usize) {
        if !self.done[k] {
            self.done[k] = true;
            self.taken[k] = true;
            self.done_count += 1;
            let r = self.range(k);
            self.done_bytes += r.end - r.start;
        }
    }

    fn prioritize(&mut self, range: &std::ops::Range<u64>) {
        for k in self.covering(range) {
            if !self.taken[k] && !self.urgent.contains(&k) {
                self.urgent.push_back(k);
            }
        }
    }
}

/// A segment that may still be downloading ([`crate::Store::fetch_segment`]): ranges become
/// readable as the parts holding them arrive. Cheap to clone. Once complete it holds the memory
/// map, so like [`SegmentData`] it keeps the file from being evicted: switch to [`Self::data`]
/// and drop it.
#[derive(Clone)]
pub struct SegmentFetch(FetchState);

#[derive(Clone)]
enum FetchState {
    Cached(SegmentData),
    Pending { dl: Arc<Download>, size: u64, key: Arc<str> },
}

impl SegmentFetch {
    /// The segment's size.
    pub fn len(&self) -> u64 {
        match &self.0 {
            FetchState::Cached(d) => d.len() as u64,
            FetchState::Pending { size, .. } => *size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The memory-mapped segment, if the download has completed.
    pub fn data(&self) -> Option<SegmentData> {
        match &self.0 {
            FetchState::Cached(d) => Some(d.clone()),
            FetchState::Pending { dl, .. } => match &*dl.result.borrow() {
                Some(Ok(d)) => Some(d.clone()),
                _ => None,
            },
        }
    }

    /// Bytes downloaded so far.
    pub fn downloaded(&self) -> u64 {
        match &self.0 {
            FetchState::Cached(d) => d.len() as u64,
            FetchState::Pending { dl, .. } => dl.parts.lock().as_ref().map_or(0, |p| p.done_bytes),
        }
    }

    /// Whether `range` is downloaded (can be read without waiting). `false` for a range outside
    /// the segment.
    pub fn is_ready(&self, range: std::ops::Range<u64>) -> bool {
        match &self.0 {
            FetchState::Cached(d) => range.start <= range.end && range.end <= d.len() as u64,
            FetchState::Pending { dl, .. } => {
                // Out of range is never ready: `map_partial` users index by what this says.
                range.start <= range.end
                    && range.end <= self.len()
                    && (range.is_empty() || dl.parts.lock().as_ref().is_some_and(|p| p.complete(&range)))
            }
        }
    }

    /// Download the parts holding these ranges before the others (a reader that will need
    /// them soon: a term dictionary, skip data). Already-requested parts are unaffected.
    pub fn prioritize(&self, ranges: &[std::ops::Range<u64>]) {
        if let FetchState::Pending { dl, size, .. } = &self.0
            && let Some(p) = dl.parts.lock().as_mut()
        {
            for r in ranges.iter().filter(|r| !r.is_empty() && r.end <= *size) {
                p.prioritize(r);
            }
        }
    }

    /// Bytes `range` of the segment, as soon as the parts holding them are in. Zero-copy: a
    /// slice of the segment's memory map (which, like [`SegmentData`], keeps the file in the
    /// cache while held).
    pub async fn read(&self, range: std::ops::Range<u64>) -> anyhow::Result<Bytes> {
        anyhow::ensure!(range.start <= range.end && range.end <= self.len(), "range {range:?} is outside a segment of {} bytes", self.len());
        if range.is_empty() {
            return Ok(Bytes::new());
        }
        let (dl, key) = match &self.0 {
            FetchState::Cached(d) => return Ok(d.to_bytes().slice(range.start as usize..range.end as usize)),
            FetchState::Pending { dl, key, .. } => (dl, key),
        };
        self.prioritize(std::slice::from_ref(&range));
        dl.wait(std::slice::from_ref(&range), key).await?;
        let d = match self.data() {
            Some(d) => d,
            None => SegmentData(dl.map()?),
        };
        Ok(d.to_bytes().slice(range.start as usize..range.end as usize))
    }

    /// Wait until every one of `ranges` is downloaded, asking for them first (e.g. the header,
    /// table of contents and the sections a reader opens, then the postings a query needs).
    /// Fails for a range outside the segment, or when the download fails.
    pub async fn wait_ready(&self, ranges: &[std::ops::Range<u64>]) -> anyhow::Result<()> {
        let len = self.len();
        if let Some(r) = ranges.iter().find(|r| r.start > r.end || r.end > len) {
            anyhow::bail!("range {r:?} is outside a segment of {len} bytes");
        }
        match &self.0 {
            FetchState::Cached(_) => Ok(()),
            FetchState::Pending { dl, key, .. } => {
                self.prioritize(ranges);
                dl.wait(ranges, key).await
            }
        }
    }

    /// The whole segment as one memory map *now*, before the download completes, so a reader
    /// built over the whole file (the codec's `SegmentReader`) can start on the sections that
    /// are already in. Once the download completes this is the same map the cache hands out.
    ///
    /// # Safety
    ///
    /// Bytes whose parts aren't downloaded yet read as zeros and change when their part lands.
    /// The caller must read only ranges it has seen ready ([`Self::is_ready`],
    /// [`Self::wait_ready`], [`Self::read`]) or after [`Self::complete`]; anything else may
    /// observe bytes changing underneath a shared reference.
    pub unsafe fn map_partial(&self) -> anyhow::Result<SegmentData> {
        match &self.0 {
            FetchState::Cached(d) => Ok(d.clone()),
            FetchState::Pending { dl, key, .. } => {
                if let Some(d) = self.data() {
                    return Ok(d);
                }
                if let Some(Err(e)) = &*dl.result.borrow() {
                    anyhow::bail!("fetching {key}: {e}");
                }
                Ok(SegmentData(dl.map()?))
            }
        }
    }

    /// Wait for the whole segment (memory-mapped, as [`crate::Store::open_segment`] returns it).
    pub async fn complete(&self) -> anyhow::Result<SegmentData> {
        match &self.0 {
            FetchState::Cached(d) => Ok(d.clone()),
            FetchState::Pending { dl, key, .. } => {
                let mut rx = dl.result.subscribe();
                let r = rx.wait_for(|v| v.is_some()).await.map_err(|_| anyhow::anyhow!("fetching {key} was cancelled"))?.clone();
                r.expect("completed fetch").map_err(|e| anyhow::anyhow!(e))
            }
        }
    }
}

impl std::fmt::Debug for SegmentFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SegmentFetch({} of {} bytes)", self.downloaded(), self.len())
    }
}

/// Once eviction starts it frees down to this fraction of the budget (in 1/64ths: 61/64 ≈ 95%).
const LOW_WATER: u64 = 61;
/// Downloads read objects in ranges of this size; smaller objects take one request.
pub const FETCH_PART: u64 = 8 << 20;
/// Ranged reads in flight (and so buffered) at once across the node.
pub const FETCH_PARTS: usize = 8;

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    fetched_bytes: AtomicU64,
    evictions: AtomicU64,
    evicted_bytes: AtomicU64,
    over_budget: AtomicU64,
    verified: AtomicU64,
    verify_failures: AtomicU64,
}

/// Cache counters and gauges.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CacheStats {
    pub budget: u64,
    /// Bytes of cached files…
    pub used: u64,
    /// …and reserved by downloads in progress (both count against the budget).
    pub downloading: u64,
    pub files: u64,
    /// Files currently memory-mapped, and their bytes.
    pub mapped_files: u64,
    pub mapped_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub fetched_bytes: u64,
    pub evictions: u64,
    pub evicted_bytes: u64,
    /// Times an insert left the cache over budget because everything else was mapped.
    pub over_budget: u64,
    /// Downloads checked by the [`Verifier`], and those it refused (damaged objects).
    pub verified: u64,
    pub verify_failures: u64,
}

pub(crate) struct Cache {
    dir: PathBuf,
    tmp: PathBuf,
    budget: AtomicU64,
    objects: Objects,
    verify: Option<Verifier>,
    st: Mutex<State>,
    fetches: Mutex<HashMap<String, Arc<Download>>>,
    /// Permits for ranged reads in flight ([`FETCH_PARTS`]).
    parts: tokio::sync::Semaphore,
    counters: Counters,
}

impl Cache {
    /// Open the cache in `dir` (files) with `tmp` for downloads in progress, adopting whatever an
    /// earlier run left, oldest-modified first in eviction order.
    pub(crate) fn open(dir: PathBuf, tmp: PathBuf, budget: u64, objects: Objects, verify: Option<Verifier>) -> anyhow::Result<Arc<Cache>> {
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let mut found = Vec::new();
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata()?;
            if !meta.is_file() || crate::keys::parse_seg_file(&name).is_none() {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            found.push((meta.modified().ok(), name, meta.len()));
        }
        found.sort();
        let mut st = State { entries: HashMap::with_capacity(found.len()), used: 0, downloading: 0, tick: 0 };
        for (_, name, size) in found {
            st.tick += 1;
            st.used += size;
            st.entries.insert(name.into(), Entry { size, last_used: st.tick, map: Weak::new() });
        }
        let cache = Arc::new(Cache {
            dir,
            tmp,
            budget: AtomicU64::new(budget),
            objects,
            verify,
            st: Mutex::new(st),
            fetches: Mutex::new(HashMap::new()),
            parts: tokio::sync::Semaphore::new(FETCH_PARTS),
            counters: Counters::default(),
        });
        let evicted = cache.make_room(&mut cache.st.lock(), 0);
        unlink(evicted);
        Ok(cache)
    }

    pub(crate) fn set_budget(&self, bytes: u64) {
        self.budget.store(bytes, Relaxed);
        let evicted = self.make_room(&mut self.st.lock(), 0);
        unlink(evicted);
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.st.lock().entries.contains_key(name)
    }

    pub(crate) fn size_of(&self, name: &str) -> Option<u64> {
        self.st.lock().entries.get(name).map(|e| e.size)
    }

    /// Open a cached file, fetching `key` on a miss.
    pub(crate) async fn get(self: &Arc<Self>, name: &str, key: &str) -> anyhow::Result<SegmentData> {
        if let Some(d) = self.open_cached(name)? {
            self.counters.hits.fetch_add(1, Relaxed);
            return Ok(d);
        }
        let dl = match self.start(name, key)? {
            Ok(d) => return Ok(d),
            Err(dl) => dl,
        };
        let mut rx = dl.result.subscribe();
        let r = rx.wait_for(|v| v.is_some()).await.map_err(|_| anyhow::anyhow!("fetching {key} was cancelled"))?.clone();
        r.expect("completed fetch").map_err(|e| anyhow::anyhow!(e))
    }

    /// Like [`Cache::get`], but returns as soon as the object's size is known.
    pub(crate) async fn fetch_progressive(self: &Arc<Self>, name: &str, key: &str) -> anyhow::Result<SegmentFetch> {
        if let Some(d) = self.open_cached(name)? {
            self.counters.hits.fetch_add(1, Relaxed);
            return Ok(SegmentFetch(FetchState::Cached(d)));
        }
        let dl = match self.start(name, key)? {
            Ok(d) => return Ok(SegmentFetch(FetchState::Cached(d))),
            Err(dl) => dl,
        };
        let mut tick = dl.tick.subscribe();
        loop {
            if let Some(p) = dl.parts.lock().as_ref() {
                let size = p.size;
                return Ok(SegmentFetch(FetchState::Pending { dl: dl.clone(), size, key: key.into() }));
            }
            match &*dl.result.borrow() {
                Some(Ok(d)) => return Ok(SegmentFetch(FetchState::Cached(d.clone()))),
                Some(Err(e)) => anyhow::bail!("{e}"),
                None => {}
            }
            tick.changed().await.map_err(|_| anyhow::anyhow!("fetching {key} stopped"))?;
        }
    }

    /// Join the download of `name`, or start one. `Ok` when it turned out to be cached.
    fn start(self: &Arc<Self>, name: &str, key: &str) -> anyhow::Result<Result<SegmentData, Arc<Download>>> {
        let mut fetches = self.fetches.lock();
        if let Some(dl) = fetches.get(name) {
            return Ok(Err(dl.clone()));
        }
        // Re-check under the fetch lock: a fetch may have just finished.
        if let Some(d) = self.open_cached(name)? {
            self.counters.hits.fetch_add(1, Relaxed);
            return Ok(Ok(d));
        }
        self.counters.misses.fetch_add(1, Relaxed);
        let dl = Arc::new(Download::new(name));
        fetches.insert(name.to_string(), dl.clone());
        let me = self.clone();
        let (name, key, task_dl) = (name.to_string(), key.to_string(), dl.clone());
        tokio::spawn(async move {
            let r = me.fetch(&name, &key, &task_dl).await.map_err(|e| format!("{e:#}"));
            me.fetches.lock().remove(&name);
            task_dl.result.send_replace(Some(r));
            task_dl.tick.send_modify(|t| *t += 1);
        });
        Ok(Err(dl))
    }

    fn open_cached(&self, name: &str) -> anyhow::Result<Option<SegmentData>> {
        let mut st = self.st.lock();
        st.tick += 1;
        let tick = st.tick;
        let Some(e) = st.entries.get_mut(name) else { return Ok(None) };
        e.last_used = tick;
        if let Some(m) = e.map.upgrade() {
            return Ok(Some(SegmentData(m)));
        }
        let file = match File::open(self.dir.join(name)) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Removed behind our back: forget it and fetch again.
                let size = e.size;
                st.entries.remove(name);
                st.used -= size;
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        // SAFETY: cached files are immutable: they are written completely under a temporary name,
        // renamed into place, and only ever deleted (never truncated or rewritten) afterwards.
        let mmap = unsafe { Mmap::map(&file)? };
        let m = Arc::new(Mapped { mmap, name: name.to_string() });
        e.map = Arc::downgrade(&m);
        Ok(Some(SegmentData(m)))
    }

    async fn fetch(self: &Arc<Self>, name: &str, key: &str, dl: &Arc<Download>) -> anyhow::Result<SegmentData> {
        let tmp = self.tmp.join(format!("{name}.{:016x}.part", crate::random_u64()));
        let result = async {
            // Readable too: `SegmentFetch` maps it while parts arrive.
            let file = Arc::new(std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?);
            let _ = dl.file.set(file.clone());
            let size = self.download(key, &file, dl).await.map_err(|e| anyhow::anyhow!("fetching {key}: {e:#}"))?;
            self.counters.fetched_bytes.fetch_add(size, Relaxed);
            let (me, dl, name, tmp, key_owned) = (self.clone(), dl.clone(), name.to_string(), tmp.clone(), key.to_string());
            tokio::task::spawn_blocking(move || {
                file.sync_all()?;
                // Through the readers' shared map (made now if none holds one), held until the
                // cache entry takes it over: the file is mapped once either way (dropping it
                // here would leave `place` to map the file again).
                let verified = match &me.verify {
                    Some(v) => {
                        let m = SegmentData(dl.map()?);
                        if let Err(e) = v.check(&m) {
                            me.counters.verify_failures.fetch_add(1, Relaxed);
                            anyhow::bail!("{key_owned} is damaged: {e:#}");
                        }
                        me.counters.verified.fetch_add(1, Relaxed);
                        Some(m)
                    }
                    None => None,
                };
                // Readers' map of the partial file becomes the cache entry's (same inode after
                // the rename), and later readers get the entry's: held under the lock so no
                // reader maps the file once more in between.
                let mut map = dl.map.lock();
                let existing = map.upgrade().or_else(|| verified.map(|m| m.0));
                let reserved = dl.reserved.swap(0, Relaxed);
                let (d, evicted) = match me.place(&name, &tmp, size, Some(existing), reserved) {
                    Ok(placed) => placed,
                    Err(e) => {
                        me.release(reserved);
                        return Err(e);
                    }
                };
                let d = d.expect("mapped");
                *map = Arc::downgrade(&d.0);
                drop(map);
                unlink(evicted);
                Ok(d)
            })
            .await?
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
            self.release(dl.reserved.swap(0, Relaxed));
        }
        result
    }

    /// Give back budget a download reserved and won't use.
    fn release(&self, reserved: u64) {
        if reserved > 0 {
            let mut st = self.st.lock();
            st.downloading = st.downloading.saturating_sub(reserved);
        }
    }

    /// Download `key` into `file`: a suffix read tells the size and brings the last part, then
    /// the rest come in parallel, parts readers wait for first.
    async fn download(&self, key: &str, file: &Arc<File>, dl: &Download) -> anyhow::Result<u64> {
        let (tail, size) = {
            let _permit = self.parts.acquire().await?;
            self.objects.get_suffix(key, FETCH_PART).await?.ok_or_else(|| anyhow::anyhow!("object {key} is missing"))?
        };
        anyhow::ensure!(size > 0, "object {key} is empty");
        anyhow::ensure!(tail.len() as u64 == size.min(FETCH_PART), "short read of {key}");
        // Room for the whole file now: its blocks fill in from here on.
        let evicted = {
            let mut st = self.st.lock();
            let evicted = self.make_room(&mut st, size);
            st.downloading += size;
            dl.reserved.store(size, Relaxed);
            evicted
        };
        unlink(evicted);
        {
            // Sparse until the parts land: the whole object can be mapped from now on.
            let f = file.clone();
            let at = size - size.min(FETCH_PART);
            tokio::task::spawn_blocking(move || {
                use std::os::unix::fs::FileExt;
                f.set_len(size)?;
                f.write_all_at(&tail, at)
            })
            .await??;
        }
        let workers = {
            let mut parts = Parts::new(size);
            let last = parts.count() - 1;
            parts.finish(last);
            let left = parts.count() - parts.done_count;
            *dl.parts.lock() = Some(parts);
            dl.tick.send_modify(|t| *t += 1);
            left.min(FETCH_PARTS)
        };
        let worker = || async {
            loop {
                let permit = self.parts.acquire().await?;
                let Some((k, range)) = dl.parts.lock().as_mut().and_then(|p| p.take().map(|k| (k, p.range(k)))) else { return anyhow::Ok(()) };
                let (data, now) = self.objects.get_range(key, range.clone()).await?.ok_or_else(|| anyhow::anyhow!("object {key} is missing"))?;
                anyhow::ensure!(now == size && data.len() as u64 == range.end - range.start, "{key} changed while being read");
                write_at(file, data, range.start).await?;
                drop(permit);
                if let Some(p) = dl.parts.lock().as_mut() {
                    p.finish(k);
                }
                dl.tick.send_modify(|t| *t += 1);
            }
        };
        futures::future::try_join_all((0..workers).map(|_| worker())).await?;
        Ok(size)
    }

    /// Move a complete file into the cache (renaming when possible). With `map`, also map it
    /// and register the map (reusing `Some(existing)`, a map of `src` itself, when the file
    /// could be renamed); without, the file is only cached.
    fn insert_file(&self, name: &str, src: &Path, size: u64, map: Option<Option<Arc<Mapped>>>) -> anyhow::Result<Option<SegmentData>> {
        let (d, evicted) = self.place(name, src, size, map, 0)?;
        unlink(evicted);
        Ok(d)
    }

    /// [`Cache::insert_file`], leaving the evicted files for the caller to [`unlink`]. The file
    /// takes over `reserved` bytes of download reservation (released only once it has the lock,
    /// so an error leaves the reservation to the caller).
    fn place(
        &self,
        name: &str,
        src: &Path,
        size: u64,
        map: Option<Option<Arc<Mapped>>>,
        reserved: u64,
    ) -> anyhow::Result<(Option<SegmentData>, Vec<PathBuf>)> {
        let dst = self.dir.join(name);
        let mut existing = map.as_ref().and_then(|m| m.clone());
        match std::fs::rename(src, &dst) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                let tmp = self.tmp.join(format!("{name}.{:016x}.copy", crate::random_u64()));
                std::fs::copy(src, &tmp)?;
                File::open(&tmp)?.sync_all()?;
                std::fs::rename(&tmp, &dst)?;
                std::fs::remove_file(src)?;
                // A map of `src` is of another inode now.
                existing = None;
            }
            Err(e) => return Err(e.into()),
        }
        let mapped = match (map.is_some(), existing) {
            (false, _) => None,
            (true, Some(m)) => Some(m),
            (true, None) => {
                let file = File::open(&dst)?;
                // SAFETY: see `open_cached`.
                let mmap = unsafe { Mmap::map(&file)? };
                Some(Arc::new(Mapped { mmap, name: name.to_string() }))
            }
        };
        let mut st = self.st.lock();
        st.downloading = st.downloading.saturating_sub(reserved);
        if let Some(old) = st.entries.remove(name) {
            st.used -= old.size;
        }
        let evicted = self.make_room(&mut st, size);
        st.tick += 1;
        let tick = st.tick;
        st.used += size;
        st.entries.insert(name.into(), Entry { size, last_used: tick, map: mapped.as_ref().map_or_else(Weak::new, Arc::downgrade) });
        drop(st);
        Ok((mapped.map(SegmentData), evicted))
    }

    /// Take over a finished, uploaded file (a segment from the staging directory).
    pub(crate) fn adopt(&self, name: &str, src: &Path) -> anyhow::Result<()> {
        let f = File::open(src)?;
        f.sync_all()?;
        let size = f.metadata()?.len();
        drop(f);
        self.insert_file(name, src, size, None).map(|_| ())
    }

    /// Cache bytes that were just uploaded (a live-docs object).
    pub(crate) fn insert_bytes(&self, name: &str, data: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        let tmp = self.tmp.join(format!("{name}.{:016x}.part", crate::random_u64()));
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        self.insert_file(name, &tmp, data.len() as u64, None).map(|_| ())
    }

    /// Drop a file unless it is mapped (its object was deleted). Returns whether it was dropped.
    pub(crate) fn remove(&self, name: &str) -> bool {
        let mut st = self.st.lock();
        let Some(e) = st.entries.get(name) else { return false };
        if e.map.strong_count() > 0 {
            return false;
        }
        let size = e.size;
        st.entries.remove(name);
        st.used -= size;
        let _ = std::fs::remove_file(self.dir.join(name));
        true
    }

    /// Evict least recently used unmapped files until `need` more bytes fit. Evicted files are
    /// renamed aside under the lock (so a fetch of the same name right after can't race the
    /// deletion) and returned for the caller to [`unlink`] once it has released the lock:
    /// freeing a large file's blocks can take a while.
    #[must_use]
    fn make_room(&self, st: &mut State, need: u64) -> Vec<PathBuf> {
        let budget = self.budget.load(Relaxed);
        let mut doomed = Vec::new();
        // Downloads in progress hold their reservations: room is made beside them.
        let need = need + st.downloading;
        if st.used + need <= budget {
            return doomed;
        }
        let target = budget / 64 * LOW_WATER;
        let mut cands: Vec<(u64, Arc<str>, u64)> = st
            .entries
            .iter()
            .filter(|(_, e)| e.map.strong_count() == 0)
            .map(|(n, e)| (e.last_used, n.clone(), e.size))
            .collect();
        cands.sort_unstable_by_key(|c| c.0);
        for (_, name, size) in cands {
            if st.used + need <= target {
                break;
            }
            st.entries.remove(&name);
            st.used -= size;
            let aside = self.tmp.join(format!("{name}.{:016x}.evicted", crate::random_u64()));
            match std::fs::rename(self.dir.join(&*name), &aside) {
                Ok(()) => doomed.push(aside),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, file = &*name, "evicting a cached file failed"),
            }
            self.counters.evictions.fetch_add(1, Relaxed);
            self.counters.evicted_bytes.fetch_add(size, Relaxed);
        }
        if st.used + need > budget {
            self.counters.over_budget.fetch_add(1, Relaxed);
        }
        doomed
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let st = self.st.lock();
        let (mut mapped_files, mut mapped_bytes) = (0, 0);
        for e in st.entries.values() {
            if e.map.strong_count() > 0 {
                mapped_files += 1;
                mapped_bytes += e.size;
            }
        }
        let c = &self.counters;
        CacheStats {
            budget: self.budget.load(Relaxed),
            used: st.used,
            downloading: st.downloading,
            files: st.entries.len() as u64,
            mapped_files,
            mapped_bytes,
            hits: c.hits.load(Relaxed),
            misses: c.misses.load(Relaxed),
            fetched_bytes: c.fetched_bytes.load(Relaxed),
            evictions: c.evictions.load(Relaxed),
            evicted_bytes: c.evicted_bytes.load(Relaxed),
            over_budget: c.over_budget.load(Relaxed),
            verified: c.verified.load(Relaxed),
            verify_failures: c.verify_failures.load(Relaxed),
        }
    }
}

/// Write `data` at `offset` of `file` on the blocking pool.
async fn write_at(file: &Arc<File>, data: Bytes, offset: u64) -> anyhow::Result<()> {
    use std::os::unix::fs::FileExt;
    let f = file.clone();
    tokio::task::spawn_blocking(move || f.write_all_at(&data, offset)).await??;
    Ok(())
}

/// Delete files [`Cache::make_room`] set aside.
fn unlink(paths: Vec<PathBuf>) {
    for p in paths {
        if let Err(e) = std::fs::remove_file(&p) {
            tracing::warn!(error = %e, file = %p.display(), "deleting an evicted cache file failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_align_to_the_end_and_cover_the_object() {
        for size in [1, 100, FETCH_PART - 1, FETCH_PART, FETCH_PART + 1, 3 * FETCH_PART, 3 * FETCH_PART + 7] {
            let p = Parts::new(size);
            assert_eq!(p.count() as u64, size.div_ceil(FETCH_PART), "{size}");
            let last = p.range(p.count() - 1);
            assert_eq!(last, size - size.min(FETCH_PART)..size, "the last part is the suffix read");
            let mut at = 0;
            for k in 0..p.count() {
                let r = p.range(k);
                assert_eq!(r.start, at, "{size} part {k}");
                assert!(r.end > r.start && r.end - r.start <= FETCH_PART);
                assert_eq!(p.part_of(r.start), k);
                assert_eq!(p.part_of(r.end - 1), k);
                at = r.end;
            }
            assert_eq!(at, size);
        }
    }

    #[tokio::test]
    async fn partial_reads_wait_for_their_parts_only() {
        use std::os::unix::fs::FileExt;
        let size = 2 * FETCH_PART + 100;
        let path = crate::test_dir().join(format!("partial-{:016x}", crate::random_u64()));
        let file = Arc::new(std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap());
        file.set_len(size).unwrap();
        let dl = Arc::new(Download::new("partial.seg"));
        *dl.parts.lock() = Some(Parts::new(size));
        let _ = dl.file.set(file.clone());
        let f = SegmentFetch(FetchState::Pending { dl: dl.clone(), size, key: "k".into() });
        // The last part is in (as after the first request).
        let last = dl.parts.lock().as_ref().unwrap().range(2);
        file.write_all_at(&vec![7u8; (last.end - last.start) as usize], last.start).unwrap();
        dl.parts.lock().as_mut().unwrap().finish(2);
        assert_eq!(&f.read(size - 3..size).await.unwrap()[..], &[7, 7, 7]);
        assert_eq!(f.downloaded(), FETCH_PART);
        // A range spanning parts 0 and 1 waits for both, and asks for them first.
        let first = dl.parts.lock().as_ref().unwrap().range(0);
        let want = first.end - 2..first.end + 2;
        assert!(!f.is_ready(want.clone()));
        let reader = tokio::spawn({
            let f = f.clone();
            let want = want.clone();
            async move { f.read(want).await.unwrap() }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(dl.parts.lock().as_mut().unwrap().take(), Some(0), "the reader's parts are next");
        file.write_all_at(&vec![1u8; (first.end - first.start) as usize], 0).unwrap();
        dl.parts.lock().as_mut().unwrap().finish(0);
        dl.tick.send_modify(|t| *t += 1);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!reader.is_finished(), "part 1 isn't in yet");
        file.write_all_at(&vec![2u8; FETCH_PART as usize], first.end).unwrap();
        dl.parts.lock().as_mut().unwrap().finish(1);
        dl.tick.send_modify(|t| *t += 1);
        assert_eq!(&reader.await.unwrap()[..], &[1, 1, 2, 2]);
        // Everything is in: waiting returns at once, and the partial map shows every part,
        // including the ones that landed after it was made.
        f.wait_ready(&[0..1, first.end..first.end + 1, size - 1..size]).await.unwrap();
        assert!(f.wait_ready(std::slice::from_ref(&(0..size + 1))).await.is_err(), "past the end");
        // SAFETY: only ready ranges are read.
        let m = unsafe { f.map_partial() }.unwrap();
        assert_eq!(m.len() as u64, size);
        assert_eq!((m[0], m[first.end as usize], m[size as usize - 1]), (1, 2, 7));
        assert!(Arc::ptr_eq(&m.0, &dl.map().unwrap()), "one map per download");
        // A map made before a part lands sees it once it does.
        let path3 = crate::test_dir().join(format!("partial-{:016x}", crate::random_u64()));
        let file3 = Arc::new(std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path3).unwrap());
        file3.set_len(size).unwrap();
        let dl3 = Arc::new(Download::new("k3.seg"));
        *dl3.parts.lock() = Some(Parts::new(size));
        let _ = dl3.file.set(file3.clone());
        let f3 = SegmentFetch(FetchState::Pending { dl: dl3.clone(), size, key: "k3".into() });
        // SAFETY: the bytes read below are read only once their part is marked done.
        let early = unsafe { f3.map_partial() }.unwrap();
        let waiter = tokio::spawn({
            let f3 = f3.clone();
            async move { f3.wait_ready(std::slice::from_ref(&(0..10))).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        file3.write_all_at(&[9u8; 100], 0).unwrap();
        dl3.parts.lock().as_mut().unwrap().finish(0);
        dl3.tick.send_modify(|t| *t += 1);
        waiter.await.unwrap().unwrap();
        assert_eq!(&early[..10], &[9u8; 10]);
        drop((early, f3));
        let _ = std::fs::remove_file(path3);
        // A failed download fails waiting readers.
        let dl2 = Arc::new(Download::new("k2.seg"));
        *dl2.parts.lock() = Some(Parts::new(size));
        let f2 = SegmentFetch(FetchState::Pending { dl: dl2.clone(), size, key: "k2".into() });
        let reader = tokio::spawn(async move { f2.read(0..10).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dl2.result.send_replace(Some(Err("boom".into())));
        dl2.tick.send_modify(|t| *t += 1);
        let err = reader.await.unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("boom"), "{err:#}");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_verified_download_is_mapped_once() {
        let objects = crate::test_objects("cache-verify-map");
        let dir = crate::test_dir().join(format!("verify-map-{:016x}", crate::random_u64()));
        let seen: Arc<Mutex<Vec<Weak<Mapped>>>> = Arc::default();
        let verify = {
            let seen = seen.clone();
            Verifier::new(move |d: &SegmentData| {
                seen.lock().push(Arc::downgrade(&d.0));
                Ok(())
            })
        };
        let cache = Cache::open(dir.join("seg"), dir.join("tmp"), 1 << 30, objects.clone(), Some(verify)).unwrap();
        for (name, len) in [("a.seg", 1000usize), ("b.seg", 2 * FETCH_PART as usize + 5)] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let key = format!("search/seg/{name}");
            objects.put(&key, Bytes::from(data.clone())).await.unwrap();
            let d = cache.get(name, &key).await.unwrap();
            assert_eq!(&d[..], &data[..]);
            // The map the verifier checked is the one the cache handed out: not dropped and
            // mapped again.
            let checked = seen.lock().pop().unwrap().upgrade().expect("the verified map is still alive");
            assert!(Arc::ptr_eq(&checked, &d.0), "{name}");
        }
        assert_eq!(cache.stats().verified, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wanted_parts_come_first_and_each_part_is_taken_once() {
        let mut p = Parts::new(5 * FETCH_PART + 10);
        assert_eq!(p.count(), 6);
        p.finish(5);
        assert_eq!(p.take(), Some(0));
        // A reader wants bytes in parts 3 and 4.
        p.prioritize(&(p.range(3).start + 5..p.range(4).start + 1));
        p.prioritize(&(p.range(3).start..p.range(3).start + 1));
        assert_eq!(p.take(), Some(3));
        assert_eq!(p.take(), Some(4));
        assert_eq!(p.take(), Some(1));
        // Already taken: prioritizing changes nothing.
        p.prioritize(&p.range(1));
        assert_eq!(p.take(), Some(2));
        assert_eq!(p.take(), None);
        for k in 0..5 {
            assert!(!p.complete(&p.range(k)));
            p.finish(k);
        }
        p.finish(4);
        assert_eq!((p.done_count, p.done_bytes), (6, p.size));
        assert!(p.complete(&(0..p.size)));
    }
}

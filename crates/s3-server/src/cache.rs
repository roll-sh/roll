//! The gateway's hot-object cache: object bytes kept in memory and on local disk, so a hot
//! object is served without an S3 GET.
//!
//! - **Keyed by blob and chunk.** Blobs never change (a write always makes a new blob), so an
//!   entry can never be stale and is never invalidated: a deleted blob's chunks just stop being
//!   asked for and age out. Nothing here depends on a TTL for correctness.
//! - **Aligned chunks** (`chunk_bytes`, 1 MiB by default) hold any range of an object: a range
//!   read over a cached object hits, and a whole-object read fills the chunks ranges use.
//! - **Two tiers, both admitted by frequency** (TinyLFU, `moka`): a new chunk only displaces
//!   one that's asked for less often, so a one-off scan can't flush what's hot. Memory is the
//!   first tier; a chunk reaches disk when it's asked for again after memory dropped it (fetched
//!   twice, or evicted after a hit), so scans don't churn the disk either. Objects over
//!   `max_object_bytes` bypass the cache.
//! - **Single flight.** Concurrent misses of a chunk make one S3 request; a miss fetches its
//!   run of missing chunks (up to `run_chunks`) with one ranged GET.
//! - **Checked disk entries.** Each disk entry carries its CRC32C, verified on every read; a
//!   mismatch drops the entry and the read falls through to S3. Bad bytes are never served.
//! - **What's never cached:** SSE-C objects (the key comes with each request), and SSE-KMS
//!   objects unless the operator allows it (`kms`; S3 would check KMS access on every read).
//!
//! Requests are metered exactly as without the cache (class B per GET): a hit only costs us
//! nothing.

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use s3_store::{ByteStream, Encryption, ObjectMeta, Store, StoreError};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Memory tier size; 0 turns the cache off.
    pub memory_bytes: u64,
    /// Disk tier size; 0: memory only.
    pub disk_bytes: u64,
    /// Where the disk tier lives (needed for one).
    pub dir: Option<PathBuf>,
    /// Chunk size (1 MiB to 8 MiB is sensible).
    pub chunk_bytes: u64,
    /// Larger objects bypass the cache.
    pub max_object_bytes: u64,
    /// Chunks one miss fetches with one GET at most.
    pub run_chunks: u64,
    /// Cache SSE-KMS objects too (`ROLL_S3_CACHE_KMS`).
    pub kms: bool,
}

impl Default for CacheConfig {
    /// Off (rolld turns it on).
    fn default() -> Self {
        Self { memory_bytes: 0, disk_bytes: 0, dir: None, chunk_bytes: 1 << 20, max_object_bytes: 256 << 20, run_chunks: 8, kms: false }
    }
}

/// Counters since start, and what's held.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct CacheStats {
    pub enabled: bool,
    /// Chunks served from memory, from disk, by joining another request's fetch, and fetched.
    pub memory_hits: u64,
    pub disk_hits: u64,
    pub coalesced: u64,
    pub misses: u64,
    /// GETs served wholly from the cache: each saved an S3 GET.
    pub requests: u64,
    pub gets_avoided: u64,
    /// GETs the cache made to S3 (one per run of missing chunks).
    pub s3_gets: u64,
    /// Bytes served from the cache instead of S3.
    pub bytes_saved: u64,
    pub bytes_fetched: u64,
    /// Reads that bypassed the cache (too large, or encrypted).
    pub bypassed: u64,
    /// Disk entries that failed their checksum (dropped; served from S3 instead).
    pub disk_corrupt: u64,
    pub memory_bytes: u64,
    pub memory_capacity: u64,
    pub disk_bytes: u64,
    pub disk_capacity: u64,
    pub chunk_bytes: u64,
}

#[derive(Default)]
struct Counters {
    memory_hits: AtomicU64,
    disk_hits: AtomicU64,
    coalesced: AtomicU64,
    misses: AtomicU64,
    requests: AtomicU64,
    gets_avoided: AtomicU64,
    s3_gets: AtomicU64,
    bytes_saved: AtomicU64,
    bytes_fetched: AtomicU64,
    bypassed: AtomicU64,
    disk_corrupt: AtomicU64,
}

fn add(a: &AtomicU64, n: u64) {
    a.fetch_add(n, Ordering::Relaxed);
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ChunkKey {
    blob: Arc<str>,
    index: u64,
}

/// A chunk in memory, and whether it was ever served from there (then it's worth keeping on
/// disk once memory lets it go).
struct MemEntry {
    bytes: Bytes,
    hit: AtomicBool,
}

/// A fetch in flight: `None` until the chunk (or the fetch's error) is in.
type Slot = Option<Result<Bytes, Arc<str>>>;

pub struct BlobCache {
    cfg: CacheConfig,
    memory: moka::sync::Cache<ChunkKey, Arc<MemEntry>>,
    disk: Option<Arc<Disk>>,
    inflight: Mutex<HashMap<ChunkKey, watch::Receiver<Slot>>>,
    stats: Counters,
}

impl BlobCache {
    /// The cache `cfg` describes (`None`: off).
    pub fn new(cfg: CacheConfig) -> std::io::Result<Option<Arc<Self>>> {
        if cfg.memory_bytes == 0 || cfg.chunk_bytes == 0 {
            return Ok(None);
        }
        let disk = match (&cfg.dir, cfg.disk_bytes) {
            (Some(dir), n) if n > 0 => Some(Disk::open(dir, n)?),
            _ => None,
        };
        let spill = disk.clone();
        let memory = moka::sync::Cache::builder()
            .max_capacity(cfg.memory_bytes)
            .weigher(|_, e: &Arc<MemEntry>| e.bytes.len().try_into().unwrap_or(u32::MAX))
            .eviction_listener(move |k: Arc<ChunkKey>, e: Arc<MemEntry>, cause| {
                // Memory let go of a chunk that was served from it: keep it on disk.
                if cause == moka::notification::RemovalCause::Size
                    && e.hit.load(Ordering::Relaxed)
                    && let Some(d) = &spill
                {
                    d.store((*k).clone(), e.bytes.clone());
                }
            })
            .build();
        Ok(Some(Arc::new(Self { cfg, memory, disk, inflight: Mutex::new(HashMap::new()), stats: Counters::default() })))
    }

    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    /// Empty the memory tier (the disk tier stays).
    pub fn clear_memory(&self) {
        self.memory.invalidate_all();
        self.memory.run_pending_tasks();
    }

    /// Whether reads of this object go through the cache.
    pub fn eligible(&self, meta: &ObjectMeta, enc: Option<&Encryption>) -> bool {
        let ok = meta.size > 0
            && meta.size <= self.cfg.max_object_bytes
            && match enc {
                None => true,
                Some(Encryption::Kms(_)) => self.cfg.kms,
                Some(Encryption::Customer { .. }) => false,
            };
        if !ok {
            add(&self.stats.bypassed, 1);
        }
        ok
    }

    pub fn stats(&self) -> CacheStats {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let s = &self.stats;
        self.memory.run_pending_tasks();
        let (disk_bytes, disk_capacity) = match &self.disk {
            Some(d) => {
                d.index.run_pending_tasks();
                (d.index.weighted_size(), d.capacity)
            }
            None => (0, 0),
        };
        CacheStats {
            enabled: true,
            memory_hits: l(&s.memory_hits),
            disk_hits: l(&s.disk_hits),
            coalesced: l(&s.coalesced),
            misses: l(&s.misses),
            requests: l(&s.requests),
            gets_avoided: l(&s.gets_avoided),
            s3_gets: l(&s.s3_gets),
            bytes_saved: l(&s.bytes_saved),
            bytes_fetched: l(&s.bytes_fetched),
            bypassed: l(&s.bypassed),
            disk_corrupt: l(&s.disk_corrupt),
            memory_bytes: self.memory.weighted_size(),
            memory_capacity: self.cfg.memory_bytes,
            disk_bytes,
            disk_capacity,
            chunk_bytes: self.cfg.chunk_bytes,
        }
    }

    /// Bytes `range` of an object (eligible: [`Self::eligible`]). The first chunk is read
    /// before this returns, so a missing blob fails the request rather than its body.
    pub async fn open(self: &Arc<Self>, store: &Arc<Store>, meta: &ObjectMeta, range: Range<u64>) -> Result<ByteStream, StoreError> {
        let cs = self.cfg.chunk_bytes;
        let end = range.end.min(meta.size);
        let start = range.start.min(end);
        add(&self.stats.requests, 1);
        if start == end {
            return Ok(futures::stream::empty().boxed());
        }
        let mut r = Reader { cache: self.clone(), store: store.clone(), meta: Arc::new(meta.clone()), blob: meta.blob_id.as_str().into(), next: start / cs, last: (end - 1) / cs, start, end, pending: HashMap::new(), gets: 0 };
        let first = r.next_slice().await.transpose().map_err(|e| StoreError::Internal(anyhow::anyhow!("{e}")))?;
        let head = futures::stream::iter(first.map(Ok));
        let rest = futures::stream::unfold(r, |mut r| async move { r.next_slice().await.map(|x| (x, r)) });
        Ok(head.chain(rest).boxed())
    }

    fn key(&self, blob: &Arc<str>, index: u64) -> ChunkKey {
        ChunkKey { blob: blob.clone(), index }
    }

    /// Keep a fetched chunk: memory, and disk on its second fetch.
    fn keep(&self, k: ChunkKey, bytes: Bytes) {
        if let Some(d) = &self.disk
            && !d.index.contains_key(&k)
        {
            if d.seen.contains_key(&k) {
                d.store(k.clone(), bytes.clone());
            } else {
                d.seen.insert(k.clone(), ());
            }
        }
        self.memory.insert(k, Arc::new(MemEntry { bytes, hit: AtomicBool::new(false) }));
    }

    /// Fetch chunks `first..=last` of a blob with one GET, publishing each as it's in.
    async fn fetch_run(self: Arc<Self>, store: Arc<Store>, meta: Arc<ObjectMeta>, blob: Arc<str>, first: u64, last: u64, slots: Vec<watch::Sender<Slot>>) {
        let cs = self.cfg.chunk_bytes;
        add(&self.stats.s3_gets, 1);
        let mut slots = slots.into_iter();
        let mut body = match store.open_blob_sse(&meta, Some(first * cs..((last + 1) * cs).min(meta.size)), None).await {
            Ok((_, s)) => s,
            Err(e) => return self.fail(&blob, first, slots, e.to_string().into()),
        };
        let mut buf = BytesMut::new();
        for index in first..=last {
            let want = (((index + 1) * cs).min(meta.size) - index * cs) as usize;
            while buf.len() < want {
                match body.next().await {
                    Some(Ok(b)) => buf.extend_from_slice(&b),
                    Some(Err(e)) => return self.fail(&blob, index, slots, e.to_string().into()),
                    None => return self.fail(&blob, index, slots, "object storage sent fewer bytes than the object has".into()),
                }
            }
            let chunk = buf.split_to(want).freeze();
            add(&self.stats.bytes_fetched, chunk.len() as u64);
            let k = self.key(&blob, index);
            self.keep(k.clone(), chunk.clone());
            if let Some(tx) = slots.next() {
                let _ = tx.send(Some(Ok(chunk)));
            }
            self.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&k);
        }
    }

    /// A fetch failed: its chunks from `index` on fail their readers.
    fn fail(&self, blob: &Arc<str>, index: u64, slots: impl Iterator<Item = watch::Sender<Slot>>, why: Arc<str>) {
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        for (i, tx) in (index..).zip(slots) {
            let _ = tx.send(Some(Err(why.clone())));
            map.remove(&self.key(blob, i));
        }
    }
}

/// One read through the cache.
struct Reader {
    cache: Arc<BlobCache>,
    store: Arc<Store>,
    meta: Arc<ObjectMeta>,
    blob: Arc<str>,
    next: u64,
    last: u64,
    start: u64,
    end: u64,
    /// Chunks of runs this read started, still to come.
    pending: HashMap<u64, watch::Receiver<Slot>>,
    /// GETs this read made.
    gets: u32,
}

async fn wait(mut rx: watch::Receiver<Slot>) -> Option<Result<Bytes, Arc<str>>> {
    match rx.wait_for(Option::is_some).await {
        Ok(v) => v.clone(),
        Err(_) => None,
    }
}

impl Reader {
    /// The next chunk's part of the range (`None`: done).
    async fn next_slice(&mut self) -> Option<std::io::Result<Bytes>> {
        if self.next > self.last {
            return None;
        }
        let i = self.next;
        let chunk = match self.chunk(i).await {
            Ok(c) => c,
            Err(e) => {
                self.next = self.last + 1;
                return Some(Err(std::io::Error::other(e.to_string())));
            }
        };
        self.next += 1;
        // The whole range came from the cache: an S3 GET saved (counted as the last chunk goes
        // out; a client may never ask for the end of a stream whose length it knows).
        if self.next > self.last && self.gets == 0 {
            add(&self.cache.stats.gets_avoided, 1);
        }
        let cs = self.cache.cfg.chunk_bytes;
        let base = i * cs;
        let a = self.start.max(base) - base;
        let b = (self.end.min(base + chunk.len() as u64) - base).max(a);
        Some(Ok(chunk.slice(a as usize..b as usize)))
    }

    async fn chunk(&mut self, i: u64) -> Result<Bytes, Arc<str>> {
        let c = self.cache.clone();
        let s = &c.stats;
        let k = c.key(&self.blob, i);
        if let Some(rx) = self.pending.remove(&i)
            && let Some(r) = wait(rx).await
        {
            return r;
        }
        for _ in 0..3 {
            if let Some(e) = c.memory.get(&k) {
                e.hit.store(true, Ordering::Relaxed);
                add(&s.memory_hits, 1);
                add(&s.bytes_saved, e.bytes.len() as u64);
                return Ok(e.bytes.clone());
            }
            let joined = c.inflight.lock().unwrap_or_else(|e| e.into_inner()).get(&k).cloned();
            if let Some(rx) = joined {
                if let Some(r) = wait(rx).await {
                    if r.is_ok() {
                        add(&s.coalesced, 1);
                    }
                    return r;
                }
                continue;
            }
            if let Some(d) = &c.disk
                && d.index.contains_key(&k)
            {
                match d.read(&k).await {
                    Some(bytes) => {
                        add(&s.disk_hits, 1);
                        add(&s.bytes_saved, bytes.len() as u64);
                        c.memory.insert(k.clone(), Arc::new(MemEntry { bytes: bytes.clone(), hit: AtomicBool::new(true) }));
                        return Ok(bytes);
                    }
                    None => add(&s.disk_corrupt, 1),
                }
            }
            if let Some(rx) = self.start_run(i)
                && let Some(r) = wait(rx).await
            {
                return r;
            }
        }
        Err("the cache couldn't read the object".into())
    }

    /// Register and start a fetch of chunk `i` and the missing ones after it (up to the end of
    /// this read and `run_chunks`); returns chunk `i`'s slot (`None`: someone else started it).
    fn start_run(&mut self, i: u64) -> Option<watch::Receiver<Slot>> {
        let c = self.cache.clone();
        let mut slots = Vec::new();
        let mut rxs = Vec::new();
        {
            let mut map = c.inflight.lock().unwrap_or_else(|e| e.into_inner());
            let mut j = i;
            while j <= self.last && j - i < c.cfg.run_chunks.max(1) {
                let k = c.key(&self.blob, j);
                let busy = map.contains_key(&k) || (j > i && (c.memory.contains_key(&k) || c.disk.as_ref().is_some_and(|d| d.index.contains_key(&k))));
                if busy {
                    if j == i {
                        return map.get(&k).cloned();
                    }
                    break;
                }
                let (tx, rx) = watch::channel(None);
                map.insert(k, rx.clone());
                slots.push(tx);
                rxs.push(rx);
                j += 1;
            }
        }
        let n = slots.len() as u64;
        add(&c.stats.misses, n);
        self.gets += 1;
        tokio::spawn(c.clone().fetch_run(self.store.clone(), self.meta.clone(), self.blob.clone(), i, i + n - 1, slots));
        let mut rxs = rxs.into_iter();
        let first = rxs.next();
        for (j, rx) in (i + 1..).zip(rxs) {
            self.pending.insert(j, rx);
        }
        first
    }
}

// ---- The disk tier ----

const MAGIC: &[u8; 4] = b"RLC1";
const HEADER: usize = 16;

/// Chunk files under the cache directory (`<dir>/<2 chars>/<blob>-<chunk>-<generation>`):
/// a 16-byte header (magic, CRC32C, length) and the bytes. The index (TinyLFU, weighted by
/// size) decides what stays; evicting an entry deletes its file.
struct Disk {
    dir: PathBuf,
    capacity: u64,
    /// Chunk → (generation, length).
    index: moka::sync::Cache<ChunkKey, (u64, u32)>,
    /// Chunks fetched once recently (the second fetch admits one to disk).
    seen: moka::sync::Cache<ChunkKey, ()>,
    generation: AtomicU64,
    writes: std::sync::mpsc::SyncSender<(ChunkKey, Bytes)>,
}

fn chunk_path(dir: &Path, k: &ChunkKey, generation: u64) -> PathBuf {
    let shard: String = k.blob.chars().take(2).collect();
    dir.join(shard).join(format!("{}-{}-{generation}", k.blob, k.index))
}

impl Disk {
    fn open(dir: &Path, capacity: u64) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let files = dir.to_path_buf();
        let index = moka::sync::Cache::builder()
            .max_capacity(capacity)
            .weigher(|_, v: &(u64, u32)| v.1.saturating_add(HEADER as u32))
            .eviction_listener(move |k: Arc<ChunkKey>, v: (u64, u32), _| {
                let _ = std::fs::remove_file(chunk_path(&files, &k, v.0));
            })
            .build();
        let seen = moka::sync::Cache::builder().max_capacity(200_000).time_to_idle(std::time::Duration::from_secs(3600)).build();
        let (writes, rx) = std::sync::mpsc::sync_channel::<(ChunkKey, Bytes)>(64);
        let d = Arc::new(Self { dir: dir.to_path_buf(), capacity, index, seen, generation: AtomicU64::new(1), writes });
        let weak = Arc::downgrade(&d);
        std::thread::Builder::new().name("roll-s3-cache".into()).spawn(move || {
            // What a previous run left is still good (blobs never change): index it first.
            if let Some(d) = weak.upgrade() {
                d.load();
            }
            while let Ok((k, bytes)) = rx.recv() {
                let Some(d) = weak.upgrade() else { return };
                d.write(k, &bytes);
            }
        })?;
        Ok(d)
    }

    /// Index the chunk files on disk; anything unreadable goes.
    fn load(&self) {
        let mut max_gen = 0;
        for shard in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let parsed = (|| {
                    let mut it = name.rsplitn(3, '-');
                    let generation: u64 = it.next()?.parse().ok()?;
                    let index: u64 = it.next()?.parse().ok()?;
                    let blob = it.next()?;
                    let len = f.metadata().ok()?.len().checked_sub(HEADER as u64)?;
                    Some((ChunkKey { blob: blob.into(), index }, generation, u32::try_from(len).ok()?))
                })();
                match parsed {
                    Some((k, generation, len)) => {
                        max_gen = max_gen.max(generation);
                        self.index.insert(k, (generation, len));
                    }
                    None => {
                        let _ = std::fs::remove_file(f.path());
                    }
                }
            }
        }
        self.generation.fetch_max(max_gen + 1, Ordering::Relaxed);
    }

    /// Queue a chunk to be written (dropped if the writer is behind).
    fn store(&self, k: ChunkKey, bytes: Bytes) {
        let _ = self.writes.try_send((k, bytes));
    }

    fn write(&self, k: ChunkKey, bytes: &Bytes) {
        if self.index.contains_key(&k) {
            return;
        }
        let Ok(len) = u32::try_from(bytes.len()) else { return };
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let path = chunk_path(&self.dir, &k, generation);
        let mut out = Vec::with_capacity(HEADER + bytes.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&crc32c::crc32c(bytes).to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(bytes);
        let tmp = path.with_extension("tmp");
        let ok = path.parent().is_some_and(|p| std::fs::create_dir_all(p).is_ok()) && std::fs::write(&tmp, &out).is_ok() && std::fs::rename(&tmp, &path).is_ok();
        if ok {
            self.index.insert(k, (generation, len));
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// A chunk from disk, checked; a bad one is dropped (`None`).
    async fn read(&self, k: &ChunkKey) -> Option<Bytes> {
        let (generation, len) = self.index.get(k)?;
        let path = chunk_path(&self.dir, k, generation);
        let data = tokio::task::spawn_blocking(move || std::fs::read(path)).await.ok().and_then(Result::ok);
        let good = data.filter(|d| {
            d.len() == HEADER + len as usize
                && &d[..4] == MAGIC
                && u32::from_le_bytes(d[8..12].try_into().unwrap_or_default()) == len
                && u32::from_le_bytes(d[4..8].try_into().unwrap_or_default()) == crc32c::crc32c(&d[HEADER..])
        });
        match good {
            Some(d) => Some(Bytes::from(d).slice(HEADER..)),
            None => {
                tracing::warn!(blob = &*k.blob, chunk = k.index, "a cached chunk failed its checksum; dropped");
                self.index.invalidate(k);
                None
            }
        }
    }
}

/// Parse a size: `256MiB`, `10GiB`, `512MB`, `1048576` (bytes); `0` or `off` is 0.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("off") {
        return Ok(0);
    }
    let split = t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("{s:?} isn't a size (e.g. 256MiB, 10GiB)"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "kib" => 1024.0,
        "m" | "mb" => 1e6,
        "mib" => 1024.0 * 1024.0,
        "g" | "gb" => 1e9,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1e12,
        "tib" => 1024f64.powi(4),
        u => return Err(format!("unknown size unit {u:?} in {s:?}")),
    };
    Ok((n * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("256MiB").unwrap(), 256 << 20);
        assert_eq!(parse_size("10GiB").unwrap(), 10 << 30);
        assert_eq!(parse_size("1.5GB").unwrap(), 1_500_000_000);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("off").unwrap(), 0);
        assert!(parse_size("lots").is_err());
        assert!(parse_size("5 parsecs").is_err());
    }
}

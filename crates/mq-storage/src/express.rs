//! Logs stored directly in S3 Express One Zone (a directory bucket): the "no keepers"
//! deployment, where single-digit-millisecond object writes replace the keeper quorum.
//!
//! Layout under `{prefix}/{log}/` (`{log}` encoded as in [`crate::tier`]):
//!
//! - `{index:020}`: one object per append batch, written with put-if-absent
//!   (`PutMode::Create`). Two writers can never both write the same index, which is what fences
//!   a deposed owner: its next put collides with an object the new owner already wrote.
//! - `seg/{first:020}-{end:020}`: batches merged by background compaction (segment format of
//!   [`crate::tier`]).
//! - `epoch`: JSON `{epoch, first}` marker: the last owner's epoch and the truncation point.
//!
//! On open the new owner finds the tail `T` and claims it with a *fence marker*, an empty batch
//! at index `T` (it occupies one index, so offsets skip one value per ownership change). The old
//! owner's next put lands on `T` and fails, so it learns it is fenced before acknowledging
//! anything. Fence markers are never deleted (they are 24 bytes), so a paused old owner can't
//! slip a write into an index after compaction.
//!
//! With [`ExpressConfig::group`], appends of every log go to one shared stream first and reach
//! the log's own directory later in large batches (see [`crate::group`]); the log's directory
//! then also holds *links* (a marker plus the stream they point to) and, after a clean close, a
//! closing marker. Opening a log whose last link isn't followed by a closing marker drains that
//! stream first.

use crate::group::{self, Group, GroupConfig, GroupErr, Record, Requests};
use crate::quorum::FencedError;
use crate::stats::{Metrics, StorageStats};
use crate::tier::{self, Cache, SegmentData};
use anyhow::{Context, bail};
use async_trait::async_trait;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_util::TryStreamExt;
use mq_core::log::{Log, LogStore, crc32};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

const BATCH_MAGIC: u32 = 0x5251_4231; // "RQB1"
const HEADER_LEN: usize = 24;
/// A link: a marker followed by `[stream u64][from seq u64]`.
const LINK_LEN: usize = HEADER_LEN + 16;

/// Configuration of an [`S3ExpressStore`].
#[derive(Clone, Debug)]
pub struct ExpressConfig {
    /// A directory bucket ([`S3ExpressStore::directory_bucket`]); `InMemory` or
    /// `LocalFileSystem` in tests (both support put-if-absent).
    pub store: Arc<dyn ObjectStore>,
    pub prefix: String,
    /// Target size of compacted segments.
    pub segment_bytes: u64,
    /// Batches younger than this are not compacted.
    pub compact_after: Duration,
    /// Compact once this many old batches are waiting (or `segment_bytes` of them).
    pub compact_min_batches: usize,
    /// Bytes of decoded objects cached for reads.
    pub cache_bytes: u64,
    /// Recently appended bytes per log kept in memory for tail reads.
    pub tail_bytes: usize,
    /// Group appends of all logs into one stream per store (`None`: one object per append).
    pub group: Option<GroupConfig>,
}

impl ExpressConfig {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        ExpressConfig {
            store,
            prefix: prefix.into(),
            segment_bytes: 8 << 20,
            compact_after: Duration::from_secs(60),
            compact_min_batches: 64,
            cache_bytes: 128 << 20,
            tail_bytes: 4 << 20,
            group: None,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Marker {
    epoch: u64,
    first: u64,
}

struct Ctx {
    cfg: ExpressConfig,
    cache: Cache,
    metrics: Metrics,
    requests: Arc<Requests>,
    group: Option<Arc<Group>>,
    /// `{prefix}/_streams`.
    streams: Path,
}

/// [`LogStore`] on S3 Express One Zone with fencing by index.
pub struct S3ExpressStore {
    ctx: Arc<Ctx>,
    open: Mutex<HashMap<String, Weak<ExpressLog>>>,
    /// Logs of this store, including closed ones still materializing what they staged.
    shared: Mutex<HashMap<String, Weak<Shared>>>,
}

impl S3ExpressStore {
    pub fn new(cfg: ExpressConfig) -> Arc<Self> {
        let cache = Cache::new(cfg.cache_bytes);
        let streams = streams_root(&cfg.prefix);
        let requests = Arc::new(Requests::default());
        let group = cfg.group.clone().map(|g| Group::new(g, cfg.store.clone(), streams.clone(), requests.clone()));
        let ctx = Arc::new(Ctx { cfg, cache, metrics: Metrics::default(), requests, group, streams });
        Arc::new(S3ExpressStore { ctx, open: Mutex::new(HashMap::new()), shared: Mutex::new(HashMap::new()) })
    }

    /// Requests made so far, by kind.
    pub fn requests(&self) -> &Requests {
        &self.ctx.requests
    }

    /// An S3 Express One Zone directory bucket (`name--usw2-az1--x-s3`), with credentials from
    /// the environment. Conditional puts (`If-None-Match`) are on by default.
    pub fn directory_bucket(bucket: &str, region: &str) -> anyhow::Result<Arc<dyn ObjectStore>> {
        let s3 = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket).with_region(region).with_s3_express(true).build()?;
        Ok(Arc::new(s3))
    }

    pub async fn open_log(&self, key: &str, epoch: u64) -> anyhow::Result<Arc<ExpressLog>> {
        {
            let mut open = self.open.lock();
            open.retain(|_, w| w.strong_count() > 0);
            if let Some(l) = open.get(key).and_then(Weak::upgrade)
                && (epoch == 0 || epoch == l.lease)
                && l.usable()
            {
                return Ok(l);
            }
        }
        // A closed handle of this log may still be materializing what it staged: let it finish,
        // so what it wrote is what we find.
        let closing = self.shared.lock().get(key).and_then(Weak::upgrade);
        if let Some(old) = closing {
            let _ = tokio::time::timeout(Duration::from_secs(30), old.released()).await;
        }
        let sh = open_shared(self.ctx.clone(), key, epoch).await?;
        self.shared.lock().insert(key.to_string(), Arc::downgrade(&sh));
        tokio::spawn(compactor(sh.clone()));
        let log = Arc::new(ExpressLog { sh, lease: epoch });
        self.open.lock().insert(key.to_string(), Arc::downgrade(&log));
        Ok(log)
    }

    pub fn stats(&self) -> StorageStats {
        let mut s = StorageStats { backend: self.describe(), ..Default::default() };
        for l in self.open.lock().values().filter_map(Weak::upgrade) {
            s.logs += 1;
            s.bytes_hot += l.sh.st.lock().bytes;
        }
        s.cache_bytes = self.ctx.cache.bytes();
        self.ctx.metrics.fill(&mut s);
        s
    }
}

#[async_trait]
impl LogStore for S3ExpressStore {
    async fn open(&self, key: &str, epoch: u64) -> anyhow::Result<Arc<dyn Log>> {
        Ok(self.open_log(key, epoch).await?)
    }

    fn describe(&self) -> String {
        format!("S3 Express One Zone ({}/{})", self.ctx.cfg.store, self.ctx.cfg.prefix)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Batch,
    Marker,
    Seg,
}

#[derive(Clone)]
struct Obj {
    end: u64,
    size: u64,
    kind: Kind,
    path: Path,
    /// When we wrote it (`None`: found on open, so old enough to compact).
    born: Option<Instant>,
}

struct St {
    first: u64,
    next: u64,
    objs: BTreeMap<u64, Obj>,
    tail: VecDeque<(u64, Bytes)>,
    tail_bytes: usize,
    /// A batch whose put failed ambiguously; retried (unchanged) before anything else.
    uncertain: Option<(u64, Bytes)>,
    fenced: Option<u64>,
    destroyed: bool,
    bytes: u64,
    /// Grouped appends: entries from here to `next` are only in the stream (and in `tail`).
    staged_from: u64,
    staged_bytes: usize,
    /// Stream objects holding staged entries, with the index each one's record ends at.
    staged_objs: Vec<(u64, u64, u64)>,
    staged_since: Option<Instant>,
    /// The stream this log has a link to, if any.
    linked: Option<u64>,
    /// `first` moved without the epoch marker saying so yet (written at the next flush).
    first_dirty: bool,
}

struct Shared {
    key: String,
    dir: Path,
    epoch: u64,
    writer: u64,
    ctx: Arc<Ctx>,
    st: Mutex<St>,
    write: tokio::sync::Mutex<()>,
    maint: tokio::sync::Mutex<()>,
    wake: Notify,
    closed: AtomicBool,
    /// Set once a closed log has materialized what it staged (or given up).
    done: tokio::sync::watch::Sender<bool>,
}

/// One log on S3 Express. Dropping the last handle stops compaction.
pub struct ExpressLog {
    sh: Arc<Shared>,
    lease: u64,
}

impl ExpressLog {
    pub fn epoch(&self) -> u64 {
        self.sh.epoch
    }

    fn usable(&self) -> bool {
        let st = self.sh.st.lock();
        st.fenced.is_none() && !st.destroyed
    }

    /// Run one compaction pass now (normally done in the background).
    pub async fn compact(&self) -> anyhow::Result<bool> {
        self.sh.compact_once(true).await
    }

    /// Number of objects backing the log (batches, markers and segments).
    pub fn objects(&self) -> usize {
        self.sh.st.lock().objs.len()
    }
}

impl Drop for ExpressLog {
    fn drop(&mut self) {
        self.sh.closed.store(true, Ordering::Release);
        self.sh.wake.notify_one();
    }
}

fn encode_batch(epoch: u64, writer: u64, entries: &[Bytes]) -> Bytes {
    let mut b = BytesMut::with_capacity(HEADER_LEN + entries.iter().map(|e| 8 + e.len()).sum::<usize>());
    b.put_u32(BATCH_MAGIC);
    b.put_u64(epoch);
    b.put_u64(writer);
    b.put_u32(entries.len() as u32);
    for e in entries {
        b.put_u32(e.len() as u32);
        b.put_u32(crc32(e));
        b.put_slice(e);
    }
    b.freeze()
}

/// `(epoch, writer, count)`.
fn decode_header(mut h: &[u8]) -> anyhow::Result<(u64, u64, u32)> {
    if h.len() < HEADER_LEN || h.get_u32() != BATCH_MAGIC {
        bail!("not a batch object");
    }
    Ok((h.get_u64(), h.get_u64(), h.get_u32()))
}

fn decode_batch(index: u64, data: Bytes) -> anyhow::Result<SegmentData> {
    let (_, _, count) = decode_header(&data)?;
    let size = data.len() as u64;
    let mut pos = HEADER_LEN;
    let mut entries = Vec::with_capacity(count as usize);
    for i in 0..count as u64 {
        if pos + 8 > data.len() {
            bail!("truncated batch object");
        }
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let crc = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
        if pos + 8 + len > data.len() {
            bail!("truncated batch object");
        }
        let e = data.slice(pos + 8..pos + 8 + len);
        if crc32(&e) != crc {
            bail!("batch entry {} failed its checksum", index + i);
        }
        entries.push((index + i, e));
        pos += 8 + len;
    }
    Ok(SegmentData { end: index + (count as u64).max(1), entries, size })
}

/// Drop the oldest tail entries past `max` bytes, never ones only the stream holds.
fn trim_tail(st: &mut St, max: usize) {
    while st.tail_bytes > max && st.tail.len() > 1 && st.tail.front().is_some_and(|(i, _)| *i < st.staged_from) {
        if let Some((_, e)) = st.tail.pop_front() {
            st.tail_bytes -= e.len();
        }
    }
}

fn encode_link(epoch: u64, writer: u64, stream: u64, from: u64) -> Bytes {
    let mut b = BytesMut::from(&encode_batch(epoch, writer, &[])[..]);
    b.put_u64(stream);
    b.put_u64(from);
    b.freeze()
}

fn streams_root(prefix: &str) -> Path {
    let p = prefix.trim_matches('/');
    Path::from(if p.is_empty() { "_streams".to_string() } else { format!("{p}/_streams") })
}

fn is_already_exists(e: &object_store::Error) -> bool {
    matches!(e, object_store::Error::AlreadyExists { .. })
}

async fn read_header(store: &dyn ObjectStore, path: &Path) -> anyhow::Result<(u64, u64, u32)> {
    let h = store.get_range(path, 0..HEADER_LEN as u64).await.with_context(|| format!("read {path}"))?;
    decode_header(&h)
}

/// What a listing of the log directory contains.
#[derive(Default)]
struct Listing {
    batches: BTreeMap<u64, (Path, u64)>,
    segs: BTreeMap<u64, (u64, Path, u64)>,
}

async fn list(store: &dyn ObjectStore, dir: &Path) -> anyhow::Result<Listing> {
    let depth = dir.parts().count();
    let metas: Vec<_> = store.list(Some(dir)).try_collect().await.with_context(|| format!("list {dir}"))?;
    let mut l = Listing::default();
    for m in metas {
        let parts: Vec<String> = m.location.parts().skip(depth).map(|p| p.as_ref().to_string()).collect();
        match parts.as_slice() {
            [name] if name.len() == 20 => {
                if let Ok(i) = name.parse() {
                    l.batches.insert(i, (m.location, m.size));
                }
            }
            [seg, name] if seg == "seg" => {
                if let Some((a, b)) = name.split_once('-')
                    && let (Ok(a), Ok(b)) = (a.parse(), b.parse())
                {
                    l.segs.insert(a, (b, m.location, m.size));
                }
            }
            _ => {}
        }
    }
    Ok(l)
}

async fn open_shared(ctx: Arc<Ctx>, key: &str, lease: u64) -> anyhow::Result<Arc<Shared>> {
    let store = ctx.cfg.store.clone();
    let dir = tier::log_dir(&ctx.cfg.prefix, key)?;
    let marker_path = dir.clone().join("epoch");
    let marker: Marker = match store.get(&marker_path).await {
        Ok(r) => serde_json::from_slice(&r.bytes().await?).context("bad epoch marker")?,
        Err(object_store::Error::NotFound { .. }) => Marker::default(),
        Err(e) => return Err(e).context("read epoch marker"),
    };
    ctx.requests.list();
    let mut listing = list(store.as_ref(), &dir).await?;
    // Entries may be waiting in a stream the last writer linked: drain it first (twice at most:
    // a drained stream stays drained).
    for _ in 0..2 {
        let Some(stream) = undrained_link(&ctx, &listing).await? else { break };
        drain_stream(&ctx, stream).await.with_context(|| format!("drain stream {stream:016x} for {key}"))?;
        ctx.requests.list();
        listing = list(store.as_ref(), &dir).await?;
    }
    let mut tail = listing.segs.values().map(|(b, _, _)| *b).max().unwrap_or(0).max(marker.first);
    let mut last_epoch = marker.epoch;
    if let Some((&i, (path, _))) = listing.batches.last_key_value() {
        let (e, _, count) = read_header(store.as_ref(), path).await?;
        tail = tail.max(i + (count as u64).max(1));
        last_epoch = last_epoch.max(e);
    }
    if lease >= 1 << 47 {
        bail!("lease epoch {lease} is too large");
    }
    let mut epoch = (last_epoch + 1).max(lease << 16);
    let writer = mq_core::rand_u64();
    // Claim the tail with a fence marker; anyone who wrote there first pushes us further.
    let mut claimed = false;
    for _ in 0..10_000 {
        if lease > 0 && last_epoch >> 16 > lease {
            return Err(FencedError { log: key.to_string(), epoch: lease << 16, current: last_epoch }.into());
        }
        let path = dir.clone().join(format!("{tail:020}"));
        let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
        match store.put_opts(&path, encode_batch(epoch, writer, &[]).into(), opts).await {
            Ok(_) => {
                claimed = true;
                break;
            }
            Err(e) if is_already_exists(&e) => {
                let (e, _, count) = read_header(store.as_ref(), &path).await?;
                last_epoch = last_epoch.max(e);
                epoch = epoch.max(e + 1);
                tail += (count as u64).max(1);
            }
            Err(e) => return Err(e).context("write fence marker"),
        }
    }
    if !claimed {
        bail!("could not claim the tail of {key}");
    }
    let marker = Marker { epoch, first: marker.first };
    store.put(&marker_path, serde_json::to_vec(&marker)?.into()).await.context("write epoch marker")?;
    // Re-list: everything up to our marker is now final.
    let listing = list(store.as_ref(), &dir).await?;
    let mut objs: BTreeMap<u64, Obj> = BTreeMap::new();
    let mut stale = Vec::new();
    for (&a, (b, path, size)) in &listing.segs {
        if *b <= marker.first {
            stale.push(path.clone());
        } else {
            objs.insert(a, Obj { end: *b, size: *size, kind: Kind::Seg, path: path.clone(), born: None });
        }
    }
    // Grouped logs truncate between entries of one batch: the batch holding `first` stays.
    let straddling = listing.batches.range(..marker.first).next_back().filter(|(_, (_, size))| *size as usize != HEADER_LEN).map(|(&i, _)| i).filter(|&i| {
        let next = listing.batches.range(i + 1..).next().map(|(&k, _)| k).unwrap_or(u64::MAX);
        let seg = listing.segs.range(i..).next().map(|(&a, _)| a).unwrap_or(u64::MAX);
        next.min(seg) > marker.first
    });
    for (&i, (path, size)) in &listing.batches {
        let marker_obj = *size as usize == HEADER_LEN;
        let covered = objs.range(..=i).next_back().is_some_and(|(_, o)| o.kind == Kind::Seg && i < o.end);
        if covered || (i < marker.first && Some(i) != straddling) {
            // Leftovers of compaction or truncation (fence markers stay forever).
            if !marker_obj {
                stale.push(path.clone());
            }
            continue;
        }
        let kind = if marker_obj { Kind::Marker } else { Kind::Batch };
        objs.insert(i, Obj { end: 0, size: *size, kind, path: path.clone(), born: None });
    }
    // Batch ends: the next object's start; the last one is our marker at the tail.
    let keys: Vec<u64> = objs.keys().copied().collect();
    for (k, i) in keys.iter().enumerate() {
        let end = keys.get(k + 1).copied().unwrap_or(tail + 1);
        if let Some(o) = objs.get_mut(i)
            && o.kind != Kind::Seg
        {
            o.end = end;
        }
    }
    for p in stale {
        let _ = store.delete(&p).await;
    }
    let first = objs.keys().next().copied().unwrap_or(tail).max(marker.first);
    let bytes = objs.values().map(|o| o.size).sum();
    info!(log = key, epoch, first, next = tail + 1, "opened S3 Express log");
    let st = St {
        first,
        next: tail + 1,
        objs,
        tail: VecDeque::new(),
        tail_bytes: 0,
        uncertain: None,
        fenced: None,
        destroyed: false,
        bytes,
        staged_from: tail + 1,
        staged_bytes: 0,
        staged_objs: Vec::new(),
        staged_since: None,
        linked: None,
        first_dirty: false,
    };
    Ok(Arc::new(Shared {
        key: key.to_string(),
        dir,
        epoch,
        writer,
        ctx,
        st: Mutex::new(st),
        write: tokio::sync::Mutex::new(()),
        maint: tokio::sync::Mutex::new(()),
        wake: Notify::new(),
        closed: AtomicBool::new(false),
        done: tokio::sync::watch::channel(false).0,
    }))
}

/// The stream of the log's last link, unless a clean close or a drain says it holds nothing more.
async fn undrained_link(ctx: &Ctx, listing: &Listing) -> anyhow::Result<Option<u64>> {
    let store = ctx.cfg.store.as_ref();
    for (_, (path, size)) in listing.batches.iter().rev() {
        match *size as usize {
            // A closing marker (or a fence marker) after the last link: nothing pending.
            HEADER_LEN => return Ok(None),
            LINK_LEN => {
                ctx.requests.get();
                let b = store.get(path).await.with_context(|| format!("read {path}"))?.bytes().await?;
                anyhow::ensure!(b.len() == LINK_LEN, "bad link {path}");
                if decode_header(&b)?.2 != 0 {
                    // A batch of one 8-byte entry is as long as a link.
                    continue;
                }
                let stream = u64::from_be_bytes(b[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
                ctx.requests.get();
                return match store.head(&group::stream_dir(&ctx.streams, stream).join("drained")).await {
                    Ok(_) => Ok(None),
                    Err(object_store::Error::NotFound { .. }) => Ok(Some(stream)),
                    Err(e) => Err(e.into()),
                };
            }
            _ => {}
        }
    }
    Ok(None)
}

/// Fence `stream` so its writer can't confirm anything more, copy every record in it into its
/// log's own objects, and mark it drained (see [`crate::group`]). Safe to run concurrently or
/// again: every write is put-if-absent and skips what is already there.
async fn drain_stream(ctx: &Ctx, stream: u64) -> anyhow::Result<()> {
    let store = ctx.cfg.store.as_ref();
    let dir = group::stream_dir(&ctx.streams, stream);
    let depth = dir.parts().count();
    ctx.requests.list();
    let metas: Vec<_> = store.list(Some(&dir)).try_collect().await.with_context(|| format!("list {dir}"))?;
    let mut objects: BTreeMap<u64, u64> = BTreeMap::new();
    for m in metas {
        let name: Vec<String> = m.location.parts().skip(depth).map(|p| p.as_ref().to_string()).collect();
        match name.as_slice() {
            [n] if n == "drained" => return Ok(()),
            [n] => {
                if let Ok(seq) = n.parse() {
                    objects.insert(seq, m.size);
                }
            }
            _ => {}
        }
    }
    // The fence: an existing one, or ours right after the last object (further on if the writer
    // gets there first). Nothing at or after it was ever confirmed.
    let mut fence = match objects.iter().find(|(_, size)| **size as usize == group::STREAM_HEADER) {
        Some((seq, _)) => *seq,
        None => objects.keys().next_back().map(|s| s + 1).unwrap_or(0),
    };
    if !objects.get(&fence).is_some_and(|size| *size as usize == group::STREAM_HEADER) {
        let nonce = mq_core::rand_u64();
        loop {
            ctx.requests.put();
            let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
            match store.put_opts(&group::object_path(&ctx.streams, stream, fence), group::encode_fence(stream, nonce).into(), opts).await {
                Ok(_) => break,
                Err(e) if is_already_exists(&e) => {
                    ctx.requests.get();
                    let h = store.get_range(&group::object_path(&ctx.streams, stream, fence), 0..group::STREAM_HEADER as u64).await?;
                    if group::decode_object(h).is_ok_and(|(_, recs)| recs.is_empty()) {
                        break;
                    }
                    objects.insert(fence, 0);
                    fence += 1;
                }
                Err(e) => return Err(e).context("fence stream"),
            }
        }
    }
    let mut by_log: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    for (&seq, _) in objects.range(..fence) {
        ctx.requests.get();
        let path = group::object_path(&ctx.streams, stream, seq);
        let data = match store.get(&path).await {
            Ok(r) => r.bytes().await?,
            // Already materialized and deleted by its writer.
            Err(object_store::Error::NotFound { .. }) => continue,
            Err(e) => return Err(e).with_context(|| format!("get {path}")),
        };
        for rec in group::decode_object(data)?.1 {
            by_log.entry(rec.key.clone()).or_default().push(rec);
        }
    }
    let logs = by_log.len();
    for (key, mut recs) in by_log {
        recs.sort_by_key(|r| r.first);
        materialize(ctx, &key, recs).await.with_context(|| format!("materialize {key}"))?;
    }
    ctx.requests.put();
    store.put(&dir.clone().join("drained"), Bytes::new().into()).await.context("mark stream drained")?;
    for (&seq, &size) in objects.range(..fence) {
        if size as usize != group::STREAM_HEADER {
            ctx.requests.delete();
            let _ = store.delete(&group::object_path(&ctx.streams, stream, seq)).await;
        }
    }
    info!(stream = format!("{stream:016x}"), fence, logs, "drained stream");
    Ok(())
}

/// Copy records of one log from a stream into the log's own objects, as their writer would
/// have: put-if-absent at the end of what's there, skipping whatever an object already holds.
async fn materialize(ctx: &Ctx, key: &str, recs: Vec<Record>) -> anyhow::Result<()> {
    let store = ctx.cfg.store.as_ref();
    let dir = tier::log_dir(&ctx.cfg.prefix, key)?;
    ctx.requests.list();
    let listing = list(store, &dir).await?;
    if listing.batches.is_empty() && listing.segs.is_empty() {
        // Destroyed (or never opened): nothing to bring back.
        return Ok(());
    }
    for rec in recs {
        let mut at = rec.first;
        let mut rest = &rec.entries[..];
        // An object starting before the record may already cover its beginning.
        let before = listing.segs.range(..at).next_back().map(|(a, (b, _, _))| (*a, Some(*b))).into_iter().chain(listing.batches.range(..at).next_back().map(|(a, _)| (*a, None))).max_by_key(|(a, _)| *a);
        if let Some((start, end)) = before {
            let end = match end {
                Some(b) => b,
                None => {
                    ctx.requests.get();
                    let (_, _, n) = read_header(store, &dir.clone().join(format!("{start:020}"))).await?;
                    start + (n as u64).max(1)
                }
            };
            if end > at {
                let skip = ((end - at) as usize).min(rest.len());
                rest = &rest[skip..];
                at = end;
            }
        }
        while !rest.is_empty() {
            ctx.requests.put();
            let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
            let path = dir.clone().join(format!("{at:020}"));
            match store.put_opts(&path, encode_batch(rec.epoch, rec.writer, rest).into(), opts).await {
                Ok(_) => break,
                Err(e) if is_already_exists(&e) => {
                    ctx.requests.get();
                    let (_, writer, n) = read_header(store, &path).await?;
                    if writer != rec.writer {
                        // Someone else's object: the log moved on before this point, so nothing
                        // from here was ever confirmed.
                        break;
                    }
                    let k = (n as usize).max(1);
                    rest = &rest[k.min(rest.len())..];
                    at += k as u64;
                }
                Err(e) => return Err(e).with_context(|| format!("put {path}")),
            }
        }
    }
    Ok(())
}

async fn compactor(sh: Arc<Shared>) {
    compact_loop(&sh).await;
    if let Some(g) = &sh.ctx.group {
        sh.close_grouped(g).await;
    }
    let _ = sh.done.send(true);
}

async fn compact_loop(sh: &Arc<Shared>) {
    let mut every = (sh.ctx.cfg.compact_after / 2).max(Duration::from_millis(500));
    if let Some(g) = &sh.ctx.group {
        every = every.min((g.cfg.flush_after / 2).max(Duration::from_millis(100)));
    }
    while !sh.closed.load(Ordering::Acquire) {
        tokio::select! {
            _ = sh.wake.notified() => {}
            _ = tokio::time::sleep(every) => {}
        }
        if sh.closed.load(Ordering::Acquire) {
            return;
        }
        if let Some(g) = &sh.ctx.group
            && sh.flush_due(g)
        {
            let _w = sh.write.lock().await;
            if let Err(e) = sh.flush_locked(g).await {
                warn!(log = %sh.key, error = %format!("{e:#}"), "materializing staged entries failed");
            }
        }
        loop {
            match sh.compact_once(false).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    warn!(log = %sh.key, error = %format!("{e:#}"), "compaction failed");
                    break;
                }
            }
        }
    }
}

enum PutErr {
    Fenced(u64),
    Other(anyhow::Error),
}

impl Shared {
    fn fenced_err(&self, current: u64) -> anyhow::Error {
        FencedError { log: self.key.clone(), epoch: self.epoch, current }.into()
    }

    async fn put_batch(&self, index: u64, body: Bytes) -> Result<(), PutErr> {
        self.put_or_existing(index, body).await.map(|_| ())
    }

    /// Put-if-absent at `index`. `Ok(Some(n))`: an object of ours was already there, holding `n`
    /// entries (an earlier attempt, or a copy a drainer made from our stream).
    async fn put_or_existing(&self, index: u64, body: Bytes) -> Result<Option<u32>, PutErr> {
        let path = self.dir.clone().join(format!("{index:020}"));
        let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
        self.ctx.requests.put();
        match self.ctx.cfg.store.put_opts(&path, body.into(), opts).await {
            Ok(_) => Ok(None),
            Err(e) if is_already_exists(&e) => {
                self.ctx.requests.get();
                match read_header(self.ctx.cfg.store.as_ref(), &path).await {
                    Ok((_, w, n)) if w == self.writer => Ok(Some(n)),
                    Ok((e, _, _)) => Err(PutErr::Fenced(e)),
                    Err(e) => Err(PutErr::Other(e)),
                }
            }
            Err(e) => Err(PutErr::Other(e.into())),
        }
    }

    async fn released(&self) {
        let mut rx = self.done.subscribe();
        let _ = rx.wait_for(|d| *d).await;
    }

    fn insert_obj(&self, index: u64, end: u64, size: u64, kind: Kind) {
        let mut st = self.st.lock();
        let path = self.dir.clone().join(format!("{index:020}"));
        st.objs.insert(index, Obj { end, size, kind, path, born: Some(Instant::now()) });
        st.bytes += size;
    }

    /// Put a marker (or, with `link`, a link to that stream position) at the next index.
    async fn put_marker(&self, link: Option<(u64, u64)>) -> anyhow::Result<()> {
        let idx = self.st.lock().next;
        let body = match link {
            Some((stream, from)) => encode_link(self.epoch, self.writer, stream, from),
            None => encode_batch(self.epoch, self.writer, &[]),
        };
        let size = body.len() as u64;
        match self.put_or_existing(idx, body).await {
            Ok(_) => {
                self.insert_obj(idx, idx + 1, size, Kind::Marker);
                let mut st = self.st.lock();
                st.next = st.next.max(idx + 1);
                st.staged_from = st.staged_from.max(st.next);
                st.linked = link.map(|l| l.0);
                Ok(())
            }
            Err(PutErr::Fenced(cur)) => {
                self.st.lock().fenced = Some(cur);
                Err(self.fenced_err(cur))
            }
            Err(PutErr::Other(e)) => Err(e.context(format!("log {}: put marker", self.key))),
        }
    }

    /// Keep entries `[idx, idx + n)` that went into stream object `obj` (if known).
    fn stage(&self, g: &Group, idx: u64, entries: &[Bytes], obj: Option<(u64, u64)>) {
        let n = entries.len() as u64;
        let bytes: usize = entries.iter().map(|e| e.len()).sum();
        let mut st = self.st.lock();
        if st.staged_from >= st.next {
            st.staged_from = idx;
        }
        st.next = st.next.max(idx + n);
        st.staged_bytes += bytes;
        st.staged_since.get_or_insert_with(Instant::now);
        if let Some((stream, seq)) = obj {
            st.staged_objs.push((stream, seq, idx + n));
        }
        for (k, e) in entries.iter().enumerate() {
            st.tail_bytes += e.len();
            st.tail.push_back((idx + k as u64, e.clone()));
        }
        g.staged.fetch_add(bytes, Ordering::Relaxed);
    }

    fn flush_due(&self, g: &Group) -> bool {
        let st = self.st.lock();
        st.staged_from < st.next
            && (st.staged_bytes >= g.cfg.flush_bytes
                || st.staged_since.is_some_and(|t| t.elapsed() >= g.cfg.flush_after)
                || g.staged.load(Ordering::Relaxed) > g.cfg.max_staged_bytes)
    }

    /// Write staged entries into the log's own objects (the caller holds `write`), and the
    /// epoch marker if truncation moved `first`.
    async fn flush_locked(&self, g: &Arc<Group>) -> anyhow::Result<()> {
        let (mut at, staged): (u64, Vec<(u64, Bytes)>) = {
            let st = self.st.lock();
            (st.staged_from, st.tail.iter().filter(|(i, _)| *i >= st.staged_from).cloned().collect())
        };
        let mut rest = &staged[..];
        while !rest.is_empty() {
            // Chunks of about a segment each.
            let mut size = 0u64;
            let n = rest.iter().take_while(|(_, e)| {
                size += e.len() as u64 + 8;
                size <= self.ctx.cfg.segment_bytes
            });
            let n = n.count().max(1);
            let chunk: Vec<Bytes> = rest[..n].iter().map(|(_, e)| e.clone()).collect();
            let body = encode_batch(self.epoch, self.writer, &chunk);
            let len = body.len() as u64;
            match self.put_or_existing(at, body).await {
                Ok(None) => {
                    self.insert_obj(at, at + n as u64, len, Kind::Batch);
                    at += n as u64;
                    rest = &rest[n..];
                }
                // Already there (a drainer copied it from our stream): skip what it holds.
                Ok(Some(k)) => {
                    let k = (k as u64).max(1);
                    self.insert_obj(at, at + k, 0, Kind::Batch);
                    at += k;
                    rest = &rest[(k as usize).min(rest.len())..];
                }
                Err(PutErr::Fenced(cur)) => {
                    self.st.lock().fenced = Some(cur);
                    return Err(self.fenced_err(cur));
                }
                Err(PutErr::Other(e)) => return Err(e.context(format!("log {}: materialize staged entries", self.key))),
            }
        }
        let (released, bytes, marker) = {
            let mut st = self.st.lock();
            st.staged_from = st.staged_from.max(at);
            let upto = st.staged_from;
            let (done, keep): (Vec<_>, Vec<_>) = st.staged_objs.drain(..).partition(|(_, _, end)| *end <= upto);
            st.staged_objs = keep;
            let bytes = std::mem::take(&mut st.staged_bytes);
            st.staged_since = None;
            let marker = std::mem::take(&mut st.first_dirty).then_some(st.first);
            trim_tail(&mut st, self.ctx.cfg.tail_bytes);
            (done, bytes, marker)
        };
        g.staged.fetch_sub(bytes, Ordering::Relaxed);
        for (stream, seq, _) in released {
            g.release(stream, seq);
        }
        if let Some(first) = marker {
            self.ctx.requests.put();
            let m = Marker { epoch: self.epoch, first };
            self.ctx.cfg.store.put(&self.dir.clone().join("epoch"), serde_json::to_vec(&m)?.into()).await.context("write epoch marker")?;
        }
        Ok(())
    }

    /// Grouped append (the caller holds `write`).
    async fn append_grouped(&self, g: &Arc<Group>, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        let mut last = None;
        for _ in 0..5 {
            let (stream, from) = g.current();
            if self.st.lock().linked != Some(stream) {
                // What's staged elsewhere goes to our own objects before we point at a new
                // stream: only the last link is ever drained.
                self.flush_locked(g).await?;
                self.put_marker(Some((stream, from))).await?;
            }
            let idx = self.st.lock().next;
            let rec = Record { key: self.key.clone(), epoch: self.epoch, writer: self.writer, first: idx, entries: entries.clone() };
            match g.submit(stream, rec).await {
                Ok(obj) => {
                    self.stage(g, idx, &entries, Some(obj));
                    if self.flush_due(g) {
                        self.wake.notify_one();
                    }
                    return Ok(idx);
                }
                Err(GroupErr::Changed | GroupErr::Fenced) => continue,
                Err(GroupErr::Failed(e, obj)) => {
                    // It may have landed: keep the indices; the entries reach our own objects at
                    // the next flush either way.
                    self.stage(g, idx, &entries, obj);
                    self.wake.notify_one();
                    last = Some(anyhow::anyhow!("log {}: append failed: {e}", self.key));
                    break;
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("log {}: the stream kept changing", self.key)))
    }

    /// A closed log: materialize what it staged and mark the clean close, so whoever opens it
    /// next needn't drain our stream.
    async fn close_grouped(&self, g: &Arc<Group>) {
        let _w = self.write.lock().await;
        let linked = {
            let st = self.st.lock();
            st.linked.is_some() && st.fenced.is_none() && !st.destroyed
        };
        if !linked {
            return;
        }
        if let Err(e) = self.flush_locked(g).await {
            warn!(log = %self.key, error = %format!("{e:#}"), "could not materialize staged entries on close; the next owner drains the stream");
            return;
        }
        if let Err(e) = self.put_marker(None).await {
            warn!(log = %self.key, error = %format!("{e:#}"), "could not mark a clean close");
        }
    }

    fn record(&self, index: u64, entries: &[Bytes], size: u64) {
        let mut st = self.st.lock();
        let n = (entries.len() as u64).max(1);
        let path = self.dir.clone().join(format!("{index:020}"));
        st.objs.insert(index, Obj { end: index + n, size, kind: Kind::Batch, path, born: Some(Instant::now()) });
        st.bytes += size;
        st.next = st.next.max(index + n);
        for (k, e) in entries.iter().enumerate() {
            st.tail_bytes += e.len();
            st.tail.push_back((index + k as u64, e.clone()));
        }
        st.staged_from = st.next;
        trim_tail(&mut st, self.ctx.cfg.tail_bytes);
    }

    async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        let started = Instant::now();
        let _w = self.write.lock().await;
        {
            let st = self.st.lock();
            if let Some(cur) = st.fenced {
                return Err(self.fenced_err(cur));
            }
            if st.destroyed {
                bail!("log {} was destroyed", self.key);
            }
            if entries.is_empty() {
                return Ok(st.next);
            }
        }
        if let Some(g) = &self.ctx.group {
            let bytes: u64 = entries.iter().map(|e| e.len() as u64).sum();
            let res = self.append_grouped(g, entries).await;
            self.ctx.metrics.record(started.elapsed(), bytes, res.is_ok());
            return res;
        }
        let pending = self.st.lock().uncertain.clone();
        if let Some((idx, body)) = pending {
            match self.put_batch(idx, body).await {
                Ok(()) => self.st.lock().uncertain = None,
                Err(PutErr::Fenced(cur)) => {
                    self.st.lock().fenced = Some(cur);
                    return Err(self.fenced_err(cur));
                }
                Err(PutErr::Other(e)) => {
                    self.ctx.metrics.record(started.elapsed(), 0, false);
                    return Err(e.context(format!("log {}: retrying an earlier batch", self.key)));
                }
            }
        }
        let idx = self.st.lock().next;
        let body = encode_batch(self.epoch, self.writer, &entries);
        let size = body.len() as u64;
        let bytes: u64 = entries.iter().map(|e| e.len() as u64).sum();
        let res = match self.put_batch(idx, body.clone()).await {
            Ok(()) => {
                self.record(idx, &entries, size);
                Ok(idx)
            }
            Err(PutErr::Fenced(cur)) => {
                self.st.lock().fenced = Some(cur);
                Err(self.fenced_err(cur))
            }
            Err(PutErr::Other(e)) => {
                // It may have landed: keep the indices and resend the same batch first next time.
                self.record(idx, &entries, size);
                self.st.lock().uncertain = Some((idx, body));
                Err(e.context(format!("log {}: append failed", self.key)))
            }
        };
        self.ctx.metrics.record(started.elapsed(), bytes, res.is_ok());
        if res.is_ok() && self.st.lock().objs.len() >= self.ctx.cfg.compact_min_batches {
            self.wake.notify_one();
        }
        res
    }

    async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut at = from;
        while out.len() < max && (out.is_empty() || bytes < max_bytes) {
            let obj = {
                let st = self.st.lock();
                at = at.max(st.first);
                if at >= st.next {
                    break;
                }
                if st.tail.front().is_some_and(|(i, _)| *i <= at) {
                    for (i, e) in st.tail.iter().skip_while(|(i, _)| *i < at) {
                        if out.len() >= max || (bytes > 0 && bytes + e.len() > max_bytes) {
                            break;
                        }
                        bytes += e.len();
                        out.push((*i, e.clone()));
                    }
                    break;
                }
                match st.objs.range(..=at).next_back().filter(|(_, o)| at < o.end) {
                    Some((i, o)) if o.kind == Kind::Marker => {
                        at = o.end.max(*i + 1);
                        continue;
                    }
                    Some((i, o)) => (*i, o.clone()),
                    None => match st.objs.range(at..).next() {
                        Some((i, _)) => {
                            at = *i;
                            continue;
                        }
                        None => break,
                    },
                }
            };
            let seg = self.fetch(obj.0, &obj.1).await?;
            let next_at = seg.read_into(at, max, max_bytes, &mut out, &mut bytes);
            if out.len() >= max || next_at < seg.end {
                break;
            }
            at = next_at.max(obj.1.end);
        }
        Ok(out)
    }

    async fn fetch(&self, index: u64, o: &Obj) -> anyhow::Result<Arc<SegmentData>> {
        let store = self.ctx.cfg.store.as_ref();
        match o.kind {
            Kind::Seg => tier::fetch_segment(store, &self.ctx.cache, &o.path).await,
            _ => {
                let key = o.path.to_string();
                if let Some(s) = self.ctx.cache.get(&key) {
                    return Ok(s);
                }
                let data = store.get(&o.path).await.with_context(|| format!("get {}", o.path))?.bytes().await?;
                let seg = Arc::new(decode_batch(index, data)?);
                self.ctx.cache.insert(&key, seg.clone());
                Ok(seg)
            }
        }
    }

    async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64> {
        let _m = self.maint.lock().await;
        let (cut, first) = {
            let st = self.st.lock();
            if let Some(cur) = st.fenced {
                return Err(self.fenced_err(cur));
            }
            let in_objs = st.objs.iter().find(|(_, o)| o.end > before).map(|(i, _)| *i);
            let cut = match in_objs {
                // Staged entries are addressable one by one (they're in memory).
                _ if before >= st.staged_from => before.min(st.next),
                Some(i) => i,
                None => st.next,
            };
            let limit = st.uncertain.as_ref().map(|(i, _)| *i).unwrap_or(st.next);
            (cut.min(limit).max(st.first), st.first)
        };
        if cut <= first {
            return Ok(first);
        }
        if self.ctx.group.is_some() {
            // Written with the next flush: until then a reader starts earlier and replays
            // records that cancel out, which costs time, not correctness.
            self.st.lock().first_dirty = true;
        } else {
            self.ctx.requests.put();
            let marker = Marker { epoch: self.epoch, first: cut };
            self.ctx.cfg.store.put(&self.dir.clone().join("epoch"), serde_json::to_vec(&marker)?.into()).await.context("write epoch marker")?;
        }
        let gone: Vec<Obj> = {
            let mut st = self.st.lock();
            st.first = cut;
            let keep = st.objs.split_off(&cut);
            let gone: Vec<Obj> = std::mem::replace(&mut st.objs, keep).into_values().collect();
            st.bytes -= gone.iter().map(|o| o.size).sum::<u64>();
            // Staged entries stay until they're materialized, so a log's objects never have gaps.
            while st.tail.front().is_some_and(|(i, _)| *i < cut && *i < st.staged_from) {
                if let Some((_, e)) = st.tail.pop_front() {
                    st.tail_bytes -= e.len();
                }
            }
            gone
        };
        for o in gone.into_iter().filter(|o| o.kind != Kind::Marker) {
            self.ctx.cache.remove(o.path.as_ref());
            self.ctx.requests.delete();
            if let Err(e) = self.ctx.cfg.store.delete(&o.path).await
                && !matches!(e, object_store::Error::NotFound { .. })
            {
                warn!(path = %o.path, error = %e, "could not delete truncated object");
            }
        }
        Ok(cut)
    }

    /// Merge the oldest run of batch objects into one segment. Returns whether it did.
    async fn compact_once(&self, force: bool) -> anyhow::Result<bool> {
        let _m = self.maint.lock().await;
        let cfg = &self.ctx.cfg;
        let run: Vec<(u64, Obj)> = {
            let st = self.st.lock();
            if st.fenced.is_some() || st.destroyed {
                return Ok(false);
            }
            let limit = st.uncertain.as_ref().map(|(i, _)| *i).unwrap_or(u64::MAX);
            let mut run = Vec::new();
            let mut size = 0;
            for (i, o) in st.objs.iter().skip_while(|(_, o)| o.kind == Kind::Seg) {
                let old = force || o.born.is_none_or(|b| b.elapsed() >= cfg.compact_after);
                if o.kind == Kind::Seg || !old || *i >= limit || size >= cfg.segment_bytes {
                    break;
                }
                size += o.size;
                run.push((*i, o.clone()));
            }
            // Never fold the newest object away: it holds the writer's tail.
            if run.last().is_some_and(|(_, o)| o.end >= st.next) {
                run.pop();
            }
            let batches = run.iter().filter(|(_, o)| o.kind == Kind::Batch).count();
            if run.len() < 2 || batches == 0 || (!force && run.len() < cfg.compact_min_batches && size < cfg.segment_bytes) {
                return Ok(false);
            }
            run
        };
        let mut entries = Vec::new();
        for (i, o) in &run {
            if o.kind == Kind::Batch {
                entries.extend(self.fetch(*i, o).await?.entries.iter().cloned());
            }
        }
        let (a, b) = (run[0].0, run[run.len() - 1].1.end);
        let data = tier::encode_segment(&entries, a, b);
        let size = data.len() as u64;
        let path = self.dir.clone().join("seg").join(format!("{a:020}-{b:020}"));
        cfg.store.put(&path, data.into()).await.with_context(|| format!("put {path}"))?;
        let ok = {
            let mut st = self.st.lock();
            if run.iter().all(|(i, _)| st.objs.contains_key(i)) {
                for (i, o) in &run {
                    st.objs.remove(i);
                    st.bytes -= o.size;
                }
                st.objs.insert(a, Obj { end: b, size, kind: Kind::Seg, path: path.clone(), born: None });
                st.bytes += size;
                true
            } else {
                false
            }
        };
        if !ok {
            let _ = cfg.store.delete(&path).await;
            return Ok(false);
        }
        for (_, o) in run.into_iter().filter(|(_, o)| o.kind == Kind::Batch) {
            self.ctx.cache.remove(o.path.as_ref());
            let _ = cfg.store.delete(&o.path).await;
        }
        debug!(log = %self.key, first = a, end = b, bytes = size, "compacted batches");
        Ok(true)
    }

    async fn destroy(&self) -> anyhow::Result<()> {
        let _m = self.maint.lock().await;
        let _w = self.write.lock().await;
        let store = &self.ctx.cfg.store;
        let metas: Vec<_> = store.list(Some(&self.dir)).try_collect().await?;
        for m in metas {
            self.ctx.cache.remove(m.location.as_ref());
            let _ = store.delete(&m.location).await;
        }
        let released = {
            let mut st = self.st.lock();
            st.destroyed = true;
            st.first = st.next;
            st.objs.clear();
            st.tail.clear();
            st.tail_bytes = 0;
            st.bytes = 0;
            st.uncertain = None;
            st.staged_from = st.next;
            st.staged_since = None;
            (std::mem::take(&mut st.staged_bytes), std::mem::take(&mut st.staged_objs))
        };
        if let Some(g) = &self.ctx.group {
            g.staged.fetch_sub(released.0, Ordering::Relaxed);
            for (stream, seq, _) in released.1 {
                g.release(stream, seq);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Log for ExpressLog {
    async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        self.sh.append(entries).await
    }

    async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>> {
        self.sh.read(from, max, max_bytes).await
    }

    fn bounds(&self) -> (u64, u64) {
        let st = self.sh.st.lock();
        (st.first, st.next)
    }

    async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64> {
        self.sh.truncate_prefix(before).await
    }

    fn bytes(&self) -> u64 {
        self.sh.st.lock().bytes
    }

    async fn destroy(&self) -> anyhow::Result<()> {
        self.sh.destroy().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_roundtrip() {
        let b = encode_batch(5, 9, &[Bytes::from_static(b"a"), Bytes::from_static(b"bc")]);
        assert_eq!(decode_header(&b).unwrap(), (5, 9, 2));
        let s = decode_batch(40, b).unwrap();
        assert_eq!(s.end, 42);
        assert_eq!(s.entries[1], (41, Bytes::from_static(b"bc")));
        let m = decode_batch(7, encode_batch(1, 1, &[])).unwrap();
        assert_eq!((m.end, m.entries.len()), (8, 0));
    }
}

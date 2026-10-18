//! Durable logs behind queues and streams.
//!
//! A log is a sequence of opaque entries numbered from 0. Queues append records (enqueue,
//! settle) and compact by copying live messages forward and truncating whole segments; streams
//! append messages and read them back by offset. Backends: memory (tests), local segment files
//! (single node, and the storage keepers' own disks) and, in `mq-storage`, the quorum-replicated
//! log and the S3 tier.

use anyhow::{Context, bail};
use async_trait::async_trait;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::wal::{Checkpoint, Wal, WalWriter};

/// Resolves once a pipelined append is durable.
pub type Durable = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;

#[async_trait]
pub trait Log: Send + Sync {
    /// Append entries durably (fsync, or a quorum of replicas). Returns the first new index.
    async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64>;
    /// Append entries and return as soon as they have their indices (in call order), with a
    /// future that resolves once they are durable, so the caller can carry on meanwhile. Logs
    /// that can't separate the two wait here and return a finished future.
    async fn append_pipelined(&self, entries: Vec<Bytes>) -> anyhow::Result<(u64, Durable)> {
        let at = self.append(entries).await?;
        Ok((at, Box::pin(std::future::ready(Ok(())))))
    }
    /// Append entries in order without waiting for durability; they become durable with the
    /// next [`Log::append`]. For records whose loss only means a redelivery (acks).
    async fn append_relaxed(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        self.append(entries).await
    }
    /// Up to `max` entries starting at `from` (clamped to the first retained index).
    async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>>;
    /// `(first, next)`: the first retained index and the index the next append gets.
    fn bounds(&self) -> (u64, u64);
    /// Drop whole segments that end before `before`. Returns the new first index.
    async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64>;
    /// Bytes retained (local and tiered).
    fn bytes(&self) -> u64;
    /// Delete everything (the queue was deleted).
    async fn destroy(&self) -> anyhow::Result<()>;
    /// Seal the active segment so it can be tiered/truncated (streams roll segments by size).
    async fn roll(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
pub trait LogStore: Send + Sync {
    /// Open (or create) a log. `epoch` fences off older writers in replicated backends.
    async fn open(&self, key: &str, epoch: u64) -> anyhow::Result<Arc<dyn Log>>;
    fn describe(&self) -> String;
    /// Make everything written durable and release background work, after every log is done.
    async fn close(&self) {}
}

// ── memory ──

#[derive(Default)]
pub struct MemStore {
    logs: Mutex<std::collections::HashMap<String, Arc<MemLog>>>,
}

impl MemStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl LogStore for MemStore {
    async fn open(&self, key: &str, _epoch: u64) -> anyhow::Result<Arc<dyn Log>> {
        Ok(self.logs.lock().entry(key.to_string()).or_insert_with(|| Arc::new(MemLog::new(4096))).clone())
    }
    fn describe(&self) -> String {
        "memory".into()
    }
}

/// In-memory log with fixed-size "segments" (for tests and transient use).
pub struct MemLog {
    inner: Mutex<MemInner>,
    segment_entries: u64,
}

struct MemInner {
    first: u64,
    entries: VecDeque<Bytes>,
    bytes: u64,
}

impl MemLog {
    pub fn new(segment_entries: u64) -> Self {
        MemLog { inner: Mutex::new(MemInner { first: 0, entries: VecDeque::new(), bytes: 0 }), segment_entries }
    }
}

#[async_trait]
impl Log for MemLog {
    async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        let mut i = self.inner.lock();
        let at = i.first + i.entries.len() as u64;
        for e in entries {
            i.bytes += e.len() as u64;
            i.entries.push_back(e);
        }
        Ok(at)
    }
    async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>> {
        let i = self.inner.lock();
        let from = from.max(i.first);
        let mut out = Vec::new();
        let mut bytes = 0;
        for (n, e) in i.entries.iter().enumerate().skip((from - i.first) as usize) {
            if out.len() >= max || (bytes > 0 && bytes + e.len() > max_bytes) {
                break;
            }
            bytes += e.len();
            out.push((i.first + n as u64, e.clone()));
        }
        Ok(out)
    }
    fn bounds(&self) -> (u64, u64) {
        let i = self.inner.lock();
        (i.first, i.first + i.entries.len() as u64)
    }
    async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64> {
        let mut i = self.inner.lock();
        let seg = self.segment_entries;
        let cut = (before / seg) * seg;
        while i.first < cut && !i.entries.is_empty() {
            let e = i.entries.pop_front().unwrap();
            i.bytes -= e.len() as u64;
            i.first += 1;
        }
        Ok(i.first)
    }
    fn bytes(&self) -> u64 {
        self.inner.lock().bytes
    }
    async fn destroy(&self) -> anyhow::Result<()> {
        let mut i = self.inner.lock();
        i.first += i.entries.len() as u64;
        i.entries.clear();
        i.bytes = 0;
        Ok(())
    }
}

// ── local segment files ──

/// Logs as directories of segment files under a root directory.
pub struct FileStore {
    root: PathBuf,
    pub segment_bytes: u64,
    pub fsync: bool,
    /// Shared WAL: logs skip their own fsyncs and commit through it in groups.
    wal: Option<Arc<Wal>>,
}

/// WAL file size that triggers a checkpoint.
const WAL_LIMIT: u64 = 256 << 20;

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(FileStore { root: root.into(), segment_bytes: 16 << 20, fsync: true, wal: None })
    }

    pub fn with(root: impl Into<PathBuf>, segment_bytes: u64, fsync: bool) -> Arc<Self> {
        Arc::new(FileStore { root: root.into(), segment_bytes, fsync, wal: None })
    }

    /// A store whose logs group-commit through a shared WAL in `root/.wal`, replaying whatever
    /// a crash left in it first. Only for a root one process owns: another node taking a log
    /// over could not see this node's WAL.
    pub fn with_wal(root: impl Into<PathBuf>, segment_bytes: u64) -> anyhow::Result<Arc<Self>> {
        let root = root.into();
        let (writer, records, old) = WalWriter::open(&root.join(".wal")).context("open the WAL")?;
        let mut store = FileStore { root, segment_bytes, fsync: true, wal: None };
        if !records.is_empty() {
            store.recover(&records)?;
            tracing::info!(records = records.len(), "replayed the WAL");
        }
        writer.remove(&old)?;
        store.wal = Some(Wal::start(writer, WAL_LIMIT)?);
        Ok(Arc::new(store))
    }

    /// Stop the WAL as a crash would, leaving it for the next start to replay. For tests.
    #[doc(hidden)]
    pub fn crash_wal(&self) {
        if let Some(w) = &self.wal {
            w.crash();
        }
    }

    /// Re-apply WAL records to the logs they belong to and make them durable.
    fn recover(&self, records: &[Bytes]) -> anyhow::Result<()> {
        // key -> runs of entries still to apply (None: destroyed).
        type Runs = Vec<(u64, Vec<Bytes>)>;
        let mut logs: std::collections::BTreeMap<String, Option<Runs>> = Default::default();
        for r in records {
            let Some(rec) = WalRecord::decode(r.clone()) else {
                bail!("unreadable WAL record");
            };
            match rec {
                WalRecord::Append { key, first, entries } => logs.entry(key).or_insert(Some(Vec::new())).get_or_insert_default().push((first, entries)),
                WalRecord::Truncate { key, before } => {
                    if let Some(Some(runs)) = logs.get_mut(&key) {
                        runs.retain_mut(|(first, entries)| {
                            let drop = (before.saturating_sub(*first) as usize).min(entries.len());
                            entries.drain(..drop);
                            *first += drop as u64;
                            !entries.is_empty()
                        });
                    }
                }
                WalRecord::Destroy { key } => {
                    logs.insert(key, None);
                }
            }
        }
        for (key, runs) in logs {
            let dir = self.path_for(&key);
            match runs {
                None => {
                    std::fs::remove_dir_all(&dir).ok();
                }
                Some(runs) => {
                    let log = FileLog::open(&dir, self.segment_bytes, true)?;
                    let mut i = log.inner.lock();
                    for (first, entries) in runs {
                        if first > i.next {
                            if i.segments.iter().any(|s| !s.offsets.is_empty()) {
                                bail!("WAL for {key} starts at {first} but the log ends at {}", i.next);
                            }
                            // Every segment was truncated away before the crash; continue numbering.
                            for s in i.segments.drain(..) {
                                std::fs::remove_file(&s.path).ok();
                            }
                            i.active = None;
                            i.next = first;
                        }
                        let skip = (i.next - first) as usize;
                        if skip < entries.len() {
                            i.write_locked(&entries[skip..], false)?;
                        }
                    }
                    i.close_active()?;
                }
            }
        }
        Ok(())
    }

    pub fn path_for(&self, key: &str) -> PathBuf {
        // Keys are `kind/vhost/name/generation`; percent-encode anything path-hostile.
        let mut p = self.root.clone();
        for part in key.split('/') {
            p.push(encode_component(part));
        }
        p
    }
}

pub fn encode_component(s: &str) -> String {
    if s.is_empty() {
        return "%".into();
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[async_trait]
impl LogStore for FileStore {
    async fn open(&self, key: &str, _epoch: u64) -> anyhow::Result<Arc<dyn Log>> {
        let dir = self.path_for(key);
        let (segment_bytes, fsync) = (self.segment_bytes, self.fsync);
        let log = tokio::task::spawn_blocking(move || FileLog::open(&dir, segment_bytes, fsync)).await??;
        if let Some(wal) = &self.wal {
            log.inner.lock().wal = Some((wal.clone(), key.to_string()));
            let weak: std::sync::Weak<dyn Checkpoint> = Arc::downgrade(&(log.inner.clone() as Arc<dyn Checkpoint>));
            wal.register(weak);
        }
        Ok(Arc::new(log))
    }
    fn describe(&self) -> String {
        match self.wal {
            Some(_) => format!("local disk with a shared WAL ({})", self.root.display()),
            None => format!("local disk ({})", self.root.display()),
        }
    }

    async fn close(&self) {
        if let Some(wal) = self.wal.clone() {
            let _ = tokio::task::spawn_blocking(move || wal.stop()).await;
        }
    }
}

/// A log stored as segment files `<first index>.seg`, each a run of
/// `[len u32][crc32 u32][entry]` frames. Torn tails are truncated on open.
pub struct FileLog {
    inner: Arc<Mutex<FileInner>>,
}

struct Segment {
    first: u64,
    path: PathBuf,
    /// Byte offset of each entry.
    offsets: Vec<u64>,
    len: u64,
}

struct FileInner {
    dir: PathBuf,
    segments: Vec<Segment>,
    active: Option<File>,
    segment_bytes: u64,
    fsync: bool,
    /// The active segment has writes that aren't fsynced yet (relaxed appends, or ones the
    /// WAL covers).
    dirty: bool,
    bytes: u64,
    next: u64,
    /// The store's WAL and this log's key in it.
    wal: Option<(Arc<Wal>, String)>,
    /// A segment was created since the directory was last fsynced.
    dir_dirty: bool,
    /// Closed segments with writes the WAL covers that aren't fsynced yet; the next checkpoint
    /// syncs them, so a full segment never makes an append wait for a large fsync.
    unsynced: Vec<File>,
}

impl FileLog {
    pub fn open(dir: &Path, segment_bytes: u64, fsync: bool) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let mut names: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let stem = p.file_stem()?.to_str()?.to_string();
                (p.extension()? == "seg").then(|| stem.parse().ok().map(|n| (n, p)))?
            })
            .collect();
        names.sort();
        let mut segments = Vec::new();
        let mut bytes = 0;
        let count = names.len();
        for (i, (first, path)) in names.into_iter().enumerate() {
            let (offsets, len) = scan_segment(&path, i + 1 == count)?;
            bytes += len;
            segments.push(Segment { first, path, offsets, len });
        }
        // Drop empty trailing segments except the very first so indices stay continuous.
        let next = segments.last().map(|s| s.first + s.offsets.len() as u64).unwrap_or(0);
        let inner = FileInner { dir: dir.to_path_buf(), segments, active: None, segment_bytes, fsync, dirty: false, bytes, next, wal: None, dir_dirty: false, unsynced: Vec::new() };
        Ok(FileLog { inner: Arc::new(Mutex::new(inner)) })
    }

    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce(&mut FileInner) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || f(&mut inner.lock())).await?
    }
}

fn scan_segment(path: &Path, is_last: bool) -> anyhow::Result<(Vec<u64>, u64)> {
    let mut f = File::open(path)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    let mut offsets = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        if pos + 8 + len > data.len() || crc32(&data[pos + 8..pos + 8 + len]) != crc {
            break;
        }
        offsets.push(pos as u64);
        pos += 8 + len;
    }
    if pos < data.len() {
        if !is_last {
            bail!("corrupt segment {} at byte {pos}", path.display());
        }
        // A torn write at the tail: the append was never acknowledged, so drop it.
        let f = OpenOptions::new().write(true).open(path)?;
        f.set_len(pos as u64)?;
        f.sync_all()?;
    }
    Ok((offsets, pos as u64))
}

pub fn crc32(data: &[u8]) -> u32 {
    // CRC-32 (IEEE) with the CPU's CRC instructions where there are any: every append checksums
    // its payload (in the segment, and again in the WAL), so this is on the hot path.
    crc32fast::hash(data)
}

impl FileInner {
    fn active_segment(&mut self) -> anyhow::Result<()> {
        let need_new = match self.segments.last() {
            None => true,
            Some(s) => s.len >= self.segment_bytes,
        };
        if need_new {
            let first = self.next;
            let path = self.dir.join(format!("{first:020}.seg"));
            let f = OpenOptions::new().create(true).append(true).open(&path)?;
            if self.fsync {
                // Make the new file's directory entry durable too: now, or (when the WAL covers
                // the file's contents) before the checkpoint that retires them.
                if self.wal.is_some() {
                    self.dir_dirty = true;
                } else {
                    File::open(&self.dir).and_then(|d| d.sync_all()).ok();
                }
            }
            self.segments.push(Segment { first, path, offsets: Vec::new(), len: 0 });
            self.active = Some(f);
        } else if self.active.is_none() {
            let path = self.segments.last().unwrap().path.clone();
            self.active = Some(OpenOptions::new().append(true).open(path)?);
        }
        Ok(())
    }
}

/// A drained log's only segment is deleted once it holds this much; until then it is reused, so
/// a queue whose consumers keep up doesn't create (and fsync) a new file for every message.
const DROP_DRAINED_BYTES: u64 = 1 << 20;

impl FileInner {
    /// Whether `truncate_prefix(before)` would delete anything.
    fn prunable(&self, before: u64) -> bool {
        match &self.segments[..] {
            [_, second, ..] => second.first <= before,
            [only] => self.next <= before && only.len > 0 && only.len >= DROP_DRAINED_BYTES.min(self.segment_bytes),
            [] => false,
        }
    }

    /// Close the active segment for good, first making any relaxed writes in it durable. With a
    /// WAL, they already are (in the WAL) and the next checkpoint syncs the segment.
    fn close_active(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.active.take()
            && self.fsync
            && self.dirty
        {
            if self.wal.is_some() {
                self.unsynced.push(f);
            } else {
                f.sync_data()?;
            }
        }
        self.dirty = false;
        if self.wal.is_some() { Ok(()) } else { self.sync_dir() }
    }

    /// Sync everything the WAL still covers for this log, here and now.
    fn sync_pending(&mut self) -> std::io::Result<()> {
        if self.fsync
            && self.dirty
            && let Some(f) = &self.active
        {
            f.sync_data()?;
        }
        self.dirty = false;
        for f in std::mem::take(&mut self.unsynced) {
            f.sync_data()?;
        }
        self.sync_dir()
    }

    fn sync_dir(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.dir_dirty) {
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(())
    }
}

impl FileInner {
    /// Write entries to the segments; with `sync`, fsync them (unless the WAL covers them).
    fn write_locked(&mut self, entries: &[Bytes], sync: bool) -> anyhow::Result<u64> {
        let at = self.next;
        let mut buf = BytesMut::new();
        let mut pending: Vec<u64> = Vec::new();
        let mut idx = 0;
        while idx < entries.len() {
            self.active_segment()?;
            let seg_len = self.segments.last().unwrap().len;
            buf.clear();
            pending.clear();
            let mut len = seg_len;
            while idx < entries.len() && (pending.is_empty() || len < self.segment_bytes) {
                let e = &entries[idx];
                pending.push(len);
                buf.put_u32(e.len() as u32);
                buf.put_u32(crc32(e));
                buf.put_slice(e);
                len += 8 + e.len() as u64;
                idx += 1;
            }
            let durable = self.fsync && sync && self.wal.is_none();
            let f = self.active.as_mut().unwrap();
            f.write_all(&buf)?;
            if durable {
                f.sync_data()?;
            }
            self.dirty = !durable;
            let seg = self.segments.last_mut().unwrap();
            seg.offsets.extend_from_slice(&pending);
            let added = len - seg.len;
            seg.len = len;
            self.bytes += added;
            self.next += pending.len() as u64;
            if len >= self.segment_bytes {
                self.close_active()?;
            }
        }
        Ok(at)
    }
}

/// Largest write [`FileLog::start_write`] does without the blocking pool.
const INLINE_WRITE: usize = 256 << 10;

impl FileLog {
    async fn write(&self, entries: Vec<Bytes>, sync: bool) -> anyhow::Result<u64> {
        let (at, durable) = self.start_write(entries, sync).await?;
        durable.await?;
        Ok(at)
    }

    /// Write to the segments and queue the WAL record; the future waits for the WAL's fsync.
    async fn start_write(&self, entries: Vec<Bytes>, sync: bool) -> anyhow::Result<(u64, Durable)> {
        fn write(i: &mut FileInner, entries: &[Bytes], sync: bool) -> anyhow::Result<(u64, Option<tokio::sync::oneshot::Receiver<std::io::Result<()>>>)> {
            let at = i.write_locked(entries, sync)?;
            // Queued under the log's lock, so the WAL sees this log's writes in order.
            let done = i.wal.as_ref().and_then(|(wal, key)| wal.submit(WalRecord::encode_append(key, at, entries), sync));
            Ok((at, done))
        }
        // Behind a WAL a write only reaches the page cache (the WAL thread does the fsync), so a
        // small one is done right here: handing it to the blocking pool costs more in thread
        // wake-ups than the write. A busy lock (a read from disk, a checkpoint) means waiting:
        // that goes to the pool.
        let small = entries.iter().map(|e| e.len() + 8).sum::<usize>() <= INLINE_WRITE;
        let inline = match small.then(|| self.inner.try_lock()).flatten() {
            Some(mut i) if i.wal.is_some() => Some(write(&mut i, &entries, sync)),
            _ => None,
        };
        let (at, done) = match inline {
            Some(r) => r?,
            None => self.blocking(move |i| write(i, &entries, sync)).await?,
        };
        let durable: Durable = match done {
            Some(done) => Box::pin(async move { done.await.context("WAL stopped")?.context("WAL commit") }),
            None => Box::pin(std::future::ready(Ok(()))),
        };
        Ok((at, durable))
    }

    /// Queue a marker in the WAL (truncations, destroys) and wait for it if `durable`.
    async fn mark(&self, rec: Option<Bytes>, durable: bool) -> anyhow::Result<()> {
        let done = rec.and_then(|rec| self.inner.lock().wal.as_ref().and_then(|(wal, _)| wal.submit(rec, durable)));
        if let Some(done) = done {
            done.await.context("WAL stopped")?.context("WAL commit")?;
        }
        Ok(())
    }
}

impl Checkpoint for Mutex<FileInner> {
    /// Collect what needs syncing under the lock, then fsync without it: appends to this log
    /// carry on (the newer WAL file covers them) instead of waiting behind a large fsync.
    fn checkpoint(&self) -> std::io::Result<()> {
        let (files, dir) = {
            let mut i = self.lock();
            let active = match &i.active {
                Some(f) if i.dirty => Some(f.try_clone()?),
                _ => None,
            };
            let mut files = std::mem::take(&mut i.unsynced);
            files.extend(active);
            i.dirty = false;
            let dir = std::mem::take(&mut i.dir_dirty).then(|| i.dir.clone());
            (files, dir)
        };
        let synced = files.iter().try_for_each(|f| f.sync_data()).and_then(|_| match &dir {
            Some(d) => File::open(d)?.sync_all(),
            None => Ok(()),
        });
        if synced.is_err() {
            // Keep the WAL file (the caller does) and try again at the next checkpoint.
            let mut i = self.lock();
            i.unsynced.extend(files);
            i.dir_dirty |= dir.is_some();
        }
        synced
    }
}

impl Drop for FileInner {
    fn drop(&mut self) {
        // Writes the WAL covers must be durable before a checkpoint can forget them.
        if let Err(e) = self.sync_pending() {
            tracing::warn!(dir = %self.dir.display(), error = %e, "syncing a closed log");
        }
    }
}

/// What the store logs in its WAL.
enum WalRecord {
    Append { key: String, first: u64, entries: Vec<Bytes> },
    Truncate { key: String, before: u64 },
    Destroy { key: String },
}

impl WalRecord {
    fn header(buf: &mut BytesMut, op: u8, key: &str, n: u64) {
        buf.put_u8(op);
        buf.put_u16(key.len() as u16);
        buf.put_slice(key.as_bytes());
        buf.put_u64(n);
    }

    fn encode_append(key: &str, first: u64, entries: &[Bytes]) -> Bytes {
        let mut buf = BytesMut::with_capacity(16 + key.len() + entries.iter().map(|e| 4 + e.len()).sum::<usize>());
        Self::header(&mut buf, 1, key, first);
        buf.put_u32(entries.len() as u32);
        for e in entries {
            buf.put_u32(e.len() as u32);
            buf.put_slice(e);
        }
        buf.freeze()
    }

    fn encode(op: u8, key: &str, n: u64) -> Bytes {
        let mut buf = BytesMut::new();
        Self::header(&mut buf, op, key, n);
        buf.freeze()
    }

    fn decode(mut b: Bytes) -> Option<WalRecord> {
        let need = |b: &Bytes, n: usize| (b.remaining() >= n).then_some(());
        need(&b, 3)?;
        let op = b.get_u8();
        let klen = b.get_u16() as usize;
        need(&b, klen + 8)?;
        let key = String::from_utf8(b.split_to(klen).to_vec()).ok()?;
        let n = b.get_u64();
        Some(match op {
            1 => {
                need(&b, 4)?;
                let count = b.get_u32() as usize;
                let mut entries = Vec::with_capacity(count.min(1 << 16));
                for _ in 0..count {
                    need(&b, 4)?;
                    let len = b.get_u32() as usize;
                    need(&b, len)?;
                    entries.push(b.split_to(len));
                }
                WalRecord::Append { key, first: n, entries }
            }
            2 => WalRecord::Truncate { key, before: n },
            3 => WalRecord::Destroy { key },
            _ => return None,
        })
    }
}

#[async_trait]
impl Log for FileLog {
    async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        self.write(entries, true).await
    }

    async fn append_relaxed(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        self.write(entries, false).await
    }

    async fn append_pipelined(&self, entries: Vec<Bytes>) -> anyhow::Result<(u64, Durable)> {
        self.start_write(entries, true).await
    }

    async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>> {
        self.blocking(move |i| {
            let mut out = Vec::new();
            let mut bytes = 0usize;
            let first = i.segments.first().map(|s| s.first).unwrap_or(i.next);
            let mut idx = from.max(first);
            for seg in &i.segments {
                let end = seg.first + seg.offsets.len() as u64;
                if idx >= end || idx < seg.first {
                    continue;
                }
                // Work out the exact byte range from the offset index, then read it in one go.
                let start = (idx - seg.first) as usize;
                let entry_end = |j: usize| seg.offsets.get(j + 1).copied().unwrap_or(seg.len);
                let mut stop = start;
                while stop < seg.offsets.len() && out.len() + (stop - start) < max {
                    let len = (entry_end(stop) - seg.offsets[stop] - 8) as usize;
                    if bytes > 0 && bytes + len > max_bytes {
                        break;
                    }
                    bytes += len;
                    stop += 1;
                }
                if stop == start {
                    break;
                }
                let lo = seg.offsets[start];
                let hi = entry_end(stop - 1);
                let mut f = File::open(&seg.path)?;
                f.seek(SeekFrom::Start(lo))?;
                let mut data = vec![0u8; (hi - lo) as usize];
                f.read_exact(&mut data)?;
                let mut data = Bytes::from(data);
                for _ in start..stop {
                    let len = (&data[..4]).get_u32() as usize;
                    out.push((idx, data.slice(8..8 + len)));
                    data.advance(8 + len);
                    idx += 1;
                }
                if out.len() >= max || stop < seg.offsets.len() {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    fn bounds(&self) -> (u64, u64) {
        let i = self.inner.lock();
        (i.segments.first().map(|s| s.first).unwrap_or(i.next), i.next)
    }

    async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64> {
        let rec = {
            let i = self.inner.lock();
            if !i.prunable(before) {
                return Ok(i.segments.first().map(|s| s.first).unwrap_or(i.next));
            }
            i.wal.as_ref().map(|(_, key)| WalRecord::encode(2, key, before))
        };
        // Durable before files go, so a replay never resurrects what they held.
        self.mark(rec, true).await?;
        self.blocking(move |i| {
            while i.segments.len() > 1 {
                let s = &i.segments[0];
                let end = i.segments[1].first;
                if end > before {
                    break;
                }
                std::fs::remove_file(&s.path).ok();
                i.bytes -= s.len;
                i.segments.remove(0);
            }
            // A fully consumed single segment: start a fresh one.
            if i.prunable(before) {
                let s = i.segments.remove(0);
                std::fs::remove_file(&s.path).ok();
                i.bytes -= s.len;
                // Everything in it was consumed; no need to sync it.
                i.active = None;
                i.dirty = false;
            }
            Ok(i.segments.first().map(|s| s.first).unwrap_or(i.next))
        })
        .await
    }

    fn bytes(&self) -> u64 {
        self.inner.lock().bytes
    }

    async fn destroy(&self) -> anyhow::Result<()> {
        self.blocking(|i| {
            i.active = None;
            i.dirty = false;
            i.dir_dirty = false;
            i.unsynced.clear();
            i.segments.clear();
            i.bytes = 0;
            std::fs::remove_dir_all(&i.dir).ok();
            Ok(())
        })
        .await?;
        // Durable, so a replay never brings a deleted log back.
        let rec = self.inner.lock().wal.as_ref().map(|(_, key)| WalRecord::encode(3, key, 0));
        self.mark(rec, true).await
    }

    async fn roll(&self) -> anyhow::Result<()> {
        self.blocking(|i| {
            if i.segments.last().is_some_and(|s| s.len > 0) {
                let next = i.next;
                let path = i.dir.join(format!("{next:020}.seg"));
                OpenOptions::new().create(true).append(true).open(&path)?;
                i.segments.push(Segment { first: next, path, offsets: Vec::new(), len: 0 });
                i.close_active()?;
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_is_the_ieee_crc32_existing_logs_were_written_with() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        // The byte-at-a-time table the format was first written with.
        let table_crc = |data: &[u8]| {
            let mut c = !0u32;
            for b in data {
                c ^= *b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        };
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect();
        assert_eq!(crc32(&data), table_crc(&data));
    }

    async fn exercise(log: Arc<dyn Log>) {
        let a = log.append((0..100).map(|i| Bytes::from(format!("e{i}"))).collect()).await.unwrap();
        assert_eq!(a, 0);
        let b = log.append(vec![Bytes::from_static(b"x")]).await.unwrap();
        assert_eq!(b, 100);
        let r = log.read(98, 10, 1 << 20).await.unwrap();
        assert_eq!(r.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [98, 99, 100]);
        assert_eq!(&r[2].1[..], b"x");
        assert_eq!(log.bounds(), (0, 101));
    }

    #[tokio::test]
    async fn mem_log() {
        exercise(Arc::new(MemLog::new(10))).await;
    }

    #[tokio::test]
    async fn file_log_survives_reopen_and_truncates() {
        let dir = std::env::temp_dir().join(format!("mq-log-test-{}", crate::rand_u64()));
        let store = FileStore::with(&dir, 256, true);
        let log = store.open("q/%2F/test/1", 0).await.unwrap();
        exercise(log.clone()).await;
        drop(log);
        let log = store.open("q/%2F/test/1", 0).await.unwrap();
        assert_eq!(log.bounds(), (0, 101));
        let r = log.read(0, 1000, 1 << 20).await.unwrap();
        assert_eq!(r.len(), 101);
        assert_eq!(&r[57].1[..], b"e57");
        let first = log.truncate_prefix(60).await.unwrap();
        assert!(first > 0 && first <= 60, "{first}");
        assert_eq!(log.read(0, 1, 1 << 20).await.unwrap()[0].0, first);
        // Torn tail.
        let seg = std::fs::read_dir(store.path_for("q/%2F/test/1")).unwrap().map(|e| e.unwrap().path()).max().unwrap();
        let mut f = OpenOptions::new().append(true).open(&seg).unwrap();
        f.write_all(&[0, 0, 0, 9, 1, 2]).unwrap();
        drop(f);
        drop(log);
        let log = store.open("q/%2F/test/1", 0).await.unwrap();
        assert_eq!(log.bounds().1, 101);
        log.destroy().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn relaxed_appends_interleave_in_order_across_segments() {
        let dir = std::env::temp_dir().join(format!("mq-log-test-{}", crate::rand_u64()));
        let store = FileStore::with(&dir, 64, true);
        let log = store.open("q/%2F/relaxed/1", 0).await.unwrap();
        for i in 0..40u64 {
            let e = vec![Bytes::from(format!("r{i}"))];
            let at = if i % 3 == 0 { log.append(e).await } else { log.append_relaxed(e).await };
            assert_eq!(at.unwrap(), i);
        }
        log.roll().await.unwrap();
        drop(log);
        let log = store.open("q/%2F/relaxed/1", 0).await.unwrap();
        let r = log.read(0, 100, 1 << 20).await.unwrap();
        assert_eq!(r.len(), 40);
        assert!(r.iter().enumerate().all(|(i, (idx, b))| *idx == i as u64 && b[..] == *format!("r{i}").as_bytes()));
        log.destroy().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn full_segments_sync_at_the_checkpoint_not_in_the_append() {
        let dir = std::env::temp_dir().join(format!("mq-log-test-{}", crate::rand_u64()));
        let (writer, _, _) = WalWriter::open(&dir.join(".wal")).unwrap();
        let wal = Wal::start(writer, u64::MAX).unwrap();
        let log = FileLog::open(&dir.join("q"), 64, true).unwrap();
        log.inner.lock().wal = Some((wal.clone(), "q".into()));
        for i in 0..20u64 {
            log.append(vec![Bytes::from(format!("entry-{i:04}"))]).await.unwrap();
        }
        {
            let i = log.inner.lock();
            assert!(i.unsynced.len() >= 3, "closed segments wait for the checkpoint: {}", i.unsynced.len());
            assert!(i.dir_dirty);
        }
        log.inner.checkpoint().unwrap();
        {
            let i = log.inner.lock();
            assert!(i.unsynced.is_empty() && !i.dirty && !i.dir_dirty);
        }
        // Appends after the checkpoint are dirty again until the next one.
        log.append(vec![Bytes::from_static(b"after")]).await.unwrap();
        assert!(log.inner.lock().dirty);
        assert_eq!(log.bounds(), (0, 21));
        drop(log);
        wal.stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn wal_store_recovers_writes_its_segments_lost() {
        let dir = std::env::temp_dir().join(format!("mq-log-test-{}", crate::rand_u64()));
        let store = FileStore::with_wal(&dir, 128).unwrap();
        let log = store.open("q/%2F/wal/1", 0).await.unwrap();
        for i in 0..30u64 {
            let e = vec![Bytes::from(format!("w{i}"))];
            let at = if i % 4 == 3 { log.append_relaxed(e).await } else { log.append(e).await };
            assert_eq!(at.unwrap(), i);
        }
        log.truncate_prefix(5).await.unwrap();
        let gone = store.open("q/%2F/gone/1", 0).await.unwrap();
        gone.append(vec![Bytes::from_static(b"x")]).await.unwrap();
        gone.destroy().await.unwrap();
        log.append(vec![Bytes::from_static(b"last")]).await.unwrap();
        store.wal.as_ref().unwrap().crash();
        drop((log, gone));
        // Lose the unsynced tail of the active segment, as a power cut could.
        let segs = store.path_for("q/%2F/wal/1");
        let last = std::fs::read_dir(&segs).unwrap().map(|e| e.unwrap().path()).max().unwrap();
        let len = std::fs::metadata(&last).unwrap().len();
        OpenOptions::new().write(true).open(&last).unwrap().set_len(len / 2).unwrap();
        drop(store);

        let store = FileStore::with_wal(&dir, 128).unwrap();
        let log = store.open("q/%2F/wal/1", 0).await.unwrap();
        let (first, next) = log.bounds();
        assert_eq!(next, 31);
        let r = log.read(first, 100, 1 << 20).await.unwrap();
        assert!(r.iter().all(|(i, b)| *i == 30 && &b[..] == b"last" || b[..] == *format!("w{i}").as_bytes()), "{r:?}");
        assert_eq!(r.len() as u64, next - first);
        assert!(!store.path_for("q/%2F/gone/1").exists(), "a destroyed log stays destroyed");
        store.wal.as_ref().unwrap().stop();
        let left: Vec<_> = std::fs::read_dir(dir.join(".wal")).unwrap().collect();
        assert!(left.is_empty(), "a clean stop leaves no WAL behind");
        std::fs::remove_dir_all(&dir).ok();
    }
}

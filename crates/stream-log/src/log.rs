//! One partition's log.
//!
//! Local disk is the write path and a cache: batches are appended to segment files
//! (`<base offset>.log`, Kafka's on-disk format), fsynced by group commit, and uploaded to object
//! storage as chunks of whole batches. Uploaded segments are dropped locally once they fall out of
//! the local retention window; reads below the local range are served from object storage.
//!
//! The high watermark (what consumers may read) is the durable end of the log: data is visible
//! only once it's fsynced, so a consumer never sees records a crash could lose.

use crate::batch::{self, BatchError, Header};
use crate::producer::{AbortedTxn, ProducerStates, SeqCheck};
use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use thiserror::Error;
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub struct LogConfig {
    /// Roll to a new segment past this size…
    pub segment_bytes: u64,
    /// …or when the active segment is older than this (ms).
    pub segment_ms: i64,
    /// A sparse index entry every this many bytes.
    pub index_interval_bytes: u64,
    /// fsync before acknowledging `acks=all` and before data becomes visible. Without it the
    /// high watermark follows the log end immediately.
    pub fsync: bool,
    /// Recent batches kept in memory so tailing consumers never touch the disk…
    pub tail_cache_bytes: usize,
    /// …within a budget shared by every open log (each gets at most its fair share).
    pub tail_budget: Option<Arc<TailBudget>>,
    pub leader_epoch: i32,
}

/// Memory for tail caches shared by many logs, so thousands of open partitions can't add up to
/// gigabytes. Each log caches at most `limit / open logs`.
#[derive(Debug)]
pub struct TailBudget {
    limit: usize,
    used: AtomicUsize,
    open: AtomicUsize,
}

impl TailBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { limit, used: AtomicUsize::new(0), open: AtomicUsize::new(0) })
    }

    /// Bytes cached across all logs.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// Logs currently open.
    pub fn open_logs(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }

    fn share(&self) -> usize {
        self.limit / self.open.load(Ordering::Relaxed).max(1)
    }

    fn over(&self) -> bool {
        self.used() > self.limit
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            segment_bytes: 64 << 20,
            segment_ms: 7 * 86_400_000,
            index_interval_bytes: 4096,
            fsync: true,
            tail_cache_bytes: 4 << 20,
            tail_budget: None,
            leader_epoch: 0,
        }
    }
}

/// A run of batches in object storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub first: i64,
    pub last: i64,
    pub max_timestamp: i64,
    pub bytes: u64,
    pub key: String,
}

/// State kept outside the log (in the broker's catalog) and handed back on open.
#[derive(Clone, Debug, Default)]
pub struct Recovered {
    pub log_start: i64,
    pub chunks: Vec<Chunk>,
    pub aborted: Vec<AbortedTxn>,
}

#[derive(Debug, Error)]
pub enum AppendError {
    #[error(transparent)]
    Corrupt(#[from] BatchError),
    #[error("batch of {0} bytes exceeds the maximum message size")]
    TooLarge(usize),
    #[error("producer epoch is older than the current one")]
    InvalidEpoch,
    #[error("out of order sequence number")]
    OutOfOrder,
    #[error("unknown producer")]
    UnknownProducer,
    #[error("timestamp out of the allowed range")]
    InvalidTimestamp,
    #[error("{0}")]
    InvalidRecord(&'static str),
    #[error("storage error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendOptions {
    pub now_ms: i64,
    /// `message.timestamp.type=LogAppendTime`.
    pub log_append_time: bool,
    pub max_batch_bytes: usize,
    /// Reject batches with timestamps more than this far before/after now (ms).
    pub max_timestamp_before_ms: Option<i64>,
    pub max_timestamp_after_ms: Option<i64>,
    /// Allow control batches (only the transaction coordinator writes them).
    pub allow_control: bool,
}

impl AppendOptions {
    pub fn at(now_ms: i64) -> Self {
        Self {
            now_ms,
            log_append_time: false,
            max_batch_bytes: usize::MAX,
            max_timestamp_before_ms: None,
            max_timestamp_after_ms: None,
            allow_control: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appended {
    pub base_offset: i64,
    pub last_offset: i64,
    /// Broker time when `LogAppendTime` is in effect, else -1.
    pub log_append_time: i64,
    pub log_start: i64,
    /// The batch was a retry of one already in the log.
    pub duplicate: bool,
    pub bytes: usize,
    pub records: i64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReadError {
    #[error("offset {offset} is out of range [{start}, {end}]")]
    OutOfRange { offset: i64, start: i64, end: i64 },
}

/// Where the data for a read lives.
#[derive(Clone, Debug)]
pub enum ReadPlan {
    /// Ready to return (possibly empty).
    Ready(Bytes),
    /// A byte range of a local segment; finish with [`read_local`].
    Local(LocalRead),
    /// An object; download it and finish with [`batch::slice_for_fetch`].
    Remote(Chunk),
}

#[derive(Clone, Debug)]
pub struct LocalRead {
    pub file: Arc<File>,
    pub pos: u64,
    pub end: u64,
}

#[derive(Clone, Debug)]
pub struct Read {
    pub plan: ReadPlan,
    pub offset: i64,
    /// Exclusive bound for returned batches (high watermark or last stable offset).
    pub upper: i64,
    pub max_bytes: usize,
    pub high_watermark: i64,
    pub last_stable: i64,
    pub log_start: i64,
}

#[derive(Clone, Debug)]
pub struct UploadPlan {
    pub file: Arc<File>,
    pub segment: i64,
    pub pos: u64,
    pub len: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Retention {
    /// -1 for unlimited.
    pub retention_ms: i64,
    pub retention_bytes: i64,
    /// Local copies of uploaded data are kept within these (-1 = keep as long as retained).
    pub local_retention_ms: i64,
    pub local_retention_bytes: i64,
}

/// A compaction pass over the sealed, uploaded, stable prefix of the log.
#[derive(Clone, Debug)]
pub struct CompactionPlan {
    /// Chunks to rewrite, in offset order; they cover `[log_start, end)` (with gaps where earlier
    /// passes removed everything).
    pub chunks: Vec<Chunk>,
    pub log_start: i64,
    pub end: i64,
    /// Offsets from here on haven't been compacted yet.
    pub dirty_from: i64,
    pub aborted: Vec<AbortedTxn>,
}

#[derive(Clone, Copy, Debug)]
pub struct CompactionPolicy {
    /// Records newer than this (ms) are left alone.
    pub min_lag_ms: i64,
    /// Compact once the oldest dirty data is this old, whatever the ratio.
    pub max_lag_ms: i64,
    /// Compact once dirty bytes / cleanable bytes reach this.
    pub dirty_ratio: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Cleaned {
    pub log_start: i64,
    pub removed_chunks: Vec<Chunk>,
    pub removed_segments: usize,
    pub start_moved: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LogStats {
    pub log_start: i64,
    pub high_watermark: i64,
    pub log_end: i64,
    pub last_stable: i64,
    pub local_bytes: u64,
    pub remote_bytes: u64,
    pub pending_upload_bytes: u64,
    pub segments: usize,
    pub chunks: usize,
    pub producers: usize,
    pub max_timestamp: i64,
}

struct Segment {
    base: i64,
    path: PathBuf,
    file: Arc<File>,
    size: u64,
    /// Bytes known to be on stable storage.
    synced: u64,
    /// Bytes already in object storage.
    uploaded: u64,
    /// When the oldest byte not yet uploaded was appended.
    pending_since_ms: i64,
    /// One past the last offset in the segment.
    next: i64,
    max_timestamp: i64,
    created_ms: i64,
    /// (batch base offset, file position), sparse.
    index: Vec<(i64, u64)>,
    /// (largest timestamp so far, batch base offset), added when the timestamp grows.
    time_index: Vec<(i64, i64)>,
    since_index: u64,
}

impl Segment {
    fn file_name(base: i64) -> String {
        format!("{base:020}.log")
    }

    fn create(dir: &Path, base: i64, now_ms: i64) -> io::Result<Self> {
        let path = dir.join(Self::file_name(base));
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
        Ok(Self {
            base,
            path,
            file: Arc::new(file),
            size: 0,
            synced: 0,
            uploaded: 0,
            pending_since_ms: now_ms,
            next: base,
            max_timestamp: -1,
            created_ms: now_ms,
            index: Vec::new(),
            time_index: Vec::new(),
            since_index: u64::MAX,
        })
    }

    fn track(&mut self, h: &Header, pos: u64, interval: u64) {
        if self.since_index >= interval {
            self.index.push((h.base_offset, pos));
            self.since_index = 0;
        }
        self.since_index += h.size as u64;
        if h.max_timestamp > self.max_timestamp {
            self.max_timestamp = h.max_timestamp;
            self.time_index.push((h.max_timestamp, h.base_offset));
        }
        self.next = h.next_offset();
    }

    /// File position to start scanning from for `offset`.
    fn floor_pos(&self, offset: i64) -> u64 {
        match self.index.partition_point(|(o, _)| *o <= offset) {
            0 => 0,
            i => self.index[i - 1].1,
        }
    }

    fn readable(&self, fsync: bool) -> u64 {
        if fsync { self.synced } else { self.size }
    }
}

struct Cached {
    base: i64,
    last: i64,
    data: Bytes,
}

struct Inner {
    config: LogConfig,
    log_start: i64,
    /// Next offset to assign.
    leo: i64,
    high_watermark: i64,
    segments: Vec<Segment>,
    chunks: Vec<Chunk>,
    /// Offsets below this are in object storage.
    uploaded_end: i64,
    tail: VecDeque<Cached>,
    tail_bytes: usize,
    producers: ProducerStates,
    aborted: Vec<AbortedTxn>,
    /// Offsets below this have been compacted.
    cleaned_to: i64,
}

pub struct Log {
    dir: PathBuf,
    inner: Mutex<Inner>,
    hw: watch::Sender<i64>,
    /// Serialises flushes so their completion order matches their snapshot order.
    flush_lock: Mutex<()>,
}

impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Log").field("dir", &self.dir).finish()
    }
}

/// Written when a log closes cleanly (everything fsynced): segment sizes that need no validation
/// on the next open.
const CLEAN_MARKER: &str = "clean";

impl Drop for Log {
    fn drop(&mut self) {
        let g = self.lock();
        if let Some(b) = &g.config.tail_budget {
            b.used.fetch_sub(g.tail_bytes, Ordering::Relaxed);
            b.open.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn read_clean_marker(dir: &Path) -> std::collections::HashMap<i64, u64> {
    let path = dir.join(CLEAN_MARKER);
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    // One use only: appends after this open change the files.
    let _ = std::fs::remove_file(&path);
    text.lines()
        .filter_map(|l| {
            let (base, size) = l.split_once(' ')?;
            Some((base.parse().ok()?, size.parse().ok()?))
        })
        .collect()
}

fn parse_segment_name(name: &str) -> Option<i64> {
    name.strip_suffix(".log")?.parse().ok()
}

/// Read a local range, returning the batches a fetch should get (see [`batch::slice_for_fetch`]).
/// Reads `max_bytes + slack` at a time (the sparse index puts the start up to an index interval
/// before the target).
pub fn read_local(r: &LocalRead, offset: i64, upper: i64, max_bytes: usize, slack: u64) -> io::Result<Bytes> {
    let mut pos = r.pos;
    loop {
        if pos >= r.end {
            return Ok(Bytes::new());
        }
        let want = (max_bytes as u64).saturating_add(slack).min(r.end - pos);
        let buf = read_at(&r.file, pos, want)?;
        let mut at = 0;
        let found = loop {
            match batch::parse_header(&buf[at..]) {
                Ok(h) if h.base_offset >= upper => return Ok(Bytes::new()),
                Ok(h) if h.last_offset() >= offset => break Some(at),
                Ok(h) => at += h.size,
                Err(BatchError::Truncated) => break None,
                Err(e) => return Err(io::Error::other(e)),
            }
        };
        if let Some(at) = found {
            return Ok(batch::slice_for_fetch(&buf.slice(at..), offset, upper, max_bytes));
        }
        if at > 0 {
            pos += at as u64;
            continue;
        }
        // A single batch larger than the read: fetch it whole.
        let prefix = read_at(&r.file, pos, batch::LOG_OVERHEAD as u64)?;
        let size = batch::batch_size(&prefix).ok_or_else(|| io::Error::other(BatchError::Truncated))?;
        let whole = read_at(&r.file, pos, size as u64)?;
        let h = batch::parse_header(&whole).map_err(io::Error::other)?;
        if h.base_offset >= upper {
            return Ok(Bytes::new());
        }
        if h.last_offset() >= offset {
            return Ok(whole);
        }
        pos += size as u64;
    }
}

fn read_at(file: &File, pos: u64, len: u64) -> io::Result<Bytes> {
    let mut buf = BytesMut::zeroed(len as usize);
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(&mut buf[done..], pos + done as u64)?;
        if n == 0 {
            buf.truncate(done);
            break;
        }
        done += n;
    }
    Ok(buf.freeze())
}

impl Log {
    /// Open (or create) the log in `dir`, recovering local segments: the last one is validated
    /// batch by batch and truncated after the last intact batch.
    pub fn open(dir: impl Into<PathBuf>, config: LogConfig, recovered: Recovered, now_ms: i64) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut chunks = recovered.chunks;
        chunks.sort_by_key(|c| c.first);
        let uploaded_end = chunks.last().map(|c| c.last + 1).unwrap_or(0).max(recovered.log_start);

        let mut bases: Vec<i64> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| parse_segment_name(&e.file_name().to_string_lossy()))
            .collect();
        bases.sort();

        let clean = read_clean_marker(&dir);
        let mut producers = ProducerStates::default();
        let mut segments: Vec<Segment> = Vec::new();
        let n = bases.len();
        for (i, base) in bases.into_iter().enumerate() {
            let path = dir.join(Segment::file_name(base));
            // Only the newest segments can hold writes that weren't synced before a crash, and not
            // even those if the log was closed cleanly and the files are as it left them.
            let len = std::fs::metadata(&path).map(|m| m.len()).ok();
            let validate = i + 2 >= n && (len.is_none() || clean.get(&base).copied() != len);
            let expected = segments.last().map(|s| s.next);
            if expected.is_some_and(|e| e != base) {
                warn!(path = %path.display(), "gap in local segments; dropping the rest");
                remove_from(&dir, base)?;
                break;
            }
            let seg = Self::recover_segment(&path, base, validate, &config, uploaded_end, &mut producers, now_ms)?;
            if seg.size > 0 || i + 1 == n {
                segments.push(seg);
            } else {
                let _ = std::fs::remove_file(&seg.path);
            }
        }
        // Local data entirely below the retained range is garbage.
        while segments.len() > 1 && segments[0].next <= recovered.log_start {
            let s = segments.remove(0);
            let _ = std::fs::remove_file(&s.path);
        }

        let local_end = segments.last().map(|s| s.next);
        let leo = local_end.unwrap_or(0).max(uploaded_end).max(recovered.log_start);
        if segments.last().is_none_or(|s| s.next != leo) {
            // No usable local tail (fresh host, or local data behind object storage): start a new
            // segment at the log end.
            if segments.last().is_some_and(|s| s.next < leo) {
                for s in segments.drain(..) {
                    let _ = std::fs::remove_file(&s.path);
                }
            }
            segments.push(Segment::create(&dir, leo, now_ms)?);
        }
        // The catalog's log start is authoritative: compaction can leave the first offsets
        // without data, and they're still part of the log.
        let log_start = recovered.log_start.min(leo);

        let mut aborted = recovered.aborted;
        aborted.sort_by_key(|a| a.first_offset);
        let hw = leo;
        let inner = Inner {
            config,
            log_start,
            leo,
            high_watermark: hw,
            segments,
            chunks,
            uploaded_end,
            tail: VecDeque::new(),
            tail_bytes: 0,
            producers,
            aborted,
            cleaned_to: 0,
        };
        if let Some(b) = &inner.config.tail_budget {
            b.open.fetch_add(1, Ordering::Relaxed);
        }
        let (hw_tx, _) = watch::channel(hw);
        Ok(Self { dir, inner: Mutex::new(inner), hw: hw_tx, flush_lock: Mutex::new(()) })
    }

    fn recover_segment(
        path: &Path,
        base: i64,
        validate: bool,
        config: &LogConfig,
        uploaded_end: i64,
        producers: &mut ProducerStates,
        now_ms: i64,
    ) -> io::Result<Segment> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        let mut seg = Segment {
            base,
            path: path.to_path_buf(),
            file: Arc::new(file),
            size: 0,
            synced: 0,
            uploaded: 0,
            pending_since_ms: now_ms,
            next: base,
            max_timestamp: -1,
            created_ms: now_ms,
            index: Vec::new(),
            time_index: Vec::new(),
            since_index: u64::MAX,
        };
        let mut pos = 0u64;
        let mut uploaded = None;
        while pos < len {
            let prefix = read_at(&seg.file, pos, batch::HEADER_LEN as u64)?;
            let Some(size) = batch::batch_size(&prefix) else { break };
            if pos + size as u64 > len || prefix.len() < batch::HEADER_LEN {
                break;
            }
            let h = if validate {
                let whole = read_at(&seg.file, pos, size as u64)?;
                match batch::validate(&whole) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(path = %path.display(), pos, error = %e, "corrupt batch; truncating");
                        break;
                    }
                }
            } else {
                // Trusted data: the header is enough (no need to read the records).
                match batch::parse_header_prefix(&prefix) {
                    Ok(h) if h.size == size => h,
                    _ => break,
                }
            };
            if h.base_offset != seg.next {
                warn!(path = %path.display(), pos, "offset discontinuity; truncating");
                break;
            }
            if uploaded.is_none() && h.base_offset >= uploaded_end {
                uploaded = Some(pos);
            }
            seg.track(&h, pos, config.index_interval_bytes);
            producers.update(&h);
            if h.is_control()
                && let Ok(whole) = read_at(&seg.file, pos, size as u64)
                && let Some(commit) = batch::control_commits(&whole)
            {
                producers.end_txn(&h, commit, 0);
            }
            pos += size as u64;
        }
        if pos < len {
            info!(path = %path.display(), from = len, to = pos, "truncating segment");
            seg.file.set_len(pos)?;
            seg.file.sync_all()?;
        }
        seg.size = pos;
        seg.synced = pos;
        seg.uploaded = uploaded.unwrap_or(pos);
        Ok(seg)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Apply changed settings (segment size and age, index interval, cache size).
    pub fn set_config(&self, config: LogConfig) {
        let mut g = self.lock();
        let fsync = g.config.fsync;
        let tail_budget = g.config.tail_budget.clone();
        g.config = LogConfig { fsync, tail_budget, ..config };
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Watch the high watermark.
    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.hw.subscribe()
    }

    pub fn high_watermark(&self) -> i64 {
        self.lock().high_watermark
    }

    pub fn log_end(&self) -> i64 {
        self.lock().leo
    }

    pub fn log_start(&self) -> i64 {
        self.lock().log_start
    }

    pub fn last_stable(&self) -> i64 {
        let g = self.lock();
        g.last_stable()
    }

    pub fn stats(&self) -> LogStats {
        let g = self.lock();
        let local: u64 = g.segments.iter().map(|s| s.size).sum();
        LogStats {
            log_start: g.log_start,
            high_watermark: g.high_watermark,
            log_end: g.leo,
            last_stable: g.last_stable(),
            local_bytes: local,
            remote_bytes: g.chunks.iter().map(|c| c.bytes).sum(),
            pending_upload_bytes: g.segments.iter().map(|s| s.size - s.uploaded).sum(),
            segments: g.segments.len(),
            chunks: g.chunks.len(),
            producers: g.producers.len(),
            max_timestamp: g
                .segments
                .iter()
                .map(|s| s.max_timestamp)
                .chain(g.chunks.iter().map(|c| c.max_timestamp))
                .max()
                .unwrap_or(-1),
        }
    }

    /// Append batches from a producer. Offsets are assigned in order; the result is visible to
    /// consumers once flushed (see [`Log::flush`]).
    pub fn append(&self, batches: &[Bytes], opts: AppendOptions) -> Result<Appended, AppendError> {
        if batches.is_empty() {
            return Err(AppendError::InvalidRecord("no record batches"));
        }
        let mut headers = Vec::with_capacity(batches.len());
        for b in batches {
            if b.len() > opts.max_batch_bytes {
                return Err(AppendError::TooLarge(b.len()));
            }
            let h = batch::validate(b)?;
            if h.is_control() && !opts.allow_control {
                return Err(AppendError::InvalidRecord("control batches can't be produced by clients"));
            }
            if !opts.log_append_time && !h.is_control() && h.max_timestamp >= 0
                && (opts.max_timestamp_before_ms.is_some_and(|d| h.max_timestamp < opts.now_ms - d)
                    || opts.max_timestamp_after_ms.is_some_and(|d| h.max_timestamp > opts.now_ms + d))
                {
                    return Err(AppendError::InvalidTimestamp);
                }
            headers.push(h);
        }

        let mut g = self.lock();
        // Idempotence: a request that only retries batches already in the log is answered with
        // their original offsets.
        for h in &headers {
            match g.producers.check(h) {
                SeqCheck::Append => {}
                SeqCheck::Duplicate(meta) if headers.len() == 1 => {
                    return Ok(Appended {
                        base_offset: meta.first_offset,
                        last_offset: meta.last_offset,
                        log_append_time: -1,
                        log_start: g.log_start,
                        duplicate: true,
                        bytes: 0,
                        records: 0,
                    });
                }
                SeqCheck::Duplicate(_) | SeqCheck::OutOfOrder => return Err(AppendError::OutOfOrder),
                SeqCheck::InvalidEpoch => return Err(AppendError::InvalidEpoch),
                SeqCheck::UnknownProducer => return Err(AppendError::UnknownProducer),
            }
        }

        let first = g.leo;
        let mut bytes = 0;
        let mut records = 0i64;
        for (b, h) in batches.iter().zip(headers.iter_mut()) {
            let mut m = BytesMut::from(&b[..]);
            batch::assign_offset(&mut m, g.leo, g.config.leader_epoch);
            if opts.log_append_time && !h.is_control() {
                batch::set_log_append_time(&mut m, opts.now_ms);
                h.max_timestamp = opts.now_ms;
            }
            h.base_offset = g.leo;
            h.leader_epoch = g.config.leader_epoch;
            let data = m.freeze();
            g.write(&self.dir, h, data, opts.now_ms)?;
            bytes += h.size;
            records += h.last_offset_delta as i64 + 1;
            g.producers.update(h);
        }
        let last = g.leo - 1;
        let log_start = g.log_start;
        if !g.config.fsync {
            g.high_watermark = g.leo;
            let hw = g.high_watermark;
            drop(g);
            self.hw.send_replace(hw);
        }
        Ok(Appended {
            base_offset: first,
            last_offset: last,
            log_append_time: if opts.log_append_time { opts.now_ms } else { -1 },
            log_start,
            duplicate: false,
            bytes,
            records,
        })
    }

    /// Write a transaction marker for `producer_id` and resolve its open transaction here.
    /// Returns the marker offset and the aborted range (for aborts of non-empty transactions).
    pub fn append_marker(
        &self,
        producer_id: i64,
        producer_epoch: i16,
        commit: bool,
        coordinator_epoch: i32,
        now_ms: i64,
    ) -> Result<(i64, Option<AbortedTxn>), AppendError> {
        let data = batch::control_batch(producer_id, producer_epoch, commit, coordinator_epoch, now_ms);
        let mut g = self.lock();
        if g.producers.get(producer_id).is_some_and(|e| e.epoch > producer_epoch) {
            return Err(AppendError::InvalidEpoch);
        }
        let mut m = BytesMut::from(&data[..]);
        let offset = g.leo;
        batch::assign_offset(&mut m, offset, g.config.leader_epoch);
        let mut h = batch::parse_header(&m)?;
        g.write(&self.dir, &mut h, m.freeze(), now_ms)?;
        let aborted = g.producers.end_txn(&h, commit, coordinator_epoch);
        if let Some(a) = aborted {
            g.aborted.push(a);
        }
        if !g.config.fsync {
            g.high_watermark = g.leo;
            let hw = g.high_watermark;
            drop(g);
            self.hw.send_replace(hw);
        }
        Ok((offset, aborted))
    }

    /// Whether `producer_id` has an open transaction on this partition.
    pub fn has_open_txn(&self, producer_id: i64) -> bool {
        self.lock().producers.get(producer_id).is_some_and(|e| e.txn_first_offset.is_some())
    }

    /// Open transactions as `(producer id, first offset)`.
    pub fn open_txns(&self) -> Vec<(i64, i64)> {
        self.lock().producers.ongoing().collect()
    }

    /// Producers with state on this partition: `(id, epoch, last sequence, last timestamp,
    /// coordinator epoch, open transaction's first offset)`.
    pub fn producers(&self) -> Vec<(i64, i16, i32, i64, i32, Option<i64>)> {
        let g = self.lock();
        g.producers
            .iter()
            .map(|(id, e)| {
                let last_seq = e.batches.back().map(|b| b.last_seq).unwrap_or(-1);
                (*id, e.epoch, last_seq, e.last_timestamp, e.coordinator_epoch, e.txn_first_offset)
            })
            .collect()
    }

    pub fn expire_producers(&self, cutoff_ms: i64) -> usize {
        self.lock().producers.expire(cutoff_ms)
    }

    /// Whether data is waiting to be made durable.
    pub fn needs_flush(&self) -> bool {
        let g = self.lock();
        g.config.fsync && g.high_watermark < g.leo
    }

    /// fsync everything appended so far and advance the high watermark. Blocking; concurrent
    /// appends continue while the sync runs.
    pub fn flush(&self) -> io::Result<i64> {
        let _serial = self.flush_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (files, target) = {
            let g = self.lock();
            if !g.config.fsync || g.high_watermark >= g.leo {
                return Ok(g.high_watermark);
            }
            let files: Vec<(i64, Arc<File>, u64)> =
                g.segments.iter().filter(|s| s.synced < s.size).map(|s| (s.base, s.file.clone(), s.size)).collect();
            (files, g.leo)
        };
        for (_, f, _) in &files {
            f.sync_data()?;
        }
        let hw = {
            let mut g = self.lock();
            for (base, _, size) in &files {
                if let Some(s) = g.segments.iter_mut().find(|s| s.base == *base) {
                    s.synced = s.synced.max(*size);
                }
            }
            g.high_watermark = g.high_watermark.max(target);
            g.high_watermark
        };
        self.hw.send_replace(hw);
        Ok(hw)
    }

    /// Plan a read at `offset`. `committed` limits it to the last stable offset (read_committed).
    pub fn read(&self, offset: i64, max_bytes: usize, committed: bool) -> Result<Read, ReadError> {
        let g = self.lock();
        let lso = g.last_stable();
        let upper = if committed { lso } else { g.high_watermark };
        if offset < g.log_start || offset > g.leo {
            return Err(ReadError::OutOfRange { offset, start: g.log_start, end: g.leo });
        }
        let base = Read {
            plan: ReadPlan::Ready(Bytes::new()),
            offset,
            upper,
            max_bytes,
            high_watermark: g.high_watermark,
            last_stable: lso,
            log_start: g.log_start,
        };
        if offset >= upper {
            return Ok(base);
        }
        // Hot path: recent batches in memory.
        if let Some(front) = g.tail.front()
            && offset >= front.base
        {
            let i = g.tail.partition_point(|c| c.last < offset);
            let mut parts = Vec::new();
            let mut total = 0;
            for c in g.tail.iter().skip(i) {
                if c.base >= upper || (total > 0 && total + c.data.len() > max_bytes) {
                    break;
                }
                total += c.data.len();
                parts.push(c.data.clone());
            }
            let data = match parts.len() {
                0 => Bytes::new(),
                1 => parts.pop().unwrap(),
                _ => {
                    let mut b = BytesMut::with_capacity(total);
                    for p in parts {
                        b.extend_from_slice(&p);
                    }
                    b.freeze()
                }
            };
            return Ok(Read { plan: ReadPlan::Ready(data), ..base });
        }
        let fsync = g.config.fsync;
        if let Some(seg) = g.segments.iter().rev().find(|s| s.base <= offset && s.size > 0)
            && offset < seg.next
        {
            let plan = ReadPlan::Local(LocalRead { file: seg.file.clone(), pos: seg.floor_pos(offset), end: seg.readable(fsync) });
            return Ok(Read { plan, ..base });
        }
        if let Some(c) = g.chunks.iter().find(|c| c.last >= offset) {
            return Ok(Read { plan: ReadPlan::Remote(c.clone()), ..base });
        }
        // A gap (e.g. compacted away); continue at the next local data.
        if let Some(seg) = g.segments.iter().find(|s| s.next > offset && s.size > 0) {
            let plan = ReadPlan::Local(LocalRead { file: seg.file.clone(), pos: 0, end: seg.readable(fsync) });
            return Ok(Read { plan, ..base });
        }
        Ok(base)
    }

    /// Aborted transactions overlapping `[offset, upper)` for read_committed consumers.
    pub fn aborted_between(&self, offset: i64, upper: i64) -> Vec<AbortedTxn> {
        let g = self.lock();
        g.aborted.iter().filter(|a| a.last_offset >= offset && a.first_offset < upper).copied().collect()
    }

    pub fn aborted(&self) -> Vec<AbortedTxn> {
        self.lock().aborted.clone()
    }

    /// Where to look for the first record with a timestamp at or after `ts`: data starting at
    /// the earliest segment or chunk that reaches it.
    pub fn find_timestamp(&self, ts: i64) -> Option<Read> {
        let g = self.lock();
        let hw = g.high_watermark;
        let base = |plan, offset| Read {
            plan,
            offset,
            upper: hw,
            max_bytes: usize::MAX,
            high_watermark: hw,
            last_stable: g.last_stable(),
            log_start: g.log_start,
        };
        let local_from = g.segments.first().map(|s| s.base).unwrap_or(g.leo);
        for c in &g.chunks {
            if c.last >= g.log_start && c.last < local_from && c.max_timestamp >= ts {
                return Some(base(ReadPlan::Remote(c.clone()), c.first.max(g.log_start)));
            }
        }
        let fsync = g.config.fsync;
        for s in &g.segments {
            if s.size == 0 || s.max_timestamp < ts {
                continue;
            }
            let i = s.time_index.partition_point(|(t, _)| *t < ts);
            // Batches before the first indexed one reaching `ts` can't contain it.
            let from = if i == 0 { s.base } else { s.time_index[i - 1].1 };
            let plan = LocalRead { file: s.file.clone(), pos: s.floor_pos(from), end: s.readable(fsync) };
            return Some(base(ReadPlan::Local(plan), from.max(g.log_start)));
        }
        None
    }

    /// The segment or chunk holding the largest timestamp.
    pub fn find_max_timestamp(&self) -> Option<(i64, Read)> {
        let g = self.lock();
        let hw = g.high_watermark;
        let mk = |plan, offset| Read {
            plan,
            offset,
            upper: hw,
            max_bytes: usize::MAX,
            high_watermark: hw,
            last_stable: g.last_stable(),
            log_start: g.log_start,
        };
        let fsync = g.config.fsync;
        let local_from = g.segments.first().map(|s| s.base).unwrap_or(g.leo);
        let mut best: Option<(i64, Read)> = None;
        for c in g.chunks.iter().filter(|c| c.last >= g.log_start && c.last < local_from) {
            if best.as_ref().is_none_or(|(t, _)| c.max_timestamp >= *t) {
                best = Some((c.max_timestamp, mk(ReadPlan::Remote(c.clone()), c.first.max(g.log_start))));
            }
        }
        for s in g.segments.iter().filter(|s| s.size > 0) {
            if best.as_ref().is_none_or(|(t, _)| s.max_timestamp >= *t) {
                let from = s.time_index.last().map(|(_, o)| *o).unwrap_or(s.base);
                let plan = LocalRead { file: s.file.clone(), pos: s.floor_pos(from), end: s.readable(fsync) };
                best = Some((s.max_timestamp, mk(ReadPlan::Local(plan), from)));
            }
        }
        best
    }

    /// Offset of the first local batch (data before it is only in object storage).
    pub fn local_start(&self) -> i64 {
        let g = self.lock();
        g.segments.iter().find(|s| s.size > 0).map(|s| s.base).unwrap_or(g.leo).max(g.log_start)
    }

    /// End of the data in object storage.
    pub fn uploaded_end(&self) -> i64 {
        self.lock().uploaded_end
    }

    /// The next range to upload, if one is due: sealed segments as soon as they're durable, the
    /// active one once `min_bytes` accumulate or its oldest pending byte is `max_age_ms` old.
    pub fn upload_plan(&self, min_bytes: u64, max_bytes: u64, max_age_ms: i64, now_ms: i64) -> Option<UploadPlan> {
        let g = self.lock();
        let fsync = g.config.fsync;
        let n = g.segments.len();
        for (i, s) in g.segments.iter().enumerate() {
            let end = s.readable(fsync);
            if s.uploaded >= end {
                continue;
            }
            let pending = end - s.uploaded;
            let sealed = i + 1 < n;
            if !sealed && pending < min_bytes && now_ms - s.pending_since_ms < max_age_ms {
                return None;
            }
            let mut len = pending;
            if len > max_bytes {
                // Cut at a batch boundary from the sparse index.
                let limit = s.uploaded + max_bytes;
                if let Some((_, p)) = s.index.iter().rev().find(|(_, p)| *p > s.uploaded && *p <= limit) {
                    len = p - s.uploaded;
                }
            }
            return Some(UploadPlan { file: s.file.clone(), segment: s.base, pos: s.uploaded, len });
        }
        None
    }

    /// Read the bytes of an upload plan.
    pub fn read_upload(plan: &UploadPlan) -> io::Result<Bytes> {
        read_at(&plan.file, plan.pos, plan.len)
    }

    /// Record a finished upload.
    pub fn uploaded(&self, plan: &UploadPlan, chunk: Chunk, now_ms: i64) {
        let mut g = self.lock();
        if let Some(s) = g.segments.iter_mut().find(|s| s.base == plan.segment)
            && s.uploaded == plan.pos
        {
            s.uploaded = plan.pos + plan.len;
            s.pending_since_ms = now_ms;
        }
        g.uploaded_end = g.uploaded_end.max(chunk.last + 1);
        if chunk.last >= g.log_start {
            let i = g.chunks.partition_point(|c| c.first < chunk.first);
            g.chunks.insert(i, chunk);
        }
    }

    /// Apply retention, `DeleteRecords` (via `truncate_to`) and local retention. Returns the
    /// chunks to delete from object storage.
    pub fn clean(&self, now_ms: i64, policy: Retention, truncate_to: Option<i64>) -> Cleaned {
        let mut g = self.lock();
        let old_start = g.log_start;
        if let Some(t) = truncate_to {
            g.log_start = g.log_start.max(t.min(g.high_watermark));
        }
        // Units of the log in offset order: chunks, then local data not yet uploaded. The active
        // segment is never deleted.
        let local_from = g.uploaded_end;
        let mut units: Vec<(i64, i64, i64, u64)> =
            g.chunks.iter().map(|c| (c.first, c.last, c.max_timestamp, c.bytes)).collect();
        let n = g.segments.len();
        for s in g.segments.iter().take(n.saturating_sub(1)) {
            if s.next > local_from && s.size > 0 {
                units.push((s.base.max(local_from), s.next - 1, s.max_timestamp, s.size - s.uploaded));
            }
        }
        let active = g.segments.last().map(|s| s.size - s.uploaded.min(s.size)).unwrap_or(0);
        let mut total: i64 = units.iter().map(|u| u.3 as i64).sum::<i64>() + active as i64;
        let mut start = g.log_start;
        for &(_, last, max_ts, bytes) in &units {
            if last < start {
                total -= bytes as i64;
                continue;
            }
            let expired = policy.retention_ms >= 0 && max_ts < now_ms - policy.retention_ms;
            let oversize = policy.retention_bytes >= 0 && total - bytes as i64 >= policy.retention_bytes;
            if !(expired || oversize) {
                break;
            }
            total -= bytes as i64;
            start = last + 1;
        }
        g.log_start = start.min(g.high_watermark).max(g.log_start);
        let log_start = g.log_start;

        let mut removed_chunks = Vec::new();
        g.chunks.retain(|c| {
            if c.last < log_start {
                removed_chunks.push(c.clone());
                false
            } else {
                true
            }
        });
        g.aborted.retain(|a| a.last_offset >= log_start);

        // Local copies: drop sealed segments below the log start, and uploaded ones beyond the
        // local retention window.
        let mut removed_segments = 0;
        let mut local_total: i64 = g.segments.iter().map(|s| s.size as i64).sum();
        let keep_from = g.keep_local_from();
        while g.segments.len() > 1 {
            let s = &g.segments[0];
            let below_start = s.next <= log_start;
            let uploaded = s.uploaded >= s.size && s.next <= keep_from;
            let old = policy.local_retention_ms >= 0 && s.max_timestamp < now_ms - policy.local_retention_ms;
            let big = policy.local_retention_bytes >= 0 && local_total - s.size as i64 >= policy.local_retention_bytes;
            if !(below_start || (uploaded && (old || big))) {
                break;
            }
            let s = g.segments.remove(0);
            local_total -= s.size as i64;
            let _ = std::fs::remove_file(&s.path);
            removed_segments += 1;
            let next = s.next;
            while g.tail.front().is_some_and(|c| c.base < next) {
                g.pop_tail();
            }
        }
        if log_start > old_start {
            while g.tail.front().is_some_and(|c| c.last < log_start) {
                g.pop_tail();
            }
        }
        Cleaned { log_start, removed_chunks, removed_segments, start_moved: log_start != old_start }
    }

    /// Sealed segments whose local copy could go because object storage has them, oldest first,
    /// as (largest timestamp, bytes).
    pub fn evictable(&self) -> Vec<(i64, u64)> {
        let g = self.lock();
        let keep_from = g.keep_local_from();
        let n = g.segments.len();
        g.segments
            .iter()
            .take(n.saturating_sub(1))
            .take_while(|s| s.uploaded >= s.size && s.next <= keep_from)
            .map(|s| (s.max_timestamp, s.size))
            .collect()
    }

    /// Drop the local copies of up to `count` of the [`Log::evictable`] segments. Returns the bytes
    /// freed; reads of that range then come from object storage.
    pub fn evict(&self, count: usize) -> u64 {
        let mut g = self.lock();
        let keep_from = g.keep_local_from();
        let mut freed = 0;
        for _ in 0..count {
            let s = &g.segments[0];
            if g.segments.len() < 2 || s.uploaded < s.size || s.next > keep_from {
                break;
            }
            let s = g.segments.remove(0);
            freed += s.size;
            let _ = std::fs::remove_file(&s.path);
            while g.tail.front().is_some_and(|c| c.base < s.next) {
                g.pop_tail();
            }
        }
        freed
    }

    /// Drop every local copy object storage already has, the active segment included (an empty one
    /// takes its place). For closing an idle log, so it keeps nothing on disk. Returns the bytes freed.
    pub fn evict_all(&self, now_ms: i64) -> io::Result<u64> {
        {
            let mut g = self.lock();
            let s = g.segments.last().unwrap();
            if s.size > 0 && s.uploaded >= s.size && s.synced >= s.size && s.next <= g.keep_local_from() {
                let seg = Segment::create(&self.dir, g.leo, now_ms)?;
                g.segments.push(seg);
            }
        }
        Ok(self.evict(usize::MAX))
    }

    /// Record that everything is durable, so the next open can skip validating the newest
    /// segments. Call when closing an idle log; the marker is consumed by the next open.
    pub fn mark_clean(&self) -> io::Result<()> {
        let g = self.lock();
        if !g.config.fsync || g.high_watermark < g.leo || g.segments.iter().any(|s| s.synced < s.size) {
            return Ok(());
        }
        let text: String = g.segments.iter().map(|s| format!("{} {}\n", s.base, s.size)).collect();
        let tmp = self.dir.join(format!("{CLEAN_MARKER}.tmp"));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, self.dir.join(CLEAN_MARKER))
    }

    /// The earliest time retention, local cleanup or compaction could change anything if no
    /// more data arrives (`i64::MAX` if never). Closed partitions are left alone until then.
    pub fn next_due_ms(&self, policy: Retention, compaction: Option<CompactionPolicy>) -> i64 {
        let g = self.lock();
        let mut due = i64::MAX;
        let n = g.segments.len();
        // Retention drops the oldest data first, so only the oldest unit matters.
        if policy.retention_ms >= 0 {
            let oldest = g
                .chunks
                .iter()
                .filter(|c| c.last >= g.log_start)
                .map(|c| c.max_timestamp)
                .next()
                .or_else(|| g.segments.iter().take(n.saturating_sub(1)).find(|s| s.size > 0).map(|s| s.max_timestamp));
            if let Some(ts) = oldest {
                due = due.min(ts.saturating_add(policy.retention_ms));
            }
        }
        if policy.local_retention_ms >= 0
            && let Some(s) = g.segments.iter().take(n.saturating_sub(1)).find(|s| s.size > 0 && s.uploaded >= s.size)
        {
            due = due.min(s.max_timestamp.saturating_add(policy.local_retention_ms));
        }
        if let Some(c) = compaction
            && let Some(first) = g.chunks.iter().find(|ch| ch.last >= g.cleaned_to.max(g.log_start))
        {
            due = due.min(first.max_timestamp.saturating_add(c.min_lag_ms.max(0)));
            if c.max_lag_ms >= 0 {
                due = due.min(first.max_timestamp.saturating_add(c.max_lag_ms));
            }
        }
        due
    }

    /// Bytes held in this log's tail cache.
    pub fn cached_bytes(&self) -> usize {
        self.lock().tail_bytes
    }

    /// Whether a chunk is still part of the log (a read may race with compaction).
    pub fn has_chunk(&self, key: &str) -> bool {
        self.lock().chunks.iter().any(|c| c.key == key)
    }

    /// Note that offsets below `offset` are already compacted (known from the catalog on open).
    pub fn set_cleaned_to(&self, offset: i64) {
        let mut g = self.lock();
        g.cleaned_to = g.cleaned_to.max(offset);
    }

    pub fn cleaned_to(&self) -> i64 {
        self.lock().cleaned_to
    }

    /// The next compaction pass, if one is due. Only uploaded chunks below the active segment and
    /// the last stable offset are cleanable, so compaction never touches data still being written,
    /// and the range ends on a local segment boundary.
    pub fn compaction_plan(&self, now_ms: i64, policy: CompactionPolicy) -> Option<CompactionPlan> {
        let g = self.lock();
        let active = g.segments.last().map(|s| s.base).unwrap_or(g.leo);
        let mut bound = active.min(g.last_stable()).min(g.uploaded_end);
        // Don't split a local segment: its tail would stay readable uncompacted.
        if let Some(s) = g.segments.iter().find(|s| s.base < bound && s.next > bound) {
            bound = s.base;
        }
        let lag_cutoff = now_ms.saturating_sub(policy.min_lag_ms.max(0));
        let mut chunks = Vec::new();
        for c in g.chunks.iter().filter(|c| c.last >= g.log_start) {
            if c.last >= bound || c.max_timestamp > lag_cutoff {
                break;
            }
            chunks.push(c.clone());
        }
        let end = chunks.last()?.last + 1;
        let total: u64 = chunks.iter().map(|c| c.bytes).sum();
        let dirty: Vec<&Chunk> = chunks.iter().filter(|c| c.last >= g.cleaned_to).collect();
        let dirty_bytes: u64 = dirty.iter().map(|c| c.bytes).sum();
        let first = dirty.first()?;
        let overdue = policy.max_lag_ms >= 0 && first.max_timestamp < now_ms.saturating_sub(policy.max_lag_ms);
        if !overdue && (dirty_bytes as f64) < policy.dirty_ratio * total as f64 {
            return None;
        }
        Some(CompactionPlan {
            dirty_from: g.cleaned_to.max(g.log_start),
            log_start: g.log_start,
            end,
            aborted: g.aborted.iter().filter(|a| a.first_offset < end && a.last_offset >= g.log_start).copied().collect(),
            chunks,
        })
    }

    /// Swap a plan's chunks for their compacted replacements. Returns `false` (changing nothing)
    /// if the log moved underneath the plan; the caller then discards `compacted`.
    pub fn compacted(&self, plan: &CompactionPlan, compacted: Vec<Chunk>) -> bool {
        let mut g = self.lock();
        if plan.chunks.iter().any(|p| !g.chunks.iter().any(|c| c.key == p.key)) {
            return false;
        }
        g.chunks.retain(|c| !plan.chunks.iter().any(|p| p.key == c.key));
        for c in compacted {
            let i = g.chunks.partition_point(|x| x.first < c.first);
            g.chunks.insert(i, c);
        }
        g.cleaned_to = g.cleaned_to.max(plan.end);
        // Local copies of the compacted range are stale now.
        while g.segments.len() > 1 && g.segments[0].next <= plan.end && g.segments[0].uploaded >= g.segments[0].size {
            let s = g.segments.remove(0);
            let _ = std::fs::remove_file(&s.path);
        }
        while g.tail.front().is_some_and(|c| c.base < plan.end) {
            g.pop_tail();
        }
        true
    }

    /// Delete all local files.
    pub fn destroy(self) -> io::Result<()> {
        let dir = self.dir.clone();
        drop(self);
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn remove_from(dir: &Path, base: i64) -> io::Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        if parse_segment_name(&e.file_name().to_string_lossy()).is_some_and(|b| b >= base) {
            std::fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

impl Inner {
    /// Local segments from the start of the oldest open transaction on have to stay: transaction
    /// state is rebuilt from them when the log opens.
    fn keep_local_from(&self) -> i64 {
        self.producers.ongoing().map(|(_, first)| first).min().unwrap_or(i64::MAX)
    }

    fn last_stable(&self) -> i64 {
        self.producers.first_unstable_offset().map(|o| o.min(self.high_watermark)).unwrap_or(self.high_watermark)
    }

    fn write(&mut self, dir: &Path, h: &mut Header, data: Bytes, now_ms: i64) -> io::Result<()> {
        let roll = {
            let s = self.segments.last().unwrap();
            s.size > 0
                && (s.size + data.len() as u64 > self.config.segment_bytes || now_ms - s.created_ms >= self.config.segment_ms)
        };
        if roll {
            let seg = Segment::create(dir, self.leo, now_ms)?;
            self.segments.push(seg);
        }
        let interval = self.config.index_interval_bytes;
        let s = self.segments.last_mut().unwrap();
        if s.size == 0 {
            s.created_ms = now_ms;
        }
        if s.uploaded >= s.size {
            s.pending_since_ms = now_ms;
        }
        let pos = s.size;
        if let Err(e) = s.file.write_all_at(&data, pos) {
            // Leave no partial batch behind.
            let _ = s.file.set_len(pos);
            return Err(e);
        }
        s.size += data.len() as u64;
        s.track(h, pos, interval);
        self.leo = h.next_offset();
        self.tail_bytes += data.len();
        if let Some(b) = &self.config.tail_budget {
            b.used.fetch_add(data.len(), Ordering::Relaxed);
        }
        self.tail.push_back(Cached { base: h.base_offset, last: h.last_offset(), data });
        let (cap, over) = match &self.config.tail_budget {
            Some(b) => (self.config.tail_cache_bytes.min(b.share()), b.over()),
            None => (self.config.tail_cache_bytes, false),
        };
        // The newest batch stays (a consumer at the end reads it next) unless memory is short.
        while self.tail_bytes > cap && (self.tail.len() > 1 || over) {
            self.pop_tail();
        }
        Ok(())
    }

    fn pop_tail(&mut self) {
        if let Some(c) = self.tail.pop_front() {
            self.tail_bytes -= c.data.len();
            if let Some(b) = &self.config.tail_budget {
                b.used.fetch_sub(c.data.len(), Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{BatchOptions, NewRecord, TRANSACTIONAL, build};

    fn tmp(name: &str) -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("roll-log-{name}-{n}"))
    }

    fn batch_of(n: usize, ts: i64) -> Bytes {
        let recs: Vec<_> = (0..n)
            .map(|i| NewRecord { value: Some(Bytes::from(format!("v{i}"))), timestamp: Some(ts), ..Default::default() })
            .collect();
        build(&recs, BatchOptions::default(), ts)
    }

    fn idem(pid: i64, seq: i32, n: usize, attrs: i16) -> Bytes {
        let recs: Vec<_> = (0..n).map(|_| NewRecord { value: Some(Bytes::from_static(b"x")), ..Default::default() }).collect();
        build(&recs, BatchOptions { producer_id: pid, producer_epoch: 0, base_sequence: seq, attributes: attrs }, 0)
    }

    fn cfg() -> LogConfig {
        LogConfig { segment_bytes: 1024, tail_cache_bytes: 0, ..Default::default() }
    }

    fn read_all(log: &Log, offset: i64) -> Bytes {
        let r = log.read(offset, usize::MAX, false).unwrap();
        match r.plan {
            ReadPlan::Ready(b) => b,
            ReadPlan::Local(l) => read_local(&l, r.offset, r.upper, r.max_bytes, 4096).unwrap(),
            ReadPlan::Remote(_) => panic!("remote"),
        }
    }

    fn offsets(buf: &Bytes) -> Vec<i64> {
        batch::headers(buf).map(|(_, h)| h.base_offset).collect()
    }

    #[test]
    fn append_flush_read() {
        let dir = tmp("basic");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        let a = log.append(&[batch_of(3, 10)], AppendOptions::at(10)).unwrap();
        assert_eq!((a.base_offset, a.last_offset), (0, 2));
        // Not durable yet → not visible.
        assert_eq!(log.high_watermark(), 0);
        assert!(read_all(&log, 0).is_empty());
        assert_eq!(log.flush().unwrap(), 3);
        let b = log.append(&[batch_of(2, 20)], AppendOptions::at(20)).unwrap();
        assert_eq!(b.base_offset, 3);
        log.flush().unwrap();
        assert_eq!(offsets(&read_all(&log, 0)), vec![0, 3]);
        // Offset inside the first batch still returns that batch.
        assert_eq!(offsets(&read_all(&log, 1)), vec![0, 3]);
        assert_eq!(offsets(&read_all(&log, 4)), vec![3]);
        assert!(read_all(&log, 5).is_empty());
        assert!(log.read(6, 100, false).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rolls_segments_and_reads_across() {
        let dir = tmp("roll");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        for i in 0..50 {
            log.append(&[batch_of(2, i)], AppendOptions::at(i)).unwrap();
        }
        log.flush().unwrap();
        let st = log.stats();
        assert!(st.segments > 3, "{st:?}");
        assert_eq!(st.log_end, 100);
        let mut next = 0;
        let mut seen = 0;
        while next < 100 {
            let buf = read_all(&log, next);
            assert!(!buf.is_empty());
            for (_, h) in batch::headers(&buf) {
                assert_eq!(h.base_offset, next);
                next = h.next_offset();
                seen += 1;
            }
        }
        assert_eq!(seen, 50);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_truncates_torn_writes() {
        let dir = tmp("recover");
        {
            let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
            log.append(&[batch_of(2, 1)], AppendOptions::at(1)).unwrap();
            log.append(&[batch_of(2, 2)], AppendOptions::at(2)).unwrap();
            log.flush().unwrap();
        }
        // Simulate a torn write: half a batch at the end of the active segment.
        let seg = dir.join(Segment::file_name(0));
        let half = batch_of(3, 3);
        let mut f = OpenOptions::new().append(true).open(&seg).unwrap();
        std::io::Write::write_all(&mut f, &half[..half.len() / 2]).unwrap();
        drop(f);
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        assert_eq!(log.log_end(), 4);
        assert_eq!(log.high_watermark(), 4);
        let a = log.append(&[batch_of(1, 4)], AppendOptions::at(4)).unwrap();
        assert_eq!(a.base_offset, 4);
        log.flush().unwrap();
        assert_eq!(offsets(&read_all(&log, 0)), vec![0, 2, 4]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn idempotent_retries_get_original_offsets() {
        let dir = tmp("idem");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        let a = log.append(&[idem(9, 0, 2, 0)], AppendOptions::at(0)).unwrap();
        let dup = log.append(&[idem(9, 0, 2, 0)], AppendOptions::at(0)).unwrap();
        assert!(dup.duplicate);
        assert_eq!(dup.base_offset, a.base_offset);
        assert!(matches!(log.append(&[idem(9, 5, 1, 0)], AppendOptions::at(0)), Err(AppendError::OutOfOrder)));
        assert_eq!(log.append(&[idem(9, 2, 1, 0)], AppendOptions::at(0)).unwrap().base_offset, 2);
        drop(log);
        // Producer state is rebuilt from the log on restart.
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        assert!(log.append(&[idem(9, 2, 1, 0)], AppendOptions::at(0)).unwrap().duplicate);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transactions_and_stable_offset() {
        let dir = tmp("txn");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        log.append(&[batch_of(1, 0)], AppendOptions::at(0)).unwrap();
        log.append(&[idem(5, 0, 2, TRANSACTIONAL)], AppendOptions::at(0)).unwrap();
        log.flush().unwrap();
        assert_eq!(log.last_stable(), 1);
        assert!(log.has_open_txn(5));
        // read_committed stops at the open transaction.
        let r = log.read(0, usize::MAX, true).unwrap();
        assert_eq!(r.upper, 1);
        let (marker, aborted) = log.append_marker(5, 0, false, 0, 0).unwrap();
        assert_eq!(marker, 3);
        assert_eq!(aborted, Some(AbortedTxn { producer_id: 5, first_offset: 1, last_offset: 3 }));
        log.flush().unwrap();
        assert_eq!(log.last_stable(), 4);
        assert_eq!(log.aborted_between(0, 4).len(), 1);
        assert!(log.aborted_between(4, 10).is_empty());
        // Clients can't write markers.
        let c = batch::control_batch(5, 0, true, 0, 0);
        assert!(matches!(log.append(&[c], AppendOptions::at(0)), Err(AppendError::InvalidRecord(_))));
        drop(log);
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        assert!(!log.has_open_txn(5));
        assert_eq!(log.last_stable(), 4);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Upload everything durable, the active segment included, the way the broker does.
    fn upload_all(log: &Log, chunks: &mut Vec<Chunk>) {
        while let Some(plan) = log.upload_plan(1, 1 << 20, 0, 100) {
            let data = Log::read_upload(&plan).unwrap();
            let span = batch::summarize(&data).unwrap();
            let chunk = Chunk { first: span.first, last: span.last, max_timestamp: span.max_timestamp, bytes: data.len() as u64, key: format!("{}", span.first) };
            log.uploaded(&plan, chunk.clone(), 100);
            chunks.push(chunk);
        }
    }

    #[test]
    fn evict_all_keeps_nothing_local_except_open_transactions() {
        let dir = tmp("evict");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        for i in 0..30 {
            log.append(&[batch_of(2, i)], AppendOptions::at(i)).unwrap();
        }
        log.flush().unwrap();
        let mut chunks = Vec::new();
        upload_all(&log, &mut chunks);
        let leo = log.log_end();
        assert!(log.evict_all(100).unwrap() > 0);
        assert_eq!(log.stats().local_bytes, 0);
        assert!(matches!(log.read(0, usize::MAX, false).unwrap().plan, ReadPlan::Remote(_)));
        // Appends carry on at the same offsets…
        assert_eq!(log.append(&[batch_of(1, 50)], AppendOptions::at(50)).unwrap().base_offset, leo);
        log.flush().unwrap();
        drop(log);
        // …and reopening with only the chunk index sees all of it.
        let recovered = |chunks: &Vec<Chunk>| Recovered { log_start: 0, chunks: chunks.clone(), aborted: Vec::new() };
        let log = Log::open(&dir, cfg(), recovered(&chunks), 200).unwrap();
        assert_eq!(log.log_end(), leo + 1);
        assert!(matches!(log.read(0, usize::MAX, false).unwrap().plan, ReadPlan::Remote(_)));
        assert_eq!(offsets(&read_all(&log, leo)), vec![leo]);

        // An open transaction's data stays: its state is rebuilt from it on open.
        log.append(&[idem(5, 0, 2, TRANSACTIONAL)], AppendOptions::at(60)).unwrap();
        log.flush().unwrap();
        upload_all(&log, &mut chunks);
        log.evict_all(300).unwrap();
        assert!(log.stats().local_bytes > 0);
        assert!(log.evictable().is_empty());
        drop(log);
        let log = Log::open(&dir, cfg(), recovered(&chunks), 400).unwrap();
        assert!(log.has_open_txn(5));
        assert_eq!(log.last_stable(), leo + 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upload_then_serve_remote_and_drop_local() {
        let dir = tmp("tier");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        for i in 0..30 {
            log.append(&[batch_of(2, i)], AppendOptions::at(i)).unwrap();
        }
        log.flush().unwrap();
        let mut uploaded = Vec::new();
        while let Some(plan) = log.upload_plan(1, 1 << 20, i64::MAX, 100) {
            let data = Log::read_upload(&plan).unwrap();
            let span = batch::summarize(&data).unwrap();
            let chunk = Chunk {
                first: span.first,
                last: span.last,
                max_timestamp: span.max_timestamp,
                bytes: data.len() as u64,
                key: format!("{}", span.first),
            };
            log.uploaded(&plan, chunk.clone(), 100);
            uploaded.push((chunk, data));
        }
        // Everything sealed is uploaded; the active segment waits for size or age.
        assert!(!uploaded.is_empty());
        assert!(log.uploaded_end() > 0);
        let policy = Retention { retention_ms: -1, retention_bytes: -1, local_retention_ms: -1, local_retention_bytes: 0 };
        let c = log.clean(100, policy, None);
        assert!(c.removed_segments > 0);
        assert!(c.removed_chunks.is_empty());
        assert_eq!(log.log_start(), 0);
        let r = log.read(0, usize::MAX, false).unwrap();
        let ReadPlan::Remote(chunk) = r.plan else { panic!("expected remote read") };
        let data = &uploaded.iter().find(|(c, _)| *c == chunk).unwrap().1;
        assert_eq!(offsets(&batch::slice_for_fetch(data, 0, r.upper, usize::MAX))[0], 0);

        // Reopen with the chunk list and no local data: offsets continue after the remote end.
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
        let chunks: Vec<_> = uploaded.iter().map(|(c, _)| c.clone()).collect();
        let end = chunks.last().unwrap().last + 1;
        let log = Log::open(&dir, cfg(), Recovered { log_start: 0, chunks, aborted: vec![] }, 0).unwrap();
        assert_eq!(log.log_end(), end);
        assert_eq!(log.log_start(), 0);
        assert_eq!(log.append(&[batch_of(1, 0)], AppendOptions::at(0)).unwrap().base_offset, end);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_by_time_size_and_delete_records() {
        let dir = tmp("retention");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        for i in 0..40 {
            log.append(&[batch_of(2, i * 1000)], AppendOptions::at(i * 1000)).unwrap();
        }
        log.flush().unwrap();
        let keep = Retention { retention_ms: -1, retention_bytes: -1, local_retention_ms: -1, local_retention_bytes: -1 };
        let c = log.clean(40_000, Retention { retention_ms: 20_000, ..keep }, None);
        assert!(c.start_moved);
        let start = log.log_start();
        assert!(start > 0 && start <= 40, "{start}");
        assert!(log.read(0, 1, false).is_err());
        // Size retention keeps at least the active segment.
        log.clean(40_000, Retention { retention_bytes: 0, ..keep }, None);
        assert!(log.log_start() > start);
        // DeleteRecords moves the start to an exact offset, never past the high watermark.
        let c = log.clean(40_000, keep, Some(79));
        assert_eq!(c.log_start, 79);
        assert_eq!(log.clean(40_000, keep, Some(1_000)).log_start, 80);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timestamps_index() {
        let dir = tmp("ts");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        for i in 0..20 {
            log.append(&[batch_of(1, i * 10)], AppendOptions::at(0)).unwrap();
        }
        log.flush().unwrap();
        let r = log.find_timestamp(95).unwrap();
        let ReadPlan::Local(l) = &r.plan else { panic!() };
        let data = read_local(l, r.offset, r.upper, usize::MAX, 0).unwrap();
        let first = batch::headers(&data).find(|(_, h)| h.max_timestamp >= 95).unwrap().1;
        assert_eq!(first.base_offset, 10);
        assert!(log.find_timestamp(1_000).is_none());
        let (ts, _) = log.find_max_timestamp().unwrap();
        assert_eq!(ts, 190);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn log_append_time_and_limits() {
        let dir = tmp("lat");
        let log = Log::open(&dir, LogConfig { fsync: false, ..cfg() }, Recovered::default(), 0).unwrap();
        let mut opts = AppendOptions::at(777);
        opts.log_append_time = true;
        let a = log.append(&[batch_of(1, 5)], opts).unwrap();
        assert_eq!(a.log_append_time, 777);
        // Without fsync, data is visible immediately.
        assert_eq!(log.high_watermark(), 1);
        let h = batch::parse_header(&read_all(&log, 0)).unwrap();
        assert_eq!(h.max_timestamp, 777);
        let mut opts = AppendOptions::at(0);
        opts.max_batch_bytes = 10;
        assert!(matches!(log.append(&[batch_of(1, 0)], opts), Err(AppendError::TooLarge(_))));
        let mut opts = AppendOptions::at(1_000_000);
        opts.max_timestamp_before_ms = Some(1000);
        assert!(matches!(log.append(&[batch_of(1, 0)], opts), Err(AppendError::InvalidTimestamp)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tail_cache_serves_recent_batches() {
        let dir = tmp("tail");
        let log = Log::open(&dir, LogConfig { tail_cache_bytes: 1 << 20, ..cfg() }, Recovered::default(), 0).unwrap();
        for i in 0..5 {
            log.append(&[batch_of(1, i)], AppendOptions::at(i)).unwrap();
        }
        log.flush().unwrap();
        let r = log.read(2, usize::MAX, false).unwrap();
        let ReadPlan::Ready(b) = r.plan else { panic!("expected a cached read") };
        assert_eq!(offsets(&b), vec![2, 3, 4]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tail_caches_share_one_budget() {
        let budget = TailBudget::new(4096);
        let dirs: Vec<PathBuf> = (0..4).map(|i| tmp(&format!("budget{i}"))).collect();
        let config = LogConfig { tail_cache_bytes: 1 << 20, tail_budget: Some(budget.clone()), segment_bytes: 1 << 20, ..cfg() };
        let logs: Vec<Log> = dirs.iter().map(|d| Log::open(d, config.clone(), Recovered::default(), 0).unwrap()).collect();
        assert_eq!(budget.open_logs(), 4);
        let one = batch_of(20, 1).len();
        for i in 0..50 {
            for log in &logs {
                log.append(&[batch_of(20, i)], AppendOptions::at(i)).unwrap();
            }
        }
        // Each log keeps about its share (1 KB) instead of the 1 MB it could alone.
        for log in &logs {
            assert!(log.cached_bytes() <= 1024 + one, "{} cached", log.cached_bytes());
        }
        assert_eq!(budget.used(), logs.iter().map(|l| l.cached_bytes()).sum::<usize>());
        drop(logs);
        assert_eq!((budget.used(), budget.open_logs()), (0, 0), "closing a log returns its memory");
        for d in dirs {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn clean_close_skips_validation_but_not_safety() {
        let dir = tmp("clean");
        {
            let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
            log.append(&[batch_of(2, 1)], AppendOptions::at(1)).unwrap();
            log.append(&[batch_of(2, 2)], AppendOptions::at(2)).unwrap();
            log.mark_clean().unwrap();
            assert!(!dir.join(CLEAN_MARKER).exists(), "not durable yet, so no marker");
            log.flush().unwrap();
            log.mark_clean().unwrap();
        }
        assert!(dir.join(CLEAN_MARKER).exists());
        {
            let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
            assert!(!dir.join(CLEAN_MARKER).exists(), "a marker is good for one open");
            assert_eq!(log.log_end(), 4);
            log.mark_clean().unwrap();
        }
        // A torn write after the clean close changes the size, so the open validates again.
        let half = batch_of(3, 3);
        let mut f = OpenOptions::new().append(true).open(dir.join(Segment::file_name(0))).unwrap();
        std::io::Write::write_all(&mut f, &half[..half.len() / 2]).unwrap();
        drop(f);
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        assert_eq!(log.log_end(), 4);
        assert_eq!(offsets(&read_all(&log, 0)), vec![0, 2]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn next_due_follows_the_oldest_data() {
        let dir = tmp("due");
        let log = Log::open(&dir, cfg(), Recovered::default(), 0).unwrap();
        for ts in [1_000, 2_000, 3_000] {
            for _ in 0..20 {
                log.append(&[batch_of(2, ts)], AppendOptions::at(ts)).unwrap();
            }
        }
        log.flush().unwrap();
        let keep = |ms| Retention { retention_ms: ms, retention_bytes: -1, local_retention_ms: -1, local_retention_bytes: -1 };
        assert_eq!(log.next_due_ms(keep(-1), None), i64::MAX, "nothing ever expires");
        assert_eq!(log.next_due_ms(keep(500), None), 1_500, "the oldest segment expires first");
        log.clean(1_600, keep(500), None);
        assert!(log.next_due_ms(keep(500), None) > 1_500);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

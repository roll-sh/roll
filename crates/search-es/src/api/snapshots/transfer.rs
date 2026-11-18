//! How bytes move between a cluster and a bucket repository's store (`s3`, `gcs`, `azure`): the
//! repository's throttles (`max_snapshot_bytes_per_sec`, `max_restore_bytes_per_sec`, and for
//! restores the cluster's `indices.recovery.max_bytes_per_sec`), the counters
//! `_nodes/stats/repositories` reports, and objects written in parts (`chunk_size`,
//! `buffer_size`, `max_multipart_parts`).
//!
//! Elasticsearch splits a file into `chunk_size` blobs and uploads a blob larger than
//! `buffer_size` as a multipart upload of `buffer_size` parts (S3; GCS's resumable uploads and
//! Azure's staged blocks the same way, in parts of a size of their own, [`crate::blob`]). roll's files are a segment's
//! records, gzipped as one stream: [`Upload`] writes one object of it, holding at most one part
//! in memory (charged to the `request` breaker), and aborts its upload when it fails. An upload
//! a crash left behind is aborted by `_cleanup` and after snapshot deletions
//! ([`super::s3`]), except those running here ([`RepoIo::uploads`]).

use crate::error::EsResult;
use crate::json::Value;
use crate::meta::settings::Settings;
use crate::blob::Store;
use crate::s3 as client;
use crate::units::ByteSize;
use bytes::Bytes;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// S3's multipart limits, Elasticsearch's bounds on `buffer_size` and `chunk_size`.
pub(super) const MIN_PART: i64 = 5 << 20;
pub(super) const MAX_PART: i64 = 5 << 30;
pub(super) const MAX_FILE: i64 = 5 << 40;
pub(super) const MAX_PARTS: i64 = 10_000;
/// `max_snapshot_bytes_per_sec`'s default (`BlobStoreRepository.MAX_SNAPSHOT_BYTES_PER_SEC`).
const SNAPSHOT_RATE: i64 = 40 << 20;
/// `indices.recovery.max_bytes_per_sec`'s default.
const RECOVERY_RATE: i64 = 40 << 20;
/// `max_multipart_upload_cleanup_size`'s default: dangling uploads aborted per cleanup.
const UPLOAD_CLEANUP: i64 = 1000;
/// A pause shorter than this is not taken (Lucene's `MIN_PAUSE_CHECK_MSEC`).
const MIN_PAUSE: Duration = Duration::from_millis(5);

/// A byte rate everyone moving bytes through it shares (Lucene's `SimpleRateLimiter`): each
/// payment is due one byte-time after the previous one's due time, so the time a caller spends
/// producing bytes between payments counts towards the next, and after an idle stretch the
/// burst is at most one payment. The rate holds over any stretch longer than one payment.
#[derive(Default)]
pub struct RateLimiter {
    state: parking_lot::Mutex<Pace>,
}

#[derive(Default)]
struct Pace {
    /// Bytes per second; 0 is unlimited.
    rate: i64,
    /// When the last payment was due (or later, once that passed).
    until: Option<Instant>,
}

impl RateLimiter {
    /// Sets the rate (0 or less: unlimited), counting from now when it changes, as Lucene does.
    pub fn set(&self, rate: i64) {
        let mut p = self.state.lock();
        if p.rate != rate.max(0) {
            p.rate = rate.max(0);
            p.until = Some(Instant::now());
        }
    }

    /// Waits until `bytes` may move: the nanoseconds waited.
    pub async fn pause(&self, bytes: u64) -> u64 {
        let wait = {
            let mut p = self.state.lock();
            if p.rate <= 0 || bytes == 0 {
                return 0;
            }
            let now = Instant::now();
            let target = p.until.unwrap_or(now) + Duration::from_secs_f64(bytes as f64 / p.rate as f64);
            p.until = Some(target.max(now));
            target.saturating_duration_since(now)
        };
        if wait < MIN_PAUSE {
            return 0;
        }
        tokio::time::sleep(wait).await;
        wait.as_nanos() as u64
    }
}

/// One repository's traffic on this node: its throttles, what `_nodes/stats/repositories`
/// counts (`RepositoriesStats.SnapshotStats`), and the multipart uploads running here.
#[derive(Default)]
pub struct RepoIo {
    pub(super) snapshot: RateLimiter,
    pub(super) restore: RateLimiter,
    read_throttled: AtomicU64,
    write_throttled: AtomicU64,
    started: AtomicU64,
    completed: AtomicU64,
    uploaded_blobs: AtomicU64,
    uploaded_bytes: AtomicU64,
    upload_ms: AtomicU64,
    read_ms: AtomicU64,
    /// Upload ids of the multipart uploads in progress here: a cleanup leaves them alone.
    uploads: parking_lot::Mutex<HashSet<String>>,
}

impl RepoIo {
    /// Pays for `bytes` written: nanoseconds throttled.
    pub(super) async fn write(&self, bytes: u64) -> u64 {
        let n = self.snapshot.pause(bytes).await;
        self.write_throttled.fetch_add(n, Ordering::Relaxed);
        n
    }

    /// Pays for `bytes` read by a restore or an analysis, through the repository's limit and
    /// then the cluster's recovery limit: nanoseconds throttled.
    pub(super) async fn read(&self, recovery: &RateLimiter, bytes: u64) -> u64 {
        let n = self.restore.pause(bytes).await + recovery.pause(bytes).await;
        self.read_throttled.fetch_add(n, Ordering::Relaxed);
        n
    }

    pub(super) fn shard_started(&self) {
        self.started.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn shard_done(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    /// Time spent reading what a snapshot uploads.
    pub(super) fn read_for_upload(&self, took: Duration) {
        self.read_ms.fetch_add(took.as_millis() as u64, Ordering::Relaxed);
    }

    pub(super) fn running_uploads(&self) -> HashSet<String> {
        self.uploads.lock().clone()
    }

    /// `_nodes/stats` `repositories.<name>`, in Elasticsearch's order.
    pub fn stats(&self, human: bool) -> Value {
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed) as i64;
        let (started, completed) = (n(&self.started), n(&self.completed));
        let nanos = |v: i64| crate::units::TimeValue { duration: v, unit: crate::units::TimeUnit::Nanos }.human();
        let mut m = crate::json::Map::new();
        if human {
            m.insert("total_read_throttled_time".into(), nanos(n(&self.read_throttled)).into());
            m.insert("total_write_throttled_time".into(), nanos(n(&self.write_throttled)).into());
        }
        m.insert("total_read_throttled_time_nanos".into(), n(&self.read_throttled).into());
        m.insert("total_write_throttled_time_nanos".into(), n(&self.write_throttled).into());
        m.insert("shard_snapshots_started".into(), started.into());
        m.insert("shard_snapshots_completed".into(), completed.into());
        m.insert("shard_snapshots_in_progress".into(), (started - completed).max(0).into());
        m.insert("uploaded_blobs".into(), n(&self.uploaded_blobs).into());
        let mut human_pair = |k: &str, raw: &str, h: String, v: i64| {
            if human {
                m.insert(k.into(), h.into());
            }
            m.insert(raw.into(), v.into());
        };
        let bytes = n(&self.uploaded_bytes);
        human_pair("uploaded_size", "uploaded_size_in_bytes", ByteSize(bytes).human(), bytes);
        let ms = |v: i64| crate::units::TimeValue::millis(v).human();
        human_pair("total_upload_time", "total_upload_time_in_millis", ms(n(&self.upload_ms)), n(&self.upload_ms));
        human_pair("total_read_time", "total_read_time_in_millis", ms(n(&self.read_ms)), n(&self.read_ms));
        Value::Obj(m)
    }
}

/// What every repository on a node reports before it moved a byte.
pub fn idle_stats(human: bool) -> Value {
    RepoIo::default().stats(human)
}

/// How a repository writes objects: an object holds at most `chunk` bytes, and one larger than
/// `buffer` goes up in `buffer`-sized parts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Sizes {
    pub chunk: u64,
    pub buffer: u64,
}

/// `buffer_size`'s default (`S3Repository.DEFAULT_BUFFER_SIZE`): 100mb, or 5% of the heap when
/// that is less, and never under 5mb.
pub(super) fn default_buffer(cluster: &crate::Cluster) -> i64 {
    let heap = crate::api::nodes::NodeMetrics::sample(cluster).heap_max;
    (heap / 20).clamp(MIN_PART, 100 << 20)
}

/// A byte size setting within `min..=max` (`Setting.byteSizeSetting`).
pub(super) fn bounded_size(settings: &Settings, key: &str, default: i64, min: i64, max: i64) -> EsResult<i64> {
    let Some(raw) = settings.get(key) else { return Ok(default) };
    let v = ByteSize::parse(raw, key).map_err(|e| crate::error::EsError::illegal_argument(e.reason().to_string()).caused_by(e))?;
    if v.bytes() < min {
        return Err(crate::error::EsError::illegal_argument(format!("failed to parse value [{raw}] for setting [{key}], must be >= [{}]", ByteSize(min).exact())));
    }
    if v.bytes() > max {
        return Err(crate::error::EsError::illegal_argument(format!("failed to parse value [{raw}] for setting [{key}], must be <= [{}]", ByteSize(max).exact())));
    }
    Ok(v.bytes())
}

/// `max_multipart_parts` (`Setting.intSetting(.., 10_000, 1, 10_000)`).
pub(super) fn max_parts(settings: &Settings) -> EsResult<i64> {
    let Some(raw) = settings.get("max_multipart_parts") else { return Ok(MAX_PARTS) };
    let key = "max_multipart_parts";
    let v = crate::units::parse_java_int(raw)
        .ok_or_else(|| crate::error::EsError::illegal_argument(format!("Failed to parse value [{raw}] for setting [{key}]")).caused_by(crate::error::EsError::number_format(raw)))?;
    if v < 1 {
        return Err(crate::error::EsError::illegal_argument(format!("Failed to parse value [{raw}] for setting [{key}] must be >= 1")));
    }
    if i64::from(v) > MAX_PARTS {
        return Err(crate::error::EsError::illegal_argument(format!("Failed to parse value [{raw}] for setting [{key}] must be <= {MAX_PARTS}")));
    }
    Ok(i64::from(v))
}

/// The sizes a repository's settings name (already validated): `chunk_size` no larger than
/// `buffer_size` times `max_multipart_parts` (`S3Repository.objectSizeLimit`).
pub(super) fn sizes(cluster: &crate::Cluster, settings: &Settings) -> Sizes {
    let buffer = bounded_size(settings, "buffer_size", default_buffer(cluster), MIN_PART, MAX_PART).unwrap_or(100 << 20);
    let chunk = bounded_size(settings, "chunk_size", MAX_FILE, MIN_PART, MAX_FILE).unwrap_or(MAX_FILE);
    let parts = max_parts(settings).unwrap_or(MAX_PARTS);
    Sizes { chunk: chunk.min(buffer.saturating_mul(parts)) as u64, buffer: buffer as u64 }
}

/// A throttle setting (`max_snapshot_bytes_per_sec`, `max_restore_bytes_per_sec`): bytes per
/// second, 0 for none.
pub(super) fn rate(settings: &Settings, key: &str, default: i64) -> EsResult<i64> {
    match settings.get(key) {
        None => Ok(default),
        Some(raw) => ByteSize::parse(raw, key).map(|b| b.bytes().max(0)).map_err(|e| crate::error::EsError::illegal_argument(e.reason().to_string()).caused_by(e)),
    }
}

/// Validates the throttle settings every blob store repository reads.
pub(super) fn validate_rates(settings: &Settings) -> EsResult<()> {
    rate(settings, "max_snapshot_bytes_per_sec", SNAPSHOT_RATE)?;
    rate(settings, "max_restore_bytes_per_sec", 0)?;
    Ok(())
}

/// Sets `io`'s limits from the repository's settings.
pub(super) fn apply_rates(io: &RepoIo, settings: &Settings) {
    io.snapshot.set(rate(settings, "max_snapshot_bytes_per_sec", SNAPSHOT_RATE).unwrap_or(SNAPSHOT_RATE));
    io.restore.set(rate(settings, "max_restore_bytes_per_sec", 0).unwrap_or(0));
}

/// The cluster's recovery limit (`indices.recovery.max_bytes_per_sec`), which Elasticsearch
/// applies to restores on top of the repository's.
pub(super) fn recovery_rate(st: &crate::meta::ClusterState) -> i64 {
    st.metadata
        .cluster_setting("indices.recovery.max_bytes_per_sec")
        .and_then(|v| ByteSize::parse(v, "indices.recovery.max_bytes_per_sec").ok())
        .map_or(RECOVERY_RATE, |b| b.bytes().max(0))
}

/// `max_multipart_upload_cleanup_size`: how many dangling uploads one cleanup aborts.
pub(super) fn upload_cleanup_limit(settings: &Settings) -> usize {
    settings.get("max_multipart_upload_cleanup_size").and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(UPLOAD_CLEANUP).max(0) as usize
}

/// Why an object couldn't be written: the bucket's error, or the `request` breaker refusing the
/// part buffer. Either way its multipart upload has been aborted.
pub(super) enum Failed {
    Store(client::Error),
    Breaker(crate::error::EsError),
}

/// One object being written: bytes are held until a part is full, then go up as a multipart
/// upload's part; an object no larger than one part goes up in one request when it finishes.
/// Bytes pay the repository's snapshot throttle as they are written in (as Elasticsearch
/// throttles the stream it reads), so producing them overlaps the wait.
pub(super) struct Upload<'a> {
    store: &'a Store,
    io: &'a RepoIo,
    key: String,
    part: usize,
    buf: Vec<u8>,
    /// The part buffer's capacity, charged to the `request` breaker.
    held: crate::breaker::Reservation,
    charged: usize,
    multipart: Option<Multipart<'a>>,
    size: u64,
    /// Parts uploaded (0: one request).
    pub parts: u32,
    /// Time throttled, and spent uploading (throttling included).
    pub throttled_nanos: u64,
    pub elapsed: Duration,
}

struct Multipart<'a> {
    id: String,
    /// What completing the upload needs of each part.
    etags: Vec<String>,
    io: &'a RepoIo,
}

impl Drop for Multipart<'_> {
    fn drop(&mut self) {
        self.io.uploads.lock().remove(&self.id);
    }
}

impl<'a> Upload<'a> {
    pub fn new(store: &'a Store, io: &'a RepoIo, budget: Arc<search_aggs::memory::MemoryBudget>, key: String, part: u64) -> Upload<'a> {
        let held = crate::breaker::Reservation::new(budget);
        Upload { store, io, key, part: part as usize, buf: Vec::new(), held, charged: 0, multipart: None, size: 0, parts: 0, throttled_nanos: 0, elapsed: Duration::ZERO }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// Bytes written so far.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Adds `data` to the object.
    pub async fn write(&mut self, mut data: &[u8]) -> Result<(), Failed> {
        let paid = self.io.write(data.len() as u64).await;
        self.throttled_nanos += paid;
        self.elapsed += Duration::from_nanos(paid);
        while !data.is_empty() {
            if self.buf.len() == self.part
                && let Err(e) = self.flush_part(None).await
            {
                return Err(self.abort_with(Failed::Store(e)).await);
            }
            let n = (self.part - self.buf.len()).min(data.len());
            let need = self.buf.len() + n;
            if need > self.buf.capacity() {
                let cap = need.max(self.buf.capacity() * 2).min(self.part);
                if let Err(e) = self.held.add((cap - self.charged) as u64, "<s3_upload_part>") {
                    return Err(self.abort_with(Failed::Breaker(e)).await);
                }
                self.charged = cap;
                self.buf.reserve_exact(cap - self.buf.len());
            }
            self.buf.extend_from_slice(&data[..n]);
            self.size += n as u64;
            data = &data[n..];
        }
        Ok(())
    }

    /// Sends the part held; `last` is the object's size when no more follow.
    async fn flush_part(&mut self, last: Option<u64>) -> Result<(), client::Error> {
        let started = Instant::now();
        if self.multipart.is_none() {
            let id = self.store.begin(&self.key).await?;
            self.io.uploads.lock().insert(id.clone());
            self.multipart = Some(Multipart { id, etags: Vec::new(), io: self.io });
        }
        let body = Bytes::copy_from_slice(&self.buf);
        let offset = self.size - self.buf.len() as u64;
        let Some(m) = self.multipart.as_mut() else { return Ok(()) };
        let etag = self.store.upload_part(&self.key, &m.id, m.etags.len() as u32 + 1, offset, body, last).await?;
        m.etags.push(etag);
        self.parts += 1;
        self.buf.clear();
        self.elapsed += started.elapsed();
        Ok(())
    }

    /// Writes what is left and completes the object: its size.
    pub async fn finish(&mut self) -> Result<u64, Failed> {
        let started = Instant::now();
        let done = match self.multipart.is_some() {
            false => {
                let body = Bytes::from(std::mem::take(&mut self.buf));
                self.store.put(&self.key, body).await
            }
            true => async {
                // Parts are sent only when more bytes follow, so the last is still held.
                self.flush_part(Some(self.size)).await?;
                match self.multipart.as_ref() {
                    Some(m) => self.store.complete(&self.key, &m.id, &m.etags).await,
                    None => Ok(()),
                }
            }
            .await,
        };
        self.elapsed += started.elapsed();
        if let Err(e) = done {
            return Err(self.abort_with(Failed::Store(e)).await);
        }
        self.multipart = None;
        self.io.uploaded_blobs.fetch_add(1, Ordering::Relaxed);
        self.io.uploaded_bytes.fetch_add(self.size, Ordering::Relaxed);
        self.io.upload_ms.fetch_add(self.elapsed.as_millis() as u64, Ordering::Relaxed);
        Ok(self.size)
    }

    async fn abort_with(&mut self, f: Failed) -> Failed {
        self.abort().await;
        f
    }

    /// Abandons the object: its multipart upload, if one was started, is aborted.
    pub async fn abort(&mut self) {
        if let Some(m) = self.multipart.take() {
            let _ = self.store.abort(&self.key, &m.id).await;
        }
    }
}

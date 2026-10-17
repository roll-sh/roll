//! Bottomless durability: object storage (S3) is the source of truth, memory is a cache.
//!
//! Every database's write stream (see [`crate::database::Feed`]) is buffered by the node's
//! [`Shipper`] and flushed, for **all databases on the node together**, as one compressed
//! segment per interval (1s by default). PUT requests therefore scale with nodes, not tenants:
//! ten thousand mostly idle databases cost the same as one, and an idle database costs nothing.
//!
//! ```text
//! redis/seg/<node>/<seq>.zst         segments: frames of (db, start offset, RESP commands)
//! redis/db/<db>/manifest.json        where a database's state lives: snapshot + segments
//! redis/db/<db>/snap/<offset>.rdb.zst snapshots
//! ```
//!
//! Manifests are rewritten lazily (every 30s for busy databases, and when a database unloads).
//! A manifest names the node that owned the database and the last segment it had flushed; after a
//! crash, the loader also scans that node's later segments, so nothing acknowledged as durable is
//! lost to the lazy manifest.
//!
//! Snapshots ([`Shipper::snapshot`]) bound how much log a load replays. Branches and in-place
//! restores embed a frozen copy of the history they start from ([`history`]), so a database's
//! own retention never breaks them, and [`collect_garbage`] can delete whatever no manifest
//! references.

use crate::database::{Database, FeedSink};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::StreamExt;
use parking_lot::Mutex;
use redis_engine::{Effect, Engine};
use redis_engine::commands::eq;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, watch};

pub use pg_storage::{Objects, StorageConfig};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapRef {
    pub key: String,
    /// Stream offset the snapshot is consistent with.
    pub offset: u64,
    /// When it was taken (Unix ms).
    pub at_ms: i64,
}

/// A branch starts from its parent's state at `offset`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Base {
    pub db: String,
    pub offset: u64,
    pub at_ms: i64,
    /// The part of the parent's history leading to `offset`, frozen when the branch was made
    /// (without it, the parent's current manifest is read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Box<Manifest>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SegRef {
    pub node: String,
    pub seq: u64,
    /// The database's stream offset at the end of its frames in this segment.
    pub end: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Retained snapshots, oldest first (point-in-time restore starts from one of them).
    #[serde(default)]
    pub snapshots: Vec<SnapRef>,
    /// Without snapshots: the parent state this database was branched from.
    #[serde(default)]
    pub base: Option<Base>,
    /// The id the log frames are written under, when it isn't the manifest's own (archived
    /// histories keep the original database's frames).
    #[serde(default)]
    pub stream: Option<String>,
    /// Segments holding the database's frames after the oldest snapshot, in order.
    pub segments: Vec<SegRef>,
    /// Stream offset covered by the snapshot and segments.
    pub end: u64,
    /// The node that owned the database when this was written: its segments after `after_seq`
    /// may hold newer frames.
    pub node: Option<String>,
    pub after_seq: u64,
}

/// Object storage for tests: S3 under a unique prefix when `ROLL_E2E_S3_BUCKET` is set (with
/// `ROLL_E2E_S3_ENDPOINT` and the usual AWS credentials), otherwise a fresh temp directory.
pub fn test_objects(name: &str) -> Objects {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let unique = format!("{name}-{}-{n}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let cfg = match std::env::var("ROLL_E2E_S3_BUCKET") {
        Ok(bucket) => StorageConfig::S3 {
            bucket,
            region: std::env::var("AWS_REGION").ok(),
            endpoint: std::env::var("ROLL_E2E_S3_ENDPOINT").ok(),
            prefix: Some(format!("redis-tests/{unique}")),
        },
        Err(_) => StorageConfig::Local(std::env::temp_dir().join(format!("roll-redis-{unique}"))),
    };
    Objects::new(&cfg).expect("test object storage")
}

pub fn manifest_key(db: &str) -> String {
    format!("redis/db/{db}/manifest.json")
}

pub fn segment_key(node: &str, seq: u64) -> String {
    format!("redis/seg/{node}/{seq:016}.zst")
}

pub fn snapshot_key(db: &str, offset: u64) -> String {
    format!("redis/db/{db}/snap/{offset:020}.rdb.zst")
}

/// Serializes a keyspace for snapshots (the RDB format in production).
pub trait SnapshotCodec: Send + Sync {
    /// Called with the engine locked (writes wait), so it should only encode.
    fn save(&self, engine: &Engine, out: &mut Vec<u8>) -> anyhow::Result<()>;
    /// Load a decoded snapshot into an empty database.
    fn load(&self, db: &Arc<Database>, data: &[u8]) -> anyhow::Result<()>;
}

/// A frozen copy of the part of `source`'s history (as returned by [`restore_at`]) that leads
/// to `upto`: the newest snapshot at or before it, the segments after that, or the base chain.
pub fn history(m: &Manifest, source: &str, upto: u64) -> Manifest {
    let snap = m.snapshots.iter().rev().find(|s| s.offset <= upto).cloned();
    let from = snap.as_ref().map_or(0, |s| s.offset);
    let mut segments = Vec::new();
    if from < upto {
        for s in m.segments.iter().filter(|s| s.end > from) {
            segments.push(s.clone());
            if s.end >= upto {
                break;
            }
        }
    }
    Manifest {
        base: if snap.is_some() { None } else { m.base.clone() },
        snapshots: snap.into_iter().collect(),
        stream: Some(m.stream.clone().unwrap_or_else(|| source.to_string())),
        segments,
        end: upto,
        node: None,
        after_seq: 0,
    }
}

/// One frame per write batch: `u16 id length, id, u64 start offset, i64 unix ms, u32 length,
/// RESP commands`.
fn put_frame(out: &mut Vec<u8>, db: &str, start: u64, at_ms: i64, data: &[u8]) {
    out.put_u16(db.len() as u16);
    out.put_slice(db.as_bytes());
    out.put_u64(start);
    out.put_i64(at_ms);
    out.put_u32(data.len() as u32);
    out.put_slice(data);
}

pub struct Frame<'a> {
    pub db: &'a str,
    pub start: u64,
    /// When the batch was executed (point-in-time restore granularity).
    pub at_ms: i64,
    pub data: &'a [u8],
}

impl Frame<'_> {
    pub fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

/// Iterate the frames of a decompressed segment.
pub fn frames(mut seg: &[u8]) -> impl Iterator<Item = Frame<'_>> {
    std::iter::from_fn(move || {
        if seg.len() < 2 {
            return None;
        }
        let n = seg.get_u16() as usize;
        let db = std::str::from_utf8(seg.get(..n)?).ok()?;
        seg.advance(n);
        if seg.len() < 20 {
            return None;
        }
        let start = seg.get_u64();
        let at_ms = seg.get_i64();
        let len = seg.get_u32() as usize;
        let data = seg.get(..len)?;
        seg.advance(len);
        Some(Frame { db, start, at_ms, data })
    })
}

/// Parse a RESP command stream into effects, following `SELECT`. MULTI/EXEC markers are kept
/// (`Engine::apply_all` skips them).
pub fn parse_stream(data: &[u8], db: &mut u16, out: &mut Vec<Effect>) -> anyhow::Result<()> {
    let mut buf = BytesMut::from(data);
    let limits = redis_proto::Limits::default();
    while let Some(args) = redis_proto::parse_request(&mut buf, limits).map_err(|e| anyhow::anyhow!("corrupt log: {e}"))? {
        if args.is_empty() {
            continue;
        }
        if args.len() == 2 && eq(&args[0], "select") {
            *db = std::str::from_utf8(&args[1]).ok().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("corrupt log: bad SELECT"))?;
            continue;
        }
        out.push(Effect { db: *db, args });
    }
    if !buf.is_empty() {
        anyhow::bail!("corrupt log: truncated command");
    }
    Ok(())
}

#[derive(Default)]
struct Pending {
    data: Vec<u8>,
    /// Stream end offset per database in `data`.
    ends: HashMap<String, u64>,
}

struct DbLog {
    manifest: Manifest,
    durable: watch::Sender<u64>,
    dirty: bool,
    feed_id: u64,
}

pub struct ShipperConfig {
    /// Group-commit interval: the most acknowledged writes a node crash can lose.
    pub flush_interval: Duration,
    /// Flush early when this much is buffered.
    pub flush_bytes: usize,
    pub manifest_interval: Duration,
    pub zstd_level: i32,
    /// Without one, databases never snapshot and loads replay their whole log.
    pub codec: Option<Arc<dyn SnapshotCodec>>,
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_secs(1),
            flush_bytes: 8 << 20,
            manifest_interval: Duration::from_secs(30),
            zstd_level: 1,
            codec: None,
        }
    }
}

/// Log a database has accumulated since its newest snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogStats {
    pub bytes: u64,
    pub segments: usize,
    /// It still reads history from another database (an unsnapshotted branch or restore).
    pub based: bool,
}

/// Rewrite unchanged manifests once their `after_seq` lags this many segments: a stale one keeps
/// all of the node's later segments from garbage collection.
const MANIFEST_LAG: u64 = 600;

/// The node's durability pipeline (one per node).
pub struct Shipper {
    pub node: String,
    pub objects: Objects,
    cfg: ShipperConfig,
    pending: Mutex<Pending>,
    kick: Notify,
    dbs: Mutex<HashMap<String, DbLog>>,
    next_seq: AtomicU64,
    flush_lock: tokio::sync::Mutex<()>,
    /// Bytes and objects written (metering / tests).
    pub bytes_put: AtomicU64,
    pub objects_put: AtomicU64,
}

struct ShipSink {
    shipper: Arc<Shipper>,
    db: String,
}

impl FeedSink for ShipSink {
    fn write(&self, chunk: &Bytes, end: u64) {
        let big = {
            let mut p = self.shipper.pending.lock();
            put_frame(&mut p.data, &self.db, end - chunk.len() as u64, redis_engine::util::now_ms(), chunk);
            p.ends.insert(self.db.clone(), end);
            p.data.len() >= self.shipper.cfg.flush_bytes
        };
        if big {
            self.shipper.kick.notify_one();
        }
    }
}

impl Shipper {
    pub async fn start(node: impl Into<String>, objects: Objects, cfg: ShipperConfig) -> anyhow::Result<Arc<Self>> {
        let node = node.into();
        // Continue numbering after whatever this node wrote before (restart).
        let existing = objects.list(&format!("redis/seg/{node}/")).await?;
        let next = existing.iter().filter_map(|(k, _)| seq_of(k)).max().map_or(1, |s| s + 1);
        let me = Arc::new(Self {
            node,
            objects,
            cfg,
            pending: Mutex::new(Pending::default()),
            kick: Notify::new(),
            dbs: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(next),
            flush_lock: tokio::sync::Mutex::new(()),
            bytes_put: AtomicU64::new(0),
            objects_put: AtomicU64::new(0),
        });
        let s = me.clone();
        tokio::spawn(async move {
            let mut last_manifests = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(s.cfg.flush_interval) => {}
                    _ = s.kick.notified() => {}
                }
                if let Err(e) = s.flush().await {
                    tracing::warn!(error = %e, "redis log flush failed, retrying");
                    continue;
                }
                if last_manifests.elapsed() >= s.cfg.manifest_interval {
                    last_manifests = tokio::time::Instant::now();
                    if let Err(e) = s.write_manifests(false).await {
                        tracing::warn!(error = %e, "redis manifest write failed");
                    }
                }
            }
        });
        Ok(me)
    }

    /// Last flushed segment number.
    pub fn last_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst) - 1
    }

    /// Start shipping `db`'s writes. `manifest` is what it was restored from.
    pub async fn attach(self: &Arc<Self>, db: &Arc<Database>, mut manifest: Manifest) -> anyhow::Result<()> {
        let sink = Arc::new(ShipSink { shipper: self.clone(), db: db.id.clone() });
        let (feed_id, offset) = db.subscribe(sink);
        manifest.end = offset;
        manifest.node = Some(self.node.clone());
        manifest.after_seq = self.last_seq();
        // Record ownership right away so a crash before the first lazy manifest is recoverable.
        self.objects.put(&manifest_key(&db.id), Bytes::from(serde_json::to_vec(&manifest)?)).await?;
        let (durable, rx) = watch::channel(offset);
        let me = Arc::downgrade(self);
        *db.durability.lock() = Some(crate::database::Durability {
            durable: rx,
            kick: Arc::new(move || {
                if let Some(s) = me.upgrade() {
                    s.kick();
                }
            }),
        });
        self.dbs.lock().insert(db.id.clone(), DbLog { manifest, durable, dirty: false, feed_id });
        Ok(())
    }

    /// Stop shipping: flush, write the final manifest. Call after the database stopped serving.
    pub async fn detach(&self, db: &Database) -> anyhow::Result<Manifest> {
        self.flush().await?;
        let log = self.dbs.lock().remove(&db.id);
        let Some(log) = log else { anyhow::bail!("database {} is not attached", db.id) };
        db.unsubscribe(log.feed_id);
        *db.durability.lock() = None;
        let mut m = log.manifest;
        m.after_seq = self.last_seq();
        self.objects.put(&manifest_key(&db.id), Bytes::from(serde_json::to_vec(&m)?)).await?;
        Ok(m)
    }

    /// Watch the durable (flushed to object storage) stream offset of a database.
    pub fn durable(&self, db: &str) -> Option<watch::Receiver<u64>> {
        self.dbs.lock().get(db).map(|l| l.durable.subscribe())
    }

    /// Ask for an early flush (a client waits for durability).
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub fn manifest(&self, db: &str) -> Option<Manifest> {
        self.dbs.lock().get(db).map(|l| l.manifest.clone())
    }

    /// Flush everything buffered as one segment.
    pub async fn flush(&self) -> anyhow::Result<()> {
        let _serial = self.flush_lock.lock().await;
        let p = std::mem::take(&mut *self.pending.lock());
        if p.data.is_empty() {
            return Ok(());
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let level = self.cfg.zstd_level;
        let raw = p.data;
        let data = if raw.len() > 64 << 10 {
            tokio::task::spawn_blocking(move || zstd::encode_all(&raw[..], level)).await??
        } else {
            zstd::encode_all(&raw[..], level)?
        };
        let key = segment_key(&self.node, seq);
        let data = Bytes::from(data);
        let mut backoff = Duration::from_millis(50);
        loop {
            match self.objects.put(&key, data.clone()).await {
                Ok(()) => break,
                Err(e) => {
                    tracing::warn!(error = %e, key, "segment upload failed, retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
        self.bytes_put.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.objects_put.fetch_add(1, Ordering::Relaxed);
        let mut dbs = self.dbs.lock();
        for (db, end) in p.ends {
            let Some(log) = dbs.get_mut(&db) else { continue };
            match log.manifest.segments.last_mut() {
                Some(last) if last.node == self.node && last.seq == seq => last.end = end,
                _ => log.manifest.segments.push(SegRef { node: self.node.clone(), seq, end }),
            }
            log.manifest.end = end;
            log.dirty = true;
            log.durable.send_replace(end);
        }
        Ok(())
    }

    /// Rewrite manifests of databases that changed (all of them with `force`).
    pub async fn write_manifests(&self, force: bool) -> anyhow::Result<()> {
        let after = self.last_seq();
        let todo: Vec<(String, Manifest)> = {
            let mut dbs = self.dbs.lock();
            dbs.iter_mut()
                // An idle database's stale `after_seq` would keep this node's segments alive.
                .filter(|(_, l)| force || l.dirty || after.saturating_sub(l.manifest.after_seq) >= MANIFEST_LAG)
                .map(|(id, l)| {
                    l.dirty = false;
                    l.manifest.after_seq = after;
                    (id.clone(), l.manifest.clone())
                })
                .collect()
        };
        for (id, m) in todo {
            self.objects.put(&manifest_key(&id), Bytes::from(serde_json::to_vec(&m)?)).await?;
        }
        Ok(())
    }

    pub fn codec(&self) -> Option<Arc<dyn SnapshotCodec>> {
        self.cfg.codec.clone()
    }

    pub fn log_stats(&self, db: &str) -> Option<LogStats> {
        let dbs = self.dbs.lock();
        let m = &dbs.get(db)?.manifest;
        let from = m.snapshots.last().map_or(0, |s| s.offset);
        Some(LogStats {
            bytes: m.end.saturating_sub(from),
            segments: m.segments.iter().filter(|s| s.end > from).count(),
            based: m.snapshots.is_empty() && m.base.is_some(),
        })
    }

    /// Snapshot an attached database at its current offset, and record it in the manifest
    /// (see [`Shipper::snapshotted`] for `retain_ms`). Writes wait while the keyspace is
    /// encoded; compression and upload don't. `None` without a codec, or when nothing changed
    /// since the last snapshot.
    pub async fn snapshot(&self, db: &Arc<Database>, retain_ms: i64) -> anyhow::Result<Option<SnapRef>> {
        let Some(codec) = self.cfg.codec.clone() else { return Ok(None) };
        let Some(m) = self.manifest(&db.id) else { anyhow::bail!("database {} is not attached", db.id) };
        let (offset, raw) = {
            let eng = db.engine(0);
            let offset = db.snapshot_point();
            if m.snapshots.last().is_some_and(|s| s.offset == offset) {
                return Ok(None);
            }
            let mut raw = Vec::new();
            codec.save(&eng, &mut raw)?;
            (offset, raw)
        };
        let at_ms = redis_engine::util::now_ms();
        let level = self.cfg.zstd_level.max(3);
        let data = Bytes::from(tokio::task::spawn_blocking(move || zstd::encode_all(&raw[..], level)).await??);
        let key = snapshot_key(&db.id, offset);
        let size = data.len() as u64;
        self.objects.put(&key, data).await?;
        self.bytes_put.fetch_add(size, Ordering::Relaxed);
        self.objects_put.fetch_add(1, Ordering::Relaxed);
        // The log up to the snapshot is in storage too, so the manifest stays consistent.
        self.flush().await?;
        let snap = SnapRef { key, offset, at_ms };
        self.snapshotted(&db.id, snap.clone(), retain_ms);
        self.write_manifest(&db.id).await?;
        Ok(Some(snap))
    }

    async fn write_manifest(&self, db: &str) -> anyhow::Result<()> {
        let m = {
            let mut dbs = self.dbs.lock();
            let Some(l) = dbs.get_mut(db) else { return Ok(()) };
            l.dirty = false;
            l.manifest.after_seq = self.last_seq();
            l.manifest.clone()
        };
        self.objects.put(&manifest_key(db), Bytes::from(serde_json::to_vec(&m)?)).await
    }

    /// Record a snapshot. Snapshots older than `retain_ms` (but always the newest) are dropped
    /// from the manifest, with the segments only they needed. Returns the dropped snapshots.
    pub fn snapshotted(&self, db: &str, snap: SnapRef, retain_ms: i64) -> Vec<SnapRef> {
        let mut dbs = self.dbs.lock();
        let Some(l) = dbs.get_mut(db) else { return Vec::new() };
        let horizon = snap.at_ms - retain_ms;
        l.manifest.snapshots.push(snap);
        let keep_from = l.manifest.snapshots.iter().rposition(|s| s.at_ms <= horizon).unwrap_or(0);
        let dropped: Vec<SnapRef> = l.manifest.snapshots.drain(..keep_from).collect();
        let oldest = l.manifest.snapshots[0].offset;
        l.manifest.segments.retain(|s| s.end > oldest);
        l.manifest.base = None;
        l.dirty = true;
        dropped
    }
}

fn seq_of(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.strip_suffix(".zst")?.parse().ok()
}

/// How far to restore.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Until {
    /// Stop before frames ending after this stream offset.
    pub offset: Option<u64>,
    /// Stop before frames executed after this Unix ms.
    pub time_ms: Option<i64>,
}

impl Until {
    pub const LATEST: Until = Until { offset: None, time_ms: None };

    pub fn time(ms: i64) -> Self {
        Self { offset: None, time_ms: Some(ms) }
    }

    fn allows(&self, f: &Frame) -> bool {
        self.offset.is_none_or(|o| f.end() <= o) && self.time_ms.is_none_or(|t| f.at_ms <= t)
    }

    fn allows_snap(&self, s: &SnapRef) -> bool {
        self.offset.is_none_or(|o| s.offset <= o) && self.time_ms.is_none_or(|t| s.at_ms <= t)
    }
}

pub async fn read_manifest(objects: &Objects, id: &str) -> anyhow::Result<Option<Manifest>> {
    match objects.get(&manifest_key(id)).await? {
        Some(raw) => Ok(Some(serde_json::from_slice(&raw)?)),
        None => Ok(None),
    }
}

/// Load a database's latest state from object storage. Returns its manifest, with `end` set to
/// the restored offset (a database without a manifest starts empty).
pub async fn restore(objects: &Objects, db: &Arc<Database>, codec: Option<&dyn SnapshotCodec>) -> anyhow::Result<Manifest> {
    let id = db.id.clone();
    restore_at(objects, &id, db, Until::LATEST, codec).await
}

/// Load the state of database `source` as of `until` into `db` (point-in-time restore when
/// `source` is `db`'s own id, branching when it's another database): the newest snapshot the
/// target allows, then log frames up to the target. Branches restore their parent's history up
/// to the branch point first. Also scans the segments that the last owning node wrote after the
/// manifest (crash recovery).
pub async fn restore_at(objects: &Objects, source: &str, db: &Arc<Database>, until: Until, codec: Option<&dyn SnapshotCodec>) -> anyhow::Result<Manifest> {
    let mut st = Replay { applied: 0, dbsel: 0, last_ms: 0 };
    let mut m = match read_manifest(objects, source).await? {
        Some(m) => replay(objects, source, m, db, until, codec, &mut st).await?,
        None => Manifest::default(),
    };
    db.set_offset(st.applied);
    m.end = st.applied;
    Ok(m)
}

struct Replay {
    applied: u64,
    dbsel: u16,
    last_ms: i64,
}

/// Segments fetched ahead while replaying (cold-start latency is mostly GET round trips).
const PREFETCH: usize = 8;

fn replay<'a>(
    objects: &'a Objects,
    id: &'a str,
    mut m: Manifest,
    db: &'a Arc<Database>,
    until: Until,
    codec: Option<&'a dyn SnapshotCodec>,
    st: &'a mut Replay,
) -> futures::future::BoxFuture<'a, anyhow::Result<Manifest>> {
    Box::pin(async move {
        let stream = m.stream.clone().unwrap_or_else(|| id.to_string());
        let id = stream.as_str();
        if let Some(snap) = m.snapshots.iter().rev().find(|s| until.allows_snap(s)).cloned() {
            let codec = codec.ok_or_else(|| anyhow::anyhow!("snapshot {} needs a snapshot codec", snap.key))?;
            let data = objects.get(&snap.key).await?.ok_or_else(|| anyhow::anyhow!("snapshot {} is missing", snap.key))?;
            let data = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
            codec.load(db, &data)?;
            st.applied = snap.offset;
            st.last_ms = snap.at_ms;
            st.dbsel = 0;
        } else if !m.snapshots.is_empty() {
            anyhow::bail!("{id}: the requested point is older than the retained history");
        } else if let Some(base) = m.base.clone() {
            let lim = Until { offset: Some(until.offset.map_or(base.offset, |o| o.min(base.offset))), time_ms: until.time_ms };
            let parent = match base.history {
                Some(h) => *h,
                None => read_manifest(objects, &base.db).await?.unwrap_or_default(),
            };
            replay(objects, &base.db, parent, db, lim, codec, st).await?;
            if st.applied < base.offset && until.offset.is_none_or(|o| o >= base.offset) && until.time_ms.is_none() {
                anyhow::bail!("{id}: parent {} history ends at {} before the branch point {}", base.db, st.applied, base.offset);
            }
        }
        let mut segs = m.segments.clone();
        if let Some(node) = &m.node {
            let mut later: Vec<u64> = objects
                .list(&format!("redis/seg/{node}/"))
                .await?
                .iter()
                .filter_map(|(k, _)| seq_of(k))
                .filter(|s| *s > m.after_seq)
                .collect();
            later.sort_unstable();
            segs.extend(later.into_iter().map(|seq| SegRef { node: node.clone(), seq, end: 0 }));
        }
        let mut seen = HashSet::new();
        // Listed segments that end before what's already applied are skipped without fetching.
        let applied = st.applied;
        segs.retain(|s| seen.insert((s.node.clone(), s.seq)) && (s.end == 0 || s.end > applied));
        let mut fetched = futures::stream::iter(segs)
            .map(|seg| async move {
                let key = segment_key(&seg.node, seg.seq);
                let Some(data) = objects.get(&key).await? else {
                    anyhow::bail!("segment {key} is missing");
                };
                let raw = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
                anyhow::Ok((seg, key, raw))
            })
            .buffered(PREFETCH);
        while let Some(r) = fetched.next().await {
            let (seg, key, raw) = r?;
            let mut effects = Vec::new();
            let mut found = None;
            let mut stop = false;
            for f in frames(&raw) {
                if f.db != id || f.end() <= st.applied {
                    continue;
                }
                if !until.allows(&f) {
                    stop = true;
                    break;
                }
                if f.start != st.applied {
                    anyhow::bail!("log gap for {id}: have {}, segment {key} continues at {}", st.applied, f.start);
                }
                parse_stream(f.data, &mut st.dbsel, &mut effects)?;
                st.applied = f.end();
                st.last_ms = f.at_ms;
                found = Some(f.end());
            }
            if let Some(end) = found
                && !m.segments.iter().any(|s| s.node == seg.node && s.seq == seg.seq)
            {
                m.segments.push(SegRef { end, ..seg });
            }
            if !effects.is_empty() {
                db.engine(0).apply_all(&effects);
            }
            if stop {
                break;
            }
        }
        Ok(m)
    })
}

/// What a garbage collection pass did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GcStats {
    pub manifests: usize,
    /// Unreferenced objects found this pass (deleted once they've stayed so for `min_age`).
    pub unreferenced: usize,
    pub deleted: usize,
    pub deleted_bytes: u64,
}

const GC_STATE: &str = "redis/gc/pending.json";

fn mark(m: &Manifest, keep: &mut HashSet<String>, after: &mut HashMap<String, u64>) {
    keep.extend(m.snapshots.iter().map(|s| s.key.clone()));
    keep.extend(m.segments.iter().map(|s| segment_key(&s.node, s.seq)));
    if let Some(node) = &m.node {
        let a = after.entry(node.clone()).or_insert(u64::MAX);
        *a = (*a).min(m.after_seq);
    }
    if let Some(h) = m.base.as_ref().and_then(|b| b.history.as_deref()) {
        mark(h, keep, after);
    }
}

/// Delete segments and snapshots that no manifest references. Deletion is two-phase: an object
/// must be found unreferenced by passes at least `min_age` apart, so references that appear
/// while a pass runs (a branch being made, a manifest not yet rewritten) keep it. Run from one
/// place per bucket, every `min_age` or so.
pub async fn collect_garbage(objects: &Objects, min_age: Duration) -> anyhow::Result<GcStats> {
    let now = redis_engine::util::now_ms();
    // List candidates before reading manifests: whatever a manifest written after this refers
    // to is either already listed and marked below, or new and not a candidate.
    let mut candidates = objects.list("redis/seg/").await?;
    candidates.extend(objects.list("redis/db/").await?.into_iter().filter(|(k, _)| k.contains("/snap/")));
    let mut pending: HashMap<String, i64> = match objects.get(GC_STATE).await? {
        Some(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
        None => HashMap::new(),
    };
    let mut st = GcStats::default();
    let (mut keep, mut after) = (HashSet::new(), HashMap::new());
    for (key, _) in objects.list("redis/db/").await? {
        if !key.ends_with("/manifest.json") {
            continue;
        }
        let Some(raw) = objects.get(&key).await? else { continue };
        let m: Manifest = serde_json::from_slice(&raw).map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
        mark(&m, &mut keep, &mut after);
        st.manifests += 1;
    }
    let live = |key: &str| {
        keep.contains(key)
            || key.strip_prefix("redis/seg/").and_then(|k| k.split_once('/')).is_some_and(|(node, _)| {
                // Possibly newer frames of a database whose manifest is lazy (or crash recovery).
                after.get(node).is_some_and(|a| seq_of(key).is_some_and(|s| s > *a))
            })
    };
    let cutoff = now - min_age.as_millis() as i64;
    let mut next = HashMap::new();
    let mut doomed = Vec::new();
    for (key, size) in candidates {
        if live(&key) {
            continue;
        }
        st.unreferenced += 1;
        let since = pending.remove(&key).unwrap_or(now);
        if since <= cutoff {
            doomed.push((key, size));
        } else {
            next.insert(key, since);
        }
    }
    let results = futures::stream::iter(doomed.into_iter().map(|(key, size)| async move { objects.delete(&key).await.map(|()| size) }))
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
    for r in results {
        let size = r?;
        st.deleted += 1;
        st.deleted_bytes += size;
    }
    objects.put(GC_STATE, Bytes::from(serde_json::to_vec(&next)?)).await?;
    Ok(st)
}

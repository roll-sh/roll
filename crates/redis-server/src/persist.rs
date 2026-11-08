//! Bottomless durability: object storage (S3) is the source of truth, memory is a cache.
//!
//! Every database's write stream (see [`crate::database::Feed`]) is buffered by the node's
//! [`Shipper`] and flushed, for **all databases on the node together**, as one compressed
//! segment per interval (1s by default). PUT requests therefore scale with nodes, not tenants:
//! ten thousand mostly idle databases cost the same as one, and an idle database costs nothing.
//!
//! ```text
//! redis/seg/<log>/<seq>.zst                 segments: frames of (db, start offset, RESP commands)
//! redis/db/<db>/manifest.json               where a database's state lives: snapshot + segments
//! redis/db/<db>/snap/<offset>-<log>.rdb.zst snapshots
//! ```
//!
//! A log (`<node>.<random>`) has one writer: the process that started it. Manifests are
//! rewritten lazily (every 30s for busy databases, and when a database unloads). While a database
//! is served, its manifest names the log its owner writes and the last segment it had flushed;
//! after a crash, the loader also scans that log's later segments, so nothing acknowledged as
//! durable is lost to the lazy manifest. A manifest written on a clean unload names no log.
//!
//! Every write that decides what a database's history is happens once: segments are created,
//! never overwritten; manifests are compare-and-swapped on the version their writer last saw; a
//! node taking a database over fences the previous owner's log first (see [`Shipper`]). Two nodes
//! that both believe they own a database (a lease that just moved) can't both commit.
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

pub use pg_storage::{Objects, StorageConfig, Version};

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
    /// The stored object's version when it was read (what replacing it compares against).
    #[serde(skip)]
    pub version: Option<Version>,
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

/// `log` is the writer's: no two nodes ever write the same snapshot key.
pub fn snapshot_key(db: &str, offset: u64, log: &str) -> String {
    format!("redis/db/{db}/snap/{offset:020}-{log}.rdb.zst")
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
        version: None,
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
    /// The log the stored manifest names (`manifest.node` as last written): frames go into a
    /// segment of the current log only once this is it.
    named: Option<String>,
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

/// A log of this process: `<node>.<random>`. Each process writes its own, and a node whose log
/// was fenced moves to a new one, so a log only ever has one writer.
fn new_log(node: &str) -> String {
    format!("{node}.{:012x}", redis_engine::util::Rng::new().next_u64() & 0xffff_ffff_ffff)
}

/// The node's durability pipeline (one per node).
///
/// **Fencing.** Correctness doesn't rest on who believes it owns a database: segments are written
/// create-only, and a node taking a database over first puts an empty object at the next
/// segment number of the log its manifest names ([`restore`]). The old owner's next segment
/// write then fails; it moves to a new log, and before any frame of a database goes into a
/// segment of it, that database's manifest is rewritten (compare-and-swap on the version it last
/// wrote) to name the new log. A database whose manifest someone else wrote meanwhile was taken
/// over: it is dropped here ([`Shipper::on_fenced`]) and its frames with it. So everything the
/// old owner made durable before the fence is in what the new owner replayed, and nothing it
/// writes after it can be acknowledged as durable.
pub struct Shipper {
    /// The node's name; its logs are `<node>.<random>`.
    pub node: String,
    pub objects: Objects,
    cfg: ShipperConfig,
    log: Mutex<String>,
    pending: Mutex<Pending>,
    kick: Notify,
    dbs: Mutex<HashMap<String, DbLog>>,
    next_seq: AtomicU64,
    flush_lock: tokio::sync::Mutex<()>,
    /// Serializes manifest writes (each compares against the version the last one left).
    manifest_lock: tokio::sync::Mutex<()>,
    fenced: std::sync::OnceLock<Box<dyn Fn(&str) + Send + Sync>>,
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

/// `data` without the frames of databases `keep` refuses.
fn filter_frames(data: &[u8], keep: impl Fn(&str) -> bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for f in frames(data) {
        if keep(f.db) {
            put_frame(&mut out, f.db, f.start, f.at_ms, f.data);
        }
    }
    out
}

impl Shipper {
    pub async fn start(node: impl Into<String>, objects: Objects, cfg: ShipperConfig) -> anyhow::Result<Arc<Self>> {
        let node = node.into();
        let me = Arc::new(Self {
            log: Mutex::new(new_log(&node)),
            node,
            objects,
            cfg,
            pending: Mutex::new(Pending::default()),
            kick: Notify::new(),
            dbs: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(1),
            flush_lock: tokio::sync::Mutex::new(()),
            manifest_lock: tokio::sync::Mutex::new(()),
            fenced: std::sync::OnceLock::new(),
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

    /// The log this node writes segments to now (`<node>.<random>`).
    pub fn log(&self) -> String {
        self.log.lock().clone()
    }

    /// Last flushed segment number of the current log.
    pub fn last_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst) - 1
    }

    /// Called with a database's id when another node took it over (its manifest changed under
    /// this one): it is no longer shipped, and must stop serving at once.
    pub fn on_fenced(&self, f: impl Fn(&str) + Send + Sync + 'static) {
        let _ = self.fenced.set(Box::new(f));
    }

    fn fence_db(&self, id: &str) {
        if self.dbs.lock().remove(id).is_some() {
            tracing::warn!(db = id, "another node took this database over; dropping it here");
            if let Some(f) = self.fenced.get() {
                f(id);
            }
        }
    }

    /// Start shipping `db`'s writes. `manifest` is what it was restored from ([`restore`]): it's
    /// rewritten to name this node's log only if nobody else wrote it since.
    pub async fn attach(self: &Arc<Self>, db: &Arc<Database>, mut manifest: Manifest) -> anyhow::Result<()> {
        // No flush in between: every frame of the database lands in a segment it records.
        let _serial = self.flush_lock.lock().await;
        let _one = self.manifest_lock.lock().await;
        let sink = Arc::new(ShipSink { shipper: self.clone(), db: db.id.clone() });
        let (feed_id, offset) = db.subscribe(sink);
        let log = self.log();
        manifest.end = offset;
        manifest.node = Some(log.clone());
        manifest.after_seq = self.last_seq();
        // Record ownership right away so a crash before the first lazy manifest is recoverable.
        let version = match self.cas_manifest(&db.id, &manifest).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                db.unsubscribe(feed_id);
                anyhow::bail!("database {} was taken over by another node while it loaded", db.id);
            }
            Err(e) => {
                db.unsubscribe(feed_id);
                return Err(e);
            }
        };
        manifest.version = Some(version);
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
        self.dbs.lock().insert(db.id.clone(), DbLog { manifest, durable, dirty: false, feed_id, named: Some(log) });
        Ok(())
    }

    /// Stop shipping: flush, write the final manifest. Call after the database stopped serving.
    /// The final manifest names no log: everything is in the segments it lists, so the next
    /// owner has nothing to fence.
    pub async fn detach(&self, db: &Database) -> anyhow::Result<Manifest> {
        self.flush().await?;
        let _one = self.manifest_lock.lock().await;
        let log = self.dbs.lock().remove(&db.id);
        let Some(log) = log else { anyhow::bail!("database {} is not attached", db.id) };
        db.unsubscribe(log.feed_id);
        *db.durability.lock() = None;
        let mut m = log.manifest;
        m.node = None;
        m.after_seq = 0;
        match self.cas_manifest(&db.id, &m).await {
            Ok(Some(v)) => m.version = Some(v),
            Ok(None) => tracing::warn!(db = db.id, "another node took this database over while it unloaded"),
            // Still named by its manifest: the next owner fences this log and replays it.
            Err(e) => tracing::warn!(db = db.id, error = %e, "the final manifest write failed; the next owner recovers from the log"),
        }
        Ok(m)
    }

    /// Stop shipping a database without a final manifest (it couldn't be unloaded cleanly): its
    /// manifest still names this node's log, so whoever loads it next fences the log and replays
    /// what was flushed.
    pub fn abandon(&self, db: &Database) {
        if let Some(log) = self.dbs.lock().remove(&db.id) {
            db.unsubscribe(log.feed_id);
        }
        *db.durability.lock() = None;
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
        loop {
            let mut p = std::mem::take(&mut *self.pending.lock());
            if p.data.is_empty() {
                return Ok(());
            }
            let log = self.log();
            // Databases about to write into this log for the first time get manifests naming it.
            let stale: Vec<String> = {
                let dbs = self.dbs.lock();
                p.ends.keys().filter(|id| dbs.get(*id).is_some_and(|l| l.named.as_ref() != Some(&log))).cloned().collect()
            };
            for id in &stale {
                if let Err(e) = self.store_manifest(id, |_| {}).await {
                    self.requeue(p);
                    return Err(e);
                }
            }
            // Frames of databases taken over (now, or while they waited) stay out of the log.
            let gone = {
                let dbs = self.dbs.lock();
                p.ends.keys().any(|id| !dbs.contains_key(id))
            };
            if gone {
                let dbs = self.dbs.lock();
                p.data = filter_frames(&p.data, |db| dbs.contains_key(db));
                p.ends.retain(|id, _| dbs.contains_key(id));
                if p.data.is_empty() {
                    continue;
                }
            }
            let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
            let level = self.cfg.zstd_level;
            let raw = p.data.clone();
            let data = if raw.len() > 64 << 10 {
                tokio::task::spawn_blocking(move || zstd::encode_all(&raw[..], level)).await??
            } else {
                zstd::encode_all(&raw[..], level)?
            };
            let key = segment_key(&log, seq);
            let data = Bytes::from(data);
            if !self.put_segment(&key, &data).await {
                // Another node took a database of this log over and fenced it: on to a new log.
                // These frames go there, behind manifests that name it.
                let next = new_log(&self.node);
                tracing::warn!(log, seq, next, "this node's redis log was fenced by a takeover; continuing on a new log");
                *self.log.lock() = next;
                self.next_seq.store(1, Ordering::SeqCst);
                self.requeue(p);
                continue;
            }
            self.bytes_put.fetch_add(data.len() as u64, Ordering::Relaxed);
            self.objects_put.fetch_add(1, Ordering::Relaxed);
            let mut dbs = self.dbs.lock();
            for (db, end) in p.ends {
                let Some(l) = dbs.get_mut(&db) else { continue };
                match l.manifest.segments.last_mut() {
                    Some(last) if last.node == log && last.seq == seq => last.end = end,
                    _ => l.manifest.segments.push(SegRef { node: log.clone(), seq, end }),
                }
                l.manifest.end = end;
                l.dirty = true;
                l.durable.send_replace(end);
            }
            return Ok(());
        }
    }

    /// Put frames back in front of whatever was buffered since.
    fn requeue(&self, p: Pending) {
        let mut cur = self.pending.lock();
        let mut data = p.data;
        data.extend_from_slice(&cur.data);
        cur.data = data;
        for (db, end) in p.ends {
            cur.ends.entry(db).or_insert(end);
        }
    }

    /// Write a segment create-only, retrying until storage answers. False: the key is taken by
    /// something else (a fence).
    async fn put_segment(&self, key: &str, data: &Bytes) -> bool {
        let mut backoff = Duration::from_millis(50);
        loop {
            let r = match self.objects.put_new(key, data.clone()).await {
                Ok(true) => Ok(true),
                // Ours, if an earlier attempt's answer was lost.
                Ok(false) => self.objects.get(key).await.map(|got| got.as_ref() == Some(data)),
                Err(e) => Err(e),
            };
            match r {
                Ok(ours) => return ours,
                Err(e) => {
                    tracing::warn!(error = %e, key, "segment upload failed, retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    /// Compare-and-swap `id`'s manifest from the version in `m`. `None`: someone else wrote it.
    async fn cas_manifest(&self, id: &str, m: &Manifest) -> anyhow::Result<Option<Version>> {
        let raw = Bytes::from(serde_json::to_vec(m)?);
        let key = manifest_key(id);
        if let Some(v) = self.objects.put_if(&key, raw.clone(), m.version.as_ref()).await? {
            return Ok(Some(v));
        }
        // This very write, if an earlier attempt's answer was lost.
        Ok(match self.objects.get_versioned(&key).await? {
            Some((got, v)) if got == raw => Some(v),
            _ => None,
        })
    }

    /// Store `id`'s manifest as this node knows it (after `edit`), naming the current log. False
    /// if the database isn't attached, or was taken over (it's dropped).
    async fn store_manifest(&self, id: &str, edit: impl FnOnce(&mut Manifest)) -> anyhow::Result<bool> {
        let _one = self.manifest_lock.lock().await;
        let log = self.log();
        let m = {
            let mut dbs = self.dbs.lock();
            let Some(l) = dbs.get_mut(id) else { return Ok(false) };
            edit(&mut l.manifest);
            l.manifest.node = Some(log.clone());
            l.manifest.after_seq = self.last_seq();
            l.dirty = false;
            l.manifest.clone()
        };
        match self.cas_manifest(id, &m).await {
            Ok(Some(v)) => {
                if let Some(l) = self.dbs.lock().get_mut(id) {
                    l.manifest.version = Some(v);
                    l.named = Some(log);
                }
                Ok(true)
            }
            Ok(None) => {
                self.fence_db(id);
                Ok(false)
            }
            Err(e) => {
                if let Some(l) = self.dbs.lock().get_mut(id) {
                    l.dirty = true;
                }
                Err(e)
            }
        }
    }

    /// Rewrite manifests of databases that changed (all of them with `force`).
    pub async fn write_manifests(&self, force: bool) -> anyhow::Result<()> {
        let after = self.last_seq();
        let todo: Vec<String> = {
            let dbs = self.dbs.lock();
            dbs.iter()
                // An idle database's stale `after_seq` would keep this node's segments alive.
                .filter(|(_, l)| force || l.dirty || after.saturating_sub(l.manifest.after_seq) >= MANIFEST_LAG)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in todo {
            self.store_manifest(&id, |_| {}).await?;
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
        // Named after this node's log too: two nodes that both thought they owned the database
        // never write the same snapshot key.
        let key = snapshot_key(&db.id, offset, &self.log());
        let size = data.len() as u64;
        self.objects.put(&key, data).await?;
        self.bytes_put.fetch_add(size, Ordering::Relaxed);
        self.objects_put.fetch_add(1, Ordering::Relaxed);
        // The log up to the snapshot is in storage too, so the manifest stays consistent.
        self.flush().await?;
        let snap = SnapRef { key, offset, at_ms };
        self.snapshotted(&db.id, snap.clone(), retain_ms);
        self.store_manifest(&db.id, |_| {}).await?;
        Ok(Some(snap))
    }

    /// Record a snapshot. Snapshots older than `retain_ms` (but always the newest) are dropped
    /// from the manifest, with the segments only they needed. Returns the dropped snapshots.
    pub fn snapshotted(&self, db: &str, snap: SnapRef, retain_ms: i64) -> Vec<SnapRef> {
        let mut dbs = self.dbs.lock();
        let Some(l) = dbs.get_mut(db) else { return Vec::new() };
        let horizon = snap.at_ms - retain_ms;
        // Two snapshots of the same point (a periodic one racing the one on unload).
        if l.manifest.snapshots.last().is_some_and(|s| s.offset == snap.offset) {
            return Vec::new();
        }
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

/// A database's manifest, with the version a compare-and-swap of it needs.
pub async fn read_manifest(objects: &Objects, id: &str) -> anyhow::Result<Option<Manifest>> {
    match objects.get_versioned(&manifest_key(id)).await? {
        Some((raw, v)) => Ok(Some(Manifest { version: Some(v), ..serde_json::from_slice(&raw)? })),
        None => Ok(None),
    }
}

/// Write a manifest that must not exist yet (a new branch, an archive). False: it does.
pub async fn create_manifest(objects: &Objects, id: &str, m: &Manifest) -> anyhow::Result<bool> {
    Ok(objects.put_if(&manifest_key(id), Bytes::from(serde_json::to_vec(m)?), None).await?.is_some())
}

/// Replace a manifest read as `m.version` (`None`: one that doesn't exist yet). False: it
/// changed meanwhile.
pub async fn replace_manifest(objects: &Objects, id: &str, m: &Manifest) -> anyhow::Result<bool> {
    Ok(objects.put_if(&manifest_key(id), Bytes::from(serde_json::to_vec(m)?), m.version.as_ref()).await?.is_some())
}

/// Segments of `log` after `after_seq` (and before `fence`), in order, without fences.
pub async fn later_segments(objects: &Objects, log: &str, after_seq: u64, fence: Option<u64>) -> anyhow::Result<Vec<u64>> {
    let mut later: Vec<u64> = objects
        .list(&format!("redis/seg/{log}/"))
        .await?
        .iter()
        .filter(|(_, size)| *size > 0)
        .filter_map(|(k, _)| seq_of(k))
        .filter(|s| *s > after_seq && fence.is_none_or(|f| *s < f))
        .collect();
    later.sort_unstable();
    Ok(later)
}

/// Fence `log` (a database's last owner's): put an empty object at its next segment number, so
/// its writer can't add to it any more. Returns that number: every segment of the log below it
/// is all there will ever be. A log fenced already keeps its fence.
pub async fn fence_log(objects: &Objects, log: &str, after_seq: u64) -> anyhow::Result<u64> {
    loop {
        let listed: Vec<(u64, u64)> = objects.list(&format!("redis/seg/{log}/")).await?.iter().filter_map(|(k, size)| Some((seq_of(k)?, *size))).collect();
        if let Some(fence) = listed.iter().filter(|(_, size)| *size == 0).map(|(s, _)| *s).min() {
            return Ok(fence);
        }
        let next = listed.iter().map(|(s, _)| *s).max().unwrap_or(0).max(after_seq) + 1;
        if objects.put_new(&segment_key(log, next), Bytes::new()).await? {
            return Ok(next);
        }
        // The writer got there first (or this very fence, its answer lost): look again.
    }
}

/// Load a database's latest state from object storage to serve it (the node is taking it on).
/// The log its manifest names (its last owner's, which may still be running) is fenced first
/// ([`fence_log`]), so the state loaded holds every write that owner ever made durable. Returns
/// its manifest, with `end` set to the restored offset and the version [`Shipper::attach`]
/// compares against (a database without a manifest starts empty).
pub async fn restore(objects: &Objects, db: &Arc<Database>, codec: Option<&dyn SnapshotCodec>) -> anyhow::Result<Manifest> {
    let id = db.id.clone();
    let mut st = Replay { applied: 0, dbsel: 0, last_ms: 0 };
    let mut m = match read_manifest(objects, &id).await? {
        Some(m) => {
            let fence = match &m.node {
                Some(log) => Some(fence_log(objects, log, m.after_seq).await?),
                None => None,
            };
            let version = m.version.clone();
            let mut m = replay(objects, &id, m, db, Until::LATEST, codec, &mut st, fence).await?;
            m.version = version;
            m
        }
        None => Manifest::default(),
    };
    db.set_offset(st.applied);
    m.end = st.applied;
    Ok(m)
}

/// Load the state of database `source` as of `until` into `db` (point-in-time restore when
/// `source` is `db`'s own id, branching when it's another database), reading only: the newest
/// snapshot the target allows, then log frames up to the target. Branches restore their parent's
/// history up to the branch point first. Also reads the segments that the last owning node wrote
/// after the manifest (crash recovery, or a primary that is running).
pub async fn restore_at(objects: &Objects, source: &str, db: &Arc<Database>, until: Until, codec: Option<&dyn SnapshotCodec>) -> anyhow::Result<Manifest> {
    let mut st = Replay { applied: 0, dbsel: 0, last_ms: 0 };
    let mut m = match read_manifest(objects, source).await? {
        Some(m) => replay(objects, source, m, db, until, codec, &mut st, None).await?,
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

#[allow(clippy::too_many_arguments)]
fn replay<'a>(
    objects: &'a Objects,
    id: &'a str,
    mut m: Manifest,
    db: &'a Arc<Database>,
    until: Until,
    codec: Option<&'a dyn SnapshotCodec>,
    st: &'a mut Replay,
    fence: Option<u64>,
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
            replay(objects, &base.db, parent, db, lim, codec, st, None).await?;
            if st.applied < base.offset && until.offset.is_none_or(|o| o >= base.offset) && until.time_ms.is_none() {
                anyhow::bail!("{id}: parent {} history ends at {} before the branch point {}", base.db, st.applied, base.offset);
            }
        }
        let mut segs = m.segments.clone();
        if let Some(node) = &m.node {
            let later = later_segments(objects, node, m.after_seq, fence).await?;
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

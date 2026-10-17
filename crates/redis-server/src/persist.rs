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

use crate::database::{Database, FeedSink};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use parking_lot::Mutex;
use redis_engine::Effect;
use redis_engine::commands::eq;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self { flush_interval: Duration::from_secs(1), flush_bytes: 8 << 20, manifest_interval: Duration::from_secs(30), zstd_level: 1 }
    }
}

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
                .filter(|(_, l)| force || l.dirty)
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
pub async fn restore(objects: &Objects, db: &Arc<Database>, load_snapshot: Option<&SnapshotLoader>) -> anyhow::Result<Manifest> {
    let id = db.id.clone();
    restore_at(objects, &id, db, Until::LATEST, load_snapshot).await
}

/// Load the state of database `source` as of `until` into `db` (point-in-time restore when
/// `source` is `db`'s own id, branching when it's another database): the newest snapshot the
/// target allows, then log frames up to the target. Branches restore their parent's history up
/// to the branch point first. Also scans the segments that the last owning node wrote after the
/// manifest (crash recovery).
pub async fn restore_at(objects: &Objects, source: &str, db: &Arc<Database>, until: Until, load_snapshot: Option<&SnapshotLoader>) -> anyhow::Result<Manifest> {
    let mut st = Replay { applied: 0, dbsel: 0, last_ms: 0 };
    let m = replay(objects, source, db, until, load_snapshot, &mut st).await?;
    db.set_offset(st.applied);
    let mut m = m;
    m.end = st.applied;
    Ok(m)
}

struct Replay {
    applied: u64,
    dbsel: u16,
    last_ms: i64,
}

fn replay<'a>(
    objects: &'a Objects,
    id: &'a str,
    db: &'a Arc<Database>,
    until: Until,
    load_snapshot: Option<&'a SnapshotLoader>,
    st: &'a mut Replay,
) -> futures::future::BoxFuture<'a, anyhow::Result<Manifest>> {
    Box::pin(async move {
        let Some(mut m) = read_manifest(objects, id).await? else {
            return Ok(Manifest::default());
        };
        let stream = m.stream.clone().unwrap_or_else(|| id.to_string());
        let id = stream.as_str();
        if let Some(snap) = m.snapshots.iter().rev().find(|s| until.allows_snap(s)).cloned() {
            let loader = load_snapshot.ok_or_else(|| anyhow::anyhow!("snapshot {} needs a snapshot loader", snap.key))?;
            let data = objects.get(&snap.key).await?.ok_or_else(|| anyhow::anyhow!("snapshot {} is missing", snap.key))?;
            let data = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
            loader(db, &data)?;
            st.applied = snap.offset;
            st.last_ms = snap.at_ms;
            st.dbsel = 0;
        } else if !m.snapshots.is_empty() {
            anyhow::bail!("{id}: the requested point is older than the retained history");
        } else if let Some(base) = &m.base {
            let lim = Until { offset: Some(until.offset.map_or(base.offset, |o| o.min(base.offset))), time_ms: until.time_ms };
            replay(objects, &base.db, db, lim, load_snapshot, st).await?;
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
        let mut seen = std::collections::HashSet::new();
        'segs: for seg in segs {
            if !seen.insert((seg.node.clone(), seg.seq)) {
                continue;
            }
            // Listed segments end before what's already applied: skip without fetching.
            if seg.end != 0 && seg.end <= st.applied {
                continue;
            }
            let key = segment_key(&seg.node, seg.seq);
            let Some(data) = objects.get(&key).await? else {
                anyhow::bail!("segment {key} is missing");
            };
            let raw = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
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
                break 'segs;
            }
        }
        Ok(m)
    })
}

/// Loads a decompressed snapshot into a database.
pub type SnapshotLoader = dyn Fn(&Arc<Database>, &[u8]) -> anyhow::Result<()> + Send + Sync;

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
    pub snapshot: Option<SnapRef>,
    /// Segments holding the database's frames after the snapshot, in order.
    pub segments: Vec<SegRef>,
    /// Stream offset covered by the snapshot and segments.
    pub end: u64,
    /// The node that owned the database when this was written: its segments after `after_seq`
    /// may hold newer frames.
    pub node: Option<String>,
    pub after_seq: u64,
}

pub fn manifest_key(db: &str) -> String {
    format!("redis/db/{db}/manifest.json")
}

pub fn segment_key(node: &str, seq: u64) -> String {
    format!("redis/seg/{node}/{seq:016}.zst")
}

/// One frame per (database, flush) in a segment.
fn put_frame(out: &mut Vec<u8>, db: &str, start: u64, data: &[u8]) {
    out.put_u16(db.len() as u16);
    out.put_slice(db.as_bytes());
    out.put_u64(start);
    out.put_u32(data.len() as u32);
    out.put_slice(data);
}

/// Iterate `(db, start offset, data)` frames of a decompressed segment.
pub fn frames(mut seg: &[u8]) -> impl Iterator<Item = (&str, u64, &[u8])> {
    std::iter::from_fn(move || {
        if seg.len() < 2 {
            return None;
        }
        let n = seg.get_u16() as usize;
        let db = std::str::from_utf8(seg.get(..n)?).ok()?;
        seg.advance(n);
        if seg.len() < 12 {
            return None;
        }
        let start = seg.get_u64();
        let len = seg.get_u32() as usize;
        let data = seg.get(..len)?;
        seg.advance(len);
        Some((db, start, data))
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
            put_frame(&mut p.data, &self.db, end - chunk.len() as u64, chunk);
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

    /// Replace the manifest after a snapshot covering everything up to `snap.offset`: older
    /// segments are no longer needed by this database.
    pub fn snapshotted(&self, db: &str, snap: SnapRef) {
        if let Some(l) = self.dbs.lock().get_mut(db) {
            l.manifest.segments.retain(|s| s.end > snap.offset);
            l.manifest.snapshot = Some(snap);
            l.dirty = true;
        }
    }
}

fn seq_of(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.strip_suffix(".zst")?.parse().ok()
}

/// Load a database's state from object storage: its manifest's snapshot and segments, plus any
/// later segments of the node that last owned it (crash recovery). Returns the manifest, with
/// `end` set to the restored offset. A database without a manifest starts empty.
pub async fn restore(objects: &Objects, db: &Arc<Database>, load_snapshot: Option<&SnapshotLoader>) -> anyhow::Result<Manifest> {
    let Some(raw) = objects.get(&manifest_key(&db.id)).await? else {
        return Ok(Manifest::default());
    };
    let mut m: Manifest = serde_json::from_slice(&raw)?;
    let mut applied = 0u64;
    if let Some(snap) = &m.snapshot {
        let loader = load_snapshot.ok_or_else(|| anyhow::anyhow!("snapshot {} needs a snapshot loader", snap.key))?;
        let data = objects.get(&snap.key).await?.ok_or_else(|| anyhow::anyhow!("snapshot {} is missing", snap.key))?;
        let data = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
        loader(db, &data)?;
        applied = snap.offset;
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
    let mut dbsel = 0u16;
    for seg in segs {
        if !seen.insert((seg.node.clone(), seg.seq)) {
            continue;
        }
        let key = segment_key(&seg.node, seg.seq);
        let Some(data) = objects.get(&key).await? else {
            anyhow::bail!("segment {key} is missing");
        };
        let raw = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
        let mut effects = Vec::new();
        let mut found = None;
        for (id, start, data) in frames(&raw) {
            if id != db.id {
                continue;
            }
            let end = start + data.len() as u64;
            if end <= applied {
                continue;
            }
            if start != applied {
                anyhow::bail!("log gap for {}: have {applied}, segment {key} continues at {start}", db.id);
            }
            parse_stream(data, &mut dbsel, &mut effects)?;
            applied = end;
            found = Some(end);
        }
        if let Some(end) = found
            && !m.segments.iter().any(|s| s.node == seg.node && s.seq == seg.seq)
        {
            m.segments.push(SegRef { end, ..seg });
        }
        if !effects.is_empty() {
            db.engine(0).apply_all(&effects);
        }
    }
    db.set_offset(applied);
    m.end = applied;
    Ok(m)
}

/// Loads a decompressed snapshot into a database.
pub type SnapshotLoader = dyn Fn(&Arc<Database>, &[u8]) -> anyhow::Result<()> + Send + Sync;

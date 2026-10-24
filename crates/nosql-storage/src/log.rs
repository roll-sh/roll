//! The commit log: every write of every database on a node, shipped to object storage.
//!
//! Writes append a *frame* to the node's buffer; the [`Shipper`] uploads the buffer as one
//! zstd-compressed segment every `flush_interval` (~100 ms), or sooner when it fills. PUTs
//! scale with nodes, not with databases. A frame carries the database id, its log position
//! (`lsn`, per database), the wall-clock time (point-in-time restore granularity) and the
//! serialised partition updates of one write or batch:
//!
//! ```text
//! frame   := db:u16+bytes lsn:u64 at_ms:i64 len:u32 payload crc32c:u32
//! payload := n:uv (table:16 bytes  update:uv+bytes)*
//! ```
//!
//! With `durability = async` a write is acknowledged once it is in the memtable; with `sync`
//! once its segment is stored ([`Shipper::wait_stored`]). Follows the Redis product's
//! `redis_server::persist::Shipper`.

use crate::codec::{Reader, put_bytes, put_uvarint};
use crate::manifest::{SegRef, TableId, segment_key, segment_seq};
use anyhow::{Result, bail};
use bytes::{BufMut, Bytes};
use parking_lot::Mutex;
use pg_storage::Objects;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{Notify, watch};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Durability {
    /// Acknowledge once applied to the memtable (loss bounded by the ship interval).
    #[default]
    Async,
    /// Acknowledge once the segment holding the write is stored.
    Sync,
}

#[derive(Clone, Debug)]
pub struct LogConfig {
    pub flush_interval: Duration,
    pub flush_bytes: usize,
    pub zstd_level: i32,
    /// Frames buffered while shipping lags (slow object storage) before writes wait.
    pub max_pending_bytes: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { flush_interval: Duration::from_millis(100), flush_bytes: 4 << 20, zstd_level: 1, max_pending_bytes: 64 << 20 }
    }
}

/// One decoded frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub db: String,
    pub lsn: u64,
    pub at_ms: i64,
    pub payload: Bytes,
}

pub fn put_frame(out: &mut Vec<u8>, db: &str, lsn: u64, at_ms: i64, payload: &[u8]) {
    put_frame_parts(out, db, lsn, at_ms, &[payload]);
}

/// A frame whose payload is the concatenation of `parts` (no intermediate copy).
pub fn put_frame_parts(out: &mut Vec<u8>, db: &str, lsn: u64, at_ms: i64, parts: &[&[u8]]) {
    let start = out.len();
    out.put_u16(db.len() as u16);
    out.put_slice(db.as_bytes());
    out.put_u64(lsn);
    out.put_i64(at_ms);
    out.put_u32(parts.iter().map(|p| p.len()).sum::<usize>() as u32);
    for p in parts {
        out.put_slice(p);
    }
    let crc = crc32c::crc32c(&out[start..]);
    out.put_u32(crc);
}

/// Decode every frame of a decompressed segment. A corrupt frame is an error.
pub fn frames(seg: &Bytes) -> Result<Vec<Frame>> {
    let mut out = Vec::new();
    let mut r = Reader::new(seg);
    while !r.is_empty() {
        let start = r.pos();
        let n = u16::from_be_bytes(r.take(2)?.try_into().expect("2"));
        let db = std::str::from_utf8(r.take(n as usize)?).map_err(|_| anyhow::anyhow!("corrupt log: bad database id"))?.to_string();
        let lsn = u64::from_be_bytes(r.take(8)?.try_into().expect("8"));
        let at_ms = i64::from_be_bytes(r.take(8)?.try_into().expect("8"));
        let len = u32::from_be_bytes(r.take(4)?.try_into().expect("4")) as usize;
        let body = r.take(len)?;
        let end = r.pos();
        let crc = u32::from_be_bytes(r.take(4)?.try_into().expect("4"));
        if crc32c::crc32c(&seg[start..end]) != crc {
            bail!("corrupt log: frame checksum mismatch");
        }
        out.push(Frame { db, lsn, at_ms, payload: seg.slice_ref(body) });
    }
    Ok(out)
}

/// Encode the updates of one write: `(table, serialised update)`.
pub fn encode_payload(updates: &[(TableId, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(updates.iter().map(|(_, u)| u.len() + 20).sum::<usize>() + 2);
    put_uvarint(&mut out, updates.len() as u64);
    for (t, u) in updates {
        out.extend_from_slice(&t.0.to_be_bytes());
        put_bytes(&mut out, u);
    }
    out
}

/// The `(table, update bytes)` pairs of a payload (zero-copy).
pub fn decode_payload(payload: &Bytes) -> Result<Vec<(TableId, Bytes)>> {
    let mut r = Reader::new(payload);
    let n = r.count(17)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let t = TableId(u128::from_be_bytes(r.take(16)?.try_into().expect("16")));
        let b = r.bytes()?;
        out.push((t, payload.slice_ref(b)));
    }
    if !r.is_empty() {
        bail!("corrupt log: trailing bytes in a frame");
    }
    Ok(out)
}

/// Receives the segments a database's frames were shipped in.
pub trait SegmentSink: Send + Sync {
    fn shipped(&self, seg: SegRef);
}

#[derive(Default)]
struct Pending {
    data: Vec<u8>,
    /// Per database: (first lsn, last lsn, last ms) in `data`.
    dbs: HashMap<String, (u64, u64, i64)>,
    /// The generation the buffer will ship as.
    generation: u64,
}

/// The node's log shipper.
pub struct Shipper {
    pub node: String,
    pub objects: Objects,
    cfg: LogConfig,
    pending: Mutex<Pending>,
    kick: Arc<Notify>,
    next_seq: AtomicU64,
    flush_lock: tokio::sync::Mutex<()>,
    /// The newest generation stored.
    stored: watch::Sender<u64>,
    /// The newest segment whose databases have been told about it.
    completed_seq: AtomicU64,
    sinks: Mutex<HashMap<String, Weak<dyn SegmentSink>>>,
    pub bytes_put: AtomicU64,
    pub segments_put: AtomicU64,
    /// Fail uploads (tests of crash behaviour).
    #[cfg(test)]
    pub fail: std::sync::atomic::AtomicBool,
}

impl Shipper {
    /// Start a node's shipper and its background flush loop.
    pub async fn start(node: impl Into<String>, objects: Objects, cfg: LogConfig) -> Result<Arc<Self>> {
        let node = node.into();
        let existing = objects.list(&format!("nosql/log/{node}/")).await?;
        let next = existing.iter().filter_map(|(k, _)| segment_seq(k)).max().map_or(1, |s| s + 1);
        let (stored, _) = watch::channel(0);
        let me = Arc::new(Self {
            node,
            objects,
            cfg,
            pending: Mutex::new(Pending { generation: 1, ..Default::default() }),
            kick: Arc::new(Notify::new()),
            next_seq: AtomicU64::new(next),
            flush_lock: tokio::sync::Mutex::new(()),
            stored,
            completed_seq: AtomicU64::new(next - 1),
            sinks: Mutex::new(HashMap::new()),
            bytes_put: AtomicU64::new(0),
            segments_put: AtomicU64::new(0),
            #[cfg(test)]
            fail: std::sync::atomic::AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&me);
        let interval = me.cfg.flush_interval;
        let kick = me.kick.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = kick.notified() => {}
                }
                let Some(s) = weak.upgrade() else { return };
                if let Err(e) = s.flush().await {
                    tracing::warn!(error = %e, "nosql log flush failed, retrying");
                }
            }
        });
        Ok(me)
    }

    /// The last segment number used.
    pub fn last_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst) - 1
    }

    /// Every segment up to this one is stored and reported to its databases' manifests: a
    /// manifest's `after_seq`.
    pub fn completed_seq(&self) -> u64 {
        self.completed_seq.load(Ordering::SeqCst)
    }

    pub fn register(&self, db: &str, sink: Weak<dyn SegmentSink>) {
        self.sinks.lock().insert(db.to_string(), sink);
    }

    pub fn unregister(&self, db: &str) {
        self.sinks.lock().remove(db);
    }

    /// Append a frame. Returns the generation to wait for with [`Shipper::wait_stored`].
    pub fn append(&self, db: &str, lsn: u64, at_ms: i64, payload: &[u8]) -> u64 {
        self.append_parts(db, lsn, at_ms, &[payload])
    }

    /// [`Shipper::append`] with the payload in pieces.
    pub fn append_parts(&self, db: &str, lsn: u64, at_ms: i64, parts: &[&[u8]]) -> u64 {
        let (generation, big) = {
            let mut p = self.pending.lock();
            put_frame_parts(&mut p.data, db, lsn, at_ms, parts);
            let e = p.dbs.entry(db.to_string()).or_insert((lsn, lsn, at_ms));
            e.0 = e.0.min(lsn);
            e.1 = e.1.max(lsn);
            e.2 = e.2.max(at_ms);
            (p.generation, p.data.len() >= self.cfg.flush_bytes)
        };
        if big {
            self.kick.notify_one();
        }
        generation
    }

    /// Bytes of frames waiting to ship.
    pub fn pending_bytes(&self) -> usize {
        self.pending.lock().data.len()
    }

    pub fn max_pending_bytes(&self) -> usize {
        self.cfg.max_pending_bytes
    }

    /// Wait while more than `max_pending_bytes` wait to ship, at most `timeout`
    /// ([`crate::memory::Overloaded`]).
    pub async fn admit(&self, timeout: Duration) -> Result<()> {
        if self.pending_bytes() < self.cfg.max_pending_bytes {
            return Ok(());
        }
        let started = tokio::time::Instant::now();
        let mut rx = self.stored.subscribe();
        self.kick();
        while self.pending_bytes() >= self.cfg.max_pending_bytes {
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() || tokio::time::timeout(left.min(Duration::from_millis(50)), rx.changed()).await.is_ok_and(|r| r.is_err()) {
                return Err(crate::memory::Overloaded { waited_ms: started.elapsed().as_millis() as u64 }.into());
            }
        }
        Ok(())
    }

    /// Ask for an early flush.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Wait until the frames of `generation` are stored.
    pub async fn wait_stored(&self, generation: u64) -> Result<()> {
        let mut rx = self.stored.subscribe();
        if *rx.borrow() >= generation {
            return Ok(());
        }
        self.kick();
        rx.wait_for(|g| *g >= generation).await.map_err(|_| anyhow::anyhow!("log shipper stopped"))?;
        Ok(())
    }

    /// Ship everything buffered now as one segment (retrying until stored).
    pub async fn flush(&self) -> Result<()> {
        let _serial = self.flush_lock.lock().await;
        let p = {
            let mut p = self.pending.lock();
            let next = Pending { generation: p.generation + 1, ..Default::default() };
            std::mem::replace(&mut *p, next)
        };
        if p.data.is_empty() {
            self.stored.send_replace(p.generation);
            return Ok(());
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let level = self.cfg.zstd_level;
        let raw = p.data;
        let data = if raw.len() > 64 << 10 {
            tokio::task::spawn_blocking(move || zstd::bulk::compress(&raw, level)).await??
        } else {
            zstd::bulk::compress(&raw, level)?
        };
        let key = segment_key(&self.node, seq);
        let data = Bytes::from(data);
        let mut backoff = Duration::from_millis(20);
        loop {
            #[cfg(test)]
            let r = if self.fail.load(Ordering::SeqCst) { Err(anyhow::anyhow!("injected failure")) } else { self.objects.put(&key, data.clone()).await };
            #[cfg(not(test))]
            let r = self.objects.put(&key, data.clone()).await;
            match r {
                Ok(()) => break,
                Err(e) => {
                    tracing::warn!(error = %e, key, "nosql log segment upload failed, retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
        self.bytes_put.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.segments_put.fetch_add(1, Ordering::Relaxed);
        let sinks: Vec<(SegRef, Option<Arc<dyn SegmentSink>>)> = {
            let s = self.sinks.lock();
            p.dbs
                .into_iter()
                .map(|(db, (first, last, ms))| {
                    let sink = s.get(&db).and_then(|w| w.upgrade());
                    (SegRef { node: self.node.clone(), seq, first_lsn: first, last_lsn: last, last_ms: ms }, sink)
                })
                .collect()
        };
        for (seg, sink) in sinks {
            if let Some(sink) = sink {
                sink.shipped(seg);
            }
        }
        self.completed_seq.store(seq, Ordering::SeqCst);
        self.stored.send_replace(p.generation);
        Ok(())
    }
}

/// Fetch and decode one segment.
pub async fn read_segment(objects: &Objects, node: &str, seq: u64) -> Result<Option<Vec<Frame>>> {
    let key = segment_key(node, seq);
    let Some(data) = objects.get(&key).await? else { return Ok(None) };
    let raw = tokio::task::spawn_blocking(move || zstd::stream::decode_all(&data[..])).await?.map_err(|e| anyhow::anyhow!("corrupt segment {key}: {e}"))?;
    let raw = Bytes::from(raw);
    Ok(Some(frames(&raw).map_err(|e| anyhow::anyhow!("segment {}: {e}", segment_key(node, seq)))?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_detect_corruption() {
        let payload = encode_payload(&[(TableId(7), b"abc".to_vec()), (TableId(9), Vec::new())]);
        let mut seg = Vec::new();
        put_frame(&mut seg, "db1", 5, 1000, &payload);
        put_frame(&mut seg, "db2", 1, 1001, b"");
        let b = Bytes::from(seg.clone());
        let f = frames(&b).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].db.as_str(), f[0].lsn, f[0].at_ms), ("db1", 5, 1000));
        let p = decode_payload(&f[0].payload).unwrap();
        assert_eq!(p, vec![(TableId(7), Bytes::from_static(b"abc")), (TableId(9), Bytes::new())]);
        for i in 0..seg.len() {
            let mut c = seg.clone();
            c[i] ^= 0x40;
            assert!(frames(&Bytes::from(c)).is_err(), "flip at {i} undetected");
        }
        assert!(frames(&Bytes::copy_from_slice(&seg[..seg.len() - 1])).is_err());
        assert!(decode_payload(&Bytes::from_static(&[5])).is_err());
    }
}

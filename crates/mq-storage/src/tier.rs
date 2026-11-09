//! Object-storage segments and the cold read cache.
//!
//! A sealed segment is uploaded as one object, `{prefix}/{log}/{first:020}.seg`, where `{log}`
//! is the log key with everything outside `[A-Za-z0-9_-]` written as `=XX`. The object holds
//! the entries in the keepers' frame format followed by a small index and a fixed footer:
//!
//! ```text
//! [len u32][crc32 u32][entry] ...              frames
//! ([index u64][offset u64]) * count            index (entry index → byte offset)
//! [count u32][magic u32][index_pos u64][first u64][end u64]
//! ```
//!
//! `end` may exceed the last entry's index + 1 when the range contains holes (S3 Express fence
//! markers), so readers always look entries up through the index.

use anyhow::{Context, bail};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use mq_core::log::crc32;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub(crate) const SEG_MAGIC: u32 = 0x5251_5347; // "RQSG"
pub(crate) const FOOTER_LEN: usize = 32;

/// Object-storage tier for [`crate::QuorumStore`].
#[derive(Clone, Debug)]
pub struct TierConfig {
    /// S3 (`object_store::aws::AmazonS3Builder`) in production; `LocalFileSystem` or
    /// `InMemory` in tests.
    pub store: Arc<dyn ObjectStore>,
    /// Key prefix inside the bucket (may be empty).
    pub prefix: String,
    /// Bytes of cold segments kept in memory for reads.
    pub cache_bytes: u64,
    /// Sealed bytes per log kept on the keepers before tiering (the hot tail stays local).
    pub keep_hot_bytes: u64,
}

impl TierConfig {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        TierConfig { store, prefix: prefix.into(), cache_bytes: 256 << 20, keep_hot_bytes: 0 }
    }
}

/// Encode a log key as one object path segment.
pub fn object_component(key: &str) -> String {
    if key.is_empty() {
        return "=".into();
    }
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("={b:02X}"));
        }
    }
    out
}

/// `{prefix}/{log}` as an object path.
pub(crate) fn log_dir(prefix: &str, key: &str) -> anyhow::Result<Path> {
    let p = prefix.trim_matches('/');
    let s = if p.is_empty() { object_component(key) } else { format!("{p}/{}", object_component(key)) };
    Path::parse(s).context("invalid object prefix")
}

pub(crate) fn seg_path(dir: &Path, first: u64) -> Path {
    dir.clone().join(format!("{first:020}.seg"))
}

/// Parse `{first:020}.seg`.
pub(crate) fn parse_seg_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".seg")?;
    (stem.len() == 20).then(|| stem.parse().ok())?
}

/// Encode entries (with their indices) as a segment object covering `[first, end)`.
pub(crate) fn encode_segment(entries: &[(u64, Bytes)], first: u64, end: u64) -> Bytes {
    let body: usize = entries.iter().map(|(_, e)| 8 + e.len()).sum();
    let mut b = BytesMut::with_capacity(body + entries.len() * 16 + FOOTER_LEN);
    let mut index = Vec::with_capacity(entries.len());
    for (i, e) in entries {
        index.push((*i, b.len() as u64));
        b.put_u32(e.len() as u32);
        b.put_u32(crc32(e));
        b.put_slice(e);
    }
    let index_pos = b.len() as u64;
    for (i, off) in &index {
        b.put_u64(*i);
        b.put_u64(*off);
    }
    b.put_u32(index.len() as u32);
    b.put_u32(SEG_MAGIC);
    b.put_u64(index_pos);
    b.put_u64(first);
    b.put_u64(end);
    b.freeze()
}

/// Footer fields: `(count, index_pos, first, end)`.
pub(crate) fn decode_footer(tail: &[u8]) -> anyhow::Result<(usize, u64, u64, u64)> {
    if tail.len() < FOOTER_LEN {
        bail!("segment too short");
    }
    let mut f = &tail[tail.len() - FOOTER_LEN..];
    let count = f.get_u32() as usize;
    if f.get_u32() != SEG_MAGIC {
        bail!("not a segment object");
    }
    Ok((count, f.get_u64(), f.get_u64(), f.get_u64()))
}

/// A decoded segment (or S3 Express batch): entries addressable by index.
pub(crate) struct SegmentData {
    pub end: u64,
    /// `(entry index, payload)` in ascending index order.
    pub entries: Vec<(u64, Bytes)>,
    pub size: u64,
}

impl SegmentData {
    pub fn decode_segment(data: Bytes) -> anyhow::Result<Self> {
        let (count, index_pos, _first, end) = decode_footer(&data)?;
        let index_end = index_pos as usize + count * 16;
        if index_end + FOOTER_LEN != data.len() {
            bail!("segment index out of bounds");
        }
        let mut idx = &data[index_pos as usize..index_end];
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let (i, off) = (idx.get_u64(), idx.get_u64() as usize);
            if off + 8 > index_pos as usize {
                bail!("segment entry out of bounds");
            }
            let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
            let crc = u32::from_be_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]);
            if off + 8 + len > index_pos as usize {
                bail!("segment entry out of bounds");
            }
            let e = data.slice(off + 8..off + 8 + len);
            if crc32(&e) != crc {
                bail!("segment entry {i} failed its checksum");
            }
            entries.push((i, e));
        }
        Ok(SegmentData { end, entries, size: data.len() as u64 })
    }

    /// Append entries from `from` onward to `out` within the limits; returns the next index to
    /// read (past the end of this segment when it was exhausted).
    pub fn read_into(&self, from: u64, max: usize, max_bytes: usize, out: &mut Vec<(u64, Bytes)>, bytes: &mut usize) -> u64 {
        let start = self.entries.partition_point(|(i, _)| *i < from);
        for (i, e) in &self.entries[start..] {
            if out.len() >= max || (*bytes > 0 && *bytes + e.len() > max_bytes) {
                return *i;
            }
            *bytes += e.len();
            out.push((*i, e.clone()));
        }
        self.end.max(from)
    }
}

/// LRU cache of decoded cold segments, bounded by bytes.
pub(crate) struct Cache {
    cap: u64,
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    map: HashMap<String, (Arc<SegmentData>, u64)>,
    order: BTreeMap<u64, String>,
    bytes: u64,
    tick: u64,
}

impl Cache {
    pub fn new(cap: u64) -> Self {
        Cache { cap, inner: Mutex::new(CacheInner::default()) }
    }

    pub fn get(&self, key: &str) -> Option<Arc<SegmentData>> {
        let mut g = self.inner.lock();
        let i = &mut *g;
        i.tick += 1;
        let tick = i.tick;
        let (seg, t) = i.map.get_mut(key)?;
        i.order.remove(t);
        *t = tick;
        i.order.insert(tick, key.to_string());
        Some(seg.clone())
    }

    pub fn insert(&self, key: &str, seg: Arc<SegmentData>) {
        if seg.size > self.cap {
            return;
        }
        let mut g = self.inner.lock();
        let i = &mut *g;
        i.tick += 1;
        let tick = i.tick;
        if let Some((old, t)) = i.map.insert(key.to_string(), (seg.clone(), tick)) {
            i.order.remove(&t);
            i.bytes -= old.size;
        }
        i.order.insert(tick, key.to_string());
        i.bytes += seg.size;
        while i.bytes > self.cap {
            let Some((_, k)) = i.order.pop_first() else { break };
            if let Some((s, _)) = i.map.remove(&k) {
                i.bytes -= s.size;
            }
        }
    }

    pub fn remove(&self, key: &str) {
        let mut g = self.inner.lock();
        if let Some((s, t)) = g.map.remove(key) {
            g.order.remove(&t);
            g.bytes -= s.size;
        }
    }

    pub fn bytes(&self) -> u64 {
        self.inner.lock().bytes
    }
}

/// Fetch and decode a segment object through the cache.
pub(crate) async fn fetch_segment(store: &dyn ObjectStore, cache: &Cache, path: &Path) -> anyhow::Result<Arc<SegmentData>> {
    let key = path.to_string();
    if let Some(s) = cache.get(&key) {
        return Ok(s);
    }
    let data = store.get(path).await.with_context(|| format!("get {path}"))?.bytes().await?;
    let seg = Arc::new(SegmentData::decode_segment(data).with_context(|| format!("decode {path}"))?);
    cache.insert(&key, seg.clone());
    Ok(seg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_roundtrip_and_cache() {
        let entries: Vec<(u64, Bytes)> = (10..20).filter(|i| *i != 13).map(|i| (i, Bytes::from(format!("e{i}")))).collect();
        let data = encode_segment(&entries, 10, 20);
        let seg = SegmentData::decode_segment(data.clone()).unwrap();
        assert_eq!((seg.end, seg.entries.len()), (20, 9));
        let mut out = Vec::new();
        let mut bytes = 0;
        assert_eq!(seg.read_into(12, 3, 1 << 20, &mut out, &mut bytes), 16);
        assert_eq!(out.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [12, 14, 15]);
        let mut out = Vec::new();
        assert_eq!(seg.read_into(18, 100, 1 << 20, &mut out, &mut 0), 20);
        assert_eq!(out.len(), 2);
        let mut bad = data.to_vec();
        bad[9] ^= 1;
        assert!(SegmentData::decode_segment(Bytes::from(bad)).is_err());
        assert_eq!(object_component("q///a b/1"), "q=2F=2F=2Fa=20b=2F1");
        assert_eq!(parse_seg_name("00000000000000000042.seg"), Some(42));
        assert_eq!(parse_seg_name("42.seg"), None);

        let cache = Cache::new(2 * seg.size);
        let seg = Arc::new(seg);
        cache.insert("a", seg.clone());
        cache.insert("b", seg.clone());
        assert!(cache.get("a").is_some());
        cache.insert("c", seg.clone());
        assert!(cache.get("b").is_none(), "least recently used is evicted");
        assert!(cache.get("a").is_some() && cache.get("c").is_some());
        assert_eq!(cache.bytes(), 2 * seg.size);
    }
}

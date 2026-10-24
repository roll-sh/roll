//! The node's block cache: decoded blocks in memory, stored blocks on local disk, and coalesced
//! range GETs from object storage for the rest. Both tiers are LRU with a byte budget shared by
//! every table of every database on the node.
//!
//! The memory tier holds decompressed blocks (a hit costs no CPU); the disk tier holds blocks as
//! stored (compressed, checksummed), so it holds more and a corrupt file is detected and
//! refetched. Adjacent missing blocks are fetched with one GET. Decompression runs on the
//! blocking pool.

use crate::sstable::cursor::BlockNeed;
use anyhow::Result;
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub memory_bytes: u64,
    /// The local disk tier (wiped when the cache starts). `None` disables it.
    pub disk_dir: Option<PathBuf>,
    pub disk_bytes: u64,
    /// The largest single range GET when coalescing adjacent blocks.
    pub max_get_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { memory_bytes: 256 << 20, disk_dir: None, disk_bytes: 4 << 30, max_get_bytes: 8 << 20 }
    }
}

type Key = (u64, u32);

/// An LRU index with a byte budget. Memory-tier entries carry their bytes; disk-tier entries
/// only their size (the bytes are in a file).
struct Lru {
    map: HashMap<Key, (Option<Bytes>, u64, u64)>,
    order: BTreeMap<u64, Key>,
    bytes: u64,
    budget: u64,
    tick: u64,
}

impl Lru {
    fn new(budget: u64) -> Self {
        Self { map: HashMap::new(), order: BTreeMap::new(), bytes: 0, budget, tick: 0 }
    }

    /// Touch an entry; returns its bytes (memory tier) or an empty `Bytes` (disk tier).
    fn get(&mut self, k: &Key) -> Option<Bytes> {
        self.tick += 1;
        let tick = self.tick;
        let (b, _, t) = self.map.get_mut(k)?;
        self.order.remove(t);
        *t = tick;
        self.order.insert(tick, *k);
        Some(b.clone().unwrap_or_default())
    }

    /// Insert; returns the evicted entries (key, size), possibly including `k` itself when it
    /// doesn't fit at all.
    fn insert(&mut self, k: Key, b: Option<Bytes>, size: u64) -> Vec<(Key, u64)> {
        let mut evicted = self.remove(&k);
        if size > self.budget {
            evicted.push((k, 0));
            return evicted;
        }
        self.tick += 1;
        self.map.insert(k, (b, size, self.tick));
        self.order.insert(self.tick, k);
        self.bytes += size;
        while self.bytes > self.budget {
            let Some((_, victim)) = self.order.pop_first() else { break };
            if let Some((_, n, _)) = self.map.remove(&victim) {
                self.bytes -= n;
                evicted.push((victim, n));
            }
        }
        evicted
    }

    /// Drop one entry (O(log n): inserts and corrupt-block drops are on the read path).
    fn remove(&mut self, k: &Key) -> Vec<(Key, u64)> {
        match self.map.remove(k) {
            Some((_, n, t)) => {
                self.order.remove(&t);
                self.bytes -= n;
                vec![(*k, n)]
            }
            None => Vec::new(),
        }
    }

    /// Drop every entry matching `f` (a scan: for forgetting a whole SSTable).
    fn remove_where(&mut self, f: impl Fn(&Key) -> bool) -> Vec<(Key, u64)> {
        let keys: Vec<Key> = self.map.keys().filter(|k| f(k)).copied().collect();
        let mut out = Vec::new();
        for k in keys {
            if let Some((_, n, t)) = self.map.remove(&k) {
                self.order.remove(&t);
                self.bytes -= n;
                out.push((k, n));
            }
        }
        out
    }
}

const SHARDS: usize = 8;

/// Bytes an insert added: nothing when the entry didn't fit.
fn added(k: &Key, n: u64, evicted: &[(Key, u64)]) -> u64 {
    if evicted.iter().any(|(x, s)| x == k && *s == 0) { 0 } else { n }
}

#[derive(Debug, Default)]
pub struct CacheMetrics {
    pub memory_hits: AtomicU64,
    pub disk_hits: AtomicU64,
    pub misses: AtomicU64,
    pub gets: AtomicU64,
    pub get_bytes: AtomicU64,
    pub corrupt_disk_blocks: AtomicU64,
}

pub struct BlockCache {
    cfg: CacheConfig,
    mem: Vec<Mutex<Lru>>,
    disk: Option<Mutex<Lru>>,
    /// Cached bytes (memory + disk) per SSTable uid, for table statistics.
    per_sst: Mutex<HashMap<u64, u64>>,
    pub metrics: CacheMetrics,
}

fn shard(k: &Key) -> usize {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    k.hash(&mut h);
    (h.finish() as usize) % SHARDS
}

impl BlockCache {
    pub fn new(cfg: CacheConfig) -> Result<Arc<Self>> {
        let disk = match &cfg.disk_dir {
            Some(dir) => {
                let _ = std::fs::remove_dir_all(dir);
                std::fs::create_dir_all(dir)?;
                Some(Mutex::new(Lru::new(cfg.disk_bytes)))
            }
            None => None,
        };
        let per = (cfg.memory_bytes / SHARDS as u64).max(1);
        Ok(Arc::new(Self {
            mem: (0..SHARDS).map(|_| Mutex::new(Lru::new(per))).collect(),
            disk,
            per_sst: Mutex::new(HashMap::new()),
            cfg,
            metrics: CacheMetrics::default(),
        }))
    }

    pub fn memory_bytes(&self) -> u64 {
        self.mem.iter().map(|m| m.lock().bytes).sum()
    }

    pub fn memory_budget(&self) -> u64 {
        self.cfg.memory_bytes
    }

    pub fn disk_bytes(&self) -> u64 {
        self.disk.as_ref().map_or(0, |d| d.lock().bytes)
    }

    /// Bytes cached (memory and disk) for one SSTable.
    pub fn sstable_bytes(&self, uid: u64) -> u64 {
        self.per_sst.lock().get(&uid).copied().unwrap_or(0)
    }

    fn account(&self, uid: u64, add: u64, sub: &[(Key, u64)]) {
        let mut p = self.per_sst.lock();
        if add > 0 {
            *p.entry(uid).or_insert(0) += add;
        }
        for ((u, _), n) in sub {
            if let Some(v) = p.get_mut(u) {
                *v = v.saturating_sub(*n);
                if *v == 0 {
                    p.remove(u);
                }
            }
        }
    }

    fn disk_path(&self, k: &Key) -> Option<PathBuf> {
        self.cfg.disk_dir.as_ref().map(|d| d.join(format!("{:x}-{}", k.0, k.1)))
    }

    fn remove_disk_files(&self, evicted: &[(Key, u64)]) {
        for (k, _) in evicted {
            if let Some(p) = self.disk_path(k) {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// Drop everything cached for an SSTable (it was compacted away or its database unloaded).
    pub fn forget(&self, uid: u64) {
        for m in &self.mem {
            let ev = m.lock().remove_where(|k| k.0 == uid);
            self.account(uid, 0, &ev);
        }
        if let Some(d) = &self.disk {
            let ev = d.lock().remove_where(|k| k.0 == uid);
            self.account(uid, 0, &ev);
            self.remove_disk_files(&ev);
        }
    }

    fn mem_get(&self, k: &Key) -> Option<Bytes> {
        self.mem[shard(k)].lock().get(k)
    }

    fn mem_put(&self, k: Key, b: Bytes) {
        let n = b.len() as u64;
        let ev = self.mem[shard(&k)].lock().insert(k, Some(b), n);
        self.account(k.0, added(&k, n, &ev), &ev);
    }

    async fn disk_get(&self, k: &Key) -> Option<Bytes> {
        let disk = self.disk.as_ref()?;
        disk.lock().get(k)?;
        let path = self.disk_path(k)?;
        match tokio::fs::read(&path).await {
            Ok(v) => Some(Bytes::from(v)),
            Err(_) => {
                let ev = disk.lock().remove(k);
                self.account(k.0, 0, &ev);
                None
            }
        }
    }

    async fn disk_put(&self, k: Key, stored: Bytes) {
        let Some(disk) = &self.disk else { return };
        let Some(path) = self.disk_path(&k) else { return };
        if disk.lock().get(&k).is_some() {
            return;
        }
        if tokio::fs::write(&path, &stored).await.is_err() {
            return;
        }
        let n = stored.len() as u64;
        let ev = disk.lock().insert(k, None, n);
        self.account(k.0, added(&k, n, &ev), &ev);
        self.remove_disk_files(&ev);
    }

    /// Fetch decoded blocks `need.blocks` of `need.sst`. With `fill`, misses are added to the
    /// caches (compaction reads pass `false` so they don't evict the working set).
    pub async fn fetch(&self, need: &BlockNeed, fill: bool) -> Result<Vec<Bytes>> {
        let sst = &need.sst;
        let range = need.blocks.clone();
        let mut out: Vec<Option<Bytes>> = vec![None; range.len()];
        let mut missing: Vec<usize> = Vec::new();
        for (j, i) in range.clone().enumerate() {
            match self.mem_get(&(sst.uid, i as u32)) {
                Some(b) => {
                    self.metrics.memory_hits.fetch_add(1, Ordering::Relaxed);
                    out[j] = Some(b);
                }
                None => missing.push(i),
            }
        }
        if missing.is_empty() {
            return Ok(out.into_iter().map(|b| b.expect("filled")).collect());
        }
        // Disk tier.
        let mut stored: Vec<(usize, Bytes)> = Vec::new();
        if self.disk.is_some() {
            let mut still = Vec::new();
            for i in missing {
                match self.disk_get(&(sst.uid, i as u32)).await {
                    Some(b) => {
                        self.metrics.disk_hits.fetch_add(1, Ordering::Relaxed);
                        stored.push((i, b))
                    }
                    None => still.push(i),
                }
            }
            missing = still;
        }
        // Object storage: contiguous runs, one GET each.
        let blocks = &sst.index.blocks;
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for i in missing {
            match runs.last_mut() {
                Some((start, end)) if *end == i && blocks[i].offset + blocks[i].stored_len() - blocks[*start].offset <= self.cfg.max_get_bytes => {
                    *end = i + 1;
                }
                _ => runs.push((i, i + 1)),
            }
        }
        let gets = runs.iter().map(|&(a, b)| {
            let key = sst.data_key();
            let start = blocks[a].offset;
            let end = blocks[b - 1].offset + blocks[b - 1].stored_len();
            async move {
                let data = sst.source.read(&key, start..end).await?;
                anyhow::Ok((a, b, start, data))
            }
        });
        let fetched = futures::future::try_join_all(gets).await?;
        let mut fresh: Vec<usize> = Vec::new();
        for (a, b, start, data) in fetched {
            self.metrics.gets.fetch_add(1, Ordering::Relaxed);
            self.metrics.get_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
            for (i, meta) in blocks.iter().enumerate().take(b).skip(a) {
                self.metrics.misses.fetch_add(1, Ordering::Relaxed);
                let lo = (meta.offset - start) as usize;
                let hi = lo + meta.stored_len() as usize;
                stored.push((i, data.slice(lo..hi)));
                fresh.push(i);
            }
        }
        // Decode everything not in memory on the blocking pool. Compaction (`fill == false`) is
        // on a blocking thread already: it decodes in place, so the blocks are allocated and
        // freed by one thread (memory freed across threads stays with the allocating thread's
        // heap until that thread runs again).
        let decode_sst = sst.clone();
        let to_decode = stored.clone();
        let decoded: Vec<(usize, Result<Bytes>)> = if fill && to_decode.iter().map(|(_, b)| b.len()).sum::<usize>() > 16 << 10 {
            tokio::task::spawn_blocking(move || to_decode.into_iter().map(|(i, b)| (i, decode_sst.decode_block(i, &b))).collect()).await?
        } else {
            to_decode.into_iter().map(|(i, b)| (i, decode_sst.decode_block(i, &b))).collect()
        };
        for ((i, res), (_, raw)) in decoded.into_iter().zip(stored) {
            let block = match res {
                Ok(b) => b,
                Err(e) if !fresh.contains(&i) => {
                    // A corrupt disk cache file: drop it and read the block from object storage.
                    self.metrics.corrupt_disk_blocks.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %e, "corrupt block in the disk cache, refetching");
                    if let Some(d) = &self.disk {
                        let ev = d.lock().remove(&(sst.uid, i as u32));
                        self.account(sst.uid, 0, &ev);
                        self.remove_disk_files(&ev);
                    }
                    let data = sst.source.read(&sst.data_key(), blocks[i].offset..blocks[i].offset + blocks[i].stored_len()).await?;
                    let s2 = sst.clone();
                    tokio::task::spawn_blocking(move || s2.decode_block(i, &data)).await??
                }
                Err(e) => return Err(e),
            };
            if fill {
                self.mem_put((sst.uid, i as u32), block.clone());
                if fresh.contains(&i) && self.disk.is_some() {
                    self.disk_put((sst.uid, i as u32), raw).await;
                }
            }
            out[i - range.start] = Some(block);
        }
        out.into_iter().enumerate().map(|(j, b)| b.ok_or_else(|| anyhow::anyhow!("block {} not fetched", range.start + j))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(n: usize) -> Option<Bytes> {
        Some(Bytes::from(vec![0u8; n]))
    }

    #[test]
    fn lru_replaces_evicts_and_removes_by_key() {
        let mut l = Lru::new(100);
        assert!(l.insert((1, 0), b(40), 40).is_empty());
        assert!(l.insert((1, 1), b(40), 40).is_empty());
        // Re-inserting a key replaces it: its old size is given back, nothing else goes.
        assert_eq!(l.insert((1, 0), b(30), 30), vec![((1, 0), 40)]);
        assert_eq!((l.bytes, l.map.len(), l.order.len()), (70, 2, 2));
        // Touch (1, 1) so (1, 0) is the least recently used, then overflow.
        assert!(l.get(&(1, 1)).is_some());
        assert_eq!(l.insert((2, 0), b(50), 50), vec![((1, 0), 30)]);
        assert_eq!((l.bytes, l.map.len(), l.order.len()), (90, 2, 2));
        assert_eq!(l.remove(&(2, 0)), vec![((2, 0), 50)]);
        assert!(l.remove(&(2, 0)).is_empty());
        assert_eq!((l.bytes, l.map.len(), l.order.len()), (40, 1, 1));
        // Too big to fit at all: reported as evicted with size 0, and not stored.
        assert_eq!(l.insert((3, 0), b(200), 200), vec![((3, 0), 0)]);
        assert_eq!(added(&(3, 0), 200, &[((3, 0), 0)]), 0);
        assert!(l.get(&(3, 0)).is_none());
        assert_eq!(l.remove_where(|k| k.0 == 1), vec![((1, 1), 40)]);
        assert_eq!((l.bytes, l.map.len(), l.order.len()), (0, 0, 0));
    }
}

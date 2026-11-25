//! The shard request cache's memory: size-0 results per shard (`IndicesRequestCache`), each
//! entry charged what it holds to the cluster's `request` breaker from when it is stored until
//! it goes, and every shard's entries together held to the cluster's cap
//! (`indices.requests.cache.size`, Elasticsearch's 1% of the heap: of the node's budget here).
//!
//! Each shard keeps its own map (lookups lock only their shard); the cluster's [`Budget`] keeps
//! the cap, the charge and one eviction order over every shard's entries (a clock: an entry hit
//! since the hand last passed it gets another round), so a new entry pushes out the least
//! recently used ones wherever they are, as Elasticsearch's node-wide LRU does.

use parking_lot::Mutex;
use search_aggs::memory::MemoryBudget;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

/// What an entry holds beyond its key and value: its map slot, its place in the eviction
/// order, the `Arc` and the cached result's own fields.
pub const ENTRY_OVERHEAD: u64 = 160;

/// A cached value.
pub type Entry = Arc<dyn Any + Send + Sync>;

/// A cluster's request cache: its cap, what its shards' entries hold, and their eviction order.
pub struct Budget {
    cap: u64,
    used: AtomicU64,
    /// The cluster's `request` breaker (`None`: nothing charged, tests and private engines).
    breaker: Option<Arc<MemoryBudget>>,
    /// Entries in the order they were stored: (shard, key, the entry's sequence number).
    ring: Mutex<VecDeque<(Weak<Shard>, String, u64)>>,
    seq: AtomicU64,
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget").field("cap", &self.cap).field("used", &self.used.load(Ordering::Relaxed)).finish()
    }
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(u64::MAX, None)
    }
}

impl Budget {
    pub fn new(cap: u64, breaker: Option<Arc<MemoryBudget>>) -> Budget {
        Budget { cap, used: AtomicU64::new(0), breaker, ring: Mutex::new(VecDeque::new()), seq: AtomicU64::new(0) }
    }

    /// The most the cluster's entries may hold.
    pub fn cap(&self) -> u64 {
        self.cap
    }

    /// What the cluster's entries hold now.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn release(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.used.fetch_sub(bytes, Ordering::Relaxed);
        if let Some(b) = &self.breaker {
            b.release(bytes);
        }
    }

    /// Reserves `bytes` under the cap and on the breaker: `false` (nothing reserved) when either
    /// has no room. A cache is memory to spare: a full breaker caches nothing and trips nothing.
    fn reserve(&self, bytes: u64) -> bool {
        let fits = self.used.try_update(Ordering::Relaxed, Ordering::Relaxed, |u| u.checked_add(bytes).filter(|n| *n <= self.cap)).is_ok();
        if !fits {
            return false;
        }
        if let Some(b) = &self.breaker
            && !b.try_reserve(bytes)
        {
            self.used.fetch_sub(bytes, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Pushes entries out, oldest first (one hit since the hand passed buys another round),
    /// until `bytes` more fit under the cap or nothing is left to push out. Locks one shard at a
    /// time, never with the ring's lock held (only [`Budget::compact`] takes them in that order).
    fn make_room(&self, bytes: u64) {
        // Each entry is passed at most twice: once to clear its hit, once to evict it.
        let mut budget = self.ring.lock().len() * 2;
        while self.used().saturating_add(bytes) > self.cap && budget > 0 {
            budget -= 1;
            let Some((shard, key, seq)) = self.ring.lock().pop_front() else { return };
            let Some(shard) = shard.upgrade() else { continue };
            let mut map = shard.map.lock();
            let Some(slot) = map.entries.get(&key).filter(|s| s.seq == seq) else { continue };
            if slot.hit.swap(false, Ordering::Relaxed) {
                drop(map);
                self.ring.lock().push_back((Arc::downgrade(&shard), key, seq));
                continue;
            }
            if let Some(slot) = map.entries.remove(&key) {
                map.bytes -= slot.bytes;
                shard.evictions.fetch_add(1, Ordering::Relaxed);
                self.release(slot.bytes);
            }
        }
    }

    /// Drops ring places of entries that are gone, once they outnumber the live ones.
    fn compact(&self, live: usize) {
        let mut ring = self.ring.lock();
        if ring.len() > 2 * live + 1024 {
            ring.retain(|(s, k, seq)| s.upgrade().is_some_and(|s| s.map.lock().entries.get(k).is_some_and(|e| e.seq == *seq)));
        }
    }
}

struct Slot {
    value: Entry,
    bytes: u64,
    seq: u64,
    /// Hit since the clock's hand last passed it.
    hit: AtomicBool,
}

#[derive(Default)]
struct Map {
    entries: HashMap<String, Slot>,
    bytes: u64,
}

/// One shard's entries.
pub struct Shard {
    map: Mutex<Map>,
    evictions: AtomicU64,
}

/// A shard's request cache, charged to its cluster's [`Budget`]. Dropping it (the shard
/// closed, deleted or unloaded) gives back what it held.
pub struct ShardCache {
    shard: Arc<Shard>,
    budget: Arc<Budget>,
}

impl std::fmt::Debug for ShardCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardCache").field("bytes", &self.bytes()).finish()
    }
}

impl Default for ShardCache {
    fn default() -> Self {
        ShardCache::new(Arc::new(Budget::default()))
    }
}

impl ShardCache {
    pub fn new(budget: Arc<Budget>) -> ShardCache {
        ShardCache { shard: Arc::new(Shard { map: Mutex::new(Map::default()), evictions: AtomicU64::new(0) }), budget }
    }

    /// The entry under `key`, marked used.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let map = self.shard.map.lock();
        let slot = map.entries.get(key)?;
        slot.hit.store(true, Ordering::Relaxed);
        Some(slot.value.clone())
    }

    /// Stores `value` under `key`, charged `bytes`, pushing out the least recently used entries
    /// of the cluster's to make room: `false` (not stored) when it doesn't fit anyway.
    pub fn insert(&self, key: String, value: Entry, bytes: u64) -> bool {
        // What it replaces is given back first.
        self.remove(&key);
        if bytes > self.budget.cap {
            return false;
        }
        self.budget.make_room(bytes);
        if !self.budget.reserve(bytes) {
            return false;
        }
        let seq = self.budget.seq.fetch_add(1, Ordering::Relaxed);
        let live = {
            let mut map = self.shard.map.lock();
            map.bytes += bytes;
            if let Some(old) = map.entries.insert(key.clone(), Slot { value, bytes, seq, hit: AtomicBool::new(false) }) {
                // A concurrent insert of the same key won: keep one charge.
                map.bytes -= old.bytes;
                self.budget.release(old.bytes);
            }
            map.entries.len()
        };
        self.budget.ring.lock().push_back((Arc::downgrade(&self.shard), key, seq));
        self.budget.compact(live);
        true
    }

    fn remove(&self, key: &str) {
        let mut map = self.shard.map.lock();
        if let Some(old) = map.entries.remove(key) {
            map.bytes -= old.bytes;
            self.budget.release(old.bytes);
        }
    }

    /// Drops every entry (a refresh, `_cache/clear`), giving back what they held.
    pub fn clear(&self) {
        let mut map = self.shard.map.lock();
        let bytes = std::mem::take(&mut map.bytes);
        map.entries.clear();
        self.budget.release(bytes);
    }

    /// What the shard's entries hold (`request_cache.memory_size_in_bytes`).
    pub fn bytes(&self) -> u64 {
        self.shard.map.lock().bytes
    }

    /// Entries pushed out to make room since the shard opened.
    pub fn evictions(&self) -> u64 {
        self.shard.evictions.load(Ordering::Relaxed)
    }

    /// The keys with what each holds.
    pub fn sizes(&self) -> Vec<(String, u64)> {
        self.shard.map.lock().entries.iter().map(|(k, s)| (k.clone(), s.bytes)).collect()
    }

    /// Whether `key` has an entry.
    pub fn contains(&self, key: &str) -> bool {
        self.shard.map.lock().entries.contains_key(key)
    }
}

impl Drop for ShardCache {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Arc::new(())
    }

    #[test]
    fn the_cap_pushes_out_the_least_recently_used_of_any_shard() {
        let breaker = MemoryBudget::new(u64::MAX);
        let budget = Arc::new(Budget::new(300, Some(breaker.clone())));
        let (a, b) = (ShardCache::new(budget.clone()), ShardCache::new(budget.clone()));
        assert!(a.insert("1".into(), entry(), 100));
        assert!(b.insert("2".into(), entry(), 100));
        assert!(a.insert("3".into(), entry(), 100));
        assert!(a.get("1").is_some());
        // Full: "2" (unused) goes, "1" (hit) stays.
        assert!(b.insert("4".into(), entry(), 100));
        assert!(a.contains("1") && !b.contains("2") && a.contains("3") && b.contains("4"));
        assert_eq!((budget.used(), breaker.used()), (300, 300));
        assert_eq!(b.evictions(), 1);
        // Larger than the cap: never stored.
        assert!(!a.insert("5".into(), entry(), 301));
        a.clear();
        assert_eq!((budget.used(), breaker.used()), (100, 100));
        drop(b);
        assert_eq!((budget.used(), breaker.used()), (0, 0));
    }
}

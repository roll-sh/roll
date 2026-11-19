//! Process-wide block cache shared by every open database, bounded by bytes (CLOCK eviction).
//! Idle databases' blocks simply age out.
//!
//! A hit takes its shard's lock shared (its block's `referenced` an atomic): readers never wait
//! for each other, only for an insert or eviction. (With the hit under the shard's mutex, 32
//! clients on four CPUs spun and yielded behind a reader preempted inside it: 70% of their
//! `sched_yield` stalls, HANDOFF "Point and range reads, round 6".) An evicted block is dropped
//! at once, under the lock: the memory the cache counts is what it holds.

use super::sst::Block;
use parking_lot::RwLock;
// (foldhash: a lookup per block read, SipHash was a fifth of it.)
use hashbrown::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub type CachedBlock = Arc<Block>;

const SHARDS: usize = 16;

struct Entry {
    block: CachedBlock,
    size: usize,
    referenced: AtomicBool,
}

#[derive(Default)]
struct Shard {
    map: HashMap<(u64, u32), Entry>,
    clock: Vec<(u64, u32)>,
    hand: usize,
    bytes: usize,
}

pub struct BlockCache {
    shards: Vec<RwLock<Shard>>,
    capacity_per_shard: usize,
    next_id: AtomicU64,
}

impl BlockCache {
    pub fn new(capacity_bytes: usize) -> Arc<Self> {
        Arc::new(BlockCache {
            shards: (0..SHARDS).map(|_| RwLock::new(Shard::default())).collect(),
            capacity_per_shard: (capacity_bytes / SHARDS).max(1 << 20),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn next_file_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn shard(&self, file: u64, block: u32) -> &RwLock<Shard> {
        let h = (file.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ block as u64) as usize;
        &self.shards[h % SHARDS]
    }

    pub fn get(&self, file: u64, block: u32) -> Option<CachedBlock> {
        let s = self.shard(file, block).read();
        let e = s.map.get(&(file, block))?;
        if !e.referenced.load(Ordering::Relaxed) {
            e.referenced.store(true, Ordering::Relaxed);
        }
        Some(e.block.clone())
    }

    pub fn insert(&self, file: u64, block: u32, b: CachedBlock, size: usize) {
        let mut s = self.shard(file, block).write();
        if s.map.contains_key(&(file, block)) {
            return;
        }
        while s.bytes + size > self.capacity_per_shard && !s.clock.is_empty() {
            let i = s.hand % s.clock.len();
            let key = s.clock[i];
            let evict = match s.map.get(&key) {
                Some(e) if e.referenced.load(Ordering::Relaxed) => {
                    e.referenced.store(false, Ordering::Relaxed);
                    false
                }
                Some(_) => true,
                None => true,
            };
            if evict {
                if let Some(e) = s.map.remove(&key) {
                    s.bytes -= e.size;
                }
                s.clock.swap_remove(i);
            } else {
                s.hand = i + 1;
            }
        }
        s.map.insert(
            (file, block),
            Entry {
                block: b,
                size,
                referenced: AtomicBool::new(false),
            },
        );
        s.clock.push((file, block));
        s.bytes += size;
    }

    /// `insert`, when the block fits without evicting anything (a bulk read's block,
    /// `storage::bulk_read`: a big scan fills a cache with room, never pushes out what is in
    /// it).
    pub fn insert_if_room(&self, file: u64, block: u32, b: CachedBlock, size: usize) {
        let mut s = self.shard(file, block).write();
        if s.bytes + size > self.capacity_per_shard || s.map.contains_key(&(file, block)) {
            return;
        }
        s.map.insert(
            (file, block),
            Entry {
                block: b,
                size,
                referenced: AtomicBool::new(false),
            },
        );
        s.clock.push((file, block));
        s.bytes += size;
    }

    /// Drop every block of a closed file.
    pub fn forget_file(&self, file: u64) {
        for shard in &self.shards {
            let mut s = shard.write();
            if !s.map.keys().any(|(f, _)| *f == file) {
                continue;
            }
            let mut freed = 0;
            s.map.retain(|(f, _), e| {
                if *f == file {
                    freed += e.size;
                    false
                } else {
                    true
                }
            });
            s.bytes -= freed;
            s.clock.retain(|(f, _)| *f != file);
        }
    }

    /// The bytes the cache may hold.
    pub fn capacity_bytes(&self) -> usize {
        self.capacity_per_shard * SHARDS
    }

    pub fn used_bytes(&self) -> usize {
        self.shards.iter().map(|s| s.read().bytes).sum()
    }
}

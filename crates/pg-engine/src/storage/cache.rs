//! Process-wide block cache shared by every open database, bounded by bytes (CLOCK eviction).
//! Idle databases' blocks simply age out.

use super::sst::Block;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub type CachedBlock = Arc<Block>;

const SHARDS: usize = 16;

struct Entry {
    block: CachedBlock,
    size: usize,
    referenced: bool,
}

#[derive(Default)]
struct Shard {
    map: HashMap<(u64, u32), Entry>,
    clock: Vec<(u64, u32)>,
    hand: usize,
    bytes: usize,
}

pub struct BlockCache {
    shards: Vec<Mutex<Shard>>,
    capacity_per_shard: usize,
    next_id: AtomicU64,
}

impl BlockCache {
    pub fn new(capacity_bytes: usize) -> Arc<Self> {
        Arc::new(BlockCache {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
            capacity_per_shard: (capacity_bytes / SHARDS).max(1 << 20),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn next_file_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn shard(&self, file: u64, block: u32) -> &Mutex<Shard> {
        let h = (file.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ block as u64) as usize;
        &self.shards[h % SHARDS]
    }

    pub fn get(&self, file: u64, block: u32) -> Option<CachedBlock> {
        let mut s = self.shard(file, block).lock();
        let e = s.map.get_mut(&(file, block))?;
        e.referenced = true;
        Some(e.block.clone())
    }

    pub fn insert(&self, file: u64, block: u32, b: CachedBlock, size: usize) {
        let mut s = self.shard(file, block).lock();
        if s.map.contains_key(&(file, block)) {
            return;
        }
        while s.bytes + size > self.capacity_per_shard && !s.clock.is_empty() {
            let i = s.hand % s.clock.len();
            let key = s.clock[i];
            let evict = match s.map.get_mut(&key) {
                Some(e) if e.referenced => {
                    e.referenced = false;
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
                referenced: false,
            },
        );
        s.clock.push((file, block));
        s.bytes += size;
    }

    /// Drop every block of a closed file.
    pub fn forget_file(&self, file: u64) {
        for shard in &self.shards {
            let mut s = shard.lock();
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

    pub fn used_bytes(&self) -> usize {
        self.shards.iter().map(|s| s.lock().bytes).sum()
    }
}

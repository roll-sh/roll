//! Process-wide block cache shared by every open database, bounded by bytes (CLOCK eviction).
//! Idle databases' blocks simply age out.
//!
//! A hit takes its shard's lock shared (its block's `referenced` an atomic): readers never wait
//! for each other, only for an insert or eviction. (With the hit under the shard's mutex, 32
//! clients on four CPUs spun and yielded behind a reader preempted inside it: 70% of their
//! `sched_yield` stalls, HANDOFF "Point and range reads, round 6".) An evicted block is dropped
//! at once, under the lock: the memory the cache counts is what it holds, its blocks'
//! allocations as the allocator sizes them (`Block::mem_bytes`) and its own tables. (Counting a
//! block's length and struct, a 256 MB cache held 268 MB at TPC-H SF 1.)
//!
//! A block's buffer is in memory the cache keeps (`arena`): a piece of a shard's chunks, which
//! the next insert reuses whichever thread freed it. A shard's arena maps no more than the
//! shard may hold; at that limit a miss's buffer is made by evicting (`BlockCache::buffer`).

mod arena;

use super::sst::Block;
pub(crate) use arena::Buf;
use arena::Arena;
use parking_lot::RwLock;
// (foldhash: a lookup per block read, SipHash was a fifth of it.)
use hashbrown::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub type CachedBlock = Arc<Block>;

const SHARDS: usize = 16;
/// Blocks a miss evicts at most to make room for its buffer in a full arena (else its buffer
/// is the heap's).
const EVICT_FOR_ROOM: usize = 16;

/// The size the server's allocator (mimalloc) gives an allocation of `n` bytes: 8, then
/// 16-byte steps up to 64, one of four steps between powers of two up to 64 kB, past it whole
/// 4 kB pages.
pub(crate) fn alloc_bytes(n: usize) -> usize {
    match n {
        0..=8 => 8,
        9..=64 => n.next_multiple_of(16),
        65..=0x10000 => n.next_multiple_of(1 << (usize::BITS - 3 - (n - 1).leading_zeros())),
        _ => n.next_multiple_of(4096),
    }
}

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

impl Shard {
    /// Its blocks' bytes and its tables'.
    fn held(&self) -> usize {
        self.bytes + self.map.allocation_size() + self.clock.capacity() * std::mem::size_of::<(u64, u32)>()
    }

    /// Evict one block: the first one not referenced from the hand on, clearing the referenced
    /// ones' bits on the way (CLOCK). False when it holds none.
    fn evict_one(&mut self) -> bool {
        while !self.clock.is_empty() {
            let i = self.hand % self.clock.len();
            let key = self.clock[i];
            if let Some(e) = self.map.get(&key)
                && e.referenced.load(Ordering::Relaxed)
            {
                e.referenced.store(false, Ordering::Relaxed);
            } else {
                if let Some(e) = self.map.remove(&key) {
                    self.bytes -= e.size;
                }
                // (The newest block moves into the slot: the hand goes past it, as CLOCK puts a
                // new block behind the hand. Left on it, the hand evicted each insert's block at
                // the next, and the cache kept only its oldest blocks.)
                self.clock.swap_remove(i);
                self.hand = i + 1;
                return true;
            }
            self.hand = i + 1;
        }
        false
    }
}

pub struct BlockCache {
    shards: Vec<RwLock<Shard>>,
    /// Each shard's blocks' buffers.
    arenas: Vec<Arc<Arena>>,
    capacity_per_shard: usize,
    next_id: AtomicU64,
}

impl BlockCache {
    pub fn new(capacity_bytes: usize) -> Arc<Self> {
        let capacity_per_shard = (capacity_bytes / SHARDS).max(1 << 20);
        Arc::new(BlockCache {
            shards: (0..SHARDS).map(|_| RwLock::new(Shard::default())).collect(),
            arenas: (0..SHARDS).map(|_| Arena::new(capacity_per_shard)).collect(),
            capacity_per_shard,
            next_id: AtomicU64::new(1),
        })
    }

    pub fn next_file_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn shard_of(file: u64, block: u32) -> usize {
        (file.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ block as u64) as usize % SHARDS
    }

    fn shard(&self, file: u64, block: u32) -> &RwLock<Shard> {
        &self.shards[Self::shard_of(file, block)]
    }

    /// A buffer of `len` bytes for block `block` of `file`, which is going into the cache, in
    /// its shard's arena. Where the arena is at its limit with no free piece that fits, it
    /// evicts (CLOCK) until one does, but not when `!evict` (a bulk read's block, which takes
    /// only room the cache has: then None also when the shard has no room). None: the buffer
    /// is the heap's.
    pub(crate) fn buffer(&self, file: u64, block: u32, len: usize, evict: bool) -> Option<Buf> {
        let k = Self::shard_of(file, block);
        let a = &self.arenas[k];
        let n = a.piece_bytes(len)?;
        if !evict && self.shards[k].read().held() + n > self.capacity_per_shard {
            return None;
        }
        if let Some(b) = a.take(len) {
            return Some(b);
        }
        if !evict {
            return None;
        }
        let mut s = self.shards[k].write();
        for _ in 0..EVICT_FOR_ROOM {
            if !s.evict_one() {
                break;
            }
            if let Some(b) = a.take(len) {
                return Some(b);
            }
        }
        None
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
        let tables = s.held() - s.bytes;
        while s.bytes + tables + size > self.capacity_per_shard && s.evict_one() {}
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
        if s.held() + size > self.capacity_per_shard || s.map.contains_key(&(file, block)) {
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
            // (A shard left empty gives its tables back: an empty cache holds nothing.)
            if s.map.is_empty() {
                *s = Shard::default();
            }
        }
    }

    /// The bytes the cache may hold.
    pub fn capacity_bytes(&self) -> usize {
        self.capacity_per_shard * SHARDS
    }

    pub fn used_bytes(&self) -> usize {
        self.shards.iter().map(|s| s.read().held()).sum()
    }
}

#[cfg(test)]
mod tests {
    /// What the cache counts of an allocation is what the server's allocator makes of it.
    #[test]
    fn alloc_bytes_is_the_allocators_size() {
        for n in 1..=256 << 10 {
            // SAFETY: mi_good_size only computes a size.
            assert_eq!(super::alloc_bytes(n), unsafe { libmimalloc_sys::mi_good_size(n) }, "{n}");
        }
    }
}

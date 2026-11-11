//! Counters that tests read and every session's statements bump (a kept tree restarted, an
//! index rescanned, a row sized): each thread adds to a cache line of its own group, not to
//! one line every core writes, and a read sums them.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const SHARDS: usize = 32;

#[repr(align(128))]
struct Shard(AtomicU64);

/// A value on a cache line of its own (two: the prefetcher reads lines in pairs), so that
/// writing it doesn't take the line from threads reading what would sit next to it.
#[repr(align(128))]
#[derive(Debug, Default)]
pub struct Padded<T>(pub T);

impl<T> std::ops::Deref for Padded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

pub struct Counter {
    shards: [Shard; SHARDS],
}

impl Default for Counter {
    fn default() -> Counter {
        Counter::new()
    }
}

impl Counter {
    pub const fn new() -> Counter {
        Counter { shards: [const { Shard(AtomicU64::new(0)) }; SHARDS] }
    }

    pub fn fetch_add(&self, n: u64, order: Ordering) {
        self.shards[shard()].0.fetch_add(n, order);
    }

    /// The sum of every thread's additions.
    pub fn load(&self, order: Ordering) -> u64 {
        self.shards.iter().map(|s| s.0.load(order)).sum()
    }
}

/// This thread's shard: threads take them in turn as they first count.
fn shard() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static SHARD: usize = NEXT.fetch_add(1, Ordering::Relaxed) % SHARDS;
    }
    SHARD.with(|s| *s)
}

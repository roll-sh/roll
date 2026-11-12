//! Crate-level tests: the LSM tree through `Table`, SSTables end to end, compaction,
//! persistence (commit log, manifests, branches, restore, GC) and a model-based property test.

mod bench;
mod compaction;
mod lifecycle;
mod lsm;
mod memory;
mod model;
mod persist;
mod sstable;

use crate::cache::{BlockCache, CacheConfig};
use crate::manifest::TableId;
use crate::model::*;
use crate::options::TableOptions;
use crate::read::{Partition, PartitionRead, RangeRead};
use crate::table::{Clock, Env, Table};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// A settable clock (ms).
pub(crate) struct TestClock(pub AtomicI64);

impl TestClock {
    pub fn new(ms: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(ms)))
    }
    pub fn clock(self: &Arc<Self>) -> Clock {
        let me = self.clone();
        Arc::new(move || me.0.load(Ordering::SeqCst))
    }
    pub fn set_s(&self, s: i64) {
        self.0.store(s * 1000, Ordering::SeqCst);
    }
}

pub(crate) fn env_with(objects: crate::Objects, clock: Clock, cache_bytes: u64) -> Arc<Env> {
    Arc::new(Env::new(objects, BlockCache::new(CacheConfig { memory_bytes: cache_bytes, ..Default::default() }).unwrap(), clock))
}

pub(crate) fn env(name: &str) -> Arc<Env> {
    env_with(crate::test_objects(name), TestClock::new(1_000_000_000).clock(), 4 << 20)
}

pub(crate) fn table(name: &str, opts: TableOptions) -> Arc<Table> {
    Table::standalone(TableId(1), "testdb", env(name), opts)
}

pub(crate) fn ck(i: u32) -> Clustering {
    Clustering::new([i.to_be_bytes()])
}

pub(crate) fn key(i: u32) -> PartitionKey {
    // Tokens out of key order, like Murmur3.
    PartitionKey::new((i as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64), Bytes::from(i.to_be_bytes().to_vec()))
}

pub(crate) fn val(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

pub(crate) fn row(c: u32, ts: i64, v: &str) -> Row {
    Row::new(ck(c)).with_liveness(LivenessInfo::live(ts)).with_cell(1, Cell::live(ts, val(v)))
}

pub(crate) async fn read(t: &Table, k: PartitionKey, now: i64) -> Option<Partition> {
    let mut p = t.read(PartitionRead::new(k, now)).partitions().await.unwrap();
    assert!(p.len() <= 1);
    p.pop()
}

pub(crate) async fn read_with(t: &Table, r: PartitionRead) -> Option<Partition> {
    let mut p = t.read(r).partitions().await.unwrap();
    p.pop()
}

pub(crate) async fn scan_all(t: &Table, now: i64) -> Vec<Partition> {
    t.scan(RangeRead::all(now)).partitions().await.unwrap()
}

pub(crate) fn clusterings(p: &Partition) -> Vec<u32> {
    p.rows.iter().map(|r| u32::from_be_bytes(r.clustering.get(0).unwrap()[..].try_into().unwrap())).collect()
}

pub(crate) fn value(r: &Row, col: ColumnId) -> Option<String> {
    r.cell(col).map(|c| String::from_utf8_lossy(&c.value).into_owned())
}

//! Hooks search-server's shard persistence needs from the engine (additive; owned by
//! search-server).
//!
//! The module is declared inside `engine::fields` (`#[path]`), so it can read and seed an
//! index's [`Numbers`]: field numbers are allocated as a mapping grows, in the order fields are
//! first seen, and segments record fields by number. A fresh [`Engine::index`] walks the final
//! mapping and would number fields differently than the segments a woken index reopens, so
//! search-server persists each index's numbering with its commits and reopens the index with it
//! ([`Engine::open_seeded`]) before any request can create it the default way.

use super::{FieldTable, Numbers};
use crate::engine::{Engine, IndexEngine, ShardCounters, ShardEngine, epoch_ms, similarities};
use crate::error::EsResult;
use crate::meta::{ClusterState, IndexMeta};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

impl Numbers {
    /// Every allocated `(key, number)`, by number. Keys are the allocation keys (field paths,
    /// with a suffix for per-layout vector fields), not necessarily schema names.
    pub fn snapshot(&self) -> Vec<(String, u32)> {
        let g = self.inner.lock();
        let mut v: Vec<(String, u32)> = g.0.iter().map(|(k, n)| (k.clone(), *n)).collect();
        v.sort_by_key(|(_, n)| *n);
        v
    }

    /// How many numbers are allocated (grows only).
    pub fn allocated(&self) -> usize {
        self.inner.lock().0.len()
    }

    /// Numbers continuing an earlier allocation: the same keys keep their numbers, and new keys
    /// get numbers past every earlier one.
    pub fn seeded(entries: &[(String, u32)]) -> Numbers {
        let map: HashMap<String, u32> = entries.iter().cloned().collect();
        let next = entries.iter().map(|(_, n)| n + 1).max().unwrap_or(0);
        Numbers { inner: Mutex::new((map, next)) }
    }
}

impl IndexEngine {
    /// The index's field numbering (to persist with a commit).
    pub fn field_numbers(&self) -> Vec<(String, u32)> {
        self.numbers.snapshot()
    }

    /// How many field numbers the index has allocated (changes whenever the numbering grows).
    pub fn field_numbers_allocated(&self) -> usize {
        self.numbers.allocated()
    }
}

impl Engine {
    /// Every loaded index engine.
    pub fn indices_snapshot(&self) -> Vec<Arc<IndexEngine>> {
        self.indices.read().values().cloned().collect()
    }

    /// Opens the engine of `idx` like [`Engine::index`] does on first use, but with field
    /// numbers continuing `numbers` (a persisted [`IndexEngine::field_numbers`]). Returns the
    /// existing engine if the index is already open.
    pub fn open_seeded(&self, cluster: &crate::Cluster, st: &ClusterState, idx: &IndexMeta, numbers: &[(String, u32)]) -> EsResult<Arc<IndexEngine>> {
        let mut g = self.indices.write();
        if let Some(ie) = g.get(&idx.uuid) {
            return Ok(ie.clone());
        }
        let analysis = Arc::new(cluster.index_analysis(st, idx)?);
        let numbers = Numbers::seeded(numbers);
        let ctx = idx.mapping_ctx();
        let mut table = FieldTable::build(idx.mapping.clone(), analysis, &ctx, &numbers, &similarities(idx))?;
        table.key = crate::engine::table_key(st, idx);
        let mut shards = Vec::new();
        for i in 0..idx.number_of_shards() {
            let shard = self.storage.open(&idx.uuid, i, table.schema.clone())?;
            shards.push(Arc::new(ShardEngine {
                id: i,
                shard,
                counters: ShardCounters::default(),
                dirty_since: AtomicI64::new(0),
                last_search: AtomicI64::new(epoch_ms()),
                request_cache: Default::default(),
            }));
        }
        let ie = Arc::new(IndexEngine::assemble(idx.uuid.clone(), idx.name.clone(), shards, numbers, table));
        g.insert(idx.uuid.clone(), ie.clone());
        Ok(ie)
    }
}

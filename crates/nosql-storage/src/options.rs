//! Per-table options storage needs: ordering, reconciliation, tombstone GC, compaction.

use crate::compaction::CompactionParams;
use crate::compare::{BytesComparator, ClusteringComparator};
use crate::reconcile::Reconcile;
use crate::sstable::Codec;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// ScyllaDB's `tombstone_gc` mode (Cassandra always uses `Timeout`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TombstoneGcMode {
    /// After `gc_grace_seconds`.
    #[default]
    Timeout,
    /// Never.
    Disabled,
    /// As soon as compaction sees them (still never if they could shadow data elsewhere).
    Immediate,
    /// After repair. A database is its single replica and there is nothing to repair, so this
    /// behaves as `Timeout`.
    Repair,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcPolicy {
    pub mode: TombstoneGcMode,
    pub gc_grace_seconds: i64,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self { mode: TombstoneGcMode::Timeout, gc_grace_seconds: 864_000 }
    }
}

impl GcPolicy {
    /// Tombstones whose local deletion time is before this (seconds) may be purged; `i64::MIN`
    /// when none may (`ColumnFamilyStore.gcBefore`).
    pub fn gc_before(&self, now: i64) -> i64 {
        match self.mode {
            TombstoneGcMode::Disabled => i64::MIN,
            // Deletions at `now` are purgeable (`<` below `now + 1`).
            TombstoneGcMode::Immediate => now.saturating_add(1),
            TombstoneGcMode::Timeout | TombstoneGcMode::Repair => now.saturating_sub(self.gc_grace_seconds.max(0)),
        }
    }
}

/// How a table is ordered and maintained. Set by the engine when it attaches a table, and again
/// when `ALTER TABLE` changes an option.
#[derive(Clone)]
pub struct TableOptions {
    pub comparator: Arc<dyn ClusteringComparator>,
    pub reconcile: Reconcile,
    pub gc: GcPolicy,
    pub compaction: CompactionParams,
    pub bloom_filter_fp_chance: f64,
    /// Materialized views: a row exists only through its row marker.
    pub strict_liveness: bool,
    /// Flush the memtable once it holds this much.
    pub memtable_flush_bytes: u64,
    /// `memtable_flush_period_in_ms` (0: none).
    pub memtable_flush_period_ms: u64,
    pub block_size: usize,
    pub codec: Codec,
}

impl Default for TableOptions {
    fn default() -> Self {
        Self {
            comparator: Arc::new(BytesComparator),
            reconcile: Reconcile::default(),
            gc: GcPolicy::default(),
            compaction: CompactionParams::default(),
            bloom_filter_fp_chance: 0.01,
            strict_liveness: false,
            memtable_flush_bytes: 64 << 20,
            memtable_flush_period_ms: 0,
            block_size: crate::sstable::writer::DEFAULT_BLOCK_SIZE,
            codec: Codec::default(),
        }
    }
}

impl std::fmt::Debug for TableOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableOptions")
            .field("reconcile", &self.reconcile)
            .field("gc", &self.gc)
            .field("compaction", &self.compaction)
            .field("strict_liveness", &self.strict_liveness)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_before_by_mode() {
        let p = |mode, grace| GcPolicy { mode, gc_grace_seconds: grace };
        assert_eq!(p(TombstoneGcMode::Timeout, 10).gc_before(100), 90);
        assert_eq!(p(TombstoneGcMode::Repair, 10).gc_before(100), 90);
        assert_eq!(p(TombstoneGcMode::Disabled, 10).gc_before(100), i64::MIN);
        assert_eq!(p(TombstoneGcMode::Immediate, 10).gc_before(100), 101);
        assert_eq!(p(TombstoneGcMode::Timeout, 0).gc_before(100), 100);
    }
}

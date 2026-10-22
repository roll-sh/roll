//! ScyllaDB's per-partition rate limit (the `per_partition_rate_limit` table option), as its
//! design notes (`docs/dev/per-partition-rate-limit.md`) describe it: one counter per (table,
//! token, operation), halved every second; an operation is accounted, then admitted with
//! probability `limit / (count * ln 2)`, drawn independently per partition. A rejected operation
//! still counts.
//!
//! What counts, as measured on `scylladb/scylla:2026.3.3`: reads that name their partitions
//! (`k = ?`, `k IN (…)`, slices, filtering, aggregates, `SERIAL`), each page a new read;
//! writes (`INSERT`, `UPDATE`, `DELETE`, JSON, TTL) and batches that touch one mutation (one
//! table and partition) or are `UNLOGGED`. Range and token scans, `DISTINCT` scans, LWT,
//! counter updates and logged multi-mutation batches (the batch log) are never limited.
//!
//! The error is `Per-partition rate limit reached for <read|write> in table <ks>.<t>, rejected by
//! coordinator`: `RATE_LIMIT_ERROR` (`0xF000`, op type and coordinator flag) on connections that
//! negotiated `SCYLLA_RATE_LIMIT_ERROR`, `CONFIG_ERROR` otherwise (the server's conversion).

use std::collections::HashMap;

use nosql_storage::manifest::TableId;
use parking_lot::Mutex;

use crate::database::Database;
use crate::error::{Error, ErrorCode, ErrorDetail, Result};
use crate::schema::{Table, TableKind};

/// Counters kept before idle ones (decayed to zero) are dropped.
const PRUNE_AT: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Read,
    Write,
}

impl Op {
    fn option(self) -> &'static str {
        match self {
            Op::Read => "max_reads_per_second",
            Op::Write => "max_writes_per_second",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
        }
    }

    /// The `op_type` byte of `RATE_LIMIT_ERROR`.
    fn code(self) -> u8 {
        match self {
            Op::Read => 0,
            Op::Write => 1,
        }
    }
}

#[derive(Default)]
pub struct Limiter {
    counters: Mutex<HashMap<(TableId, i64, Op), Counter>>,
}

struct Counter {
    count: u64,
    /// The second the count was last halved up to.
    second: u64,
}

impl Limiter {
    /// Accounts one operation at `second` and returns the partition's count.
    pub fn account(&self, table: TableId, token: i64, op: Op, second: u64) -> u64 {
        let mut m = self.counters.lock();
        if m.len() >= PRUNE_AT {
            m.retain(|_, c| decayed(c, second) > 0);
        }
        let c = m
            .entry((table, token, op))
            .or_insert(Counter { count: 0, second });
        c.count = decayed(c, second) + 1;
        c.second = second;
        c.count
    }
}

fn decayed(c: &Counter, second: u64) -> u64 {
    let halvings = second.saturating_sub(c.second);
    if halvings >= 64 {
        0
    } else {
        c.count >> halvings
    }
}

/// Whether an operation at a partition whose counter reached `count` is admitted (`draw` is
/// uniform in `[0, 1)`).
pub fn admitted(limit: u64, count: u64, draw: f64) -> bool {
    draw < limit as f64 / (count as f64 * std::f64::consts::LN_2)
}

/// The table's limit for `op`, when it has one.
pub fn limit(t: &Table, op: Op) -> Option<u64> {
    t.params
        .per_partition_rate_limit
        .get(op.option())
        .and_then(|v| v.trim().parse().ok())
}

/// Accounts `op` on each partition (`tokens`) of `t`; fails when any of them rejects it.
pub fn check(db: &Database, t: &Table, tokens: &[i64], op: Op) -> Result<()> {
    if !db.dialect().is_scylla() || t.kind != TableKind::Regular || t.is_virtual() {
        return Ok(());
    }
    let Some(limit) = limit(t, op) else {
        return Ok(());
    };
    let second = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut ok = true;
    for &token in tokens {
        let count = db.rate_limits.account(t.table_id(), token, op, second);
        ok &= admitted(limit, count, rand::random::<f64>());
    }
    if ok {
        return Ok(());
    }
    Err(Error {
        detail: ErrorDetail::RateLimit {
            op_type: op.code(),
            rejected_by_coordinator: true,
        },
        ..Error::new(
            ErrorCode::RateLimit,
            format!(
                "Per-partition rate limit reached for {} in table {}.{}, rejected by coordinator",
                op.name(),
                t.keyspace,
                t.name
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_halve_every_second() {
        let l = Limiter::default();
        let t = TableId(1);
        for _ in 0..8 {
            l.account(t, 5, Op::Write, 100);
        }
        assert_eq!(l.account(t, 5, Op::Write, 100), 9);
        // Reads and other partitions count apart.
        assert_eq!(l.account(t, 5, Op::Read, 100), 1);
        assert_eq!(l.account(t, 6, Op::Write, 100), 1);
        assert_eq!(l.account(TableId(2), 5, Op::Write, 100), 1);
        // 9 → 4 a second later, → 0 (+1) after many.
        assert_eq!(l.account(t, 5, Op::Write, 101), 5);
        assert_eq!(l.account(t, 5, Op::Write, 103), 2);
        assert_eq!(l.account(t, 5, Op::Write, 1_000), 1);
    }

    #[test]
    fn admission_probability() {
        // Up to `limit / ln 2` operations always pass; beyond, `limit / (x ln 2)`.
        assert!(admitted(1, 1, 0.999));
        assert!(!admitted(1, 2, 0.75));
        assert!(admitted(1, 2, 0.7));
        assert!(admitted(100, 144, 0.999));
        assert!(!admitted(100, 1_000, 0.2));
        assert!(admitted(100, 1_000, 0.1));
    }

    #[test]
    fn idle_counters_are_pruned() {
        let l = Limiter::default();
        for tok in 0..PRUNE_AT as i64 {
            l.account(TableId(1), tok, Op::Read, 10);
        }
        l.account(TableId(1), -1, Op::Read, 20);
        assert_eq!(l.counters.lock().len(), 1);
    }
}

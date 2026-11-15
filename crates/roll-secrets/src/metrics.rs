//! What roll secrets counts for rolld's metrics (docs/secrets.md, "Metrics"): counters kept in
//! memory since the process started (rolld publishes their change each minute), and gauges read
//! from the store when asked. Never a value, a key's name or a project: numbers only.

use crate::{Result, Secrets};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub(crate) struct Counters {
    pub reads: AtomicU64,
    pub dynamic_issued: AtomicU64,
    pub leases_revoked: AtomicU64,
    pub revoke_failures: AtomicU64,
    pub syncs_pushed: AtomicU64,
    pub sync_failures: AtomicU64,
    pub rotations: AtomicU64,
    pub rotation_failures: AtomicU64,
    pub kms_refusals: AtomicU64,
}

impl Counters {
    pub(crate) fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }
}

/// A snapshot: the counters since start, and the store's gauges now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Metrics {
    /// Values read and resolved (each folder read, by a person, a token or a runtime).
    pub reads: u64,
    /// Dynamic credentials issued (a lease each).
    pub dynamic_issued: u64,
    pub leases_revoked: u64,
    /// Revocations the product refused (the sweeper tries again).
    pub revoke_failures: u64,
    pub syncs_pushed: u64,
    pub sync_failures: u64,
    /// Scheduled rotations that succeeded, and that failed.
    pub rotations: u64,
    pub rotation_failures: u64,
    /// A customer's KMS refusing to open a project's data key.
    pub kms_refusals: u64,
    /// Gauges: leases not yet ended, syncs whose last push failed, and credentials whose last
    /// scheduled rotation failed.
    pub leases_live: u64,
    pub syncs_failing: u64,
    pub rotations_failing: u64,
}

impl Secrets {
    /// The counters since this process started and the store's gauges (rolld publishes them).
    pub fn metrics(&self) -> Result<Metrics> {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let (leases_live, syncs_failing, rotations_failing) = self.db.read(|db| {
            let n = |sql: &str| -> code_sql::Result<u64> { db.query_row(sql, (), |r| r.get::<_, i64>(0)).map(|n| n.max(0) as u64) };
            Ok::<_, crate::Error>((
                n("SELECT COUNT(*) FROM leases WHERE revoked_at IS NULL")?,
                n("SELECT COUNT(*) FROM syncs WHERE status = 'error' OR error IS NOT NULL")?,
                n("SELECT COUNT(*) FROM secrets WHERE rotation_error IS NOT NULL")?,
            ))
        })?;
        Ok(Metrics {
            reads: get(&c.reads),
            dynamic_issued: get(&c.dynamic_issued),
            leases_revoked: get(&c.leases_revoked),
            revoke_failures: get(&c.revoke_failures),
            syncs_pushed: get(&c.syncs_pushed),
            sync_failures: get(&c.sync_failures),
            rotations: get(&c.rotations),
            rotation_failures: get(&c.rotation_failures),
            kms_refusals: get(&c.kms_refusals),
            leases_live,
            syncs_failing,
            rotations_failing,
        })
    }
}

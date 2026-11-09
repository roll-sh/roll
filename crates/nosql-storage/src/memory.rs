//! Memory budgets of a node: memtables (a node-wide budget and a cap per database, flushes on
//! pressure, write backpressure), statistics, and the allocator hooks that hand freed memory
//! back to the OS.
//!
//! # Accounting
//!
//! Every memtable of a database charges the database's [`DbMemory`], which charges the node's
//! [`MemtableBudget`]. A memtable gives its bytes back when it is dropped: after its flush, once
//! no reader holds it. So the numbers are what memtables really pin, frozen ones included.
//!
//! # Enforcement
//!
//! It follows Cassandra's `memtable_heap_space` / `memtable_cleanup_threshold` and ScyllaDB's
//! dirty memory manager.
//!
//! - **Flush.** When a database's unflushed memtables reach half its cap, or the node's reach
//!   half the budget, the largest active memtables are flushed (`Store::relieve`; at most
//!   `StoreConfig::flush_concurrency` flushes write at once).
//! - **Backpressure.** A write waits while its database holds its cap, or while the node holds
//!   its budget and the database holds at least its fair share (the budget divided by the
//!   databases holding memtable data). Past the budget plus an eighth, every write waits. So a
//!   tenant that writes faster than it flushes is slowed down, and the others are not.
//! - **Timeout.** A write that waits longer than the write timeout fails with [`Overloaded`].
//!   The engine answers it as both references answer a write their memtables can't take: a
//!   `WriteTimeout`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Notify;

/// A write waited longer than the write timeout for memtable memory (or the commit log) to
/// be released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overloaded {
    pub waited_ms: u64,
}

impl std::fmt::Display for Overloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the write waited {} ms for memtable memory to be flushed", self.waited_ms)
    }
}

impl std::error::Error for Overloaded {}

#[derive(Debug, Default)]
pub struct BudgetMetrics {
    /// Writes that had to wait for memory.
    pub blocked_writes: AtomicU64,
    /// Writes waiting right now.
    pub blocked_now: AtomicU64,
    /// Writes that gave up ([`Overloaded`]).
    pub write_timeouts: AtomicU64,
    /// Flushes started because a budget was under pressure.
    pub pressure_flushes: AtomicU64,
}

/// The node's memtable budget.
pub struct MemtableBudget {
    limit: AtomicU64,
    used: AtomicU64,
    /// Databases holding memtable data (for the fair share).
    holders: AtomicU64,
    /// Memtable memory was released (waiting writers look again).
    released: Notify,
    /// Memory is past a flush threshold (the store's relief task flushes).
    pressure: Notify,
    pressure_pending: AtomicBool,
    pub metrics: BudgetMetrics,
}

impl MemtableBudget {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicU64::new(limit.max(1)),
            used: AtomicU64::new(0),
            holders: AtomicU64::new(0),
            released: Notify::new(),
            pressure: Notify::new(),
            pressure_pending: AtomicBool::new(false),
            metrics: BudgetMetrics::default(),
        })
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit.max(1), Ordering::Relaxed);
        self.released.notify_waiters();
    }

    /// Bytes held by every accounted memtable.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn holders(&self) -> u64 {
        self.holders.load(Ordering::Relaxed)
    }

    /// Wake the relief task (at most one pending wake-up).
    pub(crate) fn request_relief(&self) {
        if !self.pressure_pending.swap(true, Ordering::AcqRel) {
            self.pressure.notify_one();
        }
    }

    /// Wait until relief is requested (or `max`).
    pub(crate) async fn relief_requested(&self, max: Duration) {
        let _ = tokio::time::timeout(max, self.pressure.notified()).await;
        self.pressure_pending.store(false, Ordering::Release);
    }

    /// A per-database account.
    pub fn account(self: &Arc<Self>, cap: u64) -> Arc<DbMemory> {
        Arc::new(DbMemory { node: self.clone(), used: AtomicU64::new(0), cap: AtomicU64::new(cap.max(1)) })
    }
}

/// One database's memtable memory.
pub struct DbMemory {
    node: Arc<MemtableBudget>,
    used: AtomicU64,
    cap: AtomicU64,
}

impl DbMemory {
    pub fn node(&self) -> &Arc<MemtableBudget> {
        &self.node
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// The cap (never above the node's budget).
    pub fn cap(&self) -> u64 {
        self.cap.load(Ordering::Relaxed).min(self.node.limit())
    }

    pub(crate) fn set_cap(&self, cap: u64) {
        self.cap.store(cap.max(1), Ordering::Relaxed);
        self.node.released.notify_waiters();
        self.node.request_relief();
    }

    pub(crate) fn charge(&self, n: u64) {
        if n == 0 {
            return;
        }
        let before = self.used.fetch_add(n, Ordering::Relaxed);
        if before == 0 {
            self.node.holders.fetch_add(1, Ordering::Relaxed);
        }
        let node = self.node.used.fetch_add(n, Ordering::Relaxed) + n;
        if before + n >= self.cap() / 2 || node >= self.node.limit() / 2 {
            self.node.request_relief();
        }
    }

    pub(crate) fn release(&self, n: u64) {
        if n == 0 {
            return;
        }
        if self.used.fetch_sub(n, Ordering::Relaxed) == n {
            self.node.holders.fetch_sub(1, Ordering::Relaxed);
        }
        self.node.used.fetch_sub(n, Ordering::Relaxed);
        self.node.released.notify_waiters();
    }

    /// Whether a write must wait for memory (see the module docs).
    pub fn must_wait(&self) -> bool {
        let used = self.used();
        if used >= self.cap() {
            return true;
        }
        let node = self.node.used();
        let limit = self.node.limit();
        if node >= limit.saturating_add(limit / 8) {
            return true;
        }
        node >= limit && used >= limit / self.node.holders().max(1)
    }

    /// Wait until a write may go ahead, at most `timeout`.
    pub(crate) async fn admit(&self, timeout: Duration) -> Result<(), Overloaded> {
        if !self.must_wait() {
            return Ok(());
        }
        let m = &self.node.metrics;
        m.blocked_writes.fetch_add(1, Ordering::Relaxed);
        m.blocked_now.fetch_add(1, Ordering::Relaxed);
        let started = tokio::time::Instant::now();
        let deadline = started + timeout;
        let r = loop {
            let released = self.node.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !self.must_wait() {
                break Ok(());
            }
            self.node.request_relief();
            let now = tokio::time::Instant::now();
            if now >= deadline {
                m.write_timeouts.fetch_add(1, Ordering::Relaxed);
                break Err(Overloaded { waited_ms: (now - started).as_millis() as u64 });
            }
            // Caps change without a release: look again now and then.
            let _ = tokio::time::timeout((deadline - now).min(Duration::from_millis(50)), released).await;
        };
        m.blocked_now.fetch_sub(1, Ordering::Relaxed);
        r
    }
}

/// The process's resident memory (`/proc/self/status`; `None` where there is none).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProcessMemory {
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
}

pub fn process_memory() -> Option<ProcessMemory> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = |k: &str| s.lines().find(|l| l.starts_with(k))?.split_whitespace().nth(1)?.parse::<u64>().ok().map(|v| v << 10);
    Some(ProcessMemory { rss_bytes: kib("VmRSS:")?, peak_rss_bytes: kib("VmHWM:")? })
}

static RELEASE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// How to make the process allocator return freed memory to the OS now (`mi_collect(true)`
/// with mimalloc): the binary that links the allocator sets it, once, at startup.
pub fn set_allocator_release(f: fn()) {
    let _ = RELEASE.set(f);
}

/// Return freed memory to the OS, when the allocator can (after an unload).
pub fn release_to_os() {
    if let Some(f) = RELEASE.get() {
        f();
    }
}

/// The node's memory: budgets and what holds memory now (`GET /v1/nosql`, metrics).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MemoryStats {
    pub memtable_budget_bytes: u64,
    /// The default per-database cap.
    pub memtable_db_bytes: u64,
    pub memtable_bytes: u64,
    /// Of `memtable_bytes`: frozen memtables waiting for or in a flush.
    pub memtable_flushing_bytes: u64,
    pub databases_with_memtables: u64,
    pub blocked_writes: u64,
    pub blocked_writes_now: u64,
    pub write_timeouts: u64,
    pub pressure_flushes: u64,
    pub flushes_running: u64,
    pub flush_concurrency: u64,
    pub compactions_running: u64,
    pub compaction_concurrency: u64,
    pub cache_memory_bytes: u64,
    pub cache_memory_budget_bytes: u64,
    pub cache_disk_bytes: u64,
    pub cache_disk_budget_bytes: u64,
    /// SSTable indexes and bloom filters of loaded tables (always in memory).
    pub sstable_index_bytes: u64,
    /// Commit log frames waiting to ship.
    pub log_pending_bytes: u64,
    pub log_pending_limit_bytes: u64,
    pub process: Option<ProcessMemory>,
}

/// One database's memory (`activity`, `nodetool`-like stats).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DbMemoryStats {
    pub memtable_bytes: u64,
    pub memtable_cap_bytes: u64,
    /// Blocks of its SSTables in the node's caches (memory and disk).
    pub cache_bytes: u64,
    pub sstable_index_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accounts_caps_fair_share_and_timeouts() {
        let node = MemtableBudget::new(1000);
        let a = node.account(600);
        let b = node.account(600);
        a.charge(500);
        assert_eq!((a.used(), node.used(), node.holders()), (500, 500, 1));
        assert!(!a.must_wait());
        a.charge(100);
        // At its cap.
        assert!(a.must_wait());
        assert!(!b.must_wait());
        b.charge(450);
        // The node is past its budget: `a` (600) holds more than its share (500), `b` (450)
        // less, so `b` may still write until the budget plus an eighth.
        assert_eq!(node.used(), 1050);
        assert!(a.must_wait());
        assert!(!b.must_wait());
        b.charge(100);
        assert!(b.must_wait());
        // A waiting write goes ahead once memory is released, or times out.
        let r = tokio::time::timeout(Duration::from_secs(5), async {
            let w = b.admit(Duration::from_secs(5));
            tokio::pin!(w);
            assert!(futures::poll!(w.as_mut()).is_pending());
            a.release(600);
            w.await
        })
        .await
        .unwrap();
        assert_eq!(r, Ok(()));
        assert_eq!(node.holders(), 1);
        b.charge(50);
        assert!(b.must_wait());
        let e = b.admit(Duration::from_millis(30)).await.unwrap_err();
        assert!(e.waited_ms >= 30);
        assert_eq!(node.metrics.write_timeouts.load(Ordering::Relaxed), 1);
        assert_eq!(node.metrics.blocked_writes.load(Ordering::Relaxed), 2);
        assert_eq!(node.metrics.blocked_now.load(Ordering::Relaxed), 0);
        // A larger cap lets it write again; a cap is never above the node's budget.
        b.set_cap(10_000);
        assert_eq!(b.cap(), 1000);
        assert!(!b.must_wait());
        b.release(600);
        assert_eq!((node.used(), node.holders()), (0, 0));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn process_memory_reads_proc() {
        let m = process_memory().unwrap();
        assert!(m.rss_bytes > 0 && m.peak_rss_bytes >= m.rss_bytes);
    }
}

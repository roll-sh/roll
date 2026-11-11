//! The cluster's WAL position: the LSNs clients see (pg_current_wal_lsn, slots' positions,
//! the change stream's records). The engine has no PostgreSQL WAL; every commit takes a
//! range of positions from one cluster-wide counter instead, so positions only ever increase
//! and every database's commits are ordered on one axis, as in PostgreSQL. The counter is
//! persisted ahead of use (`RESERVE` at a time, like transaction ids), so a restart, even
//! after a crash, continues past every position handed out before.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where a new cluster's WAL starts (an initdb'd PostgreSQL cluster is at about this point).
pub const INITIAL: u64 = crate::exec::pgstat::INITDB_LSN;
/// How far ahead of use positions are persisted.
const RESERVE: u64 = 64 << 20;

impl std::fmt::Debug for WalPos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WalPos({})", fmt(self.current()))
    }
}

pub struct WalPos {
    /// The next free position (the insert position).
    cur: AtomicU64,
    /// Positions below this may have been handed out (persisted).
    reserved: AtomicU64,
    extend: Mutex<()>,
    persist: Box<dyn Fn(u64) + Send + Sync>,
}

/// MAXALIGN of a record's length, as WAL records are aligned.
pub fn align(n: u64) -> u64 {
    (n + 7) & !7
}

impl WalPos {
    /// Positions continue from `reserved` (0: a new cluster), persisted with `persist`.
    pub fn new(reserved: u64, persist: impl Fn(u64) + Send + Sync + 'static) -> WalPos {
        let start = reserved.max(INITIAL);
        let w = WalPos {
            cur: AtomicU64::new(start),
            reserved: AtomicU64::new(0),
            extend: Mutex::new(()),
            persist: Box::new(persist),
        };
        w.reserve(start + 1);
        w
    }

    /// For an engine without a cluster (tests, embedding): nothing persisted.
    pub fn local() -> WalPos {
        WalPos::new(0, |_| {})
    }

    /// The insert (and flush) position: the end of the last record.
    pub fn current(&self) -> u64 {
        self.cur.load(Ordering::Acquire)
    }

    /// Take `n` bytes of positions (aligned, starting at an aligned position); the first one.
    pub fn alloc(&self, n: u64) -> u64 {
        let n = align(n.max(8));
        let mut at = 0;
        let _ = self.cur.try_update(Ordering::AcqRel, Ordering::Acquire, |c| {
            at = align(c);
            Some(at + n)
        });
        if at + n > self.reserved.load(Ordering::Acquire) {
            self.reserve(at + n);
        }
        at
    }

    /// Move the insert position past `n` bytes exactly (pg_stat_wal's estimate of a
    /// transaction's records, `pgstat::Fixed::advance`, so the position moves by what
    /// wal_bytes counts).
    pub fn skip(&self, n: u64) {
        let at = self.cur.fetch_add(n, Ordering::AcqRel);
        if at + n > self.reserved.load(Ordering::Acquire) {
            self.reserve(at + n);
        }
    }

    /// Move the insert position to at least `lsn` (pg_switch_wal's next segment).
    pub fn advance_to(&self, lsn: u64) {
        let prev = self.cur.fetch_max(lsn, Ordering::AcqRel);
        if lsn > prev && lsn > self.reserved.load(Ordering::Acquire) {
            self.reserve(lsn);
        }
    }

    /// Persist the current reservation again (the first one, made before whoever persists
    /// it was ready).
    pub fn persist_reserved(&self) {
        let _g = self.extend.lock();
        (self.persist)(self.reserved.load(Ordering::Acquire));
    }

    fn reserve(&self, upto: u64) {
        let _g = self.extend.lock();
        if upto <= self.reserved.load(Ordering::Acquire) {
            return;
        }
        let r = upto + RESERVE;
        (self.persist)(r);
        self.reserved.store(r, Ordering::Release);
    }
}

/// An LSN as pg_lsn prints it (`%X/%X`).
pub fn fmt(lsn: u64) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn positions_increase_and_persist_ahead() {
        let saved = Arc::new(AtomicU64::new(0));
        let s = saved.clone();
        let w = WalPos::new(0, move |n| s.store(n, Ordering::SeqCst));
        let a = w.alloc(10);
        let b = w.alloc(100);
        assert!(b >= a + 16 && w.current() >= b + 104);
        assert!(saved.load(Ordering::SeqCst) > w.current());
        // A restart continues past everything handed out.
        let w2 = WalPos::new(saved.load(Ordering::SeqCst), |_| {});
        assert!(w2.alloc(1) > b);
    }
}

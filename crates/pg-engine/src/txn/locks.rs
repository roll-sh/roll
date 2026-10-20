//! Row and object locks with blocking waits, deadlock detection, cancellation and timeouts.

use crate::error::{Error, Result, code};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// FOR KEY SHARE / FOR SHARE: many holders.
    Shared,
    /// Writes, FOR UPDATE, FOR NO KEY UPDATE.
    Exclusive,
}

#[derive(Default)]
struct Entry {
    /// Exclusive holders: one, or several owners of the same backend (its session and its
    /// transaction, whose locks never conflict with each other).
    exclusive: Vec<u64>,
    shared: Vec<u64>,
}

impl Entry {
    fn is_free(&self) -> bool {
        self.exclusive.is_empty() && self.shared.is_empty()
    }

    /// Transactions that block `txn` (or its backend's `peer` owner) from taking `mode`.
    fn blockers(&self, txn: u64, peer: Option<u64>, mode: LockMode) -> Vec<u64> {
        let mine = |t: &u64| *t == txn || Some(*t) == peer;
        let mut b: Vec<u64> = self.exclusive.iter().copied().filter(|t| !mine(t)).collect();
        if mode == LockMode::Exclusive {
            b.extend(self.shared.iter().copied().filter(|t| !mine(t)));
        }
        b
    }
}

#[derive(Default)]
struct State {
    locks: HashMap<Vec<u8>, Entry>,
    /// waiter → the transactions it waits for
    waits: HashMap<u64, Vec<u64>>,
}

#[derive(Default)]
pub struct LockManager {
    state: Mutex<State>,
    cv: Condvar,
}

pub enum WaitPolicy {
    Block,
    NoWait,
    SkipLocked,
}

impl LockManager {
    /// Acquire `key` for `txn`; what `peer` (the same backend's other owner) holds doesn't
    /// conflict. Returns Ok(false) only for SKIP LOCKED when the lock is taken.
    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        &self,
        key: &[u8],
        txn: u64,
        peer: Option<u64>,
        mode: LockMode,
        policy: WaitPolicy,
        cancel: &AtomicBool,
        timeout: Option<Duration>,
        what: &dyn Fn() -> String,
    ) -> Result<bool> {
        let start = Instant::now();
        let mut s = self.state.lock();
        loop {
            let entry = s.locks.entry(key.to_vec()).or_default();
            let blockers = entry.blockers(txn, peer, mode);
            if blockers.is_empty() {
                match mode {
                    LockMode::Exclusive => {
                        if !entry.exclusive.contains(&txn) {
                            entry.exclusive.push(txn);
                        }
                        entry.shared.retain(|t| *t != txn);
                    }
                    LockMode::Shared => {
                        if !entry.exclusive.contains(&txn) && !entry.shared.contains(&txn) {
                            entry.shared.push(txn);
                        }
                    }
                }
                s.waits.remove(&txn);
                return Ok(true);
            }
            match policy {
                WaitPolicy::NoWait => {
                    return Err(Error::new(
                        code::LOCK_NOT_AVAILABLE,
                        format!("could not obtain lock on {}", what()),
                    ));
                }
                WaitPolicy::SkipLocked => return Ok(false),
                WaitPolicy::Block => {}
            }
            // Deadlock check: does any blocker (transitively) wait for us?
            let mut stack = blockers.clone();
            let mut seen = std::collections::HashSet::new();
            while let Some(t) = stack.pop() {
                if t == txn {
                    s.waits.remove(&txn);
                    return Err(Error::new(code::DEADLOCK_DETECTED, "deadlock detected")
                        .detail(format!("Process {txn} waits for ShareLock on transaction {}; blocked by process {}.", blockers[0], blockers[0]))
                        .hint("See server log for query details."));
                }
                if seen.insert(t)
                    && let Some(w) = s.waits.get(&t)
                {
                    stack.extend(w.iter().copied());
                }
            }
            s.waits.insert(txn, blockers);
            if cancel.load(Ordering::Relaxed) {
                s.waits.remove(&txn);
                return Err(Error::new(
                    code::QUERY_CANCELED,
                    "canceling statement due to user request",
                ));
            }
            if let Some(t) = timeout
                && start.elapsed() >= t
            {
                s.waits.remove(&txn);
                return Err(Error::new(
                    code::LOCK_NOT_AVAILABLE,
                    "canceling statement due to lock timeout",
                ));
            }
            self.cv.wait_for(&mut s, Duration::from_millis(50));
        }
    }

    pub fn release_all(&self, txn: u64, keys: impl IntoIterator<Item = Vec<u8>>) {
        let mut s = self.state.lock();
        for k in keys {
            if let Some(e) = s.locks.get_mut(&k) {
                e.exclusive.retain(|t| *t != txn);
                e.shared.retain(|t| *t != txn);
                if e.is_free() {
                    s.locks.remove(&k);
                }
            }
        }
        s.waits.remove(&txn);
        drop(s);
        self.cv.notify_all();
    }

    /// The keys `owner` holds, with the mode (pg_locks).
    pub fn held_by(&self, owner: u64) -> Vec<(Vec<u8>, LockMode)> {
        let s = self.state.lock();
        s.locks
            .iter()
            .filter_map(|(k, e)| {
                if e.exclusive.contains(&owner) {
                    Some((k.clone(), LockMode::Exclusive))
                } else if e.shared.contains(&owner) {
                    Some((k.clone(), LockMode::Shared))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Whether anyone holds a lock (for idle detection and tests).
    pub fn held_count(&self) -> usize {
        self.state.lock().locks.len()
    }
}

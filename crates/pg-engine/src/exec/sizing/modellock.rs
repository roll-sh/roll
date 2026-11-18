//! The lock of a shard of a database's size models (`Models`): a reader-writer lock with a third
//! mode, a group of writers that each change only state of their own (`RelFiles::hot`): a heap
//! page under its page's latch, a row's current version, counters. Writers in the group don't
//! wait for each other (two UPDATEs of rows on different pages of one table, as PostgreSQL's
//! buffer locks let them run), and exclude readers and other writers as one writer does.
//!
//! The group holds the lock exclusively: its first member takes it, later ones join while it is
//! held and nobody waits for it, and the last one to leave lets it go. A thread waiting for the
//! lock in either mode closes the group to new members, so it gets the lock once they leave.

use parking_lot::lock_api::{self, RawRwLock as _};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct RawModelLock {
    raw: parking_lot::RawRwLock,
    /// Threads waiting for `raw` (blocked or about to block): the group takes no member then.
    waiting: AtomicU32,
    /// The members of the group holding `raw` exclusively (0: no group holds it).
    group: AtomicU32,
}

unsafe impl lock_api::RawRwLock for RawModelLock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: RawModelLock = RawModelLock {
        raw: <parking_lot::RawRwLock as lock_api::RawRwLock>::INIT,
        waiting: AtomicU32::new(0),
        group: AtomicU32::new(0),
    };
    // (As parking_lot's own. A group's last member lets go of a lock its first member took,
    // through the raw lock, which allows that: its guards are this module's.)
    type GuardMarker = lock_api::GuardNoSend;

    fn lock_shared(&self) {
        if !self.raw.try_lock_shared() {
            self.waiting.fetch_add(1, Ordering::AcqRel);
            self.raw.lock_shared();
            self.waiting.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn try_lock_shared(&self) -> bool {
        self.raw.try_lock_shared()
    }

    unsafe fn unlock_shared(&self) {
        unsafe { self.raw.unlock_shared() }
    }

    fn lock_exclusive(&self) {
        if !self.raw.try_lock_exclusive() {
            self.waiting.fetch_add(1, Ordering::AcqRel);
            self.raw.lock_exclusive();
            self.waiting.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn try_lock_exclusive(&self) -> bool {
        self.raw.try_lock_exclusive()
    }

    unsafe fn unlock_exclusive(&self) {
        unsafe { self.raw.unlock_exclusive() }
    }

    fn is_locked(&self) -> bool {
        self.raw.is_locked()
    }

    fn is_locked_exclusive(&self) -> bool {
        self.raw.is_locked_exclusive()
    }
}

impl RawModelLock {
    /// Join the group holding the lock, or take it for a group of one.
    fn lock_group(&self) {
        loop {
            let g = self.group.load(Ordering::Acquire);
            if g == 0 {
                break;
            }
            if self.waiting.load(Ordering::Acquire) != 0 {
                // (Closed: wait for the lock as a writer would, and lead the next group.)
                self.lock_exclusive();
                self.group.store(1, Ordering::Release);
                return;
            }
            if self.group.compare_exchange_weak(g, g + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return;
            }
        }
        self.lock_exclusive();
        self.group.store(1, Ordering::Release);
    }

    /// Leave the group; the last member lets the lock go.
    ///
    /// # Safety
    /// The caller is a member (`lock_group`).
    unsafe fn unlock_group(&self) {
        loop {
            let g = self.group.load(Ordering::Acquire);
            debug_assert!(g != 0);
            if g == 1 {
                if self.group.compare_exchange_weak(1, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    unsafe { self.raw.unlock_exclusive() };
                    return;
                }
            } else if self.group.compare_exchange_weak(g, g - 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return;
            }
        }
    }
}

pub type ModelLock<T> = lock_api::RwLock<RawModelLock, T>;

/// A member's hold of the lock (`group`): shared access to what it guards, whose parts the
/// group's writers change are each behind a latch of their own or atomic.
pub struct GroupGuard<'a, T> {
    lock: &'a ModelLock<T>,
}

/// Join the writers' group of `lock` (`RawModelLock`).
pub fn group<T>(lock: &ModelLock<T>) -> GroupGuard<'_, T> {
    unsafe { lock.raw() }.lock_group();
    GroupGuard { lock }
}

impl<T> std::ops::Deref for GroupGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // (Shared with the group's other members only: readers and writers are excluded.)
        unsafe { &*self.lock.data_ptr() }
    }
}

impl<T> Drop for GroupGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { self.lock.raw().unlock_group() }
    }
}

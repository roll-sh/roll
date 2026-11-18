//! The lock of a shard of a database's size models (`Models`): a reader-writer lock with a third
//! mode, a group of writers that each change only state of their own (`Heap::update_hot_shared`:
//! a heap page under its page's latch, a row's current version, counts kept atomic). Writers in
//! the group don't wait for each other (two UPDATEs of rows on different pages of one table, as
//! PostgreSQL's buffer locks let them run), and exclude readers and other writers as one writer
//! does.
//!
//! The group holds the lock exclusively: its first member takes it, later ones join while it is
//! held, and the last one to leave lets it go. A reader or exclusive writer waiting for the lock
//! (an outsider) closes the group to new members, so it gets the lock once they leave; writers
//! arriving meanwhile wait for it to be served (`lock_group`), then form the next group together
//! instead of taking the lock one after another.

use parking_lot::lock_api::{self, RawRwLock as _};
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

pub struct RawModelLock {
    raw: parking_lot::RawRwLock,
    /// Readers and exclusive writers waiting for `raw` (blocked or about to block): the group
    /// takes no member while there are any.
    outsiders: AtomicU32,
    /// The members of the group holding `raw` exclusively (0: no group holds it).
    group: AtomicU32,
    /// Writers waiting to join or lead a group, and where they wait.
    candidates: AtomicU32,
    wait: (parking_lot::Mutex<()>, parking_lot::Condvar),
}

unsafe impl lock_api::RawRwLock for RawModelLock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: RawModelLock = RawModelLock {
        raw: <parking_lot::RawRwLock as lock_api::RawRwLock>::INIT,
        outsiders: AtomicU32::new(0),
        group: AtomicU32::new(0),
        candidates: AtomicU32::new(0),
        wait: (parking_lot::const_mutex(()), parking_lot::Condvar::new()),
    };
    // (As parking_lot's own. A group's last member lets go of a lock its first member took,
    // through the raw lock, which allows that: the group's guards are this module's.)
    type GuardMarker = lock_api::GuardNoSend;

    fn lock_shared(&self) {
        if !self.raw.try_lock_shared() {
            self.outsiders.fetch_add(1, SeqCst);
            self.raw.lock_shared();
            self.outsiders.fetch_sub(1, SeqCst);
        }
    }

    fn try_lock_shared(&self) -> bool {
        self.raw.try_lock_shared()
    }

    unsafe fn unlock_shared(&self) {
        unsafe { self.raw.unlock_shared() };
        self.wake();
    }

    fn lock_exclusive(&self) {
        if !self.raw.try_lock_exclusive() {
            self.outsiders.fetch_add(1, SeqCst);
            self.raw.lock_exclusive();
            self.outsiders.fetch_sub(1, SeqCst);
        }
    }

    fn try_lock_exclusive(&self) -> bool {
        self.raw.try_lock_exclusive()
    }

    unsafe fn unlock_exclusive(&self) {
        unsafe { self.raw.unlock_exclusive() };
        self.wake();
    }

    fn is_locked(&self) -> bool {
        self.raw.is_locked()
    }

    fn is_locked_exclusive(&self) -> bool {
        self.raw.is_locked_exclusive()
    }
}

impl RawModelLock {
    /// Writers waiting in `lock_group` look again: the lock was let go or a group formed.
    fn wake(&self) {
        if self.candidates.load(SeqCst) != 0 {
            let _g = self.wait.0.lock();
            self.wait.1.notify_all();
        }
    }

    /// Join the group holding the lock, or take it for a new group; while an outsider waits,
    /// or holds the lock, wait.
    fn lock_group(&self) {
        loop {
            if self.outsiders.load(SeqCst) == 0 {
                let g = self.group.load(SeqCst);
                if g != 0 {
                    if self.group.compare_exchange_weak(g, g + 1, SeqCst, SeqCst).is_ok() {
                        return;
                    }
                    continue;
                }
                if self.raw.try_lock_exclusive() {
                    self.group.store(1, SeqCst);
                    // (Writers waiting meanwhile join it.)
                    self.wake();
                    return;
                }
            }
            self.candidates.fetch_add(1, SeqCst);
            {
                let mut g = self.wait.0.lock();
                // (Looked at again under the mutex `wake` takes: no wakeup is missed. The
                // timeout bounds a wait on a lock whose holder lets go without `wake`, such as
                // a group's member taking it over.)
                let open = self.outsiders.load(SeqCst) == 0 && (self.group.load(SeqCst) != 0 || !self.raw.is_locked());
                if !open {
                    self.wait.1.wait_for(&mut g, std::time::Duration::from_micros(500));
                }
            }
            self.candidates.fetch_sub(1, SeqCst);
        }
    }

    /// Leave the group; the last member lets the lock go.
    ///
    /// # Safety
    /// The caller is a member (`lock_group`).
    unsafe fn unlock_group(&self) {
        loop {
            let g = self.group.load(SeqCst);
            debug_assert!(g != 0);
            if g == 1 {
                if self.group.compare_exchange_weak(1, 0, SeqCst, SeqCst).is_ok() {
                    unsafe { self.raw.unlock_exclusive() };
                    self.wake();
                    return;
                }
            } else if self.group.compare_exchange_weak(g, g - 1, SeqCst, SeqCst).is_ok() {
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
        // SAFETY: shared with the group's other members only: readers and exclusive writers
        // are kept out while a group holds the lock.
        unsafe { &*self.lock.data_ptr() }
    }
}

impl<T> Drop for GroupGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { self.lock.raw().unlock_group() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering::Relaxed};

    /// Readers, exclusive writers and group members of one lock: a group's members are never
    /// in while a reader or an exclusive writer is, exclusive writers are alone, and every
    /// thread gets through (an outsider closes the group, the group re-forms after it).
    #[test]
    fn modes_exclude_and_every_thread_gets_through() {
        // (Readers in, members in, exclusive writers in.)
        let lock = ModelLock::new([AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)]);
        let rounds = 2000;
        std::thread::scope(|s| {
            for t in 0..8 {
                let lock = &lock;
                s.spawn(move || {
                    for i in 0..rounds {
                        match (t + i) % 10 {
                            0 => {
                                let g = lock.write();
                                assert_eq!(g[0].load(Relaxed) + g[1].load(Relaxed) + g[2].load(Relaxed), 0);
                                g[2].fetch_add(1, Relaxed);
                                std::hint::spin_loop();
                                g[2].fetch_sub(1, Relaxed);
                            }
                            1 | 2 => {
                                let g = lock.read();
                                g[0].fetch_add(1, Relaxed);
                                assert_eq!(g[1].load(Relaxed) + g[2].load(Relaxed), 0);
                                g[0].fetch_sub(1, Relaxed);
                            }
                            _ => {
                                let g = group(lock);
                                g[1].fetch_add(1, Relaxed);
                                assert_eq!(g[0].load(Relaxed) + g[2].load(Relaxed), 0);
                                std::hint::spin_loop();
                                g[1].fetch_sub(1, Relaxed);
                            }
                        }
                    }
                });
            }
        });
        let g = lock.write();
        assert_eq!(g[0].load(Relaxed) + g[1].load(Relaxed) + g[2].load(Relaxed), 0);
    }
}

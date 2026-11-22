//! The node's sync slots, shared fairly by every cluster on the node.
//!
//! A node runs at most [`NODE_MAX`] syncs at once and at most [`OWNER_MAX`] of them for one
//! owner (the person a cluster belongs to; a cluster without one counts alone). A cluster that
//! can't get a slot waits in a FIFO queue. A slot that frees goes to the first waiter whose owner
//! is under its cap: that waiter's scheduler is woken, and nobody behind it (the cluster that
//! just freed the slot included) may take the slot first. A waiter counts only while it still
//! wants a slot (its cluster is loaded and has a runnable pending job, checked whenever the
//! queue is looked at), and a woken waiter that doesn't take its slot within [`WAKE_DEADLINE`]
//! is dropped, so no cluster holds up the queue, on purpose or not. A successful take wakes the
//! next waiter too, so slots that free together all find a taker, and so does dropping a waiter
//! that no longer counts. A waiter's check looks at a few of its own jobs (a map lookup each),
//! never every job, since it runs under the node's lock.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Syncs running at once on a node: each holds a page and, in a full sync, up to
/// `MAX_SEEN` ids' hashes.
pub const NODE_MAX: usize = 4;
/// Syncs running at once for one owner on a node.
pub const OWNER_MAX: usize = 2;
/// How long a woken waiter has to take its slot before it loses its place.
pub const WAKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

type Check = Arc<dyn Fn() -> bool + Send + Sync>;
type Wake = Arc<dyn Fn() + Send + Sync>;

/// A cluster waiting for a slot.
struct Waiter {
    key: String,
    owner: String,
    /// Whether it still wants a slot (its cluster is loaded and has a runnable pending job).
    alive: Check,
    /// Wakes its scheduler.
    wake: Wake,
    /// When it was woken for a slot.
    woken: Option<std::time::Instant>,
}

impl Waiter {
    fn counts(&self) -> bool {
        (self.alive)() && self.woken.is_none_or(|t| t.elapsed() < WAKE_DEADLINE)
    }
}

#[derive(Default)]
struct State {
    running: usize,
    owners: HashMap<String, usize>,
    waiting: VecDeque<Waiter>,
}

/// A node's slots.
pub struct Slots {
    max: usize,
    per_owner: usize,
    state: parking_lot::Mutex<State>,
}

impl std::fmt::Debug for Slots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slots({} per node, {} per owner)", self.max, self.per_owner)
    }
}

/// One running sync's hold on a slot; dropping it frees the slot for the next waiter.
pub struct Permit {
    slots: Arc<Slots>,
    owner: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut st = self.slots.state.lock();
            st.running -= 1;
            if let Some(n) = st.owners.get_mut(&self.owner) {
                *n -= 1;
                if *n == 0 {
                    st.owners.remove(&self.owner);
                }
            }
        }
        self.slots.wake_next();
    }
}

impl Slots {
    pub fn new(max: usize, per_owner: usize) -> Arc<Slots> {
        Arc::new(Slots { max, per_owner, state: Default::default() })
    }

    fn under_cap(&self, st: &State, owner: &str) -> bool {
        st.owners.get(owner).copied().unwrap_or(0) < self.per_owner
    }

    /// Whether a waiter ahead of `key` (all of them, when `key` isn't waiting) could take a slot.
    fn ahead(&self, st: &State, key: &str) -> bool {
        st.waiting.iter().take_while(|w| w.key != key).any(|w| self.under_cap(st, &w.owner))
    }

    /// Drops the waiters that no longer count: whether any went.
    fn prune(st: &mut State) -> bool {
        let before = st.waiting.len();
        st.waiting.retain(Waiter::counts);
        st.waiting.len() != before
    }

    /// Whether `key` (of `owner`) would get a slot now.
    pub fn admits(&self, key: &str, owner: &str) -> bool {
        let (admits, pruned) = {
            let mut st = self.state.lock();
            let pruned = Self::prune(&mut st);
            (st.running < self.max && self.under_cap(&st, owner) && !self.ahead(&st, key), pruned)
        };
        // A waiter that went may have stood before one that can take a free slot now.
        if pruned {
            self.wake_next();
        }
        admits
    }

    /// A slot for `key` (of `owner`) when it's its turn; otherwise `key` waits in the queue
    /// (once), and `wake` is called when a slot may be its.
    pub fn take(self: &Arc<Self>, key: &str, owner: &str, alive: Check, wake: Wake) -> Option<Permit> {
        let mut st = self.state.lock();
        let pruned = Self::prune(&mut st);
        if st.running < self.max && self.under_cap(&st, owner) && !self.ahead(&st, key) {
            st.waiting.retain(|w| w.key != key);
            st.running += 1;
            *st.owners.entry(owner.to_string()).or_default() += 1;
            drop(st);
            // Another slot may be free too: its waiter is woken now, not at the next release.
            self.wake_next();
            return Some(Permit { slots: self.clone(), owner: owner.to_string() });
        }
        if !st.waiting.iter().any(|w| w.key == key) {
            st.waiting.push_back(Waiter { key: key.to_string(), owner: owner.to_string(), alive, wake, woken: None });
        }
        drop(st);
        if pruned {
            self.wake_next();
        }
        None
    }

    /// The keys waiting, in order (tests and diagnostics).
    pub fn queued(&self) -> Vec<String> {
        self.state.lock().waiting.iter().map(|w| w.key.clone()).collect()
    }

    /// `key` wants no slot any more (nothing pending): it leaves the queue, and the next waiter
    /// is woken if a slot is free.
    pub fn leave(&self, key: &str) {
        let was = {
            let mut st = self.state.lock();
            let before = st.waiting.len();
            st.waiting.retain(|w| w.key != key);
            st.waiting.len() != before
        };
        if was {
            self.wake_next();
        }
    }

    /// Wakes the first waiter that may take a free slot.
    fn wake_next(&self) {
        let wake = {
            let mut st = self.state.lock();
            Self::prune(&mut st);
            if st.running >= self.max {
                return;
            }
            let st = &mut *st;
            let (owners, per_owner) = (&st.owners, self.per_owner);
            st.waiting.iter_mut().find(|w| owners.get(&w.owner).copied().unwrap_or(0) < per_owner).map(|w| {
                w.woken.get_or_insert_with(std::time::Instant::now);
                w.wake.clone()
            })
        };
        if let Some(w) = wake {
            w();
        }
    }
}

/// This node's slots.
pub fn node() -> &'static Arc<Slots> {
    static NODE: std::sync::LazyLock<Arc<Slots>> = std::sync::LazyLock::new(|| Slots::new(NODE_MAX, OWNER_MAX));
    &NODE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn waiter(woken: &Arc<AtomicUsize>) -> (Check, Wake) {
        let w = woken.clone();
        (Arc::new(|| true), Arc::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        }))
    }

    /// One owner can't hold every slot, and a slot that frees goes to the cluster that waited
    /// first, not back to the one that freed it.
    #[test]
    fn a_second_owner_runs_while_the_first_has_more_pending() {
        let slots = Slots::new(4, 2);
        let (a, b, c) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (aa, aw) = waiter(&a);
        // Owner A's cluster takes its two slots; a third is refused (and it waits).
        let a1 = slots.take("a", "alice", aa.clone(), aw.clone()).unwrap();
        let _a2 = slots.take("a", "alice", aa.clone(), aw.clone()).unwrap();
        assert!(slots.take("a", "alice", aa.clone(), aw.clone()).is_none(), "an owner holds at most two");
        // Owner B gets a slot though A has more pending.
        let (ba, bw) = waiter(&b);
        let b1 = slots.take("b", "bob", ba.clone(), bw.clone()).unwrap();
        let _b2 = slots.take("b", "bob", ba.clone(), bw.clone()).unwrap();
        // The node is full: C waits behind A.
        let (ca, cw) = waiter(&c);
        assert!(slots.take("c", "carol", ca.clone(), cw.clone()).is_none());
        // B frees one: A is still at its cap, so C (the first waiter that may take it) is woken,
        // and B can't take it back.
        drop(b1);
        assert_eq!((a.load(Ordering::SeqCst), c.load(Ordering::SeqCst)), (0, 1));
        assert!(slots.take("b", "bob", ba, bw).is_none(), "the freed slot isn't the freeing cluster's");
        let _c1 = slots.take("c", "carol", ca, cw).unwrap();
        // A frees one of its own: A (the first waiter, under its cap again) is woken.
        drop(a1);
        assert_eq!(a.load(Ordering::SeqCst), 1);
        // A cluster that no longer wants a slot doesn't hold up the queue.
        slots.leave("a");
        assert!(slots.admits("b", "bob"));
    }

    /// A waiter whose job stopped being runnable (canceled, its connector gone) without the
    /// cluster ever looking again doesn't block the queue: the next waiter gets the slot.
    #[test]
    fn a_waiter_with_nothing_pending_doesnt_hold_the_queue() {
        use std::sync::atomic::AtomicBool;
        let slots = Slots::new(1, 1);
        let held = slots.take("x", "xavier", Arc::new(|| true), Arc::new(|| {})).unwrap();
        let wants = Arc::new(AtomicBool::new(true));
        let (a_wakes, b_wakes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let w = wants.clone();
        let aw = a_wakes.clone();
        let a_wake: Wake = Arc::new(move || {
            aw.fetch_add(1, Ordering::SeqCst);
        });
        assert!(slots.take("a", "alice", Arc::new(move || w.load(Ordering::SeqCst)), a_wake).is_none());
        let bw = b_wakes.clone();
        let b_wake: Wake = Arc::new(move || {
            bw.fetch_add(1, Ordering::SeqCst);
        });
        assert!(slots.take("b", "bob", Arc::new(|| true), b_wake.clone()).is_none());
        assert_eq!(slots.queued(), ["a", "b"]);
        // A's job is canceled: A no longer wants a slot, and never says so.
        wants.store(false, Ordering::SeqCst);
        drop(held);
        assert_eq!((a_wakes.load(Ordering::SeqCst), b_wakes.load(Ordering::SeqCst)), (0, 1), "B is woken, not A");
        assert!(slots.take("b", "bob", Arc::new(|| true), b_wake.clone()).is_some());

        // A free slot with a dead waiter in front: whoever looks at the queue next drops it and
        // wakes the one behind, without a slot having to free first.
        let slots = Slots::new(1, 1);
        let held = slots.take("x", "xavier", Arc::new(|| true), Arc::new(|| {})).unwrap();
        wants.store(true, Ordering::SeqCst);
        let w = wants.clone();
        assert!(slots.take("a", "alice", Arc::new(move || w.load(Ordering::SeqCst)), Arc::new(|| {})).is_none());
        assert!(slots.take("b", "bob", Arc::new(|| true), b_wake).is_none());
        // B is woken for this slot only now that A is gone and the slot is free.
        drop(held);
        let before = b_wakes.load(Ordering::SeqCst);
        wants.store(false, Ordering::SeqCst);
        assert!(!slots.admits("c", "carol"), "B is ahead of C");
        assert_eq!(b_wakes.load(Ordering::SeqCst), before + 1, "dropping A woke B");
    }
}

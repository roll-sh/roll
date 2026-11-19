//! The node's sync slots, shared fairly by every cluster on the node.
//!
//! A node runs at most [`NODE_MAX`] syncs at once and at most [`OWNER_MAX`] of them for one
//! owner (the person a cluster belongs to; a cluster without one counts alone). A cluster that
//! can't get a slot waits in a FIFO queue. A slot that frees goes to the first waiter whose owner
//! is under its cap: that waiter's scheduler is woken, and nobody behind it (the cluster that
//! just freed the slot included) may take the slot first. A waiter that no longer wants one
//! ([`Slots::leave`]) or whose cluster is gone is dropped, so it never holds up the queue.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Syncs running at once on a node: each holds a page and, in a full sync, up to
/// `MAX_SEEN` ids' hashes.
pub const NODE_MAX: usize = 4;
/// Syncs running at once for one owner on a node.
pub const OWNER_MAX: usize = 2;

type Check = Arc<dyn Fn() -> bool + Send + Sync>;
type Wake = Arc<dyn Fn() + Send + Sync>;

/// A cluster waiting for a slot.
struct Waiter {
    key: String,
    owner: String,
    /// Whether its cluster is still loaded.
    alive: Check,
    /// Wakes its scheduler.
    wake: Wake,
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

    /// Whether `key` (of `owner`) would get a slot now.
    pub fn admits(&self, key: &str, owner: &str) -> bool {
        let mut st = self.state.lock();
        st.waiting.retain(|w| (w.alive)());
        st.running < self.max && self.under_cap(&st, owner) && !self.ahead(&st, key)
    }

    /// A slot for `key` (of `owner`) when it's its turn; otherwise `key` waits in the queue
    /// (once), and `wake` is called when a slot may be its.
    pub fn take(self: &Arc<Self>, key: &str, owner: &str, alive: Check, wake: Wake) -> Option<Permit> {
        let mut st = self.state.lock();
        st.waiting.retain(|w| (w.alive)());
        if st.running < self.max && self.under_cap(&st, owner) && !self.ahead(&st, key) {
            st.waiting.retain(|w| w.key != key);
            st.running += 1;
            *st.owners.entry(owner.to_string()).or_default() += 1;
            return Some(Permit { slots: self.clone(), owner: owner.to_string() });
        }
        if !st.waiting.iter().any(|w| w.key == key) {
            st.waiting.push_back(Waiter { key: key.to_string(), owner: owner.to_string(), alive, wake });
        }
        None
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
            st.waiting.retain(|w| (w.alive)());
            if st.running >= self.max {
                return;
            }
            st.waiting.iter().find(|w| self.under_cap(&st, &w.owner)).map(|w| w.wake.clone())
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
}

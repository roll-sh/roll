//! A node's slots for one kind of background egress (remote reindex tasks, migration calls,
//! connector syncs), shared fairly by every cluster on the node.
//!
//! A pool runs at most [`Caps::node`] holders at once, at most [`Caps::owner`] of them for one
//! owner (the person a cluster belongs to; a cluster without one counts alone, [`owner_of`]) and
//! at most [`Caps::cluster`] for one cluster. A cluster that can't get a slot waits in a FIFO
//! queue. Free slots go to the first waiters whose owner and cluster are under their caps, one
//! each: those waiters are woken, and nobody behind them (the cluster that just freed a slot
//! included) may take a slot they hold. So one owner's clusters can never hold the slots another
//! owner waits for.
//!
//! Two kinds of waiter:
//! - a connector scheduler ([`Slots::take`]) waits per cluster with a check (its cluster is
//!   loaded and has a runnable pending job) and a wake; it counts while the check holds, and a
//!   woken one that doesn't take its slot within [`WAKE_DEADLINE`] loses its place;
//! - a caller that answers 429 instead of waiting ([`Slots::try_take`], [`Slots::ask`]: a
//!   `_reindex` request, a migration) has nothing to wake. Its owner keeps **one** place in the
//!   queue, whichever of their clusters asked (any of them may use it), while one of them asked
//!   again within [`RETRY_WINDOW`]; once a slot is held for it, it has only a short grace
//!   ([`PASSIVE_GRACE`]) to come back for it. A refused request that's never retried (a closed
//!   tab, or an owner queueing many clusters on purpose) holds a freed slot for that grace at
//!   most, once per owner.
//!
//! A successful take wakes the next waiters too, so slots that free together all find a taker,
//! and so does dropping a waiter that no longer counts. A waiter's check runs under the pool's
//! lock, so it must be cheap (a few map lookups).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a woken waiter has to take its slot before it loses its place.
pub const WAKE_DEADLINE: Duration = Duration::from_secs(30);
/// How long a caller refused with a 429 keeps its place in the queue without asking again.
pub const RETRY_WINDOW: Duration = Duration::from_secs(60);
/// How long a slot is held for a 429'd caller's place once it's that place's turn.
pub const PASSIVE_GRACE: Duration = Duration::from_secs(3);

type Check = Arc<dyn Fn() -> bool + Send + Sync>;
type Wake = Arc<dyn Fn() + Send + Sync>;

/// What a pool lets run at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub node: usize,
    pub owner: usize,
    pub cluster: usize,
}

/// The owner fairness counts a cluster under: its owner, or the cluster alone.
pub fn owner_of(owner: Option<&str>, cluster: &str) -> String {
    match owner {
        Some(o) => o.to_string(),
        None => format!("cluster {cluster}"),
    }
}

/// A cluster (or, for 429'd callers, an owner) waiting for a slot.
struct Waiter {
    /// The cluster that queued it.
    key: String,
    owner: String,
    /// Whether it still wants a slot; `None`: a 429'd caller's place, while one of its owner's
    /// clusters asked within [`RETRY_WINDOW`].
    alive: Option<Check>,
    asked: Instant,
    wake: Option<Wake>,
    /// When a slot was first held for it.
    woken: Option<Instant>,
}

impl Waiter {
    fn passive(&self) -> bool {
        self.alive.is_none()
    }

    fn counts(&self, grace: Duration) -> bool {
        let (alive, deadline) = match &self.alive {
            Some(check) => (check(), WAKE_DEADLINE),
            None => (self.asked.elapsed() < RETRY_WINDOW, grace),
        };
        alive && self.woken.is_none_or(|t| t.elapsed() < deadline)
    }

    /// Whether this is the place of `key` (of `owner`): a 429'd caller's place is its owner's.
    fn is(&self, key: &str, owner: &str, passive: bool) -> bool {
        match (self.passive(), passive) {
            (true, true) => self.owner == owner,
            (false, false) => self.key == key,
            _ => false,
        }
    }
}

#[derive(Default)]
struct State {
    running: usize,
    owners: HashMap<String, usize>,
    clusters: HashMap<String, usize>,
    waiting: VecDeque<Waiter>,
}

/// Whether `w` could take a slot now: its owner under its cap, and its cluster too (a 429'd
/// caller's place may be used by any of its owner's clusters).
fn eligible(owners: &HashMap<String, usize>, clusters: &HashMap<String, usize>, caps: Caps, w: &Waiter) -> bool {
    owners.get(&w.owner).copied().unwrap_or(0) < caps.owner && (w.passive() || clusters.get(&w.key).copied().unwrap_or(0) < caps.cluster)
}

/// Why a slot wasn't given ([`Slots::try_take`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Full {
    /// The cluster holds [`Caps::cluster`].
    Cluster(usize),
    /// The cluster's owner holds [`Caps::owner`] across their clusters.
    Owner(usize),
    /// The node's [`Caps::node`] are all held.
    Node(usize),
    /// A slot is free, but another cluster waited for it first.
    Queue,
}

/// A pool of slots.
pub struct Slots {
    caps: Caps,
    /// [`PASSIVE_GRACE`] (tests shorten it).
    grace: Duration,
    state: parking_lot::Mutex<State>,
}

impl std::fmt::Debug for Slots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slots({} per node, {} per owner, {} per cluster)", self.caps.node, self.caps.owner, self.caps.cluster)
    }
}

/// One holder's slot; dropping it frees the slot for the next waiter.
pub struct Permit {
    slots: Arc<Slots>,
    key: String,
    owner: String,
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Permit")
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut guard = self.slots.state.lock();
            let st = &mut *guard;
            st.running -= 1;
            for (map, k) in [(&mut st.owners, &self.owner), (&mut st.clusters, &self.key)] {
                if let Some(n) = map.get_mut(k) {
                    *n -= 1;
                    if *n == 0 {
                        map.remove(k);
                    }
                }
            }
        }
        self.slots.wake_next();
    }
}

impl Slots {
    pub fn new(caps: Caps) -> Arc<Slots> {
        Self::with_grace(caps, PASSIVE_GRACE)
    }

    /// A pool whose 429'd callers' places hold a slot for `grace`.
    pub fn with_grace(caps: Caps, grace: Duration) -> Arc<Slots> {
        Arc::new(Slots { caps, grace, state: Default::default() })
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// Drops the waiters that no longer count: whether any went.
    fn prune(&self, st: &mut State) -> bool {
        let before = st.waiting.len();
        st.waiting.retain(|w| w.counts(self.grace));
        st.waiting.len() != before
    }

    /// Why `key` (of `owner`; a 429'd caller when `passive`) wouldn't get a slot now, if it
    /// wouldn't. Each free slot is held for one eligible waiter ahead of it (all of them, when
    /// it isn't waiting); a 429'd caller's place starts its grace when it's first held one.
    fn refusal(&self, st: &mut State, key: &str, owner: &str, passive: bool) -> Option<Full> {
        if st.clusters.get(key).copied().unwrap_or(0) >= self.caps.cluster {
            return Some(Full::Cluster(self.caps.cluster));
        }
        if st.owners.get(owner).copied().unwrap_or(0) >= self.caps.owner {
            return Some(Full::Owner(self.caps.owner));
        }
        if st.running >= self.caps.node {
            return Some(Full::Node(self.caps.node));
        }
        let free = self.caps.node - st.running;
        let State { owners, clusters, waiting, .. } = st;
        let mut held = 0;
        for w in waiting.iter_mut().take_while(|w| !w.is(key, owner, passive)) {
            if eligible(owners, clusters, self.caps, w) {
                if w.passive() {
                    w.woken.get_or_insert_with(Instant::now);
                }
                held += 1;
                if held >= free {
                    return Some(Full::Queue);
                }
            }
        }
        None
    }

    /// Whether `key` (of `owner`) would get a slot now.
    pub fn admits(&self, key: &str, owner: &str) -> bool {
        let (admits, pruned) = {
            let mut st = self.state.lock();
            let pruned = self.prune(&mut st);
            (self.refusal(&mut st, key, owner, false).is_none(), pruned)
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
        self.take_or_wait(key, owner, Some(alive), Some(wake)).ok()
    }

    /// A slot for `key` (of `owner`), or why not, for a caller that answers 429 rather than
    /// waiting: refused, it keeps its place in the queue while it asks again within
    /// [`RETRY_WINDOW`].
    pub fn try_take(self: &Arc<Self>, key: &str, owner: &str) -> Result<Permit, Full> {
        self.take_or_wait(key, owner, None, None)
    }

    /// Whether `key` (of `owner`) would get a slot now; if not, it keeps a place in the queue as
    /// [`Slots::try_take`] does (a caller that takes its slot through another path later, as a
    /// migration's `_reindex` does).
    pub fn ask(&self, key: &str, owner: &str) -> bool {
        let (admits, pruned) = {
            let mut st = self.state.lock();
            let pruned = self.prune(&mut st);
            let admits = self.refusal(&mut st, key, owner, true).is_none();
            if !admits {
                Self::queue(&mut st, key, owner, None, None);
            }
            (admits, pruned)
        };
        if pruned {
            self.wake_next();
        }
        admits
    }

    fn queue(st: &mut State, key: &str, owner: &str, alive: Option<Check>, wake: Option<Wake>) {
        let passive = alive.is_none();
        match st.waiting.iter_mut().find(|w| w.is(key, owner, passive)) {
            Some(w) => {
                w.asked = Instant::now();
                if alive.is_some() {
                    w.alive = alive;
                }
                if wake.is_some() {
                    w.wake = wake;
                }
            }
            None => st.waiting.push_back(Waiter { key: key.to_string(), owner: owner.to_string(), alive, asked: Instant::now(), wake, woken: None }),
        }
    }

    fn take_or_wait(self: &Arc<Self>, key: &str, owner: &str, alive: Option<Check>, wake: Option<Wake>) -> Result<Permit, Full> {
        let passive = alive.is_none();
        let mut st = self.state.lock();
        let pruned = self.prune(&mut st);
        let Some(why) = self.refusal(&mut st, key, owner, passive) else {
            st.waiting.retain(|w| !w.is(key, owner, passive));
            st.running += 1;
            *st.owners.entry(owner.to_string()).or_default() += 1;
            *st.clusters.entry(key.to_string()).or_default() += 1;
            drop(st);
            // Another slot may be free too: its waiter is woken now, not at the next release.
            self.wake_next();
            return Ok(Permit { slots: self.clone(), key: key.to_string(), owner: owner.to_string() });
        };
        Self::queue(&mut st, key, owner, alive, wake);
        drop(st);
        if pruned {
            self.wake_next();
        }
        Err(why)
    }

    /// The keys waiting, in order (tests and diagnostics).
    pub fn queued(&self) -> Vec<String> {
        self.state.lock().waiting.iter().map(|w| w.key.clone()).collect()
    }

    /// The slots `key` holds now.
    pub fn held(&self, key: &str) -> usize {
        self.state.lock().clusters.get(key).copied().unwrap_or(0)
    }

    /// `key` wants no slot any more (nothing pending): it leaves the queue, and the next waiter
    /// is woken if a slot is free.
    pub fn leave(&self, key: &str) {
        let was = {
            let mut st = self.state.lock();
            let before = st.waiting.len();
            st.waiting.retain(|w| w.passive() || w.key != key);
            st.waiting.len() != before
        };
        if was {
            self.wake_next();
        }
    }

    /// Holds each free slot for the first eligible waiters, one each, and wakes those that are
    /// newly holding one.
    fn wake_next(&self) {
        let wakes: Vec<Wake> = {
            let mut guard = self.state.lock();
            self.prune(&mut guard);
            let st = &mut *guard;
            let free = self.caps.node.saturating_sub(st.running);
            let State { owners, clusters, waiting, .. } = st;
            let mut wakes = Vec::new();
            for w in waiting.iter_mut().filter(|w| eligible(owners, clusters, self.caps, w)).take(free) {
                if w.woken.is_none() {
                    w.woken = Some(Instant::now());
                    wakes.extend(w.wake.clone());
                }
            }
            wakes
        };
        for w in wakes {
            w();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn caps(node: usize, owner: usize) -> Caps {
        Caps { node, owner, cluster: owner }
    }

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
        let slots = Slots::new(caps(4, 2));
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

    /// Callers that answer 429 (remote reindex, migration): one owner with two clusters holds at
    /// most its owner's share of the node, a second owner's refused request keeps its place, and
    /// neither the first owner's clusters nor the cluster that freed a slot can take the slot it
    /// waits for.
    #[test]
    fn one_owners_clusters_cant_take_the_slot_another_owner_waits_for() {
        let slots = Slots::new(Caps { node: 4, owner: 2, cluster: 2 });
        let _a1 = slots.try_take("shop-1", "alice").unwrap();
        let _a2 = slots.try_take("shop-1", "alice").unwrap();
        // Alice's second cluster: under its own cap, but Alice holds her share of the node.
        assert_eq!(slots.try_take("shop-2", "alice").unwrap_err(), Full::Owner(2));
        let b1 = slots.try_take("blog", "bob").unwrap();
        let _b2 = slots.try_take("blog", "bob").unwrap();
        // The node is full: Carol is refused, and keeps her place.
        assert_eq!(slots.try_take("wiki", "carol").unwrap_err(), Full::Node(4));
        // Bob's task ends: the slot is Carol's.
        drop(b1);
        assert_eq!(slots.try_take("shop-2", "alice").unwrap_err(), Full::Owner(2));
        assert_eq!(slots.try_take("shop-1", "alice").unwrap_err(), Full::Cluster(2));
        assert_eq!(slots.try_take("blog", "bob").unwrap_err(), Full::Queue, "the freed slot isn't the freeing cluster's");
        assert!(!slots.ask("blog", "bob"), "Carol is ahead");
        let _c = slots.try_take("wiki", "carol").expect("the slot Carol waited for");
        assert_eq!((slots.held("wiki"), slots.held("shop-1"), slots.held("shop-2")), (1, 2, 0));
    }

    /// 429'd callers that never come back: one owner's clusters share one place in the queue,
    /// and a slot held for it goes to the next caller once its short grace is over.
    #[test]
    fn a_refused_request_that_never_retries_holds_a_freed_slot_only_briefly() {
        let grace = Duration::from_millis(50);
        let slots = Slots::with_grace(Caps { node: 1, owner: 1, cluster: 1 }, grace);
        let bob = slots.try_take("blog", "bob").unwrap();
        // Mallory's two clusters are refused and never ask again; Dave asks after them.
        assert_eq!(slots.try_take("m1", "mallory").unwrap_err(), Full::Node(1));
        assert_eq!(slots.try_take("m2", "mallory").unwrap_err(), Full::Node(1));
        assert_eq!(slots.try_take("shop", "dave").unwrap_err(), Full::Node(1));
        assert_eq!(slots.queued(), ["m1", "shop"], "one place per owner");
        // Bob's task ends: the slot is held for Mallory's place, briefly.
        drop(bob);
        assert_eq!(slots.try_take("shop", "dave").unwrap_err(), Full::Queue);
        std::thread::sleep(grace + Duration::from_millis(20));
        let _dave = slots.try_take("shop", "dave").expect("Mallory's grace is over");
        assert!(slots.queued().is_empty());
    }

    /// A waiter whose job stopped being runnable (canceled, its connector gone) without the
    /// cluster ever looking again doesn't block the queue: the next waiter gets the slot.
    #[test]
    fn a_waiter_with_nothing_pending_doesnt_hold_the_queue() {
        use std::sync::atomic::AtomicBool;
        let slots = Slots::new(caps(1, 1));
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
        let slots = Slots::new(caps(1, 1));
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

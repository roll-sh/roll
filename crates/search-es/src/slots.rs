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
//! - a caller that answers 429 instead of waiting ([`Slots::try_take`], [`Slots::reserve`]: a
//!   `_reindex` request, a migration) has nothing to wake. Refused for the node or its owner's
//!   cap (never for its own cluster's: no slot it could use would come of it), its owner keeps
//!   **one** place in the queue, whichever of their clusters asked (any of them may use it), while
//!   one of them asks again within [`RETRY_WINDOW`]. Once a slot is held for that place, it has a
//!   short grace ([`PASSIVE_GRACE`]) to come back for it; past it the place is skipped (it holds
//!   no slot, and the next waiter's turn comes) but keeps its position, and asking again makes it
//!   eligible again. A refused request that's never retried (a closed tab, or an owner queueing
//!   many clusters on purpose) holds a freed slot for that grace at most, once per owner ahead in
//!   line.
//!
//! A caller that must do slow work between deciding and starting (a migration reads the remote's
//! definitions and creates the index before its `_reindex`) [`Slots::reserve`]s: the slot is
//! held from then on, and the same cluster's next [`Slots::try_take`] uses it. A reservation is
//! the cluster's, not the call's: another `try_take` of the same cluster meanwhile (the tenant's
//! own remote `_reindex`) uses it instead, and the reserving caller then takes or waits as anyone
//! does. That costs only the cluster itself a slot it already holds, never another owner.
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
/// Where a pool reads the time ([`Instant::now`]; tests turn it by hand).
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

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
    /// A 429'd caller's place whose grace ran out: it holds no slot until it asks again.
    skipped: bool,
}

impl Waiter {
    fn passive(&self) -> bool {
        self.alive.is_none()
    }

    fn counts(&self, now: Instant) -> bool {
        match &self.alive {
            Some(check) => check() && self.woken.is_none_or(|t| now.saturating_duration_since(t) < WAKE_DEADLINE),
            None => now.saturating_duration_since(self.asked) < RETRY_WINDOW,
        }
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
    /// Slots reserved by cluster ([`Slots::reserve`]), counted in the holds above under the
    /// owner that reserved each (one entry per reservation), that its next [`Slots::try_take`]
    /// uses.
    reserved: HashMap<String, Vec<String>>,
}

/// Whether `w` could take a slot now: its owner under its cap, and its cluster too (a 429'd
/// caller's place may be used by any of its owner's clusters).
fn eligible(owners: &HashMap<String, usize>, clusters: &HashMap<String, usize>, caps: Caps, w: &Waiter) -> bool {
    !w.skipped && owners.get(&w.owner).copied().unwrap_or(0) < caps.owner && (w.passive() || clusters.get(&w.key).copied().unwrap_or(0) < caps.cluster)
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
    /// [`PASSIVE_GRACE`].
    grace: Duration,
    clock: Clock,
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
    /// Whether dropping it frees the slot (a consumed reservation's doesn't: the permit that
    /// consumed it does).
    armed: bool,
}

/// A slot held for `key`'s next [`Slots::try_take`] ([`Slots::reserve`]); dropped unused, it
/// frees the slot.
pub struct Reservation(Permit);

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reservation")
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut st = self.0.slots.state.lock();
        // Unused while its owner's entry is still there: dropping it frees the slot. Used, the
        // permit that took it (under this owner) frees it.
        let unused = st.reserved.get_mut(&self.0.key).and_then(|owners| owners.iter().position(|o| *o == self.0.owner).map(|i| owners.swap_remove(i))).is_some();
        if st.reserved.get(&self.0.key).is_some_and(Vec::is_empty) {
            st.reserved.remove(&self.0.key);
        }
        if !unused {
            self.0.armed = false;
        }
    }
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Permit")
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
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
        Self::with_clock(caps, Arc::new(Instant::now))
    }

    /// A pool that reads the time from `clock`.
    pub fn with_clock(caps: Caps, clock: Clock) -> Arc<Slots> {
        Arc::new(Slots { caps, grace: PASSIVE_GRACE, clock, state: Default::default() })
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// Drops the waiters that no longer count, and skips the 429'd callers' places whose grace
    /// ran out: whether any changed.
    fn prune(&self, st: &mut State) -> bool {
        let now = self.now();
        let before = st.waiting.len();
        st.waiting.retain(|w| w.counts(now));
        let mut changed = st.waiting.len() != before;
        for w in st.waiting.iter_mut().filter(|w| w.passive() && w.woken.is_some_and(|t| now.saturating_duration_since(t) >= self.grace)) {
            w.woken = None;
            w.skipped = true;
            changed = true;
        }
        changed
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
        let now = self.now();
        let State { owners, clusters, waiting, .. } = st;
        let mut held = 0;
        for w in waiting.iter_mut().take_while(|w| !w.is(key, owner, passive)) {
            if eligible(owners, clusters, self.caps, w) {
                if w.passive() {
                    w.woken.get_or_insert(now);
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

    /// A slot for `key` (of `owner`) held from now until `key`'s next [`Slots::try_take`], which
    /// uses it; refused as [`Slots::try_take`] is.
    pub fn reserve(self: &Arc<Self>, key: &str, owner: &str) -> Result<Reservation, Full> {
        let permit = self.take_or_wait(key, owner, None, None)?;
        self.state.lock().reserved.entry(key.to_string()).or_default().push(owner.to_string());
        Ok(Reservation(permit))
    }

    fn queue(st: &mut State, now: Instant, key: &str, owner: &str, alive: Option<Check>, wake: Option<Wake>) {
        let passive = alive.is_none();
        match st.waiting.iter_mut().find(|w| w.is(key, owner, passive)) {
            Some(w) => {
                w.asked = now;
                w.skipped = false;
                if alive.is_some() {
                    w.alive = alive;
                }
                if wake.is_some() {
                    w.wake = wake;
                }
            }
            None => st.waiting.push_back(Waiter { key: key.to_string(), owner: owner.to_string(), alive, asked: now, wake, woken: None, skipped: false }),
        }
    }

    fn take_or_wait(self: &Arc<Self>, key: &str, owner: &str, alive: Option<Check>, wake: Option<Wake>) -> Result<Permit, Full> {
        let passive = alive.is_none();
        let mut st = self.state.lock();
        // A slot this cluster reserved is its, held under the owner that reserved it (the counts
        // went up under that owner, whatever owner this call names).
        if passive && let Some(reserver) = st.reserved.get_mut(key).and_then(Vec::pop) {
            if st.reserved.get(key).is_some_and(Vec::is_empty) {
                st.reserved.remove(key);
            }
            return Ok(Permit { slots: self.clone(), key: key.to_string(), owner: reserver, armed: true });
        }
        let pruned = self.prune(&mut st);
        let Some(why) = self.refusal(&mut st, key, owner, passive) else {
            st.waiting.retain(|w| !w.is(key, owner, passive));
            st.running += 1;
            *st.owners.entry(owner.to_string()).or_default() += 1;
            *st.clusters.entry(key.to_string()).or_default() += 1;
            drop(st);
            // Another slot may be free too: its waiter is woken now, not at the next release.
            self.wake_next();
            return Ok(Permit { slots: self.clone(), key: key.to_string(), owner: owner.to_string(), armed: true });
        };
        // A 429'd caller refused for its own cluster's cap waits for none of the node's slots.
        if !(passive && matches!(why, Full::Cluster(_))) {
            let now = self.now();
            Self::queue(&mut st, now, key, owner, alive, wake);
        }
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

    /// The slots `owner`'s clusters hold now.
    pub fn held_by(&self, owner: &str) -> usize {
        self.state.lock().owners.get(owner).copied().unwrap_or(0)
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
    /// newly holding one. A 429'd caller's place that held a slot someone else took since (a
    /// place ahead of it came back) holds none now, so its grace starts again when it next does.
    fn wake_next(&self) {
        let wakes: Vec<Wake> = {
            let mut guard = self.state.lock();
            self.prune(&mut guard);
            let now = self.now();
            let st = &mut *guard;
            let free = self.caps.node.saturating_sub(st.running);
            let State { owners, clusters, waiting, .. } = st;
            let mut wakes = Vec::new();
            let mut held = 0;
            for w in waiting.iter_mut().filter(|w| eligible(owners, clusters, self.caps, w)) {
                if held < free {
                    held += 1;
                    if w.woken.is_none() {
                        w.woken = Some(now);
                        wakes.extend(w.wake.clone());
                    }
                } else if w.passive() {
                    w.woken = None;
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

    /// A clock turned by hand: the time, and how to move it on.
    fn hand_clock() -> (Clock, impl Fn(Duration)) {
        let (start, past) = (Instant::now(), Arc::new(parking_lot::Mutex::new(Duration::ZERO)));
        let read = past.clone();
        (Arc::new(move || start + *read.lock()), move |d: Duration| *past.lock() += d)
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
        let _c = slots.try_take("wiki", "carol").expect("the slot Carol waited for");
        assert_eq!((slots.held("wiki"), slots.held("shop-1"), slots.held("shop-2")), (1, 2, 0));
    }

    /// 429'd callers that never come back: one owner's clusters share one place in the queue,
    /// and a slot held for it goes to the next caller once its short grace is over.
    #[test]
    fn a_refused_request_that_never_retries_holds_a_freed_slot_only_briefly() {
        let (clock, pass) = hand_clock();
        let slots = Slots::with_clock(Caps { node: 1, owner: 1, cluster: 1 }, clock);
        let bob = slots.try_take("blog", "bob").unwrap();
        // Bob's own cluster asking again waits for no slot of the node's (it holds its cap).
        assert_eq!(slots.try_take("blog", "bob").unwrap_err(), Full::Cluster(1));
        // Mallory's two clusters are refused and never ask again; Dave asks after them.
        assert_eq!(slots.try_take("m1", "mallory").unwrap_err(), Full::Node(1));
        assert_eq!(slots.try_take("m2", "mallory").unwrap_err(), Full::Node(1));
        assert_eq!(slots.try_take("shop", "dave").unwrap_err(), Full::Node(1));
        assert_eq!(slots.queued(), ["m1", "shop"], "one place per owner, none for a cluster at its cap");
        // Bob's task ends: the slot is held for Mallory's place, briefly.
        drop(bob);
        pass(PASSIVE_GRACE - Duration::from_millis(1));
        assert_eq!(slots.try_take("shop", "dave").unwrap_err(), Full::Queue);
        pass(Duration::from_millis(1));
        let _dave = slots.try_take("shop", "dave").expect("Mallory's grace is over");
        assert_eq!(slots.queued(), ["m1"], "Mallory keeps her position, skipped until she asks again");
    }

    /// A caller that comes back after its grace, within the retry window (the dashboard asking
    /// again for a waiting copy), still has its turn ahead of those who asked after it; and a
    /// migration's reservation holds its slot until its `_reindex` takes it.
    #[test]
    fn a_retrier_past_its_grace_keeps_its_turn_and_a_reservation_holds_the_slot() {
        let (clock, pass) = hand_clock();
        let slots = Slots::with_clock(Caps { node: 1, owner: 2, cluster: 2 }, clock);
        let bob = slots.try_take("blog", "bob").unwrap();
        assert_eq!(slots.try_take("shop", "alice").unwrap_err(), Full::Node(1));
        assert_eq!(slots.try_take("wiki", "dave").unwrap_err(), Full::Node(1));
        drop(bob);
        pass(PASSIVE_GRACE + Duration::from_secs(1));
        // Alice asked first: back after her grace, the slot is still hers before Dave's.
        let reserved = slots.reserve("shop", "alice").expect("Alice's turn");
        assert_eq!(slots.try_take("wiki", "dave").unwrap_err(), Full::Node(1));
        // Her copy starts: its `_reindex` uses the reservation, and the slot is held once.
        let task = slots.try_take("shop", "alice").expect("the reserved slot");
        drop(reserved);
        assert_eq!(slots.held("shop"), 1);
        drop(task);
        assert_eq!(slots.held("shop"), 0);
        // A reservation dropped unused frees its slot.
        drop(slots.reserve("wiki", "dave").expect("Dave's turn"));
        assert_eq!(slots.held("wiki"), 0);
    }

    /// A reservation taken by a call naming another owner (the migration names the cluster's
    /// catalog owner, the `_reindex` its loaded config's): the slot stays counted under the owner
    /// that reserved it, and both owners' counts return to zero.
    #[test]
    fn a_reservation_used_under_another_owner_is_released_under_its_own() {
        let slots = Slots::new(Caps { node: 4, owner: 2, cluster: 2 });
        let reserved = slots.reserve("shop", "alice").unwrap();
        let task = slots.try_take("shop", "cluster shop").expect("the reserved slot");
        assert_eq!((slots.held_by("alice"), slots.held_by("cluster shop"), slots.held("shop")), (1, 0, 1));
        drop(reserved);
        drop(task);
        assert_eq!((slots.held_by("alice"), slots.held_by("cluster shop"), slots.held("shop")), (0, 0, 0));
        // Unused, it's released under its owner too.
        drop(slots.reserve("shop", "alice").unwrap());
        assert_eq!((slots.held_by("alice"), slots.held("shop")), (0, 0));
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

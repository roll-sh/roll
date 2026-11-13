//! The lease state machine: membership, leases with self-fencing, placement, routing, guards,
//! handoff, rebalancing, draining and the background threads that drive them.

use crate::backend::{Backend, Holder, Registration, Timing};
use crate::{Error, Result, clock, placement, sign};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

use crate::ClusterConfig;

/// A member of the cluster: its id and the base URL it's reached at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Node {
    pub id: String,
    pub url: String,
}

/// A change of ownership on this node. For each key, a subscriber sees `Acquired` and `Lost`
/// strictly alternating, starting with `Acquired`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// This node now holds `key`. `epoch` goes up every time the holder changes.
    Acquired { key: String, epoch: i64 },
    /// This node no longer holds `key` (handed off, fenced, or taken by another node).
    Lost { key: String },
}

/// Where a request for a key runs.
pub enum Route {
    /// Here: this node holds a valid lease. The guard counts the request as in flight.
    Local(Guard),
    /// On another node, which holds a valid lease (or should take the key): forward it there.
    Forward(Node),
    /// Nowhere right now (the key is being handed off, this node's lease is past its local
    /// deadline, the holder is dead but its lease hasn't expired yet, or no node is live): ask
    /// again after `retry_after`.
    Unavailable { retry_after: Duration },
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::Local(g) => write!(f, "Local({} @{})", g.key, g.epoch),
            Route::Forward(n) => write!(f, "Forward({})", n.id),
            Route::Unavailable { retry_after } => write!(f, "Unavailable({retry_after:?})"),
        }
    }
}

/// One tenure of a key on this node: from acquiring it to losing it.
struct Tenure {
    inflight: AtomicUsize,
    /// When a guard was last granted or dropped (ms on [`clock::now`]): how long the key has
    /// been idle.
    last_active: AtomicU64,
    /// Becomes true when the key starts being handed off or is lost.
    ending: watch::Sender<bool>,
}

struct Held {
    epoch: i64,
    /// Until when (on [`clock::now`]) the lease may be acted on.
    deadline: Duration,
    /// When a handoff started.
    handoff: Option<Duration>,
    tenure: Arc<Tenure>,
    /// Placement cache: (nodes generation, whether this node is still preferred).
    pref: (u64, bool),
    /// When its handoff completes, delete the lease (an idle key, a deleted repo) instead of
    /// expiring it for the preferred node to take.
    retire: bool,
}

#[derive(Clone)]
struct NodeRow {
    node: Node,
    zone: Option<String>,
    draining: bool,
}

fn zone_in(nodes: &[NodeRow], id: &str) -> Option<String> {
    nodes.iter().find(|n| n.node.id == id).and_then(|n| n.zone.clone())
}

struct State {
    held: HashMap<String, Held>,
    /// Keys this node wants gone: if the backend still says they're ours (a release that
    /// failed, a handoff that was fenced), they're released again instead of adopted.
    release: HashSet<String>,
    /// Live nodes, as of the last refresh.
    nodes: Vec<NodeRow>,
    /// Bumped whenever the set of eligible nodes changes (placement cache).
    nodes_gen: u64,
    eligible: Vec<String>,
    remote: HashMap<String, (Node, Duration)>,
    subs: Vec<mpsc::UnboundedSender<Ownership>>,
    /// Singletons this node may not try to take again before the given time.
    singleton_backoff: HashMap<String, Duration>,
    /// Keys being handed off.
    handoffs: HashSet<String>,
    /// No held lease's deadline is before this (a lower bound; `None` when nothing's held).
    earliest: Option<Duration>,
}

#[derive(Default)]
struct Flags {
    stop: AtomicBool,
    draining: AtomicBool,
    left: AtomicBool,
    superseded: AtomicBool,
}

/// What happened to this node's leases since it joined: counters only go up. The node's
/// process reports them (rolld publishes them as metrics); this crate does no I/O for them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Keys this node handed to another node (drain, rebalancing, an idle or retired key).
    pub handoffs: u64,
    /// Handoffs that didn't go cleanly: they timed out with requests still in flight, their
    /// release didn't reach the backend (the lease lapses instead, so the key is unavailable
    /// for up to a lease length), or a drain ended with the key still held.
    pub handoff_failures: u64,
    /// Leases this node took from a holder that never released them (it died, was cut off,
    /// or left with requests in flight). Planned handoffs don't count.
    pub takeovers: u64,
    /// Leases this node stopped using because it couldn't renew them in time (self-fencing),
    /// or because a renewal found them gone.
    pub fenced: u64,
}

#[derive(Default)]
struct Counters {
    handoffs: AtomicU64,
    handoff_failures: AtomicU64,
    takeovers: AtomicU64,
    fenced: AtomicU64,
}

fn bump(c: &AtomicU64, n: usize) {
    c.fetch_add(n as u64, Ordering::Relaxed);
}

#[derive(Default)]
pub(crate) struct Faults {
    pub freeze: AtomicBool,
    pub fail_db: AtomicBool,
}

#[derive(Default)]
struct Health {
    heartbeat_ok: Option<Duration>,
    renew_ok_sent: Option<Duration>,
}

pub(crate) struct Inner {
    pub(crate) cfg: ClusterConfig,
    backend: Box<dyn Backend>,
    validity: Duration,
    state: Mutex<State>,
    /// Serializes this node's lease writes (acquire, renew, release) with the local state
    /// changes that follow them, so the two never disagree in a way another write could see.
    /// Lock order: `ops` before `state`.
    ops: Mutex<()>,
    flags: Flags,
    pub(crate) faults: Faults,
    counters: Counters,
    health: Mutex<Health>,
    tick: OnceLock<Thread>,
    watchdog: OnceLock<Thread>,
    pub(crate) http: reqwest::Client,
}

/// A request (or unit of work) in flight on a key this node owns. While any guard of a key is
/// alive, a handoff of the key waits (up to `handoff_timeout`).
///
/// Keep it alive for as long as the work runs: a handler that returns a streamed body (an event
/// stream, a git pack) should move the guard into the stream, or the handoff won't wait for it.
/// Long-lived holders (event streams) should end when [`Guard::ending`] resolves: the key is
/// being handed off or was lost.
pub struct Guard {
    inner: Arc<Inner>,
    key: String,
    epoch: i64,
    tenure: Arc<Tenure>,
}

impl Guard {
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The lease epoch this guard was granted under.
    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    /// Whether the key is still held under this guard's epoch, within its local deadline, and
    /// not being handed off. Long operations can check this before committing to more work;
    /// correctness must still come from compare-and-swap writes.
    pub fn is_valid(&self) -> bool {
        if *self.tenure.ending.borrow() {
            return false;
        }
        let st = self.inner.state.lock();
        st.held.get(&self.key).is_some_and(|h| Arc::ptr_eq(&h.tenure, &self.tenure) && h.deadline > clock::now() && h.handoff.is_none())
    }

    /// Whether the key started being handed off or was lost.
    pub fn is_ending(&self) -> bool {
        *self.tenure.ending.borrow()
    }

    /// Resolves when the key starts being handed off or is lost. Not tied to the guard's
    /// lifetime, so it can be polled next to the work it should stop.
    pub fn ending(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.tenure.ending.subscribe();
        async move {
            let _ = rx.wait_for(|v| *v).await;
        }
    }
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard").field("key", &self.key).field("epoch", &self.epoch).finish()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.tenure.last_active.store(now_ms(), Ordering::Relaxed);
        let before = self.tenure.inflight.fetch_sub(1, Ordering::AcqRel);
        if before == 1 && *self.tenure.ending.borrow() {
            // A handoff may be waiting for this one.
            self.inner.wake_tick();
        }
    }
}

/// How many pages of expired leases one sweep reads at most.
const SWEEP_PAGES: usize = 16;
/// Keys acquired per transaction by the sweeper.
const ACQUIRE_BATCH: usize = 64;
/// How long `route` waits for another lease write of this node before answering Unavailable.
const ROUTE_OPS_WAIT: Duration = Duration::from_secs(2);

enum Local {
    Guard(Guard),
    Busy(Duration),
    None,
}

impl Inner {
    fn me(&self) -> &str {
        &self.cfg.node_id
    }

    /// The backend, unless a test made it unreachable.
    fn backend(&self) -> Result<&dyn Backend> {
        if self.faults.fail_db.load(Ordering::SeqCst) {
            return Err(Error::Fault("the cluster backend is unreachable from this node (test fault)"));
        }
        Ok(&*self.backend)
    }

    fn no_acquire(&self) -> bool {
        self.flags.draining.load(Ordering::SeqCst) || self.flags.left.load(Ordering::SeqCst) || self.flags.superseded.load(Ordering::SeqCst)
    }

    pub(crate) fn wake_tick(&self) {
        if let Some(t) = self.tick.get() {
            t.unpark();
        }
    }

    fn emit(st: &mut State, ev: Ownership) {
        st.subs.retain(|s| s.send(ev.clone()).is_ok());
    }

    /// Stop holding `key` locally: end its guards' tenure and tell subscribers.
    fn drop_held(&self, st: &mut State, key: &str, why: &str) -> Option<Held> {
        let h = st.held.remove(key)?;
        h.tenure.ending.send_replace(true);
        st.remote.remove(key);
        st.handoffs.remove(key);
        tracing::info!(node = %self.me(), key, epoch = h.epoch, why, "lease lost");
        Self::emit(st, Ownership::Lost { key: key.to_string() });
        Some(h)
    }

    fn adopt(&self, st: &mut State, key: &str, epoch: i64, deadline: Duration, why: &str) {
        let (ending, _) = watch::channel(false);
        let tenure = Arc::new(Tenure { inflight: AtomicUsize::new(0), last_active: AtomicU64::new(now_ms()), ending });
        // Placement is computed on first use (no generation matches `u64::MAX`).
        st.held.insert(key.to_string(), Held { epoch, deadline, handoff: None, tenure, pref: (u64::MAX, true), retire: false });
        st.earliest = Some(st.earliest.map_or(deadline, |e| e.min(deadline)));
        st.release.remove(key);
        st.remote.remove(key);
        st.singleton_backoff.remove(key);
        tracing::info!(node = %self.me(), key, epoch, why, "lease acquired");
        Self::emit(st, Ownership::Acquired { key: key.to_string(), epoch });
    }

    /// Fence every lease past its local deadline. Returns the earliest deadline still ahead.
    fn fence_expired(&self) -> Option<Duration> {
        let now = clock::now();
        let mut st = self.state.lock();
        // `earliest` is a lower bound (renewals only push deadlines out), so nothing can have
        // expired before it.
        if let Some(e) = st.earliest
            && now < e
        {
            return Some(e);
        }
        let expired: Vec<String> = st.held.iter().filter(|(_, h)| h.deadline <= now).map(|(k, _)| k.clone()).collect();
        bump(&self.counters.fenced, expired.len());
        for k in expired {
            self.drop_held(&mut st, &k, "its lease passed its local deadline without a renewal");
        }
        st.earliest = st.held.values().map(|h| h.deadline).min();
        st.earliest
    }

    // ---- membership ----

    fn heartbeat(&self) -> Result<()> {
        let sent = clock::now();
        match self.backend()?.heartbeat(self.flags.draining.load(Ordering::SeqCst)) {
            Err(Error::Superseded) => {
                self.supersede();
                return Err(Error::Superseded);
            }
            r => r?,
        }
        self.health.lock().heartbeat_ok = Some(sent);
        Ok(())
    }

    /// Another process now runs as this node id: stop being a member at once.
    fn supersede(&self) {
        if self.flags.superseded.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::error!(node = %self.me(), "another process joined with this node id; this one stops owning anything");
        let mut st = self.state.lock();
        let keys: Vec<String> = st.held.keys().cloned().collect();
        for k in keys {
            self.drop_held(&mut st, &k, "this node id was taken over by another process");
        }
    }

    pub(crate) fn refresh_nodes(&self) -> Result<()> {
        let rows: Vec<NodeRow> =
            self.backend()?.members()?.into_iter().map(|m| NodeRow { node: m.node, zone: m.zone, draining: m.draining }).collect();
        let mut st = self.state.lock();
        st.nodes = rows;
        self.recompute_eligible(&mut st);
        Ok(())
    }

    fn recompute_eligible(&self, st: &mut State) {
        let me_out = self.no_acquire();
        let eligible: Vec<String> =
            st.nodes.iter().filter(|n| !n.draining && !(me_out && n.node.id == self.me())).map(|n| n.node.id.clone()).collect();
        if eligible != st.eligible {
            st.eligible = eligible;
            st.nodes_gen += 1;
        }
    }

    fn preferred_in(&self, st: &State, key: &str) -> Option<Node> {
        let id = placement::place(key, &st.eligible, |id| zone_in(&st.nodes, id))?;
        st.nodes.iter().find(|n| n.node.id == id).map(|n| n.node.clone())
    }

    // ---- leases ----

    /// Renew every lease of this node (one write), then reconcile local state with what the
    /// backend says we hold.
    fn renew(&self) -> Result<()> {
        let _ops = self.ops.lock();
        let confirmed = match self.backend()?.renew() {
            Err(Error::Superseded) => {
                self.supersede();
                return Err(Error::Superseded);
            }
            r => r?,
        };
        let (rows, sent) = (confirmed.leases, confirmed.basis);
        let deadline = sent + self.validity;
        self.health.lock().renew_ok_sent = Some(sent);
        self.fence_expired();
        let now = clock::now();
        tracing::debug!(node = %self.me(), took = ?(now - sent), leases = rows.len(), "renewed");
        if now - sent > self.cfg.renew_every {
            tracing::warn!(node = %self.me(), took = ?(now - sent), "renewing leases was slow");
        }
        let returned: HashMap<String, i64> = rows.into_iter().collect();
        let mut to_release = Vec::new();
        {
            let mut st = self.state.lock();
            // Everything held was acquired before this renewal was sent (both happen under
            // `ops`), so a held key the renewal didn't extend is no longer ours.
            let gone: Vec<String> = st.held.iter().filter(|(k, h)| returned.get(*k) != Some(&h.epoch)).map(|(k, _)| k.clone()).collect();
            bump(&self.counters.fenced, gone.len());
            for k in gone {
                self.drop_held(&mut st, &k, "the renewal found it expired or held by another node");
            }
            for (k, &epoch) in &returned {
                if let Some(h) = st.held.get_mut(k) {
                    h.deadline = h.deadline.max(deadline);
                    continue;
                }
                // Ours in the backend but not held here: an acquisition whose answer was lost,
                // a lease fenced while the backend was slow, or one we meant to give up.
                let leaving = self.no_acquire() || st.release.contains(k);
                if !leaving && deadline > now {
                    self.adopt(&mut st, k, epoch, deadline, "the renewal found it still ours");
                } else {
                    to_release.push((k.clone(), epoch));
                }
            }
            st.release.retain(|k| returned.contains_key(k));
        }
        if !to_release.is_empty() {
            self.release_rows(&to_release);
        }
        Ok(())
    }

    /// Expire leases we held (after they've been dropped locally) so their preferred nodes take
    /// them. Must be called with `ops` held.
    fn release_rows(&self, rows: &[(String, i64)]) {
        let res = self.backend().and_then(|b| b.release(rows));
        let mut st = self.state.lock();
        match res {
            Ok(_) => {
                for (k, _) in rows {
                    st.release.remove(k);
                }
            }
            Err(e) => {
                tracing::warn!(node = %self.me(), error = %e, keys = rows.len(), "releasing leases failed; they'll be released again or expire");
                bump(&self.counters.handoff_failures, rows.len());
                for (k, _) in rows {
                    st.release.insert(k.clone());
                }
            }
        }
    }

    /// Take `keys` (one transaction) and adopt the ones won. `wait`: how long to wait for
    /// another lease write of this node; `None` (blocked) answers `Ok(None)`.
    fn acquire_keys(&self, keys: &[String], wait: Option<Duration>) -> Result<Option<HashMap<String, i64>>> {
        if self.no_acquire() {
            return Ok(Some(HashMap::new()));
        }
        let _ops = match wait {
            None => self.ops.lock(),
            Some(d) => match self.ops.try_lock_for(d) {
                Some(g) => g,
                None => return Ok(None),
            },
        };
        let todo: Vec<&String> = {
            let st = self.state.lock();
            keys.iter().filter(|k| !st.held.contains_key(*k)).collect()
        };
        if todo.is_empty() {
            return Ok(Some(HashMap::new()));
        }
        let todo: Vec<String> = todo.into_iter().cloned().collect();
        let confirmed = self.backend()?.acquire(&todo)?;
        bump(&self.counters.takeovers, confirmed.taken_over);
        let (won, deadline) = (confirmed.leases, confirmed.basis + self.validity);
        let mut st = self.state.lock();
        // Too slow to act on, or we started leaving meanwhile: leave them to the renewal, which
        // adopts or releases what's still ours.
        if clock::now() >= deadline || self.no_acquire() {
            return Ok(Some(HashMap::new()));
        }
        let mut out = HashMap::new();
        for (k, e) in won {
            self.adopt(&mut st, &k, e, deadline, "acquired");
            out.insert(k, e);
        }
        Ok(Some(out))
    }

    // ---- routing ----

    fn local(self: &Arc<Self>, key: &str) -> Local {
        let st = self.state.lock();
        let Some(h) = st.held.get(key) else { return Local::None };
        if h.deadline <= clock::now() {
            return Local::Busy(self.cfg.renew_every);
        }
        if h.handoff.is_some() {
            return Local::Busy(self.cfg.sweep_every);
        }
        h.tenure.inflight.fetch_add(1, Ordering::AcqRel);
        h.tenure.last_active.store(now_ms(), Ordering::Relaxed);
        Local::Guard(Guard { inner: self.clone(), key: key.to_string(), epoch: h.epoch, tenure: h.tenure.clone() })
    }

    /// The answer of [`Inner::route`] if it needs no backend call.
    pub(crate) fn route_fast(self: &Arc<Self>, key: &str) -> Option<Route> {
        match self.local(key) {
            Local::Guard(g) => return Some(Route::Local(g)),
            Local::Busy(d) => return Some(Route::Unavailable { retry_after: d }),
            Local::None => {}
        }
        let st = self.state.lock();
        st.remote.get(key).filter(|(_, until)| *until > clock::now()).map(|(n, _)| Route::Forward(n.clone()))
    }

    pub(crate) fn route(self: &Arc<Self>, key: &str) -> Result<Route> {
        for _ in 0..3 {
            if let Some(r) = self.route_fast(key) {
                return Ok(r);
            }
            match self.backend()?.lookup(key)? {
                // Ours in the backend, not usable here: being released, fenced, or about to be
                // adopted by the next renewal.
                Holder::Me { left } => return Ok(Route::Unavailable { retry_after: left.min(self.cfg.renew_every) }),
                Holder::Other { id, url: Some(url), live: true, .. } => {
                    let n = Node { id, url };
                    self.cache_remote(key, &n);
                    return Ok(Route::Forward(n));
                }
                // The holder looks dead; its lease runs out first.
                Holder::Other { left, .. } => return Ok(Route::Unavailable { retry_after: left.min(self.cfg.lease_ttl) }),
                Holder::Free => {}
            }
            // Nobody holds it: its preferred node takes it.
            let pref = self.preferred_in(&self.state.lock(), key);
            match pref {
                None => return Ok(Route::Unavailable { retry_after: self.cfg.heartbeat }),
                Some(n) if n.id == self.me() => match self.acquire_keys(&[key.to_string()], Some(ROUTE_OPS_WAIT))? {
                    None => return Ok(Route::Unavailable { retry_after: self.cfg.renew_every }),
                    // Won (the next turn hands out a guard) or lost the CAS (the next turn reads
                    // who won).
                    Some(_) => continue,
                },
                Some(n) => {
                    self.cache_remote(key, &n);
                    return Ok(Route::Forward(n));
                }
            }
        }
        Ok(Route::Unavailable { retry_after: self.cfg.sweep_every })
    }

    fn cache_remote(&self, key: &str, n: &Node) {
        let now = clock::now();
        let mut st = self.state.lock();
        if st.remote.len() >= 8192 {
            st.remote.retain(|_, (_, until)| *until > now);
        }
        st.remote.insert(key.to_string(), (n.clone(), now + self.cfg.remote_cache_ttl));
    }

    // ---- handoff ----

    fn start_handoff(&self, st: &mut State, key: &str) -> bool {
        let Some(h) = st.held.get_mut(key) else { return false };
        if h.handoff.is_some() {
            return true;
        }
        h.handoff = Some(clock::now());
        h.tenure.ending.send_replace(true);
        st.handoffs.insert(key.to_string());
        st.release.insert(key.to_string());
        tracing::info!(node = %self.me(), key, "handing off");
        self.wake_tick();
        true
    }

    /// Finish handoffs whose guards are gone (or that timed out): `Lost`, then expire the lease.
    fn progress_handoffs(&self) -> bool {
        let now = clock::now();
        let ready = |st: &State| -> Vec<(String, i64)> {
            st.handoffs
                .iter()
                .filter_map(|k| st.held.get(k).map(|h| (k, h)))
                .filter(|(_, h)| h.handoff.is_some_and(|t| h.tenure.inflight.load(Ordering::Acquire) == 0 || now >= t + self.cfg.handoff_timeout))
                .map(|(k, h)| (k.clone(), h.epoch))
                .collect()
        };
        let pending = {
            let st = self.state.lock();
            if ready(&st).is_empty() {
                return !st.handoffs.is_empty();
            }
            true
        };
        let _ops = self.ops.lock();
        type Rows = Vec<(String, i64)>;
        let (rows, retired): (Rows, Rows) = {
            let mut st = self.state.lock();
            let rows = ready(&st);
            let mut retired = Vec::new();
            for (k, _) in &rows {
                let inflight = st.held.get(k).map_or(0, |h| h.tenure.inflight.load(Ordering::Acquire));
                bump(&self.counters.handoffs, 1);
                if inflight > 0 {
                    tracing::warn!(node = %self.me(), key = %k, inflight, "handoff timed out waiting for requests in flight");
                    bump(&self.counters.handoff_failures, 1);
                }
                if let Some(h) = self.drop_held(&mut st, k, "handed off")
                    && h.retire
                {
                    retired.push(k.clone());
                }
                st.release.insert(k.clone());
            }
            rows.into_iter().partition(|(k, _)| !retired.contains(k))
        };
        if !rows.is_empty() {
            self.release_rows(&rows);
        }
        if !retired.is_empty() {
            self.delete_rows(&retired);
        }
        pending
    }

    /// Delete leases we held (after they've been dropped locally): nobody takes them until a
    /// request comes. Must be called with `ops` held. If it fails, they're expired instead.
    fn delete_rows(&self, rows: &[(String, i64)]) {
        let res = self.backend().and_then(|b| b.delete(rows));
        let mut st = self.state.lock();
        match res {
            Ok(()) => {
                for (k, _) in rows {
                    st.release.remove(k);
                    tracing::info!(node = %self.me(), key = %k, "gave a lease up");
                }
            }
            Err(e) => {
                tracing::warn!(node = %self.me(), error = %e, keys = rows.len(), "deleting leases failed; they're expired instead");
                for (k, _) in rows {
                    st.release.insert(k.clone());
                }
            }
        }
    }

    fn rebalance(&self) {
        if self.no_acquire() {
            return;
        }
        let now = clock::now();
        let mut st = self.state.lock();
        if !st.eligible.iter().any(|n| n == self.me()) {
            // We don't see ourselves as live (our heartbeat is failing): don't shuffle keys.
            return;
        }
        let pending = st.handoffs.len();
        let budget = self.cfg.handoffs_per_round.saturating_sub(pending);
        if budget == 0 {
            return;
        }
        let generation = st.nodes_gen;
        let eligible = std::mem::take(&mut st.eligible);
        let nodes = std::mem::take(&mut st.nodes);
        let mut moving = Vec::new();
        for (k, h) in st.held.iter_mut() {
            if h.pref.0 != generation {
                h.pref = (generation, placement::place(k, &eligible, |id| zone_in(&nodes, id)) == Some(self.me()));
            }
            if !h.pref.1 && h.handoff.is_none() && h.deadline > now {
                moving.push(k.clone());
            }
        }
        st.eligible = eligible;
        st.nodes = nodes;
        moving.sort();
        moving.truncate(budget);
        for k in moving {
            self.start_handoff(&mut st, &k);
        }
    }

    /// Take takeable leases (expired, released, or held by a node that's gone) whose preferred
    /// node is this one.
    fn sweep(&self) -> Result<()> {
        if self.no_acquire() {
            return Ok(());
        }
        let page = self.cfg.sweep_page;
        let mut refreshed = false;
        for _ in 0..SWEEP_PAGES {
            let (rows, full) = self.backend()?.sweep(page)?;
            if rows.is_empty() {
                if full {
                    continue;
                }
                break;
            }
            if !refreshed {
                // Placement from a fresh view: a node that just started draining must not be
                // picked for the keys it's releasing.
                self.refresh_nodes()?;
                refreshed = true;
            }
            let mine: Vec<String> = {
                let st = self.state.lock();
                rows.into_iter()
                    .filter(|k| !st.held.contains_key(k) && self.preferred_in(&st, k).is_some_and(|n| n.id == self.me()))
                    .collect()
            };
            for chunk in mine.chunks(ACQUIRE_BATCH) {
                self.acquire_keys(chunk, None)?;
            }
            if !full {
                break;
            }
        }
        Ok(())
    }

    fn delete_dead_nodes(&self) {
        if let Err(e) = self.backend().and_then(|b| b.cleanup()) {
            tracing::debug!(error = %e, "cleaning up dead nodes failed");
        }
    }

    // ---- threads ----

    fn tick_loop(self: Arc<Self>) {
        let c = self.cfg.clone();
        let start = clock::now();
        let (mut next_hb, mut next_sweep) = (start + c.heartbeat, start);
        let mut next_rebalance = start + c.rebalance_every;
        let mut next_cleanup = start + c.node_ttl * 12;
        let mut next_renew = start + c.renew_every;
        // Renewal comes first: it's checked again between every other step, so a slow
        // heartbeat or sweep doesn't push it back.
        let renew_if_due = |next_renew: &mut Duration| {
            let now = clock::now();
            if now < *next_renew || self.paused() {
                return;
            }
            *next_renew = match self.renew() {
                // Paced from when it was sent, not when it came back.
                Ok(()) => (now + c.renew_every).max(clock::now() + c.renew_every / 4),
                Err(Error::Superseded) => clock::now() + c.renew_every,
                Err(e) => {
                    tracing::warn!(node = %self.me(), error = %e, "renewing leases failed");
                    clock::now() + (c.renew_every / 4).max(Duration::from_millis(10))
                }
            };
        };
        while !self.flags.stop.load(Ordering::SeqCst) && !self.flags.superseded.load(Ordering::SeqCst) {
            if self.paused() {
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            }
            // Each step checks `paused` again: a stopped (or, in tests, frozen) node must not
            // finish the rest of a round after a slow statement.
            renew_if_due(&mut next_renew);
            if clock::now() >= next_hb && !self.paused() {
                let now = clock::now();
                match self.heartbeat().and_then(|_| self.refresh_nodes()) {
                    Ok(()) => {}
                    Err(Error::Superseded) => break,
                    Err(e) => tracing::warn!(node = %self.me(), error = %e, "heartbeat failed"),
                }
                next_hb = (now + c.heartbeat).max(clock::now() + c.heartbeat / 4);
                renew_if_due(&mut next_renew);
            }
            if clock::now() >= next_sweep && !self.paused() {
                if let Err(e) = self.sweep() {
                    tracing::warn!(node = %self.me(), error = %e, "sweeping expired leases failed");
                }
                next_sweep = clock::now() + c.sweep_every;
                renew_if_due(&mut next_renew);
            }
            if clock::now() >= next_rebalance && !self.paused() {
                self.rebalance();
                next_rebalance = clock::now() + c.rebalance_every;
            }
            if clock::now() >= next_cleanup && !self.paused() {
                self.delete_dead_nodes();
                next_cleanup = clock::now() + c.node_ttl * 12;
                renew_if_due(&mut next_renew);
            }
            if self.paused() {
                continue;
            }
            let handing_off = self.progress_handoffs();
            let now = clock::now();
            let mut next = next_hb.min(next_renew).min(next_sweep).min(next_rebalance).min(next_cleanup);
            if handing_off {
                // Handoffs end on guard drops (which wake us) or on their timeout.
                next = next.min(now + Duration::from_millis(50));
            }
            if next > now {
                std::thread::park_timeout(next - now);
            }
        }
    }

    fn paused(&self) -> bool {
        self.flags.stop.load(Ordering::SeqCst) || self.flags.superseded.load(Ordering::SeqCst) || self.faults.freeze.load(Ordering::SeqCst)
    }

    fn watchdog_loop(self: Arc<Self>) {
        let poll = (self.cfg.lease_margin / 8).clamp(Duration::from_millis(5), Duration::from_millis(100));
        while !self.flags.stop.load(Ordering::SeqCst) {
            let next = self.fence_expired();
            let now = clock::now();
            let wait = next.map_or(poll, |d| d.saturating_sub(now).min(poll));
            std::thread::park_timeout(wait.max(Duration::from_millis(1)));
        }
    }

    /// Give everything up at once (leave): `Lost` for every key, and release the ones without
    /// requests in flight. Keys with requests in flight are left to expire.
    fn give_up_all(&self) {
        let ops = self.ops.try_lock_for(Duration::from_secs(5));
        let (rows, busy): (Vec<(String, i64)>, bool) = {
            let mut st = self.state.lock();
            let keys: Vec<String> = st.held.keys().cloned().collect();
            let (mut rows, mut busy) = (Vec::new(), false);
            for k in keys {
                if let Some(h) = self.drop_held(&mut st, &k, "this node is leaving") {
                    if h.tenure.inflight.load(Ordering::Acquire) == 0 {
                        rows.push((k, h.epoch));
                    } else {
                        busy = true;
                    }
                }
            }
            (rows, busy)
        };
        if ops.is_none() {
            tracing::warn!(node = %self.me(), "a lease write is stuck; leaving without releasing leases (they expire)");
            return;
        }
        if !rows.is_empty() {
            self.release_rows(&rows);
        }
        let released_all = !busy && self.state.lock().release.is_empty();
        if let Err(e) = self.backend().and_then(|b| b.leave(released_all)) {
            tracing::warn!(node = %self.me(), error = %e, "removing this node's membership failed; it goes stale");
        }
    }
}

/// [`clock::now`] in ms.
fn now_ms() -> u64 {
    clock::now().as_millis() as u64
}

/// No guard of the tenure in flight, and none granted or dropped for `min_idle`.
fn is_idle(t: &Tenure, min_idle: Duration) -> bool {
    t.inflight.load(Ordering::Acquire) == 0 && now_ms().saturating_sub(t.last_active.load(Ordering::Relaxed)) >= min_idle.as_millis() as u64
}

/// This node's membership in the cluster. See the crate docs.
pub struct Cluster {
    pub(crate) inner: Arc<Inner>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster").field("node_id", &self.inner.cfg.node_id).finish()
    }
}

impl Cluster {
    /// Join the cluster on `backend`: register this node (waiting for a previous process with
    /// the same pinned id to look dead), and start the lease thread and the fencing watchdog.
    /// Blocks on the backend.
    pub fn join(backend: Box<dyn Backend>, cfg: ClusterConfig) -> Result<Arc<Cluster>> {
        cfg.validate()?;
        let reg = Registration { node_id: cfg.node_id.clone(), url: cfg.advertise_url.clone(), zone: cfg.zone.clone() };
        backend.join(&reg, Timing { node_ttl: cfg.node_ttl, lease_ttl: cfg.lease_ttl, renew_every: cfg.renew_every })?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(cfg.connect_timeout)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))?;
        let now = clock::now();
        let inner = Arc::new(Inner {
            validity: cfg.lease_validity(),
            cfg,
            backend,
            state: Mutex::new(State {
                held: HashMap::new(),
                release: HashSet::new(),
                nodes: Vec::new(),
                nodes_gen: 0,
                eligible: Vec::new(),
                remote: HashMap::new(),
                subs: Vec::new(),
                singleton_backoff: HashMap::new(),
                handoffs: HashSet::new(),
                earliest: None,
            }),
            ops: Mutex::new(()),
            flags: Flags::default(),
            faults: Faults::default(),
            counters: Counters::default(),
            health: Mutex::new(Health { heartbeat_ok: Some(now), renew_ok_sent: Some(now) }),
            tick: OnceLock::new(),
            watchdog: OnceLock::new(),
            http,
        });
        let cluster = Cluster { inner: inner.clone(), threads: Mutex::new(Vec::new()) };
        // From here on, dropping `cluster` on an error leaves cleanly.
        inner.refresh_nodes()?;
        let spawn = |name: &str, f: fn(Arc<Inner>)| {
            let i = inner.clone();
            std::thread::Builder::new().name(name.into()).spawn(move || f(i)).map_err(|e| Error::Config(format!("spawning {name}: {e}")))
        };
        let tick = spawn("roll-cluster-lease", Inner::tick_loop)?;
        let _ = inner.tick.set(tick.thread().clone());
        let watchdog = spawn("roll-cluster-fence", Inner::watchdog_loop)?;
        let _ = inner.watchdog.set(watchdog.thread().clone());
        cluster.threads.lock().extend([tick, watchdog]);
        tracing::info!(node = %inner.cfg.node_id, url = %inner.cfg.advertise_url, backend = inner.backend.name(), "joined the cluster");
        Ok(Arc::new(cluster))
    }

    pub fn node_id(&self) -> &str {
        &self.inner.cfg.node_id
    }

    /// This node.
    pub fn node(&self) -> Node {
        Node { id: self.inner.cfg.node_id.clone(), url: self.inner.cfg.advertise_url.clone() }
    }

    pub fn config(&self) -> &ClusterConfig {
        &self.inner.cfg
    }

    /// Ownership changes on this node. The receiver first gets `Acquired` for every key held
    /// right now, then every change as it happens. Per key, events strictly alternate
    /// (`Acquired`, `Lost`, `Acquired`, …): a `Lost` is always delivered before a later
    /// `Acquired` of the same key. Events are sent while the state lock is held, so all
    /// subscribers see one order. Dropping the receiver unsubscribes.
    pub fn subscribe_ownership(&self) -> mpsc::UnboundedReceiver<Ownership> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut st = self.inner.state.lock();
        for (k, h) in &st.held {
            let _ = tx.send(Ownership::Acquired { key: k.clone(), epoch: h.epoch });
        }
        st.subs.push(tx);
        rx
    }

    /// Whether this node holds `key` with a lease still within its local deadline (true until
    /// `Lost` is emitted for it, including while it's being handed off).
    pub fn owns(&self, key: &str) -> bool {
        let st = self.inner.state.lock();
        st.held.get(key).is_some_and(|h| h.deadline > clock::now())
    }

    /// The epoch of `key` if this node owns it.
    pub fn epoch(&self, key: &str) -> Option<i64> {
        let st = self.inner.state.lock();
        st.held.get(key).filter(|h| h.deadline > clock::now()).map(|h| h.epoch)
    }

    /// Every key this node owns, with its epoch.
    pub fn owned(&self) -> Vec<(String, i64)> {
        let now = clock::now();
        let st = self.inner.state.lock();
        let mut v: Vec<(String, i64)> = st.held.iter().filter(|(_, h)| h.deadline > now).map(|(k, h)| (k.clone(), h.epoch)).collect();
        v.sort();
        v
    }

    /// A guard for local work on `key` (a landing, a queue batch) if this node owns it and isn't
    /// handing it off. Never calls the backend.
    pub fn guard(&self, key: &str) -> Option<Guard> {
        match self.inner.local(key) {
            Local::Guard(g) => Some(g),
            _ => None,
        }
    }

    /// The live, non-draining nodes as last seen (every heartbeat), this one included if it's
    /// eligible.
    pub fn nodes(&self) -> Vec<Node> {
        let st = self.inner.state.lock();
        st.nodes.iter().filter(|n| st.eligible.contains(&n.node.id)).map(|n| n.node.clone()).collect()
    }

    /// The node `key` belongs on: rendezvous hashing over the live, non-draining nodes.
    pub fn preferred(&self, key: &str) -> Option<Node> {
        let st = self.inner.state.lock();
        self.inner.preferred_in(&st, key)
    }

    /// Where a standby of `key` belongs (a hot standby, a replica): the next node by rendezvous
    /// weight, in another zone than the preferred node's when one is live. A hint only: nothing
    /// about leases depends on zones. A key `<key>#<name>` is placed this way by itself
    /// ([`placement::place`]): a read replica's lease, say.
    pub fn standby(&self, key: &str) -> Option<Node> {
        let st = self.inner.state.lock();
        let pick = placement::standby(key, key, &st.eligible, |id| zone_in(&st.nodes, id))?;
        st.nodes.iter().find(|n| n.node.id == pick).map(|n| n.node.clone())
    }

    /// Take `key` regardless of placement if it's free (no lease, an expired or released one,
    /// or a holder that's gone) or already ours. `Some(epoch)` if this node now owns it (and
    /// isn't handing it off). Blocks on the backend. Emits `Acquired` when it takes the key.
    pub fn acquire(&self, key: &str) -> Result<Option<i64>> {
        if let Some(r) = self.held_usable(key) {
            return Ok(r);
        }
        self.inner.acquire_keys(&[key.to_string()], None)?;
        Ok(self.held_usable(key).flatten())
    }

    /// Take `key` here (as [`Cluster::acquire`]) and a guard for work on it: `None` if another
    /// node holds it. For async code; the backend is called on the blocking pool.
    pub async fn lease(self: &Arc<Self>, key: &str) -> Result<Option<Guard>> {
        if let Some(g) = self.guard(key) {
            return Ok(Some(g));
        }
        let (me, k) = (self.clone(), key.to_string());
        tokio::task::spawn_blocking(move || me.acquire(&k)).await.map_err(|e| Error::Internal(format!("lease task: {e}")))??;
        Ok(self.guard(key))
    }

    /// Run `fut` while this node owns `key`, holding a guard so a handoff waits for it (up to
    /// `handoff_timeout`). `None` if the key isn't ours, or stops being ours (the work is
    /// dropped at its next await, checked every 100 ms: its writes must be safe to leave half
    /// done, as for a crash).
    pub async fn while_owner<T>(&self, key: &str, fut: impl Future<Output = T>) -> Option<T> {
        let g = self.guard(key)?;
        self.while_guarded(&g, fut).await
    }

    /// [`Cluster::while_owner`] under a guard already held.
    pub async fn while_guarded<T>(&self, g: &Guard, fut: impl Future<Output = T>) -> Option<T> {
        let mut fut = std::pin::pin!(fut);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                v = &mut fut => return Some(v),
                _ = tick.tick() => {
                    // Held through a handoff (which waits for this guard), gone once it's lost,
                    // fenced, or the process stopped taking part.
                    if self.epoch(g.key()) != Some(g.epoch()) {
                        return None;
                    }
                }
            }
        }
    }

    /// The backend's record of `key`'s lease: the node it names and its epoch (diagnostics and
    /// tests; a record doesn't mean the lease is held). Blocks on the backend.
    pub fn lease_record(&self, key: &str) -> Result<Option<(String, i64)>> {
        self.inner.backend()?.holder(key)
    }

    /// Which backend this node runs on (`memory`, `postgres`, `dynamodb`).
    pub fn backend_name(&self) -> &'static str {
        self.inner.backend.name()
    }

    fn held_usable(&self, key: &str) -> Option<Option<i64>> {
        let st = self.inner.state.lock();
        let h = st.held.get(key)?;
        Some((h.deadline > clock::now() && h.handoff.is_none()).then_some(h.epoch))
    }

    /// Where a request for `key` runs (see [`Route`]). Answers from memory when this node owns
    /// the key or recently routed it elsewhere; otherwise reads the lease (and may acquire it),
    /// so it blocks on the backend: call it from blocking context, or use
    /// [`Cluster::dispatch`], which does that for you.
    pub fn route(&self, key: &str) -> Result<Route> {
        self.inner.route(key)
    }

    /// Forget where `key` was routed (after a forward to that node failed or answered
    /// Unavailable).
    pub fn forget(&self, key: &str) {
        self.inner.state.lock().remote.remove(key);
    }

    /// Start handing `key` off now (normally rebalancing does this): no new guards, wait for
    /// the ones in flight, `Lost`, release. False if not held. A key whose preferred node is
    /// this one comes back at the next sweep.
    pub fn handoff(&self, key: &str) -> bool {
        let mut st = self.inner.state.lock();
        self.inner.start_handoff(&mut st, key)
    }

    /// Give `key` up for good (its repo was deleted): like [`Cluster::handoff`] (no new guards,
    /// wait for the ones in flight, `Lost`), but the lease is then deleted, so no node takes it
    /// until a request for the key comes. False if not held.
    pub fn retire(&self, key: &str) -> bool {
        let mut st = self.inner.state.lock();
        let Some(h) = st.held.get_mut(key) else { return false };
        h.retire = true;
        self.inner.start_handoff(&mut st, key)
    }

    /// Keys held with no guard in flight, granted or dropped none for `min_idle`, and not being
    /// handed off.
    pub fn idle(&self, min_idle: Duration) -> Vec<String> {
        let now = clock::now();
        let st = self.inner.state.lock();
        let mut v: Vec<String> = st
            .held
            .iter()
            .filter(|(_, h)| h.handoff.is_none() && h.deadline > now && is_idle(&h.tenure, min_idle))
            .map(|(k, _)| k.clone())
            .collect();
        v.sort();
        v
    }

    /// Retire `key` if it's still idle for `min_idle` and `still_idle` (asked while no guard can
    /// be granted for it, so nothing new can start meanwhile) says nothing is left to do: the
    /// lease is deleted after `Lost`, and the next request acquires it again. Requests that
    /// arrive while `still_idle` runs are answered Unavailable (forwarders retry). Blocks for as
    /// long as `still_idle` does. True if the key is being given up.
    pub fn release_if_idle(&self, key: &str, min_idle: Duration, still_idle: impl FnOnce() -> bool) -> bool {
        {
            let mut st = self.inner.state.lock();
            let now = clock::now();
            let Some(h) = st.held.get_mut(key) else { return false };
            if h.handoff.is_some() || h.deadline <= now || !is_idle(&h.tenure, min_idle) {
                return false;
            }
            // No guards while we look (as during a handoff, but nothing has ended yet).
            h.handoff = Some(now);
        }
        let idle = still_idle();
        let mut st = self.inner.state.lock();
        let Some(h) = st.held.get_mut(key) else { return false };
        let quiet = h.tenure.inflight.load(Ordering::Acquire) == 0;
        h.handoff = None;
        if !idle || !quiet || self.inner.no_acquire() {
            return false;
        }
        h.retire = true;
        tracing::info!(node = %self.inner.me(), key, "giving an idle lease up");
        self.inner.start_handoff(&mut st, key)
    }

    /// Whether this node runs the cluster-wide singleton `name` (the lease
    /// `singleton:<name>`). Takes it when this node is its preferred node and it's free; call
    /// it every time before doing the singleton's work. Blocks on the backend at most once
    /// per `sweep_every` while it isn't ours.
    pub fn singleton(&self, name: &str) -> bool {
        let key = format!("singleton:{name}");
        if let Some(r) = self.held_usable(&key) {
            return r.is_some();
        }
        let inner = &self.inner;
        if inner.no_acquire() {
            return false;
        }
        {
            let mut st = inner.state.lock();
            if inner.preferred_in(&st, &key).is_none_or(|n| n.id != inner.me()) {
                return false;
            }
            let now = clock::now();
            if st.singleton_backoff.get(&key).is_some_and(|t| *t > now) {
                return false;
            }
            if st.singleton_backoff.len() > 1024 {
                st.singleton_backoff.retain(|_, t| *t > now);
            }
            st.singleton_backoff.insert(key.clone(), now + inner.cfg.sweep_every);
        }
        match inner.acquire_keys(std::slice::from_ref(&key), Some(Duration::from_millis(100))) {
            Ok(_) => self.held_usable(&key).flatten().is_some(),
            Err(e) => {
                tracing::debug!(error = %e, key, "taking a singleton failed");
                false
            }
        }
    }

    /// This node's lease counters since it joined.
    pub fn stats(&self) -> Stats {
        let c = &self.inner.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Stats { handoffs: get(&c.handoffs), handoff_failures: get(&c.handoff_failures), takeovers: get(&c.takeovers), fenced: get(&c.fenced) }
    }

    /// For the load balancer's health check: this node is a member, its heartbeat is recent,
    /// and its last successful renewal is still within the lease's local validity.
    pub fn is_healthy(&self) -> bool {
        let inner = &self.inner;
        if inner.flags.left.load(Ordering::SeqCst) || inner.flags.superseded.load(Ordering::SeqCst) {
            return false;
        }
        let now = clock::now();
        let h = inner.health.lock();
        h.heartbeat_ok.is_some_and(|t| now < t + inner.cfg.node_ttl) && h.renew_ok_sent.is_some_and(|t| now < t + inner.validity)
    }

    /// Leave gracefully (SIGTERM): mark this node draining so no node picks it, hand every key
    /// off (each waits for its guards, up to `handoff_timeout`), and [`Cluster::leave`] once
    /// nothing is held or `timeout` has passed.
    pub async fn drain(&self, timeout: Duration) {
        let inner = self.inner.clone();
        if inner.flags.left.load(Ordering::SeqCst) {
            return;
        }
        inner.flags.draining.store(true, Ordering::SeqCst);
        {
            let mut st = inner.state.lock();
            inner.recompute_eligible(&mut st);
        }
        let i = inner.clone();
        let marked = tokio::task::spawn_blocking(move || i.backend().and_then(|b| b.set_draining())).await;
        if !matches!(marked, Ok(Ok(_))) {
            tracing::warn!(node = %inner.me(), "marking this node draining failed; handing off anyway");
        }
        {
            let mut st = inner.state.lock();
            let keys: Vec<String> = st.held.keys().cloned().collect();
            for k in keys {
                inner.start_handoff(&mut st, &k);
            }
        }
        inner.wake_tick();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && !inner.state.lock().held.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let left = inner.state.lock().held.len();
        if left > 0 {
            tracing::warn!(node = %inner.me(), keys = left, timeout_s = timeout.as_secs(), "drain timed out with keys still held");
            bump(&inner.counters.handoff_failures, left);
        }
        let me = self.inner.clone();
        let threads = std::mem::take(&mut *self.threads.lock());
        let _ = tokio::task::spawn_blocking(move || leave(&me, threads)).await;
    }

    /// Leave now: stop the threads, give up every key (`Lost`; released unless requests are in
    /// flight, in which case it expires), and delete this node's row. Idempotent; also runs on
    /// drop. Blocks on the backend (a few seconds at most if it's unreachable).
    pub fn leave(&self) {
        let threads = std::mem::take(&mut *self.threads.lock());
        leave(&self.inner, threads);
    }

    pub fn sign(&self, method: &axum::http::Method, path_and_query: &str) -> axum::http::HeaderMap {
        sign::sign(&self.inner.cfg.secret, self.node_id(), method, path_and_query, None, sign::Carried::default())
    }

    /// Check an internal request's signature (see [`crate::sign`]); the sender's node id.
    pub fn verify(&self, headers: &axum::http::HeaderMap, method: &axum::http::Method, path_and_query: &str) -> Result<String, sign::VerifyError> {
        sign::verify(&self.inner.cfg.secret, headers, method, path_and_query)
    }

    // ---- fault injection, for tests ----

    /// Test hook: stall the lease thread (no heartbeats, renewals, sweeps or handoffs), as a
    /// stuck process would. The fencing watchdog keeps running.
    #[doc(hidden)]
    pub fn fault_freeze(&self, on: bool) {
        self.inner.faults.freeze.store(on, Ordering::SeqCst);
        self.inner.wake_tick();
    }

    /// Test hook: every backend call of this node fails (it is unreachable).
    #[doc(hidden)]
    pub fn fault_db(&self, on: bool) {
        self.inner.faults.fail_db.store(on, Ordering::SeqCst);
    }

    /// Test hook: die without cleaning up (no `Lost`, no releases, the row stays), like a
    /// killed process. Its leases expire on their own.
    #[doc(hidden)]
    pub fn fault_kill(&self) {
        let inner = &self.inner;
        inner.flags.left.store(true, Ordering::SeqCst);
        inner.flags.stop.store(true, Ordering::SeqCst);
        inner.faults.freeze.store(true, Ordering::SeqCst);
        inner.wake_tick();
        if let Some(t) = inner.watchdog.get() {
            t.unpark();
        }
        let mut st = inner.state.lock();
        st.held.clear();
        st.subs.clear();
    }
}

fn leave(inner: &Arc<Inner>, threads: Vec<JoinHandle<()>>) {
    if inner.flags.left.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let mut st = inner.state.lock();
        inner.recompute_eligible(&mut st);
    }
    inner.flags.stop.store(true, Ordering::SeqCst);
    inner.wake_tick();
    if let Some(t) = inner.watchdog.get() {
        t.unpark();
    }
    // The lease thread may be stuck in a backend call; don't wait on it forever.
    let deadline = Instant::now() + Duration::from_secs(5);
    for t in threads {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if t.is_finished() {
            let _ = t.join();
        }
    }
    inner.give_up_all();
    tracing::info!(node = %inner.cfg.node_id, "left the cluster");
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.leave();
    }
}

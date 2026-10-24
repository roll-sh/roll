//! The lease state machine: membership, leases with self-fencing, placement, routing, guards,
//! handoff, rebalancing, draining and the background threads that drive them.

use crate::sql::{SCHEMA, Sql};
use crate::{Error, Result, clock, placement, sign};
use code_sql::{Conn, Db, Isolation, OptionalExtension, params};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// On another node, which holds a valid lease (or should take the key).
    Remote(Node),
    /// Nowhere right now (the key is being handed off, this node's lease is past its local
    /// deadline, the holder is dead but its lease hasn't expired yet, or no node is live): ask
    /// again after `retry_after`.
    Unavailable { retry_after: Duration },
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::Local(g) => write!(f, "Local({} @{})", g.key, g.epoch),
            Route::Remote(n) => write!(f, "Remote({})", n.id),
            Route::Unavailable { retry_after } => write!(f, "Unavailable({retry_after:?})"),
        }
    }
}

/// One tenure of a key on this node: from acquiring it to losing it.
struct Tenure {
    inflight: AtomicUsize,
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
}

#[derive(Clone)]
struct NodeRow {
    node: Node,
    draining: bool,
}

struct State {
    held: HashMap<String, Held>,
    /// Keys this node wants gone: if the database still says they're ours (a release that
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
    db: Db,
    sql: Sql,
    /// This process's incarnation of the node id (the `started_at` of its row).
    started_at: i64,
    validity: Duration,
    state: Mutex<State>,
    /// Serializes this node's lease writes (acquire, renew, release) with the local state
    /// changes that follow them, so the two never disagree in a way another write could see.
    /// Lock order: `ops` before `state`.
    ops: Mutex<()>,
    flags: Flags,
    pub(crate) faults: Faults,
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

    fn ttl_ms(&self) -> i64 {
        self.cfg.lease_ttl.as_millis() as i64
    }

    fn fault(&self) -> Result<()> {
        if self.faults.fail_db.load(Ordering::SeqCst) {
            return Err(Error::Fault("the database is unreachable from this node (test fault)"));
        }
        Ok(())
    }

    fn read<T>(&self, mut f: impl FnMut(&Conn<'_>) -> code_sql::Result<T>) -> Result<T> {
        self.fault()?;
        Ok(self.db.read(|c| f(c))?)
    }

    fn write<T>(&self, iso: Isolation, mut f: impl FnMut(&Conn<'_>) -> code_sql::Result<T>) -> Result<T> {
        self.fault()?;
        Ok(self.db.write_with(iso, |c| f(c))?)
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
        let tenure = Arc::new(Tenure { inflight: AtomicUsize::new(0), ending });
        // Placement is computed on first use (no generation matches `u64::MAX`).
        st.held.insert(key.to_string(), Held { epoch, deadline, handoff: None, tenure, pref: (u64::MAX, true) });
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
        for k in expired {
            self.drop_held(&mut st, &k, "its lease passed its local deadline without a renewal");
        }
        st.earliest = st.held.values().map(|h| h.deadline).min();
        st.earliest
    }

    // ---- membership ----

    fn heartbeat(&self) -> Result<()> {
        let sent = clock::now();
        let draining = self.flags.draining.load(Ordering::SeqCst) as i64;
        let (id, url, started) = (self.me(), self.cfg.advertise_url.as_str(), self.started_at);
        let superseded = self.write(Isolation::ReadCommitted, |c| {
            if c.execute(&self.sql.heartbeat, params![id, started])? == 1 {
                return Ok(false);
            }
            // No row of ours: either another process took this node id over (it waited for us
            // to look dead first), or our row was cleaned up as dead while we were stalled.
            let other: Option<i64> = c.query_row("SELECT started_at FROM code_nodes WHERE id = ?1", [id], |r| r.get(0)).optional()?;
            if other.is_some() {
                return Ok(true);
            }
            c.execute(&self.sql.reinsert_node, params![id, url, started, draining])?;
            Ok(false)
        })?;
        if superseded {
            self.supersede();
            return Err(Error::Superseded);
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
        let ttl = self.cfg.node_ttl.as_millis() as i64;
        let rows = self.read(|c| {
            c.query(&self.sql.live_nodes, [ttl], |r| {
                Ok(NodeRow { node: Node { id: r.get(0)?, url: r.get(1)? }, draining: r.get::<_, i64>(2)? != 0 })
            })
        })?;
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
        let id = placement::rendezvous(key, st.eligible.iter().map(String::as_str))?;
        st.nodes.iter().find(|n| n.node.id == id).map(|n| n.node.clone())
    }

    // ---- leases ----

    /// Renew every lease of this node in one statement, then reconcile local state with what
    /// the database says we hold.
    fn renew(&self) -> Result<()> {
        let _ops = self.ops.lock();
        let sent = clock::now();
        let rows: Vec<(String, i64)> = self.write(Isolation::ReadCommitted, |c| {
            c.query(&self.sql.renew, params![self.ttl_ms(), self.me(), self.started_at], |r| Ok((r.get(0)?, r.get(1)?)))
        })?;
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
            for k in gone {
                self.drop_held(&mut st, &k, "the renewal found it expired or held by another node");
            }
            for (k, &epoch) in &returned {
                if let Some(h) = st.held.get_mut(k) {
                    h.deadline = h.deadline.max(deadline);
                    continue;
                }
                // Ours in the database but not held here: an acquisition whose answer was lost,
                // a lease fenced while the database was slow, or one we meant to give up.
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
        let res = self.write(Isolation::ReadCommitted, |c| {
            let mut n = 0;
            for (k, e) in rows {
                n += c.execute(&self.sql.release, params![k, self.me(), *e])?;
            }
            Ok(n)
        });
        let mut st = self.state.lock();
        match res {
            Ok(_) => {
                for (k, _) in rows {
                    st.release.remove(k);
                }
            }
            Err(e) => {
                tracing::warn!(node = %self.me(), error = %e, keys = rows.len(), "releasing leases failed; they'll be released again or expire");
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
        let sent = clock::now();
        let won: Vec<(String, i64)> = self.write(Isolation::Serializable, |c| {
            let mut won = Vec::new();
            for k in &todo {
                let e = c.query_row(&self.sql.acquire, params![k.as_str(), self.me(), self.ttl_ms(), self.started_at], |r| r.get::<_, i64>(0)).optional()?;
                if let Some(e) = e {
                    won.push(((*k).clone(), e));
                }
            }
            Ok(won)
        })?;
        let deadline = sent + self.validity;
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
        Local::Guard(Guard { inner: self.clone(), key: key.to_string(), epoch: h.epoch, tenure: h.tenure.clone() })
    }

    /// The answer of [`Inner::route`] if it needs no database access.
    pub(crate) fn route_fast(self: &Arc<Self>, key: &str) -> Option<Route> {
        match self.local(key) {
            Local::Guard(g) => return Some(Route::Local(g)),
            Local::Busy(d) => return Some(Route::Unavailable { retry_after: d }),
            Local::None => {}
        }
        let st = self.state.lock();
        st.remote.get(key).filter(|(_, until)| *until > clock::now()).map(|(n, _)| Route::Remote(n.clone()))
    }

    pub(crate) fn route(self: &Arc<Self>, key: &str) -> Result<Route> {
        for _ in 0..3 {
            if let Some(r) = self.route_fast(key) {
                return Ok(r);
            }
            let row = self.read(|c| {
                c.query_row(&self.sql.lease, [key], |r| {
                    Ok(LeaseRow {
                        node_id: r.get(0)?,
                        expires_at: r.get(2)?,
                        now: r.get(3)?,
                        url: r.get(4)?,
                        heartbeat_at: r.get(5)?,
                    })
                })
                .optional()
            })?;
            if let Some(r) = row.as_ref().filter(|r| r.expires_at > r.now) {
                let left = Duration::from_millis((r.expires_at - r.now).max(1) as u64);
                if r.node_id == self.me() {
                    // Ours in the database, not usable here: being released, fenced, or about
                    // to be adopted by the next renewal.
                    return Ok(Route::Unavailable { retry_after: left.min(self.cfg.renew_every) });
                }
                let live = r.heartbeat_at.is_some_and(|hb| hb > r.now - self.cfg.node_ttl.as_millis() as i64);
                return Ok(match (&r.url, live) {
                    (Some(url), true) => {
                        let n = Node { id: r.node_id.clone(), url: url.clone() };
                        self.cache_remote(key, &n);
                        Route::Remote(n)
                    }
                    // The holder looks dead; its lease runs out first.
                    _ => Route::Unavailable { retry_after: left.min(self.cfg.lease_ttl) },
                });
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
                    return Ok(Route::Remote(n));
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
        let rows: Vec<(String, i64)> = {
            let mut st = self.state.lock();
            let rows = ready(&st);
            for (k, _) in &rows {
                let inflight = st.held.get(k).map_or(0, |h| h.tenure.inflight.load(Ordering::Acquire));
                if inflight > 0 {
                    tracing::warn!(node = %self.me(), key = %k, inflight, "handoff timed out waiting for requests in flight");
                }
                self.drop_held(&mut st, k, "handed off");
                st.release.insert(k.clone());
            }
            rows
        };
        if !rows.is_empty() {
            self.release_rows(&rows);
        }
        pending
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
        let mut moving = Vec::new();
        for (k, h) in st.held.iter_mut() {
            if h.pref.0 != generation {
                h.pref = (generation, placement::rendezvous(k, eligible.iter().map(String::as_str)) == Some(self.me()));
            }
            if !h.pref.1 && h.handoff.is_none() && h.deadline > now {
                moving.push(k.clone());
            }
        }
        st.eligible = eligible;
        moving.sort();
        moving.truncate(budget);
        for k in moving {
            self.start_handoff(&mut st, &k);
        }
    }

    /// Take expired leases whose preferred node is this one.
    fn sweep(&self, cursor: &mut (i64, String)) -> Result<()> {
        if self.no_acquire() {
            return Ok(());
        }
        let page = self.cfg.sweep_page;
        let mut refreshed = false;
        for _ in 0..SWEEP_PAGES {
            let rows: Vec<(String, i64)> = self.read(|c| {
                c.query(&self.sql.sweep, params![cursor.0, cursor.1.as_str(), page as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            })?;
            let full = rows.len() == page;
            match (full, rows.last()) {
                (true, Some((k, e))) => *cursor = (*e, k.clone()),
                _ => *cursor = (i64::MIN, String::new()),
            }
            if rows.is_empty() {
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
                    .map(|(k, _)| k)
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
        let age = (self.cfg.node_ttl * 12).max(Duration::from_secs(60)).as_millis() as i64;
        if let Err(e) = self.write(Isolation::ReadCommitted, |c| c.execute(&self.sql.delete_dead_nodes, [age])) {
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
        let mut cursor = (i64::MIN, String::new());
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
                if let Err(e) = self.sweep(&mut cursor) {
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
        let rows: Vec<(String, i64)> = {
            let mut st = self.state.lock();
            let keys: Vec<String> = st.held.keys().cloned().collect();
            let mut rows = Vec::new();
            for k in keys {
                if let Some(h) = self.drop_held(&mut st, &k, "this node is leaving")
                    && h.tenure.inflight.load(Ordering::Acquire) == 0
                {
                    rows.push((k, h.epoch));
                }
            }
            rows
        };
        if ops.is_none() {
            tracing::warn!(node = %self.me(), "a lease write is stuck; leaving without releasing leases (they expire)");
            return;
        }
        if !rows.is_empty() {
            self.release_rows(&rows);
        }
        if let Err(e) = self.write(Isolation::ReadCommitted, |c| c.execute(&self.sql.delete_node, params![self.me(), self.started_at])) {
            tracing::warn!(node = %self.me(), error = %e, "deleting this node's row failed; it goes stale");
        }
    }
}

struct LeaseRow {
    node_id: String,
    expires_at: i64,
    now: i64,
    url: Option<String>,
    heartbeat_at: Option<i64>,
}

/// Register this process as `cfg.node_id`, waiting (up to `node_ttl + lease_ttl` and a bit)
/// for a previous process with the same id to look dead and for its leases to expire. Returns
/// the new incarnation (`started_at`).
fn claim_node_id(db: &Db, sql: &Sql, cfg: &ClusterConfig) -> Result<i64> {
    let deadline = Instant::now() + cfg.node_ttl + cfg.lease_ttl + Duration::from_secs(1);
    let node_ttl = cfg.node_ttl.as_millis() as i64;
    loop {
        let claimed = db.write(|c| -> code_sql::Result<Option<i64>> {
            let now: i64 = c.query_row(&sql.now, (), |r| r.get(0))?;
            let prev: Option<(i64, i64)> =
                c.query_row("SELECT started_at, heartbeat_at FROM code_nodes WHERE id = ?1", [&cfg.node_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if prev.is_some_and(|(_, hb)| hb > now - node_ttl) {
                return Ok(None);
            }
            let held: i64 =
                c.query_row("SELECT COUNT(*) FROM code_leases WHERE node_id = ?1 AND expires_at > CAST(?2 AS BIGINT)", params![&cfg.node_id, now], |r| r.get(0))?;
            if held > 0 {
                return Ok(None);
            }
            let started = prev.map_or(now, |(s, _)| now.max(s + 1));
            c.execute(
                "INSERT INTO code_nodes (id, url, started_at, heartbeat_at, draining) VALUES (?1, ?2, CAST(?3 AS BIGINT), CAST(?4 AS BIGINT), 0) \
                 ON CONFLICT (id) DO UPDATE SET url = excluded.url, started_at = excluded.started_at, heartbeat_at = excluded.heartbeat_at, draining = 0",
                params![&cfg.node_id, &cfg.advertise_url, started, now],
            )?;
            // Expired leases a previous process with this id left behind: disown them, so taking
            // one again counts as a change of holder (the epoch goes up).
            c.execute("UPDATE code_leases SET node_id = '' WHERE node_id = ?1", [&cfg.node_id])?;
            Ok(Some(started))
        })?;
        if let Some(s) = claimed {
            return Ok(s);
        }
        if Instant::now() >= deadline {
            return Err(Error::NodeIdInUse(cfg.node_id.clone()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
    /// Join the cluster: create the tables if needed, register this node (waiting for a
    /// previous process with the same pinned id to look dead), and start the lease thread and
    /// the fencing watchdog. Blocks on the database.
    pub fn join(db: Db, cfg: ClusterConfig) -> Result<Arc<Cluster>> {
        cfg.validate()?;
        db.ensure_schema(SCHEMA)?;
        let sql = Sql::new(db.backend());
        let started_at = claim_node_id(&db, &sql, &cfg)?;
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
            db,
            sql,
            started_at,
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
        let tick = spawn("code-cluster-lease", Inner::tick_loop)?;
        let _ = inner.tick.set(tick.thread().clone());
        let watchdog = spawn("code-cluster-fence", Inner::watchdog_loop)?;
        let _ = inner.watchdog.set(watchdog.thread().clone());
        cluster.threads.lock().extend([tick, watchdog]);
        tracing::info!(node = %inner.cfg.node_id, url = %inner.cfg.advertise_url, "joined the code cluster");
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
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Ownership> {
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
    /// handing it off. Never touches the database.
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

    /// Take `key` regardless of placement if it's free (no lease, or expired on the database's
    /// clock) or already ours. `Some(epoch)` if this node now owns it (and isn't handing it
    /// off). Blocks on the database. Emits `Acquired` when it takes the key.
    pub fn acquire(&self, key: &str) -> Result<Option<i64>> {
        if let Some(r) = self.held_usable(key) {
            return Ok(r);
        }
        self.inner.acquire_keys(&[key.to_string()], None)?;
        Ok(self.held_usable(key).flatten())
    }

    fn held_usable(&self, key: &str) -> Option<Option<i64>> {
        let st = self.inner.state.lock();
        let h = st.held.get(key)?;
        Some((h.deadline > clock::now() && h.handoff.is_none()).then_some(h.epoch))
    }

    /// Where a request for `key` runs (see [`Route`]). Answers from memory when this node owns
    /// the key or recently routed it elsewhere; otherwise reads the lease (and may acquire it),
    /// so it blocks on the database: call it from blocking context, or use
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

    /// Whether this node runs the cluster-wide singleton `name` (the lease
    /// `singleton:<name>`). Takes it when this node is its preferred node and it's free; call
    /// it every time before doing the singleton's work. Blocks on the database at most once
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
        let marked = tokio::task::spawn_blocking(move || {
            i.write(Isolation::ReadCommitted, |c| c.execute(&i.sql.set_draining, params![i.me(), i.started_at]))
        })
        .await;
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
        let me = self.inner.clone();
        let threads = std::mem::take(&mut *self.threads.lock());
        let _ = tokio::task::spawn_blocking(move || leave(&me, threads)).await;
    }

    /// Leave now: stop the threads, give up every key (`Lost`; released unless requests are in
    /// flight, in which case it expires), and delete this node's row. Idempotent; also runs on
    /// drop. Blocks on the database (a few seconds at most if it's unreachable).
    pub fn leave(&self) {
        let threads = std::mem::take(&mut *self.threads.lock());
        leave(&self.inner, threads);
    }

    pub fn sign(&self, method: &axum::http::Method, path_and_query: &str) -> axum::http::HeaderMap {
        sign::sign(&self.inner.cfg.secret, self.node_id(), method, path_and_query, None, None)
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

    /// Test hook: every database statement of this node fails.
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
    // The lease thread may be stuck in a database call; don't wait on it forever.
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
    tracing::info!(node = %inner.cfg.node_id, "left the code cluster");
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.leave();
    }
}

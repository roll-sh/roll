//! Budgets that keep one tenant from starving the others.
//!
//! - [`RateLimiter`]: a token bucket per cluster (requests per second with a burst), checked
//!   before a request's body is read.
//! - [`ByteBudget`]: request body bytes in flight per cluster (Elasticsearch's indexing
//!   pressure: past it, 429), reserved from `Content-Length` before the body is read, and
//!   grown as bytes arrive when there is none (chunked bodies) or as a compressed body
//!   inflates ([`ByteBudget::grow`]), so no body is buffered beyond its tenant's budget.
//! - [`FairScheduler`]: a node-wide cap on requests *running* at once, granted round-robin
//!   across clusters. Each cluster still has its own in-flight cap and queue
//!   ([`crate::node::Lease::acquire`]); this decides whose queued request runs next when the
//!   node is busy, so a tenant with hundreds of queued requests gets one turn per round like a
//!   tenant with one. Waiters that go away (client disconnected) are skipped.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::oneshot;

/// A token bucket.
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    state: Mutex<(f64, Instant)>,
}

impl RateLimiter {
    /// `rate` requests per second, up to `burst` at once.
    pub fn new(rate: f64, burst: f64) -> RateLimiter {
        let burst = burst.max(1.0);
        RateLimiter { rate: rate.max(0.0), burst, state: Mutex::new((burst, Instant::now())) }
    }

    /// Takes a token if there is one.
    pub fn try_take(&self) -> bool {
        let mut g = self.state.lock();
        let now = Instant::now();
        let refill = now.duration_since(g.1).as_secs_f64() * self.rate;
        g.0 = (g.0 + refill).min(self.burst);
        g.1 = now;
        if g.0 >= 1.0 {
            g.0 -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }
}

/// Bytes in flight under a limit.
pub struct ByteBudget {
    limit: AtomicUsize,
    used: AtomicUsize,
}

/// Bytes reserved from a [`ByteBudget`]; returned when dropped.
pub struct Reservation {
    budget: Option<Arc<ByteBudget>>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(b) = &self.budget {
            b.used.fetch_sub(self.bytes, Ordering::SeqCst);
        }
    }
}

impl Reservation {
    pub fn none() -> Reservation {
        Reservation { budget: None, bytes: 0 }
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Gives `bytes` of the reservation back (a compressed body dropped once inflated).
    pub fn shrink(&mut self, bytes: usize) {
        let n = bytes.min(self.bytes);
        if let Some(b) = &self.budget {
            b.used.fetch_sub(n, Ordering::SeqCst);
        }
        self.bytes -= n;
    }
}

impl ByteBudget {
    pub fn new(limit: usize) -> Arc<ByteBudget> {
        Arc::new(ByteBudget { limit: AtomicUsize::new(limit), used: AtomicUsize::new(0) })
    }

    /// Reserves `bytes`, or returns the bytes in use when that would pass the limit. A single
    /// request larger than the whole budget is still let through when nothing else is in
    /// flight (the body limit bounds it).
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Reservation, usize> {
        let mut cur = self.used.load(Ordering::SeqCst);
        loop {
            if cur > 0 && cur.saturating_add(bytes) > self.limit() {
                return Err(cur);
            }
            match self.used.compare_exchange(cur, cur + bytes, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Ok(Reservation { budget: Some(self.clone()), bytes }),
                Err(v) => cur = v,
            }
        }
    }

    /// Adds `bytes` to `r` (a reservation of this budget, or none yet), or returns the bytes in
    /// use when that would pass the limit. As with [`ByteBudget::reserve`], a request may grow
    /// past the limit only while nothing else is in flight.
    pub fn grow(self: &Arc<Self>, r: &mut Reservation, bytes: usize) -> Result<(), usize> {
        debug_assert!(r.budget.as_ref().is_none_or(|b| Arc::ptr_eq(b, self)));
        let mut cur = self.used.load(Ordering::SeqCst);
        loop {
            let others = cur.saturating_sub(r.bytes);
            if others > 0 && cur.saturating_add(bytes) > self.limit() {
                return Err(cur);
            }
            match self.used.compare_exchange(cur, cur + bytes, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
        if r.budget.is_none() {
            r.budget = Some(self.clone());
        }
        r.bytes += bytes;
        Ok(())
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Changes the limit; reservations already granted stay.
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }
}

struct FairState {
    free: usize,
    /// Clusters with waiters, in turn order.
    ring: VecDeque<Arc<str>>,
    waiters: HashMap<Arc<str>, VecDeque<oneshot::Sender<FairPermit>>>,
}

/// Node-wide running-request slots, granted round-robin by cluster.
pub struct FairScheduler {
    state: Mutex<FairState>,
    capacity: usize,
}

/// A running slot; the next waiter (in cluster turn order) gets it when dropped.
pub struct FairPermit {
    sched: Option<Arc<FairScheduler>>,
}

impl Drop for FairPermit {
    fn drop(&mut self) {
        if let Some(s) = self.sched.take() {
            s.release();
        }
    }
}

impl FairScheduler {
    pub fn new(capacity: usize) -> Arc<FairScheduler> {
        let capacity = capacity.max(1);
        Arc::new(FairScheduler { state: Mutex::new(FairState { free: capacity, ring: VecDeque::new(), waiters: HashMap::new() }), capacity })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Slots in use.
    pub fn running(&self) -> usize {
        self.capacity - self.state.lock().free
    }

    /// Requests waiting, over every cluster.
    pub fn waiting(&self) -> usize {
        self.state.lock().waiters.values().map(VecDeque::len).sum()
    }

    /// Waits for a slot for a request of `cluster`.
    pub async fn acquire(self: &Arc<Self>, cluster: &Arc<str>) -> FairPermit {
        let rx = {
            let mut st = self.state.lock();
            if st.free > 0 && st.ring.is_empty() {
                st.free -= 1;
                return FairPermit { sched: Some(self.clone()) };
            }
            let (tx, rx) = oneshot::channel();
            let st = &mut *st;
            let q = st.waiters.entry(cluster.clone()).or_default();
            if q.is_empty() {
                st.ring.push_back(cluster.clone());
            }
            q.push_back(tx);
            rx
        };
        match rx.await {
            Ok(p) => p,
            // The scheduler went away: run unscheduled.
            Err(_) => FairPermit { sched: None },
        }
    }

    fn release(self: &Arc<Self>) {
        let mut st = self.state.lock();
        loop {
            let Some(c) = st.ring.pop_front() else {
                st.free += 1;
                return;
            };
            let Some(q) = st.waiters.get_mut(&c) else { continue };
            let mut granted = false;
            while let Some(tx) = q.pop_front() {
                // A waiter that gave up hands the slot on to the next one.
                match tx.send(FairPermit { sched: Some(self.clone()) }) {
                    Ok(()) => {
                        granted = true;
                        break;
                    }
                    // Not handed out: disarm it (its drop would re-enter this lock).
                    Err(mut p) => p.sched = None,
                }
            }
            if q.is_empty() {
                st.waiters.remove(&c);
            } else {
                st.ring.push_back(c);
            }
            if granted {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn token_bucket() {
        let r = RateLimiter::new(10.0, 3.0);
        assert!(r.try_take() && r.try_take() && r.try_take());
        assert!(!r.try_take());
        std::thread::sleep(Duration::from_millis(120));
        assert!(r.try_take());
        assert!(!r.try_take());
    }

    #[test]
    fn byte_budget() {
        let b = ByteBudget::new(100);
        let a = b.reserve(60).unwrap();
        assert_eq!(b.reserve(50).err(), Some(60));
        let c = b.reserve(40).unwrap();
        drop(a);
        drop(c);
        assert_eq!(b.used(), 0);
        // Alone, a request larger than the budget passes.
        let big = b.reserve(500).unwrap();
        assert!(b.reserve(1).is_err());
        drop(big);
    }

    #[test]
    fn byte_budget_grows_as_bodies_arrive() {
        let b = ByteBudget::new(1000);
        // A chunked upload: nothing reserved up front, frames reserved as they arrive.
        let mut first = Reservation::none();
        b.grow(&mut first, 600).unwrap();
        b.grow(&mut first, 300).unwrap();
        assert_eq!((b.used(), first.bytes()), (900, 900));
        // A second upload can't take what's left plus more.
        let mut second = Reservation::none();
        assert_eq!(b.grow(&mut second, 200), Err(900));
        b.grow(&mut second, 100).unwrap();
        assert_eq!(b.used(), 1000);
        assert!(b.reserve(1).is_err());
        // Alone, a body may grow past the budget (the body limit bounds it).
        drop(second);
        b.grow(&mut first, 5000).unwrap();
        assert_eq!(b.used(), 5900);
        // An inflated body gives its compressed bytes back.
        first.shrink(5000);
        assert_eq!((b.used(), first.bytes()), (900, 900));
        first.shrink(10_000);
        assert_eq!((b.used(), first.bytes()), (0, 0));
        drop(first);
        assert_eq!(b.used(), 0);
    }

    #[tokio::test]
    async fn round_robin_across_clusters() {
        let s = FairScheduler::new(1);
        let noisy: Arc<str> = "noisy".into();
        let quiet: Arc<str> = "quiet".into();
        let first = s.acquire(&noisy).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for i in 0..20 {
            let (s, c, o) = (s.clone(), noisy.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let p = s.acquire(&c).await;
                o.lock().push(format!("n{i}"));
                drop(p);
            }));
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (s2, q, o) = (s.clone(), quiet.clone(), order.clone());
        let quiet_task = tokio::spawn(async move {
            let p = s2.acquire(&q).await;
            o.lock().push("quiet".to_string());
            drop(p);
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(s.waiting(), 21);
        drop(first);
        quiet_task.await.unwrap();
        for t in tasks {
            t.await.unwrap();
        }
        let order = order.lock();
        // The quiet tenant runs second, not after the 20 queued noisy requests.
        let pos = order.iter().position(|x| x == "quiet").unwrap();
        assert!(pos <= 1, "quiet ran at {pos}: {order:?}");
        assert_eq!(s.running(), 0);
    }

    #[tokio::test]
    async fn abandoned_waiters_pass_their_turn() {
        let s = FairScheduler::new(1);
        let c: Arc<str> = "c".into();
        let held = s.acquire(&c).await;
        let s2 = s.clone();
        let c2 = c.clone();
        let gone = tokio::spawn(async move { s2.acquire(&c2).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        gone.abort();
        let _ = gone.await;
        let s3 = s.clone();
        let c3 = c.clone();
        let next = tokio::spawn(async move { drop(s3.acquire(&c3).await) });
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(held);
        tokio::time::timeout(Duration::from_secs(2), next).await.unwrap().unwrap();
        assert_eq!(s.running(), 0);
    }
}

//! A memory budget shared by every request the git server is handling.
//!
//! Each request takes a [`Lease`] and charges it for what it actually holds, as it comes to hold
//! it: body bytes as they're read, inflated and delta-resolved objects as unpacking produces
//! them, objects read for a fetch as they're read. Charges are released as the memory is freed,
//! and whatever a lease still holds is released when it's dropped, so a request that fails or
//! whose client goes away gives everything back.
//!
//! A charge that doesn't fit waits for others to release, up to `wait`, then fails with
//! [`BudgetError::Busy`]. Two rules keep one large request from starving the rest:
//!
//! - No request may hold more than `per_request`; a charge past that fails at once with
//!   [`BudgetError::TooLarge`] instead of waiting for memory that can never come.
//! - A request holding more than a small allowance can only grow while the total stays under
//!   `per_request`, so the last `total - per_request` bytes are always left for small requests
//!   (ref advertisements, negotiation, small pushes and fetches).
//!
//! When every request holding memory is waiting for more, nobody would ever release, so waiting
//! could only end in timeouts: the youngest of them fails at once as busy instead, and the older
//! ones, which have done more, go on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error("the git server is busy (its memory budget is in use); try again in a moment")]
    Busy,
    #[error("this request needs more than {0} bytes of server memory; split it into smaller pushes")]
    TooLarge(u64),
}

/// A memory budget; clones share it.
#[derive(Clone)]
pub struct MemoryBudget {
    inner: Arc<Inner>,
}

struct Inner {
    total: u64,
    per_request: u64,
    /// Requests holding at most this may use the bytes kept for small requests.
    small: u64,
    wait: Duration,
    state: Mutex<State>,
    notify: Notify,
}

#[derive(Default)]
struct State {
    used: u64,
    peak: u64,
    next: u64,
    leases: HashMap<u64, Slot>,
    busy: u64,
}

#[derive(Default)]
struct Slot {
    held: u64,
    /// Charges of this lease currently waiting.
    waiting: u32,
    cancelled: bool,
}

enum Decision {
    Grant,
    Wait,
    Fail(BudgetError),
    /// Fail that (younger) lease and wait.
    Doom(u64),
}

impl std::fmt::Debug for MemoryBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBudget")
            .field("total", &self.inner.total)
            .field("per_request", &self.inner.per_request)
            .field("wait", &self.inner.wait)
            .field("used", &self.used())
            .finish()
    }
}

impl MemoryBudget {
    /// `total` bytes for every request together, at most `per_request` (clamped to `total`) for
    /// one, waiting up to `wait` for memory.
    pub fn new(total: u64, per_request: u64, wait: Duration) -> Self {
        let per_request = per_request.min(total);
        let small = ((total - per_request) / 4).min(64 << 20);
        MemoryBudget {
            inner: Arc::new(Inner { total, per_request, small, wait, state: Mutex::default(), notify: Notify::new() }),
        }
    }

    /// No limit (for callers outside the server).
    pub fn unlimited() -> Self {
        Self::new(u64::MAX, u64::MAX, Duration::ZERO)
    }

    pub fn total(&self) -> u64 {
        self.inner.total
    }

    pub fn per_request(&self) -> u64 {
        self.inner.per_request
    }

    /// Bytes charged right now.
    pub fn used(&self) -> u64 {
        self.inner.lock().used
    }

    /// The most ever charged at once.
    pub fn peak(&self) -> u64 {
        self.inner.lock().peak
    }

    /// Open leases (requests in progress).
    pub fn leases(&self) -> usize {
        self.inner.lock().leases.len()
    }

    /// Charges that failed as busy.
    pub fn busy_count(&self) -> u64 {
        self.inner.lock().busy
    }

    /// A lease for one request.
    pub fn lease(&self) -> Lease {
        let mut st = self.inner.lock();
        let id = st.next;
        st.next += 1;
        st.leases.insert(id, Slot::default());
        Lease { inner: self.inner.clone(), id }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn decide(&self, st: &State, id: u64, n: u64) -> Decision {
        let Some(slot) = st.leases.get(&id) else { return Decision::Fail(BudgetError::Busy) };
        if slot.cancelled {
            return Decision::Fail(BudgetError::Busy);
        }
        let after = slot.held.saturating_add(n);
        if after > self.per_request {
            return Decision::Fail(BudgetError::TooLarge(self.per_request));
        }
        let limit = if after <= self.small { self.total } else { self.per_request };
        if st.used.saturating_add(n) <= limit {
            return Decision::Grant;
        }
        // Someone who holds memory and isn't blocked on the budget will release some eventually
        // (a cancelled lease is about to).
        let others_running = st.leases.iter().any(|(&other, s)| other != id && s.held > 0 && (s.waiting == 0 || s.cancelled));
        if slot.held > 0 && !others_running {
            // Every holder is waiting on another: the youngest of them fails, so older requests,
            // which have done more, go on.
            let youngest = st.leases.iter().filter(|(_, s)| s.held > 0).map(|(&k, _)| k).max().unwrap_or(id);
            return if youngest == id { Decision::Fail(BudgetError::Busy) } else { Decision::Doom(youngest) };
        }
        Decision::Wait
    }

    fn grant(st: &mut State, id: u64, n: u64) {
        st.used += n;
        st.peak = st.peak.max(st.used);
        st.leases.get_mut(&id).expect("lease").held += n;
    }

    /// Decides under the lock; on `Wait`, marks the lease waiting.
    fn try_charge(&self, id: u64, n: u64, then_wait: bool) -> Option<Result<(), BudgetError>> {
        let mut st = self.lock();
        let decision = match self.decide(&st, id, n) {
            Decision::Doom(victim) => {
                st.leases.get_mut(&victim).expect("lease").cancelled = true;
                // Wake it to fail; this charge waits for what it releases.
                self.notify.notify_waiters();
                Decision::Wait
            }
            d => d,
        };
        match decision {
            Decision::Doom(_) => unreachable!(),
            Decision::Grant => {
                Self::grant(&mut st, id, n);
                Some(Ok(()))
            }
            Decision::Fail(e) => {
                if e == BudgetError::Busy {
                    st.busy += 1;
                }
                Some(Err(e))
            }
            Decision::Wait => {
                if then_wait {
                    st.leases.get_mut(&id).expect("lease").waiting += 1;
                }
                None
            }
        }
    }
}

/// One request's share of a [`MemoryBudget`]. Dropping it releases everything it holds.
pub struct Lease {
    inner: Arc<Inner>,
    id: u64,
}

/// Unmarks a waiting charge, also when its future is dropped.
struct Waiting<'a>(&'a Lease);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(s) = self.0.inner.lock().leases.get_mut(&self.0.id) {
            s.waiting -= 1;
        }
    }
}

impl Lease {
    /// Charges `bytes`, waiting for others to release if they don't fit.
    pub async fn charge(&self, bytes: u64) -> Result<(), BudgetError> {
        if bytes == 0 {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + self.inner.wait;
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            // Registered before deciding, so a release between the decision and the wait wakes us.
            notified.as_mut().enable();
            if let Some(r) = self.inner.try_charge(self.id, bytes, true) {
                return r;
            }
            let waiting = Waiting(self);
            let timed_out = tokio::time::timeout_at(deadline, notified).await.is_err();
            drop(waiting);
            if timed_out {
                return match self.inner.try_charge(self.id, bytes, false) {
                    Some(r) => r,
                    None => {
                        self.inner.lock().busy += 1;
                        Err(BudgetError::Busy)
                    }
                };
            }
        }
    }

    /// [`Lease::charge`] from a blocking thread (not inside the runtime).
    pub fn charge_blocking(&self, bytes: u64, handle: &tokio::runtime::Handle) -> Result<(), BudgetError> {
        match self.inner.try_charge(self.id, bytes, false) {
            Some(r) => r,
            None => handle.block_on(self.charge(bytes)),
        }
    }

    /// Gives back up to `bytes`.
    pub fn release(&self, bytes: u64) {
        let mut st = self.inner.lock();
        let Some(slot) = st.leases.get_mut(&self.id) else { return };
        let n = bytes.min(slot.held);
        slot.held -= n;
        st.used -= n;
        drop(st);
        if n > 0 {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn held(&self) -> u64 {
        self.inner.lock().leases.get(&self.id).map_or(0, |s| s.held)
    }

    /// Fails this lease's pending and future charges (its request is gone).
    pub fn cancel(&self) {
        if let Some(s) = self.inner.lock().leases.get_mut(&self.id) {
            s.cancelled = true;
        }
        self.inner.notify.notify_waiters();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut st = self.inner.lock();
        if let Some(slot) = st.leases.remove(&self.id) {
            st.used -= slot.held;
        }
        drop(st);
        self.inner.notify.notify_waiters();
    }
}

/// Bytes charged to a shared lease, released when dropped.
pub(crate) struct Charge {
    lease: Arc<Lease>,
    bytes: u64,
}

impl Charge {
    pub(crate) async fn new(lease: &Arc<Lease>, bytes: u64) -> Result<Charge, BudgetError> {
        lease.charge(bytes).await?;
        Ok(Charge { lease: lease.clone(), bytes })
    }

    pub(crate) fn empty(lease: &Arc<Lease>) -> Charge {
        Charge { lease: lease.clone(), bytes: 0 }
    }

    /// Takes over `other`'s bytes.
    pub(crate) fn absorb(&mut self, mut other: Charge) {
        self.bytes += std::mem::take(&mut other.bytes);
    }

    /// Releases part of the charge early.
    pub(crate) fn release(&mut self, bytes: u64) {
        let n = bytes.min(self.bytes);
        self.bytes -= n;
        self.lease.release(n);
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.lease.release(self.bytes);
    }
}

/// Cancels the lease when the request handling it is dropped (the client went away), so work
/// left on blocking threads stops waiting for memory.
pub(crate) struct CancelOnDrop(pub(crate) Arc<Lease>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Charges pack parsing to a lease from a blocking thread; releases what it charged when dropped.
pub(crate) struct LeaseMeter {
    lease: Arc<Lease>,
    handle: tokio::runtime::Handle,
    charged: u64,
}

impl LeaseMeter {
    pub(crate) fn new(lease: Arc<Lease>, handle: tokio::runtime::Handle) -> Self {
        LeaseMeter { lease, handle, charged: 0 }
    }
}

impl crate::pack::Meter for LeaseMeter {
    fn charge(&mut self, bytes: u64) -> Result<(), crate::pack::PackError> {
        self.lease.charge_blocking(bytes, &self.handle)?;
        self.charged += bytes;
        Ok(())
    }

    fn release(&mut self, bytes: u64) {
        let n = bytes.min(self.charged);
        self.charged -= n;
        self.lease.release(n);
    }
}

impl Drop for LeaseMeter {
    fn drop(&mut self) {
        self.lease.release(self.charged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grants_waits_and_releases() {
        let b = MemoryBudget::new(100, 80, Duration::from_millis(50));
        let a = b.lease();
        a.charge(60).await.unwrap();
        assert_eq!(b.used(), 60);
        // Past one request's share: fails at once.
        assert_eq!(a.charge(30).await, Err(BudgetError::TooLarge(80)));
        // Another big request can't grow into the bytes kept for small ones…
        let c = b.lease();
        assert_eq!(c.charge(30).await, Err(BudgetError::Busy));
        // …but a small one can (small = (100 - 80) / 4 = 5).
        let small = b.lease();
        small.charge(5).await.unwrap();
        assert_eq!(b.used(), 65);
        drop(small);
        // A waiter gets memory as soon as it's released.
        let waiter = tokio::spawn({
            let b = b.clone();
            async move {
                let l = b.lease();
                l.charge(50).await.map(|_| l.held())
            }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        a.release(40);
        assert_eq!(waiter.await.unwrap(), Ok(50));
        drop(a);
        drop(c);
        assert_eq!((b.used(), b.leases()), (0, 0));
        assert!(b.peak() <= 100);
    }

    #[tokio::test]
    async fn holders_that_would_all_wait_fail_instead() {
        let b = MemoryBudget::new(100, 80, Duration::from_secs(60));
        let x = b.lease();
        let y = b.lease();
        x.charge(40).await.unwrap();
        y.charge(40).await.unwrap();
        // x waits for y to release; y then needs more too: nobody would ever release, so y fails
        // now instead of both waiting a minute.
        let xw = tokio::spawn(async move { x.charge(30).await.map(|_| x) });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(y.charge(30).await, Err(BudgetError::Busy));
        drop(y);
        let x = xw.await.unwrap().unwrap();
        assert_eq!(x.held(), 70);
    }

    #[tokio::test]
    async fn the_youngest_holder_fails_even_when_an_older_one_blocks_last() {
        let b = MemoryBudget::new(100, 80, Duration::from_secs(60));
        let old = b.lease();
        let young = b.lease();
        old.charge(40).await.unwrap();
        young.charge(30).await.unwrap();
        // young waits for old; then old blocks too: young is failed (and dropped by its task), and
        // old gets the memory.
        let yw = tokio::spawn(async move { young.charge(30).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        old.charge(30).await.unwrap();
        assert_eq!(yw.await.unwrap(), Err(BudgetError::Busy));
        assert_eq!((old.held(), b.used()), (70, 70));
    }

    #[tokio::test]
    async fn cancel_wakes_waiters() {
        let b = MemoryBudget::new(10, 10, Duration::from_secs(60));
        let hog = b.lease();
        hog.charge(10).await.unwrap();
        let l = Arc::new(b.lease());
        let w = tokio::spawn({
            let l = l.clone();
            async move { l.charge(5).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        l.cancel();
        assert_eq!(w.await.unwrap(), Err(BudgetError::Busy));
    }
}

//! Cluster ownership for nodes sharing a bucket: one owner per cluster at a time, through a
//! lease with an epoch in a metadata store.
//!
//! - **Acquire** takes a free or expired lease (bumping the epoch) or renews one this node
//!   holds (same epoch); a lease another node holds and hasn't let expire is refused with its
//!   owner.
//! - **Epochs** only grow, per cluster, also across releases, and are what search-store writes
//!   into manifests and cluster states: once a newer owner wrote anything, an older one's writes
//!   fail (`Fenced`).
//! - **Acknowledgements** need the lease: the node renews every `ttl / 6` and computes a local
//!   deadline from the instant it *sent* a successful acquire or renewal (`sent + ttl - margin`),
//!   which is never later than the store's expiry. Writes are only acknowledged before it, so
//!   a node that loses contact stops acknowledging before another node can take over.
//! - Stores: [`MemoryLeases`] (tests, one process); on a fleet, roll-cluster's leases
//!   (`search_control::cluster::ClusterLeases`).
//! - **Renewal is batched** ([`Leases::renew_many`]): a node holding thousands of clusters
//!   renews them all at once per period.

use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Who holds a lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub node: String,
    pub epoch: u64,
    /// Milliseconds until it expires as the store sees it (0: expired or released).
    pub remaining_ms: u64,
}

#[async_trait]
pub trait Leases: Send + Sync + 'static {
    /// Takes `cluster`'s lease for `node` until `ttl` from now (or renews it). `Ok(Ok(epoch))`:
    /// held under `epoch`; `Ok(Err(owner))`: another node holds it.
    async fn acquire(&self, cluster: &str, node: &str, ttl: Duration) -> anyhow::Result<Result<u64, Owner>>;
    /// Extends a lease held under `epoch`; `false` when it is not held any more. A lease that
    /// expired or was released is never extended, even if nobody took it since: a renewal that
    /// raced a release (an unload, a drain) must not keep the next owner out for a whole `ttl`.
    async fn renew(&self, cluster: &str, node: &str, epoch: u64, ttl: Duration) -> anyhow::Result<bool>;
    /// Extends every lease in `held` (`(cluster, epoch)`); answers per entry, in order. The
    /// default renews one by one; stores override it with one round trip.
    async fn renew_many(&self, held: &[(String, u64)], node: &str, ttl: Duration) -> anyhow::Result<Vec<bool>> {
        let mut out = Vec::with_capacity(held.len());
        for (cluster, epoch) in held {
            out.push(self.renew(cluster, node, *epoch, ttl).await?);
        }
        Ok(out)
    }
    /// Gives a lease up (the epoch stays: the next owner gets a higher one).
    async fn release(&self, cluster: &str, node: &str, epoch: u64) -> anyhow::Result<()>;
    async fn owner(&self, cluster: &str) -> anyhow::Result<Option<Owner>>;
    /// A check every acknowledgement of `cluster` runs too, next to the deadline renewals give:
    /// whether the lease is still held right now (roll-cluster's guard). `None`: the deadline
    /// alone decides.
    fn validity(&self, _cluster: &str) -> Option<Validity> {
        None
    }
    /// Whether the epochs [`Leases::acquire`] answers only order this store's holders (they can
    /// start over, as roll-cluster's do once a lease row is deleted): the node then writes under
    /// an epoch one past the newest ownership marker in object storage instead
    /// (`search_store::Store::claim_next`).
    fn store_epochs(&self) -> bool {
        false
    }
}

/// See [`Leases::validity`].
pub type Validity = Arc<dyn Fn() -> bool + Send + Sync>;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The decision every store applies, given the current row `(node, epoch, expires_ms)`.
fn decide(row: Option<(&str, u64, u64)>, node: &str, now: u64, ttl: Duration) -> Result<(u64, u64), Owner> {
    let expires = now + ttl.as_millis() as u64;
    match row {
        None => Ok((1, expires)),
        Some((n, e, exp)) if n == node && exp > now => Ok((e, expires)),
        Some((_, e, exp)) if exp <= now => Ok((e + 1, expires)),
        Some((n, e, exp)) => Err(Owner { node: n.to_string(), epoch: e, remaining_ms: exp - now }),
    }
}

/// Leases in memory (one process: tests, a single node).
#[derive(Default)]
pub struct MemoryLeases {
    rows: Mutex<HashMap<String, (String, u64, u64)>>,
}

impl MemoryLeases {
    pub fn new() -> Arc<MemoryLeases> {
        Arc::new(MemoryLeases::default())
    }
}

#[async_trait]
impl Leases for MemoryLeases {
    async fn acquire(&self, cluster: &str, node: &str, ttl: Duration) -> anyhow::Result<Result<u64, Owner>> {
        let mut rows = self.rows.lock();
        let row = rows.get(cluster).map(|(n, e, x)| (n.as_str(), *e, *x));
        Ok(decide(row, node, now_ms(), ttl).map(|(epoch, expires)| {
            rows.insert(cluster.to_string(), (node.to_string(), epoch, expires));
            epoch
        }))
    }

    async fn renew(&self, cluster: &str, node: &str, epoch: u64, ttl: Duration) -> anyhow::Result<bool> {
        let mut rows = self.rows.lock();
        let now = now_ms();
        match rows.get_mut(cluster) {
            Some((n, e, x)) if n == node && *e == epoch && *x > now => {
                *x = now + ttl.as_millis() as u64;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release(&self, cluster: &str, node: &str, epoch: u64) -> anyhow::Result<()> {
        if let Some((n, e, x)) = self.rows.lock().get_mut(cluster)
            && n == node
            && *e == epoch
        {
            *x = 0;
        }
        Ok(())
    }

    async fn owner(&self, cluster: &str) -> anyhow::Result<Option<Owner>> {
        let now = now_ms();
        Ok(self.rows.lock().get(cluster).map(|(n, e, x)| Owner { node: n.clone(), epoch: *e, remaining_ms: x.saturating_sub(now) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exercise(l: &dyn Leases) {
        // Long enough that a loaded test machine doesn't expire leases between two steps.
        let ttl = Duration::from_millis(1500);
        assert_eq!(l.acquire("c", "a", ttl).await.unwrap(), Ok(1));
        // Renewing through acquire keeps the epoch; another node is refused.
        assert_eq!(l.acquire("c", "a", ttl).await.unwrap(), Ok(1));
        let owner = l.acquire("c", "b", ttl).await.unwrap().unwrap_err();
        assert_eq!((owner.node.as_str(), owner.epoch), ("a", 1));
        assert!(l.renew("c", "a", 1, ttl).await.unwrap());
        assert!(!l.renew("c", "b", 1, ttl).await.unwrap());
        // Expiry lets another node in, with a higher epoch; the old holder can't renew.
        tokio::time::sleep(ttl + Duration::from_millis(100)).await;
        assert_eq!(l.acquire("c", "b", ttl).await.unwrap(), Ok(2));
        assert!(!l.renew("c", "a", 1, ttl).await.unwrap());
        // Release: the next holder (even the same node) gets a newer epoch at once.
        l.release("c", "b", 2).await.unwrap();
        // A renewal that raced the release doesn't bring the lease back, alone or batched.
        assert!(!l.renew("c", "b", 2, ttl).await.unwrap());
        assert_eq!(l.renew_many(&[("c".to_string(), 2)], "b", ttl).await.unwrap(), vec![false]);
        assert_eq!(l.acquire("c", "b", ttl).await.unwrap(), Ok(3));
        assert_eq!(l.owner("c").await.unwrap().unwrap().epoch, 3);
        assert_eq!(l.owner("nope").await.unwrap(), None);
        // Batched renewal answers per lease: held, wrong epoch, never taken.
        assert_eq!(l.acquire("c2", "b", ttl).await.unwrap(), Ok(1));
        let held = vec![("c".to_string(), 3), ("c2".to_string(), 7), ("nope".to_string(), 1)];
        assert_eq!(l.renew_many(&held, "b", Duration::from_secs(5)).await.unwrap(), vec![true, false, false]);
        assert_eq!(l.renew_many(&[], "b", ttl).await.unwrap(), Vec::<bool>::new());
        // The renewal took: past the old ttl, the lease is still b's.
        tokio::time::sleep(ttl + Duration::from_millis(100)).await;
        assert!(l.acquire("c", "a", ttl).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn memory() {
        exercise(&*MemoryLeases::new()).await;
    }
}

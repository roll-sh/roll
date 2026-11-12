//! Keeping a cluster's settings consistent with the node while it wakes.
//!
//! The node reads a cluster's description ([`ClusterSpec`]: password, limits, dialect) from the
//! catalog when a request wakes it, and caches it in the loaded instance for the cluster's whole
//! stay. The node re-reads the catalog just before it publishes a waking instance, and
//! [`search_server::Node::update_spec`] parks changes made after that until the instance is
//! published, so the control plane hands every change of a cluster with a load under way to the
//! node as well. What remains is a delete racing a wake (the instance must not come up at all)
//! and changes the node can't apply in place (the dialect: a reload).
//!
//! The control plane handles those with the three seams the node gives it:
//!
//! - **The catalog** ([`SqlCatalog`]) records when a cluster was last looked up for a wake, and
//!   hides clusters being deleted.
//! - **The metadata stores** ([`GuardedStores`]): opening a cluster's metadata is the first thing
//!   a load does. It refuses clusters that are being deleted or no longer exist (so a wake that
//!   raced a delete fails instead of resurrecting the cluster), and registers the load until the
//!   instance is reconciled (or the load fails: the store handle is dropped unused).
//! - **The load hook** ([`ControlHooks`]): when the cluster is open, the control plane waits for
//!   the node to publish the instance and re-applies the catalog's current description to it
//!   ([`crate::Control`]'s reconcile), or locks it out and evicts it if the cluster was deleted.
//!
//! Every change of a description is serialized with the re-application, and then waits for wakes
//! that may have read the old description: loads registered by the store, and lookups made just
//! before the change ([`LOOKUP_GRACE`]: the step from a lookup to opening the store has no I/O).
//! When the change returns, the node enforces the new description.

use crate::control::Control;
use crate::meter::MeterHooks;
use crate::store::Store;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use search_es::{Cluster, MetaStore};
use search_server::{Catalog, ClusterSpec, LifecycleHooks, ObjectStores, StoreProvider};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

/// How long after a catalog lookup a change still waits for the wake it may start.
pub(crate) const LOOKUP_GRACE: Duration = Duration::from_millis(100);

/// Longest a change waits for a load in progress (they normally take milliseconds).
pub(crate) const LOAD_WAIT: Duration = Duration::from_secs(30);

/// A load between opening its metadata store and being reconciled.
struct Load {
    token: u64,
    /// The load hook ran: the reconcile clears it, not the store handle.
    claimed: bool,
}

/// Wakes in progress, shared by the catalog, the stores, the hook and the control plane.
#[derive(Default)]
pub(crate) struct Wakes {
    inner: Mutex<WakeState>,
    /// Test seam: delay every metadata load by this many milliseconds (0: none).
    delay_ms: AtomicU64,
}

#[derive(Default)]
struct WakeState {
    deleting: HashSet<String>,
    looked_up: HashMap<String, Instant>,
    loading: HashMap<String, Load>,
    next: u64,
}

impl Wakes {
    pub(crate) fn is_deleting(&self, name: &str) -> bool {
        self.inner.lock().deleting.contains(name)
    }

    pub(crate) fn set_deleting(&self, name: &str, on: bool) {
        let mut st = self.inner.lock();
        if on {
            st.deleting.insert(name.to_string());
        } else {
            st.deleting.remove(name);
        }
    }

    /// The cluster is gone: forget what is tracked for it.
    pub(crate) fn forget(&self, name: &str) {
        self.inner.lock().looked_up.remove(name);
    }

    pub(crate) fn is_loading(&self, name: &str) -> bool {
        self.inner.lock().loading.contains_key(name)
    }

    /// Whether a wake that read the catalog before `since` may still be on its way.
    pub(crate) fn looked_up_near(&self, name: &str, since: Instant) -> bool {
        self.inner.lock().looked_up.get(name).is_some_and(|t| *t + LOOKUP_GRACE > since)
    }

    /// The load hook ran for `name`: returns its load's token, now the reconcile's to clear.
    pub(crate) fn claim(&self, name: &str) -> Option<u64> {
        let mut st = self.inner.lock();
        let l = st.loading.get_mut(name)?;
        l.claimed = true;
        Some(l.token)
    }

    pub(crate) fn finish(&self, name: &str, token: u64) {
        let mut st = self.inner.lock();
        if st.loading.get(name).is_some_and(|l| l.token == token) {
            st.loading.remove(name);
        }
    }

    pub(crate) fn set_delay(&self, d: Duration) {
        self.delay_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }
}

/// The node's catalog: clusters in SQLite, minus those being deleted.
pub struct SqlCatalog {
    pub(crate) store: Arc<Store>,
    pub(crate) wakes: Arc<Wakes>,
}

#[async_trait]
impl Catalog for SqlCatalog {
    async fn lookup(&self, cluster: &str) -> Option<ClusterSpec> {
        {
            let mut st = self.wakes.inner.lock();
            if st.deleting.contains(cluster) {
                return None;
            }
            // Stamped before reading: a change made after the read is after the stamp.
            st.looked_up.insert(cluster.to_string(), Instant::now());
        }
        match self.store.by_name(cluster).await {
            Ok(Some(c)) => Some(crate::control::spec_for(&c)),
            Ok(None) => {
                // Not a cluster: don't keep a stamp for every probed name.
                self.wakes.inner.lock().looked_up.remove(cluster);
                None
            }
            Err(e) => {
                tracing::warn!(cluster, error = %e, "search catalog lookup failed");
                None
            }
        }
    }
}

/// [`ObjectStores`], refusing loads of deleted clusters and tracking loads in progress.
pub(crate) struct GuardedStores {
    pub(crate) inner: ObjectStores,
    pub(crate) store: Arc<Store>,
    pub(crate) wakes: Arc<Wakes>,
}

#[async_trait]
impl StoreProvider for GuardedStores {
    async fn meta_store(&self, cluster: &str) -> anyhow::Result<Arc<dyn MetaStore>> {
        let inner = self.inner.meta_store(cluster).await?;
        // Read before the critical section (it can't await). A delete hides the row only after
        // marking the cluster deleting below, so a row seen here and no mark seen there means
        // the delete hasn't started: it will see this load and wait for it.
        let exists = self.store.by_name(cluster).await?.is_some();
        let token = {
            let mut st = self.wakes.inner.lock();
            // Checked and registered in one critical section with `set_deleting`: either the
            // delete sees this load and waits for it, or this load sees the delete.
            anyhow::ensure!(!st.deleting.contains(cluster), "cluster [{cluster}] is being deleted");
            anyhow::ensure!(exists, "cluster [{cluster}] no longer exists");
            st.next += 1;
            let token = st.next;
            st.loading.insert(cluster.to_string(), Load { token, claimed: false });
            token
        };
        Ok(Arc::new(GuardedMeta { inner, name: cluster.to_string(), token, wakes: self.wakes.clone() }))
    }

    async fn branch(&self, parent: &str, child: &str) -> anyhow::Result<()> {
        self.inner.branch(parent, child).await
    }

    async fn delete(&self, cluster: &str) -> anyhow::Result<()> {
        self.inner.delete(cluster).await
    }

    async fn release(&self, cluster: &str) {
        self.inner.release(cluster).await
    }

    async fn branch_at(&self, parent: &str, child: &str, at_ms: Option<u64>) -> anyhow::Result<()> {
        self.inner.branch_at(parent, child, at_ms).await
    }

    /// Forwarded, or the node would keep every cluster's documents in memory only.
    fn data(&self) -> Option<search_store::Store> {
        self.inner.data()
    }
}

/// A cluster's metadata store; while its load is unclaimed, dropping it ends the load.
struct GuardedMeta {
    inner: Arc<dyn MetaStore>,
    name: String,
    token: u64,
    wakes: Arc<Wakes>,
}

#[async_trait]
impl MetaStore for GuardedMeta {
    async fn load(&self) -> anyhow::Result<Option<Bytes>> {
        let delay = self.wakes.delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.inner.load().await
    }

    async fn save(&self, version: u64, state: Bytes) -> anyhow::Result<()> {
        self.inner.save(version, state).await
    }
}

impl Drop for GuardedMeta {
    fn drop(&mut self) {
        let mut st = self.wakes.inner.lock();
        // A failed load drops its store before the hook: nothing will reconcile it.
        if st.loading.get(&self.name).is_some_and(|l| l.token == self.token && !l.claimed) {
            st.loading.remove(&self.name);
        }
    }
}

/// The node's lifecycle hooks: metering, and reconciling every instance that loads.
pub(crate) struct ControlHooks {
    pub(crate) meter: MeterHooks,
    pub(crate) wakes: Arc<Wakes>,
    pub(crate) control: OnceLock<Weak<Control>>,
}

#[async_trait]
impl LifecycleHooks for ControlHooks {
    async fn loaded(&self, cluster: &Arc<Cluster>) {
        self.meter.loaded(cluster).await;
        if let Some(c) = self.control.get().and_then(Weak::upgrade) {
            c.note_loaded(&cluster.name, true).await;
        }
        let token = self.wakes.claim(&cluster.name);
        match self.control.get().and_then(Weak::upgrade) {
            // The node publishes the instance after this hook returns, so the reconcile waits
            // for it on its own task.
            Some(control) => {
                tokio::spawn(control.reconcile(cluster.name.clone(), token));
            }
            None => {
                if let Some(t) = token {
                    self.wakes.finish(&cluster.name, t);
                }
            }
        }
    }

    async fn unloading(&self, cluster: &Arc<Cluster>) {
        self.meter.unloading(cluster).await;
        if let Some(c) = self.control.get().and_then(Weak::upgrade) {
            c.note_loaded(&cluster.name, false).await;
        }
    }
}

//! Snapshot repositories' credential references (`roll://ENV/KEY`, `search_es::secret_refs`)
//! resolved for the cluster as a workspace resource: rolld installs roll secrets'
//! ([`Control::set_secrets`](crate::Control::set_secrets)), and this names who is asking.
//!
//! The resource is the cluster's id. A branch (anything with a parent) is a copy its project
//! never vetted: it asks as the cluster it came from (the root of its line, the one a canvas
//! holds) and as an untrusted copy, so roll secrets never gives it a protected environment or a
//! sensitive key. A cluster restored after a delete keeps its id and owner, so it stays trusted.
//! Without roll secrets installed (dev nodes, tests) every reference is refused.

use crate::store::Store;
use async_trait::async_trait;
pub use search_es::secret_refs::Revealed;
use search_es::secret_refs::SecretRefs;
use std::sync::{Arc, OnceLock};

/// The cluster asking, as roll secrets sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The cluster's id (`srch_…`), or for a branch its line's root's.
    pub id: String,
    /// A branch: an untrusted copy.
    pub copy: bool,
}

/// The values behind references, for a [`Holder`] (rolld's: `Secrets::resolve_for` and
/// `digest_for`). Errors say why, never with a value.
#[async_trait]
pub trait ResourceSecrets: Send + Sync {
    async fn resolve(&self, holder: &Holder, reference: &str) -> Result<Revealed, String>;
    async fn digest(&self, holder: &Holder, reference: &str) -> Result<String, String>;
}

/// How many parents a branch's line is followed up (branches of branches).
const MAX_DEPTH: usize = 64;

/// The node's resolver: clusters by name from the catalog, then roll secrets.
pub(crate) struct CatalogRefs {
    pub(crate) store: Arc<Store>,
    pub(crate) secrets: OnceLock<Arc<dyn ResourceSecrets>>,
}

impl CatalogRefs {
    async fn holder(&self, name: &str) -> Result<(Holder, Arc<dyn ResourceSecrets>), String> {
        let secrets = self.secrets.get().cloned().ok_or("roll secrets isn't available on this node")?;
        let fail = |e: anyhow::Error| format!("reading the search catalog: {e}");
        let c = self.store.by_name(name).await.map_err(fail)?.ok_or_else(|| format!("no search cluster {name}"))?;
        let mut holder = Holder { id: c.id, copy: false };
        let mut parent = c.parent_id;
        for _ in 0..MAX_DEPTH {
            let Some(p) = parent.take() else { break };
            holder = Holder { id: p.clone(), copy: true };
            // A deleted parent ends the line there.
            parent = self.store.get(&p).await.map_err(fail)?.and_then(|c| c.parent_id);
        }
        Ok((holder, secrets))
    }
}

#[async_trait]
impl SecretRefs for CatalogRefs {
    async fn resolve(&self, cluster: &str, reference: &str) -> Result<Revealed, String> {
        let (holder, secrets) = self.holder(cluster).await?;
        secrets.resolve(&holder, reference).await
    }

    async fn digest(&self, cluster: &str, reference: &str) -> Result<String, String> {
        let (holder, secrets) = self.holder(cluster).await?;
        secrets.digest(&holder, reference).await
    }
}

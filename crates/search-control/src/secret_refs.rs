//! Snapshot repositories' credential references (`roll://ENV/KEY`, `search_es::secret_refs`):
//! who may write one, and who resolves it. rolld installs roll secrets'
//! ([`Control::set_secrets`](crate::Control::set_secrets)); this names who is asking.
//!
//! **Writing.** Only [`Control::put_repository`](crate::Control::put_repository)
//! (`PUT /v1/search/clusters/{id}/repositories/{name}`) takes references, from a caller roll
//! secrets lets reveal each one ([`ResourceSecrets::may_write`], its `Caller::allows`), and records
//! the caller as their maker. The cluster's own endpoint refuses them.
//!
//! **Resolving.** For the cluster's id, as its maker: every resolve and digest re-checks that the
//! maker still may reveal what the reference names (roll secrets' `maker_caller`). A branch
//! (anything with a parent) is a copy its project never vetted: it asks as the cluster it came
//! from (the root of its line, the one a canvas holds) and as an untrusted copy, so roll secrets
//! never gives it a protected environment or a sensitive key, whoever made the reference. A
//! cluster restored after a delete keeps its id and owner, so it stays trusted. Without roll
//! secrets installed (dev nodes, tests) every reference is refused.

use crate::store::Store;
use async_trait::async_trait;
pub use search_es::secret_refs::Revealed;
use search_es::secret_refs::SecretRefs;
use std::sync::{Arc, OnceLock};

/// The cluster asking, as roll secrets sees it, and who wrote the reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The cluster's id (`srch_…`), or for a branch its line's root's.
    pub id: String,
    /// A branch: an untrusted copy.
    pub copy: bool,
    /// The reference's maker, a roll principal (`operator`, `usr_…`, `token:…`); empty while
    /// one is being written.
    pub maker: String,
}

/// Who is writing a reference: what rolld's gate put on the request (roll secrets makes its
/// `Caller` of it).
#[derive(Clone, Debug, Default)]
pub struct Who {
    pub operator: bool,
    pub user: Option<roll_auth::SignedIn>,
    pub grant: Option<roll_auth::Grant>,
}

impl Who {
    pub fn of(ext: &axum::http::Extensions) -> Who {
        Who { operator: roll_auth::Operator::on(ext), user: ext.get::<roll_auth::SignedIn>().cloned(), grant: roll_auth::Grant::on(ext).cloned() }
    }
}

/// Why a reference can't be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The caller may not reveal what it names (HTTP 403).
    Forbidden(String),
    /// It can't be used here at all: not a reference, no project standing for the cluster, no
    /// such key (HTTP 400).
    Invalid(String),
}

/// roll secrets for a [`Holder`] (rolld's: `Secrets::may_reveal_for`, `maker_caller`,
/// `resolve_for` and `digest_for`). Errors say why, never with a value.
#[async_trait]
pub trait ResourceSecrets: Send + Sync {
    /// Whether `who` may write `reference` for the cluster: they may reveal what it names. Their
    /// principal, kept as the maker.
    async fn may_write(&self, who: &Who, holder: &Holder, reference: &str) -> Result<String, Refused>;
    /// The value, for the maker (who must still be able to reveal it).
    async fn resolve(&self, holder: &Holder, reference: &str) -> Result<Revealed, String>;
    /// The digest, under the same check.
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
    pub(crate) async fn holder(&self, name: &str, maker: &str) -> Result<(Holder, Arc<dyn ResourceSecrets>), String> {
        let secrets = self.secrets.get().cloned().ok_or("roll secrets isn't available on this node")?;
        let fail = |e: anyhow::Error| format!("reading the search catalog: {e}");
        let c = self.store.by_name(name).await.map_err(fail)?.ok_or_else(|| format!("no search cluster {name}"))?;
        let mut holder = Holder { id: c.id, copy: false, maker: maker.to_string() };
        let mut parent = c.parent_id;
        for _ in 0..MAX_DEPTH {
            let Some(p) = parent.take() else { break };
            holder.id = p.clone();
            holder.copy = true;
            // A deleted parent ends the line there.
            parent = self.store.get(&p).await.map_err(fail)?.and_then(|c| c.parent_id);
        }
        Ok((holder, secrets))
    }
}

#[async_trait]
impl SecretRefs for CatalogRefs {
    async fn resolve(&self, cluster: &str, maker: &str, reference: &str) -> Result<Revealed, String> {
        let (holder, secrets) = self.holder(cluster, maker).await?;
        secrets.resolve(&holder, reference).await
    }

    async fn digest(&self, cluster: &str, maker: &str, reference: &str) -> Result<String, String> {
        let (holder, secrets) = self.holder(cluster, maker).await?;
        secrets.digest(&holder, reference).await
    }
}

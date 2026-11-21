//! Credentials held as references to another system's secrets (roll secrets' `roll://ENV/KEY`)
//! instead of copied values: a snapshot repository's credential setting may name one
//! ([`crate::api::snapshots`], "Credentials by reference"). This crate knows only the seam: the
//! node hands each cluster a [`SecretRefs`] (rolld's resolves through roll secrets, for the
//! cluster as a workspace resource). Whether a setting is a reference is the resolver's to say
//! ([`classify`], roll secrets' own parser), never guessed here: a cluster without one has no
//! references, only values.

use async_trait::async_trait;
use std::sync::Arc;

/// The header with which roll's control plane names who wrote a request's references (a roll
/// principal: `operator`, `usr_…`, `token:…`), after checking they may reveal what each names.
/// The node's REST port drops it from every request but roll's own (`search_server::http`), and
/// the control plane drops it from whatever it relays (its console).
pub const MAKER_HEADER: &str = "x-roll-repository-maker";

/// What a setting's value is, as the resolver parses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    /// A value, used as given.
    Value,
    /// A reference, resolved when it's used.
    Reference,
    /// Written like a reference but not one (a leading or trailing space, another case): refused,
    /// never used as a value. Why, without the value.
    Malformed(String),
}

/// `value` as cluster `cluster`'s resolver reads it (no resolver: a value).
pub fn classify(cluster: &crate::Cluster, value: &str) -> Setting {
    match &cluster.config.secret_refs {
        Some(r) => r.0.classify(value),
        None => Setting::Value,
    }
}

/// A resolved value: never printed (`Debug` redacts it), opened only to build a client, wiped
/// when dropped.
#[derive(Clone)]
pub struct Revealed(String);

impl Drop for Revealed {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl Revealed {
    pub fn new(value: impl Into<String>) -> Revealed {
        Revealed(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Revealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Revealed(..)")
    }
}

/// Resolves references for cluster `cluster` (its control-plane id,
/// [`crate::cluster::ClusterConfig::catalog_id`], else its name), written by `maker` (what
/// [`MAKER_HEADER`] said): both re-check that the maker still may reveal what `reference` names.
/// Errors say why (the reference, the part that failed), never with a value.
#[async_trait]
pub trait SecretRefs: Send + Sync {
    /// Whether `value` is a reference (the referenced system's own parser).
    fn classify(&self, value: &str) -> Setting;
    /// `reference`'s value now (read, and audited, by the system that holds it).
    async fn resolve(&self, cluster: &str, maker: &str, reference: &str) -> Result<Revealed, String>;
    /// A digest that moves when the value would, read without the value: cheap enough to ask
    /// before each repository operation.
    async fn digest(&self, cluster: &str, maker: &str, reference: &str) -> Result<String, String>;
}

/// A cluster's resolver ([`crate::cluster::ClusterConfig::secret_refs`]).
#[derive(Clone)]
pub struct Resolver(pub Arc<dyn SecretRefs>);

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resolver")
    }
}

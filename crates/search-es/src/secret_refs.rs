//! Credentials held as references to another system's secrets (roll secrets' `roll://ENV/KEY`)
//! instead of copied values: a snapshot repository's credential setting may name one
//! ([`crate::api::snapshots`], "Credentials by reference"). This crate knows only the seam: the
//! node hands each cluster a [`SecretRefs`] (rolld's resolves through roll secrets, for the
//! cluster as a workspace resource), and a cluster without one refuses references.

use async_trait::async_trait;
use std::sync::Arc;

/// The scheme a reference starts with.
pub const SCHEME: &str = "roll://";

/// Whether a setting's value is a reference rather than a value.
pub fn is_reference(value: &str) -> bool {
    value.trim_start().starts_with(SCHEME)
}

/// A resolved value: never printed (`Debug` redacts it), opened only to build a client.
#[derive(Clone)]
pub struct Revealed(String);

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

/// Resolves references for cluster `cluster` (its name). Errors say why (the reference, the
/// part that failed), never with a value.
#[async_trait]
pub trait SecretRefs: Send + Sync {
    /// `reference`'s value now (read, and audited, by the system that holds it).
    async fn resolve(&self, cluster: &str, reference: &str) -> Result<Revealed, String>;
    /// A digest that moves when the value would, read without the value: cheap enough to ask
    /// before each repository operation.
    async fn digest(&self, cluster: &str, reference: &str) -> Result<String, String>;
}

/// A cluster's resolver ([`crate::cluster::ClusterConfig::secret_refs`]).
#[derive(Clone)]
pub struct Resolver(pub Arc<dyn SecretRefs>);

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resolver")
    }
}

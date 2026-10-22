//! Which clusters exist, how clients prove they may use them, and where their metadata lives.
//!
//! - A [`Catalog`] answers `lookup(cluster) -> ClusterSpec`: the control plane implements it in
//!   production; [`StaticCatalog`] is a fixed map for local use and tests.
//! - A [`StoreProvider`] hands out each cluster's [`MetaStore`]. `search-store` will implement
//!   it over object storage; [`MemoryStores`] keeps states in memory, so an unloaded cluster
//!   wakes with its metadata intact for the life of the process.

use async_trait::async_trait;
use parking_lot::RwLock;
use search_es::{Dialect, MemoryStore, MetaStore};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// A secret a client may present for a cluster.
#[derive(Clone, Debug)]
pub struct Credential {
    /// The Basic auth user it belongs to (`None`: any user name, e.g. `elastic`).
    pub user: Option<String>,
    /// For `Authorization: ApiKey`, the key id (`None`: the cluster name is the id).
    pub api_key_id: Option<String>,
    pub secret: String,
}

impl Credential {
    pub fn password(secret: impl Into<String>) -> Credential {
        Credential { user: None, api_key_id: None, secret: secret.into() }
    }
}

/// What a node needs to know to serve a cluster.
#[derive(Clone, Debug)]
pub struct ClusterSpec {
    pub name: String,
    pub dialect: Dialect,
    /// Accepted secrets. Empty means no authentication (local development only).
    pub credentials: Vec<Credential>,
    /// Unload after this long without requests (`None`: the node's default).
    pub idle_unload: Option<Duration>,
    /// Requests running at once; more wait in a queue of the same size, then get 429.
    pub max_in_flight: usize,
    /// `http.max_content_length` for this cluster (`None`: the node's).
    pub max_body_bytes: Option<usize>,
    /// Default of `action.destructive_requires_name` (Elasticsearch's default is `true`).
    pub destructive_requires_name: bool,
}

impl ClusterSpec {
    pub fn new(name: impl Into<String>) -> ClusterSpec {
        ClusterSpec {
            name: name.into(),
            dialect: Dialect::Elasticsearch,
            credentials: Vec::new(),
            idle_unload: None,
            max_in_flight: 64,
            max_body_bytes: None,
            destructive_requires_name: true,
        }
    }

    pub fn with_password(mut self, secret: impl Into<String>) -> ClusterSpec {
        self.credentials.push(Credential::password(secret));
        self
    }
}

#[async_trait]
pub trait Catalog: Send + Sync + 'static {
    async fn lookup(&self, cluster: &str) -> Option<ClusterSpec>;
}

/// A fixed set of clusters (local development, tests, single-tenant deployments).
#[derive(Default)]
pub struct StaticCatalog(RwLock<HashMap<String, ClusterSpec>>);

impl StaticCatalog {
    pub fn new() -> StaticCatalog {
        StaticCatalog::default()
    }

    pub fn insert(&self, spec: ClusterSpec) {
        self.0.write().insert(spec.name.clone(), spec);
    }

    pub fn remove(&self, name: &str) -> Option<ClusterSpec> {
        self.0.write().remove(name)
    }
}

#[async_trait]
impl Catalog for StaticCatalog {
    async fn lookup(&self, cluster: &str) -> Option<ClusterSpec> {
        self.0.read().get(cluster).cloned()
    }
}

/// Where clusters keep their metadata.
#[async_trait]
pub trait StoreProvider: Send + Sync + 'static {
    /// The metadata store of `cluster` (created empty if it has none yet).
    async fn meta_store(&self, cluster: &str) -> anyhow::Result<Arc<dyn MetaStore>>;
    /// Makes `child` a copy of `parent`'s current metadata.
    async fn branch(&self, parent: &str, child: &str) -> anyhow::Result<()>;
    /// Forgets a cluster's data.
    async fn delete(&self, cluster: &str) -> anyhow::Result<()>;
    /// The cluster was unloaded from this node: drop whatever the provider tracks for it.
    async fn release(&self, _cluster: &str) {}
}

/// Metadata in memory, per cluster.
#[derive(Default)]
pub struct MemoryStores(RwLock<HashMap<String, Arc<MemoryStore>>>);

impl MemoryStores {
    pub fn new() -> MemoryStores {
        MemoryStores::default()
    }

    fn get_or_create(&self, cluster: &str) -> Arc<MemoryStore> {
        if let Some(s) = self.0.read().get(cluster) {
            return s.clone();
        }
        self.0.write().entry(cluster.to_string()).or_insert_with(|| Arc::new(MemoryStore::new())).clone()
    }
}

#[async_trait]
impl StoreProvider for MemoryStores {
    async fn meta_store(&self, cluster: &str) -> anyhow::Result<Arc<dyn MetaStore>> {
        Ok(self.get_or_create(cluster))
    }

    async fn branch(&self, parent: &str, child: &str) -> anyhow::Result<()> {
        let src = self.get_or_create(parent);
        let state = src.load().await?;
        let dst = Arc::new(MemoryStore::new());
        if let Some(bytes) = state {
            dst.save(0, bytes).await?;
        }
        self.0.write().insert(child.to_string(), dst);
        Ok(())
    }

    async fn delete(&self, cluster: &str) -> anyhow::Result<()> {
        self.0.write().remove(cluster);
        Ok(())
    }
}

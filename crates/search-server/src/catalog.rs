//! Which clusters exist, how clients prove they may use them, and where their metadata lives.
//!
//! - A [`Catalog`] answers `lookup(cluster) -> ClusterSpec`: the control plane implements it in
//!   production; [`StaticCatalog`] is a fixed map for local use and tests.
//! - A [`StoreProvider`] hands out each cluster's [`MetaStore`] and, when it keeps clusters in
//!   `search-store` ([`crate::ObjectStores`]), the node's [`search_store::Store`] for the shards
//!   ([`StoreProvider::data`]). [`MemoryStores`] keeps states in memory (and shards stay in
//!   memory too), so an unloaded cluster wakes with its metadata intact for the life of the
//!   process.
//! - Secrets can be given as SHA-256 hashes ([`Credential::hashed`]), so a catalog never has to
//!   hold them in the clear; either way they are compared in constant time.

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
    /// The secret in the clear (empty when only its hash is known).
    pub secret: String,
    /// SHA-256 of the secret; when set, presented secrets are hashed and compared with it.
    pub secret_sha256: Option<[u8; 32]>,
}

impl Credential {
    pub fn password(secret: impl Into<String>) -> Credential {
        Credential { user: None, api_key_id: None, secret: secret.into(), secret_sha256: None }
    }

    /// A credential known only by the SHA-256 of its secret.
    pub fn hashed(sha256: [u8; 32]) -> Credential {
        Credential { user: None, api_key_id: None, secret: String::new(), secret_sha256: Some(sha256) }
    }

    pub fn for_user(mut self, user: impl Into<String>) -> Credential {
        self.user = Some(user.into());
        self
    }

    pub fn for_api_key(mut self, id: impl Into<String>) -> Credential {
        self.api_key_id = Some(id.into());
        self
    }
}

/// SHA-256 of a secret (for [`Credential::hashed`]).
pub fn secret_hash(secret: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(secret.as_bytes()).into()
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
    /// Requests per second (token bucket, burst of `max(1, rate)`); `None`: unlimited.
    pub max_requests_per_second: Option<f64>,
    /// Request body bytes in flight at once (indexing pressure); `None`: twice the body limit.
    pub max_in_flight_bytes: Option<usize>,
    /// Memory the loaded cluster may hold in indexing buffers, in-heap segments and version
    /// maps; past it the node commits the cluster's shards (moving that memory to segment
    /// files) at its next check. `None`: no limit of its own.
    pub max_resident_bytes: Option<u64>,
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
            max_requests_per_second: None,
            max_in_flight_bytes: None,
            max_resident_bytes: None,
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
    /// Makes `child` a branch of `parent` as of `at_ms` (ms since the epoch; `None`: now).
    async fn branch_at(&self, parent: &str, child: &str, at_ms: Option<u64>) -> anyhow::Result<()> {
        match at_ms {
            None => self.branch(parent, child).await,
            Some(_) => anyhow::bail!("point-in-time branches need object storage"),
        }
    }
    /// The node's search-store, when clusters live in it: their shards are then durable too
    /// (WAL, segments, manifests). `None`: shards stay in memory. Wrappers must forward it.
    fn data(&self) -> Option<search_store::Store> {
        None
    }
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

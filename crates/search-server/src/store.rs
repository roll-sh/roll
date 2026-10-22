//! [`ObjectStores`]: cluster metadata on object storage through `search-store`.
//!
//! Each cluster's metadata is the JSON document `search-es` produces
//! ([`search_es::meta::ClusterState::to_json`]), written by [`search_store::Store::write_cluster`]
//! as a new generation on every change and read back (the newest generation) when the cluster
//! wakes. Branching and deletion are the store's (`branch` copies the metadata and shares the
//! segments; `delete_cluster` removes everything of the cluster). Writes are fenced by the
//! store's ownership epochs: a node that lost a cluster's lease gets an error instead of
//! overwriting the new owner's state, and the request fails before anything is published.

use crate::catalog::StoreProvider;
use async_trait::async_trait;
use bytes::Bytes;
use search_es::MetaStore;
use search_store::{ClusterState, Store};
use std::sync::Arc;

/// A [`StoreProvider`] over a node's [`Store`].
#[derive(Clone)]
pub struct ObjectStores {
    store: Store,
}

impl ObjectStores {
    pub fn new(store: Store) -> ObjectStores {
        ObjectStores { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
}

/// One cluster's metadata in the store.
struct ClusterMeta {
    store: Store,
    cluster: String,
}

#[async_trait]
impl MetaStore for ClusterMeta {
    async fn load(&self) -> anyhow::Result<Option<Bytes>> {
        Ok(self.store.read_cluster(&self.cluster).await?.map(|s| s.json))
    }

    async fn save(&self, _version: u64, state: Bytes) -> anyhow::Result<()> {
        self.store.write_cluster(&self.cluster, &ClusterState::new(state)).await?;
        Ok(())
    }
}

#[async_trait]
impl StoreProvider for ObjectStores {
    async fn meta_store(&self, cluster: &str) -> anyhow::Result<Arc<dyn MetaStore>> {
        Ok(Arc::new(ClusterMeta { store: self.store.clone(), cluster: cluster.to_string() }))
    }

    async fn branch(&self, parent: &str, child: &str) -> anyhow::Result<()> {
        self.store.branch(parent, child, None).await
    }

    async fn delete(&self, cluster: &str) -> anyhow::Result<()> {
        self.store.delete_cluster(cluster).await
    }

    async fn release(&self, cluster: &str) {
        self.store.release_cluster(cluster);
    }
}

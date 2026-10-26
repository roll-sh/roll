//! [`CheckedMeta`]: a cluster's metadata store that refuses to save a state that would not
//! load again.
//!
//! A cluster wakes by parsing its saved state ([`search_es::Cluster::open`] →
//! `ClusterState::from_json_in`). If the engine ever accepts a change that its own parser
//! rejects (a validation run when a mapping is parsed but skipped by some update path), the
//! state is saved, every later wake fails with "corrupt cluster state", and the tenant's data
//! can never be served again. Unloading is routine here (idle clusters go after minutes), so
//! that gap turns a single bad request into a permanently broken cluster. The guard closes it
//! on the write side: before a state reaches the store it is parsed exactly as a wake would
//! parse it, and a state that fails is refused, so the request fails (500, nothing published,
//! memory unchanged) and the cluster keeps its last good state.
//!
//! Parsing mappings is the expensive part of a state, and most changes touch one index, so
//! indices whose JSON is unchanged since the last good state (by a 64-bit hash of their
//! compact JSON) are not parsed again; everything else (templates, settings, pipelines, data
//! streams) is cheap and always checked. The first save after a wake checks every index (the
//! loaded state isn't kept or hashed, so wakes cost nothing extra and hold no copy of it).
//!
//! Once its cluster unloads ([`CheckedMeta::close`]) the guard refuses every save: a request
//! still running on the dropped instance (a forced unload) must not acknowledge a change that
//! the next instance won't see (with leases, a save after the store forgot the epoch would be
//! written under epoch 0 and never be the latest).

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use search_es::json::{self, Map, Value};
use search_es::meta::ClusterState;
use search_es::{Dialect, MetaStore};
use std::collections::HashMap;
use std::hash::Hasher;
use std::sync::Arc;

/// Why a state was refused (the request's error reason carries it).
#[derive(Debug)]
pub struct Unloadable(pub String);

impl std::fmt::Display for Unloadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the change would leave a cluster state that cannot be loaded again: {}", self.0)
    }
}

impl std::error::Error for Unloadable {}

/// A [`MetaStore`] that saves only states its cluster can load again.
pub struct CheckedMeta {
    inner: Arc<dyn MetaStore>,
    dialect: Dialect,
    /// Hash of each index's compact JSON in the last state saved, by index name.
    known: Mutex<HashMap<String, u64>>,
    /// States checked and refused (tests, metrics).
    pub refused: std::sync::atomic::AtomicU64,
    closed: std::sync::atomic::AtomicBool,
}

impl CheckedMeta {
    pub fn new(inner: Arc<dyn MetaStore>, dialect: Dialect) -> CheckedMeta {
        CheckedMeta { inner, dialect, known: Mutex::new(HashMap::new()), refused: Default::default(), closed: Default::default() }
    }

    /// The cluster unloaded: refuse every later save with [`crate::persist::Unloaded`].
    pub fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Parses `state` as a wake would, skipping indices whose JSON `known` already holds.
    /// Returns the hashes of every index in it.
    fn check(&self, state: &[u8], known: &HashMap<String, u64>) -> Result<HashMap<String, u64>, Unloadable> {
        let mut v = match json::parse(state) {
            Ok(Some(v)) => v,
            Ok(None) => return Err(Unloadable("empty document".into())),
            Err(e) => return Err(Unloadable(e.to_string())),
        };
        let mut hashes = HashMap::new();
        if let Value::Obj(top) = &mut v
            && let Some(Value::Obj(indices)) = top.get_mut("indices")
        {
            let all = std::mem::take(indices);
            let mut changed = Map::with_capacity(all.len().min(4));
            let mut buf = Vec::new();
            for (name, iv) in all {
                let h = hash_json(&iv, &mut buf);
                hashes.insert(name.clone(), h);
                if known.get(&name) != Some(&h) {
                    changed.insert(name, iv);
                }
            }
            *indices = changed;
        }
        ClusterState::from_json_in(&v, self.dialect).map_err(|e| Unloadable(e.to_string()))?;
        Ok(hashes)
    }
}

/// A 64-bit hash of `v`'s compact JSON (`buf` is scratch space).
fn hash_json(v: &Value, buf: &mut Vec<u8>) -> u64 {
    buf.clear();
    json::write::write_json(buf, v, false);
    let mut h = std::hash::DefaultHasher::new();
    h.write(buf);
    h.finish()
}

#[async_trait]
impl MetaStore for CheckedMeta {
    async fn load(&self) -> anyhow::Result<Option<Bytes>> {
        self.known.lock().clear();
        self.inner.load().await
    }

    async fn save(&self, version: u64, state: Bytes) -> anyhow::Result<()> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(unloaded().context(crate::persist::Unloaded));
        }
        // Saves are serialized by the cluster's write lock; the mutex only guards the cache.
        let known = std::mem::take(&mut *self.known.lock());
        let checked = self.check(&state, &known);
        let hashes = match checked {
            Ok(h) => h,
            Err(e) => {
                self.refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(error = %e, "refused to save a cluster state that would not load");
                *self.known.lock() = known;
                return Err(e.into());
            }
        };
        let r = self.inner.save(version, state).await;
        // An unload that raced the save (a forced unload: lease lost, persistence failed) may
        // have released the cluster's epoch before the write landed, so it may not be the
        // state the next owner reads: never acknowledge it.
        let raced = self.closed.load(std::sync::atomic::Ordering::SeqCst);
        match r {
            Ok(()) if !raced => {
                *self.known.lock() = hashes;
                Ok(())
            }
            Ok(()) => Err(unloaded().context(crate::persist::Unloaded)),
            Err(e) => {
                *self.known.lock() = known;
                if raced { Err(unloaded().context(format!("{e:#}"))) } else { Err(e) }
            }
        }
    }
}

/// What a client gets for a state change on a cluster that unloaded meanwhile (search-es maps
/// an [`EsError`](search_es::EsError) a [`MetaStore`] returns to the response as is).
fn unloaded() -> anyhow::Error {
    anyhow::Error::new(search_es::EsError::new(
        503,
        "cluster_block_exception",
        "blocked by: [SERVICE_UNAVAILABLE/1/state not recovered / initialized];",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};

    async fn send(c: &Cluster, method: &str, uri: &str, body: &str) -> search_es::EsResponse {
        search_es::handle(c, EsRequest::json(method, uri, body.to_string())).await
    }

    #[tokio::test]
    async fn good_states_round_trip_and_unchanged_indices_are_not_reparsed() {
        let mem: Arc<dyn MetaStore> = Arc::new(MemoryStore::new());
        let checked = Arc::new(CheckedMeta::new(mem.clone(), Dialect::Elasticsearch));
        let c = Cluster::open(ClusterConfig::new("g"), checked.clone()).await.unwrap();
        assert_eq!(send(&c, "PUT", "/a", r#"{"mappings":{"properties":{"t":{"type":"text"}}}}"#).await.status, 200);
        assert_eq!(send(&c, "PUT", "/b", "{}").await.status, 200);
        assert_eq!(checked.known.lock().len(), 2);
        // A change to b leaves a's hash (and so a isn't parsed again).
        let before = checked.known.lock().get("a").copied();
        assert_eq!(send(&c, "PUT", "/b/_mapping", r#"{"properties":{"k":{"type":"keyword"}}}"#).await.status, 200);
        assert_eq!(checked.known.lock().get("a").copied(), before);
        assert_eq!(checked.refused.load(std::sync::atomic::Ordering::Relaxed), 0);
        // Reopens with both.
        let again = Arc::new(CheckedMeta::new(mem, Dialect::Elasticsearch));
        let c2 = Cluster::open(ClusterConfig::new("g"), again.clone()).await.unwrap();
        assert_eq!(c2.state().metadata.indices.len(), 2);
        // Deleting an index after a wake works (the first save checks everything).
        assert_eq!(send(&c2, "DELETE", "/a", "").await.status, 200);
        assert_eq!(again.known.lock().keys().cloned().collect::<Vec<_>>(), vec!["b".to_string()]);
    }

    #[tokio::test]
    async fn refuses_states_that_would_not_load() {
        let mem: Arc<dyn MetaStore> = Arc::new(MemoryStore::new());
        let checked = CheckedMeta::new(mem.clone(), Dialect::Elasticsearch);
        let good = br#"{"format":1,"version":2,"cluster_uuid":"u","indices":{"i":{"uuid":"x","settings":{"index.number_of_shards":"1"},"mappings":{"properties":{"f":{"type":"keyword"}}}}}}"#;
        checked.save(2, Bytes::from_static(good)).await.unwrap();
        let bad = br#"{"format":1,"version":3,"cluster_uuid":"u","indices":{"i":{"uuid":"x","settings":{"index.number_of_shards":"1"},"mappings":{"properties":{"f":{"type":"no_such_type"}}}}}}"#;
        let e = checked.save(3, Bytes::from_static(bad)).await.unwrap_err();
        assert!(e.downcast_ref::<Unloadable>().is_some(), "{e:#}");
        assert!(format!("{e}").contains("no_such_type"), "{e}");
        assert_eq!(checked.refused.load(std::sync::atomic::Ordering::Relaxed), 1);
        // The store keeps the last good state, and the cache still describes it.
        assert_eq!(mem.load().await.unwrap().as_deref(), Some(&good[..]));
        assert_eq!(checked.known.lock().len(), 1);
        assert!(checked.save(4, Bytes::from_static(b"{not json")).await.is_err());
        assert!(checked.save(4, Bytes::from_static(b"")).await.is_err());
    }

    /// A store whose saves wait until released, so a test can unload the cluster mid-save.
    struct Gated {
        inner: MemoryStore,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl MetaStore for Gated {
        async fn load(&self) -> anyhow::Result<Option<Bytes>> {
            self.inner.load().await
        }

        async fn save(&self, version: u64, state: Bytes) -> anyhow::Result<()> {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.save(version, state).await
        }
    }

    /// A state change whose save was still running when the cluster was force-unloaded is
    /// answered like a cluster that is not ready (503 cluster_block_exception), never a 500, and
    /// nothing is published; later changes on the closed instance are refused the same way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_racing_an_unload_is_a_cluster_block() {
        let gated = Arc::new(Gated { inner: MemoryStore::new(), entered: Default::default(), release: Default::default() });
        let checked = Arc::new(CheckedMeta::new(gated.clone(), Dialect::Elasticsearch));
        let c = Arc::new(Cluster::open(ClusterConfig::new("g"), checked.clone()).await.unwrap());
        let req = tokio::spawn({
            let c = c.clone();
            async move { send(&c, "PUT", "/racing", "{}").await }
        });
        gated.entered.notified().await;
        checked.close();
        gated.release.notify_one();
        let r = req.await.unwrap();
        assert_eq!(r.status, 503, "{}", String::from_utf8_lossy(&r.body));
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["error"]["type"], "cluster_block_exception", "{v}");
        assert!(c.state().metadata.indices.is_empty(), "a refused change is not published");
        // Closed before the next change starts: refused before reaching the store.
        let r = send(&c, "PUT", "/later", "{}").await;
        assert_eq!(r.status, 503);
    }
}

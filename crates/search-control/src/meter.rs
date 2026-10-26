//! Metering: how long each cluster is loaded, how big it is while loaded, how many requests it
//! serves, and how much it stores.
//!
//! The node tells the meter when a cluster loads and unloads ([`MeterHooks`], the node's
//! `LifecycleHooks`), so loaded time is exact: every tick (60 s in rolld) accounts the whole
//! seconds since the last one for each loaded cluster, and an unload accounts the remainder
//! (rounded up: a cluster woken for 200 ms is billed one second). Nothing runs per cluster while
//! it sleeps: storage accrues from the change log in `search_storage` (see [`crate::store`]).
//!
//! Requests are the node's own count of requests admitted to each cluster
//! (`search_server::Node::take_request_counts`, console requests included), handed to
//! [`Meter::count_requests`] by every metering pass; each request is billed once, also when its
//! cluster unloaded in between.
//!
//! The resident size of a loaded cluster is measured from its engine ([`resident_bytes`]): the
//! heap its shards hold (segments still in memory, segment readers' heap and caches, indexing
//! buffers, the version map) plus [`LOADED_BASE_BYTES`] for the cluster itself (state, analysis
//! chains, scripts). It is measured every tick and once more as the cluster unloads, and each
//! interval is billed at the larger of the two measurements around it, so a cluster that grew
//! and shrank between ticks is billed for its size at either end, never less. Segments on local
//! disk are a cache and not counted (they are billed as storage).

use crate::store::{Store, UsageSample};
use async_trait::async_trait;
use parking_lot::Mutex;
use search_es::Cluster;
use search_server::LifecycleHooks;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

/// Memory a loaded cluster holds besides its segments.
pub const LOADED_BASE_BYTES: i64 = 16 << 20;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Engine figures of a loaded cluster, summed over its open shards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineFigures {
    /// Live Lucene documents, nested ones included: what `_stats` and `_cat/indices` report as
    /// `docs.count` (the root-document figure is `_count`'s), and what the commit points count
    /// while the cluster sleeps.
    pub docs: u64,
    pub deleted_docs: u64,
    /// Bytes of the shards' segments, in memory or on disk.
    pub segment_bytes: u64,
    /// Heap the shards hold (see the module docs).
    pub heap_bytes: u64,
    /// Indices of the cluster's state whose engine is open (an index's shards open on its first
    /// use after a wake).
    pub open_indices: usize,
    /// Indices in the cluster's state.
    pub indices: usize,
}

impl EngineFigures {
    /// Whether every index is open, so the figures cover the whole cluster.
    pub fn complete(&self) -> bool {
        self.open_indices == self.indices
    }
}

/// Per-index engine figures of an open index (`None` when its shards aren't open).
pub fn index_figures(cluster: &Cluster, uuid: &str) -> Option<EngineFigures> {
    let ie = cluster.engine.loaded(uuid)?;
    let mut f = EngineFigures { open_indices: 1, indices: 1, ..Default::default() };
    // Shard statistics only: no search runs, so reading them is cheap and grows no cache.
    for s in &ie.shards {
        let st = s.shard.stats();
        f.docs += st.num_docs;
        f.deleted_docs += st.deleted_docs;
        f.segment_bytes += st.segment_bytes;
        f.heap_bytes += shard_heap(&st);
    }
    Some(f)
}

/// Figures of every open index of a loaded cluster.
pub fn engine_figures(cluster: &Cluster) -> EngineFigures {
    let st = cluster.state();
    let mut total = EngineFigures { indices: st.metadata.indices.len(), ..Default::default() };
    for idx in st.metadata.indices.values() {
        if let Some(f) = index_figures(cluster, &idx.uuid) {
            total.docs += f.docs;
            total.deleted_docs += f.deleted_docs;
            total.segment_bytes += f.segment_bytes;
            total.heap_bytes += f.heap_bytes;
            total.open_indices += 1;
        }
    }
    total
}

/// What a loaded cluster holds in memory now. Reads only shard statistics: it runs no search,
/// so measuring never grows the caches it measures.
pub fn resident_bytes(cluster: &Cluster) -> i64 {
    let st = cluster.state();
    let mut heap = 0u64;
    for idx in st.metadata.indices.values() {
        if let Some(ie) = cluster.engine.loaded(&idx.uuid) {
            heap += ie.shards.iter().map(|s| shard_heap(&s.shard.stats())).sum::<u64>();
        }
    }
    LOADED_BASE_BYTES.saturating_add(i64::try_from(heap).unwrap_or(i64::MAX))
}

fn shard_heap(st: &search_index::ShardStats) -> u64 {
    st.segment_memory_bytes + st.segment_heap_bytes + st.buffer_bytes + st.version_map_bytes
}

/// A loaded cluster as the meter tracks it.
struct Resident {
    /// The time up to which its loaded time is accounted (ms).
    since: i64,
    /// The cluster, to measure (gone once the node dropped it).
    cluster: Option<Weak<Cluster>>,
    /// The size measured at `since` (`None`: not measured yet).
    bytes: Option<i64>,
}

#[derive(Default)]
struct State {
    /// Loaded clusters by name.
    loaded: HashMap<String, Resident>,
    /// Requests counted since the last sample, by cluster name.
    requests: HashMap<String, i64>,
    /// Clusters whose figures (indices, docs, stored bytes) should be refreshed.
    dirty: HashSet<String>,
    /// Index counts seen when clusters unloaded, for the next refresh.
    indices: HashMap<String, i64>,
}

pub struct Meter {
    store: Arc<Store>,
    state: Mutex<State>,
}

impl Meter {
    pub fn new(store: Arc<Store>) -> Arc<Meter> {
        Arc::new(Meter { store, state: Mutex::new(State::default()) })
    }

    pub fn count_request(&self, cluster: &str) {
        self.count_requests(cluster, 1);
    }

    pub fn count_requests(&self, cluster: &str, n: i64) {
        *self.state.lock().requests.entry(cluster.to_string()).or_default() += n;
    }

    pub fn is_loaded(&self, cluster: &str) -> bool {
        self.state.lock().loaded.contains_key(cluster)
    }

    /// A cluster loaded at `at_ms`; `instance` lets the meter measure its resident size.
    pub fn on_loaded(&self, cluster: &str, at_ms: i64, instance: Option<&Arc<Cluster>>) {
        let bytes = instance.map(|c| resident_bytes(c));
        let mut st = self.state.lock();
        st.loaded.entry(cluster.to_string()).or_insert(Resident { since: at_ms, cluster: instance.map(Arc::downgrade), bytes });
        st.dirty.insert(cluster.to_string());
    }

    /// Accounts the rest of a cluster's loaded time and marks it for a figures refresh.
    /// `resident` is its size measured now.
    pub async fn on_unloading(&self, cluster: &str, at_ms: i64, indices: Option<i64>, resident: Option<i64>) -> anyhow::Result<()> {
        let (r, requests) = {
            let mut st = self.state.lock();
            let r = st.loaded.remove(cluster);
            st.dirty.insert(cluster.to_string());
            if let Some(n) = indices {
                st.indices.insert(cluster.to_string(), n);
            }
            (r, st.requests.remove(cluster).unwrap_or(0))
        };
        let Some(r) = r else { return Ok(()) };
        let seconds = ((at_ms - r.since).max(0) + 999) / 1000;
        self.record(cluster, at_ms, seconds, requests, larger(r.bytes, resident)).await
    }

    /// One metering interval: accounts whole loaded seconds and the requests counted since the
    /// last one. Returns the clusters whose figures may need refreshing: every loaded one, and
    /// those that must be refreshed now (`true`: just loaded or unloaded).
    pub async fn sample(&self, at_ms: i64) -> anyhow::Result<Vec<(String, bool)>> {
        // Measure outside the lock: it walks every shard of the cluster.
        let instances: Vec<(String, Arc<Cluster>)> = {
            let st = self.state.lock();
            st.loaded.iter().filter_map(|(n, r)| Some((n.clone(), r.cluster.as_ref()?.upgrade()?))).collect()
        };
        let measured: HashMap<String, i64> = instances.iter().map(|(n, c)| (n.clone(), resident_bytes(c))).collect();
        drop(instances);
        let work: Vec<(String, i64, i64, Option<i64>)> = {
            let mut st = self.state.lock();
            let State { loaded, requests, .. } = &mut *st;
            let mut out = Vec::new();
            for (name, r) in loaded.iter_mut() {
                let seconds = (at_ms - r.since).max(0) / 1000;
                r.since += seconds * 1000;
                let now = measured.get(name).copied();
                out.push((name.clone(), seconds, requests.remove(name).unwrap_or(0), larger(r.bytes, now)));
                if now.is_some() {
                    r.bytes = now;
                }
            }
            // Requests of clusters that unloaded since (counted after their last interval).
            for (name, n) in requests.drain() {
                out.push((name, 0, n, None));
            }
            out
        };
        for (name, seconds, requests, bytes) in &work {
            self.record(name, at_ms, *seconds, *requests, *bytes).await?;
        }
        let mut st = self.state.lock();
        let mut refresh: Vec<(String, bool)> = st.dirty.drain().map(|n| (n, true)).collect();
        for name in st.loaded.keys() {
            if !refresh.iter().any(|(n, _)| n == name) {
                refresh.push((name.clone(), false));
            }
        }
        refresh.sort();
        Ok(refresh)
    }

    /// The index count a cluster had when it unloaded (consumed).
    pub fn take_indices(&self, cluster: &str) -> Option<i64> {
        self.state.lock().indices.remove(cluster)
    }

    /// Clusters marked for a refresh are put back when the refresh failed.
    pub fn mark_dirty(&self, cluster: &str) {
        self.state.lock().dirty.insert(cluster.to_string());
    }

    /// `resident`: the measured size over the interval; without a measurement (a cluster the
    /// meter can't see), its base plus stored bytes.
    async fn record(&self, name: &str, at_ms: i64, seconds: i64, requests: i64, resident: Option<i64>) -> anyhow::Result<()> {
        if seconds == 0 && requests == 0 {
            return Ok(());
        }
        // Deleted meanwhile: nothing to bill.
        let Some(c) = self.store.by_name(name).await? else { return Ok(()) };
        let loaded_bytes = if seconds == 0 { 0 } else { resident.unwrap_or_else(|| LOADED_BASE_BYTES.saturating_add(c.stored_bytes)) };
        let sample = UsageSample { at: at_ms / 1000, seconds, loaded_bytes, requests };
        self.store.record_usage(&c.id, &sample).await
    }
}

fn larger(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The node's lifecycle hooks: feed the meter.
pub struct MeterHooks(pub Arc<Meter>);

#[async_trait]
impl LifecycleHooks for MeterHooks {
    async fn loaded(&self, cluster: &Arc<Cluster>) {
        self.0.on_loaded(&cluster.name, now_ms(), Some(cluster));
    }

    async fn unloading(&self, cluster: &Arc<Cluster>) {
        let indices = cluster.state().metadata.indices.len() as i64;
        if let Err(e) = self.0.on_unloading(&cluster.name, now_ms(), Some(indices), Some(resident_bytes(cluster))).await {
            tracing::warn!(cluster = cluster.name, error = %e, "search metering failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SearchCluster;

    async fn meter() -> (Arc<Meter>, Arc<Store>) {
        let store = Arc::new(Store::memory().unwrap());
        store
            .insert(&SearchCluster {
                id: "srch_a".into(),
                name: "app".into(),
                region: "local".into(),
                flavor: "elasticsearch".into(),
                password: "pw".into(),
                idle_timeout_s: 300,
                max_in_flight: 64,
                max_body_mb: 100,
                destructive_requires_name: true,
                parent_id: None,
                parent_at: None,
                created_at: 0,
                password_rotated_at: None,
                indices: 0,
                docs: 0,
                stored_bytes: 1000,
            })
            .await
            .unwrap();
        (Meter::new(store.clone()), store)
    }

    #[tokio::test]
    async fn loaded_time_is_exact() {
        let (m, s) = meter().await;
        m.on_loaded("app", 10_000, None);
        m.count_request("app");
        m.count_requests("app", 2);
        assert_eq!(m.sample(70_500).await.unwrap(), vec![("app".to_string(), true)]);
        // Loaded and refreshed already: listed, not forced.
        assert_eq!(m.sample(70_600).await.unwrap(), vec![("app".to_string(), false)]);
        // 60 whole seconds now; the 500 ms carry over.
        let t = s.usage_since("srch_a", 0, 100).await.unwrap();
        assert_eq!((t.loaded_seconds, t.requests), (60, 3));
        // Unloading rounds the remainder up: 70.0 s → 71.7 s is 1.7 s → 2 s.
        m.on_unloading("app", 71_700, Some(4), None).await.unwrap();
        let t = s.usage_since("srch_a", 0, 100).await.unwrap();
        assert_eq!(t.loaded_seconds, 62);
        assert!(!m.is_loaded("app"));
        assert_eq!(m.take_indices("app"), Some(4));
        // Asleep: no rows; the refresh still comes once.
        assert_eq!(m.sample(200_000).await.unwrap(), vec![("app".to_string(), true)]);
        assert!(m.sample(260_000).await.unwrap().is_empty());
        assert_eq!(s.usage_since("srch_a", 0, 300).await.unwrap().loaded_seconds, 62);
        // Not measurable (no instance): base + stored bytes.
        assert_eq!(s.usage_since("srch_a", 0, 300).await.unwrap().peak_loaded_bytes, LOADED_BASE_BYTES + 1000);
    }

    #[tokio::test]
    async fn measured_sizes_bill_the_larger_end() {
        let (m, s) = meter().await;
        m.on_loaded("app", 0, None);
        // Measured only at unload: the whole stay at that size.
        m.on_unloading("app", 10_000, None, Some(5 << 20)).await.unwrap();
        let t = s.usage_since("srch_a", 0, 100).await.unwrap();
        assert_eq!((t.loaded_seconds, t.peak_loaded_bytes), (10, 5 << 20));
        assert!((t.loaded_gib_hours - 10.0 * (5 << 20) as f64 / (1u64 << 30) as f64 / 3600.0).abs() < 1e-12);
        // Request-only rows carry no size, so they don't move the peak.
        m.count_requests("app", 3);
        m.sample(20_000).await.unwrap();
        let t = s.usage_since("srch_a", 0, 100).await.unwrap();
        assert_eq!((t.requests, t.peak_loaded_bytes, t.loaded_seconds), (3, 5 << 20, 10));
        assert_eq!(larger(Some(1), Some(3)), Some(3));
        assert_eq!(larger(None, Some(3)), Some(3));
        assert_eq!(larger(Some(2), None), Some(2));
        assert_eq!(larger(None, None), None);
    }

    /// Runs one REST request against a cluster.
    async fn rest(c: &Cluster, method: &str, uri: &str, body: &str) -> u16 {
        let mut r = search_es::EsRequest::new(method, uri);
        if !body.is_empty() {
            let nd = uri.contains("_bulk");
            r.headers.push(("Content-Type".into(), if nd { "application/x-ndjson" } else { "application/json" }.into()));
            r.body = bytes::Bytes::from(body.to_string());
        }
        search_es::handle(c, r).await.status
    }

    #[tokio::test]
    async fn engine_figures_and_resident_size() {
        let c = Arc::new(Cluster::in_memory("app").await);
        let empty = engine_figures(&c);
        assert_eq!(empty, EngineFigures::default());
        assert!(empty.complete());
        assert_eq!(resident_bytes(&c), LOADED_BASE_BYTES);
        assert_eq!(rest(&c, "PUT", "/a", "").await, 200);
        assert_eq!(rest(&c, "PUT", "/b", "").await, 200);
        // Created, not used yet: its shards aren't open.
        let f = engine_figures(&c);
        assert_eq!((f.indices, f.open_indices, f.complete()), (2, 0, false));
        let bulk = "{\"index\":{\"_index\":\"a\",\"_id\":\"1\"}}\n{\"t\":\"x\"}\n{\"index\":{\"_index\":\"a\",\"_id\":\"2\"}}\n{\"t\":\"y\",\"n\":[{\"k\":1}]}\n{\"index\":{\"_index\":\"b\",\"_id\":\"1\"}}\n{\"t\":\"z\"}\n";
        let status = rest(&c, "POST", "/_bulk?refresh=true", bulk).await;
        if status == 501 {
            // The document APIs aren't wired in this build of search-es.
            return;
        }
        assert_eq!(status, 200);
        let f = engine_figures(&c);
        assert_eq!((f.indices, f.open_indices, f.docs, f.deleted_docs), (2, 2, 3, 0), "{f:?}");
        assert!(f.complete());
        assert!(f.heap_bytes > 0 && f.segment_bytes > 0, "{f:?}");
        // Measuring is free of side effects: the same figure twice, and the figures' heap.
        assert_eq!(resident_bytes(&c), LOADED_BASE_BYTES + f.heap_bytes as i64);
        assert_eq!(resident_bytes(&c), resident_bytes(&c));
        let uuid = c.state().metadata.indices["a"].uuid.clone();
        assert_eq!(index_figures(&c, &uuid).map(|f| f.docs), Some(2));
        assert_eq!(index_figures(&c, "nope"), None);
        // The meter measures a cluster it was given.
        let (m, s) = meter().await;
        m.on_loaded("app", 0, Some(&c));
        m.sample(60_000).await.unwrap();
        let t = s.usage_since("srch_a", 0, 100).await.unwrap();
        assert_eq!((t.loaded_seconds, t.peak_loaded_bytes), (60, resident_bytes(&c)));
    }

    /// Elasticsearch 9.5.5, an index with a nested field and one document holding two nested
    /// objects: `_cat/indices` docs.count = 3 and `_stats` primaries.docs.count = 3 (`_count` = 1).
    #[tokio::test]
    async fn nested_documents_count_like_stats() {
        let c = Arc::new(Cluster::in_memory("app").await);
        assert_eq!(rest(&c, "PUT", "/n", r#"{"mappings":{"properties":{"items":{"type":"nested"}}}}"#).await, 200);
        let status = rest(&c, "PUT", "/n/_doc/1?refresh=true", r#"{"title":"x","items":[{"k":1},{"k":2}]}"#).await;
        if status == 501 {
            return;
        }
        assert_eq!(status, 201);
        let uuid = c.state().metadata.indices["n"].uuid.clone();
        let f = index_figures(&c, &uuid).unwrap();
        assert_eq!((f.docs, f.deleted_docs), (3, 0), "{f:?}");
        assert_eq!(engine_figures(&c).docs, 3);
    }

    #[tokio::test]
    async fn requests_after_unload_are_kept() {
        let (m, s) = meter().await;
        m.count_request("app");
        m.sample(1_000).await.unwrap();
        assert_eq!(s.usage_since("srch_a", 0, 10).await.unwrap().requests, 1);
        // Unknown clusters (deleted) are ignored.
        m.count_request("gone");
        m.sample(2_000).await.unwrap();
    }
}

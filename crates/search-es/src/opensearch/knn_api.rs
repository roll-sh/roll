//! The k-NN plugin's node APIs: `GET _plugins/_knn/stats`, `GET _plugins/_knn/warmup/{index}`
//! and `POST _plugins/_knn/clear_cache/{index}`, and the bookkeeping behind them
//! ([`KnnRuntime`], one per cluster).
//!
//! OpenSearch keeps the native (faiss) graph of every segment's vector field in an off-heap
//! cache: a search loads the graphs it touches (a miss) or finds them loaded (a hit), warmup
//! loads every graph of an index, clear_cache evicts them. roll's vector indexes live with
//! their segments and need no loading, so the cache here is a ledger with the same semantics:
//! entries are (index, shard, segment, field) with the graph's estimated size, so the stats
//! report what OpenSearch would hold for the same searches. Lucene-engine fields never enter
//! it (OpenSearch searches them through Lucene's own HNSW).
//!
//! Counters follow the plugin's `KNNCounter`s: one `knn_query_requests` per shard a top-`k`
//! query is built on (`with_filter` when it has a filter), `min_score` / `max_distance` for
//! radial queries instead, one `graph_query_requests` per native graph searched, and the
//! `knn_score` script's compilations and queries.
//!
//! The stats response mirrors `KNNStatsResponse`: `_nodes`, `cluster_name`, the cluster-level
//! stats, then `nodes.{id}` with the node stats in the iteration order of the `HashMap` the
//! plugin fills (inserted in `KNNStats`' registration order).

use crate::api::resolve_indices;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::IndexMeta;
use crate::meta::resolve::IndicesOptions;
use crate::obj;
use crate::rest::{Ctx, Reply};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The plugin's request counters (`KNNCounter`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    KnnQuery,
    KnnQueryWithFilter,
    MinScoreQuery,
    MinScoreQueryWithFilter,
    MaxDistanceQuery,
    MaxDistanceQueryWithFilter,
    GraphQueryErrors,
    GraphQuery,
    GraphIndexErrors,
    GraphIndex,
    ScriptCompilations,
    ScriptCompilationErrors,
    ScriptQuery,
    ScriptQueryErrors,
    TrainingRequests,
    TrainingErrors,
}

const COUNTERS: usize = 16;

/// One cached native graph: (index uuid, shard, segment uid, field number).
type GraphKey = (u32, u64, u32);

#[derive(Default)]
struct IndexGraphs {
    name: String,
    graphs: HashMap<GraphKey, u64>,
}

#[derive(Default)]
struct Cache {
    indices: HashMap<String, IndexGraphs>,
    hits: u64,
    misses: u64,
    loads: u64,
    load_nanos: u64,
}

/// The k-NN plugin's per-cluster state: counters and the graph-cache ledger.
#[derive(Default)]
pub struct KnnRuntime {
    counters: [AtomicU64; COUNTERS],
    compiled: std::sync::atomic::AtomicBool,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for KnnRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnnRuntime").finish_non_exhaustive()
    }
}

/// `knn.memory.circuit_breaker.limit`'s default share, as the denominator of the usage
/// percentages: half of the node's reported heap.
fn cache_limit_kb(ctx: &Ctx) -> f64 {
    let heap = crate::api::nodes::NodeMetrics::sample(ctx.cluster).heap_max as f64;
    (heap / 2.0 / 1024.0).max(1.0)
}

impl KnnRuntime {
    /// Counts the `knn_score` script's compilation the first time it is used.
    pub fn compile_once(&self) {
        if !self.compiled.swap(true, Ordering::Relaxed) {
            self.bump(Counter::ScriptCompilations);
        }
    }

    pub fn bump(&self, c: Counter) {
        self.counters[c as usize].fetch_add(1, Ordering::Relaxed);
    }

    fn get(&self, c: Counter) -> u64 {
        self.counters[c as usize].load(Ordering::Relaxed)
    }

    /// Records a search (or warmup) touching a segment's native graph of `bytes`.
    pub fn touch(&self, index_uuid: &str, index_name: &str, key: GraphKey, bytes: u64) {
        let started = std::time::Instant::now();
        let mut c = self.cache.lock();
        let entry = c.indices.entry(index_uuid.to_string()).or_default();
        if entry.name.is_empty() {
            entry.name = index_name.to_string();
        }
        match entry.graphs.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => c.hits += 1,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(bytes);
                c.misses += 1;
                c.loads += 1;
                c.load_nanos += started.elapsed().as_nanos() as u64;
            }
        }
    }

    /// Drops an index's graphs (`clear_cache`, or the index went away).
    pub fn evict(&self, index_uuid: &str) {
        self.cache.lock().indices.remove(index_uuid);
    }

    /// Forgets graphs of indices that no longer exist and of segments merged away.
    fn prune(&self, ctx: &Ctx) {
        let st = ctx.state();
        let live: HashMap<&str, &Arc<IndexMeta>> = st.metadata.indices.values().map(|i| (i.uuid.as_str(), i)).collect();
        let mut c = self.cache.lock();
        c.indices.retain(|uuid, _| live.get(uuid.as_str()).is_some_and(|i| !i.is_closed()));
        for (uuid, graphs) in c.indices.iter_mut() {
            let Some(engine) = ctx.cluster.engine.loaded(uuid) else {
                graphs.graphs.clear();
                continue;
            };
            let segments: Vec<Vec<u64>> = engine.shards.iter().map(|s| s.shard.searcher().segments().iter().map(|g| g.core.uid).collect()).collect();
            graphs.graphs.retain(|(shard, seg, _), _| segments.get(*shard as usize).is_some_and(|s| s.contains(seg)));
        }
        c.indices.retain(|_, g| !g.graphs.is_empty());
    }
}

/// A native graph's estimated memory: its vectors plus `2m` level-0 neighbour ids each.
pub fn graph_bytes(vectors: u32, vector_bytes: usize, m: u16) -> u64 {
    u64::from(vectors) * (vector_bytes as u64 + 8 * u64::from(m))
}

/// The plugin's model system index.
pub const MODEL_INDEX: &str = ".opensearch-knn-models";

/// The node-level stat names in `KNNStats.buildStatsMap` order.
const NODE_STATS: &[&str] = &[
    "knn_query_requests",
    "knn_query_with_filter_requests",
    "min_score_query_requests",
    "min_score_query_with_filter_requests",
    "max_distance_query_requests",
    "max_distance_query_with_filter_requests",
    "hit_count",
    "miss_count",
    "load_success_count",
    "load_exception_count",
    "total_load_time",
    "eviction_count",
    "graph_memory_usage",
    "graph_memory_usage_percentage",
    "indices_in_cache",
    "cache_capacity_reached",
    "graph_query_errors",
    "graph_query_requests",
    "graph_index_errors",
    "graph_index_requests",
    "faiss_initialized",
    "nmslib_initialized",
    "lucene_initialized",
    "script_compilations",
    "script_compilation_errors",
    "script_query_requests",
    "script_query_errors",
    "indexing_from_model_degraded",
    "training_requests",
    "training_errors",
    "training_memory_usage",
    "training_memory_usage_percentage",
    "graph_stats",
    "remote_vector_index_build_stats",
];

/// The cluster-level stats.
const CLUSTER_STATS: &[&str] = &["circuit_breaker_triggered", "model_index_status"];

fn hash_map(entries: Vec<(&str, Value)>) -> Value {
    let mut m = Map::with_capacity(entries.len());
    for (k, v) in entries {
        m.insert(k.to_string(), v);
    }
    Value::Obj(crate::json::java_hash_map(m, 16))
}

fn zeros(keys: &[&'static str]) -> Value {
    hash_map(keys.iter().map(|k| (*k, Value::from(0))).collect())
}

fn node_stat(ctx: &Ctx, rt: &KnnRuntime, name: &str) -> Value {
    use Counter::*;
    let c = |x| Value::from(rt.get(x));
    let limit_kb = cache_limit_kb(ctx);
    match name {
        "knn_query_requests" => c(KnnQuery),
        "knn_query_with_filter_requests" => c(KnnQueryWithFilter),
        "min_score_query_requests" => c(MinScoreQuery),
        "min_score_query_with_filter_requests" => c(MinScoreQueryWithFilter),
        "max_distance_query_requests" => c(MaxDistanceQuery),
        "max_distance_query_with_filter_requests" => c(MaxDistanceQueryWithFilter),
        "graph_query_errors" => c(GraphQueryErrors),
        "graph_query_requests" => c(GraphQuery),
        "graph_index_errors" => c(GraphIndexErrors),
        "graph_index_requests" => c(GraphIndex),
        "script_compilations" => c(ScriptCompilations),
        "script_compilation_errors" => c(ScriptCompilationErrors),
        "script_query_requests" => c(ScriptQuery),
        "script_query_errors" => c(ScriptQueryErrors),
        "training_requests" => c(TrainingRequests),
        "training_errors" => c(TrainingErrors),
        "hit_count" => rt.cache.lock().hits.into(),
        "miss_count" => rt.cache.lock().misses.into(),
        "load_success_count" => rt.cache.lock().loads.into(),
        "total_load_time" => rt.cache.lock().load_nanos.into(),
        "load_exception_count" | "eviction_count" | "training_memory_usage" => Value::from(0),
        "graph_memory_usage" => {
            let kb: u64 = rt.cache.lock().indices.values().map(|i| i.graphs.values().map(|b| b / 1024).sum::<u64>()).sum();
            kb.into()
        }
        "graph_memory_usage_percentage" => {
            let kb: u64 = rt.cache.lock().indices.values().map(|i| i.graphs.values().map(|b| b / 1024).sum::<u64>()).sum();
            Value::from(f64::from(kb as f32 / limit_kb as f32 * 100.0))
        }
        "training_memory_usage_percentage" => Value::from(0.0),
        "indices_in_cache" => {
            let c = rt.cache.lock();
            let mut named: Vec<(&str, &IndexGraphs)> = c.indices.values().map(|g| (g.name.as_str(), g)).collect();
            named.sort_by(|a, b| a.0.cmp(b.0));
            let mut m = Map::new();
            for (name, g) in named {
                let kb: u64 = g.graphs.values().map(|b| b / 1024).sum();
                m.insert(
                    name.to_string(),
                    hash_map(vec![
                        ("graph_memory_usage", kb.into()),
                        ("graph_memory_usage_percentage", Value::from(f64::from(kb as f32 / limit_kb as f32 * 100.0))),
                        ("graph_count", g.graphs.len().into()),
                    ]),
                );
            }
            Value::Obj(crate::json::java_hash_map(m, 16))
        }
        "cache_capacity_reached" | "indexing_from_model_degraded" | "nmslib_initialized" => Value::Bool(false),
        "faiss_initialized" | "lucene_initialized" => Value::Bool(true),
        "graph_stats" => hash_map(vec![
            ("merge", zeros(&["current", "current_docs", "current_size_in_bytes", "total", "total_time_in_millis", "total_docs", "total_size_in_bytes"])),
            ("refresh", zeros(&["total", "total_time_in_millis"])),
        ]),
        "remote_vector_index_build_stats" => hash_map(vec![
            (
                "build_stats",
                zeros(&[
                    "remote_index_build_current_flush_operations",
                    "remote_index_build_current_merge_operations",
                    "remote_index_build_current_flush_size",
                    "remote_index_build_current_merge_size",
                    "remote_index_build_flush_time_in_millis",
                    "remote_index_build_merge_time_in_millis",
                ]),
            ),
            (
                "client_stats",
                zeros(&[
                    "build_request_success_count",
                    "build_request_failure_count",
                    "status_request_success_count",
                    "status_request_failure_count",
                    "index_build_success_count",
                    "index_build_failure_count",
                    "index_build_merge_abort_exception",
                    "index_build_terminal_exception",
                    "waiting_time_in_ms",
                ]),
            ),
            (
                "repository_stats",
                zeros(&[
                    "write_success_count",
                    "write_failure_count",
                    "successful_write_time_in_millis",
                    "read_success_count",
                    "read_failure_count",
                    "successful_read_time_in_millis",
                ]),
            ),
        ]),
        _ => Value::Null,
    }
}

/// `GET _plugins/_knn/stats` (`RestKNNStatsHandler`).
pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let rt = &ctx.cluster.knn;
    let path = ctx.path.clone();
    let wanted: Option<Vec<String>> = match ctx.param("stat").filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => {
            let set: std::collections::BTreeSet<String> = s.split(',').map(str::to_string).collect();
            if set.len() == 1 && set.contains("_all") {
                None
            } else if set.contains("_all") {
                return Err(EsError::illegal_argument(format!("Request {path} contains _all and individual stats")));
            } else {
                let known = |s: &String| NODE_STATS.contains(&s.as_str()) || CLUSTER_STATS.contains(&s.as_str());
                let (valid, invalid): (std::collections::BTreeSet<String>, std::collections::BTreeSet<String>) = set.iter().cloned().partition(known);
                if !invalid.is_empty() {
                    // The suggestions come from the request's valid stats only.
                    return Err(EsError::illegal_argument(crate::rest::params::unrecognized(&path, &invalid, &valid, "stat")));
                }
                Some(set.into_iter().collect())
            }
        }
    };
    ctx.time_param("timeout", crate::units::TimeValue::ZERO)?;
    let wants = |name: &str| wanted.as_ref().is_none_or(|w| w.iter().any(|x| x == name));
    let selected = match ctx.param("node_id").filter(|s| !s.is_empty()) {
        Some(n) => crate::api::nodes::selects_node(ctx, &n.split(',').map(str::to_string).collect::<Vec<_>>()),
        None => true,
    };
    rt.prune(ctx);
    let n = i64::from(selected);
    let mut out = Map::new();
    out.insert("_nodes".into(), obj! { "total" => n, "successful" => n, "failed" => 0 });
    out.insert("cluster_name".into(), ctx.cluster.name.clone().into());
    let mut cluster = Vec::new();
    for name in CLUSTER_STATS.iter().filter(|n| wants(n)) {
        let v = match *name {
            "circuit_breaker_triggered" => Value::Bool(false),
            _ => model_index_status(ctx),
        };
        cluster.push((*name, v));
    }
    if let Value::Obj(m) = hash_map(cluster) {
        out.extend(m);
    }
    let mut nodes = Map::new();
    if selected {
        let stats: Vec<(&str, Value)> = NODE_STATS.iter().filter(|n| wants(n)).map(|n| (*n, node_stat(ctx, rt, n))).collect();
        nodes.insert(ctx.cluster.node.id.clone(), hash_map(stats));
    }
    out.insert("nodes".into(), Value::Obj(nodes));
    Reply::ok(Value::Obj(out))
}

/// `model_index_status`: the health of `.opensearch-knn-models` (`ClusterHealthStatus` names),
/// `null` without it.
fn model_index_status(ctx: &Ctx) -> Value {
    let st = ctx.state();
    match st.metadata.indices.get(MODEL_INDEX) {
        Some(idx) => Value::from(if idx.number_of_replicas() == 0 { "GREEN" } else { "YELLOW" }),
        None => Value::Null,
    }
}

/// The open indices `{index}` names, refused unless every one has `index.knn`.
fn knn_targets(ctx: &Ctx, action: &str) -> EsResult<Vec<Arc<IndexMeta>>> {
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?;
    let indices: Vec<Arc<IndexMeta>> = names.iter().filter_map(|n| st.metadata.indices.get(n)).filter(|i| !i.is_closed()).cloned().collect();
    if indices.iter().any(|i| !i.mapping_ctx().knn) {
        return Err(EsError::java(500, "k_n_n_invalid_indices_exception", format!("{action} request rejected. One or more indices have 'index.knn' set to false.")));
    }
    Ok(indices)
}

/// `_shards` of the plugin's broadcast-by-node actions on one node: every copy counts, the
/// primaries answer.
fn shards_header(indices: &[Arc<IndexMeta>]) -> Value {
    let (mut total, mut ok) = (0i64, 0i64);
    for idx in indices {
        let s = i64::from(idx.number_of_shards());
        total += s * (1 + i64::from(idx.number_of_replicas()));
        ok += s;
    }
    obj! { "_shards" => obj! { "total" => total, "successful" => ok, "failed" => 0 } }
}

/// `GET _plugins/_knn/warmup/{index}`: loads every native graph of the indices.
pub fn warmup(ctx: &Ctx) -> EsResult<Reply> {
    let indices = knn_targets(ctx, "Warm up")?;
    let st = ctx.state();
    for idx in &indices {
        let engine = ctx.cluster.engine.index(ctx.cluster, &st, idx)?;
        let table = engine.table();
        let mctx = idx.mapping_ctx();
        let fields: Vec<(u32, u16)> = table
            .fields
            .values()
            .filter(|f| f.ty == "knn_vector")
            .filter_map(|f| {
                let r = super::knn::resolve(&f.mapper, &mctx);
                (r.ann && r.engine != super::knn::Engine::Lucene).then_some((f.number, r.m))
            })
            .collect();
        for (shard, se) in engine.shards.iter().enumerate() {
            let searcher = se.shard.searcher();
            for seg in searcher.segments() {
                for &(field, m) in &fields {
                    let Some(vr) = seg.reader.vectors(field) else { continue };
                    if vr.is_empty() {
                        continue;
                    }
                    ctx.cluster.knn.touch(&idx.uuid, &idx.name, (shard as u32, seg.core.uid, field), graph_bytes(vr.len(), vr.spec().vector_bytes(), m));
                }
            }
        }
    }
    Reply::ok(shards_header(&indices))
}

/// `POST _plugins/_knn/clear_cache/{index}`: evicts the indices' native graphs.
pub fn clear_cache(ctx: &Ctx) -> EsResult<Reply> {
    let indices = knn_targets(ctx, "ClearCache")?;
    for idx in &indices {
        ctx.cluster.knn.evict(&idx.uuid);
    }
    Reply::ok(shards_header(&indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_stats_print_in_the_plugins_hash_order() {
        // Captured from OpenSearch 3.9.0 (`GET _plugins/_knn/stats`).
        let Value::Obj(m) = zeros(NODE_STATS) else { unreachable!() };
        let keys: Vec<&str> = m.keys().map(String::as_str).collect();
        assert_eq!(
            &keys[..12],
            &[
                "remote_vector_index_build_stats",
                "max_distance_query_with_filter_requests",
                "graph_memory_usage_percentage",
                "graph_query_requests",
                "graph_memory_usage",
                "cache_capacity_reached",
                "load_success_count",
                "training_memory_usage",
                "indices_in_cache",
                "script_query_errors",
                "hit_count",
                "knn_query_requests",
            ]
        );
        assert_eq!(keys.last(), Some(&"script_compilation_errors"));
        // A filtered request's smaller map orders differently.
        let Value::Obj(m) = zeros(&["graph_memory_usage", "graph_memory_usage_percentage", "indices_in_cache", "cache_capacity_reached"]) else { unreachable!() };
        let keys: Vec<&str> = m.keys().map(String::as_str).collect();
        assert_eq!(keys, ["indices_in_cache", "graph_memory_usage_percentage", "graph_memory_usage", "cache_capacity_reached"]);
    }

    #[test]
    fn ledger_counts_hits_and_misses() {
        let rt = KnnRuntime::default();
        rt.touch("u", "i", (0, 1, 2), 4096);
        rt.touch("u", "i", (0, 1, 2), 4096);
        rt.touch("u", "i", (1, 1, 2), 2048);
        let c = rt.cache.lock();
        assert_eq!((c.hits, c.misses, c.loads), (1, 2, 2));
        assert_eq!(c.indices["u"].graphs.len(), 2);
        drop(c);
        rt.evict("u");
        assert!(rt.cache.lock().indices.is_empty());
        assert_eq!(graph_bytes(10, 16, 16), 10 * (16 + 128));
    }
}

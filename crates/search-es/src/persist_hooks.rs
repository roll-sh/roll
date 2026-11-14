//! Hooks search-server's shard persistence needs from the engine (additive; owned by
//! search-server).
//!
//! The module is declared inside `engine::fields` (`#[path]`), so it can read and seed an
//! index's [`Numbers`]: field numbers are allocated as a mapping grows, in the order fields are
//! first seen, and segments record fields by number. A fresh [`Engine::index`] walks the final
//! mapping and would number fields differently than the segments a woken index reopens, so
//! search-server persists each index's numbering with its commits and reopens the index with it
//! ([`Engine::open_seeded`]) before any request can create it the default way.
//!
//! Recovery replays the shard's operation log ([`crate::engine::LoggedOp`], written through
//! [`crate::engine::ShardLog`] by `write::apply_index` / `apply_delete`) with
//! [`Engine::replay`]: each operation is parsed again against the index's current mapping
//! (mappings only grow, and every mapping update is durable before the operation that needed
//! it is applied) and applied with its original seq_no, term and version, as Elasticsearch
//! replays its translog (`Origin::Replay`: stale and duplicate operations are skipped, any
//! order works).
//!
//! Async results (`xpack::async_results`) live in memory while the cluster is loaded; a durable
//! server also writes the changed ones to the `.async-search` system index
//! ([`crate::cluster::Cluster::sync_async_results`], before acknowledging the request that
//! changed them) and reads them back at wake ([`crate::cluster::Cluster::restore_async_results`]),
//! so they survive an idle unload or a crash as Elasticsearch's survive a restart.
//!
//! Trained model deployments run in memory too; their assignments are cluster state on
//! Elasticsearch, restarted after a node restart. A durable server persists them the same way
//! ([`crate::cluster::Cluster::sync_ml_deployments`] after a request that changed them) and
//! starts them again at wake ([`crate::cluster::Cluster::restore_ml_deployments`]).
//!
//! A wake is no restart, so what a running Elasticsearch node holds in memory carries over too:
//! the shard statistics ([`IndexEngine::persisted_stats`]: counters, request cache keys rebuilt
//! as [`WarmCacheEntry`]s, the segment order the shard opened with, the translog figures at an
//! unload) and the synonym rules an analysis chain loaded
//! ([`crate::cluster::Cluster::loaded_synonyms_json`]). Durable snapshot contents are read back
//! through [`Engine::snapshot_searchers`].

use super::{FieldTable, Numbers};
use crate::engine::convert::{self, DocMeta};
use crate::engine::{Engine, IndexEngine, LoggedOp, ShardCounters, ShardEngine, epoch_ms, similarities};
use crate::error::{EsError, EsResult};
use bytes::Bytes;
use search_index::{DeleteOp, IndexOp, Origin};
use crate::meta::{ClusterState, IndexMeta};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

impl Numbers {
    /// Every allocated `(key, number)`, by number. Keys are the allocation keys (field paths,
    /// with a suffix for per-layout vector fields), not necessarily schema names.
    pub fn snapshot(&self) -> Vec<(String, u32)> {
        let g = self.inner.lock();
        let mut v: Vec<(String, u32)> = g.0.iter().map(|(k, n)| (k.clone(), *n)).collect();
        v.sort_by_key(|(_, n)| *n);
        v
    }

    /// How many numbers are allocated (grows only).
    pub fn allocated(&self) -> usize {
        self.inner.lock().0.len()
    }

    /// Numbers continuing an earlier allocation: the same keys keep their numbers, and new keys
    /// get numbers past every earlier one.
    pub fn seeded(entries: &[(String, u32)]) -> Numbers {
        let map: HashMap<String, u32> = entries.iter().cloned().collect();
        let next = entries.iter().map(|(_, n)| n + 1).max().unwrap_or(0);
        Numbers { inner: Mutex::new((map, next)) }
    }
}

impl IndexEngine {
    /// The index's field numbering (to persist with a commit).
    pub fn field_numbers(&self) -> Vec<(String, u32)> {
        self.numbers.snapshot()
    }

    /// How many field numbers the index has allocated (changes whenever the numbering grows).
    pub fn field_numbers_allocated(&self) -> usize {
        self.numbers.allocated()
    }
}

impl Engine {
    /// Every loaded index engine.
    pub fn indices_snapshot(&self) -> Vec<Arc<IndexEngine>> {
        self.indices.read().values().cloned().collect()
    }

    /// Opens the engine of `idx` like [`Engine::index`] does on first use, but with field
    /// numbers continuing `numbers` (a persisted [`IndexEngine::field_numbers`]). Returns the
    /// existing engine if the index is already open.
    pub fn open_seeded(&self, cluster: &crate::Cluster, st: &ClusterState, idx: &IndexMeta, numbers: &[(String, u32)]) -> EsResult<Arc<IndexEngine>> {
        let mut g = self.indices.write();
        if let Some(ie) = g.get(&idx.uuid) {
            return Ok(ie.clone());
        }
        let analysis = Arc::new(cluster.index_analysis(st, idx)?);
        let numbers = Numbers::seeded(numbers);
        let ctx = idx.mapping_ctx();
        let mut table = FieldTable::build(idx.mapping.clone(), analysis, &ctx, &numbers, &similarities(idx))?;
        table.key = crate::engine::table_key(st, idx);
        // Segments written after the wake must be sorted like the ones before it.
        table.schema = table.schema.clone().with_index_sort(crate::engine::index_sort(idx, &table));
        table.stripped_vectors = crate::engine::source_vectors::stripped(idx, &table);
        let mut shards = Vec::new();
        for i in 0..idx.number_of_shards() {
            let shard = self.storage.open(&idx.uuid, i, table.schema.clone())?;
            shards.push(Arc::new(ShardEngine {
                id: i,
                shard,
                counters: ShardCounters::default(),
                dirty_since: AtomicI64::new(0),
                last_search: AtomicI64::new(epoch_ms()),
                request_cache: Default::default(),
                log: self.storage.log(&idx.uuid, i),
            }));
        }
        crate::engine::apply_shard_settings(idx, &shards);
        let ie = Arc::new(IndexEngine::assemble(idx.uuid.clone(), idx.name.clone(), shards, numbers, table));
        g.insert(idx.uuid.clone(), ie.clone());
        Ok(ie)
    }

    /// Read-only searchers over a durable snapshot's copy of an index ([`SnapshotStorage::load`]),
    /// for restoring it into `idx` (the restored index: the snapshot's mapping). Each shard's
    /// segments are read with the field numbering they were written with.
    ///
    /// [`SnapshotStorage::load`]: crate::engine::SnapshotStorage::load
    pub fn snapshot_searchers(&self, cluster: &crate::Cluster, st: &ClusterState, idx: &IndexMeta, shards: Vec<crate::engine::SnapshotShard>) -> EsResult<Vec<search_index::Searcher>> {
        let analysis = Arc::new(cluster.index_analysis(st, idx)?);
        let ctx = idx.mapping_ctx();
        let mut out = Vec::with_capacity(shards.len());
        for s in shards {
            if s.segments.is_empty() {
                continue;
            }
            let numbers = Numbers::seeded(&s.numbers);
            let table = FieldTable::build(idx.mapping.clone(), analysis.clone(), &ctx, &numbers, &similarities(idx))?;
            let schema = table.schema.clone().with_index_sort(crate::engine::index_sort(idx, &table));
            // Nothing is written: no merges, no directory.
            let cfg = search_index::ShardConfig { merge_threads: 0, ..Default::default() };
            let shard = search_index::Shard::open(cfg, schema, s.segments).map_err(convert::index_error)?;
            out.push(shard.searcher());
        }
        Ok(out)
    }

    /// The vector fields of `idx` a copy of its documents puts back into their source
    /// ([`crate::engine::source_vectors::copied`]), from its mapping: for searchers that aren't
    /// the index's own (a closed index's snapshot copy).
    pub fn copied_vectors(&self, cluster: &crate::Cluster, st: &ClusterState, idx: &IndexMeta) -> EsResult<Vec<Arc<crate::engine::fields::FieldInfo>>> {
        let analysis = Arc::new(cluster.index_analysis(st, idx)?);
        let table = FieldTable::build(idx.mapping.clone(), analysis, &idx.mapping_ctx(), &Numbers::default(), &similarities(idx))?;
        Ok(crate::engine::source_vectors::copied(&table))
    }

    /// Applies one logged operation to shard `shard` of `idx` again (recovery). The index must
    /// be open ([`Engine::open_seeded`]). Stale and duplicate operations are no-ops.
    pub fn replay(&self, scripts: &crate::scripting::ScriptService, st: &crate::meta::ClusterState, idx: &IndexMeta, shard: u32, op: &LoggedOp<'_>) -> EsResult<()> {
        let Some(ie) = self.loaded(&idx.uuid) else {
            return Err(EsError::illegal_state(format!("replaying into index [{}] that isn't open", idx.name)));
        };
        if shard as usize >= ie.shards.len() {
            return Err(EsError::illegal_state(format!("replaying into shard [{}][{shard}] of an index with {} shards", idx.name, ie.shards.len())));
        }
        let se = ie.shard(shard);
        match *op {
            LoggedOp::Index { id, routing, source, seq_no, primary_term, version } => {
                let table = ie.table();
                let src = Bytes::copy_from_slice(source);
                let value = crate::fetch::parse_source(&src)?;
                let mut ctx = idx.mapping_ctx();
                ctx.join_routing = Some(routing.is_some());
                let mut parsed = crate::document::parse_document(&idx.mapping, &ctx, &value, Some(&src), Some(id))?;
                if parsed.dynamic_update.is_some() {
                    // The mapping update an operation needed is durable before the operation is
                    // applied, so this is a lost mapping: refuse rather than drop fields.
                    return Err(EsError::illegal_state(format!(
                        "replaying [{id}] (seq_no {seq_no}) into [{}][{shard}]: the mapping lacks fields the operation added",
                        idx.name
                    )));
                }
                crate::search::index_time::apply(scripts, st, &idx.mapping, &value, &mut parsed)?;
                let stored = crate::write::stored_source(idx, &table, &src, &value);
                let doc = if crate::engine::tsdb::is_time_series(idx) {
                    let (gen_id, tsid, _) = crate::write::time_series_id(idx, &parsed, id, routing)?;
                    let meta = DocMeta { id: &gen_id, routing: None, source: stored, tsid: Some((&tsid, routing.unwrap_or(""))) };
                    convert::to_indexable(&table, &parsed, &meta)?
                } else {
                    convert::to_indexable(&table, &parsed, &DocMeta { id, routing, source: stored, tsid: None })?
                };
                let mut op = IndexOp::new(doc);
                op.origin = Origin::Replay { seq_no, primary_term, version };
                se.shard.index(op).map_err(convert::index_error)?;
            }
            LoggedOp::Delete { id, seq_no, primary_term, version } => {
                let mut op = DeleteOp::new(Bytes::copy_from_slice(id.as_bytes()));
                op.origin = Origin::Replay { seq_no, primary_term, version };
                se.shard.delete(op).map_err(convert::index_error)?;
            }
            LoggedOp::NoOp { seq_no, .. } => se.shard.mark_seq_no_processed(seq_no),
        }
        se.wrote();
        Ok(())
    }
}

// ------------------------------------------------------------------ async results

/// The system index async results are kept in (`AsyncTaskIndexService`): one document per
/// result, `_id` the result's id.
pub const ASYNC_RESULTS_INDEX: &str = ".async-search";

fn kind_name(k: crate::xpack::async_results::Kind) -> &'static str {
    use crate::xpack::async_results::Kind;
    match k {
        Kind::Search => "search",
        Kind::Esql => "esql",
        Kind::Sql => "sql",
        Kind::Eql => "eql",
        Kind::SqlCursor => "sql_cursor",
    }
}

fn kind_of(name: &str) -> Option<crate::xpack::async_results::Kind> {
    use crate::xpack::async_results::Kind;
    Some(match name {
        "search" => Kind::Search,
        "esql" => Kind::Esql,
        "sql" => Kind::Sql,
        "eql" => Kind::Eql,
        "sql_cursor" => Kind::SqlCursor,
        _ => return None,
    })
}

/// `.async-search`'s mapping and settings as Elasticsearch 9.5.5 creates them.
fn async_results_index_request() -> crate::api::indices::CreateRequest {
    use crate::json::Value;
    use crate::obj;
    let off = || obj! { "type" => "object", "enabled" => false };
    let mut req = crate::api::indices::CreateRequest::default();
    req.settings.set("index.number_of_shards", "1");
    req.settings.set("index.auto_expand_replicas", "0-1");
    req.settings.set("index.hidden", "true");
    if let Value::Obj(m) = obj! {
        "dynamic" => "strict",
        "properties" => obj! {
            "expiration_time" => obj! { "type" => "long" },
            "headers" => off(),
            "response_headers" => off(),
            "result" => off(),
        },
    } {
        req.mappings = Some(m);
    }
    req
}

fn async_result_doc(e: &crate::xpack::async_results::Entry) -> crate::json::Value {
    use crate::obj;
    obj! {
        "expiration_time" => e.expiration_ms,
        "result" => obj! {
            "kind" => kind_name(e.kind),
            "start_time_in_millis" => e.start_ms,
            "response" => e.response.clone(),
            "extra" => e.extra.clone(),
        },
    }
}

fn async_result_entry(doc: &crate::json::Value) -> Option<crate::xpack::async_results::Entry> {
    let r = doc.get("result")?;
    Some(crate::xpack::async_results::Entry {
        kind: kind_of(r.get("kind")?.as_str()?)?,
        response: r.get("response")?.clone(),
        start_ms: r.get("start_time_in_millis").and_then(|v| v.as_i64()).unwrap_or(0),
        expiration_ms: doc.get("expiration_time")?.as_i64()?,
        bytes: 0,
        extra: r.get("extra").cloned().unwrap_or(crate::json::Value::Null),
    })
}

/// The cumulative [`ShardCounters`] a durable shard carries across an unload, by stats key.
/// Memory figures (request cache bytes) and the translog describe the running instance and
/// start over.
const PERSISTED_COUNTERS: &[&str] = &[
    "index_total", "index_time_nanos", "index_failed", "delete_total", "delete_time_nanos", "noop_update_total",
    "get_total", "get_time_nanos", "get_exists", "get_missing", "query_total", "query_time_nanos", "fetch_total",
    "fetch_time_nanos", "scroll_total", "suggest_total", "flush_total", "refresh_external", "warmer_total",
    "request_cache_hit", "request_cache_miss", "request_cache_evictions",
];

impl ShardCounters {
    fn persisted_counter(&self, key: &str) -> Option<&std::sync::atomic::AtomicU64> {
        Some(match key {
            "index_total" => &self.index_total,
            "index_time_nanos" => &self.index_time_nanos,
            "index_failed" => &self.index_failed,
            "delete_total" => &self.delete_total,
            "delete_time_nanos" => &self.delete_time_nanos,
            "noop_update_total" => &self.noop_update_total,
            "get_total" => &self.get_total,
            "get_time_nanos" => &self.get_time_nanos,
            "get_exists" => &self.get_exists,
            "get_missing" => &self.get_missing,
            "query_total" => &self.query_total,
            "query_time_nanos" => &self.query_time_nanos,
            "fetch_total" => &self.fetch_total,
            "fetch_time_nanos" => &self.fetch_time_nanos,
            "scroll_total" => &self.scroll_total,
            "suggest_total" => &self.suggest_total,
            "flush_total" => &self.flush_total,
            "refresh_external" => &self.refresh_external,
            "warmer_total" => &self.warmer_total,
            "request_cache_hit" => &self.request_cache_hit,
            "request_cache_miss" => &self.request_cache_miss,
            "request_cache_evictions" => &self.request_cache_evictions,
            _ => return None,
        })
    }

    /// The cumulative statistics of the shard (indexing, get, search and its groups, request
    /// cache hits, field usage), for [`IndexEngine::persisted_stats`].
    fn persisted(&self) -> crate::json::Map {
        use crate::json::{Map, Value};
        use std::sync::atomic::Ordering::Relaxed;
        let mut m = Map::new();
        for key in PERSISTED_COUNTERS {
            let v = self.persisted_counter(key).map_or(0, |a| a.load(Relaxed));
            if v != 0 {
                m.insert((*key).into(), Value::from(v as i64));
            }
        }
        let groups = self.groups.lock();
        if !groups.is_empty() {
            m.insert("groups".into(), Value::Obj(groups.iter().map(|(k, (q, f))| (k.clone(), crate::arr![*q as i64, *f as i64])).collect()));
        }
        drop(groups);
        let usage = self.field_usage.lock();
        if let Some((id, started)) = usage.session() {
            let fields = usage
                .fields
                .iter()
                .map(|(k, c)| {
                    let v = [c.any, c.terms, c.postings, c.term_frequencies, c.positions, c.offsets, c.payloads, c.proximity, c.stored_fields, c.doc_values, c.points, c.norms, c.term_vectors, c.knn_vectors];
                    (k.clone(), Value::Arr(v.iter().map(|n| Value::from(*n as i64)).collect()))
                })
                .collect();
            m.insert("field_usage".into(), crate::obj! { "id" => id.clone(), "started" => *started, "fields" => Value::Obj(fields) });
        }
        m
    }

    /// Adds statistics [`ShardCounters::persisted`] wrote (a fresh shard at wake). Unknown keys
    /// are ignored.
    fn restore_persisted(&self, m: &crate::json::Map) {
        use crate::json::Value;
        use std::sync::atomic::Ordering::Relaxed;
        let n = |v: &Value| v.as_i64().unwrap_or(0).max(0) as u64;
        for (k, v) in m {
            if let Some(a) = self.persisted_counter(k) {
                a.fetch_add(n(v), Relaxed);
            }
        }
        if let Some(Value::Obj(g)) = m.get("groups") {
            let mut groups = self.groups.lock();
            for (name, v) in g {
                let e = groups.entry(name.clone()).or_default();
                e.0 += v.as_array().and_then(|a| a.first()).map_or(0, n);
                e.1 += v.as_array().and_then(|a| a.get(1)).map_or(0, n);
            }
        }
        if let Some(u) = m.get("field_usage")
            && let (Some(id), Some(started), Some(Value::Obj(fields))) = (u.get("id").and_then(Value::as_str), u.get("started").and_then(Value::as_i64), u.get("fields"))
        {
            let fields = fields
                .iter()
                .map(|(k, v)| {
                    let at = |i: usize| v.as_array().and_then(|a| a.get(i)).map_or(0, n);
                    let c = crate::search::field_usage::Counts {
                        any: at(0),
                        terms: at(1),
                        postings: at(2),
                        term_frequencies: at(3),
                        positions: at(4),
                        offsets: at(5),
                        payloads: at(6),
                        proximity: at(7),
                        stored_fields: at(8),
                        doc_values: at(9),
                        points: at(10),
                        norms: at(11),
                        term_vectors: at(12),
                        knn_vectors: at(13),
                    };
                    (k.clone(), c)
                })
                .collect();
            self.field_usage.lock().resume((id.to_string(), started), fields);
        }
    }
}

/// `indices.memory.shard_inactive_time` default: a shard without writes for this long is
/// flushed by Elasticsearch.
const SHARD_INACTIVE_MS: i64 = 5 * 60 * 1000;

/// Most bytes of request cache keys a shard carries across an unload.
const MAX_PERSISTED_CACHE_KEYS: usize = 256 << 10;

/// A request cache entry an earlier instance of the shard had (its key, kept across an unload):
/// the next lookup computes it again and counts the hit Elasticsearch, which never restarted,
/// would count.
pub struct WarmCacheEntry;

impl ShardEngine {
    /// Keys of the shard's request cache entries (warm ones too), up to
    /// [`MAX_PERSISTED_CACHE_KEYS`] bytes, sorted.
    pub fn request_cache_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.request_cache.lock().keys().cloned().collect();
        keys.sort_unstable();
        let mut total = 0;
        keys.retain(|k| {
            total += k.len();
            total <= MAX_PERSISTED_CACHE_KEYS
        });
        keys
    }

    /// Adds [`WarmCacheEntry`]s for `keys` (a fresh shard of a woken index).
    pub fn warm_request_cache<'a>(&self, keys: impl IntoIterator<Item = &'a str>) {
        let mut cache = self.request_cache.lock();
        for k in keys {
            if !cache.contains_key(k) {
                self.counters.request_cache_bytes.fetch_add(k.len() as u64 + 256, std::sync::atomic::Ordering::Relaxed);
                cache.insert(k.to_string(), Arc::new(WarmCacheEntry));
            }
        }
    }
}

impl IndexEngine {
    /// Shard `n`'s cumulative statistics (indexing, get, search and its groups, request cache
    /// hits, field usage) and the index's loaded field data, as JSON, for search-server to keep
    /// with the shard's commits so that a wake doesn't reset them (an Elasticsearch node that
    /// never restarted keeps counting, and keeps its field data loaded); `None` when there is
    /// nothing to keep. `ops`: the shard's flush and refresh counts (`Cluster::ops`), given by
    /// the commit of an unload, which also keeps the translog figures: that commit is no flush
    /// Elasticsearch would have made, so the operations stay in the translog stats after the
    /// wake (until the next real flush trims them).
    pub fn persisted_stats(&self, n: u32, ops: Option<crate::cluster::OpCounts>) -> Option<String> {
        use crate::json::Value;
        use std::sync::atomic::Ordering::Relaxed;
        let mut m = self.shard(n).counters.persisted();
        let c = &self.shard(n).counters;
        let translog = c.translog_ops.load(Relaxed);
        // Unless the shard has been inactive for `indices.memory.shard_inactive_time`, when
        // Elasticsearch flushes it (`IndexShard.flushOnIdle`) and the translog is trimmed anyway.
        let active = epoch_ms() - c.translog_modified_ms.load(Relaxed) < SHARD_INACTIVE_MS;
        if ops.is_some() && translog > 0 && active {
            m.insert("translog".into(), crate::arr![translog as i64, c.translog_bytes.load(Relaxed) as i64, c.translog_modified_ms.load(Relaxed)]);
        }
        if let Some(o) = ops.filter(|o| *o != crate::cluster::OpCounts::default()) {
            m.insert("ops".into(), crate::arr![o.flush as i64, o.refresh as i64, o.external_refresh as i64]);
        }
        let fielddata = self.fielddata_loaded.lock();
        if !fielddata.is_empty() {
            m.insert("fielddata".into(), Value::Arr(fielddata.iter().map(|f| Value::from(f.as_str())).collect()));
        }
        // Keys of the request cache (its entries are rebuilt on use: a wake is no restart).
        let keys = self.shard(n).request_cache_keys();
        if !keys.is_empty() {
            m.insert("request_cache".into(), Value::Arr(keys.into_iter().map(Value::from).collect()));
        }
        // The segment order the shard opened with, when reopening now would pick another (a
        // `@timestamp` field mapped since): a wake is no reopen.
        let leaf_sort = self.shard(n).shard.leaf_sort();
        if leaf_sort != crate::engine::leaf_sort_field(&self.table()) {
            m.insert("leaf_sort".into(), leaf_sort.map_or(Value::Null, |f| Value::from(f as i64)));
        }
        (!m.is_empty()).then(|| Value::Obj(m).to_json())
    }

    /// Adds what [`IndexEngine::persisted_stats`] wrote to shard `n` of a freshly opened index
    /// (flush and refresh counts into `ops`, of an index of `shards` shards).
    pub fn restore_stats(&self, n: u32, json: &str, ops: &crate::cluster::ShardOps, shards: usize) -> EsResult<()> {
        use crate::json::Value;
        let Some(Value::Obj(m)) = crate::json::parse::parse(json.as_bytes()).map_err(|e| EsError::internal(format!("persisted shard statistics: {e:?}")))? else {
            return Err(EsError::internal("persisted shard statistics are not an object"));
        };
        self.shard(n).counters.restore_persisted(&m);
        if let Some(Value::Arr(f)) = m.get("fielddata") {
            self.fielddata_loaded.lock().extend(f.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if let Some(Value::Arr(t)) = m.get("translog") {
            use std::sync::atomic::Ordering::Relaxed;
            let at = |i: usize| t.get(i).and_then(Value::as_i64).unwrap_or(0);
            let c = &self.shard(n).counters;
            c.translog_ops.fetch_add(at(0).max(0) as u64, Relaxed);
            c.translog_bytes.fetch_add(at(1).max(0) as u64, Relaxed);
            c.translog_modified_ms.store(at(2), Relaxed);
        }
        if let Some(Value::Arr(keys)) = m.get("request_cache") {
            self.shard(n).warm_request_cache(keys.iter().filter_map(Value::as_str));
        }
        if let Some(v) = m.get("leaf_sort") {
            self.shard(n).shard.set_leaf_sort(v.as_i64().and_then(|f| u32::try_from(f).ok()));
        }
        if let Some(Value::Arr(o)) = m.get("ops") {
            let at = |i: usize| o.get(i).and_then(Value::as_i64).unwrap_or(0).max(0) as u64;
            let saved = crate::cluster::OpCounts { flush: at(0), refresh: at(1), external_refresh: at(2) };
            let i = std::cell::Cell::new(0u32);
            ops.record(&self.uuid, shards, |c| {
                if i.get() == n {
                    *c = saved;
                }
                i.set(i.get() + 1);
            });
        }
        Ok(())
    }
}

impl crate::cluster::Cluster {
    /// The synonym rules the analysis chain of index `uuid` loaded, as JSON, when they differ
    /// from the sets' current rules (a set changed with `refresh=false`, or replaced by rules the
    /// chain can't build): Elasticsearch's analyzers keep such rules until something reloads
    /// them, and a wake is no reload. `None` when the chain follows the sets.
    pub fn loaded_synonyms_json(&self, uuid: &str) -> Option<String> {
        let st = self.state();
        let idx = st.metadata.indices.values().find(|i| i.uuid == uuid && !i.is_closed())?;
        // Only chains that load sets, and only one this instance built.
        if !idx.settings.iter().any(|(k, _)| k.starts_with("index.analysis.") && k.ends_with(".synonyms_set")) {
            return None;
        }
        self.analysis.loaded_synonyms(uuid)?;
        // A reload Elasticsearch would have done already (a set updated with `refresh=true`
        // since the chain was last used) happens first; one that fails keeps the loaded rules.
        let _ = self.index_analysis(&st, idx);
        let (generation, loaded) = self.analysis.loaded_synonyms(uuid)?;
        let current = &st.metadata.synonyms;
        let stale = loaded.iter().any(|(name, rules)| current.get(name).map(|s| s.rules.values().cloned().collect::<Vec<_>>()) != *rules);
        stale.then(|| serde_json::json!({ "generation": generation, "sets": &*loaded }).to_string())
    }

    /// Makes the next analysis chain of index `uuid` load what [`Cluster::loaded_synonyms_json`]
    /// reported, unless the sets were reloaded since. Call before the index opens.
    ///
    /// [`Cluster::loaded_synonyms_json`]: crate::cluster::Cluster::loaded_synonyms_json
    pub fn restore_loaded_synonyms(&self, uuid: &str, json: &str) -> EsResult<()> {
        let bad = |e: String| EsError::internal(format!("persisted synonym rules: {e}"));
        let v: serde_json::Value = serde_json::from_str(json).map_err(|e| bad(e.to_string()))?;
        let generation = v["generation"].as_u64().ok_or_else(|| bad("no generation".into()))?;
        let sets: crate::analysis::LoadedSynonyms = serde_json::from_value(v["sets"].clone()).map_err(|e| bad(e.to_string()))?;
        self.analysis.seed_loaded_synonyms(uuid, generation, sets);
        Ok(())
    }

    /// Writes the async results changed since the last call to [`ASYNC_RESULTS_INDEX`]
    /// (creating it on first use), so they outlive this instance as on Elasticsearch, where
    /// they live in that index. A durable server calls it before acknowledging a request that
    /// may have changed them, and before unloading; the writes are logged and acknowledged with
    /// the request. A failed write is noted again for the next call.
    pub async fn sync_async_results(&self) -> EsResult<()> {
        let results = &self.xpack.results;
        if !results.has_changes() {
            return Ok(());
        }
        let now = crate::time::now_millis();
        let st = self.state();
        let mut changes = match st.metadata.indices.get(ASYNC_RESULTS_INDEX) {
            Some(_) => results.take_changes(now),
            None => {
                // Never written, or deleted since: every live result needs its copy.
                results.note_all();
                let c = results.take_changes(now);
                if c.iter().all(|(_, e)| e.is_none()) {
                    return Ok(());
                }
                let created = self
                    .update(move |st| {
                        if st.metadata.indices.contains_key(ASYNC_RESULTS_INDEX) {
                            return Ok(());
                        }
                        crate::api::indices::create_index(st, ASYNC_RESULTS_INDEX, ASYNC_RESULTS_INDEX, async_results_index_request(), now).map(|_| ())
                    })
                    .await;
                if let Err(e) = created {
                    results.note_changed(c.into_iter().map(|(id, _)| id));
                    return Err(e);
                }
                c
            }
        };
        let Some(idx) = self.state().metadata.indices.get(ASYNC_RESULTS_INDEX).cloned() else {
            results.note_changed(changes.into_iter().map(|(id, _)| id));
            return Ok(());
        };
        let args = crate::write::WriteArgs::default();
        let mut i = 0;
        while i < changes.len() {
            let (id, entry) = &changes[i];
            let r = match entry {
                Some(e) => {
                    let value = async_result_doc(e);
                    let source = Bytes::from(value.to_json());
                    match crate::write::prepare(self, idx.clone(), id, None, &source, &value, None).await {
                        Ok(p) => crate::write::apply_index(p, &args, false, id).map(|_| ()),
                        Err(e) => Err(e),
                    }
                }
                None => crate::write::apply_delete(self, &idx, id, None, &args).map(|_| ()),
            };
            if let Err(e) = r {
                results.note_changed(changes.drain(i..).map(|(id, _)| id));
                return Err(e);
            }
            i += 1;
        }
        Ok(())
    }

    /// Persists the trained model deployment assignments when they changed since the last call
    /// (one atomic load otherwise), as cluster state ([`crate::xpack::ml::models::ASSIGNMENTS`]),
    /// where Elasticsearch keeps them. A durable server calls it before acknowledging a request
    /// that may have changed them; a failure is noted again for the next call.
    /// `unloading`: also keeps what the deployments' processes counted, so the wake doesn't
    /// look like a restart (written whenever it differs).
    pub async fn sync_ml_deployments(&self, unloading: bool) -> EsResult<()> {
        use crate::xpack::ml::models::ASSIGNMENTS;
        let deps = &self.xpack.deployments;
        if !deps.take_changed() && !(unloading && (deps.has_any() || self.state().metadata.xpack.len(ASSIGNMENTS) > 0)) {
            return Ok(());
        }
        let now = deps.assignments(unloading);
        let st = self.state();
        if st.metadata.xpack.iter(ASSIGNMENTS).map(|(k, v)| (k.clone(), v.clone())).eq(now.iter().cloned()) {
            return Ok(());
        }
        let r = self
            .update(move |st| {
                crate::xpack::store::clear(st, ASSIGNMENTS);
                if !now.is_empty() {
                    crate::xpack::store::family_mut(st, ASSIGNMENTS).extend(now);
                }
                Ok(())
            })
            .await;
        if r.is_err() {
            deps.note_changed();
        }
        r
    }

    /// Starts the deployments an earlier instance of the cluster persisted
    /// ([`Cluster::sync_ml_deployments`]) again. Returns how many.
    pub fn restore_ml_deployments(&self) -> usize {
        let st = self.state();
        self.xpack.deployments.restore(st.metadata.xpack.iter(crate::xpack::ml::models::ASSIGNMENTS))
    }

    /// Reads the async results an earlier instance of the cluster wrote
    /// ([`Cluster::sync_async_results`]) back into memory, and notes changes from now on.
    /// Free when there are none (no [`ASYNC_RESULTS_INDEX`]). Returns how many were live.
    pub fn restore_async_results(&self) -> EsResult<usize> {
        let results = &self.xpack.results;
        results.track_changes();
        let st = self.state();
        let Some(idx) = st.metadata.indices.get(ASYNC_RESULTS_INDEX).cloned() else { return Ok(0) };
        if idx.state == crate::meta::IndexState::Close {
            return Ok(0);
        }
        // Only as recovered (opened with its field numbering and replayed by the storage): an
        // index that isn't loaded now has nothing stored, or failed to open, and must not be
        // created the default way here.
        let Some(engine) = self.engine.loaded(&idx.uuid) else { return Ok(0) };
        let now = crate::time::now_millis();
        let mut live = 0;
        for shard in &engine.shards {
            shard.shard.refresh().map_err(convert::index_error)?;
            let searcher = shard.searcher();
            for seg in searcher.segments() {
                for local in 0..seg.max_doc() {
                    if !seg.is_live(local) {
                        continue;
                    }
                    let stored = searcher.stored(seg.base + local).map_err(convert::index_error)?;
                    let Some(id) = crate::search::hits::stored_id(&stored) else { continue };
                    let Some(source) = crate::fetch::source_bytes(&stored) else { continue };
                    let Some(entry) = crate::fetch::parse_source(source).ok().as_ref().and_then(async_result_entry) else { continue };
                    live += usize::from(entry.expiration_ms > now);
                    results.restore(id, entry, now);
                }
            }
        }
        Ok(live)
    }
}

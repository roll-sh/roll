//! The document engine of a cluster: per index, its shards (`search_index::Shard`s) and the
//! [`fields::FieldTable`] its mapping indexes into.
//!
//! [`Engine`] is owned by the [`crate::Cluster`] and keyed by index UUID, so an index deleted and
//! re-created under the same name starts empty. An [`IndexEngine`] is created on first use from
//! the index's metadata; its field table is rebuilt (and pushed to the shards as a new schema)
//! whenever the published mapping changes, detected by pointer identity of the mapping `Arc`.
//!
//! Refresh follows Elasticsearch's visible behaviour without a timer: a shard with unrefreshed
//! writes refreshes before a search when its periodic refresh (`index.refresh_interval`, 1 s)
//! would have run by now, or when it has been search-idle (`index.search.idle.after`, 30 s)
//! — the case where Elasticsearch makes the search wait for a refresh. Shards nobody reads do
//! no work.
//!
//! Durability (WAL, flush to object storage, recovery) plugs in through [`ShardStorage`]; the
//! default keeps shards in memory.

pub mod completion;
pub mod convert;
pub mod fields;
pub mod ranges;
pub mod shapes;
pub mod synthetic;
pub mod source;
pub mod tsdb;

use crate::error::{EsError, EsResult};
use crate::meta::{ClusterState, IndexMeta};
use fields::{FieldInfo, FieldTable, Kind, Numbers, Similarities};
use parking_lot::{Mutex, RwLock};
use search_index::{Shard, ShardConfig, Similarity};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Opens the shards of an index (persistence hooks for search-server).
pub trait ShardStorage: Send + Sync {
    fn open(&self, index_uuid: &str, shard: u32, schema: search_index::Schema) -> EsResult<Shard>;
}

/// Shards in memory.
pub struct MemoryShards;

impl ShardStorage for MemoryShards {
    fn open(&self, _uuid: &str, _shard: u32, schema: search_index::Schema) -> EsResult<Shard> {
        let cfg = ShardConfig { merge_threads: 1, ..Default::default() };
        Shard::open(cfg, schema, Vec::new()).map_err(|e| EsError::internal(e.to_string()))
    }
}

/// Per-shard operation counters for `_stats`.
#[derive(Default, Debug)]
pub struct ShardCounters {
    pub index_total: AtomicU64,
    pub index_time_nanos: AtomicU64,
    pub index_failed: AtomicU64,
    pub delete_total: AtomicU64,
    pub delete_time_nanos: AtomicU64,
    pub noop_update_total: AtomicU64,
    pub get_total: AtomicU64,
    pub get_time_nanos: AtomicU64,
    pub get_exists: AtomicU64,
    pub get_missing: AtomicU64,
    pub query_total: AtomicU64,
    pub query_time_nanos: AtomicU64,
    pub fetch_total: AtomicU64,
    pub fetch_time_nanos: AtomicU64,
    pub scroll_total: AtomicU64,
    pub suggest_total: AtomicU64,
    pub flush_total: AtomicU64,
    pub refresh_external: AtomicU64,
    pub warmer_total: AtomicU64,
    pub request_cache_hit: AtomicU64,
    pub request_cache_miss: AtomicU64,
    pub request_cache_evictions: AtomicU64,
    pub request_cache_bytes: AtomicU64,
    /// Query and fetch counts per search `stats` group.
    pub groups: Mutex<std::collections::BTreeMap<String, (u64, u64)>>,
}

static REFRESH_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Counts refreshes that published changes anywhere (keys request-cache entries that read
/// other indices).
pub fn refresh_epoch() -> u64 {
    REFRESH_EPOCH.load(Ordering::Relaxed)
}

/// One shard of an index.
pub struct ShardEngine {
    pub id: u32,
    pub shard: Shard,
    pub counters: ShardCounters,
    /// When the first write not yet refreshed happened (ms since the engine epoch, 0: none).
    dirty_since: AtomicI64,
    /// The last search (ms since the engine epoch).
    last_search: AtomicI64,
    /// The shard request cache (size-0 search results), cleared when a refresh publishes changes.
    pub request_cache: parking_lot::Mutex<std::collections::HashMap<String, Arc<dyn std::any::Any + Send + Sync>>>,
}

/// Off-heap sizes of one vector field (`DenseVectorStats` per field), by Lucene file
/// extension: raw vectors (`vec`), HNSW graph (`vex`), scalar-quantized (`veq`) and
/// bbq-binarized (`veb`) vectors. `None`: the field's format has no such file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VectorSizes {
    pub veb: Option<u64>,
    pub vec: Option<u64>,
    pub veq: Option<u64>,
    pub vex: Option<u64>,
}

impl VectorSizes {
    fn add(&mut self, o: &VectorSizes) {
        let sum = |a: &mut Option<u64>, b: Option<u64>| {
            if let Some(b) = b {
                *a = Some(a.unwrap_or(0) + b);
            }
        };
        sum(&mut self.veb, o.veb);
        sum(&mut self.vec, o.vec);
        sum(&mut self.veq, o.veq);
        sum(&mut self.vex, o.vex);
    }

    pub fn total(&self) -> u64 {
        self.veb.unwrap_or(0) + self.vec.unwrap_or(0) + self.veq.unwrap_or(0) + self.vex.unwrap_or(0)
    }
}

/// A shard's vector statistics (`DenseVectorStats`, `SparseVectorStats`).
#[derive(Clone, Debug, Default)]
pub struct VectorStats {
    /// Indexed dense vectors.
    pub dense_count: u64,
    /// Documents with a sparse vector.
    pub sparse_count: u64,
    /// Off-heap sizes per dense vector field.
    pub fields: std::collections::BTreeMap<String, VectorSizes>,
}

impl VectorStats {
    pub fn add(&mut self, o: &VectorStats) {
        self.dense_count += o.dense_count;
        self.sparse_count += o.sparse_count;
        for (k, v) in &o.fields {
            self.fields.entry(k.clone()).or_default().add(v);
        }
    }

    /// The `dense_vector` section: `value_count` and `off_heap` totals, with the per-field
    /// breakdown (`fielddata`) when `per_field`.
    pub fn dense_value(&self, per_field: bool) -> crate::json::Value {
        use crate::json::{Map, Value};
        let mut total = VectorSizes::default();
        for v in self.fields.values() {
            total.add(v);
        }
        let mut off = Map::new();
        off.insert("total_size_bytes".into(), Value::from(total.total() as i64));
        off.insert("total_veb_size_bytes".into(), Value::from(total.veb.unwrap_or(0) as i64));
        off.insert("total_vec_size_bytes".into(), Value::from(total.vec.unwrap_or(0) as i64));
        off.insert("total_veq_size_bytes".into(), Value::from(total.veq.unwrap_or(0) as i64));
        off.insert("total_vex_size_bytes".into(), Value::from(total.vex.unwrap_or(0) as i64));
        off.insert("total_cenivf_size_bytes".into(), Value::from(0));
        off.insert("total_clivf_size_bytes".into(), Value::from(0));
        if per_field && !self.fields.is_empty() {
            let mut fd = Map::new();
            for (name, s) in &self.fields {
                let mut m = Map::new();
                for (k, v) in [("veb_size_bytes", s.veb), ("vec_size_bytes", s.vec), ("veq_size_bytes", s.veq), ("vex_size_bytes", s.vex)] {
                    if let Some(v) = v {
                        m.insert(k.into(), Value::from(v as i64));
                    }
                }
                fd.insert(name.clone(), Value::Obj(m));
            }
            off.insert("fielddata".into(), Value::Obj(fd));
        }
        crate::obj! { "value_count" => self.dense_count as i64, "off_heap" => Value::Obj(off) }
    }
}

impl ShardEngine {
    /// The shard's vector statistics over its searchable segments: vector counts, and per
    /// field the bytes its formats keep off heap (raw vectors, graph, quantized vectors; the
    /// sizes the files' payloads take: Lucene adds headers and footers).
    pub fn vector_stats(&self, table: &FieldTable) -> VectorStats {
        use search_codec::VectorQuantization;
        let mut out = VectorStats::default();
        let searcher = self.shard.searcher();
        for f in table.fields.values() {
            if f.kind == Kind::SparseVector && f.parent.is_none() {
                let q: search_index::QueryRef = Arc::new(search_index::query::FieldExistsQuery { field: f.number });
                out.sparse_count += searcher.count(&q).unwrap_or(0);
                continue;
            }
            let Some(spec) = f.vector.filter(|_| f.index && f.kind == Kind::DenseVector) else { continue };
            let mut sizes = VectorSizes { vec: Some(0), ..Default::default() };
            if spec.hnsw.is_some() {
                sizes.vex = Some(0);
            }
            match spec.quantization {
                VectorQuantization::Scalar { .. } => sizes.veq = Some(0),
                VectorQuantization::Binary => sizes.veb = Some(0),
                VectorQuantization::None => {}
            }
            for seg in searcher.segments() {
                for n in std::iter::once(f.number).chain(f.vector_layouts.iter().copied()) {
                    let Some(vr) = seg.reader.vectors(n) else { continue };
                    let len = vr.len() as u64;
                    let s = vr.spec();
                    out.dense_count += len;
                    *sizes.vec.get_or_insert(0) += len * s.vector_bytes() as u64;
                    match s.quantization {
                        // A quantized byte (or nibble pair) per dimension plus a float correction.
                        VectorQuantization::Scalar { bits, .. } => {
                            let per = if bits <= 4 { s.dims.div_ceil(2) } else { s.dims } as u64 + 4;
                            *sizes.veq.get_or_insert(0) += len * per;
                        }
                        // One bit per dimension plus three float corrections and a short.
                        VectorQuantization::Binary => *sizes.veb.get_or_insert(0) += len * (s.dims.div_ceil(8) as u64 + 14),
                        VectorQuantization::None => {}
                    }
                    if vr.has_graph() {
                        // Neighbour lists: up to 2·m neighbours of 4 bytes on level 0.
                        let m = s.hnsw.map(|h| h.m as u64).unwrap_or(16);
                        *sizes.vex.get_or_insert(0) += len * (len.saturating_sub(1).min(2 * m) * 4 + 4);
                    }
                }
            }
            out.fields.insert(f.name.clone(), sizes);
        }
        out
    }

    fn now(&self) -> i64 {
        epoch_ms()
    }

    /// Called after every write.
    pub fn wrote(&self) {
        let now = self.now().max(1);
        let _ = self.dirty_since.compare_exchange(0, now, Ordering::AcqRel, Ordering::Relaxed);
    }

    /// Refreshes the shard (external refresh).
    pub fn refresh(&self) -> EsResult<bool> {
        self.dirty_since.store(0, Ordering::Release);
        self.counters.refresh_external.fetch_add(1, Ordering::Relaxed);
        let changed = self.shard.refresh().map_err(convert::index_error)?;
        if changed {
            self.clear_request_cache();
            REFRESH_EPOCH.fetch_add(1, Ordering::Relaxed);
        }
        Ok(changed)
    }

    /// Drops the request cache (refresh, `_cache/clear`).
    pub fn clear_request_cache(&self) {
        let mut c = self.request_cache.lock();
        if !c.is_empty() {
            c.clear();
            self.counters.request_cache_bytes.store(0, Ordering::Relaxed);
        }
    }

    /// Makes writes visible that a scheduled refresh would have published by now; see the
    /// module documentation. `interval_ms < 0` disables periodic refreshes.
    pub fn before_search(&self, interval_ms: i64) -> EsResult<()> {
        let now = self.now();
        let last = self.last_search.swap(now, Ordering::AcqRel);
        let dirty = self.dirty_since.load(Ordering::Acquire);
        if dirty == 0 || interval_ms < 0 {
            return Ok(());
        }
        let idle = now - last >= 30_000;
        if idle || now - dirty >= interval_ms {
            self.dirty_since.store(0, Ordering::Release);
            self.shard.refresh().map_err(convert::index_error)?;
        }
        Ok(())
    }

    pub fn searcher(&self) -> search_index::Searcher {
        self.shard.searcher()
    }

    /// Adds this shard's figures to an `_stats` block (sections that are present).
    pub fn stats_into(&self, block: &mut crate::json::Map) {
        use crate::json::Value;
        let st = self.shard.stats();
        let c = &self.counters;
        let ld = |a: &AtomicU64| a.load(Ordering::Relaxed) as i64;
        let ms = |a: &AtomicU64| (a.load(Ordering::Relaxed) / 1_000_000) as i64;
        let mut add = |section: &str, key: &str, v: i64| {
            if let Some(Value::Obj(m)) = block.get_mut(section) {
                let cur = m.get(key).and_then(Value::as_i64).unwrap_or(0);
                m.insert(key.into(), Value::from(cur + v));
            }
        };
        // Root documents only (nested children are internal), as `docs.count` reports.
        let searcher = self.shard.searcher();
        let roots = searcher.count(&search_index::query::nested::root_filter()).unwrap_or(st.num_docs) as i64;
        add("docs", "count", roots);
        add("docs", "deleted", st.deleted_docs as i64);
        add("docs", "total_size_in_bytes", st.segment_bytes as i64);
        add("store", "size_in_bytes", st.segment_bytes as i64);
        add("store", "total_data_set_size_in_bytes", st.segment_bytes as i64);
        add("indexing", "index_total", ld(&c.index_total));
        add("indexing", "index_time_in_millis", ms(&c.index_time_nanos));
        add("indexing", "index_failed", ld(&c.index_failed));
        add("indexing", "delete_total", ld(&c.delete_total));
        add("indexing", "delete_time_in_millis", ms(&c.delete_time_nanos));
        add("indexing", "noop_update_total", ld(&c.noop_update_total));
        add("get", "total", ld(&c.get_total));
        add("get", "time_in_millis", ms(&c.get_time_nanos));
        add("get", "exists_total", ld(&c.get_exists));
        add("get", "missing_total", ld(&c.get_missing));
        add("search", "query_total", ld(&c.query_total));
        add("search", "query_time_in_millis", ms(&c.query_time_nanos));
        add("search", "fetch_total", ld(&c.fetch_total));
        add("search", "fetch_time_in_millis", ms(&c.fetch_time_nanos));
        add("search", "scroll_total", ld(&c.scroll_total));
        add("search", "suggest_total", ld(&c.suggest_total));
        add("request_cache", "hit_count", ld(&c.request_cache_hit));
        add("request_cache", "miss_count", ld(&c.request_cache_miss));
        add("request_cache", "evictions", ld(&c.request_cache_evictions));
        add("request_cache", "memory_size_in_bytes", ld(&c.request_cache_bytes));
        add("segments", "count", st.segments as i64);
        add("segments", "memory_in_bytes", st.segment_heap_bytes as i64);
        add("segments", "index_writer_memory_in_bytes", st.buffer_bytes as i64);
        add("segments", "version_map_memory_in_bytes", st.version_map_bytes as i64);
        add("merges", "current", st.merges_running as i64);
        add("merges", "current_docs", st.merges_current_docs as i64);
        add("merges", "current_size_in_bytes", st.merges_current_bytes as i64);
        add("merges", "total", st.merges_total as i64);
        add("merges", "total_docs", st.merges_total_docs as i64);
        add("merges", "total_size_in_bytes", st.merges_total_bytes as i64);
        add("merges", "total_time_in_millis", st.merges_total_time.as_millis() as i64);
        add("translog", "operations", (st.max_seq_no - st.local_checkpoint).max(0));
    }

    /// Adds the search `groups` matching `patterns` to an `_stats` block's `search` section.
    pub fn group_stats_into(&self, block: &mut crate::json::Map, patterns: &[String]) {
        use crate::json::{Map, Value};
        if patterns.is_empty() {
            return;
        }
        let Some(Value::Obj(search)) = block.get_mut("search") else { return };
        let groups = self.counters.groups.lock();
        let mut out = match search.shift_remove("groups") {
            Some(Value::Obj(m)) => m,
            _ => Map::new(),
        };
        for (name, (q, f)) in groups.iter() {
            if !patterns.iter().any(|p| crate::xcontent::filter::glob_match(p, name)) {
                continue;
            }
            let prev = |k: &str| out.get(name).and_then(|g| g.get(k)).and_then(Value::as_i64).unwrap_or(0);
            let (pq, pf) = (prev("query_total"), prev("fetch_total"));
            out.insert(
                name.clone(),
                crate::obj! {
                    "query_total" => pq + *q as i64, "query_time_in_millis" => 0, "query_current" => 0, "query_failure" => 0,
                    "fetch_total" => pf + *f as i64, "fetch_time_in_millis" => 0, "fetch_current" => 0, "fetch_failure" => 0,
                    "scroll_total" => 0, "scroll_time_in_millis" => 0, "scroll_current" => 0,
                    "suggest_total" => 0, "suggest_time_in_millis" => 0, "suggest_current" => 0,
                },
            );
        }
        if !out.is_empty() {
            search.insert("groups".into(), Value::Obj(out));
        }
    }
}

fn epoch_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as i64 + 1
}

/// One index's engine state.
pub struct IndexEngine {
    pub uuid: String,
    pub name: String,
    pub shards: Vec<Arc<ShardEngine>>,
    numbers: Numbers,
    table: RwLock<Arc<FieldTable>>,
    /// Serializes table rebuilds.
    rebuild: Mutex<()>,
    /// Text fields whose field data (uninverted terms) a sort or aggregation has loaded: their
    /// memory shows in `fielddata` stats until `_cache/clear` drops it.
    pub fielddata_loaded: Mutex<std::collections::BTreeSet<String>>,
}

impl std::fmt::Debug for IndexEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexEngine").field("name", &self.name).field("shards", &self.shards.len()).finish()
    }
}

impl IndexEngine {
    /// A loaded index engine. Construct through here (not a struct literal), so new engine
    /// state doesn't break other constructors.
    pub(crate) fn assemble(uuid: String, name: String, shards: Vec<Arc<ShardEngine>>, numbers: Numbers, table: FieldTable) -> IndexEngine {
        IndexEngine { uuid, name, shards, numbers, table: RwLock::new(Arc::new(table)), rebuild: Mutex::new(()), fielddata_loaded: Mutex::new(Default::default()) }
    }

    /// Records that a request loads `field`'s field data: text fields with `fielddata: true`
    /// (sorts and aggregations), or the global ordinals of keyword fields (aggregations).
    pub fn load_fielddata(&self, field: &str, aggregation: bool) {
        let t = self.table();
        let text = |f: &FieldInfo| f.kind.is_text() && f.subfield("fielddata").is_some();
        let ordinals = |f: &FieldInfo| aggregation && f.doc_values && matches!(f.kind, fields::Kind::Keyword | fields::Kind::Ip | fields::Kind::Version);
        if t.get(field).is_some_and(|f| text(f) || ordinals(f)) {
            let mut l = self.fielddata_loaded.lock();
            if !l.contains(field) {
                l.insert(field.to_string());
            }
        }
    }

    /// Adds `fielddata` and `completion` memory to a stats block: totals, and per field for the
    /// fields matching `fielddata_fields` / `completion_fields` (`fields` patterns).
    pub fn field_stats_into(&self, shard: &ShardEngine, block: &mut crate::json::Map, fielddata_fields: &[String], completion_fields: &[String]) {
        use crate::json::{Map, Value};
        let t = self.table();
        let searcher = shard.searcher();
        let bytes_of = |n: u32, kind: search_codec::container::SectionKind| -> i64 {
            searcher.segments().iter().map(|s| s.reader.sections().into_iter().filter(|(k, f, _)| *k == kind && *f == n).map(|(_, _, b)| b as i64).sum::<i64>()).sum()
        };
        let matches = |pats: &[String], name: &str| pats.iter().any(|p| crate::xcontent::filter::glob_match(p, name));
        let loaded: Vec<String> = self.fielddata_loaded.lock().iter().cloned().collect();
        if let Some(Value::Obj(fd)) = block.get_mut("fielddata") {
            let mut total = fd.get("memory_size_in_bytes").and_then(Value::as_i64).unwrap_or(0);
            let mut per = match fd.shift_remove("fields") {
                Some(Value::Obj(m)) => m,
                _ => Map::new(),
            };
            let mut ords = match fd.get_mut("global_ordinals").and_then(|g| match g {
                Value::Obj(m) => m.shift_remove("fields"),
                _ => None,
            }) {
                Some(Value::Obj(m)) => m,
                _ => Map::new(),
            };
            for name in &loaded {
                let Some(f) = t.get(name) else { continue };
                let b = match f.subfield("fielddata") {
                    Some(n) => bytes_of(n, search_codec::container::SectionKind::DocValues),
                    None => {
                        // Keyword global ordinals exist once a shard has several segments: an
                        // ordinal map of (about) eight bytes per term and segment.
                        let segs = searcher.segments();
                        if segs.len() < 2 {
                            continue;
                        }
                        let counts: Vec<i64> = segs.iter().map(|s| s.reader.sorted_set(f.number).map_or(0, |d| d.value_count() as i64)).collect();
                        let max = counts.iter().copied().max().unwrap_or(0);
                        if matches(fielddata_fields, name) {
                            let cur = ords.get(name).and_then(|v| v.get("shard_max_value_count")).and_then(Value::as_i64).unwrap_or(0);
                            ords.insert(name.clone(), crate::obj! { "build_time_in_millis" => 0, "shard_max_value_count" => cur.max(max) });
                        }
                        16 * segs.len() as i64 + 8 * counts.iter().sum::<i64>()
                    }
                };
                total += b;
                if matches(fielddata_fields, name) {
                    let cur = per.get(name).and_then(|v| v.get("memory_size_in_bytes")).and_then(Value::as_i64).unwrap_or(0);
                    per.insert(name.clone(), crate::obj! { "memory_size_in_bytes" => cur + b });
                }
            }
            fd.insert("memory_size_in_bytes".into(), Value::from(total));
            if !fielddata_fields.is_empty() && !per.is_empty() {
                fd.insert("fields".into(), Value::Obj(per));
            }
            if !fielddata_fields.is_empty()
                && !ords.is_empty()
                && let Some(Value::Obj(g)) = fd.get_mut("global_ordinals")
            {
                g.insert("fields".into(), Value::Obj(ords));
            }
        }
        if let Some(Value::Obj(c)) = block.get_mut("completion") {
            let mut total = c.get("size_in_bytes").and_then(Value::as_i64).unwrap_or(0);
            let mut per = match c.shift_remove("fields") {
                Some(Value::Obj(m)) => m,
                _ => Map::new(),
            };
            let mut names: Vec<(String, u32)> = t.fields.values().filter(|f| f.kind == fields::Kind::Completion).map(|f| (f.name.clone(), f.number)).collect();
            names.sort();
            for (name, n) in names {
                let b = bytes_of(n, search_codec::container::SectionKind::Suggest);
                total += b;
                if matches(completion_fields, &name) {
                    let cur = per.get(&name).and_then(|v| v.get("size_in_bytes")).and_then(Value::as_i64).unwrap_or(0);
                    per.insert(name, crate::obj! { "size_in_bytes" => cur + b });
                }
            }
            c.insert("size_in_bytes".into(), Value::from(total));
            if !completion_fields.is_empty() && !per.is_empty() {
                c.insert("fields".into(), Value::Obj(per));
            }
        }
    }

    pub fn table(&self) -> Arc<FieldTable> {
        self.table.read().clone()
    }

    pub fn shard(&self, id: u32) -> &Arc<ShardEngine> {
        &self.shards[id as usize % self.shards.len()]
    }

    /// `index.refresh_interval` in milliseconds (`-1`: disabled).
    pub fn refresh_interval(idx: &IndexMeta) -> i64 {
        match idx.settings.get("index.refresh_interval") {
            Some(v) => crate::units::TimeValue::parse(v, "index.refresh_interval").map(|t| t.as_millis()).unwrap_or(1000),
            None => 1000,
        }
    }

    pub fn refresh_all(&self) -> EsResult<()> {
        for s in &self.shards {
            s.refresh()?;
        }
        Ok(())
    }

    /// The largest point value of a long-encoded field (`long`, `date`, `date_nanos`) across
    /// the shards' searchable segments.
    pub fn max_long_point(&self, field: &str) -> Option<i64> {
        let number = self.table().get(field)?.number;
        self.shards
            .iter()
            .filter_map(|s| s.searcher().point_bounds(number))
            .filter_map(|b| b.max.get(..8).and_then(|m| <[u8; 8]>::try_from(m).ok()))
            .map(|m| (u64::from_be_bytes(m) ^ (1 << 63)) as i64)
            .max()
    }

    /// Total live docs (root docs only).
    pub fn doc_count(&self) -> u64 {
        self.shards.iter().map(|s| s.searcher().count(&search_index::query::nested::root_filter()).unwrap_or(0)).sum()
    }
}

/// The index sort segments keep (`index.sort.*`, or the index mode's default sort): keys over
/// numeric doc values (numbers, dates, booleans, as sortable longs) or sorted ones (keywords,
/// ips, `_tsid`), each with its order, missing placement and multi-value selector.
pub fn index_sort(idx: &IndexMeta, table: &FieldTable) -> Option<search_codec::IndexSort> {
    use search_codec::{SortField, SortKind, SortMissing, SortSelector};
    let s = &idx.settings;
    let defaults = crate::meta::index_sort::sort_defaults(s);
    if defaults.fields.is_empty() {
        return None;
    }
    let list = |k: &str, d: &[String]| s.get_list(k).unwrap_or_else(|| d.to_vec());
    let order = list("index.sort.order", &defaults.order);
    let mode = list("index.sort.mode", &defaults.mode);
    let missing = list("index.sort.missing", &defaults.missing);
    let mut fields = Vec::with_capacity(defaults.fields.len());
    for (i, name) in defaults.fields.iter().enumerate() {
        let (number, kind) = if name == "_tsid" {
            (fields::TSID_FIELD, SortKind::String)
        } else {
            let f = table.get(name)?;
            if !f.doc_values {
                return None;
            }
            let kind = if f.kind.is_numeric() || f.kind.is_date() || f.kind == Kind::Boolean {
                SortKind::Long
            } else if f.kind.is_keyword_like() || f.kind == Kind::Ip {
                SortKind::String
            } else {
                return None;
            };
            (f.number, kind)
        };
        let reverse = order.get(i).is_some_and(|o| o == "desc");
        let selector = match mode.get(i).map(String::as_str) {
            Some("max") => SortSelector::Max,
            Some("min") => SortSelector::Min,
            _ if reverse => SortSelector::Max,
            _ => SortSelector::Min,
        };
        let missing = if missing.get(i).is_some_and(|m| m == "_first") { SortMissing::First } else { SortMissing::Last };
        fields.push(SortField { field: number, kind, reverse, missing, selector });
    }
    Some(search_codec::IndexSort { fields })
}

/// Parses `index.similarity.*` (Elasticsearch's `SimilarityService`).
pub fn similarities(idx: &IndexMeta) -> Similarities {
    let mut named = HashMap::new();
    let groups = idx.settings.by_prefix("index.similarity.");
    let mut names: Vec<String> = groups.keys().filter_map(|k| k.split('.').next().map(str::to_string)).collect();
    names.sort();
    names.dedup();
    for n in names {
        let get = |k: &str| groups.get(&format!("{n}.{k}")).map(|v| v.to_string());
        let f = |k: &str, d: f32| get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
        let discount = get("discount_overlaps").map(|v| v != "false").unwrap_or(true);
        let sim = match get("type").as_deref() {
            // `legacy: false` (Lucene's plain BM25) is internal: percolator memory indices.
            Some("BM25") | None => Similarity::Bm25 { k1: f("k1", 1.2), b: f("b", 0.75), discount_overlaps: discount, legacy: get("legacy").as_deref() != Some("false") },
            Some("boolean") => Similarity::Boolean,
            Some("LMDirichlet") => Similarity::LmDirichlet { mu: f("mu", 2000.0), discount_overlaps: discount },
            Some("LMJelinekMercer") => Similarity::LmJelinekMercer { lambda: f("lambda", 0.1), discount_overlaps: discount },
            Some("DFI") => {
                use search_index::similarity::Independence;
                let independence = match get("independence_measure").as_deref() {
                    Some("saturated") => Independence::Saturated,
                    Some("chisquared") => Independence::ChiSquared,
                    _ => Independence::Standardized,
                };
                Similarity::Dfi { independence, discount_overlaps: discount }
            }
            Some("DFR") => {
                use search_index::similarity::{AfterEffect, BasicModel};
                let basic_model = match get("basic_model").as_deref() {
                    Some("if") => BasicModel::IF,
                    Some("in") => BasicModel::In,
                    Some("ine") => BasicModel::Ine,
                    _ => BasicModel::G,
                };
                let after_effect = match get("after_effect").as_deref() {
                    Some("b") => AfterEffect::B,
                    _ => AfterEffect::L,
                };
                Similarity::Dfr { basic_model, after_effect, normalization: normalization(&get), discount_overlaps: discount }
            }
            Some("IB") => {
                use search_index::similarity::{Distribution, Lambda};
                let distribution = match get("distribution").as_deref() {
                    Some("spl") => Distribution::SPL,
                    _ => Distribution::LL,
                };
                let lambda = match get("lambda").as_deref() {
                    Some("ttf") => Lambda::TTF,
                    _ => Lambda::DF,
                };
                Similarity::Ib { distribution, lambda, normalization: normalization(&get), discount_overlaps: discount }
            }
            _ => Similarity::bm25(),
        };
        named.insert(n, sim);
    }
    let default = named.get("default").cloned().unwrap_or_default();
    Similarities { default, named }
}

fn normalization(get: &dyn Fn(&str) -> Option<String>) -> search_index::similarity::Normalization {
    use search_index::similarity::Normalization;
    let f = |k: &str, d: f32| get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    match get("normalization").as_deref() {
        Some("no") => Normalization::None,
        Some("h1") => Normalization::H1(f("normalization.h1.c", 1.0)),
        Some("h3") => Normalization::H3(f("normalization.h3.c", 800.0)),
        Some("z") => Normalization::Z(f("normalization.z.z", 0.30)),
        _ => Normalization::H2(f("normalization.h2.c", 1.0)),
    }
}

/// Every index engine of a cluster.
pub struct Engine {
    indices: RwLock<HashMap<String, Arc<IndexEngine>>>,
    storage: Arc<dyn ShardStorage>,
}

impl Default for Engine {
    fn default() -> Self {
        Engine { indices: RwLock::new(HashMap::new()), storage: Arc::new(MemoryShards) }
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("indices", &self.indices.read().len()).finish()
    }
}

impl Engine {
    pub fn with_storage(storage: Arc<dyn ShardStorage>) -> Engine {
        Engine { indices: RwLock::new(HashMap::new()), storage }
    }

    /// The engine of an existing (loaded) index, without creating one.
    pub fn loaded(&self, uuid: &str) -> Option<Arc<IndexEngine>> {
        self.indices.read().get(uuid).cloned()
    }

    /// The engine of `idx`, created on first use; its field table follows the published mapping.
    pub fn index(&self, cluster: &crate::Cluster, st: &ClusterState, idx: &IndexMeta) -> EsResult<Arc<IndexEngine>> {
        let existing = self.indices.read().get(&idx.uuid).cloned();
        let ie = match existing {
            Some(ie) => ie,
            None => {
                let mut g = self.indices.write();
                if let Some(ie) = g.get(&idx.uuid) {
                    ie.clone()
                } else {
                    // Forget engines of indices that are gone.
                    g.retain(|uuid, _| st.metadata.indices.values().any(|i| &i.uuid == uuid));
                    let analysis = Arc::new(cluster.index_analysis(st, idx)?);
                    let numbers = Numbers::default();
                    let ctx = idx.mapping_ctx();
                    let mut table = FieldTable::build(idx.mapping.clone(), analysis, &ctx, &numbers, &similarities(idx))?;
                    table.key = table_key(st, idx);
                    table.schema = table.schema.clone().with_index_sort(index_sort(idx, &table));
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
                        }));
                    }
                    let ie = Arc::new(IndexEngine::assemble(idx.uuid.clone(), idx.name.clone(), shards, numbers, table));
                    g.insert(idx.uuid.clone(), ie.clone());
                    return Ok(ie);
                }
            }
        };
        self.sync_table(cluster, st, idx, &ie)?;
        Ok(ie)
    }

    /// Brings `ie`'s field table up to `idx`. Requests resolve their `IndexMeta` before taking
    /// any lock, so a request may carry metadata older than the table a concurrent mapping
    /// update installed: the table only ever moves forward, or a stale request would roll the
    /// table and the shard schemas back and later writers would drop the newly mapped fields.
    fn sync_table(&self, cluster: &crate::Cluster, st: &ClusterState, idx: &IndexMeta, ie: &IndexEngine) -> EsResult<()> {
        let key = table_key(st, idx);
        let current = |t: &FieldTable| (Arc::ptr_eq(&t.mapping, &idx.mapping) && t.key == key) || is_older(key, t.key);
        if current(&ie.table()) {
            return Ok(());
        }
        let _g = ie.rebuild.lock();
        if current(&ie.table()) {
            return Ok(());
        }
        let analysis = cluster.index_analysis(st, idx)?;
        let ctx = idx.mapping_ctx();
        let mut table = FieldTable::build(idx.mapping.clone(), Arc::new(analysis), &ctx, &ie.numbers, &similarities(idx))?;
        table.key = key;
        table.schema = table.schema.clone().with_index_sort(index_sort(idx, &table));
        for s in &ie.shards {
            s.shard.update_schema(table.schema.clone()).map_err(convert::index_error)?;
        }
        *ie.table.write() = Arc::new(table);
        Ok(())
    }

    /// Drops the engines of indices no longer in `st`.
    pub fn retain(&self, st: &ClusterState) {
        self.indices.write().retain(|uuid, _| st.metadata.indices.values().any(|i| &i.uuid == uuid));
    }

    pub fn remove(&self, uuid: &str) {
        self.indices.write().remove(uuid);
    }
}

/// The versions a field table is built from: (mapping, settings, synonyms). Each only grows.
pub fn table_key(st: &ClusterState, idx: &IndexMeta) -> (u64, u64, u64) {
    (idx.mapping_version, idx.settings_version, st.metadata.synonyms_version)
}

/// `a` comes from metadata older than `b` in some component (metadata versions are monotonic,
/// so such a request saw a state from before `b` was installed).
fn is_older(a: (u64, u64, u64), b: (u64, u64, u64)) -> bool {
    a.0 < b.0 || a.1 < b.1 || a.2 < b.2
}


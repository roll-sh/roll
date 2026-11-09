//! The module's INFO sections (`RS_moduleInfoFunc`, `info/info_redis/info_redis.c`), all
//! prefixed `search_`: the version; without indexes (and `search-_info-on-zero-indexes` off)
//! only the zeroed index counts and the runtime configuration; else the index totals
//! (`IndexesInfo_TotalInfo`), the field statistics, memory, vector indexes, cursors, the GC,
//! the query counters, errors and warnings, threads, dialects and the configuration.

use super::global::ErrorCounters;
use super::spec;
use crate::config::Config;
use crate::engine::Engine;
use std::fmt::Write;
use std::sync::atomic::Ordering::Relaxed;

/// `RedisModule_InfoAddFieldDouble`'s `%.17g`.
fn dbl(v: f64) -> String {
    crate::util::fmt_g_prec(v, 17)
}

/// `MEMORY_MB`.
fn mb(bytes: u64) -> String {
    dbl(bytes as f64 / (1024.0 * 1024.0))
}

/// A section's body: `search_<name>:<value>` lines.
#[derive(Default)]
struct Body(String);

impl Body {
    fn f(&mut self, k: &str, v: impl std::fmt::Display) {
        let _ = write!(self.0, "search_{k}:{v}\r\n");
    }

    /// `RedisModule_InfoBeginDictField`: `search_<name>:a=1,b=2`.
    fn dict(&mut self, k: &str, items: &[(&str, u64)]) {
        let v: Vec<String> = items.iter().map(|(a, b)| format!("{a}={b}")).collect();
        self.f(k, v.join(","));
    }
}

/// `TotalIndexesInfo` plus the field counts (`RSGlobalStats.fieldsStats`, kept with the specs).
#[derive(Default)]
struct Totals {
    total_mem: u64,
    min_mem: Option<u64>,
    max_mem: u64,
    indexing_ns: u64,
    docs: u64,
    blocks: i64,
    failures: u64,
    max_failures: u64,
    gc_collected: i64,
    gc_cycles: u64,
    gc_ms: u64,
    vec_mem: u64,
    vec_marked: u64,
    vec_flat_buffer: u64,
}

/// One field type's counts (`FieldsGlobalStats_UpdateStats`) and errors.
#[derive(Default, Clone, Copy)]
struct FieldCounts {
    n: u64,
    sortable: u64,
    noindex: u64,
    case_sensitive: u64,
    flat: u64,
    hnsw: u64,
    svs: u64,
    svs_compressed: u64,
    errors: u64,
}

/// `(section name, body)` pairs in the order RediSearch adds them; every field already carries
/// the `search_` prefix and a CRLF.
pub fn sections(eng: &Engine) -> Vec<(&'static str, String)> {
    let cfg = &eng.cfg;
    let mut out = Vec::new();
    out.push(("search_version", format!("search_version:{}\r\nsearch_redis_version:{} - oss\r\n", super::VERSION_TEXT, crate::commands::server::REDIS_VERSION)));
    let g = &eng.search;
    if g.indexes.is_empty() && !cfg.flag("search-_info-on-zero-indexes") {
        let mut s = Body::default();
        for f in [
            "number_of_indexes",
            "number_of_active_indexes",
            "number_of_active_indexes_running_queries",
            "number_of_active_indexes_indexing",
            "total_active_write_threads",
            "total_indexing_time",
            "total_num_docs_in_indexes",
            "total_inverted_index_blocks",
        ] {
            s.f(f, 0);
        }
        out.push(("search_indexes", s.0));
        out.push(("search_runtime_configurations", runtime(cfg)));
        return out;
    }

    // `IndexesInfo_TotalInfo` over the registry, and the field statistics.
    let mut t = Totals::default();
    // text, numeric, tag, geo, vector, geoshape.
    let mut fc = [FieldCounts::default(); 6];
    for ix in g.indexes.values() {
        let mut vec_mem = 0u64;
        for f in ix.fields.iter().filter_map(|f| f.vectors.as_ref()) {
            vec_mem += f.memory() as u64;
            t.vec_marked += f.marked() as u64;
            t.vec_flat_buffer += f.flat_buffer() as u64;
        }
        t.vec_mem += vec_mem;
        let mem = ix.total_mem(vec_mem as usize);
        t.total_mem += mem;
        t.min_mem = Some(t.min_mem.map_or(mem, |m| m.min(mem)));
        t.max_mem = t.max_mem.max(mem);
        t.indexing_ns += ix.stats.total_index_time;
        if let Some(gc) = &ix.gc {
            t.gc_collected += gc.total_collected;
            t.gc_cycles += gc.num_cycles;
            t.gc_ms += gc.total_ms_run;
        }
        t.docs += ix.stats.num_documents;
        t.blocks += ix.stats.total_blocks;
        let failures = ix.error.count;
        t.failures += failures;
        t.max_failures = t.max_failures.max(failures);
        for fs in &ix.spec.fields {
            let k = if fs.is(spec::T_TEXT) {
                0
            } else if fs.is(spec::T_NUMERIC) {
                1
            } else if fs.is(spec::T_GEO) {
                3
            } else if fs.is(spec::T_VECTOR) {
                4
            } else if fs.is(spec::T_TAG) {
                2
            } else if fs.is(spec::T_GEOMETRY) {
                5
            } else {
                continue;
            };
            let c = &mut fc[k];
            c.n += 1;
            if k == 2 && fs.tag_case_sensitive {
                c.case_sensitive += 1;
            }
            if k == 4
                && let Some(v) = &fs.vector
            {
                match v.algo {
                    spec::Algo::Flat => c.flat += 1,
                    spec::Algo::Hnsw => c.hnsw += 1,
                    spec::Algo::Svs => {
                        c.svs += 1;
                        if v.quant != spec::Quant::None {
                            c.svs_compressed += 1;
                        }
                    }
                }
            }
            if k != 4 && fs.sortable() {
                c.sortable += 1;
            }
            if k != 4 && !fs.indexable() {
                c.noindex += 1;
            }
        }
        // `FieldIndexErrorCounter`, by the erring field's type.
        for (fs, fi) in ix.spec.fields.iter().zip(&ix.fields) {
            let pos = [spec::T_TEXT, spec::T_NUMERIC, spec::T_TAG, spec::T_GEO, spec::T_VECTOR, spec::T_GEOMETRY].iter().position(|&ty| fs.is(ty));
            if let Some(p) = pos {
                fc[p].errors += fi.error.count;
            }
        }
    }
    // The shared SVS thread pool's memory, tied to no index.
    let shared = g.vecsim_shared_memory(cfg.num("search-workers").max(0) as usize) as u64;
    t.vec_mem += shared;
    t.total_mem += shared;

    let mut s = Body::default();
    s.f("number_of_indexes", g.indexes.len());
    for f in ["number_of_active_indexes", "number_of_active_indexes_running_queries", "number_of_active_indexes_indexing", "total_active_write_threads"] {
        s.f(f, 0);
    }
    // `(float)indexing_time / (float)CLOCKS_PER_MILLISEC`: nanoseconds over 1000.
    s.f("total_indexing_time", dbl((t.indexing_ns as f32 / 1000f32) as f64));
    s.f("total_num_docs_in_indexes", t.docs);
    s.f("total_inverted_index_blocks", t.blocks as u64);
    out.push(("search_indexes", s.0));

    let mut s = Body::default();
    let opt = |name: &'static str, v: u64| (v > 0).then_some((name, v));
    let c = fc[0];
    if c.n > 0 {
        let items: Vec<(&str, u64)> = [Some(("Text", c.n)), opt("Sortable", c.sortable), opt("NoIndex", c.noindex), Some(("IndexErrors", c.errors))].into_iter().flatten().collect();
        s.dict("fields_text", &items);
    }
    let c = fc[1];
    if c.n > 0 {
        let items: Vec<(&str, u64)> = [Some(("Numeric", c.n)), opt("Sortable", c.sortable), opt("NoIndex", c.noindex), Some(("IndexErrors", c.errors))].into_iter().flatten().collect();
        s.dict("fields_numeric", &items);
    }
    let c = fc[2];
    if c.n > 0 {
        let items: Vec<(&str, u64)> = [Some(("Tag", c.n)), opt("Sortable", c.sortable), opt("NoIndex", c.noindex), opt("CaseSensitive", c.case_sensitive), Some(("IndexErrors", c.errors))]
            .into_iter()
            .flatten()
            .collect();
        s.dict("fields_tag", &items);
    }
    let c = fc[3];
    if c.n > 0 {
        let items: Vec<(&str, u64)> = [Some(("Geo", c.n)), opt("Sortable", c.sortable), opt("NoIndex", c.noindex), Some(("IndexErrors", c.errors))].into_iter().flatten().collect();
        s.dict("fields_geo", &items);
    }
    let c = fc[4];
    if c.n > 0 {
        let mut items: Vec<(&str, u64)> = [Some(("Vector", c.n)), opt("Flat", c.flat), opt("HNSW", c.hnsw)].into_iter().flatten().collect();
        if c.svs > 0 {
            items.push(("SVS_VAMANA", c.svs));
            items.extend(opt("SVS_VAMANA_Compressed", c.svs_compressed));
        }
        items.push(("IndexErrors", c.errors));
        s.dict("fields_vector", &items);
    }
    let c = fc[5];
    if c.n > 0 {
        let items: Vec<(&str, u64)> = [Some(("Geoshape", c.n)), opt("Sortable", c.sortable), opt("NoIndex", c.noindex), Some(("IndexErrors", c.errors))].into_iter().flatten().collect();
        s.dict("fields_geoshape", &items);
    }
    let ops = &g.indexing;
    s.f("total_indexing_ops_text_fields", ops.ops_text);
    s.f("total_indexing_ops_tag_fields", ops.ops_tag);
    s.f("total_indexing_ops_numeric_fields", ops.ops_numeric);
    s.f("total_indexing_ops_geo_fields", ops.ops_geo);
    s.f("total_indexing_ops_geoshape_fields", ops.ops_geoshape);
    s.f("total_indexing_ops_vector_fields", ops.ops_vector);
    out.push(("search_fields_statistics", s.0));

    let mut s = Body::default();
    let min = t.min_mem.unwrap_or(0);
    s.f("used_memory_indexes", t.total_mem);
    s.f("used_memory_indexes_human", mb(t.total_mem));
    s.f("smallest_memory_index", min);
    s.f("smallest_memory_index_human", mb(min));
    s.f("largest_memory_index", t.max_mem);
    s.f("largest_memory_index_human", mb(t.max_mem));
    out.push(("search_memory", s.0));

    let mut s = Body::default();
    s.f("used_memory_vector_index", t.vec_mem);
    s.f("hnsw_direct_main_thread_insertions", 0);
    s.f("tiered_index_frontend_buffer_size", t.vec_flat_buffer);
    out.push(("search_vector_index", s.0));

    // `Cursors_GetInfoStats`: the user cursors (idle ones included until collected); a
    // standalone server has no coordinator cursors.
    let (idle, total) = g.cursors.counts();
    let mut s = Body::default();
    s.f("global_idle_user", idle);
    s.f("global_idle_internal", 0);
    s.f("global_total_user", total);
    s.f("global_total_internal", 0);
    out.push(("search_cursors", s.0));

    let mut s = Body::default();
    s.f("gc_bytes_collected", t.gc_collected);
    s.f("gc_total_cycles", t.gc_cycles);
    s.f("gc_total_ms_run", t.gc_ms);
    s.f("gc_total_docs_not_collected", g.logically_deleted(crate::util::now_ms()));
    s.f("gc_marked_deleted_vectors", t.vec_marked);
    out.push(("search_garbage_collector", s.0));

    let q = &g.stats;
    let mut s = Body::default();
    s.f("total_queries_processed", q.queries_processed.load(Relaxed));
    s.f("total_query_commands", q.query_commands.load(Relaxed));
    s.f("total_query_execution_time_ms", q.execution_ns.load(Relaxed) / 1_000_000);
    s.f("total_active_queries", 0);
    s.f("total_coord_dispatch_time_ms", dbl(0.0));
    out.push(("search_queries", s.0));

    let mut s = Body::default();
    s.f("errors_indexing_failures", t.failures);
    s.f("errors_for_index_with_max_failures", t.max_failures);
    s.f("OOM_indexing_failures_indexes_count", 0);
    counters(&mut s, "shard", &q.shard);
    out.push(("search_warnings_and_errors", s.0));
    let mut s = Body::default();
    counters(&mut s, "coord", &q.coord);
    out.push(("search_coordinator_warnings_and_errors", s.0));

    let mut s = Body::default();
    for f in [
        "uv_threads_running_queries",
        "uv_threads_running_topology_update",
        "active_worker_threads",
        "active_coord_threads",
        "workers_low_priority_pending_jobs",
        "workers_high_priority_pending_jobs",
        "workers_admin_priority_pending_jobs",
        "coord_high_priority_pending_jobs",
    ] {
        s.f(f, 0);
    }
    out.push(("search_multi_threading", s.0));

    let mut s = Body::default();
    let d = g.used_dialects();
    for i in 1..=4 {
        s.f(&format!("dialect_{i}"), d >> (i - 1) & 1);
    }
    out.push(("search_dialect_statistics", s.0));

    out.push(("search_runtime_configurations", runtime(cfg)));
    out
}

/// One side's query errors and warnings, in `AddToInfo_ErrorsAndWarnings`' order.
fn counters(s: &mut Body, side: &str, c: &ErrorCounters) {
    for (k, v) in [
        ("errors_syntax", &c.syntax),
        ("errors_arguments", &c.arguments),
        ("errors_timeout", &c.timeout),
        ("warnings_timeout", &c.warn_timeout),
        ("errors_oom", &c.oom),
        ("errors_unavailable_slots", &c.unavailable_slots),
        ("warnings_oom", &c.warn_oom),
        ("warnings_max_prefix_expansions", &c.warn_max_prefix_expansions),
        ("warnings_asm_inaccurate_results", &c.warn_asm_inaccurate),
    ] {
        s.f(&format!("{side}_total_query_{k}"), v.load(Relaxed));
    }
}

fn runtime(c: &Config) -> String {
    let mut s = String::new();
    let mut f = |k: &str, v: String| {
        let _ = write!(s, "search_{k}:{v}\r\n");
    };
    let on = |b: bool| if b { "ON" } else { "OFF" }.to_string();
    f("extension_load", c.text("search-ext-load"));
    f("friso_ini", c.text("search-friso-ini"));
    f("default_scorer", c.text("search-default-scorer:value"));
    f("enableGC", on(!c.flag("search-no-gc")));
    f("minimal_term_prefix", c.num("search-min-prefix").to_string());
    f("minimal_stem_length", c.num("search-min-stem-len").to_string());
    f("maximal_prefix_expansions", c.num("search-max-prefix-expansions").to_string());
    f("query_timeout_ms", c.num("search-timeout").to_string());
    f("timeout_policy", super::config::TIMEOUT_POLICIES[c.num("search-on-timeout") as usize].into());
    f("oom_policy", super::config::OOM_POLICIES[c.num("search-on-oom") as usize].into());
    // `cursorReadSize` has no setting.
    f("cursor_read_size", "1000".into());
    f("cursor_max_idle_time", c.num("search-cursor-max-idle").to_string());
    f("max_doc_table_size", c.num("search-max-doctablesize").to_string());
    f("max_search_results", c.num("search-max-search-results").to_string());
    f("max_aggregate_results", c.num("search-max-aggregate-results").to_string());
    f("max_aggregate_groups", c.num("search-max-aggregate-groups").to_string());
    f("gc_scan_size", c.num("search-gc-scan-size").to_string());
    f("min_phonetic_term_length", c.num("search-min-phonetic-term-len").to_string());
    f("bm25std_tanh_factor", c.num("search-bm25std-tanh-factor").to_string());
    f("info_on_zero_indexes", on(c.flag("search-_info-on-zero-indexes")));
    s
}

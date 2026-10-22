//! The registry of index-scoped settings: names, defaults, value types and update scope, as
//! Elasticsearch's `IndexScopedSettings` declares them, with Elasticsearch's validation texts.
//!
//! - Unknown keys fail with `unknown setting [k] please check that any required plugins are
//!   installed, …` (the first one; later ones are attached as `suppressed`).
//! - Values are validated per type (`Failed to parse value [x] for setting [k] must be >= 1`, …).
//! - Updates distinguish dynamic settings, static ones (only on closed indices), final ones
//!   (never after creation), private ones (managed by Elasticsearch) and internal ones (which
//!   users can't set at all and are reported as unknown).
//! - `include_defaults` prints every registered default that isn't set.

use super::settings::{SettingValue, Settings};
use crate::cluster::Dialect;
use crate::error::EsError;
use crate::units::{ByteSize, TimeValue, parse_bool, parse_java_double, parse_java_long};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Updatable on open indices.
    Dynamic,
    /// Updatable only while the index is closed.
    Static,
    /// Fixed at creation.
    Final,
    /// Managed by Elasticsearch; can't be set explicitly.
    Private,
    /// Written by Elasticsearch; reported as unknown when a user sets it.
    Internal,
    /// Settable at creation but only changed through a dedicated API afterwards.
    Managed,
}

#[derive(Clone, Copy, Debug)]
pub enum Ty {
    Bool,
    Int(i64, i64),
    Long(i64, i64),
    Double(f64, f64),
    /// With a minimum in milliseconds (`-1` allowed when the minimum is `-1`).
    Time(i64),
    /// A data stream global retention: unset (`-1`) or at least 10 seconds.
    Retention,
    /// `Setting.dateSetting`: `strict_date_optional_time||epoch_millis`.
    Date,
    /// `action.auto_create_index`: a boolean or index patterns with `+`/`-` prefixes.
    AutoCreate,
    Bytes,
    Str,
    List,
    /// Values compared case-sensitively against the options; the error text.
    Enum(&'static [&'static str], EnumErr),
    AutoExpandReplicas,
    ActiveShards,
    /// A base type plus a validator that runs on the raw value once it parses (`Setting`
    /// validators); its error text is the reason as is.
    Check(&'static Ty, fn(&str) -> Result<(), String>),
}

#[derive(Clone, Copy, Debug)]
pub enum EnumErr {
    /// `unknown value for [k] must be one of [a, b] but was: x`
    OneOf,
    /// `No enum constant org.elasticsearch.<class>.<VALUE>` (Java `Enum.valueOf` on upper-case).
    JavaEnum(&'static str),
    /// `Enum.valueOf(class, value.toUpperCase())` over lower-case option names.
    JavaEnumCaseInsensitive(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub enum Dflt {
    Str(&'static str),
    List(&'static [&'static str]),
    /// Computed from other settings (`number_of_routing_shards`, …).
    Computed,
    None,
}

#[derive(Debug)]
pub struct IndexSetting {
    pub key: &'static str,
    pub default: Dflt,
    pub ty: Ty,
    pub scope: Scope,
}

macro_rules! s {
    ($k:expr, $d:expr, $t:expr, $s:ident) => {
        IndexSetting { key: $k, default: $d, ty: $t, scope: Scope::$s }
    };
}

use Dflt::{Computed, List as L, Str as D};
use Ty::*;

const B: Ty = Bool;
const DUR: &[&str] = &["request", "async", "REQUEST", "ASYNC"];

/// `IndexMode` names, in Elasticsearch's declaration order.
pub const INDEX_MODES: &[&str] = &["standard", "time_series", "logsdb", "lookup", "columnar", "logsdb_columnar", "vectordb_document"];

/// Sorted by key.
pub static INDEX_SETTINGS: &[IndexSetting] = &[
    s!("index.allocation.existing_shards_allocator", D("gateway_allocator"), Str, Static),
    s!("index.allocation.max_retries", D("5"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.analyze.max_char_count", D("1000000"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.analyze.max_token_count", D("10000"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.auto_expand_replicas", D("false"), AutoExpandReplicas, Dynamic),
    s!("index.blocks.metadata", D("false"), B, Dynamic),
    s!("index.blocks.read", D("false"), B, Dynamic),
    s!("index.blocks.read_only", D("false"), B, Dynamic),
    s!("index.blocks.read_only_allow_delete", D("false"), B, Dynamic),
    s!("index.blocks.write", D("false"), B, Dynamic),
    s!("index.bloom_filter_for_id_field.enabled", D("true"), B, Dynamic),
    s!("index.codec", D("default"), Enum(&["default", "best_compression", "legacy_default", "legacy_best_compression", "lucene_default"], EnumErr::OneOf), Static),
    s!("index.compound_format", D("1gb"), Str, Dynamic),
    s!("index.creation_date", Dflt::None, Long(-1, i64::MAX), Internal),
    s!("index.data_path", D(""), Str, Final),
    s!("index.default_pipeline", D("_none"), Str, Dynamic),
    s!("index.dense_vector.experimental_features", D("false"), B, Final),
    s!("index.dense_vector.hnsw_enable_early_termination", D("true"), B, Dynamic),
    s!("index.dense_vector.hnsw_filter_heuristic", D("ACORN"), Str, Dynamic),
    s!("index.dense_vector.post_filter_selectivity_threshold", D("1.0"), Double(0.0, 1.0), Dynamic),
    s!("index.dimensions", L(&[]), List, Private),
    s!("index.dimensions_tsid_strategy_enabled", D("true"), B, Final),
    s!("index.disable_sequence_numbers", D("false"), B, Final),
    s!("index.dlm.frozen.created", D("false"), B, Managed),
    s!("index.downsample.interval", D(""), Str, Internal),
    s!("index.downsample.origin.name", D(""), Str, Private),
    s!("index.downsample.origin.uuid", D(""), Str, Private),
    s!("index.downsample.sampling_method", D(""), Str, Internal),
    s!("index.downsample.source.name", D(""), Str, Private),
    s!("index.downsample.source.uuid", D(""), Str, Private),
    s!("index.downsample.status", D("unknown"), Str, Internal),
    s!("index.esql.stored_fields_sequential_proportion", D("0.2"), Double(0.0, f64::MAX), Dynamic),
    s!("index.failure_store.version", D("0"), Long(0, i64::MAX), Static),
    s!("index.fast_refresh", D("false"), B, Final),
    s!("index.fielddata.cache", D("node"), Str, Static),
    s!("index.final_pipeline", D("_none"), Str, Dynamic),
    s!("index.flush_after_merge", D("512mb"), Bytes, Dynamic),
    s!("index.force_memory_term_dictionary", D("false"), B, Internal),
    s!("index.format", D("0"), Int(0, i32::MAX as i64), Final),
    s!("index.gc_deletes", D("60s"), Time(-1), Dynamic),
    s!("index.hidden", D("false"), B, Dynamic),
    s!("index.highlight.max_analyzed_offset", D("1000000"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.highlight.weight_matches_mode.enabled", D("true"), B, Dynamic),
    s!("index.indexing.slowlog.include.user", D("false"), B, Dynamic),
    s!("index.indexing.slowlog.level", D(""), Str, Internal),
    s!("index.indexing.slowlog.reformat", D("true"), B, Dynamic),
    s!("index.indexing.slowlog.source", D("1000"), Str, Dynamic),
    s!("index.indexing.slowlog.threshold.index.debug", D("-1"), Time(-1), Dynamic),
    s!("index.indexing.slowlog.threshold.index.info", D("-1"), Time(-1), Dynamic),
    s!("index.indexing.slowlog.threshold.index.trace", D("-1"), Time(-1), Dynamic),
    s!("index.indexing.slowlog.threshold.index.warn", D("-1"), Time(-1), Dynamic),
    s!("index.lifecycle.indexing_complete", D("false"), B, Dynamic),
    s!("index.lifecycle.name", D(""), Str, Dynamic),
    s!("index.lifecycle.origination_date", D("-1"), Long(-1, i64::MAX), Dynamic),
    s!("index.lifecycle.parse_origination_date", D("false"), B, Dynamic),
    s!("index.lifecycle.prefer_ilm", D("true"), B, Dynamic),
    s!("index.lifecycle.rollover_alias", D(""), Str, Dynamic),
    s!("index.lifecycle.skip", D("false"), B, Dynamic),
    s!("index.lifecycle.step.wait_time_threshold", D("12h"), Time(-1), Dynamic),
    s!("index.load_fixed_bitset_filters_eagerly", D("true"), B, Static),
    s!("index.logsdb.add_host_name_field", D("false"), B, Private),
    s!("index.logsdb.default_sort_on_message_template", D("false"), B, Final),
    s!("index.logsdb.route_on_sort_fields", D("false"), B, Final),
    s!("index.logsdb.sort_on_host_name", D("false"), B, Private),
    s!("index.logsdb.sort_on_message_template", D("false"), B, Private),
    s!("index.look_ahead_time", D("9m"), Time(0), Dynamic),
    s!("index.look_back_time", D("2h"), Time(0), Dynamic),
    s!("index.mapper.dynamic", D("true"), B, Dynamic),
    s!("index.mapping.array_objects.limit", D("50000"), Long(1, i64::MAX), Dynamic),
    s!("index.mapping.coerce", D("false"), B, Static),
    s!("index.mapping.depth.limit", D("20"), Long(1, i64::MAX), Dynamic),
    s!("index.mapping.dimension_fields.limit", D("32768"), Long(0, i64::MAX), Dynamic),
    s!("index.mapping.doc_values.multi_value", D("true"), B, Final),
    s!("index.mapping.doc_values.nullability", D("true"), B, Final),
    s!("index.mapping.dynamic_strings.auto_text", D("true"), B, Dynamic),
    s!("index.mapping.exclude_source_vectors", D("true"), B, Final),
    s!("index.mapping.field_name_length.ignore_dynamic_beyond_limit", D("false"), B, Dynamic),
    s!("index.mapping.field_name_length.limit", D("9223372036854775807"), Long(1, i64::MAX), Dynamic),
    s!("index.mapping.flattened.store_root_doc_values", D("false"), B, Final),
    s!("index.mapping.ignore_above", D("2147483647"), Int(0, i32::MAX as i64), Static),
    s!("index.mapping.ignore_malformed", D("false"), B, Static),
    s!("index.mapping.index_disabled_by_default", D("false"), B, Final),
    s!("index.mapping.nested_fields.limit", D("100"), Long(0, i64::MAX), Dynamic),
    s!("index.mapping.nested_objects.limit", D("10000"), Long(0, i64::MAX), Dynamic),
    s!("index.mapping.nested_parents.limit", D("50"), Long(0, i64::MAX), Dynamic),
    s!("index.mapping.pattern_text.disable_templating", D("false"), B, Private),
    s!("index.mapping.semantic_text.use_legacy_format", D("false"), B, Managed),
    s!("index.mapping.source.mode", D("STORED"), Enum(&["STORED", "SYNTHETIC", "DISABLED", "COLUMNAR_STORED", "stored", "synthetic", "disabled", "columnar_stored"], EnumErr::JavaEnum("index.mapper.SourceFieldMapper.Mode")), Final),
    s!("index.mapping.synthetic_id", D("false"), B, Final),
    s!("index.mapping.synthetic_source.skip_ignored_source_read", D("false"), B, Dynamic),
    s!("index.mapping.synthetic_source.skip_ignored_source_write", D("false"), B, Dynamic),
    s!("index.mapping.synthetic_source_keep", D("none"), Enum(&["none", "arrays", "all"], EnumErr::OneOf), Static),
    s!("index.mapping.total_fields.ignore_dynamic_beyond_limit", D("false"), B, Dynamic),
    s!("index.mapping.total_fields.limit", D("1000"), Long(0, i64::MAX), Dynamic),
    s!("index.mapping.use_columnar_id_mode_by_default", D("false"), B, Final),
    s!("index.mapping.use_doc_values_skipper", D("false"), B, Final),
    s!("index.max_adjacency_matrix_filters", D("100"), Int(2, i32::MAX as i64), Dynamic),
    s!("index.max_docvalue_fields_search", D("100"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.max_inner_result_window", D("100"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.max_ngram_diff", D("1"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.max_refresh_listeners", D("1000"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.max_regex_length", D("1000"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.max_rescore_window", Computed, Int(1, i32::MAX as i64), Dynamic),
    s!("index.max_result_window", D("10000"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.max_script_fields", D("32"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.max_shingle_diff", D("3"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.max_slices_per_scroll", D("1024"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.max_terms_count", D("65536"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.merge.intra_merge_parallelism_enabled", D("false"), B, Dynamic),
    s!("index.merge.policy.deletes_pct_allowed", D("20.0"), Double(5.0, 50.0), Dynamic),
    s!("index.merge.policy.expunge_deletes_allowed", D("10.0"), Double(0.0, f64::MAX), Dynamic),
    s!("index.merge.policy.floor_segment", D("16mb"), Bytes, Dynamic),
    s!("index.merge.policy.max_merge_at_once", D("16"), Int(2, i32::MAX as i64), Dynamic),
    s!("index.merge.policy.max_merge_at_once_explicit", D("30"), Int(2, i32::MAX as i64), Dynamic),
    s!("index.merge.policy.max_merged_segment", D("0b"), Bytes, Dynamic),
    s!("index.merge.policy.merge_factor", D("32"), Int(2, i32::MAX as i64), Dynamic),
    s!("index.merge.policy.segments_per_tier", D("8.0"), Double(2.0, f64::MAX), Dynamic),
    s!("index.merge.policy.type", D("UNSET"), Str, Dynamic),
    s!("index.merge.scheduler.auto_throttle", D("true"), B, Dynamic),
    s!("index.merge.scheduler.max_merge_count", D("14"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.merge.scheduler.max_thread_count", D("9"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.mode", D("standard"), Enum(INDEX_MODES, EnumErr::JavaEnumCaseInsensitive("index.IndexMode")), Final),
    s!("index.number_of_replicas", D("1"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.number_of_routing_shards", Computed, Int(1, 1024), Final),
    s!("index.number_of_shards", D("1"), Int(1, 1024), Final),
    s!("index.optimize_auto_generated_id", D("true"), B, Static),
    s!("index.override_write_load_forecast", D("0.0"), Double(0.0, f64::MAX), Dynamic),
    s!("index.percolator.map_unmapped_fields_as_text", D("false"), B, Static),
    s!("index.priority", D("1"), Int(0, i32::MAX as i64), Dynamic),
    s!("index.provided_name", Dflt::None, Str, Internal),
    s!("index.queries.cache.enabled", D("true"), B, Static),
    s!("index.query.default_field", L(&["*"]), List, Dynamic),
    s!("index.query.parse.allow_unmapped_fields", D("true"), B, Static),
    s!("index.query_string.lenient", D("false"), B, Static),
    s!("index.recovery.type", D(""), Str, Final),
    s!("index.recovery.use_synthetic_source", D("false"), B, Final),
    s!("index.refresh_interval", D("1s"), Time(-1), Dynamic),
    s!("index.requests.cache.enable", D("true"), B, Dynamic),
    s!("index.resize.source.name", Dflt::None, Str, Internal),
    s!("index.resize.source.uuid", Dflt::None, Str, Internal),
    s!("index.rollup.source.name", D(""), Str, Private),
    s!("index.rollup.source.uuid", D(""), Str, Private),
    s!("index.routing.allocation.disk.watermark.ignore", D("false"), B, Dynamic),
    s!("index.routing.allocation.enable", D("all"), Enum(&["all", "primaries", "new_primaries", "none", "ALL", "PRIMARIES", "NEW_PRIMARIES", "NONE"], EnumErr::OneOf), Dynamic),
    s!("index.routing.allocation.total_shards_per_node", D("-1"), Int(-1, i32::MAX as i64), Dynamic),
    s!("index.routing.rebalance.enable", D("all"), Enum(&["all", "primaries", "replicas", "none", "ALL", "PRIMARIES", "REPLICAS", "NONE"], EnumErr::OneOf), Dynamic),
    s!("index.routing_partition_size", D("1"), Int(1, 1024), Final),
    s!("index.routing_path", L(&[]), List, Final),
    s!("index.search.idle.after", D("30s"), Time(0), Dynamic),
    s!("index.search.slowlog.include.user", D("false"), B, Dynamic),
    s!("index.search.slowlog.level", D(""), Str, Internal),
    s!("index.search.slowlog.threshold.fetch.debug", D("-1"), Time(-1), Dynamic),
    s!("index.search.slowlog.threshold.fetch.info", D("-1"), Time(-1), Dynamic),
    s!("index.search.slowlog.threshold.fetch.trace", D("-1"), Time(-1), Dynamic),
    s!("index.search.slowlog.threshold.fetch.warn", D("-1"), Time(-1), Dynamic),
    s!("index.search.slowlog.threshold.query.debug", D("-1"), Time(-1), Dynamic),
    s!("index.search.slowlog.threshold.query.info", D("-1"), Time(-1), Dynamic),
    s!("index.search.slowlog.threshold.query.trace", D("-1"), Time(-1), Dynamic),
    s!("index.search.slowlog.threshold.query.warn", D("-1"), Time(-1), Dynamic),
    s!("index.semantic_text.default_inference_id", D(""), Str, Final),
    s!("index.seq_no.index_options", D("POINTS_AND_DOC_VALUES"), Enum(&["POINTS_AND_DOC_VALUES", "DOC_VALUES_ONLY", "points_and_doc_values", "doc_values_only"], EnumErr::OneOf), Final),
    s!("index.shard.check_on_startup", D("false"), Enum(&["false", "true", "checksum"], EnumErr::OneOf), Static),
    s!("index.shard_limit.group", D("normal"), Str, Static),
    s!("index.slice.enabled", D("false"), B, Final),
    s!("index.soft_deletes.enabled", D("true"), B, Final),
    s!("index.soft_deletes.retention.operations", D("0"), Long(0, i64::MAX), Dynamic),
    s!("index.soft_deletes.retention_lease.period", D("12h"), Time(0), Dynamic),
    s!("index.sort.field", Computed, List, Final),
    s!("index.sort.missing", Computed, List, Final),
    s!("index.sort.mode", Computed, List, Final),
    s!("index.sort.order", Computed, List, Final),
    s!("index.source_only", D("false"), B, Private),
    s!("index.store.fs.directio_async_prefetch_limit", D("64"), Long(0, i64::MAX), Static),
    s!("index.store.fs.fs_lock", D("native"), Enum(&["native", "simple"], EnumErr::OneOf), Static),
    s!("index.store.preload", L(&[]), List, Static),
    s!("index.store.snapshot.blob_cache.metadata_files.max_length", D("64kb"), Bytes, Static),
    s!("index.store.snapshot.cache.enabled", D("true"), B, Static),
    s!("index.store.snapshot.cache.excluded_file_types", L(&[]), List, Static),
    s!("index.store.snapshot.cache.prewarm.enabled", D("true"), B, Static),
    s!("index.store.snapshot.delete_searchable_snapshot", D("false"), B, Private),
    s!("index.store.snapshot.index_name", D(""), Str, Private),
    s!("index.store.snapshot.index_uuid", D(""), Str, Private),
    s!("index.store.snapshot.partial", D("false"), B, Private),
    s!("index.store.snapshot.repository_name", D(""), Str, Private),
    s!("index.store.snapshot.repository_uuid", D(""), Str, Private),
    s!("index.store.snapshot.snapshot_name", D(""), Str, Private),
    s!("index.store.snapshot.snapshot_uuid", D(""), Str, Private),
    s!("index.store.snapshot.uncached_chunk_size", D("-1b"), Bytes, Static),
    s!("index.store.stats_refresh_interval", D("10s"), Time(-1), Static),
    s!("index.store.type", D(""), Str, Static),
    s!("index.synthetic_id.bloom_filter.high_bits_per_doc", D("128.0"), Double(0.0, f64::MAX), Final),
    s!("index.synthetic_id.bloom_filter.large_segment_min_docs", D("320000"), Long(0, i64::MAX), Final),
    s!("index.synthetic_id.bloom_filter.low_bits_per_doc", D("24.0"), Double(0.0, f64::MAX), Final),
    s!("index.synthetic_id.bloom_filter.max_size", D("8mb"), Bytes, Final),
    s!("index.synthetic_id.bloom_filter.num_hash_functions", D("4"), Long(0, i64::MAX), Final),
    s!("index.synthetic_id.bloom_filter.optimized_merge", D("true"), B, Final),
    s!("index.synthetic_id.bloom_filter.small_segment_max_docs", D("160000"), Long(0, i64::MAX), Final),
    s!("index.time_series.end_time", D("9999-12-31T23:59:59.999Z"), Date, Dynamic),
    s!("index.time_series.es87tsdb_codec.enabled", D("true"), B, Final),
    s!("index.time_series.es95_codec.enabled", D("false"), B, Final),
    s!("index.time_series.start_time", D("-9999-01-01T00:00:00Z"), Date, Final),
    s!("index.time_series.temporality_field", D(""), Str, Final),
    s!("index.top_metrics_max_size", D("10"), Int(1, i32::MAX as i64), Dynamic),
    s!("index.translog.durability", D("REQUEST"), Enum(DUR, EnumErr::JavaEnum("index.translog.Translog.Durability")), Dynamic),
    s!("index.translog.flush_threshold_age", D("1m"), Time(1000), Dynamic),
    s!("index.translog.flush_threshold_size", D("10gb"), Bytes, Dynamic),
    s!("index.translog.generation_threshold_size", D("64mb"), Bytes, Dynamic),
    s!("index.translog.retention.age", D("-1"), Time(-1), Dynamic),
    s!("index.translog.retention.size", D("-1"), Bytes, Dynamic),
    s!("index.translog.sync_interval", D("5s"), Time(100), Dynamic),
    s!("index.unassigned.node_left.delayed_timeout", D("1m"), Time(0), Dynamic),
    s!("index.use_legacy_postings_format", D("false"), B, Final),
    s!("index.use_time_series_doc_values_format", D("false"), B, Final),
    s!("index.use_time_series_doc_values_format_large_binary_block_size", D("true"), B, Final),
    s!("index.use_time_series_doc_values_format_large_block_size", D("false"), B, Final),
    s!("index.uuid", Dflt::None, Str, Internal),
    s!("index.verified_before_close", D("false"), B, Private),
    s!("index.verified_read_only", D("false"), B, Private),
    s!("index.version.compatibility", Computed, Str, Private),
    s!("index.version.created", Computed, Str, Private),
    s!("index.warmer.enabled", D("true"), B, Dynamic),
    s!("index.write.wait_for_active_shards", D("1"), ActiveShards, Dynamic),
    s!("index.xpack.ccr.following_index", D("false"), B, Static),
    s!("index.xpack.version", D(""), Str, Static),
    s!("index.xpack.watcher.template.version", D(""), Str, Static),
];

/// Prefixes under which any key is accepted (group/affix settings).
const GROUP_PREFIXES: &[&str] = &[
    "index.analysis.",
    "index.similarity.",
    "index.routing.allocation.include.",
    "index.routing.allocation.exclude.",
    "index.routing.allocation.require.",
    "index.routing.allocation.initial_recovery.",
];

pub fn lookup(key: &str) -> Option<&'static IndexSetting> {
    lookup_in(Dialect::Elasticsearch, key)
}

/// The index settings a dialect registers (OpenSearch's are generated from OpenSearch 3.9.0).
pub fn registry(d: Dialect) -> &'static [IndexSetting] {
    match d {
        Dialect::Elasticsearch => INDEX_SETTINGS,
        Dialect::OpenSearch => super::index_settings_table_os::OS_INDEX_SETTINGS,
    }
}

pub fn lookup_in(d: Dialect, key: &str) -> Option<&'static IndexSetting> {
    let r = registry(d);
    r.binary_search_by(|s| s.key.cmp(key)).ok().map(|i| &r[i])
}

/// The update scope of a group key: allocation filters are dynamic, analysis and similarity
/// definitions only change on closed indices.
pub fn group_scope(key: &str) -> Scope {
    if key.starts_with("index.routing.allocation.") { Scope::Dynamic } else { Scope::Static }
}

pub fn is_group_key(key: &str) -> bool {
    GROUP_PREFIXES.iter().any(|p| key.starts_with(p) && key.len() > p.len())
}

pub fn unknown_setting(key: &str) -> EsError {
    unknown_setting_in(Dialect::Elasticsearch, key)
}

/// `AbstractScopedSettings.validate`'s text for an unknown key: registered keys more than 70%
/// similar (Lucene's `LevenshteinDistance`) are suggested, best first; ties keep the order the
/// Java `HashMap` of registered keys iterates in.
pub fn unknown_setting_in(d: Dialect, key: &str) -> EsError {
    // List settings match `key.N` too, so they aren't among the plain keys suggestions come from.
    let keys: Vec<&str> = registry(d).iter().filter(|s| !matches!(s.ty, Ty::List)).map(|s| s.key).collect();
    EsError::illegal_argument(format!("unknown setting [{key}]{}", suggestion(keys.into_iter(), key)))
}

/// Internal settings are registered, so they get no suggestions (not even themselves).
pub fn unknown_internal(key: &str) -> EsError {
    EsError::illegal_argument(format!("unknown setting [{key}]{PLUGINS_HINT}"))
}

const PLUGINS_HINT: &str = " please check that any required plugins are installed, or check the breaking changes documentation for removed settings";

/// ` did you mean [x]?`, ` did you mean any of [x, y]?`, or the plugins hint.
pub fn suggestion<'a>(keys: impl ExactSizeIterator<Item = &'a str>, key: &str) -> String {
    let cap = java_hash_map_capacity(keys.len());
    let mut scored: Vec<(f32, usize, &str)> = keys
        .filter_map(|k| {
            let d = crate::rest::params::levenshtein_similarity(key, k);
            (d > 0.7).then(|| (d, java_bucket(k, cap), k))
        })
        .collect();
    if scored.is_empty() {
        return PLUGINS_HINT.into();
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    if scored.len() == 1 {
        return format!(" did you mean [{}]?", scored[0].2);
    }
    let names: Vec<&str> = scored.iter().map(|s| s.2).collect();
    format!(" did you mean any of [{}]?", names.join(", "))
}

/// The table size of a Java `HashMap` filled by `put`s from the default capacity (16, load
/// factor 0.75).
fn java_hash_map_capacity(n: usize) -> usize {
    let mut cap = 16;
    while n as f64 > cap as f64 * 0.75 {
        cap *= 2;
    }
    cap
}

/// The bucket a key lands in: `String.hashCode` spread as `HashMap.hash` does.
fn java_bucket(key: &str, cap: usize) -> usize {
    let mut h: i32 = 0;
    for u in key.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    let h = (h as u32) ^ ((h as u32) >> 16);
    (h as usize) & (cap - 1)
}

/// The JVM's `For input string: "x"` text of a `NumberFormatException`.
fn nfe(s: &str) -> EsError {
    EsError::java(400, "number_format_exception", format!("For input string: \"{s}\""))
}

/// Parses a date setting value (`Setting.dateSetting`) to epoch milliseconds.
pub fn parse_date_setting(s: &str) -> Result<i64, EsError> {
    const FORMAT: &str = "strict_date_optional_time||epoch_millis";
    if s.is_empty() {
        return Err(EsError::illegal_argument("cannot parse empty datetime"));
    }
    let f = crate::time::DateFormatter::new(FORMAT).map_err(EsError::illegal_argument)?;
    f.parse_millis(s).map_err(|_| {
        EsError::illegal_argument(format!("failed to parse date field [{s}] with format [{FORMAT}]"))
            .caused_by(EsError::java(400, "date_time_parse_exception", "Failed to parse with all enclosed parsers"))
    })
}

/// Validates one value against its definition.
pub fn validate_value(def: &IndexSetting, v: &SettingValue) -> Result<(), EsError> {
    validate_value_in(Dialect::Elasticsearch, def, v)
}

/// [`validate_value`] with the dialect's Java package in enum errors.
pub fn validate_value_in(d: Dialect, def: &IndexSetting, v: &SettingValue) -> Result<(), EsError> {
    validate_value_inner(def, v).map_err(|mut e| {
        if d == Dialect::OpenSearch && e.reason().contains("org.elasticsearch.") {
            let r = e.reason().replace("org.elasticsearch.", "org.opensearch.");
            e.reason = Some(r);
        }
        e
    })
}

fn validate_value_inner(def: &IndexSetting, v: &SettingValue) -> Result<(), EsError> {
    let key = def.key;
    let s = match v {
        SettingValue::Null => return Ok(()),
        SettingValue::List(l) => {
            if matches!(def.ty, Ty::List) {
                return Ok(());
            }
            // A list given for a scalar setting reads as its comma-joined form.
            &l.join(",")
        }
        SettingValue::Str(s) => s.as_str(),
    };
    match def.ty {
        Ty::Bool => parse_bool(s).map(|_| ()),
        Ty::Int(min, max) | Ty::Long(min, max) => {
            // Integers past the type's range still compare against its bounds (Elasticsearch
            // checks them as big integers); anything else is a number format error.
            let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
            let n = match parse_java_long(s) {
                Some(n) => n,
                None if s.strip_prefix('-').is_some_and(digits) => i64::MIN,
                None if digits(s.strip_prefix('+').unwrap_or(s)) => i64::MAX,
                None => return Err(EsError::illegal_argument(format!("Failed to parse value [{s}] for setting [{key}]")).caused_by(nfe(s))),
            };
            if n < min {
                return Err(EsError::illegal_argument(format!("Failed to parse value [{s}] for setting [{key}] must be >= {min}")));
            }
            if n > max {
                return Err(EsError::illegal_argument(format!("Failed to parse value [{s}] for setting [{key}] must be <= {max}")));
            }
            Ok(())
        }
        Ty::Double(min, max) => {
            let Some(n) = parse_java_double(s) else {
                return Err(EsError::illegal_argument(format!("Failed to parse value [{s}] for setting [{key}]")).caused_by(nfe(s)));
            };
            if n < min {
                return Err(EsError::illegal_argument(format!(
                    "Failed to parse value [{s}] for setting [{key}] must be >= {}",
                    crate::json::write::java_double(min)
                )));
            }
            if n > max {
                return Err(EsError::illegal_argument(format!(
                    "Failed to parse value [{s}] for setting [{key}] must be <= {}",
                    crate::json::write::java_double(max)
                )));
            }
            Ok(())
        }
        Ty::Time(min_ms) => {
            let t = TimeValue::parse(s, key)?;
            let ok = if min_ms == -1 { t.duration >= -1 || t == TimeValue::MINUS_ONE } else { t.as_millis() >= min_ms && t.duration >= 0 };
            if !ok {
                let min = if min_ms == -1 { "-1".to_string() } else { TimeValue::millis(min_ms).string_rep() };
                let min = if min_ms > 0 && min_ms % 1000 == 0 { TimeValue::seconds(min_ms / 1000).string_rep() } else { min };
                return Err(EsError::illegal_argument(format!("failed to parse value [{s}] for setting [{key}], must be >= [{min}]")));
            }
            Ok(())
        }
        Ty::Date => parse_date_setting(s).map(|_| ()),
        Ty::AutoCreate => {
            if s.is_empty() || s == "true" || s == "false" {
                return Ok(());
            }
            // Elasticsearch tries a boolean first and reports that failure as suppressed.
            let not_bool = || vec![EsError::illegal_argument(format!("Failed to parse value [{s}] as only [true] or [false] are allowed."))];
            for part in s.split(',') {
                if part.is_empty() {
                    return Err(EsError::illegal_argument(format!(
                        "Can't parse [{s}] for setting [{key}] must be either [true, false, or a comma separated list of index patterns]"
                    ))
                    .with_suppressed(not_bool()));
                }
                if part == "-" || part == "+" {
                    return Err(EsError::illegal_argument(format!("Can't parse [{s}] for setting [{key}] must contain an index name after [{part}]"))
                        .with_suppressed(not_bool()));
                }
            }
            Ok(())
        }
        Ty::Retention => {
            let t = TimeValue::parse(s, key)?;
            if t != TimeValue::MINUS_ONE && t.as_millis() < 10_000 {
                return Err(EsError::illegal_argument(format!("Setting '{key}' should be greater than 10s")));
            }
            Ok(())
        }
        Ty::Bytes => ByteSize::parse(s, key).map(|_| ()).map_err(|e| EsError::illegal_argument(e.reason()).caused_by(e)),
        Ty::Str | Ty::List => Ok(()),
        Ty::Check(base, check) => {
            validate_value_inner(&IndexSetting { ty: *base, ..*def }, &SettingValue::Str(s.to_string()))?;
            check(s).map_err(EsError::illegal_argument)
        }
        Ty::Enum(options, err) => {
            if options.contains(&s) {
                return Ok(());
            }
            match err {
                EnumErr::OneOf => {
                    let canonical: Vec<&str> = options.iter().copied().filter(|o| o.chars().all(|c| !c.is_ascii_uppercase())).collect();
                    let list = match key {
                        // The codecs Elasticsearch and OpenSearch list (the legacy and extra ones are accepted unlisted).
                        "index.codec" if options.contains(&"lz4") => "default, lz4, best_compression, zlib".to_string(),
                        "index.codec" => "default, best_compression".to_string(),
                        _ => canonical.join(", "),
                    };
                    Err(EsError::illegal_argument(format!("unknown value for [{key}] must be one of [{list}] but was: {s}")))
                }
                EnumErr::JavaEnum(class) => {
                    let upper = s.to_uppercase();
                    if options.contains(&upper.as_str()) {
                        return Ok(());
                    }
                    Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.{class}.{upper}")))
                }
                EnumErr::JavaEnumCaseInsensitive(class) => {
                    if options.contains(&s.to_ascii_lowercase().as_str()) {
                        return Ok(());
                    }
                    Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.{class}.{}", s.to_uppercase())))
                }
            }
        }
        Ty::AutoExpandReplicas => validate_auto_expand(s, key),
        Ty::ActiveShards => {
            if s == "all" || s == "ALL" || parse_java_long(s).is_some_and(|n| n >= 0) || s.is_empty() {
                return Ok(());
            }
            if parse_java_long(s).is_some() {
                return Err(EsError::illegal_argument("shard count cannot be a negative value".to_string()));
            }
            Err(EsError::illegal_argument(format!("cannot parse ActiveShardCount[{s}]")).caused_by(nfe(s)))
        }
    }
}

fn validate_auto_expand(s: &str, key: &str) -> Result<(), EsError> {
    if s == "false" {
        return Ok(());
    }
    let bad = || EsError::illegal_argument(format!("failed to parse [{key}] from value: [{s}] at index {}", s.find('-').map(|i| i as i64).unwrap_or(-1)));
    let Some(dash) = s.find('-') else { return Err(bad()) };
    let (min, max) = (&s[..dash], &s[dash + 1..]);
    let min_n = parse_java_long(min).ok_or_else(|| {
        EsError::illegal_argument(format!("failed to parse [{key}] from value: [{s}] at index {dash}")).caused_by(nfe(min))
    })?;
    if max != "all" {
        let max_n = parse_java_long(max).ok_or_else(|| {
            EsError::illegal_argument(format!("failed to parse [{key}] from value: [{s}] at index {dash}")).caused_by(nfe(max))
        })?;
        if min_n > max_n {
            return Err(EsError::illegal_argument(format!(
                "[{key}] minReplicas must be =< maxReplicas but wasn't {min_n} > {max_n}"
            )));
        }
    }
    Ok(())
}

/// Translog retention settings are rejected since Elasticsearch 8 (OpenSearch still accepts
/// and ignores them).
pub fn reject_translog_retention_in(d: Dialect, settings: &Settings) -> Result<(), EsError> {
    if d == Dialect::OpenSearch {
        return Ok(());
    }
    reject_translog_retention(settings)
}

/// Translog retention settings are rejected since Elasticsearch 8.
pub fn reject_translog_retention(settings: &Settings) -> Result<(), EsError> {
    let set = |k: &str| matches!(settings.get_value(k), Some(SettingValue::Str(_)) | Some(SettingValue::List(_)));
    if set("index.translog.retention.age") || set("index.translog.retention.size") {
        return Err(EsError::illegal_argument(
            "Translog retention settings [index.translog.retention.age] and [index.translog.retention.size] are no longer supported. Please do not specify values for these settings",
        ));
    }
    Ok(())
}

/// Validates user-provided index settings (already `index.`-prefixed) for index creation or a
/// template. Unknown keys fail first (all of them, as one error with `suppressed`), then values.
pub fn validate_user_settings(settings: &Settings, creating: bool) -> Result<(), EsError> {
    validate_user_settings_in(Dialect::Elasticsearch, settings, creating)
}

/// [`validate_user_settings`] against a dialect's registry.
pub fn validate_user_settings_in(d: Dialect, settings: &Settings, creating: bool) -> Result<(), EsError> {
    validate_keys_and_values(d, settings)?;
    if creating {
        reject_private(d, settings)?;
    }
    Ok(())
}

/// Private settings (managed by the server) can't be given at creation.
pub fn reject_private(d: Dialect, settings: &Settings) -> Result<(), EsError> {
    let private: Vec<&String> = settings.keys().filter(|k| lookup_in(d, k).is_some_and(|def| def.scope == Scope::Private)).collect();
    if !private.is_empty() {
        let errs: Vec<String> = private.iter().map(|k| format!("private index setting [{k}] can not be set explicitly")).collect();
        let mut e = EsError::action_request_validation(&errs);
        e.kind = "validation_exception".into();
        return Err(e);
    }
    Ok(())
}

/// The registry check a create-index request gets before anything else (even before the
/// index-exists check): unknown keys (all of them, as one error with `suppressed`), then
/// values.
pub fn validate_keys_and_values(d: Dialect, settings: &Settings) -> Result<(), EsError> {
    let mut unknown: Vec<EsError> = Vec::new();
    for (k, _) in settings.iter() {
        if k.starts_with("archived.") {
            continue;
        }
        match lookup_in(d, k) {
            Some(def) if def.scope == Scope::Internal => unknown.push(unknown_internal(k)),
            Some(_) => {}
            None if is_group_key(k) => {}
            None => unknown.push(unknown_setting_in(d, k)),
        }
    }
    if !unknown.is_empty() {
        if d == Dialect::OpenSearch {
            for e in unknown.iter_mut() {
                e.kind = "settings_exception".into();
            }
        }
        let first = unknown.remove(0);
        return Err(first.with_suppressed(unknown));
    }
    // The time series bounds are validated together: the end time's validator also reads the
    // start time, so an invalid start time is reported twice (the second as suppressed).
    let mut date_errors: Vec<EsError> = Vec::new();
    let bound = |k: &str| settings.get(k).map(parse_date_setting);
    if let Some(Err(e)) = bound("index.time_series.start_time") {
        date_errors.push(e);
    }
    if let Some(Err(e)) = bound("index.time_series.end_time") {
        date_errors.push(e);
    }
    if let Some(Err(e)) = bound("index.time_series.start_time") {
        date_errors.push(e);
    }
    if !date_errors.is_empty() {
        let first = date_errors.remove(0);
        return Err(first.with_suppressed(date_errors));
    }
    for (k, v) in settings.iter() {
        if let Some(def) = lookup_in(d, k) {
            validate_value_in(d, def, v)?;
        } else if k.starts_with("index.similarity.") {
            validate_similarity(k)?;
        }
        if k.starts_with("index.synthetic_id.") && settings.get("index.mapping.synthetic_id") != Some("true") {
            return Err(EsError::illegal_argument(format!("The setting [{k}] can only be set when [index.mapping.synthetic_id] is enabled.")));
        }
    }
    Ok(())
}

const BUILT_IN_SIMILARITIES: &[&str] = &["BM25", "boolean"];

fn validate_similarity(k: &str) -> Result<(), EsError> {
    let rest = &k["index.similarity.".len()..];
    let name = rest.split('.').next().unwrap_or(rest);
    if BUILT_IN_SIMILARITIES.contains(&name) {
        return Err(EsError::illegal_argument(format!("illegal value for [index.similarity.{name}] cannot redefine built-in similarity")));
    }
    Ok(())
}

/// The defaults section of `include_defaults=true`: every registered default not set in
/// `settings`.
pub fn defaults_for(settings: &Settings) -> Settings {
    defaults_for_in(Dialect::Elasticsearch, settings)
}

pub fn defaults_for_in(d: Dialect, settings: &Settings) -> Settings {
    let mut out = Settings::new();
    let mode = super::index_mode::IndexMode::of(settings);
    for def in registry(d) {
        if settings.contains(def.key) {
            continue;
        }
        if let Some(v) = mode.setting_default(def.key) {
            out.set(def.key, v);
            continue;
        }
        match def.default {
            Dflt::Str(s) => out.set(def.key, s),
            Dflt::List(l) => out.insert(def.key, SettingValue::List(l.iter().map(|s| s.to_string()).collect())),
            Dflt::Computed => match def.key {
                "index.number_of_routing_shards" => out.set(def.key, settings.get("index.number_of_shards").unwrap_or("1")),
                "index.max_rescore_window" => out.set(def.key, settings.get("index.max_result_window").unwrap_or("10000")),
                "index.sort.field" | "index.sort.order" | "index.sort.mode" | "index.sort.missing" => {
                    let d = super::index_sort::sort_defaults(settings);
                    let v = match def.key {
                        "index.sort.field" => d.fields,
                        "index.sort.order" => d.order,
                        "index.sort.mode" => d.mode,
                        _ => d.missing,
                    };
                    out.insert(def.key, SettingValue::List(v));
                }
                "index.version.compatibility" => {
                    out.set(def.key, settings.get("index.version.created").unwrap_or(crate::version::INDEX_VERSION_ID))
                }
                _ => {}
            },
            Dflt::None => {}
        }
    }
    out
}

/// The default value of a registered setting as a string (for typed accessors).
pub fn default_str(key: &str) -> Option<&'static str> {
    match lookup(key)?.default {
        Dflt::Str(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(pairs: &[(&str, &str)]) -> Settings {
        let mut s = Settings::new();
        for (k, v) in pairs {
            s.set(*k, *v);
        }
        s
    }

    #[test]
    fn registry_is_sorted() {
        for w in INDEX_SETTINGS.windows(2) {
            assert!(w[0].key < w[1].key, "{} >= {}", w[0].key, w[1].key);
        }
    }

    #[test]
    fn texts() {
        let e = validate_user_settings(&st(&[("index.number_of_shards", "0")]), true).unwrap_err();
        assert_eq!(e.reason(), "Failed to parse value [0] for setting [index.number_of_shards] must be >= 1");
        let e = validate_user_settings(&st(&[("index.number_of_shards", "x")]), true).unwrap_err();
        assert_eq!(e.reason(), "Failed to parse value [x] for setting [index.number_of_shards]");
        assert_eq!(e.caused_by.unwrap().reason(), "For input string: \"x\"");
        let e = validate_user_settings(&st(&[("index.number_of_shards", "1025")]), true).unwrap_err();
        assert_eq!(e.reason(), "Failed to parse value [1025] for setting [index.number_of_shards] must be <= 1024");
        let e = validate_user_settings(&st(&[("index.foo", "1"), ("index.bar", "2")]), true).unwrap_err();
        assert!(e.reason().starts_with("unknown setting [index.bar]"));
        assert_eq!(e.suppressed().len(), 1);
        let e = validate_user_settings(&st(&[("index.refresh_interval", "5")]), true).unwrap_err();
        assert_eq!(e.reason(), "failed to parse setting [index.refresh_interval] with value [5] as a time value: unit is missing or unrecognized");
        let e = validate_user_settings(&st(&[("index.codec", "zzz")]), true).unwrap_err();
        assert_eq!(e.reason(), "unknown value for [index.codec] must be one of [default, best_compression] but was: zzz");
        let e = validate_user_settings(&st(&[("index.mode", "zzz")]), true).unwrap_err();
        assert_eq!(e.reason(), "No enum constant org.elasticsearch.index.IndexMode.ZZZ");
        let e = validate_user_settings(&st(&[("index.uuid", "abc")]), true).unwrap_err();
        assert!(e.reason().starts_with("unknown setting [index.uuid]"));
        let e = validate_user_settings(&st(&[("index.version.created", "8000099")]), true).unwrap_err();
        assert_eq!(e.kind, "validation_exception");
        assert_eq!(e.reason(), "Validation Failed: 1: private index setting [index.version.created] can not be set explicitly;");
        assert!(validate_user_settings(&st(&[("index.analysis.analyzer.a.type", "standard"), ("index.routing.allocation.include.zone", "a")]), true).is_ok());
        let e = validate_user_settings(&st(&[("index.translog.sync_interval", "10ms")]), true).unwrap_err();
        assert_eq!(e.reason(), "failed to parse value [10ms] for setting [index.translog.sync_interval], must be >= [100ms]");
    }
}

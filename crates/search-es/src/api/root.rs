//! `GET /`, `HEAD /` and `GET /_capabilities`.

use crate::cluster::Dialect;
use crate::error::EsResult;
use crate::json::Value;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::{self, Method, Routed};
use crate::version::*;

pub fn info(ctx: &Ctx) -> EsResult<Reply> {
    let c = ctx.cluster;
    let body = match c.dialect {
        Dialect::Elasticsearch => obj! {
            "name" => c.node.name.clone(),
            "cluster_name" => c.name.clone(),
            "cluster_uuid" => c.uuid(),
            "version" => obj! {
                "number" => ES_VERSION,
                "build_flavor" => "default",
                "build_type" => "docker",
                "build_hash" => ES_BUILD_HASH,
                "build_date" => ES_BUILD_DATE,
                "build_snapshot" => false,
                "lucene_version" => LUCENE_VERSION,
                "minimum_wire_compatibility_version" => MIN_WIRE_COMPAT,
                "minimum_index_compatibility_version" => MIN_INDEX_COMPAT,
            },
            "tagline" => TAGLINE,
        },
        Dialect::OpenSearch => obj! {
            "name" => c.node.name.clone(),
            "cluster_name" => c.name.clone(),
            "cluster_uuid" => c.uuid(),
            "version" => obj! {
                "distribution" => "opensearch",
                "number" => OPENSEARCH_VERSION,
                "build_type" => "tar",
                "build_hash" => OPENSEARCH_BUILD_HASH,
                "build_date" => OPENSEARCH_BUILD_DATE,
                "build_snapshot" => false,
                "lucene_version" => OPENSEARCH_LUCENE_VERSION,
                "minimum_wire_compatibility_version" => "2.19.0",
                "minimum_index_compatibility_version" => "2.0.0",
            },
            "tagline" => OPENSEARCH_TAGLINE,
        },
    };
    Reply::ok(body)
}

/// `HEAD /`: Elasticsearch answers with the `GET /` response's headers (and length).
pub fn ping(ctx: &Ctx) -> EsResult<Reply> {
    info(ctx)
}

/// Cluster features published in `_cluster/state` `nodes_features` (the YAML runner reads them).
/// These are the features a production Elasticsearch 9.5.5 publishes, so the YAML suites run
/// (and skip) the same tests against roll as against it.
pub const NODE_FEATURES: &[&str] = &[
    "ES_V_8", "ES_V_9", "aggregate_metric_double.default_metric.deprecated", "certificate_identity_field", "cluster.reroute.ignores_metric_param",
    "cluster.stats.source_modes", "data_stream.failure_store", "data_stream.modify.delete_index", "data_stream.time_series.past_index_creation",
    "downsample.cumulative_histogram_consecutive_resets_fix", "inference.auth_poller.persistent_task", "inference.ccm.cache",
    "inference.ccm.enablement_service", "inference.clear_preferences_cache", "inference.elastic.reasoning_task_settings",
    "inference.embedding_task_type", "inference.endpoint.cache", "inference.inference_index_doc_type", "inference.internal_delete_endpoints_action",
    "inference.metadata_field", "inference.oauth2.token_cache", "ingest.field_access_pattern", "linear_retriever_supported", "lucene_10_1_upgrade",
    "lucene_10_upgrade", "mapper.tsdb.metric_temporality_support", "ml.components.reset", "ml.info.model_platform_variant",
    "reindex_management_endpoints", "reindex_pit_search", "reindex_relocate_on_shutdown", "resolve_index_returns_mode",
    "security.primary_encryption_key", "security.queryable_built_in_roles", "security_stats_endpoint", "simulate.ignored.fields",
    "snapshots.get.state_parameter", "synonyms.multiple_sets_per_filter", "vectors.indexing.gpu_monitoring",
];

/// Elasticsearch 9.5.5's test features (`FeatureSpecification.getTestFeatures()` of every module,
/// with the feature flags Elasticsearch's YAML test clusters enable). Elasticsearch publishes them
/// only with `-Dtests.testfeatures.enabled=true`, as the clusters its YAML REST suites run
/// against do; roll publishes them when [`crate::ClusterConfig::test_features`] is set, so the
/// suites run the same tests against roll.
pub const TEST_FEATURES: &[&str] = &[
    "analysis.icu_transform.custom_ruleset", "closing_invalid_pit_id", "cluster.stats.multiple_synonym_graph_filters",
    "columnar.enabled_setting", "constant_field_type.normalized_wildcard_query_support",
    "data_stream.downsample.default_aggregate_metric_fix", "data_stream.downsample.fix_support_multi_value_dimensions",
    "data_stream.failure_store.tsdb_fix", "data_stream.mappings_api", "embedding_query_vector_builder", "field_caps.inference_field",
    "ids_query_limit_max_ids", "index.logsdb_no_host_name_field", "index.merge_scheduler_clamps_max_thread_count",
    "index.shadowing_dimensions_and_metrics_is_valid_in_non_tsdb", "index.slice_indexing", "index.synonyms_set_lenient_on_non_existing",
    "index.throw_exception_for_unknown_token_in_rest_index_put_alias_action",
    "index.throw_exception_on_index_creation_if_unsupported_value_type_in_alias", "index.time_series_es95_codec",
    "index.time_series_no_seqno", "index.time_series_synthetic_id", "index.time_series_synthetic_id_default",
    "indices.put_mapping.no_types_check", "inference.cohere.v2", "inference_fields.get_via_source_exclude_vectors",
    "inference_fields.get_via_source_includes", "inference.semantic_text_usage", "ingest.append.copy_from",
    "ingest.append.ignore_empty_values", "ingest.append.ignore_empty_values_fix", "ingest.attachment.max_field_size_feature",
    "ingest.dynamic_strings.auto_text_setting", "inner_retrievers_filter_support", "linear_retriever.l2_norm",
    "linear_retriever.minmax_single_doc_fix", "linear_retriever_minscore_fix", "linear_retriever.multi_fields_query_format_support",
    "linear_retriever.multi_index_simplified_format_support", "linear_retriever.top_level_normalizer",
    "logs_data_streams.failure_store.enabled", "logs_stream", "lucene_10_4_upgrade", "mapper.analyzer-wrapper.reloadable_search_analyzer",
    "mapper.array_objects_limit", "mapper.base64_dense_vectors", "mapper.bbq_disk_stats_support", "mapper.bbq_disk_support",
    "mapper.columnar.accepts_subobjects_false", "mapper.columnar.drops_dynamic_false_fields",
    "mapper.columnar.inline_array_order_binary_doc_values", "mapper.columnar.ip_inline_array_order_binary_doc_values",
    "mapper.columnar.maintain_array_order", "mapper.columnar.maintain_array_order_ip_text", "mapper.columnar_rejects_runtime_dynamic",
    "mapper.columnar.store_not_allowed", "mapper.columnar.supports_shape_fields", "mapper.constant_keyword.synthetic_source_write_fix",
    "mapper.copy_to.dynamic_handling", "mapper.counted_keyword.synthetic_source_native_support",
    "mapper.dense_vector.default_oversample_value_for_bbq", "mapper.dense_vector.dynamic_template_dotted_field_fix",
    "mapper.dense_vector.dynamic_template_nested_object_fix", "mapper.dense_vector.rescore_vector",
    "mapper.dense_vector.rescore_zero_vector", "mapper.doc_values.extended_form_only_in_columnar",
    "mapper.doc_values.ignored_values_in_binary_dv", "mapper.doc_values.multi_value", "mapper.doc_values.multi_value_enforcement",
    "mapper.doc_values.multi_value_false_alias", "mapper.doc_values.multi_value_index_setting", "mapper.doc_values.multi_value_rename",
    "mapper.doc_values.nullability", "mapper.doc_values_skipper", "mapper.es940_disk_bbq", "mapper.exclude_vectors_docvalue_bugfix",
    "mapper.exponential_histogram_type", "mapper.fix_dense_vector_wrong_fields", "mapper.fix_parsing_subobjects_false_dynamic_false",
    "mapper.flattened.mapped_subfields", "mapper.flattened.passthrough", "mapper.icu_collation_keyword.supports_binary_dv",
    "mapper.id_field.mode_mapping_attribute", "mapper.ignored_source.always_store_object_arrays_in_nested",
    "mapper.ignored_source_as_top_level_metadata_array_field", "mapper.ignored_source.dont_expand_dots",
    "mapper.ignored_source_fields_per_entry", "mapper.ignore_dynamic_field_names_beyond_limit", "mapper.ip.doc_values_cardinality_option",
    "mapper.ivf_format_cluster_feature", "mapper.ivf_nested_support", "mapper.keyword.columnar_default_high_cardinality",
    "mapper.keyword.doc_values_case_insensitive_regexp", "mapper.keyword.high_cardinality_length_function_fuse_to_load",
    "mapper.keyword.multi_fields_not_stored_when_ignored", "mapper.keyword.mv_max_function_fuse_to_load",
    "mapper.keyword.mv_min_function_fuse_to_load", "mapper.keyword.normalizer_skip_store_setting",
    "mapper.keyword.store_high_cardinality_in_binary_doc_values", "mapper.logsdb_default_ignore_dynamic_beyond_limit",
    "mapper.match_only_text_block_loader_fix", "mapper.match_only_text.doc_values_prefix_wildcard_regexp",
    "mapper.multi_field.unicode_optimisation_fix", "mapper.nested.sorting_fields_check_fix", "mapper.npe_on_dims_update_fix",
    "mapper.patterned_text", "mapper.pattern_text_rename", "mapper.provide_index_sort_setting_defaults",
    "mapper.range.date_range_indexing_fix", "mapper.range.invalid_date_fix", "mapper.rank_vectors.dynamic_template_dotted_field_fix",
    "mapper.routing_as_doc_values", "mapper.routing_as_doc_values_by_default", "mapper.search_load_per_shard",
    "mapper.skippers_on_unindexed_fields", "mapper.source.mode_from_index_setting", "mapper.source.remove_synthetic_source_only_validation",
    "mapper.sparse_vector.store_support", "mapper.stored_fields_spec_merge_bug", "mapper.subobjects_false_mapping_update_fix",
    "mapper.synthetic_recovery_source", "mapper.tdigest_type", "mapper.text.doc_values", "mapper.text.doc_values_prefix_wildcard_regexp",
    "mapper.text_fields.enable_doc_values_by_default_in_columnar_mode", "mapper.tsdb_nested_field_support",
    "mapper.unknown_field_mapping_update_error_message", "mapper.vectors.diskbbq_on_disk_rescoring", "mapper.vectors.generic_vector_format",
    "meta_fetch_fields_error_code_changed", "nested_agg_top_hits_with_inner_hits", "nested_retriever_min_score_total_hits_fix",
    "pinned_retriever_supported", "prometheus_plugin", "query_rules_fix_multiple_exclude_rules", "query_rules.numeric_validation",
    "random_sampling", "rank_vectors", "resolve_index_returns_mode", "retriever.mmr_null_dense_vector_fix",
    "retriever.negative_rank_window_size_fix", "retriever.result_diversification_mmr", "routing.logsb_route_on_sort_fields",
    "rrf_retriever.multi_fields_query_format_support", "rrf_retriever.multi_index_simplified_format_support",
    "rrf_retriever.simplified_weighted_support", "rrf_retriever.weighted_support", "search.aggs.composite.doc_values_skipper_fix",
    "search.aggs.date_histogram.hard_bounds_outside_data_fix", "search.completion_field.duplicate.support", "search.count.stats_parameter",
    "search.default_disk_bbq", "search.exponential_histogram_querydsl_boxplot", "search.exponential_histogram_querydsl_min_max",
    "search.exponential_histogram_querydsl_percentile_ranks", "search.exponential_histogram_querydsl_percentiles",
    "search.exponential_histogram_querydsl_range", "search.exponential_histogram_upscaling_removed",
    "search.fetch_fields.excludes_non_metadata_type", "search.function_score.named_queries", "search.indices_boost_remote_index_fix",
    "search.multi.match.checks.positions", "search.negative.function.score.bad.request", "search.new_semantic_query_interceptors",
    "search.profile.coordinator_request_metadata", "search.rescorer.missing.field.bad.request", "search.rescore.script",
    "search.retriever.rescorer.enabled", "search.scroll.empty_context_returns_200", "search.semantic_knn_filter_fix",
    "search.semantic_knn_vector_query_filters_rewrite_interception_supported",
    "search.semantic_knn_vector_query_rewrite_interception_supported", "search.semantic_match_query_rewrite_interception_supported",
    "search.semantic_sparse_vector_query_rewrite_interception_supported", "search.sort.date_format_missing_as_null",
    "search.sort.int_sort_for_int_short_byte_fields", "search.usage.extended_data", "search.vectors.flat_index_threshold",
    "search.vectors.lookup_query_vector_builder", "search.vectors.mappers.default_bbq_hnsw",
    "search.vectors.nested_knn_inner_hits_match_query_phase_scoring", "search.vectors.no_dimensions_bugfix",
    "semantic.exists_query_auto_prefilter_fix", "semantic_field.semantic_field_mapper", "semantic_query.filter_field_caps_fix",
    "semantic_query.multiple_inference_ids", "semantic_query_rewrite_interceptors.propagate_boost_and_query_name_fix",
    "semantic_text.always_emit_inference_id_fix", "semantic_text.auto_prefiltering", "semantic_text.bfloat16_support",
    "semantic_text.bit_vector_support", "semantic_text.delete_fix", "semantic_text.element_type_in_index_options",
    "semantic_text.exclude_sub_fields_from_field_caps", "semantic_text.fields_chunks_format", "semantic_text.handle_empty_input",
    "semantic_text.highlighter", "semantic_text.highlighter.bbq_and_similarity_support", "semantic_text.highlighter.default",
    "semantic_text.highlighter.flat_index_options", "semantic_text.highlighter.vector_similarity_support", "semantic_text.index_options",
    "semantic_text.index_options_with_defaults", "semantic_text.inference_metadata_fields.enabled_by_default",
    "semantic_text.inference_metadata_fields.explicit_null_fixes", "semantic_text.inference_using_embedding_task",
    "semantic_text.in_object_field_fix", "semantic_text.match_all_highlighter", "semantic_text.original_values_in_doc_values",
    "semantic_text.prevent_legacy_format_new_indices", "semantic_text.single_field_update_fix", "semantic_text.skip_inference_fields",
    "semantic_text.sparse_vector_index_options", "semantic_text.support_chunking_config", "semantic_text.updatable_inference_id",
    "semantic_text.zero_size_fix", "simulate.ingest.400_on_failure", "sparse_vector.index_options_supported",
    "standard_retriever.fix_for_empty_query_in_compound_retrievers", "synonyms.append", "synonyms.get.search_after",
    "synonyms.max_synonym_rules", "synonyms_set.get.return_empty_synonym_sets", "test_features_enabled",
    "test_reranking_service.parse_text_as_score", "test_rule_retriever.with_indices_that_dont_return_rank_docs",
    "text_embedding_query_vector_builder.used_by.result_diversification_mmr_retriever", "text_similarity_rank_doc_explain_chunks",
    "text_similarity_reranker_alias_handling_fix", "text_similarity_reranker.comprehensive_top_n_handling",
    "text_similarity_reranker_empty_result_fix", "text_similarity_reranker_inference_id_chunking", "text_similarity_reranker_minscore_fix",
    "text_similarity_reranker_snippets", "vectordb_document.usage", "vectors.indexing.use_gpu",
];

/// The features the node publishes (sorted, as Elasticsearch lists them).
pub fn node_features(config: &crate::cluster::ClusterConfig) -> Vec<&'static str> {
    let mut f = NODE_FEATURES.to_vec();
    if config.test_features {
        f.extend_from_slice(TEST_FEATURES);
        f.sort_unstable();
        f.dedup();
    }
    f
}

/// Named capabilities each API supports (`?capabilities=`), exactly as Elasticsearch 9.5.5's
/// REST handlers declare them (`supportedCapabilities()`). They only count for APIs roll
/// implements (see [`capabilities`]).
fn api_capabilities(api: routes::Api) -> &'static [&'static str] {
    use routes::Api::*;
    match api {
        ClusterHealth | CatHealth => &["unassigned_pri_shard_count"],
        ClusterAllocationExplain => &["query_parameter_support"],
        IndicesClose => &["wait-for-active-shards-index-setting-removed"],
        IndicesCreate => &[
            "logsdb_index_mode",
            "lookup_index_mode",
            "nested_dense_vector_synthetic_test",
            "poorly_formatted_bad_request",
            "hunspell_dict_400",
            "disable_sequence_numbers",
            "reject_runtime_field_shadowing_sort_field",
            "columnar_index_modes",
            "vectordb_document_index_mode",
        ],
        IndicesPutMapping => &["reject_runtime_field_shadowing_sort_field"],
        IndicesAnalyze => &["wrong_custom_analyzer_returns_400", "analyze_max_char_count"],
        IndicesPutIndexTemplate => &[
            "data_stream_options.failure_store",
            "data_stream_options.failure_store.lifecycle",
            "index_template_tracking_info",
            "dlm.downsampling_method",
            "dlm.frozen_after",
            "failure_store.lifecycle.frozen_after_rejected",
        ],
        ClusterPutComponentTemplate => &[
            "data_stream_options.failure_store",
            "data_stream_options.failure_store.lifecycle",
            "component_template_tracking_info",
            "dlm.downsampling_method",
            "dlm.frozen_after",
            "failure_store.lifecycle.frozen_after_rejected",
        ],
        IndicesUpdateAliases => &["fix_filtered_data_stream_alias_removal"],
        IndicesRollover => &["return-404-on-missing-target", "index_expression_selectors", "max_size_deprecation"],
        IndicesResolveIndex => &["mode_filter"],
        IngestPutPipeline => &["pipeline_tracking_info", "field_access_pattern.flexible"],
        CatNodes => &["available_processors_availability"],
        CatSegments => &["allow_closed"],
        ClusterStats => &[
            "human-readable-total-docs-size",
            "verbose-dense-vector-mapping-stats",
            "ccs-stats",
            "retrievers-usage-stats",
            "esql-stats",
            "extended-search-usage-stats",
        ],
        ClusterPutSettings => &["repositories.default_repository_setting"],
        NodesStats => &["dense_vector_off_heap_stats"],
        ClusterPostVotingConfigExclusions | ClusterDeleteVotingConfigExclusions => &["plain_text_empty_response"],
        InternalDeleteDesiredNodes | InternalDeleteDesiredBalance => &["plain_text_empty_response"],
        InternalGetDesiredBalance => &["desired_balance_node_weights_in_response"],
        StreamsStatus => &["stream_split"],
        IndicesGetIndexTemplate | ClusterGetComponentTemplate | IndicesGetTemplate => &["local_param_deprecated"],
        SynonymsPutSynonym | SynonymsPutSynonymRule | SynonymsDeleteSynonymRule => &["synonyms_refresh_param"],
        IndicesGetDataStream => &["data_stream_lifecycle_effective_retention", "failure_store.lifecycle", "failure_store.lifecycle.default_retention"],
        IndicesGetDataStreamOptions => &["failure_store.lifecycle"],
        IndicesPutDataStreamOptions => &["failure_store.lifecycle", "failure_store.lifecycle.frozen_after_rejected"],
        IndicesGetDataLifecycle => &["data_stream_lifecycle_effective_retention", "data_stream_global_retention"],
        IndicesExplainDataLifecycle => &["data_stream_lifecycle_effective_retention"],
        IndicesPutDataLifecycle => &["dlm.downsampling_method", "dlm.frozen_after"],
        IndicesModifyDataStream => &["data_streams.modify.delete_index"],
        _ => &[],
    }
}

/// `GET /_capabilities`: whether an endpoint (with parameters and capabilities) is supported.
/// APIs that route but aren't implemented report `false`, so test suites skip them.
pub fn capabilities(ctx: &Ctx) -> EsResult<Reply> {
    let method = ctx.query("method").unwrap_or("GET").to_ascii_uppercase();
    let path = ctx.query("path").unwrap_or("/");
    let mut supported = true;
    let mut api = None;
    if let Some(m) = Method::parse(&method)
        && let Routed::Found(r) = routes::route(m, path)
    {
        api = Some(r.api);
        supported = implemented(r.api);
        if let Some(params) = ctx.query("parameters") {
            for p in params.split(',').filter(|p| !p.is_empty()) {
                let known = r.api.param(p).is_some() || r.params.iter().any(|(n, _)| *n == p) || matches!(p, "pretty" | "human" | "error_trace" | "filter_path" | "format");
                supported &= known;
            }
        }
    }
    if let Some(caps) = ctx.query("capabilities") {
        for c in caps.split(',').filter(|c| !c.is_empty()) {
            supported &= api.is_some_and(|a| api_capabilities(a).contains(&c));
        }
    }
    Reply::ok(obj! {
        "_nodes" => obj! { "total" => 1, "successful" => 1, "failed" => 0 },
        "cluster_name" => ctx.cluster.name.clone(),
        "supported" => Value::Bool(supported),
    })
}

/// Whether roll search implements an API (rather than answering "not supported").
pub fn implemented(api: routes::Api) -> bool {
    super::implemented(api)
}

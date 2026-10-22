//! Index APIs: create, delete, exists, get, mappings, settings, open/close, blocks, stats,
//! resolve.

use super::{index_expressions, resolve_indices, warn_system_access};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::mapping::{self, Mapping, MappingCtx, MapperRef, MergeReason};
use crate::meta::index_mode::IndexMode;
use crate::units::TimeValue;
use crate::meta::blocks::{self, Level};
use crate::meta::index_settings::{self, Scope};
use crate::meta::names::{self, random_uuid};
use crate::meta::resolve::{self, IndicesOptions};
use crate::meta::settings::{SettingValue, Settings};
use crate::meta::{AliasMeta, ClusterState, IndexMeta, IndexState, templates};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;
use std::collections::BTreeMap;
use std::sync::Arc;

/// `MetadataCreateIndexService.calculateNumRoutingShards`.
pub fn routing_num_shards(shards: u32) -> u32 {
    let log2_shards = 32 - (shards.saturating_sub(1)).leading_zeros();
    let splits = (10i32 - log2_shards as i32).max(1);
    shards << splits
}

/// The parsed body of a create-index request.
#[derive(Default)]
pub struct CreateRequest {
    pub settings: Settings,
    pub mappings: Option<Map>,
    pub aliases: Vec<(String, AliasMeta)>,
}

pub fn parse_create_body(body: Option<Map>) -> EsResult<CreateRequest> {
    let mut r = CreateRequest::default();
    let Some(body) = body else { return Ok(r) };
    for (k, v) in &body {
        match k.as_str() {
            "settings" => {
                if !matches!(v, Value::Obj(_)) {
                    return Err(EsError::parse("key [settings] must be an object"));
                }
                r.settings = Settings::from_json(v)?;
            }
            "mappings" => match v {
                Value::Obj(m) => r.mappings = Some(m.clone()),
                _ => return Err(EsError::parse("key [mappings] must be an object")),
            },
            "aliases" => {
                if !matches!(v, Value::Obj(_)) {
                    return Err(EsError::parse("key [aliases] must be an object"));
                }
                r.aliases = crate::meta::aliases::parse_aliases(v)?;
            }
            other => return Err(EsError::parse(format!("unknown key [{other}] for create index"))),
        }
    }
    // A body that is only settings (`{"number_of_shards": 1}`) is accepted by Elasticsearch's
    // parser only under `settings`; anything else was rejected above.
    r.settings.normalize_prefix("index.");
    Ok(r)
}

/// What a backing index of a data stream is created from.
pub struct BackingOf<'a> {
    pub data_stream: &'a str,
    pub template: &'a templates::ComposableTemplate,
    /// Data-stream-level settings and mappings overrides.
    pub settings: &'a Settings,
    pub mappings: Option<&'a Map>,
}

/// The `@timestamp` mapping data stream backing indices start from (lowest precedence), with
/// `_routing.required` following the template's `allow_custom_routing`.
pub fn default_timestamp_mapping(custom_routing: bool) -> Map {
    // Elasticsearch's snippet also spells out `_routing.required: false`, the default, which is
    // left out here; `ignore_malformed: false` matters where the index default is `true` (logs).
    let mut m = Map::new();
    if custom_routing {
        m.insert("_routing".into(), obj! { "required" => true });
    }
    m.insert("properties".into(), obj! { "@timestamp" => obj! { "type" => "date", "ignore_malformed" => false } });
    m
}

/// The `_data_stream_timestamp` snippet every backing index ends with (highest precedence).
pub fn data_stream_mapping_snippet() -> Map {
    match obj! { "_data_stream_timestamp" => obj! { "enabled" => true } } {
        Value::Obj(m) => m,
        _ => unreachable!(),
    }
}

/// The mapping of failure store indices (`DataStreamFailureStoreDefinition`): the failed
/// document's identity and the error, nothing dynamic.
fn failure_store_mapping() -> Map {
    let kw = || obj! { "type" => "keyword" };
    let m = obj! {
        "dynamic" => "false",
        "_data_stream_timestamp" => obj! { "enabled" => true },
        "properties" => obj! {
            "@timestamp" => obj! { "type" => "date" },
            "document" => obj! { "properties" => obj! { "id" => kw(), "index" => kw(), "routing" => kw() } },
            "error" => obj! { "properties" => obj! {
                "message" => obj! { "type" => "match_only_text" },
                "pipeline" => kw(),
                "pipeline_trace" => kw(),
                "processor_tag" => kw(),
                "processor_type" => kw(),
                "stack_trace" => obj! { "type" => "text" },
                "type" => kw(),
            } },
        },
    };
    match m {
        Value::Obj(m) => m,
        _ => unreachable!(),
    }
}

/// Creates a failure store index of a data stream: the template's settings (not its mappings),
/// hidden, with the failure store mapping.
pub fn create_failure_index(st: &mut ClusterState, name: &str, template: &templates::ComposableTemplate, ds_settings: &Settings, now_ms: i64) -> EsResult<()> {
    let composed = templates::compose(&st.metadata, template)?;
    let mut req = CreateRequest { settings: composed.settings, ..CreateRequest::default() };
    req.settings.merge(ds_settings);
    // Settings that only make sense for the data stream's own backing indices.
    for k in ["index.mode", "index.routing_path", "index.sort.field", "index.sort.order", "index.sort.mode", "index.sort.missing", "index.lifecycle.name", "index.default_pipeline", "index.final_pipeline"] {
        req.settings.remove(k);
    }
    req.settings.set("index.hidden", "true");
    req.settings.set("index.failure_store.version", "1");
    if !req.settings.contains("index.routing.allocation.include._tier_preference") {
        req.settings.set("index.routing.allocation.include._tier_preference", "data_hot");
    }
    req.mappings = Some(failure_store_mapping());
    create_index_core(st, name, name, req, now_ms, None).map(|_| ())
}

/// The settings Elasticsearch's index settings providers add to a data stream's new backing
/// index: logsdb for `logs-*-*` streams, TSDB time bounds and dimensions, logsdb host sorting.
pub fn data_stream_index_settings(md: &crate::meta::Metadata, data_stream: &str, settings: &mut Settings, mappings: &[Map], now_ms: i64) -> EsResult<()> {
    // `logs-*-*` data streams default to logsdb (`cluster.logsdb.enabled`).
    if !settings.contains("index.mode") && logs_pattern(data_stream) && cluster_bool(md, "cluster.logsdb.enabled", true) {
        settings.set("index.mode", "logsdb");
    }
    let mode = IndexMode::of(settings);
    if mode == IndexMode::TimeSeries {
        tsdb_backing_settings(md, data_stream, settings, mappings, now_ms)?;
    } else if mode.is_logs() {
        logsdb_backing_settings(settings, mappings)?;
    }
    Ok(())
}

/// Whether a data stream name matches `logs-*-*`.
fn logs_pattern(name: &str) -> bool {
    crate::xcontent::filter::glob_match("logs-*-*", name)
}

/// A boolean cluster setting (transient over persistent over the default).
fn cluster_bool(md: &crate::meta::Metadata, key: &str, default: bool) -> bool {
    md.transient.get(key).or_else(|| md.persistent.get(key)).map(|v| v == "true").unwrap_or(default)
}

/// `LogsdbIndexModeSettingsProvider` for data stream backing indices in a logs mode: sort on
/// `host.name` when it is (or can be added as) a keyword or numeric field with doc values.
fn logsdb_backing_settings(settings: &mut Settings, mappings: &[Map]) -> EsResult<()> {
    if settings.contains("index.sort.field") {
        return Ok(());
    }
    let flag = |s: &Settings, k: &str| s.get(k) == Some("true");
    let ctx = MappingCtx::from_settings(settings);
    let mut merged = Mapping::empty();
    for src in mappings {
        // Invalid mappings fail later, at the real merge.
        let Ok(m) = Mapping::parse(&mapping::unwrap_type(src), &ctx) else { return Ok(()) };
        let Ok(next) = merged.merge(&m, &ctx, mapping::MergeReason::IndexTemplate) else { return Ok(()) };
        merged = next;
    }
    let host_name = merged.lookup("host.name");
    let add_host = flag(settings, "index.logsdb.add_host_name_field")
        || (host_name.is_none() && (merged.lookup("host").is_none() || !merged.root.subobjects_enabled()));
    let sortable = match host_name {
        Some(MapperRef::Field(f)) => {
            let numeric = matches!(f.ty, "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float" | "unsigned_long");
            (f.ty == "keyword" || numeric) && f.bool_param("doc_values", &ctx)
        }
        _ => false,
    };
    let sort_on_host = flag(settings, "index.logsdb.sort_on_host_name") || add_host || sortable;
    if sort_on_host {
        if add_host {
            settings.set("index.logsdb.add_host_name_field", "true");
        }
        settings.set("index.logsdb.sort_on_host_name", "true");
    }
    Ok(())
}

/// `DataStreamIndexSettingsProvider`: what a time series data stream's backing index gets on
/// top of its template — its time bounds (from now, or continuing the previous write index), no
/// sequence numbers, synthetic ids, and the dimensions to route on (`index.dimensions`, or
/// `index.routing_path` when dynamic templates make dimensions).
fn tsdb_backing_settings(md: &crate::meta::Metadata, data_stream: &str, settings: &mut Settings, mappings: &[Map], now_ms: i64) -> EsResult<()> {
    let time = |key: &str, default: TimeValue, max: Option<TimeValue>| -> EsResult<TimeValue> {
        let t = match settings.get(key) {
            Some(v) => TimeValue::parse(v, key)?,
            None => default,
        };
        Ok(match max {
            Some(m) if t.as_millis() > m.as_millis() => m,
            _ => t,
        })
    };
    let look_ahead = time("index.look_ahead_time", TimeValue::minutes(9), Some(TimeValue::hours(2)))?;
    let look_back = time("index.look_back_time", TimeValue::hours(2), None)?;
    let truncate = |ms: i64| ms.div_euclid(1000) * 1000;
    let fmt = crate::time::DateFormatter::new("strict_date_optional_time").map_err(EsError::illegal_argument)?;
    let ds = md.data_streams.get(data_stream);
    let migrating = ds.is_some_and(|d| d.index_mode.as_deref().is_none_or(|m| m.eq_ignore_ascii_case("standard")));
    let (start, end) = match ds.and_then(|d| d.write_index()).and_then(|w| md.indices.get(w)) {
        Some(write) if !migrating => {
            let Some(prev_end) = write.settings.get("index.time_series.end_time") else {
                return Err(EsError::illegal_state(format!(
                    "backing index [{}] in tsdb mode doesn't have the [index.time_series.end_time] index setting",
                    write.name
                )));
            };
            let start = fmt.parse_millis(prev_end).map_err(EsError::illegal_argument)?;
            let end = if start > now_ms { truncate(start + look_ahead.as_millis()) } else { truncate(now_ms + look_ahead.as_millis()) };
            (start, end)
        }
        _ => (truncate(now_ms - look_back.as_millis()), truncate(now_ms + look_ahead.as_millis())),
    };
    settings.set("index.time_series.start_time", fmt.format_millis(start));
    settings.set("index.time_series.end_time", fmt.format_millis(end));
    if !settings.contains("index.disable_sequence_numbers") {
        settings.set("index.disable_sequence_numbers", "true");
    }
    if !settings.contains("index.mapping.synthetic_id") {
        settings.set("index.mapping.synthetic_id", "true");
    }
    if !settings.contains("index.routing_path") && !mappings.is_empty() {
        let ctx = MappingCtx::from_settings(settings);
        let mut merged = Mapping::empty();
        for src in mappings {
            let m = Mapping::parse(&mapping::unwrap_type(src), &ctx).map_err(|e| ctx.wrap_parse_error(e))?;
            merged = merged.merge(&m, &ctx, mapping::MergeReason::IndexTemplate)?;
        }
        let (dimensions, all) = merged.dimension_fields(&ctx);
        if !dimensions.is_empty() {
            let key = if all && settings.get("index.dimensions_tsid_strategy_enabled") != Some("false") { "index.dimensions" } else { "index.routing_path" };
            settings.insert(key, SettingValue::List(dimensions));
        }
    }
    Ok(())
}

/// Creates an index in `st` (templates applied). Returns warnings.
pub fn create_index(st: &mut ClusterState, provided_name: &str, name: &str, req: CreateRequest, now_ms: i64) -> EsResult<Vec<String>> {
    create_index_core(st, provided_name, name, req, now_ms, None)
}

/// Creates a data stream's backing index from its template (the request carries only
/// `index.hidden`).
pub fn create_backing_index(st: &mut ClusterState, name: &str, backing: BackingOf<'_>, now_ms: i64) -> EsResult<Vec<String>> {
    let mut req = CreateRequest::default();
    req.settings.set("index.hidden", "true");
    create_index_core(st, name, name, req, now_ms, Some(backing))
}

fn create_index_core(st: &mut ClusterState, provided_name: &str, name: &str, req: CreateRequest, now_ms: i64, backing: Option<BackingOf<'_>>) -> EsResult<Vec<String>> {
    names::validate_index_name(name)?;
    // The dot-prefix check is a filter on the create index action; data stream backing
    // indices are created by other actions and never warned about.
    let mut warnings = if backing.is_none() { names::index_name_warnings(name) } else { Vec::new() };
    // The request's settings are checked against the registry before the index's existence.
    index_settings::validate_keys_and_values(st.dialect, &req.settings)?;
    if let Some(existing) = st.metadata.indices.get(name) {
        return Err(EsError::index_already_exists(name, &existing.uuid));
    }
    if st.has_alias(name) || st.metadata.data_stream_aliases.contains_key(name) {
        return Err(EsError::invalid_index_name(name, "already exists as alias"));
    }
    index_settings::reject_private(st.dialect, &req.settings)?;
    index_settings::reject_translog_retention_in(st.dialect, &req.settings)?;
    if req.settings.get("index.soft_deletes.enabled") == Some("false") {
        return Err(EsError::illegal_argument(
            "Creating indices with soft-deletes disabled is no longer supported. Please do not specify a value for setting [index.soft_deletes.enabled].",
        ));
    }
    let mut req = req;
    // System indices are hidden.
    if names::is_system_index(name) && !req.settings.contains("index.hidden") {
        req.settings.set("index.hidden", "true");
    }
    let hidden = req.settings.get("index.hidden") == Some("true");
    if backing.is_none()
        && let Some((tname, t)) = templates::find_composable(&st.metadata, name, hidden)
        && t.data_stream.is_some()
    {
        return Err(EsError::illegal_argument(format!(
            "cannot create index with name [{name}], because it matches with template [{tname}] that creates data streams only, use create data stream api instead"
        )));
    }
    let mut composed = match &backing {
        Some(b) => templates::compose(&st.metadata, b.template)?,
        None => templates::for_new_index(&st.metadata, name, hidden)?,
    };
    if let Some(b) = &backing {
        // Data stream aliases are the data stream's, not its backing indices'.
        composed.aliases.clear();
        composed.settings.merge(b.settings);
        let custom_routing = b.template.data_stream.as_ref().and_then(|d| d.get("allow_custom_routing")).and_then(Value::as_bool).unwrap_or(false);
        composed.mappings.insert(0, default_timestamp_mapping(custom_routing));
        if let Some(m) = b.mappings {
            composed.mappings.push(m.clone());
        }
        composed.mappings.push(data_stream_mapping_snippet());
    }
    if backing.is_none() && templates::find_composable(&st.metadata, name, hidden).is_none() {
        let legacy = templates::find_legacy(&st.metadata, name, hidden);
        if legacy.len() > 1 {
            let mut names: Vec<&str> = legacy.iter().map(|(n, _)| n.as_str()).collect();
            names.sort_unstable();
            warnings.push(format!(
                "index [{name}] matches multiple legacy templates [{}], composable templates will only match a single template",
                names.join(", ")
            ));
        }
    }

    // Settings: templates, then the request.
    let mut settings = composed.settings.clone();
    // Explicit `null`s in the request stay in the index's settings (Elasticsearch prints them).
    settings.overlay_keeping_nulls(&req.settings);
    if !settings.contains("index.number_of_shards") {
        settings.set("index.number_of_shards", "1");
    }
    if !settings.contains("index.number_of_replicas") {
        settings.set("index.number_of_replicas", "1");
    }
    let shards: u32 = settings.get("index.number_of_shards").and_then(|s| s.parse().ok()).unwrap_or(1);
    if settings.get("index.mode").is_some_and(|m| m.eq_ignore_ascii_case("lookup")) && shards != 1 {
        return Err(EsError::illegal_argument(format!(
            "index with [lookup] mode must have [index.number_of_shards] set to 1 or unset; provided {shards}"
        )));
    }
    let routing_shards = match settings.get("index.number_of_routing_shards").and_then(|s| s.parse::<u32>().ok()) {
        Some(r) => {
            if r < shards || r % shards != 0 {
                return Err(EsError::illegal_argument(format!("the number of source shards [{shards}] must be a factor of [{r}]")));
            }
            r
        }
        None => routing_num_shards(shards),
    };
    let partition = settings.get("index.routing_partition_size").and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    if partition >= routing_shards {
        return Err(EsError::illegal_argument(format!(
            "routing partition size [{partition}] should be a positive number less than the number of routing shards [{routing_shards}] for [{name}]"
        )));
    }
    let opensearch = st.dialect == crate::cluster::Dialect::OpenSearch;
    // Elasticsearch keeps the routing shard count in the index metadata, not in its settings
    // (OpenSearch keeps it in both).
    if !opensearch {
        settings.remove("index.number_of_routing_shards");
    }
    let uuid = random_uuid();
    if opensearch {
        // OpenSearch has no data tiers; its indices replicate documents by default.
        if !settings.contains("index.replication.type") {
            settings.set("index.replication.type", "DOCUMENT");
        }
    } else if !settings.contains("index.routing.allocation.include._tier_preference") {
        let tier = if backing.is_some() { "data_hot" } else { "data_content" };
        settings.set("index.routing.allocation.include._tier_preference", tier);
    }
    settings.set("index.provided_name", provided_name);
    settings.set("index.creation_date", now_ms.to_string());
    settings.set("index.uuid", uuid.clone());
    settings.set("index.version.created", if opensearch { crate::version::OPENSEARCH_INDEX_VERSION_ID } else { crate::version::INDEX_VERSION_ID });
    crate::meta::auto_expand::apply(&mut settings);
    if let Some(b) = &backing {
        data_stream_index_settings(&st.metadata, b.data_stream, &mut settings, &composed.mappings, now_ms)?;
    }
    let mode = IndexMode::of(&settings);
    mode.validate_settings(&settings)?;
    mode.provided_settings(&mut settings);
    crate::meta::index_sort::validate_settings(&settings)?;

    // Analysis is built (and so validated) before the mappings that reference it.
    crate::analysis::validate_settings(&settings, Some(name))?;
    warnings.extend(crate::analysis::deprecation_warnings(&settings));

    // Mappings: the index mode's, templates in order, then the request (later definitions
    // win), then validation.
    let ctx = MappingCtx::from_settings_in(st.dialect, &settings);
    let mut sources = composed.mappings.clone();
    if let Some(default) = mode.default_mapping(&settings) {
        sources.insert(0, default);
    }
    if let Some(m) = &req.mappings {
        if sources.is_empty() && backing.is_none() {
            let mapping = mapping::for_create(m, &ctx)?;
            ctx.warnings.borrow_mut().iter().for_each(|w| warnings.push(w.clone()));
            return finish_create(st, name, uuid, settings, mapping, composed.aliases, req.aliases, routing_shards, warnings, false);
        }
        sources.push(m.clone());
    }
    let mapping = if sources.is_empty() {
        Mapping::empty()
    } else {
        let mut acc = Mapping::empty();
        for src in &sources {
            let m = Mapping::parse(&mapping::unwrap_type(src), &ctx).map_err(|e| ctx.wrap_parse_error(e))?;
            acc = acc.merge(&m, &ctx, MergeReason::IndexTemplate)?;
        }
        acc.validate(&ctx)?;
        acc
    };
    warnings.extend(ctx.warnings.borrow().iter().cloned());
    finish_create(st, name, uuid, settings, mapping, composed.aliases, req.aliases, routing_shards, warnings, backing.is_some())
}

#[allow(clippy::too_many_arguments)]
fn finish_create(
    st: &mut ClusterState,
    name: &str,
    uuid: String,
    settings: Settings,
    mapping: Mapping,
    template_aliases: BTreeMap<String, AliasMeta>,
    request_aliases: Vec<(String, AliasMeta)>,
    routing_shards: u32,
    warnings: Vec<String>,
    data_stream: bool,
) -> EsResult<Vec<String>> {
    let mode = IndexMode::of(&settings);
    mode.validate_source(&mapping, &settings)?;
    mode.validate_mapping(&mapping, &settings, data_stream)?;
    // A partitioned index must require routing (`MapperService` checks it once a mapping exists).
    if settings.get("index.routing_partition_size").and_then(|s| s.parse::<u32>().ok()).unwrap_or(1) > 1
        && !mapping.is_empty()
        && mapping.routing_required != Some(true)
    {
        return Err(EsError::illegal_argument(format!("mapping type [_doc] must have routing required for partitioned index [{name}]")));
    }
    crate::meta::index_sort::validate_mapping_in(st.dialect, &settings, &mapping, name)?;
    crate::meta::index_sort::validate_fields(&settings, &mapping, name)?;
    // Template aliases may name the index they land on (`{index}`, MetadataCreateIndexService).
    let mut aliases: BTreeMap<String, AliasMeta> = template_aliases.into_iter().map(|(k, v)| (k.replace("{index}", name), v)).collect();
    for (k, v) in request_aliases {
        // Alias names may use date math (`<logs-{now/d}>`).
        let k = resolve::resolve_date_math(&k, crate::time::now_millis())?;
        names::validate_alias_name(&k)?;
        aliases.insert(k, v);
    }
    for alias in aliases.keys() {
        if alias == name {
            return Err(EsError::illegal_argument(format!("alias name [{alias}] self-conflicts with index name")));
        }
        if st.metadata.indices.contains_key(alias) {
            return Err(EsError::invalid_alias_name(alias, "an index, data stream, ESQL view, or ESQL dataset exists with the same name as the alias"));
        }
    }
    let shards = settings.get("index.number_of_shards").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
    let idx = IndexMeta {
        name: name.to_string(),
        uuid,
        state: IndexState::Open,
        settings,
        mapping: Arc::new(mapping),
        aliases,
        version: 1,
        mapping_version: 1,
        settings_version: 1,
        aliases_version: 1,
        routing_num_shards: routing_shards,
        primary_terms: vec![1; shards],
        system: names::is_system_index(name),
        recovery_source: None,
        dialect: st.dialect,
    };
    st.metadata.indices.insert(name.to_string(), Arc::new(idx));
    st.check_write_indices()?;
    st.check_hidden_aliases()?;
    st.check_unique_names()?;
    Ok(warnings)
}

pub async fn create(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let provided = ctx.path_param("index").unwrap_or_default().to_string();
    let name = resolve::resolve_date_math(&provided, ctx.now_ms)?;
    let body = ctx.body_map()?;
    let req = parse_create_body(body)?;
    let hidden = req.settings.get("index.hidden") == Some("true");
    let now = ctx.now_ms;
    let n2 = name.clone();
    let warnings = ctx.cluster.update(move |st| create_index(st, &provided, &n2, req, now)).await?;
    ctx.warn_all(warnings);
    if name.starts_with('.') && !hidden && !names::is_system_index(&name) {
        ctx.warn_dialect(
            None,
            Some(format!(
                "index name [{name}] starts with a dot '.', in the next major version, index names starting with a dot are reserved for hidden indices and system indices"
            )),
        );
    }
    let st = ctx.state();
    let started = st.metadata.indices.get(&name).is_none_or(|idx| !ctx.cluster.shards_failed(&st, idx));
    Reply::ok(obj! { "acknowledged" => true, "shards_acknowledged" => started, "index" => name })
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let exprs = index_expressions(ctx);
    if ctx.cluster.destructive_requires_name() {
        resolve::require_names(&exprs)?;
    }
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            // Aliases can't be used to delete indices (with `ignore_unavailable` they are skipped).
            let mut kept = Vec::with_capacity(exprs.len());
            for e in &exprs {
                if !st.metadata.indices.contains_key(e) && st.has_alias(e) && !resolve::is_wildcard(e) {
                    if opts.ignore_unavailable {
                        continue;
                    }
                    return Err(EsError::illegal_argument(format!(
                        "The provided expression [{e}] matches an alias, specify the corresponding concrete indices instead."
                    )));
                }
                kept.push(e.clone());
            }
            let opts = IndicesOptions { allow_aliases: false, include_data_streams: false, ..opts };
            let names = if kept.is_empty() && !exprs.is_empty() { Vec::new() } else { resolve::resolve(st, &kept, &opts, now)? };
            delete_indices(st, &names)
        })
        .await?;
    let st = ctx.state();
    let live: std::collections::HashSet<&str> = st.metadata.indices.values().map(|i| i.uuid.as_str()).collect();
    ctx.cluster.ops.retain(|u| live.contains(u));
    Reply::acknowledged()
}

/// Deletes indices from `st`, detaching backing indices from their data streams (whose write
/// indices can't be deleted).
pub fn delete_indices(st: &mut ClusterState, names: &[String]) -> EsResult<()> {
    for n in names {
        if let Some(ds) = st.metadata.data_stream_of(n) {
            let failure = ds.failure_write_index() == Some(n.as_str());
            if failure || ds.write_index() == Some(n.as_str()) {
                return Err(EsError::illegal_argument(format!(
                    "index [{n}] is the {}write index for data stream [{}] and cannot be deleted",
                    if failure { "failure store " } else { "" },
                    ds.name
                )));
            }
        }
    }
    for n in names {
        if let Some(ds) = st.metadata.data_stream_of(n).cloned() {
            let mut d = (*ds).clone();
            d.indices.retain(|i| i != n);
            d.failure_indices.retain(|i| i != n);
            d.generation += 1;
            st.metadata.data_streams.insert(d.name.clone(), Arc::new(d));
        }
        st.metadata.indices.remove(n);
        // Synonym sets are documents of their system index on Elasticsearch.
        if n == super::synonyms::SYSTEM_INDEX && !st.metadata.synonyms.is_empty() {
            st.metadata.synonyms = Default::default();
            st.metadata.synonyms_version += 1;
        }
    }
    Ok(())
}

pub fn exists(ctx: &Ctx) -> EsResult<Reply> {
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    Ok(Reply::Empty(if names.is_empty() { 404 } else { 200 }))
}

fn get_indices<'a>(st: &'a ClusterState, names: &[String]) -> Vec<&'a IndexMeta> {
    names.iter().filter_map(|n| st.metadata.indices.get(n).map(|i| i.as_ref())).collect()
}

/// Settings of an index as `GET` prints them (`index.verified_before_close` etc. included).
/// The index's settings as `GET` prints them; `?human` adds the readable creation date and
/// version.
pub fn settings_json(idx: &IndexMeta, flat: bool, human: bool) -> Value {
    if !human {
        return idx.settings.to_json(flat);
    }
    let mut s = idx.settings.clone();
    if let Some(d) = s.get("index.creation_date").and_then(|d| d.parse::<i64>().ok()) {
        s.set("index.creation_date_string", crate::time::DateFormatter::new("strict_date_time").expect("built-in").format_millis(d));
    }
    if s.contains("index.version.created") {
        s.set("index.version.created_string", crate::version::INDEX_VERSION_STRING);
    }
    s.to_json(flat)
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    warn_system_access(ctx, &names);
    let flat = ctx.flat_settings()?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let features: Vec<String> = match ctx.query("features") {
        Some(f) => {
            let list: Vec<String> = f.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            let invalid: Vec<&String> = list.iter().filter(|x| !matches!(x.as_str(), "aliases" | "mappings" | "settings")).collect();
            if !invalid.is_empty() {
                let l: Vec<&str> = invalid.iter().map(|s| s.as_str()).collect();
                return Err(EsError::illegal_argument(format!("Invalid features specified [{}]", l.join(","))));
            }
            list
        }
        None => vec!["aliases".into(), "mappings".into(), "settings".into()],
    };
    let indices = get_indices(&st, &names);
    blocks::check(indices.iter().copied(), Level::MetadataRead)?;
    let mut out = Map::new();
    for idx in indices {
        let mut m = Map::new();
        let has = |f: &str| features.iter().any(|x| x == f);
        let mut aliases = Map::new();
        if has("aliases") {
            for (k, a) in &idx.aliases {
                aliases.insert(k.clone(), a.to_value());
            }
        }
        m.insert("aliases".into(), Value::Obj(aliases));
        m.insert("mappings".into(), if has("mappings") { idx.mapping_json(false) } else { obj! {} });
        m.insert("settings".into(), if has("settings") { settings_json(idx, flat, ctx.human) } else { obj! {} });
        if include_defaults && has("settings") {
            m.insert("defaults".into(), index_settings::defaults_for_in(idx.dialect, &idx.settings).to_json(flat));
        }
        if let Some(ds) = st.metadata.data_stream_of(&idx.name) {
            m.insert("data_stream".into(), Value::from(ds.name.clone()));
        }
        out.insert(idx.name.clone(), Value::Obj(m));
    }
    Reply::ok(Value::Obj(out))
}

pub fn get_mapping(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    warn_system_access(ctx, &names);
    let indices = get_indices(&st, &names);
    blocks::check(indices.iter().copied(), Level::MetadataRead)?;
    let mut out = Map::new();
    for idx in indices {
        out.insert(idx.name.clone(), obj! { "mappings" => idx.mapping_json(false) });
    }
    Reply::ok(Value::Obj(out))
}

pub async fn put_mapping(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?.ok_or_else(|| EsError::parse("request body is required"))?;
    // OpenSearch refuses a mapping wrapped in its `_doc` type.
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch && body.len() == 1 && body.contains_key("_doc") {
        return Err(EsError::illegal_argument("Types cannot be provided in put mapping requests"));
    }
    let exprs = index_expressions(ctx);
    let opts = ctx.indices_options(IndicesOptions { allow_no_indices: false, ..IndicesOptions::STRICT_EXPAND })?;
    let write_only = ctx.bool_param("write_index_only", false)?;
    let now = ctx.now_ms;
    let warnings = ctx
        .cluster
        .update(move |st| {
            let mut names = resolve::resolve(st, &exprs, &opts, now)?;
            if write_only {
                names.retain(|n| {
                    let idx = &st.metadata.indices[n];
                    exprs.iter().all(|e| match st.aliases().get(e.as_str()) {
                        Some(members) => members.iter().any(|(i, a)| i.name == idx.name && (a.is_write_index == Some(true) || members.len() == 1)),
                        None => true,
                    })
                });
            }
            let indices: Vec<Arc<IndexMeta>> = names.iter().map(|n| st.metadata.indices[n].clone()).collect();
            blocks::check(indices.iter().map(|i| i.as_ref()), Level::MetadataWrite)?;
            let mut warnings = Vec::new();
            let mut updated = Vec::new();
            for idx in &indices {
                let mctx = idx.mapping_ctx();
                let merged = mapping::for_update(&idx.mapping, &body, &mctx)?;
                let mode = IndexMode::of(&idx.settings);
                mode.validate_source(&merged, &idx.settings)?;
                mode.validate_mapping(&merged, &idx.settings, st.metadata.data_stream_of(&idx.name).is_some())?;
                crate::meta::index_sort::validate_mapping_in(idx.dialect, &idx.settings, &merged, &idx.name)?;
                warnings.extend(mctx.warnings.borrow().iter().cloned());
                if merged != *idx.mapping {
                    let mut ni = (**idx).clone();
                    ni.mapping = Arc::new(merged);
                    ni.mapping_version += 1;
                    ni.version += 1;
                    updated.push(ni);
                }
            }
            for ni in updated {
                st.metadata.indices.insert(ni.name.clone(), Arc::new(ni));
            }
            warnings.dedup();
            Ok(warnings)
        })
        .await?;
    ctx.warn_all(warnings);
    Reply::acknowledged()
}

/// Metadata fields `GET _mapping/field/*` lists.
const MAPPING_METADATA_FIELDS: &[&str] = &[
    "_routing", "_doc_count", "_inference_fields", "_ignored_source", "_index", "_feature", "_index_mode", "_ignored", "_tier",
    "_seq_no", "_nested_path", "_data_stream_timestamp", "_field_names", "_source", "_id", "_version",
];

pub fn get_field_mapping(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    let patterns = ctx.list_param("fields").unwrap_or_default();
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let indices = get_indices(&st, &names);
    blocks::check(indices.iter().copied(), Level::MetadataRead)?;
    let mut out = Map::new();
    for idx in indices {
        let mctx = idx.mapping_ctx();
        let mut fields = Map::new();
        let paths = idx.mapping.field_paths();
        for p in &patterns {
            for meta in MAPPING_METADATA_FIELDS {
                if glob_match(p, meta) && !fields.contains_key(*meta) {
                    let mapping = match *meta {
                        "_routing" => idx.mapping.routing_value(include_defaults).map(|r| obj! { "_routing" => r }).unwrap_or_else(|| obj! {}),
                        _ => obj! {},
                    };
                    fields.insert(meta.to_string(), obj! { "full_name" => *meta, "mapping" => mapping });
                }
            }
            for (path, m) in &paths {
                if !glob_match(p, path) || fields.contains_key(path) {
                    continue;
                }
                // The mapper's own name: the last path segment, or the whole dotted name of
                // a field flattened under `subobjects: false` (or a columnar mode).
                let leaf = match m {
                    MapperRef::Field(f) => f.name.as_str(),
                    MapperRef::Alias(a) => a.name.as_str(),
                    MapperRef::Object(o) => o.name.as_str(),
                };
                let mv = match m {
                    MapperRef::Field(f) => f.to_value(&mctx, include_defaults),
                    MapperRef::Alias(a) => obj! { "type" => "alias", "path" => a.path.clone() },
                    MapperRef::Object(_) => continue,
                };
                fields.insert(path.clone(), obj! { "full_name" => path.clone(), "mapping" => obj! { leaf => mv } });
            }
        }
        // Elasticsearch collects the fields in a `HashMap`.
        out.insert(idx.name.clone(), obj! { "mappings" => Value::Obj(crate::json::java_hash_map(fields, 16)) });
    }
    // `GetFieldMappingsResponse` keys indices in a `HashMap`.
    Reply::ok(Value::Obj(crate::json::java_hash_map(out, 16)))
}

pub fn get_settings(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    warn_system_access(ctx, &names);
    let flat = ctx.flat_settings()?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let filters = ctx.list_param("name");
    let indices = get_indices(&st, &names);
    blocks::check(indices.iter().copied(), Level::MetadataRead)?;
    let keep = |s: &Settings| -> Settings {
        match &filters {
            None => s.clone(),
            Some(fs) if fs.iter().any(|f| f == "_all") => s.clone(),
            Some(fs) => {
                let mut out = Settings::new();
                for (k, v) in s.iter() {
                    if fs.iter().any(|f| glob_match(f, k)) {
                        out.insert(k.clone(), v.clone());
                    }
                }
                out
            }
        }
    };
    let mut out = Map::new();
    for idx in indices {
        let settings = keep(&idx.settings);
        if filters.is_some() && settings.is_empty() && !include_defaults {
            continue;
        }
        let mut m = Map::new();
        m.insert("settings".into(), settings.to_json(flat));
        if include_defaults {
            m.insert("defaults".into(), keep(&index_settings::defaults_for_in(idx.dialect, &idx.settings)).to_json(flat));
        }
        out.insert(idx.name.clone(), Value::Obj(m));
    }
    // `GetSettingsResponse` keys indices in a `HashMap`.
    Reply::ok(Value::Obj(crate::json::java_hash_map(out, 16)))
}

pub async fn put_settings(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?.unwrap_or_default();
    let source = match body.get("settings") {
        Some(Value::Obj(inner)) if body.len() == 1 => Value::Obj(inner.clone()),
        _ => Value::Obj(body.clone()),
    };
    let mut update = Settings::from_json(&source)?;
    update.normalize_prefix("index.");
    if update.is_empty() {
        return Err(EsError::action_request_validation(&["no settings to update".into()]));
    }
    let preserve = ctx.bool_param("preserve_existing", false)?;
    let reopen = ctx.bool_param("reopen", false)?;
    let exprs = index_expressions(ctx);
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            // Validation that doesn't depend on the indices.
            let d = st.dialect;
            let os = d == crate::cluster::Dialect::OpenSearch;
            let mut unknown = Vec::new();
            for (k, _) in update.iter() {
                match index_settings::lookup_in(d, k) {
                    Some(def) if def.scope == Scope::Internal => unknown.push(index_settings::unknown_internal(k)),
                    Some(def) if def.scope == Scope::Private => {
                        let owner = if os { "OpenSearch" } else { "Elasticsearch" };
                        let mut e = EsError::illegal_argument(format!("can not update private setting [{k}]; this setting is managed by {owner}"));
                        if os {
                            e.kind = "settings_exception".into();
                        }
                        return Err(e);
                    }
                    Some(def) if def.scope == Scope::Managed => {
                        let mut e = EsError::illegal_argument(format!("can not update internal setting [{k}]; this setting is managed via a dedicated API"));
                        if os {
                            e.kind = "settings_exception".into();
                        }
                        return Err(e);
                    }
                    Some(_) => {}
                    None if index_settings::is_group_key(k) || k.starts_with("archived.") => {}
                    None => unknown.push(index_settings::unknown_setting_in(d, k)),
                }
            }
            if !unknown.is_empty() {
                if os {
                    // OpenSearch reports them as settings exceptions.
                    for e in unknown.iter_mut() {
                        e.kind = "settings_exception".into();
                    }
                }
                let first = unknown.remove(0);
                return Err(first.with_suppressed(unknown));
            }
            let validate_values = || -> EsResult<()> {
                for (k, v) in update.iter() {
                    if let Some(def) = index_settings::lookup_in(d, k) {
                        index_settings::validate_value_in(d, def, v)?;
                    }
                }
                Ok(())
            };
            // OpenSearch checks values first; Elasticsearch once the update scope is known to be fine.
            if os {
                validate_values()?;
            }
            index_settings::reject_translog_retention_in(d, &update)?;
            let names = resolve::resolve(st, &exprs, &opts, now)?;
            let indices: Vec<Arc<IndexMeta>> = names.iter().map(|n| st.metadata.indices[n].clone()).collect();
            let only_blocks = update.keys().all(|k| k.starts_with("index.blocks."));
            if !only_blocks {
                blocks::check(indices.iter().map(|i| i.as_ref()), Level::MetadataWrite)?;
            }
            let scope_of = |k: &str| index_settings::lookup_in(d, k).map(|d| d.scope).unwrap_or_else(|| index_settings::group_scope(k));
            let non_dynamic: Vec<&String> = update.keys().filter(|k| scope_of(k) != Scope::Dynamic).collect();
            let finals: Vec<&String> = update.keys().filter(|k| scope_of(k) == Scope::Final).collect();
            let open: Vec<&Arc<IndexMeta>> = indices.iter().filter(|i| !i.is_closed()).collect();
            if !non_dynamic.is_empty() && !open.is_empty() && !(reopen && finals.is_empty()) {
                let nd: Vec<&str> = non_dynamic.iter().map(|s| s.as_str()).collect();
                let idx_list: Vec<String> = open.iter().map(|i| format!("{}/{}", i.name, i.uuid)).collect();
                let msg = if os {
                    format!("Can't update non dynamic settings [[{}]] for open indices [[{}]]", nd.join(", "), idx_list.join(", "))
                } else if finals.is_empty() {
                    format!(
                        "Can't update non dynamic setting(s) [[{}]] for open indices [[{}]]. You can either resubmit the update with `?reopen=true`, or create a new index with the desired setting(s) and reindex your data. See https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-reindex",
                        nd.join(", "),
                        idx_list.join(", ")
                    )
                } else {
                    let f: Vec<&str> = finals.iter().map(|s| s.as_str()).collect();
                    format!(
                        "Can't update non dynamic setting(s) [[{}]] for open indices [[{}]]. The setting(s) [[{}]] cannot be modified on an index once it is created. You will need to create a new index with the desired setting(s) and reindex your data. See https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-reindex",
                        nd.join(", "),
                        idx_list.join(", "),
                        f.join(", ")
                    )
                };
                return Err(EsError::illegal_argument(msg));
            }
            if let Some(f) = finals.first() {
                let name = indices.first().map(|i| i.name.as_str()).unwrap_or("index");
                let mut e = EsError::illegal_argument(format!("final {name} setting [{f}], not updateable"));
                if os {
                    e.kind = "settings_exception".into();
                }
                return Err(e);
            }
            if !os {
                validate_values()?;
            }
            for idx in &indices {
                let mut ni = (**idx).clone();
                let mut changed = false;
                for (k, v) in update.iter() {
                    if preserve && ni.settings.contains(k) {
                        continue;
                    }
                    match v {
                        SettingValue::Null => {
                            changed |= ni.settings.remove(k).is_some();
                        }
                        v => {
                            if ni.settings.get_value(k) != Some(v) {
                                ni.settings.insert(k.clone(), v.clone());
                                changed = true;
                            }
                        }
                    }
                }
                if changed {
                    if let Some(end) = update.get("index.time_series.end_time")
                        && IndexMode::of(&ni.settings) == IndexMode::TimeSeries
                    {
                        validate_end_time_update(idx, &ni, end)?;
                    }
                    crate::meta::auto_expand::apply(&mut ni.settings);
                    if update.keys().any(|k| k.starts_with("index.analysis.")) {
                        crate::analysis::validate_settings(&ni.settings, Some(&ni.name))?;
                    }
                    // Mapping limits must still hold.
                    let mctx = ni.mapping_ctx();
                    ni.mapping.validate(&mctx)?;
                    ni.settings_version += 1;
                    ni.version += 1;
                    st.metadata.indices.insert(ni.name.clone(), Arc::new(ni));
                }
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// A TSDB index's end time may only move forward (`TimestampBounds.updateEndTime`), and stays
/// after its start time.
fn validate_end_time_update(before: &IndexMeta, after: &IndexMeta, end: &str) -> EsResult<()> {
    let new = index_settings::parse_date_setting(end)?;
    let start = after.settings.get("index.time_series.start_time").map(index_settings::parse_date_setting).transpose()?;
    if start.is_some_and(|s| s > new) {
        return Err(EsError::illegal_argument("index.time_series.end_time must be larger than index.time_series.start_time"));
    }
    if let Some(cur) = before.settings.get("index.time_series.end_time").map(index_settings::parse_date_setting).transpose()?
        && cur > new
    {
        return Err(EsError::illegal_argument(format!(
            "index.time_series.end_time must be larger than current value [{cur}] but was [{}]",
            crate::time::java_instant_string(new)
        )));
    }
    Ok(())
}

pub async fn close(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let exprs = index_expressions(ctx);
    if ctx.cluster.destructive_requires_name() {
        resolve::require_names(&exprs)?;
    }
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?.without_data_streams();
    let now = ctx.now_ms;
    let closed: Vec<(String, String)> = ctx
        .cluster
        .update(move |st| {
            let names = resolve::resolve(st, &exprs, &opts, now)?;
            let indices: Vec<Arc<IndexMeta>> = names.iter().map(|n| st.metadata.indices[n].clone()).collect();
            blocks::check(indices.iter().map(|i| i.as_ref()), Level::MetadataWrite)?;
            let mut closed = Vec::new();
            for idx in indices {
                if idx.is_closed() {
                    continue;
                }
                let mut ni = (*idx).clone();
                ni.state = IndexState::Close;
                ni.settings.set("index.verified_before_close", "true");
                ni.version += 1;
                ni.settings_version += 1;
                closed.push((ni.name.clone(), ni.uuid.clone()));
                st.metadata.indices.insert(ni.name.clone(), Arc::new(ni));
            }
            Ok(closed)
        })
        .await?;
    let mut indices = Map::new();
    for (n, uuid) in &closed {
        ctx.cluster.analysis.forget(uuid);
        ctx.cluster.ops.forget(uuid);
        indices.insert(n.clone(), obj! { "closed" => true });
    }
    Reply::ok(obj! { "acknowledged" => true, "shards_acknowledged" => !closed.is_empty(), "indices" => Value::Obj(indices) })
}

pub async fn open(ctx: &Ctx<'_>) -> EsResult<Reply> {
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch && !ctx.bool_param("wait_for_completion", true)? {
        // OpenSearch: the open runs as a task whose outcome `_tasks` reports.
        ctx.time_param("task_execution_timeout", crate::units::TimeValue::seconds(30))?;
        let description = format!("open indices [{}]", index_expressions(ctx).join(", "));
        let headers = ctx.request.header("x-opaque-id").map(|v| vec![("X-Opaque-Id".to_string(), v.to_string())]).unwrap_or_default();
        let guard = ctx.cluster.tasks.start_stored("indices:admin/open", &description, headers);
        let outcome = open_indices(ctx).await;
        let id = ctx.cluster.tasks.finish(guard, outcome);
        return Reply::ok(obj! { "task" => format!("{}:{id}", ctx.cluster.node.id) });
    }
    Reply::ok(open_indices(ctx).await?)
}

async fn open_indices(ctx: &Ctx<'_>) -> EsResult<Value> {
    ctx.validate_timeouts()?;
    let exprs = index_expressions(ctx);
    let opts = ctx.indices_options(IndicesOptions { expand_open: false, expand_closed: true, ..IndicesOptions::STRICT_EXPAND_OPEN })?;
    let now = ctx.now_ms;
    let names = ctx
        .cluster
        .update(move |st| {
            let names = resolve::resolve(st, &exprs, &opts, now)?;
            let indices: Vec<Arc<IndexMeta>> = names.iter().map(|n| st.metadata.indices[n].clone()).collect();
            blocks::check(indices.iter().map(|i| i.as_ref()), Level::MetadataWrite)?;
            for idx in indices {
                if !idx.is_closed() {
                    continue;
                }
                let mut ni = (*idx).clone();
                ni.state = IndexState::Open;
                ni.settings.remove("index.verified_before_close");
                ni.version += 1;
                ni.settings_version += 1;
                st.metadata.indices.insert(ni.name.clone(), Arc::new(ni));
            }
            Ok(names)
        })
        .await?;
    let st = ctx.state();
    let started = names.iter().filter_map(|n| st.metadata.indices.get(n)).all(|idx| !ctx.cluster.shards_failed(&st, idx));
    Ok(obj! { "acknowledged" => true, "shards_acknowledged" => started })
}

async fn set_block(ctx: &Ctx<'_>, value: bool) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let block = ctx.path_param("block").unwrap_or_default().to_string();
    if block == "read_only_allow_delete" {
        return Err(EsError::action_request_validation(&["read_only_allow_delete block is for internal use only".into()]));
    }
    let Some(key) = crate::meta::blocks::block_setting(&block) else {
        return Err(EsError::illegal_argument(format!("No block found with name {block}")));
    };
    let exprs = index_expressions(ctx);
    if value && ctx.cluster.destructive_requires_name() {
        resolve::require_names(&exprs)?;
    }
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let now = ctx.now_ms;
    let names = ctx
        .cluster
        .update(move |st| {
            let names = resolve::resolve(st, &exprs, &opts, now)?;
            for n in &names {
                let mut ni = (*st.metadata.indices[n]).clone();
                if ni.settings.get(key) != Some(if value { "true" } else { "false" }) {
                    ni.settings.set(key, if value { "true" } else { "false" });
                    ni.settings_version += 1;
                    ni.version += 1;
                    st.metadata.indices.insert(n.clone(), Arc::new(ni));
                }
            }
            Ok(names)
        })
        .await?;
    let field = if value { "blocked" } else { "unblocked" };
    let list: Vec<Value> = names.iter().map(|n| obj! { "name" => n.clone(), field => true }).collect();
    if value {
        Reply::ok(obj! { "acknowledged" => true, "shards_acknowledged" => true, "indices" => Value::Arr(list) })
    } else {
        Reply::ok(obj! { "acknowledged" => true, "indices" => Value::Arr(list) })
    }
}

pub async fn add_block(ctx: &Ctx<'_>) -> EsResult<Reply> {
    set_block(ctx, true).await
}

pub async fn remove_block(ctx: &Ctx<'_>) -> EsResult<Reply> {
    set_block(ctx, false).await
}

/// Health of an index on a single node: replicas can't be assigned.
pub fn index_health(cluster: &crate::Cluster, st: &ClusterState, idx: &IndexMeta) -> &'static str {
    if cluster.shards_failed(st, idx) {
        "red"
    } else if idx.is_closed() {
        // Closed indices with replicas report like open ones (shards are allocated closed).
        if idx.number_of_replicas() > 0 { "yellow" } else { "green" }
    } else if idx.number_of_replicas() > 0 {
        "yellow"
    } else {
        "green"
    }
}

/// A fresh index's stats block (all zeros), as each dialect prints it.
fn stats_template(d: crate::cluster::Dialect) -> Value {
    static ES: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    static OS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    match d {
        crate::cluster::Dialect::Elasticsearch => {
            ES.get_or_init(|| crate::json::parse(include_bytes!("index_stats_template.json")).ok().flatten().unwrap_or_default()).clone()
        }
        crate::cluster::Dialect::OpenSearch => {
            OS.get_or_init(|| crate::json::parse(include_bytes!("index_stats_template_os.json")).ok().flatten().unwrap_or_default()).clone()
        }
    }
}

/// OpenSearch's `indices.stats` metrics.
const OS_STATS_METRICS: &[&str] =
    &["_all", "completion", "docs", "fielddata", "query_cache", "flush", "get", "indexing", "merge", "request_cache", "refresh", "search", "segments", "store", "warmer", "suggest", "translog", "recovery"];

const STATS_METRICS: &[&str] = &[
    "_all", "completion", "docs", "fielddata", "query_cache", "flush", "get", "indexing", "mappings", "merge", "recovery", "request_cache",
    "refresh", "search", "segments", "shard_stats", "store", "translog", "warmer", "bulk", "dense_vector", "sparse_vector",
];

/// Adds shard operation counts to a stats block (where the metrics are present).
fn add_ops(block: &mut Value, c: crate::cluster::OpCounts) {
    let Some(m) = block.as_object_mut() else { return };
    let mut bump = |section: &str, key: &str, by: u64| {
        if let Some(s) = m.get_mut(section).and_then(Value::as_object_mut) {
            let cur = s.get(key).and_then(Value::as_i64).unwrap_or(0);
            s.insert(key.into(), Value::from(cur + by as i64));
        }
    };
    bump("flush", "total", c.flush);
    bump("refresh", "total", c.refresh);
    bump("refresh", "external_total", c.external_refresh);
}

/// The summed operation counts of an index's started shards.
fn index_ops(ctx: &Ctx, idx: &IndexMeta) -> crate::cluster::OpCounts {
    let mut sum = crate::cluster::OpCounts { flush: 0, refresh: 0, external_refresh: 0 };
    if idx.is_closed() {
        return sum;
    }
    for s in 0..idx.number_of_shards() as usize {
        let c = ctx.cluster.ops.get(&idx.uuid, s);
        sum.flush += c.flush;
        sum.refresh += c.refresh;
        sum.external_refresh += c.external_refresh;
    }
    sum
}

/// One index's stats block with `shards` primaries (zeros until the engine reports numbers).
pub fn stats_block(metrics: &[String], shards: u64) -> Value {
    stats_block_in(crate::cluster::Dialect::Elasticsearch, metrics, shards)
}

pub fn stats_block_in(d: crate::cluster::Dialect, metrics: &[String], shards: u64) -> Value {
    let mut t = stats_template(d);
    if let Some(m) = t.as_object_mut() {
        if let Some(Value::Obj(ss)) = m.get_mut("shard_stats") {
            ss.insert("total_count".into(), Value::from(shards));
        }
        let all = metrics.is_empty() || metrics.iter().any(|x| x == "_all");
        if !all {
            m.retain(|k, _| {
                let name = if k == "merges" { "merge" } else { k.as_str() };
                metrics.iter().any(|x| x == name)
            });
        }
    }
    t
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let mut r = stats_inner(ctx)?;
    if ctx.human
        && let Reply::Body(_, v) = &mut r
    {
        crate::units::humanize(v);
    }
    Ok(r)
}

fn stats_inner(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    let metrics = ctx.list_param("metric").unwrap_or_default();
    let d = ctx.cluster.dialect;
    let os = d == crate::cluster::Dialect::OpenSearch;
    for m in &metrics {
        if !(if os { OS_STATS_METRICS } else { STATS_METRICS }).contains(&m.as_str()) {
            return Err(EsError::illegal_argument(format!(
                "request [{}] contains unrecognized metric: [{m}]",
                ctx.path
            )));
        }
    }
    let level = ctx.query("level").unwrap_or("indices");
    if !matches!(level, "cluster" | "indices" | "shards") {
        return Err(EsError::illegal_argument(format!("level parameter must be one of [cluster] or [indices] or [shards] but was [{level}]")));
    }
    // Closed indices are only reported when asked for (`forbid_closed_indices=false`).
    let with_closed = !ctx.bool_param("forbid_closed_indices", true)?;
    let indices = get_indices(&st, &names);
    let (mut total_shards, mut primaries) = (0u64, 0u64);
    let mut all_ops = crate::cluster::OpCounts { flush: 0, refresh: 0, external_refresh: 0 };
    let mut per_index = Map::new();
    for idx in &indices {
        // Closed indices are only reported when asked for; indices without a started shard
        // have no stats.
        if (idx.is_closed() && !with_closed) || ctx.cluster.shards_failed(&st, idx) {
            continue;
        }
        let p = idx.number_of_shards() as u64;
        let t = if idx.is_closed() { p } else { p * (1 + idx.number_of_replicas() as u64) };
        total_shards += t;
        primaries += p;
        let mut m = Map::new();
        m.insert("uuid".into(), idx.uuid.clone().into());
        if !os {
            m.insert("health".into(), index_health(ctx.cluster, &st, idx).into());
            m.insert("status".into(), idx.state.as_str().into());
        }
        let ops = index_ops(ctx, idx);
        all_ops.flush += ops.flush;
        all_ops.refresh += ops.refresh;
        all_ops.external_refresh += ops.external_refresh;
        for k in ["primaries", "total"] {
            let mut b = stats_block_in(d, &metrics, p);
            add_ops(&mut b, ops);
            m.insert(k.into(), b);
        }
        if level == "shards" {
            m.insert("shards".into(), shard_stats(ctx, idx, &metrics));
        }
        per_index.insert(idx.name.clone(), Value::Obj(m));
    }
    let mut body = Map::new();
    body.insert("_shards".into(), obj! { "total" => total_shards, "successful" => primaries, "failed" => 0 });
    let mut all = Map::new();
    for k in ["primaries", "total"] {
        let mut b = stats_block_in(d, &metrics, primaries);
        add_ops(&mut b, all_ops);
        all.insert(k.into(), b);
    }
    body.insert("_all".into(), Value::Obj(all));
    if level != "cluster" {
        // `IndicesStatsResponse` collects indices in a `HashMap`.
        body.insert("indices".into(), Value::Obj(crate::json::java_hash_map(per_index, 16)));
    }
    Reply::ok(Value::Obj(body))
}

/// A stable pseudo-random base64 id derived from an index, a shard and a purpose (commit ids,
/// translog and history UUIDs; Elasticsearch's are random).
fn derived_id(idx: &IndexMeta, shard: u32, purpose: &str, len: usize, url_safe: bool) -> String {
    use base64::Engine;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut out = vec![0u8; len];
    for (i, b) in out.iter_mut().enumerate() {
        for x in idx.uuid.bytes().chain(shard.to_le_bytes()).chain(purpose.bytes()).chain([i as u8]) {
            h ^= x as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        *b = (h >> 32) as u8;
    }
    if url_safe { base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out) } else { base64::engine::general_purpose::STANDARD.encode(out) }
}

/// `level=shards`: each started copy (the primaries; replicas stay unassigned on one node).
fn shard_stats(ctx: &Ctx, idx: &IndexMeta, metrics: &[String]) -> Value {
    let node = &ctx.cluster.node;
    let mut shards = Map::new();
    for shard in 0..idx.number_of_shards() {
        let mut m = Map::new();
        m.insert("routing".into(), obj! { "state" => "STARTED", "primary" => true, "node" => node.id.clone(), "relocating_node" => Value::Null });
        let mut block = stats_block_in(ctx.cluster.dialect, metrics, 1);
        if !idx.is_closed() {
            add_ops(&mut block, ctx.cluster.ops.get(&idx.uuid, shard as usize));
        }
        if let Value::Obj(mut block) = block {
            // Off-heap vector sizes are only summed per index.
            if let Some(Value::Obj(dv)) = block.get_mut("dense_vector") {
                dv.shift_remove("off_heap");
            }
            m.extend(block);
        }
        m.insert(
            "commit".into(),
            obj! {
                "id" => derived_id(idx, shard, "commit", 16, false),
                "generation" => 2,
                "user_data" => obj! {
                    "translog_uuid" => derived_id(idx, shard, "translog", 16, true),
                    "es_version" => crate::version::INDEX_VERSION_ID,
                    "local_checkpoint" => "-1",
                    "max_seq_no" => "-1",
                    "max_unsafe_auto_id_timestamp" => "-1",
                    "history_uuid" => derived_id(idx, shard, "history", 16, true),
                },
                "num_docs" => 0,
            },
        );
        m.insert("seq_no".into(), obj! { "max_seq_no" => -1, "local_checkpoint" => -1, "global_checkpoint" => -1 });
        m.insert(
            "retention_leases".into(),
            obj! {
                "primary_term" => 1,
                "version" => 1,
                "leases" => Value::Arr(vec![obj! {
                    "id" => format!("peer_recovery/{}", node.id),
                    "retaining_seq_no" => 0,
                    "timestamp" => idx.creation_date(),
                    "source" => "peer recovery",
                }]),
            },
        );
        m.insert("shard_path".into(), obj! { "state_path" => "/data", "data_path" => "/data", "is_custom_data_path" => false });
        m.insert("search_idle".into(), false.into());
        m.insert("search_idle_time".into(), 0.into());
        shards.insert(shard.to_string(), Value::Arr(vec![Value::Obj(m)]));
    }
    Value::Obj(shards)
}

pub fn resolve_index(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let exprs = ctx.list_param("name").unwrap_or_default();
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    resolve::reject_remote(&exprs)?;
    // Aliases are listed as aliases, not expanded into the indices they point at; a concrete
    // name must still exist as one or the other.
    if !opts.ignore_unavailable {
        for e in &exprs {
            let e = resolve::resolve_date_math(e, ctx.now_ms)?;
            let md = &st.metadata;
            let known = md.indices.contains_key(&e) || st.has_alias(&e) || md.data_streams.contains_key(&e) || md.data_stream_aliases.contains_key(&e);
            if !resolve::is_wildcard(&e) && e != "_all" && !e.starts_with('-') && !known {
                return Err(EsError::index_not_found(&e));
            }
        }
    }
    // Data streams are listed as data streams, not expanded into their backing indices.
    // `mode`: only indices in these index modes (and aliases' members among them).
    let modes: Option<Vec<String>> = match ctx.query("mode") {
        None => None,
        Some(raw) => {
            let mut out = Vec::new();
            for m in raw.split(',') {
                let m = m.trim().to_ascii_lowercase();
                if !crate::meta::index_settings::INDEX_MODES.contains(&m.as_str()) {
                    return Err(EsError::illegal_argument(format!(
                        "[{m}] is an invalid index mode, valid modes are: [{}]",
                        crate::meta::index_settings::INDEX_MODES.join(",")
                    )));
                }
                out.push(m);
            }
            Some(out)
        }
    };
    let mode_of = |idx: &IndexMeta| idx.settings.get("index.mode").map(str::to_ascii_lowercase).unwrap_or_else(|| "standard".into());
    let selected = |idx: &IndexMeta| modes.as_ref().is_none_or(|ms| ms.contains(&mode_of(idx)));
    let names = resolve::resolve(&st, &exprs, &IndicesOptions { allow_aliases: false, ignore_unavailable: true, include_data_streams: false, ..opts }, ctx.now_ms)?;
    let mut indices = Vec::new();
    for n in &names {
        let idx = &st.metadata.indices[n];
        if !selected(idx) {
            continue;
        }
        let mut m = Map::new();
        m.insert("name".into(), n.clone().into());
        if !idx.aliases.is_empty() {
            m.insert("aliases".into(), Value::from(idx.aliases.keys().cloned().collect::<Vec<_>>()));
        }
        let mut attrs = Vec::new();
        if idx.is_hidden() {
            attrs.push("hidden");
        }
        if idx.system {
            attrs.push("system");
        }
        attrs.push(if idx.is_closed() { "closed" } else { "open" });
        attrs.sort_unstable();
        m.insert("attributes".into(), Value::from(attrs));
        if let Some(ds) = st.metadata.data_stream_of(n) {
            m.insert("data_stream".into(), Value::from(ds.name.clone()));
        }
        m.insert("mode".into(), mode_of(idx).into());
        indices.push(Value::Obj(m));
    }
    // Aliases matched by the expressions.
    let mut aliases = Vec::new();
    for (alias, members) in st.aliases() {
        let matched = exprs.is_empty() || exprs.iter().any(|e| e == "_all" || glob_match(e, alias));
        let hidden = members.iter().any(|(_, a)| a.is_hidden == Some(true));
        if matched && (!hidden || opts.expand_hidden) {
            let mut list: Vec<String> = members.iter().filter(|(i, _)| selected(i)).map(|(i, _)| i.name.clone()).collect();
            if list.is_empty() {
                continue;
            }
            list.sort();
            aliases.push(obj! { "name" => alias, "indices" => list });
        }
    }
    // Data stream aliases, then data streams.
    for (alias, a) in &st.metadata.data_stream_aliases {
        if exprs.is_empty() || exprs.iter().any(|e| e == "_all" || glob_match(e, alias)) {
            aliases.push(obj! { "name" => alias.clone(), "indices" => a.data_streams.clone() });
        }
    }
    aliases.sort_by(|a, b| a.get("name").and_then(Value::as_str).cmp(&b.get("name").and_then(Value::as_str)));
    let ds_opts = IndicesOptions { ignore_unavailable: true, ..opts };
    let ds_names = super::data_streams::resolve_data_streams(&st, &exprs, &ds_opts).unwrap_or_default();
    let mut data_streams = Vec::new();
    for n in ds_names {
        let ds = &st.metadata.data_streams[&n];
        let mut m = Map::new();
        m.insert("name".into(), Value::from(n.clone()));
        m.insert("backing_indices".into(), Value::from(ds.indices.clone()));
        if !ds.failure_indices.is_empty() {
            m.insert("failure_indices".into(), Value::from(ds.failure_indices.clone()));
        }
        m.insert("timestamp_field".into(), Value::from(crate::meta::data_streams::TIMESTAMP_FIELD));
        data_streams.push(Value::Obj(m));
    }
    Reply::ok(obj! { "indices" => Value::Arr(indices), "aliases" => Value::Arr(aliases), "data_streams" => Value::Arr(data_streams) })
}

/// `_flush`: parameter validation, then the broadcast shape.
pub fn flush(ctx: &Ctx) -> EsResult<Reply> {
    let force = ctx.bool_param("force", false)?;
    let wait = ctx.bool_param("wait_if_ongoing", true)?;
    if force && !wait {
        return Err(EsError::action_request_validation(&["wait_if_ongoing must be true for a force flush".into()]));
    }
    // Every open shard flushes; a forced flush refreshes internally first.
    let r = broadcast(ctx)?;
    for idx in open_targets(ctx)? {
        ctx.cluster.ops.record(&idx.uuid, idx.number_of_shards() as usize, |c| {
            c.flush += 1;
            if force {
                c.refresh += 1;
            }
        });
    }
    Ok(r)
}

/// `_refresh`: every open shard refreshes for searchers.
pub fn refresh(ctx: &Ctx) -> EsResult<Reply> {
    let r = broadcast(ctx)?;
    for idx in open_targets(ctx)? {
        ctx.cluster.ops.record(&idx.uuid, idx.number_of_shards() as usize, |c| {
            c.refresh += 1;
            c.external_refresh += 1;
        });
    }
    Ok(r)
}

/// The open indices a broadcast request targets.
fn open_targets(ctx: &Ctx) -> EsResult<Vec<Arc<IndexMeta>>> {
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    Ok(names.iter().filter_map(|n| st.metadata.indices.get(n)).filter(|i| !i.is_closed()).cloned().collect())
}

/// `_forcemerge`: parameter validation; `wait_for_completion=false` answers with a task id.
pub fn forcemerge(ctx: &Ctx) -> EsResult<Reply> {
    let os = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    let expunge = ctx.bool_param("only_expunge_deletes", false)?;
    let max_segments = ctx.int_param("max_num_segments", -1)?;
    if expunge && max_segments != -1 {
        if os {
            ctx.warn("setting only_expunge_deletes and max_num_segments at the same time is deprecated and will be rejected in a future version");
        } else {
            return Err(EsError::action_request_validation(&[
                "cannot set only_expunge_deletes and max_num_segments at the same time, those two parameters are mutually exclusive".into(),
            ]));
        }
    }
    let flush = ctx.bool_param("flush", true)?;
    let primary_only = os && ctx.bool_param("primary_only", false)?;
    if !ctx.bool_param("wait_for_completion", true)? {
        // The merge runs as a task whose outcome (even a missing index) is kept for `_tasks`.
        let mut description = format!(
            "Force-merge indices [{}], maxSegments[{max_segments}], onlyExpungeDeletes[{expunge}], flush[{flush}]",
            super::index_expressions(ctx).join(", ")
        );
        if os {
            description.push_str(&format!(", primaryOnly[{primary_only}]"));
        }
        let headers = ctx.request.header("x-opaque-id").map(|v| vec![("X-Opaque-Id".to_string(), v.to_string())]).unwrap_or_default();
        let id = ctx.cluster.tasks.run_stored("indices:admin/forcemerge", &description, headers, || broadcast_value(ctx, primary_only));
        return Reply::ok(obj! { "task" => format!("{}:{id}", ctx.cluster.node.id) });
    }
    Reply::ok(broadcast_value(ctx, primary_only)?)
}

/// `_refresh`, `_flush`, `_forcemerge`: nothing to do yet for metadata-only indices; reports
/// shards like Elasticsearch.
pub fn broadcast(ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(broadcast_value(ctx, false)?)
}

/// The `_shards` summary of a broadcast over the request's open indices (`primary_only`:
/// OpenSearch's force merge of primaries only).
fn broadcast_value(ctx: &Ctx, primary_only: bool) -> EsResult<Value> {
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    let mut total = 0u64;
    let mut ok = 0u64;
    for n in &names {
        let idx = &st.metadata.indices[n];
        if idx.is_closed() {
            continue;
        }
        let copies = if primary_only { 1 } else { 1 + idx.number_of_replicas() as u64 };
        total += idx.number_of_shards() as u64 * copies;
        ok += idx.number_of_shards() as u64;
    }
    Ok(obj! { "_shards" => obj! { "total" => total, "successful" => ok, "failed" => 0 } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_shards_like_elasticsearch() {
        assert_eq!(routing_num_shards(1), 1024);
        assert_eq!(routing_num_shards(2), 1024);
        assert_eq!(routing_num_shards(3), 768);
        assert_eq!(routing_num_shards(5), 640);
        assert_eq!(routing_num_shards(1024), 2048);
    }
}

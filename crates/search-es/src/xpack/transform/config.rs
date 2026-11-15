//! Transform configurations: parsing, defaults and validation (Elasticsearch's
//! `TransformConfig.validate` and `SourceDestValidator`), in Elasticsearch's order and words.

use super::*;

pub(super) const PUT_FIELDS: &[&str] = &["source", "dest", "pivot", "latest", "description", "frequency", "sync", "settings", "_meta", "retention_policy"];

pub(super) const UPDATE_FIELDS: &[&str] = &["source", "dest", "description", "frequency", "sync", "settings", "_meta", "retention_policy"];

/// Fields of a configuration only Elasticsearch writes (`lenient == false` parsing).
pub(super) const GENERATED_FIELDS: &[&str] = &["create_time", "version"];

/// `TransformConfig.MIN_FREQUENCY` and `MAX_FREQUENCY`.
pub(super) const MIN_FREQUENCY_MS: i64 = 1_000;

pub(super) const MAX_FREQUENCY_MS: i64 = 3_600_000;

/// The range of `max_page_search_size` (`TransformConfig` and `SettingsConfig`).
pub(super) const PAGE_RANGE: std::ops::RangeInclusive<i64> = 10..=65_536;

/// `TimeRetentionPolicyConfig.MIN_AGE`.
pub(super) const MIN_RETENTION_MS: i64 = 60_000;

/// `TimeSyncConfig.DEFAULT_DELAY`.
pub(super) const DEFAULT_SYNC_DELAY: &str = "60s";

/// The group_by sources and the fields each parser (`*_group`) accepts.
pub(super) const GROUP_PARSERS: &[(&str, &str, &[&str])] = &[
    ("terms", "data_frame_terms_group", &["field", "script", "missing_bucket"]),
    ("histogram", "data_frame_histogram_group", &["field", "script", "missing_bucket", "interval"]),
    ("date_histogram", "data_frame_date_histogram_group", &["field", "script", "missing_bucket", "calendar_interval", "fixed_interval", "interval", "time_zone", "offset"]),
    ("geotile_grid", "transform_geo_tile_group", &["field", "missing_bucket", "precision", "bounds"]),
];

/// The aggregations a pivot can use (`TransformAggregations.AggregationType`).
pub(super) const PIVOT_AGGS: &[&str] = &[
    "avg",
    "boxplot",
    "bucket_script",
    "bucket_selector",
    "cardinality",
    "extended_stats",
    "filter",
    "geo_bounds",
    "geo_centroid",
    "geo_line",
    "max",
    "median_absolute_deviation",
    "min",
    "missing",
    "percentiles",
    "range",
    "rare_terms",
    "scripted_metric",
    "stats",
    "sum",
    "terms",
    "top_metrics",
    "value_count",
    "weighted_avg",
];

/// The validation errors of a request (`action_request_validation_exception`).
pub(super) fn request_invalid(errors: Vec<String>) -> EsResult<()> {
    if errors.is_empty() { Ok(()) } else { Err(EsError::action_request_validation(&errors)) }
}

/// The validation errors of the cluster checks (`validation_exception`).
pub(super) fn validation_failed(errors: &[String]) -> EsError {
    EsError::new(400, "validation_exception", format!("Validation Failed: {}", numbered(errors)))
}

/// The source section with its defaults (`index` as a list, `query` match_all).
pub(super) fn normalize_source(src: &Value) -> EsResult<Value> {
    let Value::Obj(m) = src else { return Err(EsError::illegal_argument("Required [index]")) };
    let mut out = Map::new();
    let index = match m.get("index") {
        Some(Value::Str(s)) => Value::Arr(s.split(',').map(|p| Value::from(p.trim())).collect()),
        Some(Value::Arr(a)) => Value::Arr(a.clone()),
        _ => return Err(EsError::illegal_argument("Required [index]")),
    };
    out.insert("index".into(), index);
    out.insert("query".into(), m.get("query").cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} }));
    for (k, v) in m {
        if k != "index" && k != "query" {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Obj(out))
}

/// The pivot with `aggs` spelled `aggregations`, as Elasticsearch prints it.
pub(super) fn normalize_pivot(p: &Value) -> Value {
    let Value::Obj(m) = p else { return p.clone() };
    let mut out = Map::new();
    for (k, v) in m {
        out.insert(if k == "aggs" { "aggregations".to_string() } else { k.clone() }, v.clone());
    }
    Value::Obj(out)
}

/// `SourceDestValidator`'s checks of the indices and the pipeline, in its order: the source
/// expressions resolve (the first missing concrete index fails; a wildcard may match nothing),
/// the destination isn't one of the sources (literally, or once aliases resolve), an alias as
/// destination has a write index (which ends the checks), and the ingest pipeline exists.
pub(super) fn source_dest_errors(st: &crate::meta::ClusterState, t: &Value, now: i64) -> Vec<String> {
    use crate::meta::resolve::{IndicesOptions, resolve};
    let mut errs = Vec::new();
    let sources = source_indices(t);
    let resolved = match resolve(st, &sources, &IndicesOptions::STRICT_EXPAND_OPEN, now) {
        Ok(r) => Some(r),
        Err(e) => {
            errs.push(e.reason.unwrap_or_default());
            None
        }
    };
    let dest = dest_index(t);
    let mut literal = false;
    for s in &sources {
        if crate::xcontent::filter::glob_match(s, dest) {
            errs.push(format!("Destination index [{dest}] is included in source expression [{s}]"));
            literal = true;
        }
    }
    if !literal {
        match crate::write::write_index_name(st, dest) {
            Err(e) => {
                errs.push(e.reason.unwrap_or_default());
                return errs;
            }
            Ok(target) => {
                let target = target.unwrap_or_else(|| dest.to_string());
                if let Some(r) = resolved.as_ref().filter(|r| r.contains(&target)) {
                    errs.push(format!("Destination index [{target}] is included in source expression [{}]", r.join(",")));
                }
            }
        }
    }
    if let Some(p) = t.pointer(&["dest", "pipeline"]).and_then(Value::as_str)
        && !st.metadata.pipelines.contains_key(p)
    {
        errs.push(format!("Pipeline with id [{p}] could not be found"));
    }
    errs
}

/// The aggregations of a pivot no transform can run.
pub(super) fn unsupported_aggs(t: &Value) -> EsResult<()> {
    let aggs = t.get("pivot").and_then(|p| p.get("aggregations").or_else(|| p.get("aggs"))).and_then(Value::as_object);
    for a in aggs.into_iter().flat_map(|m| m.values()) {
        if let Some(kind) = agg_kind(a)
            && !PIVOT_AGGS.contains(&kind)
        {
            return Err(validation_failed(&[format!("Unsupported aggregation type [{kind}]")]));
        }
    }
    Ok(())
}

/// Everything Elasticsearch checks against the cluster before it stores or starts a
/// transform: the source and destination, the function's aggregations, and a test search of
/// the source (`Failed to test query`), which compiles its scripts and runtime fields.
pub(super) async fn validate_against_cluster(cluster: &crate::Cluster, t: &Value, now: i64) -> EsResult<()> {
    let errs = source_dest_errors(&cluster.state(), t, now);
    if !errs.is_empty() {
        return Err(validation_failed(&errs));
    }
    unsupported_aggs(t)?;
    let req = composite_request(t, 1, None);
    if let Err(e) = internal::call(cluster, "POST", &search_uri(t), Some(&req)).await {
        return Err(validation_failed(&[format!("Failed to test query, received status: {}", status_name(e.status))]).caused_by(e));
    }
    Ok(())
}

/// `RestStatus` names.
pub(super) fn status_name(status: u16) -> &'static str {
    match status {
        400 => "BAD_REQUEST",
        401 => "UNAUTHORIZED",
        403 => "FORBIDDEN",
        404 => "NOT_FOUND",
        409 => "CONFLICT",
        429 => "TOO_MANY_REQUESTS",
        503 => "SERVICE_UNAVAILABLE",
        _ => "INTERNAL_SERVER_ERROR",
    }
}

/// `TransformConfig.validate` (and the update's): the checks of a configuration on its own,
/// in Elasticsearch's order.
pub(super) fn config_errors(id: Option<&str>, cfg: &Map) -> Vec<String> {
    let mut errs = Vec::new();
    let page_size = |errs: &mut Vec<String>, name: &str, v: Option<&Value>| {
        if let Some(n) = v.and_then(Value::as_i64)
            && !PAGE_RANGE.contains(&n)
        {
            errs.push(format!("{name} [{n}] is out of range. The minimum value is {} and the maximum is {}", PAGE_RANGE.start(), PAGE_RANGE.end()));
        }
    };
    let settings = cfg.get("settings").cloned().unwrap_or(Value::Null);
    page_size(&mut errs, "settings.max_page_search_size", settings.get("max_page_search_size"));
    if let Some(n) = settings.get("num_failure_retries").and_then(Value::as_i64)
        && !(-1..=100).contains(&n)
    {
        errs.push(format!("settings.num_failure_retries [{n}] is out of range. The minimum value is -1 (infinity) and the maximum is 100"));
    }
    if let Some(p) = cfg.get("pivot") {
        page_size(&mut errs, "pivot.max_page_search_size", p.get("max_page_search_size"));
        errs.extend(output_name_errors(p));
    }
    if let Some(l) = cfg.get("latest") {
        errs.extend(latest_errors(l));
    }
    if let Some(age) = cfg.get("retention_policy").and_then(|r| r.pointer(&["time", "max_age"])) {
        let raw = age.to_es_string();
        if crate::units::TimeValue::parse(&raw, "max_age").is_ok_and(|v| v.as_millis() < MIN_RETENTION_MS) {
            errs.push(format!("retention_policy.time.max_age must be greater than 60s, found [{raw}]"));
        }
    }
    if let Some(dest) = cfg.get("dest").and_then(|d| d.get("index")).and_then(Value::as_str) {
        if let Some(p) = crate::meta::names::name_problem(dest) {
            errs.push(format!("Invalid index name [{dest}], {p}"));
        } else if dest.chars().any(char::is_uppercase) {
            errs.push(format!("Destination index [{dest}] must be lowercase"));
        }
    }
    if let Some(id) = id {
        let b = id.as_bytes();
        let chars_ok = !b.is_empty()
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric();
        if !chars_ok {
            errs.push(format!(
                "Invalid id; '{id}' can contain lowercase alphanumeric (a-z and 0-9), hyphens or underscores; must start and end with alphanumeric"
            ));
        }
        if id.len() > 64 {
            errs.push("The id cannot contain more than 64 characters.".into());
        }
    }
    if let Some(f) = cfg.get("frequency")
        && let Ok(v) = crate::units::TimeValue::parse(&f.to_es_string(), "frequency")
    {
        if v.as_millis() < MIN_FREQUENCY_MS {
            errs.push("minimum permitted [frequency] is [1s]".into());
        } else if v.as_millis() > MAX_FREQUENCY_MS {
            errs.push("highest permitted [frequency] is [1h]".into());
        }
    }
    errs
}

/// `PivotConfig.aggFieldValidation`: the output field names (group names and the leaf
/// aggregations' dotted paths), sorted, must be unique and none a prefix object of another.
pub(super) fn output_name_errors(pivot: &Value) -> Vec<String> {
    fn leaves(prefix: &str, name: &str, agg: &Value, out: &mut Vec<String>) {
        let subs = agg.get("aggregations").or_else(|| agg.get("aggs")).and_then(Value::as_object).filter(|m| !m.is_empty());
        match subs {
            None => out.push(format!("{prefix}{name}")),
            Some(subs) => {
                let prefix = format!("{prefix}{name}.");
                for (n, a) in subs {
                    leaves(&prefix, n, a, out);
                }
            }
        }
    }
    let mut names: Vec<String> = Vec::new();
    for (n, a) in pivot.get("aggregations").or_else(|| pivot.get("aggs")).and_then(Value::as_object).into_iter().flatten() {
        leaves("", n, a, &mut names);
    }
    names.extend(pivot.get("group_by").and_then(Value::as_object).into_iter().flat_map(|g| g.keys().cloned()));
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let mut errs = Vec::new();
    for w in names.windows(2) {
        if w[1] == w[0] {
            errs.push(format!("duplicate field [{}] detected", w[1]));
        } else if w[1].starts_with(&format!("{}.", w[0])) {
            errs.push(format!("field [{}] cannot be both an object and a field", w[0]));
        }
    }
    errs
}

/// `LatestConfig.validate`.
pub(super) fn latest_errors(l: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let keys: Vec<String> = l.get("unique_key").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    if keys.is_empty() {
        errs.push("latest.unique_key must be non-empty".to_string());
    } else {
        let mut seen = std::collections::HashSet::new();
        for (i, k) in keys.iter().enumerate() {
            if k.is_empty() {
                errs.push(format!("latest.unique_key[{i}] element must be non-empty"));
            } else if !seen.insert(k.as_str()) {
                errs.push(format!("latest.unique_key elements must be unique, found duplicate element [{k}]"));
            }
        }
    }
    if l.get("sort").and_then(Value::as_str).is_none_or(str::is_empty) {
        errs.push("latest.sort must be non-empty".to_string());
    }
    errs
}

/// Parses a pivot's group_by as Elasticsearch's group parsers do.
pub(super) fn check_group_by(ctx: &Ctx, p: &Value) -> EsResult<()> {
    let groups = p.get("group_by").and_then(Value::as_object);
    if groups.is_none_or(|g| g.is_empty()) {
        return Err(EsError::illegal_argument("Required [group_by]"));
    }
    for (name, g) in groups.into_iter().flatten() {
        let (kind, body) = g.as_object().and_then(|m| m.iter().next()).map(|(k, v)| (k.as_str(), v)).unwrap_or(("", &Value::Null));
        let Some((_, parser, fields)) = GROUP_PARSERS.iter().find(|(k, _, _)| *k == kind) else {
            return Err(EsError::parse(format!("Unknown group_by type [{kind}] for [{name}]")));
        };
        if let Some(bad) = body.as_object().and_then(|m| m.keys().find(|k| !fields.contains(&k.as_str()))) {
            let (l, c) = crate::xpack::key_loc(ctx, "pivot");
            return Err(EsError::x_content_parse(l, c, "[data_frame_transform_config] failed to parse field [pivot]").caused_by(
                EsError::x_content_parse(l, c, "[data_frame_transform_pivot] failed to parse field [group_by]")
                    .caused_by(EsError::x_content_parse(l, c, format!("[{parser}] unknown field [{bad}]"))),
            ));
        }
    }
    Ok(())
}

/// `sync.time` with its default `delay`.
pub(super) fn normalize_sync(sync: &Value) -> Value {
    let mut sync = sync.clone();
    if let Some(Value::Obj(time)) = sync.as_object_mut().and_then(|m| m.get_mut("time")) {
        time.entry("delay".to_string()).or_insert_with(|| Value::from(DEFAULT_SYNC_DELAY));
    }
    sync
}

pub(super) fn build_config(ctx: &Ctx, id: &str, body: &Map, now: i64) -> EsResult<Value> {
    for k in body.keys() {
        if !PUT_FIELDS.contains(&k.as_str()) && !GENERATED_FIELDS.contains(&k.as_str()) {
            return Err(unknown_field(ctx, "data_frame_transform_config", k));
        }
    }
    let missing: Vec<&str> = ["source", "dest"].into_iter().filter(|k| !body.contains_key(*k)).collect();
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    let build_failed = |cause: EsError| EsError::x_content_parse_bare("Failed to build [data_frame_transform_config] after last required field arrived").caused_by(cause);
    if let Some(k) = GENERATED_FIELDS.iter().find(|k| body.contains_key(**k)) {
        return Err(build_failed(EsError::illegal_argument(format!("Found [{k}], not allowed for strict parsing"))));
    }
    if body.contains_key("pivot") == body.contains_key("latest") {
        return Err(build_failed(EsError::illegal_argument("Transform configuration must specify exactly 1 function")));
    }
    let dest = body.get("dest").cloned().unwrap_or(Value::Null);
    if dest.get("index").and_then(Value::as_str).is_none() {
        return Err(EsError::illegal_argument("Required [index]"));
    }
    if let Some(p) = body.get("pivot") {
        check_group_by(ctx, p)?;
    }
    let roles: Vec<Value> = crate::xpack::security::principal(ctx).roles.iter().map(|r| Value::from(r.as_str())).collect();
    let mut out = Map::new();
    out.insert("id".into(), id.into());
    out.insert("authorization".into(), obj! { "roles" => Value::Arr(roles) });
    out.insert("version".into(), CONFIG_VERSION.into());
    out.insert("create_time".into(), now.into());
    out.insert("source".into(), normalize_source(body.get("source").unwrap_or(&Value::Null))?);
    out.insert("dest".into(), dest);
    if let Some(v) = body.get("frequency") {
        out.insert("frequency".into(), v.clone());
    }
    if let Some(v) = body.get("sync") {
        out.insert("sync".into(), normalize_sync(v));
    }
    if let Some(p) = body.get("pivot") {
        out.insert("pivot".into(), normalize_pivot(p));
    }
    if let Some(l) = body.get("latest") {
        out.insert("latest".into(), l.clone());
    }
    for k in ["description", "_meta", "retention_policy"] {
        if let Some(v) = body.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    out.insert("settings".into(), body.get("settings").cloned().unwrap_or_else(Value::object));
    let out = Value::Obj(out);
    request_invalid(config_errors(Some(id), out.as_object().expect("an object")))?;
    Ok(out)
}

pub(super) fn numbered(msgs: &[String]) -> String {
    msgs.iter().enumerate().map(|(i, m)| format!("{}: {m};", i + 1)).collect()
}

/// Elasticsearch's field order of a transform configuration.
pub(super) fn reorder(mut m: Map) -> Map {
    const ORDER: &[&str] =
        &["id", "authorization", "version", "create_time", "source", "dest", "frequency", "sync", "pivot", "latest", "description", "settings", "_meta", "retention_policy", "_roll"];
    let mut out = Map::new();
    for k in ORDER {
        if let Some(v) = m.shift_remove(*k) {
            out.insert(k.to_string(), v);
        }
    }
    out.extend(m);
    out
}

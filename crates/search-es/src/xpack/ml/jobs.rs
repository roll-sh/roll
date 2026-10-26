//! Anomaly detection jobs, datafeeds, calendars, filters and job results.
//!
//! A job's results live in the shared results index `.ml-anomalies-shared-000001`, reached
//! through the job's aliases `.ml-anomalies-<job>` (filtered on `job_id`) and
//! `.ml-anomalies-.write-<job>`, created with the job as Elasticsearch does; the results APIs
//! search them and print results with Elasticsearch's defaults (epoch-millisecond timestamps,
//! `is_interim: false`, ...).

use super::*;
use crate::json::locate::Seg;
use crate::meta::AliasMeta;
use crate::units::TimeValue;
use crate::xpack::{internal, store};

pub const RESULTS_INDEX: &str = ".ml-anomalies-shared-000001";

fn no_job(id: &str) -> EsError {
    EsError::resource_not_found(format!("No known job with id '{id}'"))
}

fn no_datafeed(id: &str) -> EsError {
    EsError::resource_not_found(format!("No datafeed with id [{id}] exists"))
}

fn conflict(reason: impl Into<String>) -> EsError {
    EsError::status(409, reason)
}

fn results_mapping() -> Value {
    let kw = || obj! { "type" => "keyword" };
    let dbl = || obj! { "type" => "double" };
    obj! {
        "job_id" => kw(), "result_type" => kw(), "timestamp" => obj! { "type" => "date" }, "bucket_span" => obj! { "type" => "long" },
        "anomaly_score" => dbl(), "initial_anomaly_score" => dbl(), "record_score" => dbl(), "initial_record_score" => dbl(),
        "influencer_score" => dbl(), "initial_influencer_score" => dbl(), "probability" => dbl(), "is_interim" => obj! { "type" => "boolean" },
        "influencer_field_name" => kw(), "influencer_field_value" => kw(), "partition_field_name" => kw(), "partition_field_value" => kw(),
        "by_field_name" => kw(), "by_field_value" => kw(), "over_field_name" => kw(), "over_field_value" => kw(),
        "snapshot_id" => kw(), "category_id" => obj! { "type" => "long" }, "detector_index" => obj! { "type" => "integer" },
        "forecast_id" => kw(), "min_version" => kw(), "retain" => obj! { "type" => "boolean" },
    }
}

/// Model memory limits print in whole megabytes (`1gb` → `1024mb`).
fn memory_limit(v: Option<&Value>) -> EsResult<String> {
    let Some(v) = v else { return Ok("1024mb".into()) };
    let mb = match v {
        Value::Num(n) => n.as_f64() as i64,
        other => {
            let s = other.to_es_string();
            match s.parse::<i64>() {
                Ok(n) => n,
                Err(_) => crate::units::ByteSize::parse(&s, "model_memory_limit")?.bytes() >> 20,
            }
        }
    };
    if mb < 1 {
        return Err(EsError::illegal_argument(format!("Invalid analysis_limits; model_memory_limit must be at least 1 MiB. Value = {}", mb.max(0))));
    }
    Ok(format!("{mb}mb"))
}

/// `Detector.getDescription`'s default text.
fn detector_description(d: &Map) -> String {
    let f = d.get("function").map(Value::to_es_string).unwrap_or_default();
    let mut s = match d.get("field_name") {
        Some(fv) => format!("{f}({})", fv.to_es_string()),
        None => f,
    };
    for (key, label) in [("by_field_name", " by "), ("over_field_name", " over ")] {
        if let Some(v) = d.get(key) {
            s.push_str(label);
            s.push_str(&v.to_es_string());
        }
    }
    if let Some(v) = d.get("partition_field_name") {
        s.push_str(" partitionfield=");
        s.push_str(&v.to_es_string());
    }
    if let Some(v) = d.get("exclude_frequent") {
        s.push_str(" excludefrequent=");
        s.push_str(&v.to_es_string());
    }
    s
}

const FUNCTIONS: &[&str] = &[
    "count", "low_count", "high_count", "non_zero_count", "low_non_zero_count", "high_non_zero_count", "distinct_count", "low_distinct_count",
    "high_distinct_count", "rare", "freq_rare", "info_content", "low_info_content", "high_info_content", "metric", "mean", "low_mean", "high_mean",
    "avg", "low_avg", "high_avg", "median", "low_median", "high_median", "min", "max", "sum", "low_sum", "high_sum", "non_null_sum",
    "low_non_null_sum", "high_non_null_sum", "varp", "low_varp", "high_varp", "time_of_day", "time_of_week", "lat_long",
];

/// `Detector` function sets (`Detector.COUNT_WITHOUT_FIELD_FUNCTIONS` and friends).
const COUNT_WITHOUT_FIELD: &[&str] = &["count", "high_count", "low_count", "non_zero_count", "low_non_zero_count", "high_non_zero_count", "time_of_day", "time_of_week"];
const FIELD_NAME_FUNCTIONS: &[&str] = &[
    "distinct_count", "low_distinct_count", "high_distinct_count", "info_content", "low_info_content", "high_info_content", "metric", "mean", "avg",
    "high_mean", "high_avg", "low_mean", "low_avg", "median", "low_median", "high_median", "min", "max", "sum", "low_sum", "high_sum", "non_null_sum",
    "low_non_null_sum", "high_non_null_sum", "varp", "low_varp", "high_varp", "lat_long",
];

fn bad_request(msg: impl Into<String>) -> EsError {
    EsError::status(400, msg)
}

/// `Detector.Builder.build`'s checks, in its order (`function`: already known to be valid).
fn check_detector(dm: &Map) -> EsResult<()> {
    let s = |k: &str| dm.get(k).filter(|v| !v.is_null()).map(Value::to_es_string).filter(|v| !v.is_empty());
    let f = s("function").unwrap_or_default();
    let (field, by, over, partition) = (s("field_name"), s("by_field_name"), s("over_field_name"), s("partition_field_name"));
    let has = |f: &[&str], x: &str| f.contains(&x);
    if field.is_none() && by.is_none() && over.is_none() && !has(COUNT_WITHOUT_FIELD, &f) {
        return Err(bad_request("Unless a count or temporal function is used one of field_name, by_field_name or over_field_name must be set"));
    }
    if field.is_none() && has(FIELD_NAME_FUNCTIONS, &f) {
        return Err(bad_request(format!("field_name must be set when the '{f}' function is used")));
    }
    if field.is_some() && !has(FIELD_NAME_FUNCTIONS, &f) {
        return Err(bad_request(format!("field_name cannot be used with function '{f}'")));
    }
    if by.is_none() && matches!(f.as_str(), "rare" | "freq_rare") {
        return Err(bad_request(format!("by_field_name must be set when the '{f}' function is used")));
    }
    if over.is_none() && f == "freq_rare" {
        return Err(bad_request(format!("over_field_name must be set when the '{f}' function is used")));
    }
    if over.is_some() && matches!(f.as_str(), "non_zero_count" | "low_non_zero_count" | "high_non_zero_count") {
        return Err(bad_request(format!("over_field_name cannot be used with function '{f}'")));
    }
    for name in [&field, &by, &over, &partition].into_iter().flatten() {
        if name.contains('"') || name.contains('\\') {
            return Err(bad_request(format!("Invalid field name '{name}'. Field names including over, by and partition fields cannot contain any of these characters: \",\\")));
        }
    }
    if let Some(p) = &partition {
        if Some(p) == by.as_ref() {
            return Err(bad_request(format!("partition_field_name and by_field_name cannot be the same: '{p}'")));
        }
        if Some(p) == over.as_ref() {
            return Err(bad_request(format!("partition_field_name and over_field_name cannot be the same: '{p}'")));
        }
    }
    if let Some(b) = &by
        && Some(b) == over.as_ref()
    {
        return Err(bad_request(format!("by_field_name and over_field_name cannot be the same: '{b}'")));
    }
    for (word, what) in [("count", "count"), ("by", "by"), ("over", "over")] {
        for (key, v) in [("by_field_name", &by), ("over_field_name", &over)] {
            if v.as_deref() == Some(word) {
                return Err(bad_request(format!("'{what}' is not a permitted value for {key}")));
            }
        }
    }
    Ok(())
}

/// Parses a detector object at `path` in the request body (`src`), with the errors and
/// locations of `Detector.STRICT_PARSER` followed by `Builder.build`.
pub(crate) fn parse_detector(src: Option<&[u8]>, path: &[Seg<'_>], d: &Value, index: usize) -> Result<Map, (EsError, (usize, usize))> {
    use crate::json::locate::{Target, locate, value_end};
    let at = |p: &[Seg<'_>], t: Option<Target>| -> (usize, usize) {
        src.and_then(|s| match t {
            Some(t) => locate(s, p, t),
            None => value_end(s, p),
        })
        .unwrap_or((1, 1))
    };
    let Some(dm) = d.as_object() else { return Err((EsError::illegal_argument("detector must be an object"), at(path, Some(Target::Value)))) };
    const DETECTOR_FIELDS: [&str; 10] = [
        "detector_description", "function", "field_name", "by_field_name", "over_field_name", "partition_field_name", "use_null", "exclude_frequent",
        "custom_rules", "detector_index",
    ];
    if let Some(k) = dm.keys().find(|k| !DETECTOR_FIELDS.contains(&k.as_str())) {
        let loc = src.and_then(|s| crate::json::locate::key_location(s, path, k)).unwrap_or((1, 1));
        return Err((EsError::x_content_parse(loc.0, loc.1, format!("[detector] unknown field [{k}]")), loc));
    }
    if let Some(f) = dm.get("function") {
        let f = f.to_es_string();
        if !FUNCTIONS.contains(&f.as_str()) {
            let mut fp = path.to_vec();
            fp.push(Seg::Key("function"));
            let loc = at(&fp, Some(Target::Value));
            return Err((
                EsError::x_content_parse(loc.0, loc.1, "[detector] failed to parse field [function]").caused_by(EsError::illegal_argument(format!("Unknown function '{f}'"))),
                loc,
            ));
        }
    }
    check_detector(dm).map_err(|e| (e, at(path, None)))?;
    let mut o = Map::new();
    o.insert("detector_description".into(), dm.get("detector_description").cloned().unwrap_or_else(|| Value::from(detector_description(dm))));
    for k in ["function", "field_name", "by_field_name", "over_field_name", "partition_field_name", "use_null", "exclude_frequent", "custom_rules"] {
        if let Some(x) = dm.get(k) {
            o.insert(k.into(), x.clone());
        }
    }
    o.insert("detector_index".into(), index.into());
    Ok(o)
}

/// Parses `analysis_config` at `path` in the request body: errors carry the location every
/// enclosing parser repeats (a detector's, else the object's closing brace).
fn parse_analysis_config(src: Option<&[u8]>, path: &[Seg<'_>], v: &Value) -> Result<Value, (EsError, (usize, usize))> {
    let end = || src.and_then(|s| crate::json::locate::value_end(s, path)).unwrap_or((1, 1));
    let Some(m) = v.as_object() else { return Err((EsError::illegal_argument("[analysis_config] must be an object"), end())) };
    let mut detectors = Vec::new();
    if let Some(Value::Arr(dets)) = m.get("detectors") {
        for (i, d) in dets.iter().enumerate() {
            let mut dp = path.to_vec();
            dp.push(Seg::Key("detectors"));
            dp.push(Seg::Index(i));
            let o = parse_detector(src, &dp, d, i).map_err(|(e, loc)| (EsError::x_content_parse(loc.0, loc.1, "[analysis_config] failed to parse field [detectors]").caused_by(e), loc))?;
            detectors.push(Value::Obj(o));
        }
    }
    let bucket_span = m.get("bucket_span").map(Value::to_es_string).unwrap_or_else(|| "5m".into());
    let span = TimeValue::parse(&bucket_span, "bucket_span").map_err(|e| (e, end()))?;
    if span.as_millis() <= 0 {
        return Err((EsError::illegal_argument(format!("bucket_span cannot be less or equal than 0. Value = {}", span.as_millis())), end()));
    }
    if span.as_millis() % 1000 != 0 {
        return Err((EsError::illegal_argument(format!("bucket_span has to be a multiple of 1s; actual was '{bucket_span}'")), end()));
    }
    if detectors.is_empty() {
        return Err((bad_request("No detectors configured"), end()));
    }
    let mut out = Map::new();
    out.insert("bucket_span".into(), Value::from(bucket_span));
    for k in ["categorization_field_name", "categorization_filters", "categorization_analyzer", "per_partition_categorization", "summary_count_field_name", "latency"] {
        if let Some(x) = m.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    out.insert("detectors".into(), Value::Arr(detectors));
    out.insert("influencers".into(), m.get("influencers").cloned().unwrap_or_else(|| Value::Arr(Vec::new())));
    if let Some(x) = m.get("multivariate_by_fields") {
        out.insert("multivariate_by_fields".into(), x.clone());
    }
    out.insert("model_prune_window".into(), m.get("model_prune_window").cloned().unwrap_or_else(|| Value::from("30d")));
    Ok(Value::Obj(out))
}

fn parse_data_description(v: Option<&Value>) -> Value {
    let m = v.and_then(Value::as_object).cloned().unwrap_or_default();
    let mut out = Map::new();
    out.insert("time_field".into(), m.get("time_field").cloned().unwrap_or_else(|| Value::from("time")));
    out.insert("time_format".into(), m.get("time_format").cloned().unwrap_or_else(|| Value::from("epoch_ms")));
    for k in ["field_delimiter", "quote_character"] {
        if let Some(x) = m.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    Value::Obj(out)
}

/// The field order of `Job.toXContent` (roll's own bookkeeping last).
const JOB_ORDER: &[&str] = &[
    "job_id", "job_type", "job_version", "create_time", "finished_time", "model_snapshot_id", "deleting", "model_snapshot_min_version",
    "custom_settings", "datafeed_config", "groups", "description", "analysis_config", "analysis_limits", "data_description", "model_plot_config",
    "renormalization_window_days", "background_persist_interval", "model_snapshot_retention_days", "daily_model_snapshot_retention_after_days",
    "results_retention_days", "results_index_name", "allow_lazy_open", "blocked",
];

/// A job with its fields in [`JOB_ORDER`].
fn job_order(v: Value) -> Value {
    let Value::Obj(mut m) = v else { return v };
    let mut out = Map::with_capacity(m.len());
    for k in JOB_ORDER {
        if let Some(x) = m.shift_remove(*k) {
            out.insert((*k).to_string(), x);
        }
    }
    out.extend(m);
    Value::Obj(out)
}

/// Builds a job config in Elasticsearch's field order from a put body.
fn build_job(src: Option<&[u8]>, id: &str, body: &Map, now: i64) -> EsResult<Value> {
    let mut out = Map::new();
    out.insert("job_id".into(), id.into());
    out.insert("job_type".into(), "anomaly_detector".into());
    out.insert("job_version".into(), CONFIG_VERSION.into());
    if let Some(g) = body.get("groups") {
        out.insert("groups".into(), g.clone());
    }
    out.insert("create_time".into(), now.into());
    if let Some(d) = body.get("description") {
        out.insert("description".into(), d.clone());
    }
    let Some(ac) = body.get("analysis_config") else {
        return Err(EsError::action_request_validation(&["An analysis_config must be set".into()]));
    };
    let ac = parse_analysis_config(src, &[Seg::Key("analysis_config")], ac)
        .map_err(|(e, (l, c))| EsError::x_content_parse(l, c, "[job_details] failed to parse field [analysis_config]").caused_by(e))?;
    out.insert("analysis_config".into(), ac);
    let limits = body.get("analysis_limits").and_then(Value::as_object).cloned().unwrap_or_default();
    out.insert(
        "analysis_limits".into(),
        obj! {
            "model_memory_limit" => memory_limit(limits.get("model_memory_limit"))?,
            "categorization_examples_limit" => limits.get("categorization_examples_limit").cloned().unwrap_or(Value::from(4)),
        },
    );
    out.insert("data_description".into(), parse_data_description(body.get("data_description")));
    for k in ["model_plot_config", "renormalization_window_days", "background_persist_interval"] {
        if let Some(x) = body.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    out.insert("model_snapshot_retention_days".into(), body.get("model_snapshot_retention_days").cloned().unwrap_or(Value::from(10)));
    out.insert("daily_model_snapshot_retention_after_days".into(), body.get("daily_model_snapshot_retention_after_days").cloned().unwrap_or(Value::from(1)));
    for k in ["results_retention_days", "custom_settings"] {
        if let Some(x) = body.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    out.insert("results_index_name".into(), body.get("results_index_name").cloned().unwrap_or(Value::from("shared")));
    out.insert("allow_lazy_open".into(), body.get("allow_lazy_open").cloned().unwrap_or(Value::Bool(false)));
    Ok(job_order(Value::Obj(out)))
}

const JOB_FIELDS: &[&str] = &[
    "job_id", "description", "groups", "analysis_config", "analysis_limits", "data_description", "model_plot_config", "renormalization_window_days",
    "background_persist_interval", "model_snapshot_retention_days", "daily_model_snapshot_retention_after_days", "results_retention_days",
    "custom_settings", "results_index_name", "allow_lazy_open", "datafeed_config", "job_type", "job_version", "create_time",
];

pub async fn put_job(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    if let Some(b) = body.get("job_id").map(Value::to_es_string)
        && b != id
    {
        return Err(EsError::illegal_argument(format!("Inconsistent job_id; '{b}' specified in the body differs from '{id}' specified as a URL argument")));
    }
    for k in body.keys() {
        if !JOB_FIELDS.contains(&k.as_str()) {
            return Err(crate::xpack::unknown_field(ctx, "job_details", k));
        }
    }
    if !valid_id(&id) {
        return Err(invalid_id("job_id", &id));
    }
    let job = build_job(ctx.raw_json(), &id, &body, ctx.now_ms)?;
    let datafeed = body.get("datafeed_config").cloned();
    let now = ctx.now_ms;
    let out = job.clone();
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(JOBS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("The job cannot be created with the Id '{id}'. The Id is already used.")));
            }
            store::ensure_index_with(st, RESULTS_INDEX, None, Some(results_mapping()), now)?;
            store::ensure_index(st, ".ml-annotations-000001", Some(".ml-annotations-read"), now)?;
            store::set_alias(st, ".ml-annotations-000001", ".ml-annotations-write", AliasMeta { is_hidden: Some(true), ..Default::default() });
            store::ensure_index(st, ".ml-notifications-000002", Some(".ml-notifications-write"), now)?;
            store::set_alias(
                st,
                RESULTS_INDEX,
                &format!(".ml-anomalies-{id}"),
                AliasMeta {
                    filter: Some(obj! { "term" => obj! { "job_id" => obj! { "value" => id.clone() } } }),
                    is_hidden: Some(true),
                    ..Default::default()
                },
            );
            store::set_alias(st, RESULTS_INDEX, &format!(".ml-anomalies-.write-{id}"), AliasMeta { is_hidden: Some(true), ..Default::default() });
            store::put(st, JOBS, &id, job, now)?;
            if let Some(Value::Obj(mut df)) = datafeed {
                df.insert("job_id".into(), id.clone().into());
                let df_id = df.get("datafeed_id").map(Value::to_es_string).unwrap_or_else(|| id.clone());
                let v = build_datafeed(&df_id, &df, &["superuser".to_string()])?;
                store::put(st, DATAFEEDS, &df_id, v, now)?;
            }
            Ok(())
        })
        .await?;
    Reply::ok(out)
}

pub fn get_jobs(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let jobs = expand(&st, JOBS, ctx.path_param("job_id"), allow, no_job)?;
    let list: Vec<Value> = jobs.iter().map(|(_, j)| public(j)).collect();
    Reply::ok(obj! { "count" => list.len(), "jobs" => Value::Arr(list) })
}

fn data_counts(id: &str) -> Value {
    obj! {
        "job_id" => id, "processed_record_count" => 0, "processed_field_count" => 0, "input_bytes" => 0, "input_field_count" => 0,
        "invalid_date_count" => 0, "missing_field_count" => 0, "out_of_order_timestamp_count" => 0, "empty_bucket_count" => 0,
        "sparse_bucket_count" => 0, "bucket_count" => 0, "input_record_count" => 0,
    }
}

pub fn job_stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let jobs = expand(&st, JOBS, ctx.path_param("job_id"), allow, no_job)?;
    let mut out = Vec::new();
    for (id, j) in jobs {
        let rs = roll_state(&j);
        let opened = rs.get("state").and_then(Value::as_str) == Some("opened");
        let mut m = Map::new();
        m.insert("job_id".into(), id.clone().into());
        m.insert("data_counts".into(), data_counts(&id));
        let mut mss = Map::new();
        mss.insert("job_id".into(), id.clone().into());
        mss.insert("result_type".into(), "model_size_stats".into());
        mss.insert("model_bytes".into(), 0.into());
        if opened {
            let limit = j.pointer(&["analysis_limits", "model_memory_limit"]).map(Value::to_es_string).unwrap_or_default();
            mss.insert("model_bytes_memory_limit".into(), limit.trim_end_matches("mb").parse::<i64>().unwrap_or(0).into());
        }
        for k in ["total_by_field_count", "total_over_field_count", "total_partition_field_count", "bucket_allocation_failures_count"] {
            mss.insert(k.into(), 0.into());
        }
        mss.insert("memory_status".into(), "ok".into());
        if opened {
            mss.insert("assignment_memory_basis".into(), "model_memory_limit".into());
        }
        for k in ["categorized_doc_count", "total_category_count", "frequent_category_count", "rare_category_count", "dead_category_count", "failed_category_count"] {
            mss.insert(k.into(), 0.into());
        }
        mss.insert("categorization_status".into(), "ok".into());
        mss.insert("log_time".into(), ctx.now_ms.into());
        m.insert("model_size_stats".into(), Value::Obj(mss));
        m.insert("forecasts_stats".into(), obj! { "total" => 0, "forecasted_jobs" => 0 });
        m.insert("state".into(), (if opened { "opened" } else { "closed" }).into());
        if opened {
            m.insert("node".into(), node_value(ctx, true));
            m.insert("assignment_explanation".into(), "".into());
            let since = rs.get("open_ms").and_then(Value::as_i64).unwrap_or(ctx.now_ms);
            m.insert("open_time".into(), format!("{}s", (ctx.now_ms - since).max(0) / 1000).into());
        }
        m.insert("timing_stats".into(), obj! { "job_id" => id.clone(), "bucket_count" => 0, "total_bucket_processing_time_ms" => 0.0, "exponential_average_bucket_processing_time_per_hour_ms" => 0.0 });
        out.push(Value::Obj(m));
    }
    Reply::ok(obj! { "count" => out.len(), "jobs" => Value::Arr(out) })
}

async fn delete_results(ctx: &Ctx<'_>, job: &str) -> EsResult<()> {
    if ctx.state().metadata.indices.contains_key(RESULTS_INDEX) {
        let q = obj! { "query" => obj! { "term" => obj! { "job_id" => job } } };
        internal::call(ctx.cluster, "POST", &format!("/{RESULTS_INDEX}/_delete_by_query?refresh=true&conflicts=proceed"), Some(&q)).await?;
    }
    Ok(())
}

pub async fn delete_job(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    let force = ctx.bool_param("force", false)?;
    let st = ctx.state();
    let Some(job) = st.metadata.xpack.get(JOBS, &id) else { return Err(no_job(&id)) };
    let feeds: Vec<String> = st.metadata.xpack.iter(DATAFEEDS).filter(|(_, d)| d.get("job_id").and_then(Value::as_str) == Some(id.as_str())).map(|(k, _)| k.clone()).collect();
    if !force {
        if roll_state(job).get("state").and_then(Value::as_str) == Some("opened") {
            return Err(conflict(format!("Cannot delete job [{id}] because the job is opened")));
        }
        if let Some(f) = feeds.first() {
            return Err(conflict(format!("Cannot delete job [{id}] because datafeed [{f}] refers to it")));
        }
    }
    delete_results(ctx, &id).await?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            store::remove(st, JOBS, &id);
            for f in &feeds {
                store::remove(st, DATAFEEDS, f);
            }
            store::remove_alias(st, &format!(".ml-anomalies-{id}"));
            store::remove_alias(st, &format!(".ml-anomalies-.write-{id}"));
            let cals: Vec<(String, Value)> = st.metadata.xpack.iter(CALENDARS).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, mut c) in cals {
                if let Some(Value::Arr(ids)) = c.get_mut_job_ids() {
                    let before = ids.len();
                    ids.retain(|j| j.as_str() != Some(id.as_str()));
                    if ids.len() != before {
                        store::put(st, CALENDARS, &k, c, now)?;
                    }
                }
            }
            Ok(())
        })
        .await?;
    if ctx.query("wait_for_completion") == Some("false") {
        return Reply::ok(obj! { "task" => format!("{}:{}", ctx.cluster.node.id, ctx.now_ms % 100_000) });
    }
    Reply::acknowledged()
}

trait JobIds {
    fn get_mut_job_ids(&mut self) -> Option<&mut Value>;
}

impl JobIds for Value {
    fn get_mut_job_ids(&mut self) -> Option<&mut Value> {
        self.as_object_mut()?.get_mut("job_ids")
    }
}

async fn set_job_state(ctx: &Ctx<'_>, id: &str, state: &'static str) -> EsResult<()> {
    let id = id.to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(j) = st.metadata.xpack.get(JOBS, &id).cloned() else { return Err(no_job(&id)) };
            let was_open = roll_state(&j).get("state").and_then(Value::as_str) == Some("opened");
            let mut j = with_roll_state(j, |r| {
                r.insert("state".into(), state.into());
                if state == "opened" {
                    r.insert("open_ms".into(), now.into());
                }
            });
            // Closing an opened job records when its process finished.
            if was_open
                && state == "closed"
                && let Value::Obj(m) = &mut j
            {
                m.insert("finished_time".into(), now.into());
            }
            store::put(st, JOBS, &id, job_order(j), now).map(|_| ())
        })
        .await
}

fn job_state(ctx: &Ctx, id: &str) -> EsResult<String> {
    let st = ctx.state();
    let j = st.metadata.xpack.get(JOBS, id).ok_or_else(|| no_job(id))?;
    Ok(roll_state(j).get("state").map(Value::to_es_string).unwrap_or_else(|| "closed".into()))
}

pub async fn open_job(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    job_state(ctx, &id)?;
    set_job_state(ctx, &id, "opened").await?;
    Reply::ok(obj! { "opened" => true, "node" => ctx.cluster.node.id.clone() })
}

pub async fn close_job(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let jobs = expand(&st, JOBS, ctx.path_param("job_id"), allow, no_job)?;
    for (id, _) in jobs {
        set_job_state(ctx, &id, "closed").await?;
        let feeds: Vec<String> = st.metadata.xpack.iter(DATAFEEDS).filter(|(_, d)| d.get("job_id").and_then(Value::as_str) == Some(id.as_str())).map(|(k, _)| k.clone()).collect();
        for f in feeds {
            set_datafeed_state(ctx, &f, "stopped", None).await?;
        }
    }
    Reply::ok(obj! { "closed" => true })
}

fn require_open(ctx: &Ctx, id: &str) -> EsResult<()> {
    if job_state(ctx, id)? != "opened" {
        return Err(conflict(format!("Cannot perform requested action because job [{id}] is not open")));
    }
    Ok(())
}

pub async fn reset_job(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    if job_state(ctx, &id)? == "opened" {
        return Err(conflict(format!("Can only reset a job that is closed or failed. Job [{id}] is opened")));
    }
    delete_results(ctx, &id).await?;
    if ctx.query("wait_for_completion") == Some("false") {
        return Reply::ok(obj! { "task" => format!("{}:{}", ctx.cluster.node.id, ctx.now_ms % 100_000) });
    }
    Reply::acknowledged()
}

pub fn flush_job(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default();
    require_open(ctx, id)?;
    Reply::ok(obj! { "flushed" => true, "last_finalized_bucket_end" => 0 })
}

pub fn forecast(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default();
    require_open(ctx, id)?;
    Err(EsError::illegal_argument("Cannot run forecast: Forecast cannot be executed as job requires data to have been processed and modeled"))
}

pub fn delete_forecast(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default();
    let allow = ctx.bool_param("allow_no_forecasts", true)?;
    if !allow {
        let f = ctx.path_param("forecast_id").unwrap_or("_all");
        return Err(EsError::resource_not_found(format!("No forecast(s) [{f}] exists for job [{id}]")));
    }
    Reply::acknowledged()
}

pub async fn update_job(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    const UPDATABLE: &[&str] = &[
        "description", "groups", "model_plot_config", "analysis_limits", "renormalization_window_days", "background_persist_interval",
        "model_snapshot_retention_days", "daily_model_snapshot_retention_after_days", "results_retention_days", "custom_settings",
        "allow_lazy_open", "detectors", "per_partition_categorization", "model_prune_window",
    ];
    for k in body.keys() {
        if !UPDATABLE.contains(&k.as_str()) {
            return Err(crate::xpack::unknown_field(ctx, "job_update", k));
        }
    }
    let now = ctx.now_ms;
    let out = ctx
        .cluster
        .update(move |st| {
            let Some(mut j) = st.metadata.xpack.get(JOBS, &id).cloned() else { return Err(no_job(&id)) };
            if let Value::Obj(m) = &mut j {
                for (k, v) in &body {
                    match k.as_str() {
                        "analysis_limits" => {
                            if let Some(l) = v.get("model_memory_limit") {
                                let mm = memory_limit(Some(l))?;
                                if let Some(Value::Obj(al)) = m.get_mut("analysis_limits") {
                                    al.insert("model_memory_limit".into(), mm.into());
                                }
                            }
                        }
                        "model_prune_window" => {
                            if let Some(Value::Obj(ac)) = m.get_mut("analysis_config") {
                                ac.insert(k.clone(), v.clone());
                            }
                        }
                        "detectors" => {}
                        _ => {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            let j = job_order(j);
            store::put(st, JOBS, &id, j.clone(), now)?;
            Ok(public(&j))
        })
        .await?;
    Reply::ok(out)
}

/// `POST _ml/anomaly_detectors/_validate/detector`: the detector parses and builds.
pub fn validate_detector(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    parse_detector(ctx.raw_json(), &[], &body, 0).map_err(|(e, _)| e)?;
    Reply::acknowledged()
}

/// `POST _ml/anomaly_detectors/_validate`: the job config parses and builds.
pub fn validate_job(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !JOB_FIELDS.contains(&k.as_str()) {
            return Err(crate::xpack::unknown_field(ctx, "job_details", k));
        }
    }
    if let Some(id) = body.get("job_id").map(Value::to_es_string)
        && !valid_id(&id)
    {
        return Err(invalid_id("job_id", &id));
    }
    let id = body.get("job_id").map(Value::to_es_string).unwrap_or_default();
    build_job(ctx.raw_json(), &id, &body, ctx.now_ms)?;
    Reply::acknowledged()
}

pub fn estimate_model_memory(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let ac = body.get("analysis_config").cloned().unwrap_or_else(Value::object);
    let card = |k: &str| -> std::collections::HashMap<String, i64> {
        body.get(k).and_then(Value::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).collect()).unwrap_or_default()
    };
    let overall = card("overall_cardinality");
    let max_bucket = card("max_bucket_cardinality");
    let bytes = estimate(&ac, &overall, &max_bucket)?;
    let mb = bytes.saturating_add((1 << 20) - 1) >> 20;
    Reply::ok(obj! { "model_memory_estimate" => format!("{mb}mb") })
}

/// Elasticsearch's model memory estimate (`TransportEstimateModelMemoryAction`).
fn estimate(ac: &Value, overall: &std::collections::HashMap<String, i64>, max_bucket: &std::collections::HashMap<String, i64>) -> EsResult<i64> {
    let span = TimeValue::parse(&ac.get("bucket_span").map(Value::to_es_string).unwrap_or_else(|| "5m".into()), "bucket_span")?.as_millis() / 1000;
    let span = span.max(1);
    let est = |desc: &str, f: &str, m: &std::collections::HashMap<String, i64>, is_overall: bool| -> EsResult<i64> {
        if let Some(v) = m.get(f) {
            return Ok(*v);
        }
        if f == "mlcategory" {
            return Ok(if is_overall { 500 } else { 50 });
        }
        Err(EsError::illegal_argument(format!(
            "[{}] cardinality estimate required for [{desc}] [{f}] but not supplied",
            if is_overall { "Overall" } else { "Bucket max" }
        )))
    };
    let reduced = |c: i64, p: i64| -> f64 {
        if c == 0 {
            return 0.0;
        }
        let power = (((span as f64).log10() + 1.0) / 8.0).min(1.0);
        c as f64 / (c.min(p) as f64).powf(power)
    };
    let mut total: i64 = 10 << 20;
    let mut analysis_fields = std::collections::HashSet::new();
    for d in ac.get("detectors").and_then(Value::as_array).into_iter().flatten() {
        let f = d.get("function").and_then(Value::as_str).unwrap_or("count");
        let (mut ans, workspace): (i64, bool) = match f {
            "distinct_count" | "low_distinct_count" | "high_distinct_count" => (32 << 10, true),
            "count" | "low_count" | "high_count" | "non_zero_count" | "low_non_zero_count" | "high_non_zero_count" => (32 << 10, false),
            "rare" | "freq_rare" => (2 << 10, false),
            "info_content" | "low_info_content" | "high_info_content" => (48 << 10, true),
            "metric" => (120 << 10, false),
            "median" | "low_median" | "high_median" | "lat_long" => (64 << 10, false),
            "time_of_day" | "time_of_week" => (10 << 10, false),
            _ => (48 << 10, false),
        };
        let get = |k: &str| d.get(k).and_then(Value::as_str).map(str::to_string);
        for k in ["field_name", "by_field_name", "over_field_name", "partition_field_name"] {
            if let Some(v) = get(k) {
                analysis_fields.insert(v);
            }
        }
        let mut part = 1i64;
        if let Some(p) = get("partition_field_name") {
            part = est("partition_field_name", &p, overall, true)?.max(1);
        }
        if let Some(b) = get("by_field_name") {
            let c = est("by_field_name", &b, overall, true)?;
            let mult = (reduced(c, part) * 2.0 / 3.0).ceil() as i64;
            ans = ans.saturating_mul(mult);
        }
        if let Some(o) = get("over_field_name") {
            let c = est("over_field_name", &o, overall, true)?;
            let mult = reduced(c, part).ceil() as i64;
            ans = ans.saturating_add(768i64.saturating_mul(mult));
        }
        if get("partition_field_name").is_some() {
            ans = ans.saturating_mul(part);
        }
        if workspace {
            ans = ans.saturating_add(5 << 20);
        }
        total = total.saturating_add(ans);
    }
    let mut inf_card = 0i64;
    for i in ac.get("influencers").and_then(Value::as_array).into_iter().flatten() {
        let i = i.to_es_string();
        if !analysis_fields.contains(&i) {
            inf_card = inf_card.saturating_add(est("influencers", &i, max_bucket, false)?);
        }
    }
    total = total.saturating_add((10i64 << 10).saturating_mul(inf_card));
    if ac.get("categorization_field_name").is_some() {
        let ppc = ac.get("per_partition_categorization");
        let enabled = ppc.and_then(|p| p.get("enabled")).and_then(Value::as_bool).unwrap_or(false);
        let mut per = 20i64;
        let mut part = 1i64;
        if enabled {
            for d in ac.get("detectors").and_then(Value::as_array).into_iter().flatten() {
                if let Some(p) = d.get("partition_field_name").and_then(Value::as_str) {
                    part = est("partition_field_name", p, overall, true)?.max(1);
                    break;
                }
            }
            if !ppc.and_then(|p| p.get("stop_on_warn")).and_then(Value::as_bool).unwrap_or(false) {
                per *= 2;
            }
        } else {
            per *= 2;
        }
        total = total.saturating_add((per * part) << 20);
    }
    Ok(total)
}

pub fn delete_expired_data(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(obj! { "deleted" => true })
}

// --- results -----------------------------------------------------------------------------------

fn millis(v: &Value) -> Value {
    match v {
        Value::Str(s) => crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis")
            .ok()
            .and_then(|f| f.parse_millis(s).ok())
            .map(Value::from)
            .unwrap_or_else(|| v.clone()),
        other => other.clone(),
    }
}

/// Searches a job's results: `(total, sources)`.
async fn search_results(ctx: &Ctx<'_>, job: &str, filter: Vec<Value>, sort: Value, from: i64, size: i64) -> EsResult<(i64, Vec<Value>)> {
    let st = ctx.state();
    if !st.metadata.indices.contains_key(RESULTS_INDEX) && !st.metadata.indices.keys().any(|k| k.starts_with(".ml-anomalies-")) {
        return Ok((0, Vec::new()));
    }
    let mut filters = vec![obj! { "term" => obj! { "job_id" => job } }];
    filters.extend(filter);
    let body = obj! {
        "query" => obj! { "bool" => obj! { "filter" => Value::Arr(filters) } },
        "sort" => sort,
        "from" => from,
        "size" => size,
        "track_total_hits" => true,
    };
    let r = internal::call(ctx.cluster, "POST", "/.ml-anomalies-*/_search?ignore_unavailable=true&allow_no_indices=true&expand_wildcards=all", Some(&body)).await?;
    let total = r.pointer(&["hits", "total", "value"]).and_then(Value::as_i64).unwrap_or(0);
    let hits = r.pointer(&["hits", "hits"]).and_then(Value::as_array).map(|a| a.iter().filter_map(|h| h.get("_source").cloned()).collect()).unwrap_or_default();
    Ok((total, hits))
}

fn page(ctx: &Ctx, body: &Value) -> (i64, i64) {
    let p = body.get("page");
    let from = p.and_then(|p| p.get("from")).and_then(Value::as_i64).or_else(|| ctx.query("from").and_then(|v| v.parse().ok())).unwrap_or(0);
    let size = p.and_then(|p| p.get("size")).and_then(Value::as_i64).or_else(|| ctx.query("size").and_then(|v| v.parse().ok())).unwrap_or(100);
    (from, size)
}

fn param<'a>(ctx: &'a Ctx, body: &'a Value, k: &str) -> Option<String> {
    body.get(k).map(Value::to_es_string).or_else(|| ctx.query(k).map(str::to_string))
}

fn time_range(ctx: &Ctx, body: &Value, field: &str) -> Vec<Value> {
    let mut r = Map::new();
    if let Some(s) = param(ctx, body, "start") {
        r.insert("gte".into(), s.into());
    }
    if let Some(e) = param(ctx, body, "end") {
        r.insert("lt".into(), e.into());
    }
    if r.is_empty() { Vec::new() } else { vec![obj! { "range" => obj! { field => Value::Obj(r) } }] }
}

fn with_defaults(src: Value, defaults: &[(&str, Value)], order: &[&str]) -> Value {
    let Value::Obj(m) = src else { return src };
    let mut out = Map::new();
    for k in order {
        if let Some(v) = m.get(*k) {
            out.insert((*k).into(), if *k == "timestamp" { millis(v) } else { v.clone() });
        } else if let Some((_, d)) = defaults.iter().find(|(n, _)| n == k) {
            out.insert((*k).into(), d.clone());
        }
    }
    for (k, v) in m {
        if !out.contains_key(&k) {
            out.insert(k.clone(), if k == "timestamp" { millis(&v) } else { v });
        }
    }
    Value::Obj(out)
}

pub async fn get_buckets(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    job_state(ctx, &id)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let (from, size) = page(ctx, &body);
    let mut filters = vec![obj! { "term" => obj! { "result_type" => "bucket" } }];
    filters.extend(time_range(ctx, &body, "timestamp"));
    if let Some(ts) = ctx.path_param("timestamp") {
        filters.push(obj! { "term" => obj! { "timestamp" => ts } });
    }
    if param(ctx, &body, "exclude_interim").as_deref() == Some("true") {
        filters.push(obj! { "bool" => obj! { "must_not" => obj! { "term" => obj! { "is_interim" => true } } } });
    }
    if let Some(s) = param(ctx, &body, "anomaly_score") {
        filters.push(obj! { "range" => obj! { "anomaly_score" => obj! { "gte" => s } } });
    }
    let sort_field = param(ctx, &body, "sort").unwrap_or_else(|| "timestamp".into());
    let desc = param(ctx, &body, "desc").as_deref() == Some("true");
    let (total, hits) = search_results(ctx, &id, filters, obj! { sort_field => if desc { "desc" } else { "asc" } }, from, size).await?;
    let defaults = [
        ("initial_anomaly_score", Value::from(0.0)),
        ("event_count", Value::from(0)),
        ("is_interim", Value::Bool(false)),
        ("bucket_influencers", Value::Arr(Vec::new())),
        ("processing_time_ms", Value::from(0)),
        ("result_type", Value::from("bucket")),
    ];
    let order = ["job_id", "timestamp", "anomaly_score", "bucket_span", "initial_anomaly_score", "event_count", "is_interim", "bucket_influencers", "processing_time_ms", "result_type"];
    let buckets: Vec<Value> = hits.into_iter().map(|h| with_defaults(h, &defaults, &order)).collect();
    Reply::ok(obj! { "count" => total, "buckets" => Value::Arr(buckets) })
}

pub async fn get_overall_buckets(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let jobs = expand(&st, JOBS, ctx.path_param("job_id"), true, no_job)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let top_n = param(ctx, &body, "top_n").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1);
    let mut by_ts: std::collections::BTreeMap<i64, Vec<(String, f64, bool)>> = Default::default();
    let mut span = 0i64;
    for (id, j) in &jobs {
        let s = TimeValue::parse(&j.pointer(&["analysis_config", "bucket_span"]).map(Value::to_es_string).unwrap_or_else(|| "5m".into()), "bucket_span")?.as_millis() / 1000;
        span = span.max(s);
        let mut filters = vec![obj! { "term" => obj! { "result_type" => "bucket" } }];
        filters.extend(time_range(ctx, &body, "timestamp"));
        let (_, hits) = search_results(ctx, id, filters, obj! { "timestamp" => "asc" }, 0, 10_000).await?;
        for h in hits {
            let ts = millis(h.get("timestamp").unwrap_or(&Value::Null)).as_i64().unwrap_or(0);
            let score = h.get("anomaly_score").map(|v| v.as_f64().unwrap_or(0.0)).unwrap_or(0.0);
            let interim = h.get("is_interim").and_then(Value::as_bool).unwrap_or(false);
            by_ts.entry(ts).or_default().push((id.clone(), score, interim));
        }
    }
    let min_score = param(ctx, &body, "overall_score").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let mut out = Vec::new();
    for (ts, mut js) in by_ts {
        js.sort_by(|a, b| b.1.total_cmp(&a.1));
        let top: Vec<&(String, f64, bool)> = js.iter().take(top_n).collect();
        let overall = top.iter().map(|j| j.1).sum::<f64>() / top.len() as f64;
        if overall < min_score {
            continue;
        }
        let mut jl: Vec<&(String, f64, bool)> = js.iter().collect();
        jl.sort_by(|a, b| a.0.cmp(&b.0));
        out.push(obj! {
            "timestamp" => ts,
            "bucket_span" => span,
            "overall_score" => overall,
            "jobs" => Value::Arr(jl.iter().map(|j| obj! { "job_id" => j.0.clone(), "max_anomaly_score" => j.1 }).collect()),
            "is_interim" => js.iter().any(|j| j.2),
            "result_type" => "overall_bucket",
        });
    }
    Reply::ok(obj! { "count" => out.len(), "overall_buckets" => Value::Arr(out) })
}

pub async fn get_records(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    job_state(ctx, &id)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let (from, size) = page(ctx, &body);
    let mut filters = vec![obj! { "term" => obj! { "result_type" => "record" } }];
    filters.extend(time_range(ctx, &body, "timestamp"));
    if let Some(s) = param(ctx, &body, "record_score") {
        filters.push(obj! { "range" => obj! { "record_score" => obj! { "gte" => s } } });
    }
    let sort_field = param(ctx, &body, "sort").unwrap_or_else(|| "record_score".into());
    let desc = param(ctx, &body, "desc").as_deref() != Some("false");
    let (total, hits) = search_results(ctx, &id, filters, obj! { sort_field => if desc { "desc" } else { "asc" } }, from, size).await?;
    let defaults = [
        ("probability", Value::from(0.0)),
        ("initial_record_score", Value::from(0.0)),
        ("detector_index", Value::from(0)),
        ("is_interim", Value::Bool(false)),
    ];
    let order = ["job_id", "result_type", "probability", "record_score", "initial_record_score", "bucket_span", "detector_index", "is_interim", "timestamp"];
    Reply::ok(obj! { "count" => total, "records" => Value::Arr(hits.into_iter().map(|h| with_defaults(h, &defaults, &order)).collect()) })
}

pub async fn get_influencers(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    job_state(ctx, &id)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let (from, size) = page(ctx, &body);
    let mut filters = vec![obj! { "term" => obj! { "result_type" => "influencer" } }];
    filters.extend(time_range(ctx, &body, "timestamp"));
    let sort_field = param(ctx, &body, "sort").unwrap_or_else(|| "influencer_score".into());
    let desc = param(ctx, &body, "desc").as_deref() != Some("false");
    let (total, hits) = search_results(ctx, &id, filters, obj! { sort_field => if desc { "desc" } else { "asc" } }, from, size).await?;
    let defaults = [("probability", Value::from(0.0)), ("initial_influencer_score", Value::from(0.0)), ("is_interim", Value::Bool(false))];
    let order = ["job_id", "result_type", "influencer_field_name", "influencer_field_value", "influencer_score", "initial_influencer_score", "probability", "bucket_span", "is_interim", "timestamp"];
    Reply::ok(obj! { "count" => total, "influencers" => Value::Arr(hits.into_iter().map(|h| with_defaults(h, &defaults, &order)).collect()) })
}

pub async fn get_categories(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    job_state(ctx, &id)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let (from, size) = page(ctx, &body);
    let mut filters = vec![obj! { "exists" => obj! { "field" => "category_id" } }];
    if let Some(c) = ctx.path_param("category_id") {
        filters.push(obj! { "term" => obj! { "category_id" => c } });
    }
    if let Some(p) = param(ctx, &body, "partition_field_value") {
        filters.push(obj! { "term" => obj! { "partition_field_value" => p } });
    }
    let (total, hits) = search_results(ctx, &id, filters, obj! { "category_id" => "asc" }, from, size).await?;
    Reply::ok(obj! { "count" => total, "categories" => Value::Arr(hits) })
}

fn snapshot_view(h: Value) -> Value {
    let defaults = [("min_version", Value::from("6.3.0")), ("retain", Value::Bool(false))];
    let order = ["job_id", "min_version", "timestamp", "description", "snapshot_id", "snapshot_doc_count", "model_size_stats", "latest_record_time_stamp", "latest_result_time_stamp", "quantiles", "retain"];
    let mut v = with_defaults(h, &defaults, &order);
    if let Value::Obj(m) = &mut v {
        for k in ["latest_record_timestamp", "latest_result_timestamp"] {
            if let Some(x) = m.get(k).cloned() {
                m.insert(k.into(), millis(&x));
            }
        }
        // `retain` prints last.
        if let Some(r) = m.shift_remove("retain") {
            m.insert("retain".into(), r);
        }
    }
    v
}

async fn snapshots(ctx: &Ctx<'_>, job: &str, snapshot: Option<&str>, from: i64, size: i64, sort: &str, desc: bool) -> EsResult<(i64, Vec<Value>)> {
    let mut filters = vec![obj! { "exists" => obj! { "field" => "snapshot_id" } }];
    if let Some(s) = snapshot.filter(|s| *s != "_all" && *s != "*") {
        filters.push(obj! { "term" => obj! { "snapshot_id" => s } });
    }
    search_results(ctx, job, filters, obj! { sort => if desc { "desc" } else { "asc" } }, from, size).await
}

pub async fn get_model_snapshots(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("job_id").unwrap_or_default().to_string();
    job_state(ctx, &id)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let (from, size) = page(ctx, &body);
    let sort = param(ctx, &body, "sort").unwrap_or_else(|| "timestamp".into());
    let desc = param(ctx, &body, "desc").as_deref() != Some("false");
    let (total, hits) = snapshots(ctx, &id, ctx.path_param("snapshot_id"), from, size, &sort, desc).await?;
    Reply::ok(obj! { "count" => total, "model_snapshots" => Value::Arr(hits.into_iter().map(snapshot_view).collect()) })
}

fn no_snapshot(job: &str, snap: &str) -> EsError {
    EsError::resource_not_found(format!("No model snapshot with id [{snap}] exists for job [{job}]"))
}

async fn find_snapshot(ctx: &Ctx<'_>, job: &str, snap: &str) -> EsResult<Value> {
    let (_, hits) = snapshots(ctx, job, Some(snap), 0, 1, "timestamp", true).await?;
    hits.into_iter().next().ok_or_else(|| no_snapshot(job, snap))
}

pub async fn delete_model_snapshot(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let job = ctx.path_param("job_id").unwrap_or_default().to_string();
    let snap = ctx.path_param("snapshot_id").unwrap_or_default().to_string();
    find_snapshot(ctx, &job, &snap).await?;
    let q = obj! { "query" => obj! { "bool" => obj! { "filter" => crate::arr![obj! { "term" => obj! { "job_id" => job.clone() } }, obj! { "term" => obj! { "snapshot_id" => snap.clone() } }] } } };
    internal::call(ctx.cluster, "POST", "/.ml-anomalies-*/_delete_by_query?refresh=true&conflicts=proceed&expand_wildcards=all", Some(&q)).await?;
    Reply::acknowledged()
}

pub async fn update_model_snapshot(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let job = ctx.path_param("job_id").unwrap_or_default().to_string();
    let snap = ctx.path_param("snapshot_id").unwrap_or_default().to_string();
    let mut s = find_snapshot(ctx, &job, &snap).await.map_err(|_| no_snapshot(&job, &snap))?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    if let Value::Obj(m) = &mut s {
        for k in ["description", "retain"] {
            if let Some(v) = body.get(k) {
                m.insert(k.into(), v.clone());
            }
        }
    }
    let q = obj! {
        "query" => obj! { "bool" => obj! { "filter" => crate::arr![obj! { "term" => obj! { "job_id" => job.clone() } }, obj! { "term" => obj! { "snapshot_id" => snap.clone() } }] } },
        "script" => obj! { "source" => "for (e in params.u.entrySet()) { ctx._source[e.getKey()] = e.getValue(); }", "params" => obj! { "u" => body.clone() } },
    };
    internal::call(ctx.cluster, "POST", "/.ml-anomalies-*/_update_by_query?refresh=true&conflicts=proceed&expand_wildcards=all", Some(&q)).await?;
    Reply::ok(obj! { "acknowledged" => true, "model" => snapshot_view(s) })
}

pub async fn revert_model_snapshot(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let job = ctx.path_param("job_id").unwrap_or_default().to_string();
    let snap = ctx.path_param("snapshot_id").unwrap_or_default().to_string();
    job_state(ctx, &job)?;
    let s = find_snapshot(ctx, &job, &snap).await?;
    Reply::ok(obj! { "model" => snapshot_view(s) })
}

pub async fn upgrade_job_snapshot(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let job = ctx.path_param("job_id").unwrap_or_default().to_string();
    let snap = ctx.path_param("snapshot_id").unwrap_or_default().to_string();
    job_state(ctx, &job)?;
    find_snapshot(ctx, &job, &snap).await?;
    // roll has no native process to load the old model state into: the upgrade is recorded as
    // attempted and failed, which is what the stats report.
    let now = ctx.now_ms;
    let key = format!("{job}/{snap}");
    ctx.cluster.update(move |st| store::put(st, "ml.snapshot_upgrade", &key, obj! { "job_id" => job, "snapshot_id" => snap, "state" => "failed" }, now).map(|_| ())).await?;
    if ctx.bool_param("wait_for_completion", false)? {
        return Err(EsError::new(500, "exception", "Snapshot upgrade failed: native model state is not available"));
    }
    Reply::ok(obj! { "node" => ctx.cluster.node.id.clone(), "completed" => false })
}

pub fn upgrade_stats(ctx: &Ctx) -> EsResult<Reply> {
    let job = ctx.path_param("job_id").unwrap_or_default();
    let snap = ctx.path_param("snapshot_id").unwrap_or("_all");
    let st = ctx.state();
    let mut out = Vec::new();
    for (_, u) in st.metadata.xpack.iter("ml.snapshot_upgrade") {
        let j = u.get("job_id").and_then(Value::as_str).unwrap_or_default();
        let s = u.get("snapshot_id").and_then(Value::as_str).unwrap_or_default();
        if !crate::xcontent::filter::glob_match(job, j) || (snap != "_all" && !crate::xcontent::filter::glob_match(snap, s)) {
            continue;
        }
        out.push(obj! { "job_id" => j, "snapshot_id" => s, "state" => u.get("state").cloned().unwrap_or(Value::from("failed")), "node" => node_value(ctx, true), "assignment_explanation" => "" });
    }
    Reply::ok(obj! { "count" => out.len(), "model_snapshot_upgrades" => Value::Arr(out) })
}

// --- datafeeds ---------------------------------------------------------------------------------

fn build_datafeed(id: &str, body: &Map, roles: &[String]) -> EsResult<Value> {
    if !valid_id(id) {
        return Err(invalid_id("datafeed_id", id));
    }
    let Some(job) = body.get("job_id").map(Value::to_es_string) else {
        return Err(EsError::illegal_argument("[job_id] must not be null."));
    };
    let indices = match body.get("indices").or_else(|| body.get("indexes")) {
        Some(Value::Arr(a)) => Value::Arr(a.clone()),
        Some(Value::Str(s)) => Value::Arr(s.split(',').map(|p| Value::from(p.trim())).collect()),
        _ => return Err(EsError::illegal_argument("[indices] must not be null.")),
    };
    let mut out = Map::new();
    out.insert("datafeed_id".into(), id.into());
    out.insert("job_id".into(), job.into());
    out.insert("authorization".into(), obj! { "roles" => Value::Arr(roles.iter().map(Value::from).collect()) });
    // Elasticsearch picks a random delay between 60 and 120 seconds.
    let delay = 60_000 + (rand::random::<u32>() % 60_000) as i64;
    out.insert("query_delay".into(), body.get("query_delay").cloned().unwrap_or_else(|| Value::from(format!("{delay}ms"))));
    out.insert("chunking_config".into(), body.get("chunking_config").cloned().unwrap_or_else(|| obj! { "mode" => "auto" }));
    out.insert(
        "indices_options".into(),
        body.get("indices_options").cloned().unwrap_or_else(|| obj! { "ignore_unavailable" => false, "expand_wildcards" => crate::arr!["open"], "allow_no_indices" => true, "ignore_throttled" => true }),
    );
    out.insert("query".into(), body.get("query").cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} }));
    if let Some(x) = body.get("frequency") {
        out.insert("frequency".into(), x.clone());
    }
    out.insert("indices".into(), indices);
    for k in ["aggregations", "aggs", "script_fields", "runtime_mappings"] {
        if let Some(x) = body.get(k) {
            out.insert(if k == "aggs" { "aggregations".into() } else { k.into() }, x.clone());
        }
    }
    out.insert("scroll_size".into(), body.get("scroll_size").cloned().unwrap_or(Value::from(1000)));
    out.insert("delayed_data_check_config".into(), body.get("delayed_data_check_config").cloned().unwrap_or_else(|| obj! { "enabled" => true }));
    if let Some(m) = body.get("max_empty_searches")
        && m.as_i64() != Some(-1)
    {
        out.insert("max_empty_searches".into(), m.clone());
    }
    Ok(Value::Obj(out))
}

pub async fn put_datafeed(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("datafeed_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let roles = crate::xpack::security::principal(ctx).roles.clone();
    let df = build_datafeed(&id, &body, &roles)?;
    let job = df.get("job_id").map(Value::to_es_string).unwrap_or_default();
    let now = ctx.now_ms;
    let out = df.clone();
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(JOBS, &job).is_none() {
                return Err(no_job(&job));
            }
            if st.metadata.xpack.get(DATAFEEDS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("A datafeed with id [{id}] already exists")));
            }
            if let Some((other, _)) = st.metadata.xpack.iter(DATAFEEDS).find(|(_, d)| d.get("job_id").and_then(Value::as_str) == Some(job.as_str())) {
                return Err(EsError::resource_already_exists(format!("A datafeed [{other}] already exists for job [{job}]")));
            }
            store::put(st, DATAFEEDS, &id, df, now).map(|_| ())
        })
        .await?;
    Reply::ok(out)
}

pub async fn update_datafeed(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("datafeed_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let now = ctx.now_ms;
    let out = ctx
        .cluster
        .update(move |st| {
            let Some(cur) = st.metadata.xpack.get(DATAFEEDS, &id).cloned() else { return Err(no_datafeed(&id)) };
            let mut merged = cur.as_object().cloned().unwrap_or_default();
            for (k, v) in &body {
                let key = if k == "indexes" { "indices" } else { k.as_str() };
                merged.insert(key.to_string(), v.clone());
            }
            let roles: Vec<String> = cur.pointer(&["authorization", "roles"]).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
            let mut df = build_datafeed(&id, &merged, &roles)?;
            if let (Value::Obj(m), Some(r)) = (&mut df, cur.get("_roll")) {
                m.insert("_roll".into(), r.clone());
            }
            store::put(st, DATAFEEDS, &id, df.clone(), now)?;
            Ok(public(&df))
        })
        .await?;
    Reply::ok(out)
}

pub fn get_datafeeds(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let feeds = expand(&st, DATAFEEDS, ctx.path_param("datafeed_id"), allow, no_datafeed)?;
    let list: Vec<Value> = feeds.iter().map(|(_, d)| public(d)).collect();
    Reply::ok(obj! { "count" => list.len(), "datafeeds" => Value::Arr(list) })
}

pub fn datafeed_stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let feeds = expand(&st, DATAFEEDS, ctx.path_param("datafeed_id"), allow, no_datafeed)?;
    let mut out = Vec::new();
    for (id, d) in feeds {
        let rs = roll_state(&d);
        let started = rs.get("state").and_then(Value::as_str) == Some("started");
        let job = d.get("job_id").map(Value::to_es_string).unwrap_or_default();
        let mut m = Map::new();
        m.insert("datafeed_id".into(), id.into());
        m.insert("state".into(), (if started { "started" } else { "stopped" }).into());
        if started {
            m.insert("node".into(), node_value(ctx, true));
            m.insert("assignment_explanation".into(), "".into());
        }
        m.insert("timing_stats".into(), obj! { "job_id" => job, "search_count" => 0, "bucket_count" => 0, "total_search_time_ms" => 0.0, "exponential_average_search_time_per_hour_ms" => 0.0 });
        if started {
            let start = rs.get("start_ms").and_then(Value::as_i64).unwrap_or(0);
            m.insert(
                "running_state".into(),
                obj! { "real_time_configured" => rs.get("end_ms").is_none(), "real_time_running" => false, "search_interval" => obj! { "start_ms" => start, "end_ms" => ctx.now_ms } },
            );
        }
        out.push(Value::Obj(m));
    }
    Reply::ok(obj! { "count" => out.len(), "datafeeds" => Value::Arr(out) })
}

async fn set_datafeed_state(ctx: &Ctx<'_>, id: &str, state: &'static str, start: Option<(i64, Option<i64>)>) -> EsResult<()> {
    let id = id.to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(d) = st.metadata.xpack.get(DATAFEEDS, &id).cloned() else { return Err(no_datafeed(&id)) };
            let d = with_roll_state(d, |r| {
                r.insert("state".into(), state.into());
                if let Some((s, e)) = start {
                    r.insert("start_ms".into(), s.into());
                    match e {
                        Some(e) => r.insert("end_ms".into(), e.into()),
                        None => r.shift_remove("end_ms"),
                    };
                }
            });
            store::put(st, DATAFEEDS, &id, d, now).map(|_| ())
        })
        .await
}

fn time_param(ctx: &Ctx, body: &Value, k: &str) -> EsResult<Option<i64>> {
    let Some(v) = param(ctx, body, k) else { return Ok(None) };
    let f = crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis").map_err(EsError::illegal_argument)?;
    f.parse_millis(&v).map(Some).map_err(|_| EsError::illegal_argument(format!("Query param [{k}] with value [{v}] cannot be parsed as a date or converted to a number (epoch).")))
}

pub async fn start_datafeed(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("datafeed_id").unwrap_or_default().to_string();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let st = ctx.state();
    let Some(d) = st.metadata.xpack.get(DATAFEEDS, &id) else { return Err(no_datafeed(&id)) };
    let job = d.get("job_id").map(Value::to_es_string).unwrap_or_default();
    if job_state(ctx, &job)? != "opened" {
        return Err(conflict(format!("cannot start datafeed [{id}] because job [{job}] is closed")));
    }
    if roll_state(d).get("state").and_then(Value::as_str) == Some("started") {
        return Err(conflict(format!("cannot start datafeed [{id}] because it has already been started")));
    }
    let start = time_param(ctx, &body, "start")?.unwrap_or(0);
    let end = time_param(ctx, &body, "end")?;
    set_datafeed_state(ctx, &id, "started", Some((start, end))).await?;
    Reply::ok(obj! { "started" => true, "node" => ctx.cluster.node.id.clone() })
}

pub async fn stop_datafeed(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let feeds = expand(&st, DATAFEEDS, ctx.path_param("datafeed_id"), allow, no_datafeed)?;
    for (id, _) in feeds {
        set_datafeed_state(ctx, &id, "stopped", None).await?;
    }
    Reply::ok(obj! { "stopped" => true })
}

pub async fn delete_datafeed(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("datafeed_id").unwrap_or_default().to_string();
    let force = ctx.bool_param("force", false)?;
    let st = ctx.state();
    let Some(d) = st.metadata.xpack.get(DATAFEEDS, &id) else { return Err(no_datafeed(&id)) };
    if !force && roll_state(d).get("state").and_then(Value::as_str) == Some("started") {
        return Err(conflict(format!("Cannot delete datafeed [{id}] while its status is started")));
    }
    ctx.cluster.update(move |st| Ok(store::remove(st, DATAFEEDS, &id))).await?;
    Reply::acknowledged()
}

/// `GET _ml/datafeeds/<id>/_preview`: the documents the datafeed would send to its job, as the
/// job's analysis fields of the matching documents sorted by time.
pub async fn preview_datafeed(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let st = ctx.state();
    let (feed, job) = match ctx.path_param("datafeed_id") {
        Some(id) => {
            let d = st.metadata.xpack.get(DATAFEEDS, id).cloned().ok_or_else(|| no_datafeed(id))?;
            let jid = d.get("job_id").map(Value::to_es_string).unwrap_or_default();
            let j = st.metadata.xpack.get(JOBS, &jid).cloned().ok_or_else(|| no_job(&jid))?;
            (d, j)
        }
        None => {
            let d = body.get("datafeed_config").cloned().unwrap_or_else(Value::object);
            let j = match body.get("job_config") {
                Some(jc) => build_job(None, "preview", jc.as_object().unwrap_or(&Map::new()), ctx.now_ms)?,
                None => {
                    let jid = d.get("job_id").map(Value::to_es_string).unwrap_or_default();
                    st.metadata.xpack.get(JOBS, &jid).cloned().ok_or_else(|| no_job(&jid))?
                }
            };
            (d, j)
        }
    };
    let time_field = job.pointer(&["data_description", "time_field"]).map(Value::to_es_string).unwrap_or_else(|| "time".into());
    let mut fields = vec![time_field.clone()];
    for d in job.pointer(&["analysis_config", "detectors"]).and_then(Value::as_array).into_iter().flatten() {
        for k in ["field_name", "by_field_name", "over_field_name", "partition_field_name"] {
            if let Some(f) = d.get(k).and_then(Value::as_str)
                && !fields.iter().any(|x| x == f)
            {
                fields.push(f.to_string());
            }
        }
    }
    for i in job.pointer(&["analysis_config", "influencers"]).and_then(Value::as_array).into_iter().flatten() {
        let f = i.to_es_string();
        if !fields.contains(&f) {
            fields.push(f);
        }
    }
    if let Some(s) = job.pointer(&["analysis_config", "summary_count_field_name"]).and_then(Value::as_str) {
        fields.retain(|f| f != s);
    }
    let indices: Vec<String> = feed.get("indices").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    let mut range = Map::new();
    if let Some(s) = time_param(ctx, &body, "start")? {
        range.insert("gte".into(), s.into());
    }
    if let Some(e) = time_param(ctx, &body, "end")? {
        range.insert("lt".into(), e.into());
    }
    let mut filter = vec![feed.get("query").cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} })];
    if !range.is_empty() {
        range.insert("format".into(), "epoch_millis".into());
        filter.push(obj! { "range" => obj! { time_field.clone() => Value::Obj(range) } });
    }
    let docvalue: Vec<Value> = fields
        .iter()
        .map(|f| if *f == time_field { obj! { "field" => f.clone(), "format" => "epoch_millis" } } else { Value::from(f.clone()) })
        .collect();
    let q = obj! {
        "query" => obj! { "bool" => obj! { "filter" => Value::Arr(filter) } },
        "sort" => crate::arr![obj! { time_field.clone() => "asc" }],
        "size" => feed.get("scroll_size").cloned().unwrap_or(Value::from(1000)),
        "_source" => false,
        "docvalue_fields" => Value::Arr(docvalue),
    };
    let uri = format!("/{}/_search?ignore_unavailable=true", indices.join(","));
    let r = internal::call(ctx.cluster, "POST", &uri, Some(&q)).await?;
    let mut out = Vec::new();
    for h in r.pointer(&["hits", "hits"]).and_then(Value::as_array).into_iter().flatten() {
        let mut m = Map::new();
        for f in &fields {
            let Some(Value::Arr(vals)) = h.pointer(&["fields", f.as_str()]) else { continue };
            let v = if vals.len() == 1 { vals[0].clone() } else { Value::Arr(vals.clone()) };
            let v = if *f == time_field { v.as_str().and_then(|s| s.parse::<i64>().ok()).map(Value::from).unwrap_or(v) } else { v };
            m.insert(f.clone(), v);
        }
        out.push(Value::Obj(m));
    }
    Reply::ok(Value::Arr(out))
}

// --- calendars and filters ---------------------------------------------------------------------

fn no_calendar(id: &str) -> EsError {
    EsError::resource_not_found(format!("No calendar with id [{id}]"))
}

pub async fn put_calendar(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("calendar_id").unwrap_or_default().to_string();
    if !valid_id(&id) {
        return Err(invalid_id("calendar_id", &id));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    if let Some(b) = body.get("calendar_id").map(Value::to_es_string)
        && b != id
    {
        return Err(EsError::illegal_argument(format!("Inconsistent calendar_id; '{b}' specified in the body differs from '{id}' specified as a URL argument")));
    }
    let mut c = Map::new();
    c.insert("calendar_id".into(), id.clone().into());
    c.insert("job_ids".into(), body.get("job_ids").cloned().unwrap_or_else(|| Value::Arr(Vec::new())));
    if let Some(d) = body.get("description") {
        c.insert("description".into(), d.clone());
    }
    let v = Value::Obj(c);
    let out = v.clone();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(CALENDARS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("A calendar with id [{id}] already exists")));
            }
            store::put(st, CALENDARS, &id, v, now).map(|_| ())
        })
        .await?;
    Reply::ok(out)
}

pub fn get_calendars(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let cals = expand(&st, CALENDARS, ctx.path_param("calendar_id"), true, no_calendar)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let (from, size) = page(ctx, &body);
    let count = cals.len();
    let list: Vec<Value> = cals.into_iter().skip(from.max(0) as usize).take(size.max(0) as usize).map(|(_, c)| c).collect();
    Reply::ok(obj! { "count" => count, "calendars" => Value::Arr(list) })
}

pub async fn delete_calendar(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("calendar_id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            if store::remove(st, CALENDARS, &id).is_none() {
                return Err(no_calendar(&id));
            }
            let events: Vec<String> = st.metadata.xpack.iter(EVENTS).filter(|(_, e)| e.get("calendar_id").and_then(Value::as_str) == Some(id.as_str())).map(|(k, _)| k.clone()).collect();
            for e in events {
                store::remove(st, EVENTS, &e);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn calendar_job(ctx: &Ctx<'_>, add: bool) -> EsResult<Reply> {
    let id = ctx.path_param("calendar_id").unwrap_or_default().to_string();
    let jobs: Vec<String> = ctx.path_param("job_id").unwrap_or_default().split(',').map(str::to_string).collect();
    let now = ctx.now_ms;
    let out = ctx
        .cluster
        .update(move |st| {
            let Some(mut c) = st.metadata.xpack.get(CALENDARS, &id).cloned() else { return Err(no_calendar(&id)) };
            if let Some(Value::Arr(ids)) = c.get_mut_job_ids() {
                for j in &jobs {
                    if add {
                        if !ids.iter().any(|x| x.as_str() == Some(j.as_str())) {
                            ids.push(Value::from(j.clone()));
                        }
                    } else {
                        if !ids.iter().any(|x| x.as_str() == Some(j.as_str())) {
                            return Err(EsError::resource_not_found(format!("Cannot remove [{j}] as it is not present in calendar [{id}]")));
                        }
                        ids.retain(|x| x.as_str() != Some(j.as_str()));
                    }
                }
            }
            store::put(st, CALENDARS, &id, c.clone(), now)?;
            Ok(c)
        })
        .await?;
    Reply::ok(out)
}

fn event_millis(v: Option<&Value>, what: &str) -> EsResult<i64> {
    match v {
        Some(Value::Num(n)) => Ok(n.as_f64() as i64),
        Some(Value::Str(s)) => crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis")
            .map_err(EsError::illegal_argument)?
            .parse_millis(s)
            .map_err(|_| EsError::illegal_argument(format!("Failed to parse [{what}] value [{s}]"))),
        _ => Err(EsError::illegal_argument(format!("[{what}] must not be null."))),
    }
}

pub async fn post_events(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("calendar_id").unwrap_or_default().to_string();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let mut events = Vec::new();
    for e in body.get("events").and_then(Value::as_array).into_iter().flatten() {
        if let Some(c) = e.get("calendar_id").and_then(Value::as_str)
            && c != id
        {
            return Err(EsError::illegal_argument(format!("Inconsistent calendar_id; '{c}' specified in the body differs from '{id}' specified as a URL argument")));
        }
        let start = event_millis(e.get("start_time"), "start_time")?;
        let end = event_millis(e.get("end_time"), "end_time")?;
        if start >= end {
            return Err(EsError::illegal_argument(format!(
                "Event start time [{}] must come before end time [{}]",
                crate::time::java_instant_string(start),
                crate::time::java_instant_string(end)
            )));
        }
        let mut m = Map::new();
        m.insert("description".into(), e.get("description").cloned().unwrap_or(Value::Null));
        m.insert("start_time".into(), start.into());
        m.insert("end_time".into(), end.into());
        m.insert("skip_result".into(), e.get("skip_result").cloned().unwrap_or(Value::Bool(true)));
        m.insert("skip_model_update".into(), e.get("skip_model_update").cloned().unwrap_or(Value::Bool(true)));
        if let Some(f) = e.get("force_time_shift") {
            m.insert("force_time_shift".into(), f.clone());
        }
        m.insert("calendar_id".into(), id.clone().into());
        events.push(Value::Obj(m));
    }
    let now = ctx.now_ms;
    let to_store = events.clone();
    let cid = id.clone();
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(CALENDARS, &cid).is_none() {
                return Err(no_calendar(&cid));
            }
            for (i, mut e) in to_store.into_iter().enumerate() {
                let eid = crate::meta::names::time_based_id();
                if let Value::Obj(m) = &mut e {
                    m.insert("event_id".into(), eid.clone().into());
                    m.insert("_roll".into(), obj! { "seq" => now * 1000 + i as i64 });
                }
                store::put(st, EVENTS, &eid, e, now)?;
            }
            Ok(())
        })
        .await?;
    Reply::ok(obj! { "events" => Value::Arr(events) })
}

pub fn get_events(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("calendar_id").unwrap_or("_all").to_string();
    let st = ctx.state();
    if id != "_all" && st.metadata.xpack.get(CALENDARS, &id).is_none() {
        return Err(no_calendar(&id));
    }
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let start = time_param(ctx, &body, "start")?;
    let end = time_param(ctx, &body, "end")?;
    let job = param(ctx, &body, "job_id");
    let job_cals: Option<Vec<String>> = job.map(|j| {
        st.metadata
            .xpack
            .iter(CALENDARS)
            .filter(|(_, c)| c.get("job_ids").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(j.as_str()))))
            .map(|(k, _)| k.clone())
            .collect()
    });
    let mut events: Vec<Value> = st
        .metadata
        .xpack
        .iter(EVENTS)
        .map(|(_, e)| e.clone())
        .filter(|e| {
            let c = e.get("calendar_id").and_then(Value::as_str).unwrap_or_default();
            (id == "_all" || c == id) && job_cals.as_ref().is_none_or(|cs| cs.iter().any(|x| x == c))
        })
        .filter(|e| {
            let s = e.get("start_time").and_then(Value::as_i64).unwrap_or(0);
            let en = e.get("end_time").and_then(Value::as_i64).unwrap_or(0);
            start.is_none_or(|st| en > st) && end.is_none_or(|e| s < e)
        })
        .collect();
    events.sort_by_key(|e| (e.get("start_time").and_then(Value::as_i64).unwrap_or(0), roll_state(e).get("seq").and_then(Value::as_i64).unwrap_or(0)));
    let (from, size) = page(ctx, &body);
    let count = events.len();
    let list: Vec<Value> = events.into_iter().skip(from.max(0) as usize).take(size.max(0) as usize).map(|e| public(&e)).collect();
    Reply::ok(obj! { "count" => count, "events" => Value::Arr(list) })
}

pub async fn delete_event(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let cid = ctx.path_param("calendar_id").unwrap_or_default().to_string();
    let eid = ctx.path_param("event_id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(CALENDARS, &cid).is_none() {
                return Err(no_calendar(&cid));
            }
            match st.metadata.xpack.get(EVENTS, &eid) {
                Some(e) if e.get("calendar_id").and_then(Value::as_str) == Some(cid.as_str()) => {
                    store::remove(st, EVENTS, &eid);
                    Ok(())
                }
                _ => Err(EsError::resource_not_found(format!("No event with id [{eid}]"))),
            }
        })
        .await?;
    Reply::acknowledged()
}

fn sorted_items(v: Option<&Value>) -> Vec<String> {
    let mut items: Vec<String> = v.and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    items.sort();
    items.dedup();
    items
}

fn filter_value(id: &str, description: Option<Value>, items: Vec<String>) -> Value {
    let mut m = Map::new();
    m.insert("filter_id".into(), id.into());
    if let Some(d) = description {
        m.insert("description".into(), d);
    }
    m.insert("items".into(), Value::Arr(items.into_iter().map(Value::from).collect()));
    Value::Obj(m)
}

pub async fn put_filter(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("filter_id").unwrap_or_default().to_string();
    if !valid_id(&id) {
        return Err(invalid_id("filter_id", &id));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    let f = filter_value(&id, body.get("description").cloned(), sorted_items(body.get("items")));
    let out = f.clone();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(FILTERS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("A filter with id [{id}] already exists")));
            }
            store::put(st, FILTERS, &id, f, now).map(|_| ())
        })
        .await?;
    Reply::ok(out)
}

pub fn get_filters(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let fs = expand(&st, FILTERS, ctx.path_param("filter_id"), false, |id| EsError::resource_not_found(format!("No filter with id [{id}] exists")))?;
    let count = fs.len();
    Reply::ok(obj! { "count" => count, "filters" => Value::Arr(fs.into_iter().map(|(_, f)| f).collect()) })
}

pub async fn update_filter(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("filter_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let now = ctx.now_ms;
    let out = ctx
        .cluster
        .update(move |st| {
            let Some(cur) = st.metadata.xpack.get(FILTERS, &id).cloned() else {
                return Err(EsError::resource_not_found(format!("No filter with id [{id}] exists")));
            };
            let mut items = sorted_items(cur.get("items"));
            let remove = sorted_items(body.get("remove_items"));
            for r in &remove {
                if !items.contains(r) {
                    return Err(EsError::illegal_argument(format!("Cannot remove item [{r}] as it is not present in filter [{id}]")));
                }
            }
            items.retain(|i| !remove.contains(i));
            items.extend(sorted_items(body.get("add_items")));
            items.sort();
            items.dedup();
            let desc = body.get("description").cloned().or_else(|| cur.get("description").cloned());
            let f = filter_value(&id, desc, items);
            store::put(st, FILTERS, &id, f.clone(), now)?;
            Ok(f)
        })
        .await?;
    Reply::ok(out)
}

pub async fn delete_filter(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("filter_id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            if store::remove(st, FILTERS, &id).is_none() {
                return Err(EsError::resource_not_found(format!("Could not delete filter with ID [{id}] because it does not exist")));
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_memory_estimate_matches_elasticsearch() {
        let ac = obj! { "bucket_span" => "15m", "detectors" => crate::arr![obj! { "function" => "max", "field_name" => "responsetime", "by_field_name" => "airline" }] };
        let overall = [("airline".to_string(), 50_000i64)].into_iter().collect();
        let bytes = estimate(&ac, &overall, &Default::default()).unwrap();
        assert_eq!((bytes + (1 << 20) - 1) >> 20, 1573);
        let e = estimate(&ac, &Default::default(), &Default::default()).unwrap_err();
        assert_eq!(e.reason(), "[Overall] cardinality estimate required for [by_field_name] [airline] but not supplied");
    }

    #[test]
    fn detector_descriptions() {
        let d = obj! { "function" => "metric", "field_name" => "responsetime", "by_field_name" => "airline" };
        assert_eq!(detector_description(d.as_object().unwrap()), "metric(responsetime) by airline");
        assert_eq!(detector_description(obj! { "function" => "count" }.as_object().unwrap()), "count");
    }

    #[test]
    fn jobs_get_elasticsearch_defaults() {
        let body = obj! {
            "analysis_config" => obj! { "detectors" => crate::arr![obj! { "function" => "count" }] },
            "data_description" => obj! { "format" => "xcontent" },
        };
        let j = build_job(None, "j", body.as_object().unwrap(), 5).unwrap();
        assert_eq!(
            j.to_json(),
            r#"{"job_id":"j","job_type":"anomaly_detector","job_version":"12.0.0","create_time":5,"analysis_config":{"bucket_span":"5m","detectors":[{"detector_description":"count","function":"count","detector_index":0}],"influencers":[],"model_prune_window":"30d"},"analysis_limits":{"model_memory_limit":"1024mb","categorization_examples_limit":4},"data_description":{"time_field":"time","time_format":"epoch_ms"},"model_snapshot_retention_days":10,"daily_model_snapshot_retention_after_days":1,"results_index_name":"shared","allow_lazy_open":false}"#
        );
        assert_eq!(memory_limit(Some(&Value::from("30mb"))).unwrap(), "30mb");
        assert!(valid_id("jobs-get-result-buckets"));
        assert!(!valid_id("Bad"));
    }
}

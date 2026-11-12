//! Data frame analytics: job configurations and states, `_explain` (field selection from the
//! source's mappings and a memory estimate), and `_evaluate`, computed over the index.
//!
//! `_evaluate` follows Elasticsearch's definitions: outlier detection's `auc_roc` comes from the
//! 1st..99th percentiles of the predicted probability of the actual-true and the other
//! documents (percentiles aggregations), turned into a ROC curve by Elasticsearch's
//! interpolation and integrated with the trapezoid rule, so it matches Elasticsearch's
//! approximate value; the confusion-matrix metrics and the regression and classification
//! metrics are exact over the matching documents.

use super::*;
use crate::xpack::{internal, store};

fn no_dfa(id: &str) -> EsError {
    EsError::resource_not_found(format!("No known data frame analytics with id [{id}]"))
}

// --- configuration parsing ---------------------------------------------------------------------
//
// `DataFrameAnalyticsConfig`'s parsers: each analysis fills in Elasticsearch's defaults in its
// `toXContent` field order, and range checks fail the way its constructors do (a
// `status_exception` inside "Failed to build [analysis] after last required field arrived",
// inside the config's "failed to parse field [analysis]").

/// `ObjectParser.declareDouble` / `declareInt` / `declareLong` (numbers and numeric strings).
fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Where the parser stands once the value at `path` is read (its end).
fn end_of(ctx: &Ctx, path: &[crate::json::locate::Seg<'_>]) -> (usize, usize) {
    ctx.raw_json().and_then(|s| crate::json::locate::value_end(s, path)).unwrap_or((1, 1))
}

/// The config's error for a failure inside its `field` object.
fn in_field(ctx: &Ctx, field: &'static str, cause: EsError) -> EsError {
    use crate::json::locate::Seg;
    let (l, c) = end_of(ctx, &[Seg::Key(field)]);
    EsError::x_content_parse(l, c, format!("[data_frame_analytics_config] failed to parse field [{field}]")).caused_by(cause)
}

fn built(name: &str, cause: EsError) -> EsError {
    EsError::x_content_parse_bare(format!("Failed to build [{name}] after last required field arrived")).caused_by(cause)
}

fn bad(msg: impl Into<String>) -> EsError {
    EsError::status(400, msg)
}

/// `BoostedTreeParams` (shared by regression and classification), in output order.
const TREE_PARAMS: [&str; 12] = [
    "alpha", "lambda", "gamma", "eta", "eta_growth_rate_per_tree", "max_trees", "feature_bag_fraction", "num_top_feature_importance_values",
    "soft_tree_depth_limit", "soft_tree_depth_tolerance", "downsample_factor", "max_optimization_rounds_per_hyperparameter",
];

/// Checks and orders the boosted tree parameters of `m` (`maximum_number_trees` is the old
/// name of `max_trees`).
fn tree_params(m: &Map) -> EsResult<Map> {
    let get = |k: &str| -> Option<f64> {
        let v = if k == "max_trees" { m.get(k).or_else(|| m.get("maximum_number_trees")) } else { m.get(k) };
        v.and_then(number)
    };
    let checks: [(&str, fn(f64) -> bool, &str); 12] = [
        ("lambda", |v| v < 0.0, "must be a non-negative double"),
        ("gamma", |v| v < 0.0, "must be a non-negative double"),
        ("eta", |v| !(0.001..=1.0).contains(&v), "must be a double in [0.001, 1]"),
        ("max_trees", |v| v <= 0.0 || v > 2000.0, "must be an integer in [1, 2000]"),
        ("feature_bag_fraction", |v| v <= 0.0 || v > 1.0, "must be a double in (0, 1]"),
        ("num_top_feature_importance_values", |v| v < 0.0, "must be a non-negative integer"),
        ("alpha", |v| v < 0.0, "must be a non-negative double"),
        ("eta_growth_rate_per_tree", |v| !(0.5..=2.0).contains(&v), "must be a double in [0.5, 2.0]"),
        ("soft_tree_depth_limit", |v| v < 0.0, "must be a non-negative double"),
        ("soft_tree_depth_tolerance", |v| v < 0.01, "must be a double greater than or equal to 0.01"),
        ("downsample_factor", |v| v <= 0.0 || v > 1.0, "must be a double in (0, 1]"),
        ("max_optimization_rounds_per_hyperparameter", |v| !(0.0..=20.0).contains(&v), "must be an integer in [0, 20]"),
    ];
    for (k, invalid, msg) in checks {
        if get(k).is_some_and(invalid) {
            return Err(bad(format!("[{k}] {msg}")));
        }
    }
    let ints = ["max_trees", "num_top_feature_importance_values", "max_optimization_rounds_per_hyperparameter"];
    let mut out = Map::new();
    for k in TREE_PARAMS {
        if let Some(v) = get(k) {
            out.insert(k.into(), if ints.contains(&k) { Value::from(v as i64) } else { Value::from(v) });
        }
    }
    Ok(out)
}

/// One analysis (`outlier_detection`, `regression`, `classification`) with its defaults.
fn parse_analysis(ctx: &Ctx, kind: &str, cfg: &Value) -> EsResult<Value> {
    use crate::json::locate::Seg;
    let Value::Obj(m) = cfg else { return Err(EsError::parse(format!("[{kind}] must be an object"))) };
    let fields: &[&str] = match kind {
        "outlier_detection" => &["n_neighbors", "method", "feature_influence_threshold", "compute_feature_influence", "outlier_fraction", "standardization_enabled"],
        "regression" => &["dependent_variable", "prediction_field_name", "training_percent", "randomize_seed", "loss_function", "loss_function_parameter", "feature_processors", "early_stopping_enabled"],
        "classification" => &["dependent_variable", "prediction_field_name", "class_assignment_objective", "num_top_classes", "training_percent", "randomize_seed", "feature_processors", "early_stopping_enabled"],
        other => return Err(EsError::new(400, "named_object_not_found_exception", format!("[1:1] unknown field [{other}]"))),
    };
    if let Some(k) = m.keys().find(|k| !fields.contains(&k.as_str()) && !TREE_PARAMS.contains(&k.as_str()) && k.as_str() != "maximum_number_trees") {
        let (l, c) = ctx.raw_json().and_then(|s| crate::json::locate::locate(s, &[Seg::Key("analysis"), Seg::Key(kind), Seg::Key(k)], crate::json::locate::Target::Name)).unwrap_or((1, 1));
        return Err(EsError::x_content_parse(l, c, format!("[{kind}] unknown field [{k}]")));
    }
    let mut out = Map::new();
    if kind == "outlier_detection" {
        if let Some(n) = m.get("n_neighbors").and_then(number) {
            if n <= 0.0 {
                return Err(bad("[n_neighbors] must be a positive integer"));
            }
            out.insert("n_neighbors".into(), (n as i64).into());
        }
        if let Some(v) = m.get("method") {
            let s = v.to_es_string().to_lowercase();
            if !["lof", "ldof", "distance_kth_nn", "distance_knn"].contains(&s.as_str()) {
                return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.xpack.core.ml.dataframe.analyses.OutlierDetection.Method.{}", s.to_uppercase())));
            }
            out.insert("method".into(), s.into());
        }
        for k in ["feature_influence_threshold", "outlier_fraction"] {
            if let Some(v) = m.get(k).and_then(number)
                && !(0.0..=1.0).contains(&v)
            {
                return Err(bad(format!("[{k}] must be in [0, 1]")));
            }
        }
        if let Some(v) = m.get("feature_influence_threshold").and_then(number) {
            out.insert("feature_influence_threshold".into(), v.into());
        }
        out.insert("compute_feature_influence".into(), m.get("compute_feature_influence").cloned().unwrap_or(Value::Bool(true)));
        out.insert("outlier_fraction".into(), m.get("outlier_fraction").and_then(number).map_or(Value::from(0.05), Value::from));
        out.insert("standardization_enabled".into(), m.get("standardization_enabled").cloned().unwrap_or(Value::Bool(true)));
        return Ok(Value::Obj(out));
    }
    let Some(dep) = m.get("dependent_variable").map(Value::to_es_string) else { return Err(EsError::illegal_argument("Required [dependent_variable]")) };
    let wrap = |e: EsError| built(kind, e);
    let trees = tree_params(m).map_err(wrap)?;
    if kind == "classification"
        && let Some(n) = m.get("num_top_classes").and_then(number)
        && !(-1.0..=1000.0).contains(&n)
    {
        return Err(wrap(bad("[num_top_classes] must be an integer in [0, 1000] or a special value -1")));
    }
    let training = m.get("training_percent").and_then(number);
    if training.is_some_and(|t| t <= 0.0 || t > 100.0) {
        return Err(wrap(bad("[training_percent] must be a positive double in (0, 100]")));
    }
    if kind == "regression"
        && m.get("loss_function_parameter").and_then(number).is_some_and(|p| p <= 0.0)
    {
        return Err(wrap(bad("[loss_function_parameter] must be a positive double")));
    }
    out.insert("dependent_variable".into(), dep.clone().into());
    out.extend(trees);
    let seed = m.get("randomize_seed").and_then(number).map_or_else(|| rand::random::<i64>(), |s| s as i64);
    let prediction = m.get("prediction_field_name").map(Value::to_es_string).unwrap_or_else(|| format!("{dep}_prediction"));
    if kind == "classification" {
        out.insert("class_assignment_objective".into(), m.get("class_assignment_objective").map(|v| v.to_es_string().to_lowercase()).unwrap_or_else(|| "maximize_minimum_recall".into()).into());
        out.insert("num_top_classes".into(), m.get("num_top_classes").and_then(number).map_or(2, |n| n as i64).into());
        out.insert("prediction_field_name".into(), prediction.into());
        out.insert("training_percent".into(), training.unwrap_or(100.0).into());
        out.insert("randomize_seed".into(), seed.into());
    } else {
        out.insert("prediction_field_name".into(), prediction.into());
        out.insert("training_percent".into(), training.unwrap_or(100.0).into());
        out.insert("randomize_seed".into(), seed.into());
        out.insert("loss_function".into(), m.get("loss_function").map(|v| v.to_es_string().to_lowercase()).unwrap_or_else(|| "mse".into()).into());
        if let Some(p) = m.get("loss_function_parameter").and_then(number) {
            out.insert("loss_function_parameter".into(), p.into());
        }
    }
    if let Some(fp) = m.get("feature_processors").filter(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
        out.insert("feature_processors".into(), fp.clone());
    }
    out.insert("early_stopping_enabled".into(), m.get("early_stopping_enabled").cloned().unwrap_or(Value::Bool(true)));
    Ok(Value::Obj(out))
}

/// `DataFrameAnalyticsSource`: `index` (one or more non-empty names), `query`, `_source`,
/// `runtime_mappings`.
fn parse_source(v: &Value) -> EsResult<Map> {
    let Value::Obj(m) = v else { return Err(EsError::illegal_argument("[source] must be an object")) };
    let index: Vec<Value> = match m.get("index") {
        None => return Err(EsError::illegal_argument("Required [index]")),
        Some(Value::Arr(a)) => a.clone(),
        Some(other) => vec![other.clone()],
    };
    if index.is_empty() {
        return Err(built("data_frame_analytics_source", EsError::illegal_argument("source.index must specify at least one index")));
    }
    if index.iter().any(|i| i.as_str().is_none_or(str::is_empty)) {
        return Err(built("data_frame_analytics_source", EsError::illegal_argument("source.index must contain non-null and non-empty strings")));
    }
    let mut out = Map::new();
    out.insert("index".into(), Value::Arr(index));
    out.insert("query".into(), m.get("query").cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} }));
    for k in ["_source", "runtime_mappings"] {
        if let Some(v) = m.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    Ok(out)
}

/// `DataFrameAnalyticsDest`: a non-empty `index` and `results_field` (`ml`).
fn parse_dest(v: &Value) -> EsResult<Map> {
    let Value::Obj(m) = v else { return Err(EsError::illegal_argument("[dest] must be an object")) };
    let Some(index) = m.get("index").map(Value::to_es_string) else { return Err(EsError::illegal_argument("Required [index]")) };
    if index.is_empty() {
        return Err(built("data_frame_analytics_dest", EsError::illegal_argument("[index] must be non-empty")));
    }
    Ok([("index".to_string(), Value::from(index)), ("results_field".to_string(), m.get("results_field").cloned().unwrap_or_else(|| Value::from("ml")))].into_iter().collect())
}

/// `PutDataFrameAnalyticsAction.Request.validate`: the destination's name, and analyzed
/// fields the source's `_source` filter drops.
fn validate_request(src: &Map, dest: &Map, analyzed: Option<&Value>) -> EsResult<()> {
    let mut errors = Vec::new();
    let index = dest.get("index").map(Value::to_es_string).unwrap_or_default();
    if let Some(p) = crate::meta::names::name_problem(&index) {
        errors.push(format!("Invalid index name [{index}], {p}"));
    } else if index.to_lowercase() != index {
        errors.push(format!("Destination index [{index}] must be lowercase"));
    }
    let list = |v: Option<&Value>| -> Vec<String> {
        match v {
            Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
            Some(Value::Str(s)) => vec![s.clone()],
            _ => Vec::new(),
        }
    };
    let includes = match analyzed {
        Some(Value::Obj(a)) => list(a.get("includes")),
        Some(other @ (Value::Arr(_) | Value::Str(_))) => list(Some(other)),
        _ => Vec::new(),
    };
    if let Some(Value::Obj(s)) = src.get("_source") {
        let (inc, exc) = (list(s.get("includes")), list(s.get("excludes")));
        for f in &includes {
            let kept = (inc.is_empty() || inc.iter().any(|p| crate::xcontent::filter::glob_match(p, f))) && !exc.iter().any(|p| crate::xcontent::filter::glob_match(p, f));
            if !kept {
                errors.push(format!("field [{f}] is included in [analyzed_fields] but not in [source._source]"));
            }
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(EsError::action_request_validation(&errors)) }
}

/// `SourceDestValidator` against the cluster: concrete source indices exist, no remote
/// source, and the destination (its write index) is not one of the sources.
fn validate_source_dest(st: &crate::meta::ClusterState, src: &Map, dest: &Map, now: i64) -> EsResult<()> {
    let sources: Vec<String> = src.get("index").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    let fail = |m: String| {
        let e = EsError::new(400, "validation_exception", format!("Validation Failed: 1: {m};"));
        Err(e)
    };
    if sources.iter().any(|s| s.contains(':')) {
        return fail("remote source and cross-project indices are not supported".into());
    }
    let opts = crate::meta::resolve::IndicesOptions::LENIENT_EXPAND_OPEN;
    let mut resolved = Vec::new();
    for s in &sources {
        if !s.contains('*') && crate::meta::resolve::resolve(st, std::slice::from_ref(s), &crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN, now).is_err() {
            return fail(format!("no such index [{s}]"));
        }
        resolved.extend(crate::meta::resolve::resolve(st, std::slice::from_ref(s), &opts, now).unwrap_or_default());
    }
    let dest_name = dest.get("index").map(Value::to_es_string).unwrap_or_default();
    let holders: Vec<&String> = st.metadata.indices.values().filter(|i| i.aliases.contains_key(&dest_name)).map(|i| &i.name).collect();
    let write = if st.metadata.indices.contains_key(&dest_name) {
        Some(dest_name.clone())
    } else if holders.len() == 1 {
        Some(holders[0].clone())
    } else if holders.len() > 1 {
        let w = st.metadata.indices.values().find(|i| i.aliases.get(&dest_name).is_some_and(|a| a.is_write_index == Some(true))).map(|i| i.name.clone());
        if w.is_none() {
            return fail(format!("no write index is defined for alias [{dest_name}]. The write index may be explicitly disabled using is_write_index=false or the alias points to multiple indices without one being designated as a write index"));
        }
        w
    } else {
        None
    };
    if let Some(w) = write
        && resolved.contains(&w)
    {
        return fail(format!("Destination index [{w}] is included in source expression [{}]", sources.join(",")));
    }
    Ok(())
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    if !valid_id(&id) {
        return Err(invalid_id("id", &id));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    // DataFrameAnalyticsConfig's strict parser (create_time, version and headers are rejected).
    const FIELDS: &[&str] = &["id", "description", "source", "dest", "analysis", "analyzed_fields", "model_memory_limit", "allow_lazy_start", "max_num_threads", "_meta"];
    if let Some(k) = body.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(crate::xpack::unknown_field(ctx, "data_frame_analytics_config", k));
    }
    if let Some(b) = body.get("id").map(Value::to_es_string)
        && b != id
    {
        return Err(EsError::illegal_argument(format!("Inconsistent id; '{b}' specified in the body differs from '{id}' specified as a URL argument")));
    }
    let src = match body.get("source") {
        Some(v) => parse_source(v).map_err(|e| in_field(ctx, "source", e))?,
        None => return Err(EsError::action_request_validation(&["[source] must not be null.".into()])),
    };
    let dest = match body.get("dest") {
        Some(v) => parse_dest(v).map_err(|e| in_field(ctx, "dest", e))?,
        None => return Err(EsError::action_request_validation(&["[dest] must not be null.".into()])),
    };
    let an = match body.get("analysis") {
        Some(Value::Obj(a)) if a.is_empty() => {
            let (l, c) = end_of(ctx, &[crate::json::locate::Seg::Key("analysis")]);
            let cause = EsError::new(400, "parsing_exception", "Failed to parse object: expecting token of type [FIELD_NAME] but found [END_OBJECT]").with_meta("line", l).with_meta("col", c);
            return Err(in_field(ctx, "analysis", cause));
        }
        Some(Value::Obj(a)) => {
            let mut an = Map::new();
            for (kind, cfg) in a {
                an.insert(kind.clone(), parse_analysis(ctx, kind, cfg).map_err(|e| in_field(ctx, "analysis", e))?);
            }
            an
        }
        _ => return Err(EsError::action_request_validation(&["[analysis] must not be null.".into()])),
    };
    validate_request(&src, &dest, body.get("analyzed_fields"))?;
    validate_source_dest(&ctx.state(), &src, &dest, ctx.now_ms)?;
    let mut m = Map::new();
    // `DataFrameAnalyticsConfig.toXContent` order; the creator's roles as `authorization`.
    m.insert("id".into(), id.clone().into());
    m.insert("create_time".into(), ctx.now_ms.into());
    m.insert("version".into(), CONFIG_VERSION.into());
    let roles = crate::xpack::security::principal(ctx).roles.clone();
    m.insert("authorization".into(), obj! { "roles" => Value::Arr(roles.into_iter().map(Value::from).collect()) });
    if let Some(d) = body.get("description") {
        m.insert("description".into(), d.clone());
    }
    m.insert("source".into(), Value::Obj(src));
    m.insert("dest".into(), Value::Obj(dest));
    m.insert("analysis".into(), Value::Obj(an));
    if let Some(a) = body.get("analyzed_fields") {
        // A `FetchSourceContext`: includes and excludes, lists either way.
        let list = |v: Option<&Value>| match v {
            Some(Value::Arr(a)) => Value::Arr(a.clone()),
            Some(Value::Str(s)) => crate::arr![s.clone()],
            _ => Value::Arr(Vec::new()),
        };
        let (inc, exc) = match a {
            Value::Obj(o) => (list(o.get("includes")), list(o.get("excludes"))),
            other => (list(Some(other)), Value::Arr(Vec::new())),
        };
        m.insert("analyzed_fields".into(), obj! { "includes" => inc, "excludes" => exc });
    }
    m.insert("model_memory_limit".into(), body.get("model_memory_limit").cloned().unwrap_or(Value::from("1gb")));
    m.insert("allow_lazy_start".into(), body.get("allow_lazy_start").cloned().unwrap_or(Value::Bool(false)));
    m.insert("max_num_threads".into(), body.get("max_num_threads").cloned().unwrap_or(Value::from(1)));
    if let Some(meta) = body.get("_meta") {
        m.insert("_meta".into(), meta.clone());
    }
    let v = Value::Obj(m);
    let out = v.clone();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(DFAS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("A data frame analytics with id [{id}] already exists")));
            }
            store::put(st, DFAS, &id, v, now).map(|_| ())
        })
        .await?;
    Reply::ok(out)
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let list = expand(&st, DFAS, ctx.path_param("id"), ctx.bool_param("allow_no_match", true)?, no_dfa)?;
    // `exclude_generated` drops what Elasticsearch generated (for exporting a config).
    let export = ctx.bool_param("exclude_generated", false)?;
    let v: Vec<Value> = list
        .iter()
        .map(|(_, d)| match public(d) {
            Value::Obj(mut m) if export => {
                for k in ["create_time", "version", "authorization"] {
                    m.shift_remove(k);
                }
                Value::Obj(m)
            }
            other => other,
        })
        .collect();
    Reply::ok(obj! { "count" => v.len(), "data_frame_analytics" => Value::Arr(v) })
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let list = expand(&st, DFAS, ctx.path_param("id"), ctx.bool_param("allow_no_match", true)?, no_dfa)?;
    let mut out = Vec::new();
    for (id, d) in list {
        let state = roll_state(&d).get("state").map(Value::to_es_string).unwrap_or_else(|| "stopped".into());
        out.push(obj! {
            "id" => id.clone(),
            "state" => state,
            "progress" => crate::arr![
                obj! { "phase" => "reindexing", "progress_percent" => 0 },
                obj! { "phase" => "loading_data", "progress_percent" => 0 },
                obj! { "phase" => "computing_outliers", "progress_percent" => 0 },
                obj! { "phase" => "writing_results", "progress_percent" => 0 },
            ],
            "data_counts" => obj! { "training_docs_count" => 0, "test_docs_count" => 0, "skipped_docs_count" => 0 },
            "memory_usage" => obj! { "peak_usage_bytes" => 0, "status" => "ok" },
        });
    }
    Reply::ok(obj! { "count" => out.len(), "data_frame_analytics" => Value::Arr(out) })
}

async fn set_state(ctx: &Ctx<'_>, id: &str, state: &'static str) -> EsResult<()> {
    let id = id.to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(d) = st.metadata.xpack.get(DFAS, &id).cloned() else { return Err(no_dfa(&id)) };
            store::put(st, DFAS, &id, with_roll_state(d, |r| {
                r.insert("state".into(), state.into());
            }), now)
            .map(|_| ())
        })
        .await
}

pub async fn start(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    set_state(ctx, &id, "started").await?;
    Reply::ok(obj! { "acknowledged" => true, "node" => ctx.cluster.node.id.clone() })
}

pub async fn stop(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    for (id, _) in expand(&st, DFAS, ctx.path_param("id"), ctx.bool_param("allow_no_match", true)?, no_dfa)? {
        set_state(ctx, &id, "stopped").await?;
    }
    Reply::ok(obj! { "stopped" => true })
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| if store::remove(st, DFAS, &id).is_some() { Ok(()) } else { Err(no_dfa(&id)) })
        .await?;
    Reply::acknowledged()
}

pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let now = ctx.now_ms;
    let out = ctx
        .cluster
        .update(move |st| {
            let Some(mut d) = st.metadata.xpack.get(DFAS, &id).cloned() else { return Err(no_dfa(&id)) };
            if let Value::Obj(m) = &mut d {
                for k in ["description", "model_memory_limit", "allow_lazy_start", "max_num_threads"] {
                    if let Some(v) = body.get(k) {
                        m.insert(k.into(), v.clone());
                    }
                }
            }
            store::put(st, DFAS, &id, d.clone(), now)?;
            Ok(public(&d))
        })
        .await?;
    Reply::ok(out)
}

const SUPPORTED: &[&str] = &["boolean", "byte", "double", "float", "half_float", "integer", "long", "scaled_float", "short", "unsigned_long"];

pub async fn explain(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let body = match ctx.path_param("id") {
        Some(id) => public(st.metadata.xpack.get(DFAS, id).ok_or_else(|| no_dfa(id))?),
        None => ctx.body()?.unwrap_or_else(Value::object),
    };
    let index = match body.pointer(&["source", "index"]) {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect::<Vec<_>>().join(","),
        Some(v) => v.to_es_string(),
        None => return Err(EsError::action_request_validation(&["[source] must not be null.".into()])),
    };
    let caps = internal::call(ctx.cluster, "GET", &format!("/{index}/_field_caps?fields=*"), None).await?;
    let mut rows = Vec::new();
    let mut included = 0usize;
    if let Some(Value::Obj(fields)) = caps.get("fields") {
        let mut names: Vec<&String> = fields.keys().filter(|k| !k.starts_with('_')).collect();
        names.sort();
        for name in names {
            let types: Vec<String> = fields[name].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
            if types.iter().any(|t| t == "object" || t == "nested") {
                continue;
            }
            let ok = types.iter().all(|t| SUPPORTED.contains(&t.as_str()));
            let mut m = Map::new();
            m.insert("name".into(), name.clone().into());
            m.insert("mapping_types".into(), Value::Arr(types.iter().map(Value::from).collect()));
            m.insert("is_included".into(), ok.into());
            m.insert("is_required".into(), false.into());
            if ok {
                included += 1;
                m.insert("feature_type".into(), "numerical".into());
            } else {
                m.insert("reason".into(), format!("unsupported type; supported types are [{}]", SUPPORTED.join(", ")).into());
            }
            rows.push(Value::Obj(m));
        }
    }
    let count = internal::call(ctx.cluster, "GET", &format!("/{index}/_count"), None).await.ok().and_then(|c| c.get("count").and_then(Value::as_i64)).unwrap_or(0);
    // A data frame of `count × included` doubles plus fixed overhead, in whole kilobytes.
    let kb = (1024 + (count as usize * included.max(1) * 8) / 1024).max(1);
    let est = if kb.is_multiple_of(1024) { format!("{}mb", kb / 1024) } else { format!("{kb}kb") };
    Reply::ok(obj! { "field_selection" => Value::Arr(rows), "memory_estimation" => obj! { "expected_memory_without_disk" => est.clone(), "expected_memory_with_disk" => est } })
}

/// Elasticsearch's ROC curve from the TP and FP percentiles (`AbstractAucRoc`).
fn auc_from_percentiles(tp: &[f64; 99], fp: &[f64; 99]) -> f64 {
    #[derive(Clone, Copy)]
    struct Point {
        tpr: f64,
        fpr: f64,
        threshold: f64,
    }
    let rate = |i: usize| 1.0 - 0.01 * (i + 1) as f64;
    let ulp = |x: f64| -> f64 {
        if x == 0.0 {
            f64::from_bits(1)
        } else {
            let b = x.abs().to_bits();
            f64::from_bits(b + 1) - x.abs()
        }
    };
    let threshold = |p: &[f64; 99], i: usize| (p[i] - ulp(p[i])).max(0.0);
    let interpolate = |p: &[f64; 99], t: f64| -> f64 {
        // Arrays.binarySearch over the (sorted) percentiles.
        match p.binary_search_by(|x| x.total_cmp(&t)) {
            Ok(i) => rate(i),
            Err(right) => {
                if right >= p.len() {
                    0.0
                } else if right == 0 {
                    1.0
                } else {
                    let left = right - 1;
                    let (x1, y1, x2, y2) = (p[left], rate(left), p[right], rate(right));
                    y1 + (t - x1) * (y2 - y1) / (x2 - x1)
                }
            }
        }
    };
    let mut points = Vec::with_capacity(198);
    for i in 0..99 {
        let t = threshold(tp, i);
        points.push(Point { tpr: rate(i), fpr: interpolate(fp, t), threshold: t });
    }
    for i in 0..99 {
        let t = threshold(fp, i);
        points.push(Point { tpr: interpolate(tp, t), fpr: rate(i), threshold: t });
    }
    points.sort_by(|a, b| b.threshold.total_cmp(&a.threshold).then(a.fpr.total_cmp(&b.fpr)).then(a.tpr.total_cmp(&b.tpr)));
    let mut collapsed: Vec<Point> = Vec::new();
    let mut group: Vec<Point> = Vec::new();
    let flush = |group: &mut Vec<Point>, out: &mut Vec<Point>| {
        if group.is_empty() {
            return;
        }
        if group.len() == 1 {
            out.push(group[0]);
        } else {
            let n = group.len() as f64;
            let (mut t, mut f, mut th) = (0.0, 0.0, 0.0);
            for p in group.iter() {
                t += p.tpr;
                f += p.fpr;
                th += p.threshold;
            }
            out.push(Point { tpr: t / n, fpr: f / n, threshold: th / n });
        }
        group.clear();
    };
    for p in points {
        if !group.is_empty() && group[0].threshold != p.threshold {
            flush(&mut group, &mut collapsed);
        }
        group.push(p);
    }
    flush(&mut group, &mut collapsed);
    let mut curve = Vec::with_capacity(collapsed.len() + 2);
    curve.push(Point { tpr: 0.0, fpr: 0.0, threshold: 1.0 });
    curve.extend(collapsed);
    curve.push(Point { tpr: 1.0, fpr: 1.0, threshold: 0.0 });
    let mut auc = 0.0;
    for w in curve.windows(2) {
        auc += (w[1].fpr - w[0].fpr) * (w[1].tpr + w[0].tpr) / 2.0;
    }
    auc
}

fn percentiles_of(v: Option<&Value>) -> EsResult<[f64; 99]> {
    let mut out = [0.0; 99];
    let Some(Value::Obj(m)) = v.and_then(|a| a.get("values")) else { return Ok(out) };
    for (k, x) in m {
        let p: f64 = k.parse().unwrap_or(0.0);
        let Some(val) = x.as_f64() else {
            return Err(EsError::new(400, "status_exception", "[auc_roc] requires at all the percentiles values to be finite numbers"));
        };
        let i = (p as usize).saturating_sub(1).min(98);
        out[i] = val;
    }
    Ok(out)
}

async fn search(ctx: &Ctx<'_>, index: &str, body: &Value) -> EsResult<Value> {
    internal::call(ctx.cluster, "POST", &format!("/{index}/_search"), Some(body)).await
}

pub async fn evaluate(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(index) = body.get("index").map(|i| match i {
        Value::Arr(a) => a.iter().map(Value::to_es_string).collect::<Vec<_>>().join(","),
        v => v.to_es_string(),
    }) else {
        return Err(EsError::action_request_validation(&["[index] must not be null.".into()]));
    };
    let query = body.get("query").cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} });
    let Some(Value::Obj(eval)) = body.get("evaluation") else {
        return Err(EsError::action_request_validation(&["[evaluation] must not be null.".into()]));
    };
    let Some((kind, cfg)) = eval.iter().next() else { return Err(EsError::illegal_argument("evaluation is empty")) };
    let actual = cfg.get("actual_field").map(Value::to_es_string).unwrap_or_default();
    let mut result = Map::new();
    match kind.as_str() {
        "outlier_detection" => {
            let predicted = cfg.get("predicted_probability_field").map(Value::to_es_string).unwrap_or_default();
            let is_true = obj! { "query_string" => obj! { "query" => format!("{actual}:(1 OR true)") } };
            let metrics = cfg.get("metrics").and_then(Value::as_object).cloned().unwrap_or_else(|| {
                let mut m = Map::new();
                for k in ["auc_roc", "precision", "recall", "confusion_matrix"] {
                    m.insert(k.into(), match k {
                        "auc_roc" => obj! { "include_curve" => false },
                        _ => obj! { "at" => crate::arr![0.25, 0.5, 0.75] },
                    });
                }
                m
            });
            for (metric, mcfg) in &metrics {
                match metric.as_str() {
                    "auc_roc" => {
                        let pct: Vec<Value> = (1..100).map(|p| Value::from(p as f64)).collect();
                        let pagg = obj! { "percentiles" => obj! { "field" => predicted.clone(), "percents" => Value::Arr(pct) } };
                        let q = obj! {
                            "size" => 0,
                            "query" => query.clone(),
                            "aggs" => obj! {
                                "t" => obj! { "filter" => is_true.clone(), "aggs" => obj! { "p" => pagg.clone() } },
                                "f" => obj! { "filter" => obj! { "bool" => obj! { "must_not" => is_true.clone() } }, "aggs" => obj! { "p" => pagg } },
                            },
                        };
                        let r = search(ctx, &index, &q).await?;
                        let count = |k: &str| r.pointer(&["aggregations", k, "doc_count"]).and_then(Value::as_i64).unwrap_or(0);
                        if count("t") == 0 {
                            return Err(EsError::new(400, "status_exception", format!("[auc_roc] requires at least one [{actual}] to have the value [true]")));
                        }
                        if count("f") == 0 {
                            return Err(EsError::new(400, "status_exception", format!("[auc_roc] requires at least one [{actual}] to have a different value than [true]")));
                        }
                        let tp = percentiles_of(r.pointer(&["aggregations", "t", "p"]))?;
                        let fp = percentiles_of(r.pointer(&["aggregations", "f", "p"]))?;
                        let _ = mcfg;
                        result.insert("auc_roc".into(), obj! { "value" => auc_from_percentiles(&tp, &fp) });
                    }
                    "precision" | "recall" | "confusion_matrix" => {
                        let at: Vec<f64> = mcfg.get("at").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
                        let mut out = Map::new();
                        for t in at {
                            let mut aggs = Map::new();
                            for (name, a, p) in [("tp", true, true), ("fp", false, true), ("tn", false, false), ("fn", true, false)] {
                                let actual_q = if a { is_true.clone() } else { obj! { "bool" => obj! { "must_not" => is_true.clone() } } };
                                let pred_q = obj! { "range" => obj! { predicted.clone() => obj! { "gte" => t } } };
                                let pred_q = if p { pred_q } else { obj! { "bool" => obj! { "must_not" => pred_q } } };
                                aggs.insert(name.into(), obj! { "filter" => obj! { "bool" => obj! { "filter" => crate::arr![actual_q, pred_q] } } });
                            }
                            let r = search(ctx, &index, &obj! { "size" => 0, "query" => query.clone(), "aggs" => Value::Obj(aggs) }).await?;
                            let c = |k: &str| r.pointer(&["aggregations", k, "doc_count"]).and_then(Value::as_i64).unwrap_or(0);
                            let (tp, fp, tn, fne) = (c("tp"), c("fp"), c("tn"), c("fn"));
                            let key = crate::json::write::java_double(t);
                            let v = match metric.as_str() {
                                "precision" => Value::from(if tp + fp == 0 { 0.0 } else { tp as f64 / (tp + fp) as f64 }),
                                "recall" => Value::from(if tp + fne == 0 { 0.0 } else { tp as f64 / (tp + fne) as f64 }),
                                _ => obj! { "tp" => tp, "fp" => fp, "tn" => tn, "fn" => fne },
                            };
                            out.insert(key, v);
                        }
                        result.insert(metric.clone(), Value::Obj(out));
                    }
                    other => return Err(EsError::x_content_parse_bare(format!("[outlier_detection] unknown field [{other}]"))),
                }
            }
        }
        "regression" | "classification" => {
            let predicted = cfg.get("predicted_field").map(Value::to_es_string).unwrap_or_default();
            let r = search(ctx, &index, &obj! { "size" => 10_000, "query" => query, "_source" => crate::arr![actual.clone(), predicted.clone()] }).await?;
            let pairs: Vec<(Value, Value)> = r
                .pointer(&["hits", "hits"])
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|h| {
                    let s = h.get("_source")?;
                    Some((crate::xpack::security::query::path_values(s, &actual).into_iter().next()?, crate::xpack::security::query::path_values(s, &predicted).into_iter().next()?))
                })
                .collect();
            if kind == "regression" {
                let nums: Vec<(f64, f64)> = pairs.iter().filter_map(|(a, p)| Some((a.as_f64()?, p.as_f64()?))).collect();
                let n = nums.len().max(1) as f64;
                let metrics = cfg.get("metrics").and_then(Value::as_object).cloned().unwrap_or_else(|| {
                    let mut m = Map::new();
                    for k in ["mse", "r_squared", "huber"] {
                        m.insert(k.into(), obj! {});
                    }
                    m
                });
                for (metric, mcfg) in metrics {
                    let v = match metric.as_str() {
                        "mse" => nums.iter().map(|(a, p)| (a - p).powi(2)).sum::<f64>() / n,
                        "msle" => {
                            let off = mcfg.get("offset").and_then(Value::as_f64).unwrap_or(1.0);
                            nums.iter().map(|(a, p)| ((a + off).ln() - (p + off).ln()).powi(2)).sum::<f64>() / n
                        }
                        "huber" => {
                            let d = mcfg.get("delta").and_then(Value::as_f64).unwrap_or(1.0);
                            nums.iter().map(|(a, p)| d * d * ((1.0 + ((a - p) / d).powi(2)).sqrt() - 1.0)).sum::<f64>() / n
                        }
                        "r_squared" => {
                            let mean = nums.iter().map(|(a, _)| a).sum::<f64>() / n;
                            let ss_res: f64 = nums.iter().map(|(a, p)| (a - p).powi(2)).sum();
                            let ss_tot: f64 = nums.iter().map(|(a, _)| (a - mean).powi(2)).sum();
                            if ss_tot == 0.0 { 0.0 } else { 1.0 - ss_res / ss_tot }
                        }
                        other => return Err(EsError::x_content_parse_bare(format!("[regression] unknown field [{other}]"))),
                    };
                    result.insert(metric, obj! { "value" => v });
                }
            } else {
                let metrics = cfg.get("metrics").and_then(Value::as_object).cloned().unwrap_or_else(|| {
                    let mut m = Map::new();
                    m.insert("accuracy".into(), obj! {});
                    m
                });
                let total = pairs.len().max(1) as f64;
                for (metric, _) in metrics {
                    match metric.as_str() {
                        "accuracy" => {
                            let mut classes: std::collections::BTreeMap<String, (i64, i64, i64)> = Default::default();
                            for (a, p) in &pairs {
                                let (a, p) = (a.to_es_string(), p.to_es_string());
                                for c in [a.clone(), p.clone()] {
                                    classes.entry(c).or_default();
                                }
                                if a == p {
                                    classes.get_mut(&a).expect("class").0 += 1;
                                }
                            }
                            let correct: i64 = pairs.iter().filter(|(a, p)| a.to_es_string() == p.to_es_string()).count() as i64;
                            let per: Vec<Value> = classes
                                .keys()
                                .map(|c| {
                                    let right = pairs.iter().filter(|(a, p)| (a.to_es_string() == *c) == (p.to_es_string() == *c)).count();
                                    obj! { "class_name" => c.clone(), "value" => right as f64 / total }
                                })
                                .collect();
                            result.insert("accuracy".into(), obj! { "classes" => Value::Arr(per), "overall_accuracy" => correct as f64 / total });
                        }
                        other => return Err(EsError::x_content_parse_bare(format!("[classification] unknown field [{other}]"))),
                    }
                }
            }
        }
        other => return Err(EsError::x_content_parse_bare(format!("Unknown NamedObject [{other}]"))),
    }
    let mut out = Map::new();
    out.insert(kind.clone(), Value::Obj(result));
    Reply::ok(Value::Obj(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_separation_has_auc_one() {
        let mut tp = [0.0; 99];
        let mut fp = [0.0; 99];
        for i in 0..99 {
            tp[i] = 0.9 + 0.001 * i as f64;
            fp[i] = 0.1 + 0.001 * i as f64;
        }
        let auc = auc_from_percentiles(&tp, &fp);
        assert!((auc - 1.0).abs() < 0.02, "{auc}");
        let auc = auc_from_percentiles(&fp, &tp);
        assert!(auc < 0.05, "{auc}");
    }
}

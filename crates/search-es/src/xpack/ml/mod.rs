//! Machine learning: anomaly detection jobs and datafeeds ([`jobs`]), calendars and filters,
//! results (read from the results indices), trained models and deployments ([`models`]), and
//! data frame analytics ([`dfa`]).
//!
//! **What is real:** every stored object (jobs, datafeeds, calendars and events, filters,
//! trained model configurations with their definition parts and vocabularies, model aliases,
//! data frame analytics configurations) with Elasticsearch's validation, defaults, ids and
//! errors; the results indices (`.ml-anomalies-shared-000001` with each job's read and write
//! aliases) and every results API reading them (buckets, overall buckets, records,
//! influencers, categories, model snapshots); datafeed previews (a search of the datafeed's
//! indices); `_evaluate` (outlier detection, regression and classification metrics computed by
//! aggregations over the index, with Elasticsearch's AUC-ROC from percentiles); `_explain`
//! (field selection from the mappings); `_estimate_model_memory` (Elasticsearch's formula);
//! tree-ensemble model inference.
//!
//! **What is emulated:** roll runs no ML native processes. Opening a job, starting a datafeed
//! or a deployment moves it through Elasticsearch's states and reports Elasticsearch's stats
//! shapes, but no data is analysed: no new buckets, records or forecasts appear, PyTorch models
//! answer inference with a placeholder of the right shape, snapshot upgrades fail, and
//! `_data` (post data) is not supported.

pub mod dfa;
pub mod jobs;
pub mod models;

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};

pub const JOBS: &str = "ml.job";
pub const DATAFEEDS: &str = "ml.datafeed";
pub const CALENDARS: &str = "ml.calendar";
pub const EVENTS: &str = "ml.event";
pub const FILTERS: &str = "ml.filter";
pub const MODELS: &str = "ml.trained_model";
pub const ALIASES: &str = "ml.model_alias";
pub const DFAS: &str = "ml.dfa";

/// The ML config version Elasticsearch 9.5.5 stamps on jobs and models.
pub const CONFIG_VERSION: &str = "12.0.0";

/// `MlStrings.isValidId`: lowercase alphanumerics, `-` and `_`, starting and ending with an
/// alphanumeric, at most 64 characters.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_' || *c == b'.')
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
}

pub fn invalid_id(kind: &str, id: &str) -> EsError {
    EsError::illegal_argument(format!(
        "Invalid {kind}; '{id}' can contain lowercase alphanumeric (a-z and 0-9), hyphens or underscores; must start and end with alphanumeric"
    ))
}

/// Expands a comma-separated id expression (`_all`, `*`, wildcards) over a family. Explicit ids
/// that don't exist fail with `missing(id)`; wildcards that match nothing fail unless
/// `allow_no_match`.
pub fn expand(
    st: &crate::meta::ClusterState,
    family: &str,
    expr: Option<&str>,
    allow_no_match: bool,
    missing: impl Fn(&str) -> EsError,
) -> EsResult<Vec<(String, Value)>> {
    let all: Vec<(String, Value)> = st.metadata.xpack.iter(family).map(|(k, v)| (k.clone(), v.clone())).collect();
    let expr = expr.unwrap_or("_all");
    if expr.is_empty() || expr == "_all" || expr == "*" {
        return Ok(all);
    }
    let mut out: Vec<(String, Value)> = Vec::new();
    for part in expr.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let matched: Vec<&(String, Value)> = all.iter().filter(|(k, _)| crate::xcontent::filter::glob_match(part, k)).collect();
        if matched.is_empty() && (!part.contains('*') || !allow_no_match) {
            return Err(missing(part));
        }
        for m in matched {
            if !out.iter().any(|(k, _)| k == &m.0) {
                out.push(m.clone());
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Removes roll's private bookkeeping (`_roll`) from a stored object.
pub fn public(v: &Value) -> Value {
    match v {
        Value::Obj(m) => Value::Obj(m.iter().filter(|(k, _)| k.as_str() != "_roll").map(|(k, v)| (k.clone(), v.clone())).collect()),
        other => other.clone(),
    }
}

/// roll's bookkeeping of a stored object.
pub fn roll_state(v: &Value) -> Map {
    v.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default()
}

pub fn with_roll_state(mut v: Value, f: impl FnOnce(&mut Map)) -> Value {
    if let Value::Obj(m) = &mut v {
        let mut r = m.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
        f(&mut r);
        m.insert("_roll".into(), Value::Obj(r));
    }
    v
}

/// The node as ML stats print it.
pub fn node_value(ctx: &Ctx, with_id: bool) -> Value {
    let n = &ctx.cluster.node;
    let mut m = Map::new();
    if with_id {
        m.insert("id".into(), n.id.clone().into());
    }
    m.insert("name".into(), n.name.clone().into());
    m.insert("ephemeral_id".into(), n.ephemeral_id.clone().into());
    m.insert("transport_address".into(), n.transport_address.clone().into());
    m.insert("attributes".into(), node_attributes());
    Value::Obj(m)
}

fn node_attributes() -> Value {
    obj! {
        "ml.allocated_processors_double" => "2.0",
        "ml.machine_memory" => "3221225472",
        "ml.config_version" => CONFIG_VERSION,
        "ml.max_jvm_size" => "1610612736",
        "ml.allocated_processors" => "2",
    }
}

pub fn info(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(obj! {
        "defaults" => obj! {
            "anomaly_detectors" => obj! {
                "model_memory_limit" => "1gb",
                "categorization_examples_limit" => 4,
                "model_snapshot_retention_days" => 10,
                "daily_model_snapshot_retention_after_days" => 1,
                "categorization_analyzer" => obj! {
                    "char_filter" => crate::arr!["first_line_with_letters"],
                    "tokenizer" => "ml_standard",
                    "filter" => crate::arr![
                        obj! { "type" => "stop", "stopwords" => Value::Arr([
                            "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun",
                            "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December",
                            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "GMT", "UTC",
                        ].iter().map(|s| Value::from(*s)).collect()) },
                        obj! { "type" => "limit", "max_token_count" => "100" },
                    ],
                },
            },
            "datafeeds" => obj! { "scroll_size" => 1000 },
            "model_platform_variant" => "linux-x86_64",
        },
        "upgrade_mode" => false,
        "native_code" => obj! { "version" => crate::version::ES_VERSION, "build_hash" => "15dbdde800b38b" },
        "limits" => obj! {
            "total_ml_memory" => "921mb",
            "total_ml_processors" => 2,
            "effective_max_model_memory_limit" => "881mb",
            "max_single_ml_node_processors" => 2,
        },
    })
}

pub fn memory_stats(ctx: &Ctx) -> EsResult<Reply> {
    let mut nodes = Map::new();
    let mut n = match node_value(ctx, false) {
        Value::Obj(m) => m,
        _ => Map::new(),
    };
    n.insert("roles".into(), crate::arr!["data", "data_cold", "data_content", "data_frozen", "data_hot", "data_warm", "ingest", "master", "ml", "remote_cluster_client", "transform"]);
    n.insert(
        "mem".into(),
        obj! {
            "total_in_bytes" => 3_221_225_472i64,
            "adjusted_total_in_bytes" => 3_221_225_472i64,
            "ml" => obj! {
                "max_in_bytes" => 966_367_641i64, "native_code_overhead_in_bytes" => 0, "anomaly_detectors_in_bytes" => 0,
                "data_frame_analytics_in_bytes" => 0, "native_inference_in_bytes" => 0,
            },
        },
    );
    n.insert("jvm".into(), obj! { "heap_max_in_bytes" => 1_610_612_736i64, "java_inference_max_in_bytes" => 644_245_094i64, "java_inference_in_bytes" => 0 });
    nodes.insert(ctx.cluster.node.id.clone(), Value::Obj(n));
    Reply::ok(obj! {
        "_nodes" => obj! { "total" => 1, "successful" => 1, "failed" => 0 },
        "cluster_name" => ctx.cluster.name.clone(),
        "nodes" => Value::Obj(nodes),
    })
}

pub fn set_upgrade_mode(ctx: &Ctx) -> EsResult<Reply> {
    ctx.bool_param("enabled", false)?;
    Reply::acknowledged()
}

macro_rules! cols {
    ($($n:literal),*) => { &[$(crate::api::cat::Column { name: $n, aliases: &[], desc: None, default: true, right: false, sibling: None, cond: crate::api::cat::Cond::Always }),*] };
}

static CAT_JOBS: &[crate::api::cat::Column] = cols!("id", "state", "data.processed_records", "model.bytes", "model.memory_status", "forecasts.total", "buckets.count");
static CAT_DATAFEEDS: &[crate::api::cat::Column] = cols!("id", "state", "buckets.count", "search.count");
static CAT_MODELS: &[crate::api::cat::Column] = cols!("id", "heap_size", "operations", "create_time", "type", "ingest.pipelines", "data_frame.id");
static CAT_DFA: &[crate::api::cat::Column] = cols!("id", "type", "create_time", "state");

/// `_cat/ml/*`: one row per object with the columns Elasticsearch shows by default.
pub fn cat(ctx: &Ctx, what: &str) -> EsResult<Reply> {
    use crate::api::cat::{Cell, Table};
    let st = ctx.state();
    let s = |v: String| Cell::Str(v);
    let mut t;
    match what {
        "anomaly_detectors" => {
            t = Table::new(CAT_JOBS);
            for (id, j) in st.metadata.xpack.iter(JOBS) {
                let state = roll_state(j).get("state").map(Value::to_es_string).unwrap_or_else(|| "closed".into());
                t.rows.push(vec![s(id.clone()), s(state), s("0".into()), Cell::Bytes(0), s("ok".into()), s("0".into()), s("0".into())]);
            }
        }
        "datafeeds" => {
            t = Table::new(CAT_DATAFEEDS);
            for (id, d) in st.metadata.xpack.iter(DATAFEEDS) {
                let state = roll_state(d).get("state").map(Value::to_es_string).unwrap_or_else(|| "stopped".into());
                t.rows.push(vec![s(id.clone()), s(state), s("0".into()), s("0".into())]);
            }
        }
        "trained_models" => {
            t = Table::new(CAT_MODELS);
            for (id, m) in st.metadata.xpack.iter(MODELS) {
                t.rows.push(vec![
                    s(id.clone()),
                    Cell::Bytes(m.get("model_size_bytes").and_then(Value::as_i64).unwrap_or(0)),
                    s(m.get("estimated_operations").map(Value::to_es_string).unwrap_or_else(|| "0".into())),
                    s(crate::time::java_instant_string(m.get("create_time").and_then(Value::as_i64).unwrap_or(0))),
                    s(m.get("model_type").map(Value::to_es_string).unwrap_or_else(|| "tree_ensemble".into())),
                    s("0".into()),
                    s("__none__".into()),
                ]);
            }
        }
        _ => {
            t = Table::new(CAT_DFA);
            for (id, d) in st.metadata.xpack.iter(DFAS) {
                let ty = d.get("analysis").and_then(Value::as_object).and_then(|m| m.keys().next().cloned()).unwrap_or_default();
                t.rows.push(vec![
                    s(id.clone()),
                    s(ty),
                    s(crate::time::java_instant_string(d.get("create_time").and_then(Value::as_i64).unwrap_or(0))),
                    s(roll_state(d).get("state").map(Value::to_es_string).unwrap_or_else(|| "stopped".into())),
                ]);
            }
        }
    }
    Ok(Reply::Table(t))
}

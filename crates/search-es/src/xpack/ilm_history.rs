//! ILM history: Elasticsearch's `ILMHistoryStore`. A run of [`super::ilm_run`] records an item
//! (`ILMHistoryItem`) where Elasticsearch's runner records one: after an asynchronous step
//! moves the index, where a row of cluster state steps stopped, at a phase change, at a
//! failure (with `error_details`), and when a step deletes the index (its state with the step
//! `complete`). Items go to the hidden `ilm-history-7` data stream after the run, its template
//! installed on first use (Elasticsearch's, with 90 days of data stream lifecycle retention),
//! unless `indices.lifecycle.history_index_enabled` is off.

use super::ilm_run::Exec;
use crate::cluster::Cluster;
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::{enc, internal};

pub(super) const INDEX: &str = "ilm-history-7";

/// `ILMHistoryItem`: `index_age` is counted from the lifecycle date.
pub(super) fn item(index: &str, policy: &str, now: i64, lifecycle_date: Option<i64>, e: &Exec, error: Option<&Value>) -> Value {
    let mut m = obj! { "index" => index, "policy" => policy, "@timestamp" => now };
    if let Value::Obj(o) = &mut m {
        if let Some(d) = lifecycle_date {
            o.insert("index_age".into(), Value::from(now - d));
        }
        o.insert("success".into(), Value::Bool(error.is_none()));
        o.insert("state".into(), state(e));
        if let Some(err) = error {
            o.insert("error_details".into(), Value::from(err.to_json()));
        }
    }
    m
}

/// `LifecycleExecutionState.asMap()`: every value a string, in a `HashMap`'s order.
fn state(e: &Exec) -> Value {
    let mut m = Map::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            m.insert(k.into(), Value::from(v));
        }
    };
    put("phase", e.phase.clone());
    put("action", e.action.clone());
    put("step", e.step.clone());
    put("creation_date", e.lifecycle_date.map(|t| t.to_string()));
    put("phase_time", e.phase_time.map(|t| t.to_string()));
    put("action_time", e.action_time.map(|t| t.to_string()));
    put("step_time", e.step_time.map(|t| t.to_string()));
    put("failed_step", e.failed_step.clone());
    put("is_auto_retryable_error", e.auto_retryable.map(|b| b.to_string()));
    put("failed_step_retry_count", e.retry_count.map(|n| n.to_string()));
    put("step_info", e.step_info.as_ref().map(Value::to_json));
    put("previous_step_info", e.previous_step_info.as_ref().map(Value::to_json));
    put("phase_definition", e.phase_definition.as_ref().map(Value::to_json));
    put("snapshot_name", e.snapshot_name.clone());
    put("snapshot_repository", e.snapshot_repository.clone());
    put("shrink_index_name", e.shrink_index_name.clone());
    put("rollup_index_name", e.downsample_index_name.clone());
    put("force_merge_clone_index_name", e.force_merge_clone_index_name.clone());
    Value::Obj(crate::json::java_hash_map(m, 16))
}

fn template() -> Value {
    let date = || obj! { "format" => "epoch_millis", "type" => "date" };
    let keyword = || obj! { "type" => "keyword" };
    let text = || obj! { "type" => "text" };
    obj! {
        "index_patterns" => vec![Value::from(format!("{INDEX}*"))],
        "template" => obj! {
            "settings" => obj! { "index.number_of_shards" => 1, "index.auto_expand_replicas" => "0-1" },
            "mappings" => obj! { "dynamic" => false, "properties" => obj! {
                "index_age" => obj! { "type" => "long" },
                "@timestamp" => date(),
                "error_details" => text(),
                "success" => obj! { "type" => "boolean" },
                "index" => keyword(),
                "state" => obj! { "dynamic" => true, "type" => "object", "properties" => obj! {
                    "phase" => keyword(),
                    "failed_step" => keyword(),
                    "phase_definition" => text(),
                    "action_time" => date(),
                    "phase_time" => date(),
                    "step_info" => text(),
                    "action" => keyword(),
                    "step" => keyword(),
                    "is_auto-retryable_error" => keyword(),
                    "creation_date" => date(),
                    "step_time" => date(),
                } },
                "policy" => keyword(),
            } },
            "lifecycle" => obj! { "enabled" => true, "data_retention" => "90d" },
        },
        "priority" => 2147483647i64,
        "version" => 7,
        "_meta" => obj! { "managed" => true, "description" => "index template for ILM history indices" },
        "data_stream" => obj! { "hidden" => true, "allow_custom_routing" => false },
        "allow_auto_create" => true,
    }
}

/// Appends a run's items to `ilm-history-7` in one bulk request (failures are dropped, as
/// Elasticsearch only logs them).
pub(super) async fn write(cluster: &Cluster, items: Vec<Value>) {
    if items.is_empty() {
        return;
    }
    let (enabled, templated) = {
        let st = cluster.state();
        (st.metadata.cluster_setting("indices.lifecycle.history_index_enabled") != Some("false"), st.metadata.index_templates.contains_key(INDEX))
    };
    if !enabled {
        return;
    }
    if !templated {
        let _ = internal::call_system(cluster, "PUT", &format!("/_index_template/{}", enc(INDEX)), Some(&template())).await;
    }
    let mut body = String::new();
    for item in items {
        body.push_str("{\"create\":{}}\n");
        body.push_str(&item.to_json());
        body.push('\n');
    }
    let _ = internal::call_ndjson_system(cluster, &format!("/{}/_bulk", enc(INDEX)), body).await;
}

//! Alerts (`Alert`): their documents in `.opendistro-alerting-alerts` and the history indices,
//! `GET _plugins/_alerting/monitors/alerts` and `_acknowledge/alerts`.

use super::{ALERTS_INDEX, ALL_ALERTS, HISTORY_WRITE, alerting};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};

/// The alert schema version the plugin writes.
const SCHEMA_VERSION: i64 = 6;

/// An alert read back: its document id, `_version` and body.
#[derive(Clone)]
pub(crate) struct Stored {
    pub id: String,
    pub version: i64,
    pub alert: Map,
}

impl Stored {
    pub fn state(&self) -> &str {
        self.alert.get("state").and_then(Value::as_str).unwrap_or_default()
    }

    pub fn trigger_id(&self) -> &str {
        self.alert.get("trigger_id").and_then(Value::as_str).unwrap_or_default()
    }

    /// A bucket-level alert's bucket keys, joined as the plugin keys them (`#`).
    pub fn bucket_hash(&self) -> Option<String> {
        let keys = self.alert.get("agg_alert_content")?.get("bucket_keys")?.as_array()?;
        Some(keys.iter().map(Value::to_es_string).collect::<Vec<_>>().join("#"))
    }

    /// The alert as a template argument (`Alert.asTemplateArg`).
    pub fn template(&self) -> Value {
        let a = &self.alert;
        let g = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
        let mut m = obj! {
            "_id" => self.id.clone(), "_version" => self.version, "state" => g("state"), "error_message" => g("error_message"),
            "severity" => g("severity"), "start_time" => g("start_time"), "last_notification_time" => g("last_notification_time"),
            "end_time" => g("end_time"), "acknowledged_time" => g("acknowledged_time"), "action_execution_results" => g("action_execution_results"),
        };
        if let (Value::Obj(m), Some(c)) = (&mut m, a.get("agg_alert_content")) {
            m.insert("agg_alert_content".into(), c.clone());
            m.insert("bucket_keys".into(), c.get("bucket_keys").cloned().unwrap_or(Value::Null));
        }
        m
    }
}

/// What a new alert is made of.
pub(crate) struct New<'a> {
    pub monitor_id: &'a str,
    pub monitor_version: i64,
    pub monitor_name: &'a Value,
    pub execution_id: &'a str,
    pub user: Value,
    pub trigger_id: &'a str,
    pub trigger_name: &'a Value,
    pub severity: &'a Value,
    pub state: &'a str,
    pub error: Option<String>,
    pub finding_ids: Vec<Value>,
    pub related_doc_ids: Vec<Value>,
    pub agg: Option<Value>,
    pub now: i64,
}

/// A new alert document, in the plugin's field order (`Alert.toXContent`).
pub(crate) fn new_alert(n: New<'_>) -> Map {
    let mut m = Map::new();
    m.insert("id".into(), "".into());
    m.insert("version".into(), (-1i64).into());
    m.insert("monitor_id".into(), n.monitor_id.into());
    m.insert("workflow_id".into(), "".into());
    m.insert("workflow_name".into(), "".into());
    m.insert("associated_alert_ids".into(), Value::Arr(vec![]));
    m.insert("schema_version".into(), SCHEMA_VERSION.into());
    m.insert("monitor_version".into(), n.monitor_version.into());
    m.insert("monitor_name".into(), n.monitor_name.clone());
    m.insert("execution_id".into(), n.execution_id.into());
    m.insert("monitor_user".into(), n.user);
    m.insert("trigger_id".into(), n.trigger_id.into());
    m.insert("trigger_name".into(), n.trigger_name.clone());
    m.insert("finding_ids".into(), Value::Arr(n.finding_ids));
    m.insert("related_doc_ids".into(), Value::Arr(n.related_doc_ids));
    m.insert("state".into(), n.state.into());
    m.insert("error_message".into(), n.error.map(Value::from).unwrap_or(Value::Null));
    m.insert("alert_history".into(), Value::Arr(vec![]));
    m.insert("severity".into(), n.severity.clone());
    m.insert("action_execution_results".into(), Value::Arr(vec![]));
    m.insert("start_time".into(), n.now.into());
    m.insert("last_notification_time".into(), n.now.into());
    m.insert("end_time".into(), Value::Null);
    m.insert("acknowledged_time".into(), Value::Null);
    if let Some(a) = n.agg {
        m.insert("agg_alert_content".into(), a);
    }
    m
}

/// A monitor's alerts in the active alerts index.
pub(crate) async fn active(cluster: &Cluster, monitor_id: &str) -> EsResult<Vec<Stored>> {
    if !cluster.state().metadata.indices.contains_key(ALERTS_INDEX) {
        return Ok(Vec::new());
    }
    let body = obj! { "size" => 10_000, "version" => true, "query" => obj! { "term" => obj! { "monitor_id" => monitor_id } }, "sort" => crate::arr![obj! { "start_time" => "asc" }] };
    let r = internal::call_system(cluster, "POST", &format!("/{ALERTS_INDEX}/_search?routing={}", enc(monitor_id)), Some(&body)).await?;
    Ok(hits(&r))
}

fn hits(r: &Value) -> Vec<Stored> {
    r.pointer(&["hits", "hits"])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some(Stored {
                id: h.get("_id")?.to_es_string(),
                version: h.get("_version").and_then(Value::as_i64).unwrap_or(1),
                alert: h.get("_source")?.as_object()?.clone(),
            })
        })
        .collect()
}

/// Writes an alert to the active index (a new one gets an id; the `id` field holds it).
pub(crate) async fn save(cluster: &Cluster, id: Option<&str>, mut alert: Map) -> EsResult<String> {
    let monitor_id = alert.get("monitor_id").map(Value::to_es_string).unwrap_or_default();
    let id = id.map(str::to_string).unwrap_or_else(|| {
        // Document-level alerts are named by a random UUID, the others by an index id.
        if alert.get("related_doc_ids").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) { super::run::random_uuid() } else { crate::meta::names::time_based_id() }
    });
    alert.insert("id".into(), id.clone().into());
    internal::call_system(cluster, "PUT", &format!("/{ALERTS_INDEX}/_doc/{}?routing={}&refresh=true", enc(&id), enc(&monitor_id)), Some(&Value::Obj(alert))).await?;
    Ok(id)
}

/// Moves an alert to the history (it completed, or its monitor went).
pub(crate) async fn to_history(cluster: &Cluster, id: &str, alert: Map, now: i64) -> EsResult<()> {
    super::ensure_history(cluster, now).await?;
    let monitor_id = alert.get("monitor_id").map(Value::to_es_string).unwrap_or_default();
    let routing = enc(&monitor_id);
    internal::call_system(cluster, "PUT", &format!("/{HISTORY_WRITE}/_doc/{}?routing={routing}&refresh=true", enc(id)), Some(&Value::Obj(alert))).await?;
    match internal::call_system(cluster, "DELETE", &format!("/{ALERTS_INDEX}/_doc/{}?routing={routing}&refresh=true", enc(id)), None).await {
        Ok(_) => Ok(()),
        Err(e) if e.status == 404 => Ok(()),
        Err(e) => Err(e),
    }
}

/// A deleted monitor's alerts: moved to the history as `DELETED`.
pub(crate) async fn retire_monitor(cluster: &Cluster, monitor_id: &str, now: i64) -> EsResult<()> {
    for a in active(cluster, monitor_id).await? {
        let mut alert = a.alert;
        alert.insert("state".into(), "DELETED".into());
        alert.insert("version".into(), a.version.into());
        to_history(cluster, &a.id, alert, now).await?;
    }
    Ok(())
}

/// An alert as the alerts API prints it: its document's version, no `monitor_user`.
fn shown(a: &Stored) -> Value {
    let mut m = a.alert.clone();
    m.shift_remove("monitor_user");
    m.insert("version".into(), a.version.into());
    Value::Obj(m)
}

pub async fn get(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let mut filters = Vec::new();
    if let Some(id) = ctx.param("monitorId").filter(|s| !s.is_empty()) {
        filters.push(obj! { "term" => obj! { "monitor_id" => id } });
    }
    if let Some(ids) = ctx.param("monitorIds").filter(|s| !s.is_empty()) {
        filters.push(obj! { "terms" => obj! { "monitor_id" => Value::Arr(ids.split(',').map(Value::from).collect()) } });
    }
    for (param, field) in [("alertState", "state"), ("severityLevel", "severity")] {
        if let Some(v) = ctx.param(param).filter(|v| *v != "ALL" && !v.is_empty()) {
            filters.push(obj! { "term" => obj! { field => v } });
        }
    }
    let mut must = Vec::new();
    if let Some(s) = ctx.param("searchString").filter(|s| !s.is_empty()) {
        must.push(obj! { "query_string" => obj! { "query" => format!("*{s}*"), "default_operator" => "AND", "fields" => crate::arr!["monitor_name", "trigger_name"] } });
    }
    let sort_field = ctx.param("sortString").unwrap_or("monitor_name.keyword");
    let order = if ctx.param("sortOrder").is_some_and(|o| o.eq_ignore_ascii_case("desc")) { "desc" } else { "asc" };
    let mut sort = obj! { "order" => order };
    if let (Some(m), Value::Obj(s)) = (ctx.param("missing"), &mut sort) {
        s.insert("missing".into(), m.into());
    }
    let body = obj! {
        "from" => ctx.int_param("startIndex", 0)?.max(0), "size" => ctx.int_param("size", 10)?.max(0), "version" => true,
        "query" => obj! { "bool" => obj! { "must" => Value::Arr(must), "filter" => Value::Arr(filters) } },
        "sort" => crate::arr![obj! { sort_field => sort }],
    };
    let r = internal::call_system(ctx.cluster, "POST", &format!("/{ALL_ALERTS}/_search?expand_wildcards=all&ignore_unavailable=true"), Some(&body)).await?;
    let total = r.pointer(&["hits", "total", "value"]).and_then(Value::as_i64).unwrap_or(0);
    let alerts: Vec<Value> = hits(&r).iter().map(shown).collect();
    Reply::ok(obj! { "alerts" => Value::Arr(alerts), "totalAlerts" => total })
}

pub async fn acknowledge(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let monitor_id = ctx.path_param("monitor_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let ids: Vec<String> = body.get("alerts").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
    if ids.is_empty() {
        return Err(EsError::illegal_argument("You must provide at least one alert id."));
    }
    if super::read(ctx.cluster, &monitor_id).await?.is_none() {
        return Err(alerting(500, "org.opensearch.ResourceNotFoundException", format!("No monitor found with id [{monitor_id}]")));
    }
    let found = active(ctx.cluster, &monitor_id).await?;
    let (mut success, mut failed) = (Vec::new(), Vec::new());
    for id in &ids {
        let reason = match found.iter().find(|a| &a.id == id) {
            None => Some(format!("Alert: {id} does not exist (it may have already completed).")),
            Some(a) if a.state() == "ACKNOWLEDGED" => Some("Alert has already been acknowledged.".to_string()),
            Some(a) if !matches!(a.state(), "ACTIVE" | "ERROR") => Some(format!("Alert: {id} is in state {}", a.state())),
            Some(a) => {
                let mut alert = a.alert.clone();
                alert.insert("state".into(), "ACKNOWLEDGED".into());
                alert.insert("acknowledged_time".into(), ctx.now_ms.into());
                save(ctx.cluster, Some(id), alert).await?;
                success.push(Value::from(id.clone()));
                None
            }
        };
        if let Some(r) = reason {
            failed.push(obj! { id.clone() => obj! { "failed_reason" => r } });
        }
    }
    Reply::ok(obj! { "success" => Value::Arr(success), "failed" => Value::Arr(failed) })
}

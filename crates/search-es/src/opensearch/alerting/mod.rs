//! OpenSearch's Alerting plugin (`_plugins/_alerting`, with its `_opendistro/_alerting`
//! aliases): monitors, their runs and the alerts they raise, as OpenSearch 3.9.0 answers them
//! (`tests/fixtures/rest/os_alerting.json`).
//!
//! **Storage, as the plugin keeps it.** A monitor is the document `{"monitor": …}` of the hidden
//! `.opendistro-alerting-config` index (its `_version`, `_seq_no` and `_primary_term` are that
//! document's, and `_search` searches the index). Alerts are documents of
//! `.opendistro-alerting-alerts` routed by their monitor's id; an alert that completes, or whose
//! monitor is deleted, moves to `.opendistro-alerting-alert-history-<yyyy.MM.dd>-1` through the
//! write alias `.opendistro-alerting-alert-history-write`. The mappings are the plugin's.
//!
//! **Running.** What the scheduler needs lives in the cluster metadata, one record per monitor
//! (family [`JOBS`]): when the monitor next runs, the user who stored it, and a document-level
//! monitor's per-shard sequence numbers. The cluster's job scheduler
//! ([`crate::opensearch::jobs`]) runs due monitors on the cluster's owner, sleeps until the next
//! one and wakes a sleeping cluster for it, so monitors run on one node only and survive unloads
//! and takeovers. [`run`] runs the three monitor types (query, bucket and document level), their
//! painless triggers and their actions, and keeps the alerts' states (`ACTIVE`, `ACKNOWLEDGED`,
//! `COMPLETED`, `ERROR`, `DELETED`) and throttles as the plugin does.
//!
//! **Security.** A monitor runs its searches as the user who stored it, at that user's rights at
//! each run ([`crate::xpack::internal::principal_refusal`] first, then
//! [`crate::xpack::internal::as_principal`]: document and field level security and masking
//! apply). The user is kept in the job record, never in the tenant-writable config index.
//! Actions send through the Notifications plugin's channels ([`super::notifications`]).
//!
//! **Not yet:** workflows (composite monitors), cluster-metrics and PPL monitors, findings
//! (`_plugins/_alerting/findings`), comments, the legacy destinations API, and bucket-level
//! `action_execution_policy` scopes other than per alert.

pub mod alerts;
pub mod run;
#[cfg(test)]
mod tests;

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::Api;
use crate::xpack::{internal, store};

pub const CONFIG_INDEX: &str = ".opendistro-alerting-config";
pub const ALERTS_INDEX: &str = ".opendistro-alerting-alerts";
pub const HISTORY_WRITE: &str = ".opendistro-alerting-alert-history-write";
/// Every alert index (active and history): what `GET …/monitors/alerts` reads.
pub const ALL_ALERTS: &str = ".opendistro-alerting-alert*";
/// One record per monitor: `next_run`, `user`, `run_context`.
pub const JOBS: &str = "alerting.job";

const CONFIG_MAPPING: &str = include_str!("config_mapping.json");
const ALERT_MAPPING: &str = include_str!("alert_mapping.json");

pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::AlertingCreateMonitor
            | Api::AlertingUpdateMonitor
            | Api::AlertingGetMonitor
            | Api::AlertingDeleteMonitor
            | Api::AlertingSearchMonitors
            | Api::AlertingExecuteMonitor
            | Api::AlertingGetAlerts
            | Api::AlertingAcknowledgeAlerts
    )
}

pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    match ctx.api {
        Api::AlertingCreateMonitor => put(ctx, None).await,
        Api::AlertingUpdateMonitor => put(ctx, ctx.path_param("monitor_id").map(str::to_string)).await,
        Api::AlertingGetMonitor => get(ctx).await,
        Api::AlertingDeleteMonitor => delete(ctx).await,
        Api::AlertingSearchMonitors => search(ctx).await,
        Api::AlertingExecuteMonitor => run::execute(ctx).await,
        Api::AlertingGetAlerts => alerts::get(ctx).await,
        Api::AlertingAcknowledgeAlerts => alerts::acknowledge(ctx).await,
        other => Err(EsError::not_implemented(other.name())),
    }
}

// ------------------------------------------------------------------------------ errors

/// The plugin's `AlertingException` around a Java exception (`class`: its name).
pub(crate) fn alerting(status: u16, class: &str, msg: impl Into<String>) -> EsError {
    let msg = msg.into();
    EsError::new(status, "alerting_exception", msg.clone())
        .caused_by(EsError::new(status, "exception", format!("{class}: {msg}")))
        .with_root_causes(vec![EsError::new(status, "alerting_exception", msg)])
}

fn iae(msg: impl Into<String>) -> EsError {
    alerting(400, "java.lang.IllegalArgumentException", msg)
}

pub(crate) fn status_exc(status: u16, msg: impl Into<String>) -> EsError {
    alerting(status, "org.opensearch.OpenSearchStatusException", msg)
}

// ------------------------------------------------------------------------------ indices

fn mapping(text: &str) -> Map {
    crate::json::parse(text.as_bytes()).ok().flatten().and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn create(st: &mut ClusterState, name: &str, mapping_text: &str, alias: Option<&str>, now: i64) -> EsResult<()> {
    if st.metadata.indices.contains_key(name) {
        return Ok(());
    }
    let mut req = crate::api::indices::CreateRequest::default();
    for (k, v) in [("index.hidden", "true"), ("index.number_of_shards", "1"), ("index.auto_expand_replicas", "0-1")] {
        req.settings.set(k, v);
    }
    req.mappings = Some(mapping(mapping_text));
    if let Some(a) = alias {
        req.aliases.push((a.to_string(), crate::meta::AliasMeta { is_hidden: Some(true), is_write_index: Some(true), ..Default::default() }));
    }
    crate::api::indices::create_index(st, name, name, req, now).map(|_| ())
}

/// Creates the config index (and with it the alerts index) if missing.
pub(crate) async fn ensure_indices(cluster: &Cluster, now: i64) -> EsResult<()> {
    let st = cluster.state();
    if st.metadata.indices.contains_key(CONFIG_INDEX) && st.metadata.indices.contains_key(ALERTS_INDEX) {
        return Ok(());
    }
    drop(st);
    cluster
        .update(move |st| {
            create(st, CONFIG_INDEX, CONFIG_MAPPING, None, now)?;
            create(st, ALERTS_INDEX, ALERT_MAPPING, None, now)
        })
        .await
}

/// Creates today's history index behind the write alias if no index has it.
pub(crate) async fn ensure_history(cluster: &Cluster, now: i64) -> EsResult<()> {
    if cluster.state().metadata.indices.values().any(|i| i.aliases.contains_key(HISTORY_WRITE)) {
        return Ok(());
    }
    let day = chrono::DateTime::from_timestamp_millis(now).map(|d| d.format("%Y.%m.%d").to_string()).unwrap_or_default();
    let name = format!(".opendistro-alerting-alert-history-{day}-1");
    cluster.update(move |st| create(st, &name, ALERT_MAPPING, Some(HISTORY_WRITE), now)).await
}

// ------------------------------------------------------------------------------ monitors

/// A stored monitor: its document's metadata and body (`user` included).
pub(crate) struct Stored {
    pub id: String,
    pub version: i64,
    pub seq_no: i64,
    pub primary_term: i64,
    pub monitor: Map,
}

/// Reads a monitor's document (`None`: no such monitor).
pub(crate) async fn read(cluster: &Cluster, id: &str) -> EsResult<Option<Stored>> {
    if id.is_empty() || !cluster.state().metadata.indices.contains_key(CONFIG_INDEX) {
        return Ok(None);
    }
    let r = match internal::call_system(cluster, "GET", &format!("/{CONFIG_INDEX}/_doc/{}", crate::xpack::enc(id)), None).await {
        Ok(r) => r,
        Err(e) if e.status == 404 => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some(Value::Obj(m)) = r.pointer(&["_source", "monitor"]).cloned() else { return Ok(None) };
    let n = |k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0);
    Ok(Some(Stored { id: id.to_string(), version: n("_version"), seq_no: n("_seq_no"), primary_term: n("_primary_term"), monitor: m }))
}

/// A monitor as the API prints it (its `user` is never shown).
pub(crate) fn shown(monitor: &Map) -> Value {
    let mut m = monitor.clone();
    m.shift_remove("user");
    Value::Obj(m)
}

/// `ChronoUnit` names, with their length in milliseconds where a period can use them.
fn unit_ms(unit: &str) -> Option<Option<i64>> {
    Some(match unit {
        "NANOS" | "MICROS" => Some(1),
        "MILLIS" => Some(1),
        "SECONDS" => Some(1_000),
        "MINUTES" => Some(60_000),
        "HOURS" => Some(3_600_000),
        "HALF_DAYS" => Some(43_200_000),
        "DAYS" => Some(86_400_000),
        "WEEKS" => Some(604_800_000),
        "MONTHS" | "YEARS" | "DECADES" | "CENTURIES" | "MILLENNIA" | "ERAS" | "FOREVER" => None,
        _ => return None,
    })
}

/// A schedule as the job scheduler parses it (`ScheduleParser`).
fn schedule(v: Option<&Value>) -> EsResult<Value> {
    let Some(Value::Obj(s)) = v else { return Err(iae("Monitor schedule is null")) };
    if let Some(Value::Obj(p)) = s.get("period") {
        let interval = p.get("interval").and_then(Value::as_i64).ok_or_else(|| iae("Interval is required"))?;
        if interval <= 0 {
            return Err(iae("Interval must be a positive integer"));
        }
        let unit = p.get("unit").map(Value::to_es_string).unwrap_or_else(|| "MINUTES".into());
        if unit_ms(&unit).is_none() {
            return Err(iae(format!("No enum constant java.time.temporal.ChronoUnit.{unit}")));
        }
        return Ok(obj! { "period" => obj! { "interval" => interval, "unit" => unit } });
    }
    if let Some(Value::Obj(c)) = s.get("cron") {
        let expr = c.get("expression").map(Value::to_es_string).unwrap_or_default();
        let tz = c.get("timezone").map(Value::to_es_string).unwrap_or_else(|| "UTC".into());
        if super::cron::next_after(&expr, &tz, 0).is_none() {
            return Err(iae(format!("Invalid cron expression [{expr}]")));
        }
        return Ok(obj! { "cron" => obj! { "expression" => expr, "timezone" => tz } });
    }
    Err(iae("Invalid schedule type"))
}

/// When a schedule next fires after `after` (a period counts from `enabled_time`).
pub(crate) fn next_run(schedule: &Value, enabled_time: i64, after: i64) -> Option<i64> {
    if let Some(p) = schedule.get("period") {
        let ms = unit_ms(&p.get("unit")?.to_es_string())??.saturating_mul(p.get("interval")?.as_i64()?);
        if ms <= 0 {
            return None;
        }
        let k = (after - enabled_time).div_euclid(ms) + 1;
        return Some(enabled_time + k.max(1) * ms);
    }
    let c = schedule.get("cron")?;
    super::cron::next_after(&c.get("expression")?.to_es_string(), &c.get("timezone").map(Value::to_es_string).unwrap_or_else(|| "UTC".into()), after)
}

/// A period's length (the cron's gap around `now`), for `period_start`.
pub(crate) fn period_ms(schedule: &Value, now: i64) -> i64 {
    if let Some(p) = schedule.get("period") {
        let unit = p.get("unit").map(Value::to_es_string).unwrap_or_default();
        return unit_ms(&unit).flatten().unwrap_or(60_000).saturating_mul(p.get("interval").and_then(Value::as_i64).unwrap_or(1));
    }
    match (next_run(schedule, 0, now), next_run(schedule, 0, now).and_then(|n| next_run(schedule, 0, n))) {
        (Some(a), Some(b)) => b - a,
        _ => 60_000,
    }
}

fn script(v: Option<&Value>, lang: &str) -> Value {
    match v {
        Some(Value::Obj(s)) => {
            let mut m = Map::new();
            m.insert("source".into(), s.get("source").or_else(|| s.get("inline")).cloned().unwrap_or(Value::from("")));
            m.insert("lang".into(), s.get("lang").cloned().unwrap_or(Value::from(lang)));
            if let Some(p) = s.get("params") {
                m.insert("params".into(), p.clone());
            }
            Value::Obj(m)
        }
        Some(Value::Str(s)) => obj! { "source" => s.clone(), "lang" => lang },
        _ => obj! { "source" => "", "lang" => lang },
    }
}

fn action(a: &Value) -> EsResult<Value> {
    let Value::Obj(a) = a else { return Err(iae("Action must be an object")) };
    let mut m = Map::new();
    m.insert("id".into(), a.get("id").map(Value::to_es_string).unwrap_or_else(crate::meta::names::time_based_id).into());
    m.insert("name".into(), a.get("name").cloned().ok_or_else(|| iae("Action name is null"))?);
    if let Some(d) = a.get("destination_id") {
        m.insert("destination_id".into(), d.clone());
    }
    m.insert("message_template".into(), script(a.get("message_template"), "mustache"));
    m.insert("throttle_enabled".into(), a.get("throttle_enabled").and_then(Value::as_bool).unwrap_or(false).into());
    if a.contains_key("subject_template") {
        m.insert("subject_template".into(), script(a.get("subject_template"), "mustache"));
    }
    if let Some(Value::Obj(t)) = a.get("throttle") {
        m.insert("throttle".into(), obj! { "value" => t.get("value").cloned().unwrap_or(Value::from(10)), "unit" => t.get("unit").cloned().unwrap_or(Value::from("MINUTES")) });
    }
    if let Some(p) = a.get("action_execution_policy") {
        m.insert("action_execution_policy".into(), p.clone());
    }
    Ok(Value::Obj(m))
}

const TRIGGER_TYPES: [&str; 3] = ["query_level_trigger", "bucket_level_trigger", "document_level_trigger"];

fn trigger(t: &Value) -> EsResult<Value> {
    let Some((kind, Value::Obj(body))) = t.as_object().and_then(|m| m.iter().next()) else { return Err(iae("Trigger must be an object")) };
    if !TRIGGER_TYPES.contains(&kind.as_str()) {
        return Err(iae(format!("Unknown trigger type [{kind}]")));
    }
    let mut m = Map::new();
    m.insert("id".into(), body.get("id").map(Value::to_es_string).unwrap_or_else(crate::meta::names::time_based_id).into());
    m.insert("name".into(), body.get("name").cloned().ok_or_else(|| iae("Trigger name is null"))?);
    m.insert("severity".into(), Value::from(body.get("severity").map(Value::to_es_string).unwrap_or_else(|| "1".into())));
    let cond = body.get("condition");
    let condition = if kind == "bucket_level_trigger" {
        let mut c = Map::new();
        let paths: Map = cond.and_then(|c| c.get("buckets_path")).and_then(Value::as_object).cloned().unwrap_or_default();
        c.insert("buckets_path".into(), Value::Obj(crate::json::java_hash_map(paths, 16)));
        c.insert("parent_bucket_path".into(), cond.and_then(|c| c.get("parent_bucket_path")).cloned().unwrap_or(Value::Null));
        c.insert("script".into(), script(cond.and_then(|c| c.get("script")), "painless"));
        c.insert("gap_policy".into(), cond.and_then(|c| c.get("gap_policy")).map(|g| Value::from(g.to_es_string().to_ascii_lowercase())).unwrap_or(Value::from("skip")));
        Value::Obj(c)
    } else {
        obj! { "script" => script(cond.and_then(|c| c.get("script")), "painless") }
    };
    m.insert("condition".into(), condition);
    let actions = body.get("actions").and_then(Value::as_array).map(|a| a.iter().map(action).collect::<EsResult<Vec<_>>>()).transpose()?.unwrap_or_default();
    m.insert("actions".into(), Value::Arr(actions));
    Ok(obj! { kind.clone() => Value::Obj(m) })
}

/// The keys `SearchSourceBuilder.toXContent` prints, in its order.
const SOURCE_ORDER: [&str; 22] = [
    "from", "size", "timeout", "terminate_after", "query", "post_filter", "min_score", "version", "seq_no_primary_term", "explain", "_source", "stored_fields",
    "docvalue_fields", "script_fields", "sort", "track_scores", "track_total_hits", "search_after", "aggregations", "highlight", "suggest", "collapse",
];

/// Aggregations as `AggregatorFactories.Builder` prints them: `aggs` named `aggregations`, a
/// composite's defaults.
fn aggregations(v: &Value) -> Value {
    let Value::Obj(aggs) = v else { return v.clone() };
    let mut out = Map::new();
    for (name, body) in aggs {
        let Value::Obj(b) = body else {
            out.insert(name.clone(), body.clone());
            continue;
        };
        let mut nb = Map::new();
        let mut subs = None;
        for (k, x) in b {
            match k.as_str() {
                "aggs" | "aggregations" => subs = Some(aggregations(x)),
                "composite" => {
                    let mut c = Map::new();
                    let src = x.as_object().cloned().unwrap_or_default();
                    c.insert("size".into(), src.get("size").cloned().unwrap_or(Value::from(10)));
                    let sources: Vec<Value> = src
                        .get("sources")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|s| {
                            let Some((sname, Value::Obj(sb))) = s.as_object().and_then(|m| m.iter().next()) else { return s.clone() };
                            let Some((ty, Value::Obj(tb))) = sb.iter().next() else { return s.clone() };
                            let mut t = tb.clone();
                            t.entry("missing_bucket".to_string()).or_insert(Value::Bool(false));
                            t.entry("order".to_string()).or_insert(Value::from("asc"));
                            obj! { sname.clone() => obj! { ty.clone() => Value::Obj(t) } }
                        })
                        .collect();
                    c.insert("sources".into(), Value::Arr(sources));
                    for (ck, cv) in &src {
                        if !matches!(ck.as_str(), "size" | "sources") {
                            c.insert(ck.clone(), cv.clone());
                        }
                    }
                    nb.insert(k.clone(), Value::Obj(c));
                }
                _ => {
                    nb.insert(k.clone(), x.clone());
                }
            }
        }
        if let Some(s) = subs {
            nb.insert("aggregations".into(), s);
        }
        out.insert(name.clone(), Value::Obj(nb));
    }
    Value::Obj(out)
}

/// A search input's request as the plugin stores it (`SearchSourceBuilder` round trip): its
/// query parsed and printed in OpenSearch's form.
fn search_source(v: Option<&Value>) -> EsResult<Value> {
    let src = v.and_then(Value::as_object).cloned().unwrap_or_default();
    let mut out = Map::new();
    for k in SOURCE_ORDER {
        let key = if k == "aggregations" && !src.contains_key("aggregations") { "aggs" } else { k };
        let Some(x) = src.get(key) else { continue };
        let printed = match k {
            "query" | "post_filter" => {
                let (q, _) = crate::query::parse_query(x, None, &[])?;
                q.to_value_in(crate::cluster::Dialect::OpenSearch)
            }
            "aggregations" => aggregations(x),
            _ => x.clone(),
        };
        out.insert(k.into(), printed);
    }
    for (k, x) in &src {
        if !SOURCE_ORDER.contains(&k.as_str()) && k != "aggs" {
            out.insert(k.clone(), x.clone());
        }
    }
    Ok(Value::Obj(out))
}

fn input(v: &Value) -> EsResult<Value> {
    if let Some(Value::Obj(s)) = v.get("search") {
        let indices: Vec<Value> = s.get("indices").and_then(Value::as_array).cloned().unwrap_or_default();
        return Ok(obj! { "search" => obj! { "indices" => Value::Arr(indices), "query" => search_source(s.get("query"))? } });
    }
    if let Some(Value::Obj(d)) = v.get("doc_level_input") {
        let mut m = Map::new();
        m.insert("description".into(), d.get("description").cloned().unwrap_or(Value::from("")));
        m.insert("indices".into(), d.get("indices").cloned().unwrap_or(Value::Arr(vec![])));
        let mut queries = Vec::new();
        for q in d.get("queries").and_then(Value::as_array).into_iter().flatten() {
            let mut qm = Map::new();
            qm.insert("id".into(), q.get("id").map(Value::to_es_string).unwrap_or_else(crate::meta::names::time_based_id).into());
            qm.insert("name".into(), q.get("name").cloned().unwrap_or(Value::from("")));
            qm.insert("fields".into(), q.get("fields").cloned().unwrap_or(Value::Arr(vec![])));
            qm.insert("query".into(), q.get("query").cloned().ok_or_else(|| iae("Query is null"))?);
            qm.insert("tags".into(), q.get("tags").cloned().unwrap_or(Value::Arr(vec![])));
            qm.insert("query_field_names".into(), q.get("query_field_names").cloned().unwrap_or(Value::Arr(vec![])));
            queries.push(Value::Obj(qm));
        }
        m.insert("queries".into(), Value::Arr(queries));
        m.insert("fan_out_enabled".into(), d.get("fan_out_enabled").cloned().unwrap_or(Value::Bool(true)));
        return Ok(obj! { "doc_level_input" => Value::Obj(m) });
    }
    Err(iae("Invalid input type"))
}

fn data_sources() -> Value {
    obj! {
        "query_index" => ".opensearch-alerting-queries",
        "findings_index" => ".opensearch-alerting-finding-history-write",
        "findings_index_pattern" => "<.opensearch-alerting-finding-history-{now/d}-1>",
        "alerts_index" => ALERTS_INDEX,
        "alerts_history_index" => HISTORY_WRITE,
        "alerts_history_index_pattern" => "<.opendistro-alerting-alert-history-{now/d}-1>",
        "comments_index" => ".opensearch-alerting-comments-history-write",
        "comments_index_pattern" => "<.opensearch-alerting-comments-history-{now/d}-1>",
        "query_index_mappings_by_type" => obj! {},
        "findings_enabled" => false,
    }
}

/// The `user` the plugin keeps with a monitor and its alerts (`User.toXContent`): empty for a
/// superuser, as without security.
pub(crate) fn user_doc(user: Option<&Value>) -> Value {
    let name = user.and_then(|u| u.get("username")).map(Value::to_es_string).unwrap_or_default();
    let roles = user.and_then(|u| u.get("roles")).cloned().unwrap_or(Value::Arr(vec![]));
    obj! { "name" => name, "backend_roles" => Value::Arr(vec![]), "roles" => roles, "user_requested_tenant" => Value::Null, "user_requested_tenant_access" => Value::Null, "custom_attribute_names" => Value::Arr(vec![]) }
}

/// Parses a monitor request body into the stored form (`Monitor.parse`, then the transport
/// action's defaults). `previous`: the monitor an update replaces.
pub(crate) fn parse_monitor(body: &Map, previous: Option<&Map>, user: Option<&Value>, now: i64) -> EsResult<Map> {
    let name = body.get("name").filter(|v| !v.is_null()).ok_or_else(|| iae("Monitor name is null"))?;
    let sched = schedule(body.get("schedule"))?;
    let monitor_type = body.get("monitor_type").map(Value::to_es_string).unwrap_or_else(|| "query_level_monitor".into());
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let enabled_time = match (enabled, previous.and_then(|p| p.get("enabled_time")).and_then(Value::as_i64)) {
        (false, _) => Value::Null,
        (true, Some(t)) if previous.and_then(|p| p.get("enabled")).and_then(Value::as_bool) == Some(true) => Value::from(t),
        (true, _) => Value::from(now),
    };
    let inputs = body.get("inputs").and_then(Value::as_array).map(|a| a.iter().map(input).collect::<EsResult<Vec<_>>>()).transpose()?.unwrap_or_default();
    let triggers = body.get("triggers").and_then(Value::as_array).map(|a| a.iter().map(trigger).collect::<EsResult<Vec<_>>>()).transpose()?.unwrap_or_default();
    let mut m = Map::new();
    m.insert("type".into(), "monitor".into());
    // The plugin prints 0 for a monitor it creates and its schema version for one it updates.
    m.insert("schema_version".into(), if previous.is_some() { 9 } else { 0 }.into());
    m.insert("name".into(), name.clone());
    m.insert("monitor_type".into(), monitor_type.into());
    m.insert("user".into(), user_doc(user));
    m.insert("enabled".into(), enabled.into());
    m.insert("enabled_time".into(), enabled_time);
    m.insert("schedule".into(), sched);
    m.insert("inputs".into(), Value::Arr(inputs));
    m.insert("triggers".into(), Value::Arr(triggers));
    m.insert("last_update_time".into(), now.into());
    m.insert("data_sources".into(), data_sources());
    m.insert("delete_query_index_in_every_run".into(), body.get("delete_query_index_in_every_run").cloned().unwrap_or(Value::Bool(false)));
    m.insert("should_create_single_alert_for_findings".into(), body.get("should_create_single_alert_for_findings").cloned().unwrap_or(Value::Bool(false)));
    m.insert("owner".into(), body.get("owner").cloned().unwrap_or(Value::from("alerting")));
    Ok(m)
}

/// The job record's `next_run` for a stored monitor.
fn job_next(monitor: &Map, now: i64) -> Option<i64> {
    if monitor.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let enabled_time = monitor.get("enabled_time").and_then(Value::as_i64).unwrap_or(now);
    next_run(monitor.get("schedule")?, enabled_time, now)
}

async fn put(ctx: &Ctx<'_>, update: Option<String>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let now = ctx.now_ms;
    let previous = match &update {
        Some(id) => Some(read(ctx.cluster, id).await?.ok_or_else(|| status_exc(404, format!("Monitor with {id} is not found")))?),
        None => None,
    };
    let user = internal::stored_user(crate::xpack::security::principal(ctx));
    let monitor = parse_monitor(&body, previous.as_ref().map(|p| &p.monitor), user.as_ref(), now)?;
    ensure_indices(ctx.cluster, now).await?;
    let doc = obj! { "monitor" => Value::Obj(monitor.clone()) };
    let r = match &update {
        Some(id) => {
            let mut uri = format!("/{CONFIG_INDEX}/_doc/{}?refresh=true", crate::xpack::enc(id));
            for k in ["if_seq_no", "if_primary_term"] {
                if let Some(v) = ctx.param(k) {
                    uri.push_str(&format!("&{k}={}", crate::xpack::enc(v)));
                }
            }
            internal::call_system(ctx.cluster, "PUT", &uri, Some(&doc)).await?
        }
        None => internal::call_system(ctx.cluster, "POST", &format!("/{CONFIG_INDEX}/_doc?refresh=true"), Some(&doc)).await?,
    };
    let id = r.get("_id").map(Value::to_es_string).unwrap_or_default();
    let next = job_next(&monitor, now);
    let job_id = id.clone();
    let doc_level = monitor.get("monitor_type").and_then(Value::as_str) == Some("doc_level_monitor");
    // A document-level monitor reads the documents written after it was stored.
    let context = if doc_level { Some(run::current_context(ctx.cluster, &monitor)) } else { None };
    ctx.cluster
        .update(move |st| {
            let mut rec = st.metadata.xpack.get(JOBS, &job_id).and_then(Value::as_object).cloned().unwrap_or_default();
            rec.shift_remove("next_run");
            if let Some(n) = next {
                rec.insert("next_run".into(), n.into());
            }
            match user {
                Some(u) => rec.insert("user".into(), u),
                None => rec.shift_remove("user"),
            };
            if let Some(c) = context
                && !rec.contains_key("run_context")
            {
                rec.insert("run_context".into(), c);
            }
            store::put(st, JOBS, &job_id, Value::Obj(rec), now).map(|_| ())
        })
        .await?;
    let n = |k: &str| r.get(k).cloned().unwrap_or(Value::from(0));
    Reply::ok(obj! { "_id" => id, "_version" => n("_version"), "_seq_no" => n("_seq_no"), "_primary_term" => n("_primary_term"), "monitor" => shown(&monitor) })
}

async fn get(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("monitor_id").unwrap_or_default();
    let Some(s) = read(ctx.cluster, id).await? else { return Err(status_exc(404, "Monitor not found.")) };
    Reply::ok(obj! {
        "_id" => s.id, "_version" => s.version, "_seq_no" => s.seq_no, "_primary_term" => s.primary_term,
        "monitor" => shown(&s.monitor), "associated_workflows" => Value::Arr(vec![]),
    })
}

async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("monitor_id").unwrap_or_default().to_string();
    if read(ctx.cluster, &id).await?.is_none() {
        return Err(EsError::new(404, "status_exception", format!("Monitor with {id} is not found")));
    }
    let r = internal::call_system(ctx.cluster, "DELETE", &format!("/{CONFIG_INDEX}/_doc/{}?refresh=true", crate::xpack::enc(&id)), None).await?;
    let now = ctx.now_ms;
    let job = id.clone();
    ctx.cluster
        .update(move |st| {
            store::remove(st, JOBS, &job);
            let _ = now;
            Ok(())
        })
        .await?;
    // Its alerts move to the history as `DELETED`.
    alerts::retire_monitor(ctx.cluster, &id, now).await?;
    Reply::ok(obj! { "_id" => id, "_version" => r.get("_version").cloned().unwrap_or(Value::from(1)) })
}

/// `_search`: the request's query over the config index's monitors (never its other documents),
/// each hit's `_source` the monitor as `GET` prints it.
async fn search(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let mut body = ctx.body_map()?.unwrap_or_default();
    let query = body.shift_remove("query").unwrap_or_else(|| obj! { "match_all" => obj! {} });
    body.shift_remove("_source");
    body.insert("query".into(), obj! { "bool" => obj! { "must" => crate::arr![query], "filter" => crate::arr![obj! { "exists" => obj! { "field" => "monitor" } }] } });
    body.insert("seq_no_primary_term".into(), Value::Bool(true));
    body.insert("version".into(), Value::Bool(true));
    if !ctx.state().metadata.indices.contains_key(CONFIG_INDEX) {
        ensure_indices(ctx.cluster, ctx.now_ms).await?;
    }
    let mut r = internal::call_system(ctx.cluster, "POST", &format!("/{CONFIG_INDEX}/_search"), Some(&Value::Obj(body))).await?;
    if let Some(Value::Arr(hits)) = r.as_object_mut().and_then(|m| m.get_mut("hits")).and_then(Value::as_object_mut).and_then(|h| h.get_mut("hits")) {
        for h in hits.iter_mut() {
            let Value::Obj(hm) = h else { continue };
            let monitor = hm.get("_source").and_then(|s| s.get("monitor")).and_then(Value::as_object).cloned().unwrap_or_default();
            let mut out = Map::new();
            for k in ["_index", "_id", "_version", "_seq_no", "_primary_term", "_score"] {
                if let Some(v) = hm.get(k) {
                    out.insert(k.into(), v.clone());
                }
            }
            out.insert("_source".into(), shown(&monitor));
            for (k, v) in hm.iter() {
                if !out.contains_key(k) && k != "_source" {
                    out.insert(k.clone(), v.clone());
                }
            }
            *hm = out;
        }
    }
    Reply::ok(r)
}

// ------------------------------------------------------------------------------ scheduling

/// When the next enabled monitor runs (the cluster's job scheduler sleeps until then).
pub fn next_due(st: &ClusterState) -> Option<i64> {
    st.metadata.xpack.iter(JOBS).filter_map(|(_, j)| j.get("next_run").and_then(Value::as_i64)).min()
}

/// Runs every monitor that is due (the job scheduler's turn). Returns how many ran.
pub async fn run_due(cluster: &Cluster, now: i64) -> usize {
    let due: Vec<(String, i64)> = cluster.state().metadata.xpack.iter(JOBS).filter_map(|(id, j)| j.get("next_run").and_then(Value::as_i64).filter(|t| *t <= now).map(|t| (id.clone(), t))).collect();
    for (id, at) in &due {
        run::scheduled(cluster, id, *at, now).await;
    }
    due.len()
}

/// Moves a monitor's `next_run` past a run that was due at `due` (unless an update moved it).
pub(crate) async fn advance(cluster: &Cluster, id: &str, due: i64, next: Option<i64>, now: i64) -> EsResult<()> {
    let id = id.to_string();
    cluster
        .update(move |st| {
            let Some(Value::Obj(mut rec)) = st.metadata.xpack.get(JOBS, &id).cloned() else { return Ok(()) };
            if rec.get("next_run").and_then(Value::as_i64) != Some(due) {
                return Ok(());
            }
            rec.shift_remove("next_run");
            if let Some(n) = next {
                rec.insert("next_run".into(), n.into());
            }
            store::put(st, JOBS, &id, Value::Obj(rec), now).map(|_| ())
        })
        .await
}

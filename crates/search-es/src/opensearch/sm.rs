//! OpenSearch's snapshot management (`_plugins/_sm/policies/*`): policies that take snapshots on
//! a cron schedule and delete them past a condition, as the index-management plugin's `SM`
//! does. Checked against OpenSearch 3.9.0 (`tests/fixtures/rest/os_sm.json`).
//!
//! The plugin keeps a policy as the document `<name>-sm-policy` of `.opendistro-ism-config`
//! (the index ISM's policies live in, sharing its sequence numbers) and its state machine's
//! metadata beside it. roll keeps both in the cluster metadata ([`crate::xpack::store`] families
//! `sm.policy` and `sm.meta`), creates the hidden config index on the first write and advances
//! ISM's counter ([`super::ism::next_seq`]) for every write the plugin makes there.
//!
//! **Running.** The plugin's job runs each enabled policy every minute from its
//! `schedule.interval.start_time`; roll's job scheduler wakes for those ticks only when one has
//! something to do ([`next_due`], driven from [`crate::xpack::slm`]: one scheduler for SLM and
//! SM). Each tick steps the creation and deletion workflows (`SMState`):
//!
//! - creation: `CREATION_START` until its cron trigger is due, then the snapshot
//!   (`<name>-<date_format in timezone, lowercased>-<8 random characters>`, `metadata.sm_policy`)
//!   is started and the workflow is `CREATING` until a later tick sees it finished;
//! - deletion: `DELETION_START` until due, then the policy's snapshots past its `condition`
//!   (`max_count`, or older than `max_age` beyond `min_count`) are deleted and the workflow is
//!   `DELETING` until a later tick sees them gone (`DELETING` with nothing to delete waits for
//!   the next trigger).
//!
//! Snapshots are taken and deleted through roll's own `_snapshot` API as the user who stored
//! the policy, as SLM's are ([`crate::xpack::internal::as_principal`]). A step that fails is
//! retried on the next three ticks (`RETRYING`), then recorded `FAILED`. `notification` and
//! `time_limit` are stored and returned; roll sends no notifications.

use super::ism;
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::Api;
use crate::xpack::{enc, internal, store};

pub const POLICIES: &str = "sm.policy";
/// Each policy's state machine metadata (`SMMetadata`), by policy name.
const META: &str = "sm.meta";
/// `IndexManagementIndices`' config mapping schema version.
const SCHEMA_VERSION: i64 = 31;
/// The plugin's job interval.
const TICK_MS: i64 = 60_000;
/// How many times a failing step is retried (`SMMetadata.Retry`).
const RETRIES: i64 = 3;
/// The snapshot metadata field naming the policy.
const POLICY_FIELD: &str = "sm_policy";

/// Whether this module handles an API.
pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::SmCreatePolicy | Api::SmUpdatePolicy | Api::SmGetPolicy | Api::SmGetPolicies | Api::SmDeletePolicy | Api::SmExplainPolicy | Api::SmStartPolicy | Api::SmStopPolicy
    )
}

pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    match ctx.api {
        Api::SmCreatePolicy => put(ctx, false).await,
        Api::SmUpdatePolicy => put(ctx, true).await,
        Api::SmGetPolicy => get(ctx),
        Api::SmGetPolicies => list(ctx),
        Api::SmDeletePolicy => delete(ctx).await,
        Api::SmExplainPolicy => explain(ctx),
        Api::SmStartPolicy => set_enabled(ctx, true).await,
        Api::SmStopPolicy => set_enabled(ctx, false).await,
        other => Err(EsError::not_implemented(other.name())),
    }
}

// ------------------------------------------------------------------------------ errors

fn iae(msg: impl Into<String>) -> EsError {
    EsError::java(400, "illegal_argument_exception", msg)
}

fn status(code: u16, msg: impl Into<String>) -> EsError {
    EsError::new(code, "status_exception", msg)
}

fn not_found() -> EsError {
    status(404, "Snapshot management policy not found")
}

fn config_missing() -> EsError {
    status(404, "Snapshot management config index not found")
}

fn doc_id(name: &str) -> String {
    format!("{name}-sm-policy")
}

// ------------------------------------------------------------------------------ parsing

/// `SMPolicy.parse` with `SMPolicy`'s `init` checks, in their order: the stored form (what GET
/// prints, without the fields roll sets: `schedule`, `enabled`, the times).
fn parse_policy(name: &str, body: &Map) -> EsResult<Map> {
    let mut out = Map::new();
    out.insert("name".into(), Value::from(name));
    if let Some(d) = body.get("description").filter(|v| !v.is_null()) {
        out.insert("description".into(), Value::from(d.to_es_string()));
    }
    out.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
    let creation = body.get("creation").filter(|v| !v.is_null()).map(parse_creation).transpose()?;
    let deletion = match body.get("deletion").filter(|v| !v.is_null()) {
        Some(d) => Some(parse_deletion(d, creation.as_ref())?),
        None => None,
    };
    if creation.is_none() && deletion.is_none() {
        return Err(iae("Must provide either creation or deletion configuration."));
    }
    let Some(Value::Obj(config)) = body.get("snapshot_config").filter(|v| !v.is_null()) else {
        return Err(iae("snapshot_config field must not be null"));
    };
    if !config.get("repository").is_some_and(|r| !r.is_null()) {
        return Err(iae("Must provide the repository in snapshot config."));
    }
    let notification = match body.get("notification").filter(|v| !v.is_null()) {
        Some(n) if n.get("channel").is_none_or(Value::is_null) => return Err(iae("Snapshot Management notification channel must not be null")),
        other => other.cloned(),
    };
    if let Some(c) = creation {
        out.insert("creation".into(), Value::Obj(c));
    }
    if let Some(d) = deletion {
        out.insert("deletion".into(), Value::Obj(d));
    }
    // A map as the plugin reads it (`xcp.map()`), printed from a `HashMap`.
    out.insert("snapshot_config".into(), Value::Obj(crate::json::java_hash_map(config.clone(), 16)));
    if let Some(n) = notification {
        out.insert("notification".into(), n);
    }
    Ok(out)
}

/// A `schedule` (`{"cron": {"expression", "timezone"}}`, the only kind the mapping allows).
fn parse_schedule(v: Option<&Value>, field: &str) -> EsResult<Value> {
    let Some(Value::Obj(s)) = v.filter(|v| !v.is_null()) else { return Err(iae("schedule field must not be null")) };
    if let Some(k) = s.keys().find(|k| *k != "cron") {
        return Err(EsError::new(500, "status_exception", format!("mapping set to strict, dynamic introduction of [{k}] within [sm_policy.{field}.schedule] is not allowed")));
    }
    let cron = s.get("cron").and_then(Value::as_object).cloned().unwrap_or_default();
    let expression = cron.get("expression").map(Value::to_es_string).unwrap_or_default();
    let Some(zone) = cron.get("timezone").filter(|v| !v.is_null()).map(Value::to_es_string) else {
        return Err(EsError::new(500, "null_pointer_exception", "zone"));
    };
    check_cron(&expression)?;
    let id = crate::query::term_level::java_zone_id(&zone).map_err(|(kind, msg)| EsError::new(500, kind, msg))?;
    crate::time::Zone::parse(&id).map_err(|msg| EsError::new(500, "zone_rules_exception", msg))?;
    Ok(obj! { "cron" => obj! { "expression" => expression, "timezone" => zone } })
}

/// cron-utils' UNIX definition, as the job scheduler's `CronSchedule` parses it.
fn check_cron(expr: &str) -> EsResult<()> {
    let parts = expr.split_whitespace().count();
    if parts != 5 {
        return Err(iae(format!("Cron expression contains {parts} parts but we expect one of [5]")));
    }
    const RANGES: [(i64, i64); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    for (field, (lo, hi)) in expr.split_whitespace().zip(RANGES) {
        for n in field.split([',', '-', '/']).filter_map(|t| t.parse::<i64>().ok()) {
            if n < lo || n > hi {
                let why = format!("Value {n} not in range [{lo}, {hi}]");
                return Err(iae(format!("Failed to parse cron expression. {why}")).caused_by(iae(why)));
            }
        }
    }
    if super::cron::next_after(expr, "UTC", 0).is_none() {
        return Err(iae(format!("Failed to parse cron expression. Invalid expression [{expr}]")));
    }
    Ok(())
}

fn parse_time(v: &Value, setting: &str) -> EsResult<String> {
    let s = v.to_es_string();
    crate::units::TimeValue::parse(&s, setting)?;
    Ok(s)
}

fn parse_creation(v: &Value) -> EsResult<Map> {
    let mut out = Map::new();
    out.insert("schedule".into(), parse_schedule(v.get("schedule"), "creation")?);
    if let Some(t) = v.get("time_limit").filter(|v| !v.is_null()) {
        out.insert("time_limit".into(), Value::from(parse_time(t, "time_limit")?));
    }
    Ok(out)
}

/// `SMPolicy.Deletion`: its schedule defaults to the creation's; `min_count` to 1.
fn parse_deletion(v: &Value, creation: Option<&Map>) -> EsResult<Map> {
    let schedule = match v.get("schedule").filter(|v| !v.is_null()) {
        Some(s) => parse_schedule(Some(s), "deletion")?,
        None => match creation.and_then(|c| c.get("schedule")) {
            Some(s) => s.clone(),
            None => parse_schedule(None, "deletion")?,
        },
    };
    let Some(Value::Obj(c)) = v.get("condition").filter(|v| !v.is_null()) else { return Err(iae("condition must not be null.")) };
    let mut cond = Map::new();
    if let Some(a) = c.get("max_age").filter(|v| !v.is_null()) {
        cond.insert("max_age".into(), Value::from(parse_time(a, "max_age")?));
    }
    let min = c.get("min_count").and_then(Value::as_i64).unwrap_or(1);
    if min < 1 {
        return Err(iae("min_count should be bigger than 0."));
    }
    cond.insert("min_count".into(), Value::from(min));
    if let Some(max) = c.get("max_count").and_then(Value::as_i64) {
        if max <= min {
            return Err(iae("max_count should be bigger than min_count."));
        }
        cond.insert("max_count".into(), Value::from(max));
    }
    let mut out = Map::new();
    out.insert("schedule".into(), schedule);
    out.insert("condition".into(), Value::Obj(cond));
    if let Some(t) = v.get("time_limit").filter(|v| !v.is_null()) {
        out.insert("time_limit".into(), Value::from(parse_time(t, "time_limit")?));
    }
    Ok(out)
}

/// The request's body as an object (`request body is required` without one).
fn body_of(ctx: &Ctx) -> EsResult<Map> {
    match ctx.body()? {
        None => Err(EsError::new(400, "parse_exception", "request body is required")),
        Some(Value::Obj(m)) => Ok(m),
        Some(other) => {
            let found = if matches!(other, Value::Arr(_)) { "START_ARRAY" } else { "VALUE_STRING" };
            Err(EsError::new(400, "parsing_exception", format!("Failed to parse object: expecting token of type [START_OBJECT] but found [{found}]")).with_meta("line", 1).with_meta("col", 1))
        }
    }
}

// ------------------------------------------------------------------------------ the stored policy

/// A stored policy: its document metadata, its policy as printed, and the user it runs as.
struct Stored {
    seq_no: i64,
    version: i64,
    policy: Map,
}

fn stored(st: &ClusterState, name: &str) -> Option<Stored> {
    let rec = st.metadata.xpack.get(POLICIES, name)?;
    Some(Stored {
        seq_no: rec.get("seq_no").and_then(Value::as_i64).unwrap_or(0),
        version: rec.get("version").and_then(Value::as_i64).unwrap_or(1),
        policy: rec.get("policy").and_then(Value::as_object).cloned().unwrap_or_default(),
    })
}

/// The policy as the plugin prints it (`sm_policy`): its parsed fields, then the job's
/// `schedule`, `enabled` and times, then `notification`.
fn printed(p: &Map) -> Value {
    let mut out = Map::new();
    for (k, v) in p {
        if k != "notification" {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(n) = p.get("notification") {
        out.shift_remove("notification");
        out.insert("notification".into(), n.clone());
    }
    Value::Obj(out)
}

/// The stored form with the job fields set: `schedule.interval.start_time` and `enabled_time`
/// at `start` (`None`: disabled), `last_updated_time` now.
fn with_job(mut p: Map, start: i64, enabled: bool, now: i64) -> Map {
    let notification = p.shift_remove("notification");
    p.insert("schedule".into(), obj! { "interval" => obj! { "start_time" => start, "period" => 1, "unit" => "Minutes" } });
    p.insert("enabled".into(), Value::Bool(enabled));
    p.insert("last_updated_time".into(), Value::from(now));
    p.insert("enabled_time".into(), if enabled { Value::from(start) } else { Value::Null });
    if let Some(n) = notification {
        p.insert("notification".into(), n);
    }
    p
}

fn conflict(st: &ClusterState, reason: String) -> EsError {
    EsError::new(409, "version_conflict_engine_exception", reason).with_meta("index", ism::CONFIG_INDEX).with_meta("shard", "0").with_meta("index_uuid", ism::config_uuid(st))
}

fn doc(name: &str, version: Option<i64>, seq: i64, policy: &Map) -> Value {
    let mut m = Map::new();
    m.insert("_id".into(), Value::from(doc_id(name)));
    if let Some(v) = version {
        m.insert("_version".into(), Value::from(v));
    }
    m.insert("_seq_no".into(), Value::from(seq));
    m.insert("_primary_term".into(), Value::from(1));
    m.insert("sm_policy".into(), printed(policy));
    Value::Obj(m)
}

// ------------------------------------------------------------------------------ the API

/// `POST` creates (409 when it exists); `PUT` updates, only with `if_seq_no` and
/// `if_primary_term`.
async fn put(ctx: &Ctx<'_>, update: bool) -> EsResult<Reply> {
    let name = ctx.path_param("policy_name").unwrap_or_default().to_string();
    if name.chars().any(|c| c.is_uppercase()) {
        return Err(iae("Policy name must be lowercase."));
    }
    let if_seq = ctx.query("if_seq_no").map(|s| s.parse::<i64>().unwrap_or(-2));
    let if_term = ctx.query("if_primary_term").map(|s| s.parse::<i64>().unwrap_or(0));
    if update && (if_seq.is_none() || if_term.is_none()) {
        return Err(EsError::action_request_validation(&["Sequence number and primary term must be provided when updating a snapshot management policy".into()]));
    }
    let body = body_of(ctx)?;
    let parsed = parse_policy(&name, &body)?;
    let now = ctx.now_ms;
    let user = internal::stored_user(crate::xpack::security::principal(ctx));
    let n = name.clone();
    let (created, seq, version, policy) = ctx
        .cluster
        .update(move |st| {
            ism::ensure_config_index(st, now)?;
            let id = doc_id(&n);
            let existing = stored(st, &n);
            let version = match (&existing, if_seq, if_term) {
                (Some(e), Some(s), Some(t)) if s == e.seq_no && t == 1 => e.version + 1,
                (Some(e), Some(s), Some(t)) => {
                    return Err(conflict(st, format!("[{id}]: version conflict, required seqNo [{s}], primary term [{t}]. current document has seqNo [{}] and primary term [1]", e.seq_no)));
                }
                (Some(e), _, _) => return Err(conflict(st, format!("[{id}]: version conflict, document already exists (current version [{}])", e.version))),
                (None, Some(s), Some(t)) => return Err(conflict(st, format!("[{id}]: version conflict, required seqNo [{s}], primary term [{t}]. but no document was found"))),
                (None, _, _) => ism::tombstone(st, &id, now).map_or(1, |v| v + 1),
            };
            let policy = with_job(parsed, now, true, now);
            let seq = ism::next_seq(st);
            let mut rec = obj! { "seq_no" => seq, "version" => version, "policy" => Value::Obj(policy.clone()) };
            if let (Some(u), Value::Obj(m)) = (user, &mut rec) {
                m.insert("_roll".into(), obj! { "user" => u });
            }
            store::put(st, POLICIES, &n, rec, now)?;
            Ok((existing.is_none(), seq, version, policy))
        })
        .await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, doc(&name, Some(version), seq, &policy)))
}

fn get(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("policy_name").unwrap_or_default();
    let st = ctx.state();
    if !ism::config_exists(&st) {
        return Err(config_missing());
    }
    let s = stored(&st, name).ok_or_else(not_found)?;
    Reply::ok(doc(name, Some(s.version), s.seq_no, &s.policy))
}

/// `GET _plugins/_sm/policies`: `from`, `size`, `sortField` (`sm_policy.name`), `sortOrder`
/// and `queryString` (a name pattern).
fn list(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let from = ctx.query("from").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let size = ctx.query("size").and_then(|v| v.parse::<usize>().ok()).unwrap_or(20);
    let desc = ctx.query("sortOrder").is_some_and(|o| o.eq_ignore_ascii_case("desc"));
    let query = ctx.query("queryString").unwrap_or("*").trim().to_lowercase();
    let mut all: Vec<(String, Stored)> = st
        .metadata
        .xpack
        .iter(POLICIES)
        .filter(|(n, _)| query.is_empty() || query == "*" || crate::xcontent::filter::glob_match(&query, n) || n.contains(query.trim_matches('*')))
        .filter_map(|(n, _)| stored(&st, n).map(|s| (n.clone(), s)))
        .collect();
    all.sort_by(|a, b| if desc { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) });
    let total = all.len();
    let policies: Vec<Value> = all.into_iter().skip(from).take(size).map(|(n, s)| doc(&n, None, s.seq_no, &s.policy)).collect();
    Reply::ok(obj! { "policies" => Value::Arr(policies), "total_policies" => total as i64 })
}

async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("policy_name").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    let n = name.clone();
    let (seq, version) = ctx
        .cluster
        .update(move |st| {
            if !ism::config_exists(st) {
                return Err(config_missing());
            }
            let s = stored(st, &n).ok_or_else(not_found)?;
            store::remove(st, POLICIES, &n);
            store::remove(st, META, &n);
            ism::remember_deleted(st, &doc_id(&n), s.version + 1, now);
            Ok((ism::next_seq(st), s.version + 1))
        })
        .await?;
    Reply::ok(obj! {
        "_index" => ism::CONFIG_INDEX,
        "_id" => doc_id(&name),
        "_version" => version,
        "result" => "deleted",
        "forced_refresh" => true,
        "_shards" => obj! { "total" => 2, "successful" => 1, "failed" => 0 },
        "_seq_no" => seq,
        "_primary_term" => 1,
    })
}

/// `_start` / `_stop`: acknowledged whether or not the policy changes (an enabled policy
/// started again is left as it is).
async fn set_enabled(ctx: &Ctx<'_>, enabled: bool) -> EsResult<Reply> {
    let name = ctx.path_param("policy_name").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if !ism::config_exists(st) {
                return Err(config_missing());
            }
            let s = stored(st, &name).ok_or_else(not_found)?;
            if s.policy.get("enabled").and_then(Value::as_bool) == Some(enabled) {
                return Ok(());
            }
            let mut p = s.policy.clone();
            p.insert("enabled".into(), Value::Bool(enabled));
            p.insert("last_updated_time".into(), Value::from(now));
            p.insert("enabled_time".into(), if enabled { Value::from(now) } else { Value::Null });
            let seq = ism::next_seq(st);
            let mut rec = st.metadata.xpack.get(POLICIES, &name).and_then(Value::as_object).cloned().unwrap_or_default();
            rec.insert("seq_no".into(), Value::from(seq));
            rec.insert("version".into(), Value::from(s.version + 1));
            rec.insert("policy".into(), Value::Obj(p));
            store::put(st, POLICIES, &name, Value::Obj(rec), now)?;
            // A stopped policy's metadata says so at once; a started one's when its job next runs.
            if !enabled && let Some(Value::Obj(mut m)) = st.metadata.xpack.get(META, &name).cloned() {
                m.insert("enabled".into(), Value::Bool(false));
                store::put(st, META, &name, Value::Obj(m), now)?;
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `_explain`: each named policy (a list, with `*` patterns) by its document's order, with its
/// state machine once its job has run (before, its name and `enabled: true`).
fn explain(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    if !ism::config_exists(&st) {
        return Err(config_missing());
    }
    let wanted: Vec<String> = ctx.path_param("policy_name").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let mut found: Vec<(i64, String)> = st
        .metadata
        .xpack
        .iter(POLICIES)
        .filter(|(n, _)| wanted.iter().any(|w| if w.contains('*') { crate::xcontent::filter::glob_match(w, n) } else { w == *n }))
        .filter_map(|(n, _)| stored(&st, n).map(|s| (s.seq_no, n.clone())))
        .collect();
    found.sort();
    let policies: Vec<Value> = found
        .into_iter()
        .map(|(_, n)| match st.metadata.xpack.get(META, &n) {
            Some(Value::Obj(m)) => {
                let mut out = Map::new();
                out.insert("name".into(), Value::from(n.clone()));
                for k in ["deletion", "policy_seq_no", "policy_primary_term", "creation", "enabled"] {
                    if let Some(v) = m.get(k) {
                        out.insert(k.into(), v.clone());
                    }
                }
                Value::Obj(out)
            }
            _ => obj! { "name" => n, "enabled" => true },
        })
        .collect();
    Reply::ok(obj! { "policies" => Value::Arr(policies) })
}

// ------------------------------------------------------------------------------ running

fn num(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

/// The first job tick (`start + k * TICK_MS`, k ≥ 1) at or after `at`.
fn tick_at_or_after(start: i64, at: i64) -> i64 {
    let k = ((at - start).max(1) + TICK_MS - 1) / TICK_MS;
    start + k.max(1) * TICK_MS
}

/// The next cron time of a workflow's schedule strictly after `after`.
fn next_trigger(workflow: &Value, after: i64) -> Option<i64> {
    let cron = workflow.pointer(&["schedule", "cron"])?;
    let expr = cron.get("expression")?.as_str()?;
    let zone = crate::query::term_level::java_zone_id(cron.get("timezone")?.as_str()?).ok()?;
    super::cron::next_after(expr, &zone, after)
}

/// When a workflow next needs its job: at its trigger while waiting for it, at once while a
/// step is running or retrying.
fn workflow_need(m: &Value, key: &str) -> Option<i64> {
    let w = m.get(key)?;
    let state = w.get("current_state").and_then(Value::as_str).unwrap_or_default();
    // `DELETING` with nothing being deleted looks again at its trigger.
    let waiting = state == "DELETING" && w.get("started").and_then(Value::as_array).is_none_or(Vec::is_empty);
    if state.ends_with("_START") || waiting {
        w.pointer(&["trigger", "time"]).and_then(Value::as_i64)
    } else {
        Some(i64::MIN)
    }
}

/// When a policy's job next has something to do (`None`: disabled, or a branch's copy).
fn policy_next(st: &ClusterState, name: &str, rec: &Value) -> Option<i64> {
    if crate::branch::paused_policy(rec) {
        return None;
    }
    let p = rec.get("policy")?;
    if p.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let start = p.pointer(&["schedule", "interval", "start_time"]).and_then(Value::as_i64)?;
    let last = st.metadata.xpack.get(META, name).and_then(|m| num(m, "last_tick")).unwrap_or(start);
    let seq = num(rec, "seq_no");
    let need = match st.metadata.xpack.get(META, name) {
        // First run, or the first since the policy changed: the job (re)initializes.
        Some(m) if num(m, "policy_seq_no") == seq => [workflow_need(m, "creation"), workflow_need(m, "deletion")].into_iter().flatten().min()?,
        _ => i64::MIN,
    };
    Some(tick_at_or_after(start, need.max(last + 1)))
}

/// The earliest tick any policy's job has work at (what wakes a sleeping cluster).
pub fn next_due(st: &ClusterState) -> Option<i64> {
    st.metadata.xpack.iter(POLICIES).filter_map(|(n, rec)| policy_next(st, n, rec)).min()
}

/// Runs every policy whose job has work at a tick at or before `now`.
pub async fn run_due(cluster: &Cluster, now: i64) {
    let due: Vec<(String, i64)> = {
        let st = cluster.state();
        st.metadata.xpack.iter(POLICIES).filter_map(|(n, rec)| policy_next(&st, n, rec).filter(|t| *t <= now).map(|t| (n.clone(), t))).collect()
    };
    for (name, _) in due {
        step(cluster, &name, now).await;
    }
}

/// `[2026-11-26T16:16:42Z]`: the plugin's message timestamps.
fn stamp(ms: i64) -> String {
    let t = chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default();
    format!("[{}]", t.format("%Y-%m-%dT%H:%M:%SZ"))
}

fn execution(status: &str, start: i64, end: Option<i64>, message: String, cause: Option<String>) -> Value {
    let mut info = obj! { "message" => message };
    if let (Some(c), Value::Obj(m)) = (cause, &mut info) {
        m.insert("cause".into(), Value::from(c));
    }
    obj! { "status" => status, "start_time" => start, "end_time" => end.map_or(Value::Null, Value::from), "info" => info }
}

/// The snapshot's name: `<policy>-<date_format in timezone>-<8 random characters>`, lowercase.
fn snapshot_name(policy: &Value, now: i64) -> String {
    let config = policy.get("snapshot_config");
    let pattern = config.and_then(|c| c.get("date_format")).map(Value::to_es_string).unwrap_or_else(|| "yyyy-MM-dd'T'HH:mm:ss".into());
    let zone = config
        .and_then(|c| c.get("timezone"))
        .and_then(|z| crate::query::term_level::java_zone_id(&z.to_es_string()).ok())
        .and_then(|id| crate::time::Zone::parse(&id).ok())
        .unwrap_or(crate::time::Zone::UTC);
    let date = crate::time::DateFormatter::new(&pattern).map(|f| f.format(crate::time::Instant::from_millis(now), &zone)).unwrap_or_default();
    let suffix: String = crate::meta::names::random_uuid().to_lowercase().chars().filter(char::is_ascii_alphanumeric).take(8).collect();
    format!("{}-{date}-{suffix}", policy.get("name").map(Value::to_es_string).unwrap_or_default()).to_lowercase()
}

/// A request to roll's own API as the policy's user.
async fn call_as(cluster: &Cluster, owner: &Option<crate::xpack::security::Principal>, method: &str, uri: &str, body: Option<&Value>) -> EsResult<Value> {
    if let Some(e) = internal::principal_refusal(cluster, owner.as_ref()) {
        return Err(e);
    }
    internal::as_principal(owner.clone(), internal::call(cluster, method, uri, body)).await
}

/// The policy's snapshots in its repository, oldest first: `(name, start, state)`.
async fn policy_snapshots(cluster: &Cluster, owner: &Option<crate::xpack::security::Principal>, repo: &str, name: &str) -> EsResult<Vec<(String, i64, String)>> {
    let v = call_as(cluster, owner, "GET", &format!("/_snapshot/{}/_all", enc(repo)), None).await?;
    let mut out: Vec<(String, i64, String)> = v
        .get("snapshots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.pointer(&["metadata", POLICY_FIELD]).and_then(Value::as_str) == Some(name))
        .map(|s| (s.get("snapshot").map(Value::to_es_string).unwrap_or_default(), num(s, "start_time_in_millis").unwrap_or(0), s.get("state").map(Value::to_es_string).unwrap_or_default()))
        .collect();
    out.sort_by_key(|s| s.1);
    Ok(out)
}

/// `SMUtils.findSnapshotsToDelete`: past `max_count`, or older than `max_age` beyond the
/// `min_count` newest.
fn to_delete(condition: &Value, snaps: &[(String, i64, String)], now: i64) -> Vec<String> {
    let min = num(condition, "min_count").unwrap_or(1).max(0) as usize;
    let mut out: Vec<String> = Vec::new();
    if let Some(max) = num(condition, "max_count")
        && snaps.len() > max as usize
    {
        out.extend(snaps[..snaps.len() - max as usize].iter().map(|s| s.0.clone()));
    }
    if let Some(age) = condition.get("max_age").and_then(Value::as_str).and_then(|a| crate::units::TimeValue::parse(a, "max_age").ok()) {
        let keep_from = snaps.len().saturating_sub(min);
        for s in &snaps[..keep_from] {
            if now - s.1 > age.as_millis() && !out.contains(&s.0) {
                out.push(s.0.clone());
            }
        }
    }
    out
}

/// A step's failure: retried on the next ticks, then recorded `FAILED` and the workflow back
/// at its start.
fn failed(w: &mut Map, start_state: &str, at: i64, message: String, cause: String) {
    // Each attempt's execution starts at its own tick.
    let left = w.get("retry").and_then(|r| num(r, "count")).unwrap_or(RETRIES + 1) - 1;
    let start = at;
    if left > 0 {
        w.insert("latest_execution".into(), execution("RETRYING", start, None, message, Some(cause)));
        w.insert("retry".into(), obj! { "count" => left });
    } else {
        w.insert("latest_execution".into(), execution("FAILED", start, Some(at), message, Some(cause)));
        w.shift_remove("retry");
        w.insert("current_state".into(), Value::from(start_state));
    }
}

/// One job run of a policy at the tick `now` falls in.
async fn step(cluster: &Cluster, name: &str, now: i64) {
    let (rec, meta) = {
        let st = cluster.state();
        (st.metadata.xpack.get(POLICIES, name).cloned(), st.metadata.xpack.get(META, name).cloned())
    };
    let Some(rec) = rec else { return };
    let Some(policy) = rec.get("policy").cloned() else { return };
    let start = policy.pointer(&["schedule", "interval", "start_time"]).and_then(Value::as_i64).unwrap_or(now);
    let tick = start + ((now - start) / TICK_MS).max(1) * TICK_MS;
    let owner = rec.pointer(&["_roll", "user"]).and_then(internal::principal_of);
    let seq = num(&rec, "seq_no").unwrap_or(0);
    let repo = policy.pointer(&["snapshot_config", "repository"]).map(Value::to_es_string).unwrap_or_default();
    // First run (or the first since the policy changed): each workflow waits for its trigger;
    // what it last did stays.
    let mut m = match meta {
        Some(Value::Obj(m)) if m.get("policy_seq_no").and_then(Value::as_i64) == Some(seq) => m,
        old => {
            let old = old.and_then(|v| v.as_object().cloned()).unwrap_or_default();
            let mut m = Map::new();
            for (key, state) in [("deletion", "DELETION_START"), ("creation", "CREATION_START")] {
                if let Some(wf) = policy.get(key) {
                    let mut w = Map::new();
                    w.insert("current_state".into(), Value::from(state));
                    w.insert("trigger".into(), obj! { "time" => next_trigger(wf, tick).map_or(Value::Null, Value::from) });
                    if let Some(e) = old.get(key).and_then(|w| w.get("latest_execution")) {
                        w.insert("latest_execution".into(), e.clone());
                    }
                    m.insert(key.into(), Value::Obj(w));
                }
            }
            m.insert("policy_seq_no".into(), Value::from(seq));
            m.insert("policy_primary_term".into(), Value::from(1));
            m.insert("enabled".into(), Value::Bool(true));
            m.insert("last_tick".into(), Value::from(tick));
            save(cluster, name, m).await;
            return;
        }
    };
    m.insert("last_tick".into(), Value::from(tick));
    if let (Some(wf), Some(Value::Obj(mut w))) = (policy.get("creation"), m.get("creation").cloned()) {
        creation(cluster, &owner, &policy, wf, &repo, &mut w, tick).await;
        m.insert("creation".into(), canonical(w));
    }
    if let (Some(wf), Some(Value::Obj(mut w))) = (policy.get("deletion"), m.get("deletion").cloned()) {
        deletion(cluster, &owner, name, wf, &repo, &mut w, tick).await;
        m.insert("deletion".into(), canonical(w));
    }
    save(cluster, name, m).await;
}

/// A workflow's metadata in the plugin's field order (`SMMetadata.WorkflowMetadata`).
fn canonical(mut w: Map) -> Value {
    let mut out = Map::new();
    for k in ["current_state", "trigger", "started", "latest_execution", "retry"] {
        if let Some(v) = w.shift_remove(k) {
            out.insert(k.into(), v);
        }
    }
    out.extend(w);
    Value::Obj(out)
}

async fn creation(cluster: &Cluster, owner: &Option<crate::xpack::security::Principal>, policy: &Value, wf: &Value, repo: &str, w: &mut Map, tick: i64) {
    let state = w.get("current_state").and_then(Value::as_str).unwrap_or_default().to_string();
    let due = w.get("trigger").and_then(|t| num(t, "time")).is_some_and(|t| t <= tick);
    match state.as_str() {
        "CREATION_START" | "CREATION_CONDITION_MET" if due || state == "CREATION_CONDITION_MET" => {
            let name = snapshot_name(policy, tick);
            let mut body = Map::new();
            let config = policy.get("snapshot_config").and_then(Value::as_object).cloned().unwrap_or_default();
            for k in ["indices", "ignore_unavailable", "include_global_state", "partial"] {
                if let Some(v) = config.get(k) {
                    body.insert(k.into(), v.clone());
                }
            }
            let mut metadata = config.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
            metadata.insert(POLICY_FIELD.into(), policy.get("name").cloned().unwrap_or(Value::Null));
            body.insert("metadata".into(), Value::Obj(metadata));
            let uri = format!("/_snapshot/{}/{}?wait_for_completion=false", enc(repo), enc(&name));
            match call_as(cluster, owner, "PUT", &uri, Some(&Value::Obj(body))).await {
                Ok(_) => {
                    w.insert("current_state".into(), Value::from("CREATING"));
                    w.insert("trigger".into(), obj! { "time" => next_trigger(wf, tick).map_or(Value::Null, Value::from) });
                    w.insert("started".into(), Value::Arr(vec![Value::from(name.clone())]));
                    w.insert("latest_execution".into(), execution("IN_PROGRESS", tick, None, format!("{}: Snapshot {name} creation has been started and waiting for completion.", stamp(tick)), None));
                    w.shift_remove("retry");
                }
                Err(e) => {
                    // The trigger moves on once, when the condition is met; retries keep it.
                    if state == "CREATION_START" {
                        w.insert("trigger".into(), obj! { "time" => next_trigger(wf, tick).map_or(Value::Null, Value::from) });
                    }
                    w.insert("current_state".into(), Value::from("CREATION_CONDITION_MET"));
                    let message = format!("{}: Caught exception while getting snapshots to decide if snapshot has been created in previous execution period.", stamp(tick));
                    failed(w, "CREATION_START", tick, message, format!("{}: {}", stamp(tick), e.reason()));
                }
            }
        }
        "CREATING" => {
            let started = w.get("started").and_then(Value::as_array).and_then(|a| a.first()).map(Value::to_es_string).unwrap_or_default();
            let uri = format!("/_snapshot/{}/{}", enc(repo), enc(&started));
            let got = call_as(cluster, owner, "GET", &uri, None).await;
            let state = got.as_ref().ok().and_then(|v| v.pointer(&["snapshots"]).and_then(Value::as_array).and_then(|a| a.first()).and_then(|s| s.get("state")).map(Value::to_es_string));
            let begun = w.get("latest_execution").and_then(|e| num(e, "start_time")).unwrap_or(tick);
            match state.as_deref() {
                Some("SUCCESS") => {
                    w.insert("current_state".into(), Value::from("CREATION_START"));
                    w.insert("trigger".into(), obj! { "time" => next_trigger(wf, tick).map_or(Value::Null, Value::from) });
                    w.shift_remove("started");
                    w.insert("latest_execution".into(), execution("SUCCESS", begun, Some(tick), format!("{}: Snapshot {started} creation has finished successfully.", stamp(tick)), None));
                }
                Some("IN_PROGRESS") | Some("STARTED") => {}
                Some(other) => {
                    w.insert("current_state".into(), Value::from("CREATION_START"));
                    w.shift_remove("started");
                    w.insert("latest_execution".into(), execution("FAILED", begun, Some(tick), format!("{}: Snapshot {started} creation end with state {other}.", stamp(tick)), None));
                }
                None => {
                    let cause = got.err().map(|e| e.reason().to_string()).unwrap_or_else(|| format!("[{started}] missing"));
                    failed(w, "CREATION_START", tick, format!("{}: Caught exception while getting started creation snapshot.", stamp(tick)), format!("{}: {cause}", stamp(tick)));
                }
            }
        }
        _ => {}
    }
}

async fn deletion(cluster: &Cluster, owner: &Option<crate::xpack::security::Principal>, policy_name: &str, wf: &Value, repo: &str, w: &mut Map, tick: i64) {
    let state = w.get("current_state").and_then(Value::as_str).unwrap_or_default().to_string();
    let due = w.get("trigger").and_then(|t| num(t, "time")).is_some_and(|t| t <= tick);
    let started: Vec<String> = w.get("started").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
    let list = |names: &[String]| format!("[{}]", names.join(", "));
    if state == "DELETING" && !started.is_empty() {
        match policy_snapshots(cluster, owner, repo, policy_name).await {
            Ok(snaps) if started.iter().all(|s| !snaps.iter().any(|x| &x.0 == s)) => {
                let begun = w.get("latest_execution").and_then(|e| num(e, "start_time")).unwrap_or(tick);
                w.insert("current_state".into(), Value::from("DELETION_START"));
                w.insert("trigger".into(), obj! { "time" => next_trigger(wf, tick).map_or(Value::Null, Value::from) });
                w.shift_remove("started");
                w.insert("latest_execution".into(), execution("SUCCESS", begun, Some(tick), format!("{}: Snapshot(s) {} deletion has finished.", stamp(tick), list(&started)), None));
            }
            Ok(_) => {}
            Err(e) => failed(w, "DELETION_START", tick, format!("{}: Caught exception while getting snapshots to decide if deletion has finished.", stamp(tick)), format!("{}: {}", stamp(tick), e.reason())),
        }
        return;
    }
    if !(state == "DELETING" || state == "DELETION_CONDITION_MET" || (state == "DELETION_START" && due)) {
        return;
    }
    let condition = wf.get("condition").cloned().unwrap_or(Value::Null);
    match policy_snapshots(cluster, owner, repo, policy_name).await {
        Ok(snaps) => {
            let gone = to_delete(&condition, &snaps, tick);
            w.insert("trigger".into(), obj! { "time" => next_trigger(wf, tick).map_or(Value::Null, Value::from) });
            w.insert("current_state".into(), Value::from("DELETING"));
            if gone.is_empty() {
                return;
            }
            let uri = format!("/_snapshot/{}/{}", enc(repo), enc(&gone.join(",")));
            match call_as(cluster, owner, "DELETE", &uri, None).await {
                Ok(_) => {
                    w.insert("started".into(), Value::Arr(gone.iter().cloned().map(Value::from).collect()));
                    w.insert("latest_execution".into(), execution("IN_PROGRESS", tick, None, format!("{}: Snapshots {} deletion has been started and waiting for completion.", stamp(tick), list(&gone)), None));
                    w.shift_remove("retry");
                }
                Err(e) => {
                    w.insert("current_state".into(), Value::from("DELETION_CONDITION_MET"));
                    failed(w, "DELETION_START", tick, format!("{}: Caught exception while deleting snapshot.", stamp(tick)), format!("{}: {}", stamp(tick), e.reason()));
                }
            }
        }
        Err(e) => {
            w.insert("current_state".into(), Value::from("DELETION_CONDITION_MET"));
            w.insert("trigger".into(), obj! { "time" => next_trigger(wf, tick).map_or(Value::Null, Value::from) });
            failed(w, "DELETION_START", tick, format!("{}: Caught exception while getting snapshots to decide which snapshots to delete.", stamp(tick)), format!("{}: {}", stamp(tick), e.reason()));
        }
    }
}

/// Writes a policy's metadata (a document write of the plugin's: a sequence number), unless the
/// policy went meanwhile.
async fn save(cluster: &Cluster, name: &str, m: Map) {
    let name = name.to_string();
    let now = crate::time::now_millis();
    let _ = cluster
        .update(move |st| {
            if st.metadata.xpack.get(POLICIES, &name).is_none() {
                return Ok(());
            }
            ism::next_seq(st);
            store::put(st, META, &name, Value::Obj(m), now).map(|_| ())
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{ClusterConfig, MemoryStore};
    use crate::rest::EsRequest;
    use std::sync::Arc;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        let r = crate::rest::handle(c, req).await;
        (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or_default())
    }

    fn explained(v: &Value) -> Value {
        v.pointer(&["policies"]).and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or_default()
    }

    fn state_of(v: &Value, wf: &str) -> String {
        explained(v).pointer(&[wf, "current_state"]).map(Value::to_es_string).unwrap_or_default()
    }

    /// The job's runs, made by hand at the ticks the plugin's job would make them: a policy
    /// takes its snapshot when its trigger is due, sees it finished a tick later, and deletes
    /// the oldest once there are more than `max_count`, each step shown by `_explain`.
    #[tokio::test]
    async fn a_policy_creates_on_its_schedule_and_deletes_past_max_count() {
        let mut cfg = ClusterConfig::new("c");
        cfg.dialect = crate::cluster::Dialect::OpenSearch;
        let dir = std::env::temp_dir().join(format!("roll-sm-unit-{}", std::process::id()));
        cfg.node_settings.push(("path.repo".into(), dir.to_string_lossy().into_owned()));
        let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
        assert_eq!(call(&c, "PUT", "/_snapshot/r", r#"{"type":"fs","settings":{"location":"r"}}"#).await.0, 200);
        assert_eq!(call(&c, "PUT", "/i/_doc/1?refresh=true", r#"{"a":1}"#).await.0, 201);
        let every_minute = r#"{"schedule":{"cron":{"expression":"* * * * *","timezone":"UTC"}}}"#;
        let body = format!(r#"{{"creation":{every_minute},"deletion":{{"schedule":{{"cron":{{"expression":"* * * * *","timezone":"UTC"}}}},"condition":{{"max_count":2}}}},"snapshot_config":{{"indices":"i","repository":"r"}}}}"#);
        let (s, v) = call(&c, "POST", "/_plugins/_sm/policies/p", &body).await;
        assert_eq!(s, 201, "{v:?}");
        let start = v.pointer(&["sm_policy", "schedule", "interval", "start_time"]).and_then(Value::as_i64).unwrap();
        let explain = || call(&c, "GET", "/_plugins/_sm/policies/p/_explain", "");
        assert_eq!(explained(&explain().await.1), crate::obj! { "name" => "p", "enabled" => true }, "before the job runs");
        // Tick by tick: the job's first run sets the triggers; each later one steps both workflows.
        let mut snapshots = Vec::new();
        for k in 1..=9 {
            let at = start + k * TICK_MS;
            assert!(next_due(&c.state()).is_some_and(|t| t <= at), "tick {k} is due");
            run_due(&c, at).await;
            let (_, v) = explain().await;
            let creation = state_of(&v, "creation");
            if k == 1 {
                assert_eq!((creation.as_str(), state_of(&v, "deletion").as_str()), ("CREATION_START", "DELETION_START"), "{v:?}");
            }
            if creation == "CREATING" {
                let name = explained(&v).pointer(&["creation", "started"]).and_then(Value::as_array).and_then(|a| a.first()).map(Value::to_es_string).unwrap();
                assert!(name.starts_with("p-"), "{name}");
                snapshots.push(name);
            }
            let (_, all) = call(&c, "GET", "/_snapshot/r/_all", "").await;
            let count = all.get("snapshots").and_then(Value::as_array).map_or(0, Vec::len);
            assert!(count <= 3, "deletion keeps at most max_count plus the one being taken: {count} at tick {k}");
        }
        assert!(snapshots.len() >= 3, "{snapshots:?}");
        let (_, all) = call(&c, "GET", "/_snapshot/r/_all", "").await;
        let names: Vec<String> = all.get("snapshots").and_then(Value::as_array).into_iter().flatten().map(|s| s.get("snapshot").map(Value::to_es_string).unwrap_or_default()).collect();
        assert!(!names.contains(&snapshots[0]), "the oldest went: {names:?}");
        let (_, v) = explain().await;
        let finished = explained(&v).pointer(&["creation", "latest_execution", "status"]).map(Value::to_es_string);
        assert!(matches!(finished.as_deref(), Some("SUCCESS") | Some("IN_PROGRESS")), "{v:?}");
        let deleted = explained(&v).pointer(&["deletion", "latest_execution", "info", "message"]).map(Value::to_es_string).unwrap_or_default();
        assert!(deleted.contains("deletion has"), "{v:?}");
        // Stopped, the job has nothing to do.
        assert_eq!(call(&c, "POST", "/_plugins/_sm/policies/p/_stop", "").await.0, 200);
        assert_eq!(next_due(&c.state()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

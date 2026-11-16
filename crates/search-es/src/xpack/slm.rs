//! Snapshot lifecycle management: `_slm/policy` (put, get, delete), `_slm/policy/<id>/_execute`,
//! `_slm/_execute_retention`, `_slm/stats`, `_slm/status` and `_slm/start|stop`, run by the
//! cluster's job scheduler ([`crate::opensearch::jobs`]).
//!
//! A policy (`SnapshotLifecyclePolicy`) takes a snapshot of its `config` (indices, global
//! state, feature states, metadata, ...) into its repository on its `schedule`: a Quartz cron
//! expression in UTC, or an interval counted from the policy's last change. The snapshot is
//! named from the policy's `name` (date math resolved) plus a random suffix, carries
//! `metadata.policy`, and is taken through roll's own `_snapshot` API as the user who last put
//! the policy (Elasticsearch's stored security headers). Each run records `last_success` or
//! `last_failure` (with the error as JSON), the failures since the last success, the policy's
//! counters, and a `CREATE` item in the `.slm-history-7` data stream
//! (`slm.history_index_enabled`).
//!
//! Retention (`SnapshotRetentionTask`) runs on `slm.retention_schedule` (01:30 UTC daily) and
//! on `_slm/_execute_retention`: for every policy with a `retention`, the snapshots in its
//! repository whose `metadata.policy` names it are deleted when
//! `SnapshotRetentionConfiguration.isSnapshotEligibleForDeletion` says so (`expire_after`,
//! `min_count`, `max_count`, unsuccessful snapshots first), within `slm.retention_duration`,
//! each deletion counted and recorded as a `DELETE` history item.
//!
//! **When runs happen.** Nothing polls. A policy's next run is the first scheduled time after
//! the latest of its last change, its last run and the last `_slm/start` (missed runs while
//! SLM was stopped are skipped, as Elasticsearch reschedules on start); retention's is the
//! first `slm.retention_schedule` time after its last run (or the earliest change of a policy
//! with retention). [`next_due`] gives the earliest of these while SLM is `RUNNING`, so a
//! sleeping cluster is woken only for a due run or retention, and a cluster whose policies
//! have no retention is never woken for retention.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::TimeValue;
use crate::xpack::{enc, internal, store};
use std::sync::atomic::Ordering;

pub const POLICIES: &str = "slm.policy";
/// `mode` (the operation mode and when SLM last started), `stats` (`SnapshotLifecycleStats`)
/// and `retention` (when retention last ran).
const STATE: &str = "slm.state";
/// `SnapshotLifecycleTemplateRegistry`'s data stream and template.
const HISTORY: &str = ".slm-history-7";
/// `SnapshotLifecyclePolicy.POLICY_ID_METADATA_FIELD`.
const POLICY_FIELD: &str = "policy";
/// `SnapshotLifecyclePolicy.MAX_METADATA_BYTES`.
const MAX_METADATA_BYTES: usize = 1004;

fn num(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

pub fn mode(st: &ClusterState) -> String {
    st.metadata.xpack.get(STATE, "mode").and_then(|v| v.get("operation_mode")).and_then(Value::as_str).unwrap_or("RUNNING").to_string()
}

fn started_at(st: &ClusterState) -> i64 {
    st.metadata.xpack.get(STATE, "mode").and_then(|v| num(v, "started_at")).unwrap_or(0)
}

fn put_mode(st: &mut ClusterState, mode: &str, now: i64) -> EsResult<()> {
    let started = if mode == "RUNNING" { now } else { started_at(st) };
    store::put(st, STATE, "mode", obj! { "operation_mode" => mode, "started_at" => started }, now).map(|_| ())
}

fn setting<'a>(st: &'a ClusterState, key: &str, default: &'a str) -> &'a str {
    st.metadata.cluster_setting(key).unwrap_or(default)
}

/// A policy's schedule: a cron expression or an interval (`SnapshotLifecyclePolicy.schedule`).
enum Schedule<'a> {
    Cron(&'a str),
    Interval(i64),
}

fn schedule_of(s: &str) -> Option<Schedule<'_>> {
    match TimeValue::parse(s, "schedule") {
        Ok(t) if t.as_millis() > 0 => Some(Schedule::Interval(t.as_millis())),
        Ok(_) => None,
        Err(_) => crate::xpack::watcher::schedule::valid_cron(s).then_some(Schedule::Cron(s)),
    }
}

/// The first scheduled time after `after` (an interval counts from `start`, the policy's last
/// change).
fn next_after(s: &Schedule, start: i64, after: i64) -> Option<i64> {
    match s {
        Schedule::Cron(expr) => crate::xpack::watcher::schedule::next_cron(expr, &crate::time::Zone::UTC, after),
        Schedule::Interval(i) => Some(start + ((after - start).max(0) / i + 1) * i),
    }
}

fn policy_body(rec: &Value) -> &Value {
    static NULL: Value = Value::Null;
    rec.get("policy").unwrap_or(&NULL)
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// When a policy runs next: the first scheduled time after its last change, its last run and
/// the last start, whichever is latest.
fn policy_next(st: &ClusterState, rec: &Value) -> Option<i64> {
    let sched = schedule_of(str_of(policy_body(rec), "schedule"))?;
    let modified = num(rec, "modified_date").unwrap_or(0);
    let reference = modified.max(num(rec, "last_run").unwrap_or(0)).max(started_at(st));
    next_after(&sched, modified, reference)
}

/// `SnapshotRetentionConfiguration`: what a policy keeps.
#[derive(Clone, Copy, Default)]
struct Retention {
    expire_after: Option<i64>,
    min_count: Option<i64>,
    max_count: Option<i64>,
}

impl Retention {
    fn of(policy: &Value) -> Option<Retention> {
        let r = policy.get("retention")?;
        let expire_after = r.get("expire_after").and_then(Value::as_str).and_then(|s| TimeValue::parse(s, "expire_after").ok()).map(|t| t.as_millis());
        let out = Retention { expire_after, min_count: num(r, "min_count"), max_count: num(r, "max_count") };
        (out.expire_after.is_some() || out.min_count.is_some() || out.max_count.is_some()).then_some(out)
    }

    /// `isSnapshotEligibleForDeletion` for `snap` (`(start, successful?)`, by name) among
    /// `all`, the policy's snapshots in its repository.
    fn eligible(&self, name: &str, all: &[(String, i64, bool)], now: i64) -> bool {
        let Some(&(_, start, ok)) = all.iter().find(|(n, _, _)| n == name) else { return false };
        let mut sorted: Vec<&(String, i64, bool)> = all.iter().collect();
        sorted.sort_by_key(|s| s.1);
        let successful = all.iter().filter(|s| s.2).count() as i64;
        let newest_ok = all.iter().filter(|s| s.2).map(|s| s.1).max().unwrap_or(i64::MIN);
        // Without expire_after, an unsuccessful snapshot older than a successful one goes.
        if self.expire_after.is_none() && !ok && newest_ok > start {
            return true;
        }
        if let Some(max) = self.max_count
            && successful > max
        {
            let over = successful - max;
            let mut seen = 0;
            for s in &sorted {
                if s.2 {
                    seen += 1;
                }
                if seen > over {
                    break;
                }
                if s.0 == name {
                    return true;
                }
            }
        }
        if let Some(min) = self.min_count
            && successful <= min
            && ok
        {
            return false;
        }
        let Some(expire) = self.expire_after else { return false };
        if let Some(min) = self.min_count
            && ok
            && !sorted.iter().filter(|s| s.2).take((successful - min).max(0) as usize).any(|s| s.0 == name)
        {
            return false;
        }
        now - start > expire
    }
}

/// Policies with a retention, with their repositories.
fn retained(st: &ClusterState) -> Vec<(String, String, Retention, i64)> {
    st.metadata
        .xpack
        .iter(POLICIES)
        .filter_map(|(id, rec)| {
            let p = policy_body(rec);
            Retention::of(p).map(|r| (id.clone(), str_of(p, "repository").to_string(), r, num(rec, "modified_date").unwrap_or(0)))
        })
        .collect()
}

/// When retention runs next (only while a policy has a retention).
fn retention_next(st: &ClusterState) -> Option<i64> {
    let since = retained(st).iter().map(|r| r.3).min()?;
    let last = st.metadata.xpack.get(STATE, "retention").and_then(|v| num(v, "last_run")).unwrap_or(0);
    let schedule = setting(st, "slm.retention_schedule", "0 30 1 * * ?");
    crate::xpack::watcher::schedule::next_cron(schedule, &crate::time::Zone::UTC, since.max(last).max(started_at(st)))
}

/// The earliest due policy run or retention (Unix ms), while SLM is running: what wakes a
/// sleeping cluster.
pub fn next_due(st: &ClusterState) -> Option<i64> {
    if mode(st) != "RUNNING" || st.metadata.xpack.len(POLICIES) == 0 {
        return None;
    }
    st.metadata.xpack.iter(POLICIES).filter_map(|(_, rec)| policy_next(st, rec)).chain(retention_next(st)).min()
}

/// Runs every due policy, then retention when due (the job scheduler's turn).
pub async fn run_due(cluster: &Cluster, now: i64) {
    let due: Vec<String> = {
        let st = cluster.state();
        if mode(&st) != "RUNNING" {
            return;
        }
        st.metadata.xpack.iter(POLICIES).filter(|(_, rec)| policy_next(&st, rec).is_some_and(|t| t <= now)).map(|(id, _)| id.clone()).collect()
    };
    for id in due {
        let _ = execute(cluster, &id, now, true).await;
    }
    if retention_next(&cluster.state()).is_some_and(|t| t <= now) {
        retention(cluster, now).await;
    }
}

/// `SnapshotLifecyclePolicy.generateSnapshotName`: the name with date math resolved, plus a
/// random lowercase suffix.
fn snapshot_name(name: &str, now: i64) -> EsResult<String> {
    let base = crate::meta::resolve::resolve_date_math(name, now)?;
    Ok(format!("{base}-{}", crate::meta::names::random_uuid().to_lowercase()))
}

fn stats_mut(st: &mut ClusterState, now: i64, f: impl FnOnce(&mut Map)) -> EsResult<()> {
    let mut stats = st.metadata.xpack.get(STATE, "stats").and_then(Value::as_object).cloned().unwrap_or_default();
    f(&mut stats);
    store::put(st, STATE, "stats", Value::Obj(stats), now).map(|_| ())
}

fn bump(m: &mut Map, k: &str, by: i64) {
    let v = m.get(k).and_then(Value::as_i64).unwrap_or(0) + by;
    m.insert(k.into(), Value::from(v));
}

fn bump_policy(stats: &mut Map, id: &str, k: &str, by: i64) {
    let policies = stats.entry("policies".to_string()).or_insert_with(Value::object);
    if let Value::Obj(p) = policies
        && let Value::Obj(one) = p.entry(id.to_string()).or_insert_with(Value::object)
    {
        bump(one, k, by);
    }
}

/// `SnapshotLifecycleTask.maybeTakeSnapshot`: takes the policy's snapshot now and records the
/// outcome; a scheduled run also moves the policy's schedule on. Returns the snapshot's name
/// (`None`: the policy is gone).
async fn execute(cluster: &Cluster, id: &str, now: i64, scheduled: bool) -> Option<String> {
    let rec = cluster.state().metadata.xpack.get(POLICIES, id).cloned()?;
    let p = policy_body(&rec).clone();
    let owner = rec.pointer(&["_roll", "user"]).and_then(internal::principal_of);
    let repo = str_of(&p, "repository").to_string();
    let config = p.get("config").and_then(Value::as_object).cloned().unwrap_or_default();
    cluster.xpack.slm_running.fetch_add(1, Ordering::SeqCst);
    let name = snapshot_name(str_of(&p, "name"), now);
    let outcome: Result<(String, i64), (String, EsError)> = match name {
        Err(e) => Err((str_of(&p, "name").to_string(), e)),
        Ok(name) => {
            let mut body = config.clone();
            let mut metadata = body.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
            metadata.insert(POLICY_FIELD.into(), Value::from(id));
            body.insert("metadata".into(), Value::Obj(metadata));
            let uri = format!("/_snapshot/{}/{}?wait_for_completion=true", enc(&repo), enc(&name));
            match internal::as_principal(owner, internal::call(cluster, "PUT", &uri, Some(&Value::Obj(body)))).await {
                Ok(v) => {
                    let s = v.get("snapshot").cloned().unwrap_or(Value::Null);
                    let (failed, total) = (s.pointer(&["shards", "failed"]).and_then(Value::as_i64).unwrap_or(0), s.pointer(&["shards", "total"]).and_then(Value::as_i64).unwrap_or(0));
                    if str_of(&s, "state") == "SUCCESS" {
                        Ok((name, num(&s, "start_time_in_millis").unwrap_or(now)))
                    } else {
                        let msg = format!("failed to create snapshot successfully, {failed} out of {total} total shards failed");
                        Err((name, EsError::new(500, "snapshot_exception", format!("[{repo}:{}] {msg}", s.get("snapshot").map(Value::to_es_string).unwrap_or_default()))))
                    }
                }
                Err(e) => Err((name, e)),
            }
        }
    };
    let end = crate::time::now_millis();
    let (snap, details) = match &outcome {
        Ok((n, _)) => (n.clone(), None),
        Err((n, e)) => (n.clone(), Some(e.to_value(false).to_json())),
    };
    let id_owned = id.to_string();
    let outcome_rec = outcome.as_ref().map(|(_, start)| *start).map_err(|_| ());
    let details_rec = details.clone();
    let snap_rec = snap.clone();
    let _ = cluster
        .update(move |st| {
            let Some(Value::Obj(mut r)) = st.metadata.xpack.get(POLICIES, &id_owned).cloned() else { return Ok(()) };
            if scheduled {
                r.insert("last_run".into(), Value::from(now));
            }
            let since = r.get("invocations_since_last_success").and_then(Value::as_i64).unwrap_or(0);
            match outcome_rec {
                Ok(start) => {
                    r.insert("last_success".into(), obj! { "snapshot_name" => snap_rec, "start_time" => start, "time" => end });
                    r.insert("invocations_since_last_success".into(), Value::from(0));
                }
                Err(()) => {
                    r.insert("last_failure".into(), obj! { "snapshot_name" => snap_rec, "time" => end, "details" => details_rec.unwrap_or_default() });
                    r.insert("invocations_since_last_success".into(), Value::from(since + 1));
                }
            }
            store::put(st, POLICIES, &id_owned, Value::Obj(r), now)?;
            stats_mut(st, now, |s| bump_policy(s, &id_owned, if outcome_rec.is_ok() { "snapshots_taken" } else { "snapshots_failed" }, 1))
        })
        .await;
    let item = history_item(end, id, &repo, &snap, "CREATE", details.is_none(), Some(Value::Obj(config)), details);
    write_history(cluster, item).await;
    finish_run(cluster, now).await;
    Some(snap)
}

/// A run ended: a `STOPPING` SLM is `STOPPED` once nothing runs.
async fn finish_run(cluster: &Cluster, now: i64) {
    if cluster.xpack.slm_running.fetch_sub(1, Ordering::SeqCst) == 1 && mode(&cluster.state()) == "STOPPING" {
        let _ = cluster.update(|st| if mode(st) == "STOPPING" { put_mode(st, "STOPPED", now) } else { Ok(()) }).await;
    }
}

/// `SnapshotHistoryItem`.
#[allow(clippy::too_many_arguments)]
fn history_item(at: i64, policy: &str, repo: &str, snapshot: &str, op: &str, success: bool, config: Option<Value>, error: Option<String>) -> Value {
    let mut m = obj! { "@timestamp" => at, "policy" => policy, "repository" => repo, "snapshot_name" => snapshot, "operation" => op, "success" => success };
    if let Value::Obj(o) = &mut m {
        if let Some(c) = config {
            o.insert("configuration".into(), c);
        }
        if let Some(e) = error {
            o.insert("error_details".into(), Value::from(e));
        }
    }
    m
}

/// Appends a history item to `.slm-history-7` (its template installed on first use), unless
/// `slm.history_index_enabled` is off.
async fn write_history(cluster: &Cluster, item: Value) {
    let (enabled, templated) = {
        let st = cluster.state();
        (setting(&st, "slm.history_index_enabled", "true") != "false", st.metadata.index_templates.contains_key(HISTORY))
    };
    if !enabled {
        return;
    }
    if !templated {
        let template = obj! {
            "index_patterns" => vec![Value::from(format!("{HISTORY}*"))],
            "template" => obj! {
                "settings" => obj! { "index.number_of_shards" => 1, "index.auto_expand_replicas" => "0-1" },
                "mappings" => obj! { "dynamic" => false, "properties" => obj! {
                    "snapshot_name" => obj! { "type" => "keyword" },
                    "@timestamp" => obj! { "format" => "epoch_millis", "type" => "date" },
                    "configuration" => obj! { "dynamic" => false, "type" => "object", "properties" => obj! {
                        "indices" => obj! { "type" => "keyword" },
                        "include_global_state" => obj! { "type" => "boolean" },
                        "partial" => obj! { "type" => "boolean" },
                    } },
                    "error_details" => obj! { "index" => false, "type" => "text" },
                    "success" => obj! { "type" => "boolean" },
                    "repository" => obj! { "type" => "keyword" },
                    "operation" => obj! { "type" => "keyword" },
                    "policy" => obj! { "type" => "keyword" },
                } },
                "lifecycle" => obj! { "enabled" => true, "data_retention" => "90d" },
            },
            "priority" => 2147483647i64,
            "version" => 7,
            "_meta" => obj! { "managed" => true, "description" => "index template for SLM history indices" },
            "data_stream" => obj! { "hidden" => true, "allow_custom_routing" => false },
            "allow_auto_create" => true,
        };
        let _ = internal::call_system(cluster, "PUT", &format!("/_index_template/{}", enc(HISTORY)), Some(&template)).await;
    }
    let _ = internal::call_system(cluster, "POST", &format!("/{}/_doc?op_type=create", enc(HISTORY)), Some(&item)).await;
}

/// `SnapshotRetentionTask`: deletes what each policy's retention no longer keeps, within
/// `slm.retention_duration`.
async fn retention(cluster: &Cluster, now: i64) {
    let started = std::time::Instant::now();
    let (deletions, budget) = {
        let st = cluster.state();
        let mut out: Vec<(String, String, String)> = Vec::new();
        for (id, repo, keep, _) in retained(&st) {
            let Some(r) = st.metadata.repositories.get(&repo) else { continue };
            let Some(data) = st.metadata.repository_data.get(&r.location_key()) else { continue };
            let all: Vec<(String, i64, bool)> = data
                .snapshots
                .iter()
                .filter(|s| s.metadata.as_ref().and_then(|m| m.get(POLICY_FIELD)).and_then(Value::as_str) == Some(id.as_str()))
                .map(|s| (s.name.clone(), s.start_ms, s.state == "SUCCESS"))
                .collect();
            for (name, _, _) in &all {
                if keep.eligible(name, &all, now) {
                    out.push((id.clone(), repo.clone(), name.clone()));
                }
            }
        }
        let budget = TimeValue::parse(setting(&st, "slm.retention_duration", "1h"), "slm.retention_duration").map_or(3_600_000, |t| t.as_millis());
        (out, budget)
    };
    let mut counts: Vec<(String, bool)> = Vec::new();
    let mut timed_out = false;
    for (id, repo, name) in deletions {
        if budget >= 0 && started.elapsed().as_millis() as i64 > budget {
            timed_out = true;
            break;
        }
        let r = internal::call_system(cluster, "DELETE", &format!("/_snapshot/{}/{}", enc(&repo), enc(&name)), None).await;
        let error = r.as_ref().err().map(|e| e.to_value(false).to_json());
        counts.push((id.clone(), r.is_ok()));
        write_history(cluster, history_item(crate::time::now_millis(), &id, &repo, &name, "DELETE", error.is_none(), None, error)).await;
    }
    let elapsed = started.elapsed().as_millis() as i64;
    let _ = cluster
        .update(move |st| {
            store::put(st, STATE, "retention", obj! { "last_run" => now }, now)?;
            stats_mut(st, now, |s| {
                bump(s, "retention_runs", 1);
                bump(s, "retention_time_ms", elapsed);
                if timed_out {
                    bump(s, "retention_timed_out", 1);
                }
                for (id, ok) in &counts {
                    bump_policy(s, id, if *ok { "snapshots_deleted" } else { "snapshot_deletion_failures" }, 1);
                }
            })
        })
        .await;
}

/// What ILM's `wait_for_snapshot` sees of a policy (`WaitForSnapshotStep`).
pub(crate) enum Since {
    NoPolicy,
    /// No successful snapshot started at or after the time asked about.
    NotYet,
    /// The last successful snapshot doesn't include the index.
    Without,
    Taken,
}

/// Whether policy `id`'s last successful snapshot started at or after `since` and includes
/// `index`.
pub(crate) fn snapshot_since(st: &ClusterState, id: &str, index: &str, since: i64) -> Since {
    let Some(rec) = st.metadata.xpack.get(POLICIES, id) else { return Since::NoPolicy };
    let Some(last) = rec.get("last_success").filter(|l| num(l, "start_time").is_some_and(|t| t >= since)) else { return Since::NotYet };
    let name = str_of(last, "snapshot_name");
    let repo = str_of(policy_body(rec), "repository");
    let included = st
        .metadata
        .repositories
        .get(repo)
        .and_then(|r| st.metadata.repository_data.get(&r.location_key()))
        .and_then(|d| d.find(name))
        .is_some_and(|s| s.indices.iter().any(|i| i == index));
    if included { Since::Taken } else { Since::Without }
}

// ---- REST ----

/// `[snapshot_lifecycle] <field> doesn't support values of type: <token>`, at the value.
fn wrong_type(ctx: &Ctx, field: &str, v: &Value) -> EsError {
    let token = match v {
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
        Value::Str(_) => "VALUE_STRING",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Null => "VALUE_NULL",
        _ => "VALUE_BOOLEAN",
    };
    let (l, c) = crate::xpack::value_loc(ctx, field);
    EsError::x_content_parse(l, c, format!("[snapshot_lifecycle] {field} doesn't support values of type: {token}"))
}

/// Parses `snapshot_retention` (unknown fields are ignored, as Elasticsearch's parser does). Its
/// errors come as the `ConstructingObjectParser` building it reports them, at the object's end.
fn parse_retention(ctx: &Ctx, v: &Value) -> EsResult<Value> {
    let Some(m) = v.as_object() else { return Err(wrong_type(ctx, "retention", v)) };
    let built = |cause: EsError| {
        use crate::json::locate::{Seg, value_end};
        let (l, c) = ctx.raw_json().and_then(|src| value_end(src, &[Seg::Key("retention")])).unwrap_or((1, 1));
        EsError::x_content_parse(l, c, "[snapshot_lifecycle] failed to parse field [retention]")
            .caused_by(EsError::x_content_parse_bare("Failed to build [snapshot_retention] after last required field arrived").caused_by(cause))
    };
    let mut out = Map::new();
    let (mut min, mut max) = (None, None);
    for (k, v) in m {
        match k.as_str() {
            "expire_after" => {
                let s = v.to_es_string();
                TimeValue::parse(&s, "expire_after").map_err(built)?;
                out.insert("expire_after".into(), Value::from(s));
            }
            "min_count" | "max_count" => {
                let n = v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
                if k == "min_count" {
                    min = n;
                } else {
                    max = n;
                }
            }
            _ => {}
        }
    }
    if let Some(n) = min
        && n < 1
    {
        return Err(built(EsError::illegal_argument(format!("minimum snapshot count must be at least 1, but was: {n}"))));
    }
    if let Some(n) = max
        && n < 1
    {
        return Err(built(EsError::illegal_argument(format!("maximum snapshot count must be at least 1, but was: {n}"))));
    }
    if let (Some(a), Some(b)) = (min, max)
        && a > b
    {
        return Err(built(EsError::illegal_argument(format!("minimum snapshot count {a} cannot be larger than maximum snapshot count {b}"))));
    }
    if let Some(n) = min {
        out.insert("min_count".into(), Value::from(n));
    }
    if let Some(n) = max {
        out.insert("max_count".into(), Value::from(n));
    }
    Ok(Value::Obj(out))
}

/// `SnapshotLifecyclePolicy.PARSER` (unknown fields ignored), then `validate()`.
fn parse_policy(ctx: &Ctx, id: &str, body: &Map) -> EsResult<Value> {
    let (mut name, mut schedule, mut repository, mut config, mut retention) = (None, None, None, None, None);
    for (k, v) in body {
        match k.as_str() {
            "name" | "schedule" | "repository" => {
                let Value::Str(text) = v else { return Err(wrong_type(ctx, k, v)) };
                match k.as_str() {
                    "name" => name = Some(text.clone()),
                    "schedule" => schedule = Some(text.clone()),
                    _ => repository = Some(text.clone()),
                }
            }
            "config" => match v {
                Value::Obj(_) => config = Some(v.clone()),
                _ => return Err(wrong_type(ctx, k, v)),
            },
            "retention" => retention = Some(parse_retention(ctx, v)?),
            _ => {}
        }
    }
    let missing: Vec<&str> = [("name", name.is_none()), ("schedule", schedule.is_none()), ("repository", repository.is_none())].into_iter().filter(|(_, m)| *m).map(|(n, _)| n).collect();
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    let (name, schedule, repository) = (name.unwrap_or_default(), schedule.unwrap_or_default(), repository.unwrap_or_default());
    let mut errors = Vec::new();
    let invalid_chars = crate::api::snapshots::INVALID_CHARS;
    if id.contains(invalid_chars) {
        errors.push(format!("invalid policy id [{id}]: must not contain the following characters {}", crate::api::snapshots::INVALID_CHARS_TEXT));
    }
    if id.starts_with('_') {
        errors.push(format!("invalid policy id [{id}]: must not start with '_'"));
    }
    if id.len() > 255 {
        errors.push(format!("invalid policy id [{id}]: name is too long, ({} > 255)", id.len()));
    }
    if schedule.trim().is_empty() {
        errors.push(format!("invalid schedule [{schedule}]: must not be empty"));
    } else if TimeValue::parse(&schedule, "schedule").is_ok_and(|t| t.as_millis() == 0) {
        errors.push(format!("invalid schedule [{schedule}]: time unit must be at least 1 millisecond"));
    } else if schedule_of(&schedule).is_none() {
        errors.push(format!("invalid schedule [{schedule}]: must be a valid cron expression or time unit"));
    }
    // `validateGeneratedSnapshotName`, on a name generated now (bad date math fails outright).
    let generated = snapshot_name(&name, ctx.now_ms)?;
    if name.is_empty() {
        errors.push(format!("invalid snapshot name [{name}]: cannot be empty"));
    }
    if generated.contains('#') {
        errors.push(format!("invalid snapshot name [{name}]: must not contain '#'"));
    }
    if generated.starts_with('_') {
        errors.push(format!("invalid snapshot name [{name}]: must not start with '_'"));
    }
    if generated.to_lowercase() != generated {
        errors.push(format!("invalid snapshot name [{name}]: must be lowercase"));
    }
    if generated.contains(invalid_chars) {
        errors.push(format!("invalid snapshot name [{name}]: must not contain contain the following characters {}", crate::api::snapshots::INVALID_CHARS_TEXT));
    }
    if repository.trim().is_empty() {
        errors.push(format!("invalid repository name [{repository}]: cannot be empty"));
    }
    if let Some(meta) = config.as_ref().and_then(|c| c.get("metadata")) {
        match meta {
            Value::Obj(m) if m.contains_key(POLICY_FIELD) => {
                errors.push(format!("invalid configuration.metadata: field name [{POLICY_FIELD}] is reserved and will be added automatically"));
            }
            Value::Obj(_) => {
                let size = meta.to_json().len();
                if size > MAX_METADATA_BYTES {
                    errors.push(format!("invalid configuration.metadata: must be smaller than [{MAX_METADATA_BYTES}] bytes, but was [{size}] bytes"));
                }
            }
            other => errors.push(format!("invalid configuration.metadata [{}]: must be an object if present", other.to_es_string())),
        }
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let mut out = obj! { "name" => name, "schedule" => schedule, "repository" => repository };
    if let Value::Obj(m) = &mut out {
        if let Some(c) = config {
            m.insert("config".into(), c);
        }
        if let Some(r) = retention {
            m.insert("retention".into(), r);
        }
    }
    Ok(out)
}

/// `SnapshotLifecycleService.validateMinimumInterval`: two consecutive runs may not be closer
/// than `slm.minimum_interval`.
fn check_interval(st: &ClusterState, schedule: &str, now: i64) -> EsResult<()> {
    let min = setting(st, "slm.minimum_interval", "15m");
    let min_ms = TimeValue::parse(min, "slm.minimum_interval").map_or(0, |t| t.as_millis());
    let gap = match schedule_of(schedule) {
        Some(Schedule::Interval(i)) => i,
        Some(s @ Schedule::Cron(_)) => match next_after(&s, now, now).and_then(|a| next_after(&s, now, a).map(|b| b - a)) {
            Some(g) => g,
            None => return Ok(()),
        },
        None => return Ok(()),
    };
    if gap > 0 && min_ms > 0 && gap < min_ms {
        return Err(EsError::illegal_argument(format!("invalid schedule [{schedule}]: schedule would be too frequent, executing more than every [{min}]")));
    }
    Ok(())
}

/// `PUT _slm/policy/{policy_id}`.
pub async fn put_lifecycle(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("policy_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.ok_or_else(|| EsError::parse("request body is required"))?;
    let parsed = parse_policy(ctx, &id, &body)?;
    let now = ctx.now_ms;
    let user = internal::stored_user(crate::xpack::security::principal(ctx));
    ctx.cluster
        .update(move |st| {
            let repo = str_of(&parsed, "repository");
            if !st.metadata.repositories.contains_key(repo) {
                return Err(EsError::illegal_argument(format!("no such repository [{repo}]")));
            }
            check_interval(st, str_of(&parsed, "schedule"), now)?;
            // An update keeps the policy's history (`SnapshotLifecyclePolicyMetadata.builder(old)`).
            let mut rec = match st.metadata.xpack.get(POLICIES, &id) {
                Some(old) if old.get("policy") == Some(&parsed) => return Ok(()),
                Some(Value::Obj(old)) => {
                    let mut r = old.clone();
                    let v = old.get("version").and_then(Value::as_i64).unwrap_or(0) + 1;
                    r.insert("version".into(), Value::from(v));
                    r.shift_remove("last_run");
                    r
                }
                _ => {
                    let mut r = Map::new();
                    r.insert("version".into(), Value::from(1));
                    r
                }
            };
            rec.insert("modified_date".into(), Value::from(now));
            rec.insert("policy".into(), parsed);
            rec.shift_remove("_roll");
            if let Some(u) = user {
                rec.insert("_roll".into(), obj! { "user" => u });
            }
            store::put(st, POLICIES, &id, Value::Obj(rec), now)?;
            // `SnapshotLifecycleMetadata` exists from the first put; a policy's counters from its
            // first run.
            stats_mut(st, now, |_| {})
        })
        .await?;
    Reply::acknowledged()
}

fn time_field(m: &mut Map, raw: &str, readable: &str, ms: i64, human: bool) {
    if human {
        m.insert(readable.into(), Value::from(crate::time::java_instant_string(ms)));
    }
    m.insert(raw.into(), Value::from(ms));
}

/// `SnapshotInvocationRecord`.
fn invocation(v: &Value, human: bool) -> Value {
    let mut m = Map::new();
    m.insert("snapshot_name".into(), v.get("snapshot_name").cloned().unwrap_or(Value::Null));
    if let Some(s) = num(v, "start_time") {
        time_field(&mut m, "start_time", "start_time_string", s, human);
    }
    time_field(&mut m, "time", "time_string", num(v, "time").unwrap_or(0), human);
    if let Some(d) = v.get("details").filter(|d| !d.is_null()) {
        m.insert("details".into(), d.clone());
    }
    Value::Obj(m)
}

/// `SnapshotPolicyStats`.
fn policy_stats(st: &ClusterState, id: &str) -> Value {
    let s = st.metadata.xpack.get(STATE, "stats").and_then(|s| s.pointer(&["policies", id]));
    let n = |k: &str| s.and_then(|s| num(s, k)).unwrap_or(0);
    obj! {
        "policy" => id,
        "snapshots_taken" => n("snapshots_taken"),
        "snapshots_failed" => n("snapshots_failed"),
        "snapshots_deleted" => n("snapshots_deleted"),
        "snapshot_deletion_failures" => n("snapshot_deletion_failures"),
    }
}

/// `SnapshotLifecyclePolicyItem`.
fn item(ctx: &Ctx, st: &ClusterState, id: &str, rec: &Value) -> Value {
    let human = ctx.human;
    let mut m = Map::new();
    m.insert("version".into(), rec.get("version").cloned().unwrap_or(Value::from(1)));
    let modified = num(rec, "modified_date").unwrap_or(0);
    time_field(&mut m, "modified_date_millis", "modified_date", modified, human);
    m.insert("policy".into(), policy_body(rec).clone());
    for k in ["last_success", "last_failure"] {
        if let Some(v) = rec.get(k) {
            m.insert(k.into(), invocation(v, human));
        }
    }
    if let Some(next) = schedule_of(str_of(policy_body(rec), "schedule")).and_then(|s| next_after(&s, modified, ctx.now_ms.max(modified))) {
        time_field(&mut m, "next_execution_millis", "next_execution", next, human);
    }
    m.insert("stats".into(), policy_stats(st, id));
    Value::Obj(m)
}

/// `GET _slm/policy[/{policy_id}]`.
pub fn get_lifecycle(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let ids = ctx.list_param("policy_id").unwrap_or_default();
    let mut out = Map::new();
    for (id, rec) in st.metadata.xpack.iter(POLICIES) {
        if ids.is_empty() || ids.contains(id) {
            out.insert(id.clone(), item(ctx, &st, id, rec));
        }
    }
    if out.is_empty() && !ids.is_empty() {
        // "no policies are configured" until the first policy creates `SnapshotLifecycleMetadata`.
        let suffix = if usage(&st).is_none() { ", no policies are configured" } else { "" };
        return Err(EsError::resource_not_found(format!("snapshot lifecycle policy or policies [{}] not found{suffix}", ids.join(", "))));
    }
    Reply::ok(Value::Obj(out))
}

/// `DELETE _slm/policy/{policy_id}`: the policy and its counters.
pub async fn delete_lifecycle(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("policy_id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if store::remove(st, POLICIES, &id).is_none() {
                return Err(EsError::resource_not_found(format!("snapshot lifecycle policy not found: {id}")));
            }
            stats_mut(st, now, |s| {
                if let Some(Value::Obj(p)) = s.get_mut("policies") {
                    p.shift_remove(&id);
                }
            })
        })
        .await?;
    Reply::acknowledged()
}

/// `PUT _slm/policy/{policy_id}/_execute`: takes the policy's snapshot now (whatever SLM's
/// mode, as Elasticsearch does), without moving its schedule.
pub async fn execute_lifecycle(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("policy_id").unwrap_or_default().to_string();
    match execute(ctx.cluster, &id, ctx.now_ms, false).await {
        Some(name) => Reply::ok(obj! { "snapshot_name" => name }),
        None => Err(EsError::illegal_argument(format!("no such snapshot lifecycle policy [{id}]"))),
    }
}

/// `POST _slm/_execute_retention`: retention now (skipped unless SLM is running).
pub async fn execute_retention(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    if mode(&ctx.state()) == "RUNNING" {
        retention(ctx.cluster, ctx.now_ms).await;
    }
    Reply::acknowledged()
}

/// `GET _slm/stats`.
pub fn get_stats(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    Reply::ok(stats(&ctx.state()))
}

/// `_xpack/usage`'s `slm` beyond `available`/`enabled` (`SLMFeatureSetUsage`): once a policy
/// was ever put, the policy count and the stats.
pub fn usage(st: &ClusterState) -> Option<Value> {
    st.metadata.xpack.get(STATE, "stats")?;
    Some(obj! { "policy_count" => st.metadata.xpack.len(POLICIES), "policy_stats" => stats(st) })
}

/// `SnapshotLifecycleStats`.
fn stats(st: &ClusterState) -> Value {
    let s = st.metadata.xpack.get(STATE, "stats").cloned().unwrap_or_else(Value::object);
    let n = |k: &str| num(&s, k).unwrap_or(0);
    let mut ids: Vec<String> = s.get("policies").and_then(Value::as_object).map(|p| p.keys().cloned().collect()).unwrap_or_default();
    ids.sort();
    let policies: Vec<Value> = ids.iter().map(|id| policy_stats(st, id)).collect();
    let total = |k: &str| policies.iter().filter_map(|p| num(p, k)).sum::<i64>();
    let (taken, failed, deleted, deletion_failures) = (total("snapshots_taken"), total("snapshots_failed"), total("snapshots_deleted"), total("snapshot_deletion_failures"));
    let time = n("retention_time_ms");
    obj! {
        "retention_runs" => n("retention_runs"),
        "retention_failed" => n("retention_failed"),
        "retention_timed_out" => n("retention_timed_out"),
        "retention_deletion_time" => TimeValue::millis(time).human(),
        "retention_deletion_time_millis" => time,
        "total_snapshots_taken" => taken,
        "total_snapshots_failed" => failed,
        "total_snapshots_deleted" => deleted,
        "total_snapshot_deletion_failures" => deletion_failures,
        "policy_stats" => Value::Arr(policies),
    }
}

/// `GET _slm/status`.
pub fn get_status(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    Reply::ok(obj! { "operation_mode" => mode(&ctx.state()) })
}

/// `POST _slm/start|stop` (`OperationModeUpdateTask.slmMode`): stopping goes through
/// `STOPPING` while a policy's snapshot is being taken.
pub async fn set_mode(ctx: &Ctx<'_>, running: bool) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let now = ctx.now_ms;
    let busy = ctx.cluster.xpack.slm_running.load(Ordering::SeqCst) > 0;
    ctx.cluster
        .update(move |st| {
            let next = match (running, mode(st).as_str()) {
                (true, "RUNNING") | (false, "STOPPED") => return Ok(()),
                (true, _) => "RUNNING",
                (false, _) if busy => "STOPPING",
                (false, _) => "STOPPED",
            };
            put_mode(st, next, now)
        })
        .await?;
    Reply::acknowledged()
}

/// `SlmHealthIndicatorService`.
pub fn health(st: &ClusterState, verbose: bool) -> Value {
    let policies = st.metadata.xpack.len(POLICIES);
    let mode = mode(st);
    let threshold: i64 = setting(st, "slm.health.failed_snapshot_warn_threshold", "5").parse().unwrap_or(5);
    let unhealthy: Vec<(&String, &Value)> = st
        .metadata
        .xpack
        .iter(POLICIES)
        .filter(|(_, r)| num(r, "invocations_since_last_success").is_some_and(|n| n >= threshold))
        .collect();
    let mut details = obj! { "slm_status" => mode.clone(), "policies" => policies };
    if !unhealthy.is_empty()
        && let Value::Obj(d) = &mut details
    {
        let mut counts = Map::new();
        for (id, r) in &unhealthy {
            counts.insert((*id).clone(), Value::from(num(r, "invocations_since_last_success").unwrap_or(0)));
        }
        d.insert("unhealthy_policies".into(), obj! { "count" => unhealthy.len(), "invocations_since_last_success" => Value::Obj(counts) });
    }
    let indicator = |status: &str, symptom: String, impacts: Option<Value>, diagnosis: Option<Value>| -> Value {
        let mut m = Map::new();
        m.insert("status".into(), status.into());
        m.insert("symptom".into(), symptom.into());
        if verbose {
            m.insert("details".into(), details.clone());
        }
        if let Some(i) = impacts {
            m.insert("impacts".into(), i);
        }
        if verbose && let Some(d) = diagnosis {
            m.insert("diagnosis".into(), d);
        }
        Value::Obj(m)
    };
    if policies == 0 {
        return indicator("green", "No Snapshot Lifecycle Management policies configured".into(), None, None);
    }
    if mode != "RUNNING" {
        let impacts = Value::Arr(vec![obj! {
            "id" => "elasticsearch:health:slm:impact:automation_disabled",
            "severity" => 3,
            "description" => "Scheduled snapshots are not running. New backup snapshots will not be created automatically.",
            "impact_areas" => vec![Value::from("backup")],
        }]);
        let diagnosis = Value::Arr(vec![obj! {
            "id" => "elasticsearch:health:slm:diagnosis:slm_disabled",
            "cause" => "Snapshot Lifecycle Management is stopped",
            "action" => "Start Snapshot Lifecycle Management using [POST /_slm/start].",
            "help_url" => "https://ela.st/fix-slm",
        }]);
        return indicator("yellow", "Snapshot Lifecycle Management is not running".into(), Some(impacts), Some(diagnosis));
    }
    if unhealthy.is_empty() {
        return indicator("green", "Snapshot Lifecycle Management is running".into(), None, None);
    }
    let many = unhealthy.len() > 1;
    let causes: Vec<String> = unhealthy
        .iter()
        .map(|(id, r)| {
            let n = num(r, "invocations_since_last_success").unwrap_or(0);
            match r.get("last_success").and_then(|s| num(s, "start_time")) {
                Some(t) => format!("- [{id}] had [{n}] repeated failures without successful execution since [{}]", crate::time::java_instant_string(t)),
                None => format!("- [{id}] had [{n}] repeated failures without successful execution"),
            }
        })
        .collect();
    let cause = format!("{}{}", if many { "Several automated snapshot policies are unhealthy:\n" } else { "An automated snapshot policy is unhealthy:\n" }, causes.join("\n"));
    let action = format!(
        "Check the snapshot lifecycle {} for detailed failure info:\n{}",
        if many { "policies" } else { "policy" },
        unhealthy.iter().map(|(id, _)| format!("- GET /_slm/policy/{id}?human")).collect::<Vec<_>>().join("\n")
    );
    let impacts = Value::Arr(vec![obj! {
        "id" => "elasticsearch:health:slm:impact:stale_snapshots",
        "severity" => 2,
        "description" => "Some automated snapshots have not had a successful execution recently. Indices restored from affected snapshots may not contain recent changes.",
        "impact_areas" => vec![Value::from("backup")],
    }]);
    let diagnosis = Value::Arr(vec![obj! {
        "id" => "elasticsearch:health:slm:diagnosis:check_recent_snapshot_failures",
        "cause" => cause,
        "action" => action,
        "help_url" => "https://ela.st/fix-recent-snapshot-failures",
        "affected_resources" => obj! { "slm_policies" => Value::Arr(unhealthy.iter().map(|(id, _)| Value::from(id.as_str())).collect()) },
    }]);
    indicator("yellow", format!("Encountered [{}] unhealthy snapshot lifecycle management policies", unhealthy.len()), Some(impacts), Some(diagnosis))
}

#[cfg(test)]
mod tests {
    use crate::cluster::{Cluster, ClusterConfig, MemoryStore};
    use crate::json::Value;
    use crate::rest::EsRequest;
    use std::sync::Arc;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        let r = crate::rest::handle(c, req).await;
        (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or(Value::Null))
    }

    fn policy_snapshots(c: &Cluster) -> usize {
        let st = c.state();
        let r = &st.metadata.repositories["repo"];
        st.metadata.repository_data.get(&r.location_key()).map_or(0, |d| {
            d.snapshots.iter().filter(|s| s.metadata.as_ref().and_then(|m| m.get("policy")).and_then(Value::as_str) == Some("p")).count()
        })
    }

    /// The scheduler takes a policy's snapshots on its schedule; retention then deletes all
    /// but `max_count` of them, and a stopped SLM has nothing due.
    #[tokio::test]
    async fn runs_on_schedule_and_retention_keeps_max_count() {
        let dir = std::env::temp_dir().join(format!("roll-slm-unit-{}", std::process::id()));
        let mut cfg = ClusterConfig::new("c");
        cfg.node_settings.push(("path.repo".into(), dir.to_string_lossy().into_owned()));
        let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
        c.enable_background_tasks();
        for (m, uri, body) in [
            ("PUT", "/_cluster/settings", r#"{"persistent":{"slm.minimum_interval":"0s"}}"#),
            ("PUT", "/_snapshot/repo", r#"{"type":"fs","settings":{"location":"repo"}}"#),
            ("PUT", "/docs/_doc/1?refresh=true", r#"{"a":1}"#),
            ("PUT", "/_slm/policy/p", r#"{"schedule":"300ms","name":"<s-{now/d}>","repository":"repo","retention":{"max_count":2}}"#),
        ] {
            let (s, v) = call(&c, m, uri, body).await;
            assert!(s < 300, "{uri}: {v}");
        }
        let modified = c.state().metadata.xpack.get(super::POLICIES, "p").and_then(|r| r.get("modified_date")).and_then(Value::as_i64).unwrap();
        assert_eq!(super::next_due(&c.state()), Some(modified + 300));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while policy_snapshots(&c) < 3 {
            assert!(std::time::Instant::now() < deadline, "the scheduler took {} snapshots", policy_snapshots(&c));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // A schedule far ahead ends the runs; retention keeps the newest two.
        let (s, v) = call(&c, "PUT", "/_slm/policy/p", r#"{"schedule":"0 0 0 1 1 ? 2099","name":"<s-{now/d}>","repository":"repo","retention":{"max_count":2}}"#).await;
        assert_eq!(s, 200, "{v}");
        let taken = policy_snapshots(&c);
        let (s, v) = call(&c, "POST", "/_slm/_execute_retention", "").await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(policy_snapshots(&c), 2);
        let (_, v) = call(&c, "GET", "/_slm/stats", "").await;
        assert_eq!(v.pointer(&["policy_stats"]).and_then(|p| p.as_array()).and_then(|p| p[0].get("snapshots_deleted")).and_then(Value::as_i64), Some(taken as i64 - 2), "{v}");
        assert!(super::next_due(&c.state()).is_some(), "retention stays scheduled");
        let (s, _) = call(&c, "POST", "/_slm/stop", "").await;
        assert_eq!(s, 200);
        assert_eq!(super::next_due(&c.state()), None, "nothing is due while SLM is stopped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

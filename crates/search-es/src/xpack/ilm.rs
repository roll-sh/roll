//! Index lifecycle management: policies (`_ilm/policy`), `_ilm/explain`, `_ilm/status`,
//! `_ilm/start|stop`, `<index>/_ilm/remove|retry`, `_ilm/move/<index>` and
//! `_ilm/migrate_to_data_tiers`.
//!
//! Policies are validated as `TimeseriesLifecycleType` does (phases `hot`, `warm`, `cold`,
//! `frozen`, `delete`; the actions each phase allows; action options with their defaults) and
//! printed in Elasticsearch's form (`min_age` defaulting to `0ms`; phases and actions in the
//! iteration order of the `HashMap`s Elasticsearch keeps them in). Elasticsearch's built-in
//! policies (captured from 9.5.5) are always present, as its index template registries install
//! them; a stored policy of the same name overrides one.
//!
//! The policies run for real ([`super::ilm_run`]): managed indices move through their phases
//! in the background, and `_ilm/explain` prints the execution state that keeps
//! (`phase`/`action`/`step` with their times, the lifecycle date and age, `phase_execution`,
//! `failed_step`, `step_info`, retry counts). `_ilm/retry` moves a failed index back to its
//! failed step, `_ilm/move` to any step of its policy, and `_ilm/stop` stops ILM through
//! `STOPPING` (what runs finishes first) to `STOPPED`.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, store};
use std::sync::OnceLock;

pub const POLICIES: &str = "ilm.policy";
/// The operation mode (`mode`).
pub(crate) const STATE_FAMILY: &str = "ilm.state";
const STEPS: &str = super::ilm_run::STATE;

const PHASES: [&str; 5] = ["hot", "warm", "cold", "frozen", "delete"];

fn allowed_actions(phase: &str) -> &'static [&'static str] {
    match phase {
        "hot" => &["set_priority", "unfollow", "rollover", "readonly", "downsample", "shrink", "forcemerge", "searchable_snapshot"],
        "warm" => &["set_priority", "unfollow", "readonly", "downsample", "allocate", "migrate", "shrink", "forcemerge"],
        "cold" => &["set_priority", "unfollow", "readonly", "downsample", "searchable_snapshot", "allocate", "migrate"],
        "frozen" => &["unfollow", "searchable_snapshot"],
        _ => &["wait_for_snapshot", "delete"],
    }
}

const ALL_ACTIONS: [&str; 13] =
    ["allocate", "delete", "downsample", "forcemerge", "migrate", "readonly", "rollover", "searchable_snapshot", "set_priority", "shrink", "unfollow", "wait_for_snapshot", "freeze"];

fn builtin() -> &'static Map {
    static B: OnceLock<Map> = OnceLock::new();
    B.get_or_init(|| match crate::json::parse(include_bytes!("ilm_builtin.json")) {
        Ok(Some(Value::Obj(m))) => m,
        _ => Map::new(),
    })
}

/// A policy by name: stored, else built in.
pub(crate) fn policy(st: &crate::meta::ClusterState, name: &str) -> Option<Value> {
    st.metadata.xpack.get(POLICIES, name).cloned().or_else(|| builtin().get(name).cloned())
}

/// Nested `[l:c] [parser] failed to parse field [f]` errors down to `cause`, all at the
/// location of `path`'s last field (its name, or its value with `value`).
fn nested(ctx: &Ctx, path: &[(&str, &str)], cause: EsError) -> EsError {
    nested_at(ctx, path, cause, false)
}

fn nested_at(ctx: &Ctx, path: &[(&str, &str)], cause: EsError, value: bool) -> EsError {
    use crate::json::locate::{Seg, Target, locate};
    let segs: Vec<Seg> = path.iter().map(|(_, f)| Seg::Key(f)).collect();
    let target = if value { Target::Value } else { Target::Name };
    let (l, c) = ctx.raw_json().and_then(|src| locate(src, &segs, target)).unwrap_or_else(|| crate::xpack::key_loc(ctx, "policy"));
    let mut err = cause;
    for (parser, field) in path.iter().rev() {
        err = EsError::x_content_parse(l, c, format!("[{parser}] failed to parse field [{field}]")).caused_by(err);
    }
    err
}

/// Validates an action's options and fills in its defaults.
fn normalize_action(name: &str, body: &Value) -> Result<Value, EsError> {
    let mut m = body.as_object().cloned().unwrap_or_default();
    match name {
        "rollover" => {
            const CONDS: [&str; 10] =
                ["max_size", "max_primary_shard_size", "max_age", "max_docs", "max_primary_shard_docs", "min_size", "min_primary_shard_size", "min_age", "min_docs", "min_primary_shard_docs"];
            if !m.keys().any(|k| k.starts_with("max_")) {
                return Err(EsError::illegal_argument("At least one max_* rollover condition must be set."));
            }
            if let Some(k) = m.keys().find(|k| !CONDS.contains(&k.as_str())) {
                return Err(EsError::x_content_parse_bare(format!("[rollover] unknown field [{k}]")));
            }
        }
        "delete" => {
            m.entry("delete_searchable_snapshot".to_string()).or_insert(Value::Bool(true));
        }
        "forcemerge" => {
            match m.get("max_num_segments").and_then(Value::as_i64) {
                Some(n) if n > 0 => {}
                Some(n) => return Err(EsError::illegal_argument(format!("[max_num_segments] must be a positive integer, got [{n}]"))),
                None => return Err(EsError::illegal_argument("Required [max_num_segments]")),
            }
        }
        "shrink" => {
            if m.contains_key("number_of_shards") == m.contains_key("max_primary_shard_size") {
                return Err(EsError::illegal_argument("Exactly one of [number_of_shards] or [max_primary_shard_size] must be set"));
            }
            m.entry("allow_write_after_shrink".to_string()).or_insert(Value::Bool(false));
        }
        "set_priority" => {
            if !m.contains_key("priority") {
                return Err(EsError::illegal_argument("Required [priority]"));
            }
        }
        "allocate" => {
            for k in ["include", "exclude", "require"] {
                m.entry(k.to_string()).or_insert_with(Value::object);
            }
        }
        "migrate" => {
            m.entry("enabled".to_string()).or_insert(Value::Bool(true));
        }
        "searchable_snapshot" => {
            if !m.contains_key("snapshot_repository") {
                return Err(EsError::illegal_argument("Required [snapshot_repository]"));
            }
            m.entry("force_merge_index".to_string()).or_insert(Value::Bool(true));
        }
        "wait_for_snapshot" => {
            if !m.contains_key("policy") {
                return Err(EsError::illegal_argument("Required [policy]"));
            }
        }
        "downsample" => {
            if !m.contains_key("fixed_interval") {
                return Err(EsError::illegal_argument("Required [fixed_interval]"));
            }
            m.entry("wait_timeout".to_string()).or_insert(Value::from("1d"));
        }
        _ => {}
    }
    Ok(Value::Obj(m))
}

/// `LifecyclePolicy` from a request's `policy` object.
fn parse_policy(ctx: &Ctx, p: &Value) -> EsResult<Value> {
    let Value::Obj(pm) = p else { return Err(EsError::illegal_argument("Required [phases]")) };
    let mut errors = Vec::new();
    let mut phases = Map::new();
    for (k, _) in pm {
        if !matches!(k.as_str(), "phases" | "_meta" | "deprecated") {
            return Err(nested(ctx, &[("put_lifecycle_request", "policy")], EsError::x_content_parse_bare(format!("[lifecycle_policy] unknown field [{k}]"))));
        }
    }
    let Some(Value::Obj(src)) = pm.get("phases") else { return Err(nested(ctx, &[("put_lifecycle_request", "policy")], EsError::illegal_argument("Required [phases]"))) };
    for (phase, def) in src {
        let path = [("put_lifecycle_request", "policy"), ("lifecycle_policy", "phases"), ("phases", phase.as_str())];
        let mut out = Map::new();
        let min_age = match def.get("min_age") {
            Some(v) => {
                let s = v.to_es_string();
                if let Err(e) = crate::units::TimeValue::parse(&s, "min_age") {
                    let mut p = path.to_vec();
                    p.push(("phase", "min_age"));
                    return Err(nested_at(ctx, &p, e, true));
                }
                s
            }
            None => "0ms".to_string(),
        };
        out.insert("min_age".into(), min_age.into());
        let mut actions = Map::new();
        for (a, body) in def.get("actions").and_then(Value::as_object).into_iter().flatten() {
            let mut p = path.to_vec();
            p.push(("phase", "actions"));
            if !ALL_ACTIONS.contains(&a.as_str()) {
                p.push(("actions", a.as_str()));
                use crate::json::locate::{Seg, Target, locate};
                let segs: Vec<Seg> = p.iter().map(|(_, f)| Seg::Key(f)).collect();
                let (l, c) = ctx.raw_json().and_then(|src| locate(src, &segs, Target::Name)).unwrap_or((1, 1));
                return Err(nested(ctx, &p, EsError::new(400, "named_object_not_found_exception", format!("[{l}:{c}] unknown field [{a}]"))));
            }
            match normalize_action(a, body) {
                Ok(v) => {
                    actions.insert(a.clone(), v);
                }
                Err(e) => {
                    p.push(("actions", a.as_str()));
                    return Err(nested(ctx, &p, e));
                }
            }
        }
        if !PHASES.contains(&phase.as_str()) {
            errors.push(format!("Timeseries lifecycle does not support phase [{phase}]"));
            continue;
        }
        for a in actions.keys() {
            if !allowed_actions(phase).contains(&a.as_str()) {
                errors.push(format!("invalid action [{a}] defined in phase [{phase}]"));
            }
        }
        out.insert("actions".into(), Value::Obj(crate::json::java_hash_map(actions, 16)));
        phases.insert(phase.clone(), Value::Obj(out));
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let mut policy = Map::new();
    policy.insert("phases".into(), Value::Obj(crate::json::java_hash_map(phases, 16)));
    if let Some(m) = pm.get("_meta") {
        policy.insert("_meta".into(), m.clone());
    }
    if let Some(d) = pm.get("deprecated") {
        policy.insert("deprecated".into(), d.clone());
    }
    Ok(Value::Obj(policy))
}

pub async fn put_lifecycle(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("policy").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if k != "policy" {
            return Err(crate::xpack::unknown_field(ctx, "put_lifecycle_request", k));
        }
    }
    let Some(p) = body.get("policy") else { return Err(EsError::illegal_argument("Required [policy]")) };
    let parsed = parse_policy(ctx, p)?;
    let now = ctx.now_ms;
    let user = internal::stored_user(crate::xpack::security::principal(ctx));
    ctx.cluster
        .update(move |st| {
            let version = match policy(st, &name) {
                Some(old) if old.get("policy") == Some(&parsed) => return Ok(()),
                Some(old) => old.get("version").and_then(Value::as_i64).unwrap_or(0) + 1,
                None => 1,
            };
            let mut rec = obj! { "version" => version, "modified_date" => now, "policy" => parsed };
            // The policy runs as the user who last put it (Elasticsearch's stored security
            // headers); a superuser's with the cluster's authority.
            if let (Some(u), Value::Obj(m)) = (user, &mut rec) {
                m.insert("_roll".into(), obj! { "user" => u });
            }
            store::put(st, POLICIES, &name, rec, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

fn in_use_by(st: &crate::meta::ClusterState, name: &str) -> Value {
    let uses = |s: &crate::meta::settings::Settings| s.get("index.lifecycle.name") == Some(name);
    let indices: Vec<Value> = st.metadata.indices.values().filter(|i| uses(&i.settings)).map(|i| Value::from(i.name.as_str())).collect();
    let data_streams: Vec<Value> = st
        .metadata
        .data_streams
        .values()
        .filter(|d| d.indices.last().and_then(|w| st.metadata.indices.get(w)).is_some_and(|i| uses(&i.settings)))
        .map(|d| Value::from(d.name.as_str()))
        .collect();
    let templates: Vec<Value> = st
        .metadata
        .index_templates
        .iter()
        .filter(|(_, t)| t.to_value().pointer(&["template", "settings", "index", "lifecycle", "name"]).and_then(Value::as_str) == Some(name))
        .map(|(n, _)| Value::from(n.as_str()))
        .collect();
    obj! { "indices" => Value::Arr(indices), "data_streams" => Value::Arr(data_streams), "composable_templates" => Value::Arr(templates) }
}

fn printed(st: &crate::meta::ClusterState, name: &str, p: &Value) -> Value {
    let modified = p.get("modified_date").and_then(Value::as_i64).unwrap_or_else(|| st.metadata.indices.values().map(|_| 0).next().unwrap_or(0));
    obj! {
        "version" => p.get("version").cloned().unwrap_or(Value::from(1)),
        "modified_date" => crate::time::java_instant_string(modified),
        "policy" => p.get("policy").cloned().unwrap_or(Value::Null),
        "in_use_by" => in_use_by(st, name),
    }
}

pub fn get_lifecycle(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let mut all = Map::new();
    for (k, v) in builtin() {
        all.insert(k.clone(), v.clone());
    }
    for (k, v) in st.metadata.xpack.iter(POLICIES) {
        all.insert(k.clone(), v.clone());
    }
    let mut out = Map::new();
    match ctx.list_param("policy") {
        None => {
            for (k, v) in &all {
                out.insert(k.clone(), printed(&st, k, v));
            }
        }
        Some(names) => {
            for n in names {
                let mut found = false;
                for (k, v) in &all {
                    if n == "_all" || crate::xcontent::filter::glob_match(&n, k) {
                        out.insert(k.clone(), printed(&st, k, v));
                        found = true;
                    }
                }
                if !found && !n.contains('*') {
                    return Err(EsError::resource_not_found(format!("Lifecycle policy not found: {n}")));
                }
            }
        }
    }
    Reply::ok(Value::Obj(out))
}

pub async fn delete_lifecycle(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("policy").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            if policy(st, &name).is_none() {
                return Err(EsError::resource_not_found(format!("Lifecycle policy not found: {name}")));
            }
            let users: Vec<String> = st.metadata.indices.values().filter(|i| i.settings.get("index.lifecycle.name") == Some(name.as_str())).map(|i| i.name.clone()).collect();
            if !users.is_empty() {
                return Err(EsError::illegal_argument(format!("Cannot delete policy [{name}]. It is in use by one or more indices: [{}]", users.join(", "))));
            }
            store::remove(st, POLICIES, &name);
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// The ILM operation mode (`RUNNING`, `STOPPING`, `STOPPED`).
pub(crate) fn mode(st: &crate::meta::ClusterState) -> String {
    st.metadata.xpack.get(STATE_FAMILY, "mode").and_then(|v| v.get("operation_mode")).and_then(Value::as_str).unwrap_or("RUNNING").to_string()
}

pub(crate) fn put_mode(st: &mut crate::meta::ClusterState, mode: &str, now: i64) -> EsResult<()> {
    store::put(st, STATE_FAMILY, "mode", obj! { "operation_mode" => mode }, now).map(|_| ())
}

pub fn get_status(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    Reply::ok(obj! { "operation_mode" => mode(&ctx.state()) })
}

pub async fn set_mode(ctx: &Ctx<'_>, running: bool) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            // `OperationModeUpdateTask`: stopping goes through `STOPPING`; the next ILM run
            // makes it `STOPPED` once no index is in a step that must finish.
            let next = match (running, mode(st).as_str()) {
                (true, _) => "RUNNING",
                (false, "STOPPED") => return Ok(()),
                (false, _) => "STOPPING",
            };
            put_mode(st, next, now)
        })
        .await?;
    Reply::acknowledged()
}

/// Concrete indices an `<index>` expression names (data streams resolve to backing indices).
async fn resolve(ctx: &Ctx<'_>, expr: &str) -> EsResult<Vec<String>> {
    let r = internal::call(ctx.cluster, "GET", &format!("/{}/_settings/index.uuid?expand_wildcards=open,closed,hidden", enc(expr)), None).await?;
    Ok(r.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default())
}

pub async fn explain(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let only_managed = ctx.bool_param("only_managed", false)?;
    let only_errors = ctx.bool_param("only_errors", false)?;
    let expr = ctx.path_param("index").unwrap_or("_all").to_string();
    let mut names = resolve(ctx, &expr).await?;
    names.sort();
    let st = ctx.state();
    let mut out = Map::new();
    for n in names {
        let Some(idx) = st.metadata.indices.get(&n) else { continue };
        let Some(pname) = super::ilm_run::policy_name(idx) else {
            if !only_managed && !only_errors {
                out.insert(n.clone(), obj! { "index" => n.clone(), "managed" => false });
            }
            continue;
        };
        let e = super::ilm_run::exec_of(&st, idx).unwrap_or_default();
        let rec = policy(&st, pname);
        // `TransportExplainLifecycleAction`: a missing policy reads as the error step.
        let step = if rec.is_none() { Some(super::ilm_run::ERROR_STEP.to_string()) } else { e.step.clone() };
        if only_errors && step.as_deref() != Some(super::ilm_run::ERROR_STEP) {
            continue;
        }
        let created = idx.creation_date();
        let human = |ms: i64| crate::units::TimeValue::millis(ms).human_with(2);
        let mut m = Map::new();
        m.insert("index".into(), n.clone().into());
        m.insert("managed".into(), true.into());
        m.insert("policy".into(), pname.into());
        m.insert("index_creation_date_millis".into(), created.into());
        m.insert("time_since_index_creation".into(), human((ctx.now_ms - created).max(0)).into());
        if let Some(date) = super::ilm_run::lifecycle_date(idx, &e) {
            let age = (ctx.now_ms - date).max(0);
            m.insert("lifecycle_date_millis".into(), date.into());
            m.insert("age".into(), human(age).into());
            m.insert("age_in_millis".into(), age.into());
        }
        let mut field = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                m.insert(k.into(), v);
            }
        };
        field("phase", e.phase.clone().map(Value::from));
        field("phase_time_millis", e.phase_time.map(Value::from));
        field("action", e.action.clone().map(Value::from));
        field("action_time_millis", e.action_time.map(Value::from));
        field("step", step.map(Value::from));
        field("step_time_millis", e.step_time.map(Value::from));
        field("failed_step", e.failed_step.clone().filter(|f| !f.is_empty()).map(Value::from));
        field("is_auto_retryable_error", e.auto_retryable.map(Value::from));
        field("failed_step_retry_count", e.retry_count.map(Value::from));
        field("step_info", e.step_info.clone());
        field("previous_step_info", e.previous_step_info.clone());
        // The phase as cached, with the rollover's default conditions shown.
        let phase_execution = e.phase_definition.clone().map(|mut p| {
            if let Some(Value::Obj(def)) = p.as_object_mut().and_then(|m| m.get_mut("phase_definition"))
                && let Some(Value::Obj(actions)) = def.get_mut("actions")
                && let Some(r) = actions.get("rollover").cloned()
            {
                actions.insert("rollover".into(), Value::Obj(super::ilm_run::conditions_with_defaults(&st, &r)));
            }
            p
        });
        field("phase_execution", phase_execution);
        m.insert("skip".into(), (idx.settings.get("index.lifecycle.skip") == Some("true")).into());
        out.insert(n, Value::Obj(m));
    }
    Reply::ok(obj! { "indices" => Value::Obj(out) })
}

pub async fn remove_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let expr = ctx.path_param("index").unwrap_or_default().to_string();
    let names = resolve(ctx, &expr).await?;
    let st = ctx.state();
    let managed: Vec<String> = names.into_iter().filter(|n| st.metadata.indices.get(n).is_some_and(|i| i.settings.get("index.lifecycle.name").is_some())).collect();
    drop(st);
    if !managed.is_empty() {
        let body = obj! { "index.lifecycle.name" => Value::Null, "index.lifecycle.indexing_complete" => Value::Null };
        internal::call(ctx.cluster, "PUT", &format!("/{}/_settings?expand_wildcards=all", enc(&managed.join(","))), Some(&body)).await?;
        ctx.cluster
            .update(move |st| {
                for n in &managed {
                    store::remove(st, STEPS, n);
                }
                Ok(())
            })
            .await?;
    }
    Reply::ok(obj! { "has_failures" => false, "failed_indexes" => Value::Arr(vec![]) })
}

/// Concrete names an `<index>` expression of the retry API names: wildcards expanded, plain
/// names kept (a missing one fails in the transition, as in Elasticsearch).
async fn named(ctx: &Ctx<'_>, expr: &str) -> EsResult<Vec<String>> {
    let mut out = Vec::new();
    for part in expr.split(',').filter(|p| !p.is_empty()) {
        if part.contains('*') || part == "_all" {
            out.extend(resolve(ctx, part).await?);
        } else {
            out.push(part.to_string());
        }
    }
    Ok(out)
}

/// `POST <index>/_ilm/retry`: `moveIndexToPreviouslyFailedStep` for each index.
pub async fn retry(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let expr = ctx.path_param("index").unwrap_or_default().to_string();
    let names = named(ctx, &expr).await?;
    let require_error = ctx.bool_param("require_error", true)?;
    if !require_error {
        // Only reruns an index's current step: the scheduler picks it up.
        ctx.cluster.jobs.kick(ctx.cluster);
        return Reply::acknowledged();
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            for n in &names {
                let Some(idx) = st.metadata.indices.get(n).cloned() else { return Err(EsError::illegal_argument(format!("index [{n}] does not exist"))) };
                let e = super::ilm_run::exec_of(st, &idx).unwrap_or_default();
                let not_failed = || EsError::illegal_argument(format!("cannot retry an action for an index [{n}] that has not encountered an error when running a Lifecycle Policy"));
                let (Some(failed), Some((phase, action, super::ilm_run::ERROR_STEP))) = (e.failed_step.clone().filter(|f| !f.is_empty()), e.key()) else { return Err(not_failed()) };
                let (phase, action) = (phase.to_string(), action.to_string());
                let pname = super::ilm_run::policy_name(&idx).unwrap_or_default().to_string();
                let rec = policy(st, &pname);
                check_step(n, &pname, rec.as_ref(), &e, &phase, &action, &failed)?;
                let refresh = rec.as_ref().is_some_and(|r| r.pointer(&["policy", "phases", &phase, "actions", &action]).is_some());
                let mut next = e.clone();
                next.move_to(&phase, &action, &failed, rec.as_ref().map(|r| (pname.as_str(), r)), refresh, false, now);
                next.auto_retryable = e.auto_retryable;
                next.retry_count = e.retry_count;
                store::family_mut(st, STEPS).insert(n.clone(), next.to_value());
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `validateTransition`'s step check.
fn check_step(index: &str, pname: &str, rec: Option<&Value>, e: &super::ilm_run::Exec, phase: &str, action: &str, name: &str) -> EsResult<()> {
    if super::ilm_run::step_exists(phase, action, name, rec, Some(e)) || (phase == "completed" && action == "completed" && name == "completed") || (action == "complete" && name == "complete" && e.phase.as_deref() == Some(phase)) {
        return Ok(());
    }
    Err(EsError::illegal_argument(format!("step [{{\"phase\":\"{phase}\",\"action\":\"{action}\",\"name\":\"{name}\"}}] for index [{index}] with policy [{pname}] does not exist")))
}

/// `POST _ilm/move/<index>` (`TransportMoveToStepAction`).
pub async fn move_to_step(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let index = ctx.path_param("index").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(next) = body.get("next_step").cloned() else { return Err(EsError::illegal_argument("Required [next_step]")) };
    let current = body.get("current_step").cloned();
    let st = ctx.state();
    let Some(idx) = st.metadata.indices.get(&index).cloned() else { return Err(EsError::illegal_argument(format!("index [{index}] does not exist"))) };
    let Some(pname) = super::ilm_run::policy_name(&idx).map(str::to_string) else {
        return Err(EsError::illegal_argument(format!("index [{index}] is not associated with an Index Lifecycle Policy")));
    };
    drop(st);
    let text = |v: &Value, k: &str| v.get(k).filter(|x| !x.is_null()).map(Value::to_es_string);
    let phase = text(&next, "phase").unwrap_or_default();
    let (action, name) = (text(&next, "action"), text(&next, "name"));
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(idx) = st.metadata.indices.get(&index).cloned() else { return Err(EsError::illegal_argument(format!("index [{index}] does not exist"))) };
            let e = super::ilm_run::exec_of(st, &idx).unwrap_or_default();
            let rec = policy(st, &pname);
            // `resolveStepKey`: a phase alone is its first step, a phase and action the
            // action's first step.
            let target = match (&action, &name) {
                (Some(a), Some(n)) => Some((a.clone(), n.clone())),
                (a, _) => rec.as_ref().and_then(|r| super::ilm_run::first_step_of(&phase, a.as_deref(), r)).map(|(a, s)| (a, s.to_string())),
            };
            let Some((action, name)) = target else {
                return Err(EsError::illegal_argument(format!(
                    "cannot move index [{index}] with policy [{pname}]: unable to determine concrete step key from target next step key: {{\"phase\":\"{phase}\",\"action\":\"{}\",\"name\":\"{}\"}}",
                    action.clone().unwrap_or_default(),
                    name.clone().unwrap_or_default()
                )));
            };
            if let Some(c) = &current {
                let cur = e.key().map(|(p, a, s)| (p.to_string(), a.to_string(), s.to_string()));
                let want = (text(c, "phase").unwrap_or_default(), text(c, "action").unwrap_or_default(), text(c, "name").unwrap_or_default());
                if cur.as_ref() != Some(&want) {
                    let (p, a, s) = cur.unwrap_or_default();
                    return Err(EsError::illegal_argument(format!(
                        "index [{index}] is not on current step [{{\"phase\":\"{}\",\"action\":\"{}\",\"name\":\"{}\"}}], currently: [{{\"phase\":\"{p}\",\"action\":\"{a}\",\"name\":\"{s}\"}}]",
                        want.0, want.1, want.2
                    )));
                }
            }
            check_step(&index, &pname, rec.as_ref(), &e, &phase, &action, &name)?;
            let mut next = super::ilm_run::Exec { uuid: idx.uuid.clone(), ..e };
            next.move_to(&phase, &action, &name, rec.as_ref().map(|r| (pname.as_str(), r)), true, true, now);
            store::family_mut(st, STEPS).insert(index.clone(), next.to_value());
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn migrate_to_data_tiers(ctx: &Ctx) -> EsResult<Reply> {
    ctx.bool_param("dry_run", false)?;
    let st = ctx.state();
    let m = mode(&st);
    if m != "STOPPED" {
        return Err(EsError::java(500, "illegal_state_exception", format!("stop ILM before migrating to data tiers, current state is [{m}]")));
    }
    Reply::ok(obj! {
        "dry_run" => ctx.bool_param("dry_run", false)?,
        "removed_legacy_template" => Value::Null,
        "migrated_ilm_policies" => Value::Arr(vec![]),
        "migrated_indices" => Value::Arr(vec![]),
        "migrated_legacy_templates" => Value::Arr(vec![]),
        "migrated_composable_templates" => Value::Arr(vec![]),
        "migrated_component_templates" => Value::Arr(vec![]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_load() {
        assert!(builtin().len() >= 40);
        assert!(builtin().contains_key("logs@lifecycle"));
    }

    #[test]
    fn hash_order_matches_elasticsearch() {
        // Elasticsearch prints {"hot","delete"} as hot, delete and {"warm","hot"} as warm, hot.
        let mut m = Map::new();
        m.insert("delete".into(), Value::Null);
        m.insert("hot".into(), Value::Null);
        let keys: Vec<String> = crate::json::java_hash_map(m, 16).keys().cloned().collect();
        assert_eq!(keys, ["hot", "delete"]);
        let mut m = Map::new();
        m.insert("hot".into(), Value::Null);
        m.insert("warm".into(), Value::Null);
        let keys: Vec<String> = crate::json::java_hash_map(m, 16).keys().cloned().collect();
        assert_eq!(keys, ["warm", "hot"]);
    }

    #[test]
    fn action_defaults() {
        assert_eq!(normalize_action("delete", &obj! {}).unwrap(), obj! { "delete_searchable_snapshot" => true });
        assert_eq!(normalize_action("rollover", &obj! {}).unwrap_err().reason(), "At least one max_* rollover condition must be set.");
    }
}

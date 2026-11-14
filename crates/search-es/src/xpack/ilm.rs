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
//! **What is emulated:** roll doesn't run the lifecycle steps (rollover, shrink, delete...) in
//! the background. Managed indices report the state Elasticsearch reports before its first ILM
//! poll (`new` / `complete` / `complete`), and `move_to_step` records the requested step.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, store};
use std::sync::OnceLock;

pub const POLICIES: &str = "ilm.policy";
const STATE: &str = "ilm.state";
const STEPS: &str = "ilm.step";

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
fn policy(st: &crate::meta::ClusterState, name: &str) -> Option<Value> {
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
    ctx.cluster
        .update(move |st| {
            let version = match policy(st, &name) {
                Some(old) if old.get("policy") == Some(&parsed) => return Ok(()),
                Some(old) => old.get("version").and_then(Value::as_i64).unwrap_or(0) + 1,
                None => 1,
            };
            store::put(st, POLICIES, &name, obj! { "version" => version, "modified_date" => now, "policy" => parsed }, now).map(|_| ())
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

fn mode(st: &crate::meta::ClusterState) -> String {
    st.metadata.xpack.get(STATE, "mode").and_then(|v| v.get("operation_mode")).and_then(Value::as_str).unwrap_or("RUNNING").to_string()
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
            // No step is ever mid-execution, so stopping completes at once.
            store::put(st, STATE, "mode", obj! { "operation_mode" => if running { "RUNNING" } else { "STOPPED" } }, now).map(|_| ())
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
        let Some(pname) = idx.settings.get("index.lifecycle.name").filter(|p| !p.is_empty()) else {
            if !only_managed && !only_errors {
                out.insert(n.clone(), obj! { "index" => n.clone(), "managed" => false });
            }
            continue;
        };
        if only_errors {
            continue;
        }
        let created = idx.settings.get("index.creation_date").and_then(|s| s.parse::<i64>().ok()).unwrap_or(ctx.now_ms);
        let lifecycle_date = idx.settings.get("index.lifecycle.origination_date").and_then(|s| s.parse::<i64>().ok()).unwrap_or(created);
        let age = (ctx.now_ms - lifecycle_date).max(0);
        let step = st.metadata.xpack.get(STEPS, &n).cloned();
        let (phase, action, stepname, at) = match &step {
            Some(s) => (
                s.get("phase").map(Value::to_es_string).unwrap_or_else(|| "new".into()),
                s.get("action").map(Value::to_es_string).unwrap_or_else(|| "complete".into()),
                s.get("name").map(Value::to_es_string).unwrap_or_else(|| "complete".into()),
                s.get("time").and_then(Value::as_i64).unwrap_or(created),
            ),
            None => ("new".to_string(), "complete".to_string(), "complete".to_string(), created),
        };
        let human = |ms: i64| crate::units::TimeValue::millis(ms).human();
        let mut m = Map::new();
        m.insert("index".into(), n.clone().into());
        m.insert("managed".into(), true.into());
        m.insert("policy".into(), pname.into());
        m.insert("index_creation_date_millis".into(), created.into());
        m.insert("time_since_index_creation".into(), human((ctx.now_ms - created).max(0)).into());
        m.insert("lifecycle_date_millis".into(), lifecycle_date.into());
        m.insert("age".into(), human(age).into());
        m.insert("age_in_millis".into(), age.into());
        m.insert("phase".into(), phase.into());
        m.insert("phase_time_millis".into(), at.into());
        m.insert("action".into(), action.into());
        m.insert("action_time_millis".into(), at.into());
        m.insert("step".into(), stepname.into());
        m.insert("step_time_millis".into(), at.into());
        if let Some(p) = policy(&st, pname) {
            let modified = p.get("modified_date").and_then(Value::as_i64).unwrap_or(0);
            m.insert("phase_execution".into(), obj! { "policy" => pname, "version" => p.get("version").cloned().unwrap_or(Value::from(1)), "modified_date_in_millis" => modified });
        }
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

pub async fn retry(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let expr = ctx.path_param("index").unwrap_or_default().to_string();
    let names = resolve(ctx, &expr).await?;
    if let Some(n) = names.first() {
        return Err(EsError::illegal_argument(format!("cannot retry an action for an index [{n}] that has not encountered an error when running a Lifecycle Policy")));
    }
    Reply::acknowledged()
}

pub async fn move_to_step(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let index = ctx.path_param("index").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(next) = body.get("next_step").cloned() else { return Err(EsError::illegal_argument("Required [next_step]")) };
    let current = body.get("current_step").cloned();
    let st = ctx.state();
    let Some(idx) = st.metadata.indices.get(&index) else { return Err(EsError::index_not_found(&index)) };
    let Some(pname) = idx.settings.get("index.lifecycle.name").map(str::to_string) else {
        return Err(EsError::illegal_argument(format!("index [{index}] is not associated with an Index Lifecycle Policy")));
    };
    let phase = next.get("phase").map(Value::to_es_string).unwrap_or_default();
    let known = policy(&st, &pname).and_then(|p| p.pointer(&["policy", "phases", phase.as_str()]).cloned()).is_some() || phase == "new";
    if !known {
        return Err(EsError::illegal_argument(format!("step [{}] for index [{index}] with policy [{pname}] does not exist", next.to_json())));
    }
    if let Some(c) = &current {
        let cur = st.metadata.xpack.get(STEPS, &index).cloned().unwrap_or_else(|| obj! { "phase" => "new", "action" => "complete", "name" => "complete" });
        let same = ["phase", "action", "name"].iter().all(|k| c.get(k).is_none_or(|v| cur.get(k) == Some(v)));
        if !same {
            return Err(EsError::illegal_argument(format!(
                "index [{index}] is not on current step [{{\"phase\":\"{}\",\"action\":\"{}\",\"name\":\"{}\"}}], currently: [{{\"phase\":\"{}\",\"action\":\"{}\",\"name\":\"{}\"}}]",
                c.get("phase").map(Value::to_es_string).unwrap_or_default(),
                c.get("action").map(Value::to_es_string).unwrap_or_default(),
                c.get("name").map(Value::to_es_string).unwrap_or_default(),
                cur.get("phase").map(Value::to_es_string).unwrap_or_default(),
                cur.get("action").map(Value::to_es_string).unwrap_or_default(),
                cur.get("name").map(Value::to_es_string).unwrap_or_default(),
            )));
        }
    }
    drop(st);
    let now = ctx.now_ms;
    let step = obj! {
        "phase" => phase,
        "action" => next.get("action").cloned().unwrap_or_else(|| Value::from("complete")),
        "name" => next.get("name").cloned().unwrap_or_else(|| Value::from("complete")),
        "time" => now,
    };
    ctx.cluster.update(move |st| store::put(st, STEPS, &index, step, now).map(|_| ())).await?;
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

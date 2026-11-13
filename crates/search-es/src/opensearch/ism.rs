//! OpenSearch's Index State Management plugin API (`_plugins/_ism/*`, and its
//! `_opendistro/_ism/*` aliases): policies, the indices they manage, and explain.
//!
//! The plugin keeps policies and managed-index configs as documents of the system index
//! `.opendistro-ism-config`, written with sequence numbers. roll keeps them in the cluster
//! metadata ([`crate::xpack::store`] families `ism.policy` and `ism.managed`) and creates the
//! hidden config index on the first write, so listings and "no such index" errors look the
//! same; one counter stands for the index's sequence numbers, advanced by every write the
//! plugin makes there (policy writes, managed-index configs added, changed or removed).
//!
//! Policies are parsed token by token like the plugin's Kotlin parsers, so errors carry the
//! same messages and positions, and stored in the plugin's normalized form (every action with
//! its `retry` defaults, `ism_template` as a list with update times, scripts with `lang`).
//!
//! Managed indices: `add`, `remove`, `change_policy` and `retry` act on index expressions;
//! `ism_template`s manage matching indices (or data streams' backing indices) on creation
//! unless they are hidden or `index.plugins.index_state_management.auto_manage` is false.
//! Each managed index's record is also its job: a copy of the policy, the interval schedule
//! and, after the first run, the `ManagedIndexMetaData` explain prints. The job scheduler
//! ([`super::jobs`]) runs each policy's state machine every `job_interval` minutes
//! ([`super::ism_run`]).

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::meta::resolve::IndicesOptions;
use crate::obj;
use crate::query::{Token as Tk, XParser};
use crate::rest::{Ctx, Reply};
use crate::routes::Api;
use crate::xpack::store;

/// The plugin's config index.
pub const CONFIG_INDEX: &str = ".opendistro-ism-config";
pub(super) const POLICIES: &str = "ism.policy";
/// Managed indices by index name.
pub(super) const MANAGED: &str = "ism.managed";
const META: &str = "ism.meta";
/// `IndexManagementIndices`' config mapping schema version, which `GET` reports.
const SCHEMA_VERSION: i64 = 31;

/// Whether this module handles an API.
pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::IsmPutPolicy
            | Api::IsmGetPolicy
            | Api::IsmGetPolicies
            | Api::IsmDeletePolicy
            | Api::IsmAddPolicy
            | Api::IsmRemovePolicy
            | Api::IsmChangePolicy
            | Api::IsmRetry
            | Api::IsmExplain
    )
}

pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    match ctx.api {
        Api::IsmPutPolicy => put_policy(ctx).await,
        Api::IsmGetPolicy => get_policy(ctx),
        Api::IsmGetPolicies => get_policies(ctx),
        Api::IsmDeletePolicy => delete_policy(ctx).await,
        Api::IsmAddPolicy => add(ctx).await,
        Api::IsmRemovePolicy => remove(ctx).await,
        Api::IsmChangePolicy => change_policy(ctx).await,
        Api::IsmRetry => retry(ctx).await,
        Api::IsmExplain => explain(ctx),
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

/// `expecting token of type [X] but found [Y]` at the parser's current token.
fn expect(p: &XParser<'_>, want: Tk) -> EsResult<()> {
    let got = p.current_token();
    if got == Some(want) {
        return Ok(());
    }
    Err(p.parsing_error(format!("Failed to parse object: expecting token of type [{}] but found [{}]", want.name(), got.map_or("null", Tk::name))))
}

fn invalid(field: &str, what: &str) -> EsError {
    iae(format!("Invalid field: [{field}] found in {what}."))
}

fn config_index_missing() -> EsError {
    EsError::new(404, "index_not_found_exception", format!("no such index [{CONFIG_INDEX}]"))
        .with_meta("index", CONFIG_INDEX)
        .with_meta("resource.id", CONFIG_INDEX)
        .with_meta("resource.type", "index_expression")
        .with_meta("index_uuid", "_na_")
}

// ------------------------------------------------------------------------------ parsing

/// `xcp.text()`: a string (numbers and booleans as text); `null` fails like Jackson.
fn text(p: &XParser<'_>) -> EsResult<String> {
    if p.current_token() == Some(Tk::Null) {
        let (l, c) = p.token_location();
        return Err(EsError::java(500, "illegal_state_exception", format!("Can't get text on a VALUE_NULL at {l}:{c}")));
    }
    p.text()
}

fn time_text(p: &XParser<'_>, field: &str) -> EsResult<String> {
    let s = text(p)?;
    crate::units::TimeValue::parse(&s, field)?;
    Ok(s)
}

fn bytes_text(p: &XParser<'_>, field: &str) -> EsResult<String> {
    let s = text(p)?;
    crate::units::ByteSize::parse(&s, field)?;
    Ok(s)
}

/// A mustache template (`Script.parse` with the default language `mustache`).
fn template(p: &mut XParser<'_>) -> EsResult<Value> {
    let v = p.take_value();
    let mut m = match v {
        Value::Obj(m) => m,
        Value::Str(s) => {
            let mut m = Map::new();
            m.insert("source".into(), Value::Str(s));
            m
        }
        _ => return Err(iae("Script must be an object or a string")),
    };
    if !m.contains_key("lang") {
        m.insert("lang".into(), Value::Str("mustache".into()));
    }
    Ok(Value::Obj(m))
}

/// The request body `{"policy": {...}}` (`Policy.parseWithType`: any first field name).
fn parse_request(p: &mut XParser<'_>, id: &str, now: i64) -> EsResult<Map> {
    p.next_token();
    expect(p, Tk::StartObject)?;
    p.next_token();
    expect(p, Tk::FieldName)?;
    p.next_token();
    expect(p, Tk::StartObject)?;
    let policy = parse_policy(p, id, now)?;
    p.next_token();
    expect(p, Tk::EndObject)?;
    Ok(policy)
}

/// Field name and value token of the next field of the current object (`None` at its end).
fn next_field(p: &mut XParser<'_>) -> Option<String> {
    match p.next_token() {
        Some(Tk::FieldName) => {
            let name = p.current_name().unwrap_or_default().to_string();
            p.next_token();
            Some(name)
        }
        _ => None,
    }
}

/// The items of the array at the current token, each parsed by `item` at its first token.
fn array<T>(p: &mut XParser<'_>, mut item: impl FnMut(&mut XParser<'_>) -> EsResult<T>) -> EsResult<Vec<T>> {
    expect(p, Tk::StartArray)?;
    let mut out = Vec::new();
    while let Some(t) = p.next_token() {
        if t == Tk::EndArray {
            break;
        }
        out.push(item(p)?);
    }
    Ok(out)
}

fn parse_policy(p: &mut XParser<'_>, id: &str, now: i64) -> EsResult<Map> {
    let (mut description, mut default_state, mut schema, mut notification) = (None, None, 1i64, Value::Null);
    let mut states: Vec<Map> = Vec::new();
    let mut templates: Option<Vec<Value>> = None;
    while let Some(name) = next_field(p) {
        match name.as_str() {
            "policy_id" | "last_updated_time" | "user" => p.skip_children(),
            "description" => description = Some(text(p)?),
            "schema_version" => schema = p.long_value()?,
            "error_notification" => {
                notification = if p.current_token() == Some(Tk::Null) { Value::Null } else { parse_notification(p)? };
            }
            "default_state" => default_state = Some(text(p)?),
            "states" => states = array(p, parse_state)?,
            "ism_template" => match p.current_token() {
                Some(Tk::Null) => {}
                Some(Tk::StartArray) => templates = Some(array(p, |p| parse_template(p, now))?),
                _ => templates = Some(vec![parse_template(p, now)?]),
            },
            other => return Err(invalid(other, "Policy")),
        }
    }
    let description = description.ok_or_else(|| iae("description is null"))?;
    let default_state = default_state.ok_or_else(|| iae("default_state is null"))?;
    // `Policy.init`.
    let names: Vec<String> = states.iter().map(|s| s.get("name").and_then(Value::as_str).unwrap_or_default().to_string()).collect();
    for s in &states {
        for t in s.get("transitions").and_then(Value::as_array).into_iter().flatten() {
            let target = t.get("state_name").and_then(Value::as_str).unwrap_or_default();
            if !names.iter().any(|n| n == target) {
                return Err(iae(format!(
                    "Policy contains a transition in state={} pointing to a nonexistent state={target}",
                    s.get("name").and_then(Value::as_str).unwrap_or_default()
                )));
            }
        }
    }
    if states.is_empty() {
        return Err(iae("Policy must contain at least one State"));
    }
    let mut distinct = names.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() != names.len() {
        return Err(iae("Policy cannot have duplicate state names"));
    }
    if !names.contains(&default_state) {
        return Err(iae("Policy must have a valid default state"));
    }
    let mut m = Map::new();
    m.insert("policy_id".into(), Value::Str(id.to_string()));
    m.insert("description".into(), Value::Str(description));
    m.insert("last_updated_time".into(), Value::from(now));
    m.insert("schema_version".into(), Value::from(schema));
    m.insert("error_notification".into(), notification);
    m.insert("default_state".into(), Value::Str(default_state));
    m.insert("states".into(), Value::Arr(states.into_iter().map(Value::Obj).collect()));
    m.insert("ism_template".into(), templates.map_or(Value::Null, Value::Arr));
    Ok(m)
}

fn parse_notification(p: &mut XParser<'_>) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    let mut target: Option<(String, Value)> = None;
    let mut message = None;
    while let Some(name) = next_field(p) {
        match name.as_str() {
            "destination" | "channel" => target = Some((name, p.take_value())),
            "message_template" => message = Some(template(p)?),
            other => return Err(invalid(other, "ErrorNotification")),
        }
    }
    let message = message.ok_or_else(|| iae("ErrorNotification message template is null"))?;
    let mut m = Map::new();
    if let Some((k, v)) = target {
        m.insert(k, v);
    }
    m.insert("message_template".into(), message);
    Ok(Value::Obj(m))
}

fn parse_template(p: &mut XParser<'_>, now: i64) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    let (mut patterns, mut priority) = (Vec::new(), 0i64);
    while let Some(name) = next_field(p) {
        match name.as_str() {
            "index_patterns" => patterns = array(p, |p| text(p).map(Value::Str))?,
            "priority" => priority = p.long_value()?,
            "last_updated_time" => p.skip_children(),
            other => return Err(invalid(other, "ISMTemplate")),
        }
    }
    if priority < 0 {
        return Err(iae("Requires priority to be >= 0"));
    }
    if patterns.is_empty() {
        return Err(iae("Requires at least one index pattern"));
    }
    Ok(obj! { "index_patterns" => Value::Arr(patterns), "priority" => priority, "last_updated_time" => now })
}

fn parse_state(p: &mut XParser<'_>) -> EsResult<Map> {
    expect(p, Tk::StartObject)?;
    let mut name = None;
    let (mut actions, mut transitions) = (Vec::new(), Vec::new());
    while let Some(field) = next_field(p) {
        match field.as_str() {
            "name" => name = Some(text(p)?),
            "actions" => actions = array(p, parse_action)?,
            "transitions" => transitions = array(p, parse_transition)?,
            other => return Err(invalid(other, "State")),
        }
    }
    let name = name.ok_or_else(|| iae("State name is null"))?;
    // `State.init`.
    let kind = |a: &Value| a.as_object().and_then(|m| m.keys().last().cloned()).unwrap_or_default();
    if actions.iter().any(|a| kind(a) == "delete") {
        if actions.last().map(kind).as_deref() != Some("delete") {
            return Err(iae(format!("State={name} must not contain an action after a delete action")));
        }
        if !transitions.is_empty() {
            return Err(iae(format!("State={name} cannot contain transitions if using delete action")));
        }
    }
    let mut m = Map::new();
    m.insert("name".into(), Value::Str(name));
    m.insert("actions".into(), Value::Arr(actions));
    m.insert("transitions".into(), Value::Arr(transitions));
    Ok(m)
}

/// The action types of `ISMActionsParser` (the bundled ones) with the name their parser
/// reports unknown fields in.
const ACTIONS: &[(&str, &str)] = &[
    ("alias", "AliasAction"),
    ("allocation", "AllocationAction"),
    ("close", "CloseAction"),
    ("delete", "DeleteAction"),
    ("force_merge", "ForceMergeActionConfig"),
    ("index_priority", "IndexPriorityAction"),
    ("notification", "NotificationAction"),
    ("open", "OpenAction"),
    ("read_only", "ReadOnlyAction"),
    ("read_write", "ReadWriteAction"),
    ("replica_count", "ReplicaCountAction"),
    ("rollover", "RolloverAction"),
    ("rollup", "RollupAction"),
    ("shrink", "ShrinkAction"),
    ("snapshot", "SnapshotAction"),
    ("transform", "TransformAction"),
    ("search_only", "SearchOnlyAction"),
    ("stop_replication", "StopReplicationAction"),
    ("convert_index_to_remote", "ConvertIndexToRemoteAction"),
];

fn parse_action(p: &mut XParser<'_>) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    let (mut timeout, mut retry, mut action) = (None, None, None::<(String, Value)>);
    while let Some(field) = next_field(p) {
        match field.as_str() {
            "timeout" => timeout = Some(time_text(p, "timeout")?),
            "retry" => retry = Some(parse_retry(p)?),
            other => {
                let Some(&(_, class)) = ACTIONS.iter().find(|(n, _)| *n == other) else {
                    return Err(invalid(other, "Actions"));
                };
                // Several action types in one object: the last one stands.
                action = Some((other.to_string(), parse_action_config(p, other, class)?));
            }
        }
    }
    let (name, config) = action.ok_or_else(|| iae("Action inside state is null"))?;
    let mut m = Map::new();
    if let Some(t) = timeout {
        m.insert("timeout".into(), Value::Str(t));
    }
    m.insert("retry".into(), retry.unwrap_or_else(|| obj! { "count" => 3, "backoff" => "exponential", "delay" => "1m" }));
    m.insert(name, config);
    Ok(Value::Obj(m))
}

fn parse_retry(p: &mut XParser<'_>) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    let (mut count, mut backoff, mut delay) = (None::<i64>, "exponential".to_string(), "1m".to_string());
    while let Some(field) = next_field(p) {
        match field.as_str() {
            "count" => count = Some(p.long_value()?),
            "backoff" => {
                let b = text(p)?.to_ascii_uppercase();
                if !matches!(b.as_str(), "EXPONENTIAL" | "CONSTANT" | "LINEAR") {
                    return Err(iae(format!("No enum constant org.opensearch.indexmanagement.spi.indexstatemanagement.model.ActionRetry.Backoff.{b}")));
                }
                backoff = b.to_ascii_lowercase();
            }
            "delay" => delay = time_text(p, "delay")?,
            other => return Err(invalid(other, "ActionRetry")),
        }
    }
    let count = count.ok_or_else(|| iae("ActionRetry count is null"))?;
    if count < 0 {
        return Err(iae("Count for ActionRetry must be a non-negative number"));
    }
    Ok(obj! { "count" => count, "backoff" => backoff, "delay" => delay })
}

/// One action's configuration in the plugin's output form.
fn parse_action_config(p: &mut XParser<'_>, name: &str, class: &str) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    // Actions without parameters: `{}` exactly.
    if matches!(name, "delete" | "read_only" | "read_write" | "open" | "close" | "stop_replication" | "search_only") {
        p.next_token();
        expect(p, Tk::EndObject)?;
        return Ok(Value::Obj(Map::new()));
    }
    let mut fields: Vec<(String, Value)> = Vec::new();
    while let Some(field) = next_field(p) {
        let v = match (name, field.as_str()) {
            ("rollover", "min_size" | "min_primary_shard_size") => Value::Str(bytes_text(p, &field)?),
            ("rollover", "min_index_age") => Value::Str(time_text(p, &field)?),
            ("rollover", "min_doc_count") => Value::from(p.long_value()?),
            ("rollover", "copy_alias") => Value::Bool(p.boolean_value()?),
            ("replica_count", "number_of_replicas") | ("force_merge", "max_num_segments") | ("index_priority", "priority") => Value::from(p.long_value()?),
            // `AllocationAction`: maps of text values (`xcp.text()`, so numbers and booleans
            // become strings).
            ("allocation", "require" | "include" | "exclude") => allocation_map(p)?,
            ("allocation", "wait_for") | ("shrink", "switch_aliases" | "force_unsafe") => Value::Bool(p.boolean_value()?),
            ("shrink", "num_new_shards") => Value::from(p.long_value()?),
            ("shrink", "max_shard_size") => Value::Str(bytes_text(p, &field)?),
            ("shrink", "percentage_of_source_shards") => Value::from(p.double_value()?),
            ("shrink", "target_index_name_template") | ("notification", "message_template") => template(p)?,
            ("snapshot" | "convert_index_to_remote", "repository" | "snapshot") | ("convert_index_to_remote", "ignore_index_settings" | "rename_pattern") => Value::Str(text(p)?),
            ("convert_index_to_remote", "include_aliases" | "delete_original_index") => Value::Bool(p.boolean_value()?),
            ("convert_index_to_remote", "number_of_replicas") => Value::from(p.long_value()?),
            ("shrink", "aliases") | ("notification", "destination" | "channel") => p.take_value(),
            ("alias", "actions") => alias_actions(p.take_value()),
            ("rollup", "ism_rollup") => rollup(p.take_value()),
            ("transform", _) => p.take_value(),
            _ => return Err(invalid(&field, class)),
        };
        fields.push((field, v));
    }
    let get = |k: &str| fields.iter().rev().find(|(f, _)| f == k).map(|(_, v)| v.clone());
    let mut m = Map::new();
    let put = |m: &mut Map, k: &str, v: Option<Value>| {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    };
    match name {
        "rollover" => {
            for k in ["min_size", "min_doc_count", "min_index_age", "min_primary_shard_size"] {
                put(&mut m, k, get(k));
            }
            put(&mut m, "copy_alias", Some(get("copy_alias").unwrap_or(Value::Bool(false))));
        }
        "snapshot" => {
            let repository = get("repository").ok_or_else(|| iae("SnapshotAction repository must be specified"))?;
            let snapshot = get("snapshot").ok_or_else(|| iae("SnapshotAction snapshot must be specified"))?;
            put(&mut m, "repository", Some(repository));
            put(&mut m, "snapshot", Some(snapshot));
        }
        "convert_index_to_remote" => {
            let repository = get("repository").ok_or_else(|| iae("ConvertIndexToRemoteAction repository must be specified"))?;
            let snapshot = get("snapshot").ok_or_else(|| iae("ConvertIndexToRemoteAction snapshot must be specified"))?;
            put(&mut m, "repository", Some(repository));
            put(&mut m, "snapshot", Some(snapshot));
            put(&mut m, "include_aliases", Some(get("include_aliases").unwrap_or(Value::Bool(false))));
            put(&mut m, "ignore_index_settings", Some(get("ignore_index_settings").unwrap_or_else(|| Value::Str(String::new()))));
            // A negative count means none.
            put(&mut m, "number_of_replicas", get("number_of_replicas").filter(|n| n.as_i64().is_some_and(|n| n >= 0)));
            put(&mut m, "delete_original_index", Some(get("delete_original_index").unwrap_or(Value::Bool(false))));
            put(&mut m, "rename_pattern", get("rename_pattern"));
        }
        "index_priority" => {
            let n = get("priority").ok_or_else(|| iae("priority is null"))?;
            if n.as_i64().is_some_and(|n| n < 0) {
                return Err(iae("IndexPriorityAction index_priority value must be a non-negative number"));
            }
            put(&mut m, "priority", Some(n));
        }
        "transform" => {
            if get("ism_transform").is_none() {
                return Err(iae("TransformAction transform is null."));
            }
            for (k, v) in &fields {
                m.insert(k.clone(), v.clone());
            }
        }
        "rollup" => {
            let r = get("ism_rollup").ok_or_else(|| iae("RollupAction rollup is null"))?;
            put(&mut m, "ism_rollup", Some(r));
        }
        "alias" => {
            // `AliasAction.init`.
            let actions = get("actions").and_then(|a| a.as_array().cloned()).unwrap_or_default();
            if actions.is_empty() {
                return Err(iae("At least one alias action needs to be specified."));
            }
            let body = |a: &Value| a.as_object().and_then(|o| o.iter().next().map(|(k, v)| (k.clone(), v.clone())));
            if !actions.iter().all(|a| body(a).is_some_and(|(k, _)| k == "add" || k == "remove")) {
                return Err(iae("Only ADD and REMOVE actions are allowed."));
            }
            let has = |a: &Value, k: &str| body(a).is_some_and(|(_, b)| b.get(k).is_some_and(|v| !matches!(v, Value::Null) && v.as_array().is_none_or(|l| !l.is_empty())));
            if actions.iter().any(|a| has(a, "index") || has(a, "indices")) {
                return Err(iae("Alias action can only work on its applied index so don't accept index/indices parameter."));
            }
            if !actions.iter().all(|a| has(a, "aliases") || has(a, "alias")) {
                return Err(iae("At least one alias needs to be specified."));
            }
            put(&mut m, "actions", Some(Value::Arr(actions)));
        }
        "replica_count" => {
            let Some(n) = get("number_of_replicas") else {
                // The plugin's message names the Kotlin companion object, hash code included.
                return Err(iae("org.opensearch.indexmanagement.indexstatemanagement.action.ReplicaCountAction$Companion@eed13cd.NUMBER_OF_REPLICAS_FIELD is null"));
            };
            if n.as_i64().is_some_and(|n| n < 0) {
                return Err(iae("ReplicaCountAction number_of_replicas value must be a non-negative number"));
            }
            put(&mut m, "number_of_replicas", Some(n));
        }
        "force_merge" => {
            let Some(n) = get("max_num_segments") else { return Err(iae("ForceMergeActionConfig maxNumSegments is null")) };
            if n.as_i64().is_some_and(|n| n <= 0) {
                return Err(iae("Force merge {max_num_segments} must be greater than 0"));
            }
            put(&mut m, "max_num_segments", Some(n));
        }
        "allocation" => {
            for k in ["require", "include", "exclude"] {
                put(&mut m, k, Some(get(k).unwrap_or_else(|| Value::Obj(Map::new()))));
            }
            put(&mut m, "wait_for", Some(get("wait_for").unwrap_or(Value::Bool(false))));
        }
        "shrink" => {
            // `ShrinkAction.init`.
            let (num, max, pct) = (get("num_new_shards"), get("max_shard_size"), get("percentage_of_source_shards"));
            if [&num, &max, &pct].iter().filter(|v| v.is_some()).count() != 1 {
                return Err(iae("Exactly one option specifying the number of shards to shrink to must be used."));
            }
            if let Some(Value::Str(b)) = &max
                && crate::units::ByteSize::parse(b, "max_shard_size").is_ok_and(|b| b.bytes() <= 0)
            {
                return Err(iae("Shrink action maxShardSize must be greater than 0."));
            }
            if let Some(p) = pct.as_ref().and_then(Value::as_f64)
                && !(p > 0.0 && p < 1.0)
            {
                return Err(iae("Percentage of source shards must be between 0.0 and 1.0 exclusively"));
            }
            if num.as_ref().and_then(Value::as_i64).is_some_and(|n| n <= 0) {
                return Err(iae("Shrink action numNewShards must be greater than 0."));
            }
            for k in ["num_new_shards", "max_shard_size", "percentage_of_source_shards", "target_index_name_template", "aliases"] {
                put(&mut m, k, get(k));
            }
            put(&mut m, "switch_aliases", Some(get("switch_aliases").unwrap_or(Value::Bool(false))));
            put(&mut m, "force_unsafe", get("force_unsafe"));
        }
        "notification" => {
            if get("message_template").is_none() {
                return Err(iae("NotificationAction message template is null"));
            }
            if get("destination").is_none() && get("channel").is_none() {
                return Err(iae("Notification must contain a destination or channel"));
            }
            for k in ["destination", "channel", "message_template"] {
                put(&mut m, k, get(k));
            }
        }
        _ => {
            for (k, v) in &fields {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(Value::Obj(m))
}

/// An allocation filter map (`require`/`include`/`exclude`): every value as text.
fn allocation_map(p: &mut XParser<'_>) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    let mut m = Map::new();
    while let Some(k) = next_field(p) {
        if matches!(p.current_token(), Some(Tk::StartArray | Tk::StartObject)) {
            let (l, c) = p.token_location();
            let tok = if p.current_token() == Some(Tk::StartArray) { "START_ARRAY" } else { "START_OBJECT" };
            return Err(EsError::java(500, "illegal_state_exception", format!("Can't get text on a {tok} at {l}:{c}")));
        }
        m.insert(k, Value::Str(text(p)?));
    }
    Ok(Value::Obj(m))
}

/// `alias` actions: `alias` becomes the `aliases` list.
fn alias_actions(v: Value) -> Value {
    let Value::Arr(list) = v else { return v };
    let op = |(name, body): (String, Value)| match body {
        Value::Obj(mut b) => {
            if let Some(one) = b.shift_remove("alias") {
                let mut out = Map::new();
                out.insert("aliases".into(), Value::Arr(vec![one]));
                out.extend(b);
                b = out;
            }
            (name, Value::Obj(b))
        }
        other => (name, other),
    };
    Value::Arr(
        list.into_iter()
            .map(|a| match a {
                Value::Obj(m) => Value::Obj(m.into_iter().map(op).collect()),
                other => other,
            })
            .collect(),
    )
}

/// An `ism_rollup`: date histogram dimensions get their defaults.
fn rollup(v: Value) -> Value {
    let Value::Obj(mut m) = v else { return v };
    if let Some(Value::Arr(dims)) = m.get_mut("dimensions") {
        for d in dims.iter_mut() {
            if let Some(Value::Obj(h)) = d.as_object_mut().and_then(|m| m.get_mut("date_histogram")) {
                let get = |k: &str| h.get(k).cloned();
                let mut out = Map::new();
                for k in ["fixed_interval", "calendar_interval"] {
                    if let Some(v) = get(k) {
                        out.insert(k.into(), v);
                    }
                }
                let source = get("source_field").unwrap_or(Value::Null);
                out.insert("source_field".into(), source.clone());
                out.insert("target_field".into(), get("target_field").unwrap_or(source));
                out.insert("timezone".into(), get("timezone").unwrap_or(Value::Str("UTC".into())));
                out.insert("format".into(), get("format").unwrap_or(Value::Null));
                *h = out;
            }
        }
    }
    Value::Obj(m)
}

fn parse_transition(p: &mut XParser<'_>) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    let (mut state, mut conditions) = (None, None);
    while let Some(field) = next_field(p) {
        match field.as_str() {
            "state_name" => state = Some(text(p)?),
            "conditions" => conditions = Some(parse_conditions(p)?),
            other => return Err(invalid(other, "Transition")),
        }
    }
    let state = state.ok_or_else(|| iae("Transition state name is null"))?;
    let mut m = Map::new();
    m.insert("state_name".into(), Value::Str(state));
    if let Some(c) = conditions {
        m.insert("conditions".into(), c);
    }
    Ok(Value::Obj(m))
}

fn parse_conditions(p: &mut XParser<'_>) -> EsResult<Value> {
    expect(p, Tk::StartObject)?;
    let mut m = Map::new();
    while let Some(field) = next_field(p) {
        let v = match field.as_str() {
            "min_index_age" | "min_rollover_age" => Value::Str(time_text(p, &field)?),
            "min_doc_count" => Value::from(p.long_value()?),
            "min_size" => Value::Str(bytes_text(p, &field)?),
            "cron" => p.take_value(),
            other => return Err(invalid(other, "Conditions")),
        };
        m.insert(field, v);
    }
    if m.len() > 1 {
        return Err(iae("Cannot provide more than one Transition condition"));
    }
    Ok(Value::Obj(m))
}

// ------------------------------------------------------------------------------ storage

/// The config index's next sequence number (advanced).
pub(super) fn next_seq(st: &mut ClusterState) -> i64 {
    let n = st.metadata.xpack.get(META, "seq").and_then(Value::as_i64).unwrap_or(0);
    store::family_mut(st, META).insert("seq".into(), Value::from(n + 1));
    n
}

fn ensure_config_index(st: &mut ClusterState, now: i64) -> EsResult<()> {
    store::ensure_index(st, CONFIG_INDEX, None, now)
}

fn config_exists(st: &ClusterState) -> bool {
    st.metadata.indices.contains_key(CONFIG_INDEX)
}

fn config_uuid(st: &ClusterState) -> String {
    st.metadata.indices.get(CONFIG_INDEX).map(|i| i.uuid.clone()).unwrap_or_else(|| "_na_".into())
}

/// A stored policy: its document metadata and normalized body.
pub(super) struct Stored {
    pub(super) seq_no: i64,
    version: i64,
    pub(super) policy: Map,
}

pub(super) fn stored(st: &ClusterState, id: &str) -> Option<Stored> {
    let v = st.metadata.xpack.get(POLICIES, id)?;
    Some(Stored {
        seq_no: v.get("seq_no").and_then(Value::as_i64).unwrap_or(0),
        version: v.get("version").and_then(Value::as_i64).unwrap_or(1),
        policy: v.get("policy").and_then(Value::as_object).cloned().unwrap_or_default(),
    })
}

/// The job's own copy of its policy (`ManagedIndexConfig.policy`): a managed index keeps
/// running the policy version it was given, whatever happens to the policy later. Copies are
/// kept once per policy version (`ism.meta` `policy:<id>:<seq_no>`) and shared by every
/// managed index on that version, so a thousand managed indices don't make a thousand copies
/// of the cluster state's largest objects.
pub(super) fn job_policy<'a>(st: &'a ClusterState, rec: &Value) -> Option<&'a Map> {
    let id = rec.get("policy_id").and_then(Value::as_str)?;
    let seq = rec.get("policy_seq_no").and_then(Value::as_i64)?;
    st.metadata.xpack.get(META, &format!("policy:{id}:{seq}")).and_then(Value::as_object)
}

/// Keeps a copy of the policy's current version for jobs: its sequence number.
pub(super) fn keep_policy_copy(st: &mut ClusterState, id: &str) -> Option<i64> {
    let s = stored(st, id)?;
    let key = format!("policy:{id}:{}", s.seq_no);
    if st.metadata.xpack.get(META, &key).is_none() {
        store::family_mut(st, META).insert(key, Value::Obj(s.policy));
    }
    Some(s.seq_no)
}

/// Drops policy copies no job uses any more.
pub(super) fn gc_policy_copies(st: &mut ClusterState) {
    let used: std::collections::HashSet<String> = st
        .metadata
        .xpack
        .iter(MANAGED)
        .filter_map(|(_, r)| Some(format!("policy:{}:{}", r.get("policy_id")?.as_str()?, r.get("policy_seq_no")?.as_i64()?)))
        .collect();
    let stale: Vec<String> = st.metadata.xpack.iter(META).map(|(k, _)| k).filter(|k| k.starts_with("policy:") && !used.contains(*k)).cloned().collect();
    if !stale.is_empty() {
        let fam = store::family_mut(st, META);
        for k in stale {
            fam.remove(&k);
        }
    }
}

/// A policy as `GET` prints it (the current schema version).
fn as_read(mut policy: Map) -> Value {
    policy.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
    Value::Obj(policy)
}

/// `ISMTemplateService.overlapping`: whether two lists of simple (`*`) patterns, each read
/// as the union of its patterns' languages (`Regex.simpleMatchToAutomaton`, an exclusion's
/// `-` taken literally), share a name.
fn overlapping(a: &[&str], b: &[&str]) -> bool {
    a.iter().any(|x| b.iter().any(|y| globs_intersect(x.as_bytes(), y.as_bytes())))
}

/// Whether some string matches both `*`-patterns: a walk over pairs of positions (a `*`
/// either ends or takes one more character the other side produces), memoized.
fn globs_intersect(a: &[u8], b: &[u8]) -> bool {
    let (n, m) = (a.len(), b.len());
    // 0 unknown, 1 no, 2 yes; positions past the end included.
    let mut memo = vec![0u8; (n + 1) * (m + 1)];
    fn go(a: &[u8], b: &[u8], i: usize, j: usize, memo: &mut [u8]) -> bool {
        let k = i * (b.len() + 1) + j;
        if memo[k] != 0 {
            return memo[k] == 2;
        }
        let r = match (a.get(i), b.get(j)) {
            (None, None) => true,
            (Some(b'*'), _) => go(a, b, i + 1, j, memo) || (j < b.len() && go(a, b, i, j + 1, memo)),
            (_, Some(b'*')) => go(a, b, i, j + 1, memo) || (i < a.len() && go(a, b, i + 1, j, memo)),
            (Some(x), Some(y)) => x == y && go(a, b, i + 1, j + 1, memo),
            _ => false,
        };
        memo[k] = if r { 2 } else { 1 };
        r
    }
    go(a, b, 0, 0, &mut memo)
}

fn glob(pattern: &str, name: &str) -> bool {
    crate::xcontent::filter::glob_match(pattern, name)
}

/// `ManagedIndexCoordinator.matchesIndexPatterns`: some inclusion pattern matches and no
/// exclusion (`-pattern`) does.
fn matches_patterns<'a>(name: &str, patterns: impl Iterator<Item = &'a str> + Clone) -> bool {
    patterns.clone().any(|p| !p.starts_with('-') && glob(p, name)) && !patterns.filter_map(|p| p.strip_prefix('-')).any(|p| glob(p, name))
}

/// `ISMTemplateService.validateFormat` over every pattern of a policy's templates.
fn validate_format(patterns: &[&str]) -> Option<EsError> {
    let mut errors: Vec<String> = Vec::new();
    let mut inclusion = false;
    for &pat in patterns {
        let exclusion = pat.starts_with('-');
        inclusion |= !exclusion;
        let body = if exclusion { &pat[1..] } else { pat };
        if exclusion && body.is_empty() {
            errors.push(format!("index_pattern [{pat}] must have content after '-' exclusion prefix"));
        }
        if body.contains('#') {
            errors.push(format!("index_pattern [{pat}] must not contain a '#'"));
        }
        if body.contains(':') {
            errors.push(format!("index_pattern [{pat}] must not contain a ':'"));
        }
        if body.starts_with('_') {
            errors.push(format!("index_pattern [{pat}] must not start with '_'"));
        }
        if body.chars().any(|c| matches!(c, '\\' | '/' | '?' | '"' | '<' | '>' | '|' | ' ' | ',')) {
            errors.push(format!("index_pattern [{pat}] must not contain the following characters [ , \", *, \\, <, |, ,, >, /, ?]"));
        }
    }
    if !inclusion {
        errors.push("index_patterns must contain at least one inclusion pattern (patterns cannot be all exclusions)".into());
    }
    if errors.is_empty() {
        return None;
    }
    let msg = errors.iter().enumerate().fold(String::from("Validation Failed: "), |mut s, (i, e)| {
        s.push_str(&format!("{}: {e};", i + 1));
        s
    });
    let cause = EsError::java(400, "exception", format!("org.opensearch.common.ValidationException: {msg}"));
    Some(EsError::new(400, "index_management_exception", msg).caused_by(cause))
}

/// `IndexManagementException.wrap` of an `IllegalArgumentException`.
fn management_iae(msg: String) -> EsError {
    let cause = EsError::java(400, "exception", format!("java.lang.IllegalArgumentException: {msg}"));
    EsError::new(400, "index_management_exception", msg).caused_by(cause)
}

fn template_patterns(t: &Value) -> Vec<&str> {
    t.get("index_patterns").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect()
}

/// `ISMTemplateService.findSelfConflictingTemplates`: two templates of the policy with one
/// priority whose patterns overlap (the first template's patterns, then the later ones').
fn self_conflict(id: &str, templates: &Value) -> Option<EsError> {
    let list = templates.as_array()?;
    let prio = |t: &Value| t.get("priority").and_then(Value::as_i64).unwrap_or(0);
    // Priorities in first-seen order (`mutableMapOf` keeps insertion order).
    let mut prios: Vec<i64> = Vec::new();
    for t in list {
        if !prios.contains(&prio(t)) {
            prios.push(prio(t));
        }
    }
    for pr in prios {
        let group: Vec<Vec<&str>> = list.iter().filter(|t| prio(t) == pr).map(template_patterns).collect();
        if group.len() < 2 {
            continue;
        }
        for (i, pats) in group.iter().enumerate() {
            let rest: Vec<&str> = group[i + 1..].iter().flatten().copied().collect();
            if overlapping(pats, &rest) {
                return Some(management_iae(format!(
                    "New policy {id} has an ISM template with index pattern [{}] matching this policy's other ISM templates with index patterns [{}], please use different priority",
                    pats.join(", "),
                    rest.join(", ")
                )));
            }
        }
    }
    None
}

/// `ISMTemplateService.findConflictingPolicyTemplates`: another policy's template with the
/// same priority and an overlapping pattern.
fn template_conflict(st: &ClusterState, id: &str, templates: &Value) -> Option<EsError> {
    for t in templates.as_array()? {
        let prio = t.get("priority").and_then(Value::as_i64).unwrap_or(0);
        let pats = template_patterns(t);
        for (other, v) in st.metadata.xpack.iter(POLICIES) {
            if other == id {
                continue;
            }
            for ot in v.get("policy").and_then(|p| p.get("ism_template")).and_then(Value::as_array).into_iter().flatten() {
                let op = ot.get("priority").and_then(Value::as_i64).unwrap_or(0);
                if op == prio && overlapping(&pats, &template_patterns(ot)) {
                    return Some(management_iae(format!(
                        "New policy {id} has an ISM template with index pattern [{}] matching existing policy templates, please use a different priority than {prio}",
                        pats.join(", ")
                    )));
                }
            }
        }
    }
    None
}

// ------------------------------------------------------------------------------ policies

async fn put_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("policy_id").unwrap_or_default().to_string();
    let Some(body) = ctx.body()? else { return Err(EsError::new(400, "parse_exception", "request body is required")) };
    let now = ctx.now_ms;
    let policy = {
        let mut p = XParser::new(&body, ctx.raw_json(), Vec::new());
        parse_request(&mut p, &id, now)?
    };
    let if_seq = ctx.query("if_seq_no").map(|s| s.parse::<i64>().unwrap_or(-2));
    let if_term = ctx.query("if_primary_term").map(|s| s.parse::<i64>().unwrap_or(0));
    let pid = id.clone();
    let stored_policy = policy.clone();
    let outcome = ctx
        .cluster
        .update(move |st| {
            // `checkAndUpdateIMConfigIndex` creates the config index before the templates are
            // validated: a refused policy still leaves it behind.
            ensure_config_index(st, now)?;
            let templates = &stored_policy["ism_template"];
            if !matches!(templates, Value::Null) {
                let all: Vec<&str> = templates.as_array().into_iter().flatten().flat_map(template_patterns).collect();
                if let Some(e) = validate_format(&all).or_else(|| self_conflict(&pid, templates)).or_else(|| template_conflict(st, &pid, templates)) {
                    return Ok(Err(e));
                }
            }
            let existing = stored(st, &pid);
            let conflict = |st: &ClusterState, reason: String| {
                EsError::new(409, "version_conflict_engine_exception", reason).with_meta("index", CONFIG_INDEX).with_meta("shard", "0").with_meta("index_uuid", config_uuid(st))
            };
            let version = match (&existing, if_seq, if_term) {
                (Some(e), Some(s), Some(t)) if s == e.seq_no && t == 1 => e.version + 1,
                (Some(e), Some(s), Some(t)) => {
                    return Ok(Err(conflict(
                        st,
                        format!("[{pid}]: version conflict, required seqNo [{s}], primary term [{t}]. current document has seqNo [{}] and primary term [1]", e.seq_no),
                    )));
                }
                (Some(e), _, _) => return Ok(Err(conflict(st, format!("[{pid}]: version conflict, document already exists (current version [{}])", e.version)))),
                (None, Some(s), Some(t)) => {
                    return Ok(Err(conflict(st, format!("[{pid}]: version conflict, required seqNo [{s}], primary term [{t}]. but no document was found"))));
                }
                // A policy deleted within `index.gc_deletes` (60s) keeps counting versions: its
                // delete tombstone still holds the last one.
                (None, _, _) => tombstone(st, &pid, now).map_or(1, |v| v + 1),
            };
            let seq = next_seq(st);
            store::put(st, POLICIES, &pid, obj! { "seq_no" => seq, "version" => version, "policy" => Value::Obj(stored_policy) }, now)?;
            Ok(Ok((existing.is_none(), seq, version)))
        })
        .await?;
    let (created, seq, version) = outcome?;
    let out = obj! {
        "_id" => id,
        "_version" => version,
        "_primary_term" => 1,
        "_seq_no" => seq,
        "policy" => obj! { "policy" => Value::Obj(policy) },
    };
    Ok(Reply::Body(if created { 201 } else { 200 }, out))
}

fn get_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("policy_id").unwrap_or_default();
    let st = ctx.state();
    if !config_exists(&st) {
        return Err(config_index_missing());
    }
    let Some(s) = stored(&st, id) else { return Err(status(404, "Policy not found")) };
    Reply::ok(obj! { "_id" => id, "_version" => s.version, "_seq_no" => s.seq_no, "_primary_term" => 1, "policy" => as_read(s.policy) })
}

fn get_policies(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let params = search_params(ctx, "policy.policy_id.keyword")?;
    let key = sort_key(ctx, &st, &params.sort_field, Doc::Policy)?;
    let mut list: Vec<Stored> = st.metadata.xpack.iter(POLICIES).filter(|(id, _)| query_matches(&params.query, id)).filter_map(|(id, _)| stored(&st, id)).collect();
    let id_of = |s: &Stored| s.policy.get("policy_id").and_then(Value::as_str).unwrap_or_default().to_string();
    list.sort_by(|a, b| {
        let (ka, kb) = (policy_sort_value(&a.policy, key), policy_sort_value(&b.policy, key));
        // Ties keep the config index's document order (the order of the last writes).
        sort_cmp(&ka, &kb, params.desc).then_with(|| a.seq_no.cmp(&b.seq_no))
    });
    let total = list.len();
    let policies: Vec<Value> = list
        .into_iter()
        .skip(params.from)
        .take(params.size)
        .map(|s| obj! { "_id" => id_of(&s), "_seq_no" => s.seq_no, "_primary_term" => 1, "policy" => as_read(s.policy) })
        .collect();
    Reply::ok(obj! { "policies" => Value::Arr(policies), "total_policies" => total })
}

/// `RestRequest.getSearchParams`: the paging, sort and `queryString` of the list APIs.
struct SearchParams {
    size: usize,
    from: usize,
    sort_field: String,
    desc: bool,
    query: String,
}

/// Parses [`SearchParams`] as the plugin builds its config index search: the ints first
/// (`paramAsInt`), then the sort order (`SortOrder.fromString`), then `from` and `size`
/// (`SearchSourceBuilder`).
fn search_params(ctx: &Ctx<'_>, default_sort: &str) -> EsResult<SearchParams> {
    let int = |k: &str, default: i32| -> EsResult<i32> {
        match ctx.query(k) {
            None => Ok(default),
            Some(v) => v.parse::<i32>().map_err(|_| {
                EsError::java(400, "illegal_argument_exception", format!("Failed to parse int parameter [{k}] with value [{v}]"))
                    .caused_by(EsError::java(400, "number_format_exception", format!("For input string: \"{v}\"")))
            }),
        }
    };
    let (size, from) = (int("size", 20)?, int("from", 0)?);
    let order = ctx.query("sortOrder").unwrap_or("asc").to_uppercase();
    let desc = match order.as_str() {
        "ASC" => false,
        "DESC" => true,
        other => return Err(iae(format!("No enum constant org.opensearch.search.sort.SortOrder.{other}"))),
    };
    if from < 0 {
        return Err(iae(format!("[from] parameter cannot be negative, found [{from}]")));
    }
    if size < 0 {
        return Err(iae(format!("[size] parameter cannot be negative, found [{size}]")));
    }
    Ok(SearchParams {
        size: size as usize,
        from: from as usize,
        sort_field: ctx.query("sortField").unwrap_or(default_sort).to_string(),
        desc,
        query: ctx.query("queryString").unwrap_or("*").to_string(),
    })
}

/// The config index document type a search lists.
#[derive(Clone, Copy, PartialEq)]
enum Doc {
    Policy,
    Managed,
}

/// A sortable field of the config index mapping (`opendistro-ism-config.json`) and which
/// document type holds it (the other type's documents all miss it).
#[derive(Clone, Copy)]
enum SortKey {
    PolicyId,
    Description,
    DefaultState,
    PolicyUpdated,
    SchemaVersion,
    Index,
    IndexUuid,
    ManagedPolicyId,
    Enabled,
    /// A mapped field the listed documents don't hold (every value missing).
    Missing,
}

/// The sort key of `field`, or the shard failure a search sorting on it gets.
fn sort_key(ctx: &Ctx<'_>, st: &ClusterState, field: &str, doc: Doc) -> EsResult<SortKey> {
    let node = ctx.cluster.node.id.as_str();
    let key = match field {
        "policy.policy_id.keyword" => SortKey::PolicyId,
        "policy.description.keyword" => SortKey::Description,
        "policy.default_state" => SortKey::DefaultState,
        "policy.last_updated_time" => SortKey::PolicyUpdated,
        "policy.schema_version" => SortKey::SchemaVersion,
        "managed_index.index" => SortKey::Index,
        "managed_index.index_uuid" => SortKey::IndexUuid,
        "managed_index.policy_id" => SortKey::ManagedPolicyId,
        "managed_index.enabled" => SortKey::Enabled,
        "managed_index.name.keyword" => SortKey::Index,
        "managed_index.enabled_time" | "managed_index.last_updated_time" | "managed_index.jitter" | "managed_index.policy_seq_no" | "managed_index.policy_primary_term" => SortKey::Missing,
        "policy.policy_id" | "policy.description" | "managed_index.name" => {
            let e = EsError::illegal_argument(crate::search::aggs::fielddata_disabled_in(true, field, CONFIG_INDEX));
            return Err(config_search_failure(node, e.clone().caused_by(e)));
        }
        _ => {
            let e = EsError::new(400, "query_shard_exception", format!("No mapping found for [{field}] in order to sort on")).with_meta("index", CONFIG_INDEX).with_meta("index_uuid", config_uuid(st));
            return Err(config_search_failure(node, e));
        }
    };
    Ok(match (key, doc) {
        (SortKey::PolicyId | SortKey::Description | SortKey::DefaultState | SortKey::PolicyUpdated | SortKey::SchemaVersion, Doc::Managed) => SortKey::Missing,
        (SortKey::Index | SortKey::IndexUuid | SortKey::ManagedPolicyId | SortKey::Enabled, Doc::Policy) => SortKey::Missing,
        (k, _) => k,
    })
}

/// A failure of the plugin's search of its config index (one shard, all failed).
fn config_search_failure(node: &str, cause: EsError) -> EsError {
    let root = match &cause.caused_by {
        Some(c) if cause.kind == "illegal_argument_exception" => (**c).clone(),
        _ => cause.clone(),
    };
    let shard = obj! { "shard" => 0, "index" => CONFIG_INDEX, "node" => node, "reason" => root.to_value(false) };
    EsError::new(400, "search_phase_execution_exception", "all shards failed")
        .with_meta("phase", "query")
        .with_meta("grouped", true)
        .with_meta("failed_shards", Value::Arr(vec![shard]))
        .with_root_causes(vec![root])
        .caused_by(cause)
}

/// A sort value: missing values sort last whatever the order (`_last`).
#[derive(PartialEq, PartialOrd)]
enum SortValue {
    Num(i64),
    Text(String),
    Missing,
}

fn sort_cmp(a: &SortValue, b: &SortValue, desc: bool) -> std::cmp::Ordering {
    match (a, b) {
        (SortValue::Missing, SortValue::Missing) => std::cmp::Ordering::Equal,
        (SortValue::Missing, _) => std::cmp::Ordering::Greater,
        (_, SortValue::Missing) => std::cmp::Ordering::Less,
        _ => {
            let o = a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
            if desc { o.reverse() } else { o }
        }
    }
}

fn policy_sort_value(policy: &Map, key: SortKey) -> SortValue {
    let text = |k: &str| policy.get(k).and_then(Value::as_str).map_or(SortValue::Missing, |s| SortValue::Text(s.to_string()));
    match key {
        SortKey::PolicyId => text("policy_id"),
        SortKey::Description => text("description"),
        SortKey::DefaultState => text("default_state"),
        SortKey::PolicyUpdated => policy.get("last_updated_time").and_then(Value::as_i64).map_or(SortValue::Missing, SortValue::Num),
        SortKey::SchemaVersion => policy.get("schema_version").and_then(Value::as_i64).map_or(SortValue::Missing, SortValue::Num),
        _ => SortValue::Missing,
    }
}

/// A Lucene wildcard (`*` any run, `?` one character) matching all of `s`.
fn wildcard(p: &[u8], s: &[u8]) -> bool {
    // Greedy with backtracking to the last `*`: linear in practice, no allocation.
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < s.len() {
        if i < p.len() && (p[i] == b'?' || p[i] == s[j]) && p[i] != b'*' {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(st) = star {
            i = st + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == b'*')
}

/// `queryString`: a `query_string` query (`AND` by default) over one keyword field of the
/// config index (a policy's id, a managed index's name): each term matches the whole value,
/// `*` and `?` as wildcards, case-sensitively; `OR` joins alternatives, `AND` and `NOT`/`-`
/// work as in Lucene's classic syntax.
fn query_matches(q: &str, value: &str) -> bool {
    let q = q.trim();
    if q.is_empty() || q == "*" {
        return true;
    }
    let term = |t: &str| {
        let t = t.trim_matches('"');
        let t = t.split_once(':').map_or(t, |(_, v)| v);
        if t.contains(['*', '?']) { wildcard(t.as_bytes(), value.as_bytes()) } else { t == value }
    };
    // Clauses joined by `OR` (any) of `AND`-joined terms (all; a `-`/`NOT` term must not match).
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut groups: Vec<Vec<(bool, &str)>> = vec![Vec::new()];
    let mut negate = false;
    for w in words {
        match w {
            "OR" | "||" => groups.push(Vec::new()),
            "AND" | "&&" => {}
            "NOT" | "!" => negate = true,
            _ => {
                let (neg, t) = match w.strip_prefix('-').or_else(|| w.strip_prefix('!')) {
                    Some(t) => (true, t),
                    None => (negate, w.strip_prefix('+').unwrap_or(w)),
                };
                negate = false;
                groups.last_mut().expect("one group").push((neg, t));
            }
        }
    }
    groups.iter().filter(|g| !g.is_empty()).any(|g| g.iter().all(|(neg, t)| term(t) != *neg) && g.iter().any(|(neg, _)| !neg))
}

/// The version a deleted policy's tombstone holds, while `index.gc_deletes` keeps it.
fn tombstone(st: &ClusterState, id: &str, now: i64) -> Option<i64> {
    let t = st.metadata.xpack.get(META, &format!("tombstone:{id}"))?;
    let at = t.get("at").and_then(Value::as_i64)?;
    (now - at < 60_000).then(|| t.get("version").and_then(Value::as_i64)).flatten()
}

async fn delete_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("policy_id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    let pid = id.clone();
    let (seq, version) = ctx
        .cluster
        .update(move |st| {
            if !config_exists(st) {
                return Err(config_index_missing());
            }
            let Some(s) = stored(st, &pid) else { return Err(status(404, format!("Policy {pid} is not found"))) };
            store::remove(st, POLICIES, &pid);
            store::family_mut(st, META).insert(format!("tombstone:{pid}"), obj! { "version" => s.version + 1, "at" => now });
            Ok((next_seq(st), s.version + 1))
        })
        .await?;
    Reply::ok(obj! {
        "_index" => CONFIG_INDEX,
        "_id" => id,
        "_version" => version,
        "result" => "deleted",
        "forced_refresh" => true,
        "_shards" => obj! { "total" => 2, "successful" => 1, "failed" => 0 },
        "_seq_no" => seq,
        "_primary_term" => 1,
    })
}

// ------------------------------------------------------------------------------ managed indices

pub(super) fn managed<'a>(st: &'a ClusterState, index: &str) -> Option<&'a Value> {
    st.metadata.xpack.get(MANAGED, index)
}

/// Registers a managed index; `writes` is how many config documents the plugin writes for it
/// (an `add` writes the config and its first metadata, a template the config alone).
fn manage(st: &mut ClusterState, index: &str, uuid: &str, policy: &str, writes: usize, now: i64) -> EsResult<()> {
    ensure_config_index(st, now)?;
    // The config document's own write comes first.
    let seq = next_seq(st);
    for _ in 1..writes {
        next_seq(st);
    }
    // The job document: the policy as it is now (later policy updates don't reach it), and an
    // interval schedule starting now.
    let copy = keep_policy_copy(st, policy);
    let interval = super::ism_run::job_interval(st);
    let next = super::ism_run::next_run(st, now, interval, now);
    let rec = obj! {
        "index_uuid" => uuid,
        "policy_id" => policy,
        "enabled" => true,
        "seq_no" => seq,
        "policy_seq_no" => copy.map_or(Value::Null, Value::from),
        "enabled_time" => now,
        "schedule_start" => now,
        "job_interval" => interval,
        "next_run" => next,
    };
    store::put(st, MANAGED, index, rec, now)?;
    Ok(())
}

/// An index expression's concrete indices (with their uuids); missing names drop out unless
/// `strict`.
fn targets(ctx: &Ctx<'_>, st: &ClusterState, strict: bool) -> EsResult<Vec<(String, String)>> {
    let Some(expr) = ctx.path_param("index") else { return Err(iae("Missing indices")) };
    let exprs = crate::meta::resolve::split_expressions(expr);
    let opts = IndicesOptions { ignore_unavailable: !strict, ..IndicesOptions::STRICT_EXPAND_HIDDEN };
    let names = crate::meta::resolve::resolve(st, &exprs, &opts, ctx.now_ms)?;
    Ok(cluster_order(st, names).into_iter().filter_map(|n| st.metadata.indices.get(&n).map(|i| (n.clone(), i.uuid.clone()))).collect())
}

/// System indices a fresh OpenSearch node holds besides the config index (job scheduler lock,
/// notifications, security analytics log types, ML config, query insights, ISM history).
const OS_SYSTEM_INDICES: usize = 6;

/// Index names in the order the cluster state's index map iterates them: the plugin walks the
/// metadata's `HashMap` (sized for every index of the cluster) to resolve expressions.
fn cluster_order(st: &ClusterState, names: Vec<String>) -> Vec<String> {
    let total = st.metadata.indices.len() + OS_SYSTEM_INDICES;
    // `new HashMap<>(map)`: `tableSizeFor((int) (n / 0.75f + 1))`.
    let cap = ((total as f32 / 0.75 + 1.0) as usize).next_power_of_two();
    let order = crate::json::java_hash_order(&names, cap);
    let mut slots: Vec<Option<String>> = names.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

fn failure(index: &str, uuid: &str, reason: &str) -> Value {
    obj! { "index_name" => index, "index_uuid" => uuid, "reason" => reason }
}

fn outcome((updated, failed): (usize, Vec<Value>)) -> EsResult<Reply> {
    Reply::ok(obj! { "updated_indices" => updated, "failures" => !failed.is_empty(), "failed_indices" => Value::Arr(failed) })
}

fn policy_id_of(ctx: &Ctx<'_>, missing: &str) -> EsResult<String> {
    match ctx.body_map()?.unwrap_or_default().get("policy_id") {
        Some(Value::Str(s)) => Ok(s.clone()),
        _ => Err(iae(missing)),
    }
}

async fn add(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let policy = policy_id_of(ctx, "Missing policy_id")?;
    if ctx.path_param("index").is_none() {
        return Err(iae("Missing indices"));
    }
    let st = ctx.state();
    // Nothing to manage: done before the policy is looked up.
    let indices = targets(ctx, &st, false)?;
    if indices.is_empty() {
        return outcome((0, Vec::new()));
    }
    if stored(&st, &policy).is_none() {
        return Err(status(404, format!("Could not find policy={policy}")));
    }
    let now = ctx.now_ms;
    let r = ctx
        .cluster
        .update(move |st| {
            let (mut updated, mut failed) = (0usize, Vec::new());
            // Unmanageable indices drop out first (`getExistingManagedIndices`).
            let (refused, indices): (Vec<_>, Vec<_>) = indices.iter().partition(|(name, _)| restricted(st, name));
            for (name, uuid) in refused {
                failed.push(failure(name, uuid, "Matches restricted index pattern defined in the cluster setting"));
            }
            for (name, uuid) in indices {
                if managed(st, name).is_some() {
                    failed.push(failure(name, uuid, "This index already has a policy, use the update policy API to update index policies"));
                    continue;
                }
                manage(st, name, uuid, &policy, 2, now)?;
                updated += 1;
            }
            Ok((updated, failed))
        })
        .await?;
    outcome(r)
}

async fn remove(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let indices = targets(ctx, &st, true)?;
    let r = ctx
        .cluster
        .update(move |st| {
            let (mut updated, mut failed) = (0usize, Vec::new());
            for (name, uuid) in &indices {
                if managed(st, name).is_none() {
                    failed.push(failure(name, uuid, "This index does not have a policy to remove"));
                    continue;
                }
                store::remove(st, MANAGED, name);
                gc_policy_copies(st);
                // The config and its metadata documents go.
                next_seq(st);
                next_seq(st);
                // Removing a policy also opts the index out of `ism_template`s.
                if let Some(idx) = st.metadata.indices.get_mut(name) {
                    let idx = std::sync::Arc::make_mut(idx);
                    idx.settings.set("index.plugins.index_state_management.auto_manage", "false");
                    idx.settings_version += 1;
                }
                updated += 1;
            }
            Ok((updated, failed))
        })
        .await?;
    outcome(r)
}

/// `ChangePolicy.parse`: `policy_id`, `state`, `include` (state filters), `is_safe`, `user`.
fn parse_change_policy(ctx: &Ctx<'_>) -> EsResult<(String, Option<String>, Vec<String>)> {
    let Some(body) = ctx.body()? else { return Err(iae("ChangePolicy policy id is null")) };
    let mut p = XParser::new(&body, ctx.raw_json(), Vec::new());
    p.next_token();
    expect(&p, Tk::StartObject)?;
    let (mut policy, mut state, mut include) = (None, None, Vec::new());
    while let Some(field) = next_field(&mut p) {
        match field.as_str() {
            "policy_id" => policy = Some(text(&p)?),
            "state" => state = if p.current_token() == Some(Tk::Null) { None } else { Some(text(&p)?) },
            "include" => {
                include = array(&mut p, |p| {
                    expect(p, Tk::StartObject)?;
                    let mut st = None;
                    while let Some(f) = next_field(p) {
                        match f.as_str() {
                            "state" => st = Some(text(p)?),
                            _ => p.skip_children(),
                        }
                    }
                    st.ok_or_else(|| iae("Must include a state when using include filter"))
                })?;
            }
            "is_safe" => {
                p.boolean_value()?;
            }
            "user" => p.skip_children(),
            other => return Err(invalid(other, "ChangePolicy")),
        }
    }
    Ok((policy.ok_or_else(|| iae("ChangePolicy policy id is null"))?, state, include))
}

async fn change_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let (policy, state_name, include) = parse_change_policy(ctx)?;
    let st = ctx.state();
    if stored(&st, &policy).is_none() {
        return Err(status(404, format!("Could not find policy={policy}")));
    }
    let indices = targets(ctx, &st, true)?;
    let r = ctx
        .cluster
        .update(move |st| {
            let (mut updated, mut failed) = (0usize, Vec::new());
            for (name, uuid) in &indices {
                let Some(Value::Obj(mut m)) = managed(st, name).cloned() else {
                    failed.push(failure(name, uuid, "This index is not being managed"));
                    continue;
                };
                let meta = super::ism_run::meta_of(&Value::Obj(m.clone()));
                if meta.as_ref().is_some_and(|x| x.transitioning()) {
                    failed.push(failure(name, uuid, "Cannot change policy while transitioning to new state"));
                    continue;
                }
                // `include` limits the change to indices in those states (uninitialized ones
                // always change).
                if let Some(x) = &meta
                    && !include.is_empty()
                    && !x.state_name().is_some_and(|s| include.iter().any(|i| i == s))
                {
                    continue;
                }
                // The change waits for the job's next run (a safe one switches right away,
                // others at the state's transitions); until then the index keeps its policy.
                let current = job_policy(st, &Value::Obj(m.clone())).cloned().unwrap_or_default();
                let new_policy = stored(st, &policy).map(|s| s.policy).unwrap_or_default();
                let safe = super::ism_run::safe_to_change(&current, &new_policy, meta.as_ref().and_then(|x| x.state_name()), state_name.is_some());
                let include_v: Vec<Value> = include.iter().map(|s| obj! { "state" => s.as_str() }).collect();
                m.insert(
                    "change_policy".into(),
                    obj! { "policy_id" => policy.as_str(), "state" => state_name.clone().map_or(Value::Null, Value::Str), "include" => Value::Arr(include_v), "is_safe" => safe },
                );
                // The config document is rewritten (it moves to the end of the index).
                m.insert("seq_no".into(), Value::from(next_seq(st)));
                store::family_mut(st, MANAGED).insert(name.clone(), Value::Obj(m));
                updated += 1;
            }
            Ok((updated, failed))
        })
        .await?;
    outcome(r)
}

/// `TransportRetryFailedManagedIndexAction`: failed jobs are enabled again with their action's
/// retries reset (optionally restarting from the body's `state`).
async fn retry(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let start_state = match ctx.body_map()? {
        Some(m) => match m.get("state") {
            None | Some(Value::Null) => None,
            Some(Value::Str(s)) => Some(s.clone()),
            Some(other) => return Err(EsError::java(500, "class_cast_exception", format!("class {} cannot be cast to class java.lang.String", java_class(other)))),
        },
        None => None,
    };
    let st = ctx.state();
    let indices = targets(ctx, &st, true)?;
    let now = ctx.now_ms;
    let r = ctx
        .cluster
        .update(move |st| {
            let (mut updated, mut failed) = (0usize, Vec::new());
            for (name, uuid) in &indices {
                let reason = match managed(st, name) {
                    None => Some("This index is not being managed."),
                    Some(rec) => match super::ism_run::meta_of(rec) {
                        None => Some("This index has no metadata information"),
                        Some(m) if !m.is_failed() => Some("This index is not in failed state."),
                        Some(_) => None,
                    },
                };
                if let Some(reason) = reason {
                    failed.push(failure(name, uuid, reason));
                    continue;
                }
                let Some(meta) = managed(st, name).and_then(super::ism_run::meta_of) else { continue };
                let retried = super::ism_run::retried(&meta, start_state.clone());
                let interval = super::ism_run::job_interval(st);
                let start = managed(st, name).and_then(|r| r.get("schedule_start")).and_then(Value::as_i64).unwrap_or(now);
                let next = super::ism_run::next_run(st, start, interval, now);
                // The job is enabled (one config write), then its metadata replaced (another).
                next_seq(st);
                next_seq(st);
                if let Some(Value::Obj(r)) = store::family_mut(st, MANAGED).get_mut(name) {
                    r.insert("enabled".into(), Value::Bool(true));
                    r.insert("enabled_time".into(), Value::from(now));
                    r.insert("last_updated_time".into(), Value::from(now));
                    r.insert("next_run".into(), Value::from(next));
                    r.insert("metadata".into(), retried);
                }
                updated += 1;
            }
            Ok((updated, failed))
        })
        .await?;
    outcome(r)
}

/// The Java class name of a parsed JSON value (cast failures name it).
fn java_class(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "java.lang.Boolean",
        Value::Arr(_) => "java.util.ArrayList",
        Value::Obj(_) => "java.util.HashMap",
        Value::Str(_) => "java.lang.String",
        Value::Null => "java.lang.Object",
        _ => "java.lang.Integer",
    }
}

fn explain_entry(st: &ClusterState, name: &str, show_policy: bool, validate: bool) -> (Value, bool) {
    let mut m = Map::new();
    let rec = managed(st, name);
    let policy = rec.and_then(|r| r.get("policy_id")).and_then(Value::as_str);
    let id = policy.map_or(Value::Null, |p| Value::Str(p.to_string()));
    m.insert("index.plugins.index_state_management.policy_id".into(), id.clone());
    m.insert("index.opendistro.index_state_management.policy_id".into(), id);
    if let Some(r) = rec {
        super::ism_run::explain_record(name, r, &mut m);
    }
    m.insert("enabled".into(), rec.map_or(Value::Null, |r| r.get("enabled").cloned().unwrap_or(Value::Bool(true))));
    // The job's own copy of its policy (`ManagedIndexConfig.policy`).
    if show_policy && let Some(p) = rec.and_then(|r| job_policy(st, r)) {
        m.insert("policy".into(), as_read(p.clone()));
    }
    if validate {
        m.insert("validate".into(), obj! { "validation_message" => "Starting Validation", "validation_status" => "passed" });
    }
    (Value::Obj(m), rec.is_some())
}

/// `ExplainFilter`: the body of a `POST` explain.
#[derive(Default)]
struct ExplainFilter {
    policy_id: Option<String>,
    state: Option<String>,
    action_type: Option<String>,
    failed: Option<bool>,
}

impl ExplainFilter {
    fn parse(ctx: &Ctx<'_>) -> EsResult<Option<ExplainFilter>> {
        if ctx.method != crate::routes::Method::Post {
            return Ok(None);
        }
        let Some(body) = ctx.body()? else { return Err(EsError::parse("request body is required")) };
        let mut p = XParser::new(&body, ctx.raw_json(), Vec::new());
        p.next_token();
        expect(&p, Tk::StartObject)?;
        let mut f = ExplainFilter::default();
        while let Some(field) = next_field(&mut p) {
            if field != "filter" {
                p.skip_children();
                continue;
            }
            expect(&p, Tk::StartObject)?;
            while let Some(k) = next_field(&mut p) {
                match k.as_str() {
                    "policy_id" => f.policy_id = Some(text(&p)?),
                    "state" => f.state = Some(text(&p)?),
                    "action_type" => f.action_type = Some(text(&p)?),
                    "failed" => f.failed = Some(p.boolean_value()?),
                    _ => p.skip_children(),
                }
            }
        }
        Ok(Some(f))
    }

    /// Whether the managed-index search finds the config (`filterByPolicyID`).
    fn finds(&self, rec: &Value) -> bool {
        self.policy_id.as_deref().is_none_or(|p| rec.get("policy_id").and_then(Value::as_str) == Some(p))
    }

    /// `byMetaData`: indices whose metadata doesn't match drop out (those without stay).
    fn keeps(&self, rec: Option<&Value>) -> bool {
        match rec.and_then(super::ism_run::meta_of) {
            Some(m) => m.matches_filter(self.state.as_deref(), self.action_type.as_deref(), self.failed),
            None => true,
        }
    }
}

fn explain(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let flag = |k: &str| ctx.query(k).is_some_and(|v| v != "false");
    let (show, validate) = (flag("show_policy"), flag("validate_action"));
    let params = search_params(ctx, "managed_index.index")?;
    let filter = ExplainFilter::parse(ctx)?.unwrap_or_default();
    let mut out = Map::new();
    let mut total = 0usize;
    match ctx.path_param("index") {
        // The managed indices of the cluster matching `queryString`, a page of them in the
        // sort order (the plugin searches its managed-index configs, filtered by the uuids of
        // the cluster's indices); the total counts every match.
        None => {
            let key = sort_key(ctx, &st, &params.sort_field, Doc::Managed)?;
            let mut list: Vec<(&String, &Value)> = st
                .metadata
                .xpack
                .iter(MANAGED)
                .filter(|(name, rec)| st.metadata.indices.get(*name).is_some_and(|i| rec.get("index_uuid").and_then(Value::as_str) == Some(i.uuid.as_str())))
                .filter(|(name, rec)| query_matches(&params.query, name) && filter.finds(rec))
                .collect();
            let seq = |r: &Value| r.get("seq_no").and_then(Value::as_i64).unwrap_or(0);
            list.sort_by(|a, b| sort_cmp(&managed_sort_value(a, key), &managed_sort_value(b, key), params.desc).then_with(|| seq(a.1).cmp(&seq(b.1))));
            total = list.len();
            for (name, rec) in list.into_iter().skip(params.from).take(params.size) {
                if !filter.keeps(Some(rec)) {
                    total -= 1;
                    continue;
                }
                out.insert(name.clone(), explain_entry(&st, name, show, validate).0);
            }
        }
        Some(expr) => {
            // Listed indices: the config search asks for `MAX_HITS` (10,000) configs from `from`.
            if params.from > 0 {
                let e = EsError::illegal_argument(format!(
                    "Result window is too large, from + size must be less than or equal to: [10000] but was [{}]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.",
                    params.from + 10_000
                ));
                return Err(config_search_failure(&ctx.cluster.node.id, e.clone().caused_by(e)));
            }
            sort_key(ctx, &st, &params.sort_field, Doc::Managed)?;
            let opts = IndicesOptions { ignore_unavailable: true, ..IndicesOptions::STRICT_EXPAND_HIDDEN };
            let exprs = crate::meta::resolve::split_expressions(expr);
            // `DefaultIndexMetadataService` collects the names in a `HashMap` (filled in cluster
            // order) that explain copies with `putAll` into a fresh `HashMap`, presized for them.
            let names = cluster_order(&st, crate::meta::resolve::resolve(&st, &exprs, &opts, ctx.now_ms)?);
            let order = crate::json::java_hash_order(&names, 16);
            let names: Vec<String> = order.iter().map(|&i| names[i].clone()).collect();
            let presized = ((names.len() as f32 / 0.75 + 1.0) as usize).next_power_of_two();
            let order = crate::json::java_hash_order(&names, presized);
            let names: Vec<String> = order.iter().map(|&i| names[i].clone()).collect();
            if names.is_empty() {
                return Err(EsError::action_request_validation(&["no documents to get".into()]));
            }
            // A `queryString` (or a policy filter) narrows the managed-index configs the plugin
            // finds: the listed indices it matches answer alone, and when it matches none every
            // listed index answers as unmanaged.
            let narrowed = params.query.trim() != "*" || filter.policy_id.is_some();
            let found: Vec<&String> = names.iter().filter(|n| managed(&st, n).is_some_and(|r| query_matches(&params.query, n) && filter.finds(r))).collect();
            for name in &names {
                if narrowed && !found.is_empty() && !found.contains(&name) {
                    continue;
                }
                let unmanaged = narrowed && found.is_empty();
                if !unmanaged && !filter.keeps(managed(&st, name)) {
                    continue;
                }
                let (v, is_managed) = if unmanaged { (explain_entry(&ClusterState::default(), name, false, validate).0, false) } else { explain_entry(&st, name, show, validate) };
                out.insert(name.clone(), v);
                total += usize::from(is_managed);
            }
        }
    }
    out.insert("total_managed_indices".into(), Value::from(total));
    Reply::ok(Value::Obj(out))
}

fn managed_sort_value((name, rec): &(&String, &Value), key: SortKey) -> SortValue {
    let text = |k: &str| rec.get(k).and_then(Value::as_str).map_or(SortValue::Missing, |s| SortValue::Text(s.to_string()));
    match key {
        SortKey::Index => SortValue::Text((*name).clone()),
        SortKey::IndexUuid => text("index_uuid"),
        SortKey::ManagedPolicyId => text("policy_id"),
        // Booleans sort as 0 / 1.
        SortKey::Enabled => SortValue::Num(i64::from(rec.get("enabled").and_then(Value::as_bool).unwrap_or(true))),
        _ => SortValue::Missing,
    }
}

// ------------------------------------------------------------------------------ hooks

/// `IndexMetadataProvider.isUnManageableIndex`: the index matches
/// `plugins.index_state_management.restricted_index_pattern` (a regex over the whole name,
/// the config index by default).
fn restricted(st: &ClusterState, name: &str) -> bool {
    match st.metadata.cluster_setting("plugins.index_state_management.restricted_index_pattern") {
        None => name == CONFIG_INDEX,
        Some(p) => regex::Regex::new(&format!("^(?:{p})$")).is_ok_and(|r| r.is_match(name)),
    }
}

/// `ManagedIndexCoordinator.sweepClusterChangedEvent`, run on every cluster state change of
/// the opensearch dialect: indices that went away stop being managed, and new ones (created,
/// resized, restored, rolled over, data stream backing indices…) are matched against the
/// `ism_template`s. Indices are told apart by uuid, so a recreated name counts as both.
pub fn reconcile(prev: &ClusterState, next: &mut ClusterState, now: i64) {
    if next.metadata.xpack.len(POLICIES) == 0 && next.metadata.xpack.len(MANAGED) == 0 {
        return;
    }
    let same = |a: &std::sync::Arc<crate::meta::IndexMeta>, b: Option<&std::sync::Arc<crate::meta::IndexMeta>>| b.is_some_and(|b| std::sync::Arc::ptr_eq(a, b) || a.uuid == b.uuid);
    let gone: Vec<String> = prev.metadata.indices.iter().filter(|(n, i)| !same(i, next.metadata.indices.get(*n))).map(|(n, _)| n.clone()).collect();
    let new: Vec<String> = next.metadata.indices.iter().filter(|(n, i)| !same(i, prev.metadata.indices.get(*n))).map(|(n, _)| n.clone()).collect();
    for n in &gone {
        on_index_deleted(next, n);
    }
    for n in &new {
        let ds = next.metadata.data_stream_of(n).map(|d| d.name.clone());
        on_index_created(next, n, ds.as_deref(), now);
    }
}

/// A new index: the highest-priority matching `ism_template` manages it (by the index name,
/// or a backing index's data stream name), unless it is hidden (by setting or a leading `.`,
/// data stream indices aside), restricted, or opted out. A template only applies to indices
/// created after its last update (`findMatchingPolicy`), which keeps restored older indices
/// out.
fn on_index_created(st: &mut ClusterState, name: &str, data_stream: Option<&str>, now: i64) {
    if st.metadata.xpack.len(POLICIES) == 0 || name == CONFIG_INDEX {
        return;
    }
    let Some(idx) = st.metadata.indices.get(name) else { return };
    let off = |k: &str| idx.settings.get(k) == Some("false");
    let hidden = idx.settings.get("index.hidden") == Some("true") || name.starts_with('.');
    if (data_stream.is_none() && hidden) || off("index.plugins.index_state_management.auto_manage") || off("index.opendistro.index_state_management.auto_manage") {
        return;
    }
    let target = data_stream.unwrap_or(name);
    if restricted(st, target) {
        return;
    }
    let uuid = idx.uuid.clone();
    let created = idx.creation_date();
    // Per priority the last policy listed with a template of it (`priorityPolicyMap`), then
    // the highest priority.
    let mut by_priority: Vec<(i64, String)> = Vec::new();
    for (id, v) in st.metadata.xpack.iter(POLICIES) {
        let mut best = -1i64;
        for t in v.get("policy").and_then(|p| p.get("ism_template")).and_then(Value::as_array).into_iter().flatten() {
            if t.get("last_updated_time").and_then(Value::as_i64).is_some_and(|u| u >= created) {
                continue;
            }
            let prio = t.get("priority").and_then(Value::as_i64).unwrap_or(0);
            if prio > best && matches_patterns(target, template_patterns(t).into_iter()) {
                best = prio;
            }
        }
        if best > -1 {
            by_priority.retain(|(p, _)| *p != best);
            by_priority.push((best, id.clone()));
        }
    }
    if let Some((_, policy)) = by_priority.into_iter().max_by_key(|(p, _)| *p) {
        // The config index already exists (it holds the policy), so this can't fail.
        let _ = manage(st, name, &uuid, &policy, 1, now);
    }
}

/// A deleted index stops being managed; deleting the config index drops everything.
pub fn on_index_deleted(st: &mut ClusterState, name: &str) {
    if name == CONFIG_INDEX {
        for f in [POLICIES, MANAGED, META] {
            store::clear(st, f);
        }
        return;
    }
    if managed(st, name).is_some() {
        store::remove(st, MANAGED, name);
        gc_policy_copies(st);
        // The config and its metadata documents are deleted (the metadata delete is written
        // even before the job first ran).
        next_seq(st);
        next_seq(st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> EsResult<Map> {
        let v = crate::json::parse(body.as_bytes()).unwrap().unwrap();
        let mut p = XParser::new(&v, Some(body.as_bytes()), Vec::new());
        parse_request(&mut p, "p", 7)
    }

    fn err(body: &str) -> String {
        parse(body).unwrap_err().reason().to_string()
    }

    fn compact(v: &Value) -> String {
        let mut out = Vec::new();
        crate::json::write::write_json(&mut out, v, false);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn policies_normalize_like_the_plugin() {
        let m = parse(r#"{"policy":{"description":"d","default_state":"hot","states":[{"name":"hot","actions":[{"rollover":{"min_index_age":"1d","min_doc_count":5}},{"timeout":"1h","retry":{"count":5},"read_only":{}},{"alias":{"actions":[{"add":{"alias":"x"}}]}}],"transitions":[{"state_name":"warm","conditions":{"min_index_age":"7d"}}]},{"name":"warm"}],"ism_template":{"index_patterns":["l-*"]}}}"#).unwrap();
        let states = m["states"].as_array().unwrap();
        let a = states[0].get("actions").and_then(Value::as_array).unwrap();
        assert_eq!(compact(&a[0]), r#"{"retry":{"count":3,"backoff":"exponential","delay":"1m"},"rollover":{"min_doc_count":5,"min_index_age":"1d","copy_alias":false}}"#);
        assert_eq!(compact(&a[1]), r#"{"timeout":"1h","retry":{"count":5,"backoff":"exponential","delay":"1m"},"read_only":{}}"#);
        assert_eq!(compact(a[2].get("alias").unwrap()), r#"{"actions":[{"add":{"aliases":["x"]}}]}"#);
        assert_eq!(compact(&states[1]), r#"{"name":"warm","actions":[],"transitions":[]}"#);
        assert_eq!(compact(&m["ism_template"]), r#"[{"index_patterns":["l-*"],"priority":0,"last_updated_time":7}]"#);
        assert_eq!(m["schema_version"], Value::from(1));
        assert_eq!(m["error_notification"], Value::Null);
    }

    #[test]
    fn validation_messages() {
        let err = |b: &str| parse(b).unwrap_err().reason().to_string();
        assert_eq!(err(r#"{"policy":{"description":"d","default_state":"x","states":[{"name":"a"}]}}"#), "Policy must have a valid default state");
        assert_eq!(err(r#"{"policy":{"description":"d","default_state":"a","states":[]}}"#), "Policy must contain at least one State");
        assert_eq!(err(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a"},{"name":"a"}]}}"#), "Policy cannot have duplicate state names");
        assert_eq!(
            err(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a","transitions":[{"state_name":"b"}]}]}}"#),
            "Policy contains a transition in state=a pointing to a nonexistent state=b"
        );
        assert_eq!(err(r#"{"policy":{"default_state":"a","states":[{"name":"a"}]}}"#), "description is null");
        assert_eq!(err(r#"{"policy":{"description":"d","states":[{"name":"a"}]}}"#), "default_state is null");
        assert_eq!(err(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a","actions":[{"bogus":{}}]}]}}"#), "Invalid field: [bogus] found in Actions.");
        assert_eq!(
            err(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a","actions":[{"delete":{}},{"open":{}}]}]}}"#),
            "State=a must not contain an action after a delete action"
        );
        assert_eq!(
            err(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a","actions":[{"delete":{}}],"transitions":[{"state_name":"a"}]}]}}"#),
            "State=a cannot contain transitions if using delete action"
        );
        assert_eq!(
            err(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a","transitions":[{"state_name":"a","conditions":{"min_doc_count":1,"min_size":"1gb"}}]}]}}"#),
            "Cannot provide more than one Transition condition"
        );
        assert_eq!(err(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a"}],"ism_template":{"index_patterns":[],"priority":-1}}}"#), "Requires priority to be >= 0");
        assert_eq!(err(r#"{"policy":1}"#), "Failed to parse object: expecting token of type [START_OBJECT] but found [VALUE_NUMBER]");
        assert_eq!(err(r#"{}"#), "Failed to parse object: expecting token of type [FIELD_NAME] but found [END_OBJECT]");
    }

    #[test]
    fn overlapping_patterns_intersect_like_automata() {
        let o = |a: &[&str], b: &[&str]| overlapping(a, b);
        assert!(o(&["logs-*"], &["logs-*"]));
        assert!(o(&["logs-*"], &["logs-2024"]));
        assert!(!o(&["logs-*"], &["metrics-*"]));
        // Neither matches the other as a name, but `review-x-zds9` matches both.
        assert!(o(&["review-*-zds*"], &["*-zds9"]));
        assert!(o(&["review-compat-os-q*"], &["*-qx"]));
        assert!(o(&["a*b*c"], &["*bc"]));
        assert!(!o(&["abc"], &["abd"]));
        assert!(!o(&["a*"], &["b*"]));
        assert!(o(&["*"], &["x"]));
        assert!(!o(&[], &["*"]));
        // Exclusions are literal text to the automaton.
        assert!(!o(&["-x*"], &["x*"]));
        assert!(globs_intersect(b"**", b""));
        assert!(!globs_intersect(b"a", b""));
    }

    #[test]
    fn exclusion_patterns_and_wildcards() {
        let m = |n: &str, p: &[&str]| matches_patterns(n, p.iter().copied());
        assert!(m("v4y1", &["v4*", "-v4x*"]));
        assert!(!m("v4x1", &["v4*", "-v4x*"]));
        assert!(!m("v4x1", &["-v4x*"]));
        assert!(wildcard(b"cos-q?", b"cos-q1"));
        assert!(!wildcard(b"cos-q?", b"cos-q12"));
        assert!(wildcard(b"*q1", b"cos-q1"));
        assert!(wildcard(b"a*b*c", b"axxbyyc"));
        assert!(!wildcard(b"a*b*c", b"axxbyy"));
    }

    #[test]
    fn query_strings_match_whole_ids_case_sensitively() {
        assert!(query_matches("*", "anything"));
        assert!(query_matches("cos*", "cos-p1"));
        assert!(!query_matches("COS*", "cos-p1"));
        assert!(!query_matches("hello", "cos-p1"), "descriptions are not searched");
        assert!(query_matches("cos-q1", "cos-q1"));
        assert!(!query_matches("cos-q1 cos-q2", "cos-q1"), "terms are ANDed");
        assert!(query_matches("cos-q1 OR cos-q2", "cos-q2"));
        assert!(!query_matches("cos* -cos-q1", "cos-q1"));
        assert!(query_matches("cos* -cos-q1", "cos-q2"));
    }

    #[test]
    fn template_patterns_validate_like_the_plugin() {
        let reason = |p: &[&str]| validate_format(p).map(|e| e.reason().to_string());
        assert_eq!(reason(&["ok-*"]), None);
        assert_eq!(reason(&["review#x"]).as_deref(), Some("Validation Failed: 1: index_pattern [review#x] must not contain a '#';"));
        assert_eq!(reason(&["_review"]).as_deref(), Some("Validation Failed: 1: index_pattern [_review] must not start with '_';"));
        assert_eq!(
            reason(&["-x*"]).as_deref(),
            Some("Validation Failed: 1: index_patterns must contain at least one inclusion pattern (patterns cannot be all exclusions);")
        );
        assert_eq!(
            reason(&["a:b", "c d"]).as_deref(),
            Some("Validation Failed: 1: index_pattern [a:b] must not contain a ':';2: index_pattern [c d] must not contain the following characters [ , \", *, \\, <, |, ,, >, /, ?];")
        );
        let t = crate::json::parse(br#"[{"index_patterns":["a*"],"priority":3},{"index_patterns":["*b"],"priority":3}]"#).unwrap().unwrap();
        assert_eq!(
            self_conflict("p", &t).map(|e| e.reason().to_string()).as_deref(),
            Some("New policy p has an ISM template with index pattern [a*] matching this policy's other ISM templates with index patterns [*b], please use different priority")
        );
        let t = crate::json::parse(br#"[{"index_patterns":["a*"],"priority":3},{"index_patterns":["*b"],"priority":4}]"#).unwrap().unwrap();
        assert!(self_conflict("p", &t).is_none());
    }

    #[test]
    fn actions_validate_like_their_parsers() {
        let action = |a: &str| err(&format!(r#"{{"policy":{{"description":"d","default_state":"a","states":[{{"name":"a","actions":[{a}]}}]}}}}"#));
        assert_eq!(action(r#"{"notification":{"message_template":{"source":"m"}}}"#), "Notification must contain a destination or channel");
        assert_eq!(action(r#"{"snapshot":{"repository":"r"}}"#), "SnapshotAction snapshot must be specified");
        assert_eq!(action(r#"{"snapshot":{"bogus":1}}"#), "Invalid field: [bogus] found in SnapshotAction.");
        assert_eq!(action(r#"{"index_priority":{}}"#), "priority is null");
        assert_eq!(action(r#"{"shrink":{}}"#), "Exactly one option specifying the number of shards to shrink to must be used.");
        assert_eq!(action(r#"{"shrink":{"num_new_shards":1,"max_shard_size":"1gb"}}"#), "Exactly one option specifying the number of shards to shrink to must be used.");
        assert_eq!(action(r#"{"convert_index_to_remote":{"repository":"r"}}"#), "ConvertIndexToRemoteAction snapshot must be specified");
        assert_eq!(action(r#"{"alias":{"actions":[]}}"#), "At least one alias action needs to be specified.");
        assert_eq!(action(r#"{"alias":{"actions":[{"add":{"alias":"a","index":"i"}}]}}"#), "Alias action can only work on its applied index so don't accept index/indices parameter.");
        assert_eq!(action(r#"{"retry":{},"read_only":{}}"#), "ActionRetry count is null");
        assert_eq!(action(r#"{"search_only":{"enable":true}}"#), "Failed to parse object: expecting token of type [END_OBJECT] but found [FIELD_NAME]");
        let m = parse(r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a","actions":[{"allocation":{"require":{"box":5,"hot":true}}}]}]}}"#).unwrap();
        let a = &m["states"].as_array().unwrap()[0].get("actions").and_then(Value::as_array).unwrap()[0];
        assert_eq!(compact(a.get("allocation").unwrap()), r#"{"require":{"box":"5","hot":"true"},"include":{},"exclude":{},"wait_for":false}"#);
    }
}

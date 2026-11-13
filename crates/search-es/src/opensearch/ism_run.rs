//! Index State Management execution: the plugin's `ManagedIndexRunner`, run by
//! [`super::jobs`] when a managed index's job is due.
//!
//! Each managed index (a record of the `ism.managed` family, see [`super::ism`]) carries the
//! plugin's job document (`policy` copy, `policy_seq_no`, `enabled`, `enabled_time`, the
//! interval schedule) and, once the first run initialized it, its `ManagedIndexMetaData`
//! (`metadata`): the state, action and step being worked on, retry counters and the `info`
//! explain prints. A run follows `runManagedIndexConfig` decision for decision: initialize,
//! disable a completed or failed job, refuse a policy version conflict, time out an action,
//! switch to a pending `change_policy`, back off a failed action's retry, then write the
//! "starting" metadata, execute one step and write its outcome. Steps act through roll's own
//! REST handlers (`_rollover`, `_settings`, `_forcemerge`, `_close`, `_snapshot`, …), so a
//! policy changes an index exactly as the same request from a client would.
//!
//! Every metadata write is a write of the plugin's config index (it advances the config
//! index's sequence numbers like the plugin's documents do) and, unless the step is starting
//! or waiting, adds the metadata to the hidden `.opendistro-ism-managed-index-history-*`
//! index the way `IndexStateManagementHistory` does.
//!
//! Runs of one batch share two cluster state updates per phase (starting, then outcomes), so a
//! sweep over many due indices costs a constant number of state writes per phase, not one per
//! index.

use super::ism::{self, MANAGED};
use crate::cluster::Cluster;
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::obj;
use crate::rest::EsRequest;
use crate::xpack::store;

// ------------------------------------------------------------------------------ metadata

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Starting,
    ConditionNotMet,
    Failed,
    Completed,
    TimedOut,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::ConditionNotMet => "condition_not_met",
            Status::Failed => "failed",
            Status::Completed => "completed",
            Status::TimedOut => "timed_out",
        }
    }

    fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "starting" => Status::Starting,
            "condition_not_met" => Status::ConditionNotMet,
            "failed" => Status::Failed,
            "completed" => Status::Completed,
            "timed_out" => Status::TimedOut,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct StateMeta {
    name: String,
    start_time: i64,
}

#[derive(Clone, Debug, PartialEq)]
struct ActionMeta {
    name: String,
    start_time: Option<i64>,
    index: i64,
    failed: bool,
    consumed_retries: i64,
    last_retry_time: Option<i64>,
    properties: Option<Map>,
}

#[derive(Clone, Debug, PartialEq)]
struct StepMeta {
    name: String,
    start_time: i64,
    status: Status,
}

#[derive(Clone, Debug, PartialEq)]
struct RetryInfo {
    failed: bool,
    consumed_retries: i64,
}

/// `ManagedIndexMetaData`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Meta {
    index: String,
    index_uuid: String,
    policy_id: String,
    policy_seq_no: Option<i64>,
    policy_primary_term: Option<i64>,
    policy_completed: Option<bool>,
    rolled_over: Option<bool>,
    rolled_over_index_name: Option<String>,
    index_creation_date: Option<i64>,
    transition_to: Option<String>,
    state: Option<StateMeta>,
    action: Option<ActionMeta>,
    step: Option<StepMeta>,
    retry_info: Option<RetryInfo>,
    info: Option<Map>,
}

fn opt_i64(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::from)
}

fn opt_bool(v: Option<bool>) -> Value {
    v.map_or(Value::Null, Value::Bool)
}

fn opt_str(v: &Option<String>) -> Value {
    v.clone().map_or(Value::Null, Value::Str)
}

fn info_of(message: impl Into<String>) -> Map {
    let mut m = Map::new();
    m.insert("message".into(), Value::Str(message.into()));
    m
}

impl StateMeta {
    fn to_value(&self) -> Value {
        obj! { "name" => self.name.as_str(), "start_time" => self.start_time }
    }

    /// Kotlin's data class `toString`, which some failure messages embed.
    fn debug(&self) -> String {
        format!("StateMetaData(name={}, startTime={})", self.name, self.start_time)
    }
}

impl ActionMeta {
    fn new(name: &str, index: i64, now: i64) -> ActionMeta {
        ActionMeta { name: name.to_string(), start_time: Some(now), index, failed: false, consumed_retries: 0, last_retry_time: Some(0), properties: None }
    }

    fn to_value(&self) -> Value {
        let mut m = obj! {
            "name" => self.name.as_str(),
            "start_time" => opt_i64(self.start_time),
            "index" => self.index,
            "failed" => self.failed,
            "consumed_retries" => self.consumed_retries,
            "last_retry_time" => opt_i64(self.last_retry_time),
        };
        if let (Some(p), Value::Obj(m)) = (&self.properties, &mut m) {
            m.insert("action_properties".into(), Value::Obj(p.clone()));
        }
        m
    }

    fn debug(&self) -> String {
        let n = |v: Option<i64>| v.map_or("null".to_string(), |v| v.to_string());
        format!(
            "ActionMetaData(name={}, startTime={}, index={}, failed={}, consumedRetries={}, lastRetryTime={}, actionProperties={})",
            self.name,
            n(self.start_time),
            self.index,
            self.failed,
            self.consumed_retries,
            n(self.last_retry_time),
            if self.properties.is_some() { "ActionProperties(...)" } else { "null" }
        )
    }
}

impl StepMeta {
    fn to_value(&self) -> Value {
        obj! { "name" => self.name.as_str(), "start_time" => self.start_time, "step_status" => self.status.as_str() }
    }
}

impl Meta {
    fn from_value(v: &Value) -> Option<Meta> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let i = |k: &str| v.get(k).and_then(Value::as_i64);
        let b = |k: &str| v.get(k).and_then(Value::as_bool);
        let state = v.get("state").filter(|x| !matches!(x, Value::Null)).map(|x| StateMeta {
            name: x.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            start_time: x.get("start_time").and_then(Value::as_i64).unwrap_or(0),
        });
        let action = v.get("action").filter(|x| !matches!(x, Value::Null)).map(|x| ActionMeta {
            name: x.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            start_time: x.get("start_time").and_then(Value::as_i64),
            index: x.get("index").and_then(Value::as_i64).unwrap_or(0),
            failed: x.get("failed").and_then(Value::as_bool).unwrap_or(false),
            consumed_retries: x.get("consumed_retries").and_then(Value::as_i64).unwrap_or(0),
            last_retry_time: x.get("last_retry_time").and_then(Value::as_i64),
            properties: x.get("action_properties").and_then(Value::as_object).cloned(),
        });
        let step = v.get("step").filter(|x| !matches!(x, Value::Null)).map(|x| StepMeta {
            name: x.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            start_time: x.get("start_time").and_then(Value::as_i64).unwrap_or(0),
            status: x.get("step_status").and_then(Value::as_str).and_then(Status::parse).unwrap_or(Status::Starting),
        });
        let retry_info = v.get("retry_info").filter(|x| !matches!(x, Value::Null)).map(|x| RetryInfo {
            failed: x.get("failed").and_then(Value::as_bool).unwrap_or(false),
            consumed_retries: x.get("consumed_retries").and_then(Value::as_i64).unwrap_or(0),
        });
        Some(Meta {
            index: s("index")?,
            index_uuid: s("index_uuid")?,
            policy_id: s("policy_id")?,
            policy_seq_no: i("policy_seq_no"),
            policy_primary_term: i("policy_primary_term"),
            policy_completed: b("policy_completed"),
            rolled_over: b("rolled_over"),
            rolled_over_index_name: s("rolled_over_index_name"),
            index_creation_date: i("index_creation_date"),
            transition_to: s("transition_to"),
            state,
            action,
            step,
            retry_info,
            info: v.get("info").and_then(Value::as_object).cloned(),
        })
    }

    /// The stored form (`toXContent(..., forIndex = true)`: every field, nulls kept).
    fn to_stored(&self) -> Value {
        obj! {
            "index" => self.index.as_str(),
            "index_uuid" => self.index_uuid.as_str(),
            "policy_id" => self.policy_id.as_str(),
            "policy_seq_no" => opt_i64(self.policy_seq_no),
            "policy_primary_term" => opt_i64(self.policy_primary_term),
            "policy_completed" => opt_bool(self.policy_completed),
            "rolled_over" => opt_bool(self.rolled_over),
            "rolled_over_index_name" => opt_str(&self.rolled_over_index_name),
            "index_creation_date" => opt_i64(self.index_creation_date),
            "transition_to" => opt_str(&self.transition_to),
            "state" => self.state.as_ref().map_or(Value::Null, StateMeta::to_value),
            "action" => self.action.as_ref().map_or(Value::Null, ActionMeta::to_value),
            "step" => self.step.as_ref().map_or(Value::Null, StepMeta::to_value),
            "retry_info" => self.retry_info.as_ref().map_or(Value::Null, |r| obj! { "failed" => r.failed, "consumed_retries" => r.consumed_retries }),
            "info" => self.info.clone().map_or(Value::Null, Value::Obj),
        }
    }

    /// The response form (`toXContent` without `forIndex`), as explain prints it after its
    /// round trip through `toMap` / `fromMap`: `info` comes back as a `HashMap`.
    pub(crate) fn explain_into(&self, m: &mut Map) {
        m.insert("index".into(), Value::Str(self.index.clone()));
        m.insert("index_uuid".into(), Value::Str(self.index_uuid.clone()));
        m.insert("policy_id".into(), Value::Str(self.policy_id.clone()));
        if let Some(s) = self.policy_seq_no {
            m.insert("policy_seq_no".into(), Value::from(s));
        }
        if let Some(t) = self.policy_primary_term {
            m.insert("policy_primary_term".into(), Value::from(t));
        }
        if self.rolled_over == Some(true) || self.action.as_ref().is_some_and(|a| a.name == "rollover") {
            m.insert("rolled_over".into(), opt_bool(self.rolled_over));
            if let Some(n) = &self.rolled_over_index_name {
                m.insert("rolled_over_index_name".into(), Value::Str(n.clone()));
            }
        }
        if let Some(d) = self.index_creation_date {
            m.insert("index_creation_date".into(), Value::from(d));
        }
        if self.policy_completed == Some(true) {
            m.insert("policy_completed".into(), Value::Bool(true));
            return;
        }
        if let Some(t) = &self.transition_to {
            m.insert("transition_to".into(), Value::Str(t.clone()));
        } else {
            if let Some(s) = &self.state {
                m.insert("state".into(), s.to_value());
            }
            if let Some(a) = &self.action {
                m.insert("action".into(), a.to_value());
            }
            if let Some(s) = &self.step {
                m.insert("step".into(), s.to_value());
            }
        }
        if let Some(r) = &self.retry_info {
            m.insert("retry_info".into(), obj! { "failed" => r.failed, "consumed_retries" => r.consumed_retries });
        }
        if let Some(i) = &self.info {
            m.insert("info".into(), super::hash_ordered(Value::Obj(i.clone())));
        }
    }

    fn explain_value(&self) -> Value {
        let mut m = Map::new();
        self.explain_into(&mut m);
        Value::Obj(m)
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.retry_info.as_ref().is_some_and(|r| r.failed) || self.action.as_ref().is_some_and(|a| a.failed)
    }

    /// `ExplainFilter.byMetaData`.
    pub(crate) fn matches_filter(&self, state: Option<&str>, action: Option<&str>, failed: Option<bool>) -> bool {
        if let Some(s) = state
            && self.state.as_ref().is_none_or(|m| m.name != s)
        {
            return false;
        }
        if let Some(a) = action
            && self.action.as_ref().is_none_or(|m| m.name != a)
        {
            return false;
        }
        if let Some(f) = failed {
            let action_off = self.action.as_ref().is_none_or(|a| a.failed != f);
            let retry_off = self.retry_info.as_ref().is_none_or(|r| r.failed != f);
            if action_off && retry_off {
                return false;
            }
        }
        true
    }

    /// Whether a transition to another state is pending.
    pub(crate) fn transitioning(&self) -> bool {
        self.transition_to.is_some()
    }

    /// The state the policy runs, after `getStateToExecute`.
    pub(crate) fn state_name(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.name.as_str())
    }

    /// `TransportRetryFailedManagedIndexAction`: the metadata a retry writes.
    fn retried(&self, start_state: Option<String>) -> Meta {
        let mut m = self.clone();
        m.step = None;
        m.retry_info = Some(RetryInfo { failed: false, consumed_retries: 0 });
        if let Some(a) = &mut m.action {
            a.failed = false;
            a.consumed_retries = 0;
            a.last_retry_time = None;
            a.start_time = None;
        }
        m.transition_to = start_state;
        m.info = Some(info_of("Pending retry of failed managed index"));
        m
    }
}

/// The stored metadata a retry writes for a failed job.
pub(crate) fn retried(meta: &Meta, start_state: Option<String>) -> Value {
    meta.retried(start_state).to_stored()
}

/// A managed record's metadata, if its job initialized it.
pub(crate) fn meta_of(rec: &Value) -> Option<Meta> {
    rec.get("metadata").and_then(Meta::from_value)
}

/// The explain entry of a managed record (`index`, `index_uuid`, `policy_id` before the first
/// run, the metadata after).
pub(crate) fn explain_record(name: &str, rec: &Value, m: &mut Map) {
    match meta_of(rec) {
        Some(meta) => meta.explain_into(m),
        None => {
            m.insert("index".into(), Value::Str(name.to_string()));
            m.insert("index_uuid".into(), rec.get("index_uuid").cloned().unwrap_or(Value::Null));
            m.insert("policy_id".into(), rec.get("policy_id").cloned().unwrap_or(Value::Null));
        }
    }
}

// ------------------------------------------------------------------------------ policy model

/// An action of a policy state: its type, position (`-1` for the transitions) and config.
struct Act<'a> {
    ty: &'a str,
    index: i64,
    config: Option<&'a Value>,
}

const TRANSITION: &str = "transition";

fn states(policy: &Map) -> &[Value] {
    policy.get("states").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn find_state<'a>(policy: &'a Map, name: &str) -> Option<&'a Value> {
    states(policy).iter().find(|s| s.get("name").and_then(Value::as_str) == Some(name))
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// An action config's type: its one key besides `retry`, `timeout` and `custom`.
fn action_type(a: &Value) -> &str {
    a.as_object().and_then(|m| m.keys().find(|k| !matches!(k.as_str(), "retry" | "timeout" | "custom"))).map_or("", String::as_str)
}

/// The steps of an action type, in order.
fn steps_of(ty: &str) -> &'static [&'static str] {
    match ty {
        TRANSITION => &["attempt_transition_step"],
        "rollover" => &["attempt_rollover"],
        "delete" => &["attempt_delete"],
        "read_only" => &["set_read_only"],
        "read_write" => &["set_read_write"],
        "replica_count" => &["attempt_set_replica_count"],
        "force_merge" => &["attempt_set_read_only", "attempt_call_force_merge", "wait_for_force_merge"],
        "close" => &["attempt_close"],
        "open" => &["attempt_open"],
        "index_priority" => &["attempt_set_index_priority"],
        "allocation" => &["attempt_allocation"],
        "alias" => &["attempt_alias"],
        "notification" => &["attempt_notification"],
        "snapshot" => &["attempt_snapshot", "wait_for_snapshot"],
        "shrink" => &["attempt_move_shards_step", "wait_for_move_shards_step", "attempt_shrink_step", "wait_for_shrink_step"],
        "rollup" => &["attempt_create_rollup", "wait_for_rollup_completion"],
        "transform" => &["attempt_create_transform", "wait_for_transform_completion"],
        "search_only" => &["attempt_search_only"],
        "convert_index_to_remote" => &["attempt_restore"],
        "stop_replication" => &["attempt_stop_replication"],
        _ => &["attempt_unknown"],
    }
}

/// `Action.getStepToExecute`: the first step, the next one once a step completed, else the
/// current one again.
fn step_to_execute(ty: &str, meta: &Meta) -> &'static str {
    let steps = steps_of(ty);
    let Some(s) = &meta.step else { return steps[0] };
    match steps.iter().position(|n| *n == s.name) {
        None => steps[0],
        Some(i) if s.status == Status::Completed && i + 1 < steps.len() => steps[i + 1],
        Some(i) => steps[i],
    }
}

fn is_last_step(ty: &str, step: &str) -> bool {
    steps_of(ty).last() == Some(&step)
}

/// `Policy.getStateToExecute`.
fn state_to_execute<'a>(policy: &'a Map, meta: &Meta) -> Option<&'a Value> {
    match &meta.transition_to {
        Some(t) => find_state(policy, t),
        None => find_state(policy, &meta.state.as_ref()?.name),
    }
}

/// `State.getActionToExecute`.
fn action_to_execute<'a>(state: &'a Value, meta: &Meta) -> Option<Act<'a>> {
    let actions = list(state, "actions");
    let transitions = || Act { ty: TRANSITION, index: -1, config: None };
    let at = |i: usize| actions.get(i).map(|a| Act { ty: action_type(a), index: i as i64, config: Some(a) });
    let Some(am) = meta.action.as_ref().filter(|_| meta.transition_to.is_none()) else {
        return Some(at(0).unwrap_or_else(transitions));
    };
    if am.name == TRANSITION {
        return Some(transitions());
    }
    let idx = usize::try_from(am.index).ok()?;
    let current = at(idx).filter(|a| a.ty == am.name)?;
    if let Some(step) = &meta.step
        && step.status == Status::Completed
        && is_last_step(current.ty, &step.name)
    {
        return Some(at(idx + 1).unwrap_or_else(transitions));
    }
    Some(current)
}

/// `Action.getUpdatedActionMetadata`.
fn updated_action_meta(act: &Act<'_>, meta: &Meta, state_name: &str, now: i64) -> ActionMeta {
    match &meta.action {
        Some(a) if meta.state.as_ref().is_some_and(|s| s.name == state_name) && a.index == act.index => {
            ActionMeta { start_time: Some(a.start_time.unwrap_or(now)), ..a.clone() }
        }
        _ => ActionMeta::new(act.ty, act.index, now),
    }
}

/// An action's `retry` (`ActionRetry`, three exponential retries a minute apart by default).
struct Retry {
    count: i64,
    backoff: String,
    delay_ms: i64,
}

fn retry_of(act: &Act<'_>) -> Retry {
    let r = act.config.and_then(|c| c.get("retry"));
    Retry {
        count: r.and_then(|r| r.get("count")).and_then(Value::as_i64).unwrap_or(3),
        backoff: r.and_then(|r| r.get("backoff")).and_then(Value::as_str).unwrap_or("exponential").to_string(),
        delay_ms: r.and_then(|r| r.get("delay")).and_then(Value::as_str).map_or(60_000, |d| time_ms(d).unwrap_or(60_000)),
    }
}

fn time_ms(v: &str) -> Option<i64> {
    crate::units::TimeValue::parse(v, "").ok().map(|t| t.as_millis())
}

/// `Backoff.shouldBackoff`: whether a failed action still waits for its next retry.
fn should_backoff(retry: &Retry, am: Option<&ActionMeta>, now: i64) -> bool {
    let Some(am) = am else { return false };
    let Some(last) = am.last_retry_time.filter(|_| am.consumed_retries > 0) else { return false };
    let wait = match retry.backoff.as_str() {
        "constant" => retry.delay_ms,
        "linear" => am.consumed_retries * retry.delay_ms,
        _ => (2f64.powi((am.consumed_retries - 1) as i32)) as i64 * retry.delay_ms,
    };
    wait - (now - last) > 0
}

/// `Action.hasTimedOut`.
fn timed_out(act: &Act<'_>, am: &ActionMeta, now: i64) -> bool {
    let Some(start) = am.start_time else { return false };
    let Some(timeout) = act.config.and_then(|c| c.get("timeout")).and_then(Value::as_str).and_then(time_ms) else { return false };
    now - start > timeout
}

/// `Step.getStepStartTime`.
fn step_start_time(meta: &Meta, step: &str, now: i64) -> i64 {
    match &meta.step {
        Some(s) if s.name == step && s.status != Status::Completed => s.start_time,
        _ => now,
    }
}

// ------------------------------------------------------------------------------ settings

fn setting<'a>(st: &'a ClusterState, key: &str) -> Option<&'a str> {
    st.metadata.cluster_setting(&format!("plugins.index_state_management.{key}")).or_else(|| st.metadata.cluster_setting(&format!("opendistro.index_state_management.{key}")))
}

fn setting_list(st: &ClusterState, key: &str) -> Option<Vec<String>> {
    for prefix in ["plugins", "opendistro"] {
        let full = format!("{prefix}.index_state_management.{key}");
        for s in [&st.metadata.transient, &st.metadata.persistent] {
            if let Some(l) = s.get_list(&full) {
                return Some(l);
            }
        }
    }
    None
}

/// `plugins.index_state_management.enabled`.
pub(crate) fn enabled(st: &ClusterState) -> bool {
    setting(st, "enabled") != Some("false")
}

/// `plugins.index_state_management.job_interval`, in minutes.
pub(crate) fn job_interval(st: &ClusterState) -> i64 {
    setting(st, "job_interval").and_then(|v| v.parse().ok()).filter(|v: &i64| *v > 0).unwrap_or(5)
}

/// `plugins.index_state_management.jitter`, capped like the job scheduler caps it.
fn jitter(st: &ClusterState) -> f64 {
    let j = setting(st, "jitter").and_then(|v| v.parse().ok()).unwrap_or(0.6f64);
    let limit = st.metadata.cluster_setting("plugins.jobscheduler.jitter_limit").or_else(|| st.metadata.cluster_setting("opendistro.jobscheduler.jitter_limit")).and_then(|v| v.parse().ok()).unwrap_or(0.95f64);
    j.clamp(0.0, limit.max(0.0))
}

fn history_enabled(st: &ClusterState) -> bool {
    setting(st, "history.enabled") != Some("false")
}

// ------------------------------------------------------------------------------ schedule

/// `IntervalSchedule.getNextExecutionTime` after `now`, plus the job scheduler's random
/// jitter (a fraction of one interval).
pub(crate) fn next_run(st: &ClusterState, start: i64, interval_min: i64, now: i64) -> i64 {
    let interval = interval_min.max(1) * 60_000;
    let next = if now < start { start } else { now + (interval - (now - start) % interval) };
    let j = jitter(st);
    let delay = if j > 0.0 { (rand::random::<u64>() % interval as u64) as f64 * j } else { 0.0 };
    next + delay.round() as i64
}

/// When the next enabled job is due (none while ISM is disabled).
pub fn next_due(st: &ClusterState) -> Option<i64> {
    if st.metadata.xpack.len(MANAGED) == 0 || !enabled(st) {
        return None;
    }
    st.metadata.xpack.iter(MANAGED).filter(|(_, r)| r.get("enabled").and_then(Value::as_bool) != Some(false)).filter_map(|(_, r)| r.get("next_run").and_then(Value::as_i64)).min()
}

// ------------------------------------------------------------------------------ the runner

/// What a run does with a managed index, decided from the cluster state alone.
enum Plan {
    /// Nothing (the index went away, or the action waits for its retry backoff).
    Skip,
    /// Write this metadata (and maybe disable the job, or replace its policy).
    Write { meta: Meta, disable: bool, new_policy: Option<String>, clear_change: bool },
    /// Disable the job (completed or failed policy, or ISM turned off).
    Disable,
    /// Write the starting metadata, then execute the step.
    Execute { starting: Meta, step: &'static str, ty: String, config: Option<Value>, state: Value, retry: Retry },
}

/// Runs every due job once. Returns how many ran.
pub async fn run_due(cluster: &Cluster, now: i64) -> usize {
    let st = cluster.state();
    if !enabled(&st) {
        return 0;
    }
    let due: Vec<String> = st
        .metadata
        .xpack
        .iter(MANAGED)
        .filter(|(_, r)| r.get("enabled").and_then(Value::as_bool) != Some(false) && r.get("next_run").and_then(Value::as_i64).is_some_and(|t| t <= now))
        .map(|(n, _)| n.clone())
        .collect();
    drop(st);
    run(cluster, &due, now).await;
    due.len()
}

/// Runs the jobs of these managed indices once at `now` (epoch millis), due or not (what the
/// plugin's tests do by moving a job's start time). Disabled jobs don't run.
pub async fn run_at(cluster: &Cluster, indices: &[String], now: i64) {
    run(cluster, indices, now).await;
}

async fn run(cluster: &Cluster, indices: &[String], now: i64) {
    if indices.is_empty() {
        return;
    }
    let st = cluster.state();
    let plans: Vec<(String, String, Plan)> = indices
        .iter()
        .filter_map(|name| {
            let rec = ism::managed(&st, name)?;
            if rec.get("enabled").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            let uuid = rec.get("index_uuid").and_then(Value::as_str)?.to_string();
            Some((name.clone(), uuid.clone(), plan(&st, name, &uuid, rec, now)))
        })
        .collect();
    drop(st);
    if plans.is_empty() {
        return;
    }
    let interval = job_interval(&cluster.state());
    // Phase one: every decision that writes without executing, and the starting metadata.
    let mut history: Vec<Meta> = Vec::new();
    let first: Vec<(String, String, &Plan)> = plans.iter().map(|(n, u, p)| (n.clone(), u.clone(), p)).collect();
    let written = cluster
        .update(|st| {
            let mut written = Vec::new();
            for (name, uuid, plan) in &first {
                if !same_index(st, name, uuid) {
                    continue;
                }
                let next = next_run(st, record_start(st, name), interval, now);
                match plan {
                    Plan::Skip => set_fields(st, name, |r| {
                        r.insert("next_run".into(), Value::from(next));
                    }),
                    Plan::Disable => {
                        ism::next_seq(st);
                        set_fields(st, name, |r| disable(r, now));
                    }
                    Plan::Write { meta, disable: off, new_policy, clear_change } => {
                        let seq = ism::next_seq(st);
                        let policy = new_policy.as_deref().and_then(|p| Some((p, ism::keep_policy_copy(st, p)?)));
                        set_fields(st, name, |r| {
                            r.insert("metadata".into(), meta.to_stored());
                            r.insert("next_run".into(), Value::from(next));
                            r.insert("metadata_seq_no".into(), Value::from(seq));
                            if let Some((id, seq)) = &policy {
                                r.insert("policy_id".into(), Value::Str(id.to_string()));
                                r.insert("policy_seq_no".into(), Value::from(*seq));
                            }
                            if *clear_change {
                                r.shift_remove("change_policy");
                            }
                            if *off {
                                disable(r, now);
                            }
                        });
                        if policy.is_some() {
                            ism::next_seq(st);
                            ism::gc_policy_copies(st);
                        }
                        if *off {
                            ism::next_seq(st);
                        }
                        written.push(meta.clone());
                    }
                    Plan::Execute { starting, .. } => {
                        let seq = ism::next_seq(st);
                        set_fields(st, name, |r| {
                            r.insert("metadata".into(), starting.to_stored());
                            r.insert("next_run".into(), Value::from(next));
                            r.insert("metadata_seq_no".into(), Value::from(seq));
                        });
                    }
                }
            }
            Ok(written)
        })
        .await
        .unwrap_or_default();
    history.extend(written);
    // Phase two: execute the steps, one index after the other, then write their outcomes.
    let mut outcomes: Vec<(String, String, Meta, bool)> = Vec::new();
    for (name, uuid, plan) in &plans {
        let Plan::Execute { starting, step, ty, config, state, retry } = plan else { continue };
        let env = Env { cluster, index: name, uuid, meta: starting, config: config.as_ref(), state, now };
        let out = execute(&env, step).await;
        let mut done = completed(starting, step, ty, retry, out, now);
        let st = cluster.state();
        let policy = ism::managed(&st, name).and_then(|r| ism::job_policy(&st, r)).cloned();
        drop(st);
        if done.is_failed() {
            done = publish_error_notification(cluster, policy.as_ref(), done).await;
        }
        let deleted = is_successful_delete(&done);
        outcomes.push((name.clone(), uuid.clone(), done, deleted));
    }
    if !outcomes.is_empty() {
        let saved = cluster
            .update(|st| {
                let mut saved = Vec::new();
                for (name, uuid, meta, deleted) in &outcomes {
                    if *deleted {
                        saved.push(meta.clone());
                        continue;
                    }
                    if !same_index(st, name, uuid) {
                        continue;
                    }
                    let seq = ism::next_seq(st);
                    let changes_interval = ism::managed(st, name).and_then(|r| r.get("job_interval")).and_then(Value::as_i64) != Some(interval);
                    set_fields(st, name, |r| {
                        r.insert("metadata".into(), meta.to_stored());
                        r.insert("metadata_seq_no".into(), Value::from(seq));
                        r.insert("job_interval".into(), Value::from(interval));
                    });
                    if changes_interval {
                        ism::next_seq(st);
                    }
                    saved.push(meta.clone());
                }
                Ok(saved)
            })
            .await
            .unwrap_or_default();
        history.extend(saved);
    }
    if !history.is_empty() && history_enabled(&cluster.state()) {
        write_history(cluster, &history, now).await;
    }
}

fn same_index(st: &ClusterState, name: &str, uuid: &str) -> bool {
    ism::managed(st, name).and_then(|r| r.get("index_uuid")).and_then(Value::as_str) == Some(uuid)
}

fn record_start(st: &ClusterState, name: &str) -> i64 {
    ism::managed(st, name).and_then(|r| r.get("schedule_start").and_then(Value::as_i64)).unwrap_or(0)
}

fn set_fields(st: &mut ClusterState, name: &str, f: impl FnOnce(&mut Map)) {
    if let Some(Value::Obj(r)) = store::family_mut(st, MANAGED).get_mut(name) {
        f(r);
    }
}

/// `updateDisableManagedIndexRequest`: `enabled: false`, no enabled time.
fn disable(r: &mut Map, now: i64) {
    r.insert("enabled".into(), Value::Bool(false));
    r.insert("enabled_time".into(), Value::Null);
    r.insert("last_updated_time".into(), Value::from(now));
}

/// `runManagedIndexConfig` up to the step execution.
fn plan(st: &ClusterState, name: &str, uuid: &str, rec: &Value, now: i64) -> Plan {
    let Some(idx) = st.metadata.indices.get(name).filter(|i| i.uuid == uuid) else { return Plan::Skip };
    let policy = ism::job_policy(st, rec);
    let Some(meta) = meta_of(rec) else {
        return init_plan(st, name, uuid, rec, idx.creation_date(), now);
    };
    if meta.policy_completed == Some(true) || meta.is_failed() {
        return Plan::Disable;
    }
    let Some(policy) = policy else { return Plan::Skip };
    let config_seq = rec.get("policy_seq_no").and_then(Value::as_i64);
    if meta.policy_seq_no != config_seq || meta.policy_primary_term != config_seq.map(|_| 1) {
        let mut m = meta.clone();
        m.retry_info = Some(RetryInfo { failed: true, consumed_retries: 0 });
        m.info = Some(info_of("There is a version conflict between your previous execution and your managed index"));
        return Plan::Write { meta: m, disable: true, new_policy: None, clear_change: false };
    }
    let state = state_to_execute(policy, &meta);
    let act = state.and_then(|s| action_to_execute(s, &meta));
    let state_name = state.and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or_default();
    let step = act.as_ref().map(|a| step_to_execute(a.ty, &meta));
    let current = act.as_ref().map(|a| updated_action_meta(a, &meta, state_name, now));
    if !enabled(st) && step.is_some() {
        return Plan::Disable;
    }
    if let (Some(a), Some(am)) = (&act, &current)
        && timed_out(a, am, now)
    {
        let mut m = meta.clone();
        m.action = Some(ActionMeta { failed: true, ..am.clone() });
        m.step = step.map(|s| StepMeta { name: s.to_string(), start_time: now, status: Status::TimedOut });
        m.info = Some(info_of("Action timed out"));
        return Plan::Write { meta: m, disable: true, new_policy: None, clear_change: false };
    }
    if let Some(change) = rec.get("change_policy").filter(|c| !matches!(c, Value::Null)) {
        let safe = change.get("is_safe").and_then(Value::as_bool) == Some(true);
        if safe || act.as_ref().is_some_and(|a| a.ty == TRANSITION) {
            return change_policy_plan(st, &meta, policy, change, act.as_ref(), now);
        }
    }
    if let Some(a) = &act
        && should_backoff(&retry_of(a), current.as_ref(), now)
    {
        return Plan::Skip;
    }
    if meta.step.as_ref().is_some_and(|s| s.status == Status::Starting) && step.is_some_and(|s| !idempotent(s)) {
        let mut m = meta.clone();
        m.retry_info = Some(RetryInfo { failed: true, consumed_retries: 0 });
        m.info = Some(info_of("Previous action was not able to update IndexMetaData."));
        return Plan::Write { meta: m, disable: true, new_policy: None, clear_change: false };
    }
    if let (Some(a), Some(s)) = (&act, step)
        && a.ty != TRANSITION
        && steps_of(a.ty)[0] == s
        && !allowed(st, a.ty)
    {
        let mut m = meta.clone();
        m.retry_info = Some(RetryInfo { failed: true, consumed_retries: 0 });
        m.info = Some(info_of(format!("Attempted to execute action={} which is not allowed.", a.ty)));
        return Plan::Write { meta: m, disable: true, new_policy: None, clear_change: false };
    }
    // `getStartingManagedIndexMetaData`.
    let Some(state) = state else {
        let mut m = meta.clone();
        m.retry_info = Some(RetryInfo { failed: true, consumed_retries: 0 });
        let what = meta.transition_to.clone().unwrap_or_else(|| meta.state.as_ref().map_or("null".into(), StateMeta::debug));
        m.info = Some(info_of(format!("Failed to find state={what} in policy={}", meta.policy_id)));
        return Plan::Write { meta: m, disable: false, new_policy: None, clear_change: false };
    };
    let (Some(act), Some(step)) = (act, step) else {
        let mut m = meta.clone();
        m.retry_info = Some(RetryInfo { failed: true, consumed_retries: 0 });
        m.info = Some(info_of(format!(
            "Failed to find action={} in state={}",
            meta.action.as_ref().map_or("null".into(), ActionMeta::debug),
            meta.state.as_ref().map_or("null".into(), StateMeta::debug)
        )));
        return Plan::Write { meta: m, disable: false, new_policy: None, clear_change: false };
    };
    let mut starting = meta.clone();
    starting.state = Some(match &meta.state {
        Some(s) if s.name == state_name => s.clone(),
        _ => StateMeta { name: state_name.to_string(), start_time: now },
    });
    starting.action = current;
    starting.step = Some(StepMeta { name: step.to_string(), start_time: step_start_time(&meta, step, now), status: Status::Starting });
    starting.info = Some(info_of(format!("Starting action {} and working on {step}", act.ty)));
    Plan::Execute { starting, step, ty: act.ty.to_string(), config: act.config.cloned(), state: state.clone(), retry: retry_of(&act) }
}

/// `initManagedIndex`: the first metadata, for the job's policy or a pending change's.
fn init_plan(st: &ClusterState, name: &str, uuid: &str, rec: &Value, created: i64, now: i64) -> Plan {
    let change = rec.get("change_policy").filter(|c| !matches!(c, Value::Null));
    let (policy_id, policy, seq) = match change {
        Some(c) => {
            let id = c.get("policy_id").and_then(Value::as_str).unwrap_or_default().to_string();
            let s = ism::stored(st, &id);
            let seq = s.as_ref().map(|s| s.seq_no);
            (id, s.map(|s| s.policy), seq)
        }
        None => (
            rec.get("policy_id").and_then(Value::as_str).unwrap_or_default().to_string(),
            ism::job_policy(st, rec).cloned(),
            rec.get("policy_seq_no").and_then(Value::as_i64),
        ),
    };
    let mut meta = Meta { index: name.to_string(), index_uuid: uuid.to_string(), policy_id: policy_id.clone(), index_creation_date: Some(created), ..Meta::default() };
    meta.policy_completed = Some(false);
    meta.rolled_over = Some(false);
    match policy {
        None => {
            meta.retry_info = Some(RetryInfo { failed: true, consumed_retries: 0 });
            meta.info = Some(info_of(format!("Fail to load policy: {policy_id}")));
        }
        Some(p) => {
            let state = change.and_then(|c| c.get("state")).and_then(Value::as_str).map(str::to_string).or_else(|| p.get("default_state").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
            meta.policy_seq_no = seq;
            meta.policy_primary_term = Some(1);
            meta.state = Some(StateMeta { name: state, start_time: now });
            meta.retry_info = Some(RetryInfo { failed: false, consumed_retries: 0 });
            meta.info = Some(info_of(format!("Successfully initialized policy: {policy_id}")));
        }
    }
    let new_policy = change.is_some().then_some(policy_id).filter(|id| ism::stored(st, id).is_some());
    let clear = new_policy.is_some();
    Plan::Write { meta, disable: false, new_policy, clear_change: clear }
}

/// `initChangePolicy`.
fn change_policy_plan(st: &ClusterState, meta: &Meta, current: &Map, change: &Value, act: Option<&Act<'_>>, now: i64) -> Plan {
    let id = change.get("policy_id").and_then(Value::as_str).unwrap_or_default();
    let target_state = change.get("state").and_then(Value::as_str).map(str::to_string);
    let Some(new) = ism::stored(st, id) else {
        let mut m = meta.clone();
        m.info = Some(info_of(format!("Failed to load change policy: {id}")));
        m.retry_info = Some(RetryInfo { failed: true, consumed_retries: 0 });
        return Plan::Write { meta: m, disable: false, new_policy: None, clear_change: false };
    };
    if change.get("is_safe").and_then(Value::as_bool) == Some(true) && !safe_to_change(current, &new.policy, meta.state_name(), target_state.is_some()) {
        // The plugin flags the change unsafe and waits for the transition action.
        return Plan::Skip;
    }
    let mut m = meta.clone();
    m.info = Some(info_of(format!("Attempting to change policy to {id}")));
    m.transition_to = target_state;
    if act.is_some_and(|a| a.ty == TRANSITION) {
        m.action = Some(ActionMeta { name: TRANSITION.into(), start_time: Some(now), index: -1, failed: false, consumed_retries: 0, last_retry_time: Some(0), properties: None });
    }
    m.step = None;
    m.policy_completed = Some(false);
    m.policy_seq_no = Some(new.seq_no);
    m.policy_primary_term = Some(1);
    m.policy_id = id.to_string();
    Plan::Write { meta: m, disable: false, new_policy: Some(id.to_string()), clear_change: true }
}

/// `Policy.isSafeToChange`.
pub(crate) fn safe_to_change(current: &Map, new: &Map, state: Option<&str>, has_state: bool) -> bool {
    let Some(state) = state else { return true };
    if has_state {
        return false;
    }
    let (Some(a), Some(b)) = (find_state(current, state), find_state(new, state)) else { return false };
    let (a, b) = (list(a, "actions"), list(b, "actions"));
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| action_type(x) == action_type(y))
}

fn allowed(st: &ClusterState, ty: &str) -> bool {
    match setting_list(st, "allow_list") {
        Some(l) => l.iter().any(|a| a == ty),
        None => true,
    }
}

/// `Step.isIdempotent`: steps that start work which can't be repeated safely.
fn idempotent(step: &str) -> bool {
    !matches!(step, "attempt_call_force_merge" | "attempt_snapshot" | "attempt_notification" | "attempt_shrink_step" | "attempt_create_rollup" | "attempt_create_transform" | "attempt_restore")
}

fn is_successful_delete(m: &Meta) -> bool {
    m.action.as_ref().is_some_and(|a| a.name == "delete" && !a.failed)
        && m.step.as_ref().is_some_and(|s| s.name == "attempt_delete" && s.status == Status::Completed)
        && !m.retry_info.as_ref().is_some_and(|r| r.failed)
}

// ------------------------------------------------------------------------------ steps

/// What a step reports (`getUpdatedManagedIndexMetadata`'s inputs).
struct Outcome {
    status: Status,
    info: Option<Map>,
    /// The transition step's target state.
    transition_to: Option<String>,
    /// Set by the transition step (`policyCompleted` is written either way).
    policy_completed: Option<bool>,
    /// Rollover: the index created by this run.
    new_index: Option<String>,
    /// Action properties: `None` keeps them, `Some(None)` clears them.
    properties: Option<Option<Map>>,
}

impl Outcome {
    fn new(status: Status, info: Option<Map>) -> Outcome {
        Outcome { status, info, transition_to: None, policy_completed: None, new_index: None, properties: None }
    }

    fn done(message: String) -> Outcome {
        Outcome::new(Status::Completed, Some(info_of(message)))
    }

    fn waiting(message: String) -> Outcome {
        Outcome::new(Status::ConditionNotMet, Some(info_of(message)))
    }

    /// `handleException`: the step's failure message and the exception's message.
    fn failed(message: String, cause: Option<String>) -> Outcome {
        let mut info = info_of(message);
        if let Some(c) = cause {
            info.insert("cause".into(), Value::Str(c));
        }
        Outcome::new(Status::Failed, Some(info))
    }
}

struct Env<'a> {
    cluster: &'a Cluster,
    index: &'a str,
    uuid: &'a str,
    /// The starting metadata.
    meta: &'a Meta,
    config: Option<&'a Value>,
    state: &'a Value,
    now: i64,
}

impl Env<'_> {
    fn conf(&self, ty: &str) -> Option<&Value> {
        self.config.and_then(|c| c.get(ty))
    }
}

/// An internal request through roll's REST layer: the response body, or the failure's reason
/// (what the plugin reports as an exception's message).
async fn call(cluster: &Cluster, method: &str, uri: &str, body: Option<Value>) -> Result<Value, Failure> {
    let req = match body {
        Some(b) => EsRequest::json(method, uri, b.to_json()),
        None => EsRequest::new(method, uri),
    };
    let resp = crate::rest::handle(cluster, req).await;
    let v = crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null);
    if resp.status >= 400 {
        let err = v.get("error");
        return Err(Failure {
            kind: err.and_then(|e| e.get("type")).and_then(Value::as_str).unwrap_or_default().to_string(),
            reason: err.and_then(|e| e.get("reason")).and_then(Value::as_str).map(str::to_string),
        });
    }
    Ok(v)
}

struct Failure {
    kind: String,
    reason: Option<String>,
}

/// A name as one URL path segment.
fn path(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn settings_step(env: &Env<'_>, settings: Value, ok: String, fail: String) -> Outcome {
    match call(env.cluster, "PUT", &format!("/{}/_settings", path(env.index)), Some(settings)).await {
        Ok(v) if v.get("acknowledged").and_then(Value::as_bool) != Some(false) => Outcome::done(ok),
        Ok(_) => Outcome::new(Status::Failed, Some(info_of(fail))),
        Err(e) => Outcome::failed(fail, e.reason),
    }
}

async fn execute(env: &Env<'_>, step: &str) -> Outcome {
    let i = env.index;
    match step {
        "attempt_transition_step" => transition(env).await,
        "attempt_rollover" => rollover(env).await,
        "attempt_delete" => match call(env.cluster, "DELETE", &format!("/{}", path(i)), None).await {
            Ok(_) => Outcome::done(format!("Successfully deleted index [index={i}]")),
            Err(e) if e.kind == "snapshot_in_progress_exception" => Outcome::waiting(format!("Index had snapshot in progress, retrying deletion [index={i}]")),
            Err(e) => Outcome::failed(format!("Failed to delete index [index={i}]"), e.reason),
        },
        "set_read_only" | "attempt_set_read_only" => {
            settings_step(env, obj! { "index.blocks.write" => true }, format!("Successfully set index to read-only [index={i}]"), format!("Failed to set index to read-only [index={i}]")).await
        }
        "set_read_write" => {
            settings_step(env, obj! { "index.blocks.write" => false }, format!("Successfully set index to read-write [index={i}]"), format!("Failed to set index to read-write [index={i}]")).await
        }
        "attempt_set_replica_count" => {
            let n = env.conf("replica_count").and_then(|c| c.get("number_of_replicas")).and_then(Value::as_i64).unwrap_or(0);
            settings_step(
                env,
                obj! { "index.number_of_replicas" => n },
                format!("Successfully set number_of_replicas to {n} [index={i}]"),
                format!("Failed to set number_of_replicas to {n} [index={i}]"),
            )
            .await
        }
        "attempt_set_index_priority" => {
            let n = env.conf("index_priority").and_then(|c| c.get("priority")).and_then(Value::as_i64).unwrap_or(0);
            settings_step(env, obj! { "index.priority" => n }, format!("Successfully set index priority to {n} [index={i}]"), format!("Failed to set index priority to {n} [index={i}]")).await
        }
        "attempt_allocation" => {
            let mut s = Map::new();
            for kind in ["require", "include", "exclude"] {
                if let Some(Value::Obj(m)) = env.conf("allocation").and_then(|c| c.get(kind)) {
                    for (k, v) in m {
                        s.insert(format!("index.routing.allocation.{kind}.{k}"), Value::Str(v.to_es_string()));
                    }
                }
            }
            settings_step(env, Value::Obj(s), format!("Successfully updated allocation setting [index={i}]"), format!("Failed to update allocation setting [index={i}]")).await
        }
        "attempt_close" => match call(env.cluster, "POST", &format!("/{}/_close", path(i)), None).await {
            Ok(_) => Outcome::done(format!("Successfully closed index [index={i}]")),
            Err(e) if e.kind == "snapshot_in_progress_exception" => Outcome::waiting(format!("Index had snapshot in progress, retrying closing [index={i}]")),
            Err(e) => Outcome::failed(format!("Failed to close index [index={i}]"), e.reason),
        },
        "attempt_open" => match call(env.cluster, "POST", &format!("/{}/_open", path(i)), None).await {
            Ok(_) => Outcome::done(format!("Successfully opened index [index={i}]")),
            Err(e) => Outcome::failed(format!("Failed to open index [index={i}]"), e.reason),
        },
        "attempt_alias" => alias(env).await,
        "attempt_call_force_merge" => {
            let n = env.conf("force_merge").and_then(|c| c.get("max_num_segments")).and_then(Value::as_i64).unwrap_or(1);
            let mut out = match call(env.cluster, "POST", &format!("/{}/_forcemerge?max_num_segments={n}", path(i)), None).await {
                Ok(_) => Outcome::done(format!("Successfully completed force merge [index={i}]")),
                Err(e) => Outcome::failed(format!("Failed to start force merge [index={i}]"), e.reason),
            };
            let mut p = Map::new();
            p.insert("max_num_segments".into(), Value::from(n));
            out.properties = Some(Some(p));
            out
        }
        "wait_for_force_merge" => wait_for_force_merge(env).await,
        "attempt_snapshot" => snapshot(env).await,
        "wait_for_snapshot" => wait_for_snapshot(env).await,
        "attempt_notification" => {
            let fail = format!("Failed to send notification [index={i}]");
            Outcome::failed(fail, Some(NO_CONNECTIVITY.into()))
        }
        other => Outcome::failed(format!("Failed to execute action step [{other}] [index={i}]"), Some(format!("roll search does not run the [{other}] step"))),
    }
}

/// Why notification destinations fail: roll search has no outbound connectivity (the same
/// rule as Watcher's webhook and HTTP inputs).
const NO_CONNECTIVITY: &str = "notification destinations need external connectivity, which roll search does not provide";

/// `AttemptTransitionStep`.
async fn transition(env: &Env<'_>) -> Outcome {
    let i = env.index;
    let transitions = list(env.state, "transitions");
    if transitions.is_empty() {
        let mut o = Outcome::new(Status::Completed, None);
        o.policy_completed = Some(true);
        return o;
    }
    let st = env.cluster.state();
    let idx = st.metadata.indices.get(i).filter(|x| x.uuid == env.uuid);
    let created = env.meta.index_creation_date.or_else(|| idx.map(|x| x.creation_date())).unwrap_or(-1);
    let rollover_date = idx.and_then(|_| ism::managed(&st, i)).and_then(|r| r.get("rolled_over_at")).and_then(Value::as_object).and_then(|m| m.values().filter_map(Value::as_i64).min());
    let cond = |t: &Value, k: &str| t.get("conditions").and_then(|c| c.get(k)).cloned();
    let mut o = Outcome::new(Status::Starting, None);
    o.policy_completed = Some(false);
    if transitions.iter().any(|t| cond(t, "min_rollover_age").is_some()) && rollover_date.is_none() {
        o.status = Status::Failed;
        o.info = Some(info_of(format!("Failed to transition index as min_rollover_age condition was used, but the index has never been rolled over [index={i}]")));
        return o;
    }
    let (mut docs, mut size) = (None, None);
    if idx.is_some() && transitions.iter().any(|t| cond(t, "min_doc_count").is_some() || cond(t, "min_size").is_some()) {
        match docs_stats(env.cluster, i).await {
            Ok((d, s, _)) => (docs, size) = (Some(d), Some(s)),
            Err(e) => {
                o.status = Status::Failed;
                let mut info = info_of(format!("Failed to transition index [index={i}]"));
                if let Some(c) = e.reason {
                    info.insert("cause".into(), Value::Str(c));
                }
                o.info = Some(info);
                return o;
            }
        }
    }
    let aliases = idx.map_or(0, |x| x.aliases.len());
    let step_start = env.meta.step.as_ref().map_or(env.now, |s| s.start_time);
    let state_start = env.meta.state.as_ref().map(|s| s.start_time);
    let now = env.now;
    let met = |t: &Value| -> bool {
        let Some(Value::Obj(c)) = t.get("conditions") else { return true };
        if let (Some(want), Some(have)) = (c.get("min_doc_count").and_then(Value::as_i64), docs)
            && want <= have
        {
            return true;
        }
        if let Some(age) = c.get("min_index_age").and_then(Value::as_str).and_then(time_ms)
            && created != -1
            && age <= now - created
        {
            return true;
        }
        if let (Some(want), Some(have)) = (c.get("min_size").and_then(Value::as_str).and_then(|s| crate::units::ByteSize::parse(s, "").ok()), size)
            && want.bytes() <= have
        {
            return true;
        }
        if let Some(cron) = c.get("cron").and_then(|c| c.get("cron"))
            && let Some(next) = super::cron::next_after(cron.get("expression").and_then(Value::as_str).unwrap_or(""), cron.get("timezone").and_then(Value::as_str).unwrap_or("UTC"), step_start)
            && next <= now
        {
            return true;
        }
        if let (Some(age), Some(rolled)) = (c.get("min_rollover_age").and_then(Value::as_str).and_then(time_ms), rollover_date)
            && age <= now - rolled
        {
            return true;
        }
        if let Some(no_alias) = c.get("no_alias").and_then(Value::as_bool)
            && ((no_alias && aliases == 0) || (!no_alias && aliases > 0))
        {
            return true;
        }
        if let (Some(age), Some(start)) = (c.get("min_state_age").and_then(Value::as_str).and_then(time_ms), state_start)
            && now - start >= age
        {
            return true;
        }
        false
    };
    match transitions.iter().find(|t| met(t)).and_then(|t| t.get("state_name")).and_then(Value::as_str) {
        Some(to) => {
            o.status = Status::Completed;
            o.info = Some(info_of(format!("Transitioning to {to} [index={i}]")));
            o.transition_to = Some(to.to_string());
        }
        None => {
            o.status = Status::ConditionNotMet;
            o.info = Some(info_of(format!("Evaluating transition conditions [index={i}]")));
        }
    }
    o
}

/// Primary docs, primary store bytes and the largest primary shard's bytes (with its shard id).
async fn docs_stats(cluster: &Cluster, index: &str) -> Result<(i64, i64, (i64, Option<i64>)), Failure> {
    let v = call(cluster, "GET", &format!("/{}/_stats/docs,store?level=shards", path(index)), None).await?;
    let p = v.get("_all").and_then(|a| a.get("primaries"));
    let docs = p.and_then(|p| p.get("docs")).and_then(|d| d.get("count")).and_then(Value::as_i64).unwrap_or(0);
    let size = p.and_then(|p| p.get("store")).and_then(|d| d.get("size_in_bytes")).and_then(Value::as_i64).unwrap_or(0);
    let mut largest = (0i64, None);
    if let Some(Value::Obj(shards)) = v.get("indices").and_then(|x| x.get(index)).and_then(|x| x.get("shards")) {
        for (id, copies) in shards {
            for c in copies.as_array().into_iter().flatten() {
                if c.get("routing").and_then(|r| r.get("primary")).and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                let b = c.get("store").and_then(|s| s.get("size_in_bytes")).and_then(Value::as_i64).unwrap_or(0);
                if largest.1.is_none() || b > largest.0 {
                    largest = (b, id.parse().ok());
                }
            }
        }
    }
    Ok((docs, size, largest))
}

fn time_human(ms: i64) -> String {
    crate::units::TimeValue::millis(ms).human()
}

/// `AttemptRolloverStep`.
async fn rollover(env: &Env<'_>) -> Outcome {
    let i = env.index;
    let conf = env.conf("rollover").cloned().unwrap_or(Value::Null);
    let st = env.cluster.state();
    let Some(idx) = st.metadata.indices.get(i) else { return Outcome::failed(format!("Failed to rollover index [index={i}]"), Some(format!("no such index [{i}]"))) };
    let flag = |k: &str| idx.settings.get(&format!("index.plugins.index_state_management.{k}")).or_else(|| idx.settings.get(&format!("index.opendistro.index_state_management.{k}")));
    if flag("rollover_skip") == Some("true") {
        return Outcome::done(format!("Skipped rollover action for [index={i}]"));
    }
    let data_stream = st.metadata.data_stream_of(i).map(|d| d.name.clone());
    let target = match &data_stream {
        Some(ds) => ds.clone(),
        None => match flag("rollover_alias") {
            Some(a) => a.to_string(),
            None => return Outcome::new(Status::Failed, Some(info_of(format!("Missing rollover_alias index setting [index={i}]")))),
        },
    };
    let rolled_at = ism::managed(&st, i).and_then(|r| r.get("rolled_over_at")).and_then(|m| m.get(&target)).is_some();
    if rolled_at {
        let o = Outcome::done(format!("This index has already been rolled over using this alias, treating as a success [index={i}, alias={target}]"));
        return copy_alias(env, &conf, &target, None, o).await;
    }
    if data_stream.is_none() {
        let ok = match idx.aliases.get(&target) {
            None => false,
            Some(a) if a.is_write_index == Some(true) => true,
            Some(_) => st.metadata.indices.values().filter(|x| x.aliases.contains_key(&target)).count() <= 1,
        };
        if !ok {
            return Outcome::new(Status::Failed, Some(info_of(format!("Missing alias or not the write index when rollover [index={i}]"))));
        }
    }
    let created = idx.creation_date();
    drop(st);
    let (docs, size, (largest, shard)) = match docs_stats(env.cluster, i).await {
        Ok(s) => s,
        Err(e) => return Outcome::failed(format!("Failed to evaluate conditions for rollover [index={i}]"), e.reason),
    };
    if conf.get("prevent_empty_rollover").and_then(Value::as_bool) == Some(true) && docs == 0 {
        let mut info = info_of(format!("Rollover prevented: index is empty (0 documents) [index={i}]"));
        info.insert("conditions".into(), obj! { "prevent_empty_rollover" => obj! { "condition" => "index must have at least 1 document", "current" => 0 } });
        return Outcome::new(Status::ConditionNotMet, Some(info));
    }
    let age = if created == -1 { 0 } else { env.now - created };
    let entries = |c: &Value| -> (Map, Vec<bool>) {
        let mut m = Map::new();
        let mut results = Vec::new();
        if let Some(a) = c.get("min_index_age").and_then(Value::as_str) {
            let want = time_ms(a).unwrap_or(0);
            m.insert("min_index_age".into(), obj! { "condition" => time_human(want), "current" => time_human(age), "creationDate" => created });
            results.push(want <= age);
        }
        if let Some(d) = c.get("min_doc_count").and_then(Value::as_i64) {
            m.insert("min_doc_count".into(), obj! { "condition" => d, "current" => docs });
            results.push(d <= docs);
        }
        if let Some(s) = c.get("min_size").and_then(Value::as_str) {
            let want = crate::units::ByteSize::parse(s, "").map_or(0, |b| b.bytes());
            m.insert("min_size".into(), obj! { "condition" => crate::units::ByteSize(want).human(), "current" => crate::units::ByteSize(size).human() });
            results.push(want <= size);
        }
        if let Some(s) = c.get("min_primary_shard_size").and_then(Value::as_str) {
            let want = crate::units::ByteSize::parse(s, "").map_or(0, |b| b.bytes());
            m.insert(
                "min_primary_shard_size".into(),
                obj! { "condition" => crate::units::ByteSize(want).human(), "current" => crate::units::ByteSize(largest).human(), "shard" => opt_i64(shard) },
            );
            results.push(want <= largest);
        }
        (m, results)
    };
    // Kotlin `mapOf` / `listOfNotNull(...).toMap()`: a `LinkedHashMap`, but explain round trips
    // it through a `HashMap` (see `Meta::explain_into`).
    let (conditions, met) = match conf.get("any_of").and_then(Value::as_array).filter(|g| !g.is_empty()) {
        Some(groups) => {
            let parts: Vec<(Map, Vec<bool>)> = groups.iter().map(entries).collect();
            let met = parts.iter().any(|(_, r)| r.iter().all(|x| *x));
            (obj! { "any_of" => Value::Arr(parts.into_iter().map(|(m, _)| Value::Obj(m)).collect()) }.as_object().cloned().unwrap_or_default(), met)
        }
        None => {
            let (m, r) = entries(&conf);
            let met = r.is_empty() || r.iter().any(|x| *x);
            (m, met)
        }
    };
    if !met {
        let mut info = info_of(format!("Pending rollover of index [index={i}]"));
        info.insert("conditions".into(), Value::Obj(conditions));
        return Outcome::new(Status::ConditionNotMet, Some(info));
    }
    let with_conditions = |message: String| {
        let mut info = info_of(message);
        if !conditions.is_empty() {
            info.insert("conditions".into(), Value::Obj(conditions.clone()));
        }
        info
    };
    match call(env.cluster, "POST", &format!("/{}/_rollover", path(&target)), None).await {
        Ok(v) => {
            let new_index = v.get("new_index").and_then(Value::as_str).map(str::to_string);
            if v.get("acknowledged").and_then(Value::as_bool) == Some(false) {
                let msg = match &data_stream {
                    Some(ds) => format!("Failed to rollover data stream [data_stream={ds}]"),
                    None => format!("New index created, but failed to update alias [index={i}, newIndex={}]", new_index.clone().unwrap_or_default()),
                };
                return Outcome::new(Status::Failed, Some(with_conditions(msg)));
            }
            mark_rolled_over(env.cluster, i, env.uuid, &target, env.now).await;
            let msg = match &data_stream {
                Some(ds) => format!("Successfully rolled over data stream [data_stream={ds} index={i}]"),
                None => format!("Successfully rolled over index [index={i}]"),
            };
            let mut o = Outcome::new(Status::Completed, Some(with_conditions(msg)));
            o.new_index = new_index.clone();
            copy_alias(env, &conf, &target, new_index, o).await
        }
        Err(e) => {
            let mut info = info_of(format!("Failed to rollover index [index={i}]"));
            if let Some(c) = e.reason {
                info.insert("cause".into(), Value::Str(c));
            }
            Outcome::new(Status::Failed, Some(info))
        }
    }
}

/// Remembers that the index was rolled over for `target` (`IndexMetadata.rolloverInfos`).
async fn mark_rolled_over(cluster: &Cluster, index: &str, uuid: &str, target: &str, now: i64) {
    let _ = cluster
        .update(|st| {
            if same_index(st, index, uuid) {
                set_fields(st, index, |r| {
                    let mut m = r.get("rolled_over_at").and_then(Value::as_object).cloned().unwrap_or_default();
                    m.insert(target.to_string(), Value::from(now));
                    r.insert("rolled_over_at".into(), Value::Obj(m));
                });
            }
            Ok(())
        })
        .await;
}

/// The rollover action's `copy_alias`: the index's other aliases go to the new index.
async fn copy_alias(env: &Env<'_>, conf: &Value, target: &str, new_index: Option<String>, mut o: Outcome) -> Outcome {
    if conf.get("copy_alias").and_then(Value::as_bool) != Some(true) {
        return o;
    }
    let i = env.index;
    let conditions = o.info.as_ref().and_then(|m| m.get("conditions")).or_else(|| env.meta.info.as_ref().and_then(|m| m.get("conditions"))).cloned();
    let with = |message: String| {
        let mut info = info_of(message);
        if let Some(c) = &conditions {
            info.insert("conditions".into(), c.clone());
        }
        info
    };
    let Some(rolled) = new_index.or_else(|| env.meta.rolled_over_index_name.clone()) else {
        o.status = Status::Completed;
        o.info = Some(with(format!("Successfully rolled over [index={i}] but ISM cannot find rolled over index from metadata to copy aliases to, please manually copy")));
        return o;
    };
    let st = env.cluster.state();
    let actions: Vec<Value> = st
        .metadata
        .indices
        .get(i)
        .map(|x| {
            x.aliases
                .iter()
                .filter(|(n, _)| n.as_str() != target)
                .map(|(n, a)| {
                    let mut add = a.to_value().as_object().cloned().unwrap_or_default();
                    add.shift_remove("is_write_index");
                    add.insert("index".into(), Value::Str(rolled.clone()));
                    add.insert("alias".into(), Value::Str(n.clone()));
                    obj! { "add" => Value::Obj(add) }
                })
                .collect()
        })
        .unwrap_or_default();
    drop(st);
    if actions.is_empty() {
        o.status = Status::Completed;
        o.info = Some(with(format!("Successfully rolled over index [index={i}] to [index={rolled}], no additional aliases to copy")));
        return o;
    }
    match call(env.cluster, "POST", "/_aliases", Some(obj! { "actions" => Value::Arr(actions) })).await {
        Ok(_) => {
            o.status = Status::Completed;
            o.info = Some(with(format!("Successfully rolled over and copied alias from [index={i}] to [index={rolled}]")));
        }
        Err(e) if e.kind == "index_not_found_exception" => {
            o.status = Status::Failed;
            o.info = Some(with(format!("Successfully rolled over but new index [index={rolled}] not found during copy alias")));
        }
        Err(e) => {
            o.status = Status::Failed;
            let mut info = with(format!("Successfully rolled over but failed to copy alias from [index={i}] to [index={rolled}]"));
            if let Some(c) = e.reason {
                info.insert("cause".into(), Value::Str(c));
            }
            o.info = Some(info);
        }
    }
    o
}

/// `AttemptAliasActionsStep`: the action's alias actions, each applied to this index.
async fn alias(env: &Env<'_>) -> Outcome {
    let i = env.index;
    let actions: Vec<Value> = env
        .conf("alias")
        .and_then(|c| c.get("actions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| match a {
            Value::Obj(m) => Value::Obj(
                m.iter()
                    .map(|(k, v)| {
                        let mut body = v.as_object().cloned().unwrap_or_default();
                        body.insert("index".into(), Value::Str(i.to_string()));
                        (k.clone(), Value::Obj(body))
                    })
                    .collect(),
            ),
            other => other.clone(),
        })
        .collect();
    let fail = format!("Failed to update alias [index={i}] for actions: [actions={}]", Value::Arr(actions.clone()).to_json());
    match call(env.cluster, "POST", "/_aliases", Some(obj! { "actions" => Value::Arr(actions) })).await {
        Ok(_) => Outcome::done(format!("Successfully updated alias [index={i}]")),
        Err(e) => Outcome::failed(fail, e.reason),
    }
}

/// `WaitForForceMergeStep`: done once no shard has more segments than asked for.
async fn wait_for_force_merge(env: &Env<'_>) -> Outcome {
    let i = env.index;
    let Some(max) = env.meta.action.as_ref().and_then(|a| a.properties.as_ref()).and_then(|p| p.get("max_num_segments")).and_then(Value::as_i64) else {
        return Outcome::new(Status::Failed, Some(info_of("Unable to retrieve [max_num_segments] from ActionProperties=null")));
    };
    let v = match call(env.cluster, "GET", &format!("/{}/_stats/segments?level=shards", path(i)), None).await {
        Ok(v) => v,
        Err(e) => return Outcome::failed(format!("Failed to check segments when waiting for force merge to complete [index={i}]"), e.reason),
    };
    let mut merging = 0;
    if let Some(Value::Obj(shards)) = v.get("indices").and_then(|x| x.get(i)).and_then(|x| x.get("shards")) {
        for copies in shards.values() {
            for c in copies.as_array().into_iter().flatten() {
                if c.get("segments").and_then(|s| s.get("count")).and_then(Value::as_i64).is_some_and(|n| n > max) {
                    merging += 1;
                }
            }
        }
    }
    let mut o = if merging == 0 {
        Outcome::done(format!("Successfully confirmed segments force merged [index={i}]"))
    } else {
        let started = env.meta.action.as_ref().and_then(|a| a.start_time).unwrap_or(env.now);
        let timeout_s = env.config.and_then(|c| c.get("timeout")).and_then(Value::as_str).and_then(time_ms).map_or(43_200, |ms| ms / 1000);
        if (env.now - started) / 1000 > timeout_s {
            Outcome::new(Status::Failed, Some(info_of(format!("Force merge timed out [index={i}]"))))
        } else {
            Outcome::waiting(format!("Waiting for force merge to complete [index={i}]"))
        }
    };
    if o.status == Status::Completed {
        o.properties = Some(None);
    }
    o
}

/// `AttemptSnapshotStep`: starts a snapshot of the index (no global state) named after the
/// rendered `snapshot` template plus the UTC time.
async fn snapshot(env: &Env<'_>) -> Outcome {
    let i = env.index;
    let conf = env.conf("snapshot").cloned().unwrap_or(Value::Null);
    let repository = conf.get("repository").and_then(Value::as_str).unwrap_or_default().to_string();
    let template = conf.get("snapshot").and_then(Value::as_str).unwrap_or_default().to_string();
    let st = env.cluster.state();
    let deny = setting_list(&st, "snapshot.deny_list").unwrap_or_default();
    drop(st);
    if deny.iter().any(|p| crate::xcontent::filter::glob_match(p, &repository)) {
        let mut o = Outcome::new(Status::Failed, Some(info_of(format!("Snapshot repository [{repository}] is blocked in [{}] [index={i}]", deny.join(", ")))));
        o.properties = Some(Some(Map::new()));
        return o;
    }
    let rendered = search_painless::mustache::Template::compile(&template, &[])
        .ok()
        .and_then(|t| t.render_json(&crate::scripting::to_serde(&obj! { "ctx" => obj! { "index" => i } })).ok())
        .unwrap_or_default();
    let base = if rendered.trim().is_empty() { if template.trim().is_empty() { template.clone() } else { i.to_string() } } else { rendered };
    let suffix = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(env.now).map(|d| d.format("%Y.%m.%d-%H:%M:%S%.3f").to_string()).unwrap_or_default();
    let name = format!("{base}-{suffix}");
    let body = obj! {
        "indices" => i,
        "include_global_state" => false,
        "metadata" => obj! { "snapshot_created" => "Open Distro for Elasticsearch Index Management" },
    };
    let uri = format!("/_snapshot/{}/{}?wait_for_completion=false", path(&repository), path(&name));
    let mut o = match call(env.cluster, "PUT", &uri, Some(body)).await {
        Ok(_) => Outcome::done(format!("Successfully started snapshot [index={i}]")),
        Err(e) if e.kind == "concurrent_snapshot_execution_exception" => Outcome::waiting(format!("Concurrent snapshot in progress, retrying next execution [index={i}]")),
        Err(e) => Outcome::failed(format!("Failed to create snapshot [index={i}]"), e.reason),
    };
    let mut p = Map::new();
    p.insert("snapshot_name".into(), Value::Str(name));
    o.properties = Some(Some(p));
    o
}

/// `WaitForSnapshotStep`.
async fn wait_for_snapshot(env: &Env<'_>) -> Outcome {
    let i = env.index;
    let repository = env.conf("snapshot").and_then(|c| c.get("repository")).and_then(Value::as_str).unwrap_or_default().to_string();
    let Some(name) = env.meta.action.as_ref().and_then(|a| a.properties.as_ref()).and_then(|p| p.get("snapshot_name")).and_then(Value::as_str).map(str::to_string) else {
        return Outcome::new(Status::Failed, Some(info_of(format!("Unable to retrieve [snapshot_name] from ActionProperties=null [index={i}]"))));
    };
    let v = match call(env.cluster, "GET", &format!("/_snapshot/{}/{}", path(&repository), path(&name)), None).await {
        Ok(v) => v,
        Err(e) if e.kind == "snapshot_missing_exception" => return Outcome::new(Status::Failed, Some(info_of(format!("Snapshot doesn't exist [index={i}]")))),
        Err(e) => return Outcome::failed(format!("Failed to get status of snapshot [index={i}]"), e.reason),
    };
    let state = v.get("snapshots").and_then(Value::as_array).and_then(|s| s.iter().find(|x| x.get("snapshot").and_then(Value::as_str) == Some(name.as_str()))).and_then(|s| s.get("state")).and_then(Value::as_str).map(str::to_string);
    let with_state = |m: String, s: &str| {
        let mut info = info_of(m);
        info.insert("state".into(), Value::Str(s.to_string()));
        info
    };
    match state.as_deref() {
        None => Outcome::new(Status::Failed, Some(info_of(format!("Snapshot doesn't exist [index={i}]")))),
        Some("IN_PROGRESS") => Outcome::new(Status::ConditionNotMet, Some(with_state(format!("Snapshot currently in progress [index={i}]"), "IN_PROGRESS"))),
        Some("SUCCESS") => Outcome::new(Status::Completed, Some(with_state(format!("Successfully created snapshot [index={i}]"), "SUCCESS"))),
        Some(other) => Outcome::new(Status::Failed, Some(with_state(format!("Snapshot doesn't exist [index={i}]"), other))),
    }
}

/// `getCompletedManagedIndexMetaData`: the step's outcome folded into the starting metadata,
/// with the action's retry counting on failure.
fn completed(starting: &Meta, step: &str, ty: &str, retry: &Retry, out: Outcome, now: i64) -> Meta {
    let mut m = starting.clone();
    m.step = Some(StepMeta { name: step.to_string(), start_time: step_start_time(starting, step, now), status: out.status });
    m.info = out.info;
    m.transition_to = out.transition_to;
    if let Some(pc) = out.policy_completed {
        m.policy_completed = Some(pc);
    }
    if step == "attempt_rollover" {
        m.rolled_over = Some(starting.rolled_over == Some(true) || out.status == Status::Completed);
        if m.rolled_over_index_name.is_none() {
            m.rolled_over_index_name = out.new_index;
        }
    }
    if let (Some(p), Some(a)) = (out.properties, m.action.as_mut()) {
        a.properties = p;
    }
    if out.status == Status::Failed
        && let Some(a) = m.action.as_mut()
    {
        let _ = ty;
        if a.consumed_retries >= retry.count {
            a.failed = true;
        } else {
            a.failed = false;
            a.consumed_retries += 1;
            a.last_retry_time = Some(now);
        }
    }
    m
}

/// `publishErrorNotification`: a failed run tells the policy's `error_notification`, and
/// records how that went in `info.error_notification`.
async fn publish_error_notification(_cluster: &Cluster, policy: Option<&Map>, mut meta: Meta) -> Meta {
    let Some(n) = policy.and_then(|p| p.get("error_notification")).filter(|n| !matches!(n, Value::Null)) else { return meta };
    let _ = n;
    let mut info = meta.info.take().unwrap_or_default();
    info.insert("error_notification".into(), Value::Str(NO_CONNECTIVITY.into()));
    meta.info = Some(info);
    meta
}

// ------------------------------------------------------------------------------ history

const HISTORY_ALIAS: &str = ".opendistro-ism-managed-index-history-write";

/// The history index mapping (`opendistro-ism-history.json` of the index-management plugin,
/// Apache-2.0).
const HISTORY_MAPPING: &str = include_str!("ism_history_mapping.json");

/// `IndexStateManagementHistory.addManagedIndexMetaDataHistory`: one document per metadata
/// write that isn't a step starting or still waiting, in a hidden daily index behind the
/// write alias (created on first use).
async fn write_history(cluster: &Cluster, metas: &[Meta], now: i64) {
    let docs: Vec<&Meta> = metas.iter().filter(|m| !matches!(m.step.as_ref().map(|s| s.status), Some(Status::Starting | Status::ConditionNotMet))).collect();
    if docs.is_empty() {
        return;
    }
    if !cluster.state().has_alias(HISTORY_ALIAS) {
        let day = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now).map(|d| d.format("%Y.%m.%d").to_string()).unwrap_or_default();
        let name = format!(".opendistro-ism-managed-index-history-{day}-1");
        let st = cluster.state();
        let shards = setting(&st, "history.number_of_shards").unwrap_or("1").to_string();
        let replicas = setting(&st, "history.number_of_replicas").unwrap_or("1").to_string();
        drop(st);
        let mapping = crate::json::parse(HISTORY_MAPPING.as_bytes()).ok().flatten().unwrap_or(Value::Null);
        let body = obj! {
            "settings" => obj! { "index.hidden" => true, "index.number_of_shards" => shards, "index.number_of_replicas" => replicas },
            "mappings" => mapping,
            "aliases" => obj! { HISTORY_ALIAS => obj! {} },
        };
        let _ = call(cluster, "PUT", &format!("/{}", path(&name)), Some(body)).await;
    }
    let mut bulk = String::new();
    for m in docs {
        let mut doc = match m.explain_value() {
            Value::Obj(d) => d,
            _ => Map::new(),
        };
        doc.insert("history_timestamp".into(), Value::from(now));
        bulk.push_str("{\"index\":{}}\n");
        bulk.push_str(&obj! { "managed_index_meta_data" => Value::Obj(doc) }.to_json());
        bulk.push('\n');
    }
    let mut req = EsRequest::new("POST", &format!("/{HISTORY_ALIAS}/_bulk"));
    req.headers.push(("content-type".into(), "application/x-ndjson".into()));
    req.body = bulk.into();
    let _ = crate::rest::handle(cluster, req).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(json: &str) -> Map {
        crate::json::parse(json.as_bytes()).unwrap().unwrap().as_object().cloned().unwrap()
    }

    #[test]
    fn actions_advance_through_steps_and_states() {
        let p = policy(r#"{"default_state":"hot","states":[{"name":"hot","actions":[{"read_only":{}},{"force_merge":{"max_num_segments":1}}],"transitions":[{"state_name":"warm"}]},{"name":"warm","actions":[],"transitions":[]}]}"#);
        let mut m = Meta { state: Some(StateMeta { name: "hot".into(), start_time: 1 }), ..Meta::default() };
        let hot = state_to_execute(&p, &m).unwrap();
        let a = action_to_execute(hot, &m).unwrap();
        assert_eq!((a.ty, a.index), ("read_only", 0));
        assert_eq!(step_to_execute(a.ty, &m), "set_read_only");
        m.action = Some(ActionMeta::new("read_only", 0, 1));
        m.step = Some(StepMeta { name: "set_read_only".into(), start_time: 1, status: Status::Completed });
        let a = action_to_execute(hot, &m).unwrap();
        assert_eq!((a.ty, a.index), ("force_merge", 1));
        assert_eq!(step_to_execute(a.ty, &m), "attempt_set_read_only");
        m.action = Some(ActionMeta::new("force_merge", 1, 1));
        m.step = Some(StepMeta { name: "attempt_set_read_only".into(), start_time: 1, status: Status::Completed });
        assert_eq!(step_to_execute("force_merge", &m), "attempt_call_force_merge");
        m.step = Some(StepMeta { name: "wait_for_force_merge".into(), start_time: 1, status: Status::ConditionNotMet });
        assert_eq!(step_to_execute("force_merge", &m), "wait_for_force_merge");
        m.step = Some(StepMeta { name: "wait_for_force_merge".into(), start_time: 1, status: Status::Completed });
        let a = action_to_execute(hot, &m).unwrap();
        assert_eq!((a.ty, a.index), (TRANSITION, -1));
        // A transition picks the target state's first action, or its transitions.
        m.transition_to = Some("warm".into());
        let warm = state_to_execute(&p, &m).unwrap();
        assert_eq!(action_to_execute(warm, &m).unwrap().ty, TRANSITION);
    }

    #[test]
    fn retries_back_off_like_the_plugin() {
        let r = Retry { count: 3, backoff: "exponential".into(), delay_ms: 60_000 };
        let mut am = ActionMeta::new("rollover", 0, 0);
        assert!(!should_backoff(&r, Some(&am), 10), "no retry consumed yet");
        am.consumed_retries = 2;
        am.last_retry_time = Some(1_000);
        assert!(should_backoff(&r, Some(&am), 1_000 + 119_999));
        assert!(!should_backoff(&r, Some(&am), 1_000 + 120_000));
        let lin = Retry { count: 3, backoff: "linear".into(), delay_ms: 10 };
        assert!(should_backoff(&lin, Some(&am), 1_019));
        assert!(!should_backoff(&lin, Some(&am), 1_020));
    }

    #[test]
    fn failures_consume_retries_then_fail_the_action() {
        let starting = Meta { action: Some(ActionMeta::new("delete", 0, 5)), step: Some(StepMeta { name: "attempt_delete".into(), start_time: 5, status: Status::Starting }), ..Meta::default() };
        let r = Retry { count: 1, backoff: "constant".into(), delay_ms: 1 };
        let m = completed(&starting, "attempt_delete", "delete", &r, Outcome::failed("x".into(), Some("y".into())), 9);
        let a = m.action.as_ref().unwrap();
        assert_eq!((a.failed, a.consumed_retries, a.last_retry_time), (false, 1, Some(9)));
        assert!(!m.is_failed());
        let m = completed(&m, "attempt_delete", "delete", &r, Outcome::failed("x".into(), None), 12);
        assert!(m.is_failed());
        assert_eq!(m.step.unwrap().status, Status::Failed);
    }

    #[test]
    fn explain_form_hides_state_while_transitioning_and_after_completion() {
        let mut m = Meta { index: "i".into(), index_uuid: "u".into(), policy_id: "p".into(), policy_seq_no: Some(0), policy_primary_term: Some(1), ..Meta::default() };
        m.state = Some(StateMeta { name: "hot".into(), start_time: 1 });
        m.transition_to = Some("warm".into());
        m.retry_info = Some(RetryInfo { failed: false, consumed_retries: 0 });
        m.info = Some(info_of("Transitioning to warm [index=i]"));
        assert_eq!(
            m.explain_value().to_json(),
            r#"{"index":"i","index_uuid":"u","policy_id":"p","policy_seq_no":0,"policy_primary_term":1,"transition_to":"warm","retry_info":{"failed":false,"consumed_retries":0},"info":{"message":"Transitioning to warm [index=i]"}}"#
        );
        m.policy_completed = Some(true);
        assert_eq!(m.explain_value().to_json(), r#"{"index":"i","index_uuid":"u","policy_id":"p","policy_seq_no":0,"policy_primary_term":1,"policy_completed":true}"#);
        let back = Meta::from_value(&m.to_stored()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn explain_filters() {
        let m = Meta { state: Some(StateMeta { name: "hot".into(), start_time: 1 }), action: Some(ActionMeta::new(TRANSITION, -1, 1)), retry_info: Some(RetryInfo { failed: false, consumed_retries: 0 }), ..Meta::default() };
        assert!(m.matches_filter(Some("hot"), None, None));
        assert!(!m.matches_filter(Some("warm"), None, None));
        assert!(m.matches_filter(None, Some("transition"), Some(false)));
        assert!(!m.matches_filter(None, None, Some(true)));
    }

    #[test]
    fn safe_policy_changes_keep_the_action_layout() {
        let a = policy(r#"{"states":[{"name":"s","actions":[{"read_only":{}},{"delete":{}}]}]}"#);
        let b = policy(r#"{"states":[{"name":"s","actions":[{"retry":{"count":1},"read_only":{}},{"delete":{}}]}]}"#);
        let c = policy(r#"{"states":[{"name":"s","actions":[{"read_write":{}},{"delete":{}}]}]}"#);
        assert!(safe_to_change(&a, &b, Some("s"), false));
        assert!(!safe_to_change(&a, &c, Some("s"), false));
        assert!(!safe_to_change(&a, &b, Some("s"), true));
        assert!(safe_to_change(&a, &c, None, false));
    }
}

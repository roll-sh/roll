//! Index lifecycle management at work: Elasticsearch's `IndexLifecycleService` and
//! `IndexLifecycleRunner`, run by the cluster's job scheduler ([`crate::opensearch::jobs`]) on
//! Elasticsearch-flavor clusters.
//!
//! Each managed index (`index.lifecycle.name`) carries its `LifecycleExecutionState` in the
//! `ilm.step` family ([`Exec`]): the phase, action and step it is on and since when, its
//! lifecycle date, the cached definition of its phase (`phase_execution`), and after a failure
//! the failed step, its error (`step_info`) and retry count. A new managed index is
//! initialized in the cluster state update that made it managed ([`reconcile`]:
//! `InitializePolicyContextStep`, then `new`/`complete`/`complete`), as Elasticsearch does
//! right after the change is applied; a missing policy is recorded the same way.
//!
//! A run moves every due index through its policy's steps, in Elasticsearch's order and with
//! its step names ([`phase_steps`]: each phase's actions in `TimeseriesLifecycleType`'s order,
//! `unfollow` and `migrate` injected where Elasticsearch injects them), until it reaches a step
//! that waits: a phase whose `min_age` hasn't passed (counted from the lifecycle date: the
//! creation date, the rollover date after a rollover, or `index.lifecycle.origination_date`),
//! a rollover whose conditions aren't met, a data stream's write index, a time series index
//! still accepting writes, or an error. Steps act through roll's own REST handlers
//! (`_rollover`, `_settings`, `_block/write`, `_forcemerge`, `DELETE`, ...) as the user who
//! last put the policy (Elasticsearch's stored security headers). What has no counterpart in
//! roll completes at once as it does on a single node without data tiers (`allocate`'s and
//! `migrate`'s routing checks, `wait-for-active-shards`, the follower steps of `unfollow` on
//! an index that isn't a follower); `shrink` completes when the index already has the target
//! shard count (`branch-check-prerequisites`). Steps roll can't run end in the `ERROR` step
//! with the reason: `shrink` to fewer shards, `downsample` and `searchable_snapshot`.
//! `wait_for_snapshot` waits for its snapshot lifecycle policy ([`super::slm`]) to have taken a
//! snapshot including the index since the action started (`WaitForSnapshotStep`).
//!
//! **When runs happen.** Elasticsearch polls every `indices.lifecycle.poll_interval` (10m).
//! roll knows when an index's next time-based step is due ([`next_due`]: a phase's `min_age`,
//! a rollover's `max_age`/`min_age`, a time series index's end time) and runs it then, also
//! waking a sleeping cluster for it; it polls only while a write could change an outcome (a
//! size or document-count rollover condition) or a retryable error is retried ([`polls`]),
//! and never wakes a cluster for that.

use super::ilm;
use crate::cluster::{Cluster, Dialect};
use crate::error::EsError;
use crate::json::{Map, Value};
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use crate::units::{ByteSize, TimeValue};
use crate::xpack::{enc, internal, store};

/// The per-index execution state family (by index name).
pub(crate) const STATE: &str = "ilm.step";

const PHASES: [&str; 5] = ["hot", "warm", "cold", "frozen", "delete"];
const ERROR: &str = "ERROR";
/// The step a failed index is on.
pub(crate) const ERROR_STEP: &str = ERROR;
const COMPLETE: &str = "complete";

/// `LifecycleExecutionState`, kept per index.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Exec {
    pub uuid: String,
    pub phase: Option<String>,
    pub action: Option<String>,
    pub step: Option<String>,
    pub phase_time: Option<i64>,
    pub action_time: Option<i64>,
    pub step_time: Option<i64>,
    pub lifecycle_date: Option<i64>,
    /// `PhaseExecutionInfo`: `{policy, phase_definition?, version, modified_date_in_millis}`.
    pub phase_definition: Option<Value>,
    pub failed_step: Option<String>,
    pub auto_retryable: Option<bool>,
    pub retry_count: Option<i64>,
    pub step_info: Option<Value>,
    pub previous_step_info: Option<Value>,
    /// When a waiting rollover's time-based condition was found due without the rollover
    /// happening (only a write can complete it now).
    pub checked_at: Option<i64>,
}

fn s(v: &Map, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn n(v: &Map, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

impl Exec {
    pub(crate) fn from_value(v: &Value) -> Option<Exec> {
        let m = v.as_object()?;
        Some(Exec {
            uuid: s(m, "uuid")?,
            phase: s(m, "phase"),
            action: s(m, "action"),
            step: s(m, "step"),
            phase_time: n(m, "phase_time"),
            action_time: n(m, "action_time"),
            step_time: n(m, "step_time"),
            lifecycle_date: n(m, "lifecycle_date"),
            phase_definition: m.get("phase_definition").filter(|v| !v.is_null()).cloned(),
            failed_step: s(m, "failed_step"),
            auto_retryable: m.get("is_auto_retryable_error").and_then(Value::as_bool),
            retry_count: n(m, "failed_step_retry_count"),
            step_info: m.get("step_info").filter(|v| !v.is_null()).cloned(),
            previous_step_info: m.get("previous_step_info").filter(|v| !v.is_null()).cloned(),
            checked_at: n(m, "checked_at"),
        })
    }

    pub(crate) fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("uuid".into(), Value::from(self.uuid.clone()));
        let mut put = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                m.insert(k.into(), v);
            }
        };
        put("phase", self.phase.clone().map(Value::from));
        put("action", self.action.clone().map(Value::from));
        put("step", self.step.clone().map(Value::from));
        put("phase_time", self.phase_time.map(Value::from));
        put("action_time", self.action_time.map(Value::from));
        put("step_time", self.step_time.map(Value::from));
        put("lifecycle_date", self.lifecycle_date.map(Value::from));
        put("phase_definition", self.phase_definition.clone());
        put("failed_step", self.failed_step.clone().map(Value::from));
        put("is_auto_retryable_error", self.auto_retryable.map(Value::from));
        put("failed_step_retry_count", self.retry_count.map(Value::from));
        put("step_info", self.step_info.clone());
        put("previous_step_info", self.previous_step_info.clone());
        put("checked_at", self.checked_at.map(Value::from));
        Value::Obj(m)
    }

    pub(crate) fn key(&self) -> Option<(&str, &str, &str)> {
        Some((self.phase.as_deref()?, self.action.as_deref()?, self.step.as_deref()?))
    }

    /// `IndexLifecycleTransition.updateExecutionStateToStep`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn move_to(&mut self, phase: &str, action: &str, step: &str, policy: Option<(&str, &Value)>, refresh_phase: bool, keep_null_previous: bool, now: i64) {
        let same_phase = self.phase.as_deref() == Some(phase);
        let same_action = self.key().is_some() && self.action.as_deref() == Some(action);
        let was_new = self.phase.as_deref() == Some("new");
        if keep_null_previous || self.step_info.is_some() {
            self.previous_step_info = self.step_info.take();
        }
        self.step_info = None;
        self.failed_step = None;
        self.auto_retryable = None;
        self.retry_count = None;
        self.checked_at = None;
        if self.key().is_none() || !same_phase || refresh_phase {
            if let Some((name, p)) = policy {
                self.phase_definition = Some(phase_execution(name, p, phase));
            }
            self.phase_time = Some(now);
        } else if was_new {
            self.phase_time = Some(now);
        }
        if !same_action {
            self.action_time = Some(now);
        }
        self.phase = Some(phase.into());
        self.action = Some(action.into());
        self.step = Some(step.into());
        self.step_time = Some(now);
    }

    /// `moveIndexToErrorStep`.
    fn fail(&mut self, error: Value, now: i64) {
        let failed = self.step.clone();
        let (phase, action) = (self.phase.clone().unwrap_or_default(), self.action.clone().unwrap_or_default());
        let retries = self.retry_count;
        self.move_to(&phase, &action, ERROR, None, false, false, now);
        self.failed_step = failed;
        self.step_info = Some(error);
        self.auto_retryable = Some(true);
        self.retry_count = retries;
    }
}

/// `PhaseExecutionInfo` for `phase` of a policy record (no definition for `new`).
fn phase_execution(name: &str, rec: &Value, phase: &str) -> Value {
    let mut m = Map::new();
    m.insert("policy".into(), Value::from(name));
    if let Some(def) = rec.pointer(&["policy", "phases", phase]) {
        m.insert("phase_definition".into(), def.clone());
    }
    m.insert("version".into(), rec.get("version").cloned().unwrap_or(Value::from(1)));
    m.insert("modified_date_in_millis".into(), rec.get("modified_date").cloned().unwrap_or(Value::from(0)));
    Value::Obj(m)
}

/// The managed index's policy name (`index.lifecycle.name`).
pub(crate) fn policy_name(idx: &IndexMeta) -> Option<&str> {
    idx.settings.get("index.lifecycle.name").filter(|p| !p.is_empty())
}

/// The index's execution state (none for another index of the same name).
pub(crate) fn exec_of(st: &ClusterState, idx: &IndexMeta) -> Option<Exec> {
    Exec::from_value(st.metadata.xpack.get(STATE, &idx.name)?).filter(|e| e.uuid == idx.uuid)
}

/// The origination date when set, else the lifecycle date (`calculateOriginationMillis`).
pub(crate) fn lifecycle_date(idx: &IndexMeta, e: &Exec) -> Option<i64> {
    idx.settings.get("index.lifecycle.origination_date").and_then(|v| v.parse::<i64>().ok()).filter(|d| *d != -1).or(e.lifecycle_date)
}

fn time_ms(v: &Value) -> i64 {
    TimeValue::parse(&v.to_es_string(), "min_age").map(|t| t.as_millis()).unwrap_or(0)
}

/// `indices.lifecycle.poll_interval` in ms.
pub fn poll_interval(st: &ClusterState) -> i64 {
    st.metadata.cluster_setting("indices.lifecycle.poll_interval").and_then(|v| TimeValue::parse(v, "").ok()).map(|t| t.as_millis()).filter(|t| *t > 0).unwrap_or(600_000)
}

// ------------------------------------------------------------------------------ the steps

/// The steps of one action, in order (`LifecycleAction.toSteps`).
fn action_steps(action: &str, conf: &Value) -> Vec<&'static str> {
    match action {
        "set_priority" => vec!["set_priority"],
        "unfollow" => vec![
            "branch-check-unfollow-prerequisites",
            "wait-for-indexing-complete",
            "check-ts-end-time-passed",
            "wait-for-follow-shard-tasks",
            "pause-follower-index",
            "close-follower-index",
            "unfollow-follower-index",
            "open-follower-index",
            "wait-for-index-color",
        ],
        "rollover" => vec!["check-rollover-ready", "attempt-rollover", "wait-for-active-shards", "update-rollover-lifecycle-date", "set-indexing-complete"],
        "readonly" => vec!["check-not-write-index", "check-ts-end-time-passed", "readonly", "set-indexing-complete"],
        "downsample" => vec!["branch-on-timeseries-check", "check-not-write-index", "wait-for-shard-history-leases", "check-ts-end-time-passed", "readonly", "rollup"],
        "shrink" => vec!["branch-check-prerequisites", "check-not-write-index", "wait-for-shard-history-leases", "check-ts-end-time-passed", "readonly", "check-target-shards-count", "shrink"],
        "forcemerge" => {
            let mut v = vec!["branch-forcemerge-check-prerequisites", "check-not-write-index", "check-ts-end-time-passed", "readonly"];
            if conf.get("index_codec").and_then(Value::as_str) == Some("best_compression") {
                v.extend(["close-index", "update-settings", "open-index", "wait-for-index-color"]);
            }
            v.extend(["forcemerge", "segment-count"]);
            v
        }
        "allocate" => vec!["allocate", "check-allocation"],
        "migrate" if conf.get("enabled").and_then(Value::as_bool) != Some(false) => vec!["branch-check-skip-action", "migrate", "check-migration"],
        "migrate" => vec![],
        "searchable_snapshot" => vec!["searchable_snapshot"],
        "freeze" => vec!["branch-freeze-check-prerequisites", "check-not-write-index", "freeze"],
        "wait_for_snapshot" => vec!["wait-for-snapshot"],
        "delete" => {
            let mut v = vec!["wait-for-shard-history-leases", "check-ts-end-time-passed"];
            if conf.get("delete_searchable_snapshot").and_then(Value::as_bool) != Some(false) {
                v.push("cleanup-snapshot");
            }
            v.push("delete");
            v
        }
        _ => vec![],
    }
}

fn ordered_actions(phase: &str) -> &'static [&'static str] {
    match phase {
        "hot" => &["set_priority", "unfollow", "rollover", "readonly", "downsample", "shrink", "forcemerge", "searchable_snapshot"],
        "warm" => &["set_priority", "unfollow", "readonly", "downsample", "allocate", "migrate", "shrink", "forcemerge"],
        "cold" => &["set_priority", "unfollow", "readonly", "downsample", "searchable_snapshot", "allocate", "migrate", "freeze"],
        "frozen" => &["unfollow", "searchable_snapshot"],
        "delete" => &["wait_for_snapshot", "delete"],
        _ => &[],
    }
}

/// A phase's steps as `(action, step)`, then `complete`
/// (`TimeseriesLifecycleType.getOrderedPhases` and `getOrderedActions`: `unfollow` added
/// before a rollover, shrink, searchable snapshot or downsample; `migrate` added to `warm`
/// and `cold` unless configured or a searchable snapshot sets the allocation).
pub(crate) fn phase_steps(phase: &str, def: Option<&Value>) -> Vec<(String, &'static str)> {
    let mut actions = def.and_then(|d| d.get("actions")).and_then(Value::as_object).cloned().unwrap_or_default();
    if phase == "new" {
        return vec![("init".into(), "init"), (COMPLETE.into(), COMPLETE)];
    }
    if !actions.contains_key("unfollow") && ["rollover", "shrink", "searchable_snapshot", "downsample"].iter().any(|a| actions.contains_key(*a)) {
        actions.insert("unfollow".into(), Value::object());
    }
    if matches!(phase, "warm" | "cold") && !actions.contains_key("searchable_snapshot") && !actions.contains_key("migrate") {
        actions.insert("migrate".into(), obj! { "enabled" => true });
    }
    let mut out = Vec::new();
    for a in ordered_actions(phase) {
        if let Some(conf) = actions.get(*a) {
            for st in action_steps(a, conf) {
                out.push((a.to_string(), st));
            }
        }
    }
    out.push((COMPLETE.into(), COMPLETE));
    out
}

/// The phases of a policy record in order.
fn policy_phases(rec: &Value) -> Vec<&'static str> {
    PHASES.iter().copied().filter(|p| rec.pointer(&["policy", "phases", p]).is_some()).collect()
}

/// The phase after `phase` in the policy (`new` is before every phase).
fn next_phase(rec: &Value, phase: &str) -> Option<&'static str> {
    let phases = policy_phases(rec);
    let rank = |p: &str| PHASES.iter().position(|q| *q == p);
    match rank(phase) {
        None => phases.first().copied(),
        Some(i) => phases.into_iter().find(|p| rank(p).is_some_and(|j| j > i)),
    }
}

/// The definition of the index's current phase: its cached one, as Elasticsearch keeps
/// running a phase as it was when the index entered it.
fn current_def(e: &Exec) -> Option<&Value> {
    e.phase_definition.as_ref().and_then(|p| p.get("phase_definition"))
}

/// The first step of `phase` in a policy record (`getFirstStepForPhase`), or of one of its
/// actions.
pub(crate) fn first_step_of(phase: &str, action: Option<&str>, rec: &Value) -> Option<(String, &'static str)> {
    phase_steps(phase, rec.pointer(&["policy", "phases", phase])).into_iter().find(|(a, _)| action.is_none_or(|x| x == a))
}

/// Whether a step exists in the phase as the policy (or the index's cached phase) has it.
pub(crate) fn step_exists(phase: &str, action: &str, step: &str, rec: Option<&Value>, cached: Option<&Exec>) -> bool {
    let has = |def: Option<&Value>| phase_steps(phase, def).iter().any(|(a, s)| a == action && *s == step);
    rec.is_some_and(|r| r.pointer(&["policy", "phases", phase]).is_some() && has(r.pointer(&["policy", "phases", phase])))
        || cached.is_some_and(|e| e.phase.as_deref() == Some(phase) && has(current_def(e)))
        || (phase == "new" && has(None))
}

/// A step of a phase: its action and name.
type StepRef = (String, &'static str);

/// The step after the current one, and the first step of the next action (`None`: the
/// current step isn't in the phase).
fn following(e: &Exec) -> Option<(StepRef, StepRef)> {
    let (phase, action, step) = e.key()?;
    let steps = phase_steps(phase, current_def(e));
    let i = steps.iter().position(|(a, s)| a == action && *s == step)?;
    let next = steps.get(i + 1).cloned()?;
    let skip = steps[i..].iter().find(|(a, _)| a != action).cloned()?;
    Some((next, skip))
}

fn step_conf(e: &Exec) -> Value {
    let Some((_, action, _)) = e.key() else { return Value::Null };
    current_def(e).and_then(|d| d.pointer(&["actions", action])).cloned().unwrap_or_else(Value::object)
}

// ------------------------------------------------------------------------------ the schedule

fn active(st: &ClusterState) -> bool {
    st.dialect == Dialect::Elasticsearch
}

/// The managed indices with their policy names.
fn managed(st: &ClusterState) -> impl Iterator<Item = (&IndexMeta, &str)> {
    st.metadata.indices.values().filter_map(|i| policy_name(i).map(|p| (&**i, p)))
}

/// Every managed index's due time and poll flag ([`index_due`]), each policy looked up once.
fn dues(st: &ClusterState, now: i64) -> impl Iterator<Item = (&IndexMeta, &str, (Option<i64>, bool))> {
    let mut policies: std::collections::HashMap<&str, Option<Value>> = std::collections::HashMap::new();
    managed(st).map(move |(i, p)| {
        let rec = policies.entry(p).or_insert_with(|| ilm::policy(st, p));
        let due = index_due(st, i, rec.as_ref(), now);
        (i, p, due)
    })
}

/// When the rollover's time-based conditions are all met (`max_age`, and `min_age` if set);
/// `None` without a `max_age`/`min_age`.
fn rollover_time(idx: &IndexMeta, conf: &Value) -> Option<i64> {
    let t = |k: &str| conf.get(k).map(time_ms);
    let (max, min) = (t("max_age"), t("min_age"));
    if max.is_none() && min.is_none() {
        return None;
    }
    Some(idx.creation_date().saturating_add(max.unwrap_or(0).max(min.unwrap_or(0))))
}

/// When the index's next step can run: `Some(t)` at a known time, `None` when only a write or a
/// retry can move it; and whether it waits on something only a poll sees.
fn index_due(st: &ClusterState, idx: &IndexMeta, rec: Option<&Value>, now: i64) -> (Option<i64>, bool) {
    if idx.settings.get("index.lifecycle.skip") == Some("true") {
        return (None, false);
    }
    let Some(e) = exec_of(st, idx).filter(|e| e.step.is_some()) else {
        // Not initialized (a policy that appeared since): now.
        return (rec.is_some().then_some(now), false);
    };
    let Some(rec) = rec else { return (None, false) };
    let Some((phase, action, step)) = e.key() else { return (None, false) };
    match step {
        ERROR => (None, e.auto_retryable == Some(true)),
        COMPLETE if action == COMPLETE => match next_phase(rec, phase) {
            Some(next) => {
                let min_age = rec.pointer(&["policy", "phases", next, "min_age"]).map(time_ms).unwrap_or(0);
                (Some(lifecycle_date(idx, &e).unwrap_or(now).saturating_add(min_age)), false)
            }
            None => (None, false),
        },
        "check-rollover-ready" => {
            if rollover_skips(st, idx) {
                return (Some(now), false);
            }
            let at = rollover_time(idx, &step_conf(&e)).filter(|t| e.checked_at.is_none_or(|c| c < *t));
            (at, true)
        }
        "check-ts-end-time-passed" => (Some(ts_end(idx).filter(|t| *t > now).unwrap_or(now)), false),
        "check-not-write-index" => (write_index_of(st, idx).is_none().then_some(now), false),
        // Moves on once the snapshot lifecycle policy has run (its run changes the state, which
        // wakes the scheduler): never polled, never a reason to wake a cluster.
        "wait-for-snapshot" => {
            let p = step_conf(&e).get("policy").map(Value::to_es_string).unwrap_or_default();
            let since = e.action_time.unwrap_or(0);
            let waiting = matches!(super::slm::snapshot_since(st, &p, &idx.name, since), super::slm::Since::NotYet);
            ((!waiting).then_some(now), false)
        }
        // A step the runner can't move on from (one its cached phase lacks, the terminal
        // `completed` step) never comes due.
        _ if following(&e).is_none() => (None, false),
        _ => (Some(now), false),
    }
}

/// When ILM next has time-based work on this cluster (an index to initialize or move on, a
/// phase's `min_age`, a rollover's age, a `STOPPING` to finish): what wakes a sleeping
/// cluster.
pub fn next_due(st: &ClusterState) -> Option<i64> {
    if !active(st) {
        return None;
    }
    let now = crate::time::now_millis();
    match ilm::mode(st).as_str() {
        "STOPPING" => Some(now),
        "STOPPED" => None,
        _ => dues(st, now).filter_map(|(_, _, d)| d.0).min(),
    }
}

/// Whether a loaded cluster polls at `indices.lifecycle.poll_interval`: an index waits on a
/// rollover condition a write could meet, or on a retry.
pub fn polls(st: &ClusterState) -> bool {
    if !active(st) || ilm::mode(st) != "RUNNING" {
        return false;
    }
    let now = crate::time::now_millis();
    dues(st, now).any(|(_, _, d)| d.1)
}

// ------------------------------------------------------------------------------ init

/// Initializes the indices that became managed in a state change, and drops the state of
/// indices that are gone or no longer managed (run by `Cluster::update`).
pub fn reconcile(prev: &ClusterState, next: &mut ClusterState, now: i64) {
    if !active(next) {
        return;
    }
    let family = |st: &ClusterState| st.metadata.xpack.family(ilm::POLICIES).map_or(std::ptr::null(), |f| f as *const store::Family);
    let policies_changed = !std::ptr::eq(family(prev), family(next));
    let changed = |n: &String, i: &std::sync::Arc<IndexMeta>| prev.metadata.indices.get(n).is_none_or(|p| !std::sync::Arc::ptr_eq(p, i));
    let mut init: Vec<String> = Vec::new();
    for (n, i) in &next.metadata.indices {
        if !(policies_changed || changed(n, i)) {
            continue;
        }
        let Some(policy) = policy_name(i) else { continue };
        let needs = match exec_of(next, i) {
            None => true,
            // A missing policy was recorded: initialize once it exists.
            Some(e) => e.step.is_none() && ilm::policy(next, policy).is_some(),
        };
        if needs {
            init.push(n.clone());
        }
    }
    let stale: Vec<String> = next
        .metadata
        .xpack
        .iter(STATE)
        .filter(|(n, r)| next.metadata.indices.get(*n).is_none_or(|i| policy_name(i).is_none() || r.get("uuid").and_then(Value::as_str) != Some(i.uuid.as_str())))
        .map(|(n, _)| n.clone())
        .collect();
    for n in stale {
        if !init.contains(&n) {
            store::remove(next, STATE, &n);
        }
    }
    for n in init {
        let Some(idx) = next.metadata.indices.get(&n).cloned() else { continue };
        let policy = policy_name(&idx).unwrap_or_default().to_string();
        let e = match ilm::policy(next, &policy) {
            Some(rec) => initialized(&idx, &policy, &rec, now),
            // `markPolicyDoesNotExist`: only the reason, no step.
            None => Exec { uuid: idx.uuid.clone(), step_info: Some(EsError::illegal_argument(format!("policy [{policy}] does not exist")).to_value(false)), ..Exec::default() },
        };
        store::family_mut(next, STATE).insert(n, e.to_value());
    }
}

/// `InitializePolicyContextStep` then `new`/`complete`/`complete`.
fn initialized(idx: &IndexMeta, policy: &str, rec: &Value, now: i64) -> Exec {
    let mut e = Exec { uuid: idx.uuid.clone(), lifecycle_date: Some(idx.creation_date()), ..Exec::default() };
    e.move_to("new", "init", "init", Some((policy, rec)), false, true, now);
    e.move_to("new", COMPLETE, COMPLETE, Some((policy, rec)), false, true, now);
    e
}

// ------------------------------------------------------------------------------ the runner

/// What one step did.
enum Outcome {
    /// Go on with the next step.
    Next,
    /// Skip the rest of the action.
    SkipAction,
    /// Stay: the condition isn't met (with its `step_info`).
    Wait(Option<Value>),
    /// Fail into the `ERROR` step.
    Fail(Value),
    /// The index is gone (deleted by its policy).
    Gone,
}

fn fail_with(reason: impl Into<String>) -> Outcome {
    Outcome::Fail(EsError::illegal_state(reason).to_value(false))
}

fn fail_arg(reason: impl Into<String>) -> Outcome {
    Outcome::Fail(EsError::illegal_argument(reason).to_value(false))
}

/// The data stream `idx` is the write index (or failure store write index) of.
fn write_index_of<'a>(st: &'a ClusterState, idx: &IndexMeta) -> Option<(&'a str, bool)> {
    let ds = st.metadata.data_stream_of(&idx.name)?;
    if ds.write_index() == Some(idx.name.as_str()) {
        Some((ds.name.as_str(), false))
    } else if ds.failure_write_index() == Some(idx.name.as_str()) {
        Some((ds.name.as_str(), true))
    } else {
        None
    }
}

fn ts_end(idx: &IndexMeta) -> Option<i64> {
    if idx.settings.get("index.mode") != Some("time_series") {
        return None;
    }
    idx.settings.get("index.time_series.end_time").and_then(|v| crate::meta::index_settings::parse_date_setting(v).ok())
}

/// The rollover check passes without asking (`WaitForRolloverReadyStep`): a data stream index
/// that isn't a write index, or an alias index already rolled over or marked complete.
fn rollover_skips(st: &ClusterState, idx: &IndexMeta) -> bool {
    if st.metadata.data_stream_of(&idx.name).is_some() {
        return write_index_of(st, idx).is_none();
    }
    let alias = idx.settings.get("index.lifecycle.rollover_alias").unwrap_or_default();
    !alias.is_empty() && (idx.rollover_info.contains_key(alias) || (idx.settings.get("index.lifecycle.indexing_complete") == Some("true") && idx.aliases.get(alias).and_then(|a| a.is_write_index) != Some(true)))
}

struct Env<'a> {
    cluster: &'a Cluster,
    index: String,
    policy: String,
    now: i64,
}

impl Env<'_> {
    fn idx(&self) -> Option<std::sync::Arc<IndexMeta>> {
        self.cluster.state().metadata.indices.get(&self.index).cloned()
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, Value> {
        internal::call(self.cluster, method, path, body.as_ref()).await.map_err(|e| e.to_value(false))
    }

    async fn act(&self, method: &str, path: &str, body: Option<Value>) -> Outcome {
        match self.call(method, path, body).await {
            Ok(_) => Outcome::Next,
            Err(e) => Outcome::Fail(e),
        }
    }

    async fn settings(&self, settings: Value) -> Outcome {
        self.act("PUT", &format!("/{}/_settings", enc(&self.index)), Some(settings)).await
    }
}

/// Runs the index's current step.
async fn execute(env: &Env<'_>, e: &mut Exec) -> Outcome {
    let Some(idx) = env.idx() else { return Outcome::Gone };
    let Some((phase, action, step)) = e.key().map(|(p, a, s)| (p.to_string(), a.to_string(), s.to_string())) else { return Outcome::Next };
    let conf = step_conf(e);
    let i = &env.index;
    let path = enc(i);
    match (action.as_str(), step.as_str()) {
        ("init", "init") => {
            e.lifecycle_date = Some(idx.creation_date());
            Outcome::Next
        }
        // Nothing to wait for or route on one node without tiers or followers.
        (_, "wait-for-active-shards" | "check-allocation" | "check-migration" | "wait-for-index-color" | "wait-for-shard-history-leases" | "cleanup-snapshot" | "check-target-shards-count") => Outcome::Next,
        (_, "branch-check-unfollow-prerequisites") => Outcome::SkipAction,
        (_, "branch-check-skip-action") => Outcome::Next,
        (_, "branch-forcemerge-check-prerequisites" | "branch-freeze-check-prerequisites") => {
            if idx.settings.get("index.store.snapshot.index_name").is_some() { Outcome::SkipAction } else { Outcome::Next }
        }
        ("forcemerge", "readonly") => Outcome::Next,
        (_, "check-not-write-index") => match write_index_of(&env.cluster.state(), &idx) {
            Some((ds, failure)) => Outcome::Wait(Some(obj! { "message" => format!(
                "index [{i}] is the{} write index for data stream [{ds}], pausing ILM execution of lifecycle [{}] until this index is no longer the write index for the data stream via manual or automated rollover",
                if failure { " failure store" } else { "" },
                env.policy
            ) })),
            None => Outcome::Next,
        },
        (_, "check-ts-end-time-passed") => match ts_end(&idx) {
            Some(end) if env.now < end => Outcome::Wait(Some(obj! { "message" => format!(
                "The [index.time_series.end_time] setting for index [{i}] is [{end}]. Waiting until the index's time series end time lapses before proceeding with action [{action}] as the index can still accept writes."
            ) })),
            _ => Outcome::Next,
        },
        ("set_priority", "set_priority") => env.settings(obj! { "index.priority" => conf.get("priority").cloned().unwrap_or(Value::Null) }).await,
        ("rollover", "check-rollover-ready") => check_rollover(env, &idx, &conf, e).await,
        ("rollover", "attempt-rollover") => attempt_rollover(env, &idx).await,
        ("rollover", "update-rollover-lifecycle-date") => {
            if idx.settings.get("index.lifecycle.indexing_complete") == Some("true") {
                e.lifecycle_date = Some(env.now);
                return Outcome::Next;
            }
            let ds = env.cluster.state().metadata.data_stream_of(i).map(|d| d.name.clone());
            let target = match ds {
                Some(ds) => ds,
                None => match idx.settings.get("index.lifecycle.rollover_alias").filter(|a| !a.is_empty()) {
                    Some(a) => a.to_string(),
                    None => return fail_with(format!("setting [index.lifecycle.rollover_alias] is not set on index [{i}]")),
                },
            };
            match idx.rollover_info.get(&target) {
                Some(r) => {
                    e.lifecycle_date = Some(r.time);
                    Outcome::Next
                }
                None => fail_with(format!("no rollover info found for [{i}] with rollover target [{target}], the index has not yet rolled over with that target")),
            }
        }
        (_, "set-indexing-complete") => env.settings(obj! { "index.lifecycle.indexing_complete" => true }).await,
        (_, "readonly") => env.act("PUT", &format!("/{path}/_block/write"), None).await,
        ("forcemerge", "close-index") => env.act("POST", &format!("/{path}/_close"), None).await,
        ("forcemerge", "update-settings") => env.settings(obj! { "index.codec" => "best_compression" }).await,
        ("forcemerge", "open-index") => env.act("POST", &format!("/{path}/_open"), None).await,
        ("forcemerge", "forcemerge") => {
            let n = conf.get("max_num_segments").and_then(Value::as_i64).unwrap_or(1);
            env.act("POST", &format!("/{path}/_forcemerge?max_num_segments={n}"), None).await
        }
        // `SegmentCountStep` moves on either way, reporting shards left unmerged.
        ("forcemerge", "segment-count") => Outcome::Next,
        ("allocate", "allocate") => {
            let mut s = Map::new();
            if let Some(r) = conf.get("number_of_replicas").filter(|v| !v.is_null()) {
                s.insert("index.number_of_replicas".into(), r.clone());
                s.insert("index.auto_expand_replicas".into(), Value::Null);
            }
            for (k, prefix) in [("include", "index.routing.allocation.include."), ("exclude", "index.routing.allocation.exclude."), ("require", "index.routing.allocation.require.")] {
                for (attr, v) in conf.get(k).and_then(Value::as_object).into_iter().flatten() {
                    s.insert(format!("{prefix}{attr}"), v.clone());
                }
            }
            if let Some(t) = conf.get("total_shards_per_node").filter(|v| !v.is_null()) {
                s.insert("index.routing.allocation.total_shards_per_node".into(), t.clone());
            }
            if s.is_empty() { Outcome::Next } else { env.settings(Value::Obj(s)).await }
        }
        ("migrate", "migrate") => {
            let tiers = match phase.as_str() {
                "warm" => "data_warm,data_hot",
                "cold" => "data_cold,data_warm,data_hot",
                _ => "data_hot",
            };
            env.settings(obj! { "index.routing.allocation.include._tier_preference" => tiers }).await
        }
        ("shrink", "branch-check-prerequisites") => {
            let shards = idx.number_of_shards() as i64;
            let target = match conf.get("number_of_shards").and_then(Value::as_i64) {
                Some(t) => t,
                None => {
                    let max = conf.get("max_primary_shard_size").and_then(|v| ByteSize::parse(&v.to_es_string(), "").ok()).map(|b| b.bytes()).unwrap_or(i64::MAX).max(1);
                    let size = match env.call("GET", &format!("/{path}/_stats/store"), None).await {
                        Ok(v) => v.pointer(&["_all", "primaries", "store", "size_in_bytes"]).and_then(Value::as_i64).unwrap_or(0),
                        Err(_) => 0,
                    };
                    (size.saturating_add(max - 1) / max).clamp(1, shards.max(1))
                }
            };
            if shards == target || idx.settings.get("index.store.snapshot.index_name").is_some() { Outcome::SkipAction } else { Outcome::Next }
        }
        ("shrink", "shrink") => fail_with(format!("roll search does not run the [shrink] action on an index with more shards than the target: index [{i}] has [{}] shards", idx.number_of_shards())),
        ("downsample", "branch-on-timeseries-check") => {
            if idx.settings.get("index.mode") == Some("time_series") { Outcome::Next } else { Outcome::SkipAction }
        }
        ("downsample", "rollup") => fail_with("roll search does not run the [downsample] action from a lifecycle policy; downsample the index with the downsample API"),
        ("searchable_snapshot", _) => fail_with("roll search does not run the [searchable_snapshot] action from a lifecycle policy"),
        ("freeze", "freeze") => Outcome::Next,
        ("wait_for_snapshot", "wait-for-snapshot") => {
            let p = conf.get("policy").map(Value::to_es_string).unwrap_or_default();
            let Some(since) = e.action_time else { return fail_with(format!("no information about ILM action start in index metadata for index '{i}'")) };
            match super::slm::snapshot_since(&env.cluster.state(), &p, i, since) {
                super::slm::Since::NoPolicy => fail_with(format!("configured policy '{p}' not found")),
                super::slm::Since::NotYet => Outcome::Wait(Some(obj! { "message" => format!("waiting for policy '{p}' to be executed since {}", java_date(since)) })),
                super::slm::Since::Without => fail_with(format!("the last successful snapshot of policy '{p}' does not include index '{i}'")),
                super::slm::Since::Taken => Outcome::Next,
            }
        }
        ("delete", "delete") => delete(env).await,
        _ => fail_with(format!("step [{step}] of action [{action}] is not supported by roll search")),
    }
}

/// `java.util.Date.toString()` in UTC (`EEE MMM dd HH:mm:ss zzz yyyy`).
fn java_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%a %b %d %H:%M:%S GMT %Y").to_string()).unwrap_or_default()
}

/// The rollover conditions with Elasticsearch's defaults (`applyDefaultConditions`): `min_docs:
/// 1` unless `indices.lifecycle.rollover.only_if_has_documents` is off or a document minimum
/// is set, and `max_primary_shard_docs` at most 200,000,000.
pub(crate) fn conditions_with_defaults(st: &ClusterState, conf: &Value) -> Map {
    let mut m = conf.as_object().cloned().unwrap_or_default();
    let only_if_docs = st.metadata.cluster_setting("indices.lifecycle.rollover.only_if_has_documents") != Some("false");
    if only_if_docs && !m.contains_key("min_docs") && !m.contains_key("min_primary_shard_docs") {
        m.insert("min_docs".into(), Value::from(1));
    }
    let current = m.get("max_primary_shard_docs").and_then(Value::as_i64).unwrap_or(i64::MAX);
    m.insert("max_primary_shard_docs".into(), Value::from(current.min(200_000_000)));
    const ORDER: [&str; 10] = ["max_size", "max_primary_shard_size", "max_age", "max_docs", "max_primary_shard_docs", "min_size", "min_primary_shard_size", "min_age", "min_docs", "min_primary_shard_docs"];
    ORDER.iter().filter_map(|k| m.get(*k).map(|v| (k.to_string(), v.clone()))).collect()
}

/// `WaitForRolloverReadyStep`: a dry-run rollover with the conditions.
async fn check_rollover(env: &Env<'_>, idx: &IndexMeta, conf: &Value, e: &mut Exec) -> Outcome {
    let st = env.cluster.state();
    let i = &env.index;
    let target = match st.metadata.data_stream_of(i) {
        Some(ds) => match write_index_of(&st, idx) {
            None => return Outcome::Next,
            Some((_, true)) => format!("{}::failures", ds.name),
            Some(_) => ds.name.clone(),
        },
        None => {
            let alias = idx.settings.get("index.lifecycle.rollover_alias").unwrap_or_default().to_string();
            if alias.is_empty() {
                return fail_arg(format!("setting [index.lifecycle.rollover_alias] for index [{i}] is empty or not defined"));
            }
            if idx.rollover_info.contains_key(&alias) {
                return Outcome::Next;
            }
            let points = idx.aliases.get(&alias);
            let write = points.and_then(|a| a.is_write_index);
            if idx.settings.get("index.lifecycle.indexing_complete") == Some("true") {
                if points.is_some() && write == Some(true) {
                    return fail_with(format!("index [{i}] has [index.lifecycle.indexing_complete] set to [true], but is still the write index for alias [{alias}]"));
                }
                return Outcome::Next;
            }
            if points.is_none() {
                return fail_arg(format!("index.lifecycle.rollover_alias [{alias}] does not point to index [{i}]"));
            }
            if write == Some(false) {
                return fail_arg(format!("index [{i}] is not the write index for alias [{alias}]"));
            }
            alias
        }
    };
    // Without a time-based condition due, only writes can meet one: an index that took none
    // since the cluster loaded (its shards aren't open) isn't asked.
    let time_due = rollover_time(idx, conf).is_some_and(|t| env.now >= t);
    if !time_due && env.cluster.engine.loaded(&idx.uuid).is_none() {
        return Outcome::Wait(Some(Value::object()));
    }
    let conditions = conditions_with_defaults(&st, conf);
    drop(st);
    let body = obj! { "conditions" => Value::Obj(conditions) };
    match env.call("POST", &format!("/{}/_rollover?dry_run=true", enc(&target)), Some(body)).await {
        Ok(r) => {
            let met = r.get("conditions").and_then(Value::as_object).is_some_and(|c| {
                let all_min = c.iter().filter(|(k, _)| k.starts_with("[min_")).all(|(_, v)| v.as_bool() == Some(true));
                let any_max = c.iter().any(|(k, v)| k.starts_with("[max_") && v.as_bool() == Some(true));
                all_min && any_max
            });
            if met {
                return Outcome::Next;
            }
            if time_due {
                e.checked_at = Some(env.now);
            }
            Outcome::Wait(Some(Value::object()))
        }
        Err(err) => Outcome::Fail(err),
    }
}

/// `RolloverStep`: an unconditional rollover.
async fn attempt_rollover(env: &Env<'_>, idx: &IndexMeta) -> Outcome {
    if idx.settings.get("index.lifecycle.indexing_complete") == Some("true") {
        return Outcome::Next;
    }
    let st = env.cluster.state();
    let i = &env.index;
    let target = match st.metadata.data_stream_of(i) {
        Some(ds) => match write_index_of(&st, idx) {
            None => return Outcome::Next,
            Some((_, true)) => format!("{}::failures", ds.name),
            Some(_) => ds.name.clone(),
        },
        None => {
            let alias = idx.settings.get("index.lifecycle.rollover_alias").unwrap_or_default().to_string();
            if alias.is_empty() {
                return fail_arg(format!("setting [index.lifecycle.rollover_alias] for index [{i}] is empty or not defined, it must be set to the name of the alias pointing to the group of indices being rolled over"));
            }
            if idx.rollover_info.contains_key(&alias) {
                return Outcome::Next;
            }
            if !idx.aliases.contains_key(&alias) {
                return fail_arg(format!("index.lifecycle.rollover_alias [{alias}] does not point to index [{i}]"));
            }
            alias
        }
    };
    drop(st);
    env.act("POST", &format!("/{}/_rollover", enc(&target)), None).await
}

/// `DeleteStep`: a data stream's only backing index deletes the data stream; another write
/// index can't be deleted.
async fn delete(env: &Env<'_>) -> Outcome {
    let st = env.cluster.state();
    let i = &env.index;
    if let Some(ds) = st.metadata.data_stream_of(i) {
        let failure_write = ds.failure_write_index() == Some(i.as_str());
        if ds.indices.len() == 1 && !failure_write && ds.write_index() == Some(i.as_str()) {
            let name = ds.name.clone();
            drop(st);
            return match env.call("DELETE", &format!("/_data_stream/{}", enc(&name)), None).await {
                Ok(_) => Outcome::Gone,
                Err(err) => Outcome::Fail(err),
            };
        }
        if failure_write || ds.write_index() == Some(i.as_str()) {
            return fail_with(format!(
                "index [{i}] is the{} write index for data stream [{}]. stopping execution of lifecycle [{}] as a data stream's write index cannot be deleted. manually rolling over the index will resume the execution of the policy as the index will not be the data stream's write index anymore",
                if failure_write { " failure store" } else { "" },
                ds.name,
                env.policy
            ));
        }
    }
    drop(st);
    match env.call("DELETE", &format!("/{}", enc(i)), None).await {
        Ok(_) => Outcome::Gone,
        Err(err) => Outcome::Fail(err),
    }
}

/// Moves one index on from its current step as far as it can go now (`runPeriodicStep` and
/// `runPolicyAfterStateChange`, which in Elasticsearch follow each other within moments).
/// Returns the state to store, or `None` when it is unchanged or the index is gone.
async fn advance(env: &Env<'_>, mut e: Exec) -> Option<Exec> {
    let start = e.clone();
    let st = env.cluster.state();
    let rec = ilm::policy(&st, &env.policy)?;
    let stopping = ilm::mode(&st) == "STOPPING";
    drop(st);
    // Enough for every step of a phase and the phase change after it.
    for _ in 0..64 {
        let Some((phase, action, step)) = e.key().map(|(p, a, s)| (p.to_string(), a.to_string(), s.to_string())) else { break };
        if stopping && !matches!(step.as_str(), "shrink" | "rollup") {
            break;
        }
        if step == ERROR {
            // `onErrorMaybeRetryFailedStep`: back to the failed step, counted; once per pass.
            if e.auto_retryable != Some(true) || e != start {
                break;
            }
            let Some(failed) = e.failed_step.clone() else { break };
            let retries = e.retry_count.map_or(1, |n| n + 1);
            let refresh = rec.pointer(&["policy", "phases", &phase, "actions", &action]).is_some();
            e.move_to(&phase, &action, &failed, Some((&env.policy, &rec)), refresh, false, env.now);
            e.auto_retryable = Some(true);
            e.retry_count = Some(retries);
            continue;
        }
        if action == COMPLETE && step == COMPLETE {
            let Some(next) = next_phase(&rec, &phase) else { break };
            let min_age = rec.pointer(&["policy", "phases", next, "min_age"]).map(time_ms).unwrap_or(0);
            let idx = env.idx()?;
            if env.now < lifecycle_date(&idx, &e).unwrap_or(env.now).saturating_add(min_age) {
                break;
            }
            let Some((a, s)) = first_step_of(next, None, &rec) else { break };
            e.move_to(next, &a, s, Some((&env.policy, &rec)), false, true, env.now);
            continue;
        }
        // A step the cached phase doesn't have stays where it is.
        let Some((next, skip)) = following(&e) else { break };
        match execute(env, &mut e).await {
            Outcome::Next => e.move_to(&phase, &next.0, next.1, Some((&env.policy, &rec)), false, true, env.now),
            Outcome::SkipAction => e.move_to(&phase, &skip.0, skip.1, Some((&env.policy, &rec)), false, true, env.now),
            Outcome::Wait(info) => {
                if info.is_some() && info != e.step_info {
                    e.step_info = info;
                }
                break;
            }
            Outcome::Fail(err) => {
                e.fail(err, env.now);
                break;
            }
            Outcome::Gone => return None,
        }
    }
    (e != start).then_some(e)
}

/// One ILM pass at `now`: every managed index whose next step is due moves on; `STOPPING`
/// becomes `STOPPED` once no index is in a step that must finish first.
pub async fn run(cluster: &Cluster, now: i64) {
    crate::time::at(now, pass(cluster, now)).await
}

async fn pass(cluster: &Cluster, now: i64) {
    let st = cluster.state();
    if !active(&st) {
        return;
    }
    let m = ilm::mode(&st);
    if m == "STOPPED" {
        return;
    }
    if m == "STOPPING" && !managed(&st).any(|(i, _)| exec_of(&st, i).is_some_and(|e| matches!(e.step.as_deref(), Some("shrink" | "rollup")))) {
        drop(st);
        let _ = cluster.update(|st| ilm::put_mode(st, "STOPPED", now)).await;
        return;
    }
    let due: Vec<(String, String, String, Option<Exec>)> = dues(&st, now)
        .filter(|(_, _, (at, poll))| at.is_some_and(|t| t <= now) || *poll)
        .map(|(i, p, _)| (i.name.clone(), i.uuid.clone(), p.to_string(), exec_of(&st, i).filter(|e| e.step.is_some())))
        .collect();
    let mut updates: Vec<(String, String, Exec)> = Vec::new();
    // Managed indices without a state (kept before ILM ran, or whose policy appeared since)
    // start from the beginning, as `reconcile` starts a newly managed one.
    for (index, uuid, policy, e) in &due {
        if e.is_none()
            && let (Some(idx), Some(rec)) = (st.metadata.indices.get(index), ilm::policy(&st, policy))
        {
            updates.push((index.clone(), uuid.clone(), initialized(idx, policy, &rec, now)));
        }
    }
    drop(st);
    for (index, uuid, policy, e) in due {
        let Some(e) = e else { continue };
        let owner = ilm::policy(&cluster.state(), &policy).and_then(|r| r.pointer(&["_roll", "user"]).and_then(internal::principal_of));
        let env = Env { cluster, index: index.clone(), policy, now };
        if let Some(next) = internal::as_principal(owner, advance(&env, e)).await {
            updates.push((index, uuid, next));
        }
    }
    if updates.is_empty() {
        return;
    }
    let _ = cluster
        .update(|st| {
            for (index, uuid, e) in &updates {
                if st.metadata.indices.get(index).is_some_and(|i| &i.uuid == uuid && policy_name(i).is_some()) {
                    store::family_mut(st, STATE).insert(index.clone(), e.to_value());
                }
            }
            Ok(())
        })
        .await;
}

#[cfg(test)]
mod tests {
    use crate::cluster::{Cluster, ClusterConfig, MemoryStore};
    use crate::json::Value;
    use crate::rest::EsRequest;
    use std::sync::Arc;

    fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
        static NULL: Value = Value::Null;
        v.pointer(path).unwrap_or(&NULL)
    }

    const DAY: i64 = 24 * 3600 * 1000;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        let r = crate::rest::handle(c, req).await;
        (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or(Value::Null))
    }

    async fn explain(c: &Cluster, index: &str) -> Value {
        let (s, v) = call(c, "GET", &format!("/{index}/_ilm/explain"), "").await;
        assert_eq!(s, 200, "{v}");
        at(&v, &["indices", index]).clone()
    }

    fn step(v: &Value) -> String {
        format!("{}/{}/{}", at(v, &["phase"]).as_str().unwrap_or("-"), at(v, &["action"]).as_str().unwrap_or("-"), at(v, &["step"]).as_str().unwrap_or("-"))
    }

    async fn cluster() -> Arc<Cluster> {
        Arc::new(Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap())
    }

    /// An alias-managed index rolls over at its `max_age` once it has a document, its lifecycle
    /// date becomes the rollover time, the next phase starts `min_age` after it and runs its
    /// actions in order, and the delete phase deletes it; the due times follow.
    #[tokio::test]
    async fn moves_through_phases_and_deletes() {
        let c = cluster().await;
        let policy = r#"{"policy":{"phases":{
            "hot":{"actions":{"rollover":{"max_age":"1d"},"set_priority":{"priority":100}}},
            "warm":{"min_age":"2d","actions":{"readonly":{},"forcemerge":{"max_num_segments":1}}},
            "delete":{"min_age":"10d","actions":{"delete":{}}}}}}"#;
        assert_eq!(call(&c, "PUT", "/_ilm/policy/p", policy).await.0, 200);
        let body = r#"{"settings":{"index.lifecycle.name":"p","index.lifecycle.rollover_alias":"logs"},"aliases":{"logs":{"is_write_index":true}}}"#;
        assert_eq!(call(&c, "PUT", "/logs-000001", body).await.0, 200);
        // Initialized with the state change that made it managed.
        let v = explain(&c, "logs-000001").await;
        assert_eq!(step(&v), "new/complete/complete", "{v}");
        let created = c.state().metadata.indices["logs-000001"].creation_date();
        assert_eq!(super::next_due(&c.state()), Some(created), "hot's min_age 0 is due at once");
        super::run(&c, created + 1000).await;
        let v = explain(&c, "logs-000001").await;
        assert_eq!(step(&v), "hot/rollover/check-rollover-ready", "{v}");
        assert_eq!(c.state().metadata.indices["logs-000001"].settings.get("index.priority"), Some("100"));
        assert_eq!(at(&v, &["phase_execution", "phase_definition", "min_age"]), &Value::from("0ms"), "{v}");
        assert_eq!(super::next_due(&c.state()), Some(created + DAY), "the rollover's max_age");
        // Empty at max_age: min_docs=1 keeps it; found once, it no longer wakes the cluster.
        super::run(&c, created + DAY).await;
        assert_eq!(step(&explain(&c, "logs-000001").await), "hot/rollover/check-rollover-ready");
        assert_eq!(super::next_due(&c.state()), None);
        assert!(super::polls(&c.state()));
        assert_eq!(call(&c, "POST", "/logs/_doc?refresh=true", r#"{"a":1}"#).await.0, 201);
        let rolled = created + DAY + 5000;
        super::run(&c, rolled).await;
        let v = explain(&c, "logs-000001").await;
        assert_eq!(step(&v), "hot/complete/complete", "{v}");
        assert_eq!(at(&v, &["lifecycle_date_millis"]).as_i64(), Some(rolled), "{v}");
        let st = c.state();
        assert!(st.metadata.indices.contains_key("logs-000002"));
        assert_eq!(st.metadata.indices["logs-000001"].settings.get("index.lifecycle.indexing_complete"), Some("true"));
        drop(st);
        // The new index has hot work due now; the old one's warm phase is 2d after the rollover.
        let st = c.state();
        let old_due = super::index_due(&st, &st.metadata.indices["logs-000001"], super::ilm::policy(&st, "p").as_ref(), rolled).0;
        drop(st);
        assert_eq!(old_due, Some(rolled + 2 * DAY));
        super::run(&c, rolled + 2 * DAY).await;
        let v = explain(&c, "logs-000001").await;
        // readonly, forcemerge, the injected migrate, then the phase completes.
        assert_eq!(step(&v), "warm/complete/complete", "{v}");
        let st = c.state();
        assert_eq!(st.metadata.indices["logs-000001"].settings.get("index.blocks.write"), Some("true"));
        assert_eq!(st.metadata.indices["logs-000001"].settings.get("index.routing.allocation.include._tier_preference"), Some("data_warm,data_hot"));
        drop(st);
        super::run(&c, rolled + 10 * DAY).await;
        assert!(!c.state().metadata.indices.contains_key("logs-000001"));
        assert!(c.state().metadata.xpack.get(super::STATE, "logs-000001").is_none());
    }

    /// A failing step goes to `ERROR` with the reason; `_ilm/retry` moves it back and the run
    /// succeeds once the cause is fixed; `_ilm/stop` goes through `STOPPING` to `STOPPED`.
    #[tokio::test]
    async fn errors_retry_and_stop() {
        let c = cluster().await;
        let policy = r#"{"policy":{"phases":{"hot":{"actions":{"rollover":{"max_docs":1}}}}}}"#;
        assert_eq!(call(&c, "PUT", "/_ilm/policy/p", policy).await.0, 200);
        assert_eq!(call(&c, "PUT", "/i-000001", r#"{"settings":{"index.lifecycle.name":"p"}}"#).await.0, 200);
        let now = crate::time::now_millis();
        super::run(&c, now).await;
        let v = explain(&c, "i-000001").await;
        assert_eq!(step(&v), "hot/rollover/ERROR", "{v}");
        assert_eq!(at(&v, &["failed_step"]), &Value::from("check-rollover-ready"));
        assert_eq!(at(&v, &["step_info", "reason"]), &Value::from("setting [index.lifecycle.rollover_alias] for index [i-000001] is empty or not defined"), "{v}");
        assert_eq!(at(&v, &["is_auto_retryable_error"]), &Value::Bool(true));
        let (_, errors) = call(&c, "GET", "/*/_ilm/explain?only_errors=true", "").await;
        assert_eq!(at(&errors, &["indices"]).as_object().map(|m| m.len()), Some(1), "{errors}");
        assert_eq!(call(&c, "PUT", "/i-000001/_settings", r#"{"index.lifecycle.rollover_alias":"i"}"#).await.0, 200);
        assert_eq!(call(&c, "POST", "/_aliases", r#"{"actions":[{"add":{"index":"i-000001","alias":"i","is_write_index":true}}]}"#).await.0, 200);
        assert_eq!(call(&c, "POST", "/i-000001/_ilm/retry", "").await.0, 200);
        assert_eq!(step(&explain(&c, "i-000001").await), "hot/rollover/check-rollover-ready");
        assert_eq!(call(&c, "POST", "/i/_doc?refresh=true", r#"{"a":1}"#).await.0, 201);
        super::run(&c, now + 1).await;
        assert_eq!(step(&explain(&c, "i-000001").await), "hot/complete/complete");
        assert_eq!(call(&c, "POST", "/_ilm/stop", "").await.0, 200);
        assert_eq!(at(&call(&c, "GET", "/_ilm/status", "").await.1, &["operation_mode"]), &Value::from("STOPPING"));
        super::run(&c, now + 2).await;
        assert_eq!(at(&call(&c, "GET", "/_ilm/status", "").await.1, &["operation_mode"]), &Value::from("STOPPED"));
        assert_eq!(super::next_due(&c.state()), None);
    }
}

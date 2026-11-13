//! Watcher: watches (`_watcher/watch`), `_execute`, `_ack`, `_activate`/`_deactivate`,
//! `_query/watches`, `_watcher/stats`, `_start`/`_stop` and `_watcher/settings`.
//!
//! Watches are validated and stored as Elasticsearch stores them in `.watches` ([`parse`]),
//! with its status bookkeeping (versions, sequence numbers, per-action ack, execution and
//! throttle states). A watch runs for real ([`exec`]): inputs `simple`, `search`, `http`,
//! `chain` and `transform`; conditions `always`, `never`, `compare`, `array_compare` and
//! painless `script`s; transforms `search`, `script` and `chain`; actions `index`, `logging`
//! and `webhook`, with throttling, acks, action conditions and transforms and `foreach`.
//!
//! **Triggers** ([`schedule`]) run on the cluster's job scheduler
//! ([`crate::opensearch::jobs`]): an active watch keeps the time it next fires in its stored
//! state (`_roll.next_run`), so it survives the cluster sleeping, and a sleeping cluster is
//! woken when one is due. Every triggered execution, and every `_execute` with
//! `record_execution`, updates the watch's status and writes its watch record to
//! `.watcher-history-18-<date>`.
//!
//! **Security:** a watch stored by a user who isn't a superuser runs its searches and writes as
//! that user (Elasticsearch's stored security headers): the user and roles are kept with the
//! watch (never credentials) and its internal calls are authorized as them.
//!
//! **What is emulated:** `email`, `slack`, `pagerduty` and `jira` actions are simulated as
//! Elasticsearch simulates them, but roll search sends no notifications: executing one fails
//! as Elasticsearch does without an account. `http` inputs and `webhook` actions only reach
//! public addresses ([`http`]).

mod compare;
pub mod exec;
pub mod http;
mod parse;
pub mod schedule;

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, store};
use exec::fmt_ms;

pub const WATCHES: &str = "watcher.watch";
const STATE: &str = "watcher.state";

/// The watch history's template version: records go to `.watcher-history-18-<yyyy.MM.dd>`.
const HISTORY: &str = ".watcher-history-18";

/// `xpack.watcher.execution.default_throttle_period`.
const DEFAULT_THROTTLE_MS: i64 = 5000;

fn state(st: &crate::meta::ClusterState) -> Map {
    st.metadata.xpack.get(STATE, "state").and_then(Value::as_object).cloned().unwrap_or_default()
}

fn parse_err(msg: impl Into<String>) -> EsError {
    EsError::new(400, "parse_exception", msg)
}

/// Checks a Quartz cron expression's shape (6 or 7 fields of the allowed characters).
fn check_cron(expr: &str) -> Result<(), EsError> {
    cron_problem(expr).map_or(Ok(()), |detail| {
        Err(parse_err("could not parse [cron] schedule").caused_by(
            EsError::illegal_argument(format!("invalid cron expression [{expr}]"))
                .caused_by(EsError::illegal_argument(format!("illegal cron expression format [java.lang.IllegalArgumentException: {detail}]"))),
        ))
    })
}

/// Why Elasticsearch's `Cron` rejects a Quartz expression (the message of its
/// `IllegalArgumentException`), or `None`. Shared with the connectors' schedules.
pub(crate) fn cron_problem(expr: &str) -> Option<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let mut pos = 0;
    for f in &fields {
        let up = f.to_ascii_uppercase();
        let word: String = up.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        const NAMES: [&str; 22] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC", "SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT", "L", "W", "LW"];
        if !word.is_empty() && !NAMES.iter().any(|n| word.contains(n)) && !word.chars().all(|c| matches!(c, 'L' | 'W')) {
            return Some(format!("illegal characters [{word}] at pos [{pos}] '"));
        }
        if f.chars().any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '*' | '?' | ',' | '-' | '/' | '#'))) {
            return Some(format!("illegal characters [{f}] at pos [{pos}] '"));
        }
        pos += f.len() + 1;
    }
    if fields.len() < 6 || fields.len() > 7 {
        return Some("Unexpected end of expression.".into());
    }
    // Value ranges per field (the numbers of each list item and range, not increments).
    const RANGES: [(u32, u32, &str); 6] = [
        (0, 59, "Minute and Second values must be between 0 and 59"),
        (0, 59, "Minute and Second values must be between 0 and 59"),
        (0, 23, "Hour values must be between 0 and 23"),
        (1, 31, "Day of month values must be between 1 and 31"),
        (1, 12, "Month values must be between 1 and 12"),
        (1, 7, "Day-of-Week values must be between 1 and 7"),
    ];
    for (f, (lo, hi, msg)) in fields.iter().zip(RANGES) {
        for item in f.split(',') {
            let base = item.split('/').next().unwrap_or("");
            for n in base.split(['-', '#']).take(1).chain(base.split('-').skip(1)) {
                let digits: String = n.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(v) = digits.parse::<u32>()
                    && (v < lo || v > hi)
                {
                    return Some(msg.into());
                }
            }
        }
    }
    if fields[3] != "?" && fields[5] != "?" {
        return Some("support for specifying both a day-of-week AND a day-of-month parameter is not implemented.".into());
    }
    None
}

/// The version a watch id's next write gets: one past the live watch, or past the tombstone a
/// recent delete left (Elasticsearch keeps deleted versions of `.watches` documents).
fn next_version(st: &crate::meta::ClusterState, id: &str) -> i64 {
    let live = st.metadata.xpack.get(WATCHES, id).and_then(|o| o.get("_version")).and_then(Value::as_i64);
    let tomb = state(st).get("tombstones").and_then(|t| t.get(id)).and_then(Value::as_i64);
    live.or(tomb).unwrap_or(0) + 1
}

/// Records (`Some`) or clears a watch id's tombstone version.
fn set_tombstone(st: &mut crate::meta::ClusterState, id: &str, version: Option<i64>, now: i64) -> EsResult<()> {
    let mut s = state(st);
    let mut t = s.get("tombstones").and_then(Value::as_object).cloned().unwrap_or_default();
    match version {
        Some(v) => {
            t.insert(id.to_string(), v.into());
        }
        None => {
            if t.shift_remove(id).is_none() {
                return Ok(());
            }
        }
    }
    s.insert("tombstones".into(), Value::Obj(t));
    store::put(st, STATE, "state", Value::Obj(s), now).map(|_| ())
}

fn initial_status(w: &Map, active: bool, now: i64) -> Map {
    let mut actions = Map::new();
    for aid in w.get("actions").and_then(Value::as_object).into_iter().flat_map(|m| m.keys()) {
        actions.insert(aid.clone(), obj! { "ack" => obj! { "timestamp" => fmt_ms(now), "state" => "awaits_successful_execution" } });
    }
    let mut s = Map::new();
    s.insert("state".into(), obj! { "active" => active, "timestamp" => fmt_ms(now) });
    s.insert("actions".into(), Value::Obj(crate::json::java_hash_map(actions, 16)));
    s
}

fn next_seq(st: &mut crate::meta::ClusterState, now: i64) -> EsResult<i64> {
    let mut s = state(st);
    let seq = s.get("seq_no").and_then(Value::as_i64).map_or(0, |n| n + 1);
    s.insert("seq_no".into(), seq.into());
    store::put(st, STATE, "state", Value::Obj(s), now)?;
    Ok(seq)
}

// --- CRUD ------------------------------------------------------------------------------------

/// When an active watch next fires, from its activation.
fn next_run(watch: &Value, activated: i64, after: i64) -> Option<i64> {
    schedule::parse("", watch.get("trigger")).ok()?.next_after(activated, after)
}

fn conflict(id: &str, seq: i64, term: i64, current: Option<i64>) -> EsError {
    let reason = match current {
        Some(c) => format!("[{id}]: version conflict, required seqNo [{seq}], primary term [{term}]. current document has seqNo [{c}] and primary term [1]"),
        None => format!("[{id}]: version conflict, required seqNo [{seq}], primary term [{term}]. but no document was found"),
    };
    EsError::new(409, "version_conflict_engine_exception", reason).with_meta("index_uuid", "_na_").with_meta("shard", "0").with_meta("index", ".watches")
}

pub async fn put_watch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let active = ctx.bool_param("active", true)?;
    if ctx.param("version").is_some() {
        return Err(EsError::java(500, "unsupported_operation_exception", "update requests do not support versioning"));
    }
    let num = |k: &str| -> EsResult<Option<i64>> { ctx.param(k).map(|v| v.parse::<i64>().map_err(|_| EsError::illegal_argument(format!("failed to parse [{k}] with value [{v}]")))).transpose() };
    let if_seq = num("if_seq_no")?;
    let if_term = num("if_primary_term")?;
    let raw = ctx.body_map()?.unwrap_or_default();
    let previous = if_seq.and_then(|_| ctx.state().metadata.xpack.get(WATCHES, &id).and_then(|w| w.get("watch").cloned()));
    let body = parse::normalize_watch(ctx.cluster, &id, &raw, previous.as_ref())?;
    let body = parse::sealed_watch(ctx.cluster, &body);
    if parse::uses_ignore_throttled(&Value::Obj(raw)) {
        ctx.warn(parse::IGNORE_THROTTLED);
    }
    let user = internal::stored_user(crate::xpack::security::principal(ctx));
    let now = ctx.now_ms;
    let (version, seq, created) = ctx
        .cluster
        .update(move |st| {
            let old = st.metadata.xpack.get(WATCHES, &id).cloned();
            if let Some(s) = if_seq {
                let term = if_term.unwrap_or(1);
                let cur = old.as_ref().and_then(|o| o.get("_seq_no")).and_then(Value::as_i64);
                if cur != Some(s) || term != 1 {
                    return Err(conflict(&id, s, term, cur));
                }
            }
            let version = next_version(st, &id);
            set_tombstone(st, &id, None, now)?;
            let seq = next_seq(st, now)?;
            let w = Value::Obj(body.clone());
            let mut roll = Map::new();
            roll.insert("activated".into(), now.into());
            if active && let Some(n) = next_run(&w, now, now) {
                roll.insert("next_run".into(), n.into());
            }
            if let Some(u) = user.clone() {
                roll.insert("user".into(), u);
            }
            let doc = obj! { "watch" => w, "status" => Value::Obj(initial_status(&body, active, now)), "_version" => version, "_seq_no" => seq, "_roll" => Value::Obj(roll) };
            store::put(st, WATCHES, &id, doc, now)?;
            Ok((version, seq, old.is_none()))
        })
        .await?;
    let id = ctx.path_param("id").unwrap_or_default();
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "_id" => id, "_version" => version, "_seq_no" => seq, "_primary_term" => 1, "created" => created }))
}

/// A stored status as Elasticsearch prints it: its fields in order, the actions as the
/// `HashMap` they are, and `version`.
fn status_out(status: &Map, version: Value) -> Value {
    let mut s = status.clone();
    let mut out = Map::new();
    for k in ["state", "last_checked", "last_met_condition", "actions", "execution_state"] {
        if let Some(v) = s.shift_remove(k) {
            let v = match (k, v) {
                ("actions", Value::Obj(a)) => Value::Obj(crate::json::java_hash_map(a, 16)),
                (_, v) => v,
            };
            out.insert(k.into(), v);
        }
    }
    s.shift_remove("version");
    out.extend(s);
    out.insert("version".into(), version);
    Value::Obj(out)
}

fn stored_status(w: &Value) -> Map {
    w.get("status").and_then(Value::as_object).cloned().unwrap_or_default()
}

pub fn get_watch(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    let st = ctx.state();
    let Some(w) = st.metadata.xpack.get(WATCHES, id) else {
        return Ok(Reply::Body(404, obj! { "found" => false, "_id" => id }));
    };
    let watch = w.get("watch").cloned().unwrap_or(Value::Null);
    if parse::uses_ignore_throttled(&watch) {
        ctx.warn(parse::IGNORE_THROTTLED);
    }
    let version = w.get("_version").cloned().unwrap_or(Value::from(1));
    Reply::ok(obj! {
        "found" => true, "_id" => id, "_version" => version.clone(),
        "_seq_no" => w.get("_seq_no").cloned().unwrap_or(Value::from(0)), "_primary_term" => 1,
        "status" => status_out(&stored_status(w), version), "watch" => parse::redacted_watch(&watch),
    })
}

pub async fn delete_watch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    let out_id = id.clone();
    let (found, version) = ctx
        .cluster
        .update(move |st| {
            let version = next_version(st, &id);
            let old = store::remove(st, WATCHES, &id);
            set_tombstone(st, &id, Some(version), now)?;
            next_seq(st, now)?;
            Ok((old.is_some(), version))
        })
        .await?;
    Ok(Reply::Body(if found { 200 } else { 404 }, obj! { "_id" => out_id, "_version" => version, "found" => found }))
}

/// Applies `f` to a stored watch (its status, and its `_roll` state), bumping its version.
async fn modify_status(ctx: &Ctx<'_>, id: String, f: impl FnOnce(&mut Map, &mut Map, &Value, i64) -> EsResult<bool> + Send) -> EsResult<Reply> {
    let now = ctx.now_ms;
    let w = ctx
        .cluster
        .update(move |st| {
            let Some(Value::Obj(mut w)) = st.metadata.xpack.get(WATCHES, &id).cloned() else {
                return Err(EsError::resource_not_found(format!("Watch with id [{id}] does not exist")));
            };
            let mut status = w.get("status").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut roll = w.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
            let watch = w.get("watch").cloned().unwrap_or(Value::Null);
            // Nothing changed (an ack with nothing to ack): the document isn't rewritten.
            if !f(&mut status, &mut roll, &watch, now)? {
                return Ok(Value::Obj(w));
            }
            w.insert("status".into(), Value::Obj(status));
            w.insert("_roll".into(), Value::Obj(roll));
            let version = w.get("_version").and_then(Value::as_i64).unwrap_or(1) + 1;
            w.insert("_version".into(), version.into());
            let seq = next_seq(st, now)?;
            w.insert("_seq_no".into(), seq.into());
            let v = Value::Obj(w);
            store::put(st, WATCHES, &id, v.clone(), now)?;
            Ok(v)
        })
        .await?;
    Reply::ok(obj! { "status" => status_out(&stored_status(&w), w.get("_version").cloned().unwrap_or(Value::from(1))) })
}

pub async fn ack_watch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("watch_id").unwrap_or_default().to_string();
    let ids = ctx.list_param("action_id");
    modify_status(ctx, id, move |status, _, _, now| {
        let mut changed = false;
        if let Some(Value::Obj(actions)) = status.get_mut("actions") {
            for (aid, a) in actions.iter_mut() {
                if ids.as_ref().is_some_and(|l| !l.iter().any(|x| x == aid || x == "_all")) {
                    continue;
                }
                if let Some(Value::Obj(ack)) = a.as_object_mut().and_then(|m| m.get_mut("ack"))
                    && ack.get("state").and_then(Value::as_str) == Some("ackable")
                {
                    ack.insert("state".into(), "acked".into());
                    ack.insert("timestamp".into(), fmt_ms(now).into());
                    changed = true;
                }
            }
        }
        Ok(changed)
    })
    .await
}

pub async fn set_active(ctx: &Ctx<'_>, active: bool) -> EsResult<Reply> {
    let id = ctx.path_param("watch_id").unwrap_or_default().to_string();
    modify_status(ctx, id, move |status, roll, watch, now| {
        status.insert("state".into(), obj! { "active" => active, "timestamp" => fmt_ms(now) });
        roll.insert("activated".into(), now.into());
        roll.shift_remove("next_run");
        if active && let Some(n) = next_run(watch, now, now) {
            roll.insert("next_run".into(), n.into());
        }
        Ok(true)
    })
    .await
}

pub fn query_watches(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let from = body.get("from").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
    let size = body.get("size").and_then(Value::as_i64).unwrap_or(10).max(0) as usize;
    let st = ctx.state();
    let mut warn = false;
    let all: Vec<Value> = st
        .metadata
        .xpack
        .iter(WATCHES)
        .map(|(id, w)| {
            let watch = w.get("watch").cloned().unwrap_or(Value::Null);
            warn |= parse::uses_ignore_throttled(&watch);
            obj! { "_id" => id.clone(), "watch" => parse::redacted_watch(&watch), "status" => status_out(&stored_status(w), Value::from(-1)), "_seq_no" => w.get("_seq_no").cloned().unwrap_or(Value::from(0)), "_primary_term" => 1 }
        })
        .collect();
    if warn {
        ctx.warn(parse::IGNORE_THROTTLED);
    }
    let count = all.len();
    Reply::ok(obj! { "count" => count, "watches" => Value::Arr(all.into_iter().skip(from).take(size).collect()) })
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let s = state(&st);
    let stopped = s.get("stopped").and_then(Value::as_bool).unwrap_or(false);
    let metrics = ctx.list_param("metric").unwrap_or_default();
    if metrics.iter().any(|m| m == "pending_watches") {
        ctx.warn("The pending_watches parameter is deprecated, use queued_watches instead");
    }
    let mut node = Map::new();
    node.insert("node_id".into(), ctx.cluster.node.id.clone().into());
    node.insert("watcher_state".into(), if stopped { "stopped" } else { "started" }.into());
    let count = if stopped { 0 } else { st.metadata.xpack.iter(WATCHES).filter(|(_, w)| w.pointer(&["status", "state", "active"]) != Some(&Value::Bool(false))).count() };
    node.insert("watch_count".into(), count.into());
    node.insert("execution_thread_pool".into(), obj! { "queue_size" => 0, "max_size" => if stopped { 2 } else { 0 } });
    for m in &metrics {
        if m == "current_watches" || m == "_all" {
            node.insert("current_watches".into(), Value::Arr(vec![]));
        }
        if m == "queued_watches" || m == "pending_watches" || m == "_all" {
            node.insert("queued_watches".into(), Value::Arr(vec![]));
        }
    }
    Reply::ok(obj! {
        "_nodes" => obj! { "total" => 1, "successful" => 1, "failed" => 0 },
        "cluster_name" => ctx.cluster.name.clone(),
        "manually_stopped" => stopped,
        "stats" => crate::arr![Value::Obj(node)],
    })
}

pub async fn set_running(ctx: &Ctx<'_>, running: bool) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let mut s = state(st);
            s.insert("stopped".into(), (!running).into());
            store::put(st, STATE, "state", Value::Obj(s), now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

const SETTABLE: [&str; 2] = ["index.auto_expand_replicas", "index.number_of_replicas"];

fn settable(k: &str) -> bool {
    SETTABLE.contains(&k)
        || (["index.routing.allocation.require.", "index.routing.allocation.include.", "index.routing.allocation.exclude."].iter().any(|p| k.starts_with(p))
            && k != "index.routing.allocation.include._tier_preference")
}

fn settings_of(st: &crate::meta::ClusterState) -> Map {
    let mut m = Map::new();
    m.insert("index.auto_expand_replicas".into(), "0-1".into());
    m.insert("index.number_of_replicas".into(), "0".into());
    if let Some(Value::Obj(s)) = state(st).get("settings") {
        for (k, v) in s {
            m.insert(k.clone(), v.clone());
        }
    }
    m
}

pub fn get_settings(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let flat = settings_of(&ctx.state());
    let s = crate::meta::settings::Settings::from_json(&Value::Obj(flat))?;
    Reply::ok(s.to_nested_json())
}

pub async fn update_settings(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?.unwrap_or_default();
    let parsed = crate::meta::settings::Settings::from_json(&Value::Obj(body))?;
    let flat = match parsed.to_flat_json() {
        Value::Obj(m) => m,
        _ => Map::new(),
    };
    let bad: Vec<String> = flat.keys().filter(|k| !settable(k)).cloned().collect();
    if !bad.is_empty() {
        return Err(EsError::action_request_validation(&[format!(
            "illegal settings: [{}], these settings may not be configured. Only the following settings may be configured: [index.auto_expand_replicas, index.number_of_replicas], [index.routing.allocation.require.*, index.routing.allocation.include.*, index.routing.allocation.exclude.*] excluding the following explicitly denied settings: [index.routing.allocation.include._tier_preference]",
            bad.join(", ")
        )]));
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let mut s = state(st);
            let mut cur = s.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
            for (k, v) in flat {
                if v.is_null() {
                    cur.shift_remove(&k);
                } else {
                    cur.insert(k, v);
                }
            }
            s.insert("settings".into(), Value::Obj(cur));
            store::put(st, STATE, "state", Value::Obj(s), now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

// --- execution ---------------------------------------------------------------------------------

fn uuid_like() -> String {
    let b: [u8; 16] = rand::random();
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-4{}-{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[16..20], &h[20..32])
}

fn now_instant() -> crate::time::Instant {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    crate::time::Instant { secs: d.as_secs() as i64, nanos: d.subsec_nanos() }
}

/// What a watch record is made of besides the execution's outcome.
struct RecordOf<'a> {
    watch: &'a Value,
    status_version: Value,
    trigger_event: Value,
    user: Option<String>,
    timestamp: String,
    started: std::time::Instant,
    debug: bool,
}

/// A watch record (`WatchRecord.toXContent`).
fn record(run: &exec::Run<'_>, out: &exec::Outcome, r: RecordOf<'_>) -> Value {
    let mut wr = Map::new();
    wr.insert("watch_id".into(), run.watch_id.clone().into());
    wr.insert("node".into(), run.cluster.node.id.clone().into());
    wr.insert("state".into(), out.state.into());
    wr.insert("@timestamp".into(), r.timestamp.into());
    if let Some(u) = r.user {
        wr.insert("user".into(), u.into());
    }
    wr.insert("status".into(), status_out(&out.status, r.status_version));
    wr.insert("trigger_event".into(), r.trigger_event);
    wr.insert("input".into(), parse::redacted_input(r.watch.get("input").unwrap_or(&Value::Null)));
    wr.insert("condition".into(), r.watch.get("condition").cloned().unwrap_or_else(|| obj! { "always" => obj! {} }));
    if let Some(m) = r.watch.get("metadata") {
        wr.insert("metadata".into(), m.clone());
    }
    let mut result = Map::new();
    result.insert("execution_time".into(), fmt_ms(run.execution.millis()).into());
    result.insert("execution_duration".into(), (r.started.elapsed().as_millis() as i64).into());
    result.insert("input".into(), out.input.clone());
    if let Some(c) = &out.condition {
        result.insert("condition".into(), c.clone());
    }
    if let Some(t) = &out.transform {
        result.insert("transform".into(), t.clone());
    }
    result.insert("actions".into(), Value::Arr(out.actions.iter().map(|(_, v)| v.clone()).collect()));
    wr.insert("result".into(), Value::Obj(result));
    if r.debug {
        wr.insert("vars".into(), obj! {});
    }
    match &out.exception {
        Some(e) => {
            wr.insert("exception".into(), e.clone());
        }
        None => {
            wr.insert("messages".into(), Value::Arr(out.messages.iter().map(Value::from).collect()));
        }
    }
    Value::Obj(wr)
}

/// Writes a watch record to the day's history index (hidden; created on first use).
async fn write_history(cluster: &Cluster, record_id: &str, exec_ms: i64, record: &Value) {
    let day = chrono::DateTime::from_timestamp_millis(exec_ms).map(|d| d.format("%Y.%m.%d").to_string()).unwrap_or_default();
    let index = format!("{HISTORY}-{day}");
    if cluster.state().index(&index).is_none() {
        let body = obj! {
            "settings" => obj! { "index.hidden" => true, "index.number_of_shards" => 1, "index.auto_expand_replicas" => "0-1" },
            "mappings" => obj! { "_meta" => obj! { "watcher-history-version" => "18" }, "dynamic" => false, "properties" => obj! {
                "watch_id" => obj! { "type" => "keyword" }, "node" => obj! { "type" => "keyword" }, "state" => obj! { "type" => "keyword" },
                "user" => obj! { "type" => "text" }, "@timestamp" => obj! { "type" => "date" },
                "trigger_event" => obj! { "type" => "object", "dynamic" => true, "properties" => obj! { "type" => obj! { "type" => "keyword" }, "triggered_time" => obj! { "type" => "date" } } },
                "result" => obj! { "type" => "object", "dynamic" => false, "properties" => obj! { "execution_time" => obj! { "type" => "date" }, "execution_duration" => obj! { "type" => "long" } } },
                "messages" => obj! { "type" => "text" },
            } },
        };
        let _ = internal::call_system(cluster, "PUT", &format!("/{}", enc(&index)), Some(&body)).await;
    }
    let _ = internal::call_system(cluster, "PUT", &format!("/{}/_doc/{}?op_type=create", enc(&index), enc(record_id)), Some(record)).await;
}

/// The execution modes of `action_modes`.
fn parse_modes(v: Option<&Value>) -> EsResult<Map> {
    let mut out = Map::new();
    for (k, m) in v.and_then(Value::as_object).into_iter().flatten() {
        let mode = m.to_es_string().to_ascii_lowercase();
        if !matches!(mode.as_str(), "simulate" | "force_simulate" | "execute" | "force_execute" | "skip") {
            return Err(parse_err("could not parse watch execution request").caused_by(EsError::illegal_argument(format!("unknown action execution mode [{}]", m.to_es_string()))));
        }
        out.insert(k.clone(), mode.into());
    }
    Ok(out)
}

pub async fn execute_watch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let started = std::time::Instant::now();
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if matches!(k.as_str(), "trigger" | "input" | "condition" | "actions" | "transform" | "metadata" | "throttle_period" | "throttle_period_in_millis") {
            return Err(parse_err(format!("could not parse watch execution request. unexpected field [{k}], please wrap watch including field [{k}] inside a \"watch\" field")));
        }
        if !matches!(k.as_str(), "trigger_data" | "ignore_condition" | "record_execution" | "alternative_input" | "action_modes" | "watch") {
            return Err(parse_err(format!("could not parse watch execution request. unexpected field [{k}]")));
        }
    }
    let debug = ctx.bool_param("debug", false)?;
    let modes = parse_modes(body.get("action_modes"))?;
    let record_execution = body.get("record_execution").and_then(Value::as_bool).unwrap_or(false);
    let id_param = ctx.path_param("id");
    let mut errors = Vec::new();
    match (id_param, body.get("watch")) {
        (Some(_), Some(_)) => errors.push("a watch execution request must either have a watch id or an inline watch source but not both".to_string()),
        (None, None) => errors.push("a watch execution request must either have a watch id or an inline watch source, but both are missing".to_string()),
        _ => {}
    }
    if body.get("watch").is_some() && record_execution {
        errors.push("the execution of an inline watch cannot be recorded".into());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let execution = now_instant();
    let now = execution.millis();
    let (id, watch, status, version, owner) = match id_param {
        Some(id) => {
            let st = ctx.state();
            let Some(w) = st.metadata.xpack.get(WATCHES, id) else { return Err(EsError::resource_not_found(format!("Watch with id [{id}] does not exist"))) };
            let watch = w.get("watch").cloned().unwrap_or(Value::Null);
            if parse::uses_ignore_throttled(&watch) {
                ctx.warn(parse::IGNORE_THROTTLED);
            }
            let owner = w.pointer(&["_roll", "user"]).and_then(internal::principal_of);
            (id.to_string(), watch, stored_status(w), w.get("_version").cloned().unwrap_or(Value::from(1)), owner)
        }
        None => {
            let Some(Value::Obj(w)) = body.get("watch") else { return Err(EsError::action_request_validation(&["a watch execution request must either have a watch id or an inline watch source, but both are missing".into()])) };
            let watch = parse::normalize_watch(ctx.cluster, "_inlined_", w, None)?;
            let status = initial_status(&watch, true, now);
            let owner = internal::stored_user(crate::xpack::security::principal(ctx)).as_ref().and_then(internal::principal_of);
            ("_inlined_".to_string(), Value::Obj(watch), status, Value::from(-1), owner)
        }
    };
    let trigger = body.get("trigger_data");
    let triggered = trigger.and_then(|t| t.get("triggered_time")).map(exec::parse_date).transpose()?;
    let scheduled = trigger.and_then(|t| t.get("scheduled_time")).map(exec::parse_date).transpose()?;
    let triggered_ms = triggered.unwrap_or(now);
    let scheduled_ms = scheduled.unwrap_or(triggered_ms);
    let record_id = format!("{id}_{}-{}", uuid_like(), crate::time::java_instant_string_nanos(execution));
    let run = exec::Run {
        cluster: ctx.cluster,
        watch_id: id.clone(),
        record_id: record_id.clone(),
        execution,
        triggered_ms,
        scheduled_ms,
        metadata: watch.get("metadata").cloned(),
        modes,
        throttle_ms: watch.get("throttle_period_in_millis").and_then(Value::as_i64).unwrap_or(DEFAULT_THROTTLE_MS),
    };
    let ignore = body.get("ignore_condition").and_then(Value::as_bool).unwrap_or(false);
    let out = internal::as_principal(owner, run.execute(&watch, status, body.get("alternative_input").cloned(), ignore)).await;
    let timestamp = if triggered.is_some() { fmt_ms(triggered_ms) } else { crate::time::java_instant_string_nanos(execution) };
    let rec = record(&run, &out, RecordOf {
        watch: &watch,
        status_version: version.clone(),
        trigger_event: obj! { "type" => "manual", "triggered_time" => fmt_ms(triggered_ms), "manual" => obj! { "schedule" => obj! { "scheduled_time" => fmt_ms(scheduled_ms) } } },
        user: Some(crate::xpack::security::principal(ctx).username.clone()),
        timestamp,
        started,
        debug,
    });
    if id_param.is_some() && record_execution {
        save_status(ctx.cluster, &id, version.as_i64(), out.status.clone(), None, now).await?;
        write_history(ctx.cluster, &record_id, now, &rec).await;
    }
    Reply::ok(obj! { "_id" => record_id, "watch_record" => rec })
}

/// Stores a watch's status after an execution (a new version), unless the watch changed since
/// it was read (`version`). `next_run`: for a triggered execution, the time it was due and the
/// time it fires next, stored even when the watch changed meanwhile (unless that change
/// rescheduled it).
async fn save_status(cluster: &Cluster, id: &str, version: Option<i64>, status: Map, next_run: Option<(i64, Option<i64>)>, now: i64) -> EsResult<()> {
    let id = id.to_string();
    cluster
        .update(move |st| {
            let Some(Value::Obj(mut w)) = st.metadata.xpack.get(WATCHES, &id).cloned() else { return Ok(()) };
            let mut roll = w.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
            let moved = match next_run {
                Some((due, next)) if roll.get("next_run").and_then(Value::as_i64) == Some(due) => {
                    roll.shift_remove("next_run");
                    if let Some(n) = next {
                        roll.insert("next_run".into(), n.into());
                    }
                    w.insert("_roll".into(), Value::Obj(roll));
                    true
                }
                _ => false,
            };
            if w.get("_version").and_then(Value::as_i64) != version {
                if moved {
                    store::put(st, WATCHES, &id, Value::Obj(w), now)?;
                }
                return Ok(());
            }
            w.insert("status".into(), Value::Obj(status));
            w.insert("_version".into(), (version.unwrap_or(1) + 1).into());
            let seq = next_seq(st, now)?;
            w.insert("_seq_no".into(), seq.into());
            store::put(st, WATCHES, &id, Value::Obj(w), now).map(|_| ())
        })
        .await
}

// --- triggers ----------------------------------------------------------------------------------

fn due_at(w: &Value) -> Option<i64> {
    if w.pointer(&["status", "state", "active"]) == Some(&Value::Bool(false)) {
        return None;
    }
    match w.pointer(&["_roll", "next_run"]).and_then(Value::as_i64) {
        Some(t) => Some(t),
        // Stored before watches were scheduled: due at once, to be given its first time.
        None if unscheduled(w) => Some(0),
        None => None,
    }
}

/// An active watch stored before watches were scheduled (no activation time kept).
fn unscheduled(w: &Value) -> bool {
    w.pointer(&["_roll", "activated"]).is_none()
}

/// Gives an [`unscheduled`] watch its activation (now) and first fire time, as Elasticsearch
/// schedules the watches it finds when it starts: missed runs aren't caught up.
async fn schedule(cluster: &Cluster, id: &str, now: i64) {
    let id = id.to_string();
    let _ = cluster
        .update(move |st| {
            let Some(Value::Obj(mut w)) = st.metadata.xpack.get(WATCHES, &id).cloned() else { return Ok(()) };
            if !unscheduled(&Value::Obj(w.clone())) {
                return Ok(());
            }
            let mut roll = w.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
            roll.insert("activated".into(), now.into());
            if let Some(n) = w.get("watch").and_then(|watch| next_run(watch, now, now)) {
                roll.insert("next_run".into(), n.into());
            }
            w.insert("_roll".into(), Value::Obj(roll));
            store::put(st, WATCHES, &id, Value::Obj(w), now).map(|_| ())
        })
        .await;
}

/// When the next active watch fires (none while watcher is stopped): the cluster's job
/// scheduler sleeps until then.
pub fn next_due(st: &crate::meta::ClusterState) -> Option<i64> {
    if state(st).get("stopped").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    st.metadata.xpack.iter(WATCHES).filter_map(|(_, w)| due_at(w)).min()
}

/// Triggers every watch that is due (the job scheduler's turn). Returns how many ran.
pub async fn run_due(cluster: &Cluster, now: i64) -> usize {
    let st = cluster.state();
    if state(&st).get("stopped").and_then(Value::as_bool) == Some(true) {
        return 0;
    }
    let mut due: Vec<(String, i64)> = Vec::new();
    let mut new: Vec<String> = Vec::new();
    for (id, w) in st.metadata.xpack.iter(WATCHES) {
        match due_at(w) {
            Some(_) if unscheduled(w) => new.push(id.clone()),
            Some(t) if t <= now => due.push((id.clone(), t)),
            _ => {}
        }
    }
    drop(st);
    for id in &new {
        schedule(cluster, id, now).await;
    }
    for (id, scheduled) in &due {
        trigger(cluster, id, *scheduled).await;
    }
    due.len()
}

/// One scheduled execution of a watch: its status and next time stored, its record written.
async fn trigger(cluster: &Cluster, id: &str, scheduled_ms: i64) {
    let started = std::time::Instant::now();
    let Some(w) = cluster.state().metadata.xpack.get(WATCHES, id).cloned() else { return };
    let watch = w.get("watch").cloned().unwrap_or(Value::Null);
    let version = w.get("_version").and_then(Value::as_i64);
    let user = w.pointer(&["_roll", "user"]).cloned();
    let owner = user.as_ref().and_then(internal::principal_of);
    let execution = now_instant();
    let now = execution.millis();
    let record_id = format!("{id}_{}-{}", uuid_like(), crate::time::java_instant_string_nanos(execution));
    let run = exec::Run {
        cluster,
        watch_id: id.to_string(),
        record_id: record_id.clone(),
        execution,
        triggered_ms: now,
        scheduled_ms,
        metadata: watch.get("metadata").cloned(),
        modes: Map::new(),
        throttle_ms: watch.get("throttle_period_in_millis").and_then(Value::as_i64).unwrap_or(DEFAULT_THROTTLE_MS),
    };
    let out = internal::as_principal(owner, run.execute(&watch, stored_status(&w), None, false)).await;
    let rec = record(&run, &out, RecordOf {
        watch: &watch,
        status_version: version.map_or(Value::from(1), Value::from),
        trigger_event: obj! { "type" => "schedule", "triggered_time" => fmt_ms(now), "schedule" => obj! { "scheduled_time" => fmt_ms(scheduled_ms) } },
        user: user.and_then(|u| u.get("username").map(Value::to_es_string)),
        timestamp: crate::time::java_instant_string_nanos(execution),
        started,
        debug: false,
    });
    let activated = w.pointer(&["_roll", "activated"]).and_then(Value::as_i64).unwrap_or(scheduled_ms);
    let next = next_run(&watch, activated, now.max(scheduled_ms));
    // A store refusing the write (a cluster unloading) leaves the watch due: the scheduler
    // runs it again.
    if save_status(cluster, id, version, out.status.clone(), Some((scheduled_ms, next)), now).await.is_ok() {
        write_history(cluster, &record_id, now, &rec).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A watch stored before watches were scheduled is given its first fire time from now by
    /// the scheduler's next pass, without running.
    #[tokio::test]
    async fn watches_stored_before_scheduling_get_a_fire_time() {
        let c = Cluster::in_memory("w").await;
        let doc = obj! {
            "watch" => obj! { "trigger" => obj! { "schedule" => obj! { "interval" => "1h" } }, "input" => obj! { "simple" => Map::new() }, "actions" => Map::new() },
            "status" => obj! { "state" => obj! { "active" => true } },
            "_version" => 1,
        };
        c.update(move |st| store::put(st, WATCHES, "old", doc, 0).map(|_| ())).await.unwrap();
        assert_eq!(next_due(&c.state()), Some(0));
        let now = crate::time::now_millis();
        assert_eq!(run_due(&c, now).await, 0, "scheduled, not run");
        let w = c.state().metadata.xpack.get(WATCHES, "old").cloned().unwrap();
        assert_eq!(w.pointer(&["_roll", "next_run"]).and_then(Value::as_i64), Some(now + 3_600_000));
        assert_eq!(next_due(&c.state()), Some(now + 3_600_000));
        assert_eq!(w.get("_version").and_then(Value::as_i64), Some(1));
    }

    /// With a sealing key: a sealed `Authorization` header is opened only where the watch
    /// stored it (before rendering, so its templates still render), never when a value merely
    /// renders to sealed text; nobody can store a sealed value of their own; and an email
    /// attachment's password is sealed and printed redacted like a webhook's.
    #[tokio::test]
    async fn sealed_secrets_open_only_where_stored() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&buf).to_ascii_lowercase());
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            }
        });
        let mut cfg = crate::cluster::ClusterConfig::new("w");
        cfg.sealer = crate::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-k");
        cfg.node_settings.push((http::ALLOW_PRIVATE.into(), "true".into()));
        let c = Cluster::open(cfg, std::sync::Arc::new(crate::cluster::MemoryStore::new())).await.unwrap();
        let call = |method: &str, uri: &str, body: String| crate::rest::handle(&c, crate::rest::EsRequest::json(method, uri, body));
        let watch = |auth: &str, metadata: &str| {
            format!(
                r#"{{"trigger":{{"schedule":{{"interval":"1h"}}}},"input":{{"simple":{{}}}},"metadata":{metadata},"actions":{{"hook":{{"webhook":{{"scheme":"http","host":"127.0.0.1","port":{port},"method":"post","path":"/x","headers":{{"Authorization":"{auth}"}},"body":"b"}}}}}}}}"#
            )
        };
        assert_eq!(call("PUT", "/_watcher/watch/w1?active=false", watch("Bearer tok-secret", "{}")).await.status, 201);
        let sealed = c.state().metadata.xpack.get(WATCHES, "w1").and_then(|w| w.pointer(&["watch", "actions", "hook", "webhook", "headers", "Authorization"]).map(Value::to_es_string)).unwrap();
        assert!(crate::seal::is_sealed(&sealed));
        let mut sent = |r: crate::rest::EsResponse| {
            assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
            rx.try_recv().unwrap_or_default()
        };
        // The stored header opens where it was stored.
        let head = sent(call("POST", "/_watcher/watch/w1/_execute", "{}".into()).await);
        assert!(head.contains("authorization: bearer tok-secret"), "{head}");
        // A value that renders to the sealed text is sent as it is.
        let inline = format!(r#"{{"watch":{}}}"#, watch("{{{ctx.metadata.h}}}", &format!(r#"{{"h":"{sealed}"}}"#)));
        let head = sent(call("POST", "/_watcher/watch/_execute", inline).await);
        assert!(!head.contains("tok-secret"), "a rendered value was opened: {head}");
        // Nobody stores a sealed value of their own.
        assert_eq!(call("PUT", "/_watcher/watch/w2?active=false", watch(&sealed, "{}")).await.status, 400);
        // Templates in a sealed header still render.
        assert_eq!(call("PUT", "/_watcher/watch/w3?active=false", watch("Bearer {{ctx.metadata.t}}", r#"{"t":"abc"}"#)).await.status, 201);
        let head = sent(call("POST", "/_watcher/watch/w3/_execute", "{}".into()).await);
        assert!(head.contains("authorization: bearer abc"), "{head}");
        // An email attachment's password.
        let mail = r#"{"trigger":{"schedule":{"interval":"1h"}},"input":{"simple":{}},"actions":{"mail":{"email":{"to":"a@b.example","attachments":{"f":{"http":{"request":{"url":"http://h.example/f","auth":{"basic":{"username":"u","password":"att-pw-5"}}}}}}}}}}"#;
        assert_eq!(call("PUT", "/_watcher/watch/m?active=false", mail.into()).await.status, 201);
        assert!(!format!("{:?}", c.state().metadata.xpack).contains("att-pw-5"), "stored in the clear");
        let shown = call("GET", "/_watcher/watch/m", String::new()).await;
        assert!(!String::from_utf8_lossy(&shown.body).contains("att-pw-5"), "printed in the clear");
    }

    #[test]
    fn cron_shapes() {
        assert!(check_cron("0 0 0 1 * ? 2099").is_ok());
        assert!(check_cron("0 0/5 * * * ?").is_ok());
        assert!(check_cron("0 0 12 ? * MON-FRI").is_ok());
        let e = check_cron("bad").unwrap_err();
        assert_eq!(e.reason(), "could not parse [cron] schedule");
        // Elasticsearch 9.5.5's `Cron` messages.
        for (expr, msg) in [
            ("61 0 0 * * ?", Some("Minute and Second values must be between 0 and 59")),
            ("0 0 24 * * ?", Some("Hour values must be between 0 and 23")),
            ("0 0 0 1-40 * ?", Some("Day of month values must be between 1 and 31")),
            ("0 0 0 * 13 ?", Some("Month values must be between 1 and 12")),
            ("0 0 0 ? * 8", Some("Day-of-Week values must be between 1 and 7")),
            ("0 0 0 * * *", Some("support for specifying both a day-of-week AND a day-of-month parameter is not implemented.")),
            ("0 0/70 0 * * ?", None),
            ("0 0 0 ? * 5#3", None),
        ] {
            assert_eq!(cron_problem(expr).as_deref(), msg, "{expr}");
        }
    }
}

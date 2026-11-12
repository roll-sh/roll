//! Watcher: watches (`_watcher/watch`), `_execute`, `_ack`, `_activate`/`_deactivate`,
//! `_query/watches`, `_watcher/stats`, `_start`/`_stop` and `_watcher/settings`.
//!
//! **What is real:** watches are validated and stored with Elasticsearch's status bookkeeping
//! (versions, sequence numbers, per-action ack states and their transitions), and `_execute`
//! runs a watch for real: inputs `simple`, `search` (through the core search API), `chain` and
//! `none`; conditions `always`, `never`, `compare` and `array_compare` (with `{{ctx...}}` and
//! `<{now...}>` values); transforms `search` and `chain`; actions `index` (into roll's indices)
//! and `logging` (rendered with mustache), honouring `action_modes`, `ignore_condition`,
//! `alternative_input` and `record_execution`.
//!
//! **What is emulated:** roll has no trigger scheduler, so watches only run through `_execute`.
//! Actions that need external services (`email`, `webhook`, `slack`, `pagerduty`, `jira`, `http`
//! inputs) fail with the error Elasticsearch gives when no account is configured, unless
//! simulated. Watch history is not written to `.watcher-history-*`.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, store};

pub const WATCHES: &str = "watcher.watch";
const STATE: &str = "watcher.state";

const ACTION_TYPES: [&str; 8] = ["index", "logging", "email", "webhook", "slack", "pagerduty", "jira", "http"];

fn iso(ms: i64) -> String {
    crate::time::java_instant_string(ms)
}

fn state(st: &crate::meta::ClusterState) -> Map {
    st.metadata.xpack.get(STATE, "state").and_then(Value::as_object).cloned().unwrap_or_default()
}

/// Checks a Quartz cron expression's shape (6 or 7 fields of the allowed characters).
fn check_cron(expr: &str) -> Result<(), EsError> {
    cron_problem(expr).map_or(Ok(()), |detail| {
        Err(EsError::new(400, "parse_exception", "could not parse [cron] schedule").caused_by(
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

/// Validates a watch definition (`WatchParser`).
fn validate_watch(id: &str, w: &Map) -> EsResult<()> {
    for k in ["trigger", "input"] {
        if k == "trigger" && !w.contains_key(k) {
            return Err(EsError::new(400, "parse_exception", format!("could not parse watch [{id}]. missing required field [trigger]")));
        }
    }
    for (k, _) in w {
        if !matches!(k.as_str(), "trigger" | "input" | "condition" | "transform" | "actions" | "metadata" | "throttle_period" | "throttle_period_in_millis" | "status") {
            return Err(EsError::new(400, "parse_exception", format!("could not parse watch [{id}]. unexpected field [{k}]")));
        }
    }
    if let Some(Value::Obj(sched)) = w.get("trigger").and_then(|t| t.get("schedule")) {
        if let Some(c) = sched.get("cron") {
            let list: Vec<String> = match c {
                Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                other => vec![other.to_es_string()],
            };
            for e in list {
                check_cron(&e)?;
            }
        }
    } else {
        return Err(EsError::new(400, "parse_exception", format!("could not parse [{}] trigger for [{id}]", w.get("trigger").and_then(|t| t.as_object()).and_then(|m| m.keys().next().cloned()).unwrap_or_default())));
    }
    for (aid, a) in w.get("actions").and_then(Value::as_object).into_iter().flatten() {
        let has_type = a.as_object().is_some_and(|m| m.keys().any(|k| ACTION_TYPES.contains(&k.as_str())));
        if !has_type {
            return Err(EsError::new(400, "parse_exception", format!("could not parse action [{id}/{aid}]. unknown action type")));
        }
    }
    Ok(())
}

/// `throttle_period` (a time value) as Elasticsearch stores it: `throttle_period_in_millis`.
fn throttle_ms(m: &Map) -> EsResult<Option<i64>> {
    match (m.get("throttle_period_in_millis"), m.get("throttle_period")) {
        (Some(v), _) => Ok(v.as_i64()),
        (None, Some(v)) => Ok(Some(crate::units::TimeValue::parse(&v.to_es_string(), "throttle_period")?.as_millis())),
        _ => Ok(None),
    }
}

/// An action body as its `toXContent` writes it (defaults filled in, Elasticsearch's order).
fn normalize_action_body(ty: &str, body: &Value) -> Value {
    let Value::Obj(b) = body else { return body.clone() };
    let mut out = Map::new();
    let mut take = |k: &str, default: Option<Value>| {
        if let Some(v) = b.get(k).cloned().or(default) {
            out.insert(k.to_string(), v);
        }
    };
    match ty {
        "logging" => {
            take("category", None);
            take("level", Some(Value::from("info")));
            take("text", None);
        }
        "webhook" => {
            take("scheme", Some(Value::from("http")));
            take("host", None);
            take("port", None);
            take("method", Some(Value::from("get")));
            take("path", None);
            take("params", Some(Value::object()));
            take("headers", Some(Value::object()));
            for k in ["auth", "body", "proxy", "connection_timeout_in_millis", "read_timeout_millis", "url"] {
                take(k, None);
            }
        }
        _ => return body.clone(),
    }
    for (k, v) in b {
        if !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Obj(out)
}

/// A watch as `Watch.toXContent` stores it: `input` and `condition` default to `none` and
/// `always`, throttle periods become milliseconds, action bodies get their defaults.
fn normalize_watch(w: &Map) -> EsResult<Map> {
    let mut out = Map::new();
    if let Some(t) = w.get("trigger") {
        out.insert("trigger".into(), t.clone());
    }
    out.insert("input".into(), w.get("input").cloned().unwrap_or_else(|| obj! { "none" => obj! {} }));
    out.insert("condition".into(), w.get("condition").cloned().unwrap_or_else(|| obj! { "always" => obj! {} }));
    if let Some(t) = w.get("transform") {
        out.insert("transform".into(), t.clone());
    }
    if let Some(ms) = throttle_ms(w)? {
        out.insert("throttle_period_in_millis".into(), ms.into());
    }
    let mut actions = Map::new();
    for (id, a) in w.get("actions").and_then(Value::as_object).into_iter().flatten() {
        let Value::Obj(am) = a else {
            actions.insert(id.clone(), a.clone());
            continue;
        };
        let mut o = Map::new();
        if let Some(ms) = throttle_ms(am)? {
            o.insert("throttle_period_in_millis".into(), ms.into());
        }
        for k in ["condition", "transform", "foreach", "max_iterations"] {
            if let Some(v) = am.get(k) {
                o.insert(k.into(), v.clone());
            }
        }
        for (k, v) in am {
            if !matches!(k.as_str(), "throttle_period" | "throttle_period_in_millis" | "condition" | "transform" | "foreach" | "max_iterations") {
                o.insert(k.clone(), normalize_action_body(k, v));
            }
        }
        actions.insert(id.clone(), Value::Obj(o));
    }
    out.insert("actions".into(), Value::Obj(actions));
    if let Some(m) = w.get("metadata") {
        out.insert("metadata".into(), m.clone());
    }
    Ok(out)
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

fn initial_status(w: &Map, active: bool, now: i64) -> Value {
    let mut actions = Map::new();
    for aid in w.get("actions").and_then(Value::as_object).into_iter().flat_map(|m| m.keys()) {
        actions.insert(aid.clone(), obj! { "ack" => obj! { "timestamp" => iso(now), "state" => "awaits_successful_execution" } });
    }
    obj! { "state" => obj! { "active" => active, "timestamp" => iso(now) }, "actions" => Value::Obj(actions) }
}

fn next_seq(st: &mut crate::meta::ClusterState, now: i64) -> EsResult<i64> {
    let mut s = state(st);
    let seq = s.get("seq_no").and_then(Value::as_i64).map_or(0, |n| n + 1);
    s.insert("seq_no".into(), seq.into());
    store::put(st, STATE, "state", Value::Obj(s), now)?;
    Ok(seq)
}

pub async fn put_watch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let active = ctx.bool_param("active", true)?;
    let body = ctx.body_map()?.unwrap_or_default();
    validate_watch(&id, &body)?;
    let body = normalize_watch(&body)?;
    let now = ctx.now_ms;
    let (version, seq, created) = ctx
        .cluster
        .update(move |st| {
            let old = st.metadata.xpack.get(WATCHES, &id).cloned();
            let version = next_version(st, &id);
            set_tombstone(st, &id, None, now)?;
            let seq = next_seq(st, now)?;
            let w = obj! { "watch" => Value::Obj(body.clone()), "status" => initial_status(&body, active, now), "_version" => version, "_seq_no" => seq };
            store::put(st, WATCHES, &id, w, now)?;
            Ok((version, seq, old.is_none()))
        })
        .await?;
    let id = ctx.path_param("id").unwrap_or_default();
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "_id" => id, "_version" => version, "_seq_no" => seq, "_primary_term" => 1, "created" => created }))
}

fn status_out(w: &Value) -> Value {
    let mut s = w.get("status").and_then(Value::as_object).cloned().unwrap_or_default();
    s.insert("version".into(), w.get("_version").cloned().unwrap_or(Value::from(1)));
    Value::Obj(s)
}

pub fn get_watch(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    let st = ctx.state();
    let Some(w) = st.metadata.xpack.get(WATCHES, id) else {
        return Ok(Reply::Body(404, obj! { "found" => false, "_id" => id }));
    };
    Reply::ok(obj! {
        "found" => true, "_id" => id, "_version" => w.get("_version").cloned().unwrap_or(Value::from(1)),
        "_seq_no" => w.get("_seq_no").cloned().unwrap_or(Value::from(0)), "_primary_term" => 1,
        "status" => status_out(w), "watch" => w.get("watch").cloned().unwrap_or(Value::Null),
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

/// Applies `f` to a stored watch's status, bumping its version.
async fn modify_status(ctx: &Ctx<'_>, id: String, f: impl FnOnce(&mut Map, i64) -> EsResult<bool> + Send) -> EsResult<Reply> {
    let now = ctx.now_ms;
    let w = ctx
        .cluster
        .update(move |st| {
            let Some(Value::Obj(mut w)) = st.metadata.xpack.get(WATCHES, &id).cloned() else {
                return Err(EsError::resource_not_found(format!("Watch with id [{id}] does not exist")));
            };
            let mut status = w.get("status").and_then(Value::as_object).cloned().unwrap_or_default();
            // Nothing changed (an ack with nothing to ack): the document isn't rewritten.
            if !f(&mut status, now)? {
                return Ok(Value::Obj(w));
            }
            w.insert("status".into(), Value::Obj(status));
            let version = w.get("_version").and_then(Value::as_i64).unwrap_or(1) + 1;
            w.insert("_version".into(), version.into());
            let seq = next_seq(st, now)?;
            w.insert("_seq_no".into(), seq.into());
            let v = Value::Obj(w);
            store::put(st, WATCHES, &id, v.clone(), now)?;
            Ok(v)
        })
        .await?;
    Reply::ok(obj! { "status" => status_out(&w) })
}

pub async fn ack_watch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("watch_id").unwrap_or_default().to_string();
    let ids = ctx.list_param("action_id");
    modify_status(ctx, id, move |status, now| {
        let mut changed = false;
        if let Some(Value::Obj(actions)) = status.get_mut("actions") {
            for (aid, a) in actions.iter_mut() {
                if ids.as_ref().is_some_and(|l| !l.iter().any(|x| x == aid || x == "_all")) {
                    continue;
                }
                if let Some(Value::Obj(ack)) = a.as_object_mut().and_then(|m| m.get_mut("ack")).map(|v| v as &mut Value)
                    && ack.get("state").and_then(Value::as_str) == Some("ackable")
                {
                    ack.insert("state".into(), "acked".into());
                    ack.insert("timestamp".into(), iso(now).into());
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
    modify_status(ctx, id, move |status, now| {
        status.insert("state".into(), obj! { "active" => active, "timestamp" => iso(now) });
        Ok(true)
    })
    .await
}

pub fn query_watches(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let from = body.get("from").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
    let size = body.get("size").and_then(Value::as_i64).unwrap_or(10).max(0) as usize;
    let st = ctx.state();
    let all: Vec<Value> = st
        .metadata
        .xpack
        .iter(WATCHES)
        .map(|(id, w)| {
            let mut status = w.get("status").and_then(Value::as_object).cloned().unwrap_or_default();
            status.insert("version".into(), Value::from(-1));
            obj! { "_id" => id.clone(), "watch" => w.get("watch").cloned().unwrap_or(Value::Null), "status" => Value::Obj(status), "_seq_no" => w.get("_seq_no").cloned().unwrap_or(Value::from(0)), "_primary_term" => 1 }
        })
        .collect();
    let count = all.len();
    Reply::ok(obj! { "count" => count, "watches" => Value::Arr(all.into_iter().skip(from).take(size).collect()) })
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let s = state(&st);
    let stopped = s.get("stopped").and_then(Value::as_bool).unwrap_or(false);
    let metrics = ctx.list_param("metric").unwrap_or_default();
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
        if m == "queued_watches" || m == "_all" {
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

/// Resolves `ctx.payload.a.b` style paths in the execution model.
fn resolve<'a>(model: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = model;
    for p in path.split('.') {
        cur = match cur {
            Value::Obj(m) => m.get(p)?,
            Value::Arr(a) => a.get(p.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// A compare value: `{{ctx.path}}` references and `<{now-5m}>` date math resolve first.
fn compare_value(model: &Value, v: &Value, now: i64) -> Value {
    if let Value::Str(s) = v {
        if let Some(path) = s.strip_prefix("{{").and_then(|x| x.strip_suffix("}}")) {
            return resolve(model, path.trim()).cloned().unwrap_or(Value::Null);
        }
        if let Some(expr) = s.strip_prefix("<{").and_then(|x| x.strip_suffix("}>"))
            && let Ok(f) = crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis")
            && let Ok(i) = crate::time::parse_date_math(expr, now, false, None, &f)
        {
            return Value::from(iso(i.millis()));
        }
    }
    v.clone()
}

fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Num(_), Value::Num(_)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Value::Str(x), Value::Str(y)) => {
            let fmt = crate::time::DateFormatter::new("strict_date_optional_time").ok()?;
            match (fmt.parse_millis(x), fmt.parse_millis(y)) {
                (Ok(p), Ok(q)) => Some(p.cmp(&q)),
                _ => Some(x.cmp(y)),
            }
        }
        (Value::Num(_), Value::Str(s)) => a.as_f64()?.partial_cmp(&s.parse::<f64>().ok()?),
        (Value::Str(s), Value::Num(_)) => s.parse::<f64>().ok()?.partial_cmp(&b.as_f64()?),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
        _ => None,
    }
}

fn op_holds(op: &str, actual: &Value, expected: &Value) -> bool {
    use std::cmp::Ordering::*;
    let c = cmp_values(actual, expected);
    match op {
        "eq" => c == Some(Equal) || actual == expected,
        "not_eq" => !(c == Some(Equal) || actual == expected),
        "gt" => c == Some(Greater),
        "gte" => matches!(c, Some(Greater | Equal)),
        "lt" => c == Some(Less),
        "lte" => matches!(c, Some(Less | Equal)),
        _ => false,
    }
}

/// Evaluates a condition: `(met, result details)`.
fn eval_condition(cond: &Value, model: &Value, now: i64) -> EsResult<(bool, String, Value)> {
    let Some((ty, body)) = cond.as_object().and_then(|m| m.iter().next()) else { return Ok((true, "always".into(), Value::Null)) };
    match ty.as_str() {
        "always" => Ok((true, ty.clone(), Value::Null)),
        "never" => Ok((false, ty.clone(), Value::Null)),
        "compare" => {
            let Some((path, spec)) = body.as_object().and_then(|m| m.iter().next()) else { return Err(EsError::new(400, "parse_exception", "could not parse [compare] condition")) };
            let Some((op, expected)) = spec.as_object().and_then(|m| m.iter().next()) else { return Err(EsError::new(400, "parse_exception", "could not parse [compare] condition")) };
            let actual = resolve(model, path).cloned().unwrap_or(Value::Null);
            let expected = compare_value(model, expected, now);
            let met = op_holds(op, &actual, &expected);
            let mut resolved = Map::new();
            resolved.insert(path.clone(), actual);
            if let Some(Value::Str(s)) = spec.as_object().and_then(|m| m.values().next())
                && (s.starts_with("{{") || s.starts_with("<{"))
            {
                resolved.insert(s.clone(), expected);
            }
            Ok((met, ty.clone(), obj! { "resolved_values" => Value::Obj(resolved) }))
        }
        "array_compare" => {
            let Some((path, spec)) = body.as_object().and_then(|m| m.iter().next()) else { return Err(EsError::new(400, "parse_exception", "could not parse [array_compare] condition")) };
            let field = spec.get("path").and_then(Value::as_str).unwrap_or("");
            let Some((op, ospec)) = spec.as_object().and_then(|m| m.iter().find(|(k, _)| k.as_str() != "path")) else {
                return Err(EsError::new(400, "parse_exception", "could not parse [array_compare] condition"));
            };
            let expected = compare_value(model, ospec.get("value").unwrap_or(&Value::Null), now);
            let quantifier = ospec.get("quantifier").and_then(Value::as_str).unwrap_or("some");
            let arr = resolve(model, path).and_then(Value::as_array).cloned().unwrap_or_default();
            let checks: Vec<bool> = arr.iter().map(|e| {
                let v = if field.is_empty() { Some(e) } else { resolve(e, field) };
                v.is_some_and(|v| op_holds(op, v, &expected))
            }).collect();
            let met = if quantifier == "all" { checks.iter().all(|c| *c) } else { checks.iter().any(|c| *c) };
            let mut resolved = Map::new();
            resolved.insert(path.clone(), Value::Arr(arr));
            Ok((met, ty.clone(), obj! { "resolved_values" => Value::Obj(resolved) }))
        }
        other => Err(EsError::new(400, "parse_exception", format!("could not parse condition for watch. unknown condition type [{other}]"))),
    }
}

/// Runs an input: `(type, payload)`.
async fn run_input(ctx: &Ctx<'_>, input: &Value, model: &Value) -> EsResult<(String, Value)> {
    let Some((ty, body)) = input.as_object().and_then(|m| m.iter().next()) else { return Ok(("none".into(), Value::object())) };
    match ty.as_str() {
        "simple" => Ok((ty.clone(), body.clone())),
        "none" => Ok((ty.clone(), Value::object())),
        "search" => {
            let req = body.get("request").cloned().unwrap_or(Value::Null);
            let indices: Vec<String> = req.get("indices").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
            let path = if indices.is_empty() { "/_search".to_string() } else { format!("/{}/_search", enc(&indices.join(","))) };
            let search_body = req.get("body").cloned().unwrap_or_else(Value::object);
            let mut r = internal::call(ctx.cluster, "POST", &format!("{path}?rest_total_hits_as_int=true"), Some(&search_body)).await?;
            if let Some(Value::Arr(paths)) = body.get("extract") {
                let mut m = Map::new();
                for p in paths {
                    let p = p.to_es_string();
                    if let Some(v) = resolve(&r, &p) {
                        m.insert(p, v.clone());
                    }
                }
                r = Value::Obj(m);
            }
            Ok((ty.clone(), r))
        }
        "chain" => {
            let mut payload = Map::new();
            let mut inner_model = model.clone();
            for step in body.get("inputs").and_then(Value::as_array).into_iter().flatten() {
                if let Some((name, inp)) = step.as_object().and_then(|m| m.iter().next()) {
                    let (_, p) = Box::pin(run_input(ctx, inp, &inner_model)).await?;
                    payload.insert(name.clone(), p);
                    if let Some(Value::Obj(c)) = inner_model.as_object_mut().and_then(|m| m.get_mut("ctx")) {
                        c.insert("payload".into(), Value::Obj(payload.clone()));
                    }
                }
            }
            Ok((ty.clone(), Value::Obj(payload)))
        }
        "http" => Err(EsError::illegal_state("http inputs need external connectivity, which roll search does not provide")),
        other => Err(EsError::new(400, "parse_exception", format!("could not parse input for watch. unknown input type [{other}]"))),
    }
}

fn render_text(ctx: &Ctx, text: &str, model: &Value) -> EsResult<String> {
    let t = search_painless::mustache::Template::compile(text, &[("content_type".into(), "text/plain".into())]).map_err(crate::scripting::script_error)?;
    let _ = ctx;
    t.render_json(&crate::scripting::to_serde(model)).map_err(crate::scripting::script_error)
}

/// Runs one action: its result object.
async fn run_action(ctx: &Ctx<'_>, id: &str, action: &Value, model: &Value, mode: &str) -> Value {
    let Some((ty, body)) = action.as_object().and_then(|m| m.iter().find(|(k, _)| ACTION_TYPES.contains(&k.as_str()))) else {
        return obj! { "id" => id, "type" => "unknown", "status" => "failure", "reason" => "unknown action type" };
    };
    if mode == "skip" {
        return obj! { "id" => id, "type" => ty.clone(), "status" => "throttled", "reason" => "Watch execution is skipped" };
    }
    let simulate = mode == "simulate" || mode == "force_simulate";
    match ty.as_str() {
        "logging" => {
            let text = body.get("text").map(Value::to_es_string).unwrap_or_default();
            match render_text(ctx, &text, model) {
                Ok(t) => obj! { "id" => id, "type" => "logging", "status" => if simulate { "simulated" } else { "success" }, "logging" => obj! { "logged_text" => t } },
                Err(e) => obj! { "id" => id, "type" => "logging", "status" => "failure", "error" => e.to_value(false) },
            }
        }
        "index" => {
            let index = body.get("index").map(Value::to_es_string).unwrap_or_default();
            let doc = model.pointer(&["ctx", "payload"]).cloned().unwrap_or_else(Value::object);
            if simulate {
                return obj! { "id" => id, "type" => "index", "status" => "simulated", "index" => obj! { "request" => obj! { "index" => index, "source" => doc } } };
            }
            let mut uri = match body.get("doc_id") {
                Some(d) => format!("/{}/_doc/{}", enc(&index), enc(&d.to_es_string())),
                None => format!("/{}/_doc", enc(&index)),
            };
            let mut q = Vec::new();
            if let Some(op) = body.get("op_type") {
                q.push(format!("op_type={}", enc(&op.to_es_string())));
            }
            if let Some(r) = body.get("refresh") {
                q.push(format!("refresh={}", enc(&r.to_es_string())));
            }
            if !q.is_empty() {
                uri.push('?');
                uri.push_str(&q.join("&"));
            }
            match internal::call(ctx.cluster, if body.get("doc_id").is_some() { "PUT" } else { "POST" }, &uri, Some(&doc)).await {
                Ok(r) => obj! {
                    "id" => id, "type" => "index", "status" => "success",
                    "index" => obj! { "response" => obj! {
                        "created" => r.get("result").and_then(Value::as_str) == Some("created"),
                        "result" => r.get("result").cloned().unwrap_or(Value::Null),
                        "id" => r.get("_id").cloned().unwrap_or(Value::Null),
                        "version" => r.get("_version").cloned().unwrap_or(Value::Null),
                        "index" => r.get("_index").cloned().unwrap_or(Value::Null),
                    } },
                },
                Err(e) => obj! { "id" => id, "type" => "index", "status" => "failure", "error" => e.to_value(false) },
            }
        }
        other => {
            if simulate {
                obj! { "id" => id, "type" => other, "status" => "simulated" }
            } else {
                let e = EsError::illegal_argument(format!("no account found for name: [{other}]"));
                obj! { "id" => id, "type" => other, "status" => "failure", "reason" => e.reason().to_string() }
            }
        }
    }
}

pub async fn execute_watch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let started = std::time::Instant::now();
    let body = ctx.body_map()?.unwrap_or_default();
    let debug = ctx.bool_param("debug", false)?;
    let now = ctx.now_ms;
    let (id, watch, mut status, stored) = match ctx.path_param("id") {
        Some(id) => {
            let st = ctx.state();
            let Some(w) = st.metadata.xpack.get(WATCHES, id) else { return Err(EsError::resource_not_found(format!("Watch with id [{id}] does not exist"))) };
            (id.to_string(), w.get("watch").cloned().unwrap_or(Value::Null), status_out(w), true)
        }
        None => {
            let Some(Value::Obj(w)) = body.get("watch") else {
                return Err(EsError::action_request_validation(&["a watch execution request must either have a watch id or an inline watch source, but both are missing".into()]));
            };
            validate_watch("_inlined_", w)?;
            let mut s = initial_status(w, true, now);
            if let Value::Obj(m) = &mut s {
                m.insert("version".into(), Value::from(-1));
            }
            ("_inlined_".to_string(), Value::Obj(w.clone()), s, false)
        }
    };
    let trigger_data = body.get("trigger_data").cloned().unwrap_or(Value::Null);
    let triggered = trigger_data.get("triggered_time").map(Value::to_es_string).unwrap_or_else(|| iso(now));
    let scheduled = trigger_data.get("scheduled_time").map(Value::to_es_string).unwrap_or_else(|| triggered.clone());
    let exec_time = iso(now);
    let mut model = obj! {
        "ctx" => obj! {
            "watch_id" => id.clone(), "execution_time" => exec_time.clone(),
            "trigger" => obj! { "triggered_time" => triggered.clone(), "scheduled_time" => scheduled.clone() },
            "metadata" => watch.get("metadata").cloned().unwrap_or(Value::Null),
            "payload" => obj! {},
        },
    };
    let input = watch.get("input").cloned().unwrap_or_else(|| obj! { "none" => obj! {} });
    let (input_type, payload) = match body.get("alternative_input") {
        Some(alt) => ("simple".to_string(), alt.clone()),
        None => run_input(ctx, &input, &model).await?,
    };
    if let Some(Value::Obj(c)) = model.as_object_mut().and_then(|m| m.get_mut("ctx")) {
        c.insert("payload".into(), payload.clone());
    }
    let cond = watch.get("condition").cloned().unwrap_or_else(|| obj! { "always" => obj! {} });
    let ignore = body.get("ignore_condition").and_then(Value::as_bool).unwrap_or(false);
    let (met, cond_type, cond_detail) = if ignore { (true, "always".to_string(), Value::Null) } else { eval_condition(&cond, &model, now)? };
    let mut cond_result = obj! { "type" => cond_type.clone(), "status" => "success", "met" => met };
    if !cond_detail.is_null()
        && let Value::Obj(m) = &mut cond_result
    {
        m.insert(cond_type.clone(), cond_detail);
    }
    let modes = body.get("action_modes").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut action_results = Vec::new();
    let mut action_ok: Vec<(String, bool, Option<String>)> = Vec::new();
    if met {
        for (aid, a) in watch.get("actions").and_then(Value::as_object).into_iter().flatten() {
            let mode = modes.get(aid).or_else(|| modes.get("_all")).map(Value::to_es_string).unwrap_or_else(|| "execute".into());
            let r = run_action(ctx, aid, a, &model, &mode).await;
            let ok = r.get("status").and_then(Value::as_str) == Some("success");
            let reason = r.get("reason").map(Value::to_es_string).or_else(|| (!ok).then(String::new));
            action_ok.push((aid.clone(), ok, reason));
            action_results.push(r);
        }
    }
    // Status bookkeeping.
    if let Value::Obj(s) = &mut status {
        s.insert("last_checked".into(), exec_time.clone().into());
        if met {
            s.insert("last_met_condition".into(), exec_time.clone().into());
        }
        if let Some(Value::Obj(actions)) = s.get_mut("actions") {
            for (aid, ok, reason) in &action_ok {
                if let Some(Value::Obj(a)) = actions.get_mut(aid) {
                    let mut last = obj! { "timestamp" => exec_time.clone(), "successful" => *ok };
                    if let (Some(r), Value::Obj(m)) = (reason, &mut last)
                        && !ok
                    {
                        m.insert("reason".into(), r.clone().into());
                    }
                    a.insert("last_execution".into(), last);
                    if *ok {
                        a.insert("last_successful_execution".into(), obj! { "timestamp" => exec_time.clone(), "successful" => true });
                        if let Some(Value::Obj(ack)) = a.get_mut("ack")
                            && ack.get("state").and_then(Value::as_str) == Some("awaits_successful_execution")
                        {
                            ack.insert("state".into(), "ackable".into());
                            ack.insert("timestamp".into(), exec_time.clone().into());
                        }
                    }
                }
            }
        }
        s.insert("execution_state".into(), if met { "executed" } else { "execution_not_needed" }.into());
        // `version` last, as Elasticsearch prints the status.
        if let Some(v) = s.shift_remove("version") {
            s.insert("version".into(), v);
        }
    }
    let record = body.get("record_execution").and_then(Value::as_bool).unwrap_or(false);
    if stored && record {
        let sid = id.clone();
        let new_status = status.clone();
        ctx.cluster
            .update(move |st| {
                if let Some(Value::Obj(mut w)) = st.metadata.xpack.get(WATCHES, &sid).cloned() {
                    let mut s = new_status.as_object().cloned().unwrap_or_default();
                    s.shift_remove("version");
                    w.insert("status".into(), Value::Obj(s));
                    store::put(st, WATCHES, &sid, Value::Obj(w), now)?;
                }
                Ok(())
            })
            .await?;
    }
    let duration = (started.elapsed().as_millis() as i64).max(1);
    let user = crate::xpack::security::principal(ctx).username.clone();
    let mut wr = Map::new();
    wr.insert("watch_id".into(), id.clone().into());
    wr.insert("node".into(), ctx.cluster.node.id.clone().into());
    wr.insert("state".into(), if met { "executed" } else { "execution_not_needed" }.into());
    wr.insert("@timestamp".into(), triggered.clone().into());
    wr.insert("user".into(), user.into());
    wr.insert("status".into(), status);
    wr.insert("trigger_event".into(), obj! { "type" => "manual", "triggered_time" => triggered.clone(), "manual" => obj! { "schedule" => obj! { "scheduled_time" => scheduled } } });
    wr.insert("input".into(), input);
    wr.insert("condition".into(), cond);
    let mut result = Map::new();
    result.insert("execution_time".into(), exec_time.clone().into());
    result.insert("execution_duration".into(), duration.into());
    result.insert("input".into(), obj! { "type" => input_type, "status" => "success", "payload" => payload });
    result.insert("condition".into(), cond_result);
    result.insert("actions".into(), Value::Arr(action_results));
    wr.insert("result".into(), Value::Obj(result));
    if debug {
        wr.insert("vars".into(), obj! {});
    }
    wr.insert("messages".into(), Value::Arr(vec![]));
    let record_id = format!("{id}_{}-{}", uuid_like(), crate::xpack::entsearch::now_nanos_string());
    Reply::ok(obj! { "_id" => record_id, "watch_record" => Value::Obj(wr) })
}

fn uuid_like() -> String {
    let b: [u8; 16] = rand::random();
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-4{}-{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn compare_conditions() {
        let model = obj! { "ctx" => obj! { "payload" => obj! { "n" => 5, "m" => 3 } } };
        let c = obj! { "compare" => obj! { "ctx.payload.n" => obj! { "gt" => 3 } } };
        assert!(eval_condition(&c, &model, 0).unwrap().0);
        let c = obj! { "compare" => obj! { "ctx.payload.n" => obj! { "lt" => "{{ctx.payload.m}}" } } };
        assert!(!eval_condition(&c, &model, 0).unwrap().0);
        let model = obj! { "ctx" => obj! { "payload" => obj! { "a" => crate::arr![obj! { "v" => 1 }, obj! { "v" => 9 }] } } };
        let c = obj! { "array_compare" => obj! { "ctx.payload.a" => obj! { "path" => "v", "gte" => obj! { "value" => 5 } } } };
        assert!(eval_condition(&c, &model, 0).unwrap().0);
    }
}

//! Parsing a watch (`WatchParser`): validation, and the form `.watches` stores it in (inputs,
//! conditions, transforms and actions with their defaults, scripts compiled to check them,
//! secrets kept but printed redacted).

use super::{http, schedule};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

pub const ACTION_TYPES: [&str; 8] = ["index", "logging", "email", "webhook", "slack", "pagerduty", "jira", "http"];

pub const IGNORE_THROTTLED: &str = "Deprecated field [ignore_throttled] used, this field is unused and will be removed entirely";

fn parse_err(msg: impl Into<String>) -> EsError {
    EsError::new(400, "parse_exception", msg)
}

/// A JSON object as a `HashMap` parsed from it iterates (every level): how Elasticsearch prints
/// payloads and `simple` inputs.
pub fn java_map(v: &Value) -> Value {
    match v {
        Value::Obj(m) => Value::Obj(crate::json::java_hash_map(m.iter().map(|(k, v)| (k.clone(), java_map(v))).collect(), 16)),
        Value::Arr(a) => Value::Arr(a.iter().map(java_map).collect()),
        other => other.clone(),
    }
}

/// `throttle_period` (a time value) as Elasticsearch stores it: `throttle_period_in_millis`.
pub fn throttle_ms(m: &Map) -> EsResult<Option<i64>> {
    match (m.get("throttle_period_in_millis"), m.get("throttle_period")) {
        (Some(v), _) => Ok(v.as_i64()),
        (None, Some(v)) => Ok(Some(crate::units::TimeValue::parse(&v.to_es_string(), "throttle_period")?.as_millis())),
        _ => Ok(None),
    }
}

/// A script as `Script.toXContent` writes it: `{source, lang}` (a bare string is painless
/// source) or `{id}`, with `options` and `params` when set.
pub fn normalize_script(v: &Value) -> Value {
    match v {
        Value::Obj(m) => {
            let mut out = Map::new();
            if let Some(id) = m.get("id").or_else(|| m.get("stored")) {
                out.insert("id".into(), id.clone());
            } else if let Some(s) = m.get("source").or_else(|| m.get("inline")) {
                out.insert("source".into(), s.clone());
                out.insert("lang".into(), m.get("lang").cloned().unwrap_or_else(|| "painless".into()));
            }
            if let Some(Value::Obj(o)) = m.get("options")
                && !o.is_empty()
            {
                out.insert("options".into(), Value::Obj(o.clone()));
            }
            if let Some(Value::Obj(p)) = m.get("params")
                && !p.is_empty()
            {
                out.insert("params".into(), Value::Obj(p.clone()));
            }
            Value::Obj(out)
        }
        other => obj! { "source" => other.to_es_string(), "lang" => "painless" },
    }
}

/// Compiles a watch script in `context`: the bound script or its compile error.
pub fn compile(cluster: &Cluster, script: &Value, context: search_painless::Context) -> EsResult<crate::scripting::Bound> {
    let mut p = crate::query::XParser::from_value(script);
    p.next_token();
    let s = crate::query::script::Script::parse(&mut p)?;
    cluster.scripts.compile(&cluster.state(), &s, context)
}

/// A search request of a `search` input or transform (`WatcherSearchTemplateRequest`).
fn normalize_search_request(r: &Value) -> EsResult<Map> {
    let Value::Obj(r) = r else { return Err(parse_err("could not parse watcher search request. expected an object")) };
    let mut out = Map::new();
    out.insert("search_type".into(), r.get("search_type").map(|s| s.to_es_string().to_ascii_lowercase()).unwrap_or_else(|| "query_then_fetch".into()).into());
    let indices: Vec<Value> = match r.get("indices") {
        None => Vec::new(),
        Some(Value::Arr(a)) => a.iter().map(|v| Value::from(v.to_es_string())).collect(),
        Some(v) => v.to_es_string().split(',').map(|s| Value::from(s.trim())).collect(),
    };
    out.insert("indices".into(), Value::Arr(indices));
    out.insert("rest_total_hits_as_int".into(), r.get("rest_total_hits_as_int").and_then(Value::as_bool).unwrap_or(true).into());
    if let Some(b) = r.get("body") {
        out.insert("body".into(), b.clone());
    }
    if let Some(t) = r.get("template") {
        let mut t = match normalize_script(t) {
            Value::Obj(m) => m,
            _ => Map::new(),
        };
        if t.contains_key("source") && t.get("lang").and_then(Value::as_str).is_none_or(|l| l == "painless") {
            t.insert("lang".into(), "mustache".into());
        }
        out.insert("template".into(), Value::Obj(t));
    }
    if let Some(Value::Obj(o)) = r.get("indices_options") {
        let flag = |k: &str, d: bool| o.get(k).and_then(|v| v.as_bool().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(d);
        let expand: Vec<Value> = match o.get("expand_wildcards") {
            None => vec!["open".into()],
            Some(Value::Arr(a)) => a.iter().map(|v| Value::from(v.to_es_string())).collect(),
            Some(v) => v.to_es_string().split(',').map(|s| Value::from(s.trim())).collect(),
        };
        out.insert(
            "indices_options".into(),
            obj! {
                "ignore_unavailable" => flag("ignore_unavailable", true), "expand_wildcards" => Value::Arr(expand),
                "allow_no_indices" => flag("allow_no_indices", true), "ignore_throttled" => flag("ignore_throttled", false),
            },
        );
    }
    Ok(out)
}

fn normalize_search(v: &Value) -> EsResult<Value> {
    let mut out = Map::new();
    out.insert("request".into(), Value::Obj(normalize_search_request(v.get("request").unwrap_or(&Value::Null))?));
    for k in ["extract", "timeout_in_millis", "dynamic_name_timezone"] {
        if let Some(x) = v.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    if let Some(t) = v.get("timeout")
        && !out.contains_key("timeout_in_millis")
    {
        out.insert("timeout_in_millis".into(), crate::units::TimeValue::parse(&t.to_es_string(), "timeout")?.as_millis().into());
    }
    Ok(Value::Obj(out))
}

/// An input: `{type: body}`.
pub fn normalize_input(cluster: &Cluster, watch_id: &str, input: &Value, previous: Option<&Value>) -> EsResult<Value> {
    let Some((ty, body)) = input.as_object().and_then(|m| m.iter().next()) else { return Ok(obj! { "none" => obj! {} }) };
    let body = match ty.as_str() {
        "none" => obj! {},
        "simple" => java_map(body),
        "search" => normalize_search(body)?,
        "http" => {
            let wrap = |e: EsError| parse_err(format!("could not parse [http] input for watch [{watch_id}]. failed to parse http request template")).caused_by(e);
            let prev = previous.and_then(|p| p.pointer(&["http", "request"]));
            let req = http::normalize(body.get("request").unwrap_or(&Value::Null), prev).map_err(wrap)?;
            let mut out = Map::new();
            out.insert("request".into(), Value::Obj(req));
            for k in ["extract", "response_content_type"] {
                if let Some(x) = body.get(k) {
                    out.insert(k.into(), x.clone());
                }
            }
            Value::Obj(out)
        }
        "chain" => {
            let mut inputs = Vec::new();
            for (i, step) in body.get("inputs").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let Some((name, inp)) = step.as_object().and_then(|m| m.iter().next()) else { continue };
                let prev = previous.and_then(|p| p.pointer(&["chain", "inputs"])).and_then(Value::as_array).and_then(|a| a.get(i)).and_then(|s| s.get(name));
                inputs.push(obj! { name.clone() => normalize_input(cluster, watch_id, inp, prev)? });
            }
            obj! { "inputs" => Value::Arr(inputs) }
        }
        "transform" => normalize_transform(cluster, body)?,
        other => return Err(parse_err(format!("could not parse input for watch [{watch_id}]. unknown input type [{other}]"))),
    };
    Ok(obj! { ty.clone() => body })
}

/// A condition; scripts are compiled, failing the put as Elasticsearch does.
pub fn normalize_condition(cluster: &Cluster, watch_id: &str, cond: &Value) -> EsResult<Value> {
    let Some((ty, body)) = cond.as_object().and_then(|m| m.iter().next()) else { return Ok(obj! { "always" => obj! {} }) };
    match ty.as_str() {
        "always" | "never" | "compare" | "array_compare" => Ok(cond.clone()),
        "script" => {
            let s = normalize_script(body);
            compile(cluster, &s, search_painless::Context::WatcherCondition)?;
            Ok(obj! { "script" => s })
        }
        other => Err(parse_err(format!("could not parse condition for watch [{watch_id}]. unknown condition type [{other}]"))),
    }
}

/// A transform (`search`, `script` or `chain`).
pub fn normalize_transform(cluster: &Cluster, t: &Value) -> EsResult<Value> {
    let Some((ty, body)) = t.as_object().and_then(|m| m.iter().next()) else { return Err(parse_err("could not parse transform. expected an object")) };
    let body = match ty.as_str() {
        "search" => normalize_search(body)?,
        "script" => {
            let s = normalize_script(body);
            compile(cluster, &s, search_painless::Context::WatcherTransform)?;
            s
        }
        "chain" => {
            let list = match body {
                Value::Arr(a) => a.clone(),
                other => other.get("transforms").and_then(Value::as_array).cloned().unwrap_or_default(),
            };
            Value::Arr(list.iter().map(|x| normalize_transform(cluster, x)).collect::<EsResult<_>>()?)
        }
        other => return Err(parse_err(format!("could not parse transform. unknown transform type [{other}]"))),
    };
    Ok(obj! { ty.clone() => body })
}

/// An action body as its `toXContent` writes it (defaults filled in, Elasticsearch's order).
fn normalize_action_body(ty: &str, body: &Value, previous: Option<&Value>, watch_id: &str, action_id: &str) -> EsResult<Value> {
    let Value::Obj(b) = body else { return Ok(body.clone()) };
    match ty {
        "logging" => {
            let mut out = Map::new();
            for (k, d) in [("category", None), ("level", Some(Value::from("info"))), ("text", None)] {
                if let Some(v) = b.get(k).cloned().or(d) {
                    out.insert(k.into(), v);
                }
            }
            for (k, v) in b {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
            Ok(Value::Obj(out))
        }
        "webhook" => {
            let wrap = |e: EsError| parse_err(format!("could not parse [webhook] action [{watch_id}/{action_id}]. failed to parse http request template")).caused_by(e);
            http::normalize(body, previous).map(Value::Obj).map_err(wrap)
        }
        "email" => {
            for (aid, a) in b.get("attachments").and_then(Value::as_object).into_iter().flatten() {
                if let Some(req) = a.pointer(&["http", "request"]) {
                    http::normalize(req, None).map_err(|e| parse_err(format!("could not parse email attachment [{aid}]")).caused_by(e))?;
                }
            }
            Ok(body.clone())
        }
        _ => Ok(body.clone()),
    }
}

/// Parses and validates a watch: the form it is stored in. `previous`: the stored watch whose
/// redacted secrets a redacted update keeps (`if_seq_no` given).
pub fn normalize_watch(cluster: &Cluster, id: &str, w: &Map, previous: Option<&Value>) -> EsResult<Map> {
    for k in w.keys() {
        if !matches!(k.as_str(), "trigger" | "input" | "condition" | "transform" | "actions" | "metadata" | "throttle_period" | "throttle_period_in_millis" | "status") {
            return Err(parse_err(format!("could not parse watch [{id}]. unexpected field [{k}]")));
        }
    }
    schedule::parse(id, w.get("trigger"))?;
    let mut out = Map::new();
    out.insert("trigger".into(), w.get("trigger").cloned().unwrap_or(Value::Null));
    let input = match w.get("input") {
        Some(i) => normalize_input(cluster, id, i, previous.and_then(|p| p.get("input")))?,
        None => obj! { "none" => obj! {} },
    };
    out.insert("input".into(), input);
    let cond = match w.get("condition") {
        Some(c) => normalize_condition(cluster, id, c)?,
        None => obj! { "always" => obj! {} },
    };
    out.insert("condition".into(), cond);
    if let Some(t) = w.get("transform") {
        out.insert("transform".into(), normalize_transform(cluster, t)?);
    }
    if let Some(ms) = throttle_ms(w)? {
        out.insert("throttle_period_in_millis".into(), ms.into());
    }
    let mut actions = Map::new();
    for (aid, a) in w.get("actions").and_then(Value::as_object).into_iter().flatten() {
        let Value::Obj(am) = a else { return Err(parse_err(format!("could not parse action [{id}/{aid}]. unknown action type"))) };
        if !am.keys().any(|k| ACTION_TYPES.contains(&k.as_str())) {
            return Err(parse_err(format!("could not parse action [{id}/{aid}]. unknown action type")));
        }
        let mut o = Map::new();
        if let Some(ms) = throttle_ms(am)? {
            o.insert("throttle_period_in_millis".into(), ms.into());
        }
        if let Some(c) = am.get("condition") {
            o.insert("condition".into(), normalize_condition(cluster, id, c)?);
        }
        if let Some(t) = am.get("transform") {
            o.insert("transform".into(), normalize_transform(cluster, t)?);
        }
        for k in ["foreach", "max_iterations"] {
            if let Some(v) = am.get(k) {
                o.insert(k.into(), v.clone());
            }
        }
        for (k, v) in am {
            if ACTION_TYPES.contains(&k.as_str()) {
                let prev = previous.and_then(|p| p.pointer(&["actions", aid.as_str(), k.as_str()]));
                o.insert(k.clone(), normalize_action_body(k, v, prev, id, aid)?);
            } else if !matches!(k.as_str(), "throttle_period" | "throttle_period_in_millis" | "condition" | "transform" | "foreach" | "max_iterations") {
                o.insert(k.clone(), v.clone());
            }
        }
        actions.insert(aid.clone(), Value::Obj(o));
    }
    out.insert("actions".into(), Value::Obj(actions));
    if let Some(m) = w.get("metadata") {
        out.insert("metadata".into(), m.clone());
    }
    Ok(out)
}

/// An input as it is printed (`http` requests redacted).
pub fn redacted_input(input: &Value) -> Value {
    let Some((ty, body)) = input.as_object().and_then(|m| m.iter().next()) else { return input.clone() };
    match ty.as_str() {
        "http" => {
            let mut b = body.clone();
            if let Value::Obj(m) = &mut b
                && let Some(r) = m.get("request").map(http::redact)
            {
                m.insert("request".into(), r);
            }
            obj! { "http" => b }
        }
        "chain" => {
            let inputs: Vec<Value> = body
                .get("inputs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|s| match s.as_object().and_then(|m| m.iter().next()) {
                    Some((n, i)) => obj! { n.clone() => redacted_input(i) },
                    None => s.clone(),
                })
                .collect();
            obj! { "chain" => obj! { "inputs" => Value::Arr(inputs) } }
        }
        _ => input.clone(),
    }
}

/// A stored watch as `GET _watcher/watch` prints it (secrets redacted).
pub fn redacted_watch(w: &Value) -> Value {
    let mut w = w.clone();
    if let Value::Obj(m) = &mut w {
        if let Some(i) = m.get("input").map(redacted_input) {
            m.insert("input".into(), i);
        }
        if let Some(Value::Obj(actions)) = m.get_mut("actions") {
            for a in actions.values_mut() {
                if let Value::Obj(am) = a
                    && let Some(wh) = am.get("webhook").map(http::redact)
                {
                    am.insert("webhook".into(), wh);
                }
            }
        }
    }
    w
}

/// Whether parsing the watch warns that `ignore_throttled` is deprecated (a search request
/// with `indices_options`, which are always stored with it).
pub fn uses_ignore_throttled(w: &Value) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Obj(m) => m.iter().any(|(k, v)| (k == "indices_options" && v.get("ignore_throttled").is_some()) || walk(v)),
            Value::Arr(a) => a.iter().any(walk),
            _ => false,
        }
    }
    ["input", "transform", "actions"].iter().any(|k| w.get(k).is_some_and(walk))
}

//! Running a watch (`ExecutionService.executeInner`): the input, the condition, the watch's
//! transform, then each action with its throttling, condition, transform and `foreach`; the
//! watch's status follows every step, and the outcome is a watch record as Elasticsearch writes
//! it to `.watcher-history-*`.

use super::{http, parse};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::{enc, internal};
use search_painless::Context;

/// A date as watch records print it (`strict_date_optional_time`, always milliseconds).
pub fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(|| ms.to_string(), |d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// `ZonedDateTime.toString()` in UTC (what templates print): seconds only when not zero.
fn zoned(i: crate::time::Instant) -> String {
    let s = crate::time::java_instant_string_nanos(i);
    if i.nanos == 0 && i.secs.rem_euclid(60) == 0 { s.replacen(":00Z", "Z", 1) } else { s }
}

/// Parses a date of `trigger_data` (a date string or epoch millis).
pub fn parse_date(v: &Value) -> EsResult<i64> {
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    let s = v.to_es_string();
    let f = crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis").map_err(EsError::illegal_argument)?;
    f.parse_millis(&s).map_err(|_| EsError::new(400, "parse_exception", format!("could not parse date/time. expected date field [{s}] to be either a number or a DateMathParser string but found [{s}] instead")))
}

/// One execution of a watch.
pub struct Run<'a> {
    pub cluster: &'a Cluster,
    pub watch_id: String,
    pub record_id: String,
    pub execution: crate::time::Instant,
    pub triggered_ms: i64,
    pub scheduled_ms: i64,
    pub metadata: Option<Value>,
    /// Action execution modes by action id (`_all` for every action).
    pub modes: Map,
    /// The watch's throttle period (default `xpack.watcher.execution.default_throttle_period`).
    pub throttle_ms: i64,
}

/// What an execution produced.
pub struct Outcome {
    pub state: &'static str,
    /// The watch's status afterwards.
    pub status: Map,
    pub input: Value,
    pub condition: Option<Value>,
    pub transform: Option<Value>,
    pub actions: Vec<(String, Value)>,
    pub messages: Vec<String>,
    pub exception: Option<Value>,
}

/// A step's result: its type, outcome and type-specific fields.
struct StepResult {
    ty: String,
    payload: Option<Value>,
    error: Option<EsError>,
    reason: Option<String>,
    extra: Vec<(String, Value)>,
}

impl StepResult {
    fn ok(ty: &str, payload: Value) -> StepResult {
        StepResult { ty: ty.into(), payload: Some(payload), error: None, reason: None, extra: Vec::new() }
    }

    fn failed(ty: &str, e: EsError) -> StepResult {
        StepResult { ty: ty.into(), payload: None, error: Some(e), reason: None, extra: Vec::new() }
    }

    fn with(mut self, k: &str, v: Value) -> StepResult {
        self.extra.push((k.into(), v));
        self
    }

    fn to_value(&self, with_reason: bool) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), self.ty.clone().into());
        match (&self.payload, &self.error) {
            (Some(p), _) => {
                m.insert("status".into(), "success".into());
                m.insert("payload".into(), p.clone());
            }
            (None, e) => {
                m.insert("status".into(), "failure".into());
                if let Some(r) = self.reason.clone().or_else(|| with_reason.then(|| e.as_ref().map(|e| e.reason().to_string())).flatten()) {
                    m.insert("reason".into(), r.into());
                }
                if let Some(e) = e {
                    m.insert("error".into(), e.to_value_with_root(false));
                }
            }
        }
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        Value::Obj(m)
    }
}

fn exception(msg: impl Into<String>) -> EsError {
    EsError::new(500, "exception", msg)
}

fn painless_value(v: &Value) -> search_painless::Value {
    search_painless::Value::from_json_map(&crate::scripting::to_serde(v))
}

/// Resolves `a.b.0.c` in a JSON value (`ObjectPath.eval`).
pub fn resolve<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for p in path.split('.') {
        cur = match cur {
            Value::Obj(m) => m.get(p)?,
            Value::Arr(a) => a.get(p.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Keeps only the `extract` paths of a payload, nested as they were
/// (`XContentFilterKeysUtils.filterMapOrdered`).
fn extract(payload: &Value, keys: &[Value]) -> Value {
    let mut out = Map::new();
    for k in keys {
        let k = k.to_es_string();
        let Some(v) = resolve(payload, &k) else { continue };
        let parts: Vec<&str> = k.split('.').collect();
        let mut cur = &mut out;
        for (i, p) in parts.iter().enumerate() {
            if i + 1 == parts.len() {
                cur.insert(p.to_string(), v.clone());
            } else {
                let e = cur.entry(p.to_string()).or_insert_with(Value::object);
                if !matches!(e, Value::Obj(_)) {
                    *e = Value::object();
                }
                cur = e.as_object_mut().expect("object");
            }
        }
    }
    Value::Obj(out)
}

impl Run<'_> {
    /// The `ctx` scripts and templates see, with `payload`.
    fn ctx(&self, payload: &Value) -> Value {
        let t = |ms: i64| zoned(crate::time::Instant::from_millis(ms));
        obj! {
            "id" => self.record_id.clone(), "watch_id" => self.watch_id.clone(),
            "execution_time" => zoned(self.execution),
            "trigger" => obj! { "triggered_time" => t(self.triggered_ms), "scheduled_time" => t(self.scheduled_ms) },
            "metadata" => self.metadata.clone().unwrap_or(Value::Null), "vars" => obj! {}, "payload" => payload.clone(),
        }
    }

    fn model(&self, payload: &Value) -> Value {
        obj! { "ctx" => self.ctx(payload) }
    }

    fn execution_ms(&self) -> i64 {
        self.execution.millis()
    }

    fn mode(&self, action: &str) -> String {
        self.modes.get(action).or_else(|| self.modes.get("_all")).map(Value::to_es_string).unwrap_or_else(|| "execute".into())
    }

    /// Renders a text template (a string, or `{source|id, params}`): the text, and whether it
    /// is JSON (a template whose source is an object).
    pub fn render(&self, tmpl: &Value, model: &Value) -> EsResult<(String, bool)> {
        let engine = &self.cluster.scripts.engine;
        let merged = |params: Option<&Value>| -> Value {
            let mut m = params.and_then(Value::as_object).cloned().unwrap_or_default();
            for (k, v) in model.as_object().into_iter().flatten() {
                m.insert(k.clone(), v.clone());
            }
            Value::Obj(m)
        };
        let mustache = |src: &str, json: bool, params: &Value| -> EsResult<String> {
            let ct = if json { "application/json" } else { "text/plain" };
            let t = engine.compile_template(src, Context::XpackTemplate, &[("content_type".into(), ct.into())]).map_err(crate::scripting::script_error)?;
            t.render_json(&crate::scripting::to_serde(params)).map_err(crate::scripting::script_error)
        };
        match tmpl {
            Value::Str(s) if !s.contains("{{") => Ok((s.clone(), false)),
            Value::Str(s) => Ok((mustache(s, false, model)?, false)),
            Value::Obj(m) if m.contains_key("id") => {
                let script = obj! { "id" => m.get("id").cloned().unwrap_or(Value::Null) };
                let bound = parse::compile(self.cluster, &script, Context::XpackTemplate)?;
                let params = merged(m.get("params"));
                match &bound.compiled {
                    search_painless::Compiled::Mustache(t) => Ok((t.render_json(&crate::scripting::to_serde(&params)).map_err(crate::scripting::script_error)?, false)),
                    search_painless::Compiled::Painless(sc) => {
                        let mut inst = sc.instance();
                        inst.set_params(&crate::scripting::to_serde(&params));
                        let v = inst.execute(&mut search_painless::NoHost).map_err(crate::scripting::script_error)?;
                        Ok((v.to_java_string(), false))
                    }
                    _ => Err(EsError::illegal_argument("only mustache and painless scripts render watch templates")),
                }
            }
            Value::Obj(m) => {
                let params = merged(m.get("params"));
                match m.get("source").or_else(|| m.get("inline")) {
                    Some(Value::Str(s)) => Ok((mustache(s, false, &params)?, false)),
                    Some(other) => Ok((mustache(&other.to_json(), true, &params)?, true)),
                    None => Ok((String::new(), false)),
                }
            }
            Value::Null => Ok((String::new(), false)),
            other => Ok((other.to_es_string(), false)),
        }
    }

    /// Runs a painless script with `ctx` (for `payload`) and its params.
    fn run_script(&self, script: &Value, context: Context, payload: &Value) -> EsResult<search_painless::Value> {
        let bound = parse::compile(self.cluster, script, context)?;
        let Some(sc) = bound.painless() else { return Err(EsError::illegal_argument("only painless scripts run in watches")) };
        let mut inst = sc.instance();
        inst.set_params(&bound.params);
        inst.set_var("ctx", painless_value(&self.ctx(payload)));
        inst.execute(&mut search_painless::NoHost).map_err(crate::scripting::script_error)
    }

    // --- search -----------------------------------------------------------------------------------

    /// Runs a stored search request: the rendered request and the response as a payload.
    async fn search(&self, req: &Value, payload: &Value) -> (Value, EsResult<Value>) {
        let mut shown = req.as_object().cloned().unwrap_or_default();
        let model = self.model(payload);
        let body = match (req.get("template"), req.get("body")) {
            (Some(t), _) => self.render_search_template(t, &model),
            (None, Some(b)) => self.render(&obj! { "source" => b.clone() }, &model).and_then(|(s, _)| Ok(crate::json::parse(s.as_bytes())?.unwrap_or_else(Value::object))),
            (None, None) => Ok(Value::object()),
        };
        let body = match body {
            Ok(b) => b,
            Err(e) => return (Value::Obj(shown), Err(e)),
        };
        // The rendered body, before a template it came from.
        let template = shown.shift_remove("template");
        let opts = shown.shift_remove("indices_options");
        shown.insert("body".into(), body.clone());
        if let Some(t) = template {
            shown.insert("template".into(), t);
        }
        if let Some(o) = opts.clone() {
            shown.insert("indices_options".into(), o);
        }
        let indices: Vec<String> = req.get("indices").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
        let path = if indices.is_empty() { "/_search".to_string() } else { format!("/{}/_search", enc(&indices.join(","))) };
        let o = opts.unwrap_or(Value::Null);
        let flag = |k: &str, d: bool| o.get(k).and_then(Value::as_bool).unwrap_or(d);
        let expand: Vec<String> = match o.get("expand_wildcards") {
            Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
            _ => vec!["open".into()],
        };
        let uri = format!(
            "{path}?search_type={}&rest_total_hits_as_int={}&ignore_unavailable={}&allow_no_indices={}&expand_wildcards={}",
            enc(&req.get("search_type").map(Value::to_es_string).unwrap_or_else(|| "query_then_fetch".into())),
            req.get("rest_total_hits_as_int").and_then(Value::as_bool).unwrap_or(true),
            flag("ignore_unavailable", true),
            flag("allow_no_indices", true),
            enc(&expand.join(",")),
        );
        let r = internal::call(self.cluster, "POST", &uri, Some(&body)).await.map(|v| parse::java_map(&v));
        (Value::Obj(shown), r)
    }

    fn render_search_template(&self, t: &Value, model: &Value) -> EsResult<Value> {
        let mut params = t.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
        for (k, v) in model.as_object().into_iter().flatten() {
            params.insert(k.clone(), v.clone());
        }
        let engine = &self.cluster.scripts.engine;
        let (source, options): (String, Vec<(String, String)>) = match (t.get("id"), t.get("source")) {
            (Some(id), _) => {
                let id = id.to_es_string();
                let st = self.cluster.state();
                let Some(sc) = st.metadata.scripts.get(&id) else { return Err(EsError::resource_not_found(format!("unable to find script [{id}] in cluster state"))) };
                (match &sc.source {
                    Value::Str(s) => s.clone(),
                    other => other.to_json(),
                }, Vec::new())
            }
            (None, Some(Value::Str(s))) => (s.clone(), vec![("content_type".into(), "application/json".into())]),
            (None, Some(other)) => (other.to_json(), vec![("content_type".into(), "application/json".into())]),
            (None, None) => return Ok(Value::object()),
        };
        let tpl = engine.compile_template(&source, Context::Template, &options).map_err(crate::scripting::script_error)?;
        let out = tpl.render_json(&crate::scripting::to_serde(&Value::Obj(params))).map_err(crate::scripting::script_error)?;
        Ok(crate::json::parse(out.as_bytes())?.unwrap_or_else(Value::object))
    }

    // --- inputs -------------------------------------------------------------------------------

    async fn input(&self, input: &Value, payload: &Value) -> StepResult {
        let Some((ty, body)) = input.as_object().and_then(|m| m.iter().next()) else { return StepResult::ok("none", Value::object()) };
        match ty.as_str() {
            "none" => StepResult::ok("none", Value::object()),
            "simple" => StepResult::ok("simple", parse::java_map(body)),
            "search" => {
                let (shown, r) = self.search(body.get("request").unwrap_or(&Value::Null), payload).await;
                let r = match r {
                    Ok(p) => StepResult::ok("search", match body.get("extract") {
                        Some(Value::Arr(keys)) => extract(&p, keys),
                        _ => p,
                    }),
                    Err(e) => StepResult::failed("search", e),
                };
                r.with("search", obj! { "request" => shown })
            }
            "http" => {
                let stored = body.get("request").and_then(Value::as_object).cloned().unwrap_or_default();
                let model = self.model(payload);
                let req = match http::render(&stored, &|t| self.render(t, &model)) {
                    Ok(r) => r,
                    Err(e) => return StepResult::failed("http", e),
                };
                match http::send(self.cluster, &req).await {
                    Ok(resp) => {
                        let parsed = if resp.is_json() || body.get("response_content_type").is_some_and(|t| t.to_es_string() == "json") {
                            crate::json::parse(resp.body.as_bytes()).ok().flatten()
                        } else {
                            None
                        };
                        let mut p = match parsed {
                            Some(Value::Obj(m)) => match body.get("extract") {
                                Some(Value::Arr(keys)) => match extract(&Value::Obj(m), keys) {
                                    Value::Obj(x) => x,
                                    _ => Map::new(),
                                },
                                _ => m,
                            },
                            Some(Value::Arr(a)) => [("data".to_string(), Value::Arr(a))].into_iter().collect(),
                            _ => [("_value".to_string(), Value::from(resp.body.clone()))].into_iter().collect(),
                        };
                        if !resp.headers.is_empty() {
                            p.insert("_headers".into(), resp.headers_value());
                        }
                        p.insert("_status_code".into(), i64::from(resp.status).into());
                        StepResult::ok("http", parse::java_map(&Value::Obj(p))).with("http", obj! { "request" => req.to_record(), "status_code" => i64::from(resp.status) })
                    }
                    Err(e) => StepResult::failed("http", e).with("http", obj! { "request" => req.to_record() }),
                }
            }
            "chain" => {
                let mut acc = Map::new();
                let mut results = Map::new();
                for step in body.get("inputs").and_then(Value::as_array).into_iter().flatten() {
                    let Some((name, inp)) = step.as_object().and_then(|m| m.iter().next()) else { continue };
                    let r = Box::pin(self.input(inp, &Value::Obj(acc.clone()))).await;
                    results.insert(name.clone(), r.to_value(false));
                    match &r.payload {
                        Some(p) => {
                            acc.insert(name.clone(), p.clone());
                        }
                        None => return StepResult { ty: "chain".into(), payload: None, error: r.error, reason: None, extra: vec![("chain".into(), Value::Obj(results))] },
                    }
                }
                StepResult::ok("chain", Value::Obj(acc)).with("chain", Value::Obj(results))
            }
            "transform" => {
                let r = Box::pin(self.transform(body, payload)).await;
                StepResult { ty: "transform".into(), ..r }
            }
            other => StepResult::failed(other, EsError::illegal_argument(format!("unknown input type [{other}]"))),
        }
    }

    // --- conditions ---------------------------------------------------------------------------

    /// Evaluates a condition: its result, or the error of a script that failed.
    fn condition(&self, cond: &Value, payload: &Value) -> EsResult<(bool, Value)> {
        let Some((ty, body)) = cond.as_object().and_then(|m| m.iter().next()) else { return Ok((true, obj! { "type" => "always", "status" => "success", "met" => true })) };
        let now = self.execution_ms();
        let model = self.model(payload);
        let (met, extra) = match ty.as_str() {
            "always" => (true, None),
            "never" => (false, None),
            "compare" | "array_compare" => {
                let (met, detail) = super::compare::eval(ty, body, &model, now)?;
                (met, Some(detail))
            }
            "script" => {
                let v = self.run_script(body, Context::WatcherCondition, payload)?;
                (matches!(v, search_painless::Value::Bool(true)), None)
            }
            other => return Err(EsError::illegal_argument(format!("unknown condition type [{other}]"))),
        };
        let mut m = Map::new();
        m.insert("type".into(), ty.clone().into());
        m.insert("status".into(), "success".into());
        m.insert("met".into(), met.into());
        if let Some(d) = extra {
            m.insert(ty.clone(), d);
        }
        Ok((met, Value::Obj(m)))
    }

    // --- transforms ---------------------------------------------------------------------------

    async fn transform(&self, t: &Value, payload: &Value) -> StepResult {
        let Some((ty, body)) = t.as_object().and_then(|m| m.iter().next()) else { return StepResult::ok("chain", payload.clone()) };
        match ty.as_str() {
            "search" => {
                let (shown, r) = self.search(body.get("request").unwrap_or(&Value::Null), payload).await;
                let r = match r {
                    Ok(p) => StepResult::ok("search", p),
                    Err(e) => StepResult { reason: Some(e.reason().to_string()), ..StepResult::failed("search", e) },
                };
                r.with("search", obj! { "request" => shown })
            }
            "script" => match self.run_script(body, Context::WatcherTransform, payload) {
                Ok(v) => {
                    if let Err(e) = search_painless::xcontent::ensure_no_self_references(&v, Some("watcher action payload")) {
                        return StepResult::failed("script", EsError::java(400, e.kind, e.reason));
                    }
                    let json = crate::scripting::from_serde(&v.to_json());
                    StepResult::ok("script", match json {
                        Value::Obj(_) => json,
                        other => obj! { "_value" => other },
                    })
                }
                Err(e) => StepResult::failed("script", e),
            },
            "chain" => {
                let mut cur = payload.clone();
                let mut results = Vec::new();
                for step in body.as_array().into_iter().flatten() {
                    let r = Box::pin(self.transform(step, &cur)).await;
                    results.push(r.to_value(false));
                    match r.payload {
                        Some(p) => cur = p,
                        None => {
                            let reason = format!("failed to execute [{}] transform for [{}]", r.ty, self.record_id);
                            return StepResult { ty: "chain".into(), payload: None, error: None, reason: Some(reason), extra: vec![("chain".into(), obj! { "results" => Value::Arr(results) })] };
                        }
                    }
                }
                StepResult::ok("chain", cur).with("chain", obj! { "results" => Value::Arr(results) })
            }
            other => StepResult::failed(other, EsError::illegal_argument(format!("unknown transform type [{other}]"))),
        }
    }

    // --- actions ------------------------------------------------------------------------------

    /// Runs one action (`ActionWrapper.execute`): its result, and how it changes the action's
    /// status.
    async fn action(&self, id: &str, action: &Value, payload: &mut Value, status: &mut Map) -> Value {
        let Some((ty, body)) = action.as_object().and_then(|m| m.iter().find(|(k, _)| parse::ACTION_TYPES.contains(&k.as_str()))) else {
            return obj! { "id" => id, "type" => "unknown", "status" => "failure", "reason" => "unknown action type" };
        };
        let mode = self.mode(id);
        let now = self.execution_ms();
        let head = |st: &str| -> Map { [("id".to_string(), Value::from(id)), ("type".to_string(), Value::from(ty.clone())), ("status".to_string(), Value::from(st))].into_iter().collect() };
        if mode == "skip" {
            set_throttle(status, id, now, "manually skipped");
            let mut m = head("throttled");
            m.insert("reason".into(), "manually skipped".into());
            return Value::Obj(m);
        }
        if !mode.starts_with("force_")
            && let Some((st, reason)) = self.throttled(id, action, status)
        {
            set_throttle(status, id, now, &reason);
            let mut m = head(st);
            m.insert("reason".into(), reason.into());
            return Value::Obj(m);
        }
        let mut cond_result = None;
        if let Some(c) = action.get("condition") {
            match self.condition(c, payload) {
                Ok((met, r)) => {
                    if !met {
                        reset_ack(status, id, crate::time::now_millis());
                        let mut m = head("condition_failed");
                        m.insert("condition".into(), r);
                        m.insert("reason".into(), "condition not met. skipping".into());
                        return Value::Obj(m);
                    }
                    cond_result = Some(r);
                }
                Err(e) => {
                    let mut m = head("condition_failed");
                    m.insert("reason".into(), format!("condition failed. skipping: {}", e.reason()).into());
                    return Value::Obj(m);
                }
            }
        }
        let mut transformed = None;
        if let Some(t) = action.get("transform") {
            let r = self.transform(t, payload).await;
            if r.payload.is_none() {
                set_execution(status, id, now, false, "Failed to transform payload");
                let mut m = head("failure");
                if let Some(c) = cond_result {
                    m.insert("condition".into(), c);
                }
                m.insert("transform".into(), r.to_value(false));
                m.insert("reason".into(), "Failed to transform payload".into());
                return Value::Obj(m);
            }
            transformed = Some(r);
        }
        let simulate = mode.contains("simulate");
        // The action may change the payload it indexes (`_id`, `_index`, the execution time
        // field), as Elasticsearch's mutable payload maps do.
        let mut own = transformed.as_ref().and_then(|r| r.payload.clone());
        let target: &mut Value = match own.as_mut() {
            Some(p) => p,
            None => payload,
        };
        let (st, fields, reason) = match action.get("foreach").map(Value::to_es_string) {
            None => self.run_type(id, ty, body, target, simulate).await,
            Some(path) => self.run_foreach(id, ty, body, (&path, action.get("max_iterations").and_then(Value::as_i64).unwrap_or(100)), target, simulate).await,
        };
        match st {
            "success" | "simulated" => set_execution(status, id, now, true, ""),
            "failure" => set_execution(status, id, now, false, reason.as_deref().unwrap_or("")),
            _ => {}
        }
        let mut m = head(st);
        if let Some(c) = cond_result {
            m.insert("condition".into(), c);
        }
        if let Some(mut r) = transformed {
            r.payload = own;
            m.insert("transform".into(), r.to_value(false));
        }
        for (k, v) in fields {
            m.insert(k, v);
        }
        Value::Obj(m)
    }

    /// Whether an action is throttled: by its period (the action's, the watch's, or 5s since
    /// its last successful execution), then by an ack.
    fn throttled(&self, id: &str, action: &Value, status: &Map) -> Option<(&'static str, String)> {
        let a = status.get("actions").and_then(|x| x.get(id))?;
        let period = action.get("throttle_period_in_millis").and_then(Value::as_i64).unwrap_or(self.throttle_ms);
        if let Some(last) = a.pointer(&["last_successful_execution", "timestamp"]).and_then(|t| parse_date(t).ok()) {
            let elapsed = crate::time::now_millis() - last;
            if elapsed <= period {
                let tv = |ms: i64| crate::units::TimeValue { duration: ms, unit: crate::units::TimeUnit::Millis }.human();
                return Some(("throttled", format!("throttling interval is set to [{}] but time elapsed since last execution is [{}]", tv(period), tv(elapsed.max(0)))));
            }
        }
        if a.pointer(&["ack", "state"]).and_then(Value::as_str) == Some("acked") {
            let ts = a.pointer(&["ack", "timestamp"]).map(Value::to_es_string).unwrap_or_default();
            return Some(("acknowledged", format!("action [{id}] was acked at [{ts}]")));
        }
        None
    }

    /// `foreach` (`(path, max_iterations)`): the action once per element of a list.
    async fn run_foreach(&self, id: &str, ty: &str, body: &Value, (path, max): (&str, i64), payload: &Value, simulate: bool) -> (&'static str, Vec<(String, Value)>, Option<String>) {
        let model = self.model(payload);
        let fail = |msg: String| ("failure", vec![("error".to_string(), exception(msg).to_value_with_root(false))], None);
        let list = match resolve(&model, path) {
            Some(Value::Arr(a)) if a.is_empty() => return fail(format!("foreach object [{path}] was an empty list, could not run any action")),
            Some(Value::Arr(a)) => a.clone(),
            None | Some(Value::Null) => return fail(format!("specified foreach object was null: [{path}]")),
            Some(_) => return fail(format!("specified foreach object was not a an array/collection: [{path}]")),
        };
        let mut results = Vec::new();
        let mut statuses = std::collections::BTreeSet::new();
        for item in list.into_iter().take(max.max(0) as usize) {
            let mut p = match item {
                Value::Obj(_) => item,
                other => obj! { "_value" => other },
            };
            let (st, fields, _) = Box::pin(self.run_type(id, ty, body, &mut p, simulate)).await;
            statuses.insert(st);
            let mut m = Map::new();
            m.insert("type".into(), ty.into());
            m.insert("status".into(), st.into());
            for (k, v) in fields {
                m.insert(k, v);
            }
            results.push(Value::Obj(m));
        }
        let st = if statuses.len() == 1 { statuses.into_iter().next().unwrap_or("success") } else { "partial_failure" };
        let n = results.len();
        (st, vec![("number_of_actions_executed".into(), n.into()), ("foreach".into(), Value::Arr(results)), ("max_iterations".into(), max.into())], None)
    }

    /// Runs an action's type against `payload`: `(status, fields, failure reason)`.
    async fn run_type(&self, id: &str, ty: &str, body: &Value, payload: &mut Value, simulate: bool) -> (&'static str, Vec<(String, Value)>, Option<String>) {
        let model = self.model(payload);
        let failed = |e: EsError| ("failure", vec![("error".to_string(), e.to_value_with_root(false))], None);
        match ty {
            "logging" => match self.render(body.get("text").unwrap_or(&Value::Null), &model) {
                Ok((t, _)) => (if simulate { "simulated" } else { "success" }, vec![("logging".into(), obj! { "logged_text" => t })], None),
                Err(e) => failed(e),
            },
            "index" => match self.index(id, body, payload, simulate).await {
                Ok((st, v)) => (st, vec![("index".into(), v)], None),
                Err(e) => failed(e),
            },
            "webhook" => {
                let stored = body.as_object().cloned().unwrap_or_default();
                let req = match http::render(&stored, &|t| self.render(t, &model)) {
                    Ok(r) => r,
                    Err(e) => return failed(e),
                };
                if simulate {
                    return ("simulated", vec![("webhook".into(), obj! { "request" => req.to_record() })], None);
                }
                match http::send(self.cluster, &req).await {
                    Ok(resp) if resp.status >= 400 => {
                        let reason = format!("received [{}] status code", resp.status);
                        ("failure", vec![("reason".into(), reason.clone().into()), ("webhook".into(), obj! { "request" => req.to_record(), "response" => resp.to_record() })], Some(reason))
                    }
                    Ok(resp) => ("success", vec![("webhook".into(), obj! { "request" => req.to_record(), "response" => resp.to_record() })], None),
                    Err(e) => failed(e),
                }
            }
            "email" => {
                if !simulate {
                    return failed(self.no_account("email"));
                }
                match self.email_message(id, body, &model) {
                    Ok(m) => ("simulated", vec![("email".into(), obj! { "message" => m })], None),
                    Err(e) => failed(e),
                }
            }
            other => {
                if simulate {
                    ("simulated", vec![(other.to_string(), obj! {})], None)
                } else {
                    failed(self.no_account(other))
                }
            }
        }
    }

    /// The error of a notification action roll can't send: no account, or no transport.
    fn no_account(&self, ty: &str) -> EsError {
        let configured = self.cluster.config.node_settings.iter().any(|(k, _)| k.starts_with(&format!("xpack.notification.{ty}.account.")));
        if configured {
            EsError::illegal_state(format!("roll search does not send [{ty}] notifications"))
        } else {
            EsError::illegal_argument(format!("no accounts of type [{ty}] configured. Please set up an account using the [xpack.notification.{ty}] settings"))
        }
    }

    fn email_message(&self, id: &str, body: &Value, model: &Value) -> EsResult<Value> {
        let mut m = Map::new();
        m.insert("id".into(), format!("{id}_{}_1", self.record_id).into());
        let text = |k: &str| -> EsResult<Option<String>> { body.get(k).map(|t| self.render(t, model).map(|r| r.0)).transpose() };
        for k in ["from", "reply_to"] {
            if let Some(v) = text(k)? {
                m.insert(k.into(), v.into());
            }
        }
        if let Some(p) = body.get("priority") {
            m.insert("priority".into(), p.clone());
        }
        m.insert("sent_date".into(), crate::xpack::entsearch::now_nanos_string().into());
        for k in ["to", "cc", "bcc"] {
            let list: Vec<Value> = match body.get(k) {
                None => continue,
                Some(Value::Arr(a)) => a.iter().map(|x| self.render(x, model).map(|r| Value::from(r.0))).collect::<EsResult<_>>()?,
                Some(v) => self.render(v, model)?.0.split(',').map(|s| Value::from(s.trim())).filter(|s| s.as_str() != Some("")).collect(),
            };
            m.insert(k.into(), Value::Arr(list));
        }
        if let Some(s) = text("subject")? {
            m.insert("subject".into(), s.into());
        }
        match body.get("body") {
            None => {}
            Some(Value::Obj(b)) => {
                let mut out = Map::new();
                for k in ["text", "html"] {
                    if let Some(t) = b.get(k) {
                        out.insert(k.into(), self.render(t, model)?.0.into());
                    }
                }
                m.insert("body".into(), Value::Obj(out));
            }
            Some(t) => {
                m.insert("body".into(), obj! { "text" => self.render(t, model)?.0 });
            }
        }
        Ok(Value::Obj(m))
    }

    /// The `index` action (`ExecutableIndexAction`): one document, or a bulk of the payload's
    /// `_doc` list.
    async fn index(&self, id: &str, body: &Value, payload: &mut Value, simulate: bool) -> EsResult<(&'static str, Value)> {
        let index = body.get("index").map(Value::to_es_string);
        let doc_id = body.get("doc_id").map(Value::to_es_string);
        let time_field = body.get("execution_time_field").map(Value::to_es_string);
        let watch_id = &self.watch_id;
        let take = |doc: &mut Map, field: &str, name: &str, default: &Option<String>| -> EsResult<Option<String>> {
            match doc.shift_remove(field) {
                Some(v) if !v.is_null() => {
                    if default.is_some() {
                        return Err(EsError::illegal_state(format!(
                            "could not execute action [{id}] of watch [{watch_id}]. [ctx.payload.{field}] or [ctx.payload._doc.{field}] were set together with action [{name}] field. Only set one of them"
                        )));
                    }
                    Ok(Some(v.to_es_string()))
                }
                _ => Ok(default.clone()),
            }
        };
        let stamp = |doc: &mut Map| {
            if let Some(f) = &time_field {
                doc.insert(f.clone(), fmt_ms(self.execution_ms()).into());
            }
        };
        let Value::Obj(data) = payload else { return Err(EsError::illegal_argument("payload is not an object")) };
        if let Some(d) = data.get("_doc").cloned() {
            match d {
                Value::Arr(list) => {
                    if doc_id.is_some() {
                        return Err(EsError::illegal_state("could not execute action [{}] of watch [{}]. [doc_id] cannot be used with bulk [_doc] indexing"));
                    }
                    let mut lines = String::new();
                    let mut shown = Vec::new();
                    for item in list {
                        let Value::Obj(mut doc) = item else {
                            return Err(EsError::illegal_state(format!("could not execute action [{id}] of watch [{watch_id}]. failed to index payload data. [_data] field must either hold a Map or an List/Array of Maps")));
                        };
                        let i = take(&mut doc, "_index", "index", &index)?;
                        let d = take(&mut doc, "_id", "id", &None)?;
                        stamp(&mut doc);
                        let mut meta = Map::new();
                        if let Some(i) = &i {
                            meta.insert("_index".into(), i.clone().into());
                        }
                        if let Some(d) = &d {
                            meta.insert("_id".into(), d.clone().into());
                        }
                        shown.push(obj! { "_id" => d.map(Value::from).unwrap_or(Value::Null), "_index" => i.map(Value::from).unwrap_or(Value::Null) });
                        lines.push_str(&obj! { "index" => Value::Obj(meta) }.to_json());
                        lines.push('\n');
                        lines.push_str(&Value::Obj(doc).to_json());
                        lines.push('\n');
                    }
                    if simulate {
                        return Ok(("simulated", obj! { "request" => obj! { "index" => "", "doc_id" => "", "source" => Value::Arr(shown) } }));
                    }
                    let mut uri = "/_bulk".to_string();
                    if let Some(r) = body.get("refresh") {
                        uri.push_str(&format!("?refresh={}", enc(&r.to_es_string())));
                    }
                    let r = internal::call_ndjson(self.cluster, "POST", &uri, lines).await?;
                    let mut items = Vec::new();
                    let mut failures = 0;
                    for it in r.get("items").and_then(Value::as_array).into_iter().flatten() {
                        let x = it.as_object().and_then(|m| m.values().next()).cloned().unwrap_or(Value::Null);
                        if let Some(e) = x.get("error") {
                            failures += 1;
                            items.push(obj! { "failed" => true, "message" => e.get("reason").cloned().unwrap_or(Value::Null), "id" => x.get("_id").cloned().unwrap_or(Value::Null), "index" => x.get("_index").cloned().unwrap_or(Value::Null) });
                        } else {
                            items.push(index_response(&x));
                        }
                    }
                    let st = if failures == 0 { "success" } else if failures == items.len() { "failure" } else { "partial_failure" };
                    return Ok((st, obj! { "response" => Value::Arr(items) }));
                }
                Value::Obj(m) => *data = m,
                _ => {
                    return Err(EsError::illegal_argument(format!(
                        "could not execute action [{id}] of watch [{watch_id}]. failed to index payload data. [_data] field must either hold a Map or an List/Array of Maps"
                    )));
                }
            }
        }
        let i = take(data, "_index", "index", &index)?;
        let d = take(data, "_id", "id", &doc_id)?;
        stamp(data);
        let source = Value::Obj(data.clone());
        if simulate {
            let mut req = Map::new();
            if let Some(i) = &i {
                req.insert("index".into(), i.clone().into());
            }
            if let Some(d) = &d {
                req.insert("doc_id".into(), d.clone().into());
            }
            if let Some(r) = body.get("refresh") {
                req.insert("refresh".into(), r.clone());
            }
            req.insert("source".into(), source);
            return Ok(("simulated", obj! { "request" => Value::Obj(req) }));
        }
        let Some(i) = i else { return Err(EsError::action_request_validation(&["index is missing".into()])) };
        let mut uri = match &d {
            Some(d) => format!("/{}/_doc/{}", enc(&i), enc(d)),
            None => format!("/{}/_doc", enc(&i)),
        };
        let mut q = Vec::new();
        for k in ["op_type", "refresh", "timeout"] {
            if let Some(v) = body.get(k) {
                q.push(format!("{k}={}", enc(&v.to_es_string())));
            }
        }
        if !q.is_empty() {
            uri.push('?');
            uri.push_str(&q.join("&"));
        }
        let r = internal::call(self.cluster, if d.is_some() { "PUT" } else { "POST" }, &uri, Some(&source)).await?;
        Ok(("success", obj! { "response" => index_response(&r) }))
    }

    /// Runs the watch.
    pub async fn execute(&self, watch: &Value, mut status: Map, alternative_input: Option<Value>, ignore_condition: bool) -> Outcome {
        let mut out = Outcome { state: "failed", status: Map::new(), input: Value::Null, condition: None, transform: None, actions: Vec::new(), messages: Vec::new(), exception: None };
        let now = self.execution_ms();
        let input = match alternative_input {
            Some(alt) => StepResult::ok("simple", parse::java_map(&alt)),
            None => self.input(watch.get("input").unwrap_or(&Value::Null), &Value::object()).await,
        };
        out.input = input.to_value(false);
        let Some(mut payload) = input.payload else {
            out.messages.push("failed to execute watch input".into());
            status.insert("execution_state".into(), "failed".into());
            out.status = status;
            return out;
        };
        let cond = if ignore_condition { Ok((true, obj! { "type" => "always", "status" => "success", "met" => true })) } else { self.condition(watch.get("condition").unwrap_or(&Value::Null), &payload) };
        let (met, cond) = match cond {
            Ok(c) => c,
            Err(e) => {
                out.exception = Some(e.to_value(false));
                status.insert("execution_state".into(), "failed".into());
                out.status = status;
                return out;
            }
        };
        out.condition = Some(cond);
        on_check(&mut status, met, now);
        let actions = watch.get("actions").and_then(Value::as_object).cloned().unwrap_or_default();
        if met {
            let mut aborted = false;
            if !actions.is_empty()
                && let Some(t) = watch.get("transform")
            {
                let r = self.transform(t, &payload).await;
                out.transform = Some(r.to_value(true));
                match r.payload {
                    Some(p) => payload = p,
                    None => {
                        out.messages.push("failed to execute watch transform".into());
                        aborted = true;
                    }
                }
            }
            if !aborted {
                for (aid, a) in &actions {
                    let r = self.action(aid, a, &mut payload, &mut status).await;
                    out.actions.push((aid.clone(), r));
                }
            }
        }
        // Results print in the order of the map that collects them.
        let keys: Vec<&String> = out.actions.iter().map(|(k, _)| k).collect();
        let order = crate::json::java_hash_order(&keys, 16);
        let mut sorted: Vec<Option<(String, Value)>> = std::mem::take(&mut out.actions).into_iter().map(Some).collect();
        out.actions = order.into_iter().filter_map(|i| sorted[i].take()).collect();
        out.state = if !met {
            "execution_not_needed"
        } else if out.actions.iter().any(|(_, r)| r.get("status").and_then(Value::as_str) == Some("acknowledged")) {
            "acknowledged"
        } else if out.actions.iter().any(|(_, r)| r.get("status").and_then(Value::as_str) == Some("throttled")) {
            "throttled"
        } else {
            "executed"
        };
        status.insert("execution_state".into(), out.state.into());
        out.status = status;
        out
    }
}

fn index_response(r: &Value) -> Value {
    obj! {
        "created" => r.get("result").and_then(Value::as_str) == Some("created"),
        "result" => r.get("result").cloned().unwrap_or(Value::Null),
        "id" => r.get("_id").cloned().unwrap_or(Value::Null),
        "version" => r.get("_version").cloned().unwrap_or(Value::Null),
        "index" => r.get("_index").cloned().unwrap_or(Value::Null),
    }
}

// --- status bookkeeping (`WatchStatus`, `ActionStatus`) ------------------------------------------

fn action_status<'a>(status: &'a mut Map, id: &str) -> Option<&'a mut Map> {
    status.get_mut("actions").and_then(Value::as_object_mut).and_then(|a| a.get_mut(id)).and_then(Value::as_object_mut)
}

/// An action status's fields in Elasticsearch's order.
fn reorder(a: &mut Map) {
    let mut out = Map::new();
    for k in ["ack", "last_execution", "last_successful_execution", "last_throttle"] {
        if let Some(v) = a.shift_remove(k) {
            out.insert(k.into(), v);
        }
    }
    out.extend(std::mem::take(a));
    *a = out;
}

/// `WatchStatus.onCheck`: an unmet condition resets every action's ack.
fn on_check(status: &mut Map, met: bool, now: i64) {
    let ts = fmt_ms(now);
    let mut out = Map::new();
    out.insert("state".into(), status.shift_remove("state").unwrap_or(Value::Null));
    out.insert("last_checked".into(), ts.clone().into());
    let last_met = status.shift_remove("last_met_condition");
    if met {
        out.insert("last_met_condition".into(), ts.into());
    } else if let Some(l) = last_met {
        out.insert("last_met_condition".into(), l);
    }
    status.shift_remove("last_checked");
    out.extend(std::mem::take(status));
    *status = out;
    if !met {
        let ids: Vec<String> = status.get("actions").and_then(Value::as_object).map(|a| a.keys().cloned().collect()).unwrap_or_default();
        for id in ids {
            reset_ack(status, &id, now);
        }
    }
}

fn reset_ack(status: &mut Map, id: &str, now: i64) {
    if let Some(a) = action_status(status, id) {
        a.insert("ack".into(), obj! { "timestamp" => fmt_ms(now), "state" => "awaits_successful_execution" });
    }
}

fn set_throttle(status: &mut Map, id: &str, now: i64, reason: &str) {
    if let Some(a) = action_status(status, id) {
        a.insert("last_throttle".into(), obj! { "timestamp" => fmt_ms(now), "reason" => reason });
        reorder(a);
    }
}

fn set_execution(status: &mut Map, id: &str, now: i64, ok: bool, reason: &str) {
    let Some(a) = action_status(status, id) else { return };
    let ts = fmt_ms(now);
    if ok {
        a.insert("last_execution".into(), obj! { "timestamp" => ts.clone(), "successful" => true });
        a.insert("last_successful_execution".into(), obj! { "timestamp" => ts.clone(), "successful" => true });
        if a.get("ack").and_then(|x| x.get("state")).and_then(Value::as_str) == Some("awaits_successful_execution") {
            a.insert("ack".into(), obj! { "timestamp" => ts, "state" => "ackable" });
        }
    } else {
        a.insert("last_execution".into(), obj! { "timestamp" => ts, "successful" => false, "reason" => reason });
    }
    reorder(a);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_extract() {
        assert_eq!(fmt_ms(1420329600000), "2015-01-04T00:00:00.000Z");
        assert_eq!(zoned(crate::time::Instant::from_millis(1420329600000)), "2015-01-04T00:00Z");
        assert_eq!(zoned(crate::time::Instant::from_millis(1420329601500)), "2015-01-04T00:00:01.500Z");
        let p = obj! { "hits" => obj! { "total" => 3, "max" => 1 }, "took" => 2 };
        assert_eq!(extract(&p, &[Value::from("hits.total")]), obj! { "hits" => obj! { "total" => 3 } });
    }

    #[test]
    fn action_order() {
        let keys = ["a_log", "b_email", "c_each", "d_idx", "e_cond"];
        let order: Vec<&str> = crate::json::java_hash_order(&keys, 16).into_iter().map(|i| keys[i]).collect();
        assert_eq!(order, ["c_each", "a_log", "b_email", "e_cond", "d_idx"]);
    }
}

//! OTLP logs (`ExportLogsServiceRequest`, protobuf or JSON) as the documents Elastic's managed
//! OTLP endpoint writes in OTel-native form, for the built-in `logs-otel@template`
//! (`meta/otel_templates.json`):
//!
//! `@timestamp`, `observed_timestamp`, `severity_text`, `severity_number`, `body.text` (a string
//! body) or `body.structured` (a key-value list, or `{"value": …}` for any other kind),
//! `attributes`, `dropped_attributes_count`, `resource.{attributes, dropped_attributes_count,
//! schema_url}`, `scope.{name, version, attributes, dropped_attributes_count, schema_url}`,
//! `trace_id`, `span_id` (lowercase hex), `event_name`, `data_stream.{type, dataset, namespace}`.
//!
//! Each record goes to `logs-<dataset>.otel-<namespace>`: `data_stream.dataset` and
//! `data_stream.namespace` from the record's attributes, else its scope's, else its resource's,
//! else `generic` and `default`, made valid as the exporter does (lowercase, `-` and the
//! characters an index name can't hold become `_`, 100 bytes at most).
//!
//! Limits: a record's attributes past [`MAX_ATTRIBUTES`] are dropped and counted in its
//! `dropped_attributes_count` (as an SDK's limits do); a record with a string or bytes value past
//! [`MAX_VALUE_BYTES`], or values nested past [`MAX_DEPTH`], is rejected and named in the
//! answer's partial success; a malformed message fails the request.

use super::wire::{Fields, Malformed, Wire};
use crate::json::{Map, Value};
use crate::obj;

/// Attributes kept per record, scope or resource.
pub const MAX_ATTRIBUTES: usize = 128;
/// A string or bytes value's longest.
pub const MAX_VALUE_BYTES: usize = 32 * 1024;
/// Arrays and key-value lists nested in one value.
pub const MAX_DEPTH: usize = 8;

/// A resource's or scope's part of its records' documents.
#[derive(Clone, Debug, Default)]
struct Context {
    attributes: Map,
    dropped: u64,
    schema_url: Option<String>,
    name: Option<String>,
    version: Option<String>,
}

#[derive(Debug, Default)]
struct Record {
    time: u64,
    observed: u64,
    severity_number: Option<i64>,
    severity_text: Option<String>,
    body: Option<Value>,
    attributes: Map,
    dropped: u64,
    trace_id: Option<String>,
    span_id: Option<String>,
    event_name: Option<String>,
}

/// Why one record wasn't written (the request goes on without it).
#[derive(Debug)]
struct Rejected(String);

/// The documents of a request, as `_bulk` `create` lines, and what was left out.
#[derive(Debug, Default)]
pub struct Batch {
    pub ndjson: String,
    pub accepted: u64,
    pub rejected: u64,
    /// The first rejection's reason.
    pub reason: Option<String>,
}

impl Batch {
    fn reject(&mut self, why: String) {
        self.rejected += 1;
        self.reason.get_or_insert(why);
    }
}

// --- values ---------------------------------------------------------------------------------

fn hex(b: &[u8]) -> Option<String> {
    if b.is_empty() || b.iter().all(|x| *x == 0) {
        return None;
    }
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn long_value(n: usize) -> Rejected {
    Rejected(format!("a value of {n} bytes (at most {MAX_VALUE_BYTES})"))
}

/// An `AnyValue` from protobuf.
fn any_pb(b: &[u8], depth: usize) -> Result<Result<Value, Rejected>, Malformed> {
    if depth > MAX_DEPTH {
        return Ok(Err(Rejected(format!("values nested deeper than {MAX_DEPTH}"))));
    }
    let mut out = Value::Null;
    for f in Fields::new(b) {
        let (n, w) = f?;
        out = match n {
            1 => {
                let s = w.string()?;
                if s.len() > MAX_VALUE_BYTES {
                    return Ok(Err(long_value(s.len())));
                }
                Value::from(s)
            }
            2 => Value::Bool(w.uint()? != 0),
            3 => Value::from(w.uint()? as i64),
            4 => {
                let f = f64::from_bits(w.uint()?);
                if f.is_finite() { Value::from(f) } else { Value::from(f.to_string()) }
            }
            5 | 6 => {
                // ArrayValue { values = 1 } / KeyValueList { values = 1 }.
                let mut arr = Vec::new();
                let mut map = Map::new();
                for g in Fields::new(w.bytes()?) {
                    let (gn, gw) = g?;
                    if gn != 1 {
                        continue;
                    }
                    if n == 5 {
                        match any_pb(gw.bytes()?, depth + 1)? {
                            Ok(v) => arr.push(v),
                            Err(r) => return Ok(Err(r)),
                        }
                    } else {
                        match key_value_pb(gw.bytes()?, depth + 1)? {
                            Ok(Some((k, v))) => {
                                map.insert(k, v);
                            }
                            Ok(None) => {}
                            Err(r) => return Ok(Err(r)),
                        }
                    }
                }
                if n == 5 { Value::Arr(arr) } else { Value::Obj(map) }
            }
            7 => {
                let b = w.bytes()?;
                if b.len() > MAX_VALUE_BYTES {
                    return Ok(Err(long_value(b.len())));
                }
                Value::from(base64_encode(b))
            }
            _ => continue,
        };
    }
    Ok(Ok(out))
}

/// A `KeyValue` from protobuf: `(key, value)`; `None` without a key.
fn key_value_pb(b: &[u8], depth: usize) -> Result<Result<Option<(String, Value)>, Rejected>, Malformed> {
    let (mut key, mut value) = (None, Value::Null);
    for f in Fields::new(b) {
        let (n, w) = f?;
        match n {
            1 => key = Some(w.string()?.to_string()),
            2 => match any_pb(w.bytes()?, depth)? {
                Ok(v) => value = v,
                Err(r) => return Ok(Err(r)),
            },
            _ => {}
        }
    }
    Ok(Ok(key.map(|k| (k, value))))
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Adds an attribute, counting it as dropped past [`MAX_ATTRIBUTES`].
fn add_attribute(m: &mut Map, dropped: &mut u64, k: String, v: Value) {
    if m.len() >= MAX_ATTRIBUTES && !m.contains_key(&k) {
        *dropped += 1;
        return;
    }
    m.insert(k, v);
}

/// An `AnyValue` from OTLP/JSON (`{"stringValue": …}`, `{"intValue": "12"}`, …).
fn any_json(v: &Value, depth: usize) -> Result<Value, Rejected> {
    if depth > MAX_DEPTH {
        return Err(Rejected(format!("values nested deeper than {MAX_DEPTH}")));
    }
    let Some(m) = v.as_object() else { return Ok(Value::Null) };
    let get = |a: &str, b: &str| m.get(a).or_else(|| m.get(b));
    if let Some(s) = get("stringValue", "string_value") {
        let s = s.as_str().unwrap_or_default();
        if s.len() > MAX_VALUE_BYTES {
            return Err(long_value(s.len()));
        }
        return Ok(Value::from(s));
    }
    if let Some(b) = get("boolValue", "bool_value") {
        return Ok(Value::Bool(b.as_bool().unwrap_or(false)));
    }
    if let Some(i) = get("intValue", "int_value") {
        // JSON carries 64-bit integers as strings.
        let n = i.as_i64().or_else(|| i.as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
        return Ok(Value::from(n));
    }
    if let Some(d) = get("doubleValue", "double_value") {
        return Ok(d.as_f64().map(Value::from).unwrap_or_else(|| d.clone()));
    }
    if let Some(a) = get("arrayValue", "array_value") {
        let vals = a.get("values").and_then(Value::as_array).cloned().unwrap_or_default();
        return Ok(Value::Arr(vals.iter().map(|x| any_json(x, depth + 1)).collect::<Result<_, _>>()?));
    }
    if let Some(kv) = get("kvlistValue", "kvlist_value") {
        let mut out = Map::new();
        for e in kv.get("values").and_then(Value::as_array).into_iter().flatten() {
            if let Some(k) = e.get("key").and_then(Value::as_str) {
                out.insert(k.to_string(), any_json(e.get("value").unwrap_or(&Value::Null), depth + 1)?);
            }
        }
        return Ok(Value::Obj(out));
    }
    if let Some(b) = get("bytesValue", "bytes_value") {
        let s = b.as_str().unwrap_or_default();
        if s.len() > MAX_VALUE_BYTES * 4 / 3 + 4 {
            return Err(long_value(s.len() * 3 / 4));
        }
        return Ok(Value::from(s));
    }
    Ok(Value::Null)
}

fn attributes_json(list: Option<&Value>, out: &mut Map, dropped: &mut u64) -> Result<(), Rejected> {
    for e in list.and_then(Value::as_array).into_iter().flatten() {
        if let Some(k) = e.get("key").and_then(Value::as_str) {
            let v = any_json(e.get("value").unwrap_or(&Value::Null), 1)?;
            add_attribute(out, dropped, k.to_string(), v);
        }
    }
    Ok(())
}

// --- routing and documents ------------------------------------------------------------------

/// `data_stream.dataset` or `.namespace` made valid as Elastic's exporter does.
fn sanitize(s: &str, max: usize) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if matches!(c, '\\' | '/' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' | ',' | '#' | ':' | '-') { '_' } else { c })
        .collect();
    while out.len() > max {
        out.pop();
    }
    out
}

fn routing(rec: &Record, scope: &Context, resource: &Context, key: &str, default: &str) -> String {
    let found = [&rec.attributes, &scope.attributes, &resource.attributes].into_iter().find_map(|m| m.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()));
    let s = sanitize(found.unwrap_or(default), 100);
    if s.is_empty() || s.starts_with(['_', '.', '+']) { sanitize(default, 100) } else { s }
}

fn timestamp(nanos: u64) -> String {
    let secs = (nanos / 1_000_000_000) as i64;
    let sub = (nanos % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, sub).unwrap_or_default().format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
}

fn context_json(c: &Context, scope: bool) -> Value {
    let mut m = Map::new();
    if scope {
        if let Some(n) = &c.name {
            m.insert("name".into(), Value::from(n.as_str()));
        }
        if let Some(v) = &c.version {
            m.insert("version".into(), Value::from(v.as_str()));
        }
    }
    m.insert("attributes".into(), Value::Obj(c.attributes.clone()));
    m.insert("dropped_attributes_count".into(), Value::from(c.dropped));
    if let Some(u) = &c.schema_url {
        m.insert("schema_url".into(), Value::from(u.as_str()));
    }
    Value::Obj(m)
}

/// Appends a record's `create` line and document.
fn emit(out: &mut Batch, rec: Record, scope: &Context, resource: &Context, now_ms: i64) {
    let dataset = routing(&rec, scope, resource, "data_stream.dataset", "generic");
    let namespace = routing(&rec, scope, resource, "data_stream.namespace", "default");
    let observed = if rec.observed > 0 { rec.observed } else { (now_ms.max(0) as u64) * 1_000_000 };
    let time = if rec.time > 0 { rec.time } else { observed };
    let mut doc = obj! {
        "@timestamp" => timestamp(time),
        "observed_timestamp" => timestamp(observed),
        "data_stream" => obj! { "type" => "logs", "dataset" => format!("{dataset}.otel"), "namespace" => namespace.clone() },
    };
    let Value::Obj(d) = &mut doc else { return };
    if let Some(t) = rec.severity_text {
        d.insert("severity_text".into(), Value::from(t));
    }
    if let Some(n) = rec.severity_number.filter(|n| *n > 0) {
        d.insert("severity_number".into(), Value::from(n));
    }
    match rec.body {
        Some(Value::Str(s)) => {
            d.insert("body".into(), obj! { "text" => s });
        }
        Some(Value::Obj(m)) => {
            d.insert("body".into(), obj! { "structured" => Value::Obj(m) });
        }
        Some(Value::Null) | None => {}
        Some(other) => {
            d.insert("body".into(), obj! { "structured" => obj! { "value" => other } });
        }
    }
    d.insert("attributes".into(), Value::Obj(rec.attributes));
    d.insert("dropped_attributes_count".into(), Value::from(rec.dropped));
    d.insert("resource".into(), context_json(resource, false));
    d.insert("scope".into(), context_json(scope, true));
    if let Some(t) = rec.trace_id {
        d.insert("trace_id".into(), Value::from(t));
    }
    if let Some(s) = rec.span_id {
        d.insert("span_id".into(), Value::from(s));
    }
    if let Some(e) = rec.event_name.filter(|e| !e.is_empty()) {
        d.insert("event_name".into(), Value::from(e));
    }
    let index = format!("logs-{dataset}.otel-{namespace}");
    out.ndjson.push_str(&obj! { "create" => obj! { "_index" => index } }.to_json());
    out.ndjson.push('\n');
    out.ndjson.push_str(&doc.to_json());
    out.ndjson.push('\n');
    out.accepted += 1;
}

// --- protobuf ---------------------------------------------------------------------------------

/// A `Resource` (`attributes = 1`, `dropped_attributes_count = 2`) or an `InstrumentationScope`
/// (`name = 1`, `version = 2`, `attributes = 3`, `dropped_attributes_count = 4`).
fn context_pb(b: &[u8], scope: bool) -> Result<Result<Context, Rejected>, Malformed> {
    let mut c = Context::default();
    let (attrs, dropped) = if scope { (3, 4) } else { (1, 2) };
    for f in Fields::new(b) {
        let (n, w) = f?;
        match n {
            1 if scope => c.name = Some(w.string()?.to_string()),
            2 if scope => c.version = Some(w.string()?.to_string()),
            n if n == attrs => match key_value_pb(w.bytes()?, 1)? {
                Ok(Some((k, v))) => add_attribute(&mut c.attributes, &mut c.dropped, k, v),
                Ok(None) => {}
                Err(r) => return Ok(Err(r)),
            },
            n if n == dropped => c.dropped = c.dropped.saturating_add(w.uint()?),
            _ => {}
        }
    }
    Ok(Ok(c))
}

/// A `LogRecord`.
fn record_pb(b: &[u8]) -> Result<Result<Record, Rejected>, Malformed> {
    let mut r = Record::default();
    for f in Fields::new(b) {
        let (n, w) = f?;
        match n {
            1 => r.time = w.uint()?,
            11 => r.observed = w.uint()?,
            2 => r.severity_number = Some(w.uint()? as i64),
            3 => r.severity_text = Some(w.string()?.to_string()),
            5 => match any_pb(w.bytes()?, 0)? {
                Ok(v) => r.body = Some(v),
                Err(x) => return Ok(Err(x)),
            },
            6 => match key_value_pb(w.bytes()?, 1)? {
                Ok(Some((k, v))) => add_attribute(&mut r.attributes, &mut r.dropped, k, v),
                Ok(None) => {}
                Err(x) => return Ok(Err(x)),
            },
            7 => r.dropped = r.dropped.saturating_add(w.uint()?),
            9 => r.trace_id = hex(w.bytes()?),
            10 => r.span_id = hex(w.bytes()?),
            12 => r.event_name = Some(w.string()?.to_string()),
            _ => {}
        }
    }
    Ok(Ok(r))
}

/// The documents of an `ExportLogsServiceRequest` in protobuf (`resource_logs = 1`; each
/// `ResourceLogs`: `resource = 1`, `scope_logs = 2`, `schema_url = 3`; each `ScopeLogs`:
/// `scope = 1`, `log_records = 2`, `schema_url = 3`), one record at a time. `charge` is told
/// the NDJSON's size as it grows and may stop the request.
pub fn from_protobuf(body: &[u8], now_ms: i64, charge: &mut dyn FnMut(usize) -> Result<(), String>) -> Result<Batch, String> {
    let mut out = Batch::default();
    let m = |e: Malformed| e.0;
    for f in Fields::new(body) {
        let (n, w) = f.map_err(m)?;
        if n != 1 {
            continue;
        }
        let rl = w.bytes().map_err(m)?;
        // The resource and the schema URL may come after the scopes: read them first.
        let mut resource = Context::default();
        let mut resource_bad = None;
        for g in Fields::new(rl) {
            let (gn, gw) = g.map_err(m)?;
            match gn {
                1 => match context_pb(gw.bytes().map_err(m)?, false).map_err(m)? {
                    Ok(c) => resource = Context { schema_url: resource.schema_url.take(), ..c },
                    Err(Rejected(why)) => resource_bad = Some(why),
                },
                3 => resource.schema_url = Some(gw.string().map_err(m)?.to_string()),
                _ => {}
            }
        }
        for g in Fields::new(rl) {
            let (gn, gw) = g.map_err(m)?;
            if gn != 2 {
                continue;
            }
            let sl = gw.bytes().map_err(m)?;
            let mut scope = Context::default();
            let mut scope_bad = resource_bad.clone();
            for h in Fields::new(sl) {
                let (hn, hw) = h.map_err(m)?;
                match hn {
                    1 => match context_pb(hw.bytes().map_err(m)?, true).map_err(m)? {
                        Ok(c) => scope = Context { schema_url: scope.schema_url.take(), ..c },
                        Err(Rejected(why)) => scope_bad = Some(why),
                    },
                    3 => scope.schema_url = Some(hw.string().map_err(m)?.to_string()),
                    _ => {}
                }
            }
            for h in Fields::new(sl) {
                let (hn, hw) = h.map_err(m)?;
                if hn != 2 {
                    continue;
                }
                let rec = record_pb(hw.bytes().map_err(m)?).map_err(m)?;
                match (rec, &scope_bad) {
                    (_, Some(why)) => out.reject(why.clone()),
                    (Err(Rejected(why)), None) => out.reject(why),
                    (Ok(r), None) => {
                        emit(&mut out, r, &scope, &resource, now_ms);
                        charge(out.ndjson.len())?;
                    }
                }
            }
        }
    }
    Ok(out)
}

// --- JSON -----------------------------------------------------------------------------------

fn get<'v>(m: &'v Value, camel: &str, snake: &str) -> Option<&'v Value> {
    m.get(camel).or_else(|| m.get(snake))
}

fn uint_json(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Str(s)) => s.parse().unwrap_or(0),
        Some(x) => x.as_i64().map_or(0, |n| n.max(0) as u64),
        None => 0,
    }
}

fn context_json_in(v: Option<&Value>, schema_url: Option<&Value>, scope: bool) -> Result<Context, Rejected> {
    let mut c = Context { schema_url: schema_url.and_then(Value::as_str).map(str::to_string), ..Context::default() };
    let Some(v) = v else { return Ok(c) };
    if scope {
        c.name = v.get("name").and_then(Value::as_str).map(str::to_string);
        c.version = v.get("version").and_then(Value::as_str).map(str::to_string);
    }
    attributes_json(v.get("attributes"), &mut c.attributes, &mut c.dropped)?;
    c.dropped = c.dropped.saturating_add(uint_json(get(v, "droppedAttributesCount", "dropped_attributes_count")));
    Ok(c)
}

fn record_json(v: &Value) -> Result<Record, Rejected> {
    let mut r = Record {
        time: uint_json(get(v, "timeUnixNano", "time_unix_nano")),
        observed: uint_json(get(v, "observedTimeUnixNano", "observed_time_unix_nano")),
        severity_number: get(v, "severityNumber", "severity_number").and_then(|s| s.as_i64().or_else(|| s.as_str().and_then(severity_by_name))),
        severity_text: get(v, "severityText", "severity_text").and_then(Value::as_str).map(str::to_string),
        trace_id: get(v, "traceId", "trace_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.chars().any(|c| c != '0')).map(str::to_ascii_lowercase),
        span_id: get(v, "spanId", "span_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.chars().any(|c| c != '0')).map(str::to_ascii_lowercase),
        event_name: get(v, "eventName", "event_name").and_then(Value::as_str).map(str::to_string),
        ..Record::default()
    };
    if let Some(b) = v.get("body") {
        r.body = Some(any_json(b, 0)?);
    }
    attributes_json(v.get("attributes"), &mut r.attributes, &mut r.dropped)?;
    r.dropped = r.dropped.saturating_add(uint_json(get(v, "droppedAttributesCount", "dropped_attributes_count")));
    Ok(r)
}

/// `SEVERITY_NUMBER_INFO` and the like, as some JSON encoders write the enum.
fn severity_by_name(s: &str) -> Option<i64> {
    let base = |p: &str, n: i64| s.strip_prefix("SEVERITY_NUMBER_").and_then(|x| x.strip_prefix(p)).map(|d| n + d.parse::<i64>().map_or(0, |k| (k - 1).clamp(0, 3)));
    base("TRACE", 1).or_else(|| base("DEBUG", 5)).or_else(|| base("INFO", 9)).or_else(|| base("WARN", 13)).or_else(|| base("ERROR", 17)).or_else(|| base("FATAL", 21))
}

/// The documents of an `ExportLogsServiceRequest` in OTLP/JSON.
pub fn from_json(body: &Value, now_ms: i64, charge: &mut dyn FnMut(usize) -> Result<(), String>) -> Result<Batch, String> {
    let mut out = Batch::default();
    let Some(list) = get(body, "resourceLogs", "resource_logs") else { return Ok(out) };
    let Some(list) = list.as_array() else { return Err("resourceLogs isn't a list".into()) };
    for rl in list {
        let resource = context_json_in(rl.get("resource"), get(rl, "schemaUrl", "schema_url"), false);
        for sl in get(rl, "scopeLogs", "scope_logs").and_then(Value::as_array).into_iter().flatten() {
            let scope = context_json_in(sl.get("scope"), get(sl, "schemaUrl", "schema_url"), true);
            for r in get(sl, "logRecords", "log_records").and_then(Value::as_array).into_iter().flatten() {
                match (&resource, &scope, record_json(r)) {
                    (Err(Rejected(why)), _, _) | (_, Err(Rejected(why)), _) => out.reject(why.clone()),
                    (_, _, Err(Rejected(why))) => out.reject(why),
                    (Ok(res), Ok(sc), Ok(rec)) => {
                        emit(&mut out, rec, sc, res, now_ms);
                        charge(out.ndjson.len())?;
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Protobuf encoders for the tests.
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }
    fn field(n: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(n) << 3 | 2, &mut out);
        varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }
    fn fixed64(n: u32, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(n) << 3 | 1, &mut out);
        out.extend_from_slice(&v.to_le_bytes());
        out
    }
    fn uint(n: u32, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(n) << 3, &mut out);
        varint(v, &mut out);
        out
    }
    fn string_kv(k: &str, v: &str) -> Vec<u8> {
        [field(1, k.as_bytes()), field(2, &field(1, v.as_bytes()))].concat()
    }
    fn request(resource_attrs: &[(&str, &str)], records: &[Vec<u8>]) -> Vec<u8> {
        let resource: Vec<u8> = resource_attrs.iter().flat_map(|(k, v)| field(1, &string_kv(k, v))).collect();
        let scope = [field(1, b"my.lib"), field(2, b"1.2.0")].concat();
        let mut scope_logs = field(1, &scope);
        for r in records {
            scope_logs.extend(field(2, r));
        }
        let rl = [field(1, &resource), field(2, &scope_logs)].concat();
        field(1, &rl)
    }
    fn docs(b: &Batch) -> Vec<(Value, Value)> {
        let lines: Vec<&str> = b.ndjson.lines().collect();
        lines.chunks(2).map(|c| (crate::json::parse(c[0].as_bytes()).unwrap().unwrap(), crate::json::parse(c[1].as_bytes()).unwrap().unwrap())).collect()
    }
    fn free(_: usize) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn protobuf_and_json_records_become_the_same_otel_documents() {
        let rec = [
            fixed64(1, 1_700_000_000_123_456_789),
            uint(2, 9),
            field(3, b"INFO"),
            field(5, &field(1, b"user signed in")),
            field(6, &string_kv("http.route", "/login")),
            field(6, &string_kv("data_stream.dataset", "Web-App")),
            field(9, &[0xab; 16]),
            field(10, &[0x01; 8]),
        ]
        .concat();
        let pb = from_protobuf(&request(&[("service.name", "checkout")], &[rec]), 0, &mut free).unwrap();
        let json = crate::json::parse(
            br#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},
                "scopeLogs":[{"scope":{"name":"my.lib","version":"1.2.0"},"logRecords":[{"timeUnixNano":"1700000000123456789","severityNumber":9,
                "severityText":"INFO","body":{"stringValue":"user signed in"},"attributes":[{"key":"http.route","value":{"stringValue":"/login"}},
                {"key":"data_stream.dataset","value":{"stringValue":"Web-App"}}],"traceId":"ABABABABABABABABABABABABABABABAB","spanId":"0101010101010101"}]}]}]}"#,
        )
        .unwrap()
        .unwrap();
        let js = from_json(&json, 0, &mut free).unwrap();
        let (a, b) = (docs(&pb), docs(&js));
        assert_eq!(a, b);
        let (action, doc) = &a[0];
        assert_eq!(action.pointer(&["create", "_index"]).and_then(Value::as_str), Some("logs-web_app.otel-default"));
        assert_eq!(doc.get("@timestamp").and_then(Value::as_str), Some("2023-11-14T22:13:20.123456789Z"));
        assert_eq!(doc.pointer(&["body", "text"]).and_then(Value::as_str), Some("user signed in"));
        assert_eq!(doc.pointer(&["resource", "attributes", "service.name"]).and_then(Value::as_str), Some("checkout"));
        assert_eq!(doc.pointer(&["scope", "name"]).and_then(Value::as_str), Some("my.lib"));
        assert_eq!(doc.get("trace_id").and_then(Value::as_str), Some("abababababababababababababababab"));
        assert_eq!(doc.get("severity_number").and_then(Value::as_i64), Some(9));
        assert_eq!(doc.pointer(&["data_stream", "dataset"]).and_then(Value::as_str), Some("web_app.otel"));
    }

    #[test]
    fn limits_drop_attributes_reject_records_and_refuse_lies() {
        // 200 attributes: 128 kept, 72 counted as dropped.
        let mut rec = vec![field(5, &field(1, b"x"))];
        for i in 0..200 {
            rec.push(field(6, &string_kv(&format!("k{i}"), "v")));
        }
        let b = from_protobuf(&request(&[], &[rec.concat()]), 1, &mut free).unwrap();
        let (_, doc) = &docs(&b)[0];
        assert_eq!(doc.get("attributes").and_then(Value::as_object).map(|m| m.len()), Some(MAX_ATTRIBUTES));
        assert_eq!(doc.get("dropped_attributes_count").and_then(Value::as_i64), Some(72));
        // A value past 32 KiB and one nested 20 deep are rejected, and the others still written.
        let long = [field(5, &field(1, &vec![b'a'; MAX_VALUE_BYTES + 1]))].concat();
        let mut deep = field(1, b"leaf");
        for _ in 0..20 {
            deep = field(5, &field(1, &deep));
        }
        let ok = field(5, &field(1, b"fine"));
        let b = from_protobuf(&request(&[], &[long, field(5, &deep), ok]), 1, &mut free).unwrap();
        assert_eq!((b.accepted, b.rejected), (1, 2));
        assert!(b.reason.unwrap().contains("32768"));
        // A length past the bytes present fails the request.
        assert!(from_protobuf(&[0x0a, 0xff, 0x01, 0x00], 1, &mut free).is_err());
        // The charge can stop it.
        let rec = field(5, &field(1, b"x"));
        let mut tight = |n: usize| if n > 10 { Err("too large".to_string()) } else { Ok(()) };
        assert!(from_protobuf(&request(&[], &[rec]), 1, &mut tight).is_err());
    }
}

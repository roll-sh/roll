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
//! Each record goes to `logs-<dataset>-<namespace>` with `.otel` ending the dataset:
//! `data_stream.dataset` and `data_stream.namespace` from the record's attributes, else its
//! scope's, else its resource's, else `generic` and `default`, made valid as the exporter does
//! (lowercase; `-` and the characters an index name can't hold become `_`; `.otel` appended
//! unless it's there; 100 bytes at most with it). A request writes at most [`MAX_TARGETS`] data
//! streams; records for more are rejected in the answer's partial success.
//!
//! Memory ("charge what you build"): every value is charged to the request's [`Budget`] as it's
//! built (its [`VALUE_COST`] plus its bytes) and given back once its record is written out, and
//! the `_bulk` NDJSON is charged as it grows, in chunks of at most [`CHUNK_BYTES`]. A record
//! holds at most [`MAX_VALUES`] values, so a wide array can't be built before the charge stops it.
//!
//! Limits: a record's attributes past [`MAX_ATTRIBUTES`] are dropped and counted in its
//! `dropped_attributes_count` (as an SDK's limits do; the routing attributes always stay); a
//! string or bytes value past [`MAX_VALUE_BYTES`] is cut there and counted (the answer's partial
//! success carries the count as a warning); values nested past [`MAX_DEPTH`] or a record past
//! [`MAX_VALUES`] reject the record; trace and span ids of the wrong length are left out; a
//! malformed message fails the request.

use super::wire::{Fields, Malformed};
use crate::breaker::Reservation;
use crate::error::EsError;
use crate::json::{Map, Value};
use crate::obj;
use std::collections::HashSet;

/// Attributes kept per record, scope or resource.
pub const MAX_ATTRIBUTES: usize = 128;
/// A string or bytes value's longest; longer ones are cut.
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;
/// Arrays and key-value lists nested in one value.
pub const MAX_DEPTH: usize = 8;
/// Values (attributes, entries of arrays and key-value lists) one record may hold.
pub const MAX_VALUES: usize = 10_000;
/// Data streams one request may write.
pub const MAX_TARGETS: usize = 64;
/// The most NDJSON one internal `_bulk` carries.
pub const CHUNK_BYTES: usize = 5 * 1024 * 1024;
/// What a built value costs beyond its bytes: the `Value` and its slot in a list or map.
pub const VALUE_COST: usize = std::mem::size_of::<Value>() + 32;
/// Charged to the breaker at a time.
const STEP: u64 = 64 * 1024;

/// Why decoding stopped.
#[derive(Debug)]
pub enum Fail {
    /// The request doesn't decode (400).
    Malformed(String),
    /// The breaker refused what the request builds (its error, a 429).
    Tripped(EsError),
}

/// Why one record wasn't written; the request goes on without it.
#[derive(Debug)]
struct Rejected(String);

/// A decoding step's outcome: a value, a rejected record, or a stopped request.
enum Step<T> {
    Ok(T),
    Reject(Rejected),
    Stop(Fail),
}

impl From<Malformed> for Fail {
    fn from(m: Malformed) -> Fail {
        Fail::Malformed(m.0)
    }
}

/// A request's memory account on the request breaker: values held for the record being built,
/// and the NDJSON written so far.
pub struct Budget {
    held: Reservation,
    /// Bytes charged and not yet reserved (reserved [`STEP`] at a time).
    owed: u64,
    /// Bytes of the record being built.
    record_bytes: u64,
    record_values: usize,
}

impl Budget {
    pub fn new(held: Reservation) -> Budget {
        Budget { held, owed: 0, record_bytes: 0, record_values: 0 }
    }

    fn charge(&mut self, bytes: u64) -> Result<(), Fail> {
        self.owed += bytes;
        if self.owed >= STEP {
            let n = std::mem::take(&mut self.owed);
            self.held.add(n, "<otlp_request>").map_err(Fail::Tripped)?;
        }
        Ok(())
    }

    /// One value of the record being built, holding `bytes` of text.
    fn value(&mut self, bytes: usize) -> Step<()> {
        self.record_values += 1;
        if self.record_values > MAX_VALUES {
            return Step::Reject(Rejected(format!("a record with more than {MAX_VALUES} values")));
        }
        let n = (VALUE_COST + bytes) as u64;
        self.record_bytes += n;
        match self.charge(n) {
            Ok(()) => Step::Ok(()),
            Err(f) => Step::Stop(f),
        }
    }

    /// `bytes` of text more in the record being built (a value already counted).
    fn bytes(&mut self, bytes: usize) -> Step<()> {
        let n = bytes as u64;
        self.record_bytes += n;
        match self.charge(n) {
            Ok(()) => Step::Ok(()),
            Err(f) => Step::Stop(f),
        }
    }

    /// A kept string of the record (or context) being built.
    fn text(&mut self, bytes: usize) -> Result<(), Fail> {
        self.record_bytes += bytes as u64;
        self.charge(bytes as u64)
    }

    /// The record was written out (or rejected): its values are given back.
    fn record_done(&mut self) {
        let n = std::mem::take(&mut self.record_bytes);
        let from_owed = n.min(self.owed);
        self.owed -= from_owed;
        self.held.release(n - from_owed);
        self.record_values = 0;
    }

    /// `bytes` more of NDJSON.
    fn ndjson(&mut self, bytes: usize) -> Result<(), Fail> {
        self.charge(bytes as u64)
    }

    /// A chunk of `bytes` was sent and dropped.
    pub fn sent(&mut self, bytes: usize) {
        let n = bytes as u64;
        let from_owed = n.min(self.owed);
        self.owed -= from_owed;
        self.held.release(n - from_owed);
    }
}

/// The documents of a request, as `_bulk` `create` lines in chunks, and what was left out.
#[derive(Debug, Default)]
pub struct Batch {
    pub chunks: Vec<String>,
    current: String,
    pub accepted: u64,
    pub rejected: u64,
    /// Values cut at [`MAX_VALUE_BYTES`].
    pub truncated: u64,
    /// The first rejection's reason.
    pub reason: Option<String>,
    targets: HashSet<String>,
}

impl Batch {
    fn reject(&mut self, why: String) {
        self.rejected += 1;
        self.reason.get_or_insert(why);
    }

    /// The chunk being written is full: kept at its size (only its length is charged).
    fn push_chunk(&mut self) {
        let mut c = std::mem::take(&mut self.current);
        c.shrink_to_fit();
        self.chunks.push(c);
    }

    fn finish(mut self) -> Batch {
        if !self.current.is_empty() {
            self.push_chunk();
        }
        self
    }
}

// --- values ---------------------------------------------------------------------------------

fn hex(b: &[u8], len: usize) -> Option<String> {
    if b.len() != len || b.iter().all(|x| *x == 0) {
        return None;
    }
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// `s` cut to [`MAX_VALUE_BYTES`] on a character boundary, counting a cut.
fn bounded(s: &str, batch: &mut Batch) -> String {
    if s.len() <= MAX_VALUE_BYTES {
        return s.to_string();
    }
    batch.truncated += 1;
    let mut end = MAX_VALUE_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A short string field (names, versions, severity text) as kept.
fn short(s: &str, batch: &mut Batch) -> String {
    bounded(s, batch)
}

/// [`short`], charged to the record (or context) being built.
fn text(s: &str, budget: &mut Budget, batch: &mut Batch) -> Result<String, Fail> {
    let kept = short(s, batch);
    budget.text(kept.len())?;
    Ok(kept)
}

macro_rules! stop {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(f) => return Step::Stop(f),
        }
    };
}

macro_rules! step {
    ($e:expr) => {
        match $e {
            Step::Ok(v) => v,
            Step::Reject(r) => return Step::Reject(r),
            Step::Stop(f) => return Step::Stop(f),
        }
    };
}

macro_rules! wire {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(m) => return Step::Stop(Fail::from(m)),
        }
    };
}

/// An `AnyValue` from protobuf.
fn any_pb(b: &[u8], depth: usize, budget: &mut Budget, batch: &mut Batch) -> Step<Value> {
    if depth > MAX_DEPTH {
        return Step::Reject(Rejected(format!("values nested deeper than {MAX_DEPTH}")));
    }
    // Counted before its fields are read: an empty value is a value too.
    step!(budget.value(0));
    let mut out = Value::Null;
    for f in Fields::new(b) {
        let (n, w) = wire!(f);
        out = match n {
            1 => {
                let s = wire!(w.string());
                step!(budget.bytes(s.len().min(MAX_VALUE_BYTES)));
                Value::from(bounded(s, batch))
            }
            2 => Value::Bool(wire!(w.uint()) != 0),
            3 => Value::from(wire!(w.uint()) as i64),
            4 => {
                let f = f64::from_bits(wire!(w.uint()));
                if f.is_finite() { Value::from(f) } else { Value::from(f.to_string()) }
            }
            5 | 6 => {
                // ArrayValue { values = 1 } / KeyValueList { values = 1 }.
                let mut arr = Vec::new();
                let mut map = Map::new();
                for g in Fields::new(wire!(w.bytes())) {
                    let (gn, gw) = wire!(g);
                    if gn != 1 {
                        continue;
                    }
                    let inner = wire!(gw.bytes());
                    if n == 5 {
                        arr.push(step!(any_pb(inner, depth + 1, budget, batch)));
                    } else if let Some((k, v)) = step!(key_value_pb(inner, depth + 1, budget, batch)) {
                        map.insert(k, v);
                    }
                }
                if n == 5 { Value::Arr(arr) } else { Value::Obj(map) }
            }
            7 => {
                let raw = wire!(w.bytes());
                let kept = &raw[..raw.len().min(MAX_VALUE_BYTES)];
                if kept.len() < raw.len() {
                    batch.truncated += 1;
                }
                step!(budget.bytes(kept.len() * 4 / 3 + 4));
                Value::from(base64_encode(kept))
            }
            _ => continue,
        };
    }
    Step::Ok(out)
}

/// A `KeyValue` from protobuf: `(key, value)`; `None` without a key.
fn key_value_pb(b: &[u8], depth: usize, budget: &mut Budget, batch: &mut Batch) -> Step<Option<(String, Value)>> {
    let (mut key, mut value) = (None, Value::Null);
    for f in Fields::new(b) {
        let (n, w) = wire!(f);
        match n {
            1 => {
                let k = wire!(w.string());
                step!(budget.value(k.len().min(MAX_VALUE_BYTES)));
                key = Some(short(k, batch));
            }
            2 => value = step!(any_pb(wire!(w.bytes()), depth, budget, batch)),
            _ => {}
        }
    }
    Step::Ok(key.map(|k| (k, value)))
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Attributes that decide where a record goes: kept past [`MAX_ATTRIBUTES`].
fn routing_key(k: &str) -> bool {
    k == "data_stream.dataset" || k == "data_stream.namespace"
}

/// Adds an attribute, counting it as dropped past [`MAX_ATTRIBUTES`].
fn add_attribute(m: &mut Map, dropped: &mut u64, k: String, v: Value) {
    if m.len() >= MAX_ATTRIBUTES && !m.contains_key(&k) && !routing_key(&k) {
        *dropped = dropped.saturating_add(1);
        return;
    }
    m.insert(k, v);
}

/// An `AnyValue` from OTLP/JSON (`{"stringValue": …}`, `{"intValue": "12"}`, …).
fn any_json(v: &Value, depth: usize, budget: &mut Budget, batch: &mut Batch) -> Step<Value> {
    if depth > MAX_DEPTH {
        return Step::Reject(Rejected(format!("values nested deeper than {MAX_DEPTH}")));
    }
    step!(budget.value(0));
    let Some(m) = v.as_object() else { return Step::Ok(Value::Null) };
    let get = |a: &str, b: &str| m.get(a).or_else(|| m.get(b));
    if let Some(s) = get("stringValue", "string_value") {
        let s = s.as_str().unwrap_or_default();
        step!(budget.bytes(s.len().min(MAX_VALUE_BYTES)));
        return Step::Ok(Value::from(bounded(s, batch)));
    }
    if let Some(b) = get("boolValue", "bool_value") {
        return Step::Ok(Value::Bool(b.as_bool().unwrap_or(false)));
    }
    if let Some(i) = get("intValue", "int_value") {
        // JSON carries 64-bit integers as strings.
        return Step::Ok(Value::from(i.as_i64().or_else(|| i.as_str().and_then(|s| s.parse().ok())).unwrap_or(0)));
    }
    if let Some(d) = get("doubleValue", "double_value") {
        return Step::Ok(d.as_f64().map(Value::from).unwrap_or(Value::Null));
    }
    if let Some(a) = get("arrayValue", "array_value") {
        let mut out = Vec::new();
        for x in a.get("values").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            out.push(step!(any_json(x, depth + 1, budget, batch)));
        }
        return Step::Ok(Value::Arr(out));
    }
    if let Some(kv) = get("kvlistValue", "kvlist_value") {
        let mut out = Map::new();
        for e in kv.get("values").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            if let Some(k) = e.get("key").and_then(Value::as_str) {
                step!(budget.value(k.len().min(MAX_VALUE_BYTES)));
                let v = step!(any_json(e.get("value").unwrap_or(&Value::Null), depth + 1, budget, batch));
                out.insert(short(k, batch), v);
            }
        }
        return Step::Ok(Value::Obj(out));
    }
    if let Some(b) = get("bytesValue", "bytes_value") {
        let s = b.as_str().unwrap_or_default();
        // Base64 is ASCII: anything else isn't a bytes value (and can't be cut by byte).
        if !s.is_ascii() {
            return Step::Reject(Rejected("a bytesValue that isn't base64".into()));
        }
        let cap = MAX_VALUE_BYTES * 4 / 3;
        let kept = &s[..s.len().min(cap - cap % 4)];
        if kept.len() < s.len() {
            batch.truncated += 1;
        }
        step!(budget.bytes(kept.len()));
        return Step::Ok(Value::from(kept));
    }
    Step::Ok(Value::Null)
}

fn attributes_json(list: Option<&Value>, out: &mut Map, dropped: &mut u64, budget: &mut Budget, batch: &mut Batch) -> Step<()> {
    for e in list.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        if let Some(k) = e.get("key").and_then(Value::as_str) {
            step!(budget.value(k.len().min(MAX_VALUE_BYTES)));
            let v = step!(any_json(e.get("value").unwrap_or(&Value::Null), 1, budget, batch));
            add_attribute(out, dropped, short(k, batch), v);
        }
    }
    Step::Ok(())
}

// --- routing and documents ------------------------------------------------------------------

/// A resource's or scope's part of its records' documents.
#[derive(Clone, Debug, Default)]
struct Context {
    attributes: Map,
    dropped: u64,
    schema_url: Option<String>,
    name: Option<String>,
    version: Option<String>,
}

/// A resource or scope as its records use it: the routing attributes, and its document part's
/// JSON rendered once, charged once, and spliced into each record's text (so a record costs its
/// own size, never its context's again). Its charge goes back when the next one replaces it.
#[derive(Debug, Default)]
struct Rendered {
    dataset: Option<String>,
    namespace: Option<String>,
    json: String,
}

/// A context's tree as rendered: its JSON charged, then its tree's charge given back. Its
/// routing attributes are kept made valid, so a record never copies them again.
fn render(c: Context, scope: bool, budget: &mut Budget) -> Result<Rendered, Fail> {
    let pick = |k: &str| c.attributes.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let dataset = pick("data_stream.dataset").map(dataset_base).filter(|d| !d.is_empty());
    let namespace = pick("data_stream.namespace").map(|n| sanitize(n, 100)).filter(|n| !n.is_empty());
    let json = context_json(c, scope).to_json();
    budget.ndjson(json.len())?;
    budget.record_done();
    Ok(Rendered { dataset, namespace, json })
}

/// A rendered context's charge given back (another replaces it).
fn drop_rendered(r: &Result<Rendered, String>, budget: &mut Budget) {
    if let Ok(r) = r {
        budget.sent(r.json.len());
    }
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

/// A dataset made valid, without its `.otel` (lowercased first, so `X.OTEL` is `x`).
fn dataset_base(raw: &str) -> String {
    let lower = raw.to_lowercase();
    sanitize(lower.strip_suffix(".otel").unwrap_or(&lower), 100 - ".otel".len())
}

/// `(dataset with its .otel, namespace)`: the record's own routing attributes, else its
/// scope's, else its resource's (both made valid once, in [`render`]).
fn target(rec: &Record, scope: &Rendered, resource: &Rendered) -> (String, String) {
    let own = |k: &str| rec.attributes.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let base = own("data_stream.dataset")
        .map(dataset_base)
        .filter(|d| !d.is_empty())
        .or_else(|| scope.dataset.clone())
        .or_else(|| resource.dataset.clone())
        .unwrap_or_else(|| "generic".into());
    let namespace = own("data_stream.namespace")
        .map(|n| sanitize(n, 100))
        .filter(|n| !n.is_empty())
        .or_else(|| scope.namespace.clone())
        .or_else(|| resource.namespace.clone())
        .unwrap_or_else(|| "default".into());
    (format!("{base}.otel"), namespace)
}

fn timestamp(nanos: u64) -> String {
    let secs = (nanos / 1_000_000_000) as i64;
    let sub = (nanos % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, sub).unwrap_or_default().format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
}

fn context_json(c: Context, scope: bool) -> Value {
    let mut m = Map::new();
    if scope {
        if let Some(n) = c.name {
            m.insert("name".into(), Value::from(n));
        }
        if let Some(v) = c.version {
            m.insert("version".into(), Value::from(v));
        }
    }
    m.insert("dropped_attributes_count".into(), Value::from(c.dropped.min(u64::from(u32::MAX))));
    m.insert("attributes".into(), Value::Obj(c.attributes));
    if let Some(u) = c.schema_url {
        m.insert("schema_url".into(), Value::from(u));
    }
    Value::Obj(m)
}

/// Appends a record's `create` line and document, or rejects it past [`MAX_TARGETS`].
fn emit(out: &mut Batch, budget: &mut Budget, rec: Record, scope: &Rendered, resource: &Rendered, now_ms: i64) -> Result<(), Fail> {
    let (dataset, namespace) = target(&rec, scope, resource);
    let index = format!("logs-{dataset}-{namespace}");
    if !out.targets.contains(&index) {
        if out.targets.len() >= MAX_TARGETS {
            out.reject(format!("a request writes at most {MAX_TARGETS} data streams"));
            return Ok(());
        }
        out.targets.insert(index.clone());
    }
    let observed = if rec.observed > 0 { rec.observed } else { (now_ms.max(0) as u64) * 1_000_000 };
    let time = if rec.time > 0 { rec.time } else { observed };
    let mut doc = obj! {
        "@timestamp" => timestamp(time),
        "observed_timestamp" => timestamp(observed),
        "data_stream" => obj! { "type" => "logs", "dataset" => dataset, "namespace" => namespace },
    };
    let Value::Obj(d) = &mut doc else { return Ok(()) };
    if let Some(t) = rec.severity_text {
        d.insert("severity_text".into(), Value::from(t));
    }
    if let Some(n) = rec.severity_number.filter(|n| *n > 0) {
        d.insert("severity_number".into(), Value::from(n.min(24)));
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
    d.insert("dropped_attributes_count".into(), Value::from(rec.dropped.min(u64::from(u32::MAX))));
    if let Some(t) = rec.trace_id {
        d.insert("trace_id".into(), Value::from(t));
    }
    if let Some(s) = rec.span_id {
        d.insert("span_id".into(), Value::from(s));
    }
    if let Some(e) = rec.event_name.filter(|e| !e.is_empty()) {
        d.insert("event_name".into(), Value::from(e));
    }
    let action = obj! { "create" => obj! { "_index" => index } }.to_json();
    // The record's own fields, without the closing brace; the resource and scope go in as
    // rendered once ([`Rendered`]), charged before anything is written.
    let mut head = doc.to_json();
    head.pop();
    const RESOURCE: &str = ",\"resource\":";
    const SCOPE: &str = ",\"scope\":";
    let len = action.len() + 1 + head.len() + RESOURCE.len() + resource.json.len() + SCOPE.len() + scope.json.len() + 2;
    budget.ndjson(len)?;
    if !out.current.is_empty() && out.current.len() + len > CHUNK_BYTES {
        out.push_chunk();
    }
    out.current.reserve(len);
    for part in [action.as_str(), "\n", head.as_str(), RESOURCE, resource.json.as_str(), SCOPE, scope.json.as_str(), "}\n"] {
        out.current.push_str(part);
    }
    out.accepted += 1;
    Ok(())
}

// --- protobuf ---------------------------------------------------------------------------------

/// A `Resource` (`attributes = 1`, `dropped_attributes_count = 2`) or an `InstrumentationScope`
/// (`name = 1`, `version = 2`, `attributes = 3`, `dropped_attributes_count = 4`).
fn context_pb(b: &[u8], scope: bool, budget: &mut Budget, batch: &mut Batch) -> Step<Context> {
    let mut c = Context::default();
    let (attrs, dropped) = if scope { (3, 4) } else { (1, 2) };
    for f in Fields::new(b) {
        let (n, w) = wire!(f);
        match n {
            1 if scope => c.name = Some(stop!(text(wire!(w.string()), budget, batch))),
            2 if scope => c.version = Some(stop!(text(wire!(w.string()), budget, batch))),
            n if n == attrs => {
                if let Some((k, v)) = step!(key_value_pb(wire!(w.bytes()), 1, budget, batch)) {
                    add_attribute(&mut c.attributes, &mut c.dropped, k, v);
                }
            }
            n if n == dropped => c.dropped = c.dropped.saturating_add(wire!(w.uint())),
            _ => {}
        }
    }
    Step::Ok(c)
}

/// A `LogRecord`.
fn record_pb(b: &[u8], budget: &mut Budget, batch: &mut Batch) -> Step<Record> {
    let mut r = Record::default();
    for f in Fields::new(b) {
        let (n, w) = wire!(f);
        match n {
            1 => r.time = wire!(w.uint()),
            11 => r.observed = wire!(w.uint()),
            2 => r.severity_number = Some(wire!(w.uint()).min(24) as i64),
            3 => r.severity_text = Some(stop!(text(wire!(w.string()), budget, batch))),
            5 => r.body = Some(step!(any_pb(wire!(w.bytes()), 0, budget, batch))),
            6 => {
                if let Some((k, v)) = step!(key_value_pb(wire!(w.bytes()), 1, budget, batch)) {
                    add_attribute(&mut r.attributes, &mut r.dropped, k, v);
                }
            }
            7 => r.dropped = r.dropped.saturating_add(wire!(w.uint())),
            9 => r.trace_id = hex(wire!(w.bytes()), 16),
            10 => r.span_id = hex(wire!(w.bytes()), 8),
            12 => r.event_name = Some(stop!(text(wire!(w.string()), budget, batch))),
            _ => {}
        }
    }
    Step::Ok(r)
}

/// A context step: a context, a reason its records are all rejected, or a stopped request.
fn context_or(s: Step<Context>) -> Result<Result<Context, String>, Fail> {
    match s {
        Step::Ok(c) => Ok(Ok(c)),
        Step::Reject(Rejected(why)) => Ok(Err(why)),
        Step::Stop(f) => Err(f),
    }
}

/// The documents of an `ExportLogsServiceRequest` in protobuf (`resource_logs = 1`; each
/// `ResourceLogs`: `resource = 1`, `scope_logs = 2`, `schema_url = 3`; each `ScopeLogs`:
/// `scope = 1`, `log_records = 2`, `schema_url = 3`), one record at a time.
pub fn from_protobuf(body: &[u8], now_ms: i64, budget: &mut Budget) -> Result<Batch, Fail> {
    let mut out = Batch::default();
    for f in Fields::new(body) {
        let (n, w) = f?;
        if n != 1 {
            continue;
        }
        let rl = w.bytes()?;
        // The resource and the schema URL may come after the scopes: read them first.
        let mut ctx: Result<Context, String> = Ok(Context::default());
        let mut schema = None;
        for g in Fields::new(rl) {
            let (gn, gw) = g?;
            match gn {
                1 => ctx = context_or(context_pb(gw.bytes()?, false, budget, &mut out))?,
                3 => schema = Some(text(gw.string()?, budget, &mut out)?),
                _ => {}
            }
        }
        let resource = match ctx {
            Ok(mut c) => {
                c.schema_url = schema;
                Ok(render(c, false, budget)?)
            }
            Err(why) => {
                budget.record_done();
                Err(why)
            }
        };
        for g in Fields::new(rl) {
            let (gn, gw) = g?;
            if gn != 2 {
                continue;
            }
            let sl = gw.bytes()?;
            let mut ctx: Result<Context, String> = Ok(Context::default());
            let mut schema = None;
            for h in Fields::new(sl) {
                let (hn, hw) = h?;
                match hn {
                    1 => ctx = context_or(context_pb(hw.bytes()?, true, budget, &mut out))?,
                    3 => schema = Some(text(hw.string()?, budget, &mut out)?),
                    _ => {}
                }
            }
            let scope = match ctx {
                Ok(mut c) => {
                    c.schema_url = schema;
                    Ok(render(c, true, budget)?)
                }
                Err(why) => {
                    budget.record_done();
                    Err(why)
                }
            };
            for h in Fields::new(sl) {
                let (hn, hw) = h?;
                if hn != 2 {
                    continue;
                }
                let rec = record_pb(hw.bytes()?, budget, &mut out);
                match (rec, &resource, &scope) {
                    (Step::Stop(f), _, _) => return Err(f),
                    (_, Err(why), _) | (_, _, Err(why)) => out.reject(why.clone()),
                    (Step::Reject(Rejected(why)), _, _) => out.reject(why),
                    (Step::Ok(r), Ok(res), Ok(sc)) => emit(&mut out, budget, r, sc, res, now_ms)?,
                }
                budget.record_done();
            }
            drop_rendered(&scope, budget);
        }
        drop_rendered(&resource, budget);
    }
    Ok(out.finish())
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

fn str_json(v: Option<&Value>, budget: &mut Budget, batch: &mut Batch) -> Result<Option<String>, Fail> {
    v.and_then(Value::as_str).map(|s| text(s, budget, batch)).transpose()
}

fn context_json_in(v: Option<&Value>, schema_url: Option<&Value>, scope: bool, budget: &mut Budget, batch: &mut Batch) -> Step<Context> {
    let mut c = Context { schema_url: stop!(str_json(schema_url, budget, batch)), ..Context::default() };
    let Some(v) = v else { return Step::Ok(c) };
    if scope {
        c.name = stop!(str_json(v.get("name"), budget, batch));
        c.version = stop!(str_json(v.get("version"), budget, batch));
    }
    step!(attributes_json(v.get("attributes"), &mut c.attributes, &mut c.dropped, budget, batch));
    c.dropped = c.dropped.saturating_add(uint_json(get(v, "droppedAttributesCount", "dropped_attributes_count")));
    Step::Ok(c)
}

/// A hex id of `len` bytes from JSON, lowercased; left out at any other length or all zeros.
fn hex_json(v: Option<&Value>, len: usize) -> Option<String> {
    let s = v.and_then(Value::as_str)?;
    (s.len() == len * 2 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0')).then(|| s.to_ascii_lowercase())
}

fn record_json(v: &Value, budget: &mut Budget, batch: &mut Batch) -> Step<Record> {
    let severity_text = stop!(str_json(get(v, "severityText", "severity_text"), budget, batch));
    let event_name = stop!(str_json(get(v, "eventName", "event_name"), budget, batch));
    let mut r = Record {
        time: uint_json(get(v, "timeUnixNano", "time_unix_nano")),
        observed: uint_json(get(v, "observedTimeUnixNano", "observed_time_unix_nano")),
        severity_number: get(v, "severityNumber", "severity_number").and_then(|s| s.as_i64().or_else(|| s.as_str().and_then(severity_by_name))).map(|n| n.clamp(0, 24)),
        severity_text,
        trace_id: hex_json(get(v, "traceId", "trace_id"), 16),
        span_id: hex_json(get(v, "spanId", "span_id"), 8),
        event_name,
        ..Record::default()
    };
    if let Some(b) = v.get("body") {
        r.body = Some(step!(any_json(b, 0, budget, batch)));
    }
    step!(attributes_json(v.get("attributes"), &mut r.attributes, &mut r.dropped, budget, batch));
    r.dropped = r.dropped.saturating_add(uint_json(get(v, "droppedAttributesCount", "dropped_attributes_count")));
    Step::Ok(r)
}

/// `SEVERITY_NUMBER_INFO` and the like, as some JSON encoders write the enum.
fn severity_by_name(s: &str) -> Option<i64> {
    let base = |p: &str, n: i64| s.strip_prefix("SEVERITY_NUMBER_").and_then(|x| x.strip_prefix(p)).map(|d| n + d.parse::<i64>().map_or(0, |k| k.saturating_sub(1).clamp(0, 3)));
    base("TRACE", 1).or_else(|| base("DEBUG", 5)).or_else(|| base("INFO", 9)).or_else(|| base("WARN", 13)).or_else(|| base("ERROR", 17)).or_else(|| base("FATAL", 21))
}

/// The documents of an `ExportLogsServiceRequest` in OTLP/JSON, read from its parsed tree
/// (borrowed, never copied).
pub fn from_json(body: &Value, now_ms: i64, budget: &mut Budget) -> Result<Batch, Fail> {
    let mut out = Batch::default();
    let Some(list) = get(body, "resourceLogs", "resource_logs") else { return Ok(out.finish()) };
    let Some(list) = list.as_array() else { return Err(Fail::Malformed("resourceLogs isn't a list".into())) };
    for rl in list {
        let resource = match context_or(context_json_in(rl.get("resource"), get(rl, "schemaUrl", "schema_url"), false, budget, &mut out))? {
            Ok(c) => Ok(render(c, false, budget)?),
            Err(why) => {
                budget.record_done();
                Err(why)
            }
        };
        for sl in get(rl, "scopeLogs", "scope_logs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            let scope = match context_or(context_json_in(sl.get("scope"), get(sl, "schemaUrl", "schema_url"), true, budget, &mut out))? {
                Ok(c) => Ok(render(c, true, budget)?),
                Err(why) => {
                    budget.record_done();
                    Err(why)
                }
            };
            for r in get(sl, "logRecords", "log_records").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                let rec = record_json(r, budget, &mut out);
                match (rec, &resource, &scope) {
                    (Step::Stop(f), _, _) => return Err(f),
                    (_, Err(why), _) | (_, _, Err(why)) => out.reject(why.clone()),
                    (Step::Reject(Rejected(why)), _, _) => out.reject(why),
                    (Step::Ok(rec), Ok(res), Ok(sc)) => emit(&mut out, budget, rec, sc, res, now_ms)?,
                }
                budget.record_done();
            }
            drop_rendered(&scope, budget);
        }
        drop_rendered(&resource, budget);
    }
    Ok(out.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::MemoryBudget;

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
        let all: String = b.chunks.concat();
        let lines: Vec<&str> = all.lines().collect();
        lines.chunks(2).map(|c| (crate::json::parse(c[0].as_bytes()).unwrap().unwrap(), crate::json::parse(c[1].as_bytes()).unwrap().unwrap())).collect()
    }
    fn budget(limit: u64) -> Budget {
        Budget::new(Reservation::new(MemoryBudget::new(limit)))
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
        let pb = from_protobuf(&request(&[("service.name", "checkout")], &[rec]), 0, &mut budget(u64::MAX)).unwrap();
        let json = crate::json::parse(
            br#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},
                "scopeLogs":[{"scope":{"name":"my.lib","version":"1.2.0"},"logRecords":[{"timeUnixNano":"1700000000123456789","severityNumber":9,
                "severityText":"INFO","body":{"stringValue":"user signed in"},"attributes":[{"key":"http.route","value":{"stringValue":"/login"}},
                {"key":"data_stream.dataset","value":{"stringValue":"Web-App"}}],"traceId":"ABABABABABABABABABABABABABABABAB","spanId":"0101010101010101"}]}]}]}"#,
        )
        .unwrap()
        .unwrap();
        let js = from_json(&json, 0, &mut budget(u64::MAX)).unwrap();
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
    fn routing_is_made_valid_as_the_exporter_does() {
        let route = |ds: &str, ns: &str| {
            let rec = [field(5, &field(1, b"x")), field(6, &string_kv("data_stream.dataset", ds)), field(6, &string_kv("data_stream.namespace", ns))].concat();
            let b = from_protobuf(&request(&[], &[rec]), 1, &mut budget(u64::MAX)).unwrap();
            docs(&b)[0].0.pointer(&["create", "_index"]).and_then(Value::as_str).unwrap().to_string()
        };
        assert_eq!(route("nginx.otel", "Prod"), "logs-nginx.otel-prod");
        assert_eq!(route("a/b:c-d", "x y"), "logs-a_b_c_d.otel-x_y");
        assert_eq!(route("_internal", "default"), "logs-_internal.otel-default");
        assert_eq!(route("Nginx.OTEL", "default"), "logs-nginx.otel-default");
        let long = "d".repeat(300);
        assert_eq!(route(&long, "n").len(), "logs-".len() + 100 + "-n".len());
        // The routing attribute counts even past the attribute cap.
        let mut rec = vec![field(5, &field(1, b"x"))];
        for i in 0..200 {
            rec.push(field(6, &string_kv(&format!("k{i}"), "v")));
        }
        rec.push(field(6, &string_kv("data_stream.dataset", "late")));
        let b = from_protobuf(&request(&[], &[rec.concat()]), 1, &mut budget(u64::MAX)).unwrap();
        let (action, doc) = &docs(&b)[0];
        assert_eq!(action.pointer(&["create", "_index"]).and_then(Value::as_str), Some("logs-late.otel-default"));
        assert_eq!(doc.get("dropped_attributes_count").and_then(Value::as_i64), Some(72));
        // At most MAX_TARGETS data streams a request; the rest rejected.
        let recs: Vec<Vec<u8>> = (0..MAX_TARGETS + 5).map(|i| [field(5, &field(1, b"x")), field(6, &string_kv("data_stream.dataset", &format!("d{i}")))].concat()).collect();
        let b = from_protobuf(&request(&[], &recs), 1, &mut budget(u64::MAX)).unwrap();
        assert_eq!((b.accepted, b.rejected), (MAX_TARGETS as u64, 5));
    }

    #[test]
    fn a_wide_value_is_stopped_before_it_is_built() {
        // One record whose body is an array of 1M empty values (2 bytes each on the wire).
        let mut arr = Vec::new();
        for _ in 0..1_000_000 {
            arr.extend(field(1, &[]));
        }
        let rec = field(5, &field(5, &arr));
        let body = request(&[], &[rec]);
        // Past MAX_VALUES the record is rejected, having built at most MAX_VALUES values.
        let mut b = budget(u64::MAX);
        let out = from_protobuf(&body, 1, &mut b).unwrap();
        assert_eq!((out.accepted, out.rejected), (0, 1));
        assert!(out.reason.unwrap().contains("values"));
        // Under a small breaker the request stops before it builds what it can't hold.
        let small = MemoryBudget::new(64 * 1024);
        let mut b = Budget::new(Reservation::new(small.clone()));
        assert!(matches!(from_protobuf(&body, 1, &mut b), Err(Fail::Tripped(_))));
        // The JSON form too, borrowed rather than copied.
        let mut text = String::from(r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"body":{"arrayValue":{"values":["#);
        for i in 0..50_000 {
            text.push_str(if i == 0 { "{}" } else { ",{}" });
        }
        text.push_str("]}}}]}]}]}");
        let v = crate::json::parse(text.as_bytes()).unwrap().unwrap();
        let out = from_json(&v, 1, &mut budget(u64::MAX)).unwrap();
        assert_eq!(out.rejected, 1);
        // Values past the cut are kept cut, and counted.
        let long = field(5, &field(1, &vec![b'a'; MAX_VALUE_BYTES + 10]));
        let out = from_protobuf(&request(&[], &[long]), 1, &mut budget(u64::MAX)).unwrap();
        assert_eq!((out.accepted, out.truncated), (1, 1));
        // A length past the bytes present fails the request.
        assert!(matches!(from_protobuf(&[0x0a, 0xff, 0x01, 0x00], 1, &mut budget(u64::MAX)), Err(Fail::Malformed(_))));
    }

    #[test]
    fn a_non_base64_bytes_value_is_refused_not_cut_mid_character() {
        let mut text = String::from(r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"body":{"bytesValue":""#);
        // One ASCII byte first: the old cut (at an even byte) then fell inside a character.
        text.push('a');
        text.push_str(&"é".repeat(800_000));
        text.push_str(r#""}},{"body":{"stringValue":"fine"}}]}]}]}"#);
        let v = crate::json::parse(text.as_bytes()).unwrap().unwrap();
        let out = from_json(&v, 1, &mut budget(u64::MAX)).unwrap();
        assert_eq!((out.accepted, out.rejected), (1, 1));
        assert!(out.reason.unwrap().contains("base64"));
    }

    #[test]
    fn a_requests_charge_is_its_ndjson_and_its_contexts_once() {
        // A large resource over two scopes of a few records: each record's values and each
        // context's tree go back once used, and each rendered context once its records are
        // written, so what stays reserved is the NDJSON (which carries the contexts' text).
        let big = "r".repeat(200_000);
        let resource = field(1, &string_kv("service.name", &big));
        let scope = |name: &[u8]| {
            let mut sl = field(1, &[field(1, name), field(3, &string_kv("scope.big", &big))].concat());
            for i in 0..3 {
                sl.extend(field(2, &field(5, &field(1, format!("record {i}").as_bytes()))));
            }
            sl
        };
        let rl = [field(1, &resource), field(2, &scope(b"a")), field(2, &scope(b"b"))].concat();
        let body = field(1, &rl);
        let limit = MemoryBudget::new(u64::MAX);
        let mut b = Budget::new(Reservation::new(limit.clone()));
        let out = from_protobuf(&body, 1, &mut b).unwrap();
        let ndjson: usize = out.chunks.iter().map(String::len).sum();
        assert_eq!(out.accepted, 6);
        assert!(limit.used() <= ndjson as u64 + STEP, "{} reserved for {ndjson} of NDJSON", limit.used());
    }

    #[test]
    fn deep_values_and_bad_ids_are_refused() {
        let mut deep = field(1, b"leaf");
        for _ in 0..20 {
            deep = field(5, &field(1, &deep));
        }
        let ok = [field(5, &field(1, b"fine")), field(9, &[1; 15]), field(10, &[2; 8])].concat();
        let b = from_protobuf(&request(&[], &[field(5, &deep), ok]), 1, &mut budget(u64::MAX)).unwrap();
        assert_eq!((b.accepted, b.rejected), (1, 1));
        let (_, doc) = &docs(&b)[0];
        assert!(doc.get("trace_id").is_none(), "a 15-byte trace id is left out");
        assert_eq!(doc.get("span_id").and_then(Value::as_str), Some("0202020202020202"));
    }
}

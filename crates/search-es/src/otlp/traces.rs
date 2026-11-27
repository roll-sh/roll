//! OTLP traces (`ExportTraceServiceRequest`, protobuf or JSON) as the documents Elastic's managed
//! OTLP endpoint writes in OTel-native form, for the built-in `traces-otel@template`:
//!
//! `@timestamp` (the span's start), `duration` (nanoseconds), `name`, `kind` (`Internal`,
//! `Server`, `Client`, `Producer`, `Consumer`, `Unspecified`), `trace_id`, `span_id`,
//! `parent_span_id`, `trace_state` (hex ids, as logs'), `status.{code, message}` (`Unset`, `Ok`,
//! `Error`), `attributes`, `dropped_attributes_count`, `dropped_events_count`,
//! `dropped_links_count`, `links[].{trace_id, span_id, trace_state, attributes,
//! dropped_attributes_count}`, `resource.*`, `scope.*`, `data_stream.*`, into
//! `traces-<dataset>.otel-<namespace>`, routed as logs are ([`super::logs`]).
//!
//! A span's events become log documents (`event_name`, `@timestamp`, `attributes`, the span's
//! `trace_id` and `span_id`) in `logs-<dataset>.otel-<namespace>`, as Elastic's exporter writes
//! them: an exception recorded on a span is a searchable log line.
//!
//! Everything a span holds is charged as it's built, as a log record's is ([`super::logs::Budget`]);
//! a span with more than [`super::logs::MAX_VALUES`] values, counting its events' and links', is
//! rejected.

use super::logs::{
    Batch, Budget, Context, Fail, Rejected, Rendered, Step, add_attribute, admit, attributes_json, context_json_in, context_or, context_pb, drop_rendered, get,
    hex, hex_json, key_value_pb, push_doc, render, str_json, target_from, text, timestamp, uint_json,
};
use super::wire::Fields;
use crate::json::{Map, Value};
use crate::obj;

#[derive(Debug, Default)]
struct Event {
    time: u64,
    name: Option<String>,
    attributes: Map,
    dropped: u64,
}

#[derive(Debug, Default)]
struct Link {
    trace_id: Option<String>,
    span_id: Option<String>,
    trace_state: Option<String>,
    attributes: Map,
    dropped: u64,
}

#[derive(Debug, Default)]
struct Span {
    trace_id: Option<String>,
    span_id: Option<String>,
    parent_span_id: Option<String>,
    trace_state: Option<String>,
    name: Option<String>,
    kind: i64,
    start: u64,
    end: u64,
    attributes: Map,
    dropped: u64,
    events: Vec<Event>,
    dropped_events: u64,
    links: Vec<Link>,
    dropped_links: u64,
    status_code: i64,
    status_message: Option<String>,
}

/// `SpanKind` as the exporter prints it.
fn kind_name(k: i64) -> &'static str {
    match k {
        1 => "Internal",
        2 => "Server",
        3 => "Client",
        4 => "Producer",
        5 => "Consumer",
        _ => "Unspecified",
    }
}

/// `Status.StatusCode` as the exporter prints it.
fn status_name(c: i64) -> &'static str {
    match c {
        1 => "Ok",
        2 => "Error",
        _ => "Unset",
    }
}

/// `SPAN_KIND_SERVER`-style enum names, as some JSON encoders write them.
fn kind_by_name(s: &str) -> i64 {
    match s.strip_prefix("SPAN_KIND_").unwrap_or(s) {
        "INTERNAL" => 1,
        "SERVER" => 2,
        "CLIENT" => 3,
        "PRODUCER" => 4,
        "CONSUMER" => 5,
        _ => 0,
    }
}

fn status_by_name(s: &str) -> i64 {
    match s.strip_prefix("STATUS_CODE_").unwrap_or(s) {
        "OK" => 1,
        "ERROR" => 2,
        _ => 0,
    }
}

macro_rules! wire_t {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(m) => return Step::Stop(Fail::from(m)),
        }
    };
}

macro_rules! step_t {
    ($e:expr) => {
        match $e {
            Step::Ok(v) => v,
            Step::Reject(r) => return Step::Reject(r),
            Step::Stop(f) => return Step::Stop(f),
        }
    };
}

macro_rules! stop_t {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(f) => return Step::Stop(f),
        }
    };
}

/// Attributes from protobuf `KeyValue`s into `m`.
fn attribute_pb(b: &[u8], m: &mut Map, dropped: &mut u64, budget: &mut Budget, batch: &mut Batch) -> Step<()> {
    if let Some((k, v)) = step_t!(key_value_pb(b, 1, budget, batch)) {
        add_attribute(m, dropped, k, v);
    }
    Step::Ok(())
}

/// A `Span.Event` (`time_unix_nano = 1`, `name = 2`, `attributes = 3`, `dropped_attributes_count = 4`).
fn event_pb(b: &[u8], budget: &mut Budget, batch: &mut Batch) -> Step<Event> {
    let mut e = Event::default();
    for f in Fields::new(b) {
        let (n, w) = wire_t!(f);
        match n {
            1 => e.time = wire_t!(w.uint()),
            2 => e.name = Some(stop_t!(text(wire_t!(w.string()), budget, batch))),
            3 => step_t!(attribute_pb(wire_t!(w.bytes()), &mut e.attributes, &mut e.dropped, budget, batch)),
            4 => e.dropped = e.dropped.saturating_add(wire_t!(w.uint())),
            _ => {}
        }
    }
    Step::Ok(e)
}

/// A `Span.Link` (`trace_id = 1`, `span_id = 2`, `trace_state = 3`, `attributes = 4`,
/// `dropped_attributes_count = 5`).
fn link_pb(b: &[u8], budget: &mut Budget, batch: &mut Batch) -> Step<Link> {
    let mut l = Link::default();
    for f in Fields::new(b) {
        let (n, w) = wire_t!(f);
        match n {
            1 => l.trace_id = hex(wire_t!(w.bytes()), 16),
            2 => l.span_id = hex(wire_t!(w.bytes()), 8),
            3 => l.trace_state = Some(stop_t!(text(wire_t!(w.string()), budget, batch))),
            4 => step_t!(attribute_pb(wire_t!(w.bytes()), &mut l.attributes, &mut l.dropped, budget, batch)),
            5 => l.dropped = l.dropped.saturating_add(wire_t!(w.uint())),
            _ => {}
        }
    }
    Step::Ok(l)
}

/// A `Span` (see the module doc for its fields' numbers).
fn span_pb(b: &[u8], budget: &mut Budget, batch: &mut Batch) -> Step<Span> {
    let mut s = Span::default();
    for f in Fields::new(b) {
        let (n, w) = wire_t!(f);
        match n {
            1 => s.trace_id = hex(wire_t!(w.bytes()), 16),
            2 => s.span_id = hex(wire_t!(w.bytes()), 8),
            3 => s.trace_state = Some(stop_t!(text(wire_t!(w.string()), budget, batch))),
            4 => s.parent_span_id = hex(wire_t!(w.bytes()), 8),
            5 => s.name = Some(stop_t!(text(wire_t!(w.string()), budget, batch))),
            6 => s.kind = wire_t!(w.uint()).min(5) as i64,
            7 => s.start = wire_t!(w.uint()),
            8 => s.end = wire_t!(w.uint()),
            9 => step_t!(attribute_pb(wire_t!(w.bytes()), &mut s.attributes, &mut s.dropped, budget, batch)),
            10 => s.dropped = s.dropped.saturating_add(wire_t!(w.uint())),
            11 => {
                step_t!(budget.value(0));
                s.events.push(step_t!(event_pb(wire_t!(w.bytes()), budget, batch)));
            }
            12 => s.dropped_events = s.dropped_events.saturating_add(wire_t!(w.uint())),
            13 => {
                step_t!(budget.value(0));
                s.links.push(step_t!(link_pb(wire_t!(w.bytes()), budget, batch)));
            }
            14 => s.dropped_links = s.dropped_links.saturating_add(wire_t!(w.uint())),
            15 => {
                // Status { message = 2, code = 3 }.
                for g in Fields::new(wire_t!(w.bytes())) {
                    let (gn, gw) = wire_t!(g);
                    match gn {
                        2 => s.status_message = Some(stop_t!(text(wire_t!(gw.string()), budget, batch))),
                        3 => s.status_code = wire_t!(gw.uint()).min(2) as i64,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Step::Ok(s)
}

fn count(n: u64) -> Value {
    Value::from(n.min(u64::from(u32::MAX)))
}

/// Appends a span's document, and its events' log documents.
fn emit(out: &mut Batch, budget: &mut Budget, span: Span, scope: &Rendered, resource: &Rendered, now_ms: i64) -> Result<(), Fail> {
    let (dataset, namespace) = target_from(&span.attributes, scope, resource);
    let Some(index) = admit(out, "traces", &dataset, &namespace) else { return Ok(()) };
    let start = if span.start > 0 { span.start } else { (now_ms.max(0) as u64) * 1_000_000 };
    let mut doc = obj! {
        "@timestamp" => timestamp(start),
        "data_stream" => obj! { "type" => "traces", "dataset" => dataset.clone(), "namespace" => namespace.clone() },
        "duration" => span.end.saturating_sub(span.start).min(i64::MAX as u64),
        "kind" => kind_name(span.kind),
        "status" => {
            let mut st = obj! { "code" => status_name(span.status_code) };
            if let (Some(m), Value::Obj(o)) = (&span.status_message, &mut st) {
                o.insert("message".into(), Value::from(m.as_str()));
            }
            st
        },
    };
    let Value::Obj(d) = &mut doc else { return Ok(()) };
    for (k, v) in [("name", &span.name), ("trace_id", &span.trace_id), ("span_id", &span.span_id), ("parent_span_id", &span.parent_span_id), ("trace_state", &span.trace_state)] {
        if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
            d.insert(k.into(), Value::from(v.as_str()));
        }
    }
    d.insert("attributes".into(), Value::Obj(span.attributes));
    d.insert("dropped_attributes_count".into(), count(span.dropped));
    d.insert("dropped_events_count".into(), count(span.dropped_events));
    d.insert("dropped_links_count".into(), count(span.dropped_links));
    if !span.links.is_empty() {
        let links: Vec<Value> = span
            .links
            .into_iter()
            .map(|l| {
                let mut o = obj! { "attributes" => Value::Obj(l.attributes), "dropped_attributes_count" => count(l.dropped) };
                if let Value::Obj(m) = &mut o {
                    for (k, v) in [("trace_id", l.trace_id), ("span_id", l.span_id), ("trace_state", l.trace_state)] {
                        if let Some(v) = v.filter(|v| !v.is_empty()) {
                            m.insert(k.into(), Value::from(v));
                        }
                    }
                }
                o
            })
            .collect();
        d.insert("links".into(), Value::Arr(links));
    }
    push_doc(out, budget, &index, doc, scope, resource)?;
    // Events: log documents beside the span, routed by the span's data stream.
    for e in span.events {
        let Some(index) = admit(out, "logs", &dataset, &namespace) else { continue };
        let time = if e.time > 0 { e.time } else { start };
        let mut doc = obj! {
            "@timestamp" => timestamp(time),
            "observed_timestamp" => timestamp(time),
            "data_stream" => obj! { "type" => "logs", "dataset" => dataset.clone(), "namespace" => namespace.clone() },
            "attributes" => Value::Obj(e.attributes),
            "dropped_attributes_count" => count(e.dropped),
        };
        if let Value::Obj(d) = &mut doc {
            if let Some(n) = e.name.filter(|n| !n.is_empty()) {
                d.insert("event_name".into(), Value::from(n));
            }
            for (k, v) in [("trace_id", &span.trace_id), ("span_id", &span.span_id)] {
                if let Some(v) = v {
                    d.insert(k.into(), Value::from(v.as_str()));
                }
            }
        }
        push_doc(out, budget, &index, doc, scope, resource)?;
    }
    Ok(())
}

/// The documents of an `ExportTraceServiceRequest` in protobuf (`resource_spans = 1`; each
/// `ResourceSpans`: `resource = 1`, `scope_spans = 2`, `schema_url = 3`; each `ScopeSpans`:
/// `scope = 1`, `spans = 2`, `schema_url = 3`), one span at a time.
pub fn from_protobuf(body: &[u8], now_ms: i64, budget: &mut Budget) -> Result<Batch, Fail> {
    let mut out = Batch::default();
    for f in Fields::new(body) {
        let (n, w) = f?;
        if n != 1 {
            continue;
        }
        let rs = w.bytes()?;
        let mut ctx: Result<Context, String> = Ok(Context::default());
        let mut schema = None;
        for g in Fields::new(rs) {
            let (gn, gw) = g?;
            match gn {
                1 => ctx = context_or(context_pb(gw.bytes()?, false, budget, &mut out))?,
                3 => schema = Some(text(gw.string()?, budget, &mut out)?),
                _ => {}
            }
        }
        let resource = finish(ctx, schema, false, budget)?;
        for g in Fields::new(rs) {
            let (gn, gw) = g?;
            if gn != 2 {
                continue;
            }
            let ss = gw.bytes()?;
            let mut ctx: Result<Context, String> = Ok(Context::default());
            let mut schema = None;
            for h in Fields::new(ss) {
                let (hn, hw) = h?;
                match hn {
                    1 => ctx = context_or(context_pb(hw.bytes()?, true, budget, &mut out))?,
                    3 => schema = Some(text(hw.string()?, budget, &mut out)?),
                    _ => {}
                }
            }
            let scope = finish(ctx, schema, true, budget)?;
            for h in Fields::new(ss) {
                let (hn, hw) = h?;
                if hn != 2 {
                    continue;
                }
                let span = span_pb(hw.bytes()?, budget, &mut out);
                take(&mut out, budget, span, &resource, &scope, now_ms)?;
                budget.record_done();
            }
            drop_rendered(&scope, budget);
        }
        drop_rendered(&resource, budget);
    }
    Ok(out.finish())
}

/// A decoded context rendered (with its schema URL), or its rejection.
fn finish(ctx: Result<Context, String>, schema: Option<String>, scope: bool, budget: &mut Budget) -> Result<Result<Rendered, String>, Fail> {
    match ctx {
        Ok(mut c) => {
            if schema.is_some() {
                c.schema_url = schema;
            }
            Ok(Ok(render(c, scope, budget)?))
        }
        Err(why) => {
            budget.record_done();
            Ok(Err(why))
        }
    }
}

fn take(out: &mut Batch, budget: &mut Budget, span: Step<Span>, resource: &Result<Rendered, String>, scope: &Result<Rendered, String>, now_ms: i64) -> Result<(), Fail> {
    match (span, resource, scope) {
        (Step::Stop(f), _, _) => Err(f),
        (_, Err(why), _) | (_, _, Err(why)) => {
            out.reject(why.clone());
            Ok(())
        }
        (Step::Reject(Rejected(why)), _, _) => {
            out.reject(why);
            Ok(())
        }
        (Step::Ok(s), Ok(res), Ok(sc)) => emit(out, budget, s, sc, res, now_ms),
    }
}

fn attributes_of(v: &Value, m: &mut Map, dropped: &mut u64, budget: &mut Budget, batch: &mut Batch) -> Step<()> {
    step_t!(attributes_json(v.get("attributes"), m, dropped, budget, batch));
    *dropped = dropped.saturating_add(uint_json(get(v, "droppedAttributesCount", "dropped_attributes_count")));
    Step::Ok(())
}

fn span_json(v: &Value, budget: &mut Budget, batch: &mut Batch) -> Step<Span> {
    let enum_of = |x: Option<&Value>, by_name: fn(&str) -> i64| x.and_then(|k| k.as_i64().or_else(|| k.as_str().map(by_name))).unwrap_or(0);
    let mut s = Span {
        trace_id: hex_json(get(v, "traceId", "trace_id"), 16),
        span_id: hex_json(get(v, "spanId", "span_id"), 8),
        parent_span_id: hex_json(get(v, "parentSpanId", "parent_span_id"), 8),
        trace_state: stop_t!(str_json(get(v, "traceState", "trace_state"), budget, batch)),
        name: stop_t!(str_json(v.get("name"), budget, batch)),
        kind: enum_of(v.get("kind"), kind_by_name).clamp(0, 5),
        start: uint_json(get(v, "startTimeUnixNano", "start_time_unix_nano")),
        end: uint_json(get(v, "endTimeUnixNano", "end_time_unix_nano")),
        dropped_events: uint_json(get(v, "droppedEventsCount", "dropped_events_count")),
        dropped_links: uint_json(get(v, "droppedLinksCount", "dropped_links_count")),
        ..Span::default()
    };
    step_t!(attributes_of(v, &mut s.attributes, &mut s.dropped, budget, batch));
    for e in v.get("events").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        step_t!(budget.value(0));
        let mut ev = Event { time: uint_json(get(e, "timeUnixNano", "time_unix_nano")), name: stop_t!(str_json(e.get("name"), budget, batch)), ..Event::default() };
        step_t!(attributes_of(e, &mut ev.attributes, &mut ev.dropped, budget, batch));
        s.events.push(ev);
    }
    for l in v.get("links").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        step_t!(budget.value(0));
        let mut lk = Link {
            trace_id: hex_json(get(l, "traceId", "trace_id"), 16),
            span_id: hex_json(get(l, "spanId", "span_id"), 8),
            trace_state: stop_t!(str_json(get(l, "traceState", "trace_state"), budget, batch)),
            ..Link::default()
        };
        step_t!(attributes_of(l, &mut lk.attributes, &mut lk.dropped, budget, batch));
        s.links.push(lk);
    }
    if let Some(st) = v.get("status") {
        s.status_message = stop_t!(str_json(st.get("message"), budget, batch));
        s.status_code = enum_of(st.get("code"), status_by_name).clamp(0, 2);
    }
    Step::Ok(s)
}

/// The documents of an `ExportTraceServiceRequest` in OTLP/JSON, from its parsed tree.
pub fn from_json(body: &Value, now_ms: i64, budget: &mut Budget) -> Result<Batch, Fail> {
    let mut out = Batch::default();
    let Some(list) = get(body, "resourceSpans", "resource_spans") else { return Ok(out.finish()) };
    let Some(list) = list.as_array() else { return Err(Fail::Malformed("resourceSpans isn't a list".into())) };
    for rs in list {
        let ctx = context_or(context_json_in(rs.get("resource"), get(rs, "schemaUrl", "schema_url"), false, budget, &mut out))?;
        let resource = finish(ctx, None, false, budget)?;
        for ss in get(rs, "scopeSpans", "scope_spans").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            let ctx = context_or(context_json_in(ss.get("scope"), get(ss, "schemaUrl", "schema_url"), true, budget, &mut out))?;
            let scope = finish(ctx, None, true, budget)?;
            for sp in ss.get("spans").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                let span = span_json(sp, budget, &mut out);
                take(&mut out, budget, span, &resource, &scope, now_ms)?;
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
    use crate::breaker::{MemoryBudget, Reservation};

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
    fn budget() -> Budget {
        Budget::new(Reservation::new(MemoryBudget::new(u64::MAX)))
    }
    fn docs(b: &Batch) -> Vec<(Value, Value)> {
        let all: String = b.chunks.concat();
        let lines: Vec<&str> = all.lines().collect();
        lines.chunks(2).map(|c| (crate::json::parse(c[0].as_bytes()).unwrap().unwrap(), crate::json::parse(c[1].as_bytes()).unwrap().unwrap())).collect()
    }

    #[test]
    fn protobuf_and_json_spans_become_the_same_otel_documents_and_events_logs() {
        let event = [fixed64(1, 1_700_000_000_500_000_000), field(2, b"exception"), field(3, &string_kv("exception.type", "Timeout"))].concat();
        let link = [field(1, &[0x22; 16]), field(2, &[0x33; 8])].concat();
        let span = [
            field(1, &[0xab; 16]),
            field(2, &[0x01; 8]),
            field(4, &[0x02; 8]),
            field(5, b"GET /cart"),
            uint(6, 2),
            fixed64(7, 1_700_000_000_000_000_000),
            fixed64(8, 1_700_000_000_250_000_000),
            field(9, &string_kv("http.route", "/cart")),
            field(11, &event),
            field(13, &link),
            field(15, &[field(2, b"boom"), uint(3, 2)].concat()),
        ]
        .concat();
        let resource = field(1, &string_kv("service.name", "checkout"));
        let ss = [field(1, &field(1, b"lib")), field(2, &span)].concat();
        let body = field(1, &[field(1, &resource), field(2, &ss)].concat());
        let pb = from_protobuf(&body, 0, &mut budget()).unwrap();
        let json = crate::json::parse(
            br#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},"scopeSpans":[{"scope":{"name":"lib"},"spans":[{
                "traceId":"abababababababababababababababab","spanId":"0101010101010101","parentSpanId":"0202020202020202","name":"GET /cart","kind":"SPAN_KIND_SERVER",
                "startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000250000000","attributes":[{"key":"http.route","value":{"stringValue":"/cart"}}],
                "events":[{"timeUnixNano":"1700000000500000000","name":"exception","attributes":[{"key":"exception.type","value":{"stringValue":"Timeout"}}]}],
                "links":[{"traceId":"22222222222222222222222222222222","spanId":"3333333333333333"}],"status":{"message":"boom","code":2}}]}]}]}"#,
        )
        .unwrap()
        .unwrap();
        let js = from_json(&json, 0, &mut budget()).unwrap();
        let (a, b) = (docs(&pb), docs(&js));
        assert_eq!(a, b);
        assert_eq!(a.len(), 2, "the span and its event");
        let (action, span) = &a[0];
        assert_eq!(action.pointer(&["create", "_index"]).and_then(Value::as_str), Some("traces-generic.otel-default"));
        assert_eq!(span.get("duration").and_then(Value::as_i64), Some(250_000_000));
        assert_eq!(span.get("kind").and_then(Value::as_str), Some("Server"));
        assert_eq!(span.pointer(&["status", "code"]).and_then(Value::as_str), Some("Error"));
        assert_eq!(span.get("parent_span_id").and_then(Value::as_str), Some("0202020202020202"));
        assert_eq!(span.pointer(&["links"]).and_then(Value::as_array).map(Vec::len), Some(1));
        assert_eq!(span.pointer(&["resource", "attributes", "service.name"]).and_then(Value::as_str), Some("checkout"));
        let (action, event) = &a[1];
        assert_eq!(action.pointer(&["create", "_index"]).and_then(Value::as_str), Some("logs-generic.otel-default"));
        assert_eq!(event.get("event_name").and_then(Value::as_str), Some("exception"));
        assert_eq!(event.get("trace_id").and_then(Value::as_str), Some("abababababababababababababababab"));
        assert_eq!(event.pointer(&["attributes", "exception.type"]).and_then(Value::as_str), Some("Timeout"));
    }

    #[test]
    fn a_span_with_too_many_events_is_rejected_before_it_is_built() {
        let mut span = vec![field(5, b"s")];
        for _ in 0..(super::super::logs::MAX_VALUES + 5) {
            span.push(field(11, &[]));
        }
        let body = field(1, &field(2, &field(2, &span.concat())));
        let out = from_protobuf(&body, 1, &mut budget()).unwrap();
        assert_eq!((out.accepted, out.rejected), (0, 1));
    }
}

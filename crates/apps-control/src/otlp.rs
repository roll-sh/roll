//! OpenTelemetry in (OTLP/HTTP, `POST …/v1/traces`, `/v1/logs`, `/v1/metrics`): the export
//! requests as protobuf (what most SDKs send) or as JSON, read into the few fields roll keeps of
//! a span, a log record and a metric's data point. The protobuf reader is a small one of its own
//! (no generated code): it walks the fields it needs and skips the rest, every length bounded by
//! the bytes actually present.

use serde_json::Value;

/// A span, as roll keeps it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    /// Hex, 32 characters.
    pub trace: String,
    /// Hex, 16 characters.
    pub span: String,
    /// Empty for a root span.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    pub name: String,
    /// The emitting service's `service.name` (its resource's).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service: String,
    /// Unix nanoseconds.
    pub start: u64,
    pub end: u64,
    /// `error` when its status said so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
    /// Its string, number and boolean attributes (at most 32, each value at most 256 characters).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<(String, String)>,
}

/// Spans, log records or data points one request may carry, and attributes one keeps.
const MAX_SPANS: usize = 10_000;
const MAX_ATTRS: usize = 32;
/// A log record's body, at most.
const MAX_BODY: usize = 8192;
/// A data point's attributes kept (they name its series).
const MAX_POINT_ATTRS: usize = 8;

/// A log record, as roll keeps it (a line of the environment's log).
#[derive(Clone, Debug, PartialEq)]
pub struct LogRecord {
    /// Unix nanoseconds (its own time, else when it was observed).
    pub time: u64,
    /// `trace`, `debug`, `info`, `warn`, `error` or `fatal` (from its severity), or empty.
    pub level: String,
    pub body: String,
    pub trace: String,
    pub span: String,
    pub service: String,
    pub attrs: Vec<(String, String)>,
}

/// A metric's data point, as roll keeps it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Point {
    pub name: String,
    /// `gauge`, `sum` or `histogram`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Unix nanoseconds.
    pub time: u64,
    /// A gauge's or sum's value; a histogram's sum.
    pub value: f64,
    /// A histogram's count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// A sum that counts up from its start (cumulative, monotonic): its rate is what to show.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub counter: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<(String, String)>,
}

/// A severity number's level (OTLP's ranges of four).
fn level_of(n: u64) -> &'static str {
    match n {
        1..=4 => "trace",
        5..=8 => "debug",
        9..=12 => "info",
        13..=16 => "warn",
        17..=20 => "error",
        21..=24 => "fatal",
        _ => "",
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ── protobuf ──

struct Reader<'a> {
    b: &'a [u8],
}

enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32,
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> anyhow::Result<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let (&byte, rest) = self.b.split_first().ok_or_else(|| anyhow::anyhow!("a varint runs off the end"))?;
            self.b = rest;
            v |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        anyhow::bail!("a varint over 10 bytes")
    }

    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(n <= self.b.len(), "a field longer than what's left");
        let (a, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(a)
    }

    /// The next field (its number and value), none at the end.
    fn next(&mut self) -> anyhow::Result<Option<(u64, Field<'a>)>> {
        if self.b.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let value = match key & 7 {
            0 => Field::Varint(self.varint()?),
            1 => Field::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes"))),
            2 => {
                let n = usize::try_from(self.varint()?).map_err(|_| anyhow::anyhow!("a length too big"))?;
                Field::Bytes(self.take(n)?)
            }
            5 => {
                self.take(4)?;
                Field::Fixed32
            }
            w => anyhow::bail!("wire type {w} isn't one OTLP uses"),
        };
        Ok(Some((key >> 3, value)))
    }
}

fn fields(b: &[u8]) -> Reader<'_> {
    Reader { b }
}

/// `AnyValue` as text: a string, an int, a double or a bool (others are left out).
fn any_value(b: &[u8]) -> anyhow::Result<Option<String>> {
    // A oneof: its one field says which it is.
    Ok(match fields(b).next()? {
        Some((1, Field::Bytes(s))) => Some(clip(&String::from_utf8_lossy(s), 256)),
        Some((2, Field::Varint(v))) => Some((v != 0).to_string()),
        Some((3, Field::Varint(v))) => Some((v as i64).to_string()),
        Some((4, Field::Fixed64(v))) => Some(f64::from_bits(v).to_string()),
        _ => None,
    })
}

/// `KeyValue`.
fn key_value(b: &[u8]) -> anyhow::Result<Option<(String, String)>> {
    let (mut key, mut value) = (None, None);
    let mut r = fields(b);
    while let Some((n, f)) = r.next()? {
        match (n, f) {
            (1, Field::Bytes(k)) => key = Some(clip(&String::from_utf8_lossy(k), 128)),
            (2, Field::Bytes(v)) => value = any_value(v)?,
            _ => {}
        }
    }
    Ok(key.zip(value))
}

fn span(b: &[u8], service: &str) -> anyhow::Result<Span> {
    let mut s = Span { trace: String::new(), span: String::new(), parent: String::new(), name: String::new(), service: service.to_string(), start: 0, end: 0, error: false, attrs: Vec::new() };
    let mut r = fields(b);
    while let Some((n, f)) = r.next()? {
        match (n, f) {
            (1, Field::Bytes(v)) => s.trace = hex(v),
            (2, Field::Bytes(v)) => s.span = hex(v),
            (4, Field::Bytes(v)) => s.parent = hex(v),
            (5, Field::Bytes(v)) => s.name = clip(&String::from_utf8_lossy(v), 256),
            (7, Field::Fixed64(v)) => s.start = v,
            (8, Field::Fixed64(v)) => s.end = v,
            (9, Field::Bytes(v)) if s.attrs.len() < MAX_ATTRS => s.attrs.extend(key_value(v)?),
            // Status: its code (field 3) 2 is an error.
            (15, Field::Bytes(v)) => {
                let mut st = fields(v);
                while let Some((sn, sf)) = st.next()? {
                    if let (3, Field::Varint(2)) = (sn, sf) {
                        s.error = true;
                    }
                }
            }
            _ => {}
        }
    }
    anyhow::ensure!(s.trace.len() == 32 && s.span.len() == 16 && (s.parent.is_empty() || s.parent.len() == 16), "a span without its trace or span id, or with a malformed parent");
    Ok(s)
}

/// Each item of a protobuf export request (`ResourceSpans`, `ResourceLogs`, `ResourceMetrics`
/// at field 1; their resource at 1, their scopes at 2, and the scope's items at 2), with its
/// resource's `service.name`.
fn each_item<'a>(b: &'a [u8], mut f: impl FnMut(&str, &'a [u8]) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let mut r = fields(b);
    while let Some((n, fl)) = r.next()? {
        let (1, Field::Bytes(rs)) = (n, fl) else { continue };
        let mut service = String::new();
        let mut scopes = Vec::new();
        let mut rr = fields(rs);
        while let Some((rn, rf)) = rr.next()? {
            match (rn, rf) {
                (1, Field::Bytes(res)) => {
                    let mut attrs = fields(res);
                    while let Some((an, af)) = attrs.next()? {
                        if let (1, Field::Bytes(kv)) = (an, af)
                            && let Some((k, v)) = key_value(kv)?
                            && k == "service.name"
                        {
                            service = v;
                        }
                    }
                }
                (2, Field::Bytes(ss)) => scopes.push(ss),
                _ => {}
            }
        }
        for ss in scopes {
            let mut sr = fields(ss);
            while let Some((sn, sf)) = sr.next()? {
                if let (2, Field::Bytes(item)) = (sn, sf) {
                    f(&service, item)?;
                }
            }
        }
    }
    Ok(())
}

/// The spans of a protobuf `ExportTraceServiceRequest`.
pub fn from_protobuf(b: &[u8]) -> anyhow::Result<Vec<Span>> {
    let mut out = Vec::new();
    each_item(b, |service, sp| {
        anyhow::ensure!(out.len() < MAX_SPANS, "over {MAX_SPANS} spans in one request");
        out.push(span(sp, service)?);
        Ok(())
    })?;
    Ok(out)
}

/// The log records of a protobuf `ExportLogsServiceRequest`.
pub fn logs_from_protobuf(b: &[u8]) -> anyhow::Result<Vec<LogRecord>> {
    let mut out = Vec::new();
    each_item(b, |service, lr| {
        anyhow::ensure!(out.len() < MAX_SPANS, "over {MAX_SPANS} log records in one request");
        let mut l = LogRecord { time: 0, level: String::new(), body: String::new(), trace: String::new(), span: String::new(), service: service.to_string(), attrs: Vec::new() };
        let (mut observed, mut text) = (0, String::new());
        let mut r = fields(lr);
        while let Some((n, f)) = r.next()? {
            match (n, f) {
                (1, Field::Fixed64(t)) => l.time = t,
                (11, Field::Fixed64(t)) => observed = t,
                (2, Field::Varint(sev)) => l.level = level_of(sev).to_string(),
                (3, Field::Bytes(t)) => text = clip(&String::from_utf8_lossy(t), 16).to_ascii_lowercase(),
                // A string body whole (to its bound); another kind as its text.
                (5, Field::Bytes(v)) => {
                    l.body = match fields(v).next()? {
                        Some((1, Field::Bytes(s))) => clip(&String::from_utf8_lossy(s), MAX_BODY),
                        _ => any_value(v)?.unwrap_or_default(),
                    }
                }
                (6, Field::Bytes(v)) if l.attrs.len() < MAX_ATTRS => l.attrs.extend(key_value(v)?),
                (9, Field::Bytes(v)) if v.len() == 16 => l.trace = hex(v),
                (10, Field::Bytes(v)) if v.len() == 8 => l.span = hex(v),
                _ => {}
            }
        }
        if l.time == 0 {
            l.time = observed;
        }
        if l.level.is_empty() {
            l.level = normal_level(&text);
        }
        out.push(l);
        Ok(())
    })?;
    Ok(out)
}

/// A severity's text as one of roll's levels.
fn normal_level(t: &str) -> String {
    match t.trim().to_ascii_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "info" | "information" => "info",
        "warn" | "warning" => "warn",
        "error" | "err" => "error",
        "fatal" | "critical" | "crit" => "fatal",
        _ => "",
    }
    .to_string()
}

/// A data point's time, value (a histogram's sum), a histogram's count, and its attributes.
struct Read {
    time: u64,
    value: Option<f64>,
    count: Option<u64>,
    attrs: Vec<(String, String)>,
}

/// A number data point (`as_double` 4 or `as_int` 6; attributes at 7) or a histogram's (count 4,
/// sum 5; attributes at 9).
fn number_point(b: &[u8], attrs_field: u64) -> anyhow::Result<Read> {
    let (mut time, mut value, mut count, mut attrs) = (0, None, None, Vec::new());
    let mut r = fields(b);
    while let Some((n, f)) = r.next()? {
        match (n, f) {
            (3, Field::Fixed64(t)) => time = t,
            (4, Field::Fixed64(v)) if attrs_field == 7 => value = Some(f64::from_bits(v)),
            (6, Field::Fixed64(v)) if attrs_field == 7 => value = Some(v as i64 as f64),
            // A histogram point: its count (4) and sum (5).
            (4, Field::Fixed64(c)) if attrs_field == 9 => count = Some(c),
            (5, Field::Fixed64(v)) if attrs_field == 9 => value = Some(f64::from_bits(v)),
            (n, Field::Bytes(kv)) if n == attrs_field && attrs.len() < MAX_POINT_ATTRS => attrs.extend(key_value(kv)?),
            _ => {}
        }
    }
    Ok(Read { time, value, count, attrs })
}

/// The data points of a protobuf `ExportMetricsServiceRequest`: gauges, sums and histograms
/// (exponential histograms and summaries are left out).
pub fn metrics_from_protobuf(b: &[u8]) -> anyhow::Result<Vec<Point>> {
    let mut out = Vec::new();
    each_item(b, |_, m| {
        let (mut name, mut unit) = (String::new(), String::new());
        let mut kinds = Vec::new();
        let mut r = fields(m);
        while let Some((n, f)) = r.next()? {
            match (n, f) {
                (1, Field::Bytes(v)) => name = clip(&String::from_utf8_lossy(v), 128),
                (3, Field::Bytes(v)) => unit = clip(&String::from_utf8_lossy(v), 32),
                (5, Field::Bytes(v)) => kinds.push(("gauge", v)),
                (7, Field::Bytes(v)) => kinds.push(("sum", v)),
                (9, Field::Bytes(v)) => kinds.push(("histogram", v)),
                _ => {}
            }
        }
        for (kind, body) in kinds {
            // A sum's temporality (2: 2 is cumulative) and monotonicity (3).
            let (mut cumulative, mut monotonic) = (false, false);
            let mut points = Vec::new();
            let mut k = fields(body);
            while let Some((n, f)) = k.next()? {
                match (n, f) {
                    (1, Field::Bytes(p)) => points.push(p),
                    (2, Field::Varint(t)) => cumulative = t == 2,
                    (3, Field::Varint(m)) => monotonic = m != 0,
                    _ => {}
                }
            }
            for p in points {
                anyhow::ensure!(out.len() < MAX_SPANS, "over {MAX_SPANS} data points in one request");
                let Read { time, value, count, attrs } = number_point(p, if kind == "histogram" { 9 } else { 7 })?;
                let Some(value) = value.filter(|v| v.is_finite()) else { continue };
                out.push(Point { name: name.clone(), kind: kind.into(), unit: unit.clone(), time, value, count, counter: kind == "sum" && cumulative && monotonic, attrs });
            }
        }
        Ok(())
    })?;
    Ok(out)
}

// ── JSON (OTLP/HTTP's JSON encoding: ids in hex, nanoseconds as strings or numbers) ──

fn json_u64(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0)
}

fn json_any(v: &Value) -> Option<String> {
    if let Some(s) = v["stringValue"].as_str() {
        return Some(clip(s, 256));
    }
    if let Some(b) = v["boolValue"].as_bool() {
        return Some(b.to_string());
    }
    if !v["intValue"].is_null() {
        return Some(json_u64(&v["intValue"]).to_string());
    }
    v["doubleValue"].as_f64().map(|d| d.to_string())
}

/// The spans of a JSON `ExportTraceServiceRequest`.
pub fn from_json(v: &Value) -> anyhow::Result<Vec<Span>> {
    let mut out = Vec::new();
    for rs in v["resourceSpans"].as_array().into_iter().flatten() {
        let service = rs["resource"]["attributes"].as_array().into_iter().flatten().find(|a| a["key"] == "service.name").and_then(|a| json_any(&a["value"])).unwrap_or_default();
        for ss in rs["scopeSpans"].as_array().into_iter().flatten() {
            for sp in ss["spans"].as_array().into_iter().flatten() {
                anyhow::ensure!(out.len() < MAX_SPANS, "over {MAX_SPANS} spans in one request");
                let id = |k: &str| sp[k].as_str().unwrap_or("").to_ascii_lowercase();
                let s = Span {
                    trace: id("traceId"),
                    span: id("spanId"),
                    parent: id("parentSpanId"),
                    name: clip(sp["name"].as_str().unwrap_or(""), 256),
                    service: service.clone(),
                    start: json_u64(&sp["startTimeUnixNano"]),
                    end: json_u64(&sp["endTimeUnixNano"]),
                    error: sp["status"]["code"] == 2 || sp["status"]["code"] == "STATUS_CODE_ERROR",
                    attrs: sp["attributes"].as_array().into_iter().flatten().take(MAX_ATTRS).filter_map(|a| Some((clip(a["key"].as_str()?, 128), json_any(&a["value"])?))).collect(),
                };
                let hexish = |x: &str, n: usize| x.len() == n && x.bytes().all(|b| b.is_ascii_hexdigit());
                anyhow::ensure!(hexish(&s.trace, 32) && hexish(&s.span, 16) && (s.parent.is_empty() || hexish(&s.parent, 16)), "a span without its trace or span id, or with a malformed parent");
                out.push(s);
            }
        }
    }
    Ok(out)
}

fn json_attrs(v: &Value, max: usize) -> Vec<(String, String)> {
    v.as_array().into_iter().flatten().take(max).filter_map(|a| Some((clip(a["key"].as_str()?, 128), json_any(&a["value"])?))).collect()
}

fn json_service(rs: &Value) -> String {
    rs["resource"]["attributes"].as_array().into_iter().flatten().find(|a| a["key"] == "service.name").and_then(|a| json_any(&a["value"])).unwrap_or_default()
}

/// The log records of a JSON `ExportLogsServiceRequest`.
pub fn logs_from_json(v: &Value) -> anyhow::Result<Vec<LogRecord>> {
    let mut out = Vec::new();
    for rl in v["resourceLogs"].as_array().into_iter().flatten() {
        let service = json_service(rl);
        for sl in rl["scopeLogs"].as_array().into_iter().flatten() {
            for r in sl["logRecords"].as_array().into_iter().flatten() {
                anyhow::ensure!(out.len() < MAX_SPANS, "over {MAX_SPANS} log records in one request");
                let id = |k: &str, n: usize| r[k].as_str().map(str::to_ascii_lowercase).filter(|x| x.len() == n && x.bytes().all(|b| b.is_ascii_hexdigit())).unwrap_or_default();
                let sev = r["severityNumber"].as_u64().or_else(|| r["severityNumber"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
                let level = match level_of(sev) {
                    "" => normal_level(r["severityText"].as_str().unwrap_or("")),
                    l => l.to_string(),
                };
                let time = match json_u64(&r["timeUnixNano"]) {
                    0 => json_u64(&r["observedTimeUnixNano"]),
                    t => t,
                };
                let body = match r["body"]["stringValue"].as_str() {
                    Some(s) => clip(s, MAX_BODY),
                    None => json_any(&r["body"]).unwrap_or_default(),
                };
                out.push(LogRecord { time, level, body, trace: id("traceId", 32), span: id("spanId", 16), service: service.clone(), attrs: json_attrs(&r["attributes"], MAX_ATTRS) });
            }
        }
    }
    Ok(out)
}

/// The data points of a JSON `ExportMetricsServiceRequest`.
pub fn metrics_from_json(v: &Value) -> anyhow::Result<Vec<Point>> {
    let mut out = Vec::new();
    for rm in v["resourceMetrics"].as_array().into_iter().flatten() {
        for sm in rm["scopeMetrics"].as_array().into_iter().flatten() {
            for m in sm["metrics"].as_array().into_iter().flatten() {
                let (name, unit) = (clip(m["name"].as_str().unwrap_or(""), 128), clip(m["unit"].as_str().unwrap_or(""), 32));
                for kind in ["gauge", "sum", "histogram"] {
                    let body = &m[kind];
                    if body.is_null() {
                        continue;
                    }
                    let temporality = &body["aggregationTemporality"];
                    let cumulative = temporality == 2 || temporality == "AGGREGATION_TEMPORALITY_CUMULATIVE";
                    let counter = kind == "sum" && cumulative && body["isMonotonic"] == true;
                    for p in body["dataPoints"].as_array().into_iter().flatten() {
                        anyhow::ensure!(out.len() < MAX_SPANS, "over {MAX_SPANS} data points in one request");
                        let num = |k: &str| p[k].as_f64().or_else(|| p[k].as_str().and_then(|s| s.parse().ok()));
                        let (value, count) = if kind == "histogram" { (num("sum"), num("count").map(|c| c as u64)) } else { (num("asDouble").or_else(|| num("asInt")), None) };
                        let Some(value) = value.filter(|v| v.is_finite()) else { continue };
                        out.push(Point { name: name.clone(), kind: kind.into(), unit: unit.clone(), time: json_u64(&p["timeUnixNano"]), value, count, counter, attrs: json_attrs(&p["attributes"], MAX_POINT_ATTRS) });
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

    /// A protobuf field: its key, then its value.
    fn field(n: u64, wire: u64, value: &[u8]) -> Vec<u8> {
        let mut out = varint(n << 3 | wire);
        if wire == 2 {
            out.extend(varint(value.len() as u64));
        }
        out.extend_from_slice(value);
        out
    }

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn kv(k: &str, s: &str) -> Vec<u8> {
        [field(1, 2, k.as_bytes()), field(2, 2, &field(1, 2, s.as_bytes()))].concat()
    }

    #[test]
    fn a_protobuf_request_reads_as_its_spans() {
        let sp = [
            field(1, 2, &[0xab; 16]),
            field(2, 2, &[0xcd; 8]),
            field(5, 2, b"GET /items"),
            field(7, 1, &1_000u64.to_le_bytes()),
            field(8, 1, &5_000u64.to_le_bytes()),
            field(9, 2, &kv("http.route", "/items")),
            field(15, 2, &field(3, 0, &[2])),
            field(99, 2, b"an unknown field is skipped"),
        ]
        .concat();
        let resource = field(1, 2, &kv("service.name", "api"));
        let rs = [field(1, 2, &resource), field(2, 2, &field(2, 2, &sp))].concat();
        let spans = from_protobuf(&field(1, 2, &rs)).unwrap();
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert_eq!((s.trace.as_str(), s.span.as_str(), s.name.as_str(), s.service.as_str(), s.start, s.end, s.error), ("ab".repeat(16).as_str(), "cd".repeat(8).as_str(), "GET /items", "api", 1_000, 5_000, true));
        assert_eq!(s.attrs, vec![("http.route".to_string(), "/items".to_string())]);
        // A length past the end, or a 2 GiB claim in a few bytes, is refused, not allocated.
        assert!(from_protobuf(&[0x0a, 0xff, 0xff, 0xff, 0xff, 0x07]).is_err());
    }

    #[test]
    fn protobuf_logs_and_metrics_read_as_roll_keeps_them() {
        let body = field(1, 2, b"payment failed");
        let lr = [field(1, 1, &7u64.to_le_bytes()), field(2, 0, &[17]), field(5, 2, &body), field(6, 2, &kv("order", "42")), field(9, 2, &[0xab; 16]), field(10, 2, &[0xcd; 8])].concat();
        let rl = [field(1, 2, &field(1, 2, &kv("service.name", "api"))), field(2, 2, &field(2, 2, &lr))].concat();
        let logs = logs_from_protobuf(&field(1, 2, &rl)).unwrap();
        assert_eq!(logs, vec![LogRecord { time: 7, level: "error".into(), body: "payment failed".into(), trace: "ab".repeat(16), span: "cd".repeat(8), service: "api".into(), attrs: vec![("order".into(), "42".into())] }]);
        // A cumulative monotonic sum (a counter) of 12, and a histogram's sum and count.
        let np = [field(3, 1, &9u64.to_le_bytes()), field(6, 1, &12u64.to_le_bytes()), field(7, 2, &kv("route", "/"))].concat();
        let sum = [field(1, 2, &np), field(2, 0, &[2]), field(3, 0, &[1])].concat();
        let hp = [field(3, 1, &9u64.to_le_bytes()), field(4, 1, &3u64.to_le_bytes()), field(5, 1, &1.5f64.to_bits().to_le_bytes())].concat();
        let metrics = [field(2, 2, &[field(1, 2, b"requests"), field(7, 2, &sum)].concat()), field(2, 2, &[field(1, 2, b"latency"), field(3, 2, b"s"), field(9, 2, &field(1, 2, &hp))].concat())].concat();
        let rm = [field(1, 2, &field(1, 2, &kv("service.name", "api"))), field(2, 2, &metrics)].concat();
        let points = metrics_from_protobuf(&field(1, 2, &rm)).unwrap();
        assert_eq!((points[0].name.as_str(), points[0].value, points[0].counter, points[0].attrs.clone()), ("requests", 12.0, true, vec![("route".to_string(), "/".to_string())]));
        assert_eq!((points[1].kind.as_str(), points[1].unit.as_str(), points[1].value, points[1].count), ("histogram", "s", 1.5, Some(3)));
    }

    #[test]
    fn a_json_request_reads_as_its_spans() {
        let v = serde_json::json!({ "resourceSpans": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "web" } }] },
            "scopeSpans": [{ "spans": [{ "traceId": "AB".repeat(16), "spanId": "cd".repeat(8), "parentSpanId": "", "name": "render", "startTimeUnixNano": "10", "endTimeUnixNano": 30, "attributes": [{ "key": "n", "value": { "intValue": "3" } }] }] }] }] });
        let spans = from_json(&v).unwrap();
        assert_eq!((spans[0].trace.as_str(), spans[0].service.as_str(), spans[0].end - spans[0].start, spans[0].attrs.clone()), ("ab".repeat(16).as_str(), "web", 20, vec![("n".to_string(), "3".to_string())]));
    }
}

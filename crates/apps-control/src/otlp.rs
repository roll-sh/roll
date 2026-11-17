//! OpenTelemetry traces in (OTLP/HTTP, `POST …/v1/traces`): `ExportTraceServiceRequest` as
//! protobuf (what most SDKs send) or as JSON, read into the few fields roll keeps of a span. The
//! protobuf reader is a small one of its own (no generated code): it walks the fields it needs and
//! skips the rest, every length bounded by the bytes actually present.

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

/// Spans one request may carry, and attributes one span keeps.
const MAX_SPANS: usize = 10_000;
const MAX_ATTRS: usize = 32;

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

/// The spans of a protobuf `ExportTraceServiceRequest`.
pub fn from_protobuf(b: &[u8]) -> anyhow::Result<Vec<Span>> {
    let mut out = Vec::new();
    let mut r = fields(b);
    while let Some((n, f)) = r.next()? {
        let (1, Field::Bytes(rs)) = (n, f) else { continue };
        // ResourceSpans: resource (1) then scope spans (2).
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
                if let (2, Field::Bytes(sp)) = (sn, sf) {
                    anyhow::ensure!(out.len() < MAX_SPANS, "over {MAX_SPANS} spans in one request");
                    out.push(span(sp, &service)?);
                }
            }
        }
    }
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
    fn a_json_request_reads_as_its_spans() {
        let v = serde_json::json!({ "resourceSpans": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "web" } }] },
            "scopeSpans": [{ "spans": [{ "traceId": "AB".repeat(16), "spanId": "cd".repeat(8), "parentSpanId": "", "name": "render", "startTimeUnixNano": "10", "endTimeUnixNano": 30, "attributes": [{ "key": "n", "value": { "intValue": "3" } }] }] }] }] });
        let spans = from_json(&v).unwrap();
        assert_eq!((spans[0].trace.as_str(), spans[0].service.as_str(), spans[0].end - spans[0].start, spans[0].attrs.clone()), ("ab".repeat(16).as_str(), "web", 20, vec![("n".to_string(), "3".to_string())]));
    }
}

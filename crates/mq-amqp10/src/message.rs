//! AMQP 1.0 messages (spec part 3) and their mapping to the broker's internal AMQP 0-9-1
//! properties, following RabbitMQ 4's message containers (`mc_amqpl`):
//!
//! - `header.durable/priority/ttl` ↔ `delivery_mode/priority/expiration`
//! - `application-properties` ↔ non-`x-` headers, `x-*` message annotations ↔ `x-*` headers
//! - `message-id`/`correlation-id`: strings as is, UUIDs as `urn:uuid:…`, ulongs as decimal
//! - `reply-to` `/queues/:q` ↔ `q`, `group-id` ↔ `app_id`, `creation-time` ↔ `timestamp`
//! - a body of `data` sections becomes the payload; any other body is kept AMQP-encoded with
//!   type `amqp-1.0`, so it round-trips to 1.0 consumers.
//!
//! Properties a 0-9-1 message can't carry (`subject`, `to`, typed ids, …) travel in the
//! `x-amqp-1.0-properties` header (as RabbitMQ 3.x did) and are restored for 1.0 consumers.

use crate::frames::{code, descriptor_code};
use crate::types::{CodecError, CodecResult, Value};
use bytes::{BufMut, Bytes, BytesMut};
use mq_core::message::Properties;
use mq_core::value::{FieldTable, FieldValue};

pub const AMQP10_TYPE: &str = "amqp-1.0";
pub const PROPERTIES_HEADER: &str = "x-amqp-1.0-properties";

/// `properties` field positions.
pub mod prop {
    pub const MESSAGE_ID: usize = 0;
    pub const USER_ID: usize = 1;
    pub const TO: usize = 2;
    pub const SUBJECT: usize = 3;
    pub const REPLY_TO: usize = 4;
    pub const CORRELATION_ID: usize = 5;
    pub const CONTENT_TYPE: usize = 6;
    pub const CONTENT_ENCODING: usize = 7;
    pub const ABSOLUTE_EXPIRY_TIME: usize = 8;
    pub const CREATION_TIME: usize = 9;
    pub const GROUP_ID: usize = 10;
    pub const GROUP_SEQUENCE: usize = 11;
    pub const REPLY_TO_GROUP_ID: usize = 12;
    pub const COUNT: usize = 13;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Header {
    pub durable: Option<bool>,
    pub priority: Option<u8>,
    pub ttl: Option<u32>,
    pub first_acquirer: Option<bool>,
    pub delivery_count: Option<u32>,
}

/// A decoded AMQP 1.0 message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Amqp1Message {
    pub header: Option<Header>,
    pub message_annotations: Vec<(Value, Value)>,
    /// The 13 `properties` fields (`Value::Null` when absent); `None` without the section.
    pub properties: Option<Vec<Value>>,
    pub application_properties: Vec<(Value, Value)>,
    /// The body sections exactly as received.
    pub body_raw: Bytes,
    /// `Some(payload)` when the body is only `data` sections.
    pub data: Option<Bytes>,
    /// The body when it is a single `amqp-value` section.
    pub value: Option<Value>,
}

fn bad(m: impl Into<String>) -> CodecError {
    CodecError(m.into())
}

impl Amqp1Message {
    pub fn prop(&self, i: usize) -> &Value {
        self.properties.as_ref().and_then(|p| p.get(i)).unwrap_or(&Value::Null)
    }

    pub fn prop_str(&self, i: usize) -> Option<&str> {
        self.prop(i).as_str()
    }

    pub fn annotation(&self, key: &str) -> Option<&Value> {
        self.message_annotations.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
    }

    /// Parse the sections of a transfer payload.
    pub fn decode(payload: Bytes) -> CodecResult<Self> {
        let mut m = Amqp1Message::default();
        let total = payload.len();
        let mut buf = payload.clone();
        let mut body_start = None;
        let mut body_end = 0;
        let mut datas: Vec<Bytes> = Vec::new();
        let mut other_body = false;
        let mut body_sections = 0;
        while !buf.is_empty() {
            let start = total - buf.len();
            let v = Value::decode(&mut buf)?;
            let end = total - buf.len();
            let Value::Described(d, body) = v else { return Err(bad("message section is not a described type")) };
            let c = descriptor_code(&d).ok_or_else(|| bad("unknown message section"))?;
            match c {
                code::HEADER => {
                    let Value::List(f) = *body else { return Err(bad("header is not a list")) };
                    let at = |i: usize| f.get(i).unwrap_or(&Value::Null);
                    m.header = Some(Header {
                        durable: at(0).as_bool(),
                        priority: at(1).as_u64().map(|p| p.min(255) as u8),
                        ttl: at(2).as_u32(),
                        first_acquirer: at(3).as_bool(),
                        delivery_count: at(4).as_u32(),
                    });
                }
                code::DELIVERY_ANNOTATIONS => {}
                code::MESSAGE_ANNOTATIONS => {
                    if let Value::Map(a) = *body {
                        m.message_annotations = a;
                    }
                }
                code::PROPERTIES => {
                    let Value::List(mut f) = *body else { return Err(bad("properties is not a list")) };
                    f.resize(prop::COUNT, Value::Null);
                    m.properties = Some(f);
                }
                code::APPLICATION_PROPERTIES => {
                    if let Value::Map(a) = *body {
                        m.application_properties = a;
                    }
                }
                code::DATA | code::AMQP_SEQUENCE | code::AMQP_VALUE => {
                    body_start.get_or_insert(start);
                    body_end = end;
                    body_sections += 1;
                    match (c, *body) {
                        (code::DATA, Value::Binary(b)) => datas.push(b),
                        (code::DATA, _) => return Err(bad("data section is not binary")),
                        (code::AMQP_VALUE, v) => {
                            other_body = true;
                            m.value = Some(v);
                        }
                        _ => other_body = true,
                    }
                }
                code::FOOTER => {}
                _ => return Err(bad(format!("unexpected message section 0x{c:02x}"))),
            }
        }
        if body_sections != 1 {
            m.value = None;
        }
        if let Some(s) = body_start {
            m.body_raw = payload.slice(s..body_end);
        }
        if !other_body {
            m.data = Some(match datas.len() {
                0 => Bytes::new(),
                1 => datas.pop().unwrap_or_default(),
                _ => {
                    let mut b = BytesMut::new();
                    for d in datas {
                        b.extend_from_slice(&d);
                    }
                    b.freeze()
                }
            });
        }
        Ok(m)
    }

    /// Encode as a transfer payload.
    pub fn encode(&self, out: &mut BytesMut) {
        if let Some(h) = &self.header {
            header_value(h).encode(out);
        }
        if !self.message_annotations.is_empty() {
            Value::described(code::MESSAGE_ANNOTATIONS, Value::Map(self.message_annotations.clone())).encode(out);
        }
        if let Some(p) = &self.properties {
            properties_value(p).encode(out);
        }
        if !self.application_properties.is_empty() {
            Value::described(code::APPLICATION_PROPERTIES, Value::Map(self.application_properties.clone())).encode(out);
        }
        if !self.body_raw.is_empty() {
            out.extend_from_slice(&self.body_raw);
        } else if let Some(d) = &self.data {
            put_data_section(out, d);
        } else if let Some(v) = &self.value {
            Value::described(code::AMQP_VALUE, v.clone()).encode(out);
        }
    }
}

fn header_value(h: &Header) -> Value {
    let mut f = vec![
        h.durable.map(Value::Bool).unwrap_or(Value::Null),
        h.priority.map(Value::Ubyte).unwrap_or(Value::Null),
        h.ttl.map(Value::Uint).unwrap_or(Value::Null),
        h.first_acquirer.map(Value::Bool).unwrap_or(Value::Null),
        h.delivery_count.map(Value::Uint).unwrap_or(Value::Null),
    ];
    while f.last().is_some_and(Value::is_null) {
        f.pop();
    }
    Value::described(code::HEADER, Value::List(f))
}

fn trimmed(p: &[Value]) -> Vec<Value> {
    let mut f = p.to_vec();
    while f.last().is_some_and(Value::is_null) {
        f.pop();
    }
    f
}

pub fn properties_value(p: &[Value]) -> Value {
    Value::described(code::PROPERTIES, Value::List(trimmed(p)))
}

/// A `data` section without copying the payload through a `Value`.
pub fn put_data_section(out: &mut BytesMut, data: &[u8]) {
    out.put_u8(0x00);
    out.put_u8(0x53);
    out.put_u8(code::DATA as u8);
    if data.len() < 256 {
        out.put_u8(0xa0);
        out.put_u8(data.len() as u8);
    } else {
        out.put_u8(0xb0);
        out.put_u32(data.len() as u32);
    }
    out.put_slice(data);
}

// ── value conversion (mc_amqpl:to_091 / from_091) ──

fn shortstr(s: &str) -> bool {
    s.len() <= 255
}

/// AMQP 1.0 → 0-9-1 field value; `None` for types 0-9-1 can't represent.
pub fn to_091(v: &Value) -> Option<FieldValue> {
    Some(match v {
        Value::String(s) | Value::Symbol(s) => FieldValue::str(s.clone()),
        Value::Long(n) => FieldValue::I64(*n),
        Value::Ulong(n) => FieldValue::I64(*n as i64),
        Value::Byte(n) => FieldValue::I8(*n),
        Value::Ubyte(n) => FieldValue::U8(*n),
        Value::Short(n) => FieldValue::I16(*n),
        Value::Ushort(n) => FieldValue::U16(*n),
        Value::Uint(n) => FieldValue::U32(*n),
        Value::Int(n) => FieldValue::I32(*n),
        Value::Double(n) => FieldValue::F64(*n),
        Value::Float(n) => FieldValue::F32(*n),
        Value::Timestamp(ms) => FieldValue::Timestamp((*ms / 1000).max(0) as u64),
        Value::Binary(b) => FieldValue::LongStr(b.clone()),
        Value::Bool(b) => FieldValue::Bool(*b),
        Value::Null => FieldValue::Void,
        Value::List(l) | Value::Array(l) => FieldValue::Array(l.iter().filter_map(to_091).collect()),
        Value::Map(m) => FieldValue::Table(FieldTable(
            m.iter().filter_map(|(k, v)| Some((k.as_str().filter(|k| shortstr(k))?.to_string(), to_091(v)?))).collect(),
        )),
        Value::Char(_) | Value::Uuid(_) | Value::Described(..) | Value::Decimal32(_) | Value::Decimal64(_) | Value::Decimal128(_) => return None,
    })
}

/// 0-9-1 → AMQP 1.0 value.
pub fn from_091(v: &FieldValue) -> Value {
    match v {
        FieldValue::LongStr(b) => match std::str::from_utf8(b) {
            Ok(s) if !s.contains('\0') => Value::str(s),
            _ => Value::Binary(b.clone()),
        },
        FieldValue::ShortStr(s) => Value::str(s.clone()),
        FieldValue::Bytes(b) => Value::Binary(b.clone()),
        FieldValue::I64(n) => Value::Long(*n),
        FieldValue::U8(n) => Value::Ubyte(*n),
        FieldValue::I8(n) => Value::Byte(*n),
        FieldValue::I16(n) => Value::Short(*n),
        FieldValue::U16(n) => Value::Ushort(*n),
        FieldValue::U32(n) => Value::Uint(*n),
        FieldValue::I32(n) => Value::Int(*n),
        FieldValue::F64(n) => Value::Double(*n),
        FieldValue::F32(n) => Value::Float(*n),
        FieldValue::Bool(b) => Value::Bool(*b),
        FieldValue::Timestamp(s) => Value::Timestamp((*s as i64).saturating_mul(1000)),
        FieldValue::Decimal(scale, n) => Value::Double(*n as f64 / 10f64.powi(*scale as i32)),
        FieldValue::Void => Value::Null,
        FieldValue::Array(a) => Value::List(a.iter().map(from_091).collect()),
        FieldValue::Table(t) => Value::Map(t.iter().map(|(k, v)| (Value::sym(k), from_091(v))).collect()),
    }
}

fn uuid_urn(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!("urn:uuid:{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn urn_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.strip_prefix("urn:uuid:")?.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || s.len() != 45 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn id_to_091(v: &Value, header: &str, headers: &mut FieldTable) -> Option<String> {
    match v {
        Value::Uuid(u) => Some(uuid_urn(u)),
        Value::Ulong(n) => Some(n.to_string()),
        Value::Binary(b) => {
            headers.insert(header, FieldValue::Bytes(b.clone()));
            None
        }
        Value::String(s) if shortstr(s) => Some(s.clone()),
        Value::String(s) => {
            headers.insert(header, FieldValue::str(s.clone()));
            None
        }
        _ => None,
    }
}

fn id_from_091(s: &Option<String>) -> Value {
    match s {
        None => Value::Null,
        Some(s) => match urn_uuid(s) {
            Some(u) => Value::Uuid(u),
            None => Value::str(s.clone()),
        },
    }
}

/// Header names RabbitMQ adds itself during conversions (never copied from annotations).
fn is_internal_header(k: &str) -> bool {
    k.starts_with("x-basic-") || matches!(k, "x-routing-key" | "x-exchange" | "x-death" | PROPERTIES_HEADER)
}

fn quote(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, crate::address::SEGMENT).to_string()
}

/// AMQP 1.0 → internal properties and payload (`mc_amqpl:convert_from(mc_amqp, …)`).
pub fn to_core(m: &Amqp1Message) -> (Properties, Bytes) {
    let mut headers = FieldTable::new();
    for (k, v) in &m.message_annotations {
        let Some(k) = k.as_str() else { continue };
        match k {
            "x-cc" => {
                if let Some(fv) = to_091(v) {
                    headers.insert("CC", fv);
                }
            }
            "x-opt-rabbitmq-received-time" => {
                if let Value::Timestamp(ts) = v {
                    headers.insert("timestamp_in_ms", FieldValue::I64(*ts));
                }
            }
            k if k.starts_with("x-") && shortstr(k) && !is_internal_header(k) => {
                if let Some(fv) = to_091(v) {
                    headers.insert(k, fv);
                }
            }
            _ => {}
        }
    }
    for (k, v) in &m.application_properties {
        if let (Value::String(k), Some(fv)) = (k, to_091(v))
            && shortstr(k)
        {
            headers.insert(k.clone(), fv);
        }
    }
    let message_id = id_to_091(m.prop(prop::MESSAGE_ID), "x-message-id", &mut headers);
    let correlation_id = id_to_091(m.prop(prop::CORRELATION_ID), "x-correlation-id", &mut headers);
    let short = |i: usize| m.prop_str(i).filter(|s| shortstr(s)).map(String::from);
    let user_id = match m.prop(prop::USER_ID) {
        Value::Binary(b) => std::str::from_utf8(b).ok().filter(|s| shortstr(s)).map(String::from),
        _ => None,
    };
    let reply_to = short(prop::REPLY_TO).map(|r| match r.strip_prefix("/queues/") {
        Some(q) => crate::address::decode(q).unwrap_or(r.clone()),
        None => r,
    });
    let h = m.header.clone().unwrap_or_default();
    let kind = if m.data.is_some() { m.annotation("x-basic-type").and_then(|v| v.as_str()).filter(|s| shortstr(s)).map(String::from) } else { Some(AMQP10_TYPE.into()) };
    let mut p = Properties {
        content_type: short(prop::CONTENT_TYPE),
        content_encoding: short(prop::CONTENT_ENCODING),
        headers: None,
        delivery_mode: match (m.header.is_some(), h.durable) {
            (_, Some(true)) => Some(2),
            (true, _) => Some(1),
            _ => None,
        },
        priority: h.priority,
        correlation_id,
        reply_to,
        expiration: h.ttl.map(|t| t.to_string()),
        message_id,
        timestamp: match m.prop(prop::CREATION_TIME) {
            Value::Timestamp(ms) if *ms >= 0 => Some(*ms as u64 / 1000),
            _ => None,
        },
        kind,
        user_id,
        app_id: short(prop::GROUP_ID),
        cluster_id: None,
    };
    // Keep what 0-9-1 can't express so 1.0 consumers get the exact properties back.
    if let Some(orig) = &m.properties
        && trimmed(&properties_from_core(&p)) != trimmed(orig)
    {
        headers.insert(PROPERTIES_HEADER, FieldValue::Bytes(properties_value(orig).to_bytes()));
    }
    if !headers.is_empty() {
        p.headers = Some(headers);
    }
    let body = match &m.data {
        Some(d) => d.clone(),
        None => m.body_raw.clone(),
    };
    (p, body)
}

/// The `properties` section for internal properties (`mc_amqpl:convert_to(mc_amqp, …)`).
pub fn properties_from_core(p: &Properties) -> Vec<Value> {
    let mut f = vec![Value::Null; prop::COUNT];
    f[prop::MESSAGE_ID] = id_from_091(&p.message_id);
    f[prop::USER_ID] = p.user_id.as_ref().map(|u| Value::Binary(Bytes::from(u.clone()))).unwrap_or(Value::Null);
    f[prop::REPLY_TO] = match &p.reply_to {
        Some(r) => Value::str(format!("/queues/{}", quote(r))),
        None => Value::Null,
    };
    f[prop::CORRELATION_ID] = id_from_091(&p.correlation_id);
    f[prop::CONTENT_TYPE] = p.content_type.clone().map(Value::Symbol).unwrap_or(Value::Null);
    f[prop::CONTENT_ENCODING] = p.content_encoding.clone().map(Value::Symbol).unwrap_or(Value::Null);
    f[prop::CREATION_TIME] = p.timestamp.map(|t| Value::Timestamp((t as i64).saturating_mul(1000))).unwrap_or(Value::Null);
    f[prop::GROUP_ID] = p.app_id.clone().map(Value::String).unwrap_or(Value::Null);
    f
}

/// Per-delivery facts that end up in the header and annotations.
pub struct DeliveryInfo<'a> {
    pub exchange: &'a str,
    pub routing_key: &'a str,
    pub redelivered: bool,
    pub delivery_count: u32,
    pub stream_offset: Option<u64>,
}

/// Internal message → AMQP 1.0 transfer payload.
pub fn from_core(props: &Properties, body: &Bytes, d: &DeliveryInfo<'_>, out: &mut BytesMut) {
    let header = Header {
        durable: Some(props.delivery_mode == Some(2)),
        priority: props.priority,
        ttl: props.ttl_ms().map(|t| t.min(u32::MAX as u64) as u32),
        first_acquirer: Some(!d.redelivered && d.delivery_count == 0),
        delivery_count: Some(d.delivery_count),
    };
    header_value(&header).encode(out);

    let mut ma: Vec<(Value, Value)> = vec![(Value::sym("x-exchange"), Value::str(d.exchange)), (Value::sym("x-routing-key"), Value::str(d.routing_key))];
    let mut ap: Vec<(Value, Value)> = Vec::new();
    let mut extra_props = None;
    if let Some(h) = &props.headers {
        for (k, v) in h.iter() {
            if k == PROPERTIES_HEADER {
                if let FieldValue::Bytes(b) | FieldValue::LongStr(b) = v {
                    extra_props = Some(b.clone());
                }
            } else if k == "CC" && matches!(v, FieldValue::Array(_)) {
                ma.push((Value::sym("x-cc"), from_091(v)));
            } else if k == "timestamp_in_ms" {
                if let Some(ts) = v.as_i64() {
                    ma.push((Value::sym("x-opt-rabbitmq-received-time"), Value::Timestamp(ts)));
                }
            } else if k.starts_with("x-") {
                if !matches!(k, "x-exchange" | "x-routing-key") {
                    ma.push((Value::sym(k), from_091(v)));
                }
            } else if !matches!(v, FieldValue::Array(_) | FieldValue::Table(_)) {
                ap.push((Value::str(k), from_091(v)));
            }
        }
    }
    let amqp10_body = props.kind.as_deref() == Some(AMQP10_TYPE);
    if let Some(t) = props.kind.as_deref().filter(|_| !amqp10_body) {
        ma.push((Value::sym("x-basic-type"), Value::str(t)));
    }
    if let Some(off) = d.stream_offset {
        ma.push((Value::sym("x-stream-offset"), Value::Long(off as i64)));
    }
    // Quorum queues count failed deliveries (only set for quorum queues, like the 0-9-1 header).
    if d.delivery_count > 0 {
        ma.push((Value::sym("x-delivery-count"), Value::Long(d.delivery_count as i64)));
    }
    Value::described(code::MESSAGE_ANNOTATIONS, Value::Map(ma)).encode(out);

    let restored = extra_props.and_then(|mut b| Value::decode(&mut b).ok()).filter(|v| matches!(v.descriptor(), Some((d, _)) if descriptor_code(d) == Some(code::PROPERTIES)));
    match restored {
        Some(v) => v.encode(out),
        None => {
            let p = properties_from_core(props);
            if p.iter().any(|v| !v.is_null()) {
                properties_value(&p).encode(out);
            }
        }
    }
    if !ap.is_empty() {
        Value::described(code::APPLICATION_PROPERTIES, Value::Map(ap)).encode(out);
    }
    if amqp10_body && is_body_sections(body) {
        out.extend_from_slice(body);
    } else {
        put_data_section(out, body);
    }
}

/// Whether `b` is a sequence of AMQP body sections (optionally followed by a footer).
fn is_body_sections(b: &Bytes) -> bool {
    let mut buf = b.clone();
    let mut n = 0;
    while !buf.is_empty() {
        let Ok(Value::Described(d, _)) = Value::decode(&mut buf) else { return false };
        match descriptor_code(&d) {
            Some(code::DATA | code::AMQP_SEQUENCE | code::AMQP_VALUE) => n += 1,
            Some(code::FOOTER) if buf.is_empty() && n > 0 => {}
            _ => return false,
        }
    }
    n > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(sections: Vec<Value>) -> Amqp1Message {
        let mut b = BytesMut::new();
        for s in sections {
            s.encode(&mut b);
        }
        Amqp1Message::decode(b.freeze()).unwrap()
    }

    fn props(fields: &[(usize, Value)]) -> Value {
        let mut f = vec![Value::Null; prop::COUNT];
        for (i, v) in fields {
            f[*i] = v.clone();
        }
        properties_value(&f)
    }

    #[test]
    fn data_body_and_properties_to_091() {
        let m = msg(vec![
            Value::described(code::HEADER, Value::List(vec![Value::Bool(true), Value::Ubyte(5), Value::Uint(60_000)])),
            Value::described(code::MESSAGE_ANNOTATIONS, Value::Map(vec![(Value::sym("x-foo"), Value::str("bar")), (Value::sym("x-routing-key"), Value::str("ignored"))])),
            props(&[
                (prop::MESSAGE_ID, Value::str("m1")),
                (prop::CORRELATION_ID, Value::Uuid([0x11; 16])),
                (prop::REPLY_TO, Value::str("/queues/reply%20q")),
                (prop::CONTENT_TYPE, Value::sym("text/plain")),
                (prop::CREATION_TIME, Value::Timestamp(1_700_000_000_123)),
                (prop::GROUP_ID, Value::str("app")),
            ]),
            Value::described(code::APPLICATION_PROPERTIES, Value::Map(vec![(Value::str("k"), Value::Long(7)), (Value::str("u"), Value::Uuid([0; 16]))])),
            Value::described(code::DATA, Value::Binary(Bytes::from_static(b"he"))),
            Value::described(code::DATA, Value::Binary(Bytes::from_static(b"llo"))),
        ]);
        let (p, body) = to_core(&m);
        assert_eq!(&body[..], b"hello");
        assert_eq!(p.delivery_mode, Some(2));
        assert_eq!(p.priority, Some(5));
        assert_eq!(p.expiration.as_deref(), Some("60000"));
        assert_eq!(p.message_id.as_deref(), Some("m1"));
        assert_eq!(p.correlation_id.as_deref(), Some("urn:uuid:11111111-1111-1111-1111-111111111111"));
        assert_eq!(p.reply_to.as_deref(), Some("reply q"));
        assert_eq!(p.content_type.as_deref(), Some("text/plain"));
        assert_eq!(p.timestamp, Some(1_700_000_000));
        assert_eq!(p.app_id.as_deref(), Some("app"));
        let h = p.headers.clone().unwrap();
        assert_eq!(h.get_str("x-foo"), Some("bar"));
        assert_eq!(h.get_i64("k"), Some(7));
        assert!(h.get("u").is_none() && h.get("x-routing-key").is_none());
        // creation-time has milliseconds 0-9-1 can't carry, so the original properties ride along.
        assert!(h.get(PROPERTIES_HEADER).is_some());
    }

    #[test]
    fn roundtrip_through_091() {
        let m = msg(vec![
            props(&[(prop::MESSAGE_ID, Value::Ulong(42)), (prop::SUBJECT, Value::str("subj")), (prop::TO, Value::str("/queues/q"))]),
            Value::described(code::AMQP_VALUE, Value::Map(vec![(Value::str("a"), Value::Int(1))])),
        ]);
        let (p, body) = to_core(&m);
        assert_eq!(p.kind.as_deref(), Some(AMQP10_TYPE));
        assert_eq!(p.message_id.as_deref(), Some("42"));
        assert_eq!(p.delivery_mode, None);
        let mut out = BytesMut::new();
        from_core(&p, &body, &DeliveryInfo { exchange: "", routing_key: "q", redelivered: false, delivery_count: 0, stream_offset: None }, &mut out);
        let back = Amqp1Message::decode(out.freeze()).unwrap();
        assert_eq!(back.prop(prop::SUBJECT), &Value::str("subj"));
        assert_eq!(back.prop(prop::MESSAGE_ID), &Value::Ulong(42));
        assert_eq!(back.value, Some(Value::Map(vec![(Value::str("a"), Value::Int(1))])));
        assert_eq!(back.annotation("x-routing-key"), Some(&Value::str("q")));
        assert_eq!(back.header.unwrap().durable, Some(false));
        assert!(back.application_properties.is_empty());
    }

    #[test]
    fn from_091_message() {
        let mut h = FieldTable::new();
        h.insert("app", FieldValue::I32(3));
        h.insert("x-custom", FieldValue::str("v"));
        h.insert("CC", FieldValue::Array(vec![FieldValue::str("k2")]));
        let p = Properties {
            delivery_mode: Some(2),
            headers: Some(h),
            message_id: Some("urn:uuid:00112233-4455-6677-8899-aabbccddeeff".into()),
            reply_to: Some("amq.gen-x/y".into()),
            kind: Some("t".into()),
            ..Default::default()
        };
        let mut out = BytesMut::new();
        from_core(&p, &Bytes::from_static(b"body"), &DeliveryInfo { exchange: "ex", routing_key: "rk", redelivered: true, delivery_count: 2, stream_offset: Some(9) }, &mut out);
        let m = Amqp1Message::decode(out.freeze()).unwrap();
        let hd = m.header.clone().unwrap();
        assert_eq!((hd.durable, hd.first_acquirer, hd.delivery_count), (Some(true), Some(false), Some(2)));
        assert_eq!(m.data.as_deref(), Some(&b"body"[..]));
        assert!(matches!(m.prop(prop::MESSAGE_ID), Value::Uuid(_)));
        assert_eq!(m.prop_str(prop::REPLY_TO), Some("/queues/amq.gen-x%2Fy"));
        assert_eq!(m.application_properties, vec![(Value::str("app"), Value::Int(3))]);
        assert_eq!(m.annotation("x-custom"), Some(&Value::str("v")));
        assert_eq!(m.annotation("x-cc"), Some(&Value::List(vec![Value::str("k2")])));
        assert_eq!(m.annotation("x-basic-type"), Some(&Value::str("t")));
        assert_eq!(m.annotation("x-stream-offset"), Some(&Value::Long(9)));
        assert_eq!(m.annotation("x-exchange"), Some(&Value::str("ex")));
    }
}

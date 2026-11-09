//! Conversions between lapin's AMQP types and `mq-core`'s.

use lapin::BasicProperties;
use lapin::types::{AMQPValue, ByteArray, DecimalValue, FieldArray, LongString, ShortString};
use mq_core::message::Properties;
use mq_core::value::{FieldTable, FieldValue};

pub fn value_from_lapin(v: &AMQPValue) -> FieldValue {
    match v {
        AMQPValue::Boolean(b) => FieldValue::Bool(*b),
        AMQPValue::ShortShortInt(n) => FieldValue::I8(*n),
        AMQPValue::ShortShortUInt(n) => FieldValue::U8(*n),
        AMQPValue::ShortInt(n) => FieldValue::I16(*n),
        AMQPValue::ShortUInt(n) => FieldValue::U16(*n),
        AMQPValue::LongInt(n) => FieldValue::I32(*n),
        AMQPValue::LongUInt(n) => FieldValue::U32(*n),
        AMQPValue::LongLongInt(n) => FieldValue::I64(*n),
        AMQPValue::Float(f) => FieldValue::F32(*f),
        AMQPValue::Double(f) => FieldValue::F64(*f),
        AMQPValue::DecimalValue(d) => FieldValue::Decimal(d.scale, d.value),
        AMQPValue::ShortString(s) => FieldValue::ShortStr(s.as_str().to_string()),
        AMQPValue::LongString(s) => FieldValue::LongStr(bytes::Bytes::copy_from_slice(s.as_bytes())),
        AMQPValue::FieldArray(a) => FieldValue::Array(a.as_slice().iter().map(value_from_lapin).collect()),
        AMQPValue::Timestamp(t) => FieldValue::Timestamp(*t),
        AMQPValue::FieldTable(t) => FieldValue::Table(table_from_lapin(t)),
        AMQPValue::ByteArray(b) => FieldValue::Bytes(bytes::Bytes::copy_from_slice(b.as_slice())),
        AMQPValue::Void => FieldValue::Void,
    }
}

pub fn value_to_lapin(v: &FieldValue) -> AMQPValue {
    match v {
        FieldValue::Bool(b) => AMQPValue::Boolean(*b),
        FieldValue::I8(n) => AMQPValue::ShortShortInt(*n),
        FieldValue::U8(n) => AMQPValue::ShortShortUInt(*n),
        FieldValue::I16(n) => AMQPValue::ShortInt(*n),
        FieldValue::U16(n) => AMQPValue::ShortUInt(*n),
        FieldValue::I32(n) => AMQPValue::LongInt(*n),
        FieldValue::U32(n) => AMQPValue::LongUInt(*n),
        FieldValue::I64(n) => AMQPValue::LongLongInt(*n),
        FieldValue::F32(f) => AMQPValue::Float(*f),
        FieldValue::F64(f) => AMQPValue::Double(*f),
        FieldValue::Decimal(scale, value) => AMQPValue::DecimalValue(DecimalValue { scale: *scale, value: *value }),
        FieldValue::ShortStr(s) => AMQPValue::LongString(LongString::from(s.as_bytes().to_vec())),
        FieldValue::LongStr(b) => AMQPValue::LongString(LongString::from(b.to_vec())),
        FieldValue::Timestamp(t) => AMQPValue::Timestamp(*t),
        FieldValue::Table(t) => AMQPValue::FieldTable(table_to_lapin(t)),
        FieldValue::Array(a) => AMQPValue::FieldArray(FieldArray::from(a.iter().map(value_to_lapin).collect::<Vec<_>>())),
        FieldValue::Bytes(b) => AMQPValue::ByteArray(ByteArray::from(b.to_vec())),
        FieldValue::Void => AMQPValue::Void,
    }
}

pub fn table_from_lapin(t: &lapin::types::FieldTable) -> FieldTable {
    FieldTable(t.inner().iter().map(|(k, v)| (k.as_str().to_string(), value_from_lapin(v))).collect())
}

pub fn table_to_lapin(t: &FieldTable) -> lapin::types::FieldTable {
    let mut out = lapin::types::FieldTable::default();
    for (k, v) in t.iter() {
        out.insert(short(k), value_to_lapin(v));
    }
    out
}

/// A short string, truncated to the 255 bytes AMQP allows.
pub fn short(s: &str) -> ShortString {
    if s.len() <= 255 { ShortString::from(s) } else { ShortString::from(&s[..s.floor_char_boundary(255)]) }
}

pub fn props_from_lapin(p: &BasicProperties) -> Properties {
    let s = |v: &Option<ShortString>| v.as_ref().map(|x| x.as_str().to_string());
    Properties {
        content_type: s(p.content_type()),
        content_encoding: s(p.content_encoding()),
        headers: p.headers().as_ref().map(table_from_lapin),
        delivery_mode: *p.delivery_mode(),
        priority: *p.priority(),
        correlation_id: s(p.correlation_id()),
        reply_to: s(p.reply_to()),
        expiration: s(p.expiration()),
        message_id: s(p.message_id()),
        timestamp: *p.timestamp(),
        kind: s(p.kind()),
        user_id: s(p.user_id()),
        app_id: s(p.app_id()),
        cluster_id: s(p.cluster_id()),
    }
}

pub fn props_to_lapin(p: &Properties) -> BasicProperties {
    let mut b = BasicProperties::default();
    if let Some(v) = &p.content_type {
        b = b.with_content_type(short(v));
    }
    if let Some(v) = &p.content_encoding {
        b = b.with_content_encoding(short(v));
    }
    if let Some(v) = &p.headers {
        b = b.with_headers(table_to_lapin(v));
    }
    if let Some(v) = p.delivery_mode {
        b = b.with_delivery_mode(v);
    }
    if let Some(v) = p.priority {
        b = b.with_priority(v);
    }
    if let Some(v) = &p.correlation_id {
        b = b.with_correlation_id(short(v));
    }
    if let Some(v) = &p.reply_to {
        b = b.with_reply_to(short(v));
    }
    if let Some(v) = &p.expiration {
        b = b.with_expiration(short(v));
    }
    if let Some(v) = &p.message_id {
        b = b.with_message_id(short(v));
    }
    if let Some(v) = p.timestamp {
        b = b.with_timestamp(v);
    }
    if let Some(v) = &p.kind {
        b = b.with_type(short(v));
    }
    if let Some(v) = &p.user_id {
        b = b.with_user_id(short(v));
    }
    if let Some(v) = &p.app_id {
        b = b.with_app_id(short(v));
    }
    if let Some(v) = &p.cluster_id {
        b = b.with_cluster_id(short(v));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_roundtrip() {
        let mut inner = FieldTable::new();
        inner.insert("n", FieldValue::I64(-3));
        let mut h = FieldTable::new();
        h.insert("s", "x");
        h.insert("t", FieldValue::Table(inner));
        h.insert("a", FieldValue::Array(vec![FieldValue::Bool(true), FieldValue::U8(7)]));
        h.insert("ts", FieldValue::Timestamp(5));
        let p = Properties {
            content_type: Some("text/plain".into()),
            headers: Some(h),
            delivery_mode: Some(2),
            priority: Some(3),
            message_id: Some("m".into()),
            timestamp: Some(1_700_000_000),
            kind: Some("k".into()),
            ..Default::default()
        };
        let back = props_from_lapin(&props_to_lapin(&p));
        assert_eq!(back.content_type, p.content_type);
        assert_eq!(back.delivery_mode, Some(2));
        assert_eq!(back.timestamp, p.timestamp);
        let h = back.headers.unwrap();
        assert_eq!(h.get_str("s"), Some("x"));
        assert_eq!(h.get("t").and_then(|t| t.as_table()).and_then(|t| t.get_i64("n")), Some(-3));
        assert_eq!(h.get("a").and_then(|a| a.as_array()).map(|a| a.len()), Some(2));
    }
}

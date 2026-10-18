//! STOMP headers ↔ AMQP 0-9-1 properties, mapped the way RabbitMQ's STOMP plugin does.

use crate::dest;
use crate::frame::Frame;
use mq_core::message::Properties;
use mq_core::value::{FieldTable, FieldValue};

/// Headers that map to properties or describe the delivery; everything else on a SEND becomes
/// an AMQP header.
const RESERVED: [&str; 17] = [
    "content-type",
    "content-encoding",
    "persistent",
    "priority",
    "correlation-id",
    "reply-to",
    "expiration",
    "amqp-message-id",
    "timestamp",
    "type",
    "user-id",
    "app-id",
    "destination",
    "message-id",
    "ack",
    "subscription",
    "redelivered",
];

/// AMQP properties for a SEND frame.
pub fn to_properties(f: &Frame) -> Properties {
    let s = |k: &str| f.header(k).map(String::from);
    let headers: FieldTable = f.headers.iter().filter(|(k, _)| !RESERVED.contains(&k.as_str())).map(|(k, v)| (k.clone(), FieldValue::str(v.clone()))).collect();
    Properties {
        content_type: s("content-type"),
        content_encoding: s("content-encoding"),
        headers: (!headers.is_empty()).then_some(headers),
        delivery_mode: (f.bool_header("persistent") == Some(true)).then_some(2),
        priority: f.int_header("priority").map(|p| p.clamp(0, 255) as u8),
        correlation_id: s("correlation-id"),
        reply_to: s("reply-to"),
        expiration: s("expiration"),
        message_id: s("amqp-message-id"),
        timestamp: f.int_header("timestamp").and_then(|t| u64::try_from(t).ok()),
        kind: s("type"),
        user_id: s("user-id"),
        app_id: s("app-id"),
        cluster_id: None,
    }
}

/// Header list that replaces existing keys, so later sources take precedence.
#[derive(Default)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    pub fn set(&mut self, k: &str, v: String) {
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = v,
            None => self.0.push((k.to_string(), v)),
        }
    }
}

fn header_text(v: &FieldValue) -> Option<String> {
    match v {
        FieldValue::ShortStr(s) => Some(s.clone()),
        FieldValue::LongStr(b) | FieldValue::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        FieldValue::Bool(b) => Some(b.to_string()),
        FieldValue::I8(_)
        | FieldValue::U8(_)
        | FieldValue::I16(_)
        | FieldValue::U16(_)
        | FieldValue::I32(_)
        | FieldValue::U32(_)
        | FieldValue::I64(_)
        | FieldValue::Timestamp(_) => v.as_i64().map(|n| n.to_string()),
        _ => None,
    }
}

/// MESSAGE headers for a message's properties: AMQP headers first (string and integer values),
/// then the properties, which win on conflicts. `extra` AMQP headers (stream offsets, delivery
/// counts) are added like headers.
pub fn message_headers(p: &Properties, extra: &[(&str, FieldValue)]) -> Headers {
    let mut h = Headers::default();
    if let Some(t) = &p.headers {
        for (k, v) in t.iter() {
            if let Some(v) = header_text(v) {
                h.set(k, v);
            }
        }
    }
    for (k, v) in extra {
        if let Some(v) = header_text(v) {
            h.set(k, v);
        }
    }
    let mut s = |k: &str, v: &Option<String>| {
        if let Some(v) = v {
            h.set(k, v.clone());
        }
    };
    s("content-type", &p.content_type);
    s("content-encoding", &p.content_encoding);
    s("correlation-id", &p.correlation_id);
    s("expiration", &p.expiration);
    s("amqp-message-id", &p.message_id);
    s("type", &p.kind);
    s("user-id", &p.user_id);
    s("app-id", &p.app_id);
    if let Some(r) = &p.reply_to {
        h.set("reply-to", dest::reply_to_header(r));
    }
    match p.delivery_mode {
        Some(2) => h.set("persistent", "true".into()),
        Some(m) => h.set("persistent", m.to_string()),
        None => {}
    }
    if let Some(v) = p.priority {
        h.set("priority", v.to_string());
    }
    if let Some(v) = p.timestamp {
        h.set("timestamp", v.to_string());
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(h: &[(&str, &str)]) -> Frame {
        Frame { command: "SEND".into(), headers: h.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: Default::default() }
    }

    #[test]
    fn send_headers_to_properties() {
        let f = frame(&[
            ("destination", "/queue/q"),
            ("content-type", "text/plain"),
            ("persistent", "true"),
            ("priority", "5"),
            ("correlation-id", "c1"),
            ("reply-to", "/queue/replies"),
            ("expiration", "60000"),
            ("amqp-message-id", "m1"),
            ("timestamp", "1700000000"),
            ("type", "t"),
            ("app-id", "a"),
            ("x-custom", "v"),
            ("receipt", "r1"),
        ]);
        let p = to_properties(&f);
        assert_eq!(p.content_type.as_deref(), Some("text/plain"));
        assert_eq!(p.delivery_mode, Some(2));
        assert_eq!(p.priority, Some(5));
        assert_eq!(p.message_id.as_deref(), Some("m1"));
        assert_eq!(p.timestamp, Some(1_700_000_000));
        let h = p.headers.unwrap();
        assert_eq!(h.get_str("x-custom"), Some("v"));
        assert_eq!(h.get_str("receipt"), Some("r1"));
        assert!(h.get("destination").is_none() && h.get("persistent").is_none());
    }

    #[test]
    fn properties_to_message_headers() {
        let mut t = FieldTable::new();
        t.insert("s", "x");
        t.insert("n", FieldValue::I32(-4));
        t.insert("content-type", "overridden");
        t.insert("nested", FieldTable::new());
        let p = Properties {
            content_type: Some("application/json".into()),
            headers: Some(t),
            delivery_mode: Some(2),
            reply_to: Some("amq.gen-abc".into()),
            ..Default::default()
        };
        let h = message_headers(&p, &[("x-stream-offset", FieldValue::I64(7))]).0;
        let get = |k: &str| h.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        assert_eq!(get("s"), Some("x"));
        assert_eq!(get("n"), Some("-4"));
        assert_eq!(get("content-type"), Some("application/json"));
        assert_eq!(get("nested"), None);
        assert_eq!(get("persistent"), Some("true"));
        assert_eq!(get("reply-to"), Some("/reply-queue/amq.gen-abc"));
        assert_eq!(get("x-stream-offset"), Some("7"));
    }
}

//! MQTT PUBLISH ↔ broker message (AMQP 0-9-1 properties), following RabbitMQ's `mc_mqtt`:
//! QoS 1 ↔ persistent, Message Expiry ↔ `expiration`, User Properties ↔ headers, Content Type
//! ↔ `content_type`, Correlation Data ↔ `correlation_id` (or `x-correlation-id`), Response
//! Topic ↔ `x-reply-to-topic`.

use crate::packet::Props;
use crate::topic;
use bytes::Bytes;
use mq_core::message::{Message, Properties};
use mq_core::value::{FieldTable, FieldValue};

/// Internal headers carrying MQTT-only metadata between MQTT clients (never shown as User
/// Properties).
pub const H_RETAIN: &str = "x-mqtt-retain";
pub const H_PAYLOAD_FORMAT: &str = "x-mqtt-payload-format-indicator";
const H_REPLY_TO_TOPIC: &str = "x-reply-to-topic";
const H_CORRELATION_ID: &str = "x-correlation-id";

fn is_shortstr(s: &str) -> bool {
    s.len() <= 255
}

/// Build the broker message for an MQTT application message.
pub fn to_message(exchange: &str, mqtt_topic: &str, qos: u8, retain: bool, props: &Props, payload: Bytes, sparkplug: bool) -> Message {
    let mut p = Properties { delivery_mode: Some(if qos > 0 { 2 } else { 1 }), ..Default::default() };
    if let Some(ct) = &props.content_type
        && is_shortstr(ct)
        && ct.is_ascii()
    {
        p.content_type = Some(ct.clone());
    }
    let mut headers: Vec<(String, FieldValue)> = Vec::new();
    for (k, v) in &props.user {
        if is_shortstr(k) && !headers.iter().any(|(x, _)| x == k) {
            headers.push((k.clone(), FieldValue::str(v.clone())));
        }
    }
    if let Some(t) = &props.response_topic {
        headers.retain(|(k, _)| k != H_REPLY_TO_TOPIC);
        headers.push((H_REPLY_TO_TOPIC.into(), FieldValue::str(topic::mqtt_to_amqp(t, sparkplug))));
    }
    if let Some(c) = &props.correlation_data {
        match std::str::from_utf8(c) {
            Ok(s) if is_shortstr(s) => p.correlation_id = Some(s.to_string()),
            _ => {
                headers.retain(|(k, _)| k != H_CORRELATION_ID);
                headers.push((H_CORRELATION_ID.into(), FieldValue::LongStr(c.clone())));
            }
        }
    }
    if let Some(s) = props.message_expiry {
        p.expiration = Some((s as u64 * 1000).to_string());
    }
    if retain {
        headers.push((H_RETAIN.into(), FieldValue::Bool(true)));
    }
    if let Some(f) = props.payload_format {
        headers.push((H_PAYLOAD_FORMAT.into(), FieldValue::U8(f)));
    }
    if !headers.is_empty() {
        // RabbitMQ sorts field tables by key.
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        p.headers = Some(FieldTable(headers));
    }
    Message::new(exchange, topic::mqtt_to_amqp(mqtt_topic, sparkplug), p, payload)
}

/// An application message as delivered to an MQTT client.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub topic: String,
    pub qos: u8,
    /// RETAIN as published (MQTT publishers only).
    pub retain: bool,
    pub props: Props,
    pub payload: Bytes,
}

fn header_to_string(v: &FieldValue) -> Option<String> {
    Some(match v {
        FieldValue::LongStr(b) => {
            let s = std::str::from_utf8(b).ok()?;
            if s.contains('\0') {
                return None;
            }
            s.to_string()
        }
        FieldValue::ShortStr(s) => s.clone(),
        FieldValue::Bool(b) => b.to_string(),
        FieldValue::F32(f) => f.to_string(),
        FieldValue::F64(f) => f.to_string(),
        FieldValue::Void => String::new(),
        FieldValue::Table(_) | FieldValue::Array(_) | FieldValue::Bytes(_) | FieldValue::Decimal(..) => return None,
        v => v.as_i64()?.to_string(),
    })
}

fn internal_header(k: &str) -> bool {
    k == H_RETAIN || k == H_PAYLOAD_FORMAT || k == "x-death" || k.starts_with("x-first-death-") || k.starts_with("x-last-death-")
}

/// Convert a broker message for delivery to an MQTT client at `now_ms`.
pub fn from_message(msg: &Message, now_ms: u64, sparkplug: bool) -> Outgoing {
    let p = &msg.props;
    let mut props = Props { content_type: p.content_type.clone(), ..Default::default() };
    let mut retain = false;
    if let Some(h) = &p.headers {
        for (k, v) in h.iter() {
            match k {
                H_RETAIN => retain = v.as_bool().unwrap_or(false),
                H_PAYLOAD_FORMAT => props.payload_format = v.as_i64().map(|f| f as u8),
                H_REPLY_TO_TOPIC => props.response_topic = v.as_str().map(|t| topic::amqp_to_mqtt(t, sparkplug)),
                H_CORRELATION_ID if p.correlation_id.is_none() => {
                    props.correlation_data = match v {
                        FieldValue::LongStr(b) | FieldValue::Bytes(b) => Some(b.clone()),
                        v => v.as_str().map(|s| Bytes::from(s.to_string())),
                    }
                }
                k if internal_header(k) => {}
                k => {
                    if let Some(s) = header_to_string(v) {
                        props.user.push((k.to_string(), s));
                    }
                }
            }
        }
    }
    if let Some(c) = &p.correlation_id {
        props.correlation_data = Some(Bytes::from(c.clone()));
    }
    if let Some(ttl) = p.ttl_ms() {
        let waited = now_ms.saturating_sub(msg.received_ms);
        props.message_expiry = Some((ttl.saturating_sub(waited) / 1000).min(u32::MAX as u64) as u32);
    }
    Outgoing {
        topic: topic::amqp_to_mqtt(&msg.routing_key, sparkplug),
        qos: if p.persistent() { 1 } else { 0 },
        retain,
        props,
        payload: msg.body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_v5_properties() {
        let props = Props {
            payload_format: Some(1),
            message_expiry: Some(30),
            content_type: Some("application/json".into()),
            response_topic: Some("reply/to".into()),
            correlation_data: Some(Bytes::from_static(b"corr")),
            user: vec![("b".into(), "2".into()), ("a".into(), "1".into()), ("a".into(), "dup".into())],
            ..Default::default()
        };
        let m = to_message("amq.topic", "sensors/t1", 1, true, &props, Bytes::from_static(b"x"), false);
        assert_eq!(m.routing_key, "sensors.t1");
        assert!(m.props.persistent());
        assert_eq!(m.props.expiration.as_deref(), Some("30000"));
        assert_eq!(m.props.correlation_id.as_deref(), Some("corr"));
        assert_eq!(m.props.header("x-reply-to-topic").and_then(|v| v.as_str()), Some("reply.to"));
        let o = from_message(&m, m.received_ms, false);
        assert_eq!(o.topic, "sensors/t1");
        assert_eq!(o.qos, 1);
        assert!(o.retain);
        assert_eq!(o.props.payload_format, Some(1));
        assert_eq!(o.props.message_expiry, Some(30));
        assert_eq!(o.props.response_topic.as_deref(), Some("reply/to"));
        assert_eq!(o.props.correlation_data.as_deref(), Some(&b"corr"[..]));
        assert_eq!(o.props.user, vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())]);
        // Expiry counts down while the message waits.
        assert_eq!(from_message(&m, m.received_ms + 10_500, false).props.message_expiry, Some(19));
    }

    #[test]
    fn binary_correlation_goes_to_header() {
        let props = Props { correlation_data: Some(Bytes::from_static(&[0xff, 0x00])), ..Default::default() };
        let m = to_message("amq.topic", "t", 0, false, &props, Bytes::new(), false);
        assert!(m.props.correlation_id.is_none());
        assert!(!m.props.persistent());
        let o = from_message(&m, 0, false);
        assert_eq!(o.props.correlation_data.as_deref(), Some(&[0xff, 0x00][..]));
        assert!(o.props.user.is_empty());
    }

    #[test]
    fn amqp_headers_become_user_properties() {
        let mut h = FieldTable::new();
        h.insert("n", FieldValue::I32(5));
        h.insert("s", "v");
        h.insert("t", FieldValue::Table(FieldTable::new()));
        h.insert("x-first-death-reason", "expired");
        let m = Message::new("amq.topic", "a.b", Properties { headers: Some(h), delivery_mode: Some(2), ..Default::default() }, Bytes::new());
        let o = from_message(&m, 0, false);
        assert_eq!(o.props.user, vec![("n".to_string(), "5".to_string()), ("s".to_string(), "v".to_string())]);
        assert_eq!(o.topic, "a/b");
    }
}

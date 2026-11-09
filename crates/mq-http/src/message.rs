//! JSON ⇄ message conversion at the HTTP edge.

use crate::error::{ApiError, ApiResult};
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use mq_core::auth::User;
use mq_core::message::{Message, Properties};
use mq_core::queue::QueueType;
use mq_core::value::{FieldTable, FieldValue};
use serde::Deserialize;
use serde_json::{Map, Value};

/// A message as clients send it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InMessage {
    #[serde(default)]
    pub routing_key: Option<String>,
    #[serde(default)]
    pub body: Value,
    #[serde(default)]
    pub body_encoding: Option<String>,
    #[serde(default)]
    pub properties: Map<String, Value>,
    #[serde(default)]
    pub headers: Map<String, Value>,
    /// Message id: becomes `message_id` and is deduplicated within the dedup window.
    #[serde(default)]
    pub id: Option<String>,
    /// Report unroutable messages (`routed: false`) like AMQP's mandatory flag.
    #[serde(default)]
    pub mandatory: bool,
}

/// A publish request body: one message, or `{"messages": [...]}`.
pub fn parse_publish(body: &[u8]) -> ApiResult<(Vec<InMessage>, bool)> {
    let v: Value = crate::auth::parse_body(body)?;
    match v {
        Value::Object(mut o) if o.contains_key("messages") => {
            let msgs = o.remove("messages").unwrap_or_default();
            if !o.is_empty() {
                return Err(ApiError::bad_request("a batch body has only a 'messages' field"));
            }
            let msgs: Vec<InMessage> = serde_json::from_value(msgs).map_err(|e| ApiError::bad_request(format!("invalid messages: {e}")))?;
            Ok((msgs, true))
        }
        v @ Value::Object(_) => Ok((vec![serde_json::from_value(v).map_err(|e| ApiError::bad_request(format!("invalid message: {e}")))?], false)),
        _ => Err(ApiError::bad_request("the body must be a JSON object")),
    }
}

fn new_id() -> String {
    format!("{:016x}{:016x}", mq_core::rand_u64(), mq_core::rand_u64())
}

/// Build the internal message. Persistent by default; `id` (or a generated one) becomes the
/// `message_id`. Returns the message and its id.
pub fn build(m: InMessage, exchange: &str, routing_key: &str, user: &User, max_size: u64) -> ApiResult<(Message, String)> {
    let props_json = Value::Object(m.properties);
    if let Some(h) = props_json.get("headers")
        && !h.is_object()
    {
        return Err(ApiError::bad_request("properties.headers must be an object"));
    }
    let mut props = Properties::from_json(&props_json);
    if !m.headers.is_empty() {
        let h = props.headers.get_or_insert_with(FieldTable::new);
        for (k, v) in &m.headers {
            h.insert(k.clone(), FieldValue::from_json(v));
        }
    }
    if props.delivery_mode.is_none() {
        props.delivery_mode = Some(2);
    }
    let encoding = m.body_encoding.as_deref().unwrap_or(match &m.body {
        Value::String(_) | Value::Null => "utf8",
        _ => "json",
    });
    let body: Bytes = match (encoding, m.body) {
        (_, Value::Null) if encoding != "json" => Bytes::new(),
        ("utf8" | "string", Value::String(s)) => Bytes::from(s),
        ("base64", Value::String(s)) => Bytes::from(B64.decode(s.trim()).map_err(|e| ApiError::bad_request(format!("body is not valid base64: {e}")))?),
        ("json", v) => {
            if props.content_type.is_none() {
                props.content_type = Some("application/json".into());
            }
            Bytes::from(serde_json::to_vec(&v).map_err(ApiError::internal)?)
        }
        ("utf8" | "string" | "base64", _) => return Err(ApiError::bad_request(format!("body must be a string when body_encoding is '{encoding}'"))),
        (e, _) => return Err(ApiError::bad_request(format!("unknown body_encoding '{e}' (utf8, base64 or json)"))),
    };
    if body.len() as u64 > max_size {
        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "content_too_large", format!("message size {} is larger than configured max size {max_size}", body.len())));
    }
    if let Some(uid) = &props.user_id
        && *uid != user.name
        && !user.has_tag("impersonator")
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "precondition_failed",
            format!("PRECONDITION_FAILED - user_id property set to '{uid}' but authenticated user was '{}'", user.name),
        ));
    }
    if let Some(exp) = &props.expiration
        && exp.trim().parse::<u64>().is_err()
    {
        let why = if exp.trim().parse::<i64>().is_ok() { format!("{{value_negative,{exp}}}") } else { "no_integer".into() };
        return Err(ApiError::bad_request(format!("PRECONDITION_FAILED - invalid expiration '{exp}': {why}")));
    }
    if let Some(id) = m.id.filter(|s| !s.is_empty()) {
        props.message_id = Some(id);
    }
    let id = props.message_id.clone().unwrap_or_else(new_id);
    props.message_id = Some(id.clone());
    if routing_key.len() > 255 {
        return Err(ApiError::bad_request("routing_key is longer than 255 bytes"));
    }
    Ok((Message::new(exchange, routing_key, props, body), id))
}

/// Body as JSON: text as is, anything binary (invalid UTF-8, or control characters other
/// than tab and newlines) base64.
pub fn body_json(body: &[u8]) -> (Value, &'static str) {
    match std::str::from_utf8(body) {
        Ok(s) if !s.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) => (Value::String(s.into()), "utf8"),
        _ => (Value::String(B64.encode(body)), "base64"),
    }
}

/// A delivered message as clients see it. Quorum redeliveries carry `x-delivery-count` and
/// streams `x-stream-offset`, like AMQP deliveries.
pub fn render(kind: QueueType, queue: &str, seq: u64, msg: &Message, redelivered: bool, delivery_count: u32) -> Map<String, Value> {
    let mut props = msg.props.to_json();
    let mut headers = props.as_object_mut().and_then(|o| o.remove("headers")).unwrap_or_else(|| Value::Object(Map::new()));
    match kind {
        QueueType::Quorum if delivery_count > 0 => {
            headers["x-delivery-count"] = (delivery_count as i64).into();
        }
        QueueType::Stream => {
            headers["x-stream-offset"] = (seq as i64).into();
        }
        _ => {}
    }
    let (body, encoding) = body_json(&msg.body);
    let mut o = Map::new();
    o.insert("id".into(), msg.props.message_id.clone().map(Value::String).unwrap_or(Value::Null));
    o.insert("queue".into(), queue.into());
    o.insert("exchange".into(), msg.exchange.clone().into());
    o.insert("routing_key".into(), msg.routing_key.clone().into());
    o.insert("body".into(), body);
    o.insert("body_encoding".into(), encoding.into());
    o.insert("properties".into(), props);
    o.insert("headers".into(), headers);
    o.insert("redelivered".into(), redelivered.into());
    o.insert("delivery_count".into(), delivery_count.into());
    o.insert("published_at_ms".into(), msg.received_ms.into());
    if kind == QueueType::Stream {
        o.insert("offset".into(), seq.into());
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user() -> User {
        User::new("alice", "pw", vec![])
    }

    fn mk(v: Value) -> InMessage {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn encodings() {
        let (m, id) = build(mk(json!({"body": "hi"})), "", "q", &user(), 1024).unwrap();
        assert_eq!(&m.body[..], b"hi");
        assert!(m.props.persistent());
        assert_eq!(m.props.message_id.as_deref(), Some(id.as_str()));
        let (m, _) = build(mk(json!({"body": {"a": 1}})), "", "q", &user(), 1024).unwrap();
        assert_eq!(&m.body[..], br#"{"a":1}"#);
        assert_eq!(m.props.content_type.as_deref(), Some("application/json"));
        let (m, _) = build(mk(json!({"body": "AAEC", "body_encoding": "base64"})), "", "q", &user(), 1024).unwrap();
        assert_eq!(&m.body[..], &[0, 1, 2]);
        assert!(build(mk(json!({"body": "x", "body_encoding": "hex"})), "", "q", &user(), 1024).is_err());
        assert!(build(mk(json!({"body": 1, "body_encoding": "utf8"})), "", "q", &user(), 1024).is_err());
        let e = build(mk(json!({"body": "too long"})), "", "q", &user(), 3).unwrap_err();
        assert_eq!(e.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn properties_and_headers() {
        let (m, id) = build(
            mk(json!({"id": "m-1", "body": "x", "properties": {"delivery_mode": 1, "priority": 5, "headers": {"a": "b"}}, "headers": {"n": 3}})),
            "ex",
            "rk",
            &user(),
            1024,
        )
        .unwrap();
        assert_eq!(id, "m-1");
        assert!(!m.props.persistent());
        assert_eq!(m.props.priority, Some(5));
        assert_eq!(m.props.header("a").and_then(|v| v.as_str()), Some("b"));
        assert_eq!(m.props.header("n").and_then(|v| v.as_i64()), Some(3));
        assert!(build(mk(json!({"properties": {"user_id": "bob"}})), "", "q", &user(), 1024).is_err());
        assert!(build(mk(json!({"properties": {"expiration": "-1"}})), "", "q", &user(), 1024).is_err());
    }

    #[test]
    fn render_shape() {
        let (m, _) = build(mk(json!({"body": "hi", "headers": {"k": "v"}})), "", "q", &user(), 1024).unwrap();
        let r = render(QueueType::Quorum, "q", 7, &m, true, 2);
        assert_eq!(r["body"], "hi");
        assert_eq!(r["headers"]["x-delivery-count"], 2);
        assert_eq!(r["headers"]["k"], "v");
        assert_eq!(r["delivery_count"], 2);
        assert!(r["properties"].get("headers").is_none());
        let (b, enc) = body_json(&[0xff, 0]);
        assert_eq!((b, enc), (json!("/wA="), "base64"));
    }
}

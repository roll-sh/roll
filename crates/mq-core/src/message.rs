//! Messages. The internal format is AMQP 0-9-1's (basic properties + body); other protocols
//! convert at their edge, the way RabbitMQ's message containers do.

use crate::value::{DecodeError, DecodeResult, FieldTable, get_longstr, get_shortstr, get_u8, get_u16, get_u64, put_longstr, put_shortstr, take};
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Properties {
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub headers: Option<FieldTable>,
    pub delivery_mode: Option<u8>,
    pub priority: Option<u8>,
    pub correlation_id: Option<String>,
    pub reply_to: Option<String>,
    pub expiration: Option<String>,
    pub message_id: Option<String>,
    pub timestamp: Option<u64>,
    pub kind: Option<String>,
    pub user_id: Option<String>,
    pub app_id: Option<String>,
    pub cluster_id: Option<String>,
}

impl Properties {
    pub fn persistent(&self) -> bool {
        self.delivery_mode == Some(2)
    }

    pub fn header(&self, key: &str) -> Option<&crate::value::FieldValue> {
        self.headers.as_ref()?.get(key)
    }

    /// Per-message TTL in ms, if the `expiration` property is a valid non-negative integer.
    pub fn ttl_ms(&self) -> Option<u64> {
        self.expiration.as_deref()?.trim().parse().ok()
    }

    /// Property flags + property list, as carried in a content header frame.
    pub fn encode(&self, out: &mut BytesMut) {
        let mut flags = 0u16;
        let flag_at = out.len();
        out.put_u16(0);
        macro_rules! s {
            ($bit:expr, $f:expr) => {
                if let Some(v) = &$f {
                    flags |= 1 << $bit;
                    put_shortstr(out, v);
                }
            };
        }
        s!(15, self.content_type);
        s!(14, self.content_encoding);
        if let Some(h) = &self.headers {
            flags |= 1 << 13;
            h.encode(out);
        }
        if let Some(v) = self.delivery_mode {
            flags |= 1 << 12;
            out.put_u8(v);
        }
        if let Some(v) = self.priority {
            flags |= 1 << 11;
            out.put_u8(v);
        }
        s!(10, self.correlation_id);
        s!(9, self.reply_to);
        s!(8, self.expiration);
        s!(7, self.message_id);
        if let Some(v) = self.timestamp {
            flags |= 1 << 6;
            out.put_u64(v);
        }
        s!(5, self.kind);
        s!(4, self.user_id);
        s!(3, self.app_id);
        s!(2, self.cluster_id);
        out[flag_at..flag_at + 2].copy_from_slice(&flags.to_be_bytes());
    }

    pub fn decode(buf: &mut Bytes) -> DecodeResult<Self> {
        let flags = get_u16(buf)?;
        if flags & 1 != 0 {
            return Err(DecodeError("property flag continuation is not supported"));
        }
        let has = |bit: u16| flags & (1 << bit) != 0;
        let mut p = Properties::default();
        let s = |bit: u16, buf: &mut Bytes| -> DecodeResult<Option<String>> {
            if has(bit) { get_shortstr(buf).map(Some) } else { Ok(None) }
        };
        p.content_type = s(15, buf)?;
        p.content_encoding = s(14, buf)?;
        if has(13) {
            p.headers = Some(FieldTable::decode(buf)?);
        }
        if has(12) {
            p.delivery_mode = Some(get_u8(buf)?);
        }
        if has(11) {
            p.priority = Some(get_u8(buf)?);
        }
        p.correlation_id = s(10, buf)?;
        p.reply_to = s(9, buf)?;
        p.expiration = s(8, buf)?;
        p.message_id = s(7, buf)?;
        if has(6) {
            p.timestamp = Some(get_u64(buf)?);
        }
        p.kind = s(5, buf)?;
        p.user_id = s(4, buf)?;
        p.app_id = s(3, buf)?;
        p.cluster_id = s(2, buf)?;
        Ok(p)
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = BytesMut::with_capacity(64);
        self.encode(&mut b);
        b.freeze()
    }

    /// Management API view of the properties.
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        let mut s = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                m.insert(k.into(), v.clone().into());
            }
        };
        s("content_type", &self.content_type);
        s("content_encoding", &self.content_encoding);
        s("correlation_id", &self.correlation_id);
        s("reply_to", &self.reply_to);
        s("expiration", &self.expiration);
        s("message_id", &self.message_id);
        s("type", &self.kind);
        s("user_id", &self.user_id);
        s("app_id", &self.app_id);
        s("cluster_id", &self.cluster_id);
        if let Some(h) = &self.headers {
            m.insert("headers".into(), h.to_json());
        }
        if let Some(v) = self.delivery_mode {
            m.insert("delivery_mode".into(), v.into());
        }
        if let Some(v) = self.priority {
            m.insert("priority".into(), v.into());
        }
        if let Some(v) = self.timestamp {
            m.insert("timestamp".into(), v.into());
        }
        serde_json::Value::Object(m)
    }

    pub fn from_json(v: &serde_json::Value) -> Self {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
        Properties {
            content_type: s("content_type"),
            content_encoding: s("content_encoding"),
            headers: v.get("headers").filter(|h| h.is_object()).map(FieldTable::from_json),
            delivery_mode: v.get("delivery_mode").and_then(|x| x.as_u64().or_else(|| x.as_str()?.parse().ok())).map(|x| x as u8),
            priority: v.get("priority").and_then(|x| x.as_u64()).map(|x| x.min(255) as u8),
            correlation_id: s("correlation_id"),
            reply_to: s("reply_to"),
            expiration: s("expiration").or_else(|| v.get("expiration").and_then(|x| x.as_u64()).map(|n| n.to_string())),
            message_id: s("message_id"),
            timestamp: v.get("timestamp").and_then(|x| x.as_u64()),
            kind: s("type"),
            user_id: s("user_id"),
            app_id: s("app_id"),
            cluster_id: s("cluster_id"),
        }
    }
}

/// An immutable published message, shared by every queue it is routed to.
#[derive(Debug)]
pub struct Message {
    pub exchange: String,
    pub routing_key: String,
    pub props: Properties,
    /// `props` in wire form, so deliveries don't re-encode them.
    pub props_raw: Bytes,
    pub body: Bytes,
    /// Arrival time at the broker, ms since the epoch.
    pub received_ms: u64,
}

pub type MessageRef = Arc<Message>;

impl Message {
    pub fn new(exchange: impl Into<String>, routing_key: impl Into<String>, props: Properties, body: Bytes) -> Self {
        let props_raw = props.to_bytes();
        Message {
            exchange: exchange.into(),
            routing_key: routing_key.into(),
            props,
            props_raw,
            body,
            received_ms: crate::now_ms(),
        }
    }

    /// From AMQP frames: the raw property bytes are kept for deliveries.
    pub fn from_wire(exchange: String, routing_key: String, props_raw: Bytes, body: Bytes) -> DecodeResult<Self> {
        let props = Properties::decode(&mut props_raw.clone())?;
        Ok(Message { exchange, routing_key, props, props_raw, body, received_ms: crate::now_ms() })
    }

    /// A copy with different properties (dead-lettering, shovels).
    pub fn with_props(&self, exchange: &str, routing_key: &str, props: Properties) -> Self {
        Message {
            exchange: exchange.into(),
            routing_key: routing_key.into(),
            props_raw: props.to_bytes(),
            props,
            body: self.body.clone(),
            received_ms: self.received_ms,
        }
    }

    pub fn size(&self) -> usize {
        self.body.len()
    }

    /// Storage / replication encoding.
    pub fn encode(&self, out: &mut BytesMut) {
        put_shortstr(out, &self.exchange);
        put_shortstr(out, &self.routing_key);
        out.put_u64(self.received_ms);
        put_longstr(out, &self.props_raw);
        put_longstr(out, &self.body);
    }

    pub fn encoded_len(&self) -> usize {
        2 + self.exchange.len() + self.routing_key.len() + 8 + 8 + self.props_raw.len() + self.body.len()
    }

    pub fn decode(buf: &mut Bytes) -> DecodeResult<Self> {
        let exchange = get_shortstr(buf)?;
        let routing_key = get_shortstr(buf)?;
        let received_ms = get_u64(buf)?;
        let props_raw = get_longstr(buf)?;
        let body = get_longstr(buf)?;
        let props = Properties::decode(&mut props_raw.clone())?;
        Ok(Message { exchange, routing_key, props, props_raw, body, received_ms })
    }
}

/// Skip `n` bytes (used by decoders of formats that embed messages).
pub fn skip(buf: &mut Bytes, n: usize) -> DecodeResult<()> {
    take(buf, n).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::FieldValue;

    #[test]
    fn properties_roundtrip() {
        let mut h = FieldTable::new();
        h.insert("k", FieldValue::I32(5));
        let p = Properties {
            content_type: Some("application/json".into()),
            headers: Some(h),
            delivery_mode: Some(2),
            priority: Some(7),
            expiration: Some("60000".into()),
            timestamp: Some(1_700_000_000),
            kind: Some("t".into()),
            cluster_id: Some("c".into()),
            ..Default::default()
        };
        let mut raw = p.to_bytes();
        assert_eq!(Properties::decode(&mut raw).unwrap(), p);
        assert_eq!(p.ttl_ms(), Some(60_000));
    }

    #[test]
    fn message_roundtrip() {
        let m = Message::new("ex", "rk", Properties { delivery_mode: Some(2), ..Default::default() }, Bytes::from_static(b"hi"));
        let mut b = BytesMut::new();
        m.encode(&mut b);
        assert_eq!(b.len(), m.encoded_len());
        let d = Message::decode(&mut b.freeze()).unwrap();
        assert_eq!((d.exchange.as_str(), d.routing_key.as_str(), &d.body[..]), ("ex", "rk", &b"hi"[..]));
        assert!(d.props.persistent());
    }
}

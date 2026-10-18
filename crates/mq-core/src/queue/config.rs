//! Effective queue settings: declared `x-` arguments combined with policies and operator
//! policies the way RabbitMQ resolves them (numeric limits take the minimum, everything else
//! prefers the queue argument).

use crate::error::{AmqpError, Result};
use crate::value::{FieldTable, FieldValue};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QueueType {
    #[default]
    Classic,
    Quorum,
    Stream,
}

impl QueueType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "classic" => Some(Self::Classic),
            "quorum" => Some(Self::Quorum),
            "stream" => Some(Self::Stream),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Quorum => "quorum",
            Self::Stream => "stream",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
pub enum Overflow {
    #[default]
    #[serde(rename = "drop-head")]
    DropHead,
    #[serde(rename = "reject-publish")]
    RejectPublish,
    #[serde(rename = "reject-publish-dlx")]
    RejectPublishDlx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
pub enum DeadLetterStrategy {
    #[default]
    #[serde(rename = "at-most-once")]
    AtMostOnce,
    #[serde(rename = "at-least-once")]
    AtLeastOnce,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct QueueConfig {
    pub kind: QueueType,
    pub message_ttl: Option<u64>,
    pub expires: Option<u64>,
    pub max_length: Option<u64>,
    pub max_length_bytes: Option<u64>,
    pub overflow: Overflow,
    pub dead_letter_exchange: Option<String>,
    pub dead_letter_routing_key: Option<String>,
    pub dead_letter_strategy: DeadLetterStrategy,
    pub max_priority: Option<u8>,
    pub single_active_consumer: bool,
    /// Quorum queues: dead-letter after this many redeliveries (RabbitMQ 4 defaults to 20).
    pub delivery_limit: Option<u64>,
    /// Close channels holding a delivery unacknowledged for longer than this.
    pub consumer_timeout: Option<u64>,
    /// Streams: retention by age (ms) and segment size.
    pub max_age_ms: Option<u64>,
    pub stream_max_segment_size_bytes: Option<u64>,
    /// Message deduplication (`x-message-deduplication`, plugin-compatible).
    pub dedup: bool,
    pub dedup_cache_size: Option<u64>,
    pub dedup_ttl: Option<u64>,
    /// Extension: spread one logical queue over this many partitions (actors, possibly nodes).
    pub partitions: u32,
    /// Policy that applied, for the management API.
    pub policy: Option<String>,
    pub operator_policy: Option<String>,
    /// Federation upstream set from policy.
    pub federation_upstream_set: Option<String>,
    pub federation_upstream: Option<String>,
}

/// RabbitMQ's default delivery acknowledgement timeout (30 minutes).
pub const DEFAULT_CONSUMER_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// RabbitMQ 4's default quorum queue delivery limit.
pub const DEFAULT_DELIVERY_LIMIT: u64 = 20;

fn bad(arg: &str, queue: &str, vhost: &str, why: impl std::fmt::Display) -> AmqpError {
    AmqpError::precondition(format!("invalid arg '{arg}' for queue '{queue}' in vhost '{vhost}': {why}"))
}

fn non_neg(args: &FieldTable, key: &str, q: &str, vh: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => match v.as_i64() {
            Some(n) if n >= 0 => Ok(Some(n as u64)),
            Some(n) => Err(bad(key, q, vh, format!("{{value_negative,{n}}}"))),
            None => Err(bad(key, q, vh, format!("{{unacceptable_type,{}}}", v.to_json()))),
        },
    }
}

fn pos(args: &FieldTable, key: &str, q: &str, vh: &str) -> Result<Option<u64>> {
    match non_neg(args, key, q, vh)? {
        Some(0) => Err(bad(key, q, vh, "{value_zero,0}")),
        v => Ok(v),
    }
}

fn pol_u64(p: &Value, key: &str) -> Option<u64> {
    p.get(key).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}

fn pol_str(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Parse a duration like `7D`, `12h`, `30m`, `10s` (stream `x-max-age`).
pub fn parse_age(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().ok()?;
    let ms = match unit {
        "Y" => 365 * 86_400_000,
        "M" => 30 * 86_400_000,
        "D" => 86_400_000,
        "h" => 3_600_000,
        "m" => 60_000,
        "s" => 1000,
        _ => return None,
    };
    Some(n * ms)
}

/// The queue type a declaration asks for (falling back to the vhost default).
pub fn declared_type(args: &FieldTable, queue: &str, vhost: &str, default: QueueType) -> Result<QueueType> {
    match args.get("x-queue-type") {
        None => Ok(default),
        Some(v) => {
            let s = v.as_str().unwrap_or("");
            QueueType::parse(s).ok_or_else(|| bad("x-queue-type", queue, vhost, format!("{{unsupported_queue_type,<<\"{s}\">>}}")))
        }
    }
}

impl QueueConfig {
    /// Validate declaration arguments and resolve them against the matching policies.
    pub fn resolve(
        kind: QueueType,
        args: &FieldTable,
        queue: &str,
        vhost: &str,
        policy: Option<(&str, &Value)>,
        op_policy: Option<(&str, &Value)>,
    ) -> Result<Self> {
        let (q, vh) = (queue, vhost);
        let mut c = QueueConfig { kind, partitions: 1, ..Default::default() };

        let arg_ttl = non_neg(args, "x-message-ttl", q, vh)?;
        let arg_expires = pos(args, "x-expires", q, vh)?;
        let arg_max_len = non_neg(args, "x-max-length", q, vh)?;
        let arg_max_bytes = non_neg(args, "x-max-length-bytes", q, vh)?;
        let arg_delivery_limit = non_neg(args, "x-delivery-limit", q, vh)?;
        let arg_consumer_timeout = non_neg(args, "x-consumer-timeout", q, vh)?;
        if let Some(p) = non_neg(args, "x-max-priority", q, vh)? {
            if p > 255 {
                return Err(bad("x-max-priority", q, vh, format!("{{value_too_large,{p}}}")));
            }
            c.max_priority = Some(p as u8);
        }
        let arg_overflow = match args.get("x-overflow") {
            None => None,
            Some(v) => Some(match v.as_str() {
                Some("drop-head") => Overflow::DropHead,
                Some("reject-publish") => Overflow::RejectPublish,
                Some("reject-publish-dlx") if kind != QueueType::Quorum => Overflow::RejectPublishDlx,
                _ => return Err(bad("x-overflow", q, vh, format!("{{invalid_value,{}}}", v.to_json()))),
            }),
        };
        let arg_dlx = args.get_str("x-dead-letter-exchange").map(String::from);
        let arg_dlrk = args.get_str("x-dead-letter-routing-key").map(String::from);
        if arg_dlrk.is_some() && arg_dlx.is_none() && args.get("x-dead-letter-exchange").is_none() {
            return Err(bad("x-dead-letter-routing-key", q, vh, "{missing_dead_letter_exchange}"));
        }
        if let Some(v) = args.get("x-single-active-consumer") {
            c.single_active_consumer = v.as_bool().ok_or_else(|| bad("x-single-active-consumer", q, vh, "{unacceptable_type}"))?;
        }
        if let Some(v) = args.get("x-queue-mode") {
            match v.as_str() {
                Some("default" | "lazy") => {}
                _ => return Err(bad("x-queue-mode", q, vh, format!("{{invalid_value,{}}}", v.to_json()))),
            }
        }
        let arg_max_age = match args.get("x-max-age") {
            None => None,
            Some(v) => Some(v.as_str().and_then(parse_age).ok_or_else(|| bad("x-max-age", q, vh, "{invalid_max_age}"))?),
        };
        let arg_segment = pos(args, "x-stream-max-segment-size-bytes", q, vh)?;
        if let Some(n) = pos(args, "x-partitions", q, vh)? {
            c.partitions = n.min(1024) as u32;
        }
        c.dedup = args.get_bool("x-message-deduplication").unwrap_or(false);
        c.dedup_cache_size = non_neg(args, "x-cache-size", q, vh)?;
        c.dedup_ttl = non_neg(args, "x-cache-ttl", q, vh)?;
        c.dead_letter_strategy = match args.get_str("x-dead-letter-strategy") {
            Some("at-least-once") => DeadLetterStrategy::AtLeastOnce,
            _ => DeadLetterStrategy::AtMostOnce,
        };

        let empty = Value::Null;
        let (pname, p) = policy.map(|(n, d)| (Some(n.to_string()), d)).unwrap_or((None, &empty));
        let (oname, o) = op_policy.map(|(n, d)| (Some(n.to_string()), d)).unwrap_or((None, &empty));
        c.policy = pname;
        c.operator_policy = oname;

        c.message_ttl = min_opt(min_opt(arg_ttl, pol_u64(p, "message-ttl")), pol_u64(o, "message-ttl"));
        c.expires = min_opt(min_opt(arg_expires, pol_u64(p, "expires")), pol_u64(o, "expires"));
        c.max_length = min_opt(min_opt(arg_max_len, pol_u64(p, "max-length")), pol_u64(o, "max-length"));
        c.max_length_bytes = min_opt(min_opt(arg_max_bytes, pol_u64(p, "max-length-bytes")), pol_u64(o, "max-length-bytes"));
        c.delivery_limit = min_opt(min_opt(arg_delivery_limit, pol_u64(p, "delivery-limit")), pol_u64(o, "delivery-limit"));
        c.consumer_timeout = min_opt(min_opt(arg_consumer_timeout, pol_u64(p, "consumer-timeout")), pol_u64(o, "consumer-timeout"));
        c.overflow = arg_overflow
            .or_else(|| match pol_str(p, "overflow").as_deref() {
                Some("reject-publish") => Some(Overflow::RejectPublish),
                Some("reject-publish-dlx") => Some(Overflow::RejectPublishDlx),
                Some("drop-head") => Some(Overflow::DropHead),
                _ => None,
            })
            .unwrap_or_default();
        c.dead_letter_exchange = arg_dlx.or_else(|| pol_str(p, "dead-letter-exchange"));
        c.dead_letter_routing_key = arg_dlrk.or_else(|| pol_str(p, "dead-letter-routing-key"));
        if c.dead_letter_strategy == DeadLetterStrategy::AtMostOnce && pol_str(p, "dead-letter-strategy").as_deref() == Some("at-least-once") {
            c.dead_letter_strategy = DeadLetterStrategy::AtLeastOnce;
        }
        c.max_age_ms = arg_max_age.or_else(|| pol_str(p, "max-age").and_then(|s| parse_age(&s)));
        c.stream_max_segment_size_bytes = arg_segment.or_else(|| pol_u64(p, "stream-max-segment-size-bytes"));
        c.federation_upstream_set = pol_str(p, "federation-upstream-set");
        c.federation_upstream = pol_str(p, "federation-upstream");

        if kind == QueueType::Quorum && c.delivery_limit.is_none() {
            c.delivery_limit = Some(DEFAULT_DELIVERY_LIMIT);
        }
        // `-1` (or 0 in some clients) disables the quorum delivery limit.
        if args.get_i64("x-delivery-limit") == Some(-1) {
            c.delivery_limit = None;
        }
        Ok(c)
    }

    pub fn summary(&self) -> FieldTable {
        let mut t = FieldTable::new();
        if let Some(v) = self.message_ttl {
            t.insert("message-ttl", FieldValue::I64(v as i64));
        }
        if let Some(v) = self.max_length {
            t.insert("max-length", FieldValue::I64(v as i64));
        }
        if let Some(v) = &self.dead_letter_exchange {
            t.insert("dead-letter-exchange", v.as_str());
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_limits_take_the_minimum() {
        let args: FieldTable = [("x-max-length", FieldValue::I32(100))].into_iter().collect();
        let p = json!({"max-length": 50, "dead-letter-exchange": "dlx"});
        let c = QueueConfig::resolve(QueueType::Classic, &args, "q", "/", Some(("p", &p)), None).unwrap();
        assert_eq!(c.max_length, Some(50));
        assert_eq!(c.dead_letter_exchange.as_deref(), Some("dlx"));
        let o = json!({"max-length": 10});
        let c = QueueConfig::resolve(QueueType::Classic, &args, "q", "/", Some(("p", &p)), Some(("o", &o))).unwrap();
        assert_eq!(c.max_length, Some(10));
    }

    #[test]
    fn rejects_bad_args() {
        let args: FieldTable = [("x-max-length", FieldValue::I32(-1))].into_iter().collect();
        let e = QueueConfig::resolve(QueueType::Classic, &args, "q", "/", None, None).unwrap_err();
        assert_eq!(e.code, crate::error::PRECONDITION_FAILED);
        assert!(e.text.contains("x-max-length"));
        let args: FieldTable = [("x-expires", FieldValue::I32(0))].into_iter().collect();
        assert!(QueueConfig::resolve(QueueType::Classic, &args, "q", "/", None, None).is_err());
    }

    #[test]
    fn quorum_defaults() {
        let c = QueueConfig::resolve(QueueType::Quorum, &FieldTable::new(), "q", "/", None, None).unwrap();
        assert_eq!(c.delivery_limit, Some(DEFAULT_DELIVERY_LIMIT));
        assert_eq!(parse_age("7D"), Some(7 * 86_400_000));
    }
}

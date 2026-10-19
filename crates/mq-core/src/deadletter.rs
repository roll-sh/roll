//! Dead-lettering: the `x-death` history RabbitMQ clients rely on.

use crate::message::{Message, Properties};
use crate::value::{FieldTable, FieldValue};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Rejected,
    Expired,
    MaxLen,
    DeliveryLimit,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Rejected => "rejected",
            Reason::Expired => "expired",
            Reason::MaxLen => "maxlen",
            Reason::DeliveryLimit => "delivery_limit",
        }
    }
}

/// The message as republished to the dead-letter exchange: `x-death` updated (the entry for
/// this queue and reason moves to the front with its count bumped), first/last death headers
/// set, and a per-message TTL moved into `original-expiration` so it doesn't expire again.
pub fn dead_letter(msg: &Message, queue: &str, reason: Reason, dlx: &str, dlrk: Option<&str>) -> Message {
    let mut props: Properties = msg.props.clone();
    let mut headers = props.headers.take().unwrap_or_default();
    let mut deaths: Vec<FieldValue> = headers.remove("x-death").and_then(|v| v.as_array().map(|a| a.to_vec())).unwrap_or_default();

    let mut routing_keys = vec![FieldValue::str(msg.routing_key.clone())];
    if let Some(a) = msg.props.header("CC").and_then(|v| v.as_array()) {
        routing_keys.extend(a.iter().cloned());
    }

    let existing = deaths.iter().position(|d| {
        d.as_table().is_some_and(|t| t.get_str("queue") == Some(queue) && t.get_str("reason") == Some(reason.as_str()))
    });
    let now_s = crate::now_ms() / 1000;
    let entry = match existing {
        Some(i) => {
            let FieldValue::Table(mut t) = deaths.remove(i) else { unreachable!() };
            let count = t.get_i64("count").unwrap_or(1) + 1;
            t.insert("count", FieldValue::I64(count));
            t.insert("time", FieldValue::Timestamp(now_s));
            t
        }
        None => {
            let mut t = FieldTable::new();
            t.insert("count", FieldValue::I64(1));
            t.insert("reason", reason.as_str());
            t.insert("queue", queue);
            t.insert("time", FieldValue::Timestamp(now_s));
            t.insert("exchange", msg.exchange.as_str());
            t.insert("routing-keys", FieldValue::Array(routing_keys));
            if let Some(exp) = &msg.props.expiration {
                t.insert("original-expiration", exp.as_str());
            }
            t
        }
    };
    deaths.insert(0, FieldValue::Table(entry));
    if headers.get("x-first-death-reason").is_none() {
        headers.insert("x-first-death-reason", reason.as_str());
        headers.insert("x-first-death-queue", queue);
        headers.insert("x-first-death-exchange", msg.exchange.as_str());
    }
    headers.insert("x-last-death-reason", reason.as_str());
    headers.insert("x-last-death-queue", queue);
    headers.insert("x-last-death-exchange", msg.exchange.as_str());
    headers.insert("x-death", FieldValue::Array(deaths));
    props.headers = Some(headers);
    props.expiration = None;
    let rk = dlrk.unwrap_or(&msg.routing_key);
    msg.with_props(dlx, rk, props)
}

/// RabbitMQ drops dead letters that would cycle back to a queue they already died in, unless
/// a consumer rejected them somewhere along the way.
pub fn is_cycle(msg: &Message, target_queue: &str) -> bool {
    let Some(deaths) = msg.props.header("x-death").and_then(|v| v.as_array()) else {
        return false;
    };
    let tables: Vec<&FieldTable> = deaths.iter().filter_map(|d| d.as_table()).collect();
    let seen = tables.iter().any(|t| t.get_str("queue") == Some(target_queue));
    let rejected = tables.iter().any(|t| t.get_str("reason") == Some("rejected"));
    seen && !rejected
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn x_death_accumulates() {
        let m = Message::new("ex", "rk", Properties { expiration: Some("10".into()), ..Default::default() }, Bytes::new());
        let d1 = dead_letter(&m, "q1", Reason::Expired, "dlx", None);
        assert_eq!(d1.exchange, "dlx");
        assert_eq!(d1.routing_key, "rk");
        assert_eq!(d1.props.expiration, None);
        let deaths = d1.props.header("x-death").unwrap().as_array().unwrap();
        let t = deaths[0].as_table().unwrap();
        assert_eq!(t.get_i64("count"), Some(1));
        assert_eq!(t.get_str("original-expiration"), Some("10"));
        let d2 = dead_letter(&d1, "q1", Reason::Expired, "dlx", Some("other"));
        assert_eq!(d2.routing_key, "other");
        let deaths = d2.props.header("x-death").unwrap().as_array().unwrap();
        assert_eq!(deaths.len(), 1);
        assert_eq!(deaths[0].as_table().unwrap().get_i64("count"), Some(2));
        assert!(is_cycle(&d2, "q1"));
        let d3 = dead_letter(&d2, "q2", Reason::Rejected, "dlx", None);
        assert!(!is_cycle(&d3, "q1"));
        assert_eq!(d3.props.header("x-first-death-queue").unwrap().as_str(), Some("q1"));
        assert_eq!(d3.props.header("x-last-death-queue").unwrap().as_str(), Some("q2"));
    }
}

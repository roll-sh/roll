//! Node addresses: RabbitMQ 4's v2 format (`/exchanges/:x/:key`, `/exchanges/:x`,
//! `/queues/:q`) and the v1 format of the 3.x plugin (`/exchange/x/key`, `/topic/key`,
//! `/queue/q`, `/amq/queue/q`, bare queue names). Segments are percent-encoded.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str};

/// Characters kept as-is in a path segment (RFC 3986 unreserved).
pub const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

pub fn encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, SEGMENT).to_string()
}

pub fn decode(s: &str) -> Option<String> {
    percent_decode_str(s).decode_utf8().ok().map(|c| c.into_owned())
}

/// The `/queues/:q` address of a queue.
pub fn queue_address(q: &str) -> String {
    format!("/queues/{}", encode(q))
}

/// Where a sending link (or the `to` of a message on the anonymous terminus) publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetAddr {
    /// An exchange; `key: None` takes the routing key from each message's `subject` (v1).
    Exchange { exchange: String, key: Option<String> },
    /// The default exchange with the queue's name; the queue must exist.
    Queue(String),
    /// v1 `/queue/:q`: the queue is declared if missing. An empty name routes by `subject`.
    DeclareQueue(String),
}

/// Where a receiving link consumes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceAddr {
    /// An existing queue.
    Queue(String),
    /// v1 `/queue/:q`: declared if missing.
    DeclareQueue(String),
    /// v1 `/exchange/x/key` and `/topic/key`: a private queue bound to the exchange.
    Exchange { exchange: String, key: String },
}

fn segments(rest: &str) -> Option<Vec<String>> {
    rest.split('/').map(decode).collect()
}

/// Parse a v2 target address (also the format of `to` on the anonymous terminus).
pub fn parse_target_v2(addr: &str) -> Option<TargetAddr> {
    if let Some(rest) = addr.strip_prefix("/exchanges/") {
        let s = segments(rest)?;
        let (x, key) = match s.as_slice() {
            [x] => (x.clone(), String::new()),
            [x, k] => (x.clone(), k.clone()),
            _ => return None,
        };
        if x.is_empty() || x == "amq.default" {
            return None;
        }
        return Some(TargetAddr::Exchange { exchange: x, key: Some(key) });
    }
    if let Some(rest) = addr.strip_prefix("/queues/") {
        let q = decode(rest)?;
        if q.is_empty() || rest.contains('/') {
            return None;
        }
        return Some(TargetAddr::Queue(q));
    }
    None
}

/// Parse a target address in either format.
pub fn parse_target(addr: &str) -> Option<TargetAddr> {
    if addr.starts_with("/exchanges/") || addr.starts_with("/queues/") {
        return parse_target_v2(addr);
    }
    if let Some(rest) = addr.strip_prefix("/exchange/") {
        let s = segments(rest)?;
        return match s.as_slice() {
            [x] if !x.is_empty() => Some(TargetAddr::Exchange { exchange: x.clone(), key: None }),
            [x, k] if !x.is_empty() => Some(TargetAddr::Exchange { exchange: x.clone(), key: Some(k.clone()) }),
            _ => None,
        };
    }
    if let Some(rest) = addr.strip_prefix("/topic/") {
        return Some(TargetAddr::Exchange { exchange: "amq.topic".into(), key: Some(decode(rest)?) });
    }
    if let Some(rest) = addr.strip_prefix("/amq/queue/") {
        return decode(rest).filter(|q| !q.is_empty()).map(TargetAddr::Queue);
    }
    if addr == "/queue" || addr == "/queue/" {
        return Some(TargetAddr::DeclareQueue(String::new()));
    }
    if let Some(rest) = addr.strip_prefix("/queue/") {
        return decode(rest).map(TargetAddr::DeclareQueue);
    }
    if !addr.is_empty() && !addr.starts_with('/') {
        return Some(TargetAddr::DeclareQueue(addr.to_string()));
    }
    None
}

/// Parse a source address in either format.
pub fn parse_source(addr: &str) -> Option<SourceAddr> {
    if let Some(rest) = addr.strip_prefix("/queues/") {
        let q = decode(rest)?;
        return (!q.is_empty() && !rest.contains('/')).then_some(SourceAddr::Queue(q));
    }
    if let Some(rest) = addr.strip_prefix("/amq/queue/") {
        return decode(rest).filter(|q| !q.is_empty()).map(SourceAddr::Queue);
    }
    if let Some(rest) = addr.strip_prefix("/queue/") {
        return decode(rest).filter(|q| !q.is_empty()).map(SourceAddr::DeclareQueue);
    }
    if let Some(rest) = addr.strip_prefix("/exchange/") {
        let s = segments(rest)?;
        return match s.as_slice() {
            [x] if !x.is_empty() => Some(SourceAddr::Exchange { exchange: x.clone(), key: String::new() }),
            [x, k] if !x.is_empty() => Some(SourceAddr::Exchange { exchange: x.clone(), key: k.clone() }),
            _ => None,
        };
    }
    if let Some(rest) = addr.strip_prefix("/topic/") {
        return Some(SourceAddr::Exchange { exchange: "amq.topic".into(), key: decode(rest)? });
    }
    if !addr.is_empty() && !addr.starts_with('/') {
        return Some(SourceAddr::DeclareQueue(addr.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2() {
        assert_eq!(parse_target("/exchanges/amq.topic/a.b"), Some(TargetAddr::Exchange { exchange: "amq.topic".into(), key: Some("a.b".into()) }));
        assert_eq!(parse_target("/exchanges/x"), Some(TargetAddr::Exchange { exchange: "x".into(), key: Some(String::new()) }));
        assert_eq!(parse_target("/exchanges/my%20x/k%2Fk"), Some(TargetAddr::Exchange { exchange: "my x".into(), key: Some("k/k".into()) }));
        assert_eq!(parse_target("/queues/q%2F1"), Some(TargetAddr::Queue("q/1".into())));
        assert_eq!(parse_target("/exchanges/amq.default/q"), None);
        assert_eq!(parse_target("/exchanges//q"), None);
        assert_eq!(parse_target("/queues/"), None);
        assert_eq!(parse_target("/exchanges/a/b/c"), None);
        assert_eq!(parse_source("/queues/q"), Some(SourceAddr::Queue("q".into())));
        assert_eq!(queue_address("a b/c"), "/queues/a%20b%2Fc");
    }

    #[test]
    fn v1() {
        assert_eq!(parse_target("/exchange/x/k"), Some(TargetAddr::Exchange { exchange: "x".into(), key: Some("k".into()) }));
        assert_eq!(parse_target("/exchange/x"), Some(TargetAddr::Exchange { exchange: "x".into(), key: None }));
        assert_eq!(parse_target("/topic/a.b"), Some(TargetAddr::Exchange { exchange: "amq.topic".into(), key: Some("a.b".into()) }));
        assert_eq!(parse_target("/queue/q"), Some(TargetAddr::DeclareQueue("q".into())));
        assert_eq!(parse_target("/queue"), Some(TargetAddr::DeclareQueue(String::new())));
        assert_eq!(parse_target("/amq/queue/q"), Some(TargetAddr::Queue("q".into())));
        assert_eq!(parse_target("plain"), Some(TargetAddr::DeclareQueue("plain".into())));
        assert_eq!(parse_source("/topic/#"), Some(SourceAddr::Exchange { exchange: "amq.topic".into(), key: "#".into() }));
        assert_eq!(parse_source("/exchange/x/k"), Some(SourceAddr::Exchange { exchange: "x".into(), key: "k".into() }));
        assert_eq!(parse_source("/queue/q"), Some(SourceAddr::DeclareQueue("q".into())));
        assert_eq!(parse_source("examples"), Some(SourceAddr::DeclareQueue("examples".into())));
        assert_eq!(parse_source("/bogus/x"), None);
    }
}

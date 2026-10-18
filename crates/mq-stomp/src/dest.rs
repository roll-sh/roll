//! STOMP destinations, as RabbitMQ's STOMP plugin understands them.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use md5::{Digest, Md5};

/// A parsed `destination` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dest {
    /// `/exchange/<name>[/<pattern>]`
    Exchange { name: String, pattern: Option<String> },
    /// `/queue/<name>` (or a bare name): declared on demand.
    Queue(String),
    /// `/topic/<key>` on the default topic exchange.
    Topic(String),
    /// `/temp-queue/<id>`: only valid as a `reply-to`.
    TempQueue(String),
    /// `/amq/queue/<name>`: must already exist.
    AmqQueue(String),
    /// `/reply-queue/<name>`: a temporary reply queue (names may contain `/`).
    ReplyQueue(String),
}

pub enum DestError {
    /// RabbitMQ's `{unknown_destination, Content}`.
    Unknown(String),
    /// RabbitMQ's `{invalid_destination, Type, Content}`.
    Invalid { kind: &'static str, content: String },
}

/// Prefixes a delivered `reply-to` may already carry (otherwise `/reply-queue/` is added).
const DEST_PREFIXES: [&str; 5] = ["/exchange", "/topic", "/queue", "/amq/queue", "/reply-queue/"];
pub const VALID_PREFIXES: &str = "/temp-queue, /exchange, /topic, /queue, /amq/queue, /reply-queue/";

fn unescape(s: &str) -> String {
    s.replace("%2F", "/")
}

pub fn strip_cr_lf(s: &str) -> String {
    s.replace(['\r', '\n'], "")
}

fn to_url(rest: &[&str]) -> String {
    if rest.is_empty() { String::new() } else { format!("/{}", rest.join("/")) }
}

pub fn parse(d: &str) -> Result<Dest, DestError> {
    let parts: Vec<&str> = d.split('/').collect();
    match parts.as_slice() {
        [name] => Ok(Dest::Queue(strip_cr_lf(&unescape(name)))),
        ["", "exchange", rest @ ..] => match rest {
            [name] if !name.is_empty() => Ok(Dest::Exchange { name: unescape(name), pattern: None }),
            [name, pattern] if !name.is_empty() => Ok(Dest::Exchange { name: unescape(name), pattern: Some(unescape(pattern)) }),
            _ => Err(DestError::Invalid { kind: "exchange", content: to_url(rest) }),
        },
        ["", "queue", rest @ ..] => match rest {
            [name] if !name.is_empty() => Ok(Dest::Queue(strip_cr_lf(&unescape(name)))),
            _ => Err(DestError::Invalid { kind: "queue", content: to_url(rest) }),
        },
        ["", "topic", rest @ ..] => single(rest, "topic").map(Dest::Topic),
        ["", "temp-queue", rest @ ..] => single(rest, "temp_queue").map(Dest::TempQueue),
        ["", "amq", "queue", rest @ ..] => single(rest, "amqqueue").map(Dest::AmqQueue),
        ["", "reply-queue", _, ..] => {
            let name = &d["/reply-queue/".len()..];
            if name.is_empty() {
                Err(DestError::Invalid { kind: "reply_queue", content: String::new() })
            } else {
                Ok(Dest::ReplyQueue(unescape(name)))
            }
        }
        _ => Err(DestError::Unknown(d.to_string())),
    }
}

fn single(rest: &[&str], kind: &'static str) -> Result<String, DestError> {
    match rest {
        [name] if !name.is_empty() => Ok(unescape(name)),
        _ => Err(DestError::Invalid { kind, content: to_url(rest) }),
    }
}

impl Dest {
    /// The exchange and routing key a SEND goes to (and a subscription binds with).
    pub fn routing(&self, default_topic_exchange: &str) -> (String, String) {
        match self {
            Dest::Exchange { name, pattern } => (name.clone(), pattern.clone().unwrap_or_default()),
            Dest::Topic(key) => (default_topic_exchange.to_string(), key.clone()),
            Dest::Queue(n) | Dest::AmqQueue(n) | Dest::ReplyQueue(n) | Dest::TempQueue(n) => (String::new(), n.clone()),
        }
    }
}

/// Escape a name for a `destination` header (`/`, `%` and non-printable bytes as `%XX`).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'/' => out.push_str("%2F"),
            b'%' => out.push_str("%25"),
            b if !(32..=127).contains(&b) => out.push_str(&format!("%{b:02X}")),
            b => out.push(b as char),
        }
    }
    out
}

/// The `destination` of a delivered message, from the exchange and routing key it was
/// published with.
pub fn format(exchange: &str, routing_key: &str) -> String {
    match (exchange, routing_key) {
        ("", rk) => format!("/queue/{}", escape(rk)),
        ("amq.topic", rk) => format!("/topic/{}", escape(rk)),
        (x, "") => format!("/exchange/{}", escape(x)),
        (x, rk) => format!("/exchange/{}/{}", escape(x), escape(rk)),
    }
}

/// A delivered `reply-to`: names without a destination prefix are temporary reply queues.
pub fn reply_to_header(v: &str) -> String {
    if DEST_PREFIXES.iter().any(|p| v.starts_with(p)) { v.to_string() } else { format!("/reply-queue/{v}") }
}

/// Erlang's `term_to_binary` of a string (a list of bytes).
fn put_erlang_string(out: &mut Vec<u8>, s: &[u8]) {
    if s.is_empty() {
        out.push(106);
    } else if s.len() <= u16::MAX as usize {
        out.push(107);
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s);
    } else {
        out.push(108);
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        for &b in s {
            out.extend_from_slice(&[97, b]);
        }
        out.push(106);
    }
}

/// Name of a topic/exchange subscription queue: `x-queue-name` if given, otherwise derived
/// from the routing key and subscription id exactly as RabbitMQ does (so durable subscriptions
/// keep their queue across brokers), or random for non-durable ones.
pub fn subscription_queue_name(routing_key: &str, durable_id: Option<&str>, x_queue_name: Option<&str>) -> String {
    if let Some(n) = x_queue_name.map(|n| strip_cr_lf(n).trim().to_string()).filter(|n| !n.is_empty()) {
        return n;
    }
    let digest: [u8; 16] = match durable_id {
        Some(id) => {
            let mut term = vec![131, 104, 2];
            put_erlang_string(&mut term, routing_key.as_bytes());
            put_erlang_string(&mut term, id.as_bytes());
            Md5::digest(&term).into()
        }
        None => random16(),
    };
    format!("stomp-subscription-{}", URL_SAFE_NO_PAD.encode(digest))
}

pub fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&mq_core::rand_u64().to_le_bytes());
    b[8..].copy_from_slice(&mq_core::rand_u64().to_le_bytes());
    b
}

/// A random id with RabbitMQ's GUID shape (`<prefix>-<base64url>`).
pub fn guid(prefix: &str) -> String {
    format!("{prefix}-{}", URL_SAFE_NO_PAD.encode(random16()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Dest {
        parse(s).unwrap_or_else(|_| panic!("{s} should parse"))
    }

    #[test]
    fn parses_every_kind() {
        assert_eq!(ok("/exchange/amq.direct"), Dest::Exchange { name: "amq.direct".into(), pattern: None });
        assert_eq!(ok("/exchange/x/a.b"), Dest::Exchange { name: "x".into(), pattern: Some("a.b".into()) });
        assert_eq!(ok("/queue/a%2Fb"), Dest::Queue("a/b".into()));
        assert_eq!(ok("plain"), Dest::Queue("plain".into()));
        assert_eq!(ok("/topic/a.*.c"), Dest::Topic("a.*.c".into()));
        assert_eq!(ok("/temp-queue/r1"), Dest::TempQueue("r1".into()));
        assert_eq!(ok("/amq/queue/q"), Dest::AmqQueue("q".into()));
        assert_eq!(ok("/reply-queue/amq.gen-1/x"), Dest::ReplyQueue("amq.gen-1/x".into()));
        assert!(matches!(parse("/topic/a/b"), Err(DestError::Invalid { kind: "topic", .. })));
        assert!(matches!(parse("/queue/"), Err(DestError::Invalid { kind: "queue", .. })));
        assert!(matches!(parse("/exchange//k"), Err(DestError::Invalid { kind: "exchange", .. })));
        assert!(matches!(parse("/bogus/x"), Err(DestError::Unknown(_))));
    }

    #[test]
    fn routing_and_format() {
        assert_eq!(ok("/topic/k").routing("amq.topic"), ("amq.topic".into(), "k".into()));
        assert_eq!(ok("/exchange/x").routing("amq.topic"), ("x".into(), "".into()));
        assert_eq!(ok("/amq/queue/q").routing("amq.topic"), ("".into(), "q".into()));
        assert_eq!(format("", "a/b"), "/queue/a%2Fb");
        assert_eq!(format("amq.topic", "a.b"), "/topic/a.b");
        assert_eq!(format("x", ""), "/exchange/x");
        assert_eq!(format("x", "k%\n"), "/exchange/x/k%25%0A");
        assert_eq!(reply_to_header("amq.gen-1"), "/reply-queue/amq.gen-1");
        assert_eq!(reply_to_header("/queue/q"), "/queue/q");
    }

    #[test]
    fn durable_subscription_names_match_rabbitmq() {
        // Computed with `erlang:md5(term_to_binary({"chat.room", "sub-1"}))` on Erlang/OTP 27.
        assert_eq!(subscription_queue_name("chat.room", Some("sub-1"), None), "stomp-subscription-rQinqvF6PMMFChLJhDuGuw");
        assert_eq!(subscription_queue_name("", Some("x"), None), "stomp-subscription-h-fwebydKM-y5XzgjiGmqA");
        assert_eq!(subscription_queue_name("k", Some("x"), Some(" mine\n")), "mine");
        assert_ne!(subscription_queue_name("k", None, None), subscription_queue_name("k", None, None));
    }
}

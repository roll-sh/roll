//! Dynamic shovels: runtime parameters of component `shovel`, with RabbitMQ's JSON keys.

use crate::endpoint::Session;
use crate::pump::{AckMode, Out, Pump};
use crate::status::{LinkStatus, State};
use crate::uri::{self, Target};
use anyhow::Context;
use mq_core::Broker;
use mq_core::message::{Message, Properties};
use mq_core::value::{FieldTable, FieldValue};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// `src-delete-after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteAfter {
    Never,
    QueueLength,
    Count(u64),
}

/// A parsed shovel definition.
#[derive(Clone, Debug)]
pub struct ShovelDef {
    pub src_protocol: String,
    pub src_uris: Vec<String>,
    pub src_queue: Option<String>,
    pub src_queue_args: FieldTable,
    pub src_exchange: Option<String>,
    pub src_exchange_key: String,
    pub src_consumer_args: FieldTable,
    pub src_prefetch: u32,
    pub src_delete_after: DeleteAfter,
    pub dest_protocol: String,
    pub dest_uris: Vec<String>,
    pub dest_queue: Option<String>,
    pub dest_queue_args: FieldTable,
    pub dest_exchange: Option<String>,
    pub dest_exchange_key: Option<String>,
    pub add_forward_headers: bool,
    pub add_timestamp_header: bool,
    pub publish_properties: Option<Properties>,
    pub ack_mode: AckMode,
    pub reconnect_delay: Duration,
}

fn get<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k)).filter(|x| !x.is_null())
}

fn string(v: &Value, keys: &[&str]) -> Result<Option<String>, String> {
    match get(v, keys) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("{} should be a string, got {other}", keys[0])),
    }
}

fn boolean(v: &Value, keys: &[&str]) -> Result<bool, String> {
    match get(v, keys) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("{} should be a boolean, got {other}", keys[0])),
    }
}

fn table(v: &Value, keys: &[&str]) -> Result<FieldTable, String> {
    match get(v, keys) {
        None => Ok(FieldTable::new()),
        Some(o @ Value::Object(_)) => Ok(FieldTable::from_json(o)),
        Some(other) => Err(format!("{} should be an object, got {other}", keys[0])),
    }
}

/// A URI or a list of URIs (tried in turn on reconnect).
pub fn uris(v: &Value, key: &str) -> Result<Vec<String>, String> {
    let list = match v.get(key) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| format!("{key} should be a string or a list of strings"))).collect::<Result<_, _>>()?,
        None | Some(Value::Null) => return Err(format!("{key} is required")),
        Some(other) => return Err(format!("{key} should be a string or a list of strings, got {other}")),
    };
    if list.is_empty() {
        return Err(format!("{key} must not be empty"));
    }
    for u in &list {
        uri::parse(u, "/")?;
    }
    Ok(list)
}

/// Seconds (integer or fractional) as a duration.
pub fn seconds(v: &Value, key: &str, default: f64) -> Result<Duration, String> {
    let s = match v.get(key) {
        None | Some(Value::Null) => default,
        Some(x) => x.as_f64().filter(|s| *s >= 0.0 && s.is_finite()).ok_or_else(|| format!("{key} should be a non-negative number, got {x}"))?,
    };
    Ok(Duration::from_secs_f64(s))
}

/// A non-negative integer, also accepted as a string.
fn count(v: &Value, keys: &[&str], default: u64) -> Result<u64, String> {
    match get(v, keys) {
        None => Ok(default),
        Some(x) => x.as_u64().or_else(|| x.as_str()?.parse().ok()).ok_or_else(|| format!("{} should be a non-negative integer, got {x}", keys[0])),
    }
}

impl ShovelDef {
    /// Parse and validate a shovel parameter value.
    pub fn parse(v: &Value) -> Result<Self, String> {
        if !v.is_object() {
            return Err("shovel definition should be an object".into());
        }
        let protocol = |key: &str| -> Result<String, String> {
            let p = string(v, &[key])?.unwrap_or_else(|| "amqp091".into());
            match p.as_str() {
                "amqp091" | "amqp10" => Ok(p),
                other => Err(format!("{key} should be amqp091 or amqp10, got {other}")),
            }
        };
        let src_protocol = protocol("src-protocol")?;
        let dest_protocol = protocol("dest-protocol")?;
        let src_queue = string(v, &["src-queue"])?;
        let src_exchange = string(v, &["src-exchange"])?;
        if src_queue.is_some() == src_exchange.is_some() && src_protocol == "amqp091" {
            return Err("Validation failed\n\nexactly one of src-queue and src-exchange must be specified".into());
        }
        let dest_queue = string(v, &["dest-queue"])?;
        let dest_exchange = string(v, &["dest-exchange"])?;
        if dest_queue.is_some() && dest_exchange.is_some() {
            return Err("Validation failed\n\ndest-queue and dest-exchange can't both be specified".into());
        }
        let src_delete_after = match get(v, &["src-delete-after", "delete-after"]) {
            None => DeleteAfter::Never,
            Some(Value::String(s)) if s == "never" => DeleteAfter::Never,
            Some(Value::String(s)) if s == "queue-length" => DeleteAfter::QueueLength,
            Some(x) => match x.as_u64() {
                Some(n) => DeleteAfter::Count(n),
                None => return Err(format!("src-delete-after should be \"never\", \"queue-length\" or an integer, got {x}")),
            },
        };
        if src_delete_after == DeleteAfter::QueueLength && src_queue.is_none() {
            return Err("Validation failed\n\nsrc-delete-after \"queue-length\" requires src-queue".into());
        }
        let ack_mode = match string(v, &["ack-mode"])? {
            None => AckMode::OnConfirm,
            Some(s) => AckMode::parse(&s).ok_or_else(|| format!("ack-mode should be on-confirm, on-publish or no-ack, got {s}"))?,
        };
        let publish_properties = match get(v, &["dest-publish-properties", "publish-properties"]) {
            None => None,
            Some(o @ Value::Object(_)) => Some(Properties::from_json(o)),
            Some(other) => return Err(format!("dest-publish-properties should be an object, got {other}")),
        };
        Ok(ShovelDef {
            src_uris: uris(v, "src-uri")?,
            src_queue,
            src_queue_args: table(v, &["src-queue-args"])?,
            src_exchange,
            src_exchange_key: string(v, &["src-exchange-key"])?.unwrap_or_else(|| "#".into()),
            src_consumer_args: table(v, &["src-consumer-args"])?,
            src_prefetch: count(v, &["src-prefetch-count", "prefetch-count"], 1000)?.min(u32::MAX as u64) as u32,
            src_delete_after,
            dest_uris: uris(v, "dest-uri")?,
            dest_queue,
            dest_queue_args: table(v, &["dest-queue-args"])?,
            dest_exchange,
            dest_exchange_key: string(v, &["dest-exchange-key"])?,
            add_forward_headers: boolean(v, &["dest-add-forward-headers", "add-forward-headers"])?,
            add_timestamp_header: boolean(v, &["dest-add-timestamp-header", "add-timestamp-header"])?,
            publish_properties,
            ack_mode,
            reconnect_delay: seconds(v, "reconnect-delay", 5.0)?,
            src_protocol,
            dest_protocol,
        })
    }

    /// `/api/shovels` fields describing the endpoints.
    pub fn describe(&self, out: &mut serde_json::Map<String, Value>) {
        out.insert("src_protocol".into(), json!(self.src_protocol));
        out.insert("dest_protocol".into(), json!(self.dest_protocol));
        out.insert("src_uri".into(), json!(uri::redact(&self.src_uris[0])));
        out.insert("dest_uri".into(), json!(uri::redact(&self.dest_uris[0])));
        let mut opt = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                out.insert(k.into(), json!(v));
            }
        };
        opt("src_queue", &self.src_queue);
        opt("src_exchange", &self.src_exchange);
        opt("dest_queue", &self.dest_queue);
        opt("dest_exchange", &self.dest_exchange);
        opt("dest_exchange_key", &self.dest_exchange_key);
        if self.src_exchange.is_some() {
            out.insert("src_exchange_key".into(), json!(self.src_exchange_key));
        }
    }
}

/// Validate a `shovel` parameter value (for the management API's PUT).
pub fn validate(v: &Value) -> Result<(), String> {
    ShovelDef::parse(v).map(|_| ())
}

/// How a run ended without an error.
enum Finished {
    /// `src-delete-after` was reached.
    Done,
}

/// Run a shovel until it completes (then delete its parameter) or is aborted, reconnecting
/// with backoff after failures.
pub async fn run(broker: Arc<Broker>, vhost: String, name: String, def: ShovelDef, status: Arc<LinkStatus>) {
    if def.src_protocol != "amqp091" || def.dest_protocol != "amqp091" {
        status.set(State::Terminated, Some("AMQP 1.0 shovel endpoints are not supported by roll queues; use amqp091".into()));
        return;
    }
    let mut attempt = 0usize;
    let mut failures = 0u32;
    let mut total: Option<u64> = None;
    loop {
        status.set(State::Starting, None);
        let started = Instant::now();
        match run_once(&broker, &vhost, &name, &def, &status, attempt, &mut total).await {
            Ok(Finished::Done) => {
                info!(%vhost, shovel = %name, forwarded = status.forwarded(), "shovel finished, deleting it");
                status.set(State::Terminated, Some("completed".into()));
                if let Err(e) = broker.delete_parameter("shovel", &vhost, &name).await {
                    warn!(%vhost, shovel = %name, error = %e, "failed to delete finished shovel");
                }
                return;
            }
            Err(e) => {
                let reason = format!("{e:#}");
                warn!(%vhost, shovel = %name, error = %reason, "shovel failed");
                status.set(State::Terminated, Some(reason));
                status.set_connection(None);
            }
        }
        if def.reconnect_delay.is_zero() {
            return;
        }
        if started.elapsed() > Duration::from_secs(30) {
            failures = 0;
        }
        failures += 1;
        attempt += 1;
        tokio::time::sleep(backoff(def.reconnect_delay, failures)).await;
    }
}

/// `reconnect-delay`, doubled after each consecutive failure up to a minute (or the delay
/// itself when that's longer).
pub fn backoff(delay: Duration, failures: u32) -> Duration {
    let cap = delay.max(Duration::from_secs(60));
    delay.saturating_mul(1 << failures.saturating_sub(1).min(6)).min(cap)
}

async fn run_once(
    broker: &Arc<Broker>,
    vhost: &str,
    name: &str,
    def: &ShovelDef,
    status: &LinkStatus,
    attempt: usize,
    total: &mut Option<u64>,
) -> anyhow::Result<Finished> {
    let src_uri = &def.src_uris[attempt % def.src_uris.len()];
    let dest_uri = &def.dest_uris[attempt % def.dest_uris.len()];
    let src_t = uri::parse(src_uri, vhost).map_err(anyhow::Error::msg)?;
    let dest_t = uri::parse(dest_uri, vhost).map_err(anyhow::Error::msg)?;
    let label = format!("Shovel {name}");
    let mut src = Session::connect(&src_t, broker, &label).await.context("source")?;
    let mut dst = Session::connect(&dest_t, broker, &label).await.context("destination")?;

    let (queue, length) = match (&def.src_queue, &def.src_exchange) {
        (Some(q), _) => (q.clone(), Some(src.ensure_queue(q, &def.src_queue_args).await.context("source")?)),
        (None, Some(x)) => {
            let q = src.declare_queue("", false, true, true, &FieldTable::new()).await.context("source")?;
            src.bind_queue(&q, x, &def.src_exchange_key, &FieldTable::new()).await.context("source")?;
            (q, None)
        }
        (None, None) => anyhow::bail!("no source queue or exchange"),
    };
    if let Some(q) = &def.dest_queue {
        dst.ensure_queue(q, &def.dest_queue_args).await.context("destination")?;
    }
    if def.ack_mode == AckMode::OnConfirm {
        dst.confirm_select().await.context("destination")?;
    }
    if total.is_none() {
        *total = match def.src_delete_after {
            DeleteAfter::Never => None,
            DeleteAfter::QueueLength => Some(length.unwrap_or(0)),
            DeleteAfter::Count(n) => Some(n),
        };
    }
    let limit = *total;
    if limit.is_some_and(|n| status.forwarded() >= n) {
        return Ok(Finished::Done);
    }
    let prefetch = match limit {
        Some(n) => def.src_prefetch.min((n - status.forwarded()).min(u32::MAX as u64) as u32).max(1),
        None => def.src_prefetch,
    };
    if let Some(n) = length {
        status.remaining.store(n as i64, std::sync::atomic::Ordering::Relaxed);
    }
    src.consume(&queue, prefetch, def.ack_mode == AckMode::NoAck, def.src_consumer_args.clone(), true).await.context("source")?;
    status.set_connection(Some(src.connection_name()));
    status.set(State::Running, None);
    info!(%vhost, shovel = %name, "shovel running");

    let forward = def.add_forward_headers.then(|| forward_entry(broker, vhost, name, def, &src_t, &dest_t));
    let pump = Pump {
        src: &mut src,
        dst: &mut dst,
        status,
        ack: def.ack_mode,
        limit,
        gate: None,
        src_queue: def.src_queue.clone(),
    };
    pump.run(|m| {
        let mut props = m.msg.props.clone();
        if let Some(pp) = &def.publish_properties {
            override_props(&mut props, pp);
        }
        if let Some(entry) = &forward {
            let headers = props.headers.get_or_insert_with(FieldTable::new);
            let mut list = vec![FieldValue::Table(entry.clone())];
            if let Some(FieldValue::Array(prev)) = headers.remove("x-shovelled") {
                list.extend(prev);
            }
            headers.insert("x-shovelled", FieldValue::Array(list));
        }
        if def.add_timestamp_header {
            props.headers.get_or_insert_with(FieldTable::new).insert("x-shovelled-timestamp", FieldValue::Timestamp(mq_core::now_ms() / 1000));
        }
        let (exchange, key) = match (&def.dest_exchange, &def.dest_queue) {
            (Some(x), _) => (x.clone(), def.dest_exchange_key.clone().unwrap_or_else(|| m.msg.routing_key.clone())),
            (None, Some(q)) => (String::new(), q.clone()),
            (None, None) => (m.msg.exchange.clone(), m.msg.routing_key.clone()),
        };
        Some(Out { msg: Message::new(exchange, key, props, m.msg.body.clone()), queue: None })
    })
    .await?;
    Ok(Finished::Done)
}

/// Properties set in `dest-publish-properties` replace the message's.
fn override_props(p: &mut Properties, o: &Properties) {
    macro_rules! set {
        ($($f:ident),*) => { $( if o.$f.is_some() { p.$f = o.$f.clone(); } )* };
    }
    set!(content_type, content_encoding, delivery_mode, priority, correlation_id, reply_to, expiration, message_id, timestamp, kind, user_id, app_id, cluster_id);
    if let Some(h) = &o.headers {
        let headers = p.headers.get_or_insert_with(FieldTable::new);
        for (k, v) in h.iter() {
            headers.insert(k, v.clone());
        }
    }
}

/// The `x-shovelled` entry RabbitMQ adds with `dest-add-forward-headers`.
fn forward_entry(broker: &Broker, vhost: &str, name: &str, def: &ShovelDef, src: &Target, dest: &Target) -> FieldTable {
    let shown = |t: &Target, raw: &str| match t {
        Target::Local { .. } => "amqp://".to_string(),
        Target::Remote { .. } => uri::redact(raw),
    };
    let mut t = FieldTable::new();
    t.insert("shovelled-by", broker.config.node_name.as_str());
    t.insert("shovel-type", "dynamic");
    t.insert("shovel-name", name);
    t.insert("shovel-vhost", vhost);
    t.insert("src-uri", shown(src, &def.src_uris[0]));
    if let Some(q) = &def.src_queue {
        t.insert("src-queue", q.as_str());
    }
    if let Some(x) = &def.src_exchange {
        t.insert("src-exchange", x.as_str());
        t.insert("src-exchange-key", def.src_exchange_key.as_str());
    }
    t.insert("dest-uri", shown(dest, &def.dest_uris[0]));
    if let Some(q) = &def.dest_queue {
        t.insert("dest-queue", q.as_str());
    }
    if let Some(x) = &def.dest_exchange {
        t.insert("dest-exchange", x.as_str());
    }
    if let Some(k) = &def.dest_exchange_key {
        t.insert("dest-exchange-key", k.as_str());
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rabbitmq_keys() {
        let d = ShovelDef::parse(&json!({
            "src-uri": ["amqp://", "amqp://h"], "src-queue": "a", "src-delete-after": "queue-length",
            "dest-uri": "amqp://remote", "dest-exchange": "x", "dest-add-forward-headers": true,
            "ack-mode": "on-publish", "reconnect-delay": 0.5, "src-prefetch-count": 10,
            "dest-publish-properties": {"delivery_mode": 2}
        }))
        .unwrap();
        assert_eq!(d.src_uris.len(), 2);
        assert_eq!(d.src_delete_after, DeleteAfter::QueueLength);
        assert_eq!(d.ack_mode, AckMode::OnPublish);
        assert_eq!(d.src_prefetch, 10);
        assert_eq!(d.reconnect_delay, Duration::from_millis(500));
        assert_eq!(d.publish_properties.unwrap().delivery_mode, Some(2));
        assert!(d.add_forward_headers);
        let d = ShovelDef::parse(&json!({"src-uri": "amqp://", "src-exchange": "x", "dest-uri": "amqp://", "dest-queue": "q", "src-delete-after": 5})).unwrap();
        assert_eq!((d.src_exchange_key.as_str(), d.src_delete_after, d.ack_mode), ("#", DeleteAfter::Count(5), AckMode::OnConfirm));
    }

    #[test]
    fn rejects_bad_definitions() {
        assert!(ShovelDef::parse(&json!({"src-uri": "amqp://", "dest-uri": "amqp://", "dest-queue": "q"})).is_err());
        assert!(ShovelDef::parse(&json!({"src-uri": "amqp://", "src-queue": "a", "src-exchange": "x", "dest-uri": "amqp://"})).is_err());
        assert!(ShovelDef::parse(&json!({"src-uri": "http://x", "src-queue": "a", "dest-uri": "amqp://"})).is_err());
        assert!(ShovelDef::parse(&json!({"src-uri": "amqp://", "src-queue": "a", "dest-uri": "amqp://", "ack-mode": "never"})).is_err());
        assert!(ShovelDef::parse(&json!({"src-uri": "amqp://", "src-queue": "a"})).is_err());
    }

    #[test]
    fn backoff_grows_and_caps() {
        let d = Duration::from_secs(1);
        assert_eq!(backoff(d, 1), d);
        assert_eq!(backoff(d, 3), Duration::from_secs(4));
        assert_eq!(backoff(d, 30), Duration::from_secs(60));
        assert_eq!(backoff(Duration::from_secs(120), 3), Duration::from_secs(120));
    }
}

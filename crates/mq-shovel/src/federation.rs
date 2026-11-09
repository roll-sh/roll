//! Federation: exchanges and queues that pull messages from upstream brokers, configured with
//! `federation-upstream` / `federation-upstream-set` parameters and policies, the way
//! RabbitMQ's federation plugin does it.
//!
//! - Exchange links declare `federation: X -> <cluster>` (a queue) and `… A` (an internal
//!   fanout exchange) on the upstream, mirror the local exchange's bindings onto the upstream
//!   exchange, and republish what arrives locally with an `x-received-from` hop added.
//!   `max-hops` and cycle detection (our cluster name already in the history) drop messages.
//! - Queue links consume from the upstream queue only while the local queue has consumers
//!   and nothing ready for them.

use crate::endpoint::{Control, Session};
use crate::pump::{AckMode, Out, Pump};
use crate::shovel::{backoff, seconds, uris};
use crate::status::{LinkStatus, State};
use crate::uri::{self, Target};
use anyhow::{Context, anyhow};
use mq_core::message::Message;
use mq_core::policy::{self, Target as PolicyTarget};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::{Broker, VHost};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkKind {
    Exchange,
    Queue,
}

/// One federation link: a local exchange or queue and one of its upstreams.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkKey {
    pub vhost: String,
    pub kind: LinkKind,
    pub local: String,
    pub upstream: String,
}

/// A parsed `federation-upstream` value.
#[derive(Clone, Debug)]
pub struct UpstreamDef {
    pub uris: Vec<String>,
    pub exchange: Option<String>,
    pub queue: Option<String>,
    pub prefetch: u32,
    pub reconnect_delay: Duration,
    pub max_hops: u64,
    pub expires: Option<i64>,
    pub message_ttl: Option<i64>,
    pub ack_mode: AckMode,
    pub trust_user_id: bool,
}

impl UpstreamDef {
    pub fn parse(v: &Value) -> Result<Self, String> {
        if !v.is_object() {
            return Err("federation upstream should be an object".into());
        }
        let s = |k: &str| -> Result<Option<String>, String> {
            match v.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(o) => Err(format!("{k} should be a string, got {o}")),
            }
        };
        let n = |k: &str| -> Result<Option<i64>, String> {
            match v.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(x) => x.as_i64().filter(|n| *n >= 0).map(Some).ok_or_else(|| format!("{k} should be a non-negative integer, got {x}")),
            }
        };
        let ack_mode = match s("ack-mode")? {
            None => AckMode::OnConfirm,
            Some(m) => AckMode::parse(&m).ok_or_else(|| format!("ack-mode should be on-confirm, on-publish or no-ack, got {m}"))?,
        };
        let max_hops = n("max-hops")?.unwrap_or(1);
        if max_hops < 1 {
            return Err("max-hops should be at least 1".into());
        }
        Ok(UpstreamDef {
            uris: uris(v, "uri")?,
            exchange: s("exchange")?,
            queue: s("queue")?,
            prefetch: n("prefetch-count")?.unwrap_or(1000).min(u32::MAX as i64) as u32,
            reconnect_delay: seconds(v, "reconnect-delay", 5.0)?,
            max_hops: max_hops as u64,
            expires: n("expires")?,
            message_ttl: n("message-ttl")?,
            ack_mode,
            trust_user_id: v.get("trust-user-id").and_then(|b| b.as_bool()).unwrap_or(false),
        })
    }
}

/// Validate a `federation-upstream` parameter value.
pub fn validate_upstream(v: &Value) -> Result<(), String> {
    UpstreamDef::parse(v).map(|_| ())
}

/// Validate a `federation-upstream-set` parameter value: `[{"upstream": name, …}, …]`.
pub fn validate_upstream_set(v: &Value) -> Result<(), String> {
    let list = v.as_array().ok_or("federation-upstream-set should be a list")?;
    for e in list {
        if e.get("upstream").and_then(|u| u.as_str()).is_none() {
            return Err(format!("upstream set member {e} needs an \"upstream\" name"));
        }
    }
    Ok(())
}

/// Upstreams (name, effective definition) named by a policy definition.
fn resolve(broker: &Broker, vhost: &str, definition: &Value) -> Vec<(String, Value)> {
    let upstream = |name: &str| broker.parameter("federation-upstream", vhost, name);
    if let Some(name) = definition.get("federation-upstream").and_then(|v| v.as_str()) {
        return upstream(name).map(|v| vec![(name.to_string(), v)]).unwrap_or_default();
    }
    let Some(set) = definition.get("federation-upstream-set").and_then(|v| v.as_str()) else { return vec![] };
    if set == "all" {
        return broker.parameters(Some("federation-upstream"), Some(vhost)).into_iter().map(|(_, _, n, v)| (n, v)).collect();
    }
    let members = broker.parameter("federation-upstream-set", vhost, set).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    members
        .iter()
        .filter_map(|m| {
            let name = m.get("upstream")?.as_str()?;
            let mut def = upstream(name)?;
            if let (Some(d), Some(o)) = (def.as_object_mut(), m.as_object()) {
                for (k, v) in o.iter().filter(|(k, _)| *k != "upstream") {
                    d.insert(k.clone(), v.clone());
                }
            }
            Some((name.to_string(), def))
        })
        .collect()
}

/// Every link the current policies ask for, with its effective upstream definition.
pub fn desired(broker: &Broker) -> BTreeMap<LinkKey, Value> {
    let policies = broker.policies.read().clone();
    let mut out = BTreeMap::new();
    for vh in broker.vhosts() {
        if broker.parameters(Some("federation-upstream"), Some(&vh.name)).is_empty() {
            continue;
        }
        let mut add = |kind: LinkKind, local: &str, target: PolicyTarget| {
            let Some(p) = policy::pick(&policies, &vh.name, target, local, &broker.regex) else { return };
            for (upstream, def) in resolve(broker, &vh.name, &p.definition) {
                out.insert(LinkKey { vhost: vh.name.clone(), kind, local: local.to_string(), upstream }, def);
            }
        };
        for x in vh.exchanges() {
            if x.is_default() || x.internal || x.args.get("x-internal-purpose").is_some() {
                continue;
            }
            add(LinkKind::Exchange, &x.name, PolicyTarget::Exchange);
        }
        for q in vh.queues() {
            if q.meta.exclusive() || q.meta.arguments.get("x-internal-purpose").is_some() {
                continue;
            }
            add(LinkKind::Queue, &q.meta.name, PolicyTarget::Queue(q.kind));
        }
    }
    out
}

/// `/api/federation-links` entry.
pub fn link_json(node: &str, key: &LinkKey, def: &Value, status: &LinkStatus) -> Value {
    let parsed = UpstreamDef::parse(def).ok();
    let mut o = serde_json::Map::new();
    o.insert("node".into(), json!(node));
    o.insert("vhost".into(), json!(key.vhost));
    o.insert("upstream".into(), json!(key.upstream));
    o.insert("id".into(), json!(link_id(key)));
    match key.kind {
        LinkKind::Exchange => {
            o.insert("type".into(), json!("exchange"));
            o.insert("exchange".into(), json!(key.local));
            let up = parsed.as_ref().and_then(|d| d.exchange.clone()).unwrap_or_else(|| key.local.clone());
            o.insert("upstream_exchange".into(), json!(up));
        }
        LinkKind::Queue => {
            o.insert("type".into(), json!("queue"));
            o.insert("queue".into(), json!(key.local));
            let up = parsed.as_ref().and_then(|d| d.queue.clone()).unwrap_or_else(|| key.local.clone());
            o.insert("upstream_queue".into(), json!(up));
        }
    }
    if let Some(u) = parsed.as_ref().and_then(|d| d.uris.first()) {
        o.insert("uri".into(), json!(uri::redact(u)));
    }
    let state = match status.state() {
        State::Running => "running",
        State::Starting => "starting",
        State::Terminated => "error",
    };
    o.insert("status".into(), json!(state));
    if let Some(r) = status.reason() {
        o.insert("error".into(), json!(r));
    }
    if let Some(c) = status.connection() {
        o.insert("local_connection".into(), json!(c));
    }
    o.insert("timestamp".into(), json!(status.timestamp()));
    o.insert("forwarded".into(), json!(status.forwarded()));
    Value::Object(o)
}

fn link_id(key: &LinkKey) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let kind = if key.kind == LinkKind::Exchange { "x" } else { "q" };
    for part in [key.vhost.as_str(), kind, &key.local, &key.upstream] {
        for b in part.bytes().chain([0]) {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{:08x}", h as u32)
}

/// Run a link until aborted, reconnecting with backoff.
pub async fn run(broker: Arc<Broker>, key: LinkKey, def: UpstreamDef, status: Arc<LinkStatus>) {
    let mut attempt = 0usize;
    let mut failures = 0u32;
    loop {
        status.set(State::Starting, None);
        let started = Instant::now();
        let r = match key.kind {
            LinkKind::Exchange => run_exchange(&broker, &key, &def, &status, attempt).await,
            LinkKind::Queue => run_queue(&broker, &key, &def, &status, attempt).await,
        };
        let reason = match r {
            Ok(()) => "link stopped".to_string(),
            Err(e) => format!("{e:#}"),
        };
        warn!(vhost = %key.vhost, local = %key.local, upstream = %key.upstream, error = %reason, "federation link failed");
        status.set(State::Terminated, Some(reason));
        status.set_connection(None);
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

fn label(key: &LinkKey) -> String {
    format!("Federation link (upstream: {}, {}: {})", key.upstream, if key.kind == LinkKind::Exchange { "exchange" } else { "queue" }, key.local)
}

async fn run_exchange(broker: &Arc<Broker>, key: &LinkKey, def: &UpstreamDef, status: &LinkStatus, attempt: usize) -> anyhow::Result<()> {
    let raw_uri = &def.uris[attempt % def.uris.len()];
    let target = uri::parse(raw_uri, &key.vhost).map_err(anyhow::Error::msg)?;
    let vh = broker.open_vhost(&key.vhost).await.ok_or_else(|| anyhow!("vhost '{}' does not exist", key.vhost))?;
    let local_x = vh.exchange(&key.local).ok_or_else(|| anyhow!("no exchange '{}' in vhost '{}'", key.local, key.vhost))?;
    let upstream_x = def.exchange.clone().unwrap_or_else(|| key.local.clone());
    let upstream_vhost = match &target {
        Target::Local { vhost } | Target::Remote { vhost, .. } => vhost.clone(),
    };
    let upstream_name = upstream_identity(broker, &target).await;
    let mut up = Session::connect(&target, broker, &label(key)).await.context("upstream")?;
    up.ensure_exchange(&upstream_x, local_x.kind.name(), local_x.durable, local_x.auto_delete, local_x.internal, &local_x.args)
        .await
        .context("upstream exchange")?;

    let node = &broker.config.cluster_name;
    let suffix = if upstream_vhost != key.vhost {
        format!(":{}:{}", key.vhost, key.local)
    } else if upstream_x != key.local {
        format!(":{}", key.local)
    } else {
        String::new()
    };
    let queue = format!("federation: {upstream_x} -> {node}{suffix}");
    let internal = format!("{queue} A");
    let mut purpose = FieldTable::new();
    purpose.insert("x-internal-purpose", "federation");
    up.delete_exchange(&internal).await.context("upstream")?;
    up.declare_exchange(&internal, "fanout", true, true, true, &purpose).await.context("upstream internal exchange")?;
    let mut qargs = purpose.clone();
    if let Some(e) = def.expires {
        qargs.insert("x-expires", FieldValue::I64(e));
    }
    if let Some(t) = def.message_ttl {
        qargs.insert("x-message-ttl", FieldValue::I64(t));
    }
    up.declare_queue(&queue, true, false, false, &qargs).await.context("upstream queue")?;
    up.bind_queue(&queue, &internal, "", &FieldTable::new()).await.context("upstream")?;

    let control = up.control().await.context("upstream")?;
    let mut mirrored = Vec::new();
    sync_bindings(&control, &vh, &key.local, &upstream_x, &internal, &mut mirrored).await.context("upstream bindings")?;
    let sync = {
        let (vh, local, upstream_x, internal) = (vh.clone(), key.local.clone(), upstream_x.clone(), internal.clone());
        AbortOnDrop(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Err(e) = sync_bindings(&control, &vh, &local, &upstream_x, &internal, &mut mirrored).await {
                    warn!(exchange = %local, error = %format!("{e:#}"), "federation binding sync failed");
                    return;
                }
            }
        }))
    };

    let mut dst = Session::connect(&Target::Local { vhost: key.vhost.clone() }, broker, &label(key)).await?;
    up.consume(&queue, def.prefetch, def.ack_mode == AckMode::NoAck, FieldTable::new(), true).await.context("upstream")?;
    status.set_connection(Some(dst.connection_name()));
    status.set(State::Running, None);
    info!(vhost = %key.vhost, exchange = %key.local, upstream = %key.upstream, "federation link running");

    let own = broker.config.cluster_name.clone();
    let shown_uri = uri::redact(raw_uri);
    let local = key.local.clone();
    let pump = Pump { src: &mut up, dst: &mut dst, status, ack: def.ack_mode, limit: None, gate: None, src_queue: None };
    let r = pump
        .run(|m| {
            let history: Vec<FieldValue> = m.msg.props.header("x-received-from").and_then(|v| v.as_array()).map(|a| a.to_vec()).unwrap_or_default();
            if history.len() as u64 >= def.max_hops {
                return None;
            }
            if history.iter().any(|h| h.as_table().and_then(|t| t.get_str("cluster-name")) == Some(own.as_str())) {
                return None;
            }
            let mut props = m.msg.props.clone();
            if !def.trust_user_id {
                props.user_id = None;
            }
            let mut hop = FieldTable::new();
            hop.insert("uri", shown_uri.as_str());
            hop.insert("exchange", upstream_x.as_str());
            hop.insert("redelivered", m.redelivered);
            hop.insert("cluster-name", upstream_name.as_str());
            let mut list = vec![FieldValue::Table(hop)];
            list.extend(history);
            props.headers.get_or_insert_with(FieldTable::new).insert("x-received-from", FieldValue::Array(list));
            Some(Out { msg: Message::new(local.as_str(), m.msg.routing_key.as_str(), props, m.msg.body.clone()), queue: None })
        })
        .await;
    drop(sync);
    r
}

async fn run_queue(broker: &Arc<Broker>, key: &LinkKey, def: &UpstreamDef, status: &LinkStatus, attempt: usize) -> anyhow::Result<()> {
    let raw_uri = &def.uris[attempt % def.uris.len()];
    let target = uri::parse(raw_uri, &key.vhost).map_err(anyhow::Error::msg)?;
    let vh = broker.open_vhost(&key.vhost).await.ok_or_else(|| anyhow!("vhost '{}' does not exist", key.vhost))?;
    let local_q = vh.queue(&key.local).ok_or_else(|| anyhow!("no queue '{}' in vhost '{}'", key.local, key.vhost))?;
    let upstream_q = def.queue.clone().unwrap_or_else(|| key.local.clone());
    let mut up = Session::connect(&target, broker, &label(key)).await.context("upstream")?;
    up.ensure_queue(&upstream_q, &local_q.meta.arguments).await.context("upstream queue")?;
    let mut dst = Session::connect(&Target::Local { vhost: key.vhost.clone() }, broker, &label(key)).await?;
    up.consume(&upstream_q, def.prefetch, def.ack_mode == AckMode::NoAck, FieldTable::new(), false).await.context("upstream")?;
    status.set_connection(Some(dst.connection_name()));
    status.set(State::Running, None);
    info!(vhost = %key.vhost, queue = %key.local, upstream = %key.upstream, "federation link running");

    // Pull while the local queue has consumers waiting; stop once a prefetch worth is ready.
    let shared = local_q.shared.clone();
    let high = def.prefetch.max(1) as u64;
    let mut on = false;
    let gate = move || {
        let consumers = shared.consumers.load(Ordering::Relaxed);
        let ready = shared.ready.load(Ordering::Relaxed);
        on = if on { consumers > 0 && ready < high } else { consumers > 0 && ready == 0 };
        on
    };
    let local = key.local.clone();
    let pump = Pump { src: &mut up, dst: &mut dst, status, ack: def.ack_mode, limit: None, gate: Some(Box::new(gate)), src_queue: None };
    pump.run(|m| {
        let mut props = m.msg.props.clone();
        if !def.trust_user_id {
            props.user_id = None;
        }
        let msg = Message::new(m.msg.exchange.as_str(), m.msg.routing_key.as_str(), props, m.msg.body.clone());
        Some(Out { msg, queue: Some(local.clone()) })
    })
    .await
}

/// Mirror the local exchange's bindings onto the upstream exchange → internal exchange.
async fn sync_bindings(
    control: &Control,
    vh: &VHost,
    local: &str,
    upstream_x: &str,
    internal: &str,
    mirrored: &mut Vec<(String, FieldTable)>,
) -> anyhow::Result<()> {
    let mut want: Vec<(String, FieldTable)> = Vec::new();
    for b in vh.exchange(local).map(|x| x.bindings()).unwrap_or_default() {
        if !contains(&want, &b.routing_key, &b.args) {
            want.push((b.routing_key, b.args));
        }
    }
    for (k, a) in &want {
        if !contains(mirrored, k, a) {
            control.bind_exchange(internal, upstream_x, k, a).await?;
            mirrored.push((k.clone(), a.clone()));
        }
    }
    let stale: Vec<(String, FieldTable)> = mirrored.iter().filter(|(k, a)| !contains(&want, k, a)).cloned().collect();
    for (k, a) in stale {
        control.unbind_exchange(internal, upstream_x, &k, &a).await?;
        mirrored.retain(|(mk, ma)| !(mk == &k && ma.equivalent(&a)));
    }
    Ok(())
}

fn contains(list: &[(String, FieldTable)], key: &str, args: &FieldTable) -> bool {
    list.iter().any(|(k, a)| k == key && a.equivalent(args))
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The upstream's cluster name, recorded in `x-received-from` for cycle detection. lapin
/// doesn't expose server properties, so plain-TCP upstreams are asked with a bare
/// `connection.start`; TLS upstreams fall back to the host name.
async fn upstream_identity(broker: &Broker, target: &Target) -> String {
    match target {
        Target::Local { .. } => broker.config.cluster_name.clone(),
        Target::Remote { host, port, tls, .. } => {
            if !tls && let Some(n) = probe_cluster_name(host, *port).await {
                return n;
            }
            host.clone()
        }
    }
}

async fn probe_cluster_name(host: &str, port: u16) -> Option<String> {
    let probe = async {
        let mut s = tokio::net::TcpStream::connect((host, port)).await.ok()?;
        s.write_all(b"AMQP\x00\x00\x09\x01").await.ok()?;
        let mut head = [0u8; 7];
        s.read_exact(&mut head).await.ok()?;
        let size = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
        if head[0] != 1 || !(6..1 << 20).contains(&size) {
            return None;
        }
        let mut payload = vec![0u8; size];
        s.read_exact(&mut payload).await.ok()?;
        // class + method + version major/minor, then the server properties table.
        let mut b = bytes::Bytes::from(payload).slice(6..);
        let props = FieldTable::decode(&mut b).ok()?;
        props.get_str("cluster_name").map(String::from)
    };
    tokio::time::timeout(Duration::from_secs(3), probe).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upstreams() {
        let d = UpstreamDef::parse(&json!({"uri": "amqp://h", "max-hops": 2, "expires": 1000, "trust-user-id": true})).unwrap();
        assert_eq!((d.max_hops, d.expires, d.prefetch, d.trust_user_id), (2, Some(1000), 1000, true));
        assert!(UpstreamDef::parse(&json!({"uri": "amqp://h", "max-hops": 0})).is_err());
        assert!(UpstreamDef::parse(&json!({"exchange": "x"})).is_err());
        assert!(validate_upstream_set(&json!([{"upstream": "a"}])).is_ok());
        assert!(validate_upstream_set(&json!([{"nope": "a"}])).is_err());
    }

    #[test]
    fn link_ids_are_stable() {
        let k = LinkKey { vhost: "/".into(), kind: LinkKind::Exchange, local: "x".into(), upstream: "u".into() };
        assert_eq!(link_id(&k), link_id(&k.clone()));
        assert_ne!(link_id(&k), link_id(&LinkKey { kind: LinkKind::Queue, ..k.clone() }));
    }
}

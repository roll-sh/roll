//! JSON views of broker objects in the management API's shapes.

use crate::Mgmt;
use crate::http::{enc, fmt_time};
use crate::stats::{self, Ages, Key, Snapshot};
use crate::sys;
use base64::Engine as _;
use mq_core::auth::{Permission, TopicPermission, User};
use mq_core::broker::{ChannelInfo, ConnectionInfo, PRODUCT, RABBITMQ_VERSION};
use mq_core::exchange::{Binding, DestKind, Exchange};
use mq_core::policy::{self, Policy, Target};
use mq_core::queue::config::DEFAULT_CONSUMER_TIMEOUT_MS;
use mq_core::queue::{ConsumerInfo, QueueHandle, QueueType};
use mq_core::value::FieldTable;
use mq_core::vhost::{VHost, VHostInfo};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

/// Bytes above which the node reports a memory alarm (RabbitMQ's 0.6 watermark).
pub(crate) const MEMORY_WATERMARK: f64 = 0.6;
/// RabbitMQ's default free disk space limit.
pub(crate) const DISK_FREE_LIMIT: u64 = 50_000_000;

pub(crate) fn exchange_types() -> Value {
    let t = |name: &str, description: &str| json!({"name": name, "description": description, "enabled": true});
    json!([
        t("direct", "AMQP direct exchange, as per the AMQP specification"),
        t("fanout", "AMQP fanout exchange, as per the AMQP specification"),
        t("headers", "AMQP headers exchange, as per the AMQP specification"),
        t("topic", "AMQP topic exchange, as per the AMQP specification"),
        t("x-consistent-hash", "Consistent Hashing Exchange"),
        t("x-delayed-message", "Delayed Message Exchange."),
        t("x-local-random", "Picks one random local binding (queue) to route via (to)."),
        t("x-message-deduplication", "Message Deduplication Exchange."),
        t("x-random", "Picks one random destination"),
    ])
}

/// Message stats (counters with `_details`) as a nested object.
fn message_stats(m: &Mgmt, key: &Key, snap: &Snapshot, ages: &Ages) -> Value {
    let only: Snapshot = snap.iter().filter(|(n, _)| stats::is_message_stat(n)).cloned().collect();
    let mut o = Map::new();
    m.stats.put(&mut o, key, &only, ages);
    Value::Object(o)
}

/// Lengths and byte counters at the top level of an object.
fn top_level_stats(m: &Mgmt, o: &mut Map<String, Value>, key: &Key, snap: &Snapshot, ages: &Ages) {
    let rest: Snapshot = snap.iter().filter(|(n, _)| !stats::is_message_stat(n)).cloned().collect();
    m.stats.put(o, key, &rest, ages);
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// The policy and operator policy that apply to a queue or exchange.
pub(crate) fn policies_for(m: &Mgmt, vhost: &str, target: Target, name: &str) -> (Option<Policy>, Option<Policy>) {
    let b = &m.broker;
    let p = policy::pick(&b.policies.read(), vhost, target, name, &b.regex).cloned();
    let o = policy::pick(&b.operator_policies.read(), vhost, target, name, &b.regex).cloned();
    (p, o)
}

/// Policy definition with operator policy limits applied (numeric limits take the minimum).
fn effective_definition(p: &Option<Policy>, o: &Option<Policy>) -> Value {
    let mut def = p.as_ref().map(|p| obj(p.definition.clone())).unwrap_or_default();
    if let Some(o) = o {
        for (k, v) in obj(o.definition.clone()) {
            let merged = match (def.get(&k).and_then(|x| x.as_i64()), v.as_i64()) {
                (Some(a), Some(b)) => json!(a.min(b)),
                _ => v,
            };
            def.insert(k, merged);
        }
    }
    Value::Object(def)
}

pub(crate) fn queue(m: &Mgmt, q: &QueueHandle, ages: &Ages) -> Value {
    let s = &q.shared;
    let (vh, name) = (&q.meta.vhost, &q.meta.name);
    // In a cluster the queue runs on its owner node.
    let node = match m.broker.placement().and_then(|p| p.locate(vh, name)) {
        Some(mq_core::cluster::Located::Remote { node, .. }) => format!("rabbit@{node}"),
        _ => m.node().to_string(),
    };
    let (p, o) = policies_for(m, vh, Target::Queue(q.kind), name);
    let consumers = s.consumers.load(Relaxed);
    let (ready_b, unacked_b) = (s.ready_bytes.load(Relaxed), s.unacked_bytes.load(Relaxed));
    let idle = s.last_activity_ms.load(Relaxed).max(0) as u64;
    let durable_store = q.meta.durable && q.meta.exclusive_owner.is_none();
    let mut out = obj(json!({
        "name": name,
        "vhost": vh,
        "type": q.kind.name(),
        "durable": q.meta.durable,
        "auto_delete": q.meta.auto_delete,
        "exclusive": q.meta.exclusive(),
        "arguments": q.meta.arguments.to_json(),
        "node": node,
        "state": "running",
        "policy": p.as_ref().map(|p| p.name.clone()),
        "operator_policy": o.as_ref().map(|p| p.name.clone()),
        "effective_policy_definition": effective_definition(&p, &o),
        "consumers": consumers,
        "consumer_capacity": if consumers > 0 { 1.0 } else { 0.0 },
        "consumer_utilisation": if consumers > 0 { 1.0 } else { 0.0 },
        "exclusive_consumer_tag": null,
        "single_active_consumer_tag": null,
        "head_message_timestamp": null,
        "memory": 0,
        "message_bytes": ready_b + unacked_b,
        "message_bytes_ready": ready_b,
        "message_bytes_unacknowledged": unacked_b,
        "message_bytes_ram": ready_b + unacked_b,
        "message_bytes_persistent": if durable_store { s.log_bytes.load(Relaxed) } else { 0 },
        "message_bytes_paged_out": 0,
        "messages_paged_out": 0,
        "messages_ram": s.messages(),
        "messages_ready_ram": s.ready.load(Relaxed),
        "messages_unacknowledged_ram": s.unacked.load(Relaxed),
        "messages_persistent": if durable_store { s.messages() } else { 0 },
        "reductions": 0,
        "reductions_details": {"rate": 0.0},
        "recoverable_slaves": null,
    }));
    if consumers == 0 && idle > 0 {
        out.insert("idle_since".into(), json!(fmt_time(idle)));
    }
    if let Some(owner) = q.meta.exclusive_owner
        && let Some(c) = m.broker.connection(owner)
    {
        out.insert("owner_pid_details".into(), json!({"name": c.name, "peer_host": c.peer.ip().to_string(), "peer_port": c.peer.port()}));
    }
    match q.kind {
        QueueType::Classic => {
            out.insert("storage_version".into(), json!(2));
        }
        QueueType::Quorum | QueueType::Stream => {
            out.insert("leader".into(), json!(node));
            out.insert("members".into(), json!([node]));
            out.insert("online".into(), json!([node]));
        }
    }
    if q.kind == QueueType::Quorum {
        let limit = q.meta.arguments.get_i64("x-delivery-limit").unwrap_or(mq_core::queue::config::DEFAULT_DELIVERY_LIMIT as i64);
        out.insert("delivery_limit".into(), json!(limit));
    }
    if q.kind == QueueType::Stream {
        out.insert("first_offset".into(), json!(s.first_offset.load(Relaxed)));
        out.insert("committed_offset".into(), json!(s.next_offset.load(Relaxed).saturating_sub(1)));
        out.insert("segments".into(), json!(1));
    }
    let key = Key::Queue(vh.clone(), name.clone());
    let snap = stats::queue_snapshot(q);
    top_level_stats(m, &mut out, &key, &snap, ages);
    out.insert("message_stats".into(), message_stats(m, &key, &snap, ages));
    out.insert("garbage_collection".into(), json!({"fullsweep_after": 65535, "max_heap_size": 0, "min_bin_vheap_size": 46422, "min_heap_size": 233, "minor_gcs": 0}));
    Value::Object(out)
}

pub(crate) fn exchange(m: &Mgmt, vhost: &str, ex: &Exchange, ages: &Ages) -> Value {
    let mut out = obj(json!({
        "name": ex.name,
        "vhost": vhost,
        "type": ex.kind.name(),
        "durable": ex.durable,
        "auto_delete": ex.auto_delete,
        "internal": ex.internal,
        "arguments": ex.args.to_json(),
        "user_who_performed_action": "rmq-internal",
    }));
    if !ex.is_default() {
        let (p, _) = policies_for(m, vhost, Target::Exchange, &ex.name);
        if let Some(p) = p {
            out.insert("policy".into(), json!(p.name));
        }
    }
    let key = Key::Exchange(vhost.into(), ex.name.clone());
    out.insert("message_stats".into(), message_stats(m, &key, &stats::exchange_snapshot(ex), ages));
    Value::Object(out)
}

/// RabbitMQ's `quote_plus` with `~` escaped (the properties key alphabet).
fn quote_binding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A stable hash of binding arguments, independent of key order.
fn args_hash(args: &FieldTable) -> String {
    let mut pairs: Vec<(String, String)> = args.iter().map(|(k, v)| (k.to_string(), v.to_json().to_string())).collect();
    pairs.sort();
    let canon: String = pairs.iter().map(|(k, v)| format!("{k}={v};")).collect();
    let h = mq_core::exchange::hash32(canon.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.to_be_bytes())
}

/// The `properties_key` that identifies a binding between a source and a destination.
pub(crate) fn props_key(routing_key: &str, args: &FieldTable) -> String {
    match (routing_key.is_empty(), args.is_empty()) {
        (true, true) => "~".into(),
        (false, true) => quote_binding(routing_key),
        _ => format!("{}~{}", quote_binding(routing_key), quote_binding(&args_hash(args))),
    }
}

pub(crate) fn binding(vhost: &str, b: &Binding) -> Value {
    json!({
        "source": b.source,
        "vhost": vhost,
        "destination": b.dest.name,
        "destination_type": match b.dest.kind { DestKind::Queue => "queue", DestKind::Exchange => "exchange" },
        "routing_key": b.routing_key,
        "arguments": b.args.to_json(),
        "properties_key": props_key(&b.routing_key, &b.args),
    })
}

/// The `Location` of a created binding, relative to the request path like RabbitMQ's.
pub(crate) fn binding_location(vhost: &str, source: &str, kind: &str, dest: &str, key: &str) -> String {
    format!("../../../../{}/e/{}/{kind}/{}/{}", enc(vhost), enc(source), enc(dest), enc(key))
}

pub(crate) fn vhost(m: &Mgmt, info: &VHostInfo, vh: Option<&Arc<VHost>>, ages: &Ages) -> Value {
    let dqt = info.default_queue_type.clone().unwrap_or_else(|| m.broker.config.default_queue_type.clone());
    let state = if vh.is_some() { "running" } else { "stopped" };
    let mut out = obj(json!({
        "name": info.name,
        "description": info.description,
        "tags": info.tags,
        "default_queue_type": dqt,
        "metadata": {"description": info.description, "tags": info.tags, "default_queue_type": dqt},
        "tracing": info.tracing,
        "protected_from_deletion": false,
        "cluster_state": {m.node(): state},
    }));
    if let Some(vh) = vh {
        let key = Key::VHost(info.name.clone());
        let snap = stats::vhost_snapshot(vh, &m.broker.connections());
        top_level_stats(m, &mut out, &key, &snap, ages);
        out.insert("message_stats".into(), message_stats(m, &key, &snap, ages));
    } else {
        for k in ["messages", "messages_ready", "messages_unacknowledged", "recv_oct", "send_oct"] {
            out.insert(k.into(), json!(0));
            out.insert(format!("{k}_details"), json!({"rate": 0.0}));
        }
    }
    Value::Object(out)
}

pub(crate) fn connection(m: &Mgmt, c: &ConnectionInfo, ages: &Ages) -> Value {
    let mut out = obj(c.to_json(m.node()));
    if let Some(n) = c.client_properties.get("connection_name").and_then(|v| v.as_str()) {
        out.insert("user_provided_name".into(), json!(n));
    }
    out.insert("connected_at".into(), json!(c.connected_at_ms));
    out.insert("ssl_protocol".into(), Value::Null);
    out.insert("peer_cert_subject".into(), Value::Null);
    out.insert("reductions".into(), json!(0));
    out.insert("reductions_details".into(), json!({"rate": 0.0}));
    out.insert("send_pend".into(), json!(0));
    out.insert("garbage_collection".into(), json!({"fullsweep_after": 65535, "max_heap_size": 0, "min_bin_vheap_size": 46422, "min_heap_size": 233, "minor_gcs": 0}));
    let key = Key::Conn(c.id);
    m.stats.put(&mut out, &key, &stats::conn_snapshot(c), ages);
    Value::Object(out)
}

pub(crate) fn channel_details(m: &Mgmt, ch: &ChannelInfo) -> Value {
    let (host, port) = m.broker.connection(ch.conn).map(|c| (c.peer.ip().to_string(), c.peer.port())).unwrap_or_default();
    json!({
        "name": format!("{} ({})", ch.conn_name, ch.number),
        "number": ch.number,
        "connection_name": ch.conn_name,
        "node": m.node(),
        "peer_host": host,
        "peer_port": port,
        "user": ch.user,
    })
}

pub(crate) fn channel(m: &Mgmt, ch: &ChannelInfo, ages: &Ages) -> Value {
    let (host, port) = m.broker.connection(ch.conn).map(|c| (c.peer.ip().to_string(), c.peer.port())).unwrap_or_default();
    let mut out = obj(ch.to_json(m.node()));
    out.insert("connection_details".into(), json!({"name": ch.conn_name, "peer_host": host, "peer_port": port}));
    out.insert("user_who_performed_action".into(), json!(ch.user));
    out.insert("pending_raft_commands".into(), json!(0));
    out.insert("cached_segments".into(), json!(0));
    out.insert("reductions".into(), json!(0));
    out.insert("reductions_details".into(), json!({"rate": 0.0}));
    out.insert("garbage_collection".into(), json!({"fullsweep_after": 65535, "max_heap_size": 0, "min_bin_vheap_size": 46422, "min_heap_size": 233, "minor_gcs": 0}));
    let key = Key::Channel(ch.id);
    out.insert("message_stats".into(), message_stats(m, &key, &stats::channel_snapshot(ch), ages));
    if ch.consumers.load(Relaxed) == 0 && ch.unacked.load(Relaxed) == 0 {
        out.insert("idle_since".into(), json!(fmt_time(mq_core::now_ms())));
    }
    Value::Object(out)
}

pub(crate) fn consumer(m: &Mgmt, vhost: &str, queue: &str, c: &ConsumerInfo, ch: Option<&Arc<ChannelInfo>>) -> Value {
    let prefetch = ch.map(|c| c.prefetch_count.load(Relaxed)).unwrap_or(0);
    json!({
        "consumer_tag": c.consumer_tag,
        "exclusive": c.exclusive,
        "ack_required": c.ack_required,
        "prefetch_count": prefetch,
        "active": c.active,
        "activity_status": if c.active { "up" } else { "waiting" },
        "arguments": c.arguments.to_json(),
        "consumer_timeout": DEFAULT_CONSUMER_TIMEOUT_MS,
        "priority": c.priority,
        "queue": {"name": queue, "vhost": vhost},
        "channel_details": ch.map(|ch| channel_details(m, ch)).unwrap_or_else(|| json!({})),
    })
}

pub(crate) fn user(u: &User) -> Value {
    json!({
        "name": u.name,
        "password_hash": u.password_hash,
        "hashing_algorithm": u.hashing_algorithm,
        "tags": u.tags,
        "limits": u.limits,
    })
}

pub(crate) fn permission(vhost: &str, user: &str, p: &Permission) -> Value {
    json!({"user": user, "vhost": vhost, "configure": p.configure, "write": p.write, "read": p.read})
}

pub(crate) fn topic_permission(vhost: &str, user: &str, p: &TopicPermission) -> Value {
    json!({"user": user, "vhost": vhost, "exchange": p.exchange, "write": p.write, "read": p.read})
}

pub(crate) fn policy(p: &Policy) -> Value {
    json!({"vhost": p.vhost, "name": p.name, "pattern": p.pattern, "apply-to": p.apply_to, "definition": p.definition, "priority": p.priority})
}

pub(crate) fn listeners(m: &Mgmt) -> Value {
    let node = m.node();
    Value::Array(
        m.broker
            .listeners
            .read()
            .iter()
            .map(|l| json!({"node": node, "protocol": l.protocol, "ip_address": l.ip_address, "port": l.port, "socket_opts": {}}))
            .collect(),
    )
}

fn contexts(m: &Mgmt) -> Value {
    let port = m.broker.listeners.read().iter().find(|l| l.protocol == "http").map(|l| l.port).unwrap_or(m.config.listen.port());
    json!([
        {"node": m.node(), "description": "RabbitMQ Management", "path": "/", "port": port.to_string(), "protocol": "http"},
        {"node": m.node(), "description": "RabbitMQ Prometheus", "path": "/", "port": port.to_string(), "protocol": "http/prometheus"},
    ])
}

/// Resource alarms in effect, as RabbitMQ lists them.
pub(crate) fn alarms(m: &Mgmt) -> Value {
    let b = &m.broker;
    let disk = b.disk_alarm.load(Relaxed);
    let mem = b.mem_alarm.load(Relaxed) || (b.alarm.load(Relaxed) && !disk);
    let mut out = Vec::new();
    if mem {
        out.push(json!({"node": m.node(), "resource": "memory"}));
    }
    if disk {
        out.push(json!({"node": m.node(), "resource": "disk"}));
    }
    Value::Array(out)
}

pub(crate) fn mem_limit() -> u64 {
    (sys::total_memory() as f64 * MEMORY_WATERMARK) as u64
}

const PLUGINS: &[&str] = &[
    "rabbitmq_management",
    "rabbitmq_management_agent",
    "rabbitmq_prometheus",
    "rabbitmq_web_dispatch",
    "rabbitmq_consistent_hash_exchange",
    "rabbitmq_delayed_message_exchange",
    "rabbitmq_federation",
    "rabbitmq_shovel",
    "rabbitmq_mqtt",
    "rabbitmq_stomp",
    "rabbitmq_stream",
];

pub(crate) fn node(m: &Mgmt, memory: bool) -> Value {
    let b = &m.broker;
    let mem_used = sys::rss_bytes();
    let mem_limit = mem_limit();
    let disk_free = sys::disk_free(&m.config.data_dir);
    let fds = sys::max_fds();
    let alarm = b.alarm.load(Relaxed);
    let queues: usize = b.vhosts().iter().map(|v| v.queue_count()).sum();
    let procs = (queues + b.connections().len() + b.channels().len()) as u64;
    let uptime = mq_core::now_ms().saturating_sub(b.started_ms);
    let mut out = obj(json!({
        "name": m.node(),
        "type": "disc",
        "running": true,
        "being_drained": b.draining.load(Relaxed),
        "uptime": uptime,
        "os_pid": std::process::id().to_string(),
        "processors": sys::processors(),
        "run_queue": 0,
        "fd_used": sys::open_fds(),
        "fd_total": fds,
        "sockets_used": b.connections().len(),
        "sockets_total": fds,
        "mem_used": mem_used,
        "mem_limit": mem_limit,
        "mem_alarm": b.mem_alarm.load(Relaxed) || (alarm && !b.disk_alarm.load(Relaxed)),
        "mem_calculation_strategy": "rss",
        "disk_free": disk_free,
        "disk_free_limit": DISK_FREE_LIMIT,
        "disk_free_alarm": b.disk_alarm.load(Relaxed),
        "proc_used": procs,
        "proc_total": 1_048_576,
        "rates_mode": "basic",
        "net_ticktime": 60,
        "partitions": [],
        "cluster_links": [],
        "alarms": alarms(m),
        "enabled_plugins": PLUGINS,
        "applications": [
            {"name": "rabbit", "description": "RabbitMQ (roll queues)", "version": RABBITMQ_VERSION},
            {"name": "rabbitmq_management", "description": "RabbitMQ Management Console", "version": RABBITMQ_VERSION},
            {"name": "rabbitmq_prometheus", "description": "Prometheus metrics for RabbitMQ", "version": RABBITMQ_VERSION},
        ],
        "auth_mechanisms": [
            {"name": "PLAIN", "description": "SASL PLAIN authentication mechanism", "enabled": true},
            {"name": "AMQPLAIN", "description": "QPid AMQPLAIN mechanism", "enabled": true},
        ],
        "exchange_types": exchange_types(),
        "contexts": contexts(m),
        "listeners": listeners(m),
        "log_files": [],
        "config_files": [],
        "db_dir": m.config.data_dir.display().to_string(),
        "rabbitmq_version": RABBITMQ_VERSION,
        "product_name": PRODUCT,
        "product_version": RABBITMQ_VERSION,
        "erlang_version": "26.2.5",
        "erlang_full_version": "roll queues (Rust)",
        "crypto_lib_version": "",
        "metrics_gc_queue_length": {},
        "gc_num": 0, "gc_num_details": {"rate": 0.0},
        "gc_bytes_reclaimed": 0, "gc_bytes_reclaimed_details": {"rate": 0.0},
        "context_switches": 0, "context_switches_details": {"rate": 0.0},
        "io_read_count": 0, "io_read_count_details": {"rate": 0.0},
        "io_write_count": 0, "io_write_count_details": {"rate": 0.0},
        "mnesia_ram_tx_count": 0, "mnesia_disk_tx_count": 0,
        "msg_store_read_count": 0, "msg_store_write_count": 0,
        "queue_index_read_count": 0, "queue_index_write_count": 0,
    }));
    let churn = m.stats.churn();
    let mut churn_obj = Map::new();
    m.stats.put(&mut churn_obj, &Key::Churn, &churn.snapshot(), &Ages::default());
    out.extend(churn_obj);
    for k in ["mem_used", "fd_used", "disk_free", "proc_used", "sockets_used"] {
        out.insert(format!("{k}_details"), json!({"rate": 0.0}));
    }
    if memory {
        out.insert("memory".into(), memory_breakdown(mem_used));
    }
    Value::Object(out)
}

pub(crate) fn memory_breakdown(total: u64) -> Value {
    json!({
        "connection_readers": 0, "connection_writers": 0, "connection_channels": 0, "connection_other": 0,
        "queue_procs": 0, "quorum_queue_procs": 0, "stream_queue_procs": 0, "stream_queue_replica_reader_procs": 0,
        "stream_queue_coordinator_procs": 0, "plugins": 0, "metadata_store": 0, "other_proc": total, "metrics": 0,
        "mgmt_db": 0, "mnesia": 0, "quorum_ets": 0, "metadata_store_ets": 0, "other_ets": 0, "binary": 0,
        "msg_index": 0, "code": 0, "atom": 0, "other_system": 0, "allocated_unused": 0, "reserved_unallocated": 0,
        "strategy": "rss",
        "total": {"rss": total, "allocated": total, "erlang": total},
    })
}

/// Cluster name: the `cluster_name` global parameter if set, else the broker's.
pub(crate) fn cluster_name(m: &Mgmt) -> String {
    m.broker
        .global_parameters()
        .into_iter()
        .find(|(n, _)| n == "cluster_name")
        .and_then(|(_, v)| v.as_str().map(String::from))
        .unwrap_or_else(|| m.broker.config.cluster_name.clone())
}

/// A stable cluster identifier (`internal_cluster_id`, or one derived from the cluster name).
pub(crate) fn cluster_id(m: &Mgmt) -> String {
    m.broker
        .global_parameters()
        .into_iter()
        .find(|(n, _)| n == "internal_cluster_id")
        .and_then(|(_, v)| v.as_str().map(String::from))
        .unwrap_or_else(|| {
            let h = mq_core::exchange::hash32(format!("{}/{}", m.broker.config.cluster_name, m.node()).as_bytes());
            format!("rabbitmq-cluster-id-{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.to_be_bytes()))
        })
}

pub(crate) fn overview(m: &Mgmt, visible: &[String], ages: &Ages) -> Value {
    let b = &m.broker;
    let in_scope = |vh: &str| visible.iter().any(|v| v == vh);
    let vhosts: Vec<Arc<VHost>> = b.vhosts().into_iter().filter(|v| in_scope(&v.name)).collect();
    let (mut queues, mut exchanges, mut consumers) = (0u64, 0u64, 0u64);
    for vh in &vhosts {
        queues += vh.queue_count() as u64;
        exchanges += vh.exchanges().len() as u64;
        consumers += vh.queues().iter().map(|q| q.shared.consumers.load(Relaxed)).sum::<u64>();
    }
    let conns = b.connections().iter().filter(|c| in_scope(&c.vhost)).count();
    let chans = b.channels().iter().filter(|c| in_scope(&c.vhost)).count();
    let global = m.stats.global(b);
    let mut totals = Map::new();
    let lengths: Snapshot = global.iter().filter(|(n, _)| n.starts_with("messages")).cloned().collect();
    m.stats.put(&mut totals, &Key::Global, &lengths, ages);
    let mut churn = Map::new();
    m.stats.put(&mut churn, &Key::Churn, &m.stats.churn().snapshot(), &Ages::default());
    let msg: Snapshot = global.iter().filter(|(n, _)| stats::is_message_stat(n) && *n != "publish_in" && *n != "routed").cloned().collect();
    json!({
        "management_version": RABBITMQ_VERSION,
        "rates_mode": "basic",
        "sample_retention_policies": {"global": [600, 3600, 28800, 86400], "basic": [600, 3600], "detailed": [600]},
        "exchange_types": exchange_types(),
        "product_version": RABBITMQ_VERSION,
        "product_name": PRODUCT,
        "rabbitmq_version": RABBITMQ_VERSION,
        "cluster_name": cluster_name(m),
        "erlang_version": "26.2.5",
        "erlang_full_version": "roll queues (Rust)",
        "crypto_lib_version": "",
        "release_series_support_status": "supported",
        "disable_stats": false,
        "is_op_policy_updating_enabled": true,
        "enable_queue_totals": false,
        "message_stats": message_stats(m, &Key::Global, &msg, ages),
        "churn_rates": churn,
        "queue_totals": totals,
        "object_totals": {"channels": chans, "connections": conns, "consumers": consumers, "exchanges": exchanges, "queues": queues},
        "statistics_db_event_queue": 0,
        "node": m.node(),
        "listeners": listeners(m),
        "contexts": contexts(m),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mq_core::value::FieldValue;

    #[test]
    fn properties_keys() {
        let none = FieldTable::new();
        assert_eq!(props_key("", &none), "~");
        assert_eq!(props_key("a.b", &none), "a.b");
        assert_eq!(props_key("#", &none), "%23");
        assert_eq!(props_key("a~b c", &none), "a%7Eb+c");
        let a: FieldTable = [("x", FieldValue::I64(1)), ("y", FieldValue::str("z"))].into_iter().collect();
        let b: FieldTable = [("y", FieldValue::str("z")), ("x", FieldValue::I64(1))].into_iter().collect();
        let k = props_key("k", &a);
        assert!(k.starts_with("k~"));
        assert_eq!(k, props_key("k", &b), "argument order doesn't matter");
        assert_eq!(binding_location("/", "x", "q", "q1", "%23"), "../../../../%2F/e/x/q/q1/%2523");
    }
}

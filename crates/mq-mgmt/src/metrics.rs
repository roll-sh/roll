//! Prometheus metrics in the `rabbitmq_prometheus` plugin's format: `/metrics` (aggregated),
//! `/metrics/per-object` (one series per queue, channel and connection) and
//! `/metrics/detailed?family=...&vhost=...` (selected families, `rabbitmq_detailed_` prefix).

use crate::Mgmt;
use crate::format;
use crate::http::parse_query;
use crate::sys;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use mq_core::broker::{PRODUCT, RABBITMQ_VERSION};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

type Labels = Vec<(&'static str, String)>;

/// Text exposition format writer.
#[derive(Default)]
struct Text {
    out: String,
    prefix: &'static str,
    /// Families already written (requested families overlap; each may appear once).
    seen: std::collections::HashSet<String>,
}

impl Text {
    fn metric(&mut self, name: &str, kind: &str, help: &str, samples: impl IntoIterator<Item = (Labels, f64)>) {
        let name = format!("{}{name}", self.prefix);
        if !self.seen.insert(name.clone()) {
            return;
        }
        let _ = writeln!(self.out, "# TYPE {name} {kind}");
        let _ = writeln!(self.out, "# HELP {name} {help}");
        for (labels, v) in samples {
            self.out.push_str(&name);
            if !labels.is_empty() {
                self.out.push('{');
                for (i, (k, val)) in labels.iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    let _ = write!(self.out, "{k}=\"{}\"", escape(val));
                }
                self.out.push('}');
            }
            let _ = writeln!(self.out, " {}", fmt_value(v));
        }
    }

    fn single(&mut self, name: &str, kind: &str, help: &str, v: f64) {
        self.metric(name, kind, help, [(vec![], v)]);
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn fmt_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 9.0e15 { format!("{}", v as i64) } else { format!("{v}") }
}

fn respond(body: String) -> Response {
    let mut r = body.into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"));
    r
}

fn info(m: &Mgmt, t: &mut Text) {
    t.metric(
        "build_info",
        "untyped",
        "RabbitMQ & Erlang/OTP version info",
        [(
            vec![
                ("rabbitmq_version", RABBITMQ_VERSION.to_string()),
                ("prometheus_plugin_version", RABBITMQ_VERSION.to_string()),
                ("prometheus_client_version", "4.11.0".to_string()),
                ("erlang_version", "26.2.5".to_string()),
                ("product_name", PRODUCT.to_string()),
                ("product_version", RABBITMQ_VERSION.to_string()),
            ],
            1.0,
        )],
    );
    t.metric(
        "identity_info",
        "untyped",
        "RabbitMQ node & cluster identity info",
        [(
            vec![
                ("rabbitmq_node", m.node().to_string()),
                ("rabbitmq_cluster", format::cluster_name(m)),
                ("rabbitmq_cluster_permanent_id", format::cluster_id(m)),
                ("rabbitmq_endpoint", "default".to_string()),
            ],
            1.0,
        )],
    );
}

/// A per-queue value, either one series per queue or summed.
struct Queues {
    rows: Vec<(String, String, Arc<mq_core::queue::QueueHandle>)>,
}

impl Queues {
    fn collect(m: &Mgmt, vhosts: Option<&[String]>) -> Self {
        let mut rows = Vec::new();
        for vh in m.broker.vhosts() {
            if vhosts.is_some_and(|v| !v.contains(&vh.name)) {
                continue;
            }
            for q in vh.queues() {
                rows.push((vh.name.clone(), q.meta.name.clone(), q));
            }
        }
        Queues { rows }
    }

    fn emit(&self, t: &mut Text, per_object: bool, name: &str, kind: &str, help: &str, f: impl Fn(&mq_core::queue::QueueHandle) -> f64) {
        if per_object {
            t.metric(name, kind, help, self.rows.iter().map(|(v, n, q)| (vec![("vhost", v.clone()), ("queue", n.clone())], f(q))));
        } else {
            t.single(name, kind, help, self.rows.iter().map(|(_, _, q)| f(q)).sum());
        }
    }
}

fn queue_families(t: &mut Text, qs: &Queues, per_object: bool) {
    let l = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed) as f64;
    qs.emit(t, per_object, "queue_messages_ready", "gauge", "Messages ready to be delivered to consumers", |q| l(&q.shared.ready));
    qs.emit(t, per_object, "queue_messages_unacked", "gauge", "Messages delivered to consumers but not yet acknowledged", |q| l(&q.shared.unacked));
    qs.emit(t, per_object, "queue_messages", "gauge", "Sum of ready and unacknowledged messages - total queue depth", |q| q.shared.messages() as f64);
    qs.emit(t, per_object, "queue_messages_ready_bytes", "gauge", "Size in bytes of ready messages", |q| l(&q.shared.ready_bytes));
    qs.emit(t, per_object, "queue_messages_unacked_bytes", "gauge", "Size in bytes of all unacknowledged messages", |q| l(&q.shared.unacked_bytes));
    qs.emit(t, per_object, "queue_messages_bytes", "gauge", "Size in bytes of ready and unacknowledged messages", |q| {
        l(&q.shared.ready_bytes) + l(&q.shared.unacked_bytes)
    });
    qs.emit(t, per_object, "queue_messages_persistent_bytes", "gauge", "Size in bytes of persistent messages", |q| l(&q.shared.log_bytes));
    qs.emit(t, per_object, "queue_consumers", "gauge", "Consumers on a queue", |q| l(&q.shared.consumers));
    qs.emit(t, per_object, "queue_consumer_capacity", "gauge", "Consumer capacity", |q| if q.shared.consumers.load(Relaxed) > 0 { 1.0 } else { 0.0 });
    qs.emit(t, per_object, "queue_messages_published_total", "counter", "Total number of messages published to queues", |q| l(&q.shared.published));
}

fn queue_delivery_families(t: &mut Text, qs: &Queues) {
    let l = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed) as f64;
    qs.emit(t, true, "queue_get_ack_total", "counter", "Total number of messages fetched from a queue with basic.get in manual acknowledgement mode", |q| l(&q.shared.got));
    qs.emit(t, true, "queue_get_total", "counter", "Total number of messages fetched from a queue with basic.get in automatic acknowledgement mode", |_| 0.0);
    qs.emit(t, true, "queue_messages_delivered_ack_total", "counter", "Total number of messages delivered from a queue to consumers in manual acknowledgement mode", |q| {
        l(&q.shared.delivered)
    });
    qs.emit(t, true, "queue_messages_delivered_total", "counter", "Total number of messages delivered from a queue to consumers in automatic acknowledgement mode", |_| 0.0);
    qs.emit(t, true, "queue_messages_redelivered_total", "counter", "Total number of messages redelivered from a queue to consumers", |q| l(&q.shared.redelivered));
    qs.emit(t, true, "queue_messages_acked_total", "counter", "Total number of messages acknowledged by consumers on a queue", |q| l(&q.shared.acked));
    qs.emit(t, true, "queue_get_empty_total", "counter", "Total number of times basic.get operations fetched no message on a queue", |_| 0.0);
}

fn connection_families(m: &Mgmt, t: &mut Text, per_object: bool, coarse: bool, detail: bool) {
    let conns = m.broker.connections();
    let mut emit = |name: &str, kind: &str, help: &str, f: &dyn Fn(&mq_core::broker::ConnectionInfo) -> f64| {
        if per_object {
            t.metric(name, kind, help, conns.iter().map(|c| (vec![("connection", c.name.clone())], f(c))));
        } else {
            t.single(name, kind, help, conns.iter().map(|c| f(c)).sum());
        }
    };
    if coarse {
        emit("connection_incoming_bytes_total", "counter", "Total number of bytes received on a connection", &|c| c.recv_oct.load(Relaxed) as f64);
        emit("connection_outgoing_bytes_total", "counter", "Total number of bytes sent on a connection", &|c| c.send_oct.load(Relaxed) as f64);
    }
    if detail {
        emit("connection_incoming_packets_total", "counter", "Total number of packets received on a connection", &|c| c.recv_cnt.load(Relaxed) as f64);
        emit("connection_outgoing_packets_total", "counter", "Total number of packets sent on a connection", &|c| c.send_cnt.load(Relaxed) as f64);
        emit("connection_pending_packets", "gauge", "Number of packets waiting to be sent on a connection", &|_| 0.0);
        emit("connection_channels", "gauge", "Channels on a connection", &|c| c.channels.load(Relaxed) as f64);
    }
}

fn channel_families(m: &Mgmt, t: &mut Text, per_object: bool, gauges: bool, counters: bool) {
    let chans = m.broker.channels();
    let mut emit = |name: &str, kind: &str, help: &str, f: &dyn Fn(&mq_core::broker::ChannelInfo) -> f64| {
        if per_object {
            t.metric(name, kind, help, chans.iter().map(|c| (vec![("channel", format!("{} ({})", c.conn_name, c.number))], f(c))));
        } else {
            t.single(name, kind, help, chans.iter().map(|c| f(c)).sum());
        }
    };
    let l = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed) as f64;
    if gauges {
        emit("channel_consumers", "gauge", "Consumers on a channel", &|c| l(&c.consumers));
        emit("channel_messages_unacked", "gauge", "Delivered but not yet acknowledged messages", &|c| l(&c.unacked));
        emit("channel_messages_unconfirmed", "gauge", "Published but not yet confirmed messages", &|_| 0.0);
        emit("channel_messages_uncommitted", "gauge", "Messages received in a transaction but not yet committed", &|_| 0.0);
        emit("channel_acks_uncommitted", "gauge", "Message acknowledgements in a transaction not yet committed", &|_| 0.0);
        emit("channel_prefetch", "gauge", "Total limit of unacknowledged messages for all consumers on a channel", &|c| c.prefetch_count.load(Relaxed) as f64);
        emit("channel_global_prefetch", "gauge", "Global limit of unacknowledged messages for all consumers on a channel", &|c| {
            c.global_prefetch_count.load(Relaxed) as f64
        });
    }
    if counters {
        emit("channel_messages_published_total", "counter", "Total number of messages published into an exchange on a channel", &|c| l(&c.published));
        emit("channel_messages_confirmed_total", "counter", "Total number of messages published into an exchange and confirmed on the channel", &|c| l(&c.confirmed));
        emit("channel_messages_unroutable_returned_total", "counter", "Total number of messages published as mandatory into an exchange and returned to the publisher as unroutable", &|c| {
            l(&c.returned)
        });
        emit("channel_messages_delivered_ack_total", "counter", "Total number of messages delivered to consumers in manual acknowledgement mode", &|c| l(&c.delivered));
        emit("channel_messages_redelivered_total", "counter", "Total number of messages redelivered to consumers", &|c| l(&c.redelivered));
        emit("channel_messages_acked_total", "counter", "Total number of messages acknowledged by consumers", &|c| l(&c.acked));
    }
}

fn churn_families(m: &Mgmt, t: &mut Text) {
    let c = m.stats.churn();
    t.single("connections_opened_total", "counter", "Total number of connections opened", c.connection_created as f64);
    t.single("connections_closed_total", "counter", "Total number of connections closed or terminated", c.connection_closed as f64);
    t.single("channels_opened_total", "counter", "Total number of channels opened", c.channel_created as f64);
    t.single("channels_closed_total", "counter", "Total number of channels closed", c.channel_closed as f64);
    t.single("queues_declared_total", "counter", "Total number of queues declared", c.queue_declared as f64);
    t.single("queues_created_total", "counter", "Total number of queues created", c.queue_created as f64);
    t.single("queues_deleted_total", "counter", "Total number of queues deleted", c.queue_deleted as f64);
}

fn node_families(m: &Mgmt, t: &mut Text) {
    let alarm = m.broker.alarm.load(Relaxed);
    let uptime = mq_core::now_ms().saturating_sub(m.broker.started_ms) as f64 / 1000.0;
    t.single("erlang_uptime_seconds", "gauge", "Node uptime", uptime.floor());
    t.single("process_open_fds", "gauge", "Open file descriptors", sys::open_fds() as f64);
    t.single("process_max_fds", "gauge", "Open file descriptors limit", sys::max_fds() as f64);
    t.single("process_resident_memory_bytes", "gauge", "Memory used in bytes", sys::rss_bytes() as f64);
    t.single("resident_memory_limit_bytes", "gauge", "Memory high watermark in bytes", format::mem_limit() as f64);
    t.single("disk_space_available_bytes", "gauge", "Disk space available in bytes", sys::disk_free(&m.config.data_dir) as f64);
    t.single("disk_space_available_limit_bytes", "gauge", "Free disk space low watermark in bytes", format::DISK_FREE_LIMIT as f64);
    t.single("alarms_memory_used_watermark", "gauge", "is set to 1 when node is above memory alarm watermark", if alarm { 1.0 } else { 0.0 });
    t.single("alarms_free_disk_space_watermark", "gauge", "is set to 1 when free disk space is below the alarm watermark", 0.0);
    t.single("alarms_file_descriptor_limit", "gauge", "is set to 1 when the node is near its file descriptor limit", 0.0);
}

fn global_families(m: &Mgmt, t: &mut Text) {
    let g = m.stats.global(&m.broker);
    let v = |n: &str| g.iter().find(|(k, _)| *k == n).map(|(_, v)| *v).unwrap_or(0.0);
    let p = || vec![("protocol", "amqp091".to_string())];
    let mut c = |name: &str, help: &str, val: f64| t.metric(name, "counter", help, [(p(), val)]);
    c("global_messages_received_total", "Total number of messages received from publishers", v("publish"));
    c("global_messages_received_confirm_total", "Total number of messages received from publishers expecting confirmations", v("confirm"));
    c("global_messages_routed_total", "Total number of messages routed to queues or streams", v("routed"));
    c("global_messages_unroutable_dropped_total", "Total number of messages published as non-mandatory into an exchange and dropped as unroutable", v("drop_unroutable"));
    c("global_messages_unroutable_returned_total", "Total number of messages published as mandatory into an exchange and returned to the publisher as unroutable", v("return_unroutable"));
    c("global_messages_confirmed_total", "Total number of messages confirmed to publishers", v("confirm"));
    c("global_messages_delivered_total", "Total number of messages delivered to consumers", v("deliver_get"));
    c("global_messages_delivered_consume_manual_ack_total", "Total number of messages delivered to consumers using basic.consume with manual acknowledgment", v("deliver"));
    c("global_messages_delivered_get_manual_ack_total", "Total number of messages delivered to consumers using basic.get with manual acknowledgment", v("get"));
    c("global_messages_acknowledged_total", "Total number of messages acknowledged by consumers", v("ack"));
    let consumers: u64 = m.broker.vhosts().iter().flat_map(|vh| vh.queues()).map(|q| q.shared.consumers.load(Relaxed)).sum();
    t.metric("global_consumers", "gauge", "Current number of consumers", [(p(), consumers as f64)]);
}

fn core(m: &Mgmt, per_object: bool) -> String {
    let mut t = Text { prefix: "rabbitmq_", ..Default::default() };
    info(m, &mut t);
    node_families(m, &mut t);
    let b = &m.broker;
    let queues: usize = b.vhosts().iter().map(|v| v.queue_count()).sum();
    let consumers: u64 = b.vhosts().iter().flat_map(|vh| vh.queues()).map(|q| q.shared.consumers.load(Relaxed)).sum();
    t.single("connections", "gauge", "Connections currently open", b.connections().len() as f64);
    t.single("channels", "gauge", "Channels currently open", b.channels().len() as f64);
    t.single("consumers", "gauge", "Consumers currently connected", consumers as f64);
    t.single("queues", "gauge", "Queues available", queues as f64);
    churn_families(m, &mut t);
    queue_families(&mut t, &Queues::collect(m, None), per_object);
    connection_families(m, &mut t, per_object, true, true);
    channel_families(m, &mut t, per_object, true, true);
    global_families(m, &mut t);
    t.out
}

pub(crate) async fn aggregated(State(m): State<Arc<Mgmt>>) -> Response {
    m.touch();
    respond(core(&m, false))
}

pub(crate) async fn per_object(State(m): State<Arc<Mgmt>>) -> Response {
    m.touch();
    respond(core(&m, true))
}

pub(crate) async fn detailed(State(m): State<Arc<Mgmt>>, RawQuery(q): RawQuery) -> Response {
    m.touch();
    let query = parse_query(q.as_deref());
    let families: Vec<&str> = query.iter().filter(|(k, _)| k == "family").map(|(_, v)| v.as_str()).collect();
    let vhosts: Vec<String> = query.iter().filter(|(k, _)| k == "vhost").map(|(_, v)| v.clone()).collect();
    let scope = (!vhosts.is_empty()).then_some(vhosts.as_slice());
    let mut t = Text { prefix: "rabbitmq_", ..Default::default() };
    info(&m, &mut t);
    t.prefix = "rabbitmq_detailed_";
    let has = |f: &str| families.contains(&f);
    let qs = Queues::collect(&m, scope);
    if has("queue_coarse_metrics") {
        qs.emit(&mut t, true, "queue_messages_ready", "gauge", "Messages ready to be delivered to consumers", |q| q.shared.ready.load(Relaxed) as f64);
        qs.emit(&mut t, true, "queue_messages_unacked", "gauge", "Messages delivered to consumers but not yet acknowledged", |q| q.shared.unacked.load(Relaxed) as f64);
        qs.emit(&mut t, true, "queue_messages", "gauge", "Sum of ready and unacknowledged messages - total queue depth", |q| q.shared.messages() as f64);
    }
    if has("queue_consumer_count") {
        qs.emit(&mut t, true, "queue_consumers", "gauge", "Consumers on a queue", |q| q.shared.consumers.load(Relaxed) as f64);
    }
    if has("queue_metrics") {
        queue_families(&mut t, &qs, true);
    }
    if has("queue_delivery_metrics") {
        queue_delivery_families(&mut t, &qs);
    }
    if has("connection_coarse_metrics") || has("connection_metrics") {
        connection_families(&m, &mut t, true, has("connection_coarse_metrics"), has("connection_metrics"));
    }
    if has("channel_metrics") || has("channel_exchange_metrics") {
        channel_families(&m, &mut t, true, has("channel_metrics"), has("channel_exchange_metrics"));
    }
    if has("connection_churn_metrics") {
        churn_families(&m, &mut t);
    }
    if has("node_coarse_metrics") {
        node_families(&m, &mut t);
    }
    let vhs: Vec<_> = m.broker.vhosts().into_iter().filter(|v| scope.is_none_or(|s| s.contains(&v.name))).collect();
    if has("exchange_names") {
        t.metric(
            "exchange_name",
            "gauge",
            "Enumerates exchanges without any additional info. This value is cluster-wide.",
            vhs.iter().flat_map(|vh| vh.exchanges().into_iter().map(|x| (vec![("vhost", vh.name.clone()), ("exchange", x.name.clone()), ("type", x.kind.name().to_string())], 1.0))),
        );
    }
    if has("exchange_bindings") {
        t.metric(
            "exchange_bindings",
            "gauge",
            "Number of bindings for an exchange. This value is cluster-wide.",
            vhs.iter().flat_map(|vh| {
                vh.exchanges()
                    .into_iter()
                    .filter(|x| !x.is_default())
                    .map(|x| (vec![("vhost", vh.name.clone()), ("exchange", x.name.clone()), ("type", x.kind.name().to_string())], x.binding_count() as f64))
            }),
        );
    }
    if has("vhost_status") {
        t.metric("vhost_status", "gauge", "Whether a given vhost is running", vhs.iter().map(|vh| (vec![("vhost", vh.name.clone())], 1.0)));
    }
    respond(t.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_format() {
        let mut t = Text { prefix: "rabbitmq_", ..Default::default() };
        t.metric("queue_messages", "gauge", "Depth", [(vec![("vhost", "/".to_string()), ("queue", "a\"b".to_string())], 3.0)]);
        t.single("connections", "gauge", "Open", 0.5);
        assert_eq!(
            t.out,
            "# TYPE rabbitmq_queue_messages gauge\n# HELP rabbitmq_queue_messages Depth\nrabbitmq_queue_messages{vhost=\"/\",queue=\"a\\\"b\"} 3\n\
             # TYPE rabbitmq_connections gauge\n# HELP rabbitmq_connections Open\nrabbitmq_connections 0.5\n"
        );
    }
}

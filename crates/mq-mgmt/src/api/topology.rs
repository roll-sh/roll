//! Connections, channels, consumers, exchanges, queues, bindings and message endpoints.

use super::{body_args, body_bool, body_i64, body_str, can_see_vhost, is_monitor, open_vhost, vhosts_in_scope};
use crate::Mgmt;
use crate::format;
use crate::http::{ApiError, ApiResult, Req, created_or_updated, list, no_content, ok, shape};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use base64::Engine as _;
use bytes::Bytes;
use mq_core::auth::Access;
use mq_core::broker::{ChannelInfo, ConnectionInfo};
use mq_core::exchange::{Binding, Dest, DestKind, ExchangeKind};
use mq_core::message::{Message, Properties};
use mq_core::queue::{ConfirmTicket, Outcome};
use mq_core::value::FieldValue;
use mq_core::vhost::{DeclareQueue, VHost};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

/// The default exchange is `amq.default` in URLs (and may arrive as an empty segment).
fn exchange_name(seg: &str) -> &str {
    if seg == "amq.default" { "" } else { seg }
}

fn check(m: &Mgmt, req: &Req, vhost: &str, access: Access, kind: &str, name: &str) -> ApiResult<()> {
    m.broker.check(&req.user, vhost, access, kind, name).map_err(ApiError::from)
}

// ── connections & channels ──

fn sees_connection(req: &Req, c: &ConnectionInfo) -> bool {
    is_monitor(&req.user) || c.user == req.user.name
}

pub(super) fn connections(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    let ages = req.ages();
    let items = m
        .broker
        .connections()
        .iter()
        .filter(|c| vhost.is_none_or(|v| c.vhost == v) && sees_connection(req, c))
        .map(|c| format::connection(m, c, &ages))
        .collect();
    list(req, items)
}

fn find_connection(m: &Mgmt, req: &Req, name: &str) -> ApiResult<Arc<ConnectionInfo>> {
    let c = m.broker.connection_by_name(name).ok_or_else(ApiError::not_found)?;
    if !sees_connection(req, &c) {
        return Err(ApiError::unauthorized("Not authorised to access this connection"));
    }
    Ok(c)
}

pub(super) fn connection(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    let c = find_connection(m, req, name)?;
    Ok(ok(shape(req, format::connection(m, &c, &req.ages()))))
}

fn close_reason(req: &Req) -> String {
    req.headers.get("x-reason").and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty()).unwrap_or("Closed via management plugin").to_string()
}

pub(super) fn close_connection(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    let c = find_connection(m, req, name)?;
    c.request_close(close_reason(req));
    Ok(no_content())
}

pub(super) fn user_connections(m: &Mgmt, req: &Req, user: &str) -> ApiResult {
    if user != req.user.name && !is_monitor(&req.user) {
        return Err(ApiError::unauthorized("Not monitor user"));
    }
    let ages = req.ages();
    let items = m.broker.connections().iter().filter(|c| c.user == user).map(|c| format::connection(m, c, &ages)).collect();
    list(req, items)
}

pub(super) fn close_user_connections(m: &Mgmt, req: &Req, user: &str) -> ApiResult {
    if user != req.user.name && !req.user.is_admin() {
        return Err(ApiError::unauthorized("Not administrator user"));
    }
    let reason = close_reason(req);
    for c in m.broker.connections().iter().filter(|c| c.user == user) {
        c.request_close(reason.clone());
    }
    Ok(no_content())
}

fn sees_channel(req: &Req, c: &ChannelInfo) -> bool {
    is_monitor(&req.user) || c.user == req.user.name
}

pub(super) fn channels(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    let ages = req.ages();
    let items = m
        .broker
        .channels()
        .iter()
        .filter(|c| vhost.is_none_or(|v| c.vhost == v) && sees_channel(req, c))
        .map(|c| format::channel(m, c, &ages))
        .collect();
    list(req, items)
}

pub(super) fn connection_channels(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    let c = find_connection(m, req, name)?;
    let ages = req.ages();
    let items = m.broker.channels().iter().filter(|ch| ch.conn == c.id).map(|ch| format::channel(m, ch, &ages)).collect();
    list(req, items)
}

pub(super) fn channel(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    let ch = m.broker.channels().into_iter().find(|c| format!("{} ({})", c.conn_name, c.number) == name).ok_or_else(ApiError::not_found)?;
    if !sees_channel(req, &ch) {
        return Err(ApiError::unauthorized("Not authorised to access this channel"));
    }
    let mut v = format::channel(m, &ch, &req.ages());
    v["publishes"] = json!([]);
    v["deliveries"] = json!([]);
    Ok(ok(shape(req, v)))
}

pub(super) async fn consumers(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    let chans: HashMap<u64, Arc<ChannelInfo>> = m.broker.channels().into_iter().map(|c| (c.id, c)).collect();
    let mut items = Vec::new();
    for vh in vhosts_in_scope(m, req, vhost).await? {
        for q in vh.queues().into_iter().filter(|q| q.shared.consumers.load(Relaxed) > 0) {
            for c in q.consumers().await {
                items.push(format::consumer(m, &vh.name, &q.meta.name, &c, chans.get(&c.channel)));
            }
        }
    }
    list(req, items)
}

// ── exchanges ──

pub(super) async fn exchanges(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    let ages = req.ages();
    let mut items = Vec::new();
    for vh in vhosts_in_scope(m, req, vhost).await? {
        items.extend(vh.exchanges().iter().map(|x| format::exchange(m, &vh.name, x, &ages)));
    }
    list(req, items)
}

pub(super) async fn exchange(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let x = vh.exchange(exchange_name(name)).ok_or_else(ApiError::not_found)?;
    let mut v = format::exchange(m, vhost, &x, &req.ages());
    v["incoming"] = json!([]);
    v["outgoing"] = json!([]);
    Ok(ok(shape(req, v)))
}

pub(super) async fn put_exchange(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let name = exchange_name(name);
    let body = req.json()?;
    let kind = body_str(&body, "type").ok_or_else(|| ApiError::bad_request("key_missing: type"))?;
    check(m, req, vhost, Access::Configure, "exchange", name)?;
    let created = vh.exchange(name).is_none();
    vh.declare_exchange(
        name,
        kind,
        body_bool(&body, "durable", true)?,
        body_bool(&body, "auto_delete", false)?,
        body_bool(&body, "internal", false)?,
        body_args(&body)?,
        false,
    )
    .await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_exchange(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let name = exchange_name(name);
    vh.exchange(name).ok_or_else(ApiError::not_found)?;
    check(m, req, vhost, Access::Configure, "exchange", name)?;
    vh.delete_exchange(name, req.flag("if-unused")).await?;
    Ok(no_content())
}

pub(super) async fn exchange_bindings(m: &Mgmt, req: &Req, vhost: &str, name: &str, source: bool) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let name = exchange_name(name);
    vh.exchange(name).ok_or_else(ApiError::not_found)?;
    let items = vh
        .bindings()
        .iter()
        .filter(|b| if source { b.source == name } else { b.dest.kind == DestKind::Exchange && b.dest.name == name })
        .map(|b| format::binding(vhost, b))
        .collect();
    list(req, items)
}

pub(super) async fn publish(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let name = exchange_name(name);
    let body = req.json()?;
    let routing_key = body_str(&body, "routing_key").ok_or_else(|| ApiError::bad_request("key_missing: routing_key"))?.to_string();
    let payload = body_str(&body, "payload").ok_or_else(|| ApiError::bad_request("key_missing: payload"))?;
    let payload = match body_str(&body, "payload_encoding").unwrap_or("string") {
        "string" => Bytes::copy_from_slice(payload.as_bytes()),
        "base64" => Bytes::from(base64::engine::general_purpose::STANDARD.decode(payload.trim()).map_err(|_| ApiError::bad_request("payload is not valid base64"))?),
        other => return Err(ApiError::bad_request(format!("unsupported payload_encoding '{other}'"))),
    };
    let props = match body.get("properties") {
        Some(p @ Value::Object(_)) => Properties::from_json(p),
        _ => Properties::default(),
    };
    check(m, req, vhost, Access::Write, "exchange", name)?;
    if let Some(uid) = &props.user_id
        && *uid != req.user.name
        && !req.user.has_tag("impersonator")
    {
        return Err(ApiError::bad_request(format!("user_id property set to '{uid}' but authenticated user was '{}'", req.user.name)));
    }
    if let Some(x) = vh.exchange(name)
        && x.kind == ExchangeKind::Topic
    {
        m.broker.check_topic(&req.user, vhost, name, &routing_key, Access::Write, None)?;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let ticket = ConfirmTicket::new(move |ok| {
        let _ = tx.send(ok);
    });
    let out = vh.publish(name, Message::new(name, routing_key, props, payload), Some(ticket)).await?;
    if let Some(t) = out.confirm {
        t.settle(true);
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
    Ok(ok(json!({"routed": out.routed > 0})))
}

// ── queues ──

pub(super) async fn queues(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    let ages = req.ages();
    let mut items = Vec::new();
    for vh in vhosts_in_scope(m, req, vhost).await? {
        items.extend(vh.queues().iter().map(|q| format::queue(m, q, &ages)));
    }
    list(req, items)
}

pub(super) async fn queue(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let q = vh.queue(name).ok_or_else(ApiError::not_found)?;
    let mut v = format::queue(m, &q, &req.ages());
    let chans: HashMap<u64, Arc<ChannelInfo>> = m.broker.channels().into_iter().map(|c| (c.id, c)).collect();
    let consumers: Vec<Value> = q.consumers().await.iter().map(|c| format::consumer(m, vhost, name, c, chans.get(&c.channel))).collect();
    v["consumer_details"] = Value::Array(consumers);
    v["incoming"] = json!([]);
    v["deliveries"] = json!([]);
    Ok(ok(shape(req, v)))
}

pub(super) async fn put_queue(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let body = req.json()?;
    let mut args = body_args(&body)?;
    if let Some(t) = body_str(&body, "type").filter(|t| !t.is_empty())
        && args.get("x-queue-type").is_none()
    {
        args.insert("x-queue-type", FieldValue::str(t));
    }
    if name.is_empty() {
        return Err(ApiError::bad_request("queue name must not be empty"));
    }
    check(m, req, vhost, Access::Configure, "queue", name)?;
    let created = vh.queue(name).is_none();
    vh.declare_queue(DeclareQueue {
        name: name.to_string(),
        durable: body_bool(&body, "durable", true)?,
        exclusive_owner: None,
        auto_delete: body_bool(&body, "auto_delete", false)?,
        args,
        passive: false,
    })
    .await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_queue(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    vh.queue(name).ok_or_else(ApiError::not_found)?;
    check(m, req, vhost, Access::Configure, "queue", name)?;
    vh.delete_queue(name, req.flag("if-unused"), req.flag("if-empty"), None).await?;
    Ok(no_content())
}

pub(super) async fn queue_bindings(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    vh.queue(name).ok_or_else(ApiError::not_found)?;
    let items = vh.bindings().iter().filter(|b| b.dest.kind == DestKind::Queue && b.dest.name == name).map(|b| format::binding(vhost, b)).collect();
    list(req, items)
}

pub(super) async fn purge(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let q = vh.queue(name).ok_or_else(ApiError::not_found)?;
    check(m, req, vhost, Access::Read, "queue", name)?;
    q.purge().await?;
    Ok(no_content())
}

pub(super) async fn queue_action(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    vh.queue(name).ok_or_else(ApiError::not_found)?;
    let body = req.json()?;
    match body_str(&body, "action") {
        // Classic queue mirroring doesn't exist in RabbitMQ 4; syncing is a no-op.
        Some("sync" | "cancel_sync") => Ok(no_content()),
        _ => Err(ApiError::bad_request("Unknown action")),
    }
}

pub(super) async fn get_messages(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let q = vh.queue(name).ok_or_else(ApiError::not_found)?;
    let body = req.json()?;
    let count = body_i64(&body, "count")?.unwrap_or(1).clamp(0, 10_000) as usize;
    let ackmode = match body_str(&body, "ackmode") {
        Some(a) => a.to_string(),
        None => match body_bool(&body, "requeue", true)? {
            true => "ack_requeue_true".into(),
            false => "ack_requeue_false".into(),
        },
    };
    let outcome = match ackmode.as_str() {
        "ack_requeue_true" | "reject_requeue_true" => Outcome::Requeue,
        "ack_requeue_false" => Outcome::Ack,
        "reject_requeue_false" => Outcome::Reject,
        other => return Err(ApiError::bad_request(format!("unknown ackmode '{other}'"))),
    };
    let encoding = body_str(&body, "encoding").unwrap_or("auto");
    if !matches!(encoding, "auto" | "base64") {
        return Err(ApiError::bad_request("Unsupported encoding. Please use auto or base64."));
    }
    let truncate = body_i64(&body, "truncate")?.filter(|t| *t >= 0).map(|t| t as usize);
    check(m, req, vhost, Access::Read, "queue", name)?;

    let chan = m.broker.next_id();
    let mut seqs = Vec::new();
    let mut out = Vec::new();
    for _ in 0..count {
        let Some(d) = q.get(chan, false).await? else { break };
        seqs.push(d.seq);
        vh.stats.delivered.fetch_add(1, Relaxed);
        vh.stats.delivered_bytes.fetch_add(d.msg.body.len() as u64, Relaxed);
        out.push(message_json(&d.msg, d.redelivered, d.remaining, encoding, truncate));
    }
    if !seqs.is_empty() {
        if outcome == Outcome::Ack {
            vh.stats.acked.fetch_add(seqs.len() as u64, Relaxed);
        }
        q.settle(chan, seqs, outcome).await;
    }
    q.channel_closed(chan).await;
    Ok(ok(Value::Array(out)))
}

fn message_json(msg: &Message, redelivered: bool, remaining: u32, encoding: &str, truncate: Option<usize>) -> Value {
    let body = match truncate {
        Some(t) if t < msg.body.len() => &msg.body[..t],
        _ => &msg.body[..],
    };
    let (payload, enc) = match (encoding, std::str::from_utf8(body)) {
        ("auto", Ok(s)) => (s.to_string(), "string"),
        _ => (base64::engine::general_purpose::STANDARD.encode(body), "base64"),
    };
    json!({
        "payload_bytes": msg.body.len(),
        "redelivered": redelivered,
        "exchange": msg.exchange,
        "routing_key": msg.routing_key,
        "message_count": remaining,
        "properties": msg.props.to_json(),
        "payload": payload,
        "payload_encoding": enc,
    })
}

// ── bindings ──

pub(super) async fn bindings(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    let mut items = Vec::new();
    for vh in vhosts_in_scope(m, req, vhost).await? {
        items.extend(vh.bindings().iter().map(|b| format::binding(&vh.name, b)));
    }
    list(req, items)
}

fn dest(kind: &str, name: &str) -> Dest {
    if kind == "e" { Dest::exchange(exchange_name(name)) } else { Dest::queue(name) }
}

/// Both ends must exist (404 otherwise).
fn ends(vh: &VHost, src: &str, d: &Dest) -> ApiResult<()> {
    vh.exchange(src).ok_or_else(ApiError::not_found)?;
    let found = match d.kind {
        DestKind::Queue => vh.queue(&d.name).is_some(),
        DestKind::Exchange => vh.exchange(&d.name).is_some(),
    };
    if found { Ok(()) } else { Err(ApiError::not_found()) }
}

fn between(vh: &VHost, src: &str, d: &Dest) -> Vec<Binding> {
    vh.bindings().into_iter().filter(|b| b.source == src && b.dest == *d).collect()
}

pub(super) async fn bindings_between(m: &Mgmt, req: &Req, vhost: &str, src: &str, kind: &str, dst: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let (src, d) = (exchange_name(src), dest(kind, dst));
    ends(&vh, src, &d)?;
    list(req, between(&vh, src, &d).iter().map(|b| format::binding(vhost, b)).collect())
}

pub(super) async fn create_binding(m: &Mgmt, req: &Req, vhost: &str, src: &str, kind: &str, dst: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let (src, d) = (exchange_name(src), dest(kind, dst));
    ends(&vh, src, &d)?;
    let body = req.json()?;
    let routing_key = body_str(&body, "routing_key").unwrap_or("").to_string();
    let args = body_args(&body)?;
    let dkind = if d.kind == DestKind::Queue { "queue" } else { "exchange" };
    check(m, req, vhost, Access::Write, dkind, &d.name)?;
    check(m, req, vhost, Access::Read, "exchange", src)?;
    let key = format::props_key(&routing_key, &args);
    vh.bind(src, d.clone(), &routing_key, args, None).await?;
    let mut r = StatusCode::CREATED.into_response();
    if let Ok(loc) = HeaderValue::from_str(&format::binding_location(vhost, src, kind, &d.name, &key)) {
        r.headers_mut().insert(header::LOCATION, loc);
    }
    Ok(r)
}

fn find_binding(vh: &VHost, src: &str, d: &Dest, key: &str) -> ApiResult<Binding> {
    between(vh, src, d).into_iter().find(|b| format::props_key(&b.routing_key, &b.args) == key).ok_or_else(ApiError::not_found)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn binding(m: &Mgmt, req: &Req, vhost: &str, src: &str, kind: &str, dst: &str, key: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let (src, d) = (exchange_name(src), dest(kind, dst));
    ends(&vh, src, &d)?;
    let b = find_binding(&vh, src, &d, key)?;
    Ok(ok(shape(req, format::binding(vhost, &b))))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn delete_binding(m: &Mgmt, req: &Req, vhost: &str, src: &str, kind: &str, dst: &str, key: &str) -> ApiResult {
    let vh = open_vhost(m, req, vhost).await?;
    let (src, d) = (exchange_name(src), dest(kind, dst));
    ends(&vh, src, &d)?;
    let b = find_binding(&vh, src, &d, key)?;
    let dkind = if d.kind == DestKind::Queue { "queue" } else { "exchange" };
    check(m, req, vhost, Access::Write, dkind, &d.name)?;
    check(m, req, vhost, Access::Read, "exchange", src)?;
    vh.unbind(src, d, &b.routing_key, b.args, None).await?;
    Ok(no_content())
}

/// Whether `req`'s user may see objects in `vhost` (used by listings of status providers).
pub(super) fn visible(m: &Mgmt, req: &Req, vhost: &str) -> bool {
    can_see_vhost(m, &req.user, vhost)
}

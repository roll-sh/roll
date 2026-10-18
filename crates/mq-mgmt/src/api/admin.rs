//! Overview, nodes, definitions, parameters, policies, health checks, feature flags and the
//! shovel / federation status listings.

use super::topology::visible;
use super::{body_i64, body_str, can_see_vhost, check_vhost, require_admin, require_monitor, require_policymaker, visible_vhosts};
use crate::Mgmt;
use crate::format;
use crate::http::{ApiError, ApiResult, Req, created_or_updated, list, no_content, ok, shape};
use axum::http::{HeaderValue, header};
use bytes::Bytes;
use mq_core::message::{Message, Properties};
use mq_core::policy::Policy;
use mq_core::queue::ConfirmTicket;
use mq_core::vhost::DeclareQueue;
use serde_json::{Map, Value, json};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

pub(super) fn overview(m: &Mgmt, req: &Req) -> ApiResult {
    let visible = visible_vhosts(m, &req.user);
    Ok(ok(shape(req, format::overview(m, &visible, &req.ages()))))
}

pub(super) fn cluster_name(m: &Mgmt, req: &Req) -> ApiResult {
    Ok(ok(shape(req, json!({"name": format::cluster_name(m)}))))
}

pub(super) async fn set_cluster_name(m: &Mgmt, req: &Req) -> ApiResult {
    require_admin(req)?;
    let body = req.json()?;
    let name = body_str(&body, "name").filter(|n| !n.is_empty()).ok_or_else(|| ApiError::bad_request("key_missing: name"))?;
    m.broker.put_global_parameter("cluster_name", json!(name)).await?;
    Ok(no_content())
}

pub(super) fn nodes(m: &Mgmt, req: &Req) -> ApiResult {
    require_monitor(req)?;
    list(req, vec![format::node(m, req.flag("memory"))])
}

pub(super) fn node(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_monitor(req)?;
    if name != m.node() {
        return Err(ApiError::not_found());
    }
    Ok(ok(shape(req, format::node(m, req.flag("memory")))))
}

pub(super) fn node_memory(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_monitor(req)?;
    if name != m.node() {
        return Err(ApiError::not_found());
    }
    Ok(ok(json!({"memory": format::memory_breakdown(crate::sys::rss_bytes())})))
}

// ── feature flags & deprecations (roll implements RabbitMQ 4.1 semantics throughout) ──

const FEATURE_FLAGS: &[(&str, &str, &str)] = &[
    ("classic_mirrored_queue_version", "Support setting version for classic mirrored queues", "required"),
    ("classic_queue_type_delivery_support", "Bug fix for classic queue deliveries using mixed versions", "required"),
    ("detailed_queues_endpoint", "Add a detailed queues HTTP API endpoint. Reduce number of metrics in the default endpoint.", "stable"),
    ("direct_exchange_routing_v2", "v2 direct exchange routing implementation", "required"),
    ("drop_unroutable_metric", "Count unroutable publishes to be dropped in stats", "required"),
    ("empty_basic_get_metric", "Count AMQP `basic.get` on empty queues in stats", "required"),
    ("feature_flags_v2", "Feature flags subsystem V2", "required"),
    ("implicit_default_bindings", "Default bindings are now implicit, instead of being stored in the database", "required"),
    ("khepri_db", "New Raft-based metadata store.", "stable"),
    ("listener_records_in_ets", "Store listener records in ETS instead of Mnesia", "required"),
    ("maintenance_mode_status", "Maintenance mode status", "required"),
    ("message_containers", "Message containers.", "required"),
    ("message_containers_deaths_v2", "Bug fix for dead letter cycle detection", "required"),
    ("quorum_queue", "Support queues of type `quorum`", "required"),
    ("quorum_queue_non_voters", "Allows new quorum queue members to be added as non voters initially.", "stable"),
    ("rabbit_exchange_type_local_random", "Local random exchange", "stable"),
    ("rabbitmq_4.0.0", "Allows rolling upgrades from 3.13.x to 4.0.x", "stable"),
    ("rabbitmq_4.1.0", "Allows rolling upgrades to 4.1.x", "stable"),
    ("restart_streams", "Support for restarting streams with optional preferred next leader argument.", "required"),
    ("stream_filtering", "Support for stream filtering.", "required"),
    ("stream_queue", "Support queues of type `stream`", "required"),
    ("stream_sac_coordinator_unblock_group", "Bug fix to unblock a group of consumers in a super stream partition", "stable"),
    ("stream_single_active_consumer", "Single active consumer for streams", "required"),
    ("stream_update_config_command", "A new internal command that is used to update streams as part of a policy.", "required"),
    ("tracking_records_in_ets", "Store tracking records in ETS instead of Mnesia", "required"),
    ("user_limits", "Configure connection and channel limits for a user", "required"),
    ("virtual_host_metadata", "Virtual host metadata (description, tags, etc)", "required"),
];

pub(super) fn feature_flags(req: &Req) -> ApiResult {
    require_admin(req)?;
    let items = FEATURE_FLAGS
        .iter()
        .map(|(n, d, s)| json!({"name": n, "desc": d, "doc_url": "", "stability": s, "provided_by": "rabbit", "state": "enabled"}))
        .collect();
    list(req, items)
}

pub(super) fn enable_feature_flag(req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    if FEATURE_FLAGS.iter().any(|(n, _, _)| *n == name) { Ok(no_content()) } else { Err(ApiError::not_found()) }
}

const DEPRECATED: &[(&str, &str, &str)] = &[
    ("amqp_address_v1", "AMQP 1.0 address format v1", "permitted_by_default"),
    ("amqp_filter_set_bug", "AMQP filter set bug", "permitted_by_default"),
    ("classic_queue_mirroring", "Classic mirrored queues", "removed"),
    ("global_qos", "QoS global parameter", "permitted_by_default"),
    ("queue_master_locator", "queue-master-locator queue argument", "permitted_by_default"),
    ("ram_node_type", "RAM node type", "permitted_by_default"),
    ("transient_nonexcl_queues", "Transient non-exclusive queues", "permitted_by_default"),
];

pub(super) fn deprecated_features(req: &Req, used_only: bool) -> ApiResult {
    require_admin(req)?;
    if used_only {
        return list(req, vec![]);
    }
    let items = DEPRECATED
        .iter()
        .map(|(n, d, p)| json!({"name": n, "desc": d, "deprecation_phase": p, "doc_url": "https://www.rabbitmq.com/docs/deprecated-features", "provided_by": "rabbit"}))
        .collect();
    list(req, items)
}

// ── health checks ──

fn healthy() -> ApiResult {
    Ok(ok(json!({"status": "ok"})))
}

/// Protocol names as RabbitMQ's health checks accept them.
fn normalise_protocol(p: &str) -> String {
    match p.trim().to_ascii_lowercase().as_str() {
        "amqp091" | "amqp0.9.1" | "amqp0-9-1" | "amqp10" | "amqp1.0" | "amqp" => "amqp".into(),
        "amqps" | "amqp/ssl" => "amqp/ssl".into(),
        "management" | "http" => "http".into(),
        "prometheus" | "http/prometheus" => "http/prometheus".into(),
        "stream" | "rabbitmq_stream" => "stream".into(),
        other => other.into(),
    }
}

pub(super) fn health(m: &Mgmt, rest: &[&str]) -> ApiResult {
    let alarm = m.broker.alarm.load(Relaxed);
    let alarms_failed = |scope: &str| {
        Err(ApiError::unavailable(json!({
            "status": "failed",
            "reason": format!("resource alarm(s) in effect {scope}"),
            "alarms": format::alarms(m),
        })))
    };
    match rest {
        ["alarms"] if alarm => alarms_failed("in the cluster"),
        ["local-alarms"] if alarm => alarms_failed("on the node"),
        ["alarms"] | ["local-alarms"] => healthy(),
        // roll terminates TLS with certificates it doesn't manage here; none are expiring.
        ["certificate-expiration", _within, _unit] => healthy(),
        ["port-listener", port] => {
            let port: u16 = port.parse().map_err(|_| ApiError::bad_request(format!("invalid port '{port}'")))?;
            let ports: Vec<u16> = m.broker.listeners.read().iter().map(|l| l.port).collect();
            if ports.contains(&port) {
                Ok(ok(json!({"status": "ok", "port": port})))
            } else {
                Err(ApiError::unavailable(json!({"status": "failed", "reason": "No active listener", "missing": port, "ports": ports})))
            }
        }
        ["protocol-listener", protocols] => {
            let active: Vec<String> = m.broker.listeners.read().iter().map(|l| l.protocol.clone()).collect();
            let missing: Vec<String> = protocols.split(',').map(normalise_protocol).filter(|p| !active.contains(p)).collect();
            if missing.is_empty() {
                Ok(ok(json!({"status": "ok", "protocols": protocols.split(',').collect::<Vec<_>>()})))
            } else {
                Err(ApiError::unavailable(json!({"status": "failed", "reason": "No active listener", "missing": missing, "protocols": active})))
            }
        }
        ["virtual-hosts"] => healthy(),
        ["node-is-quorum-critical"] | ["node-is-mirror-sync-critical"] => healthy(),
        ["ready-to-serve-clients"] | ["is-in-service"] => {
            if m.broker.draining.load(Relaxed) {
                Err(ApiError::unavailable(json!({"status": "failed", "reason": "node is being drained"})))
            } else {
                healthy()
            }
        }
        ["below-node-connection-limit"] => healthy(),
        ["metadata-store", "initialized"] | ["metadata-store", "initialized", "with-data"] => healthy(),
        ["quorum-queues-without-elected-leaders", ..] => healthy(),
        _ => Err(ApiError::not_found()),
    }
}

pub(super) async fn aliveness(m: &Mgmt, req: &Req, vhost: &str) -> ApiResult {
    let vh = super::open_vhost(m, req, vhost).await?;
    let failed = |reason: String| ApiError::unavailable(json!({"status": "failed", "reason": reason}));
    let q = "aliveness-test";
    vh.declare_queue(DeclareQueue { name: q.into(), durable: false, exclusive_owner: None, auto_delete: false, args: Default::default(), passive: false })
        .await
        .map_err(|e| failed(e.text))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let ticket = ConfirmTicket::new(move |ok| {
        let _ = tx.send(ok);
    });
    let msg = Message::new("", q, Properties::default(), Bytes::from_static(b"test_message"));
    let out = vh.publish("", msg, Some(ticket)).await.map_err(|e| failed(e.text))?;
    if let Some(t) = out.confirm {
        t.settle(true);
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
    let queue = vh.queue(q).ok_or_else(|| failed("aliveness-test queue disappeared".into()))?;
    match queue.get(m.broker.next_id(), true).await.map_err(|e| failed(e.text))? {
        Some(d) if &d.msg.body[..] == b"test_message" => Ok(ok(json!({"status": "ok"}))),
        _ => Err(failed("aliveness test message was not delivered".into())),
    }
}

// ── definitions ──

pub(super) async fn export(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    require_admin(req)?;
    if let Some(v) = vhost {
        check_vhost(m, req, v)?;
    }
    let mut defs = m.broker.export_definitions(vhost).await?;
    if let Some(v) = vhost {
        defs = vhost_definitions(m, v, defs);
    }
    let mut r = ok(defs);
    if let Some(name) = req.q("download").filter(|n| !n.is_empty()) {
        let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename={safe}")) {
            r.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    Ok(r)
}

/// RabbitMQ's single-vhost export: objects without `vhost` keys plus the vhost's metadata.
fn vhost_definitions(m: &Mgmt, vhost: &str, mut defs: Value) -> Value {
    for k in ["queues", "exchanges", "bindings", "policies", "parameters"] {
        if let Some(Value::Array(items)) = defs.get_mut(k) {
            for i in items.iter_mut() {
                if let Value::Object(o) = i {
                    o.remove("vhost");
                }
            }
        }
    }
    if let Some((info, _)) = m.broker.vhost_infos().into_iter().find(|(i, _)| i.name == vhost)
        && let Value::Object(o) = &mut defs
    {
        let dqt = info.default_queue_type.clone().unwrap_or_else(|| m.broker.config.default_queue_type.clone());
        o.insert("rabbitmq_definition_format".into(), json!("single_virtual_host"));
        o.insert("original_vhost_name".into(), json!(vhost));
        o.insert("explanation".into(), json!(format!("Definitions of virtual host '{vhost}'")));
        o.insert("metadata".into(), json!({"description": info.description, "tags": info.tags, "default_queue_type": dqt}));
        o.insert("description".into(), json!(info.description));
        o.insert("default_queue_type".into(), json!(dqt));
        o.insert("limits".into(), json!(info.limits));
    }
    defs
}

pub(super) async fn import(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    require_admin(req)?;
    let body = req.json()?;
    // RabbitMQ answers 204 with no body, so what was skipped goes to the log.
    let report = m.broker.import_definitions(&body, vhost).await?;
    for s in &report.skipped {
        tracing::warn!(kind = %s.kind, vhost = s.vhost.as_deref().unwrap_or(""), name = %s.name, partial = s.partial, reason = %s.reason, "definitions import: skipped");
    }
    Ok(no_content())
}

// ── parameters ──

fn parameter_json(c: &str, v: &str, n: &str, value: &Value) -> Value {
    json!({"component": c, "vhost": v, "name": n, "value": value})
}

pub(super) fn parameters(m: &Mgmt, req: &Req, component: Option<&str>, vhost: Option<&str>) -> ApiResult {
    require_policymaker(req)?;
    if let Some(v) = vhost {
        check_vhost(m, req, v)?;
    }
    let items = m
        .broker
        .parameters(component, vhost)
        .iter()
        .filter(|(_, v, _, _)| can_see_vhost(m, &req.user, v))
        .map(|(c, v, n, val)| parameter_json(c, v, n, val))
        .collect();
    list(req, items)
}

pub(super) fn parameter(m: &Mgmt, req: &Req, component: &str, vhost: &str, name: &str) -> ApiResult {
    require_policymaker(req)?;
    check_vhost(m, req, vhost)?;
    let v = m.broker.parameter(component, vhost, name).ok_or_else(ApiError::not_found)?;
    Ok(ok(shape(req, parameter_json(component, vhost, name, &v))))
}

pub(super) async fn put_parameter(m: &Mgmt, req: &Req, component: &str, vhost: &str, name: &str) -> ApiResult {
    require_policymaker(req)?;
    check_vhost(m, req, vhost)?;
    let body = req.json()?;
    let value = body.get("value").cloned().filter(|v| !v.is_null()).ok_or_else(|| ApiError::bad_request("key_missing: value"))?;
    let created = m.broker.parameter(component, vhost, name).is_none();
    m.broker.put_parameter(component, vhost, name, value).await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_parameter(m: &Mgmt, req: &Req, component: &str, vhost: &str, name: &str) -> ApiResult {
    require_policymaker(req)?;
    check_vhost(m, req, vhost)?;
    if m.broker.delete_parameter(component, vhost, name).await? { Ok(no_content()) } else { Err(ApiError::not_found()) }
}

pub(super) fn global_parameters(m: &Mgmt, req: &Req) -> ApiResult {
    require_admin(req)?;
    list(req, m.broker.global_parameters().iter().map(|(n, v)| json!({"name": n, "value": v})).collect())
}

pub(super) fn global_parameter(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    let (n, v) = m.broker.global_parameters().into_iter().find(|(n, _)| n == name).ok_or_else(ApiError::not_found)?;
    Ok(ok(shape(req, json!({"name": n, "value": v}))))
}

pub(super) async fn put_global_parameter(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    let body = req.json()?;
    let value = body.get("value").cloned().ok_or_else(|| ApiError::bad_request("key_missing: value"))?;
    let created = !m.broker.global_parameters().iter().any(|(n, _)| n == name);
    m.broker.put_global_parameter(name, value).await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_global_parameter(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    if m.broker.delete_global_parameter(name).await? { Ok(no_content()) } else { Err(ApiError::not_found()) }
}

// ── policies ──

fn policy_list(m: &Mgmt, operator: bool) -> Vec<Policy> {
    let l = if operator { m.broker.operator_policies.read() } else { m.broker.policies.read() };
    let mut v = l.clone();
    v.sort_by(|a, b| (&a.vhost, &a.name).cmp(&(&b.vhost, &b.name)));
    v
}

fn require_policy_access(req: &Req, operator: bool) -> ApiResult<()> {
    if operator { require_admin(req) } else { require_policymaker(req) }
}

pub(super) fn policies(m: &Mgmt, req: &Req, vhost: Option<&str>, operator: bool) -> ApiResult {
    require_policy_access(req, operator)?;
    if let Some(v) = vhost {
        check_vhost(m, req, v)?;
    }
    let items = policy_list(m, operator)
        .iter()
        .filter(|p| vhost.is_none_or(|v| p.vhost == v) && can_see_vhost(m, &req.user, &p.vhost))
        .map(format::policy)
        .collect();
    list(req, items)
}

pub(super) fn policy(m: &Mgmt, req: &Req, vhost: &str, name: &str, operator: bool) -> ApiResult {
    require_policy_access(req, operator)?;
    check_vhost(m, req, vhost)?;
    let p = policy_list(m, operator).into_iter().find(|p| p.vhost == vhost && p.name == name).ok_or_else(ApiError::not_found)?;
    Ok(ok(shape(req, format::policy(&p))))
}

const APPLY_TO: &[&str] = &["all", "exchanges", "queues", "classic_queues", "quorum_queues", "streams"];

pub(super) async fn put_policy(m: &Mgmt, req: &Req, vhost: &str, name: &str, operator: bool) -> ApiResult {
    require_policy_access(req, operator)?;
    check_vhost(m, req, vhost)?;
    let body = req.json()?;
    let pattern = body_str(&body, "pattern").ok_or_else(|| ApiError::bad_request("key_missing: pattern"))?.to_string();
    let definition = match body.get("definition") {
        Some(Value::Object(o)) => Value::Object(o.clone()),
        Some(Value::String(s)) => serde_json::from_str::<Map<String, Value>>(s).map(Value::Object).map_err(|_| ApiError::bad_request("definition must be an object"))?,
        _ => return Err(ApiError::bad_request("key_missing: definition")),
    };
    let apply_to = body_str(&body, "apply-to").or_else(|| body_str(&body, "apply_to")).unwrap_or(if operator { "queues" } else { "all" }).to_string();
    if !APPLY_TO.contains(&apply_to.as_str()) || (operator && matches!(apply_to.as_str(), "all" | "exchanges")) {
        return Err(ApiError::bad_request(format!("Validation failed\n\n'{apply_to}' is not a valid apply-to value\n")));
    }
    let priority = body_i64(&body, "priority")?.unwrap_or(0);
    let created = !policy_list(m, operator).iter().any(|p| p.vhost == vhost && p.name == name);
    let p = Policy { vhost: vhost.to_string(), name: name.to_string(), pattern, apply_to, definition, priority };
    m.broker.put_policy(p, operator).await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_policy(m: &Mgmt, req: &Req, vhost: &str, name: &str, operator: bool) -> ApiResult {
    require_policy_access(req, operator)?;
    check_vhost(m, req, vhost)?;
    if m.broker.delete_policy(vhost, name, operator).await? { Ok(no_content()) } else { Err(ApiError::not_found()) }
}

// ── shovels & federation links ──

fn item_vhost(v: &Value) -> &str {
    v.get("vhost").and_then(|x| x.as_str()).unwrap_or("/")
}

pub(super) fn status_list(m: &Mgmt, req: &Req, items: Vec<Value>, vhost: Option<&str>) -> ApiResult {
    if let Some(v) = vhost {
        check_vhost(m, req, v)?;
    }
    let items = items.into_iter().filter(|i| vhost.is_none_or(|v| item_vhost(i) == v) && visible(m, req, item_vhost(i))).collect();
    list(req, items)
}

/// `DELETE /api/shovels/vhost/{vhost}/{name}/restart`: like RabbitMQ, 204 once restarted.
pub(super) fn restart_shovel(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    require_policymaker(req)?;
    check_vhost(m, req, vhost)?;
    if m.restart_shovel(vhost, name) { Ok(no_content()) } else { Err(ApiError::not_found()) }
}

pub(super) fn status_item(m: &Mgmt, req: &Req, items: Vec<Value>, vhost: &str, name: &str) -> ApiResult {
    check_vhost(m, req, vhost)?;
    let item = items
        .into_iter()
        .find(|i| item_vhost(i) == vhost && i.get("name").and_then(|n| n.as_str()) == Some(name))
        .ok_or_else(ApiError::not_found)?;
    Ok(ok(shape(req, item)))
}

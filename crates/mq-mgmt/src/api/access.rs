//! Virtual hosts, users, permissions, topic permissions and limits.

use super::{body_i64, body_str, body_tags, check_vhost, require_admin, visible_vhosts};
use crate::Mgmt;
use crate::format;
use crate::http::{ApiError, ApiResult, Req, created_or_updated, list, no_content, ok, shape};
use mq_core::auth::{Permission, TopicPermission, User, hash_password};
use mq_core::queue::QueueType;
use mq_core::vhost::VHostInfo;
use serde_json::{Value, json};
use std::collections::HashMap;

// ── vhosts ──

pub(super) fn vhosts(m: &Mgmt, req: &Req) -> ApiResult {
    let visible = visible_vhosts(m, &req.user);
    let ages = req.ages();
    let items = m
        .broker
        .vhost_infos()
        .iter()
        .filter(|(i, _)| visible.contains(&i.name))
        .map(|(i, vh)| format::vhost(m, i, vh.as_ref(), &ages))
        .collect();
    list(req, items)
}

pub(super) fn vhost(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    check_vhost(m, req, name)?;
    let (info, vh) = m.broker.vhost_infos().into_iter().find(|(i, _)| i.name == name).ok_or_else(ApiError::not_found)?;
    Ok(ok(shape(req, format::vhost(m, &info, vh.as_ref(), &req.ages()))))
}

pub(super) async fn put_vhost(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    let body = req.json()?;
    let existing = m.broker.vhost_infos().into_iter().find(|(i, _)| i.name == name).map(|(i, _)| i);
    let created = existing.is_none();
    let mut info = existing.unwrap_or_else(|| VHostInfo { name: name.to_string(), ..Default::default() });
    let md = body.get("metadata").cloned().unwrap_or(Value::Null);
    if let Some(d) = body_str(&body, "description").or_else(|| body_str(&md, "description")) {
        info.description = d.to_string();
    }
    if let Some(t) = body.get("tags").or_else(|| md.get("tags")) {
        info.tags = body_tags(Some(t));
    }
    if let Some(q) = body_str(&body, "default_queue_type").or_else(|| body_str(&md, "default_queue_type")).filter(|q| !q.is_empty()) {
        if q != "undefined" && QueueType::parse(q).is_none() {
            return Err(ApiError::bad_request(format!("Unsupported default queue type: {q}")));
        }
        info.default_queue_type = (q != "undefined").then(|| q.to_string());
    }
    if let Some(t) = body.get("tracing").and_then(|t| t.as_bool()) {
        info.tracing = t;
    }
    m.broker.create_vhost(info).await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_vhost(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    if !m.broker.vhost_exists(name) {
        return Err(ApiError::not_found());
    }
    m.broker.delete_vhost(name).await?;
    Ok(no_content())
}

pub(super) fn vhost_permissions(m: &Mgmt, req: &Req, vhost: &str) -> ApiResult {
    require_admin(req)?;
    check_vhost(m, req, vhost)?;
    let items = m.broker.permissions().iter().filter(|(v, _, _)| v == vhost).map(|(v, u, p)| format::permission(v, u, p)).collect();
    list(req, items)
}

pub(super) fn vhost_topic_permissions(m: &Mgmt, req: &Req, vhost: &str) -> ApiResult {
    require_admin(req)?;
    check_vhost(m, req, vhost)?;
    let items = m.broker.topic_permissions().iter().filter(|(v, _, _)| v == vhost).map(|(v, u, p)| format::topic_permission(v, u, p)).collect();
    list(req, items)
}

// ── users ──

pub(super) fn users(m: &Mgmt, req: &Req) -> ApiResult {
    require_admin(req)?;
    list(req, m.broker.users().iter().map(format::user).collect())
}

pub(super) fn users_without_permissions(m: &Mgmt, req: &Req) -> ApiResult {
    require_admin(req)?;
    let perms = m.broker.permissions();
    let items = m.broker.users().iter().filter(|u| !perms.iter().any(|(_, pu, _)| *pu == u.name)).map(format::user).collect();
    list(req, items)
}

pub(super) async fn bulk_delete_users(m: &Mgmt, req: &Req) -> ApiResult {
    require_admin(req)?;
    let body = req.json()?;
    let names = body.get("users").and_then(|u| u.as_array()).ok_or_else(|| ApiError::bad_request("key_missing: users"))?;
    for n in names.iter().filter_map(|n| n.as_str()) {
        m.broker.delete_user(n).await?;
    }
    Ok(no_content())
}

pub(super) fn user(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    let u = m.broker.user(name).ok_or_else(ApiError::not_found)?;
    Ok(ok(shape(req, format::user(&u))))
}

pub(super) async fn put_user(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    let body = req.json()?;
    let existing = m.broker.user(name);
    let created = existing.is_none();
    let tags = match body.get("tags") {
        Some(t) => body_tags(Some(t)),
        None => existing.as_ref().map(|u| u.tags.clone()).unwrap_or_default(),
    };
    let (hash, algo) = match (body_str(&body, "password"), body.get("password_hash")) {
        (Some(p), _) => (hash_password(p), "rabbit_password_hashing_sha256".to_string()),
        (None, Some(h)) => {
            let h = h.as_str().ok_or_else(|| ApiError::bad_request("password_hash must be a string"))?;
            let algo = body_str(&body, "hashing_algorithm").unwrap_or("rabbit_password_hashing_sha256");
            if !matches!(algo, "rabbit_password_hashing_sha256" | "rabbit_password_hashing_sha512") {
                return Err(ApiError::bad_request(format!("Unsupported hashing algorithm: {algo}")));
            }
            (h.to_string(), algo.to_string())
        }
        (None, None) => match &existing {
            Some(u) => (u.password_hash.clone(), u.hashing_algorithm.clone()),
            None => return Err(ApiError::bad_request("Must specify password or password_hash")),
        },
    };
    let limits = existing.map(|u| u.limits).unwrap_or_default();
    m.broker.put_user(User { name: name.to_string(), password_hash: hash, hashing_algorithm: algo, tags, limits }).await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_user(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    if m.broker.delete_user(name).await? { Ok(no_content()) } else { Err(ApiError::not_found()) }
}

pub(super) fn user_permissions(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    m.broker.user(name).ok_or_else(ApiError::not_found)?;
    let items = m.broker.permissions().iter().filter(|(_, u, _)| u == name).map(|(v, u, p)| format::permission(v, u, p)).collect();
    list(req, items)
}

pub(super) fn user_topic_permissions(m: &Mgmt, req: &Req, name: &str) -> ApiResult {
    require_admin(req)?;
    m.broker.user(name).ok_or_else(ApiError::not_found)?;
    let items = m.broker.topic_permissions().iter().filter(|(_, u, _)| u == name).map(|(v, u, p)| format::topic_permission(v, u, p)).collect();
    list(req, items)
}

// ── permissions ──

pub(super) fn permissions(m: &Mgmt, req: &Req) -> ApiResult {
    require_admin(req)?;
    list(req, m.broker.permissions().iter().map(|(v, u, p)| format::permission(v, u, p)).collect())
}

pub(super) fn permission(m: &Mgmt, req: &Req, vhost: &str, user: &str) -> ApiResult {
    require_admin(req)?;
    let p = m.broker.permission(vhost, user).ok_or_else(ApiError::not_found)?;
    Ok(ok(shape(req, format::permission(vhost, user, &p))))
}

fn user_and_vhost(m: &Mgmt, vhost: &str, user: &str) -> ApiResult<()> {
    if !m.broker.vhost_exists(vhost) {
        return Err(ApiError::bad_request("vhost_not_found"));
    }
    if m.broker.user(user).is_none() {
        return Err(ApiError::bad_request("user_not_found"));
    }
    Ok(())
}

fn pattern(body: &Value, key: &str) -> ApiResult<String> {
    let p = body_str(body, key).ok_or_else(|| ApiError::bad_request(format!("key_missing: {key}")))?;
    regex::Regex::new(p).map_err(|e| ApiError::bad_request(format!("invalid regular expression '{p}': {e}")))?;
    Ok(p.to_string())
}

pub(super) async fn put_permission(m: &Mgmt, req: &Req, vhost: &str, user: &str) -> ApiResult {
    require_admin(req)?;
    user_and_vhost(m, vhost, user)?;
    let body = req.json()?;
    let p = Permission { configure: pattern(&body, "configure")?, write: pattern(&body, "write")?, read: pattern(&body, "read")? };
    let created = m.broker.permission(vhost, user).is_none();
    m.broker.set_permission(vhost, user, p).await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_permission(m: &Mgmt, req: &Req, vhost: &str, user: &str) -> ApiResult {
    require_admin(req)?;
    if m.broker.permission(vhost, user).is_none() {
        return Err(ApiError::not_found());
    }
    m.broker.clear_permission(vhost, user).await?;
    Ok(no_content())
}

pub(super) fn topic_permissions(m: &Mgmt, req: &Req) -> ApiResult {
    require_admin(req)?;
    list(req, m.broker.topic_permissions().iter().map(|(v, u, p)| format::topic_permission(v, u, p)).collect())
}

pub(super) fn topic_permission(m: &Mgmt, req: &Req, vhost: &str, user: &str) -> ApiResult {
    require_admin(req)?;
    let items: Vec<Value> =
        m.broker.topic_permissions().iter().filter(|(v, u, _)| v == vhost && u == user).map(|(v, u, p)| format::topic_permission(v, u, p)).collect();
    if items.is_empty() {
        return Err(ApiError::not_found());
    }
    list(req, items)
}

pub(super) async fn put_topic_permission(m: &Mgmt, req: &Req, vhost: &str, user: &str) -> ApiResult {
    require_admin(req)?;
    user_and_vhost(m, vhost, user)?;
    let body = req.json()?;
    let exchange = body_str(&body, "exchange").ok_or_else(|| ApiError::bad_request("key_missing: exchange"))?.to_string();
    let p = TopicPermission { exchange: exchange.clone(), write: pattern(&body, "write")?, read: pattern(&body, "read")? };
    let created = !m.broker.topic_permissions().iter().any(|(v, u, tp)| v == vhost && u == user && tp.exchange == exchange);
    m.broker.set_topic_permission(vhost, user, p).await?;
    Ok(created_or_updated(created))
}

pub(super) async fn delete_topic_permission(m: &Mgmt, req: &Req, vhost: &str, user: &str) -> ApiResult {
    require_admin(req)?;
    if !m.broker.topic_permissions().iter().any(|(v, u, _)| v == vhost && u == user) {
        return Err(ApiError::not_found());
    }
    let body = req.json()?;
    m.broker.clear_topic_permission(vhost, user, body_str(&body, "exchange")).await?;
    Ok(no_content())
}

pub(super) async fn delete_topic_permission_for(m: &Mgmt, req: &Req, vhost: &str, user: &str, exchange: &str) -> ApiResult {
    require_admin(req)?;
    if !m.broker.topic_permissions().iter().any(|(v, u, p)| v == vhost && u == user && p.exchange == exchange) {
        return Err(ApiError::not_found());
    }
    m.broker.clear_topic_permission(vhost, user, Some(exchange)).await?;
    Ok(no_content())
}

// ── limits ──

fn limit_value(req: &Req) -> ApiResult<i64> {
    let body = req.json()?;
    body_i64(&body, "value")?.ok_or_else(|| ApiError::bad_request("key_missing: value"))
}

pub(super) fn vhost_limits(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult {
    if let Some(v) = vhost {
        check_vhost(m, req, v)?;
    }
    let visible = visible_vhosts(m, &req.user);
    let items = m
        .broker
        .vhost_infos()
        .iter()
        .filter(|(i, _)| vhost.is_none_or(|v| i.name == v) && visible.contains(&i.name) && !i.limits.is_empty())
        .map(|(i, _)| json!({"vhost": i.name, "value": i.limits}))
        .collect();
    list(req, items)
}

const VHOST_LIMITS: &[&str] = &["max-connections", "max-queues", "max-publish-rate", "max-publish-bytes-rate"];
const USER_LIMITS: &[&str] = &["max-connections", "max-channels"];

async fn update_vhost_limits(m: &Mgmt, vhost: &str, f: impl FnOnce(&mut HashMap<String, i64>)) -> ApiResult<()> {
    let mut limits = m.broker.vhost_infos().into_iter().find(|(i, _)| i.name == vhost).map(|(i, _)| i.limits).ok_or_else(ApiError::not_found)?;
    f(&mut limits);
    m.broker.set_vhost_limits(vhost, limits).await?;
    Ok(())
}

pub(super) async fn put_vhost_limit(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    require_admin(req)?;
    check_vhost(m, req, vhost)?;
    if !VHOST_LIMITS.contains(&name) {
        return Err(ApiError::bad_request(format!("Validation failed\n\nUnrecognised terms [{{<<\"{name}\">>,..}}] in limits\n")));
    }
    let value = limit_value(req)?;
    update_vhost_limits(m, vhost, |l| {
        l.insert(name.to_string(), value);
    })
    .await?;
    Ok(no_content())
}

pub(super) async fn delete_vhost_limit(m: &Mgmt, req: &Req, vhost: &str, name: &str) -> ApiResult {
    require_admin(req)?;
    check_vhost(m, req, vhost)?;
    update_vhost_limits(m, vhost, |l| {
        l.remove(name);
    })
    .await?;
    Ok(no_content())
}

pub(super) fn user_limits(m: &Mgmt, req: &Req, user: Option<&str>) -> ApiResult {
    require_admin(req)?;
    if let Some(u) = user {
        m.broker.user(u).ok_or_else(ApiError::not_found)?;
    }
    let items = m
        .broker
        .users()
        .iter()
        .filter(|u| user.is_none_or(|n| u.name == n) && !u.limits.is_empty())
        .map(|u| json!({"user": u.name, "value": u.limits}))
        .collect();
    list(req, items)
}

pub(super) async fn put_user_limit(m: &Mgmt, req: &Req, user: &str, name: &str) -> ApiResult {
    require_admin(req)?;
    let mut u = m.broker.user(user).ok_or_else(ApiError::not_found)?;
    if !USER_LIMITS.contains(&name) {
        return Err(ApiError::bad_request(format!("Validation failed\n\nUnrecognised terms [{{<<\"{name}\">>,..}}] in user limits\n")));
    }
    u.limits.insert(name.to_string(), limit_value(req)?);
    m.broker.put_user(u).await?;
    Ok(no_content())
}

pub(super) async fn delete_user_limit(m: &Mgmt, req: &Req, user: &str, name: &str) -> ApiResult {
    require_admin(req)?;
    let mut u = m.broker.user(user).ok_or_else(ApiError::not_found)?;
    u.limits.remove(name);
    m.broker.put_user(u).await?;
    Ok(no_content())
}

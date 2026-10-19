//! `/api/...`: authentication, access rules and routing to the endpoint handlers.
//!
//! Paths are matched on percent-decoded segments (RabbitMQ encodes the default vhost as
//! `%2F`, and empty segments such as the default exchange are meaningful), so routing is done
//! here rather than by axum's path matcher.

mod access;
mod admin;
mod topology;

use crate::Mgmt;
use crate::http::{ApiError, ApiResult, M, Req, basic_credentials, parse_query, segments};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use mq_core::auth::User;
use mq_core::vhost::VHost;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

pub(crate) async fn handle(State(m): State<Arc<Mgmt>>, req: Request) -> Response {
    m.touch();
    let (parts, body) = req.into_parts();
    let method = match parts.method {
        Method::GET | Method::HEAD => M::Get,
        Method::PUT => M::Put,
        Method::POST => M::Post,
        Method::DELETE => M::Delete,
        _ => M::Other,
    };
    let segs = segments(parts.uri.path());
    if method == M::Get && segs == ["auth"] {
        return crate::http::ok(json!({"oauth_enabled": false, "oauth_client_id": "", "oauth_provider_url": ""}));
    }
    let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let user = match authenticate(&m, &parts.headers, peer) {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let body = match axum::body::to_bytes(body, m.config.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "bad_request", "request body is too large").into_response(),
    };
    let req = Req { method, segs, query: parse_query(parts.uri.query()), headers: parts.headers, body, user };
    match dispatch(&m, &req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

fn authenticate(m: &Mgmt, headers: &axum::http::HeaderMap, peer: Option<SocketAddr>) -> ApiResult<User> {
    let (u, p) = basic_credentials(headers).ok_or_else(|| ApiError::unauthorized("Not_Authorized"))?;
    let user = m.broker.authenticate(&u, &p, peer).map_err(|_| ApiError::unauthorized("Login failed"))?;
    if !user.can_manage() {
        return Err(ApiError::unauthorized("Not management user"));
    }
    Ok(user)
}

// ── access rules (RabbitMQ's user tags) ──

pub(crate) fn is_monitor(u: &User) -> bool {
    u.is_admin() || u.has_tag("monitoring")
}

pub(crate) fn require_admin(req: &Req) -> ApiResult<()> {
    if req.user.is_admin() { Ok(()) } else { Err(ApiError::unauthorized("Not administrator user")) }
}

pub(crate) fn require_monitor(req: &Req) -> ApiResult<()> {
    if is_monitor(&req.user) { Ok(()) } else { Err(ApiError::unauthorized("Not monitor user")) }
}

pub(crate) fn require_policymaker(req: &Req) -> ApiResult<()> {
    if req.user.is_admin() || req.user.has_tag("policymaker") { Ok(()) } else { Err(ApiError::unauthorized("Not policymaker user")) }
}

/// Vhosts the user may see: all of them for administrators and monitoring users, otherwise
/// those the user has permissions in.
pub(crate) fn visible_vhosts(m: &Mgmt, u: &User) -> Vec<String> {
    let all = m.broker.vhost_infos().into_iter().map(|(i, _)| i.name);
    if is_monitor(u) { all.collect() } else { all.filter(|v| m.broker.permission(v, &u.name).is_some()).collect() }
}

pub(crate) fn can_see_vhost(m: &Mgmt, u: &User, vhost: &str) -> bool {
    m.broker.vhost_exists(vhost) && (is_monitor(u) || m.broker.permission(vhost, &u.name).is_some())
}

/// 404 for a missing vhost, 401 for one the user may not access.
pub(crate) fn check_vhost(m: &Mgmt, req: &Req, vhost: &str) -> ApiResult<()> {
    if !m.broker.vhost_exists(vhost) {
        return Err(ApiError::not_found());
    }
    if !can_see_vhost(m, &req.user, vhost) {
        return Err(ApiError::vhost_denied());
    }
    Ok(())
}

/// A vhost the user may access, woken if it was scaled to zero.
pub(crate) async fn open_vhost(m: &Mgmt, req: &Req, vhost: &str) -> ApiResult<Arc<VHost>> {
    check_vhost(m, req, vhost)?;
    m.broker.open_vhost(vhost).await.ok_or_else(ApiError::not_found)
}

/// The loaded vhosts a listing covers: one (woken if needed) or every visible one.
pub(crate) async fn vhosts_in_scope(m: &Mgmt, req: &Req, vhost: Option<&str>) -> ApiResult<Vec<Arc<VHost>>> {
    match vhost {
        Some(v) => Ok(vec![open_vhost(m, req, v).await?]),
        None => {
            let visible = visible_vhosts(m, &req.user);
            Ok(m.broker.vhosts().into_iter().filter(|v| visible.contains(&v.name)).collect())
        }
    }
}

// ── body helpers (rabbitmqadmin v1 sends booleans and numbers as strings) ──

pub(crate) fn body_bool(v: &Value, key: &str, default: bool) -> ApiResult<bool> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s == "true" => Ok(true),
        Some(Value::String(s)) if s == "false" => Ok(false),
        Some(other) => Err(ApiError::bad_request(format!("{key} must be a boolean, got {other}"))),
    }
}

pub(crate) fn body_i64(v: &Value, key: &str) -> ApiResult<Option<i64>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_i64().map(Some).ok_or_else(|| ApiError::bad_request(format!("{key} must be an integer"))),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| ApiError::bad_request(format!("{key} must be an integer"))),
        Some(_) => Err(ApiError::bad_request(format!("{key} must be an integer"))),
    }
}

pub(crate) fn body_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// `x-...` arguments (an object, or a JSON-encoded string from older tools).
pub(crate) fn body_args(v: &Value) -> ApiResult<mq_core::FieldTable> {
    match v.get("arguments") {
        None | Some(Value::Null) => Ok(mq_core::FieldTable::new()),
        Some(Value::Object(o)) => Ok(mq_core::FieldTable::from_json_map(o)),
        Some(Value::Array(a)) if a.is_empty() => Ok(mq_core::FieldTable::new()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(mq_core::FieldTable::new()),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(o)) => Ok(mq_core::FieldTable::from_json_map(&o)),
            _ => Err(ApiError::bad_request("arguments must be an object")),
        },
        Some(_) => Err(ApiError::bad_request("arguments must be an object")),
    }
}

/// Tags as a list or a comma-separated string.
pub(crate) fn body_tags(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|t| t.as_str()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
        Some(Value::String(s)) => s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
        _ => vec![],
    }
}

async fn dispatch(m: &Arc<Mgmt>, req: &Req) -> ApiResult {
    use M::{Delete, Get, Post, Put};
    let s: Vec<&str> = req.segs.iter().map(String::as_str).collect();
    match (req.method, s.as_slice()) {
        // cluster & node
        (Get, ["overview"]) => admin::overview(m, req),
        (Get, ["cluster-name"]) => admin::cluster_name(m, req),
        (Put, ["cluster-name"]) => admin::set_cluster_name(m, req).await,
        (Get, ["nodes"]) => admin::nodes(m, req),
        (Get, ["nodes", n]) => admin::node(m, req, n),
        (Get, ["nodes", n, "memory"]) => admin::node_memory(m, req, n),
        (Get, ["extensions"]) => Ok(crate::http::ok(json!([]))),
        (Get, ["whoami"]) => Ok(crate::http::ok(json!({"name": req.user.name, "tags": req.user.tags, "is_internal_user": true}))),
        (Get, ["auth", "attempts", _]) | (Get, ["auth", "attempts", _, "source"]) => {
            require_monitor(req)?;
            Ok(crate::http::ok(json!([])))
        }
        (Delete, ["auth", "attempts", ..]) => {
            require_admin(req)?;
            Ok(crate::http::no_content())
        }
        (Delete, ["reset"]) | (Delete, ["reset", _]) => {
            require_admin(req)?;
            Ok(crate::http::no_content())
        }
        (Get, ["feature-flags"]) => admin::feature_flags(req),
        (Put, ["feature-flags", name, "enable"]) => admin::enable_feature_flag(req, name),
        (Get, ["deprecated-features"]) => admin::deprecated_features(req, false),
        (Get, ["deprecated-features", "used"]) => admin::deprecated_features(req, true),
        (Post, ["rebalance", "queues"]) => {
            require_admin(req)?;
            Ok(crate::http::no_content())
        }
        (Get, ["health", "checks", rest @ ..]) => admin::health(m, rest),
        (Get, ["aliveness-test", vh]) => admin::aliveness(m, req, vh).await,

        // definitions
        (Get, ["definitions"]) => admin::export(m, req, None).await,
        (Post, ["definitions"]) => admin::import(m, req, None).await,
        (Get, ["definitions", vh]) => admin::export(m, req, Some(vh)).await,
        (Post, ["definitions", vh]) => admin::import(m, req, Some(vh)).await,

        // connections, channels, consumers
        (Get, ["connections"]) => topology::connections(m, req, None),
        (Get, ["connections", "username", u]) => topology::user_connections(m, req, u),
        (Delete, ["connections", "username", u]) => topology::close_user_connections(m, req, u),
        (Get, ["connections", name]) => topology::connection(m, req, name),
        (Delete, ["connections", name]) => topology::close_connection(m, req, name),
        (Get, ["connections", name, "channels"]) => topology::connection_channels(m, req, name),
        (Get, ["channels"]) => topology::channels(m, req, None),
        (Get, ["channels", name]) => topology::channel(m, req, name),
        (Get, ["consumers"]) => topology::consumers(m, req, None).await,
        (Get, ["consumers", vh]) => topology::consumers(m, req, Some(vh)).await,

        // exchanges
        (Get, ["exchanges"]) => topology::exchanges(m, req, None).await,
        (Get, ["exchanges", vh]) => topology::exchanges(m, req, Some(vh)).await,
        (Get, ["exchanges", vh, x]) => topology::exchange(m, req, vh, x).await,
        (Put, ["exchanges", vh, x]) => topology::put_exchange(m, req, vh, x).await,
        (Delete, ["exchanges", vh, x]) => topology::delete_exchange(m, req, vh, x).await,
        (Get, ["exchanges", vh, x, "bindings", "source"]) => topology::exchange_bindings(m, req, vh, x, true).await,
        (Get, ["exchanges", vh, x, "bindings", "destination"]) => topology::exchange_bindings(m, req, vh, x, false).await,
        (Post, ["exchanges", vh, x, "publish"]) => topology::publish(m, req, vh, x).await,

        // queues
        (Get, ["queues"]) => topology::queues(m, req, None).await,
        (Get, ["queues", "detailed"]) => topology::queues(m, req, None).await,
        (Get, ["queues", "detailed", vh]) => topology::queues(m, req, Some(vh)).await,
        (Get, ["queues", vh]) => topology::queues(m, req, Some(vh)).await,
        (Get, ["queues", vh, q]) => topology::queue(m, req, vh, q).await,
        (Put, ["queues", vh, q]) => topology::put_queue(m, req, vh, q).await,
        (Delete, ["queues", vh, q]) => topology::delete_queue(m, req, vh, q).await,
        (Get, ["queues", vh, q, "bindings"]) => topology::queue_bindings(m, req, vh, q).await,
        (Delete, ["queues", vh, q, "contents"]) => topology::purge(m, req, vh, q).await,
        (Post, ["queues", vh, q, "get"]) => topology::get_messages(m, req, vh, q).await,
        (Post, ["queues", vh, q, "actions"]) => topology::queue_action(m, req, vh, q).await,

        // bindings
        (Get, ["bindings"]) => topology::bindings(m, req, None).await,
        (Get, ["bindings", vh]) => topology::bindings(m, req, Some(vh)).await,
        (Get, ["bindings", vh, "e", src, kind @ ("q" | "e"), dst]) => topology::bindings_between(m, req, vh, src, kind, dst).await,
        (Post, ["bindings", vh, "e", src, kind @ ("q" | "e"), dst]) => topology::create_binding(m, req, vh, src, kind, dst).await,
        (Get, ["bindings", vh, "e", src, kind @ ("q" | "e"), dst, key]) => topology::binding(m, req, vh, src, kind, dst, key).await,
        (Delete, ["bindings", vh, "e", src, kind @ ("q" | "e"), dst, key]) => topology::delete_binding(m, req, vh, src, kind, dst, key).await,

        // vhosts
        (Get, ["vhosts"]) => access::vhosts(m, req),
        (Get, ["vhosts", vh]) => access::vhost(m, req, vh),
        (Put, ["vhosts", vh]) => access::put_vhost(m, req, vh).await,
        (Delete, ["vhosts", vh]) => access::delete_vhost(m, req, vh).await,
        (Get, ["vhosts", vh, "permissions"]) => access::vhost_permissions(m, req, vh),
        (Get, ["vhosts", vh, "topic-permissions"]) => access::vhost_topic_permissions(m, req, vh),
        (Get, ["vhosts", vh, "connections"]) => {
            check_vhost(m, req, vh)?;
            topology::connections(m, req, Some(vh))
        }
        (Get, ["vhosts", vh, "channels"]) => {
            check_vhost(m, req, vh)?;
            topology::channels(m, req, Some(vh))
        }
        (Post, ["vhosts", vh, "start", _node]) => {
            require_admin(req)?;
            check_vhost(m, req, vh)?;
            m.broker.open_vhost(vh).await.ok_or_else(ApiError::not_found)?;
            Ok(crate::http::no_content())
        }

        // users
        (Get, ["users"]) => access::users(m, req),
        (Get, ["users", "without-permissions"]) => access::users_without_permissions(m, req),
        (Post, ["users", "bulk-delete"]) => access::bulk_delete_users(m, req).await,
        (Get, ["users", u]) => access::user(m, req, u),
        (Put, ["users", u]) => access::put_user(m, req, u).await,
        (Delete, ["users", u]) => access::delete_user(m, req, u).await,
        (Get, ["users", u, "permissions"]) => access::user_permissions(m, req, u),
        (Get, ["users", u, "topic-permissions"]) => access::user_topic_permissions(m, req, u),

        // permissions
        (Get, ["permissions"]) => access::permissions(m, req),
        (Get, ["permissions", vh, u]) => access::permission(m, req, vh, u),
        (Put, ["permissions", vh, u]) => access::put_permission(m, req, vh, u).await,
        (Delete, ["permissions", vh, u]) => access::delete_permission(m, req, vh, u).await,
        (Get, ["topic-permissions"]) => access::topic_permissions(m, req),
        (Get, ["topic-permissions", vh, u]) => access::topic_permission(m, req, vh, u),
        (Put, ["topic-permissions", vh, u]) => access::put_topic_permission(m, req, vh, u).await,
        (Delete, ["topic-permissions", vh, u]) => access::delete_topic_permission(m, req, vh, u).await,
        (Delete, ["topic-permissions", vh, u, x]) => access::delete_topic_permission_for(m, req, vh, u, x).await,

        // limits
        (Get, ["vhost-limits"]) => access::vhost_limits(m, req, None),
        (Get, ["vhost-limits", vh]) => access::vhost_limits(m, req, Some(vh)),
        (Put, ["vhost-limits", vh, name]) => access::put_vhost_limit(m, req, vh, name).await,
        (Delete, ["vhost-limits", vh, name]) => access::delete_vhost_limit(m, req, vh, name).await,
        (Get, ["user-limits"]) => access::user_limits(m, req, None),
        (Get, ["user-limits", u]) => access::user_limits(m, req, Some(u)),
        (Put, ["user-limits", u, name]) => access::put_user_limit(m, req, u, name).await,
        (Delete, ["user-limits", u, name]) => access::delete_user_limit(m, req, u, name).await,

        // parameters & policies
        (Get, ["parameters"]) => admin::parameters(m, req, None, None),
        (Get, ["parameters", c]) => admin::parameters(m, req, Some(c), None),
        (Get, ["parameters", c, vh]) => admin::parameters(m, req, Some(c), Some(vh)),
        (Get, ["parameters", c, vh, n]) => admin::parameter(m, req, c, vh, n),
        (Put, ["parameters", c, vh, n]) => admin::put_parameter(m, req, c, vh, n).await,
        (Delete, ["parameters", c, vh, n]) => admin::delete_parameter(m, req, c, vh, n).await,
        (Get, ["global-parameters"]) => admin::global_parameters(m, req),
        (Get, ["global-parameters", n]) => admin::global_parameter(m, req, n),
        (Put, ["global-parameters", n]) => admin::put_global_parameter(m, req, n).await,
        (Delete, ["global-parameters", n]) => admin::delete_global_parameter(m, req, n).await,
        (Get, ["policies"]) => admin::policies(m, req, None, false),
        (Get, ["policies", vh]) => admin::policies(m, req, Some(vh), false),
        (Get, ["policies", vh, n]) => admin::policy(m, req, vh, n, false),
        (Put, ["policies", vh, n]) => admin::put_policy(m, req, vh, n, false).await,
        (Delete, ["policies", vh, n]) => admin::delete_policy(m, req, vh, n, false).await,
        (Get, ["operator-policies"]) => admin::policies(m, req, None, true),
        (Get, ["operator-policies", vh]) => admin::policies(m, req, Some(vh), true),
        (Get, ["operator-policies", vh, n]) => admin::policy(m, req, vh, n, true),
        (Put, ["operator-policies", vh, n]) => admin::put_policy(m, req, vh, n, true).await,
        (Delete, ["operator-policies", vh, n]) => admin::delete_policy(m, req, vh, n, true).await,

        // shovels & federation (status from pluggable providers)
        (Get, ["shovels"]) => admin::status_list(m, req, m.shovel_status(), None),
        (Get, ["shovels", vh]) => admin::status_list(m, req, m.shovel_status(), Some(vh)),
        (Get, ["shovels", "vhost", vh, n]) => admin::status_item(m, req, m.shovel_status(), vh, n),
        (Delete, ["shovels", "vhost", vh, n, "restart"]) => admin::restart_shovel(m, req, vh, n),
        (Get, ["federation-links"]) => admin::status_list(m, req, m.federation_status(), None),
        (Get, ["federation-links", vh]) => admin::status_list(m, req, m.federation_status(), Some(vh)),

        (M::Other, _) => Err(ApiError::method_not_allowed()),
        _ => Err(ApiError::not_found()),
    }
}

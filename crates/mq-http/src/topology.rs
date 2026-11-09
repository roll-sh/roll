//! Exchanges and bindings: declare and delete exchanges, bind and unbind. Permissions are the
//! ones AMQP checks (`configure` on the exchange; `write` on the destination and `read` on the
//! source to bind); tokens need a `manage` scope for the exchange or the destination.

use crate::HttpApi;
use crate::auth::{Auth, DEFAULT_EXCHANGE, Op, P, Q, parse_body};
use crate::error::{ApiError, ApiResult};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use mq_core::auth::Access;
use mq_core::exchange::{Dest, ExchangeRef};
use mq_core::value::FieldTable;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

fn exchange_json(vhost: &str, x: &ExchangeRef) -> Value {
    json!({
        "name": x.name,
        "vhost": vhost,
        "type": x.kind.name(),
        "durable": x.durable,
        "auto_delete": x.auto_delete,
        "internal": x.internal,
        "arguments": x.args.to_json(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclareReq {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    durable: Option<bool>,
    #[serde(default)]
    auto_delete: bool,
    #[serde(default)]
    internal: bool,
    #[serde(default)]
    arguments: Map<String, Value>,
}

/// `PUT /v1/vhosts/{vhost}/exchanges/{exchange}` — idempotent declare (201 when created).
pub async fn declare(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, exchange)): P<(String, String)>, body: Bytes) -> ApiResult<Response> {
    let req: DeclareReq = parse_body(&body)?;
    let kind = req.kind.filter(|k| !k.is_empty()).ok_or_else(|| ApiError::bad_request("type is required (direct, fanout, topic, headers or x-delayed-message)"))?;
    if exchange.is_empty() || exchange.len() > 255 {
        return Err(ApiError::bad_request("exchange names are 1-255 bytes"));
    }
    let vh = api.vhost(&auth, &vhost).await?;
    api.allow(&auth, &vhost, Op::Manage, "exchange", &exchange, None)?;
    let args = FieldTable::from_json(&Value::Object(req.arguments));
    if let Some(ae) = args.get_str("alternate-exchange") {
        // Like AMQP: an alternate exchange needs read access to it.
        api.permit(&auth, &vhost, Access::Read, "exchange", ae)?;
    }
    let existed = vh.exchange(&exchange).is_some();
    let x = vh.declare_exchange(&exchange, &kind, req.durable.unwrap_or(true), req.auto_delete, req.internal, args, false).await?;
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(exchange_json(&vhost, &x))).into_response())
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    /// Only delete the exchange if nothing is bound to it (409 otherwise).
    #[serde(default)]
    if_unused: bool,
}

/// `DELETE /v1/vhosts/{vhost}/exchanges/{exchange}` — 204, or 404 if there's no such exchange.
pub async fn delete(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, exchange)): P<(String, String)>, Q(q): Q<DeleteQuery>) -> ApiResult<StatusCode> {
    let vh = api.vhost(&auth, &vhost).await?;
    api.allow(&auth, &vhost, Op::Manage, "exchange", &exchange, None)?;
    vh.delete_exchange(&exchange, q.if_unused).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindReq {
    source: String,
    destination: String,
    #[serde(default)]
    destination_type: Option<String>,
    #[serde(default)]
    routing_key: String,
    #[serde(default)]
    arguments: Map<String, Value>,
}

/// Parse a bind/unbind body and check permissions: `write` on the destination and `read` on
/// the source (AMQP `queue.bind` / `exchange.bind`), plus a token `manage` scope for the
/// destination.
fn binding(api: &HttpApi, auth: &Auth, vhost: &str, body: &[u8]) -> ApiResult<(String, Dest, String, FieldTable, Value)> {
    let req: BindReq = parse_body(body)?;
    let (dest, kind) = match req.destination_type.as_deref().unwrap_or("queue") {
        "queue" => (Dest::queue(&req.destination), "queue"),
        "exchange" => (Dest::exchange(&req.destination), "exchange"),
        t => return Err(ApiError::bad_request(format!("destination_type is 'queue' or 'exchange', not '{t}'"))),
    };
    if req.source.is_empty() || req.destination.is_empty() {
        return Err(ApiError::bad_request("source and destination are required"));
    }
    api.scope(auth, vhost, Op::Manage, kind, &req.destination, None)?;
    api.permit(auth, vhost, Access::Write, kind, &req.destination)?;
    api.permit(auth, vhost, Access::Read, "exchange", &req.source)?;
    let source = if req.source == DEFAULT_EXCHANGE { String::new() } else { req.source };
    let shown = json!({
        "source": if source.is_empty() { DEFAULT_EXCHANGE } else { source.as_str() },
        "vhost": vhost,
        "destination": req.destination,
        "destination_type": kind,
        "routing_key": req.routing_key,
        "arguments": Value::Object(req.arguments.clone()),
    });
    Ok((source, dest, req.routing_key, FieldTable::from_json(&Value::Object(req.arguments)), shown))
}

/// `POST /v1/vhosts/{vhost}/bindings` — bind a queue (or exchange) to an exchange; 201.
pub async fn bind(State(api): State<Arc<HttpApi>>, auth: Auth, P(vhost): P<String>, body: Bytes) -> ApiResult<Response> {
    let vh = api.vhost(&auth, &vhost).await?;
    let (source, dest, key, args, shown) = binding(&api, &auth, &vhost, &body)?;
    vh.bind(&source, dest, &key, args, None).await?;
    Ok((StatusCode::CREATED, Json(shown)).into_response())
}

/// `DELETE /v1/vhosts/{vhost}/bindings` (same body) — remove a binding; 204 (also when there
/// was no such binding, like AMQP).
pub async fn unbind(State(api): State<Arc<HttpApi>>, auth: Auth, P(vhost): P<String>, body: Bytes) -> ApiResult<StatusCode> {
    let vh = api.vhost(&auth, &vhost).await?;
    let (source, dest, key, args, _) = binding(&api, &auth, &vhost, &body)?;
    vh.unbind(&source, dest, &key, args, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

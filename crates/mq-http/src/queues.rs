//! Queue endpoints (list, declare, depth, purge, delete) and health.

use crate::HttpApi;
use crate::auth::{Auth, Op, P, Q, parse_body};
use crate::error::{ApiError, ApiResult};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use mq_core::auth::Access;
use mq_core::queue::{QueueRef, QueueType};
use mq_core::value::FieldTable;
use mq_core::vhost::DeclareQueue;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// `GET /v1/health` (no auth): 503 while the broker drains, so load balancers move on.
pub async fn health(State(api): State<Arc<HttpApi>>) -> Response {
    let draining = api.is_stopping();
    let body = json!({
        "status": if draining { "draining" } else { "ok" },
        "node": api.broker.config.node_name,
        "version": env!("CARGO_PKG_VERSION"),
        "url": api.config.public_url,
    });
    let status = if draining { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (status, Json(body)).into_response()
}

pub(crate) fn queue_json(q: &QueueRef) -> Value {
    let s = &q.shared;
    let mut v = json!({
        "name": q.meta.name,
        "vhost": q.meta.vhost,
        "type": q.kind.name(),
        "durable": q.meta.durable,
        "auto_delete": q.meta.auto_delete,
        "exclusive": q.meta.exclusive(),
        "arguments": q.meta.arguments.to_json(),
        "messages_ready": s.ready.load(Ordering::Relaxed),
        "messages_unacked": s.unacked.load(Ordering::Relaxed),
        "messages": s.messages(),
        "consumers": s.consumers.load(Ordering::Relaxed),
    });
    if q.kind == QueueType::Stream {
        v["first_offset"] = s.first_offset.load(Ordering::Relaxed).into();
        v["next_offset"] = s.next_offset.load(Ordering::Relaxed).into();
    }
    v
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclareReq {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    durable: Option<bool>,
    #[serde(default)]
    auto_delete: bool,
    #[serde(default)]
    arguments: Map<String, Value>,
}

/// `PUT /v1/vhosts/{vhost}/queues/{queue}` — idempotent declare (201 when created).
pub async fn declare(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, body: Bytes) -> ApiResult<Response> {
    let req: DeclareReq = parse_body(&body)?;
    if queue.is_empty() || queue.len() > 255 {
        return Err(ApiError::bad_request("queue names are 1-255 bytes"));
    }
    let vh = api.vhost(&auth, &vhost).await?;
    api.allow(&auth, &vhost, Op::Manage, "queue", &queue, None)?;
    let mut args = FieldTable::from_json(&Value::Object(req.arguments));
    if let Some(t) = req.kind {
        if QueueType::parse(&t).is_none() {
            return Err(ApiError::bad_request(format!("unknown queue type '{t}' (classic, quorum or stream)")));
        }
        args.insert("x-queue-type", t.as_str());
    }
    if let Some(dlx) = args.get_str("x-dead-letter-exchange") {
        // Like AMQP: dead-lettering into an exchange needs write access to it.
        let dlx = if dlx.is_empty() { crate::auth::DEFAULT_EXCHANGE } else { dlx };
        api.broker.check(&auth.user, &vhost, mq_core::auth::Access::Write, "exchange", dlx)?;
    }
    let existed = vh.queue(&queue).is_some();
    let ok = vh
        .declare_queue(DeclareQueue { name: queue, durable: req.durable.unwrap_or(true), exclusive_owner: None, auto_delete: req.auto_delete, args, passive: false })
        .await?;
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(queue_json(&ok.queue))).into_response())
}

/// `GET /v1/vhosts/{vhost}/queues/{queue}` — depth and consumers.
pub async fn get_queue(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>) -> ApiResult<Json<Value>> {
    let vh = api.vhost(&auth, &vhost).await?;
    if api.allow(&auth, &vhost, Op::Consume, "queue", &queue, None).is_err() {
        api.allow(&auth, &vhost, Op::Manage, "queue", &queue, None)?;
    }
    let q = vh.queue_for(&queue, Some(0))?;
    Ok(Json(queue_json(&q)))
}

#[derive(Deserialize)]
pub struct ListQuery {
    /// Only queues whose name starts with this.
    #[serde(default)]
    prefix: Option<String>,
}

/// `GET /v1/vhosts/{vhost}/queues` — the queues the caller may read or configure, by name.
pub async fn list(State(api): State<Arc<HttpApi>>, auth: Auth, P(vhost): P<String>, Q(q): Q<ListQuery>) -> ApiResult<Json<Value>> {
    let vh = api.vhost(&auth, &vhost).await?;
    let mut queues: Vec<QueueRef> = vh
        .queues()
        .into_iter()
        .filter(|x| x.meta.exclusive_owner.is_none())
        .filter(|x| q.prefix.as_deref().is_none_or(|p| x.meta.name.starts_with(p)))
        .filter(|x| {
            let name = &x.meta.name;
            api.allow(&auth, &vhost, Op::Consume, "queue", name, None).is_ok() || api.allow(&auth, &vhost, Op::Manage, "queue", name, None).is_ok()
        })
        .collect();
    queues.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    Ok(Json(Value::Array(queues.iter().map(queue_json).collect())))
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    /// Only delete the queue if it has no messages (409 otherwise).
    #[serde(default)]
    if_empty: bool,
    /// Only delete the queue if it has no consumers (409 otherwise).
    #[serde(default)]
    if_unused: bool,
}

/// `DELETE /v1/vhosts/{vhost}/queues/{queue}` — delete a queue and its messages (`configure`
/// on the queue, like AMQP `queue.delete`).
pub async fn delete(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, Q(q): Q<DeleteQuery>) -> ApiResult<Json<Value>> {
    let vh = api.vhost(&auth, &vhost).await?;
    api.allow(&auth, &vhost, Op::Manage, "queue", &queue, None)?;
    let n = vh.delete_queue(&queue, q.if_unused, q.if_empty, None).await?;
    Ok(Json(json!({ "deleted": queue, "messages": n })))
}

/// `POST /v1/vhosts/{vhost}/queues/{queue}/purge` — drop the ready messages (`read` on the
/// queue, like AMQP `queue.purge`; tokens need a `manage` scope).
pub async fn purge(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>) -> ApiResult<Json<Value>> {
    let vh = api.vhost(&auth, &vhost).await?;
    api.scope(&auth, &vhost, Op::Manage, "queue", &queue, None)?;
    api.permit(&auth, &vhost, Access::Read, "queue", &queue)?;
    let n = vh.queue_for(&queue, None)?.purge().await?;
    Ok(Json(json!({ "purged": n })))
}

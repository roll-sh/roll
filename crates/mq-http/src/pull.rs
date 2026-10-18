//! Pull consumption: `receive` leases messages (long-polling on the queue's `available`
//! notifier) and returns receipts; `ack`, `nack` and `visibility` settle or extend them.
//! Leases share the queue with AMQP consumers: an expired lease goes back to the queue (and
//! to whichever consumer is next), rejects dead-letter, and delivery counts keep counting.

use crate::HttpApi;
use crate::auth::{Auth, Op, P, parse_body};
use crate::error::{ApiError, ApiResult};
use crate::message;
use crate::receipt::{Holder, Receipt};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use bytes::Bytes;
use mq_core::VHost;
use mq_core::queue::{Outcome, QueueRef, QueueType};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiveReq {
    #[serde(default)]
    max: Option<u64>,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default)]
    visibility_ms: Option<u64>,
}

/// Resolve the vhost and queue for a consume-side request.
async fn consume_queue(api: &HttpApi, auth: &Auth, vhost: &str, queue: &str) -> ApiResult<(Arc<VHost>, QueueRef)> {
    let vh = api.vhost(auth, vhost).await?;
    api.allow(auth, vhost, Op::Consume, "queue", queue, None)?;
    let q = vh.queue_for(queue, Some(0))?;
    Ok((vh, q))
}

/// `POST /v1/vhosts/{vhost}/queues/{queue}/receive`
pub async fn receive(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, body: Bytes) -> ApiResult<Json<Value>> {
    let req: ReceiveReq = parse_body(&body)?;
    let max = req.max.unwrap_or(1);
    if !(1..=100).contains(&max) {
        return Err(ApiError::bad_request("max must be between 1 and 100"));
    }
    let wait_ms = req.wait_ms.unwrap_or(0);
    if wait_ms > api.config.max_wait_ms {
        return Err(ApiError::bad_request(format!("wait_ms must be at most {}", api.config.max_wait_ms)));
    }
    let visibility_ms = req.visibility_ms.unwrap_or(api.config.default_visibility_ms);
    if visibility_ms == 0 || visibility_ms > api.config.max_visibility_ms {
        return Err(ApiError::bad_request(format!("visibility_ms must be between 1 and {}", api.config.max_visibility_ms)));
    }
    let (vh, q) = consume_queue(&api, &auth, &vhost, &queue).await?;
    if q.kind == QueueType::Stream {
        return Err(ApiError::bad_request("receive is not supported on stream queues; read them with the events endpoint (SSE) or the WebSocket"));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
    let mut stop = api.stop_rx();
    let leased = loop {
        // Register interest before looking, so a publish in between isn't missed.
        let available = q.shared.available.notified();
        tokio::pin!(available);
        available.as_mut().enable();
        let leased = q.lease(max as usize, visibility_ms).await?;
        if !leased.is_empty() || wait_ms == 0 || tokio::time::Instant::now() >= deadline || api.is_stopping() || q.is_deleted() {
            break leased;
        }
        tokio::select! {
            _ = available => {}
            _ = tokio::time::sleep_until(deadline) => {}
            _ = stop.changed() => {}
        }
    };
    let mut out = Vec::with_capacity(leased.len());
    for l in leased {
        vh.stats.delivered.fetch_add(1, Ordering::Relaxed);
        vh.stats.delivered_bytes.fetch_add(l.msg.body.len() as u64, Ordering::Relaxed);
        let r = Receipt { vhost: vhost.clone(), queue: queue.clone(), seq: l.seq, delivery_count: l.delivery_count, holder: Holder::Lease { deadline_ms: l.deadline_ms } };
        let mut m = message::render(q.kind, &queue, l.seq, &l.msg, l.redelivered, l.delivery_count);
        m.insert("receipt".into(), r.encode(api.receipt_key()).into());
        m.insert("visible_at_ms".into(), l.deadline_ms.into());
        out.push(Value::Object(m));
    }
    Ok(Json(json!({ "messages": out })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettleReq {
    receipts: Vec<String>,
    #[serde(default)]
    requeue: Option<bool>,
    #[serde(default)]
    delay_ms: Option<u64>,
    #[serde(default)]
    visibility_ms: Option<u64>,
}

#[derive(Clone, Copy)]
enum Action {
    Ack,
    Nack { requeue: bool, delay_ms: u64 },
    Visibility(u64),
}

impl HttpApi {
    pub(crate) fn receipt_key(&self) -> &[u8] {
        &self.receipt_key
    }

    /// Settle (or extend) one receipt. Returns a fresh receipt for visibility changes.
    async fn apply(&self, vh: &VHost, q: &QueueRef, raw: &str, action: Action) -> ApiResult<Option<String>> {
        let r = Receipt::decode(&self.receipt_key, raw).ok_or_else(|| ApiError::bad_request("invalid receipt").code("invalid_receipt"))?;
        if r.vhost != vh.name || r.queue != q.meta.name {
            return Err(ApiError::bad_request(format!("receipt belongs to queue '{}' in vhost '{}'", r.queue, r.vhost)).code("invalid_receipt"));
        }
        let outcome = match action {
            Action::Ack => Outcome::Ack,
            Action::Nack { requeue: false, .. } => Outcome::Reject,
            Action::Nack { requeue: true, .. } => Outcome::Requeue,
            Action::Visibility(_) => Outcome::Requeue,
        };
        match r.holder {
            Holder::Lease { deadline_ms } => {
                // A delayed nack keeps the lease until the delay is over; then it returns.
                let ok = match action {
                    Action::Visibility(ms) => q.extend_lease(r.seq, r.delivery_count, ms).await?,
                    Action::Nack { requeue: true, delay_ms } if delay_ms > 0 => q.extend_lease(r.seq, r.delivery_count, delay_ms).await?,
                    _ => q.settle_lease(r.seq, r.delivery_count, outcome).await?,
                };
                if !ok {
                    return Err(stale(deadline_ms));
                }
                if let Action::Visibility(ms) = action {
                    let fresh = Receipt { holder: Holder::Lease { deadline_ms: mq_core::now_ms() + ms }, ..r };
                    return Ok(Some(fresh.encode(&self.receipt_key)));
                }
            }
            Holder::Session { channel } => {
                match action {
                    Action::Visibility(_) => return Err(ApiError::bad_request("visibility applies to receipts from receive, not to streamed deliveries")),
                    Action::Nack { delay_ms, .. } if delay_ms > 0 => {
                        return Err(ApiError::bad_request("delay_ms applies to receipts from receive, not to streamed deliveries"));
                    }
                    _ => {}
                }
                let session = self.sessions.read().get(&channel).cloned();
                let Some(s) = session else {
                    return Err(ApiError::new(StatusCode::GONE, "session_closed", "the stream that delivered this message has ended; the message was returned to the queue"));
                };
                if !s.settle(r.seq, outcome).await {
                    return Err(ApiError::conflict("receipt_stale", "this message was already acknowledged or returned to the queue"));
                }
            }
        }
        if matches!(action, Action::Ack) {
            vh.stats.acked.fetch_add(1, Ordering::Relaxed);
        }
        Ok(None)
    }
}

fn stale(deadline_ms: u64) -> ApiError {
    if mq_core::now_ms() >= deadline_ms {
        ApiError::new(StatusCode::GONE, "receipt_expired", "the visibility timeout expired and the message was returned to the queue; receive it again")
    } else {
        ApiError::conflict("receipt_stale", "this message was already acknowledged, returned to the queue or redelivered")
    }
}

impl ApiError {
    fn code(mut self, code: &'static str) -> Self {
        self.code = code;
        self
    }
}

async fn settle_all(api: &HttpApi, auth: &Auth, vhost: &str, queue: &str, receipts: &[String], action: Action) -> ApiResult<Json<Value>> {
    if receipts.is_empty() {
        return Err(ApiError::bad_request("no receipts"));
    }
    if receipts.len() > 1000 {
        return Err(ApiError::bad_request("at most 1000 receipts per request"));
    }
    let (vh, q) = consume_queue(api, auth, vhost, queue).await?;
    let mut results = Vec::with_capacity(receipts.len());
    let mut first_err: Option<ApiError> = None;
    let mut ok = 0;
    for raw in receipts {
        match api.apply(&vh, &q, raw, action).await {
            Ok(fresh) => {
                ok += 1;
                let mut r = json!({"receipt": raw, "ok": true});
                if let Some(f) = fresh {
                    r["receipt"] = f.into();
                    r["previous_receipt"] = raw.clone().into();
                }
                results.push(r);
            }
            Err(e) => {
                results.push(json!({"receipt": raw, "ok": false, "error": e.body()}));
                first_err.get_or_insert(e);
            }
        }
    }
    if matches!(action, Action::Ack | Action::Nack { .. }) {
        q.wake();
    }
    match first_err {
        Some(e) => Err(e.with("results", Value::Array(results)).with("succeeded", ok.into())),
        None => Ok(Json(json!({"succeeded": ok, "results": results}))),
    }
}

/// `POST .../ack`
pub async fn ack(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, body: Bytes) -> ApiResult<Json<Value>> {
    let req: SettleReq = parse_body(&body)?;
    if req.requeue.is_some() || req.delay_ms.is_some() || req.visibility_ms.is_some() {
        return Err(ApiError::bad_request("ack takes only 'receipts'"));
    }
    settle_all(&api, &auth, &vhost, &queue, &req.receipts, Action::Ack).await
}

/// `POST .../nack` — `requeue` (default true) returns messages to the queue, after `delay_ms`
/// if given; `requeue: false` rejects them (dead-lettered if the queue has a DLX).
pub async fn nack(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, body: Bytes) -> ApiResult<Json<Value>> {
    let req: SettleReq = parse_body(&body)?;
    let requeue = req.requeue.unwrap_or(true);
    let delay_ms = req.delay_ms.unwrap_or(0);
    if req.visibility_ms.is_some() {
        return Err(ApiError::bad_request("nack doesn't take visibility_ms; use delay_ms"));
    }
    if delay_ms > 0 && !requeue {
        return Err(ApiError::bad_request("delay_ms needs requeue: true"));
    }
    if delay_ms > api.config.max_visibility_ms {
        return Err(ApiError::bad_request(format!("delay_ms must be at most {}", api.config.max_visibility_ms)));
    }
    settle_all(&api, &auth, &vhost, &queue, &req.receipts, Action::Nack { requeue, delay_ms }).await
}

/// `POST .../visibility` — sets the remaining invisibility of leased messages (0 releases them
/// now). Returns fresh receipts; the old ones stay valid.
pub async fn visibility(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, queue)): P<(String, String)>, body: Bytes) -> ApiResult<Json<Value>> {
    let req: SettleReq = parse_body(&body)?;
    let Some(ms) = req.visibility_ms else {
        return Err(ApiError::bad_request("visibility_ms is required"));
    };
    if ms > api.config.max_visibility_ms {
        return Err(ApiError::bad_request(format!("visibility_ms must be at most {}", api.config.max_visibility_ms)));
    }
    settle_all(&api, &auth, &vhost, &queue, &req.receipts, Action::Visibility(ms)).await
}

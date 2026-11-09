//! HTTP API, mounted under `/v1/tasks`.

use crate::engine::{Dur, FlowRequest, PublishRequest, QueueUpdate, RateSpec, TaskError, Tasks, Time, parse_rate};
use crate::model::*;
use crate::schedules::{ScheduleRequest, ScheduleUpdate};
use crate::store::{EventQuery, MessageQuery, RunQuery};
use crate::workflow::{EventIn, SyncRequest};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Tasks>>;
type ApiResult<T = Value> = Result<Json<T>, TaskError>;

impl IntoResponse for TaskError {
    fn into_response(self) -> Response {
        let status = match &self {
            TaskError::NotFound(_) => StatusCode::NOT_FOUND,
            TaskError::Invalid(_) => StatusCode::BAD_REQUEST,
            TaskError::Conflict(_) => StatusCode::CONFLICT,
            TaskError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            TaskError::Internal(e) if e.chain().any(|c| c.is::<crate::store::log::Unavailable>()) => {
                // This node stopped leading mid-request; nothing was committed. Proxies retry
                // these on the new leader.
                let mut res = (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the node handling this request stepped down; retry" }))).into_response();
                res.headers_mut().insert(crate::cluster::NOT_LEADER, header::HeaderValue::from_static("1"));
                return res;
            }
            TaskError::Internal(e) => {
                // The whole chain goes to the log; clients don't see backend details.
                tracing::error!(error = format!("{e:#}"), "internal error");
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal error" }))).into_response();
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

fn ok(v: impl serde::Serialize) -> ApiResult {
    Ok(Json(serde_json::to_value(v).map_err(anyhow::Error::from)?))
}

/// The tasks API. Merge into a larger router, or serve alone.
pub fn router(tasks: Arc<Tasks>) -> Router {
    let limit = tasks.config.max_body_bytes + (256 << 10);
    let api = Router::new()
        .route("/publish", post(publish))
        .route("/publish/{*destination}", post(publish_raw))
        .route("/batch", post(batch))
        .route("/messages", get(list_messages))
        .route("/messages/cancel", post(cancel_messages))
        .route("/messages/{id}", get(get_message).delete(delete_message))
        .route("/messages/{id}/retry", post(retry_message))
        .route("/dlq", get(list_dlq))
        .route("/dlq/retry", post(retry_dlq))
        .route("/dlq/{id}", delete(delete_dlq))
        .route("/queues", get(list_queues))
        .route("/queues/{name}", get(get_queue).put(put_queue).delete(delete_queue))
        .route("/queues/{name}/pause", post(pause_queue))
        .route("/queues/{name}/resume", post(resume_queue))
        .route("/queues/{name}/pull", post(pull))
        .route("/queues/{name}/ack", post(ack))
        .route("/queues/{name}/nack", post(nack))
        .route("/queues/{name}/extend", post(extend))
        .route("/topics", get(list_topics))
        .route("/topics/{name}", get(get_topic).put(put_topic).delete(delete_topic))
        .route("/topics/{name}/endpoints", post(add_endpoints).delete(remove_endpoints))
        .route("/schedules", get(list_schedules).post(create_schedule))
        .route("/schedules/{id}", get(get_schedule).patch(update_schedule).delete(delete_schedule))
        .route("/schedules/{id}/pause", post(pause_schedule))
        .route("/schedules/{id}/resume", post(resume_schedule))
        .route("/schedules/{id}/trigger", post(trigger_schedule))
        .route("/events", get(list_events).post(send_events))
        .route("/events/{id}", get(get_event))
        .route("/e/{*name}", post(ingest))
        .route("/apps", get(list_apps).post(sync_app))
        .route("/apps/{id}", delete(delete_app))
        .route("/functions", get(list_functions))
        .route("/functions/{id}", get(get_function))
        .route("/functions/{id}/invoke", post(invoke_function))
        .route("/functions/{id}/pause", post(pause_function))
        .route("/functions/{id}/resume", post(resume_function))
        .route("/functions/{id}/cancel", post(cancel_function_runs))
        .route("/runs", get(list_runs))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/runs/{id}/rerun", post(rerun))
        .route("/runs/{id}/wait", get(wait_run))
        .route("/keys", get(signing_keys))
        .route("/keys/rotate", post(rotate_keys))
        .route("/api-keys", get(list_api_keys).post(create_api_key))
        .route("/api-keys/{id}", delete(delete_api_key))
        .route("/usage", get(usage))
        .route("/stats", get(stats))
        .layer(middleware::from_fn_with_state(tasks.clone(), auth))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::map_response(json_errors))
        .with_state(tasks);
    // Bearer tokens, no cookies: any origin may call it (the dashboard does).
    Router::new().nest("/v1/tasks", api).layer(tower_http::cors::CorsLayer::permissive())
}

/// Extractor rejections (malformed JSON, oversized bodies …) are plain text; make every error
/// `{"error": …}` like the rest of the API.
async fn json_errors(res: Response) -> Response {
    let plain = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/plain"));
    if !(plain && res.status().is_client_error()) {
        return res;
    }
    let (parts, body) = res.into_parts();
    let text = axum::body::to_bytes(body, 64 << 10).await.unwrap_or_default();
    (parts.status, Json(json!({ "error": String::from_utf8_lossy(&text).trim() }))).into_response()
}

async fn auth(State(t): S, req: Request, next: Next) -> Response {
    // API-only nodes (Lambda) have no heartbeat: keep config and keys fresh per request.
    if let Err(e) = t.refresh_if_older(t.config.refresh_ms).await {
        tracing::warn!(error = format!("{e:#}"), "refreshing config");
    }
    if t.auth_required() {
        let bearer = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .map(|s| s.trim().to_string());
        // A key minted on another node moments ago: look again before refusing.
        let ok = t.authorize(bearer.as_deref())
            || (bearer.is_some() && t.refresh_if_older(250).await.is_ok() && t.authorize(bearer.as_deref()));
        if !ok {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing or invalid API token" }))).into_response();
        }
        if let Some(token) = &bearer
            && t.config.token.as_deref() != Some(token.as_str())
        {
            t.touch_api_key(token).await;
        }
    }
    let res = next.run(req).await;
    if let Err(e) = t.flush_usage_if_older(t.config.heartbeat_ms).await {
        tracing::warn!(error = format!("{e:#}"), "flushing usage");
    }
    res
}

async fn health(State(t): S) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "store": t.store.describe(),
        "node": t.config.node_id,
        "auth": t.auth_required(),
    }))
}

// --- publishing ---

#[derive(serde::Serialize)]
struct PublishResponse {
    message_id: String,
    messages: Vec<crate::engine::Published>,
}

fn published(out: Vec<crate::engine::Published>) -> PublishResponse {
    PublishResponse { message_id: out.first().map(|p| p.message_id.clone()).unwrap_or_default(), messages: out }
}

async fn publish(State(t): S, Json(req): Json<PublishRequest>) -> Result<(StatusCode, Json<Value>), TaskError> {
    let out = t.publish(req).await?;
    Ok((StatusCode::CREATED, ok(published(out))?))
}

async fn batch(State(t): S, Json(reqs): Json<Vec<PublishRequest>>) -> Result<(StatusCode, Json<Value>), TaskError> {
    if reqs.len() > 1000 {
        return Err(TaskError::Invalid("at most 1000 messages per batch".into()));
    }
    let mut results = Vec::with_capacity(reqs.len());
    for r in reqs {
        results.push(match t.publish(r).await {
            Ok(out) => serde_json::to_value(published(out)).map_err(anyhow::Error::from)?,
            Err(e) => json!({ "error": e.to_string() }),
        });
    }
    Ok((StatusCode::CREATED, Json(json!(results))))
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

fn header_parse<T: std::str::FromStr>(h: &HeaderMap, name: &str) -> Result<Option<T>, TaskError> {
    header_str(h, name)
        .map(|s| s.parse::<T>().map_err(|_| TaskError::Invalid(format!("bad {name} header {s:?}"))))
        .transpose()
}

/// `POST /v1/tasks/publish/<url or topic>` with the raw body; options in `Roll-*` headers,
/// headers to forward as `Roll-Forward-<name>`.
async fn publish_raw(
    State(t): S,
    Path(destination): Path<String>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), TaskError> {
    let mut req = PublishRequest::default();
    let dest = destination.trim_start_matches('/');
    if dest.starts_with("http://") || dest.starts_with("https://") || dest.starts_with("http:/") || dest.starts_with("https:/") {
        // Some proxies squash `//`; put it back, and keep the destination's query string.
        let fixed = if dest.contains("://") { dest.to_string() } else { dest.replacen(":/", "://", 1) };
        req.url = Some(match uri.query() {
            Some(q) => format!("{fixed}?{q}"),
            None => fixed,
        });
    } else if !dest.is_empty() {
        req.topic = Some(dest.to_string());
    }
    req.queue = header_str(&headers, "roll-queue").map(String::from);
    req.method = header_str(&headers, "roll-method").map(String::from);
    req.delay = header_str(&headers, "roll-delay").map(|s| crate::schedule::parse_duration_ms(s).map(Dur)).transpose().map_err(TaskError::Invalid)?;
    req.not_before = header_str(&headers, "roll-not-before").map(|s| crate::engine::parse_time(s).map(Time)).transpose().map_err(TaskError::Invalid)?;
    req.retries = header_parse(&headers, "roll-retries")?;
    req.timeout = header_str(&headers, "roll-timeout").map(|s| crate::schedule::parse_duration_ms(s).map(Dur)).transpose().map_err(TaskError::Invalid)?;
    req.callback = header_str(&headers, "roll-callback").map(String::from);
    req.failure_callback = header_str(&headers, "roll-failure-callback").map(String::from);
    req.dedup_id = header_str(&headers, "roll-deduplication-id").map(String::from);
    req.content_dedup = header_str(&headers, "roll-content-based-deduplication").is_some_and(|v| v == "true" || v == "1");
    req.group = header_str(&headers, "roll-group").map(String::from);
    req.priority = header_parse(&headers, "roll-priority")?;
    req.label = header_str(&headers, "roll-label").map(String::from);
    req.content_type = header_str(&headers, "content-type").map(String::from);
    if let Some(key) = header_str(&headers, "roll-flow-key") {
        req.flow = Some(FlowRequest {
            key: key.into(),
            parallelism: header_parse(&headers, "roll-flow-parallelism")?,
            rate: header_str(&headers, "roll-flow-rate").map(|r| parse_rate(&json!(r)).map(RateSpec)).transpose().map_err(TaskError::Invalid)?,
        });
    }
    for (name, value) in &headers {
        if let Some(fwd) = name.as_str().strip_prefix("roll-forward-")
            && let Ok(v) = value.to_str()
        {
            req.headers.insert(fwd.to_string(), v.to_string());
        }
    }
    req.body_base64 = (!body.is_empty()).then(|| base64::engine::general_purpose::STANDARD.encode(&body));
    let out = t.publish(req).await?;
    Ok((StatusCode::CREATED, ok(published(out))?))
}

// --- messages ---

#[derive(Deserialize, Default)]
struct ListParams {
    state: Option<String>,
    queue: Option<String>,
    topic: Option<String>,
    schedule_id: Option<String>,
    run_id: Option<String>,
    label: Option<String>,
    url: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
    /// Include internal messages (ticks, timers, run calls).
    #[serde(default)]
    all: bool,
}

impl ListParams {
    fn query(self) -> Result<MessageQuery, TaskError> {
        let state = match self.state.as_deref() {
            None | Some("") | Some("all") => None,
            Some("dlq") => Some(MsgState::Failed),
            Some(s) => Some(MsgState::parse(s).ok_or_else(|| TaskError::Invalid(format!("unknown state {s:?}")))?),
        };
        Ok(MessageQuery {
            state,
            queue: self.queue,
            topic: self.topic,
            schedule_id: self.schedule_id,
            run_id: self.run_id,
            label: self.label,
            url: self.url,
            user_only: !self.all,
            cursor: self.cursor,
            limit: self.limit.unwrap_or(50),
        })
    }
}

async fn list_messages(State(t): S, Query(p): Query<ListParams>) -> ApiResult {
    ok(t.messages(&p.query()?).await?)
}

fn body_json(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!({ "body": s }),
        Err(_) => json!({ "body_base64": base64::engine::general_purpose::STANDARD.encode(b) }),
    }
}

async fn get_message(State(t): S, Path(id): Path<String>) -> ApiResult {
    let m = t.message(&id).await?;
    let attempts = t.attempts(&id).await?;
    let body = t.message_body(&id).await?;
    let mut v = serde_json::to_value(&m).map_err(anyhow::Error::from)?;
    v["attempts_log"] = json!(attempts);
    if let (Value::Object(o), Value::Object(b)) = (&mut v, body_json(&body)) {
        o.extend(b);
    }
    Ok(Json(v))
}

#[derive(Deserialize, Default)]
struct DeleteParams {
    #[serde(default)]
    purge: bool,
}

async fn delete_message(State(t): S, Path(id): Path<String>, Query(p): Query<DeleteParams>) -> Result<Response, TaskError> {
    if p.purge {
        t.delete_message(&id).await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(ok(t.cancel(&id, "canceled").await?)?.into_response())
}

#[derive(Deserialize, Default)]
struct CancelMany {
    #[serde(default)]
    ids: Vec<String>,
    queue: Option<String>,
    url: Option<String>,
    topic: Option<String>,
    label: Option<String>,
    schedule_id: Option<String>,
    /// Required to cancel by filter without any filter.
    #[serde(default)]
    all: bool,
}

async fn cancel_messages(State(t): S, Json(b): Json<CancelMany>) -> ApiResult {
    if !b.ids.is_empty() {
        let mut n = 0;
        for id in &b.ids {
            if t.cancel(id, "canceled").await.is_ok() {
                n += 1;
            }
        }
        return ok(json!({ "canceled": n }));
    }
    let q = MessageQuery { queue: b.queue, url: b.url, topic: b.topic, label: b.label, schedule_id: b.schedule_id, ..Default::default() };
    if q.queue.is_none() && q.url.is_none() && q.topic.is_none() && q.label.is_none() && q.schedule_id.is_none() && !b.all {
        return Err(TaskError::Invalid("give ids, a filter, or all: true".into()));
    }
    ok(json!({ "canceled": t.cancel_many(q).await? }))
}

async fn retry_message(State(t): S, Path(id): Path<String>) -> ApiResult {
    ok(t.retry(&id).await?)
}

async fn list_dlq(State(t): S, Query(mut p): Query<ListParams>) -> ApiResult {
    p.state = Some("failed".into());
    ok(t.messages(&p.query()?).await?)
}

#[derive(Deserialize, Default)]
struct RetryMany {
    #[serde(default)]
    ids: Vec<String>,
    queue: Option<String>,
    #[serde(default)]
    all: bool,
}

async fn retry_dlq(State(t): S, Json(b): Json<RetryMany>) -> ApiResult {
    let mut ids = b.ids;
    if ids.is_empty() {
        if b.queue.is_none() && !b.all {
            return Err(TaskError::Invalid("give ids, a queue, or all: true".into()));
        }
        let mut q = MessageQuery { state: Some(MsgState::Failed), queue: b.queue, user_only: true, limit: 500, ..Default::default() };
        loop {
            let page = t.messages(&q).await?;
            ids.extend(page.items.into_iter().map(|m| m.id));
            match page.cursor {
                Some(c) => q.cursor = Some(c),
                None => break,
            }
        }
    }
    let mut n = 0;
    for id in &ids {
        if t.retry(id).await.is_ok() {
            n += 1;
        }
    }
    ok(json!({ "retried": n }))
}

async fn delete_dlq(State(t): S, Path(id): Path<String>) -> Result<StatusCode, TaskError> {
    let m = t.message(&id).await?;
    if m.state != MsgState::Failed {
        return Err(TaskError::Conflict(format!("message {id} isn't in the dead-letter queue")));
    }
    t.delete_message(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- queues ---

async fn queue_view(t: &Tasks, q: Queue) -> Result<Value, TaskError> {
    let stats = t.queue_stats(&q).await?;
    let mut v = serde_json::to_value(&q).map_err(anyhow::Error::from)?;
    v["stats"] = stats;
    Ok(v)
}

async fn list_queues(State(t): S) -> ApiResult {
    let mut out = Vec::new();
    for q in t.queues().await? {
        out.push(queue_view(&t, q).await?);
    }
    ok(out)
}

async fn get_queue(State(t): S, Path(name): Path<String>) -> ApiResult {
    let q = t.queue(&name).await?.value;
    ok(queue_view(&t, q).await?)
}

async fn put_queue(State(t): S, Path(name): Path<String>, body: Option<Json<QueueUpdate>>) -> ApiResult {
    let update = body.map(|b| b.0).unwrap_or_default();
    ok(t.put_queue(&name, update).await?)
}

#[derive(Deserialize, Default)]
struct ForceParams {
    #[serde(default)]
    force: bool,
}

async fn delete_queue(State(t): S, Path(name): Path<String>, Query(p): Query<ForceParams>) -> ApiResult {
    if name.starts_with("fn:") {
        return Err(TaskError::Invalid("function queues go away with their app".into()));
    }
    ok(json!({ "canceled": t.delete_queue(&name, p.force).await? }))
}

async fn pause_queue(State(t): S, Path(name): Path<String>) -> ApiResult {
    t.queue(&name).await?;
    ok(t.put_queue(&name, QueueUpdate { paused: Some(true), internal: true, ..Default::default() }).await?)
}

async fn resume_queue(State(t): S, Path(name): Path<String>) -> ApiResult {
    t.queue(&name).await?;
    ok(t.put_queue(&name, QueueUpdate { paused: Some(false), internal: true, ..Default::default() }).await?)
}

#[derive(Deserialize, Default)]
struct PullBody {
    max: Option<usize>,
    visibility: Option<Dur>,
    wait: Option<Dur>,
}

async fn pull(State(t): S, Path(name): Path<String>, body: Option<Json<PullBody>>) -> ApiResult {
    let b = body.map(|b| b.0).unwrap_or_default();
    let msgs = t.pull(&name, b.max.unwrap_or(1), b.visibility.map(|d| d.0).unwrap_or(30_000), b.wait.map(|d| d.0).unwrap_or(0)).await?;
    ok(json!({ "messages": msgs }))
}

#[derive(Deserialize)]
struct AckBody {
    receipt: Option<String>,
    #[serde(default)]
    receipts: Vec<String>,
}

async fn ack(State(t): S, Path(name): Path<String>, Json(b): Json<AckBody>) -> ApiResult {
    let mut receipts = b.receipts;
    receipts.extend(b.receipt);
    if receipts.is_empty() {
        return Err(TaskError::Invalid("give a receipt".into()));
    }
    let mut errors = Vec::new();
    for r in &receipts {
        if let Err(e) = t.ack(&name, r).await {
            if receipts.len() == 1 {
                return Err(e);
            }
            errors.push(json!({ "receipt": r, "error": e.to_string() }));
        }
    }
    ok(json!({ "acked": receipts.len() - errors.len(), "errors": errors }))
}

#[derive(Deserialize)]
struct NackBody {
    receipt: String,
    delay: Option<Dur>,
    error: Option<String>,
    /// Don't retry: dead-letter it now.
    #[serde(default)]
    dead: bool,
}

async fn nack(State(t): S, Path(name): Path<String>, Json(b): Json<NackBody>) -> ApiResult {
    let m = t.nack(&name, &b.receipt, b.delay.map(|d| d.0), b.error, b.dead).await?;
    ok(json!({ "state": m.state, "due_at": m.due_at, "attempts": m.attempts }))
}

#[derive(Deserialize)]
struct ExtendBody {
    receipt: String,
    visibility: Dur,
}

async fn extend(State(t): S, Path(name): Path<String>, Json(b): Json<ExtendBody>) -> ApiResult {
    ok(json!({ "lease_until": t.extend_lease(&name, &b.receipt, b.visibility.0).await? }))
}

// --- topics ---

async fn list_topics(State(t): S) -> ApiResult {
    ok(t.topics().await?)
}

async fn get_topic(State(t): S, Path(name): Path<String>) -> ApiResult {
    ok(t.topic(&name).await?)
}

#[derive(Deserialize)]
struct Endpoints {
    #[serde(default)]
    endpoints: Vec<EndpointIn>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EndpointIn {
    Url(String),
    Full(TopicEndpoint),
}

impl From<EndpointIn> for TopicEndpoint {
    fn from(e: EndpointIn) -> Self {
        match e {
            EndpointIn::Url(url) => TopicEndpoint { url, name: None },
            EndpointIn::Full(e) => e,
        }
    }
}

async fn put_topic(State(t): S, Path(name): Path<String>, Json(b): Json<Endpoints>) -> ApiResult {
    ok(t.put_topic(&name, b.endpoints.into_iter().map(Into::into).collect()).await?)
}

async fn add_endpoints(State(t): S, Path(name): Path<String>, Json(b): Json<Endpoints>) -> ApiResult {
    ok(t.add_topic_endpoints(&name, b.endpoints.into_iter().map(Into::into).collect()).await?)
}

#[derive(Deserialize)]
struct RemoveEndpoints {
    #[serde(default)]
    urls: Vec<String>,
}

async fn remove_endpoints(State(t): S, Path(name): Path<String>, Json(b): Json<RemoveEndpoints>) -> ApiResult {
    ok(t.remove_topic_endpoints(&name, &b.urls).await?)
}

async fn delete_topic(State(t): S, Path(name): Path<String>) -> Result<StatusCode, TaskError> {
    t.delete_topic(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- schedules ---

fn schedule_view(s: &Schedule) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    if let Ok(spec) = crate::schedule::parse(&s.cron, s.tz.as_deref())
        && !s.paused
    {
        v["upcoming"] = json!(spec.upcoming(now_ms(), s.created_at, 5));
    }
    v
}

async fn list_schedules(State(t): S) -> ApiResult {
    ok(t.schedules().await?.iter().map(schedule_view).collect::<Vec<_>>())
}

async fn create_schedule(State(t): S, Json(req): Json<ScheduleRequest>) -> Result<(StatusCode, Json<Value>), TaskError> {
    let s = t.create_schedule(req).await?;
    Ok((StatusCode::CREATED, Json(schedule_view(&s))))
}

async fn get_schedule(State(t): S, Path(id): Path<String>) -> ApiResult {
    Ok(Json(schedule_view(&t.schedule(&id).await?.value)))
}

async fn update_schedule(State(t): S, Path(id): Path<String>, Json(u): Json<ScheduleUpdate>) -> ApiResult {
    Ok(Json(schedule_view(&t.update_schedule(&id, u).await?)))
}

async fn delete_schedule(State(t): S, Path(id): Path<String>) -> Result<StatusCode, TaskError> {
    t.delete_schedule(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn pause_schedule(State(t): S, Path(id): Path<String>) -> ApiResult {
    Ok(Json(schedule_view(&t.pause_schedule(&id, true).await?)))
}

async fn resume_schedule(State(t): S, Path(id): Path<String>) -> ApiResult {
    Ok(Json(schedule_view(&t.pause_schedule(&id, false).await?)))
}

async fn trigger_schedule(State(t): S, Path(id): Path<String>) -> ApiResult {
    ok(json!({ "fired": t.trigger_schedule(&id).await? }))
}

// --- events ---

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    Many(Vec<EventIn>),
    One(EventIn),
}

async fn send_events(State(t): S, Json(b): Json<OneOrMany>) -> Result<(StatusCode, Json<Value>), TaskError> {
    let events = match b {
        OneOrMany::Many(v) => v,
        OneOrMany::One(e) => vec![e],
    };
    let ids = t.send_events(events).await?;
    Ok((StatusCode::CREATED, Json(json!({ "ids": ids }))))
}

/// Webhook ingestion: any JSON body becomes the data of event `<name>`.
async fn ingest(State(t): S, Path(name): Path<String>, headers: HeaderMap, body: Bytes) -> Result<(StatusCode, Json<Value>), TaskError> {
    let data: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    let id = header_str(&headers, "roll-event-id").or(header_str(&headers, "idempotency-key")).map(String::from);
    let ids = t.send_events(vec![EventIn { name, data, id, ts: None, user: None }]).await?;
    Ok((StatusCode::CREATED, Json(json!({ "ids": ids }))))
}

#[derive(Deserialize, Default)]
struct EventParams {
    name: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn list_events(State(t): S, Query(p): Query<EventParams>) -> ApiResult {
    ok(t.store.event_list(&EventQuery { name: p.name, cursor: p.cursor, limit: p.limit.unwrap_or(50) }).await.map_err(TaskError::from)?)
}

async fn get_event(State(t): S, Path(id): Path<String>) -> ApiResult {
    ok(t.event(&id).await?)
}

// --- apps, functions, runs ---

async fn list_apps(State(t): S) -> ApiResult {
    ok(t.apps().await)
}

async fn sync_app(State(t): S, Json(req): Json<SyncRequest>) -> ApiResult {
    ok(t.sync_app(req).await?)
}

async fn delete_app(State(t): S, Path(id): Path<String>) -> Result<StatusCode, TaskError> {
    t.delete_app(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn function_view(app: &App, f: &FunctionDef) -> Value {
    let mut v = serde_json::to_value(f).unwrap_or_default();
    v["app_id"] = json!(app.id);
    v["url"] = json!(app.url);
    v
}

async fn list_functions(State(t): S) -> ApiResult {
    let cache = t.cache.read().unwrap();
    ok(cache.registry.functions().map(|(a, f)| function_view(a, f)).collect::<Vec<_>>())
}

async fn get_function(State(t): S, Path(id): Path<String>) -> ApiResult {
    let v = {
        let cache = t.cache.read().unwrap();
        let (a, f) = cache.registry.function(&id).ok_or_else(|| TaskError::NotFound(format!("function {id}")))?;
        function_view(a, f)
    };
    let mut v = v;
    if let Ok(q) = t.queue(&format!("fn:{id}")).await {
        v["stats"] = t.queue_stats(&q.value).await?;
    }
    Ok(Json(v))
}

#[derive(Deserialize, Default)]
struct InvokeBody {
    #[serde(default)]
    data: Value,
    /// Deduplication id.
    id: Option<String>,
    user: Option<Value>,
    /// Wait for the result up to this long.
    wait: Option<Dur>,
}

async fn invoke_function(State(t): S, Path(id): Path<String>, body: Option<Json<InvokeBody>>) -> ApiResult {
    let b = body.map(|b| b.0).unwrap_or_default();
    let run = t
        .invoke_function(&id, EventIn { name: crate::workflow::EVENT_INVOKED.into(), data: b.data, id: b.id, ts: None, user: b.user })
        .await?;
    match b.wait {
        Some(w) => ok(t.wait_run(&run, w.0).await?),
        None => ok(json!({ "run_id": run })),
    }
}

async fn pause_function(State(t): S, Path(id): Path<String>) -> ApiResult {
    ok(t.set_function_paused(&id, true).await?)
}

async fn resume_function(State(t): S, Path(id): Path<String>) -> ApiResult {
    ok(t.set_function_paused(&id, false).await?)
}

#[derive(Deserialize, Default)]
struct CancelRuns {
    #[serde(rename = "if")]
    if_: Option<String>,
}

async fn cancel_function_runs(State(t): S, Path(id): Path<String>, body: Option<Json<CancelRuns>>) -> ApiResult {
    let b = body.map(|b| b.0).unwrap_or_default();
    ok(json!({ "canceled": t.cancel_runs(&id, b.if_.as_deref(), "canceled").await? }))
}

#[derive(Deserialize, Default)]
struct RunParams {
    function: Option<String>,
    status: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn list_runs(State(t): S, Query(p): Query<RunParams>) -> ApiResult {
    let status = match p.status.as_deref() {
        None | Some("") | Some("all") => None,
        Some(s) => Some(RunStatus::parse(s).ok_or_else(|| TaskError::Invalid(format!("unknown status {s:?}")))?),
    };
    ok(t.runs(&RunQuery { function_id: p.function, status, cursor: p.cursor, limit: p.limit.unwrap_or(50) }).await?)
}

async fn get_run(State(t): S, Path(id): Path<String>) -> ApiResult {
    let run = t.run(&id).await?;
    let steps = t.run_steps(&id).await?;
    let calls = t.messages(&MessageQuery { run_id: Some(id.clone()), limit: 100, ..Default::default() }).await?;
    let mut v = serde_json::to_value(&run).map_err(anyhow::Error::from)?;
    v["steps"] = json!(steps);
    v["messages"] = json!(calls.items);
    Ok(Json(v))
}

async fn cancel_run(State(t): S, Path(id): Path<String>) -> ApiResult {
    ok(t.cancel_run(&id, "canceled").await?)
}

async fn rerun(State(t): S, Path(id): Path<String>) -> ApiResult {
    ok(t.rerun(&id).await?)
}

#[derive(Deserialize, Default)]
struct WaitParams {
    timeout: Option<String>,
}

async fn wait_run(State(t): S, Path(id): Path<String>, Query(p): Query<WaitParams>) -> ApiResult {
    let timeout = p.timeout.as_deref().map(crate::schedule::parse_duration_ms).transpose().map_err(TaskError::Invalid)?.unwrap_or(30_000);
    ok(t.wait_run(&id, timeout).await?)
}

// --- keys, usage ---

async fn signing_keys(State(t): S) -> ApiResult {
    ok(t.signing_keys().await?)
}

async fn rotate_keys(State(t): S) -> ApiResult {
    ok(t.rotate_signing_keys().await?)
}

async fn list_api_keys(State(t): S) -> ApiResult {
    ok(t.api_keys().await?)
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
}

async fn create_api_key(State(t): S, Json(b): Json<NewKey>) -> Result<(StatusCode, Json<Value>), TaskError> {
    let (key, token) = t.create_api_key(&b.name).await?;
    let mut v = serde_json::to_value(key).map_err(anyhow::Error::from)?;
    v["token"] = json!(token);
    Ok((StatusCode::CREATED, Json(v)))
}

async fn delete_api_key(State(t): S, Path(id): Path<String>) -> Result<StatusCode, TaskError> {
    t.delete_api_key(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct UsageParams {
    days: Option<i64>,
}

async fn usage(State(t): S, Query(p): Query<UsageParams>) -> ApiResult {
    ok(t.usage_report(p.days.unwrap_or(30)).await?)
}

async fn stats(State(t): S) -> ApiResult {
    let nodes = t.cache.read().unwrap().nodes.clone();
    let mut queues = Vec::new();
    for q in t.queues().await? {
        let s = t.queue_stats(&q).await?;
        queues.push(json!({ "name": q.name, "paused": q.paused, "stats": s }));
    }
    let failed = t.messages(&MessageQuery { state: Some(MsgState::Failed), user_only: true, limit: 1000, ..Default::default() }).await?;
    ok(json!({
        "nodes": nodes,
        "queues": queues,
        "dlq": failed.items.len(),
        "dlq_more": failed.cursor.is_some(),
        "schedules": t.schedules().await?.len(),
        "functions": t.cache.read().unwrap().registry.functions().count(),
    }))
}

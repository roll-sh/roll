//! The roll API for queues (`/v1/queues/...`), used by the dashboard and `roll queues`.
//! Routes and types are specified in `docs/queues/contracts.md`.

use crate::QueuesControl;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use mq_core::auth::{Permission, User};
use mq_core::exchange::{Dest, DestKind};
use mq_core::message::Properties;
use mq_core::policy::{self, Policy, Target};
use mq_core::queue::{ConfirmTicket, Outcome, QueueConfig, QueueRef};
use mq_core::vhost::{DeclareQueue, VHost, VHostInfo};
use mq_core::{AmqpError, FieldTable, FieldValue, Message};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

type AppState = Arc<QueuesControl>;
type ApiResult<T = Json<Value>> = Result<T, ApiError>;

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        ApiError(status, msg.into())
    }
    fn bad(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<AmqpError> for ApiError {
    fn from(e: AmqpError) -> Self {
        use mq_core::error::*;
        let status = match e.code {
            NOT_FOUND => StatusCode::NOT_FOUND,
            ACCESS_REFUSED => StatusCode::FORBIDDEN,
            RESOURCE_LOCKED => StatusCode::CONFLICT,
            PRECONDITION_FAILED | COMMAND_INVALID | SYNTAX_ERROR | INVALID_PATH | NOT_ALLOWED => StatusCode::BAD_REQUEST,
            NOT_IMPLEMENTED => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.reply_text())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<AmqpError>() {
            Ok(a) => a.into(),
            Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }
}

pub fn router(c: Arc<QueuesControl>) -> Router {
    Router::new()
        .route("/v1/queues/health", get(health))
        .route("/v1/queues/overview", get(overview))
        .route("/v1/queues/rates", get(rates))
        .route("/v1/queues/usage", get(usage))
        .route("/v1/queues/vhosts", get(list_vhosts).post(create_vhost))
        .route("/v1/queues/vhosts/{vhost}", delete(delete_vhost))
        .route("/v1/queues/vhosts/{vhost}/credentials", post(issue_credentials))
        .route("/v1/queues/vhosts/{vhost}/suspend", post(suspend_vhost))
        .route("/v1/queues/vhosts/{vhost}/wake", post(wake_vhost))
        .route("/v1/queues/vhosts/{vhost}/queues", get(list_queues).post(declare_queue))
        .route("/v1/queues/vhosts/{vhost}/queues/{queue}", get(get_queue).delete(delete_queue))
        .route("/v1/queues/vhosts/{vhost}/queues/{queue}/purge", post(purge_queue))
        .route("/v1/queues/vhosts/{vhost}/queues/{queue}/messages", get(browse))
        .route("/v1/queues/vhosts/{vhost}/queues/{queue}/replay", post(replay))
        .route("/v1/queues/vhosts/{vhost}/queues/{queue}/rates", get(queue_rates))
        .route("/v1/queues/vhosts/{vhost}/exchanges", get(list_exchanges).post(declare_exchange))
        .route("/v1/queues/vhosts/{vhost}/exchanges/{exchange}", delete(delete_exchange))
        .route("/v1/queues/vhosts/{vhost}/bindings", get(list_bindings).post(bind).delete(unbind))
        .route("/v1/queues/vhosts/{vhost}/publish", post(publish))
        .route("/v1/queues/connections", get(list_connections))
        .route("/v1/queues/connections/{id}", delete(close_connection))
        .route("/v1/queues/channels", get(list_channels))
        .route("/v1/queues/policies", get(list_policies))
        .route("/v1/queues/policies/{vhost}/{name}", put(put_policy).delete(delete_policy))
        .route("/v1/queues/users", get(list_users))
        .route("/v1/queues/users/{name}", put(put_user).delete(delete_user))
        .route("/v1/queues/permissions/{vhost}/{user}", put(put_permission).delete(delete_permission))
        .route("/v1/queues/definitions", get(export_definitions).post(import_definitions))
        .route("/v1/queues/migrate", get(migrate_status).post(migrate).delete(stop_migration))
        .layer(axum::middleware::from_fn_with_state(c.clone(), awake))
        .with_state(c)
        .layer(tower_http::cors::CorsLayer::permissive())
}

/// Dashboard and API requests (publishing a test message, replaying a DLQ) keep the broker out
/// of quiet mode for a little while.
async fn awake(State(c): State<AppState>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    c.broker.note_request();
    next.run(req).await
}

// ── helpers ──

/// Percent-encodes everything but RFC 3986 unreserved characters, so `my-app` stays readable.
const URL_PART: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, URL_PART).to_string()
}

async fn vhost(c: &QueuesControl, name: &str) -> ApiResult<Arc<VHost>> {
    c.broker.open_vhost(name).await.ok_or_else(|| ApiError::not_found(format!("no vhost '{name}'")))
}

fn queue(vh: &VHost, name: &str) -> ApiResult<QueueRef> {
    vh.queue(name).ok_or_else(|| ApiError::not_found(format!("no queue '{name}' in vhost '{}'", vh.name)))
}

fn table(v: &Option<Value>) -> FieldTable {
    v.as_ref().filter(|v| v.is_object()).map(FieldTable::from_json).unwrap_or_default()
}

fn amqp_url(c: &QueuesControl, vhost: &str) -> String {
    let base = c.endpoints.amqp.clone().unwrap_or_else(|| "amqp://127.0.0.1:5672".into());
    let cfg = &c.broker.config;
    let creds = if cfg.default_pass == "guest" { format!("{}:{}@", enc(&cfg.default_user), enc(&cfg.default_pass)) } else { format!("{}@", enc(&cfg.default_user)) };
    let rest = base.trim_start_matches("amqp://");
    format!("amqp://{creds}{rest}/{}", enc(vhost))
}

fn policy_name(c: &QueuesControl, vhost: &str, target: Target, name: &str) -> Option<String> {
    let policies = c.broker.policies.read();
    policy::pick(&policies, vhost, target, name, &c.broker.regex).map(|p| p.name.clone())
}

fn resolved(c: &QueuesControl, q: &QueueRef) -> Option<QueueConfig> {
    let b = &c.broker;
    let policies = b.policies.read();
    let ops = b.operator_policies.read();
    let t = Target::Queue(q.kind);
    let p = policy::pick(&policies, &q.meta.vhost, t, &q.meta.name, &b.regex).map(|p| (p.name.as_str(), &p.definition));
    let o = policy::pick(&ops, &q.meta.vhost, t, &q.meta.name, &b.regex).map(|p| (p.name.as_str(), &p.definition));
    QueueConfig::resolve(q.kind, &q.meta.arguments, &q.meta.name, &q.meta.vhost, p, o).ok()
}

/// Queues something dead-letters into: bound to a dead-letter exchange, or named by a
/// dead-letter routing key on the default exchange.
fn dead_letter_queues(c: &QueuesControl, vh: &VHost) -> HashSet<String> {
    let mut dlx = HashSet::new();
    let mut direct = HashSet::new();
    for q in vh.queues() {
        if let Some(cfg) = resolved(c, &q)
            && let Some(x) = cfg.dead_letter_exchange
        {
            if x.is_empty() {
                direct.insert(cfg.dead_letter_routing_key.unwrap_or_else(|| q.meta.name.clone()));
            } else {
                dlx.insert(x);
            }
        }
    }
    let mut out = direct;
    let mut frontier: Vec<String> = dlx.iter().cloned().collect();
    let mut seen = dlx;
    while let Some(x) = frontier.pop() {
        let Some(ex) = vh.exchange(&x) else { continue };
        for b in ex.bindings() {
            match b.dest.kind {
                DestKind::Queue => {
                    out.insert(b.dest.name);
                }
                DestKind::Exchange => {
                    if seen.insert(b.dest.name.clone()) {
                        frontier.push(b.dest.name);
                    }
                }
            }
        }
    }
    out
}

fn queue_json(c: &QueuesControl, q: &QueueRef, dlqs: &HashSet<String>) -> Value {
    let s = &q.shared;
    let r = c.sampler.queue(&q.meta.vhost, &q.meta.name);
    let cfg = resolved(c, q);
    let ready = s.ready.load(Ordering::Relaxed);
    let unacked = s.unacked.load(Ordering::Relaxed);
    let mut v = json!({
        "name": q.meta.name,
        "vhost": q.meta.vhost,
        "type": q.kind.name(),
        "durable": q.meta.durable,
        "auto_delete": q.meta.auto_delete,
        "exclusive": q.meta.exclusive(),
        "arguments": q.meta.arguments.to_json(),
        "policy": policy_name(c, &q.meta.vhost, Target::Queue(q.kind), &q.meta.name),
        "state": "running",
        "messages_ready": ready,
        "messages_unacked": unacked,
        "messages": ready + unacked,
        "consumers": s.consumers.load(Ordering::Relaxed),
        "bytes": s.ready_bytes.load(Ordering::Relaxed) + s.unacked_bytes.load(Ordering::Relaxed),
        "stored_bytes": s.log_bytes.load(Ordering::Relaxed),
        "rates": r,
        "dead_letter_exchange": cfg.as_ref().and_then(|c| c.dead_letter_exchange.clone()),
        "dead_lettered": s.dead_lettered.load(Ordering::Relaxed),
        "is_dlq": dlqs.contains(&q.meta.name),
    });
    if q.kind == mq_core::queue::QueueType::Stream {
        v["first_offset"] = s.first_offset.load(Ordering::Relaxed).into();
        v["next_offset"] = s.next_offset.load(Ordering::Relaxed).into();
    }
    v
}

fn binding_json(b: &mq_core::exchange::Binding) -> Value {
    json!({
        "source": b.source,
        "destination": b.dest.name,
        "destination_type": b.dest.kind,
        "routing_key": b.routing_key,
        "arguments": b.args.to_json(),
    })
}

fn x_death(msg: &Message) -> Option<Value> {
    let deaths = msg.props.header("x-death")?.as_array()?;
    let out: Vec<Value> = deaths
        .iter()
        .filter_map(|d| match d {
            FieldValue::Table(t) => Some(json!({
                "queue": t.get("queue").and_then(|v| v.as_str()).unwrap_or_default(),
                "reason": t.get("reason").and_then(|v| v.as_str()).unwrap_or_default(),
                "count": t.get("count").and_then(|v| v.as_i64()).unwrap_or(0),
                "exchange": t.get("exchange").and_then(|v| v.as_str()).unwrap_or_default(),
                "routing_keys": t.get("routing-keys").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default(),
            })),
            _ => None,
        })
        .collect();
    Some(Value::Array(out))
}

// ── overview, rates, usage ──

async fn health(State(c): State<AppState>) -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION"), "node": c.broker.config.node_name, "endpoints": c.endpoints }))
}

async fn overview(State(c): State<AppState>) -> Json<Value> {
    let b = &c.broker;
    let (mut exchanges, mut queues, mut consumers, mut ready, mut unacked, mut stored) = (0, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut sleeping = 0;
    let infos = b.vhost_infos();
    for (info, vh) in &infos {
        match vh {
            Some(vh) => {
                exchanges += vh.exchanges().len();
                for q in vh.queues() {
                    queues += 1;
                    consumers += q.shared.consumers.load(Ordering::Relaxed);
                    ready += q.shared.ready.load(Ordering::Relaxed);
                    unacked += q.shared.unacked.load(Ordering::Relaxed);
                }
                stored += vh.bytes_stored();
            }
            None => {
                // Asleep: its data is kept, so report what it held when last loaded.
                let t = c.sampler.totals(&info.name);
                sleeping += 1;
                queues += t.queues;
                ready += t.messages_ready;
                unacked += t.messages_unacked;
                stored += t.bytes;
            }
        }
    }
    let r = c.sampler.latest();
    Json(json!({
        "version": mq_core::broker::RABBITMQ_VERSION,
        "roll_version": env!("CARGO_PKG_VERSION"),
        "node": b.config.node_name,
        "cluster_name": b.config.cluster_name,
        "uptime_s": mq_core::now_ms().saturating_sub(b.started_ms) / 1000,
        "endpoints": c.endpoints,
        "totals": {
            "vhosts": infos.len(),
            "sleeping_vhosts": sleeping,
            "connections": b.connections().len(),
            "channels": b.channels().len(),
            "exchanges": exchanges,
            "queues": queues,
            "consumers": consumers,
            "messages_ready": ready,
            "messages_unacked": unacked,
            "stored_bytes": stored,
        },
        "rates": { "publish": r.publish, "deliver": r.deliver, "ack": r.ack },
        "storage": {
            "backend": if c.config.storage_backend.is_empty() { b.logs.describe() } else { c.config.storage_backend.clone() },
            "metadata": if c.config.metadata_backend.is_empty() { b.meta.describe() } else { c.config.metadata_backend.clone() },
        },
    }))
}

#[derive(Deserialize)]
struct RatesQuery {
    seconds: Option<u64>,
}

async fn rates(State(c): State<AppState>, Query(q): Query<RatesQuery>) -> Json<Value> {
    let seconds = q.seconds.unwrap_or(300).clamp(5, 3600);
    Json(json!({ "step_s": crate::sampler::STEP_S, "points": c.sampler.points(seconds) }))
}

async fn queue_rates(State(c): State<AppState>, Path((vhost, queue)): Path<(String, String)>, Query(q): Query<RatesQuery>) -> Json<Value> {
    let seconds = q.seconds.unwrap_or(300).clamp(5, 300);
    Json(json!({ "step_s": crate::sampler::STEP_S, "points": c.sampler.queue_history(&vhost, &queue, seconds) }))
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<u64>,
    vhost: Option<String>,
}

async fn usage(State(c): State<AppState>, Query(q): Query<UsageQuery>) -> ApiResult {
    let days = q.days.unwrap_or(30).clamp(1, 400);
    let bytes: u64 = c
        .broker
        .vhost_infos()
        .iter()
        .filter(|(i, _)| q.vhost.as_deref().is_none_or(|v| v == i.name))
        .map(|(i, vh)| vh.as_ref().map(|v| v.bytes_stored()).unwrap_or_else(|| c.sampler.stored_bytes(&i.name)))
        .sum();
    let live = c.sampler.latest();
    let report = c.meter.report(days, q.vhost.as_deref(), bytes, live.publish + live.deliver, live.connections)?;
    Ok(Json(report))
}

// ── vhosts ──

async fn list_vhosts(State(c): State<AppState>) -> Json<Value> {
    let b = &c.broker;
    let out: Vec<Value> = b
        .vhost_infos()
        .into_iter()
        .map(|(info, vh)| {
            let (ready, unacked, queues, stored) = match &vh {
                Some(vh) => {
                    let (r, u) = vh.messages();
                    (r, u, vh.queue_count() as u64, vh.bytes_stored())
                }
                // Asleep: last known totals (its data is kept on disk).
                None => {
                    let t = c.sampler.totals(&info.name);
                    (t.messages_ready, t.messages_unacked, t.queues, t.bytes)
                }
            };
            let r = c.sampler.vhost(&info.name);
            json!({
                "name": info.name,
                "description": info.description,
                "default_queue_type": info.default_queue_type.clone().unwrap_or_else(|| b.config.default_queue_type.clone()),
                "state": if vh.is_some() { "running" } else { "sleeping" },
                "sleeping": vh.is_none(),
                "connections": b.vhost_connections(&info.name),
                "queues": queues,
                "messages_ready": ready,
                "messages_unacked": unacked,
                "stored_bytes": stored,
                "rates": { "publish": r.publish, "deliver": r.deliver },
                "limits": info.limits,
                "amqp_url": amqp_url(&c, &info.name),
            })
        })
        .collect();
    Json(Value::Array(out))
}

#[derive(Deserialize)]
struct CreateVHost {
    name: String,
    #[serde(default)]
    description: String,
    default_queue_type: Option<String>,
    /// Also create a user that can only reach this vhost and return its connection URLs.
    #[serde(default)]
    credentials: bool,
    /// Name for that user (default: the vhost name). Implies `credentials`.
    user: Option<String>,
}

/// A password for a generated user: 128 random bits as hex, so it is safe in every URL.
fn generate_password() -> String {
    format!("{:016x}{:016x}", mq_core::rand_u64(), mq_core::rand_u64())
}

/// `scheme://host:port` with `user:pass@` put in front of the host.
fn with_creds(base: &str, user: &str, pass: &str) -> String {
    let (scheme, rest) = base.split_once("://").unwrap_or(("amqp", base));
    format!("{scheme}://{}:{}@{}", enc(user), enc(pass), rest.trim_end_matches('/'))
}

/// One connection URL per enabled protocol for `user` on `vhost`: the app pastes one in and
/// changes nothing else. MQTT has no vhost in its URL, so it goes in the username as
/// `vhost:user` (RabbitMQ's convention).
fn connection_urls(c: &QueuesControl, vhost: &str, user: &str, pass: &str) -> Value {
    let e = &c.endpoints;
    let vh = enc(vhost);
    let with_vhost = |base: &Option<String>| base.as_deref().map(|b| format!("{}/{vh}", with_creds(b, user, pass)));
    json!({
        "amqp": with_vhost(&e.amqp),
        "amqps": with_vhost(&e.amqps),
        "stream": with_vhost(&e.stream),
        "stomp": with_vhost(&e.stomp),
        "mqtt": e.mqtt.as_deref().map(|b| with_creds(b, &format!("{vhost}:{user}"), pass)),
        "http": e.http.as_deref().map(|b| format!("{}/v1/vhosts/{vh}", with_creds(b, user, pass))),
        "management": e.management.as_deref().map(|b| with_creds(b, user, pass)),
    })
}

/// Creates `user` (or gives it a new password) with full rights on `vhost` and none
/// elsewhere, and returns its credentials and connection URLs. The password is only ever
/// shown here.
async fn provision_user(c: &QueuesControl, vhost: &str, user: &str) -> ApiResult<Value> {
    let user = user.trim();
    if user.is_empty() {
        return Err(ApiError::bad("user name is required"));
    }
    let existing = c.broker.user(user);
    if let Some(u) = &existing {
        // Never hand out a new password for a user that can reach other vhosts.
        let foreign = c.broker.permissions().into_iter().any(|(v, u, _)| u == user && v != vhost);
        if u.is_admin() || foreign {
            return Err(ApiError::new(StatusCode::CONFLICT, format!("user '{user}' already exists with access outside vhost '{vhost}'")));
        }
    }
    let password = generate_password();
    let mut u = User::new(user, &password, vec!["management".into()]);
    if let Some(e) = existing {
        u.limits = e.limits;
    }
    c.broker.put_user(u).await?;
    c.broker.set_permission(vhost, user, Permission { configure: ".*".into(), write: ".*".into(), read: ".*".into() }).await?;
    Ok(json!({
        "user": user,
        "password": password,
        "urls": connection_urls(c, vhost, user, &password),
    }))
}

#[derive(Deserialize, Default)]
struct CredentialsBody {
    user: Option<String>,
}

/// `POST /v1/queues/vhosts/{vhost}/credentials`: issue (or rotate) the vhost's own user.
async fn issue_credentials(State(c): State<AppState>, Path(name): Path<String>, body: Option<Json<CredentialsBody>>) -> ApiResult {
    if !c.broker.vhost_exists(&name) {
        return Err(ApiError::not_found(format!("no vhost '{name}'")));
    }
    let user = body.and_then(|b| b.0.user).unwrap_or_else(|| name.clone());
    Ok(Json(provision_user(&c, &name, &user).await?))
}

async fn create_vhost(State(c): State<AppState>, Json(body): Json<CreateVHost>) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad("vhost name is required"));
    }
    if c.broker.vhost_exists(&name) {
        return Err(ApiError::new(StatusCode::CONFLICT, format!("vhost '{name}' already exists")));
    }
    if let Some(t) = &body.default_queue_type
        && mq_core::queue::QueueType::parse(t).is_none()
    {
        return Err(ApiError::bad(format!("unknown queue type '{t}'")));
    }
    let tenant = body.user.clone().or_else(|| body.credentials.then(|| name.clone()));
    if let Some(u) = &tenant
        && c.broker.user(u.trim()).is_some()
    {
        return Err(ApiError::new(StatusCode::CONFLICT, format!("user '{}' already exists", u.trim())));
    }
    let info = VHostInfo { name: name.clone(), description: body.description, default_queue_type: body.default_queue_type, ..Default::default() };
    c.broker.create_vhost(info).await?;
    let user = c.broker.config.default_user.clone();
    if c.broker.user(&user).is_some() {
        c.broker.set_permission(&name, &user, Permission { configure: ".*".into(), write: ".*".into(), read: ".*".into() }).await?;
    }
    let credentials = match &tenant {
        Some(u) => Some(provision_user(&c, &name, u).await?),
        None => None,
    };
    let all = list_vhosts(State(c.clone())).await.0;
    let mut v = all.as_array().and_then(|a| a.iter().find(|v| v["name"] == name.as_str()).cloned()).unwrap_or(Value::Null);
    if let (Some(cr), Some(o)) = (credentials, v.as_object_mut()) {
        o.insert("credentials".into(), cr);
    }
    Ok((StatusCode::CREATED, Json(v)))
}

async fn delete_vhost(State(c): State<AppState>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    if !c.broker.vhost_exists(&name) {
        return Err(ApiError::not_found(format!("no vhost '{name}'")));
    }
    c.broker.delete_vhost(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn suspend_vhost(State(c): State<AppState>, Path(name): Path<String>) -> ApiResult {
    if c.broker.is_asleep(&name) {
        return Ok(Json(json!({ "state": "sleeping" })));
    }
    let Some(vh) = c.broker.vhost(&name) else {
        return Err(ApiError::not_found(format!("no vhost '{name}'")));
    };
    c.sampler.remember(&vh);
    drop(vh);
    if !c.broker.suspend_vhost(&name).await {
        return Err(ApiError::new(StatusCode::CONFLICT, "vhost is busy (connections, consumers or timers); it can't sleep now"));
    }
    Ok(Json(json!({ "state": "sleeping" })))
}

async fn wake_vhost(State(c): State<AppState>, Path(name): Path<String>) -> ApiResult {
    let t = std::time::Instant::now();
    vhost(&c, &name).await?;
    Ok(Json(json!({ "state": "running", "took_ms": t.elapsed().as_millis() as u64 })))
}

// ── queues ──

async fn list_queues(State(c): State<AppState>, Path(vh): Path<String>) -> ApiResult {
    let vh = vhost(&c, &vh).await?;
    let dlqs = dead_letter_queues(&c, &vh);
    let mut qs = vh.queues();
    qs.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    Ok(Json(Value::Array(qs.iter().map(|q| queue_json(&c, q, &dlqs)).collect())))
}

#[derive(Deserialize)]
struct DeclareQueueBody {
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    durable: Option<bool>,
    #[serde(default)]
    auto_delete: bool,
    arguments: Option<Value>,
}

async fn declare_queue(State(c): State<AppState>, Path(vh): Path<String>, Json(body): Json<DeclareQueueBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    let vh = vhost(&c, &vh).await?;
    let mut args = table(&body.arguments);
    if let Some(t) = body.kind.filter(|t| !t.is_empty()) {
        args.insert("x-queue-type", FieldValue::str(t));
    }
    let ok = vh
        .declare_queue(DeclareQueue { name: body.name, durable: body.durable.unwrap_or(true), exclusive_owner: None, auto_delete: body.auto_delete, args, passive: false })
        .await?;
    let dlqs = dead_letter_queues(&c, &vh);
    Ok((StatusCode::CREATED, Json(queue_json(&c, &ok.queue, &dlqs))))
}

async fn get_queue(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>) -> ApiResult {
    let vh = vhost(&c, &vh).await?;
    let q = queue(&vh, &name)?;
    let dlqs = dead_letter_queues(&c, &vh);
    let mut v = queue_json(&c, &q, &dlqs);
    let channels: HashMap<u64, u32> = c.broker.channels().iter().map(|ch| (ch.id, ch.prefetch_count.load(Ordering::Relaxed))).collect();
    let consumers: Vec<Value> = q
        .consumers()
        .await
        .into_iter()
        .map(|ci| {
            json!({
                "consumer_tag": ci.consumer_tag,
                "channel": ci.channel,
                "connection": ci.connection,
                "ack_required": ci.ack_required,
                "exclusive": ci.exclusive,
                "priority": ci.priority,
                "active": ci.active,
                "prefetch": channels.get(&ci.channel).copied().unwrap_or(0),
                "arguments": ci.arguments.to_json(),
            })
        })
        .collect();
    let bindings: Vec<Value> = vh.bindings().iter().filter(|b| b.dest.kind == DestKind::Queue && b.dest.name == name).map(binding_json).collect();
    // `consumers` stays the count (as in the list view); the list is `consumer_details`, as in
    // RabbitMQ's management API.
    v["consumer_details"] = Value::Array(consumers);
    v["bindings"] = Value::Array(bindings);
    if let Some(cfg) = resolved(&c, &q) {
        v["effective"] = json!({
            "message_ttl": cfg.message_ttl, "expires": cfg.expires, "max_length": cfg.max_length,
            "max_length_bytes": cfg.max_length_bytes, "overflow": format!("{:?}", cfg.overflow).to_lowercase(),
            "dead_letter_routing_key": cfg.dead_letter_routing_key, "max_priority": cfg.max_priority,
            "single_active_consumer": cfg.single_active_consumer, "delivery_limit": cfg.delivery_limit,
        });
    }
    Ok(Json(v))
}

async fn delete_queue(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let vh = vhost(&c, &vh).await?;
    queue(&vh, &name)?;
    vh.delete_queue(&name, false, false, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn purge_queue(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>) -> ApiResult {
    let vh = vhost(&c, &vh).await?;
    let purged = queue(&vh, &name)?.purge().await?;
    Ok(Json(json!({ "purged": purged })))
}

#[derive(Deserialize)]
struct BrowseQuery {
    offset: Option<usize>,
    count: Option<usize>,
}

async fn browse(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>, Query(q): Query<BrowseQuery>) -> ApiResult {
    let vh = vhost(&c, &vh).await?;
    let queue = queue(&vh, &name)?;
    let msgs = queue.peek(q.offset.unwrap_or(0), q.count.unwrap_or(50).clamp(1, 500)).await?;
    let out: Vec<Value> = msgs
        .iter()
        .map(|p| {
            let m = &p.msg;
            let (payload, encoding) = match std::str::from_utf8(&m.body) {
                Ok(s) => (s.to_string(), "string"),
                Err(_) => (base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &m.body), "base64"),
            };
            let mut v = json!({
                "seq": p.seq,
                "exchange": m.exchange,
                "routing_key": m.routing_key,
                "redelivered": p.redelivered,
                "delivery_count": p.delivery_count,
                "properties": m.props.to_json(),
                "payload": payload,
                "payload_encoding": encoding,
                "payload_bytes": m.body.len(),
                "received_ms": m.received_ms,
            });
            if let Some(d) = x_death(m) {
                v["x_death"] = d;
            }
            v
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize, Default)]
struct ReplayBody {
    count: Option<usize>,
    target_exchange: Option<String>,
    target_routing_key: Option<String>,
}

/// Publish through a vhost and wait until every queue it reached has it durably.
async fn publish_confirmed(vh: &VHost, exchange: &str, msg: Message) -> ApiResult<usize> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let ticket = ConfirmTicket::new(move |ok| {
        let _ = tx.send(ok);
    });
    let out = vh.publish(exchange, msg, Some(ticket)).await?;
    if let Some(t) = out.confirm {
        t.settle(true);
    }
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(true)) => Ok(out.routed),
        Ok(_) => Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "message was not confirmed (a queue rejected it)")),
        Err(_) => Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "timed out waiting for the confirm")),
    }
}

/// DLQ replay: move dead-lettered messages back to where they were first published.
async fn replay(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>, body: Option<Json<ReplayBody>>) -> ApiResult {
    let body = body.map(|b| b.0).unwrap_or_default();
    let vh = vhost(&c, &vh).await?;
    let q = queue(&vh, &name)?;
    let max = body.count.unwrap_or(1000).clamp(1, 100_000);
    let (mut replayed, mut skipped) = (0usize, 0usize);
    while replayed + skipped < max {
        let batch = q.lease((max - replayed - skipped).min(100), 60_000).await?;
        if batch.is_empty() {
            break;
        }
        for l in batch {
            let m = &l.msg;
            let first = x_death(m).and_then(|d| d.as_array().and_then(|a| a.last().cloned()));
            let exchange = body.target_exchange.clone().or_else(|| first.as_ref().and_then(|d| d["exchange"].as_str().map(String::from)));
            let key = body
                .target_routing_key
                .clone()
                .or_else(|| first.as_ref().and_then(|d| d["routing_keys"].get(0).and_then(|k| k.as_str()).map(String::from)))
                .unwrap_or_else(|| m.routing_key.clone());
            let Some(exchange) = exchange else {
                skipped += 1;
                q.settle_lease(l.seq, l.delivery_count, Outcome::Requeue).await?;
                continue;
            };
            let out = Message::new(exchange.clone(), key, m.props.clone(), m.body.clone());
            match publish_confirmed(&vh, &exchange, out).await {
                Ok(routed) if routed > 0 => {
                    q.settle_lease(l.seq, l.delivery_count, Outcome::Ack).await?;
                    replayed += 1;
                }
                _ => {
                    q.settle_lease(l.seq, l.delivery_count, Outcome::Requeue).await?;
                    skipped += 1;
                }
            }
        }
        if skipped > 0 && replayed == 0 {
            break;
        }
    }
    Ok(Json(json!({ "replayed": replayed, "skipped": skipped })))
}

// ── exchanges and bindings ──

async fn list_exchanges(State(c): State<AppState>, Path(vh): Path<String>) -> ApiResult {
    let vh = vhost(&c, &vh).await?;
    let mut xs = vh.exchanges();
    xs.sort_by(|a, b| a.name.cmp(&b.name));
    let out: Vec<Value> = xs
        .iter()
        .map(|x| {
            let r = c.sampler.exchange(&vh.name, &x.name);
            json!({
                "name": x.name,
                "vhost": vh.name,
                "type": x.kind.name(),
                "durable": x.durable,
                "auto_delete": x.auto_delete,
                "internal": x.internal,
                "arguments": x.args.to_json(),
                "policy": policy_name(&c, &vh.name, Target::Exchange, &x.name),
                "bindings": if x.is_default() { vh.queue_count() } else { x.binding_count() },
                "rates": r,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
struct DeclareExchangeBody {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    durable: Option<bool>,
    #[serde(default)]
    auto_delete: bool,
    #[serde(default)]
    internal: bool,
    arguments: Option<Value>,
}

async fn declare_exchange(State(c): State<AppState>, Path(vh): Path<String>, Json(b): Json<DeclareExchangeBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    let vh = vhost(&c, &vh).await?;
    let x = vh.declare_exchange(&b.name, &b.kind, b.durable.unwrap_or(true), b.auto_delete, b.internal, table(&b.arguments), false).await?;
    Ok((StatusCode::CREATED, Json(json!({ "name": x.name, "type": x.kind.name(), "durable": x.durable }))))
}

async fn delete_exchange(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let vh = vhost(&c, &vh).await?;
    vh.delete_exchange(&name, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_bindings(State(c): State<AppState>, Path(vh): Path<String>) -> ApiResult {
    let vh = vhost(&c, &vh).await?;
    Ok(Json(Value::Array(vh.bindings().iter().map(binding_json).collect())))
}

#[derive(Deserialize)]
struct BindingBody {
    source: String,
    destination: String,
    #[serde(default = "queue_kind")]
    destination_type: String,
    #[serde(default)]
    routing_key: String,
    arguments: Option<Value>,
}

fn queue_kind() -> String {
    "queue".into()
}

fn dest(b: &BindingBody) -> ApiResult<Dest> {
    match b.destination_type.as_str() {
        "queue" | "q" => Ok(Dest::queue(&b.destination)),
        "exchange" | "e" => Ok(Dest::exchange(&b.destination)),
        t => Err(ApiError::bad(format!("destination_type must be 'queue' or 'exchange', not '{t}'"))),
    }
}

async fn bind(State(c): State<AppState>, Path(vh): Path<String>, Json(b): Json<BindingBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    let vh = vhost(&c, &vh).await?;
    let d = dest(&b)?;
    vh.bind(&b.source, d, &b.routing_key, table(&b.arguments), None).await?;
    Ok((StatusCode::CREATED, Json(json!({ "ok": true }))))
}

async fn unbind(State(c): State<AppState>, Path(vh): Path<String>, Json(b): Json<BindingBody>) -> ApiResult<StatusCode> {
    let vh = vhost(&c, &vh).await?;
    let d = dest(&b)?;
    vh.unbind(&b.source, d, &b.routing_key, table(&b.arguments), None).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PublishBody {
    #[serde(default)]
    exchange: String,
    #[serde(default)]
    routing_key: String,
    #[serde(default)]
    body: String,
    body_encoding: Option<String>,
    properties: Option<Value>,
}

async fn publish(State(c): State<AppState>, Path(vh): Path<String>, Json(b): Json<PublishBody>) -> ApiResult {
    let vh = vhost(&c, &vh).await?;
    let body = match b.body_encoding.as_deref() {
        Some("base64") => base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b.body.trim()).map_err(|e| ApiError::bad(format!("invalid base64 body: {e}")))?,
        None | Some("utf8") | Some("string") => b.body.into_bytes(),
        Some(e) => return Err(ApiError::bad(format!("unknown body_encoding '{e}'"))),
    };
    let props = b.properties.as_ref().map(Properties::from_json).unwrap_or_default();
    let exchange = if b.exchange == "amq.default" { String::new() } else { b.exchange };
    let msg = Message::new(exchange.clone(), b.routing_key, props, body.into());
    let routed = publish_confirmed(&vh, &exchange, msg).await?;
    Ok(Json(json!({ "routed": routed > 0, "queues": routed })))
}

// ── connections and channels ──

async fn list_connections(State(c): State<AppState>) -> Json<Value> {
    let out: Vec<Value> = c
        .broker
        .connections()
        .iter()
        .map(|ci| {
            let props = &ci.client_properties;
            let client = props.get("product").and_then(|p| p.as_str()).map(|p| match props.get("version").and_then(|v| v.as_str()) {
                Some(v) => format!("{p} {v}"),
                None => p.to_string(),
            });
            json!({
                "id": ci.id,
                "name": ci.name,
                "protocol": ci.protocol,
                "vhost": ci.vhost,
                "user": ci.user,
                "peer": ci.peer.to_string(),
                "tls": ci.tls,
                "channels": ci.channels.load(Ordering::Relaxed),
                "connected_at_ms": ci.connected_at_ms,
                "recv_oct": ci.recv_oct.load(Ordering::Relaxed),
                "send_oct": ci.send_oct.load(Ordering::Relaxed),
                "client": client,
                "connection_name": props.get("connection_name").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Json(Value::Array(out))
}

async fn close_connection(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let conn = id.parse::<u64>().ok().and_then(|id| c.broker.connection(id)).or_else(|| c.broker.connection_by_name(&id));
    let conn = conn.ok_or_else(|| ApiError::not_found(format!("no connection '{id}'")))?;
    conn.request_close("Closed via roll API");
    Ok(StatusCode::NO_CONTENT)
}

async fn list_channels(State(c): State<AppState>) -> Json<Value> {
    let out: Vec<Value> = c
        .broker
        .channels()
        .iter()
        .map(|ch| {
            json!({
                "id": ch.id,
                "connection": ch.conn,
                "number": ch.number,
                "vhost": ch.vhost,
                "user": ch.user,
                "prefetch": ch.prefetch_count.load(Ordering::Relaxed),
                "unacked": ch.unacked.load(Ordering::Relaxed),
                "consumers": ch.consumers.load(Ordering::Relaxed),
                "confirm": ch.confirm.load(Ordering::Relaxed),
                "transactional": ch.transactional.load(Ordering::Relaxed),
            })
        })
        .collect();
    Json(Value::Array(out))
}

// ── policies, users, permissions ──

fn policy_json(p: &Policy, operator: bool) -> Value {
    json!({ "vhost": p.vhost, "name": p.name, "pattern": p.pattern, "apply_to": p.apply_to, "definition": p.definition, "priority": p.priority, "operator": operator })
}

async fn list_policies(State(c): State<AppState>) -> Json<Value> {
    let mut out: Vec<Value> = c.broker.policies.read().iter().map(|p| policy_json(p, false)).collect();
    out.extend(c.broker.operator_policies.read().iter().map(|p| policy_json(p, true)));
    Json(Value::Array(out))
}

#[derive(Deserialize)]
struct PolicyBody {
    pattern: String,
    #[serde(default = "all", alias = "apply-to")]
    apply_to: String,
    definition: Value,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    operator: bool,
}

fn all() -> String {
    "all".into()
}

async fn put_policy(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>, Json(b): Json<PolicyBody>) -> ApiResult {
    if !c.broker.vhost_exists(&vh) {
        return Err(ApiError::not_found(format!("no vhost '{vh}'")));
    }
    let p = Policy { vhost: vh, name, pattern: b.pattern, apply_to: b.apply_to, definition: b.definition, priority: b.priority };
    c.broker.put_policy(p.clone(), b.operator).await?;
    Ok(Json(policy_json(&p, b.operator)))
}

#[derive(Deserialize)]
struct OperatorQuery {
    #[serde(default)]
    operator: bool,
}

async fn delete_policy(State(c): State<AppState>, Path((vh, name)): Path<(String, String)>, Query(q): Query<OperatorQuery>) -> ApiResult<StatusCode> {
    if !c.broker.delete_policy(&vh, &name, q.operator).await? {
        return Err(ApiError::not_found(format!("no policy '{name}' in vhost '{vh}'")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_users(State(c): State<AppState>) -> Json<Value> {
    let perms = c.broker.permissions();
    let mut users = c.broker.users();
    users.sort_by(|a, b| a.name.cmp(&b.name));
    let out: Vec<Value> = users
        .iter()
        .map(|u| {
            let p: Vec<Value> = perms
                .iter()
                .filter(|(_, user, _)| *user == u.name)
                .map(|(vh, _, p)| json!({ "vhost": vh, "configure": p.configure, "write": p.write, "read": p.read }))
                .collect();
            json!({ "name": u.name, "tags": u.tags, "permissions": p, "limits": u.limits })
        })
        .collect();
    Json(Value::Array(out))
}

#[derive(Deserialize)]
struct UserBody {
    password: Option<String>,
    tags: Option<Value>,
}

async fn put_user(State(c): State<AppState>, Path(name): Path<String>, Json(b): Json<UserBody>) -> ApiResult {
    let tags: Option<Vec<String>> = b.tags.map(|t| match t {
        Value::String(s) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => vec![],
    });
    let user = match (c.broker.user(&name), b.password) {
        (_, Some(pw)) => {
            let existing = c.broker.user(&name);
            let mut u = User::new(&name, &pw, tags.clone().or_else(|| existing.as_ref().map(|e| e.tags.clone())).unwrap_or_default());
            if let Some(e) = existing {
                u.limits = e.limits;
            }
            u
        }
        (Some(mut u), None) => {
            if let Some(t) = tags {
                u.tags = t;
            }
            u
        }
        (None, None) => return Err(ApiError::bad("password is required for a new user")),
    };
    c.broker.put_user(user.clone()).await?;
    Ok(Json(json!({ "name": user.name, "tags": user.tags })))
}

async fn delete_user(State(c): State<AppState>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    if !c.broker.delete_user(&name).await? {
        return Err(ApiError::not_found(format!("no user '{name}'")));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PermissionBody {
    #[serde(default)]
    configure: String,
    #[serde(default)]
    write: String,
    #[serde(default)]
    read: String,
}

async fn put_permission(State(c): State<AppState>, Path((vh, user)): Path<(String, String)>, Json(b): Json<PermissionBody>) -> ApiResult {
    if !c.broker.vhost_exists(&vh) {
        return Err(ApiError::not_found(format!("no vhost '{vh}'")));
    }
    if c.broker.user(&user).is_none() {
        return Err(ApiError::not_found(format!("no user '{user}'")));
    }
    for re in [&b.configure, &b.write, &b.read] {
        regex::Regex::new(re).map_err(|e| ApiError::bad(format!("invalid regex '{re}': {e}")))?;
    }
    c.broker.set_permission(&vh, &user, Permission { configure: b.configure.clone(), write: b.write.clone(), read: b.read.clone() }).await?;
    Ok(Json(json!({ "vhost": vh, "user": user, "configure": b.configure, "write": b.write, "read": b.read })))
}

async fn delete_permission(State(c): State<AppState>, Path((vh, user)): Path<(String, String)>) -> ApiResult<StatusCode> {
    c.broker.clear_permission(&vh, &user).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── definitions and migration ──

async fn export_definitions(State(c): State<AppState>) -> ApiResult {
    Ok(Json(c.broker.export_definitions(None).await?))
}

async fn import_definitions(State(c): State<AppState>, Json(defs): Json<Value>) -> ApiResult {
    let report = c.broker.import_definitions(&defs, None).await?;
    Ok(Json(json!({ "ok": true, "skipped": report.skipped })))
}

fn migrator(c: &QueuesControl) -> Result<crate::Migrator, ApiError> {
    c.migrator().ok_or_else(|| ApiError::new(StatusCode::NOT_IMPLEMENTED, "migration isn't available on this node"))
}

async fn migrate(State(c): State<AppState>, Json(body): Json<Value>) -> ApiResult {
    let start = migrator(&c)?.start;
    let url = body.pointer("/source/management_url").and_then(|v| v.as_str()).unwrap_or_default();
    if url.is_empty() {
        return Err(ApiError::bad("source.management_url is required"));
    }
    Ok(Json(start(body).await?))
}

async fn migrate_status(State(c): State<AppState>) -> Json<Value> {
    match c.migrator() {
        Some(m) => Json((m.status)()),
        None => Json(json!({ "shovels": [] })),
    }
}

/// Stop the migration: remove every migration shovel (and the source credentials they hold).
async fn stop_migration(State(c): State<AppState>) -> ApiResult {
    let stop = migrator(&c)?.stop;
    Ok(Json(stop().await?))
}


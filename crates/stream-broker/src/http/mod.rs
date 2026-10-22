//! The HTTP API.
//!
//! - Control plane (`/v1/streams`): streams, topics, API keys, groups and usage, for the
//!   dashboard and CLI.
//! - Data plane: produce and consume without a Kafka connection — one request per batch,
//!   stateless cursors, server-managed consumer groups with partition leases, and SSE tailing.
//!   It reads and writes the same log as the Kafka listener.
//! - Confluent REST v3 (`/kafka/v3`) for tools that already speak it.

mod confluent;
mod data;
mod leases;
pub mod time;

use crate::auth;
use crate::broker::{Broker, Stream, Topic};
use crate::error::KafkaError;
use crate::ids::now;
use crate::store::{Credential, Settings};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower_http::cors::CorsLayer;

pub struct Api {
    pub b: Arc<Broker>,
    leases: leases::Leases,
    /// Sticky partition choice for keyless HTTP records.
    rr: AtomicUsize,
}

type S = State<Arc<Api>>;

/// Produce one record to a topic from inside the process (roll storage's event notifications),
/// exactly as the HTTP data plane does: a keyed record goes to its key's partition, and this
/// returns once it's durable.
pub async fn produce_one(b: &Arc<Broker>, stream: &Arc<Stream>, topic: &Arc<Topic>, key: Option<bytes::Bytes>, value: bytes::Bytes, headers: Vec<(String, bytes::Bytes)>, timeout: std::time::Duration) -> Result<(), String> {
    let api = Api { b: b.clone(), leases: leases::Leases::default(), rr: AtomicUsize::new(0) };
    let record = stream_log::batch::NewRecord { key, value: Some(value), headers: headers.into_iter().map(|(k, v)| (k, Some(v))).collect(), timestamp: None };
    let out = data::produce_records(&api, stream, topic, vec![data::Incoming { partition: None, record }], data::Acks::All, timeout).await;
    match out.into_iter().next() {
        Some(Ok(_)) => Ok(()),
        Some(Err(e)) => Err(e.message),
        None => Err("nothing was produced".into()),
    }
}

pub fn router(b: Arc<Broker>) -> Router {
    let api = Arc::new(Api { b, leases: leases::Leases::default(), rr: AtomicUsize::new(0) });
    Router::new()
        .route("/v1/kafka", get(info))
        .route("/v1/streams", get(list_streams).post(create_stream))
        .route("/v1/streams/{s}", get(get_stream).patch(update_stream).delete(delete_stream))
        .route("/v1/streams/{s}/keys", get(list_keys).post(create_key))
        .route("/v1/streams/{s}/keys/{username}", delete(delete_key))
        .route("/v1/streams/{s}/topics", get(list_topics).post(create_topic))
        .route("/v1/streams/{s}/topics/{t}", get(get_topic).patch(update_topic).delete(delete_topic))
        .route("/v1/streams/{s}/groups", get(list_groups))
        .route("/v1/streams/{s}/groups/{g}", get(get_group).delete(delete_group))
        .route("/v1/streams/{s}/groups/{g}/reset", post(reset_group))
        .route("/v1/streams/{s}/usage", get(usage))
        .merge(data::routes())
        .merge(confluent::routes())
        .layer(CorsLayer::permissive())
        .with_state(api)
}

// ── errors ──

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub extra: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into(), extra: None }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }
    pub fn with(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }
}

/// `UnknownTopicOrPartition` → `unknown_topic_or_partition`.
pub fn snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

impl From<KafkaError> for ApiError {
    fn from(e: KafkaError) -> Self {
        let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
        Self::new(status, &snake(&format!("{:?}", e.code)), e.message())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.message, "code": self.code });
        if let (Some(Value::Object(extra)), Value::Object(m)) = (self.extra, &mut body) {
            m.extend(extra);
        }
        let mut resp = (self.status, Json(body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, header::HeaderValue::from_static("Basic realm=\"roll\""));
        }
        resp
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

// ── auth ──

/// The API key in `Authorization` (Basic `user:secret`, or Bearer `user:secret`), verified.
fn principal(api: &Api, headers: &HeaderMap) -> ApiResult<Option<Credential>> {
    let Some(h) = headers.get(header::AUTHORIZATION) else { return Ok(None) };
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "invalid API key");
    let h = h.to_str().map_err(|_| unauthorized())?;
    let pair = if let Some(b) = h.strip_prefix("Basic ") {
        String::from_utf8(B64.decode(b.trim()).map_err(|_| unauthorized())?).map_err(|_| unauthorized())?
    } else if let Some(t) = h.strip_prefix("Bearer ") {
        t.trim().to_string()
    } else {
        return Err(unauthorized());
    };
    let (user, secret) = pair.split_once(':').ok_or_else(unauthorized)?;
    let cred = api.b.credential(user).ok_or_else(unauthorized)?;
    if !api.b.plain.verify(&cred, secret) {
        return Err(unauthorized());
    }
    Ok(Some(cred))
}

/// The stream a data-plane request targets, checked against its API key.
pub fn data_stream(api: &Api, headers: &HeaderMap, key: &str) -> ApiResult<Arc<Stream>> {
    let cred = principal(api, headers)?;
    let stream = api.b.find_stream(key).ok_or_else(|| ApiError::not_found(format!("stream {key} not found")))?;
    match cred {
        Some(c) if c.stream_id == stream.id => {}
        Some(_) => return Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden", "this API key belongs to another stream")),
        None if api.b.config.http_require_auth => {
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "an API key is required"));
        }
        None => {}
    }
    stream.usage.requests.fetch_add(1, Ordering::Relaxed);
    Ok(stream)
}

fn stream_of(api: &Api, key: &str) -> ApiResult<Arc<Stream>> {
    api.b.find_stream(key).ok_or_else(|| ApiError::not_found(format!("stream {key} not found")))
}

fn topic_of(stream: &Stream, name: &str) -> ApiResult<Arc<Topic>> {
    stream.topic(name).ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_topic_or_partition", format!("topic {name} not found")))
}

// ── views ──

fn bootstrap(b: &Broker) -> String {
    format!("{}:{}", b.config.advertised_host, b.config.advertised_port)
}

fn security_protocol(b: &Broker) -> &'static str {
    if b.config.tls.is_some() { "SASL_SSL" } else { "SASL_PLAINTEXT" }
}

fn stream_view(api: &Api, s: &Stream) -> Value {
    let row = s.row.read().unwrap().clone();
    let topics = s.topics();
    json!({
        "id": row.id,
        "name": row.name,
        "region": row.region,
        "created_at": row.created_at,
        "cluster_id": s.cluster_id,
        "settings": row.settings,
        "bootstrap": bootstrap(&api.b),
        "http": format!("{}/v1/streams/{}", api.b.config.http_url, row.id),
        "topics": topics.len(),
        "partitions": topics.iter().map(|t| t.partition_count()).sum::<i32>(),
        "throughput": {
            "total_in_bytes": s.usage.total_in.load(Ordering::Relaxed),
            "total_out_bytes": s.usage.total_out.load(Ordering::Relaxed),
        },
    })
}

/// Client settings for a key: Kafka properties plus the HTTP endpoints.
fn connect_view(api: &Api, s: &Stream, username: &str, secret: &str) -> Value {
    let b = &api.b;
    let boot = bootstrap(b);
    let proto = security_protocol(b);
    json!({
        "bootstrap": boot,
        "security_protocol": proto,
        "sasl_mechanism": "SCRAM-SHA-256",
        "username": username,
        "secret": secret,
        "properties": format!(
            "bootstrap.servers={boot}\nsecurity.protocol={proto}\nsasl.mechanism=SCRAM-SHA-256\nsasl.jaas.config=org.apache.kafka.common.security.scram.ScramLoginModule required username=\"{username}\" password=\"{secret}\";\n"
        ),
        "librdkafka": {
            "bootstrap.servers": boot,
            "security.protocol": proto,
            "sasl.mechanisms": "SCRAM-SHA-256",
            "sasl.username": username,
            "sasl.password": secret,
        },
        "http": {
            "produce": format!("{}/v1/streams/{}/topics/{{topic}}/records", b.config.http_url, s.id),
            "authorization": format!("Bearer {username}:{secret}"),
        },
    })
}

fn key_view(c: &Credential) -> Value {
    json!({ "username": c.username, "label": c.label, "created_at": c.created_at })
}

fn topic_summary(t: &Topic) -> Value {
    json!({
        "id": t.id.to_string(),
        "name": t.name,
        "partitions": t.partition_count(),
        "created_at": t.created_at,
        "configs": t.configs(),
    })
}

fn topic_view(b: &Broker, t: &Topic) -> Value {
    let mut parts = vec![];
    let (mut records, mut local, mut remote) = (0i64, 0u64, 0u64);
    for p in t.partitions() {
        match b.log(t, &p) {
            Ok(log) => {
                let st = log.stats();
                records += st.high_watermark - st.log_start;
                local += st.local_bytes;
                remote += st.remote_bytes;
                parts.push(json!({
                    "partition": p.index,
                    "log_start_offset": st.log_start,
                    "high_watermark": st.high_watermark,
                    "last_stable_offset": st.last_stable,
                    "local_bytes": st.local_bytes,
                    "remote_bytes": st.remote_bytes,
                    "pending_upload_bytes": st.pending_upload_bytes,
                    "segments": st.segments,
                    "chunks": st.chunks,
                    "producers": st.producers,
                    "max_timestamp": st.max_timestamp,
                }));
            }
            Err(e) => parts.push(json!({ "partition": p.index, "error": e.message() })),
        }
    }
    let mut v = topic_summary(t);
    let m = v.as_object_mut().unwrap();
    m.insert("records".into(), json!(records));
    m.insert("local_bytes".into(), json!(local));
    m.insert("remote_bytes".into(), json!(remote));
    m.insert("partition_stats".into(), Value::Array(parts));
    m.insert(
        "effective_configs".into(),
        json!(
            crate::handlers::configs::topic_configs(b, t, None)
                .into_iter()
                .map(|e| (e.name, json!({ "value": e.value, "default": e.source == crate::handlers::configs::SOURCE_DEFAULT })))
                .collect::<BTreeMap<_, _>>()
        ),
    );
    v
}

// ── control plane ──

async fn info(State(api): S) -> Json<Value> {
    let b = &api.b;
    Json(json!({
        "bootstrap": bootstrap(b),
        "security_protocol": security_protocol(b),
        "sasl_mechanisms": auth::MECHANISMS,
        "default_stream": b.config.default_stream,
        "http_require_auth": b.config.http_require_auth,
        "storage": b.objects.description,
        "pricing": b.config.pricing,
        "streams": b.streams().len(),
        "open_partitions": b.tail_budget.open_logs(),
        "tail_cache_bytes": b.tail_budget.used(),
        "uptime_s": b.started.elapsed().as_secs(),
    }))
}

async fn list_streams(State(api): S) -> Json<Value> {
    let mut streams = api.b.streams();
    streams.sort_by_key(|s| s.row.read().unwrap().created_at);
    Json(Value::Array(streams.iter().map(|s| stream_view(&api, s)).collect()))
}

#[derive(Deserialize)]
struct CreateStream {
    name: String,
    #[serde(default)]
    settings: Option<Settings>,
}

async fn create_stream(State(api): S, Json(req): Json<CreateStream>) -> ApiResult<(StatusCode, Json<Value>)> {
    let (s, cred, secret) = api.b.create_stream(&req.name, req.settings.unwrap_or_default())?;
    let mut v = stream_view(&api, &s);
    v["connect"] = connect_view(&api, &s, &cred.username, &secret);
    Ok((StatusCode::CREATED, Json(v)))
}

async fn get_stream(State(api): S, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    let mut v = stream_view(&api, &s);
    v["topic_list"] = Value::Array(s.topics().iter().map(|t| topic_summary(t)).collect());
    v["keys"] = Value::Array(api.b.store.credentials(&s.id).map_err(KafkaError::internal)?.iter().map(key_view).collect());
    Ok(Json(v))
}

#[derive(Deserialize)]
struct UpdateStream {
    settings: Value,
}

async fn update_stream(State(api): S, Path(key): Path<String>, Json(req): Json<UpdateStream>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    let Value::Object(patch) = req.settings else { return Err(ApiError::bad("settings must be an object")) };
    let mut current = serde_json::to_value(s.settings()).map_err(KafkaError::internal)?;
    for (k, v) in patch {
        if current.get(&k).is_none() {
            return Err(ApiError::bad(format!("unknown setting {k}")));
        }
        current[k] = v;
    }
    let next: Settings = serde_json::from_value(current).map_err(|e| ApiError::bad(e.to_string()))?;
    api.b.update_settings(&s, |st| *st = next)?;
    Ok(Json(stream_view(&api, &s)))
}

async fn delete_stream(State(api): S, Path(key): Path<String>) -> ApiResult<StatusCode> {
    let s = stream_of(&api, &key)?;
    api.leases.drop_stream(&s.id);
    api.b.delete_stream(&s.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_keys(State(api): S, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    let keys = api.b.store.credentials(&s.id).map_err(KafkaError::internal)?;
    Ok(Json(Value::Array(keys.iter().map(key_view).collect())))
}

#[derive(Deserialize, Default)]
struct CreateKey {
    #[serde(default)]
    label: Option<String>,
}

async fn create_key(State(api): S, Path(key): Path<String>, body: Option<Json<CreateKey>>) -> ApiResult<(StatusCode, Json<Value>)> {
    let s = stream_of(&api, &key)?;
    let label = body.and_then(|b| b.0.label).unwrap_or_else(|| "key".into());
    let (cred, secret) = api.b.create_key(&s, &label)?;
    let mut v = key_view(&cred);
    v["secret"] = json!(secret);
    v["connect"] = connect_view(&api, &s, &cred.username, &secret);
    Ok((StatusCode::CREATED, Json(v)))
}

async fn delete_key(State(api): S, Path((key, username)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let s = stream_of(&api, &key)?;
    if !api.b.store.delete_credential(&s.id, &username).map_err(KafkaError::internal)? {
        return Err(ApiError::not_found(format!("key {username} not found")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_topics(State(api): S, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    Ok(Json(Value::Array(s.topics().iter().map(|t| topic_summary(t)).collect())))
}

#[derive(Deserialize)]
struct CreateTopic {
    name: String,
    #[serde(default)]
    partitions: Option<i32>,
    #[serde(default)]
    configs: BTreeMap<String, String>,
}

async fn create_topic(State(api): S, Path(key): Path<String>, Json(req): Json<CreateTopic>) -> ApiResult<(StatusCode, Json<Value>)> {
    let s = stream_of(&api, &key)?;
    let t = api.b.create_topic(&s, &req.name, req.partitions.unwrap_or(-1), -1, req.configs, false)?;
    let t = t.ok_or_else(|| ApiError::conflict("topic was not created"))?;
    Ok((StatusCode::CREATED, Json(topic_view(&api.b, &t))))
}

async fn get_topic(State(api): S, Path((key, name)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    let t = topic_of(&s, &name)?;
    Ok(Json(topic_view(&api.b, &t)))
}

#[derive(Deserialize)]
struct UpdateTopic {
    #[serde(default)]
    partitions: Option<i32>,
    /// `name → value`; `null` removes the override.
    #[serde(default)]
    configs: Option<BTreeMap<String, Option<String>>>,
}

/// Online changes: partitions only grow, configs apply to open logs immediately.
async fn update_topic(State(api): S, Path((key, name)): Path<(String, String)>, Json(req): Json<UpdateTopic>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    let t = topic_of(&s, &name)?;
    if let Some(changes) = req.configs {
        let mut configs = t.configs();
        for (k, v) in changes {
            match v {
                Some(v) => configs.insert(k, v),
                None => configs.remove(&k),
            };
        }
        api.b.set_topic_configs(&t, configs, false)?;
    }
    if let Some(n) = req.partitions
        && n != t.partition_count()
    {
        api.b.add_partitions(&s, &t, n, false)?;
    }
    Ok(Json(topic_view(&api.b, &t)))
}

async fn delete_topic(State(api): S, Path((key, name)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let s = stream_of(&api, &key)?;
    let t = topic_of(&s, &name)?;
    api.leases.topic_deleted(&s.id, &t.name);
    api.b.delete_topic(&s, &t).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Per-partition committed offsets and lag of a group.
fn group_lag(b: &Broker, s: &Stream, group: &str) -> ApiResult<(Vec<Value>, i64)> {
    let rows = b.store.offsets(&s.id, group).map_err(KafkaError::internal)?;
    let mut total = 0;
    let mut out = vec![];
    for r in rows {
        let end = s.topic(&r.topic).and_then(|t| t.partition(r.partition).and_then(|p| b.log(&t, &p).ok())).map(|l| l.high_watermark());
        let lag = end.map(|e| (e - r.offset).max(0));
        total += lag.unwrap_or(0);
        out.push(json!({
            "topic": r.topic,
            "partition": r.partition,
            "committed_offset": r.offset,
            "high_watermark": end,
            "lag": lag,
            "metadata": r.metadata,
            "committed_at": r.committed_at,
        }));
    }
    Ok((out, total))
}

async fn list_groups(State(api): S, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    let mut out = vec![];
    let mut seen = std::collections::BTreeSet::new();
    for (id, protocol_type, state) in api.b.groups.list(&api.b, &s.id) {
        let (_, lag) = group_lag(&api.b, &s, &id)?;
        let members = api.b.groups.describe(&api.b, &s.id, &id).map(|g| g.members.len()).unwrap_or(0);
        let http = api.leases.consumers(&s.id, &id);
        out.push(json!({ "group_id": id, "protocol_type": protocol_type, "state": state, "members": members, "http_consumers": http.len(), "lag": lag }));
        seen.insert(id);
    }
    for id in api.leases.groups(&s.id) {
        if !seen.contains(&id) {
            let (_, lag) = group_lag(&api.b, &s, &id)?;
            let http = api.leases.consumers(&s.id, &id);
            out.push(json!({ "group_id": id, "protocol_type": "http", "state": "Stable", "members": 0, "http_consumers": http.len(), "lag": lag }));
        }
    }
    Ok(Json(Value::Array(out)))
}

async fn get_group(State(api): S, Path((key, group)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    let info = api.b.groups.describe(&api.b, &s.id, &group);
    let http = api.leases.consumers(&s.id, &group);
    if info.is_none() && http.is_empty() {
        return Err(ApiError::not_found(format!("group {group} not found")));
    }
    let (offsets, lag) = group_lag(&api.b, &s, &group)?;
    Ok(Json(json!({
        "group_id": group,
        "group": info,
        "http_consumers": http,
        "offsets": offsets,
        "lag": lag,
    })))
}

async fn delete_group(State(api): S, Path((key, group)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let s = stream_of(&api, &key)?;
    if api.leases.is_active(&s.id, &group) {
        return Err(ApiError::conflict("the group has active HTTP consumers"));
    }
    api.b.groups.delete(&api.b, &s.id, &group).map_err(KafkaError::code)?;
    api.leases.drop_group(&s.id, &group);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ResetGroup {
    topic: String,
    /// `earliest`, `latest`, an offset, or `timestamp:<ms>`.
    to: String,
    #[serde(default)]
    partitions: Option<Vec<i32>>,
}

/// Move a group's committed offsets (like `kafka-consumer-groups --reset-offsets`). The group
/// must be inactive.
async fn reset_group(State(api): S, Path((key, group)): Path<(String, String)>, Json(req): Json<ResetGroup>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    if api.b.groups.is_active(&s.id, &group) || api.leases.is_active(&s.id, &group) {
        return Err(ApiError::conflict("stop the group's consumers before resetting offsets"));
    }
    let t = topic_of(&s, &req.topic)?;
    let mut rows = vec![];
    for p in t.partitions() {
        if req.partitions.as_ref().is_some_and(|ps| !ps.contains(&p.index)) {
            continue;
        }
        let log = api.b.log(&t, &p)?;
        let offset = match req.to.as_str() {
            "earliest" => log.log_start(),
            "latest" => log.high_watermark(),
            other => match other.strip_prefix("timestamp:") {
                Some(ts) => {
                    let ts: i64 = ts.parse().map_err(|_| ApiError::bad("bad timestamp"))?;
                    crate::handlers::offsets::offset_for_timestamp(&api.b, &s, &t, &p, ts)
                        .await
                        .map_err(KafkaError::storage)?
                        .map(|(o, _)| o)
                        .unwrap_or_else(|| log.high_watermark())
                }
                None => other.parse::<i64>().map_err(|_| ApiError::bad("to must be earliest, latest, an offset or timestamp:<ms>"))?.clamp(log.log_start(), log.high_watermark()),
            },
        };
        rows.push(crate::store::OffsetRow { topic: t.name.clone(), partition: p.index, offset, leader_epoch: -1, metadata: None, committed_at: now() });
    }
    api.b.store.commit_offsets(&s.id, &group, &rows).map_err(KafkaError::internal)?;
    api.leases.drop_group(&s.id, &group);
    let (offsets, lag) = group_lag(&api.b, &s, &group)?;
    Ok(Json(json!({ "group_id": group, "offsets": offsets, "lag": lag })))
}

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default)]
    hours: Option<i64>,
}

/// Metered usage and a monthly AWS cost estimate, next to what MSK would cost.
async fn usage(State(api): S, Path(key): Path<String>, Query(q): Query<UsageQuery>) -> ApiResult<Json<Value>> {
    let s = stream_of(&api, &key)?;
    api.b.flush_usage();
    let hours = q.hours.unwrap_or(24 * 7).clamp(1, 24 * 90);
    let since = now() - hours * 3600;
    let rows = api.b.store.usage_since(&s.id, since).map_err(KafkaError::internal)?;
    let storage = api.b.store.storage_since(&s.id, since).map_err(KafkaError::internal)?;
    Ok(Json(json!({
        "stream": s.id,
        "hours": hours,
        "usage": rows,
        "storage": storage,
        "estimate": estimate(&api.b.config.pricing, &rows, &storage, hours),
    })))
}

/// Project the window onto a 730-hour month.
fn estimate(p: &crate::config::Pricing, rows: &[crate::store::UsageRow], storage: &[crate::store::StorageSample], hours: i64) -> Value {
    const GB: f64 = 1e9;
    let scale = 730.0 / hours as f64;
    let sum = |f: fn(&crate::store::UsageRow) -> i64| rows.iter().map(f).sum::<i64>() as f64 * scale;
    let (bytes_in, bytes_out) = (sum(|r| r.bytes_in), sum(|r| r.bytes_out));
    let (puts, gets) = (sum(|r| r.puts), sum(|r| r.gets));
    let last = storage.last();
    let remote_gb = last.map(|s| s.remote_bytes as f64 / GB).unwrap_or(0.0);
    let local_gb = last.map(|s| s.local_bytes as f64 / GB).unwrap_or(0.0);
    let s3 = remote_gb * p.usd_per_gb_month_s3;
    let local = local_gb * p.usd_per_gb_month_local;
    let requests = puts / 1000.0 * p.usd_per_1k_put + gets / 1000.0 * p.usd_per_1k_get;
    let transfer = (bytes_in + bytes_out) / GB * p.usd_per_gb_transfer;
    let roll = s3 + local + requests + transfer;
    // MSK keeps every byte on replicated broker storage.
    let msk_compute = p.msk_broker_usd_per_hour * p.msk_brokers as f64 * 730.0;
    let msk_storage = (remote_gb + local_gb) * 3.0 * p.msk_storage_usd_per_gb_month;
    json!({
        "monthly_usd": {
            "s3_storage": s3,
            "local_storage": local,
            "requests": requests,
            "transfer": transfer,
            "total": roll,
        },
        "msk_monthly_usd": { "brokers": msk_compute, "storage": msk_storage, "total": msk_compute + msk_storage },
        "projected": { "bytes_in": bytes_in, "bytes_out": bytes_out, "puts": puts, "gets": gets, "remote_gb": remote_gb, "local_gb": local_gb },
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn snake_case_codes() {
        assert_eq!(super::snake("UnknownTopicOrPartition"), "unknown_topic_or_partition");
        assert_eq!(super::snake("InvalidRequest"), "invalid_request");
    }
}

//! Control-plane HTTP API for Redis (`/v1/redis/...`), merged into rolld's API.

use crate::control::{Control, ControlError, DbOptions};
use crate::store::{RedisDb, Replica};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

type AppState = Arc<Control>;
type ApiResult<T> = Result<Json<T>, ControlError>;

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let status = match &self {
            ControlError::NotFound(_) => StatusCode::NOT_FOUND,
            ControlError::Invalid(_) => StatusCode::BAD_REQUEST,
            ControlError::Conflict(_) => StatusCode::CONFLICT,
            ControlError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

pub fn router(control: Arc<Control>) -> Router {
    Router::new()
        .route("/v1/redis", get(info))
        .route("/v1/redis/databases", get(list).post(create))
        .route("/v1/redis/databases/{id}", get(show).patch(update).delete(remove))
        .route("/v1/redis/databases/{id}/branches", post(branch))
        .route("/v1/redis/databases/{id}/restore", post(restore))
        .route("/v1/redis/databases/{id}/password", post(rotate))
        .route("/v1/redis/databases/{id}/replicas", get(replicas).post(create_replica))
        .route("/v1/redis/databases/{id}/usage", get(usage))
        .route("/v1/redis/databases/{id}/unload", post(unload))
        .route("/v1/redis/databases/{id}/command", post(command))
        .route("/v1/redis/replicas/{id}", delete(delete_replica))
        .with_state(control)
}

async fn info(State(c): State<AppState>) -> Json<Value> {
    Json(json!({
        "redis_version": redis_engine::commands::server::REDIS_VERSION,
        "region": c.cfg.region,
        "resp": format!("{}:{}", c.cfg.public_host, c.cfg.resp_port),
        "http": format!("{}:{}", c.cfg.public_host, c.cfg.http_port),
        "tls": c.cfg.tls,
        "storage": c.objects().description,
        "defaults": c.cfg.defaults,
        "pricing": c.cfg.pricing,
        "loaded": c.node.loaded().len(),
    }))
}

#[derive(Serialize)]
struct DbView {
    #[serde(flatten)]
    db: RedisDb,
    #[serde(flatten)]
    connection: Value,
    status: Value,
    replicas: Vec<ReplicaView>,
}

#[derive(Serialize)]
struct ReplicaView {
    #[serde(flatten)]
    replica: Replica,
    #[serde(flatten)]
    connection: Value,
}

fn replica_view(c: &Control, r: Replica) -> ReplicaView {
    ReplicaView { connection: c.connection(&r.id, &r.password), replica: r }
}

fn view(c: &Control, d: RedisDb) -> Result<DbView, ControlError> {
    let replicas = c.store.replicas(&d.id)?.into_iter().map(|r| replica_view(c, r)).collect();
    Ok(DbView { connection: c.connection(&d.id, &d.password), status: c.status(&d.id)?, replicas, db: d })
}

async fn list(State(c): State<AppState>) -> ApiResult<Vec<DbView>> {
    Ok(Json(c.list()?.into_iter().map(|d| view(&c, d)).collect::<Result<_, _>>()?))
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    #[serde(flatten)]
    opts: DbOptions,
}

async fn create(State(c): State<AppState>, Json(r): Json<CreateReq>) -> ApiResult<DbView> {
    let d = c.create(&r.name, r.opts)?;
    Ok(Json(view(&c, d)?))
}

async fn show(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<DbView> {
    Ok(Json(view(&c, c.get(&id)?)?))
}

async fn update(State(c): State<AppState>, Path(id): Path<String>, Json(o): Json<DbOptions>) -> ApiResult<DbView> {
    let d = c.update(&id, o)?;
    Ok(Json(view(&c, d)?))
}

async fn remove(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    c.delete(&id).await?;
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Deserialize)]
struct BranchReq {
    name: String,
    /// Unix seconds; default now.
    at: Option<i64>,
}

async fn branch(State(c): State<AppState>, Path(id): Path<String>, Json(r): Json<BranchReq>) -> ApiResult<DbView> {
    let d = c.branch(&id, &r.name, r.at).await?;
    Ok(Json(view(&c, d)?))
}

#[derive(Deserialize)]
struct RestoreReq {
    at: i64,
}

async fn restore(State(c): State<AppState>, Path(id): Path<String>, Json(r): Json<RestoreReq>) -> ApiResult<Value> {
    let archive = c.restore(&id, r.at).await?;
    Ok(Json(json!({ "restored_to": r.at, "previous_state": archive })))
}

async fn rotate(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<DbView> {
    let d = c.rotate_password(&id)?;
    Ok(Json(view(&c, d)?))
}

async fn replicas(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Vec<ReplicaView>> {
    Ok(Json(c.replicas(&id)?.into_iter().map(|r| replica_view(&c, r)).collect()))
}

async fn create_replica(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ReplicaView> {
    let r = c.create_replica(&id)?;
    Ok(Json(replica_view(&c, r)))
}

async fn delete_replica(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    c.delete_replica(&id).await?;
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
}

async fn usage(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<UsageQuery>) -> ApiResult<crate::control::Usage> {
    Ok(Json(c.usage(&id, q.days.unwrap_or(30).clamp(1, 366))?))
}

async fn unload(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!({ "unloaded": c.unload(&id).await? })))
}

#[derive(Deserialize)]
struct CommandReq {
    command: Vec<Value>,
}

/// Run a command (dashboard console). Replies use the same JSON shape as the REST API.
async fn command(State(c): State<AppState>, Path(id): Path<String>, Json(r): Json<CommandReq>) -> ApiResult<Value> {
    let args = r
        .command
        .iter()
        .map(|v| match v {
            Value::String(s) => Ok(bytes::Bytes::copy_from_slice(s.as_bytes())),
            Value::Number(n) => Ok(bytes::Bytes::from(n.to_string())),
            _ => Err(ControlError::Invalid("command arguments must be strings or numbers".into())),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let reply = c.command(&id, args).await?;
    Ok(Json(match &reply {
        redis_engine::Reply::Error(e) => json!({ "error": e }),
        r => json!({ "result": redis_server::http::to_json(r, false) }),
    }))
}

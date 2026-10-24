//! The control-plane HTTP API for roll nosql (`/v1/nosql/...`, docs/nosql/contracts.md),
//! merged into rolld's API. Errors are `{"error": "..."}` with 400, 404, 409 or 500.

use crate::control::{BranchReq, Control, ControlError, CreateReq, PatchReq};
use crate::import::ImportReq;
use crate::store::Import;
use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

type AppState = Arc<Control>;
type ApiResult<T> = Result<Json<T>, ControlError>;

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let status = match &self {
            ControlError::NotFound(_) => StatusCode::NOT_FOUND,
            ControlError::Invalid(_) => StatusCode::BAD_REQUEST,
            ControlError::Conflict(_) => StatusCode::CONFLICT,
            ControlError::Internal(e) => {
                tracing::warn!(error = %format!("{e:#}"), "nosql control API error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let msg = match &self {
            ControlError::Internal(e) => format!("{e:#}"),
            e => e.to_string(),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

/// A JSON body whose rejections (bad JSON, missing fields) are `400 {"error": …}`. An empty
/// body reads as `{}`.
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ControlError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state).await.map_err(|e| ControlError::Invalid(e.body_text()))?;
        let bytes = if bytes.iter().all(u8::is_ascii_whitespace) { axum::body::Bytes::from_static(b"{}") } else { bytes };
        serde_json::from_slice(&bytes).map(Body).map_err(|e| ControlError::Invalid(format!("invalid request body: {e}")))
    }
}

pub fn router(control: Arc<Control>) -> Router {
    let db = |rest: &str| format!("/v1/nosql/databases/{{id}}{rest}");
    Router::new()
        .route("/v1/nosql", get(info))
        .route("/v1/nosql/databases", get(list).post(create))
        .route(&db(""), get(show).patch(update).delete(remove))
        .route(&db("/branches"), post(branch))
        .route(&db("/reset"), post(reset))
        .route(&db("/restore"), post(restore))
        .route(&db("/diff"), get(diff))
        .route(&db("/imports"), get(imports).post(import))
        .route(&db("/password"), post(rotate))
        .route(&db("/cql"), post(cql))
        .route(&db("/schema"), get(schema))
        .route(&db("/usage"), get(usage))
        .route(&db("/alternator"), get(alternator))
        .route(&db("/activity"), get(activity))
        .route(&db("/flush"), post(flush))
        .route(&db("/compact"), post(compact))
        .route(&db("/unload"), post(unload))
        // The dashboard calls it from the browser, like the other products' APIs.
        .layer(CorsLayer::permissive())
        .with_state(control)
}

async fn info(State(c): State<AppState>) -> Json<Value> {
    let defaults = |d: nosql_types::Dialect| {
        let mut cfg = nosql_engine::DbConfig::new("", d);
        cfg.settings.authenticator = "PasswordAuthenticator".into();
        cfg.settings.authorizer = "CassandraAuthorizer".into();
        crate::settings::effective(&cfg)
    };
    let host = &c.cfg.public_host;
    Json(json!({
        "version": { "scylla": "2026.3.3", "cassandra": "5.0.9" },
        "dialects": ["scylla", "cassandra"],
        "endpoints": {
            "cql": format!("{host}:{}", c.cfg.cql_port),
            "cql_shard_aware": c.cfg.cql_shard_aware_port.map(|p| format!("{host}:{p}")),
            "alternator": c.cfg.alternator_port.map(|p| format!("http://{host}:{p}")),
            "alternator_https": c.cfg.alternator_https_port.map(|p| format!("https://{host}:{p}")),
            "rest": c.cfg.rest_port.map(|p| format!("http://{host}:{p}")),
        },
        "alternator_domain": c.cfg.alternator_domain,
        "tls": c.cfg.tls,
        "domain": c.cfg.domain,
        "single": c.single_id,
        "storage": c.objects().description,
        "defaults": c.cfg.defaults,
        "settings": { "keys": crate::settings::KEYS, "scylla": defaults(nosql_types::Dialect::Scylla), "cassandra": defaults(nosql_types::Dialect::Cassandra) },
        "pricing": c.cfg.pricing,
        "loaded": c.node.loaded().len(),
        "memory": c.storage.memory_stats(),
    }))
}

async fn list(State(c): State<AppState>) -> ApiResult<Vec<Value>> {
    Ok(Json(c.list()?.iter().map(|d| c.view(d)).collect()))
}

async fn create(State(c): State<AppState>, Body(r): Body<CreateReq>) -> ApiResult<Value> {
    let d = c.create(r).await?;
    c.storage_bytes(&d.id).await?;
    Ok(Json(c.view(&d)))
}

async fn show(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let d = c.get(&id)?;
    c.storage_bytes(&d.id).await?;
    Ok(Json(c.view(&d)))
}

async fn update(State(c): State<AppState>, Path(id): Path<String>, Body(p): Body<PatchReq>) -> ApiResult<Value> {
    let d = c.update(&id, p).await?;
    Ok(Json(c.view(&d)))
}

async fn remove(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let d = c.get(&id)?;
    c.delete(&d.id).await?;
    Ok(Json(json!({ "deleted": d.id })))
}

async fn branch(State(c): State<AppState>, Path(id): Path<String>, Body(r): Body<BranchReq>) -> ApiResult<Value> {
    let d = c.branch(&id, r).await?;
    Ok(Json(c.view(&d)))
}

#[derive(Deserialize, Default)]
struct AtReq {
    at: Option<i64>,
}

async fn reset(State(c): State<AppState>, Path(id): Path<String>, Body(r): Body<AtReq>) -> ApiResult<Value> {
    let d = c.reset(&id, r.at).await?;
    Ok(Json(c.view(&d)))
}

async fn restore(State(c): State<AppState>, Path(id): Path<String>, Body(r): Body<AtReq>) -> ApiResult<Value> {
    let Some(at) = r.at else { return Err(ControlError::Invalid("at is required (unix seconds)".into())) };
    let archived = c.restore(&id, at).await?;
    Ok(Json(json!({ "restored_to": at, "archived": archived })))
}

async fn diff(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(c.diff(&id).await?))
}

fn import_view(i: &Import) -> Value {
    let mut v = serde_json::to_value(i).unwrap_or_else(|_| json!({}));
    v["from"] = json!(i.from_redacted());
    v
}

async fn imports(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Vec<Value>> {
    Ok(Json(c.imports(&id)?.iter().map(import_view).collect()))
}

async fn import(State(c): State<AppState>, Path(id): Path<String>, Body(r): Body<ImportReq>) -> ApiResult<Value> {
    Ok(Json(import_view(&c.start_import(&id, r)?)))
}

async fn rotate(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let d = c.rotate_password(&id).await?;
    Ok(Json(c.view(&d)))
}

async fn cql(State(c): State<AppState>, Path(id): Path<String>, Body(r): Body<nosql_server::http::CqlRequest>) -> ApiResult<Value> {
    if r.query.trim().is_empty() {
        return Err(ControlError::Invalid("query is required".into()));
    }
    Ok(Json(c.cql(&id, r).await?))
}

async fn schema(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(c.schema(&id).await?))
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
}

/// The database's Alternator credentials (the secret is the superuser's salted hash).
async fn alternator(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(c.alternator_credentials(&id).await?))
}

async fn usage(State(c): State<AppState>, Path(id): Path<String>, q: Result<Query<UsageQuery>, QueryRejection>) -> ApiResult<crate::control::Usage> {
    // A bad query string is `400 {"error": …}` too, like a bad body.
    let Query(q) = q.map_err(|e| ControlError::Invalid(e.body_text()))?;
    Ok(Json(c.usage(&id, q.days.unwrap_or(30).clamp(1, 366)).await?))
}

async fn activity(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(c.activity(&id).await?))
}

async fn flush(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!({ "flushed": c.flush(&id).await? })))
}

async fn compact(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!({ "compacted_tables": c.compact(&id).await? })))
}

async fn unload(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!({ "unloaded": c.unload(&id).await? })))
}

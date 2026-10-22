//! Control-plane HTTP API for roll search (`/v1/search/...`), merged into rolld's API.
//!
//! Errors are `{"error": "..."}` with 400 (invalid), 404 (no such cluster), 409 (name taken,
//! busy), 413 (a console request over the cluster's body limit) or 500, like the other control
//! planes. A cluster's password appears only in the
//! responses that create it (create, branch) or change it (rotate). Creating a cluster answers
//! in the shape `search-yaml run --cluster-hook` expects (`url`, `username`, `password`, `id`).

use crate::control::{ClusterOptions, ConsoleRequest, ConsoleResponse, Control, ControlError, Usage};
use crate::store::SearchCluster;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
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
            ControlError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ControlError::Internal(e) => {
                tracing::warn!(error = %e, "search control error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

pub fn router(control: Arc<Control>) -> Router {
    Router::new()
        .route("/v1/search", get(info))
        .route("/v1/search/clusters", get(list).post(create))
        .route("/v1/search/clusters/{id}", get(show).patch(update).delete(remove))
        .route("/v1/search/clusters/{id}/branches", get(branches).post(branch))
        .route("/v1/search/clusters/{id}/password", post(rotate))
        .route("/v1/search/clusters/{id}/unload", post(unload))
        .route("/v1/search/clusters/{id}/indices", get(indices))
        .route("/v1/search/clusters/{id}/request", post(request))
        .route("/v1/search/clusters/{id}/usage", get(usage))
        // The dashboard calls it from the browser, like the other control planes.
        .layer(CorsLayer::permissive())
        .with_state(control)
}

async fn info(State(c): State<AppState>) -> Json<Value> {
    let stats = c.data.stats();
    Json(json!({
        "version": search_es::version::ES_VERSION,
        "opensearch_version": search_es::version::OPENSEARCH_VERSION,
        "lucene_version": search_es::version::LUCENE_VERSION,
        "region": c.cfg.region,
        "http": format!("{}:{}", c.cfg.public_host, c.cfg.http_port),
        "tls": c.cfg.tls,
        "domain": c.cfg.domain,
        "storage": c.objects().description,
        "history_retention_s": c.cfg.history_retention.as_secs(),
        "defaults": c.cfg.defaults,
        "pricing": c.cfg.pricing,
        "loaded": c.node.loaded().len(),
        "cache": { "used_bytes": stats.cache.used, "budget_bytes": stats.cache.budget },
    }))
}

#[derive(Serialize)]
struct BranchRef {
    id: String,
    name: String,
    parent_at: Option<i64>,
    created_at: i64,
}

#[derive(Serialize)]
struct ClusterView {
    #[serde(flatten)]
    cluster: SearchCluster,
    #[serde(flatten)]
    connection: Value,
    status: Value,
    /// Name of the parent (`parent_id` stays when the parent is deleted; this doesn't).
    parent_name: Option<String>,
    branches: Vec<BranchRef>,
    /// Only when created or rotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

fn view(c: &Control, d: SearchCluster, with_password: bool) -> Result<ClusterView, ControlError> {
    let branches = c
        .store
        .children(&d.id)?
        .into_iter()
        .map(|b| BranchRef { id: b.id, name: b.name, parent_at: b.parent_at, created_at: b.created_at })
        .collect();
    let parent_name = match &d.parent_id {
        Some(p) => c.store.get(p)?.map(|p| p.name),
        None => None,
    };
    Ok(ClusterView {
        connection: c.connection(&d),
        status: c.status(&d),
        parent_name,
        branches,
        password: with_password.then(|| d.password.clone()),
        cluster: d,
    })
}

async fn list(State(c): State<AppState>) -> ApiResult<Vec<ClusterView>> {
    Ok(Json(c.list()?.into_iter().map(|d| view(&c, d, false)).collect::<Result<_, _>>()?))
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    /// Branch of this cluster (id or name)…
    parent: Option<String>,
    /// …as of this Unix second (default now).
    at: Option<i64>,
    #[serde(flatten)]
    opts: ClusterOptions,
}

async fn create(State(c): State<AppState>, Json(r): Json<CreateReq>) -> ApiResult<ClusterView> {
    let d = match &r.parent {
        Some(p) => c.branch(p, &r.name, r.at, r.opts).await?,
        None if r.at.is_some() => return Err(ControlError::Invalid("at needs a parent".into())),
        None => c.create(&r.name, r.opts).await?,
    };
    Ok(Json(view(&c, d, true)?))
}

async fn show(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ClusterView> {
    Ok(Json(view(&c, c.get(&id)?, false)?))
}

async fn update(State(c): State<AppState>, Path(id): Path<String>, Json(o): Json<ClusterOptions>) -> ApiResult<ClusterView> {
    let d = c.update(&id, o).await?;
    Ok(Json(view(&c, d, false)?))
}

async fn remove(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let d = c.get(&id)?;
    c.delete(&d.id).await?;
    Ok(Json(json!({ "deleted": d.id, "name": d.name })))
}

#[derive(Deserialize)]
struct BranchReq {
    name: String,
    /// Unix seconds; default now.
    at: Option<i64>,
    #[serde(flatten)]
    opts: ClusterOptions,
}

async fn branch(State(c): State<AppState>, Path(id): Path<String>, Json(r): Json<BranchReq>) -> ApiResult<ClusterView> {
    let d = c.branch(&id, &r.name, r.at, r.opts).await?;
    Ok(Json(view(&c, d, true)?))
}

async fn branches(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Vec<ClusterView>> {
    let p = c.get(&id)?;
    Ok(Json(c.store.children(&p.id)?.into_iter().map(|d| view(&c, d, false)).collect::<Result<_, _>>()?))
}

async fn rotate(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ClusterView> {
    let d = c.rotate_password(&id).await?;
    Ok(Json(view(&c, d, true)?))
}

async fn unload(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!({ "unloaded": c.unload(&id).await? })))
}

async fn indices(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Vec<Value>> {
    Ok(Json(c.indices(&id).await?))
}

/// The console request is read up to the cluster's own body limit (`max_body_mb`, like a
/// client talking to it directly) plus room for its JSON envelope, instead of axum's 2 MB
/// default: a body sent as a JSON string grows by its escapes. The cluster then enforces its
/// limit exactly, with Elasticsearch's 413.
async fn request(State(c): State<AppState>, Path(id): Path<String>, body: Body) -> ApiResult<ConsoleResponse> {
    let cluster = c.get(&id)?;
    let limit = console_limit(cluster.max_body_mb);
    let bytes = axum::body::to_bytes(body, limit).await.map_err(|e| {
        if e.to_string().contains("length limit") {
            ControlError::TooLarge(format!("the request is larger than cluster {}'s body limit allows ({} MiB, max_body_mb)", cluster.name, cluster.max_body_mb))
        } else {
            ControlError::Invalid(format!("reading the request failed: {e}"))
        }
    })?;
    let r: ConsoleRequest = serde_json::from_slice(&bytes).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    Ok(Json(c.request(&cluster.id, r).await?))
}

/// Bytes of a console request for a cluster whose bodies may be `max_body_mb` MiB: twice that
/// (escaped quotes and newlines double in a JSON string) plus 1 MiB for the rest.
pub fn console_limit(max_body_mb: i64) -> usize {
    ((max_body_mb.max(1) as usize) << 21).saturating_add(1 << 20)
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
}

async fn usage(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<UsageQuery>) -> ApiResult<Usage> {
    Ok(Json(c.usage(&id, q.days.unwrap_or(30).clamp(1, 366))?))
}

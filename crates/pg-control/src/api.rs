//! HTTP API for the dashboard and CLI, plus the internal WAL endpoint computes restore from.

use crate::control::{Control, ControlError, ProxyRouter, SUPPORTED_VERSIONS};
use crate::history::Target;
use crate::ids::now;
use crate::store::{Branch, Endpoint, Project};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pg_storage::Lsn;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

type AppState = Arc<Control>;

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

type ApiResult<T> = Result<Json<T>, ControlError>;

pub fn router(control: Arc<Control>) -> Router {
    let public = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/{id}", get(get_project).delete(delete_project).patch(update_project))
        .route("/v1/projects/{id}/branches", post(create_branch))
        .route("/v1/projects/{id}/branches/{branch}", axum::routing::delete(delete_branch))
        .route("/v1/projects/{id}/branches/{branch}/history", get(branch_history))
        .route("/v1/projects/{id}/branches/{branch}/restore", post(restore_branch))
        .route("/v1/projects/{id}/branches/{branch}/backup", post(backup_branch))
        .route("/v1/projects/{id}/branches/{branch}/evict", post(evict_branch))
        .route("/v1/projects/{id}/branches/{branch}/replicas", post(create_replica))
        .route("/v1/projects/{id}/usage", get(usage))
        .route("/v1/endpoints/{id}", get(get_endpoint).patch(update_endpoint).delete(delete_endpoint))
        .route("/v1/endpoints/{id}/start", post(start_endpoint))
        .route("/v1/endpoints/{id}/suspend", post(suspend_endpoint))
        .route("/v1/endpoints/{id}/query", post(query))
        .layer(CorsLayer::permissive());
    public.route("/internal/wal/{branch}/{file}", get(internal_wal)).with_state(control)
}

/// The proxy router for this control plane.
pub fn proxy_router(control: Arc<Control>) -> Arc<ProxyRouter> {
    Arc::new(ProxyRouter(control))
}

async fn health(State(c): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "pg_versions": SUPPORTED_VERSIONS,
        "default_pg_version": c.config.default_pg_version,
        "host_cpus": c.driver.host_cpus,
        "pricing": c.config.pricing,
        "proxy": format!("{}:{}", c.config.public_host, c.config.public_port),
        "storage": c.objects.description,
        "evict_after_s": c.config.evict_after_s,
    }))
}

#[derive(Serialize)]
struct EndpointView {
    #[serde(flatten)]
    endpoint: Endpoint,
    connections: usize,
    connection_uri: String,
}

#[derive(Serialize)]
struct BranchView {
    #[serde(flatten)]
    branch: Branch,
    endpoint: Option<EndpointView>,
    replicas: Vec<EndpointView>,
    connection_uri: Option<String>,
    /// Local data on this host (0 when evicted).
    storage_bytes: Option<i64>,
    /// Durable history in object storage (WAL + base images).
    object_bytes: Option<i64>,
}

#[derive(Serialize)]
struct ProjectView {
    #[serde(flatten)]
    project: Project,
    branches: Vec<BranchView>,
}

fn endpoint_view(c: &Control, project: &Project, e: Endpoint) -> EndpointView {
    EndpointView { connections: c.connections(&e.id), connection_uri: c.connection_uri(project, &e.id), endpoint: e }
}

fn project_view(c: &Control, project: Project) -> Result<ProjectView, ControlError> {
    let mut endpoints: HashMap<String, Vec<Endpoint>> = HashMap::new();
    for e in c.store.endpoints(&project.id)? {
        endpoints.entry(e.branch_id.clone()).or_default().push(e);
    }
    let storage = latest_storage(c, &project.id)?;
    let branches = c
        .store
        .branches(&project.id)?
        .into_iter()
        .map(|b| {
            let eps = endpoints.remove(&b.id).unwrap_or_default();
            let (primary, replicas): (Vec<_>, Vec<_>) = eps.into_iter().partition(|e| e.is_primary());
            let primary = primary.into_iter().next();
            BranchView {
                connection_uri: primary.as_ref().map(|e| c.connection_uri(&project, &e.id)),
                endpoint: primary.map(|e| endpoint_view(c, &project, e)),
                replicas: replicas.into_iter().map(|e| endpoint_view(c, &project, e)).collect(),
                storage_bytes: storage.get(&(b.id.clone(), "local".into())).copied(),
                object_bytes: storage.get(&(b.id.clone(), "s3".into())).copied(),
                branch: b,
            }
        })
        .collect();
    Ok(ProjectView { project, branches })
}

fn latest_storage(c: &Control, project_id: &str) -> Result<HashMap<(String, String), i64>, ControlError> {
    let mut out = HashMap::new();
    for s in c.store.storage_since(project_id, now() - 86_400 * 31)? {
        out.insert((s.branch_id, s.tier), s.bytes); // ordered by time, so the last one wins
    }
    Ok(out)
}

async fn list_projects(State(c): State<AppState>) -> ApiResult<Vec<ProjectView>> {
    let projects = c.store.projects()?;
    Ok(Json(projects.into_iter().map(|p| project_view(&c, p)).collect::<Result<_, _>>()?))
}

#[derive(Deserialize)]
struct CreateProject {
    name: String,
    pg_version: Option<u32>,
}

async fn create_project(State(c): State<AppState>, Json(body): Json<CreateProject>) -> Result<Response, ControlError> {
    let project = c.create_project(&body.name, body.pg_version).await?;
    let password = project.password.clone();
    let mut view = serde_json::to_value(project_view(&c, project)?).map_err(anyhow::Error::from)?;
    // Shown once on creation, like a managed database would.
    view["password"] = json!(password);
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn get_project(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ProjectView> {
    let project = c.store.project(&id)?.ok_or_else(|| ControlError::NotFound(format!("project {id}")))?;
    Ok(Json(project_view(&c, project)?))
}

#[derive(Deserialize)]
struct UpdateProject {
    history_retention_s: Option<i64>,
}

async fn update_project(
    State(c): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProject>,
) -> ApiResult<ProjectView> {
    if let Some(s) = body.history_retention_s {
        c.set_history_retention(&id, s).await?;
    }
    get_project(State(c), Path(id)).await
}

async fn delete_project(State(c): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ControlError> {
    c.delete_project(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A point in history: `at` (unix seconds or RFC 3339) or `lsn` (`"0/16B3748"`).
#[derive(Deserialize, Default)]
struct PointInTime {
    at: Option<Value>,
    lsn: Option<String>,
    timeline: Option<u32>,
}

impl PointInTime {
    fn target(&self) -> Result<Option<Target>, ControlError> {
        if let Some(lsn) = &self.lsn {
            let lsn: Lsn = lsn.parse().map_err(|_| ControlError::Invalid(format!("bad LSN {lsn:?}")))?;
            return Ok(Some(Target::Lsn { timeline: self.timeline, lsn }));
        }
        match &self.at {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => {
                Ok(Some(Target::Time(n.as_i64().ok_or_else(|| ControlError::Invalid("bad timestamp".into()))?)))
            }
            Some(Value::String(s)) => {
                let t = humantime::parse_rfc3339_weak(s)
                    .map_err(|e| ControlError::Invalid(format!("bad timestamp {s:?}: {e}")))?;
                let secs = t.duration_since(std::time::UNIX_EPOCH).map_err(anyhow::Error::from)?.as_secs();
                Ok(Some(Target::Time(secs as i64)))
            }
            Some(_) => Err(ControlError::Invalid("`at` must be unix seconds or an RFC 3339 string".into())),
        }
    }
}

#[derive(Deserialize)]
struct CreateBranch {
    name: String,
    /// Parent branch id or name; defaults to `main`.
    parent: Option<String>,
    #[serde(flatten)]
    point: PointInTime,
}

async fn create_branch(
    State(c): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateBranch>,
) -> Result<Response, ControlError> {
    let started = std::time::Instant::now();
    let branch = c.create_branch(&id, &body.name, body.parent.as_deref(), body.point.target()?).await?;
    let project = c.store.project(&id)?.ok_or_else(|| ControlError::NotFound(id.clone()))?;
    let ep = c.store.endpoint_for_branch(&branch.id)?;
    let body = json!({
        "branch": branch,
        "endpoint": ep,
        "connection_uri": ep.as_ref().map(|e| c.connection_uri(&project, &e.id)),
        "took_ms": started.elapsed().as_millis() as u64,
    });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

async fn delete_branch(
    State(c): State<AppState>,
    Path((id, branch)): Path<(String, String)>,
) -> Result<StatusCode, ControlError> {
    c.delete_branch(&id, &branch).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn branch_history(
    State(c): State<AppState>,
    Path((id, branch)): Path<(String, String)>,
) -> ApiResult<crate::history::History> {
    let branch = c.branch_in(&id, &branch)?;
    Ok(Json(c.history(&branch.id).await?))
}

async fn restore_branch(
    State(c): State<AppState>,
    Path((id, branch)): Path<(String, String)>,
    Json(body): Json<PointInTime>,
) -> ApiResult<Value> {
    let started = std::time::Instant::now();
    let target = body.target()?.ok_or_else(|| ControlError::Invalid("give `at` or `lsn`".into()))?;
    let branch = c.branch_in(&id, &branch)?;
    let backup = c.restore_branch(&id, &branch.id, target).await?;
    Ok(Json(json!({ "backup_branch": backup, "took_ms": started.elapsed().as_millis() as u64 })))
}

async fn backup_branch(
    State(c): State<AppState>,
    Path((id, branch)): Path<(String, String)>,
) -> ApiResult<crate::store::BaseImage> {
    let branch = c.branch_in(&id, &branch)?;
    Ok(Json(c.take_base_backup(&branch.id).await?))
}

async fn evict_branch(State(c): State<AppState>, Path((id, branch)): Path<(String, String)>) -> ApiResult<Value> {
    let started = std::time::Instant::now();
    let branch = c.branch_in(&id, &branch)?;
    if let Some(ep) = c.store.endpoint_for_branch(&branch.id)? {
        c.suspend(&ep.id).await?;
    }
    c.evict_branch(&branch.id).await?;
    Ok(Json(json!({ "evicted": branch.id, "took_ms": started.elapsed().as_millis() as u64 })))
}

async fn create_replica(
    State(c): State<AppState>,
    Path((id, branch)): Path<(String, String)>,
) -> Result<Response, ControlError> {
    let ep = c.create_replica(&id, &branch).await?;
    let project = c.store.project(&id)?.ok_or_else(|| ControlError::NotFound(id.clone()))?;
    Ok((StatusCode::CREATED, Json(endpoint_view(&c, &project, ep))).into_response())
}

async fn get_endpoint(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<EndpointView> {
    let endpoint = c.endpoint_or_404(&id)?;
    let project = c.store.project(&endpoint.project_id)?.ok_or_else(|| ControlError::NotFound(id.clone()))?;
    Ok(Json(endpoint_view(&c, &project, endpoint)))
}

async fn delete_endpoint(State(c): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ControlError> {
    c.delete_endpoint(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UpdateEndpoint {
    min_cu: Option<f64>,
    max_cu: Option<f64>,
    suspend_timeout_s: Option<i64>,
}

async fn update_endpoint(
    State(c): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateEndpoint>,
) -> ApiResult<Endpoint> {
    Ok(Json(c.update_endpoint(&id, body.min_cu, body.max_cu, body.suspend_timeout_s).await?))
}

async fn start_endpoint(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let started = std::time::Instant::now();
    c.ensure_running(&id).await?;
    Ok(Json(json!({ "endpoint": c.endpoint_or_404(&id)?, "took_ms": started.elapsed().as_millis() as u64 })))
}

async fn suspend_endpoint(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Endpoint> {
    c.suspend(&id).await?;
    Ok(Json(c.endpoint_or_404(&id)?))
}

#[derive(Deserialize)]
struct QueryBody {
    sql: String,
}

async fn query(State(c): State<AppState>, Path(id): Path<String>, Json(body): Json<QueryBody>) -> ApiResult<Value> {
    let started = std::time::Instant::now();
    let results = c.query(&id, &body.sql).await?;
    Ok(Json(json!({ "results": results, "took_ms": started.elapsed().as_millis() as u64 })))
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

/// `restore_command` target for computes: WAL files of a branch.
async fn internal_wal(
    State(c): State<AppState>,
    Path((branch, file)): Path<(String, String)>,
    Query(q): Query<TokenQuery>,
) -> Response {
    if q.token != c.config.internal_token {
        return StatusCode::FORBIDDEN.into_response();
    }
    match c.wal_file(&branch, &file).await {
        Ok(Some(data)) => ([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(ControlError::Internal(e)) if e.to_string().contains("not a WAL file") => StatusCode::NOT_FOUND.into_response(),
        Err(e) => e.into_response(),
    }
}

// ── usage ──

#[derive(Deserialize)]
struct UsageQuery {
    /// Days to look back (default 30).
    days: Option<i64>,
}

const DAY: i64 = 86_400;
const MONTH_HOURS: f64 = 730.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

async fn usage(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<UsageQuery>) -> ApiResult<Value> {
    let project = c.store.project(&id)?.ok_or_else(|| ControlError::NotFound(format!("project {id}")))?;
    let days = q.days.unwrap_or(30).clamp(1, 365);
    let end = now();
    let since = (end / DAY - days + 1) * DAY;
    let pricing = &c.config.pricing;

    // Compute: CU-seconds per UTC day.
    let mut daily: BTreeMap<i64, f64> = (0..days).map(|d| (since + d * DAY, 0.0)).collect();
    for seg in c.store.segments_since(&id, since)? {
        let mut t = seg.started_at.max(since);
        let stop = seg.ended_at.unwrap_or(end).min(end);
        while t < stop {
            let day = t / DAY * DAY;
            let next = (day + DAY).min(stop);
            *daily.entry(day).or_default() += seg.cu * (next - t) as f64;
            t = next;
        }
    }
    let cu_hours: f64 = daily.values().sum::<f64>() / 3600.0;

    // Storage: integrate samples per (branch, tier) over time → GiB-months.
    let samples = c.store.storage_since(&id, since)?;
    let mut series: HashMap<(&str, &str), Vec<(i64, i64)>> = HashMap::new();
    for s in &samples {
        series.entry((&s.branch_id, &s.tier)).or_default().push((s.at, s.bytes));
    }
    let mut tiers: HashMap<&str, (f64, i64)> = HashMap::new(); // tier → (GiB-seconds, current bytes)
    for ((_, tier), points) in &series {
        let t = tiers.entry(tier).or_default();
        for w in points.windows(2) {
            t.0 += w[0].1 as f64 / GIB * (w[1].0 - w[0].0) as f64;
        }
        if let Some(&(at, bytes)) = points.last() {
            t.0 += bytes as f64 / GIB * (end - at) as f64;
            t.1 += bytes;
        }
    }
    let month_s = MONTH_HOURS * 3600.0;
    let (local_gib_s, local_bytes) = tiers.get("local").copied().unwrap_or_default();
    let (s3_gib_s, s3_bytes) = tiers.get("s3").copied().unwrap_or_default();
    let local_usd = local_gib_s / month_s * pricing.usd_per_gb_month;
    let s3_usd = s3_gib_s / month_s * pricing.usd_per_gb_month_s3;

    let compute_usd = cu_hours * pricing.usd_per_cu_hour;
    let storage_usd = local_usd + s3_usd;

    // Monthly projection from the time the project has actually existed in this window.
    let observed_hours = (end - since.max(project.created_at)) as f64 / 3600.0;
    let scale = MONTH_HOURS / observed_hours.max(1.0);
    let storage_month_usd = local_bytes as f64 / GIB * pricing.usd_per_gb_month
        + s3_bytes as f64 / GIB * pricing.usd_per_gb_month_s3;

    // The same peak sizes provisioned 24/7 with all data on block storage, as on a classic
    // managed database.
    let endpoints = c.store.endpoints(&id)?;
    let always_on_cu: f64 = endpoints.iter().map(|e| e.max_cu).sum();
    let always_on_usd = always_on_cu * MONTH_HOURS * pricing.usd_per_cu_hour
        + (local_bytes + s3_bytes) as f64 / GIB * pricing.usd_per_gb_month;

    Ok(Json(json!({
        "period": { "start": since, "end": end, "days": days },
        "compute": {
            "cu_hours": cu_hours,
            "usd": compute_usd,
            "active_hours": daily.values().filter(|v| **v > 0.0).count(),
        },
        "storage": {
            "current_bytes": local_bytes + s3_bytes,
            "local_bytes": local_bytes,
            "object_bytes": s3_bytes,
            "gib_months": (local_gib_s + s3_gib_s) / month_s,
            "usd": storage_usd,
        },
        "total_usd": compute_usd + storage_usd,
        "projected_month_usd": compute_usd * scale + storage_month_usd,
        "always_on_month_usd": always_on_usd,
        "pricing": pricing,
        "daily": daily.iter().map(|(day, cu_s)| json!({ "day": day, "cu_hours": cu_s / 3600.0 })).collect::<Vec<_>>(),
        "endpoints": endpoints.iter().map(|e| json!({
            "id": e.id, "branch_id": e.branch_id, "kind": e.kind, "state": e.state, "cu": e.cu,
            "min_cu": e.min_cu, "max_cu": e.max_cu, "connections": c.connections(&e.id),
        })).collect::<Vec<_>>(),
    })))
}

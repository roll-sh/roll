//! Many tenants in one cell: each has its own database and engine, and all of them share the
//! cell's log (so their writes share commits), its node and its HTTP connection pool.
//!
//! - `/v1/tasks/…` is the default tenant: a single-tenant deployment never sees the rest.
//! - `/t/<tenant>/v1/tasks/…` is any other tenant. Its API takes only its own API keys (it has
//!   no admin token), so a hosted tenant's API is never open.
//! - `/_roll/tenants` manages tenants, with the node's admin token:
//!   `GET` lists, `POST {"id"}` creates (and returns a first API key, `token`),
//!   `DELETE /_roll/tenants/<id>` deletes with all its data, and
//!   `POST /_roll/tenants/<id>/keys {"name"}` mints another key.

use crate::engine::{Config, Tasks};
use crate::store::log::{DEFAULT_TENANT, Log, LogStore, TenantExists, valid_tenant_id};
use crate::{Worker, start};
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use tower::ServiceExt;
use tracing::{info, warn};

/// Idle tenants re-check their lanes at most this often; new work wakes them at once (the API
/// and the worker share the node).
const TENANT_IDLE_POLL_MS: i64 = 10_000;

struct Runtime {
    tasks: Arc<Tasks>,
    worker: Mutex<Option<Worker>>,
    api: Router,
}

pub struct Cell {
    log: Arc<Log>,
    base: Config,
    http: reqwest::Client,
    tenants: RwLock<BTreeMap<String, Arc<Runtime>>>,
}

impl Cell {
    /// Start an engine for every tenant in the log (creating the default tenant if new).
    pub async fn start(log: Arc<Log>, base: Config) -> anyhow::Result<Arc<Self>> {
        let http = Tasks::http_client(&base)?;
        let cell = Arc::new(Self { log: log.clone(), base, http, tenants: RwLock::new(BTreeMap::new()) });
        if log.tenant(DEFAULT_TENANT).is_none() {
            log.create_tenant(DEFAULT_TENANT).await?;
        }
        let ids = log.tenant_ids();
        let started: Vec<anyhow::Result<()>> = futures::stream::iter(ids)
            .map(|id| {
                let cell = cell.clone();
                async move {
                    let store = cell.log.tenant(&id).ok_or_else(|| anyhow::anyhow!("tenant {id} vanished"))?;
                    cell.run(store).await.map(|_| ())
                }
            })
            .buffer_unordered(32)
            .collect()
            .await;
        for r in started {
            r?;
        }
        info!(tenants = cell.tenants.read().unwrap().len(), "tenants started");
        Ok(cell)
    }

    fn config_for(&self, tenant: &str) -> Config {
        let mut c = self.base.clone();
        if tenant != DEFAULT_TENANT {
            c.token = None;
            c.require_auth = true;
            c.hosted = true;
            c.poll_ms = c.poll_ms.max(TENANT_IDLE_POLL_MS);
        }
        c
    }

    async fn run(&self, store: Arc<LogStore>) -> anyhow::Result<Arc<Runtime>> {
        let id = store.tenant_id().to_string();
        let tasks = Tasks::with_http(self.config_for(&id), store, self.http.clone()).await?;
        let rt = Arc::new(Runtime { worker: Mutex::new(Some(start(tasks.clone()))), api: crate::api::router(tasks.clone()), tasks });
        self.tenants.write().unwrap().insert(id, rt.clone());
        Ok(rt)
    }

    fn runtime(&self, tenant: &str) -> Option<Arc<Runtime>> {
        self.tenants.read().unwrap().get(tenant).cloned()
    }

    pub fn tenant_ids(&self) -> Vec<String> {
        self.tenants.read().unwrap().keys().cloned().collect()
    }

    /// Create a tenant; returns its first API key.
    pub async fn create(&self, id: &str) -> anyhow::Result<String> {
        let store = self.log.create_tenant(id).await?;
        let rt = self.run(store).await?;
        let (_, token) = rt.tasks.create_api_key("default").await?;
        info!(tenant = id, "tenant created");
        Ok(token)
    }

    /// Stop a tenant's engine and delete it with all its data.
    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        anyhow::ensure!(id != DEFAULT_TENANT, "the default tenant can't be deleted");
        let Some(rt) = self.tenants.write().unwrap().remove(id) else { return Ok(false) };
        if let Some(w) = rt.worker.lock().unwrap().take() {
            w.kill();
        }
        let deleted = self.log.drop_tenant(id).await?;
        info!(tenant = id, "tenant deleted");
        Ok(deleted)
    }

    fn runtimes(&self) -> Vec<Arc<Runtime>> {
        self.tenants.read().unwrap().values().cloned().collect()
    }

    /// Drain every tenant's deliveries.
    pub async fn shutdown(&self) {
        let rts = self.runtimes();
        futures::future::join_all(rts.iter().map(|rt| async move {
            let w = rt.worker.lock().unwrap().take();
            if let Some(w) = w {
                w.shutdown().await;
            }
        }))
        .await;
    }

    /// Record every tenant's usage.
    pub async fn flush_usage(&self) {
        let rts = self.runtimes();
        futures::future::join_all(rts.iter().map(|rt| async move {
            if let Err(e) = rt.tasks.flush_usage().await {
                warn!(error = %e, "flushing usage");
            }
        }))
        .await;
    }

    /// Stop every engine at once, as a crash would.
    pub fn kill(&self) {
        for rt in self.runtimes() {
            if let Some(w) = rt.worker.lock().unwrap().take() {
                w.kill();
            }
        }
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/_roll/tenants", get(list).post(create))
            .route("/_roll/tenants/{id}", axum::routing::delete(delete))
            .route("/_roll/tenants/{id}/keys", post(new_key))
            .route("/t/{id}/{*rest}", any(tenant_api))
            .fallback(default_api)
            .with_state(self)
    }

    fn is_admin(&self, headers: &axum::http::HeaderMap) -> bool {
        let Some(token) = &self.base.token else { return true };
        let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        bearer.is_some_and(|b| constant_eq(b.as_bytes(), token.as_bytes()))
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "the node's admin token is required")
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

type S = State<Arc<Cell>>;

async fn default_api(State(cell): S, req: Request) -> Response {
    match cell.runtime(DEFAULT_TENANT) {
        Some(rt) => rt.api.clone().oneshot(req).await.into_response(),
        None => error(StatusCode::SERVICE_UNAVAILABLE, "starting"),
    }
}

async fn tenant_api(State(cell): S, Path((id, rest)): Path<(String, String)>, req: Request) -> Response {
    let Some(rt) = cell.runtime(&id) else { return error(StatusCode::NOT_FOUND, format!("tenant {id} not found")) };
    let path = match req.uri().query() {
        Some(q) => format!("/{rest}?{q}"),
        None => format!("/{rest}"),
    };
    let Ok(uri) = path.parse::<Uri>() else { return error(StatusCode::BAD_REQUEST, "bad path") };
    // A fresh request: this route's own path parameters must not reach the tenant's router.
    let (parts, body) = req.into_parts();
    let mut inner = Request::new(body);
    *inner.method_mut() = parts.method;
    *inner.uri_mut() = uri;
    *inner.version_mut() = parts.version;
    *inner.headers_mut() = parts.headers;
    rt.api.clone().oneshot(inner).await.into_response()
}

async fn list(State(cell): S, headers: axum::http::HeaderMap) -> Response {
    if !cell.is_admin(&headers) {
        return unauthorized();
    }
    let stats = cell.log.stats();
    Json(json!({ "tenants": cell.tenant_ids(), "log": stats })).into_response()
}

#[derive(serde::Deserialize)]
struct NewTenant {
    id: String,
}

async fn create(State(cell): S, headers: axum::http::HeaderMap, Json(b): Json<NewTenant>) -> Response {
    if !cell.is_admin(&headers) {
        return unauthorized();
    }
    if !valid_tenant_id(&b.id) {
        return error(StatusCode::BAD_REQUEST, "tenant ids are 1-63 of a-z, 0-9 and inner '-'");
    }
    let exists = || error(StatusCode::CONFLICT, format!("tenant {} already exists", b.id));
    if cell.log.tenant(&b.id).is_some() {
        return exists();
    }
    match cell.create(&b.id).await {
        Ok(token) => (StatusCode::CREATED, Json(json!({ "id": b.id, "path": format!("/t/{}/v1/tasks", b.id), "token": token }))).into_response(),
        // Lost a race with another create of the same id.
        Err(e) if e.is::<TenantExists>() => exists(),
        Err(e) => internal(e),
    }
}

async fn delete(State(cell): S, headers: axum::http::HeaderMap, Path(id): Path<String>) -> Response {
    if !cell.is_admin(&headers) {
        return unauthorized();
    }
    if id == DEFAULT_TENANT {
        return error(StatusCode::BAD_REQUEST, "the default tenant can't be deleted");
    }
    match cell.delete(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, format!("tenant {id} not found")),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct NewKey {
    name: String,
}

async fn new_key(State(cell): S, headers: axum::http::HeaderMap, Path(id): Path<String>, Json(b): Json<NewKey>) -> Response {
    if !cell.is_admin(&headers) {
        return unauthorized();
    }
    let Some(rt) = cell.runtime(&id) else { return error(StatusCode::NOT_FOUND, format!("tenant {id} not found")) };
    match rt.tasks.create_api_key(&b.name).await {
        Ok((key, token)) => (StatusCode::CREATED, Json(json!({ "id": key.id, "name": key.name, "token": token }))).into_response(),
        Err(e) => e.into_response(),
    }
}

fn internal(e: anyhow::Error) -> Response {
    crate::engine::TaskError::Internal(e).into_response()
}

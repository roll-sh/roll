//! `/v1/previews`: JSON in and out, errors as `{"error": "..."}` (docs/previews.md, "HTTP
//! contract"). The routes only change desired state and ask the engine to converge; branching,
//! deploying and deleting happen in the engine.

use crate::cost::Cost;
use crate::{Engine, Error, Preview, State};
use axum::extract::{Path, Query, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;

type S = AxState<Arc<Engine>>;

pub fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/v1/previews", get(list).post(create))
        .route("/v1/previews/{id}", get(one).delete(delete))
        .route("/v1/previews/{id}/refresh", post(refresh))
        .route("/v1/previews/{id}/sleep", post(sleep))
        .route("/v1/previews/{id}/wake", post(wake))
        .route("/v1/previews/{id}/reset", post(reset))
        .route("/v1/previews/{id}/logs", get(logs))
        .route("/v1/previews/{id}/activity", get(activity))
        .route("/v1/previews/projects/{project}/settings", get(settings).put(put_settings))
        .route("/v1/previews/projects/{project}/usage", get(usage))
        .with_state(engine)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Db(_) | Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Run `f` off the async runtime: the store blocks on its database.
async fn blocking<T: Send + 'static>(e: Arc<Engine>, f: impl FnOnce(&Engine) -> crate::Result<T> + Send + 'static) -> crate::Result<T> {
    tokio::task::spawn_blocking(move || f(&e)).await.map_err(|e| Error::Internal(e.to_string()))?
}

fn body<T: DeserializeOwned>(b: &Bytes) -> crate::Result<T> {
    let b: &[u8] = if b.is_empty() { b"{}" } else { b };
    serde_json::from_slice(b).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))
}

fn check_id(id: &str) -> crate::Result<()> {
    match id.strip_prefix("pv_") {
        Some(rest) if !rest.is_empty() && rest.len() <= 32 && rest.bytes().all(|b| b.is_ascii_alphanumeric()) => Ok(()),
        _ => Err(Error::NotFound(format!("no preview {id}"))),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    project: Option<String>,
    state: Option<String>,
}

async fn list(AxState(e): S, Query(q): Query<ListQuery>) -> crate::Result<Json<Vec<Preview>>> {
    let state = q.state.as_deref().map(State::parse).transpose()?;
    blocking(e, move |e| {
        if let Some(p) = &q.project {
            e.workspace_project(p)?;
        }
        let rows = e.store.rows(q.project.as_deref())?;
        rows.into_iter().filter(|r| state.is_none_or(|s| r.state == s)).map(|r| e.store.view(r)).collect::<crate::Result<Vec<_>>>()
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    project: String,
    repo: String,
    branch: Option<String>,
    change: Option<String>,
}

async fn create(AxState(e): S, b: Bytes) -> crate::Result<Response> {
    let b: CreateBody = body(&b)?;
    if b.branch.is_some() == b.change.is_some() {
        return Err(Error::BadRequest("give one of branch or change".into()));
    }
    let (p, made) = e.create(&b.project, &b.repo, b.branch.as_deref(), b.change.as_deref(), "operator").await?;
    let status = if made { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, Json(p)).into_response())
}

async fn one(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    blocking(e, move |e| e.store.preview(&id)).await.map(Json)
}

async fn delete(AxState(e): S, Path(id): Path<String>) -> crate::Result<Response> {
    check_id(&id)?;
    let p = blocking(e, move |e| e.delete(&id)).await?;
    Ok((StatusCode::ACCEPTED, Json(p)).into_response())
}

async fn refresh(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    blocking(e, move |e| e.refresh(&id)).await.map(Json)
}

async fn sleep(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    e.sleep(&id).await.map(Json)
}

async fn wake(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    e.wake(&id).await.map(Json)
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ResetBody {
    resources: Vec<String>,
}

async fn reset(AxState(e): S, Path(id): Path<String>, b: Bytes) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    let b: ResetBody = body(&b)?;
    blocking(e, move |e| e.reset(&id, b.resources)).await.map(Json)
}

#[derive(Deserialize)]
struct LogsQuery {
    service: Option<String>,
    lines: Option<usize>,
}

async fn logs(AxState(e): S, Path(id): Path<String>, Query(q): Query<LogsQuery>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    let (service, lines) = e.logs(&id, q.service.as_deref(), q.lines.unwrap_or(200).clamp(1, 2000)).await?;
    Ok(Json(json!({ "service": service, "lines": lines })))
}

async fn activity(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    blocking(e, move |e| {
        e.store.row(&id)?;
        Ok(json!(e.store.activity(&id)?))
    })
    .await
    .map(Json)
}

async fn settings(AxState(e): S, Path(project): Path<String>) -> crate::Result<Json<crate::Settings>> {
    blocking(e, move |e| {
        e.workspace_project(&project)?;
        e.store.settings(&project)
    })
    .await
    .map(Json)
}

async fn put_settings(AxState(e): S, Path(project): Path<String>, b: Bytes) -> crate::Result<Json<crate::Settings>> {
    let patch: Value = body(&b)?;
    let s = blocking(e.clone(), move |e| {
        e.workspace_project(&project)?;
        let s = e.store.settings(&project)?.patched(&patch)?;
        e.store.put_settings(&project, &s)
    })
    .await?;
    e.kick();
    Ok(Json(s))
}

async fn usage(AxState(e): S, Path(project): Path<String>) -> crate::Result<Json<Value>> {
    blocking(e, move |e| {
        e.workspace_project(&project)?;
        let mut total = Cost::default();
        let mut n = 0;
        for r in e.store.rows(Some(&project))? {
            n += 1;
            total.add(&e.store.view(r)?.cost);
        }
        let t = total.rounded();
        Ok(json!({
            "previews": n,
            "sleeping_usd_month": t.sleeping_usd_month,
            "active_usd_hour": t.active_usd_hour,
            "month_to_date_usd": t.month_to_date_usd,
            "bytes": t.bytes,
        }))
    })
    .await
    .map(Json)
}

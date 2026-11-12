//! `/v1/workspace`: JSON in and out, errors as `{"error": "..."}` (docs/workspace.md).

use crate::{Error, Kind, Workspace};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

type S = State<Arc<Workspace>>;

pub fn router(ws: Arc<Workspace>) -> Router {
    Router::new()
        .route("/v1/workspace/projects", get(list).post(create))
        .route("/v1/workspace/projects/{id}", get(get_one).patch(rename).delete(delete))
        .route("/v1/workspace/projects/{id}/resources/{kind}/{ref}", put(place).delete(remove))
        .with_state(ws)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Db(_) | Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(serde_json::json!({ "error": self.to_string() }))).into_response()
    }
}

/// Run `f` off the async runtime: the store blocks on its database.
async fn blocking<T: Send + 'static>(ws: Arc<Workspace>, f: impl FnOnce(&Workspace) -> crate::Result<T> + Send + 'static) -> crate::Result<T> {
    tokio::task::spawn_blocking(move || f(&ws)).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// The body as JSON, a 400 in the API's shape if it isn't.
fn body<T: DeserializeOwned>(b: &Bytes) -> crate::Result<T> {
    serde_json::from_slice(b).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
struct PlaceBody {
    x: f64,
    y: f64,
}

async fn list(State(ws): S) -> crate::Result<Response> {
    Ok(Json(blocking(ws, |ws| ws.list()).await?).into_response())
}

async fn create(State(ws): S, b: Bytes) -> crate::Result<Response> {
    let NameBody { name } = body(&b)?;
    let project = blocking(ws, move |ws| ws.create(&name)).await?;
    Ok((StatusCode::CREATED, Json(project)).into_response())
}

async fn get_one(State(ws): S, Path(id): Path<String>) -> crate::Result<Response> {
    Ok(Json(blocking(ws, move |ws| ws.get(&id)).await?).into_response())
}

async fn rename(State(ws): S, Path(id): Path<String>, b: Bytes) -> crate::Result<Response> {
    let NameBody { name } = body(&b)?;
    Ok(Json(blocking(ws, move |ws| ws.rename(&id, &name)).await?).into_response())
}

async fn delete(State(ws): S, Path(id): Path<String>) -> crate::Result<StatusCode> {
    blocking(ws, move |ws| ws.delete(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn place(State(ws): S, Path((id, kind, r#ref)): Path<(String, String, String)>, b: Bytes) -> crate::Result<Response> {
    let kind = Kind::parse(&kind)?;
    let PlaceBody { x, y } = body(&b)?;
    Ok(Json(blocking(ws, move |ws| ws.place(&id, kind, &r#ref, x, y)).await?).into_response())
}

async fn remove(State(ws): S, Path((id, kind, r#ref)): Path<(String, String, String)>) -> crate::Result<StatusCode> {
    let kind = Kind::parse(&kind)?;
    blocking(ws, move |ws| ws.remove(&id, kind, &r#ref)).await?;
    Ok(StatusCode::NO_CONTENT)
}

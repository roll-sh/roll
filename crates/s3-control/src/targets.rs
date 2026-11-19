//! `/v1/storage/replication-targets`: S3 buckets outside roll that replication can go to
//! (`s3_store::targets`). The secret access key is write-only: it's sealed in the metadata and
//! never shown again. Either credential may be a `roll://` reference to a roll secret instead,
//! kept as written and shown (it names a value, it isn't one).
//!
//! Only the operator (its token, an admin's session) sets a reference, or changes a target that
//! has one: a scoped token allowed to manage targets could otherwise point a reference at any key
//! roll secrets holds for a bucket and send it to an endpoint of its choosing.

use crate::control::{Control, ControlError};
use axum::extract::{Path, State};
use axum::http::Extensions;
use axum::routing::get;
use axum::{Json, Router};
use s3_store::{ReplicationTarget, TargetSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type AppState = Arc<Control>;
type ApiResult<T> = Result<Json<T>, ControlError>;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/storage/replication-targets", get(list).post(create))
        .route("/v1/storage/replication-targets/{name}", get(show).put(replace).delete(remove))
}

/// A target as shown: never its secret.
pub(crate) fn view(t: &ReplicationTarget) -> Value {
    json!({
        "name": t.name,
        "endpoint": t.endpoint,
        "region": t.region,
        "access_key_id": t.access_key_id,
        "secret_ref": t.secret_ref,
        "credentials": if !t.references().is_empty() { "reference" } else if t.access_key_id.is_some() { "access_key" } else { "deployment" },
        "path_style": t.path_style,
        "buckets": t.buckets,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
    })
}

#[derive(Deserialize)]
struct Create {
    name: String,
    #[serde(flatten)]
    spec: TargetSpec,
}

async fn list(State(c): State<AppState>) -> ApiResult<Vec<Value>> {
    Ok(Json(c.store.replication_targets()?.iter().map(view).collect()))
}

/// A scoped token (a roll-access grant) may not set a `roll://` reference, nor change a target
/// whose credentials are references (see the module doc).
fn operator_for_references(ext: &Extensions, spec: &TargetSpec, old: Option<&ReplicationTarget>) -> Result<(), ControlError> {
    let refers = spec.access_key_id.as_deref().is_some_and(s3_store::targets::is_reference) || spec.secret_access_key.as_ref().is_some_and(|s| s3_store::targets::is_reference(s.expose())) || old.is_some_and(|o| !o.references().is_empty());
    if refers && roll_auth::Grant::on(ext).is_some() {
        return Err(ControlError::Forbidden("only the operator sets a replication target's roll:// references or changes a target that has them".into()));
    }
    Ok(())
}

async fn create(State(c): State<AppState>, ext: Extensions, crate::api::JsonBody(r): crate::api::JsonBody<Create>) -> ApiResult<Value> {
    operator_for_references(&ext, &r.spec, None)?;
    let t = c.store.put_replication_target(&r.name, &r.spec, true).await?;
    Ok(Json(view(&t)))
}

async fn show(State(c): State<AppState>, Path(name): Path<String>) -> ApiResult<Value> {
    let mut t = c.store.replication_target(&name)?.ok_or_else(|| ControlError::NotFound(format!("no replication target {name}")))?;
    t.secret = None;
    Ok(Json(view(&t)))
}

async fn replace(State(c): State<AppState>, ext: Extensions, Path(name): Path<String>, crate::api::JsonBody(spec): crate::api::JsonBody<TargetSpec>) -> ApiResult<Value> {
    let old = c.store.replication_targets()?.into_iter().find(|t| t.name == name);
    operator_for_references(&ext, &spec, old.as_ref())?;
    let t = c.store.put_replication_target(&name, &spec, false).await?;
    Ok(Json(view(&t)))
}

async fn remove(State(c): State<AppState>, Path(name): Path<String>) -> ApiResult<Value> {
    c.store.delete_replication_target(&name).await?;
    Ok(Json(json!({ "deleted": name })))
}

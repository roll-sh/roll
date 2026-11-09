//! The storage developer API's settings (`/v1/storage/api`): the account's endpoint, its anon
//! and service keys, its auth providers, and rule presets for buckets.
//!
//! - `GET /v1/storage/api`: the view below.
//! - `POST /v1/storage/api/keys/rotate`: new keys (the old ones stop working at once).
//! - `PUT /v1/storage/api/auth`: `{jwt_secret, jwt_issuer, jwt_audience, oidc}` (omitted:
//!   unchanged, `null`: removed). The secret is write-only.
//! - `GET /v1/storage/buckets/{name}/rules/{preset}`: a preset's policy for the bucket.
//! - `POST /v1/storage/buckets/{name}/rules` `{preset}`: set it as the bucket's policy.
//!
//! These are the operator's (a signed-in IAM user gets `OperatorOnly`), except the rules, which
//! are `s3:PutBucketPolicy` on the bucket.

use crate::api::{Acct, JsonBody};
use crate::control::{Control, ControlError};
use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use s3_store::{ApiProject, AuthUpdate};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type AppState = Arc<Control>;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/storage/api", get(show))
        .route("/v1/storage/api/keys/rotate", post(rotate))
        .route("/v1/storage/api/auth", put(set_auth))
        .route("/v1/storage/buckets/{name}/rules/{preset}", get(preview))
        .route("/v1/storage/buckets/{name}/rules", post(apply))
}

fn view(c: &Control, p: &ApiProject) -> Value {
    json!({
        "account_id": p.account,
        "endpoint": c.api_endpoint.get(),
        // Public and signed reads (and image renditions) through the CDN, when it's set up.
        "cdn_endpoint": c.api_cdn_url(),
        "anon_key": p.anon_key.expose(),
        "service_key": p.service_key.expose(),
        "key_generation": p.generation,
        "keys_issued_at": p.issued_at,
        "auth": {
            "jwt_secret_set": p.jwt_secret.is_some(),
            "jwt_issuer": p.jwt_issuer,
            "jwt_audience": p.jwt_audience,
            "oidc": p.oidc,
        },
        "presets": s3_server::app::PRESETS,
    })
}

async fn show(State(c): State<AppState>, Acct(a): Acct) -> Result<Json<Value>, ControlError> {
    let p = c.store.api_project(&a).await?;
    Ok(Json(view(&c, &p)))
}

async fn rotate(State(c): State<AppState>, Acct(a): Acct) -> Result<Json<Value>, ControlError> {
    let p = c.store.rotate_api_keys(&a).await?;
    Ok(Json(view(&c, &p)))
}

async fn set_auth(State(c): State<AppState>, Acct(a): Acct, JsonBody(u): JsonBody<AuthUpdate>) -> Result<Json<Value>, ControlError> {
    let p = c.store.set_api_auth(&a, u).await?;
    Ok(Json(view(&c, &p)))
}

fn preset(bucket: &str, id: &str) -> Result<Value, ControlError> {
    s3_server::app::preset_policy(id, bucket).ok_or_else(|| ControlError::Invalid(format!("unknown preset {id} (public-read, authenticated-read, user-folders, owner-only)")))
}

async fn preview(State(c): State<AppState>, Acct(a): Acct, Path((name, id)): Path<(String, String)>) -> Result<Json<Value>, ControlError> {
    c.bucket_in(&a, &name)?;
    Ok(Json(json!({ "preset": id, "policy": preset(&name, &id)? })))
}

#[derive(Deserialize)]
struct Apply {
    preset: String,
}

async fn apply(State(c): State<AppState>, Acct(a): Acct, Path(name): Path<String>, JsonBody(r): JsonBody<Apply>) -> Result<Json<Value>, ControlError> {
    let b = c.bucket_in(&a, &name)?;
    let policy = preset(&name, &r.preset)?;
    s3_server::app::put_bucket_policy(&c.store, &name, Some(&policy.to_string())).await.map_err(|e| match e {
        s3_server::app::PolicyError::Invalid(m) => ControlError::Invalid(format!("invalid bucket policy: {m}")),
        s3_server::app::PolicyError::Store(e) => e.into(),
    })?;
    let b = c.store.bucket(&b.name)?;
    Ok(Json(c.bucket_view(&b)))
}

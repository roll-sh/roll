//! roll's reviewer routes (docs/code/http-api.md, "roll's reviewer"), merged into
//! [`crate::api::router`].

use crate::api::Body as B;
use crate::{Caller, Code, Error, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Code>>;

pub fn routes() -> Router<Arc<Code>> {
    let repo = |p: &str| format!("/v1/code/repos/{{owner}}/{{repo}}{p}");
    Router::new()
        .route("/v1/code/pricing", get(pricing))
        .route(&repo("/reviewer"), get(reviewer).patch(patch_reviewer))
        .route(&repo("/learnings"), get(learnings).post(add_learning))
        .route(&repo("/learnings/{id}"), delete(delete_learning))
        .route(&repo("/search"), get(search))
        .route("/v1/code/changes/{id}/ai-review", post(ai_review))
        .route("/v1/code/changes/{id}/ai-jobs", get(ai_jobs))
        .route("/v1/code/owners/{owner}/review-provider", get(provider).put(set_provider).delete(clear_provider))
        .route("/v1/code/owners/{owner}/billing", get(billing).put(set_billing))
        .route("/v1/code/owners/{owner}/usage", get(usage))
}

async fn pricing(State(code): S) -> Json<Value> {
    Json(code_review::pricing::pricing_view(&code.config.review.rates))
}

async fn reviewer(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    let mut v = code.reviewer_view(&r).await?;
    // A public repo's readers see how its reviewer is set up, but not the custom prompt the
    // owner wrote through the API (one kept in the repo, `prompt_file`, is public anyway).
    if code.check_write(&caller, &r).is_err() {
        for k in ["settings", "effective"] {
            if let Some(p) = v.get_mut(k).and_then(|x| x.get_mut("prompt"))
                && !p.is_null()
            {
                *p = json!("(hidden)");
            }
        }
    }
    Ok(Json(v))
}

async fn patch_reviewer(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, B(b): B) -> Result<Json<Value>> {
    let r = code.writable(&caller, &owner, &repo)?;
    Ok(Json(code.patch_reviewer_settings(&r, &caller, &b).await?))
}

async fn learnings(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(serde_json::to_value(code.db.learnings(&r.id)?).unwrap_or_default()))
}

async fn add_learning(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, B(b): B) -> Result<(StatusCode, Json<Value>)> {
    let r = code.readable(&caller, &owner, &repo)?;
    let l = code.add_learning(&r, &caller, &b)?;
    Ok((StatusCode::CREATED, Json(serde_json::to_value(l).unwrap_or_default())))
}

async fn delete_learning(State(code): S, caller: Caller, Path((owner, repo, id)): Path<(String, String, String)>) -> Result<StatusCode> {
    let r = code.readable(&caller, &owner, &repo)?;
    code.delete_learning(&r, &caller, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    regex: Option<bool>,
    case: Option<bool>,
    path: Option<String>,
    limit: Option<usize>,
    rev: Option<String>,
}

async fn search(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, Query(q): Query<SearchQuery>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    let text = q.q.unwrap_or_default();
    Ok(Json(code.search_code(&r, &text, q.regex.unwrap_or(false), q.case.unwrap_or(false), q.path.as_deref().filter(|p| !p.is_empty()), q.limit.unwrap_or(100), q.rev.as_deref()).await?))
}

async fn ai_review(State(code): S, caller: Caller, Path(id): Path<String>, B(b): B) -> Result<(StatusCode, Json<Value>)> {
    let (r, c) = code.readable_change(&caller, &id)?;
    let job = code.request_ai_review(&caller, &r, &c, &b).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "job": job }))))
}

async fn ai_jobs(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Value>> {
    let (_, c) = code.readable_change(&caller, &id)?;
    Ok(Json(serde_json::to_value(code.db.change_ai_jobs(&c.id)?).unwrap_or_default()))
}

async fn provider(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    Ok(Json(code.owner_provider_view(&caller, &owner)?))
}

async fn set_provider(State(code): S, caller: Caller, Path(owner): Path<String>, B(b): B) -> Result<Json<Value>> {
    if matches!(caller, Caller::Anonymous) {
        return Err(Error::Unauthorized);
    }
    Ok(Json(code.set_owner_provider(&caller, &owner, &b)?))
}

async fn clear_provider(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<StatusCode> {
    if matches!(caller, Caller::Anonymous) {
        return Err(Error::Unauthorized);
    }
    code.clear_owner_provider(&caller, &owner)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn billing(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    Ok(Json(code.billing_view(&caller, &owner)?))
}

async fn set_billing(State(code): S, caller: Caller, Path(owner): Path<String>, B(b): B) -> Result<Json<Value>> {
    if matches!(caller, Caller::Anonymous) {
        return Err(Error::Unauthorized);
    }
    Ok(Json(code.set_billing(&caller, &owner, &b)?))
}

#[derive(Deserialize)]
struct MonthQuery {
    month: Option<String>,
}

async fn usage(State(code): S, caller: Caller, Path(owner): Path<String>, Query(q): Query<MonthQuery>) -> Result<Json<Value>> {
    Ok(Json(code.usage_view(&caller, &owner, q.month.as_deref())?))
}

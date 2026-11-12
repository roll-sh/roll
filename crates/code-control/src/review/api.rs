//! Review routes (docs/code/http-api.md, "Reviews"), merged into [`crate::api::router`].

use super::{Origin, Reviewer, ThreadInput};
use crate::{Caller, Code, Error, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use std::sync::Arc;

type S = State<Arc<Code>>;
use crate::api::Body as B;

pub fn routes() -> Router<Arc<Code>> {
    let repo = |p: &str| format!("/v1/code/repos/{{owner}}/{{repo}}{p}");
    Router::new()
        .route(&repo("/review-settings"), get(settings).patch(patch_settings))
        .route("/v1/code/changes/{id}/reviews", get(list_reviews).post(submit))
        .route("/v1/code/changes/{id}/reviews/{rid}/dismiss", post(dismiss))
        .route("/v1/code/changes/{id}/threads", get(list_threads).post(create_thread))
        .route("/v1/code/changes/{id}/review-status", get(status))
        .route("/v1/code/changes/{id}/review-requests", get(list_requests).post(request))
        .route("/v1/code/threads/{id}", get(get_thread))
        .route("/v1/code/threads/{id}/comments", post(reply))
        .route("/v1/code/threads/{id}/resolve", post(resolve))
        .route("/v1/code/threads/{id}/unresolve", post(unresolve))
        .route("/v1/code/threads/{id}/apply", post(apply))
        .route("/v1/code/owners/{owner}/review-apps", get(list_apps).post(create_app))
        .route("/v1/code/review-apps/{id}", get(get_app).patch(update_app).delete(delete_app))
        .route("/v1/code/review-apps/{id}/rotate", post(rotate_app))
        .route("/v1/code/review-apps/{id}/deliveries", get(deliveries))
        .route("/v1/code/review-apps/{id}/deliveries/{did}/redeliver", post(redeliver))
}

// ── settings ──

async fn settings(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.review_settings_view(&r).await?))
}

async fn patch_settings(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, B(b): B) -> Result<Json<Value>> {
    let r = code.writable(&caller, &owner, &repo)?;
    Ok(Json(code.patch_review_settings(&r, &caller, &b).await?))
}

// ── reviews ──

async fn list_reviews(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Vec<super::Review>>> {
    let (_, c) = code.readable_change(&caller, &id)?;
    Ok(Json(code.reviews_view(&c)?))
}

async fn submit(State(code): S, caller: Caller, headers: HeaderMap, Path(id): Path<String>, B(b): B) -> Result<(StatusCode, Json<Value>)> {
    let (r, c) = code.readable_change(&caller, &id)?;
    code.check_comment(&caller, &r)?;
    let reviewer = code.reviewer(&caller, &headers)?;
    let decides = b.get("verdict").and_then(Value::as_str).is_some_and(|v| v != "comment" && v != "commented");
    if decides && !Code::may_decide(&caller, &r) {
        return Err(Error::Forbidden(format!("approving or requesting changes on {} needs write access", r.full_name())));
    }
    code.prepare_review(&r, &c).await;
    let mut review = code.submit_review(&r, &c, &reviewer, &b, &Origin::ROLL).await?;
    review.stale = false;
    Ok((StatusCode::CREATED, Json(serde_json::to_value(review).map_err(|e| Error::Internal(e.into()))?)))
}

async fn dismiss(State(code): S, caller: Caller, headers: HeaderMap, Path((id, rid)): Path<(String, String)>, B(b): B) -> Result<Json<super::Review>> {
    let (r, c) = code.readable_change(&caller, &id)?;
    let reviewer = code.reviewer(&caller, &headers)?;
    // Writers dismiss anyone's review; a reviewer can withdraw their own.
    let own = code.db.review(&rid)?.is_some_and(|v| v.reviewer.same(&reviewer));
    if !own {
        code.check_write(&caller, &r)?;
    }
    let reason = b.get("reason").and_then(Value::as_str).unwrap_or("");
    if reason.len() > 2000 {
        return Err(Error::Invalid("reason is longer than 2000 characters".into()));
    }
    Ok(Json(code.dismiss_review(&r, &c, &rid, &reviewer.display(), reason)?))
}

async fn status(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Value>> {
    let (r, c) = code.readable_change(&caller, &id)?;
    Ok(Json(code.review_status_view(&r, &c).await?))
}

// ── requests ──

async fn list_requests(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Vec<Value>>> {
    let (_, c) = code.readable_change(&caller, &id)?;
    Ok(Json(code.review_requests_view(&c)?))
}

async fn request(State(code): S, caller: Caller, Path(id): Path<String>, B(b): B) -> Result<(StatusCode, Json<Vec<Value>>)> {
    let (r, c) = code.writable_change(&caller, &id)?;
    let who = b.get("reviewer").and_then(Value::as_str).ok_or_else(|| Error::Invalid("reviewer is required (@name, agent:name, app:name or roll)".into()))?;
    let mut reviewer = Reviewer::parse(who).ok_or_else(|| Error::Invalid(format!("{who:?} isn't a reviewer")))?;
    if reviewer.kind == super::ReviewerKind::App {
        let app = code
            .apps_for_repo(&r)?
            .into_iter()
            .find(|a| a.name.eq_ignore_ascii_case(&reviewer.name))
            .ok_or_else(|| Error::NotFound(format!("review app {} on {}", reviewer.name, r.full_name())))?;
        reviewer = super::apps::app_reviewer(&app);
    }
    let required = match b.get("required") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(v)) => *v,
        Some(_) => return Err(Error::Invalid("required must be true or false".into())),
    };
    code.request_review(&r, &c, &reviewer, required)?;
    Ok((StatusCode::CREATED, Json(code.review_requests_view(&c)?)))
}

// ── threads ──

async fn list_threads(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Vec<super::Thread>>> {
    let (_, c) = code.readable_change(&caller, &id)?;
    Ok(Json(code.threads_view(&c)?))
}

async fn create_thread(State(code): S, caller: Caller, headers: HeaderMap, Path(id): Path<String>, B(b): B) -> Result<(StatusCode, Json<super::Thread>)> {
    let (r, c) = code.readable_change(&caller, &id)?;
    code.check_comment(&caller, &r)?;
    let reviewer = code.reviewer(&caller, &headers)?;
    let t: ThreadInput = serde_json::from_value(b).map_err(|e| Error::Invalid(e.to_string()))?;
    let th = code.create_thread(&r, &c, &reviewer, &t, &Origin::ROLL).await?;
    if let Some(cm) = th.comments.first() {
        code.ai_on_comment(&caller, &r, &c, &th, &cm.id);
    }
    Ok((StatusCode::CREATED, Json(th)))
}

async fn get_thread(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<super::Thread>> {
    Ok(Json(code.readable_thread(&caller, &id)?.2))
}

async fn reply(State(code): S, caller: Caller, headers: HeaderMap, Path(id): Path<String>, B(b): B) -> Result<(StatusCode, Json<super::Thread>)> {
    let (r, c, th) = code.readable_thread(&caller, &id)?;
    code.check_comment(&caller, &r)?;
    let reviewer = code.reviewer(&caller, &headers)?;
    let body = b.get("body").and_then(Value::as_str).unwrap_or("");
    let th = code.reply(&r, &c, &th, &reviewer, body, &Origin::ROLL)?;
    if let Some(cm) = th.comments.last() {
        code.ai_on_comment(&caller, &r, &c, &th, &cm.id);
    }
    Ok((StatusCode::CREATED, Json(th)))
}

/// Writers resolve any thread; others (apps, readers who comment) only threads they started.
fn may_resolve(code: &Code, caller: &Caller, r: &crate::model::RepoRow, th: &super::Thread, reviewer: &Reviewer) -> Result<()> {
    if Code::owns(caller, &r.owner_id) || th.comments.first().is_some_and(|c| c.author.same(reviewer)) {
        return Ok(());
    }
    code.check_write(caller, r)
}

async fn resolve(State(code): S, caller: Caller, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<super::Thread>> {
    let (r, c, th) = code.readable_thread(&caller, &id)?;
    let reviewer = code.reviewer(&caller, &headers)?;
    may_resolve(&code, &caller, &r, &th, &reviewer)?;
    Ok(Json(code.set_resolved(&r, &c, &th, &reviewer, true)?))
}

async fn unresolve(State(code): S, caller: Caller, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<super::Thread>> {
    let (r, c, th) = code.readable_thread(&caller, &id)?;
    let reviewer = code.reviewer(&caller, &headers)?;
    may_resolve(&code, &caller, &r, &th, &reviewer)?;
    Ok(Json(code.set_resolved(&r, &c, &th, &reviewer, false)?))
}

async fn apply(State(code): S, caller: Caller, headers: HeaderMap, Path(id): Path<String>, B(b): B) -> Result<Json<Value>> {
    let (r, c, th) = code.readable_thread(&caller, &id)?;
    // Applying writes code: writers only.
    code.check_write(&caller, &r)?;
    let reviewer = code.reviewer(&caller, &headers)?;
    let comment = b.get("comment").and_then(Value::as_str);
    Ok(Json(code.apply_suggestion(&r, &caller, &reviewer, &c, &th, comment).await?))
}

// ── review apps ──

async fn list_apps(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Vec<Value>>> {
    Ok(Json(code.list_apps(&caller, &owner)?))
}

async fn create_app(State(code): S, caller: Caller, Path(owner): Path<String>, B(b): B) -> Result<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(code.create_app(&caller, &owner, &b)?)))
}

async fn get_app(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(code.get_app(&caller, &id)?))
}

async fn update_app(State(code): S, caller: Caller, Path(id): Path<String>, B(b): B) -> Result<Json<Value>> {
    Ok(Json(code.update_app(&caller, &id, &b)?))
}

async fn delete_app(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<StatusCode> {
    code.delete_app(&caller, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rotate_app(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(code.rotate_app(&caller, &id)?))
}

async fn deliveries(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Vec<Value>>> {
    Ok(Json(code.app_deliveries(&caller, &id)?))
}

async fn redeliver(State(code): S, caller: Caller, Path((id, did)): Path<(String, String)>) -> Result<Json<Value>> {
    Ok(Json(code.redeliver(&caller, &id, &did)?))
}

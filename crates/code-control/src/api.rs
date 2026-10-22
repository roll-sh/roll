//! HTTP API: `/v1/code/...` (docs/code/http-api.md) and git smart HTTP under `/git/...`.

use crate::git::GitAdapter;
use crate::model::*;
use crate::render;
use crate::service::{WriteFiles, short_ref};
use crate::{Caller, Code, Error, Result, cs_id};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

type S = State<Arc<Code>>;

/// Largest JSON body the API takes (axum's default is 2 MB).
const MAX_BODY: usize = 64 << 20;

/// The token from `Authorization: Bearer …`, or the password of `Authorization: Basic …`.
fn token(parts: &Parts) -> Option<String> {
    use base64::Engine;
    let v = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(value.trim().to_string())
    } else if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD.decode(value.trim()).ok()?;
        let s = String::from_utf8(decoded).ok()?;
        s.split_once(':').map(|(_, p)| p.to_string())
    } else {
        None
    }
}

impl FromRequestParts<Arc<Code>> for Caller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, code: &Arc<Code>) -> Result<Self, Self::Rejection> {
        code.caller(token(parts).as_deref())
    }
}

/// The JSON body, or a 400 that says what's wrong with it.
pub(crate) struct Body(pub(crate) Value);

impl<St: Send + Sync> axum::extract::FromRequest<St> for Body {
    type Rejection = Error;

    async fn from_request(req: axum::extract::Request, state: &St) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state).await.map_err(|e| Error::Invalid(e.to_string()))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Body(json!({})));
        }
        serde_json::from_slice(&bytes).map(Body).map_err(|e| Error::Invalid(format!("invalid JSON: {e}")))
    }
}

/// The query string, or a JSON 400 (axum's own `Query` rejection is plain text).
pub(crate) struct Q<T>(pub(crate) T);

impl<T: serde::de::DeserializeOwned, St: Send + Sync> FromRequestParts<St> for Q<T> {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &St) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state).await.map(|Query(q)| Q(q)).map_err(|e| Error::Invalid(e.body_text()))
    }
}

/// The browser calls the API directly. `Access-Control-Allow-Headers: *` doesn't cover
/// `Authorization`, so requested headers are mirrored.
fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers([header::HeaderName::from_static("x-roll-changeset"), header::ETAG])
        .max_age(Duration::from_secs(3600))
}

pub fn router(code: Arc<Code>) -> Router {
    let repo = |p: &str| format!("/v1/code/repos/{{owner}}/{{repo}}{p}");
    let api = Router::new()
        .route("/v1/code", get(info))
        .route("/v1/code/names/{name}", get(check_name))
        .route("/v1/code/names/{owner}/{repo}", get(check_repo_name))
        .route("/v1/code/owners", get(list_owners).post(create_owner))
        .route("/v1/code/owners/{owner}", get(get_owner).patch(update_owner))
        .route("/v1/code/owners/{owner}/tokens", post(create_token).get(list_tokens))
        .route("/v1/code/owners/{owner}/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/v1/code/repos", get(list_repos).post(create_repo))
        .route(&repo(""), get(get_repo).patch(update_repo).delete(delete_repo))
        .route(&repo("/refs"), get(refs))
        .route(&repo("/tree/{rev}"), get(tree_root))
        .route(&repo("/tree/{rev}/"), get(tree_root))
        .route(&repo("/tree/{rev}/{*path}"), get(tree))
        .route(&repo("/blob/{rev}/{*path}"), get(blob))
        .route(&repo("/readme/{rev}"), get(readme))
        .route(&repo("/blame/{rev}/{*path}"), get(blame))
        .route(&repo("/webhooks"), get(list_webhooks).post(create_webhook))
        .route(&repo("/webhooks/{id}"), get(get_webhook).patch(update_webhook).delete(delete_webhook))
        .route(&repo("/webhooks/{id}/ping"), post(ping_webhook))
        .route(&repo("/webhooks/{id}/deliveries"), get(webhook_deliveries))
        .route(&repo("/webhooks/{id}/deliveries/{did}/redeliver"), post(redeliver_webhook))
        .route(&repo("/statuses/{rev}"), get(statuses).post(post_status))
        .route(&repo("/paths/{rev}"), get(paths))
        .route(&repo("/log"), get(log))
        .route(&repo("/changesets"), post(write_changeset))
        .route(&repo("/changesets/{id}"), get(changeset))
        .route(&repo("/compare/{spec}"), get(compare))
        .route(&repo("/land"), post(land))
        .route(&repo("/changes"), get(list_changes).post(create_change))
        .route(&repo("/changes/{number}"), get(change_by_number))
        .route(&repo("/work"), get(list_work).post(create_work))
        .route(&repo("/work/claim"), post(claim))
        .route(&repo("/work/{number}"), get(work_by_number))
        .route(&repo("/events"), get(events))
        .route(&repo("/queue"), get(queue))
        .route(&repo("/queue/pause"), post(pause_queue))
        .route(&repo("/queue/resume"), post(resume_queue))
        .route(&repo("/audit"), get(audit))
        .route("/v1/code/changes/{id}", get(get_change).patch(update_change))
        .route("/v1/code/changes/{id}/enqueue", post(enqueue).delete(dequeue).patch(set_lane))
        .route("/v1/code/changes/{id}/approve-run", post(approve_run))
        .route("/v1/code/changes/{id}/revise", post(revise))
        .route("/v1/code/changes/{id}/land", post(land_change))
        .route("/v1/code/changes/{id}/close", post(close_change))
        .route("/v1/code/changes/{id}/checks", post(report_check))
        .route("/v1/code/changes/{id}/preview", post(create_preview).delete(delete_preview))
        .route("/v1/code/work/{id}", get(get_work).patch(update_work))
        .route("/v1/code/work/{id}/heartbeat", post(heartbeat))
        .route("/v1/code/work/{id}/release", post(release))
        .route("/v1/code/work/{id}/complete", post(complete))
        .route("/v1/code/github", get(github))
        .route("/v1/code/github/installations", get(github_installations))
        // GitHub sends payloads up to 25 MB (a push with many commits); axum's default is 2 MB.
        .route("/v1/code/github/webhook", post(github_webhook).layer(axum::extract::DefaultBodyLimit::max(25 << 20)))
        .route("/v1/code/github/setup", get(github_setup))
        .merge(crate::review::api::routes())
        .merge(crate::ai::api::routes())
        // Writes carry file contents (base64 for binary): room for a file of MAX_WRITE_FILE.
        // The webhook route sets its own limit.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .layer(cors())
        .with_state(code.clone());
    let git = code_git::router(GitAdapter::new(code.clone()), code_git::ServerConfig::default());
    let all = api.merge(git);
    // MCP tools call the rest of the API on the caller's behalf.
    let mcp = crate::mcp::routes(code, all.clone()).layer(axum::extract::DefaultBodyLimit::max(MAX_BODY));
    all.merge(mcp)
}

// ── info, names, owners ──

/// Server info, plus who the request is: the dashboard's sign-in checks a token against it.
async fn info(State(code): S, headers: axum::http::HeaderMap) -> Result<Json<Value>> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim().to_string());
    let resolved = code.caller(token.as_deref());
    // Who this caller reviews and comments as (so the UI knows which threads are theirs).
    let reviewer = resolved.as_ref().ok().and_then(|c| code.reviewer(c, &headers).ok()).map(|r| json!({ "kind": r.kind, "name": r.name }));
    let mut caller = match resolved {
        Ok(Caller::Admin) => json!({ "kind": "admin", "owner": null }),
        Ok(Caller::Owner { name, .. }) => json!({ "kind": "owner", "owner": name }),
        Ok(Caller::App { name, .. }) => json!({ "kind": "app", "owner": null, "app": name }),
        Ok(Caller::Anonymous) => json!({ "kind": "anonymous", "owner": null }),
        Err(_) => json!({ "kind": "invalid", "owner": null }),
    };
    caller["reviewer"] = reviewer.unwrap_or(Value::Null);
    Ok(Json(json!({
        "storage": code.config.storage,
        "git_url": code.config.git_url,
        "repos": code.db.count_repos()?,
        "stats": code.stats()?,
        "github": { "configured": code.github_hub().is_some() },
        "auth": { "required": code.config.admin_token.is_some(), "caller": caller },
    })))
}

async fn check_name(State(code): S, Path(name): Path<String>) -> Result<Json<Value>> {
    Ok(Json(code.check_name(&name)?))
}

async fn check_repo_name(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Value>> {
    Ok(Json(code.check_repo_name(&caller, &owner, &repo)?))
}

async fn list_owners(State(code): S, caller: Caller) -> Result<Json<Vec<Owner>>> {
    let mut owners = code.db.owners()?;
    if caller != Caller::Admin {
        // `repos_count` counts what this caller can see, as on `GET /owners/{owner}`.
        let repos = code.db.repos(None)?;
        for o in &mut owners {
            o.repos_count = repos.iter().filter(|r| r.owner_id == o.id && Code::can_read(&caller, r)).count() as u64;
        }
    }
    Ok(Json(owners))
}

async fn create_owner(State(code): S, caller: Caller, Body(b): Body) -> Result<(StatusCode, Json<Owner>)> {
    let name = b.get("name").and_then(Value::as_str).unwrap_or("");
    let kind = match b.get("kind").and_then(Value::as_str) {
        None | Some("user") => OwnerKind::User,
        Some("org") => OwnerKind::Org,
        Some(k) => return Err(Error::Invalid(format!("kind must be user or org, not {k}"))),
    };
    let o = code.create_owner(&caller, name, kind, b.get("display_name").and_then(Value::as_str))?;
    Ok((StatusCode::CREATED, Json(o)))
}

async fn get_owner(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = code.owner_or_404(&owner)?;
    let mut repos = Vec::new();
    for r in code.db.repos(Some(&o.id))? {
        if Code::can_read(&caller, &r) {
            repos.push(code.repo_view(&r).await?);
        }
    }
    let mut v = serde_json::to_value(&o).map_err(|e| Error::Internal(e.into()))?;
    v["repos_count"] = json!(repos.len());
    v["repos"] = serde_json::to_value(repos).map_err(|e| Error::Internal(e.into()))?;
    Ok(Json(v))
}

async fn update_owner(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<Owner>> {
    Ok(Json(code.update_owner(&caller, &owner, &b)?))
}

async fn create_token(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<(StatusCode, Json<Value>)> {
    let name = b.get("name").and_then(Value::as_str).unwrap_or("token");
    Ok((StatusCode::CREATED, Json(code.create_token(&caller, &owner, name)?)))
}

async fn list_tokens(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Vec<Value>>> {
    Ok(Json(code.list_tokens(&caller, &owner)?))
}

async fn revoke_token(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<StatusCode> {
    code.revoke_token(&caller, &owner, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── repos ──

#[derive(Deserialize)]
struct OwnerQuery {
    owner: Option<String>,
}

async fn list_repos(State(code): S, caller: Caller, Q(q): Q<OwnerQuery>) -> Result<Json<Vec<Repo>>> {
    let owner_id = match q.owner.as_deref().filter(|o| !o.is_empty()) {
        Some(o) => Some(code.owner_or_404(o)?.id),
        None => None,
    };
    let mut out = Vec::new();
    for r in code.db.repos(owner_id.as_deref())? {
        if Code::can_read(&caller, &r) {
            out.push(code.repo_view(&r).await?);
        }
    }
    Ok(Json(out))
}

async fn create_repo(State(code): S, caller: Caller, Body(b): Body) -> Result<(StatusCode, Json<Repo>)> {
    let r = code.create_repo(&caller, &b).await?;
    Ok((StatusCode::CREATED, Json(code.repo_view(&r).await?)))
}

async fn get_repo(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Repo>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.repo_view(&r).await?))
}

async fn update_repo(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, Body(b): Body) -> Result<Json<Repo>> {
    let r = code.update_repo(&caller, &owner, &repo, &b).await?;
    Ok(Json(code.repo_view(&r).await?))
}

async fn delete_repo(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<StatusCode> {
    code.delete_repo(&caller, &owner, &repo).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── reading ──

async fn refs(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Vec<Value>>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.refs_view(&r)?))
}

async fn tree_root(State(code): S, caller: Caller, Path((owner, repo, rev)): Path<(String, String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.tree_view(&r, &rev, "").await?))
}

async fn tree(
    State(code): S,
    caller: Caller,
    Path((owner, repo, rev, path)): Path<(String, String, String, String)>,
) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.tree_view(&r, &rev, &path).await?))
}

async fn blob(
    State(code): S,
    caller: Caller,
    Path((owner, repo, rev, path)): Path<(String, String, String, String)>,
) -> Result<Response> {
    let r = code.readable(&caller, &owner, &repo)?;
    let (cs, file) = code.blob(&r, &rev, &path).await?;
    let mut resp = file.data.clone().into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(render::content_type(&path, &file.data)));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"));
    if let Ok(v) = HeaderValue::from_str(&cs_id(&cs)) {
        h.insert("x-roll-changeset", v);
    }
    if let Ok(v) = HeaderValue::from_str(&file.oid.hex()) {
        h.insert(header::ETAG, v);
    }
    // A full changeset id never changes what it points at.
    let immutable = rev.trim_start_matches("cs_").len() == 64;
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "no-cache" }),
    );
    Ok(resp)
}

async fn blame(
    State(code): S,
    caller: Caller,
    Path((owner, repo, rev, path)): Path<(String, String, String, String)>,
) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.blame_view(&r, &rev, &path).await?))
}

// ── webhooks and commit statuses ──

async fn list_webhooks(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Vec<Value>>> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok(Json(code.list_webhooks(&r)?))
}

async fn create_webhook(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, Body(b): Body) -> Result<(StatusCode, Json<Value>)> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok((StatusCode::CREATED, Json(code.create_webhook(&caller, &r, &b)?)))
}

async fn get_webhook(State(code): S, caller: Caller, Path((owner, repo, id)): Path<(String, String, String)>) -> Result<Json<Value>> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok(Json(code.get_webhook(&r, &id)?))
}

async fn update_webhook(State(code): S, caller: Caller, Path((owner, repo, id)): Path<(String, String, String)>, Body(b): Body) -> Result<Json<Value>> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok(Json(code.update_webhook(&caller, &r, &id, &b)?))
}

async fn delete_webhook(State(code): S, caller: Caller, Path((owner, repo, id)): Path<(String, String, String)>) -> Result<StatusCode> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    code.delete_webhook(&caller, &r, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ping_webhook(State(code): S, caller: Caller, Path((owner, repo, id)): Path<(String, String, String)>) -> Result<(StatusCode, Json<Value>)> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok((StatusCode::ACCEPTED, Json(code.ping_webhook(&caller, &r, &id)?)))
}

async fn webhook_deliveries(State(code): S, caller: Caller, Path((owner, repo, id)): Path<(String, String, String)>) -> Result<Json<Vec<Value>>> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok(Json(code.webhook_deliveries(&caller, &r, &id)?))
}

async fn redeliver_webhook(
    State(code): S,
    caller: Caller,
    Path((owner, repo, id, did)): Path<(String, String, String, String)>,
) -> Result<Json<Value>> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok(Json(code.redeliver_webhook(&caller, &r, &id, &did)?))
}

async fn statuses(State(code): S, caller: Caller, Path((owner, repo, rev)): Path<(String, String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.get_statuses(&r, &rev)?))
}

async fn post_status(State(code): S, caller: Caller, Path((owner, repo, rev)): Path<(String, String, String)>, Body(b): Body) -> Result<(StatusCode, Json<Value>)> {
    let r = code.writable(&caller, &owner, &repo)?;
    Ok((StatusCode::CREATED, Json(code.post_status(&caller, &r, &rev, &b)?)))
}

#[derive(Deserialize)]
struct PathQuery {
    path: Option<String>,
}

async fn readme(
    State(code): S,
    caller: Caller,
    Path((owner, repo, rev)): Path<(String, String, String)>,
    Q(q): Q<PathQuery>,
) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.readme(&r, &rev, q.path.as_deref()).await?))
}

async fn paths(State(code): S, caller: Caller, Path((owner, repo, rev)): Path<(String, String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.paths(&r, &rev).await?))
}

#[derive(Deserialize)]
struct LogQuery {
    rev: Option<String>,
    path: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

async fn log(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, Q(q): Q<LogQuery>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.log_view(&r, q.rev.as_deref(), q.path.as_deref(), q.limit.unwrap_or(0), q.cursor.as_deref())?))
}

async fn changeset(State(code): S, caller: Caller, Path((owner, repo, id)): Path<(String, String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    Ok(Json(code.changeset_view(&r, &id).await?))
}

#[derive(Deserialize)]
struct FormatQuery {
    format: Option<String>,
}

/// `?format=diff`: the files' unified diffs as one text/plain body (what review bots read).
async fn compare(
    State(code): S,
    caller: Caller,
    Path((owner, repo, spec)): Path<(String, String, String)>,
    Q(q): Q<FormatQuery>,
) -> Result<Response> {
    let r = code.readable(&caller, &owner, &repo)?;
    let (base, head) = spec.split_once("...").ok_or_else(|| Error::Invalid("compare takes base...head".into()))?;
    let v = code.compare_view(&r, base, head).await?;
    match q.format.as_deref() {
        None | Some("json") => Ok(Json(v).into_response()),
        Some("diff" | "patch") => {
            let mut out = String::new();
            for f in v["files"].as_array().into_iter().flatten() {
                let path = f["path"].as_str().unwrap_or("");
                out.push_str(&format!("diff --git a/{path} b/{path}\n"));
                match f["patch"].as_str() {
                    Some(p) => out.push_str(p),
                    None if f["binary"] == true => out.push_str("Binary files differ\n"),
                    None => out.push_str("(diff too large to show)\n"),
                }
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], out).into_response())
        }
        Some(f) => Err(Error::Invalid(format!("format is json or diff, not {f}"))),
    }
}

// ── writing and landing ──

fn write_files(b: &Value) -> Result<WriteFiles> {
    serde_json::from_value(b.clone()).map_err(|e| Error::Invalid(e.to_string()))
}

async fn write_changeset(
    State(code): S,
    caller: Caller,
    Path((owner, repo)): Path<(String, String)>,
    Body(b): Body,
) -> Result<(StatusCode, Json<Value>)> {
    let r = code.writable(&caller, &owner, &repo)?;
    let mut w = write_files(&b)?;
    w.changeset = None;
    let cs = code.write(&r, &caller, &w).await?;
    Ok((StatusCode::CREATED, Json(code.changeset_json(&r, &cs).await?)))
}

/// 200 for a landing, 202 when it was queued.
fn landed_or_queued(body: Value) -> (StatusCode, Json<Value>) {
    let status = if body.get("status").and_then(Value::as_str) == Some("queued") { StatusCode::ACCEPTED } else { StatusCode::OK };
    (status, Json(body))
}

fn bypass(b: &Value) -> Result<bool> {
    match b.get("bypass") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(v)) => Ok(*v),
        Some(_) => Err(Error::Invalid("bypass must be true or false".into())),
    }
}

fn lane(b: &Value) -> Result<Lane> {
    match b.get("lane").and_then(Value::as_str) {
        None => Ok(Lane::Normal),
        Some(l) => Lane::parse(l).ok_or_else(|| Error::Invalid("lane is urgent or normal".into())),
    }
}

async fn land(
    State(code): S,
    caller: Caller,
    Path((owner, repo)): Path<(String, String)>,
    Body(b): Body,
) -> Result<(StatusCode, Json<Value>)> {
    let r = code.writable(&caller, &owner, &repo)?;
    let w = write_files(&b)?;
    let ref_name = b.get("ref").and_then(Value::as_str).map(|s| short_ref(s.trim()));
    Ok(landed_or_queued(code.land_api(&r, &caller, &w, ref_name.as_deref(), bypass(&b)?).await?))
}

// ── changes ──

#[derive(Deserialize)]
struct StateQuery {
    state: Option<String>,
    label: Option<String>,
}

async fn list_changes(
    State(code): S,
    caller: Caller,
    Path((owner, repo)): Path<(String, String)>,
    Q(q): Q<StateQuery>,
) -> Result<Json<Vec<Change>>> {
    let r = code.readable(&caller, &owner, &repo)?;
    let state = match q.state.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(ChangeState::parse(s).ok_or_else(|| Error::Invalid(format!("unknown state {s}")))?),
        None => None,
    };
    code.settings(&r).await;
    let list = code.db.changes(&r.id, state, 200)?;
    for c in &list {
        code.prepare_review(&r, c).await;
    }
    Ok(Json(list.into_iter().map(|c| code.change_view(c)).collect::<Result<_>>()?))
}

async fn create_change(
    State(code): S,
    caller: Caller,
    Path((owner, repo)): Path<(String, String)>,
    Body(b): Body,
) -> Result<(StatusCode, Json<Change>)> {
    let r = code.writable(&caller, &owner, &repo)?;
    Ok((StatusCode::CREATED, Json(code.create_change(&r, &caller, &b, None).await?)))
}

async fn change_by_number(
    State(code): S,
    caller: Caller,
    Path((owner, repo, number)): Path<(String, String, String)>,
) -> Result<Json<Change>> {
    let r = code.readable(&caller, &owner, &repo)?;
    let c = if number.starts_with("chg_") {
        code.db.change(&number)?.filter(|c| c.repo_id == r.id)
    } else {
        let n: u64 = number.trim_start_matches('#').parse().map_err(|_| Error::NotFound(format!("change {number}")))?;
        code.db.change_by_number(&r.id, n)?
    };
    let c = c.ok_or_else(|| Error::NotFound(format!("change {number}")))?;
    code.settings(&r).await;
    code.prepare_review(&r, &c).await;
    Ok(Json(code.change_view(c)?))
}

async fn get_change(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Change>> {
    let (r, c) = code.readable_change(&caller, &id)?;
    code.settings(&r).await;
    code.prepare_review(&r, &c).await;
    Ok(Json(code.change_or_404(&id)?))
}

async fn update_change(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<Change>> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.update_change(&r, &c, &b)?))
}

async fn approve_run(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Change>> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.approve_run(&r, &caller, &c)?))
}

async fn enqueue(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<(StatusCode, Json<Value>)> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(landed_or_queued(code.enqueue(&r, &c, lane(&b)?, &caller).await?))
}

async fn set_lane(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<Change>> {
    let (r, c) = code.writable_change(&caller, &id)?;
    if b.get("lane").is_none() {
        return Err(Error::Invalid("lane is required: urgent or normal".into()));
    }
    Ok(Json(code.set_lane(&r, &c, lane(&b)?, &caller)?))
}

async fn dequeue(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Change>> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.dequeue(&r, &c)?))
}

async fn revise(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<Change>> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.revise_change(&r, &caller, &c, &b).await?))
}

async fn land_change(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<(StatusCode, Json<Value>)> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(landed_or_queued(code.land_change(&r, &caller, &c, bypass(&b)?, lane(&b)?).await?))
}

async fn close_change(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Change>> {
    let (_, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.close_change(&c)?))
}

async fn report_check(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<Change>> {
    let (_, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.report_check(&c, &b)?))
}

async fn create_preview(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Change>> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.create_preview(&r, &c).await?))
}

async fn delete_preview(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<Change>> {
    let (r, c) = code.writable_change(&caller, &id)?;
    Ok(Json(code.delete_preview(&r, &c).await?))
}

// ── work ──

async fn list_work(
    State(code): S,
    caller: Caller,
    Path((owner, repo)): Path<(String, String)>,
    Q(q): Q<StateQuery>,
) -> Result<Json<Vec<WorkItem>>> {
    let r = code.readable(&caller, &owner, &repo)?;
    let state = match q.state.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(WorkState::parse(s).ok_or_else(|| Error::Invalid(format!("unknown state {s}")))?),
        None => None,
    };
    let list = code.db.work_items(&r.id, state, q.label.as_deref().filter(|l| !l.is_empty()), 500)?;
    Ok(Json(list.into_iter().map(|mut w| {
        w.repo = r.full_name();
        w
    }).collect()))
}

async fn create_work(
    State(code): S,
    caller: Caller,
    Path((owner, repo)): Path<(String, String)>,
    Body(b): Body,
) -> Result<(StatusCode, Json<WorkItem>)> {
    let r = code.writable(&caller, &owner, &repo)?;
    Ok((StatusCode::CREATED, Json(code.create_work(&r, &b)?)))
}

async fn claim(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>, Body(b): Body) -> Result<Response> {
    let r = code.writable(&caller, &owner, &repo)?;
    Ok(match code.claim(&r, &b)? {
        Some(w) => Json(w).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

async fn work_by_number(
    State(code): S,
    caller: Caller,
    Path((owner, repo, number)): Path<(String, String, String)>,
) -> Result<Json<WorkItem>> {
    let r = code.readable(&caller, &owner, &repo)?;
    let w = if number.starts_with("wi_") {
        code.db.work(&number)?.filter(|w| w.repo_id == r.id)
    } else {
        let n: u64 = number.trim_start_matches('#').parse().map_err(|_| Error::NotFound(format!("work item {number}")))?;
        code.db.work_by_number(&r.id, n)?
    };
    let w = w.ok_or_else(|| Error::NotFound(format!("work item {number}")))?;
    Ok(Json(code.work_view(w)?))
}

async fn get_work(State(code): S, caller: Caller, Path(id): Path<String>) -> Result<Json<WorkItem>> {
    Ok(Json(code.readable_work(&caller, &id)?.1))
}

async fn update_work(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<WorkItem>> {
    let (_, w) = code.writable_work(&caller, &id)?;
    Ok(Json(code.update_work(&w, &b)?))
}

async fn heartbeat(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<WorkItem>> {
    let (_, w) = code.writable_work(&caller, &id)?;
    Ok(Json(code.heartbeat(&w, &b)?))
}

async fn release(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<WorkItem>> {
    let (_, w) = code.writable_work(&caller, &id)?;
    Ok(Json(code.release(&w, &b)?))
}

async fn complete(State(code): S, caller: Caller, Path(id): Path<String>, Body(b): Body) -> Result<Json<WorkItem>> {
    let (_, w) = code.writable_work(&caller, &id)?;
    Ok(Json(code.complete(&w, &b)?))
}

// ── merge queue ──

async fn queue(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Value>> {
    let r = code.readable(&caller, &owner, &repo)?;
    code.settings(&r).await;
    Ok(Json(code.queue_view(&r)?))
}

async fn pause_queue(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Value>> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok(Json(code.set_queue_paused(&r, &caller, true)?))
}

async fn resume_queue(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Value>> {
    let r = code.writable(&caller, &owner, &repo)?;
    code.check_manage(&caller, &r)?;
    Ok(Json(code.set_queue_paused(&r, &caller, false)?))
}

/// Queue bypasses, pauses and approved runs: writers only.
async fn audit(State(code): S, caller: Caller, Path((owner, repo)): Path<(String, String)>) -> Result<Json<Vec<Value>>> {
    let r = code.writable(&caller, &owner, &repo)?;
    Ok(Json(code.db.audit_log(&r.id, 200)?))
}

// ── events ──

async fn events(State(code): S, caller: Caller, parts: Parts, Path((owner, repo)): Path<(String, String)>) -> Result<Response> {
    let r = code.readable(&caller, &owner, &repo)?;
    let peer = parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
    let key = crate::streams::key(&caller, token(&parts).as_deref(), &parts.headers, peer, code.config.trust_forwarded_for);
    let permit = match code.stream_permit(key) {
        Ok(p) => p,
        Err(resp) => return Ok(*resp),
    };
    let repo_id = r.id.clone();
    let landed = futures::stream::unfold((code.store.subscribe(&r.id), code.clone()), |(mut rx, code)| async move {
        loop {
            match rx.recv().await {
                Ok(l) if l.already => continue,
                Ok(l) => {
                    let data = code.landed_json(&l);
                    return Some((("landed", data), (rx, code)));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let control = futures::stream::unfold((code.subscribe(), repo_id), |(mut rx, repo_id)| async move {
        loop {
            match rx.recv().await {
                Ok(e) if e.repo_id == repo_id => return Some(((e.kind, e.data), (rx, repo_id))),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    // The permit lives as long as the stream: the slot frees when the client goes away.
    let stream = futures::stream::select(landed, control).map(move |(kind, data)| {
        let _held = &permit;
        Ok::<_, std::convert::Infallible>(SseEvent::default().event(kind).data(data.to_string()))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}

impl Code {
    /// A landing as the `landed` event (SSE and webhooks).
    pub(crate) fn landed_json(&self, l: &code_store::Landed) -> Value {
        let change = self.landing_changes.get(&(l.repo.clone(), l.source));
        json!({
            "changeset": cs_id(&l.changeset),
            "git_commit": l.git_commit.hex(),
            "ref": short_ref(&l.ref_name),
            "generation": l.generation,
            "message": l.summary,
            "author": l.author,
            "change": change,
            "files": l.files,
            "at": l.at_ms / 1000,
            "count": l.count,
        })
    }
}

// ── GitHub (phase 2) ──

async fn github(State(code): S) -> Json<Value> {
    Json(crate::github::info(&code))
}

async fn github_installations(State(code): S, caller: Caller) -> Result<Json<Vec<Value>>> {
    // The App's installations belong to every tenant: an owner token must not list (or
    // connect) another account's repositories. Installations aren't bound to roll owners yet.
    match caller {
        Caller::Admin => {}
        Caller::Anonymous => return Err(Error::Unauthorized),
        Caller::Owner { .. } | Caller::App { .. } => {
            return Err(Error::Forbidden("listing GitHub installations needs the admin token".into()));
        }
    }
    Ok(Json(crate::github::installations(&code).await?))
}

async fn github_webhook(State(code): S, headers: axum::http::HeaderMap, body: axum::body::Bytes) -> Result<StatusCode> {
    crate::github::webhook(code, headers, body).await
}

/// The App's setup URL: back to the dashboard after installing it on GitHub.
async fn github_setup(State(code): S) -> Response {
    let to = format!("{}/dashboard/code?github=installed", code.config.web_url.trim_end_matches('/'));
    (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
}

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
use std::collections::BTreeMap;
use serde_json::{Value, json};
use std::sync::Arc;

type S = AxState<Arc<Engine>>;

pub fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/v1/previews", get(list).post(create))
        .route("/v1/previews/events", get(events))
        .route("/v1/previews/{id}", get(one).delete(delete))
        .route("/v1/previews/{id}/refresh", post(refresh))
        .route("/v1/previews/{id}/keep", post(keep))
        .route("/v1/previews/{id}/visual/accept", post(accept_visual))
        .route("/v1/previews/{id}/regressions/accept", post(accept_regressions))
        .route("/v1/previews/{id}/approve", post(approve))
        .route("/v1/previews/{id}/visit", post(visit))
        .route("/v1/previews/{id}/env", get(vars).put(put_vars))
        .route("/v1/previews/{id}/shares", get(shares).post(share))
        .route("/v1/previews/{id}/shares/{share}", axum::routing::delete(unshare))
        .route("/v1/previews/{id}/screenshot", get(screenshot))
        .route("/v1/previews/{id}/qr.png", get(qr))
        .route("/v1/previews/{id}/connect", get(connect))
        .route("/v1/previews/{id}/notes", get(notes))
        .route("/v1/previews/{id}/notes/{note}/resolve", post(resolve_note))
        .route("/v1/previews/{id}/notes/{note}/crop", get(note_crop))
        .route("/v1/previews/{id}/mail", get(mails))
        .route("/v1/previews/{id}/mail/{mail}", get(mail_raw))
        .route("/v1/previews/{id}/screenshot.png", get(screenshot_link))
        .route("/v1/previews/{id}/reset", post(reset))
        .route("/v1/previews/{id}/data/{resource}", get(browse_data))
        // These act on running services, which live on the preview's holder in a cluster.
        .merge(
            Router::new()
                .route("/v1/previews/{id}/sleep", post(sleep))
                .route("/v1/previews/{id}/wake", post(wake))
                .route("/v1/previews/{id}/logs", get(logs))
                .route("/v1/previews/{id}/requests", get(requests))
                .route("/v1/previews/{id}/metrics", get(metrics))
                .route("/v1/previews/{id}/errors", get(errors))
                .route("/v1/previews/{id}/errors/count", get(error_count))
                // A shell in a running service: its session lives on the holder too.
                .route("/v1/previews/{id}/shell/sessions", post(open_shell))
                .route("/v1/previews/{id}/shell/sessions/{sh}", axum::routing::delete(close_shell))
                .route("/v1/previews/{id}/shell/sessions/{sh}/screen", get(shell_screen))
                .route("/v1/previews/{id}/shell/sessions/{sh}/keys", post(shell_keys))
                .route("/v1/previews/{id}/shell/sessions/{sh}/resize", post(shell_resize))
                .route("/v1/previews/{id}/forward", get(forward))
                .route("/v1/previews/{id}/swap", get(swap))
                // A page read through roll (roll MCP's `previews_fetch`): its services are here.
                .route("/v1/previews/{id}/fetch", get(fetch_page))
                // A preview's spans, kept on its holder: the intake (its token the credential) and the reads.
                .route("/v1/previews/{id}/otlp/{token}/v1/traces", post(otlp_traces).layer(axum::extract::DefaultBodyLimit::max(8 << 20)))
                .route("/v1/previews/{id}/traces", get(traces))
                .route("/v1/previews/{id}/traces/{trace}", get(trace))
                .route("/v1/previews/{id}/routes", get(routes))
                .route("/v1/previews/{id}/files", get(read_files).put(write_files).layer(axum::extract::DefaultBodyLimit::max(crate::engine::shell::FILES_MAX + (1 << 20))))
                .route_layer(axum::middleware::from_fn_with_state(engine.clone(), on_leader))
                .with_state(engine.clone()),
        )
        .route("/v1/previews/{id}/activity", get(activity))
        .route("/v1/previews/{id}/visitors", get(visitors))
        .route("/v1/previews/projects/{project}/settings", get(settings).put(put_settings))
        .route("/v1/previews/projects/{project}/usage", get(usage))
        .route("/v1/previews/projects/{project}/mask-suggestions", get(mask_suggestions))
        .route("/v1/previews/projects/{project}/bypass", get(bypass).post(rotate_bypass).delete(delete_bypass))
        .route("/v1/previews/projects/{project}/hooks", get(hooks).post(create_hook))
        .route("/v1/previews/projects/{project}/hooks/{hook}", axum::routing::delete(delete_hook))
        .route_layer(axum::middleware::from_fn_with_state(engine.clone(), scoped))
        // A deploy hook: the secret in its path is the credential (rolld's gate lets it through).
        .route("/v1/previews/hooks/{secret}", axum::routing::post(run_hook))
        .with_state(engine)
}

/// The projects a signed-in person who isn't an admin may see previews of: the ones they're a
/// member of, as the workspace API scopes them (what their role lets them do there, roll-access
/// decides). `None`: every project (tokens, the operator, admins).
#[derive(Clone)]
struct Visible(Option<std::collections::HashSet<String>>);

impl Visible {
    fn sees(&self, project: &str) -> bool {
        self.0.as_ref().is_none_or(|p| p.contains(project))
    }
}

/// Another owner's preview or project is a 404 to a signed-in person (as the workspace's own
/// projects are), so they can't learn it exists. The screenshot's signed link stays public.
async fn scoped(AxState(e): S, mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let who = req.extensions().get::<roll_auth::SignedIn>().filter(|u| !u.admin).map(|u| u.user.clone());
    // roll-access decided a person's request by their role (their session's grant, or their token's,
    // is beside it). Without one it came another way (in process, past the gate): only the
    // projects they own, so reaching this router directly never widens a member's role.
    let decided = req.extensions().get::<roll_auth::Grant>().is_some();
    let visible = match who {
        None => Visible(None),
        Some(user) => {
            let e2 = e.clone();
            match blocking(e2, move |e| if decided { e.projects_of(&user) } else { e.projects_owned(&user) }).await {
                Ok(ids) => Visible(Some(ids)),
                Err(err) => return err.into_response(),
            }
        }
    };
    // The path's segments as the handlers read them (percent-decoded): a client may encode an id
    // (`pv%5Fabc`), and a check on the raw one would look up nothing.
    let segs = segments(req.uri().path().trim_start_matches("/v1/previews"));
    let rest: Vec<&str> = segs.iter().map(String::as_str).collect();
    let project = match rest.as_slice() {
        ["projects", project, ..] => Some(project.to_string()),
        [_, "screenshot.png"] => None,
        [id, ..] => match e.store.row(id) {
            Ok(r) => Some(r.project_id),
            Err(_) => None,
        },
        [] => None,
    };
    if let Some(p) = project
        && !visible.sees(&p)
    {
        return Error::NotFound("no such preview or project".into()).into_response();
    }
    req.extensions_mut().insert(visible);
    // A person's session on a member route (docs/members.md): roll-access decided it by their role
    // in the project, and its grant is them, not a token. Without it the handlers see the person,
    // as they did before members (their name, a person's accept).
    if req.extensions().get::<roll_auth::Grant>().is_some_and(|g| g.kind == roll_auth::GrantKind::Session) {
        req.extensions_mut().remove::<roll_auth::Grant>();
    }
    next.run(req).await
}

/// A path's non-empty segments, each percent-decoded as axum's `Path` decodes it (`+` is itself:
/// it's a query's space, not a path's).
fn segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let b = seg.as_bytes();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
                if b[i] == b'%'
                    && let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
                {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        })
        .collect()
}

/// Run on the preview's holder (`preview:<id>`), which runs its services, forwarding if need be.
async fn on_leader(AxState(e): S, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    // Decoded as the handler's `Path` is, so the lease named is the preview the handler serves.
    let id = segments(req.uri().path()).into_iter().nth(2).unwrap_or_default();
    match e.cluster.get().cloned() {
        Some(cluster) => {
            cluster
                .dispatch(&crate::engine::preview_key(&id), req, move |req, guard| async move {
                    let resp = next.run(req).await;
                    drop(guard);
                    resp
                })
                .await
        }
        None => next.run(req).await,
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Db(_) | Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Every name a signed-in person goes by (their handle, their linked GitHub logins), as a change's
/// author may be written with either. Read for sure: an error refuses.
async fn aliases_of(e: &Arc<Engine>, who: Option<&roll_auth::SignedIn>) -> crate::Result<Vec<String>> {
    let Some(u) = who else { return Ok(Vec::new()) };
    let (handle, user) = (u.handle.clone(), u.user.clone());
    let others = match e.visitors.get().cloned() {
        Some(v) => tokio::task::spawn_blocking(move || v.names_of(&user)).await.map_err(|err| Error::Internal(err.to_string()))?.map_err(|err| Error::Internal(format!("reading who you are on GitHub: {err}")))?,
        None => Vec::new(),
    };
    Ok(std::iter::once(handle).chain(others).collect())
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

async fn list(AxState(e): S, axum::Extension(visible): axum::Extension<Visible>, Query(q): Query<ListQuery>) -> crate::Result<Json<Vec<Preview>>> {
    let state = q.state.as_deref().map(State::parse).transpose()?;
    blocking(e, move |e| {
        if let Some(p) = &q.project {
            if !visible.sees(p) {
                return Err(Error::NotFound("no such preview or project".into()));
            }
            e.workspace_project(p)?;
        }
        let rows = e.store.rows(q.project.as_deref())?;
        rows.into_iter().filter(|r| visible.sees(&r.project_id) && state.is_none_or(|s| r.state == s)).map(|r| e.store.view(r).map(Preview::light)).collect::<crate::Result<Vec<_>>>()
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsQuery {
    after: Option<i64>,
    limit: Option<usize>,
    latest: Option<String>,
}

/// What happened to previews after `after`, oldest first (`{events, next}`; `next` is where to
/// ask from again, past the events the caller doesn't see too). `latest=1`: none, and the newest
/// seq, to follow from now on.
async fn events(AxState(e): S, axum::Extension(visible): axum::Extension<Visible>, Query(q): Query<EventsQuery>) -> crate::Result<Json<Value>> {
    let latest = matches!(q.latest.as_deref(), Some("1" | "true"));
    if latest && q.after.is_some() {
        return Err(Error::BadRequest("latest or after, not both".into()));
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    blocking(e, move |e| {
        if latest {
            return Ok(json!({ "events": [], "next": e.store.latest_event()? }));
        }
        let after = q.after.unwrap_or(0);
        let got = e.store.events(after, limit)?;
        let next = got.last().map_or(after, |x| x.seq);
        let seen: Vec<crate::Event> = got.into_iter().filter(|x| visible.sees(&x.project)).collect();
        Ok(json!({ "events": seen, "next": next }))
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

async fn create(AxState(e): S, axum::Extension(visible): axum::Extension<Visible>, who: Option<axum::Extension<roll_auth::SignedIn>>, headers: axum::http::HeaderMap, b: Bytes) -> crate::Result<Response> {
    let b: CreateBody = body(&b)?;
    if b.branch.is_some() == b.change.is_some() {
        return Err(Error::BadRequest("give one of branch or change".into()));
    }
    // roll-access decides a create by the project `X-Roll-Project` names (a member's role there, a
    // token's rule): the preview is made in that one, never in another the body names.
    if let Some(named) = headers.get("x-roll-project").map(|v| v.to_str().unwrap_or_default().trim())
        && named != b.project
    {
        return Err(Error::BadRequest(format!("X-Roll-Project names {named}, the body {}: they must be the same project", b.project)));
    }
    if !visible.sees(&b.project) {
        return Err(Error::NotFound("no such preview or project".into()));
    }
    let by = who.map(|axum::Extension(u)| u.handle).unwrap_or_else(|| "operator".into());
    let (p, made) = e.create(&b.project, &b.repo, b.branch.as_deref(), b.change.as_deref(), &by).await?;
    let status = if made { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, Json(p)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VisitBody {
    to: String,
}

/// A signed-in person's one-use way into a team-access preview (roll's dashboard asks for it as
/// them, then sends the browser there). A token isn't a person: only a session gets one.
async fn visit(AxState(e): S, Path(id): Path<String>, who: Option<axum::Extension<roll_auth::SignedIn>>, b: Bytes) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    let Some(axum::Extension(user)) = who else {
        return Err(Error::Forbidden("signing in to a preview is for a person signed in to roll, not a token".into()));
    };
    let b: VisitBody = body(&b)?;
    let url = blocking(e, move |e| e.visit_ticket(&id, &user.user, &user.handle, &user.session, &b.to)).await?;
    Ok(Json(json!({ "url": url })))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeepBody {
    hours: u32,
}

async fn keep(AxState(e): S, Path(id): Path<String>, b: Bytes) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    let k: KeepBody = body(&b)?;
    blocking(e, move |e| e.keep(&id, k.hours)).await.map(Json)
}

/// Accept how a change's pages look at its latest screenshot (the `roll/visual` status).
/// A scoped or OAuth token (an agent's, an app's) isn't a person: it can't accept a required one.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AcceptBody {
    /// The head whose screenshots were looked at (`visual_review.head`); refused when it isn't
    /// the latest. Left out: whatever is latest.
    head: Option<String>,
    /// One page only (`visual_review.pages[].path`: `""` the first page). Left out: every page.
    path: Option<String>,
}

async fn accept_visual(AxState(e): S, Path(id): Path<String>, who: Option<axum::Extension<roll_auth::SignedIn>>, grant: Option<axum::Extension<roll_auth::Grant>>, b: Bytes) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    let AcceptBody { head: seen, path } = if b.is_empty() { AcceptBody::default() } else { body::<AcceptBody>(&b)? };
    // The person behind a token too: their own token doesn't let an author accept their change.
    let aliases = aliases_of(&e, who.as_ref().map(|u| &u.0)).await?;
    // A signed-in person who can write to the change's repo in roll code (an admin can).
    let writer = who.as_ref().filter(|u| grant.is_none() && !u.admin).map(|u| u.code_owner.clone());
    let (by, person) = match (who, grant) {
        (Some(axum::Extension(u)), None) => (u.handle, true),
        (_, Some(axum::Extension(g))) => (format!("token:{}", g.name), false),
        (None, None) => ("operator".into(), true),
    };
    // A person signed in to roll, not the operator's token, as for regressions.
    let person = person && by != "operator";
    e.accept_visual(&id, &by, &aliases, person, writer.as_ref().map(|o| o.as_deref()), seen.as_deref(), path.as_deref()).await.map(Json)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AcceptRegressionsBody {
    /// The head that was looked at: refused (409) when a newer push came in.
    head: Option<String>,
}

async fn accept_regressions(AxState(e): S, Path(id): Path<String>, who: Option<axum::Extension<roll_auth::SignedIn>>, grant: Option<axum::Extension<roll_auth::Grant>>, b: Bytes) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    let AcceptRegressionsBody { head: seen } = if b.is_empty() { AcceptRegressionsBody::default() } else { body::<AcceptRegressionsBody>(&b)? };
    // As the visual review's: the person behind a token too.
    let aliases = aliases_of(&e, who.as_ref().map(|u| &u.0)).await?;
    let signed = who.as_ref().filter(|_| grant.is_none()).map(|axum::Extension(u)| u.clone());
    let (by, person) = match (who, grant) {
        (Some(axum::Extension(u)), None) => (u.handle, true),
        (_, Some(axum::Extension(g))) => (format!("token:{}", g.name), false),
        (None, None) => ("operator".into(), true),
    };
    // A person signed in to roll, not the operator's token (a CI job may hold it), who can write to
    // the change's repo (an admin can).
    let person = person && by != "operator";
    let writer = signed.filter(|u| !u.admin).map(|u| u.code_owner);
    e.accept_regressions(&id, &by, &aliases, person, writer.as_ref().map(|o| o.as_deref()), seen.as_deref()).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveBody {
    /// The head that was looked at (`head`): refused (409) when the pull request moved on.
    head: String,
}

/// A writer lets one head of a fork's pull request on a Git URL service run.
async fn approve(AxState(e): S, Path(id): Path<String>, who: Option<axum::Extension<roll_auth::SignedIn>>, grant: Option<axum::Extension<roll_auth::Grant>>, b: Bytes) -> crate::Result<Json<Preview>> {
    check_id(&id)?;
    let ApproveBody { head } = body(&b)?;
    // A person's call (or the operator's), never a token's: an automation reading the pull
    // request's own text mustn't release a fork's code.
    let by = match (who, grant) {
        (_, Some(_)) => return Err(Error::Forbidden("approving a fork's code to run is a person's call: sign in, not a token".into())),
        (Some(axum::Extension(u)), None) => u.handle,
        (None, None) => "operator".into(),
    };
    blocking(e, move |e| e.approve(&id, &head, &by)).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareBody {
    hours: u32,
}

async fn shares(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Vec<crate::Share>>> {
    check_id(&id)?;
    blocking(e, move |e| {
        e.store.row(&id)?;
        e.store.shares(&id)
    })
    .await
    .map(Json)
}

/// The link is in this answer only (its token isn't kept).
async fn share(AxState(e): S, Path(id): Path<String>, who: Option<axum::Extension<roll_auth::SignedIn>>, grant: Option<axum::Extension<roll_auth::Grant>>, b: Bytes) -> crate::Result<Response> {
    check_id(&id)?;
    let s: ShareBody = body(&b)?;
    // Whose link it is: the person signed in, or the person behind a token (an operator's: none).
    let maker = who.as_ref().map(|axum::Extension(u)| u.user.clone()).or_else(|| grant.and_then(|axum::Extension(g)| g.user.map(|u| u.to_string())));
    let by = who.map(|axum::Extension(u)| u.handle).unwrap_or_else(|| "operator".into());
    let (share, url) = blocking(e, move |e| e.share(&id, s.hours, &by, maker.as_deref())).await?;
    let mut v = serde_json::to_value(&share).unwrap_or_default();
    v["url"] = json!(url);
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

async fn unshare(AxState(e): S, Path((id, share)): Path<(String, String)>) -> crate::Result<Response> {
    check_id(&id)?;
    blocking(e, move |e| e.unshare(&id, &share)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VarsBody {
    /// `null` removes one.
    vars: BTreeMap<String, Option<String>>,
}

async fn vars(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<serde_json::Value>> {
    check_id(&id)?;
    let vars = blocking(e, move |e| {
        e.store.row(&id)?;
        e.store.vars(&id)
    })
    .await?;
    Ok(Json(serde_json::json!({ "vars": vars })))
}

/// The notes left on the preview's pages that nobody has resolved (each with its id, to resolve).
async fn notes(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<serde_json::Value>> {
    check_id(&id)?;
    e.store.row(&id)?;
    let notes = e.open_notes(&id).await.map_err(|err| Error::Internal(format!("{err:#}")))?;
    Ok(Json(serde_json::json!({ "notes": notes })))
}

/// A picture of a note's element, as a screenshot run cropped it from the page (404 until one has).
async fn note_crop(AxState(e): S, Path((id, note)): Path<(String, String)>) -> crate::Result<Response> {
    check_id(&id)?;
    e.store.row(&id)?;
    let png = e.store.note_crop(&id, &note)?.ok_or_else(|| Error::NotFound("no picture of that note's element yet".into()))?;
    Ok(([(axum::http::header::CONTENT_TYPE, "image/png"), (axum::http::header::CACHE_CONTROL, "private, max-age=3600")], png).into_response())
}

/// Resolve a note left on the preview's page (its review's id): the page stops listing it.
async fn resolve_note(AxState(e): S, Path((id, note)): Path<(String, String)>, who: Option<axum::Extension<roll_auth::SignedIn>>, grant: Option<axum::Extension<roll_auth::Grant>>) -> crate::Result<Json<serde_json::Value>> {
    check_id(&id)?;
    let by = match (who, grant) {
        (Some(axum::Extension(u)), None) => u.handle,
        (_, Some(axum::Extension(g))) => format!("token:{}", g.name),
        (None, None) => "operator".into(),
    };
    e.resolve_note(&id, &note, &by).await?;
    Ok(Json(serde_json::json!({ "resolved": note })))
}

/// The mail the preview's services sent, caught (newest first; the last 50).
async fn mails(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<serde_json::Value>> {
    check_id(&id)?;
    let mail = blocking(e, move |e| {
        e.store.row(&id)?;
        e.store.mails(&id)
    })
    .await?;
    Ok(Json(serde_json::json!({ "mail": mail })))
}

/// One caught message as it was sent (`message/rfc822`, shown as text, never rendered).
async fn mail_raw(AxState(e): S, Path((id, mail)): Path<(String, String)>) -> crate::Result<Response> {
    check_id(&id)?;
    let raw = blocking(e, move |e| {
        e.store.row(&id)?;
        e.store.mail_raw(&id, &mail)?.ok_or_else(|| Error::NotFound("no such mail".into()))
    })
    .await?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (axum::http::header::CONTENT_DISPOSITION, "inline"),
            (axum::http::header::HeaderName::from_static("x-content-type-options"), "nosniff"),
        ],
        raw,
    )
        .into_response())
}

/// Each data branch's connection (its own credentials), to query what the preview sees.
async fn connect(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<serde_json::Value>> {
    check_id(&id)?;
    let branches = e.connections(&id).await?;
    Ok(Json(serde_json::json!({ "branches": branches })))
}

async fn put_vars(AxState(e): S, Path(id): Path<String>, ext: axum::http::Extensions, b: Bytes) -> crate::Result<Json<serde_json::Value>> {
    check_id(&id)?;
    let v: VarsBody = body(&b)?;
    let vars = e.set_vars_as(&id, v.vars, &crate::ports::Asker::of(&ext)).await?;
    Ok(Json(serde_json::json!({ "vars": vars })))
}

/// The preview's screenshot (a PNG), taken once per head when the project's rules ask.
/// `?previous=1`: the one before it, at an earlier head (to compare).
/// The preview's link with its access (`share_url`; `service=` another service's) as a QR code,
/// so a phone opens it by scanning (Vercel's toolbar): it carries the access token, so it's never
/// cached.
async fn qr(AxState(e): S, Path(id): Path<String>, Query(q): Query<std::collections::HashMap<String, String>>) -> crate::Result<Response> {
    check_id(&id)?;
    let service = q.get("service").cloned();
    let png = blocking(e, move |e| {
        let p = e.store.view(e.store.row(&id)?)?;
        let url = match &service {
            Some(name) => p.services.iter().find(|s| &s.name == name).and_then(|s| s.share_url.clone()),
            None => p.share_url.clone(),
        };
        let url = url.ok_or_else(|| Error::NotFound("the preview has no link for that yet".into()))?;
        roll_auth::totp::qr_png(&url).ok_or_else(|| Error::Internal("the link is too long for a QR code".into()))
    })
    .await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "image/png"), (axum::http::header::CACHE_CONTROL, "no-store")], png).into_response())
}

async fn screenshot(AxState(e): S, Path(id): Path<String>, Query(q): Query<std::collections::HashMap<String, String>>) -> crate::Result<Response> {
    check_id(&id)?;
    let previous = q.get("previous").is_some_and(|v| v == "1" || v == "true");
    let phone = match q.get("view").map(String::as_str) {
        None | Some("desktop") => false,
        Some("phone") => true,
        Some(v) => return Err(Error::BadRequest(format!("view: desktop or phone, not {v:?}"))),
    };
    let base = q.get("base").is_some_and(|v| v == "1" || v == "true");
    // `page=/pricing`: another of the project's pages (`github.pages`), at the desktop width.
    let page = q.get("page").cloned();
    if let Some(p) = &page
        && ((crate::settings::page_entry(p).is_none() && crate::settings::width_entry(p).is_none()) || phone)
    {
        return Err(Error::BadRequest("page: a path from github.pages, or @<width> from github.widths (desktop only)".into()));
    }
    // `diff=base|previous`: where this one differs from the baseline's or the one before (the
    // same view and page), as a picture.
    let diff = match q.get("diff").map(String::as_str) {
        None => None,
        Some(d @ ("base" | "previous")) => Some(d == "base"),
        Some(v) => return Err(Error::BadRequest(format!("diff: base or previous, not {v:?}"))),
    };
    let png = blocking(e, move |e| {
        let shot = |base: bool, previous: bool| -> crate::Result<Option<Vec<u8>>> {
            // `base=1`: the default branch's preview's, the one this was compared with (at the
            // change's merge base when that was kept).
            if base {
                return e.store.base_screenshot(&id, phone, page.as_deref());
            }
            if let Some(path) = &page {
                return e.store.page_screenshot(&id, path, previous);
            }
            match (phone, previous) {
                (true, p) => e.store.phone_screenshot(&id, p),
                (false, true) => e.store.previous_screenshot(&id),
                (false, false) => Ok(e.store.screenshot(&id)?.map(|(_, png)| png)),
            }
        };
        match diff {
            Some(against_base) => {
                let (Some(before), Some(after)) = (shot(against_base, !against_base)?, shot(false, false)?) else { return Ok(None) };
                Ok(crate::visual::diff(&before, &after))
            }
            None => shot(base, previous),
        }
    })
    .await?;
    match png.filter(|png| !png.is_empty()) {
        Some(png) => Ok(([(axum::http::header::CONTENT_TYPE, "image/png"), (axum::http::header::CACHE_CONTROL, "private, no-store")], png).into_response()),
        None => Err(Error::NotFound(if previous { "the preview has no earlier screenshot" } else { "the preview has no screenshot" }.into())),
    }
}

/// The screenshot behind a signed, expiring link (a public route: GitHub fetches the pull request
/// comment's image without credentials). Anything else is 403.
async fn screenshot_link(AxState(e): S, Path(id): Path<String>, Query(q): Query<std::collections::HashMap<String, String>>) -> crate::Result<Response> {
    check_id(&id)?;
    let (Some(exp), Some(sig)) = (q.get("exp").and_then(|x| x.parse::<i64>().ok()), q.get("sig").cloned()) else {
        return Ok((StatusCode::FORBIDDEN, "a signed link only").into_response());
    };
    // `diff=base`: where it differs from the baseline's; `note=<id>`: a note's crop. Each under
    // its own signature.
    let note = q.get("note").cloned();
    let variant: Option<String> = match (q.get("diff").map(String::as_str), note.as_deref()) {
        (None, None) => None,
        (Some("base"), None) => Some("diff=base".into()),
        (None, Some(n)) if crate::store::note_id_ok(n) => Some(format!("note={n}")),
        _ => return Ok((StatusCode::FORBIDDEN, "not a valid link").into_response()),
    };
    let shot = blocking(e, move |e| {
        if !e.store.screenshot_link_ok(&id, exp, &sig, variant.as_deref())? {
            return Ok(None);
        }
        if let Some(n) = &note {
            return e.store.note_crop(&id, n);
        }
        if variant.is_none() {
            return Ok(e.store.screenshot(&id)?.map(|(_, png)| png));
        }
        let (Some(before), Some((_, after))) = (e.store.base_screenshot(&id, false, None)?, e.store.screenshot(&id)?) else { return Ok(None) };
        Ok(crate::visual::diff(&before, &after))
    })
    .await?;
    match shot.filter(|png| !png.is_empty()) {
        Some(png) => Ok(([(axum::http::header::CONTENT_TYPE, "image/png"), (axum::http::header::CACHE_CONTROL, "public, max-age=86400, immutable")], png).into_response()),
        None => Ok((StatusCode::FORBIDDEN, "not a valid link").into_response()),
    }
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
    /// `1` or `true`: keep the response open and send lines as they come (server-sent events).
    follow: Option<String>,
}

/// What its running services use (`{services: [...]}`; 404 for no such preview).
async fn metrics(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    Ok(Json(json!({ "services": e.metrics(&id).await? })))
}

#[derive(Deserialize)]
struct ErrorsQuery {
    minutes: Option<i64>,
}

/// Its services' errors in the last 5–1,440 minutes (60 by default, rounded up to 15-minute
/// steps), grouped, newest first (`{minutes, groups: [...]}`; 404 for no such preview).
async fn errors(AxState(e): S, Path(id): Path<String>, Query(q): Query<ErrorsQuery>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    let minutes = crate::engine::error_span(q.minutes.unwrap_or(60));
    Ok(Json(json!({ "minutes": minutes, "groups": e.errors(&id, minutes).await? })))
}

/// How many errors its services wrote in the span (`{minutes, count}`): a list's rows, with
/// nothing masked to read.
async fn error_count(AxState(e): S, Path(id): Path<String>, Query(q): Query<ErrorsQuery>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    let minutes = crate::engine::error_span(q.minutes.unwrap_or(60));
    Ok(Json(json!({ "minutes": minutes, "count": e.error_count(&id, minutes).await? })))
}

async fn requests(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    Ok(Json(json!({ "requests": e.requests(&id).await? })))
}

/// A service's last lines; with `follow`, server-sent events: `event: service` (its name), then
/// one event per line as it comes, with keep-alives.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ShellAsk {
    service: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// `POST …/{id}/shell/sessions {service?, cols?, rows?}`: a shell in the service, said in the
/// activity as the person who opened it (or the token's: the operator's).
async fn open_shell(AxState(e): S, Path(id): Path<String>, who: Option<axum::Extension<roll_auth::SignedIn>>, b: Bytes) -> crate::Result<Response> {
    check_id(&id)?;
    let ask: ShellAsk = if b.is_empty() { ShellAsk::default() } else { serde_json::from_slice(&b).map_err(|err| Error::BadRequest(format!("bad body: {err}")))? };
    let by = who.map_or_else(|| "the operator".to_string(), |w| w.handle.clone());
    let sid = e.open_shell(&id, ask.service.as_deref(), &by, ask.cols.unwrap_or(120), ask.rows.unwrap_or(32)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": sid }))).into_response())
}

/// The shell's screen: its scrollback, then what comes, as base64 chunks; `event: closed` at its end.
async fn shell_screen(AxState(e): S, Path((id, sh)): Path<(String, String)>) -> crate::Result<Response> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use base64::Engine as _;
    check_id(&id)?;
    let (first, rx, closed) = e.shell_screen(&id, &sh)?;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let head = futures::stream::iter((!first.is_empty()).then(|| Ok::<_, std::convert::Infallible>(Event::default().data(b64(&first)))));
    let rest = futures::stream::unfold((rx, closed, false), move |(mut rx, mut closed, done)| async move {
        if done {
            return None;
        }
        let end = |rx, closed| Some((Ok(Event::default().event("closed").data("")), (rx, closed, true)));
        loop {
            // Ended: what it wrote last (still waiting for this viewer) first, then `closed`.
            if *closed.borrow() {
                match rx.try_recv() {
                    Ok(chunk) => return Some((Ok(Event::default().data(b64(&chunk))), (rx, closed, false))),
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(_) => return end(rx, closed),
                }
            }
            tokio::select! {
                biased;
                got = rx.recv() => match got {
                    Ok(chunk) => return Some((Ok(Event::default().data(b64(&chunk))), (rx, closed, false))),
                    // A viewer that fell behind misses what it missed (the shell goes on).
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return end(rx, closed),
                },
                c = closed.changed() => if c.is_err() {
                    return end(rx, closed);
                },
            }
        }
    });
    let mut r = Sse::new(futures::StreamExt::chain(head, rest)).keep_alive(KeepAlive::default()).into_response();
    r.headers_mut().insert("x-accel-buffering", axum::http::HeaderValue::from_static("no"));
    Ok(r)
}

/// `{data}`: keys typed, base64, at most 64 KiB.
async fn shell_keys(AxState(e): S, Path((id, sh)): Path<(String, String)>, b: Bytes) -> crate::Result<StatusCode> {
    use base64::Engine as _;
    check_id(&id)?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Keys {
        data: String,
    }
    let k: Keys = serde_json::from_slice(&b).map_err(|err| Error::BadRequest(format!("bad body: {err}")))?;
    let data = base64::engine::general_purpose::STANDARD.decode(k.data.as_bytes()).map_err(|_| Error::BadRequest("data is base64".into()))?;
    if data.len() > 64 << 10 {
        return Err(Error::BadRequest("at most 64 KiB of keys at once".into()));
    }
    e.shell_keys(&id, &sh, data).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn shell_resize(AxState(e): S, Path((id, sh)): Path<(String, String)>, b: Bytes) -> crate::Result<StatusCode> {
    check_id(&id)?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Size {
        cols: u16,
        rows: u16,
    }
    let s: Size = serde_json::from_slice(&b).map_err(|err| Error::BadRequest(format!("bad body: {err}")))?;
    e.shell_resize(&id, &sh, s.cols, s.rows).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// OTLP/HTTP `…/v1/traces` from a preview's service: protobuf or JSON (its `Content-Type`), gzip
/// or not (its `Content-Encoding`). OTLP's success answer: an empty partial success.
async fn otlp_traces(AxState(e): S, Path((id, token)): Path<(String, String)>, headers: axum::http::HeaderMap, body: axum::body::Body) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    let Some(reader) = e.span_reader.get().cloned() else { return Err(Error::NotFound("no such trace address".into())) };
    let h = |k: axum::http::HeaderName| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let json = h(axum::http::header::CONTENT_TYPE).contains("json");
    let gzip = h(axum::http::header::CONTENT_ENCODING).contains("gzip");
    // The token first: an address that isn't a live preview's reads none of its body.
    e.check_trace_token(&id, &token)?;
    let b = axum::body::to_bytes(body, 8 << 20).await.map_err(|_| Error::BadRequest("traces are at most 8 MiB a request".into()))?;
    // Parsed off the async workers: a large request is CPU, not waiting.
    let spans = tokio::task::spawn_blocking(move || reader.spans(&b, json, gzip)).await.map_err(|err| Error::Internal(err.to_string()))?.map_err(|err| Error::BadRequest(err.to_string()))?;
    e.keep_spans(&id, &token, spans)?;
    Ok(Json(json!({ "partialSuccess": {} })))
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TracesQuery {
    minutes: Option<i64>,
}

async fn traces(AxState(e): S, Path(id): Path<String>, Query(q): Query<TracesQuery>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    Ok(Json(e.traces(&id, q.minutes.unwrap_or(60)).await?))
}

async fn routes(AxState(e): S, Path(id): Path<String>, Query(q): Query<TracesQuery>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    Ok(Json(e.routes(&id, q.minutes.unwrap_or(60)).await?))
}

async fn trace(AxState(e): S, Path((id, t)): Path<(String, String)>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    Ok(Json(e.trace(&id, &t).await?))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardQuery {
    service: Option<String>,
    port: u16,
}

/// A WebSocket carrying one TCP connection to `port` in the service, from inside it: binary both
/// ways, an empty message the end of a direction (a half-close), as roll apps' tunnel.
async fn forward(AxState(e): S, Path(id): Path<String>, Query(q): Query<ForwardQuery>, who: Option<axum::Extension<roll_auth::SignedIn>>, ws: axum::extract::ws::WebSocketUpgrade) -> crate::Result<Response> {
    check_id(&id)?;
    let by = who.map_or_else(|| "the operator".to_string(), |w| w.handle.clone());
    let f = e.forward(&id, q.service.as_deref(), q.port, &by).await?;
    Ok(ws.max_message_size(1 << 20).max_frame_size(1 << 20).on_upgrade(move |socket| bridge(socket, f)))
}

async fn bridge(socket: axum::extract::ws::WebSocket, mut f: crate::engine::shell::Forward) {
    // `f` lives as long as the bridge: it holds the service awake.
    let ended = f.ended.clone();
    bridge_io(socket, &mut f.io, ended).await;
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowseQuery {
    table: Option<String>,
}

/// A look at one of the preview's data branches (its tables, or a table's first rows).
async fn browse_data(AxState(e): S, Path((id, resource)): Path<(String, String)>, Query(q): Query<BrowseQuery>) -> crate::Result<Json<crate::Browse>> {
    check_id(&id)?;
    Ok(Json(e.browse(&id, &resource, q.table.as_deref()).await?))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchQuery {
    path: String,
    service: Option<String>,
    #[serde(default)]
    head: bool,
}

/// One page of a preview's service, read through roll's own way in: for an agent checking its
/// change (roll MCP's `previews_fetch`) without anyone making a share link.
async fn fetch_page(AxState(e): S, Path(id): Path<String>, Query(q): Query<FetchQuery>) -> crate::Result<Json<crate::engine::shell::FetchedPage>> {
    check_id(&id)?;
    Ok(Json(e.fetch(&id, q.service.as_deref(), &q.path, q.head).await?))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SwapQuery {
    service: Option<String>,
}

/// A laptop's process standing in for a service (`roll preview swap`): the WebSocket carries one
/// HTTP/2 connection the laptop serves (binary both ways, as `forward`'s), and the service's
/// requests go over it while it's open.
async fn swap(AxState(e): S, Path(id): Path<String>, Query(q): Query<SwapQuery>, who: Option<axum::Extension<roll_auth::SignedIn>>, ws: axum::extract::ws::WebSocketUpgrade) -> crate::Result<Response> {
    check_id(&id)?;
    let by = who.map_or_else(|| "the operator".to_string(), |w| w.handle.clone());
    let mut held = e.swap(&id, q.service.as_deref(), &by).await?;
    Ok(ws.max_message_size(1 << 20).max_frame_size(1 << 20).on_upgrade(move |socket| async move {
        let (ours, theirs) = tokio::io::duplex(256 << 10);
        let carried = bridge_io(socket, theirs, held.ended.clone());
        let laptop = async {
            let Ok((sender, conn)) = hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), hyper_util::rt::TokioIo::new(ours)).await else { return };
            held.attach(sender);
            let _ = conn.await;
        };
        // Either end going ends the swap (the service's container answers again).
        tokio::select! {
            _ = carried => {}
            _ = laptop => {}
        }
        drop(held);
    }))
}

/// One WebSocket carrying `io`'s bytes both ways until either side ends or `ended` turns.
async fn bridge_io(socket: axum::extract::ws::WebSocket, io: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin, mut ended: tokio::sync::watch::Receiver<bool>) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut tx, mut rx) = socket.split();
    let (mut rd, mut wr) = tokio::io::split(io);
    let mut buf = vec![0u8; 64 << 10];
    let (mut from_client, mut from_service) = (true, true);
    while from_client || from_service {
        tokio::select! {
            m = rx.next() => match m {
                Some(Ok(Message::Binary(b))) if b.is_empty() => {
                    if from_client {
                        let _ = wr.shutdown().await;
                        from_client = false;
                    }
                }
                Some(Ok(Message::Binary(_))) if !from_client => {}
                Some(Ok(Message::Binary(b))) => {
                    if wr.write_all(&b).await.is_err() || wr.flush().await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            // The preview went: its forwards end with it.
            _ = ended.changed() => break,
            n = rd.read(&mut buf), if from_service => match n {
                Ok(0) | Err(_) => {
                    if tx.send(Message::Binary(Bytes::new())).await.is_err() {
                        break;
                    }
                    from_service = false;
                }
                Ok(n) => {
                    if tx.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        break;
                    }
                }
            },
        }
    }
    let _ = tx.send(Message::Close(None)).await;
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesQuery {
    service: Option<String>,
    path: String,
}

/// What's at `path` in the service, as a tar (`x-roll-service` says which), as it's read.
async fn read_files(AxState(e): S, Path(id): Path<String>, Query(q): Query<FilesQuery>, who: Option<axum::Extension<roll_auth::SignedIn>>) -> crate::Result<Response> {
    check_id(&id)?;
    let by = who.map_or_else(|| "the operator".to_string(), |w| w.handle.clone());
    let (svc, stream) = e.read_files(&id, q.service.as_deref(), &q.path, &by).await?;
    let body = futures::stream::unfold(stream, |mut s| async move {
        let mut buf = vec![0u8; 64 << 10];
        match tokio::io::AsyncReadExt::read(&mut s, &mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), s))
            }
            Err(err) => Some((Err(err), s)),
        }
    });
    let mut r = axum::body::Body::from_stream(body).into_response();
    r.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/x-tar"));
    if let Ok(v) = axum::http::HeaderValue::from_str(&svc) {
        r.headers_mut().insert("x-roll-service", v);
    }
    Ok(r)
}

/// A tar's files (at most 100 MiB) written under `path` in the service: `{service}`.
async fn write_files(AxState(e): S, Path(id): Path<String>, Query(q): Query<FilesQuery>, who: Option<axum::Extension<roll_auth::SignedIn>>, b: Bytes) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    let by = who.map_or_else(|| "the operator".to_string(), |w| w.handle.clone());
    let svc = e.write_files(&id, q.service.as_deref(), &q.path, b, &by).await?;
    Ok(Json(json!({ "service": svc, "path": q.path })))
}

async fn close_shell(AxState(e): S, Path((id, sh)): Path<(String, String)>) -> crate::Result<StatusCode> {
    check_id(&id)?;
    e.close_shell(&id, &sh)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn logs(AxState(e): S, Path(id): Path<String>, Query(q): Query<LogsQuery>) -> crate::Result<Response> {
    check_id(&id)?;
    let lines = q.lines.unwrap_or(200).clamp(1, 2000);
    if !matches!(q.follow.as_deref(), Some("1" | "true" | "yes")) {
        let (service, lines) = e.logs(&id, q.service.as_deref(), lines).await?;
        return Ok(Json(json!({ "service": service, "lines": lines })).into_response());
    }
    use axum::response::sse::{Event, KeepAlive, Sse};
    let (service, rx) = e.follow(&id, q.service.as_deref(), lines).await?;
    let first = futures::stream::once(async move { Ok::<_, std::convert::Infallible>(Event::default().event("service").data(service)) });
    // A stderr line is `event: stderr`; a stdout one the default event, as before.
    let rest = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|(stderr, l)| {
            // `event:` before `data:` on the wire (axum writes them in the order they're set): a
            // client that acts on each `data:` line as it reads it (the CLI) knows the stream.
            let ev = if stderr { Event::default().event("stderr") } else { Event::default() };
            (Ok(ev.data(l.replace(['\r', '\n'], ""))), rx)
        })
    });
    let mut r = Sse::new(futures::StreamExt::chain(first, rest)).keep_alive(KeepAlive::default()).into_response();
    // Proxies pass it on as it comes.
    r.headers_mut().insert("x-accel-buffering", axum::http::HeaderValue::from_static("no"));
    Ok(r)
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

async fn visitors(AxState(e): S, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    check_id(&id)?;
    blocking(e, move |e| {
        e.store.row(&id)?;
        Ok(json!(e.store.visitors(&id)?))
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

/// Columns of the project's data that look like people's data and aren't masked yet:
/// `{suggestions: [{resource, path, rule, why}], unread: [{resource, why}]}`.
/// A scoped token sees only resources its grant lets it read (the route's own check is
/// `previews` read on the project): column names are the products' metadata.
async fn mask_suggestions(AxState(e): S, Path(project): Path<String>, grant: Option<axum::Extension<roll_auth::Grant>>) -> crate::Result<Json<Value>> {
    use roll_auth::scope::{Action, Outcome, Resource};
    let may_read = |kind: crate::Kind, r#ref: &str| {
        grant.as_ref().is_none_or(|axum::Extension(g)| {
            let resource = Resource { product: kind.as_str().into(), id: Some(r#ref.into()), project: Some(project.clone()), ..Default::default() };
            g.allows(&resource, Action::Read) == Outcome::Allow
        })
    };
    e.mask_suggestions(&project, may_read).await.map(Json)
}

async fn put_settings(AxState(e): S, Path(project): Path<String>, ext: axum::http::Extensions, b: Bytes) -> crate::Result<Json<crate::Settings>> {
    let patch: Value = body(&b)?;
    let (p, check) = (project.clone(), patch.clone());
    let (old, new) = blocking(e.clone(), move |e| {
        e.workspace_project(&p)?;
        let old = e.store.settings(&p)?;
        let new = old.patched(&check)?;
        Ok((old, new))
    })
    .await?;
    e.check_masks(&old, &new).await?;
    // A reference to an environment other than the one the previews follow routes its values into
    // code whoever pushes a branch controls: a new or changed one needs its writer's reveal there.
    let who = crate::ports::Asker::of(&ext);
    let may_reveal = |env: &str, folder: &str| e.reveal.get().is_some_and(|r| r.may_reveal(&who, &project, env, folder));
    // So does a new base: every preview's code then reads that environment.
    let base = &new.secrets.base;
    if *base != old.secrets.base && !may_reveal(base, "/") {
        return Err(Error::Forbidden(format!("secrets.base: following {base} needs reveal on {base} in roll secrets")));
    }
    // A reference to the base needed no reveal when the base was the old one: moved, it's checked
    // as new.
    let place = |reference: &str| match e.reveal.get() {
        Some(r) => r.place(reference).map_err(Error::BadRequest),
        None => Err(Error::BadRequest("roll:// references need roll secrets on this node".into())),
    };
    let mut before = crate::settings::references(&old);
    if *base != old.secrets.base {
        before.retain(|(_, _, reference)| place(reference).is_ok_and(|(env, _)| env != old.secrets.base));
    }
    for (scope, name, reference) in crate::settings::references(&new).into_iter().filter(|r| !before.contains(r)) {
        let (env, folder) = place(&reference).map_err(|err| Error::BadRequest(format!("{scope}: {name}: {err}")))?;
        if env == *base {
            continue;
        }
        if !may_reveal(&env, &folder) {
            return Err(Error::Forbidden(format!("{scope}: {name}: naming {env}'s keys needs reveal on {env} in roll secrets")));
        }
    }
    let by = who.principal();
    let s = blocking(e.clone(), move |e| {
        // What's written is what was checked: rules another request changed since are its own.
        let current = e.store.settings(&project)?;
        if current != old {
            return Err(Error::Conflict("the project's preview rules changed while these were checked: try again".into()));
        }
        let s = current.patched(&patch)?;
        let s = e.store.put_settings(&project, &s)?;
        e.store.set_env_refs(&project, &crate::settings::references(&s), &by)?;
        // Whom its previews' placeholders are made by: a person or their token, never the operator.
        e.store.set_rules_maker(&project, Some(by.as_str()).filter(|b| *b != "operator"))?;
        // Every preview of the project converges on the new rules (modes, masks, variables,
        // access): nothing waits for its next push.
        reconverge(e, &project)?;
        Ok(s)
    })
    .await?;
    e.kick();
    Ok(Json(s))
}

/// Whether the project has a bypass secret for automation (never the secret).
async fn bypass(AxState(e): S, Path(project): Path<String>) -> crate::Result<Json<Value>> {
    blocking(e, move |e| {
        e.workspace_project(&project)?;
        let at = e.store.bypass_created(&project)?;
        Ok(json!({ "enabled": at.is_some(), "created_at": at, "header": crate::runtime::BYPASS_HEADER }))
    })
    .await
    .map(Json)
}

/// A new bypass secret (the old one stops working): the only time it's shown.
async fn rotate_bypass(AxState(e): S, Path(project): Path<String>) -> crate::Result<Response> {
    let v = blocking(e.clone(), move |e| {
        e.workspace_project(&project)?;
        let (secret, at) = e.store.rotate_bypass(&project)?;
        // Services that check access themselves (roll apps) get the new secret's hash.
        reconverge(e, &project)?;
        Ok(json!({ "enabled": true, "created_at": at, "header": crate::runtime::BYPASS_HEADER, "secret": secret }))
    })
    .await?;
    e.kick();
    let mut r = (StatusCode::CREATED, Json(v)).into_response();
    r.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    Ok(r)
}

async fn delete_bypass(AxState(e): S, Path(project): Path<String>) -> crate::Result<StatusCode> {
    blocking(e.clone(), move |e| {
        e.workspace_project(&project)?;
        e.store.delete_bypass(&project)?;
        reconverge(e, &project)
    })
    .await?;
    e.kick();
    Ok(StatusCode::NO_CONTENT)
}

/// `{hooks: [{id, name, branches, created_at, fired_at}]}`: never their secrets.
async fn hooks(AxState(e): S, Path(project): Path<String>) -> crate::Result<Json<Value>> {
    let hooks = blocking(e, move |e| {
        e.workspace_project(&project)?;
        e.store.hooks(&project)
    })
    .await?;
    Ok(Json(json!({ "hooks": hooks })))
}

#[derive(Deserialize)]
struct NewHook {
    name: String,
    #[serde(default = "every_branch")]
    branches: Vec<String>,
}

fn every_branch() -> Vec<String> {
    vec!["*".into()]
}

/// `{name, branches?}` → 201 `{hook, path}`: the path (`/v1/previews/hooks/rlb_…`) is shown
/// only here.
async fn create_hook(AxState(e): S, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let b: NewHook = serde_json::from_slice(&b).map_err(|err| Error::BadRequest(format!("expected {{name, branches?}}: {err}")))?;
    let (hook, path) = blocking(e, move |e| e.create_hook(&project, &b.name, &b.branches)).await?;
    let mut r = (StatusCode::CREATED, Json(json!({ "hook": hook, "path": path }))).into_response();
    r.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    Ok(r)
}

async fn delete_hook(AxState(e): S, Path((project, hook)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(e, move |e| {
        e.workspace_project(&project)?;
        e.store.delete_hook(&project, &hook)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A deploy hook fired: 202 `{rebuilt: [preview ids], coalesced}`; anything but a live hook is
/// a 404. The body is ignored (a CMS sends its own payload).
async fn run_hook(AxState(e): S, Path(secret): Path<String>, req: axum::extract::Request) -> crate::Result<Response> {
    let from = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string());
    let (rebuilt, coalesced) = blocking(e, move |e| e.run_hook(&secret, from.as_deref())).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "rebuilt": rebuilt, "coalesced": coalesced }))).into_response())
}

/// Ask every live preview of `project` to converge again.
fn reconverge(e: &Engine, project: &str) -> crate::Result<()> {
    for r in e.store.rows(Some(project))? {
        if r.desired == crate::Desired::Active {
            e.store.update(&r.id, |row| row.next_at = 0)?;
        }
    }
    Ok(())
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
        // This month against the budget: live previews and the ones deleted this month.
        let now = e.store.now_ms()?;
        let spent = e.store.spent(&project, crate::store::month_start(now))? + t.month_to_date_usd;
        let budget = e.store.settings(&project)?.max_usd_month;
        Ok(json!({
            "previews": n,
            "sleeping_usd_month": t.sleeping_usd_month,
            "active_usd_hour": t.active_usd_hour,
            "month_to_date_usd": t.month_to_date_usd,
            "bytes": t.bytes,
            "spent_usd_month": (spent * 1e6).round() / 1e6,
            "max_usd_month": budget,
        }))
    })
    .await
    .map(Json)
}

#[cfg(test)]
mod tests {
    /// A route's id read by the middlewares is the one the handler's `Path` reads: a client may
    /// percent-encode it (`pv%5Fabc`), and a `+` in a path is itself.
    #[test]
    fn path_segments_are_read_as_the_handlers_read_them() {
        assert_eq!(super::segments("/v1/previews/pv%5Fabc/logs"), ["v1", "previews", "pv_abc", "logs"]);
        assert_eq!(super::segments("/a+b/%2"), ["a+b", "%2"]);
    }
}

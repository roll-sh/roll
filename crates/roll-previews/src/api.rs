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
        .route("/v1/previews/{id}", get(one).delete(delete))
        .route("/v1/previews/{id}/refresh", post(refresh))
        .route("/v1/previews/{id}/keep", post(keep))
        .route("/v1/previews/{id}/visual/accept", post(accept_visual))
        .route("/v1/previews/{id}/env", get(vars).put(put_vars))
        .route("/v1/previews/{id}/shares", get(shares).post(share))
        .route("/v1/previews/{id}/shares/{share}", axum::routing::delete(unshare))
        .route("/v1/previews/{id}/screenshot", get(screenshot))
        .route("/v1/previews/{id}/qr.png", get(qr))
        .route("/v1/previews/{id}/connect", get(connect))
        .route("/v1/previews/{id}/notes", get(notes))
        .route("/v1/previews/{id}/notes/{note}/resolve", post(resolve_note))
        .route("/v1/previews/{id}/mail", get(mails))
        .route("/v1/previews/{id}/mail/{mail}", get(mail_raw))
        .route("/v1/previews/{id}/screenshot.png", get(screenshot_link))
        .route("/v1/previews/{id}/reset", post(reset))
        // These act on running services, which live on the preview's holder in a cluster.
        .merge(
            Router::new()
                .route("/v1/previews/{id}/sleep", post(sleep))
                .route("/v1/previews/{id}/wake", post(wake))
                .route("/v1/previews/{id}/logs", get(logs))
                .route_layer(axum::middleware::from_fn_with_state(engine.clone(), on_leader))
                .with_state(engine.clone()),
        )
        .route("/v1/previews/{id}/activity", get(activity))
        .route("/v1/previews/projects/{project}/settings", get(settings).put(put_settings))
        .route("/v1/previews/projects/{project}/usage", get(usage))
        .route("/v1/previews/projects/{project}/bypass", get(bypass).post(rotate_bypass).delete(delete_bypass))
        .route_layer(axum::middleware::from_fn_with_state(engine.clone(), scoped))
        .with_state(engine)
}

/// The projects a signed-in person who isn't an admin may see previews of: their own, as the
/// workspace API scopes them. `None`: every project (tokens, the operator, admins).
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
    let visible = match who {
        None => Visible(None),
        Some(user) => {
            let e2 = e.clone();
            match blocking(e2, move |e| e.projects_of(&user)).await {
                Ok(ids) => Visible(Some(ids)),
                Err(err) => return err.into_response(),
            }
        }
    };
    let path = req.uri().path().to_string();
    let rest: Vec<&str> = path.trim_start_matches("/v1/previews").trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
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
    next.run(req).await
}

/// Run on the preview's holder (`preview:<id>`), which runs its services, forwarding if need be.
async fn on_leader(AxState(e): S, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let id = req.uri().path().split('/').nth(3).unwrap_or_default().to_string();
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
struct CreateBody {
    project: String,
    repo: String,
    branch: Option<String>,
    change: Option<String>,
}

async fn create(AxState(e): S, axum::Extension(visible): axum::Extension<Visible>, who: Option<axum::Extension<roll_auth::SignedIn>>, b: Bytes) -> crate::Result<Response> {
    let b: CreateBody = body(&b)?;
    if b.branch.is_some() == b.change.is_some() {
        return Err(Error::BadRequest("give one of branch or change".into()));
    }
    if !visible.sees(&b.project) {
        return Err(Error::NotFound("no such preview or project".into()));
    }
    let by = who.map(|axum::Extension(u)| u.handle).unwrap_or_else(|| "operator".into());
    let (p, made) = e.create(&b.project, &b.repo, b.branch.as_deref(), b.change.as_deref(), &by).await?;
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
    let behind = who.as_ref().map(|u| u.handle.clone());
    let (by, person) = match (who, grant) {
        (Some(axum::Extension(u)), None) => (u.handle, true),
        (_, Some(axum::Extension(g))) => (format!("token:{}", g.name), false),
        (None, None) => ("operator".into(), true),
    };
    e.accept_visual(&id, &by, behind.as_deref(), person, seen.as_deref(), path.as_deref()).await.map(Json)
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
async fn share(AxState(e): S, Path(id): Path<String>, who: Option<axum::Extension<roll_auth::SignedIn>>, b: Bytes) -> crate::Result<Response> {
    check_id(&id)?;
    let s: ShareBody = body(&b)?;
    let by = who.map(|axum::Extension(u)| u.handle).unwrap_or_else(|| "operator".into());
    let (share, url) = blocking(e, move |e| e.share(&id, s.hours, &by)).await?;
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

async fn put_vars(AxState(e): S, Path(id): Path<String>, b: Bytes) -> crate::Result<Json<serde_json::Value>> {
    check_id(&id)?;
    let v: VarsBody = body(&b)?;
    let vars = e.set_vars(&id, v.vars).await?;
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
    // `diff=base`: where it differs from the baseline's, under its own signature.
    let variant = match q.get("diff").map(String::as_str) {
        None => None,
        Some("base") => Some("diff=base"),
        Some(_) => return Ok((StatusCode::FORBIDDEN, "not a valid link").into_response()),
    };
    let shot = blocking(e, move |e| {
        if !e.store.screenshot_link_ok(&id, exp, &sig, variant)? {
            return Ok(None);
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

/// A service's last lines; with `follow`, server-sent events: `event: service` (its name), then
/// one event per line as it comes, with keep-alives.
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
    let rest = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|l| (Ok(Event::default().data(l.replace(['\r', '\n'], ""))), rx)) });
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
    let (p, check) = (project.clone(), patch.clone());
    let (old, new) = blocking(e.clone(), move |e| {
        e.workspace_project(&p)?;
        let old = e.store.settings(&p)?;
        let new = old.patched(&check)?;
        Ok((old, new))
    })
    .await?;
    e.check_masks(&old, &new).await?;
    let s = blocking(e.clone(), move |e| {
        let s = e.store.settings(&project)?.patched(&patch)?;
        let s = e.store.put_settings(&project, &s)?;
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

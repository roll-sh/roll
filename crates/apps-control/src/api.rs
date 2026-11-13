//! `/v1/apps` (docs/apps/http-api.md). JSON in and out, errors as `{"error": …}`.
//!
//! Every route under `/v1/apps/services/{id}` runs on the service's owner: a node that isn't
//! forwards it there (`Cluster::dispatch`), so the deploy queue, the instances and the live
//! logs are always the owner's. No route checks a token: rolld's gate does (docs/apps/README.md,
//! "Permissions and MCP").

use crate::logs::LogLine;
use crate::model::{By, Trigger};
use crate::{Apps, Error, lease_key};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{Extensions, HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

type S = State<Arc<Apps>>;
type R = Result<Response, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Error::Db(_) | Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::warn!(error = %self, "apps: internal error");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

fn body(b: &Bytes) -> Result<Value, Error> {
    if b.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(b).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))
}

fn ok<T: serde::Serialize>(v: T) -> R {
    Ok(Json(v).into_response())
}

pub fn router(apps: Arc<Apps>) -> Router {
    let env = "/v1/apps/services/{id}/environments/{env}";
    let service_routes = Router::new()
        .route("/v1/apps/services/{id}", get(get_service).patch(patch_service).delete(delete_service))
        .route("/v1/apps/services/{id}/links", get(links))
        .route("/v1/apps/services/{id}/activity", get(activity))
        .route("/v1/apps/services/{id}/links/{kind}/{ref}", put(put_link).delete(delete_link))
        .route("/v1/apps/events", get(events))
        .route("/v1/apps/services/{id}/changes", get(changes))
        .route("/v1/apps/projects/{project}/notifications", get(targets).post(add_target))
        .route("/v1/apps/projects/{project}/limits", get(limits).put(set_limits))
        .route("/v1/apps/projects/{project}/drains", get(drains).post(add_drain))
        .route("/v1/apps/projects/{project}/drains/{did}", axum::routing::delete(delete_drain))
        .route("/v1/apps/projects/{project}/notifications/{nid}", axum::routing::delete(delete_target))
        .route("/v1/apps/projects/{project}/notifications/{nid}/test", post(test_target))
        .route("/v1/apps/projects/{project}/notifications/{nid}/deliveries", get(deliveries))
        .route("/v1/apps/services/{id}/registry", put(put_registry).delete(delete_registry))
        .route("/v1/apps/services/{id}/environments", get(environments))
        .route(env, get(get_env).put(put_env).delete(delete_env))
        .route(&format!("{env}/deploys"), get(deploys).post(create_deploy).layer(DefaultBodyLimit::max(apps.cfg.upload_max)))
        .route(&format!("{env}/deploys/{{dep}}"), get(get_deploy))
        .route(&format!("{env}/deploys/{{dep}}/logs"), get(deploy_logs))
        .route(&format!("{env}/deploys/{{dep}}/rollback"), post(rollback))
        .route(&format!("{env}/deploys/{{dep}}/cancel"), post(cancel))
        .route(&format!("{env}/redeploy"), post(redeploy))
        .route(&format!("{env}/promote"), post(promote))
        .route(&format!("{env}/volume/backups"), get(backups).post(backup_volume))
        .route(&format!("{env}/volume/backups/{{bid}}/restore"), post(restore_volume))
        .route(&format!("{env}/restart"), post(restart))
        .route(&format!("{env}/sleep"), post(sleep))
        .route(&format!("{env}/wake"), post(wake))
        .route(&format!("{env}/variables"), get(variables).patch(change_variables))
        .route(&format!("{env}/variables/values"), get(reveal))
        .route(&format!("{env}/logs"), get(env_logs))
        .route(&format!("{env}/metrics"), get(metrics))
        .route(&format!("{env}/usage"), get(usage))
        .route(&format!("{env}/runs"), get(runs).post(run_now))
        .route(&format!("{env}/shell"), get(shell))
        .route(&format!("{env}/shell/sessions"), post(open_session))
        .route(&format!("{env}/shell/sessions/{{sh}}"), axum::routing::delete(close_session))
        .route(&format!("{env}/shell/sessions/{{sh}}/screen"), get(session_screen))
        .route(&format!("{env}/shell/sessions/{{sh}}/keys"), post(session_keys))
        .route(&format!("{env}/shell/sessions/{{sh}}/resize"), post(session_resize))
        .route(&format!("{env}/jobs"), get(jobs).post(start_job))
        .route(&format!("{env}/jobs/{{job}}"), get(get_job))
        .route(&format!("{env}/jobs/{{job}}/logs"), get(job_logs))
        .route(&format!("{env}/jobs/{{job}}/cancel"), post(cancel_job))
        .route(&format!("{env}/domains"), get(domains).post(add_domain))
        .route(&format!("{env}/domains/{{host}}/verify"), post(verify_domain))
        .route(&format!("{env}/domains/{{host}}"), axum::routing::delete(delete_domain).patch(patch_domain))
        .route_layer(axum::middleware::from_fn_with_state(apps.clone(), on_owner));
    Router::new()
        .route("/v1/apps", get(overview))
        .route("/v1/apps/plan", post(plan))
        .route("/v1/apps/services", get(list_services).post(create_service))
        .route("/v1/apps/projects/{project}/services", get(project_services).post(create_project_service))
        .merge(service_routes)
        .with_state(apps)
}

/// Run a service's route on its owner.
async fn on_owner(State(apps): S, req: Request, next: Next) -> Response {
    let Some(id) = req.uri().path().strip_prefix("/v1/apps/services/").map(|r| r.split('/').next().unwrap_or("").to_string()) else {
        return next.run(req).await;
    };
    let cluster = apps.deps.cluster.clone();
    cluster.dispatch(&lease_key(&id), req, |req, _guard| next.run(req)).await
}

async fn overview(State(apps): S) -> R {
    let services = apps.services(None).await?;
    let mut o = apps.overview();
    o["services"] = serde_json::to_value(services).expect("JSON");
    ok(o)
}

/// `POST /v1/apps/plan`: what a build would detect, for a repo at a revision or for files.
async fn plan(State(apps): S, b: Bytes) -> R {
    let v = body(&b)?;
    let overrides: crate::model::Build = crate::model::merge(&Default::default(), v.get("build"), "build")?;
    if let Some(files) = v.get("files") {
        let map: std::collections::BTreeMap<String, String> = serde_json::from_value(files.clone()).map_err(|e| Error::BadRequest(format!("files: {e}")))?;
        if map.len() > 2000 || map.values().map(String::len).sum::<usize>() > 8 << 20 {
            return Err(Error::BadRequest("files is at most 2000 files and 8 MiB".into()));
        }
        let p = apps_build::plan(&apps_build::MapTree(map), &overrides.overrides()).map_err(|e| Error::BadRequest(e.to_string()))?;
        return ok(p);
    }
    let repo = v.get("repo").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("repo or files is required".into()))?.to_string();
    let sources = apps.deps.sources.clone().ok_or_else(|| Error::BadRequest("roll code isn't on this node".into()))?;
    let rev = match v.get("revision").and_then(Value::as_str) {
        Some(r) => sources.revision(&repo, r).await,
        None => sources.resolve(&repo, None).await,
    }
    .map_err(|e| Error::BadRequest(format!("{repo}: {e}")))?;
    let dir = apps.cfg.home.join("apps").join("plans").join(crate::model::random(12));
    tokio::fs::create_dir_all(&dir).await.map_err(anyhow::Error::from)?;
    let result = async {
        sources.materialize(&repo, &rev.sha, &dir).await?;
        let root = match v.get("root").and_then(Value::as_str) {
            Some(r) => crate::model::in_tree(&dir, "root", r, true)?,
            None => dir.clone(),
        };
        // What a build would use: roll.toml `[app]` over the settings sent, said in the notes.
        let (mut build, mut said) = match crate::config::read(&root)? {
            Some(app) => {
                let (b, _, said) = app.apply(&overrides, &crate::model::Run::default(), None)?;
                (b, said)
            }
            None => (overrides, Vec::new()),
        };
        // As a build does: a root inside a JS workspace plans from the repo's root.
        let mut context = root.clone();
        if let Some(r) = v.get("root").and_then(Value::as_str)
            && build.builder == crate::model::Builder::Auto
            && std::fs::symlink_metadata(root.join("Dockerfile")).is_err()
            && let Some(ws) = apps_build::workspace_app(&apps_build::DirTree::new(&dir), r)
        {
            said.push(ws.note);
            build.build = build.build.or(ws.build);
            build.start = build.start.or(ws.start);
            context = dir.clone();
        }
        let o = build.overrides();
        let p = tokio::task::spawn_blocking(move || apps_build::plan(&apps_build::DirTree::new(context), &o)).await.map_err(|e| Error::Internal(e.to_string()))?;
        p.map(|mut p| {
            p.notes.splice(0..0, said);
            p
        })
        .map_err(|e| Error::BadRequest(e.to_string()))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    let p: apps_build::Plan = match result {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    ok(p)
}

#[derive(Deserialize)]
struct ProjectQuery {
    project: Option<String>,
}

async fn list_services(State(apps): S, Query(q): Query<ProjectQuery>) -> R {
    ok(apps.services(q.project).await?)
}

async fn project_services(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.services(Some(project)).await?)
}

async fn create_service(State(apps): S, ext: Extensions, b: Bytes) -> R {
    let s = apps.create_service(body(&b)?, actor(&ext)).await?;
    Ok((StatusCode::CREATED, Json(s)).into_response())
}

async fn create_project_service(State(apps): S, Path(project): Path<String>, ext: Extensions, b: Bytes) -> R {
    let mut v = body(&b)?;
    if let Some(p) = v.get("project").and_then(Value::as_str)
        && p != project
    {
        return Err(Error::BadRequest("project in the body isn't the path's".into()));
    }
    v["project"] = Value::String(project);
    let s = apps.create_service(v, actor(&ext)).await?;
    Ok((StatusCode::CREATED, Json(s)).into_response())
}

async fn get_service(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.service(&id).await?)
}

/// The keys a JSON patch sets (`run.min_instances`, `name`), for the audit trail.
fn changed(v: &Value) -> Vec<String> {
    match v.as_object() {
        Some(o) => o.iter().flat_map(|(k, v)| match v.as_object() {
            Some(inner) if !inner.is_empty() => inner.keys().map(|i| format!("{k}.{i}")).collect::<Vec<_>>(),
            _ => vec![k.clone()],
        }).collect(),
        None => Vec::new(),
    }
}

async fn patch_service(State(apps): S, Path(id): Path<String>, ext: Extensions, b: Bytes) -> R {
    let patch = body(&b)?;
    let s = apps.patch_service(&id, patch.clone()).await?;
    apps.settings_changed();
    apps.audit("settings", &id, "", actor(&ext), json!({ "changed": changed(&patch) })).await;
    ok(s)
}

async fn delete_service(State(apps): S, Path(id): Path<String>) -> R {
    apps.delete_service(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn links(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.links(&id).await?)
}

async fn activity(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.activity(&id).await?)
}

async fn put_link(State(apps): S, Path((id, kind, r)): Path<(String, String, String)>, ext: Extensions, b: Bytes) -> R {
    let l = apps.put_link(&id, &kind, &r, body(&b)?).await?;
    apps.settings_changed();
    apps.audit("link.added", &id, "", actor(&ext), json!({ "kind": kind, "ref": r })).await;
    ok(l)
}

async fn delete_link(State(apps): S, Path((id, kind, r)): Path<(String, String, String)>, ext: Extensions) -> R {
    apps.delete_link(&id, &kind, &r).await?;
    apps.settings_changed();
    apps.audit("link.removed", &id, "", actor(&ext), json!({ "kind": kind, "ref": r })).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn drains(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.drains(&project).await?)
}

async fn add_drain(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    Ok((StatusCode::CREATED, Json(apps.add_drain(&project, body(&b)?).await?)).into_response())
}

async fn delete_drain(State(apps): S, Path((project, id)): Path<(String, String)>) -> R {
    apps.delete_drain(&project, &id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn limits(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.limits(&project).await?)
}

async fn set_limits(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    ok(apps.set_limits(&project, body(&b)?).await?)
}

async fn targets(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.targets(&project).await?)
}

async fn add_target(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    let t = apps.add_target(&project, body(&b)?).await?;
    Ok((StatusCode::CREATED, Json(t)).into_response())
}

async fn delete_target(State(apps): S, Path((project, nid)): Path<(String, String)>) -> R {
    apps.delete_target(&project, &nid).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn test_target(State(apps): S, Path((project, nid)): Path<(String, String)>) -> R {
    ok(apps.test_target(&project, &nid).await?)
}

async fn deliveries(State(apps): S, Path((project, nid)): Path<(String, String)>) -> R {
    ok(apps.deliveries(&project, &nid).await?)
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<i64>,
    limit: Option<usize>,
}

/// `GET /v1/apps/events?after=&limit=`: what happened to services, in order, after a cursor;
/// `next` is the cursor for the following call.
async fn events(State(apps): S, Query(q): Query<EventsQuery>) -> R {
    let after = q.after.unwrap_or(0);
    let limit = q.limit.unwrap_or(100);
    let events = apps.db(move |s| s.events(after, limit)).await?;
    let next = events.last().map_or(after, |e| e.seq);
    ok(json!({ "events": events, "next": next }))
}

/// `GET /v1/apps/services/{id}/changes`: server-sent `changed` events, one when something about
/// the service changed (a deploy moved, an instance woke or slept, a setting changed), so a page
/// reads it again instead of polling. `ready` first; nothing in them but the event.
async fn changes(State(apps): S, Path(id): Path<String>) -> R {
    apps.service(&id).await?;
    let (tx, rx) = tokio::sync::mpsc::channel::<&'static str>(4);
    tokio::spawn(async move {
        let mut sub = apps.subscribe_changes();
        let mut last = apps.instances_digest(&id);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        if tx.send("ready").await.is_err() {
            return;
        }
        loop {
            let changed = tokio::select! {
                r = sub.recv() => match r {
                    Ok(s) => s == id,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(_) => return,
                },
                _ = tick.tick() => {
                    let now = apps.instances_digest(&id);
                    let moved = now != last;
                    last = now;
                    moved
                }
                _ = tx.closed() => return,
            };
            // A full channel is a viewer behind on ticks it would read as one anyway.
            if changed && let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = tx.try_send("changed") {
                return;
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        let e = rx.recv().await?;
        Some((Ok::<_, Infallible>(Event::default().event(e).data("{}")), rx))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}

async fn put_registry(State(apps): S, Path(id): Path<String>, ext: Extensions, b: Bytes) -> R {
    let s = apps.put_registry(&id, body(&b)?).await?;
    apps.audit("registry.set", &id, "", actor(&ext), json!({ "server": s.registry.as_ref().map(|r| r.server.clone()) })).await;
    ok(s)
}

async fn delete_registry(State(apps): S, Path(id): Path<String>, ext: Extensions) -> R {
    let s = apps.delete_registry(&id).await?;
    apps.audit("registry.removed", &id, "", actor(&ext), json!({})).await;
    ok(s)
}

async fn environments(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.environments(&id).await?)
}

async fn get_env(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.environment(&id, &env).await?)
}

async fn put_env(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let put = body(&b)?;
    let e = apps.put_environment(&id, &env, put.clone()).await?;
    apps.settings_changed();
    // The access token is a secret: the trail says it changed, never what it is.
    apps.audit("environment", &id, &env, actor(&ext), json!({ "changed": put.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() })).await;
    ok(e)
}

async fn delete_env(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions) -> R {
    apps.delete_environment(&id, &env).await?;
    apps.audit("environment.removed", &id, &env, actor(&ext), json!({})).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn deploys(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.deployments(&id, &env).await?)
}

/// JSON (`NewDeploy`), or a gzipped tar of the tree (`roll up`).
async fn create_deploy(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, headers: HeaderMap, b: Bytes) -> R {
    let actor = actor(&ext);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let key = match headers.get("idempotency-key") {
        Some(v) => Some(v.to_str().map_err(|_| Error::BadRequest("Idempotency-Key is printable ASCII".into()))?),
        None => None,
    };
    let d = if ct.starts_with("application/gzip") || ct.starts_with("application/x-gzip") {
        // Percent-encoded UTF-8 (a commit subject isn't ASCII): `roll up` sends it so.
        let message = headers.get("x-roll-message").and_then(|v| v.to_str().ok()).map(|m| percent_encoding::percent_decode_str(m).decode_utf8_lossy().into_owned()).unwrap_or_default();
        let revision = headers.get("x-roll-revision").and_then(|v| v.to_str().ok()).filter(|r| (7..=64).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_hexdigit()));
        apps.deploy(&id, &env, json!({ "message": message, "revision": revision }), By { trigger: Trigger::Upload, actor }, Some(b), key).await?
    } else {
        apps.deploy(&id, &env, body(&b)?, By { trigger: Trigger::Manual, actor }, None, key).await?
    };
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn get_deploy(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>) -> R {
    ok(apps.deployment(&id, &env, &dep).await?)
}

#[derive(Deserialize)]
struct LogQuery {
    follow: Option<String>,
    tail: Option<usize>,
    since: Option<i64>,
    /// Lines that contain this (any case), from memory and the stored chunks.
    q: Option<String>,
    /// The proxy's access log instead of the runtime's (`http=1`).
    http: Option<String>,
    /// Access lines with this status: `500`, `5xx` or `400-499`.
    status: Option<String>,
    /// Access lines that took at least this long.
    min_ms: Option<u64>,
}

/// Which access lines `status` and `min_ms` keep (`None`: all of them).
fn http_filter(q: &LogQuery) -> crate::Result<Option<impl Fn(&crate::logs::LogLine) -> bool + Send + Sync + 'static>> {
    if q.status.is_none() && q.min_ms.is_none() {
        return Ok(None);
    }
    let range = match q.status.as_deref().map(str::trim) {
        None | Some("") => (0, 999),
        Some(s) if s.len() == 3 && s.ends_with("xx") => {
            let d: u16 = s[..1].parse().map_err(|_| Error::BadRequest("status is 500, 5xx or 400-499".into()))?;
            (d * 100, d * 100 + 99)
        }
        Some(s) => match s.split_once('-') {
            Some((a, b)) => (a.trim().parse().map_err(|_| Error::BadRequest("status is 500, 5xx or 400-499".into()))?, b.trim().parse().map_err(|_| Error::BadRequest("status is 500, 5xx or 400-499".into()))?),
            None => {
                let n: u16 = s.parse().map_err(|_| Error::BadRequest("status is 500, 5xx or 400-499".into()))?;
                (n, n)
            }
        },
    };
    let min = q.min_ms.unwrap_or(0);
    Ok(Some(move |l: &crate::logs::LogLine| {
        let v: Value = serde_json::from_str(&l.text).unwrap_or(Value::Null);
        let status = v["status"].as_u64().unwrap_or(0) as u16;
        (range.0..=range.1).contains(&status) && v["ms"].as_u64().unwrap_or(0) >= min
    }))
}

fn truthy(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"))
}

/// Lines as JSON, or (`follow`) as server-sent events: what's there, then what comes.
/// The lines `keep` lets through (none: all), as read and as they come.
type Keep = Option<Box<dyn Fn(&LogLine) -> bool + Send + Sync>>;

async fn stream_log(apps: &Arc<Apps>, key: String, q: LogQuery, done: Option<tokio::sync::watch::Receiver<bool>>, keep: Keep) -> R {
    let tail = q.tail.unwrap_or(500).min(crate::logs::RING);
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        if text.len() > 200 {
            return Err(Error::BadRequest("q is at most 200 characters".into()));
        }
        let (lines, cut) = apps.logs.search(&key, text, q.since, tail).await;
        let mut r = Json(lines).into_response();
        if cut {
            r.headers_mut().insert("x-roll-search-truncated", axum::http::HeaderValue::from_static("1"));
        }
        return Ok(r);
    }
    if !truthy(&q.follow) {
        let Some(k) = &keep else { return ok(apps.logs.tail(&key, tail, q.since).await) };
        // Filtered: read back as far as memory holds, then the newest `tail` matches.
        let mut lines = apps.logs.tail(&key, crate::logs::RING, q.since).await;
        lines.retain(|l| k(l));
        let cut = lines.len().saturating_sub(tail);
        return ok(lines.split_off(cut));
    }
    // A finished build's log is read, not followed: nothing more comes, and following would
    // bring a retired log back into memory.
    let (mut first, mut rx) = match &done {
        Some(d) if *d.borrow() => (Vec::new(), tokio::sync::broadcast::channel(1).1),
        _ => apps.logs.follow(&key, tail),
    };
    if first.is_empty() {
        first = apps.logs.tail(&key, tail, q.since).await;
    }
    if let Some(s) = q.since {
        first.retain(|l| l.t >= s);
    }
    if let Some(k) = &keep {
        first.retain(|l| k(l));
    }
    let stream = async_stream(first, move |tx| async move {
        let mut done = done;
        loop {
            let next = async {
                match &mut done {
                    Some(d) => {
                        tokio::select! {
                            l = rx.recv() => Some(l),
                            _ = d.wait_for(|v| *v) => None,
                        }
                    }
                    None => Some(rx.recv().await),
                }
            };
            match next.await {
                Some(Ok(l)) => {
                    if keep.as_ref().is_some_and(|k| !k(&l)) {
                        continue;
                    }
                    if tx.send(l).await.is_err() {
                        return;
                    }
                }
                Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Some(Err(_)) | None => return,
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}

/// An SSE stream of `first`, then whatever `more` sends.
fn async_stream<F, Fut>(first: Vec<LogLine>, more: F) -> impl futures::Stream<Item = Result<Event, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<LogLine>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        for l in first {
            if tx.send(l).await.is_err() {
                return;
            }
        }
        more(tx).await;
    });
    futures::stream::unfold(rx, |mut rx| async move {
        let l = rx.recv().await?;
        Some((Ok(Event::default().data(serde_json::to_string(&l).expect("JSON"))), rx))
    })
}

async fn deploy_logs(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>, Query(q): Query<LogQuery>) -> R {
    let d = apps.deployment(&id, &env, &dep).await?;
    // A follower of a build stops when the deploy is done.
    let done = if d.status.pending() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (apps2, depid) = (apps.clone(), dep.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let id = depid.clone();
                match apps2.db(move |s| s.deployment(&id)).await {
                    Ok(d) if d.status.pending() => {}
                    _ => break,
                }
                if tx.is_closed() {
                    return;
                }
            }
            // Let the last lines through.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx.send(true);
        });
        Some(rx)
    } else {
        let (_tx, rx) = tokio::sync::watch::channel(true);
        Some(rx)
    };
    stream_log(&apps, format!("build:{dep}"), q, done, None).await
}

async fn env_logs(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<LogQuery>) -> R {
    apps.environment(&id, &env).await?;
    if truthy(&q.http) {
        // The access log, filtered by status and duration when asked (read and followed).
        let keep: Keep = http_filter(&q)?.map(|f| Box::new(f) as Box<dyn Fn(&LogLine) -> bool + Send + Sync>);
        return stream_log(&apps, format!("http:{id}/{env}"), q, None, keep).await;
    }
    stream_log(&apps, format!("env:{id}/{env}"), q, None, None).await
}

/// Who made a request, for the deployments it makes: what rolld's gate put in its extensions
/// (the operator's token today; on roll-mcp a scoped token's or connection's name,
/// `roll_auth::Grant`). Nothing a client sends is believed.
fn actor(ext: &Extensions) -> Option<String> {
    roll_auth::Operator::on(ext).then(|| "operator".to_string())
}

async fn redeploy(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let d = apps.redeploy(&id, &env, if b.is_empty() { Value::Null } else { body(&b)? }, actor(&ext)).await?;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn backups(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.backups(&id, &env).await?)
}

async fn backup_volume(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions) -> R {
    let b = apps.backup_volume(&id, &env).await?;
    apps.audit("volume.backed_up", &id, &env, actor(&ext), json!({ "backup": b.id })).await;
    Ok((StatusCode::CREATED, Json(b)).into_response())
}

async fn restore_volume(State(apps): S, Path((id, env, bid)): Path<(String, String, String)>, ext: Extensions) -> R {
    let b = apps.restore_volume(&id, &env, &bid).await?;
    apps.audit("volume.restored", &id, &env, actor(&ext), json!({ "backup": b.id })).await;
    ok(b)
}

async fn promote(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let d = apps.promote(&id, &env, body(&b)?, actor(&ext)).await?;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn rollback(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>, ext: Extensions) -> R {
    let d = apps.rollback(&id, &env, &dep, actor(&ext)).await?;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn cancel(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>) -> R {
    ok(apps.cancel(&id, &env, &dep).await?)
}

async fn restart(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    let e = apps.restart(&id, &env).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

async fn sleep(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.sleep(&id, &env).await?)
}

async fn wake(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.wake(&id, &env).await?)
}

async fn variables(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.variables(&id, &env, false).await?)
}

async fn reveal(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions) -> R {
    let v = apps.variables(&id, &env, true).await?;
    apps.audit("variables.revealed", &id, &env, actor(&ext), json!({})).await;
    ok(v)
}

async fn change_variables(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let change = body(&b)?;
    let v = apps.change_variables(&id, &env, change.clone()).await?;
    apps.settings_changed();
    // Names only: a value never reaches the trail.
    let set: Vec<String> = change["set"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    apps.audit("variables", &id, &env, actor(&ext), json!({ "set": set, "unset": change["unset"] })).await;
    ok(v)
}

#[derive(Deserialize)]
struct Window {
    minutes: Option<i64>,
    days: Option<i64>,
}

async fn metrics(State(apps): S, Path((id, env)): Path<(String, String)>, Query(w): Query<Window>) -> R {
    ok(apps.metrics(&id, &env, w.minutes.unwrap_or(60)).await?)
}

async fn usage(State(apps): S, Path((id, env)): Path<(String, String)>, Query(w): Query<Window>) -> R {
    apps.environment(&id, &env).await?;
    ok(apps.usage(&id, Some(&env), w.days.unwrap_or(30)).await?)
}

#[derive(Deserialize)]
struct ShellQuery {
    instance: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// `GET …/shell` (a WebSocket): a shell in a running instance; see `crate::shell`.
async fn shell(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<ShellQuery>, ws: axum::extract::ws::WebSocketUpgrade) -> R {
    let slot = apps.shell_slot(&id, &env)?;
    let target = apps.shell_target(&id, &env, q.instance.as_deref()).await?;
    let (cols, rows) = (q.cols.unwrap_or(80).clamp(10, 1000), q.rows.unwrap_or(24).clamp(2, 500));
    // Keys and resizes are small: nothing near axum's 64 MiB default is held for a peer.
    let ws = ws.max_message_size(1 << 20).max_frame_size(1 << 20);
    Ok(ws.on_upgrade(move |socket| apps.bridge_shell(socket, target, slot, cols, rows)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenSession {
    instance: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// `POST …/shell/sessions`: a shell over HTTP (the dashboard's terminal); see `crate::shell`.
async fn open_session(State(apps): S, Path((id, env)): Path<(String, String)>, b: Bytes) -> R {
    let o: OpenSession = if b.is_empty() { OpenSession { instance: None, cols: None, rows: None } } else { serde_json::from_slice(&b).map_err(|e| Error::BadRequest(e.to_string()))? };
    let sh = apps.open_session(&id, &env, o.instance.as_deref(), o.cols.unwrap_or(80).clamp(10, 1000), o.rows.unwrap_or(24).clamp(2, 500)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": sh }))).into_response())
}

/// The screen over SSE: each event a base64 chunk, the scrollback first; it ends with the shell.
async fn session_screen(State(apps): S, Path((id, env, sh)): Path<(String, String, String)>) -> R {
    use base64::Engine as _;
    let (first, rx, closed) = apps.session_screen(&id, &env, &sh)?;
    let b64 = |b: &[u8]| Event::default().data(base64::engine::general_purpose::STANDARD.encode(b));
    let head = futures::stream::iter((!first.is_empty()).then(|| Ok::<_, Infallible>(b64(&first))));
    let rest = futures::stream::unfold((rx, closed), move |(mut rx, closed)| async move {
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Ok(chunk) => return Some((Ok::<_, Infallible>(b64(&chunk)), (rx, closed))),
                    // A slow viewer missed some: carry on from what's next.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                },
                _ = closed.cancelled() => return None,
            }
        }
    });
    use futures::StreamExt as _;
    // The shell ended: say so, so the viewer stops instead of reconnecting.
    let end = futures::stream::once(async { Ok::<_, Infallible>(Event::default().event("closed").data("")) });
    Ok(Sse::new(head.chain(rest).chain(end)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}

async fn session_keys(State(apps): S, Path((id, env, sh)): Path<(String, String, String)>, b: Bytes) -> R {
    use base64::Engine as _;
    let v = body(&b)?;
    let data = v.get("data").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("data is base64".into()))?;
    let keys = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| Error::BadRequest("data is base64".into()))?;
    if keys.len() > 64 << 10 {
        return Err(Error::BadRequest("at most 64 KiB of keys at once".into()));
    }
    apps.session_keys(&id, &env, &sh, keys).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn session_resize(State(apps): S, Path((id, env, sh)): Path<(String, String, String)>, b: Bytes) -> R {
    let v = body(&b)?;
    let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|n| n.min(u16::MAX as u64) as u16).ok_or_else(|| Error::BadRequest(format!("{k} is a number")));
    apps.session_resize(&id, &env, &sh, n("cols")?, n("rows")?).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn close_session(State(apps): S, Path((id, env, sh)): Path<(String, String, String)>) -> R {
    apps.close_session(&id, &env, &sh)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn jobs(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.jobs(&id, &env).await?)
}

async fn start_job(State(apps): S, Path((id, env)): Path<(String, String)>, b: Bytes) -> R {
    let job = apps.start_job(&id, &env, body(&b)?).await?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

async fn get_job(State(apps): S, Path((id, env, job)): Path<(String, String, String)>) -> R {
    ok(apps.job(&id, &env, &job).await?)
}

async fn cancel_job(State(apps): S, Path((id, env, job)): Path<(String, String, String)>) -> R {
    ok(apps.cancel_job(&id, &env, &job).await?)
}

/// A job's output; `follow` streams it until the job ends.
async fn job_logs(State(apps): S, Path((id, env, job)): Path<(String, String, String)>, Query(q): Query<LogQuery>) -> R {
    let j = apps.job(&id, &env, &job).await?;
    let (tx, rx) = tokio::sync::watch::channel(j.status != crate::model::RunStatus::Running);
    if j.status == crate::model::RunStatus::Running {
        let (apps2, id2, env2, job2) = (apps.clone(), id.clone(), env.clone(), job.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                match apps2.job(&id2, &env2, &job2).await {
                    Ok(j) if j.status == crate::model::RunStatus::Running => {}
                    _ => break,
                }
                if tx.is_closed() {
                    return;
                }
            }
            // Let the last lines through.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx.send(true);
        });
    }
    stream_log(&apps, format!("job:{job}"), q, Some(rx), None).await
}

async fn runs(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    apps.environment(&id, &env).await?;
    let (s, e) = (id.clone(), env.clone());
    ok(apps.db(move |st| st.runs(&s, &e, 50)).await?)
}

async fn run_now(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    let r = apps.run_cron(&id, &env).await?;
    Ok((StatusCode::ACCEPTED, Json(r)).into_response())
}

async fn domains(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.domains(&id, &env).await?)
}

async fn add_domain(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let d = apps.add_domain(&id, &env, body(&b)?).await?;
    apps.audit("domain.added", &id, &env, actor(&ext), json!({ "hostname": d.hostname })).await;
    Ok((StatusCode::CREATED, Json(d)).into_response())
}

async fn verify_domain(State(apps): S, Path((id, env, host)): Path<(String, String, String)>) -> R {
    ok(apps.verify_domain(&id, &env, &host).await?)
}

async fn patch_domain(State(apps): S, Path((id, env, host)): Path<(String, String, String)>, ext: Extensions, b: Bytes) -> R {
    let d = apps.set_domain_redirect(&id, &env, &host, body(&b)?).await?;
    apps.audit("domain.redirect", &id, &env, actor(&ext), json!({ "hostname": d.hostname, "redirect_to": d.redirect_to })).await;
    ok(d)
}

async fn delete_domain(State(apps): S, Path((id, env, host)): Path<(String, String, String)>, ext: Extensions) -> R {
    apps.delete_domain(&id, &env, &host).await?;
    apps.audit("domain.removed", &id, &env, actor(&ext), json!({ "hostname": host })).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

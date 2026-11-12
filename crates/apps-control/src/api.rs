//! `/v1/apps` (docs/apps/http-api.md). JSON in and out, errors as `{"error": …}`.
//!
//! Every route under `/v1/apps/services/{id}` runs on the service's owner: a node that isn't
//! forwards it there (`Cluster::dispatch`), so the deploy queue, the instances and the live
//! logs are always the owner's. No route checks a token: rolld's gate does (docs/apps/README.md,
//! "Permissions and MCP").

use crate::logs::LogLine;
use crate::model::Trigger;
use crate::{Apps, Error, lease_key};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
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
        .route("/v1/apps/services/{id}/links/{kind}/{ref}", put(put_link).delete(delete_link))
        .route("/v1/apps/services/{id}/environments", get(environments))
        .route(env, get(get_env).put(put_env).delete(delete_env))
        .route(&format!("{env}/deploys"), get(deploys).post(create_deploy).layer(DefaultBodyLimit::max(apps.cfg.upload_max)))
        .route(&format!("{env}/deploys/{{dep}}"), get(get_deploy))
        .route(&format!("{env}/deploys/{{dep}}/logs"), get(deploy_logs))
        .route(&format!("{env}/deploys/{{dep}}/rollback"), post(rollback))
        .route(&format!("{env}/deploys/{{dep}}/cancel"), post(cancel))
        .route(&format!("{env}/redeploy"), post(redeploy))
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
        .route(&format!("{env}/domains/{{host}}"), axum::routing::delete(delete_domain))
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
        let (build, said) = match crate::config::read(&root)? {
            Some(app) => {
                let (b, _, said) = app.apply(&overrides, &crate::model::Run::default())?;
                (b, said)
            }
            None => (overrides, Vec::new()),
        };
        let o = build.overrides();
        let p = tokio::task::spawn_blocking(move || apps_build::plan(&apps_build::DirTree::new(root), &o)).await.map_err(|e| Error::Internal(e.to_string()))?;
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

async fn create_service(State(apps): S, b: Bytes) -> R {
    let s = apps.create_service(body(&b)?).await?;
    Ok((StatusCode::CREATED, Json(s)).into_response())
}

async fn create_project_service(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    let mut v = body(&b)?;
    if let Some(p) = v.get("project").and_then(Value::as_str)
        && p != project
    {
        return Err(Error::BadRequest("project in the body isn't the path's".into()));
    }
    v["project"] = Value::String(project);
    let s = apps.create_service(v).await?;
    Ok((StatusCode::CREATED, Json(s)).into_response())
}

async fn get_service(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.service(&id).await?)
}

async fn patch_service(State(apps): S, Path(id): Path<String>, b: Bytes) -> R {
    ok(apps.patch_service(&id, body(&b)?).await?)
}

async fn delete_service(State(apps): S, Path(id): Path<String>) -> R {
    apps.delete_service(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn links(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.links(&id).await?)
}

async fn put_link(State(apps): S, Path((id, kind, r)): Path<(String, String, String)>, b: Bytes) -> R {
    ok(apps.put_link(&id, &kind, &r, body(&b)?).await?)
}

async fn delete_link(State(apps): S, Path((id, kind, r)): Path<(String, String, String)>) -> R {
    apps.delete_link(&id, &kind, &r).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn environments(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.environments(&id).await?)
}

async fn get_env(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.environment(&id, &env).await?)
}

async fn put_env(State(apps): S, Path((id, env)): Path<(String, String)>, b: Bytes) -> R {
    ok(apps.put_environment(&id, &env, body(&b)?).await?)
}

async fn delete_env(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    apps.delete_environment(&id, &env).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn deploys(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.deployments(&id, &env).await?)
}

/// JSON (`NewDeploy`), or a gzipped tar of the tree (`roll up`).
async fn create_deploy(State(apps): S, Path((id, env)): Path<(String, String)>, headers: HeaderMap, b: Bytes) -> R {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let key = match headers.get("idempotency-key") {
        Some(v) => Some(v.to_str().map_err(|_| Error::BadRequest("Idempotency-Key is printable ASCII".into()))?),
        None => None,
    };
    let d = if ct.starts_with("application/gzip") || ct.starts_with("application/x-gzip") {
        // Percent-encoded UTF-8 (a commit subject isn't ASCII): `roll up` sends it so.
        let message = headers.get("x-roll-message").and_then(|v| v.to_str().ok()).map(|m| percent_encoding::percent_decode_str(m).decode_utf8_lossy().into_owned()).unwrap_or_default();
        let revision = headers.get("x-roll-revision").and_then(|v| v.to_str().ok()).filter(|r| (7..=64).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_hexdigit()));
        apps.deploy(&id, &env, json!({ "message": message, "revision": revision }), Trigger::Upload, Some(b), key).await?
    } else {
        apps.deploy(&id, &env, body(&b)?, Trigger::Manual, None, key).await?
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
}

fn truthy(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"))
}

/// Lines as JSON, or (`follow`) as server-sent events: what's there, then what comes.
async fn stream_log(apps: &Arc<Apps>, key: String, q: LogQuery, done: Option<tokio::sync::watch::Receiver<bool>>) -> R {
    let tail = q.tail.unwrap_or(500).min(crate::logs::RING);
    if !truthy(&q.follow) {
        return ok(apps.logs.tail(&key, tail, q.since).await);
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
    stream_log(&apps, format!("build:{dep}"), q, done).await
}

async fn env_logs(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<LogQuery>) -> R {
    apps.environment(&id, &env).await?;
    stream_log(&apps, format!("env:{id}/{env}"), q, None).await
}

async fn redeploy(State(apps): S, Path((id, env)): Path<(String, String)>, b: Bytes) -> R {
    let d = apps.redeploy(&id, &env, if b.is_empty() { Value::Null } else { body(&b)? }).await?;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn rollback(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>) -> R {
    let d = apps.rollback(&id, &env, &dep).await?;
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

async fn reveal(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.variables(&id, &env, true).await?)
}

async fn change_variables(State(apps): S, Path((id, env)): Path<(String, String)>, b: Bytes) -> R {
    ok(apps.change_variables(&id, &env, body(&b)?).await?)
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
    let target = apps.shell_target(&id, &env, q.instance.as_deref()).await?;
    let (cols, rows) = (q.cols.unwrap_or(80).clamp(10, 1000), q.rows.unwrap_or(24).clamp(2, 500));
    Ok(ws.on_upgrade(move |socket| apps.bridge_shell(socket, target, cols, rows)).into_response())
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
    stream_log(&apps, format!("job:{job}"), q, Some(rx)).await
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

async fn add_domain(State(apps): S, Path((id, env)): Path<(String, String)>, b: Bytes) -> R {
    let d = apps.add_domain(&id, &env, body(&b)?).await?;
    Ok((StatusCode::CREATED, Json(d)).into_response())
}

async fn verify_domain(State(apps): S, Path((id, env, host)): Path<(String, String, String)>) -> R {
    ok(apps.verify_domain(&id, &env, &host).await?)
}

async fn delete_domain(State(apps): S, Path((id, env, host)): Path<(String, String, String)>) -> R {
    apps.delete_domain(&id, &env, &host).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

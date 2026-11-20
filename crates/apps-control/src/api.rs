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
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
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
        .route("/v1/apps/services/{id}/restore", post(restore_service))
        .route("/v1/apps/services/{id}/links", get(links))
        .route("/v1/apps/services/{id}/activity", get(activity))
        .route("/v1/apps/services/{id}/links/{kind}/{ref}", put(put_link).delete(delete_link))
        .route("/v1/apps/events", get(events))
        .route("/v1/apps/services/{id}/changes", get(changes))
        .route("/v1/apps/projects/{project}/notifications", get(targets).post(add_target))
        .route("/v1/apps/projects/{project}/limits", get(limits).put(set_limits))
        .route("/v1/apps/projects/{project}/drains", get(drains).post(add_drain))
        .route("/v1/apps/projects/{project}/export", post(export_project))
        .route("/v1/apps/projects/{project}/status-page", get(status_page).put(put_status_page).delete(delete_status_page))
        .route("/v1/apps/projects/{project}/status-page/incidents", get(status_incidents).post(post_status_incident))
        .route("/v1/apps/projects/{project}/status-page/incidents/{iid}/updates", post(update_status_incident))
        .route("/v1/apps/projects/{project}/preview-domain", get(preview_domain).post(set_preview_domain).delete(delete_preview_domain))
        .route("/v1/apps/projects/{project}/preview-domain/verify", post(verify_preview_domain))
        .route("/v1/apps/projects/{project}/drains/{did}", axum::routing::delete(delete_drain).patch(update_drain))
        .route("/v1/apps/projects/{project}/drains/{did}/test", post(test_drain))
        .route("/v1/apps/projects/{project}/notifications/{nid}", axum::routing::delete(delete_target).patch(update_target))
        .route("/v1/apps/projects/{project}/notifications/{nid}/test", post(test_target))
        .route("/v1/apps/projects/{project}/notifications/{nid}/deliveries", get(deliveries))
        .route("/v1/apps/services/{id}/registry", put(put_registry).delete(delete_registry))
        .route("/v1/apps/services/{id}/git-login", put(put_git_login).delete(delete_git_login))
        .route("/v1/apps/services/{id}/github", put(put_github).delete(delete_github))
        .route("/v1/apps/services/{id}/environments", get(environments))
        .route(env, get(get_env).put(put_env).delete(delete_env))
        .route(&format!("{env}/deploys"), get(deploys).post(create_deploy).layer(DefaultBodyLimit::max(apps.cfg.upload_max)))
        .route(&format!("{env}/deploys/{{dep}}"), get(get_deploy))
        .route(&format!("{env}/deploys/{{dep}}/logs"), get(deploy_logs))
        .route(&format!("{env}/deploys/{{dep}}/commits"), get(deploy_commits))
        .route(&format!("{env}/deploys/{{dep}}/vulnerabilities"), get(deploy_vulnerabilities))
        .route(&format!("{env}/deploys/{{dep}}/sbom"), get(deploy_sbom))
        .route(&format!("{env}/deploys/{{dep}}/rollback"), post(rollback))
        .route(&format!("{env}/deploys/{{dep}}/rollout"), post(rollout_action))
        .route(&format!("{env}/rollback"), post(rollback_previous))
        .route(&format!("{env}/deploys/{{dep}}/cancel"), post(cancel))
        .route(&format!("{env}/redeploy"), post(redeploy))
        .route(&format!("{env}/promote"), post(promote))
        .route(&format!("{env}/hook"), post(put_hook).delete(delete_hook))
        .route(&format!("{env}/volume/backups"), get(backups).post(backup_volume))
        .route(&format!("{env}/volume/backups/{{bid}}/restore"), post(restore_volume))
        .route(&format!("{env}/restart"), post(restart))
        .route(&format!("{env}/cache/purge"), post(purge_cache))
        .route(&format!("{env}/instances/{{name}}/restart"), post(restart_instance))
        .route(&format!("{env}/files"), get(read_files).put(write_files).layer(DefaultBodyLimit::max(crate::shell::FILES_MAX)))
        .route(&format!("{env}/sleep"), post(sleep))
        .route(&format!("{env}/wake"), post(wake))
        .route(&format!("{env}/variables"), get(variables).patch(change_variables))
        .route(&format!("{env}/diff"), get(env_diff))
        .route(&format!("{env}/pending"), get(pending))
        .route(&format!("{env}/variables/values"), get(reveal))
        .route(&format!("{env}/logs"), get(env_logs))
        .route(&format!("{env}/metrics"), get(metrics))
        .route(&format!("{env}/slo"), get(slo))
        .route(&format!("{env}/routes"), get(routes))
        .route(&format!("{env}/analytics"), get(analytics))
        .route(&format!("{env}/visit"), post(visit))
        .route(&format!("{env}/vitals"), get(vitals))
        .route(&format!("{env}/app-metrics"), get(app_metrics))
        .route(&format!("{env}/errors"), get(errors))
        .route(&format!("{env}/errors/{{fingerprint}}"), axum::routing::patch(error_state))
        .route(&format!("{env}/forms"), get(forms))
        .route(&format!("{env}/forms/{{form}}/submissions"), get(form_submissions))
        .route(&format!("{env}/forms/{{form}}/submissions/{{sub}}"), axum::routing::delete(delete_form_submission))
        .route(&format!("{env}/traces"), get(traces))
        .route(&format!("{env}/traces/{{trace}}"), get(trace))
        .route(&format!("{env}/usage"), get(usage))
        .route(&format!("{env}/runs"), get(runs).post(run_now))
        .route(&format!("{env}/runs/{{run}}"), get(get_run))
        .route(&format!("{env}/runs/{{run}}/logs"), get(run_logs))
        .route(&format!("{env}/runs/{{run}}/cancel"), post(cancel_run))
        .route(&format!("{env}/shell"), get(shell))
        .route(&format!("{env}/tunnel"), get(tunnel))
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
        .route("/v1/apps/import", post(import))
        .route("/v1/apps/services", get(list_services).post(create_service))
        .route("/v1/apps/projects/{project}/services", get(project_services).post(create_project_service))
        .merge(service_routes)
        .with_state(apps.clone())
        .merge(hooks_router(apps))
}

/// Run a service's route on its owner.
async fn on_owner(State(apps): S, req: Request, next: Next) -> Response {
    let Some(raw) = req.uri().path().strip_prefix("/v1/apps/services/").map(|r| r.split('/').next().unwrap_or("").to_string()) else {
        return next.run(req).await;
    };
    // The id as the handlers read it: `svc%5Fx` (a client may encode `_`) is `svc_x`, so one
    // service has one lease and one owner, however its id was spelled.
    let id = percent_encoding::percent_decode_str(&raw).decode_utf8_lossy().into_owned();
    // A deleted service is no service but to restore it or delete it for good.
    let root = req.uri().path().trim_end_matches('/') == format!("/v1/apps/services/{raw}");
    let keeps = (root && req.method() == axum::http::Method::DELETE) || req.uri().path() == format!("/v1/apps/services/{raw}/restore");
    if !keeps {
        let sid = id.clone();
        if let Ok((_, Some(_))) = apps.db(move |s| s.service_any(&sid)).await {
            return Error::NotFound(format!("no service {id}")).into_response();
        }
    }
    let cluster = apps.deps.cluster.clone();
    cluster.dispatch(&lease_key(&id), req, |req, _guard| next.run(req)).await
}

async fn status_page(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.get_status_page(&project).await?)
}

async fn put_status_page(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    ok(apps.put_status_page(&project, body(&b)?).await?)
}

async fn status_incidents(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.incidents(&project).await?)
}

async fn post_status_incident(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    Ok((StatusCode::CREATED, Json(apps.post_incident(&project, body(&b)?).await?)).into_response())
}

async fn update_status_incident(State(apps): S, Path((project, iid)): Path<(String, String)>, b: Bytes) -> R {
    apps.update_incident(&project, &iid, body(&b)?).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_status_page(State(apps): S, Path(project): Path<String>) -> R {
    apps.delete_status_page(&project).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn preview_domain(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.preview_domain(&project).await?)
}

async fn set_preview_domain(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    Ok((StatusCode::CREATED, Json(apps.set_preview_domain(&project, body(&b)?).await?)).into_response())
}

async fn verify_preview_domain(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.verify_preview_domain(&project).await?)
}

async fn delete_preview_domain(State(apps): S, Path(project): Path<String>) -> R {
    apps.delete_preview_domain(&project).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn overview(State(apps): S) -> R {
    let services = apps.services(None).await?;
    let mut o = apps.overview();
    o["services"] = serde_json::to_value(services).expect("JSON");
    ok(o)
}

/// A project in one file, read (no service made): a roll.toml's `[apps]` and `[databases]`, or
/// a Compose file turned into them, checked as `roll up --all` checks them. The dashboard's import
/// plans against the project with it and makes what's missing through the usual routes.
async fn import(State(apps): S, b: Bytes) -> R {
    use apps_build::project::{VarSpec, compose_to_toml, entry_kind, entry_run, parse_project};
    let v = body(&b)?;
    let text = |k: &str| v.get(k).and_then(Value::as_str);
    if [text("roll_toml"), text("compose"), text("render"), text("fly"), text("railway"), text("heroku"), text("procfile")].into_iter().flatten().any(|t| t.len() > 1 << 20) {
        return Err(Error::BadRequest("the file is at most 1 MiB".into()));
    }
    // A repo's own roll.toml (a "deploy to roll" link): its root's, at a revision or its head.
    let from_repo = match text("repo") {
        None => None,
        Some(repo) => {
            let sources = apps.deps.sources.clone().ok_or_else(|| Error::BadRequest("roll code isn't on this node".into()))?;
            let rev = match text("revision") {
                Some(r) => sources.revision(repo, r).await,
                None => sources.resolve(repo, None).await,
            }
            .map_err(|e| Error::BadRequest(format!("{repo}: {e}")))?;
            let bytes = sources.read_file(repo, &rev.sha, "roll.toml").await.map_err(|e| Error::BadRequest(format!("{repo}: {e}")))?;
            let bytes = bytes.ok_or_else(|| Error::BadRequest(format!("{repo} has no roll.toml at its root to deploy from (or one over 64 KiB)")))?;
            if bytes.len() > 1 << 20 {
                return Err(Error::BadRequest("the file is at most 1 MiB".into()));
            }
            Some(String::from_utf8(bytes).map_err(|_| Error::BadRequest(format!("{repo}'s roll.toml isn't UTF-8")))?)
        }
    };
    // A public Git repository's (a "deploy to roll" link for GitHub, GitLab…): its branch's head,
    // or the default one's; the file's folders become services built from the repository.
    let from_git = match text("git") {
        None => None,
        Some(url) => {
            if from_repo.is_some() {
                return Err(Error::BadRequest("give one of repo or git".into()));
            }
            crate::git_source::check_url(url, apps.cfg.allow_private).map_err(Error::BadRequest)?;
            let remote = crate::git_source::Remote::public(apps.cfg.allow_private, url);
            let (sha, branch) = crate::git_source::head(&remote, text("branch")).await.map_err(|e| Error::BadRequest(format!("{url}: {e:#}")))?;
            let bytes = crate::git_source::read_file(&remote, &sha, "roll.toml").await.map_err(|e| Error::BadRequest(format!("{url}: {e:#}")))?;
            let bytes = bytes.ok_or_else(|| Error::BadRequest(format!("{url} has no roll.toml at its root to deploy from")))?;
            Some((url.to_string(), branch, String::from_utf8(bytes).map_err(|_| Error::BadRequest(format!("{url}'s roll.toml isn't UTF-8")))?))
        }
    };
    let (git_url, git_branch) = from_git.as_ref().map(|(u, b, _)| (Some(u.clone()), Some(b.clone()))).unwrap_or_default();
    let repo = text("repo").map(str::to_string);
    let from_repo = from_repo.or(from_git.map(|(_, _, t)| t));
    // Another platform's file (Render's Blueprint, fly.toml, railway.json, Heroku's app.json and
    // Procfile), read as roll.toml with notes on what isn't carried over.
    let platform: Vec<(&str, String)> = ["render", "fly", "railway", "heroku"].into_iter().filter_map(|k| Some((k, text(k)?.to_string()))).collect();
    let procfile = text("procfile").map(str::to_string);
    let given = [from_repo.is_some(), text("roll_toml").is_some(), text("compose").is_some()].into_iter().filter(|b| *b).count() + platform.len();
    if given != 1 {
        return Err(Error::BadRequest("give one of roll_toml, compose, render, fly, railway, heroku (with procfile), repo or git".into()));
    }
    let (toml_text, notes) = match (from_repo, text("roll_toml"), text("compose"), platform.into_iter().next()) {
        (Some(t), _, _, _) => (t, Vec::new()),
        (_, Some(t), _, _) => (t.to_string(), Vec::new()),
        // Off the runtime's threads: a large file is a few hundred milliseconds of parsing.
        (_, _, Some(c), _) => {
            let c = c.to_string();
            tokio::task::spawn_blocking(move || compose_to_toml(&c)).await.map_err(|e| Error::Internal(e.to_string()))?.map_err(|e| Error::BadRequest(e.to_string()))?
        }
        (_, _, _, Some((kind, t))) => tokio::task::spawn_blocking(move || match kind {
            "render" => apps_build::imports::render_to_toml(&t),
            "fly" => apps_build::imports::fly_to_toml(&t),
            "railway" => apps_build::imports::railway_to_toml(&t),
            _ => apps_build::imports::heroku_to_toml(&t, procfile.as_deref()),
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
        .map_err(|e| Error::BadRequest(e.to_string()))?,
        _ => return Err(Error::BadRequest("give one of roll_toml, compose, render, fly, railway, heroku, repo or git".into())),
    };
    let f = parse_project(&toml_text).map_err(|e| Error::BadRequest(e.to_string()))?;
    let apps: serde_json::Map<String, Value> = f
        .apps
        .iter()
        .map(|(n, a)| {
            let source = match entry_kind(a) {
                "repo" => json!({ "kind": "repo", "repo": a.repo, "root": a.root }),
                "git" => json!({ "kind": "git", "url": a.git, "root": a.root }),
                "image" => json!({ "kind": "image", "image": a.image }),
                // A folder of the repository the file came from: built from it, at that folder
                // (an upload would need `roll up`, and a link has no checkout to run it in).
                _ if repo.is_some() => json!({ "kind": "repo", "repo": repo, "root": a.root.as_deref().filter(|r| *r != ".") }),
                _ if git_url.is_some() => json!({ "kind": "git", "url": git_url, "root": a.root.as_deref().filter(|r| *r != ".") }),
                _ => json!({ "kind": "upload", "root": a.root.as_deref().unwrap_or(".") }),
            };
            // A folder of a Git URL follows the branch the file was read from.
            let branch = a.branch.clone().or_else(|| (entry_kind(a) == "upload").then(|| git_branch.clone()).flatten());
            let links: Vec<Value> = a.links.iter().map(|l| json!({ "kind": apps_build::project::link_kind(&f, l), "name": l })).collect();
            // What to set as it is, what to make (a secret, once), and what to ask whoever makes it.
            let mut fixed = serde_json::Map::new();
            let (mut generate, mut ask) = (Vec::new(), Vec::new());
            for (k, spec) in &a.variables {
                match spec {
                    VarSpec::Value(val) => {
                        fixed.insert(k.clone(), json!(val));
                    }
                    VarSpec::Generate(_) => generate.push(json!(k)),
                    VarSpec::Ask(q) => ask.push(json!({ "name": k, "ask": q.ask, "default": q.default })),
                }
            }
            // Its other environments: a branch, its own run settings, variables as they are.
            let environments: serde_json::Map<String, Value> = a
                .environments
                .iter()
                .map(|(e, env)| (e.clone(), json!({ "branch": env.branch, "run": env.run.as_ref().map(|t| serde_json::to_value(t).unwrap_or_default()).unwrap_or_else(|| json!({})), "variables": env.variables })))
                .collect();
            (n.clone(), json!({ "environments": environments, "source": source, "branch": branch, "run": entry_run(a), "links": links, "variables": fixed, "generate": generate, "ask": ask }))
        })
        .collect();
    ok(json!({ "roll_toml": toml_text, "notes": notes, "apps": apps, "databases": f.databases }))
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
        // As a new web service would build (a Procfile's `web:`).
        let o = apps_build::Overrides { process: Some("web".into()), ..overrides.overrides() };
        let p = apps_build::plan(&apps_build::MapTree(map), &o).map_err(|e| Error::BadRequest(e.to_string()))?;
        return ok(p);
    }
    // A roll code repo at a revision, or a public Git URL at its branch's head (fetched as its
    // deploy would fetch it), before anything is made.
    enum From<'a> {
        Repo(Arc<dyn crate::integrations::Sources>, String, String),
        Git(crate::git_source::Remote<'a>, String),
    }
    let from = match (v.get("repo").and_then(Value::as_str), v.get("git").and_then(Value::as_str)) {
        (Some(repo), None) => {
            let sources = apps.deps.sources.clone().ok_or_else(|| Error::BadRequest("roll code isn't on this node".into()))?;
            let rev = match v.get("revision").and_then(Value::as_str) {
                Some(r) => sources.revision(repo, r).await,
                None => sources.resolve(repo, None).await,
            }
            .map_err(|e| Error::BadRequest(format!("{repo}: {e}")))?;
            From::Repo(sources, repo.to_string(), rev.sha)
        }
        (None, Some(url)) => {
            crate::git_source::check_url(url, apps.cfg.allow_private).map_err(Error::BadRequest)?;
            // A private repository's: the login the service would be made with.
            let login = match (v["git_login"]["username"].as_str(), v["git_login"]["password"].as_str()) {
                (Some(u), Some(p)) => Some((u.to_string(), p.to_string())),
                _ => None,
            };
            let remote = crate::git_source::Remote::new(apps.cfg.allow_private, url, login);
            let (sha, _) = crate::git_source::head(&remote, v.get("branch").and_then(Value::as_str)).await.map_err(|e| Error::BadRequest(format!("{url}: {e:#}")))?;
            From::Git(remote, sha)
        }
        _ => return Err(Error::BadRequest("one of repo, git or files is required".into())),
    };
    let dir = apps.cfg.home.join("apps").join("plans").join(crate::model::random(12));
    tokio::fs::create_dir_all(&dir).await.map_err(anyhow::Error::from)?;
    let result = async {
        match &from {
            From::Repo(sources, repo, sha) => sources.materialize(repo, sha, &dir).await?,
            From::Git(remote, sha) => {
                crate::git_source::materialize(remote, sha, &dir).await?;
            }
        }
        // `roots`: the tree's folders that build on their own, to choose one from (a monorepo).
        if v.get("roots") == Some(&Value::Bool(true)) {
            let d = dir.clone();
            let roots = tokio::task::spawn_blocking(move || apps_build::candidates(&apps_build::DirTree::new(d))).await.map_err(|e| Error::Internal(e.to_string()))?;
            return Ok(json!({ "roots": roots }));
        }
        let root = match v.get("root").and_then(Value::as_str) {
            Some(r) => crate::model::in_tree(&dir, "root", r, true)?,
            None => dir.clone(),
        };
        // What a build would use: roll.toml `[app]` over the settings sent, said in the notes.
        use crate::config::Apply;
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
        // As a new web service would build (a Procfile's `web:`), but from a workspace's root: its
        // Procfile is the repo's.
        let process = (context == root).then(|| "web".to_string());
        let o = apps_build::Overrides { process, ..build.overrides() };
        let p = tokio::task::spawn_blocking(move || apps_build::plan(&apps_build::DirTree::new(context), &o)).await.map_err(|e| Error::Internal(e.to_string()))?;
        p.map(|mut p| {
            p.notes.splice(0..0, said);
            serde_json::to_value(p).expect("JSON")
        })
        .map_err(|e| Error::BadRequest(e.to_string()))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    ok(result?)
}

#[derive(Deserialize)]
struct ProjectQuery {
    project: Option<String>,
    /// The deleted services instead, kept for a restore.
    deleted: Option<bool>,
}

async fn list_services(State(apps): S, Query(q): Query<ProjectQuery>) -> R {
    if q.deleted == Some(true) {
        return ok(apps.deleted_services(q.project).await?);
    }
    ok(apps.services(q.project).await?)
}

async fn project_services(State(apps): S, Path(project): Path<String>) -> R {
    ok(apps.services(Some(project)).await?)
}

async fn create_service(State(apps): S, ext: Extensions, b: Bytes) -> R {
    let b = body(&b)?;
    let values: Vec<&str> = b["variables"].as_object().into_iter().flatten().filter_map(|(_, v)| v.as_str()).collect();
    if let (Some(project), Some(name)) = (b["project"].as_str(), b["name"].as_str())
        && values.iter().any(|v| v.contains("${{"))
    {
        apps.check_references(project, name, crate::model::PRODUCTION, &values, asker(&ext)).await?;
    }
    let s = apps.create_service(b, actor(&ext)).await?;
    created_audit(&apps, &s, &ext).await;
    Ok((StatusCode::CREATED, Json(s)).into_response())
}

/// A service made with a private repository's login: the login's audit entry, as `PUT …/git-login`'s.
async fn created_audit(apps: &Arc<Apps>, s: &crate::model::Service, ext: &Extensions) {
    if let Some(l) = &s.git_login {
        apps.audit("git_login.set", &s.id, "", actor(ext), json!({ "host": l.host })).await;
    }
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
    created_audit(&apps, &s, &ext).await;
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

async fn delete_service(State(apps): S, Path(id): Path<String>, ext: Extensions) -> R {
    apps.delete_service(&id, actor(&ext)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn restore_service(State(apps): S, Path(id): Path<String>, ext: Extensions) -> R {
    ok(apps.restore_service(&id, actor(&ext)).await?)
}

async fn links(State(apps): S, Path(id): Path<String>) -> R {
    ok(apps.links(&id).await?)
}

#[derive(Deserialize)]
struct ActivityQuery {
    /// Every kind of event, not only the audit trail: the service's timeline.
    all: Option<String>,
}

async fn activity(State(apps): S, Path(id): Path<String>, Query(q): Query<ActivityQuery>) -> R {
    ok(apps.activity(&id, truthy(&q.all)).await?)
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

async fn update_drain(State(apps): S, Path((project, did)): Path<(String, String)>, b: Bytes) -> R {
    ok(apps.update_drain(&project, &did, body(&b)?).await?)
}

async fn test_drain(State(apps): S, Path((project, did)): Path<(String, String)>) -> R {
    ok(apps.test_drain(&project, &did).await?)
}

async fn export_project(State(apps): S, Path(project): Path<String>, b: Bytes) -> R {
    ok(apps.export_project(&project, body(&b)?).await?)
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

async fn update_target(State(apps): S, Path((project, nid)): Path<(String, String)>, b: Bytes) -> R {
    ok(apps.update_target(&project, &nid, body(&b)?).await?)
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

async fn put_git_login(State(apps): S, Path(id): Path<String>, ext: Extensions, b: Bytes) -> R {
    let s = apps.put_git_login(&id, body(&b)?).await?;
    apps.audit("git_login.set", &id, "", actor(&ext), json!({ "host": s.git_login.as_ref().map(|l| l.host.clone()) })).await;
    ok(s)
}

async fn delete_git_login(State(apps): S, Path(id): Path<String>, ext: Extensions) -> R {
    let s = apps.delete_git_login(&id).await?;
    apps.audit("git_login.removed", &id, "", actor(&ext), json!({})).await;
    ok(s)
}

async fn put_github(State(apps): S, Path(id): Path<String>, ext: Extensions) -> R {
    let s = apps.put_github(&id).await?;
    apps.audit("github.connected", &id, "", actor(&ext), json!({ "repo": s.github.as_ref().map(|g| g.repo.clone()), "installation": s.github.as_ref().map(|g| g.installation) })).await;
    ok(s)
}

async fn delete_github(State(apps): S, Path(id): Path<String>, ext: Extensions) -> R {
    let s = apps.delete_github(&id).await?;
    apps.audit("github.disconnected", &id, "", actor(&ext), json!({})).await;
    ok(s)
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

#[derive(serde::Deserialize)]
struct DeploysQuery {
    /// Made before then (unix ms; the last one's `created_at`): the next page.
    before: Option<i64>,
}

async fn deploys(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<DeploysQuery>) -> R {
    ok(apps.deployments(&id, &env, q.before).await?)
}

/// A person's deploy past a freeze: `X-Roll-Override: 1`, which roll-access makes `admin`
/// (a deploy token can't go past a freeze the team set). The body's `override` is refused, so
/// no caller says it where the gate doesn't read.
fn with_override(mut b: Value, headers: &HeaderMap) -> crate::Result<Value> {
    if b.get("override").is_some() {
        return Err(Error::BadRequest("a freeze's override is the header X-Roll-Override: 1 (admin), not a body field".into()));
    }
    if headers.get("x-roll-override").is_some_and(|v| v == "1")
        && let Some(o) = b.as_object_mut()
    {
        o.insert("override".into(), Value::Bool(true));
    }
    Ok(b)
}

/// JSON (`NewDeploy`), or a gzipped tar of the tree (`roll up`).
async fn create_deploy(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, headers: HeaderMap, b: Bytes) -> R {
    let actor = actor(&ext);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let key = match headers.get("idempotency-key") {
        Some(v) => Some(v.to_str().map_err(|_| Error::BadRequest("Idempotency-Key is printable ASCII".into()))?),
        None => None,
    };
    // A freeze's override is always the header (roll-access makes it admin); never the body.
    let over = headers.get("x-roll-override").is_some_and(|v| v == "1");
    let d = if ct.starts_with("application/gzip") || ct.starts_with("application/x-gzip") {
        // Percent-encoded UTF-8 (a commit subject isn't ASCII): `roll up` sends it so.
        let message = headers.get("x-roll-message").and_then(|v| v.to_str().ok()).map(|m| percent_encoding::percent_decode_str(m).decode_utf8_lossy().into_owned()).unwrap_or_default();
        let revision = headers.get("x-roll-revision").and_then(|v| v.to_str().ok()).filter(|r| (7..=64).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_hexdigit()));
        let no_cache = headers.get("x-roll-no-cache").is_some_and(|v| v == "1");
        apps.deploy(&id, &env, json!({ "message": message, "revision": revision, "no_cache": no_cache, "override": over }), By { trigger: Trigger::Upload, actor }, Some(b), key).await?
    } else if ct.starts_with("application/x-docker-image") {
        // An image built elsewhere (`docker save`'s archive, `roll apps deploy --image-file`).
        let message = headers.get("x-roll-message").and_then(|v| v.to_str().ok()).map(|m| percent_encoding::percent_decode_str(m).decode_utf8_lossy().into_owned()).unwrap_or_default();
        apps.deploy(&id, &env, json!({ "message": message, "image_upload": true, "override": over }), By { trigger: Trigger::Upload, actor }, Some(b), key).await?
    } else {
        apps.deploy(&id, &env, with_override(body(&b)?, &headers)?, By { trigger: Trigger::Manual, actor }, None, key).await?
    };
    apps.lift_hold(&id, &env).await;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn deploy_commits(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>) -> R {
    ok(apps.deploy_commits(&id, &env, &dep).await?)
}

#[derive(Deserialize)]
struct VulnQuery {
    severity: Option<String>,
}

async fn deploy_sbom(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>) -> R {
    let body = apps.sbom(&id, &env, &dep).await?;
    let file = format!("attachment; filename=\"{dep}.cdx.json\"");
    Ok(([(axum::http::header::CONTENT_TYPE, "application/vnd.cyclonedx+json".to_string()), (axum::http::header::CONTENT_DISPOSITION, file)], body).into_response())
}

async fn deploy_vulnerabilities(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>, Query(q): Query<VulnQuery>) -> R {
    ok(apps.vulnerabilities(&id, &env, &dep, q.severity.as_deref()).await?)
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
    /// Access lines of answers roll gave itself, and why (`firewall`, `rate_limit`, `wake`…; `any`:
    /// every one of them).
    by: Option<String>,
    /// Lines made before then (unix ms): reading back through the stored chunks.
    before: Option<i64>,
    /// One instance's lines, or one deployment's, or one stream's (`stdout`, `stderr`, `system`).
    instance: Option<String>,
    deployment: Option<String>,
    stream: Option<String>,
    /// Lines whose app says this level or a worse one (`warn`: warnings, errors, fatal).
    level: Option<String>,
    /// Lines whose fields say so (`level:error user_id:42`; `crate::log_where`).
    #[serde(rename = "where")]
    fields: Option<String>,
}

/// Which access lines `status` and `min_ms` keep (`None`: all of them).
fn http_filter(q: &LogQuery) -> crate::Result<Option<impl Fn(&crate::logs::LogLine) -> bool + Send + Sync + 'static>> {
    if q.status.is_none() && q.min_ms.is_none() && q.by.is_none() {
        return Ok(None);
    }
    let want_by = q.by.clone().filter(|b| !b.trim().is_empty());
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
        (range.0..=range.1).contains(&status)
            && v["ms"].as_u64().unwrap_or(0) >= min
            && want_by.as_deref().is_none_or(|b| v["by"].as_str().is_some_and(|got| b == "any" || got == b))
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
    let text = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_lowercase);
    if text.as_ref().is_some_and(|t| t.len() > 200) {
        return Err(Error::BadRequest("q is at most 200 characters".into()));
    }
    let stream = match q.stream.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(serde_json::from_value::<crate::logs::Stream>(json!(s)).map_err(|_| Error::BadRequest("stream is stdout, stderr, system, http or build".into()))?),
    };
    let level = match q.level.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(l) => Some(crate::logs::Level::parse(l).ok_or_else(|| Error::BadRequest("level is trace, debug, info, warn, error or fatal".into()))?),
    };
    let fields = match q.fields.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(w) if w.len() > 1000 => return Err(Error::BadRequest("where is at most 1000 characters".into())),
        Some(w) => Some(crate::log_where::Where::parse(w).map_err(|e| Error::BadRequest(format!("where: {e}")))?),
    };
    // Every filter in one: the route's own (`keep`), then the asker's.
    let (instance, deployment) = (q.instance.clone().filter(|s| !s.is_empty()), q.deployment.clone().filter(|s| !s.is_empty()));
    let narrowed = keep.is_some() || instance.is_some() || deployment.is_some() || stream.is_some() || level.is_some() || fields.is_some();
    let keep: Keep = narrowed.then(|| {
        let k = keep;
        Box::new(move |l: &LogLine| {
            k.as_ref().is_none_or(|k| k(l))
                && instance.as_deref().is_none_or(|i| l.instance.as_deref() == Some(i))
                && deployment.as_deref().is_none_or(|d| l.deployment.as_deref() == Some(d))
                && stream.is_none_or(|s| l.stream == s)
                && level.is_none_or(|min| l.level.is_some_and(|x| x >= min))
                && fields.as_ref().is_none_or(|w| w.matches(&l.text))
        }) as Box<dyn Fn(&LogLine) -> bool + Send + Sync>
    });
    // Read back (a search, a time window, or filtered): memory, then the stored chunks.
    if text.is_some() || q.before.is_some() || (keep.is_some() && !truthy(&q.follow)) {
        let (lines, cut) = apps.logs.read(&key, |l| keep.as_ref().is_none_or(|k| k(l)) && text.as_ref().is_none_or(|t| l.text.to_lowercase().contains(t)), q.since, q.before, tail).await;
        let mut r = Json(lines).into_response();
        if cut {
            r.headers_mut().insert("x-roll-search-truncated", axum::http::HeaderValue::from_static("1"));
        }
        return Ok(r);
    }
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
/// Who acted, for the activity log: a scoped token by its name, a signed-in admin by their user
/// id, else the operator.
/// Who asks, for another product's rule (roll secrets' reveal): what rolld's gate put on it.
fn asker(ext: &Extensions) -> crate::integrations::Asker<'_> {
    crate::integrations::Asker { operator: roll_auth::Operator::on(ext), user: ext.get::<roll_auth::SignedIn>(), grant: roll_auth::Grant::on(ext) }
}

fn actor(ext: &Extensions) -> Option<String> {
    if let Some(g) = roll_auth::Grant::on(ext) {
        return Some(format!("token:{}", g.name));
    }
    if let Some(u) = ext.get::<roll_auth::SignedIn>() {
        return Some(u.user.clone());
    }
    roll_auth::Operator::on(ext).then(|| "operator".to_string())
}

async fn redeploy(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, headers: HeaderMap, b: Bytes) -> R {
    let key = match headers.get("idempotency-key") {
        Some(v) => Some(v.to_str().map_err(|_| Error::BadRequest("Idempotency-Key is printable ASCII".into()))?),
        None => None,
    };
    let d = apps.redeploy(&id, &env, with_override(if b.is_empty() { json!({}) } else { body(&b)? }, &headers)?, actor(&ext), key).await?;
    apps.lift_hold(&id, &env).await;
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

async fn restore_volume(State(apps): S, Path((id, env, bid)): Path<(String, String, String)>, ext: Extensions, b: Bytes) -> R {
    // `from`: another environment's backup (seeding staging from production).
    let o = body(&b)?;
    if let Some(k) = o.as_object().into_iter().flatten().map(|(k, _)| k).find(|k| k.as_str() != "from") {
        return Err(Error::BadRequest(format!("unknown field {k}")));
    }
    let from = match o.get("from") {
        None | Some(Value::Null) => None,
        Some(Value::String(f)) => Some(f.clone()),
        Some(_) => return Err(Error::BadRequest("from is another environment's name".into())),
    };
    let b = apps.restore_volume(&id, &env, &bid, from.as_deref()).await?;
    apps.audit("volume.restored", &id, &env, actor(&ext), json!({ "backup": b.id, "from": from })).await;
    ok(b)
}

async fn put_hook(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions) -> R {
    let v = apps.put_hook(&id, &env).await?;
    apps.audit("hook.created", &id, &env, actor(&ext), json!({})).await;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

async fn delete_hook(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions) -> R {
    apps.delete_hook(&id, &env).await?;
    apps.audit("hook.removed", &id, &env, actor(&ext), json!({})).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(serde::Deserialize)]
struct HookQuery {
    image: Option<String>,
}

/// Deploy hooks: the URL is the secret, so rolld's gate lets them through to this check (its
/// `RULES`), on the service's owner. Access lines log the route pattern, never the token.
fn hooks_router(apps: Arc<Apps>) -> Router {
    Router::new()
        .route("/v1/apps/hooks/{id}/{env}/{token}", post(run_hook))

        .route_layer(axum::middleware::from_fn_with_state(apps.clone(), hook_on_owner))
        .with_state(apps)
}

/// The caller is anonymous: only an id shaped like a service's, of a service that exists, is
/// routed to its owner (a made-up one would otherwise have this node take a lease for it).
async fn hook_on_owner(State(apps): S, req: Request, next: Next) -> Response {
    let raw = req.uri().path().strip_prefix("/v1/apps/hooks/").and_then(|r| r.split('/').next()).unwrap_or("");
    let id = percent_encoding::percent_decode_str(raw).decode_utf8_lossy().into_owned();
    let shaped = id.strip_prefix("svc_").is_some_and(|r| r.len() == 12 && r.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)));
    let sid = id.clone();
    if !shaped || apps.db(move |s| s.service(&sid)).await.is_err() {
        return Error::NotFound("no such hook".into()).into_response();
    }
    let cluster = apps.deps.cluster.clone();
    cluster.dispatch(&lease_key(&id), req, |req, _guard| next.run(req)).await
}

async fn run_hook(State(apps): S, Path((id, env, token)): Path<(String, String, String)>, Query(q): Query<HookQuery>, ext: Extensions, headers: HeaderMap, b: Bytes) -> R {
    // A repository's webhook: JSON, or GitHub's default form encoding (`payload=<JSON>`), and
    // the event its platform names. Anything else is a plain deploy.
    let event = ["x-github-event", "x-gitlab-event", "x-event-key"].iter().find_map(|h| headers.get(*h)).and_then(|v| v.to_str().ok()).map(str::to_string);
    let form = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"));
    let json = if form {
        url::form_urlencoded::parse(&b).find(|(k, _)| k == "payload").and_then(|(_, v)| serde_json::from_str::<Value>(&v).ok())
    } else {
        serde_json::from_slice::<Value>(&b).ok()
    };
    let payload = match (json.filter(Value::is_object), event) {
        (Some(body), event) => Some(crate::hooks::Webhook { body, event }),
        (None, Some(event)) => Some(crate::hooks::Webhook { body: json!({ "event": event }), event: Some(event) }),
        (None, None) => None,
    };
    let d = match apps.run_hook(&id, &env, &token, q.image, payload).await? {
        crate::hooks::HookOutcome::Deployed(d) => *d,
        crate::hooks::HookOutcome::Ignored(why) => return ok(json!({ "ignored": why })),
        crate::hooks::HookOutcome::Plain => unreachable!("run_hook deploys a plain call"),
    };
    // The gate never saw it: who called is in the service's activity.
    let address = ext.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string());
    apps.audit("hook.deployed", &id, &env, Some("deploy hook".into()), json!({ "deployment": d.id, "address": address })).await;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn promote(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, headers: HeaderMap, b: Bytes) -> R {
    let d = apps.promote(&id, &env, with_override(body(&b)?, &headers)?, actor(&ext)).await?;
    apps.lift_hold(&id, &env).await;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn rollback(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>, ext: Extensions, headers: HeaderMap, b: Bytes) -> R {
    let o = with_override(body(&b)?, &headers)?;
    let (keep, over) = (o.get("keep_variables").and_then(Value::as_bool).unwrap_or(false), o.get("override").and_then(Value::as_bool).unwrap_or(false));
    let d = apps.rollback(&id, &env, &dep, actor(&ext), keep, over).await?;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

#[derive(Deserialize)]
struct RolloutBody {
    action: crate::model::RolloutAction,
}

async fn rollout_action(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>, ext: Extensions, Json(b): Json<RolloutBody>) -> R {
    apps.deployment(&id, &env, &dep).await?;
    apps.rollout_action(&dep, b.action)?;
    apps.audit("rollout.decided", &id, &env, actor(&ext), json!({ "deployment": dep, "action": format!("{:?}", b.action).to_ascii_lowercase() })).await;
    Ok((StatusCode::ACCEPTED, Json(json!({ "deployment": dep }))).into_response())
}

async fn rollback_previous(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let keep = body(&b)?.get("keep_variables").and_then(Value::as_bool).unwrap_or(false);
    let d = apps.rollback_previous(&id, &env, actor(&ext), keep).await?;
    Ok((StatusCode::ACCEPTED, Json(d)).into_response())
}

async fn cancel(State(apps): S, Path((id, env, dep)): Path<(String, String, String)>) -> R {
    ok(apps.cancel(&id, &env, &dep).await?)
}

async fn purge_cache(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let v = apps.purge_cache(&id, &env, if b.is_empty() { json!({}) } else { body(&b)? }).await?;
    apps.audit("cache.purged", &id, &env, actor(&ext), v.clone()).await;
    ok(v)
}

async fn restart(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    let e = apps.restart(&id, &env).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

async fn restart_instance(State(apps): S, Path((id, env, name)): Path<(String, String, String)>, ext: Extensions) -> R {
    let e = apps.restart_instance(&id, &env, &name).await?;
    apps.audit("instance.restarted", &id, &env, actor(&ext), json!({ "instance": name })).await;
    ok(e)
}

async fn sleep(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.sleep(&id, &env).await?)
}

async fn wake(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.wake(&id, &env).await?)
}

/// `GET …/pending`: what the next deploy changes (names, never values).
async fn pending(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.pending(&id, &env).await?)
}

async fn variables(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.variables(&id, &env, None).await?)
}

async fn reveal(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions) -> R {
    let who = asker(&ext);
    let v = apps.variables(&id, &env, Some(who)).await?;
    apps.audit("variables.revealed", &id, &env, actor(&ext), json!({})).await;
    ok(v)
}

#[derive(Deserialize)]
struct DiffQuery {
    from: String,
}

async fn env_diff(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<DiffQuery>, ext: Extensions) -> R {
    ok(apps.environment_diff(&id, &env, &q.from, roll_auth::Grant::on(&ext)).await?)
}

async fn change_variables(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    let mut change = body(&b)?;
    // Another environment's values copied (`copy_from`, `copy`: names): set here as given there.
    if let Some(from) = change.get("copy_from").cloned() {
        let Value::String(from) = from else { return Err(Error::BadRequest("copy_from is another environment's name".into())) };
        if from == env {
            return Err(Error::BadRequest("copy_from is another environment".into()));
        }
        let names: Vec<String> = change["copy"].as_array().map(|a| a.iter().filter_map(|n| n.as_str().map(str::to_string)).collect()).filter(|n: &Vec<String>| !n.is_empty()).ok_or_else(|| Error::BadRequest("copy is a list of the names to copy".into()))?;
        let copied = apps.copied_variables(&id, &from, &names).await?;
        let o = change.as_object_mut().expect("an object");
        o.remove("copy_from");
        o.remove("copy");
        let set = o.entry("set").or_insert_with(|| json!({}));
        let Some(set) = set.as_object_mut() else { return Err(Error::BadRequest("set is an object of names to values".into())) };
        for (k, v) in copied {
            if set.contains_key(&k) {
                return Err(Error::BadRequest(format!("{k} is both set and copied")));
            }
            set.insert(k, Value::String(v));
        }
    } else if change.get("copy").is_some() {
        return Err(Error::BadRequest("copy needs copy_from, the environment to copy from".into()));
    }
    let values: Vec<&str> = change["set"].as_object().into_iter().flatten().filter_map(|(_, v)| v.as_str()).collect();
    if values.iter().any(|v| v.contains("${{")) {
        let svc = apps.service(&id).await?;
        apps.check_references(&svc.project, &svc.name, &env, &values, asker(&ext)).await?;
    }
    let v = apps.change_variables(&id, &env, change.clone()).await?;
    apps.settings_changed();
    // Names only: a value never reaches the trail.
    let set: Vec<String> = change["set"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    apps.audit("variables", &id, &env, actor(&ext), json!({ "set": set, "unset": change["unset"], "seal": change["seal"] })).await;
    ok(v)
}

#[derive(Deserialize)]
struct Window {
    minutes: Option<i64>,
    days: Option<i64>,
    /// `prometheus`: the metrics as a scraper reads them.
    format: Option<String>,
}

async fn slo(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.slo_report(&id, &env).await?)
}

async fn metrics(State(apps): S, Path((id, env)): Path<(String, String)>, Query(w): Query<Window>) -> R {
    // For a scraper (Prometheus, Grafana Agent): the last finished minute as gauges.
    if w.format.as_deref() == Some("prometheus") {
        let text = apps.metrics_prometheus(&id, &env).await?;
        return Ok(([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], text).into_response());
    }
    ok(apps.metrics(&id, &env, w.minutes.unwrap_or(60)).await?)
}

/// `GET …/routes?minutes=`: errors and latency by route, from the access log (`crate::traffic`).
#[derive(serde::Deserialize)]
struct TraceQuery {
    minutes: Option<i64>,
    min_ms: Option<u64>,
}

async fn traces(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<TraceQuery>) -> R {
    ok(apps.traces(&id, &env, q.minutes.unwrap_or(60), q.min_ms.unwrap_or(0)).await?)
}

#[derive(Deserialize)]
struct VitalsQuery {
    minutes: Option<i64>,
}

async fn error_state(State(apps): S, Path((id, env, fp)): Path<(String, String, String)>, ext: Extensions, b: Bytes) -> R {
    let body = body(&b)?;
    let out = apps.set_error_state(&id, &env, &fp, body.clone()).await?;
    apps.audit("error.state", &id, &env, actor(&ext), json!({ "fingerprint": fp, "state": out["state"] })).await;
    ok(out)
}

async fn errors(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<VitalsQuery>) -> R {
    ok(apps.errors(&id, &env, q.minutes.unwrap_or(1440)).await?)
}

async fn forms(State(apps): S, Path((id, env)): Path<(String, String)>) -> R {
    ok(apps.forms(&id, &env).await?)
}

#[derive(Deserialize)]
struct SubmissionsQuery {
    limit: Option<usize>,
    before: Option<String>,
}

/// A form's submissions in the clear: what people sent (a reveal, as variables' values are).
async fn form_submissions(State(apps): S, Path((id, env, form)): Path<(String, String, String)>, Query(q): Query<SubmissionsQuery>, ext: Extensions) -> R {
    let v = apps.form_submissions(&id, &env, &form, q.limit.unwrap_or(50), q.before).await?;
    apps.audit("forms.read", &id, &env, actor(&ext), json!({ "form": form })).await;
    ok(v)
}

async fn delete_form_submission(State(apps): S, Path((id, env, form, sub)): Path<(String, String, String, String)>, ext: Extensions) -> R {
    apps.delete_form_submission(&id, &env, &form, &sub).await?;
    apps.audit("forms.deleted", &id, &env, actor(&ext), json!({ "form": form, "submission": sub })).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn app_metrics(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<VitalsQuery>) -> R {
    ok(apps.app_metrics(&id, &env, q.minutes.unwrap_or(60)).await?)
}

async fn vitals(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<VitalsQuery>) -> R {
    ok(apps.vitals(&id, &env, q.minutes.unwrap_or(1440)).await?)
}

#[derive(Deserialize)]
struct VisitBody {
    to: String,
}

/// A signed-in person's one-use way into a sign-in-gated environment (roll's dashboard asks
/// for it as them, then sends the browser there). A token isn't a person: only a session gets one.
async fn visit(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, Json(b): Json<VisitBody>) -> R {
    let Some(user) = ext.get::<roll_auth::SignedIn>() else {
        return Err(Error::Forbidden("signing in to an environment is for a person signed in to roll, not a token".into()));
    };
    ok(json!({ "url": apps.visit_ticket(&id, &env, &user.user, &user.session, &b.to).await? }))
}

async fn trace(State(apps): S, Path((id, env, trace)): Path<(String, String, String)>) -> R {
    ok(apps.trace(&id, &env, &trace).await?)
}

async fn analytics(State(apps): S, Path((id, env)): Path<(String, String)>, Query(w): Query<Window>) -> R {
    ok(apps.analytics(&id, &env, w.minutes.unwrap_or(1_440)).await?)
}

async fn routes(State(apps): S, Path((id, env)): Path<(String, String)>, Query(w): Query<Window>) -> R {
    ok(apps.routes(&id, &env, w.minutes.unwrap_or(60)).await?)
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
    /// A one-off console (a fresh container) instead of a running instance, with what to run in
    /// it and its own size.
    console: Option<bool>,
    command: Option<String>,
    cpu: Option<f64>,
    memory_mb: Option<u32>,
}

/// `GET …/shell` (a WebSocket): a shell in a running instance; see `crate::shell`.
async fn shell(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<ShellQuery>, ext: Extensions, ws: axum::extract::ws::WebSocketUpgrade) -> R {
    // A shell reads every variable: roll secrets' reveal rule, as for their values.
    apps.check_secret_reveal(&id, &env, asker(&ext)).await?;
    let slot = apps.shell_slot(&id, &env)?;
    let target = if q.console == Some(true) {
        let t = apps.console_target(&id, &env, crate::shell::ConsoleAsk { command: q.command.clone(), cpu: q.cpu, memory_mb: q.memory_mb }).await?;
        // What it runs stays out of the trail (a command line can hold a credential).
        apps.audit("console.opened", &id, &env, actor(&ext), json!({ "command": q.command.is_some(), "cpu": q.cpu, "memory_mb": q.memory_mb })).await;
        t
    } else {
        if q.command.is_some() || q.cpu.is_some() || q.memory_mb.is_some() {
            return Err(Error::BadRequest("command, cpu and memory_mb are a console's (console=true)".into()));
        }
        apps.shell_target(&id, &env, q.instance.as_deref()).await?
    };
    let (cols, rows) = (q.cols.unwrap_or(80).clamp(10, 1000), q.rows.unwrap_or(24).clamp(2, 500));
    // Keys and resizes are small: nothing near axum's 64 MiB default is held for a peer.
    let ws = ws.max_message_size(1 << 20).max_frame_size(1 << 20);
    Ok(ws.on_upgrade(move |socket| apps.bridge_shell(socket, target, slot, cols, rows)).into_response())
}

#[derive(Deserialize)]
struct FilesQuery {
    path: String,
    instance: Option<String>,
}

/// `GET …/files?path=`: what's at a path in an instance, as a tar. Files can hold anything the
/// app has (its secrets too): roll secrets' reveal rule, as a shell's.
async fn read_files(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<FilesQuery>, ext: Extensions) -> R {
    apps.check_secret_reveal(&id, &env, asker(&ext)).await?;
    let (name, tar) = apps.read_files(&id, &env, q.instance.as_deref().filter(|s| !s.is_empty()), &q.path).await?;
    apps.audit("files.read", &id, &env, actor(&ext), json!({ "instance": name, "path": q.path })).await;
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(tar));
    Ok(([(header::CONTENT_TYPE, "application/x-tar"), (header::HeaderName::from_static("x-roll-instance"), name.as_str())], body).into_response())
}

/// `PUT …/files?path=` (a tar): its files written under the path in an instance.
async fn write_files(State(apps): S, Path((id, env)): Path<(String, String)>, Query(q): Query<FilesQuery>, ext: Extensions, b: Bytes) -> R {
    apps.check_secret_reveal(&id, &env, asker(&ext)).await?;
    let name = apps.write_files(&id, &env, q.instance.as_deref().filter(|s| !s.is_empty()), &q.path, b).await?;
    apps.audit("files.written", &id, &env, actor(&ext), json!({ "instance": name, "path": q.path })).await;
    ok(json!({ "instance": name, "path": q.path }))
}

/// `GET …/tunnel` (a WebSocket): one TCP connection to the service's port; see `crate::tunnel`.
async fn tunnel(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, ws: axum::extract::ws::WebSocketUpgrade) -> R {
    let slot = apps.tunnel_slot(&id, &env)?;
    let lease = apps.tunnel_target(&id, &env).await?;
    apps.audit("tunnel.opened", &id, &env, actor(&ext), json!({})).await;
    let ws = ws.max_message_size(1 << 20).max_frame_size(1 << 20);
    Ok(ws.on_upgrade(move |socket| apps.bridge_tunnel(socket, lease, slot)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenSession {
    instance: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    #[serde(default)]
    console: bool,
    command: Option<String>,
    cpu: Option<f64>,
    memory_mb: Option<u32>,
}

/// `POST …/shell/sessions`: a shell over HTTP (the dashboard's terminal); see `crate::shell`.
async fn open_session(State(apps): S, Path((id, env)): Path<(String, String)>, ext: Extensions, b: Bytes) -> R {
    apps.check_secret_reveal(&id, &env, asker(&ext)).await?;
    let o: OpenSession = if b.is_empty() { OpenSession { instance: None, cols: None, rows: None, console: false, command: None, cpu: None, memory_mb: None } } else { serde_json::from_slice(&b).map_err(|e| Error::BadRequest(e.to_string()))? };
    let console = o.console.then(|| crate::shell::ConsoleAsk { command: o.command.clone(), cpu: o.cpu, memory_mb: o.memory_mb });
    if console.is_none() && (o.command.is_some() || o.cpu.is_some() || o.memory_mb.is_some()) {
        return Err(Error::BadRequest("command, cpu and memory_mb are a console's (console: true)".into()));
    }
    if console.is_some() {
        apps.audit("console.opened", &id, &env, actor(&ext), json!({ "command": o.command.is_some(), "cpu": o.cpu, "memory_mb": o.memory_mb })).await;
    }
    let sh = apps.open_session(&id, &env, o.instance.as_deref(), console, o.cols.unwrap_or(80).clamp(10, 1000), o.rows.unwrap_or(24).clamp(2, 500)).await?;
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
async fn job_logs(State(apps): S, Path((id, env, job)): Path<(String, String, String)>, Query(q): Query<LogQuery>, ext: Extensions) -> R {
    let j = apps.job(&id, &env, &job).await?;
    let who = asker(&ext);
    apps.check_secret_reveal(&id, &env, who).await?;
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

async fn get_run(State(apps): S, Path((id, env, run)): Path<(String, String, String)>) -> R {
    ok(apps.cron_run(&id, &env, &run).await?)
}

async fn cancel_run(State(apps): S, Path((id, env, run)): Path<(String, String, String)>) -> R {
    ok(apps.cancel_run(&id, &env, &run).await?)
}

/// A cron run's output: its container's lines in the environment's log (read as that log is:
/// they're there for anyone who reads it); `follow` streams them until the run ends.
async fn run_logs(State(apps): S, Path((id, env, run)): Path<(String, String, String)>, Query(q): Query<LogQuery>) -> R {
    let r = apps.cron_run(&id, &env, &run).await?;
    let instance = crate::live::run_name(&id, &env, &r.deployment, &r.id);
    let (tx, rx) = tokio::sync::watch::channel(r.status != crate::model::RunStatus::Running);
    if r.status == crate::model::RunStatus::Running {
        let (apps2, id2, env2, run2) = (apps.clone(), id.clone(), env.clone(), run.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                match apps2.cron_run(&id2, &env2, &run2).await {
                    Ok(r) if r.status == crate::model::RunStatus::Running => {}
                    _ => break,
                }
                if tx.is_closed() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx.send(true);
        });
    }
    stream_log(&apps, format!("env:{id}/{env}"), q, Some(rx), Some(Box::new(move |l: &LogLine| l.instance.as_deref() == Some(instance.as_str())))).await
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
    let patch = body(&b)?;
    let d = apps.patch_domain(&id, &env, &host, patch.clone()).await?;
    if patch.get("redirect_to").is_some() {
        apps.audit("domain.redirect", &id, &env, actor(&ext), json!({ "hostname": d.hostname, "redirect_to": d.redirect_to })).await;
    }
    if patch.get("port").is_some() {
        apps.audit("domain.port", &id, &env, actor(&ext), json!({ "hostname": d.hostname, "port": d.port })).await;
    }
    ok(d)
}

async fn delete_domain(State(apps): S, Path((id, env, host)): Path<(String, String, String)>, ext: Extensions) -> R {
    apps.delete_domain(&id, &env, &host).await?;
    apps.audit("domain.removed", &id, &env, actor(&ext), json!({ "hostname": host })).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

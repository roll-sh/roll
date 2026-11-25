//! GitLab's and Bitbucket's commit statuses for a Git URL service (GitHub's go through roll's
//! GitHub App): read for `wait_for_ci` (its pipelines' jobs, roll's own left out) and written
//! for roll's deploys (`roll/apps/<service>/<env>`), with the service's Git login's token,
//! through the client for tenant-chosen URLs (public addresses only, no redirects, bounded
//! reads). GitLab is gitlab.com or a host named `gitlab.…`; Bitbucket is bitbucket.org.

use crate::integrations::{Ci, CommitStatus};
use serde_json::{Value, json};
use std::time::Duration;

/// Where a repository's statuses are.
#[derive(Debug, PartialEq)]
pub(crate) enum Host {
    /// Its API (`https://gitlab.com/api/v4`) and the project's path (`group/sub/name`).
    GitLab { api: String, project: String },
    /// Its API (`https://api.bitbucket.org/2.0`), workspace and repository.
    Bitbucket { api: String, workspace: String, repo: String },
}

/// The most of an answer read.
const MAX_ANSWER: usize = 1 << 20;

/// The host a Git URL's statuses are on, when it's one roll reads.
pub(crate) fn host_of(url: &str) -> Option<Host> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let path = u.path().trim_matches('/').trim_end_matches(".git").to_string();
    if path.is_empty() || path.contains("..") {
        return None;
    }
    if host == "bitbucket.org" {
        let (workspace, repo) = path.split_once('/')?;
        return (!repo.contains('/')).then(|| Host::Bitbucket { api: "https://api.bitbucket.org/2.0".into(), workspace: workspace.into(), repo: repo.into() });
    }
    if host == "gitlab.com" || host.starts_with("gitlab.") {
        let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
        return path.contains('/').then(|| Host::GitLab { api: format!("{}://{host}{port}/api/v4", u.scheme()), project: path });
    }
    None
}

fn client(allow_private: bool) -> reqwest::Client {
    crate::net::client(allow_private, Duration::from_secs(15))
}

/// A check roll made itself, which it never waits on.
fn ours(name: &str) -> bool {
    name.starts_with("roll/") || name.starts_with("roll-")
}

/// What the latest status of each check says, together (the newest first in `states`).
fn verdict(states: impl Iterator<Item = (String, &'static str)>) -> Ci {
    let mut seen = std::collections::HashSet::new();
    let (mut any, mut running, mut failed) = (false, false, None);
    for (name, state) in states {
        if ours(&name) || !seen.insert(name.clone()) {
            continue;
        }
        any = true;
        match state {
            "ok" => {}
            "running" => running = true,
            _ => {
                failed.get_or_insert(name);
            }
        }
    }
    match (failed, running, any) {
        (Some(f), _, _) => Ci::Failed(f),
        (None, true, _) => Ci::Pending,
        (None, false, true) => Ci::Passed,
        _ => Ci::None,
    }
}

async fn json_of(res: reqwest::Response) -> anyhow::Result<Value> {
    anyhow::ensure!(res.status().is_success(), "answered {}", res.status());
    let body = crate::net::read_bounded(res, MAX_ANSWER).await;
    Ok(serde_json::from_slice(&body)?)
}

/// A commit's checks on its host.
pub(crate) async fn read(allow_private: bool, host: &Host, login: (&str, &str), sha: &str) -> anyhow::Result<Ci> {
    let c = client(allow_private);
    let sha = percent_encoding::utf8_percent_encode(sha, percent_encoding::NON_ALPHANUMERIC).to_string();
    match host {
        Host::GitLab { api, project } => {
            let p = percent_encoding::utf8_percent_encode(project, percent_encoding::NON_ALPHANUMERIC);
            let v = json_of(c.get(format!("{api}/projects/{p}/repository/commits/{sha}/statuses?per_page=100&all=false")).header("PRIVATE-TOKEN", login.1).send().await?).await?;
            // Newest first, as GitLab lists them by id; each job's latest decides.
            let mut list: Vec<&Value> = v.as_array().into_iter().flatten().collect();
            list.sort_by_key(|s| std::cmp::Reverse(s["id"].as_i64().unwrap_or(0)));
            Ok(verdict(list.into_iter().map(|s| {
                let state = match s["status"].as_str() {
                    Some("success" | "skipped") => "ok",
                    Some("pending" | "running" | "created" | "waiting_for_resource" | "preparing" | "scheduled") => "running",
                    Some("manual") => "ok",
                    _ => "failed",
                };
                (s["name"].as_str().unwrap_or("a job").to_string(), state)
            })))
        }
        Host::Bitbucket { api, workspace, repo } => {
            let v = json_of(c.get(format!("{api}/repositories/{workspace}/{repo}/commit/{sha}/statuses?pagelen=100&sort=-updated_on")).basic_auth(login.0, Some(login.1)).send().await?).await?;
            Ok(verdict(v["values"].as_array().into_iter().flatten().map(|s| {
                let state = match s["state"].as_str() {
                    Some("SUCCESSFUL") => "ok",
                    Some("INPROGRESS") => "running",
                    _ => "failed",
                };
                (s["key"].as_str().or(s["name"].as_str()).unwrap_or("a build").to_string(), state)
            })))
        }
    }
}

/// roll's deploy status on the commit.
pub(crate) async fn post(allow_private: bool, host: &Host, login: (&str, &str), sha: &str, s: &CommitStatus) -> anyhow::Result<()> {
    let c = client(allow_private);
    let enc = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
    let res = match host {
        Host::GitLab { api, project } => {
            let state = match s.state {
                "pending" => "running",
                "success" => "success",
                _ => "failed",
            };
            let mut body = json!({ "state": state, "name": s.context, "description": s.description });
            if let Some(u) = &s.target_url {
                body["target_url"] = json!(u);
            }
            c.post(format!("{api}/projects/{}/statuses/{}", enc(project), enc(sha))).header("PRIVATE-TOKEN", login.1).json(&body).send().await?
        }
        Host::Bitbucket { api, workspace, repo } => {
            let state = match s.state {
                "pending" => "INPROGRESS",
                "success" => "SUCCESSFUL",
                _ => "FAILED",
            };
            // A key is at most 40 characters.
            let key: String = s.context.chars().take(40).collect();
            let body = json!({ "key": key, "state": state, "name": s.context, "description": s.description, "url": s.target_url.clone().unwrap_or_default() });
            c.post(format!("{api}/repositories/{workspace}/{repo}/commit/{}/statuses/build", enc(sha))).basic_auth(login.0, Some(login.1)).json(&body).send().await?
        }
    };
    anyhow::ensure!(res.status().is_success(), "answered {}", res.status());
    Ok(())
}

/// roll previews' note on merge request (GitLab) or pull request (Bitbucket) `number`: `existing`
/// edited in place, or a new one when there's none or it's gone. Its id.
pub(crate) async fn note(allow_private: bool, host: &Host, login: (&str, &str), number: u64, body: &str, existing: Option<u64>) -> anyhow::Result<u64> {
    let c = client(allow_private);
    let enc = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
    let (base, payload) = match host {
        Host::GitLab { api, project } => (format!("{api}/projects/{}/merge_requests/{number}/notes", enc(project)), json!({ "body": body })),
        Host::Bitbucket { api, workspace, repo } => (format!("{api}/repositories/{workspace}/{repo}/pullrequests/{number}/comments"), json!({ "content": { "raw": body } })),
    };
    let auth = |r: reqwest::RequestBuilder| match host {
        Host::GitLab { .. } => r.header("PRIVATE-TOKEN", login.1),
        Host::Bitbucket { .. } => r.basic_auth(login.0, Some(login.1)),
    };
    if let Some(id) = existing {
        let res = auth(c.put(format!("{base}/{id}"))).json(&payload).send().await?;
        if res.status().is_success() {
            return Ok(id);
        }
        // Deleted on the host: a new one.
        anyhow::ensure!(res.status() == reqwest::StatusCode::NOT_FOUND, "answered {}", res.status());
    }
    let made = json_of(auth(c.post(base)).json(&payload).send().await?).await?;
    made["id"].as_u64().ok_or_else(|| anyhow::anyhow!("answered no note id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitlab_and_bitbucket_urls_and_verdicts() {
        assert_eq!(host_of("https://gitlab.com/acme/tools/site.git"), Some(Host::GitLab { api: "https://gitlab.com/api/v4".into(), project: "acme/tools/site".into() }));
        assert_eq!(host_of("https://gitlab.acme.dev:8443/x/y"), Some(Host::GitLab { api: "https://gitlab.acme.dev:8443/api/v4".into(), project: "x/y".into() }));
        assert_eq!(host_of("https://bitbucket.org/ws/repo"), Some(Host::Bitbucket { api: "https://api.bitbucket.org/2.0".into(), workspace: "ws".into(), repo: "repo".into() }));
        assert_eq!(host_of("https://github.com/a/b"), None);
        assert_eq!(host_of("https://bitbucket.org/ws/a/b"), None);
        let v = |l: &[(&str, &'static str)]| verdict(l.iter().map(|(n, s)| (n.to_string(), *s)));
        assert_eq!(v(&[]), Ci::None);
        assert_eq!(v(&[("roll/apps/web/production", "failed")]), Ci::None, "roll's own aren't waited on");
        assert_eq!(v(&[("test", "running"), ("lint", "ok")]), Ci::Pending);
        // The newest of a job decides: a retried job that passed isn't a failure.
        assert_eq!(v(&[("test", "ok"), ("test", "failed")]), Ci::Passed);
        assert_eq!(v(&[("test", "failed"), ("lint", "running")]), Ci::Failed("test".into()));
    }

    /// roll previews' one note on a merge request (GitLab) or pull request (Bitbucket), with each
    /// host's own auth: made, edited in place, and made again when it was deleted there.
    #[tokio::test]
    async fn a_previews_note_is_made_then_edited_in_place() {
        use axum::routing::{post, put};
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let (s1, s2, s3, s4) = (seen.clone(), seen.clone(), seen.clone(), seen.clone());
        let app = axum::Router::new()
            .route("/api/v4/projects/{project}/merge_requests/{n}/notes", post(move |h: axum::http::HeaderMap, axum::extract::Path((project, n)): axum::extract::Path<(String, u64)>, axum::Json(b): axum::Json<Value>| async move {
                assert_eq!((h["private-token"].to_str().unwrap(), project.as_str(), n), ("glpat-x", "acme/site", 4));
                s1.lock().push(format!("gl new {}", b["body"].as_str().unwrap()));
                axum::Json(json!({ "id": 70 }))
            }))
            .route("/api/v4/projects/{project}/merge_requests/{n}/notes/{id}", put(move |axum::extract::Path((_, _, id)): axum::extract::Path<(String, u64, u64)>, axum::Json(b): axum::Json<Value>| async move {
                s2.lock().push(format!("gl edit {id} {}", b["body"].as_str().unwrap()));
                if id == 70 { axum::http::StatusCode::OK } else { axum::http::StatusCode::NOT_FOUND }
            }))
            .route("/bb/repositories/{ws}/{repo}/pullrequests/{n}/comments", post(move |h: axum::http::HeaderMap, axum::Json(b): axum::Json<Value>| async move {
                assert!(h["authorization"].to_str().unwrap().starts_with("Basic "));
                s3.lock().push(format!("bb new {}", b["content"]["raw"].as_str().unwrap()));
                axum::Json(json!({ "id": 900 }))
            }))
            .route("/bb/repositories/{ws}/{repo}/pullrequests/{n}/comments/{id}", put(move |axum::extract::Path((_, _, _, id)): axum::extract::Path<(String, String, u64, u64)>| async move {
                s4.lock().push(format!("bb edit {id}"));
                axum::http::StatusCode::OK
            }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let gl = Host::GitLab { api: format!("{base}/api/v4"), project: "acme/site".into() };
        let bb = Host::Bitbucket { api: format!("{base}/bb"), workspace: "ws".into(), repo: "site".into() };
        let login = ("deploy", "glpat-x");
        assert_eq!(note(true, &gl, login, 4, "one", None).await.unwrap(), 70);
        assert_eq!(note(true, &gl, login, 4, "two", Some(70)).await.unwrap(), 70, "edited in place");
        assert_eq!(note(true, &gl, login, 4, "three", Some(69)).await.unwrap(), 70, "deleted there: made again");
        assert_eq!(note(true, &bb, login, 4, "one", None).await.unwrap(), 900);
        assert_eq!(note(true, &bb, login, 4, "two", Some(900)).await.unwrap(), 900);
        assert_eq!(*seen.lock(), ["gl new one", "gl edit 70 two", "gl edit 69 three", "gl new three", "bb new one", "bb edit 900"]);
    }

    /// Against fake GitLab and Bitbucket APIs: each one's statuses read with its token as each
    /// takes it (GitLab's `PRIVATE-TOKEN`, Bitbucket's basic auth), and roll's posted there.
    #[tokio::test]
    async fn statuses_are_read_and_posted_as_each_host_takes_them() {
        use axum::routing::get;
        let posted = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<(String, Value)>::new()));
        let p = posted.clone();
        let app = axum::Router::new()
            .route("/api/v4/projects/{project}/repository/commits/{sha}/statuses", get(|h: axum::http::HeaderMap, axum::extract::Path((project, _)): axum::extract::Path<(String, String)>| async move {
                assert_eq!((h["private-token"].to_str().unwrap(), project.as_str()), ("glpat-x", "acme/site"));
                axum::Json(json!([{ "id": 2, "name": "test", "status": "running" }, { "id": 1, "name": "test", "status": "failed" }, { "id": 3, "name": "roll/apps/site/production", "status": "failed" }]))
            }))
            .route("/bb/repositories/{ws}/{repo}/commit/{sha}/statuses", get(|h: axum::http::HeaderMap| async move {
                assert!(h["authorization"].to_str().unwrap().starts_with("Basic "));
                axum::Json(json!({ "values": [{ "key": "pipeline", "state": "SUCCESSFUL" }] }))
            }))
            .fallback(move |req: axum::extract::Request| {
                let p = p.clone();
                async move {
                    let path = req.uri().path().to_string();
                    let b = axum::body::to_bytes(req.into_body(), 1 << 16).await.unwrap();
                    p.lock().push((path, serde_json::from_slice(&b).unwrap_or(Value::Null)));
                    axum::Json(json!({}))
                }
            });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let gl = Host::GitLab { api: format!("{base}/api/v4"), project: "acme/site".into() };
        let bb = Host::Bitbucket { api: format!("{base}/bb"), workspace: "ws".into(), repo: "site".into() };
        assert_eq!(read(true, &gl, ("oauth2", "glpat-x"), "abc").await.unwrap(), Ci::Pending, "the retried job's newest run decides; roll's own is left out");
        assert_eq!(read(true, &bb, ("me", "app-pw"), "abc").await.unwrap(), Ci::Passed);
        let s = CommitStatus { context: "roll/apps/site/production".into(), state: "success", description: "live".into(), target_url: Some("https://site.roll.test".into()) };
        post(true, &gl, ("oauth2", "glpat-x"), "abc", &s).await.unwrap();
        post(true, &bb, ("me", "app-pw"), "abc", &s).await.unwrap();
        let got = posted.lock().clone();
        assert_eq!((got[0].0.as_str(), got[0].1["state"].as_str()), ("/api/v4/projects/acme%2Fsite/statuses/abc", Some("success")), "{got:?}");
        assert_eq!((got[1].0.as_str(), got[1].1["state"].as_str(), got[1].1["key"].as_str()), ("/bb/repositories/ws/site/commit/abc/statuses/build", Some("SUCCESSFUL"), Some("roll/apps/site/production")), "{got:?}");
    }
}

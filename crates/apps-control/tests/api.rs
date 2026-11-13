//! roll apps end to end in process: `/v1/apps` and the waking proxy on SQLite, with the fake
//! runtime and builder (no Docker), a fake roll code and a fake DNS.

use apps_control::integrations::{Landing, NoResources, NoSecrets, Revision, Sources, StaticDns};
use apps_control::runtime::fake::{Fake, FakeState};
use apps_control::store::{Sealer, Store};
use apps_control::{Apps, AppsConfig, Deps};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tower::ServiceExt;

/// A roll code with one repo whose `main` moves when a test pushes.
#[derive(Default)]
struct FakeCode {
    files: Mutex<BTreeMap<String, String>>,
    /// The paths each push changed.
    pushed: Mutex<BTreeMap<String, Vec<String>>>,
    head: Mutex<(String, String)>,
    landings: Mutex<Vec<Landing>>,
}

impl FakeCode {
    fn push(&self, sha: &str, message: &str, files: &[(&str, &str)]) {
        self.pushed.lock().insert(sha.to_string(), files.iter().map(|(k, _)| k.to_string()).collect());
        for (k, v) in files {
            self.files.lock().insert(k.to_string(), v.to_string());
        }
        *self.head.lock() = (sha.to_string(), message.to_string());
        let mut l = self.landings.lock();
        let seq = l.len() as i64 + 1;
        l.push(Landing { seq, repo: "acme/site".into(), branch: "main".into(), revision: Revision { sha: sha.into(), message: message.into() }, author: Some("Ada Lovelace".into()) });
    }
}

#[async_trait]
impl Sources for FakeCode {
    async fn resolve(&self, repo: &str, _branch: Option<&str>) -> anyhow::Result<Revision> {
        anyhow::ensure!(repo == "acme/site", "no repo {repo}");
        let (sha, message) = self.head.lock().clone();
        Ok(Revision { sha, message })
    }
    async fn default_branch(&self, _repo: &str) -> anyhow::Result<String> {
        Ok("main".into())
    }
    async fn revision(&self, repo: &str, rev: &str) -> anyhow::Result<Revision> {
        if rev == "HEAD" {
            return self.resolve(repo, None).await;
        }
        Ok(Revision { sha: rev.to_string(), message: format!("at {rev}") })
    }
    async fn materialize(&self, _repo: &str, _revision: &str, dir: &Path) -> anyhow::Result<()> {
        for (k, v) in self.files.lock().iter() {
            let p = dir.join(k);
            std::fs::create_dir_all(p.parent().unwrap())?;
            match v.strip_prefix("symlink:") {
                Some(target) => std::os::unix::fs::symlink(target, p)?,
                None => std::fs::write(p, v)?,
            }
        }
        Ok(())
    }
    async fn landings(&self, after: i64, limit: usize) -> anyhow::Result<Vec<Landing>> {
        Ok(self.landings.lock().iter().filter(|l| l.seq > after).take(limit).cloned().collect())
    }
    async fn head(&self) -> anyhow::Result<i64> {
        Ok(self.landings.lock().len() as i64)
    }
    /// The paths `to`'s push changed, matched as `dir/**` prefixes (roll code's real matcher is
    /// CODEOWNERS', tested there).
    async fn touches(&self, _repo: &str, _from: &str, to: &str, patterns: &[String]) -> anyhow::Result<bool> {
        let changed = self.pushed.lock().get(to).cloned().unwrap_or_default();
        Ok(changed.iter().any(|f| patterns.iter().any(|p| f.starts_with(p.trim_end_matches("**")))))
    }
}

/// A certificate provider that records what it was told (Cloudflare for SaaS's shape: it says
/// where a certificate is when asked).
#[derive(Default)]
struct FakeTls {
    calls: Mutex<Vec<String>>,
    issued: Mutex<Vec<String>>,
}

#[async_trait]
impl apps_control::integrations::DomainTls for FakeTls {
    async fn ensure(&self, host: &str) -> anyhow::Result<Option<(apps_control::model::TlsStatus, Option<String>)>> {
        self.calls.lock().push(format!("ensure {host}"));
        Ok(Some(match self.issued.lock().iter().any(|h| h == host) {
            true => (apps_control::model::TlsStatus::Issued, None),
            false => (apps_control::model::TlsStatus::Pending, Some("waiting for HTTP validation".into())),
        }))
    }
    async fn remove(&self, host: &str) -> anyhow::Result<()> {
        self.calls.lock().push(format!("remove {host}"));
        Ok(())
    }
}

struct World {
    apps: Arc<Apps>,
    api: axum::Router,
    public: axum::Router,
    fake: Arc<Fake>,
    code: Arc<FakeCode>,
    dns: Arc<StaticDns>,
    tls: Arc<FakeTls>,
    project: String,
    _dir: tempfile::TempDir,
}

async fn world() -> World {
    world_with(|_| {}).await
}

async fn world_with(setup: impl FnOnce(&mut AppsConfig)) -> World {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("acme", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let store = Store::new(db, Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
    let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
    let fake = Fake::new();
    let code = Arc::new(FakeCode::default());
    code.push("a1a1a1a1a1a1", "first", &[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http')")]);
    let dns = Arc::new(StaticDns::default());
    let tls = Arc::new(FakeTls::default());
    let mut cfg = AppsConfig::new(dir.path().to_path_buf());
    cfg.live.worker_grace = Duration::from_millis(20);
    cfg.live.probe_every = Duration::from_millis(2);
    cfg.live.drain = Duration::from_millis(50);
    cfg.allow_private = true;
    setup(&mut cfg);
    let deps = Deps {
        runtime: fake.clone(),
        builder: fake.clone(),
        sources: Some(code.clone()),
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: dns.clone(),
        tls: tls.clone(),
        objects: None,
        cluster,
        workspace: Some(ws),
    };
    let apps = Apps::new(cfg, store, deps);
    apps.start();
    World { api: apps_control::api::router(apps.clone()), public: apps_control::public::router(apps.clone()), apps, fake, code, dns, tls, project, _dir: dir }
}

async fn call(r: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(path).header("content-type", "application/json");
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn visit(w: &World, host: &str, path: &str) -> (StatusCode, Value) {
    let req = Request::builder().uri(path).header("host", host).body(Body::empty()).unwrap();
    let resp = w.public.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

/// Wait for a deployment to finish; its final state.
async fn settled(w: &World, svc: &str, env: &str, dep: &str) -> Value {
    for _ in 0..1000 {
        let (_, d) = call(&w.api, "GET", &format!("/v1/apps/services/{svc}/environments/{env}/deploys/{dep}"), None).await;
        if !["queued", "building", "deploying"].contains(&d["status"].as_str().unwrap_or("")) {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{dep} didn't finish");
}

fn host(url: &Value) -> String {
    url.as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string()
}

async fn image_service(w: &World, name: &str, run: Value) -> (Value, Value) {
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": name, "source": {"kind": "image", "image": "nginx:1.27"}, "run": run }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap();
    let latest = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    let d = settled(w, id, "production", &latest).await;
    (svc, d)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_image_service_scales_to_zero_and_wakes_on_the_first_request() {
    let w = world().await;
    let (svc, d) = image_service(&w, "web", json!({"port": 80})).await;
    assert_eq!(d["status"], "active", "{d}");
    assert!(d["image"].as_str().unwrap().contains("@sha256:"), "pinned by digest: {d}");
    let id = svc["id"].as_str().unwrap();
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    assert_eq!(env["state"], "stopped", "health-checked at deploy, then back to zero: {env}");
    // On the project's canvas without anyone placing it.
    let ws = w.apps.deps.workspace.as_ref().unwrap();
    assert!(ws.get(&w.project).unwrap().resources.iter().any(|r| r.r#ref == id));
    // The first request wakes it; the instance sees roll's headers.
    let (st, body) = visit(&w, &host(&env["url"]), "/hello").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["path"], "/hello");
    assert!(!body["forwarded_host"].as_str().unwrap().is_empty());
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    assert_eq!(env["state"], "running");
    // Idle: suspended, then stopped by the reaper; nothing metered while asleep.
    let live = w.apps.live(id, "production");
    live.backdate(Duration::from_secs(31));
    live.tick().await;
    assert_eq!(live.state(), apps_control::model::InstanceState::Suspended);
    let (st, _) = visit(&w, &host(&env["url"]), "/").await;
    assert_eq!(st, StatusCode::OK);
    assert!(Fake::count(&w.fake.counts.resumes) >= 1, "a suspended instance resumes, it isn't started");
    live.backdate(Duration::from_secs(601));
    live.tick().await;
    live.tick().await;
    assert_eq!(live.state(), apps_control::model::InstanceState::Stopped);
    let (_, m) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/metrics?minutes=5"), None).await;
    let sum = |k: &str| m[k].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).sum::<f64>();
    assert_eq!(sum("requests"), 2.0);
    assert_eq!(sum("cold_starts"), 1.0);
    // An unknown host is a 404 page, not an error.
    assert_eq!(visit(&w, "nope.apps.localhost", "/").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repo_service_builds_its_plan_deploys_on_push_and_rolls_back_without_a_build() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let first = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    let d1 = settled(&w, &id, "production", &first).await;
    assert_eq!(d1["status"], "active", "{d1}");
    assert_eq!(d1["revision"], "a1a1a1a1a1a1");
    assert_eq!(d1["plan"]["provider"], "node", "{d1}");
    let steps: Vec<&str> = d1["steps"].as_array().unwrap().iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["build", "rollout", "live"], "{d1}");
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{first}/logs"), None).await;
    assert!(log.as_array().unwrap().iter().any(|l| l["text"].as_str().unwrap().contains("generated Dockerfile")), "the build log shows the Dockerfile");
    // A root reached through a symlink in the pushed tree would build from the host: refused.
    let host = tempfile::tempdir().unwrap();
    std::fs::create_dir(host.path().join("app")).unwrap();
    std::fs::write(host.path().join("app/package.json"), r#"{"scripts":{"start":"node host.js"}}"#).unwrap();
    w.code.files.lock().insert("web".into(), format!("symlink:{}", host.path().display()));
    let (st, v) = call(&w.api, "POST", "/v1/apps/plan", Some(json!({"repo": "acme/site", "root": "web/app"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    w.code.files.lock().remove("web");
    // A push to main deploys it.
    w.code.push("b2b2b2b2b2b2", "second", &[("server.js", "require('http') // v2")]);
    let mut d2 = Value::Null;
    for _ in 0..500 {
        let (_, all) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys"), None).await;
        if let Some(d) = all.as_array().unwrap().iter().find(|d| d["revision"] == "b2b2b2b2b2b2") {
            d2 = settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(d2["status"], "active", "{d2}");
    assert_eq!(d2["trigger"], "push");
    assert_eq!(d2["actor"], "Ada Lovelace", "a push names its commit's author");
    assert_eq!(settled(&w, &id, "production", &first).await["status"], "inactive");
    // A rollback runs the old image again: no build.
    let builds = Fake::count(&w.fake.counts.builds);
    let (st, rb) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys/{first}/rollback"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{rb}");
    let rb = settled(&w, &id, "production", rb["id"].as_str().unwrap()).await;
    assert_eq!((rb["status"].as_str(), rb["rollback_of"].as_str(), rb["image"].clone()), (Some("active"), Some(first.as_str()), d1["image"].clone()));
    assert_eq!(Fake::count(&w.fake.counts.builds), builds);
    // Watching web/: a push elsewhere in the monorepo isn't deployed, one into web/ is.
    let (st, v) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"build": {"watch": ["web/**"]}}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let count = || async { call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys"), None).await.1.as_array().unwrap().len() };
    let before = count().await;
    w.code.push("c3c3c3c3c3c3", "api only", &[("api/main.go", "package main")]);
    w.code.push("d4d4d4d4d4d4", "the site", &[("web/index.js", "v4")]);
    let mut seen = Vec::new();
    for _ in 0..500 {
        let (_, all) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys"), None).await;
        seen = all.as_array().unwrap().iter().map(|d| d["revision"].as_str().unwrap_or("").to_string()).collect();
        if seen.iter().any(|r| r == "d4d4d4d4d4d4") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(seen.iter().any(|r| r == "d4d4d4d4d4d4") && !seen.iter().any(|r| r == "c3c3c3c3c3c3"), "{seen:?}");
    assert_eq!(count().await, before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_that_fails_its_health_check_leaves_the_old_one_serving() {
    let w = world().await;
    let (svc, d1) = image_service(&w, "api", json!({"port": 80, "min_instances": 1, "health_path": "/healthz", "health_timeout_s": 1})).await;
    assert_eq!(d1["status"], "active");
    let id = svc["id"].as_str().unwrap();
    let base = format!("/v1/apps/services/{id}/environments/production");
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_UNHEALTHY": "1"}}))).await;
    let (_, env) = call(&w.api, "GET", &base, None).await;
    assert_eq!(env["needs_deploy"], true, "a variable changed since the active deployment");
    let (_, d2) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d2 = settled(&w, id, "production", d2["id"].as_str().unwrap()).await;
    assert_eq!(d2["status"], "failed", "{d2}");
    assert!(d2["error"].as_str().unwrap().contains("health check"), "{d2}");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    assert_eq!(env["active"]["id"], d1["id"]);
    let (st, body) = visit(&w, &host(&env["url"]), "/").await;
    assert_eq!((st, body["deployment"].clone()), (StatusCode::OK, d1["id"].clone()));
    // Only the old deployment's instance is left.
    assert!(w.fake.instances(id).iter().all(|(_, dep, _)| *dep == d1["id"]));
    // Both are in the event log, in order, for whoever must not miss them (after a second's
    // settling).
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, log) = call(&w.api, "GET", "/v1/apps/events?after=0", None).await;
    let mine: Vec<(&str, &str)> = log["events"].as_array().unwrap().iter().filter(|e| e["service"] == id).map(|e| (e["kind"].as_str().unwrap(), e["deployment"].as_str().unwrap_or(""))).collect();
    assert_eq!(mine, [("deploy.active", d1["id"].as_str().unwrap()), ("deploy.failed", d2["id"].as_str().unwrap())], "{log}");
    assert!(log["events"].as_array().unwrap().iter().any(|e| e["kind"] == "deploy.failed" && e["data"]["error"].as_str().unwrap_or("").contains("health check")));
    let next = log["next"].as_i64().unwrap();
    assert!(call(&w.api, "GET", &format!("/v1/apps/events?after={next}"), None).await.1["events"].as_array().unwrap().is_empty());
    // A pre-deploy command (a migration) that fails stops the deploy before anything starts.
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"unset": ["FAKE_UNHEALTHY"]}))).await;
    let (st, v) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"pre_deploy": "exit 3"}}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, d3) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d3 = settled(&w, id, "production", d3["id"].as_str().unwrap()).await;
    assert_eq!(d3["status"], "failed", "{d3}");
    assert!(d3["error"].as_str().unwrap().contains("pre-deploy command exited with 3"), "{d3}");
    assert!(w.fake.instances(id).iter().all(|(_, dep, _)| *dep == d1["id"]), "nothing of it ran, its container is gone");
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"pre_deploy": "exit 0"}}))).await;
    let (_, d4) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    assert_eq!(settled(&w, id, "production", d4["id"].as_str().unwrap()).await["status"], "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn variables_are_sealed_masked_and_revealed_only_on_their_route() {
    let w = world().await;
    let (svc, _) = image_service(&w, "vars", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production/variables", svc["id"].as_str().unwrap());
    let (st, v) = call(&w.api, "PATCH", &base, Some(json!({"set": {"API_KEY": "sk-very-secret", "FAKE_ECHO": "API_KEY"}}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let key = v["vars"].as_array().unwrap().iter().find(|x| x["name"] == "API_KEY").unwrap();
    assert!(!key["masked"].as_str().unwrap().contains("secret"));
    assert_eq!(key["source"], "service");
    let port = v["vars"].as_array().unwrap().iter().find(|x| x["name"] == "PORT").unwrap();
    assert_eq!((port["masked"].as_str(), port["source"].as_str()), (Some("80"), Some("roll")));
    let (_, values) = call(&w.api, "GET", &format!("{base}/values"), None).await;
    assert_eq!(values["API_KEY"], "sk-very-secret");
    assert_eq!(call(&w.api, "PATCH", &base, Some(json!({"set": {"1BAD": "x"}}))).await.0, StatusCode::BAD_REQUEST);
    // Never in the database in the clear.
    let raw = std::fs::read(w.apps.cfg.home.join("apps.db")).unwrap();
    assert!(!raw.windows(14).any(|x| x == b"sk-very-secret"));
    // Given at creation, they're in production before its first deploy.
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "early", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": {"DATABASE_URL": "postgres://x"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let sid = svc["id"].as_str().unwrap();
    settled(&w, sid, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{sid}/environments/production"), None).await;
    assert_eq!(env["needs_deploy"], false, "the first deploy had them: {env}");
    assert_eq!(call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "bad", "source": {"kind": "upload"}, "variables": {"1X": "y"} }))).await.0, StatusCode::BAD_REQUEST);
    // A private registry: its login is sealed, never returned, and a pull uses it.
    let (st, svc) = call(&w.api, "PUT", &format!("/v1/apps/services/{sid}/registry"), Some(json!({"username": "bot", "password": "ghp-registry-secret"}))).await;
    assert_eq!(st, StatusCode::OK, "{svc}");
    assert_eq!(svc["registry"], json!({"server": "https://index.docker.io/v1/", "username": "bot"}));
    assert!(!svc.to_string().contains("ghp-registry-secret"));
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{sid}/environments/production/deploys"), Some(json!({}))).await;
    settled(&w, sid, "production", d["id"].as_str().unwrap()).await;
    assert_eq!(w.fake.pulled_as.lock().as_deref(), Some("bot"));
    let raw = std::fs::read(w.apps.cfg.home.join("apps.db")).unwrap();
    assert!(!raw.windows(19).any(|x| x == b"ghp-registry-secret"));
    // A login goes only to its own registry: never named for another, forgotten when the image
    // moves to another registry (a write token can change the image, not see the password).
    let (st, _) = call(&w.api, "PUT", &format!("/v1/apps/services/{sid}/registry"), Some(json!({"username": "bot", "password": "p", "server": "evil.example"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (_, svc) = call(&w.api, "PATCH", &format!("/v1/apps/services/{sid}"), Some(json!({"source": {"kind": "image", "image": "nginx:1.28"}}))).await;
    assert_eq!(svc["registry"]["username"], "bot", "same registry: {svc}");
    let (_, svc) = call(&w.api, "PATCH", &format!("/v1/apps/services/{sid}"), Some(json!({"source": {"kind": "image", "image": "evil.example/x:1"}}))).await;
    assert_eq!(svc["registry"], Value::Null, "{svc}");
    // A registry on this host or a private network isn't one dockerd is sent to.
    for image in ["127.0.0.1:5000/app", "localhost:5000/app", "10.0.0.5/app:1", "[::1]:5000/app"] {
        let (st, v) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "priv", "source": {"kind": "image", "image": image} }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{image}: {v}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preview_environment_deploys_a_revision_and_tears_down() {
    let w = world().await;
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"}, "run": {"min_instances": 1} }))).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/pr-7");
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"ephemeral": true}))).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert!(env["url"].as_str().unwrap().contains("-pr-7"), "{env}");
    // A retry with the same Idempotency-Key answers the deployment the first call queued.
    let mut ids = Vec::new();
    for _ in 0..2 {
        let req = Request::post(format!("{base}/deploys")).header("content-type", "application/json").header("idempotency-key", "pv-7@c3c3").body(Body::from(r#"{"revision": "c3c3c3c3c3c3"}"#)).unwrap();
        let resp = w.api.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let d: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        ids.push(d["id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids[0], ids[1]);
    let req = Request::post(format!("{base}/deploys")).header("content-type", "application/json").header("idempotency-key", "pv-7@c3c3").body(Body::from(r#"{"revision": "d4d4d4d4d4d4"}"#)).unwrap();
    assert_eq!(w.api.clone().oneshot(req).await.unwrap().status(), StatusCode::CONFLICT, "the same key on another request");
    let d = settled(&w, &id, "pr-7", &ids[0]).await;
    assert_eq!((d["status"].as_str(), d["revision"].as_str()), (Some("active"), Some("c3c3c3c3c3c3")));
    assert!(w.fake.instances(&id).iter().any(|(_, dep, st)| *dep == d["id"] && *st == FakeState::Running), "min_instances 1 keeps one up");
    // Private, as previews are: its hostname answers only visitors with the token.
    let token = "aaaaaaaabbbbbbbbccccdddd";
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"access": {"token": token}}))).await;
    assert_eq!((st, env["private"].as_bool()), (StatusCode::OK, Some(true)), "{env}");
    let get = |path: &str, cookie: Option<&str>| {
        let mut r = Request::builder().uri(path).header("host", host(&env["url"]));
        if let Some(c) = cookie {
            r = r.header("cookie", c);
        }
        w.public.clone().oneshot(r.body(Body::empty()).unwrap())
    };
    assert_eq!(get("/", None).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(get("/?roll_access=wrongwrongwrongwrongwrong", None).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let r = get(&format!("/pricing?roll_access={token}&plan=pro"), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(r.headers()["location"], "/pricing?plan=pro", "the token leaves the URL");
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set.starts_with(&format!("roll_access_pr-7={token};")) && set.contains("HttpOnly") && set.contains("SameSite=Lax") && !set.contains("Domain"), "{set}");
    let r = get("/pricing", Some(&format!("theme=dark; roll_access_pr-7={token}"))).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["cookie"], "theme=dark", "the app never sees the token");
    // An automation's bypass (CI's end-to-end tests): its header lets a request in, no cookie.
    let bypass = "ci-bypass-secret-0123456789";
    let sha = { use sha2::Digest; sha2::Sha256::digest(bypass.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>() };
    let (st, _) = call(&w.api, "PUT", &base, Some(json!({"access": {"token": token, "bypass_sha256": sha}}))).await;
    assert_eq!(st, StatusCode::OK);
    let with = |v: &str| {
        let r = Request::builder().uri("/").header("host", host(&env["url"])).header("x-roll-preview-bypass", v).body(Body::empty()).unwrap();
        w.public.clone().oneshot(r)
    };
    let r = with(bypass).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(!body.to_string().contains(bypass), "the app never sees the bypass");
    assert_eq!(with("not-the-secret").await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let (st, _) = call(&w.api, "DELETE", &base, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(!w.fake.instances(&id).iter().any(|(_, dep, _)| *dep == d["id"]), "its instances are gone");
    assert_eq!(call(&w.api, "GET", &base, None).await.0, StatusCode::NOT_FOUND);
    let (_, all) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments"), None).await;
    assert_eq!(all.as_array().unwrap().len(), 1, "production stays");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_custom_domain_routes_once_its_dns_points_at_roll() {
    let w = world().await;
    let (svc, _) = image_service(&w, "www", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production/domains", svc["id"].as_str().unwrap());
    let (st, d) = call(&w.api, "POST", &base, Some(json!({"hostname": "www.example.com"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert_eq!(d["status"], "pending");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"hostname": "x.apps.localhost"}))).await.0, StatusCode::BAD_REQUEST, "roll's own names can't be added");
    assert_eq!(visit(&w, "www.example.com", "/").await.0, StatusCode::NOT_FOUND, "not before it's verified");
    w.dns.add("www.example.com", "CNAME", d["target"].as_str().unwrap());
    let (_, v) = call(&w.api, "POST", &format!("{base}/www.example.com/verify"), None).await;
    assert_eq!(v["status"], "active", "{v}");
    assert_eq!(visit(&w, "www.example.com", "/x").await.0, StatusCode::OK);
    // The certificate: the provider heard of the hostname only once it was accepted (never the
    // refused one), and verifying reads where its certificate is.
    assert_eq!((d["tls"].as_str(), d["tls_error"].as_str()), (Some("pending"), Some("waiting for HTTP validation")), "{d}");
    w.tls.issued.lock().push("www.example.com".into());
    let (_, v) = call(&w.api, "POST", &format!("{base}/www.example.com/verify"), None).await;
    assert_eq!((v["tls"].as_str(), &v["tls_error"]), (Some("issued"), &Value::Null), "{v}");
    assert!(w.tls.calls.lock().iter().all(|c| c.ends_with(" www.example.com")), "{:?}", w.tls.calls.lock());
    assert_eq!(call(&w.api, "DELETE", &format!("{base}/www.example.com"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(w.tls.calls.lock().last().map(String::as_str), Some("remove www.example.com"));
}

/// A new environment starts with another's variables, and promoting deploys what another
/// environment runs (its image, no pull or build) with the target's own variables.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_is_cloned_and_a_live_image_promoted_to_it() {
    let w = world().await;
    let (svc, d) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments");
    let (st, _) = call(&w.api, "PATCH", &format!("{base}/production/variables"), Some(json!({"set": {"API_KEY": "k1"}}))).await;
    assert_eq!(st, StatusCode::OK);
    let (st, env) = call(&w.api, "PUT", &format!("{base}/staging"), Some(json!({"variables_from": "production"}))).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    let (_, vals) = call(&w.api, "GET", &format!("{base}/staging/variables/values"), None).await;
    assert_eq!(vals["API_KEY"], "k1", "{vals}");
    let pulls = Fake::count(&w.fake.counts.pulls);
    let (st, p) = call(&w.api, "POST", &format!("{base}/staging/promote"), Some(json!({"from": "production"}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{p}");
    let p = settled(&w, &id, "staging", p["id"].as_str().unwrap()).await;
    assert_eq!((p["status"].as_str(), p["trigger"].as_str(), &p["image"]), (Some("active"), Some("promote"), &d["image"]), "{p}");
    assert_eq!(Fake::count(&w.fake.counts.pulls), pulls, "nothing pulled or built");
    assert_eq!(call(&w.api, "POST", &format!("{base}/staging/promote"), Some(json!({"from": "staging"}))).await.0, StatusCode::BAD_REQUEST);
}

/// A running instance whose health check keeps failing (its port still open) is replaced, and
/// the environment's log says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_instance_that_fails_its_health_check_is_replaced() {
    let w = world_with(|cfg| cfg.live.health_every = Duration::from_millis(10)).await;
    let (svc, d) = image_service(&w, "api", json!({"port": 80, "health_path": "/healthz", "min_instances": 1})).await;
    assert_eq!(d["status"], "active", "{d}");
    let id = svc["id"].as_str().unwrap().to_string();
    let running = |w: &World| w.fake.instances(&id).into_iter().filter(|(_, _, st)| *st == FakeState::Running).map(|(n, _, _)| n).collect::<Vec<_>>();
    let first = running(&w).pop().expect("min_instances 1 keeps one up");
    w.fake.sicken(&first);
    let mut replaced = false;
    for _ in 0..300 {
        let now = running(&w);
        if !now.contains(&first) && !now.is_empty() {
            replaced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(replaced, "{:?}", w.fake.instances(&id));
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs"), None).await;
    assert!(log.to_string().contains("health check /healthz failed"), "{log}");
}

/// Where every tenant's CNAME points at one target (Cloudflare for SaaS), a CNAME proves nothing:
/// a hostname someone else already points there (or left pointing) needs its TXT challenge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_cname_target_takes_the_txt_challenge_as_proof() {
    let w = world_with(|cfg| cfg.cname_target = Some("up.roll.test".into())).await;
    let (svc, _) = image_service(&w, "www", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production/domains", svc["id"].as_str().unwrap());
    let (_, d) = call(&w.api, "POST", &base, Some(json!({"hostname": "www.victim.com"}))).await;
    assert_eq!((d["record"].as_str(), d["target"].as_str()), (Some("both"), Some("up.roll.test")), "{d}");
    w.dns.add("www.victim.com", "CNAME", "up.roll.test");
    let (_, v) = call(&w.api, "POST", &format!("{base}/www.victim.com/verify"), None).await;
    assert_eq!(v["status"], "pending", "a CNAME to the shared target isn't proof: {v}");
    w.dns.add("_roll-challenge.www.victim.com", "TXT", d["token"].as_str().unwrap());
    let (_, v) = call(&w.api, "POST", &format!("{base}/www.victim.com/verify"), None).await;
    assert_eq!(v["status"], "active", "{v}");
}

/// A claim nobody verifies expires (the hostname is free again, and gone from the certificate's
/// provider); a verified one stays; a removed environment drops its claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unverified_domain_claims_expire_and_removed_environments_drop_theirs() {
    let w = world().await;
    let (svc, _) = image_service(&w, "shop", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production/domains");
    for h in ["squat.example.com", "mine.example.com"] {
        assert_eq!(call(&w.api, "POST", &base, Some(json!({ "hostname": h }))).await.0, StatusCode::CREATED);
    }
    let (_, d) = call(&w.api, "GET", &base, None).await;
    w.dns.add("mine.example.com", "CNAME", d[1]["target"].as_str().unwrap());
    assert_eq!(call(&w.api, "POST", &format!("{base}/mine.example.com/verify"), None).await.1["status"], "active");
    assert!(w.apps.expire_domain_claims(apps_control::PENDING_CLAIM_TTL).await.unwrap().is_empty(), "nothing is a week old");
    assert_eq!(w.apps.expire_domain_claims(Duration::ZERO).await.unwrap(), ["squat.example.com"]);
    let (_, d) = call(&w.api, "GET", &base, None).await;
    let hosts: Vec<&str> = d.as_array().unwrap().iter().filter(|d| d["kind"] == "custom").map(|d| d["hostname"].as_str().unwrap()).collect();
    assert_eq!(hosts, ["mine.example.com"]);
    assert!(w.tls.calls.lock().contains(&"remove squat.example.com".to_string()));

    // Another environment with a domain, removed: the claim goes with it.
    let staging = format!("/v1/apps/services/{id}/environments/staging");
    assert_eq!(call(&w.api, "PUT", &staging, Some(json!({}))).await.0, StatusCode::OK);
    assert_eq!(call(&w.api, "POST", &format!("{staging}/domains"), Some(json!({"hostname": "staging.example.com"}))).await.0, StatusCode::CREATED);
    assert_eq!(call(&w.api, "DELETE", &staging, None).await.0, StatusCode::NO_CONTENT);
    assert!(w.tls.calls.lock().contains(&"remove staging.example.com".to_string()));
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"hostname": "staging.example.com"}))).await.0, StatusCode::CREATED, "the hostname is free again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roll_up_deploys_an_uploaded_tree_and_cron_runs_on_demand() {
    let w = world().await;
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "local", "source": {"kind": "upload"} }))).await;
    let id = svc["id"].as_str().unwrap().to_string();
    assert!(svc["environments"][0]["latest"].is_null(), "an upload service waits for `roll up`");
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("index.html"), "<h1>hi</h1>").unwrap();
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let req = Request::post(format!("/v1/apps/services/{id}/environments/production/deploys")).header("content-type", "application/gzip").body(Body::from(bundle)).unwrap();
    let resp = w.api.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let d: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let d = settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    assert_eq!((d["status"].as_str(), d["trigger"].as_str(), d["plan"]["provider"].as_str()), (Some("active"), Some("upload"), Some("static")), "{d}");
    // The image is the deployment's now; the uploaded tree isn't kept.
    let uploads = w.apps.cfg.home.join("apps/uploads");
    assert!(std::fs::read_dir(&uploads).map(|mut d| d.next().is_none()).unwrap_or(true), "the upload is deleted after its build");
    // A variable changes: a redeploy runs the same image with it, no upload and no build.
    let base = format!("/v1/apps/services/{id}/environments/production");
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"GREETING": "hej", "FAKE_ECHO": "GREETING"}}))).await;
    assert_eq!(call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await.0, StatusCode::BAD_REQUEST, "nothing to deploy without an upload");
    let builds = Fake::count(&w.fake.counts.builds);
    let (st, r) = call(&w.api, "POST", &format!("{base}/redeploy"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{r}");
    let r = settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    assert_eq!((r["status"].as_str(), r["trigger"].as_str()), (Some("active"), Some("settings")), "{r}");
    assert_eq!(r["image"], d["image"]);
    assert_eq!(Fake::count(&w.fake.counts.builds), builds, "no build");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    assert_eq!(visit(&w, &host(&env["url"]), "/").await.1["vars"]["GREETING"], "hej");

    // A cron service: runs to its exit, recorded.
    let (_, cron) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "nightly", "source": {"kind": "image", "image": "busybox"}, "run": {"type": "cron", "schedule": "0 3 * * *"} }))).await;
    let cid = cron["id"].as_str().unwrap().to_string();
    let dep = cron["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &cid, "production", &dep).await["status"], "active");
    let base = format!("/v1/apps/services/{cid}/environments/production");
    // Its next run is the schedule's next 03:00 UTC.
    let next = call(&w.api, "GET", &base, None).await.1["next_run_at"].as_i64().unwrap();
    let at = chrono::DateTime::from_timestamp_millis(next).unwrap();
    assert_eq!((chrono::Timelike::hour(&at), chrono::Timelike::minute(&at)), (3, 0));
    assert!(next > chrono::Utc::now().timestamp_millis() && next <= chrono::Utc::now().timestamp_millis() + 86_400_000);
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_EXIT": "0"}}))).await;
    // The variable reaches runs of a new deployment.
    let (_, d) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d = settled(&w, &cid, "production", d["id"].as_str().unwrap()).await;
    assert_eq!(d["status"], "active", "{d}");
    let (st, run) = call(&w.api, "POST", &format!("{base}/runs"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{run}");
    let mut last = Value::Null;
    for _ in 0..500 {
        let (_, runs) = call(&w.api, "GET", &format!("{base}/runs"), None).await;
        last = runs[0].clone();
        if last["status"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!((last["status"].as_str(), last["exit_code"].as_i64()), (Some("succeeded"), Some(0)), "{last}");
    let _ = Ordering::SeqCst;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_off_job_runs_the_live_image_to_its_exit_and_can_be_canceled() {
    let w = world().await;
    let (svc, d) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let instances = w.fake.instances(&id).len();
    let ended = |job: String| {
        let (api, base) = (w.api.clone(), base.clone());
        async move {
            for _ in 0..500 {
                let (_, j) = call(&api, "GET", &format!("{base}/jobs/{job}"), None).await;
                if j["status"] != "running" {
                    return j;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("job {job} never ended");
        }
    };
    let (st, j) = call(&w.api, "POST", &format!("{base}/jobs"), Some(json!({"command": "exit 3"}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{j}");
    assert_eq!(j["deployment"], d["id"], "the live deployment's image");
    let j = ended(j["id"].as_str().unwrap().to_string()).await;
    assert_eq!((j["status"].as_str(), j["exit_code"].as_i64()), (Some("failed"), Some(3)), "{j}");
    let (_, log) = call(&w.api, "GET", &format!("{base}/jobs/{}/logs", j["id"].as_str().unwrap()), None).await;
    assert!(log.as_array().unwrap().iter().any(|l| l["text"] == "$ exit 3"), "{log}");
    let (_, ok) = call(&w.api, "POST", &format!("{base}/jobs"), Some(json!({"command": "exit 0"}))).await;
    assert_eq!(ended(ok["id"].as_str().unwrap().to_string()).await["status"], "succeeded");
    // One that doesn't end by itself is canceled, and its container goes.
    let (_, long) = call(&w.api, "POST", &format!("{base}/jobs"), Some(json!({"command": "sleep 3600"}))).await;
    let lid = long["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(call(&w.api, "POST", &format!("{base}/jobs/{lid}/cancel"), None).await.0, StatusCode::OK);
    assert_eq!(ended(lid.clone()).await["status"], "canceled");
    assert_eq!(call(&w.api, "POST", &format!("{base}/jobs/{lid}/cancel"), None).await.0, StatusCode::CONFLICT);
    assert_eq!(w.fake.instances(&id).len(), instances, "no job's container is left");
    assert_eq!(call(&w.api, "GET", &format!("{base}/jobs"), None).await.1.as_array().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shell_wakes_the_service_and_carries_the_terminal_both_ways() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, w.api.clone()).into_future());
    let url = format!("ws://{addr}/v1/apps/services/{id}/environments/production/shell?cols=100&rows=30");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let first = ws.next().await.unwrap().unwrap().into_data();
    let banner = String::from_utf8_lossy(&first).to_string();
    assert!(banner.contains("fake shell in roll-apps-") && banner.contains("(100x30)"), "{banner}");
    // Asleep before: the shell woke it, and keeps it up while open.
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    assert_eq!(env["state"], "running", "{env}");
    ws.send(Message::Text(r#"{"resize":[120,40]}"#.into())).await.unwrap();
    ws.send(Message::Binary(b"ls -la\r".to_vec().into())).await.unwrap();
    let echoed = ws.next().await.unwrap().unwrap().into_data();
    assert_eq!(&echoed[..], b"ls -la\r");
    ws.close(None).await.unwrap();

    // The same over HTTP (the dashboard's terminal): the screen as SSE, keys as POSTs.
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let base = format!("/v1/apps/services/{id}/environments/production/shell/sessions");
    let (st, open) = call(&w.api, "POST", &base, Some(json!({"cols": 90, "rows": 20}))).await;
    assert_eq!(st, StatusCode::CREATED, "{open}");
    let sh = open["id"].as_str().unwrap().to_string();
    let resp = w.api.clone().oneshot(Request::get(format!("{base}/{sh}/screen")).body(Body::empty()).unwrap()).await.unwrap();
    let mut body = resp.into_body();
    assert!(sse_chunk(&mut body).await.contains("(90x20)"), "the scrollback first");
    assert_eq!(call(&w.api, "POST", &format!("{base}/{sh}/keys"), Some(json!({"data": b64.encode("echo hi\r")}))).await.0, StatusCode::NO_CONTENT);
    assert_eq!(sse_chunk(&mut body).await, "echo hi\r");
    assert_eq!(call(&w.api, "POST", &format!("{base}/{sh}/resize"), Some(json!({"cols": 100, "rows": 30}))).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&w.api, "DELETE", &format!("{base}/{sh}"), None).await.0, StatusCode::NO_CONTENT);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(call(&w.api, "POST", &format!("{base}/{sh}/keys"), Some(json!({"data": b64.encode("x")}))).await.0, StatusCode::NOT_FOUND, "closed");
}

/// The next base64 event of an SSE body, decoded.
async fn sse_chunk(body: &mut Body) -> String {
    use base64::Engine as _;
    loop {
        let f = body.frame().await.unwrap().unwrap();
        if let Some(d) = f.data_ref()
            && let Some(data) = String::from_utf8_lossy(d).lines().find_map(|l| l.strip_prefix("data:").map(str::to_string))
        {
            return String::from_utf8(base64::engine::general_purpose::STANDARD.decode(data.trim()).unwrap()).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_deploy_is_sent_to_the_projects_targets_signed_and_retried() {
    use hmac::Mac;
    let w = world().await;
    // A receiver that answers 200, and one that keeps failing.
    let got: Arc<parking_lot::Mutex<Vec<(axum::http::HeaderMap, Value)>>> = Default::default();
    let g = got.clone();
    let ok = axum::Router::new().route("/hook", axum::routing::post(move |h: axum::http::HeaderMap, b: bytes::Bytes| {
        let g = g.clone();
        async move {
            g.lock().push((h, serde_json::from_slice(&b).unwrap()));
            StatusCode::OK
        }
    }));
    let bad = axum::Router::new().fallback(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "down") });
    let serve = |app: axum::Router| async move {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(axum::serve(l, app).into_future());
        a
    };
    let (ok_at, bad_at) = (serve(ok).await, serve(bad).await);
    let base = format!("/v1/apps/projects/{}/notifications", w.project);
    let (st, hook) = call(&w.api, "POST", &base, Some(json!({"url": format!("http://{ok_at}/hook")}))).await;
    assert_eq!(st, StatusCode::CREATED, "{hook}");
    let secret = hook["secret"].as_str().unwrap().to_string();
    assert_eq!(hook["events"], json!(["deploy.failed", "crash_loop"]));
    let (_, down) = call(&w.api, "POST", &base, Some(json!({"kind": "webhook", "url": format!("http://{bad_at}/x"), "events": ["deploy.failed"]}))).await;
    let (_, list) = call(&w.api, "GET", &base, None).await;
    assert!(!list.to_string().contains(&secret) && !list.to_string().contains("/hook"), "neither the secret nor the URL comes back: {list}");
    assert_eq!(call(&w.api, "POST", &format!("{base}/{}/test", hook["id"].as_str().unwrap()), None).await.1["ok"], true);
    // A deploy fails its health check: both targets get a try.
    let (svc, _) = image_service(&w, "api", json!({"port": 80, "health_path": "/healthz", "health_timeout_s": 1})).await;
    let id = svc["id"].as_str().unwrap();
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}/environments/production/variables"), Some(json!({"set": {"FAKE_UNHEALTHY": "1"}}))).await;
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), Some(json!({}))).await;
    assert_eq!(settled(&w, id, "production", d["id"].as_str().unwrap()).await["status"], "failed");
    let mut sent = None;
    for _ in 0..100 {
        if let Some(x) = got.lock().iter().find(|(_, b)| b["type"] == "deploy.failed").cloned() {
            sent = Some(x);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (h, body) = sent.expect("the webhook got deploy.failed");
    assert_eq!((body["service"].as_str(), body["environment"].as_str()), (Some(id), Some("production")));
    assert!(body["text"].as_str().unwrap().contains("health check"), "{body}");
    // Signed: HMAC-SHA256 of "<timestamp>.<body>" with the target's secret.
    let raw = serde_json::to_vec(&body).unwrap();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(h["x-roll-timestamp"].as_bytes());
    mac.update(b".");
    mac.update(&raw);
    let want: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(h["x-roll-signature-256"].to_str().unwrap(), format!("sha256={want}"));
    // Seconds at send time (a retry passes a receiver's replay window), and the delivery's id.
    assert!((h["x-roll-timestamp"].to_str().unwrap().parse::<i64>().unwrap() - code_sql::now_ms() / 1000).abs() < 120);
    assert!(h.contains_key("x-roll-delivery"));
    // The failing one is logged and waits to try again.
    let log = call(&w.api, "GET", &format!("{base}/{}/deliveries", down["id"].as_str().unwrap()), None).await.1;
    let first = &log.as_array().unwrap()[0];
    assert_eq!((first["status"].as_str(), first["attempts"].as_u64(), first["code"].as_u64()), (Some("pending"), Some(1), Some(500)), "{log}");
    // Targets are checked: not a URL that isn't one, not Slack anywhere but Slack.
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": "ftp://x"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": "https://hooks.example.com/x", "events": ["nope"]}))).await.0, StatusCode::BAD_REQUEST);
}

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
    head: Mutex<(String, String)>,
    landings: Mutex<Vec<Landing>>,
}

impl FakeCode {
    fn push(&self, sha: &str, message: &str, files: &[(&str, &str)]) {
        for (k, v) in files {
            self.files.lock().insert(k.to_string(), v.to_string());
        }
        *self.head.lock() = (sha.to_string(), message.to_string());
        let mut l = self.landings.lock();
        let seq = l.len() as i64 + 1;
        l.push(Landing { seq, repo: "acme/site".into(), branch: "main".into(), revision: Revision { sha: sha.into(), message: message.into() } });
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
}

struct World {
    apps: Arc<Apps>,
    api: axum::Router,
    public: axum::Router,
    fake: Arc<Fake>,
    code: Arc<FakeCode>,
    dns: Arc<StaticDns>,
    project: String,
    _dir: tempfile::TempDir,
}

async fn world() -> World {
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
    let mut cfg = AppsConfig::new(dir.path().to_path_buf());
    cfg.live.worker_grace = Duration::from_millis(20);
    cfg.live.probe_every = Duration::from_millis(2);
    cfg.live.drain = Duration::from_millis(50);
    let deps = Deps {
        runtime: fake.clone(),
        builder: fake.clone(),
        sources: Some(code.clone()),
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: dns.clone(),
        objects: None,
        cluster,
        workspace: Some(ws),
    };
    let apps = Apps::new(cfg, store, deps);
    apps.start();
    World { api: apps_control::api::router(apps.clone()), public: apps_control::public::router(apps.clone()), apps, fake, code, dns, project, _dir: dir }
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
    assert_eq!(settled(&w, &id, "production", &first).await["status"], "inactive");
    // A rollback runs the old image again: no build.
    let builds = Fake::count(&w.fake.counts.builds);
    let (st, rb) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys/{first}/rollback"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{rb}");
    let rb = settled(&w, &id, "production", rb["id"].as_str().unwrap()).await;
    assert_eq!((rb["status"].as_str(), rb["rollback_of"].as_str(), rb["image"].clone()), (Some("active"), Some(first.as_str()), d1["image"].clone()));
    assert_eq!(Fake::count(&w.fake.counts.builds), builds);
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
    let d = settled(&w, &id, "pr-7", &ids[0]).await;
    assert_eq!((d["status"].as_str(), d["revision"].as_str()), (Some("active"), Some("c3c3c3c3c3c3")));
    assert!(w.fake.instances(&id).iter().any(|(_, dep, st)| *dep == d["id"] && *st == FakeState::Running), "min_instances 1 keeps one up");
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

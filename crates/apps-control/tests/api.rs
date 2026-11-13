//! roll apps end to end in process: `/v1/apps` and the waking proxy on SQLite, with the fake
//! runtime and builder (no Docker), a fake roll code and a fake DNS.

use apps_control::integrations::{Landing, NoSecrets, Revision, Sources, StaticDns};
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
    /// Commit statuses roll apps posted: (sha, context, state).
    statuses: Mutex<Vec<(String, String, String)>>,
    /// What a commit's checks say (CI).
    ci: Mutex<Option<apps_control::integrations::Ci>>,
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
    async fn set_status(&self, _repo: &str, sha: &str, st: &apps_control::integrations::CommitStatus) -> anyhow::Result<()> {
        self.statuses.lock().push((sha.to_string(), st.context.clone(), st.state.to_string()));
        Ok(())
    }
    async fn ci(&self, _repo: &str, _sha: &str) -> anyhow::Result<apps_control::integrations::Ci> {
        Ok(self.ci.lock().clone().unwrap_or(apps_control::integrations::Ci::None))
    }
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

/// Two Postgres projects a service can link, `main` and `orders`, whose variables are read
/// fresh; and a Redis database, `cache`, that makes each link a key of its own.
struct FakeResources;

/// What the fake made and revoked for `cache` links, in order.
static MINTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[async_trait]
impl apps_control::integrations::Resources for FakeResources {
    async fn link_vars(&self, kind: apps_control::model::LinkKind, r#ref: &str) -> anyhow::Result<Vec<(String, String)>> {
        match (kind.as_str(), r#ref) {
            ("postgres", "main" | "orders") => Ok(vec![("DATABASE_URL".into(), format!("postgres://app:pw@db.roll.test:5432/{ref}"))]),
            _ => anyhow::bail!("no {} {ref}", kind.as_str()),
        }
    }

    async fn link(&self, kind: apps_control::model::LinkKind, r#ref: &str, service: &str) -> anyhow::Result<Option<apps_control::integrations::Minted>> {
        if (kind.as_str(), r#ref) != ("redis", "cache") {
            return Ok(None);
        }
        let mut m = MINTED.lock();
        let key = format!("key{}", m.len());
        m.push(format!("link {service} {key}"));
        Ok(Some(apps_control::integrations::Minted { vars: vec![("REDIS_URL".into(), format!("redis://{key}@cache.roll.test:6379"))], kept: key }))
    }

    async fn unlink(&self, _kind: apps_control::model::LinkKind, _ref: &str, kept: &str) -> anyhow::Result<()> {
        MINTED.lock().push(format!("unlink {kept}"));
        Ok(())
    }

    async fn backlog(&self, stream: &str, _group: &str) -> anyhow::Result<u64> {
        anyhow::ensure!(stream == "st_ag2345678abc", "no stream {stream}");
        Ok(BACKLOG.load(Ordering::SeqCst))
    }
}

/// The fake stream's consumer group lag (a scaling worker's test sets it).
static BACKLOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    world_full(setup, false).await
}

/// `objects`: a directory as object storage (log chunks, volume backups).
async fn world_full(setup: impl FnOnce(&mut AppsConfig), objects: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("", "acme", "", None).unwrap().id;
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
        resources: Arc::new(FakeResources),
        resolver: dns.clone(),
        tls: tls.clone(),
        objects: objects.then(|| pg_storage::Objects::new(&pg_storage::StorageConfig::Local(dir.path().join("objects"))).unwrap()),
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

/// GET `path` until `pred` holds of its answer (about 4 s at most).
async fn get_until(api: &axum::Router, path: &str, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let (_, v) = call(api, "GET", path, None).await;
        if pred(&v) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{path} never got there")
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
    assert!(ws.get(roll_workspace::Scope::All, &w.project).unwrap().resources.iter().any(|r| r.r#ref == id));
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
    assert_eq!(d2["actor"], "git:Ada Lovelace", "a push names its commit's author, as the commit claims it");
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
    let mine: Vec<(&str, &str)> = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["service"] == id && !e["kind"].as_str().unwrap_or("").starts_with("audit."))
        .map(|e| (e["kind"].as_str().unwrap(), e["deployment"].as_str().unwrap_or("")))
        .collect();
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
    // Given at creation, a private image's first pull already logs in.
    *w.fake.pulled_as.lock() = None;
    let (st, v) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "private", "source": {"kind": "image", "image": "ghcr.io/acme/api:1"}, "registry": {"username": "ci", "password": "ghp-x"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    settled(&w, v["id"].as_str().unwrap(), "production", v["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!(w.fake.pulled_as.lock().as_deref(), Some("ci"));
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
    // A preview scales to zero though the service keeps one up (roll.toml [app.previews.run] could
    // say otherwise): an open pull request costs nothing until someone opens it.
    assert!(!w.fake.instances(&id).iter().any(|(_, dep, st)| *dep == d["id"] && *st == FakeState::Running), "{:?}", w.fake.instances(&id));
    let (_, log) = call(&w.api, "GET", &format!("{base}/deploys/{}/logs", d["id"].as_str().unwrap()), None).await;
    assert!(log.to_string().contains("a preview environment scales to zero"), "{log}");
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

/// A link puts its resource's variables (or credentials it made for itself, once, revoked when it
/// goes) in the service's environment (prefixed when asked),
/// and a running instance has them; a resource that doesn't exist can't be linked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_puts_its_variables_in_the_instances() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let links = format!("/v1/apps/services/{id}/links");
    let (st, l) = call(&w.api, "PUT", &format!("{links}/postgres/main"), Some(json!({}))).await;
    assert_eq!((st, &l["vars"]), (StatusCode::OK, &json!(["DATABASE_URL"])), "{l}");
    let (_, l) = call(&w.api, "PUT", &format!("{links}/postgres/orders"), Some(json!({"prefix": "ORDERS"}))).await;
    assert_eq!(l["vars"], json!(["ORDERS_DATABASE_URL"]), "{l}");
    assert_eq!(call(&w.api, "PUT", &format!("{links}/postgres/nope"), Some(json!({}))).await.0, StatusCode::BAD_REQUEST);
    // A link that makes credentials of its own makes them once: setting it again keeps them.
    let (st, l) = call(&w.api, "PUT", &format!("{links}/redis/cache"), Some(json!({}))).await;
    assert_eq!((st, &l["vars"]), (StatusCode::OK, &json!(["REDIS_URL"])), "{l}");
    call(&w.api, "PUT", &format!("{links}/redis/cache"), Some(json!({}))).await;
    assert_eq!(*MINTED.lock(), [format!("link {id} key0")]);
    let base = format!("/v1/apps/services/{id}/environments/production");
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_ECHO": "DATABASE_URL,ORDERS_DATABASE_URL,REDIS_URL"}}))).await;
    let (_, r) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let (st, body) = visit(&w, &host(&svc["environments"][0]["url"]), "/").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["vars"], json!({"DATABASE_URL": "postgres://app:pw@db.roll.test:5432/main", "ORDERS_DATABASE_URL": "postgres://app:pw@db.roll.test:5432/orders", "REDIS_URL": "redis://key0@cache.roll.test:6379"}));
    // Unlinking revokes them, and so does deleting the service.
    assert_eq!(call(&w.api, "DELETE", &format!("{links}/redis/cache"), None).await.0, StatusCode::NO_CONTENT);
    call(&w.api, "PUT", &format!("{links}/redis/cache"), Some(json!({}))).await;
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(*MINTED.lock(), [format!("link {id} key0"), "unlink key0".into(), format!("link {id} key2"), "unlink key2".into()]);
    // Linked when it's made, the first deploy has them; a link that doesn't resolve makes nothing.
    let (st, made) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "worker", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "links": [{"kind": "postgres", "ref": "main"}], "variables": {"FAKE_ECHO": "DATABASE_URL"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{made}");
    settled(&w, made["id"].as_str().unwrap(), "production", made["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let (_, body) = visit(&w, &host(&made["environments"][0]["url"]), "/").await;
    assert_eq!(body["vars"]["DATABASE_URL"], "postgres://app:pw@db.roll.test:5432/main");
    let (st, _) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "broken", "source": {"kind": "image", "image": "nginx:1.27"}, "links": [{"kind": "postgres", "ref": "nope"}] }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(call(&w.api, "GET", "/v1/apps/services", None).await.1.as_array().unwrap().iter().all(|s| s["name"] != "broken"), "nothing was left behind");
}

/// Requests that pile up past an instance's concurrency start more instances, up to
/// `max_instances`; once they've gone, the extra instances go one at a time, the newest
/// request's instance last.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_past_an_instances_concurrency_scale_it_out_and_back_in() {
    let w = world_with(|c| c.live.scale_down_after = Duration::ZERO).await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "busy", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80, "concurrency": 1, "max_instances": 3}, "variables": {"FAKE_SLOW_MS": "400"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let host = host(&svc["environments"][0]["url"]);
    let all = futures::future::join_all((0..6).map(|_| visit(&w, &host, "/"))).await;
    assert!(all.iter().all(|(st, _)| *st == StatusCode::OK), "{all:?}");
    let running = |w: &World| w.fake.instances(&id).iter().filter(|(_, _, st)| *st == FakeState::Running).count();
    // Counted as made: the reaper may already be taking them in.
    let made = w.fake.instances(&id).len();
    assert!((2..=3).contains(&made), "scaled out to {made} (max 3)");
    // The crowd gone: one goes per pass (each pass first notes it's over, then stops one).
    let live = w.apps.live(&id, "production");
    for _ in 0..(2 * made) {
        live.tick().await;
    }
    assert_eq!(running(&w), 1, "back to one, still awake for the next request");
    assert_eq!(visit(&w, &host, "/").await.0, StatusCode::OK);
}

/// A push while a build runs queues; a newer one supersedes the queued one, and only the newest
/// goes live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_push_supersedes_a_queued_deploy() {
    let w = world().await;
    w.code.push("f0f0f0f0f0f0", "slow", &[("SLOW_BUILD", "600")]);
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let first = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production/deploys");
    get_until(&w.api, &format!("{base}/{first}"), |d| d["status"] == "building").await;
    let (_, second) = call(&w.api, "POST", &base, Some(json!({"revision": "f0f0f0f0f0f0", "message": "second"}))).await;
    let (_, third) = call(&w.api, "POST", &base, Some(json!({"revision": "f0f0f0f0f0f0", "message": "third"}))).await;
    assert_eq!(settled(&w, &id, "production", second["id"].as_str().unwrap()).await["status"], "superseded");
    assert_eq!(settled(&w, &id, "production", third["id"].as_str().unwrap()).await["status"], "active");
    assert_eq!(settled(&w, &id, "production", &first).await["status"], "inactive", "the first finished, then the newest replaced it");
}

/// `roll.toml` sets run settings in the tree, an environment's block on top, and a redeploy
/// (no build) keeps what the file set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roll_toml_sets_run_settings_per_environment() {
    let w = world().await;
    w.code.push("e5e5e5e5e5e5", "config", &[("roll.toml", "[app.run]\nconcurrency = 7\n[app.environments.production.run]\nmin_instances = 1\n")]);
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let d = settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!(d["status"], "active", "{d}");
    let up = |w: &World| w.fake.instances(&id).iter().filter(|(_, _, st)| *st == FakeState::Running).count();
    assert_eq!(up(&w), 1, "production's block keeps one up (the service's setting is 0)");
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{}/logs", d["id"].as_str().unwrap()), None).await;
    assert!(log.to_string().contains("roll.toml sets run.min_instances: 1 (production)"), "{log}");
    let (_, r) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/redeploy"), Some(json!({}))).await;
    assert_eq!(settled(&w, &id, "production", r["id"].as_str().unwrap()).await["status"], "active");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(up(&w), 1, "a redeploy keeps what roll.toml set");
}

/// A static build with a client-side router is a single-page app: a page path it has no file for
/// answers its index.html; an asset that's missing stays a 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_page_apps_deep_links_answer_its_index() {
    let w = world().await;
    w.code.push("e7e7e7e7e7e7", "spa", &[("package.json", r#"{"name":"site","scripts":{"build":"vite build"},"devDependencies":{"vite":"5.0.0"},"dependencies":{"react-router-dom":"6.0.0"}}"#)]);
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "spa", "source": {"kind": "repo", "repo": "acme/site"}, "variables": {"FAKE_MISSING": "/dashboard"} }))).await;
    let id = svc["id"].as_str().unwrap_or_else(|| panic!("{svc}")).to_string();
    let d = settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!((d["status"].as_str(), d["plan"]["spa"].as_bool()), (Some("active"), Some(true)), "{d}");
    let host = host(&svc["environments"][0]["url"]);
    // Its files answer, exported from the image: no instance, no cold start.
    let get = |path: &'static str, etag: Option<String>| {
        let (public, host) = (w.public.clone(), host.clone());
        async move {
            let mut req = Request::builder().uri(path).header("host", &host);
            if let Some(e) = etag {
                req = req.header("if-none-match", e);
            }
            let resp = public.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            let (st, h) = (resp.status(), resp.headers().clone());
            (st, h, String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    let (st, h, body) = get("/dashboard/settings", None).await;
    assert_eq!((st, body.as_str()), (StatusCode::OK, "<!doctype html><title>fake site</title>"), "a deep link is the app's index");
    assert_eq!(get("/", Some(h["etag"].to_str().unwrap().to_string())).await.0, StatusCode::NOT_MODIFIED);
    let (st, h, body) = get("/assets/app.js", None).await;
    assert_eq!((st, h["content-type"].to_str().unwrap(), body.as_str()), (StatusCode::OK, "text/javascript; charset=utf-8", "start()"));
    let (st, _, body) = get("/dashboard/logo.png", None).await;
    assert_eq!((st, body.as_str()), (StatusCode::NOT_FOUND, "<h1>not here</h1>"), "a missing file stays missing (the site's 404.html)");
    assert!(!get("/passwd", None).await.2.contains("root:"), "a symlink in the image is never followed");
    assert!(w.fake.instances(&id).is_empty(), "nothing ran: {:?}", w.fake.instances(&id));
    assert_eq!(call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await.1["state"], "running");
}

/// A deploy reports on its commit; an environment that waits for CI deploys a push only once its
/// checks pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploys_report_on_their_commit_and_can_wait_for_ci() {
    use apps_control::integrations::Ci;
    let w = world().await;
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    let id = svc["id"].as_str().unwrap_or_else(|| panic!("{svc}")).to_string();
    assert_eq!(settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await["status"], "active");
    // "success" follows the switch by a moment.
    for _ in 0..200 {
        if w.code.statuses.lock().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let posted: Vec<(String, String)> = w.code.statuses.lock().iter().map(|(_, c, s)| (c.clone(), s.clone())).collect();
    assert_eq!(posted, [("roll/apps/site/production".to_string(), "pending".to_string()), ("roll/apps/site/production".to_string(), "success".to_string())]);
    // Waiting for CI: a push whose checks are still running waits, then deploys once they pass.
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (_, env) = call(&w.api, "PUT", &base, Some(json!({"wait_for_ci": true}))).await;
    assert_eq!(env["wait_for_ci"], true, "{env}");
    *w.code.ci.lock() = Some(Ci::Pending);
    w.code.push("c1c1c1c1c1c1", "tested", &[("server.js", "v2")]);
    let env = get_until(&w.api, &base, |e| e["waiting_for_ci"] == "c1c1c1c1c1c1").await;
    assert_ne!(env["latest"]["status"], "queued", "nothing deploys while its checks run: {env}");
    *w.code.ci.lock() = Some(Ci::Passed);
    let env = get_until(&w.api, &base, |e| e["waiting_for_ci"].is_null() && e["active"]["revision"] == "c1c1c1c1c1c1").await;
    assert_eq!(env["active"]["revision"], "c1c1c1c1c1c1");
    // A push whose check failed doesn't deploy.
    *w.code.ci.lock() = Some(Ci::Failed("ci/test".into()));
    w.code.push("d2d2d2d2d2d2", "broken", &[("server.js", "v3")]);
    let (_, log) = (0, get_until(&w.api, &format!("{base}/logs"), |l| l.to_string().contains("its check ci/test failed")).await);
    assert!(log.to_string().contains("d2d2d2d2d2d2"));
    assert_eq!(call(&w.api, "GET", &base, None).await.1["active"]["revision"], "c1c1c1c1c1c1");
}

/// A volume is backed up to object storage and restored from it, the environment asleep
/// meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volume_is_backed_up_and_restored() {
    let w = world_full(|_| {}, true).await;
    let (svc, _) = image_service(&w, "db", json!({"type": "worker", "volume": {"mount": "/data", "size_gb": 1}})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let volume = apps_control::volume_name(&id, "production");
    w.fake.put_volume(&volume, b"the first state");
    let base = format!("/v1/apps/services/{id}/environments/production/volume/backups");
    let (st, b) = call(&w.api, "POST", &base, None).await;
    assert_eq!((st, b["size"].as_u64()), (StatusCode::CREATED, Some(15)), "{b}");
    w.fake.put_volume(&volume, b"what went wrong later");
    let (_, list) = call(&w.api, "GET", &base, None).await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    let (st, _) = call(&w.api, "POST", &format!("{base}/{}/restore", b["id"].as_str().unwrap()), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(w.fake.volume(&volume).as_deref(), Some(&b"the first state"[..]));
    let (_, trail) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert!(trail.to_string().contains("audit.volume.restored"), "{trail}");
    let (st, _) = call(&w.api, "POST", &format!("{base}/123/restore"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Changes people make are an audit trail: what changed and who, never a value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_are_an_audit_trail_without_values() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"API_KEY": "sk-s3cr3t"}}))).await;
    call(&w.api, "GET", &format!("{base}/variables/values"), None).await;
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"min_instances": 1}}))).await;
    let (st, trail) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert_eq!(st, StatusCode::OK, "{trail}");
    let kinds: Vec<&str> = trail.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["audit.settings", "audit.variables.revealed", "audit.variables"], "newest first");
    assert_eq!(trail[2]["data"]["set"], json!(["API_KEY"]));
    assert_eq!(trail[0]["data"]["changed"], json!(["run.min_instances"]));
    assert!(!trail.to_string().contains("sk-s3cr3t"), "a value never reaches the trail");
}

/// A project past its alert gets `spend.alert` once; past its hard limit it's capped: its
/// instances stop, a visitor gets a 402, deploys are refused; raising the limit lifts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_over_its_spend_limit_is_paused_until_the_limit_is_raised() {
    let w = world_with(|cfg| cfg.spend_every = Duration::from_millis(20)).await;
    let (svc, _) = image_service(&w, "shop", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let limits = format!("/v1/apps/projects/{}/limits", w.project);
    let (st, l) = call(&w.api, "PUT", &limits, Some(json!({"alert_usd": 1, "hard_usd": 5}))).await;
    assert_eq!((st, l["capped"].as_bool()), (StatusCode::OK, Some(false)), "{l}");
    // $3 of CPU this month: past the alert, under the limit.
    let hour = code_sql::now_ms() / 3_600_000;
    w.apps.store.add_usage(&id, "production", hour, &apps_control::store::UsageAdd { cpu_seconds: 250_000.0, ..Default::default() }).unwrap();
    let events = || async { call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1["events"].as_array().cloned().unwrap_or_default() };
    for _ in 0..200 {
        if events().await.iter().any(|e| e["kind"] == "spend.alert") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(events().await.iter().filter(|e| e["kind"] == "spend.alert").count(), 1, "once a month");
    assert!(!get_until(&w.api, &limits, |l| l["month_usd"].as_f64() > Some(2.9)).await["capped"].as_bool().unwrap());
    // Another $3: past the limit.
    w.apps.store.add_usage(&id, "production", hour, &apps_control::store::UsageAdd { cpu_seconds: 250_000.0, ..Default::default() }).unwrap();
    get_until(&w.api, &limits, |l| l["capped"] == true).await;
    let host = host(&svc["environments"][0]["url"]);
    let mut stopped = false;
    for _ in 0..200 {
        if !w.fake.instances(&id).iter().any(|(_, _, st)| *st == FakeState::Running) {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(stopped, "a capped project's instances stop: {:?}", w.fake.instances(&id));
    assert_eq!(visit(&w, &host, "/").await.0, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), Some(json!({}))).await.0, StatusCode::CONFLICT);
    // Raised: it answers again.
    call(&w.api, "PUT", &limits, Some(json!({"hard_usd": 50}))).await;
    get_until(&w.api, &limits, |l| l["capped"] == false).await;
    assert_eq!(visit(&w, &host, "/").await.0, StatusCode::OK);
    // Deleting the service doesn't take its spend off the month.
    let before = call(&w.api, "GET", &limits, None).await.1["month_usd"].as_f64().unwrap();
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert!(call(&w.api, "GET", &limits, None).await.1["month_usd"].as_f64().unwrap() >= before);
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
    // Staging's own scale: its next deployment keeps one up, production keeps the service's 0.
    let (st, env) = call(&w.api, "PUT", &format!("{base}/staging"), Some(json!({"run": {"min_instances": 1, "max_instances": 1}}))).await;
    assert_eq!((st, env["run"].clone(), env["needs_deploy"].clone()), (StatusCode::OK, json!({"min_instances": 1, "max_instances": 1}), json!(true)), "{env}");
    assert_eq!(call(&w.api, "PUT", &format!("{base}/staging"), Some(json!({"run": {"start": "sh"}}))).await.0, StatusCode::BAD_REQUEST, "what runs is the service's");
    assert_eq!(call(&w.api, "PUT", &format!("{base}/staging"), Some(json!({"run": {"min_instances": 9}}))).await.0, StatusCode::BAD_REQUEST, "checked against max_instances");
    let up = |w: &World, dep: &Value| w.fake.instances(&id).iter().filter(|(_, d, st)| d == dep && *st == FakeState::Running).count();
    let (_, s2) = call(&w.api, "POST", &format!("{base}/staging/deploys"), Some(json!({}))).await;
    let s2 = settled(&w, &id, "staging", s2["id"].as_str().unwrap()).await;
    let (_, p2) = call(&w.api, "POST", &format!("{base}/production/deploys"), Some(json!({}))).await;
    let p2 = settled(&w, &id, "production", p2["id"].as_str().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!((up(&w, &s2["id"]), up(&w, &p2["id"])), (1, 0), "staging keeps one up, production sleeps");
    let (_, env) = call(&w.api, "PUT", &format!("{base}/staging"), Some(json!({"run": {"min_instances": null, "max_instances": null}}))).await;
    assert_eq!(env["run"], json!({}), "null goes back to the service's");
}

/// Each request through the proxy is an access line (no query string), filterable by status and
/// duration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_are_access_lines_filtered_by_status() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let host = host(&svc["environments"][0]["url"]);
    assert_eq!(visit(&w, &host, "/ok?token=s3cr3t").await.0, StatusCode::OK);
    for (up, _, _) in w.fake.instances(&id).into_iter().filter(|(_, _, st)| *st == FakeState::Running) {
        w.fake.sicken(&up);
    }
    assert_eq!(visit(&w, &host, "/broken").await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let base = format!("/v1/apps/services/{id}/environments/production/logs?http=1");
    let all = get_until(&w.api, &base, |l| l.as_array().is_some_and(|a| a.len() >= 2)).await;
    let first: Value = serde_json::from_str(all[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((first["method"].as_str(), first["path"].as_str(), first["status"].as_u64()), (Some("GET"), Some("/ok"), Some(200)), "{first}");
    assert!(!all.to_string().contains("s3cr3t"), "a query string never reaches the log");
    let (_, errors) = call(&w.api, "GET", &format!("{base}&status=5xx"), None).await;
    assert_eq!(errors.as_array().unwrap().len(), 1, "{errors}");
    assert!(errors[0]["text"].as_str().unwrap().contains("/broken"));
    let (_, slow) = call(&w.api, "GET", &format!("{base}&min_ms=60000"), None).await;
    assert_eq!(slow, json!([]));
}

/// Five minutes of 5xx answers past the alert's share make one `alert.errors` event; more of the
/// same don't make another until it clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_5xx_rate_past_the_alert_is_an_event_once() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80, "alerts": {"error_pct": 10}})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    // Well inside a minute: the five written and the five judged are the same five.
    let into = code_sql::now_ms() % 60_000;
    if into > 50_000 {
        tokio::time::sleep(Duration::from_millis((60_000 - into + 100) as u64)).await;
    }
    let now = code_sql::now_ms() / 60_000;
    let minute = |m: i64, errors: i64| apps_control::store::MinuteRow { minute: m, requests: 100, errors, instances: 1, memory_bytes: 64 << 20, ..Default::default() };
    w.apps.store.put_minutes(&id, "production", &(1..=5).map(|i| minute(now - i, 20)).collect::<Vec<_>>()).unwrap();
    let live = w.apps.live(&id, "production");
    w.apps.judge_alerts(&live).await;
    w.apps.judge_alerts(&live).await;
    let (_, ev) = call(&w.api, "GET", "/v1/apps/events?after=0", None).await;
    let alerts: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "alert.errors").collect();
    assert_eq!(alerts.len(), 1, "{ev}");
    assert_eq!((alerts[0]["data"]["value"].as_i64(), alerts[0]["data"]["threshold"].as_i64()), (Some(20), Some(10)));
    // Memory stayed low and no latency alert is set: nothing else.
    assert!(!ev.to_string().contains("alert.memory") && !ev.to_string().contains("alert.latency"), "{ev}");
}

/// An instance the kernel killed for its memory says so: in the log and in why it crashed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instance_killed_for_its_memory_says_so() {
    let w = world().await;
    let (svc, _) = image_service(&w, "hungry", json!({"port": 80, "min_instances": 1, "memory_mb": 256})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let up = w.fake.instances(&id).into_iter().find(|(_, _, st)| *st == FakeState::Running).expect("min_instances 1").0;
    w.fake.oom(&up);
    let path = format!("/v1/apps/services/{id}/environments/production/logs");
    let log = get_until(&w.api, &path, |l| l.to_string().contains("ran out of memory")).await;
    assert!(log.to_string().contains("its limit is 256 MB"), "{log}");
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

/// A custom domain can redirect to another hostname of its environment (www → apex): a 308 with
/// the path and query; loops and chains are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn www_redirects_to_the_apex() {
    let w = world().await;
    let (svc, _) = image_service(&w, "www", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production/domains", svc["id"].as_str().unwrap());
    for h in ["www.example.org", "example.org"] {
        let (_, d) = call(&w.api, "POST", &base, Some(json!({ "hostname": h }))).await;
        w.dns.add(h, "CNAME", d["target"].as_str().unwrap());
        w.dns.add(&format!("_roll-challenge.{h}"), "TXT", d["token"].as_str().unwrap());
        assert_eq!(call(&w.api, "POST", &format!("{base}/{h}/verify"), None).await.1["status"], "active");
    }
    let (st, d) = call(&w.api, "PATCH", &format!("{base}/www.example.org"), Some(json!({"redirect_to": "example.org"}))).await;
    assert_eq!((st, d["redirect_to"].as_str()), (StatusCode::OK, Some("example.org")), "{d}");
    let req = Request::builder().uri("/pricing?plan=pro").header("host", "www.example.org").body(Body::empty()).unwrap();
    let r = w.public.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(r.headers()["location"], "http://example.org/pricing?plan=pro");
    assert_eq!(visit(&w, "example.org", "/").await.0, StatusCode::OK, "the apex serves");
    let (st, _) = call(&w.api, "PATCH", &format!("{base}/example.org"), Some(json!({"redirect_to": "www.example.org"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no loop");
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
    // A static upload: its files answer (no instance reads variables at run time).
    assert_eq!(visit(&w, &host(&env["url"]), "/").await.0, StatusCode::OK);

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
    assert!(!call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1.to_string().contains("cron.failed"));
    // A run that fails is an event (notified by default), with its exit code.
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_EXIT": "3"}}))).await;
    let (_, d) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &cid, "production", d["id"].as_str().unwrap()).await;
    call(&w.api, "POST", &format!("{base}/runs"), None).await;
    let ev = get_until(&w.api, "/v1/apps/events?after=0", |v| v.to_string().contains("cron.failed")).await;
    let failed = ev["events"].as_array().unwrap().iter().find(|e| e["kind"] == "cron.failed").unwrap();
    assert_eq!((failed["data"]["exit_code"].as_i64(), failed["data"]["status"].as_str()), (Some(3), Some("failed")), "{failed}");
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

/// A project's log drain gets its services' runtime output as signed NDJSON batches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_drain_gets_the_runtime_output_signed() {
    use hmac::Mac;
    let w = world().await;
    let got: Arc<parking_lot::Mutex<Vec<(axum::http::HeaderMap, bytes::Bytes)>>> = Default::default();
    let g = got.clone();
    let app = axum::Router::new().route("/logs", axum::routing::post(move |h: axum::http::HeaderMap, b: bytes::Bytes| {
        let g = g.clone();
        async move {
            g.lock().push((h, b));
            StatusCode::OK
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, app).into_future());
    let base = format!("/v1/apps/projects/{}/drains", w.project);
    let (st, d) = call(&w.api, "POST", &base, Some(json!({"url": format!("http://{at}/logs")}))).await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    let secret = d["secret"].as_str().unwrap().to_string();
    assert!(!call(&w.api, "GET", &base, None).await.1.to_string().contains("/logs"), "the URL never comes back");
    // A request wakes the service: its instance's output goes out.
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    visit(&w, &host(&svc["environments"][0]["url"]), "/").await;
    let mut lines = Vec::new();
    for _ in 0..200 {
        let batches = got.lock().clone();
        lines = batches.iter().flat_map(|(_, b)| String::from_utf8_lossy(b).lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).collect::<Vec<_>>()).collect();
        if lines.iter().any(|l| l["text"].as_str().is_some_and(|t| t.contains("listening on"))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let line = lines.iter().find(|l| l["text"].as_str().is_some_and(|t| t.contains("listening on"))).unwrap_or_else(|| panic!("{lines:?}"));
    assert_eq!((line["service_name"].as_str(), line["environment"].as_str()), (Some("api"), Some("production")));
    let (h, b) = got.lock()[0].clone();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(h["x-roll-timestamp"].as_bytes());
    mac.update(b".");
    mac.update(&b);
    let want = mac.finalize().into_bytes().iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert_eq!(h["x-roll-signature-256"].to_str().unwrap(), format!("sha256={want}"));
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
    assert_eq!(hook["events"], json!(["deploy.failed", "crash_loop", "spend.alert", "spend.limit", "alert.memory", "alert.errors", "alert.latency", "cron.failed"]), "the default events");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gradual_rollout_shares_requests_and_rolls_back_a_deployment_that_errors() {
    let w = world().await;
    let run = json!({"port": 80, "min_instances": 1, "rollout": {"steps": [50], "step_s": 2, "error_pct": 20}});
    // The first deployment has nothing to share requests with: it switches at once.
    let (svc, d1) = image_service(&w, "api", run).await;
    assert_eq!(d1["status"], "active", "{d1}");
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    let h = host(&env["url"]);
    // Traffic all along, as a live site has: what answered, by deployment.
    let traffic = |w: &World, stop: Arc<std::sync::atomic::AtomicBool>| {
        let (public, h) = (w.public.clone(), h.clone());
        tokio::spawn(async move {
            let mut seen: Vec<(u16, String)> = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let req = Request::builder().uri("/").header("host", &h).body(Body::empty()).unwrap();
                let resp = public.clone().oneshot(req).await.unwrap();
                let st = resp.status().as_u16();
                let b = resp.into_body().collect().await.unwrap().to_bytes();
                let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                seen.push((st, v["deployment"].as_str().unwrap_or("").to_string()));
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            seen
        })
    };

    // A good deployment takes half the requests for its step, then all of them.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let t = traffic(&w, stop.clone());
    let (_, d2) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    // A visitor keeps their deployment through the step: bucket 10 is under the 50% share, 90
    // isn't; a visitor without a bucket is given one.
    let canary = get_until(&w.api, &base, |e| e["canary"].is_object()).await;
    let new_id = canary["canary"]["deployment"].clone();
    let as_visitor = |cookie: Option<&'static str>| {
        let (public, h) = (w.public.clone(), h.clone());
        async move {
            let mut req = Request::builder().uri("/").header("host", &h);
            if let Some(c) = cookie {
                req = req.header("cookie", c);
            }
            let resp = public.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            let set = resp.headers().get("set-cookie").map(|v| v.to_str().unwrap().to_string());
            let b = resp.into_body().collect().await.unwrap().to_bytes();
            (serde_json::from_slice::<Value>(&b).unwrap()["deployment"].clone(), set)
        }
    };
    for _ in 0..5 {
        assert_eq!(as_visitor(Some("theme=dark; roll_bucket=10")).await, (new_id.clone(), None));
        assert_eq!(as_visitor(Some("roll_bucket=90")).await, (d1["id"].clone(), None));
    }
    assert!(as_visitor(None).await.1.is_some_and(|c| c.starts_with("roll_bucket=")), "a new visitor gets a bucket");
    let d2 = settled(&w, &id, "production", d2["id"].as_str().unwrap()).await;
    assert_eq!(d2["status"], "active", "{d2}");
    assert_eq!(new_id, d2["id"]);
    assert!(d2["steps"].as_array().unwrap().iter().any(|s| s["step"] == "canary_50"), "{d2}");
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let seen = t.await.unwrap();
    assert!(seen.iter().all(|(st, _)| *st == 200), "no request failed during the rollout");
    let old = seen.iter().filter(|(_, d)| *d == d1["id"]).count();
    let new = seen.iter().filter(|(_, d)| *d == d2["id"]).count();
    assert!(old > 20 && new > 20, "both answered during the step: {old} old, {new} new");
    for _ in 0..5 {
        let (st, v) = visit(&w, &h, "/").await;
        assert_eq!((st, v["deployment"].clone()), (StatusCode::OK, d2["id"].clone()));
    }

    // One that answers 5xx is rolled back mid-step; the old one never stopped serving.
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_UNHEALTHY": "1"}}))).await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let t = traffic(&w, stop.clone());
    let (_, d3) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d3 = settled(&w, &id, "production", d3["id"].as_str().unwrap()).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    t.await.unwrap();
    assert_eq!(d3["status"], "failed", "{d3}");
    assert!(d3["error"].as_str().unwrap().contains("rolled back at 50%"), "{d3}");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    assert_eq!(env["active"]["id"], d2["id"]);
    let (st, v) = visit(&w, &h, "/").await;
    assert_eq!((st, v["deployment"].clone()), (StatusCode::OK, d2["id"].clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(w.fake.instances(&id).iter().all(|(_, dep, _)| *dep == d2["id"]), "the canary's instances are gone");
}

/// The service page follows a service instead of polling it: a tick when someone changes it,
/// and when an instance wakes with nobody changing anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_services_changes_are_streamed_as_they_happen() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let req = Request::builder().uri(format!("/v1/apps/services/{id}/changes")).body(Body::empty()).unwrap();
    let resp = w.api.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    async fn next(body: &mut Body) -> String {
        loop {
            let f = body.frame().await.unwrap().unwrap();
            if let Some(e) = f.data_ref().and_then(|d| String::from_utf8_lossy(d).lines().find_map(|l| l.strip_prefix("event:").map(|e| e.trim().to_string()))) {
                return e;
            }
        }
    }
    assert_eq!(tokio::time::timeout(Duration::from_secs(3), next(&mut body)).await.unwrap(), "ready");
    let (st, _) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"concurrency": 10}}))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(tokio::time::timeout(Duration::from_secs(3), next(&mut body)).await.expect("a tick for the change"), "changed");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    assert_eq!(visit(&w, &host(&env["url"]), "/").await.0, StatusCode::OK);
    assert_eq!(tokio::time::timeout(Duration::from_secs(3), next(&mut body)).await.expect("a tick for the wake"), "changed");
}

/// A deploy hook deploys its environment with nothing but its URL, only the service's own image
/// repository, and stops working when removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_hook_deploys_with_its_url_alone() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let hooks = w.api.clone();
    let (st, h) = call(&w.api, "POST", &format!("{base}/hook"), None).await;
    assert_eq!(st, StatusCode::CREATED, "{h}");
    let path = h["path"].as_str().unwrap().to_string();
    assert_eq!(call(&w.api, "GET", &base, None).await.1["hook"], true);
    // One character off (never the same one: `x` is in the alphabet too).
    let wrong = format!("{}{}", &path[..path.len() - 1], if path.ends_with('x') { 'y' } else { 'x' });
    assert_eq!(call(&hooks, "POST", &wrong, None).await.0, StatusCode::NOT_FOUND, "a wrong token is a 404 like any other");
    let (st, d) = call(&hooks, "POST", &format!("{path}?image=nginx:1.28"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let d = settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    assert_eq!((d["status"].as_str(), d["actor"].as_str()), (Some("active"), Some("deploy hook")), "{d}");
    assert!(d["image"].as_str().unwrap().starts_with("nginx:1.28"), "{d}");
    assert_eq!(call(&hooks, "POST", &format!("{path}?image=evil/miner:1"), None).await.0, StatusCode::BAD_REQUEST, "only its own repository");
    assert!(path.rsplit('/').next().unwrap().starts_with("rlk_"), "a secret scanners know: {path}");
    let (_, activity) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert!(activity.as_array().unwrap().iter().any(|e| e["kind"] == "audit.hook.deployed" && e["data"]["deployment"] == d["id"]), "{activity}");
    assert_eq!(call(&w.api, "DELETE", &format!("{base}/hook"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&hooks, "POST", &path, None).await.0, StatusCode::NOT_FOUND, "removed, it's gone");
}

/// An environment that follows its image's tag deploys when the tag moves, once, and not while
/// it stays put.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_followed_tag_that_moves_deploys_once() {
    let w = world().await;
    let (svc, d1) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"follow_tag": true}))).await;
    assert_eq!((st, env["follow_tag"].clone()), (StatusCode::OK, json!(true)), "{env}");
    assert!(w.apps.watch_tags().await.is_empty(), "the tag still points at what runs");
    let moved = format!("sha256:{}", "1".repeat(64));
    w.fake.move_tag("nginx:1.27", &moved);
    let started = w.apps.watch_tags().await;
    assert_eq!(started.len(), 1, "one deploy for the move");
    let d2 = settled(&w, &id, "production", &started[0]).await;
    assert_eq!((d2["status"].as_str(), d2["actor"].as_str()), (Some("active"), Some("tag watch")), "{d2}");
    assert!(d2["image"].as_str().unwrap().ends_with(&moved) && d2["id"] != d1["id"], "{d2}");
    assert!(w.apps.watch_tags().await.is_empty(), "deployed: nothing more to do");
    // A rollback while following isn't undone: the tag hasn't moved since.
    let (_, r) = call(&w.api, "POST", &format!("{base}/deploys/{}/rollback", d1["id"].as_str().unwrap()), None).await;
    assert_eq!(settled(&w, &id, "production", r["id"].as_str().unwrap()).await["status"], "active");
    assert!(w.apps.watch_tags().await.is_empty(), "the rollback stays");
}

/// A deployment has its own URL: one deployed without traffic is reached there, not on the
/// environment's, until a rollback to it puts it live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deployment_without_traffic_answers_on_its_own_url_until_put_live() {
    let w = world().await;
    let (svc, d1) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    let h = host(&env["url"]);
    let own = |dep: &Value| format!("{}--{h}", dep.as_str().unwrap().trim_start_matches("dep_"));
    let (st, d2) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({"no_traffic": true}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d2}");
    let d2 = settled(&w, &id, "production", d2["id"].as_str().unwrap()).await;
    assert_eq!(d2["status"], "ready", "{d2}");
    assert_eq!(call(&w.api, "GET", &base, None).await.1["active"]["id"], d1["id"], "nothing switched");
    assert_eq!(visit(&w, &h, "/").await.1["deployment"], d1["id"]);
    let (st, v) = visit(&w, &own(&d2["id"]), "/").await;
    assert_eq!((st, v["deployment"].clone()), (StatusCode::OK, d2["id"].clone()), "its own URL runs it");
    assert_eq!(visit(&w, &own(&d1["id"]), "/").await.1["deployment"], d1["id"], "the live one has its own URL too");
    assert_eq!(visit(&w, &format!("aaaaaaaaaaaa--{h}"), "/").await.0, StatusCode::NOT_FOUND);
    // No environment's hostname can look like a deployment's: slugs never hold `--`.
    let (other, _) = image_service(&w, "abcdefghijkl--victim", json!({"port": 80})).await;
    let (_, oenv) = call(&w.api, "GET", &format!("/v1/apps/services/{}/environments/production", other["id"].as_str().unwrap()), None).await;
    assert!(!oenv["url"].as_str().unwrap_or_else(|| panic!("{oenv}")).contains("--"), "{oenv}");
    // Put live: its image again, no build.
    let (_, d3) = call(&w.api, "POST", &format!("{base}/deploys/{}/rollback", d2["id"].as_str().unwrap()), None).await;
    let d3 = settled(&w, &id, "production", d3["id"].as_str().unwrap()).await;
    assert_eq!((d3["status"].as_str(), &d3["image"]), (Some("active"), &d2["image"]), "{d3}");
    // Live a moment after it reads as live (the switch, then the instances).
    let mut served = Value::Null;
    for _ in 0..100 {
        served = visit(&w, &h, "/").await.1["deployment"].clone();
        if served == d3["id"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(served, d3["id"]);
}

/// An h2c app (a gRPC server) is spoken HTTP/2 to, with `te: trailers`, and its trailers reach
/// the client; an HTTP/1 app still gets HTTP/1.1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_h2c_app_gets_http2_and_its_trailers_pass() {
    let w = world().await;
    let (svc, _) = image_service(&w, "grpc", json!({"port": 80, "h2c": true})).await;
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{}/environments/production", svc["id"].as_str().unwrap()), None).await;
    let h = host(&env["url"]);
    let req = Request::builder().uri("/").header("host", &h).header("te", "trailers").body(Body::empty()).unwrap();
    let v: Value = serde_json::from_slice(&w.public.clone().oneshot(req).await.unwrap().into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!((v["version"].as_str(), v["te"].as_str()), (Some("HTTP/2.0"), Some("trailers")), "{v}");
    let req = Request::builder().uri("/trailers").header("host", &h).body(Body::empty()).unwrap();
    let got = w.public.clone().oneshot(req).await.unwrap().into_body().collect().await.unwrap();
    assert_eq!(got.trailers().and_then(|t| t.get("grpc-status")).map(|v| v.to_str().unwrap()), Some("0"), "the status trailer reached the client");
    let (plain, _) = image_service(&w, "web", json!({"port": 80})).await;
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{}/environments/production", plain["id"].as_str().unwrap()), None).await;
    assert_eq!(visit(&w, &host(&env["url"]), "/").await.1["version"], "HTTP/1.1");
}

/// A preview roll previews turned feedback on for: its pages carry the button for a visitor who
/// came in through its link, and the script comes from the preview's own host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_previews_pages_carry_the_feedback_button_for_its_link_visitors() {
    let w = world_with(|c| c.web_url = Some("https://roll.test".into())).await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let token = "t".repeat(32);
    assert_eq!(call(&w.api, "PUT", &format!("/v1/apps/services/{id}/environments/production"), Some(json!({"feedback": true}))).await.0, StatusCode::BAD_REQUEST, "previews only");
    let base = format!("/v1/apps/services/{id}/environments/pv-abc");
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"ephemeral": true, "access": {"token": token}, "feedback": true}))).await;
    assert_eq!((st, env["feedback"].clone()), (StatusCode::OK, json!(true)), "{env}");
    let (_, d) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    settled(&w, &id, "pv-abc", d["id"].as_str().unwrap()).await;
    let (_, env) = call(&w.api, "GET", &base, None).await;
    let h = host(&env["url"]);
    let get = |path: &'static str, accept: &'static str| {
        let (public, h, token) = (w.public.clone(), h.clone(), token.clone());
        async move {
            let req = Request::builder().uri(path).header("host", &h).header("accept", accept).header("cookie", format!("roll_access_pv-abc={token}")).body(Body::empty()).unwrap();
            let resp = public.oneshot(req).await.unwrap();
            (resp.status(), String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    let (st, page) = get("/page", "text/html").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.ends_with(r#"<script src="/__roll/feedback.js" defer></script></body></html>"#), "{page}");
    let (st, js) = get("/__roll/feedback.js", "*/*").await;
    assert_eq!(st, StatusCode::OK);
    assert!(js.contains("https://roll.test/dashboard/projects/") && js.contains("previews?preview=abc"), "the note goes to the preview in roll");
    assert!(!get("/", "application/json").await.1.contains("feedback.js"), "an API call isn't a page");
}

/// A worker that follows its work: its instances follow a stream group's lag within its bounds,
/// scaling in one at a time once fewer are wanted; streams by id, and only for a worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_follows_its_streams_backlog() {
    let w = world_with(|c| c.live.scale_down_after = Duration::ZERO).await;
    let ws = w.apps.deps.workspace.clone().unwrap();
    ws.place(roll_workspace::Scope::All, &w.project, roll_workspace::Kind::Stream, "st_ag2345678abc", 0.0, 0.0).unwrap();
    let run = json!({"type": "worker", "min_instances": 1, "max_instances": 4, "scale": {"stream": "st_ag2345678abc", "group": "billing", "per_instance": 10}});
    let (svc, d) = image_service(&w, "billing", run).await;
    assert_eq!(d["status"], "active", "{d}");
    let id = svc["id"].as_str().unwrap().to_string();
    let live = w.apps.live(&id, "production");
    let up = |w: &World| w.fake.instances(&id).iter().filter(|(_, _, st)| *st == FakeState::Running).count();
    BACKLOG.store(35, Ordering::SeqCst);
    // The world's own reaper reads the backlog too: wait for the outcome, not one pass.
    for _ in 0..100 {
        w.apps.scale_workers().await;
        live.tick().await;
        if up(&w) == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(up(&w), 4, "35 behind at 10 each: 4 (the ceiling)");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    assert_eq!(env["scaling"], json!({"backlog": 35, "wanted": 4}), "{env}");
    BACKLOG.store(0, Ordering::SeqCst);
    w.apps.scale_workers().await;
    live.tick().await;
    live.tick().await;
    // Its stop runs off the wake lock, the drain to finish its work.
    for _ in 0..200 {
        if up(&w) == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(up(&w), 3, "caught up: one goes at a time");
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"scale": {"stream": "orders", "group": "g"}}}))).await.0, StatusCode::BAD_REQUEST, "a stream by its id, never a name");
    // Becoming a web service drops it (a web service scales on its requests).
    let (st, s) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"type": "web"}}))).await;
    assert_eq!((st, s["run"].get("scale")), (StatusCode::OK, None), "{s}");
}


/// A private service answers no public name (the same 404 as an unknown address) and has no
/// URL, at once; public again, it answers again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_service_has_no_public_url() {
    let w = world().await;
    let (svc, _) = image_service(&w, "admin", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let h = host(&call(&w.api, "GET", &base, None).await.1["url"]);
    assert_eq!(visit(&w, &h, "/").await.0, StatusCode::OK);
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"public": false}}))).await.0, StatusCode::OK);
    assert_eq!(visit(&w, &h, "/").await.0, StatusCode::NOT_FOUND, "no deploy needed");
    assert!(call(&w.api, "GET", &base, None).await.1["url"].is_null());
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"public": true}}))).await;
    assert_eq!(visit(&w, &h, "/").await.0, StatusCode::OK);
}

/// A rollback brings back the variables of the deployment it returns to (in the activity), or
/// keeps today's when asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rollback_restores_its_deployments_variables() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "api", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": {"MODE": "old"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let d1 = settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let base = format!("/v1/apps/services/{id}/environments/production");
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"MODE": "broken", "EXTRA": "1"}}))).await;
    let (_, d2) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d2 = settled(&w, &id, "production", d2["id"].as_str().unwrap()).await;
    let vals = |w: &World| {
        let (api, base) = (w.api.clone(), base.clone());
        async move { call(&api, "GET", &format!("{base}/variables/values"), None).await.1 }
    };
    let (_, r) = call(&w.api, "POST", &format!("{base}/deploys/{}/rollback", d1["id"].as_str().unwrap()), None).await;
    assert_eq!(settled(&w, &id, "production", r["id"].as_str().unwrap()).await["status"], "active");
    let v = vals(&w).await;
    assert_eq!((v["MODE"].as_str(), v.get("EXTRA")), (Some("old"), None), "{v}");
    let (_, act) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert!(act.as_array().unwrap().iter().any(|e| e["kind"] == "audit.variables.restored"), "{act}");
    let (_, r) = call(&w.api, "POST", &format!("{base}/deploys/{}/rollback", d2["id"].as_str().unwrap()), Some(json!({"keep_variables": true}))).await;
    assert_eq!(settled(&w, &id, "production", r["id"].as_str().unwrap()).await["status"], "active");
    assert_eq!(vals(&w).await["MODE"], "old", "kept");
}

/// "Roll back" with no deployment walks back in time, through earlier rollbacks too, and never
/// forward to what it just left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rolling_back_again_goes_further_back() {
    let w = world().await;
    let (svc, d1) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let mut deploy = Vec::new();
    for _ in 0..2 {
        let (_, d) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
        deploy.push(settled(&w, &id, "production", d["id"].as_str().unwrap()).await);
    }
    let back = |w: &World| {
        let (api, base) = (w.api.clone(), base.clone());
        async move { call(&api, "POST", &format!("{base}/rollback"), None).await }
    };
    let (st, r1) = back(&w).await;
    assert_eq!(st, StatusCode::ACCEPTED);
    assert_eq!(r1["rollback_of"], deploy[0]["id"], "from d3 to d2");
    settled(&w, &id, "production", r1["id"].as_str().unwrap()).await;
    let (_, r2) = back(&w).await;
    assert_eq!(r2["rollback_of"], d1["id"], "then to d1, not forward to d3");
    settled(&w, &id, "production", r2["id"].as_str().unwrap()).await;
    assert_eq!(back(&w).await.0, StatusCode::CONFLICT, "nothing before d1");
}

/// A worker that keeps crashing is restarted each time (backing off), and three crashes in ten
/// minutes are a crash loop: on the environment with why, and one event, sent to the project's
/// targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashing_worker_restarts_and_its_crash_loop_is_one_event() {
    let w = world_with(|c| c.live.restart_backoff = Duration::from_millis(5)).await;
    let got: Arc<Mutex<Vec<Value>>> = Default::default();
    let g = got.clone();
    let hook = axum::Router::new().route("/hook", axum::routing::post(move |b: bytes::Bytes| {
        let g = g.clone();
        async move {
            g.lock().push(serde_json::from_slice(&b).unwrap());
            StatusCode::OK
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, hook).into_future());
    call(&w.api, "POST", &format!("/v1/apps/projects/{}/notifications", w.project), Some(json!({"url": format!("http://{at}/hook")}))).await;
    let (svc, d) = image_service(&w, "billing", json!({"type": "worker", "min_instances": 1})).await;
    assert_eq!(d["status"], "active", "{d}");
    let id = svc["id"].as_str().unwrap().to_string();
    let live = w.apps.live(&id, "production");
    let running = |w: &World| w.fake.instances(&id).into_iter().filter(|(_, _, st)| *st == FakeState::Running).map(|(n, _, _)| n).collect::<Vec<_>>();
    for n in 0..3 {
        let up = running(&w);
        assert_eq!(up.len(), 1, "one running worker: {:?}", w.fake.instances(&id));
        let name = up[0].clone();
        w.fake.crash(&name);
        let mut back = false;
        for _ in 0..200 {
            live.tick().await;
            if running(&w).len() == 1 {
                back = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(back, "crash {n}: restarted");
    }
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    assert_eq!(env["crash_loop"]["crashes"], 3, "{env}");
    assert!(env["crash_loop"]["last_error"].as_str().unwrap().contains("137"), "{env}");
    let mut sent = Vec::new();
    for _ in 0..100 {
        sent = got.lock().iter().filter(|b| b["type"] == "crash_loop").cloned().collect();
        if !sent.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["service"].as_str(), Some(id.as_str()));
    let events = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    assert_eq!(events["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "crash_loop").count(), 1, "once, however long it lasts: {events}");
}

/// A crowd at a sleeping service wakes one instance; those waiting on a start slower than the
/// wake timeout get a 503 page that says when to try again, and the retry gets the app.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crowd_wakes_one_instance_and_a_slow_start_says_when_to_retry() {
    let w = world_with(|c| c.live.wake_timeout = Duration::from_millis(300)).await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "slow", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": {"FAKE_BOOT_MS": "900"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await["status"], "active");
    let h = host(&svc["environments"][0]["url"]);
    let get = || async {
        let req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
        let resp = w.public.clone().oneshot(req).await.unwrap();
        (resp.status(), resp.headers().get("retry-after").map(|v| v.to_str().unwrap().to_string()))
    };
    let created = Fake::count(&w.fake.counts.creates);
    let crowd = futures::future::join_all((0..8).map(|_| get())).await;
    // The one that woke it waits for its start; those behind it give up at the timeout.
    let waited = crowd.iter().filter(|(st, retry)| *st == StatusCode::SERVICE_UNAVAILABLE && retry.is_some()).count();
    assert!(waited >= 7 && crowd.iter().all(|(st, _)| *st == StatusCode::OK || *st == StatusCode::SERVICE_UNAVAILABLE), "{crowd:?}");
    let mut ok = None;
    for _ in 0..50 {
        let (st, _) = get().await;
        if st == StatusCode::OK {
            ok = Some(st);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(ok, Some(StatusCode::OK), "a retry gets the app once it has started");
    assert!(Fake::count(&w.fake.counts.creates) - created <= 1, "one instance woke for all of them");
    assert_eq!(w.fake.instances(&id).iter().filter(|(_, _, st)| *st == FakeState::Running).count(), 1);
}

/// A cron service runs when its schedule comes round, and a run still going when the next is
/// due makes that one skipped, not a second copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cron_fires_on_its_schedule_and_skips_while_the_last_run_goes() {
    let w = world().await;
    // No exit: each run keeps going.
    let (_, cron) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "every-minute", "source": {"kind": "image", "image": "busybox"}, "run": {"type": "cron", "schedule": "* * * * *"} }))).await;
    let cid = cron["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &cid, "production", cron["environments"][0]["latest"]["id"].as_str().unwrap()).await["status"], "active");
    let runs = format!("/v1/apps/services/{cid}/environments/production/runs");
    // Minutes from now on: the reaper's own passes, at the real time, find nothing due.
    let t = code_sql::now_ms() + 3_600_000;
    w.apps.cron_pass(t).await;
    assert_eq!(call(&w.api, "GET", &runs, None).await.1, json!([]), "the first pass only starts the clock");
    w.apps.cron_pass(t + 61_000).await;
    let first = get_until(&w.api, &runs, |r| r[0]["status"] == "running").await;
    assert_eq!(first.as_array().unwrap().len(), 1, "{first}");
    w.apps.cron_pass(t + 122_000).await;
    let both = get_until(&w.api, &runs, |r| r.as_array().is_some_and(|a| a.len() == 2)).await;
    assert_eq!((both[0]["status"].as_str(), both[1]["status"].as_str()), (Some("skipped"), Some("running")), "{both}");
    w.apps.cron_pass(t + 122_500).await;
    assert_eq!(call(&w.api, "GET", &runs, None).await.1.as_array().unwrap().len(), 2, "once per due time");
}


/// The project's other services reach a sleeping service: a caller waiting at a stopped
/// instance starts it, one at a frozen instance resumes it, and while they're connected it
/// stays awake though rolld sees no request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_caller_wakes_a_sleeping_service_and_keeps_it_awake() {
    use apps_control::runtime::Callers;
    let w = world().await;
    let (svc, _) = image_service(&w, "internal", json!({"port": 80, "public": false})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let state = || async { call(&w.api, "GET", &base, None).await.1["state"].as_str().unwrap_or("").to_string() };
    let until = |want: &'static str| async move {
        for _ in 0..100 {
            if state().await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never {want}: {}", state().await);
    };
    assert_eq!(state().await, "stopped");
    let name = w.fake.instances(&id)[0].0.clone();
    w.fake.set_callers(&name, Callers { waiting: true, private: 1 });
    until("running").await;
    // Connected: in use, however long since rolld's last request.
    w.fake.set_callers(&name, Callers { waiting: false, private: 1 });
    let live = w.apps.live(&id, "production");
    live.backdate(Duration::from_secs(31));
    live.answer_private(true).await;
    live.tick().await;
    assert_eq!(state().await, "running");
    // Gone: it sleeps, frozen; the next caller thaws it.
    w.fake.set_callers(&name, Callers::default());
    live.backdate(Duration::from_secs(31));
    live.tick().await;
    assert_eq!(state().await, "suspended");
    let resumes = Fake::count(&w.fake.counts.resumes);
    w.fake.set_callers(&name, Callers { waiting: true, private: 0 });
    until("running").await;
    assert_eq!(Fake::count(&w.fake.counts.resumes), resumes + 1, "resumed, not started again");
}

/// A service link gives another web service's URLs, named for it: private on the project
/// network, public when it's public. Only another web service of the project links, and its
/// links go with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_link_gives_another_services_urls() {
    let w = world().await;
    let (api, _) = image_service(&w, "api", json!({"port": 3000})).await;
    let (web, _) = image_service(&w, "web", json!({"port": 80})).await;
    let (worker, _) = image_service(&w, "jobs", json!({"type": "worker"})).await;
    let (aid, wid) = (api["id"].as_str().unwrap().to_string(), web["id"].as_str().unwrap().to_string());
    let links = format!("/v1/apps/services/{wid}/links");
    let (st, l) = call(&w.api, "PUT", &format!("{links}/service/{aid}"), Some(json!({}))).await;
    assert_eq!((st, &l["vars"]), (StatusCode::OK, &json!(["API_URL", "API_PUBLIC_URL"])), "{l}");
    assert_eq!(call(&w.api, "PUT", &format!("{links}/service/{wid}"), Some(json!({}))).await.0, StatusCode::BAD_REQUEST, "not itself");
    assert_eq!(call(&w.api, "PUT", &format!("{links}/service/{}", worker["id"].as_str().unwrap()), Some(json!({}))).await.0, StatusCode::BAD_REQUEST, "a worker has no URL");
    let base = format!("/v1/apps/services/{wid}/environments/production");
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_ECHO": "API_URL,API_PUBLIC_URL"}}))).await;
    let (_, r) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &wid, "production", r["id"].as_str().unwrap()).await;
    let (_, body) = visit(&w, &host(&web["environments"][0]["url"]), "/").await;
    assert_eq!(body["vars"]["API_URL"], "http://api.production.internal:3000", "{body}");
    assert_eq!(body["vars"]["API_PUBLIC_URL"], api["environments"][0]["url"], "{body}");
    // Private, it has no public URL to give.
    call(&w.api, "PATCH", &format!("/v1/apps/services/{aid}"), Some(json!({"run": {"public": false}}))).await;
    assert_eq!(call(&w.api, "GET", &links, None).await.1[0]["vars"], json!(["API_URL"]));
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{aid}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&w.api, "GET", &links, None).await.1, json!([]), "gone with the service");
}


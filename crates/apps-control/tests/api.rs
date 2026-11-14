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
    /// It can issue a wildcard's certificate (DNS-01).
    dns01: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl apps_control::integrations::DomainTls for FakeTls {
    fn wildcards(&self) -> bool {
        self.dns01.load(Ordering::SeqCst)
    }
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
        // A provider takes a moment: two requests at once both get here.
        tokio::time::sleep(Duration::from_millis(30)).await;
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
    world_full(setup, false, |_| {}).await
}

/// `objects`: a directory as object storage (log chunks, volume backups).
async fn world_full(setup: impl FnOnce(&mut AppsConfig), objects: bool, deps_setup: impl FnOnce(&mut Deps)) -> World {
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
    cfg.supersede_every = Duration::from_millis(50);
    cfg.allow_private = true;
    setup(&mut cfg);
    let mut deps = Deps {
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
    deps_setup(&mut deps);
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

/// Visit until `dep` answers: a deployment reads as active at its switch, and its instances
/// take the requests a moment after (about 4 s at most).
async fn visit_deployment(w: &World, host: &str, dep: &str) -> (StatusCode, Value) {
    let mut last = (StatusCode::NOT_FOUND, Value::Null);
    for _ in 0..200 {
        last = visit(w, host, "/").await;
        if last.1["deployment"] == dep {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    last
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
    // Its cache mounts are its own (another service's build can't name them, and the log
    // doesn't), until a deploy without the cache starts them afresh.
    let ns = |df: &str| df.split(",id=").skip(1).find_map(|r| r.split_once('-').filter(|(n, _)| n.len() == 16).map(|(n, _)| n.to_string()));
    let (no_cache, df) = w.fake.built.lock().last().cloned().unwrap();
    let first_ns = ns(&df).unwrap_or_else(|| panic!("{df}"));
    assert!(!no_cache && !log.to_string().contains(&first_ns) && df.contains("ARG ROLL_LAYERS"), "{df}");
    let base = format!("/v1/apps/services/{id}/environments/production/deploys");
    let (_, d) = call(&w.api, "POST", &base, Some(json!({"no_cache": true}))).await;
    settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    let (no_cache, df) = w.fake.built.lock().last().cloned().unwrap();
    assert!(no_cache && ns(&df).is_some_and(|n| n != first_ns), "{df}");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"no_cache": true, "image": "nginx:1.27"}))).await.0, StatusCode::BAD_REQUEST, "an image has no build");
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

/// A GET that finds its instance gone (killed: nothing listens, and roll doesn't know yet) goes
/// to a fresh instance instead of a 503; a request with a body isn't sent twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_get_to_a_vanished_instance_goes_to_a_fresh_one() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80})).await;
    let h = host(&svc["environments"][0]["url"]);
    let (st, body) = visit(&w, &h, "/").await;
    assert_eq!(st, StatusCode::OK);
    let first = body["instance"].as_str().unwrap().to_string();
    w.fake.crash(&first);
    let (st, body) = visit(&w, &h, "/").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let second = body["instance"].as_str().unwrap().to_string();
    assert_ne!(second, first);
    w.fake.crash(&second);
    let made = w.fake.instances(svc["id"].as_str().unwrap()).len();
    let post = Request::post("/").header("host", h.as_str()).header("content-length", "1").body(Body::from("x")).unwrap();
    let st = w.public.clone().oneshot(post).await.unwrap().status();
    assert!(matches!(st, StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY), "{st}");
    assert_eq!(w.fake.instances(svc["id"].as_str().unwrap()).len(), made, "not sent again");
}

/// A newer push stops a push's build that's still running (superseded, its minutes saved); the
/// newer one goes live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_push_stops_the_build_of_an_older_one() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let pkg = ("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#);
    let builds = Fake::count(&w.fake.counts.builds);
    w.code.push("b2b2b2b2b2b2", "slow", &[pkg, ("SLOW_BUILD", "30000")]);
    let deploys = |w: &World| {
        let (api, id) = (w.api.clone(), id.clone());
        async move { call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys"), None).await.1 }
    };
    let by_rev = |all: &Value, rev: &str| all.as_array().unwrap().iter().find(|d| d["revision"] == rev).cloned();
    // Its build has begun (its tree read: the fake's checkout is whatever was pushed last).
    for _ in 0..500 {
        if by_rev(&deploys(&w).await, "b2b2b2b2b2b2").is_some_and(|d| d["status"] == "building") && Fake::count(&w.fake.counts.builds) > builds {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let began = std::time::Instant::now();
    w.code.push("c3c3c3c3c3c3", "fast", &[pkg, ("SLOW_BUILD", "0")]);
    let mut all = Value::Null;
    for _ in 0..1000 {
        all = deploys(&w).await;
        if by_rev(&all, "c3c3c3c3c3c3").is_some_and(|d| d["status"] == "active") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (slow, fast) = (by_rev(&all, "b2b2b2b2b2b2").unwrap(), by_rev(&all, "c3c3c3c3c3c3").unwrap());
    assert_eq!((slow["status"].as_str(), slow["error"].as_str(), fast["status"].as_str()), (Some("superseded"), Some("a newer push replaced it"), Some("active")), "{all}");
    assert!(began.elapsed() < Duration::from_secs(10), "the slow build didn't run to its end");
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
    let (st, body) = visit_deployment(&w, &host(&svc["environments"][0]["url"]), r["id"].as_str().unwrap()).await;
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
    // Two requests making one link at once: one credential stays, the other is revoked.
    let (racer, _) = image_service(&w, "racer", json!({"port": 80})).await;
    let rid = racer["id"].as_str().unwrap().to_string();
    let path = format!("/v1/apps/services/{rid}/links/redis/cache");
    let before = MINTED.lock().len();
    let (a, b) = tokio::join!(call(&w.api, "PUT", &path, Some(json!({}))), call(&w.api, "PUT", &path, Some(json!({}))));
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    let log: Vec<String> = MINTED.lock()[before..].to_vec();
    assert_eq!(log.iter().filter(|l| l.starts_with("link ")).count(), 2, "{log:?}");
    assert_eq!(log.iter().filter(|l| l.starts_with("unlink ")).count(), 1, "the loser's goes: {log:?}");
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
    let w = world_full(|_| {}, true, |_| {}).await;
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
    // A static upload: its files answer (no instance reads variables at run time), with its
    // own `_redirects` and `_headers`, which aren't served themselves.
    let h = host(&env["url"]);
    assert_eq!(visit(&w, &h, "/").await.0, StatusCode::OK);
    let site = |path: &str| {
        let req = Request::builder().uri(path).header("host", h.as_str()).body(Body::empty()).unwrap();
        w.public.clone().oneshot(req)
    };
    let r = site("/old?v=2").await.unwrap();
    assert_eq!((r.status(), r.headers()["location"].to_str().unwrap()), (StatusCode::PERMANENT_REDIRECT, "/assets/app.js?v=2"));
    let r = site("/assets/app.js").await.unwrap();
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(site("/app/settings").await.unwrap().status(), StatusCode::OK, "a rewrite to its index");
    assert_eq!(site("/_redirects").await.unwrap().status(), StatusCode::NOT_FOUND);
    // Compressed for a client that takes it: gzipped here, or the build's own `.br`.
    let get = |path: &str, ae: Option<&str>| {
        let mut b = Request::builder().uri(path).header("host", h.as_str());
        if let Some(ae) = ae {
            b = b.header("accept-encoding", ae);
        }
        let (public, req) = (w.public.clone(), b.body(Body::empty()).unwrap());
        async move {
            let r = public.oneshot(req).await.unwrap();
            let hd = r.headers().clone();
            (hd, r.into_body().collect().await.unwrap().to_bytes())
        }
    };
    let (hd, raw) = get("/assets/big.js", None).await;
    assert!(hd.get("content-encoding").is_none() && hd["vary"] == "Accept-Encoding" && raw.len() == 21 * 200, "{hd:?}");
    let (hz, gz) = get("/assets/big.js", Some("br;q=0, gzip, deflate")).await;
    assert_eq!(hz["content-encoding"], "gzip");
    assert!(gz.len() < raw.len() / 5 && hz["etag"] != hd["etag"], "{} bytes", gz.len());
    let mut plain = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut plain).unwrap();
    assert_eq!(plain, raw);
    // One byte range of the file as it is (a video's seek); a stale If-Range gets it whole.
    let ranged = |range: &str, if_range: Option<&str>| {
        let mut b = Request::builder().uri("/assets/big.js").header("host", h.as_str()).header("range", range);
        if let Some(v) = if_range {
            b = b.header("if-range", v);
        }
        let (public, req) = (w.public.clone(), b.body(Body::empty()).unwrap());
        async move {
            let r = public.oneshot(req).await.unwrap();
            let (st, hd) = (r.status(), r.headers().clone());
            (st, hd, r.into_body().collect().await.unwrap().to_bytes())
        }
    };
    let (st, hr, part) = ranged("bytes=21-41", Some(hd["etag"].to_str().unwrap())).await;
    assert_eq!((st, hr["content-range"].to_str().unwrap(), &part[..]), (StatusCode::PARTIAL_CONTENT, "bytes 21-41/4200", &raw[21..42]));
    assert_eq!(ranged("bytes=0-1", Some("\"stale\"")).await.0, StatusCode::OK);
    assert_eq!(ranged("bytes=5000-", None).await.0, StatusCode::RANGE_NOT_SATISFIABLE);
    let (hb, br) = get("/assets/pre.css", Some("gzip, br")).await;
    assert_eq!((hb["content-encoding"].to_str().unwrap(), &br[..]), ("br", &b"BROTLI-BYTES"[..]));
    assert!(get("/index.html", Some("gzip")).await.0.get("content-encoding").is_none(), "too small to bother");
    // A page loaded before the next deploy still gets its old chunks (skew protection); the new
    // site has its own, and a fingerprinted name no deployment had is still missing.
    let chunk = |image: &Value| {
        let h = image.as_str().unwrap().bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, c| (h ^ c as u64).wrapping_mul(0x0100_0000_01b3));
        format!("/assets/chunk-{:08x}z9.js", h as u32)
    };
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let req = Request::post(format!("/v1/apps/services/{id}/environments/production/deploys")).header("content-type", "application/gzip").body(Body::from(bundle)).unwrap();
    let d2: Value = serde_json::from_slice(&w.api.clone().oneshot(req).await.unwrap().into_body().collect().await.unwrap().to_bytes()).unwrap();
    let d2 = settled(&w, &id, "production", d2["id"].as_str().unwrap()).await;
    assert_ne!(d2["image"], d["image"]);
    for (path, want) in [(chunk(&d["image"]), StatusCode::OK), (chunk(&d2["image"]), StatusCode::OK), ("/assets/chunk-0123abcdz9.js".to_string(), StatusCode::NOT_FOUND)] {
        let r = site(&path).await.unwrap();
        assert_eq!(r.status(), want, "{path}");
    }
    // `/api/* service:api/v1/:splat 200`: the project's api service answers on the site's origin.
    assert_eq!(site("/api/users").await.unwrap().status(), StatusCode::BAD_GATEWAY, "no api service yet");
    image_service(&w, "api", json!({"port": 80})).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let r = site("/api/users?page=2").await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["path"], "/v1/users", "{body}");

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
    // In tasks (Cloud Run Jobs'): each container told which it is, the job done when all are.
    assert_eq!(call(&w.api, "POST", &format!("{base}/jobs"), Some(json!({"command": "exit 0", "tasks": 101}))).await.0, StatusCode::BAD_REQUEST);
    let (_, t) = call(&w.api, "POST", &format!("{base}/jobs"), Some(json!({"command": "exit 0", "tasks": 3}))).await;
    let t = ended(t["id"].as_str().unwrap().to_string()).await;
    assert_eq!((t["status"].as_str(), t["tasks"].as_u64()), (Some("succeeded"), Some(3)), "{t}");
    let (_, log) = call(&w.api, "GET", &format!("{base}/jobs/{}/logs", t["id"].as_str().unwrap()), None).await;
    for i in 0..3 {
        assert!(log.as_array().unwrap().iter().any(|l| l["text"] == format!("job ran as task {i} of 3")), "task {i}: {log}");
    }
    // One that doesn't end by itself is canceled, and its container goes.
    let (_, long) = call(&w.api, "POST", &format!("{base}/jobs"), Some(json!({"command": "sleep 3600"}))).await;
    let lid = long["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(call(&w.api, "POST", &format!("{base}/jobs/{lid}/cancel"), None).await.0, StatusCode::OK);
    assert_eq!(ended(lid.clone()).await["status"], "canceled");
    assert_eq!(call(&w.api, "POST", &format!("{base}/jobs/{lid}/cancel"), None).await.0, StatusCode::CONFLICT);
    assert_eq!(w.fake.instances(&id).len(), instances, "no job's container is left");
    assert_eq!(call(&w.api, "GET", &format!("{base}/jobs"), None).await.1.as_array().unwrap().len(), 4);
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
    assert_eq!(hook["events"], json!(["deploy.failed", "crash_loop", "spend.alert", "spend.limit", "alert.memory", "alert.errors", "alert.latency", "alert.log", "cron.failed"]), "the default events");
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
    // What's been said about a page, from roll previews: none before it's on this node.
    assert_eq!(get("/__roll/feedback.json?path=%2Fpricing", "*/*").await.0, StatusCode::NOT_FOUND);
    struct Said(Mutex<Vec<(String, String)>>);
    #[async_trait]
    impl apps_control::integrations::FeedbackNotes for Said {
        async fn notes(&self, env: &str, path: &str) -> anyhow::Result<Vec<apps_control::integrations::Note>> {
            self.0.lock().push((env.into(), path.into()));
            Ok(vec![apps_control::integrations::Note { path: path.into(), selector: "h1".into(), note: "bigger".into(), by: "ana".into(), at: 1 }])
        }
    }
    let said = Arc::new(Said(Mutex::new(Vec::new())));
    w.apps.set_feedback_notes(said.clone());
    let (st, list) = get("/__roll/feedback.json?path=%2Fpricing", "*/*").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&list).unwrap(), json!([{"path": "/pricing", "selector": "h1", "note": "bigger", "by": "ana", "at": 1}]));
    assert_eq!(*said.0.lock(), [("pv-abc".to_string(), "/pricing".to_string())], "the preview and the page's path");
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
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"MODE": "broken", "EXTRA": "1", "TOKEN": "t"}, "seal": ["TOKEN"]}))).await;
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
    // What a rollback brings back sealed comes back sealed.
    let (_, r) = call(&w.api, "POST", &format!("{base}/deploys/{}/rollback", d2["id"].as_str().unwrap()), None).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let v = vals(&w).await;
    assert_eq!((v["MODE"].as_str(), v.get("TOKEN")), (Some("broken"), None), "{v}");
    let (_, listed) = call(&w.api, "GET", &format!("{base}/variables"), None).await;
    assert!(listed["vars"].as_array().unwrap().iter().any(|x| x["name"] == "TOKEN" && x["sealed"] == true), "{listed}");
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
    // Asleep first, and stopped (a rested instance may be frozen, which resumes at once).
    w.apps.live(&id, "production").sleep().await;
    get_until(&w.api, &format!("/v1/apps/services/{id}/environments/production"), |e| e["state"] == "stopped").await;
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
    // An hour from now, half a minute past a minute (so each pass's due time is fixed): the
    // reaper's own passes, at the real time, find nothing due.
    let t = (code_sql::now_ms() / 60_000 + 60) * 60_000 + 30_000;
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
    let live = w.apps.live(&id, "production");
    // A caller connects while it's asleep.
    w.fake.connect(&name);
    until("running").await;
    // Connected: in use, however long since rolld's last request.
    w.fake.set_private(&name, 1);
    live.backdate(Duration::from_secs(31));
    live.answer_private(true).await;
    live.tick().await;
    assert_eq!(state().await, "running");
    // Gone: it sleeps, frozen; the next caller thaws it.
    w.fake.set_private(&name, 0);
    live.backdate(Duration::from_secs(31));
    live.tick().await;
    assert_eq!(state().await, "suspended");
    let resumes = Fake::count(&w.fake.counts.resumes);
    w.fake.connect(&name);
    until("running").await;
    assert_eq!(Fake::count(&w.fake.counts.resumes), resumes + 1, "resumed, not started again");
    // Crashed (its waker armed again by the stop): a caller still wakes it.
    w.fake.crash(&name);
    live.tick().await;
    assert_eq!(state().await, "crashed");
    w.fake.connect(&name);
    until("running").await;
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
    let (_, body) = visit_deployment(&w, &host(&web["environments"][0]["url"]), r["id"].as_str().unwrap()).await;
    assert_eq!(body["vars"]["API_URL"], "http://api.production.internal:3000", "{body}");
    assert_eq!(body["vars"]["API_PUBLIC_URL"], api["environments"][0]["url"], "{body}");
    // Private, it has no public URL to give.
    call(&w.api, "PATCH", &format!("/v1/apps/services/{aid}"), Some(json!({"run": {"public": false}}))).await;
    assert_eq!(call(&w.api, "GET", &links, None).await.1[0]["vars"], json!(["API_URL"]));
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{aid}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&w.api, "GET", &links, None).await.1, json!([]), "gone with the service");
}

/// A wildcard domain serves every name one label under it once its DNS points at roll, after
/// any exact domain; where certificates can't be wildcards it's refused, not left pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wildcard_domain_serves_the_names_under_it() {
    let w = world().await;
    let (shop, _) = image_service(&w, "shop", json!({"port": 80})).await;
    let (api, _) = image_service(&w, "api", json!({"port": 80})).await;
    let domains = |svc: &Value| format!("/v1/apps/services/{}/environments/production/domains", svc["id"].as_str().unwrap());
    let (st, e) = call(&w.api, "POST", &domains(&shop), Some(json!({"hostname": "*.example.com"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no DNS-01 here: {e}");
    w.tls.dns01.store(true, Ordering::SeqCst);
    let (st, d) = call(&w.api, "POST", &domains(&shop), Some(json!({"hostname": "*.Example.com."}))).await;
    assert_eq!((st, d["hostname"].as_str()), (StatusCode::CREATED, Some("*.example.com")), "{d}");
    assert_eq!(call(&w.api, "POST", &domains(&shop), Some(json!({"hostname": "*.*.example.com"}))).await.0, StatusCode::BAD_REQUEST);
    // Its CNAME (the wildcard record itself) routes it; the proof can be the TXT on the name under it.
    w.dns.add("_roll-challenge.example.com", "TXT", d["token"].as_str().unwrap());
    let (_, v) = call(&w.api, "POST", &format!("{}/*.example.com/verify", domains(&shop)), None).await;
    assert_eq!(v["status"], "active", "{v}");
    let me = |(_, b): (StatusCode, Value)| b["instance"].as_str().unwrap_or("").to_string();
    assert!(me(visit(&w, "store.example.com", "/").await).contains(&shop["id"].as_str().unwrap()[4..]));
    assert_eq!(visit(&w, "a.b.example.com", "/").await.0, StatusCode::NOT_FOUND, "one label only");
    assert_eq!(visit(&w, "example.com", "/").await.0, StatusCode::NOT_FOUND, "not the name itself");
    // An exact name comes first.
    let (_, x) = call(&w.api, "POST", &domains(&api), Some(json!({"hostname": "api.example.com"}))).await;
    w.dns.add("_roll-challenge.api.example.com", "TXT", x["token"].as_str().unwrap());
    call(&w.api, "POST", &format!("{}/api.example.com/verify", domains(&api)), None).await;
    assert!(me(visit(&w, "api.example.com", "/").await).contains(&api["id"].as_str().unwrap()[4..]));
}


/// In maintenance an environment's public names answer roll's page with the message (as text)
/// and nothing wakes; a deployment's own URL still answers, and turning it off serves again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_answers_roll_page_and_wakes_nothing() {
    let w = world().await;
    let (svc, d) = image_service(&w, "shop", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let h = host(&call(&w.api, "GET", &base, None).await.1["url"]);
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"maintenance": {"message": "<b>Back at 5</b>"}}))).await;
    assert_eq!((st, &env["maintenance"]), (StatusCode::OK, &json!({"message": "<b>Back at 5</b>"})), "{env}");
    let starts = Fake::count(&w.fake.counts.starts);
    let resp = w.public.clone().oneshot(Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!((resp.status(), resp.headers()["retry-after"].to_str().unwrap()), (StatusCode::SERVICE_UNAVAILABLE, "30"));
    let page = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(page.contains("&lt;b&gt;Back at 5&lt;/b&gt;") && !page.contains("<b>Back"), "the message is text: {page}");
    assert_eq!(Fake::count(&w.fake.counts.starts), starts, "nothing woke");
    let own = format!("{}--{h}", d["id"].as_str().unwrap().trim_start_matches("dep_"));
    assert_eq!(visit(&w, &own, "/").await.0, StatusCode::OK, "a deployment's own URL answers");
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"maintenance": {"message": "x".repeat(501)}}))).await.0, StatusCode::BAD_REQUEST);
    // Its own pages (Heroku's) take the place of roll's, with the same status and Retry-After.
    let get = |path: &str| {
        let (public, req) = (w.public.clone(), Request::builder().uri(path).header("host", h.as_str()).body(Body::empty()).unwrap());
        async move {
            let r = public.oneshot(req).await.unwrap();
            let (st, retry) = (r.status(), r.headers().get("retry-after").map(|v| v.to_str().unwrap().to_string()));
            (st, retry, String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    for bad in [json!({"pages": {"error": "x".repeat(64 * 1024 + 1)}}), json!({"pages": {"other": "<p>x</p>"}}), json!({"pages": {"error": ""}})] {
        assert_eq!(call(&w.api, "PUT", &base, Some(bad)).await.0, StatusCode::BAD_REQUEST);
    }
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"pages": {"maintenance": "<h1>ours</h1>", "error": "<h1>oops</h1>"}}))).await;
    assert_eq!((st, &env["pages"]), (StatusCode::OK, &json!({"maintenance": "<h1>ours</h1>", "error": "<h1>oops</h1>"})), "{env}");
    assert_eq!(get("/").await, (StatusCode::SERVICE_UNAVAILABLE, Some("30".into()), "<h1>ours</h1>".into()));
    let (_, env) = call(&w.api, "PUT", &base, Some(json!({"maintenance": false}))).await;
    assert_eq!(env["maintenance"], Value::Null);
    assert_eq!(visit(&w, &h, "/").await.0, StatusCode::OK);
    // roll's 504 for a request past its timeout is their error page; the app's own answers stay.
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"timeout_s": 1}}))).await;
    let (_, r) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    assert_eq!(get("/sleep/1500").await, (StatusCode::GATEWAY_TIMEOUT, None, "<h1>oops</h1>".into()));
    assert_eq!(get("/").await.0, StatusCode::OK);
    let (_, env) = call(&w.api, "PUT", &base, Some(json!({"pages": {"error": null, "maintenance": null}}))).await;
    assert!(env.get("pages").is_none(), "back to roll's: {env}");
}

/// `run.drain_s`: a request still in flight to the deployment being replaced gets to finish
/// (the world's own drain is 50 ms).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replaced_instance_finishes_its_requests_within_drain_s() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "slow", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80, "drain_s": 5} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"drain_s": 0}}))).await.0, StatusCode::BAD_REQUEST);
    let h = host(&svc["environments"][0]["url"]);
    let base = format!("/v1/apps/services/{id}/environments/production");
    // Awake, then a request in flight while a redeploy replaces it.
    visit(&w, &h, "/").await;
    let slow = tokio::spawn({
        let (public, h) = (w.public.clone(), h.clone());
        async move { public.oneshot(Request::builder().uri("/sleep/1500").header("host", h).body(Body::empty()).unwrap()).await.unwrap().status() }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (_, r) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    assert_eq!(slow.await.unwrap(), StatusCode::OK, "it finished on the old instance");
}

/// Project secrets in an environment's values are shown only to whom roll secrets would show
/// them: otherwise the reveal is a 403 naming them, never a partial answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_values_are_revealed_only_as_roll_secrets_allows() {
    struct FakeSecrets;
    // Which version of the secrets is current (its digest, and DB_PASSWORD's value).
    static VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    #[async_trait]
    impl apps_control::integrations::SecretSource for FakeSecrets {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn digest(&self, _: &str, _: &str) -> anyhow::Result<Option<String>> {
            Ok(Some(format!("v{}", VERSION.load(std::sync::atomic::Ordering::SeqCst))))
        }
        async fn resolve(&self, _: &str, _: &str, _: &str) -> anyhow::Result<Vec<apps_control::integrations::SecretVar>> {
            let pw = match VERSION.load(std::sync::atomic::Ordering::SeqCst) {
                1 => "s3cret".to_string(),
                n => format!("s3cret-{n}"),
            };
            Ok(vec![
                apps_control::integrations::SecretVar { name: "DB_PASSWORD".into(), value: pw, linked: false, sensitive: false },
                apps_control::integrations::SecretVar { name: "SIGNING_KEY".into(), value: "only-for-runtimes".into(), linked: false, sensitive: true },
            ])
        }
        async fn may_reveal(&self, who: apps_control::integrations::Asker<'_>, _: &str, _: &str) -> bool {
            who.operator
        }
    }
    let w = world_full(|_| {}, false, |d| d.secrets = Arc::new(FakeSecrets)).await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let values = format!("/v1/apps/services/{}/environments/production/variables/values", svc["id"].as_str().unwrap());
    let (st, e) = call(&w.api, "GET", &values, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{e}");
    assert!(e["error"].as_str().unwrap().contains("DB_PASSWORD") && !e.to_string().contains("s3cret"), "{e}");
    let mut req = Request::builder().uri(&values).body(Body::empty()).unwrap();
    req.extensions_mut().insert(roll_auth::Operator);
    let resp = w.api.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["DB_PASSWORD"], "s3cret");
    // A sensitive one: to runtimes only, never in an answer, the operator's either.
    assert!(v.get("SIGNING_KEY").is_none() && !v.to_string().contains("only-for-runtimes"), "{v}");
    let (_, listed) = call(&w.api, "GET", values.trim_end_matches("/values"), None).await;
    assert!(listed["vars"].as_array().unwrap().iter().any(|x| x["name"] == "SIGNING_KEY" && x["sealed"] == true && x["source"] == "secret"), "{listed}");
    // A shell reads them all and a job can print them: the same rule.
    let base = values.trim_end_matches("/variables/values").to_string();
    // A secret changes after the deploy: the environment says so at once, by roll secrets' digest.
    assert_eq!(call(&w.api, "GET", &format!("{base}/pending"), None).await.1["secrets"], false);
    let _ = call(&w.api, "GET", &base, None).await;
    VERSION.store(2, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(call(&w.api, "GET", &format!("{base}/pending"), None).await.1["secrets"], true);
    assert_eq!(call(&w.api, "GET", &base, None).await.1["needs_deploy"], true, "not after the minute's cache");
    // Set to redeploy on a change: the owner's minute's watch does it, once.
    assert!(w.apps.watch_secrets().await.is_empty(), "off by default");
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"redeploy_on_secrets": true}))).await;
    assert_eq!((st, &env["redeploy_on_secrets"]), (StatusCode::OK, &json!(true)), "{env}");
    let started = w.apps.watch_secrets().await;
    assert_eq!(started.len(), 1);
    let d = settled(&w, svc["id"].as_str().unwrap(), "production", &started[0]).await;
    assert_eq!((d["status"].as_str(), d["trigger"].as_str()), (Some("active"), Some("settings")), "{d}");
    assert_eq!(call(&w.api, "GET", &format!("{base}/pending"), None).await.1["secrets"], false);
    assert!(w.apps.watch_secrets().await.is_empty(), "nothing moved since");
    assert_eq!(call(&w.api, "POST", &format!("{base}/shell/sessions"), None).await.0, StatusCode::FORBIDDEN);
    let (_, job) = call(&w.api, "POST", &format!("{base}/jobs"), Some(json!({"command": "exit 0"}))).await;
    assert_eq!(call(&w.api, "GET", &format!("{base}/jobs/{}/logs", job["id"].as_str().unwrap()), None).await.0, StatusCode::FORBIDDEN);
}


/// An allow list lets only its networks in through an environment's public names (from the
/// address the fleet vouches for), before anything wakes; no address, no entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allow_list_lets_only_its_networks_in() {
    let w = world().await;
    let (svc, _) = image_service(&w, "admin", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production", svc["id"].as_str().unwrap());
    let h = host(&call(&w.api, "GET", &base, None).await.1["url"]);
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"allow_ips": ["203.0.113.7", {"cidr": "10.1.2.3/8", "description": "office"}]}))).await;
    assert_eq!((st, &env["allow_ips"]), (StatusCode::OK, &json!([{"cidr": "203.0.113.7/32"}, {"cidr": "10.0.0.0/8", "description": "office"}])), "{env}");
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"allow_ips": ["10.0.0.0/33"]}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"allow_ips": vec!["10.0.0.1"; 101]}))).await.0, StatusCode::BAD_REQUEST);
    let from = |addr: Option<&str>| {
        let mut req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
        if let Some(a) = addr {
            req.extensions_mut().insert(axum::extract::ConnectInfo(a.parse::<std::net::SocketAddr>().unwrap()));
        }
        let public = w.public.clone();
        async move { public.oneshot(req).await.unwrap().status() }
    };
    assert_eq!(from(Some("10.9.9.9:5000")).await, StatusCode::OK);
    assert_eq!(from(Some("[::ffff:10.0.0.5]:5000")).await, StatusCode::OK, "an IPv4 client over IPv6");
    assert_eq!(from(Some("198.51.100.1:5000")).await, StatusCode::FORBIDDEN);
    assert_eq!(from(None).await, StatusCode::FORBIDDEN, "no address: no entry");
    call(&w.api, "PUT", &base, Some(json!({"allow_ips": null}))).await;
    assert_eq!(from(Some("198.51.100.1:5000")).await, StatusCode::OK);
}

/// `run.cpu_target`: a web service whose instances stay over the target for the window gets
/// another (one at a time), and goes back down once they'd be under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_web_service_scales_on_its_cpu() {
    let w = world_with(|c| {
        c.live.cpu_window = Duration::from_millis(300);
        c.live.scale_down_after = Duration::ZERO;
    })
    .await;
    let (svc, d) = image_service(&w, "render", json!({"port": 80, "min_instances": 1, "max_instances": 3, "cpu_target": 50})).await;
    assert_eq!(d["status"], "active", "{d}");
    let id = svc["id"].as_str().unwrap().to_string();
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"cpu_target": 5}}))).await.0, StatusCode::BAD_REQUEST);
    fn running(w: &World, id: &str) -> Vec<String> {
        w.fake.instances(id).into_iter().filter(|(_, _, st)| *st == FakeState::Running).map(|(n, _, _)| n).collect()
    }
    async fn until(w: &World, id: &str, n: usize) {
        let live = w.apps.live(id, "production");
        for _ in 0..100 {
            live.tick().await;
            if running(w, id).len() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        panic!("never {n} running: {:?}", w.fake.instances(id));
    }
    until(&w, &id, 1).await;
    w.fake.set_busy(&running(&w, &id)[0], 0.9);
    until(&w, &id, 2).await;
    for n in running(&w, &id) {
        w.fake.set_busy(&n, 0.05);
    }
    until(&w, &id, 1).await;
}

/// A web service's `timeout_s`: a request the app hasn't answered by then is a 504 (in its
/// access log as one); one answered in time is the app's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_past_its_timeout_is_a_504() {
    let w = world().await;
    let (svc, _) = image_service(&w, "slowpoke", json!({"port": 80, "timeout_s": 1})).await;
    let id = svc["id"].as_str().unwrap();
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"timeout_s": 3601}}))).await.0, StatusCode::BAD_REQUEST);
    let h = host(&svc["environments"][0]["url"]);
    assert_eq!(visit(&w, &h, "/sleep/100").await.0, StatusCode::OK);
    let began = std::time::Instant::now();
    assert_eq!(visit(&w, &h, "/sleep/1500").await.0, StatusCode::GATEWAY_TIMEOUT);
    assert!(began.elapsed() < Duration::from_millis(1400), "{:?}", began.elapsed());
}

/// A tunnel carries one TCP connection to a web service's port, private or not, waking it and
/// holding it awake; it's in the activity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_reaches_a_private_services_port() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let w = world().await;
    let (svc, _) = image_service(&w, "internal", json!({"port": 80, "public": false})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, w.api.clone()).into_future());
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{at}/v1/apps/services/{id}/environments/production/tunnel")).await.unwrap();
    ws.send(Message::Binary(b"GET /through HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec().into())).await.unwrap();
    let mut got = Vec::new();
    while let Some(Ok(m)) = ws.next().await {
        match m {
            // The app's end (`Connection: close`): an empty message.
            Message::Binary(b) if b.is_empty() => break,
            Message::Binary(b) => got.extend_from_slice(&b),
            Message::Close(_) => break,
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.starts_with("HTTP/1.1 200") && text.contains(r#""path":"/through""#), "{text}");
    let (_, act) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert!(act.to_string().contains("audit.tunnel.opened"), "{act}");
}

/// A rate limit lets each client make so many requests (a burst, then its rate) and tells the
/// rest when to come back, before anything wakes; another client has its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limit_slows_each_client_down() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production", svc["id"].as_str().unwrap());
    let h = host(&call(&w.api, "GET", &base, None).await.1["url"]);
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"rate_limit": {"per_minute": 0}}))).await.0, StatusCode::BAD_REQUEST);
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"rate_limit": {"per_minute": 60, "burst": 2}}))).await;
    assert_eq!((st, &env["rate_limit"]), (StatusCode::OK, &json!({"per_minute": 60, "burst": 2})), "{env}");
    let from = |addr: &'static str| {
        let mut req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(addr.parse::<std::net::SocketAddr>().unwrap()));
        let public = w.public.clone();
        async move {
            let r = public.oneshot(req).await.unwrap();
            (r.status(), r.headers().get("retry-after").map(|v| v.to_str().unwrap().to_string()))
        }
    };
    assert_eq!(from("10.0.0.1:1").await.0, StatusCode::OK);
    assert_eq!(from("10.0.0.1:2").await.0, StatusCode::OK);
    assert_eq!(from("10.0.0.1:3").await, (StatusCode::TOO_MANY_REQUESTS, Some("1".into())), "a token a second");
    assert_eq!(from("10.0.0.2:1").await.0, StatusCode::OK, "another client has its own");
    call(&w.api, "PUT", &base, Some(json!({"rate_limit": null}))).await;
    assert_eq!(from("10.0.0.1:4").await.0, StatusCode::OK);
}

/// A worker's restart policy: `on_failure` starts it again after a crash but leaves it stopped
/// once it ends cleanly, nothing taking its place, until a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workers_restart_policy_decides_whether_it_runs_again() {
    let w = world_with(|c| c.live.restart_backoff = Duration::from_millis(5)).await;
    let (st, e) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80, "restart": "never"} }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "workers only: {e}");
    let (svc, d) = image_service(&w, "jobs", json!({"type": "worker", "restart": "on_failure", "restart_tries": 3})).await;
    assert_eq!(d["status"], "active", "{d}");
    let id = svc["id"].as_str().unwrap().to_string();
    let live = w.apps.live(&id, "production");
    fn running(w: &World, id: &str) -> Vec<String> {
        w.fake.instances(id).into_iter().filter(|(_, _, st)| *st == FakeState::Running).map(|(n, _, _)| n).collect()
    }
    async fn ticks(live: &Arc<apps_control::live::EnvLive>, w: &World, id: &str, want: usize) -> bool {
        for _ in 0..100 {
            live.tick().await;
            if running(w, id).len() == want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }
    // A crash: started again.
    let name = running(&w, &id)[0].clone();
    w.fake.crash(&name);
    live.tick().await;
    assert!(ticks(&live, &w, &id, 1).await, "restarted after a failure");
    // Done with its work: left stopped, and nothing takes its place.
    w.fake.exit(&running(&w, &id)[0], 0);
    for _ in 0..20 {
        live.tick().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(running(&w, &id).is_empty(), "{:?}", w.fake.instances(&id));
    let made = w.fake.instances(&id).len();
    assert_eq!(made, 1, "no new instance");
    // A restart runs it again.
    assert_eq!(call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/restart"), None).await.0, StatusCode::ACCEPTED);
    assert!(ticks(&live, &w, &id, 1).await, "running after a restart");
}

/// Affinity keeps a visitor on the instance their cookie names while it's up (the app never sees
/// the cookie); when it's gone they go elsewhere and the cookie follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn affinity_keeps_a_visitor_on_one_instance() {
    let w = world().await;
    let (svc, _) = image_service(&w, "sessions", json!({"port": 80, "min_instances": 2, "max_instances": 2, "affinity": true})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let h = host(&svc["environments"][0]["url"]);
    let live = w.apps.live(&id, "production");
    // Running as roll sees them (its phase follows the runtime's a moment later).
    let base = format!("/v1/apps/services/{id}/environments/production");
    let running = || async {
        let (_, e) = call(&w.api, "GET", &base, None).await;
        e["instances"].as_array().unwrap().iter().filter(|i| i["state"] == "running").map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };
    for _ in 0..100 {
        live.tick().await;
        if running().await.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut up = running().await;
    up.sort();
    assert_eq!(up.len(), 2, "{:?}", w.fake.instances(&id));
    let get = |cookie: Option<String>| {
        let mut b = Request::builder().uri("/").header("host", h.as_str());
        if let Some(c) = cookie {
            b = b.header("cookie", format!("theme=dark; roll_instance={c}"));
        }
        let (public, req) = (w.public.clone(), b.body(Body::empty()).unwrap());
        async move {
            let r = public.oneshot(req).await.unwrap();
            let set = r.headers().get("set-cookie").map(|v| v.to_str().unwrap().to_string());
            let body: Value = serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
            (body["instance"].as_str().unwrap_or("").to_string(), body["cookie"].as_str().unwrap_or("").to_string(), set)
        }
    };
    // Pointed at the second instance, it stays there.
    let second = up[1].clone();
    let key = apps_control::live::affinity_key(&second);
    for _ in 0..4 {
        let (inst, cookie, set) = get(Some(key.clone())).await;
        assert_eq!((inst.as_str(), cookie.as_str(), set), (second.as_str(), "theme=dark", None), "kept, and the app sees only its own cookies");
    }
    // Gone: another answers, and the cookie follows it.
    w.fake.crash(&second);
    live.tick().await;
    let (inst, _, set) = get(Some(key)).await;
    assert_ne!(inst, second);
    assert!(set.is_some_and(|s| s.starts_with(&format!("roll_instance={};", apps_control::live::affinity_key(&inst)))), "the cookie follows the instance that answered");
}

/// A TCP service answers each environment on a port of its own, woken and spliced per
/// connection (its HTTP names don't serve it); a TCP service has no HTTP settings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tcp_service_answers_on_its_own_port() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let w = world_with(|c| {
        c.tcp_ports = Some((free, free));
        c.tcp_host = "127.0.0.1".into();
    })
    .await;
    let (st, e) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "bad", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80, "tcp": true, "health_path": "/"} }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
    let (svc, _) = image_service(&w, "mqtt", json!({"port": 80, "tcp": true})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    tokio::spawn(w.apps.clone().serve_tcp_ports("127.0.0.1".parse().unwrap()));
    let env = get_until(&w.api, &format!("/v1/apps/services/{id}/environments/production"), |e| e["tcp"].is_string()).await;
    assert_eq!(env["tcp"], format!("127.0.0.1:{free}"), "{env}");
    let mut conn = None;
    for _ in 0..100 {
        if let Ok(c) = tokio::net::TcpStream::connect(("127.0.0.1", free)).await {
            conn = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let mut conn = conn.expect("its port listens");
    conn.write_all(b"GET /raw HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut got = String::new();
    conn.read_to_string(&mut got).await.unwrap();
    assert!(got.starts_with("HTTP/1.1 200") && got.contains(r#""path":"/raw""#), "spliced to the app: {got}");
    assert_eq!(visit(&w, &host(&svc["environments"][0]["url"]), "/").await.0, StatusCode::NOT_FOUND, "not over HTTP");
    // Its rate limit counts connections per client: past the burst, closed at once.
    call(&w.api, "PUT", &format!("/v1/apps/services/{id}/environments/production"), Some(json!({"rate_limit": {"per_minute": 1, "burst": 1}}))).await;
    let mut once = tokio::net::TcpStream::connect(("127.0.0.1", free)).await.unwrap();
    once.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut a = String::new();
    once.read_to_string(&mut a).await.unwrap();
    assert!(a.starts_with("HTTP/1.1 200"), "the first: {a}");
    let mut twice = tokio::net::TcpStream::connect(("127.0.0.1", free)).await.unwrap();
    let _ = twice.write_all(b"GET /b HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    let mut b = String::new();
    let _ = twice.read_to_string(&mut b).await;
    assert!(b.is_empty(), "the second is closed: {b}");
    call(&w.api, "PUT", &format!("/v1/apps/services/{id}/environments/production"), Some(json!({"rate_limit": null}))).await;
    // Private, it has no public port (TCP carries no token): the private network reaches it.
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"public": false}}))).await;
    assert!(call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await.1["tcp"].is_null());
    assert!(w.apps.tcp_endpoints().await.unwrap().is_empty());
}

/// An environment's metrics as a scraper reads them (Prometheus' text format).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_come_in_prometheus_text_too() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap();
    let req = Request::builder().uri(format!("/v1/apps/services/{id}/environments/production/metrics?format=prometheus")).body(Body::empty()).unwrap();
    let resp = w.api.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/plain; version=0.0.4"));
    let text = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    let labels = format!(r#"service="api",service_id="{id}",environment="production""#);
    for name in ["roll_apps_requests_per_minute", "roll_apps_errors_per_minute", "roll_apps_cpu_seconds_per_minute", "roll_apps_memory_bytes", "roll_apps_instances", "roll_apps_cold_starts_per_minute"] {
        assert!(text.contains(&format!("# TYPE {name} gauge\n{name}{{{labels}}} ")), "{name}: {text}");
    }
    assert!(text.contains(&format!(r#"roll_apps_latency_ms{{{labels},quantile="0.95"}}"#)), "{text}");
}


/// Firewall rules: the first that matches a request decides (allow or deny), on its path (one
/// an app would read the same way included), methods and user agent, before anything wakes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn firewall_rules_decide_in_order() {
    let w = world().await;
    let (svc, _) = image_service(&w, "shop", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production", svc["id"].as_str().unwrap());
    let h = host(&call(&w.api, "GET", &base, None).await.1["url"]);
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "deny"}]}))).await.0, StatusCode::BAD_REQUEST, "a rule matches on something");
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "deny", "path": "/a*b"}]}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "deny", "path": "/é"}]}))).await.0, StatusCode::OK, "a path ending in a wide character");
    let rules = json!([
        {"action": "allow", "path": "/admin/health"},
        {"action": "deny", "path": "/admin/*"},
        {"action": "deny", "methods": ["delete"]},
        {"action": "deny", "path": "/report", "methods": ["GET"]},
        {"action": "deny", "user_agent": "BadBot"},
    ]);
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"firewall": rules}))).await;
    assert_eq!((st, env["firewall"][2]["methods"].clone()), (StatusCode::OK, json!(["DELETE"])), "{env}");
    let req = |method: &str, path: &str, ua: &str| {
        let r = Request::builder().method(method).uri(path).header("host", h.as_str()).header("user-agent", ua).body(Body::empty()).unwrap();
        let public = w.public.clone();
        async move { public.oneshot(r).await.unwrap().status() }
    };
    assert_eq!(req("GET", "/admin/health", "curl").await, StatusCode::OK, "the first match lets it in");
    assert_eq!(req("GET", "/admin/users", "curl").await, StatusCode::FORBIDDEN);
    assert_eq!(req("GET", "/%61dmin/users", "curl").await, StatusCode::FORBIDDEN, "as the app would read it");
    assert_eq!(req("GET", "/x/../admin/users", "curl").await, StatusCode::FORBIDDEN);
    assert_eq!(req("GET", "/admin", "curl").await, StatusCode::FORBIDDEN, "a prefix rule takes the bare path");
    assert_eq!(req("GET", "/ADMIN/users", "curl").await, StatusCode::FORBIDDEN, "any case");
    assert_eq!(req("GET", "/admin;jsessionid=1/users", "curl").await, StatusCode::FORBIDDEN, "path parameters");
    assert_eq!(req("GET", "/admin/health/", "curl").await, StatusCode::OK, "a trailing slash is the same path");
    assert_eq!(req("DELETE", "/cart", "curl").await, StatusCode::FORBIDDEN);
    assert_eq!(req("HEAD", "/report", "curl").await, StatusCode::FORBIDDEN, "HEAD is a GET");
    assert_eq!(req("GET", "/", "Mozilla badbot/2").await, StatusCode::FORBIDDEN);
    assert_eq!(req("GET", "/", "Mozilla").await, StatusCode::OK, "nothing matching lets it in");
    call(&w.api, "PUT", &base, Some(json!({"firewall": null}))).await;
    assert_eq!(req("GET", "/admin/users", "curl").await, StatusCode::OK);
}

/// What the next deploy changes: the environment's own variables by name (never a value) and
/// the run settings that differ from the live deployment's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_changes_say_what_the_next_deploy_does() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "api", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": {"MODE": "old", "GONE": "x"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let base = format!("/v1/apps/services/{id}/environments/production");
    assert_eq!(call(&w.api, "GET", &format!("{base}/pending"), None).await.1, json!({"variables": {"added": [], "removed": [], "changed": []}, "run": [], "secrets": null}));
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"MODE": "new-secret-value", "ADDED": "1"}, "unset": ["GONE"]}))).await;
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"memory_mb": 1024}}))).await;
    let (_, p) = call(&w.api, "GET", &format!("{base}/pending"), None).await;
    assert_eq!(p, json!({"variables": {"added": ["ADDED"], "removed": ["GONE"], "changed": ["MODE"]}, "run": ["memory_mb"], "secrets": null}));
    assert!(!p.to_string().contains("new-secret-value"), "names only");
}

/// A log alert: its output saying the pattern often enough in five minutes is one event (roll's
/// own lines don't count), as metric alerts are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_alert_is_an_event_once() {
    use apps_control::logs::Stream;
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80, "alerts": {"log_match": "panic", "log_count": 2}})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let live = w.apps.live(&id, "production");
    let key = format!("env:{id}/production");
    let alerts = || async { call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "alert.log").cloned().collect::<Vec<_>>() };
    w.apps.logs.sink(key.clone(), Stream::Stdout).line("thread main PANICKED at boom");
    w.apps.logs.sink(key.clone(), Stream::System).line("roll says panic too");
    w.apps.judge_alerts(&live).await;
    assert!(alerts().await.is_empty(), "one of its own lines, and roll's doesn't count");
    w.apps.logs.sink(key.clone(), Stream::Stderr).line("panic again");
    // Lines reach the log a moment after they're written: judged until it fires, then again.
    for _ in 0..100 {
        w.apps.judge_alerts(&live).await;
        if !alerts().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.apps.judge_alerts(&live).await;
    let fired = alerts().await;
    assert_eq!(fired.len(), 1, "once while it lasts: {fired:?}");
    assert_eq!((fired[0]["data"]["count"].as_u64(), fired[0]["data"]["pattern"].as_str()), (Some(2), Some("panic")));
}


/// `run.rollback_on_crash`: a deployment that crash-loops soon after going live is replaced by
/// the one before it (its image, no build), and the rollback isn't rolled back in turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deployment_that_crash_loops_is_rolled_back() {
    let w = world_with(|c| c.live.restart_backoff = Duration::from_millis(5)).await;
    let (svc, d1) = image_service(&w, "jobs", json!({"type": "worker", "rollback_on_crash": true})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (_, r) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    let d2 = settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    assert_eq!(d2["status"], "active", "{d2}");
    let live = w.apps.live(&id, "production");
    let running = |w: &World| w.fake.instances(&id).into_iter().filter(|(_, dep, st)| *st == FakeState::Running && *dep == d2["id"].as_str().unwrap()).map(|(n, _, _)| n).collect::<Vec<_>>();
    for _ in 0..3 {
        let mut up = running(&w);
        for _ in 0..200 {
            if !up.is_empty() {
                break;
            }
            live.tick().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            up = running(&w);
        }
        w.fake.crash(&up[0]);
        live.tick().await;
    }
    // The reaper notices the loop and rolls back.
    let env = get_until(&w.api, &base, |e| e["active"]["id"] != d2["id"] && e["active"]["id"].is_string()).await;
    let (_, now) = call(&w.api, "GET", &format!("{base}/deploys/{}", env["active"]["id"].as_str().unwrap()), None).await;
    assert_eq!((now["rollback_of"].as_str(), now["actor"].as_str()), (d1["id"].as_str(), Some("crash loop")), "{now}");
}

/// Reference variables: `${{ VAR }}` and `${{ service.VAR }}` are filled in at the deploy (the
/// listing shows the template, never the value); one that names no service is refused, and a
/// token needs reveal on the service it reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reference_variables_are_filled_at_the_deploy() {
    let w = world().await;
    let (st, db) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "db", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 5432}, "variables": {"PASS": "s3cret"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{db}");
    let (st, _) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "bad", "source": {"kind": "image", "image": "nginx:1.27"}, "variables": {"X": "${{ nobody.Y }}"} }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no such service");
    let vars = json!({"DB_URL": "postgres://app:${{ db.PASS }}@${{ db.ROLL_PRIVATE_HOST }}:${{db.PORT}}/x", "SELF": "${{ ROLL_SERVICE }}-${{ MISSING }}", "FAKE_ECHO": "DB_URL,SELF"});
    let (st, api) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "api", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": vars }))).await;
    assert_eq!(st, StatusCode::CREATED, "{api}");
    let id = api["id"].as_str().unwrap().to_string();
    let d = settled(&w, &id, "production", api["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let (_, body) = visit_deployment(&w, &host(&api["environments"][0]["url"]), d["id"].as_str().unwrap()).await;
    assert_eq!(body["vars"], json!({"DB_URL": "postgres://app:s3cret@db.production.internal:5432/x", "SELF": "api-"}), "{body}");
    let (_, listed) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/variables"), None).await;
    let db_url = listed["vars"].as_array().unwrap().iter().find(|v| v["name"] == "DB_URL").unwrap().clone();
    assert_eq!((db_url["source"].as_str(), db_url["reference"].as_str()), (Some("reference"), Some("postgres://app:${{ db.PASS }}@${{ db.ROLL_PRIVATE_HOST }}:${{db.PORT}}/x")));
    assert!(!listed.to_string().contains("s3cret"), "the listing never shows the value");
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs?tail=50"), None).await;
    assert!(log.to_string().contains("SELF: MISSING"), "{log}");
    // A token that may write apps but not reveal db's values can't reference them.
    let scope: roll_auth::scope::Scope = serde_json::from_value(json!([{"product": "apps", "resources": "*", "actions": {"write": "allow", "read": "allow"}}])).unwrap();
    let grant = roll_auth::Grant { id: "tok_t".into(), name: "ci".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(scope), ask_otherwise: false };
    let mut req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/apps/services/{id}/environments/production/variables"))
        .header("content-type", "application/json")
        .body(Body::from(json!({"set": {"STOLEN": "${{ db.PASS }}"}}).to_string()))
        .unwrap();
    req.extensions_mut().insert(grant);
    assert_eq!(w.api.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
    // Sealed (Railway's): run with, never shown again, nor through a reference; a new value
    // stays sealed, and so does a copy into another environment.
    let db_vars = format!("/v1/apps/services/{}/environments/production/variables", db["id"].as_str().unwrap());
    assert_eq!(call(&w.api, "PATCH", &db_vars, Some(json!({"seal": ["NOPE"]}))).await.0, StatusCode::BAD_REQUEST);
    let (st, v) = call(&w.api, "PATCH", &db_vars, Some(json!({"seal": ["PASS"]}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, v) = call(&w.api, "PATCH", &db_vars, Some(json!({"set": {"PASS": "s3cret"}}))).await;
    let pass = v["vars"].as_array().unwrap().iter().find(|x| x["name"] == "PASS").unwrap().clone();
    assert_eq!((pass["sealed"].as_bool(), pass["masked"].as_str()), (Some(true), Some("••••••••")), "{v}");
    assert!(call(&w.api, "GET", &format!("{db_vars}/values"), None).await.1.get("PASS").is_none());
    let api_vars = format!("/v1/apps/services/{id}/environments/production/variables");
    let (_, vals) = call(&w.api, "GET", &format!("{api_vars}/values"), None).await;
    assert!(vals.get("DB_URL").is_none() && vals["SELF"] == "api-", "a reference that reads it is sealed too: {vals}");
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), Some(json!({}))).await;
    let d = settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    let (_, body) = visit_deployment(&w, &host(&api["environments"][0]["url"]), d["id"].as_str().unwrap()).await;
    assert_eq!(body["vars"]["DB_URL"], "postgres://app:s3cret@db.production.internal:5432/x", "the app still gets it");
    let staging = db_vars.replace("production", "staging");
    let (st, _) = call(&w.api, "PUT", staging.trim_end_matches("/variables"), Some(json!({"variables_from": "production"}))).await;
    assert_eq!(st, StatusCode::OK);
    let (_, listed) = call(&w.api, "GET", &staging, None).await;
    assert!(!listed["vars"].as_array().unwrap().iter().any(|v| v["name"] == "PASS"), "a copy leaves sealed ones behind: {listed}");
}

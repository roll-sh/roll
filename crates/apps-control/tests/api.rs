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
use std::time::{Duration, Instant};
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
    async fn read_file(&self, _repo: &str, _revision: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.files.lock().get(path).map(|v| v.as_bytes().to_vec()))
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
    async fn commits(&self, _repo: &str, from: &str, to: &str, limit: usize) -> anyhow::Result<Option<(Vec<apps_control::integrations::CommitInfo>, bool)>> {
        // Its landings are its history, oldest first.
        let l = self.landings.lock();
        let Some(end) = l.iter().position(|x| x.revision.sha == to) else { return Ok(None) };
        let start = l.iter().position(|x| x.revision.sha == from);
        let mut out: Vec<_> = l[start.map_or(0, |s| s + 1)..=end].iter().rev().map(|x| apps_control::integrations::CommitInfo { sha: x.revision.sha.clone(), summary: x.revision.message.clone(), author: x.author.clone().unwrap_or_default() }).collect();
        out.truncate(limit);
        Ok(Some((out, start.is_some())))
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
        Ok(self.any_path_matches(patterns, &changed))
    }
    fn any_path_matches(&self, patterns: &[String], paths: &[String]) -> bool {
        paths.iter().any(|f| patterns.iter().any(|p| f.starts_with(p.trim_end_matches("**"))))
    }
}

/// A certificate provider that records what it was told (Cloudflare for SaaS's shape: it says
/// where a certificate is when asked).
#[derive(Default)]
struct FakeTls {
    calls: Mutex<Vec<String>>,
    issued: Mutex<Vec<String>>,
    /// Hosts whose certificate failed, and why.
    failed: Mutex<Vec<(String, String)>>,
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
        if let Some((_, why)) = self.failed.lock().iter().find(|(h, _)| h == host) {
            return Ok(Some((apps_control::model::TlsStatus::Failed, Some(why.clone()))));
        }
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
/// fresh; and a search cluster, `logs`, that makes each link an API key of its own (in rolld:
/// roll secrets' search provider, through search's keys route).
struct FakeResources;

/// What the fake made and revoked for `logs` links, in order.
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
        if (kind.as_str(), r#ref) != ("search", "logs") {
            return Ok(None);
        }
        // A provider takes a moment: two requests at once both get here.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut m = MINTED.lock();
        let key = format!("key{}", m.len());
        m.push(format!("link {service} {key}"));
        let vars = vec![("ELASTICSEARCH_URL".into(), "https://logs.search.roll.test".into()), ("ELASTICSEARCH_API_KEY".into(), format!("{key}-secret"))];
        Ok(Some(apps_control::integrations::Minted { vars, kept: format!("key:{key}") }))
    }

    async fn unlink(&self, _kind: apps_control::model::LinkKind, _ref: &str, kept: &str) -> anyhow::Result<()> {
        MINTED.lock().push(format!("unlink {}", kept.trim_start_matches("key:")));
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
    cfg.otlp_base = Some("http://127.0.0.1:7070".into());
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

/// Sidecars (Cloud Run's, Fly's): containers beside each instance, their images pinned with the
/// deployment, each given its own settings and only the variables it names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sidecars_run_beside_each_instance_with_only_what_they_name() {
    let w = world().await;
    let proxy = json!({"name": "db-proxy", "image": "cloud-sql-proxy:2", "env": {"PORT": "5432"}, "variables": ["DATABASE_URL", "MISSING"], "memory_mb": 256});
    let make = |name: &str, run: Value| json!({ "project": w.project, "name": name, "source": {"kind": "image", "image": "nginx:1.27"}, "run": run, "variables": {"DATABASE_URL": "postgres://u:p@db/x", "STRIPE_KEY": "sk_live"} });
    for (run, why) in [
        (json!({"type": "cron", "schedule": "@daily", "sidecars": [proxy]}), "a cron service has none"),
        (json!({"port": 80, "sidecars": [proxy, proxy]}), "names are unique"),
        (json!({"port": 80, "sidecars": [{"name": "x", "image": "y", "cpu": 4}]}), "cpu is 0.1–2"),
        (json!({"port": 80, "sidecars": [{"name": "x", "image": "y", "variables": ["not a name"]}]}), "variables are names"),
    ] {
        assert_eq!(call(&w.api, "POST", "/v1/apps/services", Some(make("bad", run))).await.0, StatusCode::BAD_REQUEST, "{why}");
    }
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(make("api", json!({"port": 80, "min_instances": 1, "sidecars": [proxy]})))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let mut inst = None;
    for _ in 0..200 {
        inst = w.fake.instances(&id).into_iter().find(|(_, _, st)| *st == FakeState::Running).map(|(n, _, _)| n);
        if inst.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let inst = inst.expect("an instance runs");
    let spec = w.fake.spec(&inst).unwrap();
    let sc = &spec.sidecars[0];
    assert_eq!((sc.name.as_str(), sc.memory_mb, sc.cpu), ("db-proxy", 256, 0.25));
    // Pinned with the deployment (stored with it): a rollback or a redeploy runs the same one.
    assert_eq!(sc.image, format!("cloud-sql-proxy:2@sha256:{}", "0".repeat(64)));
    // Its own setting and the variable it named; not the app's others, nor one that isn't set.
    let mut names: Vec<&str> = sc.vars.iter().map(|(k, _)| k.as_str()).collect();
    names.sort();
    assert_eq!(names, ["DATABASE_URL", "PORT"]);
    // Its output in the environment's log under its own container's name.
    let log = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs"), None).await.1;
    let line = log.as_array().unwrap().iter().find(|l| l["text"].as_str().is_some_and(|t| t.starts_with("sidecar db-proxy running"))).unwrap_or_else(|| panic!("{log}"));
    assert_eq!(line["instance"].as_str(), Some(format!("{inst}-sc-db-proxy").as_str()), "{line}");
    assert!(!log.to_string().contains("sk_live"), "a value never goes in the log");
}

/// Traces: turned on, an environment's instances get the OpenTelemetry SDK's variables pointing
/// at roll; what they send (OTLP JSON here) reads back as recent traces and as one trace's
/// spans; another token is a 404; turned off, the variables go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn traces_sent_by_instances_read_back() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let env = format!("/v1/apps/services/{id}/environments/production");
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({"traces": true}))).await;
    assert_eq!((st, &e["traces"]), (StatusCode::OK, &json!(true)), "{e}");
    let (_, vars) = call(&w.api, "GET", &format!("{env}/variables/values"), None).await;
    assert_eq!((vars["OTEL_SERVICE_NAME"].as_str(), vars["OTEL_EXPORTER_OTLP_PROTOCOL"].as_str(), vars["OTEL_METRICS_EXPORTER"].as_str()), (Some("api"), Some("http/protobuf"), Some("otlp")), "{vars}");
    // Its token is a credential: sealed, never in an answer (the instances still get it).
    assert!(vars.get("OTEL_EXPORTER_OTLP_ENDPOINT").is_none(), "{vars}");
    let (_, listed) = call(&w.api, "GET", &format!("{env}/variables"), None).await;
    let token = w.apps.store.trace_token(&id, "production").unwrap().unwrap();
    assert!(!listed.to_string().contains(&token), "{listed}");
    // On the apps listener (public traffic's, never the API's).
    let path = format!("/_roll/otlp/{id}/production/{token}/v1/traces");
    let post = |path: String, body: Value| {
        let req = Request::builder().method("POST").uri(path).header("host", "anything.example").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
        let public = w.public.clone();
        async move {
            let r = public.oneshot(req).await.unwrap();
            let st = r.status();
            (st, serde_json::from_slice::<Value>(&r.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
        }
    };
    let (t, root, child) = ("a1".repeat(16), "b2".repeat(8), "c3".repeat(8));
    let ms = 1_000_000u64;
    let body = json!({ "resourceSpans": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "api" } }] }, "scopeSpans": [{ "spans": [
        { "traceId": t, "spanId": root, "name": "GET /items", "startTimeUnixNano": (1_700_000_000_000 * ms).to_string(), "endTimeUnixNano": (1_700_000_000_120 * ms).to_string() },
        { "traceId": t, "spanId": child, "parentSpanId": root, "name": "SELECT items", "startTimeUnixNano": (1_700_000_000_010 * ms).to_string(), "endTimeUnixNano": (1_700_000_000_090 * ms).to_string(), "status": { "code": 2 } },
    ] }] }] });
    let (st, v) = post(path, body.clone()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, _) = post(format!("/_roll/otlp/{id}/production/rlx_nope/v1/traces"), body.clone()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(call(&w.api, "POST", &format!("/v1/apps/otlp/{id}/production/{token}/v1/traces"), Some(body)).await.0, StatusCode::NOT_FOUND, "not on the API");
    let (_, list) = call(&w.api, "GET", &format!("{env}/traces?minutes=60"), None).await;
    assert_eq!(list["traces"][0], json!({ "trace": t, "name": "GET /items", "service": "api", "start_ms": 1_700_000_000_000u64, "ms": 120, "spans": 2, "error": true }), "{list}");
    let (_, one) = call(&w.api, "GET", &format!("{env}/traces/{t}"), None).await;
    assert_eq!((one["spans"][0]["name"].as_str(), one["spans"][1]["parent"].as_str()), (Some("GET /items"), Some(root.as_str())), "{one}");
    // The proxy's own span: under the client's traceparent, the app's parent, its wake a child.
    let h = host(&svc["environments"][0]["url"]);
    let client_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    w.apps.live(&id, "production").sleep().await;
    let req = Request::builder().uri("/items/42").header("host", h.clone()).header("traceparent", format!("00-{client_trace}-00f067aa0ba902b7-01")).body(Body::empty()).unwrap();
    let r = w.public.clone().oneshot(req).await.unwrap();
    let app: Value = serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let seen = app["traceparent"].as_str().unwrap_or("").to_string();
    assert!(seen.starts_with(&format!("00-{client_trace}-")) && !seen.contains("00f067aa0ba902b7"), "the app's parent is the proxy's span: {app}");
    let (_, one) = call(&w.api, "GET", &format!("{env}/traces/{client_trace}"), None).await;
    let spans = one["spans"].as_array().cloned().unwrap_or_default();
    let proxy = spans.iter().find(|s| s["service"] == "roll" && s["name"] == "GET /items/:id").unwrap_or_else(|| panic!("{one}"));
    assert_eq!((proxy["parent"].as_str(), proxy["span"].as_str()), (Some("00f067aa0ba902b7"), seen.split('-').nth(2)), "{one}");
    assert!(spans.iter().any(|s| s["name"] == "roll wake" && s["parent"] == proxy["span"]), "a cold start's wait: {one}");
    // Logs into the environment's own log (its level and trace id readable there), metrics as
    // series: a counter's rate a second.
    let logs = json!({ "resourceLogs": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "api" } }] }, "scopeLogs": [{ "logRecords": [
        { "timeUnixNano": "1700000000000000000", "severityNumber": 17, "body": { "stringValue": "payment failed" }, "traceId": t, "spanId": root, "attributes": [{ "key": "order", "value": { "intValue": "42" } }] },
    ] }] }] });
    let otlp = |signal: &str, body: Value| post(format!("/_roll/otlp/{id}/production/{token}/v1/{signal}"), body);
    assert_eq!(otlp("logs", logs).await.0, StatusCode::OK);
    let (_, lines) = call(&w.api, "GET", &format!("{env}/logs?level=error&tail=20"), None).await;
    let line: Value = lines.as_array().unwrap().iter().filter_map(|l| serde_json::from_str::<Value>(l["text"].as_str()?).ok()).find(|l| l["msg"] == "payment failed").unwrap_or_else(|| panic!("{lines}"));
    assert_eq!((line["trace_id"].as_str(), line["order"].as_str()), (Some(t.as_str()), Some("42")), "{line}");
    let now = code_sql::now_ms() as u64 * 1_000_000;
    let counter = |n: u64, at: u64| json!({ "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{ "name": "http.requests", "unit": "1", "sum": { "aggregationTemporality": 2, "isMonotonic": true, "dataPoints": [{ "asInt": n.to_string(), "timeUnixNano": at.to_string() }] } }] }] }] });
    assert_eq!(otlp("metrics", counter(100, now - 120_000_000_000)).await.0, StatusCode::OK);
    assert_eq!(otlp("metrics", counter(160, now - 60_000_000_000)).await.0, StatusCode::OK);
    assert_eq!(otlp("profiles", json!({})).await.0, StatusCode::NOT_FOUND);
    let (_, m) = call(&w.api, "GET", &format!("{env}/app-metrics?minutes=30"), None).await;
    let s = &m["series"][0];
    assert_eq!((s["name"].as_str(), s["shows"].as_str()), (Some("http.requests"), Some("per_second")), "{m}");
    assert_eq!(s["points"].as_array().and_then(|p| p.last()).map(|p| p[1].clone()), Some(json!(1.0)), "60 more in 60 s: {m}");
    call(&w.api, "PUT", &env, Some(json!({"traces": false}))).await;
    let (_, vars) = call(&w.api, "GET", &format!("{env}/variables/values"), None).await;
    assert!(vars.get("OTEL_SERVICE_NAME").is_none(), "{vars}");
}

/// Web Vitals: turned on, an environment's pages carry roll's tag and its script is served from
/// its own host; the browser's beacons are kept checked (a route, bounded numbers) and read back
/// as p75s rated by web.dev's thresholds, overall and by route; turned off, nothing is tagged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_vitals_from_visitors_read_back_as_p75s() {
    let w = world().await;
    let (svc, _) = image_service(&w, "site", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let h = host(&svc["environments"][0]["url"]);
    let env = format!("/v1/apps/services/{id}/environments/production");
    let req = |method: &str, path: &str, accept: &str, body: Option<String>| {
        let r = Request::builder().method(method).uri(path).header("host", h.clone()).header("accept", accept).header("cf-ipcountry", "de").body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap();
        let public = w.public.clone();
        async move {
            let resp = public.oneshot(r).await.unwrap();
            (resp.status(), String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    assert!(!req("GET", "/page", "text/html", None).await.1.contains("vitals.js"), "off by default");
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({"vitals": true}))).await;
    assert_eq!((st, &e["vitals"]), (StatusCode::OK, &json!(true)), "{e}");
    let (st, page) = req("GET", "/page", "text/html", None).await;
    assert!(st == StatusCode::OK && page.ends_with(r#"<script src="/__roll/vitals.js" defer></script></body></html>"#), "{page}");
    let (st, js) = req("GET", "/__roll/vitals.js", "*/*", None).await;
    assert!(st == StatusCode::OK && js.contains("PerformanceObserver") && js.contains("/__roll/vitals"), "{js}");
    for (path, lcp, cls) in [("/products/1", 1200, 0.01), ("/products/2", 1800, 0.02), ("/products/3", 3100, 0.3), ("/products/4", 5200, 0.05), ("/", 900, 0.0)] {
        let (st, _) = req("POST", "/__roll/vitals", "*/*", Some(json!({ "path": path, "lcp": lcp, "cls": cls, "ttfb": 120 }).to_string())).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
    }
    assert_eq!(req("POST", "/__roll/vitals", "*/*", Some(r#"{"path":"/x"}"#.into())).await.0, StatusCode::BAD_REQUEST, "nothing measured");
    let (_, v) = call(&w.api, "GET", &format!("{env}/vitals?minutes=60"), None).await;
    assert_eq!(v["samples"], 5, "{v}");
    assert_eq!(v["metrics"]["lcp"], json!({ "p75": 3100.0, "rating": "needs-improvement", "samples": 5 }), "{v}");
    assert_eq!(v["metrics"]["cls"]["rating"], "good", "{v}");
    assert_eq!((v["routes"][0]["route"].as_str(), v["routes"][0]["samples"].as_u64()), (Some("/products/:id"), Some(4)), "{v}");
    call(&w.api, "PUT", &env, Some(json!({"vitals": false}))).await;
    assert!(!req("GET", "/page", "text/html", None).await.1.contains("vitals.js"));
    assert_eq!(req("POST", "/__roll/vitals", "*/*", Some(json!({"path": "/", "lcp": 1}).to_string())).await.0, StatusCode::OK, "off, the path is the app's own");
}

/// Signing in to an environment: a page load with no session goes to roll's dashboard; a person
/// signed in there gets a one-use ticket to a page at one of the environment's names (a token
/// gets none); the ticket opens a host-only session, which the app never sees; changing the
/// environment's access ends it, and so does the person losing the project.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_in_person_gets_into_a_gated_environment() {
    let w = world_with(|c| {
        c.web_url = Some("https://roll.test".into());
        c.visitor_recheck = Duration::ZERO;
    })
    .await;
    struct Allowed(std::sync::atomic::AtomicBool, std::sync::atomic::AtomicBool);
    impl apps_control::integrations::Visitors for Allowed {
        fn may_open(&self, user: &str, _project: &str) -> bool {
            user == "usr_ada" && self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn session_live(&self, _user: &str, session: &str) -> bool {
            session == "ses_1" && self.1.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    let allowed = Arc::new(Allowed(true.into(), true.into()));
    w.apps.set_visitors(allowed.clone());
    let (svc, _) = image_service(&w, "admin", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let h = host(&svc["environments"][0]["url"]);
    let env = format!("/v1/apps/services/{id}/environments/production");
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({"access": {"signed_in": true}}))).await;
    assert_eq!((st, &e["private"], &e["signed_in"]), (StatusCode::OK, &json!(true), &json!(true)), "{e}");
    let get = |path: String, cookie: Option<String>, html: bool| {
        let mut r = Request::builder().uri(path).header("host", h.clone());
        if html {
            r = r.header("accept", "text/html");
        }
        if let Some(c) = cookie {
            r = r.header("cookie", c);
        }
        let public = w.public.clone();
        async move {
            let resp = public.oneshot(r.body(Body::empty()).unwrap()).await.unwrap();
            let (st, hd) = (resp.status(), resp.headers().clone());
            let b = resp.into_body().collect().await.unwrap().to_bytes();
            (st, hd, serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null))
        }
    };
    let (st, hd, _) = get("/dash?x=1".into(), None, true).await;
    assert_eq!(st, StatusCode::FOUND);
    let to = format!("http://{h}/dash?x=1");
    let q = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
    assert_eq!(hd["location"].to_str().unwrap(), format!("https://roll.test/dashboard/apps/visit?service={}&env=production&to={}", q(&id), q(&to)));
    assert_eq!(get("/api".into(), None, false).await.0, StatusCode::UNAUTHORIZED, "an API call isn't sent to sign in");
    // A ticket, for a person only, to the environment's own names only.
    let ask = |user: Option<&str>, to: String| {
        let mut req = Request::builder().method("POST").uri(format!("{env}/visit")).header("content-type", "application/json").body(Body::from(json!({ "to": to }).to_string())).unwrap();
        if let Some(u) = user {
            req.extensions_mut().insert(roll_auth::SignedIn { user: u.into(), handle: "ada".into(), admin: true, code_owner: None, session: "ses_1".into() });
        }
        let api = w.api.clone();
        async move {
            let resp = api.oneshot(req).await.unwrap();
            let st = resp.status();
            (st, serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
        }
    };
    assert_eq!(ask(None, to.clone()).await.0, StatusCode::FORBIDDEN);
    assert_eq!(ask(Some("usr_ada"), "https://evil.example/".into()).await.0, StatusCode::BAD_REQUEST);
    // The ticket goes to the environment's own scheme and port, whatever `to` names: never over
    // another scheme, nor to whatever listens on another port of the address.
    let origin = svc["environments"][0]["url"].as_str().unwrap().trim_end_matches('/').to_string();
    let (_, t) = ask(Some("usr_ada"), format!("https://{h}:9999/dash?x=1")).await;
    assert!(t["url"].as_str().unwrap().starts_with(&format!("{origin}/__roll/visit?ticket=")), "{t} vs {origin}");
    let (st, t) = ask(Some("usr_ada"), to.clone()).await;
    assert_eq!(st, StatusCode::OK, "{t}");
    let url = t["url"].as_str().unwrap();
    let path = &url[url.find("/__roll/visit").unwrap()..];
    let (st, hd, _) = get(path.to_string(), None, true).await;
    assert_eq!((st, hd["location"].to_str().unwrap()), (StatusCode::FOUND, "/dash?x=1"));
    let set = hd["set-cookie"].to_str().unwrap().to_string();
    assert!(set.contains("HttpOnly") && set.contains("Max-Age=43200"), "{set}");
    let session = set.split(';').next().unwrap().to_string();
    assert_eq!(get(path.to_string(), None, true).await.0, StatusCode::UNAUTHORIZED, "a ticket opens once");
    let (st, _, body) = get("/dash".into(), Some(format!("{session}; theirs=1; roll_access_production=a-shared-token")), true).await;
    assert_eq!((st, body["cookie"].as_str()), (StatusCode::OK, Some("theirs=1")), "the app never sees roll's cookie: {body}");
    assert_eq!(ask(Some("usr_bob"), to.clone()).await.0, StatusCode::FORBIDDEN, "someone who can't open the project gets no ticket");
    // The person loses the project: their session ends at the next look.
    allowed.0.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(get("/dash".into(), Some(session.clone()), true).await.0, StatusCode::FOUND);
    allowed.0.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(get("/dash".into(), Some(session), true).await.0, StatusCode::FOUND, "it ended for good");
    // New access, new sign-in.
    let (_, t) = ask(Some("usr_ada"), to.clone()).await;
    let url = t["url"].as_str().unwrap();
    let (_, hd, _) = get(url[url.find("/__roll/visit").unwrap()..].to_string(), None, true).await;
    let session = hd["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    assert_eq!(get("/dash".into(), Some(session.clone()), true).await.0, StatusCode::OK);
    // The roll login that let them in ends (signed out, or revoked): so does the session.
    allowed.1.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(get("/dash".into(), Some(session.clone()), true).await.0, StatusCode::FOUND, "a revoked login's session ends");
    allowed.1.store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, t) = ask(Some("usr_ada"), to.clone()).await;
    let url = t["url"].as_str().unwrap();
    let (_, hd, _) = get(url[url.find("/__roll/visit").unwrap()..].to_string(), None, true).await;
    let session = hd["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    call(&w.api, "PUT", &env, Some(json!({"access": {"signed_in": true}}))).await;
    assert_eq!(get("/dash".into(), Some(session), true).await.0, StatusCode::FOUND);
}

/// Errors grouped from the log (Cloud Run's Error Reporting): a trace's lines are one error, the
/// same error at another line is one group, the newest is its sample.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errors_group_from_the_environments_log() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let log = w.apps.logs.sink(format!("env:{id}/production"), apps_control::logs::Stream::Stderr).instance("roll-apps-x-0", "dep_x");
    for line in [10, 42] {
        for l in ["Traceback (most recent call last):".to_string(), format!("  File \"/app/main.py\", line {line}, in charge"), "ZeroDivisionError: division by zero".into()] {
            log.line(l);
        }
    }
    log.line(r#"{"level":"error","msg":"payment 17 failed"}"#);
    let (st, e) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/errors?minutes=60"), None).await;
    assert_eq!(st, StatusCode::OK, "{e}");
    let g = &e["groups"][0];
    assert_eq!((g["kind"].as_str(), g["title"].as_str(), g["count"].as_u64(), g["instances"].as_u64()), (Some("python"), Some("ZeroDivisionError: division by zero"), Some(2), Some(1)), "{e}");
    assert!(g["sample"].as_str().is_some_and(|s| s.contains("line 42")), "the newest is the sample: {e}");
    assert_eq!((e["groups"][1]["kind"].as_str(), e["groups"][1]["title"].as_str()), (Some("line"), Some("payment 17 failed")), "{e}");
}

/// A new error tells someone: the first time a fingerprint appears it's an `error.new` event
/// (its type, never its message), once ever; another error is another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_error_is_an_event_once() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let live = w.apps.live(&id, "production");
    let log = w.apps.logs.sink(format!("env:{id}/production"), apps_control::logs::Stream::Stderr).instance("roll-apps-x-0", "dep_x");
    let trace = |err: &str| ["Traceback (most recent call last):".to_string(), "  File \"/app/pay.py\", line 9, in charge".into(), format!("{err}: card 4242424242424242 declined")];
    let news = || async {
        call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "error.new").cloned().collect::<Vec<_>>()
    };
    w.apps.note_new_errors(&live).await;
    for l in trace("ValueError") {
        log.line(l);
    }
    w.apps.note_new_errors(&live).await;
    let first = news().await;
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0]["data"]["name"], "ValueError");
    assert!(!first[0].to_string().contains("4242424242424242"), "never the message: {}", first[0]);
    // The same error again: no second event, even as if this process had restarted.
    tokio::time::sleep(Duration::from_millis(5)).await;
    for l in trace("ValueError") {
        log.line(l);
    }
    w.apps.note_new_errors(&live).await;
    assert_eq!(news().await.len(), 1);
    tokio::time::sleep(Duration::from_millis(5)).await;
    for l in trace("KeyError") {
        log.line(l);
    }
    w.apps.note_new_errors(&live).await;
    assert_eq!(news().await.len(), 2);
}

/// Secret files (Render's): a variable kept as a file is out of the instance's environment and
/// in its files at its path; paths are checked; null puts it back, and unsetting it drops both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_variable_kept_as_a_file_reaches_the_instance_there() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let env = format!("/v1/apps/services/{id}/environments/production");
    let vars = format!("{env}/variables");
    for bad in ["etc/key", "/proc/self/x", "/etc/../x", "/etc/secrets/", "/a\nb"] {
        assert_eq!(call(&w.api, "PATCH", &vars, Some(json!({"set": {"KEY": "x"}, "files": {"KEY": bad}}))).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(call(&w.api, "PATCH", &vars, Some(json!({"files": {"NOPE": "/etc/x"}}))).await.0, StatusCode::BAD_REQUEST, "not a variable");
    let key = r#"{"type":"service_account","private_key":"-----BEGIN KEY-----\nabc\n-----END KEY-----"}"#;
    let (st, v) = call(&w.api, "PATCH", &vars, Some(json!({"set": {"GCP_KEY": key, "PLAIN": "1"}, "seal": ["GCP_KEY"], "files": {"GCP_KEY": "/etc/secrets/gcp.json"}}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let row = v["vars"].as_array().unwrap().iter().find(|x| x["name"] == "GCP_KEY").cloned().unwrap();
    assert_eq!((row["file"].as_str(), row["sealed"].as_bool()), (Some("/etc/secrets/gcp.json"), Some(true)), "{v}");
    // The live deployment's running instance (the one before may still be draining).
    let live = |w: &World, dep: &str| {
        let name = w.fake.instances(&id).into_iter().find(|(_, d, s)| d == dep && *s == apps_control::runtime::fake::FakeState::Running).map(|(n, _, _)| n).expect("a running instance");
        w.fake.spec(&name).unwrap()
    };
    let (_, r) = call(&w.api, "POST", &format!("{env}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let spec = live(&w, r["id"].as_str().unwrap());
    assert_eq!(spec.files, vec![("/etc/secrets/gcp.json".to_string(), key.to_string())]);
    assert!(!spec.vars.iter().any(|(k, _)| k == "GCP_KEY") && spec.vars.iter().any(|(k, _)| k == "PLAIN"), "a file isn't in the environment too");
    // Back into the environment, then gone with its variable.
    call(&w.api, "PATCH", &vars, Some(json!({"files": {"GCP_KEY": null}}))).await;
    let (_, r) = call(&w.api, "POST", &format!("{env}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let spec = live(&w, r["id"].as_str().unwrap());
    assert!(spec.files.is_empty() && spec.vars.iter().any(|(k, _)| k == "GCP_KEY"));
    call(&w.api, "PATCH", &vars, Some(json!({"files": {"PLAIN": "/run/plain"}}))).await;
    let (_, v) = call(&w.api, "PATCH", &vars, Some(json!({"unset": ["PLAIN"]}))).await;
    assert!(v.to_string().find("/run/plain").is_none(), "{v}");
    assert!(w.apps.store.variable_files(&id, "production").unwrap().is_empty());
}

/// Egress rules: an environment with `egress` `roll` runs on a network with no route out, its
/// listed hosts through its proxy (the variables HTTP clients read, the project's services
/// reached directly), what the proxy refused shown; hosts need `roll` and are names; opened
/// again, the next deploy is back on the project's network.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_egress_runs_on_its_own_network_with_a_proxy() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let env = format!("/v1/apps/services/{id}/environments/production");
    for bad in [json!({"mode": "open", "hosts": ["api.stripe.com"]}), json!({"mode": "roll", "hosts": ["10.0.0.1"]}), json!({"mode": "roll", "hosts": ["*"]}), json!({"mode": "shut"})] {
        assert_eq!(call(&w.api, "PUT", &env, Some(json!({"egress": bad}))).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({"egress": {"mode": "roll", "hosts": ["API.Stripe.com", "*.sentry.io"]}}))).await;
    assert_eq!((st, &e["egress"]), (StatusCode::OK, &json!({"mode": "roll", "hosts": ["*.sentry.io", "api.stripe.com"]})), "{e}");
    // The live deployment's running instance (the one before may still be draining).
    let live = |w: &World, dep: &str| {
        let name = w.fake.instances(&id).into_iter().find(|(_, d, s)| d == dep && *s == apps_control::runtime::fake::FakeState::Running).map(|(n, _, _)| n).expect("a running instance");
        w.fake.spec(&name).unwrap()
    };
    let (_, r) = call(&w.api, "POST", &format!("{env}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let spec = live(&w, r["id"].as_str().unwrap());
    let closed = apps_control::closed_network_name(&w.project, "production");
    assert_eq!((spec.closed, spec.network.as_deref()), (true, Some(closed.as_str())));
    assert!(w.fake.closed_networks().contains(&closed));
    let var = |k: &str| spec.vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let hosts = vec!["*.sentry.io".to_string(), "api.stripe.com".to_string()];
    assert_eq!(var("HTTPS_PROXY"), Some(apps_control::runtime::egress::proxy_url(&closed, &hosts)));
    assert!(var("NO_PROXY").is_some_and(|n| n.split(',').any(|h| h == "api")), "the project's services are reached directly: {:?}", var("NO_PROXY"));
    assert_eq!(w.fake.proxies.lock().last().map(|(n, h)| (n.clone(), h.clone())), Some((closed.clone(), hosts)));
    w.fake.refuse("evil.example.com", false);
    w.fake.refuse("internal.example.com", true);
    assert_eq!(call(&w.api, "GET", &env, None).await.1["egress_refused"], json!([{"host": "evil.example.com"}, {"host": "internal.example.com", "private": true}]));
    // Opened: the next deploy is back on the project's network, with no proxy.
    let (_, e) = call(&w.api, "PUT", &env, Some(json!({"egress": null}))).await;
    assert_eq!(e["egress"], json!({"mode": "open", "hosts": []}), "{e}");
    let (_, r) = call(&w.api, "POST", &format!("{env}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let spec = live(&w, r["id"].as_str().unwrap());
    assert_eq!((spec.closed, spec.network), (false, Some(apps_control::network_name(&w.project, "production"))));
    assert!(!spec.vars.iter().any(|(k, _)| k == "HTTPS_PROXY"));
}

/// Visitors and page views from the access log: page loads only (not an API's calls), a visitor
/// an address and browser a day, the referring site's host (never its own), the edge's country.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analytics_count_page_views_and_visitors() {
    let w = world().await;
    let (svc, _) = image_service(&w, "site", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let h = host(&svc["environments"][0]["url"]);
    let visit = |path: &'static str, ua: &'static str, html: bool, referer: Option<String>| {
        let mut r = Request::builder().uri(path).header("host", h.clone()).header("user-agent", ua).header("cf-ipcountry", "DE");
        r = r.header("accept", if html { "text/html,application/xhtml+xml" } else { "application/json" });
        if let Some(rf) = referer {
            r = r.header("referer", rf);
        }
        let req = r.body(Body::empty()).unwrap();
        let public = w.public.clone();
        async move { public.oneshot(req).await.unwrap().status() }
    };
    assert_eq!(visit("/", "browser-a", true, Some("https://news.example.com/item?id=1".into())).await, StatusCode::OK);
    visit("/", "browser-a", true, None).await;
    visit("/about", "browser-a", true, Some(format!("http://{h}/"))).await;
    // A phone, from a campaign's link (only its utm_* are read, never the rest of the query).
    let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
    visit("/?utm_source=Newsletter&utm_campaign=launch&token=s3cret", iphone, true, Some("https://www.news.example.com/".into())).await;
    visit("/api/items", iphone, false, None).await;
    // Custom events (`roll.track`): kept with their bounded properties, read back by name.
    let event = |body: &'static str| {
        let req = Request::builder().method("POST").uri("/__roll/event").header("host", h.clone()).header("content-type", "application/json").body(Body::from(body)).unwrap();
        let public = w.public.clone();
        async move { public.oneshot(req).await.unwrap().status() }
    };
    assert_eq!(event(r#"{"name":"Signup","props":{"plan":"pro"},"path":"/pricing"}"#).await, StatusCode::NO_CONTENT);
    assert_eq!(event(r#"{"name":"Signup","props":{"plan":"pro"}}"#).await, StatusCode::NO_CONTENT);
    assert_eq!(event(r#"{"name":"Signup","props":{"plan":"free"}}"#).await, StatusCode::NO_CONTENT);
    assert_eq!(event(r#"{"name":"Signup","props":{"plan":{"nested":1}}}"#).await, StatusCode::BAD_REQUEST);
    let script = w.public.clone().oneshot(Request::builder().uri("/__roll/track.js").header("host", h.clone()).body(Body::empty()).unwrap()).await.unwrap();
    assert!(script.status() == StatusCode::OK && script.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"));
    let mut a = Value::Null;
    for _ in 0..100 {
        a = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/analytics?minutes=60"), None).await.1;
        if a["page_views"] == 4 && a["events"][0]["count"] == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!((a["page_views"].as_u64(), a["visitors"].as_u64()), (Some(4), Some(2)), "{a}");
    assert_eq!(a["pages"][0], json!({"path": "/", "views": 3, "visitors": 2}), "{a}");
    assert_eq!(a["referrers"], json!([{"host": "news.example.com", "views": 2}]), "its own host isn't a referrer: {a}");
    assert_eq!(a["countries"], json!([{"country": "DE", "views": 4}]), "{a}");
    assert_eq!(a["campaigns"], json!([{"source": "newsletter", "medium": "", "campaign": "launch", "views": 1, "visitors": 1}]), "{a}");
    assert_eq!((&a["devices"]["classes"], &a["devices"]["os"][0]), (&json!([{"class": "desktop", "views": 3}, {"class": "mobile", "views": 1}]), &json!({"os": "Other", "views": 3})), "{a}");
    assert_eq!(a["events"], json!([{"name": "Signup", "count": 3, "props": {"plan": [{"value": "pro", "count": 2}, {"value": "free", "count": 1}]}}]), "{a}");
    let access: Vec<String> = w.apps.logs.follow(&format!("http:{id}/production"), 50).0.into_iter().map(|l| l.text).collect();
    assert!(access.iter().any(|l| l.contains("\"utm\"")) && !access.iter().any(|l| l.contains("s3cret")), "the query's other parameters are never kept: {access:?}");
}

/// An answer its app lets a shared cache keep (`s-maxage`) is answered from the owner's cache
/// until it's stale, without reaching the app; another query is another answer, a request with
/// `Authorization` never reads it, and a new live deployment empties it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_cache_answers_without_reaching_the_app() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "cached", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": {"FAKE_CACHE": "public, s-maxage=60"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let h = host(&svc["environments"][0]["url"]);
    let get = |path: &'static str, auth: bool| {
        let mut r = Request::builder().uri(path).header("host", h.clone());
        if auth {
            r = r.header("authorization", "Bearer x");
        }
        let req = r.body(Body::empty()).unwrap();
        let public = w.public.clone();
        async move {
            let resp = public.oneshot(req).await.unwrap();
            let cache = resp.headers().get("x-roll-cache").map(|v| v.to_str().unwrap().to_string());
            assert!(!resp.headers().contains_key("cache-tag"), "an answer's cache tags are roll's");
            let body = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
            (cache, body)
        }
    };
    let count = || async { get("/count", false).await.1.parse::<u64>().unwrap() };
    let first = get("/page-a", false).await;
    assert_eq!(first.0.as_deref(), Some("MISS"), "{first:?}");
    let before = count().await;
    let again = get("/page-a", false).await;
    assert_eq!((again.0.as_deref(), &again.1), (Some("HIT"), &first.1));
    assert_eq!(count().await, before, "the app wasn't asked");
    assert_eq!(get("/page-a?b=1", false).await.0.as_deref(), Some("MISS"), "another query, another answer");
    assert_eq!(get("/page-a", true).await.0, None, "Authorization goes past it");
    // Purged on demand (an app revalidating a page): that path's answers, then everything.
    let purge = format!("/v1/apps/services/{id}/environments/production/cache/purge");
    assert_eq!(call(&w.api, "POST", &purge, Some(json!({"paths": ["/page-*"]}))).await.1, json!({"purged": 2}));
    assert_eq!(get("/page-a", false).await.0.as_deref(), Some("MISS"));
    // By the tags its app gave it (`Cache-Tag`), whatever their case; the others stay.
    get("/page-b", false).await;
    assert_eq!(call(&w.api, "POST", &purge, Some(json!({"tags": ["PAGE-B"]}))).await.1, json!({"purged": 1}));
    assert_eq!(get("/page-a", false).await.0.as_deref(), Some("HIT"));
    assert_eq!(call(&w.api, "POST", &purge, None).await.1, json!({"purged": 1}));
    // A new live deployment's answers aren't the old one's.
    let (_, r) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    assert_eq!(get("/page-a", false).await.0.as_deref(), Some("MISS"));
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
    let echo = json!({"FAKE_ECHO": "ROLL_GIT_COMMIT,ROLL_GIT_BRANCH,ROLL_INSTANCE"});
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"}, "variables": echo }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let first = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    let d1 = settled(&w, &id, "production", &first).await;
    assert_eq!(d1["status"], "active", "{d1}");
    assert_eq!(d1["revision"], "a1a1a1a1a1a1");
    assert_eq!(d1["plan"]["provider"], "node", "{d1}");
    let steps: Vec<&str> = d1["steps"].as_array().unwrap().iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["build", "rollout", "live"], "{d1}");
    // The app knows its commit, its branch and which instance it is.
    let (_, body) = visit_deployment(&w, &host(&svc["environments"][0]["url"]), &first).await;
    assert_eq!((&body["vars"]["ROLL_GIT_COMMIT"], &body["vars"]["ROLL_GIT_BRANCH"]), (&json!("a1a1a1a1a1a1"), &json!("main")), "{body}");
    assert!(w.fake.instances(&id).iter().any(|(n, ..)| body["vars"]["ROLL_INSTANCE"] == n.as_str()), "{body}");
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
    // A push whose message says `[skip ci]` isn't deployed, and the timeline says why.
    w.code.push("c3c3c3c3c3c3", "docs: a typo\n\n[skip ci]", &[("README.md", "hi")]);
    let skipped = get_until(&w.api, &format!("/v1/apps/services/{id}/activity?all=1"), |a| a.as_array().is_some_and(|a| a.iter().any(|e| e["kind"] == "deploy.skipped"))).await;
    assert!(skipped.to_string().contains("c3c3c3c3c3c3"), "{skipped}");
    let (_, all) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys"), None).await;
    assert!(!all.to_string().contains("c3c3c3c3c3c3"), "{all}");
    // A rollback runs the old image again: no build.
    let builds = Fake::count(&w.fake.counts.builds);
    let (st, rb) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys/{first}/rollback"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{rb}");
    let rb = settled(&w, &id, "production", rb["id"].as_str().unwrap()).await;
    assert_eq!((rb["status"].as_str(), rb["rollback_of"].as_str(), rb["image"].clone()), (Some("active"), Some(first.as_str()), d1["image"].clone()));
    assert_eq!(Fake::count(&w.fake.counts.builds), builds);
    // The rollback holds pushes (a_rollback_holds_the_next_push_until_deploys_resume): resumed.
    assert_eq!(call(&w.api, "PUT", &format!("/v1/apps/services/{id}/environments/production"), Some(json!({"held": false}))).await.0, StatusCode::OK);
    // Watching web/: a push elsewhere in the monorepo isn't deployed, one into web/ is.
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"build": {"target": "--no-cache"}}))).await.0, StatusCode::BAD_REQUEST, "a stage's name, never a flag");
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

/// Each environment keeps its newest built deployments to roll back to; older ones are pruned
/// (history only, their images gone unless another deployment runs the same one), a rollback to
/// one is refused with why, and the list pages back through all of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_deployments_are_pruned_past_what_an_environment_keeps() {
    let w = world_with(|c| {
        c.keep_deployments = 2;
        c.keep_recent = Duration::ZERO;
    })
    .await;
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    let id = svc["id"].as_str().unwrap_or_else(|| panic!("{svc}")).to_string();
    let base = format!("/v1/apps/services/{id}/environments/production/deploys");
    let mut deps = vec![svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string()];
    settled(&w, &id, "production", &deps[0]).await;
    // Each built anew (the same tree's image would be reused otherwise).
    for _ in 0..3 {
        let (_, d) = call(&w.api, "POST", &base, Some(json!({"no_cache": true}))).await;
        let d = settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
        assert_eq!(d["status"], "active", "{d}");
        deps.push(d["id"].as_str().unwrap().to_string());
    }
    let mut images = Vec::new();
    for d in &deps {
        images.push(call(&w.api, "GET", &format!("{base}/{d}"), None).await.1["image"].as_str().unwrap().to_string());
    }
    let (old0, old1, kept) = (images[0].clone(), images[1].clone(), images[2].clone());
    // Builds that never went live (failed health checks) don't push the rollback targets out.
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"health_path": "/healthz", "health_timeout_s": 1}}))).await;
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}/environments/production/variables"), Some(json!({"set": {"FAKE_UNHEALTHY": "1"}}))).await;
    for _ in 0..2 {
        let (_, d) = call(&w.api, "POST", &base, Some(json!({}))).await;
        assert_eq!(settled(&w, &id, "production", d["id"].as_str().unwrap()).await["status"], "failed");
    }
    w.apps.prune().await;
    let removed = w.fake.removed.lock().clone();
    assert!(removed.contains(&old0) && removed.contains(&old1) && !removed.contains(&kept), "{removed:?}");
    let (_, d0) = call(&w.api, "GET", &format!("{base}/{}", deps[0]), None).await;
    assert!(d0["pruned_at"].is_i64(), "{d0}");
    let (st, r) = call(&w.api, "POST", &format!("{base}/{}/rollback", deps[0]), Some(json!({}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert_eq!(call(&w.api, "POST", &format!("{base}/{}/rollback", deps[2]), Some(json!({}))).await.0, StatusCode::ACCEPTED, "a kept one rolls back");
    // Paged back with `before`: every one, newest first.
    let (_, page) = call(&w.api, "GET", &base, None).await;
    let last = page.as_array().unwrap().last().unwrap()["created_at"].as_i64().unwrap();
    let (_, older) = call(&w.api, "GET", &format!("{base}?before={last}"), None).await;
    assert!(older.as_array().unwrap().is_empty() && page.as_array().unwrap().len() == 7, "{page}");
    // Pruned once: a second pass removes nothing again.
    w.apps.prune().await;
    assert_eq!(w.fake.removed.lock().len(), removed.len());
}

/// An app that doesn't answer on its port says where it listens instead: on its port but only on
/// loopback fails at once (nothing can ever reach it); on another port, the timeout says which.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_whose_app_listens_elsewhere_says_where() {
    let w = world().await;
    let deploy = |name: &'static str, listen: &'static str| {
        let w = &w;
        async move {
            let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": name, "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 8080, "min_instances": 1, "health_timeout_s": 4}, "variables": {"FAKE_LISTEN": listen} }))).await;
            assert_eq!(st, StatusCode::CREATED, "{svc}");
            let t = Instant::now();
            let d = settled(w, svc["id"].as_str().unwrap(), "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
            (d, t.elapsed())
        }
    };
    let (d, took) = deploy("local", "127.0.0.1:8080").await;
    assert_eq!(d["status"], "failed", "{d}");
    assert!(d["error"].as_str().unwrap().contains("listens on 127.0.0.1:8080 only") && d["error"].as_str().unwrap().contains("0.0.0.0:8080"), "{d}");
    assert!(took < Duration::from_secs(4), "not the whole health timeout: {took:?}");
    let (d, _) = deploy("other", "0.0.0.0:3000").await;
    assert_eq!(d["status"], "failed", "{d}");
    assert!(d["error"].as_str().unwrap().contains("listens on 0.0.0.0:3000, not on port 8080"), "{d}");
}

/// One instance is replaced on its own: a fresh one answers before it leaves, the others stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_instance_is_restarted_without_the_rest() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80, "min_instances": 2})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let running = |v: &Value| v["instances"].as_array().unwrap().iter().filter(|i| i["state"] == "running").map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let env = get_until(&w.api, &base, |v| running(v).len() == 2).await;
    let (old, kept) = (running(&env)[0].clone(), running(&env)[1].clone());
    let (st, after) = call(&w.api, "POST", &format!("{base}/instances/{old}/restart"), None).await;
    assert_eq!(st, StatusCode::OK, "{after}");
    let now = running(&after);
    assert!(!now.contains(&old) && now.contains(&kept) && now.len() == 2, "{now:?}");
    assert_eq!(call(&w.api, "POST", &format!("{base}/instances/nope/restart"), None).await.0, StatusCode::NOT_FOUND);
    // One with a volume stops first: never two on the volume at once.
    let (svc, _) = image_service(&w, "db", json!({"port": 80, "min_instances": 1, "volume": {"mount": "/data", "size_gb": 1}})).await;
    let vid = svc["id"].as_str().unwrap().to_string();
    let vbase = format!("/v1/apps/services/{vid}/environments/production");
    let env = get_until(&w.api, &vbase, |v| running(v).len() == 1).await;
    let old = running(&env)[0].clone();
    let watch = {
        let (fake, vid, stop) = (w.fake.clone(), vid.clone(), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (s2, st2) = (seen.clone(), stop.clone());
        tokio::spawn(async move {
            while !st2.load(Ordering::SeqCst) {
                let n = fake.instances(&vid).iter().filter(|(_, _, st)| *st == FakeState::Running).count();
                s2.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        (seen, stop)
    };
    assert_eq!(call(&w.api, "POST", &format!("{vbase}/instances/{old}/restart"), None).await.0, StatusCode::OK);
    let after = get_until(&w.api, &vbase, |v| running(v).len() == 1).await;
    watch.1.store(true, Ordering::SeqCst);
    assert!(!running(&after).contains(&old), "{after}");
    assert_eq!(watch.0.load(Ordering::SeqCst), 1, "never two instances on the volume");
}

/// An environment's own headers (HSTS, a CSP) on its answers, by path; the app's own header of
/// the same name wins unless the rule replaces it; nothing that frames the answer or is roll's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environments_headers_are_on_its_answers() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80, "min_instances": 1})).await;
    let base = format!("/v1/apps/services/{}/environments/production", svc["id"].as_str().unwrap());
    let rules = json!([
        {"set": {"Strict-Transport-Security": "max-age=63072000", "Content-Type": "text/plain"}},
        {"path": "/api/*", "set": {"Cache-Control": "no-store", "Content-Type": "application/problem+json"}, "replace": true},
    ]);
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"headers": rules}))).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert_eq!(env["headers"][0]["set"]["strict-transport-security"], "max-age=63072000", "kept lower case: {env}");
    let get = |path: &str| {
        let req = Request::builder().uri(path).header("host", host(&env["url"]).as_str()).body(Body::empty()).unwrap();
        w.public.clone().oneshot(req)
    };
    let r = get("/").await.unwrap();
    let h = |r: &axum::response::Response, k: &str| r.headers().get(k).map(|v| v.to_str().unwrap().to_string());
    assert_eq!(h(&r, "strict-transport-security").as_deref(), Some("max-age=63072000"));
    assert_eq!(h(&r, "content-type").as_deref(), Some("application/json"), "the app's own wins");
    assert_eq!(h(&r, "cache-control"), None, "not on this path");
    let r = get("/API/users").await.unwrap();
    assert_eq!((h(&r, "cache-control").as_deref(), h(&r, "content-type").as_deref()), (Some("no-store"), Some("application/problem+json")), "replaced");
    for bad in [json!({"set": {"Set-Cookie": "a=b"}}), json!({"set": {"X-Roll-Via": "x"}}), json!({"set": {"X-A": "a\r\nX-B: b"}}), json!({"path": "api", "set": {"X-A": "a"}})] {
        assert_eq!(call(&w.api, "PUT", &base, Some(json!({"headers": [bad.clone()]}))).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (_, env) = call(&w.api, "PUT", &base, Some(json!({"headers": null}))).await;
    assert!(env.get("headers").is_none(), "{env}");
}

/// A GET that finds its instance gone (killed: nothing listens, and roll doesn't know yet) goes
/// to a fresh instance instead of a 503; a request with a body isn't sent twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_get_to_a_vanished_instance_goes_to_a_fresh_one() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80})).await;
    let h = host(&svc["environments"][0]["url"]);
    let log = || w.apps.logs.follow(&format!("env:{}/production", svc["id"].as_str().unwrap()), 50).0.iter().map(|l| l.text.clone()).collect::<Vec<_>>();
    let (st, body) = visit(&w, &h, "/").await;
    assert_eq!(st, StatusCode::OK, "{body} {:?}", log());
    // Production is indexed as the app says (roll adds no X-Robots-Tag there).
    let r = w.public.clone().oneshot(Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap()).await.unwrap();
    assert!(r.headers().get("x-robots-tag").is_none());
    let first = body["instance"].as_str().unwrap().to_string();
    w.fake.vanish(&first);
    let (st, body) = visit(&w, &h, "/").await;
    assert_eq!(st, StatusCode::OK, "{body} {:?}", log());
    let second = body["instance"].as_str().unwrap().to_string();
    assert_ne!(second, first);
    // Every running instance gone, roll not told: a POST that finds one is never sent again
    // (one that finds none starts a created one, and that's its only send).
    for (name, ..) in w.fake.instances(svc["id"].as_str().unwrap()) {
        w.fake.vanish(&name);
    }
    let resent = || log().iter().filter(|l| l.ends_with("the request goes to a fresh instance")).count();
    let before = resent();
    let post = Request::post("/").header("host", h.as_str()).header("content-length", "1").body(Body::from("x")).unwrap();
    let st = w.public.clone().oneshot(post).await.unwrap().status();
    assert!(matches!(st, StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY | StatusCode::OK), "{st}");
    assert_eq!(resent(), before, "not sent again: {:?}", log());
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
    // What it brought over the deployment live before it: both pushes, newest first.
    let (_, c) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{}/commits", fast["id"].as_str().unwrap()), None).await;
    let shas: Vec<&str> = c["commits"].as_array().unwrap().iter().map(|x| x["sha"].as_str().unwrap()).collect();
    assert_eq!((shas, c["complete"].as_bool(), c["since"]["revision"].as_str()), (vec!["c3c3c3c3c3c3", "b2b2b2b2b2b2"], Some(true), Some("a1a1a1a1a1a1")), "{c}");
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
    // Lists say so too (the canvas's cards and the project's staged changes read them).
    let (_, list) = call(&w.api, "GET", "/v1/apps/services", None).await;
    assert_eq!(list[0]["environments"][0]["needs_deploy"], true, "{list}");
    let (_, d2) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d2 = settled(&w, id, "production", d2["id"].as_str().unwrap()).await;
    assert_eq!(d2["status"], "failed", "{d2}");
    assert!(d2["error"].as_str().unwrap().contains("health check"), "{d2}");
    // What the new instance said is in the deploy's log and its detail, not only the
    // environment's; never in the error that goes out (commit statuses, notifications).
    assert!(d2["error_detail"].as_str().unwrap_or("").contains("its last output: listening on"), "{d2}");
    assert!(!d2["error"].as_str().unwrap().contains("listening on"), "{d2}");
    let (_, dlog) = call(&w.api, "GET", &format!("{base}/deploys/{}/logs", d2["id"].as_str().unwrap()), None).await;
    assert!(dlog.as_array().unwrap().iter().any(|l| l["text"].as_str().unwrap_or("").contains(": listening on")), "{dlog}");
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
    // Given at creation, they're in production before its first deploy (a template's made
    // secret sealed from the start).
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "early", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": {"DATABASE_URL": "postgres://x", "APP_SECRET": "made-here"}, "sealed": ["APP_SECRET"] }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let sid = svc["id"].as_str().unwrap();
    let (_, values) = call(&w.api, "GET", &format!("/v1/apps/services/{sid}/environments/production/variables/values"), None).await;
    assert!(values.get("APP_SECRET").is_none() && values["DATABASE_URL"] == "postgres://x", "{values}");
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
    // Never indexed (a public preview, a share link): roll's 401, the link's redirect, the app's.
    let r = get("/", None).await.unwrap();
    assert_eq!((r.status(), r.headers().get("x-robots-tag").and_then(|v| v.to_str().ok())), (StatusCode::UNAUTHORIZED, Some("noindex")));
    assert_eq!(get("/?roll_access=wrongwrongwrongwrongwrong", None).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let r = get(&format!("/pricing?roll_access={token}&plan=pro"), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(r.headers()["x-robots-tag"], "noindex");
    assert_eq!(r.headers()["location"], "/pricing?plan=pro", "the token leaves the URL");
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set.starts_with(&format!("roll_access_pr-7={token};")) && set.contains("HttpOnly") && set.contains("SameSite=Lax") && !set.contains("Domain"), "{set}");
    let r = get("/pricing", Some(&format!("theme=dark; roll_access_pr-7={token}"))).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["x-robots-tag"], "noindex");
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
    // Pointed at the old host: the check says where it points, not only that it doesn't.
    w.dns.add("www.example.com", "CNAME", "old.herokudns.com");
    let (_, v) = call(&w.api, "POST", &format!("{base}/www.example.com/verify"), None).await;
    assert!(v["error"].as_str().unwrap().starts_with("www.example.com points at old.herokudns.com instead of"), "{v}");
    w.dns.add("www.example.com", "CNAME", d["target"].as_str().unwrap());
    let (_, v) = call(&w.api, "POST", &format!("{base}/www.example.com/verify"), None).await;
    assert_eq!(v["status"], "active", "{v}");
    let ev = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    assert!(ev["events"].as_array().unwrap().iter().any(|e| e["kind"] == "domain.active" && e["data"]["hostname"] == "www.example.com"), "{ev}");
    assert_eq!(visit(&w, "www.example.com", "/x").await.0, StatusCode::OK);
    // The certificate: the provider heard of the hostname only once it was accepted (never the
    // refused one), and verifying reads where its certificate is.
    assert_eq!((d["tls"].as_str(), d["tls_error"].as_str()), (Some("pending"), Some("waiting for HTTP validation")), "{d}");
    // A certificate that fails is an event once (checks that still find it failed aren't).
    w.tls.failed.lock().push(("www.example.com".into(), "CAA forbids the issuer".into()));
    for _ in 0..2 {
        call(&w.api, "POST", &format!("{base}/www.example.com/verify"), None).await;
    }
    let ev = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    let failed: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "domain.tls_failed").collect();
    assert!(failed.len() == 1 && failed[0]["data"]["error"] == "CAA forbids the issuer", "{ev}");
    w.tls.failed.lock().clear();
    w.tls.issued.lock().push("www.example.com".into());
    let (_, v) = call(&w.api, "POST", &format!("{base}/www.example.com/verify"), None).await;
    assert_eq!((v["tls"].as_str(), &v["tls_error"]), (Some("issued"), &Value::Null), "{v}");
    assert!(w.tls.calls.lock().iter().all(|c| c.ends_with(" www.example.com")), "{:?}", w.tls.calls.lock());
    assert_eq!(call(&w.api, "DELETE", &format!("{base}/www.example.com"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(w.tls.calls.lock().last().map(String::as_str), Some("remove www.example.com"));
    // One still unverified a day before it's removed: its owner is told, once, with why.
    call(&w.api, "POST", &base, Some(json!({"hostname": "shop.example.com"}))).await;
    call(&w.api, "POST", &format!("{base}/shop.example.com/verify"), None).await;
    assert_eq!(w.apps.db(|s| s.warn_domain_claims(i64::MAX)).await.unwrap(), 1);
    assert_eq!(w.apps.db(|s| s.warn_domain_claims(i64::MAX)).await.unwrap(), 0);
    let ev = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    let warned = ev["events"].as_array().unwrap().iter().find(|e| e["kind"] == "domain.expiring").cloned().unwrap_or_default();
    assert!(warned["data"]["error"].as_str().is_some_and(|e| e.contains("no CNAME")), "{warned}");
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
    // A preview (an ephemeral environment) gets no resource's production variables: roll
    // previews sets the branched ones itself.
    let pv = format!("/v1/apps/services/{id}/environments/pv-1");
    call(&w.api, "PUT", &pv, Some(json!({"ephemeral": true}))).await;
    let (_, v) = call(&w.api, "GET", &format!("{pv}/variables/values"), None).await;
    assert!(v.get("DATABASE_URL").is_none() && v.get("ORDERS_DATABASE_URL").is_none(), "{v}");
    let (_, v) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/variables/values"), None).await;
    assert!(v["DATABASE_URL"].is_string(), "{v}");
    call(&w.api, "DELETE", &pv, None).await;
    // A link that makes credentials of its own makes them once: setting it again keeps them.
    let (st, l) = call(&w.api, "PUT", &format!("{links}/search/logs"), Some(json!({}))).await;
    assert_eq!((st, &l["vars"]), (StatusCode::OK, &json!(["ELASTICSEARCH_URL", "ELASTICSEARCH_API_KEY"])), "{l}");
    let (_, l) = call(&w.api, "PUT", &format!("{links}/search/logs"), Some(json!({"prefix": "LOGS"}))).await;
    assert_eq!(l["vars"], json!(["LOGS_ELASTICSEARCH_URL", "LOGS_ELASTICSEARCH_API_KEY"]), "{l}");
    assert_eq!(*MINTED.lock(), [format!("link {id} key0")]);
    // Kept sealed: never in the database in the clear.
    let raw = [std::fs::read(w.apps.cfg.home.join("apps.db")).unwrap(), std::fs::read(w.apps.cfg.home.join("apps.db-wal")).unwrap_or_default()].concat();
    assert!(!raw.windows(11).any(|x| x == b"key0-secret"));
    let base = format!("/v1/apps/services/{id}/environments/production");
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_ECHO": "DATABASE_URL,ORDERS_DATABASE_URL,LOGS_ELASTICSEARCH_API_KEY"}}))).await;
    let (_, r) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let (st, body) = visit_deployment(&w, &host(&svc["environments"][0]["url"]), r["id"].as_str().unwrap()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["vars"], json!({"DATABASE_URL": "postgres://app:pw@db.roll.test:5432/main", "ORDERS_DATABASE_URL": "postgres://app:pw@db.roll.test:5432/orders", "LOGS_ELASTICSEARCH_API_KEY": "key0-secret"}));
    // Unlinking revokes them, and so does deleting the service.
    assert_eq!(call(&w.api, "DELETE", &format!("{links}/search/logs"), None).await.0, StatusCode::NO_CONTENT);
    call(&w.api, "PUT", &format!("{links}/search/logs"), Some(json!({}))).await;
    // (Deleted, it's kept for a restore with its credentials; deleted again, they go.)
    for _ in 0..2 {
        assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None).await.0, StatusCode::NO_CONTENT);
    }
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
    let path = format!("/v1/apps/services/{rid}/links/search/logs");
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

/// A static site's `_redirects` proxies a path to another site (Netlify's 200 proxies, Vercel's
/// external rewrites): the visitor's request sent on without roll's own headers and cookies, the
/// answer back as it came; a rule's condition decides whether it applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_site_proxies_to_another_site() {
    let w = world().await;
    let origin = axum::Router::new().fallback(|uri: axum::http::Uri, h: axum::http::HeaderMap| async move {
        let cookie = h.get("cookie").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
        let roll = h.keys().any(|k| k.as_str().starts_with("x-roll-"));
        ([("x-origin", "yes")], format!("origin {uri} cookie={cookie} roll={roll}"))
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, origin).into_future());
    *w.fake.site_redirects.lock() = format!("/ext/* http://{at}/v2/:splat 200\n/beta/* http://{at}/beta/:splat 200 Cookie=beta\n");
    w.code.push("f1f1f1f1f1f1", "site", &[("package.json", r#"{"name":"site","scripts":{"build":"vite build"},"devDependencies":{"vite":"5.0.0"}}"#)]);
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    let id = svc["id"].as_str().unwrap_or_else(|| panic!("{svc}")).to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let host = host(&svc["environments"][0]["url"]);
    let get = |path: &'static str, cookie: Option<&'static str>| {
        let (public, host) = (w.public.clone(), host.clone());
        async move {
            let mut r = Request::builder().uri(path).header("host", &host).header("x-roll-visitor", "forged");
            if let Some(c) = cookie {
                r = r.header("cookie", c);
            }
            let resp = public.oneshot(r.body(Body::empty()).unwrap()).await.unwrap();
            let (st, origin) = (resp.status(), resp.headers().get("x-origin").is_some());
            (st, origin, String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    let (st, from_origin, body) = get("/ext/users/7?a=1", Some("session=s1; roll_bucket=42")).await;
    assert_eq!((st, from_origin), (StatusCode::OK, true), "{body}");
    assert_eq!(body, "origin /v2/users/7?a=1 cookie=session=s1 roll=false", "its path and query; roll's cookie and headers stay here");
    // A condition: only with the cookie set.
    assert!(get("/beta/x", Some("beta=1")).await.2.starts_with("origin /beta/x"));
    assert!(!get("/beta/x", None).await.1, "not without it");
    // Metered and in the access log, as an external answer.
    let access: Vec<String> = w.apps.logs.follow(&format!("http:{id}/production"), 50).0.into_iter().map(|l| l.text).collect();
    assert!(access.iter().any(|l| l.contains("\"external\"")), "{access:?}");
}

/// A static site's marked form takes submissions (Netlify Forms): kept sealed per environment, a
/// honeypot's dropped, read back only through the reveal route, each one an event naming its
/// fields and never their values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_sites_form_keeps_its_submissions() {
    let w = world().await;
    w.code.push("f0f0f0f0f0f0", "site", &[("package.json", r#"{"name":"site","scripts":{"build":"vite build"},"devDependencies":{"vite":"5.0.0"}}"#)]);
    let (_, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    let id = svc["id"].as_str().unwrap_or_else(|| panic!("{svc}")).to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let host = host(&svc["environments"][0]["url"]);
    let post = |path: &'static str, ct: &'static str, body: String| {
        let (public, host) = (w.public.clone(), host.clone());
        async move {
            let resp = public.oneshot(Request::builder().method("POST").uri(path).header("host", &host).header("content-type", ct).body(Body::from(body)).unwrap()).await.unwrap();
            (resp.status(), resp.headers().get("location").map(|v| v.to_str().unwrap().to_string()))
        }
    };
    let form = "application/x-www-form-urlencoded";
    // Sent from its page: kept, and the sender goes to its action's page.
    assert_eq!(post("/contact.html", form, "email=ana%40example.com&message=hello+there".into()).await, (StatusCode::SEE_OTHER, Some("/thanks.html".into())));
    // A bot filled the honeypot: answered the same, never kept.
    assert_eq!(post("/contact.html", form, "email=x&bot-field=buy+now".into()).await.0, StatusCode::SEE_OTHER);
    assert_eq!(post("/contact.html", "multipart/form-data; boundary=x", "--x--".into()).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(post("/contact.html", form, format!("message={}", "x".repeat(70_000))).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(post("/index.html", form, "a=b".into()).await.0, StatusCode::METHOD_NOT_ALLOWED, "a page with no form takes no POST");
    assert_eq!(post("/index.html", "multipart/form-data; boundary=x", "--x--".into()).await.0, StatusCode::METHOD_NOT_ALLOWED, "nor is it a form's error");
    // Posted straight to its action (a script's fetch), by its form-name: the second one.
    assert_eq!(post("/thanks.html", form, "form-name=contact&email=bo%40example.com".into()).await.0, StatusCode::SEE_OTHER);
    let base = format!("/v1/apps/services/{id}/environments/production/forms");
    let (_, forms) = call(&w.api, "GET", &base, None).await;
    assert_eq!((forms[0]["name"].as_str(), forms[0]["submissions"].as_i64()), (Some("contact"), Some(2)), "{forms}");
    let (_, subs) = call(&w.api, "GET", &format!("{base}/contact/submissions"), None).await;
    assert_eq!(subs[0]["fields"], json!([{"name": "email", "value": "bo@example.com"}]), "the newest first: {subs}");
    // The page after the newest: the first one, whole.
    let (_, older) = call(&w.api, "GET", &format!("{base}/contact/submissions?before={}", subs[0]["id"].as_str().unwrap()), None).await;
    assert_eq!(older.as_array().map(|a| a.len()), Some(1), "{older}");
    assert_eq!(older[0]["fields"], json!([{"name": "email", "value": "ana@example.com"}, {"name": "message", "value": "hello there"}]), "{older}");
    // Its event names the fields, never what was sent.
    let (_, ev) = call(&w.api, "GET", "/v1/apps/events?after=0", None).await;
    let sent: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "form.submitted").collect();
    assert_eq!((sent.len(), &sent[0]["data"]["fields"]), (2, &json!(["email", "message"])), "{ev}");
    assert!(!ev.to_string().contains("ana@example.com"), "{ev}");
    for sub in [&subs[0], &older[0]] {
        assert_eq!(call(&w.api, "DELETE", &format!("{base}/contact/submissions/{}", sub["id"].as_str().unwrap()), None).await.0, StatusCode::NO_CONTENT);
    }
    assert_eq!(call(&w.api, "GET", &format!("{base}/contact/submissions"), None).await.1, json!([]));
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
    // What the restore wrote over was backed up first: it can be undone.
    let (_, list) = call(&w.api, "GET", &base, None).await;
    assert!(list.as_array().unwrap().iter().any(|b| b["size"] == 21), "the state before the restore: {list}");
    // A deleted environment's volume is backed up before it goes (kept a week, restorable by hand).
    let staging = format!("/v1/apps/services/{id}/environments/staging");
    call(&w.api, "PUT", &staging, Some(json!({}))).await;
    w.fake.put_volume(&apps_control::volume_name(&id, "staging"), b"staging data");
    assert_eq!(call(&w.api, "DELETE", &staging, None).await.0, StatusCode::NO_CONTENT);
    call(&w.api, "PUT", &staging, Some(json!({}))).await;
    let (_, kept) = call(&w.api, "GET", &format!("{staging}/volume/backups"), None).await;
    assert!(kept.as_array().unwrap().iter().any(|b| b["size"] == 12), "its last backup: {kept}");
    let (_, trail) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert!(trail.to_string().contains("audit.volume.restored"), "{trail}");
    let (st, _) = call(&w.api, "POST", &format!("{base}/123/restore"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // Staging seeded from production's backup (Fly's restore elsewhere): production's files on
    // staging's volume, what staging had backed up first under its own name.
    let staging_volume = apps_control::volume_name(&id, "staging");
    w.fake.put_volume(&staging_volume, b"stale");
    let (st, r) = call(&w.api, "POST", &format!("{staging}/volume/backups/{}/restore", b["id"].as_str().unwrap()), Some(json!({"from": "production"}))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(w.fake.volume(&staging_volume).as_deref(), Some(&b"the first state"[..]));
    let (_, kept) = call(&w.api, "GET", &format!("{staging}/volume/backups"), None).await;
    assert!(kept.as_array().unwrap().iter().any(|b| b["size"] == 5), "staging's own state before: {kept}");
    assert_eq!(call(&w.api, "POST", &format!("{staging}/volume/backups/{}/restore", b["id"].as_str().unwrap()), Some(json!({"from": "nope"}))).await.0, StatusCode::NOT_FOUND);
    // Its own retention: the newest two kept.
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"volume": {"mount": "/data", "size_gb": 1, "backups": {"every_hours": 6, "keep": 0}}}}))).await.0, StatusCode::BAD_REQUEST);
    let (st, s) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"volume": {"mount": "/data", "size_gb": 1, "backups": {"every_hours": 6, "keep": 2}}}}))).await;
    assert_eq!((st, &s["run"]["volume"]["backups"]), (StatusCode::OK, &json!({"every_hours": 6, "keep": 2})), "{s}");
    call(&w.api, "POST", &base, None).await;
    let (_, list) = call(&w.api, "GET", &base, None).await;
    assert_eq!(list.as_array().unwrap().len(), 2, "{list}");
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
    // Its timeline: what roll did too (the deploy that went live), never a value either.
    let (_, all) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity?all=1"), None).await;
    let kinds: Vec<&str> = all.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"deploy.active") && kinds.contains(&"audit.settings"), "{kinds:?}");
    assert!(!all.to_string().contains("sk-s3cr3t"));
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
    // Its summary says so, for a list or a canvas card.
    let (_, s) = call(&w.api, "GET", &format!("/v1/apps/services/{id}"), None).await;
    assert!(s["environments"][0]["attention"].as_array().is_some_and(|a| a.contains(&json!("capped"))), "{s}");
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

/// Two environments compared (Railway's environment sync): variables missing, extra or with
/// another value (never a value, nor a sealed one's), and each differing setting with the body
/// that makes it the other's; copied through the usual routes, nothing differs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_environments_are_compared_and_synced() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80, "max_instances": 4})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments");
    call(&w.api, "PUT", &format!("{base}/staging"), Some(json!({}))).await;
    call(&w.api, "PATCH", &format!("{base}/production/variables"), Some(json!({"set": {"API_KEY": "k1", "MODE": "live", "TOKEN": "t"}, "seal": ["TOKEN"]}))).await;
    call(&w.api, "PATCH", &format!("{base}/staging/variables"), Some(json!({"set": {"MODE": "test", "DEBUG": "1"}}))).await;
    let prod = json!({"run": {"max_instances": 2}, "rate_limit": {"per_minute": 60, "burst": 2}, "headers": [{"set": {"x-frame-options": "DENY"}}], "allow_ips": [{"cidr": "203.0.113.0/24"}], "pages": {"error": "<h1>sorry</h1>"}, "vitals": true});
    assert_eq!(call(&w.api, "PUT", &format!("{base}/production"), Some(prod)).await.0, StatusCode::OK);
    let (st, d) = call(&w.api, "GET", &format!("{base}/staging/diff?from=production"), None).await;
    assert_eq!(st, StatusCode::OK, "{d}");
    assert_eq!(d["variables"], json!({"missing": [{"name": "API_KEY", "sealed": false}, {"name": "TOKEN", "sealed": true}], "extra": ["DEBUG"], "differ": ["MODE"]}));
    assert!(!d.to_string().contains("live") && !d.to_string().contains("k1"), "no variable's value: {d}");
    // A token that reads both but reveals neither isn't told which values differ (setting one
    // here and comparing would tell production's).
    let scope: roll_auth::scope::Scope = serde_json::from_value(json!([{"product": "apps", "resources": "*", "actions": {"read": "allow", "write": "allow"}}])).unwrap();
    let grant = roll_auth::Grant { id: "tok_t".into(), name: "ci".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(scope), user: None, ask_otherwise: false };
    let mut req = Request::builder().uri(format!("{base}/staging/diff?from=production")).body(Body::empty()).unwrap();
    req.extensions_mut().insert(grant);
    req.extensions_mut().insert(roll_auth::Operator);
    let resp = w.api.clone().oneshot(req).await.unwrap();
    let narrowed: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!((&narrowed["variables"]["differ"], &narrowed["variables"]["extra"]), (&Value::Null, &json!(["DEBUG"])), "{narrowed}");
    let fields: Vec<&str> = d["settings"].as_array().unwrap().iter().filter_map(|x| x["field"].as_str()).collect();
    assert_eq!(fields, ["run.max_instances", "vitals", "allow_ips", "rate_limit", "headers", "pages.error"], "{d}");
    // Synced: each setting's body, and the variables that can be copied (a sealed one can't).
    for x in d["settings"].as_array().unwrap() {
        let (st, e) = call(&w.api, "PUT", &format!("{base}/staging"), Some(x["put"].clone())).await;
        assert_eq!(st, StatusCode::OK, "{x} {e}");
    }
    let vars = format!("{base}/staging/variables");
    let (st, e) = call(&w.api, "PATCH", &vars, Some(json!({"copy_from": "production", "copy": ["TOKEN"]}))).await;
    assert!(st == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("sealed"), "{st} {e}");
    assert_eq!(call(&w.api, "PATCH", &vars, Some(json!({"copy_from": "production", "copy": ["API_KEY", "MODE"], "unset": ["DEBUG"]}))).await.0, StatusCode::OK);
    let (_, vals) = call(&w.api, "GET", &format!("{vars}/values"), None).await;
    assert_eq!((&vals["API_KEY"], &vals["MODE"], &vals["DEBUG"]), (&json!("k1"), &json!("live"), &Value::Null), "{vals}");
    let (_, d) = call(&w.api, "GET", &format!("{base}/staging/diff?from=production"), None).await;
    assert_eq!((&d["variables"], &d["settings"]), (&json!({"missing": [{"name": "TOKEN", "sealed": true}], "extra": [], "differ": []}), &json!([])), "{d}");
    assert_eq!(call(&w.api, "GET", &format!("{base}/staging/diff?from=staging"), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "GET", &format!("{base}/staging/diff?from=nope"), None).await.0, StatusCode::NOT_FOUND);
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
    // Only what was looked at: a deployment that isn't production's live one is refused.
    let (st, v) = call(&w.api, "POST", &format!("{base}/staging/promote"), Some(json!({"from": "production", "expect": "dep_somethingelse"}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    let (st, p) = call(&w.api, "POST", &format!("{base}/staging/promote"), Some(json!({"from": "production", "expect": d["id"]}))).await;
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
    // What roll turns away itself is in the log too (why, who asked, what host), and metered.
    let env = format!("/v1/apps/services/{id}/environments/production");
    assert_eq!(call(&w.api, "PUT", &env, Some(json!({"firewall": [{"action": "deny", "path": "/admin"}]}))).await.0, StatusCode::OK);
    let req = Request::builder().uri("/admin").header("host", host.as_str()).header("user-agent", "scanner/1.0").body(Body::empty()).unwrap();
    assert_eq!(w.public.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
    let denied = get_until(&w.api, &format!("{base}&by=firewall"), |l| l.as_array().is_some_and(|a| !a.is_empty())).await;
    let line: Value = serde_json::from_str(denied[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((line["by"].as_str(), line["status"].as_u64(), line["ua"].as_str(), line["host"].as_str()), (Some("firewall"), Some(403), Some("scanner/1.0"), Some(host.as_str())), "{line}");
    assert_eq!(call(&w.api, "GET", &format!("{base}&by=any"), None).await.1.as_array().unwrap().len(), 1, "the app's own answers have no `by`");
    let (_, m) = call(&w.api, "GET", &format!("{env}/metrics?minutes=5"), None).await;
    assert_eq!(m["requests"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).sum::<f64>(), 3.0, "{m}");
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
    let minute = |m: i64, errors: i64| apps_control::store::MinuteRow { minute: m, requests: 100, errors, instances: 1, memory_bytes: 64 << 20, ..Default::default() };
    let live = w.apps.live(&id, "production");
    // A loaded machine can cross into the next minute, or judge before the live deployment's
    // template is in place: written and judged again until it fires (then judged once more: once).
    let fired = || async {
        let (_, ev) = call(&w.api, "GET", "/v1/apps/events?after=0", None).await;
        ev["events"].as_array().unwrap().iter().any(|e| e["kind"] == "alert.errors")
    };
    for _ in 0..100 {
        let now = code_sql::now_ms() / 60_000;
        w.apps.store.put_minutes(&id, "production", &(1..=5).map(|i| minute(now - i, 20)).collect::<Vec<_>>()).unwrap();
        w.apps.judge_alerts(&live).await;
        if fired().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    w.apps.judge_alerts(&live).await;
    let (_, ev) = call(&w.api, "GET", "/v1/apps/events?after=0", None).await;
    let alerts: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "alert.errors").collect();
    assert_eq!(alerts.len(), 1, "{ev}");
    assert_eq!((alerts[0]["data"]["value"].as_i64(), alerts[0]["data"]["threshold"].as_i64()), (Some(20), Some(10)));
    // Memory stayed low and no latency alert is set: nothing else.
    assert!(!ev.to_string().contains("alert.memory") && !ev.to_string().contains("alert.latency"), "{ev}");
    // Five clean minutes: it's said once that it cleared, naming the alert.
    let now = code_sql::now_ms() / 60_000;
    w.apps.store.put_minutes(&id, "production", &(0..=6).map(|i| minute(now - i, 0)).collect::<Vec<_>>()).unwrap();
    w.apps.judge_alerts(&live).await;
    w.apps.judge_alerts(&live).await;
    let (_, ev) = call(&w.api, "GET", "/v1/apps/events?after=0", None).await;
    let resolved: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "alert.resolved").collect();
    assert_eq!(resolved.len(), 1, "{ev}");
    assert_eq!(resolved[0]["data"]["alert"], "alert.errors");
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

/// A custom domain sent to another of the app's ports (Railway's target port): from the next
/// deploy that host's requests reach that port of the same instance; the service's own names
/// stay on its port, and health checks and wakes too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_custom_domain_reaches_another_port_of_the_app() {
    let w = world().await;
    let (svc, _) = image_service(&w, "panel", json!({"port": 8080})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let env = format!("/v1/apps/services/{id}/environments/production");
    let h = "admin.example.org";
    let (_, d) = call(&w.api, "POST", &format!("{env}/domains"), Some(json!({ "hostname": h }))).await;
    w.dns.add(h, "CNAME", d["target"].as_str().unwrap());
    w.dns.add(&format!("_roll-challenge.{h}"), "TXT", d["token"].as_str().unwrap());
    assert_eq!(call(&w.api, "POST", &format!("{env}/domains/{h}/verify"), None).await.1["status"], "active");
    assert_eq!(call(&w.api, "PATCH", &format!("{env}/domains/{h}"), Some(json!({"port": 0}))).await.0, StatusCode::BAD_REQUEST);
    let (st, d) = call(&w.api, "PATCH", &format!("{env}/domains/{h}"), Some(json!({"port": 9000}))).await;
    assert_eq!((st, &d["port"], &d["status"]), (StatusCode::OK, &json!(9000), &json!("active")), "{d}");
    // Not published by the live deployment yet: said so, not sent to the main port.
    assert_eq!(visit(&w, h, "/").await.0, StatusCode::BAD_GATEWAY);
    let (_, r) = call(&w.api, "POST", &format!("{env}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    let (st, body) = visit(&w, h, "/").await;
    assert_eq!((st, &body["port"]), (StatusCode::OK, &json!(9000)), "{body}");
    let (st, body) = visit(&w, &host(&svc["environments"][0]["url"]), "/").await;
    assert_eq!((st, &body["port"]), (StatusCode::OK, &json!(8080)), "the service's own name: {body}");
    // Back to the service's port.
    let (_, d) = call(&w.api, "PATCH", &format!("{env}/domains/{h}"), Some(json!({"port": null}))).await;
    assert!(d.get("port").is_none(), "{d}");
    assert_eq!(visit(&w, h, "/").await.1["port"], json!(8080));
}

/// A project's own preview domain (Vercel's preview deployment suffix): a wildcard verified as a
/// custom domain is; a preview environment answers to its `preview_host` one label under it, and
/// any other label is none of roll apps'.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preview_environment_answers_under_its_projects_preview_domain() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/projects/{}/preview-domain", w.project);
    assert_eq!(call(&w.api, "GET", &base, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"hostname": "preview.acme.dev"}))).await.0, StatusCode::BAD_REQUEST, "a wildcard");
    let (st, d) = call(&w.api, "POST", &base, Some(json!({"hostname": "*.preview.acme.dev"}))).await;
    assert_eq!((st, d["status"].as_str()), (StatusCode::CREATED, Some("pending")), "{d}");
    // A preview environment and its label (roll previews sets it on the pv- environment it makes).
    let pv = format!("/v1/apps/services/{id}/environments/pv-1");
    assert_eq!(call(&w.api, "PUT", &pv, Some(json!({"ephemeral": true, "preview_host": "Web_PR1"}))).await.0, StatusCode::BAD_REQUEST);
    let (st, e) = call(&w.api, "PUT", &pv, Some(json!({"ephemeral": true, "preview_host": "web-pr1-shop"}))).await;
    assert_eq!((st, e["preview_host"].as_str()), (StatusCode::OK, Some("web-pr1-shop")), "{e}");
    let (_, r) = call(&w.api, "POST", &format!("{pv}/deploys"), Some(json!({}))).await;
    settled(&w, &id, "pv-1", r["id"].as_str().unwrap()).await;
    let other = format!("/v1/apps/services/{id}/environments/pv-2");
    assert_eq!(call(&w.api, "PUT", &other, Some(json!({"ephemeral": true, "preview_host": "web-pr1-shop"}))).await.0, StatusCode::CONFLICT, "one label an environment");
    // Not verified yet: nothing answers under it.
    assert_eq!(visit(&w, "web-pr1-shop.preview.acme.dev", "/").await.0, StatusCode::NOT_FOUND);
    w.dns.add("_roll-challenge.preview.acme.dev", "TXT", d["token"].as_str().unwrap());
    let (_, v) = call(&w.api, "POST", &format!("{base}/verify"), None).await;
    assert_eq!(v["status"], "active", "{v}");
    let (st, body) = visit(&w, "web-pr1-shop.preview.acme.dev", "/").await;
    assert_eq!((st, body["deployment"].as_str()), (StatusCode::OK, r["id"].as_str()), "{body}");
    assert_eq!(visit(&w, "someone-else.preview.acme.dev", "/").await.0, StatusCode::NOT_FOUND, "another label is none of roll apps'");
    // With roll previews' router on the node, such a label is its (a preview on roll's own
    // runtime), the request as it came; one held by an environment stays roll apps'.
    struct Previews;
    #[async_trait::async_trait]
    impl apps_control::integrations::PreviewRouter for Previews {
        async fn serve(&self, req: axum::extract::Request) -> axum::response::Response {
            use axum::response::IntoResponse;
            format!("previews for {}", req.headers()["host"].to_str().unwrap()).into_response()
        }
    }
    w.apps.set_preview_router(Arc::new(Previews));
    let req = Request::builder().uri("/").header("host", "someone-else.preview.acme.dev").body(Body::empty()).unwrap();
    let resp = w.public.clone().oneshot(req).await.unwrap();
    let text = String::from_utf8(axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap().to_vec()).unwrap();
    assert_eq!(text, "previews for someone-else.preview.acme.dev");
    assert_eq!(visit(&w, "web-pr1-shop.preview.acme.dev", "/").await.1["deployment"].as_str(), r["id"].as_str());
    assert_eq!(visit(&w, "nobody.elsewhere.dev", "/").await.0, StatusCode::NOT_FOUND, "only under an active preview domain");
    // Another project can't take it, nor a service's custom domain.
    let p2 = w.apps.deps.workspace.as_ref().unwrap().create("", "other", "", None).unwrap().id;
    assert_eq!(call(&w.api, "POST", &format!("/v1/apps/projects/{p2}/preview-domain"), Some(json!({"hostname": "*.preview.acme.dev"}))).await.0, StatusCode::CONFLICT);
    // Gone: its names answer nothing.
    assert_eq!(call(&w.api, "DELETE", &base, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(visit(&w, "web-pr1-shop.preview.acme.dev", "/").await.0, StatusCode::NOT_FOUND);
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
    // A retry with its Idempotency-Key answers the same redeploy.
    let keyed = |w: &World| {
        let (api, base) = (w.api.clone(), base.clone());
        async move {
            let req = Request::post(format!("{base}/redeploy")).header("idempotency-key", "redeploy-1").body(Body::empty()).unwrap();
            let resp = api.oneshot(req).await.unwrap();
            let st = resp.status();
            (st, serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap())
        }
    };
    let (st1, k1) = keyed(&w).await;
    let (st2, k2) = keyed(&w).await;
    assert_eq!((st1, st2, &k1["id"]), (StatusCode::ACCEPTED, StatusCode::ACCEPTED, &k2["id"]), "{k1} {k2}");
    settled(&w, &id, "production", k1["id"].as_str().unwrap()).await;
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
    // Image optimization (Vercel's, Next.js's `/_next/image`): the site's own image, resized to an
    // allowed width and made WebP for a browser that takes it, then answered from the node's cache;
    // never a path elsewhere, an unlisted width, or a file that isn't an image.
    let image = |q: &str, accept: &str| {
        let req = Request::builder().uri(format!("/__roll/image?{q}")).header("host", h.as_str()).header("accept", accept).body(Body::empty()).unwrap();
        let public = w.public.clone();
        async move {
            let r = public.oneshot(req).await.unwrap();
            let (st, hd) = (r.status(), r.headers().clone());
            (st, hd, r.into_body().collect().await.unwrap().to_bytes())
        }
    };
    let (st, hd, webp) = image("url=/assets/hero.png&w=32", "image/avif,image/webp,*/*").await;
    assert_eq!((st, hd["content-type"].to_str().unwrap(), hd["vary"].to_str().unwrap(), hd["x-roll-image"].to_str().unwrap()), (StatusCode::OK, "image/webp", "Accept", "miss"), "{hd:?}");
    assert_eq!(&webp[..4], b"RIFF");
    assert_eq!(image("url=/assets/hero.png&w=32", "image/webp").await.1["x-roll-image"], "hit");
    let (_, hd, png) = image("url=/assets/hero.png&w=32", "image/png").await;
    assert_eq!((hd["content-type"].to_str().unwrap(), &png[1..4]), ("image/png", &b"PNG"[..]), "the source's format without WebP");
    assert_eq!(image("url=/assets/app.js&w=32", "*/*").await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(image("url=/nope.png&w=32", "*/*").await.0, StatusCode::NOT_FOUND);
    for bad in ["url=//elsewhere.example/a.png&w=32", "url=/assets/hero.png&w=33", "url=https%3A%2F%2Felsewhere.example%2Fa.png&w=32"] {
        assert_eq!(image(bad, "*/*").await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
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
    // Live in the store a moment before it serves: until then the old site answers, and the new
    // chunk isn't one of its.
    for _ in 0..250 {
        if site(&chunk(&d2["image"])).await.unwrap().status() == StatusCode::OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for (path, want) in [(chunk(&d["image"]), StatusCode::OK), (chunk(&d2["image"]), StatusCode::OK), ("/assets/chunk-0123abcdz9.js".to_string(), StatusCode::NOT_FOUND)] {
        let r = site(&path).await.unwrap();
        assert_eq!(r.status(), want, "{path}");
    }
    // `/api/* service:api/v1/:splat 200`: the project's api service answers on the site's origin.
    assert_eq!(site("/api/users").await.unwrap().status(), StatusCode::BAD_GATEWAY, "no api service yet");
    let (api, _) = image_service(&w, "api", json!({"port": 80})).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let r = site("/api/users?page=2").await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["path"], "/v1/users", "{body}");
    assert_eq!(site("/api/..%2F..%2Fadmin").await.unwrap().status(), StatusCode::BAD_REQUEST, "never above the rule's path");
    // A private api: the site lets its visitors through by the project's rule; a visitor who
    // says it's a rewrite, straight to the api, still needs the api's access.
    let api_env = format!("/v1/apps/services/{}/environments/production", api["id"].as_str().unwrap());
    assert_eq!(call(&w.api, "PUT", &api_env, Some(json!({"access": {"token": "api-only-token-0123456789"}}))).await.0, StatusCode::OK);
    assert_eq!(site("/api/users").await.unwrap().status(), StatusCode::OK);
    let forged = Request::builder().uri("/").header("host", host(&api["environments"][0]["url"])).header("x-roll-via-site", "1").body(Body::empty()).unwrap();
    assert_ne!(w.public.clone().oneshot(forged).await.unwrap().status(), StatusCode::OK);

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
    // A run is read on its own, and its output is its own (not the environment's whole log).
    let rid = last["id"].as_str().unwrap().to_string();
    assert_eq!(call(&w.api, "GET", &format!("{base}/runs/{rid}"), None).await.1["status"], "succeeded");
    let out = get_until(&w.api, &format!("{base}/runs/{rid}/logs"), |l| l.as_array().is_some_and(|a| !a.is_empty())).await;
    assert!(out.as_array().unwrap().iter().all(|l| l["text"] == "job ran"), "{out}");
    assert_eq!(call(&w.api, "POST", &format!("{base}/runs/{rid}/cancel"), None).await.0, StatusCode::CONFLICT, "only a running one");
    // A run that fails is an event (notified by default), with its exit code.
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_EXIT": "3"}}))).await;
    let (_, d) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &cid, "production", d["id"].as_str().unwrap()).await;
    call(&w.api, "POST", &format!("{base}/runs"), None).await;
    let ev = get_until(&w.api, "/v1/apps/events?after=0", |v| v.to_string().contains("cron.failed")).await;
    let failed = ev["events"].as_array().unwrap().iter().find(|e| e["kind"] == "cron.failed").unwrap();
    assert_eq!((failed["data"]["exit_code"].as_i64(), failed["data"]["status"].as_str()), (Some(3), Some("failed")), "{failed}");
    // One that hangs is canceled, and the schedule's next run isn't held back by it.
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"unset": ["FAKE_EXIT"]}))).await;
    let (_, d) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &cid, "production", d["id"].as_str().unwrap()).await;
    let (_, hung) = call(&w.api, "POST", &format!("{base}/runs"), None).await;
    let hid = hung["id"].as_str().unwrap().to_string();
    assert_eq!(hung["status"], "running", "{hung}");
    assert_eq!(call(&w.api, "POST", &format!("{base}/runs/{hid}/cancel"), None).await.0, StatusCode::OK);
    get_until(&w.api, &format!("{base}/runs/{hid}"), |r| r["status"] == "canceled").await;
    let (_, next) = call(&w.api, "POST", &format!("{base}/runs"), None).await;
    assert_eq!(next["status"], "running", "not skipped: {next}");
    call(&w.api, "POST", &format!("{base}/runs/{}/cancel", next["id"].as_str().unwrap()), None).await;
    // A search of one run's output is of its own lines only (other runs said the same).
    let (_, found) = call(&w.api, "GET", &format!("{base}/runs/{rid}/logs?q=job%20ran"), None).await;
    assert_eq!(found.as_array().unwrap().len(), 1, "{found}");
    let (_, all) = call(&w.api, "GET", &format!("{base}/logs?q=job%20ran"), None).await;
    assert!(all.as_array().unwrap().len() >= 2, "{all}");
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
    // A one-off console (Heroku's `heroku run bash`): a fresh container of the live deployment
    // with what's asked running in it, the serving instances untouched, gone when it closes.
    let before: Vec<String> = w.fake.instances(&id).into_iter().map(|(n, _, _)| n).collect();
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"command": "rails console"}))).await.0, StatusCode::BAD_REQUEST, "a command is a console's");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"console": true, "memory_mb": 64}))).await.0, StatusCode::BAD_REQUEST);
    let (st, open) = call(&w.api, "POST", &base, Some(json!({"console": true, "command": "rails console", "memory_mb": 1024}))).await;
    assert_eq!(st, StatusCode::CREATED, "{open}");
    let sh = open["id"].as_str().unwrap().to_string();
    let resp = w.api.clone().oneshot(Request::get(format!("{base}/{sh}/screen")).body(Body::empty()).unwrap()).await.unwrap();
    let mut body = resp.into_body();
    let banner = sse_chunk(&mut body).await;
    assert!(banner.contains("-console") && banner.contains("running rails console"), "{banner}");
    let console = w.fake.instances(&id).into_iter().map(|(n, _, _)| n).find(|n| n.contains("console")).expect("its own container");
    assert_eq!(w.fake.spec(&console).map(|s| (s.memory_mb, s.port)), Some((1024, None)), "its size, no port");
    assert!(before.iter().all(|n| w.fake.instances(&id).iter().any(|(m, _, _)| m == n)), "the serving instances are untouched");
    assert_eq!(call(&w.api, "DELETE", &format!("{base}/{sh}"), None).await.0, StatusCode::NO_CONTENT);
    let mut gone = false;
    for _ in 0..100 {
        if !w.fake.instances(&id).iter().any(|(n, _, _)| *n == console) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "the console's container goes with it");
    let (_, trail) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert!(trail.to_string().contains("audit.console.opened") && !trail.to_string().contains("rails console"), "{trail}");
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
    let vendor: Arc<parking_lot::Mutex<Vec<(axum::http::HeaderMap, bytes::Bytes)>>> = Default::default();
    let (g, v) = (got.clone(), vendor.clone());
    let app = axum::Router::new()
        .route("/logs", axum::routing::post(move |h: axum::http::HeaderMap, b: bytes::Bytes| {
            let g = g.clone();
            async move {
                g.lock().push((h, b));
                StatusCode::OK
            }
        }))
        .route("/vendor", axum::routing::post(move |h: axum::http::HeaderMap, b: bytes::Bytes| {
            let v = v.clone();
            async move {
                v.lock().push((h, b));
                StatusCode::ACCEPTED
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
    // A vendor's drain: its key as a header (sealed, never shown), only the requests, only this service.
    let (st, vd) = call(&w.api, "POST", &base, Some(json!({"url": format!("http://{at}/vendor"), "sources": ["http"], "services": ["api"], "headers": {"DD-API-KEY": "k-123"}}))).await;
    assert_eq!(st, StatusCode::CREATED, "{vd}");
    assert_eq!((&vd["sources"], &vd["headers"]), (&json!(["http"]), &json!(["DD-API-KEY"])), "{vd}");
    assert!(!call(&w.api, "GET", &base, None).await.1.to_string().contains("k-123"), "a header's value never comes back");
    let (st, v) = call(&w.api, "POST", &base, Some(json!({"url": format!("http://{at}/vendor"), "headers": {"X-Roll-Signature-256": "x"}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "roll's own headers aren't a drain's: {v}");
    let test = call(&w.api, "POST", &format!("{base}/{}/test", vd["id"].as_str().unwrap()), None).await.1;
    assert_eq!((&test["ok"], &test["code"]), (&json!(true), &json!(202)), "{test}");
    vendor.lock().clear();
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
    // The vendor got the request, with its key, and nothing else.
    let mut sent = Vec::new();
    for _ in 0..200 {
        sent = vendor.lock().clone();
        if !sent.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (h, b) = sent.first().cloned().unwrap_or_else(|| panic!("nothing reached the vendor's drain"));
    assert_eq!(h["dd-api-key"], "k-123");
    let lines: Vec<Value> = String::from_utf8_lossy(&b).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(!lines.is_empty() && lines.iter().all(|l| l["source"] == "http"), "{lines:?}");
}

/// A drain in another format: syslog (RFC 5424, octet-counted, as Papertrail takes it) and OTLP
/// logs (a collector's `/v1/logs`), each with the runtime output.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_drain_speaks_syslog_or_otlp() {
    use tokio::io::AsyncReadExt;
    let w = world().await;
    let base = format!("/v1/apps/projects/{}/drains", w.project);
    // A syslog server: what each connection sends, kept.
    let syslog = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sys_at = syslog.local_addr().unwrap();
    let heard: Arc<parking_lot::Mutex<Vec<u8>>> = Default::default();
    let h = heard.clone();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = syslog.accept().await {
            let h = h.clone();
            tokio::spawn(async move {
                let mut b = Vec::new();
                let _ = c.read_to_end(&mut b).await;
                h.lock().extend_from_slice(&b);
            });
        }
    });
    let otlp: Arc<parking_lot::Mutex<Vec<(axum::http::HeaderMap, Value)>>> = Default::default();
    let o = otlp.clone();
    let app = axum::Router::new().route("/v1/logs", axum::routing::post(move |h: axum::http::HeaderMap, b: bytes::Bytes| {
        let o = o.clone();
        async move {
            o.lock().push((h, serde_json::from_slice(&b).unwrap()));
            StatusCode::OK
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let otlp_at = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, app).into_future());
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": format!("syslog://{sys_at}"), "format": "syslog", "headers": {"X-Key": "k"}}))).await.0, StatusCode::BAD_REQUEST, "a syslog server is sent no headers");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": "syslog+tls://logs.example.com", "format": "syslog"}))).await.0, StatusCode::BAD_REQUEST, "it names its port");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": format!("http://{otlp_at}/v1/logs"), "format": "gelf"}))).await.0, StatusCode::BAD_REQUEST);
    let (st, sd) = call(&w.api, "POST", &base, Some(json!({"url": format!("syslog://{sys_at}"), "format": "syslog"}))).await;
    assert_eq!((st, sd["format"].as_str()), (StatusCode::CREATED, Some("syslog")), "{sd}");
    let (st, od) = call(&w.api, "POST", &base, Some(json!({"url": format!("http://{otlp_at}/v1/logs"), "format": "otlp", "headers": {"Authorization": "Bearer t-1"}}))).await;
    assert_eq!(st, StatusCode::CREATED, "{od}");
    assert_eq!(call(&w.api, "PATCH", &format!("{base}/{}", od["id"].as_str().unwrap()), Some(json!({"format": "ndjson"}))).await.0, StatusCode::BAD_REQUEST, "a new format is a new drain");
    let test = call(&w.api, "POST", &format!("{base}/{}/test", sd["id"].as_str().unwrap()), None).await.1;
    assert_eq!((&test["ok"], &test["code"]), (&json!(true), &Value::Null), "{test}");
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    visit(&w, &host(&svc["environments"][0]["url"]), "/").await;
    let mut said = String::new();
    let mut sent = Vec::new();
    for _ in 0..200 {
        said = String::from_utf8_lossy(&heard.lock()).to_string();
        sent = otlp.lock().clone();
        if said.contains("listening on") && sent.iter().any(|(_, b)| b.to_string().contains("listening on")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Octet-counted RFC 5424: `<len> <14>1 <time> api production <instance> runtime - <text>`.
    let mut rest = said.as_str();
    let mut messages = Vec::new();
    while let Some((n, after)) = rest.split_once(' ') {
        let n: usize = n.parse().unwrap_or_else(|_| panic!("not octet-counted: {said:?}"));
        messages.push(&after[..n]);
        rest = &after[n..];
    }
    let m = messages.iter().find(|m| m.contains("listening on")).unwrap_or_else(|| panic!("{said:?}"));
    let parts: Vec<&str> = m.splitn(8, ' ').collect();
    assert_eq!((parts[0], parts[2], parts[3], parts[5], parts[6]), ("<14>1", "api", "production", "runtime", "-"), "{m}");
    assert!(parts[1].ends_with('Z') && parts[4].starts_with("roll-apps-"), "{m}");
    // OTLP: a resource for the service and environment, a record a line, the vendor's key sent.
    let (h, body) = sent.iter().find(|(_, b)| b.to_string().contains("listening on")).cloned().unwrap_or_else(|| panic!("{sent:?}"));
    assert_eq!((h["content-type"].to_str().unwrap(), h["authorization"].to_str().unwrap()), ("application/json", "Bearer t-1"));
    let rl = &body["resourceLogs"][0];
    let attrs: Vec<(String, String)> = rl["resource"]["attributes"].as_array().unwrap().iter().map(|a| (a["key"].as_str().unwrap().into(), a["value"]["stringValue"].as_str().unwrap().into())).collect();
    assert!(attrs.contains(&("service.name".into(), "api".into())) && attrs.contains(&("deployment.environment.name".into(), "production".into())), "{attrs:?}");
    let records = rl["scopeLogs"][0]["logRecords"].as_array().unwrap();
    assert!(records.iter().any(|r| r["body"]["stringValue"].as_str().is_some_and(|t| t.contains("listening on")) && r["timeUnixNano"].as_str().is_some_and(|t| t.len() >= 19)), "{records:?}");
}

/// Uptime checks (Cloud Monitoring's): a path GET'd through the environment's own proxy on its
/// schedule; two failures in a row are `check.down` once, the next pass resolves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uptime_check_goes_down_and_comes_back() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    for bad in [json!([{"name": "Bad Name", "path": "/"}]), json!([{"name": "a", "path": "x"}]), json!([{"name": "a", "path": "/", "every_s": 5}]), json!([{"name": "a", "path": "/"}, {"name": "a", "path": "/b"}])] {
        assert_eq!(call(&w.api, "PUT", &base, Some(json!({"checks": bad}))).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"checks": [{"name": "home", "path": "/", "every_s": 60}]}))).await;
    assert_eq!((st, env["checks"][0]["every_s"].as_u64(), env["check_states"][0]["up"].clone()), (StatusCode::OK, Some(60), Value::Null), "{env}");
    // Each pass a minute on (the pass runs a check that's due), its result read back.
    let t0 = code_sql::now_ms() + 3_600_000;
    let pass = |n: i64| {
        let w = &w;
        let base = base.clone();
        async move {
            w.apps.cron_pass(t0 + n * 61_000).await;
            for _ in 0..200 {
                let st = call(&w.api, "GET", &base, None).await.1["check_states"][0].clone();
                if st["last_at"].as_i64() == Some(t0 + n * 61_000) && !st["status"].is_null() {
                    return st;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("pass {n} never came back");
        }
    };
    let up = pass(0).await;
    assert_eq!((up["up"].as_bool(), up["status"].as_u64()), (Some(true), Some(200)), "{up}");
    // It now expects what the app never answers (whichever instance answers): it fails.
    call(&w.api, "PUT", &base, Some(json!({"checks": [{"name": "home", "path": "/", "every_s": 60, "status": 299}]}))).await;
    let once = pass(1).await;
    assert_eq!((once["up"].as_bool(), once["failures"].as_u64()), (Some(true), Some(1)), "one failure isn't down: {once}");
    let down = pass(2).await;
    assert_eq!((down["up"].as_bool(), down["error"].as_str()), (Some(false), Some("answered 200")), "{down}");
    pass(3).await;
    call(&w.api, "PUT", &base, Some(json!({"checks": [{"name": "home", "path": "/", "every_s": 60}]}))).await;
    assert_eq!(pass(4).await["up"].as_bool(), Some(true));
    // Each event is written a moment after its state.
    let mut ev = Value::Null;
    for _ in 0..200 {
        ev = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
        if ev.to_string().contains("alert.resolved") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let kinds: Vec<&str> = ev["events"].as_array().unwrap().iter().filter(|e| e["data"]["check"] == "home").map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["check.down", "alert.resolved"], "down once, then resolved: {ev}");
}

/// Heartbeats (Healthchecks', Cronitor's): something outside roll pings its URL; one that
/// doesn't within its period and grace is `heartbeat.missed` once, the next ping resolves it. The
/// URL is shown once, when the heartbeat is added, and a kept heartbeat keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeats_ping_and_go_missed() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let h = host(&svc["environments"][0]["url"]);
    let base = format!("/v1/apps/services/{id}/environments/production");
    for bad in [json!([{"name": "b", "every_s": 5}]), json!([{"name": "b", "every_s": 3600, "url": "x"}]), json!([{"name": "b", "every_s": 3600}, {"name": "b", "every_s": 60}])] {
        assert_eq!(call(&w.api, "PUT", &base, Some(json!({"heartbeats": bad}))).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    // Private: a heartbeat's token is a ping's own way in.
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"access": {"token": "aaaaaaaabbbbbbbbccccdddd"}, "heartbeats": [{"name": "backup", "every_s": 3600}]}))).await;
    assert_eq!((st, env["private"].as_bool()), (StatusCode::OK, Some(true)), "{env}");
    let url = env["heartbeat_urls"]["backup"].as_str().unwrap().to_string();
    let path = format!("/__roll/{}", url.split_once("/__roll/").unwrap().1);
    let token = path.rsplit('/').next().unwrap().to_string();
    let read = call(&w.api, "GET", &base, None).await.1;
    assert!(read["heartbeat_urls"].is_null() && !read.to_string().contains(&token), "the URL is shown once: {read}");
    let ping = |path: String| {
        let public = w.public.clone();
        let req = Request::builder().method("POST").uri(path).header("host", h.clone()).body(Body::empty()).unwrap();
        async move { public.oneshot(req).await.unwrap().status() }
    };
    assert_eq!(ping("/__roll/heartbeat/hb_wrong".into()).await, StatusCode::NOT_FOUND);
    assert_eq!(ping(path.clone()).await, StatusCode::OK);
    let state = call(&w.api, "GET", &base, None).await.1["heartbeat_states"][0].clone();
    assert!(state["last_ping_at"].is_i64() && state["missed"] == false, "{state}");
    // Past its hour and five minutes' grace: missed once, however many passes.
    let late = state["last_ping_at"].as_i64().unwrap() + 3_900_000;
    w.apps.cron_pass(late).await;
    w.apps.cron_pass(late + 60_000).await;
    assert_eq!(call(&w.api, "GET", &base, None).await.1["heartbeat_states"][0]["missed"], true);
    // A kept heartbeat keeps its URL; the next ping resolves the miss.
    let (_, env) = call(&w.api, "PUT", &base, Some(json!({"heartbeats": [{"name": "backup", "every_s": 7200, "grace_s": 60}]}))).await;
    assert!(env["heartbeat_urls"].is_null() && env["heartbeats"][0]["every_s"] == 7200, "{env}");
    assert_eq!(ping(path).await, StatusCode::OK);
    assert_eq!(call(&w.api, "GET", &base, None).await.1["heartbeat_states"][0]["missed"], false);
    let ev = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    let kinds: Vec<&str> = ev["events"].as_array().unwrap().iter().filter(|e| e["data"]["heartbeat"] == "backup").map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["heartbeat.missed", "alert.resolved"], "missed once, then resolved: {ev}");
}

/// Images scanned for known vulnerabilities (Cloud Run's Artifact Analysis): beside the rollout,
/// counts on the deployment and the findings most severe first; new ones at high or worse are
/// `deploy.vulnerable`; `run.block_on` scans first and fails a deploy that has one, never a
/// rollback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn images_are_scanned_and_block_on_holds_a_deploy() {
    use apps_control::model::{Severity, Vulnerability};
    let w = world().await;
    let v = |id: &str, severity: Severity, fixed: Option<&str>| Vulnerability { id: id.into(), severity, package: "openssl".into(), installed: "3.0.1".into(), fixed: fixed.map(String::from), title: None, target: "debian 12".into() };
    *w.fake.scan_findings.lock() = Some(vec![v("CVE-1", Severity::Critical, Some("3.0.2")), v("CVE-2", Severity::High, None), v("CVE-3", Severity::Low, None)]);
    let (svc, first) = image_service(&w, "web", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let env = format!("/v1/apps/services/{id}/environments/production");
    let first = first["id"].as_str().unwrap().to_string();
    // Beside the rollout: read back once it's kept.
    let mut d = Value::Null;
    for _ in 0..200 {
        d = call(&w.api, "GET", &format!("{env}/deploys/{first}"), None).await.1;
        if !d["vulnerabilities"].is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = &d["vulnerabilities"];
    assert_eq!((s["critical"].as_u64(), s["high"].as_u64(), s["low"].as_u64(), s["fixable"].as_u64()), (Some(1), Some(1), Some(1), Some(1)), "{d}");
    assert_eq!(call(&w.api, "GET", &format!("{env}/deploys"), None).await.1[0]["vulnerabilities"]["critical"], 1, "the list has the counts too");
    let (_, list) = call(&w.api, "GET", &format!("{env}/deploys/{first}/vulnerabilities?severity=high"), None).await;
    let ids: Vec<&str> = list["vulnerabilities"].as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["CVE-1", "CVE-2"], "{list}");
    assert_eq!(call(&w.api, "GET", &format!("{env}/deploys/{first}/vulnerabilities?severity=bad"), None).await.0, StatusCode::BAD_REQUEST);
    // With block_on, a deploy whose image has one fails before it goes live.
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"block_on": "unknown"}}))).await.0, StatusCode::BAD_REQUEST);
    let (st, r) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"block_on": "critical"}}))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (_, r) = call(&w.api, "POST", &format!("{env}/redeploy"), Some(json!({}))).await;
    let held = settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    assert_eq!(held["status"], "failed", "{held}");
    assert!(held["error"].as_str().unwrap().contains("1 known vulnerability at critical or worse"), "{held}");
    assert_eq!(call(&w.api, "GET", &env, None).await.1["active"]["id"], first.as_str(), "the live one keeps serving");
    // A rollback is never held.
    let (st, rb) = call(&w.api, "POST", &format!("{env}/deploys/{first}/rollback"), None).await;
    assert!(st.is_success(), "{rb}");
    assert_eq!(settled(&w, &id, "production", rb["id"].as_str().unwrap()).await["status"], "active");
    // Fixed: it goes live, and nothing new at high or worse says so again.
    *w.fake.scan_findings.lock() = Some(vec![v("CVE-2", Severity::High, None)]);
    let (_, r) = call(&w.api, "POST", &format!("{env}/redeploy"), Some(json!({}))).await;
    assert_eq!(settled(&w, &id, "production", r["id"].as_str().unwrap()).await["status"], "active");
    let ev = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    let found: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "deploy.vulnerable").collect();
    assert_eq!(found.len(), 1, "{ev}");
    assert_eq!((found[0]["deployment"].as_str(), found[0]["data"]["new"].as_u64()), (Some(first.as_str()), Some(2)), "{ev}");
    // A day on, the live one is scanned again: an advisory out since is new.
    let live = r["id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        if !call(&w.api, "GET", &format!("{env}/deploys/{live}"), None).await.1["vulnerabilities"].is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    *w.fake.scan_findings.lock() = Some(vec![v("CVE-2", Severity::High, None), v("CVE-9", Severity::Critical, Some("3.0.9"))]);
    w.apps.cron_pass(code_sql::now_ms() + 25 * 3_600_000).await;
    let mut again = Vec::new();
    for _ in 0..200 {
        let ev = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
        again = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "deploy.vulnerable" && e["deployment"] == live.as_str()).map(|e| e["data"]["ids"].clone()).collect();
        if !again.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(again, [json!(["CVE-9"])], "only what's new since its last scan");
}

/// PagerDuty and Opsgenie targets (Heroku's, Cloud Monitoring's): a failure opens an incident
/// keyed by its service, environment and alert, a resolved alert or the next good deploy closes
/// it; the key goes with each call and never comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_call_targets_open_and_close_incidents() {
    let got: Arc<parking_lot::Mutex<Vec<(String, axum::http::HeaderMap, Value)>>> = Default::default();
    let g = got.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, h: axum::http::HeaderMap, b: bytes::Bytes| {
        let g = g.clone();
        async move {
            g.lock().push((uri.to_string(), h, serde_json::from_slice(&b).unwrap_or(Value::Null)));
            StatusCode::ACCEPTED
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, app).into_future());
    let w = world_full(
        |c| {
            c.oncall_apis = apps_control::OncallApis { pagerduty_us: format!("http://{at}/pd"), pagerduty_eu: format!("http://{at}/pd-eu"), opsgenie_us: format!("http://{at}/og"), opsgenie_eu: format!("http://{at}/og-eu") };
        },
        false,
        |_| {},
    )
    .await;
    // Deployed before the targets: its first deploy's event isn't theirs.
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/projects/{}/notifications", w.project);
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"kind": "pagerduty"}))).await.0, StatusCode::BAD_REQUEST, "its routing key");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"kind": "pagerduty", "routing_key": "r0ut1ng", "url": "https://evil.example/"}))).await.0, StatusCode::BAD_REQUEST, "never a URL of the tenant's");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"kind": "webhook", "url": format!("http://{at}/x"), "routing_key": "x"}))).await.0, StatusCode::BAD_REQUEST);
    let (st, pd) = call(&w.api, "POST", &base, Some(json!({"kind": "pagerduty", "routing_key": "r0ut1ngk3y", "events": ["deploy.failed", "alert.memory"]}))).await;
    assert_eq!((st, pd["kind"].as_str(), pd.get("secret")), (StatusCode::CREATED, Some("pagerduty"), None), "{pd}");
    let (st, og) = call(&w.api, "POST", &base, Some(json!({"kind": "opsgenie", "api_key": "g3n1e", "region": "eu", "events": ["alert.memory"]}))).await;
    assert_eq!(st, StatusCode::CREATED, "{og}");
    assert!(!call(&w.api, "GET", &base, None).await.1.to_string().contains("r0ut1ngk3y"), "the key never comes back");
    for (kind, data) in [("deploy.failed", json!({"error": "health check"})), ("alert.memory", json!({"value": 95, "limit_mb": 512, "threshold": 90})), ("alert.resolved", json!({"alert": "alert.memory"})), ("deploy.active", json!({}))] {
        let sid = id.clone();
        w.apps.db(move |s| s.add_event(kind, &sid, "production", None, &data)).await.unwrap();
    }
    // One a pass (two seconds apart) for an on-call target: in order. The first deploy's own
    // `deploy.active` (queued once the targets were there) closes nothing yet: left out.
    let pd = |seen: &[(String, axum::http::HeaderMap, Value)]| -> Vec<Value> {
        seen.iter().filter(|(u, ..)| u == "/pd").map(|(.., b)| b.clone()).skip_while(|b| b["event_action"] == "resolve").collect()
    };
    let mut seen = Vec::new();
    for _ in 0..400 {
        seen = got.lock().clone();
        if pd(&seen).len() >= 4 && seen.iter().filter(|(u, ..)| u.starts_with("/og-eu")).count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pd_calls = pd(&seen);
    let key = |k: &str| format!("roll:{id}/production/{k}");
    let actions: Vec<(String, String)> = pd_calls.iter().map(|b| (b["event_action"].as_str().unwrap().to_string(), b["dedup_key"].as_str().unwrap().to_string())).collect();
    assert_eq!(actions, vec![("trigger".into(), key("deploy.failed")), ("trigger".into(), key("alert.memory")), ("resolve".into(), key("alert.memory")), ("resolve".into(), key("deploy.failed"))], "the next good deploy closes the failed one's: {pd_calls:?}");
    assert_eq!((pd_calls[0]["routing_key"].as_str(), pd_calls[0]["payload"]["severity"].as_str()), (Some("r0ut1ngk3y"), Some("error")), "{:?}", pd_calls[0]);
    // Opsgenie (its EU region): an alert by alias, closed by alias.
    let og_calls: Vec<&(String, axum::http::HeaderMap, Value)> = seen.iter().filter(|(u, ..)| u.starts_with("/og-eu")).collect();
    assert_eq!(og_calls.len(), 2, "{seen:?}");
    assert_eq!((og_calls[0].1["authorization"].to_str().unwrap(), og_calls[0].2["alias"].as_str(), og_calls[0].2["priority"].as_str()), ("GenieKey g3n1e", Some(key("alert.memory").as_str()), Some("P3")));
    assert!(og_calls[1].0.ends_with("/close?identifierType=alias") && og_calls[1].0.contains("alert.memory"), "{}", og_calls[1].0);
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
    assert_eq!(hook["events"], json!(["check.down", "heartbeat.missed", "deploy.failed", "rollout.waiting", "deploy.ready", "error.new", "crash_loop", "spend.alert", "spend.limit", "alert.memory", "alert.errors", "alert.latency", "alert.capacity", "alert.log", "cron.failed", "domain.expiring", "domain.expired", "domain.tls_failed", "volume.backup_failed", "volume.full"]), "the default events");
    let (_, down) = call(&w.api, "POST", &base, Some(json!({"kind": "webhook", "url": format!("http://{bad_at}/x"), "events": ["deploy.failed"]}))).await;
    let (_, list) = call(&w.api, "GET", &base, None).await;
    assert!(!list.to_string().contains(&secret) && !list.to_string().contains("/hook"), "neither the secret nor the URL comes back: {list}");
    assert_eq!(call(&w.api, "POST", &format!("{base}/{}/test", hook["id"].as_str().unwrap()), None).await.1["ok"], true);
    // Edited in place: what's given changes, the rest (and its URL and secret) stays.
    let (st, edited) = call(&w.api, "PATCH", &format!("{base}/{}", down["id"].as_str().unwrap()), Some(json!({"events": ["deploy.failed", "crash_loop"], "environments": ["production"]}))).await;
    assert_eq!(st, StatusCode::OK, "{edited}");
    assert_eq!((&edited["events"], &edited["environments"], &edited["host"]), (&json!(["deploy.failed", "crash_loop"]), &json!(["production"]), &down["host"]), "{edited}");
    let (st, v) = call(&w.api, "PATCH", &format!("{base}/{}", down["id"].as_str().unwrap()), Some(json!({"url": "https://example.com/x"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "a new URL is a new target: {v}");
    call(&w.api, "PATCH", &format!("{base}/{}", down["id"].as_str().unwrap()), Some(json!({"environments": null}))).await;
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
    // Tried at once, as the other was (the eight at a time run side by side: a moment apart).
    let log = get_until(&w.api, &format!("{base}/{}/deliveries", down["id"].as_str().unwrap()), |l| l[0]["attempts"].as_u64().is_some_and(|n| n >= 1)).await;
    let first = &log.as_array().unwrap()[0];
    assert_eq!((first["status"].as_str(), first["attempts"].as_u64(), first["code"].as_u64()), (Some("pending"), Some(1), Some(500)), "{log}");
    // Targets are checked: not a URL that isn't one, not Slack anywhere but Slack.
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": "ftp://x"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": "https://hooks.example.com/x", "events": ["nope"]}))).await.0, StatusCode::BAD_REQUEST);
    // A target follows the environments and services it names, and leaves previews out unless
    // asked: each pull request's failed deploys don't reach the team's channel.
    assert_eq!(hook["previews"], false, "{hook}");
    let (_, staging) = call(&w.api, "POST", &base, Some(json!({"url": format!("http://{ok_at}/staging"), "events": ["deploy.failed"], "environments": ["staging"]}))).await;
    let (_, by_name) = call(&w.api, "POST", &base, Some(json!({"url": format!("http://{ok_at}/api"), "events": ["deploy.failed"], "services": ["api"], "previews": true}))).await;
    assert_eq!(by_name["services"], json!([id]), "kept by id: {by_name}");
    assert_eq!(call(&w.api, "POST", &base, Some(json!({"url": format!("http://{ok_at}/x"), "services": ["nope"]}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "PUT", &format!("/v1/apps/services/{id}/environments/pr-1"), Some(json!({"ephemeral": true}))).await.0, StatusCode::OK);
    let sid = id.to_string();
    w.apps.db(move |s| s.add_event("deploy.failed", &sid, "pr-1", None, &json!({"error": "a preview's"}))).await.unwrap();
    let sid = id.to_string();
    w.apps.db(move |s| s.add_event("deploy.failed", &sid, "production", None, &json!({"error": "production's"}))).await.unwrap();
    let seqs = |env: &'static str| {
        let api = w.api.clone();
        async move {
            let ev = call(&api, "GET", "/v1/apps/events?after=0&limit=1000", None).await.1;
            ev["events"].as_array().unwrap().iter().filter(|e| e["environment"] == env && e["data"]["error"].is_string()).map(|e| e["seq"].as_i64().unwrap()).collect::<Vec<_>>()
        }
    };
    let (pr, prod) = (seqs("pr-1").await, seqs("production").await);
    let (pr, prod) = (vec![*pr.last().unwrap()], vec![*prod.last().unwrap()]);
    let got_of = |t: &Value| format!("{base}/{}/deliveries", t["id"].as_str().unwrap());
    let has = |log: &Value, seq: i64| log.as_array().unwrap().iter().any(|d| d["event"] == seq);
    let log = get_until(&w.api, &got_of(&by_name), |l| has(l, pr[0]) && has(l, prod[0])).await;
    assert!(has(&log, pr[0]) && has(&log, prod[0]), "the service's, previews too: {log}");
    let log = get_until(&w.api, &got_of(&hook), |l| has(l, prod[0])).await;
    assert!(!has(&log, pr[0]), "not a preview's: {log}");
    let log = call(&w.api, "GET", &got_of(&staging), None).await.1;
    assert!(!has(&log, pr[0]) && !has(&log, prod[0]), "staging's only: {log}");
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
    // A manual rollout (Vercel's manual rolling releases): its step passes, then it waits for a
    // person; advanced past its last step it's live, rolled back by one it fails.
    call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"unset": ["FAKE_UNHEALTHY"]}))).await;
    let (st, s) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"rollout": {"steps": [50], "step_s": 1, "error_pct": 20, "manual": true}}}))).await;
    assert_eq!(st, StatusCode::OK, "{s}");
    let (_, d4) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d4 = d4["id"].as_str().unwrap().to_string();
    let held = get_until(&w.api, &base, |e| e["canary"]["waiting"] == true).await;
    assert_eq!((held["canary"]["deployment"].as_str(), held["active"]["id"].as_str()), (Some(d4.as_str()), d2["id"].as_str()), "held at 50%, the old one still live: {held}");
    let events = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    assert!(events["events"].as_array().unwrap().iter().any(|e| e["kind"] == "rollout.waiting" && e["deployment"] == d4.as_str() && e["data"]["share"] == 50), "someone is told: {events}");
    let act = |dep: String, action: &'static str| {
        let (api, base) = (w.api.clone(), base.clone());
        async move { call(&api, "POST", &format!("{base}/deploys/{dep}/rollout"), Some(json!({ "action": action }))).await.0 }
    };
    assert_eq!(act(d4.clone(), "advance").await, StatusCode::ACCEPTED);
    assert_eq!(settled(&w, &id, "production", &d4).await["status"], "active");
    assert_eq!(act(d4.clone(), "advance").await, StatusCode::CONFLICT, "not waiting any more");
    let (_, d5) = call(&w.api, "POST", &format!("{base}/deploys"), Some(json!({}))).await;
    let d5 = d5["id"].as_str().unwrap().to_string();
    get_until(&w.api, &base, |e| e["canary"]["waiting"] == true).await;
    assert_eq!(act(d5.clone(), "abort").await, StatusCode::ACCEPTED);
    let d5 = settled(&w, &id, "production", &d5).await;
    assert!(d5["status"] == "failed" && d5["error"].as_str().unwrap().contains("by a person"), "{d5}");
    assert_eq!(call(&w.api, "GET", &base, None).await.1["active"]["id"].as_str(), Some(d4.as_str()));
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
    let events = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    assert!(events["events"].as_array().unwrap().iter().any(|e| e["kind"] == "deploy.ready" && e["deployment"] == d2["id"]), "someone is told it waits: {events}");
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
    // Two crash while it's caught up (a target of one): they go, none started again to be
    // scaled in, and what runs is left as it is.
    let running: Vec<String> = w.fake.instances(&id).into_iter().filter(|(_, _, st)| *st == FakeState::Running).map(|(n, ..)| n).collect();
    for n in &running[..2] {
        w.fake.crash(n);
    }
    let starts = Fake::count(&w.fake.counts.starts);
    for _ in 0..5 {
        live.tick().await;
    }
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    let states: Vec<&str> = env["instances"].as_array().unwrap().iter().map(|i| i["state"].as_str().unwrap()).collect();
    assert!(!states.contains(&"crashed"), "{states:?}");
    assert_eq!(Fake::count(&w.fake.counts.starts), starts, "none started again");
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

/// A worker with a `health_command` whose check fails (a consumer that hung, its process up)
/// is replaced as a crash and says why; its check goes to the runtime with the interval and
/// failures it was given, and a web service can't have one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_whose_health_command_fails_is_replaced() {
    let w = world_with(|c| c.live.restart_backoff = Duration::from_millis(5)).await;
    let (st, bad) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "webcheck", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80, "health_command": "true"} }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}");
    let (svc, d) = image_service(&w, "consumer", json!({"type": "worker", "min_instances": 1, "health_command": "test -f /tmp/alive", "health_every_s": 5, "health_failures": 2})).await;
    assert_eq!(d["status"], "active", "{d}");
    let id = svc["id"].as_str().unwrap().to_string();
    let live = w.apps.live(&id, "production");
    let running = |w: &World| w.fake.instances(&id).into_iter().filter(|(_, _, st)| *st == FakeState::Running).map(|(n, _, _)| n).collect::<Vec<_>>();
    let first = running(&w);
    assert_eq!(first.len(), 1, "{:?}", w.fake.instances(&id));
    let h = w.fake.spec(&first[0]).unwrap().health.unwrap();
    assert_eq!((h.command.as_str(), h.every, h.failures), ("test -f /tmp/alive", Duration::from_secs(5), 2));
    let starts = w.fake.counts.starts.load(Ordering::SeqCst);
    w.fake.fail_health(&first[0]);
    let mut replaced = false;
    for _ in 0..200 {
        live.tick().await;
        if running(&w).len() == 1 && w.fake.counts.starts.load(Ordering::SeqCst) > starts {
            replaced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(replaced, "started again: {:?}", w.fake.instances(&id));
    let log = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs"), None).await.1.to_string();
    assert!(log.contains("its health command failed 2 times in a row"), "{log}");
}

/// A sealed variable's value is `***` in the environment's log whoever prints it (the app, a
/// trace, roll), and a link-free unsealed one shows as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sealed_value_never_reaches_the_log() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let vars = format!("/v1/apps/services/{id}/environments/production/variables");
    let (st, b) = call(&w.api, "PATCH", &vars, Some(json!({"set": {"STRIPE_KEY": "sk_live_51Habc", "DATABASE_URL": "postgres://app:hunter2pw@db:5432/app", "GREETING": "hello-world"}, "seal": ["STRIPE_KEY"]}))).await;
    assert!(st.is_success(), "{b}");
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    let out = w.apps.logs.sink(format!("env:{id}/production"), apps_control::logs::Stream::Stdout).instance("roll-apps-x-0", d["id"].as_str().unwrap());
    out.line("charging with sk_live_51Habc via postgres://app:hunter2pw@db:5432/app, hello-world");
    let log = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs"), None).await.1.to_string();
    assert!(log.contains("charging with *** via postgres://app:***@db:5432/app, hello-world"), "{log}");
    assert!(!log.contains("sk_live_51Habc") && !log.contains("hunter2pw"), "{log}");
}

/// An environment's route sends a path to another service of the project (its environment of
/// the same name, the path as it came); other paths stay its own, and a route must name a
/// service the project has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_sends_a_path_to_another_service() {
    let w = world().await;
    let (web, _) = image_service(&w, "web", json!({"port": 80})).await;
    let (api, _) = image_service(&w, "api", json!({"port": 80})).await;
    let base = format!("/v1/apps/services/{}/environments/production", web["id"].as_str().unwrap());
    let (st, e) = call(&w.api, "PUT", &base, Some(json!({"routes": [{"path": "/api/*", "service": "api"}]}))).await;
    assert_eq!(st, StatusCode::OK, "{e}");
    assert_eq!(e["routes"], json!([{"path": "/api/*", "service": "api"}]));
    let h = host(&web["environments"][0]["url"]);
    let (st, b) = visit(&w, &h, "/api/users?x=1").await;
    let api_dep = call(&w.api, "GET", &format!("/v1/apps/services/{}/environments/production", api["id"].as_str().unwrap()), None).await.1["active"]["id"].clone();
    assert_eq!((st, &b["deployment"], b["path"].as_str()), (StatusCode::OK, &api_dep, Some("/api/users")), "{b}");
    let (_, b) = visit(&w, &h, "/about").await;
    assert_ne!(b["deployment"], api_dep, "its own paths stay its own: {b}");
    for bad in [json!([{"path": "/x/*", "service": "nope"}]), json!([{"path": "/x/*", "service": "web"}]), json!([{"path": "/a*b", "service": "api"}])] {
        assert_eq!(call(&w.api, "PUT", &base, Some(json!({ "routes": bad }))).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
}

/// A built service keeps the digests of the images it builds on; one that moves in its registry
/// is said once (the environment's `base_updated`, `deploy.base_updated`), and with
/// `rebuild_on_base` the same revision is built again without the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moved_base_image_is_said_and_built_again() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let d1 = settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let base = d1["plan"]["bases"][0]["image"].as_str().unwrap_or_else(|| panic!("{d1}")).to_string();
    let env = format!("/v1/apps/services/{id}/environments/production");
    assert!(w.apps.watch_bases().await.is_empty());
    assert!(call(&w.api, "GET", &env, None).await.1.get("base_updated").is_none(), "nothing moved yet");
    w.fake.move_tag(&base, &format!("sha256:{}", "1".repeat(64)));
    assert!(w.apps.watch_bases().await.is_empty(), "said, not built: rebuild_on_base is off");
    w.apps.watch_bases().await;
    assert_eq!(call(&w.api, "GET", &env, None).await.1["base_updated"], json!([base]));
    let events = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    assert_eq!(events["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "deploy.base_updated").count(), 1, "once per new digest: {events}");
    let (st, _) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"rebuild_on_base": true}}))).await;
    assert!(st.is_success());
    w.fake.move_tag(&base, &format!("sha256:{}", "2".repeat(64)));
    let started = w.apps.watch_bases().await;
    assert_eq!(started.len(), 1, "built again");
    let d = settled(&w, &id, "production", &started[0]).await;
    assert_eq!((d["revision"].clone(), d["status"].as_str()), (d1["revision"].clone(), Some("active")), "the same revision: {d}");
    assert!(w.fake.built.lock().last().unwrap().0, "without the cache");
}

/// A closed environment builds on its project's closed build network (out only through roll's
/// proxy, to public hosts) and gets none of its secret values, only the rest; its instances still
/// get them all. An open one builds on the default network.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_environments_build_gets_no_secret_values() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let env = format!("/v1/apps/services/{id}/environments/production");
    let (st, b) = call(&w.api, "PATCH", &format!("{env}/variables"), Some(json!({"set": {"API_TOKEN": "tok_9f8e7d6c5b", "DATABASE_URL": "postgres://app:tok_9f8e7d6c5b@db/app", "GREETING": "hello-world"}, "seal": ["API_TOKEN"]}))).await;
    assert!(st.is_success(), "{b}");
    // A deploy that asks to be built closed (roll previews' for a fork's head) is refused while
    // the environment is open; a node without the field refuses it as unknown.
    assert_eq!(call(&w.api, "POST", &format!("{env}/deploys"), Some(json!({"closed": true}))).await.0, StatusCode::CONFLICT);
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({"egress": {"mode": "roll"}}))).await;
    assert_eq!((st, e["build_closed"].as_bool()), (StatusCode::OK, Some(true)), "{e}");
    let (st, d) = call(&w.api, "POST", &format!("{env}/deploys"), Some(json!({"closed": true}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    let vars = w.fake.build_vars.lock().last().cloned().unwrap();
    assert!(vars.contains(&"GREETING".to_string()) && !vars.contains(&"API_TOKEN".to_string()) && !vars.contains(&"DATABASE_URL".to_string()), "{vars:?}");
    let nets = w.fake.build_networks.lock().clone();
    assert_eq!((nets.first(), nets.last()), (Some(&None), Some(&Some(format!("roll-apps-build-{}", w.project.strip_prefix("prj_").unwrap_or(&w.project))))), "{nets:?}");
}

/// A build whose inputs were built before in the project is skipped (Railway's skipped builds):
/// a second service from the same repository runs the first's image, and a changed build
/// variable builds again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_inputs_are_built_once() {
    let w = world().await;
    let builds = || w.fake.counts.builds.load(Ordering::SeqCst);
    let make = |name: &'static str| {
        let api = w.api.clone();
        let project = w.project.clone();
        async move { call(&api, "POST", "/v1/apps/services", Some(json!({ "project": project, "name": name, "source": {"kind": "repo", "repo": "acme/site"} }))).await.1 }
    };
    let web = make("web").await;
    let d1 = settled(&w, web["id"].as_str().unwrap(), "production", web["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let after_first = builds();
    let worker = make("worker").await;
    let wid = worker["id"].as_str().unwrap().to_string();
    let d2 = settled(&w, &wid, "production", worker["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!((builds(), d2["image"].clone()), (after_first, d1["image"].clone()), "the same source and settings: no build, the same image");
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{wid}/environments/production/deploys/{}/logs", d2["id"].as_str().unwrap()), None).await;
    assert!(log.to_string().contains("no build"), "it says so: {log}");
    call(&w.api, "PATCH", &format!("/v1/apps/services/{wid}/environments/production/variables"), Some(json!({"set": {"NEXT_PUBLIC_API": "https://api.example.com"}}))).await;
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{wid}/environments/production/deploys"), Some(json!({}))).await;
    settled(&w, &wid, "production", d["id"].as_str().unwrap()).await;
    assert_eq!(builds(), after_first + 1, "a build variable changed: built again");
}

/// A crowd at a sleeping service wakes one instance; those waiting on a start slower than the
/// wake timeout get a 503 page that says when to try again, and the retry gets the app.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crowd_wakes_one_instance_and_a_slow_start_says_when_to_retry() {
    // Ten times the wake timeout to start: a crowd held up by a busy machine still meets it.
    let w = world_with(|c| c.live.wake_timeout = Duration::from_millis(300)).await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "slow", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80}, "variables": {"FAKE_BOOT_MS": "3000"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await["status"], "active");
    let h = host(&svc["environments"][0]["url"]);
    // The deploy reads as active once the store switched; its instance joins the live state a
    // moment later (its commit). Until then a request (this test's reads too) adopts the
    // deployment with a warm, never-started one, and a crowd waking that keeps the deploy's own
    // up: so this waits for the deploy's, the one its health check started.
    let live = w.apps.live(&id, "production");
    let joined = || live.instances().iter().any(|i| i.started_at().is_some());
    for _ in 0..500 {
        if joined() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(joined(), "the deploy's instance joined: {:?}", live.instances().iter().map(|i| (i.name.clone(), i.state())).collect::<Vec<_>>());
    // Asleep first, and stopped (a rested instance may be frozen, which resumes at once).
    live.sleep().await;
    get_until(&w.api, &format!("/v1/apps/services/{id}/environments/production"), |e| e["state"] == "stopped").await;
    let get = || async {
        let req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
        let resp = w.public.clone().oneshot(req).await.unwrap();
        (resp.status(), resp.headers().get("retry-after").map(|v| v.to_str().unwrap().to_string()))
    };
    let created = Fake::count(&w.fake.counts.creates);
    let before: Vec<(String, apps_control::model::InstanceState)> = w.apps.live(&id, "production").instances().iter().map(|i| (i.name.clone(), i.state())).collect();
    let crowd = futures::future::join_all((0..8).map(|_| get())).await;
    // The one that woke it waits for its start; those behind it give up at the timeout.
    let waited = crowd.iter().filter(|(st, retry)| *st == StatusCode::SERVICE_UNAVAILABLE && retry.is_some()).count();
    // It failed once (all eight 200 at once, an instance already up): what was there says why.
    let c = &w.fake.counts;
    assert!(
        waited >= 7 && crowd.iter().all(|(st, _)| *st == StatusCode::OK || *st == StatusCode::SERVICE_UNAVAILABLE),
        "{crowd:?}; instances before the crowd {before:?}; fake creates {} starts {} resumes {}",
        Fake::count(&c.creates),
        Fake::count(&c.starts),
        Fake::count(&c.resumes)
    );
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
    let (logs, logkey) = (&w.apps.logs, format!("env:{id}/production"));
    let logkey = &logkey;
    let until = |want: &'static str| async move {
        for _ in 0..100 {
            if state().await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let log: Vec<String> = logs.follow(logkey, 30).0.into_iter().map(|l| l.text).collect();
        panic!("never {want}: {} {log:?}", state().await);
    };
    // Live in the store a moment before the environment has it.
    until("stopped").await;
    for _ in 0..100 {
        if !w.fake.instances(&id).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let name = w.fake.instances(&id)[0].0.clone();
    let live = w.apps.live(&id, "production");
    // A caller connects while it's asleep.
    w.fake.connect(&name);
    until("running").await;
    // Connected: in use, however long since rolld's last request. Checked in its last two idle
    // seconds before it would freeze (29 s of its 30), as the check runs every second there; a
    // jump past 30 would let the reaper's own tick freeze it first.
    w.fake.set_private(&name, 1);
    live.backdate(Duration::from_secs(29));
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
    for _ in 0..2 {
        assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{aid}"), None).await.0, StatusCode::NO_CONTENT);
    }
    assert_eq!(call(&w.api, "GET", &links, None).await.1, json!([]), "gone with the service, for good");
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
    // Live in the store a moment before it answers: the slow request goes to it, not to the
    // old deployment's instance, which its drain stops.
    for _ in 0..500 {
        if visit(&w, &h, "/").await.1["deployment"] == r["id"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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
        // roll secrets' long poll: the digest once it differs from `since`.
        async fn wait_digest(&self, p: &str, e: &str, since: &str, wait: Duration) -> anyhow::Result<Option<String>> {
            let until = std::time::Instant::now() + wait;
            loop {
                let now = self.digest(p, e).await?;
                if now.as_deref() != Some(since) || std::time::Instant::now() >= until {
                    return Ok(now);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        async fn resolve(&self, _: &str, _: &str, _: &str) -> anyhow::Result<Vec<apps_control::integrations::SecretVar>> {
            let pw = match VERSION.load(std::sync::atomic::Ordering::SeqCst) {
                1 => "s3cret".to_string(),
                n => format!("s3cret-{n}"),
            };
            Ok(vec![
                apps_control::integrations::SecretVar { name: "DB_PASSWORD".into(), value: pw, linked: false, sensitive: false, plain: false },
                apps_control::integrations::SecretVar { name: "SIGNING_KEY".into(), value: "only-for-runtimes".into(), linked: false, sensitive: true, plain: false },
            ])
        }
        async fn may_reveal(&self, who: apps_control::integrations::Asker<'_>, _: &str, _: &str) -> bool {
            who.operator
        }
        async fn ensure(&self, _: &str, env: &str) -> anyhow::Result<()> {
            ENSURED.lock().push(env.to_string());
            Ok(())
        }
    }
    static ENSURED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let w = world_full(|_| {}, false, |d| d.secrets = Arc::new(FakeSecrets)).await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    // A project secret is hidden in the log whatever its name or tag (GitHub Actions' rule).
    let sid = svc["id"].as_str().unwrap();
    w.apps.logs.sink(format!("env:{sid}/production"), apps_control::logs::Stream::Stdout).line("connecting with s3cret");
    let log = call(&w.api, "GET", &format!("/v1/apps/services/{sid}/environments/production/logs"), None).await.1.to_string();
    assert!(log.contains("connecting with ***") && !log.contains("s3cret"), "{log}");
    // A new environment's secrets have somewhere to go; a preview's are roll previews' to make.
    let envs = format!("/v1/apps/services/{}/environments", svc["id"].as_str().unwrap());
    call(&w.api, "PUT", &format!("{envs}/qa"), Some(json!({}))).await;
    call(&w.api, "PUT", &format!("{envs}/pr-1"), Some(json!({"ephemeral": true}))).await;
    assert_eq!(*ENSURED.lock(), ["production", "qa"]);
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
    // Set to redeploy on a change: its own long poll on the owner does it, once, within moments.
    assert!(w.apps.watch_secrets().await.is_empty(), "off by default");
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"redeploy_on_secrets": true}))).await;
    assert_eq!((st, &env["redeploy_on_secrets"]), (StatusCode::OK, &json!(true)), "{env}");
    // The redeploys the watch made, newest first, each once it's live.
    let sid = svc["id"].as_str().unwrap().to_string();
    let redeploys = |n: usize| {
        let (api, base) = (w.api.clone(), base.clone());
        async move {
            for _ in 0..300 {
                let (_, list) = call(&api, "GET", &format!("{base}/deploys"), None).await;
                let made: Vec<Value> = list.as_array().unwrap().iter().filter(|d| d["message"] == "the project's secrets changed").cloned().collect();
                if made.len() >= n && made.iter().all(|d| !["queued", "building", "deploying"].contains(&d["status"].as_str().unwrap_or(""))) {
                    return made;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("{n} redeploys never came");
        }
    };
    let made = redeploys(1).await;
    assert_eq!((made.len(), made[0]["status"].as_str(), made[0]["trigger"].as_str()), (1, Some("active"), Some("settings")), "{made:?}");
    assert_eq!(call(&w.api, "GET", &format!("{base}/pending"), None).await.1["secrets"], false);
    assert!(w.apps.watch_secrets().await.is_empty(), "nothing moved since: the minute's pass adds nothing");
    // Back to the first set: a new redeploy (not the first one's, long superseded).
    VERSION.store(1, std::sync::atomic::Ordering::SeqCst);
    let made = redeploys(2).await;
    assert_eq!((made.len(), made[0]["status"].as_str()), (2, Some("active")), "{made:?}");
    // And to the second set again: its key once answered the first redeploy; now a new one.
    VERSION.store(2, std::sync::atomic::Ordering::SeqCst);
    let made = redeploys(3).await;
    assert_eq!((made.len(), made[0]["status"].as_str()), (3, Some("active")), "{made:?}");
    settled(&w, &sid, "production", made[0]["id"].as_str().unwrap()).await;
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

/// `run.cpu_target` and `run.memory_target`: a web service whose instances stay over the target
/// for the window gets another (one at a time), and goes back down once they'd be under it.
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
    let busy = running(&w, &id)[0].clone();
    w.fake.set_busy(&busy, 0.9);
    until(&w, &id, 2).await;
    // Each instance's own CPU and memory, its last hour by minute (Railway's per replica).
    let (_, m) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/metrics?minutes=60"), None).await;
    let each = m["by_instance"].as_array().unwrap_or_else(|| panic!("{m}"));
    let of = |n: &str| each.iter().find(|x| x["instance"] == n).cloned().unwrap_or_else(|| panic!("{n} in {m}"));
    let hot = of(&busy);
    assert!(hot["cpu"].as_array().unwrap().last().unwrap().as_f64().unwrap() > 0.1, "{hot}");
    assert_eq!(hot["memory_bytes"].as_array().unwrap().last().unwrap().as_u64(), Some(64 << 20), "{hot}");
    assert_eq!(hot["minutes"].as_array().unwrap().len(), hot["cpu"].as_array().unwrap().len());
    for n in running(&w, &id) {
        w.fake.set_busy(&n, 0.05);
    }
    until(&w, &id, 1).await;
    // On memory too (Render's target memory): over 70% of its 512 MB, another; under, back.
    assert_eq!(call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"memory_target": 101}}))).await.0, StatusCode::BAD_REQUEST);
    let (st, s) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"cpu_target": null, "memory_target": 70}}))).await;
    assert_eq!(st, StatusCode::OK, "{s}");
    let (_, r) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", r["id"].as_str().unwrap()).await;
    until(&w, &id, 1).await;
    w.fake.set_memory(&running(&w, &id)[0], 480 << 20);
    until(&w, &id, 2).await;
    for n in running(&w, &id) {
        w.fake.set_memory(&n, 64 << 20);
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
    // One abusive network, a bot's header, a cookie's value and a query parameter (Vercel's custom rules).
    let rules = json!([
        {"action": "deny", "ips": ["203.0.113.9/24", "2001:db8::1"]},
        {"action": "deny", "header": {"name": "X-Scanner"}},
        {"action": "deny", "cookie": {"name": "role", "contains": "Intruder"}},
        {"action": "deny", "query": {"name": "debug"}, "path": "/api/*"},
    ]);
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"firewall": rules}))).await;
    assert_eq!((st, &env["firewall"][0]["ips"]), (StatusCode::OK, &json!(["203.0.113.0/24", "2001:db8::1/128"])), "kept canonical: {env}");
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "deny", "ips": ["not-an-ip"]}]}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "deny", "header": {"name": "a b"}}]}))).await.0, StatusCode::BAD_REQUEST);
    let ask = |from: &str, path: &str, headers: &[(&str, &str)]| {
        let mut r = Request::builder().uri(path).header("host", h.as_str());
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        let mut r = r.body(Body::empty()).unwrap();
        r.extensions_mut().insert(axum::extract::ConnectInfo(from.parse::<std::net::SocketAddr>().unwrap()));
        let public = w.public.clone();
        async move { public.oneshot(r).await.unwrap().status() }
    };
    assert_eq!(ask("203.0.113.77:5000", "/", &[]).await, StatusCode::FORBIDDEN, "its network");
    assert_eq!(ask("[2001:db8::1]:5000", "/", &[]).await, StatusCode::FORBIDDEN);
    assert_eq!(ask("198.51.100.1:5000", "/", &[]).await, StatusCode::OK, "another address");
    assert_eq!(ask("198.51.100.1:5000", "/", &[("x-scanner", "1")]).await, StatusCode::FORBIDDEN, "the header, any case");
    assert_eq!(ask("198.51.100.1:5000", "/", &[("cookie", "a=1; role=intruder-x")]).await, StatusCode::FORBIDDEN, "a cookie holding it");
    assert_eq!(ask("198.51.100.1:5000", "/", &[("cookie", "a=1; role=admin")]).await, StatusCode::OK);
    assert_eq!(ask("198.51.100.1:5000", "/api/items?debug", &[]).await, StatusCode::FORBIDDEN, "the parameter, on its path");
    assert_eq!(ask("198.51.100.1:5000", "/items?debug", &[]).await, StatusCode::OK, "every field must match");
    // A challenge: a browser that worked out the page's proof gets in, nothing else does.
    let (st, e) = call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "challenge", "path": "/*"}]}))).await;
    assert_eq!(st, StatusCode::OK, "{e}");
    let page = w.public.clone().oneshot(Request::builder().uri("/shop?x=1").header("host", h.as_str()).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(page.status(), StatusCode::FORBIDDEN);
    let html = String::from_utf8(page.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    let seed = html.split("const seed=\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or_else(|| panic!("{html}")).to_string();
    assert!(html.contains("\"/shop?x=1\""), "it goes back where it was asked: {html}");
    use sha2::Digest;
    let ok = |n: &u64| sha2::Sha256::digest(format!("{seed}:{n}").as_bytes()).iter().take(3).fold((0u32, true), |(z, go), b| if !go { (z, false) } else if *b == 0 { (z + 8, true) } else { (z + b.leading_zeros(), false) }).0 >= apps_control::challenge::BITS;
    let nonce = (0u64..).find(ok).unwrap();
    let answer = |nonce: u64| Request::builder().method("POST").uri(apps_control::challenge::PATH).header("host", h.as_str()).header("content-type", "application/x-www-form-urlencoded").body(Body::from(format!("seed={seed}&nonce={nonce}"))).unwrap();
    assert_eq!(w.public.clone().oneshot(answer(nonce + 1)).await.unwrap().status(), StatusCode::FORBIDDEN, "a wrong answer");
    let passed = w.public.clone().oneshot(answer(nonce)).await.unwrap();
    assert_eq!(passed.status(), StatusCode::NO_CONTENT);
    let cookie = passed.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let with = w.public.clone().oneshot(Request::builder().uri("/shop").header("host", h.as_str()).header("cookie", &cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(with.status(), StatusCode::OK, "in with its pass");
    let seen: Value = serde_json::from_slice(&with.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(!seen["cookie"].as_str().unwrap_or("").contains("roll_pass_"), "the pass is roll's: {seen}");
    // A limit rule: its own rate a client, the rest of the site untouched.
    let (st, e) = call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "limit", "path": "/login", "per_minute": 1, "burst": 2}]}))).await;
    assert_eq!(st, StatusCode::OK, "{e}");
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(req("GET", "/login", "curl").await);
    }
    assert_eq!(got, [StatusCode::OK, StatusCode::OK, StatusCode::TOO_MANY_REQUESTS], "its burst through, the next held");
    assert_eq!(req("GET", "/", "curl").await, StatusCode::OK, "other paths aren't limited");
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "limit", "path": "/x"}]}))).await.0, StatusCode::BAD_REQUEST, "a limit says its rate");
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"firewall": [{"action": "deny", "path": "/x", "per_minute": 5}]}))).await.0, StatusCode::BAD_REQUEST);
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
    // A redeploy says what it puts live (Heroku's "Set MODE config vars"), names only.
    let (_, d) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    assert_eq!(d["message"], "Redeploy: ADDED added; MODE changed; GONE removed; run.memory_mb", "{d}");
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
    let grant = roll_auth::Grant { id: "tok_t".into(), name: "ci".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(scope), user: None, ask_otherwise: false };
    let mut req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/apps/services/{id}/environments/production/variables"))
        .header("content-type", "application/json")
        .body(Body::from(json!({"set": {"STOLEN": "${{ db.PASS }}"}}).to_string()))
        .unwrap();
    // As rolld's gate sends an operator's token on: the grant, and the operator marker beside it.
    req.extensions_mut().insert(grant);
    req.extensions_mut().insert(roll_auth::Operator);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compose_file_is_read_as_a_project_without_making_anything() {
    let w = world().await;
    let compose = "services:\n  web:\n    image: ghcr.io/acme/web:1\n    ports: [\"8080:3000\"]\n    environment: [\"MODE=prod\", \"DATABASE_URL=postgres://u:p@db:5432/x\"]\n    depends_on: [db]\n  db:\n    image: postgres:17\n";
    let (st, v) = call(&w.api, "POST", "/v1/apps/import", Some(json!({ "compose": compose }))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let web = &v["apps"]["web"];
    assert_eq!(web["source"], json!({ "kind": "image", "image": "ghcr.io/acme/web:1" }), "{v}");
    assert_eq!((web["run"]["port"].as_u64(), &web["links"], &web["variables"]), (Some(3000), &json!([{ "kind": "postgres", "name": "db" }]), &json!({ "MODE": "prod" })), "{v}");
    assert_eq!(v["databases"]["db"]["kind"], "postgres", "{v}");
    assert!(v["roll_toml"].as_str().unwrap().contains("[apps.web]") && v["notes"].as_array().is_some_and(|n| !n.is_empty()), "{v}");
    // Read, not made; and a file that isn't a project says why.
    let (_, all) = call(&w.api, "GET", "/v1/apps/services", None).await;
    assert_eq!(all, json!([]));
    let (st, v) = call(&w.api, "POST", "/v1/apps/import", Some(json!({ "roll_toml": "[apps.web]\nimage = \"a\"\nrepo = \"b/c\"\n" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("app web"), "{v}");
    // Another platform's file (Render's Blueprint): its services and database, as a project.
    let render = "services:\n  - type: web\n    name: api\n    rootDir: server\n    startCommand: node index.js\n    envVars:\n      - key: DATABASE_URL\n        fromDatabase: { name: main, property: connectionString }\n      - key: SECRET\n        generateValue: true\ndatabases:\n  - name: main\n";
    let (st, v) = call(&w.api, "POST", "/v1/apps/import", Some(json!({ "render": render }))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((&v["apps"]["api"]["links"], &v["databases"]["main"]["kind"], &v["apps"]["api"]["generate"]), (&json!([{ "kind": "postgres", "name": "main" }]), &json!("postgres"), &json!(["SECRET"])), "{v}");
    assert_eq!(call(&w.api, "POST", "/v1/apps/import", Some(json!({ "render": render, "compose": "x" }))).await.0, StatusCode::BAD_REQUEST, "one file at a time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_is_made_again_from_its_own_settings() {
    // The dashboard's Duplicate sends a service's build and run back as GET gave them.
    let w = world().await;
    let run = json!({ "port": 80, "min_instances": 1, "health_path": "/healthz", "alerts": { "memory_pct": 80, "error_pct": 5, "p95_ms": 900, "log_match": "panic", "log_count": 3 }, "rollout": { "steps": [10, 50], "step_s": 60, "error_pct": 5 }, "affinity": true });
    let (a, _) = image_service(&w, "web", run).await;
    // Its own build timeout (Render's), a minute up to six hours (held to the node's at a build).
    let svc = format!("/v1/apps/services/{}", a["id"].as_str().unwrap());
    assert_eq!(call(&w.api, "PATCH", &svc, Some(json!({"build": {"timeout_s": 30}}))).await.0, StatusCode::BAD_REQUEST);
    let (st, a) = call(&w.api, "PATCH", &svc, Some(json!({"build": {"timeout_s": 1200}}))).await;
    assert_eq!((st, &a["build"]["timeout_s"]), (StatusCode::OK, &json!(1200)), "{a}");
    let (st, b) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "web-copy", "source": a["source"], "build": a["build"], "run": a["run"] }))).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!((&b["run"], &b["build"]), (&a["run"], &a["build"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repo_service_made_with_a_branch_follows_it_from_the_first_deploy() {
    // A project file's `branch` (roll up --all, the dashboard's import): set as it's made, never after.
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"}, "branch": "release" }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{}/environments/production", svc["id"].as_str().unwrap()), None).await;
    assert_eq!(env["branch"], "release", "{env}");
    let (st, v) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "img", "source": {"kind": "image", "image": "nginx:1.27"}, "branch": "main" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "a branch is a repo's: {v}");
}

/// A paused environment (Render's Suspend): a worker's instances stop and stay stopped through
/// the reaper's passes, nothing deploys, runs or wakes, and resuming brings it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_worker_stays_stopped_until_its_resumed() {
    let w = world().await;
    let (svc, _) = image_service(&w, "billing", json!({"type": "worker", "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let env = format!("/v1/apps/services/{id}/environments/production");
    get_until(&w.api, &env, |e| e["state"] == "running").await;
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({ "paused": true }))).await;
    assert_eq!((st, &e["paused"]), (StatusCode::OK, &json!(true)), "{e}");
    get_until(&w.api, &env, |e| e["state"] == "stopped").await;
    // The reaper passes and keeps no floor.
    for _ in 0..5 {
        w.apps.live(&id, "production").tick().await;
    }
    let (_, e) = call(&w.api, "GET", &env, None).await;
    assert_eq!(e["state"], "stopped", "{e}");
    let (_, list) = call(&w.api, "GET", "/v1/apps/services", None).await;
    assert!(list[0]["environments"][0]["attention"].as_array().unwrap().contains(&json!("paused")), "{list}");
    let (st, v) = call(&w.api, "POST", &format!("{env}/deploys"), Some(json!({}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "nothing deploys: {v}");
    let (st, v) = call(&w.api, "POST", &format!("{env}/jobs"), Some(json!({ "command": "true" }))).await;
    assert_eq!(st, StatusCode::CONFLICT, "nothing runs: {v}");
    // Resumed: back at its floor.
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({ "paused": false }))).await;
    assert_eq!(st, StatusCode::OK, "{e}");
    assert!(e.get("paused").is_none(), "{e}");
    get_until(&w.api, &env, |e| e["state"] == "running").await;
}

/// What an app's line says its level is (JSON, logfmt, a leading `ERROR`) filters its log:
/// `level=warn` is warnings and worse, and a line that says none isn't one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apps_log_filters_by_the_level_its_lines_say() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let out = w.apps.logs.sink(format!("env:{id}/production"), apps_control::logs::Stream::Stdout);
    for line in [r#"{"level":"info","msg":"ready"}"#, r#"{"level":"error","msg":"db down"}"#, "level=warn msg=slow", "an error in prose", "FATAL out of memory"] {
        out.line(line);
    }
    let path = format!("/v1/apps/services/{id}/environments/production/logs?level=warn&tail=100");
    let (st, lines) = call(&w.api, "GET", &path, None).await;
    assert_eq!(st, StatusCode::OK, "{lines}");
    let got: Vec<(&str, &str)> = lines.as_array().unwrap().iter().map(|l| (l["level"].as_str().unwrap_or(""), l["text"].as_str().unwrap())).collect();
    assert_eq!(got, [("error", r#"{"level":"error","msg":"db down"}"#), ("warn", "level=warn msg=slow"), ("fatal", "FATAL out of memory")]);
    let (st, v) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs?level=loud"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    // And by a line's fields, JSON's or logfmt's (Railway's @attr:value).
    let (_, lines) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs?where=msg:slow&tail=100"), None).await;
    assert_eq!(lines.as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap()).collect::<Vec<_>>(), ["level=warn msg=slow"]);
    let (_, lines) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs?where=level:*%20-level:info&tail=100"), None).await;
    assert_eq!(lines.as_array().unwrap().len(), 2, "{lines}");
}

/// A paused cron service skips what falls due and doesn't make it up when it's resumed, and
/// pausing stops a run that's going.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_cron_skips_its_runs_and_pausing_stops_one() {
    let w = world().await;
    let (_, cron) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "every-minute", "source": {"kind": "image", "image": "busybox"}, "run": {"type": "cron", "schedule": "* * * * *"} }))).await;
    let cid = cron["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &cid, "production", cron["environments"][0]["latest"]["id"].as_str().unwrap()).await["status"], "active");
    let (env, runs) = (format!("/v1/apps/services/{cid}/environments/production"), format!("/v1/apps/services/{cid}/environments/production/runs"));
    let t = (code_sql::now_ms() / 60_000 + 60) * 60_000 + 30_000;
    w.apps.cron_pass(t).await;
    call(&w.api, "PUT", &env, Some(json!({ "paused": true }))).await;
    for m in 1..=10 {
        w.apps.cron_pass(t + m * 60_000).await;
    }
    assert_eq!(call(&w.api, "GET", &runs, None).await.1, json!([]), "nothing runs while paused");
    call(&w.api, "PUT", &env, Some(json!({ "paused": false }))).await;
    w.apps.cron_pass(t + 10 * 60_000 + 1_000).await;
    w.apps.cron_pass(t + 11 * 60_000).await;
    let one = get_until(&w.api, &runs, |r| r[0]["status"] == "running").await;
    assert_eq!(one.as_array().unwrap().len(), 1, "the next one, not the ten it missed: {one}");
    // Paused with it going: it stops.
    call(&w.api, "PUT", &env, Some(json!({ "paused": true }))).await;
    let ended = get_until(&w.api, &runs, |r| r[0]["status"] != "running").await;
    assert_eq!(ended[0]["status"], "canceled", "{ended}");
}

/// A failed cron run is tried again as `run.retries` allows (Cloud Run Jobs' retries), each try
/// its own run; only the last failure is a `cron.failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_cron_run_is_retried_and_only_the_last_failure_is_an_incident() {
    let w = world_with(|c| c.cron_retry_after = Duration::from_millis(20)).await;
    let (_, cron) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "nightly", "source": {"kind": "image", "image": "busybox"}, "run": {"type": "cron", "schedule": "0 3 * * *", "retries": 2}, "variables": {"FAKE_EXIT": "1"} }))).await;
    let cid = cron["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &cid, "production", cron["environments"][0]["latest"]["id"].as_str().unwrap()).await["status"], "active");
    let runs = format!("/v1/apps/services/{cid}/environments/production/runs");
    let (st, r) = call(&w.api, "POST", &runs, Some(json!({}))).await;
    assert!(st.is_success(), "{r}");
    let all = get_until(&w.api, &runs, |r| r.as_array().is_some_and(|a| a.len() == 3 && a.iter().all(|x| x["status"] == "failed"))).await;
    let attempts: Vec<u64> = all.as_array().unwrap().iter().map(|x| x["attempt"].as_u64().unwrap()).collect();
    assert_eq!(attempts, [3, 2, 1], "newest first: {all}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(call(&w.api, "GET", &runs, None).await.1.as_array().unwrap().len(), 3, "no fourth try");
    let events = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    assert_eq!(events["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "cron.failed").count(), 1, "{events}");
    let (st, v) = call(&w.api, "PATCH", &format!("/v1/apps/services/{cid}"), Some(json!({ "run": {"retries": 9} }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
}

/// A project written as roll.toml reads back as the project: its services, their sources and the
/// settings that differ from a new one's, a variable by name and never its value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_is_written_as_roll_toml_that_reads_back() {
    let w = world().await;
    let (web, _) = image_service(&w, "web", json!({"port": 80, "min_instances": 1, "health_path": "/healthz"})).await;
    let (_, cron) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "nightly", "source": {"kind": "image", "image": "busybox"}, "run": {"type": "cron", "schedule": "0 3 * * *"} }))).await;
    assert!(cron["id"].is_string(), "{cron}");
    call(&w.api, "PATCH", &format!("/v1/apps/services/{}/environments/production/variables", web["id"].as_str().unwrap()), Some(json!({"set": {"API_KEY": "sk-live-123"}}))).await;
    // Another lasting environment, with its own size: written, and read back.
    call(&w.api, "PUT", &format!("/v1/apps/services/{}/environments/staging", web["id"].as_str().unwrap()), Some(json!({"run": {"max_instances": 1}}))).await;
    let (st, out) = call(&w.api, "POST", &format!("/v1/apps/projects/{}/export", w.project), Some(json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{out}");
    let text = out["roll_toml"].as_str().unwrap();
    assert!(text.contains("API_KEY") && !text.contains("sk-live-123"), "a variable by name, never its value:\n{text}");
    // Read back by the parser roll up --all and the import use.
    let (st, back) = call(&w.api, "POST", "/v1/apps/import", Some(json!({ "roll_toml": text }))).await;
    assert_eq!(st, StatusCode::OK, "{back}\n{text}");
    assert_eq!(back["apps"]["web"]["source"], json!({"kind": "image", "image": "nginx:1.27"}), "{back}");
    assert_eq!((back["apps"]["web"]["run"]["port"].as_u64(), back["apps"]["web"]["run"]["min_instances"].as_u64(), back["apps"]["web"]["run"]["health_path"].as_str()), (Some(80), Some(1), Some("/healthz")), "{back}");
    assert_eq!((back["apps"]["nightly"]["run"]["type"].as_str(), back["apps"]["nightly"]["run"]["schedule"].as_str()), (Some("cron"), Some("0 3 * * *")), "{back}");
    assert_eq!(back["apps"]["web"]["environments"]["staging"]["run"]["max_instances"], 1, "{back}\n{text}");
}

/// A rollback runs the old deployment's settings, but the volume is the service's as it is now:
/// rolling back past the volume's addition keeps the data mounted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rollback_keeps_the_services_volume() {
    let w = world().await;
    let (svc, first) = image_service(&w, "db", json!({"type": "worker"})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"volume": {"mount": "/data", "size_gb": 1}}}))).await;
    let (_, second) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    assert_eq!(settled(&w, &id, "production", second["id"].as_str().unwrap()).await["status"], "active");
    let (st, back) = call(&w.api, "POST", &format!("{base}/deploys/{}/rollback", first["id"].as_str().unwrap()), Some(json!({}))).await;
    assert!(st.is_success(), "{back}");
    let row = w.apps.store.deployment(back["id"].as_str().unwrap()).unwrap();
    assert_eq!(row.run.volume.as_ref().map(|v| v.mount.as_str()), Some("/data"), "the volume stays mounted");
}

/// After a rollback (Vercel's Instant Rollback) a push isn't put live: deploys are held until a
/// person deploys or resumes them. A person's own hold builds pushes to ready instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rollback_holds_the_next_push_until_deploys_resume() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "site", "source": {"kind": "repo", "repo": "acme/site"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let first = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &id, "production", &first).await["status"], "active");
    w.code.push("b2b2b2b2b2b2", "second", &[("server.js", "// v2")]);
    let second = get_until(&w.api, &format!("{base}/deploys"), |d| d.as_array().is_some_and(|a| a.len() == 2)).await[0]["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &id, "production", &second).await["status"], "active");
    let (_, back) = call(&w.api, "POST", &format!("{base}/deploys/{first}/rollback"), Some(json!({}))).await;
    assert_eq!(settled(&w, &id, "production", back["id"].as_str().unwrap()).await["status"], "active");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    assert_eq!(env["held_after"], back["id"], "{env}");
    // A push while held: logged, not deployed.
    w.code.push("c3c3c3c3c3c3", "third", &[("server.js", "// v3")]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, all) = call(&w.api, "GET", &format!("{base}/deploys"), None).await;
    assert!(!all.as_array().unwrap().iter().any(|d| d["revision"] == "c3c3c3c3c3c3"), "held: {all}");
    // Resumed: the hold is gone (the next push deploys).
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({ "held": false }))).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert!(env.get("held_after").is_none(), "{env}");
    w.code.push("d4d4d4d4d4d4", "fourth", &[("server.js", "// v4")]);
    let fourth = get_until(&w.api, &format!("{base}/deploys"), |d| d.as_array().is_some_and(|a| a.iter().any(|x| x["revision"] == "d4d4d4d4d4d4"))).await;
    let fourth = fourth.as_array().unwrap().iter().find(|x| x["revision"] == "d4d4d4d4d4d4").unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &id, "production", &fourth).await["status"], "active");
    // A person's own hold (Netlify's locked deploys): a push builds to ready beside what's live,
    // putting it live keeps the hold, and only `held: false` ends it.
    let (_, env) = call(&w.api, "PUT", &base, Some(json!({ "held": true }))).await;
    assert_eq!((env["held"].as_bool(), env.get("held_after")), (Some(true), None), "{env}");
    w.code.push("e5e5e5e5e5e5", "fifth", &[("server.js", "// v5")]);
    let fifth = get_until(&w.api, &format!("{base}/deploys"), |d| d.as_array().is_some_and(|a| a.iter().any(|x| x["revision"] == "e5e5e5e5e5e5"))).await;
    let fifth = fifth.as_array().unwrap().iter().find(|x| x["revision"] == "e5e5e5e5e5e5").unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &id, "production", &fifth).await["status"], "ready", "built, not live");
    assert_eq!(call(&w.api, "GET", &base, None).await.1["active"]["id"], fourth.as_str());
    let (_, live) = call(&w.api, "POST", &format!("{base}/deploys/{fifth}/rollback"), Some(json!({"keep_variables": true}))).await;
    assert_eq!(settled(&w, &id, "production", live["id"].as_str().unwrap()).await["status"], "active");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    assert_eq!((env["held"].as_bool(), env.get("held_after")), (Some(true), None), "still the person's hold: {env}");
    let (_, env) = call(&w.api, "PUT", &base, Some(json!({ "held": false }))).await;
    assert!(env.get("held").is_none(), "{env}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errors_and_latency_read_by_route() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let h = host(&svc["environments"][0]["url"]);
    for p in ["/users/1", "/users/2?tab=a", "/users/3"] {
        assert_eq!(visit(&w, &h, p).await.0, StatusCode::OK);
    }
    // Every instance it has (a loaded host can have started another by now) answers 500s.
    let all = w.fake.instances(&id);
    let inst = all[0].0.clone();
    for (i, ..) in &all {
        w.fake.sicken(i);
    }
    assert_eq!(visit(&w, &h, "/checkout").await.0, StatusCode::INTERNAL_SERVER_ERROR);
    // Ids folded, the query dropped, the failing route counted as one.
    let mut v = Value::Null;
    for _ in 0..100 {
        v = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/routes?minutes=15"), None).await.1;
        if v["requests"].as_u64() >= Some(4) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let route = |r: &str| v["routes"].as_array().unwrap().iter().find(|x| x["route"] == r).cloned().unwrap_or_else(|| panic!("{v}"));
    assert_eq!((route("/users/:id")["requests"].as_u64(), route("/users/:id")["errors"].as_u64()), (Some(3), Some(0)), "{v}");
    assert_eq!((route("/checkout")["requests"].as_u64(), route("/checkout")["errors"].as_u64(), route("/checkout")["method"].as_str()), (Some(1), Some(1), Some("GET")), "{v}");
    // The same requests by the deployment and the instance that answered.
    let live = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await.1["active"]["id"].clone();
    assert_eq!((&v["deployments"][0]["deployment"], v["deployments"][0]["requests"].as_u64(), v["deployments"][0]["errors"].as_u64()), (&live, Some(4), Some(1)), "{v}");
    // By instance: all four, the first instance's among them (a busy machine may wake a second).
    let insts = v["instances"].as_array().unwrap();
    assert_eq!(insts.iter().map(|i| i["requests"].as_u64().unwrap()).sum::<u64>(), 4, "{v}");
    assert!(insts.iter().any(|i| i["instance"] == inst.as_str()), "{v}");
}

/// What runs is billed: requests through the proxy and the instance's CPU and memory reach the
/// environment's usage at the configured prices (the spend limit's test reads that usage into
/// the month).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_and_cpu_through_the_proxy_are_billed() {
    let w = world().await;
    let (svc, _) = image_service(&w, "shop", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let h = host(&svc["environments"][0]["url"]);
    for _ in 0..3 {
        assert_eq!(visit(&w, &h, "/").await.0, StatusCode::OK);
    }
    for (name, ..) in w.fake.instances(&id) {
        w.fake.set_busy(&name, 0.5);
    }
    let mut today = Value::Null;
    for _ in 0..300 {
        w.apps.flush_meters(true).await;
        today = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/usage?days=1"), None).await.1["days"][0].clone();
        if today["requests"].as_f64() >= Some(3.0) && today["cpu_seconds"].as_f64() > Some(0.0) && today["gb_seconds"].as_f64() > Some(0.0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(today["requests"].as_f64() >= Some(3.0) && today["cpu_seconds"].as_f64() > Some(0.0) && today["gb_seconds"].as_f64() > Some(0.0), "{today}");
    assert!(today["egress_bytes"].as_f64() > Some(0.0), "{today}");
    let p = apps_control::metrics::Prices::default();
    let f = |k: &str| today[k].as_f64().unwrap();
    let usd = f("cpu_seconds") * p.cpu_second + f("gb_seconds") * p.gb_second + f("egress_bytes") / 1e9 * p.egress_gb;
    assert!((today["usd"].as_f64().unwrap() - usd).abs() < 1e-12, "priced as configured: {today}");
}

/// A deleted service stops and is hidden, kept 48 hours for a restore (Railway's): its name,
/// volume and hostname wait, and a restore brings it back as it was. Deleted again, or once the
/// prune sweep finds it past 48 hours, it goes for good and leaves nothing running or kept: its
/// volume goes (after a last backup), and the project's network once no other service uses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_service_is_restored_or_goes_for_good() {
    let w = world_full(|_| {}, true, |_| {}).await;
    let (db, _) = image_service(&w, "db", json!({"type": "worker", "volume": {"mount": "/data", "size_gb": 1}})).await;
    let (web, _) = image_service(&w, "web", json!({"port": 80, "min_instances": 1})).await;
    let url = host(&web["environments"][0]["url"]);
    let (db, web) = (db["id"].as_str().unwrap().to_string(), web["id"].as_str().unwrap().to_string());
    let volume = apps_control::volume_name(&db, "production");
    w.fake.put_volume(&volume, b"rows");
    let net = apps_control::network_name(&w.project, "production");
    assert!(w.fake.networks().contains(&net), "{:?}", w.fake.networks());
    assert_eq!(visit(&w, &url, "/").await.0, StatusCode::OK);
    // Deleted: stopped, answering nothing, listed only as deleted; its name is still its own.
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{web}"), None).await.0, StatusCode::NO_CONTENT);
    assert!(w.fake.instances(&web).iter().all(|(_, _, st)| *st != FakeState::Running), "{:?}", w.fake.instances(&web));
    assert_eq!(visit(&w, &url, "/").await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&w.api, "GET", &format!("/v1/apps/services/{web}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&w.api, "GET", &format!("/v1/apps/services/{web}/environments/production/deploys"), None).await.0, StatusCode::NOT_FOUND);
    let (_, list) = call(&w.api, "GET", &format!("/v1/apps/services?project={}", w.project), None).await;
    assert!(list.as_array().unwrap().iter().all(|s| s["id"] != web.as_str()), "{list}");
    let (_, gone) = call(&w.api, "GET", &format!("/v1/apps/services?project={}&deleted=true", w.project), None).await;
    assert_eq!((gone[0]["id"].as_str(), gone[0]["purge_at"].as_i64().unwrap() - gone[0]["deleted_at"].as_i64().unwrap()), (Some(web.as_str()), 48 * 3600 * 1000), "{gone}");
    let (st, e) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "web", "source": {"kind": "image", "image": "nginx:1.27"} }))).await;
    assert!(st == StatusCode::CONFLICT && e["error"].as_str().unwrap().contains("restore it"), "{st} {e}");
    // Restored: as it was, answering at the same name.
    let (st, back) = call(&w.api, "POST", &format!("/v1/apps/services/{web}/restore"), None).await;
    assert_eq!((st, back["name"].as_str()), (StatusCode::OK, Some("web")), "{back}");
    let mut answered = StatusCode::NOT_FOUND;
    for _ in 0..200 {
        answered = visit(&w, &url, "/").await.0;
        if answered == StatusCode::OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(answered, StatusCode::OK);
    let (_, trail) = call(&w.api, "GET", &format!("/v1/apps/services/{web}/activity"), None).await;
    let kinds: Vec<&str> = trail.as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
    assert_eq!(kinds[..2], ["audit.restored", "audit.deleted"], "{trail}");
    // Deleted twice: gone for good, its volume (after a last backup) and instances with it.
    for _ in 0..2 {
        assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{db}"), None).await.0, StatusCode::NO_CONTENT);
    }
    assert_eq!((w.fake.volume(&volume), w.fake.instances(&db).len()), (None, 0), "its volume and instances");
    assert!(w.fake.networks().contains(&net), "web still uses the network");
    assert_eq!(call(&w.api, "POST", &format!("/v1/apps/services/{db}/restore"), None).await.0, StatusCode::NOT_FOUND);
    // Past 48 hours, the prune sweep deletes it for good.
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{web}"), None).await.0, StatusCode::NO_CONTENT);
    w.apps.store.set_deleted(&web, None).unwrap();
    w.apps.store.set_deleted(&web, Some(code_sql::now_ms() - 49 * 3600 * 1000)).unwrap();
    w.apps.prune().await;
    assert_eq!(call(&w.api, "GET", &format!("/v1/apps/services?project={}&deleted=true", w.project), None).await.1, json!([]));
    assert!(!w.fake.networks().contains(&net) && w.fake.instances(&web).is_empty(), "{:?}", w.fake.networks());
}

/// Mail a fake mailbox took: (user, subject, text).
#[derive(Default)]
struct FakeMail(parking_lot::Mutex<Vec<(String, String, String)>>);

#[async_trait::async_trait]
impl apps_control::integrations::Mailbox for FakeMail {
    fn on(&self) -> bool {
        true
    }
    async fn send(&self, user: &str, subject: &str, text: &str) -> anyhow::Result<()> {
        self.0.lock().push((user.into(), subject.into(), text.into()));
        Ok(())
    }
}

/// A project's owner is emailed what needs them from its first service on (Render's and
/// Vercel's default), only ever at their account (never an address a caller names); a removed
/// email target stays removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_projects_owner_is_emailed_by_default() {
    let w = world().await;
    let mail = Arc::new(FakeMail::default());
    w.apps.set_mailbox(mail.clone());
    let project = w.apps.deps.workspace.as_ref().unwrap().create("usr_ada", "shop", "", None).unwrap().id;
    let new = |name: &str| json!({ "project": project, "name": name, "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80} });
    assert_eq!(call(&w.api, "POST", "/v1/apps/services", Some(new("web"))).await.0, StatusCode::CREATED);
    let base = format!("/v1/apps/projects/{project}/notifications");
    let (_, list) = call(&w.api, "GET", &base, None).await;
    let t = list.as_array().unwrap().iter().find(|t| t["kind"] == "email").cloned().unwrap_or_else(|| panic!("{list}"));
    assert!(t["events"].as_array().unwrap().contains(&json!("deploy.failed")), "{t}");
    let tid = t["id"].as_str().unwrap();
    assert_eq!(call(&w.api, "POST", &format!("{base}/{tid}/test"), None).await.1["ok"], true);
    let sent = mail.0.lock().clone();
    assert_eq!((sent.len(), sent[0].0.as_str()), (1, "usr_ada"), "{sent:?}");
    assert!(sent[0].1.starts_with("[roll] ") && sent[0].2.contains("you own the project"), "{sent:?}");
    // Never to an address someone names.
    let (st, _) = call(&w.api, "POST", &base, Some(json!({"kind": "email", "url": "someone@example.com"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Removed, it stays removed when the project gets another service.
    assert_eq!(call(&w.api, "DELETE", &format!("{base}/{tid}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&w.api, "POST", "/v1/apps/services", Some(new("api"))).await.0, StatusCode::CREATED);
    assert_eq!(call(&w.api, "GET", &base, None).await.1, json!([]));
    // An operator's project has no owner to email.
    let (st, _) = call(&w.api, "POST", &format!("/v1/apps/projects/{}/notifications", w.project), Some(json!({"kind": "email"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// Any service runs a command on a schedule (Heroku Scheduler, Vercel crons): a job of its live
/// deployment at each due time, counted from when the schedule was first seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_web_service_runs_a_command_on_its_schedule() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80, "schedules": [{"name": "cleanup", "schedule": "*/5 * * * *", "command": "echo cleanup"}]})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let jobs = format!("/v1/apps/services/{id}/environments/production/jobs");
    let now = code_sql::now_ms();
    w.apps.cron_pass(now).await;
    assert_eq!(call(&w.api, "GET", &jobs, None).await.1, json!([]), "nothing made up for the time before it was seen");
    w.apps.cron_pass(now + 6 * 60_000).await;
    let list = get_until(&w.api, &jobs, |j| j.as_array().is_some_and(|a| !a.is_empty())).await;
    assert_eq!(list[0]["command"], "echo cleanup", "{list}");
    // The environment says when it's next due and which job it last started (Heroku's "Next due").
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    let s = &env["schedules"][0];
    assert!(s["next_run_at"].as_i64() > Some(now) && s["last"]["job"] == list[0]["id"], "{env}");
    let (st, v) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"schedules": [{"name": "Bad Name", "schedule": "@daily", "command": "x"}]}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
}

/// A schedule can call a path instead (Vercel's crons): GET through the environment's own
/// proxy at each due time, its answer on the environment, and one that fails is `cron.failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_calls_a_path_and_a_failing_answer_is_said() {
    let w = world().await;
    let s = json!([{"name": "ping", "schedule": "*/5 * * * *", "path": "/api/cron"}, {"name": "gone", "schedule": "*/5 * * * *", "path": "/missing/x"}]);
    let (svc, _) = image_service(&w, "web", json!({"port": 80, "schedules": s})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (st, _) = call(&w.api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_MISSING": "/missing"}}))).await;
    assert!(st.is_success());
    let (_, d) = call(&w.api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    let now = code_sql::now_ms();
    w.apps.cron_pass(now).await;
    w.apps.cron_pass(now + 6 * 60_000).await;
    let env = get_until(&w.api, &base, |e| e["schedules"].as_array().is_some_and(|a| a.iter().all(|s| s["last_call"].is_object()))).await;
    let call_of = |n: &str| env["schedules"].as_array().unwrap().iter().find(|s| s["name"] == n).unwrap()["last_call"]["status"].clone();
    assert_eq!((call_of("ping"), call_of("gone")), (json!(200), json!(404)), "{env}");
    let events = call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1;
    let failed: Vec<&Value> = events["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "cron.failed").collect();
    assert_eq!(failed.len(), 1, "{events}");
    assert_eq!((failed[0]["data"]["path"].as_str(), failed[0]["data"]["http_status"].as_u64()), (Some("/missing/x"), Some(404)));
    let (st, v) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"run": {"schedules": [{"name": "both", "schedule": "@daily", "command": "x", "path": "/y"}]}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
}

/// How full a volume is, measured on the owner: the environment says it, and passing 90% is
/// `volume.full` once (again only after it went under 80%).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volume_filling_up_is_said_once() {
    let w = world().await;
    let (svc, _) = image_service(&w, "db", json!({"type": "worker", "volume": {"mount": "/data", "size_gb": 1}})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let volume = apps_control::volume_name(&id, "production");
    let events = || async { call(&w.api, "GET", "/v1/apps/events?after=0", None).await.1["events"].as_array().cloned().unwrap_or_default().into_iter().filter(|e| e["kind"] == "volume.full").count() };
    w.fake.set_volume_used(&volume, 1_000_000_000);
    // Measured once the environment's live state has its deployment (a moment after it settles).
    for _ in 0..200 {
        w.apps.measure_volumes().await;
        if events().await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.apps.measure_volumes().await;
    assert_eq!(events().await, 1, "once while it stays full");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    assert_eq!(env["volume_used"]["bytes"], 1_000_000_000u64, "{env}");
    w.fake.set_volume_used(&volume, 1_000);
    w.apps.measure_volumes().await;
    w.fake.set_volume_used(&volume, 1_050_000_000);
    w.apps.measure_volumes().await;
    assert_eq!(events().await, 2, "again after it had room");
}

/// Files to and from an instance (`roll apps cp`, Fly's sftp): a tar written under a path reads
/// back as it was, from the instance a shell would get; a path must be absolute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_go_to_and_from_an_instance() {
    let w = world().await;
    let (svc, _) = image_service(&w, "web", json!({"port": 80, "min_instances": 1})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let files = format!("/v1/apps/services/{id}/environments/production/files");
    let raw = |method: &str, uri: String, body: Vec<u8>| {
        let req = Request::builder().method(method).uri(uri).header("content-type", "application/x-tar").body(Body::from(body)).unwrap();
        w.api.clone().oneshot(req)
    };
    // Its floor's instance is up first (it starts a moment after the deploy, later on a busy machine).
    get_until(&w.api, &format!("/v1/apps/services/{id}/environments/production"), |e| e["state"] == "running").await;
    let tar = b"a tar's bytes".to_vec();
    let resp = raw("PUT", format!("{files}?path=/data"), tar.clone()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = raw("GET", format!("{files}?path=/data"), vec![]).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let inst = resp.headers()["x-roll-instance"].to_str().unwrap().to_string();
    assert!(w.fake.instances(&id).iter().any(|(n, ..)| *n == inst), "{inst}");
    assert_eq!(resp.into_body().collect().await.unwrap().to_bytes().to_vec(), tar);
    assert_eq!(raw("GET", format!("{files}?path=data"), vec![]).await.unwrap().status(), StatusCode::BAD_REQUEST);
    let (_, trail) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/activity"), None).await;
    assert!(trail.to_string().contains("audit.files.written") && trail.to_string().contains("audit.files.read"), "{trail}");
}

/// A "deploy to roll" link reads a repo's own roll.toml: what to set, what to make (a secret),
/// what to ask whoever deploys it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repos_roll_toml_is_read_for_a_deploy_link() {
    let w = world().await;
    w.code.files.lock().insert("roll.toml".into(), "[apps.web]\nrepo = \"acme/site\"\n[apps.web.variables]\nNODE_ENV = \"production\"\nSESSION_SECRET = { generate = \"secret\" }\nSTRIPE_KEY = { ask = \"Your Stripe secret key\" }\n".into());
    let (st, imp) = call(&w.api, "POST", "/v1/apps/import", Some(json!({"repo": "acme/site"}))).await;
    assert_eq!(st, StatusCode::OK, "{imp}");
    let web = &imp["apps"]["web"];
    assert_eq!((&web["variables"], &web["generate"], &web["ask"][0]["name"]), (&json!({"NODE_ENV": "production"}), &json!(["SESSION_SECRET"]), &json!("STRIPE_KEY")), "{imp}");
    w.code.files.lock().insert("roll.toml".into(), "[apps.web]\nrepo = \"acme/site\"\n[apps.web.variables]\nX = { generate = \"uuid\" }\n".into());
    assert_eq!(call(&w.api, "POST", "/v1/apps/import", Some(json!({"repo": "acme/site"}))).await.0, StatusCode::BAD_REQUEST, "only secrets are made");
}

/// A public Git host (read-only, in memory): what a `git` source fetches from.
#[derive(Default)]
struct GitHost {
    objects: Mutex<std::collections::HashMap<code_git::ObjectId, code_git::Object>>,
    main: Mutex<Option<code_git::ObjectId>>,
    /// Other branches, by name.
    branches: Mutex<std::collections::BTreeMap<String, code_git::ObjectId>>,
    /// A private repository's login: reads need it.
    login: Mutex<Option<(String, String)>>,
    /// Pull requests' heads (`refs/pull/N/head`), by number.
    pulls: Mutex<std::collections::BTreeMap<u64, code_git::ObjectId>>,
}

/// roll previews' side of a Git URL's pushes: what it was handed.
#[derive(Default)]
struct FakePushes(Mutex<Vec<apps_control::integrations::GitPush>>);

#[async_trait]
impl apps_control::integrations::GitPushes for FakePushes {
    async fn pushed(&self, push: apps_control::integrations::GitPush) {
        self.0.lock().push(push);
    }
    fn takes_pulls(&self) -> bool {
        true
    }
}

impl GitHost {
    /// A commit of these files (one directory) on main, after the last one.
    fn commit(&self, files: &[(&str, &str)]) -> String {
        self.commit_saying(files, "change")
    }

    /// A commit with this message.
    fn commit_saying(&self, files: &[(&str, &str)], message: &str) -> String {
        use code_git::{Kind, Object, mode};
        let mut objects = self.objects.lock();
        let mut tree = Vec::new();
        let mut sorted = files.to_vec();
        sorted.sort();
        for (name, text) in sorted {
            let blob = Object::new(Kind::Blob, text.as_bytes().to_vec());
            tree.extend_from_slice(format!("{:o} {name}\0", mode::REGULAR).as_bytes());
            tree.extend_from_slice(&blob.id.0);
            objects.insert(blob.id, blob);
        }
        let tree = Object::new(Kind::Tree, tree);
        let mut c = format!("tree {}\n", tree.id);
        if let Some(p) = *self.main.lock() {
            c.push_str(&format!("parent {p}\n"));
        }
        c.push_str(&format!("author a <a@example.com> 1700000000 +0000\ncommitter a <a@example.com> 1700000000 +0000\n\n{message}\n"));
        let commit = Object::new(Kind::Commit, c.into_bytes());
        let id = commit.id;
        objects.insert(tree.id, tree);
        objects.insert(id, commit);
        *self.main.lock() = Some(id);
        id.to_string()
    }
}

#[async_trait]
impl code_git::GitBackend for GitHost {
    async fn authorize(&self, _repo: &str, c: Option<&code_git::Credentials>, access: code_git::Access) -> code_git::GitResult<Option<String>> {
        match access {
            code_git::Access::Read => match &*self.login.lock() {
                Some((u, p)) if c.is_none_or(|c| (&c.username, &c.password) != (u, p)) => Err(code_git::GitError::Unauthorized),
                _ => Ok(None),
            },
            code_git::Access::Write => Err(code_git::GitError::Unauthorized),
        }
    }
    async fn refs(&self, _repo: &str) -> code_git::GitResult<code_git::RefList> {
        let mut refs: Vec<code_git::Ref> = self.main.lock().iter().map(|oid| code_git::Ref { name: "refs/heads/main".into(), oid: *oid, peeled: None }).collect();
        refs.extend(self.branches.lock().iter().map(|(b, oid)| code_git::Ref { name: format!("refs/heads/{b}"), oid: *oid, peeled: None }));
        refs.extend(self.pulls.lock().iter().map(|(n, oid)| code_git::Ref { name: format!("refs/pull/{n}/head"), oid: *oid, peeled: None }));
        Ok(code_git::RefList { head: Some("refs/heads/main".into()), refs })
    }
    async fn read_object(&self, _repo: &str, oid: &code_git::ObjectId) -> code_git::GitResult<Option<code_git::Object>> {
        Ok(self.objects.lock().get(oid).cloned())
    }
    async fn receive(&self, _repo: &str, _req: code_git::ReceiveRequest) -> code_git::GitResult<code_git::ReceiveOutcome> {
        Err(code_git::GitError::Unauthorized)
    }
}

/// A public Git URL: made on its default branch's head, fetched shallowly and built, and a new
/// commit there deploys once (`watch_git`); a branch it hasn't is refused when it's made.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_public_git_url_deploys_its_branch_and_follows_new_commits() {
    let w = world().await;
    let git = Arc::new(GitHost::default());
    let first = git.commit(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http')")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/git/acme/site", listener.local_addr().unwrap());
    let app = code_git::router(git.clone(), code_git::ServerConfig::default());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // What it would build as, before anything is made.
    let (st, plan) = call(&w.api, "POST", "/v1/apps/plan", Some(json!({ "git": url }))).await;
    assert_eq!((st, plan["provider"].as_str()), (StatusCode::OK, Some("node")), "{plan}");
    let body = |branch: Option<&str>| json!({ "project": w.project, "name": "site", "source": {"kind": "git", "url": url}, "branch": branch, "variables": {"FAKE_ECHO": "ROLL_GIT_COMMIT,ROLL_GIT_BRANCH"} });
    let (st, v) = call(&w.api, "POST", "/v1/apps/services", Some(body(Some("nope")))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(body(None))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let d1 = settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!((d1["status"].as_str(), d1["revision"].as_str(), d1["plan"]["provider"].as_str()), (Some("active"), Some(first.as_str()), Some("node")), "{d1}");
    let (_, out) = visit_deployment(&w, &host(&svc["environments"][0]["url"]), d1["id"].as_str().unwrap()).await;
    assert_eq!((&out["vars"]["ROLL_GIT_COMMIT"], &out["vars"]["ROLL_GIT_BRANCH"]), (&json!(first), &json!("main")), "{out}");
    assert!(w.apps.watch_git().await.is_empty(), "main is where it runs");
    let second = git.commit(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http') // v2")]);
    let started = w.apps.watch_git().await;
    assert_eq!(started.len(), 1, "one deploy for the new commit");
    let d2 = settled(&w, &id, "production", &started[0]).await;
    assert_eq!((d2["status"].as_str(), d2["revision"].as_str(), d2["trigger"].as_str()), (Some("active"), Some(second.as_str()), Some("push")), "{d2}");
    assert!(w.apps.watch_git().await.is_empty(), "deployed: nothing more to do");
    // Its hook is the repository's push webhook: a push to main deploys that commit at once,
    // once however often it's delivered; another branch and GitHub's ping deploy nothing.
    let (_, h) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/hook"), None).await;
    let hook = h["path"].as_str().unwrap().to_string();
    let third = git.commit_saying(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http') // v3")], "fix the header\n\nlonger text");
    let push = json!({ "ref": "refs/heads/main", "after": third, "head_commit": { "message": "fix the header\n\nlonger text" } });
    let (st, d3) = call(&w.api, "POST", &hook, Some(push.clone())).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d3}");
    assert_eq!((d3["revision"].as_str(), d3["message"].as_str(), d3["trigger"].as_str()), (Some(third.as_str()), Some("fix the header"), Some("push")), "{d3}");
    assert_eq!(call(&w.api, "POST", &hook, Some(push)).await.1["id"], d3["id"], "a redelivery is the same deploy");
    let (st, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/feature", "after": third }))).await;
    assert!(st == StatusCode::OK && v["ignored"].as_str().unwrap().contains("no environment follows it"), "{v}");
    assert_eq!(call(&w.api, "POST", &hook, Some(json!({ "zen": "Keep it logically awesome.", "hook_id": 1 }))).await.1["ignored"], "pong");
    assert_eq!(settled(&w, &id, "production", d3["id"].as_str().unwrap()).await["status"], "active");
    // What a repository sends that isn't a branch push deploys nothing: a tag, another event,
    // a body that isn't a push. GitHub's default form encoding is read as its JSON is.
    let raw = |ct: &'static str, event: Option<&'static str>, body: String| {
        let mut r = Request::builder().method("POST").uri(hook.clone()).header("content-type", ct);
        if let Some(e) = event {
            r = r.header("x-github-event", e);
        }
        r.body(Body::from(body)).unwrap()
    };
    let answer = |r: Request<Body>| {
        let api = w.api.clone();
        async move {
            let resp = api.oneshot(r).await.unwrap();
            let st = resp.status();
            (st, serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
        }
    };
    let (st, v) = answer(raw("application/json", Some("push"), json!({ "ref": "refs/tags/v1", "after": third }).to_string())).await;
    assert!(st == StatusCode::OK && v["ignored"].as_str().unwrap().contains("not a branch"), "{v}");
    let (st, v) = answer(raw("application/json", Some("issues"), json!({ "action": "opened" }).to_string())).await;
    assert!(st == StatusCode::OK && v["ignored"].as_str().unwrap().contains("isn't a push"), "{v}");
    let (st, v) = answer(raw("application/json", None, json!({ "star": true }).to_string())).await;
    assert!(st == StatusCode::OK && v["ignored"] == "not a push", "{v}");
    let fourth = git.commit(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http') // v4")]);
    let form = format!("payload={}", url::form_urlencoded::byte_serialize(json!({ "ref": "refs/heads/main", "after": fourth, "head_commit": { "message": "form encoded" } }).to_string().as_bytes()).collect::<String>());
    let (st, d4) = answer(raw("application/x-www-form-urlencoded", Some("push"), form)).await;
    assert_eq!((st, d4["revision"].as_str()), (StatusCode::ACCEPTED, Some(fourth.as_str())), "{d4}");
    settled(&w, &id, "production", d4["id"].as_str().unwrap()).await;
    // The watch reads a commit's message as the hook does: `[skip ci]` isn't deployed.
    git.commit_saying(&[("README.md", "docs")], "docs only [skip ci]");
    assert!(w.apps.watch_git().await.is_empty(), "its message says [skip ci]");
    // Watch paths: a commit that changes none of them (the live commit's tree against its) isn't
    // deployed, by the watch or the hook; one that does is.
    let (st, v) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"build": {"watch": ["server.js"]}}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let pkg = ("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#);
    let docs = git.commit(&[pkg, ("server.js", "require('http') // v4"), ("README.md", "more docs")]);
    assert!(w.apps.watch_git().await.is_empty(), "only README.md changed");
    let (st, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/main", "after": docs }))).await;
    assert!(st == StatusCode::OK && v["ignored"].as_str().unwrap().contains("watches"), "{v}");
    let code = git.commit(&[pkg, ("server.js", "require('http') // v5"), ("README.md", "more docs")]);
    let started = w.apps.watch_git().await;
    assert_eq!(started.len(), 1, "server.js changed");
    assert_eq!(settled(&w, &id, "production", &started[0]).await["revision"].as_str(), Some(code.as_str()));
    // With roll previews on the node, a push to a branch no environment follows is handed to it,
    // once the repository says the branch is there (or gone, for a deletion): a body naming
    // another commit hands nothing on.
    let pushes = Arc::new(FakePushes::default());
    w.apps.set_git_pushes(pushes.clone());
    git.branches.lock().insert("feature".into(), code.parse().unwrap());
    let (st, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/feature", "after": code, "head_commit": { "message": "try it" }, "sender": { "login": "renovate[bot]" } }))).await;
    assert!(st == StatusCode::OK && v["ignored"].as_str().unwrap().contains("roll previews"), "{v}");
    // Who pushed, for roll previews' `skip_authors` (untrusted: never for trust).
    assert_eq!(pushes.0.lock()[0].pusher.as_deref(), Some("renovate[bot]"));
    let (_, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/feature", "after": first }))).await;
    assert!(v["ignored"].as_str().unwrap().contains("the repository has it at"), "{v}");
    let (_, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/gone", "after": "0".repeat(40), "deleted": true }))).await;
    assert!(v["ignored"].as_str().unwrap().contains("its preview goes"), "{v}");
    let got: Vec<_> = pushes.0.lock().iter().map(|p| (p.service.clone(), p.url.clone(), p.branch.clone(), p.sha.clone(), p.message.clone())).collect();
    assert_eq!(got, vec![(id.clone(), url.clone(), "feature".into(), Some(code.clone()), "try it".into()), (id.clone(), url.clone(), "gone".into(), None, String::new())]);
    // main is production's: deployed by apps as before, never handed on.
    let (st, _) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/main", "after": code }))).await;
    assert!(st.is_success() && pushes.0.lock().len() == 2);
    // A body naming an older commit (a leaked URL, or a delivery a newer push overtook) deploys
    // nothing: only where the branch is now does.
    let (st, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/main", "after": first }))).await;
    assert!(st == StatusCode::OK && v["ignored"].as_str().unwrap().contains("only its head deploys"), "{v}");
    // roll.toml's watch paths at the pushed commit (over the service's), and a Bitbucket push
    // that moves two branches: main deploys here, feature goes to previews.
    call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"build": {"watch": null}}))).await;
    let toml = ("roll.toml", "[app]\nwatch = [\"api.js\"]\n");
    let readme = git.commit(&[pkg, ("server.js", "require('http') // v5"), ("README.md", "even more docs"), toml]);
    let (_, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/main", "after": readme }))).await;
    assert!(v["ignored"].as_str().unwrap().contains("api.js"), "roll.toml's watch: {v}");
    let api = git.commit_saying(&[pkg, ("server.js", "require('http') // v5"), ("README.md", "even more docs"), toml, ("api.js", "x")], "add the api");
    let bitbucket = json!({ "push": { "changes": [
        { "new": { "type": "branch", "name": "main", "target": { "hash": api, "message": "add the api" } } },
        { "new": { "type": "branch", "name": "feature", "target": { "hash": code, "message": "try it" } } },
    ] } });
    let (st, d) = call(&w.api, "POST", &hook, Some(bitbucket)).await;
    assert_eq!((st, d["revision"].as_str(), d["message"].as_str()), (StatusCode::ACCEPTED, Some(api.as_str()), Some("add the api")), "{d}");
    assert_eq!(pushes.0.lock().len(), 3, "feature went to previews too");
    // One webhook for every environment: a push to the branch staging follows, posted to
    // production's hook, deploys staging.
    let (st, v) = call(&w.api, "PUT", &format!("/v1/apps/services/{id}/environments/staging"), Some(json!({"branch": "feature"}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, d) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/feature", "after": code, "head_commit": { "message": "try it on staging" } }))).await;
    assert_eq!((st, d["environment"].as_str(), d["revision"].as_str()), (StatusCode::ACCEPTED, Some("staging"), Some(code.as_str())), "{d}");
    assert_eq!(pushes.0.lock().len(), 3, "a followed branch isn't a preview's");
    // The commit's own message decides `[skip ci]`, not the body's.
    let skip = git.commit_saying(&[pkg, ("server.js", "require('http') // v6"), ("README.md", "even more docs"), toml, ("api.js", "y")], "wip [skip ci]");
    let (_, v) = call(&w.api, "POST", &hook, Some(json!({ "ref": "refs/heads/main", "after": skip, "head_commit": { "message": "ship it" } }))).await;
    assert!(v["ignored"].as_str().unwrap().contains("[skip ci]"), "{v}");
    // Pull requests go to previews with their number, title and page: a same-repository one on
    // its branch, a fork's as `pull/N` once `refs/pull/N/head` agrees, a close with no commit.
    let pr = |action: &str, n: u64, branch: &str, sha: &str, head_repo: u64| {
        json!({ "action": action, "number": n, "pull_request": { "number": n, "title": "Try it", "user": { "login": "dependabot[bot]" }, "head": { "ref": branch, "sha": sha, "repo": { "id": head_repo } }, "base": { "repo": { "id": 1 } } } }).to_string()
    };
    // One from a branch an environment follows (staging's) is that environment's, not a preview.
    let (_, v) = answer(raw("application/json", Some("pull_request"), pr("opened", 6, "feature", &code, 1))).await;
    assert!(v["ignored"].as_str().unwrap().contains("an environment's branch"), "{v}");
    git.branches.lock().insert("topic".into(), code.parse().unwrap());
    let (st, v) = answer(raw("application/json", Some("pull_request"), pr("opened", 7, "topic", &code, 1))).await;
    assert!(st == StatusCode::OK && v["ignored"].as_str().unwrap().contains("handed to roll previews"), "{v}");
    git.pulls.lock().insert(8, first.parse().unwrap());
    let (_, v) = answer(raw("application/json", Some("pull_request"), pr("synchronize", 8, "main", &code, 2))).await;
    assert!(v["ignored"].as_str().unwrap().contains("refs/pull/8/head at"), "a fork's head that isn't refs/pull/8/head: {v}");
    answer(raw("application/json", Some("pull_request"), pr("synchronize", 8, "main", &first, 2))).await;
    answer(raw("application/json", Some("pull_request"), pr("closed", 8, "main", &first, 2))).await;
    let got: Vec<_> = pushes.0.lock()[3..].iter().map(|p| (p.branch.clone(), p.sha.clone(), p.pull.as_ref().map(|q| (q.number, q.fork, q.url.clone(), q.title.clone())))).collect();
    // Who opened it, for roll previews' `skip_authors` (untrusted: never for trust).
    assert_eq!(pushes.0.lock()[3].pull.as_ref().and_then(|q| q.author.clone()).as_deref(), Some("dependabot[bot]"));
    let page = |n: u64| format!("{url}/pull/{n}");
    assert_eq!(got, vec![
        ("topic".to_string(), Some(code.clone()), Some((7, false, page(7), "Try it".to_string()))),
        ("pull/8".to_string(), Some(first.clone()), Some((8, true, page(8), "Try it".to_string()))),
        ("pull/8".to_string(), None, Some((8, true, page(8), "Try it".to_string()))),
    ]);
    // GitLab's merge request: its own event and fields, its page under `/-/merge_requests/`.
    let mr = |user: (u64, &str), action: &str| json!({ "object_kind": "merge_request", "user": { "id": user.0, "username": user.1 }, "object_attributes": { "iid": 3, "title": "Try it on GitLab", "action": action, "author_id": 9, "source_branch": "topic", "source_project_id": 5, "target_project_id": 5, "last_commit": { "id": code } } }).to_string();
    let gitlab = |body: String| Request::builder().method("POST").uri(hook.clone()).header("content-type", "application/json").header("x-gitlab-event", "Merge Request Hook").body(Body::from(body)).unwrap();
    let (_, v) = answer(gitlab(mr((9, "renovate-bot"), "open"))).await;
    assert!(v["ignored"].as_str().unwrap().contains("handed to roll previews"), "{v}");
    let last = pushes.0.lock().last().cloned().unwrap();
    assert_eq!((last.branch.as_str(), last.pull.map(|p| (p.number, p.fork, p.url, p.author))), ("topic", Some((3, false, format!("{url}/-/merge_requests/3"), Some("renovate-bot".to_string())))));
    // A person's push to it: GitLab names the actor, not the opener, so no author.
    answer(gitlab(mr((10, "ana"), "update"))).await;
    assert_eq!(pushes.0.lock().last().cloned().unwrap().pull.map(|p| p.author), Some(None));
}

/// A private repository on any Git host: the service's own login (sealed, never returned, sent
/// only to the URL's host) is what its branch check, deploys and the watch fetch with.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_git_url_fetches_with_its_login() {
    let w = world().await;
    let git = Arc::new(GitHost::default());
    *git.login.lock() = Some(("deploy".into(), "glpat-sekret-token".into()));
    let pkg = ("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#);
    let first = git.commit(&[pkg, ("server.js", "require('http')")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/git/acme/private");
    let app = code_git::router(git.clone(), code_git::ServerConfig::default());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let body = |login: Value| json!({ "project": w.project, "name": "private", "source": {"kind": "git", "url": url}, "git_login": login });
    let (st, v) = call(&w.api, "POST", "/v1/apps/services", Some(body(Value::Null))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no login: {v}");
    let (st, v) = call(&w.api, "POST", "/v1/apps/services", Some(body(json!({"username": "deploy", "password": "wrong"})))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "a wrong one: {v}");
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(body(json!({"username": "deploy", "password": "glpat-sekret-token"})))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    assert_eq!(svc["git_login"], json!({"host": format!("127.0.0.1:{port}"), "username": "deploy"}));
    assert!(!svc.to_string().contains("sekret"));
    let id = svc["id"].as_str().unwrap().to_string();
    let d1 = settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!((d1["status"].as_str(), d1["revision"].as_str()), (Some("active"), Some(first.as_str())), "{d1}");
    let raw = [std::fs::read(w.apps.cfg.home.join("apps.db")).unwrap(), std::fs::read(w.apps.cfg.home.join("apps.db-wal")).unwrap_or_default()].concat();
    assert!(!raw.windows(6).any(|x| x == b"sekret"), "sealed at rest");
    // The watch fetches with it too.
    let second = git.commit(&[pkg, ("server.js", "require('http') // v2")]);
    let started = w.apps.watch_git().await;
    assert_eq!(started.len(), 1);
    assert_eq!(settled(&w, &id, "production", &started[0]).await["revision"].as_str(), Some(second.as_str()));
    // Another repository never gets it, on the same host either: moving the source drops it.
    let (_, s) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"source": {"kind": "git", "url": format!("http://127.0.0.1:{port}/git/acme/other")}}))).await;
    assert_eq!(s["git_login"], Value::Null, "{s}");
    call(&w.api, "PUT", &format!("/v1/apps/services/{id}/git-login"), Some(json!({"username": "deploy", "password": "glpat-sekret-token"}))).await;
    // Nor another host.
    let (_, s) = call(&w.api, "PATCH", &format!("/v1/apps/services/{id}"), Some(json!({"source": {"kind": "git", "url": format!("http://localhost:{port}/git/acme/private")}}))).await;
    assert_eq!(s["git_login"], Value::Null, "{s}");
    let (st, s) = call(&w.api, "PUT", &format!("/v1/apps/services/{id}/git-login"), Some(json!({"username": "deploy", "password": "glpat-sekret-token"}))).await;
    assert_eq!((st, s["git_login"]["host"].as_str()), (StatusCode::OK, Some(format!("localhost:{port}").as_str())), "{s}");
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}/git-login"), None).await.1["git_login"], Value::Null);
}

/// roll's GitHub App, faked: it sees `acme/site` (installation 42), hands out one token, and
/// records the commit statuses posted through it.
struct FakeGitHub {
    host: String,
    statuses: Mutex<Vec<(u64, String, String, String)>>,
    /// Deployments made (id, environment, production), then each state posted (id, state, url).
    deployments: Mutex<Vec<(u64, String, bool)>>,
    deployment_states: Mutex<Vec<(u64, String, Option<String>)>>,
    /// What every commit's checks say: `none`, `pending`, `passed` or a failed check's name.
    ci: Mutex<String>,
}

#[async_trait]
impl apps_control::integrations::GitHubApp for FakeGitHub {
    fn host(&self) -> Option<String> {
        Some(self.host.clone())
    }
    async fn installation_for(&self, owner: &str, repo: &str) -> anyhow::Result<Option<u64>> {
        Ok((owner, repo) == ("acme", "site")).map(|seen| seen.then_some(42))
    }
    async fn git_login(&self, installation: u64, _owner: &str, _repo: &str) -> anyhow::Result<(String, String)> {
        anyhow::ensure!(installation == 42, "not the installation");
        Ok(("x-access-token".into(), "ghs_installation".into()))
    }
    async fn status(&self, installation: u64, owner: &str, repo: &str, _sha: &str, s: &apps_control::integrations::CommitStatus) -> anyhow::Result<()> {
        self.statuses.lock().push((installation, format!("{owner}/{repo}"), s.context.clone(), s.state.to_string()));
        Ok(())
    }
    async fn deployment(&self, _installation: u64, _owner: &str, _repo: &str, _sha: &str, environment: &str, production: bool, _transient: bool) -> anyhow::Result<u64> {
        let mut d = self.deployments.lock();
        let id = d.len() as u64 + 1;
        d.push((id, environment.to_string(), production));
        Ok(d.len() as u64)
    }
    async fn deployment_status(&self, _installation: u64, _owner: &str, _repo: &str, deployment: u64, state: &str, url: Option<&str>, _description: &str) -> anyhow::Result<()> {
        self.deployment_states.lock().push((deployment, state.to_string(), url.map(str::to_string)));
        Ok(())
    }
    async fn ci(&self, installation: u64, _owner: &str, _repo: &str, _sha: &str) -> anyhow::Result<apps_control::integrations::Ci> {
        use apps_control::integrations::Ci;
        anyhow::ensure!(installation == 42, "not the installation");
        Ok(match self.ci.lock().as_str() {
            "none" => Ci::None,
            "pending" => Ci::Pending,
            "passed" => Ci::Passed,
            failed => Ci::Failed(failed.to_string()),
        })
    }
}

/// A private repository on roll's GitHub App's host: made without its first deploy, connected to
/// the App (only when it sees the repository), then fetched with the installation's token, its
/// deploys posting commit statuses through it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_git_url_service_connects_to_rolls_github_app() {
    let w = world().await;
    let git = Arc::new(GitHost::default());
    *git.login.lock() = Some(("x-access-token".into(), "ghs_installation".into()));
    let first = git.commit(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http')")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    // GitHub's paths are `/<owner>/<repo>`; the fake host's are under `/git/`.
    let inner = code_git::router(git.clone(), code_git::ServerConfig::default());
    let app = axum::Router::new().fallback(move |mut req: Request<Body>| {
        let inner = inner.clone();
        async move {
            let to = format!("/git{}", req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"));
            *req.uri_mut() = to.parse().unwrap();
            inner.oneshot(req).await.unwrap()
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let gh = Arc::new(FakeGitHub { host: host.clone(), statuses: Mutex::new(Vec::new()), deployments: Mutex::new(Vec::new()), deployment_states: Mutex::new(Vec::new()), ci: Mutex::new("none".into()) });
    w.apps.set_github_app(gh.clone());
    let make = |name: &str, repo: &str| json!({ "project": w.project, "name": name, "source": {"kind": "git", "url": format!("http://{host}/acme/{repo}")} });
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(make("site", "site"))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    assert_eq!(svc["environments"][0]["latest"], Value::Null, "nothing deployed before it's connected: {svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    // One the App doesn't see isn't connected.
    let (_, other) = call(&w.api, "POST", "/v1/apps/services", Some(make("other", "other"))).await;
    let (st, v) = call(&w.api, "PUT", &format!("/v1/apps/services/{}/github", other["id"].as_str().unwrap()), None).await;
    assert!(st == StatusCode::CONFLICT && v["error"].as_str().unwrap().contains("isn't installed on acme/other"), "{v}");
    let (st, s) = call(&w.api, "PUT", &format!("/v1/apps/services/{id}/github"), None).await;
    assert_eq!((st, &s["github"]), (StatusCode::OK, &json!({"installation": 42, "repo": "acme/site"})), "{s}");
    let (st, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), Some(json!({}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let d = settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    assert_eq!((d["status"].as_str(), d["revision"].as_str()), (Some("active"), Some(first.as_str())), "{d}");
    // Reported just after it goes live: waited for.
    let success = |p: &[(u64, String, String, String)]| p.iter().any(|(i, r, c, s)| (*i, r.as_str(), c.as_str(), s.as_str()) == (42, "acme/site", "roll/apps/site/production", "success"));
    for _ in 0..250 {
        if success(&gh.statuses.lock()) && gh.deployment_states.lock().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let posted = gh.statuses.lock().clone();
    assert!(success(&posted), "{posted:?}");
    // And a GitHub Deployment of production: in progress, then live at its URL.
    assert_eq!(*gh.deployments.lock(), vec![(1, "production".to_string(), true)]);
    let states: Vec<_> = gh.deployment_states.lock().iter().map(|(id, s, u)| (*id, s.clone(), u.is_some())).collect();
    assert_eq!(states, vec![(1, "in_progress".to_string(), false), (1, "success".to_string(), true)]);
    // An environment someone else reports (roll previews' `pv-` ones, `report: false`): its
    // deploys post no status and no Deployment; reporting again, they do.
    let prod = format!("/v1/apps/services/{id}/environments/production");
    let (st, e) = call(&w.api, "PUT", &prod, Some(json!({"report": false}))).await;
    assert_eq!((st, &e["report"]), (StatusCode::OK, &json!(false)), "{e}");
    let before = (gh.statuses.lock().len(), gh.deployments.lock().len());
    let (_, q) = call(&w.api, "POST", &format!("{prod}/deploys"), Some(json!({}))).await;
    settled(&w, &id, "production", q["id"].as_str().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!((gh.statuses.lock().len(), gh.deployments.lock().len()), before, "nothing reported");
    assert_eq!(call(&w.api, "PUT", &prod, Some(json!({"report": true}))).await.0, StatusCode::OK);
    // The App's own (verified) push reaches it with no hook set: run on its owner, it deploys the
    // pushed head; another installation's, or another repository's, doesn't.
    let second = git.commit_saying(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http') // v2")], "via the App");
    let push = |installation: u64, repo: &str| json!({ "ref": "refs/heads/main", "after": second, "head_commit": { "message": "via the App" }, "repository": { "full_name": repo }, "installation": { "id": installation } }).to_string();
    w.apps.github_event("push", push(7, "acme/site").as_bytes());
    w.apps.github_event("push", push(42, "acme/elsewhere").as_bytes());
    w.apps.github_event("push", push(42, "ACME/site").as_bytes());
    let mut via = Value::Null;
    for _ in 0..200 {
        let (_, list) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys"), None).await;
        if let Some(d) = list.as_array().into_iter().flatten().find(|d| d["revision"] == second.as_str()) {
            via = d.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!((via["message"].as_str(), via["trigger"].as_str(), via["actor"].as_str()), (Some("via the App"), Some("push"), Some("github app")), "{via}");
    settled(&w, &id, "production", via["id"].as_str().unwrap()).await;
    let (_, list) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys"), None).await;
    assert_eq!(list.as_array().unwrap().iter().filter(|d| d["revision"] == second.as_str()).count(), 1, "one deploy: {list}");
    // Wait for CI, on the App (Railway's): a push waits while GitHub's checks run, deploys once
    // they pass; one whose check fails isn't deployed.
    let env = format!("/v1/apps/services/{id}/environments/production");
    let (st, e) = call(&w.api, "PUT", &env, Some(json!({"wait_for_ci": true}))).await;
    assert_eq!((st, &e["wait_for_ci"]), (StatusCode::OK, &json!(true)), "{e}");
    *gh.ci.lock() = "pending".into();
    let third = git.commit_saying(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http') // v3")], "tested");
    let push3 = json!({ "ref": "refs/heads/main", "after": third, "head_commit": { "message": "tested" }, "repository": { "full_name": "acme/site" }, "installation": { "id": 42 } }).to_string();
    w.apps.github_event("push", push3.as_bytes());
    let deployed = |rev: String| {
        let (api, env) = (w.api.clone(), env.clone());
        async move { call(&api, "GET", &format!("{env}/deploys"), None).await.1.as_array().unwrap().iter().find(|d| d["revision"] == rev.as_str()).cloned() }
    };
    let mut waiting = false;
    for _ in 0..200 {
        if call(&w.api, "GET", &env, None).await.1["waiting_for_ci"] == third.as_str() {
            waiting = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(waiting, "the push waits for its checks");
    w.apps.settle_ci_waits().await;
    assert!(deployed(third.clone()).await.is_none(), "still running");
    *gh.ci.lock() = "passed".into();
    w.apps.settle_ci_waits().await;
    let d = deployed(third.clone()).await.expect("deployed once they passed");
    settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    *gh.ci.lock() = "test (node 22)".into();
    let fourth = git.commit_saying(&[("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#), ("server.js", "require('http') // v4")], "broken");
    w.apps.github_event("push", push3.replace(&third, &fourth).replace("tested", "broken").as_bytes());
    for _ in 0..200 {
        if call(&w.api, "GET", &env, None).await.1["waiting_for_ci"] == fourth.as_str() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.apps.settle_ci_waits().await;
    assert!(deployed(fourth.clone()).await.is_none() && call(&w.api, "GET", &env, None).await.1["waiting_for_ci"].is_null(), "its check failed: dropped");
    call(&w.api, "PUT", &env, Some(json!({"wait_for_ci": false}))).await;
    // Disconnected: no token, so the private repository can't be read.
    call(&w.api, "DELETE", &format!("/v1/apps/services/{id}/github"), None).await;
    let (st, v) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), Some(json!({}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
}

/// A Git login whose password is one of the project's secrets: read at each fetch, so a rotated
/// token is used at once and nothing is kept here; a secret that isn't there fetches without one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_git_login_reads_its_token_from_roll_secrets() {
    struct Tokens;
    static TOKEN: Mutex<String> = Mutex::new(String::new());
    #[async_trait]
    impl apps_control::integrations::SecretSource for Tokens {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn resolve(&self, _: &str, env: &str, _: &str) -> anyhow::Result<Vec<apps_control::integrations::SecretVar>> {
            assert_eq!(env, "production");
            Ok(vec![apps_control::integrations::SecretVar { name: "GIT_TOKEN".into(), value: TOKEN.lock().clone(), linked: false, sensitive: true, plain: false }])
        }
    }
    *TOKEN.lock() = "tok-1".into();
    let w = world_full(|_| {}, false, |d| d.secrets = Arc::new(Tokens)).await;
    let git = Arc::new(GitHost::default());
    *git.login.lock() = Some(("deploy".into(), "tok-1".into()));
    let pkg = ("package.json", r#"{"name":"site","scripts":{"start":"node server.js"}}"#);
    git.commit(&[pkg, ("server.js", "require('http')")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/git/acme/private", listener.local_addr().unwrap());
    let app = code_git::router(git.clone(), code_git::ServerConfig::default());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "private", "source": {"kind": "git", "url": url}, "git_login": {"username": "deploy", "password": "tok-1"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let login = format!("/v1/apps/services/{id}/git-login");
    assert_eq!(call(&w.api, "PUT", &login, Some(json!({"username": "deploy", "password": "x", "secret": "GIT_TOKEN"}))).await.0, StatusCode::BAD_REQUEST, "not both");
    let (st, s) = call(&w.api, "PUT", &login, Some(json!({"username": "deploy", "secret": "GIT_TOKEN"}))).await;
    assert_eq!((st, &s["git_login"]["secret"]), (StatusCode::OK, &json!("GIT_TOKEN")), "{s}");
    // Rotated on the host and in roll secrets: the next deploy reads the new one.
    *git.login.lock() = Some(("deploy".into(), "tok-2".into()));
    *TOKEN.lock() = "tok-2".into();
    let deploys = format!("/v1/apps/services/{id}/environments/production/deploys");
    let (st, d) = call(&w.api, "POST", &deploys, Some(json!({}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    assert_eq!(settled(&w, &id, "production", d["id"].as_str().unwrap()).await["status"], "active");
    // A secret that isn't there: fetched without a login, refused by the host.
    call(&w.api, "PUT", &login, Some(json!({"username": "deploy", "secret": "NOT_THERE"}))).await;
    assert_eq!(call(&w.api, "POST", &deploys, Some(json!({}))).await.0, StatusCode::BAD_REQUEST);
}

/// The dashboard's starter templates (apps/dashboard/src/lib/templates) read as the import reads
/// them, on public images only, with run settings a service takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_starter_template_imports() {
    let w = world().await;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/dashboard/src/lib/templates");
    let mut n = 0;
    for f in std::fs::read_dir(&dir).unwrap() {
        let path = f.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        let (st, imp) = call(&w.api, "POST", "/v1/apps/import", Some(json!({ "roll_toml": text }))).await;
        assert_eq!(st, StatusCode::OK, "{}: {imp}", path.display());
        for (name, app) in imp["apps"].as_object().unwrap() {
            assert_eq!(app["source"]["kind"], "image", "{name}: a template runs a public image");
            // Merged as creation merges it: every setting checked.
            apps_control::model::Run::default().merged(Some(&app["run"])).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        n += 1;
    }
    assert!(n >= 5, "{n} templates in {}", dir.display());
}

/// An environment's generated name can be changed (Railway's): the new one serves at once and
/// the old one is a 404; a name another environment has, or roll's own, is refused; with custom
/// domains whose CNAMEs name it, it's refused until they're gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environments_generated_name_can_be_changed() {
    let w = world().await;
    let (svc, d) = image_service(&w, "api", json!({"port": 80})).await;
    let (other, _) = image_service(&w, "web", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let old = host(&svc["environments"][0]["url"]);
    visit_deployment(&w, &old, d["id"].as_str().unwrap()).await;
    let (st, env) = call(&w.api, "PUT", &base, Some(json!({"hostname": "shop-eu"}))).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert!(env["url"].as_str().unwrap().starts_with("http://shop-eu.apps.localhost"), "{env}");
    assert_eq!(visit(&w, "shop-eu.apps.localhost", "/").await.0, StatusCode::OK);
    assert_eq!(visit(&w, &old, "/").await.0, StatusCode::NOT_FOUND, "the old name is let go");
    let taken = host(&other["environments"][0]["url"]).split('.').next().unwrap().to_string();
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"hostname": taken}))).await.0, StatusCode::CONFLICT);
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"hostname": "www"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&w.api, "PUT", &base, Some(json!({"hostname": "a--b"}))).await.0, StatusCode::BAD_REQUEST);
    call(&w.api, "POST", &format!("{base}/domains"), Some(json!({"hostname": "shop.example.com"}))).await;
    let (st, v) = call(&w.api, "PUT", &base, Some(json!({"hostname": "shop-us"}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "a custom domain's CNAME names it: {v}");
}

/// At max_instances a request goes to a busy instance rather than wait: each such request is
/// counted (Metrics' over capacity), and five minutes with enough of them are `alert.capacity`
/// once, naming what to raise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturation_is_counted_and_alerted() {
    let w = world().await;
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "full", "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80, "concurrency": 1, "max_instances": 1}, "variables": {"FAKE_SLOW_MS": "300"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let d = settled(&w, &id, "production", svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    let host = host(&svc["environments"][0]["url"]);
    // Ten at once on one instance taking one. On a loaded host the instance can be asleep again
    // by the time they come (then they wait for its wake, not for a busy instance): sent again,
    // awake first, until they're counted.
    let mut m = Value::Null;
    for _ in 0..5 {
        visit_deployment(&w, &host, d["id"].as_str().unwrap()).await;
        let all = futures::future::join_all((0..10).map(|_| visit(&w, &host, "/"))).await;
        assert!(all.iter().all(|(st, _)| *st == StatusCode::OK), "{all:?}");
        m = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/metrics?minutes=5"), None).await.1;
        if m["over_capacity"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).sum::<f64>() >= 5.0 {
            break;
        }
    }
    let over: f64 = m["over_capacity"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).sum();
    assert!(over >= 5.0, "ten at once on one instance taking one: {m}");
    // Five minutes with 30% of requests over capacity: one alert, saying what to raise.
    let into = code_sql::now_ms() % 60_000;
    if into > 50_000 {
        tokio::time::sleep(Duration::from_millis((60_000 - into + 100) as u64)).await;
    }
    let minute = |m: i64| apps_control::store::MinuteRow { minute: m, requests: 100, over_capacity: 30, instances: 1, memory_bytes: 64 << 20, ..Default::default() };
    let live = w.apps.live(&id, "production");
    loop {
        let now = code_sql::now_ms() / 60_000;
        w.apps.store.put_minutes(&id, "production", &(1..=5).map(|i| minute(now - i)).collect::<Vec<_>>()).unwrap();
        w.apps.judge_alerts(&live).await;
        w.apps.judge_alerts(&live).await;
        if code_sql::now_ms() / 60_000 == now {
            break;
        }
    }
    let (_, ev) = call(&w.api, "GET", "/v1/apps/events?after=0", None).await;
    let alerts: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "alert.capacity").collect();
    assert_eq!(alerts.len(), 1, "{ev}");
    assert_eq!((alerts[0]["data"]["value"].as_i64(), alerts[0]["data"]["threshold"].as_i64(), alerts[0]["data"]["max_instances"].as_i64()), (Some(30), Some(10), Some(1)), "{ev}");
}

/// The methods each route of `router` serves, from its `Debug` (as rolld's `route_methods`
/// reads them: axum has no API for its routes).
fn router_routes(router: &axum::Router) -> std::collections::BTreeSet<(String, String)> {
    let text = format!("{router:?}");
    let routes = text.split("fallback_router:").next().unwrap_or_default();
    let (routes, paths) = routes.split_once("node: Node { paths:").unwrap_or((routes, ""));
    let mut allow: Vec<(String, String)> = Vec::new();
    let mut rest = routes;
    while let Some(i) = rest.find("RouteId(") {
        rest = &rest[i + "RouteId(".len()..];
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let entry = rest.find("RouteId(").map_or(rest, |j| &rest[..j]);
        let methods = entry.split_once("allow_header: Bytes(b\"").and_then(|(_, a)| a.split_once('"')).map(|(a, _)| a.to_string()).unwrap_or_default();
        allow.push((id, methods));
    }
    let mut out = std::collections::BTreeSet::new();
    let mut rest = paths;
    while let Some(i) = rest.find("RouteId(") {
        rest = &rest[i + "RouteId(".len()..];
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let Some(after) = rest.trim_start_matches(|c: char| c.is_ascii_digit()).strip_prefix("): \"") else { continue };
        let Some(end) = after.find('"') else { break };
        let path = &after[..end];
        if !path.starts_with('/') {
            continue;
        }
        for m in allow.iter().find(|(a, _)| *a == id).map(|(_, m)| m.as_str()).unwrap_or("").split(',').filter(|m| !m.is_empty() && *m != "HEAD") {
            out.insert((m.to_string(), path.to_string()));
        }
    }
    out
}

/// An operation's summary from its method and path: `List deploys`, `Get deploy`, `Restart`.
fn summary(method: &str, path: &str) -> String {
    let segs: Vec<&str> = path.split('/').skip(3).collect();
    let words = |s: &str| s.replace(['-', '_'], " ");
    let one = |s: &str| words(s.strip_suffix("ies").map(|b| format!("{b}y")).as_deref().unwrap_or(s.strip_suffix('s').unwrap_or(s)));
    let literal = |s: &&&str| !s.starts_with('{');
    let item = segs.last().is_some_and(|l| l.starts_with('{'));
    let last = segs.iter().rev().find(literal).copied().unwrap_or("apps");
    // What an action acts on: the resource before it (`drains/{did}/test`: a drain).
    let on = segs.iter().rev().skip(1).skip_while(|s| !s.starts_with('{')).find(literal).map(|s| one(s));
    let plural = last.ends_with('s');
    let verb = |v: &str, n: String| format!("{v} {n}");
    match (method, item, plural) {
        _ if segs.is_empty() => "Get the overview".into(),
        // A deploy hook's own URL.
        ("POST", true, _) => verb("Call", one(last)),
        ("GET", true, _) => verb("Get", one(last)),
        ("PATCH", true, _) => verb("Update", one(last)),
        ("DELETE", true, _) => verb("Delete", one(last)),
        ("PUT", true, _) => verb("Set", one(last)),
        ("GET", false, true) => verb("List", words(last)),
        ("GET", false, false) => verb("Get", words(last)),
        ("POST", false, true) => verb("Create", one(last)),
        ("PUT", false, _) => verb("Set", words(last)),
        ("PATCH", false, _) => verb("Update", words(last)),
        ("DELETE", false, _) => verb("Delete", words(last)),
        // An action: its own verb (`restart`, `rollback`, `verify`), and what it acts on.
        _ => {
            let w = words(last);
            let mut c = w.chars();
            let v = c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default();
            on.map_or(v.clone(), |o| format!("{v} {o}"))
        }
    }
}

/// docs/apps/openapi.json is the contract's routes (docs/apps/http-api.md, "Routes and actions")
/// as OpenAPI 3.1, and the contract lists exactly the routes the router serves: a route added
/// to one and not the others fails here. `ROLL_OPENAPI=write` writes the spec again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_openapi_spec_is_the_contract_and_the_router() {
    let w = world().await;
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/apps");
    let contract = std::fs::read_to_string(docs.join("http-api.md")).unwrap();
    // `| \`METHOD /path?query\` | body | answer | action |`
    let mut rows: Vec<(String, String, String, String, String)> = Vec::new();
    for line in contract.lines().filter(|l| l.starts_with("| `")) {
        // Cells split on `|`, never on an escaped `\|` inside one.
        let mut cells: Vec<String> = vec![String::new()];
        let mut chars = line.trim().trim_start_matches('|').trim_end_matches('|').chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&'|') => cells.last_mut().unwrap().push(chars.next().unwrap()),
                '|' => cells.push(String::new()),
                c => cells.last_mut().unwrap().push(c),
            }
        }
        let cells: Vec<&str> = cells.iter().map(|c| c.trim()).collect();
        let Some((method, path)) = cells[0].trim_matches('`').split_once(' ') else { continue };
        if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method) || !path.starts_with("/v1/apps") {
            continue;
        }
        let path = path.split('?').next().unwrap().to_string();
        let cell = |i: usize| cells.get(i).map_or(String::new(), |c| c.to_string());
        rows.push((method.to_string(), path, cell(1), cell(2), cell(3)));
    }
    let documented: std::collections::BTreeSet<(String, String)> = rows.iter().map(|(m, p, ..)| (m.clone(), p.clone())).collect();
    let served: std::collections::BTreeSet<(String, String)> = router_routes(&w.api).into_iter().filter(|(_, p)| p.starts_with("/v1/apps")).collect();
    let undocumented: Vec<_> = served.difference(&documented).collect();
    let unserved: Vec<_> = documented.difference(&served).collect();
    assert!(undocumented.is_empty() && unserved.is_empty(), "served but not in http-api.md: {undocumented:?}\nin http-api.md but not served: {unserved:?}");
    // The spec: one operation per row, its parameters from the path, the contract's words as
    // its description, the permission it needs as `x-roll-action`.
    let mut paths = serde_json::Map::new();
    for (method, path, body, answer, action) in &rows {
        let params: Vec<Value> = path.split('/').filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}'))).map(|p| json!({ "name": p, "in": "path", "required": true, "schema": { "type": "string" } })).collect();
        let tag = path.trim_start_matches("/v1/apps").trim_start_matches('/').split('/').next().filter(|s| !s.is_empty()).unwrap_or("apps");
        let status = if answer.starts_with("201") { "201" } else if answer.starts_with("202") { "202" } else if answer.starts_with("204") { "204" } else { "200" };
        let id: String = format!("{}{}", method.to_lowercase(), path.split('/').skip(3).map(|s| s.trim_matches(|c| c == '{' || c == '}')).map(|s| s.split(['-', '_']).map(|w| { let mut c = w.chars(); c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default() }).collect::<String>()).collect::<String>());
        let mut op = json!({ "operationId": id, "tags": [tag], "summary": summary(method, path), "description": format!("Answers {answer}."), "x-roll-action": action, "responses": { status: { "description": answer }, "4XX": { "$ref": "#/components/responses/Error" } } });
        if !params.is_empty() {
            op["parameters"] = json!(params);
        }
        if !body.is_empty() {
            op["requestBody"] = json!({ "description": body, "content": { "application/json": { "schema": { "type": "object" } } } });
        }
        paths.entry(path.clone()).or_insert_with(|| json!({}))[method.to_lowercase()] = op;
    }
    let spec = json!({
        "openapi": "3.1.0",
        "info": { "title": "roll apps", "version": "1", "description": "Host your own code next to your roll databases. The shapes named here (Service, Environment, Deployment…) are defined in docs/apps/http-api.md, which this file is generated from." },
        "servers": [{ "url": "http://127.0.0.1:7070", "description": "a local rolld" }],
        "security": [{ "bearer": [] }],
        "components": {
            "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer", "description": "A roll token (`rlt_…`, `rlo_…`, `usr_…`) or the operator's" } },
            "responses": { "Error": { "description": "What's wrong: 400 a bad request, 401/403 not allowed, 404 not found, 409 a conflict, 413 too large, 429 too many", "content": { "application/json": { "schema": { "type": "object", "required": ["error"], "properties": { "error": { "type": "string" } } } } } } },
        },
        "paths": paths,
    });
    let text = serde_json::to_string_pretty(&spec).unwrap() + "\n";
    let file = docs.join("openapi.json");
    if std::env::var("ROLL_OPENAPI").as_deref() == Ok("write") {
        std::fs::write(&file, &text).unwrap();
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap_or_default(), text, "docs/apps/openapi.json is stale: ROLL_OPENAPI=write cargo test -p apps-control --test api openapi");
}

/// A "deploy to roll" link for a public Git repository: its root roll.toml read by URL (no app,
/// no mirror), its folders made Git URL services at that folder on the branch it was read from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_public_repos_roll_toml_imports_by_url() {
    let w = world().await;
    let git = Arc::new(GitHost::default());
    git.commit(&[("roll.toml", "[apps.web]\nroot = \"web\"\nlinks = [\"db\"]\n[databases.db]\nkind = \"postgres\"\n"), ("README.md", "a site")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/git/acme/site", listener.local_addr().unwrap());
    let app = code_git::router(git.clone(), code_git::ServerConfig::default());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (st, imp) = call(&w.api, "POST", "/v1/apps/import", Some(json!({ "git": url }))).await;
    assert_eq!(st, StatusCode::OK, "{imp}");
    assert_eq!(imp["apps"]["web"]["source"], json!({ "kind": "git", "url": url, "root": "web" }), "{imp}");
    assert_eq!(imp["apps"]["web"]["branch"], "main", "{imp}");
    assert_eq!(imp["databases"]["db"]["kind"], "postgres", "{imp}");
    git.commit(&[("README.md", "no file")]);
    let (st, e) = call(&w.api, "POST", "/v1/apps/import", Some(json!({ "git": url }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"].as_str().unwrap().contains("has no roll.toml"), "{e}");
}

/// Minutes older than a week are rolled up into hours before they go (counts summed, peaks
/// kept), and a range past a week answers a point an hour, those hours and the week's minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_past_a_week_come_from_hourly_rollups() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let now = code_sql::now_ms() / 60_000;
    let old = (now - 10 * 24 * 60) / 60 * 60;
    let row = |m: i64, requests: i64, p95: f64| apps_control::store::MinuteRow { minute: m, requests, p95_ms: p95, instances: 1, ..Default::default() };
    // Two minutes of one hour ten days ago, and one two days ago.
    w.apps.store.put_minutes(&id, "production", &[row(old + 1, 30, 120.0), row(old + 2, 12, 400.0), row(now - 2 * 24 * 60, 5, 50.0)]).unwrap();
    w.apps.store.prune_metrics(now - 7 * 24 * 60).unwrap();
    let base = format!("/v1/apps/services/{id}/environments/production/metrics");
    let (_, m) = call(&w.api, "GET", &format!("{base}?minutes={}", 30 * 24 * 60), None).await;
    assert_eq!(m["step"], 60, "{}", m["step"]);
    let requests: Vec<i64> = m["requests"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    assert_eq!(requests.iter().sum::<i64>(), 47, "every request counted once");
    let at = m["minutes"].as_array().unwrap().iter().position(|v| v.as_i64() == Some(old)).expect("the old hour");
    assert_eq!((requests[at], m["p95_ms"][at].as_f64()), (42, Some(400.0)), "summed, and the peak");
    // A week is minutes again, without the rolled-up ones.
    let (_, wk) = call(&w.api, "GET", &format!("{base}?minutes={}", 7 * 24 * 60), None).await;
    assert_eq!((wk["step"].as_i64(), wk["requests"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).sum::<i64>()), (Some(1), 5));
}

/// An image built elsewhere (`docker save`'s archive) deploys without a build: loaded with its
/// own names taken out (it can't retag anything on the host), tagged as a build's would be.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uploaded_image_deploys_without_its_names() {
    let w = world().await;
    let (svc, _) = image_service(&w, "api", json!({"port": 80})).await;
    let id = svc["id"].as_str().unwrap().to_string();
    let mut b = tar::Builder::new(Vec::new());
    for (name, body) in [("manifest.json", r#"[{"Config":"blobs/sha256/c","RepoTags":["nginx:latest"],"Layers":["blobs/sha256/l"]}]"#), ("blobs/sha256/l", "layer")] {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, body.as_bytes()).unwrap();
    }
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/apps/services/{id}/environments/production/deploys"))
        .header("content-type", "application/x-docker-image")
        .header("x-roll-message", "built on my laptop")
        .body(Body::from(b.into_inner().unwrap()))
        .unwrap();
    let resp = w.api.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let d: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let d = settled(&w, &id, "production", d["id"].as_str().unwrap()).await;
    assert_eq!((d["status"].as_str(), d["trigger"].as_str(), d["message"].as_str()), (Some("active"), Some("upload"), Some("built on my laptop")), "{d}");
    assert!(d["image"].as_str().unwrap().starts_with("roll-apps-"), "{d}");
    let loaded = w.fake.loaded.lock().last().cloned().unwrap();
    assert!(!loaded.contains("nginx") && loaded.contains("blobs/sha256/l"), "{loaded}");
}

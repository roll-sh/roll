//! roll apps on real Docker (`ROLL_APPS_DOCKER=1`; `pnpm test:apps --docker`): a build from an
//! uploaded tree, and the three ways a request reaches an instance, timed. The numbers go in
//! docs/apps/README.md, "Measured".
//!
//! Everything it makes is named `roll-apps-…` and removed at the end (the service's delete
//! removes its containers and images; the network is removed by its exact name).

use apps_control::integrations::{NoResources, NoSecrets, StaticDns};
use apps_control::runtime::docker::{Docker, DockerBuilder};
use apps_control::store::{Sealer, Store};
use apps_control::{Apps, AppsConfig, Deps};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

async fn call(r: &axum::Router, method: &str, path: &str, body: Option<Value>, ct: &str, raw: Option<Vec<u8>>) -> (StatusCode, Value) {
    let b = match (body, raw) {
        (_, Some(raw)) => Body::from(raw),
        (Some(b), None) => Body::from(b.to_string()),
        (None, None) => Body::empty(),
    };
    let resp = r.clone().oneshot(Request::builder().method(method).uri(path).header("content-type", ct).body(b).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn get(public: &axum::Router, host: &str) -> (StatusCode, Duration) {
    let t = Instant::now();
    let resp = public.clone().oneshot(Request::get("/").header("host", host).body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let _ = resp.into_body().collect().await;
    (status, t.elapsed())
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

struct World {
    _dir: tempfile::TempDir,
    apps: Arc<Apps>,
    api: axum::Router,
    public: axum::Router,
    project: String,
}

/// roll apps on this host's Docker, or None when the Docker tests aren't asked for.
async fn docker_world() -> Option<World> {
    if std::env::var("ROLL_APPS_DOCKER").ok().as_deref() != Some("1") || !apps_control::runtime::docker::available().await {
        eprintln!("skipped: set ROLL_APPS_DOCKER=1 with Docker running");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("measure", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let store = Store::new(db, Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
    let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
    let mut cfg = AppsConfig::new(dir.path().to_path_buf());
    cfg.live.probe_every = Duration::from_millis(5);
    let deps = Deps {
        runtime: Arc::new(Docker::new(dir.path().join("scratch"))),
        builder: Arc::new(DockerBuilder::new(2)),
        sources: None,
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: Arc::new(StaticDns::default()),
        tls: Arc::new(apps_control::integrations::NoTls),
        objects: None,
        cluster,
        workspace: Some(ws),
    };
    let apps = Apps::new(cfg, store, deps);
    apps.start();
    let api = apps_control::api::router(apps.clone());
    let public = apps_control::public::router(apps.clone());
    Some(World { _dir: dir, apps, api, public, project })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_site_builds_and_its_cold_starts_are_measured() {
    let Some(World { _dir, apps, api, public, project }) = docker_world().await else { return };
    // A pull through the Engine API with a login header (an empty one is anonymous): what a
    // private registry's pull runs, its progress read off the stream.
    {
        let engine = apps_control::runtime::engine::Engine::local().expect("a local docker socket");
        let login = apps_control::runtime::RegistryLogin { server: "https://index.docker.io/v1/".into(), username: String::new(), password: String::new() };
        let hub = apps_control::logs::LogHub::new(None);
        engine.pull("busybox:1.37", &login, &hub.sink("pull", apps_control::logs::Stream::Build)).await.unwrap();
        let lines = hub.tail("pull", 50, None).await;
        assert!(lines.iter().any(|l| l.text.contains("busybox:1.37") || l.text.contains("Image is up to date") || l.text.contains("Downloaded")), "{lines:?}");
    }

    let (st, svc) = call(&api, "POST", "/v1/apps/services", Some(json!({ "project": project, "name": "site", "source": {"kind": "upload"}, "build": {"build": "test \"$GREETING\" = hej"}, "run": {"memory_mb": 256, "cpu": 0.25, "pre_deploy": "test \"$GREETING\" = hej"} })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    // The build step sees the environment's variables (a BuildKit secret): it fails without.
    let (st, v) = call(&api, "PATCH", &format!("/v1/apps/services/{id}/environments/production/variables"), Some(json!({"set": {"GREETING": "hej"}})), "application/json", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("index.html"), "<!doctype html><h1>roll apps</h1>").unwrap();
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let built = Instant::now();
    let (st, d) = call(&api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), None, "application/gzip", Some(bundle)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let dep = d["id"].as_str().unwrap().to_string();
    let mut d = Value::Null;
    for _ in 0..1200 {
        let (_, x) = call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}"), None, "application/json", None).await;
        if !["queued", "building", "deploying"].contains(&x["status"].as_str().unwrap_or("")) {
            d = x;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let first_deploy = built.elapsed();
    let (_, log) = call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}/logs?tail=400"), None, "application/json", None).await;
    assert_eq!(d["status"], "active", "{d}\n{log}");
    assert!(log.as_array().unwrap().iter().any(|l| l["text"] == "pre-deploy done"), "the pre-deploy command ran in a container of the new image");
    // A one-off job: the live image, its variables, to its exit, its container gone after.
    let (st, job) = call(&api, "POST", &format!("/v1/apps/services/{id}/environments/production/jobs"), Some(json!({"command": "test \"$GREETING\" = hej && echo ran"})), "application/json", None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{job}");
    let mut job_status = Value::Null;
    for _ in 0..300 {
        let (_, j) = call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production/jobs/{}", job["id"].as_str().unwrap()), None, "application/json", None).await;
        if j["status"] != "running" {
            job_status = j;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(job_status["status"], "succeeded", "{job_status}");
    // A shell in the running instance (a TTY exec over the Engine API), over a WebSocket.
    {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, api.clone()).into_future());
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/apps/services/{id}/environments/production/shell?cols=120&rows=40")).await.unwrap();
        ws.send(Message::Binary(b"echo \"[$GREETING]\"; exit\r".to_vec().into())).await.unwrap();
        let mut screen = Vec::new();
        while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            screen.extend_from_slice(&m.into_data());
            if screen.windows(5).any(|w| w == b"[hej]") {
                break;
            }
        }
        assert!(screen.windows(5).any(|w| w == b"[hej]"), "{}", String::from_utf8_lossy(&screen));
    }
    let (_, env) = call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await;
    let host = env["url"].as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string();
    let live = apps.live(&id, "production");

    // Five rounds of each path: from stopped (a start), from suspended (a thaw), warm.
    let (mut cold, mut thaw, mut warm) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..5 {
        live.sleep().await;
        let (st, t) = get(&public, &host).await;
        assert_eq!(st, StatusCode::OK);
        cold.push(t);
        warm.push(get(&public, &host).await.1);
        live.backdate(Duration::from_secs(31));
        live.tick().await;
        assert_eq!(live.state(), apps_control::model::InstanceState::Suspended);
        let (st, t) = get(&public, &host).await;
        assert_eq!(st, StatusCode::OK);
        thaw.push(t);
    }
    // It started with the boost and runs within its own limit once it answered.
    let (st, _) = get(&public, &host).await;
    assert_eq!(st, StatusCode::OK);
    let name = live.instances()[0].name.clone();
    let mut nanos = String::new();
    for _ in 0..50 {
        nanos = apps_control::runtime::docker::docker(&["inspect", "-f", "{{.HostConfig.NanoCpus}}", &name]).await.unwrap();
        if nanos == "250000000" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(nanos, "250000000", "a running instance is held to its cpu: 0.25");
    // Idle: stopped, and nothing metered meanwhile.
    live.sleep().await;
    eprintln!(
        "MEASURED first deploy (build + health check): {:.1} s; cold start from stopped: median {} ms (all {:?}); resume from suspended: median {} ms; warm: median {} ms",
        first_deploy.as_secs_f64(),
        median(cold.clone()).as_millis(),
        cold.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        median(thaw).as_millis(),
        median(warm).as_millis(),
    );
    let (st, _) = call(&api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let net = apps_control::network_name(&project, "production");
    let _ = apps_control::runtime::docker::docker(&["network", "rm", &net]).await;
    assert!(apps_control::runtime::docker::docker(&["ps", "-aq", "--filter", &format!("label=sh.roll.apps={id}")]).await.unwrap().is_empty(), "nothing left");
    apps.shutdown().await;
}

/// A service from an upload of `files`, built from its generated Dockerfile and live: its id and host.
async fn deploy_tree(w: &World, name: &str, files: &[(&str, &str)]) -> (String, String) {
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": name, "source": {"kind": "upload"}, "run": {"memory_mb": 256, "cpu": 0.5} })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let tree = tempfile::tempdir().unwrap();
    for (path, body) in files {
        std::fs::write(tree.path().join(path), body).unwrap();
    }
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let (st, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), None, "application/gzip", Some(bundle)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let dep = d["id"].as_str().unwrap().to_string();
    let mut d = Value::Null;
    for _ in 0..3000 {
        let (_, x) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}"), None, "application/json", None).await;
        if !["queued", "building", "deploying"].contains(&x["status"].as_str().unwrap_or("")) {
            d = x;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}/logs?tail=200"), None, "application/json", None).await;
    assert_eq!(d["status"], "active", "{name}: {d}\n{log}");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await;
    (id, env["url"].as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string())
}

/// What the generated Dockerfiles of Node, Python and Go make actually builds and answers, and a
/// Node cold start (node started directly, no npm in front) is measured.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generated_node_python_and_go_images_build_and_answer() {
    let Some(w) = docker_world().await else { return };
    let node = deploy_tree(&w, "node", &[
        ("package.json", r#"{"name": "n", "version": "1.0.0", "scripts": {"start": "node server.js"}}"#),
        ("server.js", "require('http').createServer((q, r) => r.end('node ok')).listen(process.env.PORT)"),
    ]).await;
    let python = deploy_tree(&w, "python", &[
        ("requirements.txt", ""),
        ("main.py", "import os, http.server\nclass H(http.server.BaseHTTPRequestHandler):\n    def do_GET(self):\n        self.send_response(200); self.end_headers(); self.wfile.write(b'python ok')\nhttp.server.HTTPServer(('0.0.0.0', int(os.environ['PORT'])), H).serve_forever()\n"),
    ]).await;
    let go = deploy_tree(&w, "go", &[
        ("go.mod", "module example.com/g\n\ngo 1.24\n"),
        ("main.go", "package main\n\nimport (\n\t\"net/http\"\n\t\"os\"\n)\n\nfunc main() {\n\thttp.HandleFunc(\"/\", func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(\"go ok\")) })\n\thttp.ListenAndServe(\":\"+os.Getenv(\"PORT\"), nil)\n}\n"),
    ]).await;
    for ((_, host), want) in [(&node, "node ok"), (&python, "python ok"), (&go, "go ok")] {
        let resp = w.public.clone().oneshot(Request::get("/").header("host", host.as_str()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{want}");
        assert_eq!(&resp.into_body().collect().await.unwrap().to_bytes()[..], want.as_bytes());
    }
    let live = w.apps.live(&node.0, "production");
    let mut cold = Vec::new();
    for _ in 0..5 {
        live.sleep().await;
        let (st, t) = get(&w.public, &node.1).await;
        assert_eq!(st, StatusCode::OK);
        cold.push(t);
    }
    eprintln!("MEASURED Node 22 cold start from stopped (node server.js, no npm): median {} ms (all {:?})", median(cold.clone()).as_millis(), cold.iter().map(|d| d.as_millis()).collect::<Vec<_>>());
    for (id, _) in [&node, &python, &go] {
        assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
    }
    let _ = apps_control::runtime::docker::docker(&["network", "rm", &apps_control::network_name(&w.project, "production")]).await;
    w.apps.shutdown().await;
}

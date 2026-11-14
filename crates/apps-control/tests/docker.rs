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
    db: code_sql::Db,
    ws: Arc<roll_workspace::Workspace>,
}

impl World {
    /// roll apps again on the same store and home, a new process (nothing in memory): the old
    /// one's loops stop, its containers don't.
    async fn restart(&mut self) {
        self.apps.shutdown().await;
        let store = Store::new(self.db.clone(), Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
        let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
        let mut cfg = AppsConfig::new(self._dir.path().to_path_buf());
        cfg.live.probe_every = Duration::from_millis(5);
        let deps = Deps {
            runtime: Arc::new(Docker::new(self._dir.path().join("scratch"))),
            builder: Arc::new(DockerBuilder::new(2)),
            sources: None,
            secrets: Arc::new(NoSecrets),
            resources: Arc::new(NoResources),
            resolver: Arc::new(StaticDns::default()),
            tls: Arc::new(apps_control::integrations::NoTls),
            objects: None,
            cluster,
            workspace: Some(self.ws.clone()),
        };
        let apps = Apps::new(cfg, store, deps);
        apps.start();
        (self.api, self.public, self.apps) = (apps_control::api::router(apps.clone()), apps_control::public::router(apps.clone()), apps);
    }
}

/// roll apps on this host's Docker, or None when the Docker tests aren't asked for.
async fn docker_world() -> Option<World> {
    docker_world_netns(None).await
}

async fn docker_world_netns(netns: Option<bool>) -> Option<World> {
    docker_world_with(netns, |_| {}).await
}

async fn docker_world_with(netns: Option<bool>, setup: impl FnOnce(&mut AppsConfig)) -> Option<World> {
    if std::env::var("ROLL_APPS_DOCKER").ok().as_deref() != Some("1") || !apps_control::runtime::docker::available().await {
        eprintln!("skipped: set ROLL_APPS_DOCKER=1 with Docker running");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("", "measure", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let store = Store::new(db.clone(), Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
    let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
    let mut cfg = AppsConfig::new(dir.path().to_path_buf());
    cfg.live.probe_every = Duration::from_millis(5);
    // A TCP service's port: one free here.
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    cfg.tcp_ports = Some((free, free));
    cfg.tcp_host = "127.0.0.1".into();
    setup(&mut cfg);
    let deps = Deps {
        runtime: Arc::new(match netns {
            Some(on) => Docker::new(dir.path().join("scratch")).with_netns(on),
            None => Docker::new(dir.path().join("scratch")),
        }),
        builder: Arc::new(DockerBuilder::new(2)),
        sources: None,
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: Arc::new(StaticDns::default()),
        tls: Arc::new(apps_control::integrations::NoTls),
        objects: None,
        cluster,
        workspace: Some(ws.clone()),
    };
    let apps = Apps::new(cfg, store, deps);
    apps.start();
    let api = apps_control::api::router(apps.clone());
    let public = apps_control::public::router(apps.clone());
    Some(World { _dir: dir, apps, api, public, project, db, ws })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_site_builds_and_its_cold_starts_are_measured() {
    let Some(World { _dir, apps, api, public, project, .. }) = docker_world().await else { return };
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

    let (st, svc) = call(&api, "POST", "/v1/apps/services", Some(json!({ "project": project, "name": "site", "source": {"kind": "upload"}, "build": {"build": "test \"$GREETING\" = hej", "start": "httpd -f -p $PORT -h /site"}, "run": {"memory_mb": 256, "cpu": 0.25, "pre_deploy": "test \"$GREETING\" = hej"} })), "application/json", None).await;
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

/// A static site is served from its files, exported from its image once: no container runs, so
/// there's no cold start; the first request reads the site's index, the rest are warm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_sites_files_answer_without_a_container() {
    let Some(w) = docker_world().await else { return };
    let (id, host) = deploy_tree(&w, "files", &[("index.html", "<!doctype html><h1>from files</h1>")]).await;
    let (mut first, mut warm) = (Vec::new(), Vec::new());
    for i in 0..6 {
        let began = Instant::now();
        let resp = w.public.clone().oneshot(Request::get("/").header("host", host.as_str()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(&resp.into_body().collect().await.unwrap().to_bytes()[..], b"<!doctype html><h1>from files</h1>");
        if i == 0 { first.push(began.elapsed()) } else { warm.push(began.elapsed()) }
    }
    let running = apps_control::runtime::docker::docker(&["ps", "-q", "--filter", &format!("label=sh.roll.apps={id}")]).await.unwrap();
    assert!(running.is_empty(), "no container serves it: {running}");
    eprintln!("MEASURED static site from its files: first request {} ms (reads its index), warm median {} ms", first[0].as_millis(), median(warm).as_millis());
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
    let _ = apps_control::runtime::docker::docker(&["network", "rm", &apps_control::network_name(&w.project, "production")]).await;
    w.apps.shutdown().await;
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

/// A volume's backup reads back as a tar that restores it; a cut-short backup restores nothing
/// and leaves the volume as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volume_restores_from_its_tar_and_a_broken_one_changes_nothing() {
    use apps_control::runtime::Runtime;
    use tokio::io::AsyncReadExt;
    if std::env::var("ROLL_APPS_DOCKER").ok().as_deref() != Some("1") || !apps_control::runtime::docker::available().await {
        eprintln!("skipped: set ROLL_APPS_DOCKER=1 with Docker running");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rt = Docker::new(dir.path().join("scratch"));
    let vol = format!("roll-apps-test-restore-{}", std::process::id());
    let sh = |script: &str| {
        let out = std::process::Command::new("docker").args(["run", "--rm", "-v", &format!("{vol}:/data"), "busybox:1.37", "sh", "-c", script]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    sh("mkdir -p /data/a && echo one > /data/a/f && echo dot > /data/.hidden");
    let mut tar = Vec::new();
    rt.read_volume(&vol).await.unwrap().read_to_end(&mut tar).await.unwrap();
    sh("rm -rf /data/* /data/.hidden && echo later > /data/later");
    // Cut short: refused, and the volume keeps what it had.
    let err = rt.write_volume(&vol, Box::new(std::io::Cursor::new(tar[..700].to_vec()))).await;
    assert!(err.is_err(), "a broken tar is refused");
    assert_eq!(sh("ls -A /data"), "later\n");
    // Whole: exactly the backup's files.
    rt.write_volume(&vol, Box::new(std::io::Cursor::new(tar))).await.unwrap();
    assert_eq!(sh("ls -A /data && cat /data/a/f"), ".hidden\na\none\n");
    rt.remove_volume(&vol).await;
}

/// A cold start with the instance's network in a sandbox made with it (the wake starts only the
/// app, in the sandbox's network namespace) against the instance holding its own: alternating
/// pairs on the same image, everything removed after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_start_skips_network_setup_with_a_sandbox() {
    let (Some(on), Some(off)) = (docker_world_netns(Some(true)).await, docker_world_netns(Some(false)).await) else { return };
    let (a, ah) = deploy_container(&on, "sandboxed").await;
    let (b, bh) = deploy_container(&off, "own").await;
    let cold = |w: &World, id: String, host: String| {
        let (apps, public) = (w.apps.clone(), w.public.clone());
        async move {
            apps.live(&id, "production").sleep().await;
            let (st, t) = get(&public, &host).await;
            assert_eq!(st, StatusCode::OK);
            t
        }
    };
    let (mut with, mut without, mut faster) = (Vec::new(), Vec::new(), 0);
    for i in 0..5 {
        // Alternating which goes first.
        let (x, y) = if i % 2 == 0 {
            let x = cold(&on, a.clone(), ah.clone()).await;
            (x, cold(&off, b.clone(), bh.clone()).await)
        } else {
            let y = cold(&off, b.clone(), bh.clone()).await;
            (cold(&on, a.clone(), ah.clone()).await, y)
        };
        faster += usize::from(x < y);
        with.push(x);
        without.push(y);
    }
    let sandboxes = apps_control::runtime::docker::docker(&["ps", "-q", "--filter", &format!("label=sh.roll.apps={a}"), "--filter", "label=sh.roll.apps.role=network"]).await.unwrap();
    assert!(!sandboxes.is_empty(), "the sandboxed instance has its network sandbox");
    eprintln!(
        "MEASURED cold start, network in a sandbox vs its own: median {} ms vs {} ms (pairs {:?} vs {:?}; sandbox faster in {faster} of 5)",
        median(with.clone()).as_millis(),
        median(without.clone()).as_millis(),
        with.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        without.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
    );
    for (w, id) in [(&on, &a), (&off, &b)] {
        assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
        let _ = apps_control::runtime::docker::docker(&["network", "rm", &apps_control::network_name(&w.project, "production")]).await;
    }
    let left = apps_control::runtime::docker::docker(&["ps", "-aq", "--filter", &format!("label=sh.roll.apps={a}")]).await.unwrap();
    assert!(left.is_empty(), "the instance and its sandbox are gone: {left}");
    on.apps.shutdown().await;
    off.apps.shutdown().await;
}

/// A small busybox site run as a container (a start command keeps it from being served as files): its id and host.
async fn deploy_container(w: &World, name: &str) -> (String, String) {
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": name, "source": {"kind": "upload"}, "build": {"start": "httpd -f -p $PORT -h /site"}, "run": {"memory_mb": 256, "cpu": 0.5} })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("index.html"), "<h1>cold</h1>").unwrap();
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), None, "application/gzip", Some(bundle)).await;
    let dep = d["id"].as_str().unwrap().to_string();
    for _ in 0..1200 {
        let (_, x) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}"), None, "application/json", None).await;
        if x["status"] == "active" {
            break;
        }
        assert!(!["failed", "canceled"].contains(&x["status"].as_str().unwrap_or("")), "{x}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await;
    (id, env["url"].as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string())
}

/// Another service of the project reaches a sleeping one by name: stopped, the waker in its
/// sandbox takes the call and hands it to the app once it's up; frozen, it's thawed. A public
/// request still wakes it afterwards (the waker lets go of the port first).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_caller_wakes_a_sleeping_service() {
    use apps_control::runtime::docker::docker;
    let Some(w) = docker_world_netns(Some(true)).await else { return };
    let (id, host) = deploy_container(&w, "inner").await;
    let (_, svc) = call(&w.api, "GET", &format!("/v1/apps/services/{id}"), None, "application/json", None).await;
    let port = svc["run"]["port"].as_u64().unwrap_or(8080);
    let net = apps_control::network_name(&w.project, "production");
    // A caller on the project's network, as another service is.
    let caller = format!("roll-apps-test-caller-{}", &id[4..]);
    docker(&["run", "-d", "--name", &caller, "--network", &net, "busybox:1.37", "sleep", "600"]).await.unwrap();
    let url = format!("http://inner:{port}/");
    let fetch = || async {
        let t = Instant::now();
        let body = docker(&["exec", &caller, "wget", "-q", "-O-", "-T", "30", &url]).await;
        (body, t.elapsed())
    };
    let live = w.apps.live(&id, "production");
    let state = || async { call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await.1["state"].as_str().unwrap_or("").to_string() };
    // Awake: the baseline (an exec and a request).
    assert_eq!(get(&w.public, &host).await.0, StatusCode::OK);
    let (body, awake) = fetch().await;
    assert!(body.as_deref().is_ok_and(|b| b.contains("<h1>cold</h1>")), "{body:?}");
    // Stopped.
    live.sleep().await;
    assert_eq!(state().await, "stopped");
    let wakers = docker(&["ps", "-q", "--filter", &format!("label=sh.roll.apps={id}"), "--filter", "label=sh.roll.apps.role=wake"]).await.unwrap();
    assert!(!wakers.is_empty(), "asleep, the waker listens");
    let (body, stopped) = fetch().await;
    assert!(body.as_deref().is_ok_and(|b| b.contains("<h1>cold</h1>")), "the first caller gets the app: {body:?}");
    assert_eq!(state().await, "running");
    // Frozen.
    live.backdate(Duration::from_secs(31));
    live.tick().await;
    assert_eq!(state().await, "suspended");
    let (body, frozen) = fetch().await;
    assert!(body.is_ok(), "{body:?}");
    let mut now = String::new();
    for _ in 0..100 {
        now = state().await;
        if now == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(now, "running", "{}", call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await.1);
    // Asleep again, a public request starts it: the waker gives up the port in time.
    live.sleep().await;
    let (st, public) = get(&w.public, &host).await;
    if st != StatusCode::OK {
        let (_, l) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/logs?tail=40"), None, "application/json", None).await;
        for x in l.as_array().into_iter().flatten() {
            eprintln!("LOG {} {}", x["stream"], x["text"]);
        }
        let _ = docker(&["rm", "-f", &caller]).await;
    }
    assert_eq!(st, StatusCode::OK);
    eprintln!(
        "MEASURED a private caller (docker exec + wget) at a service that's awake {} ms, stopped {} ms, frozen {} ms; a public cold start with the waker armed {} ms",
        awake.as_millis(),
        stopped.as_millis(),
        frozen.as_millis(),
        public.as_millis()
    );
    let _ = docker(&["rm", "-f", &caller]).await;
    // From inside its network, as roll previews' screenshots reach it: asleep, woken, held.
    live.sleep().await;
    let r = w.apps.reachable(&id, "production").await.unwrap().expect("a container to join");
    let inside = docker(&["run", "--rm", "--network", &format!("container:{}", r.container), "--cap-drop", "ALL", "--read-only", "busybox:1.37", "wget", "-q", "-O-", &format!("http://127.0.0.1:{}/", r.port)]).await;
    assert!(inside.as_deref().is_ok_and(|b| b.contains("<h1>cold</h1>")), "{inside:?}");
    drop(r);
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
    let _ = docker(&["network", "rm", &net]).await;
    let left = docker(&["ps", "-aq", "--filter", &format!("label=sh.roll.apps={id}")]).await.unwrap();
    assert!(left.is_empty(), "the instance, its sandbox and its waker are gone: {left}");
    w.apps.shutdown().await;
}

/// A TCP service and a tunnel on real Docker: a busybox echo server, asleep each time, answers
/// a raw connection to its own port and a tunnel's bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tcp_service_and_a_tunnel_reach_a_sleeping_port() {
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message;
    let Some(w) = docker_world().await else { return };
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "echo", "source": {"kind": "image", "image": "busybox:1.37"}, "run": {"port": 9000, "tcp": true, "start": "nc -lk -p 9000 -e cat", "memory_mb": 256, "cpu": 0.25} })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    // The loop gives its environment a port and listens there.
    tokio::spawn(w.apps.clone().serve_tcp_ports("127.0.0.1".parse().unwrap()));
    for _ in 0..1200 {
        let (_, e) = call(&w.api, "GET", &base, None, "application/json", None).await;
        if e["active"].is_object() && e["tcp"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (_, e) = call(&w.api, "GET", &base, None, "application/json", None).await;
    let addr = e["tcp"].as_str().unwrap_or_else(|| panic!("{e}")).to_string();
    let live = w.apps.live(&id, "production");
    // Its own port, from asleep.
    live.sleep().await;
    let mut conn = None;
    for _ in 0..100 {
        if let Ok(c) = tokio::net::TcpStream::connect(addr.as_str()).await {
            conn = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let mut conn = conn.expect("its port listens");
    let began = Instant::now();
    conn.write_all(b"over tcp\n").await.unwrap();
    let mut buf = [0u8; 9];
    tokio::time::timeout(Duration::from_secs(30), conn.read_exact(&mut buf)).await.expect("an echo").unwrap();
    assert_eq!(&buf, b"over tcp\n");
    let tcp_cold = began.elapsed();
    drop(conn);
    // A tunnel, from asleep.
    tokio::time::sleep(Duration::from_millis(200)).await;
    live.sleep().await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, w.api.clone()).into_future());
    let began = Instant::now();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{at}{base}/tunnel")).await.unwrap();
    ws.send(Message::Binary(b"tunnelled\n".to_vec().into())).await.unwrap();
    let mut got = Vec::new();
    while got.len() < 10 {
        match tokio::time::timeout(Duration::from_secs(30), ws.next()).await.expect("an echo") {
            Some(Ok(Message::Binary(b))) => got.extend_from_slice(&b),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(&got, b"tunnelled\n");
    eprintln!("MEASURED a TCP service from asleep: {} ms to its echo; a tunnel from asleep: {} ms", tcp_cold.as_millis(), began.elapsed().as_millis());
    let _ = ws.close(None).await;
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
    let _ = apps_control::runtime::docker::docker(&["network", "rm", &apps_control::network_name(&w.project, "production")]).await;
    w.apps.shutdown().await;
}

/// `run.cpu_target` on real cgroups: an instance kept busy (a loop beside its server) is over
/// its target, and a second one starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_instance_brings_another_on_its_cpu() {
    let Some(w) = docker_world_with(None, |c| c.live.cpu_window = Duration::from_secs(3)).await else { return };
    let run = json!({"port": 8080, "min_instances": 1, "max_instances": 2, "cpu": 0.25, "memory_mb": 256, "cpu_target": 50, "start": "while :; do :; done & mkdir -p /w && echo busy > /w/index.html && httpd -f -p 8080 -h /w"});
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "busy", "source": {"kind": "image", "image": "busybox:1.37"}, "run": run })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let running = || async {
        let (_, e) = call(&w.api, "GET", &base, None, "application/json", None).await;
        e["instances"].as_array().map_or(0, |a| a.iter().filter(|i| i["state"] == "running").count())
    };
    let began = Instant::now();
    let mut two = false;
    while began.elapsed() < Duration::from_secs(90) {
        if running().await >= 2 {
            two = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let (_, log) = call(&w.api, "GET", &format!("{base}/logs?tail=50"), None, "application/json", None).await;
    let said = log.as_array().into_iter().flatten().find_map(|l| l["text"].as_str().filter(|t| t.starts_with("scaled out: CPU")).map(str::to_string));
    assert!(two, "a second instance for its CPU: {log}");
    eprintln!("MEASURED CPU scaling on cgroups: a second instance {} s after the service was made ({})", began.elapsed().as_secs(), said.unwrap_or_default());
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
    let _ = apps_control::runtime::docker::docker(&["network", "rm", &apps_control::network_name(&w.project, "production")]).await;
    w.apps.shutdown().await;
}


/// rolld starting again (an upgrade, a crash) keeps the container the last process ran: the
/// next visitor gets the same container, not started again (its start time unchanged), and
/// one that was frozen is thawed. The instance's port is its network sandbox's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_running_container() {
    let Some(mut w) = docker_world().await else { return };
    let (id, host) = deploy_container(&w, "kept").await;
    assert_eq!(get(&w.public, &host).await.0, StatusCode::OK);
    let short = id.trim_start_matches("svc_");
    let started = |name: String| async move { apps_control::runtime::docker::docker(&["inspect", "-f", "{{.State.StartedAt}}", &name]).await.unwrap() };
    let names = apps_control::runtime::docker::docker(&["ps", "--filter", &format!("label=sh.roll.apps={id}"), "--format", "{{.Names}}"]).await.unwrap();
    let name = names.lines().find(|n| n.starts_with(&format!("roll-apps-{short}-production-")) && !n.ends_with("-net") && !n.ends_with("-wake")).unwrap_or_else(|| panic!("{names}")).to_string();
    let before = started(name.clone()).await;
    w.restart().await;
    assert_eq!(get(&w.public, &host).await.0, StatusCode::OK);
    assert_eq!(started(name.clone()).await, before, "{name} kept running, not started again");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await;
    assert!(env["instances"].as_array().unwrap().iter().any(|i| i["id"] == name.as_str()), "{env}");
    // Frozen when the process goes: still frozen, then thawed by the next visit.
    let live = w.apps.live(&id, "production");
    live.backdate(Duration::from_secs(31));
    live.tick().await;
    let paused = || async { apps_control::runtime::docker::docker(&["inspect", "-f", "{{.State.Paused}}", &name]).await };
    assert_eq!(paused().await.unwrap(), "true");
    w.restart().await;
    let mut env = Value::Null;
    for _ in 0..100 {
        env = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await.1;
        if env["state"] == "suspended" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(env["state"], "suspended", "{env}");
    assert_eq!(get(&w.public, &host).await.0, StatusCode::OK);
    assert_eq!(paused().await.unwrap(), "false");
    assert_eq!(started(name.clone()).await, before, "thawed, not started again");
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
}

/// An app bound to loopback in its container (a framework's dev default) fails its deploy at once
/// saying so, read from its sandbox's sockets, not after the whole health timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_on_loopback_fails_its_deploy_saying_so() {
    let Some(w) = docker_world().await else { return };
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "loopback", "source": {"kind": "upload"}, "build": {"start": "httpd -f -p 127.0.0.1:$PORT -h /site"}, "run": {"memory_mb": 256, "cpu": 0.5, "health_timeout_s": 30} })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("index.html"), "<h1>hi</h1>").unwrap();
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), None, "application/gzip", Some(bundle)).await;
    let dep = d["id"].as_str().unwrap().to_string();
    let mut x = Value::Null;
    for _ in 0..1200 {
        x = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}"), None, "application/json", None).await.1;
        if ["failed", "active"].contains(&x["status"].as_str().unwrap_or("")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(x["status"], "failed", "{x}");
    assert!(x["error"].as_str().unwrap_or("").contains("only, which nothing outside its container reaches"), "{x}");
    let took = x["finished_at"].as_i64().unwrap() - x["steps"].as_array().unwrap().iter().find(|s| s["step"] == "rollout").and_then(|s| s["at"].as_i64()).unwrap();
    assert!(took < 15_000, "not the whole 30 s health timeout: {took} ms");
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
}

/// A multi-stage Dockerfile whose last stage isn't what runs: `build.target` builds its stage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dockerfiles_stage_is_built_by_its_target() {
    let Some(w) = docker_world().await else { return };
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": "staged", "source": {"kind": "upload"}, "build": {"builder": "dockerfile", "target": "web"}, "run": {"memory_mb": 256, "cpu": 0.5, "health_timeout_s": 30} })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(
        tree.path().join("Dockerfile"),
        "FROM busybox:1.37 AS web\nRUN mkdir /site && echo '<h1>the web stage</h1>' > /site/index.html\nCMD [\"sh\", \"-c\", \"httpd -f -p $PORT -h /site\"]\n\nFROM busybox:1.37\nCMD [\"false\"]\n",
    )
    .unwrap();
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let (_, d) = call(&w.api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), None, "application/gzip", Some(bundle)).await;
    let dep = d["id"].as_str().unwrap().to_string();
    let mut x = Value::Null;
    for _ in 0..1200 {
        x = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}"), None, "application/json", None).await.1;
        if ["failed", "active"].contains(&x["status"].as_str().unwrap_or("")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(x["status"], "active", "the last stage would exit at once: {x}");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await;
    let host = env["url"].as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string();
    let resp = w.public.clone().oneshot(Request::get("/").header("host", host.as_str()).body(Body::empty()).unwrap()).await.unwrap();
    let body = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(body.contains("the web stage"), "{body}");
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await.0, StatusCode::NO_CONTENT);
}


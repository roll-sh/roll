//! roll-hosted sandboxes (`src/agents/sandbox/`, docs/code/agents.md "Sandboxes"): the API, the
//! owner's reconciler and metering against a mock agent37 Sandbox API (no real account), the
//! machine a sandbox gets (its token really opens the relay link), and the docker backend against
//! real Docker (skipped without it).

mod common;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use code_control::agents::sandbox::agent37::Agent37Config;
use code_control::agents::sandbox::docker::{Docker, DockerConfig, Runtime};
use code_control::agents::sandbox::{BackendConfig, BackendState, SandboxBackend, SandboxConfig, SandboxSpec, default_sizes};
use common::daemon::Daemon;
use common::{App, app_with};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ADMIN: &str = "admin-secret";
const KEY: &str = "sk_live_test_key_never_shown";
const SECRET_KEY: &str = "0123456789abcdef0123456789abcdef";

// ── a mock agent37 ──

/// What the mock does on the next create.
#[derive(Clone, Copy, PartialEq)]
enum OnCreate {
    Ok,
    /// 503 no_capacity (worth retrying).
    NoCapacity,
    /// 402 insufficient_balance (won't work).
    Broke,
    /// Make the instance, then answer 500 as if the answer was lost.
    LoseAnswer,
}

#[derive(Default)]
struct Mock {
    instances: BTreeMap<String, Value>,
    /// Create bodies received, in order.
    creates: Vec<Value>,
    /// `METHOD /path` of every call.
    calls: Vec<String>,
    on_create: Vec<OnCreate>,
    next: u32,
}

type M = Arc<Mutex<Mock>>;

fn err(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": { "code": code, "message": format!("mock {code}") } }))).into_response()
}

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {KEY}"))
}

async fn create(State(m): State<M>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authed(&h) {
        return err(StatusCode::UNAUTHORIZED, "invalid_api_key");
    }
    let mut m = m.lock();
    m.calls.push("POST /v1/instances".into());
    m.creates.push(body.clone());
    let what = if m.on_create.is_empty() { OnCreate::Ok } else { m.on_create.remove(0) };
    match what {
        OnCreate::NoCapacity => return err(StatusCode::SERVICE_UNAVAILABLE, "no_capacity"),
        OnCreate::Broke => return err(StatusCode::PAYMENT_REQUIRED, "insufficient_balance"),
        OnCreate::Ok | OnCreate::LoseAnswer => {}
    }
    m.next += 1;
    let id = format!("inst{:06}", m.next);
    let inst = json!({ "id": id, "status": "running", "metadata": body["metadata"], "resources": body["resources"], "template": body["template"] });
    m.instances.insert(id, inst.clone());
    if what == OnCreate::LoseAnswer {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }
    (StatusCode::CREATED, Json(inst)).into_response()
}

async fn list(State(m): State<M>, h: HeaderMap) -> Response {
    if !authed(&h) {
        return err(StatusCode::UNAUTHORIZED, "invalid_api_key");
    }
    let mut m = m.lock();
    m.calls.push("GET /v1/instances".into());
    Json(json!({ "data": m.instances.values().rev().cloned().collect::<Vec<_>>() })).into_response()
}

async fn one(State(m): State<M>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !authed(&h) {
        return err(StatusCode::UNAUTHORIZED, "invalid_api_key");
    }
    let mut m = m.lock();
    m.calls.push(format!("GET /v1/instances/{id}"));
    match m.instances.get(&id) {
        Some(i) => Json(i.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "not_found"),
    }
}

async fn remove(State(m): State<M>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !authed(&h) {
        return err(StatusCode::UNAUTHORIZED, "invalid_api_key");
    }
    let mut m = m.lock();
    m.calls.push(format!("DELETE /v1/instances/{id}"));
    match m.instances.remove(&id) {
        Some(_) => Json(json!({ "id": id, "deleted": true })).into_response(),
        None => err(StatusCode::NOT_FOUND, "not_found"),
    }
}

async fn action(State(m): State<M>, h: HeaderMap, Path((id, action)): Path<(String, String)>) -> Response {
    if !authed(&h) {
        return err(StatusCode::UNAUTHORIZED, "invalid_api_key");
    }
    let mut m = m.lock();
    m.calls.push(format!("POST /v1/instances/{id}/{action}"));
    let Some(i) = m.instances.get_mut(&id) else { return err(StatusCode::NOT_FOUND, "not_found") };
    i["status"] = json!(match action.as_str() {
        "start" => "running",
        "stop" => "stopped",
        _ => return err(StatusCode::NOT_FOUND, "not_found"),
    });
    Json(json!({ "id": id, "status": i["status"] })).into_response()
}

/// The mock on a free port: its URL and state.
async fn agent37() -> (String, M) {
    let m: M = Arc::default();
    let router = Router::new()
        .route("/v1/instances", get(list).post(create))
        .route("/v1/instances/{id}", get(one).delete(remove))
        .route("/v1/instances/{id}/{action}", post(action))
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, m)
}

// ── roll with sandboxes on ──

struct World {
    a: App,
    url: String,
    mock: M,
    owner: String,
}

async fn world(f: impl FnOnce(&mut SandboxConfig)) -> World {
    let (mock_url, mock) = agent37().await;
    let mut a37 = Agent37Config::new(KEY.into());
    a37.url = mock_url;
    a37.template = "roll-agent-sandbox@3".into();
    let mut sc = SandboxConfig { backend: Some(BackendConfig::Agent37(a37)), reconcile_every: Duration::from_millis(100), ..Default::default() };
    f(&mut sc);
    let a = app_with(|c| {
        c.admin_token = Some(ADMIN.into());
        c.review.secret_key = Some(SECRET_KEY.into());
        c.sandboxes = sc;
    });
    for o in ["acme", "other"] {
        let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": o, "kind": "org" })), Some(ADMIN)).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let (_, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "me" })), Some(ADMIN)).await;
    let url = a.serve().await;
    a.code.start_sandboxes();
    World { a, url, mock, owner: t["token"].as_str().unwrap().to_string() }
}

impl World {
    async fn req(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.a.req(method, path, body, Some(&self.owner)).await
    }

    async fn create(&self, body: Value) -> Value {
        let (s, v) = self.req("POST", "/v1/code/agents/acme/sandboxes", Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v
    }

    async fn sandbox(&self, id: &str) -> Option<Value> {
        let (s, v) = self.req("GET", "/v1/code/agents/acme/sandboxes", None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["sandboxes"].as_array().unwrap().iter().find(|x| x["id"] == id).cloned()
    }

    /// Wait until the sandbox satisfies `f` (the reconciler looks every 100 ms).
    async fn until(&self, id: &str, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.sandbox(id).await.unwrap_or(Value::Null);
            if f(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "{what}: {s}");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn machine(&self, id: &str) -> Option<Value> {
        let (_, v) = self.req("GET", "/v1/code/agents/acme/machines", None).await;
        v.as_array().unwrap().iter().find(|m| m["id"] == id).cloned()
    }
}

fn status_is(want: &'static str) -> impl Fn(&Value) -> bool {
    move |s| s["status"] == want
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandbox_runs_on_agent37_from_create_to_delete() {
    let w = world(|_| {}).await;
    let (s, list) = w.req("GET", "/v1/code/agents/acme/sandboxes", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((list["backend"].as_str(), list["max"].as_u64(), list["sizes"][0]["name"].as_str(), list["sizes"][0]["monthly_cents"].as_u64()), (Some("agent37"), Some(5), Some("standard"), Some(2000)));

    let made = w.create(json!({ "name": "box", "size": "standard", "env": { "ANTHROPIC_API_KEY": "sk-ant-owner-key" } })).await;
    let text = made.to_string();
    assert!(!text.contains("sk-ant-owner-key") && !text.contains("rlm_"), "secrets never come back: {text}");
    let id = made["sandbox"]["id"].as_str().unwrap().to_string();
    let machine = made["machine"]["id"].as_str().unwrap().to_string();
    assert_eq!((made["sandbox"]["status"].as_str(), made["sandbox"]["machine"].as_str()), (Some("creating"), Some(machine.as_str())));
    assert_eq!(made["machine"]["sandbox"], json!({ "backend": "agent37", "size": "standard", "status": "creating", "monthly_cents": 2000 }));

    // The reconciler creates the instance from roll's template, with the machine's token.
    let running = w.until(&id, "running", status_is("running")).await;
    assert_eq!(running["error"], Value::Null);
    let body = w.mock.lock().creates[0].clone();
    assert_eq!(body["template"], "roll-agent-sandbox@3");
    assert_eq!(body["resources"], json!({ "cpu": 2, "memory": 4, "disk": 8 }));
    assert_eq!((body["metadata"]["roll_sandbox"].as_str(), body["auto_sleep"].as_bool()), (Some(id.as_str()), Some(false)));
    let env = &body["env"];
    assert_eq!(env["ANTHROPIC_API_KEY"], "sk-ant-owner-key");
    assert_eq!((env["ROLL_API_URL"].as_str(), env["ROLL_AGENT_MACHINE"].as_str(), env["ROLL_AGENT_OWNER"].as_str()), (Some("https://api.roll.sh"), Some(machine.as_str()), Some("acme")));
    let token = env["ROLL_AGENT_TOKEN"].as_str().unwrap().to_string();
    assert!(token.starts_with("rlm_"));
    // Sealed until the backend had them, then dropped.
    let sealed: Option<String> = w.a.code.db.sql().query_row("SELECT sealed FROM agent_sandboxes WHERE id = ?", [&id], |r| r.get(0)).unwrap();
    assert_eq!(sealed, None);
    assert_eq!(w.machine(&machine).await.unwrap()["sandbox"]["status"], "running");

    // The token the sandbox got opens the relay link as its machine.
    let (_daemon, _) = Daemon::connect(&w.url, &token).await;
    assert_eq!(w.machine(&machine).await.unwrap()["online"], true);

    // Stop and start go to agent37 at once.
    let (s, stopped) = w.req("POST", &format!("/v1/code/agents/acme/sandboxes/{id}/stop"), None).await;
    assert_eq!((s, stopped["status"].as_str(), stopped["desired"].as_str()), (StatusCode::OK, Some("stopped"), Some("stopped")), "{stopped}");
    assert_eq!(w.mock.lock().instances.values().next().unwrap()["status"], "stopped");
    assert_eq!(w.machine(&machine).await.unwrap()["sandbox"]["status"], "stopped");
    let (s, started) = w.req("POST", &format!("/v1/code/agents/acme/sandboxes/{id}/start"), None).await;
    assert_eq!((s, started["status"].as_str()), (StatusCode::OK, Some("running")), "{started}");

    // Running time is metered, per month, at each look.
    let deadline = Instant::now() + Duration::from_secs(10);
    let usage = loop {
        let (s, usage) = w.req("GET", "/v1/code/agents/acme/sandboxes/usage", None).await;
        assert_eq!(s, StatusCode::OK, "{usage}");
        if usage["totals"]["running_ms"].as_i64().unwrap_or(0) >= 200 {
            break usage;
        }
        assert!(Instant::now() < deadline, "not metered: {usage}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let row = &usage["sandboxes"][0];
    assert_eq!((row["sandbox"].as_str(), row["size"].as_str(), row["monthly_cents"].as_u64()), (Some(id.as_str()), Some("standard"), Some(2000)), "{usage}");
    assert_eq!(usage["totals"]["running_ms"], row["running_ms"]);
    let (s, _) = w.req("GET", "/v1/code/agents/acme/sandboxes/usage?month=1999-01", None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = w.req("GET", "/v1/code/agents/acme/sandboxes/usage?month=January", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Delete: the instance, the machine (its link closes) and the sandbox are gone.
    let (s, _) = w.req("DELETE", &format!("/v1/code/agents/acme/sandboxes/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(w.mock.lock().instances.is_empty());
    assert!(w.machine(&machine).await.is_none());
    assert!(w.sandbox(&id).await.is_none());
    for (m, p) in [("DELETE", format!("/v1/code/agents/acme/sandboxes/{id}")), ("POST", format!("/v1/code/agents/acme/sandboxes/{id}/start"))] {
        assert_eq!(w.req(m, &p, None).await.0, StatusCode::NOT_FOUND, "{m} {p}");
    }
    // Its running time stays on the bill.
    let (_, usage) = w.req("GET", "/v1/code/agents/acme/sandboxes/usage", None).await;
    assert!(usage["totals"]["running_ms"].as_i64().unwrap() >= 200);
    // agent37 never saw anything but its own key on its own API.
    assert!(w.mock.lock().calls.iter().all(|c| c.contains("/v1/instances")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creates_are_retried_adopted_or_failed_and_limits_hold() {
    let w = world(|c| c.max_per_owner = 3).await;
    // Bad input first.
    for body in [json!({ "size": "huge" }), json!({ "env": { "ROLL_AGENT_TOKEN": "x" } }), json!({ "env": "x" }), json!({ "name": "" })] {
        let (s, v) = w.req("POST", "/v1/code/agents/acme/sandboxes", Some(body.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {v}");
    }

    // No capacity, then capacity: created on a later look, the error cleared.
    w.mock.lock().on_create = vec![OnCreate::NoCapacity, OnCreate::NoCapacity];
    let a = w.create(json!({ "name": "a" })).await["sandbox"]["id"].as_str().unwrap().to_string();
    let seen = w.until(&a, "a retried create", |s| s["status"] == "running").await;
    assert_eq!(seen["error"], Value::Null);
    assert!(w.mock.lock().creates.len() >= 3);

    // The answer of a create was lost: the instance is found by roll's id, not made twice.
    w.mock.lock().on_create = vec![OnCreate::LoseAnswer];
    let b = w.create(json!({ "name": "b" })).await["sandbox"]["id"].as_str().unwrap().to_string();
    w.until(&b, "an adopted instance", status_is("running")).await;
    let n = w.mock.lock().instances.values().filter(|i| i["metadata"]["roll_sandbox"] == b.as_str()).count();
    assert_eq!(n, 1);

    // A create agent37 refuses fails with its reason, and isn't tried again.
    w.mock.lock().on_create = vec![OnCreate::Broke];
    let c = w.create(json!({ "name": "c" })).await["sandbox"]["id"].as_str().unwrap().to_string();
    let failed = w.until(&c, "a failed create", status_is("failed")).await;
    assert_eq!(failed["error"], "agent37: insufficient_balance: mock insufficient_balance");
    let creates = w.mock.lock().creates.len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.mock.lock().creates.len(), creates);

    // At the limit (3, failed ones count until deleted).
    let (s, v) = w.req("POST", "/v1/code/agents/acme/sandboxes", Some(json!({}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // Deleting the failed one (agent37 never had it) frees a place.
    assert_eq!(w.req("DELETE", &format!("/v1/code/agents/acme/sandboxes/{c}"), None).await.0, StatusCode::NO_CONTENT);
    let d = w.create(json!({ "name": "d" })).await["sandbox"]["id"].as_str().unwrap().to_string();
    w.until(&d, "d running", status_is("running")).await;

    // Always on: an instance that stopped on its own is started again.
    let inst_a = w.mock.lock().instances.iter().find(|(_, i)| i["metadata"]["roll_sandbox"] == a.as_str()).map(|(k, _)| k.clone()).unwrap();
    w.mock.lock().instances.get_mut(&inst_a).unwrap()["status"] = json!("stopped");
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.mock.lock().instances[&inst_a]["status"] != "running" {
        assert!(Instant::now() < deadline, "never started again");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // agent37 losing an instance fails the sandbox.
    w.mock.lock().instances.remove(&inst_a);
    let lost = w.until(&a, "a lost instance", status_is("failed")).await;
    assert_eq!(lost["error"], "agent37 no longer has this sandbox");

    // Removing a sandbox's machine (from the machines list) deletes the sandbox.
    let bm = w.sandbox(&b).await.unwrap()["machine"].as_str().unwrap().to_string();
    assert_eq!(w.req("DELETE", &format!("/v1/code/agents/acme/machines/{bm}"), None).await.0, StatusCode::NO_CONTENT);
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.sandbox(&b).await.is_some() || w.mock.lock().instances.values().any(|i| i["metadata"]["roll_sandbox"] == b.as_str()) {
        assert!(Instant::now() < deadline, "the sandbox outlived its machine");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandboxes_need_a_backend_and_a_server_secret() {
    // Off: listing works, creating is a 503.
    let a = app_with(|c| c.admin_token = Some(ADMIN.into()));
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some(ADMIN)).await;
    let (s, v) = a.req("GET", "/v1/code/agents/acme/sandboxes", None, Some(ADMIN)).await;
    assert_eq!((s, &v["sandboxes"], &v["backend"]), (StatusCode::OK, &json!([]), &Value::Null));
    assert_eq!(a.req("POST", "/v1/code/agents/acme/sandboxes", Some(json!({})), Some(ADMIN)).await.0, StatusCode::SERVICE_UNAVAILABLE);
    // On without ROLL_SECRET_KEY: nothing to seal the token with.
    let (url, _mock) = agent37().await;
    let mut cfg = Agent37Config::new(KEY.into());
    cfg.url = url;
    let a = app_with(|c| {
        c.admin_token = Some(ADMIN.into());
        c.sandboxes.backend = Some(BackendConfig::Agent37(cfg));
    });
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some(ADMIN)).await;
    let (s, v) = a.req("POST", "/v1/code/agents/acme/sandboxes", Some(json!({})), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"].as_str().unwrap().contains("ROLL_SECRET_KEY"));
}

/// The agent37 client on its own: a wrong key is a clear, key-free error; huge answers are
/// refused rather than read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent37_client_is_bounded_and_keeps_its_key() {
    let (url, _m) = agent37().await;
    let mut cfg = Agent37Config::new("sk_live_wrong_key".into());
    cfg.url = url;
    let b = code_control::agents::sandbox::agent37::Agent37::new(cfg).unwrap();
    let e = b.status("inst000001").await.unwrap_err().to_string();
    assert!(e.contains("ROLL_AGENT37_API_KEY") && !e.contains("sk_live"), "{e}");

    // An endless answer.
    let router = Router::new().route("/v1/instances", get(|| async { "x".repeat(17 << 20) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let big = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut cfg = Agent37Config::new(KEY.into());
    cfg.url = big;
    let b = code_control::agents::sandbox::agent37::Agent37::new(cfg).unwrap();
    let e = b.find("sbx_x").await.unwrap_err().to_string();
    assert!(e.contains("more than"), "{e}");
    // Nothing listening: worth retrying.
    let mut cfg = Agent37Config::new(KEY.into());
    cfg.url = "http://127.0.0.1:1".into();
    let b = code_control::agents::sandbox::agent37::Agent37::new(cfg).unwrap();
    assert!(matches!(b.status("inst000001").await, Err(code_control::agents::sandbox::BackendError::Transient(_))));
}

// ── docker ──

fn docker_available() -> bool {
    std::process::Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success())
}

fn inspect(name: &str, format: &str) -> String {
    let out = std::process::Command::new("docker").args(["inspect", "-f", format, name]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The docker backend makes hardened containers: no new privileges, no capabilities, limits,
/// its own home volume, no host network or Docker socket, the env through a file that's gone
/// after; stop, start and delete (with the volume).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_docker_backend_runs_hardened_containers() {
    if !docker_available() {
        eprintln!("skipped: no Docker");
        return;
    }
    let d = Docker::new(DockerConfig { image: "busybox:1.37".into(), runtime: Runtime::Auto, command: vec!["sleep".into(), "3600".into()], ..Default::default() });
    let id = format!("sbx_test{}", std::process::id());
    let spec = SandboxSpec {
        id: id.clone(),
        name: "box".into(),
        size: default_sizes()[0].clone(),
        env: [("ROLL_AGENT_TOKEN".to_string(), "rlm_docker_test".to_string()), ("ANTHROPIC_API_KEY".to_string(), "sk-ant-x".to_string())].into(),
    };
    let name = d.create(&spec).await.unwrap();
    assert_eq!(name, format!("roll-sbx-test{}", std::process::id()));
    // Idempotent: a second create finds it.
    assert_eq!(d.create(&spec).await.unwrap(), name);
    assert_eq!(d.find(&id).await.unwrap().as_deref(), Some(name.as_str()));
    let deadline = Instant::now() + Duration::from_secs(30);
    while d.status(&name).await.unwrap() != BackendState::Running {
        assert!(Instant::now() < deadline, "never running");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(inspect(&name, "{{json .HostConfig.CapDrop}}"), r#"["ALL"]"#);
    assert_eq!(inspect(&name, "{{json .HostConfig.SecurityOpt}}"), r#"["no-new-privileges"]"#);
    assert_eq!(inspect(&name, "{{.HostConfig.Privileged}}"), "false");
    assert_eq!(inspect(&name, "{{.HostConfig.PidsLimit}}"), "1024");
    assert_eq!(inspect(&name, "{{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.HostConfig.NanoCpus}}"), format!("{0} {0} 2000000000", 4u64 << 30));
    assert_ne!(inspect(&name, "{{.HostConfig.NetworkMode}}"), "host");
    assert_eq!(inspect(&name, "{{.HostConfig.Init}}"), "true");
    let mounts = inspect(&name, "{{range .Mounts}}{{.Type}}:{{.Name}}:{{.Destination}} {{end}}");
    assert_eq!(mounts, format!("volume:{name}-home:/home/roll"));
    assert!(!inspect(&name, "{{json .Mounts}}").contains("docker.sock"));
    let env = inspect(&name, "{{json .Config.Env}}");
    assert!(env.contains("ROLL_AGENT_TOKEN=rlm_docker_test") && env.contains("ANTHROPIC_API_KEY=sk-ant-x"), "{env}");
    // The env file is gone.
    let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("roll-sandbox-")).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    d.stop(&name).await.unwrap();
    assert_eq!(d.status(&name).await.unwrap(), BackendState::Stopped);
    d.start(&name).await.unwrap();
    assert_eq!(d.status(&name).await.unwrap(), BackendState::Running);
    d.delete(&name).await.unwrap();
    assert_eq!(d.status(&name).await.unwrap(), BackendState::Gone);
    assert_eq!(d.find(&id).await.unwrap(), None);
    let vol = std::process::Command::new("docker").args(["volume", "inspect", &format!("{name}-home")]).output().unwrap();
    assert!(!vol.status.success(), "the home volume is removed too");
    // Deleting again is fine.
    d.delete(&name).await.unwrap();
}

/// The real sandbox image (`deploy/agent/Dockerfile`), started with the env a sandbox gets and the
/// docker backend's hardening: `roll agent up` takes the machine token from its environment and
/// comes online as that machine, with both providers installed. Opt-in (the image takes a while to
/// build): `docker build -f deploy/agent/Dockerfile -t roll-agent-sandbox:dev .` and set
/// `ROLL_SANDBOX_TEST_IMAGE=roll-agent-sandbox:dev`. The container uses the host's network here
/// only so it reaches this test's server on any host's firewall; the backend never does that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sandbox_image_comes_online_with_its_token() {
    let Some(image) = std::env::var("ROLL_SANDBOX_TEST_IMAGE").ok().filter(|i| !i.is_empty()) else {
        eprintln!("skipped: set ROLL_SANDBOX_TEST_IMAGE to the sandbox image");
        return;
    };
    assert!(docker_available(), "ROLL_SANDBOX_TEST_IMAGE needs Docker");
    let a = app_with(|c| c.admin_token = Some(ADMIN.into()));
    let url = a.serve().await;
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some(ADMIN)).await;
    let (s, minted) = a.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": "image" })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED, "{minted}");
    let (machine, token) = (minted["machine"]["id"].as_str().unwrap().to_string(), minted["token"].as_str().unwrap().to_string());
    let name = format!("roll-sbx-imagetest{}", std::process::id());
    let env = [("ROLL_API_URL", url.as_str()), ("ROLL_AGENT_TOKEN", &token), ("ROLL_AGENT_MACHINE", &machine), ("ROLL_AGENT_OWNER", "acme"), ("ROLL_AGENT_NAME", "image")];
    let mut cmd = std::process::Command::new("docker");
    cmd.args(["run", "-d", "--name", &name, "--network", "host", "--init", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--pids-limit", "1024"]);
    for (k, v) in env {
        cmd.env(k, v).args(["-e", k]);
    }
    let out = cmd.arg(&image).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let deadline = Instant::now() + Duration::from_secs(60);
    let online = loop {
        let (_, ms) = a.req("GET", "/v1/code/agents/acme/machines", None, Some(ADMIN)).await;
        let m = ms.as_array().unwrap().iter().find(|m| m["id"] == machine.as_str()).cloned().unwrap();
        if m["online"] == true {
            break m;
        }
        if Instant::now() > deadline {
            let logs = std::process::Command::new("docker").args(["logs", "--tail", "50", &name]).output().unwrap();
            let _ = std::process::Command::new("docker").args(["rm", "-f", &name]).output();
            panic!("never online: {m}\n{}{}", String::from_utf8_lossy(&logs.stdout), String::from_utf8_lossy(&logs.stderr));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let _ = std::process::Command::new("docker").args(["rm", "-f", &name]).output();
    assert_eq!((online["info"]["home"].as_str(), online["info"]["hostname"].is_string()), (Some("/home/roll"), true), "{online}");
    let providers = online["info"]["providers"].as_array().unwrap();
    for p in ["codex", "claude"] {
        let info = providers.iter().find(|x| x["id"] == p).unwrap_or_else(|| panic!("{p}: {online}"));
        assert_eq!(info["installed"], true, "{info}");
    }
}

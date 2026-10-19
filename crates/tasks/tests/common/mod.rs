//! Test harness: a tasks node (engine + worker + API) on a fresh store, programmable HTTP
//! receivers, and a mock SDK app that speaks the workflow step protocol.
#![allow(dead_code)]

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tasks::model::now_ms;
use tasks::store::Store;
use tasks::{Config, Tasks, Worker};

pub fn fast_config() -> Config {
    Config {
        poll_ms: 25,
        heartbeat_ms: 150,
        node_ttl_ms: 700,
        lease_ms: 1_500,
        backoff: tasks::model::Backoff { strategy: tasks::model::Strategy::Exponential, base_ms: 50, max_ms: 400, jitter: 0.0 },
        default_timeout_ms: 5_000,
        run_timeout_ms: 5_000,
        max_park_ms: 1_000,
        drain_ms: 3_000,
        ..Config::default()
    }
}

/// The store under test: SQLite by default; the log store when `ROLL_TASKS_E2E_LOG` is a log URL
/// (`memory://`, `s3://bucket/prefix` …); DynamoDB when `ROLL_TASKS_E2E_DYNAMO` is an endpoint.
pub async fn make_store(dir: &std::path::Path) -> Arc<dyn Store> {
    if let Ok(url) = std::env::var("ROLL_TASKS_E2E_LOG") {
        let (objects, prefix) = tasks::store::log::objects_from_url(&url).expect("log url");
        let prefix = format!("{prefix}/e2e-{}", tasks::ids::ulid());
        let mut c = tasks::store::log::LogConfig::new(objects, prefix, dir.join("log"));
        c.shards = 4;
        // The suites time delays in hundreds of ms; the default commit window (100 ms, a cost
        // cap) would eat into them. The cell tests run with the defaults.
        c.commit_ms = 5;
        c.relaxed_ms = 200;
        return tasks::store::log::LogStore::open(c).await.expect("log store");
    }
    #[cfg(feature = "dynamo")]
    if let Ok(endpoint) = std::env::var("ROLL_TASKS_E2E_DYNAMO") {
        let table = format!("roll-tasks-e2e-{}", tasks::ids::ulid());
        let store = tasks::store::dynamo::DynamoStore::connect(tasks::store::dynamo::DynamoConfig {
            table,
            endpoint: Some(endpoint),
            region: Some("us-east-1".into()),
            shards: 4,
            bucket: std::env::var("ROLL_TASKS_E2E_S3_BUCKET").ok(),
            s3_endpoint: std::env::var("ROLL_TASKS_E2E_S3_ENDPOINT").ok(),
            inline_max: 2048,
            create_table: true,
        })
        .await
        .expect("dynamo store");
        return Arc::new(store);
    }
    Arc::new(tasks::store::sqlite::SqliteStore::open(&dir.join("tasks.db"), 4).unwrap())
}

pub struct Node {
    pub tasks: Arc<Tasks>,
    pub worker: Option<Worker>,
}

pub struct Env {
    pub store: Arc<dyn Store>,
    pub tasks: Arc<Tasks>,
    pub api: String,
    pub http: reqwest::Client,
    pub token: Option<String>,
    worker: Mutex<Option<Worker>>,
    _dir: tempfile::TempDir,
}

pub async fn env() -> Env {
    env_with(|_| {}).await
}

pub async fn env_with(f: impl FnOnce(&mut Config)) -> Env {
    let _ = tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into())).with_test_writer().try_init();
    let dir = tempfile::tempdir().unwrap();
    let store = make_store(dir.path()).await;
    let mut config = fast_config();
    f(&mut config);
    let token = config.token.clone();
    let tasks = Tasks::new(config, store.clone()).await.unwrap();
    let worker = tasks::start(tasks.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let router = tasks::api::router(tasks.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Env { store, tasks, api, http: reqwest::Client::new(), token, worker: Mutex::new(Some(worker)), _dir: dir }
}

impl Env {
    /// Another node on the same store.
    pub async fn node(&self, f: impl FnOnce(&mut Config)) -> Node {
        let mut config = fast_config();
        f(&mut config);
        let tasks = Tasks::new(config, self.store.clone()).await.unwrap();
        let worker = Some(tasks::start(tasks.clone()));
        Node { tasks, worker }
    }

    pub fn take_worker(&self) -> Option<Worker> {
        self.worker.lock().unwrap().take()
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, format!("{}/v1/tasks{path}", self.api));
        if let Some(t) = &self.token {
            r = r.bearer_auth(t);
        }
        r
    }

    pub async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = self.req(method, path);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
        let text = resp.text().await.unwrap();
        (status, if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) })
    }

    pub async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(Method::POST, path, Some(body)).await;
        assert!(s.is_success(), "POST {path}: {s} {v}");
        v
    }

    pub async fn put(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(Method::PUT, path, Some(body)).await;
        assert!(s.is_success(), "PUT {path}: {s} {v}");
        v
    }

    pub async fn patch(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(Method::PATCH, path, Some(body)).await;
        assert!(s.is_success(), "PATCH {path}: {s} {v}");
        v
    }

    pub async fn get(&self, path: &str) -> Value {
        let (s, v) = self.call(Method::GET, path, None).await;
        assert!(s.is_success(), "GET {path}: {s} {v}");
        v
    }

    pub async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::DELETE, path, None).await
    }

    /// Publish and return the first message id.
    pub async fn publish(&self, body: Value) -> String {
        let v = self.post("/publish", body).await;
        v["message_id"].as_str().unwrap().to_string()
    }

    pub async fn message(&self, id: &str) -> Value {
        self.get(&format!("/messages/{id}")).await
    }

    /// Poll a message until its state is `state`.
    pub async fn wait_state(&self, id: &str, state: &str, timeout_ms: u64) -> Value {
        let deadline = now_ms() + timeout_ms as i64;
        loop {
            let m = self.message(id).await;
            if m["state"] == state {
                return m;
            }
            assert!(now_ms() < deadline, "message {id} never became {state}: {m:#}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn wait_run(&self, id: &str, timeout_ms: u64) -> Value {
        let deadline = now_ms() + timeout_ms as i64;
        loop {
            let r = self.get(&format!("/runs/{id}")).await;
            if ["completed", "failed", "canceled"].contains(&r["status"].as_str().unwrap_or("")) {
                return r;
            }
            assert!(now_ms() < deadline, "run {id} didn't finish: {r:#}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Runs of a function, newest first.
    pub async fn runs(&self, function: &str) -> Vec<Value> {
        self.get(&format!("/runs?function={function}&limit=200")).await["items"].as_array().unwrap().clone()
    }

    pub async fn shutdown(&self) {
        if let Some(w) = self.take_worker() {
            w.shutdown().await;
        }
    }
}

pub async fn eventually<F, Fut>(timeout_ms: u64, what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = now_ms() + timeout_ms as i64;
    while !f().await {
        assert!(now_ms() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// --- receivers ---

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub query: Option<String>,
    pub method: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub at: i64,
}

impl Hit {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(String::from)
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    /// Fail this many requests first.
    pub fail_times: usize,
    pub fail_status: u16,
    pub status: u16,
    pub delay_ms: u64,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Default for Rule {
    fn default() -> Self {
        Self { fail_times: 0, fail_status: 500, status: 200, delay_ms: 0, headers: vec![], body: "ok".into() }
    }
}

#[derive(Default)]
struct RecvState {
    hits: Mutex<Vec<Hit>>,
    rules: Mutex<HashMap<String, Rule>>,
    served: Mutex<HashMap<String, usize>>,
    inflight: Mutex<HashMap<String, usize>>,
    max_inflight: Mutex<HashMap<String, usize>>,
}

#[derive(Clone)]
pub struct Receiver {
    pub base: String,
    state: Arc<RecvState>,
}

impl Receiver {
    pub async fn start() -> Receiver {
        let state = Arc::new(RecvState::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(receive).with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Receiver { base, state }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn on(&self, path: &str, rule: Rule) {
        self.state.rules.lock().unwrap().insert(path.into(), rule);
    }

    pub fn hits(&self, path: &str) -> Vec<Hit> {
        self.state.hits.lock().unwrap().iter().filter(|h| h.path == path).cloned().collect()
    }

    pub fn all_hits(&self) -> Vec<Hit> {
        self.state.hits.lock().unwrap().clone()
    }

    pub fn max_inflight(&self, path: &str) -> usize {
        self.state.max_inflight.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    pub async fn wait(&self, path: &str, n: usize, timeout_ms: u64) -> Vec<Hit> {
        let deadline = now_ms() + timeout_ms as i64;
        loop {
            let hits = self.hits(path);
            if hits.len() >= n {
                return hits;
            }
            assert!(now_ms() < deadline, "{path}: wanted {n} requests, got {}", hits.len());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn receive(State(s): State<Arc<RecvState>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let path = uri.path().to_string();
    s.hits.lock().unwrap().push(Hit {
        path: path.clone(),
        query: uri.query().map(String::from),
        method: method.to_string(),
        headers,
        body: body.to_vec(),
        at: now_ms(),
    });
    let rule = s.rules.lock().unwrap().get(&path).cloned().unwrap_or_default();
    let n = {
        let mut served = s.served.lock().unwrap();
        let n = served.entry(path.clone()).or_insert(0);
        *n += 1;
        *n
    };
    {
        let mut inflight = s.inflight.lock().unwrap();
        let cur = inflight.entry(path.clone()).or_insert(0);
        *cur += 1;
        let mut max = s.max_inflight.lock().unwrap();
        let m = max.entry(path.clone()).or_insert(0);
        *m = (*m).max(*cur);
    }
    if rule.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(rule.delay_ms)).await;
    }
    *s.inflight.lock().unwrap().get_mut(&path).unwrap() -= 1;
    let status = if n <= rule.fail_times { rule.fail_status } else { rule.status };
    let mut resp = (StatusCode::from_u16(status).unwrap(), rule.body.clone()).into_response();
    for (k, v) in &rule.headers {
        resp.headers_mut().insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
    }
    resp
}

// --- a mock SDK app ---

pub type FnBody = Arc<dyn Fn(&mut Ctx) -> Result<Value, Stop> + Send + Sync>;

#[derive(Default)]
pub struct AppState {
    functions: Mutex<Vec<(Value, FnBody)>>,
    pub calls: Mutex<Vec<(String, Value)>>,
    effects: Mutex<HashMap<String, usize>>,
    id: Mutex<String>,
}

pub struct MockApp {
    pub url: String,
    pub state: Arc<AppState>,
}

pub enum Stop {
    Ops(Vec<Value>),
    Error { message: String, retriable: bool },
}

pub type StepFn = Box<dyn FnOnce() -> Value>;

pub struct Ctx {
    pub event: Value,
    pub events: Vec<Value>,
    pub run_id: String,
    pub attempt: u64,
    steps: serde_json::Map<String, Value>,
    app: Arc<AppState>,
}

impl Ctx {
    /// Count a side effect (to check steps run exactly once).
    pub fn effect(&self, name: &str) {
        *self.app.effects.lock().unwrap().entry(name.into()).or_insert(0) += 1;
    }

    pub fn memo(&self, id: &str) -> Option<Result<Value, Value>> {
        let s = self.steps.get(id)?;
        Some(match s.get("error") {
            Some(e) if !e.is_null() => Err(e.clone()),
            _ => Ok(s.get("data").cloned().unwrap_or(Value::Null)),
        })
    }

    /// A step whose failure the function sees (Err) once retries are used up.
    pub fn try_run(&mut self, id: &str, f: impl FnOnce() -> Result<Value, String>) -> Result<Result<Value, Value>, Stop> {
        if let Some(m) = self.memo(id) {
            return Ok(m);
        }
        Err(Stop::Ops(vec![match f() {
            Ok(v) => json!({ "op": "run", "id": id, "data": v }),
            Err(e) => json!({ "op": "run", "id": id, "error": { "name": "Error", "message": e } }),
        }]))
    }

    pub fn run(&mut self, id: &str, f: impl FnOnce() -> Result<Value, String>) -> Result<Value, Stop> {
        match self.try_run(id, f)? {
            Ok(v) => Ok(v),
            Err(e) => Err(Stop::Error { message: format!("step {id} failed: {e}"), retriable: false }),
        }
    }

    /// Several steps at once, executed in one call.
    pub fn parallel(&mut self, steps: Vec<(&str, StepFn)>) -> Result<Vec<Value>, Stop> {
        let mut ops = Vec::new();
        let mut out = Vec::new();
        for (id, f) in steps {
            match self.memo(id) {
                Some(Ok(v)) => out.push(v),
                Some(Err(e)) => return Err(Stop::Error { message: e.to_string(), retriable: false }),
                None => ops.push(json!({ "op": "run", "id": id, "data": f() })),
            }
        }
        if ops.is_empty() { Ok(out) } else { Err(Stop::Ops(ops)) }
    }

    pub fn sleep(&mut self, id: &str, ms: i64) -> Result<(), Stop> {
        match self.memo(id) {
            Some(_) => Ok(()),
            None => Err(Stop::Ops(vec![json!({ "op": "sleep", "id": id, "duration_ms": ms })])),
        }
    }

    pub fn wait_for_event(&mut self, id: &str, event: &str, matching: Option<&str>, cond: Option<&str>, timeout_ms: i64) -> Result<Value, Stop> {
        match self.memo(id) {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(Stop::Error { message: e.to_string(), retriable: false }),
            None => Err(Stop::Ops(vec![
                json!({ "op": "wait_for_event", "id": id, "event": event, "match": matching, "if": cond, "timeout_ms": timeout_ms }),
            ])),
        }
    }

    pub fn invoke(&mut self, id: &str, function: &str, data: Value, timeout_ms: Option<i64>) -> Result<Result<Value, Value>, Stop> {
        match self.memo(id) {
            Some(r) => Ok(r),
            None => Err(Stop::Ops(vec![json!({ "op": "invoke", "id": id, "function": function, "data": data, "timeout_ms": timeout_ms })])),
        }
    }

    pub fn send_event(&mut self, id: &str, events: Value) -> Result<Value, Stop> {
        match self.memo(id) {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(Stop::Error { message: e.to_string(), retriable: false }),
            None => Err(Stop::Ops(vec![json!({ "op": "send_event", "id": id, "events": events })])),
        }
    }
}

impl MockApp {
    pub async fn start(id: &str) -> MockApp {
        let state = Arc::new(AppState::default());
        *state.id.lock().unwrap() = id.into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/roll", listener.local_addr().unwrap());
        let app = Router::new().fallback(app_handler).with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockApp { url, state }
    }

    pub fn function(&self, def: Value, body: impl Fn(&mut Ctx) -> Result<Value, Stop> + Send + Sync + 'static) {
        self.state.functions.lock().unwrap().push((def, Arc::new(body)));
    }

    pub fn effects(&self, name: &str) -> usize {
        self.state.effects.lock().unwrap().get(name).copied().unwrap_or(0)
    }

    pub fn calls(&self, function: &str) -> Vec<Value> {
        self.state.calls.lock().unwrap().iter().filter(|(f, _)| f == function).map(|(_, v)| v.clone()).collect()
    }

    /// Register the app (the engine fetches the function list from the app's URL).
    pub async fn sync(&self, env: &Env) -> Value {
        env.post("/apps", json!({ "url": self.url })).await
    }
}

async fn app_handler(State(s): State<Arc<AppState>>, method: Method, Query(q): Query<HashMap<String, String>>, body: Bytes) -> Response {
    if method == Method::GET {
        let defs: Vec<Value> = s.functions.lock().unwrap().iter().map(|(d, _)| d.clone()).collect();
        return axum::Json(json!({ "id": *s.id.lock().unwrap(), "functions": defs, "sdk": "mock" })).into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let fid = q.get("fnId").cloned().unwrap_or_default();
    s.calls.lock().unwrap().push((fid.clone(), req.clone()));
    let body = s.functions.lock().unwrap().iter().find(|(d, _)| d["id"] == fid.as_str()).map(|(_, b)| b.clone());
    let Some(body) = body else {
        return (StatusCode::NOT_FOUND, "no such function").into_response();
    };
    let mut ctx = Ctx {
        event: req["event"].clone(),
        events: req["events"].as_array().cloned().unwrap_or_default(),
        run_id: req["run_id"].as_str().unwrap_or_default().into(),
        attempt: req["attempt"].as_u64().unwrap_or(0),
        steps: req["steps"].as_object().cloned().unwrap_or_default(),
        app: s.clone(),
    };
    let reply = match body(&mut ctx) {
        Ok(output) => json!({ "status": "done", "output": output }),
        Err(Stop::Ops(ops)) => json!({ "status": "steps", "steps": ops }),
        Err(Stop::Error { message, retriable }) => {
            json!({ "status": "error", "error": { "name": "Error", "message": message }, "retriable": retriable })
        }
    };
    axum::Json(reply).into_response()
}

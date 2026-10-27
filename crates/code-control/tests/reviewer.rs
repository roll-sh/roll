//! roll's own reviewer end to end, against a mock Anthropic API (no test calls a real one):
//! automatic and requested reviews, incremental re-reviews and dedupe, `auto` mode and deep
//! context, `fast` streaming, `@roll` commands, learnings, metering and pricing, caps,
//! bring-your-own keys, retries and give-ups, restart recovery, blockers and code search.

mod common;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use code_control::ai::Hosted;
use code_review::provider::RetryPolicy;
use common::{App, app_with};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const ROOT: &str = "root-token";
const SECRET: &str = "0123456789abcdef0123456789abcdef-test";

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Value,
}

impl Seen {
    fn user(&self) -> &str {
        self.body["messages"][0]["content"][0]["text"].as_str().unwrap_or("")
    }
    fn system(&self) -> String {
        self.body["system"].as_array().map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default()
    }
}

type Script = Arc<dyn Fn(&Seen, u32) -> Option<(u16, Value)> + Send + Sync>;

struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    script: Arc<Mutex<Option<Script>>>,
    delay_ms: Arc<AtomicU32>,
}

impl Mock {
    fn messages(&self) -> Vec<Seen> {
        self.seen.lock().iter().filter(|s| s.path == "/v1/messages").cloned().collect()
    }
    fn set(&self, s: impl Fn(&Seen, u32) -> Option<(u16, Value)> + Send + Sync + 'static) {
        *self.script.lock() = Some(Arc::new(s));
    }
}

fn reply(json: Value, usage: Value) -> (u16, Value) {
    (200, json!({ "model": "claude-opus-5-5", "stop_reason": "end_turn", "content": [{"type": "text", "text": json.to_string()}], "usage": usage }))
}

/// A review of every file in the request: one bug on its first added line, with a suggestion.
fn default_answer(s: &Seen) -> (u16, Value) {
    let user = s.user();
    if user.contains("checking another reviewer's findings") {
        return reply(json!({"results": [{"index": 0, "confirmed": true, "reason": "real"}]}), json!({}));
    }
    if user.contains("how deep a code review") {
        return reply(json!({"mode": "deep", "reasons": ["model says deep"]}), json!({"input_tokens": 20}));
    }
    if user.contains("# Thread") {
        let sugg = if user.contains("Write a fix") { json!("let fixed = true;") } else { Value::Null };
        return reply(json!({"reply": "Here is why: it can be empty.", "suggestion": sugg}), json!({"input_tokens": 30, "output_tokens": 10}));
    }
    let mut findings = Vec::new();
    let mut files = Vec::new();
    let mut cur: Option<String> = None;
    for line in user.lines() {
        if let Some(h) = line.strip_prefix("## ")
            && h.contains(" (")
        {
            cur = Some(h.split(" (").next().unwrap().to_string());
            files.push(json!({"path": cur.clone().unwrap(), "summary": "edited"}));
            continue;
        }
        if let (Some(p), Some(rest)) = (&cur, line.strip_prefix("+ "))
            && !findings.iter().any(|f: &Value| f["path"] == p.as_str())
        {
            let n: u64 = rest.split('|').next().unwrap().trim().parse().unwrap();
            findings.push(json!({"path": p, "side": "new", "start_line": n, "line": n, "severity": "bug", "title": format!("Unchecked input in {p}"),
                "body": "This panics when the input is empty.", "suggestion": "let fixed = true;", "confidence": 0.95}));
        }
    }
    reply(
        json!({"walkthrough": "Adds things.", "files": files, "diagram": null, "findings": findings, "notes": []}),
        json!({"input_tokens": 1000, "output_tokens": 200, "cache_creation_input_tokens": 3000, "cache_read_input_tokens": 0}),
    )
}

async fn mock() -> Mock {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let script: Arc<Mutex<Option<Script>>> = Arc::default();
    let delay = Arc::new(AtomicU32::new(0));
    let (s2, sc2, d2) = (seen.clone(), script.clone(), delay.clone());
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
        let (seen, script, delay) = (s2.clone(), sc2.clone(), d2.clone());
        async move {
            let s = Seen { path: uri.path().to_string(), headers, body: serde_json::from_slice(&body).unwrap_or(Value::Null) };
            let n = {
                let mut v = seen.lock();
                v.push(s.clone());
                v.iter().filter(|x| x.path == s.path).count() as u32
            };
            if s.path.ends_with("/count_tokens") {
                return axum::Json(json!({"input_tokens": 500})).into_response();
            }
            // `u32::MAX`: held until the test sets another delay.
            while delay.load(Ordering::SeqCst) == u32::MAX {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let d = delay.load(Ordering::SeqCst);
            if d > 0 {
                tokio::time::sleep(Duration::from_millis(d as u64)).await;
            }
            let custom = script.lock().clone();
            let (status, v) = custom.and_then(|f| f(&s, n)).unwrap_or_else(|| default_answer(&s));
            (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response()
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Mock { url, seen, script, delay_ms: delay }
}

struct T {
    a: App,
    m: Mock,
    acme: String,
    mallory: String,
}

fn fast_retry() -> RetryPolicy {
    RetryPolicy { attempts: 2, base: Duration::from_millis(5), max_delay: Duration::from_millis(50), timeout: Duration::from_secs(10) }
}

async fn call(a: &App, method: &str, path: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = a.send(req).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
    (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) })
}

async fn ok(a: &App, method: &str, path: &str, body: Option<Value>) -> Value {
    let (s, v) = call(a, method, path, body, Some(ROOT)).await;
    assert!(s.is_success(), "{method} {path}: {s} {v}");
    v
}

/// The reviewer's configuration against the mock at `url` (besides the admin token).
fn base_config(c: &mut code_control::Config, url: &str) {
    c.review.hosted = Some(Hosted { provider: "anthropic".into(), api_key: "sk-hosted".into(), base_url: Some(url.to_string()), model: None, secondary_model: None });
    c.review.retry = fast_retry();
    c.review.retry_ms = 50;
    c.review.secret_key = Some(SECRET.into());
    c.review.anthropic_url = url.to_string();
}

/// A second node on `t`'s databases (control.db and the store), with connections and a
/// reviewer of its own, configured like the first and then by `f`.
fn second_node(t: &T, f: impl FnOnce(&mut code_control::Config)) -> App {
    let code = common::open_in(t.a.dir.path(), |c| {
        c.admin_token = t.a.code.config.admin_token.clone();
        base_config(c, &t.m.url);
        f(c);
    });
    code.start_reviewer();
    App { router: code_control::api::router(code.clone()), code, dir: tempfile::tempdir().unwrap() }
}

async fn setup_with(visibility: &str, f: impl FnOnce(&mut code_control::Config, &str)) -> T {
    let m = mock().await;
    let url = m.url.clone();
    let a = app_with(|c| {
        c.admin_token = Some(ROOT.into());
        base_config(c, &url);
        f(c, &url);
    });
    a.code.start_reviewer();
    for (name, kind) in [("acme", "org"), ("mallory", "user")] {
        ok(&a, "POST", "/v1/code/owners", Some(json!({ "name": name, "kind": kind }))).await;
    }
    let acme = ok(&a, "POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "deploy" }))).await["token"].as_str().unwrap().to_string();
    let mallory = ok(&a, "POST", "/v1/code/owners/mallory/tokens", Some(json!({ "name": "laptop" }))).await["token"].as_str().unwrap().to_string();
    ok(&a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api", "visibility": visibility }))).await;
    let files = json!({
        "AGENTS.md": { "content": "Always bound allocations by bytes present.\n" },
        "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    input.len()\n}\n" },
        "src/server.rs": { "content": "use crate::parse::parse_header;\n\npub fn handle(req: &str) {\n    let n = parse_header(req);\n}\n" },
        "tests/parse.rs": { "content": "#[test]\nfn parses() {\n    assert_eq!(parse_header(\"ab\"), 2);\n}\n" },
        "Cargo.lock": { "content": "# lock\n" },
    });
    ok(&a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "base", "files": files }))).await;
    T { a, m, acme, mallory }
}

async fn setup(f: impl FnOnce(&mut code_control::Config, &str)) -> T {
    setup_with("private", f).await
}

async fn enable(t: &T, settings: Value) -> Value {
    let mut s = json!({ "enabled": true });
    for (k, v) in settings.as_object().unwrap() {
        s[k] = v.clone();
    }
    ok(&t.a, "PATCH", "/v1/code/repos/acme/api/reviewer", Some(s)).await
}

async fn change(t: &T, title: &str, files: Value) -> Value {
    ok(&t.a, "POST", "/v1/code/repos/acme/api/changes", Some(json!({ "title": title, "message": title, "author": "alice", "base": "main", "files": files }))).await
}

/// Wait until the change has `n` roll reviews; return them.
async fn roll_reviews(t: &T, change: &str, n: usize) -> Vec<Value> {
    for _ in 0..400 {
        let v = ok(&t.a, "GET", &format!("/v1/code/changes/{change}/reviews"), None).await;
        let r: Vec<Value> = v.as_array().unwrap().iter().filter(|x| x["reviewer"]["kind"] == "roll").cloned().collect();
        let done = r.iter().filter(|x| !x["ai"].is_null() || x["body"].as_str().unwrap_or("").contains("couldn't review") || x["body"].as_str().unwrap_or("").contains("paused for this month")).count();
        if done >= n {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no {n} roll reviews on {change}: {:?}", ok(&t.a, "GET", &format!("/v1/code/changes/{change}/ai-jobs"), None).await);
}

async fn jobs_idle(t: &T, change: &str) -> Vec<Value> {
    jobs_idle_n(t, change, 0).await
}

/// Wait until the change has at least `n` reviewer jobs and none is waiting or running.
async fn jobs_idle_n(t: &T, change: &str, n: usize) -> Vec<Value> {
    for _ in 0..400 {
        let v = ok(&t.a, "GET", &format!("/v1/code/changes/{change}/ai-jobs"), None).await;
        let jobs = v.as_array().unwrap().clone();
        if jobs.len() >= n && jobs.iter().all(|j| j["state"] != "queued" && j["state"] != "running") {
            return jobs;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("jobs still running on {change}");
}

async fn threads(t: &T, change: &str) -> Vec<Value> {
    ok(&t.a, "GET", &format!("/v1/code/changes/{change}/threads"), None).await.as_array().unwrap().clone()
}

#[tokio::test]
async fn reviews_a_change_automatically_with_the_right_request() {
    let t = setup(|_, _| {}).await;
    let v = enable(&t, json!({ "required": true, "prompt": "Care about tenancy.", "mode": "standard" })).await;
    assert_eq!(v["effective"]["enabled"], true);
    assert_eq!(v["provider"]["source"], "hosted");
    assert_eq!(v["provider"]["model"], "claude-opus-5-5");
    t.m.delay_ms.store(400, Ordering::SeqCst);
    let c = change(&t, "Add limit", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    // A required reviewer: the change waits for roll.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let now = ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await;
    assert!(now["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "review_pending"), "{}", now["blockers"]);
    t.m.delay_ms.store(0, Ordering::SeqCst);
    let r = roll_reviews(&t, &id, 1).await;
    let rv = &r[0];
    assert_eq!(rv["verdict"], "request_changes");
    assert!(rv["body"].as_str().unwrap().starts_with("**roll review** · standard mode"));
    assert_eq!(rv["ai"]["mode"], "standard");
    assert_eq!(rv["ai"]["chosen_by"], "settings");
    assert_eq!(rv["ai"]["key"], "hosted");
    assert_eq!(rv["ai"]["model"], "claude-opus-5-5");
    assert_eq!(rv["ai"]["tokens"]["cache_write"], 3000);
    let th = threads(&t, &id).await;
    assert_eq!(th.len(), 1);
    assert_eq!(th[0]["path"], "src/parse.rs");
    assert_eq!(th[0]["line"], 2);
    assert!(th[0]["comments"][0]["suggestion"].as_str().unwrap().contains("let fixed = true;"));
    let after = ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await;
    assert!(!after["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "review_pending"));
    assert!(after["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "changes_requested"), "{}", after["blockers"]);
    // The request that went out.
    let msgs = t.m.messages();
    assert_eq!(msgs.len(), 1);
    let s = &msgs[0];
    assert_eq!(s.headers["x-api-key"], "sk-hosted");
    assert_eq!(s.headers["anthropic-version"], "2023-06-01");
    assert_eq!(s.headers["anthropic-beta"], "server-side-fallback-2026-07-01");
    assert_eq!(s.body["fallbacks"], "default");
    assert_eq!(s.body["output_config"]["effort"], "medium");
    assert!(s.body.get("thinking").is_none() && s.body.get("tool_choice").is_none());
    assert_eq!(s.body["system"][1]["cache_control"]["type"], "ephemeral");
    let sys = s.system();
    assert!(sys.contains("Care about tenancy.") && sys.contains("## AGENTS.md") && sys.contains("Always bound allocations"));
    assert!(s.user().contains("Title: Add limit"));
    // Metering (a private repo: charged).
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["reviews"], 1);
    assert_eq!(u["totals"]["price_usd"], 0.25);
    assert_eq!(u["by_mode"]["standard"]["reviews"], 1);
    assert_eq!(u["by_key"]["hosted"]["count"], 1);
    let rec = &u["recent"][0];
    assert_eq!(rec["files_reviewed"], 1);
    assert_eq!(rec["human_author"], true);
    // 1000×4 + 200×20 + 3000×4×1.25 per million.
    assert!((rec["provider_cost_usd"].as_f64().unwrap() - 0.023).abs() < 1e-9, "{rec}");
    // Others can't see the usage.
    let (s, _) = call(&t.a, "GET", "/v1/code/owners/acme/usage", None, Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&t.a, "GET", "/v1/code/owners/acme/usage", None, Some(&t.acme)).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn incremental_re_reviews_dedupe_and_skips() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "standard", "ignore": ["*.lock"] })).await;
    let c = change(&t, "Two files", json!({
        "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n\n}\n" },
        "src/server.rs": { "content": "use crate::parse::parse_header;\n\npub fn handle(req: &str) {\n    let n = parse_header(req);\n    drop(n);\n}\n" },
    }))
    .await;
    let id = c["id"].as_str().unwrap().to_string();
    roll_reviews(&t, &id, 1).await;
    assert_eq!(threads(&t, &id).await.len(), 2);
    // Revise only server.rs (and a lock file): the re-review sees only server.rs, and repeats nothing.
    let head = c["head"].as_str().unwrap().to_string();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": head, "message": "more", "files": {
        "src/server.rs": { "content": "use crate::parse::parse_header;\n\npub fn handle(req: &str) {\n    let n = parse_header(req);\n    drop(n);\n    log(req);\n}\n" },
        "Cargo.lock": { "content": "# lock 2\n" },
    }}))).await;
    let r = roll_reviews(&t, &id, 2).await;
    let second = r.last().unwrap();
    assert!(second["body"].as_str().unwrap().contains("changes since the last review"));
    assert_eq!(second["ai"]["incremental"], true);
    let user = t.m.messages().last().unwrap().user().to_string();
    assert!(user.contains("## src/server.rs") && !user.contains("## src/parse.rs") && !user.contains("Cargo.lock"), "{user}");
    assert!(user.contains("only what changed since then"));
    // The same finding two lines down repeats the open thread: dropped, not posted again.
    let th = threads(&t, &id).await;
    assert_eq!(th.iter().filter(|x| x["path"] == "src/server.rs").count(), 1, "{th:?}");
    assert_eq!(second["ai"]["details"]["dropped"]["duplicates"], 1);
    // Only an ignored file changed: no review.
    let head = ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await["head"].as_str().unwrap().to_string();
    let n = jobs_idle(&t, &id).await.len();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": head, "message": "lock", "files": { "Cargo.lock": { "content": "# lock 3\n" } } }))).await;
    let jobs = jobs_idle_n(&t, &id, n + 1).await;
    assert_eq!(jobs.last().unwrap()["state"], "skipped");
    assert_eq!(t.m.messages().len(), 2);
    // A full review on request reviews everything again.
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/ai-review"), Some(json!({ "full": true }))).await;
    roll_reviews(&t, &id, 3).await;
    let user = t.m.messages().last().unwrap().user().to_string();
    assert!(user.contains("## src/parse.rs") && user.contains("## src/server.rs"));
    assert!(!user.contains("Cargo.lock"));
}

#[tokio::test]
async fn auto_mode_deep_context_and_classifier() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "auto": { "deep_paths": ["src/parse.rs"] } })).await;
    // Changes parse_header's definition: deep (deep_paths), with its callers from the index.
    let c = change(&t, "Rewrite parse", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    input.trim().len()\n}\n\npub fn helper_one(x: u32) -> u32 {\n    x + 1\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert_eq!(r[0]["ai"]["mode"], "deep");
    assert_eq!(r[0]["ai"]["chosen_by"], "auto-heuristic");
    assert!(r[0]["ai"]["reasons"][0].as_str().unwrap().contains("deep_paths"));
    assert!(r[0]["body"].as_str().unwrap().contains("(auto: src/parse.rs matches deep_paths)"));
    let user = t.m.messages().last().unwrap().user().to_string();
    assert!(user.contains("# Repository context"), "{user}");
    assert!(user.contains("Tests related to the changed code") && user.contains("tests/parse.rs"), "{user}");
    assert_eq!(t.m.messages().last().unwrap().body["output_config"]["effort"], "high");
    // Deep comments show confidence.
    assert!(threads(&t, &id).await[0]["comments"][0]["body"].as_str().unwrap().contains("confidence 95%"));
    // A small, unremarkable change is standard.
    let c2 = change(&t, "Tiny", json!({ "src/server.rs": { "content": "use crate::parse::parse_header;\n\npub fn handle(req: &str) {\n    let n = parse_header(req);\n    drop(n);\n}\n" } })).await;
    let r2 = roll_reviews(&t, c2["id"].as_str().unwrap(), 1).await;
    assert_eq!(r2[0]["ai"]["mode"], "standard");
    assert!(r2[0]["ai"]["reasons"][0].as_str().unwrap().contains("small change"));
    // The model classifier overrides the heuristic and its tokens are metered.
    enable(&t, json!({ "auto": { "classifier": "model" } })).await;
    let c3 = change(&t, "Tiny 2", json!({ "src/server.rs": { "content": "pub fn handle() {}\n" } })).await;
    let r3 = roll_reviews(&t, c3["id"].as_str().unwrap(), 1).await;
    assert_eq!(r3[0]["ai"]["mode"], "deep");
    assert_eq!(r3[0]["ai"]["chosen_by"], "auto-model");
    assert_eq!(r3[0]["ai"]["reasons"][0], "model says deep");
    let cls = t.m.messages().into_iter().find(|s| s.user().contains("how deep a code review")).unwrap();
    assert_eq!(cls.body["model"], "claude-haiku-5-5");
    assert_eq!(cls.body["output_config"]["effort"], "low");
    assert!(cls.body.get("fallbacks").is_none());
    // Code search uses the same index.
    let s = ok(&t.a, "GET", "/v1/code/repos/acme/api/search?q=parse_header&path=src/**", None).await;
    let paths: Vec<&str> = s["matches"].as_array().unwrap().iter().map(|m| m["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"src/server.rs") && !paths.contains(&"tests/parse.rs"), "{s}");
    let (st, _) = call(&t.a, "GET", "/v1/code/repos/acme/api/search?q=(&regex=true", None, Some(ROOT)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&t.a, "GET", "/v1/code/repos/acme/api/search?q=x", None, Some(&t.mallory)).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let rv = ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await;
    assert!(rv["index"]["stats"]["files"].as_u64().unwrap() >= 3);
}

#[tokio::test]
async fn fast_streams_threads_and_max_verifies() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "deep", "speed": "fast" })).await;
    let c = change(&t, "Fast", json!({
        "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n\n}\n" },
        "src/new.rs": { "content": "pub fn added() {}\n" },
    }))
    .await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert_eq!(r[0]["ai"]["speed"], "fast");
    // Streamed threads, not duplicated by the final review.
    let th = threads(&t, &id).await;
    assert_eq!(th.len(), 2, "{th:?}");
    assert!(th.iter().all(|x| x["review"].is_null()));
    let msgs = t.m.messages();
    assert!(msgs.iter().all(|s| s.body["model"] == "claude-haiku-5-5" && s.body["output_config"]["effort"] == "low"));
    // max: a verification pass that drops what it doesn't confirm.
    enable(&t, json!({ "mode": "max", "speed": "balanced" })).await;
    t.m.set(|s, _| {
        s.user().contains("checking another reviewer's findings").then(|| reply(json!({"results": [{"index": 0, "confirmed": false, "reason": "handled"}]}), json!({})))
    });
    let c2 = change(&t, "Max", json!({ "src/new.rs": { "content": "pub fn added() {}\n" } })).await;
    let r2 = roll_reviews(&t, c2["id"].as_str().unwrap(), 1).await;
    assert_eq!(r2[0]["ai"]["mode"], "max");
    assert_eq!(r2[0]["ai"]["details"]["dropped"]["unverified"], 1);
    assert_eq!(r2[0]["verdict"], "approve");
    assert!(threads(&t, c2["id"].as_str().unwrap()).await.is_empty());
}

#[tokio::test]
async fn roll_commands_in_threads() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "standard" })).await;
    let c = change(&t, "Cmds", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    roll_reviews(&t, &id, 1).await;
    let th = threads(&t, &id).await[0].clone();
    let tid = th["id"].as_str().unwrap().to_string();
    let say = |body: &str| {
        let (a, tid, body) = (&t.a, tid.clone(), body.to_string());
        async move { ok(a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": body }))).await }
    };
    let last_comment = |th: &Value| th["comments"].as_array().unwrap().last().unwrap()["body"].as_str().unwrap().to_string();
    let wait_comments = |n: usize| {
        let (a, tid) = (&t.a, tid.clone());
        async move {
            for _ in 0..400 {
                let th = ok(a, "GET", &format!("/v1/code/threads/{tid}"), None).await;
                if th["comments"].as_array().unwrap().len() >= n {
                    return th;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("thread never got {n} comments");
        }
    };
    // explain: a model reply in the thread, metered as chat.
    say("@roll explain").await;
    let th2 = wait_comments(3).await;
    assert_eq!(th2["comments"][2]["author"]["kind"], "roll");
    assert!(last_comment(&th2).contains("Here is why"));
    let ask = t.m.messages().into_iter().find(|s| s.user().contains("# Thread")).unwrap();
    assert!(ask.user().contains("@roll explain") && ask.user().contains("This panics"));
    // A question.
    say("@roll why does this matter?").await;
    let th3 = wait_comments(5).await;
    assert!(last_comment(&th3).contains("Here is why"));
    // don't flag → a learning, used by later reviews.
    say("@roll don't flag unchecked input in parsers").await;
    let th4 = wait_comments(7).await;
    assert!(last_comment(&th4).contains("I'll remember"));
    let l = ok(&t.a, "GET", "/v1/code/repos/acme/api/learnings", None).await;
    assert_eq!(l[0]["text"], "Don't flag unchecked input in parsers");
    assert_eq!(l[0]["source"], "command");
    // pause: new revisions aren't reviewed; resume: they are.
    say("@roll pause").await;
    wait_comments(9).await;
    let head = ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await["head"].as_str().unwrap().to_string();
    let before = t.m.messages().len();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": head, "message": "p", "files": { "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n + 0\n}\n" } } }))).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(t.m.messages().len(), before, "paused");
    say("@roll resume").await;
    wait_comments(11).await;
    // review deep: a forced mode, with the learning in the system prompt.
    say("@roll review deep").await;
    let r = roll_reviews(&t, &id, 2).await;
    let last = r.last().unwrap();
    assert_eq!(last["ai"]["mode"], "deep");
    assert_eq!(last["ai"]["chosen_by"], "command");
    let sys = t.m.messages().last().unwrap().system();
    assert!(sys.contains("Don't flag unchecked input in parsers"), "{sys}");
    // fix: the thread's lines rewritten as a new revision.
    let rev_before = ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await["revision"].as_u64().unwrap();
    let open: Vec<Value> = threads(&t, &id).await.into_iter().filter(|x| x["resolved"] == false && x["outdated"] == false).collect();
    let fix_tid = open.last().unwrap()["id"].as_str().unwrap().to_string();
    ok(&t.a, "POST", &format!("/v1/code/threads/{fix_tid}/comments"), Some(json!({ "body": "@roll fix" }))).await;
    let mut rev_after = rev_before;
    for _ in 0..400 {
        rev_after = ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await["revision"].as_u64().unwrap();
        if rev_after > rev_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(rev_after > rev_before, "fix made a revision");
    let file = t.a.raw("GET", &format!("/v1/code/repos/acme/api/blob/{}/src/parse.rs", ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await["head"].as_str().unwrap()), None, Some(ROOT)).await;
    assert!(String::from_utf8_lossy(&file.1).contains("let fixed = true;"));
    // resolve: roll's open threads are resolved.
    jobs_idle(&t, &id).await;
    let any = threads(&t, &id).await.into_iter().find(|x| x["resolved"] == false).map(|x| x["id"].as_str().unwrap().to_string());
    if let Some(tid) = any {
        ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "@roll resolve" }))).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(threads(&t, &id).await.iter().filter(|x| x["comments"][0]["author"]["kind"] == "roll").all(|x| x["resolved"] == true));
    }
    // Chats are metered.
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert!(u["totals"]["chats"].as_u64().unwrap() >= 3, "{}", u["totals"]);
    // Someone without write access can't command roll.
    let before = t.m.messages().len();
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "@roll explain" })), Some(&t.mallory)).await;
    assert!(s == StatusCode::NOT_FOUND || s == StatusCode::FORBIDDEN, "{s}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(t.m.messages().len(), before);
}

#[tokio::test]
async fn learnings_from_resolving_and_dismissing() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "standard" })).await;
    let c = change(&t, "L", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    let tid = threads(&t, &id).await[0]["id"].as_str().unwrap().to_string();
    // Resolving without saying why teaches nothing; with a reply it does.
    ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "Inputs are validated upstream." }))).await;
    ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/resolve"), None).await;
    let l = ok(&t.a, "GET", "/v1/code/repos/acme/api/learnings", None).await;
    assert_eq!(l.as_array().unwrap().len(), 1);
    assert_eq!(l[0]["source"], "resolved");
    assert!(l[0]["text"].as_str().unwrap().contains("Inputs are validated upstream.") && l[0]["text"].as_str().unwrap().contains("src/parse.rs"));
    // Resolving again doesn't duplicate it.
    ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/unresolve"), None).await;
    ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/resolve"), None).await;
    assert_eq!(ok(&t.a, "GET", "/v1/code/repos/acme/api/learnings", None).await.as_array().unwrap().len(), 1);
    // Dismissing roll's review with a reason.
    let rid = r[0]["id"].as_str().unwrap();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews/{rid}/dismiss"), Some(json!({ "reason": "False positive: we never pass empty input." }))).await;
    let l = ok(&t.a, "GET", "/v1/code/repos/acme/api/learnings", None).await;
    assert_eq!(l.as_array().unwrap().len(), 2);
    assert!(l.as_array().unwrap().iter().any(|x| x["source"] == "dismissed"));
    // Manual learnings, scoped by path; delete.
    let added = ok(&t.a, "POST", "/v1/code/repos/acme/api/learnings", Some(json!({ "text": "Prefer early returns", "path": "src/**" }))).await;
    assert_eq!(added["path"], "src/**");
    let (s, _) = call(&t.a, "POST", "/v1/code/repos/acme/api/learnings", Some(json!({ "text": "x" })), Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&t.a, "POST", "/v1/code/repos/acme/api/learnings", Some(json!({ "text": "" })), Some(ROOT)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let lid = added["id"].as_str().unwrap();
    let (s, _) = call(&t.a, "DELETE", &format!("/v1/code/repos/acme/api/learnings/{lid}"), None, Some(ROOT)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&t.a, "DELETE", &format!("/v1/code/repos/acme/api/learnings/{lid}"), None, Some(ROOT)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pricing_caps_allowances_and_byok() {
    // Public repo: hosted standard reviews are free (allowance 1), then charged; human cap $0.30.
    let t = setup_with("public", |c, _| {
        c.review.rates.public_free_reviews = 1;
        c.review.rates.human_monthly_cap = 0.30;
    })
    .await;
    enable(&t, json!({ "mode": "standard" })).await;
    let p = ok(&t.a, "GET", "/v1/code/pricing", None).await;
    assert_eq!(p["hosted"]["standard"]["min_per_review"], 0.25);
    assert_eq!(p["competitors"][0]["name"], "Greptile Pro");
    let one = |n: u32| json!({ format!("src/f{n}.rs"): { "content": format!("pub fn f{n}() {{}}\n") } });
    for n in 0..3 {
        let c = change(&t, &format!("c{n}"), one(n)).await;
        roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    }
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    let recent = u["recent"].as_array().unwrap();
    // Newest first: capped (0.05 left of 0.30), charged 0.25, free.
    assert_eq!(recent[2]["free"], true);
    assert_eq!(recent[2]["price_usd"], 0.0);
    assert_eq!(recent[1]["price_usd"], 0.25);
    assert_eq!(recent[0]["capped"], true);
    assert_eq!(recent[0]["price_usd"], 0.05);
    assert_eq!(u["totals"]["free_reviews"], 1);
    assert_eq!(u["totals"]["price_usd"], 0.3);
    // A spending cap that's reached pauses hosted reviews.
    let b = ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0.29, "on_cap": "pause" }))).await;
    assert_eq!(b["remaining_usd"], 0.0);
    let (s, _) = call(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "on_cap": "never" })), Some(ROOT)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 1 })), Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let before = t.m.messages().len();
    let c = change(&t, "capped", one(10)).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert!(r[0]["body"].as_str().unwrap().contains("paused for this month"));
    assert_eq!(t.m.messages().len(), before, "no model call");
    // byok_fallback without a key still pauses; with one, the owner's key is used.
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "on_cap": "byok_fallback" }))).await;
    let set = ok(&t.a, "PUT", "/v1/code/owners/acme/review-provider", Some(json!({ "provider": "anthropic", "api_key": "sk-ant-owner-key-9999" }))).await;
    assert_eq!(set["key_last4"], "9999");
    assert!(!set.to_string().contains("owner-key"));
    // With a BYOK key every review uses it (BYOK first), at the BYOK fee.
    let c = change(&t, "byok", one(11)).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!(r[0]["ai"]["key"], "byok");
    assert_eq!(t.m.messages().last().unwrap().headers["x-api-key"], "sk-ant-owner-key-9999");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["recent"][0]["price_usd"], 0.02);
    assert_eq!(u["recent"][0]["key_source"], "byok");
    // The key isn't readable anywhere: not in the API, not in the database file.
    let g = ok(&t.a, "GET", "/v1/code/owners/acme/review-provider", None).await;
    assert!(!g.to_string().contains("owner-key") && g["key_last4"] == "9999");
    let has = |b: &[u8]| b.windows(b"owner-key".len()).any(|w| w == b"owner-key");
    match t.a.code.db.sql().pg_schema() {
        None => {
            let db = std::fs::read(t.a.dir.path().join("code").join("control.db")).unwrap();
            let wal = std::fs::read(t.a.dir.path().join("code").join("control.db-wal")).unwrap_or_default();
            assert!(!has(&db) && !has(&wal));
        }
        // Postgres: every row of every table, as text.
        Some(_) => {
            let sql = t.a.code.db.sql();
            let tables: Vec<String> =
                sql.query("SELECT table_name FROM information_schema.tables WHERE table_schema = current_schema()", (), |r| r.get(0)).unwrap();
            assert!(tables.len() > 10, "{tables:?}");
            for table in tables {
                let rows: Vec<String> = sql.query(&format!("SELECT CAST(x AS TEXT) FROM {table} x"), (), |r| r.get(0)).unwrap();
                assert!(rows.iter().all(|r| !has(r.as_bytes())), "{table} holds the key");
            }
        }
    }
    let (s, _) = call(&t.a, "GET", "/v1/code/owners/acme/review-provider", None, Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&t.a, "PUT", "/v1/code/owners/acme/review-provider", Some(json!({ "api_key": "x" })), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&t.a, "PUT", "/v1/code/owners/acme/review-provider", Some(json!({ "provider": "openai_compatible", "api_key": "x" })), Some(ROOT)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "openai_compatible needs a model and base_url");
    let (s, _) = call(&t.a, "DELETE", "/v1/code/owners/acme/review-provider", None, Some(&t.acme)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Cleared (hosted): the owner reads nulls, not a 404; outsiders still get the 404.
    let (s, v) = call(&t.a, "GET", "/v1/code/owners/acme/review-provider", None, Some(ROOT)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "owner": "acme", "provider": null, "base_url": null, "model": null, "secondary_model": null, "key_last4": null, "updated_at": null }));
    let (s, _) = call(&t.a, "GET", "/v1/code/owners/acme/review-provider", None, Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // standard_only: a deep review over the cap is downgraded.
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "on_cap": "standard_only", "monthly_cap_usd": 0.31 }))).await;
    enable(&t, json!({ "mode": "deep" })).await;
    let c = change(&t, "downgraded", one(12)).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!(r[0]["ai"]["mode"], "standard");
    assert!(r[0]["ai"]["reasons"].to_string().contains("standard only"));
    let (s, _) = call(&t.a, "GET", "/v1/code/owners/acme/usage?month=2026-13x", None, Some(ROOT)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn byok_needs_a_server_secret() {
    let t = setup(|c, _| c.review.secret_key = None).await;
    let (s, v) = call(&t.a, "PUT", "/v1/code/owners/acme/review-provider", Some(json!({ "api_key": "sk-1" })), Some(ROOT)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(v["error"].as_str().unwrap().contains("ROLL_SECRET_KEY"));
    let weak = setup(|c, _| c.review.secret_key = Some("short".into())).await;
    let (s, v) = call(&weak.a, "PUT", "/v1/code/owners/acme/review-provider", Some(json!({ "api_key": "sk-1" })), Some(ROOT)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(v["error"].as_str().unwrap().contains("32"));
}

#[tokio::test]
async fn retries_then_gives_up_and_no_provider() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "standard", "required": true })).await;
    // 500 twice (the provider retries once each attempt), then fine: the job retries.
    t.m.set(|s, n| (s.path == "/v1/messages" && n <= 2).then(|| (500, json!({"error": {"message": "boom"}}))));
    let c = change(&t, "Retry", json!({ "src/parse.rs": { "content": "pub fn parse_header() {}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert!(!r[0]["ai"].is_null());
    let jobs = jobs_idle(&t, &id).await;
    assert_eq!(jobs[0]["state"], "done");
    assert_eq!(jobs[0]["attempts"], 2);
    // A bad key: no retries, a comment that says why, and the change stops waiting.
    t.m.set(|s, _| (s.path == "/v1/messages").then(|| (401, json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}))));
    let before = t.m.messages().len();
    let c2 = change(&t, "Bad key", json!({ "src/server.rs": { "content": "pub fn handle() {}\n" } })).await;
    let id2 = c2["id"].as_str().unwrap().to_string();
    let r2 = roll_reviews(&t, &id2, 1).await;
    assert!(r2[0]["body"].as_str().unwrap().contains("invalid x-api-key"));
    assert_eq!(t.m.messages().len(), before + 1);
    let ch = ok(&t.a, "GET", &format!("/v1/code/changes/{id2}"), None).await;
    assert!(!ch["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "review_pending"));
    assert_eq!(jobs_idle(&t, &id2).await[0]["state"], "failed");
    // No provider at all.
    let none = setup(|c, _| c.review.hosted = None).await;
    enable(&none, json!({ "mode": "standard" })).await;
    let v = ok(&none.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await;
    assert!(v["provider"].is_null());
    let c3 = change(&none, "None", json!({ "src/parse.rs": { "content": "x\n" } })).await;
    let r3 = roll_reviews(&none, c3["id"].as_str().unwrap(), 1).await;
    assert!(r3[0]["body"].as_str().unwrap().contains("no model provider"));
}

#[tokio::test]
async fn triggers_settings_and_auth() {
    let t = setup(|_, _| {}).await;
    // Off by default: nothing happens.
    let c = change(&t, "Off", json!({ "src/parse.rs": { "content": "a\n" } })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(t.m.messages().is_empty());
    assert!(ok(&t.a, "GET", &format!("/v1/code/changes/{}/ai-jobs", c["id"].as_str().unwrap()), None).await.as_array().unwrap().is_empty());
    // Settings validation and auth.
    let (s, v) = call(&t.a, "PATCH", "/v1/code/repos/acme/api/reviewer", Some(json!({ "mode": "huge" })), Some(ROOT)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("reviewer.mode"));
    let (s, _) = call(&t.a, "PATCH", "/v1/code/repos/acme/api/reviewer", Some(json!({ "enabled": true })), Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None, Some(&t.acme)).await;
    assert_eq!(s, StatusCode::OK);
    // .roll/review.yaml wins over the repo's settings, and its errors are reported.
    enable(&t, json!({ "mode": "standard", "drafts": false })).await;
    ok(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "cfg", "files": { ".roll/review.yaml": { "content": "reviewer:\n  mode: max\n  bogus: 1\n" } } }))).await;
    let v = ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await;
    assert_eq!(v["effective"]["mode"], "max");
    assert_eq!(v["settings"]["mode"], "standard");
    assert_eq!(v["file"]["mode"], "max");
    assert!(v["errors"][0].as_str().unwrap().contains("bogus"));
    // Drafts wait until they're ready.
    let d = ok(&t.a, "POST", "/v1/code/repos/acme/api/changes", Some(json!({ "title": "Draft", "message": "d", "author": "alice", "base": "main", "draft": true, "files": { "src/parse.rs": { "content": "d\n" } } }))).await;
    let did = d["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(ok(&t.a, "GET", &format!("/v1/code/changes/{did}/ai-jobs"), None).await.as_array().unwrap().is_empty());
    ok(&t.a, "PATCH", &format!("/v1/code/changes/{did}"), Some(json!({ "draft": false }))).await;
    roll_reviews(&t, &did, 1).await;
    // Requesting a review needs write access and a valid mode.
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{did}/ai-review"), Some(json!({})), Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{did}/ai-review"), Some(json!({ "mode": "huge" })), Some(ROOT)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = call(&t.a, "POST", &format!("/v1/code/changes/{did}/ai-review"), Some(json!({ "mode": "standard" })), Some(&t.acme)).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(v["job"]["mode"], "standard");
    roll_reviews(&t, &did, 2).await;
    // A fork's change (untrusted) isn't reviewed on its own.
    let v = ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await;
    assert!(v["jobs"]["last"]["state"].is_string());
}

#[tokio::test]
async fn restart_recovers_running_jobs() {
    let m = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = m.url.clone();
    let cfg = move |c: &mut code_control::Config| {
        c.admin_token = Some(ROOT.into());
        c.review.hosted = Some(Hosted { provider: "anthropic".into(), api_key: "sk-hosted".into(), base_url: Some(url.clone()), model: None, secondary_model: None });
        c.review.retry = fast_retry();
    };
    let code = common::open_in(dir.path(), cfg.clone());
    let a = App { router: code_control::api::router(code.clone()), code, dir };
    ok(&a, "POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    ok(&a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api" }))).await;
    ok(&a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "base", "files": { "a.rs": { "content": "x\n" } } }))).await;
    ok(&a, "PATCH", "/v1/code/repos/acme/api/reviewer", Some(json!({ "enabled": true, "mode": "standard" }))).await;
    // The reviewer isn't started: the job waits; mark it running as if the daemon died mid-review.
    let c = ok(&a, "POST", "/v1/code/repos/acme/api/changes", Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "a.rs": { "content": "y\n" } } }))).await;
    let id = c["id"].as_str().unwrap().to_string();
    let mut job = None;
    for _ in 0..100 {
        job = ok(&a, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await.as_array().unwrap().first().cloned();
        if job.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let job = job.unwrap();
    assert!(a.code.db.claim_ai_job(job["id"].as_str().unwrap()).unwrap());
    let home = a.dir.path().to_path_buf();
    drop(a.router);
    let code2 = common::open_in(&home, cfg);
    code2.start_reviewer();
    let a2 = App { router: code_control::api::router(code2.clone()), code: code2, dir: a.dir };
    let t = T { a: a2, m, acme: String::new(), mallory: String::new() };
    let r = roll_reviews(&t, &id, 1).await;
    assert!(!r[0]["ai"].is_null());
    assert_eq!(jobs_idle(&t, &id).await[0]["attempts"], 2);
}

#[tokio::test]
async fn one_job_at_a_time_per_change() {
    let a = app_with(|_| {});
    let db = &a.code.db;
    db.insert_ai_job("aij_1", "repo_x", "chg_1", "review", None, false, None, None).unwrap();
    db.insert_ai_job("aij_2", "repo_x", "chg_1", "review", Some("deep"), false, None, None).unwrap();
    db.insert_ai_job("aij_3", "repo_x", "chg_2", "review", None, false, None, None).unwrap();
    assert_eq!(db.due_ai_jobs(10).unwrap().len(), 3);
    assert!(db.claim_ai_job("aij_1").unwrap());
    // chg_1 is busy: its second job waits, other changes don't.
    assert!(!db.claim_ai_job("aij_2").unwrap());
    let due: Vec<String> = db.due_ai_jobs(10).unwrap().into_iter().map(|j| j.id).collect();
    assert_eq!(due, ["aij_3"]);
    assert!(db.claim_ai_job("aij_3").unwrap());
    assert!(!db.claim_ai_job("aij_3").unwrap(), "claimed once");
    db.finish_ai_job("aij_1", "done", None, None).unwrap();
    assert!(db.claim_ai_job("aij_2").unwrap());
    // A retry waits for its time.
    db.retry_ai_job("aij_2", "boom", code_control::db::now_ms() + 60_000).unwrap();
    assert!(db.due_ai_jobs(10).unwrap().iter().all(|j| j.id != "aij_2"));
    assert_eq!(db.ai_job("aij_2").unwrap().unwrap().error.as_deref(), Some("boom"));
}

/// Bring your own OpenAI-compatible key, end to end through code-control: the owner's key,
/// base URL and model are used for a strict `json_schema` chat completion (never the hosted
/// Anthropic key), and the review, its threads and its job record how it went.
#[tokio::test]
async fn byok_openai_compatible_end_to_end() {
    // The mock provider is on loopback: allowed explicitly (token mode refuses private endpoints).
    let t = setup(|c, _| c.allow_private_providers = Some(true)).await;
    t.m.set(|s, _| {
        if !s.path.ends_with("/chat/completions") {
            return None;
        }
        let user = s.body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
        let fake = Seen { path: "/v1/messages".into(), headers: HeaderMap::new(), body: json!({ "messages": [{ "content": [{ "text": user }] }] }) };
        let (_, v) = default_answer(&fake);
        let text = v["content"][0]["text"].as_str().unwrap().to_string();
        Some((200, json!({ "model": "gpt-test", "choices": [{ "finish_reason": "stop", "message": { "content": text } }], "usage": { "prompt_tokens": 900, "completion_tokens": 100, "prompt_tokens_details": { "cached_tokens": 300 } } })))
    });
    let set = ok(
        &t.a,
        "PUT",
        "/v1/code/owners/acme/review-provider",
        Some(json!({ "provider": "openai_compatible", "api_key": "sk-openai-owner-1234", "base_url": format!("{}/v1", t.m.url), "model": "gpt-test" })),
    )
    .await;
    assert_eq!((set["provider"].as_str(), set["model"].as_str(), set["key_last4"].as_str()), (Some("openai_compatible"), Some("gpt-test"), Some("1234")));
    enable(&t, json!({ "mode": "standard" })).await;
    let c = change(&t, "openai", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    input.len() + 1\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    let ai = &r[0]["ai"];
    assert_eq!((ai["provider"].as_str(), ai["key"].as_str(), ai["model"].as_str()), (Some("openai_compatible"), Some("byok"), Some("gpt-test")), "{ai}");
    assert_eq!((ai["tokens"]["input"].as_u64(), ai["tokens"]["cache_read"].as_u64(), ai["tokens"]["output"].as_u64()), (Some(600), Some(300), Some(100)));
    let calls: Vec<Seen> = t.m.seen.lock().iter().filter(|s| s.path == "/v1/chat/completions").cloned().collect();
    assert!(!calls.is_empty());
    for s in &calls {
        assert_eq!(s.headers["authorization"], "Bearer sk-openai-owner-1234");
        assert_eq!(s.body["model"], "gpt-test");
        assert_eq!(s.body["response_format"]["type"], "json_schema");
        assert_eq!(s.body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(s.body["messages"][0]["role"], "system");
    }
    assert!(t.m.messages().is_empty(), "the hosted Anthropic key is never used when the owner brings a key");
    // roll's finding comments carry the finding as data too.
    let th = threads(&t, &id).await;
    let roll = th.iter().find(|x| x["comments"][0]["author"]["kind"] == "roll").expect("a roll thread");
    let f = &roll["comments"][0]["finding"];
    assert_eq!(f["severity"], "bug");
    assert!(f["title"].as_str().unwrap().starts_with("Unchecked input"), "{f}");
    assert!(f["confidence"].is_null(), "standard reviews don't show confidence");
    // The job: done, with how far it got.
    let jobs = jobs_idle(&t, &id).await;
    let j = jobs.iter().find(|j| j["kind"] == "review").unwrap();
    assert_eq!(j["stage"], "done");
    let total = j["progress"]["groups_total"].as_u64().unwrap();
    assert!(total >= 1);
    assert_eq!(j["progress"]["groups_done"].as_u64(), Some(total));
}

/// A job reports its stage while it runs.
#[tokio::test]
async fn jobs_report_their_stage_while_running() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "standard" })).await;
    t.m.delay_ms.store(600, Ordering::SeqCst);
    let c = change(&t, "slow", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    input.len() * 2\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..200 {
        let jobs = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await;
        if let Some(j) = jobs.as_array().unwrap().first() {
            seen.insert(j["stage"].as_str().unwrap().to_string());
            if j["stage"] == "reviewing" {
                assert_eq!(j["progress"]["groups_done"], 0);
                assert!(j["progress"]["groups_total"].as_u64().unwrap() >= 1);
            }
            if j["state"] == "done" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    t.m.delay_ms.store(0, Ordering::SeqCst);
    assert!(seen.contains("reviewing"), "{seen:?}");
    assert!(seen.contains("done"), "{seen:?}");
    assert!(seen.iter().all(|s| ["queued", "indexing", "chunking", "reviewing", "verifying", "posting", "done"].contains(&s.as_str())), "{seen:?}");
}

/// An owner's own provider can't point at this server's private network: a private `base_url`
/// is refused when it's set, and one stored before (or under another setting) is refused when
/// it would be used, with no request sent. Nothing about the endpoint leaks into the job.
#[tokio::test]
async fn byok_endpoints_on_private_addresses_are_refused() {
    let t = setup(|_, _| {}).await;
    for base in ["http://127.0.0.1:9/v1", "http://169.254.169.254/latest", "https://[::1]/v1", "http://localhost:8080/v1", "http://user:pw@api.example.com/v1"] {
        let (s, v) = call(
            &t.a,
            "PUT",
            "/v1/code/owners/acme/review-provider",
            Some(json!({ "provider": "openai_compatible", "api_key": "sk-x-1234", "base_url": base, "model": "m" })),
            Some(ROOT),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{base}: {v}");
    }
    let (s, _) = call(
        &t.a,
        "PUT",
        "/v1/code/owners/acme/review-provider",
        Some(json!({ "provider": "anthropic", "api_key": "sk-x-1234", "base_url": "https://10.0.0.5" })),
        Some(ROOT),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // A row stored directly (as if written under ROLL_CODE_PROVIDER_ALLOW_PRIVATE): never used.
    let owner = t.a.code.db.owner("acme").unwrap().unwrap();
    let sealer = code_review::secret::Sealer::new(SECRET).unwrap();
    let row = code_control::ai::store::OwnerProvider {
        provider: "openai_compatible".into(),
        base_url: Some(format!("{}/v1", t.m.url)),
        model: Some("m".into()),
        secondary_model: None,
        key_sealed: sealer.seal(&owner.id, "sk-x-1234"),
        key_last4: "1234".into(),
        updated_at: 0,
    };
    t.a.code.db.set_owner_provider(&owner.id, Some(&row)).unwrap();
    enable(&t, json!({ "mode": "standard" })).await;
    let c = change(&t, "private", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    0\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let jobs = jobs_idle_n(&t, &id, 1).await;
    assert!(jobs.iter().all(|j| j["state"] != "done"), "{jobs:?}");
    assert!(t.m.seen.lock().iter().all(|s| !s.path.ends_with("/chat/completions")), "no request reached the private endpoint");
}

/// roll approves only what it read: a clean review that skipped files (past `max_files`) is a
/// comment, as is an incremental review that leaves earlier findings open. A full clean review
/// of everything still approves.
#[tokio::test]
async fn roll_doesnt_approve_what_it_didnt_read() {
    let t = setup(|_, _| {}).await;
    let clean = |s: &Seen, _: u32| {
        (s.path.ends_with("/messages") && !s.user().contains("how deep a code review"))
            .then(|| reply(json!({"walkthrough": "Fine.", "files": [], "diagram": null, "findings": [], "notes": []}), json!({"input_tokens": 10, "output_tokens": 5})))
    };
    t.m.set(clean);
    enable(&t, json!({ "mode": "standard", "max_files": 1 })).await;
    let c = change(&t, "Hidden", json!({ "src/a.rs": { "content": "pub fn a() {}\n" }, "src/b.rs": { "content": "pub fn b() { evil() }\n" } })).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!(r[0]["verdict"], "comment", "{}", r[0]["body"]);
    assert!(r[0]["body"].as_str().unwrap().contains("weren't reviewed"), "{}", r[0]["body"]);

    enable(&t, json!({ "mode": "standard", "max_files": 300 })).await;
    let c = change(&t, "Whole", json!({ "src/a.rs": { "content": "pub fn a2() {}\n" } })).await;
    assert_eq!(roll_reviews(&t, c["id"].as_str().unwrap(), 1).await[0]["verdict"], "approve");

    // Incremental: the first review finds a bug; a revision touching another file reviews clean,
    // but the earlier finding is still open, so no approval.
    t.m.set(|_, _| None);
    let c = change(&t, "Inc", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    input.len() - 1\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    assert_eq!(roll_reviews(&t, &id, 1).await[0]["verdict"], "request_changes");
    t.m.set(clean);
    let head = c["head"].as_str().unwrap().to_string();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": head, "message": "more", "files": { "src/server.rs": { "content": "pub fn handle() {}\n" } } }))).await;
    let r = roll_reviews(&t, &id, 2).await;
    let second = r.last().unwrap();
    assert_eq!(second["ai"]["incremental"], true);
    assert_eq!(second["verdict"], "comment", "{}", second["body"]);
    assert!(second["body"].as_str().unwrap().contains("still open"));
}

/// Anyone reading a public repo sees its reviewer setup, but the custom prompt the owner wrote
/// through the API only shows to writers.
#[tokio::test]
async fn custom_prompt_is_for_writers() {
    let t = setup_with("public", |_, _| {}).await;
    enable(&t, json!({ "prompt": "Our auth layer lives in src/secret_auth; be strict there." })).await;
    let (s, v) = call(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None, Some(&t.mallory)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["settings"]["prompt"], "(hidden)", "{v}");
    assert_eq!(v["effective"]["prompt"], "(hidden)");
    assert!(!v.to_string().contains("secret_auth"));
    let (_, v) = call(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None, None).await;
    assert!(!v.to_string().contains("secret_auth"));
    let v = ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await;
    assert!(v["effective"]["prompt"].as_str().unwrap().contains("secret_auth"));
    let (_, v) = call(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None, Some(&t.acme)).await;
    assert!(v["settings"]["prompt"].as_str().unwrap().contains("secret_auth"));
}

#[tokio::test]
async fn concurrent_reviews_and_chats_never_overrun_the_cap() {
    // Four reviews at once, room under the cap for two ($0.25 each): the cap holds.
    let t = setup(|c, _| c.review.concurrency = 4).await;
    enable(&t, json!({ "mode": "standard" })).await;
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0.5, "on_cap": "pause" }))).await;
    // The model doesn't answer until told to, so all four jobs decide while two are running.
    t.m.delay_ms.store(u32::MAX, Ordering::SeqCst);
    let mut ids = Vec::new();
    for n in 0..4 {
        let c = change(&t, &format!("c{n}"), json!({ format!("src/c{n}.rs"): { "content": format!("pub fn c{n}() {{}}\n") } })).await;
        ids.push(c["id"].as_str().unwrap().to_string());
    }
    // Two are refused before anything is metered: the running two's reservations did that, not
    // the month's spend.
    let owner = t.a.code.owner_or_404("acme").unwrap().id;
    let mut paused = 0;
    for _ in 0..200 {
        paused = 0;
        for id in &ids {
            let (_, v) = call(&t.a, "GET", &format!("/v1/code/changes/{id}/reviews"), None, Some(ROOT)).await;
            paused += v.as_array().unwrap().iter().filter(|r| r["body"].as_str().unwrap_or_default().contains("paused for this month")).count();
        }
        if paused == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(paused, 2, "two refused while the others run");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.0, "nothing metered yet: {u}");
    assert_eq!(t.a.code.reserved_usd(&owner), 0.5, "the running two hold the cap");
    t.m.delay_ms.store(0, Ordering::SeqCst);
    let mut reviewed = Vec::new();
    for id in &ids {
        let r = roll_reviews(&t, id, 1).await;
        if !r[0]["ai"].is_null() {
            reviewed.push(id.clone());
        } else {
            assert!(r[0]["body"].as_str().unwrap().contains("paused for this month"), "{}", r[0]["body"]);
        }
    }
    t.m.delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(reviewed.len(), 2, "exactly two fit under the cap");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.5);
    assert_eq!(t.m.messages().iter().filter(|s| !s.user().contains("# Thread")).count(), 2, "no model call for capped reviews");
    assert_eq!(t.a.code.reserved_usd(&t.a.code.owner_or_404("acme").unwrap().id), 0.0, "reservations released");

    // Chats count too: at the cap, `@roll explain` answers without a model call.
    let th = threads(&t, &reviewed[0]).await[0].clone();
    let tid = th["id"].as_str().unwrap().to_string();
    let before = t.m.messages().len();
    ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "@roll explain" }))).await;
    let mut last = Value::Null;
    for _ in 0..400 {
        last = ok(&t.a, "GET", &format!("/v1/code/threads/{tid}"), None).await;
        if last["comments"].as_array().unwrap().len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let reply = last["comments"][2]["body"].as_str().unwrap_or_default();
    assert!(reply.contains("cap") && reply.contains("reached"), "{last}");
    assert_eq!(t.m.messages().len(), before, "no model call for a capped chat");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.5, "nothing charged past the cap");

    // Raising the cap lets chats through again (and they're metered).
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0.6 }))).await;
    ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "@roll explain" }))).await;
    for _ in 0..400 {
        last = ok(&t.a, "GET", &format!("/v1/code/threads/{tid}"), None).await;
        if last["comments"].as_array().unwrap().len() >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(last["comments"][4]["body"].as_str().unwrap_or_default().contains("Here is why"), "{last}");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.55);

    // A chat while a review runs: room for the review ($0.25) only, so the review's reservation
    // refuses the chat before either is metered.
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0.8 }))).await;
    t.m.delay_ms.store(u32::MAX, Ordering::SeqCst);
    let c = change(&t, "c4", json!({ "src/c4.rs": { "content": "pub fn c4() {}\n" } })).await;
    for _ in 0..200 {
        if t.a.code.reserved_usd(&owner) > 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(t.a.code.reserved_usd(&owner), 0.25, "the review is running");
    ok(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "@roll explain" }))).await;
    for _ in 0..400 {
        last = ok(&t.a, "GET", &format!("/v1/code/threads/{tid}"), None).await;
        if last["comments"].as_array().unwrap().len() >= 7 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(last["comments"][6]["body"].as_str().unwrap_or_default().contains("reached"), "{last}");
    assert_eq!(ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await["totals"]["price_usd"], 0.55, "refused while the review held its share");
    t.m.delay_ms.store(0, Ordering::SeqCst);
    assert!(!roll_reviews(&t, c["id"].as_str().unwrap(), 1).await[0]["ai"].is_null(), "the review ran");
    t.m.delay_ms.store(0, Ordering::SeqCst);
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.8);
    assert_eq!(u["totals"]["chats"], 1, "only the chat that fit was metered");
}

/// The month's free public reviews can't be claimed twice by reviews running at once: each
/// running review holds the free one it took, and is metered as free exactly when it did.
#[tokio::test]
async fn concurrent_public_reviews_never_share_the_last_free_one() {
    let t = setup_with("public", |c, _| {
        c.review.concurrency = 3;
        c.review.rates.public_free_reviews = 1;
    })
    .await;
    enable(&t, json!({ "mode": "standard" })).await;
    // Room for one paid review ($0.25) besides the free one.
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0.25, "on_cap": "pause" }))).await;
    t.m.delay_ms.store(1_500, Ordering::SeqCst);
    let mut ids = Vec::new();
    for n in 0..3 {
        let c = change(&t, &format!("p{n}"), json!({ format!("src/p{n}.rs"): { "content": format!("pub fn p{n}() {{}}\n") } })).await;
        ids.push(c["id"].as_str().unwrap().to_string());
    }
    let mut prices = Vec::new();
    let mut paused = 0;
    for id in &ids {
        let r = roll_reviews(&t, id, 1).await;
        match r[0]["ai"]["price_usd"].as_f64() {
            Some(p) => prices.push(p),
            None => {
                assert!(r[0]["body"].as_str().unwrap().contains("paused for this month"), "{}", r[0]["body"]);
                paused += 1;
            }
        }
    }
    t.m.delay_ms.store(0, Ordering::SeqCst);
    prices.sort_by(f64::total_cmp);
    assert_eq!((prices, paused), (vec![0.0, 0.25], 1), "one free, one paid, one over the cap");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.25, "never over the cap: {u}");
    assert_eq!(u["totals"]["free_reviews"], 1);
    let owner = t.a.code.owner_or_404("acme").unwrap().id;
    assert_eq!((t.a.code.reserved_usd(&owner), t.a.code.reserved_free(&owner)), (0.0, 0));
}

#[tokio::test]
async fn a_deep_review_goes_on_without_context_while_the_index_is_first_built() {
    let t = setup(|c, _| c.review_index_wait_ms = 0).await;
    enable(&t, json!({ "mode": "deep" })).await;
    let c = change(&t, "Deep", json!({ "src/parse.rs": { "content": "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n\n}\n" } })).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!(r[0]["ai"]["mode"], "deep");
    assert!(r[0]["ai"]["reasons"].to_string().contains("still being built"), "{}", r[0]["ai"]["reasons"]);
    let req = t.m.messages().into_iter().find(|s| s.user().contains("src/parse.rs")).unwrap();
    assert!(!req.user().contains("outside this change"), "no repository context");
}

// ── Coverage of every route and status, mode, speed, on_cap, command, error and job stage ──

/// `(method, path, body, token, expected status)`.
type Case<'a> = (&'a str, String, Option<Value>, Option<&'a str>, StatusCode);

/// Every `(method, path, body, token) → status` in order, with the answer on failure.
async fn expect(a: &App, cases: Vec<Case<'_>>) {
    for (m, p, b, tok, want) in cases {
        let (s, v) = call(a, m, &p, b, tok).await;
        assert_eq!(s, want, "{m} {p} as {tok:?}: {v}");
    }
}

/// An `agent:` token of acme: works code, never manages the owner.
async fn agent_token(t: &T) -> String {
    ok(&t.a, "POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "agent:bot" }))).await["token"].as_str().unwrap().to_string()
}

/// Wait until thread `tid` has at least `n` comments.
async fn thread_with(t: &T, tid: &str, n: usize) -> Value {
    for _ in 0..400 {
        let th = ok(&t.a, "GET", &format!("/v1/code/threads/{tid}"), None).await;
        if th["comments"].as_array().unwrap().len() >= n {
            return th;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("thread {tid} never got {n} comments");
}

fn last_body(th: &Value) -> String {
    th["comments"].as_array().unwrap().last().unwrap()["body"].as_str().unwrap().to_string()
}

const PARSE_EDIT: &str = "pub fn parse_header(input: &str) -> usize {\n    let n = input.len();\n    n\n}\n";

/// `GET|PATCH …/reviewer`, `GET|POST …/learnings`, `DELETE …/learnings/{id}` and `GET …/search`:
/// every status each can answer. A public repo gives readers (200 on reads, 401/403 on writes);
/// a private one hides itself (404).
#[tokio::test]
async fn repo_reviewer_routes_answer_every_status() {
    use StatusCode as S;
    let t = setup_with("public", |_, _| {}).await;
    ok(&t.a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "secret", "visibility": "private" }))).await;
    let agent = agent_token(&t).await;
    let (root, acme, mallory, bot) = (Some(ROOT), Some(t.acme.as_str()), Some(t.mallory.as_str()), Some(agent.as_str()));
    let api = |p: &str| format!("/v1/code/repos/acme/api{p}");
    let secret = |p: &str| format!("/v1/code/repos/acme/secret{p}");
    let nope = |p: &str| format!("/v1/code/repos/acme/nope{p}");

    // GET …/reviewer: readers of a public repo; nobody but the owner on a private one.
    expect(
        &t.a,
        vec![
            ("GET", api("/reviewer"), None, root, S::OK),
            ("GET", api("/reviewer"), None, acme, S::OK),
            ("GET", api("/reviewer"), None, bot, S::OK),
            ("GET", api("/reviewer"), None, mallory, S::OK),
            ("GET", api("/reviewer"), None, None, S::OK),
            ("GET", secret("/reviewer"), None, acme, S::OK),
            ("GET", secret("/reviewer"), None, mallory, S::NOT_FOUND),
            ("GET", secret("/reviewer"), None, None, S::NOT_FOUND),
            ("GET", nope("/reviewer"), None, root, S::NOT_FOUND),
        ],
    )
    .await;
    let v = ok(&t.a, "GET", &api("/reviewer"), None).await;
    assert_eq!(v["effective"]["enabled"], false, "off by default");
    assert_eq!(v["jobs"], json!({ "queued": 0, "running": 0, "last": null }));
    assert!(v["errors"].as_array().unwrap().is_empty() && v["file"].is_null());

    // PATCH …/reviewer: managers only (not agents); 400 names the bad field.
    expect(
        &t.a,
        vec![
            ("PATCH", api("/reviewer"), Some(json!({ "speed": "thorough", "profile": "assertive" })), acme, S::OK),
            ("PATCH", api("/reviewer"), Some(json!({ "enabled": true })), None, S::UNAUTHORIZED),
            ("PATCH", api("/reviewer"), Some(json!({ "enabled": true })), mallory, S::FORBIDDEN),
            ("PATCH", api("/reviewer"), Some(json!({ "enabled": true })), bot, S::FORBIDDEN),
            ("PATCH", secret("/reviewer"), Some(json!({ "enabled": true })), mallory, S::NOT_FOUND),
            ("PATCH", secret("/reviewer"), Some(json!({ "enabled": true })), None, S::NOT_FOUND),
            ("PATCH", nope("/reviewer"), Some(json!({ "enabled": true })), root, S::NOT_FOUND),
        ],
    )
    .await;
    for (bad, field) in [
        (json!({ "speed": "warp" }), "speed"),
        (json!({ "profile": "loud" }), "profile"),
        (json!({ "effort": "extreme" }), "effort"),
        (json!({ "max_files": 0 }), "max_files"),
        (json!({ "auto": { "classifier": "coin" } }), "auto"),
        (json!({ "enabled": "yes" }), "enabled"),
        (json!({ "bogus": 1 }), "bogus"),
    ] {
        let (s, v) = call(&t.a, "PATCH", &api("/reviewer"), Some(bad.clone()), root).await;
        assert_eq!(s, S::BAD_REQUEST, "{bad}");
        assert!(v["error"].as_str().unwrap().contains(field), "{bad}: {v}");
    }
    let v = ok(&t.a, "GET", &api("/reviewer"), None).await;
    assert_eq!((v["effective"]["speed"].as_str(), v["effective"]["profile"].as_str()), (Some("thorough"), Some("assertive")), "a bad patch changes nothing");

    // GET/POST …/learnings and DELETE …/learnings/{id}.
    ok(&t.a, "POST", "/v1/code/repos/acme/secret/land", Some(json!({ "message": "s", "files": { "a.rs": { "content": "x\n" } } }))).await;
    let other = ok(&t.a, "POST", &secret("/learnings"), Some(json!({ "text": "secret repo's own" }))).await;
    expect(
        &t.a,
        vec![
            ("GET", api("/learnings"), None, root, S::OK),
            ("GET", api("/learnings"), None, None, S::OK),
            ("GET", secret("/learnings"), None, mallory, S::NOT_FOUND),
            ("GET", secret("/learnings"), None, None, S::NOT_FOUND),
            ("GET", nope("/learnings"), None, root, S::NOT_FOUND),
            ("POST", api("/learnings"), Some(json!({ "text": "x" })), None, S::UNAUTHORIZED),
            ("POST", api("/learnings"), Some(json!({ "text": "x" })), mallory, S::FORBIDDEN),
            ("POST", secret("/learnings"), Some(json!({ "text": "x" })), mallory, S::NOT_FOUND),
            ("POST", nope("/learnings"), Some(json!({ "text": "x" })), root, S::NOT_FOUND),
            ("POST", api("/learnings"), Some(json!({})), root, S::BAD_REQUEST),
            ("POST", api("/learnings"), Some(json!({ "text": "   " })), root, S::BAD_REQUEST),
            ("POST", api("/learnings"), Some(json!({ "text": "x".repeat(2001) })), root, S::BAD_REQUEST),
        ],
    )
    .await;
    let (s, l) = call(&t.a, "POST", &api("/learnings"), Some(json!({ "text": " Prefer early returns ", "path": "src/**" })), acme).await;
    assert_eq!(s, S::CREATED);
    assert_eq!((l["text"].as_str(), l["path"].as_str(), l["source"].as_str()), (Some("Prefer early returns"), Some("src/**"), Some("api")));
    assert!(l["id"].as_str().unwrap().starts_with("lrn_"));
    let (s, by_agent) = call(&t.a, "POST", &api("/learnings"), Some(json!({ "text": "Agents write code too" })), bot).await;
    assert_eq!(s, S::CREATED, "an agent token is a writer");
    let lid = l["id"].as_str().unwrap();
    expect(
        &t.a,
        vec![
            ("DELETE", api(&format!("/learnings/{lid}")), None, None, S::UNAUTHORIZED),
            ("DELETE", api(&format!("/learnings/{lid}")), None, mallory, S::FORBIDDEN),
            ("DELETE", secret(&format!("/learnings/{lid}")), None, mallory, S::NOT_FOUND),
            ("DELETE", api("/learnings/lrn_nope"), None, root, S::NOT_FOUND),
            // Another repo's learning isn't this repo's to delete.
            ("DELETE", api(&format!("/learnings/{}", other["id"].as_str().unwrap())), None, root, S::NOT_FOUND),
            ("DELETE", api(&format!("/learnings/{lid}")), None, acme, S::NO_CONTENT),
            ("DELETE", api(&format!("/learnings/{lid}")), None, acme, S::NOT_FOUND),
            ("DELETE", api(&format!("/learnings/{}", by_agent["id"].as_str().unwrap())), None, bot, S::NO_CONTENT),
        ],
    )
    .await;
    assert!(ok(&t.a, "GET", &api("/learnings"), None).await.as_array().unwrap().is_empty());
    assert_eq!(ok(&t.a, "GET", &secret("/learnings"), None).await.as_array().unwrap().len(), 1);

    // GET …/search: 200 to readers, 400 for a bad query, 404 for what they can't see.
    expect(
        &t.a,
        vec![
            ("GET", api("/search?q=parse_header"), None, None, S::OK),
            ("GET", api("/search?q=parse_header&case=true&limit=1"), None, mallory, S::OK),
            ("GET", api("/search?q="), None, root, S::BAD_REQUEST),
            ("GET", api("/search"), None, root, S::BAD_REQUEST),
            ("GET", api(&format!("/search?q={}", "a".repeat(501))), None, root, S::BAD_REQUEST),
            ("GET", api("/search?q=[&regex=true"), None, root, S::BAD_REQUEST),
            ("GET", api("/search?q=x&rev=no-such-rev"), None, root, S::NOT_FOUND),
            ("GET", secret("/search?q=x"), None, mallory, S::NOT_FOUND),
            ("GET", secret("/search?q=x"), None, None, S::NOT_FOUND),
            ("GET", nope("/search?q=x"), None, root, S::NOT_FOUND),
        ],
    )
    .await;
    let one = ok(&t.a, "GET", &api("/search?q=parse_header&limit=1"), None).await;
    assert_eq!((one["matches"].as_array().unwrap().len(), one["truncated"].as_bool()), (1, Some(true)), "{one}");
    let cs = ok(&t.a, "GET", &api("/search?q=PARSE_HEADER&case=true"), None).await;
    assert!(cs["matches"].as_array().unwrap().is_empty(), "case-sensitive: {cs}");
    let ci = ok(&t.a, "GET", &api("/search?q=PARSE_HEADER"), None).await;
    assert!(!ci["matches"].as_array().unwrap().is_empty(), "case-insensitive by default");
}

/// Without the codebase index (`review.index = false`) code search is off: 422, and the
/// reviewer view shows no index. (503 while a first build runs: `index_scale.rs`.)
#[tokio::test]
async fn search_is_unprocessable_without_the_index() {
    let t = setup(|c, _| c.review.index = false).await;
    let (s, v) = call(&t.a, "GET", "/v1/code/repos/acme/api/search?q=parse_header", None, Some(ROOT)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"].as_str().unwrap().contains("code search is off"));
    assert!(ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await["index"].is_null());
}

/// `POST /changes/{id}/ai-review` (202, 400, 401, 403, 404, 409) and `GET /changes/{id}/ai-jobs`
/// (200, 404), plus how requested jobs queue: one job runs per change, a plain request folds into
/// the queued one, `full` or a mode makes a new one, and a request for an unchanged head is
/// `skipped`. Jobs are listed oldest first.
#[tokio::test]
async fn change_reviewer_routes_answer_every_status_and_jobs_queue() {
    use StatusCode as S;
    let t = setup_with("public", |_, _| {}).await;
    enable(&t, json!({ "mode": "standard", "auto_review": false })).await;
    let agent = agent_token(&t).await;
    let (root, acme, mallory, bot) = (Some(ROOT), Some(t.acme.as_str()), Some(t.mallory.as_str()), Some(agent.as_str()));
    let c = change(&t, "Req", json!({ "src/parse.rs": { "content": PARSE_EDIT } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    // auto_review off: nothing runs on its own.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(ok(&t.a, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await.as_array().unwrap().is_empty());

    // A private repo's change, for the 404s.
    ok(&t.a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "secret", "visibility": "private" }))).await;
    ok(&t.a, "POST", "/v1/code/repos/acme/secret/land", Some(json!({ "message": "s", "files": { "a.rs": { "content": "x\n" } } }))).await;
    let hidden = ok(&t.a, "POST", "/v1/code/repos/acme/secret/changes", Some(json!({ "title": "h", "message": "h", "base": "main", "files": { "a.rs": { "content": "y\n" } } }))).await;
    let hid = hidden["id"].as_str().unwrap().to_string();
    let review = |c: &str| format!("/v1/code/changes/{c}/ai-review");
    let jobs = |c: &str| format!("/v1/code/changes/{c}/ai-jobs");
    expect(
        &t.a,
        vec![
            ("POST", review(&id), Some(json!({})), None, S::UNAUTHORIZED),
            ("POST", review(&id), Some(json!({})), mallory, S::FORBIDDEN),
            ("POST", review(&hid), Some(json!({})), mallory, S::NOT_FOUND),
            ("POST", review(&hid), Some(json!({})), None, S::NOT_FOUND),
            ("POST", review("chg_nope"), Some(json!({})), root, S::NOT_FOUND),
            ("POST", review(&id), Some(json!({ "mode": "huge" })), root, S::BAD_REQUEST),
            ("GET", jobs(&id), None, None, S::OK),
            ("GET", jobs(&id), None, mallory, S::OK),
            ("GET", jobs(&hid), None, mallory, S::NOT_FOUND),
            ("GET", jobs(&hid), None, None, S::NOT_FOUND),
            ("GET", jobs("chg_nope"), None, root, S::NOT_FOUND),
        ],
    )
    .await;

    // One job at a time per change: while the first runs, later requests queue.
    t.m.delay_ms.store(500, Ordering::SeqCst);
    let (s, first) = call(&t.a, "POST", &review(&id), Some(json!({ "mode": "standard" })), bot).await;
    assert_eq!(s, S::ACCEPTED, "an agent token can ask for a review");
    let first = first["job"].clone();
    assert_eq!((first["kind"].as_str(), first["mode"].as_str(), first["full"].as_bool()), (Some("review"), Some("standard"), Some(false)));
    assert_eq!(first["requested_by"], "acme", "the owner the token belongs to");
    assert!(first["id"].as_str().unwrap().starts_with("aij_"));
    let mut running = false;
    for _ in 0..200 {
        let j = ok(&t.a, "GET", &jobs(&id), None).await;
        if j[0]["state"] == "running" {
            running = true;
            assert_eq!(j[0]["attempts"], 1);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(running, "the first job runs");
    let (s, plain) = call(&t.a, "POST", &review(&id), Some(json!({ "mode": "auto" })), acme).await;
    assert_eq!(s, S::ACCEPTED);
    assert!(plain["job"]["mode"].is_null(), "auto means the repo's settings decide");
    assert_eq!((plain["job"]["state"].as_str(), plain["job"]["stage"].as_str()), (Some("queued"), Some("queued")));
    let (_, again) = call(&t.a, "POST", &review(&id), Some(json!({})), acme).await;
    assert_eq!(again["job"]["id"], plain["job"]["id"], "a plain request folds into the queued one");
    let (_, full) = call(&t.a, "POST", &review(&id), Some(json!({ "full": true })), root).await;
    assert_ne!(full["job"]["id"], plain["job"]["id"], "a full review is its own job");
    assert_eq!(full["job"]["full"], true);
    let view = ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await;
    assert_eq!((view["jobs"]["queued"].as_u64(), view["jobs"]["running"].as_u64()), (Some(2), Some(1)), "{}", view["jobs"]);
    t.m.delay_ms.store(0, Ordering::SeqCst);
    let all = jobs_idle_n(&t, &id, 3).await;
    let ids: Vec<&str> = all.iter().map(|j| j["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [first["id"].as_str().unwrap(), plain["job"]["id"].as_str().unwrap(), full["job"]["id"].as_str().unwrap()], "oldest first");
    let states: Vec<&str> = all.iter().map(|j| j["state"].as_str().unwrap()).collect();
    assert_eq!(states, ["done", "skipped", "done"], "the plain request found nothing new since the first review");
    assert!(all.iter().all(|j| j["stage"] == "done"));
    let reviews = roll_reviews(&t, &id, 2).await;
    assert_eq!(all[0]["review_id"], reviews[0]["id"]);
    assert_eq!(all[2]["review_id"], reviews[1]["id"]);
    assert!(all[1]["review_id"].is_null());
    assert_eq!(reviews[1]["ai"]["incremental"], false);

    // Landed and closed changes: 409.
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/close"), None).await;
    let (s, v) = call(&t.a, "POST", &review(&id), Some(json!({})), root).await;
    assert_eq!(s, S::CONFLICT, "{v}");
    let c2 = change(&t, "Land me", json!({ "src/server.rs": { "content": "pub fn handle() {}\n" } })).await;
    let id2 = c2["id"].as_str().unwrap().to_string();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id2}/land"), Some(json!({}))).await;
    assert_eq!(ok(&t.a, "GET", &format!("/v1/code/changes/{id2}"), None).await["state"], "landed");
    let (s, _) = call(&t.a, "POST", &review(&id2), Some(json!({ "mode": "deep" })), root).await;
    assert_eq!(s, S::CONFLICT);
}

/// `GET /pricing`, `GET|PUT|DELETE /owners/{owner}/review-provider`, `GET|PUT /owners/{owner}/billing`
/// and `GET /owners/{owner}/usage`: every status. Reads by anyone but the owner (or admin) are
/// 404; writes are 401 without a token and 403 with another owner's or an agent's.
#[tokio::test]
async fn owner_reviewer_routes_answer_every_status() {
    use StatusCode as S;
    let t = setup(|_, _| {}).await;
    let agent = agent_token(&t).await;
    let (root, acme, mallory, bot) = (Some(ROOT), Some(t.acme.as_str()), Some(t.mallory.as_str()), Some(agent.as_str()));
    let o = |p: &str| format!("/v1/code/owners/acme{p}");
    let nobody = |p: &str| format!("/v1/code/owners/nobody{p}");

    // Pricing is public.
    let (s, p) = call(&t.a, "GET", "/v1/code/pricing", None, None).await;
    assert_eq!(s, S::OK);
    assert_eq!(p["spending_caps"]["on_cap"], json!(["pause", "byok_fallback", "standard_only"]));
    assert_eq!(p["currency"], "USD");
    assert!(p["byok"]["standard"].is_number() && p["hosted"]["chat"].is_number() && p["public_repos"]["free_standard_reviews_per_month"].is_number(), "{p}");

    // review-provider.
    expect(
        &t.a,
        vec![
            ("GET", o("/review-provider"), None, root, S::OK),
            ("GET", o("/review-provider"), None, acme, S::OK),
            ("GET", o("/review-provider"), None, mallory, S::NOT_FOUND),
            ("GET", o("/review-provider"), None, bot, S::NOT_FOUND),
            ("GET", o("/review-provider"), None, None, S::NOT_FOUND),
            ("GET", nobody("/review-provider"), None, root, S::NOT_FOUND),
            ("PUT", o("/review-provider"), Some(json!({ "api_key": "sk-1" })), None, S::UNAUTHORIZED),
            ("PUT", o("/review-provider"), Some(json!({ "api_key": "sk-1" })), mallory, S::FORBIDDEN),
            ("PUT", o("/review-provider"), Some(json!({ "api_key": "sk-1" })), bot, S::FORBIDDEN),
            ("PUT", nobody("/review-provider"), Some(json!({ "api_key": "sk-1" })), root, S::NOT_FOUND),
            ("PUT", o("/review-provider"), Some(json!({})), root, S::BAD_REQUEST),
            ("PUT", o("/review-provider"), Some(json!({ "api_key": "  " })), root, S::BAD_REQUEST),
            ("PUT", o("/review-provider"), Some(json!({ "api_key": "k".repeat(4097) })), root, S::BAD_REQUEST),
            ("PUT", o("/review-provider"), Some(json!({ "provider": "gemini", "api_key": "sk-1" })), root, S::BAD_REQUEST),
            ("PUT", o("/review-provider"), Some(json!({ "api_key": "sk-1", "base_url": "http://api.example.com" })), root, S::BAD_REQUEST),
            ("PUT", o("/review-provider"), Some(json!({ "api_key": "sk-1", "base_url": "not a url" })), root, S::BAD_REQUEST),
            ("PUT", o("/review-provider"), Some(json!({ "provider": "openai_compatible", "api_key": "sk-1", "base_url": "https://api.example.com/v1" })), root, S::BAD_REQUEST),
            ("DELETE", o("/review-provider"), None, None, S::UNAUTHORIZED),
            ("DELETE", o("/review-provider"), None, mallory, S::FORBIDDEN),
            ("DELETE", o("/review-provider"), None, bot, S::FORBIDDEN),
            ("DELETE", nobody("/review-provider"), None, root, S::NOT_FOUND),
        ],
    )
    .await;
    assert!(ok(&t.a, "GET", &o("/review-provider"), None).await["key_last4"].is_null(), "no bad request stored a key");
    // A key too short to show any of: no last 4.
    let (s, v) = call(&t.a, "PUT", &o("/review-provider"), Some(json!({ "api_key": "sk-short-1234" })), acme).await;
    assert_eq!((s, v["key_last4"].as_str()), (S::OK, Some("")));
    let (s, v) = call(&t.a, "PUT", &o("/review-provider"), Some(json!({ "api_key": "sk-ant-api-abcd1234", "secondary_model": "claude-haiku-5-5" })), acme).await;
    assert_eq!(s, S::OK);
    assert_eq!((v["owner"].as_str(), v["provider"].as_str(), v["key_last4"].as_str(), v["secondary_model"].as_str()), (Some("acme"), Some("anthropic"), Some("1234"), Some("claude-haiku-5-5")));
    assert!(v["updated_at"].is_number());
    let rv = ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await;
    assert_eq!(rv["provider"]["source"], "byok");
    expect(&t.a, vec![("DELETE", o("/review-provider"), None, acme, S::NO_CONTENT), ("DELETE", o("/review-provider"), None, root, S::NO_CONTENT)]).await;
    assert_eq!(ok(&t.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await["provider"]["source"], "hosted");

    // billing.
    let b = ok(&t.a, "GET", &o("/billing"), None).await;
    assert_eq!((b["monthly_cap_usd"].clone(), b["on_cap"].as_str(), b["remaining_usd"].clone(), b["spent_usd"].as_f64()), (Value::Null, Some("pause"), Value::Null, Some(0.0)));
    assert!(b["updated_at"].is_null() && b["month"].as_str().unwrap().len() == 7);
    expect(
        &t.a,
        vec![
            ("GET", o("/billing"), None, acme, S::OK),
            ("GET", o("/billing"), None, mallory, S::NOT_FOUND),
            ("GET", o("/billing"), None, bot, S::NOT_FOUND),
            ("GET", o("/billing"), None, None, S::NOT_FOUND),
            ("GET", nobody("/billing"), None, root, S::NOT_FOUND),
            ("PUT", o("/billing"), Some(json!({ "monthly_cap_usd": 5 })), None, S::UNAUTHORIZED),
            ("PUT", o("/billing"), Some(json!({ "monthly_cap_usd": 5 })), mallory, S::FORBIDDEN),
            ("PUT", o("/billing"), Some(json!({ "monthly_cap_usd": 5 })), bot, S::FORBIDDEN),
            ("PUT", nobody("/billing"), Some(json!({ "monthly_cap_usd": 5 })), root, S::NOT_FOUND),
            ("PUT", o("/billing"), Some(json!({ "monthly_cap_usd": -1 })), root, S::BAD_REQUEST),
            ("PUT", o("/billing"), Some(json!({ "monthly_cap_usd": "10" })), root, S::BAD_REQUEST),
            ("PUT", o("/billing"), Some(json!({ "on_cap": 3 })), root, S::BAD_REQUEST),
            ("PUT", o("/billing"), Some(json!({ "on_cap": "never" })), root, S::BAD_REQUEST),
        ],
    )
    .await;
    for on_cap in ["byok_fallback", "pause", "standard_only"] {
        let b = ok(&t.a, "PUT", &o("/billing"), Some(json!({ "monthly_cap_usd": 10, "on_cap": on_cap }))).await;
        assert_eq!((b["monthly_cap_usd"].as_f64(), b["on_cap"].as_str(), b["remaining_usd"].as_f64()), (Some(10.0), Some(on_cap), Some(10.0)));
        assert!(b["updated_at"].is_number());
    }
    let b = ok(&t.a, "PUT", &o("/billing"), Some(json!({}))).await;
    assert_eq!((b["monthly_cap_usd"].as_f64(), b["on_cap"].as_str()), (Some(10.0), Some("standard_only")), "an empty body changes nothing");
    let (s, b) = call(&t.a, "PUT", &o("/billing"), Some(json!({ "monthly_cap_usd": null, "on_cap": null })), acme).await;
    assert_eq!(s, S::OK);
    assert!(b["monthly_cap_usd"].is_null() && b["remaining_usd"].is_null(), "null clears the cap");
    assert_eq!(b["on_cap"], "standard_only", "null keeps on_cap");

    // usage.
    let u = ok(&t.a, "GET", &o("/usage"), None).await;
    assert_eq!(u["totals"]["reviews"], 0);
    assert_eq!(u["month"], b["month"]);
    assert_eq!(u["cap"]["on_cap"], "standard_only");
    let old = ok(&t.a, "GET", &o("/usage?month=2020-01"), None).await;
    assert_eq!((old["month"].as_str(), old["recent"].as_array().map(Vec::len)), (Some("2020-01"), Some(0)));
    expect(
        &t.a,
        vec![
            ("GET", o("/usage"), None, acme, S::OK),
            ("GET", o("/usage?month=2020-1"), None, root, S::BAD_REQUEST),
            ("GET", o("/usage?month=abcd-ef"), None, root, S::BAD_REQUEST),
            ("GET", o("/usage?month=2020-01-01"), None, root, S::BAD_REQUEST),
            ("GET", o("/usage"), None, mallory, S::NOT_FOUND),
            ("GET", o("/usage"), None, bot, S::NOT_FOUND),
            ("GET", o("/usage"), None, None, S::NOT_FOUND),
            ("GET", nobody("/usage"), None, root, S::NOT_FOUND),
        ],
    )
    .await;
}

/// Every `@roll` command from someone who can only read (a public repo's reader) is ignored:
/// no reply, no job, no model call, no learning, no pause, nothing resolved. Their comment
/// itself is posted. Writers' `@roll review` (nothing new: skipped), `@roll full review [mode]`,
/// `@roll ignore …` and `@roll remember …` act. (explain, questions, don't flag, pause, resume,
/// review deep, fix and resolve from writers: `roll_commands_in_threads`.)
#[tokio::test]
async fn roll_commands_from_writers_act_and_from_readers_are_ignored() {
    let t = setup_with("public", |_, _| {}).await;
    enable(&t, json!({ "mode": "standard" })).await;
    let c = change(&t, "Cmds", json!({ "src/parse.rs": { "content": PARSE_EDIT } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    roll_reviews(&t, &id, 1).await;
    let tid = threads(&t, &id).await[0]["id"].as_str().unwrap().to_string();
    let jobs_before = jobs_idle(&t, &id).await.len();
    let calls_before = t.m.messages().len();

    let all = [
        "@roll review",
        "@roll review max",
        "@roll full review",
        "@roll full review deep",
        "@roll pause",
        "@roll resume",
        "@roll resolve",
        "@roll explain",
        "@roll fix",
        "@roll don't flag unchecked input",
        "@roll ignore generated code",
        "@roll remember we validate upstream",
        "@roll why is this a bug?",
    ];
    for body in all {
        let (s, v) = call(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": body })), Some(&t.mallory)).await;
        assert_eq!(s, StatusCode::CREATED, "a reader can comment on a public repo: {v}");
    }
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "@roll review" })), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "anonymous callers can't comment at all");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let th = ok(&t.a, "GET", &format!("/v1/code/threads/{tid}"), None).await;
    let comments = th["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1 + all.len(), "no reply from roll");
    assert!(comments[1..].iter().all(|c| c["author"]["kind"] != "roll"));
    assert_eq!(th["resolved"], false);
    assert_eq!(t.m.messages().len(), calls_before, "no model call");
    assert_eq!(ok(&t.a, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await.as_array().unwrap().len(), jobs_before, "no job");
    assert!(ok(&t.a, "GET", "/v1/code/repos/acme/api/learnings", None).await.as_array().unwrap().is_empty(), "no learning");
    assert!(!t.a.code.db.ai_paused(&id).unwrap(), "not paused");

    // A writer: `@roll review` with nothing new since roll's last review is skipped.
    let say = |body: &'static str| {
        let (a, tid) = (&t.a, tid.clone());
        async move { ok(a, "POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": body }))).await["comments"].as_array().unwrap().len() }
    };
    let n = say("@roll review").await;
    assert_eq!(last_body(&thread_with(&t, &tid, n + 1).await), "On it: a review of what changed.");
    let jobs = jobs_idle_n(&t, &id, jobs_before + 1).await;
    assert_eq!(jobs.last().unwrap()["state"], "skipped");
    assert_eq!(jobs.last().unwrap()["requested_by"], "@admin");

    // `@roll full review`: everything again, not incremental, the settings' mode.
    let n = say("@roll full review").await;
    assert_eq!(last_body(&thread_with(&t, &tid, n + 1).await), "On it: a full review.");
    let r = roll_reviews(&t, &id, 2).await;
    let last = r.last().unwrap();
    assert_eq!((last["ai"]["incremental"].as_bool(), last["ai"]["chosen_by"].as_str(), last["ai"]["mode"].as_str()), (Some(false), Some("settings"), Some("standard")));
    let job = jobs_idle_n(&t, &id, jobs_before + 2).await.last().unwrap().clone();
    assert_eq!((job["full"].as_bool(), job["state"].as_str()), (Some(true), Some("done")));

    // `@roll full review max`: a forced mode.
    let n = say("@roll full review max").await;
    assert_eq!(last_body(&thread_with(&t, &tid, n + 1).await), "On it: a full max review.");
    let r = roll_reviews(&t, &id, 3).await;
    let last = r.last().unwrap();
    assert_eq!((last["ai"]["mode"].as_str(), last["ai"]["chosen_by"].as_str(), last["ai"]["incremental"].as_bool()), (Some("max"), Some("command"), Some(false)));
    assert_eq!(last["ai"]["reasons"][0], "max requested");
    let job = jobs_idle_n(&t, &id, jobs_before + 3).await.last().unwrap().clone();
    assert_eq!((job["mode"].as_str(), job["full"].as_bool()), (Some("max"), Some(true)));

    // `ignore …` and `remember …` are learnings tied to the thread.
    let n = say("@roll ignore generated code.").await;
    assert!(last_body(&thread_with(&t, &tid, n + 1).await).contains("I'll remember for future reviews: _Don't flag generated code_"));
    let n = say("@roll remember we validate input upstream").await;
    assert!(last_body(&thread_with(&t, &tid, n + 1).await).contains("_we validate input upstream_"));
    let l = ok(&t.a, "GET", "/v1/code/repos/acme/api/learnings", None).await;
    let mut texts: Vec<&str> = l.as_array().unwrap().iter().map(|x| x["text"].as_str().unwrap()).collect();
    texts.sort();
    assert_eq!(texts, ["Don't flag generated code", "we validate input upstream"]);
    assert!(l.as_array().unwrap().iter().all(|x| x["source"] == "command" && x["thread"] == tid.as_str() && x["path"].is_null()), "{l}");

    // `@roll review` also resumes a paused change.
    let n = say("@roll pause").await;
    thread_with(&t, &tid, n + 1).await;
    assert!(t.a.code.db.ai_paused(&id).unwrap());
    let n = say("@roll review").await;
    thread_with(&t, &tid, n + 1).await;
    assert!(!t.a.code.db.ai_paused(&id).unwrap(), "asking for a review resumes");
}

/// `on_cap` past a hosted monthly cap, each value's behaviour (the concurrent race and the
/// reservation are `concurrent_reviews_and_chats_never_overrun_the_cap`):
/// - `pause`: no model call, a comment review, the job `skipped`, the change stops waiting, nothing metered
/// - `byok_fallback` without a key: the same, and chats answer that the cap is reached
/// - `byok_fallback` with a key: reviews and chats run on the owner's key at BYOK prices
/// - `standard_only`: deep/max run as standard, standard reviews go on past the cap and are charged; chats stop
#[tokio::test]
async fn on_cap_pause_byok_fallback_and_standard_only() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "standard", "required": true })).await;
    let one = |n: u32| json!({ format!("src/cap{n}.rs"): { "content": format!("pub fn cap{n}() {{}}\n") } });

    // pause.
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0, "on_cap": "pause" }))).await;
    let c = change(&t, "paused", one(0)).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert!(r[0]["ai"].is_null() && r[0]["verdict"] == "comment");
    assert!(r[0]["body"].as_str().unwrap().contains("paused for this month") && r[0]["body"].as_str().unwrap().contains("($0.00)"), "{}", r[0]["body"]);
    let j = jobs_idle(&t, &id).await;
    assert_eq!((j[0]["state"].as_str(), j[0]["attempts"].as_u64()), (Some("skipped"), Some(1)));
    let ch = ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None).await;
    assert!(!ch["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "review_pending"), "the change doesn't wait on a paused reviewer");
    assert!(t.m.messages().is_empty(), "no model call");
    assert_eq!(ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await["totals"]["reviews"], 0);

    // byok_fallback without a key: paused the same way; a chat answers that the cap is reached.
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "on_cap": "byok_fallback" }))).await;
    let c = change(&t, "fallback none", one(1)).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert!(r[0]["ai"].is_null() && r[0]["body"].as_str().unwrap().contains("paused for this month"));
    let tid = person_thread(&t, &id, "src/cap1.rs", "@roll explain").await;
    let th = thread_with(&t, &tid, 2).await;
    assert!(last_body(&th).contains("cap ($0.00) is reached"), "{th}");
    assert!(t.m.messages().is_empty(), "still no model call");

    // byok_fallback with a key: the owner's key, BYOK prices, not capped.
    ok(&t.a, "PUT", "/v1/code/owners/acme/review-provider", Some(json!({ "api_key": "sk-ant-owner-fallback-7777" }))).await;
    let c = change(&t, "fallback key", one(2)).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert_eq!(r[0]["ai"]["key"], "byok");
    let tid = person_thread(&t, &id, "src/cap2.rs", "@roll explain").await;
    let th = thread_with(&t, &tid, 2).await;
    assert!(last_body(&th).contains("Here is why"), "{th}");
    assert!(!t.m.messages().is_empty() && t.m.messages().iter().all(|s| s.headers["x-api-key"] == "sk-ant-owner-fallback-7777"));
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    let recent = u["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 2, "{u}");
    assert!(recent.iter().all(|x| x["key_source"] == "byok" && x["capped"] == false));
    assert_eq!(u["by_key"]["byok"]["price_usd"], 0.02, "BYOK review fee, chats free");
    assert!(u["by_key"]["hosted"].is_null());

    // standard_only: max runs as standard, standard goes on past the cap (charged); chats stop.
    ok(&t.a, "DELETE", "/v1/code/owners/acme/review-provider", None).await;
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "on_cap": "standard_only" }))).await;
    enable(&t, json!({ "mode": "max" })).await;
    let c = change(&t, "downgrade max", one(3)).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert_eq!((r[0]["ai"]["mode"].as_str(), r[0]["ai"]["key"].as_str()), (Some("standard"), Some("hosted")));
    assert!(r[0]["ai"]["reasons"].to_string().contains("the monthly cap ($0.00) is reached: standard only"), "{}", r[0]["ai"]["reasons"]);
    assert!(!t.m.messages().iter().any(|s| s.user().contains("checking another reviewer's findings")), "no max verification pass");
    enable(&t, json!({ "mode": "standard" })).await;
    let c = change(&t, "standard goes on", one(4)).await;
    let id = c["id"].as_str().unwrap().to_string();
    let r = roll_reviews(&t, &id, 1).await;
    assert_eq!(r[0]["ai"]["mode"], "standard");
    assert!(!r[0]["ai"]["reasons"].to_string().contains("standard only"), "nothing to downgrade");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["by_key"]["hosted"]["count"], 2);
    assert_eq!(u["by_key"]["hosted"]["price_usd"], 0.5, "charged past the cap");
    assert_eq!(u["cap"]["remaining_usd"], 0.0);
    let before = t.m.messages().len();
    let tid = person_thread(&t, &id, "src/cap4.rs", "@roll explain").await;
    let th = thread_with(&t, &tid, 2).await;
    assert!(last_body(&th).contains("is reached"), "standard_only covers reviews, not chats: {th}");
    assert_eq!(t.m.messages().len(), before);
}

/// A thread a person opens on a change (no roll review needed); returns its id.
async fn person_thread(t: &T, change: &str, path: &str, body: &str) -> String {
    let th = ok(&t.a, "POST", &format!("/v1/code/changes/{change}/threads"), Some(json!({ "path": path, "line": 1, "body": body }))).await;
    th["id"].as_str().unwrap().to_string()
}

/// The job of a review that gave up, and roll's comment saying why.
async fn gave_up(t: &T, change: &str) -> (Value, String) {
    let r = roll_reviews(t, change, 1).await;
    let body = r[0]["body"].as_str().unwrap().to_string();
    assert!(r[0]["ai"].is_null() && body.contains("couldn't review this change"), "{body}");
    assert_eq!(r[0]["verdict"], "comment");
    let j = jobs_idle(t, change).await[0].clone();
    assert_eq!((j["state"].as_str(), j["stage"].as_str()), (Some("failed"), Some("failed")), "{j}");
    let ch = ok(&t.a, "GET", &format!("/v1/code/changes/{change}"), None).await;
    assert!(!ch["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "review_pending"), "a reviewer that gave up isn't owed a review");
    (j, body)
}

/// Provider errors through code-control: a key without permission (403) isn't retried; a
/// provider that stays overloaded (529) is retried by the provider and then by the job (three
/// attempts) before roll gives up; a 400 gives up too. Each gives up with a comment saying why,
/// a `failed` job with the error, and the change no longer waits.
#[tokio::test]
async fn provider_errors_403_persistent_529_and_400() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "standard", "required": true })).await;

    t.m.set(|s, _| (s.path == "/v1/messages").then(|| (403, json!({"type": "error", "error": {"type": "permission_error", "message": "key lacks access"}}))));
    let c = change(&t, "403", json!({ "src/a.rs": { "content": "pub fn a() {}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (j, body) = gave_up(&t, &id).await;
    assert!(body.contains("403") && body.contains("key lacks access"), "{body}");
    assert_eq!(j["attempts"], 1, "a bad key isn't retried");
    assert!(j["error"].as_str().unwrap().contains("key lacks access"));
    assert_eq!(t.m.messages().len(), 1);

    t.m.set(|s, _| (s.path == "/v1/messages").then(|| (529, json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}))));
    let before = t.m.messages().len();
    let c = change(&t, "529", json!({ "src/b.rs": { "content": "pub fn b() {}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (j, body) = gave_up(&t, &id).await;
    assert!(body.contains("kept failing") && body.contains("Overloaded"), "{body}");
    // Retried by the job (exactly 3 attempts: `a_failing_review_runs_three_times`).
    let attempts = j["attempts"].as_u64().unwrap();
    assert_eq!(attempts, 3, "{j}");
    assert_eq!(t.m.messages().len() - before, attempts as usize * 2, "each attempt retries in the provider too");

    t.m.set(|s, _| (s.path == "/v1/messages").then(|| (400, json!({"type": "error", "error": {"type": "invalid_request_error", "message": "messages: bad shape"}}))));
    let c = change(&t, "400", json!({ "src/c.rs": { "content": "pub fn c() {}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (j, body) = gave_up(&t, &id).await;
    assert!(body.contains("400") && body.contains("messages: bad shape"), "{body}");
    assert!(j["error"].as_str().unwrap().contains("bad shape"));
}

/// reviews.md: "A failed attempt is retried twice with backoff (30 s, then ×4)" — three attempts
/// in all (`MAX_ATTEMPTS`), with delays of `retry_ms` then `retry_ms × 4`.
#[tokio::test]
async fn a_failing_review_runs_three_times() {
    let t = setup(|c, _| c.review.retry_ms = 100).await;
    enable(&t, json!({ "mode": "standard" })).await;
    t.m.set(|s, _| (s.path == "/v1/messages").then(|| (529, json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}))));
    let started = std::time::Instant::now();
    let c = change(&t, "529", json!({ "src/b.rs": { "content": "pub fn b() {}\n" } })).await;
    let (j, _) = gave_up(&t, c["id"].as_str().unwrap()).await;
    assert_eq!(j["attempts"], 3);
    assert_eq!(t.m.messages().len(), 3 * 2);
    // 100 ms, then 400 ms between attempts.
    assert!(started.elapsed() >= Duration::from_millis(500), "{:?}", started.elapsed());
}

/// The model declining (`refusal`) or running out of output (`max_tokens`) isn't an error: the
/// review is posted, the files are listed as not reviewed with the reason, and roll doesn't
/// approve what it didn't read.
#[tokio::test]
async fn refusals_and_cut_off_reviews_list_the_files_and_dont_approve() {
    let t = setup(|_, _| {}).await;
    t.m.set(|s, _| {
        let u = s.user();
        if u.contains("## src/evil.rs") {
            return Some((200, json!({"model": "claude-opus-5-5", "stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}, "content": [], "usage": {"input_tokens": 5}})));
        }
        if u.contains("## src/long.rs") {
            return Some((200, json!({"model": "claude-opus-5-5", "stop_reason": "max_tokens", "content": [{"type": "text", "text": "{\"walk"}], "usage": {"output_tokens": 16000}})));
        }
        None
    });
    enable(&t, json!({ "mode": "standard" })).await;
    for (path, reason) in [("src/evil.rs", "declined by the model (cyber)"), ("src/long.rs", "didn't fit in the model's output")] {
        let c = change(&t, path, json!({ path.to_string(): { "content": "pub fn x() {}\n" } })).await;
        let id = c["id"].as_str().unwrap().to_string();
        let r = roll_reviews(&t, &id, 1).await;
        let ai = &r[0]["ai"];
        assert_eq!(r[0]["verdict"], "comment", "{}", r[0]["body"]);
        assert!(r[0]["body"].as_str().unwrap().contains("weren't reviewed") && r[0]["body"].as_str().unwrap().contains("Not reviewed"), "{}", r[0]["body"]);
        let skipped = ai["skipped_files"].as_array().unwrap();
        assert_eq!(skipped.len(), 1, "{ai}");
        assert_eq!(skipped[0]["path"], path);
        assert!(skipped[0]["reason"].as_str().unwrap().contains(reason), "{ai}");
        assert_eq!(ai["details"]["files_reviewed"], 0);
        assert!(threads(&t, &id).await.is_empty());
        assert_eq!(jobs_idle(&t, &id).await[0]["state"], "done", "not an error");
    }
}

/// Configuration errors aren't retried: a hosted provider that can't be built (an
/// OpenAI-compatible one without a base URL), and an owner's sealed key the server can't open
/// (no `ROLL_SECRET_KEY` now, or a different one). An owner with a key of their own never falls
/// back to the hosted key. A chat without any provider answers that in the thread.
#[tokio::test]
async fn config_and_key_errors_give_up_without_retrying() {
    // A hosted OpenAI-compatible provider missing its base URL.
    let bad = setup(|c, _| c.review.hosted = Some(Hosted { provider: "openai_compatible".into(), api_key: "sk-x".into(), base_url: None, model: Some("m".into()), secondary_model: None })).await;
    enable(&bad, json!({ "mode": "standard", "required": true })).await;
    assert!(ok(&bad.a, "GET", "/v1/code/repos/acme/api/reviewer", None).await["provider"].is_null());
    let c = change(&bad, "cfg", json!({ "src/a.rs": { "content": "pub fn a() {}\n" } })).await;
    let (j, _) = gave_up(&bad, c["id"].as_str().unwrap()).await;
    assert_eq!(j["attempts"], 1);

    // A sealed key, then the server's secret is gone or changed.
    for secret in [None, Some("a-different-secret-of-at-least-32-chars")] {
        let t = setup(move |c, _| c.review.secret_key = secret.map(str::to_string)).await;
        let owner = t.a.code.db.owner("acme").unwrap().unwrap();
        let row = code_control::ai::store::OwnerProvider {
            provider: "anthropic".into(),
            base_url: None,
            model: None,
            secondary_model: None,
            key_sealed: code_review::secret::Sealer::new(SECRET).unwrap().seal(&owner.id, "sk-ant-sealed-5555"),
            key_last4: "5555".into(),
            updated_at: 0,
        };
        t.a.code.db.set_owner_provider(&owner.id, Some(&row)).unwrap();
        assert_eq!(ok(&t.a, "GET", "/v1/code/owners/acme/review-provider", None).await["key_last4"], "5555", "the key's shape is still shown");
        enable(&t, json!({ "mode": "standard", "required": true })).await;
        let c = change(&t, "sealed", json!({ "src/a.rs": { "content": "pub fn a() {}\n" } })).await;
        let (j, body) = gave_up(&t, c["id"].as_str().unwrap()).await;
        assert_eq!(j["attempts"], 1, "{secret:?}");
        if secret.is_none() {
            assert!(body.contains("ROLL_SECRET_KEY"), "{body}");
        }
        assert!(t.m.messages().is_empty(), "never falls back to the hosted key: {secret:?}");
    }

    // A chat with no provider at all.
    let none = setup(|c, _| c.review.hosted = None).await;
    let c = change(&none, "q", json!({ "src/a.rs": { "content": "pub fn a() {}\n" } })).await;
    let tid = person_thread(&none, c["id"].as_str().unwrap(), "src/a.rs", "@roll what does this do?").await;
    let th = thread_with(&none, &tid, 2).await;
    assert!(last_body(&th).contains("no model provider"), "{th}");
    let jobs = jobs_idle(&none, c["id"].as_str().unwrap()).await;
    assert_eq!((jobs[0]["kind"].as_str(), jobs[0]["state"].as_str()), (Some("command"), Some("done")));
}

/// A chat (`kind: "command"`) whose provider rejects the key fails at once, not retried.
#[tokio::test]
async fn a_chat_with_a_bad_key_fails_without_retrying() {
    let t = setup(|_, _| {}).await;
    t.m.set(|s, _| (s.path == "/v1/messages").then(|| (401, json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}))));
    let c = change(&t, "q", json!({ "src/a.rs": { "content": "pub fn a() {}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    person_thread(&t, &id, "src/a.rs", "@roll explain").await;
    let jobs = jobs_idle_n(&t, &id, 1).await;
    assert_eq!((jobs[0]["kind"].as_str(), jobs[0]["state"].as_str(), jobs[0]["stage"].as_str()), (Some("command"), Some("failed"), Some("failed")));
    assert_eq!(jobs[0]["attempts"], 1);
    assert!(jobs[0]["error"].as_str().unwrap().contains("invalid x-api-key"));
    assert_eq!(t.m.messages().len(), 1);
    assert_eq!(ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await["totals"]["chats"], 0, "nothing metered");
    // The thread says why instead of going quiet.
    let th = threads(&t, &id).await.into_iter().find(|th| th["comments"][0]["body"] == "@roll explain").unwrap();
    let last = th["comments"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["author"]["kind"], "roll", "{th}");
    assert!(last["body"].as_str().unwrap().starts_with("I couldn't answer:") && last["body"].as_str().unwrap().contains("invalid x-api-key"), "{last}");
}

/// Modes, speeds and profiles through code-control: `auto` picks `max` for `max_paths` (with
/// its verification pass); `thorough` raises effort to xhigh and verifies even standard
/// reviews; `effort` overrides the table; a failing classifier falls back to the heuristic;
/// `assertive` posts the nits `chill` drops.
#[tokio::test]
async fn modes_speeds_effort_classifier_fallback_and_profiles() {
    let t = setup(|_, _| {}).await;
    let latest_review = |t: &T| t.m.messages().into_iter().rev().find(|s| s.user().contains("# Diff")).unwrap();
    let verifications = |t: &T| t.m.messages().iter().filter(|s| s.user().contains("checking another reviewer's findings")).count();

    // auto → max (max_paths), verified.
    enable(&t, json!({ "auto": { "max_paths": ["src/parse.rs"] } })).await;
    let c = change(&t, "max", json!({ "src/parse.rs": { "content": PARSE_EDIT } })).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!((r[0]["ai"]["mode"].as_str(), r[0]["ai"]["chosen_by"].as_str()), (Some("max"), Some("auto-heuristic")));
    assert!(r[0]["ai"]["reasons"].to_string().contains("max_paths"), "{}", r[0]["ai"]["reasons"]);
    assert_eq!(latest_review(&t).body["output_config"]["effort"], "high");
    assert_eq!(verifications(&t), 1);

    // thorough: xhigh, and a verification pass even in standard mode.
    enable(&t, json!({ "mode": "standard", "speed": "thorough", "auto": {} })).await;
    let c = change(&t, "thorough", json!({ "src/server.rs": { "content": "pub fn handle() { thorough() }\n" } })).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!((r[0]["ai"]["speed"].as_str(), r[0]["ai"]["details"]["effort"].as_str()), (Some("thorough"), Some("xhigh")), "{}", r[0]["ai"]);
    assert!(r[0]["body"].as_str().unwrap().contains("· thorough ·"));
    assert_eq!(latest_review(&t).body["output_config"]["effort"], "xhigh");
    assert_eq!(verifications(&t), 2);

    // effort overrides the table (deep × balanced would be high).
    enable(&t, json!({ "mode": "deep", "speed": "balanced", "effort": "low" })).await;
    let c = change(&t, "effort", json!({ "src/server.rs": { "content": "pub fn handle() { low() }\n" } })).await;
    roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!(latest_review(&t).body["output_config"]["effort"], "low");

    // The classifier failing: the heuristic decides, and the review goes on.
    t.m.set(|s, _| s.user().contains("how deep a code review").then(|| (500, json!({"error": {"message": "down"}}))));
    enable(&t, json!({ "mode": "auto", "effort": null, "auto": { "classifier": "model" } })).await;
    let c = change(&t, "cls", json!({ "src/server.rs": { "content": "pub fn handle() { cls() }\n" } })).await;
    let r = roll_reviews(&t, c["id"].as_str().unwrap(), 1).await;
    assert_eq!((r[0]["ai"]["mode"].as_str(), r[0]["ai"]["chosen_by"].as_str()), (Some("standard"), Some("auto-heuristic")), "{}", r[0]["ai"]);
    assert!(t.m.messages().iter().any(|s| s.user().contains("how deep a code review")), "the classifier was asked");

    // Profiles: a nit is dropped by chill (approve), posted by assertive (comment).
    t.m.set(|s, _| {
        if !s.user().contains("# Diff") {
            return None;
        }
        let (status, mut v) = default_answer(s);
        let mut a: Value = serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        for f in a["findings"].as_array_mut().unwrap() {
            f["severity"] = json!("nit");
        }
        v["content"][0]["text"] = json!(a.to_string());
        Some((status, v))
    });
    for (profile, verdict, posted) in [("chill", "approve", 0), ("assertive", "comment", 1)] {
        enable(&t, json!({ "mode": "standard", "auto": {}, "profile": profile })).await;
        let c = change(&t, profile, json!({ "src/server.rs": { "content": format!("pub fn handle() {{ {profile}() }}\n") } })).await;
        let id = c["id"].as_str().unwrap().to_string();
        let r = roll_reviews(&t, &id, 1).await;
        assert_eq!(r[0]["verdict"], verdict, "{profile}: {}", r[0]["body"]);
        assert_eq!(threads(&t, &id).await.len(), posted, "{profile}");
    }
}

/// A job's states and stages while it runs: `queued` before the runner, then `running` through
/// `reviewing` (with progress) and `verifying` (max), then `done` with its review. (`failed`:
/// `gave_up`; `skipped`: `change_reviewer_routes_answer_every_status_and_jobs_queue`.)
#[tokio::test]
async fn jobs_go_queued_running_reviewing_verifying_done() {
    let t = setup(|_, _| {}).await;
    enable(&t, json!({ "mode": "max" })).await;
    t.m.delay_ms.store(400, Ordering::SeqCst);
    let c = change(&t, "stages", json!({ "src/parse.rs": { "content": PARSE_EDIT } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut last = Value::Null;
    for _ in 0..400 {
        if let Some(j) = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await.as_array().unwrap().first().cloned() {
            let pair = (j["state"].as_str().unwrap().to_string(), j["stage"].as_str().unwrap().to_string());
            if seen.last() != Some(&pair) {
                seen.push(pair);
            }
            if j["stage"] == "verifying" {
                assert_eq!(j["progress"]["groups_done"], j["progress"]["groups_total"], "every group is done before verification");
            }
            last = j;
            if last["state"] == "done" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    t.m.delay_ms.store(0, Ordering::SeqCst);
    let has = |s: &str, g: &str| seen.iter().any(|(a, b)| a == s && b == g);
    assert!(has("running", "reviewing") && has("running", "verifying") && has("done", "done"), "{seen:?}");
    let order: Vec<&str> = seen.iter().map(|(_, g)| g.as_str()).collect();
    let pos = |g: &str| order.iter().position(|x| *x == g).unwrap();
    assert!(pos("reviewing") < pos("verifying") && pos("verifying") < pos("done"), "{order:?}");
    let r = roll_reviews(&t, &id, 1).await;
    assert_eq!(last["review_id"], r[0]["id"]);
    assert!(last["error"].is_null());

    // Before a runner starts, a job is queued.
    let idle = app_with(|c| {
        c.admin_token = Some(ROOT.into());
        c.review.hosted = Some(Hosted { provider: "anthropic".into(), api_key: "k".into(), base_url: Some(t.m.url.clone()), model: None, secondary_model: None });
    });
    ok(&idle, "POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    ok(&idle, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api" }))).await;
    ok(&idle, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "base", "files": { "a.rs": { "content": "x\n" } } }))).await;
    ok(&idle, "PATCH", "/v1/code/repos/acme/api/reviewer", Some(json!({ "enabled": true }))).await;
    let c = ok(&idle, "POST", "/v1/code/repos/acme/api/changes", Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "a.rs": { "content": "y\n" } } }))).await;
    let mut j = Value::Null;
    for _ in 0..100 {
        if let Some(x) = ok(&idle, "GET", &format!("/v1/code/changes/{}/ai-jobs", c["id"].as_str().unwrap()), None).await.as_array().unwrap().first() {
            j = x.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!((j["state"].as_str(), j["stage"].as_str(), j["attempts"].as_u64()), (Some("queued"), Some("queued"), Some(0)), "{j}");
    assert!(j["progress"].is_null() && j["review_id"].is_null() && j["mode"].is_null() && j["kind"] == "review");
    assert_eq!(ok(&idle, "GET", "/v1/code/repos/acme/api/reviewer", None).await["jobs"]["queued"], 1);
}

/// A deep review waits while the repo's codebase index is first built, and says so: the job's
/// stage is `indexing` until the build is done.
#[tokio::test]
async fn a_deep_review_reports_indexing_while_the_index_is_built() {
    let t = setup(|_, _| {}).await;
    let mut files = serde_json::Map::new();
    for i in 0..1500 {
        files.insert(format!("gen/f{i}.rs"), json!({ "content": format!("pub fn generated_{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n") }));
    }
    ok(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "many files", "files": Value::Object(files) }))).await;
    enable(&t, json!({ "mode": "deep" })).await;
    let c = change(&t, "Deep", json!({ "src/parse.rs": { "content": PARSE_EDIT } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let mut stages = Vec::new();
    for _ in 0..2000 {
        if let Some(j) = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await.as_array().unwrap().first() {
            let s = j["stage"].as_str().unwrap().to_string();
            if stages.last() != Some(&s) {
                stages.push(s);
            }
            if j["state"] == "done" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(stages.contains(&"indexing".to_string()), "{stages:?}");
    let r = roll_reviews(&t, &id, 1).await;
    assert!(!r[0]["ai"]["reasons"].to_string().contains("still being built"), "it waited for the index");
    assert!(t.m.messages().last().unwrap().user().contains("# Repository context"));
}

/// A query string that doesn't parse is a JSON 400 on the reviewer's routes too.
#[tokio::test]
async fn bad_query_strings_are_json_400s() {
    let t = setup(|_, _| {}).await;
    for path in ["/v1/code/repos/acme/api/search?q=x&limit=lots", "/v1/code/repos/acme/api/search?q=x&regex=maybe", "/v1/code/owners/acme/usage?month=1&month=2"] {
        let (s, v) = call(&t.a, "GET", path, None, Some(ROOT)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{path}: {v}");
        assert!(v["error"].is_string(), "{path}: {v}");
    }
}

/// Wait (5 s at most) until `cond` holds.
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

/// `concurrent_reviews_and_chats_never_overrun_the_cap` with the reviews on two nodes (one at
/// a time each): what one node's running review holds counts in the other's cap check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reviews_on_two_nodes_never_overrun_the_cap() {
    let t = setup(|c, _| c.review.concurrency = 1).await;
    let b = second_node(&t, |c| c.review.concurrency = 1);
    enable(&t, json!({ "mode": "standard" })).await;
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0.5, "on_cap": "pause" }))).await;
    t.m.delay_ms.store(2_000, Ordering::SeqCst);
    let mut ids = Vec::new();
    for n in 0..4 {
        let c = change(&t, &format!("c{n}"), json!({ format!("src/c{n}.rs"): { "content": format!("pub fn c{n}() {{}}\n") } })).await;
        ids.push(c["id"].as_str().unwrap().to_string());
    }
    let owner = t.a.code.owner_or_404("acme").unwrap().id;
    // Two reviews hold the cap at once: each node runs one at a time, so one on each.
    eventually("both nodes hold a review's price", || b.code.reserved_usd(&owner) == 0.5).await;
    assert_eq!(t.a.code.reserved_usd(&owner), 0.5, "seen the same from either node");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.0, "nothing metered yet: {u}");
    let mut reviewed = 0;
    for id in &ids {
        let r = roll_reviews(&t, id, 1).await;
        if !r[0]["ai"].is_null() {
            reviewed += 1;
        } else {
            assert!(r[0]["body"].as_str().unwrap().contains("paused for this month"), "{}", r[0]["body"]);
        }
    }
    t.m.delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(reviewed, 2, "exactly two fit under the cap, across both nodes");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.5, "never over the cap: {u}");
    assert_eq!(t.m.messages().len(), 2, "no model call for capped reviews");
    eventually("reservations released", || t.a.code.reserved_usd(&owner) == 0.0 && b.code.reserved_usd(&owner) == 0.0).await;
}

/// `concurrent_public_reviews_never_share_the_last_free_one` with the reviews on two nodes: a
/// free review one node's job claimed can't be claimed by the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_public_reviews_on_two_nodes_never_share_the_last_free_one() {
    let t = setup_with("public", |c, _| {
        c.review.concurrency = 1;
        c.review.rates.public_free_reviews = 1;
    })
    .await;
    let b = second_node(&t, |c| {
        c.review.concurrency = 1;
        c.review.rates.public_free_reviews = 1;
    });
    enable(&t, json!({ "mode": "standard" })).await;
    ok(&t.a, "PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 0.25, "on_cap": "pause" }))).await;
    t.m.delay_ms.store(1_500, Ordering::SeqCst);
    let mut ids = Vec::new();
    for n in 0..3 {
        let c = change(&t, &format!("p{n}"), json!({ format!("src/p{n}.rs"): { "content": format!("pub fn p{n}() {{}}\n") } })).await;
        ids.push(c["id"].as_str().unwrap().to_string());
    }
    let owner = t.a.code.owner_or_404("acme").unwrap().id;
    // The free one and a paid one held at once: one job per node.
    eventually("a free and a paid review held at once", || (t.a.code.reserved_usd(&owner), b.code.reserved_free(&owner)) == (0.25, 1)).await;
    let mut prices = Vec::new();
    let mut paused = 0;
    for id in &ids {
        let r = roll_reviews(&t, id, 1).await;
        match r[0]["ai"]["price_usd"].as_f64() {
            Some(p) => prices.push(p),
            None => {
                assert!(r[0]["body"].as_str().unwrap().contains("paused for this month"), "{}", r[0]["body"]);
                paused += 1;
            }
        }
    }
    t.m.delay_ms.store(0, Ordering::SeqCst);
    prices.sort_by(f64::total_cmp);
    assert_eq!((prices, paused), (vec![0.0, 0.25], 1), "one free, one paid, one over the cap");
    let u = ok(&t.a, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(u["totals"]["price_usd"], 0.25, "never over the cap: {u}");
    assert_eq!(u["totals"]["free_reviews"], 1, "the free review was used once");
    eventually("reservations released", || (b.code.reserved_usd(&owner), t.a.code.reserved_free(&owner)) == (0.0, 0)).await;
}

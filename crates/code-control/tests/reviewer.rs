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

async fn setup_with(visibility: &str, f: impl FnOnce(&mut code_control::Config, &str)) -> T {
    let m = mock().await;
    let url = m.url.clone();
    let a = app_with(|c| {
        c.admin_token = Some(ROOT.into());
        c.review.hosted = Some(Hosted { provider: "anthropic".into(), api_key: "sk-hosted".into(), base_url: Some(url.clone()), model: None, secondary_model: None });
        c.review.retry = fast_retry();
        c.review.retry_ms = 50;
        c.review.secret_key = Some(SECRET.into());
        c.review.anthropic_url = url.clone();
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
    let db = std::fs::read(t.a.dir.path().join("code").join("control.db")).unwrap();
    let wal = std::fs::read(t.a.dir.path().join("code").join("control.db-wal")).unwrap_or_default();
    let has = |b: &[u8]| b.windows(b"owner-key".len()).any(|w| w == b"owner-key");
    assert!(!has(&db) && !has(&wal));
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
    t.m.delay_ms.store(300, Ordering::SeqCst);
    let mut ids = Vec::new();
    for n in 0..4 {
        let c = change(&t, &format!("c{n}"), json!({ format!("src/c{n}.rs"): { "content": format!("pub fn c{n}() {{}}\n") } })).await;
        ids.push(c["id"].as_str().unwrap().to_string());
    }
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
    assert_eq!(t.a.code.ai.reserved_usd(&t.a.code.owner_or_404("acme").unwrap().id), 0.0, "reservations released");

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

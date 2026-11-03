//! The reviewer against mock Anthropic and OpenAI-compatible servers: exact request bodies and
//! headers, retries, chunking, refusals, verification, streaming. No test calls a real API.

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use code_review::engine::{ContextBlock, Existing, Limits};
use code_review::provider::{Anthropic, OpenAiCompatible, RetryPolicy, Stop};
use code_review::settings::{Profile, ReviewerSettings};
use code_review::{Engine, FileInput, Mode, Partial, Provider, ProviderError, ReviewInput, Speed, prompt, schema};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Value,
    at: Instant,
}

type Script = Arc<dyn Fn(&str, &Value, usize) -> (u16, Vec<(&'static str, String)>, Value) + Send + Sync>;

struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn mock(script: Script) -> Mock {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let s2 = seen.clone();
    let app = Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
        let seen = s2.clone();
        let script = script.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let n = {
                let mut s = seen.lock();
                s.push(Seen { path: uri.path().to_string(), headers, body: body.clone(), at: Instant::now() });
                s.iter().filter(|x| x.path == uri.path()).count()
            };
            // Slow down /v1/messages a little so concurrency is observable.
            if uri.path().ends_with("/messages") || uri.path().ends_with("/completions") {
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
            let (status, hs, v) = script(uri.path(), &body, n);
            let mut r: Response = (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response();
            for (k, val) in hs {
                r.headers_mut().insert(k, val.parse().unwrap());
            }
            r
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Mock { url, seen }
}

fn fast_retry() -> RetryPolicy {
    RetryPolicy { attempts: 3, base: Duration::from_millis(10), max_delay: Duration::from_secs(2), timeout: Duration::from_secs(10) }
}

fn ok(json: Value, usage: Value) -> (u16, Vec<(&'static str, String)>, Value) {
    (200, vec![], json!({ "model": "claude-opus-5-5", "stop_reason": "end_turn", "content": [{"type": "text", "text": json.to_string()}], "usage": usage }))
}

fn review_json(path: &str, line: u64) -> Value {
    json!({
        "walkthrough": format!("Changes {path}."),
        "files": [{"path": path, "summary": "edited"}],
        "diagram": null,
        "findings": [{"path": path, "side": "new", "start_line": line, "line": line, "severity": "bug", "title": format!("Bug in {path}"),
                      "body": "Panics on empty input.", "suggestion": "let x = 1;", "confidence": 0.9}],
        "notes": []
    })
}

/// The review the mock gives for whatever files the request contains.
fn answer_for(body: &Value) -> Value {
    let user = body["messages"][0]["content"][0]["text"].as_str().or_else(|| body["messages"][1]["content"].as_str()).unwrap_or("");
    let paths: Vec<String> = user.lines().filter_map(|l| l.strip_prefix("## ")).filter(|l| l.contains(" (")).map(|l| l.split(" (").next().unwrap().to_string()).collect();
    let findings: Vec<Value> = paths.iter().map(|p| review_json(p, 2)["findings"][0].clone()).collect();
    json!({
        "walkthrough": format!("Reviewed {}.", paths.join(", ")),
        "files": paths.iter().map(|p| json!({"path": p, "summary": "edited"})).collect::<Vec<_>>(),
        "diagram": null, "findings": findings, "notes": []
    })
}

fn file(path: &str, lines: usize) -> FileInput {
    let mut patch = format!("--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,{} @@\n a\n", lines + 1);
    for i in 0..lines {
        patch.push_str(&format!("+line {i} of {path}\n"));
    }
    FileInput { path: path.into(), status: "modified".into(), patch, content: Some(format!("a\n{}", (0..lines).map(|i| format!("line {i} of {path}\n")).collect::<String>())) }
}

fn input(files: Vec<FileInput>, mode: Mode, speed: Speed) -> ReviewInput {
    let settings = ReviewerSettings { prompt: Some("Care about tenancy.".into()), ..Default::default() };
    let system = prompt::system(&prompt::RepoPrompt { settings: &settings, prompt_file: None, guidelines: &[], learnings: &[] });
    ReviewInput {
        title: "Add x".into(),
        body: "Why: y".into(),
        files,
        skipped: vec![],
        mode,
        speed,
        incremental: false,
        system,
        context: vec![],
        existing: vec![],
        settings,
    }
}

fn anthropic(url: &str) -> Arc<dyn Provider> {
    Arc::new(Anthropic::new("sk-ant-test", None).with_base_url(url).with_retry(fast_retry()))
}

#[tokio::test]
async fn anthropic_request_shape_and_cache_prefix() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 1200}));
        }
        ok(answer_for(body), json!({"input_tokens": 300, "output_tokens": 50, "cache_creation_input_tokens": 1000, "cache_read_input_tokens": 0}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), Some("claude-haiku-5-5".into()));
    let i = input(vec![file("src/a.rs", 3)], Mode::Standard, Speed::Balanced);
    let r1 = e.review(&i, None).await.unwrap();
    let r2 = e.review(&i, None).await.unwrap();
    assert_eq!(r1.findings.len(), 1);
    assert_eq!(r1.verdict, "request_changes");
    assert_eq!(r1.findings[0].suggestion.as_deref(), Some("let x = 1;"));
    assert_eq!(r2.findings.len(), 1);
    let seen = m.seen.lock().clone();
    let msgs: Vec<&Seen> = seen.iter().filter(|s| s.path == "/v1/messages").collect();
    assert_eq!(msgs.len(), 2);
    for s in &msgs {
        assert_eq!(s.headers["x-api-key"], "sk-ant-test");
        assert_eq!(s.headers["anthropic-version"], "2023-06-01");
        assert_eq!(s.headers["anthropic-beta"], "server-side-fallback-2026-07-01");
        assert!(s.headers["content-type"].to_str().unwrap().starts_with("application/json"));
        let b = &s.body;
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["max_tokens"], 16000);
        assert_eq!(b["fallbacks"], "default");
        assert_eq!(b["output_config"]["effort"], "medium");
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert!(schema::check(&b["output_config"]["format"]["schema"]).is_empty());
        assert!(b.get("thinking").is_none(), "never configure thinking");
        assert!(b.get("tool_choice").is_none() && b.get("tools").is_none());
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "no assistant prefill");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(b["system"].as_array().unwrap().len(), 2);
        assert!(b["system"][0].get("cache_control").is_none());
        assert_eq!(b["system"][1]["cache_control"]["type"], "ephemeral");
        assert!(b["system"][1]["text"].as_str().unwrap().contains("Care about tenancy."));
        // The diff is in the user message, numbered.
        assert!(msgs[0]["content"][0]["text"].as_str().unwrap().contains("+     2 | line 0 of src/a.rs"));
    }
    // Byte-identical prefix across reviews.
    assert_eq!(msgs[0].body["system"], msgs[1].body["system"]);
    // count_tokens: same model/system/messages, no output_config, no fallbacks, no beta header.
    let counts: Vec<&Seen> = seen.iter().filter(|s| s.path == "/v1/messages/count_tokens").collect();
    assert_eq!(counts.len(), 2);
    assert!(counts[0].body.get("fallbacks").is_none() && counts[0].body.get("output_config").is_none());
    assert!(counts[0].headers.get("anthropic-beta").is_none());
    assert_eq!(counts[0].body["system"], msgs[0].body["system"]);
    let u = r1.total_usage();
    assert_eq!((u.input_tokens, u.output_tokens, u.cache_write_tokens), (300, 50, 1000));
    assert_eq!(r1.usage[0].model, "claude-opus-5-5");
}

#[tokio::test]
async fn retries_429_with_retry_after_but_not_400() {
    let m = mock(Arc::new(|path, body, n| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        if n == 1 {
            return (429, vec![("retry-after", "1".into())], json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}));
        }
        if n == 2 {
            return (529, vec![], json!({"type": "error", "error": {"type": "overloaded_error", "message": "overloaded"}}));
        }
        ok(answer_for(body), json!({}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), None);
    let t = Instant::now();
    let r = e.review(&input(vec![file("a.rs", 1)], Mode::Standard, Speed::Balanced), None).await.unwrap();
    assert_eq!(r.findings.len(), 1);
    assert!(t.elapsed() >= Duration::from_secs(1), "retry-after honoured");
    let msgs: Vec<Seen> = m.seen.lock().iter().filter(|s| s.path == "/v1/messages").cloned().collect();
    assert_eq!(msgs.len(), 3);
    assert!(msgs[1].at - msgs[0].at >= Duration::from_millis(950));

    let bad = mock(Arc::new(|_, _, _| (400, vec![], json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad schema"}})))).await;
    let e = Engine::new(anthropic(&bad.url), None);
    let err = e.review(&input(vec![file("a.rs", 1)], Mode::Standard, Speed::Balanced), None).await.unwrap_err();
    assert!(err.to_string().contains("400") && err.to_string().contains("bad schema"), "{err}");
    let n = bad.seen.lock().iter().filter(|s| s.path == "/v1/messages").count();
    assert_eq!(n, 1, "400 is not retried");

    let down = mock(Arc::new(|_, _, _| (503, vec![], json!({"error": {"message": "down"}})))).await;
    let p = Anthropic::new("k", None).with_base_url(&down.url).with_retry(fast_retry());
    let req = code_review::provider::Request {
        model: String::new(),
        system: vec![],
        user: "x".into(),
        schema: schema::reply(),
        schema_name: "reply",
        effort: code_review::Effort::Low,
        max_tokens: 10,
    };
    assert!(matches!(p.complete(&req).await, Err(ProviderError::Exhausted { attempts: 3, .. })));
}

#[tokio::test]
async fn chunks_by_token_count_and_lists_skipped_files() {
    // The counter says anything with 3+ files is too big; one file is too big on its own.
    let m = mock(Arc::new(|path, body, _| {
        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("");
        let files = user.lines().filter(|l| l.starts_with("## ") && l.contains(" (")).count() as u64;
        if path.ends_with("count_tokens") {
            let huge = user.contains("huge.rs");
            return (200, vec![], json!({"input_tokens": if huge { 500_000 } else { files * 40_000 }}));
        }
        ok(answer_for(body), json!({"input_tokens": 1}))
    }))
    .await;
    let mut e = Engine::new(anthropic(&m.url), None);
    e.limits = Limits { group_tokens: 60_000, max_file_tokens: 150_000, ..Limits::default() };
    let mut files: Vec<FileInput> = (0..5).map(|i| file(&format!("f{i}.rs"), 2)).collect();
    files.push(file("huge.rs", 2));
    let mut i = input(files, Mode::Standard, Speed::Balanced);
    i.skipped.push(code_review::Skipped { path: "Cargo.lock".into(), reason: "ignored".into() });
    let r = e.review(&i, None).await.unwrap();
    let mut reviewed: Vec<&str> = r.findings.iter().map(|f| f.path.as_str()).collect();
    reviewed.sort();
    assert_eq!(reviewed, ["f0.rs", "f1.rs", "f2.rs", "f3.rs", "f4.rs"]);
    assert!(r.skipped.iter().any(|s| s.path == "huge.rs" && s.reason.contains("too large")), "{:?}", r.skipped);
    assert!(r.skipped.iter().any(|s| s.path == "Cargo.lock"));
    assert_eq!(r.files_reviewed, 5);
    // Every messages request had at most 2 files.
    for s in m.seen.lock().iter().filter(|s| s.path == "/v1/messages") {
        let user = s.body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(user.lines().filter(|l| l.starts_with("## ") && l.contains(" (")).count() <= 2);
    }
}

#[tokio::test]
async fn max_tokens_splits_and_refusals_skip() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("");
        let files = user.lines().filter(|l| l.starts_with("## ") && l.contains(" (")).count();
        if files > 1 {
            return (200, vec![], json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "{\"walk"}], "usage": {"output_tokens": 16000}}));
        }
        if user.contains("## evil.rs") {
            return (200, vec![], json!({"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}, "content": [], "usage": {"input_tokens": 5}}));
        }
        if user.contains("## long.rs") {
            return (200, vec![], json!({"stop_reason": "max_tokens", "content": [], "usage": {}}));
        }
        ok(answer_for(body), json!({}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), None);
    let r = e.review(&input(vec![file("a.rs", 1), file("b.rs", 1), file("evil.rs", 1), file("long.rs", 1)], Mode::Standard, Speed::Balanced), None).await.unwrap();
    let mut got: Vec<&str> = r.findings.iter().map(|f| f.path.as_str()).collect();
    got.sort();
    assert_eq!(got, ["a.rs", "b.rs"]);
    assert!(r.skipped.iter().any(|s| s.path == "evil.rs" && s.reason == "declined by the model (cyber)"), "{:?}", r.skipped);
    assert!(r.skipped.iter().any(|s| s.path == "long.rs" && s.reason.contains("didn't fit")));
    // Usage of the cut-off attempts is still counted.
    assert!(r.total_usage().output_tokens >= 16000);
}

#[tokio::test]
async fn fast_runs_groups_in_parallel_and_streams() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        ok(answer_for(body), json!({}))
    }))
    .await;
    let mut e = Engine::new(anthropic(&m.url), Some("claude-haiku-5-5".into()));
    // One file per group.
    e.limits.group_tokens = 50;
    let files: Vec<FileInput> = (0..8).map(|i| file(&format!("p{i}.rs"), 2)).collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let t = Instant::now();
    let r = e.review(&input(files, Mode::Deep, Speed::Fast), Some(tx)).await.unwrap();
    let took = t.elapsed();
    assert_eq!(r.findings.len(), 8);
    let (mut streamed, mut groups, mut done) = (0, None, Vec::new());
    while let Ok(p) = rx.try_recv() {
        match p {
            Partial::Findings(f) => streamed += f.len(),
            Partial::Groups(n) => groups = Some(n),
            Partial::GroupDone(n, total) => done.push((n, total)),
            Partial::Verifying => panic!("fast never verifies"),
        }
    }
    assert_eq!(streamed, 8);
    // Progress: the group count first, then each group as it finishes.
    assert_eq!(groups, Some(8));
    assert_eq!(done.len(), 8);
    assert!(done.iter().all(|(_, t)| *t == 8));
    let mut ns: Vec<usize> = done.iter().map(|(n, _)| *n).collect();
    ns.sort();
    assert_eq!(ns, (1..=8).collect::<Vec<_>>());
    // 8 requests × 60 ms at concurrency 4 ≈ 120 ms, sequential would be ≥ 480 ms.
    assert!(took < Duration::from_millis(450), "{took:?}");
    let msgs: Vec<Seen> = m.seen.lock().iter().filter(|s| s.path == "/v1/messages").cloned().collect();
    assert_eq!(msgs.len(), 8, "fast never verifies");
    for s in &msgs {
        assert_eq!(s.body["model"], "claude-haiku-5-5");
        assert_eq!(s.body["output_config"]["effort"], "low");
        assert!(s.body.get("fallbacks").is_none() && s.headers.get("anthropic-beta").is_none(), "no server-side fallback on Haiku");
        assert!(s.body["messages"][0]["content"][0]["text"].as_str().unwrap().contains("Set `diagram` to null"));
    }
}

#[tokio::test]
async fn max_mode_verifies_and_uses_context() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("");
        if user.contains("checking another reviewer's findings") {
            // Confirm only finding 0.
            return ok(json!({"results": [{"index": 0, "confirmed": true, "reason": "real"}, {"index": 1, "confirmed": false, "reason": "handled"}]}), json!({}));
        }
        ok(answer_for(body), json!({}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), None);
    let mut i = input(vec![file("a.rs", 2), file("b.rs", 2)], Mode::Max, Speed::Balanced);
    i.context = vec![ContextBlock { title: "Callers of parse".into(), text: "src/main.rs:10 parse(x)".into() }];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let r = e.review(&i, Some(tx)).await.unwrap();
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.findings[0].path, "a.rs");
    assert_eq!(r.dropped_unverified, 1);
    // Progress ends with the verification pass.
    let mut last = None;
    while let Ok(p) = rx.try_recv() {
        if !matches!(p, Partial::Findings(_)) {
            last = Some(p);
        }
    }
    assert!(matches!(last, Some(Partial::Verifying)), "{last:?}");
    let msgs: Vec<Seen> = m.seen.lock().iter().filter(|s| s.path == "/v1/messages").cloned().collect();
    assert_eq!(msgs.len(), 2);
    let review_user = msgs[0].body["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(review_user.contains("# Repository context") && review_user.contains("Callers of parse"));
    assert_eq!(msgs[0].body["output_config"]["effort"], "high");
    let verify_user = msgs[1].body["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(verify_user.contains("## 0: Bug in a.rs") && verify_user.contains("line 0 of a.rs"));
    assert!(schema::check(&msgs[1].body["output_config"]["format"]["schema"]).is_empty());
    // Same cached prefix for the verification pass.
    assert_eq!(msgs[0].body["system"], msgs[1].body["system"]);
}

#[tokio::test]
async fn standard_has_no_context_and_thorough_raises_effort() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("");
        if user.contains("checking another reviewer's findings") {
            return ok(json!({"results": [{"index": 0, "confirmed": true, "reason": "ok"}]}), json!({}));
        }
        ok(answer_for(body), json!({}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), None);
    let mut i = input(vec![file("a.rs", 2)], Mode::Standard, Speed::Balanced);
    i.context = vec![ContextBlock { title: "ctx".into(), text: "x".into() }];
    e.review(&i, None).await.unwrap();
    i.speed = Speed::Thorough;
    e.review(&i, None).await.unwrap();
    let msgs: Vec<Seen> = m.seen.lock().iter().filter(|s| s.path == "/v1/messages").cloned().collect();
    assert!(!msgs[0].body["messages"][0]["content"][0]["text"].as_str().unwrap().contains("# Repository context"));
    assert_eq!(msgs[1].body["output_config"]["effort"], "xhigh");
    // Thorough verifies even in standard mode.
    assert_eq!(msgs.len(), 3);
    // An effort override wins.
    let mut i2 = input(vec![file("a.rs", 2)], Mode::Deep, Speed::Fast);
    i2.settings.effort = Some(code_review::Effort::Max);
    e.review(&i2, None).await.unwrap();
    let last = m.seen.lock().iter().rfind(|s| s.path == "/v1/messages").cloned().unwrap();
    assert_eq!(last.body["output_config"]["effort"], "max");
}

#[tokio::test]
async fn dedupes_against_open_threads_and_profiles() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        let mut a = answer_for(body);
        a["findings"].as_array_mut().unwrap().push(json!({"path": "a.rs", "side": "new", "start_line": 3, "line": 3, "severity": "nit",
            "title": "Rename x", "body": "naming", "suggestion": null, "confidence": 0.9}));
        a["findings"].as_array_mut().unwrap().push(json!({"path": "a.rs", "side": "new", "start_line": 99, "line": 99, "severity": "bug",
            "title": "Outside the diff", "body": "x", "suggestion": null, "confidence": 0.9}));
        ok(a, json!({}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), None);
    let mut i = input(vec![file("a.rs", 3)], Mode::Standard, Speed::Balanced);
    i.existing = vec![Existing { path: "a.rs".into(), line: 2, text: "**bug** · Bug in a.rs\n\nPanics on empty input.".into() }];
    let r = e.review(&i, None).await.unwrap();
    // The repeat is dropped, the nit too (chill), the out-of-diff one becomes a note.
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    assert_eq!(r.dropped_duplicates, 1);
    assert_eq!(r.dropped_invalid, 1);
    assert!(r.notes.iter().any(|n| n.contains("Outside the diff")));
    assert_eq!(r.verdict, "approve");
    i.existing.clear();
    i.settings.profile = Profile::Assertive;
    let r = e.review(&i, None).await.unwrap();
    assert_eq!(r.findings.len(), 2);
    assert_eq!(r.verdict, "request_changes");
}

#[tokio::test]
async fn classifier_and_replies() {
    let m = mock(Arc::new(|_, body, _| {
        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("");
        if user.contains("how deep a code review") {
            return ok(json!({"mode": "max", "reasons": ["touches key handling"]}), json!({"input_tokens": 40}));
        }
        ok(json!({"reply": "Because x can be empty.", "suggestion": "if x.is_empty() { return; }"}), json!({"input_tokens": 7}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), Some("claude-haiku-5-5".into()));
    let s = ReviewerSettings::default();
    let (mode, reasons, model, usage) = e.classify(&[], "Files: src/kms.rs +10 -2", &s).await.unwrap();
    assert_eq!((mode, reasons[0].as_str(), model.as_str(), usage.input_tokens), (Mode::Max, "touches key handling", "claude-opus-5-5", 40));
    let (text, sugg, _, _) = e.reply(&[], "explain", &s).await.unwrap();
    assert_eq!(text, "Because x can be empty.");
    assert!(sugg.unwrap().contains("is_empty"));
    let seen = m.seen.lock().clone();
    assert_eq!(seen[0].body["model"], "claude-haiku-5-5");
    assert_eq!(seen[0].body["output_config"]["effort"], "low");
    assert_eq!(seen[1].body["output_config"]["effort"], "medium");
    assert!(schema::check(&seen[0].body["output_config"]["format"]["schema"]).is_empty());
}

#[tokio::test]
async fn openai_compatible_request_shape() {
    let m = mock(Arc::new(|_, body, _| {
        let user = body["messages"][1]["content"].as_str().unwrap_or("").to_string();
        let fake = json!({"messages": [{"content": [{"text": user}]}]});
        let a = answer_for(&fake);
        (200, vec![], json!({"model": "llama-3", "choices": [{"finish_reason": "stop", "message": {"content": a.to_string()}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 30}}}))
    }))
    .await;
    let p = OpenAiCompatible::new(&format!("{}/v1", m.url), "sk-or-secret", "llama-3").unwrap().with_retry(fast_retry());
    let e = Engine::new(Arc::new(p), None);
    let r = e.review(&input(vec![file("a.rs", 2)], Mode::Standard, Speed::Balanced), None).await.unwrap();
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.model, "llama-3");
    let u = r.total_usage();
    assert_eq!((u.input_tokens, u.cache_read_tokens, u.output_tokens), (70, 30, 20));
    let seen = m.seen.lock().clone();
    assert_eq!(seen.len(), 1, "no token counter on chat completions");
    let s = &seen[0];
    assert_eq!(s.path, "/v1/chat/completions");
    assert_eq!(s.headers["authorization"], "Bearer sk-or-secret");
    assert_eq!(s.body["model"], "llama-3");
    assert_eq!(s.body["messages"][0]["role"], "system");
    assert!(s.body["messages"][0]["content"].as_str().unwrap().contains("Care about tenancy."));
    assert_eq!(s.body["response_format"]["type"], "json_schema");
    assert_eq!(s.body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(s.body["response_format"]["json_schema"]["name"], "review");
    assert!(schema::check(&s.body["response_format"]["json_schema"]["schema"]).is_empty());
}

#[tokio::test]
async fn every_group_failing_is_an_error_and_stop_reasons_are_read() {
    let m = mock(Arc::new(|path, _, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        (401, vec![], json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), None);
    let err = e.review(&input(vec![file("a.rs", 1)], Mode::Standard, Speed::Balanced), None).await.unwrap_err();
    assert!(err.to_string().contains("invalid x-api-key"));
    let r = code_review::provider::anthropic::parse_response(&json!({"stop_reason": "pause_turn", "content": [], "usage": {}})).unwrap();
    assert_eq!(r.stop, Stop::Other("pause_turn".into()));
    // Nothing to review at all.
    let e2 = Engine::new(anthropic(&m.url), None);
    let mut i = input(vec![], Mode::Standard, Speed::Balanced);
    i.skipped.push(code_review::Skipped { path: "x.png".into(), reason: "binary".into() });
    assert!(e2.review(&i, None).await.is_err());
}

/// A provider answer bigger than `MAX_RESPONSE_BYTES` is refused, not buffered; redirects are
/// never followed (a bring-your-own `base_url` mustn't bounce requests to another host).
#[tokio::test]
async fn oversized_answers_and_redirects_are_refused() {
    use code_review::provider::{Effort, MAX_RESPONSE_BYTES, Request};
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h2 = hits.clone();
    let app = Router::new()
        .route(
            "/big/chat/completions",
            axum::routing::post(|| async { (StatusCode::OK, vec![b' '; MAX_RESPONSE_BYTES + 1024]).into_response() }),
        )
        .route(
            "/hop/chat/completions",
            axum::routing::post(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/elsewhere")]).into_response() }),
        )
        .route(
            "/elsewhere",
            axum::routing::post(move || {
                let h = h2.clone();
                async move {
                    h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let req = Request { model: String::new(), system: vec![], user: "x".into(), schema: json!({}), schema_name: "review", effort: Effort::Low, max_tokens: 10 };

    let big = OpenAiCompatible::new(&format!("{url}/big"), "k", "m").unwrap().with_retry(fast_retry());
    match big.complete(&req).await {
        Err(ProviderError::Decode(m, _)) => assert!(m.contains("larger than"), "{m}"),
        other => panic!("expected a size error, got {other:?}"),
    }
    let hop = OpenAiCompatible::new(&format!("{url}/hop"), "k", "m").unwrap().with_retry(fast_retry());
    match hop.complete(&req).await {
        Err(ProviderError::Rejected { status: 307, .. }) => {}
        other => panic!("expected the redirect to be refused, got {other:?}"),
    }
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "the redirect target was never called");
}

/// The effort table in reviews.md, cell by cell (mode × speed), through real requests: the
/// review request's `output_config.effort`, the model (`fast` uses the secondary one) and whether
/// a verification pass runs (`max` or `thorough`, never `fast`). `effort` overrides every cell.
#[tokio::test]
async fn effort_follows_the_table_for_every_mode_and_speed() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("");
        if user.contains("checking another reviewer's findings") {
            return ok(json!({"results": [{"index": 0, "confirmed": true, "reason": "ok"}]}), json!({}));
        }
        ok(answer_for(body), json!({}))
    }))
    .await;
    let e = Engine::new(anthropic(&m.url), Some("claude-haiku-5-5".into()));
    let table = [
        (Mode::Standard, Speed::Fast, "low"),
        (Mode::Deep, Speed::Fast, "low"),
        (Mode::Max, Speed::Fast, "low"),
        (Mode::Standard, Speed::Balanced, "medium"),
        (Mode::Deep, Speed::Balanced, "high"),
        (Mode::Max, Speed::Balanced, "high"),
        (Mode::Standard, Speed::Thorough, "xhigh"),
        (Mode::Deep, Speed::Thorough, "xhigh"),
        (Mode::Max, Speed::Thorough, "xhigh"),
    ];
    let messages = |from: usize| m.seen.lock()[from..].iter().filter(|s| s.path == "/v1/messages").cloned().collect::<Vec<Seen>>();
    for (mode, speed, effort) in table {
        assert_eq!(Engine::effort_for(mode, speed, &ReviewerSettings::default()).as_str(), effort, "{mode:?} × {speed:?}");
        let from = m.seen.lock().len();
        let r = e.review(&input(vec![file("a.rs", 2)], mode, speed), None).await.unwrap();
        assert_eq!(r.effort.map(|x| x.as_str()), Some(effort));
        let msgs = messages(from);
        assert_eq!(msgs[0].body["output_config"]["effort"], effort, "{mode:?} × {speed:?}");
        let fast = speed == Speed::Fast;
        assert_eq!(msgs[0].body["model"] == "claude-haiku-5-5", fast, "{mode:?} × {speed:?}: {}", msgs[0].body["model"]);
        let verifies = !fast && (mode == Mode::Max || speed == Speed::Thorough);
        assert_eq!(msgs.len(), 1 + verifies as usize, "{mode:?} × {speed:?}: verification pass");
        if verifies {
            assert!(msgs[1].body["messages"][0]["content"][0]["text"].as_str().unwrap().contains("checking another reviewer's findings"));
        }
    }
    // The `effort` setting wins over every cell.
    for over in ["low", "medium", "high", "xhigh", "max"] {
        for (mode, speed, _) in table {
            let mut i = input(vec![file("a.rs", 2)], mode, speed);
            i.settings.effort = code_review::Effort::parse(over);
            assert_eq!(Engine::effort_for(mode, speed, &i.settings).as_str(), over);
            let from = m.seen.lock().len();
            e.review(&i, None).await.unwrap();
            assert_eq!(messages(from)[0].body["output_config"]["effort"], over, "{over} over {mode:?} × {speed:?}");
        }
    }
}

/// Provider error classes at the provider and engine level: a timeout is retried and then
/// reported as exhausted; 403 (like 401 and 400) is rejected at once; 500/502 are retried until
/// an answer; an answer that isn't JSON is a decode error, not retried.
#[tokio::test]
async fn timeouts_forbidden_server_errors_and_garbage() {
    use code_review::provider::Request;
    let req = Request { model: String::new(), system: vec![], user: "x".into(), schema: schema::reply(), schema_name: "reply", effort: code_review::Effort::Low, max_tokens: 10 };
    // Timeouts: a server slower than the policy's timeout.
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h2 = hits.clone();
    let app = Router::new().fallback(move || {
        let h = h2.clone();
        async move {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(500)).await;
            axum::Json(json!({}))
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let policy = RetryPolicy { attempts: 2, base: Duration::from_millis(5), max_delay: Duration::from_millis(20), timeout: Duration::from_millis(100) };
    let p = Anthropic::new("k", None).with_base_url(&slow).with_retry(policy.clone());
    match p.complete(&req).await {
        Err(ProviderError::Exhausted { attempts: 2, last, .. }) => assert_eq!(last, "timed out"),
        other => panic!("expected a timeout to exhaust the retries, got {other:?}"),
    }
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "a timeout is retried");
    let o = OpenAiCompatible::new(&slow, "k", "m").unwrap().with_retry(policy);
    assert!(matches!(o.complete(&req).await, Err(ProviderError::Exhausted { .. })));

    // 403: rejected at once (a key without permission), surfaced with its message.
    let forbidden = mock(Arc::new(|_, _, _| (403, vec![], json!({"type": "error", "error": {"type": "permission_error", "message": "key lacks access"}})))).await;
    let p = Anthropic::new("k", None).with_base_url(&forbidden.url).with_retry(fast_retry());
    match p.complete(&req).await {
        Err(ProviderError::Rejected { status: 403, message, .. }) => assert_eq!(message, "key lacks access"),
        other => panic!("expected 403 rejected, got {other:?}"),
    }
    assert_eq!(forbidden.seen.lock().len(), 1, "403 is not retried");

    // 500 then 502, then an answer.
    let flaky = mock(Arc::new(|path, body, n| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        match n {
            1 => (500, vec![], json!({"error": {"message": "internal"}})),
            2 => (502, vec![], json!({"error": {"message": "bad gateway"}})),
            _ => ok(answer_for(body), json!({})),
        }
    }))
    .await;
    let r = Engine::new(anthropic(&flaky.url), None).review(&input(vec![file("a.rs", 1)], Mode::Standard, Speed::Balanced), None).await.unwrap();
    assert_eq!(r.findings.len(), 1);
    assert_eq!(flaky.seen.lock().iter().filter(|s| s.path == "/v1/messages").count(), 3);

    // A 200 that isn't JSON: a decode error, once.
    let garbage = Router::new().fallback(|| async { (StatusCode::OK, "<html>proxy error</html>") });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let g = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, garbage).await.unwrap() });
    let p = Anthropic::new("k", None).with_base_url(&g).with_retry(fast_retry());
    assert!(matches!(p.complete(&req).await, Err(ProviderError::Decode(_, "anthropic"))));
}

/// One group failing doesn't sink the review: the other groups' findings stand, and the failed
/// group's files are listed as not reviewed with the provider's error. A refusal without a
/// category is still listed, and code-control's guard lowers the clean verdict.
#[tokio::test]
async fn a_failing_group_is_listed_and_the_rest_is_reviewed() {
    let m = mock(Arc::new(|path, body, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("");
        if user.contains("## bad.rs") {
            return (400, vec![], json!({"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long"}}));
        }
        ok(answer_for(body), json!({}))
    }))
    .await;
    let mut e = Engine::new(anthropic(&m.url), None);
    e.limits.group_tokens = 50; // one file per group
    let r = e.review(&input(vec![file("good.rs", 2), file("bad.rs", 2)], Mode::Standard, Speed::Balanced), None).await.unwrap();
    assert_eq!(r.findings.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["good.rs"]);
    let s = r.skipped.iter().find(|s| s.path == "bad.rs").expect("bad.rs listed");
    assert!(s.reason.starts_with("not reviewed:") && s.reason.contains("prompt is too long"), "{}", s.reason);
    assert_eq!(r.files_reviewed, 1);

    let refuse = mock(Arc::new(|path, _, _| {
        if path.ends_with("count_tokens") {
            return (200, vec![], json!({"input_tokens": 10}));
        }
        (200, vec![], json!({"stop_reason": "refusal", "content": [], "usage": {}}))
    }))
    .await;
    let r = Engine::new(anthropic(&refuse.url), None).review(&input(vec![file("x.rs", 1)], Mode::Standard, Speed::Balanced), None).await.unwrap();
    assert_eq!(r.skipped[0].reason, "declined by the model");
    assert_eq!(r.verdict, "approve", "the engine's own verdict");
    let (v, why) = code_review::engine::guard_approval(&r.verdict, &r.skipped, false, 0);
    assert_eq!(v, "comment");
    assert!(why.is_some());
}

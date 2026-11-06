//! The running gateway against mock upstreams: client keys, pass-through, failover, 429s,
//! limits, listeners and recording.

mod common;

use agent_proto::{GatewayRun, ModelPrice, RouteSpec, RouteTarget, UpstreamKind, UpstreamSpec, UsageBucket, UsageBy, WireFormat};
use axum::body::Bytes;
use common::*;
use serde_json::json;
use std::net::Ipv4Addr;
use std::time::Duration;

#[tokio::test]
async fn health_needs_no_key_and_everything_else_does() {
    let mock = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let g = Gw::start(vec![upstream("a", UpstreamKind::Anthropic, &mock.url(""))], vec![], &[("a", "sk-a")]).await;
    let health = g.http.get(g.url("/health")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    assert_eq!(health.json::<serde_json::Value>().await.unwrap(), json!({ "ok": true }));

    // No key: a 401 in each client's own shape.
    let r = g.http.post(g.url("/v1/messages")).json(&messages("claude-x", false)).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
    let r = g.http.post(g.url("/v1/chat/completions")).bearer_auth("rgk_wrong").json(&json!({ "model": "x", "messages": [] })).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.json::<serde_json::Value>().await.unwrap()["error"]["message"].is_string());
    assert_eq!(g.http.get(g.url("/v1/models")).send().await.unwrap().status(), 401);
    assert_eq!(mock.count(), 0);

    // Each way a key can come.
    for (name, value) in [("authorization", format!("Bearer {}", g.key)), ("x-api-key", g.key.clone()), ("api-key", g.key.clone())] {
        let r = g.http.post(g.url("/v1/messages")).header(name, value).json(&messages("claude-x", false)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{name}");
    }
    // There is nothing else: no management API.
    for path in ["/v1/clients", "/v1/settings", "/admin", "/v0/management/config"] {
        assert_eq!(g.http.get(g.url(path)).bearer_auth(&g.key).send().await.unwrap().status(), 404, "{path}");
    }
    // A revoked key is refused at once.
    let client = g.gw.clients().into_iter().next().unwrap();
    assert!(g.gw.revoke_client(&client.id).unwrap());
    assert_eq!(g.post("/v1/messages", &messages("claude-x", false)).await.status(), 401);
}

#[tokio::test]
async fn each_api_passes_through_byte_exact() {
    let chat_answer = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-x","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
    let chat_stream = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\r\n\r\n: keep-alive\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let responses_answer = r#"{"id":"resp_1","object":"response","status":"completed","model":"gpt-x","output":[],"usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}"#;
    let responses_stream = "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n";
    let anthropic_stream = messages_stream("claude-x").concat();
    let anthropic_answer = messages_answer("claude-x", 3, 1).to_string();
    let (cs, ca, rs, ra, ms, ma) = (chat_stream, chat_answer, responses_stream, responses_answer, anthropic_stream.clone(), anthropic_answer.clone());
    let mock = Mock::start(move |s| {
        let stream = s.json()["stream"].as_bool().unwrap_or(false);
        // Each in small pieces, so events cross chunks.
        let chunked = |text: &str| stream_chunks(text);
        match (s.uri.as_str(), stream) {
            ("/v1/messages", true) => chunked(&ms),
            ("/v1/messages", false) => bytes(200, "application/json", ma.clone()),
            ("/v1/chat/completions", true) => chunked(cs),
            ("/v1/chat/completions", false) => bytes(200, "application/json", ca),
            ("/v1/responses", true) => chunked(rs),
            ("/v1/responses", false) => bytes(200, "application/json", ra),
            _ => status(404),
        }
    })
    .await;
    let g = Gw::start(
        vec![upstream("claude", UpstreamKind::Anthropic, &mock.url("")), upstream("openai", UpstreamKind::Openai, &mock.url("/v1"))],
        vec![
            RouteSpec { model: "claude-x".into(), targets: vec![RouteTarget { upstream: "claude".into(), model: None }] },
            RouteSpec { model: "gpt-x".into(), targets: vec![RouteTarget { upstream: "openai".into(), model: None }] },
        ],
        &[("claude", "sk-ant"), ("openai", "sk-oai")],
    )
    .await;
    let cases = [
        ("/v1/messages", messages("claude-x", true), anthropic_stream.as_str(), "text/event-stream"),
        ("/v1/messages", messages("claude-x", false), anthropic_answer.as_str(), "application/json"),
        ("/v1/chat/completions", json!({ "model": "gpt-x", "stream": true, "messages": [] }), chat_stream, "text/event-stream"),
        ("/v1/chat/completions", json!({ "model": "gpt-x", "messages": [] }), chat_answer, "application/json"),
        ("/v1/responses", json!({ "model": "gpt-x", "stream": true, "input": "hi" }), responses_stream, "text/event-stream"),
        ("/v1/responses", json!({ "model": "gpt-x", "input": "hi" }), responses_answer, "application/json"),
    ];
    for (path, body, want, content_type) in cases {
        let r = g.post(path, &body).await;
        assert_eq!(r.status(), 200, "{path}");
        assert_eq!(r.headers()["content-type"], content_type, "{path}");
        assert_eq!(r.text().await.unwrap(), want, "{path} {body}");
    }
    let seen = mock.seen();
    assert_eq!(seen.len(), 6);
    // Keys by kind.
    assert_eq!(seen[0].header("x-api-key"), Some("sk-ant"));
    assert_eq!(seen[2].header("authorization"), Some("Bearer sk-oai"));
    let log = g.logged(6).await;
    assert!(log.iter().all(|e| e.status == 200 && e.attempts.len() == 1 && e.error.is_none()), "{log:?}");
    assert_eq!(log.iter().filter(|e| e.stream).count(), 3);
}

/// `text` split into 7-byte chunks.
fn stream_chunks(text: &str) -> axum::response::Response {
    let chunks: Vec<Result<Bytes, &'static str>> = text.as_bytes().chunks(7).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
    stream("text/event-stream", Duration::ZERO, chunks)
}

#[tokio::test]
async fn requests_go_out_as_the_gateway_with_only_what_upstream_needs() {
    let mock = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let mut up = upstream("a", UpstreamKind::Anthropic, &mock.url(""));
    up.headers = vec![("anthropic-organization".into(), "org-1".into())];
    let g = Gw::start(vec![up], vec![], &[("a", "sk-upstream")]).await;
    let r = g
        .http
        .post(g.url("/v1/messages"))
        .header("x-api-key", &g.key)
        .header("user-agent", "claude-cli/2.1.0 (external, cli)")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .header("cookie", "session=secret")
        .header("x-claude-code-session-id", "sess-1")
        .header("x-roll-thread", "thr_1")
        .header("x-roll-project", "prj_9")
        .header("x-stainless-os", "MacOS")
        .json(&messages("claude-x", false))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let s = &mock.seen()[0];
    assert_eq!(s.header("user-agent"), Some(&*format!("roll-gateway/{} (claude-cli/2.1.0 (external, cli))", env!("CARGO_PKG_VERSION"))));
    assert_eq!(s.header("x-api-key"), Some("sk-upstream"));
    assert_eq!(s.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(s.header("anthropic-beta"), Some("interleaved-thinking-2025-05-14"));
    assert_eq!(s.header("anthropic-organization"), Some("org-1"));
    for gone in ["cookie", "authorization", "x-claude-code-session-id", "x-roll-thread", "x-roll-project", "x-stainless-os"] {
        assert_eq!(s.header(gone), None, "{gone} went upstream");
    }
    assert!(!String::from_utf8_lossy(&s.body).contains(&g.key));

    // The thread and project are logged (a thread that isn't an id is dropped), with the client.
    g.http.post(g.url("/v1/messages")).bearer_auth(&g.key).header("x-roll-thread", "not an id; drop table").json(&messages("claude-x", false)).send().await.unwrap();
    let log = g.logged(2).await;
    assert_eq!(log[1].thread.as_deref(), Some("thr_1"));
    assert_eq!(log[1].project.as_deref(), Some("prj_9"));
    assert_eq!(log[0].thread, None);
    let client = g.gw.clients().into_iter().next().unwrap();
    assert_eq!(log[0].client.as_deref(), Some(client.id.as_str()));
}

#[tokio::test]
async fn failover_on_500_529_refused_and_timeout_in_priority_order() {
    let five = Mock::start(|_| status(500)).await;
    let overloaded = Mock::start(|_| status(529)).await;
    let slow = Mock::delayed(Duration::from_secs(5), |_| json(200, messages_answer("claude-x", 1, 1))).await;
    let good = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    // Port 9 (discard): nothing listens there. A freed ephemeral port could be taken by another
    // test binary running alongside.
    let refused = format!("http://{}:9", Ipv4Addr::LOCALHOST);
    let ups = vec![
        upstream("five", UpstreamKind::AnthropicCompatible, &five.url("")),
        upstream("overloaded", UpstreamKind::AnthropicCompatible, &overloaded.url("")),
        upstream("refused", UpstreamKind::AnthropicCompatible, &refused),
        upstream("good", UpstreamKind::AnthropicCompatible, &good.url("")),
    ];
    let route = RouteSpec {
        model: "claude-*".into(),
        targets: ["five", "overloaded", "refused", "good"].iter().map(|u| RouteTarget { upstream: u.to_string(), model: None }).collect(),
    };
    let g = Gw::start(ups, vec![route], &[]).await;
    let r = g.post("/v1/messages", &messages("claude-x", false)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["content"][0]["text"], "hello");
    let log = g.logged(1).await;
    let statuses: Vec<(String, u16)> = log[0].attempts.iter().map(|a| (a.upstream.clone(), a.status)).collect();
    assert_eq!(statuses, vec![("five".into(), 500), ("overloaded".into(), 529), ("refused".into(), 0), ("good".into(), 200)]);
    assert!(log[0].attempts[2].error.as_deref().unwrap().contains("refused"), "{:?}", log[0].attempts[2]);

    // A timeout: an upstream whose answer doesn't come in time.
    g.gw.set_timeouts_for_tests(Duration::from_millis(300), Duration::from_secs(5));
    let ups = vec![upstream("slow", UpstreamKind::AnthropicCompatible, &slow.url("")), upstream("good", UpstreamKind::AnthropicCompatible, &good.url(""))];
    g.gw.apply(settings(g.port, ups, vec![]), keys(&[])).await;
    let r = g.post("/v1/messages", &messages("claude-x", false)).await;
    assert_eq!(r.status(), 200);
    let log = g.logged(2).await;
    assert_eq!(log[0].attempts[0].upstream, "slow");
    assert!(log[0].attempts[0].error.as_deref().unwrap().contains("no answer"), "{:?}", log[0].attempts);
    assert_eq!(log[0].attempts[1].upstream, "good");

    // A stream that fails before its first event fails over too.
    let early_error = Mock::start(|_| sse(&["event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n".to_string()])).await;
    let streams = Mock::start(|_| sse(&messages_stream("claude-x"))).await;
    let ups = vec![upstream("early", UpstreamKind::AnthropicCompatible, &early_error.url("")), upstream("streams", UpstreamKind::AnthropicCompatible, &streams.url(""))];
    g.gw.apply(settings(g.port, ups, vec![]), keys(&[])).await;
    let r = g.post("/v1/messages", &messages("claude-x", true)).await;
    assert_eq!(r.text().await.unwrap(), messages_stream("claude-x").concat());
    let log = g.logged(3).await;
    assert_eq!(log[0].attempts.iter().map(|a| a.upstream.as_str()).collect::<Vec<_>>(), vec!["early", "streams"]);
}

#[tokio::test]
async fn nothing_fails_over_after_the_first_event() {
    let events = messages_stream("claude-x");
    let first = events[0].clone();
    let breaks = Mock::start(move |_| stream("text/event-stream", Duration::from_millis(10), vec![Ok(Bytes::from(first.clone())), Err("gone")])).await;
    let other = Mock::start(|_| sse(&messages_stream("claude-x"))).await;
    let ups = vec![upstream("breaks", UpstreamKind::AnthropicCompatible, &breaks.url("")), upstream("other", UpstreamKind::AnthropicCompatible, &other.url(""))];
    let g = Gw::start(ups, vec![], &[]).await;
    let r = g.post("/v1/messages", &messages("claude-x", true)).await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.starts_with(&events[0]), "{text}");
    assert!(text.contains("event: error"), "{text}");
    assert_eq!(other.count(), 0);
    let log = g.logged(1).await;
    assert_eq!(log[0].attempts.len(), 1);
    assert!(log[0].error.as_deref().unwrap().contains("broke"), "{:?}", log[0].error);
}

#[tokio::test]
async fn a_429_cools_its_provider_down_and_only_another_provider_takes_over() {
    let limited = Mock::start(|_| {
        let mut r = status(429);
        r.headers_mut().insert("retry-after", "120".parse().unwrap());
        r
    })
    .await;
    let same_provider = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let other_provider = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let ups = vec![
        // Two Anthropic keys are one provider, wherever they point.
        upstream("anthropic-1", UpstreamKind::Anthropic, &limited.url("")),
        upstream("anthropic-2", UpstreamKind::Anthropic, &same_provider.url("")),
        // A compatible server at another host is another provider.
        upstream("elsewhere", UpstreamKind::AnthropicCompatible, &other_provider.url_by_name("")),
    ];
    let g = Gw::start(ups, vec![], &[("anthropic-1", "k1"), ("anthropic-2", "k2")]).await;
    let r = g.post("/v1/messages", &messages("claude-x", false)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(same_provider.count(), 0, "a 429 moved to another key of the same provider");
    assert_eq!(other_provider.count(), 1);
    let log = g.logged(1).await;
    assert_eq!(log[0].attempts.iter().map(|a| (a.upstream.as_str(), a.status)).collect::<Vec<_>>(), vec![("anthropic-1", 429), ("elsewhere", 200)]);
    let status = g.gw.status().await;
    let health = status.upstreams.iter().find(|h| h.upstream == "anthropic-1").unwrap();
    assert!(health.cooling_until.is_some());

    // While it cools down, nothing goes to it or to its provider's other keys.
    assert_eq!(g.post("/v1/messages", &messages("claude-x", false)).await.status(), 200);
    assert_eq!((limited.count(), same_provider.count(), other_provider.count()), (1, 0, 2));

    // With no other provider, the client hears the 429 and when to come back.
    let ups = vec![upstream("anthropic-1", UpstreamKind::Anthropic, &limited.url("")), upstream("anthropic-2", UpstreamKind::Anthropic, &same_provider.url(""))];
    g.gw.apply(settings(g.port, ups, vec![]), keys(&[("anthropic-1", "k1"), ("anthropic-2", "k2")])).await;
    let r = g.post("/v1/messages", &messages("claude-x", false)).await;
    assert_eq!(r.status(), 429);
    let wait: u64 = r.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((100..=120).contains(&wait), "{wait}");
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["error"]["type"], "rate_limit_error");
    assert_eq!((limited.count(), same_provider.count()), (1, 0));
}

#[tokio::test]
async fn an_open_circuit_is_skipped_without_trying_it() {
    let broken = Mock::start(|_| status(503)).await;
    let good = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let ups = vec![upstream("broken", UpstreamKind::AnthropicCompatible, &broken.url("")), upstream("good", UpstreamKind::AnthropicCompatible, &good.url(""))];
    let g = Gw::start(ups, vec![], &[]).await;
    for _ in 0..model_gateway::route::OPEN_AFTER {
        assert_eq!(g.post("/v1/messages", &messages("claude-x", false)).await.status(), 200);
    }
    assert_eq!(broken.count(), 5);
    let h = g.gw.status().await.upstreams.into_iter().find(|h| h.upstream == "broken").unwrap();
    assert_eq!(h.circuit, agent_proto::CircuitState::Open);
    assert_eq!(h.failures, 5);
    assert!(h.last_error.unwrap().contains("503"));
    assert_eq!(g.post("/v1/messages", &messages("claude-x", false)).await.status(), 200);
    assert_eq!(broken.count(), 5, "an open circuit was tried");
    let log = g.logged(6).await;
    assert_eq!(log[0].attempts.len(), 1);
    let good_health = g.gw.status().await.upstreams.into_iter().find(|h| h.upstream == "good").unwrap();
    assert_eq!(good_health.requests_hour, 6);
    assert!(good_health.p50_ms.is_some());
}

#[tokio::test]
async fn bodies_over_the_cap_are_413() {
    let mock = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let g = Gw::start(vec![upstream("a", UpstreamKind::Anthropic, &mock.url(""))], vec![], &[("a", "k")]).await;
    let big = format!("{{\"model\":\"claude-x\",\"pad\":\"{}\"}}", "x".repeat(model_gateway::server::MAX_BODY));
    let r = g.http.post(g.url("/v1/messages")).bearer_auth(&g.key).header("content-type", "application/json").body(big).send().await.unwrap();
    assert_eq!(r.status(), 413);
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["error"]["type"], "request_too_large");
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn an_oversize_event_ends_the_stream_with_an_error() {
    let events = messages_stream("claude-x");
    let first = events[0].clone();
    let huge = format!("event: content_block_delta\ndata: {}\n\n", "x".repeat(model_gateway::stream::MAX_EVENT + 1));
    let mock = Mock::start(move |_| stream("text/event-stream", Duration::ZERO, vec![Ok(Bytes::from(first.clone())), Ok(Bytes::from(huge.clone()))])).await;
    let g = Gw::start(vec![upstream("a", UpstreamKind::AnthropicCompatible, &mock.url(""))], vec![], &[]).await;
    let text = g.post("/v1/messages", &messages("claude-x", true)).await.text().await.unwrap();
    assert!(text.starts_with(&events[0]));
    assert!(text.len() < 64 << 10, "the oversize event reached the client");
    assert!(text.contains("event: error") && text.contains("over"), "{text}");
    let log = g.logged(1).await;
    assert!(log[0].error.as_deref().unwrap().contains("over"));
}

#[tokio::test]
async fn private_addresses_are_refused_unless_allowed() {
    let mock = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let mut up = upstream("lan", UpstreamKind::AnthropicCompatible, &mock.url(""));
    up.allow_private = false;
    let g = Gw::start(vec![up.clone()], vec![], &[]).await;
    let r = g.post("/v1/messages", &messages("claude-x", false)).await;
    assert_eq!(r.status(), 502);
    assert!(r.text().await.unwrap().contains("private address"));
    // A name that is loopback is refused as well.
    let mut by_name = up.clone();
    by_name.base_url = Some(mock.url_by_name(""));
    g.gw.apply(settings(g.port, vec![by_name], vec![]), keys(&[])).await;
    assert_eq!(g.post("/v1/messages", &messages("claude-x", false)).await.status(), 502);
    assert_eq!(mock.count(), 0);
    let check = g.gw.test(None).await;
    assert!(!check[0].ok && check[0].error.as_deref().unwrap().contains("private"), "{check:?}");

    up.allow_private = true;
    g.gw.apply(settings(g.port, vec![up], vec![]), keys(&[])).await;
    assert_eq!(g.post("/v1/messages", &messages("claude-x", false)).await.status(), 200);
}

#[tokio::test]
async fn applying_settings_mid_stream_drops_nothing() {
    let events = messages_stream("claude-x");
    let chunks: Vec<Result<Bytes, &'static str>> = events.iter().map(|e| Ok(Bytes::from(e.clone()))).collect();
    let mock = Mock::start(move |_| stream("text/event-stream", Duration::from_millis(150), chunks.clone())).await;
    let g = Gw::start(vec![upstream("a", UpstreamKind::AnthropicCompatible, &mock.url(""))], vec![], &[]).await;
    let r = g.post("/v1/messages", &messages("claude-x", true)).await;
    // The stream is open: change upstreams, routes and keys under it.
    let ups = vec![upstream("a", UpstreamKind::AnthropicCompatible, &mock.url("")), upstream("b", UpstreamKind::Openai, &mock.url("/v1"))];
    let route = RouteSpec { model: "fast".into(), targets: vec![RouteTarget { upstream: "b".into(), model: Some("gpt-x".into()) }] };
    let run = g.gw.apply(settings(g.port, ups, vec![route]), keys(&[("b", "k")])).await;
    assert!(matches!(run, GatewayRun::Running { .. }));
    assert_eq!(r.text().await.unwrap(), events.concat());
}

#[tokio::test]
async fn a_port_change_rebinds_and_a_failed_bind_retries() {
    let mock = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let g = Gw::start(vec![upstream("a", UpstreamKind::AnthropicCompatible, &mock.url(""))], vec![], &[]).await;
    let mut watch = g.gw.watch_run();
    let ups = vec![upstream("a", UpstreamKind::AnthropicCompatible, &mock.url(""))];
    let new_port = free_port();
    let run = g.gw.apply(settings(new_port, ups.clone(), vec![]), keys(&[])).await;
    let GatewayRun::Running { addresses, .. } = run else { panic!("{run:?}") };
    assert!(addresses.contains(&format!("{}:{new_port}", Ipv4Addr::LOCALHOST)), "{addresses:?}");
    assert!(watch.has_changed().unwrap());
    watch.borrow_and_update();
    let health = |port: u16| reqwest::get(format!("http://{}:{port}/health", Ipv4Addr::LOCALHOST));
    assert_eq!(health(new_port).await.unwrap().status(), 200);
    // The old port closes (once it has finished what it served).
    let mut closed = false;
    for _ in 0..50 {
        if health(g.port).await.is_err() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "the old port still answers");

    // Same settings again: nothing is rebound.
    g.gw.apply(settings(new_port, ups.clone(), vec![]), keys(&[])).await;
    assert!(!watch.has_changed().unwrap());

    // A port something else holds: Failed, then Running once it's free.
    let taken = free_port();
    let holder = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, taken)).unwrap();
    let run = g.gw.apply(settings(taken, ups.clone(), vec![]), keys(&[])).await;
    assert!(matches!(&run, GatewayRun::Failed { error, .. } if error.contains(&taken.to_string())), "{run:?}");
    drop(holder);
    // (The `Ref` `wait_for` answers holds the value: let it go before applying again.)
    let retried = tokio::time::timeout(Duration::from_secs(10), watch.wait_for(|r| matches!(r, GatewayRun::Running { .. }))).await.is_ok_and(|r| r.is_ok());
    assert!(retried, "the bind was never retried");
    assert_eq!(health(taken).await.unwrap().status(), 200);

    // Disabled: stopped.
    let mut off = settings(taken, ups, vec![]);
    off.enabled = false;
    assert_eq!(g.gw.apply(off, keys(&[])).await, GatewayRun::Stopped);
    assert_eq!(g.gw.run_state(), GatewayRun::Stopped);
}

#[tokio::test]
async fn models_lists_routes_patterns_and_what_upstreams_list() {
    let mock = Mock::start(|s| match s.uri.as_str() {
        "/v1/models?limit=1000" => json(200, json!({ "data": [{ "id": "claude-opus-4-5", "type": "model" }, { "id": "claude-haiku-4-5", "type": "model" }] })),
        "/v1/models" => json(200, json!({ "object": "list", "data": [{ "id": "gpt-5", "object": "model" }] })),
        _ => status(404),
    })
    .await;
    let mut patterned = upstream("router", UpstreamKind::OpenaiCompatible, &mock.url("/v1"));
    patterned.models = vec!["llama-3".into(), "qwen-*".into()];
    let listed = |id: &str, kind, base: &str| UpstreamSpec { models: vec![], ..upstream(id, kind, base) };
    let ups = vec![listed("claude", UpstreamKind::Anthropic, &mock.url("")), listed("openai", UpstreamKind::Openai, &mock.url("/v1")), patterned];
    let routes = vec![RouteSpec { model: "fast".into(), targets: vec![RouteTarget { upstream: "claude".into(), model: Some("claude-haiku-4-5".into()) }] }];
    let g = Gw::start(ups, routes, &[("claude", "k"), ("openai", "k")]).await;
    let r = g.http.get(g.url("/v1/models")).header("x-api-key", &g.key).header("anthropic-version", "2023-06-01").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    let ids: Vec<&str> = body["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["fast", "claude-opus-4-5", "claude-haiku-4-5", "gpt-5", "llama-3"]);
    let m = &body["data"][0];
    assert_eq!((m["object"].as_str(), m["type"].as_str(), m["display_name"].as_str()), (Some("model"), Some("model"), Some("fast")));
    assert!(m["created_at"].is_string());
    assert_eq!(body["has_more"], false);
    assert_eq!(g.gw.status().await.models, ids);
}

#[tokio::test]
async fn count_tokens_passes_to_anthropic_or_is_estimated() {
    let mock = Mock::start(|s| match s.uri.as_str() {
        "/v1/messages/count_tokens" => json(200, json!({ "input_tokens": 42 })),
        _ => status(404),
    })
    .await;
    let body = json!({ "model": "claude-x", "messages": [{ "role": "user", "content": "hello there" }] });
    let g = Gw::start(vec![upstream("a", UpstreamKind::Anthropic, &mock.url(""))], vec![], &[("a", "k")]).await;
    let r = g.post("/v1/messages/count_tokens", &body).await;
    assert_eq!(r.json::<serde_json::Value>().await.unwrap(), json!({ "input_tokens": 42 }));
    assert_eq!(mock.seen()[0].json()["model"], "claude-x");

    let ups = vec![upstream("o", UpstreamKind::OpenaiCompatible, &mock.url("/v1"))];
    g.gw.apply(settings(g.port, ups, vec![]), keys(&[])).await;
    let r = g.post("/v1/messages/count_tokens", &body).await;
    let n = r.json::<serde_json::Value>().await.unwrap()["input_tokens"].as_u64().unwrap();
    assert_eq!(n, body.to_string().len() as u64 / 4);
}

#[tokio::test]
async fn usage_and_cost_are_recorded_per_request() {
    let mock = Mock::start(|s| if s.json()["stream"] == true { sse(&messages_stream("claude-x")) } else { json(200, messages_answer("claude-x", 1000, 500)) }).await;
    let price = ModelPrice { model: "claude-x".into(), input: 3_000_000, output: 15_000_000, cache_read: 0, cache_write: 0 };
    let g = Gw::with(|mut s| {
        s.prices = vec![price];
        s.log_bodies = true;
        s
    }, vec![upstream("a", UpstreamKind::Anthropic, &mock.url(""))], vec![], &[("a", "k")])
    .await;
    g.post("/v1/messages", &messages("claude-x", false)).await.text().await.unwrap();
    g.post("/v1/messages", &messages("claude-x", true)).await.text().await.unwrap();
    let log = g.logged(2).await;
    let (stream, whole) = (&log[0], &log[1]);
    assert_eq!((whole.input_tokens, whole.output_tokens), (1000, 500));
    // 1000 × $3/M + 500 × $15/M = $0.0105.
    assert_eq!(whole.cost_micros, 10_500);
    assert_eq!((stream.input_tokens, stream.output_tokens), (12, 5));
    assert!(stream.first_byte_ms.is_some());
    assert_eq!(whole.format, WireFormat::Messages);
    assert!(whole.has_body);
    let body = g.gw.log_body(whole.seq).unwrap().unwrap();
    assert!(body.request.contains("\"hi\"") && body.response.contains("hello"));
    let usage = g.gw.usage(0, u64::MAX / 2, UsageBy::Model, UsageBucket::Day).unwrap();
    assert_eq!(usage.rows.iter().map(|r| r.requests).sum::<u64>(), 2);
    assert_eq!(usage.rows.iter().map(|r| r.input_tokens).sum::<u64>(), 1012);
}

fn chat_stream(model: &str) -> Vec<String> {
    let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
        format!("data: {}\n\n", json!({ "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] }))
    };
    vec![
        chunk(json!({ "role": "assistant", "content": "" }), json!(null)),
        chunk(json!({ "content": "Hel" }), json!(null)),
        chunk(json!({ "content": "lo" }), json!(null)),
        chunk(json!({}), json!("stop")),
        format!("data: {}\n\n", json!({ "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9 } })),
        "data: [DONE]\n\n".into(),
    ]
}

/// The client's stream, as (event name, data) pairs.
fn events(text: &str) -> Vec<(Option<String>, serde_json::Value)> {
    text.split("\n\n")
        .filter(|e| !e.trim().is_empty())
        .filter_map(|e| {
            let name = e.lines().find_map(|l| l.strip_prefix("event: ")).map(str::to_string);
            let data = e.lines().find_map(|l| l.strip_prefix("data: "))?;
            Some((name, serde_json::from_str(data).unwrap_or(json!(data))))
        })
        .collect()
}

#[tokio::test]
async fn translated_paths_reach_upstreams_of_another_api() {
    let mock = Mock::start(|s| {
        let stream = s.json()["stream"] == true;
        match (s.uri.as_str(), stream) {
            ("/v1/chat/completions", true) => sse(&chat_stream("gpt-x")),
            ("/v1/chat/completions", false) => json(200, json!({
                "id": "c1", "object": "chat.completion", "created": 1, "model": "gpt-x",
                "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9 }
            })),
            ("/v1/messages", true) => sse(&messages_stream("claude-x")),
            ("/v1/messages", false) => json(200, messages_answer("claude-x", 12, 5)),
            _ => status(404),
        }
    })
    .await;
    let mut oai = upstream("oai", UpstreamKind::OpenaiCompatible, &mock.url("/v1"));
    oai.models = vec!["gpt-*".into()];
    let mut ant = upstream("ant", UpstreamKind::AnthropicCompatible, &mock.url(""));
    ant.models = vec!["claude-*".into()];
    let g = Gw::start(vec![oai, ant], vec![], &[]).await;

    // A Messages client on a Chat upstream, streamed.
    let text = g.post("/v1/messages", &messages("gpt-x", true)).await.text().await.unwrap();
    let ev = events(&text);
    assert_eq!(ev.first().unwrap().0.as_deref(), Some("message_start"), "{text}");
    assert_eq!(ev.last().unwrap().0.as_deref(), Some("message_stop"), "{text}");
    let said: String = ev.iter().filter_map(|(_, d)| d.pointer("/delta/text").and_then(|t| t.as_str())).collect();
    assert_eq!(said, "Hello");
    let up = &mock.seen()[0];
    assert_eq!(up.uri, "/v1/chat/completions");
    assert_eq!(up.json()["stream_options"]["include_usage"], true);
    assert_eq!(up.json()["messages"][0]["content"], "hi");

    // … and whole.
    let r: serde_json::Value = g.post("/v1/messages", &messages("gpt-x", false)).await.json().await.unwrap();
    assert_eq!((r["type"].as_str(), r["model"].as_str(), r["content"][0]["text"].as_str()), (Some("message"), Some("gpt-x"), Some("hi")));

    // A Responses client on a Messages upstream.
    let text = g.post("/v1/responses", &json!({ "model": "claude-x", "stream": true, "input": "hi" })).await.text().await.unwrap();
    let names: Vec<String> = events(&text).into_iter().filter_map(|(n, _)| n).collect();
    assert_eq!(names.first().map(String::as_str), Some("response.created"), "{text}");
    assert!(names.iter().any(|n| n == "response.output_text.delta"), "{names:?}");
    assert_eq!(names.last().map(String::as_str), Some("response.completed"), "{names:?}");
    assert_eq!(mock.seen()[2].uri, "/v1/messages");

    // A Chat client on a Messages upstream, whole and streamed.
    let r: serde_json::Value = g.post("/v1/chat/completions", &json!({ "model": "claude-x", "messages": [{ "role": "user", "content": "hi" }] })).await.json().await.unwrap();
    assert_eq!(r["choices"][0]["message"]["content"], "hello");
    assert_eq!(r["usage"]["prompt_tokens"], 12);
    let text = g
        .post("/v1/chat/completions", &json!({ "model": "claude-x", "stream": true, "stream_options": { "include_usage": true }, "messages": [{ "role": "user", "content": "hi" }] }))
        .await
        .text()
        .await
        .unwrap();
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    let said: String = events(&text).iter().filter_map(|(_, d)| d.pointer("/choices/0/delta/content").and_then(|t| t.as_str())).collect();
    assert_eq!(said, "Hello");

    let log = g.logged(5).await;
    let first = log.iter().find(|e| e.model == "gpt-x" && e.stream).unwrap();
    assert_eq!((first.input_tokens, first.output_tokens), (7, 2));
    assert!(log.iter().all(|e| e.status == 200), "{log:?}");
}

#[tokio::test]
async fn reasoning_another_provider_signed_never_passes_through() {
    let mock = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let g = Gw::start(vec![upstream("a", UpstreamKind::Anthropic, &mock.url(""))], vec![], &[("a", "k")]).await;
    let body = json!({
        "model": "claude-x", "max_tokens": 64,
        "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "from openai", "signature": "roll-oai:rs_1:gAAAA" },
                { "type": "thinking", "thinking": "from anthropic", "signature": "EqQBCkYIBxgC" },
                { "type": "text", "text": "hello" }
            ] },
            { "role": "user", "content": "again" }
        ]
    });
    assert_eq!(g.post("/v1/messages", &body).await.status(), 200);
    let sent = mock.seen()[0].json();
    let blocks = sent["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["signature"], "EqQBCkYIBxgC");

    // After a failover: the conversation was on OpenAI (which is down now), and Anthropic takes
    // over without the reasoning OpenAI signed.
    let openai = Mock::start(|_| status(500)).await;
    let anthropic = Mock::start(|_| json(200, messages_answer("claude-x", 1, 1))).await;
    let mut oai = upstream("oai", UpstreamKind::Openai, &openai.url("/v1"));
    oai.models = vec!["claude-x".into()];
    let ups = vec![oai, upstream("ant", UpstreamKind::Anthropic, &anthropic.url(""))];
    g.gw.apply(settings(g.port, ups, vec![]), keys(&[("oai", "k"), ("ant", "k")])).await;
    assert_eq!(g.post("/v1/messages", &body).await.status(), 200);
    let log = g.logged(2).await;
    assert_eq!(log[0].attempts.iter().map(|a| (a.upstream.as_str(), a.status)).collect::<Vec<_>>(), vec![("oai", 500), ("ant", 200)]);
    let to_anthropic = anthropic.seen()[0].json();
    let blocks = to_anthropic["messages"][1]["content"].as_array().unwrap();
    assert!(blocks.iter().all(|b| !b["signature"].as_str().unwrap_or_default().starts_with("roll-oai:")), "{blocks:?}");
    assert_eq!(blocks.len(), 2);
}

//! Publishing and delivery: signing, delays, retries, dead letters, callbacks, deduplication,
//! topics, cancellation, limits and auth.

mod common;

use axum::http::{Method, StatusCode};
use base64::Engine;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;
use tasks::model::now_ms;

#[tokio::test(flavor = "multi_thread")]
async fn delivers_a_signed_request() {
    let env = env().await;
    let recv = Receiver::start().await;
    let id = env
        .publish(json!({
            "url": recv.url("/hook?x=1"),
            "body": {"hello": "world"},
            "headers": {"X-Custom": "abc", "Authorization": "Bearer downstream"},
            "label": "signup",
        }))
        .await;
    let hit = recv.wait("/hook", 1, 5000).await.remove(0);
    assert_eq!(hit.method, "POST");
    assert_eq!(hit.query.as_deref(), Some("x=1"));
    assert_eq!(hit.json(), json!({"hello": "world"}));
    assert_eq!(hit.header("content-type").as_deref(), Some("application/json"));
    assert_eq!(hit.header("x-custom").as_deref(), Some("abc"));
    assert_eq!(hit.header("authorization").as_deref(), Some("Bearer downstream"));
    assert_eq!(hit.header("roll-message-id").as_deref(), Some(id.as_str()));
    assert_eq!(hit.header("roll-retried").as_deref(), Some("0"));
    assert_eq!(hit.header("roll-label").as_deref(), Some("signup"));
    assert!(hit.header("user-agent").unwrap().starts_with("roll-tasks/"));

    // Standard Webhooks signature, valid with either signing key.
    let keys = env.get("/keys").await;
    let (wid, ts, sig) = (hit.header("webhook-id").unwrap(), hit.header("webhook-timestamp").unwrap(), hit.header("webhook-signature").unwrap());
    assert_eq!(wid, id);
    assert_eq!(sig.split(' ').count(), 2);
    for k in ["current", "next"] {
        tasks::sign::verify(keys[k].as_str().unwrap(), &wid, &ts, &sig, &hit.body, now_ms() / 1000).unwrap();
    }
    assert!(tasks::sign::verify(&tasks::sign::new_secret(), &wid, &ts, &sig, &hit.body, now_ms() / 1000).is_err());

    let m = env.wait_state(&id, "delivered", 3000).await;
    assert_eq!(m["attempts"], 1);
    assert_eq!(m["last"]["status"], 200);
    assert_eq!(m["last"]["body"], "ok");
    assert_eq!(m["attempts_log"].as_array().unwrap().len(), 1);
    assert_eq!(m["body"], r#"{"hello":"world"}"#);
}

#[tokio::test(flavor = "multi_thread")]
async fn rotating_keys_keeps_receivers_verifying() {
    let env = env().await;
    let recv = Receiver::start().await;
    let before = env.get("/keys").await;
    let after = env.post("/keys/rotate", json!({})).await;
    assert_eq!(after["current"], before["next"], "next key becomes current");
    assert_ne!(after["next"], before["next"]);
    env.publish(json!({ "url": recv.url("/k"), "body": "x" })).await;
    let hit = recv.wait("/k", 1, 5000).await.remove(0);
    // A receiver that pinned the old next key still verifies.
    tasks::sign::verify(
        before["next"].as_str().unwrap(),
        &hit.header("webhook-id").unwrap(),
        &hit.header("webhook-timestamp").unwrap(),
        &hit.header("webhook-signature").unwrap(),
        &hit.body,
        now_ms() / 1000,
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_publish_reads_options_from_headers() {
    let env = env().await;
    let recv = Receiver::start().await;
    let start = now_ms();
    let resp = env
        .http
        .post(format!("{}/v1/tasks/publish/{}", env.api, recv.url("/raw?a=b&c=d")))
        .header("content-type", "text/plain")
        .header("roll-delay", "300ms")
        .header("roll-method", "PUT")
        .header("roll-forward-x-tenant", "acme")
        .header("roll-retries", "0")
        .header("roll-label", "raw")
        .body("plain body")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let id = resp.json::<Value>().await.unwrap()["message_id"].as_str().unwrap().to_string();
    let hit = recv.wait("/raw", 1, 5000).await.remove(0);
    assert!(hit.at - start >= 290, "delay honoured ({}ms)", hit.at - start);
    assert_eq!(hit.method, "PUT");
    assert_eq!(hit.query.as_deref(), Some("a=b&c=d"));
    assert_eq!(hit.text(), "plain body");
    assert_eq!(hit.header("content-type").as_deref(), Some("text/plain"));
    assert_eq!(hit.header("x-tenant").as_deref(), Some("acme"));
    let m = env.wait_state(&id, "delivered", 3000).await;
    assert_eq!(m["retries"], 0);
    assert_eq!(m["label"], "raw");

    // Binary bodies survive.
    let resp = env
        .http
        .post(format!("{}/v1/tasks/publish/{}", env.api, recv.url("/bin")))
        .header("content-type", "application/octet-stream")
        .body(vec![0u8, 159, 146, 150, 255])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let hit = recv.wait("/bin", 1, 5000).await.remove(0);
    assert_eq!(hit.body, vec![0u8, 159, 146, 150, 255]);
}

#[tokio::test(flavor = "multi_thread")]
async fn delay_and_not_before() {
    let env = env().await;
    let recv = Receiver::start().await;
    let start = now_ms();
    env.publish(json!({ "url": recv.url("/delay"), "delay": "400ms" })).await;
    env.publish(json!({ "url": recv.url("/at"), "not_before": start + 600 })).await;
    let d = env.publish(json!({ "url": recv.url("/later"), "delay": "1h" })).await;
    let hit = recv.wait("/delay", 1, 5000).await.remove(0);
    assert!(hit.at - start >= 390, "{}", hit.at - start);
    let hit = recv.wait("/at", 1, 5000).await.remove(0);
    assert!(hit.at >= start + 590);
    let m = env.message(&d).await;
    assert_eq!(m["state"], "pending");
    assert!(m["due_at"].as_i64().unwrap() > now_ms() + 3_500_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_with_backoff_until_success() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/flaky", Rule { fail_times: 2, fail_status: 503, ..Default::default() });
    let id = env.publish(json!({ "url": recv.url("/flaky"), "retries": 5 })).await;
    let hits = recv.wait("/flaky", 3, 5000).await;
    assert_eq!(hits[1].header("roll-retried").as_deref(), Some("1"));
    assert_eq!(hits[2].header("roll-retried").as_deref(), Some("2"));
    // Same message id on every attempt, so receivers can dedupe.
    assert!(hits.iter().all(|h| h.header("webhook-id").as_deref() == Some(id.as_str())));
    // Backoff: 50ms, then 100ms.
    assert!(hits[1].at - hits[0].at >= 45, "{}", hits[1].at - hits[0].at);
    assert!(hits[2].at - hits[1].at >= 95, "{}", hits[2].at - hits[1].at);
    let m = env.wait_state(&id, "delivered", 3000).await;
    assert_eq!(m["attempts"], 3);
    let log = m["attempts_log"].as_array().unwrap();
    assert_eq!(log.iter().map(|a| a["status"].as_u64().unwrap()).collect::<Vec<_>>(), vec![503, 503, 200]);
    assert!(log[0]["next_at"].is_i64() && log[2]["next_at"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn exhausted_retries_dead_letter_with_failure_callback() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/down", Rule { fail_times: 1000, ..Default::default() });
    let id = env
        .publish(json!({
            "url": recv.url("/down"),
            "body": "payload",
            "retries": 2,
            "callback": recv.url("/ok-cb"),
            "failure_callback": recv.url("/fail-cb"),
        }))
        .await;
    recv.wait("/down", 3, 5000).await;
    let m = env.wait_state(&id, "failed", 5000).await;
    assert_eq!(m["attempts"], 3);
    assert_eq!(m["reason"], "HTTP 500");
    let cb = recv.wait("/fail-cb", 1, 5000).await.remove(0).json();
    assert_eq!(cb["message_id"], id.as_str());
    assert_eq!(cb["state"], "failed");
    assert_eq!(cb["status"], 500);
    assert_eq!(cb["attempts"], 3);
    assert_eq!(cb["source_body"], "cGF5bG9hZA==");
    assert!(recv.hits("/ok-cb").is_empty());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(recv.hits("/down").len(), 3, "no more attempts after dead-lettering");

    let dlq = env.get("/dlq").await;
    assert!(dlq["items"].as_array().unwrap().iter().any(|m| m["id"] == id.as_str()));

    // Fix the receiver and redrive from the DLQ.
    recv.on("/down", Rule::default());
    let r = env.post("/dlq/retry", json!({ "ids": [id] })).await;
    assert_eq!(r["retried"], 1);
    let m = env.wait_state(&id, "delivered", 5000).await;
    assert_eq!(m["attempts"], 1);
    assert!(env.get("/dlq").await["items"].as_array().unwrap().iter().all(|m| m["id"] != id.as_str()));
    let cb = recv.wait("/ok-cb", 1, 5000).await.remove(0).json();
    assert_eq!(cb["state"], "delivered");
    assert_eq!(cb["body"], "ok");

    // Delete from the DLQ.
    recv.on("/down", Rule { fail_times: 1000, ..Default::default() });
    let id2 = env.publish(json!({ "url": recv.url("/down"), "retries": 0 })).await;
    env.wait_state(&id2, "failed", 5000).await;
    let (s, _) = env.delete(&format!("/dlq/{id2}")).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = env.call(Method::GET, &format!("/messages/{id2}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_retryable_responses_stop_immediately() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/489", Rule { status: 489, ..Default::default() });
    recv.on("/flagged", Rule { status: 400, headers: vec![("roll-non-retryable".into(), "true".into())], ..Default::default() });
    let a = env.publish(json!({ "url": recv.url("/489"), "retries": 5 })).await;
    let b = env.publish(json!({ "url": recv.url("/flagged"), "retries": 5 })).await;
    let ma = env.wait_state(&a, "failed", 5000).await;
    let mb = env.wait_state(&b, "failed", 5000).await;
    assert_eq!((ma["attempts"].as_u64(), mb["attempts"].as_u64()), (Some(1), Some(1)));
    assert!(ma["reason"].as_str().unwrap().contains("not retryable"));
}

#[tokio::test(flavor = "multi_thread")]
async fn honours_retry_after() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/busy", Rule { fail_times: 1, fail_status: 429, headers: vec![("retry-after".into(), "0.4".into())], ..Default::default() });
    let id = env.publish(json!({ "url": recv.url("/busy"), "backoff": {"base_ms": 1, "max_ms": 5000} })).await;
    let hits = recv.wait("/busy", 2, 5000).await;
    assert!(hits[1].at - hits[0].at >= 390, "waited {}ms", hits[1].at - hits[0].at);
    env.wait_state(&id, "delivered", 3000).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timeouts_count_as_failures() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/slow", Rule { delay_ms: 3000, ..Default::default() });
    let id = env.publish(json!({ "url": recv.url("/slow"), "timeout": "1s", "retries": 0 })).await;
    let m = env.wait_state(&id, "failed", 6000).await;
    assert!(m["reason"].as_str().unwrap().contains("timed out"), "{m}");
    // Unreachable hosts fail as connection errors.
    let id = env.publish(json!({ "url": "http://127.0.0.1:1/nothing", "retries": 0 })).await;
    let m = env.wait_state(&id, "failed", 6000).await;
    assert!(m["reason"].as_str().unwrap().contains("connection"), "{m}");
}

#[tokio::test(flavor = "multi_thread")]
async fn success_callback_carries_the_response() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/api", Rule { status: 201, body: r#"{"created":true}"#.into(), ..Default::default() });
    let id = env.publish(json!({ "url": recv.url("/api"), "body": "in", "callback": recv.url("/cb"), "label": "l1" })).await;
    let cb = recv.wait("/cb", 1, 5000).await.remove(0);
    assert_eq!(cb.header("roll-callback-for").as_deref(), Some(id.as_str()));
    let v = cb.json();
    assert_eq!(v["status"], 201);
    assert_eq!(v["body"], r#"{"created":true}"#);
    assert_eq!(v["url"], recv.url("/api"));
    assert_eq!(v["label"], "l1");
    assert!(cb.header("webhook-signature").is_some(), "callbacks are signed too");
}

#[tokio::test(flavor = "multi_thread")]
async fn deduplicates_by_id_and_content() {
    let env = env().await;
    let recv = Receiver::start().await;
    let a = env.post("/publish", json!({ "url": recv.url("/d"), "dedup_id": "order-1", "delay": "200ms" })).await;
    let b = env.post("/publish", json!({ "url": recv.url("/d"), "dedup_id": "order-1" })).await;
    assert_eq!(a["message_id"], b["message_id"]);
    assert_eq!(b["messages"][0]["deduplicated"], true);
    let c = env.post("/publish", json!({ "url": recv.url("/c"), "body": {"x": 1}, "content_dedup": true })).await;
    let d = env.post("/publish", json!({ "url": recv.url("/c"), "body": {"x": 1}, "content_dedup": true })).await;
    let e = env.post("/publish", json!({ "url": recv.url("/c"), "body": {"x": 2}, "content_dedup": true })).await;
    assert_eq!(c["message_id"], d["message_id"]);
    assert_ne!(c["message_id"], e["message_id"]);
    recv.wait("/d", 1, 5000).await;
    recv.wait("/c", 2, 5000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(recv.hits("/d").len(), 1);
    assert_eq!(recv.hits("/c").len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn topics_fan_out_with_independent_retries() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/b", Rule { fail_times: 2, ..Default::default() });
    env.put("/topics/orders", json!({ "endpoints": [recv.url("/a"), {"url": recv.url("/b"), "name": "billing"}] })).await;
    env.post("/topics/orders/endpoints", json!({ "endpoints": [recv.url("/c"), recv.url("/a")] })).await;
    let t = env.get("/topics/orders").await;
    assert_eq!(t["endpoints"].as_array().unwrap().len(), 3, "duplicates ignored");
    let r = env.post("/publish", json!({ "topic": "orders", "body": {"id": 7} })).await;
    let msgs = r["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    for p in ["/a", "/c"] {
        let h = recv.wait(p, 1, 5000).await;
        assert_eq!(h[0].json(), json!({"id": 7}));
        assert_eq!(h[0].header("roll-topic").as_deref(), Some("orders"));
    }
    recv.wait("/b", 3, 5000).await;
    for m in msgs {
        env.wait_state(m["message_id"].as_str().unwrap(), "delivered", 5000).await;
    }
    assert_eq!(recv.hits("/a").len(), 1, "a isn't retried because b failed");
    let listed = env.get("/messages?topic=orders").await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 3);

    env.call(Method::DELETE, "/topics/orders/endpoints", Some(json!({ "urls": ["billing"] }))).await;
    assert_eq!(env.get("/topics/orders").await["endpoints"].as_array().unwrap().len(), 2);
    let (s, _) = env.delete("/topics/orders").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = env.call(Method::POST, "/publish", Some(json!({ "topic": "orders" }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_single_and_by_filter() {
    let env = env().await;
    let recv = Receiver::start().await;
    let a = env.publish(json!({ "url": recv.url("/x"), "delay": "500ms" })).await;
    let m = env.delete(&format!("/messages/{a}")).await.1;
    assert_eq!(m["state"], "canceled");
    let (s, _) = env.delete(&format!("/messages/{a}")).await;
    assert_eq!(s, StatusCode::CONFLICT, "can't cancel twice");
    for _ in 0..3 {
        env.publish(json!({ "url": recv.url("/y"), "delay": "500ms", "label": "bulk" })).await;
    }
    let keep = env.publish(json!({ "url": recv.url("/z"), "delay": "300ms" })).await;
    let r = env.post("/messages/cancel", json!({ "label": "bulk" })).await;
    assert_eq!(r["canceled"], 3);
    let (s, _) = env.call(Method::POST, "/messages/cancel", Some(json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a filter is required");
    env.wait_state(&keep, "delivered", 5000).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(recv.hits("/x").is_empty() && recv.hits("/y").is_empty());

    // A canceled message can be sent again.
    env.post(&format!("/messages/{a}/retry"), json!({})).await;
    env.wait_state(&a, "delivered", 5000).await;
    // Finished messages can be purged.
    let (s, _) = env.call(Method::DELETE, &format!("/messages/{a}?purge=true"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_filters_and_pages() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/bad", Rule { fail_times: 100, ..Default::default() });
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(env.publish(json!({ "url": recv.url("/good"), "label": format!("l{}", i % 2) })).await);
    }
    let bad = env.publish(json!({ "url": recv.url("/bad"), "retries": 0 })).await;
    env.wait_state(&bad, "failed", 5000).await;
    for id in &ids {
        env.wait_state(id, "delivered", 5000).await;
    }
    let p1 = env.get("/messages?limit=4").await;
    assert_eq!(p1["items"].as_array().unwrap().len(), 4);
    assert_eq!(p1["items"][0]["id"], bad.as_str(), "newest first");
    let cursor = p1["cursor"].as_str().unwrap();
    let p2 = env.get(&format!("/messages?limit=4&cursor={cursor}")).await;
    assert_eq!(p2["items"].as_array().unwrap().len(), 2);
    assert!(p2["cursor"].is_null());
    assert_eq!(env.get("/messages?state=delivered").await["items"].as_array().unwrap().len(), 5);
    assert_eq!(env.get("/messages?state=dlq").await["items"].as_array().unwrap().len(), 1);
    assert_eq!(env.get("/messages?label=l0").await["items"].as_array().unwrap().len(), 3);
    let url = urlencode(&recv.url("/good"));
    assert_eq!(env.get(&format!("/messages?url={url}")).await["items"].as_array().unwrap().len(), 5);
    let (s, _) = env.call(Method::GET, "/messages?state=bogus", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_publish() {
    let env = env().await;
    let recv = Receiver::start().await;
    let r = env
        .post("/batch", json!([
            { "url": recv.url("/b1"), "body": "1" },
            { "url": "ftp://nope" },
            { "url": recv.url("/b2"), "body": "2" },
        ]))
        .await;
    let r = r.as_array().unwrap();
    assert!(r[0]["message_id"].is_string() && r[2]["message_id"].is_string());
    assert!(r[1]["error"].as_str().unwrap().contains("http"));
    recv.wait("/b1", 1, 5000).await;
    recv.wait("/b2", 1, 5000).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_and_limits() {
    let env = env_with(|c| c.max_body_bytes = 1024).await;
    let bad = [
        json!({}),
        json!({ "url": "notaurl" }),
        json!({ "url": "http://x", "topic": "t" }),
        json!({ "url": "http://x", "method": "TRACE" }),
        json!({ "url": "http://x", "body": "a", "body_base64": "YQ==" }),
        json!({ "url": "http://x", "group": "g" }),
        json!({ "url": "http://x", "flow": { "key": "k" } }),
        json!({ "url": "http://x", "delay": "soon" }),
        json!({ "url": "http://x", "delay": "400d" }),
        json!({ "url": "http://x", "callback": "nope" }),
        json!({ "url": "http://x", "queue": "bad name!" }),
    ];
    for b in bad {
        let (s, v) = env.call(Method::POST, "/publish", Some(b.clone())).await;
        assert!(s == StatusCode::BAD_REQUEST || s == StatusCode::UNPROCESSABLE_ENTITY, "{b} → {s} {v}");
        assert!(v["error"].is_string(), "errors are JSON: {v}");
    }
    // Bodies the JSON extractor rejects get the same shape.
    let (s, v) = env.call(Method::PUT, "/queues/x", Some(json!({ "rate": "abc" }))).await;
    assert!(s.is_client_error() && v["error"].as_str().is_some_and(|e| e.contains("rate")), "{s} {v}");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/tasks/publish", env.api))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error());
    assert!(r.json::<Value>().await.unwrap()["error"].is_string());
    let (s, v) = env.call(Method::POST, "/publish", Some(json!({ "url": "http://x.test", "body": "x".repeat(2000) }))).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn blocks_private_destinations_when_asked() {
    let env = env_with(|c| c.block_private_urls = true).await;
    for url in ["http://127.0.0.1:9/x", "http://169.254.169.254/latest/meta-data", "http://localhost/x", "http://[::1]/"] {
        let (s, _) = env.call(Method::POST, "/publish", Some(json!({ "url": url }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}");
    }
    // A public-looking name that resolves to a private address fails at delivery.
    let id = env.publish(json!({ "url": "http://localtest.me:9/x", "retries": 0 })).await;
    let m = env.wait_state(&id, "failed", 10_000).await;
    assert!(m["reason"].as_str().unwrap().contains("private") || m["reason"].as_str().unwrap().contains("dns") || m["reason"].as_str().unwrap().contains("resolve"), "{m}");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_with_admin_token_and_api_keys() {
    let env = env_with(|c| c.token = Some("admin-secret".into())).await;
    let raw = reqwest::Client::new();
    let base = format!("{}/v1/tasks", env.api);
    assert_eq!(raw.get(format!("{base}/health")).send().await.unwrap().status(), 200, "health is open");
    assert_eq!(raw.get(format!("{base}/queues")).send().await.unwrap().status(), 401);
    assert_eq!(raw.get(format!("{base}/queues")).bearer_auth("wrong").send().await.unwrap().status(), 401);
    let key = env.post("/api-keys", json!({ "name": "ci" })).await;
    let token = key["token"].as_str().unwrap();
    assert!(token.starts_with("rt_"));
    assert_eq!(raw.get(format!("{base}/queues")).bearer_auth(token).send().await.unwrap().status(), 200);
    let keys = env.get("/api-keys").await;
    assert!(keys[0].get("token").is_none() && keys[0]["hash"].is_string());
    assert!(keys[0]["last_used_at"].as_i64().is_some_and(|t| t > now_ms() - 10_000), "use is recorded: {keys}");
    env.delete(&format!("/api-keys/{}", key["id"].as_str().unwrap())).await;
    assert_eq!(raw.get(format!("{base}/queues")).bearer_auth(token).send().await.unwrap().status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_is_metered() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/f", Rule { fail_times: 1, ..Default::default() });
    let a = env.publish(json!({ "url": recv.url("/f"), "body": "12345" })).await;
    let b = env.publish(json!({ "url": recv.url("/g"), "body": "123" })).await;
    env.wait_state(&a, "delivered", 5000).await;
    env.wait_state(&b, "delivered", 5000).await;
    env.post("/events", json!({ "name": "metered" })).await;
    let u = env.get("/usage?days=1").await;
    let t = &u["total"];
    assert_eq!(t["published"], 2);
    assert_eq!(t["deliveries"], 3);
    assert_eq!(t["delivered"], 2);
    assert_eq!(t["failed_attempts"], 1);
    assert_eq!(t["events"], 1);
    assert!(t["bytes_in"].as_u64().unwrap() >= 8);
    assert!(u["cost"]["usd_per_million_messages"].as_f64().unwrap() > 0.0);
    assert_eq!(u["daily"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn health_and_stats() {
    let env = env().await;
    let h = env.get("/health").await;
    assert_eq!(h["ok"], true);
    assert_eq!(h["auth"], false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = env.get("/stats").await;
    assert_eq!(s["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(s["dlq"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn large_binary_bodies_arrive_intact() {
    // Over the inline limit, so on DynamoDB the body takes the S3 path.
    let env = env().await;
    let recv = Receiver::start().await;
    let body: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
    let id = env
        .publish(json!({ "url": recv.url("/big"), "body_base64": b64, "content_type": "application/octet-stream" }))
        .await;
    let hits = recv.wait("/big", 1, 5000).await;
    assert_eq!(hits[0].body.len(), body.len());
    assert!(hits[0].body == body, "body changed in transit");
    assert_eq!(hits[0].header("content-type").as_deref(), Some("application/octet-stream"));
    let m = env.wait_state(&id, "delivered", 3000).await;
    assert_eq!(m["body_base64"], b64);
    // Deleting the message removes its stored body too.
    let (s, _) = env.delete(&format!("/messages/{id}?purge=true")).await;
    assert!(s.is_success());
    let (s, _) = env.call(Method::GET, &format!("/messages/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn publishers_cannot_spoof_system_headers() {
    let env = env().await;
    let recv = Receiver::start().await;
    let id = env
        .publish(json!({
            "url": recv.url("/spoof"),
            "body": "{}",
            "headers": { "roll-message-id": "msg_fake", "webhook-signature": "v1,forged", "x-custom": "kept" },
        }))
        .await;
    let hit = &recv.wait("/spoof", 1, 5000).await[0];
    for name in ["roll-message-id", "webhook-signature", "webhook-id"] {
        assert_eq!(hit.headers.get_all(name).iter().count(), 1, "{name} sent once");
    }
    assert_eq!(hit.header("roll-message-id").unwrap(), id);
    assert_ne!(hit.header("webhook-signature").unwrap(), "v1,forged");
    assert_eq!(hit.header("x-custom").as_deref(), Some("kept"));
}

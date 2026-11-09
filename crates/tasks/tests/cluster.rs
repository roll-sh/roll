//! Many workers on one store: work spreads, nothing is lost when a worker dies or drains,
//! and nothing is delivered twice in normal operation.

mod common;

use common::*;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

fn ids_seen(recv: &Receiver, path: &str) -> HashMap<String, usize> {
    let mut seen = HashMap::new();
    for h in recv.hits(path) {
        *seen.entry(h.header("roll-message-id").unwrap()).or_insert(0) += 1;
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_spreads_without_duplicates() {
    let env = env().await;
    let b = env.node(|_| {}).await;
    let c = env.node(|_| {}).await;
    eventually(3000, "three live workers", || async {
        let s = env.get("/stats").await;
        s["nodes"].as_array().unwrap().len() == 3
    })
    .await;
    // Every lane has exactly one owner.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let s = env.get("/stats").await;
    let lanes: u64 = s["nodes"].as_array().unwrap().iter().map(|n| n["lanes"].as_u64().unwrap()).sum();
    assert_eq!(lanes, 4, "4 shards split across 3 workers: {s}");

    let recv = Receiver::start().await;
    let n = 300;
    let mut ids = Vec::new();
    for i in 0..n {
        ids.push(env.publish(json!({ "url": recv.url("/many"), "body": format!("{i}") })).await);
    }
    recv.wait("/many", n, 20_000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = ids_seen(&recv, "/many");
    assert_eq!(seen.len(), n);
    assert!(seen.values().all(|c| *c == 1), "duplicates: {:?}", seen.iter().filter(|(_, c)| **c > 1).collect::<Vec<_>>());
    drop((b, c));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_worker_loses_nothing() {
    let env = env().await;
    let mut b = env.node(|_| {}).await;
    let _c = env.node(|_| {}).await;
    eventually(3000, "three workers", || async { env.get("/stats").await["nodes"].as_array().unwrap().len() == 3 }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let recv = Receiver::start().await;
    recv.on("/slow", Rule { delay_ms: 700, ..Default::default() });
    let mut ids = Vec::new();
    for _ in 0..40 {
        ids.push(env.publish(json!({ "url": recv.url("/slow") })).await);
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    // Kill one worker mid-delivery: no drain, no hand-back.
    b.worker.take().unwrap().kill();
    for id in &ids {
        env.wait_state(id, "delivered", 15_000).await;
    }
    let seen = ids_seen(&recv, "/slow");
    assert_eq!(seen.len(), 40, "every message arrived");
    eventually(5000, "the dead worker to drop out", || async { env.get("/stats").await["nodes"].as_array().unwrap().len() == 2 }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn draining_hands_work_back() {
    let env = env_with(|c| c.drain_ms = 200).await;
    let recv = Receiver::start().await;
    recv.on("/long", Rule { delay_ms: 1500, ..Default::default() });
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(env.publish(json!({ "url": recv.url("/long") })).await);
    }
    recv.wait("/long", 3, 5000).await;
    // Deploy: the old worker drains, a new one takes over.
    env.shutdown().await;
    for id in &ids {
        let m = env.message(id).await;
        assert_eq!(m["state"], "pending", "handed back: {m}");
        assert_eq!(m["attempts"], 0, "an interrupted attempt doesn't count");
    }
    let _next = env.node(|_| {}).await;
    for id in &ids {
        env.wait_state(id, "delivered", 10_000).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn draining_waits_for_short_deliveries() {
    let env = env_with(|c| c.drain_ms = 3000).await;
    let recv = Receiver::start().await;
    recv.on("/short", Rule { delay_ms: 400, ..Default::default() });
    let id = env.publish(json!({ "url": recv.url("/short") })).await;
    recv.wait("/short", 1, 5000).await;
    env.shutdown().await;
    assert_eq!(env.message(&id).await["state"], "delivered", "finished during the drain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_deliveries_keep_their_lease() {
    // Lease 400ms, delivery 3s: heartbeats must keep extending it, or another worker would
    // deliver the same message again.
    let env = env_with(|c| c.lease_ms = 400).await;
    let _b = env.node(|c| c.lease_ms = 400).await;
    let recv = Receiver::start().await;
    recv.on("/marathon", Rule { delay_ms: 3000, ..Default::default() });
    let id = env.publish(json!({ "url": recv.url("/marathon"), "timeout": "10s" })).await;
    recv.wait("/marathon", 1, 3000).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let m = env.message(&id).await;
    assert_eq!(m["state"], "active");
    assert!(m["due_at"].as_i64().unwrap() > tasks::model::now_ms(), "lease kept alive: {m}");
    env.wait_state(&id, "delivered", 10_000).await;
    assert_eq!(recv.hits("/marathon").len(), 1);
    assert_eq!(env.message(&id).await["attempts"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_only_nodes_publish_for_workers() {
    // An API node without a worker (like the Lambda deployment) still gets work delivered.
    let env = env().await;
    let api_node = tasks::Tasks::new(fast_config(), env.store.clone()).await.unwrap();
    let recv = Receiver::start().await;
    let out = api_node
        .publish(tasks::PublishRequest { url: Some(recv.url("/via-api")), ..Default::default() })
        .await
        .unwrap();
    recv.wait("/via-api", 1, 5000).await;
    env.wait_state(&out[0].message_id, "delivered", 3000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_nodes_pick_up_changes_made_elsewhere() {
    // Like two warm Lambdas: B has no worker heartbeat, yet sees keys, apps and usage from A.
    let env = env_with(|c| c.token = Some("admin".into())).await;
    let b = tasks::Tasks::new(
        { let mut c = fast_config(); c.token = Some("admin".into()); c.refresh_ms = 300; c },
        env.store.clone(),
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b_url = format!("http://{}/v1/tasks", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, tasks::api::router(b.clone())).into_future());
    let http = reqwest::Client::new();
    let on_b = |method: reqwest::Method, path: &str, token: &str| http.request(method, format!("{b_url}{path}")).bearer_auth(token.to_string());
    tokio::time::sleep(Duration::from_millis(350)).await;

    // A key minted on A works on B right away …
    let key = env.post("/api-keys", json!({ "name": "ci" })).await;
    let token = key["token"].as_str().unwrap();
    assert_eq!(on_b(reqwest::Method::GET, "/queues", token).send().await.unwrap().status(), 200);
    // … and stops working there soon after it's revoked.
    env.delete(&format!("/api-keys/{}", key["id"].as_str().unwrap())).await;
    eventually(3000, "the revoked key to be refused on B", || async {
        on_b(reqwest::Method::GET, "/queues", token).send().await.unwrap().status() == 401
    })
    .await;

    // An app synced through A: events sent to B start its functions.
    let app = MockApp::start("elsewhere").await;
    app.function(json!({ "id": "pong", "triggers": [{ "event": "ping" }] }), |ctx| Ok(ctx.event["data"].clone()));
    app.sync(&env).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sent = on_b(reqwest::Method::POST, "/events", "admin").json(&json!({ "name": "ping", "data": 1 })).send().await.unwrap();
    assert_eq!(sent.status(), 201);
    eventually(5000, "a run from B's event", || async { env.runs("pong").await.iter().any(|r| r["status"] == "completed") }).await;

    // Usage counted on B shows up in reports.
    for _ in 0..3 {
        let r = on_b(reqwest::Method::POST, "/publish", "admin").json(&json!({ "url": "http://127.0.0.1:9/x", "delay": "1h" })).send().await.unwrap();
        assert!(r.status().is_success());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eventually(3000, "B's publishes in the usage report", || async {
        let _ = on_b(reqwest::Method::GET, "/health", "admin").send().await;
        let _ = on_b(reqwest::Method::GET, "/queues", "admin").send().await;
        env.get("/usage?days=1").await["total"]["published"].as_u64().unwrap() >= 3
    })
    .await;
}

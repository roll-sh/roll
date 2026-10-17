//! Queues: parallelism, rate limits, pausing, FIFO groups, per-key flow control, pull
//! consumers; and schedules.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::{Value, json};
use std::time::Duration;
use tasks::model::now_ms;

#[tokio::test(flavor = "multi_thread")]
async fn queue_parallelism_caps_concurrent_deliveries() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/work", Rule { delay_ms: 150, ..Default::default() });
    env.put("/queues/emails", json!({ "parallelism": 2 })).await;
    let mut ids = Vec::new();
    for i in 0..10 {
        ids.push(env.publish(json!({ "url": recv.url("/work"), "queue": "emails", "body": format!("{i}") })).await);
    }
    recv.wait("/work", 10, 10_000).await;
    for id in &ids {
        env.wait_state(id, "delivered", 5000).await;
    }
    assert_eq!(recv.max_inflight("/work"), 2);
    let q = env.get("/queues/emails").await;
    assert_eq!(q["parallelism"], 2);
    assert_eq!(q["stats"]["ready"], 0);
    assert_eq!(q["stats"]["inflight"], 0, "slots released");
    let hits = recv.hits("/work");
    assert!(hits.iter().all(|h| h.header("roll-queue").as_deref() == Some("emails")));
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_rate_limit_spreads_deliveries() {
    let env = env().await;
    let recv = Receiver::start().await;
    env.put("/queues/api", json!({ "rate": "5/s" })).await;
    let start = now_ms();
    for _ in 0..10 {
        env.publish(json!({ "url": recv.url("/r"), "queue": "api" })).await;
    }
    let hits = recv.wait("/r", 10, 10_000).await;
    // A burst of 5, then one every 200ms.
    let span = hits.last().unwrap().at - start;
    assert!(span >= 900, "10 messages at 5/s took only {span}ms");
    let in_first_second = hits.iter().filter(|h| h.at - start < 950).count();
    assert!((5..=10).contains(&in_first_second), "{in_first_second}");
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_queues_hold_messages() {
    let env = env().await;
    let recv = Receiver::start().await;
    env.put("/queues/held", json!({})).await;
    env.post("/queues/held/pause", json!({})).await;
    let id = env.publish(json!({ "url": recv.url("/h"), "queue": "held" })).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(recv.hits("/h").is_empty());
    let q = env.get("/queues/held").await;
    assert_eq!(q["paused"], true);
    assert_eq!(q["stats"]["ready"], 1);
    env.post("/queues/held/resume", json!({})).await;
    recv.wait("/h", 1, 5000).await;
    env.wait_state(&id, "delivered", 5000).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fifo_groups_deliver_in_order_one_at_a_time() {
    let env = env().await;
    let recv = Receiver::start().await;
    // The third message fails twice; nothing behind it may overtake.
    recv.on("/fifo/a", Rule { delay_ms: 20, ..Default::default() });
    recv.on("/fifo/b", Rule { delay_ms: 20, ..Default::default() });
    let mut ids = Vec::new();
    for i in 0..6 {
        let path = if i % 2 == 0 { "/fifo/a" } else { "/fifo/b" };
        ids.push(env.publish(json!({ "url": recv.url(path), "queue": "ledger", "group": "acct-1", "body": format!("{i}") })).await);
    }
    // Another group runs independently.
    let other = env.publish(json!({ "url": recv.url("/other"), "queue": "ledger", "group": "acct-2" })).await;
    env.wait_state(&other, "delivered", 5000).await;
    for id in &ids {
        env.wait_state(id, "delivered", 10_000).await;
    }
    let mut hits: Vec<_> = recv.all_hits().into_iter().filter(|h| h.path.starts_with("/fifo")).collect();
    hits.sort_by_key(|h| h.at);
    let order: Vec<String> = hits.iter().map(|h| h.text()).collect();
    assert_eq!(order, vec!["0", "1", "2", "3", "4", "5"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn fifo_waits_for_retries_of_the_head() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/head", Rule { fail_times: 2, ..Default::default() });
    let a = env.publish(json!({ "url": recv.url("/head"), "queue": "orders", "group": "g" })).await;
    let b = env.publish(json!({ "url": recv.url("/next"), "queue": "orders", "group": "g" })).await;
    env.wait_state(&b, "delivered", 10_000).await;
    let head = recv.hits("/head");
    assert_eq!(head.len(), 3);
    let next = recv.hits("/next");
    assert!(next[0].at >= head[2].at, "the second message went before the first succeeded");
    env.wait_state(&a, "delivered", 1000).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn flow_keys_limit_across_messages() {
    let env = env().await;
    let recv = Receiver::start().await;
    recv.on("/user", Rule { delay_ms: 80, ..Default::default() });
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(env.publish(json!({ "url": recv.url("/user"), "flow": { "key": "user-42", "parallelism": 1 } })).await);
    }
    for id in &ids {
        env.wait_state(id, "delivered", 10_000).await;
    }
    assert_eq!(recv.max_inflight("/user"), 1);

    // Rate per key, through the raw API.
    let start = now_ms();
    for _ in 0..4 {
        let resp = env
            .http
            .post(format!("{}/v1/tasks/publish/{}", env.api, recv.url("/rated")))
            .header("roll-flow-key", "partner")
            .header("roll-flow-rate", "2/s")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
    }
    let hits = recv.wait("/rated", 4, 10_000).await;
    assert!(hits.last().unwrap().at - start >= 900, "4 at 2/s took {}ms", hits.last().unwrap().at - start);
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_consumers_lease_ack_nack_extend() {
    let env = env().await;
    env.put("/queues/jobs", json!({ "retries": 1 })).await;
    let a = env.publish(json!({ "queue": "jobs", "body": {"job": 1} })).await;
    let b = env.publish(json!({ "queue": "jobs", "body": "two" })).await;

    let got = env.post("/queues/jobs/pull", json!({ "max": 10, "visibility": "500ms" })).await;
    let msgs = got["messages"].as_array().unwrap().clone();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["id"], a.as_str());
    assert_eq!(msgs[0]["body"], r#"{"job":1}"#);
    assert_eq!(msgs[1]["body"], "two");
    assert_eq!(msgs[0]["attempts"], 1);
    // Leased messages aren't handed out twice.
    assert!(env.post("/queues/jobs/pull", json!({ "max": 10 })).await["messages"].as_array().unwrap().is_empty());

    // Ack the first.
    env.post("/queues/jobs/ack", json!({ "receipt": msgs[0]["receipt"] })).await;
    env.wait_state(&a, "delivered", 1000).await;
    let (s, _) = env.call(Method::POST, "/queues/jobs/ack", Some(json!({ "receipt": msgs[0]["receipt"] }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "a receipt settles once");

    // Extend the second, then let its lease lapse: it comes back.
    let ext = env.post("/queues/jobs/extend", json!({ "receipt": msgs[1]["receipt"], "visibility": "700ms" })).await;
    assert!(ext["lease_until"].as_i64().unwrap() > now_ms() + 500);
    let start = now_ms();
    let again = env.post("/queues/jobs/pull", json!({ "max": 1, "wait": "3s" })).await;
    assert!(now_ms() - start >= 500, "long poll waited for the lease");
    let m = &again["messages"][0];
    assert_eq!(m["id"], b.as_str());
    assert_eq!(m["attempts"], 2);
    let (s, _) = env.call(Method::POST, "/queues/jobs/ack", Some(json!({ "receipt": msgs[1]["receipt"] }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "the old receipt is dead");

    // Nack after the last retry: dead letter.
    let n = env.post("/queues/jobs/nack", json!({ "receipt": m["receipt"], "error": "bad input" })).await;
    assert_eq!(n["state"], "failed");
    let failed = env.message(&b).await;
    assert_eq!(failed["reason"], "bad input");

    // Nack with a delay retries later.
    let c = env.publish(json!({ "queue": "jobs", "body": "three" })).await;
    let p = env.post("/queues/jobs/pull", json!({})).await;
    let n = env.post("/queues/jobs/nack", json!({ "receipt": p["messages"][0]["receipt"], "delay": "300ms" })).await;
    assert_eq!(n["state"], "pending");
    assert!(env.post("/queues/jobs/pull", json!({})).await["messages"].as_array().unwrap().is_empty());
    let p = env.post("/queues/jobs/pull", json!({ "wait": "2s" })).await;
    assert_eq!(p["messages"][0]["id"], c.as_str());
    env.post("/queues/jobs/ack", json!({ "receipts": [p["messages"][0]["receipt"]] })).await;

    // A consumer that knows retrying won't help dead-letters it at once.
    let d = env.publish(json!({ "queue": "jobs", "body": "poison", "retries": 5 })).await;
    let p = env.post("/queues/jobs/pull", json!({})).await;
    let n = env.post("/queues/jobs/nack", json!({ "receipt": p["messages"][0]["receipt"], "error": "unparseable", "dead": true })).await;
    assert_eq!(n["state"], "failed");
    assert_eq!(env.message(&d).await["reason"], "unparseable");
    assert!(env.get("/dlq").await["items"].as_array().unwrap().iter().any(|m| m["id"] == d.as_str()));

    // Long poll wakes up on publish.
    let env2 = &env;
    let poll = async { env2.post("/queues/jobs/pull", json!({ "wait": "5s" })).await };
    let publish = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        env2.publish(json!({ "queue": "jobs", "body": "late" })).await
    };
    let (got, _) = tokio::join!(poll, publish);
    assert_eq!(got["messages"][0]["body"], "late");
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_respects_groups_and_parallelism() {
    let env = env().await;
    env.put("/queues/serial", json!({ "parallelism": 1 })).await;
    env.publish(json!({ "queue": "serial", "body": "1", "group": "g" })).await;
    env.publish(json!({ "queue": "serial", "body": "2", "group": "g" })).await;
    let p = env.post("/queues/serial/pull", json!({ "max": 5 })).await;
    let msgs = p["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["body"], "1");
    assert_eq!(msgs[0]["group"], "g");
    env.post("/queues/serial/ack", json!({ "receipt": msgs[0]["receipt"] })).await;
    let p = env.post("/queues/serial/pull", json!({ "max": 5 })).await;
    assert_eq!(p["messages"][0]["body"], "2");
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_admin() {
    let env = env().await;
    let q = env.put("/queues/admin", json!({ "parallelism": 3, "rate": "10/s", "retries": 7, "timeout": "9s", "partitions": 2 })).await;
    assert_eq!(q["parallelism"], 3);
    assert_eq!(q["rate"], json!({"limit": 10, "period_ms": 1000}));
    assert_eq!(q["retries"], 7);
    assert_eq!(q["timeout_ms"], 9000);
    let q = env.put("/queues/admin", json!({ "parallelism": 0, "rate": null })).await;
    assert!(q.get("parallelism").is_none() && q.get("rate").is_none(), "{q}");
    assert_eq!(q["retries"], 7, "untouched fields stay");
    let (s, _) = env.call(Method::PUT, "/queues/admin", Some(json!({ "partitions": 1 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "partitions only grow");

    // Queue defaults apply to messages.
    let id = env.publish(json!({ "url": "http://127.0.0.1:1/", "queue": "admin", "delay": "1h" })).await;
    let m = env.message(&id).await;
    assert_eq!((m["retries"].as_u64(), m["timeout_ms"].as_i64()), (Some(7), Some(9000)));
    assert!(m["lane"].as_str().unwrap().starts_with("q:admin:"));

    // Publishing to an unknown queue creates it.
    env.publish(json!({ "url": "http://127.0.0.1:1/", "queue": "auto", "delay": "1h" })).await;
    let names: Vec<Value> = env.get("/queues").await.as_array().unwrap().iter().map(|q| q["name"].clone()).collect();
    assert!(names.contains(&json!("auto")) && names.contains(&json!("admin")));

    let (s, _) = env.delete("/queues/admin").await;
    assert_eq!(s, StatusCode::CONFLICT, "has unfinished messages");
    let (s, v) = env.call(Method::DELETE, "/queues/admin?force=true", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["canceled"], 1);
    assert_eq!(env.message(&id).await["state"], "canceled");
    let (s, _) = env.call(Method::GET, "/queues/admin", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// --- schedules ---

#[tokio::test(flavor = "multi_thread")]
async fn cron_schedules_fire_and_can_be_managed() {
    let env = env().await;
    let recv = Receiver::start().await;
    let s = env
        .post("/schedules", json!({
            "id": "heartbeat",
            "cron": "* * * * * *",
            "publish": { "url": recv.url("/tick"), "body": {"ping": true}, "headers": {"x-from": "cron"} },
            "label": "hb",
        }))
        .await;
    assert_eq!(s["id"], "heartbeat");
    assert_eq!(s["upcoming"].as_array().unwrap().len(), 5);
    let hits = recv.wait("/tick", 2, 5000).await;
    assert_eq!(hits[0].json(), json!({"ping": true}));
    assert_eq!(hits[0].header("x-from").as_deref(), Some("cron"));
    assert_eq!(hits[0].header("roll-schedule-id").as_deref(), Some("heartbeat"));
    assert_eq!(hits[0].headers.get_all("roll-schedule-id").iter().count(), 1, "sent once");
    let gap = hits[1].at - hits[0].at;
    assert!((700..=1400).contains(&gap), "fires every second, got {gap}ms");
    let s = env.get("/schedules/heartbeat").await;
    assert!(s["last_at"].is_i64() && s["last_message_id"].is_string());
    let listed = env.get("/messages?schedule_id=heartbeat").await;
    assert!(!listed["items"].as_array().unwrap().is_empty());

    env.post("/schedules/heartbeat/pause", json!({})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = recv.hits("/tick").len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(recv.hits("/tick").len(), n, "paused schedules don't fire");
    assert!(env.get("/schedules/heartbeat").await.get("upcoming").is_none());

    let fired = env.post("/schedules/heartbeat/trigger", json!({})).await;
    assert_eq!(fired["fired"].as_array().unwrap().len(), 1);
    recv.wait("/tick", n + 1, 3000).await;

    env.patch("/schedules/heartbeat", json!({ "paused": false, "cron": "@every 1s", "publish": { "url": recv.url("/tock") } })).await;
    recv.wait("/tock", 2, 5000).await;

    let (s, _) = env.delete("/schedules/heartbeat").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = recv.hits("/tock").len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(recv.hits("/tock").len(), n, "deleted schedules don't fire");
}

#[tokio::test(flavor = "multi_thread")]
async fn schedules_target_queues_events_and_validate() {
    let env = env().await;
    let recv = Receiver::start().await;
    env.post("/schedules", json!({ "id": "to-queue", "cron": "* * * * * *", "publish": { "queue": "nightly", "body": "job" } })).await;
    let p = env.post("/queues/nightly/pull", json!({ "wait": "3s" })).await;
    assert_eq!(p["messages"][0]["body"], "job");

    env.post("/schedules", json!({ "id": "to-event", "cron": "* * * * * *", "event": { "name": "cron.tick", "data": {"n": 1} } })).await;
    eventually(4000, "a scheduled event", || async { !env.get("/events?name=cron.tick").await["items"].as_array().unwrap().is_empty() }).await;
    let ev = &env.get("/events?name=cron.tick").await["items"][0];
    assert_eq!(ev["data"], json!({"n": 1}));

    for bad in [
        json!({ "cron": "nope", "publish": { "url": recv.url("/x") } }),
        json!({ "cron": "* * * * *" }),
        json!({ "cron": "* * * * *", "publish": { "url": recv.url("/x") }, "event": { "name": "e" } }),
        json!({ "cron": "* * * * *", "tz": "Nowhere/City", "publish": { "url": recv.url("/x") } }),
        json!({ "cron": "* * * * *", "function": "missing" }),
    ] {
        let (s, v) = env.call(Method::POST, "/schedules", Some(bad.clone())).await;
        assert!(s.is_client_error(), "{bad} → {s} {v}");
    }
    let s = env.post("/schedules", json!({ "cron": "0 9 * * MON-FRI", "tz": "Europe/Budapest", "publish": { "url": recv.url("/x") } })).await;
    assert!(s["id"].as_str().unwrap().starts_with("sch_"));
    assert!(s["next_at"].as_i64().unwrap() > now_ms());
    let (s2, _) = env.call(Method::POST, "/schedules", Some(json!({ "id": "to-event", "cron": "* * * * *", "event": { "name": "x" } }))).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert!(env.get("/schedules").await.as_array().unwrap().len() >= 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_fires_once_per_time_across_workers() {
    let env = env().await;
    let _b = env.node(|_| {}).await;
    let _c = env.node(|_| {}).await;
    let recv = Receiver::start().await;
    env.post("/schedules", json!({ "cron": "* * * * * *", "publish": { "url": recv.url("/once") } })).await;
    tokio::time::sleep(Duration::from_millis(3600)).await;
    let hits = recv.hits("/once");
    assert!((2..=4).contains(&hits.len()), "{} fires in 3.6s", hits.len());
    let mut seconds: Vec<i64> = hits.iter().map(|h| (h.at + 500) / 1000).collect();
    seconds.dedup();
    assert_eq!(seconds.len(), hits.len(), "two fires in the same second");
}

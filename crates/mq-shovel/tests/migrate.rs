//! Live migration against a stand-in RabbitMQ: a roll broker plays the source's AMQP side and a
//! small HTTP server plays its management API, reporting a port nobody listens on (a remapped
//! port), so the shovels only work through the `amqp_uri` override.

mod common;

use axum::Json;
use axum::routing::get;
use common::*;
use mq_shovel::MigrateRequest;
use serde_json::{Value, json};

/// Serve `/api/definitions` and `/api/overview` like a RabbitMQ 3.x management API.
async fn fake_management(defs: Value) -> String {
    let overview = json!({"listeners": [{"protocol": "amqp", "port": 1}, {"protocol": "clustering", "port": 25672}]});
    let app = axum::Router::new()
        .route("/api/definitions", get(move || async move { Json(defs) }))
        .route("/api/overview", get(move || async move { Json(overview) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrate_with_remapped_port_skips_and_stops() {
    let source = Node::start("source").await;
    let (_cs, chs) = source.channel().await;
    declare(&chs, "orders").await;
    publish_n(&chs, "", "orders", "o", 30).await;

    let defs = json!({
        "rabbit_version": "3.13.7",
        "users": [], "vhosts": [{"name": "/"}], "permissions": [],
        "parameters": [
            {"component": "shovel", "vhost": "/", "name": "legacy-move", "value": {"src-uri": "amqp://", "src-queue": "orders", "dest-uri": "amqp://", "dest-queue": "elsewhere"}},
            {"component": "federation-upstream", "vhost": "/", "name": "dc2", "value": {"uri": "amqp://dc2"}},
        ],
        "policies": [
            {"vhost": "/", "name": "ha-all", "pattern": ".*", "apply-to": "queues", "definition": {"ha-mode": "all", "ha-sync-batch-size": 50}, "priority": 0},
            {"vhost": "/", "name": "ttl", "pattern": "^orders$", "apply-to": "queues", "definition": {"ha-mode": "exactly", "ha-params": 2, "message-ttl": 3600000}, "priority": 1},
            {"vhost": "/", "name": "erlang-re", "pattern": "^(?!amq\\.).*", "apply-to": "all", "definition": {"max-length": 100}, "priority": 0},
        ],
        "exchanges": [{"vhost": "/", "name": "orders.x", "type": "topic", "durable": true, "auto_delete": false, "internal": false, "arguments": {}}],
        "queues": [
            {"vhost": "/", "name": "orders", "durable": true, "auto_delete": false, "arguments": {}},
            {"vhost": "/", "name": "broken", "durable": true, "auto_delete": false, "arguments": {"x-queue-type": "nonsense"}},
        ],
        "bindings": [{"vhost": "/", "source": "orders.x", "destination": "orders", "destination_type": "queue", "routing_key": "order.#", "arguments": {}}],
    });
    let mgmt = fake_management(defs).await;
    let roll = Node::start("roll").await;
    let req = MigrateRequest {
        management_url: mgmt,
        username: "guest".into(),
        password: "guest".into(),
        move_messages: true,
        amqp_uri: Some(format!("amqp://{}", source.addr)),
        ..Default::default()
    };
    let report = mq_shovel::migrate(&roll.broker, req).await.expect("migration goes through despite bad items");

    // What was left out, and why.
    let skipped = |kind: &str, name: &str| report.skipped.iter().find(|s| s.kind == kind && s.name == name).unwrap_or_else(|| panic!("{kind} {name} not in {:#?}", report.skipped)).clone();
    assert!(skipped("parameter", "shovel/legacy-move").reason.contains("include_shovels"));
    assert!(skipped("parameter", "federation-upstream/dc2").reason.contains("include_shovels"));
    assert!(!skipped("policy", "ha-all").partial);
    assert!(skipped("policy", "ttl").partial);
    assert!(skipped("policy", "erlang-re").reason.contains("look-around"));
    skipped("queue", "broken");
    skipped("shovel", "roll-migrate-broken");
    assert_eq!(report.imported.queues, 1);
    assert_eq!(report.imported.policies, 1, "ttl is imported without its ha keys");
    assert_eq!(report.imported.exchanges, 1);
    assert_eq!(report.imported.bindings, 1);
    assert_eq!(report.shovels, vec!["roll-migrate-orders".to_string()]);
    assert!(roll.broker.parameter("shovel", "/", "legacy-move").is_none());
    assert!(roll.broker.parameter("federation-upstream", "/", "dc2").is_none());
    let ttl = roll.broker.export_definitions(None).await.unwrap()["policies"].as_array().unwrap().iter().find(|p| p["name"] == "ttl").cloned().unwrap();
    assert_eq!(ttl["definition"], json!({"message-ttl": 3600000}));

    // The override, not the reported port, is what the shovel uses.
    until(20, "30 messages moved", async || roll.queue_len("orders") == 30).await;
    until(10, "shovel drained", async || {
        roll.shovels.migration_status().first().is_some_and(|s| s["state"] == "running" && s["remaining"] == 0 && s["transferred"] == 30)
    })
    .await;
    let src = roll.shovels.migration_status()[0]["src"].as_str().unwrap().to_string();
    assert!(src.contains(&source.addr.port().to_string()) && !src.contains("guest:guest"), "{src}");

    // Stopping removes the shovel and its stored credentials.
    let stopped = mq_shovel::stop_migration_body(&roll.broker).await.unwrap();
    assert_eq!(stopped, json!({"stopped": ["roll-migrate-orders"]}));
    assert!(roll.broker.parameters(Some("shovel"), None).is_empty());
    until(5, "shovel gone", async || roll.shovels.migration_status().is_empty()).await;
    publish_n(&chs, "", "orders", "late", 3).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(source.queue_len("orders"), 3, "nothing moves after stop");
    assert_eq!(mq_shovel::stop_migration(&roll.broker).await.unwrap(), Vec::<String>::new());
}

//! Live migration, shovels and federation against a real RabbitMQ (`rabbitmq:4-management` in
//! Docker). Runs only with `ROLL_E2E=1`.

mod common;

use common::*;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Connection, ConnectionProperties};
use mq_core::policy::Policy;
use mq_shovel::MigrateRequest;
use serde_json::json;
use std::process::Command;

const USER: &str = "roll";
const PASS: &str = "rollpass";

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", "-v", &self.0]).output();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrate_shovel_and_federate_with_rabbitmq() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set ROLL_E2E=1 to run against RabbitMQ in Docker");
        return;
    }
    // RabbitMQ listens on the same port inside and outside the container, so the port it
    // reports in /api/overview is the one migration connects to.
    let (amqp_port, http_port) = (free_port(), free_port());
    let name = format!("roll-shovel-e2e-{}", std::process::id());
    let out = Command::new("docker")
        .args(["run", "-d", "--rm", "--name", &name, "--hostname", "rabbit-e2e"])
        .args(["-e", &format!("RABBITMQ_NODE_PORT={amqp_port}"), "-e", "RABBITMQ_DIST_PORT=25672", "-e", &format!("RABBITMQ_DEFAULT_USER={USER}"), "-e", &format!("RABBITMQ_DEFAULT_PASS={PASS}")])
        .args(["-p", &format!("127.0.0.1:{amqp_port}:{amqp_port}"), "-p", &format!("127.0.0.1:{http_port}:15672"), "rabbitmq:4-management"])
        .output()
        .expect("docker");
    assert!(out.status.success(), "docker run failed: {}", String::from_utf8_lossy(&out.stderr));
    let _container = Container(name);

    let mgmt = format!("http://127.0.0.1:{http_port}");
    let rabbit_url = format!("amqp://{USER}:{PASS}@127.0.0.1:{amqp_port}/%2f");
    let http = reqwest::Client::new();
    until(180, "RabbitMQ to start", async || {
        let ok = http.get(format!("{mgmt}/api/overview")).basic_auth(USER, Some(PASS)).send().await.is_ok_and(|r| r.status().is_success());
        ok && Connection::connect(&rabbit_url, ConnectionProperties::default()).await.is_ok()
    })
    .await;

    // 1. Topology and 1000 messages in RabbitMQ.
    let rconn = Connection::connect(&rabbit_url, ConnectionProperties::default()).await.unwrap();
    let rch = rconn.create_channel().await.unwrap();
    rch.exchange_declare("orders.x".into(), lapin::ExchangeKind::Topic, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default())
        .await
        .unwrap();
    let mut qargs = FieldTable::default();
    qargs.insert("x-max-length".into(), AMQPValue::LongLongInt(100_000));
    rch.queue_declare("orders".into(), QueueDeclareOptions { durable: true, ..Default::default() }, qargs).await.unwrap();
    rch.queue_bind("orders".into(), "orders.x".into(), "order.#".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    rch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
    let mut confirms = Vec::new();
    for i in 0..1000 {
        let mut h = FieldTable::default();
        h.insert("n".into(), AMQPValue::LongLongInt(i));
        let props = BasicProperties::default()
            .with_delivery_mode(2)
            .with_message_id(i.to_string().into())
            .with_correlation_id(format!("c{i}").into())
            .with_content_type("application/json".into())
            .with_headers(h);
        let body = format!("{{\"n\":{i}}}");
        confirms.push(rch.basic_publish("orders.x".into(), "order.created".into(), BasicPublishOptions::default(), body.as_bytes(), props).await.unwrap());
    }
    for c in confirms {
        assert!(c.await.unwrap().is_ack());
    }

    // 2. Migrate, moving messages.
    let roll = Node::start("roll-e2e").await;
    let report = mq_shovel::migrate(
        &roll.broker,
        MigrateRequest { management_url: mgmt.clone(), username: USER.into(), password: PASS.into(), vhost: None, move_messages: true, ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(report.imported.queues, 1);
    assert_eq!(report.imported.exchanges, 1);
    assert_eq!(report.imported.bindings, 1);
    assert!(report.imported.users >= 1 && report.imported.vhosts >= 1);
    assert_eq!(report.shovels, vec!["roll-migrate-orders".to_string()]);
    roll.broker.authenticate(USER, PASS, None).expect("RabbitMQ password hashes work in roll");

    // 3. All 1000 arrive in order with their properties.
    until(60, "1000 messages migrated", async || roll.queue_len("orders") == 1000).await;
    let (_c, ch) = roll.channel().await;
    let got = drain(&ch, "orders").await;
    assert_eq!(got.len(), 1000);
    for (i, m) in got.iter().enumerate() {
        assert_eq!(body(m), format!("{{\"n\":{i}}}"));
        let p = &m.delivery.properties;
        assert_eq!(p.message_id().as_ref().map(|s| s.to_string()), Some(i.to_string()));
        assert_eq!(p.correlation_id().as_ref().map(|s| s.to_string()), Some(format!("c{i}")));
        assert_eq!(p.content_type().as_ref().map(|s| s.as_str()), Some("application/json"));
        assert_eq!(p.delivery_mode(), &Some(2));
        assert_eq!(p.headers().as_ref().and_then(|h| h.inner().get("n").cloned()), Some(AMQPValue::LongLongInt(i as i64)));
    }
    let x = roll.broker.vhost("/").unwrap().exchange("orders.x").expect("exchange imported");
    assert_eq!(x.kind.name(), "topic");
    until(10, "migration status", async || {
        roll.shovels.migration_status().first().is_some_and(|s| s["transferred"] == 1000 && s["state"] == "running" && s["remaining"] == 0)
    })
    .await;
    // Producers still on RabbitMQ keep being moved until the shovel is removed.
    rch.basic_publish("orders.x".into(), "order.late".into(), BasicPublishOptions::default(), b"late", BasicProperties::default()).await.unwrap().await.unwrap();
    until(20, "late message migrated", async || roll.queue_len("orders") == 1).await;

    // 4. Shovel from roll back to RabbitMQ.
    declare(&ch, "outbound").await;
    publish_n(&ch, "", "outbound", "back", 100).await;
    roll.broker
        .put_parameter("shovel", "/", "back", json!({"src-uri": "amqp://", "src-queue": "outbound", "dest-uri": rabbit_url, "dest-queue": "from-roll"}))
        .await
        .unwrap();
    until(30, "100 messages in RabbitMQ", async || {
        // A failed passive declare closes the channel, so use a throwaway one.
        let Ok(tmp) = rconn.create_channel().await else { return false };
        tmp.queue_declare("from-roll".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default())
            .await
            .is_ok_and(|q| q.message_count() == 100)
    })
    .await;
    let mut back = Vec::new();
    while let Some(m) = rch.basic_get("from-roll".into(), BasicGetOptions { no_ack: true }).await.unwrap() {
        back.push(String::from_utf8_lossy(&m.delivery.data).into_owned());
    }
    assert_eq!(back, (0..100).map(|i| format!("back-{i}")).collect::<Vec<_>>());

    // 5. Federate an exchange from RabbitMQ.
    ch.exchange_declare("fed.x".into(), lapin::ExchangeKind::Topic, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default())
        .await
        .unwrap();
    declare(&ch, "fed.q").await;
    ch.queue_bind("fed.q".into(), "fed.x".into(), "metrics.#".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    roll.broker.put_parameter("federation-upstream", "/", "rabbit", json!({"uri": rabbit_url, "max-hops": 1})).await.unwrap();
    let policy = Policy {
        vhost: "/".into(),
        name: "federate".into(),
        pattern: "^fed\\.x$".into(),
        apply_to: "exchanges".into(),
        definition: json!({"federation-upstream-set": "all"}),
        priority: 0,
    };
    roll.broker.put_policy(policy, false).await.unwrap();
    until(30, "federation link running", async || roll.shovels.federation_status().first().is_some_and(|l| l["status"] == "running")).await;
    for i in 0..10 {
        rch.basic_publish("fed.x".into(), "metrics.cpu".into(), BasicPublishOptions::default(), format!("f{i}").as_bytes(), BasicProperties::default())
            .await
            .unwrap()
            .await
            .unwrap();
    }
    rch.basic_publish("fed.x".into(), "logs.x".into(), BasicPublishOptions::default(), b"not bound", BasicProperties::default()).await.unwrap().await.unwrap();
    until(20, "federated messages", async || roll.queue_len("fed.q") == 10).await;
    let fed = drain(&ch, "fed.q").await;
    assert_eq!(fed.iter().map(body).collect::<Vec<_>>(), (0..10).map(|i| format!("f{i}")).collect::<Vec<_>>());
    let h = fed[0].delivery.properties.headers().as_ref().expect("x-received-from").inner();
    let Some(AMQPValue::FieldArray(hops)) = h.get("x-received-from") else { panic!("no x-received-from: {h:?}") };
    let Some(AMQPValue::FieldTable(hop)) = hops.as_slice().first() else { panic!() };
    assert_eq!(hop.inner().get("exchange"), Some(&AMQPValue::LongString("fed.x".into())));
    assert_eq!(hop.inner().get("cluster-name"), Some(&AMQPValue::LongString("rabbit@rabbit-e2e".into())));

    roll.shovels.stop();
}

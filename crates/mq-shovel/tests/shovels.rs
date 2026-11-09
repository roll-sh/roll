//! Shovels and federation between two (or three) in-process brokers, each with its own AMQP
//! listener; local endpoints use `amqp://` (in-process), remote ones go over TCP.

mod common;

use common::*;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable};
use mq_core::policy::Policy;
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;

fn status_of(n: &Node, name: &str) -> serde_json::Value {
    n.shovels.status().into_iter().find(|s| s["name"] == name).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shovel_local_to_remote() {
    let a = Node::start("a").await;
    let b = Node::start("b").await;
    let (_ca, cha) = a.channel().await;
    declare(&cha, "src").await;
    publish_n(&cha, "", "src", "m", 200).await;

    let def = json!({"src-uri": "amqp://", "src-queue": "src", "dest-uri": b.url, "dest-queue": "dst", "src-prefetch-count": 25});
    a.broker.put_parameter("shovel", "/", "to-b", def).await.unwrap();
    until(20, "200 messages on b", async || b.queue_len("dst") == 200).await;
    until(5, "source drained", async || a.queue_len("src") == 0).await;

    let (_cb, chb) = b.channel().await;
    let got = drain(&chb, "dst").await;
    let bodies: Vec<String> = got.iter().map(body).collect();
    assert_eq!(bodies, (0..200).map(|i| format!("m-{i}")).collect::<Vec<_>>());
    let p = &got[7].delivery.properties;
    assert_eq!(p.message_id().as_ref().map(|s| s.as_str()), Some("7"));
    assert_eq!(p.delivery_mode(), &Some(2));
    assert_eq!(p.content_type().as_ref().map(|s| s.as_str()), Some("text/plain"));

    let s = status_of(&a, "to-b");
    assert_eq!(s["state"], "running", "{s}");
    assert_eq!(s["type"], "dynamic");
    assert_eq!(s["src_uri"], "amqp://");
    assert!(!s["dest_uri"].as_str().unwrap().contains("guest"), "credentials are redacted: {s}");
    assert_eq!(s["forwarded"], 200);

    // Live: new messages keep flowing; deleting the parameter stops the shovel.
    publish_n(&cha, "", "src", "late", 5).await;
    until(10, "late messages", async || b.queue_len("dst") == 5).await;
    a.broker.delete_parameter("shovel", "/", "to-b").await.unwrap();
    until(5, "shovel removed", async || a.shovels.status().is_empty()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shovel_remote_to_local_exchange_with_headers() {
    let a = Node::start("a").await;
    let b = Node::start("b").await;
    let (_cb, chb) = b.channel().await;
    declare(&chb, "remote-src").await;
    publish_n(&chb, "", "remote-src", "r", 50).await;

    let (_ca, cha) = a.channel().await;
    cha.exchange_declare("landing.x".into(), lapin::ExchangeKind::Direct, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default())
        .await
        .unwrap();
    declare(&cha, "landing").await;
    cha.queue_bind("landing".into(), "landing.x".into(), "k".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();

    let def = json!({
        "src-uri": [b.url], "src-queue": "remote-src",
        "dest-uri": "amqp://", "dest-exchange": "landing.x", "dest-exchange-key": "k",
        "dest-add-forward-headers": true, "dest-add-timestamp-header": true,
        "dest-publish-properties": {"app_id": "shovelled"},
        "ack-mode": "on-publish",
    });
    a.broker.put_parameter("shovel", "/", "from-b", def).await.unwrap();
    until(20, "50 messages landed", async || a.queue_len("landing") == 50).await;
    until(5, "remote source drained", async || b.queue_len("remote-src") == 0).await;

    let got = drain(&cha, "landing").await;
    assert_eq!(body(&got[0]), "r-0");
    let p = &got[0].delivery.properties;
    assert_eq!(p.app_id().as_ref().map(|s| s.as_str()), Some("shovelled"));
    let headers = p.headers().as_ref().unwrap().inner();
    let Some(AMQPValue::FieldArray(hops)) = headers.get("x-shovelled") else { panic!("no x-shovelled header: {headers:?}") };
    let Some(AMQPValue::FieldTable(hop)) = hops.as_slice().first() else { panic!() };
    assert_eq!(hop.inner().get("shovel-name"), Some(&AMQPValue::LongString("from-b".into())));
    assert_eq!(hop.inner().get("src-queue"), Some(&AMQPValue::LongString("remote-src".into())));
    assert!(headers.contains_key("x-shovelled-timestamp"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_confirm_loses_nothing_when_destination_restarts() {
    let a = Node::start("a").await;
    let mut b = Node::start("b").await;
    {
        let (_cb, chb) = b.channel().await;
        declare(&chb, "dst").await;
    }
    let (_ca, cha) = a.channel().await;
    declare(&cha, "src").await;
    const N: usize = 3000;
    publish_n(&cha, "", "src", "m", N).await;

    let def = json!({"src-uri": "amqp://", "src-queue": "src", "dest-uri": b.url, "dest-queue": "dst",
                     "ack-mode": "on-confirm", "src-prefetch-count": 50, "reconnect-delay": 0.2});
    a.broker.put_parameter("shovel", "/", "durable", def).await.unwrap();
    until(20, "transfer under way", async || b.queue_len("dst") > 0).await;
    let moved_before = b.queue_len("dst");
    assert!(moved_before < N as u64, "restart must happen mid-transfer");
    b = b.restart().await;

    until(60, "source drained", async || a.queue_len("src") == 0).await;
    until(10, "shovel running again", async || status_of(&a, "durable")["state"] == "running").await;
    let (_cb, chb) = b.channel().await;
    let got = drain(&chb, "dst").await;
    let ids: HashSet<String> = got.iter().map(body).collect();
    assert_eq!(ids.len(), N, "every message arrived (duplicates allowed): {} of {N}, {} deliveries", ids.len(), got.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_after_queue_length() {
    let a = Node::start("a").await;
    let (_ca, cha) = a.channel().await;
    declare(&cha, "batch").await;
    publish_n(&cha, "", "batch", "b", 30).await;

    let def = json!({"src-uri": "amqp://", "src-queue": "batch", "src-delete-after": "queue-length",
                     "dest-uri": "amqp://", "dest-queue": "batch-out"});
    a.broker.put_parameter("shovel", "/", "once", def).await.unwrap();
    until(20, "30 messages moved", async || a.queue_len("batch-out") == 30).await;
    until(10, "shovel deleted itself", async || a.broker.parameter("shovel", "/", "once").is_none()).await;
    until(5, "status cleared", async || a.shovels.status().is_empty()).await;

    publish_n(&cha, "", "batch", "after", 3).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(a.queue_len("batch"), 3, "nothing moves after the shovel finished");
    assert_eq!(a.queue_len("batch-out"), 30);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_and_unsupported_shovels_report_terminated() {
    let a = Node::start("a").await;
    a.broker.put_parameter("shovel", "/", "bad", json!({"src-uri": "amqp://", "dest-uri": "amqp://"})).await.unwrap();
    a.broker
        .put_parameter("shovel", "/", "amqp10", json!({"src-protocol": "amqp10", "src-uri": "amqp://", "src-address": "q", "dest-uri": "amqp://", "dest-queue": "x"}))
        .await
        .unwrap();
    until(5, "terminated", async || {
        let s = a.shovels.status();
        s.len() == 2 && s.iter().all(|x| x["state"] == "terminated" && x["reason"].is_string())
    })
    .await;
    assert!(mq_shovel::validate_parameter("shovel", &json!({"src-uri": "amqp://", "dest-uri": "amqp://"})).is_err());
    assert!(mq_shovel::validate_parameter("shovel", &json!({"src-uri": "amqp://", "src-queue": "q", "dest-uri": "amqp://"})).is_ok());
}

async fn federate(node: &Node, upstream: &str, uri: &str, extra: serde_json::Value, pattern: &str, apply_to: &str) {
    let mut def = json!({"uri": uri, "reconnect-delay": 0.2});
    def.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    node.broker.put_parameter("federation-upstream", "/", upstream, def).await.unwrap();
    let policy = Policy {
        vhost: "/".into(),
        name: format!("fed-{upstream}"),
        pattern: pattern.into(),
        apply_to: apply_to.into(),
        definition: json!({"federation-upstream-set": "all"}),
        priority: 0,
    };
    node.broker.put_policy(policy, false).await.unwrap();
}

async fn link_running(node: &Node, n: usize) {
    until(20, "federation links running", async || {
        let l = node.shovels.federation_status();
        l.len() == n && l.iter().all(|x| x["status"] == "running")
    })
    .await;
}

fn hops(m: &lapin::message::BasicGetMessage) -> Vec<String> {
    let Some(h) = m.delivery.properties.headers() else { return vec![] };
    let Some(AMQPValue::FieldArray(a)) = h.inner().get("x-received-from") else { return vec![] };
    a.as_slice()
        .iter()
        .filter_map(|e| match e {
            AMQPValue::FieldTable(t) => match t.inner().get("cluster-name") {
                Some(AMQPValue::LongString(s)) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exchange_federation_respects_max_hops() {
    let up = Node::start("fed-a").await;
    let mid = Node::start("fed-b").await;
    let down = Node::start("fed-c").await;
    let topic = |ch: lapin::Channel| async move {
        ch.exchange_declare("fed".into(), lapin::ExchangeKind::Topic, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default())
            .await
            .unwrap();
    };
    let (_c1, ch_up) = up.channel().await;
    topic(ch_up.clone()).await;
    let (_c2, ch_mid) = mid.channel().await;
    topic(ch_mid.clone()).await;
    declare(&ch_mid, "qb").await;
    ch_mid.queue_bind("qb".into(), "fed".into(), "a.#".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    let (_c3, ch_down) = down.channel().await;
    topic(ch_down.clone()).await;
    declare(&ch_down, "qc").await;
    ch_down.queue_bind("qc".into(), "fed".into(), "#".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();

    federate(&mid, "a", &up.url, json!({}), "^fed$", "exchanges").await;
    federate(&down, "b", &mid.url, json!({"max-hops": 1}), "^fed$", "exchanges").await;
    link_running(&mid, 1).await;
    link_running(&down, 1).await;
    // The upstream mirrors the downstream's binding onto `federation: fed -> fed-b A`.
    let link = &mid.shovels.federation_status()[0];
    assert_eq!((link["type"].as_str(), link["exchange"].as_str(), link["upstream"].as_str()), (Some("exchange"), Some("fed"), Some("a")));
    assert!(up.broker.vhost("/").unwrap().exchange("federation: fed -> fed-b A").is_some());
    assert!(up.broker.vhost("/").unwrap().queue("federation: fed -> fed-b").is_some());

    publish_n(&ch_up, "fed", "a.1", "from-a", 3).await;
    publish_n(&ch_mid, "fed", "b.1", "from-b", 2).await;
    publish_n(&ch_up, "fed", "z.1", "unbound", 2).await;
    until(10, "a's messages at b", async || mid.queue_len("qb") == 3).await;
    until(10, "b's messages at c", async || down.queue_len("qc") == 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(down.queue_len("qc"), 2, "a's messages are 1 hop away at b: max-hops 1 stops them");
    let at_b = drain(&ch_mid, "qb").await;
    assert_eq!(hops(&at_b[0]), vec!["fed-a"]);
    let at_c = drain(&ch_down, "qc").await;
    assert_eq!(at_c.iter().map(body).collect::<Vec<_>>(), vec!["from-b-0", "from-b-1"]);

    // Two hops allowed: a → b → c.
    federate(&down, "b", &mid.url, json!({"max-hops": 2}), "^fed$", "exchanges").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    link_running(&down, 1).await;
    publish_n(&ch_up, "fed", "a.2", "far", 1).await;
    until(10, "a's message at c", async || down.queue_len("qc") == 1).await;
    let at_c = drain(&ch_down, "qc").await;
    assert_eq!(body(&at_c[0]), "far-0");
    assert_eq!(hops(&at_c[0]), vec!["fed-b", "fed-a"]);

    // A cycle (a federates from c) never brings a message back to a.
    let (_c4, ch_up2) = up.channel().await;
    declare(&ch_up2, "qa").await;
    ch_up2.queue_bind("qa".into(), "fed".into(), "#".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    federate(&up, "c", &down.url, json!({"max-hops": 5}), "^fed$", "exchanges").await;
    federate(&mid, "a", &up.url, json!({"max-hops": 5}), "^fed$", "exchanges").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    link_running(&up, 1).await;
    link_running(&mid, 1).await;
    publish_n(&ch_up2, "fed", "a.3", "loop", 1).await;
    until(10, "a's message at c", async || down.queue_len("qc") == 1).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(up.queue_len("qa"), 1, "the message published at a is not federated back to a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_federation_pulls_only_for_consumers() {
    let up = Node::start("qf-a").await;
    let down = Node::start("qf-b").await;
    let (_c1, ch_up) = up.channel().await;
    declare(&ch_up, "work").await;
    publish_n(&ch_up, "", "work", "w", 40).await;
    let (_c2, ch_down) = down.channel().await;
    declare(&ch_down, "work").await;

    federate(&down, "a", &up.url, json!({"prefetch-count": 10}), "^work$", "queues").await;
    link_running(&down, 1).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(up.queue_len("work"), 40, "no local consumers: nothing is pulled");
    let link = &down.shovels.federation_status()[0];
    assert_eq!((link["type"].as_str(), link["queue"].as_str()), (Some("queue"), Some("work")));

    ch_down.basic_qos(5, BasicQosOptions::default()).await.unwrap();
    let mut consumer = ch_down.basic_consume("work".into(), "c".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    let mut got = Vec::new();
    while got.len() < 40 {
        use futures_util::StreamExt;
        let d = tokio::time::timeout(Duration::from_secs(10), consumer.next()).await.expect("federated messages").unwrap().unwrap();
        got.push(String::from_utf8_lossy(&d.data).into_owned());
        d.ack(BasicAckOptions::default()).await.unwrap();
    }
    assert_eq!(got, (0..40).map(|i| format!("w-{i}")).collect::<Vec<_>>());
    until(5, "upstream drained", async || up.queue_len("work") == 0).await;
}

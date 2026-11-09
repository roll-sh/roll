//! Partitioned queues (`x-partitions`) through an unmodified AMQP client.

use futures::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Connection, ConnectionProperties};
use mq_core::log::MemStore;
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use std::collections::HashMap;
use std::time::Duration;

async fn start() -> String {
    let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = mq_amqp::serve_listener(broker, listener, mq_amqp::ServerConfig { listen: addr, amqp10: None }).await;
    });
    format!("amqp://guest:guest@{addr}/%2f")
}

fn partitions(n: i32) -> FieldTable {
    let mut t = FieldTable::default();
    t.insert("x-partitions".into(), AMQPValue::LongInt(n));
    t
}

#[tokio::test]
async fn partitioned_queue_spreads_and_keeps_per_key_order() {
    let url = start().await;
    let conn = Connection::connect(&url, ConnectionProperties::default()).await.unwrap();
    let ch = conn.create_channel().await.unwrap();
    ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
    let q = ch.queue_declare("orders".into(), QueueDeclareOptions { durable: true, ..Default::default() }, partitions(4)).await.unwrap();
    assert_eq!(q.message_count(), 0);

    let bad = conn.create_channel().await.unwrap();
    let err = bad.queue_declare("p-auto".into(), QueueDeclareOptions { durable: true, auto_delete: true, ..Default::default() }, partitions(2)).await;
    assert!(err.is_err(), "partitioned auto-delete queues are rejected");

    for i in 0..200u32 {
        let key = format!("customer-{}", i % 7);
        let props = BasicProperties::default().with_message_id(key.into()).with_delivery_mode(2);
        ch.basic_publish("".into(), "orders".into(), BasicPublishOptions::default(), i.to_string().as_bytes(), props)
            .await
            .unwrap()
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let q = ch.queue_declare("orders".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    assert_eq!(q.message_count(), 200);

    // basic.get + ack works through the partition-tagged delivery tag.
    let got = ch.basic_get("orders".into(), BasicGetOptions::default()).await.unwrap().expect("a message");
    got.ack(BasicAckOptions::default()).await.unwrap();

    ch.basic_qos(50, BasicQosOptions::default()).await.unwrap();
    let mut consumer = ch.basic_consume("orders".into(), "c".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    let mut last: HashMap<String, u32> = HashMap::new();
    let mut n = 0;
    while n < 199 {
        let d = tokio::time::timeout(Duration::from_secs(5), consumer.next()).await.unwrap_or_else(|_| panic!("delivery {n}")).unwrap().unwrap();
        let key = d.properties.message_id().as_ref().unwrap().to_string();
        let v: u32 = std::str::from_utf8(&d.data).unwrap().parse().unwrap();
        if let Some(prev) = last.insert(key.clone(), v) {
            assert!(v > prev, "order kept per key {key}: {prev} then {v}");
        }
        d.ack(BasicAckOptions::default()).await.unwrap();
        n += 1;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let q = ch.queue_declare("orders".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    assert_eq!(q.message_count(), 0);
    let purged = ch.queue_delete("orders".into(), QueueDeleteOptions::default()).await.unwrap();
    assert_eq!(purged, 0);
}

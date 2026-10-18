//! End to end: an mq-core broker on a QuorumStore, driven over AMQP by an unmodified client
//! (lapin). 10k confirmed messages survive losing a keeper and a broker restart with a new
//! epoch.

mod common;

use common::Cluster;
use futures::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable, ShortString};
use lapin::{BasicProperties, Connection, ConnectionProperties};
use mq_core::meta::SqliteMeta;
use mq_core::{Broker, BrokerConfig};
use mq_storage::QuorumStore;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MESSAGES: usize = 10_000;

async fn start_broker(dir: &Path, store: Arc<QuorumStore>) -> (Arc<Broker>, String, tokio::task::JoinHandle<()>) {
    let meta = SqliteMeta::open(&dir.join("meta.db")).unwrap();
    let broker = Broker::start(BrokerConfig::default(), meta, store).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let b = broker.clone();
    let server = tokio::spawn(async move {
        let _ = mq_amqp::serve_listener(b, listener, mq_amqp::ServerConfig { listen: addr, amqp10: None }).await;
    });
    (broker, format!("amqp://guest:guest@{addr}/%2f"), server)
}

fn s(v: &str) -> ShortString {
    v.into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_messages_survive_keeper_loss_and_broker_restart() {
    let mut cluster = Cluster::start(3).await;
    let dir = tempfile::tempdir().unwrap();

    let store = QuorumStore::connect(cluster.config()).await.unwrap();
    let (broker, url, server) = start_broker(dir.path(), store.clone()).await;
    let conn = Connection::connect(&url, ConnectionProperties::default()).await.unwrap();
    let ch = conn.create_channel().await.unwrap();
    let mut args = FieldTable::default();
    args.insert("x-queue-type".into(), AMQPValue::LongString("quorum".into()));
    ch.queue_declare(s("orders"), QueueDeclareOptions { durable: true, ..Default::default() }, args).await.unwrap();
    ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
    let props = BasicProperties::default().with_delivery_mode(2);
    let mut confirms = Vec::with_capacity(MESSAGES);
    for i in 0..MESSAGES {
        let body = format!("order-{i}");
        let c = ch.basic_publish(s(""), s("orders"), BasicPublishOptions::default(), body.as_bytes(), props.clone()).await.unwrap();
        confirms.push(c);
        // Kill a keeper mid-stream: confirms keep coming from the remaining quorum.
        if i == MESSAGES / 2 {
            cluster.kill(1).await;
        }
    }
    for c in confirms {
        assert!(c.await.unwrap().is_ack(), "publish was nacked");
    }
    assert!(store.stats().appends > 0);
    conn.close(200, "bye".into()).await.ok();

    // The broker process dies; a new one starts from scratch (new store, new epoch) while the
    // keeper is still down.
    server.abort();
    broker.stop().await;
    drop(broker);
    drop(store);
    let store = QuorumStore::connect(cluster.config()).await.unwrap();
    let (broker, url, _server) = start_broker(dir.path(), store.clone()).await;
    let conn = Connection::connect(&url, ConnectionProperties::default()).await.unwrap();
    let ch = conn.create_channel().await.unwrap();
    ch.basic_qos(1000, BasicQosOptions::default()).await.unwrap();
    let mut consumer = ch.basic_consume(s("orders"), s("c"), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    let mut seen = HashSet::new();
    while seen.len() < MESSAGES {
        let d = tokio::time::timeout(Duration::from_secs(20), consumer.next())
            .await
            .unwrap_or_else(|_| panic!("only {} of {MESSAGES} messages recovered", seen.len()))
            .unwrap()
            .unwrap();
        seen.insert(String::from_utf8(d.data.clone()).unwrap());
        d.ack(BasicAckOptions::default()).await.unwrap();
    }
    assert!((0..MESSAGES).all(|i| seen.contains(&format!("order-{i}"))));
    // The returning keeper gets repaired.
    cluster.restart(1).await;
    let key = store.list_logs().await.unwrap().into_iter().find(|k| k.contains("orders")).expect("queue log");
    common::eventually(15, "keeper 1 repaired", async || {
        let (a, b) = (cluster.state(0, &key).await, cluster.state(1, &key).await);
        a.next == b.next && a.epoch == b.epoch
    })
    .await;
    assert_eq!(store.stats().logs, 1);
    conn.close(200, "bye".into()).await.ok();
    broker.stop().await;
}

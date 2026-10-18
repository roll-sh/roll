//! In-process brokers with AMQP listeners, driven by lapin.

#![allow(dead_code)]

use lapin::options::*;
use lapin::types::FieldTable;
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use mq_core::log::FileStore;
use mq_core::meta::SqliteMeta;
use mq_core::{Broker, BrokerConfig};
use mq_shovel::Shovels;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub struct Node {
    pub broker: Arc<Broker>,
    pub shovels: Arc<Shovels>,
    pub addr: SocketAddr,
    pub url: String,
    pub cluster: String,
    dir: Arc<tempfile::TempDir>,
    server: tokio::task::JoinHandle<()>,
}

impl Node {
    pub async fn start(cluster: &str) -> Node {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        Self::start_in(cluster, dir, "127.0.0.1:0".parse().unwrap()).await
    }

    async fn start_in(cluster: &str, dir: Arc<tempfile::TempDir>, listen: SocketAddr) -> Node {
        let _ = tracing_subscriber::fmt().with_env_filter("mq_shovel=info,mq_core=warn").with_test_writer().try_init();
        let meta = SqliteMeta::open(&dir.path().join("meta.db")).unwrap();
        let logs = FileStore::new(dir.path().join("logs"));
        let config = BrokerConfig { cluster_name: cluster.into(), node_name: format!("rabbit@{cluster}"), ..Default::default() };
        let broker = Broker::start(config, meta, logs).await.unwrap();
        let listener = bind(listen).await;
        let addr = listener.local_addr().unwrap();
        let cfg = mq_amqp::ServerConfig { listen: addr, amqp10: None };
        let b = broker.clone();
        let server = tokio::spawn(async move {
            let _ = mq_amqp::serve_listener(b, listener, cfg).await;
        });
        let shovels = Shovels::start(broker.clone());
        Node { broker, shovels, addr, url: format!("amqp://guest:guest@{addr}/%2f"), cluster: cluster.into(), dir, server }
    }

    /// Simulate a crash and restart on the same port and data directory: connections are
    /// dropped, then a fresh broker recovers from disk.
    pub async fn restart(self) -> Node {
        self.shovels.stop();
        self.server.abort();
        for c in self.broker.connections() {
            c.request_close("broker restarting");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.broker.stop().await;
        let (cluster, dir, addr) = (self.cluster.clone(), self.dir.clone(), self.addr);
        drop(self);
        Self::start_in(&cluster, dir, addr).await
    }

    pub async fn channel(&self) -> (Connection, Channel) {
        let c = Connection::connect(&self.url, ConnectionProperties::default()).await.unwrap();
        let ch = c.create_channel().await.unwrap();
        (c, ch)
    }

    pub fn queue_len(&self, q: &str) -> u64 {
        self.broker.vhost("/").and_then(|v| v.queue(q)).map(|q| q.shared.messages()).unwrap_or(0)
    }
}

async fn bind(addr: SocketAddr) -> tokio::net::TcpListener {
    for _ in 0..50 {
        if let Ok(l) = tokio::net::TcpListener::bind(addr).await {
            return l;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("could not bind {addr}");
}

pub async fn declare(ch: &Channel, q: &str) {
    ch.queue_declare(q.into(), QueueDeclareOptions { durable: true, ..Default::default() }, FieldTable::default()).await.unwrap();
}

/// Publish `n` persistent messages `prefix-i` with `message_id = i`, waiting for confirms.
pub async fn publish_n(ch: &Channel, exchange: &str, key: &str, prefix: &str, n: usize) {
    ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
    let mut confirms = Vec::with_capacity(n);
    for i in 0..n {
        let props = BasicProperties::default().with_delivery_mode(2).with_message_id(i.to_string().into()).with_content_type("text/plain".into());
        let pc = ch.basic_publish(exchange.into(), key.into(), BasicPublishOptions::default(), format!("{prefix}-{i}").as_bytes(), props).await.unwrap();
        confirms.push(pc);
    }
    for c in confirms {
        assert!(c.await.unwrap().is_ack());
    }
}

/// Drain a queue with basic.get, returning the bodies.
pub async fn drain(ch: &Channel, q: &str) -> Vec<lapin::message::BasicGetMessage> {
    let mut out = Vec::new();
    while let Some(m) = ch.basic_get(q.into(), BasicGetOptions { no_ack: true }).await.unwrap() {
        out.push(m);
    }
    out
}

pub fn body(m: &lapin::message::BasicGetMessage) -> String {
    String::from_utf8_lossy(&m.delivery.data).into_owned()
}

/// Poll `f` until it returns true, or panic with `what` after `secs`.
pub async fn until(secs: u64, what: &str, mut f: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

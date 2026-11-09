use lapin::options::{BasicGetOptions, BasicPublishOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use lapin::{BasicProperties, Connection, ConnectionProperties};
use mq_core::log::MemStore;
use mq_core::meta::{MemMeta, MetaStore};
use mq_core::{Broker, BrokerConfig};
use mq_proxy::{BoxFuture, Directory, NODE, Proxy, ProxyConfig, Waker};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A fake broker that writes its name to every connection.
async fn named_server(name: &'static str) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = s.write_all(name.as_bytes()).await;
                let mut buf = [0u8; 16];
                let _ = s.read(&mut buf).await;
            });
        }
    });
    addr
}

async fn register(meta: &Arc<MemMeta>, node: &str, az: &str, addr: SocketAddr, draining: bool) {
    let v = json!({ "addr": "127.0.0.1:1", "az": az, "draining": draining, "hb": 1, "listeners": { "amqp": addr.to_string() } });
    meta.put(NODE, node, &v).await.unwrap();
}

async fn start_proxy(meta: Arc<MemMeta>, az: &str, waker: Option<Arc<dyn Waker>>) -> (Arc<Proxy>, SocketAddr) {
    let config = ProxyConfig { az: Some(az.into()), refresh: Duration::from_millis(50), wake_timeout: Duration::from_secs(5), ..ProxyConfig::default() };
    let d = Directory::new(meta, config);
    d.refresh().await.unwrap();
    d.spawn_refresh();
    let proxy = Proxy::new(d, waker);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(proxy.clone().serve_listener("amqp".into(), l));
    (proxy, addr)
}

async fn who(proxy: SocketAddr) -> (String, TcpStream) {
    let mut s = TcpStream::connect(proxy).await.unwrap();
    let mut buf = vec![0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(8), s.read(&mut buf)).await.expect("forwarded in time").unwrap();
    (String::from_utf8_lossy(&buf[..n]).into_owned(), s)
}

#[tokio::test]
async fn prefers_own_az_then_least_connections_and_skips_draining() {
    let meta = MemMeta::new();
    register(&meta, "a1", "eu-west-1a", named_server("a1").await, false).await;
    register(&meta, "a2", "eu-west-1a", named_server("a2").await, false).await;
    register(&meta, "b1", "eu-west-1b", named_server("b1").await, false).await;
    register(&meta, "a3", "eu-west-1a", named_server("a3").await, true).await;
    let (_p, addr) = start_proxy(meta.clone(), "eu-west-1a", None).await;

    let mut held = Vec::new();
    let mut names = Vec::new();
    for _ in 0..4 {
        let (n, s) = who(addr).await;
        names.push(n);
        held.push(s);
    }
    names.sort();
    assert_eq!(names, ["a1", "a1", "a2", "a2"], "same-AZ brokers only, spread evenly, draining a3 skipped");

    // The AZ's brokers go away: fall back to the other AZ.
    meta.delete(NODE, "a1").await.unwrap();
    meta.delete(NODE, "a2").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(who(addr).await.0, "b1");
}

#[tokio::test]
async fn dead_broker_is_skipped() {
    let meta = MemMeta::new();
    // Nothing listens on the first one's port.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    register(&meta, "a0", "z", dead, false).await;
    register(&meta, "a9", "z", named_server("a9").await, false).await;
    let (_p, addr) = start_proxy(meta, "z", None).await;
    for _ in 0..3 {
        assert_eq!(who(addr).await.0, "a9");
    }
}

struct StartBroker {
    meta: Arc<MemMeta>,
    calls: Arc<AtomicUsize>,
}

impl Waker for StartBroker {
    fn wake(&self) -> BoxFuture<anyhow::Result<()>> {
        let meta = self.meta.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(300)).await;
            register(&meta, "woken", "z", named_server("woken").await, false).await;
            Ok(())
        })
    }
}

#[tokio::test]
async fn wakes_the_cluster_and_holds_the_connection() {
    let meta = MemMeta::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(StartBroker { meta: meta.clone(), calls: calls.clone() });
    let (_p, addr) = start_proxy(meta, "z", Some(waker)).await;
    // Several clients connect while nothing runs: one wake, everyone forwarded.
    let a = tokio::spawn(who(addr));
    let b = tokio::spawn(who(addr));
    assert_eq!(a.await.unwrap().0, "woken");
    assert_eq!(b.await.unwrap().0, "woken");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn amqp_clients_work_through_the_proxy() {
    let meta = MemMeta::new();
    let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let baddr = l.local_addr().unwrap();
    tokio::spawn(mq_amqp::serve_listener(broker, l, mq_amqp::ServerConfig { listen: baddr, amqp10: None }));
    register(&meta, "n1", "z", baddr, false).await;
    let (_p, addr) = start_proxy(meta, "z", None).await;

    let conn = Connection::connect(&format!("amqp://guest:guest@{addr}/%2f"), ConnectionProperties::default()).await.unwrap();
    let ch = conn.create_channel().await.unwrap();
    ch.queue_declare("via-proxy".into(), QueueDeclareOptions::default(), FieldTable::default()).await.unwrap();
    ch.basic_publish("".into(), "via-proxy".into(), BasicPublishOptions::default(), b"hello", BasicProperties::default()).await.unwrap().await.unwrap();
    let mut got = None;
    for _ in 0..50 {
        got = ch.basic_get("via-proxy".into(), BasicGetOptions { no_ack: true }).await.unwrap();
        if got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got.expect("message").delivery.data, b"hello");
}

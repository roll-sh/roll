//! A three-node cluster in one process. Each node runs on its own Tokio runtime, so killing a
//! node (shutting its runtime down) drops every task and socket it had, like a crashed machine.
//! Nodes share one metadata store and one log store (standing in for roll postgres and the
//! replicated keepers). Clients are unmodified AMQP clients (lapin).

use futures::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use mq_cluster::{Cluster, ClusterConfig};
use mq_core::log::{LogStore, MemStore};
use mq_core::meta::{MemMeta, MetaStore};
use mq_core::transport::{Transport, TransportConfig};
use mq_core::{Broker, BrokerConfig};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

struct Node {
    rt: Option<Runtime>,
    url: String,
    /// RabbitMQ Stream protocol listener.
    stream_port: u16,
    cluster: Arc<Cluster>,
    #[allow(dead_code)]
    broker: Arc<Broker>,
}

impl Node {
    fn start(id: &str, meta: Arc<dyn MetaStore>, logs: Arc<dyn LogStore>, rebalance: Option<Duration>) -> Node {
        Self::start_in(id, None, meta, logs, rebalance)
    }

    /// A node in availability zone `az`.
    fn start_in(id: &str, az: Option<&str>, meta: Arc<dyn MetaStore>, logs: Arc<dyn LogStore>, rebalance: Option<Duration>) -> Node {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let stream_listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let stream_addr = stream_listener.local_addr().unwrap();
        let config = ClusterConfig {
            node_id: id.into(),
            listeners: [("stream".to_string(), stream_addr)].into(),
            heartbeat: Duration::from_millis(100),
            node_ttl: Duration::from_millis(800),
            follow: Duration::from_millis(30),
            rebalance,
            az: az.map(String::from),
            place_min_ops: 50,
            // Every node-to-node connection goes through the secret handshake.
            transport: Transport::new("cluster", &TransportConfig { secret: Some("cluster-test".into()), ..Default::default() }).unwrap(),
            ..Default::default()
        };
        let (url, cluster, broker) = rt.block_on(async move {
            let cluster = Cluster::join(config, meta.clone(), logs.clone()).await.unwrap();
            let bc = BrokerConfig { node_name: format!("rabbit@{id}"), ..Default::default() };
            let broker = Broker::start_with_placement(bc, meta, logs, Some(cluster.placement())).await.unwrap();
            cluster.attach(&broker);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let b = broker.clone();
            tokio::spawn(async move {
                let _ = mq_amqp::serve_listener(b, listener, mq_amqp::ServerConfig { listen: addr, amqp10: None }).await;
            });
            let sc = mq_stream::Config { advertised_host: "127.0.0.1".into(), ..mq_stream::Config::new(stream_addr) };
            tokio::spawn(mq_stream::serve_listener(broker.clone(), stream_listener, sc));
            (format!("amqp://guest:guest@{addr}/%2f"), cluster, broker)
        });
        Node { rt: Some(rt), url, stream_port: stream_addr.port(), cluster, broker }
    }

    /// Crash: every task and socket of this node goes away at once.
    fn kill(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }

    /// Run `f` on this node's runtime (from any other runtime).
    fn spawn<T: Send + 'static>(&self, f: impl std::future::Future<Output = T> + Send + 'static) -> tokio::task::JoinHandle<T> {
        self.rt.as_ref().unwrap().spawn(f)
    }
}

fn shared() -> (Arc<dyn MetaStore>, Arc<dyn LogStore>) {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "mq_cluster=info,mq_core=warn".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_test_writer().try_init();
    (MemMeta::new(), MemStore::new())
}

async fn channel(url: &str) -> (Connection, Channel) {
    let c = Connection::connect(url, ConnectionProperties::default()).await.unwrap();
    let ch = c.create_channel().await.unwrap();
    (c, ch)
}

fn quorum() -> FieldTable {
    let mut t = FieldTable::default();
    t.insert("x-queue-type".into(), AMQPValue::LongString("quorum".into()));
    t
}

async fn declare(ch: &Channel, name: &str) {
    ch.queue_declare(name.into(), QueueDeclareOptions { durable: true, ..Default::default() }, quorum()).await.unwrap();
}

async fn wait_until(what: &str, timeout: Duration, mut f: impl AsyncFnMut() -> bool) {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Publish `ids` with confirms through `ch`, returning the ids the broker confirmed.
async fn publish_confirmed(ch: &Channel, queue: &str, ids: impl Iterator<Item = u32>) -> Vec<u32> {
    let mut ok = Vec::new();
    for i in ids {
        let props = BasicProperties::default().with_delivery_mode(2).with_message_id(i.to_string().into());
        let Ok(confirm) = ch.basic_publish("".into(), queue.into(), BasicPublishOptions::default(), i.to_string().as_bytes(), props).await else { break };
        match tokio::time::timeout(Duration::from_secs(10), confirm).await {
            Ok(Ok(c)) if c.is_ack() => ok.push(i),
            _ => {}
        }
    }
    ok
}

/// Consume until `want` distinct ids have arrived (or time out); returns the ids seen.
async fn consume_ids(url: &str, queue: &str, want: usize, timeout: Duration) -> HashSet<u32> {
    let (_c, ch) = channel(url).await;
    ch.basic_qos(200, BasicQosOptions::default()).await.unwrap();
    let mut consumer = ch.basic_consume(queue.into(), "".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    let mut seen = HashSet::new();
    let t = Instant::now();
    while seen.len() < want && t.elapsed() < timeout {
        let Ok(Some(Ok(d))) = tokio::time::timeout(Duration::from_millis(500), consumer.next()).await else { continue };
        seen.insert(std::str::from_utf8(&d.data).unwrap().parse().unwrap());
        d.ack(BasicAckOptions::default()).await.unwrap();
    }
    seen
}

#[test]
fn queues_are_reachable_from_every_node() {
    let (meta, logs) = shared();
    let a = Node::start("a", meta.clone(), logs.clone(), None);
    let b = Node::start("b", meta.clone(), logs.clone(), None);
    let c = Node::start("c", meta.clone(), logs.clone(), None);
    let client = Runtime::new().unwrap();
    client.block_on(async {
        let (_ca, cha) = channel(&a.url).await;
        declare(&cha, "orders").await;
        cha.exchange_declare("events".into(), lapin::ExchangeKind::Topic, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default())
            .await
            .unwrap();
        cha.queue_bind("orders".into(), "events".into(), "order.*".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
        assert_eq!(a.cluster.owned_count(), 1);

        // Topology reaches the other nodes through the change feed.
        let (_cc, chc) = channel(&c.url).await;
        wait_until("queue on c", Duration::from_secs(3), async || {
            let (_x, ch) = channel(&c.url).await;
            ch.queue_declare("orders".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_ok()
        })
        .await;

        // Publish on b through the topic exchange, consume on c; the queue runs on a.
        let (_cb, chb) = channel(&b.url).await;
        wait_until("binding on b", Duration::from_secs(3), async || {
            chb.confirm_select(ConfirmSelectOptions::default()).await.ok();
            let c = chb
                .basic_publish("events".into(), "order.created".into(), BasicPublishOptions { mandatory: true, ..Default::default() }, b"probe", BasicProperties::default())
                .await
                .unwrap()
                .await
                .unwrap();
            c.is_ack() && c.take_message().is_none()
        })
        .await;
        let got = chc.basic_get("orders".into(), BasicGetOptions { no_ack: true }).await.unwrap().expect("probe");
        assert_eq!(got.data, b"probe");

        let (_cb2, pub_ch) = channel(&b.url).await;
        pub_ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        let sent = publish_confirmed(&pub_ch, "orders", 0..2000).await;
        assert_eq!(sent.len(), 2000);
        let seen = consume_ids(&c.url, "orders", 2000, Duration::from_secs(20)).await;
        assert_eq!(seen.len(), 2000, "every message delivered across nodes");

        // Counters reach non-owners; delete from a non-owner removes it everywhere.
        // Counters on non-owners follow the owner within a moment.
        wait_until("counters on c", Duration::from_secs(3), async || {
            let q = chc.queue_declare("orders".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.unwrap();
            q.message_count() == 0
        })
        .await;
        chb.queue_delete("orders".into(), QueueDeleteOptions::default()).await.unwrap();
        wait_until("delete everywhere", Duration::from_secs(3), async || {
            let (_x, ch) = channel(&a.url).await;
            ch.queue_declare("orders".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_err()
        })
        .await;
    });
    drop((a, b, c));
}

#[test]
fn failover_loses_no_confirmed_message() {
    let (meta, logs) = shared();
    let mut a = Node::start("a", meta.clone(), logs.clone(), None);
    let b = Node::start("b", meta.clone(), logs.clone(), None);
    let c = Node::start("c", meta.clone(), logs.clone(), None);
    let client = Runtime::new().unwrap();
    client.block_on(async {
        let (_ca, cha) = channel(&a.url).await;
        declare(&cha, "jobs").await;
        assert_eq!(a.cluster.owned_count(), 1, "the declaring node owns the queue");
        wait_until("queue on b", Duration::from_secs(3), async || {
            let (_x, ch) = channel(&b.url).await;
            ch.queue_declare("jobs".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_ok()
        })
        .await;
        let (_cb, chb) = channel(&b.url).await;
        chb.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        let mut confirmed: Vec<u32> = publish_confirmed(&chb, "jobs", 0..1000).await;
        assert_eq!(confirmed.len(), 1000);

        // Crash the owner.
        a.kill();
        let t = Instant::now();

        // Keep publishing through b until the queue is back (b or c takes over).
        let mut next = 1000;
        while confirmed.len() < 2000 && t.elapsed() < Duration::from_secs(20) {
            let (_cb, ch) = channel(&b.url).await;
            ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
            let ok = publish_confirmed(&ch, "jobs", next..(next + 100).min(2000)).await;
            next += 100;
            confirmed.extend(ok);
            if next >= 2000 && confirmed.len() < 2000 {
                // Retry what wasn't confirmed, like a real publisher would.
                let have: HashSet<u32> = confirmed.iter().copied().collect();
                let missing: Vec<u32> = (0..2000).filter(|i| !have.contains(i)).collect();
                let ok = publish_confirmed(&ch, "jobs", missing.into_iter()).await;
                confirmed.extend(ok);
            }
        }
        let failover = t.elapsed();
        assert!(confirmed.len() >= 2000, "publishing recovered after failover (took {failover:?})");
        assert!(b.cluster.owned_count() + c.cluster.owned_count() == 1, "a survivor owns the queue");

        let want: HashSet<u32> = confirmed.into_iter().collect();
        let seen = consume_ids(&c.url, "jobs", want.len(), Duration::from_secs(20)).await;
        let lost: Vec<_> = want.difference(&seen).collect();
        assert!(lost.is_empty(), "lost {} confirmed messages: {:?}", lost.len(), &lost[..lost.len().min(10)]);
        eprintln!("failover: publishing resumed within {failover:?}");
    });
    drop((b, c));
}

#[test]
fn live_rebalance_and_drain_keep_every_message() {
    let (meta, logs) = shared();
    let a = Node::start("a", meta.clone(), logs.clone(), None);
    let client = Runtime::new().unwrap();
    let queues: Vec<String> = (0..12).map(|i| format!("q{i}")).collect();
    client.block_on(async {
        let (_ca, cha) = channel(&a.url).await;
        for q in &queues {
            declare(&cha, q).await;
        }
    });
    assert_eq!(a.cluster.owned_count(), 12);
    let b = Node::start("b", meta.clone(), logs.clone(), None);
    let c = Node::start("c", meta.clone(), logs.clone(), None);
    client.block_on(async {
        // A consumer on b stays connected the whole time; a publisher on c keeps going.
        let (_cb, chb) = channel(&b.url).await;
        wait_until("queues on b", Duration::from_secs(3), async || {
            let (_x, ch) = channel(&b.url).await;
            ch.queue_declare("q11".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_ok()
        })
        .await;
        wait_until("queues on c", Duration::from_secs(3), async || {
            let (_x, ch) = channel(&c.url).await;
            ch.queue_declare("q11".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_ok()
        })
        .await;
        chb.basic_qos(100, BasicQosOptions::default()).await.unwrap();
        let mut consumers = Vec::new();
        for q in &queues {
            consumers.push(chb.basic_consume(q.as_str().into(), "".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap());
        }
        let seen = Arc::new(parking_lot::Mutex::new(HashSet::<String>::new()));
        let mut tasks = Vec::new();
        for mut cons in consumers {
            let seen = seen.clone();
            tasks.push(tokio::spawn(async move {
                while let Some(Ok(d)) = cons.next().await {
                    seen.lock().insert(String::from_utf8(d.data.clone()).unwrap());
                    let _ = d.ack(BasicAckOptions::default()).await;
                }
            }));
        }

        let (_cc, chc) = channel(&c.url).await;
        chc.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        let publisher = {
            let chc = chc.clone();
            let queues = queues.clone();
            tokio::spawn(async move {
                let mut confirmed = HashSet::new();
                for round in 0..300u32 {
                    for q in &queues {
                        let body = format!("{q}:{round}");
                        let props = BasicProperties::default().with_delivery_mode(2);
                        if let Ok(p) = chc.basic_publish("".into(), q.as_str().into(), BasicPublishOptions::default(), body.as_bytes(), props).await
                            && let Ok(Ok(conf)) = tokio::time::timeout(Duration::from_secs(10), p).await
                            && conf.is_ack()
                        {
                            confirmed.insert(body);
                        }
                    }
                }
                confirmed
            })
        };

        // Rebalance while traffic flows, then drain a completely.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ac = a.cluster.clone();
        let moved = a.spawn(async move { ac.rebalance(64).await }).await.unwrap();
        assert!(moved > 0, "rebalance moved queues");
        let spread = [a.cluster.owned_count(), b.cluster.owned_count(), c.cluster.owned_count()];
        assert!(spread[1] > 0 && spread[2] > 0, "queues spread over b and c: {spread:?}");
        assert_eq!(spread.iter().sum::<usize>(), 12);

        // Rolling upgrade: drain a completely while traffic keeps flowing.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ac = a.cluster.clone();
        let left = a.spawn(async move { ac.drain(Duration::from_secs(15)).await }).await.unwrap();
        assert_eq!(left, 0, "drain handed off every queue");
        assert_eq!(b.cluster.owned_count() + c.cluster.owned_count(), 12);

        let confirmed = publisher.await.unwrap();
        assert_eq!(confirmed.len(), 300 * 12, "every publish confirmed through rebalance");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(20) && !confirmed.iter().all(|m| seen.lock().contains(m)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let missing = confirmed.iter().filter(|m| !seen.lock().contains(*m)).count();
        if missing > 0 {
            let (_x, ch) = channel(&c.url).await;
            for q in &queues {
                let d = ch.queue_declare(q.as_str().into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.unwrap();
                let owner = [("a", &a), ("b", &b), ("c", &c)].iter().find(|(_, n)| n.cluster.owned_keys().iter().any(|k| k.contains(q.as_str()))).map(|x| x.0);
                let unacked = [&a, &b, &c].iter().filter_map(|n| n.broker.vhost("/")?.queue(q)).map(|r| r.shared.unacked.load(std::sync::atomic::Ordering::Relaxed)).collect::<Vec<_>>();
                eprintln!("{q}: owner {owner:?} messages {} consumers {} unacked per node {unacked:?}", d.message_count(), d.consumer_count());
            }
        }
        assert_eq!(missing, 0, "consumer on b received every confirmed message without reconnecting");
        assert!(_cb.status().connected(), "client connections never moved");
        for t in tasks {
            t.abort();
        }
    });
}

/// Stream protocol clients connected to any node publish to and read from the stream's owner:
/// `Metadata` names the owner as leader, as RabbitMQ does for its stream members.
#[test]
fn stream_clients_are_sent_to_the_owner() {
    use rabbitmq_stream_client::Environment;
    use rabbitmq_stream_client::types::{Message, OffsetSpecification};
    let (meta, logs) = shared();
    let a = Node::start("a", meta.clone(), logs.clone(), None);
    let b = Node::start("b", meta, logs, None);
    let client = Runtime::new().unwrap();
    client.block_on(async {
        let (_c, ch) = channel(&a.url).await;
        let mut args = FieldTable::default();
        args.insert("x-queue-type".into(), AMQPValue::LongString("stream".into()));
        ch.queue_declare("events".into(), QueueDeclareOptions { durable: true, ..Default::default() }, args).await.unwrap();
        wait_until("stream on b", Duration::from_secs(3), async || {
            let (_x, ch) = channel(&b.url).await;
            ch.queue_declare("events".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_ok()
        })
        .await;
        assert!(a.cluster.owned_keys().iter().any(|k| k.contains("events")), "a owns the stream");

        let env = Environment::builder().host("127.0.0.1").port(b.stream_port).build().await.unwrap();
        let producer = env.producer().build("events").await.unwrap();
        for i in 0..100 {
            producer.send_with_confirm(Message::builder().body(format!("e{i}")).build()).await.unwrap();
        }
        let mut consumer = env.consumer().offset(OffsetSpecification::First).build("events").await.unwrap();
        let mut got = Vec::new();
        while got.len() < 100 {
            let d = tokio::time::timeout(Duration::from_secs(10), consumer.next()).await.expect("deliveries").unwrap().unwrap();
            got.push(String::from_utf8(d.message().data().unwrap().to_vec()).unwrap());
        }
        assert_eq!(got, (0..100).map(|i| format!("e{i}")).collect::<Vec<_>>());
        let _ = producer.close().await;
    });
}

/// Clients in one AZ using a queue that lives in another pull it over: its home AZ follows the
/// traffic and the queue moves to a node there, live, without losing a message.
#[test]
fn queues_follow_their_clients_to_their_az() {
    let (meta, logs) = shared();
    let every = Some(Duration::from_millis(300));
    let a = Node::start_in("a1", Some("az-a"), meta.clone(), logs.clone(), every);
    let b = Node::start_in("b1", Some("az-b"), meta.clone(), logs.clone(), every);
    let key = mq_cluster::queue_key("/", "follow", 0);
    let main = tokio::runtime::Runtime::new().unwrap();
    main.block_on(async {
        // Declared from AZ a: it starts there.
        let (_ca, cha) = channel(&a.url).await;
        declare(&cha, "follow").await;
        wait_until("a owns the queue", Duration::from_secs(5), async || a.cluster.owned_keys().contains(&key)).await;
        assert_eq!(a.cluster.home_az(&key).as_deref(), Some("az-a"));

        // Every client is in AZ b from now on. Until b has the queue from the change feed, a
        // publish there is unroutable, and confirmed as such: wait for it.
        let (_cb, chb) = channel(&b.url).await;
        wait_until("queue on b", Duration::from_secs(5), async || {
            let (_x, ch) = channel(&b.url).await;
            ch.queue_declare("follow".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_ok()
        })
        .await;
        chb.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        let mut sent = Vec::new();
        let mut next = 0u32;
        let t = Instant::now();
        while !b.cluster.owned_keys().contains(&key) {
            assert!(t.elapsed() < Duration::from_secs(20), "the queue never moved to AZ b");
            sent.extend(publish_confirmed(&chb, "follow", next..next + 50).await);
            next += 50;
        }
        assert_eq!(b.cluster.home_az(&key).as_deref(), Some("az-b"));
        assert!(!a.cluster.owned_keys().contains(&key));
        // Messages from before and after the move are all there.
        sent.extend(publish_confirmed(&chb, "follow", next..next + 50).await);
        let seen = consume_ids(&b.url, "follow", sent.len(), Duration::from_secs(20)).await;
        assert_eq!(seen.len(), sent.len(), "lost {} of {}", sent.len() - seen.len(), sent.len());

        // Quiet queues stay where they are: the next passes see no traffic.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(b.cluster.owned_keys().contains(&key));
    });
    drop(main);
    let (mut a, mut b) = (a, b);
    a.kill();
    b.kill();
}

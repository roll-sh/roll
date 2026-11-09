//! Three nodes in one process, on roll-cluster. Each node runs on its own Tokio runtime with its
//! own internal API (the RPC route), roll-cluster membership and log store, so killing a node
//! (its membership dies without cleanup, its runtime shuts down) drops every task and socket it
//! had, like a crashed machine. Nodes share one metadata store (standing in for the system
//! database) and one object store (standing in for S3), which each node's object log
//! (`S3ExpressStore`, group commit on, as on the fleet) writes with put-if-absent. Leases are on
//! roll-cluster's memory backend, and the failover test also runs on DynamoDB Local. Clients
//! are unmodified AMQP clients (lapin).

use futures::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use mq_cluster::{Cluster, ClusterConfig};
use mq_core::log::LogStore;
use mq_core::meta::{MemMeta, MetaStore};
use mq_core::{Broker, BrokerConfig};
use roll_cluster::{Backend, DynamoDb, DynamoDbConfig, Memory, MemoryStore};
use std::collections::HashSet;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

const SECRET: &[u8] = b"queues cluster test secret, 32 b";

enum Leases {
    Memory(MemoryStore),
    Dynamo { endpoint: String, table: String },
}

/// What the nodes of one test share.
struct Env {
    leases: Leases,
    meta: Arc<dyn MetaStore>,
    objects: Arc<dyn object_store::ObjectStore>,
}

impl Env {
    fn memory() -> Env {
        logs_on();
        Env { leases: Leases::Memory(MemoryStore::new()), meta: MemMeta::new(), objects: Arc::new(object_store::memory::InMemory::new()) }
    }

    /// On DynamoDB Local, or `None` (with a message) when it isn't available.
    fn dynamodb(name: &str) -> Option<Env> {
        logs_on();
        let endpoint = dynamodb_endpoint()?;
        let table = format!("q-{name}-{:x}", mq_core::rand_u64());
        Some(Env { leases: Leases::Dynamo { endpoint, table }, meta: MemMeta::new(), objects: Arc::new(object_store::memory::InMemory::new()) })
    }

    fn backend(&self) -> Box<dyn Backend> {
        match &self.leases {
            Leases::Memory(m) => Box::new(Memory::new(m.clone())),
            Leases::Dynamo { endpoint, table } => {
                let mut c = DynamoDbConfig::new(table.clone());
                c.endpoint = Some(endpoint.clone());
                c.create_table = true;
                Box::new(DynamoDb::connect(c).unwrap())
            }
        }
    }

    /// roll-cluster's test timings (leases of 1.5 s), twice as long on DynamoDB Local.
    fn timings(&self, mut c: roll_cluster::ClusterConfig) -> roll_cluster::ClusterConfig {
        if matches!(self.leases, Leases::Dynamo { .. }) {
            for d in [&mut c.heartbeat, &mut c.node_ttl, &mut c.lease_ttl, &mut c.renew_every, &mut c.lease_margin, &mut c.sweep_every, &mut c.rebalance_every, &mut c.handoff_timeout] {
                *d *= 2;
            }
        }
        c
    }

    /// A node's own object log on the shared bucket, as a separate process would have.
    fn logs(&self) -> Arc<dyn LogStore> {
        let cfg = mq_storage::ExpressConfig { group: Some(mq_storage::GroupConfig::default()), ..mq_storage::ExpressConfig::new(self.objects.clone(), "logs") };
        mq_storage::S3ExpressStore::new(cfg)
    }
}

fn logs_on() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "mq_cluster=info,roll_cluster=warn,mq_core=warn".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_test_writer().try_init();
}

struct Node {
    rt: Option<Runtime>,
    url: String,
    /// RabbitMQ Stream protocol listener.
    stream_port: u16,
    /// The stream protocol's server (the edge hands it connections).
    stream: mq_stream::Handle,
    nodes: Arc<roll_cluster::Cluster>,
    cluster: Arc<Cluster>,
    broker: Arc<Broker>,
}

impl Node {
    fn start(env: &Env, id: &str) -> Node {
        Node::start_with(env, id, None)
    }

    /// With `front`, other nodes reach this one's internal API through a proxy on that runtime
    /// that goes silent when the flag is set ([`cuttable`]).
    fn start_with(env: &Env, id: &str, front: Option<(&Runtime, Arc<AtomicBool>)>) -> Node {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let (api, stream_listener) = rt.block_on(async { (tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(), tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()) });
        let stream_addr = stream_listener.local_addr().unwrap();
        let api_addr = api.local_addr().unwrap();
        let advertise = match front {
            Some((on, cut)) => format!("http://{}", on.block_on(cuttable(api_addr, cut))),
            None => format!("http://{api_addr}"),
        };
        let nodes = roll_cluster::Cluster::join(env.backend(), env.timings(roll_cluster::ClusterConfig::for_tests(id, advertise, SECRET))).unwrap();
        let (meta, logs) = (env.meta.clone(), env.logs());
        let config = ClusterConfig { listeners: [("stream".to_string(), stream_addr)].into(), follow: Duration::from_millis(30) };
        let n = nodes.clone();
        let (url, cluster, broker, stream) = rt.block_on(async move {
            let cluster = Cluster::join(n, config, meta.clone(), logs.clone()).await.unwrap();
            let bc = BrokerConfig { node_name: format!("rabbit@{id}"), ..Default::default() };
            let broker = Broker::start_with_placement(bc, meta, logs, Some(cluster.placement())).await.unwrap();
            cluster.attach(&broker);
            tokio::spawn(axum::serve(api, cluster.routes()).into_future());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let b = broker.clone();
            tokio::spawn(async move {
                let _ = mq_amqp::serve_listener(b, listener, mq_amqp::ServerConfig { listen: addr, amqp10: None }).await;
            });
            let sc = mq_stream::Config { advertised_host: "127.0.0.1".into(), advertised_port: stream_addr.port(), ..mq_stream::Config::new(stream_addr) };
            let stream = mq_stream::start(broker.clone(), sc).await;
            let s = stream.clone();
            tokio::spawn(async move { s.serve_listener(stream_listener).await });
            (format!("amqp://guest:guest@{addr}/%2f"), cluster, broker, stream)
        });
        Node { rt: Some(rt), url, stream_port: stream_addr.port(), stream, nodes, cluster, broker }
    }

    /// Crash: membership stops without a word (its leases lapse), and every task and socket of
    /// this node goes away at once.
    fn kill(&mut self) {
        self.nodes.fault_kill();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }

    /// Run `f` on this node's runtime (from any other runtime).
    fn spawn<T: Send + 'static>(&self, f: impl std::future::Future<Output = T> + Send + 'static) -> tokio::task::JoinHandle<T> {
        self.rt.as_ref().unwrap().spawn(f)
    }

    fn owns(&self, queue: &str) -> bool {
        self.cluster.owned_keys().contains(&mq_cluster::queue_key("/", queue, 0))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A TCP proxy to `to` that, once `cut` is set, passes nothing on in either direction and closes
/// nothing: a machine that died without a word (no FIN, no RST), as the other nodes see it.
async fn cuttable(to: std::net::SocketAddr, cut: Arc<AtomicBool>) -> std::net::SocketAddr {
    async fn pump(mut r: tokio::net::tcp::OwnedReadHalf, mut w: tokio::net::tcp::OwnedWriteHalf, cut: Arc<AtomicBool>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 64 << 10];
        while let Ok(n) = r.read(&mut buf).await {
            while cut.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if n == 0 || w.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = l.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect(to).await else { continue };
            let ((cr, cw), (sr, sw)) = (client.into_split(), server.into_split());
            tokio::spawn(pump(cr, sw, cut.clone()));
            tokio::spawn(pump(sr, cw, cut.clone()));
        }
    });
    at
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

/// Wait until `node` knows queue `name` (through the change feed).
async fn known_on(node: &Node, name: &str) {
    wait_until(&format!("{name} on {}", node.nodes.node_id()), Duration::from_secs(5), async || {
        let (_x, ch) = channel(&node.url).await;
        ch.queue_declare(name.into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.is_ok()
    })
    .await;
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
    let env = Env::memory();
    let (a, b, c) = (Node::start(&env, "a"), Node::start(&env, "b"), Node::start(&env, "c"));
    let client = Runtime::new().unwrap();
    client.block_on(async {
        let (_ca, cha) = channel(&a.url).await;
        declare(&cha, "orders").await;
        cha.exchange_declare("events".into(), lapin::ExchangeKind::Topic, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default())
            .await
            .unwrap();
        cha.queue_bind("orders".into(), "events".into(), "order.*".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
        wait_until("one owner", Duration::from_secs(5), async || [&a, &b, &c].iter().filter(|n| n.owns("orders")).count() == 1).await;

        // Topology reaches the other nodes through the change feed.
        known_on(&c, "orders").await;
        let (_cc, chc) = channel(&c.url).await;

        // Publish on b through the topic exchange, consume on c, wherever the queue runs.
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

        // Counters on non-owners follow the owner within a moment; a delete from any node removes
        // the queue everywhere, and its lease goes with it.
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
        let key = mq_cluster::queue_key("/", "orders", 0);
        wait_until("the lease given up", Duration::from_secs(5), async || a.nodes.lease_record(&key).unwrap().is_none()).await;
    });
}

/// The owner dies while a publisher on another node waits for confirms: another node takes the
/// lease over once it lapses, replays the log, and every confirmed message is there.
fn owner_killed_under_load(env: Env) {
    let mut nodes: Vec<Node> = ["a", "b", "c"].iter().map(|id| Node::start(&env, id)).collect();
    let client = Runtime::new().unwrap();
    client.block_on(async {
        let (_ca, cha) = channel(&nodes[0].url).await;
        declare(&cha, "jobs").await;
        wait_until("an owner", Duration::from_secs(10), async || nodes.iter().any(|n| n.owns("jobs"))).await;
        let owner = nodes.iter().position(|n| n.owns("jobs")).unwrap();
        let (p, q) = ((owner + 1) % 3, (owner + 2) % 3);
        known_on(&nodes[p], "jobs").await;
        let (_cp, chp) = channel(&nodes[p].url).await;
        chp.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        let publisher = {
            let chp = chp.clone();
            tokio::spawn(async move {
                let mut confirmed = Vec::new();
                let t = Instant::now();
                let mut next = 0;
                // Keep publishing (and retrying what wasn't confirmed, as a real publisher would)
                // until 3000 are confirmed.
                while confirmed.len() < 3000 && t.elapsed() < Duration::from_secs(60) {
                    let have: HashSet<u32> = confirmed.iter().copied().collect();
                    let batch: Vec<u32> = (0..next).filter(|i| !have.contains(i)).chain(next..next + 100).take(100).collect();
                    next = next.max(batch.last().map_or(0, |l| l + 1));
                    confirmed.extend(publish_confirmed(&chp, "jobs", batch.into_iter()).await);
                }
                confirmed
            })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        nodes[owner].kill();
        let killed = Instant::now();
        let confirmed = publisher.await.unwrap();
        assert!(confirmed.len() >= 3000, "publishing recovered after the owner died ({} confirmed)", confirmed.len());
        wait_until("a survivor owns the queue", Duration::from_secs(10), async || nodes[p].owns("jobs") || nodes[q].owns("jobs")).await;
        eprintln!("failover: 3000 confirmed {:?} after the kill", killed.elapsed());

        let want: HashSet<u32> = confirmed.into_iter().collect();
        let seen = consume_ids(&nodes[q].url, "jobs", want.len(), Duration::from_secs(30)).await;
        let lost: Vec<_> = want.difference(&seen).collect();
        assert!(lost.is_empty(), "lost {} confirmed messages: {:?}", lost.len(), &lost[..lost.len().min(10)]);
    });
}

#[test]
fn the_owner_killed_under_load_loses_no_confirmed_message() {
    owner_killed_under_load(Env::memory());
}

#[test]
fn the_owner_killed_under_load_loses_no_confirmed_message_on_dynamodb() {
    if let Some(env) = Env::dynamodb("kill") {
        owner_killed_under_load(env);
    }
}

/// Queues spread to nodes that join, and a node that drains (a deploy, scale-in, Spot) hands
/// every queue off, while a consumer and a publisher on other nodes carry on without
/// reconnecting and without losing a confirmed message.
#[test]
fn a_drain_hands_every_queue_off_while_clients_carry_on() {
    let env = Env::memory();
    let a = Node::start(&env, "a");
    let client = Runtime::new().unwrap();
    let queues: Vec<String> = (0..12).map(|i| format!("q{i}")).collect();
    client.block_on(async {
        let (_ca, cha) = channel(&a.url).await;
        for q in &queues {
            declare(&cha, q).await;
        }
    });
    assert_eq!(a.cluster.owned_count(), 12, "a node alone owns every queue");
    let b = Node::start(&env, "b");
    let c = Node::start(&env, "c");
    client.block_on(async {
        known_on(&b, "q11").await;
        known_on(&c, "q11").await;
        let (cb, chb) = channel(&b.url).await;
        chb.basic_qos(100, BasicQosOptions::default()).await.unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(HashSet::<String>::new()));
        let mut tasks = Vec::new();
        for q in &queues {
            let mut cons = chb.basic_consume(q.as_str().into(), "".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
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
            let (chc, queues) = (chc.clone(), queues.clone());
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

        // roll-cluster rebalances onto the new nodes while traffic flows.
        wait_until("queues spread over b and c", Duration::from_secs(20), async || b.cluster.owned_count() > 0 && c.cluster.owned_count() > 0).await;

        // A rolling upgrade: a drains completely while traffic keeps flowing.
        let an = a.nodes.clone();
        a.spawn(async move { an.drain(Duration::from_secs(15)).await }).await.unwrap();
        assert_eq!(a.cluster.owned_count(), 0, "the drain handed off every queue");
        wait_until("b and c own every queue", Duration::from_secs(10), async || b.cluster.owned_count() + c.cluster.owned_count() == 12).await;

        let confirmed = publisher.await.unwrap();
        assert_eq!(confirmed.len(), 300 * 12, "every publish confirmed through the moves");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(20) && !confirmed.iter().all(|m| seen.lock().contains(m)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let missing = confirmed.iter().filter(|m| !seen.lock().contains(*m)).count();
        assert_eq!(missing, 0, "the consumer on b received every confirmed message without reconnecting");
        assert!(cb.status().connected(), "client connections never moved");
        for t in tasks {
            t.abort();
        }
    });
}

/// An owner that vanishes without a word (its connections neither close nor answer, as when a
/// machine dies) doesn't hold up its queue's publishers on other nodes past a lease: the RPC
/// connection to it is dropped when it stops answering pings, so a publish in flight goes to the
/// next owner and is confirmed there, and nothing confirmed before is lost.
#[test]
fn an_owner_that_vanishes_without_a_word_holds_nothing_up() {
    let env = Env::memory();
    let client = Runtime::new().unwrap();
    let cuts = [Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false))];
    let nodes = [Node::start_with(&env, "a", Some((&client, cuts[0].clone()))), Node::start_with(&env, "b", Some((&client, cuts[1].clone())))];
    client.block_on(async {
        let (_c, ch) = channel(&nodes[0].url).await;
        declare(&ch, "silent").await;
        known_on(&nodes[1], "silent").await;
        let key = mq_cluster::queue_key("/", "silent", 0);
        wait_until("the queue on its node", Duration::from_secs(10), async || {
            let at = nodes[0].nodes.preferred(&key).map(|n| n.id);
            nodes.iter().any(|n| Some(n.nodes.node_id()) == at.as_deref() && n.owns("silent"))
        })
        .await;
        let owner = nodes.iter().position(|n| n.owns("silent")).unwrap();
        let other = &nodes[1 - owner];
        let (_c, ch) = channel(&other.url).await;
        ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        let mut confirmed = publish_confirmed(&ch, "silent", 0..50).await;
        assert_eq!(confirmed.len(), 50);

        // The owner goes silent: its membership stops (its lease lapses) and its connections
        // carry nothing more, without closing.
        cuts[owner].store(true, Ordering::SeqCst);
        nodes[owner].nodes.fault_kill();
        let t = Instant::now();
        let props = BasicProperties::default().with_delivery_mode(2).with_message_id("50".into());
        let c = ch.basic_publish("".into(), "silent".into(), BasicPublishOptions::default(), b"50", props).await.unwrap();
        let c = tokio::time::timeout(Duration::from_secs(25), c).await.expect("the publish in flight is confirmed by the next owner").unwrap();
        assert!(c.is_ack(), "{c:?}");
        confirmed.push(50);
        assert!(other.owns("silent"), "the other node took the queue over");
        eprintln!("confirmed {:.1}s after the owner went silent", t.elapsed().as_secs_f64());
        confirmed.extend(publish_confirmed(&ch, "silent", 51..100).await);
        assert_eq!(confirmed.len(), 100);
        let seen = consume_ids(&other.url, "silent", 100, Duration::from_secs(20)).await;
        assert_eq!(seen, confirmed.into_iter().collect::<HashSet<u32>>(), "every confirmed message is there");
    });
}

/// Correctness never rests on the lease: an owner whose membership died but whose process keeps
/// running (cut off, frozen) is fenced out of the log by the next owner, so it can't confirm a
/// message the next owner doesn't have.
#[test]
fn a_deposed_owner_cannot_confirm() {
    let env = Env::memory();
    let a = Node::start(&env, "a");
    let client = Runtime::new().unwrap();
    let (_ca, cha) = client.block_on(async {
        let (ca, cha) = channel(&a.url).await;
        declare(&cha, "fenced").await;
        cha.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        assert_eq!(publish_confirmed(&cha, "fenced", 0..100).await.len(), 100);
        assert!(a.owns("fenced"));
        (ca, cha)
    });
    // a's membership dies without a word; its broker, actor and client connection go on.
    a.nodes.fault_kill();
    let b = Node::start(&env, "b");
    client.block_on(async {
        known_on(&b, "fenced").await;
        // Until a's lease lapses b forwards to a (whose actor still confirms: it still owns the
        // log); then b takes the lease over, fencing a's log.
        let (_cb, chb) = channel(&b.url).await;
        chb.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        let mut from_b = Vec::new();
        let mut next = 1000;
        let t = Instant::now();
        while !b.owns("fenced") {
            assert!(t.elapsed() < Duration::from_secs(15), "b never took the queue over");
            from_b.extend(publish_confirmed(&chb, "fenced", next..next + 10).await);
            next += 10;
        }
        from_b.extend(publish_confirmed(&chb, "fenced", next..next + 10).await);

        // a still runs the queue it no longer owns: its publishes can't be confirmed.
        let from_a = publish_confirmed(&cha, "fenced", 2000..2010).await;

        let want: HashSet<u32> = (0..100).chain(from_b).collect();
        let seen = consume_ids(&b.url, "fenced", want.len() + from_a.len(), Duration::from_secs(20)).await;
        let kept: Vec<&u32> = from_a.iter().filter(|i| seen.contains(i)).collect();
        assert!(from_a.is_empty(), "a deposed owner confirmed {from_a:?} (the new owner has {kept:?} of them)");
        assert_eq!(seen, want, "the new owner has exactly what was confirmed");
    });
}

/// Stream protocol clients connected to any node publish to and read from the stream's owner:
/// `Metadata` names the owner as leader, as RabbitMQ does for its stream members.
#[test]
fn stream_clients_are_sent_to_the_owner() {
    use rabbitmq_stream_client::Environment;
    use rabbitmq_stream_client::types::{Message, OffsetSpecification};
    let env = Env::memory();
    let (a, b) = (Node::start(&env, "a"), Node::start(&env, "b"));
    let client = Runtime::new().unwrap();
    client.block_on(async {
        let (_c, ch) = channel(&a.url).await;
        let mut args = FieldTable::default();
        args.insert("x-queue-type".into(), AMQPValue::LongString("stream".into()));
        ch.queue_declare("events".into(), QueueDeclareOptions { durable: true, ..Default::default() }, args).await.unwrap();
        known_on(&b, "events").await;
        // Settled where placement puts it (a may take it before it sees b, then hand it over).
        let key = mq_cluster::queue_key("/", "events", 0);
        wait_until("the stream on its node", Duration::from_secs(10), async || {
            let at = a.nodes.preferred(&key).map(|n| n.id);
            [&a, &b].iter().any(|n| Some(n.nodes.node_id()) == at.as_deref() && n.owns("events"))
        })
        .await;
        // Connect to the node that doesn't own it.
        let other = if a.owns("events") { &b } else { &a };
        let env = Environment::builder().host("127.0.0.1").port(other.stream_port).build().await.unwrap();
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

/// Behind the edge, every node answers stream clients as one virtual node at the address the
/// client used (`Open` and `Metadata` name it), and the stream's router carries publishes and
/// reads to wherever the stream runs. A producer (deduplicating) and a consumer that only ever
/// reach a node that doesn't own the stream keep going while the stream moves (its owner drains),
/// without reconnecting: every confirmed message is read once, in order.
#[test]
fn stream_clients_behind_the_edge_see_one_node_across_a_move() {
    use rabbitmq_stream_client::Environment;
    use rabbitmq_stream_client::types::{Message, OffsetSpecification};
    let env = Env::memory();
    let nodes = [Node::start(&env, "a"), Node::start(&env, "b"), Node::start(&env, "c")];
    let client = Runtime::new().unwrap();
    client.block_on(async {
        let (_c, ch) = channel(&nodes[0].url).await;
        let mut args = FieldTable::default();
        args.insert("x-queue-type".into(), AMQPValue::LongString("stream".into()));
        ch.queue_declare("events".into(), QueueDeclareOptions { durable: true, ..Default::default() }, args).await.unwrap();
        for n in &nodes {
            known_on(n, "events").await;
        }
        let key = mq_cluster::queue_key("/", "events", 0);
        wait_until("the stream on its node", Duration::from_secs(10), async || {
            let at = nodes[0].nodes.preferred(&key).map(|n| n.id);
            nodes.iter().any(|n| Some(n.nodes.node_id()) == at.as_deref() && n.owns("events"))
        })
        .await;
        let owner = nodes.iter().position(|n| n.owns("events")).unwrap();
        let edge = (owner + 1) % 3;

        // The edge's stream port on a node that doesn't own the stream: one virtual node, at the
        // address the client connects to.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let handle = nodes[edge].stream.clone();
        nodes[edge].spawn(async move {
            while let Ok((sock, peer)) = l.accept().await {
                let (h, local) = (handle.clone(), sock.local_addr().unwrap());
                tokio::spawn(async move { h.serve_connection(sock, peer, local, false, Some(("127.0.0.1".to_string(), port))).await });
            }
        });

        let senv = Environment::builder().host("127.0.0.1").port(port).build().await.unwrap();
        let mut producer = senv.producer().name("app").build("events").await.unwrap();
        let mut consumer = senv.consumer().offset(OffsetSpecification::First).build("events").await.unwrap();
        let got = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let reader = {
            let got = got.clone();
            tokio::spawn(async move {
                while let Ok(Some(Ok(d))) = tokio::time::timeout(Duration::from_secs(20), consumer.next()).await {
                    got.lock().push(String::from_utf8(d.message().data().unwrap().to_vec()).unwrap());
                }
            })
        };
        let mut send = async |i: u32| {
            let c = tokio::time::timeout(Duration::from_secs(20), producer.send_with_confirm(Message::builder().body(format!("e{i}")).build())).await;
            assert!(matches!(c, Ok(Ok(ref c)) if c.confirmed()), "e{i} not confirmed: {c:?}");
        };
        for i in 0..300 {
            send(i).await;
        }
        // The owner drains while the clients carry on: the stream moves.
        let on = nodes[owner].nodes.clone();
        let drain = nodes[owner].spawn(async move { on.drain(Duration::from_secs(15)).await });
        for i in 300..600 {
            send(i).await;
        }
        drain.await.unwrap();
        assert!(!nodes[owner].owns("events") && nodes.iter().any(|n| n.owns("events")), "the stream moved");
        for i in 600..800 {
            send(i).await;
        }
        let want: Vec<String> = (0..800).map(|i| format!("e{i}")).collect();
        let t = Instant::now();
        while got.lock().len() < want.len() && t.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(*got.lock(), want, "read once each, in order");
        // The clients never reached any node but the edge's.
        for (i, n) in nodes.iter().enumerate() {
            let streams = n.broker.connections().iter().filter(|c| c.protocol == "stream").count();
            assert_eq!(streams > 0, i == edge, "stream connections on node {i}: {streams}");
        }
        let _ = producer.close().await;
        reader.abort();
    });
}

// ── DynamoDB Local ──

const DDB_CONTAINER: &str = "roll-cloud-queues-ddb";

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

extern "C" fn remove_container() {
    let _ = Command::new("docker").args(["rm", "-f", DDB_CONTAINER]).output();
}

fn answers_http(addr: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(addr) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 12];
    s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_ok() && s.read_exact(&mut buf).is_ok() && buf.starts_with(b"HTTP/")
}

/// DynamoDB Local: `ROLL_CLUSTER_TEST_DYNAMODB` (the gates start one), else a container this
/// test binary starts and removes when it exits. `None` (with a message) without Docker, unless
/// `ROLL_CLUSTER_DYNAMODB_REQUIRED=1`.
fn dynamodb_endpoint() -> Option<String> {
    static EP: OnceLock<Option<String>> = OnceLock::new();
    let ep = EP.get_or_init(|| {
        if let Ok(e) = std::env::var("ROLL_CLUSTER_TEST_DYNAMODB")
            && !e.is_empty()
        {
            return Some(e);
        }
        let port = |s: &str| s.lines().next().and_then(|l| l.rsplit(':').next()).map(str::to_string);
        let p = match docker(&["port", DDB_CONTAINER, "8000/tcp"]).ok().and_then(|s| port(&s)) {
            Some(p) => p,
            None => {
                if let Err(e) = docker(&["run", "-d", "--rm", "--name", DDB_CONTAINER, "-p", "127.0.0.1::8000", "amazon/dynamodb-local"]) {
                    eprintln!("skipping the DynamoDB test: Docker can't start amazon/dynamodb-local ({e})");
                    return None;
                }
                // SAFETY: registering a plain extern "C" function to run at exit.
                unsafe { libc::atexit(remove_container) };
                port(&docker(&["port", DDB_CONTAINER, "8000/tcp"]).ok()?)?
            }
        };
        let addr = format!("127.0.0.1:{p}");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !answers_http(&addr) {
            if Instant::now() > deadline {
                eprintln!("skipping the DynamoDB test: DynamoDB Local never answered on {addr}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Some(format!("http://{addr}"))
    });
    if ep.is_none() && std::env::var("ROLL_CLUSTER_DYNAMODB_REQUIRED").is_ok_and(|v| v == "1") {
        panic!("DynamoDB Local is required (ROLL_CLUSTER_DYNAMODB_REQUIRED=1) but isn't available");
    }
    ep.clone()
}

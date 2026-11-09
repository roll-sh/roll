//! The stream protocol against an in-process broker, driven by the unmodified
//! `rabbitmq-stream-client` crate (and lapin for AMQP 0-9-1 interop).

use futures::StreamExt;
use lapin::options::{BasicConsumeOptions, BasicPublishOptions, BasicQosOptions};
use lapin::types::{AMQPValue, FieldTable, LongString};
use lapin::{BasicProperties, ConnectionProperties};
use mq_core::log::FileStore;
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use rabbitmq_stream_client::types::{Message, OffsetSpecification, RoutingKeyRoutingStrategy, RoutingStrategy};
use rabbitmq_stream_client::{Client, ClientOptions, Consumer, Environment, FilterConfiguration};
use std::sync::Arc;
use std::time::Duration;

struct Env {
    broker: Arc<Broker>,
    port: u16,
    env: Environment,
    _dir: tempfile::TempDir,
}

impl Env {
    async fn start() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), FileStore::new(dir.path())).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = mq_stream::Config { advertised_host: "127.0.0.1".into(), ..mq_stream::Config::new(addr) };
        tokio::spawn(mq_stream::serve_listener(broker.clone(), listener, config));
        let env = Environment::builder().host("127.0.0.1").port(addr.port()).build().await.unwrap();
        Env { broker, port: addr.port(), env, _dir: dir }
    }

    async fn client(&self) -> Client {
        Client::connect(ClientOptions::builder().host("127.0.0.1").port(self.port).build()).await.unwrap()
    }

    /// An AMQP 0-9-1 listener on the same broker.
    async fn amqp(&self) -> lapin::Connection {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(mq_amqp::serve_listener(self.broker.clone(), listener, mq_amqp::ServerConfig { listen: addr, amqp10: None }));
        let url = format!("amqp://guest:guest@127.0.0.1:{}/%2F", addr.port());
        lapin::Connection::connect(&url, ConnectionProperties::default()).await.unwrap()
    }
}

fn msg(body: &str) -> Message {
    Message::builder().body(body.as_bytes()).build()
}

fn text(m: &Message) -> String {
    String::from_utf8_lossy(m.data().unwrap_or_default()).into_owned()
}

/// The next `n` deliveries as (offset, body).
async fn take(c: &mut Consumer, n: usize) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    while out.len() < n {
        let d = tokio::time::timeout(Duration::from_secs(10), c.next()).await.expect("delivery timed out").unwrap().unwrap();
        out.push((d.offset(), text(d.message())));
    }
    out
}

async fn nothing(c: &mut Consumer) {
    if let Ok(Some(d)) = tokio::time::timeout(Duration::from_millis(400), c.next()).await {
        panic!("unexpected delivery: {:?}", d.map(|d| text(d.message())));
    }
}

fn bodies(v: &[(u64, String)]) -> Vec<&str> {
    v.iter().map(|(_, b)| b.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_confirm_and_consume_from_offsets() {
    let e = Env::start().await;
    e.env.stream_creator().create("s1").await.unwrap();
    // A second create of the same stream: STREAM_ALREADY_EXISTS.
    let again = e.env.stream_creator().create("s1").await.unwrap_err();
    assert!(format!("{again:?}").contains("StreamAlreadyExists"), "{again:?}");

    let producer = e.env.producer().build("s1").await.unwrap();
    let before = mq_core::now_ms();
    let st = producer.batch_send_with_confirm((0..10).map(|i| msg(&format!("m{i}"))).collect()).await.unwrap();
    assert!(st.iter().all(|s| s.confirmed()));
    assert!(producer.send_with_confirm(msg("m10")).await.unwrap().confirmed());

    let mut c = e.env.consumer().offset(OffsetSpecification::First).build("s1").await.unwrap();
    let got = take(&mut c, 11).await;
    assert_eq!(got.iter().map(|(o, _)| *o).collect::<Vec<_>>(), (0..11).collect::<Vec<_>>());
    assert_eq!(got[10].1, "m10");

    let mut c = e.env.consumer().offset(OffsetSpecification::Offset(7)).build("s1").await.unwrap();
    assert_eq!(bodies(&take(&mut c, 4).await), ["m7", "m8", "m9", "m10"]);

    let mut c = e.env.consumer().offset(OffsetSpecification::Timestamp(before as i64 - 1000)).build("s1").await.unwrap();
    assert_eq!(take(&mut c, 1).await[0].1, "m0");

    // `next` only sees what is published afterwards.
    let mut c = e.env.consumer().offset(OffsetSpecification::Next).build("s1").await.unwrap();
    nothing(&mut c).await;
    producer.send_with_confirm(msg("late")).await.unwrap();
    assert_eq!(take(&mut c, 1).await, [(11, "late".to_string())]);

    // Unknown stream.
    let err = e.env.producer().build("missing").await.err().unwrap();
    assert!(format!("{err:?}").contains("StreamDoesNotExist"), "{err:?}");
    e.env.delete_stream("s1").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn store_and_query_offset() {
    let e = Env::start().await;
    e.env.stream_creator().create("s2").await.unwrap();
    let producer = e.env.producer().build("s2").await.unwrap();
    producer.batch_send_with_confirm((0..5).map(|i| msg(&format!("m{i}"))).collect()).await.unwrap();

    let mut c = e.env.consumer().name("app").offset(OffsetSpecification::First).build("s2").await.unwrap();
    let err = c.query_offset().await.unwrap_err();
    assert!(format!("{err:?}").contains("OffsetNotFound"), "{err:?}");
    take(&mut c, 3).await;
    c.store_offset(2).await.unwrap();
    // StoreOffset has no response: poll until it is visible.
    let mut seen = None;
    for _ in 0..50 {
        if let Ok(o) = c.query_offset().await {
            seen = Some(o);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(seen, Some(2));

    // Another connection sees it; resuming at offset + 1.
    let client = e.client().await;
    assert_eq!(client.query_offset("app".into(), "s2").await.unwrap(), 2);
    let mut c2 = e.env.consumer().offset(OffsetSpecification::Offset(3)).build("s2").await.unwrap();
    assert_eq!(bodies(&take(&mut c2, 2).await), ["m3", "m4"]);
    let err = client.query_offset("app".into(), "nope").await.unwrap_err();
    assert!(format!("{err:?}").contains("StreamDoesNotExist"), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn deduplication_across_reconnects() {
    let e = Env::start().await;
    e.env.stream_creator().create("s3").await.unwrap();
    let mut p = e.env.producer().name("writer").build("s3").await.unwrap();
    for id in [1, 2, 3, 2, 3, 4] {
        let m = Message::builder().body(format!("m{id}")).publishing_id(id).build();
        assert!(p.send_with_confirm(m).await.unwrap().confirmed(), "duplicates are confirmed too");
    }
    p.close().await.unwrap();

    let client = e.client().await;
    assert_eq!(client.query_publisher_sequence("writer", "s3").await.unwrap(), 4);

    // A new producer with the same name resumes after 4 and resends are dropped.
    let mut p = e.env.producer().name("writer").build("s3").await.unwrap();
    p.send_with_confirm(Message::builder().body("dup").publishing_id(3).build()).await.unwrap();
    p.send_with_confirm(msg("m5")).await.unwrap();
    let mut c = e.env.consumer().offset(OffsetSpecification::First).build("s3").await.unwrap();
    assert_eq!(bodies(&take(&mut c, 5).await), ["m1", "m2", "m3", "m4", "m5"]);
    nothing(&mut c).await;
    assert_eq!(client.query_publisher_sequence("writer", "s3").await.unwrap(), 5);
}

fn region(m: &Message) -> String {
    text(m).split(':').next().unwrap_or_default().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn super_stream_routing() {
    let e = Env::start().await;
    e.env.stream_creator().create_super_stream("orders", 3, None).await.unwrap();
    let client = e.client().await;
    assert_eq!(client.partitions("orders".into()).await.unwrap().streams, ["orders-0", "orders-1", "orders-2"]);
    assert_eq!(client.route("2".into(), "orders".into()).await.unwrap().streams, ["orders-2"]);
    assert!(client.route("9".into(), "orders".into()).await.unwrap().streams.is_empty());
    assert!(!client.partitions("nope".into()).await.unwrap().is_ok());

    let strategy = RoutingStrategy::RoutingKeyStrategy(RoutingKeyRoutingStrategy { routing_extractor: &region });
    let mut p = e.env.super_stream_producer(strategy).build("orders").await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for m in ["0:a", "1:b", "2:c", "1:d"] {
        let tx = tx.clone();
        p.send(msg(m), move |r| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(r.is_ok_and(|s| s.confirmed()));
            }
        })
        .await
        .unwrap();
    }
    for _ in 0..4 {
        assert!(tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap());
    }
    let mut c = e.env.consumer().offset(OffsetSpecification::First).build("orders-1").await.unwrap();
    assert_eq!(bodies(&take(&mut c, 2).await), ["1:b", "1:d"]);
    nothing(&mut c).await;

    e.env.delete_super_stream("orders").await.unwrap();
    assert!(!client.partitions("orders".into()).await.unwrap().is_ok());
    assert!(e.env.producer().build("orders-0").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn single_active_consumer_fails_over() {
    let e = Env::start().await;
    e.env.stream_creator().create("s4").await.unwrap();
    let producer = e.env.producer().build("s4").await.unwrap();
    producer.batch_send_with_confirm((0..3).map(|i| msg(&format!("m{i}"))).collect()).await.unwrap();

    let sac = |e: &Env| {
        e.env
            .consumer()
            .name("group")
            .enable_single_active_consumer(true)
            .offset(OffsetSpecification::First)
            .consumer_update(|_active, _ctx| async { OffsetSpecification::First })
    };
    let mut a = sac(&e).build("s4").await.unwrap();
    let mut b = sac(&e).build("s4").await.unwrap();
    assert_eq!(bodies(&take(&mut a, 3).await), ["m0", "m1", "m2"]);
    nothing(&mut b).await;

    // When the active consumer leaves, the other one takes over from where its listener says.
    a.handle().close().await.unwrap();
    assert_eq!(bodies(&take(&mut b, 3).await), ["m0", "m1", "m2"]);
    producer.send_with_confirm(msg("m3")).await.unwrap();
    assert_eq!(bodies(&take(&mut b, 1).await), ["m3"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn filter_values() {
    let e = Env::start().await;
    e.env.stream_creator().create("s5").await.unwrap();
    let producer = e.env.producer().filter_value_extractor(region).build("s5").await.unwrap();
    let all = ["red:1", "blue:2", "red:3", "green:4", "blue:5"];
    producer.batch_send_with_confirm(all.iter().map(|m| msg(m)).collect()).await.unwrap();

    let filter = FilterConfiguration::new(vec!["blue".into()], false);
    let mut c = e.env.consumer().offset(OffsetSpecification::First).filter_input(Some(filter)).build("s5").await.unwrap();
    let got = take(&mut c, 2).await;
    assert_eq!(got, [(1, "blue:2".to_string()), (4, "blue:5".to_string())]);
    nothing(&mut c).await;

    let filter = FilterConfiguration::new(vec!["red".into(), "green".into()], false);
    let mut c = e.env.consumer().offset(OffsetSpecification::First).filter_input(Some(filter)).build("s5").await.unwrap();
    assert_eq!(bodies(&take(&mut c, 3).await), ["red:1", "red:3", "green:4"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn amqp091_interop() {
    let e = Env::start().await;
    e.env.stream_creator().create("mixed").await.unwrap();
    let producer = e.env.producer().build("mixed").await.unwrap();
    let m = Message::builder()
        .body("from-stream")
        .properties()
        .message_id("id-1".to_string())
        .content_type("text/plain")
        .message_builder()
        .application_properties()
        .insert("color", "blue")
        .message_builder()
        .build();
    producer.send_with_confirm(m).await.unwrap();

    // 0-9-1 reads the stream message.
    let conn = e.amqp().await;
    let ch = conn.create_channel().await.unwrap();
    ch.basic_qos(10, BasicQosOptions::default()).await.unwrap();
    let args = FieldTable::from(std::collections::BTreeMap::from([("x-stream-offset".into(), AMQPValue::LongString(LongString::from("first")))]));
    let mut consumer = ch.basic_consume("mixed".into(), "c".into(), BasicConsumeOptions::default(), args).await.unwrap();
    let d = tokio::time::timeout(Duration::from_secs(10), consumer.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(d.data, b"from-stream");
    assert_eq!(d.routing_key.as_str(), "mixed");
    let p = &d.properties;
    assert_eq!(p.message_id().as_ref().map(|s| s.as_str()), Some("id-1"));
    assert_eq!(p.content_type().as_ref().map(|s| s.as_str()), Some("text/plain"));
    let h = p.headers().as_ref().unwrap();
    assert_eq!(h.inner().get("color"), Some(&AMQPValue::LongString(LongString::from("blue"))));

    // The stream protocol reads a 0-9-1 message.
    let headers = FieldTable::from(std::collections::BTreeMap::from([("tier".into(), AMQPValue::LongString(LongString::from("gold")))]));
    let props = BasicProperties::default().with_headers(headers).with_message_id("m-2".into());
    ch.basic_publish("".into(), "mixed".into(), BasicPublishOptions::default(), b"from-091", props).await.unwrap().await.unwrap();
    let mut c = e.env.consumer().offset(OffsetSpecification::First).build("mixed").await.unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let d = tokio::time::timeout(Duration::from_secs(10), c.next()).await.unwrap().unwrap().unwrap();
        seen.push(d.message().clone());
    }
    assert_eq!(text(&seen[0]), "from-stream");
    assert_eq!(text(&seen[1]), "from-091");
    let ap = seen[1].application_properties().expect("application properties");
    assert!(format!("{ap:?}").contains("gold"), "{ap:?}");
    let id = seen[1].properties().and_then(|p| p.message_id.clone());
    assert!(format!("{id:?}").contains("m-2"), "{id:?}");
    conn.close(200, "bye".into()).await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_is_listed_and_force_closed() {
    let e = Env::start().await;
    let client = e.client().await;
    let conns = e.broker.connections();
    let info = conns.iter().find(|c| c.protocol == "stream").expect("stream connection registered");
    info.request_close("bye");
    // The server sends Close and drops the socket at the latest after its close timeout.
    let gone = || !e.broker.connections().iter().any(|c| c.protocol == "stream");
    for _ in 0..250 {
        if gone() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone());
    assert!(client.query_offset("x".into(), "y").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_credentials_are_refused() {
    let e = Env::start().await;
    let r = Client::connect(ClientOptions::builder().host("127.0.0.1").port(e.port).user("guest").password("wrong").build()).await;
    assert!(r.is_err());
    let r = Client::connect(ClientOptions::builder().host("127.0.0.1").port(e.port).v_host("nope").build()).await;
    assert!(r.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_drops_old_segments_as_they_roll() {
    use rabbitmq_stream_client::types::ByteCapacity;
    let e = Env::start().await;
    e.env.stream_creator().max_length(ByteCapacity::KB(200)).max_segment_size(ByteCapacity::KB(50)).create("kept").await.unwrap();
    let producer = e.env.producer().build("kept").await.unwrap();
    let body = "x".repeat(1000);
    for _ in 0..10 {
        let st = producer.batch_send_with_confirm((0..100).map(|_| msg(&body)).collect()).await.unwrap();
        assert!(st.iter().all(|s| s.confirmed()));
    }
    // Retention runs when a segment rolls, not on a timer.
    let q = e.broker.vhost("/").unwrap().queue("kept").unwrap();
    let mut first = 0;
    for _ in 0..20 {
        first = q.shared.first_offset.load(std::sync::atomic::Ordering::Relaxed);
        if first > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!((500..1000).contains(&first), "first offset {first}");
    let mut c = e.env.consumer().offset(OffsetSpecification::First).build("kept").await.unwrap();
    let from = take(&mut c, 1).await[0].0;
    assert!(from >= first && from < 1000, "first delivered offset {from}");
}

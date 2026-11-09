//! AMQP 0-9-1 compatibility tests against an in-process broker, driven by an unmodified
//! client library (lapin). The Docker client matrix (`scripts/test-queues.sh`) covers the other
//! languages.

use futures::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable, LongString, ShortString};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties, ExchangeKind};
use mq_core::log::{FileStore, LogStore};
use mq_core::meta::SqliteMeta;
use mq_core::{Broker, BrokerConfig};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Harness {
    broker: Arc<Broker>,
    logs: Arc<FileStore>,
    url: String,
    dir: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

fn unique(prefix: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("{prefix}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

impl Harness {
    async fn start() -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique("amqp"));
        Self::start_in(dir).await
    }

    async fn start_in(dir: PathBuf) -> Self {
        let _ = tracing_subscriber::fmt().with_env_filter("mq_core=info,mq_amqp=info").with_test_writer().try_init();
        let meta = SqliteMeta::open(&dir.join("meta.db")).unwrap();
        let logs = FileStore::with_wal(dir.join("logs"), 16 << 20).unwrap();
        let broker = Broker::start(BrokerConfig::default(), meta, logs.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = mq_amqp::ServerConfig { listen: addr, amqp10: None };
        let b = broker.clone();
        let server = tokio::spawn(async move {
            let _ = mq_amqp::serve_listener(b, listener, cfg).await;
        });
        Harness { broker, logs, url: format!("amqp://guest:guest@{addr}/%2f"), dir, server }
    }

    async fn restart(self) -> Self {
        self.server.abort();
        self.broker.stop().await;
        self.logs.close().await;
        let dir = self.dir.clone();
        drop(self);
        Self::start_in(dir).await
    }

    /// Restart as after a crash: the WAL stops dead and the next start replays it.
    async fn crash_restart(self) -> Self {
        self.server.abort();
        self.logs.crash_wal();
        self.broker.stop().await;
        let dir = self.dir.clone();
        drop(self);
        Self::start_in(dir).await
    }

    async fn conn(&self) -> Connection {
        Connection::connect(&self.url, ConnectionProperties::default()).await.unwrap()
    }

    async fn channel(&self) -> (Connection, Channel) {
        let c = self.conn().await;
        let ch = c.create_channel().await.unwrap();
        (c, ch)
    }
}

fn s(v: &str) -> ShortString {
    v.into()
}

fn args(pairs: &[(&str, AMQPValue)]) -> FieldTable {
    let mut t = FieldTable::default();
    for (k, v) in pairs {
        t.insert((*k).into(), v.clone());
    }
    t
}

fn lstr(v: &str) -> AMQPValue {
    AMQPValue::LongString(LongString::from(v))
}

async fn declare(ch: &Channel, name: &str, a: FieldTable) {
    ch.queue_declare(s(name), QueueDeclareOptions { durable: true, ..Default::default() }, a).await.unwrap();
}

async fn publish(ch: &Channel, exchange: &str, key: &str, body: &[u8], props: BasicProperties) {
    ch.basic_publish(s(exchange), s(key), BasicPublishOptions::default(), body, props).await.unwrap().await.unwrap();
}

async fn get(ch: &Channel, queue: &str) -> Option<lapin::message::BasicGetMessage> {
    ch.basic_get(s(queue), BasicGetOptions { no_ack: true }).await.unwrap()
}

async fn wait_get(ch: &Channel, queue: &str) -> lapin::message::BasicGetMessage {
    for _ in 0..200 {
        if let Some(m) = get(ch, queue).await {
            return m;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no message on {queue}");
}

fn persistent() -> BasicProperties {
    BasicProperties::default().with_delivery_mode(2)
}

#[tokio::test]
async fn publish_consume_ack() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "work", FieldTable::default()).await;
    ch.basic_qos(10, BasicQosOptions::default()).await.unwrap();
    let mut consumer = ch.basic_consume(s("work"), s("c1"), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    for i in 0..100 {
        publish(&ch, "", "work", format!("m{i}").as_bytes(), persistent()).await;
    }
    for i in 0..100 {
        let d = tokio::time::timeout(Duration::from_secs(5), consumer.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(d.data, format!("m{i}").as_bytes());
        assert!(!d.redelivered);
        d.ack(BasicAckOptions::default()).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let q = h.broker.vhost("/").unwrap().queue("work").unwrap();
    assert_eq!(q.shared.messages(), 0);
}

#[tokio::test]
async fn publisher_confirms_and_mandatory_returns() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
    declare(&ch, "confirmed", FieldTable::default()).await;
    let c = ch.basic_publish(s(""), s("confirmed"), BasicPublishOptions::default(), b"x", persistent()).await.unwrap().await.unwrap();
    assert!(c.is_ack());
    // Unroutable + mandatory: returned, then acked.
    let c = ch
        .basic_publish(s(""), s("nowhere"), BasicPublishOptions { mandatory: true, ..Default::default() }, b"lost", BasicProperties::default())
        .await
        .unwrap()
        .await
        .unwrap();
    assert!(c.is_ack());
    let ret = c.take_message().expect("basic.return");
    assert_eq!(ret.reply_code, 312);
    assert_eq!(ret.delivery.data, b"lost");
}

#[tokio::test]
async fn topic_fanout_headers_and_e2e_bindings() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    ch.exchange_declare(s("logs"), ExchangeKind::Topic, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    ch.exchange_declare(s("fan"), ExchangeKind::Fanout, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    ch.exchange_declare(s("hdr"), ExchangeKind::Headers, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    for q in ["errors", "all", "fan-a", "hdr-q"] {
        declare(&ch, q, FieldTable::default()).await;
    }
    ch.queue_bind(s("errors"), s("logs"), s("*.error"), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    ch.queue_bind(s("all"), s("logs"), s("#"), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    // fan → logs (exchange-to-exchange), and fan → fan-a.
    ch.exchange_bind(s("logs"), s("fan"), s(""), ExchangeBindOptions::default(), FieldTable::default()).await.unwrap();
    ch.queue_bind(s("fan-a"), s("fan"), s(""), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    ch.queue_bind(s("hdr-q"), s("hdr"), s(""), QueueBindOptions::default(), args(&[("x-match", lstr("all")), ("kind", lstr("pdf"))])).await.unwrap();

    publish(&ch, "logs", "app.error", b"e1", BasicProperties::default()).await;
    publish(&ch, "logs", "app.info", b"i1", BasicProperties::default()).await;
    publish(&ch, "fan", "app.error", b"f1", BasicProperties::default()).await;
    let mut h1 = FieldTable::default();
    h1.insert("kind".into(), lstr("pdf"));
    publish(&ch, "hdr", "", b"h1", BasicProperties::default().with_headers(h1)).await;
    let mut h2 = FieldTable::default();
    h2.insert("kind".into(), lstr("png"));
    publish(&ch, "hdr", "", b"h2", BasicProperties::default().with_headers(h2)).await;

    let drain = async |q: &str| {
        let mut v = vec![];
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Some(m) = get(&ch, q).await {
            v.push(String::from_utf8(m.delivery.data.clone()).unwrap());
        }
        v
    };
    assert_eq!(drain("errors").await, ["e1", "f1"]);
    assert_eq!(drain("all").await, ["e1", "i1", "f1"]);
    assert_eq!(drain("fan-a").await, ["f1"]);
    assert_eq!(drain("hdr-q").await, ["h1"]);
}

#[tokio::test]
async fn dead_lettering_ttl_and_reject() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    ch.exchange_declare(s("dlx"), ExchangeKind::Fanout, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    declare(&ch, "dead", FieldTable::default()).await;
    ch.queue_bind(s("dead"), s("dlx"), s(""), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    declare(&ch, "ttl", args(&[("x-message-ttl", AMQPValue::LongInt(50)), ("x-dead-letter-exchange", lstr("dlx"))])).await;
    publish(&ch, "", "ttl", b"expires", BasicProperties::default()).await;
    let m = wait_get(&ch, "dead").await;
    assert_eq!(m.delivery.data, b"expires");
    let headers = m.delivery.properties.headers().clone().unwrap();
    let deaths = headers.inner().get("x-death").unwrap();
    let AMQPValue::FieldArray(arr) = deaths else { panic!("x-death not an array") };
    let AMQPValue::FieldTable(first) = &arr.as_slice()[0] else { panic!() };
    assert_eq!(first.inner().get("reason"), Some(&lstr("expired")));
    assert_eq!(first.inner().get("queue"), Some(&lstr("ttl")));

    // Rejected without requeue → dead-lettered with reason "rejected".
    declare(&ch, "rej", args(&[("x-dead-letter-exchange", lstr("dlx"))])).await;
    publish(&ch, "", "rej", b"bad", BasicProperties::default()).await;
    let g = ch.basic_get(s("rej"), BasicGetOptions { no_ack: false }).await.unwrap().unwrap();
    g.delivery.reject(BasicRejectOptions { requeue: false }).await.unwrap();
    let m = wait_get(&ch, "dead").await;
    assert_eq!(m.delivery.data, b"bad");
}

#[tokio::test]
async fn max_length_overflow_modes() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "capped", args(&[("x-max-length", AMQPValue::LongInt(3))])).await;
    for i in 0..5 {
        publish(&ch, "", "capped", format!("{i}").as_bytes(), BasicProperties::default()).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut got = vec![];
    while let Some(m) = get(&ch, "capped").await {
        got.push(String::from_utf8(m.delivery.data.clone()).unwrap());
    }
    assert_eq!(got, ["2", "3", "4"], "drop-head keeps the newest");

    ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
    declare(&ch, "rejecting", args(&[("x-max-length", AMQPValue::LongInt(1)), ("x-overflow", lstr("reject-publish"))])).await;
    let a = ch.basic_publish(s(""), s("rejecting"), BasicPublishOptions::default(), b"1", BasicProperties::default()).await.unwrap().await.unwrap();
    let b = ch.basic_publish(s(""), s("rejecting"), BasicPublishOptions::default(), b"2", BasicProperties::default()).await.unwrap().await.unwrap();
    assert!(a.is_ack());
    assert!(b.is_nack(), "reject-publish nacks when full");
}

#[tokio::test]
async fn priority_queue() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "prio", args(&[("x-max-priority", AMQPValue::ShortShortUInt(10))])).await;
    for (body, p) in [("low", 1u8), ("high", 9), ("mid", 5), ("none", 0)] {
        publish(&ch, "", "prio", body.as_bytes(), BasicProperties::default().with_priority(p)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut order = vec![];
    while let Some(m) = get(&ch, "prio").await {
        order.push(String::from_utf8(m.delivery.data.clone()).unwrap());
    }
    assert_eq!(order, ["high", "mid", "low", "none"]);
}

#[tokio::test]
async fn nack_requeue_sets_redelivered() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "rq", FieldTable::default()).await;
    publish(&ch, "", "rq", b"again", BasicProperties::default()).await;
    let g = ch.basic_get(s("rq"), BasicGetOptions { no_ack: false }).await.unwrap().unwrap();
    assert!(!g.delivery.redelivered);
    g.delivery.nack(BasicNackOptions { requeue: true, multiple: false }).await.unwrap();
    let g = wait_get(&ch, "rq").await;
    assert!(g.delivery.redelivered);
}

#[tokio::test]
async fn errors_match_rabbitmq() {
    let h = Harness::start().await;
    let c = h.conn().await;
    let ch = c.create_channel().await.unwrap();
    declare(&ch, "strict", args(&[("x-max-length", AMQPValue::LongInt(10))])).await;
    let e = ch
        .queue_declare(s("strict"), QueueDeclareOptions { durable: true, ..Default::default() }, args(&[("x-max-length", AMQPValue::LongInt(20))]))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("PRECONDITION_FAILED"), "{e}");
    assert!(e.to_string().contains("inequivalent arg 'x-max-length'"), "{e}");

    let ch = c.create_channel().await.unwrap();
    let e = ch.queue_declare(s("missing"), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.unwrap_err();
    assert!(e.to_string().contains("NOT_FOUND - no queue 'missing' in vhost '/'"), "{e}");

    let ch = c.create_channel().await.unwrap();
    let e = ch.exchange_declare(s("nope"), ExchangeKind::Direct, ExchangeDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await.unwrap_err();
    assert!(e.to_string().contains("NOT_FOUND - no exchange 'nope' in vhost '/'"), "{e}");

    // Publishing to a missing exchange closes the channel asynchronously.
    let ch = c.create_channel().await.unwrap();
    let _ = ch.basic_publish(s("nope"), s(""), BasicPublishOptions::default(), b"", BasicProperties::default()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!ch.status().connected(), "channel closed by the broker");

    let ch = c.create_channel().await.unwrap();
    let e = ch.exchange_declare(s("amq.custom"), ExchangeKind::Direct, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap_err();
    assert!(e.to_string().contains("ACCESS_REFUSED"), "{e}");

    // The connection survives channel errors.
    let ch = c.create_channel().await.unwrap();
    ch.queue_declare(s("fine"), QueueDeclareOptions::default(), FieldTable::default()).await.unwrap();
}

#[tokio::test]
async fn exclusive_queue_is_locked_to_its_connection() {
    let h = Harness::start().await;
    let (c1, ch1) = h.channel().await;
    let q = ch1.queue_declare(s(""), QueueDeclareOptions { exclusive: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let name = q.name().to_string();
    assert!(name.starts_with("amq.gen-"));
    let (_c2, ch2) = h.channel().await;
    let e = ch2.basic_consume(s(&name), s(""), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap_err();
    assert!(e.to_string().contains("RESOURCE_LOCKED"), "{e}");
    c1.close(200, s("bye")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.broker.vhost("/").unwrap().queue(&name).is_none(), "exclusive queue deleted with its connection");
}

#[tokio::test]
async fn transactions_commit_and_rollback() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "txq", FieldTable::default()).await;
    ch.tx_select().await.unwrap();
    ch.basic_publish(s(""), s("txq"), BasicPublishOptions::default(), b"rolled-back", BasicProperties::default()).await.unwrap();
    ch.tx_rollback().await.unwrap();
    ch.basic_publish(s(""), s("txq"), BasicPublishOptions::default(), b"committed", BasicProperties::default()).await.unwrap();
    ch.tx_commit().await.unwrap();
    let (_c2, ch2) = h.channel().await;
    let m = wait_get(&ch2, "txq").await;
    assert_eq!(m.delivery.data, b"committed");
    assert!(get(&ch2, "txq").await.is_none());
}

#[tokio::test]
async fn durable_messages_survive_restart() {
    durable_messages_survive(false).await;
}

#[tokio::test]
async fn durable_messages_survive_a_crash() {
    durable_messages_survive(true).await;
}

async fn durable_messages_survive(crash: bool) {
    let h = Harness::start().await;
    {
        let (c, ch) = h.channel().await;
        ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        declare(&ch, "keep", FieldTable::default()).await;
        declare(&ch, "keep-q", args(&[("x-queue-type", lstr("quorum"))])).await;
        ch.exchange_declare(s("kx"), ExchangeKind::Direct, ExchangeDeclareOptions { durable: true, ..Default::default() }, FieldTable::default()).await.unwrap();
        ch.queue_bind(s("keep"), s("kx"), s("k"), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
        for i in 0..50 {
            publish(&ch, "kx", "k", format!("p{i}").as_bytes(), persistent()).await;
        }
        publish(&ch, "", "keep", b"transient", BasicProperties::default()).await;
        publish(&ch, "", "keep-q", b"quorum", BasicProperties::default()).await;
        // Ack the first ten.
        for _ in 0..10 {
            let g = ch.basic_get(s("keep"), BasicGetOptions { no_ack: false }).await.unwrap().unwrap();
            g.delivery.ack(BasicAckOptions::default()).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.close(200, s("bye")).await.unwrap();
    }
    let h = if crash { h.crash_restart().await } else { h.restart().await };
    let (_c, ch) = h.channel().await;
    let mut got = vec![];
    while let Some(m) = get(&ch, "keep").await {
        got.push(String::from_utf8(m.delivery.data.clone()).unwrap());
    }
    let want: Vec<String> = (10..50).map(|i| format!("p{i}")).collect();
    assert_eq!(got, want, "persistent messages recovered in order; transient ones are not");
    assert_eq!(wait_get(&ch, "keep-q").await.delivery.data, b"quorum", "quorum queues persist everything");
    // The binding survived too.
    publish(&ch, "kx", "k", b"after", BasicProperties::default()).await;
    assert_eq!(wait_get(&ch, "keep").await.delivery.data, b"after");
}

#[tokio::test]
async fn stream_replay_from_offsets() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "events", args(&[("x-queue-type", lstr("stream"))])).await;
    ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
    for i in 0..20 {
        publish(&ch, "", "events", format!("e{i}").as_bytes(), BasicProperties::default()).await;
    }
    ch.basic_qos(100, BasicQosOptions::default()).await.unwrap();
    let read = async |offset: AMQPValue, tag: &str, n: usize| {
        let mut c = ch.basic_consume(s("events"), s(tag), BasicConsumeOptions::default(), args(&[("x-stream-offset", offset)])).await.unwrap();
        let mut v = vec![];
        for _ in 0..n {
            let d = tokio::time::timeout(Duration::from_secs(5), c.next()).await.unwrap().unwrap().unwrap();
            v.push(String::from_utf8(d.data.clone()).unwrap());
            d.ack(BasicAckOptions::default()).await.unwrap();
        }
        ch.basic_cancel(s(tag), BasicCancelOptions::default()).await.unwrap();
        v
    };
    assert_eq!(read(lstr("first"), "a", 3).await, ["e0", "e1", "e2"]);
    assert_eq!(read(AMQPValue::LongLongInt(15), "b", 5).await, ["e15", "e16", "e17", "e18", "e19"]);
    // Two consumers read the same data independently (non-destructive).
    assert_eq!(read(lstr("first"), "c", 1).await, ["e0"]);
}

#[tokio::test]
async fn consumer_prefetch_limits_unacked() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "pf", FieldTable::default()).await;
    for i in 0..10 {
        publish(&ch, "", "pf", format!("{i}").as_bytes(), BasicProperties::default()).await;
    }
    ch.basic_qos(3, BasicQosOptions::default()).await.unwrap();
    let mut c = ch.basic_consume(s("pf"), s("slow"), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    let mut held = vec![];
    for _ in 0..3 {
        held.push(tokio::time::timeout(Duration::from_secs(5), c.next()).await.unwrap().unwrap().unwrap());
    }
    // A fourth must not arrive while three are unacked.
    assert!(tokio::time::timeout(Duration::from_millis(200), c.next()).await.is_err());
    held.remove(0).ack(BasicAckOptions::default()).await.unwrap();
    let d = tokio::time::timeout(Duration::from_secs(5), c.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(d.data, b"3");
}

#[tokio::test]
async fn alternate_exchange_and_consistent_hash() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    ch.exchange_declare(s("ae"), ExchangeKind::Fanout, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    declare(&ch, "unrouted", FieldTable::default()).await;
    ch.queue_bind(s("unrouted"), s("ae"), s(""), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    ch.exchange_declare(s("main"), ExchangeKind::Direct, ExchangeDeclareOptions::default(), args(&[("alternate-exchange", lstr("ae"))])).await.unwrap();
    publish(&ch, "main", "nobody", b"to-ae", BasicProperties::default()).await;
    assert_eq!(wait_get(&ch, "unrouted").await.delivery.data, b"to-ae");

    ch.exchange_declare(s("hash"), ExchangeKind::Custom("x-consistent-hash".into()), ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    for q in ["h1", "h2", "h3"] {
        declare(&ch, q, FieldTable::default()).await;
        ch.queue_bind(s(q), s("hash"), s("10"), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    }
    for i in 0..300 {
        publish(&ch, "hash", &format!("order-{i}"), b"x", BasicProperties::default()).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let vh = h.broker.vhost("/").unwrap();
    let counts: Vec<u64> = ["h1", "h2", "h3"].iter().map(|q| vh.queue(q).unwrap().shared.messages()).collect();
    assert_eq!(counts.iter().sum::<u64>(), 300);
    assert!(counts.iter().all(|c| *c > 50), "{counts:?}");
}

#[tokio::test]
async fn delayed_message_exchange() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    ch.exchange_declare(
        s("later"),
        ExchangeKind::Custom("x-delayed-message".into()),
        ExchangeDeclareOptions { durable: true, ..Default::default() },
        args(&[("x-delayed-type", lstr("direct"))]),
    )
    .await
    .unwrap();
    declare(&ch, "delayed", FieldTable::default()).await;
    ch.queue_bind(s("delayed"), s("later"), s("k"), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    let mut hd = FieldTable::default();
    hd.insert("x-delay".into(), AMQPValue::LongInt(300));
    let t = std::time::Instant::now();
    publish(&ch, "later", "k", b"wait", persistent().with_headers(hd)).await;
    assert!(get(&ch, "delayed").await.is_none());
    let m = wait_get(&ch, "delayed").await;
    assert_eq!(m.delivery.data, b"wait");
    assert!(t.elapsed() >= Duration::from_millis(280));
}

#[tokio::test]
async fn single_active_consumer_fails_over() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    declare(&ch, "sac", args(&[("x-single-active-consumer", AMQPValue::Boolean(true))])).await;
    let mut a = ch.basic_consume(s("sac"), s("a"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let mut b = ch.basic_consume(s("sac"), s("b"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    for i in 0..5 {
        publish(&ch, "", "sac", format!("{i}").as_bytes(), BasicProperties::default()).await;
    }
    for _ in 0..5 {
        tokio::time::timeout(Duration::from_secs(5), a.next()).await.unwrap().unwrap().unwrap();
    }
    assert!(tokio::time::timeout(Duration::from_millis(200), b.next()).await.is_err(), "inactive consumer gets nothing");
    ch.basic_cancel(s("a"), BasicCancelOptions::default()).await.unwrap();
    publish(&ch, "", "sac", b"to-b", BasicProperties::default()).await;
    let d = tokio::time::timeout(Duration::from_secs(5), b.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(d.data, b"to-b");
}

#[tokio::test]
async fn direct_reply_to_rpc() {
    let h = Harness::start().await;
    let (_c, server) = h.channel().await;
    declare(&server, "rpc", FieldTable::default()).await;
    let mut requests = server.basic_consume(s("rpc"), s("srv"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let (_c2, client) = h.channel().await;
    let mut replies = client
        .basic_consume(s("amq.rabbitmq.reply-to"), s("cli"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default())
        .await
        .unwrap();
    publish(&client, "", "rpc", b"ping", BasicProperties::default().with_reply_to(s("amq.rabbitmq.reply-to"))).await;
    let req = tokio::time::timeout(Duration::from_secs(5), requests.next()).await.unwrap().unwrap().unwrap();
    let reply_to = req.properties.reply_to().clone().unwrap().to_string();
    assert!(reply_to.starts_with("amq.rabbitmq.reply-to."), "{reply_to}");
    publish(&server, "", &reply_to, b"pong", BasicProperties::default()).await;
    let rep = tokio::time::timeout(Duration::from_secs(5), replies.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(rep.data, b"pong");
}

#[tokio::test]
async fn auto_delete_and_expires() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    ch.queue_declare(s("ad"), QueueDeclareOptions { auto_delete: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    ch.basic_consume(s("ad"), s("t"), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    ch.basic_cancel(s("t"), BasicCancelOptions::default()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let vh = h.broker.vhost("/").unwrap();
    assert!(vh.queue("ad").is_none(), "auto-delete queue removed after its last consumer");
    declare(&ch, "exp", args(&[("x-expires", AMQPValue::LongInt(200))])).await;
    assert!(vh.queue("exp").is_some());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(vh.queue("exp").is_none(), "unused queue expired");
}

#[tokio::test]
async fn quorum_delivery_limit_dead_letters() {
    let h = Harness::start().await;
    let (_c, ch) = h.channel().await;
    ch.exchange_declare(s("poison"), ExchangeKind::Fanout, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    declare(&ch, "poisoned", FieldTable::default()).await;
    ch.queue_bind(s("poisoned"), s("poison"), s(""), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    declare(&ch, "qq", args(&[("x-queue-type", lstr("quorum")), ("x-delivery-limit", AMQPValue::LongInt(2)), ("x-dead-letter-exchange", lstr("poison"))])).await;
    publish(&ch, "", "qq", b"bad", BasicProperties::default()).await;
    for _ in 0..3 {
        let g = wait_get_ack(&ch, "qq").await;
        g.delivery.nack(BasicNackOptions { requeue: true, multiple: false }).await.unwrap();
    }
    let m = wait_get(&ch, "poisoned").await;
    assert_eq!(m.delivery.data, b"bad");
}

async fn wait_get_ack(ch: &Channel, queue: &str) -> lapin::message::BasicGetMessage {
    for _ in 0..200 {
        if let Some(m) = ch.basic_get(s(queue), BasicGetOptions { no_ack: false }).await.unwrap() {
            return m;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no message on {queue}");
}

/// A raw connection through the handshake (PLAIN guest/guest, frame_max 4096).
async fn raw_open(addr: &str) -> (tokio::net::TcpStream, bytes::BytesMut) {
    use mq_amqp::method::{Method, PROTOCOL_HEADER};
    use tokio::io::AsyncWriteExt;
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(PROTOCOL_HEADER).await.unwrap();
    let mut buf = bytes::BytesMut::new();
    let mut out = bytes::BytesMut::new();
    raw_method(&mut sock, &mut buf).await; // start
    let start_ok = Method::ConnectionStartOk {
        client_properties: mq_core::FieldTable::new(),
        mechanism: "PLAIN".into(),
        response: bytes::Bytes::from_static(b"\0guest\0guest"),
        locale: "en_US".into(),
    };
    start_ok.frame(0, &mut out);
    sock.write_all(&out.split()).await.unwrap();
    raw_method(&mut sock, &mut buf).await; // tune
    Method::ConnectionTuneOk { channel_max: 16, frame_max: 4096, heartbeat: 0 }.frame(0, &mut out);
    Method::ConnectionOpen { virtual_host: "/".into(), capabilities: String::new(), insist: false }.frame(0, &mut out);
    sock.write_all(&out.split()).await.unwrap();
    assert!(matches!(raw_method(&mut sock, &mut buf).await, Some(Method::ConnectionOpenOk { .. })));
    (sock, buf)
}

/// The next method frame, or None at EOF.
async fn raw_method(sock: &mut tokio::net::TcpStream, buf: &mut bytes::BytesMut) -> Option<mq_amqp::method::Method> {
    use tokio::io::AsyncReadExt;
    loop {
        if buf.len() >= 7 {
            let size = u32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]) as usize;
            if buf.len() >= size + 8 {
                let f = buf.split_to(size + 8);
                if f[0] == mq_amqp::method::FRAME_METHOD {
                    return Some(mq_amqp::method::Method::parse(f.freeze().slice(7..7 + size)).unwrap());
                }
                continue;
            }
        }
        if sock.read_buf(buf).await.unwrap_or(0) == 0 {
            return None;
        }
    }
}

#[tokio::test]
async fn oversized_frame_closes_connection_once() {
    use tokio::io::AsyncWriteExt;
    let h = Harness::start().await;
    let addr = h.url.trim_start_matches("amqp://guest:guest@").trim_end_matches("/%2f").to_string();
    let (mut sock, mut buf) = raw_open(&addr).await;
    // A frame header announcing 1 MiB on a connection negotiated at frame_max 4096.
    sock.write_all(&[1, 0, 1, 0, 0x10, 0, 0]).await.unwrap();
    let close = tokio::time::timeout(Duration::from_secs(5), raw_method(&mut sock, &mut buf)).await.unwrap();
    assert!(matches!(close, Some(mq_amqp::method::Method::ConnectionClose { reply_code: 501, .. })), "{close:?}");
    // Then the broker hangs up instead of spinning on the unreadable frame.
    let next = tokio::time::timeout(Duration::from_secs(5), raw_method(&mut sock, &mut buf)).await.unwrap();
    assert!(next.is_none(), "{next:?}");
}

/// Classic queues deliver before the enqueue is on disk; a message an auto-ack consumer took
/// in that window must still be settled in the log, or it would come back after a restart.
#[tokio::test]
async fn auto_acked_messages_stay_consumed_across_restarts() {
    for crash in [false, true] {
        let h = Harness::start().await;
        {
            let (c, ch) = h.channel().await;
            declare(&ch, "drained", FieldTable::default()).await;
            let mut cons = ch.basic_consume(s("drained"), s(""), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
            ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
            for i in 0..200 {
                publish(&ch, "", "drained", format!("m{i}").as_bytes(), persistent()).await;
            }
            for _ in 0..200 {
                tokio::time::timeout(Duration::from_secs(5), cons.next()).await.unwrap().unwrap().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            c.close(200, s("bye")).await.unwrap();
        }
        let h = if crash { h.crash_restart().await } else { h.restart().await };
        let (_c, ch) = h.channel().await;
        assert!(get(&ch, "drained").await.is_none(), "consumed messages came back (crash: {crash})");
    }
}

#[tokio::test]
async fn modified_annotations_survive_restart() {
    modified_annotations_survive(false).await;
}

#[tokio::test]
async fn modified_annotations_survive_a_crash() {
    modified_annotations_survive(true).await;
}

/// AMQP 1.0's `modified` outcome rewrites a held message; the change is logged, not just kept
/// in memory.
async fn modified_annotations_survive(crash: bool) {
    let h = Harness::start().await;
    {
        let (c, ch) = h.channel().await;
        ch.confirm_select(ConfirmSelectOptions::default()).await.unwrap();
        declare(&ch, "modq", args(&[("x-queue-type", lstr("quorum"))])).await;
        publish(&ch, "", "modq", b"m", persistent()).await;
        c.close(200, "bye".into()).await.unwrap();
    }
    let q = h.broker.vhost("/").unwrap().queue("modq").unwrap();
    let channel = h.broker.next_id();
    let d = q.get(channel, false).await.unwrap().unwrap();
    let mut annotations = mq_core::FieldTable::new();
    annotations.insert("x-opt-retried", mq_core::FieldValue::Bool(true));
    q.modify(channel, vec![d.seq], mq_core::queue::Modification { annotations, delivery_failed: true, undeliverable_here: false }).await;
    // Processed in order after the modify; then give its (relaxed) log append a moment.
    let peeked = q.peek(0, 1).await.unwrap();
    assert_eq!(peeked.len(), 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let h = if crash { h.crash_restart().await } else { h.restart().await };
    let (_c, ch) = h.channel().await;
    let got = ch.basic_get(s("modq"), BasicGetOptions { no_ack: true }).await.unwrap().expect("message lost");
    let headers = got.delivery.properties.headers().clone().unwrap_or_default();
    assert_eq!(headers.inner().get("x-opt-retried"), Some(&AMQPValue::Boolean(true)), "{headers:?}");
    assert_eq!(headers.inner().get("x-delivery-count"), Some(&AMQPValue::LongLongInt(1)), "{headers:?}");
}

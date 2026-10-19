//! AMQP 1.0 against an in-process broker with real clients: fe2o3-amqp for 1.0 and lapin
//! for 0-9-1 interop. The broker shares one port between both protocols, like RabbitMQ 4.

use fe2o3_amqp::link::receiver::CreditMode;
use fe2o3_amqp::types::messaging::{ApplicationProperties, Body, Message, Modified, Outcome, Properties};
use fe2o3_amqp::types::primitives::{OrderedMap, SimpleValue, Value};
use fe2o3_amqp::connection::ConnectionHandle;
use fe2o3_amqp::session::SessionHandle;
use fe2o3_amqp::{Connection, Delivery, Receiver, Sender, Session};
use futures::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable, LongString};
use lapin::{BasicProperties, ConnectionProperties};
use mq_core::log::MemStore;
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

struct Env {
    broker: Arc<Broker>,
    addr: SocketAddr,
}

impl Env {
    async fn start() -> Env {
        let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = mq_amqp::ServerConfig { listen: addr, amqp10: Some(mq_amqp10::handoff(mq_amqp10::Config::default())) };
        tokio::spawn(mq_amqp::serve_listener(broker.clone(), listener, cfg));
        Env { broker, addr }
    }

    async fn connect(&self) -> (ConnectionHandle<()>, SessionHandle<()>) {
        let url = format!("amqp://guest:guest@127.0.0.1:{}", self.addr.port());
        let mut c = Connection::open("test-container", url.as_str()).await.unwrap();
        let s = Session::begin(&mut c).await.unwrap();
        (c, s)
    }

    async fn lapin(&self) -> (lapin::Connection, lapin::Channel) {
        let url = format!("amqp://guest:guest@127.0.0.1:{}/%2F", self.addr.port());
        let c = lapin::Connection::connect(&url, ConnectionProperties::default()).await.unwrap();
        let ch = c.create_channel().await.unwrap();
        (c, ch)
    }

    async fn declare(&self, name: &str, args: FieldTable) {
        let (_c, ch) = self.lapin().await;
        ch.queue_declare(name.into(), QueueDeclareOptions { durable: true, ..Default::default() }, args).await.unwrap();
    }

    fn ready(&self, q: &str) -> u64 {
        self.broker.vhost("/").unwrap().queue(q).unwrap().shared.ready.load(std::sync::atomic::Ordering::Relaxed)
    }
}

async fn recv(r: &mut Receiver) -> Delivery<Body<Value>> {
    tokio::time::timeout(Duration::from_secs(5), r.recv::<Body<Value>>()).await.expect("delivery timed out").unwrap()
}

async fn no_delivery(r: &mut Receiver) {
    let got = tokio::time::timeout(Duration::from_millis(300), r.recv::<Body<Value>>()).await;
    assert!(got.is_err(), "unexpected delivery");
}

fn text(d: &Delivery<Body<Value>>) -> String {
    match d.body() {
        Body::Value(v) => match &v.0 {
            Value::String(s) => s.clone(),
            other => panic!("unexpected value {other:?}"),
        },
        Body::Data(b) => String::from_utf8(b.iter().flat_map(|d| d.0.to_vec()).collect()).unwrap(),
        other => panic!("unexpected body {other:?}"),
    }
}

async fn wait_ready(env: &Env, q: &str, n: u64) {
    for _ in 0..250 {
        if env.ready(q) == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("queue {q}: expected {n} ready, have {}", env.ready(q));
}

#[tokio::test]
async fn send_and_receive_on_a_queue() {
    let env = Env::start().await;
    env.declare("q1", FieldTable::default()).await;
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "tx", "/queues/q1").await.unwrap();
    for i in 0..3 {
        let o = tx.send(Message::builder().value(format!("m{i}")).build()).await.unwrap();
        assert!(o.is_accepted(), "{o:?}");
    }
    let mut rx = Receiver::attach(&mut s, "rx", "/queues/q1").await.unwrap();
    for i in 0..3 {
        let d = recv(&mut rx).await;
        assert_eq!(text(&d), format!("m{i}"));
        rx.accept(&d).await.unwrap();
    }
    tx.close().await.unwrap();
    rx.close().await.unwrap();
    s.end().await.unwrap();
    c.close().await.unwrap();
    wait_ready(&env, "q1", 0).await;
}

#[tokio::test]
async fn exchange_with_routing_key_and_unroutable_release() {
    let env = Env::start().await;
    env.declare("orders", FieldTable::default()).await;
    let (_l, ch) = env.lapin().await;
    ch.queue_bind("orders".into(), "amq.topic".into(), "order.*".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "tx", "/exchanges/amq.topic/order.created").await.unwrap();
    assert!(tx.send(Message::builder().value("hello").build()).await.unwrap().is_accepted());
    let mut miss = Sender::attach(&mut s, "miss", "/exchanges/amq.topic/nobody").await.unwrap();
    let o = miss.send(Message::builder().value("lost").build()).await.unwrap();
    assert!(matches!(o, Outcome::Released(_)), "unroutable should be released: {o:?}");

    // Anonymous terminus with a per-message `to`.
    let mut anon = Sender::builder().name("anon").target(fe2o3_amqp::types::messaging::Target::builder().build()).attach(&mut s).await.unwrap();
    let m = Message::builder().properties(Properties::builder().to("/exchanges/amq.topic/order.paid").build()).value("paid").build();
    assert!(anon.send(m).await.unwrap().is_accepted());
    let m = Message::builder().properties(Properties::builder().to("/queues/orders").build()).value("direct").build();
    assert!(anon.send(m).await.unwrap().is_accepted());
    let m = Message::builder().properties(Properties::builder().to("/exchanges/nope").build()).value("x").build();
    let o = anon.send(m).await.unwrap();
    assert!(matches!(o, Outcome::Rejected(_)), "missing exchange should reject: {o:?}");

    let mut rx = Receiver::attach(&mut s, "rx", "/queues/orders").await.unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        let d = recv(&mut rx).await;
        let ann = d.message().message_annotations.clone().unwrap();
        let rk = ann.0.iter().find(|(k, _)| format!("{k:?}").contains("x-routing-key")).map(|(_, v)| v.clone());
        got.push((text(&d), rk));
        rx.accept(&d).await.unwrap();
    }
    assert_eq!(got[0].0, "hello");
    assert_eq!(got[0].1, Some(Value::String("order.created".into())));
    assert_eq!(got[1].0, "paid");
    assert_eq!(got[2].0, "direct");

    // Attaching to a missing queue is refused with amqp:not-found.
    let err = Sender::attach(&mut s, "bad", "/queues/missing").await;
    assert!(err.is_err());
    c.close().await.ok();
}

#[tokio::test]
async fn dispositions() {
    let env = Env::start().await;
    let dlx = FieldTable::from(std::collections::BTreeMap::from([
        ("x-dead-letter-exchange".into(), AMQPValue::LongString(LongString::from(""))),
        ("x-dead-letter-routing-key".into(), AMQPValue::LongString(LongString::from("dlq"))),
    ]));
    env.declare("dlq", FieldTable::default()).await;
    env.declare("work", dlx).await;
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "tx", "/queues/work").await.unwrap();
    tx.send(Message::builder().value("a").build()).await.unwrap();
    let mut rx = Receiver::builder().name("rx").source("/queues/work").credit_mode(CreditMode::Manual).auto_accept(false).attach(&mut s).await.unwrap();
    rx.set_credit(1).await.unwrap();

    // released: back to the queue, first acquirer stays true (not counted as a failure).
    let d = recv(&mut rx).await;
    assert!(d.message().header.as_ref().is_none_or(|h| h.first_acquirer));
    rx.release(&d).await.unwrap();
    rx.set_credit(1).await.unwrap();
    let d = recv(&mut rx).await;
    assert_eq!(text(&d), "a");

    // modified(delivery-failed): requeued and redelivered.
    rx.modify(&d, Modified { delivery_failed: Some(true), undeliverable_here: None, message_annotations: None }).await.unwrap();
    rx.set_credit(1).await.unwrap();
    let d = recv(&mut rx).await;
    assert!(!d.message().header.as_ref().unwrap().first_acquirer, "redelivery must not be first acquirer");

    // rejected: dead-lettered.
    rx.reject(&d, None).await.unwrap();
    wait_ready(&env, "dlq", 1).await;
    wait_ready(&env, "work", 0).await;

    // modified(undeliverable-here): also dead-lettered.
    tx.send(Message::builder().value("b").build()).await.unwrap();
    rx.set_credit(1).await.unwrap();
    let d = recv(&mut rx).await;
    rx.modify(&d, Modified { delivery_failed: Some(true), undeliverable_here: Some(true), message_annotations: None }).await.unwrap();
    wait_ready(&env, "dlq", 2).await;

    // accepted: gone.
    tx.send(Message::builder().value("c").build()).await.unwrap();
    rx.set_credit(1).await.unwrap();
    let d = recv(&mut rx).await;
    rx.accept(&d).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let q = env.broker.vhost("/").unwrap().queue("work").unwrap();
    assert_eq!(q.shared.messages(), 0);

    // Unsettled deliveries go back to the queue when the link detaches.
    tx.send(Message::builder().value("d").build()).await.unwrap();
    rx.set_credit(1).await.unwrap();
    let _held = recv(&mut rx).await;
    rx.close().await.unwrap();
    wait_ready(&env, "work", 1).await;
    c.close().await.ok();
}

#[tokio::test]
async fn link_credit_is_honoured() {
    let env = Env::start().await;
    env.declare("credit", FieldTable::default()).await;
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "tx", "/queues/credit").await.unwrap();
    for i in 0..5 {
        tx.send(Message::builder().value(format!("{i}")).build()).await.unwrap();
    }
    let mut rx = Receiver::builder().name("rx").source("/queues/credit").credit_mode(CreditMode::Manual).attach(&mut s).await.unwrap();
    rx.set_credit(2).await.unwrap();
    for _ in 0..2 {
        let d = recv(&mut rx).await;
        rx.accept(&d).await.unwrap();
    }
    no_delivery(&mut rx).await;
    wait_ready(&env, "credit", 3).await;
    rx.set_credit(3).await.unwrap();
    for i in 2..5 {
        let d = recv(&mut rx).await;
        assert_eq!(text(&d), format!("{i}"));
        rx.accept(&d).await.unwrap();
    }
    // Drain with nothing to deliver consumes the credit; nothing arrives afterwards.
    rx.set_credit(10).await.unwrap();
    rx.drain().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(Message::builder().value("late").build()).await.unwrap();
    no_delivery(&mut rx).await;
    wait_ready(&env, "credit", 1).await;
    c.close().await.ok();
}

#[tokio::test]
async fn application_properties_interop_with_091() {
    let env = Env::start().await;
    env.declare("interop", FieldTable::default()).await;
    let (_l, ch) = env.lapin().await;
    let (mut c, mut s) = env.connect().await;

    // 1.0 → 0-9-1
    let mut tx = Sender::attach(&mut s, "tx", "/queues/interop").await.unwrap();
    let mut ap = OrderedMap::new();
    ap.insert("color".to_string(), SimpleValue::String("blue".into()));
    ap.insert("n".to_string(), SimpleValue::Int(7));
    let m = Message::builder()
        .properties(Properties::builder().message_id("id-1".to_string()).correlation_id("c-1".to_string()).content_type("text/plain").build())
        .application_properties(ApplicationProperties(ap))
        .data(fe2o3_amqp::types::primitives::Binary::from(b"raw-bytes".to_vec()))
        .build();
    assert!(tx.send(m).await.unwrap().is_accepted());
    let got = loop {
        if let Some(m) = ch.basic_get("interop".into(), BasicGetOptions { no_ack: true }).await.unwrap() {
            break m;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(got.delivery.data, b"raw-bytes");
    let p = &got.delivery.properties;
    assert_eq!(p.message_id().as_ref().map(|s| s.as_str()), Some("id-1"));
    assert_eq!(p.correlation_id().as_ref().map(|s| s.as_str()), Some("c-1"));
    assert_eq!(p.content_type().as_ref().map(|s| s.as_str()), Some("text/plain"));
    let h = p.headers().as_ref().unwrap();
    assert_eq!(h.inner().get("color"), Some(&AMQPValue::LongString(LongString::from("blue"))));
    assert_eq!(h.inner().get("n"), Some(&AMQPValue::LongInt(7)));

    // 0-9-1 → 1.0
    let headers = FieldTable::from(std::collections::BTreeMap::from([("tier".into(), AMQPValue::LongString(LongString::from("gold")))]));
    let props = BasicProperties::default().with_headers(headers).with_message_id("m-2".into()).with_delivery_mode(2).with_priority(3);
    ch.basic_publish("".into(), "interop".into(), BasicPublishOptions::default(), b"from-091", props).await.unwrap().await.unwrap();
    let mut rx = Receiver::attach(&mut s, "rx", "/queues/interop").await.unwrap();
    let d = recv(&mut rx).await;
    assert_eq!(text(&d), "from-091");
    let m = d.message();
    let h = m.header.as_ref().unwrap();
    assert!(h.durable);
    assert_eq!(h.priority.0, 3);
    let id = m.properties.as_ref().and_then(|p| p.message_id.clone());
    assert_eq!(format!("{id:?}"), format!("{:?}", Some(fe2o3_amqp::types::messaging::MessageId::String("m-2".into()))));
    let ap = m.application_properties.as_ref().unwrap();
    assert_eq!(ap.0.get("tier"), Some(&SimpleValue::String("gold".into())));
    rx.accept(&d).await.unwrap();
    rx.close().await.unwrap();

    // 0-9-1 consumer receives an AMQP 1.0 amqp-value message with type "amqp-1.0".
    tx.send(Message::builder().value(42i64).build()).await.unwrap();
    let mut consumer = ch.basic_consume("interop".into(), "c".into(), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let del = tokio::time::timeout(Duration::from_secs(5), consumer.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(del.properties.kind().as_ref().map(|s| s.as_str()), Some("amqp-1.0"));
    c.close().await.ok();
}

/// A management request on the `/management` link pair.
async fn mgmt(tx: &mut Sender, rx: &mut Receiver, id: u64, method: &str, path: &str, body: Value) -> (String, Value) {
    let m = Message::builder()
        .properties(Properties::builder().message_id(id).to(path).subject(method).reply_to("$me").build())
        .value(body)
        .build();
    tx.send(m).await.unwrap();
    let d: Delivery<Value> = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    let props = d.message().properties.clone().unwrap();
    assert_eq!(format!("{:?}", props.correlation_id), format!("{:?}", Some(fe2o3_amqp::types::messaging::MessageId::Ulong(id))));
    (props.subject.unwrap(), d.into_body())
}

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut m = OrderedMap::new();
    for (k, v) in pairs {
        m.insert(Value::String(k.to_string()), v.clone());
    }
    Value::Map(m)
}

#[tokio::test]
async fn management_node() {
    let env = Env::start().await;
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "mgmt-tx", "/management").await.unwrap();
    let mut rx = Receiver::attach(&mut s, "mgmt-rx", "/management").await.unwrap();

    let args = map(&[("x-queue-type", Value::String("quorum".into()))]);
    let (st, body) = mgmt(&mut tx, &mut rx, 1, "PUT", "/queues/orders", map(&[("durable", Value::Bool(true)), ("arguments", args)])).await;
    assert_eq!(st, "201", "{body:?}");
    let Value::Map(m) = &body else { panic!("{body:?}") };
    assert_eq!(m.get(&Value::String("type".into())), Some(&Value::String("quorum".into())));
    assert_eq!(m.get(&Value::String("name".into())), Some(&Value::String("orders".into())));
    let (st, _) = mgmt(&mut tx, &mut rx, 2, "PUT", "/queues/orders", map(&[("durable", Value::Bool(true))])).await;
    assert_eq!(st, "200", "redeclaring an existing queue");
    let classic = map(&[("x-queue-type", Value::String("classic".into()))]);
    let (st, _) = mgmt(&mut tx, &mut rx, 2, "PUT", "/queues/orders", map(&[("durable", Value::Bool(true)), ("arguments", classic)])).await;
    assert_eq!(st, "409", "redeclare with another queue type is a precondition failure");

    let (st, _) = mgmt(&mut tx, &mut rx, 3, "PUT", "/exchanges/ex1", map(&[("type", Value::String("direct".into()))])).await;
    assert_eq!(st, "204");
    let bind = map(&[
        ("source", Value::String("ex1".into())),
        ("destination_queue", Value::String("orders".into())),
        ("binding_key", Value::String("k".into())),
        ("arguments", Value::Map(OrderedMap::new())),
    ]);
    let (st, body) = mgmt(&mut tx, &mut rx, 4, "POST", "/bindings", bind).await;
    assert_eq!(st, "204", "{body:?}");

    let mut pubr = Sender::attach(&mut s, "pub", "/exchanges/ex1/k").await.unwrap();
    assert!(pubr.send(Message::builder().value("x").build()).await.unwrap().is_accepted());
    let (st, body) = mgmt(&mut tx, &mut rx, 5, "GET", "/queues/orders", Value::Null).await;
    assert_eq!(st, "200");
    let Value::Map(m) = &body else { panic!("{body:?}") };
    assert_eq!(m.get(&Value::String("message_count".into())), Some(&Value::Ulong(1)));

    let (st, body) = mgmt(&mut tx, &mut rx, 6, "GET", "/bindings?src=ex1&dstq=orders&key=k", Value::Null).await;
    assert_eq!(st, "200");
    let Value::List(l) = &body else { panic!("{body:?}") };
    assert_eq!(l.len(), 1);

    let (st, body) = mgmt(&mut tx, &mut rx, 7, "DELETE", "/queues/orders/messages", Value::Null).await;
    assert_eq!(st, "200");
    assert_eq!(body, map(&[("message_count", Value::Ulong(1))]));

    let (st, body) = mgmt(&mut tx, &mut rx, 8, "PUT", "/queues/amq.nope", map(&[])).await;
    assert_eq!(st, "403", "{body:?}");
    let (st, _) = mgmt(&mut tx, &mut rx, 9, "GET", "/queues/missing", Value::Null).await;
    assert_eq!(st, "404");

    let (st, _) = mgmt(&mut tx, &mut rx, 10, "DELETE", "/queues/orders", Value::Null).await;
    assert_eq!(st, "200");
    assert!(env.broker.vhost("/").unwrap().queue("orders").is_none());
    c.close().await.ok();
}

#[tokio::test]
async fn consumer_is_detached_when_its_queue_is_deleted() {
    let env = Env::start().await;
    env.declare("gone", FieldTable::default()).await;
    let (mut c, mut s) = env.connect().await;
    let mut rx = Receiver::attach(&mut s, "rx", "/queues/gone").await.unwrap();
    let (_l, ch) = env.lapin().await;
    ch.queue_delete("gone".into(), QueueDeleteOptions::default()).await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), rx.recv::<Body<Value>>()).await.unwrap();
    let e = format!("{:?}", err.err().unwrap());
    assert!(e.contains("resource-deleted") || e.contains("ResourceDeleted"), "{e}");
    c.close().await.ok();
}

#[tokio::test]
async fn bad_credentials_are_refused() {
    let env = Env::start().await;
    let url = format!("amqp://guest:wrong@127.0.0.1:{}", env.addr.port());
    assert!(Connection::open("bad", url.as_str()).await.is_err());
    // ANONYMOUS is not offered unless configured.
    let url = format!("amqp://127.0.0.1:{}", env.addr.port());
    assert!(Connection::open("anon", url.as_str()).await.is_err());
    let _ = &env.broker;
}

#[tokio::test]
async fn large_messages_span_frames() {
    let env = Env::start().await;
    env.declare("big", FieldTable::default()).await;
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "tx", "/queues/big").await.unwrap();
    let payload = vec![7u8; 600_000];
    let m = Message::builder().data(fe2o3_amqp::types::primitives::Binary::from(payload.clone())).build();
    assert!(tx.send(m).await.unwrap().is_accepted());
    let mut rx = Receiver::attach(&mut s, "rx", "/queues/big").await.unwrap();
    let d = recv(&mut rx).await;
    let Body::Data(b) = d.body() else { panic!("expected data") };
    let got: Vec<u8> = b.iter().flat_map(|d| d.0.to_vec()).collect();
    assert_eq!(got.len(), payload.len());
    rx.accept(&d).await.unwrap();
    c.close().await.ok();
}

fn filter(name: &str, value: Value) -> Value {
    use fe2o3_amqp::types::primitives::Symbol;
    use serde_amqp::described::Described;
    use serde_amqp::descriptor::Descriptor;
    Value::Described(Box::new(Described { descriptor: Descriptor::Name(Symbol::from(name)), value }))
}

#[tokio::test]
async fn stream_offset_and_property_filters() {
    let env = Env::start().await;
    let args = FieldTable::from(std::collections::BTreeMap::from([("x-queue-type".into(), AMQPValue::LongString(LongString::from("stream")))]));
    env.declare("events", args).await;
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "tx", "/queues/events").await.unwrap();
    for (i, color) in ["red", "blue", "red"].iter().enumerate() {
        let mut ap = OrderedMap::new();
        ap.insert("color".to_string(), SimpleValue::String(color.to_string()));
        let m = Message::builder().application_properties(ApplicationProperties(ap)).value(format!("e{i}")).build();
        assert!(tx.send(m).await.unwrap().is_accepted());
    }

    // Offset 1 skips the first event.
    let mut fs = fe2o3_amqp::types::messaging::FilterSet::new();
    fs.insert("rabbitmq:stream-offset-spec".into(), filter("rabbitmq:stream-offset-spec", Value::Ulong(1)));
    let src = fe2o3_amqp::types::messaging::Source::builder().address("/queues/events").filter(fs).build();
    let mut rx = Receiver::builder().name("from-1").source(src).attach(&mut s).await.unwrap();
    for i in 1..3 {
        let d = recv(&mut rx).await;
        assert_eq!(text(&d), format!("e{i}"));
        rx.accept(&d).await.unwrap();
    }
    rx.close().await.unwrap();

    // From the start, only red events.
    let mut fs = fe2o3_amqp::types::messaging::FilterSet::new();
    fs.insert("rabbitmq:stream-offset-spec".into(), filter("rabbitmq:stream-offset-spec", Value::String("first".into())));
    let mut red = OrderedMap::new();
    red.insert(Value::String("color".into()), Value::String("red".into()));
    fs.insert("props".into(), filter("amqp:application-properties-filter", Value::Map(red)));
    let src = fe2o3_amqp::types::messaging::Source::builder().address("/queues/events").filter(fs).build();
    let mut rx = Receiver::builder().name("red").source(src).attach(&mut s).await.unwrap();
    for want in ["e0", "e2"] {
        let d = recv(&mut rx).await;
        assert_eq!(text(&d), want);
        rx.accept(&d).await.unwrap();
    }
    no_delivery(&mut rx).await;
    c.close().await.ok();
}

#[tokio::test]
async fn single_active_consumer_and_priority() {
    let env = Env::start().await;
    let args = FieldTable::from(std::collections::BTreeMap::from([("x-single-active-consumer".into(), AMQPValue::Boolean(true))]));
    env.declare("sac", args).await;
    let (mut c, mut s) = env.connect().await;
    let mut a = Receiver::attach(&mut s, "a", "/queues/sac").await.unwrap();
    let mut b = Receiver::attach(&mut s, "b", "/queues/sac").await.unwrap();
    let mut tx = Sender::attach(&mut s, "tx", "/queues/sac").await.unwrap();
    for i in 0..3 {
        tx.send(Message::builder().value(format!("{i}")).build()).await.unwrap();
    }
    for i in 0..3 {
        let d = recv(&mut a).await;
        assert_eq!(text(&d), format!("{i}"));
        a.accept(&d).await.unwrap();
    }
    no_delivery(&mut b).await;
    // The standby takes over once the active consumer goes.
    a.close().await.unwrap();
    tx.send(Message::builder().value("next").build()).await.unwrap();
    let d = recv(&mut b).await;
    assert_eq!(text(&d), "next");
    b.accept(&d).await.unwrap();

    // rabbitmq:priority link property: the higher priority consumer is served first.
    env.declare("prio", FieldTable::default()).await;
    let mut props = OrderedMap::new();
    props.insert(fe2o3_amqp::types::primitives::Symbol::from("rabbitmq:priority"), Value::Int(10));
    let mut hi = Receiver::builder().name("hi").source("/queues/prio").properties(props).attach(&mut s).await.unwrap();
    let mut lo = Receiver::attach(&mut s, "lo", "/queues/prio").await.unwrap();
    let mut tp = Sender::attach(&mut s, "tp", "/queues/prio").await.unwrap();
    for i in 0..3 {
        tp.send(Message::builder().value(format!("{i}")).build()).await.unwrap();
    }
    for _ in 0..3 {
        let d = recv(&mut hi).await;
        hi.accept(&d).await.unwrap();
    }
    no_delivery(&mut lo).await;
    c.close().await.ok();
}

#[tokio::test]
async fn modified_outcome_merges_annotations() {
    use fe2o3_amqp::types::primitives::Symbol;
    let env = Env::start().await;
    let dlx = FieldTable::from(std::collections::BTreeMap::from([
        ("x-dead-letter-exchange".into(), AMQPValue::LongString(LongString::from(""))),
        ("x-dead-letter-routing-key".into(), AMQPValue::LongString(LongString::from("mod-dlq"))),
    ]));
    env.declare("mod-dlq", FieldTable::default()).await;
    env.declare("mod", dlx).await;
    let (mut c, mut s) = env.connect().await;
    let mut tx = Sender::attach(&mut s, "tx", "/queues/mod").await.unwrap();
    tx.send(Message::builder().value("a").build()).await.unwrap();
    let mut rx = Receiver::builder().name("rx").source("/queues/mod").credit_mode(CreditMode::Manual).auto_accept(false).attach(&mut s).await.unwrap();
    rx.set_credit(1).await.unwrap();
    let d = recv(&mut rx).await;

    // Only x-opt-* keys may be changed (RabbitMQ 4).
    let mut anns = OrderedMap::new();
    anns.insert(Symbol::from("x-opt-retried"), Value::Bool(true));
    anns.insert(Symbol::from("not-allowed"), Value::Bool(true));
    rx.modify(&d, Modified { delivery_failed: Some(true), undeliverable_here: None, message_annotations: Some(anns) }).await.unwrap();
    rx.set_credit(1).await.unwrap();
    let d = recv(&mut rx).await;
    let got = d.message().message_annotations.clone().unwrap_or_default();
    let get = |k: &str| got.0.iter().find(|(key, _)| format!("{key:?}").contains(k)).map(|(_, v)| v.clone());
    assert_eq!(get("x-opt-retried"), Some(Value::Bool(true)), "{got:?}");
    assert_eq!(get("not-allowed"), None, "{got:?}");

    // Dead-lettered with its new annotations; 0-9-1 sees them as headers.
    let mut anns = OrderedMap::new();
    anns.insert(Symbol::from("x-opt-reason"), Value::String("bad input".into()));
    rx.modify(&d, Modified { delivery_failed: Some(true), undeliverable_here: Some(true), message_annotations: Some(anns) }).await.unwrap();
    wait_ready(&env, "mod-dlq", 1).await;
    let (_l, ch) = env.lapin().await;
    let m = ch.basic_get("mod-dlq".into(), BasicGetOptions { no_ack: true }).await.unwrap().unwrap();
    let headers = m.delivery.properties.headers().clone().unwrap_or_default();
    let h = |k: &str| headers.inner().get(k).cloned();
    assert_eq!(h("x-opt-reason"), Some(AMQPValue::LongString(LongString::from("bad input"))));
    assert_eq!(h("x-opt-retried"), Some(AMQPValue::Boolean(true)));
    c.close().await.ok();
}

#[tokio::test]
async fn refused_link_leaves_the_connection_usable() {
    let env = Env::start().await;
    env.declare("there", FieldTable::default()).await;
    let (mut c, mut s) = env.connect().await;
    // attach + detach(not-found); the client's answering detach is not echoed back.
    assert!(Sender::attach(&mut s, "missing", "/queues/not-there").await.is_err());
    let mut s2 = Session::begin(&mut c).await.unwrap();
    let mut tx = Sender::attach(&mut s2, "ok", "/queues/there").await.unwrap();
    tx.send(Message::builder().value("x").build()).await.unwrap();
    let mut rx = Receiver::attach(&mut s2, "rx", "/queues/there").await.unwrap();
    assert_eq!(text(&recv(&mut rx).await), "x");
    c.close().await.unwrap();
}

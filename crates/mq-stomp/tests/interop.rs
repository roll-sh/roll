//! STOMP ↔ AMQP 0-9-1 interop through an unmodified AMQP client (lapin), and STOMP over
//! WebSocket.

mod common;

use common::{Client, Harness, wait_until};
use futures::{SinkExt, StreamExt};
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable, LongString, ShortString};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn s(v: &str) -> ShortString {
    ShortString::from(v)
}

async fn channel(h: &Harness) -> (Connection, Channel) {
    let c = Connection::connect(&h.amqp_url(), ConnectionProperties::default()).await.unwrap();
    let ch = c.create_channel().await.unwrap();
    (c, ch)
}

async fn next(consumer: &mut lapin::Consumer) -> lapin::message::Delivery {
    tokio::time::timeout(Duration::from_secs(5), consumer.next()).await.expect("delivery").unwrap().unwrap()
}

#[tokio::test]
async fn stomp_to_amqp() {
    let h = Harness::start().await;
    let (_c, ch) = channel(&h).await;
    ch.queue_declare(s("from-stomp"), QueueDeclareOptions { durable: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let mut consumer = ch.basic_consume(s("from-stomp"), s("amqp"), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();

    let mut st = h.client().await;
    st.sync(
        "SEND",
        &[
            ("destination", "/queue/from-stomp"),
            ("content-type", "text/plain"),
            ("persistent", "true"),
            ("correlation-id", "c-9"),
            ("reply-to", "/queue/back"),
            ("x-custom", "v"),
        ],
        b"hello amqp",
    )
    .await;
    let d = next(&mut consumer).await;
    assert_eq!(d.data, b"hello amqp");
    assert_eq!(d.routing_key.as_str(), "from-stomp");
    let p = &d.properties;
    assert_eq!(p.content_type().as_ref().map(|v| v.as_str()), Some("text/plain"));
    assert_eq!(*p.delivery_mode(), Some(2));
    assert_eq!(p.correlation_id().as_ref().map(|v| v.as_str()), Some("c-9"));
    assert_eq!(p.reply_to().as_ref().map(|v| v.as_str()), Some("/queue/back"));
    let headers = p.headers().clone().unwrap();
    assert_eq!(headers.inner().get("x-custom"), Some(&AMQPValue::LongString(LongString::from("v"))));
    d.ack(BasicAckOptions::default()).await.unwrap();

    // Topics land on amq.topic with the destination's routing key.
    let q = ch.queue_declare(s(""), QueueDeclareOptions { exclusive: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    ch.queue_bind(q.name().clone(), s("amq.topic"), s("orders.#"), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    let mut tc = ch.basic_consume(q.name().clone(), s("t"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    st.sync("SEND", &[("destination", "/topic/orders.eu.new")], b"order").await;
    let d = next(&mut tc).await;
    assert_eq!((d.exchange.as_str(), d.routing_key.as_str(), d.data.as_slice()), ("amq.topic", "orders.eu.new", b"order".as_slice()));
}

#[tokio::test]
async fn amqp_to_stomp() {
    let h = Harness::start().await;
    let mut st = h.client().await;
    st.sync("SUBSCRIBE", &[("destination", "/queue/to-stomp"), ("id", "s"), ("ack", "client-individual")], b"").await;
    st.sync("SUBSCRIBE", &[("destination", "/topic/events.*"), ("id", "t")], b"").await;

    let (_c, ch) = channel(&h).await;
    let mut headers = FieldTable::default();
    headers.insert(s("x-str"), AMQPValue::LongString(LongString::from("s")));
    headers.insert(s("x-int"), AMQPValue::LongInt(7));
    headers.insert(s("x-bool"), AMQPValue::Boolean(true));
    let props = BasicProperties::default()
        .with_content_type(s("application/json"))
        .with_delivery_mode(2)
        .with_priority(4)
        .with_correlation_id(s("corr"))
        .with_reply_to(s("replies"))
        .with_message_id(s("mid"))
        .with_type(s("evt"))
        .with_app_id(s("app"))
        .with_timestamp(1_700_000_000)
        .with_expiration(s("60000"))
        .with_headers(headers);
    ch.basic_publish(s(""), s("to-stomp"), BasicPublishOptions::default(), b"{\"a\":1}", props).await.unwrap().await.unwrap();
    let m = st.expect("MESSAGE").await;
    assert_eq!(m.body, b"{\"a\":1}");
    assert_eq!(m.hh("destination"), "/queue/to-stomp");
    for (k, v) in [
        ("content-type", "application/json"),
        ("persistent", "true"),
        ("priority", "4"),
        ("correlation-id", "corr"),
        ("reply-to", "/reply-queue/replies"),
        ("amqp-message-id", "mid"),
        ("type", "evt"),
        ("app-id", "app"),
        ("timestamp", "1700000000"),
        ("expiration", "60000"),
        ("x-str", "s"),
        ("x-int", "7"),
        ("x-bool", "true"),
    ] {
        assert_eq!(m.h(k), Some(v), "{k}: {:?}", m.headers);
    }
    st.sync("ACK", &[("id", &Client::ack_id(&m))], b"").await;
    wait_until(|| h.queue_len("to-stomp") == Some(0)).await;

    ch.basic_publish(s("amq.topic"), s("events.created"), BasicPublishOptions::default(), b"evt", BasicProperties::default()).await.unwrap().await.unwrap();
    let m = st.expect("MESSAGE").await;
    assert_eq!((m.text().as_str(), m.hh("destination"), m.hh("subscription")), ("evt", "/topic/events.created", "t"));
    ch.basic_publish(s("amq.direct"), s("k"), BasicPublishOptions::default(), b"x", BasicProperties::default()).await.unwrap().await.unwrap();
    assert!(st.try_recv(200).await.is_none());
}

#[tokio::test]
async fn amqp_direct_reply_to_answered_over_stomp() {
    let h = Harness::start().await;
    let mut server = h.client().await;
    server.sync("SUBSCRIBE", &[("destination", "/queue/rpc"), ("id", "srv")], b"").await;
    let (_c, ch) = channel(&h).await;
    let mut replies = ch
        .basic_consume(s("amq.rabbitmq.reply-to"), s("cli"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default())
        .await
        .unwrap();
    let props = BasicProperties::default().with_reply_to(s("amq.rabbitmq.reply-to")).with_correlation_id(s("42"));
    ch.basic_publish(s(""), s("rpc"), BasicPublishOptions::default(), b"ping", props).await.unwrap().await.unwrap();
    let req = server.expect("MESSAGE").await;
    let reply_to = req.hh("reply-to").to_string();
    assert!(reply_to.starts_with("/reply-queue/amq.rabbitmq.reply-to."), "{reply_to}");
    server.send("SEND", &[("destination", &reply_to), ("correlation-id", req.hh("correlation-id"))], b"pong").await;
    let d = next(&mut replies).await;
    assert_eq!(d.data, b"pong");
    assert_eq!(d.properties.correlation_id().as_ref().map(|v| v.as_str()), Some("42"));
}

#[tokio::test]
async fn stomp_temp_queue_answered_over_amqp() {
    let h = Harness::start().await;
    let (_c, ch) = channel(&h).await;
    ch.queue_declare(s("svc"), QueueDeclareOptions { durable: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let mut consumer = ch.basic_consume(s("svc"), s("svc"), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let mut st = h.client().await;
    st.send("SEND", &[("destination", "/queue/svc"), ("reply-to", "/temp-queue/r")], b"q").await;
    let d = next(&mut consumer).await;
    let reply_to = d.properties.reply_to().clone().unwrap().to_string();
    assert!(reply_to.starts_with("amq.gen-"), "{reply_to}");
    ch.basic_publish(s(""), s(&reply_to), BasicPublishOptions::default(), b"a", BasicProperties::default()).await.unwrap().await.unwrap();
    let m = st.expect("MESSAGE").await;
    assert_eq!((m.text().as_str(), m.hh("subscription")), ("a", "/temp-queue/r"));
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Next STOMP frame (without its NUL) from a WebSocket that may pack several per message.
async fn ws_recv(ws: &mut Ws, frames: &mut Vec<String>) -> String {
    loop {
        if let Some(f) = frames.pop() {
            return f;
        }
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
        if let Message::Text(t) = msg {
            frames.extend(t.split('\0').map(|f| f.trim_start_matches('\n').to_string()).filter(|f| !f.is_empty()).rev());
        }
    }
}

#[tokio::test]
async fn web_stomp() {
    let h = Harness::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mq_stomp::serve_ws_listener(h.broker.clone(), listener, mq_stomp::Config::default()));

    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("Sec-WebSocket-Protocol", "v10.stomp, v11.stomp, v12.stomp".parse().unwrap());
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(resp.headers().get("sec-websocket-protocol").unwrap(), "v12.stomp");

    let mut frames = Vec::new();
    ws.send(Message::text("CONNECT\naccept-version:1.2\nhost:/\nlogin:guest\npasscode:guest\n\n\0")).await.unwrap();
    assert!(ws_recv(&mut ws, &mut frames).await.starts_with("CONNECTED\n"));
    // Two frames in one WebSocket message.
    ws.send(Message::text("SUBSCRIBE\ndestination:/queue/ws\nid:0\n\n\0SEND\ndestination:/queue/ws\nreceipt:r\n\nover ws\0")).await.unwrap();
    let mut got = vec![ws_recv(&mut ws, &mut frames).await, ws_recv(&mut ws, &mut frames).await];
    got.sort();
    assert!(got[0].starts_with("MESSAGE\n") && got[0].ends_with("\n\nover ws"), "{got:?}");
    assert!(got[1].starts_with("RECEIPT\nreceipt-id:r"), "{got:?}");
    wait_until(|| h.broker.connections().iter().any(|c| c.protocol == "STOMP 1.2")).await;

    // Wrong path.
    assert!(tokio_tungstenite::connect_async(format!("ws://{addr}/nope")).await.is_err());
}

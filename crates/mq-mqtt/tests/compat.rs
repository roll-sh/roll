//! MQTT compatibility tests against an in-process broker, driven by unmodified client libraries
//! (rumqttc for MQTT 3.1.1 and 5.0, lapin for AMQP interop) plus a packet-level client for the
//! edges libraries hide.

#![allow(clippy::result_large_err)] // rumqttc's ConnectionError

mod common;

use bytes::Bytes;
use common::{Harness, Raw, connect_packet};
use mq_core::BrokerConfig;
use mq_core::auth::{Permission, TopicPermission};
use mq_core::vhost::VHostInfo;
use mq_mqtt::packet::{self, Packet, Props, Publish, SubOpts, rc};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(5);

// ── MQTT 3.1.1 client (rumqttc) ──

mod v4 {
    use super::*;
    pub use rumqttc::{AsyncClient, ConnectReturnCode, ConnectionError, Event, LastWill, MqttOptions, Packet, QoS};

    pub struct C {
        pub client: AsyncClient,
        pub rx: mpsc::UnboundedReceiver<Packet>,
        task: tokio::task::JoinHandle<()>,
        pub session_present: bool,
    }

    pub async fn connect(h: &Harness, id: &str, f: impl FnOnce(&mut MqttOptions)) -> Result<C, ConnectionError> {
        let mut o = MqttOptions::new(id, "127.0.0.1", h.port());
        o.set_credentials("guest", "guest");
        o.set_keep_alive(Duration::from_secs(30));
        f(&mut o);
        let (client, mut el) = AsyncClient::new(o, 100);
        let session_present = loop {
            if let Event::Incoming(Packet::ConnAck(a)) = timeout(T, el.poll()).await.expect("connect timed out")? {
                break a.session_present;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                match el.poll().await {
                    Ok(Event::Incoming(p)) => {
                        if tx.send(p).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(C { client, rx, task, session_present })
    }

    impl C {
        pub async fn next(&mut self) -> Option<Packet> {
            timeout(T, self.rx.recv()).await.ok().flatten()
        }

        pub async fn publish(&mut self) -> rumqttc::Publish {
            loop {
                match self.next().await {
                    Some(Packet::Publish(p)) => return p,
                    Some(_) => {}
                    None => panic!("no PUBLISH received"),
                }
            }
        }

        pub async fn sub(&mut self, filter: &str, qos: QoS) -> rumqttc::SubAck {
            self.client.subscribe(filter, qos).await.unwrap();
            loop {
                match self.next().await {
                    Some(Packet::SubAck(a)) => return a,
                    Some(_) => {}
                    None => panic!("no SUBACK"),
                }
            }
        }

        pub async fn pub_ack(&mut self, topic: &str, qos: QoS, retain: bool, payload: &[u8]) {
            self.client.publish(topic, qos, retain, payload.to_vec()).await.unwrap();
            if qos == QoS::AtLeastOnce {
                loop {
                    match self.next().await {
                        Some(Packet::PubAck(_)) => return,
                        Some(_) => {}
                        None => panic!("no PUBACK"),
                    }
                }
            }
        }

        pub async fn quiet(&mut self, ms: u64) -> bool {
            loop {
                match timeout(Duration::from_millis(ms), self.rx.recv()).await {
                    Err(_) => return true,
                    Ok(Some(Packet::Publish(_))) => return false,
                    Ok(Some(_)) => {}
                    Ok(None) => return true,
                }
            }
        }

        /// Drop the TCP connection without DISCONNECT.
        pub async fn kill(self) {
            self.task.abort();
            let _ = self.task.await;
        }

        pub async fn disconnect(mut self) {
            self.client.disconnect().await.unwrap();
            let _ = timeout(T, async { while self.rx.recv().await.is_some() {} }).await;
            self.task.abort();
        }

        /// Wait until the server closes the connection.
        pub async fn closed(&mut self) -> bool {
            timeout(T, async { while self.rx.recv().await.is_some() {} }).await.is_ok()
        }
    }
}

// ── MQTT 5.0 client (rumqttc::v5) ──

mod v5 {
    use super::*;
    pub use rumqttc::v5::mqttbytes::QoS;
    pub use rumqttc::v5::mqttbytes::v5::{Filter, LastWill, LastWillProperties, Packet, PublishProperties, RetainForwardRule, SubscribeProperties};
    pub use rumqttc::v5::{AsyncClient, ConnectionError, Event, MqttOptions, StateError};
    use rumqttc::v5::mqttbytes::v5::Disconnect;

    pub struct C {
        pub client: AsyncClient,
        pub rx: mpsc::UnboundedReceiver<Packet>,
        task: tokio::task::JoinHandle<()>,
        pub connack: rumqttc::v5::mqttbytes::v5::ConnAck,
    }

    pub async fn connect(h: &Harness, id: &str, f: impl FnOnce(&mut MqttOptions)) -> Result<C, ConnectionError> {
        let mut o = MqttOptions::new(id, "127.0.0.1", h.port());
        o.set_credentials("guest", "guest");
        o.set_keep_alive(Duration::from_secs(30));
        f(&mut o);
        let (client, mut el) = AsyncClient::new(o, 100);
        let connack = loop {
            if let Event::Incoming(Packet::ConnAck(a)) = timeout(T, el.poll()).await.expect("connect timed out")? {
                break a;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                match el.poll().await {
                    Ok(Event::Incoming(p)) => {
                        if tx.send(p).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    // rumqttc surfaces a server DISCONNECT as an error.
                    Err(ConnectionError::MqttState(StateError::ServerDisconnect { reason_code, .. })) => {
                        let _ = tx.send(Packet::Disconnect(Disconnect { reason_code, properties: None }));
                        break;
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(C { client, rx, task, connack })
    }

    impl C {
        pub async fn next(&mut self) -> Option<Packet> {
            timeout(T, self.rx.recv()).await.ok().flatten()
        }

        pub async fn publish(&mut self) -> rumqttc::v5::mqttbytes::v5::Publish {
            loop {
                match self.next().await {
                    Some(Packet::Publish(p)) => return p,
                    Some(_) => {}
                    None => panic!("no PUBLISH received"),
                }
            }
        }

        pub async fn suback(&mut self) -> rumqttc::v5::mqttbytes::v5::SubAck {
            loop {
                match self.next().await {
                    Some(Packet::SubAck(a)) => return a,
                    Some(_) => {}
                    None => panic!("no SUBACK"),
                }
            }
        }

        pub async fn quiet(&mut self, ms: u64) -> bool {
            loop {
                match timeout(Duration::from_millis(ms), self.rx.recv()).await {
                    Err(_) => return true,
                    Ok(Some(Packet::Publish(_))) => return false,
                    Ok(Some(_)) => {}
                    Ok(None) => return true,
                }
            }
        }

        pub async fn kill(self) {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

fn s(b: &Bytes) -> &str {
    std::str::from_utf8(b).unwrap()
}

async fn wait_for(mut f: impl FnMut() -> bool) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached");
}

// ── tests ──

#[tokio::test]
async fn pubsub_qos0_and_qos1() {
    let h = Harness::start().await;
    let mut sub = v4::connect(&h, "sub", |_| {}).await.unwrap();
    assert!(!sub.session_present);
    let ack = sub.sub("t/a", v4::QoS::AtLeastOnce).await;
    assert_eq!(ack.return_codes, vec![rumqttc::SubscribeReasonCode::Success(v4::QoS::AtLeastOnce)]);
    let mut publ = v4::connect(&h, "pub", |_| {}).await.unwrap();
    publ.pub_ack("t/a", v4::QoS::AtLeastOnce, false, b"one").await;
    publ.pub_ack("t/a", v4::QoS::AtMostOnce, false, b"zero").await;
    let m = sub.publish().await;
    assert_eq!((s(&m.payload), m.qos, &m.topic[..]), ("one", v4::QoS::AtLeastOnce, "t/a"));
    let m = sub.publish().await;
    assert_eq!((s(&m.payload), m.qos), ("zero", v4::QoS::AtMostOnce));
    // QoS 2 subscriptions are granted QoS 1, like RabbitMQ.
    let ack = sub.sub("t/b", v4::QoS::ExactlyOnce).await;
    assert_eq!(ack.return_codes, vec![rumqttc::SubscribeReasonCode::Success(v4::QoS::AtLeastOnce)]);
    // Many QoS 1 messages flow through the prefetch window.
    for i in 0..200 {
        publ.client.publish("t/a", v4::QoS::AtLeastOnce, false, format!("m{i}")).await.unwrap();
    }
    for i in 0..200 {
        assert_eq!(s(&sub.publish().await.payload), format!("m{i}"));
    }
    let c = h.broker.connections();
    assert!(c.iter().all(|c| c.protocol == "MQTT 3.1.1"), "{:?}", c.iter().map(|c| &c.protocol).collect::<Vec<_>>());
    assert_eq!(c.len(), 2);
}

#[tokio::test]
async fn wildcards() {
    let h = Harness::start().await;
    let mut a = v4::connect(&h, "wa", |_| {}).await.unwrap();
    a.sub("sensors/+/temp", v4::QoS::AtMostOnce).await;
    let mut b = v4::connect(&h, "wb", |_| {}).await.unwrap();
    b.sub("sensors/#", v4::QoS::AtMostOnce).await;
    let mut p = v4::connect(&h, "wp", |_| {}).await.unwrap();
    p.pub_ack("sensors/k1/temp", v4::QoS::AtLeastOnce, false, b"21").await;
    p.pub_ack("sensors/k1/hum", v4::QoS::AtLeastOnce, false, b"40").await;
    assert_eq!(&a.publish().await.topic[..], "sensors/k1/temp");
    assert!(a.quiet(300).await);
    assert_eq!(&b.publish().await.topic[..], "sensors/k1/temp");
    assert_eq!(&b.publish().await.topic[..], "sensors/k1/hum");
    // A topic exchange binding with the RabbitMQ mapping exists.
    let x = h.broker.vhost("/").unwrap().exchange("amq.topic").unwrap();
    assert!(x.bindings().iter().any(|b| b.routing_key == "sensors.*.temp" && b.dest.name == "mqtt-subscription-waqos0"));
}

#[tokio::test]
async fn retained_messages() {
    let h = Harness::start().await;
    let mut p = v4::connect(&h, "rp", |_| {}).await.unwrap();
    p.pub_ack("r/1", v4::QoS::AtLeastOnce, true, b"hello").await;
    p.pub_ack("r/2", v4::QoS::AtMostOnce, true, b"zero").await;
    let mut s1 = v4::connect(&h, "rs1", |_| {}).await.unwrap();
    s1.sub("r/1", v4::QoS::AtLeastOnce).await;
    let m = s1.publish().await;
    assert!(m.retain);
    assert_eq!((s(&m.payload), m.qos), ("hello", v4::QoS::AtLeastOnce));
    let mut s2 = v4::connect(&h, "rs2", |_| {}).await.unwrap();
    s2.sub("r/#", v4::QoS::AtLeastOnce).await;
    let mut got = vec![s(&s2.publish().await.payload).to_string(), s(&s2.publish().await.payload).to_string()];
    got.sort();
    assert_eq!(got, vec!["hello", "zero"]);
    // Live messages are not flagged retained for v3 subscribers.
    p.pub_ack("r/1", v4::QoS::AtLeastOnce, true, b"again").await;
    let m = s1.publish().await;
    assert!(!m.retain);
    // An empty retained message clears it.
    p.pub_ack("r/1", v4::QoS::AtLeastOnce, true, b"").await;
    let _ = s1.publish().await;
    let mut s3 = v4::connect(&h, "rs3", |_| {}).await.unwrap();
    s3.sub("r/1", v4::QoS::AtLeastOnce).await;
    assert!(s3.quiet(300).await);
    // Retained messages are stored durably and survive a restart.
    for c in [p, s1, s2, s3] {
        c.disconnect().await;
    }
    let h = h.restart().await;
    let mut s4 = v4::connect(&h, "rs4", |_| {}).await.unwrap();
    s4.sub("r/2", v4::QoS::AtLeastOnce).await;
    let m = s4.publish().await;
    assert_eq!((s(&m.payload), m.retain, m.qos), ("zero", true, v4::QoS::AtMostOnce));
}

#[tokio::test]
async fn will_messages() {
    let h = Harness::start().await;
    let mut w = v4::connect(&h, "watcher", |_| {}).await.unwrap();
    w.sub("wills/#", v4::QoS::AtLeastOnce).await;
    let a = v4::connect(&h, "dies", |o| {
        o.set_last_will(v4::LastWill::new("wills/dies", "gone", v4::QoS::AtLeastOnce, false));
    })
    .await
    .unwrap();
    a.kill().await;
    let m = w.publish().await;
    assert_eq!((&m.topic[..], s(&m.payload)), ("wills/dies", "gone"));
    // A clean DISCONNECT discards the will.
    let b = v4::connect(&h, "leaves", |o| {
        o.set_last_will(v4::LastWill::new("wills/leaves", "gone", v4::QoS::AtLeastOnce, false));
    })
    .await
    .unwrap();
    b.disconnect().await;
    assert!(w.quiet(500).await);
}

#[tokio::test]
async fn keepalive_timeout_publishes_will() {
    let h = Harness::start().await;
    let mut w = v4::connect(&h, "kw", |_| {}).await.unwrap();
    w.sub("ka/#", v4::QoS::AtMostOnce).await;
    let mut c = connect_packet(packet::V311, "sleepy", true);
    c.keep_alive = 1;
    c.will = Some(packet::Will { topic: "ka/sleepy".into(), payload: Bytes::from_static(b"timeout"), qos: 0, retain: false, props: Props::default() });
    let (mut raw, ack) = Raw::connect(h.addr, c).await;
    assert!(matches!(ack, Some(Packet::ConnAck { code: 0, .. })));
    let t = std::time::Instant::now();
    assert_eq!(raw.recv().await, None, "server closes the silent connection");
    assert!(t.elapsed() >= Duration::from_millis(1400), "{:?}", t.elapsed());
    assert_eq!(s(&w.publish().await.payload), "timeout");
}

#[tokio::test]
async fn persistent_session_redelivery() {
    let h = Harness::start().await;
    let mut c = v4::connect(&h, "durable", |o| {
        o.set_clean_session(false);
    })
    .await
    .unwrap();
    c.sub("p/t", v4::QoS::AtLeastOnce).await;
    c.disconnect().await;
    let q = h.broker.vhost("/").unwrap().queue("mqtt-subscription-durableqos1").expect("session queue");
    assert!(q.meta.durable && q.meta.exclusive_owner.is_none());
    assert_eq!(q.meta.arguments.get_i64("x-expires"), Some(86_400_000));

    let mut p = v4::connect(&h, "producer", |_| {}).await.unwrap();
    for i in 0..3 {
        p.pub_ack("p/t", v4::QoS::AtLeastOnce, false, format!("m{i}").as_bytes()).await;
    }
    // Reconnect without acking: messages come back flagged DUP on the next connection.
    let mut c = v4::connect(&h, "durable", |o| {
        o.set_clean_session(false);
        o.set_manual_acks(true);
    })
    .await
    .unwrap();
    assert!(c.session_present);
    for i in 0..3 {
        let m = c.publish().await;
        assert_eq!(s(&m.payload), format!("m{i}"));
        assert!(!m.dup);
    }
    c.kill().await;
    let mut c = v4::connect(&h, "durable", |o| {
        o.set_clean_session(false);
    })
    .await
    .unwrap();
    assert!(c.session_present);
    for i in 0..3 {
        let m = c.publish().await;
        assert_eq!(s(&m.payload), format!("m{i}"));
        assert!(m.dup, "redelivery is flagged DUP");
    }
    // Acked now: nothing comes back.
    c.disconnect().await;
    let mut c = v4::connect(&h, "durable", |o| {
        o.set_clean_session(false);
    })
    .await
    .unwrap();
    assert!(c.quiet(400).await);
    // A clean session discards it.
    c.disconnect().await;
    let c = v4::connect(&h, "durable", |_| {}).await.unwrap();
    assert!(!c.session_present);
    assert!(h.broker.vhost("/").unwrap().queue("mqtt-subscription-durableqos1").is_none());
    c.disconnect().await;
}

#[tokio::test]
async fn persistent_session_survives_restart() {
    let h = Harness::start().await;
    let mut c = v4::connect(&h, "survivor", |o| {
        o.set_clean_session(false);
    })
    .await
    .unwrap();
    c.sub("rs/#", v4::QoS::AtLeastOnce).await;
    c.disconnect().await;
    let mut p = v4::connect(&h, "rs-pub", |_| {}).await.unwrap();
    p.pub_ack("rs/x", v4::QoS::AtLeastOnce, false, b"kept").await;
    p.disconnect().await;
    let h = h.restart().await;
    let mut c = v4::connect(&h, "survivor", |o| {
        o.set_clean_session(false);
    })
    .await
    .unwrap();
    assert!(c.session_present);
    assert_eq!(s(&c.publish().await.payload), "kept");
    // The subscription is still active after the restart.
    let mut p = v4::connect(&h, "rs-pub", |_| {}).await.unwrap();
    p.pub_ack("rs/y", v4::QoS::AtLeastOnce, false, b"live").await;
    assert_eq!(s(&c.publish().await.payload), "live");
}

#[tokio::test]
async fn clean_session_cleanup() {
    let h = Harness::start().await;
    let mut c = v4::connect(&h, "cleaner", |_| {}).await.unwrap();
    c.sub("c/0", v4::QoS::AtMostOnce).await;
    c.sub("c/1", v4::QoS::AtLeastOnce).await;
    let vh = h.broker.vhost("/").unwrap();
    for n in ["mqtt-subscription-cleanerqos0", "mqtt-subscription-cleanerqos1"] {
        assert!(vh.queue(n).unwrap().meta.exclusive_owner.is_some(), "{n} is exclusive");
    }
    c.disconnect().await;
    wait_for(|| vh.queue("mqtt-subscription-cleanerqos0").is_none() && vh.queue("mqtt-subscription-cleanerqos1").is_none()).await;
    wait_for(|| h.broker.connections().is_empty()).await;
}

#[tokio::test]
async fn auth_failures() {
    let h = Harness::start().await;
    let r = v4::connect(&h, "bad", |o| {
        o.set_credentials("guest", "wrong");
    })
    .await;
    assert!(matches!(r, Err(v4::ConnectionError::ConnectionRefused(v4::ConnectReturnCode::BadUserNamePassword))), "{:?}", r.err());
    // Anonymous logins are refused unless enabled.
    let (_, ack) = Raw::connect(h.addr, mq_mqtt::packet::Connect { username: None, password: None, ..connect_packet(packet::V311, "anon", true) }).await;
    assert_eq!(ack, Some(Packet::ConnAck { session_present: false, code: 4, props: Props::default() }));
    // Unknown vhost.
    let r = v4::connect(&h, "novh", |o| {
        o.set_credentials("nope:guest", "guest");
    })
    .await;
    assert!(matches!(r, Err(v4::ConnectionError::ConnectionRefused(v4::ConnectReturnCode::BadUserNamePassword))));
    // v5 reason codes.
    let r = v5::connect(&h, "bad5", |o| {
        o.set_credentials("guest", "wrong");
    })
    .await;
    assert!(matches!(r, Err(v5::ConnectionError::ConnectionRefused(rumqttc::v5::mqttbytes::v5::ConnectReturnCode::BadUserNamePassword))), "{:?}", r.err());
    // Empty client id with a persistent v3.1.1 session.
    let (_, ack) = Raw::connect(h.addr, connect_packet(packet::V311, "", false)).await;
    assert!(matches!(ack, Some(Packet::ConnAck { code: 2, .. })));

    let mut cfg = mq_mqtt::Config { allow_anonymous: true, ..Default::default() };
    cfg.default_vhost = Some("/".into());
    let h2 = Harness::start_with(cfg, BrokerConfig::default(), "127.0.0.1:0").await;
    let (_, ack) = Raw::connect(h2.addr, mq_mqtt::packet::Connect { username: None, password: None, ..connect_packet(packet::V311, "anon", true) }).await;
    assert!(matches!(ack, Some(Packet::ConnAck { code: 0, .. })));
    wait_for(|| h2.broker.connections().iter().any(|c| c.user == "guest")).await;
}

#[tokio::test]
async fn vhost_in_username_and_permissions() {
    let h = Harness::start().await;
    h.broker.create_vhost(VHostInfo { name: "iot".into(), ..Default::default() }).await.unwrap();
    h.broker.set_permission("iot", "guest", Permission { configure: ".*".into(), write: ".*".into(), read: ".*".into() }).await.unwrap();
    h.broker
        .set_topic_permission("iot", "guest", TopicPermission { exchange: "amq.topic".into(), write: "^dev\\.{client_id}\\.".into(), read: ".*".into() })
        .await
        .unwrap();
    let mut sub = v4::connect(&h, "watch", |o| {
        o.set_credentials("iot:guest", "guest");
    })
    .await
    .unwrap();
    sub.sub("dev/#", v4::QoS::AtLeastOnce).await;
    assert!(h.broker.connections().iter().all(|c| c.vhost == "iot"));
    let mut d = v4::connect(&h, "d1", |o| {
        o.set_credentials("iot:guest", "guest");
    })
    .await
    .unwrap();
    d.pub_ack("dev/d1/temp", v4::QoS::AtLeastOnce, false, b"ok").await;
    assert_eq!(s(&sub.publish().await.payload), "ok");
    // Publishing outside the allowed topic closes the connection (disconnect_on_unauthorized).
    d.client.publish("dev/d2/temp", v4::QoS::AtLeastOnce, false, "no").await.unwrap();
    assert!(d.closed().await);
    assert!(sub.quiet(300).await);
}

#[tokio::test]
async fn v5_properties_roundtrip() {
    let h = Harness::start().await;
    let mut sub = v5::connect(&h, "v5sub", |_| {}).await.unwrap();
    let ca = sub.connack.properties.clone().unwrap();
    assert_eq!(ca.max_qos, Some(1));
    assert_eq!(ca.shared_subscription_available, Some(0));
    let props = v5::SubscribeProperties { id: Some(42), user_properties: vec![] };
    sub.client.subscribe_with_properties("v5/+", v5::QoS::AtLeastOnce, props).await.unwrap();
    sub.suback().await;
    let p = v5::connect(&h, "v5pub", |_| {}).await.unwrap();
    let pp = v5::PublishProperties {
        payload_format_indicator: Some(1),
        message_expiry_interval: Some(60),
        user_properties: vec![("color".into(), "blue".into()), ("size".into(), "L".into())],
        content_type: Some("text/plain".into()),
        response_topic: Some("replies/v5".into()),
        correlation_data: Some(Bytes::from_static(b"corr-1")),
        ..Default::default()
    };
    p.client.publish_with_properties("v5/a", v5::QoS::AtLeastOnce, false, "payload", pp).await.unwrap();
    let m = sub.publish().await;
    assert_eq!(&m.topic[..], b"v5/a");
    let props = m.properties.unwrap();
    assert_eq!(props.user_properties, vec![("color".to_string(), "blue".to_string()), ("size".to_string(), "L".to_string())]);
    assert_eq!(props.content_type.as_deref(), Some("text/plain"));
    assert_eq!(props.response_topic.as_deref(), Some("replies/v5"));
    assert_eq!(props.correlation_data.as_deref(), Some(&b"corr-1"[..]));
    assert_eq!(props.payload_format_indicator, Some(1));
    assert!(props.message_expiry_interval.is_some_and(|e| (58..=60).contains(&e)));
    assert_eq!(props.subscription_identifiers, vec![42]);
    assert!(h.broker.connections().iter().all(|c| c.protocol == "MQTT 5.0"));
}

#[tokio::test]
async fn v5_subscription_options_and_will() {
    let h = Harness::start().await;
    let p = v5::connect(&h, "opts-pub", |_| {}).await.unwrap();
    p.client.publish("so/kept", v5::QoS::AtLeastOnce, true, "retained").await.unwrap();
    let mut c = v5::connect(&h, "opts", |_| {}).await.unwrap();
    let filters = vec![
        v5::Filter { path: "so/own".into(), qos: v5::QoS::AtLeastOnce, nolocal: true, preserve_retain: false, retain_forward_rule: v5::RetainForwardRule::OnEverySubscribe },
        v5::Filter { path: "so/kept".into(), qos: v5::QoS::AtLeastOnce, nolocal: false, preserve_retain: true, retain_forward_rule: v5::RetainForwardRule::Never },
    ];
    // The retained message may still be in flight; wait for it before subscribing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.client.subscribe_many(filters).await.unwrap();
    let ack = c.suback().await;
    assert_eq!(ack.return_codes.len(), 2);
    // Retain Handling 2: no retained message on subscribe.
    assert!(c.quiet(300).await);
    c.client.publish("so/own", v5::QoS::AtLeastOnce, false, "mine").await.unwrap();
    assert!(c.quiet(300).await, "No Local suppresses our own publish");
    p.client.publish("so/own", v5::QoS::AtLeastOnce, false, "theirs").await.unwrap();
    assert_eq!(&c.publish().await.payload[..], b"theirs");

    let will = v5::LastWill::new("so/will", "bye", v5::QoS::AtLeastOnce, false, Some(v5::LastWillProperties {
            delay_interval: None,
            payload_format_indicator: None,
            message_expiry_interval: None,
            content_type: Some("text/plain".into()),
            response_topic: None,
            correlation_data: None,
            user_properties: vec![("why".into(), "crash".into())],
        }));
    let mut w = v5::connect(&h, "opts-watch", |_| {}).await.unwrap();
    w.client.subscribe("so/will", v5::QoS::AtLeastOnce).await.unwrap();
    w.suback().await;
    let dying = v5::connect(&h, "opts-dying", |o| {
        o.set_last_will(will);
    })
    .await
    .unwrap();
    dying.kill().await;
    let m = w.publish().await;
    assert_eq!(&m.payload[..], b"bye");
    let props = m.properties.unwrap();
    assert_eq!(props.content_type.as_deref(), Some("text/plain"));
    assert_eq!(props.user_properties, vec![("why".to_string(), "crash".to_string())]);
}

#[tokio::test]
async fn v5_unroutable_puback_and_shared_subscriptions() {
    let h = Harness::start().await;
    let c = mq_mqtt::packet::Connect { props: Props { topic_alias_maximum: Some(5), ..Default::default() }, ..connect_packet(packet::V5, "raw5", true) };
    let (mut raw, ack) = Raw::connect(h.addr, c).await;
    let Some(Packet::ConnAck { code: 0, props, .. }) = ack else { panic!("{ack:?}") };
    assert_eq!(props.topic_alias_maximum, Some(16));
    // No matching subscribers → reason code 0x10.
    raw.send(&Packet::Publish(Publish { dup: false, qos: 1, retain: false, topic: "nobody/home".into(), packet_id: 1, props: Props::default(), payload: Bytes::new() })).await;
    assert_eq!(raw.recv().await, Some(Packet::PubAck { id: 1, code: rc::NO_MATCHING_SUBSCRIBERS, props: Props::default() }));
    // Shared subscriptions are not supported; invalid filters are refused.
    let o = SubOpts { qos: 1, ..Default::default() };
    raw.send(&Packet::Subscribe { id: 2, props: Props::default(), filters: vec![("$share/g/a".into(), o), ("a/#/b".into(), o), ("ok/+".into(), o)] }).await;
    assert_eq!(
        raw.recv().await,
        Some(Packet::SubAck { id: 2, props: Props::default(), codes: vec![rc::SHARED_SUBSCRIPTIONS_NOT_SUPPORTED, rc::TOPIC_FILTER_INVALID, 1] })
    );
    // No Local: our own publish doesn't come back; Retain As Published keeps RETAIN.
    let nl = SubOpts { qos: 1, no_local: true, ..Default::default() };
    let rap = SubOpts { qos: 0, retain_as_published: true, ..Default::default() };
    raw.send(&Packet::Subscribe { id: 3, props: Props::default(), filters: vec![("nl/t".into(), nl), ("rap/t".into(), rap)] }).await;
    assert!(matches!(raw.recv().await, Some(Packet::SubAck { id: 3, .. })));
    raw.send(&Packet::Publish(Publish { dup: false, qos: 1, retain: false, topic: "nl/t".into(), packet_id: 4, props: Props::default(), payload: Bytes::from_static(b"x") })).await;
    assert!(matches!(raw.recv().await, Some(Packet::PubAck { id: 4, code: 0, .. })));
    raw.send(&Packet::Publish(Publish { dup: false, qos: 0, retain: true, topic: "rap/t".into(), packet_id: 0, props: Props::default(), payload: Bytes::from_static(b"r") })).await;
    let Some(Packet::Publish(p)) = raw.recv().await else { panic!() };
    assert_eq!((&p.topic[..], p.retain, &p.payload[..]), ("rap/t", true, &b"r"[..]));
    assert_eq!(p.props.topic_alias, Some(1), "server assigns topic aliases");
    // A second message on the same topic only carries the alias.
    raw.send(&Packet::Publish(Publish { dup: false, qos: 0, retain: false, topic: "rap/t".into(), packet_id: 0, props: Props::default(), payload: Bytes::from_static(b"s") })).await;
    let Some(Packet::Publish(p)) = raw.recv().await else { panic!() };
    assert_eq!((&p.topic[..], p.retain, p.props.topic_alias), ("", false, Some(1)));
    // Inbound topic alias.
    raw.send(&Packet::Publish(Publish {
        dup: false,
        qos: 0,
        retain: false,
        topic: "rap/t".into(),
        packet_id: 0,
        props: Props { topic_alias: Some(2), ..Default::default() },
        payload: Bytes::from_static(b"a1"),
    }))
    .await;
    raw.send(&Packet::Publish(Publish {
        dup: false,
        qos: 0,
        retain: false,
        topic: String::new(),
        packet_id: 0,
        props: Props { topic_alias: Some(2), ..Default::default() },
        payload: Bytes::from_static(b"a2"),
    }))
    .await;
    for want in ["a1", "a2"] {
        let Some(Packet::Publish(p)) = raw.recv().await else { panic!() };
        assert_eq!(&p.payload[..], want.as_bytes());
    }
    // QoS 2 publishes are refused in v5.
    raw.send(&Packet::Publish(Publish { dup: false, qos: 2, retain: false, topic: "q2".into(), packet_id: 9, props: Props::default(), payload: Bytes::new() })).await;
    assert_eq!(raw.recv().await, Some(Packet::Disconnect { code: rc::QOS_NOT_SUPPORTED, props: Props::default() }));
    assert_eq!(raw.recv().await, None);
}

#[tokio::test]
async fn v5_session_expiry_and_assigned_client_id() {
    let h = Harness::start().await;
    let c = mq_mqtt::packet::Connect { props: Props { session_expiry: Some(120), ..Default::default() }, ..connect_packet(packet::V5, "", true) };
    let (mut raw, ack) = Raw::connect(h.addr, c).await;
    let Some(Packet::ConnAck { code: 0, props, .. }) = ack else { panic!("{ack:?}") };
    let id = props.assigned_client_id.expect("assigned client id");
    assert_eq!(props.session_expiry, Some(120));
    raw.send(&Packet::Subscribe { id: 1, props: Props::default(), filters: vec![("se/t".into(), SubOpts { qos: 1, ..Default::default() })] }).await;
    assert!(matches!(raw.recv().await, Some(Packet::SubAck { .. })));
    let q = h.broker.vhost("/").unwrap().queue(&mq_mqtt::queue_name(&id, 1)).unwrap();
    assert_eq!(q.meta.arguments.get_i64("x-expires"), Some(120_000));
    // DISCONNECT may shorten the expiry to 0: the session ends with the connection.
    raw.send(&Packet::Disconnect { code: 0, props: Props { session_expiry: Some(0), ..Default::default() } }).await;
    let vh = h.broker.vhost("/").unwrap();
    wait_for(|| vh.queue(&mq_mqtt::queue_name(&id, 1)).is_none()).await;
}

#[tokio::test]
async fn v5_delayed_will() {
    let h = Harness::start().await;
    let mut w = v5::connect(&h, "dw-watch", |_| {}).await.unwrap();
    w.client.subscribe("dw/#", v5::QoS::AtLeastOnce).await.unwrap();
    w.suback().await;
    let c = mq_mqtt::packet::Connect {
        props: Props { session_expiry: Some(60), ..Default::default() },
        will: Some(packet::Will {
            topic: "dw/x".into(),
            payload: Bytes::from_static(b"late"),
            qos: 1,
            retain: false,
            props: Props { will_delay: Some(1), ..Default::default() },
        }),
        ..connect_packet(packet::V5, "dw", true)
    };
    let (raw, _) = Raw::connect(h.addr, c).await;
    drop(raw);
    let t = std::time::Instant::now();
    let m = w.publish().await;
    assert_eq!(&m.payload[..], b"late");
    assert!(t.elapsed() >= Duration::from_millis(800), "will was delayed ({:?})", t.elapsed());
}

#[tokio::test]
async fn client_id_takeover() {
    let h = Harness::start().await;
    let mut first = v5::connect(&h, "same", |_| {}).await.unwrap();
    let _second = v5::connect(&h, "same", |_| {}).await.unwrap();
    loop {
        match first.next().await {
            Some(v5::Packet::Disconnect(d)) => {
                assert_eq!(d.reason_code, rumqttc::v5::mqttbytes::v5::DisconnectReasonCode::SessionTakenOver);
                break;
            }
            Some(_) => {}
            None => panic!("first connection not disconnected"),
        }
    }
    wait_for(|| h.broker.connections().len() == 1).await;
}

#[tokio::test]
async fn force_close_from_management() {
    let h = Harness::start().await;
    let mut c = v4::connect(&h, "victim", |_| {}).await.unwrap();
    wait_for(|| h.broker.connections().len() == 1).await;
    h.broker.connections()[0].request_close("closed via management plugin");
    assert!(c.closed().await);
    wait_for(|| h.broker.connections().is_empty()).await;
}

#[tokio::test]
async fn packet_too_large() {
    let bc = BrokerConfig { max_message_size: 1024, ..Default::default() };
    let h = Harness::start_with(mq_mqtt::Config::default(), bc, "127.0.0.1:0").await;
    let (mut raw, _) = Raw::connect(h.addr, connect_packet(packet::V5, "big", true)).await;
    raw.send(&Packet::Publish(Publish { dup: false, qos: 0, retain: false, topic: "big".into(), packet_id: 0, props: Props::default(), payload: Bytes::from(vec![0u8; 2048]) })).await;
    assert_eq!(raw.recv().await, Some(Packet::Disconnect { code: rc::PACKET_TOO_LARGE, props: Props::default() }));
    // Over the unauthenticated limit before CONNACK.
    let c = mq_mqtt::packet::Connect { password: Some(Bytes::from(vec![b'x'; 65_000])), ..connect_packet(packet::V311, "huge", true) };
    let mut c2 = c.clone();
    c2.username = Some("x".repeat(1000));
    let (_, ack) = Raw::connect(h.addr, mq_mqtt::packet::Connect { will: Some(packet::Will { topic: "a".into(), payload: Bytes::from(vec![0; 2000]), qos: 0, retain: false, props: Props::default() }), ..c }).await;
    assert_eq!(ack, None);
}

#[tokio::test]
async fn interop_with_amqp() {
    use futures_util::StreamExt;
    use lapin::options::*;
    use lapin::types::FieldTable;
    use lapin::{BasicProperties, Connection, ConnectionProperties};

    let h = Harness::start().await;
    let amqp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let amqp_addr = amqp_listener.local_addr().unwrap();
    let b = h.broker.clone();
    tokio::spawn(async move {
        let _ = mq_amqp::serve_listener(b, amqp_listener, mq_amqp::ServerConfig { listen: amqp_addr, amqp10: None }).await;
    });
    let conn = Connection::connect(&format!("amqp://guest:guest@{amqp_addr}/%2f"), ConnectionProperties::default()).await.unwrap();
    let ch = conn.create_channel().await.unwrap();

    // AMQP → MQTT.
    let mut m = v5::connect(&h, "interop", |_| {}).await.unwrap();
    m.client.subscribe("i/+", v5::QoS::AtLeastOnce).await.unwrap();
    m.suback().await;
    let mut headers = FieldTable::default();
    headers.insert("origin".into(), lapin::types::AMQPValue::LongString("amqp".into()));
    ch.basic_publish(
        "amq.topic".into(),
        "i.a".into(),
        BasicPublishOptions::default(),
        b"from-amqp",
        BasicProperties::default().with_delivery_mode(2).with_headers(headers).with_content_type("text/plain".into()),
    )
    .await
    .unwrap()
    .await
    .unwrap();
    let p = m.publish().await;
    assert_eq!((&p.topic[..], &p.payload[..]), (&b"i/a"[..], &b"from-amqp"[..]));
    assert_eq!(p.qos, v5::QoS::AtLeastOnce);
    let props = p.properties.unwrap();
    assert_eq!(props.user_properties, vec![("origin".to_string(), "amqp".to_string())]);
    assert_eq!(props.content_type.as_deref(), Some("text/plain"));

    // MQTT → AMQP.
    let q = ch.queue_declare("".into(), QueueDeclareOptions { exclusive: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    ch.queue_bind(q.name().clone(), "amq.topic".into(), "i.*".into(), QueueBindOptions::default(), FieldTable::default()).await.unwrap();
    let mut consumer = ch.basic_consume(q.name().clone(), "c".into(), BasicConsumeOptions { no_ack: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    let pp = v5::PublishProperties { user_properties: vec![("k".into(), "v".into())], ..Default::default() };
    m.client.publish_with_properties("i/b", v5::QoS::AtLeastOnce, false, "from-mqtt", pp).await.unwrap();
    let d = timeout(T, consumer.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(d.routing_key.as_str(), "i.b");
    assert_eq!(d.exchange.as_str(), "amq.topic");
    assert_eq!(&d.data[..], b"from-mqtt");
    assert_eq!(*d.properties.delivery_mode(), Some(2));
    let hdrs = d.properties.headers().clone().unwrap();
    assert!(hdrs.inner().iter().any(|(k, v)| k.as_str() == "k" && matches!(v, lapin::types::AMQPValue::LongString(s) if s.to_string() == "v")));
    let _ = m.publish().await; // the MQTT subscriber sees its own message too
}

#[tokio::test]
async fn websocket_transport() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let h = Harness::start().await;
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws_listener.local_addr().unwrap();
    let b = h.broker.clone();
    tokio::spawn(async move {
        let _ = mq_mqtt::serve_ws_listener(b, ws_listener, mq_mqtt::Config::default()).await;
    });
    let mut req = format!("ws://{ws_addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("sec-websocket-protocol", "mqtt".parse().unwrap());
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(resp.headers().get("sec-websocket-protocol").unwrap(), "mqtt");
    let send = |p: Packet| Message::Binary(p.to_bytes(packet::V311));
    ws.send(send(Packet::Connect(Box::new(connect_packet(packet::V311, "wsc", true))))).await.unwrap();
    let mut buf = bytes::BytesMut::new();
    let mut next = async || loop {
        if let Some(p) = packet::decode(&mut buf, packet::V311, u32::MAX).unwrap() {
            return p;
        }
        if let Message::Binary(b) = timeout(T, ws.next()).await.unwrap().unwrap().unwrap() {
            buf.extend_from_slice(&b);
        }
    };
    assert!(matches!(next().await, Packet::ConnAck { code: 0, .. }));
    drop(next);
    ws.send(send(Packet::Subscribe { id: 1, props: Props::default(), filters: vec![("ws/t".into(), SubOpts { qos: 0, ..Default::default() })] })).await.unwrap();
    ws.send(send(Packet::Publish(Publish { dup: false, qos: 0, retain: false, topic: "ws/t".into(), packet_id: 0, props: Props::default(), payload: Bytes::from_static(b"over ws") })))
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut buf = bytes::BytesMut::new();
    while got.len() < 2 {
        if let Some(p) = packet::decode(&mut buf, packet::V311, u32::MAX).unwrap() {
            got.push(p);
            continue;
        }
        if let Message::Binary(b) = timeout(T, ws.next()).await.unwrap().unwrap().unwrap() {
            buf.extend_from_slice(&b);
        }
    }
    assert!(matches!(got[0], Packet::SubAck { .. }));
    assert!(matches!(&got[1], Packet::Publish(p) if &p.payload[..] == b"over ws"));
}

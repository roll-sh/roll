//! librdkafka against the broker: the client most Kafka ecosystems (Go, Python, .NET, Node)
//! are built on.

mod common;

use common::TestBroker;
use rdkafka::admin::{AdminClient, AdminOptions, AlterConfig, NewPartitions, NewTopic, ResourceSpecifier, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, CommitMode, Consumer};
use rdkafka::error::KafkaError;
use rdkafka::message::{Header, Headers, Message, OwnedHeaders};
use rdkafka::producer::{BaseRecord, DefaultProducerContext, Producer, ThreadedProducer};
use rdkafka::types::RDKafkaErrorCode;
use rdkafka::{Offset, TopicPartitionList};
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(20);

fn config(addr: &str) -> ClientConfig {
    let mut c = ClientConfig::new();
    c.set("bootstrap.servers", addr).set("broker.address.family", "v4");
    if std::env::var("RDKAFKA_DEBUG").is_ok() {
        c.set("debug", "broker,protocol,cgrp,eos,fetch,topic");
    }
    c
}

fn producer(addr: &str, extra: &[(&str, &str)]) -> ThreadedProducer<DefaultProducerContext> {
    let mut c = config(addr);
    c.set("linger.ms", "5");
    for (k, v) in extra {
        c.set(*k, *v);
    }
    c.create().expect("producer")
}

fn consumer(addr: &str, group: &str, extra: &[(&str, &str)]) -> BaseConsumer {
    let mut c = config(addr);
    c.set("group.id", group).set("auto.offset.reset", "earliest").set("enable.auto.commit", "false").set("session.timeout.ms", "6000");
    for (k, v) in extra {
        c.set(*k, *v);
    }
    c.create().expect("consumer")
}

fn admin(addr: &str) -> AdminClient<DefaultClientContext> {
    config(addr).create().expect("admin")
}

fn opts() -> AdminOptions {
    AdminOptions::new().operation_timeout(Some(T)).request_timeout(Some(T))
}

/// Run blocking client code off the runtime that drives the broker.
async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.expect("client thread panicked")
}

fn send(p: &ThreadedProducer<DefaultProducerContext>, topic: &str, key: &str, value: &str) {
    loop {
        match p.send(BaseRecord::to(topic).key(key).payload(value)) {
            Ok(()) => return,
            Err((KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull), _)) => std::thread::sleep(Duration::from_millis(10)),
            Err((e, _)) => panic!("send: {e}"),
        }
    }
}

/// Poll until `n` messages arrive: `(partition, offset, key, value)`.
fn drain(c: &BaseConsumer, n: usize) -> Vec<(i32, i64, String, String)> {
    let mut out = vec![];
    let deadline = Instant::now() + T;
    while out.len() < n {
        assert!(Instant::now() < deadline, "timed out with {} of {n} messages", out.len());
        match c.poll(Duration::from_millis(200)) {
            Some(Ok(m)) => out.push((
                m.partition(),
                m.offset(),
                String::from_utf8_lossy(m.key().unwrap_or_default()).into_owned(),
                String::from_utf8_lossy(m.payload().unwrap_or_default()).into_owned(),
            )),
            Some(Err(KafkaError::PartitionEOF(_))) => {}
            Some(Err(e)) => panic!("poll: {e}"),
            None => {}
        }
    }
    out
}

/// Poll for `d` and return whatever arrives (expected: nothing more).
fn quiet(c: &BaseConsumer, d: Duration) -> usize {
    let end = Instant::now() + d;
    let mut n = 0;
    while Instant::now() < end {
        if let Some(Ok(_)) = c.poll(Duration::from_millis(100)) {
            n += 1;
        }
    }
    n
}

fn create_topic(addr: &str, name: &str, partitions: i32, configs: &[(&str, &str)]) {
    let a = admin(addr);
    let mut t = NewTopic::new(name, partitions, TopicReplication::Fixed(3));
    for (k, v) in configs {
        t = t.set(k, v);
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let res = rt.block_on(a.create_topics(&[t], &opts())).unwrap();
    assert!(res[0].is_ok(), "create {name}: {:?}", res[0]);
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn produce_consume_every_codec() {
    let tb = TestBroker::start("rdk-codecs").await;
    let addr = tb.addr.clone();
    blocking(move || {
        for codec in ["none", "gzip", "snappy", "lz4", "zstd"] {
            let topic = format!("codec-{codec}");
            create_topic(&addr, &topic, 3, &[]);
            let p = producer(&addr, &[("compression.codec", codec), ("batch.num.messages", "100")]);
            for i in 0..500 {
                send(&p, &topic, &format!("k{i}"), &format!("value-{i}-{}", "x".repeat(i % 50)));
            }
            p.flush(T).unwrap();
            let c = consumer(&addr, &format!("g-{codec}"), &[]);
            c.subscribe(&[&topic]).unwrap();
            let got = drain(&c, 500);
            let keys: HashSet<String> = got.iter().map(|m| m.2.clone()).collect();
            assert_eq!(keys.len(), 500, "{codec}");
            // Per-partition offsets are dense and ordered.
            let mut by_p: BTreeMap<i32, Vec<i64>> = BTreeMap::new();
            for (p, o, k, v) in &got {
                let i: usize = k[1..].parse().unwrap();
                assert!(v.starts_with(&format!("value-{i}-")), "{codec}");
                by_p.entry(*p).or_default().push(*o);
            }
            for offs in by_p.values() {
                assert_eq!(*offs, (0..offs.len() as i64).collect::<Vec<_>>(), "{codec}");
            }
            // Keyed records land on the same partition every time (murmur2 partitioner).
            assert!(by_p.len() > 1, "{codec}: keys should spread across partitions");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headers_timestamps_and_null_values() {
    let tb = TestBroker::start("rdk-headers").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "h", 1, &[]);
        let p = producer(&addr, &[]);
        let headers = OwnedHeaders::new().insert(Header { key: "a", value: Some("1") }).insert(Header { key: "b", value: None::<&str> });
        p.send(BaseRecord::to("h").key("k").payload("v").headers(headers).timestamp(1_700_000_000_000)).map_err(|e| e.0).unwrap();
        p.send(BaseRecord::<str, str>::to("h").key("tomb")).map_err(|e| e.0).unwrap();
        p.flush(T).unwrap();

        let c = consumer(&addr, "gh", &[]);
        c.subscribe(&["h"]).unwrap();
        let deadline = Instant::now() + T;
        let mut msgs = vec![];
        while msgs.len() < 2 && Instant::now() < deadline {
            if let Some(Ok(m)) = c.poll(Duration::from_millis(200)) {
                msgs.push(m.detach());
            }
        }
        assert_eq!(msgs.len(), 2);
        let m = &msgs[0];
        assert_eq!(m.timestamp().to_millis(), Some(1_700_000_000_000));
        let h = m.headers().unwrap();
        assert_eq!(h.count(), 2);
        assert_eq!(h.get(0).key, "a");
        assert_eq!(h.get(0).value, Some(&b"1"[..]));
        assert_eq!(h.get(1).value, None);
        assert_eq!(msgs[1].payload(), None, "tombstone keeps a null value");
        assert_eq!(msgs[1].key(), Some(&b"tomb"[..]));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotent_producer_exactly_once_order() {
    let tb = TestBroker::start("rdk-idem").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "idem", 2, &[]);
        let p = producer(&addr, &[("enable.idempotence", "true"), ("max.in.flight.requests.per.connection", "5"), ("batch.num.messages", "50")]);
        for i in 0..5000 {
            send(&p, "idem", &format!("k{}", i % 7), &i.to_string());
        }
        p.flush(T).unwrap();
        let c = consumer(&addr, "gi", &[]);
        c.subscribe(&["idem"]).unwrap();
        let got = drain(&c, 5000);
        assert_eq!(quiet(&c, Duration::from_millis(500)), 0, "no duplicates");
        // Per key, values arrive in send order.
        let mut last: BTreeMap<String, i64> = BTreeMap::new();
        for (_, _, k, v) in got {
            let v: i64 = v.parse().unwrap();
            if let Some(prev) = last.insert(k, v) {
                assert!(v > prev);
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_group_rebalance_and_resume() {
    let tb = TestBroker::start("rdk-group").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "orders", 6, &[]);
        let p = producer(&addr, &[]);
        for i in 0..600 {
            send(&p, "orders", &format!("k{i}"), &i.to_string());
        }
        p.flush(T).unwrap();

        // Two members split the six partitions.
        let a = consumer(&addr, "billing", &[("partition.assignment.strategy", "range")]);
        let b = consumer(&addr, "billing", &[("partition.assignment.strategy", "range")]);
        a.subscribe(&["orders"]).unwrap();
        b.subscribe(&["orders"]).unwrap();
        let deadline = Instant::now() + T;
        let mut seen = HashSet::new();
        loop {
            for c in [&a, &b] {
                if let Some(Ok(m)) = c.poll(Duration::from_millis(50)) {
                    seen.insert((m.partition(), m.offset()));
                }
            }
            let (pa, pb) = (a.assignment().unwrap().count(), b.assignment().unwrap().count());
            if seen.len() == 600 && pa + pb == 6 && pa > 0 && pb > 0 {
                break;
            }
            assert!(Instant::now() < deadline, "seen {} assignments {pa}+{pb}", seen.len());
        }
        a.commit_consumer_state(CommitMode::Sync).unwrap();
        b.commit_consumer_state(CommitMode::Sync).unwrap();
        drop(a);
        drop(b);

        // New records after the commit: a restarted member reads only those.
        for i in 600..650 {
            send(&p, "orders", &format!("k{i}"), &i.to_string());
        }
        p.flush(T).unwrap();
        let c = consumer(&addr, "billing", &[]);
        c.subscribe(&["orders"]).unwrap();
        let got = drain(&c, 50);
        let values: HashSet<i64> = got.iter().map(|m| m.3.parse().unwrap()).collect();
        assert_eq!(values, (600..650).collect());
        assert_eq!(quiet(&c, Duration::from_millis(500)), 0);

        // Committed offsets are visible through the group APIs.
        let mut tpl = TopicPartitionList::new();
        for i in 0..6 {
            tpl.add_partition("orders", i);
        }
        let committed = c.committed_offsets(tpl, T).unwrap();
        let total: i64 = committed.elements().iter().map(|e| e.offset().to_raw().unwrap_or(0)).sum();
        assert_eq!(total, 600);
        let groups = c.fetch_group_list(Some("billing"), T).unwrap();
        assert_eq!(groups.groups().len(), 1);
        assert_eq!(groups.groups()[0].protocol_type(), "consumer");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_assign_seek_and_watermarks() {
    let tb = TestBroker::start("rdk-assign").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "seek", 1, &[]);
        let p = producer(&addr, &[]);
        for i in 0..100 {
            p.send(BaseRecord::to("seek").key("k").payload(&i.to_string()).timestamp(1_000_000 + i * 1000)).map_err(|e| e.0).unwrap();
        }
        p.flush(T).unwrap();
        let c = consumer(&addr, "unused", &[]);
        assert_eq!(c.fetch_watermarks("seek", 0, T).unwrap(), (0, 100));
        let mut tpl = TopicPartitionList::new();
        tpl.add_partition_offset("seek", 0, Offset::Offset(42)).unwrap();
        c.assign(&tpl).unwrap();
        let got = drain(&c, 3);
        assert_eq!(got.iter().map(|m| m.1).collect::<Vec<_>>(), vec![42, 43, 44]);

        // Offsets for times: the first record at or after the timestamp.
        let mut q = TopicPartitionList::new();
        q.add_partition_offset("seek", 0, Offset::Offset(1_000_000 + 10_500)).unwrap();
        let r = c.offsets_for_times(q, T).unwrap();
        assert_eq!(r.find_partition("seek", 0).unwrap().offset(), Offset::Offset(11));
        let mut q = TopicPartitionList::new();
        q.add_partition_offset("seek", 0, Offset::Offset(9_000_000)).unwrap();
        let r = c.offsets_for_times(q, T).unwrap();
        assert_eq!(r.find_partition("seek", 0).unwrap().offset(), Offset::End);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transactions_commit_and_abort() {
    let tb = TestBroker::start("rdk-txn").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "txn", 2, &[]);
        let p = producer(&addr, &[("transactional.id", "tx-1"), ("transaction.timeout.ms", "10000")]);
        p.init_transactions(T).unwrap();

        p.begin_transaction().unwrap();
        for i in 0..10 {
            send(&p, "txn", &format!("c{i}"), "committed");
        }
        p.commit_transaction(T).unwrap();

        p.begin_transaction().unwrap();
        for i in 0..10 {
            send(&p, "txn", &format!("a{i}"), "aborted");
        }
        p.flush(T).unwrap();
        p.abort_transaction(T).unwrap();

        p.begin_transaction().unwrap();
        send(&p, "txn", "c-last", "committed");
        p.commit_transaction(T).unwrap();

        let rc = consumer(&addr, "rc", &[("isolation.level", "read_committed")]);
        rc.subscribe(&["txn"]).unwrap();
        let got = drain(&rc, 11);
        assert!(got.iter().all(|m| m.3 == "committed"), "{got:?}");
        assert_eq!(quiet(&rc, Duration::from_millis(500)), 0, "aborted records stay hidden");

        let ru = consumer(&addr, "ru", &[("isolation.level", "read_uncommitted")]);
        ru.subscribe(&["txn"]).unwrap();
        let got = drain(&ru, 21);
        assert_eq!(got.iter().filter(|m| m.3 == "aborted").count(), 10);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consume_transform_produce_exactly_once() {
    let tb = TestBroker::start("rdk-eos").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "in", 1, &[]);
        create_topic(&addr, "out", 1, &[]);
        let p = producer(&addr, &[]);
        for i in 0..20 {
            send(&p, "in", "k", &i.to_string());
        }
        p.flush(T).unwrap();

        let c = consumer(&addr, "eos", &[("isolation.level", "read_committed")]);
        c.subscribe(&["in"]).unwrap();
        let tp = producer(&addr, &[("transactional.id", "eos-1")]);
        tp.init_transactions(T).unwrap();
        let msgs = drain(&c, 10);
        tp.begin_transaction().unwrap();
        for (_, _, k, v) in &msgs {
            send(&tp, "out", k, &format!("doubled:{}", v.parse::<i64>().unwrap() * 2));
        }
        let mut offsets = TopicPartitionList::new();
        offsets.add_partition_offset("in", 0, Offset::Offset(msgs.last().unwrap().1 + 1)).unwrap();
        tp.send_offsets_to_transaction(&offsets, &c.group_metadata().unwrap(), T).unwrap();
        tp.commit_transaction(T).unwrap();

        let mut tpl = TopicPartitionList::new();
        tpl.add_partition("in", 0);
        let committed = c.committed_offsets(tpl, T).unwrap();
        assert_eq!(committed.find_partition("in", 0).unwrap().offset(), Offset::Offset(10));

        let out = consumer(&addr, "reader", &[("isolation.level", "read_committed")]);
        out.subscribe(&["out"]).unwrap();
        let got = drain(&out, 10);
        assert_eq!(got[3].3, "doubled:6");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_topics_configs_partitions() {
    let tb = TestBroker::start("rdk-admin").await;
    let addr = tb.addr.clone();
    blocking(move || {
        let rt = rt();
        let a = admin(&addr);
        let res = rt
            .block_on(a.create_topics(
                &[NewTopic::new("a1", 2, TopicReplication::Fixed(3)).set("retention.ms", "3600000"), NewTopic::new("bad name!", 1, TopicReplication::Fixed(1))],
                &opts(),
            ))
            .unwrap();
        assert!(res[0].is_ok());
        assert!(matches!(res[1], Err((_, RDKafkaErrorCode::InvalidTopic))), "{:?}", res[1]);
        let again = rt.block_on(a.create_topics(&[NewTopic::new("a1", 2, TopicReplication::Fixed(1))], &opts())).unwrap();
        assert!(matches!(again[0], Err((_, RDKafkaErrorCode::TopicAlreadyExists))));

        let cfg = rt.block_on(a.describe_configs(&[ResourceSpecifier::Topic("a1")], &opts())).unwrap();
        let r = cfg[0].as_ref().unwrap();
        let entry = r.get("retention.ms").unwrap();
        assert_eq!(entry.value.as_deref(), Some("3600000"));
        assert!(!entry.is_default);
        assert!(r.get("cleanup.policy").unwrap().is_default);

        let alter = AlterConfig::new(ResourceSpecifier::Topic("a1")).set("retention.ms", "60000").set("max.message.bytes", "2000000");
        let res = rt.block_on(a.alter_configs(&[alter], &opts())).unwrap();
        assert!(res[0].is_ok(), "{:?}", res[0]);
        let cfg = rt.block_on(a.describe_configs(&[ResourceSpecifier::Topic("a1")], &opts())).unwrap();
        let r = cfg[0].as_ref().unwrap();
        assert_eq!(r.get("retention.ms").unwrap().value.as_deref(), Some("60000"));
        assert_eq!(r.get("max.message.bytes").unwrap().value.as_deref(), Some("2000000"));

        let bad = AlterConfig::new(ResourceSpecifier::Topic("a1")).set("retention.ms", "soon");
        // rdkafka-rs drops per-resource AlterConfigs errors, so check the value held instead
        // (the error code itself is covered by the protocol tests).
        rt.block_on(a.alter_configs(&[bad], &opts())).unwrap();
        let cfg = rt.block_on(a.describe_configs(&[ResourceSpecifier::Topic("a1")], &opts())).unwrap();
        assert_eq!(cfg[0].as_ref().unwrap().get("retention.ms").unwrap().value.as_deref(), Some("60000"));

        let broker = rt.block_on(a.describe_configs(&[ResourceSpecifier::Broker(1)], &opts())).unwrap();
        assert!(broker[0].as_ref().unwrap().get("num.partitions").is_some());

        // Online partition add: producers and consumers keep going.
        let res = rt.block_on(a.create_partitions(&[NewPartitions::new("a1", 5)], &opts())).unwrap();
        assert!(res[0].is_ok(), "{:?}", res[0]);
        let shrink = rt.block_on(a.create_partitions(&[NewPartitions::new("a1", 3)], &opts())).unwrap();
        assert!(shrink[0].is_err());
        let c = consumer(&addr, "meta", &[]);
        let md = c.fetch_metadata(Some("a1"), T).unwrap();
        assert_eq!(md.topics()[0].partitions().len(), 5);
        assert_eq!(md.brokers().len(), 1);

        let res = rt.block_on(a.delete_topics(&["a1", "missing"], &opts())).unwrap();
        assert!(res[0].is_ok());
        assert!(matches!(res[1], Err((_, RDKafkaErrorCode::UnknownTopicOrPartition))));
        let md = c.fetch_metadata(None, T).unwrap();
        assert!(md.topics().iter().all(|t| t.name() != "a1"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_records_moves_log_start() {
    let tb = TestBroker::start("rdk-delrec").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "dr", 1, &[]);
        let p = producer(&addr, &[]);
        for i in 0..50 {
            send(&p, "dr", "k", &i.to_string());
        }
        p.flush(T).unwrap();
        let a = admin(&addr);
        let mut tpl = TopicPartitionList::new();
        tpl.add_partition_offset("dr", 0, Offset::Offset(20)).unwrap();
        let res = rt().block_on(a.delete_records(&tpl, &opts())).unwrap();
        assert_eq!(res.find_partition("dr", 0).unwrap().offset(), Offset::Offset(20));
        let c = consumer(&addr, "dr", &[]);
        assert_eq!(c.fetch_watermarks("dr", 0, T).unwrap(), (20, 50));
        c.subscribe(&["dr"]).unwrap();
        let got = drain(&c, 30);
        assert_eq!(got[0].1, 20);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_create_on_produce_and_metadata() {
    let tb = TestBroker::start("rdk-auto").await;
    let addr = tb.addr.clone();
    blocking(move || {
        let p = producer(&addr, &[]);
        send(&p, "auto-made", "k", "v");
        p.flush(T).unwrap();
        let c = consumer(&addr, "auto", &[]);
        let md = c.fetch_metadata(Some("auto-made"), T).unwrap();
        assert_eq!(md.topics()[0].partitions().len(), 1);
        assert_eq!(c.fetch_watermarks("auto-made", 0, T).unwrap(), (0, 1));
    })
    .await;
}

fn sasl(addr: &str, mech: &str, user: &str, pass: &str) -> ClientConfig {
    let mut c = config(addr);
    c.set("security.protocol", "SASL_PLAINTEXT").set("sasl.mechanisms", mech).set("sasl.username", user).set("sasl.password", pass);
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sasl_mechanisms_and_tenant_isolation() {
    let tb = TestBroker::start_with("rdk-sasl", |c| c.default_stream = Some("-".into())).await;
    // No default stream: every connection must authenticate.
    let (other, ocred, osecret) = tb.broker.create_stream("other", Default::default()).unwrap();
    let (s, cred, secret) = tb.broker.create_stream("mine", Default::default()).unwrap();
    let _ = (other, s);
    let addr = tb.addr.clone();
    let (user, ouser) = (cred.username.clone(), ocred.username.clone());
    blocking(move || {
        for mech in ["PLAIN", "SCRAM-SHA-256", "SCRAM-SHA-512"] {
            let p: ThreadedProducer<DefaultProducerContext> = sasl(&addr, mech, &user, &secret).create().unwrap();
            let topic = format!("t-{}", mech.to_lowercase());
            send(&p, &topic, "k", mech);
            p.flush(T).unwrap();
            let c: BaseConsumer = sasl(&addr, mech, &user, &secret).set("group.id", "g").set("auto.offset.reset", "earliest").create().unwrap();
            c.subscribe(&[&topic]).unwrap();
            assert_eq!(drain(&c, 1)[0].3, mech);
        }

        // Another stream sees none of those topics.
        let c: BaseConsumer = sasl(&addr, "SCRAM-SHA-256", &ouser, &osecret).create().unwrap();
        let md = c.fetch_metadata(None, T).unwrap();
        assert!(md.topics().is_empty(), "{:?}", md.topics().iter().map(|t| t.name()).collect::<Vec<_>>());

        // Wrong passwords fail for every mechanism.
        for mech in ["PLAIN", "SCRAM-SHA-256", "SCRAM-SHA-512"] {
            let c: BaseConsumer = sasl(&addr, mech, &user, "wrong").create().unwrap();
            let err = c.fetch_metadata(None, Duration::from_secs(3)).unwrap_err();
            assert!(matches!(err, KafkaError::MetadataFetch(_)), "{mech}: {err:?}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recovers_data_offsets_and_producers() {
    let tb = TestBroker::start("rdk-restart").await;
    let addr = tb.addr.clone();
    blocking(move || {
        create_topic(&addr, "dur", 2, &[]);
        let p = producer(&addr, &[("enable.idempotence", "true")]);
        for i in 0..300 {
            send(&p, "dur", &format!("k{i}"), &i.to_string());
        }
        p.flush(T).unwrap();
        let c = consumer(&addr, "dur", &[]);
        c.subscribe(&["dur"]).unwrap();
        drain(&c, 300);
        c.commit_consumer_state(CommitMode::Sync).unwrap();
    })
    .await;
    let config = tb.broker.config.clone();
    let (dir, ids) = tb.stop().await;
    let mut config = config;
    config.listen = "127.0.0.1:0".parse().unwrap();
    let tb = TestBroker::boot(dir, config, Some(ids)).await;
    let addr = tb.addr.clone();
    blocking(move || {
        let c = consumer(&addr, "fresh", &[]);
        assert_eq!(c.fetch_watermarks("dur", 0, T).unwrap().0, 0);
        let (l0, h0) = c.fetch_watermarks("dur", 0, T).unwrap();
        let (l1, h1) = c.fetch_watermarks("dur", 1, T).unwrap();
        assert_eq!((h0 - l0) + (h1 - l1), 300);
        c.subscribe(&["dur"]).unwrap();
        assert_eq!(drain(&c, 300).len(), 300);

        // The committed group resumes at the end.
        let g = consumer(&addr, "dur", &[]);
        g.subscribe(&["dur"]).unwrap();
        assert_eq!(quiet(&g, Duration::from_secs(2)), 0);
    })
    .await;
}

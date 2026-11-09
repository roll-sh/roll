//! Log compaction end to end: latest value per key, tombstones, gaps that clients read across,
//! the swap in object storage, restarts and keyless records.

mod common;

use bytes::Bytes;
use common::TestBroker;
use common::client::Conn;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::*;
use kafka_protocol::protocol::StrBytes;
use rdkafka::Message;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, Consumer};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use stream_log::batch::{self, BatchOptions, NewRecord};

fn s(v: &str) -> StrBytes {
    StrBytes::from_string(v.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

const COMPACT: &[(&str, &str)] = &[("cleanup.policy", "compact"), ("segment.bytes", "1024"), ("min.cleanable.dirty.ratio", "0.01")];

async fn create(c: &mut Conn, name: &str, configs: &[(&str, &str)]) {
    let topic = create_topics_request::CreatableTopic::default()
        .with_name(TopicName(s(name)))
        .with_num_partitions(1)
        .with_replication_factor(-1)
        .with_configs(configs.iter().map(|(k, v)| create_topics_request::CreatableTopicConfig::default().with_name(s(k)).with_value(Some(s(v)))).collect());
    let r = c.call(7, &CreateTopicsRequest::default().with_topics(vec![topic])).await;
    assert_eq!(r.topics[0].error_code, 0, "{:?}", r.topics[0].error_message);
}

/// Produce `(key, value)` pairs as one batch; returns the error code and base offset.
async fn produce(c: &mut Conn, topic: &str, recs: &[(Option<&str>, Option<&str>)]) -> (i16, i64) {
    let recs: Vec<NewRecord> = recs
        .iter()
        .map(|(k, v)| NewRecord { key: k.map(|k| Bytes::from(k.to_string())), value: v.map(|v| Bytes::from(v.to_string())), ..Default::default() })
        .collect();
    let data = batch::build(&recs, BatchOptions::default(), now_ms());
    let req = ProduceRequest::default().with_acks(-1).with_timeout_ms(10_000).with_topic_data(vec![
        produce_request::TopicProduceData::default()
            .with_name(TopicName(s(topic)))
            .with_partition_data(vec![produce_request::PartitionProduceData::default().with_index(0).with_records(Some(data))]),
    ]);
    let r = c.call(9, &req).await;
    let p = &r.responses[0].partition_responses[0];
    (p.error_code, p.base_offset)
}

async fn fetch(c: &mut Conn, topic: &str, offset: i64) -> fetch_response::PartitionData {
    let req = FetchRequest::default().with_replica_id(BrokerId(-1)).with_max_wait_ms(0).with_max_bytes(4 << 20).with_session_epoch(-1).with_topics(vec![
        fetch_request::FetchTopic::default()
            .with_topic(TopicName(s(topic)))
            .with_partitions(vec![fetch_request::FetchPartition::default().with_partition(0).with_fetch_offset(offset).with_partition_max_bytes(64 << 10).with_current_leader_epoch(-1)]),
    ]);
    c.call(12, &req).await.responses[0].partitions[0].clone()
}

type Rec = (i64, Option<String>, Option<String>);

/// Everything readable from `from`, following batch offsets across compaction gaps.
async fn read_all(c: &mut Conn, topic: &str, from: i64) -> Vec<Rec> {
    let mut out = vec![];
    let mut offset = from;
    loop {
        let p = fetch(c, topic, offset).await;
        assert_eq!(p.error_code, 0, "fetch at {offset}");
        if offset >= p.high_watermark {
            break;
        }
        let data = p.records.unwrap_or_default();
        assert!(!data.is_empty(), "no data at {offset} below the high watermark {}", p.high_watermark);
        for r in stream_broker::records::decode(&data, None).unwrap() {
            if r.offset >= offset {
                let text = |b: Option<Bytes>| b.map(|b| String::from_utf8(b.to_vec()).unwrap());
                out.push((r.offset, text(r.key), text(r.value)));
            }
        }
        offset = batch::headers(&data).last().map(|(_, h)| h.next_offset()).unwrap();
    }
    out
}

fn latest(recs: &[Rec]) -> BTreeMap<String, Option<String>> {
    recs.iter().map(|(_, k, v)| (k.clone().unwrap(), v.clone())).collect()
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 30 rounds over keys k0..k9; returns what was written.
async fn write_rounds(c: &mut Conn, topic: &str, rounds: usize) -> Vec<Rec> {
    let mut written = vec![];
    for round in 0..rounds {
        let pairs: Vec<(String, String)> = (0..10).map(|k| (format!("k{k}"), format!("v{round}-{k}"))).collect();
        let recs: Vec<(Option<&str>, Option<&str>)> = pairs.iter().map(|(k, v)| (Some(k.as_str()), Some(v.as_str()))).collect();
        let (err, base) = produce(c, topic, &recs).await;
        assert_eq!(err, 0);
        for (i, (k, v)) in pairs.into_iter().enumerate() {
            written.push((base + i as i64, Some(k), Some(v)));
        }
    }
    written
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keeps_the_latest_value_per_key() {
    let tb = TestBroker::start_with("compact-latest", |c| c.compaction_grace_ms = 0).await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "table", COMPACT).await;
    let written = write_rounds(&mut c, "table", 30).await;
    let part = tb.broker.find_stream("test").unwrap().topic("table").unwrap().partition(0).unwrap();
    eventually("a compaction pass", || part.loaded().is_some_and(|l| l.cleaned_to() > 150)).await;
    // Hold the log still (no pass can be due at ratio 1 once something is clean) and let any
    // pass in flight finish.
    let topic = tb.broker.find_stream("test").unwrap().topic("table").unwrap();
    let mut configs: BTreeMap<String, String> = COMPACT.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    configs.insert("min.cleanable.dirty.ratio".into(), "1".into());
    tb.broker.set_topic_configs(&topic, configs, false).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cleaned = part.loaded().unwrap().cleaned_to();

    let read = read_all(&mut c, "table", 0).await;
    assert!(read.len() < written.len(), "compaction removed records ({} of {})", read.len(), written.len());
    assert!(read.windows(2).all(|w| w[0].0 < w[1].0), "offsets stay in order");
    assert_eq!(latest(&read), latest(&written), "the latest value of every key survives");
    // In the compacted range each key appears once, at its latest offset there.
    let compacted: Vec<&Rec> = read.iter().filter(|r| r.0 < cleaned).collect();
    let mut seen = HashMap::new();
    for r in &compacted {
        assert!(seen.insert(r.1.clone(), r.0).is_none(), "{:?} appears twice below {cleaned}", r.1);
    }
    for r in &compacted {
        let last_below = written.iter().filter(|w| w.0 < cleaned && w.1 == r.1).map(|w| w.0).max().unwrap();
        assert_eq!(r.0, last_below);
    }
    // Every original record above the compacted range is untouched.
    let tail: Vec<&Rec> = written.iter().filter(|w| w.0 >= cleaned).collect();
    assert_eq!(read.iter().filter(|r| r.0 >= cleaned).collect::<Vec<_>>(), tail);

    // A fetch inside a gap returns the next surviving data, not an error.
    let gap = (0..cleaned).find(|o| !read.iter().any(|r| r.0 == *o)).expect("a gap");
    let p = fetch(&mut c, "table", gap).await;
    assert_eq!(p.error_code, 0);
    let first = batch::headers(&p.records.unwrap()).next().map(|(_, h)| h).unwrap();
    assert!(first.last_offset() > gap);

    // The replaced objects are gone; what's left is the compacted chunks plus the newer tail.
    let prefix = stream_broker::config::topic_prefix(&tb.stream_id, &topic.id);
    let objects = tb.broker.objects.clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stale = objects
            .list(&prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| !stream_broker::config::is_compacted_key(k))
            .filter(|k| tb.broker.store.chunks(&topic.id, 0).unwrap().iter().all(|c| &c.key != k))
            .count();
        if stale == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "{stale} replaced objects left behind");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Restart: the catalog has the compacted chunks and knows they're clean.
    drop(c);
    let mut config = tb.broker.config.clone();
    let (dir, ids) = tb.stop().await;
    config.listen = "127.0.0.1:0".parse().unwrap();
    let tb = TestBroker::boot(dir, config, Some(ids)).await;
    let mut c = Conn::connect(&tb.addr).await;
    let again = read_all(&mut c, "table", 0).await;
    assert_eq!(latest(&again), latest(&written));
    assert!(again.len() <= read.len());
    let part = tb.broker.find_stream("test").unwrap().topic("table").unwrap().partition(0).unwrap();
    assert!(part.loaded().unwrap().cleaned_to() >= cleaned);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tombstones_delete_keys() {
    let tb = TestBroker::start("compact-tombstones").await;
    let mut c = Conn::connect(&tb.addr).await;
    let mut configs = COMPACT.to_vec();
    configs.push(("delete.retention.ms", "0"));
    create(&mut c, "users", &configs).await;
    create(&mut c, "users-kept", COMPACT).await;
    for t in ["users", "users-kept"] {
        produce(&mut c, t, &[(Some("alice"), Some("1")), (Some("bob"), Some("1"))]).await;
        produce(&mut c, t, &[(Some("alice"), None)]).await;
        write_rounds(&mut c, t, 20).await;
    }
    let s = tb.broker.find_stream("test").unwrap();
    for t in ["users", "users-kept"] {
        let part = s.topic(t).unwrap().partition(0).unwrap();
        eventually("compaction", || part.loaded().is_some_and(|l| l.cleaned_to() > 3)).await;
    }
    let gone = latest(&read_all(&mut c, "users", 0).await);
    assert!(!gone.contains_key("alice"), "an expired tombstone removes the key entirely");
    assert_eq!(gone.get("bob"), Some(&Some("1".to_string())));
    let kept = read_all(&mut c, "users-kept", 0).await;
    assert_eq!(kept.iter().filter(|r| r.1.as_deref() == Some("alice")).collect::<Vec<_>>(), vec![&(2, Some("alice".into()), None)], "a recent tombstone stays, the value it deletes goes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyless_records_are_rejected() {
    let tb = TestBroker::start("compact-keyless").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "keyed", COMPACT).await;
    let (err, _) = produce(&mut c, "keyed", &[(Some("a"), Some("1")), (None, Some("2"))]).await;
    assert_eq!(err, ResponseError::InvalidRecord.code());
    assert_eq!(produce(&mut c, "keyed", &[(Some("a"), Some("1"))]).await, (0, 0));

    // The HTTP data plane gets the same answer.
    let http = reqwest::Client::new();
    let r = http.post(format!("{}/v1/streams/test/topics/keyed/records", tb.http)).json(&serde_json::json!({ "value": "x" })).send().await.unwrap();
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(body.to_string().contains("without key"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_and_delete_together() {
    let tb = TestBroker::start("compact-delete").await;
    let mut c = Conn::connect(&tb.addr).await;
    let mut configs = COMPACT.to_vec();
    configs[0] = ("cleanup.policy", "compact,delete");
    configs.push(("retention.bytes", "4096"));
    // Compaction waits until the writes are in: passes between slow writes (fsync) would keep the
    // log under retention.bytes, and nothing would ever be deleted.
    let mut held = configs.clone();
    held.push(("min.compaction.lag.ms", "3600000"));
    create(&mut c, "both", &held).await;
    let written = write_rounds(&mut c, "both", 60).await;
    let log = || tb.broker.find_stream("test").unwrap().topic("both").unwrap().partition(0).unwrap().loaded().unwrap();
    eventually("retention", || log().log_start() > 0).await;
    let topic = tb.broker.find_stream("test").unwrap().topic("both").unwrap();
    tb.broker.set_topic_configs(&topic, configs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), false).unwrap();
    eventually("compaction", || log().cleaned_to() > log().log_start()).await;
    let read = read_all(&mut c, "both", log().log_start()).await;
    assert_eq!(latest(&read), latest(&written), "recent values all survive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn librdkafka_reads_across_gaps() {
    let tb = TestBroker::start("compact-rdkafka").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "rk", COMPACT).await;
    let written = write_rounds(&mut c, "rk", 30).await;
    let part = tb.broker.find_stream("test").unwrap().topic("rk").unwrap().partition(0).unwrap();
    eventually("compaction", || part.loaded().is_some_and(|l| l.cleaned_to() > 100)).await;
    let end = written.last().unwrap().0;
    let addr = tb.addr.clone();
    let got = tokio::task::spawn_blocking(move || {
        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &addr)
            .set("broker.address.family", "v4")
            .set("group.id", "table-loader")
            .set("auto.offset.reset", "earliest")
            .set("enable.auto.commit", "false")
            .create()
            .unwrap();
        consumer.subscribe(&["rk"]).unwrap();
        let mut got: Vec<Rec> = vec![];
        let deadline = Instant::now() + Duration::from_secs(30);
        while got.last().is_none_or(|r| r.0 < end) && Instant::now() < deadline {
            if let Some(m) = consumer.poll(Duration::from_millis(200)) {
                let m = m.unwrap();
                let text = |b: Option<&[u8]>| b.map(|b| String::from_utf8(b.to_vec()).unwrap());
                got.push((m.offset(), text(m.key()), text(m.payload())));
            }
        }
        got
    })
    .await
    .unwrap();
    // Compaction may run again while it reads; whatever it sees is a consistent compacted log.
    assert_eq!(got.last().map(|r| r.0), Some(end), "reached the end");
    assert!(got.len() < written.len());
    assert!(got.windows(2).all(|w| w[0].0 < w[1].0));
    assert!(got.iter().all(|r| written.contains(r)), "only original records, at their original offsets");
    assert_eq!(latest(&got), latest(&written));
}

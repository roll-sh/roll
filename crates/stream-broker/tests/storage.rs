//! Storage behaviour end to end: tiering to object storage and reading it back, retention,
//! crash recovery, scale-to-zero partitions and restarts.
//!
//! With `ROLL_E2E_S3_BUCKET` (plus `ROLL_E2E_S3_ENDPOINT` and AWS credentials) object storage
//! is S3 instead of a local directory (see `common::test_config`).

mod common;

use bytes::Bytes;
use common::TestBroker;
use common::client::Conn;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::*;
use kafka_protocol::protocol::StrBytes;
use std::time::{Duration, Instant};
use stream_log::batch::{self, BatchOptions, NewRecord};

fn s(v: &str) -> StrBytes {
    StrBytes::from_string(v.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn batch_of(start: usize, n: usize, ts: Option<i64>) -> Bytes {
    let recs: Vec<NewRecord> = (start..start + n).map(|i| NewRecord { value: Some(Bytes::from(format!("value-{i:06}-{}", "x".repeat(40)))), timestamp: ts, ..Default::default() }).collect();
    batch::build(&recs, BatchOptions::default(), now_ms())
}

async fn create(c: &mut Conn, name: &str, configs: &[(&str, &str)]) {
    let topic = create_topics_request::CreatableTopic::default()
        .with_name(TopicName(s(name)))
        .with_num_partitions(1)
        .with_replication_factor(-1)
        .with_configs(configs.iter().map(|(k, v)| create_topics_request::CreatableTopicConfig::default().with_name(s(k)).with_value(Some(s(v)))).collect());
    let r = c.call(7, &CreateTopicsRequest::default().with_topics(vec![topic])).await;
    assert_eq!(r.topics[0].error_code, 0, "{:?}", r.topics[0].error_message);
}

async fn produce(c: &mut Conn, topic: &str, records: Bytes) -> i64 {
    let req = ProduceRequest::default().with_acks(-1).with_timeout_ms(10_000).with_topic_data(vec![
        produce_request::TopicProduceData::default()
            .with_name(TopicName(s(topic)))
            .with_partition_data(vec![produce_request::PartitionProduceData::default().with_index(0).with_records(Some(records))]),
    ]);
    let r = c.call(9, &req).await;
    let p = &r.responses[0].partition_responses[0];
    assert_eq!(p.error_code, 0);
    p.base_offset
}

async fn fetch(c: &mut Conn, topic: &str, offset: i64) -> fetch_response::PartitionData {
    let req = FetchRequest::default().with_replica_id(BrokerId(-1)).with_max_wait_ms(0).with_max_bytes(4 << 20).with_session_epoch(-1).with_topics(vec![
        fetch_request::FetchTopic::default()
            .with_topic(TopicName(s(topic)))
            .with_partitions(vec![fetch_request::FetchPartition::default().with_partition(0).with_fetch_offset(offset).with_partition_max_bytes(256 << 10).with_current_leader_epoch(-1)]),
    ]);
    c.call(12, &req).await.responses[0].partitions[0].clone()
}

/// Read the whole partition from `from` with Fetch and return the values.
async fn read_all(c: &mut Conn, topic: &str, from: i64) -> Vec<String> {
    let mut out = vec![];
    let mut offset = from;
    loop {
        let p = fetch(c, topic, offset).await;
        assert_eq!(p.error_code, 0, "fetch at {offset}");
        let data = p.records.unwrap_or_default();
        if data.is_empty() {
            break;
        }
        for r in stream_broker::records::decode(&data, None).unwrap() {
            if r.offset >= offset {
                out.push(String::from_utf8(r.value.unwrap().to_vec()).unwrap());
            }
        }
        offset = batch::headers(&data).last().map(|(_, h)| h.next_offset()).unwrap();
        if offset >= p.high_watermark {
            break;
        }
    }
    out
}

fn expected(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("value-{i:06}-{}", "x".repeat(40))).collect()
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiered_partitions_read_from_object_storage() {
    let tb = TestBroker::start_with("store-tier", |c| {
        // Keep almost nothing in memory so reads really go to disk and object storage.
        c.tail_cache_bytes = 1024;
        c.remote_cache_bytes = 8 << 10;
    })
    .await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "tier", &[("segment.bytes", "4096"), ("local.retention.bytes", "0"), ("local.retention.ms", "0")]).await;
    for i in 0..100 {
        assert_eq!(produce(&mut c, "tier", batch_of(i * 10, 10, None)).await, (i * 10) as i64);
    }
    let topic = tb.broker.find_stream("test").unwrap().topic("tier").unwrap();
    let part = topic.partition(0).unwrap();
    // Everything sealed is uploaded and most of it is gone from local disk.
    eventually("upload and local cleanup", || {
        let l = part.loaded().unwrap();
        let st = l.stats();
        l.local_start() > 500 && st.pending_upload_bytes == 0 && st.local_bytes < st.remote_bytes / 2
    })
    .await;
    let st = part.loaded().unwrap().stats();
    assert_eq!(st.log_start, 0, "local cleanup doesn't move the log start");

    // Earliest-local and latest-tiered offsets reflect the split.
    let lo = |ts: i64| {
        ListOffsetsRequest::default().with_replica_id(BrokerId(-1)).with_topics(vec![
            list_offsets_request::ListOffsetsTopic::default().with_name(TopicName(s("tier"))).with_partitions(vec![list_offsets_request::ListOffsetsPartition::default().with_partition_index(0).with_timestamp(ts)]),
        ])
    };
    let local = c.call(9, &lo(-4)).await.topics[0].partitions[0].offset;
    let tiered = c.call(9, &lo(-5)).await.topics[0].partitions[0].offset;
    assert!(local > 0 && tiered >= local - 1, "local {local} tiered {tiered}");

    assert_eq!(read_all(&mut c, "tier", 0).await, expected(1000), "reads below the local range come from object storage");
    let stream = tb.broker.find_stream("test").unwrap();
    assert!(stream.usage.gets.load(std::sync::atomic::Ordering::Relaxed) > 0);

    // After a restart the catalog still knows where everything is.
    drop(c);
    let config = tb.broker.config.clone();
    let (dir, ids) = tb.stop().await;
    let mut config = config;
    config.listen = "127.0.0.1:0".parse().unwrap();
    let tb = TestBroker::boot(dir, config, Some(ids)).await;
    let mut c = Conn::connect(&tb.addr).await;
    assert_eq!(read_all(&mut c, "tier", 0).await, expected(1000));
    assert_eq!(read_all(&mut c, "tier", 777).await, expected(1000)[777..].to_vec());
    // And deleting the topic removes its objects.
    let objects = tb.broker.objects.clone();
    let prefix = stream_broker::config::topic_prefix(&tb.stream_id, &topic.id);
    assert!(!objects.list(&prefix).await.unwrap().is_empty());
    let r = c.call(4, &DeleteTopicsRequest::default().with_topic_names(vec![TopicName(s("tier"))])).await;
    assert_eq!(r.responses[0].error_code, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !objects.list(&prefix).await.unwrap().is_empty() {
        assert!(Instant::now() < deadline, "objects left behind");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!tb.broker.config.topic_dir(&topic.id).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_by_time_and_by_size() {
    let tb = TestBroker::start("store-retention").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "by-time", &[("segment.bytes", "2048"), ("retention.ms", "60000")]).await;
    create(&mut c, "by-size", &[("segment.bytes", "2048"), ("retention.bytes", "8192")]).await;
    let old = now_ms() - 3_600_000;
    for i in 0..30 {
        produce(&mut c, "by-time", batch_of(i * 5, 5, Some(old))).await;
        produce(&mut c, "by-size", batch_of(i * 5, 5, None)).await;
    }
    // Fresh data keeps the newest segment.
    produce(&mut c, "by-time", batch_of(150, 5, None)).await;
    let stream = tb.broker.find_stream("test").unwrap();
    let log = |t: &str| stream.topic(t).unwrap().partition(0).unwrap().loaded().unwrap();
    eventually("time retention", || log("by-time").log_start() >= 140).await;
    eventually("size retention", || log("by-size").log_start() > 0).await;
    let st = log("by-size").stats();
    assert!(st.local_bytes.max(st.remote_bytes) <= 8192 + 2 * 2048, "{st:?}");

    let p = fetch(&mut c, "by-time", 0).await;
    assert_eq!(p.error_code, ResponseError::OffsetOutOfRange.code());
    assert_eq!(p.log_start_offset, log("by-time").log_start());
    let values = read_all(&mut c, "by-time", p.log_start_offset).await;
    assert!(values.contains(&format!("value-{:06}-{}", 154, "x".repeat(40))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovers_from_a_torn_write() {
    let tb = TestBroker::start("store-torn").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "torn", &[]).await;
    for i in 0..20 {
        produce(&mut c, "torn", batch_of(i * 5, 5, None)).await;
    }
    let topic_id = tb.broker.find_stream("test").unwrap().topic("torn").unwrap().id;
    let config = tb.broker.config.clone();
    drop(c);
    let (dir, ids) = tb.stop().await;

    // Half a batch at the end of the active segment, as if the machine died mid-write.
    let pdir = config.partition_dir(&topic_id, 0);
    let mut logs: Vec<_> = std::fs::read_dir(&pdir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "log")).collect();
    logs.sort();
    let last = logs.last().unwrap();
    let partial = batch_of(1000, 5, None);
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(last).unwrap().write_all(&partial[..partial.len() / 2]).unwrap();

    let mut config = config;
    config.listen = "127.0.0.1:0".parse().unwrap();
    let tb = TestBroker::boot(dir, config, Some(ids)).await;
    let mut c = Conn::connect(&tb.addr).await;
    assert_eq!(read_all(&mut c, "torn", 0).await, expected(100), "the torn tail is dropped, everything before survives");
    assert_eq!(produce(&mut c, "torn", batch_of(100, 5, None)).await, 100, "appends continue at the right offset");
    assert_eq!(read_all(&mut c, "torn", 0).await, expected(105));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledged_writes_survive_an_unclean_stop() {
    let tb = TestBroker::start("store-crash").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "crash", &[]).await;
    for i in 0..50 {
        produce(&mut c, "crash", batch_of(i * 2, 2, None)).await;
    }
    let mut config = tb.broker.config.clone();
    let dir = tb.dir.clone();
    let ids = (tb.stream_id.clone(), tb.username.clone(), tb.secret.clone());
    // No sync_all: only what acks=all made durable.
    drop(c);
    drop(tb);
    config.listen = "127.0.0.1:0".parse().unwrap();
    let tb = TestBroker::boot(dir, config, Some(ids)).await;
    let mut c = Conn::connect(&tb.addr).await;
    assert_eq!(read_all(&mut c, "crash", 0).await, expected(100));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_partitions_scale_to_zero_and_wake_on_demand() {
    let tb = TestBroker::start_with("store-idle", |c| {
        c.idle_unload_ms = 100;
        c.clean_interval_ms = 100;
    })
    .await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "idle", &[]).await;
    produce(&mut c, "idle", batch_of(0, 10, None)).await;
    let part = tb.broker.find_stream("test").unwrap().topic("idle").unwrap().partition(0).unwrap();
    eventually("unload", || part.loaded().is_none()).await;
    assert_eq!(read_all(&mut c, "idle", 0).await, expected(10));
    assert!(part.loaded().is_some(), "a read opens it again");
    assert_eq!(produce(&mut c, "idle", batch_of(10, 10, None)).await, 10);
    eventually("unload again", || part.loaded().is_none()).await;
    assert_eq!(read_all(&mut c, "idle", 0).await, expected(20));
}

/// Bytes of partition data on local disk.
fn local_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "log")).map(|e| e.metadata().unwrap().len()).sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_partitions_keep_nothing_on_local_disk() {
    let tb = TestBroker::start_with("store-evict", |c| {
        c.idle_unload_ms = 100;
        c.clean_interval_ms = 100;
    })
    .await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "cold", &[("segment.bytes", "4096")]).await;
    for i in 0..10 {
        produce(&mut c, "cold", batch_of(i * 20, 20, None)).await;
    }
    let topic = tb.broker.find_stream("test").unwrap().topic("cold").unwrap();
    let part = topic.partition(0).unwrap();
    let dir = tb.broker.config.partition_dir(&topic.id, 0);
    assert!(local_bytes(&dir) > 0);
    eventually("unload", || part.loaded().is_none()).await;
    assert_eq!(local_bytes(&dir), 0, "everything is in object storage");
    // Reads come from object storage; appends carry on at the next offset.
    assert_eq!(read_all(&mut c, "cold", 0).await, expected(200));
    assert_eq!(produce(&mut c, "cold", batch_of(200, 20, None)).await, 200);
    eventually("unload again", || part.loaded().is_none()).await;
    assert_eq!(local_bytes(&dir), 0);
    assert_eq!(read_all(&mut c, "cold", 0).await, expected(220));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_disk_budget_drops_the_oldest_uploaded_data_first() {
    // Batches carry their creation time, which is what eviction orders by: build them per topic.
    let batches = || -> Vec<Bytes> { (0..40).map(|i| batch_of(i * 10, 10, None)).collect() };
    let per_topic: u64 = batches().iter().map(|b| b.len() as u64).sum();
    // Room for one topic and a bit: dropping the older topic's segments is enough.
    let budget = per_topic * 3 / 2;
    assert!(per_topic / 2 > 2 * 2048, "more than an active segment over");
    let tb = TestBroker::start_with("store-budget", |c| c.local_disk_bytes = budget).await;
    let mut c = Conn::connect(&tb.addr).await;
    let stream = tb.broker.find_stream("test").unwrap();
    for name in ["old", "new"] {
        create(&mut c, name, &[("segment.bytes", "2048")]).await;
        for b in batches() {
            produce(&mut c, name, b).await;
        }
        // Distinct timestamps, so every "old" segment is older than every "new" one.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let local = |name: &str| stream.topic(name).unwrap().partition(0).unwrap().loaded().unwrap().stats().local_bytes;
    eventually("under budget", || local("old") + local("new") <= budget).await;
    assert_eq!(local("new"), per_topic, "the newer topic keeps all of its data");
    assert!(local("old") < per_topic / 2);
    // Everything is still readable, partly from object storage.
    assert_eq!(read_all(&mut c, "old", 0).await, expected(400));
    assert_eq!(read_all(&mut c, "new", 0).await, expected(400));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_transactions_survive_a_restart() {
    let tb = TestBroker::start("store-txn").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "tx", &[]).await;
    let init = c.call(3, &InitProducerIdRequest::default().with_transactional_id(Some(TransactionalId(s("durable")))).with_transaction_timeout_ms(60_000).with_producer_id(ProducerId(-1)).with_producer_epoch(-1)).await;
    let (pid, epoch) = (init.producer_id, init.producer_epoch);
    c.call(
        3,
        &AddPartitionsToTxnRequest::default()
            .with_v3_and_below_transactional_id(TransactionalId(s("durable")))
            .with_v3_and_below_producer_id(pid)
            .with_v3_and_below_producer_epoch(epoch)
            .with_v3_and_below_topics(vec![add_partitions_to_txn_request::AddPartitionsToTxnTopic::default().with_name(TopicName(s("tx"))).with_partitions(vec![0])]),
    )
    .await;
    let recs: Vec<NewRecord> = (0..3).map(|i| NewRecord { value: Some(Bytes::from(format!("in-txn-{i}"))), ..Default::default() }).collect();
    let b = batch::build(&recs, BatchOptions { producer_id: pid.0, producer_epoch: epoch, base_sequence: 0, attributes: batch::TRANSACTIONAL }, now_ms());
    let req = ProduceRequest::default().with_acks(-1).with_timeout_ms(10_000).with_transactional_id(Some(TransactionalId(s("durable")))).with_topic_data(vec![
        produce_request::TopicProduceData::default().with_name(TopicName(s("tx"))).with_partition_data(vec![produce_request::PartitionProduceData::default().with_index(0).with_records(Some(b))]),
    ]);
    assert_eq!(c.call(9, &req).await.responses[0].partition_responses[0].error_code, 0);
    drop(c);
    let config = tb.broker.config.clone();
    let (dir, ids) = tb.stop().await;
    let mut config = config;
    config.listen = "127.0.0.1:0".parse().unwrap();
    let tb = TestBroker::boot(dir, config, Some(ids)).await;
    let mut c = Conn::connect(&tb.addr).await;
    // Still open: read_committed is held at the LSO.
    let req = |iso: i8| {
        FetchRequest::default().with_replica_id(BrokerId(-1)).with_max_wait_ms(0).with_isolation_level(iso).with_session_epoch(-1).with_topics(vec![
            fetch_request::FetchTopic::default().with_topic(TopicName(s("tx"))).with_partitions(vec![fetch_request::FetchPartition::default().with_partition(0).with_partition_max_bytes(1 << 20).with_current_leader_epoch(-1)]),
        ])
    };
    let p = c.call(12, &req(1)).await.responses[0].partitions[0].clone();
    assert_eq!((p.high_watermark, p.last_stable_offset), (3, 0));
    let end = c.call(3, &EndTxnRequest::default().with_transactional_id(TransactionalId(s("durable"))).with_producer_id(pid).with_producer_epoch(epoch).with_committed(true)).await;
    assert_eq!(end.error_code, 0, "the restarted coordinator finishes the transaction");
    let p = c.call(12, &req(1)).await.responses[0].partitions[0].clone();
    assert_eq!(p.last_stable_offset, 4);
    let values: Vec<String> = stream_broker::records::decode(&p.records.unwrap(), Some(&[])).unwrap().into_iter().map(|r| String::from_utf8(r.value.unwrap().to_vec()).unwrap()).collect();
    assert_eq!(values, vec!["in-txn-0", "in-txn-1", "in-txn-2"]);
}

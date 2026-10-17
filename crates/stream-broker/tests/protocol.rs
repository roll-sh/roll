//! The Kafka wire protocol, request by request: every API at its lowest and highest version,
//! old-client paths, and the semantics clients rely on (idempotence, transactions, groups,
//! fetch long polling, pipelining, SASL, quotas, tenancy).

mod common;

use bytes::{Bytes, BytesMut};
use common::TestBroker;
use common::client::Conn;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::*;
use kafka_protocol::protocol::StrBytes;
use std::time::{Duration, Instant};
use stream_broker::auth::{Mechanism, client::Scram};
use stream_broker::handlers::APIS;
use stream_log::batch::{self, BatchOptions, NewRecord};
use uuid::Uuid;

fn s(v: &str) -> StrBytes {
    StrBytes::from_string(v.to_string())
}

fn tn(v: &str) -> TopicName {
    TopicName(s(v))
}

fn code(e: ResponseError) -> i16 {
    e.code()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn values(prefix: &str, n: usize) -> Vec<NewRecord> {
    (0..n).map(|i| NewRecord { key: Some(Bytes::from(format!("k{i}"))), value: Some(Bytes::from(format!("{prefix}-{i}"))), ..Default::default() }).collect()
}

fn plain(prefix: &str, n: usize) -> Bytes {
    batch::build(&values(prefix, n), BatchOptions::default(), now_ms())
}

fn producer_batch(prefix: &str, n: usize, pid: i64, epoch: i16, seq: i32, transactional: bool) -> Bytes {
    let attributes = if transactional { batch::TRANSACTIONAL } else { 0 };
    batch::build(&values(prefix, n), BatchOptions { producer_id: pid, producer_epoch: epoch, base_sequence: seq, attributes }, now_ms())
}

fn produce_req(topic: &str, partition: i32, records: Bytes, acks: i16, txn: Option<&str>) -> ProduceRequest {
    ProduceRequest::default()
        .with_acks(acks)
        .with_timeout_ms(10_000)
        .with_transactional_id(txn.map(|t| TransactionalId(s(t))))
        .with_topic_data(vec![
            produce_request::TopicProduceData::default()
                .with_name(tn(topic))
                .with_partition_data(vec![produce_request::PartitionProduceData::default().with_index(partition).with_records(Some(records))]),
        ])
}

async fn produce(c: &mut Conn, v: i16, topic: &str, partition: i32, records: Bytes) -> produce_response::PartitionProduceResponse {
    let r = c.call(v, &produce_req(topic, partition, records, -1, None)).await;
    r.responses[0].partition_responses[0].clone()
}

fn fetch_req(topic: &str, id: Uuid, partition: i32, offset: i64, isolation: i8) -> FetchRequest {
    FetchRequest::default()
        .with_replica_id(BrokerId(-1))
        .with_max_wait_ms(100)
        .with_min_bytes(1)
        .with_max_bytes(10 << 20)
        .with_isolation_level(isolation)
        .with_session_epoch(-1)
        .with_topics(vec![
            fetch_request::FetchTopic::default().with_topic(tn(topic)).with_topic_id(id).with_partitions(vec![
                fetch_request::FetchPartition::default().with_partition(partition).with_fetch_offset(offset).with_partition_max_bytes(1 << 20).with_current_leader_epoch(-1).with_last_fetched_epoch(-1),
            ]),
        ])
}

async fn fetch(c: &mut Conn, v: i16, tb: &TestBroker, topic: &str, partition: i32, offset: i64, isolation: i8) -> fetch_response::PartitionData {
    let r = c.call(v, &fetch_req(topic, topic_id(tb, topic), partition, offset, isolation)).await;
    r.responses[0].partitions[0].clone()
}

fn topic_id(tb: &TestBroker, name: &str) -> Uuid {
    tb.broker.find_stream("test").unwrap().topic(name).map(|t| t.id).unwrap_or_default()
}

async fn create(c: &mut Conn, name: &str, partitions: i32, configs: &[(&str, &str)]) -> create_topics_response::CreatableTopicResult {
    let topic = create_topics_request::CreatableTopic::default()
        .with_name(tn(name))
        .with_num_partitions(partitions)
        .with_replication_factor(-1)
        .with_configs(configs.iter().map(|(k, v)| create_topics_request::CreatableTopicConfig::default().with_name(s(k)).with_value(Some(s(v)))).collect());
    let r = c.call(7, &CreateTopicsRequest::default().with_topics(vec![topic]).with_timeout_ms(5000)).await;
    r.topics[0].clone()
}

fn records_of(p: &fetch_response::PartitionData) -> Vec<String> {
    let data = p.records.clone().unwrap_or_default();
    stream_broker::records::decode(&data, None).unwrap().into_iter().map(|r| String::from_utf8_lossy(&r.value.unwrap_or_default()).into_owned()).collect()
}

/// A request of every supported API with default fields.
fn default_request(api: ApiKey) -> RequestKind {
    match api {
        ApiKey::Produce => RequestKind::Produce(ProduceRequest::default().with_acks(-1)),
        ApiKey::Fetch => RequestKind::Fetch(FetchRequest::default().with_replica_id(BrokerId(-1)).with_max_wait_ms(0).with_session_epoch(-1)),
        ApiKey::ListOffsets => RequestKind::ListOffsets(ListOffsetsRequest::default().with_replica_id(BrokerId(-1))),
        ApiKey::Metadata => RequestKind::Metadata(MetadataRequest::default()),
        ApiKey::OffsetCommit => RequestKind::OffsetCommit(OffsetCommitRequest::default().with_group_id(GroupId(s("g")))),
        ApiKey::OffsetFetch => RequestKind::OffsetFetch(OffsetFetchRequest::default()),
        ApiKey::FindCoordinator => RequestKind::FindCoordinator(FindCoordinatorRequest::default()),
        ApiKey::JoinGroup => RequestKind::JoinGroup(JoinGroupRequest::default()),
        ApiKey::Heartbeat => RequestKind::Heartbeat(HeartbeatRequest::default()),
        ApiKey::LeaveGroup => RequestKind::LeaveGroup(LeaveGroupRequest::default()),
        ApiKey::SyncGroup => RequestKind::SyncGroup(SyncGroupRequest::default()),
        ApiKey::DescribeGroups => RequestKind::DescribeGroups(DescribeGroupsRequest::default()),
        ApiKey::ListGroups => RequestKind::ListGroups(ListGroupsRequest::default()),
        ApiKey::SaslHandshake => RequestKind::SaslHandshake(SaslHandshakeRequest::default()),
        ApiKey::ApiVersions => RequestKind::ApiVersions(ApiVersionsRequest::default().with_client_software_name(s("roll-tests")).with_client_software_version(s("1"))),
        ApiKey::CreateTopics => RequestKind::CreateTopics(CreateTopicsRequest::default()),
        ApiKey::DeleteTopics => RequestKind::DeleteTopics(DeleteTopicsRequest::default()),
        ApiKey::DeleteRecords => RequestKind::DeleteRecords(DeleteRecordsRequest::default()),
        ApiKey::InitProducerId => RequestKind::InitProducerId(InitProducerIdRequest::default().with_producer_id(ProducerId(-1)).with_producer_epoch(-1)),
        ApiKey::OffsetForLeaderEpoch => RequestKind::OffsetForLeaderEpoch(OffsetForLeaderEpochRequest::default()),
        ApiKey::AddPartitionsToTxn => RequestKind::AddPartitionsToTxn(AddPartitionsToTxnRequest::default()),
        ApiKey::AddOffsetsToTxn => RequestKind::AddOffsetsToTxn(AddOffsetsToTxnRequest::default()),
        ApiKey::EndTxn => RequestKind::EndTxn(EndTxnRequest::default()),
        ApiKey::TxnOffsetCommit => RequestKind::TxnOffsetCommit(TxnOffsetCommitRequest::default()),
        ApiKey::DescribeAcls => RequestKind::DescribeAcls(DescribeAclsRequest::default().with_resource_type_filter(1).with_pattern_type_filter(1).with_operation(1).with_permission_type(1)),
        ApiKey::CreateAcls => RequestKind::CreateAcls(CreateAclsRequest::default()),
        ApiKey::DeleteAcls => RequestKind::DeleteAcls(DeleteAclsRequest::default()),
        ApiKey::DescribeConfigs => RequestKind::DescribeConfigs(DescribeConfigsRequest::default()),
        ApiKey::AlterConfigs => RequestKind::AlterConfigs(AlterConfigsRequest::default()),
        ApiKey::DescribeLogDirs => RequestKind::DescribeLogDirs(DescribeLogDirsRequest::default()),
        ApiKey::SaslAuthenticate => RequestKind::SaslAuthenticate(SaslAuthenticateRequest::default()),
        ApiKey::CreatePartitions => RequestKind::CreatePartitions(CreatePartitionsRequest::default()),
        ApiKey::DeleteGroups => RequestKind::DeleteGroups(DeleteGroupsRequest::default()),
        ApiKey::ElectLeaders => RequestKind::ElectLeaders(ElectLeadersRequest::default()),
        ApiKey::IncrementalAlterConfigs => RequestKind::IncrementalAlterConfigs(IncrementalAlterConfigsRequest::default()),
        ApiKey::ListPartitionReassignments => RequestKind::ListPartitionReassignments(ListPartitionReassignmentsRequest::default()),
        ApiKey::OffsetDelete => RequestKind::OffsetDelete(OffsetDeleteRequest::default()),
        ApiKey::DescribeClientQuotas => RequestKind::DescribeClientQuotas(DescribeClientQuotasRequest::default()),
        ApiKey::AlterClientQuotas => RequestKind::AlterClientQuotas(AlterClientQuotasRequest::default()),
        ApiKey::DescribeUserScramCredentials => RequestKind::DescribeUserScramCredentials(DescribeUserScramCredentialsRequest::default()),
        ApiKey::DescribeCluster => RequestKind::DescribeCluster(DescribeClusterRequest::default()),
        ApiKey::DescribeProducers => RequestKind::DescribeProducers(DescribeProducersRequest::default()),
        ApiKey::DescribeTransactions => RequestKind::DescribeTransactions(DescribeTransactionsRequest::default()),
        ApiKey::ListTransactions => RequestKind::ListTransactions(ListTransactionsRequest::default().with_duration_filter(-1)),
        ApiKey::DescribeTopicPartitions => RequestKind::DescribeTopicPartitions(DescribeTopicPartitionsRequest::default()),
        ApiKey::DescribeQuorum => RequestKind::DescribeQuorum(DescribeQuorumRequest::default()),
        other => panic!("add a default request for {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_api_answers_at_its_min_and_max_version() {
    let tb = TestBroker::start("proto-all").await;
    for (api, min, max) in APIS {
        for v in *min..=*max {
            // A fresh connection each time: SASL requests change connection state.
            let mut c = Conn::connect(&tb.addr).await;
            let resp = c.call_kind(*api, v, &default_request(*api)).await;
            assert!(resp.is_some(), "{api:?} v{v}: connection closed");
            // The connection is still usable afterwards (unless SASL failed it on purpose).
            if !matches!(api, ApiKey::SaslAuthenticate | ApiKey::SaslHandshake) {
                let after = c.call_kind(ApiKey::ApiVersions, 3, &default_request(ApiKey::ApiVersions)).await;
                assert!(after.is_some(), "{api:?} v{v} broke the connection");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_versions_negotiation() {
    let tb = TestBroker::start("proto-versions").await;
    for v in 0..=4 {
        let mut c = Conn::connect(&tb.addr).await;
        let r = c.call(v, &ApiVersionsRequest::default().with_client_software_name(s("t")).with_client_software_version(s("1"))).await;
        assert_eq!(r.error_code, 0);
        for (api, min, max) in APIS {
            let e = r.api_keys.iter().find(|k| k.api_key == *api as i16).unwrap_or_else(|| panic!("{api:?} missing"));
            assert_eq!((e.min_version, e.max_version), (*min, *max));
        }
    }
    // A version from the future: answered at v0 with UNSUPPORTED_VERSION and the list.
    let mut c = Conn::connect(&tb.addr).await;
    let mut frame = bytes::BytesMut::new();
    // Clients send newer ApiVersions with header v2, as Kafka's own client does.
    RequestHeader::default().with_request_api_key(ApiKey::ApiVersions as i16).with_request_api_version(99).with_correlation_id(7).encode(&mut frame, 2).unwrap();
    c.send_frame(&frame).await;
    let mut resp = c.recv_frame().await.unwrap();
    let h = ResponseHeader::decode(&mut resp, 0).unwrap();
    assert_eq!(h.correlation_id, 7);
    let r = ApiVersionsResponse::decode(&mut resp, 0).unwrap();
    assert_eq!(r.error_code, code(ResponseError::UnsupportedVersion));
    assert!(r.api_keys.len() >= APIS.len());
    // Any other unsupported version closes the connection, as Kafka does.
    let mut c = Conn::connect(&tb.addr).await;
    let mut frame = bytes::BytesMut::new();
    RequestHeader::default().with_request_api_key(ApiKey::Produce as i16).with_request_api_version(2).with_correlation_id(1).encode(&mut frame, 1).unwrap();
    c.send_frame(&frame).await;
    assert!(c.recv_frame().await.is_none());
}

use kafka_protocol::protocol::{Decodable, Encodable};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn produce_and_fetch_across_versions_are_zero_copy() {
    let tb = TestBroker::start("proto-data").await;
    let mut c = Conn::connect(&tb.addr).await;
    assert_eq!(create(&mut c, "data", 1, &[]).await.error_code, 0);
    let id = topic_id(&tb, "data");
    let mut next = 0;
    for (pv, fv) in [(3, 4), (5, 7), (7, 11), (9, 12), (12, 16), (13, 17), (13, 18)] {
        let sent = plain(&format!("p{pv}"), 3);
        let req = if pv >= 13 {
            let mut r = produce_req("", 0, sent.clone(), -1, None);
            r.topic_data[0].topic_id = id;
            r
        } else {
            produce_req("data", 0, sent.clone(), -1, None)
        };
        let r = c.call(pv, &req).await;
        let pr = &r.responses[0].partition_responses[0];
        assert_eq!(pr.error_code, 0, "produce v{pv}");
        assert_eq!(pr.base_offset, next);
        let p = fetch(&mut c, fv, &tb, "data", 0, next, 0).await;
        assert_eq!(p.error_code, 0, "fetch v{fv}");
        assert_eq!(p.high_watermark, next + 3);
        let got = p.records.clone().unwrap();
        // The stored batch is the producer's batch with only the base offset (and leader
        // epoch, outside the CRC) changed.
        assert_eq!(&got[16..], &sent[16..], "v{pv}/v{fv}");
        assert_eq!(i64::from_be_bytes(got[0..8].try_into().unwrap()), next);
        assert_eq!(records_of(&p), vec![format!("p{pv}-0"), format!("p{pv}-1"), format!("p{pv}-2")]);
        next += 3;
    }
    // Compressed batches pass through untouched too.
    for codec in [kafka_protocol::records::Compression::Gzip, kafka_protocol::records::Compression::Snappy, kafka_protocol::records::Compression::Lz4, kafka_protocol::records::Compression::Zstd] {
        let sent = stream_broker::records::encode(&values("z", 20), codec, now_ms()).unwrap();
        let pr = produce(&mut c, 9, "data", 0, sent.clone()).await;
        assert_eq!(pr.error_code, 0, "{codec:?}");
        let p = fetch(&mut c, 12, &tb, "data", 0, pr.base_offset, 0).await;
        assert_eq!(&p.records.as_ref().unwrap()[16..sent.len()], &sent[16..], "{codec:?}");
        assert_eq!(records_of(&p).len(), 20);
    }
    // Unknown topic ids and partitions.
    let mut bad = produce_req("", 0, plain("x", 1), -1, None);
    bad.topic_data[0].topic_id = Uuid::from_u128(42);
    let r = c.call(13, &bad).await;
    assert_eq!(r.responses[0].partition_responses[0].error_code, code(ResponseError::UnknownTopicId));
    assert_eq!(produce(&mut c, 9, "data", 5, plain("x", 1)).await.error_code, code(ResponseError::UnknownTopicOrPartition));
    let r = c.call(13, &fetch_req("", Uuid::from_u128(42), 0, 0, 0)).await;
    assert_eq!(r.responses[0].partitions[0].error_code, code(ResponseError::UnknownTopicId));
    // Corrupt batches are rejected.
    let mut corrupt = plain("c", 2).to_vec();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    assert_eq!(produce(&mut c, 9, "data", 0, Bytes::from(corrupt)).await.error_code, code(ResponseError::CorruptMessage));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_versions_and_auto_create() {
    let tb = TestBroker::start("proto-meta").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "m", 3, &[]).await;
    for v in [0, 1, 4, 9, 10, 12, 13] {
        let topics = Some(vec![metadata_request::MetadataRequestTopic::default().with_name(Some(tn("m")))]);
        let r = c.call(v, &MetadataRequest::default().with_topics(topics).with_allow_auto_topic_creation(true)).await;
        assert_eq!(r.brokers.len(), 1, "v{v}");
        assert_eq!(r.brokers[0].port, tb.addr.rsplit(':').next().unwrap().parse::<i32>().unwrap());
        assert_eq!(r.topics[0].error_code, 0);
        assert_eq!(r.topics[0].partitions.len(), 3);
        if v >= 10 {
            assert_eq!(r.topics[0].topic_id, topic_id(&tb, "m"));
        }
        if v >= 2 {
            assert_eq!(r.cluster_id.as_deref(), Some(tb.broker.find_stream("test").unwrap().cluster_id.as_str()));
        }
    }
    // v4+ honours allow_auto_topic_creation; v0-3 always auto-create.
    let req = |name: &str, allow: bool| MetadataRequest::default().with_topics(Some(vec![metadata_request::MetadataRequestTopic::default().with_name(Some(tn(name)))])).with_allow_auto_topic_creation(allow);
    let r = c.call(9, &req("nope", false)).await;
    assert_eq!(r.topics[0].error_code, code(ResponseError::UnknownTopicOrPartition));
    let r = c.call(9, &req("yes", true)).await;
    assert_eq!(r.topics[0].error_code, 0);
    let r = c.call(0, &req("old-client", true)).await;
    assert_eq!(r.topics[0].error_code, 0);
    let r = c.call(9, &req("bad/name", true)).await;
    assert_eq!(r.topics[0].error_code, code(ResponseError::InvalidTopicException));
    // All topics.
    let r = c.call(12, &MetadataRequest::default().with_topics(None)).await;
    let mut names: Vec<String> = r.topics.iter().map(|t| t.name.as_ref().unwrap().to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["m", "old-client", "yes"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_offsets_every_special_timestamp() {
    let tb = TestBroker::start("proto-offsets").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "o", 1, &[]).await;
    for i in 0..10 {
        let recs = vec![NewRecord { value: Some(Bytes::from(format!("{i}"))), timestamp: Some(1000 * (i + 1)), ..Default::default() }];
        produce(&mut c, 9, "o", 0, batch::build(&recs, BatchOptions::default(), now_ms())).await;
    }
    let q = |ts: i64| {
        ListOffsetsRequest::default().with_replica_id(BrokerId(-1)).with_topics(vec![
            list_offsets_request::ListOffsetsTopic::default()
                .with_name(tn("o"))
                .with_partitions(vec![list_offsets_request::ListOffsetsPartition::default().with_partition_index(0).with_timestamp(ts).with_current_leader_epoch(-1)]),
        ])
    };
    for v in [1, 4, 7, 8, 9, 10] {
        let get = |r: ListOffsetsResponse| r.topics[0].partitions[0].clone();
        assert_eq!(get(c.call(v, &q(-1)).await).offset, 10, "latest v{v}");
        assert_eq!(get(c.call(v, &q(-2)).await).offset, 0, "earliest v{v}");
        let p = get(c.call(v, &q(4500)).await);
        assert_eq!((p.offset, p.timestamp), (4, 5000), "by time v{v}");
        assert_eq!(get(c.call(v, &q(99_999)).await).offset, -1, "after the end v{v}");
        if v >= 7 {
            let p = get(c.call(v, &q(-3)).await);
            assert_eq!((p.offset, p.timestamp), (9, 10_000), "max timestamp v{v}");
        }
        if v >= 8 {
            assert_eq!(get(c.call(v, &q(-4)).await).offset, 0, "earliest local v{v}");
        }
    }
    let r = c.call(1, &q(-1).with_topics(vec![list_offsets_request::ListOffsetsTopic::default().with_name(tn("missing")).with_partitions(vec![list_offsets_request::ListOffsetsPartition::default()])])).await;
    assert_eq!(r.topics[0].partitions[0].error_code, code(ResponseError::UnknownTopicOrPartition));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotent_dedup_sequence_and_epochs() {
    let tb = TestBroker::start("proto-idem").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "i", 1, &[]).await;
    let init = c.call(4, &InitProducerIdRequest::default().with_transaction_timeout_ms(60_000).with_producer_id(ProducerId(-1)).with_producer_epoch(-1)).await;
    assert_eq!(init.error_code, 0);
    let (pid, epoch) = (init.producer_id.0, init.producer_epoch);
    assert!(pid >= 1000);

    let b0 = producer_batch("a", 5, pid, epoch, 0, false);
    assert_eq!(produce(&mut c, 9, "i", 0, b0.clone()).await.base_offset, 0);
    let b1 = producer_batch("b", 5, pid, epoch, 5, false);
    assert_eq!(produce(&mut c, 9, "i", 0, b1.clone()).await.base_offset, 5);
    // Retries of either of the last batches are acknowledged with their original offsets.
    let dup = produce(&mut c, 9, "i", 0, b0).await;
    assert_eq!((dup.error_code, dup.base_offset), (0, 0));
    let dup = produce(&mut c, 9, "i", 0, b1).await;
    assert_eq!((dup.error_code, dup.base_offset), (0, 5));
    assert_eq!(fetch(&mut c, 12, &tb, "i", 0, 0, 0).await.high_watermark, 10);
    // A gap is out of order.
    let gap = produce(&mut c, 9, "i", 0, producer_batch("c", 1, pid, epoch, 20, false)).await;
    assert_eq!(gap.error_code, code(ResponseError::OutOfOrderSequenceNumber));
    // A bumped epoch restarts sequences; the old epoch is then fenced.
    assert_eq!(produce(&mut c, 9, "i", 0, producer_batch("d", 1, pid, epoch + 1, 0, false)).await.error_code, 0);
    let old = produce(&mut c, 9, "i", 0, producer_batch("e", 1, pid, epoch, 10, false)).await;
    assert_eq!(old.error_code, code(ResponseError::InvalidProducerEpoch));
    // Like Kafka since KIP-360, a producer the log has never seen may start mid-sequence (its
    // state could have expired); from then on it's tracked.
    assert_eq!(produce(&mut c, 9, "i", 0, producer_batch("f", 1, pid + 7, 0, 3, false)).await.error_code, 0);
    let gap = produce(&mut c, 9, "i", 0, producer_batch("g", 1, pid + 7, 0, 9, false)).await;
    assert_eq!(gap.error_code, code(ResponseError::OutOfOrderSequenceNumber));
}

async fn find_coordinator(c: &mut Conn, key: &str, key_type: i8) -> (i32, i32) {
    let r = c.call(3, &FindCoordinatorRequest::default().with_key(s(key)).with_key_type(key_type)).await;
    assert_eq!(r.error_code, 0);
    (r.node_id.0, r.port)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transactions_on_the_wire() {
    let tb = TestBroker::start("proto-txn").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "t", 2, &[]).await;
    let (node, port) = find_coordinator(&mut c, "tx", 1).await;
    assert_eq!((node, port.to_string()), (1, tb.addr.rsplit(':').next().unwrap().to_string()));
    // v4+ batched lookups.
    let r = c.call(4, &FindCoordinatorRequest::default().with_key_type(1).with_coordinator_keys(vec![s("a"), s("b")])).await;
    assert_eq!(r.coordinators.len(), 2);

    let init = c.call(3, &InitProducerIdRequest::default().with_transactional_id(Some(TransactionalId(s("tx")))).with_transaction_timeout_ms(30_000).with_producer_id(ProducerId(-1)).with_producer_epoch(-1)).await;
    assert_eq!(init.error_code, 0);
    let (pid, epoch) = (init.producer_id, init.producer_epoch);

    // Transactional data for a partition that isn't in the transaction is refused (KIP-890).
    let unverified = c.call(9, &produce_req("t", 0, producer_batch("x", 1, pid.0, epoch, 0, true), -1, Some("tx"))).await;
    assert_ne!(unverified.responses[0].partition_responses[0].error_code, 0);

    let add = |parts: Vec<i32>| {
        AddPartitionsToTxnRequest::default()
            .with_v3_and_below_transactional_id(TransactionalId(s("tx")))
            .with_v3_and_below_producer_id(pid)
            .with_v3_and_below_producer_epoch(epoch)
            .with_v3_and_below_topics(vec![add_partitions_to_txn_request::AddPartitionsToTxnTopic::default().with_name(tn("t")).with_partitions(parts)])
    };
    let r = c.call(3, &add(vec![0, 1])).await;
    assert!(r.results_by_topic_v3_and_below.iter().all(|t| t.results_by_partition.iter().all(|p| p.partition_error_code == 0)));
    let r = c.call(9, &produce_req("t", 0, producer_batch("aborted", 3, pid.0, epoch, 0, true), -1, Some("tx"))).await;
    assert_eq!(r.responses[0].partition_responses[0].error_code, 0);

    // While open, read_committed stops at the LSO.
    let p = fetch(&mut c, 12, &tb, "t", 0, 0, 1).await;
    assert_eq!((p.high_watermark, p.last_stable_offset), (3, 0));
    assert!(p.records.as_ref().is_none_or(|r| r.is_empty()));
    let end = c.call(3, &EndTxnRequest::default().with_transactional_id(TransactionalId(s("tx"))).with_producer_id(pid).with_producer_epoch(epoch).with_committed(false)).await;
    assert_eq!(end.error_code, 0);
    // After the abort the data is visible with its aborted range, plus the marker.
    let p = fetch(&mut c, 12, &tb, "t", 0, 0, 1).await;
    assert_eq!((p.high_watermark, p.last_stable_offset), (4, 4));
    let aborted = p.aborted_transactions.clone().unwrap_or_default();
    assert_eq!(aborted.len(), 1);
    assert_eq!((aborted[0].producer_id, aborted[0].first_offset), (pid, 0));
    let headers: Vec<_> = batch::headers(p.records.as_ref().unwrap()).map(|(_, h)| h).collect();
    assert!(headers.last().unwrap().is_control());

    // Commit a second transaction.
    c.call(3, &add(vec![1])).await;
    c.call(9, &produce_req("t", 1, producer_batch("committed", 2, pid.0, epoch, 0, true), -1, Some("tx"))).await;
    let end = c.call(3, &EndTxnRequest::default().with_transactional_id(TransactionalId(s("tx"))).with_producer_id(pid).with_producer_epoch(epoch).with_committed(true)).await;
    assert_eq!(end.error_code, 0);
    let p = fetch(&mut c, 12, &tb, "t", 1, 0, 1).await;
    assert!(p.aborted_transactions.clone().unwrap_or_default().is_empty());
    assert_eq!(records_of(&p), vec!["committed-0", "committed-1"]);

    // Re-initialising bumps the epoch and fences the old producer.
    let again = c.call(3, &InitProducerIdRequest::default().with_transactional_id(Some(TransactionalId(s("tx")))).with_transaction_timeout_ms(30_000).with_producer_id(ProducerId(-1)).with_producer_epoch(-1)).await;
    assert_eq!(again.producer_id, pid);
    assert!(again.producer_epoch > epoch);
    let stale = c.call(3, &EndTxnRequest::default().with_transactional_id(TransactionalId(s("tx"))).with_producer_id(pid).with_producer_epoch(epoch).with_committed(true)).await;
    assert!(stale.error_code == code(ResponseError::ProducerFenced) || stale.error_code == code(ResponseError::InvalidProducerEpoch), "{}", stale.error_code);

    let list = c.call(1, &ListTransactionsRequest::default().with_duration_filter(-1)).await;
    assert_eq!(list.transaction_states.len(), 1);
    let d = c.call(0, &DescribeTransactionsRequest::default().with_transactional_ids(vec![TransactionalId(s("tx"))])).await;
    assert_eq!(d.transaction_states[0].producer_id, pid);
}

fn consumer_meta(topics: &[&str]) -> Bytes {
    // ConsumerProtocolSubscription v0: version, topics, user data.
    let mut b = bytes::BytesMut::new();
    use bytes::BufMut;
    b.put_i16(0);
    b.put_i32(topics.len() as i32);
    for t in topics {
        b.put_i16(t.len() as i16);
        b.put_slice(t.as_bytes());
    }
    b.put_i32(-1);
    b.freeze()
}

fn join_req(group: &str, member: &str, instance: Option<&str>) -> JoinGroupRequest {
    JoinGroupRequest::default()
        .with_group_id(GroupId(s(group)))
        .with_session_timeout_ms(10_000)
        .with_rebalance_timeout_ms(5_000)
        .with_member_id(s(member))
        .with_group_instance_id(instance.map(s))
        .with_protocol_type(s("consumer"))
        .with_protocols(vec![join_group_request::JoinGroupRequestProtocol::default().with_name(s("range")).with_metadata(consumer_meta(&["g"]))])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn classic_group_protocol_paths() {
    let tb = TestBroker::start("proto-groups").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "g", 2, &[]).await;
    let (node, _) = find_coordinator(&mut c, "grp", 0).await;
    assert_eq!(node, 1);

    // KIP-394: the first join without a member id gets one back.
    let r = c.call(9, &join_req("grp", "", None)).await;
    assert_eq!(r.error_code, code(ResponseError::MemberIdRequired));
    let member = r.member_id.to_string();
    assert!(!member.is_empty());
    let r = c.call(9, &join_req("grp", &member, None)).await;
    assert_eq!(r.error_code, 0);
    assert_eq!(r.leader.to_string(), member);
    let generation = r.generation_id;
    assert_eq!(r.members.len(), 1);
    let sync = c
        .call(
            5,
            &SyncGroupRequest::default()
                .with_group_id(GroupId(s("grp")))
                .with_generation_id(generation)
                .with_member_id(s(&member))
                .with_protocol_type(Some(s("consumer")))
                .with_protocol_name(Some(s("range")))
                .with_assignments(vec![sync_group_request::SyncGroupRequestAssignment::default().with_member_id(s(&member)).with_assignment(Bytes::from_static(b"assignment"))]),
        )
        .await;
    assert_eq!(sync.error_code, 0);
    assert_eq!(&sync.assignment[..], b"assignment");
    let hb = |gen_id: i32, m: &str| HeartbeatRequest::default().with_group_id(GroupId(s("grp"))).with_generation_id(gen_id).with_member_id(s(m));
    assert_eq!(c.call(4, &hb(generation, &member)).await.error_code, 0);
    assert_eq!(c.call(4, &hb(generation + 5, &member)).await.error_code, code(ResponseError::IllegalGeneration));
    assert_eq!(c.call(4, &hb(generation, "stranger")).await.error_code, code(ResponseError::UnknownMemberId));

    // Commits are fenced by generation and member.
    let commit = |gen_id: i32, m: &str| {
        OffsetCommitRequest::default().with_group_id(GroupId(s("grp"))).with_generation_id_or_member_epoch(gen_id).with_member_id(s(m)).with_topics(vec![
            offset_commit_request::OffsetCommitRequestTopic::default()
                .with_name(tn("g"))
                .with_partitions(vec![offset_commit_request::OffsetCommitRequestPartition::default().with_partition_index(0).with_committed_offset(7).with_committed_metadata(Some(s("meta")))]),
        ])
    };
    assert_eq!(c.call(8, &commit(generation, &member)).await.topics[0].partitions[0].error_code, 0);
    assert_eq!(c.call(8, &commit(generation + 1, &member)).await.topics[0].partitions[0].error_code, code(ResponseError::IllegalGeneration));
    let fetch_off = c
        .call(
            7,
            &OffsetFetchRequest::default().with_group_id(GroupId(s("grp"))).with_topics(Some(vec![offset_fetch_request::OffsetFetchRequestTopic::default().with_name(tn("g")).with_partition_indexes(vec![0, 1])])),
        )
        .await;
    let parts = &fetch_off.topics[0].partitions;
    assert_eq!((parts[0].committed_offset, parts[0].metadata.as_deref()), (7, Some("meta")));
    assert_eq!(parts[1].committed_offset, -1);

    // A second member joining puts the first into a rebalance.
    // Static members (KIP-345) skip MEMBER_ID_REQUIRED; the join parks until the rebalance
    // completes.
    let mut c2 = Conn::connect(&tb.addr).await;
    let join2 = tokio::spawn(async move {
        let r = c2.call(9, &join_req("grp", "", Some("static-1"))).await;
        (c2, r)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let e = c.call(4, &hb(generation, &member)).await.error_code;
        if e == code(ResponseError::RebalanceInProgress) {
            break;
        }
        assert!(Instant::now() < deadline, "no rebalance signalled ({e})");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let r = c.call(9, &join_req("grp", &member, None)).await;
    assert_eq!(r.error_code, 0);
    assert_eq!(r.members.len(), 2, "the leader sees both members");
    let (_c2, r2) = join2.await.unwrap();
    assert_eq!(r2.error_code, 0);
    assert_eq!(r2.generation_id, r.generation_id);

    let d = c.call(5, &DescribeGroupsRequest::default().with_groups(vec![GroupId(s("grp"))])).await;
    assert_eq!(d.groups[0].members.len(), 2);
    assert_eq!(d.groups[0].protocol_type.to_string(), "consumer");
    let l = c.call(5, &ListGroupsRequest::default().with_states_filter(vec![s("CompletingRebalance"), s("Stable")])).await;
    assert_eq!(l.groups.len(), 1);
    let leave = c.call(5, &LeaveGroupRequest::default().with_group_id(GroupId(s("grp"))).with_members(vec![leave_group_request::MemberIdentity::default().with_member_id(s(&member))])).await;
    assert_eq!(leave.error_code, 0);
    let del = c.call(2, &DeleteGroupsRequest::default().with_groups_names(vec![GroupId(s("grp"))])).await;
    assert_eq!(del.results[0].error_code, code(ResponseError::NonEmptyGroup));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_long_poll_and_errors() {
    let tb = TestBroker::start("proto-fetch").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "f", 1, &[]).await;
    produce(&mut c, 9, "f", 0, plain("x", 2)).await;
    let id = topic_id(&tb, "f");

    let p = fetch(&mut c, 12, &tb, "f", 0, 99, 0).await;
    assert_eq!(p.error_code, code(ResponseError::OffsetOutOfRange));
    assert_eq!((p.high_watermark, p.log_start_offset), (2, 0));
    let r = c.call(12, &fetch_req("f", id, 0, 0, 0).with_session_id(1234).with_session_epoch(1)).await;
    assert_eq!(r.error_code, code(ResponseError::FetchSessionIdNotFound));

    // Parks until max_wait when nothing is there...
    let started = Instant::now();
    let r = c.call(12, &fetch_req("f", id, 0, 2, 0).with_max_wait_ms(400).with_min_bytes(1)).await;
    assert!(started.elapsed() >= Duration::from_millis(350));
    assert!(r.responses[0].partitions[0].records.as_ref().is_none_or(|b| b.is_empty()));
    // ...and wakes as soon as data arrives.
    let addr = tb.addr.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut p = Conn::connect(&addr).await;
        produce(&mut p, 9, "f", 0, plain("late", 1)).await;
    });
    let started = Instant::now();
    let r = c.call(12, &fetch_req("f", id, 0, 2, 0).with_max_wait_ms(10_000).with_min_bytes(1)).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(records_of(&r.responses[0].partitions[0]), vec!["late-0"]);
    // min_bytes larger than what's there waits for max_wait.
    let started = Instant::now();
    c.call(12, &fetch_req("f", id, 0, 0, 0).with_max_wait_ms(300).with_min_bytes(1 << 20)).await;
    assert!(started.elapsed() >= Duration::from_millis(250));
    // max_bytes still returns at least one whole batch.
    let r = c.call(12, &fetch_req("f", id, 0, 0, 0).with_max_bytes(1)).await;
    assert_eq!(records_of(&r.responses[0].partitions[0]).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_requests_answer_in_order_and_acks_zero_is_silent() {
    let tb = TestBroker::start("proto-pipeline").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "pp", 1, &[]).await;
    let id = topic_id(&tb, "pp");
    let mut ids = vec![];
    for i in 0..60 {
        match i % 3 {
            // acks=0: no response at all.
            0 => {
                c.send(9, &produce_req("pp", 0, plain("zero", 1), 0, None)).await;
            }
            1 => ids.push(("produce", c.send(9, &produce_req("pp", 0, plain("one", 1), -1, None)).await)),
            _ => ids.push(("fetch", c.send(12, &fetch_req("pp", id, 0, 0, 0).with_max_wait_ms(50)).await)),
        }
    }
    for (kind, want) in ids {
        let got = match kind {
            "produce" => c.recv::<ProduceRequest>(9).await.0,
            _ => c.recv::<FetchRequest>(12).await.0,
        };
        assert_eq!(got, want, "responses come back in request order");
    }
    let p = fetch(&mut c, 12, &tb, "pp", 0, 0, 0).await;
    assert_eq!(p.high_watermark, 40);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_frames_close_the_connection() {
    let tb = TestBroker::start_with("proto-frames", |c| c.max_request_bytes = 1 << 20).await;
    let mut c = Conn::connect(&tb.addr).await;
    c.send_frame(&vec![0u8; 2 << 20]).await;
    assert!(c.closed_within(Duration::from_secs(5)).await, "oversized frame");
    let mut c = Conn::connect(&tb.addr).await;
    c.send_frame(&[0x7f, 0x7f, 0, 0, 0, 0, 0, 1, 0, 0]).await;
    assert!(c.closed_within(Duration::from_secs(5)).await, "unknown api key");
    let mut c = Conn::connect(&tb.addr).await;
    c.send_frame(&[0, 3, 0, 9, 0, 0, 0, 1, 0xff, 0xff, 1, 2, 3]).await;
    assert!(c.closed_within(Duration::from_secs(5)).await, "undecodable body");
}

/// Counts and lengths come from the peer: one that lies has to cost an error, not an allocation of
/// what it claims (`common::Capped` aborts the test on anything over 1 GiB).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lying_counts_and_lengths_are_rejected_without_allocating_them() {
    let tb = TestBroker::start("proto-lengths").await;
    let header = |api: i16, version: i16| -> Vec<u8> { [&api.to_be_bytes()[..], &version.to_be_bytes(), &1i32.to_be_bytes(), &(-1i16).to_be_bytes()].concat() };
    // Metadata v4: a topics array claiming i32::MAX entries.
    let array = [header(3, 4), i32::MAX.to_be_bytes().to_vec()].concat();
    // Metadata v12 (flexible): no header tags, then a compact array claiming u32::MAX - 1 entries.
    let compact = [header(3, 12), vec![0, 0xff, 0xff, 0xff, 0xff, 0x0f]].concat();
    // SaslAuthenticate v0: auth bytes claiming i32::MAX bytes.
    let bytes = [header(36, 0), i32::MAX.to_be_bytes().to_vec()].concat();
    for (what, frame) in [("array", array), ("compact array", compact), ("bytes", bytes)] {
        let mut c = Conn::connect(&tb.addr).await;
        c.send_frame(&frame).await;
        assert!(c.closed_within(Duration::from_secs(5)).await, "{what}");
    }

    // A batch with a valid CRC whose header claims far more records than it holds, on a compacted
    // topic, where the broker decodes records to check their keys.
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "lying", 1, &[("cleanup.policy", "compact")]).await;
    let recs: Vec<NewRecord> = (0..2).map(|i| NewRecord { key: Some(Bytes::from(format!("k{i}"))), value: Some(Bytes::from_static(b"v")), ..Default::default() }).collect();
    let mut b = BytesMut::from(&batch::build(&recs, BatchOptions::default(), now_ms())[..]);
    b[23..27].copy_from_slice(&(i32::MAX - 1).to_be_bytes()); // last offset delta
    b[57..61].copy_from_slice(&i32::MAX.to_be_bytes()); // record count
    let crc = batch::compute_crc(&b);
    b[17..21].copy_from_slice(&crc.to_be_bytes());
    assert_ne!(produce(&mut c, 9, "lying", 0, b.freeze()).await.error_code, 0);
    assert_eq!(produce(&mut c, 9, "lying", 0, batch::build(&recs, BatchOptions::default(), now_ms())).await.error_code, 0, "still serving");
}

async fn scram_login(c: &mut Conn, mech: Mechanism, name: &str, user: &str, pass: &str) -> i16 {
    let h = c.call(1, &SaslHandshakeRequest::default().with_mechanism(s(name))).await;
    assert_eq!(h.error_code, 0);
    assert!(h.mechanisms.iter().any(|m| m.as_str() == name));
    let mut scram = Scram::new(mech, user, pass);
    let first = c.call(2, &SaslAuthenticateRequest::default().with_auth_bytes(Bytes::from(scram.first()))).await;
    if first.error_code != 0 {
        return first.error_code;
    }
    let Some(last) = scram.last(&first.auth_bytes) else { return -1 };
    let fin = c.call(2, &SaslAuthenticateRequest::default().with_auth_bytes(Bytes::from(last))).await;
    if fin.error_code == 0 {
        assert!(scram.verify(&fin.auth_bytes), "server signature");
    }
    fin.error_code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sasl_flows_and_tenant_isolation() {
    // No default stream: authentication is mandatory.
    let tb = TestBroker::start_with("proto-sasl", |c| c.default_stream = Some("-".into())).await;
    let (a, ca, sa) = tb.broker.create_stream("tenant-a", Default::default()).unwrap();
    let (_b, cb, sb) = tb.broker.create_stream("tenant-b", Default::default()).unwrap();

    // Before authenticating only ApiVersions and SASL are allowed.
    let mut c = Conn::connect(&tb.addr).await;
    c.call(3, &ApiVersionsRequest::default()).await;
    c.send(9, &MetadataRequest::default()).await;
    assert!(c.recv_frame().await.is_none(), "metadata before auth closes");

    // SCRAM-SHA-256 and -512 through SaslAuthenticate.
    let mut c = Conn::connect(&tb.addr).await;
    assert_eq!(scram_login(&mut c, Mechanism::Scram256, "SCRAM-SHA-256", &ca.username, &sa).await, 0);
    assert_eq!(create(&mut c, "same-name", 1, &[]).await.error_code, 0);
    produce(&mut c, 9, "same-name", 0, plain("from-a", 1)).await;
    let mut c2 = Conn::connect(&tb.addr).await;
    assert_eq!(scram_login(&mut c2, Mechanism::Scram512, "SCRAM-SHA-512", &cb.username, &sb).await, 0);
    assert_eq!(create(&mut c2, "same-name", 1, &[]).await.error_code, 0, "names are per stream");
    produce(&mut c2, 9, "same-name", 0, plain("from-b", 1)).await;
    let id_a = a.topic("same-name").unwrap().id;
    let r = c.call(12, &fetch_req("same-name", id_a, 0, 0, 0)).await;
    assert_eq!(records_of(&r.responses[0].partitions[0]), vec!["from-a-0"]);
    let md = c2.call(12, &MetadataRequest::default().with_topics(None)).await;
    assert_eq!(md.topics.len(), 1);
    assert_ne!(md.cluster_id.as_deref(), Some(a.cluster_id.as_str()), "each stream is its own cluster");

    // Wrong password: an error, then the connection closes.
    let mut c3 = Conn::connect(&tb.addr).await;
    assert_eq!(scram_login(&mut c3, Mechanism::Scram256, "SCRAM-SHA-256", &ca.username, "nope").await, code(ResponseError::SaslAuthenticationFailed));
    c3.send(9, &MetadataRequest::default()).await;
    assert!(c3.recv_frame().await.is_none());
    // Unknown mechanism.
    let mut c4 = Conn::connect(&tb.addr).await;
    let h = c4.call(1, &SaslHandshakeRequest::default().with_mechanism(s("GSSAPI"))).await;
    assert_eq!(h.error_code, code(ResponseError::UnsupportedSaslMechanism));

    // SASL v0: a handshake at v0, then the raw PLAIN token in its own frame.
    let mut c5 = Conn::connect(&tb.addr).await;
    let h = c5.call(0, &SaslHandshakeRequest::default().with_mechanism(s("PLAIN"))).await;
    assert_eq!(h.error_code, 0);
    c5.send_frame(format!("\0{}\0{}", cb.username, sb).as_bytes()).await;
    let reply = c5.recv_frame().await.expect("raw SASL reply");
    assert!(reply.is_empty());
    let md = c5.call(1, &MetadataRequest::default().with_topics(None)).await;
    assert_eq!(md.topics.len(), 1, "authenticated into tenant b");
    // PLAIN through SaslAuthenticate, wrong password.
    let mut c6 = Conn::connect(&tb.addr).await;
    c6.call(1, &SaslHandshakeRequest::default().with_mechanism(s("PLAIN"))).await;
    let r = c6.call(1, &SaslAuthenticateRequest::default().with_auth_bytes(Bytes::from(format!("\0{}\0bad", ca.username)))).await;
    assert_eq!(r.error_code, code(ResponseError::SaslAuthenticationFailed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_apis_on_the_wire() {
    let tb = TestBroker::start("proto-admin").await;
    let mut c = Conn::connect(&tb.addr).await;
    let t = create(&mut c, "adm", 2, &[("cleanup.policy", "delete"), ("retention.ms", "60000")]).await;
    assert_eq!(t.error_code, 0);
    assert_eq!(t.topic_id, topic_id(&tb, "adm"));
    assert_eq!(t.num_partitions, 2);
    let configs = t.configs.unwrap();
    assert_eq!(configs.iter().find(|c| c.name.as_str() == "retention.ms").unwrap().value.as_deref(), Some("60000"));
    assert_eq!(create(&mut c, "adm", 1, &[]).await.error_code, code(ResponseError::TopicAlreadyExists));
    assert_eq!(create(&mut c, "bad", 1, &[("retention.ms", "x")]).await.error_code, code(ResponseError::InvalidConfig));
    assert_eq!(create(&mut c, "bad", 1, &[("nope", "1")]).await.error_code, code(ResponseError::InvalidConfig));
    assert_eq!(create(&mut c, "bad", 0, &[]).await.error_code, code(ResponseError::InvalidPartitions));
    let rf = create_topics_request::CreatableTopic::default().with_name(tn("rf")).with_num_partitions(1).with_replication_factor(5);
    let r = c.call(7, &CreateTopicsRequest::default().with_topics(vec![rf])).await;
    assert_eq!(r.topics[0].error_code, code(ResponseError::InvalidReplicationFactor));
    let vo = create_topics_request::CreatableTopic::default().with_name(tn("dry")).with_num_partitions(1).with_replication_factor(1);
    let r = c.call(7, &CreateTopicsRequest::default().with_topics(vec![vo]).with_validate_only(true)).await;
    assert_eq!(r.topics[0].error_code, 0);
    assert_eq!(topic_id(&tb, "dry"), Uuid::nil(), "validate_only creates nothing");

    // Incremental alter: APPEND/SUBTRACT on a list config, SET and DELETE.
    let inc = |ops: Vec<(&str, i8, Option<&str>)>| {
        IncrementalAlterConfigsRequest::default().with_resources(vec![
            incremental_alter_configs_request::AlterConfigsResource::default().with_resource_type(2).with_resource_name(s("adm")).with_configs(
                ops.into_iter().map(|(n, op, v)| incremental_alter_configs_request::AlterableConfig::default().with_name(s(n)).with_config_operation(op).with_value(v.map(s))).collect(),
            ),
        ])
    };
    assert_eq!(c.call(1, &inc(vec![("cleanup.policy", 2, Some("compact"))])).await.responses[0].error_code, 0);
    let describe = |names: Option<Vec<&str>>| {
        DescribeConfigsRequest::default().with_include_synonyms(true).with_include_documentation(true).with_resources(vec![
            describe_configs_request::DescribeConfigsResource::default().with_resource_type(2).with_resource_name(s("adm")).with_configuration_keys(names.map(|n| n.into_iter().map(s).collect())),
        ])
    };
    let get = |r: &DescribeConfigsResponse, name: &str| r.results[0].configs.iter().find(|c| c.name.as_str() == name).and_then(|c| c.value.as_ref()).map(|v| v.to_string());
    let r = c.call(4, &describe(Some(vec!["cleanup.policy"]))).await;
    assert_eq!(r.results[0].configs.len(), 1);
    assert_eq!(get(&r, "cleanup.policy").as_deref(), Some("compact,delete"));
    c.call(1, &inc(vec![("cleanup.policy", 3, Some("delete")), ("retention.ms", 1, None), ("segment.ms", 0, Some("3600000"))])).await;
    let r = c.call(4, &describe(None)).await;
    assert_eq!(get(&r, "cleanup.policy").as_deref(), Some("compact"));
    assert_eq!(get(&r, "retention.ms").as_deref(), Some("604800000"), "back to the default");
    assert_eq!(get(&r, "segment.ms").as_deref(), Some("3600000"));
    let retention = r.results[0].configs.iter().find(|c| c.name.as_str() == "retention.ms").unwrap();
    assert_eq!(retention.config_source, 5, "DEFAULT_CONFIG");
    assert!(retention.documentation.is_some());
    assert_eq!(c.call(1, &inc(vec![("retention.ms", 2, Some("5"))])).await.responses[0].error_code, code(ResponseError::InvalidConfig), "APPEND needs a list");
    // Legacy AlterConfigs replaces everything and validates.
    let alter = |v: &str| {
        AlterConfigsRequest::default().with_resources(vec![
            alter_configs_request::AlterConfigsResource::default()
                .with_resource_type(2)
                .with_resource_name(s("adm"))
                .with_configs(vec![alter_configs_request::AlterableConfig::default().with_name(s("retention.ms")).with_value(Some(s(v)))]),
        ])
    };
    assert_eq!(c.call(2, &alter("soon")).await.responses[0].error_code, code(ResponseError::InvalidConfig));
    assert_eq!(c.call(2, &alter("1000")).await.responses[0].error_code, 0);
    let r = c.call(4, &describe(None)).await;
    assert_eq!(get(&r, "cleanup.policy").as_deref(), Some("delete"), "the compact override is gone");
    let broker_cfg = c
        .call(4, &DescribeConfigsRequest::default().with_resources(vec![describe_configs_request::DescribeConfigsResource::default().with_resource_type(4).with_resource_name(s("1"))]))
        .await;
    assert!(broker_cfg.results[0].configs.iter().any(|c| c.name.as_str() == "num.partitions"));

    // Partitions grow online; validate_only doesn't apply.
    let cp = |n: i32, dry: bool| CreatePartitionsRequest::default().with_validate_only(dry).with_topics(vec![create_partitions_request::CreatePartitionsTopic::default().with_name(tn("adm")).with_count(n)]);
    assert_eq!(c.call(3, &cp(6, true)).await.results[0].error_code, 0);
    assert_eq!(tb.broker.find_stream("test").unwrap().topic("adm").unwrap().partition_count(), 2);
    assert_eq!(c.call(3, &cp(6, false)).await.results[0].error_code, 0);
    assert_eq!(tb.broker.find_stream("test").unwrap().topic("adm").unwrap().partition_count(), 6);
    assert_eq!(c.call(3, &cp(3, false)).await.results[0].error_code, code(ResponseError::InvalidPartitions));

    // DeleteRecords, including past the end.
    produce(&mut c, 9, "adm", 0, plain("r", 10)).await;
    let dr = |o: i64| {
        DeleteRecordsRequest::default().with_topics(vec![
            delete_records_request::DeleteRecordsTopic::default().with_name(tn("adm")).with_partitions(vec![delete_records_request::DeleteRecordsPartition::default().with_partition_index(0).with_offset(o)]),
        ])
    };
    let r = c.call(2, &dr(4)).await;
    assert_eq!((r.topics[0].partitions[0].error_code, r.topics[0].partitions[0].low_watermark), (0, 4));
    assert_eq!(c.call(2, &dr(99)).await.topics[0].partitions[0].error_code, code(ResponseError::OffsetOutOfRange));
    let r = c.call(2, &dr(-1)).await;
    assert_eq!(r.topics[0].partitions[0].low_watermark, 10, "-1 means the high watermark");

    // Describe producers / log dirs / leader epochs.
    let lde = c
        .call(
            4,
            &OffsetForLeaderEpochRequest::default().with_topics(vec![
                offset_for_leader_epoch_request::OffsetForLeaderTopic::default()
                    .with_topic(tn("adm"))
                    .with_partitions(vec![offset_for_leader_epoch_request::OffsetForLeaderPartition::default().with_partition(0).with_leader_epoch(0).with_current_leader_epoch(-1)]),
            ]),
        )
        .await;
    assert_eq!(lde.topics[0].partitions[0].end_offset, 10);
    let dirs = c.call(4, &DescribeLogDirsRequest::default().with_topics(None)).await;
    assert!(dirs.results[0].topics.iter().any(|t| t.name.as_str() == "adm"));

    // Delete by name (v1) and by id (v6).
    create(&mut c, "gone", 1, &[]).await;
    let id = topic_id(&tb, "gone");
    let r = c.call(6, &DeleteTopicsRequest::default().with_topics(vec![delete_topics_request::DeleteTopicState::default().with_topic_id(id)])).await;
    assert_eq!(r.responses[0].error_code, 0);
    let r = c.call(1, &DeleteTopicsRequest::default().with_topic_names(vec![tn("adm"), tn("missing")])).await;
    assert_eq!(r.responses.iter().map(|r| r.error_code).collect::<Vec<_>>(), vec![0, code(ResponseError::UnknownTopicOrPartition)]);
    assert!(tb.broker.find_stream("test").unwrap().topics().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quotas_throttle_producers() {
    let tb = TestBroker::start("proto-quota").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "q", 1, &[]).await;
    let entity = vec![alter_client_quotas_request::EntityData::default().with_entity_type(s("user")).with_entity_name(None)];
    let r = c
        .call(
            1,
            &AlterClientQuotasRequest::default().with_entries(vec![alter_client_quotas_request::EntryData::default().with_entity(entity).with_ops(vec![alter_client_quotas_request::OpData::default().with_key(s("producer_byte_rate")).with_value(20_000.0)])]),
        )
        .await;
    assert_eq!(r.entries[0].error_code, 0);
    assert_eq!(tb.broker.find_stream("test").unwrap().settings().ingress_bytes_per_s, 20_000);
    let d = c.call(1, &DescribeClientQuotasRequest::default()).await;
    let values: Vec<_> = d.entries.unwrap().into_iter().flat_map(|e| e.values).collect();
    assert!(values.iter().any(|v| v.key.as_str() == "producer_byte_rate" && v.value == 20_000.0));

    let big = batch::build(&[NewRecord { value: Some(Bytes::from(vec![b'x'; 60_000])), ..Default::default() }], BatchOptions::default(), now_ms());
    let first = c.call(9, &produce_req("q", 0, big.clone(), -1, None)).await;
    assert_eq!(first.responses[0].partition_responses[0].error_code, 0);
    assert!(first.throttle_time_ms > 0, "over quota: the client is told to back off");
    let started = Instant::now();
    c.call(3, &ApiVersionsRequest::default()).await;
    assert!(started.elapsed() >= Duration::from_millis(first.throttle_time_ms as u64 / 2), "the broker stops reading for the throttle time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transactional_offsets_commit_with_the_transaction() {
    let tb = TestBroker::start("proto-txoff").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "src", 1, &[]).await;
    let init = c.call(3, &InitProducerIdRequest::default().with_transactional_id(Some(TransactionalId(s("eos")))).with_transaction_timeout_ms(30_000).with_producer_id(ProducerId(-1)).with_producer_epoch(-1)).await;
    let (pid, epoch) = (init.producer_id, init.producer_epoch);
    let r = c.call(3, &AddOffsetsToTxnRequest::default().with_transactional_id(TransactionalId(s("eos"))).with_producer_id(pid).with_producer_epoch(epoch).with_group_id(GroupId(s("eos-group")))).await;
    assert_eq!(r.error_code, 0);
    let commit = TxnOffsetCommitRequest::default()
        .with_transactional_id(TransactionalId(s("eos")))
        .with_group_id(GroupId(s("eos-group")))
        .with_producer_id(pid)
        .with_producer_epoch(epoch)
        .with_generation_id(-1)
        .with_topics(vec![
            txn_offset_commit_request::TxnOffsetCommitRequestTopic::default()
                .with_name(tn("src"))
                .with_partitions(vec![txn_offset_commit_request::TxnOffsetCommitRequestPartition::default().with_partition_index(0).with_committed_offset(42).with_committed_leader_epoch(-1)]),
        ]);
    assert_eq!(c.call(3, &commit).await.topics[0].partitions[0].error_code, 0);
    let fetch_offsets = |stable: bool| {
        OffsetFetchRequest::default()
            .with_group_id(GroupId(s("eos-group")))
            .with_require_stable(stable)
            .with_topics(Some(vec![offset_fetch_request::OffsetFetchRequestTopic::default().with_name(tn("src")).with_partition_indexes(vec![0])]))
    };
    let r = c.call(7, &fetch_offsets(true)).await;
    assert_eq!(r.topics[0].partitions[0].error_code, code(ResponseError::UnstableOffsetCommit), "pending until the transaction ends");
    assert_eq!(c.call(7, &fetch_offsets(false)).await.topics[0].partitions[0].committed_offset, -1);
    c.call(3, &EndTxnRequest::default().with_transactional_id(TransactionalId(s("eos"))).with_producer_id(pid).with_producer_epoch(epoch).with_committed(true)).await;
    assert_eq!(c.call(7, &fetch_offsets(true)).await.topics[0].partitions[0].committed_offset, 42);
}


/// Requests with real content for the main APIs, at every supported version, against a broker
/// with topics, data, committed offsets and a finished transaction. Any response field set
/// outside its version range would fail to encode and close the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn populated_requests_at_every_version() {
    let tb = TestBroker::start("proto-every").await;
    let mut c = Conn::connect(&tb.addr).await;
    create(&mut c, "pop", 2, &[("retention.ms", "3600000")]).await;
    for p in 0..2 {
        produce(&mut c, 9, "pop", p, plain("x", 5)).await;
    }
    let commit = OffsetCommitRequest::default().with_group_id(GroupId(s("popg"))).with_generation_id_or_member_epoch(-1).with_topics(vec![
        offset_commit_request::OffsetCommitRequestTopic::default().with_name(tn("pop")).with_partitions(vec![offset_commit_request::OffsetCommitRequestPartition::default().with_partition_index(0).with_committed_offset(3)]),
    ]);
    c.call(8, &commit).await;
    let init = c.call(3, &InitProducerIdRequest::default().with_transactional_id(Some(TransactionalId(s("poptx")))).with_transaction_timeout_ms(30_000).with_producer_id(ProducerId(-1)).with_producer_epoch(-1)).await;
    let (pid, epoch) = (init.producer_id, init.producer_epoch);
    c.call(
        3,
        &AddPartitionsToTxnRequest::default()
            .with_v3_and_below_transactional_id(TransactionalId(s("poptx")))
            .with_v3_and_below_producer_id(pid)
            .with_v3_and_below_producer_epoch(epoch)
            .with_v3_and_below_topics(vec![add_partitions_to_txn_request::AddPartitionsToTxnTopic::default().with_name(tn("pop")).with_partitions(vec![1])]),
    )
    .await;
    c.call(9, &produce_req("pop", 1, producer_batch("t", 2, pid.0, epoch, 0, true), -1, Some("poptx"))).await;
    c.call(3, &EndTxnRequest::default().with_transactional_id(TransactionalId(s("poptx"))).with_producer_id(pid).with_producer_epoch(epoch).with_committed(false)).await;
    let id = topic_id(&tb, "pop");

    let build = |api: ApiKey, v: i16| -> Option<RequestKind> {
        Some(match api {
            ApiKey::Produce => {
                let mut r = produce_req(if v >= 13 { "" } else { "pop" }, 0, plain("v", 1), -1, None);
                if v >= 13 {
                    r.topic_data[0].topic_id = id;
                }
                RequestKind::Produce(r)
            }
            ApiKey::Fetch => {
                let mut r = fetch_req(if v >= 13 { "" } else { "pop" }, if v >= 13 { id } else { Uuid::nil() }, 0, 0, 1);
                r.topics[0].partitions.push(fetch_request::FetchPartition::default().with_partition(1).with_fetch_offset(0).with_partition_max_bytes(1 << 20).with_current_leader_epoch(-1).with_last_fetched_epoch(-1));
                RequestKind::Fetch(r.with_max_wait_ms(0))
            }
            ApiKey::ListOffsets => RequestKind::ListOffsets(
                ListOffsetsRequest::default().with_replica_id(BrokerId(-1)).with_topics(vec![
                    list_offsets_request::ListOffsetsTopic::default().with_name(tn("pop")).with_partitions(vec![
                        list_offsets_request::ListOffsetsPartition::default().with_partition_index(0).with_timestamp(-1).with_current_leader_epoch(if v >= 4 { 0 } else { -1 }),
                        list_offsets_request::ListOffsetsPartition::default().with_partition_index(1).with_timestamp(-2).with_current_leader_epoch(-1),
                    ]),
                ]),
            ),
            ApiKey::Metadata => RequestKind::Metadata(
                MetadataRequest::default()
                    .with_topics(Some(vec![metadata_request::MetadataRequestTopic::default().with_name(Some(tn("pop")))]))
                    .with_include_topic_authorized_operations(v >= 8),
            ),
            ApiKey::OffsetCommit => RequestKind::OffsetCommit(commit.clone()),
            ApiKey::OffsetFetch if v >= 8 => RequestKind::OffsetFetch(OffsetFetchRequest::default().with_groups(vec![
                offset_fetch_request::OffsetFetchRequestGroup::default()
                    .with_group_id(GroupId(s("popg")))
                    .with_topics(Some(vec![offset_fetch_request::OffsetFetchRequestTopics::default().with_name(tn("pop")).with_partition_indexes(vec![0, 1])])),
            ])),
            ApiKey::OffsetFetch => RequestKind::OffsetFetch(
                OffsetFetchRequest::default()
                    .with_group_id(GroupId(s("popg")))
                    .with_topics(Some(vec![offset_fetch_request::OffsetFetchRequestTopic::default().with_name(tn("pop")).with_partition_indexes(vec![0, 1])])),
            ),
            ApiKey::FindCoordinator if v >= 4 => RequestKind::FindCoordinator(FindCoordinatorRequest::default().with_coordinator_keys(vec![s("popg")])),
            ApiKey::FindCoordinator => RequestKind::FindCoordinator(FindCoordinatorRequest::default().with_key(s("popg"))),
            ApiKey::DescribeGroups => RequestKind::DescribeGroups(DescribeGroupsRequest::default().with_groups(vec![GroupId(s("popg")), GroupId(s("missing"))]).with_include_authorized_operations(v >= 3)),
            ApiKey::ListGroups => RequestKind::ListGroups(ListGroupsRequest::default()),
            ApiKey::CreateTopics => RequestKind::CreateTopics(CreateTopicsRequest::default().with_topics(vec![
                create_topics_request::CreatableTopic::default()
                    .with_name(tn(&format!("new-v{v}")))
                    .with_num_partitions(2)
                    .with_replication_factor(1)
                    .with_configs(vec![create_topics_request::CreatableTopicConfig::default().with_name(s("retention.ms")).with_value(Some(s("1000")))]),
                create_topics_request::CreatableTopic::default().with_name(tn("pop")).with_num_partitions(1).with_replication_factor(1),
            ])),
            ApiKey::DeleteTopics if v >= 6 => RequestKind::DeleteTopics(DeleteTopicsRequest::default().with_topics(vec![delete_topics_request::DeleteTopicState::default().with_name(Some(tn("nope")))])),
            ApiKey::DeleteTopics => RequestKind::DeleteTopics(DeleteTopicsRequest::default().with_topic_names(vec![tn("nope")])),
            ApiKey::DeleteRecords => RequestKind::DeleteRecords(DeleteRecordsRequest::default().with_topics(vec![
                delete_records_request::DeleteRecordsTopic::default().with_name(tn("pop")).with_partitions(vec![delete_records_request::DeleteRecordsPartition::default().with_partition_index(0).with_offset(0)]),
            ])),
            ApiKey::InitProducerId => RequestKind::InitProducerId(InitProducerIdRequest::default().with_transaction_timeout_ms(1000).with_producer_id(ProducerId(-1)).with_producer_epoch(-1)),
            ApiKey::OffsetForLeaderEpoch => RequestKind::OffsetForLeaderEpoch(OffsetForLeaderEpochRequest::default().with_topics(vec![
                offset_for_leader_epoch_request::OffsetForLeaderTopic::default()
                    .with_topic(tn("pop"))
                    .with_partitions(vec![offset_for_leader_epoch_request::OffsetForLeaderPartition::default().with_partition(0).with_leader_epoch(0).with_current_leader_epoch(-1)]),
            ])),
            ApiKey::DescribeConfigs => RequestKind::DescribeConfigs(
                DescribeConfigsRequest::default().with_include_synonyms(v >= 1).with_include_documentation(v >= 3).with_resources(vec![
                    describe_configs_request::DescribeConfigsResource::default().with_resource_type(2).with_resource_name(s("pop")).with_configuration_keys(None),
                    describe_configs_request::DescribeConfigsResource::default().with_resource_type(4).with_resource_name(s("1")).with_configuration_keys(None),
                    describe_configs_request::DescribeConfigsResource::default().with_resource_type(2).with_resource_name(s("missing")).with_configuration_keys(None),
                ]),
            ),
            ApiKey::AlterConfigs => RequestKind::AlterConfigs(AlterConfigsRequest::default().with_validate_only(true).with_resources(vec![
                alter_configs_request::AlterConfigsResource::default()
                    .with_resource_type(2)
                    .with_resource_name(s("pop"))
                    .with_configs(vec![alter_configs_request::AlterableConfig::default().with_name(s("retention.ms")).with_value(Some(s("bad")))]),
            ])),
            ApiKey::IncrementalAlterConfigs => RequestKind::IncrementalAlterConfigs(IncrementalAlterConfigsRequest::default().with_validate_only(true).with_resources(vec![
                incremental_alter_configs_request::AlterConfigsResource::default()
                    .with_resource_type(2)
                    .with_resource_name(s("pop"))
                    .with_configs(vec![incremental_alter_configs_request::AlterableConfig::default().with_name(s("retention.ms")).with_config_operation(0).with_value(Some(s("5000")))]),
            ])),
            ApiKey::DescribeLogDirs => RequestKind::DescribeLogDirs(DescribeLogDirsRequest::default().with_topics(None)),
            ApiKey::CreatePartitions => RequestKind::CreatePartitions(CreatePartitionsRequest::default().with_validate_only(true).with_topics(vec![
                create_partitions_request::CreatePartitionsTopic::default().with_name(tn("pop")).with_count(4),
                create_partitions_request::CreatePartitionsTopic::default().with_name(tn("missing")).with_count(4),
            ])),
            ApiKey::DeleteGroups => RequestKind::DeleteGroups(DeleteGroupsRequest::default().with_groups_names(vec![GroupId(s("missing"))])),
            ApiKey::ElectLeaders => RequestKind::ElectLeaders(ElectLeadersRequest::default().with_topic_partitions(Some(vec![elect_leaders_request::TopicPartitions::default().with_topic(tn("pop")).with_partitions(vec![0, 1])]))),
            ApiKey::OffsetDelete => RequestKind::OffsetDelete(OffsetDeleteRequest::default().with_group_id(GroupId(s("missing"))).with_topics(vec![
                offset_delete_request::OffsetDeleteRequestTopic::default().with_name(tn("pop")).with_partitions(vec![offset_delete_request::OffsetDeleteRequestPartition::default().with_partition_index(0)]),
            ])),
            ApiKey::DescribeProducers => RequestKind::DescribeProducers(DescribeProducersRequest::default().with_topics(vec![describe_producers_request::TopicRequest::default().with_name(tn("pop")).with_partition_indexes(vec![0, 1, 7])])),
            ApiKey::DescribeTransactions => RequestKind::DescribeTransactions(DescribeTransactionsRequest::default().with_transactional_ids(vec![TransactionalId(s("poptx")), TransactionalId(s("missing"))])),
            ApiKey::ListTransactions => RequestKind::ListTransactions(ListTransactionsRequest::default().with_duration_filter(-1)),
            ApiKey::DescribeTopicPartitions => RequestKind::DescribeTopicPartitions(
                DescribeTopicPartitionsRequest::default().with_topics(vec![describe_topic_partitions_request::TopicRequest::default().with_name(tn("pop")), describe_topic_partitions_request::TopicRequest::default().with_name(tn("missing"))]).with_response_partition_limit(1),
            ),
            ApiKey::DescribeCluster => RequestKind::DescribeCluster(DescribeClusterRequest::default().with_include_cluster_authorized_operations(true)),
            ApiKey::DescribeQuorum => RequestKind::DescribeQuorum(DescribeQuorumRequest::default().with_topics(vec![
                describe_quorum_request::TopicData::default().with_topic_name(tn("__cluster_metadata")).with_partitions(vec![describe_quorum_request::PartitionData::default().with_partition_index(0)]),
            ])),
            _ => return None,
        })
    };
    let mut checked = 0;
    for (api, min, max) in APIS {
        for v in *min..=*max {
            let Some(req) = build(*api, v) else { continue };
            assert!(Conn::encodes(*api, v, &req), "{api:?} v{v}: the test request doesn't fit this version");
            let resp = c.call_kind(*api, v, &req).await;
            assert!(resp.is_some(), "{api:?} v{v}: connection closed (a response field outside its version?)");
            checked += 1;
        }
    }
    assert!(checked >= 147, "{checked}");
}

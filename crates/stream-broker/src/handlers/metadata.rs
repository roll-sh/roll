//! Metadata, DescribeCluster and DescribeTopicPartitions. Every stream looks like a one-broker
//! cluster that leads all its partitions; durability comes from object storage, not replicas.

use super::{Ctx, opt_s, s};
use crate::broker::{Topic, validate_topic_name};
use kafka_protocol::ResponseError;
use kafka_protocol::messages::describe_cluster_response::{DescribeClusterBroker, DescribeClusterResponse};
use kafka_protocol::messages::describe_topic_partitions_response::{
    DescribeTopicPartitionsResponse, DescribeTopicPartitionsResponsePartition, DescribeTopicPartitionsResponseTopic,
};
use kafka_protocol::messages::metadata_response::{MetadataResponse, MetadataResponseBroker, MetadataResponsePartition, MetadataResponseTopic};
use kafka_protocol::messages::describe_quorum_response::{self, DescribeQuorumResponse, Listener, ReplicaState};
use kafka_protocol::messages::{BrokerId, DescribeClusterRequest, DescribeQuorumRequest, DescribeTopicPartitionsRequest, MetadataRequest, TopicName};
use std::sync::Arc;
use uuid::Uuid;

/// `authorized_operations` when not requested.
pub const OPS_OMITTED: i32 = i32::MIN;
/// READ WRITE CREATE DELETE ALTER DESCRIBE DESCRIBE_CONFIGS ALTER_CONFIGS.
pub const TOPIC_OPS: i32 = (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 10) | (1 << 11);
/// CREATE ALTER DESCRIBE CLUSTER_ACTION DESCRIBE_CONFIGS ALTER_CONFIGS IDEMPOTENT_WRITE.
pub const CLUSTER_OPS: i32 = (1 << 5) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 12);
/// READ DELETE DESCRIBE (consumer groups).
pub const GROUP_OPS: i32 = (1 << 3) | (1 << 6) | (1 << 8);

fn node(ctx: &Ctx) -> BrokerId {
    BrokerId(ctx.b.config.node_id)
}

fn partitions(ctx: &Ctx, topic: &Topic) -> Vec<MetadataResponsePartition> {
    (0..topic.partition_count())
        .map(|i| {
            MetadataResponsePartition::default()
                .with_partition_index(i)
                .with_leader_id(node(ctx))
                .with_leader_epoch(0)
                .with_replica_nodes(vec![node(ctx)])
                .with_isr_nodes(vec![node(ctx)])
        })
        .collect()
}

fn topic_response(ctx: &Ctx, topic: &Topic, ops: bool) -> MetadataResponseTopic {
    MetadataResponseTopic::default()
        .with_name(Some(TopicName(s(topic.name.clone()))))
        .with_topic_id(topic.id)
        .with_partitions(partitions(ctx, topic))
        .with_topic_authorized_operations(if ops { TOPIC_OPS } else { OPS_OMITTED })
}

pub fn metadata(ctx: &Ctx, req: MetadataRequest) -> MetadataResponse {
    let v = ctx.version;
    let ops = req.include_topic_authorized_operations;
    let allow_auto = if v >= 4 { req.allow_auto_topic_creation } else { true };
    let all = match &req.topics {
        None => true,
        Some(t) if t.is_empty() && v == 0 => true,
        _ => false,
    };
    let topics = if all {
        ctx.stream.topics().iter().map(|t| topic_response(ctx, t, ops)).collect()
    } else {
        req.topics
            .unwrap_or_default()
            .into_iter()
            .map(|rt| match rt.name {
                Some(name) => {
                    let found = ctx.stream.topic(&name).or_else(|| {
                        (allow_auto && validate_topic_name(&name).is_ok()).then(|| ctx.b.auto_create(&ctx.stream, &name)).flatten()
                    });
                    match found {
                        Some(t) => topic_response(ctx, &t, ops),
                        None => {
                            let code = if validate_topic_name(&name).is_err() {
                                ResponseError::InvalidTopicException
                            } else {
                                ResponseError::UnknownTopicOrPartition
                            };
                            MetadataResponseTopic::default()
                                .with_name(Some(name))
                                .with_error_code(code.code())
                                .with_topic_authorized_operations(OPS_OMITTED)
                        }
                    }
                }
                None => match ctx.stream.topic_by_id(&rt.topic_id) {
                    Some(t) => topic_response(ctx, &t, ops),
                    None => MetadataResponseTopic::default()
                        .with_topic_id(rt.topic_id)
                        .with_error_code(ResponseError::UnknownTopicId.code())
                        .with_topic_authorized_operations(OPS_OMITTED),
                },
            })
            .collect()
    };
    let broker = MetadataResponseBroker::default()
        .with_node_id(node(ctx))
        .with_host(s(ctx.b.config.advertised_host.clone()))
        .with_port(ctx.b.config.advertised_port as i32)
        .with_rack(ctx.b.config.rack.clone().map(s));
    MetadataResponse::default()
        .with_brokers(vec![broker])
        .with_cluster_id(opt_s(ctx.stream.cluster_id.clone()))
        .with_controller_id(node(ctx))
        .with_topics(topics)
        .with_cluster_authorized_operations(if req.include_cluster_authorized_operations { CLUSTER_OPS } else { OPS_OMITTED })
}

pub fn describe_cluster(ctx: &Ctx, req: DescribeClusterRequest) -> DescribeClusterResponse {
    let broker = DescribeClusterBroker::default()
        .with_broker_id(node(ctx))
        .with_host(s(ctx.b.config.advertised_host.clone()))
        .with_port(ctx.b.config.advertised_port as i32)
        .with_rack(ctx.b.config.rack.clone().map(s));
    DescribeClusterResponse::default()
        .with_cluster_id(s(ctx.stream.cluster_id.clone()))
        .with_controller_id(node(ctx))
        .with_endpoint_type(req.endpoint_type.max(1))
        .with_brokers(vec![broker])
        .with_cluster_authorized_operations(if req.include_cluster_authorized_operations { CLUSTER_OPS } else { OPS_OMITTED })
}

/// DescribeQuorum: one voter, this node, always caught up (there is no metadata log to
/// replicate; the catalog is local SQLite plus object storage).
pub fn describe_quorum(ctx: &Ctx, req: DescribeQuorumRequest) -> DescribeQuorumResponse {
    let now = crate::ids::now_ms();
    let mut voter = ReplicaState::default().with_replica_id(node(ctx)).with_log_end_offset(0);
    if ctx.version >= 1 {
        voter = voter.with_last_fetch_timestamp(now).with_last_caught_up_timestamp(now);
    }
    let topics = req
        .topics
        .into_iter()
        .map(|t| {
            let known = t.topic_name.as_str() == "__cluster_metadata";
            let partitions = t
                .partitions
                .into_iter()
                .map(|p| {
                    let mut r = describe_quorum_response::PartitionData::default().with_partition_index(p.partition_index);
                    if known && p.partition_index == 0 {
                        r = r.with_leader_id(node(ctx)).with_leader_epoch(0).with_high_watermark(0).with_current_voters(vec![voter.clone()]);
                    } else {
                        r.error_code = ResponseError::UnknownTopicOrPartition.code();
                    }
                    r
                })
                .collect();
            describe_quorum_response::TopicData::default().with_topic_name(t.topic_name).with_partitions(partitions)
        })
        .collect();
    let mut resp = DescribeQuorumResponse::default().with_topics(topics);
    if ctx.version >= 2 {
        let listener = Listener::default().with_name(s("CONTROLLER")).with_host(s(ctx.b.config.advertised_host.clone())).with_port(ctx.b.config.advertised_port);
        resp.nodes = vec![describe_quorum_response::Node::default().with_node_id(node(ctx)).with_listeners(vec![listener])];
    }
    resp
}

pub fn describe_topic_partitions(ctx: &Ctx, req: DescribeTopicPartitionsRequest) -> DescribeTopicPartitionsResponse {
    let limit = if req.response_partition_limit > 0 { req.response_partition_limit as usize } else { 2000 };
    let mut topics: Vec<Result<Arc<Topic>, TopicName>> = if req.topics.is_empty() {
        ctx.stream.topics().into_iter().map(Ok).collect()
    } else {
        req.topics.into_iter().map(|t| ctx.stream.topic(&t.name).ok_or(t.name)).collect()
    };
    topics.sort_by(|a, b| {
        let n = |x: &Result<Arc<Topic>, TopicName>| match x {
            Ok(t) => t.name.clone(),
            Err(n) => n.to_string(),
        };
        n(a).cmp(&n(b))
    });
    // Resume from the cursor.
    let (cursor_topic, cursor_part) = req.cursor.as_ref().map(|c| (c.topic_name.to_string(), c.partition_index)).unwrap_or_default();
    let mut out = Vec::new();
    let mut budget = limit;
    let mut next = None;
    for t in topics {
        match t {
            Err(name) => out.push(
                DescribeTopicPartitionsResponseTopic::default()
                    .with_name(Some(name))
                    .with_topic_id(Uuid::nil())
                    .with_error_code(ResponseError::UnknownTopicOrPartition.code())
                    .with_topic_authorized_operations(OPS_OMITTED),
            ),
            Ok(t) => {
                if !cursor_topic.is_empty() && t.name < cursor_topic {
                    continue;
                }
                let start = if t.name == cursor_topic { cursor_part } else { 0 };
                let mut parts = Vec::new();
                for i in start..t.partition_count() {
                    if budget == 0 {
                        next = Some((t.name.clone(), i));
                        break;
                    }
                    budget -= 1;
                    parts.push(
                        DescribeTopicPartitionsResponsePartition::default()
                            .with_partition_index(i)
                            .with_leader_id(node(ctx))
                            .with_leader_epoch(0)
                            .with_replica_nodes(vec![node(ctx)])
                            .with_isr_nodes(vec![node(ctx)])
                            // The Java admin client expects empty lists, not null (KIP-966).
                            .with_eligible_leader_replicas(Some(vec![]))
                            .with_last_known_elr(Some(vec![])),
                    );
                }
                out.push(
                    DescribeTopicPartitionsResponseTopic::default()
                        .with_name(Some(TopicName(s(t.name.clone()))))
                        .with_topic_id(t.id)
                        .with_partitions(parts)
                        .with_topic_authorized_operations(TOPIC_OPS),
                );
                if next.is_some() {
                    break;
                }
            }
        }
    }
    let cursor = next.map(|(name, i)| {
        kafka_protocol::messages::describe_topic_partitions_response::Cursor::default().with_topic_name(TopicName(s(name))).with_partition_index(i)
    });
    DescribeTopicPartitionsResponse::default().with_topics(out).with_next_cursor(cursor)
}

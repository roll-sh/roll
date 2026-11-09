//! Consumer group APIs: coordinator lookup, membership, committed offsets and group admin.

use super::metadata::{GROUP_OPS, OPS_OMITTED};
use super::{Ctx, Handled, later, s};
use crate::groups::{JoinRequest, SyncRequest};
use crate::ids::now;
use crate::store::OffsetRow;
use bytes::{Buf, Bytes};
use kafka_protocol::ResponseError;
use kafka_protocol::messages::delete_groups_response::{DeletableGroupResult, DeleteGroupsResponse};
use kafka_protocol::messages::describe_groups_response::{DescribeGroupsResponse, DescribedGroup, DescribedGroupMember};
use kafka_protocol::messages::find_coordinator_response::{Coordinator, FindCoordinatorResponse};
use kafka_protocol::messages::heartbeat_response::HeartbeatResponse;
use kafka_protocol::messages::join_group_response::{JoinGroupResponse, JoinGroupResponseMember};
use kafka_protocol::messages::leave_group_response::{LeaveGroupResponse, MemberResponse};
use kafka_protocol::messages::list_groups_response::{ListGroupsResponse, ListedGroup};
use kafka_protocol::messages::offset_commit_response::{OffsetCommitResponse, OffsetCommitResponsePartition, OffsetCommitResponseTopic};
use kafka_protocol::messages::offset_delete_response::{OffsetDeleteResponse, OffsetDeleteResponsePartition, OffsetDeleteResponseTopic};
use kafka_protocol::messages::offset_fetch_response::{
    OffsetFetchResponse, OffsetFetchResponseGroup, OffsetFetchResponsePartition, OffsetFetchResponsePartitions, OffsetFetchResponseTopic,
    OffsetFetchResponseTopics,
};
use kafka_protocol::messages::sync_group_response::SyncGroupResponse;
use kafka_protocol::messages::{
    BrokerId, ConsumerProtocolAssignment, ConsumerProtocolSubscription, DeleteGroupsRequest, DescribeGroupsRequest, FindCoordinatorRequest,
    GroupId, HeartbeatRequest, JoinGroupRequest, LeaveGroupRequest, ListGroupsRequest, OffsetCommitRequest, OffsetDeleteRequest,
    OffsetFetchRequest, ResponseKind, SyncGroupRequest, TopicName,
};
use kafka_protocol::protocol::Decodable;
use std::collections::{BTreeMap, HashMap, HashSet};

pub const MAX_METADATA: usize = 4096;

/// Topics in a consumer protocol subscription (`version i16` + message).
pub fn subscription_topics(metadata: &Bytes) -> Vec<String> {
    let mut b = metadata.clone();
    if b.len() < 2 {
        return vec![];
    }
    let v = b.get_i16();
    ConsumerProtocolSubscription::decode(&mut b, v.clamp(0, 3)).map(|s| s.topics.iter().map(|t| t.to_string()).collect()).unwrap_or_default()
}

/// Partitions in a consumer protocol assignment.
pub fn assignment_partitions(assignment: &Bytes) -> Vec<(String, Vec<i32>)> {
    let mut b = assignment.clone();
    if b.len() < 2 {
        return vec![];
    }
    let v = b.get_i16();
    ConsumerProtocolAssignment::decode(&mut b, v.clamp(0, 3))
        .map(|a| a.assigned_partitions.into_iter().map(|tp| (tp.topic.to_string(), tp.partitions)).collect())
        .unwrap_or_default()
}

pub fn find_coordinator(ctx: &Ctx, req: FindCoordinatorRequest) -> FindCoordinatorResponse {
    let host = s(ctx.b.config.advertised_host.clone());
    let port = ctx.b.config.advertised_port as i32;
    let node = BrokerId(ctx.b.config.node_id);
    if !matches!(req.key_type, 0 | 1) {
        let e = ResponseError::InvalidRequest.code();
        return FindCoordinatorResponse::default().with_error_code(e).with_node_id(BrokerId(-1)).with_port(-1).with_coordinators(
            req.coordinator_keys.into_iter().map(|k| Coordinator::default().with_key(k).with_error_code(e).with_node_id(BrokerId(-1)).with_port(-1)).collect(),
        );
    }
    if ctx.version >= 4 {
        let coordinators = req
            .coordinator_keys
            .into_iter()
            .map(|k| Coordinator::default().with_key(k).with_node_id(node).with_host(host.clone()).with_port(port))
            .collect();
        return FindCoordinatorResponse::default().with_coordinators(coordinators);
    }
    FindCoordinatorResponse::default().with_node_id(node).with_host(host).with_port(port)
}

pub fn join(ctx: Ctx, req: JoinGroupRequest) -> Handled {
    let v = ctx.version;
    later(async move {
        let jr = JoinRequest {
            group_id: req.group_id.to_string(),
            member_id: req.member_id.to_string(),
            instance_id: req.group_instance_id.map(|i| i.to_string()),
            client_id: ctx.client_id.clone(),
            client_host: ctx.client_host.clone(),
            session_timeout_ms: req.session_timeout_ms,
            rebalance_timeout_ms: if v >= 1 { req.rebalance_timeout_ms } else { req.session_timeout_ms },
            protocol_type: req.protocol_type.to_string(),
            protocols: req.protocols.into_iter().map(|p| (p.name.to_string(), p.metadata)).collect(),
            require_known_member_id: v >= 4,
        };
        let r = ctx.b.groups.join(&ctx.b, &ctx.stream.id, jr).await;
        let members = r
            .members
            .into_iter()
            .map(|(id, instance, metadata)| JoinGroupResponseMember::default().with_member_id(s(id)).with_group_instance_id(instance.map(s)).with_metadata(metadata))
            .collect();
        ResponseKind::JoinGroup(
            JoinGroupResponse::default()
                .with_error_code(r.error)
                .with_generation_id(r.generation)
                .with_protocol_type(r.protocol_type.map(s))
                .with_protocol_name(Some(s(r.protocol_name.unwrap_or_default())))
                .with_leader(s(r.leader))
                .with_member_id(s(r.member_id))
                .with_members(members),
        )
    })
}

pub fn sync(ctx: Ctx, req: SyncGroupRequest) -> Handled {
    later(async move {
        let sr = SyncRequest {
            group_id: req.group_id.to_string(),
            member_id: req.member_id.to_string(),
            instance_id: req.group_instance_id.map(|i| i.to_string()),
            generation: req.generation_id,
            protocol_type: req.protocol_type.map(|p| p.to_string()),
            protocol_name: req.protocol_name.map(|p| p.to_string()),
            assignments: req.assignments.into_iter().map(|a| (a.member_id.to_string(), a.assignment)).collect(),
        };
        let r = ctx.b.groups.sync(&ctx.stream.id, sr).await;
        ResponseKind::SyncGroup(
            SyncGroupResponse::default()
                .with_error_code(r.error)
                .with_protocol_type(r.protocol_type.map(s))
                .with_protocol_name(r.protocol_name.map(s))
                .with_assignment(r.assignment),
        )
    })
}

pub fn heartbeat(ctx: &Ctx, req: HeartbeatRequest) -> HeartbeatResponse {
    let instance = req.group_instance_id.as_ref().map(|i| i.to_string());
    let e = ctx.b.groups.heartbeat(&ctx.stream.id, &req.group_id, &req.member_id, instance.as_deref(), req.generation_id);
    HeartbeatResponse::default().with_error_code(e)
}

pub fn leave(ctx: &Ctx, req: LeaveGroupRequest) -> LeaveGroupResponse {
    let members: Vec<(String, Option<String>)> = if ctx.version >= 3 {
        req.members.iter().map(|m| (m.member_id.to_string(), m.group_instance_id.as_ref().map(|i| i.to_string()))).collect()
    } else {
        vec![(req.member_id.to_string(), None)]
    };
    match ctx.b.groups.leave(&ctx.b, &ctx.stream.id, &req.group_id, &members) {
        Err(e) => LeaveGroupResponse::default().with_error_code(e.code()),
        Ok(codes) => {
            if ctx.version < 3 {
                return LeaveGroupResponse::default().with_error_code(codes[0]);
            }
            let rs = members
                .into_iter()
                .zip(codes)
                .map(|((m, i), c)| MemberResponse::default().with_member_id(s(m)).with_group_instance_id(i.map(s)).with_error_code(c))
                .collect();
            LeaveGroupResponse::default().with_members(rs)
        }
    }
}

pub fn offset_commit(ctx: &Ctx, req: OffsetCommitRequest) -> OffsetCommitResponse {
    let instance = req.group_instance_id.as_ref().map(|i| i.to_string());
    let group_error = ctx
        .b
        .groups
        .check_commit(&ctx.b, &ctx.stream.id, &req.group_id, &req.member_id, instance.as_deref(), req.generation_id_or_member_epoch)
        .err();
    let at = now();
    let mut rows = Vec::new();
    let mut topics = Vec::with_capacity(req.topics.len());
    for t in req.topics {
        let topic = ctx.stream.topic(&t.name);
        let partitions = t
            .partitions
            .into_iter()
            .map(|p| {
                let code = if let Some(e) = group_error {
                    e.code()
                } else if topic.as_ref().and_then(|tp| tp.partition(p.partition_index)).is_none() {
                    ResponseError::UnknownTopicOrPartition.code()
                } else if p.committed_metadata.as_ref().is_some_and(|m| m.len() > MAX_METADATA) {
                    ResponseError::OffsetMetadataTooLarge.code()
                } else {
                    rows.push(OffsetRow {
                        topic: t.name.to_string(),
                        partition: p.partition_index,
                        offset: p.committed_offset,
                        leader_epoch: p.committed_leader_epoch,
                        metadata: p.committed_metadata.map(|m| m.to_string()),
                        committed_at: at,
                    });
                    0
                };
                OffsetCommitResponsePartition::default().with_partition_index(p.partition_index).with_error_code(code)
            })
            .collect();
        topics.push(OffsetCommitResponseTopic::default().with_name(t.name).with_topic_id(t.topic_id).with_partitions(partitions));
    }
    if !rows.is_empty()
        && ctx.b.store.commit_offsets(&ctx.stream.id, &req.group_id, &rows).is_err()
    {
        for t in &mut topics {
            for p in &mut t.partitions {
                if p.error_code == 0 {
                    p.error_code = ResponseError::KafkaStorageError.code();
                }
            }
        }
    }
    OffsetCommitResponse::default().with_topics(topics)
}

/// `(partition, offset, leader epoch, metadata, error)`.
type Committed = (i32, i64, i32, String, i16);

/// Committed offsets of one group, per topic.
fn group_offsets(ctx: &Ctx, group: &str, topics: Option<Vec<(String, Vec<i32>)>>, require_stable: bool) -> Result<Vec<(String, Vec<Committed>)>, ResponseError> {
    let rows = ctx.b.store.offsets(&ctx.stream.id, group).map_err(|_| ResponseError::CoordinatorNotAvailable)?;
    let unstable: HashSet<(String, i32)> = if require_stable { ctx.b.groups.unstable(&ctx.stream.id, group).into_iter().collect() } else { HashSet::new() };
    let mut by_topic: BTreeMap<String, HashMap<i32, OffsetRow>> = BTreeMap::new();
    for r in rows {
        by_topic.entry(r.topic.clone()).or_default().insert(r.partition, r);
    }
    let wanted: Vec<(String, Vec<i32>)> = match topics {
        Some(t) => t,
        None => by_topic.iter().map(|(t, ps)| {
            let mut v: Vec<i32> = ps.keys().copied().collect();
            v.sort();
            (t.clone(), v)
        }).collect(),
    };
    Ok(wanted
        .into_iter()
        .map(|(t, parts)| {
            let committed = by_topic.get(&t);
            let ps = parts
                .into_iter()
                .map(|p| {
                    if unstable.contains(&(t.clone(), p)) {
                        return (p, -1, -1, String::new(), ResponseError::UnstableOffsetCommit.code());
                    }
                    match committed.and_then(|c| c.get(&p)) {
                        Some(r) => (p, r.offset, r.leader_epoch, r.metadata.clone().unwrap_or_default(), 0),
                        None => (p, -1, -1, String::new(), 0),
                    }
                })
                .collect();
            (t, ps)
        })
        .collect())
}

pub fn offset_fetch(ctx: &Ctx, req: OffsetFetchRequest) -> OffsetFetchResponse {
    if ctx.version >= 8 {
        let groups = req
            .groups
            .into_iter()
            .map(|g| {
                let topics = g.topics.map(|ts| ts.into_iter().map(|t| (t.name.to_string(), t.partition_indexes)).collect());
                match group_offsets(ctx, &g.group_id, topics, req.require_stable) {
                    Ok(ts) => OffsetFetchResponseGroup::default().with_group_id(g.group_id).with_topics(
                        ts.into_iter()
                            .map(|(t, ps)| {
                                OffsetFetchResponseTopics::default().with_name(TopicName(s(t))).with_partitions(
                                    ps.into_iter()
                                        .map(|(p, o, e, m, c)| {
                                            OffsetFetchResponsePartitions::default()
                                                .with_partition_index(p)
                                                .with_committed_offset(o)
                                                .with_committed_leader_epoch(e)
                                                .with_metadata(Some(s(m)))
                                                .with_error_code(c)
                                        })
                                        .collect(),
                                )
                            })
                            .collect(),
                    ),
                    Err(e) => OffsetFetchResponseGroup::default().with_group_id(g.group_id).with_error_code(e.code()),
                }
            })
            .collect();
        return OffsetFetchResponse::default().with_groups(groups);
    }
    let topics = req.topics.map(|ts| ts.into_iter().map(|t| (t.name.to_string(), t.partition_indexes)).collect());
    match group_offsets(ctx, &req.group_id, topics, req.require_stable) {
        Ok(ts) => OffsetFetchResponse::default().with_topics(
            ts.into_iter()
                .map(|(t, ps)| {
                    OffsetFetchResponseTopic::default().with_name(TopicName(s(t))).with_partitions(
                        ps.into_iter()
                            .map(|(p, o, e, m, c)| {
                                OffsetFetchResponsePartition::default()
                                    .with_partition_index(p)
                                    .with_committed_offset(o)
                                    .with_committed_leader_epoch(e)
                                    .with_metadata(Some(s(m)))
                                    .with_error_code(c)
                            })
                            .collect(),
                    )
                })
                .collect(),
        ),
        Err(e) => OffsetFetchResponse::default().with_error_code(e.code()),
    }
}

pub fn offset_delete(ctx: &Ctx, req: OffsetDeleteRequest) -> OffsetDeleteResponse {
    let known = ctx.b.store.offset_groups(&ctx.stream.id).unwrap_or_default().iter().any(|g| g.as_str() == req.group_id.as_str())
        || ctx.b.groups.is_active(&ctx.stream.id, &req.group_id);
    if !known {
        return OffsetDeleteResponse::default().with_error_code(ResponseError::GroupIdNotFound.code());
    }
    let subscribed = ctx.b.groups.subscribed_topics(&ctx.stream.id, &req.group_id).unwrap_or_default();
    let mut delete = Vec::new();
    let topics = req
        .topics
        .into_iter()
        .map(|t| {
            let exists = ctx.stream.topic(&t.name);
            let partitions = t
                .partitions
                .into_iter()
                .map(|p| {
                    let code = if subscribed.iter().any(|x| x.as_str() == t.name.as_str()) {
                        ResponseError::GroupSubscribedToTopic.code()
                    } else if exists.as_ref().and_then(|x| x.partition(p.partition_index)).is_none() {
                        ResponseError::UnknownTopicOrPartition.code()
                    } else {
                        delete.push((t.name.to_string(), p.partition_index));
                        0
                    };
                    OffsetDeleteResponsePartition::default().with_partition_index(p.partition_index).with_error_code(code)
                })
                .collect();
            OffsetDeleteResponseTopic::default().with_name(t.name).with_partitions(partitions)
        })
        .collect();
    if !delete.is_empty() && ctx.b.store.delete_offsets(&ctx.stream.id, &req.group_id, Some(&delete)).is_err() {
        return OffsetDeleteResponse::default().with_error_code(ResponseError::KafkaStorageError.code());
    }
    OffsetDeleteResponse::default().with_topics(topics)
}

pub fn describe(ctx: &Ctx, req: DescribeGroupsRequest) -> DescribeGroupsResponse {
    let ops = if req.include_authorized_operations { GROUP_OPS } else { OPS_OMITTED };
    let groups = req
        .groups
        .into_iter()
        .map(|g| match ctx.b.groups.describe(&ctx.b, &ctx.stream.id, &g) {
            Some(info) => DescribedGroup::default()
                .with_group_id(g)
                .with_group_state(s(info.state))
                .with_protocol_type(s(info.protocol_type))
                .with_protocol_data(s(info.protocol))
                .with_authorized_operations(ops)
                .with_members(
                    info.members
                        .into_iter()
                        .map(|m| {
                            DescribedGroupMember::default()
                                .with_member_id(s(m.member_id))
                                .with_group_instance_id(m.instance_id.map(s))
                                .with_client_id(s(m.client_id))
                                .with_client_host(s(m.client_host))
                                .with_member_metadata(m.metadata)
                                .with_member_assignment(m.assignment)
                        })
                        .collect(),
                ),
            None if ctx.version >= 6 => DescribedGroup::default()
                .with_group_id(g.clone())
                .with_error_code(ResponseError::GroupIdNotFound.code())
                .with_error_message(Some(s(format!("Group {} not found.", g.as_str()))))
                .with_group_state(s("Dead"))
                .with_authorized_operations(ops),
            None => DescribedGroup::default().with_group_id(g).with_group_state(s("Dead")).with_authorized_operations(ops),
        })
        .collect();
    DescribeGroupsResponse::default().with_groups(groups)
}

pub fn list(ctx: &Ctx, req: ListGroupsRequest) -> ListGroupsResponse {
    let states: Vec<String> = req.states_filter.iter().map(|s| s.to_lowercase()).collect();
    let types: Vec<String> = req.types_filter.iter().map(|s| s.to_lowercase()).collect();
    let groups = ctx
        .b
        .groups
        .list(&ctx.b, &ctx.stream.id)
        .into_iter()
        .filter(|(_, _, st)| states.is_empty() || states.contains(&st.to_lowercase()))
        .filter(|_| types.is_empty() || types.iter().any(|t| t == "classic"))
        .map(|(id, pt, st)| {
            ListedGroup::default().with_group_id(GroupId(s(id))).with_protocol_type(s(pt)).with_group_state(s(st)).with_group_type(s("classic"))
        })
        .collect();
    ListGroupsResponse::default().with_groups(groups)
}

pub fn delete(ctx: &Ctx, req: DeleteGroupsRequest) -> DeleteGroupsResponse {
    let results = req
        .groups_names
        .into_iter()
        .map(|g| {
            let code = match ctx.b.groups.delete(&ctx.b, &ctx.stream.id, &g) {
                Ok(()) => 0,
                Err(e) => e.code(),
            };
            DeletableGroupResult::default().with_group_id(g).with_error_code(code)
        })
        .collect();
    DeleteGroupsResponse::default().with_results(results)
}

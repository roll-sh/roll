//! Admin APIs: topics, partitions, records, leadership, log dirs, quotas, ACLs, SCRAM users.

use super::configs::topic_configs;
use super::{Ctx, Handled, later, s};
use crate::broker::dir_size;
use crate::error::KafkaError;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::alter_client_quotas_response::{self, AlterClientQuotasResponse};
use kafka_protocol::messages::create_acls_response::{AclCreationResult, CreateAclsResponse};
use kafka_protocol::messages::create_partitions_response::{CreatePartitionsResponse, CreatePartitionsTopicResult};
use kafka_protocol::messages::create_topics_response::{CreatableTopicConfigs, CreatableTopicResult, CreateTopicsResponse};
use kafka_protocol::messages::delete_acls_response::{DeleteAclsFilterResult, DeleteAclsResponse};
use kafka_protocol::messages::delete_records_response::{DeleteRecordsPartitionResult, DeleteRecordsResponse, DeleteRecordsTopicResult};
use kafka_protocol::messages::delete_topics_response::{DeletableTopicResult, DeleteTopicsResponse};
use kafka_protocol::messages::describe_acls_response::DescribeAclsResponse;
use kafka_protocol::messages::describe_client_quotas_response::{self, DescribeClientQuotasResponse};
use kafka_protocol::messages::describe_log_dirs_response::{DescribeLogDirsPartition, DescribeLogDirsResponse, DescribeLogDirsResult, DescribeLogDirsTopic};
use kafka_protocol::messages::describe_user_scram_credentials_response::{CredentialInfo, DescribeUserScramCredentialsResponse, DescribeUserScramCredentialsResult};
use kafka_protocol::messages::elect_leaders_response::{ElectLeadersResponse, PartitionResult, ReplicaElectionResult};
use kafka_protocol::messages::list_partition_reassignments_response::ListPartitionReassignmentsResponse;
use kafka_protocol::messages::{
    AlterClientQuotasRequest, CreateAclsRequest, CreatePartitionsRequest, CreateTopicsRequest, DeleteAclsRequest, DeleteRecordsRequest,
    DeleteTopicsRequest, DescribeAclsRequest, DescribeClientQuotasRequest, DescribeLogDirsRequest, DescribeUserScramCredentialsRequest,
    ElectLeadersRequest, ListPartitionReassignmentsRequest, ResponseKind, TopicName,
};
use std::collections::BTreeMap;

pub fn create_topics(ctx: &Ctx, req: CreateTopicsRequest) -> CreateTopicsResponse {
    let node = ctx.b.config.node_id;
    let topics = req
        .topics
        .into_iter()
        .map(|t| {
            let mut r = CreatableTopicResult::default().with_name(t.name.clone()).with_num_partitions(-1).with_replication_factor(-1);
            let configs: BTreeMap<String, String> =
                t.configs.iter().filter_map(|c| c.value.as_ref().map(|v| (c.name.to_string(), v.to_string()))).collect();
            let (mut partitions, mut rf) = (t.num_partitions, t.replication_factor);
            if !t.assignments.is_empty() {
                if partitions != -1 || rf != -1 {
                    r.error_code = ResponseError::InvalidRequest.code();
                    r.error_message = Some(s("Both numPartitions or replicationFactor and replicasAssignments were set."));
                    return r;
                }
                let ok = t.assignments.iter().enumerate().all(|(i, a)| {
                    a.partition_index == i as i32 && !a.broker_ids.is_empty() && a.broker_ids.iter().all(|b| b.0 == node)
                });
                if !ok {
                    r.error_code = ResponseError::InvalidReplicaAssignment.code();
                    r.error_message = Some(s(format!("Replica assignments may only name broker {node}.")));
                    return r;
                }
                partitions = t.assignments.len() as i32;
                rf = 1;
            }
            match ctx.b.create_topic(&ctx.stream, &t.name, partitions, rf, configs, req.validate_only) {
                Ok(topic) => {
                    let n = topic.as_ref().map(|t| t.partition_count()).unwrap_or(if partitions == -1 {
                        ctx.stream.settings().default_partitions
                    } else {
                        partitions
                    });
                    r.num_partitions = n;
                    r.replication_factor = if rf == -1 { 1 } else { rf };
                    if let Some(topic) = &topic {
                        r.topic_id = topic.id;
                        r.configs = Some(
                            topic_configs(&ctx.b, topic, None)
                                .into_iter()
                                .map(|c| {
                                    CreatableTopicConfigs::default()
                                        .with_name(s(c.name))
                                        .with_value(Some(s(c.value)))
                                        .with_read_only(c.read_only)
                                        .with_config_source(c.source)
                                        .with_is_sensitive(false)
                                })
                                .collect(),
                        );
                    }
                }
                Err(e) => {
                    r.error_code = e.as_i16();
                    r.error_message = e.str_message();
                }
            }
            r
        })
        .collect();
    CreateTopicsResponse::default().with_topics(topics)
}

pub fn delete_topics(ctx: Ctx, req: DeleteTopicsRequest) -> Handled {
    later(async move {
        // v6 identifies topics by name or id; older versions by name.
        let targets: Vec<(Option<TopicName>, uuid::Uuid)> = if ctx.version >= 6 {
            req.topics.into_iter().map(|t| (t.name, t.topic_id)).collect()
        } else {
            req.topic_names.into_iter().map(|n| (Some(n), uuid::Uuid::nil())).collect()
        };
        let mut out = Vec::new();
        for (name, id) in targets {
            let topic = match &name {
                Some(n) => ctx.stream.topic(n),
                None => ctx.stream.topic_by_id(&id),
            };
            let mut r = DeletableTopicResult::default().with_name(name.clone()).with_topic_id(id);
            match topic {
                None => {
                    r.error_code = if name.is_some() { ResponseError::UnknownTopicOrPartition } else { ResponseError::UnknownTopicId }.code();
                }
                Some(t) => {
                    r.name = Some(TopicName(s(t.name.clone())));
                    r.topic_id = t.id;
                    if let Err(e) = ctx.b.delete_topic(&ctx.stream, &t).await {
                        r.error_code = e.as_i16();
                        r.error_message = e.str_message();
                    }
                }
            }
            out.push(r);
        }
        ResponseKind::DeleteTopics(DeleteTopicsResponse::default().with_responses(out))
    })
}

pub fn create_partitions(ctx: &Ctx, req: CreatePartitionsRequest) -> CreatePartitionsResponse {
    let node = ctx.b.config.node_id;
    let results = req
        .topics
        .into_iter()
        .map(|t| {
            let mut r = CreatePartitionsTopicResult::default().with_name(t.name.clone());
            let outcome = match ctx.stream.topic(&t.name) {
                None => Err(KafkaError::code(ResponseError::UnknownTopicOrPartition)),
                Some(_) if t.assignments.as_ref().is_some_and(|a| a.iter().any(|a| a.broker_ids.iter().any(|b| b.0 != node))) => {
                    Err(KafkaError::new(ResponseError::InvalidReplicaAssignment, format!("Replica assignments may only name broker {node}.")))
                }
                Some(topic) => ctx.b.add_partitions(&ctx.stream, &topic, t.count, req.validate_only),
            };
            if let Err(e) = outcome {
                r.error_code = e.as_i16();
                r.error_message = e.str_message();
            }
            r
        })
        .collect();
    CreatePartitionsResponse::default().with_results(results)
}

pub fn delete_records(ctx: Ctx, req: DeleteRecordsRequest) -> Handled {
    later(async move {
        let mut topics = Vec::new();
        for t in req.topics {
            let topic = ctx.stream.topic(&t.name);
            let mut tr = DeleteRecordsTopicResult::default().with_name(t.name.clone());
            for p in t.partitions {
                let mut pr = DeleteRecordsPartitionResult::default().with_partition_index(p.partition_index).with_low_watermark(-1);
                match topic.as_ref().and_then(|x| x.partition(p.partition_index).map(|pp| (x.clone(), pp))) {
                    None => pr.error_code = ResponseError::UnknownTopicOrPartition.code(),
                    Some((topic, part)) => match ctx.b.log(&topic, &part) {
                        Err(e) => pr.error_code = e.as_i16(),
                        Ok(log) => {
                            let hw = log.high_watermark();
                            let target = if p.offset == -1 { hw } else { p.offset };
                            if target < 0 || target > hw {
                                pr.error_code = ResponseError::OffsetOutOfRange.code();
                            } else {
                                match ctx.b.clean_partition(&ctx.stream, &topic, &part, Some(target)).await {
                                    Ok(start) => pr.low_watermark = start,
                                    Err(e) => pr.error_code = e.as_i16(),
                                }
                            }
                        }
                    },
                }
                tr.partitions.push(pr);
            }
            topics.push(tr);
        }
        ResponseKind::DeleteRecords(DeleteRecordsResponse::default().with_topics(topics))
    })
}

pub fn elect_leaders(ctx: &Ctx, req: ElectLeadersRequest) -> ElectLeadersResponse {
    // Every partition is always led by this broker.
    let targets: Vec<(TopicName, Vec<i32>)> = match req.topic_partitions {
        Some(tp) => tp.into_iter().map(|t| (t.topic, t.partitions)).collect(),
        None => ctx.stream.topics().iter().map(|t| (TopicName(s(t.name.clone())), (0..t.partition_count()).collect())).collect(),
    };
    let results = targets
        .into_iter()
        .map(|(name, parts)| {
            let topic = ctx.stream.topic(&name);
            let pr = parts
                .into_iter()
                .map(|p| {
                    let code = if topic.as_ref().and_then(|t| t.partition(p)).is_some() {
                        ResponseError::ElectionNotNeeded
                    } else {
                        ResponseError::UnknownTopicOrPartition
                    };
                    PartitionResult::default().with_partition_id(p).with_error_code(code.code())
                })
                .collect();
            ReplicaElectionResult::default().with_topic(name).with_partition_result(pr)
        })
        .collect();
    ElectLeadersResponse::default().with_replica_election_results(results)
}

pub fn list_reassignments(_ctx: &Ctx, _req: ListPartitionReassignmentsRequest) -> ListPartitionReassignmentsResponse {
    ListPartitionReassignmentsResponse::default()
}

pub fn describe_log_dirs(ctx: &Ctx, req: DescribeLogDirsRequest) -> DescribeLogDirsResponse {
    let wanted: Vec<(String, Option<Vec<i32>>)> = match req.topics {
        Some(ts) => ts.into_iter().map(|t| (t.topic.to_string(), Some(t.partitions))).collect(),
        None => ctx.stream.topics().iter().map(|t| (t.name.clone(), None)).collect(),
    };
    let mut topics = Vec::new();
    for (name, parts) in wanted {
        let Some(topic) = ctx.stream.topic(&name) else { continue };
        let parts = parts.unwrap_or_else(|| (0..topic.partition_count()).collect());
        let ps = parts
            .into_iter()
            .filter_map(|i| {
                let p = topic.partition(i)?;
                let (size, lag) = match p.loaded() {
                    Some(log) => {
                        let st = log.stats();
                        (st.local_bytes + st.remote_bytes, st.log_end - st.high_watermark)
                    }
                    None => (dir_size(&ctx.b.config.partition_dir(&topic.id, i)), 0),
                };
                Some(DescribeLogDirsPartition::default().with_partition_index(i).with_partition_size(size as i64).with_offset_lag(lag))
            })
            .collect();
        topics.push(DescribeLogDirsTopic::default().with_name(TopicName(s(name))).with_partitions(ps));
    }
    let dir = ctx.b.config.home.join("data").display().to_string();
    DescribeLogDirsResponse::default().with_results(vec![
        DescribeLogDirsResult::default().with_log_dir(s(dir)).with_topics(topics).with_total_bytes(-1).with_usable_bytes(-1),
    ])
}

const ACLS_DISABLED: &str = "ACLs are not used: each API key has full access to its stream.";

pub fn describe_acls(_ctx: &Ctx, _req: DescribeAclsRequest) -> DescribeAclsResponse {
    DescribeAclsResponse::default().with_error_code(ResponseError::SecurityDisabled.code()).with_error_message(Some(s(ACLS_DISABLED)))
}

pub fn create_acls(_ctx: &Ctx, req: CreateAclsRequest) -> CreateAclsResponse {
    let results = req
        .creations
        .iter()
        .map(|_| AclCreationResult::default().with_error_code(ResponseError::SecurityDisabled.code()).with_error_message(Some(s(ACLS_DISABLED))))
        .collect();
    CreateAclsResponse::default().with_results(results)
}

pub fn delete_acls(_ctx: &Ctx, req: DeleteAclsRequest) -> DeleteAclsResponse {
    let results = req
        .filters
        .iter()
        .map(|_| DeleteAclsFilterResult::default().with_error_code(ResponseError::SecurityDisabled.code()).with_error_message(Some(s(ACLS_DISABLED))))
        .collect();
    DeleteAclsResponse::default().with_filter_results(results)
}

/// Stream byte-rate limits are exposed as the default `user` quota.
pub fn describe_quotas(ctx: &Ctx, req: DescribeClientQuotasRequest) -> DescribeClientQuotasResponse {
    let only_users = req.components.iter().all(|c| c.entity_type.as_str() == "user");
    let settings = ctx.stream.settings();
    let mut values = Vec::new();
    if settings.ingress_bytes_per_s > 0 {
        values.push(describe_client_quotas_response::ValueData::default().with_key(s("producer_byte_rate")).with_value(settings.ingress_bytes_per_s as f64));
    }
    if settings.egress_bytes_per_s > 0 {
        values.push(describe_client_quotas_response::ValueData::default().with_key(s("consumer_byte_rate")).with_value(settings.egress_bytes_per_s as f64));
    }
    let entries = if only_users && !values.is_empty() {
        vec![describe_client_quotas_response::EntryData::default()
            .with_entity(vec![describe_client_quotas_response::EntityData::default().with_entity_type(s("user")).with_entity_name(None)])
            .with_values(values)]
    } else {
        vec![]
    };
    DescribeClientQuotasResponse::default().with_entries(Some(entries))
}

pub fn alter_quotas(ctx: &Ctx, req: AlterClientQuotasRequest) -> AlterClientQuotasResponse {
    let entries = req
        .entries
        .into_iter()
        .map(|e| {
            let entity: Vec<_> = e
                .entity
                .iter()
                .map(|x| alter_client_quotas_response::EntityData::default().with_entity_type(x.entity_type.clone()).with_entity_name(x.entity_name.clone()))
                .collect();
            let mut r = alter_client_quotas_response::EntryData::default().with_entity(entity);
            if !e.entity.iter().all(|x| x.entity_type.as_str() == "user") {
                r.error_code = ResponseError::InvalidRequest.code();
                r.error_message = Some(s("only user quotas are supported; they apply to the whole stream"));
                return r;
            }
            if let Some(op) = e.ops.iter().find(|o| !matches!(o.key.as_str(), "producer_byte_rate" | "consumer_byte_rate")) {
                r.error_code = ResponseError::InvalidRequest.code();
                r.error_message = Some(s(format!("unsupported quota {}", op.key.as_str())));
                return r;
            }
            if req.validate_only {
                return r;
            }
            let res = ctx.b.update_settings(&ctx.stream, |st| {
                for op in &e.ops {
                    let v = if op.remove { 0 } else { op.value.max(0.0) as u64 };
                    match op.key.as_str() {
                        "producer_byte_rate" => st.ingress_bytes_per_s = v,
                        _ => st.egress_bytes_per_s = v,
                    }
                }
            });
            if let Err(err) = res {
                r.error_code = err.as_i16();
                r.error_message = err.str_message();
            }
            r
        })
        .collect();
    AlterClientQuotasResponse::default().with_entries(entries)
}

pub fn describe_scram(ctx: &Ctx, req: DescribeUserScramCredentialsRequest) -> DescribeUserScramCredentialsResponse {
    let creds = ctx.b.store.credentials(&ctx.stream.id).unwrap_or_default();
    let info = |iterations: u32| {
        vec![
            CredentialInfo::default().with_mechanism(1).with_iterations(iterations as i32),
            CredentialInfo::default().with_mechanism(2).with_iterations(iterations as i32),
        ]
    };
    let results = match req.users {
        None => creds.iter().map(|c| DescribeUserScramCredentialsResult::default().with_user(s(c.username.clone())).with_credential_infos(info(c.iterations))).collect(),
        Some(users) => users
            .into_iter()
            .map(|u| match creds.iter().find(|c| c.username == u.name.as_str()) {
                Some(c) => DescribeUserScramCredentialsResult::default().with_user(u.name).with_credential_infos(info(c.iterations)),
                None => DescribeUserScramCredentialsResult::default()
                    .with_user(u.name.clone())
                    .with_error_code(ResponseError::ResourceNotFound.code())
                    .with_error_message(Some(s(format!("Attempt to describe a user credential that does not exist: {}", u.name.as_str())))),
            })
            .collect(),
    };
    DescribeUserScramCredentialsResponse::default().with_results(results)
}

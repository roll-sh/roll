//! DescribeConfigs, AlterConfigs and IncrementalAlterConfigs for topics, plus a read-only view
//! of the broker configs clients and tools look at.

use super::{Ctx, s};
use crate::broker::{Broker, Topic};
use crate::error::KafkaError;
use crate::topic_config::{self, DEFS, Kind};
use kafka_protocol::ResponseError;
use kafka_protocol::messages::alter_configs_response::{AlterConfigsResourceResponse, AlterConfigsResponse};
use kafka_protocol::messages::describe_configs_response::{DescribeConfigsResourceResult, DescribeConfigsResponse, DescribeConfigsResult, DescribeConfigsSynonym};
use kafka_protocol::messages::incremental_alter_configs_response::{self, IncrementalAlterConfigsResponse};
use kafka_protocol::messages::{AlterConfigsRequest, DescribeConfigsRequest, IncrementalAlterConfigsRequest};
use std::collections::BTreeMap;

pub const TOPIC: i8 = 2;
pub const BROKER: i8 = 4;
pub const BROKER_LOGGER: i8 = 8;

pub const SOURCE_TOPIC: i8 = 1;
pub const SOURCE_STATIC_BROKER: i8 = 4;
pub const SOURCE_DEFAULT: i8 = 5;

pub struct Entry {
    pub name: String,
    pub value: String,
    pub read_only: bool,
    pub source: i8,
    pub kind: Kind,
    pub doc: &'static str,
}

/// Effective configs of a topic (optionally only `keys`).
pub fn topic_configs(b: &Broker, topic: &Topic, keys: Option<&[String]>) -> Vec<Entry> {
    let overrides = topic.configs();
    DEFS.iter()
        .filter(|d| keys.is_none_or(|k| k.iter().any(|x| x == d.name)))
        .map(|d| {
            let (value, default) = topic_config::value(&overrides, &b.config.topics, d.name);
            Entry {
                name: d.name.into(),
                value,
                read_only: false,
                source: if default { SOURCE_DEFAULT } else { SOURCE_TOPIC },
                kind: d.kind,
                doc: d.doc,
            }
        })
        .collect()
}

/// What the listener speaks: SASL is optional when a default stream takes unauthenticated
/// connections.
fn listener_protocol(b: &Broker) -> &'static str {
    match (b.config.tls.is_some(), b.config.default_stream.is_some()) {
        (true, true) => "SSL",
        (true, false) => "SASL_SSL",
        (false, true) => "PLAINTEXT",
        (false, false) => "SASL_PLAINTEXT",
    }
}

fn broker_configs(ctx: &Ctx) -> Vec<Entry> {
    let b = &ctx.b;
    let c = &b.config;
    let st = ctx.stream.settings();
    let e = |name: &str, value: String, kind: Kind| Entry { name: name.into(), value, read_only: true, source: SOURCE_STATIC_BROKER, kind, doc: "" };
    vec![
        e("advertised.listeners", format!("{}://{}:{}", listener_protocol(b), c.advertised_host, c.advertised_port), Kind::String),
        e("auto.create.topics.enable", st.auto_create_topics.to_string(), Kind::Bool),
        e("broker.id", c.node_id.to_string(), Kind::Int),
        e("compression.type", "producer".into(), Kind::String),
        e("default.replication.factor", "1".into(), Kind::Int),
        e("delete.topic.enable", "true".into(), Kind::Bool),
        e("group.initial.rebalance.delay.ms", c.groups.initial_rebalance_delay_ms.to_string(), Kind::Int),
        e("group.max.session.timeout.ms", c.groups.max_session_timeout_ms.to_string(), Kind::Int),
        e("group.min.session.timeout.ms", c.groups.min_session_timeout_ms.to_string(), Kind::Int),
        e("log.local.retention.bytes", c.topics.local_retention_bytes.to_string(), Kind::Long),
        e("log.local.retention.ms", c.topics.local_retention_ms.to_string(), Kind::Long),
        e("log.message.timestamp.type", "CreateTime".into(), Kind::String),
        e("log.retention.bytes", "-1".into(), Kind::Long),
        e("log.retention.ms", st.retention_ms.unwrap_or(c.topics.retention_ms).to_string(), Kind::Long),
        e("log.segment.bytes", c.topics.segment_bytes.to_string(), Kind::Int),
        e("message.max.bytes", c.topics.max_message_bytes.to_string(), Kind::Int),
        e("min.insync.replicas", "1".into(), Kind::Int),
        e("node.id", c.node_id.to_string(), Kind::Int),
        e("num.partitions", st.default_partitions.to_string(), Kind::Int),
        e("offsets.retention.minutes", (c.groups.offsets_retention_ms / 60_000).to_string(), Kind::Int),
        e("offsets.topic.replication.factor", "1".into(), Kind::Short),
        e("producer.id.expiration.ms", c.txn.producer_id_expiration_ms.to_string(), Kind::Int),
        e("remote.log.storage.system.enable", "true".into(), Kind::Bool),
        e("socket.request.max.bytes", c.max_request_bytes.to_string(), Kind::Int),
        e("transaction.max.timeout.ms", c.txn.max_timeout_ms.to_string(), Kind::Int),
        e("transaction.state.log.min.isr", "1".into(), Kind::Int),
        e("transaction.state.log.replication.factor", "1".into(), Kind::Short),
        e("transactional.id.expiration.ms", c.txn.id_expiration_ms.to_string(), Kind::Int),
    ]
}

pub fn describe(ctx: &Ctx, req: DescribeConfigsRequest) -> DescribeConfigsResponse {
    let results = req
        .resources
        .into_iter()
        .map(|r| {
            // Null means every config; so does an empty list (some clients send that).
            let keys: Option<Vec<String>> = r.configuration_keys.as_ref().filter(|k| !k.is_empty()).map(|k| k.iter().map(|x| x.to_string()).collect());
            let mut out = DescribeConfigsResult::default().with_resource_type(r.resource_type).with_resource_name(r.resource_name.clone());
            let entries = match r.resource_type {
                TOPIC => match ctx.stream.topic(&r.resource_name) {
                    Some(t) => topic_configs(&ctx.b, &t, keys.as_deref()),
                    None => {
                        out.error_code = ResponseError::UnknownTopicOrPartition.code();
                        out.error_message = Some(s(format!("Topic {} does not exist", r.resource_name.as_str())));
                        return out;
                    }
                },
                BROKER => {
                    if !r.resource_name.is_empty() && r.resource_name.as_str() != ctx.b.config.node_id.to_string() {
                        out.error_code = ResponseError::InvalidRequest.code();
                        out.error_message = Some(s(format!("Unexpected broker id {}", r.resource_name.as_str())));
                        return out;
                    }
                    broker_configs(ctx).into_iter().filter(|e| keys.as_ref().is_none_or(|k| k.contains(&e.name))).collect()
                }
                BROKER_LOGGER => vec![],
                _ => {
                    out.error_code = ResponseError::InvalidRequest.code();
                    out.error_message = Some(s("unsupported resource type"));
                    return out;
                }
            };
            out.configs = entries
                .into_iter()
                .map(|e| {
                    let synonyms = if req.include_synonyms {
                        vec![DescribeConfigsSynonym::default().with_name(s(e.name.clone())).with_value(Some(s(e.value.clone()))).with_source(e.source)]
                    } else {
                        vec![]
                    };
                    DescribeConfigsResourceResult::default()
                        .with_name(s(e.name))
                        .with_value(Some(s(e.value)))
                        .with_read_only(e.read_only)
                        .with_config_source(e.source)
                        .with_is_sensitive(false)
                        .with_synonyms(synonyms)
                        .with_config_type(e.kind.code())
                        .with_documentation(if req.include_documentation && !e.doc.is_empty() { Some(s(e.doc)) } else { None })
                })
                .collect();
            out
        })
        .collect();
    DescribeConfigsResponse::default().with_results(results)
}

fn not_topic(resource_type: i8) -> KafkaError {
    if resource_type == BROKER || resource_type == BROKER_LOGGER {
        KafkaError::new(ResponseError::PolicyViolation, "broker configs are managed by roll; set stream settings instead")
    } else {
        KafkaError::invalid_request("unsupported resource type")
    }
}

pub fn alter(ctx: &Ctx, req: AlterConfigsRequest) -> AlterConfigsResponse {
    let responses = req
        .resources
        .into_iter()
        .map(|r| {
            let result = if r.resource_type != TOPIC {
                Err(not_topic(r.resource_type))
            } else {
                match ctx.stream.topic(&r.resource_name) {
                    None => Err(KafkaError::code(ResponseError::UnknownTopicOrPartition)),
                    Some(t) => {
                        // Non-incremental: the given set replaces every override.
                        let configs: BTreeMap<String, String> =
                            r.configs.iter().filter_map(|c| c.value.as_ref().map(|v| (c.name.to_string(), v.to_string()))).collect();
                        ctx.b.set_topic_configs(&t, configs, req.validate_only)
                    }
                }
            };
            let mut out = AlterConfigsResourceResponse::default().with_resource_type(r.resource_type).with_resource_name(r.resource_name);
            if let Err(e) = result {
                out.error_code = e.as_i16();
                out.error_message = e.str_message();
            }
            out
        })
        .collect();
    AlterConfigsResponse::default().with_responses(responses)
}

pub fn incremental_alter(ctx: &Ctx, req: IncrementalAlterConfigsRequest) -> IncrementalAlterConfigsResponse {
    let responses = req
        .resources
        .into_iter()
        .map(|r| {
            let result = if r.resource_type != TOPIC {
                Err(not_topic(r.resource_type))
            } else {
                match ctx.stream.topic(&r.resource_name) {
                    None => Err(KafkaError::code(ResponseError::UnknownTopicOrPartition)),
                    Some(t) => {
                        let mut configs = t.configs();
                        let mut err = None;
                        for c in &r.configs {
                            let name = c.name.to_string();
                            let value = c.value.as_ref().map(|v| v.to_string()).unwrap_or_default();
                            let is_list = topic_config::find(&name).is_some_and(|d| d.kind == Kind::List);
                            match c.config_operation {
                                0 => {
                                    configs.insert(name, value);
                                }
                                1 => {
                                    configs.remove(&name);
                                }
                                2 | 3 if !is_list => {
                                    err = Some(KafkaError::new(ResponseError::InvalidConfig, format!("{name} is not a list config")));
                                }
                                2 => {
                                    let current = topic_config::value(&configs, &ctx.b.config.topics, &name).0;
                                    let mut items: Vec<String> = current.split(',').filter(|x| !x.is_empty()).map(String::from).collect();
                                    for v in value.split(',').map(str::trim).filter(|x| !x.is_empty()) {
                                        if !items.iter().any(|i| i == v) {
                                            items.push(v.to_string());
                                        }
                                    }
                                    configs.insert(name, items.join(","));
                                }
                                3 => {
                                    let current = topic_config::value(&configs, &ctx.b.config.topics, &name).0;
                                    let remove: Vec<&str> = value.split(',').map(str::trim).collect();
                                    let items: Vec<&str> = current.split(',').filter(|x| !x.is_empty() && !remove.contains(x)).collect();
                                    configs.insert(name, items.join(","));
                                }
                                _ => err = Some(KafkaError::invalid_request("unknown config operation")),
                            }
                        }
                        match err {
                            Some(e) => Err(e),
                            None => ctx.b.set_topic_configs(&t, configs, req.validate_only),
                        }
                    }
                }
            };
            let mut out = incremental_alter_configs_response::AlterConfigsResourceResponse::default()
                .with_resource_type(r.resource_type)
                .with_resource_name(r.resource_name);
            if let Err(e) = result {
                out.error_code = e.as_i16();
                out.error_message = e.str_message();
            }
            out
        })
        .collect();
    IncrementalAlterConfigsResponse::default().with_responses(responses)
}

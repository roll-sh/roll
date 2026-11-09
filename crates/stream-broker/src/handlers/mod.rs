//! Kafka API handlers. Each takes a decoded request and returns a response now, or a future for
//! work that waits (it runs as a task while the connection keeps reading).

pub mod admin;
pub mod configs;
pub mod fetch;
pub mod groups;
pub mod metadata;
pub mod offsets;
pub mod produce;
pub mod sasl;
pub mod txn;

use crate::broker::{Broker, Stream};
use crate::server::Session;
use futures::future::BoxFuture;
use kafka_protocol::messages::api_versions_response::{ApiVersion, ApiVersionsResponse};
use kafka_protocol::messages::{ApiKey, RequestHeader, RequestKind, ResponseKind};
use kafka_protocol::protocol::StrBytes;
use kafka_protocol::ResponseError;
use std::sync::Arc;
use tracing::debug;

#[allow(clippy::large_enum_variant)]
pub enum Reply {
    Now(ResponseKind),
    Later(BoxFuture<'static, ResponseKind>),
    /// `acks=0` produce: the client expects nothing back.
    None,
    Close,
}

pub struct Handled {
    pub reply: Reply,
    pub throttle_ms: u32,
}

pub fn now(r: impl Into<ResponseKind>) -> Handled {
    Handled { reply: Reply::Now(r.into()), throttle_ms: 0 }
}

pub fn later(f: impl std::future::Future<Output = ResponseKind> + Send + 'static) -> Handled {
    Handled { reply: Reply::Later(Box::pin(f)), throttle_ms: 0 }
}

/// What a handler needs: owned, so deferred work can outlive the connection's borrow.
#[derive(Clone)]
pub struct Ctx {
    pub b: Arc<Broker>,
    pub stream: Arc<Stream>,
    pub version: i16,
    pub client_id: String,
    pub client_host: String,
}

/// APIs and versions we serve (advertised by ApiVersions).
pub const APIS: &[(ApiKey, i16, i16)] = &[
    (ApiKey::Produce, 3, 13),
    (ApiKey::Fetch, 4, 18),
    (ApiKey::ListOffsets, 1, 10),
    (ApiKey::Metadata, 0, 13),
    (ApiKey::OffsetCommit, 2, 9),
    (ApiKey::OffsetFetch, 1, 9),
    (ApiKey::FindCoordinator, 0, 6),
    (ApiKey::JoinGroup, 0, 9),
    (ApiKey::Heartbeat, 0, 4),
    (ApiKey::LeaveGroup, 0, 5),
    (ApiKey::SyncGroup, 0, 5),
    (ApiKey::DescribeGroups, 0, 6),
    (ApiKey::ListGroups, 0, 5),
    (ApiKey::SaslHandshake, 0, 1),
    (ApiKey::ApiVersions, 0, 4),
    (ApiKey::CreateTopics, 2, 7),
    (ApiKey::DeleteTopics, 1, 6),
    (ApiKey::DeleteRecords, 0, 2),
    (ApiKey::InitProducerId, 0, 5),
    (ApiKey::OffsetForLeaderEpoch, 2, 4),
    (ApiKey::AddPartitionsToTxn, 0, 4),
    (ApiKey::AddOffsetsToTxn, 0, 3),
    (ApiKey::EndTxn, 0, 4),
    (ApiKey::TxnOffsetCommit, 0, 4),
    (ApiKey::DescribeAcls, 1, 3),
    (ApiKey::CreateAcls, 1, 3),
    (ApiKey::DeleteAcls, 1, 3),
    (ApiKey::DescribeConfigs, 1, 4),
    (ApiKey::AlterConfigs, 0, 2),
    (ApiKey::DescribeLogDirs, 1, 4),
    (ApiKey::SaslAuthenticate, 0, 2),
    (ApiKey::CreatePartitions, 0, 3),
    (ApiKey::DeleteGroups, 0, 2),
    (ApiKey::ElectLeaders, 0, 2),
    (ApiKey::IncrementalAlterConfigs, 0, 1),
    (ApiKey::ListPartitionReassignments, 0, 0),
    (ApiKey::OffsetDelete, 0, 0),
    (ApiKey::DescribeClientQuotas, 0, 1),
    (ApiKey::AlterClientQuotas, 0, 1),
    (ApiKey::DescribeUserScramCredentials, 0, 0),
    (ApiKey::DescribeCluster, 0, 2),
    (ApiKey::DescribeProducers, 0, 0),
    (ApiKey::DescribeTransactions, 0, 0),
    (ApiKey::ListTransactions, 0, 2),
    (ApiKey::DescribeTopicPartitions, 0, 0),
    (ApiKey::DescribeQuorum, 0, 2),
];

pub fn supported(api: ApiKey, version: i16) -> bool {
    APIS.iter().any(|(k, min, max)| *k == api && (*min..=*max).contains(&version))
}

fn api_list() -> Vec<ApiVersion> {
    APIS.iter()
        .map(|(k, min, max)| ApiVersion::default().with_api_key(*k as i16).with_min_version(*min).with_max_version(*max))
        .collect()
}

pub fn api_versions_unsupported() -> ResponseKind {
    ApiVersionsResponse::default().with_error_code(ResponseError::UnsupportedVersion.code()).with_api_keys(api_list()).into()
}

fn api_versions() -> ApiVersionsResponse {
    ApiVersionsResponse::default().with_api_keys(api_list()).with_finalized_features_epoch(-1)
}

pub fn s(v: impl Into<String>) -> StrBytes {
    StrBytes::from_string(v.into())
}

pub fn opt_s(v: impl Into<String>) -> Option<StrBytes> {
    Some(s(v))
}

pub async fn handle(b: &Arc<Broker>, session: &mut Session, header: &RequestHeader, api: ApiKey, version: i16, mut body: bytes::Bytes) -> Handled {
    let req = match RequestKind::decode(api, &mut body, version) {
        Ok(r) => r,
        Err(e) => {
            debug!(?api, version, error = %e, "malformed request; closing");
            return Handled { reply: Reply::Close, throttle_ms: 0 };
        }
    };
    let _ = header;
    match req {
        RequestKind::ApiVersions(_) => now(api_versions()),
        RequestKind::SaslHandshake(r) => sasl::handshake(session, version, r),
        RequestKind::SaslAuthenticate(r) => sasl::authenticate(b, session, r),
        req => {
            let stream = session.stream.clone().expect("checked by the connection");
            let ctx = Ctx { b: b.clone(), stream, version, client_id: session.client_id.clone(), client_host: format!("/{}", session.peer.ip()) };
            dispatch(ctx, req).await
        }
    }
}

async fn dispatch(ctx: Ctx, req: RequestKind) -> Handled {
    match req {
        RequestKind::Produce(r) => produce::produce(ctx, r).await,
        RequestKind::Fetch(r) => fetch::fetch(ctx, r),
        RequestKind::ListOffsets(r) => offsets::list_offsets(ctx, r),
        RequestKind::OffsetForLeaderEpoch(r) => now(offsets::leader_epoch(&ctx, r)),
        RequestKind::Metadata(r) => now(metadata::metadata(&ctx, r)),
        RequestKind::DescribeCluster(r) => now(metadata::describe_cluster(&ctx, r)),
        RequestKind::DescribeQuorum(r) => now(metadata::describe_quorum(&ctx, r)),
        RequestKind::DescribeTopicPartitions(r) => now(metadata::describe_topic_partitions(&ctx, r)),
        RequestKind::FindCoordinator(r) => now(groups::find_coordinator(&ctx, r)),
        RequestKind::JoinGroup(r) => groups::join(ctx, r),
        RequestKind::SyncGroup(r) => groups::sync(ctx, r),
        RequestKind::Heartbeat(r) => now(groups::heartbeat(&ctx, r)),
        RequestKind::LeaveGroup(r) => now(groups::leave(&ctx, r)),
        RequestKind::OffsetCommit(r) => now(groups::offset_commit(&ctx, r)),
        RequestKind::OffsetFetch(r) => now(groups::offset_fetch(&ctx, r)),
        RequestKind::OffsetDelete(r) => now(groups::offset_delete(&ctx, r)),
        RequestKind::DescribeGroups(r) => now(groups::describe(&ctx, r)),
        RequestKind::ListGroups(r) => now(groups::list(&ctx, r)),
        RequestKind::DeleteGroups(r) => now(groups::delete(&ctx, r)),
        RequestKind::InitProducerId(r) => txn::init_producer_id(ctx, r),
        RequestKind::AddPartitionsToTxn(r) => txn::add_partitions(ctx, r),
        RequestKind::AddOffsetsToTxn(r) => txn::add_offsets(ctx, r),
        RequestKind::EndTxn(r) => txn::end(ctx, r),
        RequestKind::TxnOffsetCommit(r) => txn::offset_commit(ctx, r),
        RequestKind::DescribeTransactions(r) => now(txn::describe(&ctx, r)),
        RequestKind::ListTransactions(r) => now(txn::list(&ctx, r)),
        RequestKind::DescribeProducers(r) => now(txn::describe_producers(&ctx, r)),
        RequestKind::CreateTopics(r) => now(admin::create_topics(&ctx, r)),
        RequestKind::DeleteTopics(r) => admin::delete_topics(ctx, r),
        RequestKind::CreatePartitions(r) => now(admin::create_partitions(&ctx, r)),
        RequestKind::DeleteRecords(r) => admin::delete_records(ctx, r),
        RequestKind::ElectLeaders(r) => now(admin::elect_leaders(&ctx, r)),
        RequestKind::ListPartitionReassignments(r) => now(admin::list_reassignments(&ctx, r)),
        RequestKind::DescribeLogDirs(r) => now(admin::describe_log_dirs(&ctx, r)),
        RequestKind::DescribeAcls(r) => now(admin::describe_acls(&ctx, r)),
        RequestKind::CreateAcls(r) => now(admin::create_acls(&ctx, r)),
        RequestKind::DeleteAcls(r) => now(admin::delete_acls(&ctx, r)),
        RequestKind::DescribeClientQuotas(r) => now(admin::describe_quotas(&ctx, r)),
        RequestKind::AlterClientQuotas(r) => now(admin::alter_quotas(&ctx, r)),
        RequestKind::DescribeUserScramCredentials(r) => now(admin::describe_scram(&ctx, r)),
        RequestKind::DescribeConfigs(r) => now(configs::describe(&ctx, r)),
        RequestKind::AlterConfigs(r) => now(configs::alter(&ctx, r)),
        RequestKind::IncrementalAlterConfigs(r) => now(configs::incremental_alter(&ctx, r)),
        _ => Handled { reply: Reply::Close, throttle_ms: 0 },
    }
}

//! Idempotent and transactional producer APIs.

use super::{Ctx, Handled, later, s};
use crate::ids::{now, now_ms};
use crate::store::OffsetRow;
use crate::txn::ONGOING;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::add_offsets_to_txn_response::AddOffsetsToTxnResponse;
use kafka_protocol::messages::add_partitions_to_txn_response::{
    AddPartitionsToTxnPartitionResult, AddPartitionsToTxnResponse, AddPartitionsToTxnResult, AddPartitionsToTxnTopicResult,
};
use kafka_protocol::messages::add_partitions_to_txn_request::AddPartitionsToTxnTopic;
use kafka_protocol::messages::describe_producers_response::{DescribeProducersResponse, PartitionResponse, ProducerState, TopicResponse};
use kafka_protocol::messages::describe_transactions_response::{DescribeTransactionsResponse, TopicData, TransactionState};
use kafka_protocol::messages::end_txn_response::EndTxnResponse;
use kafka_protocol::messages::init_producer_id_response::InitProducerIdResponse;
use kafka_protocol::messages::list_transactions_response::{self, ListTransactionsResponse};
use kafka_protocol::messages::txn_offset_commit_response::{TxnOffsetCommitResponse, TxnOffsetCommitResponsePartition, TxnOffsetCommitResponseTopic};
use kafka_protocol::messages::{
    AddOffsetsToTxnRequest, AddPartitionsToTxnRequest, DescribeProducersRequest, DescribeTransactionsRequest, EndTxnRequest,
    InitProducerIdRequest, ListTransactionsRequest, ProducerId, ResponseKind, TopicName, TransactionalId, TxnOffsetCommitRequest,
};

pub fn init_producer_id(ctx: Ctx, req: InitProducerIdRequest) -> Handled {
    later(async move {
        let txn_id = req.transactional_id.as_ref().map(|t| t.to_string());
        let r = ctx.b.txns.init(&ctx.b, &ctx.stream.id, txn_id.as_deref(), req.transaction_timeout_ms, req.producer_id.0, req.producer_epoch).await;
        // Older clients don't know PRODUCER_FENCED.
        let error = if r.error == ResponseError::ProducerFenced.code() && ctx.version < 4 { ResponseError::InvalidProducerEpoch.code() } else { r.error };
        ResponseKind::InitProducerId(
            InitProducerIdResponse::default().with_error_code(error).with_producer_id(ProducerId(r.producer_id)).with_producer_epoch(r.epoch),
        )
    })
}

fn fenced(code: ResponseError, version: i16, since: i16) -> i16 {
    if code == ResponseError::ProducerFenced && version < since { ResponseError::InvalidProducerEpoch.code() } else { code.code() }
}

/// Add the partitions of one transaction; returns per-topic results.
async fn add_one(ctx: &Ctx, txn_id: &TransactionalId, pid: i64, epoch: i16, topics: &[AddPartitionsToTxnTopic], verify_only: bool) -> Vec<AddPartitionsToTxnTopicResult> {
    let mut bad = false;
    let mut valid = Vec::new();
    let mut codes: Vec<Vec<(i32, i16)>> = Vec::new();
    for t in topics {
        let topic = ctx.stream.topic(&t.name);
        let mut c = Vec::new();
        for &p in &t.partitions {
            if topic.as_ref().and_then(|x| x.partition(p)).is_some() {
                valid.push((t.name.to_string(), p));
                c.push((p, 0));
            } else {
                bad = true;
                c.push((p, ResponseError::UnknownTopicOrPartition.code()));
            }
        }
        codes.push(c);
    }
    let outcome = if bad {
        Err(ResponseError::OperationNotAttempted)
    } else if verify_only {
        let mut r = Ok(());
        for (t, p) in &valid {
            if let Err(e) = ctx.b.txns.verify(&ctx.stream.id, pid, epoch, t, *p).await {
                r = Err(e);
                break;
            }
        }
        r
    } else {
        ctx.b.txns.add_partitions(&ctx.b, &ctx.stream.id, txn_id, pid, epoch, &valid).await
    };
    topics
        .iter()
        .zip(codes)
        .map(|(t, c)| {
            let results = c
                .into_iter()
                .map(|(p, code)| {
                    let code = match (&outcome, code) {
                        (_, c) if c != 0 => c,
                        (Err(ResponseError::OperationNotAttempted), _) => ResponseError::OperationNotAttempted.code(),
                        (Err(e), _) => fenced(*e, ctx.version, 2),
                        (Ok(()), _) => 0,
                    };
                    AddPartitionsToTxnPartitionResult::default().with_partition_index(p).with_partition_error_code(code)
                })
                .collect();
            AddPartitionsToTxnTopicResult::default().with_name(t.name.clone()).with_results_by_partition(results)
        })
        .collect()
}

pub fn add_partitions(ctx: Ctx, req: AddPartitionsToTxnRequest) -> Handled {
    later(async move {
        if ctx.version <= 3 {
            let results =
                add_one(&ctx, &req.v3_and_below_transactional_id, req.v3_and_below_producer_id.0, req.v3_and_below_producer_epoch, &req.v3_and_below_topics, false).await;
            return ResponseKind::AddPartitionsToTxn(AddPartitionsToTxnResponse::default().with_results_by_topic_v3_and_below(results));
        }
        let mut out = Vec::new();
        for t in &req.transactions {
            let results = add_one(&ctx, &t.transactional_id, t.producer_id.0, t.producer_epoch, &t.topics, t.verify_only).await;
            out.push(AddPartitionsToTxnResult::default().with_transactional_id(t.transactional_id.clone()).with_topic_results(results));
        }
        ResponseKind::AddPartitionsToTxn(AddPartitionsToTxnResponse::default().with_results_by_transaction(out))
    })
}

pub fn add_offsets(ctx: Ctx, req: AddOffsetsToTxnRequest) -> Handled {
    later(async move {
        let code = match ctx.b.txns.add_offsets(&ctx.b, &ctx.stream.id, &req.transactional_id, req.producer_id.0, req.producer_epoch, &req.group_id).await {
            Ok(()) => 0,
            Err(e) => fenced(e, ctx.version, 2),
        };
        ResponseKind::AddOffsetsToTxn(AddOffsetsToTxnResponse::default().with_error_code(code))
    })
}

pub fn end(ctx: Ctx, req: EndTxnRequest) -> Handled {
    later(async move {
        let code = match ctx.b.txns.end(&ctx.b, &ctx.stream.id, &req.transactional_id, req.producer_id.0, req.producer_epoch, req.committed).await {
            Ok(()) => 0,
            Err(e) => fenced(e, ctx.version, 2),
        };
        ResponseKind::EndTxn(EndTxnResponse::default().with_error_code(code).with_producer_id(req.producer_id).with_producer_epoch(req.producer_epoch))
    })
}

pub fn offset_commit(ctx: Ctx, req: TxnOffsetCommitRequest) -> Handled {
    later(async move {
        let check = ctx.b.txns.check_offsets(&ctx.stream.id, &req.transactional_id, req.producer_id.0, req.producer_epoch, &req.group_id).await;
        // v3+ carries the member's generation so zombie consumers are fenced.
        let check = check.and_then(|()| {
            if ctx.version >= 3 && req.generation_id >= 0 {
                let instance = req.group_instance_id.as_ref().map(|i| i.to_string());
                ctx.b.groups.check_commit(&ctx.b, &ctx.stream.id, &req.group_id, &req.member_id, instance.as_deref(), req.generation_id)
            } else {
                Ok(())
            }
        });
        let at = now();
        let mut rows = Vec::new();
        let topics = req
            .topics
            .into_iter()
            .map(|t| {
                let topic = ctx.stream.topic(&t.name);
                let partitions = t
                    .partitions
                    .into_iter()
                    .map(|p| {
                        let code = match &check {
                            Err(e) => fenced(*e, ctx.version, 2),
                            Ok(()) if topic.as_ref().and_then(|x| x.partition(p.partition_index)).is_none() => ResponseError::UnknownTopicOrPartition.code(),
                            Ok(()) if p.committed_metadata.as_ref().is_some_and(|m| m.len() > super::groups::MAX_METADATA) => ResponseError::OffsetMetadataTooLarge.code(),
                            Ok(()) => {
                                rows.push(OffsetRow {
                                    topic: t.name.to_string(),
                                    partition: p.partition_index,
                                    offset: p.committed_offset,
                                    leader_epoch: p.committed_leader_epoch,
                                    metadata: p.committed_metadata.map(|m| m.to_string()),
                                    committed_at: at,
                                });
                                0
                            }
                        };
                        TxnOffsetCommitResponsePartition::default().with_partition_index(p.partition_index).with_error_code(code)
                    })
                    .collect();
                TxnOffsetCommitResponseTopic::default().with_name(t.name).with_partitions(partitions)
            })
            .collect();
        if !rows.is_empty() {
            ctx.b.groups.stage_txn_offsets(&ctx.stream.id, &req.group_id, req.producer_id.0, rows);
        }
        ResponseKind::TxnOffsetCommit(TxnOffsetCommitResponse::default().with_topics(topics))
    })
}

pub fn describe(ctx: &Ctx, req: DescribeTransactionsRequest) -> DescribeTransactionsResponse {
    let states = req
        .transactional_ids
        .into_iter()
        .map(|id| match ctx.b.txns.describe(&ctx.stream.id, &id) {
            Some(t) => TransactionState::default()
                .with_transactional_id(id)
                .with_transaction_state(s(t.state.clone()))
                .with_transaction_timeout_ms(t.timeout_ms)
                .with_transaction_start_time_ms(if t.state == ONGOING { t.started_at } else { -1 })
                .with_producer_id(ProducerId(t.producer_id))
                .with_producer_epoch(t.epoch)
                .with_topics(t.partitions.into_iter().map(|(n, ps)| TopicData::default().with_topic(TopicName(s(n))).with_partitions(ps)).collect()),
            None => TransactionState::default().with_transactional_id(id).with_error_code(ResponseError::TransactionalIdNotFound.code()),
        })
        .collect();
    DescribeTransactionsResponse::default().with_transaction_states(states)
}

pub fn list(ctx: &Ctx, req: ListTransactionsRequest) -> ListTransactionsResponse {
    let known = ["Empty", "Ongoing", "PrepareCommit", "PrepareAbort", "CompleteCommit", "CompleteAbort", "Dead", "PrepareEpochFence"];
    let unknown: Vec<_> = req.state_filters.iter().filter(|f| !known.contains(&f.as_str())).cloned().collect();
    let pattern = req.transactional_id_pattern.as_ref().map(|p| p.to_string()).filter(|p| !p.is_empty());
    let now = now_ms();
    let states = ctx
        .b
        .txns
        .list(&ctx.stream.id)
        .into_iter()
        .filter(|t| req.state_filters.is_empty() || req.state_filters.iter().any(|f| f.as_str() == t.state))
        .filter(|t| req.producer_id_filters.is_empty() || req.producer_id_filters.iter().any(|p| p.0 == t.producer_id))
        .filter(|t| req.duration_filter <= 0 || (t.state == ONGOING && now - t.started_at >= req.duration_filter))
        .filter(|t| pattern.as_ref().is_none_or(|p| glob(p, &t.txn_id)))
        .map(|t| {
            list_transactions_response::TransactionState::default()
                .with_transactional_id(TransactionalId(s(t.txn_id)))
                .with_producer_id(ProducerId(t.producer_id))
                .with_transaction_state(s(t.state))
        })
        .collect();
    ListTransactionsResponse::default().with_unknown_state_filters(unknown).with_transaction_states(states)
}

/// Java-regex-lite matching for `transactional_id_pattern`: `.*` wildcards, otherwise literal.
fn glob(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split(".*").collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(p) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(at) => rest = &rest[at + p.len()..],
                None => return false,
            }
        }
    }
    true
}

pub fn describe_producers(ctx: &Ctx, req: DescribeProducersRequest) -> DescribeProducersResponse {
    let topics = req
        .topics
        .into_iter()
        .map(|t| {
            let topic = ctx.stream.topic(&t.name);
            let partitions = t
                .partition_indexes
                .into_iter()
                .map(|i| {
                    let mut pr = PartitionResponse::default().with_partition_index(i);
                    match topic.as_ref().and_then(|x| x.partition(i).map(|p| (x.clone(), p))) {
                        None => pr.error_code = ResponseError::UnknownTopicOrPartition.code(),
                        Some((topic, p)) => match ctx.b.log(&topic, &p) {
                            Ok(log) => {
                                pr.active_producers = log
                                    .producers()
                                    .into_iter()
                                    .map(|(id, epoch, seq, ts, ce, txn)| {
                                        ProducerState::default()
                                            .with_producer_id(ProducerId(id))
                                            .with_producer_epoch(epoch as i32)
                                            .with_last_sequence(seq)
                                            .with_last_timestamp(ts)
                                            .with_coordinator_epoch(ce)
                                            .with_current_txn_start_offset(txn.unwrap_or(-1))
                                    })
                                    .collect()
                            }
                            Err(e) => pr.error_code = e.as_i16(),
                        },
                    }
                    pr
                })
                .collect();
            TopicResponse::default().with_name(t.name).with_partitions(partitions)
        })
        .collect();
    DescribeProducersResponse::default().with_topics(topics)
}

#[cfg(test)]
mod tests {
    #[test]
    fn glob() {
        assert!(super::glob("app-.*", "app-1"));
        assert!(!super::glob("app-.*", "web-1"));
        assert!(super::glob(".*-txn", "orders-txn"));
        assert!(super::glob("exact", "exact"));
        assert!(super::glob("a.*b.*c", "a1b2c"));
    }
}

//! Produce: record batches are validated and appended as received (zero-copy apart from the
//! offset patch); `acks=all` waits for the group-commit fsync.

use super::{Ctx, Handled, Reply, later};
use crate::broker::{Broker, Partition, Stream, Topic};
use crate::error::KafkaError;
use crate::fleet::Serving;
use crate::ids::now_ms;
use bytes::Bytes;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::produce_response::{PartitionProduceResponse, ProduceResponse, TopicProduceResponse};
use kafka_protocol::messages::{ProduceRequest, ResponseKind};
use std::sync::Arc;
use std::time::Duration;
use stream_log::{AppendError, AppendOptions, Appended, batch};
use tokio::sync::watch;

pub fn append_error(e: &AppendError) -> KafkaError {
    let code = match e {
        AppendError::Corrupt(_) => ResponseError::CorruptMessage,
        AppendError::TooLarge(_) => ResponseError::MessageTooLarge,
        AppendError::InvalidEpoch => ResponseError::InvalidProducerEpoch,
        AppendError::OutOfOrder => ResponseError::OutOfOrderSequenceNumber,
        AppendError::UnknownProducer => ResponseError::UnknownProducerId,
        AppendError::InvalidTimestamp => ResponseError::InvalidTimestamp,
        AppendError::InvalidRecord(_) => ResponseError::InvalidRecord,
        AppendError::Io(_) => ResponseError::KafkaStorageError,
    };
    KafkaError::new(code, e.to_string())
}

/// Something to await before the append is acknowledged: the high watermark reaching it, and
/// on the fleet the stream staying here meanwhile.
pub struct Durable(Option<(watch::Receiver<i64>, i64)>, Option<watch::Receiver<Serving>>);

impl Durable {
    /// `REQUEST_TIMED_OUT` if it isn't durable in time; `NOT_LEADER_OR_FOLLOWER` if the stream
    /// left this node first (the client retries on the new owner).
    pub async fn wait(self, timeout: Duration) -> Result<(), ResponseError> {
        let Some((mut rx, target)) = self.0 else { return Ok(()) };
        let moved = async {
            match self.1 {
                Some(mut s) => {
                    let now = *s.borrow();
                    let _ = s.wait_for(|v| *v != now).await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            r = tokio::time::timeout(timeout, rx.wait_for(|hw| *hw >= target)) => match r {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(_)) => Err(ResponseError::NotLeaderOrFollower),
                Err(_) => Err(ResponseError::RequestTimedOut),
            },
            _ = moved => Err(ResponseError::NotLeaderOrFollower),
        }
    }
}

/// Append batches to a partition. Returns the result and what to wait on for `acks=all`.
pub fn append(b: &Broker, stream: &Stream, topic: &Topic, p: &Arc<Partition>, batches: &[Bytes]) -> Result<(Appended, Durable), KafkaError> {
    if b.fleet.is_some() && !stream.tenure.has_room() {
        // Object storage isn't taking the write-ahead log: don't pile up what can't be acknowledged.
        return Err(KafkaError::new(ResponseError::NotEnoughReplicas, "object storage is not keeping up; retry"));
    }
    let log = b.log(topic, p)?;
    let e = topic.effective();
    if e.compact {
        // Compaction keeps the latest record per key, so every record needs one.
        for bt in batches {
            let records = crate::records::decode(bt, None).map_err(|e| KafkaError::new(ResponseError::CorruptMessage, e.to_string()))?;
            if records.iter().any(|r| r.key.is_none()) {
                return Err(KafkaError::new(ResponseError::InvalidRecord, "Compacted topic cannot accept message without key".to_string()));
            }
        }
    }
    let opts = AppendOptions {
        now_ms: now_ms(),
        log_append_time: e.log_append_time,
        max_batch_bytes: e.max_message_bytes,
        max_timestamp_before_ms: e.timestamp_before_ms,
        max_timestamp_after_ms: e.timestamp_after_ms,
        allow_control: false,
    };
    let appended = log.append(batches, opts).map_err(|e| append_error(&e))?;
    if !appended.duplicate {
        stream.usage.record_in(appended.bytes as u64, appended.records as u64);
    }
    let durable = if b.acks_wait() {
        // A retry of a batch the log has is acknowledged once the original is durable.
        if !appended.duplicate {
            b.request_flush(p, &log, appended.bytes);
        }
        Durable(Some((log.subscribe(), appended.last_offset + 1)), b.fleet.as_ref().map(|_| stream.tenure.subscribe()))
    } else {
        Durable(None, None)
    };
    Ok((appended, durable))
}

pub async fn produce(ctx: Ctx, req: ProduceRequest) -> Handled {
    let v = ctx.version;
    let acks = req.acks;
    let txn_id = req.transactional_id.as_ref().map(|t| t.to_string());
    let mut responses = Vec::with_capacity(req.topic_data.len());
    let mut waits = Vec::new();
    let mut bytes_in = 0u64;
    let bad_acks = !matches!(acks, -1..=1);
    for td in req.topic_data {
        let topic = if v >= 13 { ctx.stream.topic_by_id(&td.topic_id) } else { ctx.stream.topic(&td.name) };
        let mut tr = TopicProduceResponse::default().with_name(td.name.clone()).with_topic_id(td.topic_id);
        for pd in td.partition_data {
            let mut pr = PartitionProduceResponse::default().with_index(pd.index).with_base_offset(-1).with_log_append_time_ms(-1).with_log_start_offset(-1);
            let result = (|| {
                if bad_acks {
                    return Err(KafkaError::code(ResponseError::InvalidRequiredAcks));
                }
                let missing = if v >= 13 { ResponseError::UnknownTopicId } else { ResponseError::UnknownTopicOrPartition };
                let topic = topic.as_ref().ok_or(KafkaError::code(missing))?;
                let p = topic.partition(pd.index).ok_or(KafkaError::code(ResponseError::UnknownTopicOrPartition))?;
                let records = pd.records.clone().ok_or(KafkaError::code(ResponseError::CorruptMessage))?;
                bytes_in += records.len() as u64;
                let batches = batch::split(records).map_err(|e| KafkaError::new(ResponseError::CorruptMessage, e.to_string()))?;
                Ok((topic.clone(), p, batches))
            })();
            let outcome = match result {
                Ok((topic, p, batches)) => {
                    // Transactional data must belong to the producer's open transaction (KIP-890).
                    let mut verified = Ok(());
                    for bt in &batches {
                        if let Ok(h) = batch::parse_header(bt)
                            && h.is_transactional()
                        {
                            verified = match &txn_id {
                                None => Err(KafkaError::code(ResponseError::InvalidTxnState)),
                                Some(_) => ctx
                                    .b
                                    .txns
                                    .verify(&ctx.stream.id, h.producer_id, h.producer_epoch, &topic.name, p.index)
                                    .await
                                    .map_err(KafkaError::code),
                            };
                            break;
                        }
                    }
                    verified.and_then(|()| append(&ctx.b, &ctx.stream, &topic, &p, &batches))
                }
                Err(e) => Err(e),
            };
            match outcome {
                Ok((a, durable)) => {
                    pr.base_offset = a.base_offset;
                    pr.log_append_time_ms = a.log_append_time;
                    pr.log_start_offset = a.log_start;
                    if acks == -1 {
                        waits.push((responses.len(), tr.partition_responses.len(), durable));
                    }
                }
                Err(e) => {
                    pr.error_code = e.as_i16();
                    if v >= 8 {
                        pr.error_message = e.msg.map(super::s);
                    }
                }
            }
            tr.partition_responses.push(pr);
        }
        responses.push(tr);
    }
    let rate = ctx.stream.settings().ingress_bytes_per_s;
    let throttle_ms = ctx.stream.ingress.charge(bytes_in, rate);
    let mut resp = ProduceResponse::default().with_responses(responses).with_throttle_time_ms(throttle_ms as i32);
    if acks == 0 {
        return Handled { reply: Reply::None, throttle_ms };
    }
    if waits.is_empty() {
        return Handled { reply: Reply::Now(resp.into()), throttle_ms };
    }
    let timeout = Duration::from_millis(req.timeout_ms.max(1) as u64);
    let mut h = later(async move {
        for (t, p, durable) in waits {
            if let Err(e) = durable.wait(timeout).await {
                let pr = &mut resp.responses[t].partition_responses[p];
                pr.error_code = e.code();
                pr.base_offset = -1;
            }
        }
        ResponseKind::Produce(resp)
    });
    h.throttle_ms = throttle_ms;
    h
}

//! Fetch with long polling: reads come from the in-memory tail, local segments or object
//! storage; if fewer than `min_bytes` are available the request parks until a high watermark
//! moves or `max_wait_ms` passes.

use super::{Ctx, Handled, later};
use crate::broker::{Broker, Partition, Stream, Topic};
use crate::error::KafkaError;
use bytes::Bytes;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::fetch_response::{AbortedTransaction, FetchResponse, FetchableTopicResponse, PartitionData};
use kafka_protocol::messages::{FetchRequest, ProducerId, ResponseKind, TopicName};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream_log::{AbortedTxn, ReadError, ReadPlan, batch, read_local};
use tokio::sync::watch;
use uuid::Uuid;

/// The outcome of reading one partition.
pub struct PartitionRead {
    pub records: Bytes,
    pub high_watermark: i64,
    pub last_stable: i64,
    pub log_start: i64,
    pub aborted: Vec<AbortedTxn>,
}

/// Read from `offset`, up to `max_bytes` (the first batch is always whole).
pub async fn read_partition(
    b: &Broker,
    stream: &Stream,
    topic: &Topic,
    p: &Partition,
    offset: i64,
    max_bytes: usize,
    committed: bool,
) -> Result<PartitionRead, (KafkaError, i64, i64)> {
    let r = read_raw(b, stream, topic, p, offset, max_bytes, committed).await?;
    stream.usage.record_out(r.records.len() as u64);
    Ok(r)
}

/// [`read_partition`] without metering (internal lookups).
pub async fn read_raw(
    b: &Broker,
    stream: &Stream,
    topic: &Topic,
    p: &Partition,
    offset: i64,
    max_bytes: usize,
    committed: bool,
) -> Result<PartitionRead, (KafkaError, i64, i64)> {
    let log = b.log(topic, p).map_err(|e| (e, -1, -1))?;
    let mut attempt = 0;
    let (r, records) = loop {
        let r = match log.read(offset, max_bytes, committed) {
            Ok(r) => r,
            Err(ReadError::OutOfRange { start, .. }) => {
                return Err((KafkaError::code(ResponseError::OffsetOutOfRange), log.high_watermark(), start));
            }
        };
        let (offset, upper, max) = (r.offset, r.upper, r.max_bytes);
        let records = match &r.plan {
            ReadPlan::Ready(bytes) => bytes.clone(),
            ReadPlan::Local(local) => {
                let slack = topic.effective().index_interval_bytes + 64 * 1024;
                let local = local.clone();
                tokio::task::spawn_blocking(move || read_local(&local, offset, upper, max, slack))
                    .await
                    .map_err(|e| (KafkaError::storage(e), r.high_watermark, r.log_start))?
                    .map_err(|e| (KafkaError::storage(e), r.high_watermark, r.log_start))?
            }
            ReadPlan::Remote(chunk) => match b.chunk(stream, chunk).await {
                Ok(data) => batch::slice_for_fetch(&data, offset, upper, max),
                // Compaction (or retention) replaced the chunk after the read was planned.
                Err(_) if attempt < 3 && !log.has_chunk(&chunk.key) => {
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err((KafkaError::storage(e), r.high_watermark, r.log_start)),
            },
        };
        break (r, records);
    };
    let (offset, upper) = (r.offset, r.upper);
    let aborted = if committed && !records.is_empty() { log.aborted_between(offset, upper) } else { vec![] };
    Ok(PartitionRead { records, high_watermark: r.high_watermark, last_stable: r.last_stable, log_start: r.log_start, aborted })
}

struct Slot {
    topic: Option<Arc<Topic>>,
    partition: Option<Arc<Partition>>,
    index: i32,
    offset: i64,
    max_bytes: i32,
    leader_epoch: i32,
}

pub fn fetch(ctx: Ctx, req: FetchRequest) -> Handled {
    let rate = ctx.stream.settings().egress_bytes_per_s;
    let throttle = ctx.stream.egress.charge(0, rate);
    let mut h = later(async move { ResponseKind::Fetch(run(ctx, req).await) });
    h.throttle_ms = throttle;
    h
}

async fn run(ctx: Ctx, req: FetchRequest) -> FetchResponse {
    let v = ctx.version;
    if req.session_id != 0 {
        // We never hand out fetch sessions, so an incremental fetch can't refer to one.
        return FetchResponse::default().with_error_code(ResponseError::FetchSessionIdNotFound.code());
    }
    let committed = req.isolation_level == 1;
    let max_total = if req.max_bytes > 0 { req.max_bytes as usize } else { usize::MAX };
    let deadline = Instant::now() + Duration::from_millis(req.max_wait_ms.max(0) as u64);
    let min_bytes = req.min_bytes.max(0) as usize;

    let topics: Vec<(TopicName, Uuid, Vec<Slot>)> = req
        .topics
        .into_iter()
        .map(|t| {
            let topic = if v >= 13 { ctx.stream.topic_by_id(&t.topic_id) } else { ctx.stream.topic(&t.topic) };
            let slots = t
                .partitions
                .into_iter()
                .map(|p| Slot {
                    partition: topic.as_ref().and_then(|t| t.partition(p.partition)),
                    topic: topic.clone(),
                    index: p.partition,
                    offset: p.fetch_offset,
                    max_bytes: p.partition_max_bytes,
                    leader_epoch: p.current_leader_epoch,
                })
                .collect();
            (t.topic, t.topic_id, slots)
        })
        .collect();

    loop {
        // Subscribe before reading so an append in between still wakes us.
        let mut watches: Vec<watch::Receiver<i64>> = Vec::new();
        let mut total = 0usize;
        let mut errors = false;
        let mut responses = Vec::with_capacity(topics.len());
        for (name, id, slots) in &topics {
            let mut tr = FetchableTopicResponse::default().with_topic(name.clone()).with_topic_id(*id);
            for slot in slots {
                let mut pd = PartitionData::default()
                    .with_partition_index(slot.index)
                    .with_high_watermark(-1)
                    .with_last_stable_offset(-1)
                    .with_log_start_offset(-1)
                    .with_preferred_read_replica(kafka_protocol::messages::BrokerId(-1))
                    .with_records(Some(Bytes::new()));
                let missing = if v >= 13 { ResponseError::UnknownTopicId } else { ResponseError::UnknownTopicOrPartition };
                match (&slot.topic, &slot.partition) {
                    (None, _) => pd.error_code = missing.code(),
                    (Some(_), None) => pd.error_code = ResponseError::UnknownTopicOrPartition.code(),
                    (Some(_), Some(_)) if slot.leader_epoch > 0 => pd.error_code = ResponseError::UnknownLeaderEpoch.code(),
                    (Some(topic), Some(p)) => {
                        if let Ok(log) = ctx.b.log(topic, p) {
                            watches.push(log.subscribe());
                        }
                        let budget = (slot.max_bytes.max(0) as usize).min(max_total.saturating_sub(total));
                        if budget == 0 && total > 0 {
                            tr.partitions.push(pd);
                            continue;
                        }
                        match read_partition(&ctx.b, &ctx.stream, topic, p, slot.offset, budget.max(1), committed).await {
                            Ok(r) => {
                                total += r.records.len();
                                pd.high_watermark = r.high_watermark;
                                pd.last_stable_offset = r.last_stable;
                                pd.log_start_offset = r.log_start;
                                if committed {
                                    pd.aborted_transactions = Some(
                                        r.aborted
                                            .iter()
                                            .map(|a| AbortedTransaction::default().with_producer_id(ProducerId(a.producer_id)).with_first_offset(a.first_offset))
                                            .collect(),
                                    );
                                }
                                pd.records = Some(r.records);
                            }
                            Err((e, hw, start)) => {
                                errors = true;
                                pd.error_code = e.as_i16();
                                pd.high_watermark = hw;
                                pd.log_start_offset = start;
                            }
                        }
                    }
                }
                if pd.error_code != 0 {
                    errors = true;
                }
                tr.partitions.push(pd);
            }
            responses.push(tr);
        }
        let now = Instant::now();
        if total >= min_bytes || errors || now >= deadline || watches.is_empty() {
            return FetchResponse::default().with_responses(responses);
        }
        // Park until any partition's high watermark moves, or the deadline.
        let changed = futures::future::select_all(watches.iter_mut().map(|w| Box::pin(w.changed())));
        let _ = tokio::time::timeout(deadline - now, changed).await;
    }
}

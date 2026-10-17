//! ListOffsets (earliest, latest, by timestamp, max timestamp, tiered bounds) and
//! OffsetForLeaderEpoch.

use super::fetch::read_raw;
use super::{Ctx, Handled, later};
use crate::broker::{Broker, Partition, Stream, Topic};
use crate::records;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::list_offsets_response::{ListOffsetsPartitionResponse, ListOffsetsResponse, ListOffsetsTopicResponse};
use kafka_protocol::messages::offset_for_leader_epoch_response::{EpochEndOffset, OffsetForLeaderEpochResponse, OffsetForLeaderTopicResult};
use kafka_protocol::messages::{ListOffsetsRequest, OffsetForLeaderEpochRequest, ResponseKind};

pub const LATEST: i64 = -1;
pub const EARLIEST: i64 = -2;
pub const MAX_TIMESTAMP: i64 = -3;
pub const EARLIEST_LOCAL: i64 = -4;
pub const LATEST_TIERED: i64 = -5;
pub const EARLIEST_PENDING_UPLOAD: i64 = -6;

const SCAN_BYTES: usize = 4 << 20;

/// The first record at or after `ts`: `(offset, timestamp)`.
pub async fn offset_for_timestamp(b: &Broker, stream: &Stream, topic: &Topic, p: &Partition, ts: i64) -> anyhow::Result<Option<(i64, i64)>> {
    let log = b.log(topic, p).map_err(|e| anyhow::anyhow!(e.message()))?;
    let Some(start) = log.find_timestamp(ts) else { return Ok(None) };
    scan(b, stream, topic, p, start.offset, |_, t| t >= ts).await
}

/// Read forward from `offset` until `pred(offset, timestamp)` holds.
async fn scan(b: &Broker, stream: &Stream, topic: &Topic, p: &Partition, mut offset: i64, pred: impl Fn(i64, i64) -> bool) -> anyhow::Result<Option<(i64, i64)>> {
    for _ in 0..10_000 {
        let r = read_raw(b, stream, topic, p, offset, SCAN_BYTES, false).await.map_err(|(e, _, _)| anyhow::anyhow!(e.message()))?;
        if r.records.is_empty() {
            return Ok(None);
        }
        for (o, t) in records::timestamps(&r.records)? {
            if o >= offset && pred(o, t) {
                return Ok(Some((o, t)));
            }
        }
        let next = stream_log::batch::headers(&r.records).last().map(|(_, h)| h.next_offset()).unwrap_or(offset + 1);
        if next <= offset {
            return Ok(None);
        }
        offset = next;
    }
    Ok(None)
}

async fn max_timestamp(b: &Broker, stream: &Stream, topic: &Topic, p: &Partition) -> anyhow::Result<Option<(i64, i64)>> {
    let log = b.log(topic, p).map_err(|e| anyhow::anyhow!(e.message()))?;
    let Some((max, read)) = log.find_max_timestamp() else { return Ok(None) };
    scan(b, stream, topic, p, read.offset, |_, t| t >= max).await
}

pub fn list_offsets(ctx: Ctx, req: ListOffsetsRequest) -> Handled {
    later(async move {
        let committed = req.isolation_level == 1;
        let mut topics = Vec::with_capacity(req.topics.len());
        for t in req.topics {
            let topic = ctx.stream.topic(&t.name);
            let mut tr = ListOffsetsTopicResponse::default().with_name(t.name.clone());
            for rp in t.partitions {
                let mut pr = ListOffsetsPartitionResponse::default().with_partition_index(rp.partition_index).with_timestamp(-1).with_offset(-1);
                let Some(topic) = &topic else {
                    pr.error_code = ResponseError::UnknownTopicOrPartition.code();
                    tr.partitions.push(pr);
                    continue;
                };
                let Some(p) = topic.partition(rp.partition_index) else {
                    pr.error_code = ResponseError::UnknownTopicOrPartition.code();
                    tr.partitions.push(pr);
                    continue;
                };
                if rp.current_leader_epoch > 0 {
                    pr.error_code = ResponseError::UnknownLeaderEpoch.code();
                    tr.partitions.push(pr);
                    continue;
                }
                let log = match ctx.b.log(topic, &p) {
                    Ok(l) => l,
                    Err(e) => {
                        pr.error_code = e.as_i16();
                        tr.partitions.push(pr);
                        continue;
                    }
                };
                if ctx.version >= 4 {
                    pr.leader_epoch = 0;
                }
                let found: anyhow::Result<Option<(i64, i64)>> = match rp.timestamp {
                    LATEST => Ok(Some((if committed { log.last_stable() } else { log.high_watermark() }, -1))),
                    EARLIEST => Ok(Some((log.log_start(), -1))),
                    EARLIEST_LOCAL => Ok(Some((log.local_start(), -1))),
                    LATEST_TIERED => Ok(Some((log.uploaded_end().max(log.log_start()) - 1, -1))),
                    EARLIEST_PENDING_UPLOAD => Ok(Some((log.uploaded_end().max(log.log_start()), -1))),
                    MAX_TIMESTAMP => max_timestamp(&ctx.b, &ctx.stream, topic, &p).await,
                    ts if ts >= 0 => offset_for_timestamp(&ctx.b, &ctx.stream, topic, &p, ts).await,
                    _ => {
                        pr.error_code = ResponseError::InvalidRequest.code();
                        Ok(None)
                    }
                };
                match found {
                    Ok(Some((o, ts))) => {
                        pr.offset = o;
                        pr.timestamp = ts;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "offset lookup");
                        pr.error_code = ResponseError::KafkaStorageError.code();
                    }
                }
                tr.partitions.push(pr);
            }
            topics.push(tr);
        }
        ResponseKind::ListOffsets(ListOffsetsResponse::default().with_topics(topics))
    })
}

pub fn leader_epoch(ctx: &Ctx, req: OffsetForLeaderEpochRequest) -> OffsetForLeaderEpochResponse {
    let topics = req
        .topics
        .into_iter()
        .map(|t| {
            let topic = ctx.stream.topic(&t.topic);
            let partitions = t
                .partitions
                .into_iter()
                .map(|rp| {
                    let mut r = EpochEndOffset::default().with_partition(rp.partition).with_leader_epoch(-1).with_end_offset(-1);
                    let p = topic.as_ref().and_then(|t| t.partition(rp.partition).map(|p| (t.clone(), p)));
                    match p {
                        None => r.error_code = ResponseError::UnknownTopicOrPartition.code(),
                        Some(_) if rp.current_leader_epoch > 0 => r.error_code = ResponseError::UnknownLeaderEpoch.code(),
                        Some((topic, p)) => match ctx.b.log(&topic, &p) {
                            // One epoch (0) for the partition's whole life.
                            Ok(log) if rp.leader_epoch <= 0 => {
                                r.leader_epoch = 0;
                                r.end_offset = log.log_end();
                            }
                            Ok(_) => {}
                            Err(e) => r.error_code = e.as_i16(),
                        },
                    }
                    r
                })
                .collect();
            OffsetForLeaderTopicResult::default().with_topic(t.topic).with_partitions(partitions)
        })
        .collect();
    OffsetForLeaderEpochResponse::default().with_topics(topics)
}

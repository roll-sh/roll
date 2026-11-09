//! Log compaction (`cleanup.policy=compact`): keep the latest record per key.
//!
//! Compaction works on object storage, where all data ends up: a pass takes the cleanable prefix
//! of a partition (uploaded, sealed, below the last stable offset and `min.compaction.lag.ms`),
//! builds a map of the last offset of every key, rewrites the chunks keeping only those records,
//! and swaps the new chunks in for the old ones atomically in the catalog. Offsets never change;
//! compaction only leaves gaps. Old objects are deleted after a grace period so reads planned
//! against them can finish (and reads re-plan if one disappears anyway).
//!
//! Per Kafka's rules: records without a key are dropped, tombstones (null values) are kept for
//! `delete.retention.ms` (measured from the tombstone's timestamp), records of aborted
//! transactions are dropped, and transaction markers are kept.

use crate::broker::{Broker, Partition, Stream, Topic};
use crate::config;
use crate::ids::{now, now_ms};
use bytes::{Bytes, BytesMut};
use kafka_protocol::records::{Compression, Record, RecordBatchDecoder, RecordBatchEncoder, RecordEncodeOptions, TimestampType};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use stream_log::batch::{self, Header};
use stream_log::{AbortedTxn, Chunk, CompactionPlan, CompactionPolicy, Log};
use tracing::{debug, warn};

/// Chunks downloaded for the map are kept for the rewrite up to this many bytes; past it the
/// rewrite downloads them again.
const HOLD_BYTES: u64 = 64 << 20;

type KeyHash = [u8; 16];

fn key_hash(key: &[u8]) -> KeyHash {
    Sha256::digest(key)[..16].try_into().unwrap()
}

/// What a pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compacted {
    pub records_in: u64,
    pub records_out: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

fn is_aborted(h: &Header, aborted: &[AbortedTxn]) -> bool {
    h.is_transactional()
        && aborted.iter().any(|a| a.producer_id == h.producer_id && h.base_offset >= a.first_offset && h.base_offset <= a.last_offset)
}

fn decode(one: &Bytes) -> anyhow::Result<Vec<Record>> {
    Ok(RecordBatchDecoder::decode(&mut one.clone())?.records)
}

fn codec(h: &Header) -> Compression {
    match h.compression() {
        1 => Compression::Gzip,
        2 => Compression::Snappy,
        3 => Compression::Lz4,
        4 => Compression::Zstd,
        _ => Compression::None,
    }
}

fn timestamp(h: &Header, r: &Record) -> i64 {
    if r.timestamp_type == TimestampType::LogAppend { h.max_timestamp } else { r.timestamp }
}

/// The last offset of every key in `data` (concatenated batches) from `log_start` on.
fn index(data: &Bytes, log_start: i64, aborted: &[AbortedTxn], map: &mut HashMap<KeyHash, i64>) -> anyhow::Result<()> {
    for (at, h) in batch::headers(data) {
        if h.is_control() || h.last_offset() < log_start || is_aborted(&h, aborted) {
            continue;
        }
        for r in decode(&data.slice(at..at + h.size))? {
            if r.offset >= log_start
                && let Some(k) = &r.key
            {
                map.insert(key_hash(k), r.offset);
            }
        }
    }
    Ok(())
}

/// The batches of `data` that survive compaction: whole batches as they are, partly compacted
/// ones rebuilt with the same producer fields, sequences and compression.
fn rewrite(
    data: &Bytes,
    plan: &CompactionPlan,
    map: &HashMap<KeyHash, i64>,
    tombstone_cutoff: i64,
    stats: &mut Compacted,
) -> anyhow::Result<Vec<(Header, Bytes)>> {
    let mut out = Vec::new();
    for (at, h) in batch::headers(data) {
        let one = data.slice(at..at + h.size);
        stats.bytes_in += h.size as u64;
        if h.last_offset() < plan.log_start {
            continue;
        }
        if h.is_control() {
            stats.bytes_out += h.size as u64;
            out.push((h, one));
            continue;
        }
        let records = decode(&one)?;
        stats.records_in += records.len() as u64;
        if is_aborted(&h, &plan.aborted) {
            continue;
        }
        let total = records.len();
        let kept: Vec<Record> = records
            .into_iter()
            .filter(|r| {
                r.offset >= plan.log_start
                    && r.key.as_ref().is_some_and(|k| map.get(&key_hash(k)) == Some(&r.offset))
                    && (r.value.is_some() || timestamp(&h, r) >= tombstone_cutoff)
            })
            .collect();
        stats.records_out += kept.len() as u64;
        if kept.is_empty() {
            continue;
        }
        if kept.len() == total {
            stats.bytes_out += h.size as u64;
            out.push((h, one));
            continue;
        }
        let first = kept[0].offset;
        let recs: Vec<Record> = kept
            .into_iter()
            .map(|mut r| {
                // The encoder keeps records in one batch while offset - sequence is constant.
                r.sequence = if h.base_sequence >= 0 {
                    h.base_sequence.wrapping_add((r.offset - h.base_offset) as i32)
                } else {
                    (r.offset - first) as i32 - 1
                };
                r.partition_leader_epoch = h.leader_epoch;
                r
            })
            .collect();
        let mut buf = BytesMut::new();
        RecordBatchEncoder::encode(&mut buf, &recs, &RecordEncodeOptions { version: 2, compression: codec(&h) })?;
        if h.attributes & batch::TIMESTAMP_LOG_APPEND != 0 {
            let mut at = 0;
            while at < buf.len() {
                let size = batch::batch_size(&buf[at..]).ok_or_else(|| anyhow::anyhow!("rebuilt batch is truncated"))?;
                batch::set_log_append_time(&mut buf[at..at + size], h.max_timestamp);
                at += size;
            }
        }
        let buf = buf.freeze();
        for (at, nh) in batch::headers(&buf) {
            stats.bytes_out += nh.size as u64;
            out.push((nh, buf.slice(at..at + nh.size)));
        }
    }
    Ok(out)
}

/// The offsets below which a partition's chunks are known to be compacted: the contiguous run of
/// compacted chunks from the start (their keys say so).
pub fn cleaned_to(chunks: &[Chunk], log_start: i64) -> i64 {
    let mut to = log_start;
    for c in chunks.iter().filter(|c| c.last >= log_start) {
        if !config::is_compacted_key(&c.key) {
            break;
        }
        to = c.last + 1;
    }
    to
}

impl Broker {
    /// Run a compaction pass on a partition if one is due. Returns what it did, or `None` if
    /// nothing was due (or the log changed underneath it; the next pass retries).
    pub async fn compact_partition(&self, stream: &Stream, topic: &Topic, p: &Partition, log: &Arc<Log>) -> anyhow::Result<Option<Compacted>> {
        let e = topic.effective();
        if !e.compact || p.compacting.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let result = self.compact_inner(stream, topic, p, log, &e).await;
        p.compacting.store(false, Ordering::Release);
        result
    }

    async fn compact_inner(
        &self,
        stream: &Stream,
        topic: &Topic,
        p: &Partition,
        log: &Arc<Log>,
        e: &crate::topic_config::Effective,
    ) -> anyhow::Result<Option<Compacted>> {
        let at = now_ms();
        let policy = CompactionPolicy {
            min_lag_ms: e.min_compaction_lag_ms,
            max_lag_ms: e.max_compaction_lag_ms,
            dirty_ratio: e.min_cleanable_dirty_ratio,
        };
        let Some(plan) = log.compaction_plan(at, policy) else { return Ok(None) };

        let mut map = HashMap::new();
        let mut held: Vec<Option<Bytes>> = Vec::with_capacity(plan.chunks.len());
        let mut held_bytes = 0;
        for c in &plan.chunks {
            let data = self.chunk_uncached(stream, c).await?;
            index(&data, plan.log_start, &plan.aborted, &mut map)?;
            held_bytes += data.len() as u64;
            held.push((held_bytes <= HOLD_BYTES).then_some(data));
        }

        let tombstone_cutoff = at.saturating_sub(e.delete_retention_ms.max(0));
        let max_chunk = self.config.upload.max_bytes as usize;
        let generation = at;
        let mut stats = Compacted::default();
        let mut written: Vec<Chunk> = Vec::new();
        let mut pending: Vec<(Header, Bytes)> = Vec::new();
        let mut pending_bytes = 0;
        for (i, c) in plan.chunks.iter().enumerate() {
            let data = match held[i].take() {
                Some(d) => d,
                None => self.chunk_uncached(stream, c).await?,
            };
            for (h, b) in rewrite(&data, &plan, &map, tombstone_cutoff, &mut stats)? {
                if pending_bytes > 0 && pending_bytes + b.len() > max_chunk {
                    written.push(self.put_compacted(stream, topic, p, generation, std::mem::take(&mut pending)).await?);
                    pending_bytes = 0;
                }
                pending_bytes += b.len();
                pending.push((h, b));
            }
        }
        if !pending.is_empty() {
            written.push(self.put_compacted(stream, topic, p, generation, pending).await?);
        }

        let discard = |b: &Broker, chunks: Vec<Chunk>| {
            let objects = b.objects.clone();
            async move {
                for c in chunks {
                    let _ = objects.delete(&c.key).await;
                }
            }
        };
        if topic.deleted.load(Ordering::SeqCst) || !log.compacted(&plan, written.clone()) {
            discard(self, written).await;
            return Ok(None);
        }
        let firsts: Vec<i64> = plan.chunks.iter().map(|c| c.first).collect();
        if let Err(err) = self.store.replace_chunks(&topic.id, p.index, &firsts, &written, now()) {
            // The catalog still points at the old chunks, which are intact; the in-memory log
            // serves the new ones until the partition is reopened.
            warn!(topic = %topic.name, partition = p.index, error = %err, "recording compacted chunks");
            return Err(err);
        }
        debug!(
            stream = %stream.id, topic = %topic.name, partition = p.index,
            records_in = stats.records_in, records_out = stats.records_out, bytes_in = stats.bytes_in, bytes_out = stats.bytes_out,
            "compacted"
        );
        let old: Vec<Chunk> = plan.chunks.clone();
        for c in &old {
            self.remote.remove(&c.key);
        }
        let grace = Duration::from_millis(self.config.compaction_grace_ms);
        let objects = self.objects.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            for c in old {
                if let Err(e) = objects.delete(&c.key).await {
                    warn!(key = %c.key, error = %e, "deleting a compacted-away chunk");
                }
            }
        });
        Ok(Some(stats))
    }

    async fn put_compacted(&self, s: &Stream, t: &Topic, p: &Partition, generation: i64, batches: Vec<(Header, Bytes)>) -> anyhow::Result<Chunk> {
        let first = batches[0].0.base_offset;
        let last = batches.last().unwrap().0.last_offset();
        let max_timestamp = batches.iter().map(|(h, _)| h.max_timestamp).max().unwrap_or(-1);
        let mut data = BytesMut::with_capacity(batches.iter().map(|(_, b)| b.len()).sum());
        for (_, b) in &batches {
            data.extend_from_slice(b);
        }
        let key = config::compacted_chunk_key(&s.id, &t.id, p.index, first, last, generation);
        let chunk = Chunk { first, last, max_timestamp, bytes: data.len() as u64, key };
        self.objects.put(&chunk.key, data.freeze()).await?;
        s.usage.puts.fetch_add(1, Ordering::Relaxed);
        Ok(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stream_log::batch::{BatchOptions, NewRecord};

    fn batch_at(offset: i64, recs: &[(Option<&str>, Option<&str>, i64)], opts: BatchOptions) -> Bytes {
        let recs: Vec<NewRecord> = recs
            .iter()
            .map(|(k, v, ts)| NewRecord {
                key: k.map(|k| Bytes::copy_from_slice(k.as_bytes())),
                value: v.map(|v| Bytes::copy_from_slice(v.as_bytes())),
                timestamp: Some(*ts),
                ..Default::default()
            })
            .collect();
        let mut b = batch::build(&recs, opts, 0).to_vec();
        batch::assign_offset(&mut b, offset, 3);
        Bytes::from(b)
    }

    fn plan(aborted: Vec<AbortedTxn>) -> CompactionPlan {
        CompactionPlan { chunks: vec![], log_start: 0, end: 100, dirty_from: 0, aborted }
    }

    fn compact(data: &Bytes, plan: &CompactionPlan, cutoff: i64) -> (Vec<(Header, Bytes)>, Compacted) {
        let mut map = HashMap::new();
        index(data, plan.log_start, &plan.aborted, &mut map).unwrap();
        let mut stats = Compacted::default();
        (rewrite(data, plan, &map, cutoff, &mut stats).unwrap(), stats)
    }

    fn values(out: &[(Header, Bytes)]) -> Vec<(i64, Option<String>, Option<String>)> {
        let mut all = BytesMut::new();
        for (_, b) in out {
            all.extend_from_slice(b);
        }
        crate::records::decode(&all.freeze(), None)
            .unwrap()
            .into_iter()
            .map(|r| (r.offset, r.key.map(|k| String::from_utf8(k.to_vec()).unwrap()), r.value.map(|v| String::from_utf8(v.to_vec()).unwrap())))
            .collect()
    }

    #[test]
    fn keeps_the_latest_value_per_key() {
        let mut data = BytesMut::new();
        data.extend_from_slice(&batch_at(0, &[(Some("a"), Some("1"), 10), (Some("b"), Some("1"), 10), (Some("a"), Some("2"), 10)], BatchOptions::default()));
        data.extend_from_slice(&batch_at(3, &[(Some("c"), Some("1"), 10)], BatchOptions::default()));
        data.extend_from_slice(&batch_at(4, &[(Some("b"), Some("2"), 10), (None, Some("nokey"), 10)], BatchOptions::default()));
        let (out, stats) = compact(&data.freeze(), &plan(vec![]), 0);
        assert_eq!(
            values(&out),
            vec![
                (2, Some("a".into()), Some("2".into())),
                (3, Some("c".into()), Some("1".into())),
                (4, Some("b".into()), Some("2".into())),
            ]
        );
        assert_eq!((stats.records_in, stats.records_out), (6, 3));
        // The untouched batch is copied as is; rebuilt ones keep their leader epoch and are valid.
        assert_eq!(out[1].0.base_offset, 3);
        for (h, b) in &out {
            assert_eq!(batch::validate(b).unwrap(), *h);
            assert_eq!(h.leader_epoch, 3);
        }
    }

    #[test]
    fn tombstones_expire_and_aborted_data_goes() {
        let txn = BatchOptions { producer_id: 7, producer_epoch: 1, base_sequence: 5, attributes: batch::TRANSACTIONAL };
        let idem = BatchOptions { producer_id: 8, producer_epoch: 0, base_sequence: 10, attributes: 0 };
        let mut data = BytesMut::new();
        data.extend_from_slice(&batch_at(0, &[(Some("x"), Some("1"), 10), (Some("old"), None, 10), (Some("new"), None, 1000)], idem));
        data.extend_from_slice(&batch_at(3, &[(Some("x"), Some("aborted"), 10)], txn));
        let mut marker = batch::control_batch(7, 1, false, 0, 0).to_vec();
        batch::assign_offset(&mut marker, 4, 0);
        data.extend_from_slice(&marker);
        let aborted = vec![AbortedTxn { producer_id: 7, first_offset: 3, last_offset: 4 }];
        let (out, _) = compact(&data.freeze(), &plan(aborted), 500);
        assert_eq!(values(&out), vec![(0, Some("x".into()), Some("1".into())), (2, Some("new".into()), None)]);
        // The rebuilt idempotent batch keeps its producer and sequence numbers.
        let h = out[0].0;
        assert_eq!((h.producer_id, h.producer_epoch, h.base_sequence, h.base_offset, h.last_offset()), (8, 0, 10, 0, 2));
        assert!(out[1].0.is_control(), "markers stay");
    }

    #[test]
    fn rebuilt_batches_keep_compression_and_log_append_time() {
        let recs: Vec<NewRecord> = (0..20)
            .map(|i| NewRecord {
                key: Some(Bytes::from(format!("k{}", i % 4))),
                value: Some(Bytes::from("v".repeat(50))),
                timestamp: Some(100),
                ..Default::default()
            })
            .collect();
        let mut b = crate::records::encode(&recs, Compression::Zstd, 0).unwrap().to_vec();
        batch::assign_offset(&mut b, 40, 0);
        batch::set_log_append_time(&mut b, 777);
        let data = Bytes::from(b);
        let p = CompactionPlan { log_start: 40, ..plan(vec![]) };
        let (out, _) = compact(&data, &p, 0);
        assert_eq!(out.len(), 1);
        let h = out[0].0;
        assert_eq!(h.compression(), Compression::Zstd as i16);
        assert_eq!(h.max_timestamp, 777);
        assert!(h.attributes & batch::TIMESTAMP_LOG_APPEND != 0);
        assert_eq!(values(&out).iter().map(|v| v.0).collect::<Vec<_>>(), vec![56, 57, 58, 59]);
    }

    #[test]
    fn cleaned_prefix_from_keys() {
        let id = uuid::Uuid::nil();
        let c = |first, last, compacted: bool| Chunk {
            first,
            last,
            max_timestamp: 0,
            bytes: 1,
            key: if compacted { config::compacted_chunk_key("s", &id, 0, first, last, 1) } else { config::chunk_key("s", &id, 0, first, last) },
        };
        assert_eq!(cleaned_to(&[c(0, 9, true), c(10, 19, true), c(20, 29, false), c(30, 39, true)], 0), 20);
        assert_eq!(cleaned_to(&[c(0, 9, false)], 5), 5);
    }
}

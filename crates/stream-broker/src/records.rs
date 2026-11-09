//! Record-level decoding (any compression) and encoding, for the paths that need individual
//! records: timestamp lookups, the HTTP API and recompression. The Kafka produce/fetch path
//! never decodes records.

use bytes::{Bytes, BytesMut};
use kafka_protocol::protocol::StrBytes;
use kafka_protocol::records::{Compression, Record, RecordBatchDecoder, RecordBatchEncoder, RecordEncodeOptions, TimestampType};
use stream_log::batch::{self, BatchOptions, NewRecord};
use stream_log::AbortedTxn;

#[derive(Clone, Debug)]
pub struct Decoded {
    pub offset: i64,
    pub timestamp: i64,
    pub key: Option<Bytes>,
    pub value: Option<Bytes>,
    pub headers: Vec<(String, Option<Bytes>)>,
}

/// Decode every data record in `buf` (concatenated batches), skipping control batches and,
/// when `aborted` is given, records of aborted transactions.
pub fn decode(buf: &Bytes, aborted: Option<&[AbortedTxn]>) -> anyhow::Result<Vec<Decoded>> {
    let mut out = Vec::new();
    for (at, h) in batch::headers(buf) {
        if h.is_control() {
            continue;
        }
        if let Some(ab) = aborted
            && h.is_transactional()
            && ab.iter().any(|a| a.producer_id == h.producer_id && h.base_offset >= a.first_offset && h.base_offset < a.last_offset)
        {
            continue;
        }
        let mut one = buf.slice(at..at + h.size);
        let set = RecordBatchDecoder::decode(&mut one)?;
        for r in set.records {
            out.push(Decoded {
                offset: r.offset,
                timestamp: if r.timestamp_type == TimestampType::LogAppend { h.max_timestamp } else { r.timestamp },
                key: r.key,
                value: r.value,
                headers: r.headers.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            });
        }
    }
    Ok(out)
}

/// `(offset, timestamp)` of every record (cheaper than [`decode`] when values aren't needed).
pub fn timestamps(buf: &Bytes) -> anyhow::Result<Vec<(i64, i64)>> {
    Ok(decode(buf, None)?.into_iter().map(|r| (r.offset, r.timestamp)).collect())
}

pub fn compression(name: &str) -> Option<Compression> {
    match name {
        "none" | "uncompressed" => Some(Compression::None),
        "gzip" => Some(Compression::Gzip),
        "snappy" => Some(Compression::Snappy),
        "lz4" => Some(Compression::Lz4),
        "zstd" => Some(Compression::Zstd),
        _ => None,
    }
}

/// Build one batch from new records, compressed with `codec`.
pub fn encode(records: &[NewRecord], codec: Compression, now_ms: i64) -> anyhow::Result<Bytes> {
    if codec == Compression::None {
        return Ok(batch::build(records, BatchOptions::default(), now_ms));
    }
    let recs: Vec<Record> = records
        .iter()
        .enumerate()
        .map(|(i, r)| Record {
            transactional: false,
            control: false,
            delete_horizon: false,
            partition_leader_epoch: 0,
            producer_id: -1,
            producer_epoch: -1,
            timestamp_type: TimestampType::Creation,
            offset: i as i64,
            // Constant offset-sequence keeps everything in one batch; base sequence stays -1.
            sequence: i as i32 - 1,
            timestamp: r.timestamp.unwrap_or(now_ms),
            key: r.key.clone(),
            value: r.value.clone(),
            headers: r.headers.iter().map(|(k, v)| (StrBytes::from_string(k.clone()), v.clone())).collect(),
        })
        .collect();
    let mut buf = BytesMut::new();
    RecordBatchEncoder::encode(&mut buf, &recs, &RecordEncodeOptions { version: 2, compression: codec })?;
    Ok(buf.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_codecs() {
        let recs: Vec<NewRecord> = (0..50)
            .map(|i| NewRecord {
                key: Some(Bytes::from(format!("k{i}"))),
                value: Some(Bytes::from("x".repeat(100))),
                headers: vec![("h".into(), Some(Bytes::from_static(b"1")))],
                timestamp: Some(1000 + i),
            })
            .collect();
        for codec in [Compression::None, Compression::Gzip, Compression::Snappy, Compression::Lz4, Compression::Zstd] {
            let b = encode(&recs, codec, 0).unwrap();
            let h = batch::validate(&b).unwrap();
            assert_eq!(h.compression(), codec as i16, "{codec:?}");
            let out = decode(&b, None).unwrap();
            assert_eq!(out.len(), 50);
            assert_eq!(out[7].key.as_deref(), Some(&b"k7"[..]));
            assert_eq!(out[7].timestamp, 1007);
            assert_eq!(out[7].headers[0].0, "h");
            if codec != Compression::None {
                assert!(b.len() < 50 * 100, "{codec:?} should compress");
            }
        }
    }

    #[test]
    fn skips_control_and_aborted() {
        let mut data = BytesMut::new();
        let rec = |v: &str| NewRecord { value: Some(Bytes::copy_from_slice(v.as_bytes())), ..Default::default() };
        let mut t = batch::build(&[rec("txn")], BatchOptions { producer_id: 9, producer_epoch: 0, base_sequence: 0, attributes: batch::TRANSACTIONAL }, 0).to_vec();
        batch::assign_offset(&mut t, 0, 0);
        data.extend_from_slice(&t);
        let mut m = batch::control_batch(9, 0, false, 0, 0).to_vec();
        batch::assign_offset(&mut m, 1, 0);
        data.extend_from_slice(&m);
        let mut p = batch::build(&[rec("plain")], BatchOptions::default(), 0).to_vec();
        batch::assign_offset(&mut p, 2, 0);
        data.extend_from_slice(&p);
        let data = data.freeze();
        assert_eq!(decode(&data, None).unwrap().len(), 2);
        let aborted = [AbortedTxn { producer_id: 9, first_offset: 0, last_offset: 1 }];
        let out = decode(&data, Some(&aborted)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value.as_deref(), Some(&b"plain"[..]));
    }
}

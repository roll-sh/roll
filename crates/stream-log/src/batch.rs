//! Kafka record batches (magic v2), handled zero-copy: the log stores exactly the bytes producers
//! send, with only the base offset and leader epoch patched in, so fetches return slices of it.
//!
//! ```text
//! 0  baseOffset            i64      ┐ not covered by the CRC
//! 8  batchLength           i32      │ (bytes after this field)
//! 12 partitionLeaderEpoch  i32      ┘
//! 16 magic                 i8  (2)
//! 17 crc                   u32  CRC-32C of everything from `attributes` to the end
//! 21 attributes            i16  0-2 compression, 3 timestamp type, 4 transactional, 5 control
//! 23 lastOffsetDelta       i32
//! 27 baseTimestamp         i64
//! 35 maxTimestamp          i64
//! 43 producerId            i64
//! 51 producerEpoch         i16
//! 53 baseSequence          i32
//! 57 recordsCount          i32
//! 61 records…
//! ```

use bytes::{BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const HEADER_LEN: usize = 61;
/// Offset + length prefix: what must be read to learn a batch's size.
pub const LOG_OVERHEAD: usize = 12;
pub const MAGIC: i8 = 2;

const BASE_OFFSET: usize = 0;
const BATCH_LENGTH: usize = 8;
const LEADER_EPOCH: usize = 12;
const MAGIC_AT: usize = 16;
const CRC: usize = 17;
const ATTRIBUTES: usize = 21;
const LAST_OFFSET_DELTA: usize = 23;
const BASE_TIMESTAMP: usize = 27;
const MAX_TIMESTAMP: usize = 35;
const PRODUCER_ID: usize = 43;
const PRODUCER_EPOCH: usize = 51;
const BASE_SEQUENCE: usize = 53;
const RECORDS_COUNT: usize = 57;

pub const COMPRESSION_MASK: i16 = 0x07;
pub const TIMESTAMP_LOG_APPEND: i16 = 0x08;
pub const TRANSACTIONAL: i16 = 0x10;
pub const CONTROL: i16 = 0x20;

pub const NO_PRODUCER_ID: i64 = -1;
pub const NO_SEQUENCE: i32 = -1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BatchError {
    #[error("record batch is truncated")]
    Truncated,
    #[error("unsupported record batch magic {0} (only v2 is supported)")]
    Magic(i8),
    #[error("record batch CRC mismatch")]
    Crc,
    #[error("invalid record batch: {0}")]
    Invalid(&'static str),
}

/// Decoded batch header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub base_offset: i64,
    /// Total size of the batch in bytes, including the offset and length prefix.
    pub size: usize,
    pub leader_epoch: i32,
    pub attributes: i16,
    pub last_offset_delta: i32,
    pub base_timestamp: i64,
    pub max_timestamp: i64,
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub base_sequence: i32,
    pub records: i32,
}

impl Header {
    pub fn last_offset(&self) -> i64 {
        self.base_offset + self.last_offset_delta as i64
    }
    pub fn next_offset(&self) -> i64 {
        self.last_offset() + 1
    }
    pub fn compression(&self) -> i16 {
        self.attributes & COMPRESSION_MASK
    }
    pub fn is_transactional(&self) -> bool {
        self.attributes & TRANSACTIONAL != 0
    }
    pub fn is_control(&self) -> bool {
        self.attributes & CONTROL != 0
    }
    pub fn has_producer(&self) -> bool {
        self.producer_id >= 0
    }
    /// Sequence number of the last record (sequences wrap at `i32::MAX`).
    pub fn last_sequence(&self) -> i32 {
        if self.base_sequence == NO_SEQUENCE {
            return NO_SEQUENCE;
        }
        let last = self.base_sequence as i64 + self.last_offset_delta as i64;
        if last > i32::MAX as i64 { (last - i32::MAX as i64 - 1) as i32 } else { last as i32 }
    }
}

fn i16_at(b: &[u8], at: usize) -> i16 {
    i16::from_be_bytes([b[at], b[at + 1]])
}
fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn i64_at(b: &[u8], at: usize) -> i64 {
    i64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The total size of the batch starting at `buf`, if its length prefix is present.
pub fn batch_size(buf: &[u8]) -> Option<usize> {
    if buf.len() < LOG_OVERHEAD {
        return None;
    }
    let len = i32_at(buf, BATCH_LENGTH);
    (len >= 0).then(|| LOG_OVERHEAD + len as usize)
}

/// Parse the header of the batch at the start of `buf` (which may hold more batches after it).
/// Doesn't check the CRC.
pub fn parse_header(buf: &[u8]) -> Result<Header, BatchError> {
    let h = parse_header_prefix(buf)?;
    if buf.len() < h.size {
        return Err(BatchError::Truncated);
    }
    Ok(h)
}

/// [`parse_header`] from just the first [`HEADER_LEN`] bytes: the records needn't be present.
pub fn parse_header_prefix(buf: &[u8]) -> Result<Header, BatchError> {
    if buf.len() < HEADER_LEN {
        return Err(BatchError::Truncated);
    }
    let magic = buf[MAGIC_AT] as i8;
    if magic != MAGIC {
        return Err(BatchError::Magic(magic));
    }
    let len = i32_at(buf, BATCH_LENGTH);
    if len < (HEADER_LEN - LOG_OVERHEAD) as i32 {
        return Err(BatchError::Invalid("batch length is smaller than the header"));
    }
    let size = LOG_OVERHEAD + len as usize;
    let h = Header {
        base_offset: i64_at(buf, BASE_OFFSET),
        size,
        leader_epoch: i32_at(buf, LEADER_EPOCH),
        attributes: i16_at(buf, ATTRIBUTES),
        last_offset_delta: i32_at(buf, LAST_OFFSET_DELTA),
        base_timestamp: i64_at(buf, BASE_TIMESTAMP),
        max_timestamp: i64_at(buf, MAX_TIMESTAMP),
        producer_id: i64_at(buf, PRODUCER_ID),
        producer_epoch: i16_at(buf, PRODUCER_EPOCH),
        base_sequence: i32_at(buf, BASE_SEQUENCE),
        records: i32_at(buf, RECORDS_COUNT),
    };
    if h.last_offset_delta < 0 || h.records < 0 {
        return Err(BatchError::Invalid("negative record count"));
    }
    Ok(h)
}

/// CRC-32C over `attributes..end`, as stored in the header.
pub fn compute_crc(batch: &[u8]) -> u32 {
    crc32c::crc32c(&batch[ATTRIBUTES..])
}

/// Parse and fully validate a batch as received from a producer.
pub fn validate(batch: &[u8]) -> Result<Header, BatchError> {
    let h = parse_header(batch)?;
    if h.size != batch.len() {
        return Err(BatchError::Invalid("trailing bytes after batch"));
    }
    let crc = u32::from_be_bytes(batch[CRC..CRC + 4].try_into().unwrap());
    if crc != compute_crc(batch) {
        return Err(BatchError::Crc);
    }
    // Uncompressed batches store records inline, so their count must be the offset span. Compressed
    // or compacted batches may have fewer records than the span, never more.
    if h.records > h.last_offset_delta + 1 || (h.records == 0 && !h.is_control()) {
        return Err(BatchError::Invalid("record count doesn't match the offset delta"));
    }
    if h.is_control() && !h.is_transactional() {
        return Err(BatchError::Invalid("control batches must be transactional"));
    }
    if h.has_producer() && h.base_sequence < 0 && !h.is_control() {
        return Err(BatchError::Invalid("idempotent batch without a sequence number"));
    }
    Ok(h)
}

/// Split concatenated batches (a produce request's records) into individual batches.
pub fn split(mut records: Bytes) -> Result<Vec<Bytes>, BatchError> {
    let mut out = Vec::with_capacity(1);
    while !records.is_empty() {
        let size = batch_size(&records).ok_or(BatchError::Truncated)?;
        if size > records.len() {
            return Err(BatchError::Truncated);
        }
        out.push(records.split_to(size));
    }
    Ok(out)
}

/// Set the base offset and leader epoch of a batch (fields outside the CRC).
pub fn assign_offset(batch: &mut [u8], base_offset: i64, leader_epoch: i32) {
    batch[BASE_OFFSET..BASE_OFFSET + 8].copy_from_slice(&base_offset.to_be_bytes());
    batch[LEADER_EPOCH..LEADER_EPOCH + 4].copy_from_slice(&leader_epoch.to_be_bytes());
}

/// Rewrite a batch to use broker time (`message.timestamp.type=LogAppendTime`). Recomputes the CRC.
pub fn set_log_append_time(batch: &mut [u8], now_ms: i64) {
    let attrs = i16_at(batch, ATTRIBUTES) | TIMESTAMP_LOG_APPEND;
    batch[ATTRIBUTES..ATTRIBUTES + 2].copy_from_slice(&attrs.to_be_bytes());
    batch[MAX_TIMESTAMP..MAX_TIMESTAMP + 8].copy_from_slice(&now_ms.to_be_bytes());
    let crc = compute_crc(batch);
    batch[CRC..CRC + 4].copy_from_slice(&crc.to_be_bytes());
}

/// Iterate batch headers in a buffer of concatenated batches, stopping at the first incomplete one.
pub fn headers(buf: &[u8]) -> impl Iterator<Item = (usize, Header)> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let h = parse_header(&buf[pos..]).ok()?;
        let at = pos;
        pos += h.size;
        Some((at, h))
    })
}

/// Slice the batches of `buf` that a fetch at `offset` should return: starting with the batch that
/// contains `offset`, ending before `upper` (exclusive), at most `max_bytes` unless the first batch
/// alone is larger (it's always returned whole so consumers can make progress).
pub fn slice_for_fetch(buf: &Bytes, offset: i64, upper: i64, max_bytes: usize) -> Bytes {
    let (mut start, mut end) = (None, 0);
    for (at, h) in headers(buf) {
        if h.base_offset >= upper {
            break;
        }
        if h.last_offset() < offset {
            continue;
        }
        match start {
            None => {
                start = Some(at);
                end = at + h.size;
            }
            Some(s) => {
                if at + h.size - s > max_bytes {
                    break;
                }
                end = at + h.size;
            }
        }
    }
    match start {
        Some(s) => buf.slice(s..end),
        None => Bytes::new(),
    }
}

/// Summary of a run of batches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub first: i64,
    pub last: i64,
    pub max_timestamp: i64,
    pub batches: usize,
}

pub fn summarize(buf: &[u8]) -> Option<Span> {
    let mut span: Option<Span> = None;
    for (_, h) in headers(buf) {
        let s = span.get_or_insert(Span { first: h.base_offset, last: h.last_offset(), max_timestamp: -1, batches: 0 });
        s.last = h.last_offset();
        s.max_timestamp = s.max_timestamp.max(h.max_timestamp);
        s.batches += 1;
    }
    span
}

// ── records ──

/// Varint (zigzag) encoding used inside records.
pub fn put_varint(buf: &mut impl BufMut, v: i64) {
    let mut z = ((v << 1) ^ (v >> 63)) as u64;
    while z >= 0x80 {
        buf.put_u8((z as u8) | 0x80);
        z >>= 7;
    }
    buf.put_u8(z as u8);
}

pub fn get_varint(buf: &[u8], pos: &mut usize) -> Option<i64> {
    let mut z = 0u64;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        z |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    Some(((z >> 1) as i64) ^ -((z & 1) as i64))
}

/// A record to build an uncompressed batch from.
#[derive(Clone, Debug, Default)]
pub struct NewRecord {
    pub key: Option<Bytes>,
    pub value: Option<Bytes>,
    pub headers: Vec<(String, Option<Bytes>)>,
    /// Milliseconds; `None` for "now".
    pub timestamp: Option<i64>,
}

/// Options for building a batch.
#[derive(Clone, Copy, Debug)]
pub struct BatchOptions {
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub base_sequence: i32,
    pub attributes: i16,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self { producer_id: NO_PRODUCER_ID, producer_epoch: -1, base_sequence: NO_SEQUENCE, attributes: 0 }
    }
}

fn put_bytes(buf: &mut BytesMut, b: Option<&[u8]>) {
    match b {
        Some(b) => {
            put_varint(buf, b.len() as i64);
            buf.put_slice(b);
        }
        None => put_varint(buf, -1),
    }
}

/// Encode an uncompressed v2 batch at base offset 0 (the log assigns real offsets).
pub fn build(records: &[NewRecord], opts: BatchOptions, now_ms: i64) -> Bytes {
    let base_ts = records.iter().map(|r| r.timestamp.unwrap_or(now_ms)).min().unwrap_or(now_ms);
    let mut max_ts = -1;
    let mut body = BytesMut::new();
    for (i, r) in records.iter().enumerate() {
        let ts = r.timestamp.unwrap_or(now_ms);
        max_ts = max_ts.max(ts);
        let mut rec = BytesMut::new();
        rec.put_i8(0);
        put_varint(&mut rec, ts - base_ts);
        put_varint(&mut rec, i as i64);
        put_bytes(&mut rec, r.key.as_deref());
        put_bytes(&mut rec, r.value.as_deref());
        put_varint(&mut rec, r.headers.len() as i64);
        for (k, v) in &r.headers {
            put_bytes(&mut rec, Some(k.as_bytes()));
            put_bytes(&mut rec, v.as_deref());
        }
        put_varint(&mut body, rec.len() as i64);
        body.put_slice(&rec);
    }
    let mut b = BytesMut::with_capacity(HEADER_LEN + body.len());
    b.put_i64(0);
    b.put_i32((HEADER_LEN - LOG_OVERHEAD + body.len()) as i32);
    b.put_i32(0);
    b.put_i8(MAGIC);
    b.put_u32(0); // CRC, filled in below
    b.put_i16(opts.attributes);
    b.put_i32(records.len().saturating_sub(1) as i32);
    b.put_i64(base_ts);
    b.put_i64(max_ts);
    b.put_i64(opts.producer_id);
    b.put_i16(opts.producer_epoch);
    b.put_i32(opts.base_sequence);
    b.put_i32(records.len() as i32);
    b.put_slice(&body);
    let crc = compute_crc(&b);
    b[CRC..CRC + 4].copy_from_slice(&crc.to_be_bytes());
    b.freeze()
}

/// A transaction marker (control batch) ending a producer's transaction on a partition.
pub fn control_batch(producer_id: i64, producer_epoch: i16, commit: bool, coordinator_epoch: i32, now_ms: i64) -> Bytes {
    let mut key = BytesMut::new();
    key.put_i16(0);
    key.put_i16(if commit { 1 } else { 0 });
    let mut value = BytesMut::new();
    value.put_i16(0);
    value.put_i32(coordinator_epoch);
    build(
        &[NewRecord { key: Some(key.freeze()), value: Some(value.freeze()), headers: vec![], timestamp: Some(now_ms) }],
        BatchOptions { producer_id, producer_epoch, base_sequence: NO_SEQUENCE, attributes: TRANSACTIONAL | CONTROL },
        now_ms,
    )
}

/// A decoded record (for uncompressed batches; compressed ones are decoded by the broker).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub offset: i64,
    pub timestamp: i64,
    pub key: Option<Bytes>,
    pub value: Option<Bytes>,
    pub headers: Vec<(Bytes, Option<Bytes>)>,
}

/// Decode the records of an uncompressed record section (`records` bytes after the header).
pub fn decode_records(body: &Bytes, base_offset: i64, base_timestamp: i64, count: i32) -> Option<Vec<Record>> {
    let mut out = Vec::with_capacity(count.max(0) as usize);
    let mut pos = 0usize;
    let take = |pos: &mut usize| -> Option<Option<Bytes>> {
        let len = get_varint(body, pos)?;
        if len < 0 {
            return Some(None);
        }
        let end = pos.checked_add(len as usize)?;
        if end > body.len() {
            return None;
        }
        let b = body.slice(*pos..end);
        *pos = end;
        Some(Some(b))
    };
    for _ in 0..count {
        let len = get_varint(body, &mut pos)?;
        let end = pos.checked_add(len as usize)?;
        if end > body.len() {
            return None;
        }
        pos += 1; // attributes
        let ts_delta = get_varint(body, &mut pos)?;
        let off_delta = get_varint(body, &mut pos)?;
        let key = take(&mut pos)?;
        let value = take(&mut pos)?;
        let n = get_varint(body, &mut pos)?;
        let mut headers = Vec::with_capacity(n.max(0) as usize);
        for _ in 0..n {
            let k = take(&mut pos)?.unwrap_or_default();
            headers.push((k, take(&mut pos)?));
        }
        pos = end;
        out.push(Record { offset: base_offset + off_delta, timestamp: base_timestamp + ts_delta, key, value, headers });
    }
    Some(out)
}

/// Records of an uncompressed batch (`None` if compressed or malformed).
pub fn records_of(batch: &Bytes) -> Option<Vec<Record>> {
    let h = parse_header(batch).ok()?;
    if h.compression() != 0 {
        return None;
    }
    decode_records(&batch.slice(HEADER_LEN..h.size), h.base_offset, h.base_timestamp, h.records)
}

/// Whether a control batch commits (`Some(true)`) or aborts (`Some(false)`) its transaction.
pub fn control_commits(batch: &Bytes) -> Option<bool> {
    let recs = records_of(batch)?;
    let key = recs.first()?.key.as_ref()?;
    (key.len() >= 4).then(|| i16::from_be_bytes([key[2], key[3]]) == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(v: &str) -> NewRecord {
        NewRecord { key: None, value: Some(Bytes::copy_from_slice(v.as_bytes())), headers: vec![], timestamp: Some(1000) }
    }

    #[test]
    fn build_validate_and_decode() {
        let mut r = rec("hello");
        r.key = Some(Bytes::from_static(b"k"));
        r.headers = vec![("h".into(), Some(Bytes::from_static(b"v"))), ("n".into(), None)];
        let b = build(&[r, rec("world")], BatchOptions::default(), 5000);
        let h = validate(&b).unwrap();
        assert_eq!((h.records, h.last_offset_delta, h.base_timestamp, h.max_timestamp), (2, 1, 1000, 1000));
        let recs = records_of(&b).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].key.as_deref(), Some(&b"k"[..]));
        assert_eq!(recs[0].value.as_deref(), Some(&b"hello"[..]));
        assert_eq!(recs[0].headers[0], (Bytes::from_static(b"h"), Some(Bytes::from_static(b"v"))));
        assert_eq!(recs[0].headers[1].1, None);
        assert_eq!(recs[1].offset, 1);
    }

    #[test]
    fn detects_corruption() {
        let b = build(&[rec("x")], BatchOptions::default(), 0);
        let mut bad = b.to_vec();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(validate(&bad), Err(BatchError::Crc));
        assert_eq!(validate(&b[..b.len() - 1]), Err(BatchError::Truncated));
        let mut magic = b.to_vec();
        magic[16] = 1;
        assert_eq!(validate(&magic), Err(BatchError::Magic(1)));
        let mut extra = b.to_vec();
        extra.push(0);
        assert!(matches!(validate(&extra), Err(BatchError::Invalid(_))));
    }

    #[test]
    fn offsets_patch_outside_crc() {
        let b = build(&[rec("a"), rec("b"), rec("c")], BatchOptions::default(), 0);
        let mut m = b.to_vec();
        assign_offset(&mut m, 42, 7);
        let h = validate(&m).unwrap();
        assert_eq!((h.base_offset, h.last_offset(), h.leader_epoch), (42, 44, 7));
    }

    #[test]
    fn log_append_time_rewrites_crc() {
        let b = build(&[rec("a")], BatchOptions::default(), 0);
        let mut m = b.to_vec();
        set_log_append_time(&mut m, 99_999);
        let h = validate(&m).unwrap();
        assert_eq!(h.max_timestamp, 99_999);
        assert!(h.attributes & TIMESTAMP_LOG_APPEND != 0);
    }

    #[test]
    fn split_and_slice() {
        let mut all = BytesMut::new();
        for (i, base) in [0i64, 2, 4].iter().enumerate() {
            let mut m = build(&[rec("a"), rec("b")], BatchOptions::default(), 0).to_vec();
            assign_offset(&mut m, *base, 0);
            all.put_slice(&m);
            let _ = i;
        }
        let all = all.freeze();
        assert_eq!(split(all.clone()).unwrap().len(), 3);
        assert!(split(all.slice(..all.len() - 3)).is_err());
        let one = split(all.clone()).unwrap()[0].len();
        // Offset 3 lives in the second batch; the first is skipped.
        let s = slice_for_fetch(&all, 3, i64::MAX, usize::MAX);
        assert_eq!(parse_header(&s).unwrap().base_offset, 2);
        assert_eq!(s.len(), one * 2);
        // max_bytes smaller than a batch still returns the first one whole.
        assert_eq!(slice_for_fetch(&all, 0, i64::MAX, 1).len(), one);
        // Upper bound excludes batches starting at or after it.
        assert_eq!(slice_for_fetch(&all, 0, 4, usize::MAX).len(), one * 2);
        assert!(slice_for_fetch(&all, 6, i64::MAX, usize::MAX).is_empty());
        assert_eq!(summarize(&all), Some(Span { first: 0, last: 5, max_timestamp: 1000, batches: 3 }));
    }

    #[test]
    fn control_markers() {
        let c = control_batch(7, 3, true, 0, 10);
        let h = validate(&c).unwrap();
        assert!(h.is_control() && h.is_transactional());
        assert_eq!((h.producer_id, h.producer_epoch), (7, 3));
        assert_eq!(control_commits(&c), Some(true));
        assert_eq!(control_commits(&control_batch(7, 3, false, 0, 10)), Some(false));
    }

    #[test]
    fn varints_roundtrip() {
        for v in [0i64, 1, -1, 63, -64, 64, 300, -300, i32::MAX as i64, i64::MIN, i64::MAX] {
            let mut b = BytesMut::new();
            put_varint(&mut b, v);
            let mut pos = 0;
            assert_eq!(get_varint(&b, &mut pos), Some(v));
            assert_eq!(pos, b.len());
        }
    }

    #[test]
    fn sequence_wraps() {
        let mut m = build(&[rec("a"), rec("b"), rec("c")], BatchOptions { producer_id: 1, producer_epoch: 0, base_sequence: i32::MAX - 1, attributes: 0 }, 0).to_vec();
        assign_offset(&mut m, 0, 0);
        let h = validate(&m).unwrap();
        assert_eq!(h.last_sequence(), 0);
    }
}

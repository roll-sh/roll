//! The connectionless data plane: produce and consume over plain HTTP requests.
//!
//! Produce builds one record batch per partition per request and appends it exactly like a
//! Kafka producer's batch. Reads decode batches from the same log the Kafka listener serves,
//! with long polling (`wait_ms`), stateless multi-partition cursors, server-managed groups
//! (see [`super::leases`]) and Server-Sent Events.

use super::{Api, ApiError, ApiResult, S, data_stream, leases};
use crate::broker::{Broker, Stream, Topic};
use crate::error::KafkaError;
use crate::handlers::fetch::read_partition;
use crate::handlers::produce;
use crate::ids::now_ms;
use crate::{partitioner, records};
use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use stream_log::batch::{self, NewRecord};
use tokio::sync::{mpsc, watch};

pub fn routes() -> Router<Arc<Api>> {
    Router::new()
        .route("/v1/streams/{s}/topics/{t}/records", post(produce).get(read_topic))
        .route("/v1/streams/{s}/topics/{t}/partitions/{p}/records", get(read_one))
        .route("/v1/streams/{s}/topics/{t}/tail", get(tail))
        .route("/v1/streams/{s}/groups/{g}/commit", post(commit))
        .route("/v1/streams/{s}/groups/{g}/consumers/{c}", delete(leave))
        .layer(DefaultBodyLimit::max(100 << 20))
}

pub const MAX_WAIT_MS: u64 = 60_000;
const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 10_000;
const DEFAULT_MAX_BYTES: usize = 4 << 20;
const MAX_BYTES: usize = 64 << 20;

// ── produce ──

/// One record to append, with an optional explicit partition.
pub struct Incoming {
    pub partition: Option<i32>,
    pub record: NewRecord,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Acks {
    None,
    Leader,
    All,
}

impl Acks {
    pub fn parse(s: Option<&str>) -> ApiResult<Self> {
        match s.unwrap_or("all") {
            "0" => Ok(Acks::None),
            "1" => Ok(Acks::Leader),
            "all" | "-1" => Ok(Acks::All),
            other => Err(ApiError::bad(format!("acks must be 0, 1 or all, not {other}"))),
        }
    }
}

/// Where a record went.
pub struct Produced {
    pub partition: i32,
    pub offset: i64,
    pub timestamp: i64,
}

/// Append records, one batch per partition (split at `max.message.bytes`), compressed as the
/// topic's `compression.type` says. Results are in input order.
pub async fn produce_records(api: &Api, stream: &Arc<Stream>, topic: &Arc<Topic>, recs: Vec<Incoming>, acks: Acks, timeout: Duration) -> Vec<ApiResult<Produced>> {
    let b = &api.b;
    let n = topic.partition_count();
    let sticky = (api.rr.fetch_add(1, Ordering::Relaxed) % n.max(1) as usize) as i32;
    let mut out: Vec<Option<ApiResult<Produced>>> = (0..recs.len()).map(|_| None).collect();
    let mut by_partition: BTreeMap<i32, Vec<(usize, NewRecord)>> = BTreeMap::new();
    for (i, r) in recs.into_iter().enumerate() {
        let p = match (r.partition, &r.record.key) {
            (Some(p), _) => p,
            (None, Some(k)) => partitioner::partition_for_key(k, n),
            (None, None) => sticky,
        };
        if !(0..n).contains(&p) {
            out[i] = Some(Err(ApiError::new(StatusCode::BAD_REQUEST, "unknown_topic_or_partition", format!("partition {p} does not exist (the topic has {n})"))));
            continue;
        }
        by_partition.entry(p).or_default().push((i, r.record));
    }

    let e = topic.effective();
    let codec = records::compression(&e.compression).unwrap_or(kafka_protocol::records::Compression::None);
    let budget = e.max_message_bytes.saturating_sub(batch::HEADER_LEN + 64).max(1024);
    let now = now_ms();
    let mut waits = vec![];
    for (p, items) in by_partition {
        let partition = topic.partition(p).expect("checked");
        let result = (|| {
            let mut batches = vec![];
            let mut start = 0;
            while start < items.len() {
                let mut size = 0;
                let mut end = start;
                while end < items.len() {
                    let r = &items[end].1;
                    let est = 24 + r.key.as_ref().map_or(0, |k| k.len()) + r.value.as_ref().map_or(0, |v| v.len()) + r.headers.iter().map(|(k, v)| k.len() + v.as_ref().map_or(0, |v| v.len()) + 8).sum::<usize>();
                    if end > start && size + est > budget {
                        break;
                    }
                    size += est;
                    end += 1;
                }
                let chunk: Vec<NewRecord> = items[start..end].iter().map(|(_, r)| r.clone()).collect();
                batches.push(records::encode(&chunk, codec, now).map_err(KafkaError::internal)?);
                start = end;
            }
            produce::append(b, stream, topic, &partition, &batches)
        })();
        match result {
            Ok((appended, durable)) => {
                for (j, (i, r)) in items.iter().enumerate() {
                    let ts = if appended.log_append_time >= 0 { appended.log_append_time } else { r.timestamp.unwrap_or(now) };
                    out[*i] = Some(Ok(Produced { partition: p, offset: appended.base_offset + j as i64, timestamp: ts }));
                }
                if acks == Acks::All {
                    waits.push((items.iter().map(|(i, _)| *i).collect::<Vec<_>>(), durable));
                }
            }
            Err(e) => {
                for (i, _) in &items {
                    out[*i] = Some(Err(e.clone().into()));
                }
            }
        }
    }
    let results = futures::future::join_all(waits.into_iter().map(|(idx, d)| async move { (idx, d.wait(timeout).await) })).await;
    for (idx, ok) in results {
        if !ok {
            for i in idx {
                out[i] = Some(Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "request_timed_out", "timed out waiting for the write to become durable")));
            }
        }
    }
    out.into_iter().map(|r| r.expect("every record has a result")).collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProduceQuery {
    partition: Option<i32>,
    key: Option<String>,
    acks: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum InTimestamp {
    Ms(i64),
    Text(String),
}

#[derive(Deserialize)]
struct InHeader {
    #[serde(alias = "name")]
    key: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    value_base64: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum InHeaders {
    Map(BTreeMap<String, Option<String>>),
    List(Vec<InHeader>),
}

/// A record in JSON. The body may also be `{"records": [...]}` (the same fields per record).
#[derive(Deserialize, Default)]
#[serde(default)]
struct InRecord<'a> {
    #[serde(borrow)]
    records: Option<Vec<InRecord<'a>>>,
    #[serde(borrow)]
    key: Option<&'a RawValue>,
    key_base64: Option<String>,
    #[serde(borrow)]
    value: Option<&'a RawValue>,
    value_base64: Option<String>,
    headers: Option<InHeaders>,
    partition: Option<i32>,
    timestamp: Option<InTimestamp>,
}

/// A JSON string is stored as its text; anything else as its JSON encoding, verbatim.
fn raw_bytes(raw: Option<&RawValue>, b64: Option<String>, what: &str) -> ApiResult<Option<Bytes>> {
    if let Some(s) = b64 {
        return B64.decode(s.trim()).map(|v| Some(Bytes::from(v))).map_err(|_| ApiError::bad(format!("{what}_base64 is not valid base64")));
    }
    let Some(raw) = raw else { return Ok(None) };
    let text = raw.get();
    if text.starts_with('"') {
        let s: String = serde_json::from_str(text).map_err(|e| ApiError::bad(e.to_string()))?;
        Ok(Some(Bytes::from(s)))
    } else {
        Ok(Some(Bytes::copy_from_slice(text.as_bytes())))
    }
}

fn timestamp_ms(t: Option<InTimestamp>) -> ApiResult<Option<i64>> {
    match t {
        None => Ok(None),
        Some(InTimestamp::Ms(ms)) => Ok(Some(ms)),
        Some(InTimestamp::Text(s)) => super::time::parse_ms(&s).map(Some).ok_or_else(|| ApiError::bad(format!("bad timestamp {s}"))),
    }
}

fn incoming(r: InRecord<'_>) -> ApiResult<Incoming> {
    let headers = match r.headers {
        None => vec![],
        Some(InHeaders::Map(m)) => m.into_iter().map(|(k, v)| (k, v.map(Bytes::from))).collect(),
        Some(InHeaders::List(l)) => l
            .into_iter()
            .map(|h| {
                let v = match h.value_base64 {
                    Some(b) => Some(Bytes::from(B64.decode(b.trim()).map_err(|_| ApiError::bad("header value_base64 is not valid base64"))?)),
                    None => h.value.map(Bytes::from),
                };
                Ok((h.key, v))
            })
            .collect::<ApiResult<_>>()?,
    };
    Ok(Incoming {
        partition: r.partition,
        record: NewRecord {
            key: raw_bytes(r.key, r.key_base64, "key")?,
            value: raw_bytes(r.value, r.value_base64, "value")?,
            headers,
            timestamp: timestamp_ms(r.timestamp)?,
        },
    })
}

fn looks_like_json(body: &[u8]) -> bool {
    matches!(body.iter().find(|c| !c.is_ascii_whitespace()), Some(b'{' | b'['))
}

/// Records from a request body:
/// - JSON: one record object, an array of them, or `{"records": [...]}`.
/// - NDJSON (`application/x-ndjson`): one record object per line.
/// - Anything else: the body is one record's value (`?key=` and `?partition=` apply).
fn parse_body(content_type: &str, body: &Bytes, q: &ProduceQuery) -> ApiResult<Vec<Incoming>> {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let ndjson = ct.contains("ndjson") || ct.contains("jsonl") || ct.contains("json-seq");
    let json = !ndjson && (ct.ends_with("json") || ((ct.is_empty() || ct == "application/x-www-form-urlencoded") && looks_like_json(body)));
    let defaults = |mut i: Incoming| {
        if i.partition.is_none() {
            i.partition = q.partition;
        }
        if i.record.key.is_none()
            && let Some(k) = &q.key
        {
            i.record.key = Some(Bytes::from(k.clone()));
        }
        i
    };
    if ndjson {
        return body
            .split(|c| *c == b'\n')
            .filter(|l| l.iter().any(|c| !c.is_ascii_whitespace()))
            .map(|l| {
                let r: InRecord = serde_json::from_slice(l).map_err(|e| ApiError::bad(format!("bad NDJSON line: {e}")))?;
                incoming(r).map(defaults)
            })
            .collect();
    }
    if json {
        let first = body.iter().find(|c| !c.is_ascii_whitespace());
        let recs: Vec<InRecord> = if first == Some(&b'[') {
            serde_json::from_slice(body).map_err(|e| ApiError::bad(format!("bad JSON: {e}")))?
        } else {
            let mut r: InRecord = serde_json::from_slice(body).map_err(|e| ApiError::bad(format!("bad JSON: {e}")))?;
            match r.records.take() {
                Some(list) => list,
                None => vec![r],
            }
        };
        return recs.into_iter().map(|r| incoming(r).map(defaults)).collect();
    }
    Ok(vec![defaults(Incoming { partition: None, record: NewRecord { key: None, value: Some(body.clone()), headers: vec![], timestamp: None } })])
}

async fn produce(State(api): S, Path((s, t)): Path<(String, String)>, Query(q): Query<ProduceQuery>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let stream = data_stream(&api, &headers, &s)?;
    let acks = Acks::parse(q.acks.as_deref())?;
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let recs = parse_body(ct, &body, &q)?;
    if recs.is_empty() {
        return Err(ApiError::bad("no records in the request"));
    }
    let topic = match stream.topic(&t) {
        Some(t) => t,
        None => api.b.auto_create(&stream, &t).ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_topic_or_partition", format!("topic {t} not found")))?,
    };
    let timeout = Duration::from_millis(q.timeout_ms.unwrap_or(30_000).min(120_000));
    let results = produce_records(&api, &stream, &topic, recs, acks, timeout).await;
    let throttle = stream.ingress.charge(body.len() as u64, stream.settings().ingress_bytes_per_s);
    if throttle > 0 {
        tokio::time::sleep(Duration::from_millis(throttle as u64)).await;
    }
    let mut status = StatusCode::OK;
    let offsets: Vec<Value> = results
        .into_iter()
        .map(|r| match r {
            Ok(p) => json!({ "partition": p.partition, "offset": p.offset, "timestamp": p.timestamp }),
            Err(e) => {
                if status == StatusCode::OK {
                    status = e.status;
                }
                json!({ "error": e.message, "code": e.code })
            }
        })
        .collect();
    Ok((status, Json(json!({ "topic": topic.name, "offsets": offsets }))).into_response())
}

// ── reading ──

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Enc {
    Auto,
    Text,
    Base64,
    Json,
}

impl Enc {
    pub fn parse(s: Option<&str>) -> ApiResult<Self> {
        match s.unwrap_or("auto") {
            "auto" => Ok(Enc::Auto),
            "text" | "string" => Ok(Enc::Text),
            "base64" | "binary" => Ok(Enc::Base64),
            "json" => Ok(Enc::Json),
            other => Err(ApiError::bad(format!("encoding must be auto, text, base64 or json, not {other}"))),
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Out {
    Null,
    Str(String),
    Raw(Box<RawValue>),
}

#[derive(Serialize)]
pub struct OutHeader {
    key: String,
    value: Out,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<&'static str>,
}

#[derive(Serialize)]
pub struct OutRecord {
    pub partition: i32,
    pub offset: i64,
    pub timestamp: i64,
    pub key: Out,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_encoding: Option<&'static str>,
    pub value: Out,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_encoding: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<OutHeader>,
    #[serde(skip)]
    pub size: usize,
}

/// Render bytes: JSON objects/arrays inline (auto, json), UTF-8 as strings, else base64.
fn field(b: Option<&Bytes>, enc: Enc) -> (Out, Option<&'static str>) {
    let Some(b) = b else { return (Out::Null, None) };
    let b64 = || (Out::Str(B64.encode(b)), Some("base64"));
    match enc {
        Enc::Base64 => b64(),
        Enc::Text => (Out::Str(String::from_utf8_lossy(b).into_owned()), None),
        Enc::Auto | Enc::Json => match std::str::from_utf8(b) {
            Ok(s) => {
                let structured = enc == Enc::Json || matches!(s.trim_start().as_bytes().first(), Some(b'{' | b'['));
                if structured && let Ok(raw) = RawValue::from_string(s.trim().to_string()) {
                    return (Out::Raw(raw), None);
                }
                (Out::Str(s.to_string()), None)
            }
            Err(_) => b64(),
        },
    }
}

pub fn out_record(partition: i32, d: records::Decoded, enc: Enc) -> OutRecord {
    let size = d.key.as_ref().map_or(0, |k| k.len()) + d.value.as_ref().map_or(0, |v| v.len()) + 32;
    let (key, key_encoding) = field(d.key.as_ref(), enc);
    let (value, value_encoding) = field(d.value.as_ref(), enc);
    let headers = d
        .headers
        .into_iter()
        .map(|(k, v)| {
            // Header values are text unless they aren't UTF-8 (or base64 was asked for).
            let text = enc != Enc::Base64 && v.as_ref().is_none_or(|v| std::str::from_utf8(v).is_ok());
            let (value, encoding) = field(v.as_ref(), if text { Enc::Text } else { Enc::Base64 });
            OutHeader { key: k, value, encoding }
        })
        .collect();
    OutRecord { partition, offset: d.offset, timestamp: d.timestamp, key, key_encoding, value, value_encoding, headers, size }
}

/// Limits and rendering for a read.
#[derive(Clone, Copy)]
pub struct ReadOpts {
    pub limit: usize,
    pub max_bytes: usize,
    pub committed: bool,
    pub enc: Enc,
}

/// What one partition read produced.
pub struct PartRead {
    pub records: Vec<OutRecord>,
    /// Where the next read should start.
    pub next: i64,
    pub high_watermark: i64,
    pub log_start: i64,
    pub last_stable: i64,
}

/// Read up to `limit` records / `max_bytes` from one partition starting at `offset`. Control
/// batches and aborted transactions (for `committed`) are skipped but still advance `next`.
pub async fn read_records(b: &Broker, stream: &Stream, topic: &Topic, p: i32, offset: i64, o: ReadOpts) -> Result<PartRead, (KafkaError, i64, i64)> {
    let ReadOpts { limit, max_bytes, committed, enc } = o;
    let partition = topic.partition(p).ok_or((KafkaError::code(kafka_protocol::ResponseError::UnknownTopicOrPartition), -1, -1))?;
    let mut out = PartRead { records: vec![], next: offset, high_watermark: -1, log_start: -1, last_stable: -1 };
    let mut bytes = 0;
    loop {
        let r = read_partition(b, stream, topic, &partition, out.next, (max_bytes - bytes).max(1), committed).await?;
        out.high_watermark = r.high_watermark;
        out.log_start = r.log_start;
        out.last_stable = r.last_stable;
        if r.records.is_empty() {
            break;
        }
        let batch_end = batch::headers(&r.records).last().map(|(_, h)| h.next_offset()).unwrap_or(out.next);
        let decoded = records::decode(&r.records, committed.then_some(&r.aborted[..])).map_err(|e| (KafkaError::storage(e), r.high_watermark, r.log_start))?;
        let mut cut = false;
        for d in decoded {
            if d.offset < out.next {
                continue;
            }
            if out.records.len() >= limit || (bytes >= max_bytes && !out.records.is_empty()) {
                cut = true;
                break;
            }
            out.next = d.offset + 1;
            let rec = out_record(p, d, enc);
            bytes += rec.size;
            out.records.push(rec);
        }
        if cut {
            break;
        }
        out.next = out.next.max(batch_end);
        let upper = if committed { r.last_stable } else { r.high_watermark };
        if out.records.len() >= limit || bytes >= max_bytes || out.next >= upper {
            break;
        }
    }
    Ok(out)
}

/// Wait until one of `rxs` changes or `deadline` passes.
pub async fn wait_any(rxs: &mut [watch::Receiver<i64>], deadline: Instant) {
    let left = deadline.saturating_duration_since(Instant::now());
    if rxs.is_empty() || left.is_zero() {
        tokio::time::sleep(left).await;
        return;
    }
    let changed = futures::future::select_all(rxs.iter_mut().map(|rx| Box::pin(rx.changed())));
    // A closed channel means the log was unloaded; back off instead of spinning.
    if let Ok((Err(_), _, _)) = tokio::time::timeout(left, changed).await {
        tokio::time::sleep(Duration::from_millis(50).min(left)).await;
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ReadQuery {
    /// `earliest`, `latest`, an offset, or a negative number for "this many before the end".
    pub offset: Option<String>,
    /// Start at the first record at or after this time (epoch ms or RFC 3339).
    pub timestamp: Option<String>,
    pub limit: Option<usize>,
    pub max_bytes: Option<usize>,
    pub wait_ms: Option<u64>,
    /// `read_committed` (default) or `read_uncommitted`.
    pub isolation: Option<String>,
    pub encoding: Option<String>,
    /// Topic reads: resume from a previous response's cursor.
    pub cursor: Option<String>,
    /// Topic reads without a cursor or group: `earliest` (default) or `latest`.
    pub from: Option<String>,
    /// Topic reads: only these partitions (`0,2,5`).
    pub partitions: Option<String>,
    /// Server-managed consumer group.
    pub group: Option<String>,
    pub consumer: Option<String>,
    /// Group reads: `auto` (default; commit on the consumer's next read), `read` (commit as
    /// delivered) or `manual` (POST .../commit).
    pub commit: Option<String>,
    pub lease_ms: Option<u64>,
    /// Group reads with no committed offset: `earliest` (default) or `latest`.
    pub reset: Option<String>,
}

impl ReadQuery {
    pub fn committed(&self) -> ApiResult<bool> {
        match self.isolation.as_deref().unwrap_or("read_committed") {
            "read_committed" | "committed" => Ok(true),
            "read_uncommitted" | "uncommitted" => Ok(false),
            other => Err(ApiError::bad(format!("isolation must be read_committed or read_uncommitted, not {other}"))),
        }
    }
    pub fn opts(&self) -> ApiResult<ReadOpts> {
        Ok(ReadOpts { limit: self.limit(), max_bytes: self.max_bytes(), committed: self.committed()?, enc: Enc::parse(self.encoding.as_deref())? })
    }
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }
    pub fn max_bytes(&self) -> usize {
        self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES).clamp(1024, MAX_BYTES)
    }
    pub fn wait(&self) -> Duration {
        Duration::from_millis(self.wait_ms.unwrap_or(0).min(MAX_WAIT_MS))
    }
    pub fn partition_filter(&self, n: i32) -> ApiResult<Vec<i32>> {
        match &self.partitions {
            None => Ok((0..n).collect()),
            Some(s) => s
                .split(',')
                .filter(|x| !x.trim().is_empty())
                .map(|x| match x.trim().parse::<i32>() {
                    Ok(p) if (0..n).contains(&p) => Ok(p),
                    _ => Err(ApiError::bad(format!("bad partition {x}"))),
                })
                .collect(),
        }
    }
}

fn out_of_range(e: KafkaError, hw: i64, start: i64) -> ApiError {
    let mut err = ApiError::from(e);
    if err.code == "offset_out_of_range" {
        err.status = StatusCode::RANGE_NOT_SATISFIABLE;
        err = err.with(json!({ "log_start_offset": start, "high_watermark": hw }));
    }
    err
}

async fn start_offset(b: &Broker, stream: &Stream, topic: &Topic, p: i32, q: &ReadQuery, committed: bool) -> ApiResult<i64> {
    let partition = topic.partition(p).ok_or_else(|| ApiError::not_found(format!("partition {p} not found")))?;
    let log = b.log(topic, &partition)?;
    let end = if committed { log.last_stable() } else { log.high_watermark() };
    if let Some(ts) = &q.timestamp {
        let ms = ts.parse::<i64>().ok().or_else(|| super::time::parse_ms(ts)).ok_or_else(|| ApiError::bad("timestamp must be epoch ms or RFC 3339"))?;
        let found = crate::handlers::offsets::offset_for_timestamp(b, stream, topic, &partition, ms).await.map_err(KafkaError::storage)?;
        return Ok(found.map(|(o, _)| o).unwrap_or(end));
    }
    Ok(match q.offset.as_deref().unwrap_or("earliest") {
        "earliest" | "beginning" => log.log_start(),
        "latest" | "end" => end,
        n => match n.parse::<i64>() {
            Ok(n) if n < 0 => (end + n).max(log.log_start()),
            Ok(n) => n,
            Err(_) => return Err(ApiError::bad("offset must be earliest, latest or a number")),
        },
    })
}

/// `GET .../partitions/{p}/records`: stateless reads by offset.
async fn read_one(State(api): S, Path((s, t, p)): Path<(String, String, i32)>, Query(q): Query<ReadQuery>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let stream = data_stream(&api, &headers, &s)?;
    let topic = super::topic_of(&stream, &t)?;
    let offset = start_offset(&api.b, &stream, &topic, p, &q, q.committed()?).await?;
    let partition = topic.partition(p).ok_or_else(|| ApiError::not_found(format!("partition {p} not found")))?;
    let mut rx = api.b.log(&topic, &partition)?.subscribe();
    let deadline = Instant::now() + q.wait();
    loop {
        let r = read_records(&api.b, &stream, &topic, p, offset, q.opts()?).await.map_err(|(e, hw, start)| out_of_range(e, hw, start))?;
        if !r.records.is_empty() || r.next != offset || Instant::now() >= deadline {
            throttle_out(&stream, &r.records).await;
            return Ok(Json(json!({
                "topic": topic.name,
                "partition": p,
                "records": r.records,
                "next_offset": r.next,
                "high_watermark": r.high_watermark,
                "last_stable_offset": r.last_stable,
                "log_start_offset": r.log_start,
            })));
        }
        wait_any(std::slice::from_mut(&mut rx), deadline).await;
    }
}

async fn throttle_out(stream: &Stream, records: &[OutRecord]) {
    let bytes: usize = records.iter().map(|r| r.size).sum();
    let ms = stream.egress.charge(bytes as u64, stream.settings().egress_bytes_per_s);
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
    }
}

/// `p.offset,p.offset`: where each partition's next read starts.
pub fn format_cursor(pos: &BTreeMap<i32, i64>) -> String {
    pos.iter().map(|(p, o)| format!("{p}.{o}")).collect::<Vec<_>>().join(",")
}

pub fn parse_cursor(s: &str) -> ApiResult<BTreeMap<i32, i64>> {
    s.split(',')
        .filter(|x| !x.is_empty())
        .map(|x| {
            let (p, o) = x.split_once('.').ok_or_else(|| ApiError::bad("bad cursor"))?;
            Ok((p.parse().map_err(|_| ApiError::bad("bad cursor"))?, o.parse().map_err(|_| ApiError::bad("bad cursor"))?))
        })
        .collect()
}

/// Read several partitions from `pos` (advancing it), fairly, long-polling until something
/// arrives or `wait` passes. Positions below the log start (retention) skip ahead.
pub async fn read_many(
    api: &Api,
    stream: &Stream,
    topic: &Topic,
    pos: &mut BTreeMap<i32, i64>,
    o: ReadOpts,
    wait: Duration,
) -> ApiResult<Vec<OutRecord>> {
    let b = &api.b;
    let mut rxs = vec![];
    for p in pos.keys() {
        if let Some(part) = topic.partition(*p) {
            rxs.push(b.log(topic, &part)?.subscribe());
        }
    }
    let deadline = Instant::now() + wait;
    let parts: Vec<i32> = pos.keys().copied().collect();
    loop {
        let mut out = vec![];
        let mut bytes = 0;
        let start = api.rr.fetch_add(1, Ordering::Relaxed);
        for i in 0..parts.len() {
            if out.len() >= o.limit || bytes >= o.max_bytes {
                break;
            }
            let p = parts[(start + i) % parts.len()];
            let offset = pos[&p];
            let r = match read_records(b, stream, topic, p, offset, ReadOpts { limit: o.limit - out.len(), max_bytes: o.max_bytes - bytes, ..o }).await {
                Ok(r) => r,
                Err((e, hw, start)) if e.code == kafka_protocol::ResponseError::OffsetOutOfRange => {
                    pos.insert(p, if offset < start { start } else { hw });
                    continue;
                }
                Err((e, _, _)) => return Err(e.into()),
            };
            pos.insert(p, r.next);
            bytes += r.records.iter().map(|r| r.size).sum::<usize>();
            out.extend(r.records);
        }
        if !out.is_empty() || Instant::now() >= deadline {
            throttle_out(stream, &out).await;
            return Ok(out);
        }
        wait_any(&mut rxs, deadline).await;
    }
}

/// `GET .../topics/{t}/records`: all partitions, with a cursor (stateless) or a group.
async fn read_topic(State(api): S, Path((s, t)): Path<(String, String)>, Query(q): Query<ReadQuery>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let stream = data_stream(&api, &headers, &s)?;
    let topic = super::topic_of(&stream, &t)?;
    if let Some(group) = q.group.clone() {
        return leases::consume(&api, &stream, &topic, &group, &q).await.map(Json);
    }
    let mut pos = initial_positions(&api.b, &stream, &topic, &q, q.committed()?).await?;
    let records = read_many(&api, &stream, &topic, &mut pos, q.opts()?, q.wait()).await?;
    Ok(Json(json!({ "topic": topic.name, "records": records, "cursor": format_cursor(&pos) })))
}

/// Positions from `cursor`, else `timestamp`/`from` for every selected partition. Partitions a
/// cursor doesn't mention (added since) start at their beginning.
async fn initial_positions(b: &Broker, stream: &Stream, topic: &Topic, q: &ReadQuery, committed: bool) -> ApiResult<BTreeMap<i32, i64>> {
    let parts = q.partition_filter(topic.partition_count())?;
    let cursor = q.cursor.as_deref().map(parse_cursor).transpose()?;
    let mut pos = BTreeMap::new();
    for p in parts {
        let o = match cursor.as_ref().and_then(|c| c.get(&p)) {
            Some(o) => *o,
            None if cursor.is_some() => {
                let part = topic.partition(p).expect("filtered");
                b.log(topic, &part)?.log_start()
            }
            None => {
                let mut pq = ReadQuery { timestamp: q.timestamp.clone(), offset: q.from.clone(), ..Default::default() };
                if pq.offset.is_none() {
                    pq.offset = Some("earliest".into());
                }
                start_offset(b, stream, topic, p, &pq, committed).await?
            }
        };
        pos.insert(p, o);
    }
    Ok(pos)
}

// ── SSE ──

/// `GET .../topics/{t}/tail`: Server-Sent Events, one `record` event per record. Event ids are
/// cursors, so `Last-Event-ID` resumes where a dropped connection stopped.
async fn tail(State(api): S, Path((s, t)): Path<(String, String)>, Query(mut q): Query<ReadQuery>, headers: HeaderMap) -> ApiResult<Response> {
    let stream = data_stream(&api, &headers, &s)?;
    let topic = super::topic_of(&stream, &t)?;
    let opts = q.opts()?;
    if let Some(id) = headers.get("last-event-id").and_then(|v| v.to_str().ok()) {
        q.cursor = Some(id.to_string());
    }
    if q.from.is_none() && q.timestamp.is_none() {
        q.from = Some("latest".into());
    }
    let mut pos = initial_positions(&api.b, &stream, &topic, &q, opts.committed).await?;
    let (tx, rx) = mpsc::channel::<Event>(64);
    let api2 = api.clone();
    tokio::spawn(async move {
        loop {
            let records = match read_many(&api2, &stream, &topic, &mut pos, ReadOpts { limit: 500, max_bytes: 1 << 20, ..opts }, Duration::from_secs(15)).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Event::default().event("error").data(json!({ "error": e.message, "code": e.code }).to_string())).await;
                    return;
                }
            };
            let n = records.len();
            for (i, r) in records.into_iter().enumerate() {
                let mut ev = Event::default().event("record").data(serde_json::to_string(&r).unwrap_or_default());
                if i + 1 == n {
                    ev = ev.id(format_cursor(&pos));
                }
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
            if tx.is_closed() || topic.deleted.load(Ordering::Relaxed) {
                return;
            }
        }
    });
    let events = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (Ok::<_, Infallible>(e), rx)) });
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}

// ── group commits ──

#[derive(Deserialize)]
struct CommitOffset {
    topic: String,
    partition: i32,
    offset: i64,
    #[serde(default)]
    metadata: Option<String>,
}

#[derive(Deserialize)]
struct CommitBody {
    #[serde(default)]
    consumer: Option<String>,
    offsets: Vec<CommitOffset>,
}

async fn commit(State(api): S, Path((s, g)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<CommitBody>) -> ApiResult<Json<Value>> {
    let stream = data_stream(&api, &headers, &s)?;
    if api.b.groups.is_active(&stream.id, &g) {
        return Err(ApiError::conflict(format!("group {g} has Kafka members; commit through the Kafka protocol")));
    }
    let mut rows = vec![];
    for o in body.offsets {
        let topic = super::topic_of(&stream, &o.topic)?;
        if topic.partition(o.partition).is_none() {
            return Err(ApiError::not_found(format!("partition {} of {} not found", o.partition, o.topic)));
        }
        rows.push(crate::store::OffsetRow { topic: o.topic, partition: o.partition, offset: o.offset, leader_epoch: -1, metadata: o.metadata, committed_at: crate::ids::now() });
    }
    api.b.store.commit_offsets(&stream.id, &g, &rows).map_err(KafkaError::internal)?;
    api.leases.committed(&stream.id, &g, body.consumer.as_deref(), &rows);
    Ok(Json(json!({ "group": g, "committed": rows.len() })))
}

/// Leave a group: commit what was delivered (in `auto` mode) and free the partitions now.
async fn leave(State(api): S, Path((s, g, c)): Path<(String, String, String)>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let stream = data_stream(&api, &headers, &s)?;
    let rows = api.leases.leave(&stream.id, &g, &c);
    if !rows.is_empty() {
        api.b.store.commit_offsets(&stream.id, &g, &rows).map_err(KafkaError::internal)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

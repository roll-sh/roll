//! Server access logging: Get/PutBucketLogging, and delivery of the logs.
//!
//! Requests to a bucket with logging enabled are recorded in memory, one line each in S3's
//! server access log format. Every `Config::access_log_interval` (and on
//! [`crate::Server::flush_access_logs`]) the buffered lines are written as new objects to each
//! target bucket, through the normal put path: they're durable once written, and metered (and
//! billed) to the target bucket like any upload.
//!
//! Like S3's, delivery is best-effort: lines still in memory when the process stops are lost,
//! a failed write is logged and dropped, and at most [`MAX_BUFFERED`] lines wait at a time
//! (beyond that they're dropped and counted). Fields roll can't know (TLS details, turn-around
//! time) are `-`.

use crate::config::{Doc, malformed, node_xml, read_xml, reading, writing};
use crate::ctx::Ctx;
use crate::response::{self, Reply};
use crate::Server;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use s3_proto::http::uri_encode;
use s3_proto::{Code, S3Error};
use s3_store::config::kind;
use s3_store::{Bucket, Class, ObjectHeaders, WriteCondition};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Most log lines held in memory at once.
pub(crate) const MAX_BUFFERED: usize = 100_000;
/// How long a bucket's logging configuration is cached for recording.
const CONFIG_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct LoggingConfig {
    pub target_bucket: String,
    pub target_prefix: String,
    /// `None`: `SimplePrefix`; else `PartitionedPrefix` with this `PartitionDateSource`
    /// (`EventTime` or `DeliveryTime`).
    pub partition_date_source: Option<String>,
    /// `TargetGrants` as given (stored and returned; the target's own ACL decides access).
    pub target_grants: Option<String>,
}

struct Line {
    source: String,
    target: LoggingConfig,
    at_ms: i64,
    text: String,
}

#[derive(Default)]
pub(crate) struct Logs {
    buf: Mutex<Vec<Line>>,
    targets: Mutex<HashMap<String, (Instant, Option<LoggingConfig>)>>,
    pub dropped: AtomicU64,
}

impl Logs {
    fn target(&self, server: &Server, bucket: &str) -> Option<LoggingConfig> {
        if let Some((at, t)) = self.targets.lock().unwrap().get(bucket)
            && at.elapsed() < CONFIG_TTL
        {
            return t.clone();
        }
        let t = server.store.bucket_config(bucket, kind::LOGGING, "").ok().flatten().and_then(|v| serde_json::from_str(&v).ok());
        self.targets.lock().unwrap().insert(bucket.to_string(), (Instant::now(), t.clone()));
        t
    }

    pub(crate) fn forget(&self, bucket: &str) {
        self.targets.lock().unwrap().remove(bucket);
    }
}

pub(crate) fn get_config(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let c: Option<LoggingConfig> = ctx.access(&bucket.name)?.settings.get(kind::LOGGING).and_then(|v| serde_json::from_str(v).ok());
    let mut d = Doc::new("BucketLoggingStatus");
    if let Some(c) = c {
        d.open("LoggingEnabled").el("TargetBucket", &c.target_bucket).el("TargetPrefix", &c.target_prefix);
        if let Some(g) = &c.target_grants {
            d.raw(g);
        }
        d.open("TargetObjectKeyFormat");
        match &c.partition_date_source {
            None => d.raw("<SimplePrefix></SimplePrefix>"),
            Some(s) => d.open("PartitionedPrefix").el("PartitionDateSource", s).close("PartitionedPrefix"),
        };
        d.close("TargetObjectKeyFormat").close("LoggingEnabled");
    }
    response::ok_xml(d.finish("BucketLoggingStatus"))
}

pub(crate) async fn put_config(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "BucketLoggingStatus").await?;
    let value = match n.child("LoggingEnabled") {
        None => None,
        Some(le) => {
            let target = le.text_of("TargetBucket").map(str::trim).filter(|t| !t.is_empty()).ok_or_else(malformed)?.to_string();
            let prefix = le.text_of("TargetPrefix").unwrap_or_default().to_string();
            let missing = || S3Error::new(Code::InvalidTargetBucketForLogging).with("TargetBucket", target.clone());
            let Ok(t) = ctx.store().bucket(&target) else { return Err(missing()) };
            // As on S3, the source and target buckets must have the same owner.
            if t.account_id != bucket.account_id {
                return Err(S3Error::msg(Code::InvalidTargetBucketForLogging, "The owner for the bucket to be logged and the target bucket must be the same.").with("TargetBucket", target.clone()));
            }
            // The requester must be able to write there: logs are delivered as them, in effect.
            if crate::access::authorize(ctx, ctx.who(), &target, None, "s3:PutObject", crate::auth::Need::Write).is_err() {
                return Err(S3Error::msg(Code::InvalidTargetBucketForLogging, "You must be able to write to the target bucket").with("TargetBucket", target.clone()));
            }
            let partition_date_source = match le.child("TargetObjectKeyFormat") {
                None => None,
                Some(f) => match (f.child("SimplePrefix"), f.child("PartitionedPrefix")) {
                    (Some(_), None) => None,
                    (None, Some(p)) => Some(p.text_of("PartitionDateSource").map(str::trim).filter(|s| *s == "EventTime" || *s == "DeliveryTime").unwrap_or("EventTime").to_string()),
                    _ => return Err(malformed()),
                },
            };
            let c = LoggingConfig { target_bucket: target, target_prefix: prefix, partition_date_source, target_grants: le.child("TargetGrants").map(node_xml) };
            Some(serde_json::to_string(&c).map_err(|_| S3Error::new(Code::InternalError))?)
        }
    };
    crate::config::save(ctx, bucket, kind::LOGGING, value.as_deref()).await?;
    ctx.server.logs.forget(&bucket.name);
    Ok(response::empty(StatusCode::OK))
}

/// `[06/Feb/2019:00:00:38 +0000]`.
fn log_time(ms: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = ms.div_euclid(1000);
    let (y, m, d) = s3_proto::time::civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("[{d:02}/{}/{y}:{:02}:{:02}:{:02} +0000]", MONTHS[(m - 1) as usize], t / 3600, t % 3600 / 60, t % 60)
}

/// `YYYY-MM-DD-hh-mm-ss`, and the date parts.
fn stamp(ms: i64) -> (String, (i64, u32, u32)) {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = s3_proto::time::civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    (format!("{y:04}-{m:02}-{d:02}-{:02}-{:02}-{:02}", t / 3600, t % 3600 / 60, t % 60), (y, m, d))
}

/// S3's operation name: `REST.GET.OBJECT`, `REST.PUT.PART`, `WEBSITE.GET.OBJECT`, …
fn operation(ctx: &Ctx) -> String {
    let api = if ctx.website { "WEBSITE" } else { "REST" };
    let mut method = ctx.method.as_str().to_string();
    let resource = if ctx.key.is_some() {
        if ctx.has("tagging") {
            "OBJECT_TAGGING".to_string()
        } else if ctx.has("acl") {
            "ACL".to_string()
        } else if ctx.has("uploads") {
            "UPLOADS".to_string()
        } else if ctx.has("uploadId") {
            if ctx.method == axum::http::Method::PUT { "PART".to_string() } else { "UPLOAD".to_string() }
        } else if ctx.has("restore") {
            "OBJECT_RESTORE".to_string()
        } else {
            if ctx.method == axum::http::Method::PUT && ctx.h("x-amz-copy-source").is_some() {
                method = "COPY".into();
            }
            "OBJECT".to_string()
        }
    } else {
        const NAMES: [(&str, &str); 12] = [
            ("policy", "BUCKETPOLICY"),
            ("policyStatus", "POLICY_STATUS"),
            ("logging", "LOGGING_STATUS"),
            ("delete", "MULTI_OBJECT_DELETE"),
            ("publicAccessBlock", "PUBLIC_ACCESS_BLOCK"),
            ("ownershipControls", "OWNERSHIP_CONTROLS"),
            ("requestPayment", "REQUEST_PAYMENT"),
            ("intelligent-tiering", "INTELLIGENT_TIERING"),
            ("object-lock", "OBJECT_LOCK_CONFIGURATION"),
            ("versions", "BUCKETVERSIONS"),
            ("location", "LOCATION"),
            ("uploads", "UPLOADS"),
        ];
        const SUBS: [&str; 15] = ["acl", "cors", "lifecycle", "tagging", "website", "versioning", "encryption", "accelerate", "replication", "notification", "analytics", "metrics", "inventory", "torrent", "session"];
        if let Some((_, n)) = NAMES.iter().find(|(s, _)| ctx.has(s)) {
            n.to_string()
        } else if let Some(s) = SUBS.iter().find(|s| ctx.has(s)) {
            s.to_ascii_uppercase()
        } else {
            "BUCKET".to_string()
        }
    };
    format!("{api}.{method}.{resource}")
}

fn quoted(v: Option<&str>) -> String {
    match v {
        Some(v) => format!("\"{}\"", v.replace('"', "\\\"")),
        None => "\"-\"".into(),
    }
}

/// Record a request in its bucket's access log, if the bucket has logging on.
pub(crate) fn record(ctx: &Ctx, resp: &Response, request_id: &str, error: Option<Code>) {
    let Some(bucket) = ctx.bucket.as_deref() else { return };
    let server = &ctx.server;
    let Some(target) = server.logs.target(server, bucket) else { return };
    let store = &server.store;
    let now = store.now();
    let dash = || "-".to_string();
    // The requester as S3 logs it: an IAM user's ARN, else a canonical id (a roll key's).
    let requester = ctx.auth.as_ref().map_or_else(dash, |a| match &a.identity.kind {
        crate::identity::Kind::User { arn, .. } => arn.clone(),
        _ => store.canonical_user(&a.key.id),
    });
    let owner = store.bucket(bucket).ok().and_then(|b| store.account(&b.account_id).ok().flatten()).map_or_else(|| store.owner_id(), |a| a.canonical);
    // Session tokens never go into logs.
    let uri = if ctx.raw_query.is_empty() { ctx.raw_path.clone() } else { format!("{}?{}", ctx.raw_path, crate::temporary::redact_query(&ctx.raw_query)) };
    let bytes_sent = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).filter(|v| *v != "0").map_or_else(dash, str::to_string);
    let presigned = ctx.query.iter().any(|(k, _)| k.eq_ignore_ascii_case("X-Amz-Signature"));
    let fields = [
        owner,
        bucket.to_string(),
        log_time(now),
        ctx.peer.map_or_else(dash, |p| p.to_string()),
        requester,
        request_id.to_string(),
        operation(ctx),
        ctx.key.as_deref().map_or_else(dash, |k| uri_encode(k, false)),
        quoted(Some(&format!("{} {uri} HTTP/1.1", ctx.method))),
        resp.status().as_u16().to_string(),
        error.map_or_else(dash, |c| c.as_str().to_string()),
        bytes_sent,
        dash(),
        ctx.started.elapsed().as_millis().to_string(),
        dash(),
        quoted(ctx.h("referer")),
        quoted(ctx.h("user-agent")),
        dash(),
        request_id.to_string(),
        if ctx.auth.is_some() { "SigV4".into() } else { dash() },
        dash(),
        match (&ctx.auth, presigned) {
            (Some(_), true) => "QueryString".into(),
            (Some(_), false) => "AuthHeader".into(),
            (None, _) => dash(),
        },
        ctx.h("host").map_or_else(dash, str::to_string),
        dash(),
        dash(),
        dash(),
    ];
    let line = Line { source: bucket.to_string(), target, at_ms: now, text: fields.join(" ") };
    let mut buf = server.logs.buf.lock().unwrap();
    if buf.len() >= MAX_BUFFERED {
        server.logs.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    buf.push(line);
}

fn unique() -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode_upper(b)
}

/// Write the buffered lines to their target buckets: one object per target (and source
/// bucket, for partitioned keys). Returns how many objects were written.
pub(crate) async fn flush(server: &Server) -> usize {
    let lines = std::mem::take(&mut *server.logs.buf.lock().unwrap());
    if lines.is_empty() {
        return 0;
    }
    let mut groups: BTreeMap<(String, String, Option<String>, String), Vec<Line>> = BTreeMap::new();
    for l in lines {
        let source = if l.target.partition_date_source.is_some() { l.source.clone() } else { String::new() };
        groups.entry((l.target.target_bucket.clone(), l.target.target_prefix.clone(), l.target.partition_date_source.clone(), source)).or_default().push(l);
    }
    let store = &server.store;
    let now = store.now();
    let mut written = 0;
    for ((target, prefix, partition, source), lines) in groups {
        let first = lines.iter().map(|l| l.at_ms).min().unwrap_or(now);
        let key = match partition.as_deref() {
            None => format!("{prefix}{}-{}", stamp(now).0, unique()),
            Some(src) => {
                let at = if src == "EventTime" { first } else { now };
                let (s, (y, m, d)) = stamp(at);
                let account = store.bucket(&source).map_or_else(|_| store.account_id(), |b| b.account_id);
                format!("{prefix}{account}/{}/{source}/{y:04}/{m:02}/{d:02}/{s}-{}", server.cfg.region, unique())
            }
        };
        let mut body = String::new();
        for l in &lines {
            body.push_str(&l.text);
            body.push('\n');
        }
        let bucket = match store.bucket(&target) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(target, error = %e, "access log target bucket is gone; dropping its logs");
                continue;
            }
        };
        let len = body.len() as u64;
        let headers = ObjectHeaders { content_type: Some("text/plain".into()), ..Default::default() };
        match crate::put_object(store, &bucket, &key, headers, s3_store::bytes_body(bytes::Bytes::from(body)), len, &WriteCondition::none()).await {
            Ok(_) => {
                store.meter.request(&bucket.id, Class::A);
                store.meter.bytes_in(&bucket.id, len);
                written += 1;
            }
            Err(e) => tracing::warn!(target, key, error = %e, "delivering access logs failed; they're dropped (best effort)"),
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(log_time(1_549_411_238_000), "[06/Feb/2019:00:00:38 +0000]");
        assert_eq!(stamp(1_549_411_238_000).0, "2019-02-06-00-00-38");
    }
}

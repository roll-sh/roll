//! Event notifications: the dispatcher that drains the store's durable outbox
//! (`s3_store::events`), the S3 event JSON, and delivery.
//!
//! Destinations, by ARN:
//! - `arn:roll:queues:<vhost>:<queue>` and `arn:roll:streams:<stream>:<topic>`: roll queues and
//!   roll streams, in process, through the [`EventSink`] rolld installs
//!   ([`Notifier::set_sink`]). The s3 crates don't depend on either product.
//! - `arn:roll:webhook:<url>`: an HTTP POST of the event JSON, signed with the bucket's webhook
//!   secret (`X-Roll-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">`).
//! - `arn:aws:sqs:…`, `arn:aws:sns:…`, `arn:aws:lambda:…` and `EventBridgeConfiguration`: real
//!   AWS, SigV4-signed with the deployment's AWS credentials, only when
//!   [`NotifyConfig::aws`] is on (`ROLL_S3_NOTIFY_AWS`).
//!
//! Delivery is at least once. Each outbox row is tried until it's delivered, with exponential
//! backoff; a row still failing after [`NotifyConfig::retention`] (3 days) is dropped, counted
//! and logged. Rows of one destination and key go out in order (the store only hands out the
//! oldest), and every record carries S3's per-key `sequencer`.

use crate::Server;
use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use hmac::{Hmac, Mac};
use s3_proto::time::{amz_date, iso8601_ms};
use s3_store::events::{BACKFILL, EVENT_BRIDGE, REPLICATE};
use s3_store::{Bucket, EventRecord, NotificationConfig, PendingEvent, Replicated, Store, StoreError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How events are delivered.
#[derive(Clone, Debug)]
pub struct NotifyConfig {
    /// How often the dispatcher looks for due events; `None`: only on
    /// [`Server::dispatch_events`].
    pub dispatch_interval: Option<Duration>,
    /// Events still failing after this long are dropped (counted and logged).
    pub retention: Duration,
    /// Retries back off from `backoff_base`, doubling up to `backoff_max`.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Deliver to real AWS (SQS, SNS, Lambda, EventBridge) with the deployment's AWS credentials
    /// (`ROLL_S3_NOTIFY_AWS`). Off: AWS destinations fail validation, as on S3 for a destination
    /// S3 can't reach.
    pub aws: bool,
    /// Let webhooks reach loopback, private and link-local addresses (tests, single-tenant
    /// installs). Off, a webhook resolving there is refused (no SSRF into the network).
    pub webhook_allow_private: bool,
    pub webhook_timeout: Duration,
    /// How often inventory configurations are checked for a due report; `None`: only on
    /// [`Server::run_inventory`].
    pub inventory_interval: Option<Duration>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            dispatch_interval: Some(Duration::from_millis(250)),
            retention: Duration::from_secs(3 * 86_400),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(600),
            aws: false,
            webhook_allow_private: false,
            webhook_timeout: Duration::from_secs(10),
            inventory_interval: Some(Duration::from_secs(3600)),
        }
    }
}

/// A roll product an event can go to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RollDestination {
    /// `arn:roll:queues:<vhost>:<queue>`: published to the queue (default exchange).
    Queue { vhost: String, queue: String },
    /// `arn:roll:streams:<stream>:<topic>`: produced to the topic, keyed by the object key.
    Stream { stream: String, topic: String },
}

/// One event to hand to a destination.
#[derive(Clone, Debug)]
pub struct Delivery {
    /// Unique per outbox row (stable across retries): a message id.
    pub id: String,
    /// `s3:ObjectCreated:Put`, `s3:TestEvent`, …
    pub event_name: String,
    pub bucket: String,
    pub key: String,
    /// The S3 event JSON.
    pub body: Bytes,
}

/// Delivery into roll queues and streams. rolld implements it with mq-core and stream-broker;
/// the s3 crates only know this trait. Errors are messages (the dispatcher retries).
pub trait EventSink: Send + Sync {
    /// Whether the destination exists (destination validation).
    fn check<'a>(&'a self, dest: &'a RollDestination) -> BoxFuture<'a, Result<(), String>>;
    /// Deliver, returning once the destination has it durably.
    fn deliver<'a>(&'a self, dest: &'a RollDestination, d: &'a Delivery) -> BoxFuture<'a, Result<(), String>>;
}

/// Where an ARN points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    Roll(RollDestination),
    Webhook(String),
    Sqs { region: String, account: String, name: String },
    Sns { region: String, arn: String },
    Lambda { region: String, arn: String },
    EventBridge,
}

impl Destination {
    pub fn parse(arn: &str) -> Result<Self, String> {
        let bad = || "The ARN is not well formed".to_string();
        if arn == EVENT_BRIDGE {
            return Ok(Self::EventBridge);
        }
        if let Some(rest) = arn.strip_prefix("arn:roll:queues:") {
            let (vhost, queue) = rest.split_once(':').ok_or_else(bad)?;
            let vhost = s3_proto::http::percent_decode_utf8(vhost).map_err(|_| bad())?;
            if vhost.is_empty() || queue.is_empty() {
                return Err(bad());
            }
            return Ok(Self::Roll(RollDestination::Queue { vhost, queue: queue.to_string() }));
        }
        if let Some(rest) = arn.strip_prefix("arn:roll:streams:") {
            let (stream, topic) = rest.split_once(':').ok_or_else(bad)?;
            if stream.is_empty() || topic.is_empty() {
                return Err(bad());
            }
            return Ok(Self::Roll(RollDestination::Stream { stream: stream.to_string(), topic: topic.to_string() }));
        }
        if let Some(url) = arn.strip_prefix("arn:roll:webhook:") {
            let u = url::Url::parse(url).map_err(|_| "The webhook URL is not valid".to_string())?;
            if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() || !u.username().is_empty() {
                return Err("A webhook URL is http(s)://host/path, without credentials".into());
            }
            return Ok(Self::Webhook(url.to_string()));
        }
        let parts: Vec<&str> = arn.splitn(6, ':').collect();
        if parts.len() < 6 || parts[0] != "arn" || !parts[1].starts_with("aws") || parts[3].is_empty() {
            return Err(bad());
        }
        let region = parts[3].to_string();
        match parts[2] {
            "sqs" if !parts[5].is_empty() && !parts[4].is_empty() => Ok(Self::Sqs { region, account: parts[4].to_string(), name: parts[5].to_string() }),
            "sns" if !parts[5].is_empty() => Ok(Self::Sns { region, arn: arn.to_string() }),
            "lambda" if parts[5].starts_with("function:") => Ok(Self::Lambda { region, arn: arn.to_string() }),
            _ => Err(bad()),
        }
    }

    /// The configuration element S3 would list it under when none is named.
    pub fn default_kind(arn: &str) -> &'static str {
        match Self::parse(arn) {
            Ok(Self::Sqs { .. } | Self::Roll(RollDestination::Queue { .. })) => "queue",
            Ok(Self::Lambda { .. }) => "function",
            _ => "topic",
        }
    }
}

/// Why a delivery didn't happen.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Try again later.
    Retry(String),
    /// It can never work (an AWS destination on a deployment without AWS delivery).
    Permanent(String),
}

/// Why a notification configuration was refused.
#[derive(Debug)]
pub enum NotifyError {
    /// The configuration itself (S3's `InvalidArgument`).
    Invalid(String),
    /// Destinations that failed validation: `(ARN, why)`.
    Destinations(Vec<(String, String)>),
    Store(StoreError),
}

impl From<StoreError> for NotifyError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// Counters since start.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct EventStats {
    pub delivered: u64,
    pub retried: u64,
    /// Given up on (past the retention window, or undeliverable).
    pub dropped: u64,
    pub replicated: u64,
    pub replication_failed: u64,
}

#[derive(Default)]
struct Counters {
    delivered: AtomicU64,
    retried: AtomicU64,
    dropped: AtomicU64,
    replicated: AtomicU64,
    replication_failed: AtomicU64,
}

/// The event dispatcher and destination validation, one per [`Server`].
pub struct Notifier {
    store: Arc<Store>,
    cfg: NotifyConfig,
    region: String,
    sink: RwLock<Option<Arc<dyn EventSink>>>,
    http: reqwest::Client,
    aws: tokio::sync::OnceCell<Option<aws_sdk_s3::config::SharedCredentialsProvider>>,
    counters: Counters,
}

/// Resolves webhook hosts, refusing addresses inside the network unless allowed.
struct GuardedResolver {
    allow_private: bool,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let allow = self.allow_private;
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let ok: Vec<SocketAddr> = addrs.into_iter().filter(|a| allow || public_ip(a.ip())).collect();
            if ok.is_empty() {
                return Err(format!("{host} resolves only to addresses inside the network").into());
            }
            Ok(Box::new(ok.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Not loopback, private, link-local (cloud metadata), CGNAT, multicast or unspecified.
pub(crate) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast() || o[0] == 0 || (o[0] == 100 && (64..128).contains(&o[1])))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_ip(IpAddr::V4(v4)),
            None => {
                let s = v6.segments();
                !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80)
            }
        },
    }
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// `X-Roll-Signature` of a webhook body.
pub fn webhook_signature(secret: &str, unix_s: i64, body: &[u8]) -> String {
    let mut msg = unix_s.to_string().into_bytes();
    msg.push(b'.');
    msg.extend_from_slice(body);
    format!("t={unix_s},v1={}", hex::encode(hmac(secret.as_bytes(), &msg)))
}

/// An object key as S3 writes it in event records: form-encoded (`red flower.jpg` →
/// `red+flower.jpg`), slashes kept.
pub fn event_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        match b {
            b' ' => out.push('+'),
            b if b.is_ascii_alphanumeric() || b"-_.*/".contains(&b) => out.push(b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn event_version(name: &str) -> &'static str {
    if name.starts_with("Replication:") {
        "2.2"
    } else if name.starts_with("LifecycleExpiration:") || name.starts_with("LifecycleTransition") || name.starts_with("IntelligentTiering") || name.starts_with("ObjectAcl:") || name.starts_with("ObjectTagging:") || name == "ObjectRestore:Delete" {
        "2.3"
    } else {
        "2.1"
    }
}

/// The S3 event JSON (`{"Records": [...]}`) of a record; `s3:TestEvent` has its own shape.
pub fn event_json(r: &EventRecord, region: &str, owner: &str) -> Value {
    let request_id = r.request_id.clone().unwrap_or_else(|| r.sequencer.clone());
    if r.name == "TestEvent" {
        return json!({ "Service": "Amazon S3", "Event": "s3:TestEvent", "Time": iso8601_ms(r.time), "Bucket": r.bucket, "RequestId": request_id, "HostId": request_id });
    }
    let mut object = json!({ "key": event_key(&r.key), "sequencer": r.sequencer });
    if let Some(s) = r.size {
        object["size"] = json!(s);
    }
    if let Some(e) = &r.etag {
        object["eTag"] = json!(e);
    }
    if let Some(v) = &r.version_id {
        object["versionId"] = json!(v);
    }
    let mut rec = json!({
        "eventVersion": event_version(&r.name),
        "eventSource": "aws:s3",
        "awsRegion": region,
        "eventTime": iso8601_ms(r.time),
        "eventName": r.name,
        "userIdentity": { "principalId": r.principal.as_deref().unwrap_or("roll") },
        "requestParameters": { "sourceIPAddress": r.source_ip.as_deref().unwrap_or("") },
        "responseElements": { "x-amz-request-id": request_id, "x-amz-id-2": request_id },
        "s3": {
            "s3SchemaVersion": "1.0",
            "configurationId": r.config_id,
            "bucket": { "name": r.bucket, "ownerIdentity": { "principalId": owner }, "arn": format!("arn:aws:s3:::{}", r.bucket) },
            "object": object,
        },
    });
    if r.name == "ObjectRestore:Completed" {
        // roll's trash restore: permanent, so no expiry.
        rec["glacierEventData"] = json!({ "restoreEventData": { "lifecycleRestoreStorageClass": "STANDARD" } });
    }
    if r.name.starts_with("Replication:") {
        rec["replicationEventData"] = json!({
            "destinationBucket": r.replication_destination.as_ref().map(|d| format!("arn:aws:s3:::{d}")),
            "s3Operation": "OBJECT_PUT",
            "requestTime": iso8601_ms(r.time),
            "failureReason": r.reason,
        });
    }
    json!({ "Records": [rec] })
}

/// EventBridge's `detail-type` for an event.
fn bridge_detail_type(name: &str) -> &'static str {
    match name.split(':').next().unwrap_or_default() {
        "ObjectCreated" => "Object Created",
        "ObjectRemoved" => "Object Deleted",
        "ObjectRestore" => "Object Restore Completed",
        "ObjectTagging" if name.ends_with("Delete") => "Object Tags Deleted",
        "ObjectTagging" => "Object Tags Added",
        "ObjectAcl" => "Object ACL Updated",
        "LifecycleExpiration" => "Object Deleted",
        "Replication" => "Object Replication Operation Failed",
        _ => "Object Created",
    }
}

fn backoff(cfg: &NotifyConfig, attempts: u32) -> i64 {
    let base = cfg.backoff_base.as_millis() as i64;
    let d = base.saturating_mul(1i64 << attempts.min(30)).min(cfg.backoff_max.as_millis() as i64);
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    // Up to 20% jitter, so a burst of failures doesn't retry in lockstep.
    d + d * (u16::from_le_bytes(b) as i64 % 20) / 100
}

impl Notifier {
    pub(crate) fn new(store: Arc<Store>, cfg: NotifyConfig, region: String) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.webhook_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(GuardedResolver { allow_private: cfg.webhook_allow_private }))
            .user_agent("roll-storage-events/1")
            .build()
            .expect("an HTTP client");
        Arc::new(Self { store, cfg, region, sink: RwLock::new(None), http, aws: Default::default(), counters: Counters::default() })
    }

    /// Install the delivery into roll queues and streams (rolld does, once they're up).
    pub fn set_sink(&self, sink: Arc<dyn EventSink>) {
        *self.sink.write().unwrap() = Some(sink);
    }

    fn sink(&self) -> Option<Arc<dyn EventSink>> {
        self.sink.read().unwrap().clone()
    }

    pub fn stats(&self) -> EventStats {
        let c = &self.counters;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        EventStats { delivered: l(&c.delivered), retried: l(&c.retried), dropped: l(&c.dropped), replicated: l(&c.replicated), replication_failed: l(&c.replication_failed) }
    }

    pub(crate) fn spawn(self: &Arc<Self>) {
        let (Some(every), Ok(rt)) = (self.cfg.dispatch_interval, tokio::runtime::Handle::try_current()) else { return };
        let weak = Arc::downgrade(self);
        rt.spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(n) = weak.upgrade() else { return };
                // Drain while there's work, then wait for the next tick.
                loop {
                    match n.dispatch().await {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(StoreError::Unavailable(_)) => return,
                        Err(e) => {
                            tracing::warn!(error = %e, "dispatching storage events failed");
                            break;
                        }
                    }
                }
            }
        });
    }

    /// One pass over the due outbox rows: deliver notifications, apply replication. Returns how
    /// many rows were looked at.
    pub async fn dispatch(&self) -> Result<usize, StoreError> {
        let rows = self.store.due_events(256).await?;
        let n = rows.len();
        if n == 0 {
            return Ok(0);
        }
        let results: Vec<(PendingEvent, Result<bool, Failure>)> = futures::stream::iter(rows)
            .map(|ev| async move {
                let r = self.handle(&ev).await;
                (ev, r)
            })
            .buffer_unordered(32)
            .collect()
            .await;
        let now = self.store.now();
        let retention = self.cfg.retention.as_millis() as i64;
        let mut done = Vec::new();
        for (ev, r) in results {
            match r {
                // Delivered (to be deleted), or handled by the store itself.
                Ok(remove) => {
                    if remove {
                        done.push(ev.seq);
                    }
                }
                Err(Failure::Permanent(why)) => {
                    self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(seq = ev.seq, dest = ev.dest, key = ev.key, error = why, "storage event can't be delivered; dropped");
                    done.push(ev.seq);
                }
                Err(Failure::Retry(why)) if now - ev.created_at > retention => {
                    self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(seq = ev.seq, dest = ev.dest, key = ev.key, attempts = ev.attempts, error = why, "storage event failed past its retention; dropped");
                    if ev.kind == REPLICATE {
                        self.store.abandon_replication(&ev, &why).await?;
                    } else {
                        done.push(ev.seq);
                    }
                }
                Err(Failure::Retry(why)) => {
                    self.counters.retried.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(seq = ev.seq, dest = ev.dest, error = why, "storage event delivery failed; retrying");
                    self.store.event_retry(ev.seq, now + backoff(&self.cfg, ev.attempts), &why)?;
                }
            }
        }
        self.store.events_done(&done)?;
        Ok(n)
    }

    /// Handle one row. `Ok(true)`: delivered, delete it; `Ok(false)`: the store already did.
    async fn handle(&self, ev: &PendingEvent) -> Result<bool, Failure> {
        match ev.kind.as_str() {
            REPLICATE => match self.store.replicate(ev).await {
                Ok(Replicated::Failed(why)) => {
                    self.counters.replication_failed.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(bucket = ev.bucket_id, key = ev.key, why, "replication failed");
                    Ok(false)
                }
                Ok(Replicated::Completed) => {
                    self.counters.replicated.fetch_add(1, Ordering::Relaxed);
                    Ok(false)
                }
                Ok(Replicated::Skipped) => Ok(false),
                Err(e) => Err(Failure::Retry(e.to_string())),
            },
            BACKFILL => self.store.backfill(ev).await.map(|_| false).map_err(|e| Failure::Retry(e.to_string())),
            _ => {
                let rec = ev.record().map_err(|e| Failure::Permanent(format!("unreadable event: {e}")))?;
                let dest = Destination::parse(&ev.dest).map_err(Failure::Permanent)?;
                self.deliver(&dest, &rec, &ev.bucket_id, &format!("{}-{}", rec.sequencer, ev.seq)).await?;
                self.counters.delivered.fetch_add(1, Ordering::Relaxed);
                Ok(true)
            }
        }
    }

    async fn deliver(&self, dest: &Destination, rec: &EventRecord, bucket_id: &str, id: &str) -> Result<(), Failure> {
        let owner = self.store.owner_id();
        let body = serde_json::to_vec(&event_json(rec, &self.region, &owner)).map_err(|e| Failure::Permanent(e.to_string()))?;
        let d = Delivery { id: id.to_string(), event_name: format!("s3:{}", rec.name), bucket: rec.bucket.clone(), key: rec.key.clone(), body: Bytes::from(body) };
        match dest {
            Destination::Roll(r) => {
                let sink = self.sink().ok_or_else(|| Failure::Retry("roll queues and streams aren't connected to storage".into()))?;
                sink.deliver(r, &d).await.map_err(Failure::Retry)
            }
            Destination::Webhook(url) => {
                // Never unsigned: a bucket deleted since has no secret, so its events can't go.
                let secret = self.store.webhook_secret_of(bucket_id).map_err(|e| Failure::Retry(e.to_string()))?.ok_or_else(|| Failure::Permanent("the bucket's webhook secret is gone (the bucket was deleted)".into()))?;
                self.webhook(url, &secret, &d).await.map_err(Failure::Retry)
            }
            Destination::EventBridge => {
                let detail = bridge_detail(rec, &owner);
                let entry = json!({ "Source": "roll.s3", "DetailType": bridge_detail_type(&rec.name), "Detail": detail.to_string(), "Resources": [format!("arn:aws:s3:::{}", rec.bucket)], "EventBusName": "default" });
                let body = json!({ "Entries": [entry] }).to_string();
                let region = self.region.clone();
                self.aws_call("events", &region, "/", Some("AWSEvents.PutEvents"), "application/x-amz-json-1.1", body.into_bytes(), &[]).await
            }
            Destination::Sqs { region, account, name } => {
                let queue_url = format!("https://sqs.{region}.amazonaws.com/{account}/{name}");
                let body = json!({ "QueueUrl": queue_url, "MessageBody": String::from_utf8_lossy(&d.body) }).to_string();
                self.aws_call("sqs", region, "/", Some("AmazonSQS.SendMessage"), "application/x-amz-json-1.0", body.into_bytes(), &[]).await
            }
            Destination::Sns { region, arn } => {
                let form = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("Action", "Publish")
                    .append_pair("Version", "2010-03-31")
                    .append_pair("TopicArn", arn)
                    .append_pair("Subject", "Amazon S3 Notification")
                    .append_pair("Message", &String::from_utf8_lossy(&d.body))
                    .finish();
                self.aws_call("sns", region, "/", None, "application/x-www-form-urlencoded", form.into_bytes(), &[]).await
            }
            Destination::Lambda { region, arn } => {
                let path = format!("/2015-03-31/functions/{}/invocations", s3_proto::http::uri_encode(arn, true));
                self.aws_call("lambda", region, &path, None, "application/json", d.body.to_vec(), &[("x-amz-invocation-type", "Event")]).await
            }
        }
    }

    async fn webhook(&self, url: &str, secret: &str, d: &Delivery) -> Result<(), String> {
        let u = url::Url::parse(url).map_err(|e| e.to_string())?;
        if !self.cfg.webhook_allow_private
            && let Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) = u.host()
            && let Some(ip) = u.host_str().and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            && !public_ip(ip)
        {
            return Err(format!("{ip} is inside the network; webhooks can't reach it"));
        }
        let now_s = self.store.now().div_euclid(1000);
        let req = self
            .http
            .post(u)
            .header("content-type", "application/json")
            .header("x-roll-event", &d.event_name)
            .header("x-roll-delivery", &d.id)
            .header("x-roll-signature", webhook_signature(secret, now_s, &d.body))
            .body(d.body.clone());
        let resp = req.send().await.map_err(|e| format!("webhook request failed: {e}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("webhook answered {}", resp.status()))
        }
    }

    async fn credentials(&self) -> Option<aws_sdk_s3::config::Credentials> {
        use aws_sdk_s3::config::ProvideCredentials;
        let p = self
            .aws
            .get_or_init(|| async {
                let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
                cfg.credentials_provider()
            })
            .await
            .clone()?;
        p.provide_credentials().await.ok()
    }

    /// A SigV4-signed POST to an AWS service.
    #[allow(clippy::too_many_arguments)]
    async fn aws_call(&self, service: &str, region: &str, path: &str, target: Option<&str>, content_type: &str, body: Vec<u8>, extra: &[(&str, &str)]) -> Result<(), Failure> {
        if !self.cfg.aws {
            return Err(Failure::Permanent("delivery to AWS isn't enabled on this deployment (ROLL_S3_NOTIFY_AWS)".into()));
        }
        let creds = self.credentials().await.ok_or_else(|| Failure::Retry("no AWS credentials".into()))?;
        let host = format!("{service}.{region}.amazonaws.com");
        let mut headers: Vec<(String, String)> = vec![("content-type".into(), content_type.into()), ("host".into(), host.clone()), ("x-amz-date".into(), amz_date(self.store.now().div_euclid(1000)))];
        if let Some(t) = target {
            headers.push(("x-amz-target".into(), t.into()));
        }
        if let Some(tok) = creds.session_token() {
            headers.push(("x-amz-security-token".into(), tok.into()));
        }
        headers.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let auth = sigv4("POST", path, "", &headers, &body, creds.access_key_id(), creds.secret_access_key(), region, service);
        let mut req = self.http.post(format!("https://{host}{path}")).header("authorization", auth).body(body);
        for (k, v) in &headers {
            if k != "host" {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        let resp = req.send().await.map_err(|e| Failure::Retry(format!("{service}: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        Err(Failure::Retry(format!("{service} answered {status}: {}", text.chars().take(300).collect::<String>())))
    }

    /// Check a configuration and its destinations the way S3 does (unless `skip_validation`),
    /// sending each destination an `s3:TestEvent`, then store it. Returns it as stored.
    pub async fn put_config(&self, bucket: &Bucket, mut cfg: NotificationConfig, skip_validation: bool) -> Result<NotificationConfig, NotifyError> {
        for t in &mut cfg.configurations {
            if t.kind.is_empty() {
                t.kind = Destination::default_kind(&t.destination).to_string();
            }
        }
        cfg.validate().map_err(NotifyError::Invalid)?;
        let mut dests: Vec<(String, Destination)> = Vec::new();
        let mut bad = Vec::new();
        for t in &cfg.configurations {
            match Destination::parse(&t.destination) {
                Ok(d) => {
                    if !dests.iter().any(|(a, _)| *a == t.destination) {
                        dests.push((t.destination.clone(), d));
                    }
                }
                Err(why) => bad.push((t.destination.clone(), why)),
            }
        }
        if cfg.event_bridge {
            dests.push(("EventBridgeConfiguration".into(), Destination::EventBridge));
        }
        if !bad.is_empty() {
            return Err(NotifyError::Destinations(bad));
        }
        if dests.iter().any(|(_, d)| matches!(d, Destination::Webhook(_))) {
            self.store.webhook_secret(&bucket.name, false).await?;
        }
        if !skip_validation {
            let test = EventRecord { name: "TestEvent".into(), time: self.store.now(), bucket: bucket.name.clone(), sequencer: s3_store::events::sequencer(0), ..Default::default() };
            let checks = dests.iter().map(|(arn, d)| {
                let test = &test;
                async move { (arn.clone(), self.validate(d, test, &bucket.id).await) }
            });
            let failed: Vec<(String, String)> = futures::future::join_all(checks).await.into_iter().filter_map(|(arn, r)| r.err().map(|e| (arn, e))).collect();
            if !failed.is_empty() {
                return Err(NotifyError::Destinations(failed));
            }
        }
        self.store.set_notifications(&bucket.name, Some(&cfg)).await?;
        Ok(cfg)
    }

    /// S3's destination validation: the destination exists and takes a test event.
    async fn validate(&self, d: &Destination, test: &EventRecord, bucket_id: &str) -> Result<(), String> {
        match d {
            Destination::Roll(r) => {
                let sink = self.sink().ok_or("roll queues and streams aren't available to storage on this deployment")?;
                sink.check(r).await?;
            }
            Destination::Webhook(_) => {}
            Destination::EventBridge if !self.cfg.aws => return Err("EventBridge isn't available: delivery to AWS isn't enabled on this deployment".into()),
            Destination::EventBridge => return Ok(()),
            _ if !self.cfg.aws => return Err("Permissions on the destination do not allow S3 to publish notifications from this bucket: delivery to AWS isn't enabled on this deployment".into()),
            _ => {}
        }
        match self.deliver(d, test, bucket_id, "s3-test-event").await {
            Ok(()) => Ok(()),
            Err(Failure::Retry(e) | Failure::Permanent(e)) => Err(e),
        }
    }
}

/// The `detail` of an EventBridge event (S3's shape).
fn bridge_detail(r: &EventRecord, owner: &str) -> Value {
    let mut object = json!({ "key": r.key, "sequencer": r.sequencer });
    if let Some(s) = r.size {
        object["size"] = json!(s);
    }
    if let Some(e) = &r.etag {
        object["etag"] = json!(e);
    }
    if let Some(v) = &r.version_id {
        object["version-id"] = json!(v);
    }
    json!({
        "version": "0",
        "bucket": { "name": r.bucket },
        "object": object,
        "request-id": r.request_id,
        "requester": r.principal.as_deref().unwrap_or(owner),
        "source-ip-address": r.source_ip,
        "reason": r.name,
    })
}

/// SigV4 `Authorization` for a request to an AWS service (every header given is signed).
#[allow(clippy::too_many_arguments)]
pub(crate) fn sigv4(method: &str, path: &str, query: &str, headers: &[(String, String)], body: &[u8], access_key: &str, secret: &str, region: &str, service: &str) -> String {
    let mut hs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())).collect();
    hs.sort();
    let amz_date = hs.iter().find(|(k, _)| k == "x-amz-date").map(|(_, v)| v.clone()).unwrap_or_default();
    let date = amz_date.get(..8).unwrap_or_default().to_string();
    let canonical_headers: String = hs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: Vec<&str> = hs.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let canonical = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{}", hex::encode(Sha256::digest(body)));
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let sig = hex::encode(hmac(&k, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={sig}")
}

impl Server {
    /// Deliver due events now (the dispatcher also runs every `notify.dispatch_interval`).
    /// Returns how many outbox rows were handled.
    pub async fn dispatch_events(&self) -> Result<usize, StoreError> {
        let mut total = 0;
        loop {
            let n = self.notifier.dispatch().await?;
            if n == 0 {
                return Ok(total);
            }
            total += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arns() {
        assert_eq!(Destination::parse("arn:roll:queues:/:orders").unwrap(), Destination::Roll(RollDestination::Queue { vhost: "/".into(), queue: "orders".into() }));
        assert_eq!(Destination::parse("arn:roll:queues:%2F:a:b").unwrap(), Destination::Roll(RollDestination::Queue { vhost: "/".into(), queue: "a:b".into() }));
        assert_eq!(Destination::parse("arn:roll:streams:main:events").unwrap(), Destination::Roll(RollDestination::Stream { stream: "main".into(), topic: "events".into() }));
        assert_eq!(Destination::parse("arn:roll:webhook:https://example.com/h?a=1").unwrap(), Destination::Webhook("https://example.com/h?a=1".into()));
        assert!(Destination::parse("arn:roll:webhook:ftp://x").is_err());
        assert!(Destination::parse("arn:roll:webhook:https://u:p@x/").is_err());
        assert_eq!(Destination::parse("arn:aws:sqs:us-east-1:123456789012:q").unwrap(), Destination::Sqs { region: "us-east-1".into(), account: "123456789012".into(), name: "q".into() });
        assert!(matches!(Destination::parse("arn:aws:sns:eu-west-1:1:t").unwrap(), Destination::Sns { .. }));
        assert!(matches!(Destination::parse("arn:aws:lambda:eu-west-1:1:function:f").unwrap(), Destination::Lambda { .. }));
        assert!(Destination::parse("arn:aws:s3:::b").is_err());
        assert!(Destination::parse("nonsense").is_err());
        assert_eq!(Destination::default_kind("arn:roll:queues:/:q"), "queue");
        assert_eq!(Destination::default_kind("arn:roll:webhook:https://x/"), "topic");
    }

    #[test]
    fn keys_are_form_encoded() {
        assert_eq!(event_key("red flower.jpg"), "red+flower.jpg");
        assert_eq!(event_key("a/b c/ü+=.txt"), "a/b+c/%C3%BC%2B%3D.txt");
    }

    #[test]
    fn addresses() {
        for ip in ["127.0.0.1", "10.0.0.1", "169.254.169.254", "192.168.1.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "100.64.0.1", "0.0.0.0"] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2606:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    /// AWS's SigV4 test suite, `get-vanilla`.
    #[test]
    fn sigv4_vector() {
        let headers = vec![("Host".to_string(), "example.amazonaws.com".to_string()), ("X-Amz-Date".to_string(), "20150830T123600Z".to_string())];
        let auth = sigv4("GET", "/", "", &headers, b"", "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "us-east-1", "service");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn signatures() {
        let s = webhook_signature("secret", 1_700_000_000, b"{}");
        let (t, v1) = s.split_once(',').unwrap();
        assert_eq!(t, "t=1700000000");
        assert_eq!(v1.len(), 3 + 64);
        assert_ne!(s, webhook_signature("other", 1_700_000_000, b"{}"));
    }

    #[test]
    fn event_records() {
        let r = EventRecord {
            name: "ObjectCreated:Put".into(),
            time: 0,
            bucket: "b".into(),
            key: "a b".into(),
            size: Some(3),
            etag: Some("e".into()),
            version_id: Some("v".into()),
            sequencer: "000000000000000001".into(),
            config_id: "c".into(),
            principal: Some("RK1".into()),
            source_ip: Some("1.2.3.4".into()),
            request_id: Some("R".into()),
            ..Default::default()
        };
        let v = event_json(&r, "us-east-1", "own");
        let rec = &v["Records"][0];
        assert_eq!(rec["eventVersion"], "2.1");
        assert_eq!(rec["eventSource"], "aws:s3");
        assert_eq!(rec["eventTime"], "1970-01-01T00:00:00.000Z");
        assert_eq!(rec["s3"]["object"]["key"], "a+b");
        assert_eq!(rec["s3"]["object"]["versionId"], "v");
        assert_eq!(rec["s3"]["bucket"]["arn"], "arn:aws:s3:::b");
        assert_eq!(rec["s3"]["bucket"]["ownerIdentity"]["principalId"], "own");
        assert_eq!(rec["responseElements"]["x-amz-request-id"], "R");
        let t = event_json(&EventRecord { name: "ObjectTagging:Put".into(), ..r.clone() }, "r", "o");
        assert_eq!(t["Records"][0]["eventVersion"], "2.3");
        let test = event_json(&EventRecord { name: "TestEvent".into(), ..r }, "r", "o");
        assert_eq!(test["Event"], "s3:TestEvent");
        assert_eq!(test["Service"], "Amazon S3");
    }
}

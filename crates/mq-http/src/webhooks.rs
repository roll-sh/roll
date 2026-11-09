//! Webhooks: a queue consumer that POSTs each message to a URL. 2xx acknowledges; anything
//! else is retried with exponential backoff and jitter, then rejected (dead-lettered if the
//! queue has a dead-letter exchange). Definitions live in the metadata store (kind `webhook`)
//! and restart with the broker. Every node runs every webhook (competing consumers) and
//! follows changes made on other nodes through the cluster change feed.
//!
//! Requests are signed: `Roll-Signature: t=<unix>,v1=<hex hmac_sha256(secret, "<t>.<body>")>`.

use crate::HttpApi;
use crate::auth::{Auth, Op, P, parse_body};
use crate::error::{ApiError, ApiResult};
use crate::message;
use crate::push::Event;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use bytes::Bytes;
use hmac::{Hmac, Mac};
use mq_core::auth::Access;
use mq_core::broker::Broker;
use mq_core::meta::{self, kind};
use mq_core::queue::{ChannelId, ConsumerSink, ConsumerSpec, Delivery, Outcome, QueueRef};
use mq_core::value::FieldTable;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    #[serde(default = "d_attempts")]
    pub max_attempts: u32,
    #[serde(default = "d_backoff")]
    pub backoff_ms: u64,
    #[serde(default = "d_max_backoff")]
    pub max_backoff_ms: u64,
}

impl Default for Retry {
    fn default() -> Self {
        Retry { max_attempts: d_attempts(), backoff_ms: d_backoff(), max_backoff_ms: d_max_backoff() }
    }
}

fn d_attempts() -> u32 {
    5
}
fn d_backoff() -> u64 {
    1000
}
fn d_max_backoff() -> u64 {
    60_000
}
fn d_in_flight() -> u32 {
    10
}
fn d_timeout() -> u64 {
    10_000
}

/// A webhook definition as clients send it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub queue: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default = "d_in_flight")]
    pub max_in_flight: u32,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub retry: Retry,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl Spec {
    fn validate(&self) -> Result<(), String> {
        let url = reqwest::Url::parse(&self.url).map_err(|e| format!("invalid url: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("url must be http or https".into());
        }
        if self.queue.is_empty() || self.queue.len() > 255 {
            return Err("queue names are 1-255 bytes".into());
        }
        if !(1..=1000).contains(&self.max_in_flight) {
            return Err("max_in_flight must be between 1 and 1000".into());
        }
        if !(100..=300_000).contains(&self.timeout_ms) {
            return Err("timeout_ms must be between 100 and 300000".into());
        }
        let r = &self.retry;
        if !(1..=100).contains(&r.max_attempts) {
            return Err("retry.max_attempts must be between 1 and 100".into());
        }
        if r.max_backoff_ms < r.backoff_ms || r.max_backoff_ms > 24 * 3600 * 1000 {
            return Err("retry.max_backoff_ms must be at least backoff_ms and at most a day".into());
        }
        if self.secret.as_ref().is_some_and(|s| s.is_empty()) {
            return Err("secret must not be empty".into());
        }
        for (k, v) in &self.headers {
            HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("invalid header name '{k}'"))?;
            HeaderValue::from_str(v).map_err(|_| format!("invalid value for header '{k}'"))?;
            if k.to_ascii_lowercase().starts_with("roll-") || k.eq_ignore_ascii_case("content-type") {
                return Err(format!("header '{k}' is set by roll"));
            }
        }
        Ok(())
    }
}

/// A persisted webhook.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Stored {
    vhost: String,
    name: String,
    owner: String,
    created_at: u64,
    updated_at: u64,
    spec: Spec,
}

#[derive(Default)]
struct Stats {
    delivered: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
    in_flight: AtomicU32,
    last_success_ms: AtomicU64,
    last_error: Mutex<Option<(String, u64)>>,
    state: Mutex<&'static str>,
}

/// A running webhook.
pub struct Runner {
    stored: Stored,
    stats: Stats,
    stop: watch::Sender<bool>,
}

impl Runner {
    fn view(&self, with_secret: bool) -> Value {
        let s = &self.stored;
        let mut spec = s.spec.clone();
        if !with_secret {
            spec.secret = None;
        }
        let mut v = serde_json::to_value(&spec).unwrap_or(Value::Null);
        v["name"] = s.name.clone().into();
        v["vhost"] = s.vhost.clone().into();
        v["owner"] = s.owner.clone().into();
        v["created_at"] = s.created_at.into();
        v["updated_at"] = s.updated_at.into();
        let st = &self.stats;
        let last_error = st.last_error.lock().clone();
        v["status"] = json!({
            "state": *st.state.lock(),
            "delivered": st.delivered.load(Ordering::Relaxed),
            "failed": st.failed.load(Ordering::Relaxed),
            "retries": st.retries.load(Ordering::Relaxed),
            "in_flight": st.in_flight.load(Ordering::Relaxed),
            "last_success_at_ms": Some(st.last_success_ms.load(Ordering::Relaxed)).filter(|v| *v > 0),
            "last_error": last_error.as_ref().map(|(e, _)| e.clone()),
            "last_error_at_ms": last_error.as_ref().map(|(_, t)| *t),
        });
        v
    }

    fn set_state(&self, s: &'static str) {
        *self.stats.state.lock() = s;
    }
}

fn key(vhost: &str, name: &str) -> String {
    meta::key(&[vhost, name])
}

fn start(api: &HttpApi, stored: Stored) -> Arc<Runner> {
    let runner = Arc::new(Runner { stored, stats: Stats::default(), stop: watch::channel(false).0 });
    runner.set_state("starting");
    let k = key(&runner.stored.vhost, &runner.stored.name);
    if let Some(old) = api.webhooks.lock().insert(k, runner.clone()) {
        old.stop.send_replace(true);
    }
    let ctx = Ctx { broker: api.broker.clone(), http: api.http.clone(), runner: runner.clone(), api_stop: api.stop_rx(), stop: runner.stop.subscribe() };
    // A webhook moves messages with no client connected: the broker stays out of quiet mode.
    let awake = api.broker.awake_guard();
    tokio::spawn(async move {
        let _awake = awake;
        run(ctx).await
    });
    runner
}

/// Start every persisted webhook (on boot).
pub async fn restore(api: &HttpApi) {
    let rows = match api.broker.meta.list(kind::WEBHOOK).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "failed to load webhooks");
            return;
        }
    };
    for (k, v) in rows {
        match serde_json::from_value::<Stored>(v) {
            Ok(s) => {
                start(api, s);
            }
            Err(e) => warn!(key = %k, error = %e, "skipping unreadable webhook"),
        }
    }
    let n = api.webhooks.lock().len();
    if n > 0 {
        info!(webhooks = n, "webhooks started");
    }
}

/// Bring one webhook in line with the store after a change (possibly made on another node).
/// A definition this node already runs is left alone, so its own writes don't restart it.
pub async fn sync(api: &HttpApi, k: &str) {
    let current = match api.broker.meta.get(kind::WEBHOOK, k).await {
        Ok(v) => v.and_then(|v| serde_json::from_value::<Stored>(v).ok()),
        Err(e) => {
            warn!(key = %k, error = %e, "failed to read webhook");
            return;
        }
    };
    match current {
        Some(s) => {
            if api.webhooks.lock().get(k).is_none_or(|r| r.stored != s) {
                debug!(webhook = %s.name, vhost = %s.vhost, "webhook changed elsewhere; restarting");
                start(api, s);
            }
        }
        None => {
            if let Some(r) = api.webhooks.lock().remove(k) {
                debug!(webhook = %r.stored.name, vhost = %r.stored.vhost, "webhook deleted elsewhere; stopping");
                r.stop.send_replace(true);
            }
        }
    }
}

/// Reconcile every webhook (after missing change events).
pub async fn sync_all(api: &HttpApi) {
    let Ok(rows) = api.broker.meta.list(kind::WEBHOOK).await else { return };
    let mut keys: std::collections::HashSet<String> = rows.into_iter().map(|(k, _)| k).collect();
    keys.extend(api.webhooks.lock().keys().cloned());
    for k in keys {
        sync(api, &k).await;
    }
}

#[derive(Clone)]
struct Ctx {
    broker: Arc<Broker>,
    http: reqwest::Client,
    runner: Arc<Runner>,
    api_stop: watch::Receiver<bool>,
    stop: watch::Receiver<bool>,
}

impl Ctx {
    fn stopped(&self) -> bool {
        *self.stop.borrow() || *self.api_stop.borrow()
    }

    async fn until_stopped(&mut self) {
        tokio::select! {
            _ = self.stop.wait_for(|s| *s) => {}
            _ = self.api_stop.wait_for(|s| *s) => {}
        }
    }

    /// Sleep unless stopped first; true if stopped.
    async fn pause(&mut self, d: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(d) => self.stopped(),
            _ = self.until_stopped() => true,
        }
    }
}

struct HookSink {
    tx: mpsc::UnboundedSender<Event>,
    runner: Arc<Runner>,
    closed: AtomicBool,
}

impl ConsumerSink for HookSink {
    fn has_credit(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.runner.stats.in_flight.load(Ordering::Acquire) < self.runner.stored.spec.max_in_flight
    }

    fn deliver(&self, d: Delivery) {
        self.runner.stats.in_flight.fetch_add(1, Ordering::AcqRel);
        let _ = self.tx.send(Event::Deliver(d));
    }

    fn cancelled(&self, _tag: &str) {
        let _ = self.tx.send(Event::Cancelled);
    }
}

async fn run(mut ctx: Ctx) {
    let r = ctx.runner.clone();
    let (vhost, queue) = (r.stored.vhost.clone(), r.stored.spec.queue.clone());
    while !ctx.stopped() {
        let Some(vh) = ctx.broker.open_vhost(&vhost).await else {
            r.set_state("waiting_for_vhost");
            if ctx.pause(Duration::from_secs(5)).await {
                break;
            }
            continue;
        };
        let allowed = ctx.broker.user(&r.stored.owner).is_some_and(|u| ctx.broker.check(&u, &vhost, Access::Read, "queue", &queue).is_ok());
        if !allowed {
            r.set_state("unauthorized");
            if ctx.pause(Duration::from_secs(5)).await {
                break;
            }
            continue;
        }
        let Some(q) = vh.queue(&queue).filter(|q| !q.is_deleted() && q.meta.exclusive_owner.is_none()) else {
            r.set_state("waiting_for_queue");
            if ctx.pause(Duration::from_secs(1)).await {
                break;
            }
            continue;
        };
        let channel = ctx.broker.next_id();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = Arc::new(HookSink { tx, runner: r.clone(), closed: AtomicBool::new(false) });
        let spec = ConsumerSpec {
            tag: Arc::from(format!("webhook.{}", r.stored.name).as_str()),
            channel,
            conn: 0,
            no_ack: false,
            exclusive: false,
            priority: 0,
            args: FieldTable::new(),
            sink: sink.clone(),
        };
        if let Err(e) = q.consume(spec).await {
            r.set_state("waiting_for_queue");
            *r.stats.last_error.lock() = Some((e.reply_text(), mq_core::now_ms()));
            if ctx.pause(Duration::from_secs(2)).await {
                break;
            }
            continue;
        }
        r.set_state("running");
        debug!(webhook = %r.stored.name, %queue, "webhook consuming");
        q.wake();
        let mut tasks = JoinSet::new();
        let stopped = loop {
            tokio::select! {
                e = rx.recv() => match e {
                    Some(Event::Deliver(d)) => {
                        tasks.spawn(deliver(ctx.clone(), q.clone(), channel, d));
                    }
                    Some(Event::TimedOut(_)) => {}
                    Some(Event::Cancelled) | None => break false,
                },
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                _ = ctx.until_stopped() => break true,
            }
        };
        sink.closed.store(true, Ordering::Release);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        q.channel_closed(channel).await;
        r.stats.in_flight.store(0, Ordering::Release);
        if stopped {
            break;
        }
        r.set_state("waiting_for_queue");
    }
    r.set_state("stopped");
}

fn sign(secret: &str, t: u64, body: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    m.update(format!("{t}.{body}").as_bytes());
    hex::encode(m.finalize().into_bytes())
}

fn backoff(retry: &Retry, attempt: u32) -> Duration {
    let exp = retry.backoff_ms.saturating_mul(1u64 << (attempt.saturating_sub(1)).min(30)).min(retry.max_backoff_ms);
    // Equal jitter: half fixed, half random.
    let half = exp / 2;
    Duration::from_millis(half + mq_core::rand_u64() % (exp - half + 1))
}

async fn deliver(mut ctx: Ctx, q: QueueRef, channel: ChannelId, d: Delivery) {
    let r = ctx.runner.clone();
    let spec = &r.stored.spec;
    let mut m = message::render(q.kind, &q.meta.name, d.seq, &d.msg, d.redelivered, d.delivery_count);
    m.insert("vhost".into(), r.stored.vhost.clone().into());
    m.insert("webhook".into(), r.stored.name.clone().into());
    let body = Value::Object(m).to_string();
    let message_id = d.msg.props.message_id.clone().unwrap_or_else(|| format!("{}:{}", q.meta.name, d.seq));
    let secret = spec.secret.clone().unwrap_or_default();
    let mut outcome = Outcome::Reject;
    for attempt in 1..=spec.retry.max_attempts {
        if ctx.stopped() {
            // Left unsettled: closing the consumer returns it to the queue.
            return;
        }
        let t = mq_core::now_ms() / 1000;
        let mut req = ctx
            .http
            .post(&spec.url)
            .timeout(Duration::from_millis(spec.timeout_ms))
            .header("content-type", "application/json")
            .header("roll-signature", format!("t={t},v1={}", sign(&secret, t, &body)))
            .header("roll-webhook-id", &r.stored.name)
            .header("roll-delivery-attempt", attempt.to_string())
            .header("roll-message-id", &message_id);
        for (k, v) in &spec.headers {
            req = req.header(k, v);
        }
        let err = match req.body(body.clone()).send().await {
            Ok(resp) if resp.status().is_success() => {
                outcome = Outcome::Ack;
                break;
            }
            Ok(resp) => format!("HTTP {}", resp.status()),
            Err(e) if e.is_timeout() => format!("timed out after {} ms", spec.timeout_ms),
            Err(e) => format!("request failed: {}", e.without_url()),
        };
        *r.stats.last_error.lock() = Some((err, mq_core::now_ms()));
        if attempt == spec.retry.max_attempts {
            break;
        }
        r.stats.retries.fetch_add(1, Ordering::Relaxed);
        if ctx.pause(backoff(&spec.retry, attempt)).await {
            return;
        }
    }
    match outcome {
        Outcome::Ack => {
            r.stats.delivered.fetch_add(1, Ordering::Relaxed);
            r.stats.last_success_ms.store(mq_core::now_ms(), Ordering::Relaxed);
            if let Some(vh) = ctx.broker.vhost(&r.stored.vhost) {
                vh.stats.acked.fetch_add(1, Ordering::Relaxed);
            }
        }
        _ => {
            r.stats.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
    q.settle(channel, vec![d.seq], outcome).await;
    r.stats.in_flight.fetch_sub(1, Ordering::AcqRel);
    q.wake();
}

// ── endpoints ──

async fn authorize(api: &HttpApi, auth: &Auth, vhost: &str, queue: &str) -> ApiResult<()> {
    api.vhost(auth, vhost).await?;
    api.allow(auth, vhost, Op::Manage, "queue", queue, None)?;
    api.allow(auth, vhost, Op::Consume, "queue", queue, None)
}

fn runner(api: &HttpApi, vhost: &str, name: &str) -> ApiResult<Arc<Runner>> {
    api.webhooks.lock().get(&key(vhost, name)).cloned().ok_or_else(|| ApiError::not_found(format!("no webhook '{name}' in vhost '{vhost}'")))
}

/// `PUT /v1/vhosts/{vhost}/webhooks/{name}` — create or replace (201 when created). The
/// response is the only one that shows the signing secret.
pub async fn put(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, name)): P<(String, String)>, body: Bytes) -> ApiResult<Response> {
    let mut spec: Spec = parse_body(&body)?;
    if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request("webhook names are 1-255 printable characters"));
    }
    spec.validate().map_err(ApiError::bad_request)?;
    authorize(&api, &auth, &vhost, &spec.queue).await?;
    let k = key(&vhost, &name);
    let previous: Option<Stored> = api.broker.meta.get(kind::WEBHOOK, &k).await?.and_then(|v| serde_json::from_value(v).ok());
    if let Some(p) = &previous {
        api.allow(&auth, &vhost, Op::Manage, "queue", &p.spec.queue, None)?;
    }
    if spec.secret.is_none() {
        spec.secret = Some(previous.as_ref().and_then(|p| p.spec.secret.clone()).unwrap_or_else(|| {
            let b: Vec<u8> = (0..3).flat_map(|_| mq_core::rand_u64().to_le_bytes()).collect();
            format!("whsec_{}", B64URL.encode(b))
        }));
    }
    let now = mq_core::now_ms() / 1000;
    let stored = Stored { vhost, name, owner: auth.user.name.clone(), created_at: previous.as_ref().map(|p| p.created_at).unwrap_or(now), updated_at: now, spec };
    api.broker.meta.put(kind::WEBHOOK, &k, &serde_json::to_value(&stored).map_err(ApiError::internal)?).await?;
    let r = start(&api, stored);
    let status = if previous.is_some() { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(r.view(true))).into_response())
}

/// `GET /v1/vhosts/{vhost}/webhooks/{name}` — definition and delivery status.
pub async fn get_one(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, name)): P<(String, String)>) -> ApiResult<Json<Value>> {
    api.vhost(&auth, &vhost).await?;
    let r = runner(&api, &vhost, &name)?;
    api.allow(&auth, &vhost, Op::Consume, "queue", &r.stored.spec.queue, None)?;
    Ok(Json(r.view(false)))
}

/// `GET /v1/vhosts/{vhost}/webhooks` — the webhooks on queues the caller can read.
pub async fn list(State(api): State<Arc<HttpApi>>, auth: Auth, P(vhost): P<String>) -> ApiResult<Json<Value>> {
    api.vhost(&auth, &vhost).await?;
    let mut all: Vec<Arc<Runner>> = api.webhooks.lock().values().filter(|r| r.stored.vhost == vhost).cloned().collect();
    all.sort_by(|a, b| a.stored.name.cmp(&b.stored.name));
    let out: Vec<Value> = all.iter().filter(|r| api.allow(&auth, &vhost, Op::Consume, "queue", &r.stored.spec.queue, None).is_ok()).map(|r| r.view(false)).collect();
    Ok(Json(Value::Array(out)))
}

/// `DELETE /v1/vhosts/{vhost}/webhooks/{name}` — stops it; held messages return to the queue.
pub async fn delete(State(api): State<Arc<HttpApi>>, auth: Auth, P((vhost, name)): P<(String, String)>) -> ApiResult<StatusCode> {
    api.vhost(&auth, &vhost).await?;
    let r = runner(&api, &vhost, &name)?;
    api.allow(&auth, &vhost, Op::Manage, "queue", &r.stored.spec.queue, None)?;
    api.broker.meta.delete(kind::WEBHOOK, &key(&vhost, &name)).await?;
    if let Some(r) = api.webhooks.lock().remove(&key(&vhost, &name)) {
        r.stop.send_replace(true);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_reference() {
        // echo -n '1700000000.{"a":1}' | openssl dgst -sha256 -hmac whsec_test
        let s = sign("whsec_test", 1_700_000_000, r#"{"a":1}"#);
        let mut m = Hmac::<Sha256>::new_from_slice(b"whsec_test").unwrap();
        m.update(br#"1700000000.{"a":1}"#);
        assert_eq!(s, hex::encode(m.finalize().into_bytes()));
        assert_eq!(s.len(), 64);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let r = Retry { max_attempts: 10, backoff_ms: 100, max_backoff_ms: 1000 };
        for _ in 0..50 {
            let d1 = backoff(&r, 1).as_millis();
            assert!((50..=100).contains(&d1), "{d1}");
            let d3 = backoff(&r, 3).as_millis();
            assert!((200..=400).contains(&d3), "{d3}");
            let d9 = backoff(&r, 9).as_millis();
            assert!((500..=1000).contains(&d9), "{d9}");
        }
    }

    #[test]
    fn spec_validation() {
        let ok: Spec = serde_json::from_value(json!({"queue": "q", "url": "https://example.com/hook"})).unwrap();
        assert_eq!(ok.max_in_flight, 10);
        assert_eq!(ok.retry.max_attempts, 5);
        assert!(ok.validate().is_ok());
        let bad = |v: Value| serde_json::from_value::<Spec>(v).map_err(|e| e.to_string()).and_then(|s| s.validate());
        assert!(bad(json!({"queue": "q", "url": "ftp://x"})).is_err());
        assert!(bad(json!({"queue": "q", "url": "https://x", "max_in_flight": 0})).is_err());
        assert!(bad(json!({"queue": "q", "url": "https://x", "headers": {"Roll-Signature": "x"}})).is_err());
        assert!(bad(json!({"queue": "q", "url": "https://x", "retry": {"backoff_ms": 10, "max_backoff_ms": 5}})).is_err());
        assert!(bad(json!({"queue": "q", "url": "https://x", "unknown": 1})).is_err());
    }
}

//! The engine: publishing, message management, queues, topics, schedules, signing keys and
//! API keys. Delivery lives in `dispatch`, workflows in `workflow`.

use crate::ids::new_id;
use crate::model::*;
use crate::schedule;
use crate::store::{self, Expect, MessageQuery, Page, Store, Versioned};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Notify;

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    TooLarge(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, TaskError>;

pub(crate) fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(TaskError::Invalid(msg.into()))
}

#[derive(Clone, Debug)]
pub struct Config {
    pub node_id: String,
    /// Longest a worker waits before looking at an idle lane again.
    pub poll_ms: i64,
    /// Messages fetched per lane per poll.
    pub batch: usize,
    /// Deliveries in flight per worker.
    pub max_inflight: usize,
    /// Lease length; extended while a delivery is still running.
    pub lease_ms: i64,
    pub heartbeat_ms: i64,
    /// A worker that hasn't been seen for this long is considered gone.
    pub node_ttl_ms: i64,
    pub default_retries: u32,
    pub max_retries: u32,
    pub backoff: Backoff,
    pub default_timeout_ms: i64,
    pub max_timeout_ms: i64,
    /// Request timeout for workflow invocations.
    pub run_timeout_ms: i64,
    pub max_body_bytes: usize,
    /// Response bytes kept per attempt.
    pub response_max_bytes: usize,
    /// How long finished messages, runs and events are kept.
    pub retention_ms: i64,
    pub dedup_window_ms: i64,
    /// Longest wait between retries of a message held back by flow control.
    pub max_park_ms: i64,
    /// Refuse to deliver to loopback, private and link-local addresses (cloud metadata).
    pub block_private_urls: bool,
    /// Admin bearer token; when set, the API requires a token.
    pub token: Option<String>,
    /// How long a draining worker lets deliveries finish before handing them back.
    pub drain_ms: i64,
    /// API nodes reload queues, apps and API keys at least this often (workers also reload on
    /// every heartbeat). Bounds how long a revoked key keeps working on a warm Lambda.
    pub refresh_ms: i64,
    /// Print a CloudWatch Embedded Metric Format line per heartbeat (worker saturation), for
    /// autoscaling on ECS without an agent.
    pub emf_metrics: bool,
    /// Refuse unauthenticated calls even without an admin token: only API keys get in (hosted
    /// tenants, whose keys are minted by the platform).
    pub require_auth: bool,
    /// A tenant hosted on a shared cell (prices usage without the cell's fixed cost).
    pub hosted: bool,
    /// What carries API calls to a log store cell (prices usage).
    pub front_door: crate::cost::FrontDoor,
    /// The log's commits go to S3 Express One Zone (prices usage).
    pub log_express: bool,
    pub user_agent: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node_id: new_id("node"),
            poll_ms: 500,
            batch: 100,
            max_inflight: 1000,
            lease_ms: 60_000,
            heartbeat_ms: 5_000,
            node_ttl_ms: 20_000,
            default_retries: 3,
            max_retries: 100,
            backoff: Backoff::default(),
            default_timeout_ms: 30_000,
            max_timeout_ms: 2 * 3_600_000,
            run_timeout_ms: 300_000,
            max_body_bytes: 1 << 20,
            response_max_bytes: 8 << 10,
            retention_ms: 7 * 86_400_000,
            dedup_window_ms: 10 * 60_000,
            max_park_ms: 60_000,
            block_private_urls: false,
            token: None,
            drain_ms: 25_000,
            refresh_ms: 1_000,
            emf_metrics: false,
            require_auth: false,
            hosted: false,
            front_door: crate::cost::FrontDoor::Lambda,
            log_express: false,
            user_agent: format!("roll-tasks/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// A duration given as milliseconds or a string like `"30s"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Dur(pub i64);

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Number(n) => n.as_f64().map(|f| Dur(f as i64)).ok_or_else(|| serde::de::Error::custom("bad duration")),
            Value::String(s) => schedule::parse_duration_ms(&s).map(Dur).map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("duration must be milliseconds or a string like \"30s\"")),
        }
    }
}

/// A point in time given as unix milliseconds or RFC 3339.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Time(pub Ms);

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Number(n) => n.as_f64().map(|f| Time(f as i64)).ok_or_else(|| serde::de::Error::custom("bad time")),
            Value::String(s) => parse_time(&s).map(Time).map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("time must be unix milliseconds or RFC 3339")),
        }
    }
}

pub fn parse_time(s: &str) -> std::result::Result<Ms, String> {
    if let Ok(n) = s.trim().parse::<i64>() {
        return Ok(n);
    }
    chrono::DateTime::parse_from_rfc3339(s.trim()).map(|t| t.timestamp_millis()).map_err(|e| format!("bad time {s:?}: {e}"))
}

/// `{limit, period}` or `"10/s"`, `"100/1m"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RateSpec(pub Rate);

impl<'de> Deserialize<'de> for RateSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        parse_rate(&v).map(RateSpec).map_err(serde::de::Error::custom)
    }
}

pub fn parse_rate(v: &Value) -> std::result::Result<Rate, String> {
    let rate = match v {
        Value::String(s) => {
            let (n, per) = s.split_once('/').ok_or("rate must look like \"10/s\" or \"100/1m\"")?;
            let limit: u32 = n.trim().parse().map_err(|_| format!("bad rate limit {n:?}"))?;
            let per = per.trim();
            let per = if per.chars().next().is_some_and(|c| c.is_ascii_digit()) { per.to_string() } else { format!("1{per}") };
            Rate { limit, period_ms: schedule::parse_duration_ms(&per)? }
        }
        Value::Object(o) => {
            let limit = o.get("limit").and_then(Value::as_u64).ok_or("rate needs a limit")? as u32;
            let period_ms = match o.get("period").or_else(|| o.get("period_ms")) {
                Some(Value::Number(n)) => n.as_i64().ok_or("bad period")?,
                Some(Value::String(s)) => schedule::parse_duration_ms(s)?,
                _ => 1000,
            };
            Rate { limit, period_ms }
        }
        _ => return Err("rate must be {limit, period} or a string like \"10/s\"".into()),
    };
    if rate.limit == 0 || rate.period_ms <= 0 {
        return Err("rate limit and period must be positive".into());
    }
    Ok(rate)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowRequest {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<RateSpec>,
}

/// What to publish. Exactly one of `url` and `topic`; or only `queue` for a pull message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PublishRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// A string is sent as is; anything else as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<Dur>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<Backoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Dur>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_callback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_id: Option<String>,
    /// Deduplicate on destination + body + headers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_dedup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<FlowRequest>,
    /// FIFO key: messages in the same queue and group are delivered one at a time, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Seconds to jump ahead of (+) or fall behind (−) other due messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Published {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deduplicated: bool,
}

/// Internal message fields that callers of `enqueue` set.
pub(crate) struct NewMessage {
    pub target: Target,
    pub queue: Option<String>,
    pub due_at: Ms,
    pub retries: u32,
    pub backoff: Backoff,
    pub timeout_ms: i64,
    pub flows: Vec<Flow>,
    pub group: Option<String>,
    pub run_id: Option<String>,
    pub schedule_id: Option<String>,
    pub label: Option<String>,
}

impl NewMessage {
    pub fn internal(target: Target, due_at: Ms) -> Self {
        Self {
            target,
            queue: None,
            due_at,
            retries: 10,
            backoff: Backoff { base_ms: 1000, max_ms: 60_000, ..Backoff::default() },
            timeout_ms: 60_000,
            flows: vec![],
            group: None,
            run_id: None,
            schedule_id: None,
            label: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Cache {
    pub queues: HashMap<String, Queue>,
    pub registry: crate::workflow::Registry,
    pub keys: Option<SigningKeys>,
    pub api_keys: Vec<ApiKey>,
    pub nodes: Vec<Node>,
    /// Bumped by every change, so a refresh that read the store before a newer change landed
    /// doesn't overwrite it.
    pub generation: u64,
}

pub struct Tasks {
    pub config: Config,
    pub store: Arc<dyn Store>,
    pub(crate) http: reqwest::Client,
    pub(crate) cache: RwLock<Cache>,
    pub(crate) usage: Mutex<Usage>,
    /// Wakes the local worker: something became due.
    pub(crate) wake: Notify,
    /// Lanes that should be polled at a given time regardless of their schedule.
    pub(crate) hints: Mutex<HashMap<String, Ms>>,
    pub(crate) inflight: Mutex<HashMap<String, crate::dispatch::Inflight>>,
    pub(crate) pull_wake: Notify,
    last_refresh: AtomicI64,
    last_flush: AtomicI64,
    /// Storage writes taken from the store, waiting for other usage to be recorded with.
    storage_writes: std::sync::atomic::AtomicU64,
}

/// Longest delay; large bodies in S3 expire some weeks after it.
pub const MAX_DELAY_MS: Ms = 365 * 86_400_000;

pub const KIND_QUEUE: &str = "queue";
pub const KIND_TOPIC: &str = "topic";
pub const KIND_SCHEDULE: &str = "schedule";
pub const KIND_APP: &str = "app";
pub const KIND_KEYS: &str = "keys";
pub const KIND_API_KEY: &str = "api_key";
pub const KIND_NODE: &str = "node";

fn valid_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'));
    if ok { Ok(()) } else { invalid(format!("{kind} names use letters, digits and - _ . : / (max 128): {name:?}")) }
}

pub fn check_url(url: &str, block_private: bool) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).map_err(|e| TaskError::Invalid(format!("bad URL {url:?}: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return invalid(format!("URL must be http or https: {url:?}"));
    }
    if block_private {
        let blocked = match parsed.host() {
            Some(url::Host::Ipv4(ip)) => crate::dispatch::is_private(std::net::IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => crate::dispatch::is_private(std::net::IpAddr::V6(ip)),
            Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost") || d.ends_with(".internal"),
            None => true,
        };
        if blocked {
            return invalid(format!("URL points at a private address: {url:?}"));
        }
    }
    Ok(parsed)
}


impl Tasks {
    pub async fn new(config: Config, store: Arc<dyn Store>) -> anyhow::Result<Arc<Self>> {
        let http = crate::dispatch::client(&config)?;
        Self::with_http(config, store, http).await
    }

    /// An HTTP client for deliveries, to share between engines (one connection pool, so
    /// connections and TLS sessions to a host are reused across tenants).
    pub fn http_client(config: &Config) -> anyhow::Result<reqwest::Client> {
        crate::dispatch::client(config)
    }

    pub async fn with_http(config: Config, store: Arc<dyn Store>, http: reqwest::Client) -> anyhow::Result<Arc<Self>> {
        let tasks = Arc::new(Self {
            config,
            store,
            http,
            cache: RwLock::new(Cache::default()),
            usage: Mutex::new(Usage::default()),
            wake: Notify::new(),
            hints: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            pull_wake: Notify::new(),
            last_refresh: AtomicI64::new(0),
            last_flush: AtomicI64::new(0),
            storage_writes: Default::default(),
        });
        tasks.signing_keys().await?;
        tasks.refresh().await?;
        Ok(tasks)
    }

    pub fn now(&self) -> Ms {
        now_ms()
    }

    /// Change cached configuration after writing it to the store.
    pub(crate) fn edit_cache<R>(&self, f: impl FnOnce(&mut Cache) -> R) -> R {
        let mut cache = self.cache.write().unwrap();
        cache.generation += 1;
        f(&mut cache)
    }

    pub(crate) fn count(&self, f: impl FnOnce(&mut Usage)) {
        f(&mut self.usage.lock().unwrap());
    }

    /// Reload cached configuration (queues, functions, keys, workers).
    pub async fn refresh(&self) -> anyhow::Result<()> {
        self.last_refresh.store(now_ms(), Ordering::Relaxed);
        for _ in 0..5 {
            let generation = self.cache.read().unwrap().generation;
            let queues: Vec<Queue> = store::list_docs(&*self.store, KIND_QUEUE).await?;
            let apps: Vec<App> = store::list_docs(&*self.store, KIND_APP).await?;
            let api_keys: Vec<ApiKey> = store::list_docs(&*self.store, KIND_API_KEY).await?;
            let keys = store::get_doc::<SigningKeys>(&*self.store, KIND_KEYS, "signing").await?.map(|v| v.value);
            let mut cache = self.cache.write().unwrap();
            if cache.generation != generation {
                // Changed while we were reading: what we read may be older than the cache.
                continue;
            }
            cache.generation += 1;
            cache.queues = queues.into_iter().map(|q| (q.name.clone(), q)).collect();
            cache.registry = crate::workflow::Registry::build(apps);
            cache.api_keys = api_keys;
            if keys.is_some() {
                cache.keys = keys;
            }
            return Ok(());
        }
        Ok(())
    }

    /// Refresh if the cache is older than `max_age_ms`. Concurrent callers don't pile on: one
    /// refreshes, the rest carry on with what's cached.
    pub async fn refresh_if_older(&self, max_age_ms: i64) -> anyhow::Result<()> {
        let last = self.last_refresh.load(Ordering::Relaxed);
        let now = now_ms();
        if now - last < max_age_ms || self.last_refresh.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return Ok(());
        }
        self.refresh().await
    }

    pub(crate) fn queue_cached(&self, name: &str) -> Option<Queue> {
        self.cache.read().unwrap().queues.get(name).cloned()
    }

    /// Hint the local worker that `lane` has work due at `at`.
    pub(crate) fn nudge_lane(&self, lane: &str, at: Ms) {
        if lane.starts_with("p:") {
            self.pull_wake.notify_waiters();
            return;
        }
        let mut hints = self.hints.lock().unwrap();
        let e = hints.entry(lane.to_string()).or_insert(at);
        *e = (*e).min(at);
        drop(hints);
        if at <= now_ms() + self.config.poll_ms {
            self.wake.notify_one();
        }
    }

    // --- publishing ---

    pub async fn publish(&self, req: PublishRequest) -> Result<Vec<Published>> {
        self.publish_from(req, None).await
    }

    /// Publish on behalf of a schedule (tagged with it).
    pub(crate) async fn publish_from(&self, req: PublishRequest, schedule_id: Option<String>) -> Result<Vec<Published>> {
        let now = now_ms();
        let body = request_body(&req)?;
        if body.len() > self.config.max_body_bytes {
            return Err(TaskError::TooLarge(format!(
                "body is {} bytes; the limit is {}",
                body.len(),
                self.config.max_body_bytes
            )));
        }
        let method = req.method.clone().unwrap_or_else(|| "POST".into()).to_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS") {
            return invalid(format!("unsupported method {method}"));
        }
        let content_type = req.content_type.clone().or_else(|| match &req.body {
            Some(Value::String(_)) => None,
            Some(_) => Some("application/json".into()),
            None => None,
        });
        for (k, v) in &req.headers {
            if reqwest::header::HeaderName::from_bytes(k.as_bytes()).is_err() || reqwest::header::HeaderValue::from_str(v).is_err() {
                return invalid(format!("bad header {k:?}"));
            }
        }
        let headers: Vec<(String, String)> = req.headers.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();

        // Destinations.
        let mut destinations: Vec<Option<String>> = Vec::new();
        match (&req.url, &req.topic) {
            (Some(_), Some(_)) => return invalid("give either url or topic, not both"),
            (Some(url), None) => {
                check_url(url, self.config.block_private_urls)?;
                destinations.push(Some(url.clone()));
            }
            (None, Some(topic)) => {
                let t = self.topic(topic).await?;
                if t.endpoints.is_empty() {
                    return invalid(format!("topic {topic} has no endpoints"));
                }
                destinations.extend(t.endpoints.into_iter().map(|e| Some(e.url)));
            }
            (None, None) => {
                if req.queue.is_none() {
                    return invalid("give a url, a topic, or a queue to pull from");
                }
                destinations.push(None);
            }
        }
        for cb in [&req.callback, &req.failure_callback].into_iter().flatten() {
            check_url(cb, self.config.block_private_urls)?;
        }
        let queue = match &req.queue {
            Some(name) => Some(self.ensure_queue(name).await?),
            None => None,
        };
        if req.group.is_some() && queue.is_none() {
            return invalid("ordering groups need a queue");
        }

        // Deduplication: one key covers the whole publish (every topic endpoint).
        let dedup_id = match (&req.dedup_id, req.content_dedup) {
            (Some(id), _) => Some(id.clone()),
            (None, true) => {
                let mut h = Vec::new();
                h.extend(format!("{:?}|{:?}|{:?}|{method}|", req.url, req.topic, req.queue).as_bytes());
                for (k, v) in &headers {
                    h.extend(format!("{k}:{v}|").as_bytes());
                }
                h.extend(&body);
                Some(crate::sign::sha256_hex(&h))
            }
            _ => None,
        };
        let ids: Vec<String> = destinations.iter().map(|_| new_id("msg")).collect();
        if let Some(d) = &dedup_id {
            let key = format!("dedup:{d}");
            let claimed = self
                .store
                .kv_put(&key, &json!(ids), Expect::Absent, Some(now + self.config.dedup_window_ms))
                .await?;
            if !claimed {
                let existing = self.store.kv_get(&key).await?.map(|v| v.value).unwrap_or(json!([]));
                let existing: Vec<String> = serde_json::from_value(existing).unwrap_or_default();
                return Ok(existing
                    .into_iter()
                    .map(|id| Published { message_id: id, url: None, deduplicated: true })
                    .collect());
            }
        }

        let delay = req.delay.map(|d| d.0).unwrap_or(0).max(0);
        let mut due = now + delay;
        if let Some(t) = req.not_before {
            due = due.max(t.0);
        }
        if due > now + MAX_DELAY_MS {
            return invalid("messages can be delayed by at most a year");
        }
        if let Some(p) = req.priority {
            due -= p.clamp(-3600, 3600) * 1000;
        }
        let retries = req
            .retries
            .or(queue.as_ref().and_then(|q| q.retries))
            .unwrap_or(self.config.default_retries)
            .min(self.config.max_retries);
        let timeout_ms = req
            .timeout
            .map(|d| d.0)
            .or(queue.as_ref().and_then(|q| q.timeout_ms))
            .unwrap_or(self.config.default_timeout_ms)
            .clamp(1000, self.config.max_timeout_ms);
        let mut flows = Vec::new();
        if let Some(f) = &req.flow {
            if f.key.is_empty() {
                return invalid("flow control needs a key");
            }
            if f.parallelism.is_none() && f.rate.is_none() {
                return invalid("flow control needs parallelism or a rate");
            }
            flows.push(Flow { key: f.key.clone(), parallelism: f.parallelism, rate: f.rate.map(|r| r.0) });
        }
        let backoff = req.backoff.clone().unwrap_or_else(|| self.config.backoff.clone());
        backoff.validate().map_err(TaskError::Invalid)?;

        let mut out = Vec::new();
        for (id, dest) in ids.into_iter().zip(destinations) {
            let target = match &dest {
                Some(url) => Target::Http { url: url.clone(), method: method.clone(), headers: headers.clone() },
                None => Target::Pull,
            };
            let mut m = self.build_message(
                id,
                NewMessage {
                    target,
                    queue: queue.as_ref().map(|q| q.name.clone()),
                    due_at: due,
                    retries,
                    backoff: backoff.clone(),
                    timeout_ms,
                    flows: flows.clone(),
                    group: req.group.as_ref().map(|g| format!("{}/{g}", req.queue.as_deref().unwrap_or(""))),
                    run_id: None,
                    schedule_id: schedule_id.clone(),
                    label: req.label.clone(),
                },
                queue.as_ref(),
            );
            m.callback = req.callback.clone();
            m.failure_callback = req.failure_callback.clone();
            m.dedup_id = dedup_id.clone();
            m.topic = req.topic.clone();
            m.content_type = content_type.clone();
            m.body_size = body.len();
            self.insert(&m, &body).await?;
            self.count(|u| {
                u.published += 1;
                u.bytes_in += body.len() as u64;
            });
            out.push(Published { message_id: m.id, url: dest, deduplicated: false });
        }
        Ok(out)
    }

    pub(crate) fn build_message(&self, id: String, n: NewMessage, queue: Option<&Queue>) -> Message {
        let now = now_ms();
        let lane = match (&n.target, queue) {
            (Target::Pull, Some(q)) => Queue::pull_lane(&q.name),
            (_, Some(q)) => {
                let parts = q.partitions.max(1);
                let p = store::shard_of(n.group.as_deref().unwrap_or(&id), parts);
                format!("q:{}:{p}", q.name)
            }
            (_, None) => format!("s{}", store::shard_of(&id, self.store.shards())),
        };
        Message {
            id,
            target: n.target,
            queue: n.queue,
            lane,
            state: MsgState::Pending,
            due_at: n.due_at,
            created_at: now,
            updated_at: now,
            attempts: 0,
            retries: n.retries,
            backoff: n.backoff,
            timeout_ms: n.timeout_ms,
            callback: None,
            failure_callback: None,
            dedup_id: None,
            group: n.group,
            flows: n.flows,
            topic: None,
            schedule_id: n.schedule_id,
            run_id: n.run_id,
            label: n.label,
            content_type: None,
            body_size: 0,
            lease: None,
            deferrals: 0,
            last: None,
            reason: None,
            expires_at: None,
            version: 1,
        }
    }

    pub(crate) async fn insert(&self, m: &Message, body: &[u8]) -> Result<()> {
        self.store.msg_insert(m, body).await?;
        self.nudge_lane(&m.lane, m.due_at);
        Ok(())
    }

    /// Insert an internal message (timer, tick, run invocation…).
    pub(crate) async fn enqueue(&self, n: NewMessage, body: &[u8]) -> Result<Message> {
        let queue = match &n.queue {
            Some(name) => Some(self.ensure_queue(name).await?),
            None => None,
        };
        let m = self.build_message(new_id("msg"), n, queue.as_ref());
        self.insert(&m, body).await?;
        Ok(m)
    }

    // --- messages ---

    pub async fn message(&self, id: &str) -> Result<Message> {
        self.store.msg_get(id).await?.ok_or_else(|| TaskError::NotFound(format!("message {id}")))
    }

    pub async fn message_body(&self, id: &str) -> Result<Vec<u8>> {
        Ok(self.store.msg_body(id).await?.unwrap_or_default())
    }

    pub async fn messages(&self, q: &MessageQuery) -> Result<Page<Message>> {
        Ok(self.store.msg_list(q).await?)
    }

    pub async fn attempts(&self, id: &str) -> Result<Vec<Attempt>> {
        Ok(self.store.attempts(id).await?)
    }

    /// Cancel a message that hasn't finished. A delivery in flight completes but its result is dropped.
    pub async fn cancel(&self, id: &str, reason: &str) -> Result<Message> {
        for _ in 0..10 {
            let mut m = self.message(id).await?;
            if m.state.is_final() {
                return Err(TaskError::Conflict(format!("message {id} is already {}", m.state.as_str())));
            }
            let v = m.version;
            let now = now_ms();
            m.state = MsgState::Canceled;
            m.reason = Some(reason.into());
            m.updated_at = now;
            m.lease = None;
            m.expires_at = Some(now + self.config.retention_ms);
            if self.store.msg_update(&m, v).await? {
                m.version = v + 1;
                self.after_final(&m).await;
                return Ok(m);
            }
        }
        Err(TaskError::Conflict(format!("message {id} is changing; try again")))
    }

    /// Cancel every unfinished message matching a filter. Returns how many were canceled.
    pub async fn cancel_many(&self, mut q: MessageQuery) -> Result<usize> {
        // `Pending` lists active messages too: those mid-delivery are canceled as well.
        q.state = Some(MsgState::Pending);
        q.user_only = true;
        q.limit = 500;
        let mut n = 0;
        loop {
            let page = self.store.msg_list(&q).await?;
            for m in &page.items {
                if self.cancel(&m.id, "canceled").await.is_ok() {
                    n += 1;
                }
            }
            match page.cursor {
                Some(c) => q.cursor = Some(c),
                None => return Ok(n),
            }
        }
    }

    /// Send a failed (dead-lettered) or canceled message again, now.
    pub async fn retry(&self, id: &str) -> Result<Message> {
        let mut m = self.message(id).await?;
        if !matches!(m.state, MsgState::Failed | MsgState::Canceled) {
            return Err(TaskError::Conflict(format!("message {id} is {}; only failed or canceled ones can be retried", m.state.as_str())));
        }
        if !matches!(m.target, Target::Http { .. } | Target::Pull) {
            return invalid("only published messages can be retried");
        }
        let v = m.version;
        let now = now_ms();
        m.state = MsgState::Pending;
        m.due_at = now;
        m.attempts = 0;
        m.deferrals = 0;
        m.reason = None;
        m.expires_at = None;
        m.updated_at = now;
        if !self.store.msg_update(&m, v).await? {
            return Err(TaskError::Conflict(format!("message {id} changed; try again")));
        }
        m.version = v + 1;
        self.nudge_lane(&m.lane, now);
        Ok(m)
    }

    pub async fn delete_message(&self, id: &str) -> Result<()> {
        let m = self.message(id).await?;
        if !m.state.is_final() {
            return Err(TaskError::Conflict(format!("message {id} is {}; cancel it first", m.state.as_str())));
        }
        self.store.msg_delete(id).await?;
        Ok(())
    }

    /// After a message reaches a final state: let the next message of its group go.
    pub(crate) async fn after_final(&self, m: &Message) {
        if let Some(g) = &m.group {
            match self.store.msg_group_head(g).await {
                Ok(Some(next)) => self.wake_message(&next).await,
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, group = g, "group head lookup failed"),
            }
        }
    }

    /// Make a parked message due now.
    pub(crate) async fn wake_message(&self, id: &str) {
        for _ in 0..5 {
            let Ok(Some(mut m)) = self.store.msg_get(id).await else { return };
            let now = now_ms();
            if m.state != MsgState::Pending || m.due_at <= now {
                return;
            }
            let v = m.version;
            m.due_at = now;
            match self.store.msg_update(&m, v).await {
                Ok(true) => {
                    self.nudge_lane(&m.lane, now);
                    return;
                }
                Ok(false) => continue,
                Err(_) => return,
            }
        }
    }

    // --- queues ---

    pub async fn queues(&self) -> Result<Vec<Queue>> {
        Ok(store::list_docs(&*self.store, KIND_QUEUE).await?)
    }

    pub async fn queue(&self, name: &str) -> Result<Versioned<Queue>> {
        store::get_doc(&*self.store, KIND_QUEUE, name).await?.ok_or_else(|| TaskError::NotFound(format!("queue {name}")))
    }

    pub(crate) async fn ensure_queue(&self, name: &str) -> Result<Queue> {
        if let Some(q) = self.queue_cached(name) {
            return Ok(q);
        }
        valid_name("queue", name)?;
        if let Some(q) = store::get_doc::<Queue>(&*self.store, KIND_QUEUE, name).await? {
            self.edit_cache(|c| c.queues.insert(name.into(), q.value.clone()));
            return Ok(q.value);
        }
        let now = now_ms();
        let q = Queue { name: name.into(), partitions: 1, created_at: now, updated_at: now, ..Default::default() };
        store::put_doc(&*self.store, KIND_QUEUE, name, &q, Expect::Absent).await?;
        let q = self.queue(name).await?.value;
        self.edit_cache(|c| c.queues.insert(name.into(), q.clone()));
        Ok(q)
    }

    /// Create or update a queue.
    pub async fn put_queue(&self, name: &str, update: QueueUpdate) -> Result<Queue> {
        valid_name("queue", name)?;
        if name.starts_with("fn:") && !update.internal {
            return invalid("queues named fn:… belong to functions");
        }
        for _ in 0..10 {
            let now = now_ms();
            let current = store::get_doc::<Queue>(&*self.store, KIND_QUEUE, name).await?;
            let (mut q, expect) = match current {
                Some(v) => (v.value, Expect::Version(v.version)),
                None => (Queue { name: name.into(), partitions: 1, created_at: now, ..Default::default() }, Expect::Absent),
            };
            if let Some(p) = update.parallelism {
                q.parallelism = (p > 0).then_some(p);
            }
            if let Some(r) = &update.rate {
                q.rate = r.map(|r| r.0);
            }
            if let Some(p) = update.paused {
                q.paused = p;
            }
            if let Some(p) = update.partitions {
                if p == 0 || p > 256 {
                    return invalid("partitions must be 1–256");
                }
                if p < q.partitions {
                    return invalid("partitions can only grow (messages already live in the existing ones)");
                }
                q.partitions = p;
            }
            if let Some(r) = update.retries {
                q.retries = r;
            }
            if let Some(t) = update.timeout {
                q.timeout_ms = t.map(|d| d.0);
            }
            q.updated_at = now;
            if store::put_doc(&*self.store, KIND_QUEUE, name, &q, expect).await? {
                self.edit_cache(|c| c.queues.insert(name.into(), q.clone()));
                if !q.paused {
                    for lane in q.push_lanes() {
                        self.nudge_lane(&lane, now);
                    }
                }
                return Ok(q);
            }
        }
        Err(TaskError::Conflict(format!("queue {name} is changing; try again")))
    }

    pub async fn delete_queue(&self, name: &str, force: bool) -> Result<usize> {
        let q = self.queue(name).await?.value;
        let pending = self
            .store
            .msg_list(&MessageQuery { queue: Some(name.into()), state: Some(MsgState::Pending), limit: 1, ..Default::default() })
            .await?;
        let mut canceled = 0;
        if !pending.items.is_empty() {
            if !force {
                return Err(TaskError::Conflict(format!("queue {name} has unfinished messages; pass force to cancel them")));
            }
            canceled = self.cancel_many(MessageQuery { queue: Some(name.into()), ..Default::default() }).await?;
        }
        self.store.doc_delete(KIND_QUEUE, &q.name).await?;
        self.edit_cache(|c| c.queues.remove(name));
        Ok(canceled)
    }

    pub async fn queue_stats(&self, q: &Queue) -> Result<Value> {
        let now = now_ms();
        let (mut ready, mut scheduled, mut active) = (0, 0, 0);
        for lane in q.push_lanes().chain([Queue::pull_lane(&q.name)]) {
            let (r, s, a) = self.store.lane_depth(&lane, now).await?;
            ready += r;
            scheduled += s;
            active += a;
        }
        let inflight = crate::flow::slot_holders(self, &format!("q:{}", q.name)).await.unwrap_or(0);
        Ok(json!({ "ready": ready, "scheduled": scheduled, "active": active, "inflight": inflight }))
    }

    // --- topics ---

    pub async fn topics(&self) -> Result<Vec<Topic>> {
        Ok(store::list_docs(&*self.store, KIND_TOPIC).await?)
    }

    pub async fn topic(&self, name: &str) -> Result<Topic> {
        Ok(store::get_doc::<Topic>(&*self.store, KIND_TOPIC, name)
            .await?
            .ok_or_else(|| TaskError::NotFound(format!("topic {name}")))?
            .value)
    }

    /// Replace a topic's endpoints (creating the topic).
    pub async fn put_topic(&self, name: &str, endpoints: Vec<TopicEndpoint>) -> Result<Topic> {
        valid_name("topic", name)?;
        for e in &endpoints {
            check_url(&e.url, self.config.block_private_urls)?;
        }
        let now = now_ms();
        let created_at = store::get_doc::<Topic>(&*self.store, KIND_TOPIC, name).await?.map(|t| t.value.created_at).unwrap_or(now);
        let mut endpoints = endpoints;
        endpoints.dedup_by(|a, b| a.url == b.url);
        let t = Topic { name: name.into(), endpoints, created_at, updated_at: now };
        store::put_doc(&*self.store, KIND_TOPIC, name, &t, Expect::Any).await?;
        Ok(t)
    }

    pub async fn add_topic_endpoints(&self, name: &str, add: Vec<TopicEndpoint>) -> Result<Topic> {
        for e in &add {
            check_url(&e.url, self.config.block_private_urls)?;
        }
        for _ in 0..10 {
            let now = now_ms();
            let (mut t, expect) = match store::get_doc::<Topic>(&*self.store, KIND_TOPIC, name).await? {
                Some(v) => (v.value, Expect::Version(v.version)),
                None => {
                    valid_name("topic", name)?;
                    (Topic { name: name.into(), endpoints: vec![], created_at: now, updated_at: now }, Expect::Absent)
                }
            };
            for e in &add {
                if !t.endpoints.iter().any(|x| x.url == e.url) {
                    t.endpoints.push(e.clone());
                }
            }
            t.updated_at = now;
            if store::put_doc(&*self.store, KIND_TOPIC, name, &t, expect).await? {
                return Ok(t);
            }
        }
        Err(TaskError::Conflict(format!("topic {name} is changing; try again")))
    }

    pub async fn remove_topic_endpoints(&self, name: &str, urls: &[String]) -> Result<Topic> {
        for _ in 0..10 {
            let v = store::get_doc::<Topic>(&*self.store, KIND_TOPIC, name).await?.ok_or_else(|| TaskError::NotFound(format!("topic {name}")))?;
            let mut t = v.value;
            t.endpoints.retain(|e| !urls.contains(&e.url) && !e.name.as_ref().is_some_and(|n| urls.contains(n)));
            t.updated_at = now_ms();
            if store::put_doc(&*self.store, KIND_TOPIC, name, &t, Expect::Version(v.version)).await? {
                return Ok(t);
            }
        }
        Err(TaskError::Conflict(format!("topic {name} is changing; try again")))
    }

    pub async fn delete_topic(&self, name: &str) -> Result<()> {
        if !self.store.doc_delete(KIND_TOPIC, name).await? {
            return Err(TaskError::NotFound(format!("topic {name}")));
        }
        Ok(())
    }

    // --- signing keys ---

    pub async fn signing_keys(&self) -> Result<SigningKeys> {
        if let Some(k) = self.cache.read().unwrap().keys.clone() {
            return Ok(k);
        }
        let fresh = SigningKeys { current: crate::sign::new_secret(), next: crate::sign::new_secret(), rotated_at: now_ms() };
        store::put_doc(&*self.store, KIND_KEYS, "signing", &fresh, Expect::Absent).await?;
        let keys = store::get_doc::<SigningKeys>(&*self.store, KIND_KEYS, "signing")
            .await?
            .ok_or_else(|| anyhow::anyhow!("signing keys vanished"))?
            .value;
        self.edit_cache(|c| c.keys = Some(keys.clone()));
        Ok(keys)
    }

    /// The next key becomes current and a new next key is made. Receivers that already trust
    /// the next key keep verifying throughout.
    pub async fn rotate_signing_keys(&self) -> Result<SigningKeys> {
        let v = store::get_doc::<SigningKeys>(&*self.store, KIND_KEYS, "signing").await?.ok_or_else(|| TaskError::NotFound("signing keys".into()))?;
        let keys = SigningKeys { current: v.value.next, next: crate::sign::new_secret(), rotated_at: now_ms() };
        if !store::put_doc(&*self.store, KIND_KEYS, "signing", &keys, Expect::Version(v.version)).await? {
            return Err(TaskError::Conflict("keys are being rotated; try again".into()));
        }
        self.edit_cache(|c| c.keys = Some(keys.clone()));
        Ok(keys)
    }

    pub(crate) fn sign_headers(&self, id: &str, body: &[u8]) -> Vec<(String, String)> {
        let keys = self.cache.read().unwrap().keys.clone();
        let ts = now_ms() / 1000;
        let mut out = vec![("webhook-id".to_string(), id.to_string()), ("webhook-timestamp".to_string(), ts.to_string())];
        if let Some(k) = keys
            && let Ok(sig) = crate::sign::signature_header(&[&k.current, &k.next], id, ts, body)
        {
            out.push(("webhook-signature".into(), sig));
        }
        out
    }

    // --- API keys ---

    /// Whether a bearer token may use the API.
    pub fn authorize(&self, token: Option<&str>) -> bool {
        if !self.auth_required() {
            return true;
        }
        let Some(token) = token else { return false };
        if let Some(admin) = &self.config.token
            && constant_eq(token.as_bytes(), admin.as_bytes())
        {
            return true;
        }
        let hash = crate::sign::sha256_hex(token.as_bytes());
        self.cache.read().unwrap().api_keys.iter().any(|k| constant_eq(k.hash.as_bytes(), hash.as_bytes()))
    }

    /// Note that an API key was used (at most once a minute per key and node).
    pub async fn touch_api_key(&self, token: &str) {
        let hash = crate::sign::sha256_hex(token.as_bytes());
        let now = now_ms();
        let id = {
            let mut cache = self.cache.write().unwrap();
            let Some(k) = cache.api_keys.iter_mut().find(|k| constant_eq(k.hash.as_bytes(), hash.as_bytes())) else { return };
            if k.last_used_at.is_some_and(|t| now - t < 60_000) {
                return;
            }
            k.last_used_at = Some(now);
            k.id.clone()
        };
        // Versioned write: a key deleted meanwhile stays deleted.
        if let Ok(Some(mut doc)) = store::get_doc::<ApiKey>(&*self.store, KIND_API_KEY, &id).await {
            doc.value.last_used_at = Some(now);
            let _ = store::put_doc(&*self.store, KIND_API_KEY, &id, &doc.value, Expect::Version(doc.version)).await;
        }
    }

    pub fn auth_required(&self) -> bool {
        self.config.token.is_some() || self.config.require_auth
    }

    pub async fn api_keys(&self) -> Result<Vec<ApiKey>> {
        Ok(store::list_docs(&*self.store, KIND_API_KEY).await?)
    }

    /// Returns the key and the token (shown once).
    pub async fn create_api_key(&self, name: &str) -> Result<(ApiKey, String)> {
        use base64::Engine;
        let token = format!("rt_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(crate::ids::token(24)));
        let key = ApiKey {
            id: new_id("key"),
            name: name.into(),
            hash: crate::sign::sha256_hex(token.as_bytes()),
            prefix: token[..7].to_string(),
            created_at: now_ms(),
            last_used_at: None,
        };
        store::put_doc(&*self.store, KIND_API_KEY, &key.id, &key, Expect::Absent).await?;
        self.edit_cache(|c| c.api_keys.push(key.clone()));
        Ok((key, token))
    }

    pub async fn delete_api_key(&self, id: &str) -> Result<()> {
        if !self.store.doc_delete(KIND_API_KEY, id).await? {
            return Err(TaskError::NotFound(format!("api key {id}")));
        }
        self.edit_cache(|c| c.api_keys.retain(|k| k.id != id));
        Ok(())
    }

    // --- usage ---

    /// Flush usage counters if the last flush is older than `max_age_ms` (API nodes have no
    /// heartbeat to do it for them).
    pub async fn flush_usage_if_older(&self, max_age_ms: i64) -> anyhow::Result<()> {
        let last = self.last_flush.load(Ordering::Relaxed);
        let now = now_ms();
        if now - last < max_age_ms || self.last_flush.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return Ok(());
        }
        self.flush_usage().await
    }

    pub async fn flush_usage(&self) -> anyhow::Result<()> {
        self.last_flush.store(now_ms(), Ordering::Relaxed);
        let mut u = std::mem::take(&mut *self.usage.lock().unwrap());
        // Storage writes ride along with other usage: recording them is a write too, and on its
        // own would keep an idle store writing forever.
        self.storage_writes.fetch_add(self.store.take_storage_writes(), Ordering::Relaxed);
        if !u.is_zero() {
            u.storage_writes += self.storage_writes.swap(0, Ordering::Relaxed);
        }
        if !u.is_zero()
            && let Err(e) = self.store.usage_add(store::day_of(now_ms()), &u).await
        {
            self.usage.lock().unwrap().add(&u);
            return Err(e);
        }
        Ok(())
    }

    pub async fn usage_report(&self, days: i64) -> Result<Value> {
        self.flush_usage().await?;
        let days = days.clamp(1, 366);
        let today = store::day_of(now_ms());
        let rows = self.store.usage(today - days + 1).await?;
        let mut total = Usage::default();
        for (_, u) in &rows {
            total.add(u);
        }
        let layout = if self.store.describe().starts_with("dynamo") { crate::cost::Layout::Dynamodb } else { crate::cost::Layout::Log };
        let workers = self.cache.read().unwrap().nodes.len();
        let cost = crate::cost::estimate(
            &total,
            crate::cost::Deployment {
                layout,
                workers,
                front_door: self.config.front_door,
                hosted: self.config.hosted,
                log_express: self.config.log_express,
            },
        );
        let per_day = cost.total_usd / days as f64;
        Ok(json!({
            "period": { "days": days, "start_day": today - days + 1, "end_day": today },
            "total": total,
            "daily": rows.iter().map(|(d, u)| json!({ "day": d, "usage": u })).collect::<Vec<_>>(),
            "cost": cost,
            "projected_month_usd": per_day * 30.0,
        }))
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn request_body(req: &PublishRequest) -> Result<Vec<u8>> {
    use base64::Engine;
    Ok(match (&req.body, &req.body_base64) {
        (Some(_), Some(_)) => return invalid("give body or body_base64, not both"),
        (Some(Value::String(s)), None) => s.clone().into_bytes(),
        (Some(v), None) => serde_json::to_vec(v).map_err(anyhow::Error::from)?,
        (None, Some(b)) => base64::engine::general_purpose::STANDARD
            .decode(b)
            .map_err(|e| TaskError::Invalid(format!("bad body_base64: {e}")))?,
        (None, None) => Vec::new(),
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct QueueUpdate {
    /// 0 removes the limit.
    #[serde(default)]
    pub parallelism: Option<u32>,
    /// `null` removes the limit.
    #[serde(default, deserialize_with = "double_option")]
    pub rate: Option<Option<RateSpec>>,
    #[serde(default)]
    pub paused: Option<bool>,
    #[serde(default)]
    pub partitions: Option<u32>,
    #[serde(default, deserialize_with = "double_option")]
    pub retries: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option")]
    pub timeout: Option<Option<Dur>>,
    #[serde(skip)]
    pub internal: bool,
}

/// Tell "absent" (leave as is) from `null` (clear).
pub fn double_option<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates() {
        assert_eq!(parse_rate(&json!("10/s")).unwrap(), Rate { limit: 10, period_ms: 1000 });
        assert_eq!(parse_rate(&json!("100/5m")).unwrap(), Rate { limit: 100, period_ms: 300_000 });
        assert_eq!(parse_rate(&json!({"limit": 3, "period": "2s"})).unwrap(), Rate { limit: 3, period_ms: 2000 });
        assert_eq!(parse_rate(&json!({"limit": 3, "period": 500})).unwrap(), Rate { limit: 3, period_ms: 500 });
        assert!(parse_rate(&json!("0/s")).is_err());
        assert!(parse_rate(&json!("ten/s")).is_err());
        assert!(parse_rate(&json!(5)).is_err());
    }

    #[test]
    fn durations_and_times() {
        let d: Dur = serde_json::from_value(json!("1m")).unwrap();
        assert_eq!(d.0, 60_000);
        let d: Dur = serde_json::from_value(json!(250)).unwrap();
        assert_eq!(d.0, 250);
        let t: Time = serde_json::from_value(json!("2026-01-01T00:00:00Z")).unwrap();
        assert_eq!(t.0, 1_767_225_600_000);
        assert!(serde_json::from_value::<Time>(json!("tomorrow")).is_err());
    }

    #[test]
    fn urls() {
        assert!(check_url("https://example.com/x", true).is_ok());
        assert!(check_url("ftp://example.com", false).is_err());
        assert!(check_url("not a url", false).is_err());
        assert!(check_url("http://127.0.0.1:8080", false).is_ok());
        for blocked in ["http://127.0.0.1", "http://169.254.169.254/latest", "http://[::1]/", "http://10.1.2.3", "http://localhost:3000"] {
            assert!(check_url(blocked, true).is_err(), "{blocked}");
        }
    }

    #[test]
    fn bodies() {
        let req = |body: Value| PublishRequest { body: Some(body), ..Default::default() };
        assert_eq!(request_body(&req(json!("hi"))).unwrap(), b"hi");
        assert_eq!(request_body(&req(json!({"a": 1}))).unwrap(), br#"{"a":1}"#);
        let b64 = PublishRequest { body_base64: Some("aGk=".into()), ..Default::default() };
        assert_eq!(request_body(&b64).unwrap(), b"hi");
        assert!(request_body(&PublishRequest { body_base64: Some("!!".into()), ..Default::default() }).is_err());
    }
}

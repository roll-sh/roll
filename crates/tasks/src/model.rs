//! Everything roll tasks stores. Times are unix milliseconds.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Ms = i64;

pub fn now_ms() -> Ms {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MsgState {
    /// Waiting for `due_at` (first delivery, a retry, or flow control).
    Pending,
    /// Leased by a worker or pull consumer until `due_at`; it becomes due again if the lease lapses.
    Active,
    Delivered,
    /// Retries exhausted (the message is in the dead-letter queue).
    Failed,
    Canceled,
}

impl MsgState {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Delivered | Self::Failed | Self::Canceled)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "active" => Self::Active,
            "delivered" => Self::Delivered,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

/// What delivering a message means.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Target {
    /// Push: an HTTP request to `url`.
    Http {
        url: String,
        method: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<(String, String)>,
    },
    /// Pull: held in its queue until a consumer leases it.
    Pull,
    /// Fire a schedule and arm its next tick.
    Tick { schedule_id: String, at: Ms },
    /// Call a workflow function for a run.
    Run { run_id: String },
    /// Resume a run when a sleep ends or a wait/invoke times out; or time out a run.
    Timer { run_id: String, step_id: String, kind: TimerKind },
    /// Start a run from a batch of events once the batch window closes.
    Flush { function_id: String, key: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerKind {
    Sleep,
    WaitTimeout,
    InvokeTimeout,
    StartTimeout,
    FinishTimeout,
}

impl Target {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Http { .. } => "http",
            Self::Pull => "pull",
            Self::Tick { .. } => "tick",
            Self::Run { .. } => "run",
            Self::Timer { .. } => "timer",
            Self::Flush { .. } => "flush",
        }
    }
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Http { url, .. } => Some(url),
            _ => None,
        }
    }
}

/// Retry backoff. Delay before retry `n` (1-based) is `min(max, base * factor^(n-1))`, with
/// up to `jitter` of it randomised away.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Backoff {
    #[serde(default = "Backoff::default_strategy")]
    pub strategy: Strategy,
    pub base_ms: i64,
    pub max_ms: i64,
    #[serde(default = "Backoff::default_jitter")]
    pub jitter: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Exponential,
    Linear,
    Fixed,
}

impl Backoff {
    fn default_strategy() -> Strategy {
        Strategy::Exponential
    }
    fn default_jitter() -> f64 {
        0.2
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.base_ms < 0 || self.max_ms < self.base_ms || self.max_ms > 7 * 86_400_000 {
            return Err("backoff needs 0 <= base_ms <= max_ms <= 7 days".into());
        }
        if !(0.0..=1.0).contains(&self.jitter) {
            return Err("backoff jitter is between 0 and 1".into());
        }
        Ok(())
    }

    pub fn delay_ms(&self, retry: u32, random: f64) -> i64 {
        let n = retry.max(1) as f64;
        let raw = match self.strategy {
            Strategy::Exponential => self.base_ms as f64 * 2f64.powf(n - 1.0),
            Strategy::Linear => self.base_ms as f64 * n,
            Strategy::Fixed => self.base_ms as f64,
        };
        let capped = raw.min(self.max_ms as f64);
        let jitter = self.jitter.clamp(0.0, 1.0);
        (capped * (1.0 - jitter * random)).max(0.0) as i64
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self { strategy: Strategy::Exponential, base_ms: 2_000, max_ms: 3_600_000, jitter: 0.2 }
    }
}

/// `limit` per `period_ms`, as a smooth rate (GCRA) that allows a burst of `limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rate {
    pub limit: u32,
    pub period_ms: i64,
}

/// Per-key flow control shared by every message with the same key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Flow {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<Rate>,
}

/// The result of the last attempt.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub at: Ms,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub duration_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// Due-index partition: `s<n>` (shared shards), `q:<queue>:<n>` (push queue) or `p:<queue>` (pull).
    pub lane: String,
    pub state: MsgState,
    /// Next attempt, or when the current lease lapses.
    pub due_at: Ms,
    pub created_at: Ms,
    pub updated_at: Ms,
    /// Deliveries started so far.
    pub attempts: u32,
    /// Retries allowed after the first attempt.
    pub retries: u32,
    pub backoff: Backoff,
    pub timeout_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_callback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_id: Option<String>,
    /// Ordering key: messages with the same group are delivered one at a time, in publish order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Per-key limits, acquired in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flows: Vec<Flow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub body_size: usize,
    /// Lease token of the current holder (pull receipts, stale-worker detection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// Consecutive times flow control held the message back.
    #[serde(default)]
    pub deferrals: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Outcome>,
    /// Reason for the final state (cancel reason, last error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Final messages are deleted after this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Ms>,
    pub version: u64,
}

impl Message {
    pub fn is_dlq(&self) -> bool {
        self.state == MsgState::Failed
    }
}

/// One delivery attempt, kept for the message log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub message_id: String,
    pub n: u32,
    pub at: Ms,
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// When the next retry is due, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at: Option<Ms>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Queue {
    pub name: String,
    /// Deliveries in flight at once across every worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<Rate>,
    #[serde(default)]
    pub paused: bool,
    /// Due-index partitions; raise for queues above a few hundred messages per second.
    #[serde(default = "one")]
    pub partitions: u32,
    /// Defaults for messages published to the queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
    pub created_at: Ms,
    pub updated_at: Ms,
}

fn one() -> u32 {
    1
}

impl Queue {
    pub fn push_lanes(&self) -> impl Iterator<Item = String> + '_ {
        (0..self.partitions.max(1)).map(|p| format!("q:{}:{p}", self.name))
    }
    pub fn pull_lane(name: &str) -> String {
        format!("p:{name}")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TopicEndpoint {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Fan-out: publishing to a topic delivers a copy to every endpoint, each retried on its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Topic {
    pub name: String,
    pub endpoints: Vec<TopicEndpoint>,
    pub created_at: Ms,
    pub updated_at: Ms,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleTarget {
    /// Publish a message (to a URL, topic or queue).
    Publish { request: Box<crate::engine::PublishRequest> },
    /// Send an event.
    Event { name: String, #[serde(default)] data: Value },
    /// Start a run of a function.
    Function { function_id: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    /// Cron pattern (5 or 6 fields, `@hourly` …) or `@every <duration>`.
    pub cron: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    pub target: ScheduleTarget,
    #[serde(default)]
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at: Option<Ms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<Ms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_id: Option<String>,
    /// The armed tick message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: Ms,
    pub updated_at: Ms,
}

// --- workflows ---

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    /// Expression over `event` that must be true to start a run.
    #[serde(default, rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Concurrency {
    pub limit: u32,
    /// Expression over `event`; each value gets its own limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyedRate {
    pub limit: u32,
    pub period_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Debounce {
    pub period_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Start the run at the latest this long after the first event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    pub max_size: u32,
    pub timeout_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CancelOn {
    pub event: String,
    /// Field that must be equal in both events, e.g. `data.order_id`.
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_: Option<String>,
    /// Expression over `event` (the run's) and `async` (the incoming one).
    #[serde(default, rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Timeouts {
    /// Cancel a run that hasn't started this long after it was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<i64>,
    /// Cancel a run that hasn't finished this long after it was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SingletonMode {
    /// Drop new runs while one is in progress for the key.
    Skip,
    /// Cancel the run in progress and start the new one.
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Singleton {
    pub mode: SingletonMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// Between retries of a failing step or call (default: the server's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<Backoff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concurrency: Vec<Concurrency>,
    /// Hard limit: events over it are dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<KeyedRate>,
    /// Soft limit: runs over it are delayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle: Option<KeyedRate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce: Option<Debounce>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<Batch>,
    /// Expression over `event`: one run per value per 24h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cancel_on: Vec<CancelOn>,
    /// Expression over `event` giving seconds to run ahead (+) or behind (−) other runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<Timeouts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub singleton: Option<Singleton>,
    #[serde(default)]
    pub paused: bool,
}

/// An app is one deployment of the SDK: a URL serving a set of functions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub url: String,
    pub functions: Vec<FunctionDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<String>,
    pub synced_at: Ms,
    pub created_at: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    /// Sleeping, waiting for an event or for an invoked run.
    Waiting,
    Completed,
    Failed,
    Canceled,
}

impl RunStatus {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "waiting" => Self::Waiting,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

/// A step the run is blocked on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingOp {
    pub step_id: String,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Ms>,
    /// Event name (waits) or function id (invokes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Wait index key, for cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_run: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub function_id: String,
    pub app_id: String,
    pub status: RunStatus,
    /// Triggering events (one, or a batch).
    pub events: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    pub created_at: Ms,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Ms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Ms>,
    /// The invocation message queued or in flight (one at a time per run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<String>,
    /// Something changed while an invocation was in flight; invoke again when it ends.
    #[serde(default)]
    pub dirty: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<PendingOp>,
    /// Failed attempts per step, for step-level retries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub step_attempts: BTreeMap<String, u32>,
    /// `(run_id, step_id)` of the run that invoked this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<(String, String)>,
    /// Resolved keys, kept to release them when the run ends.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, String>,
    pub invocations: u32,
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Ms>,
}

/// A memoised step result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub run_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    pub at: Ms,
    #[serde(default)]
    pub attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub name: String,
    /// Sender-chosen id; a second event with the same one within 24h is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_id: Option<String>,
    #[serde(default)]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Value>,
    pub ts: Ms,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Ms>,
}

impl Event {
    /// The shape functions and expressions see.
    pub fn json(&self) -> Value {
        serde_json::json!({ "id": self.id, "name": self.name, "data": self.data, "user": self.user, "ts": self.ts })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    /// `step.waitForEvent`: resume the run with the event.
    Step,
    /// `cancelOn`: cancel the run.
    Cancel,
}

/// A run listening for an event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wait {
    pub id: String,
    pub event: String,
    /// Index key `path=value`, when the wait matches on a field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub kind: WaitKind,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_: Option<String>,
    /// The run's triggering event, for `if` expressions.
    #[serde(default)]
    pub origin: Value,
    pub expires_at: Ms,
}

/// Standard Webhooks signing secrets. Both are used while rotating.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SigningKeys {
    pub current: String,
    pub next: String,
    pub rotated_at: Ms,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    /// sha256 of the token, hex.
    pub hash: String,
    pub prefix: String,
    pub created_at: Ms,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<Ms>,
}

/// A live worker, for spreading lanes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub seen_at: Ms,
    pub started_at: Ms,
    #[serde(default)]
    pub inflight: usize,
    #[serde(default)]
    pub lanes: usize,
}

/// Daily counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub published: u64,
    pub deliveries: u64,
    pub delivered: u64,
    pub failed_attempts: u64,
    pub dead: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub events: u64,
    pub runs: u64,
    pub steps: u64,
    /// Requests that write to object storage (log store commits and snapshots).
    #[serde(default)]
    pub storage_writes: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.published += o.published;
        self.deliveries += o.deliveries;
        self.delivered += o.delivered;
        self.failed_attempts += o.failed_attempts;
        self.dead += o.dead;
        self.bytes_in += o.bytes_in;
        self.bytes_out += o.bytes_out;
        self.events += o.events;
        self.runs += o.runs;
        self.steps += o.steps;
        self.storage_writes += o.storage_writes;
    }
    pub fn is_zero(&self) -> bool {
        *self == Usage::default()
    }
    pub fn fields(&self) -> [(&'static str, u64); 11] {
        [
            ("published", self.published),
            ("deliveries", self.deliveries),
            ("delivered", self.delivered),
            ("failed_attempts", self.failed_attempts),
            ("dead", self.dead),
            ("bytes_in", self.bytes_in),
            ("bytes_out", self.bytes_out),
            ("events", self.events),
            ("runs", self.runs),
            ("steps", self.steps),
            ("storage_writes", self.storage_writes),
        ]
    }
    pub fn set(&mut self, field: &str, v: u64) {
        match field {
            "published" => self.published = v,
            "deliveries" => self.deliveries = v,
            "delivered" => self.delivered = v,
            "failed_attempts" => self.failed_attempts = v,
            "dead" => self.dead = v,
            "bytes_in" => self.bytes_in = v,
            "bytes_out" => self.bytes_out = v,
            "events" => self.events = v,
            "runs" => self.runs = v,
            "steps" => self.steps = v,
            "storage_writes" => self.storage_writes = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let b = Backoff { strategy: Strategy::Exponential, base_ms: 1000, max_ms: 10_000, jitter: 0.0 };
        assert_eq!(b.delay_ms(1, 0.5), 1000);
        assert_eq!(b.delay_ms(2, 0.5), 2000);
        assert_eq!(b.delay_ms(4, 0.5), 8000);
        assert_eq!(b.delay_ms(9, 0.5), 10_000);
        let lin = Backoff { strategy: Strategy::Linear, ..b.clone() };
        assert_eq!(lin.delay_ms(3, 0.0), 3000);
        let fixed = Backoff { strategy: Strategy::Fixed, ..b };
        assert_eq!(fixed.delay_ms(7, 0.0), 1000);
    }

    #[test]
    fn jitter_only_shortens() {
        let b = Backoff { jitter: 0.5, ..Backoff::default() };
        assert_eq!(b.delay_ms(1, 0.0), 2000);
        assert_eq!(b.delay_ms(1, 1.0), 1000);
    }

    #[test]
    fn target_round_trips() {
        let t = Target::Http { url: "https://x".into(), method: "POST".into(), headers: vec![("a".into(), "b".into())] };
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Target>(&s).unwrap(), t);
        let t = Target::Timer { run_id: "r".into(), step_id: "s".into(), kind: TimerKind::Sleep };
        assert_eq!(serde_json::from_str::<Target>(&serde_json::to_string(&t).unwrap()).unwrap(), t);
    }
}

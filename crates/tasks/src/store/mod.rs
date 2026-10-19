//! Storage. The engine only needs these primitives; each backend implements them natively:
//! SQLite for a single host, the log store (SQLite made durable in S3) or DynamoDB (+ S3 for
//! large bodies) for AWS.
//!
//! Every mutable record carries a `version` and writes are compare-and-set, so any number of
//! workers can share a store without locks.

pub mod log;
pub mod sqlite;
#[cfg(feature = "dynamo")]
pub mod dynamo;

use crate::model::*;
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub type Result<T> = anyhow::Result<T>;

/// Condition for a compare-and-set write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Any,
    /// The record must not exist (or must have expired).
    Absent,
    Version(u64),
}

#[derive(Clone, Debug)]
pub struct Versioned<T> {
    pub value: T,
    pub version: u64,
}

#[derive(Clone, Debug, Default)]
pub struct MessageQuery {
    /// `pending` also matches active messages; `dlq` is `failed`.
    pub state: Option<MsgState>,
    pub queue: Option<String>,
    pub topic: Option<String>,
    pub schedule_id: Option<String>,
    pub run_id: Option<String>,
    pub label: Option<String>,
    pub url: Option<String>,
    /// Exclude internal messages (ticks, timers, run invocations).
    pub user_only: bool,
    /// Ids are time-ordered; lists go newest first and continue below this id.
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, Default)]
pub struct RunQuery {
    pub function_id: Option<String>,
    pub status: Option<RunStatus>,
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, Default)]
pub struct EventQuery {
    pub name: Option<String>,
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub cursor: Option<String>,
}

/// Messages in a lane, earliest due first.
#[derive(Clone, Debug, Default)]
pub struct Due {
    pub ids: Vec<String>,
    /// Earliest due time after `now`, if one was seen.
    pub next_at: Option<Ms>,
}

#[async_trait]
pub trait Store: Send + Sync + 'static {
    fn describe(&self) -> String;
    /// Shared due-index shards for messages outside queues.
    fn shards(&self) -> u32;

    // --- messages ---
    async fn msg_insert(&self, m: &Message, body: &[u8]) -> Result<()>;
    async fn msg_get(&self, id: &str) -> Result<Option<Message>>;
    async fn msg_body(&self, id: &str) -> Result<Option<Vec<u8>>>;
    /// Compare-and-set on `version`; the stored copy gets `version + 1`. Keeps the due index,
    /// group index and expiry in step with the new state.
    async fn msg_update(&self, m: &Message, expect: u64) -> Result<bool>;
    async fn msg_delete(&self, id: &str) -> Result<()>;
    /// Due messages in a lane: pending ones whose time has come and active ones whose lease lapsed.
    async fn msg_due(&self, lane: &str, now: Ms, limit: usize) -> Result<Due>;
    /// The oldest unfinished message of an ordering group.
    async fn msg_group_head(&self, group: &str) -> Result<Option<String>>;
    async fn msg_list(&self, q: &MessageQuery) -> Result<Page<Message>>;
    /// `(ready, scheduled, active)` for a lane.
    async fn lane_depth(&self, lane: &str, now: Ms) -> Result<(u64, u64, u64)>;
    async fn attempt_add(&self, a: &Attempt) -> Result<()>;
    async fn attempts(&self, message_id: &str) -> Result<Vec<Attempt>>;

    // --- small records with compare-and-set and optional expiry ---
    async fn kv_get(&self, key: &str) -> Result<Option<Versioned<Value>>>;
    async fn kv_put(&self, key: &str, value: &Value, expect: Expect, expires_at: Option<Ms>) -> Result<bool>;
    async fn kv_delete(&self, key: &str, expect: Expect) -> Result<bool>;

    // --- configuration documents (queues, topics, schedules, apps, keys, nodes) ---
    async fn doc_get(&self, kind: &str, id: &str) -> Result<Option<Versioned<Value>>>;
    async fn doc_put(&self, kind: &str, id: &str, value: &Value, expect: Expect) -> Result<bool>;
    async fn doc_delete(&self, kind: &str, id: &str) -> Result<bool>;
    async fn doc_list(&self, kind: &str) -> Result<Vec<Versioned<Value>>>;

    // --- workflow runs ---
    async fn run_get(&self, id: &str) -> Result<Option<Run>>;
    /// Compare-and-set like `msg_update`; `Expect::Absent` inserts.
    async fn run_put(&self, run: &Run, expect: Expect) -> Result<bool>;
    async fn run_list(&self, q: &RunQuery) -> Result<Page<Run>>;
    async fn steps(&self, run_id: &str) -> Result<Vec<Step>>;
    async fn step_put(&self, step: &Step) -> Result<()>;

    // --- events and waits ---
    async fn event_put(&self, e: &Event) -> Result<()>;
    async fn event_get(&self, id: &str) -> Result<Option<Event>>;
    async fn event_list(&self, q: &EventQuery) -> Result<Page<Event>>;
    async fn wait_put(&self, w: &Wait) -> Result<()>;
    async fn wait_delete(&self, w: &Wait) -> Result<()>;
    /// Waits for `event` with exactly this key (`None`: waits without a key).
    async fn waits(&self, event: &str, key: Option<&str>) -> Result<Vec<Wait>>;

    // --- metering ---
    async fn usage_add(&self, day: i64, u: &Usage) -> Result<()>;
    async fn usage(&self, from_day: i64) -> Result<Vec<(i64, Usage)>>;

    /// Delete expired records (backends with native expiry do nothing).
    async fn sweep(&self, now: Ms) -> Result<u64>;

    /// Object-storage writes since the last call, for metering (stores billed per request).
    fn take_storage_writes(&self) -> u64 {
        0
    }
}

tokio::task_local! {
    static RELAXED: ();
}

/// Run `f` with relaxed durability: its writes may return before they're durable. For work
/// that is redone if lost (delivery outcomes, leases), which at-least-once delivery allows.
/// Stores that are always durable ignore it.
pub async fn relaxed<F: std::future::Future>(f: F) -> F::Output {
    RELAXED.scope((), f).await
}

pub fn is_relaxed() -> bool {
    RELAXED.try_with(|_| ()).is_ok()
}

/// Typed helpers over the JSON documents.
pub async fn get_doc<T: DeserializeOwned>(s: &dyn Store, kind: &str, id: &str) -> Result<Option<Versioned<T>>> {
    Ok(match s.doc_get(kind, id).await? {
        Some(v) => Some(Versioned { value: serde_json::from_value(v.value)?, version: v.version }),
        None => None,
    })
}

pub async fn put_doc<T: Serialize>(s: &dyn Store, kind: &str, id: &str, value: &T, expect: Expect) -> Result<bool> {
    s.doc_put(kind, id, &serde_json::to_value(value)?, expect).await
}

pub async fn list_docs<T: DeserializeOwned>(s: &dyn Store, kind: &str) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for v in s.doc_list(kind).await? {
        match serde_json::from_value(v.value) {
            Ok(t) => out.push(t),
            Err(e) => tracing::warn!(kind, error = %e, "skipping unreadable document"),
        }
    }
    Ok(out)
}

pub fn shard_of(id: &str, shards: u32) -> u32 {
    // FNV-1a: stable across processes and versions.
    let mut h: u32 = 0x811c9dc5;
    for b in id.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h % shards.max(1)
}

pub fn day_of(ms: Ms) -> i64 {
    ms.div_euclid(86_400_000)
}

#[cfg(test)]
pub mod conformance;

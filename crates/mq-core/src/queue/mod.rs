//! Queues. Each queue (or queue partition) is an actor that owns its messages, consumers and
//! log; everything else talks to it through a [`QueueHandle`].
//!
//! - `classic` and `quorum` queues share one engine ([`engine`]); quorum adds delivery limits,
//!   persists every message and only delivers what is durable.
//! - `stream` queues ([`stream`]) are append-only logs read by offset.

pub mod config;
pub mod engine;
pub mod partitioned;
pub mod stream;

use crate::error::{AmqpError, Result};
use crate::message::MessageRef;
use crate::value::FieldTable;
pub use config::{Overflow, QueueConfig, QueueType};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{Notify, mpsc, oneshot};

/// A connection, globally unique within a broker process.
pub type ConnId = u64;
/// A channel (or protocol session), globally unique within a broker process.
pub type ChannelId = u64;

/// Declared properties of a queue.
#[derive(Clone, Debug, Serialize)]
pub struct QueueMeta {
    pub name: String,
    pub vhost: String,
    pub durable: bool,
    pub auto_delete: bool,
    /// Owning connection of an exclusive queue.
    #[serde(skip)]
    pub exclusive_owner: Option<ConnId>,
    #[serde(serialize_with = "ser_table")]
    pub arguments: FieldTable,
    pub created_at: i64,
}

impl QueueMeta {
    pub fn exclusive(&self) -> bool {
        self.exclusive_owner.is_some()
    }
}

pub(crate) fn ser_table<S: serde::Serializer>(t: &FieldTable, s: S) -> std::result::Result<S::Ok, S::Error> {
    t.to_json().serialize(s)
}

/// A message handed to a consumer.
#[derive(Clone, Debug)]
pub struct Delivery {
    pub queue: QueueRef,
    pub consumer_tag: Arc<str>,
    /// Position in the queue (the stream offset for streams); acks name it.
    pub seq: u64,
    pub msg: MessageRef,
    pub redelivered: bool,
    /// Times delivered before this one (quorum queues expose it as `x-delivery-count`).
    pub delivery_count: u32,
    /// For `basic.get`: messages left in the queue.
    pub remaining: u32,
}

/// How a protocol session receives deliveries. Implementations must never block.
pub trait ConsumerSink: Send + Sync {
    /// Whether one more delivery fits in the prefetch window (and the socket isn't backed up).
    fn has_credit(&self) -> bool;
    /// Hand over a delivery (consumes one credit).
    fn deliver(&self, d: Delivery);
    /// The broker cancelled the consumer (queue deleted, moved, or failed over).
    fn cancelled(&self, tag: &str);
    /// Single active consumer: this consumer became (in)active.
    fn activated(&self, _tag: &str, _active: bool) {}
    /// A delivery wasn't acknowledged within the consumer timeout.
    fn timed_out(&self, _tag: &str, _timeout_ms: u64) {}
}

#[derive(Clone)]
pub struct ConsumerSpec {
    pub tag: Arc<str>,
    pub channel: ChannelId,
    pub conn: ConnId,
    pub no_ack: bool,
    pub exclusive: bool,
    pub priority: i32,
    pub args: FieldTable,
    pub sink: Arc<dyn ConsumerSink>,
}

impl std::fmt::Debug for ConsumerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consumer").field("tag", &self.tag).field("channel", &self.channel).finish()
    }
}

/// What to do with delivered messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ack,
    /// Back to the queue, flagged redelivered.
    Requeue,
    /// Dead-letter (or drop) with reason `rejected`.
    Reject,
}

/// AMQP 1.0's `modified` outcome (RabbitMQ 4 semantics).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Modification {
    /// Merged into the message's headers (its `x-opt-*` message annotations).
    pub annotations: FieldTable,
    /// Counts as a failed delivery (`x-delivery-count` goes up).
    pub delivery_failed: bool,
    /// Not to be redelivered here: dead-letter it (reason `rejected`).
    pub undeliverable_here: bool,
}

/// Settles a publisher confirm once every queue a message was routed to has it durably.
/// Dropping a ticket without settling counts as a failure (the publisher gets a nack).
pub struct ConfirmTicket {
    state: Arc<ConfirmState>,
    settled: bool,
}

type ConfirmDone = Box<dyn FnOnce(bool) + Send>;

struct ConfirmState {
    remaining: AtomicUsize,
    failed: AtomicBool,
    done: Mutex<Option<ConfirmDone>>,
}

impl ConfirmTicket {
    /// A ticket that calls `done(ok)` once every clone has settled.
    pub fn new(done: impl FnOnce(bool) + Send + 'static) -> Self {
        let state = ConfirmState { remaining: AtomicUsize::new(1), failed: AtomicBool::new(false), done: Mutex::new(Some(Box::new(done))) };
        ConfirmTicket { state: Arc::new(state), settled: false }
    }

    /// Another party that must settle before the confirm fires.
    pub fn share(&self) -> Self {
        self.state.remaining.fetch_add(1, Ordering::AcqRel);
        ConfirmTicket { state: self.state.clone(), settled: false }
    }

    pub fn settle(mut self, ok: bool) {
        self.finish(ok);
    }

    fn finish(&mut self, ok: bool) {
        if self.settled {
            return;
        }
        self.settled = true;
        if !ok {
            self.state.failed.store(true, Ordering::Release);
        }
        if self.state.remaining.fetch_sub(1, Ordering::AcqRel) == 1
            && let Some(f) = self.state.done.lock().take()
        {
            f(!self.state.failed.load(Ordering::Acquire));
        }
    }
}

impl Drop for ConfirmTicket {
    fn drop(&mut self) {
        self.finish(false);
    }
}

/// Counters readable without asking the actor (management API, declare-ok, metrics).
#[derive(Default, Debug)]
pub struct QueueShared {
    pub ready: AtomicU64,
    pub unacked: AtomicU64,
    pub ready_bytes: AtomicU64,
    pub unacked_bytes: AtomicU64,
    pub consumers: AtomicU64,
    pub published: AtomicU64,
    pub delivered: AtomicU64,
    pub acked: AtomicU64,
    pub redelivered: AtomicU64,
    pub got: AtomicU64,
    pub dead_lettered: AtomicU64,
    pub dropped: AtomicU64,
    /// Bytes this queue holds in its log (persistent messages, stream segments).
    pub log_bytes: AtomicU64,
    /// Unix ms of the last publish/deliver/ack.
    pub last_activity_ms: AtomicI64,
    /// Stream queues: first and next offset.
    pub first_offset: AtomicU64,
    pub next_offset: AtomicU64,
    /// Signalled whenever messages become available (HTTP long-polls wait on it).
    pub available: Notify,
    pub deleted: AtomicBool,
    /// Acks waiting for a classic or quorum queue's actor, ahead of its mailbox.
    pub settles: SettleLane,
}

impl QueueShared {
    pub fn messages(&self) -> u64 {
        self.ready.load(Ordering::Relaxed) + self.unacked.load(Ordering::Relaxed)
    }
}

/// Settles that skip the queue's mailbox. A publisher that outruns its consumers keeps the
/// mailbox full; an ack queued behind those publishes returns the consumer's prefetch credit
/// late, which starves the consumer exactly when it is needed most. The actor takes these
/// before each mailbox command, so a settle still comes before anything its channel sent later.
#[derive(Default, Debug)]
pub struct SettleLane {
    pending: Mutex<Vec<(ChannelId, Vec<u64>, Outcome)>>,
    waiting: AtomicBool,
    pub(crate) wake: Notify,
}

impl SettleLane {
    fn push(&self, channel: ChannelId, seqs: Vec<u64>, outcome: Outcome) {
        self.pending.lock().push((channel, seqs, outcome));
        if !self.waiting.swap(true, Ordering::AcqRel) {
            self.wake.notify_one();
        }
    }

    /// Everything waiting, oldest first; nothing (and no lock) when the lane is empty.
    pub(crate) fn take(&self) -> Vec<(ChannelId, Vec<u64>, Outcome)> {
        if !self.waiting.load(Ordering::Acquire) {
            return Vec::new();
        }
        let mut pending = self.pending.lock();
        self.waiting.store(false, Ordering::Release);
        std::mem::take(&mut *pending)
    }
}

/// A lease taken over HTTP (SQS-style): messages stay invisible until the deadline, then
/// return to the queue unless acknowledged.
#[derive(Clone, Debug)]
pub struct Leased {
    pub seq: u64,
    pub msg: MessageRef,
    pub delivery_count: u32,
    pub redelivered: bool,
    pub deadline_ms: u64,
}

/// A message as shown by the browser (management API / dashboard), without consuming it.
#[derive(Clone, Debug)]
pub struct Peeked {
    pub seq: u64,
    pub msg: MessageRef,
    pub redelivered: bool,
    pub delivery_count: u32,
}

/// Where a stream consumer starts reading.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamOffset {
    First,
    Last,
    Next,
    Offset(u64),
    /// Unix ms.
    Timestamp(u64),
}

#[allow(clippy::large_enum_variant)] // a mailbox: boxing would allocate per command
/// Rare commands box their large fields: a queue's mailbox allocates its first block of
/// slots up front, so the enum's size is memory every idle queue pays for.
pub enum QueueCmd {
    Publish { msg: MessageRef, confirm: Option<ConfirmTicket>, reply: Option<oneshot::Sender<PublishResult>> },
    Consume { spec: Box<ConsumerSpec>, reply: oneshot::Sender<Result<()>> },
    Cancel { channel: ChannelId, tag: Arc<str>, reply: Option<oneshot::Sender<bool>> },
    Get { channel: ChannelId, no_ack: bool, reply: oneshot::Sender<Result<Option<Delivery>>> },
    Settle { channel: ChannelId, seqs: Vec<u64>, outcome: Outcome },
    /// Requeue everything a channel holds and drop its consumers (channel closed).
    ChannelClosed { channel: ChannelId },
    /// `basic.recover`: redeliver a channel's unacked messages.
    Recover { channel: ChannelId },
    /// Return deliveries without counting a failed delivery (AMQP 1.0 `released`).
    Release { channel: ChannelId, seqs: Vec<u64> },
    /// AMQP 1.0 `modified`: merge annotations into the messages, then requeue or dead-letter.
    Modify { channel: ChannelId, seqs: Vec<u64>, modification: Box<Modification> },
    /// Credit freed up somewhere; try delivering again.
    Wake,
    Purge { reply: oneshot::Sender<u64> },
    Delete { if_unused: bool, if_empty: bool, reply: oneshot::Sender<Result<u64>> },
    Reconfigure { config: Box<QueueConfig> },
    Lease { max: usize, visibility_ms: u64, reply: oneshot::Sender<Vec<Leased>> },
    SettleLease { seq: u64, delivery_count: u32, outcome: Outcome, reply: oneshot::Sender<bool> },
    ExtendLease { seq: u64, delivery_count: u32, visibility_ms: u64, reply: oneshot::Sender<bool> },
    Peek { offset: usize, count: usize, reply: oneshot::Sender<Vec<Peeked>> },
    /// Mark the queue as used (redeclare), resetting `x-expires`.
    Touch,
    Consumers { reply: oneshot::Sender<Vec<ConsumerInfo>> },
    /// Flush and stop (the queue is moving to another node or the broker is shutting down).
    Stop { reply: oneshot::Sender<()> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishResult {
    Accepted,
    /// `reject-publish` overflow.
    Rejected,
    /// Deduplicated.
    Duplicate,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConsumerInfo {
    pub consumer_tag: String,
    pub channel: ChannelId,
    pub connection: ConnId,
    pub ack_required: bool,
    pub exclusive: bool,
    pub priority: i32,
    pub active: bool,
    #[serde(serialize_with = "ser_table")]
    pub arguments: FieldTable,
}

/// Handle to a queue actor. Cheap to clone.
pub struct QueueHandle {
    pub meta: QueueMeta,
    pub kind: QueueType,
    pub shared: Arc<QueueShared>,
    tx: mpsc::Sender<QueueCmd>,
    wake_pending: AtomicBool,
    /// Settles go through [`QueueShared::settles`] (the actor drains that lane).
    settle_lane: bool,
    /// Stream queues: the stream protocol's command channel (see [`stream::StreamCmd`]).
    stream_tx: Option<mpsc::Sender<stream::StreamCmd>>,
}

pub type QueueRef = Arc<QueueHandle>;

impl std::fmt::Debug for QueueHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue").field("name", &self.meta.name).field("vhost", &self.meta.vhost).finish()
    }
}

fn gone(q: &QueueMeta) -> AmqpError {
    AmqpError::not_found(format!("queue '{}' in vhost '{}'", q.name, q.vhost))
}

impl QueueHandle {
    pub fn new(meta: QueueMeta, kind: QueueType, shared: Arc<QueueShared>, tx: mpsc::Sender<QueueCmd>) -> Self {
        QueueHandle { meta, kind, shared, tx, wake_pending: AtomicBool::new(false), settle_lane: false, stream_tx: None }
    }

    /// Send settles through [`QueueShared::settles`]: for actors that drain it.
    pub(crate) fn with_settle_lane(mut self) -> Self {
        self.settle_lane = true;
        self
    }

    pub fn name(&self) -> &str {
        &self.meta.name
    }

    pub async fn send(&self, cmd: QueueCmd) -> Result<()> {
        self.tx.send(cmd).await.map_err(|_| gone(&self.meta))
    }

    pub async fn publish(&self, msg: MessageRef, confirm: Option<ConfirmTicket>) -> Result<()> {
        self.send(QueueCmd::Publish { msg, confirm, reply: None }).await
    }

    /// Publish and wait for the queue's verdict (HTTP API, shovels).
    pub async fn publish_wait(&self, msg: MessageRef, confirm: Option<ConfirmTicket>) -> Result<PublishResult> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::Publish { msg, confirm, reply: Some(reply) }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn consume(&self, spec: ConsumerSpec) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::Consume { spec: Box::new(spec), reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))?
    }

    pub async fn cancel(&self, channel: ChannelId, tag: Arc<str>) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.send(QueueCmd::Cancel { channel, tag, reply: Some(reply) }).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn get(&self, channel: ChannelId, no_ack: bool) -> Result<Option<Delivery>> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::Get { channel, no_ack, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))?
    }

    pub async fn settle(&self, channel: ChannelId, seqs: Vec<u64>, outcome: Outcome) {
        if self.settle_lane {
            self.shared.settles.push(channel, seqs, outcome);
        } else {
            let _ = self.send(QueueCmd::Settle { channel, seqs, outcome }).await;
        }
    }

    /// Return deliveries to the queue without incrementing their delivery count (AMQP 1.0
    /// `released`); `settle(.., Outcome::Requeue)` counts a failed delivery.
    pub async fn release(&self, channel: ChannelId, seqs: Vec<u64>) {
        let _ = self.send(QueueCmd::Release { channel, seqs }).await;
    }

    /// Settle with AMQP 1.0's `modified` outcome.
    pub async fn modify(&self, channel: ChannelId, seqs: Vec<u64>, modification: Modification) {
        let _ = self.send(QueueCmd::Modify { channel, seqs, modification: Box::new(modification) }).await;
    }

    pub async fn channel_closed(&self, channel: ChannelId) {
        let _ = self.send(QueueCmd::ChannelClosed { channel }).await;
    }

    /// Non-blocking nudge; coalesced so a flood of acks doesn't flood the mailbox.
    pub fn wake(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) && self.tx.try_send(QueueCmd::Wake).is_err() {
            self.wake_pending.store(false, Ordering::Release);
        }
    }

    /// Clear the pending-wake flag (actors call this when they handle `Wake`).
    pub fn woke(&self) {
        self.wake_pending.store(false, Ordering::Release);
    }

    pub async fn purge(&self) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::Purge { reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn lease(&self, max: usize, visibility_ms: u64) -> Result<Vec<Leased>> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::Lease { max, visibility_ms, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn settle_lease(&self, seq: u64, delivery_count: u32, outcome: Outcome) -> Result<bool> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::SettleLease { seq, delivery_count, outcome, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn extend_lease(&self, seq: u64, delivery_count: u32, visibility_ms: u64) -> Result<bool> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::ExtendLease { seq, delivery_count, visibility_ms, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn peek(&self, offset: usize, count: usize) -> Result<Vec<Peeked>> {
        let (reply, rx) = oneshot::channel();
        self.send(QueueCmd::Peek { offset, count, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn consumers(&self) -> Vec<ConsumerInfo> {
        let (reply, rx) = oneshot::channel();
        if self.send(QueueCmd::Consumers { reply }).await.is_err() {
            return vec![];
        }
        rx.await.unwrap_or_default()
    }

    pub async fn stop(&self) {
        let (reply, rx) = oneshot::channel();
        if self.send(QueueCmd::Stop { reply }).await.is_ok() {
            let _ = rx.await;
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.shared.deleted.load(Ordering::Acquire) || self.tx.is_closed()
    }
}

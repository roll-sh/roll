//! The classic/quorum queue actor.
//!
//! Commands are processed in batches: everything that is waiting in the mailbox is applied,
//! the resulting log records are appended in one write (group commit), and only then are
//! publisher confirms settled and messages delivered — so a delivered or confirmed persistent
//! message is always durable.

use super::*;
use crate::deadletter::{self, Reason};
use crate::error::{ACCESS_REFUSED, AmqpError};
use crate::log::{Durable, Log};
use crate::message::Message;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Weak;
use std::time::Duration;
use tokio::time::Instant;
use tracing::warn;

/// What a queue needs from its vhost.
pub trait QueueHost: Send + Sync + 'static {
    /// Republish a dead-lettered message through `exchange` (asynchronously).
    fn republish(&self, exchange: String, msg: Message, from_queue: String);
    /// The queue deleted itself (auto-delete, `x-expires`) or was deleted.
    fn queue_removed(&self, name: &str);
}

const MAILBOX: usize = 4096;
const BATCH: usize = 512;
const CONSUMER_TIMEOUT_CHECK: Duration = Duration::from_secs(5);
/// How long acks with nothing else to write wait for company before their log write, and how
/// many make the write worth it at once. A consumer acking one message at a time would
/// otherwise cost a log write per ack.
const SETTLE_DELAY: Duration = Duration::from_millis(10);
const SETTLE_GROUP: usize = 512;

// ── log records ──

pub(crate) enum Record {
    Enqueue { seq: u64, delivery_count: u32, msg: MessageRef },
    Settle { seqs: Vec<u64> },
    Returned { seq: u64, delivery_count: u32 },
    /// A requeued message changed by AMQP 1.0's `modified` outcome: replaces the logged one.
    Modified { seq: u64, delivery_count: u32, msg: MessageRef },
}

impl Record {
    pub(crate) fn encode(&self) -> Bytes {
        let mut b = BytesMut::new();
        match self {
            Record::Enqueue { seq, delivery_count, msg } => {
                b.reserve(13 + msg.encoded_len());
                b.put_u8(1);
                b.put_u64(*seq);
                b.put_u32(*delivery_count);
                msg.encode(&mut b);
            }
            Record::Settle { seqs } => {
                b.put_u8(2);
                b.put_u32(seqs.len() as u32);
                for s in seqs {
                    b.put_u64(*s);
                }
            }
            Record::Returned { seq, delivery_count } => {
                b.put_u8(3);
                b.put_u64(*seq);
                b.put_u32(*delivery_count);
            }
            Record::Modified { seq, delivery_count, msg } => {
                b.reserve(13 + msg.encoded_len());
                b.put_u8(4);
                b.put_u64(*seq);
                b.put_u32(*delivery_count);
                msg.encode(&mut b);
            }
        }
        b.freeze()
    }

    pub(crate) fn decode(mut b: Bytes) -> anyhow::Result<Self> {
        use crate::value::{get_u8, get_u32, get_u64};
        Ok(match get_u8(&mut b)? {
            1 => {
                let seq = get_u64(&mut b)?;
                let delivery_count = get_u32(&mut b)?;
                Record::Enqueue { seq, delivery_count, msg: Arc::new(Message::decode(&mut b)?) }
            }
            2 => {
                let n = get_u32(&mut b)? as usize;
                if b.remaining() < n * 8 {
                    anyhow::bail!("truncated settle record");
                }
                Record::Settle { seqs: (0..n).map(|_| b.get_u64()).collect() }
            }
            3 => Record::Returned { seq: get_u64(&mut b)?, delivery_count: get_u32(&mut b)? },
            4 => {
                let seq = get_u64(&mut b)?;
                let delivery_count = get_u32(&mut b)?;
                Record::Modified { seq, delivery_count, msg: Arc::new(Message::decode(&mut b)?) }
            }
            t => anyhow::bail!("unknown log record type {t}"),
        })
    }
}

// ── state ──

#[derive(Clone)]
struct Entry {
    seq: u64,
    msg: MessageRef,
    prio: u8,
    expires_ms: Option<u64>,
    delivery_count: u32,
    redelivered: bool,
    size: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Holder {
    Consumer { channel: ChannelId, tag: Arc<str> },
    Get { channel: ChannelId },
    Lease { deadline_ms: u64 },
}

impl Holder {
    fn channel(&self) -> Option<ChannelId> {
        match self {
            Holder::Consumer { channel, .. } | Holder::Get { channel } => Some(*channel),
            Holder::Lease { .. } => None,
        }
    }
}

struct Unacked {
    entry: Entry,
    holder: Holder,
    delivered_ms: u64,
}

struct Consumer {
    spec: ConsumerSpec,
    active: bool,
}

pub(crate) struct Engine {
    meta: QueueMeta,
    config: QueueConfig,
    shared: Arc<QueueShared>,
    me: Weak<QueueHandle>,
    host: Arc<dyn QueueHost>,
    /// Ready messages by (inverted priority, seq): the first entry is the head.
    ready: BTreeMap<(u8, u64), Entry>,
    unacked: HashMap<u64, Unacked>,
    leases: BTreeSet<(u64, u64)>,
    consumers: Vec<Consumer>,
    rr: usize,
    next_seq: u64,
    ready_bytes: u64,
    unacked_bytes: u64,
    log: Option<Arc<dyn Log>>,
    /// seq → log index of its enqueue record, and the reverse (for compaction).
    persisted: HashMap<u64, u64>,
    live_idx: BTreeMap<u64, u64>,
    pending_records: Vec<Record>,
    pending_seqs: Vec<Option<u64>>,
    /// Seqs whose enqueue record is in `pending_records`, and those of them that already left
    /// the queue (classic queues deliver before the flush): they get a settle once written.
    pending_enq: HashSet<u64>,
    forgotten: Vec<u64>,
    pending_settles: Vec<u64>,
    /// When the oldest of `pending_settles` gets written even without other records.
    settles_due: Option<Instant>,
    pending_confirms: Vec<(ConfirmTicket, bool)>,
    pending_replies: Vec<(oneshot::Sender<PublishResult>, PublishResult)>,
    /// Classic queues' writes waiting for their fsync, oldest first, with the confirms and
    /// replies that wait for them. The queue keeps working meanwhile.
    inflight: VecDeque<Inflight>,
    dedup: HashMap<String, u64>,
    dedup_order: VecDeque<String>,
    last_used: Instant,
    had_consumer: bool,
    timed_out_channels: HashSet<ChannelId>,
    next_timeout_check: Instant,
    deleted: bool,
    stopped: bool,
    /// Ready keys of messages published with a TTL of 0 in this batch: unless a consumer takes
    /// them right away they expire (RabbitMQ semantics).
    zero_ttl: Vec<(u8, u64)>,
}

/// A write on its way to disk and what waits for it.
struct Inflight {
    durable: Durable,
    confirms: Vec<(ConfirmTicket, bool)>,
    replies: Vec<(oneshot::Sender<PublishResult>, PublishResult)>,
}

/// Writes a classic queue may have waiting for fsync before it waits for the oldest.
const MAX_INFLIGHT: usize = 64;

/// Start a queue actor, replaying its log first.
pub async fn spawn(meta: QueueMeta, config: QueueConfig, log: Option<Arc<dyn Log>>, host: Arc<dyn QueueHost>) -> anyhow::Result<QueueRef> {
    let (tx, rx) = mpsc::channel(MAILBOX);
    let shared = Arc::new(QueueShared::default());
    let handle = Arc::new(QueueHandle::new(meta.clone(), config.kind, shared.clone(), tx).with_settle_lane());
    let mut engine = Engine {
        meta,
        config,
        shared,
        me: Arc::downgrade(&handle),
        host,
        ready: BTreeMap::new(),
        unacked: HashMap::new(),
        leases: BTreeSet::new(),
        consumers: Vec::new(),
        rr: 0,
        next_seq: 0,
        ready_bytes: 0,
        unacked_bytes: 0,
        log,
        persisted: HashMap::new(),
        live_idx: BTreeMap::new(),
        pending_records: Vec::new(),
        pending_seqs: Vec::new(),
        pending_enq: HashSet::new(),
        forgotten: Vec::new(),
        pending_settles: Vec::new(),
        settles_due: None,
        pending_confirms: Vec::new(),
        pending_replies: Vec::new(),
        inflight: VecDeque::new(),
        dedup: HashMap::new(),
        dedup_order: VecDeque::new(),
        last_used: Instant::now(),
        had_consumer: false,
        timed_out_channels: HashSet::new(),
        next_timeout_check: Instant::now() + CONSUMER_TIMEOUT_CHECK,
        deleted: false,
        stopped: false,
        zero_ttl: Vec::new(),
    };
    engine.recover().await?;
    engine.publish_stats();
    tokio::spawn(engine.run(rx));
    Ok(handle)
}

impl Engine {
    async fn recover(&mut self) -> anyhow::Result<()> {
        let Some(log) = self.log.clone() else { return Ok(()) };
        let (first, next) = log.bounds();
        let mut live: BTreeMap<u64, (Entry, u64)> = BTreeMap::new();
        let mut at = first;
        while at < next {
            let batch = log.read(at, 4096, 32 << 20).await?;
            if batch.is_empty() {
                break;
            }
            for (idx, data) in batch {
                at = idx + 1;
                match Record::decode(data)? {
                    Record::Enqueue { seq, delivery_count, msg } => {
                        let e = self.entry(seq, msg, delivery_count);
                        live.insert(seq, (e, idx));
                    }
                    Record::Settle { seqs } => {
                        for s in seqs {
                            live.remove(&s);
                        }
                    }
                    Record::Returned { seq, delivery_count } => {
                        if let Some((e, _)) = live.get_mut(&seq) {
                            e.delivery_count = delivery_count;
                        }
                    }
                    Record::Modified { seq, delivery_count, msg } => {
                        if let Some((e, _)) = live.get_mut(&seq) {
                            *e = self.entry(seq, msg, delivery_count);
                        }
                    }
                }
            }
        }
        tracing::debug!(queue = %self.meta.name, first, next, live = live.len(), lowest = ?live.keys().next(), highest = ?live.keys().next_back(), "recovered queue");
        for (seq, (mut e, idx)) in live {
            // We don't log deliveries, so anything recovered may have been seen before.
            e.redelivered = true;
            self.next_seq = self.next_seq.max(seq + 1);
            self.persisted.insert(seq, idx);
            self.live_idx.insert(idx, seq);
            self.track_dedup(&e.msg, seq);
            self.ready_bytes += e.size;
            self.ready.insert((e.prio, seq), e);
        }
        Ok(())
    }

    fn entry(&self, seq: u64, msg: MessageRef, delivery_count: u32) -> Entry {
        let prio = match (self.config.max_priority, self.config.kind) {
            (Some(max), _) => 255 - msg.props.priority.unwrap_or(0).min(max),
            // Quorum queues have two priorities without configuration: >4 is high.
            (None, QueueType::Quorum) if msg.props.priority.unwrap_or(0) > 4 => 254,
            _ => 255,
        };
        let ttl = match (self.config.message_ttl, msg.props.ttl_ms()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let expires_ms = ttl.map(|t| msg.received_ms.max(crate::now_ms().saturating_sub(1)) + t);
        Entry { seq, size: msg.size() as u64, msg, prio, expires_ms, delivery_count, redelivered: delivery_count > 0 }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<QueueCmd>) {
        // Lives on the stack for one turn of the loop; boxing the command would cost an
        // allocation per message.
        #[allow(clippy::large_enum_variant)]
        enum Woke {
            Cmd(Option<QueueCmd>),
            Durable(anyhow::Result<()>),
            Settles,
            Timer,
        }
        let shared = self.shared.clone();
        loop {
            let deadline = self.next_deadline();
            let woke = {
                let oldest = async {
                    match self.inflight.front_mut() {
                        Some(f) => (&mut f.durable).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    biased;
                    _ = shared.settles.wake.notified() => Woke::Settles,
                    c = rx.recv() => Woke::Cmd(c),
                    r = oldest => Woke::Durable(r),
                    _ = tokio::time::sleep_until(deadline) => Woke::Timer,
                }
            };
            let cmd = match woke {
                Woke::Cmd(c) => c,
                Woke::Durable(r) => {
                    self.settle_oldest(r);
                    continue;
                }
                Woke::Settles => {
                    self.take_settles().await;
                    self.after_batch().await;
                    if self.deleted || self.stopped {
                        break;
                    }
                    continue;
                }
                Woke::Timer => {
                    self.on_timer();
                    self.after_batch().await;
                    if self.deleted || self.stopped {
                        break;
                    }
                    continue;
                }
            };
            let Some(cmd) = cmd else { break };
            self.take_settles().await;
            self.handle(cmd).await;
            let mut n = 0;
            while n < BATCH && !self.deleted && !self.stopped {
                match rx.try_recv() {
                    Ok(cmd) => {
                        self.take_settles().await;
                        self.handle(cmd).await
                    }
                    Err(_) => break,
                }
                n += 1;
            }
            self.take_settles().await;
            self.after_batch().await;
            if self.deleted || self.stopped {
                break;
            }
        }
        if !self.deleted {
            // A stopping queue keeps the acks it was sent.
            self.take_settles().await;
            self.flush_all().await;
        }
        self.drain_inflight().await;
        if self.deleted {
            // Tell anyone still waiting that the queue is gone.
            rx.close();
            while let Ok(cmd) = rx.try_recv() {
                self.reject_after_delete(cmd);
            }
        }
    }

    /// Settles from the lane in [`QueueShared::settles`], handled as if from the mailbox.
    async fn take_settles(&mut self) {
        for (channel, seqs, outcome) in self.shared.settles.take() {
            if self.deleted {
                return;
            }
            self.handle(QueueCmd::Settle { channel, seqs, outcome }).await;
        }
    }

    async fn after_batch(&mut self) {
        // Classic queues hand messages to consumers before they are on disk, like RabbitMQ's:
        // publishers are still confirmed only once they are, so a crash can redeliver (the
        // publisher retries) but never loses a confirmed message. Quorum queues deliver only
        // what is durable.
        if self.config.kind != QueueType::Quorum && !self.deleted {
            self.deliver();
        }
        self.flush().await;
        if self.deleted {
            return;
        }
        self.deliver();
        self.expire_zero_ttl();
        self.enforce_limits();
        self.flush().await;
        self.maybe_compact().await;
        self.publish_stats();
    }

    fn reject_after_delete(&self, cmd: QueueCmd) {
        match cmd {
            QueueCmd::Publish { confirm, reply, .. } => {
                // Like RabbitMQ: a publish racing a delete is simply dropped (and confirmed).
                if let Some(c) = confirm {
                    c.settle(true);
                }
                if let Some(r) = reply {
                    let _ = r.send(PublishResult::Accepted);
                }
            }
            QueueCmd::Consume { reply, .. } => {
                let _ = reply.send(Err(gone(&self.meta)));
            }
            QueueCmd::Get { reply, .. } => {
                let _ = reply.send(Err(gone(&self.meta)));
            }
            QueueCmd::Delete { reply, .. } => {
                let _ = reply.send(Ok(0));
            }
            _ => {}
        }
    }

    fn next_deadline(&self) -> Instant {
        let now_ms = crate::now_ms();
        let now = Instant::now();
        let at_ms = |ms: u64| now + Duration::from_millis(ms.saturating_sub(now_ms));
        let mut d = now + Duration::from_secs(3600);
        if let Some((_, e)) = self.ready.first_key_value()
            && let Some(exp) = e.expires_ms
        {
            d = d.min(at_ms(exp + 1));
        }
        if let Some((deadline, _)) = self.leases.first() {
            d = d.min(at_ms(*deadline));
        }
        if !self.unacked.is_empty() {
            d = d.min(self.next_timeout_check);
        }
        if let Some(due) = self.settles_due {
            d = d.min(due);
        }
        if let Some(exp) = self.config.expires
            && self.consumers.is_empty()
        {
            d = d.min(self.last_used + Duration::from_millis(exp));
        }
        d
    }

    fn on_timer(&mut self) {
        let now_ms = crate::now_ms();
        self.expire_head(now_ms);
        // Leases past their visibility timeout go back to the queue.
        while let Some(&(deadline, seq)) = self.leases.first() {
            if deadline > now_ms {
                break;
            }
            self.leases.pop_first();
            if self.unacked.get(&seq).is_some_and(|u| matches!(u.holder, Holder::Lease { deadline_ms } if deadline_ms == deadline)) {
                self.requeue(seq);
            }
        }
        if Instant::now() >= self.next_timeout_check {
            self.next_timeout_check = Instant::now() + CONSUMER_TIMEOUT_CHECK;
            self.check_consumer_timeouts(now_ms);
        }
        if let Some(exp) = self.config.expires
            && self.consumers.is_empty()
            && self.last_used.elapsed() >= Duration::from_millis(exp)
        {
            self.delete_self();
        }
    }

    fn check_consumer_timeouts(&mut self, now_ms: u64) {
        let timeout = self.config.consumer_timeout.unwrap_or(super::config::DEFAULT_CONSUMER_TIMEOUT_MS);
        let mut late: Vec<ChannelId> = self
            .unacked
            .values()
            .filter(|u| now_ms.saturating_sub(u.delivered_ms) > timeout)
            .filter_map(|u| u.holder.channel())
            .collect();
        late.sort_unstable();
        late.dedup();
        for ch in late {
            if !self.timed_out_channels.insert(ch) {
                continue;
            }
            if let Some(c) = self.consumers.iter().find(|c| c.spec.channel == ch) {
                warn!(queue = %self.meta.name, channel = ch, "consumer delivery acknowledgement timed out");
                c.spec.sink.timed_out(&c.spec.tag, timeout);
            }
        }
    }

    async fn handle(&mut self, cmd: QueueCmd) {
        match cmd {
            QueueCmd::Publish { msg, confirm, reply } => self.publish(msg, confirm, reply),
            QueueCmd::Consume { spec, reply } => {
                let _ = reply.send(self.add_consumer(*spec));
            }
            QueueCmd::Cancel { channel, tag, reply } => {
                let found = self.remove_consumer(channel, &tag);
                if let Some(r) = reply {
                    let _ = r.send(found);
                }
                self.maybe_auto_delete();
            }
            QueueCmd::Get { channel, no_ack, reply } => {
                self.flush().await;
                // Like RabbitMQ, earlier publishes reach waiting consumers (and expire/overflow)
                // before a basic.get in the same batch sees them.
                self.deliver();
                self.expire_zero_ttl();
                self.enforce_limits();
                self.last_used = Instant::now();
                let _ = reply.send(Ok(self.get(channel, no_ack)));
            }
            QueueCmd::Settle { channel, seqs, outcome } => {
                for seq in seqs {
                    if self.unacked.get(&seq).is_some_and(|u| u.holder.channel() == Some(channel)) {
                        self.settle(seq, outcome);
                    }
                }
            }
            QueueCmd::ChannelClosed { channel } => {
                self.consumers.retain(|c| c.spec.channel != channel);
                self.fix_active();
                self.timed_out_channels.remove(&channel);
                let held: Vec<u64> = self.held_by(channel);
                // Requeue in original order.
                for seq in held {
                    self.requeue(seq);
                }
                self.maybe_auto_delete();
            }
            QueueCmd::Release { channel, seqs } => {
                for seq in seqs {
                    if self.unacked.get(&seq).is_some_and(|u| u.holder.channel() == Some(channel)) {
                        self.release(seq);
                    }
                }
            }
            QueueCmd::Modify { channel, seqs, modification } => {
                for seq in seqs {
                    if self.unacked.get(&seq).is_some_and(|u| u.holder.channel() == Some(channel)) {
                        self.modify(seq, &modification);
                    }
                }
                self.maybe_auto_delete();
            }
            QueueCmd::Recover { channel } => {
                for seq in self.held_by(channel) {
                    self.requeue(seq);
                }
            }
            QueueCmd::Wake => {
                if let Some(me) = self.me.upgrade() {
                    me.woke();
                }
            }
            QueueCmd::Purge { reply } => {
                self.flush_all().await;
                let _ = reply.send(self.purge());
            }
            QueueCmd::Delete { if_unused, if_empty, reply } => {
                self.flush_all().await;
                self.drain_inflight().await;
                let r = if if_unused && !self.consumers.is_empty() {
                    Err(AmqpError::precondition(format!("queue '{}' in vhost '{}' in use", self.meta.name, self.meta.vhost)))
                } else if if_empty && !(self.ready.is_empty() && self.unacked.is_empty()) {
                    Err(AmqpError::precondition(format!("queue '{}' in vhost '{}' is not empty", self.meta.name, self.meta.vhost)))
                } else {
                    let n = self.ready.len() as u64;
                    self.delete_self();
                    Ok(n)
                };
                let _ = reply.send(r);
            }
            QueueCmd::Reconfigure { config } => {
                self.config = *config;
            }
            QueueCmd::Lease { max, visibility_ms, reply } => {
                self.flush().await;
                self.last_used = Instant::now();
                let _ = reply.send(self.lease(max, visibility_ms));
            }
            QueueCmd::SettleLease { seq, delivery_count, outcome, reply } => {
                let ok = self.unacked.get(&seq).is_some_and(|u| matches!(u.holder, Holder::Lease { .. }) && u.entry.delivery_count == delivery_count);
                if ok {
                    self.settle(seq, outcome);
                }
                let _ = reply.send(ok);
            }
            QueueCmd::ExtendLease { seq, delivery_count, visibility_ms, reply } => {
                let mut ok = false;
                if let Some(u) = self.unacked.get_mut(&seq)
                    && let Holder::Lease { deadline_ms } = &mut u.holder
                    && u.entry.delivery_count == delivery_count
                {
                    self.leases.remove(&(*deadline_ms, seq));
                    *deadline_ms = crate::now_ms() + visibility_ms;
                    self.leases.insert((*deadline_ms, seq));
                    ok = true;
                }
                let _ = reply.send(ok);
            }
            QueueCmd::Peek { offset, count, reply } => {
                let v = self
                    .ready
                    .values()
                    .skip(offset)
                    .take(count)
                    .map(|e| Peeked { seq: e.seq, msg: e.msg.clone(), redelivered: e.redelivered, delivery_count: e.delivery_count })
                    .collect();
                let _ = reply.send(v);
            }
            QueueCmd::Touch => self.last_used = Instant::now(),
            QueueCmd::Consumers { reply } => {
                // Callers read the shared counters next (declare-ok); make them current.
                self.publish_stats();
                let v = self
                    .consumers
                    .iter()
                    .map(|c| ConsumerInfo {
                        consumer_tag: c.spec.tag.to_string(),
                        channel: c.spec.channel,
                        connection: c.spec.conn,
                        ack_required: !c.spec.no_ack,
                        exclusive: c.spec.exclusive,
                        priority: c.spec.priority,
                        active: c.active,
                        arguments: c.spec.args.clone(),
                    })
                    .collect();
                let _ = reply.send(v);
            }
            QueueCmd::Stop { reply } => {
                self.flush_all().await;
                self.drain_inflight().await;
                for c in self.consumers.drain(..) {
                    c.spec.sink.cancelled(&c.spec.tag);
                }
                self.stopped = true;
                let _ = reply.send(());
            }
        }
    }

    fn held_by(&self, channel: ChannelId) -> Vec<u64> {
        let mut v: Vec<u64> = self.unacked.iter().filter(|(_, u)| u.holder.channel() == Some(channel)).map(|(s, _)| *s).collect();
        v.sort_unstable();
        v
    }

    // ── publish ──

    fn dedup_key(&self, msg: &Message) -> Option<String> {
        if !self.config.dedup {
            return None;
        }
        let v = msg.props.header("x-deduplication-header")?;
        Some(v.as_str().map(String::from).unwrap_or_else(|| v.to_json().to_string()))
    }

    fn track_dedup(&mut self, msg: &Message, seq: u64) {
        if let Some(k) = self.dedup_key(msg) {
            self.dedup.insert(k.clone(), seq);
            self.dedup_order.push_back(k);
            if let Some(max) = self.config.dedup_cache_size {
                while self.dedup_order.len() as u64 > max {
                    if let Some(old) = self.dedup_order.pop_front() {
                        self.dedup.remove(&old);
                    }
                }
            }
        }
    }

    fn publish(&mut self, msg: MessageRef, confirm: Option<ConfirmTicket>, reply: Option<oneshot::Sender<PublishResult>>) {
        self.shared.last_activity_ms.store(crate::now_ms() as i64, Ordering::Relaxed);
        if let Some(k) = self.dedup_key(&msg)
            && self.dedup.contains_key(&k)
        {
            self.finish_publish(confirm, reply, PublishResult::Duplicate, true);
            return;
        }
        // reject-publish decides at publish time from the ready count.
        if matches!(self.config.overflow, Overflow::RejectPublish | Overflow::RejectPublishDlx) && self.over_limit(1, msg.size() as u64) {
            if self.config.overflow == Overflow::RejectPublishDlx {
                self.dead_letter(&msg, Reason::MaxLen);
            }
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            self.finish_publish(confirm, reply, PublishResult::Rejected, false);
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let e = self.entry(seq, msg.clone(), 0);
        self.track_dedup(&msg, seq);
        let durable = self.log.is_some() && (msg.props.persistent() || self.config.kind == QueueType::Quorum);
        if durable {
            self.pending_records.push(Record::Enqueue { seq, delivery_count: 0, msg });
            self.pending_seqs.push(Some(seq));
            self.pending_enq.insert(seq);
        }
        if self.is_zero_ttl(&e.msg) {
            self.zero_ttl.push((e.prio, seq));
        }
        self.ready_bytes += e.size;
        self.ready.insert((e.prio, seq), e);
        self.shared.published.fetch_add(1, Ordering::Relaxed);
        self.finish_publish(confirm, reply, PublishResult::Accepted, true);
    }

    fn finish_publish(&mut self, confirm: Option<ConfirmTicket>, reply: Option<oneshot::Sender<PublishResult>>, r: PublishResult, ok: bool) {
        if let Some(c) = confirm {
            self.pending_confirms.push((c, ok));
        }
        if let Some(reply) = reply {
            self.pending_replies.push((reply, r));
        }
    }

    fn over_limit(&self, extra: u64, extra_bytes: u64) -> bool {
        let n = self.ready.len() as u64 + extra;
        let b = self.ready_bytes + extra_bytes;
        self.config.max_length.is_some_and(|m| n > m) || self.config.max_length_bytes.is_some_and(|m| b > m)
    }

    fn enforce_limits(&mut self) {
        if self.config.overflow != Overflow::DropHead {
            return;
        }
        while !self.ready.is_empty() && self.over_limit(0, 0) {
            let (_, e) = self.ready.pop_first().unwrap();
            self.ready_bytes -= e.size;
            self.dead_letter(&e.msg, Reason::MaxLen);
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            self.forget(e.seq, &e.msg);
        }
    }

    /// Persist pending records, then settle confirms and publish replies.
    async fn flush(&mut self) {
        if !self.pending_settles.is_empty() {
            let now = Instant::now();
            let due = *self.settles_due.get_or_insert(now + SETTLE_DELAY);
            if !self.pending_records.is_empty() || self.pending_settles.len() >= SETTLE_GROUP || now >= due {
                self.settles_due = None;
                let seqs = std::mem::take(&mut self.pending_settles);
                self.pending_records.push(Record::Settle { seqs });
                self.pending_seqs.push(None);
            }
        }
        let mut ok = true;
        self.pending_enq.clear();
        if !self.pending_records.is_empty() {
            let records = std::mem::take(&mut self.pending_records);
            let seqs = std::mem::take(&mut self.pending_seqs);
            let log = self.log.clone().expect("records without a log");
            let data: Vec<Bytes> = records.iter().map(Record::encode).collect();
            // Acks alone needn't wait for fsync: losing one only means a redelivery, and the
            // next durable append (or segment close) persists it in order.
            let relaxed = seqs.iter().all(Option::is_none) && self.pending_confirms.is_empty() && self.pending_replies.is_empty();
            // Classic queues don't wait for the fsync: the records get their place in the log
            // now, and confirms go out when it lands. Quorum queues deliver only what is
            // durable, so they wait.
            let pipelined = !relaxed && self.config.kind != QueueType::Quorum;
            let started = std::time::Instant::now();
            let appended = if relaxed {
                log.append_relaxed(data).await
            } else if pipelined {
                if self.inflight.len() >= MAX_INFLIGHT
                    && let Some(f) = self.inflight.front_mut()
                {
                    let r = (&mut f.durable).await;
                    self.settle_oldest(r);
                }
                match log.append_pipelined(data).await {
                    Ok((first, durable)) => {
                        let confirms = std::mem::take(&mut self.pending_confirms);
                        let replies = std::mem::take(&mut self.pending_replies);
                        self.inflight.push_back(Inflight { durable, confirms, replies });
                        Ok(first)
                    }
                    Err(e) => Err(e),
                }
            } else {
                log.append(data).await
            };
            if started.elapsed() > std::time::Duration::from_millis(100) {
                warn!(queue = %self.meta.name, ms = started.elapsed().as_millis() as u64, records = seqs.len(), relaxed, "slow log append");
            }
            match appended {
                Ok(first) => {
                    for (i, seq) in seqs.into_iter().enumerate() {
                        if let Some(seq) = seq {
                            let idx = first + i as u64;
                            self.persisted.insert(seq, idx);
                            self.live_idx.insert(idx, seq);
                        }
                    }
                    // Consumed before their enqueue was written: settle them in the next write.
                    for seq in self.forgotten.drain(..) {
                        if let Some(idx) = self.persisted.remove(&seq) {
                            self.live_idx.remove(&idx);
                            self.pending_settles.push(seq);
                        }
                    }
                    self.shared.log_bytes.store(log.bytes(), Ordering::Relaxed);
                }
                Err(e) => {
                    // Without durability we can't confirm; publishers get nacks and retry.
                    warn!(queue = %self.meta.name, error = %e, "log append failed");
                    self.forgotten.clear();
                    ok = false;
                }
            }
        }
        for (c, success) in self.pending_confirms.drain(..) {
            c.settle(success && ok);
        }
        for (r, res) in self.pending_replies.drain(..) {
            let _ = r.send(if ok { res } else { PublishResult::Rejected });
        }
    }

    /// [`Engine::flush`], acks included (stopping, deleting, purging).
    async fn flush_all(&mut self) {
        if !self.pending_settles.is_empty() {
            self.settles_due = Some(Instant::now());
        }
        self.flush().await;
    }

    /// The oldest in-flight write finished: settle what waited for it.
    fn settle_oldest(&mut self, r: anyhow::Result<()>) {
        let Some(f) = self.inflight.pop_front() else { return };
        if let Err(e) = &r {
            // Without durability we can't confirm; publishers get nacks and retry.
            warn!(queue = %self.meta.name, error = %e, "log append failed");
        }
        let ok = r.is_ok();
        for (c, success) in f.confirms {
            c.settle(success && ok);
        }
        for (reply, res) in f.replies {
            let _ = reply.send(if ok { res } else { PublishResult::Rejected });
        }
    }

    /// Wait for every in-flight write (before stopping, deleting or handing the queue off).
    async fn drain_inflight(&mut self) {
        while let Some(f) = self.inflight.front_mut() {
            let r = (&mut f.durable).await;
            self.settle_oldest(r);
        }
    }

    /// Drop log segments nobody needs, copying long-lived messages forward when they pin old
    /// segments.
    async fn maybe_compact(&mut self) {
        let Some(log) = self.log.clone() else { return };
        let (first, next) = log.bounds();
        let oldest_live = self.live_idx.first_key_value().map(|(i, _)| *i).unwrap_or(next);
        if oldest_live > first {
            if let Ok(f) = log.truncate_prefix(oldest_live).await
                && f != first
            {
                self.shared.log_bytes.store(log.bytes(), Ordering::Relaxed);
            }
            return;
        }
        // Pinned: if the log is mostly garbage, rewrite the oldest live messages at the tail.
        let live_bytes = self.ready_bytes + self.unacked_bytes;
        if log.bytes() < (64 << 20).max(live_bytes * 4) {
            return;
        }
        let horizon = first + (next - first) / 2;
        let movers: Vec<u64> = self.live_idx.range(..horizon).map(|(_, s)| *s).collect();
        for seq in movers {
            let entry = self.ready.values().find(|e| e.seq == seq).cloned().or_else(|| self.unacked.get(&seq).map(|u| u.entry.clone()));
            if let Some(e) = entry {
                if let Some(old) = self.persisted.remove(&seq) {
                    self.live_idx.remove(&old);
                }
                self.pending_records.push(Record::Enqueue { seq, delivery_count: e.delivery_count, msg: e.msg });
                self.pending_seqs.push(Some(seq));
                self.pending_enq.insert(seq);
            }
        }
        self.flush().await;
        let oldest_live = self.live_idx.first_key_value().map(|(i, _)| *i).unwrap_or(next);
        let _ = log.truncate_prefix(oldest_live).await;
        self.shared.log_bytes.store(log.bytes(), Ordering::Relaxed);
    }

    // ── delivery ──

    fn add_consumer(&mut self, spec: ConsumerSpec) -> Result<()> {
        if self.consumers.iter().any(|c| c.spec.exclusive) {
            return Err(AmqpError::new(ACCESS_REFUSED, format!("queue '{}' in vhost '{}' in exclusive use", self.meta.name, self.meta.vhost)));
        }
        if spec.exclusive && !self.consumers.is_empty() {
            return Err(AmqpError::new(ACCESS_REFUSED, format!("queue '{}' in vhost '{}' in exclusive use", self.meta.name, self.meta.vhost)));
        }
        let active = !self.config.single_active_consumer || self.consumers.is_empty();
        if self.config.single_active_consumer {
            spec.sink.activated(&spec.tag, active);
        }
        // Keep consumers ordered by priority (stable within a priority).
        let pos = self.consumers.iter().position(|c| c.spec.priority < spec.priority).unwrap_or(self.consumers.len());
        self.consumers.insert(pos, Consumer { spec, active });
        self.had_consumer = true;
        self.last_used = Instant::now();
        Ok(())
    }

    fn remove_consumer(&mut self, channel: ChannelId, tag: &str) -> bool {
        let before = self.consumers.len();
        self.consumers.retain(|c| !(c.spec.channel == channel && &*c.spec.tag == tag));
        let found = self.consumers.len() != before;
        if found {
            self.fix_active();
            self.last_used = Instant::now();
        }
        found
    }

    /// Single active consumer: promote the next consumer when the active one leaves.
    fn fix_active(&mut self) {
        if !self.config.single_active_consumer || self.consumers.iter().any(|c| c.active) {
            return;
        }
        // RabbitMQ picks the highest-priority consumer, earliest registered first.
        if let Some(c) = self.consumers.first_mut() {
            c.active = true;
            c.spec.sink.activated(&c.spec.tag, true);
        }
    }

    fn maybe_auto_delete(&mut self) {
        if self.meta.auto_delete && self.had_consumer && self.consumers.is_empty() {
            self.delete_self();
        }
    }

    /// Pick the next consumer with credit: highest priority first, round-robin within it.
    fn pick(&mut self) -> Option<usize> {
        let n = self.consumers.len();
        if n == 0 {
            return None;
        }
        let mut best: Option<usize> = None;
        for k in 0..n {
            let i = (self.rr + k) % n;
            let c = &self.consumers[i];
            if !c.active || !c.spec.sink.has_credit() {
                continue;
            }
            match best {
                Some(b) if self.consumers[b].spec.priority >= c.spec.priority => {}
                _ => best = Some(i),
            }
            if c.spec.priority == self.consumers[0].spec.priority {
                break; // nothing can beat the top priority
            }
        }
        if let Some(i) = best {
            self.rr = (i + 1) % n;
        }
        best
    }

    fn pop_ready(&mut self, now_ms: u64) -> Option<Entry> {
        loop {
            let (_, e) = self.ready.pop_first()?;
            self.ready_bytes -= e.size;
            if e.expires_ms.is_some_and(|x| x < now_ms) {
                self.dead_letter(&e.msg, Reason::Expired);
                self.forget(e.seq, &e.msg);
                continue;
            }
            return Some(e);
        }
    }

    fn deliver(&mut self) {
        if self.consumers.is_empty() || self.ready.is_empty() {
            self.expire_head(crate::now_ms());
            return;
        }
        let Some(me) = self.me.upgrade() else { return };
        let now_ms = crate::now_ms();
        let mut delivered = 0u64;
        while !self.ready.is_empty() {
            let Some(i) = self.pick() else { break };
            let Some(e) = self.pop_ready(now_ms) else { break };
            let c = &self.consumers[i];
            let d = Delivery {
                queue: me.clone(),
                consumer_tag: c.spec.tag.clone(),
                seq: e.seq,
                msg: e.msg.clone(),
                redelivered: e.redelivered,
                delivery_count: e.delivery_count,
                remaining: 0,
            };
            let sink = c.spec.sink.clone();
            if c.spec.no_ack {
                self.forget(e.seq, &e.msg);
            } else {
                let holder = Holder::Consumer { channel: c.spec.channel, tag: c.spec.tag.clone() };
                self.unacked_bytes += e.size;
                self.unacked.insert(e.seq, Unacked { entry: e, holder, delivered_ms: now_ms });
            }
            sink.deliver(d);
            delivered += 1;
        }
        if delivered > 0 {
            self.shared.delivered.fetch_add(delivered, Ordering::Relaxed);
            self.shared.last_activity_ms.store(now_ms as i64, Ordering::Relaxed);
        }
        self.expire_head(now_ms);
    }

    fn expire_head(&mut self, now_ms: u64) {
        while let Some((_, e)) = self.ready.first_key_value() {
            if !e.expires_ms.is_some_and(|x| x < now_ms) {
                break;
            }
            let (_, e) = self.ready.pop_first().unwrap();
            self.ready_bytes -= e.size;
            self.dead_letter(&e.msg, Reason::Expired);
            self.forget(e.seq, &e.msg);
        }
    }

    fn is_zero_ttl(&self, msg: &Message) -> bool {
        self.config.message_ttl == Some(0) || msg.props.ttl_ms() == Some(0)
    }

    /// Zero-TTL messages no consumer took at once expire.
    fn expire_zero_ttl(&mut self) {
        for key in std::mem::take(&mut self.zero_ttl) {
            if let Some(e) = self.ready.remove(&key) {
                self.ready_bytes -= e.size;
                self.dead_letter(&e.msg, Reason::Expired);
                self.forget(e.seq, &e.msg);
            }
        }
    }

    fn get(&mut self, channel: ChannelId, no_ack: bool) -> Option<Delivery> {
        let me = self.me.upgrade()?;
        let now_ms = crate::now_ms();
        let e = self.pop_ready(now_ms)?;
        self.shared.got.fetch_add(1, Ordering::Relaxed);
        let d = Delivery {
            queue: me,
            consumer_tag: Arc::from(""),
            seq: e.seq,
            msg: e.msg.clone(),
            redelivered: e.redelivered,
            delivery_count: e.delivery_count,
            remaining: self.ready.len() as u32,
        };
        if no_ack {
            self.forget(e.seq, &e.msg);
        } else {
            self.unacked_bytes += e.size;
            self.unacked.insert(e.seq, Unacked { entry: e, holder: Holder::Get { channel }, delivered_ms: now_ms });
        }
        Some(d)
    }

    fn lease(&mut self, max: usize, visibility_ms: u64) -> Vec<Leased> {
        let now_ms = crate::now_ms();
        let deadline_ms = now_ms + visibility_ms;
        let mut out = Vec::new();
        while out.len() < max {
            let Some(e) = self.pop_ready(now_ms) else { break };
            out.push(Leased { seq: e.seq, msg: e.msg.clone(), delivery_count: e.delivery_count, redelivered: e.redelivered, deadline_ms });
            self.leases.insert((deadline_ms, e.seq));
            self.unacked_bytes += e.size;
            self.unacked.insert(e.seq, Unacked { entry: e, holder: Holder::Lease { deadline_ms }, delivered_ms: now_ms });
        }
        self.shared.got.fetch_add(out.len() as u64, Ordering::Relaxed);
        out
    }

    // ── settlement ──

    fn take_unacked(&mut self, seq: u64) -> Option<Unacked> {
        let u = self.unacked.remove(&seq)?;
        self.unacked_bytes -= u.entry.size;
        if let Holder::Lease { deadline_ms } = u.holder {
            self.leases.remove(&(deadline_ms, seq));
        }
        Some(u)
    }

    fn settle(&mut self, seq: u64, outcome: Outcome) {
        match outcome {
            Outcome::Ack => {
                if let Some(u) = self.take_unacked(seq) {
                    self.shared.acked.fetch_add(1, Ordering::Relaxed);
                    self.forget(seq, &u.entry.msg);
                }
            }
            Outcome::Requeue => self.requeue(seq),
            Outcome::Reject => {
                if let Some(u) = self.take_unacked(seq) {
                    self.dead_letter(&u.entry.msg, Reason::Rejected);
                    self.forget(seq, &u.entry.msg);
                }
            }
        }
    }

    /// Back to the queue without counting a failed delivery.
    fn release(&mut self, seq: u64) {
        let Some(u) = self.take_unacked(seq) else { return };
        let mut e = u.entry;
        e.redelivered = true;
        self.shared.redelivered.fetch_add(1, Ordering::Relaxed);
        self.ready_bytes += e.size;
        self.ready.insert((e.prio, seq), e);
    }

    /// AMQP 1.0 `modified`: merge the annotations, then dead-letter, requeue (counting a failed
    /// delivery if asked) or release. The changed message is logged so it survives a restart.
    fn modify(&mut self, seq: u64, m: &Modification) {
        let Some(u) = self.unacked.get_mut(&seq) else { return };
        let prio = u.entry.prio;
        if !m.annotations.is_empty() {
            let msg = &u.entry.msg;
            let mut props = msg.props.clone();
            let headers = props.headers.get_or_insert_with(FieldTable::new);
            for (k, v) in m.annotations.iter() {
                headers.insert(k, v.clone());
            }
            u.entry.msg = Arc::new(msg.with_props(&msg.exchange, &msg.routing_key, props));
        }
        if m.undeliverable_here {
            return self.settle(seq, Outcome::Reject);
        }
        if m.delivery_failed {
            self.requeue(seq);
        } else {
            self.release(seq);
        }
        if !m.annotations.is_empty()
            && self.persisted.contains_key(&seq)
            && let Some(e) = self.ready.get(&(prio, seq))
        {
            self.pending_records.push(Record::Modified { seq, delivery_count: e.delivery_count, msg: e.msg.clone() });
            self.pending_seqs.push(None);
        }
    }

    fn requeue(&mut self, seq: u64) {
        let Some(u) = self.take_unacked(seq) else { return };
        let mut e = u.entry;
        if self.is_zero_ttl(&e.msg) {
            // Its TTL ran out while it was out: requeued messages expire (RabbitMQ).
            self.dead_letter(&e.msg, Reason::Expired);
            self.forget(seq, &e.msg);
            return;
        }
        e.delivery_count += 1;
        e.redelivered = true;
        if self.config.delivery_limit.is_some_and(|limit| e.delivery_count as u64 > limit) {
            self.dead_letter(&e.msg, Reason::DeliveryLimit);
            self.forget(seq, &e.msg);
            return;
        }
        if self.config.kind == QueueType::Quorum && self.persisted.contains_key(&seq) {
            self.pending_records.push(Record::Returned { seq, delivery_count: e.delivery_count });
            self.pending_seqs.push(None);
        }
        self.shared.redelivered.fetch_add(1, Ordering::Relaxed);
        self.ready_bytes += e.size;
        self.ready.insert((e.prio, seq), e);
    }

    /// A message left the queue for good.
    fn forget(&mut self, seq: u64, msg: &Message) {
        if let Some(idx) = self.persisted.remove(&seq) {
            self.live_idx.remove(&idx);
            self.pending_settles.push(seq);
        } else if self.pending_enq.remove(&seq) {
            self.forgotten.push(seq);
        }
        if let Some(k) = self.dedup_key(msg)
            && self.dedup.get(&k) == Some(&seq)
            && self.config.dedup_cache_size.is_none()
        {
            self.dedup.remove(&k);
        }
    }

    fn dead_letter(&mut self, msg: &Message, reason: Reason) {
        let Some(dlx) = self.config.dead_letter_exchange.clone() else {
            return;
        };
        let dl = deadletter::dead_letter(msg, &self.meta.name, reason, &dlx, self.config.dead_letter_routing_key.as_deref());
        self.shared.dead_lettered.fetch_add(1, Ordering::Relaxed);
        self.host.republish(dlx, dl, self.meta.name.clone());
    }

    fn purge(&mut self) -> u64 {
        let n = self.ready.len() as u64;
        let ready = std::mem::take(&mut self.ready);
        self.ready_bytes = 0;
        for (_, e) in ready {
            self.forget(e.seq, &e.msg);
        }
        n
    }

    fn delete_self(&mut self) {
        if self.deleted {
            return;
        }
        self.deleted = true;
        self.shared.deleted.store(true, Ordering::Release);
        for c in self.consumers.drain(..) {
            c.spec.sink.cancelled(&c.spec.tag);
        }
        self.ready.clear();
        self.unacked.clear();
        self.pending_records.clear();
        self.pending_settles.clear();
        self.settles_due = None;
        self.ready_bytes = 0;
        self.unacked_bytes = 0;
        if let Some(log) = self.log.take() {
            tokio::spawn(async move {
                let _ = log.destroy().await;
            });
        }
        self.publish_stats();
        self.host.queue_removed(&self.meta.name);
        self.shared.available.notify_waiters();
    }

    fn publish_stats(&self) {
        let s = &self.shared;
        s.ready.store(self.ready.len() as u64, Ordering::Relaxed);
        s.unacked.store(self.unacked.len() as u64, Ordering::Relaxed);
        s.ready_bytes.store(self.ready_bytes, Ordering::Relaxed);
        s.unacked_bytes.store(self.unacked_bytes, Ordering::Relaxed);
        s.consumers.store(self.consumers.len() as u64, Ordering::Relaxed);
        if !self.ready.is_empty() {
            s.available.notify_waiters();
        }
    }
}

fn gone(q: &QueueMeta) -> AmqpError {
    AmqpError::not_found(format!("queue '{}' in vhost '{}'", q.name, q.vhost))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::MemLog;
    use crate::message::Properties;
    use async_trait::async_trait;
    use tokio::sync::watch;

    /// A log whose writes become durable only when the test says so.
    struct GatedLog {
        mem: MemLog,
        open: watch::Sender<u32>,
        fail: std::sync::atomic::AtomicBool,
        started: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl Log for GatedLog {
        async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
            self.mem.append(entries).await
        }
        async fn append_pipelined(&self, entries: Vec<Bytes>) -> anyhow::Result<(u64, Durable)> {
            let at = self.mem.append(entries).await?;
            let mut open = self.open.subscribe();
            let want = *open.borrow() + 1;
            let fail = self.fail.load(Ordering::Relaxed);
            self.started.fetch_add(1, Ordering::SeqCst);
            Ok((
                at,
                Box::pin(async move {
                    open.wait_for(|n| *n >= want).await?;
                    if fail { anyhow::bail!("disk on fire") } else { Ok(()) }
                }),
            ))
        }
        async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>> {
            self.mem.read(from, max, max_bytes).await
        }
        fn bounds(&self) -> (u64, u64) {
            self.mem.bounds()
        }
        async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64> {
            self.mem.truncate_prefix(before).await
        }
        fn bytes(&self) -> u64 {
            self.mem.bytes()
        }
        async fn destroy(&self) -> anyhow::Result<()> {
            self.mem.destroy().await
        }
    }

    struct NoHost;
    impl QueueHost for NoHost {
        fn republish(&self, _: String, _: Message, _: String) {}
        fn queue_removed(&self, _: &str) {}
    }

    fn persistent(body: &'static str) -> MessageRef {
        let props = Properties { delivery_mode: Some(2), ..Default::default() };
        Arc::new(Message::new("", "q", props, Bytes::from_static(body.as_bytes())))
    }

    fn ticket() -> (ConfirmTicket, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        (ConfirmTicket::new(move |ok| {
            let _ = tx.send(ok);
        }), rx)
    }

    #[tokio::test]
    async fn classic_queues_keep_serving_while_a_write_is_in_flight() {
        let log = Arc::new(GatedLog { mem: MemLog::new(1000), open: watch::channel(0).0, fail: false.into(), started: 0.into() });
        let meta = QueueMeta { name: "q".into(), vhost: "/".into(), durable: true, auto_delete: false, exclusive_owner: None, arguments: FieldTable::new(), created_at: 0 };
        let q = spawn(meta, QueueConfig::default(), Some(log.clone() as Arc<dyn Log>), Arc::new(NoHost)).await.unwrap();

        let (t, mut confirmed) = ticket();
        q.publish(persistent("one"), Some(t)).await.unwrap();
        // The fsync hasn't landed: no confirm yet, but the queue still hands the message out.
        let got = tokio::time::timeout(Duration::from_secs(2), q.get(1, true)).await.expect("get timed out").unwrap().expect("delivered before it is durable");
        assert_eq!(&got.msg.body[..], b"one");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(confirmed.try_recv().is_err(), "no confirm before the write is durable");

        // A second publish queues behind the first; both confirm in order once durable.
        let (t2, confirmed2) = ticket();
        q.publish(persistent("two"), Some(t2)).await.unwrap();
        while log.started.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        log.open.send_modify(|n| *n += 2);
        let wait = |rx| tokio::time::timeout(Duration::from_secs(2), rx);
        assert!(wait(confirmed).await.expect("first confirm").unwrap());
        assert!(wait(confirmed2).await.expect("second confirm").unwrap());

        // A failed fsync nacks.
        log.fail.store(true, Ordering::Relaxed);
        let (t3, confirmed3) = ticket();
        q.publish(persistent("three"), Some(t3)).await.unwrap();
        while log.started.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        log.open.send_modify(|n| *n += 1);
        assert!(!wait(confirmed3).await.expect("third confirm").unwrap(), "a write that never became durable is nacked");
    }

    #[tokio::test]
    async fn acks_skip_the_mailbox_but_stay_ahead_of_what_their_channel_sent_next() {
        let meta = QueueMeta { name: "q".into(), vhost: "/".into(), durable: false, auto_delete: false, exclusive_owner: None, arguments: FieldTable::new(), created_at: 0 };
        let q = spawn(meta, QueueConfig::default(), None, Arc::new(NoHost)).await.unwrap();
        let transient = |body: &'static str| Arc::new(Message::new("", "q", Properties::default(), Bytes::from_static(body.as_bytes())));
        q.publish(transient("one"), None).await.unwrap();
        q.publish(transient("two"), None).await.unwrap();
        let one = q.get(1, false).await.unwrap().unwrap();
        let two = q.get(1, false).await.unwrap().unwrap();

        // The ack takes the lane, the release the mailbox: the acked message must not come back.
        q.settle(1, vec![one.seq], Outcome::Ack).await;
        q.release(1, vec![one.seq, two.seq]).await;
        let back = q.get(2, true).await.unwrap().expect("the released message is back");
        assert_eq!(&back.msg.body[..], b"two");
        assert!(q.get(2, true).await.unwrap().is_none(), "the acked message stays gone");
        assert_eq!(q.shared.acked.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_lone_ack_waits_a_moment_for_company_then_reaches_the_log() {
        let log = Arc::new(MemLog::new(1000));
        let meta = QueueMeta { name: "q".into(), vhost: "/".into(), durable: true, auto_delete: false, exclusive_owner: None, arguments: FieldTable::new(), created_at: 0 };
        let q = spawn(meta, QueueConfig::default(), Some(log.clone() as Arc<dyn Log>), Arc::new(NoHost)).await.unwrap();
        let (t, confirmed) = ticket();
        q.publish(persistent("one"), Some(t)).await.unwrap();
        assert!(confirmed.await.unwrap());
        let got = q.get(1, false).await.unwrap().unwrap();
        assert_eq!(log.bounds().1, 1, "the enqueue");

        q.settle(1, vec![got.seq], Outcome::Ack).await;
        let deadline = Instant::now() + Duration::from_secs(2);
        while log.bounds().1 < 2 {
            assert!(Instant::now() < deadline, "the ack never reached the log");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

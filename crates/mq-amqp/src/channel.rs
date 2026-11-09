//! One AMQP channel: topology commands, publishing (confirms, transactions, returns),
//! consumers with prefetch, acks, and the error semantics RabbitMQ clients expect.

use crate::conn::{BACKLOG_HIGH, ConnShared, Ctl, Out};
use crate::method::Method;
use crate::reply_to;
use bytes::Bytes;
use mq_core::auth::Access;
use mq_core::broker::ChannelInfo;
use mq_core::error::*;
use mq_core::exchange::{Dest, ExchangeKind};
use mq_core::message::Message;
use mq_core::queue::{ChannelId, ConfirmTicket, ConsumerSink, ConsumerSpec, Delivery, Outcome, QueueMeta, QueueRef, QueueType};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::vhost::DeclareQueue;
use mq_core::{AmqpError, Result};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

pub(crate) struct Content {
    pub exchange: String,
    pub routing_key: String,
    pub mandatory: bool,
    pub immediate: bool,
    pub props: Bytes,
    pub body: Bytes,
}

pub(crate) enum ChannelCmd {
    Method(Method),
    Publish(Content),
    /// Raise a channel error (from the reader or a consumer sink).
    Fail(AmqpError, u16, u16),
    /// The broker cancelled a consumer.
    ConsumerGone(Arc<str>),
    /// The connection is going away.
    Shutdown,
    /// `basic.ack`s that were waiting together (tag, multiple), settled as one: the channel task
    /// builds this from consecutive acks in its mailbox.
    Acks(Vec<(u64, bool)>),
}

/// Most acks one [`ChannelCmd::Acks`] gathers.
const ACK_BATCH: usize = 1024;

struct Unacked {
    queue: QueueRef,
    seq: u64,
    consumer: Option<Arc<str>>,
}

#[derive(Default)]
struct Tags {
    next: u64,
    unacked: BTreeMap<u64, Unacked>,
    /// Deliveries that arrived after the global (per-channel) prefetch window filled: queues check
    /// credit concurrently, so one may overshoot. Sent as acks free the window.
    parked: VecDeque<Delivery>,
}

/// Channel state shared with consumer sinks (which run on queue actors).
pub(crate) struct ChanShared {
    pub number: u16,
    pub id: ChannelId,
    pub conn: Arc<ConnShared>,
    tags: Mutex<Tags>,
    pub global_prefetch: AtomicU32,
    pub unacked: AtomicU32,
    pub flow: AtomicBool,
    pub closing: AtomicBool,
    pub info: Arc<ChannelInfo>,
    /// Commands from sinks back to the channel task.
    pub ctl: mpsc::UnboundedSender<ChannelCmd>,
}

impl ChanShared {
    fn next_tag(&self) -> u64 {
        let mut t = self.tags.lock();
        t.next += 1;
        t.next
    }
}

pub(crate) struct Sink {
    chan: Arc<ChanShared>,
    no_ack: bool,
    prefetch: u32,
    unacked: AtomicU32,
    /// Held back until consume-ok is on the wire.
    ready: AtomicBool,
    cancelled: AtomicBool,
}

impl ConsumerSink for Sink {
    fn has_credit(&self) -> bool {
        let c = &self.chan;
        if !self.ready.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) || c.closing.load(Ordering::Acquire) || !c.flow.load(Ordering::Acquire) {
            return false;
        }
        if c.conn.backlog.load(Ordering::Acquire) >= BACKLOG_HIGH {
            c.conn.stalled.store(true, Ordering::Release);
            return false;
        }
        // A full global window holds back no-ack consumers too (RabbitMQ's limiter), though their
        // deliveries don't count against it.
        let g = c.global_prefetch.load(Ordering::Acquire);
        if g > 0 && c.unacked.load(Ordering::Acquire) >= g {
            return false;
        }
        self.no_ack || self.prefetch == 0 || self.unacked.load(Ordering::Acquire) < self.prefetch
    }

    fn deliver(&self, d: Delivery) {
        if let Err(d) = send_delivery(&self.chan, Some(self), d) {
            self.chan.tags.lock().parked.push_back(d);
        }
    }


    fn cancelled(&self, tag: &str) {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let c = &self.chan;
        if c.conn.has_capability("consumer_cancel_notify") && !c.closing.load(Ordering::Acquire) {
            c.conn.send(Out::Method(c.number, Method::BasicCancel { consumer_tag: tag.into(), nowait: true }));
        }
        let _ = c.ctl.send(ChannelCmd::ConsumerGone(Arc::from(tag)));
    }

    fn timed_out(&self, _tag: &str, timeout_ms: u64) {
        let e = AmqpError::precondition(format!(
            "delivery acknowledgement on channel {} timed out. Timeout value used: {timeout_ms} ms. This timeout value can be configured, see consumers doc guide to learn more",
            self.chan.number
        ));
        let _ = self.chan.ctl.send(ChannelCmd::Fail(e, 0, 0));
    }
}

/// Number a delivery and send it, unless it needs an ack and the channel's global prefetch window
/// is already full (checked and counted under the tags lock, so concurrent queues can't overshoot).
fn send_delivery(c: &ChanShared, sink: Option<&Sink>, d: Delivery) -> std::result::Result<(), Delivery> {
    let no_ack = sink.is_some_and(|s| s.no_ack);
    let tag = {
        let mut t = c.tags.lock();
        if !no_ack {
            let g = c.global_prefetch.load(Ordering::Acquire);
            if g > 0 && c.unacked.load(Ordering::Acquire) >= g {
                return Err(d);
            }
        }
        t.next += 1;
        let tag = t.next;
        if !no_ack {
            t.unacked.insert(tag, Unacked { queue: d.queue.clone(), seq: d.seq, consumer: Some(d.consumer_tag.clone()) });
            c.unacked.fetch_add(1, Ordering::AcqRel);
        }
        tag
    };
    if !no_ack {
        if let Some(s) = sink {
            s.unacked.fetch_add(1, Ordering::AcqRel);
        }
        c.info.unacked.fetch_add(1, Ordering::Relaxed);
    }
    c.info.delivered.fetch_add(1, Ordering::Relaxed);
    if d.redelivered {
        c.info.redelivered.fetch_add(1, Ordering::Relaxed);
    }
    let vs = &c.conn.vhost.stats;
    vs.delivered.fetch_add(1, Ordering::Relaxed);
    vs.delivered_bytes.fetch_add(d.msg.body.len() as u64, Ordering::Relaxed);
    c.conn.backlog.fetch_add(1, Ordering::AcqRel);
    c.conn.send(Out::Deliver { channel: c.number, tag, d });
    Ok(())
}

const DEATH_HEADERS: [&str; 7] = [
    "x-death",
    "x-first-death-reason",
    "x-first-death-queue",
    "x-first-death-exchange",
    "x-last-death-reason",
    "x-last-death-queue",
    "x-last-death-exchange",
];

/// RabbitMQ strips CR and LF from exchange and queue names on declare.
fn strip_cr_lf(name: String) -> String {
    if name.contains(['\r', '\n']) { name.replace(['\r', '\n'], "") } else { name }
}

/// Redeclaring with an argument of another type class (integer vs float, e.g. 10 vs 10.0) is
/// inequivalent in RabbitMQ even when the values compare equal.
fn check_arg_classes(current: &FieldTable, new: &FieldTable, queue: &str, vhost: &str) -> Result<()> {
    let float = |v: &FieldValue| matches!(v, FieldValue::F32(_) | FieldValue::F64(_) | FieldValue::Decimal(..));
    for (k, v) in new.iter() {
        if let Some(cur) = current.get(k)
            && float(cur) != float(v)
        {
            return Err(AmqpError::precondition(format!(
                "inequivalent arg '{k}' for queue '{queue}' in vhost '{vhost}': received the value '{}', current is '{}'",
                v.to_json(),
                cur.to_json()
            )));
        }
    }
    Ok(())
}

struct Consumer {
    queue: QueueRef,
    sink: Arc<Sink>,
    /// Direct reply-to consumers own a private queue.
    reply_key: Option<String>,
}

enum TxOp {
    Publish(Content),
    /// Acks/nacks are validated (and the tags taken) immediately, applied on commit.
    Settle(Vec<(u64, Unacked)>, Outcome),
}

pub(crate) struct Channel {
    shared: Arc<ChanShared>,
    conn: Arc<ConnShared>,
    consumers: HashMap<Arc<str>, Consumer>,
    last_queue: Option<String>,
    confirm_next: Option<u64>,
    tx: Option<Vec<TxOp>>,
    prefetch: u32,
    perm_ok: HashSet<(u8, bool, String)>,
    reply_key: Option<String>,
    closed: bool,
    /// Rotates which queue wakes first when credit frees (fairness across queues).
    wake_rr: std::sync::atomic::AtomicUsize,
}

impl Channel {
    pub fn spawn(conn: Arc<ConnShared>, number: u16) -> mpsc::Sender<ChannelCmd> {
        let (tx, mut rx) = mpsc::channel::<ChannelCmd>(256);
        let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
        let id = conn.broker.next_id();
        let info = Arc::new(ChannelInfo {
            id,
            conn: conn.info.id,
            number,
            conn_name: conn.info.name.clone(),
            vhost: conn.vhost.name.clone(),
            user: conn.user.name.clone(),
            prefetch_count: 0.into(),
            global_prefetch_count: 0.into(),
            unacked: 0.into(),
            consumers: 0.into(),
            confirm: false.into(),
            transactional: false.into(),
            published: 0.into(),
            delivered: 0.into(),
            acked: 0.into(),
            confirmed: 0.into(),
            returned: 0.into(),
            redelivered: 0.into(),
        });
        conn.broker.register_channel(info.clone());
        conn.info.channels.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(ChanShared {
            number,
            id,
            conn: conn.clone(),
            tags: Mutex::new(Tags::default()),
            global_prefetch: AtomicU32::new(0),
            unacked: AtomicU32::new(0),
            flow: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            info,
            ctl: ctl_tx,
        });
        let mut ch = Channel {
            shared,
            conn,
            consumers: HashMap::new(),
            last_queue: None,
            confirm_next: None,
            tx: None,
            prefetch: 0,
            perm_ok: HashSet::new(),
            reply_key: None,
            closed: false,
            wake_rr: std::sync::atomic::AtomicUsize::new(0),
        };
        tokio::spawn(async move {
            // A command read past a run of acks, handled next.
            let mut held = None;
            loop {
                let cmd = match held.take() {
                    Some(c) => c,
                    None => {
                        let c = tokio::select! {
                            biased;
                            c = ctl_rx.recv() => c,
                            c = rx.recv() => c,
                        };
                        let Some(c) = c else { break };
                        c
                    }
                };
                // Consumers ack one message at a time; acks that arrive together settle together,
                // one command per queue instead of one per ack. Order is kept: the first other
                // command waits in `held`.
                let cmd = match cmd {
                    ChannelCmd::Method(Method::BasicAck { delivery_tag, multiple }) => {
                        let mut acks = vec![(delivery_tag, multiple)];
                        while acks.len() < ACK_BATCH {
                            match rx.try_recv() {
                                Ok(ChannelCmd::Method(Method::BasicAck { delivery_tag, multiple })) => acks.push((delivery_tag, multiple)),
                                Ok(other) => {
                                    held = Some(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        ChannelCmd::Acks(acks)
                    }
                    c => c,
                };
                if ch.handle(cmd).await {
                    break;
                }
            }
            ch.cleanup().await;
        });
        tx
    }

    /// Returns true when the channel task should stop.
    async fn handle(&mut self, cmd: ChannelCmd) -> bool {
        if self.closed {
            if let ChannelCmd::Method(Method::ChannelClose { .. }) = cmd {
                // The client's close crossed ours: answer it, then the channel is gone.
                self.send(Method::ChannelCloseOk {});
                return true;
            }
            return matches!(cmd, ChannelCmd::Shutdown);
        }
        let (class, method) = match &cmd {
            ChannelCmd::Method(m) => m.ids(),
            ChannelCmd::Publish(_) => (60, 40),
            ChannelCmd::Fail(_, c, m) => (*c, *m),
            ChannelCmd::Acks(_) => (60, 80),
            _ => (0, 0),
        };
        let r = match cmd {
            ChannelCmd::Shutdown => return true,
            ChannelCmd::Fail(e, _, _) => Err(e),
            ChannelCmd::ConsumerGone(tag) => {
                if let Some(c) = self.consumers.remove(&tag) {
                    self.shared.info.consumers.fetch_sub(1, Ordering::Relaxed);
                    if let Some(k) = c.reply_key {
                        self.reply_key = None;
                        reply_to::unregister(&k);
                    }
                }
                Ok(false)
            }
            ChannelCmd::Publish(c) => self.publish(c).await.map(|_| false),
            ChannelCmd::Acks(acks) => self.settle_acks(acks).await.map(|_| false),
            ChannelCmd::Method(m) => self.method(m).await,
        };
        match r {
            Ok(stop) => stop,
            Err(e) if e.is_channel_error() => {
                debug!(channel = self.shared.number, error = %e, "channel error");
                self.close_with(&e, class, method).await;
                false
            }
            Err(e) => {
                let _ = self.conn.ctl.send(Ctl::Error(e, class, method));
                // Nothing this channel says after a connection error may reach the client first.
                self.release().await;
                self.closed = true;
                false
            }
        }
    }

    fn send(&self, m: Method) {
        self.conn.send(Out::Method(self.shared.number, m));
    }

    async fn close_with(&mut self, e: &AmqpError, class_id: u16, method_id: u16) {
        self.shared.closing.store(true, Ordering::Release);
        let _ = self.conn.ctl.send(Ctl::ChannelClosing(self.shared.number));
        self.send(Method::ChannelClose { reply_code: e.code, reply_text: e.reply_text(), class_id, method_id });
        self.release().await;
        self.closed = true;
    }

    /// Requeue everything this channel holds and drop its consumers.
    async fn release(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        let (unacked, parked) = {
            let mut t = self.shared.tags.lock();
            (std::mem::take(&mut t.unacked), std::mem::take(&mut t.parked))
        };
        let mut unacked: Vec<Unacked> = unacked.into_values().collect();
        // Parked deliveries are held by this channel on their queue: requeued like unacked ones.
        unacked.extend(parked.into_iter().map(|d| Unacked { queue: d.queue, seq: d.seq, consumer: None }));
        // Acks held by an uncommitted transaction are rolled back: those deliveries requeue too.
        for op in self.tx.as_mut().map(std::mem::take).unwrap_or_default() {
            if let TxOp::Settle(taken, _) = op {
                unacked.extend(taken.into_iter().map(|(_, u)| u));
            }
        }
        let mut queues: Vec<QueueRef> = Vec::new();
        for u in unacked {
            if !queues.iter().any(|q| Arc::ptr_eq(q, &u.queue)) {
                queues.push(u.queue);
            }
        }
        for (_, c) in self.consumers.drain() {
            if !queues.iter().any(|q| Arc::ptr_eq(q, &c.queue)) {
                queues.push(c.queue.clone());
            }
            if let Some(k) = c.reply_key {
                reply_to::unregister(&k);
                let _ = c.queue.send(mq_core::queue::QueueCmd::Delete { if_unused: false, if_empty: false, reply: oneshot::channel().0 }).await;
            }
        }
        for q in queues {
            q.channel_closed(self.shared.id).await;
        }
        self.shared.unacked.store(0, Ordering::Release);
        self.shared.info.unacked.store(0, Ordering::Relaxed);
        self.shared.info.consumers.store(0, Ordering::Relaxed);
    }

    async fn cleanup(&mut self) {
        if !self.closed {
            self.release().await;
        }
        self.conn.broker.channel_closed(self.shared.id);
        self.conn.info.channels.fetch_sub(1, Ordering::Relaxed);
    }

    fn check(&mut self, access: Access, is_queue: bool, name: &str) -> Result<()> {
        let key = (access as u8, is_queue, name.to_string());
        if self.perm_ok.contains(&key) {
            return Ok(());
        }
        let kind = if is_queue { "queue" } else { "exchange" };
        self.conn.broker.check(&self.conn.user, &self.conn.vhost.name, access, kind, name)?;
        if self.perm_ok.len() > 1024 {
            self.perm_ok.clear();
        }
        self.perm_ok.insert(key);
        Ok(())
    }

    fn queue_name(&self, name: String) -> Result<String> {
        if !name.is_empty() {
            return Ok(name);
        }
        self.last_queue.clone().ok_or_else(|| AmqpError::new(NOT_FOUND, "no previously declared queue"))
    }

    fn conn_id(&self) -> Option<u64> {
        Some(self.conn.info.id)
    }

    async fn method(&mut self, m: Method) -> Result<bool> {
        let vh = self.conn.vhost.clone();
        match m {
            Method::ChannelClose { .. } => {
                self.release().await;
                self.closed = true;
                self.send(Method::ChannelCloseOk {});
                return Ok(true);
            }
            Method::ChannelCloseOk {} => {}
            // Like RabbitMQ, clients may not pause deliveries with channel.flow (use prefetch).
            Method::ChannelFlow { active: false } => return Err(AmqpError::new(NOT_IMPLEMENTED, "active=false")),
            Method::ChannelFlow { active: true } => self.send(Method::ChannelFlowOk { active: true }),
            Method::ChannelFlowOk { .. } => {}
            Method::AccessRequest { .. } => self.send(Method::AccessRequestOk { ticket: 1 }),

            Method::ExchangeDeclare { exchange, kind, passive, durable, auto_delete, internal, nowait, arguments, .. } => {
                let exchange = strip_cr_lf(exchange);
                if !passive {
                    self.check(Access::Configure, false, &exchange)?;
                    if let Some(ae) = arguments.get_str("alternate-exchange") {
                        // RabbitMQ requires write on the exchange and read on the AE.
                        let ae = ae.to_string();
                        self.check(Access::Read, false, &ae)?;
                    }
                }
                vh.declare_exchange(&exchange, &kind, durable, auto_delete, internal, arguments, passive).await?;
                if !nowait {
                    self.send(Method::ExchangeDeclareOk {});
                }
            }
            Method::ExchangeDelete { exchange, if_unused, nowait, .. } => {
                self.check(Access::Configure, false, &exchange)?;
                match vh.delete_exchange(&exchange, if_unused).await {
                    // Deleting a missing exchange succeeds (RabbitMQ 3.x+).
                    Err(e) if e.code == NOT_FOUND => {}
                    r => r?,
                }
                if !nowait {
                    self.send(Method::ExchangeDeleteOk {});
                }
            }
            Method::ExchangeBind { destination, source, routing_key, nowait, arguments, .. } => {
                self.check(Access::Write, false, &destination)?;
                self.check(Access::Read, false, &source)?;
                vh.bind(&source, Dest::exchange(&destination), &routing_key, arguments, self.conn_id()).await?;
                if !nowait {
                    self.send(Method::ExchangeBindOk {});
                }
            }
            Method::ExchangeUnbind { destination, source, routing_key, nowait, arguments, .. } => {
                self.check(Access::Write, false, &destination)?;
                self.check(Access::Read, false, &source)?;
                vh.unbind(&source, Dest::exchange(&destination), &routing_key, arguments, self.conn_id()).await?;
                if !nowait {
                    self.send(Method::ExchangeUnbindOk {});
                }
            }

            Method::QueueDeclare { queue, passive, durable, exclusive, auto_delete, nowait, arguments, .. } => {
                let queue = strip_cr_lf(queue);
                if queue == reply_to::PSEUDO_QUEUE && passive {
                    self.send(Method::QueueDeclareOk { queue, message_count: 0, consumer_count: 0 });
                    return Ok(false);
                }
                if passive && queue.starts_with(reply_to::PREFIX) {
                    // A live reply-to name "exists" (RabbitMQ answers declare-ok for it).
                    if reply_to::lookup(&queue).is_none() {
                        return Err(AmqpError::not_found(format!("queue '{queue}' in vhost '{}'", vh.name)));
                    }
                    if !nowait {
                        self.send(Method::QueueDeclareOk { queue, message_count: 0, consumer_count: 1 });
                    }
                    return Ok(false);
                }
                if !passive && !queue.is_empty() {
                    self.check(Access::Configure, true, &queue)?;
                }
                if let Some(dlx) = arguments.get_str("x-dead-letter-exchange") {
                    let dlx = dlx.to_string();
                    if !passive {
                        self.check(Access::Write, false, &dlx)?;
                    }
                }
                if !passive {
                    for key in ["x-dead-letter-exchange", "x-dead-letter-routing-key"] {
                        if let Some(v) = arguments.get(key).filter(|v| !matches!(v, FieldValue::LongStr(_) | FieldValue::ShortStr(_))) {
                            return Err(AmqpError::precondition(format!(
                                "invalid arg '{key}' for queue '{queue}' in vhost '{}': {{unacceptable_type,{}}}",
                                vh.name,
                                v.to_json()
                            )));
                        }
                    }
                }
                // Another connection's exclusive queue is locked to us, whatever this declare asks.
                if !queue.is_empty()
                    && let Some(existing) = vh.queue(&queue)
                {
                    vh.queue_for(&queue, self.conn_id())?;
                    if !passive {
                        // RabbitMQ checks exclusivity (405) before the other properties (406).
                        if existing.meta.exclusive_owner.is_some() != exclusive {
                            return Err(AmqpError::locked(format!(
                                "cannot obtain exclusive access to locked queue '{queue}' in vhost '{}'. It could be originally declared on another connection or the exclusive property value does not match that of the original declaration.",
                                vh.name
                            )));
                        }
                        check_arg_classes(&existing.meta.arguments, &arguments, &queue, &vh.name)?;
                    }
                }
                let ok = vh
                    .declare_queue(DeclareQueue {
                        name: queue,
                        durable,
                        exclusive_owner: exclusive.then_some(self.conn.info.id),
                        auto_delete,
                        args: arguments,
                        passive,
                    })
                    .await?;
                self.last_queue = Some(ok.name.clone());
                if !nowait {
                    self.send(Method::QueueDeclareOk { queue: ok.name, message_count: ok.messages, consumer_count: ok.consumers });
                }
            }
            Method::QueueBind { queue, exchange, routing_key, nowait, arguments, .. } => {
                // An empty queue name means "the last queue"; with an empty key too, that name is the key.
                let routing_key = if queue.is_empty() && routing_key.is_empty() { self.queue_name(String::new())? } else { routing_key };
                let queue = self.queue_name(queue)?;
                self.check(Access::Write, true, &queue)?;
                self.check(Access::Read, false, &exchange)?;
                vh.bind(&exchange, Dest::queue(&queue), &routing_key, arguments, self.conn_id()).await?;
                if !nowait {
                    self.send(Method::QueueBindOk {});
                }
            }
            Method::QueueUnbind { queue, exchange, routing_key, arguments, .. } => {
                let routing_key = if queue.is_empty() && routing_key.is_empty() { self.queue_name(String::new())? } else { routing_key };
                let queue = self.queue_name(queue)?;
                self.check(Access::Write, true, &queue)?;
                self.check(Access::Read, false, &exchange)?;
                match vh.unbind(&exchange, Dest::queue(&queue), &routing_key, arguments, self.conn_id()).await {
                    // Unbinding a missing queue, exchange or binding succeeds (RabbitMQ 3.x+).
                    Err(e) if e.code == NOT_FOUND => {}
                    r => r?,
                }
                self.send(Method::QueueUnbindOk {});
            }
            Method::QueuePurge { queue, nowait, .. } => {
                let queue = self.queue_name(queue)?;
                self.check(Access::Read, true, &queue)?;
                let q = vh.queue_for(&queue, self.conn_id())?;
                let n = q.purge().await?;
                if !nowait {
                    self.send(Method::QueuePurgeOk { message_count: n as u32 });
                }
            }
            Method::QueueDelete { queue, if_unused, if_empty, nowait, .. } => {
                let queue = self.queue_name(queue)?;
                self.check(Access::Configure, true, &queue)?;
                let n = match vh.delete_queue(&queue, if_unused, if_empty, self.conn_id()).await {
                    Err(e) if e.code == NOT_FOUND => 0,
                    r => r?,
                };
                if !nowait {
                    self.send(Method::QueueDeleteOk { message_count: n as u32 });
                }
            }

            Method::BasicQos { prefetch_size, prefetch_count, global } => {
                if prefetch_size != 0 {
                    return Err(AmqpError::new(NOT_IMPLEMENTED, format!("prefetch_size!=0 ({prefetch_size})")));
                }
                if global {
                    self.shared.global_prefetch.store(prefetch_count as u32, Ordering::Release);
                    self.shared.info.global_prefetch_count.store(prefetch_count as u32, Ordering::Relaxed);
                    self.wake_consumers();
                } else {
                    self.prefetch = prefetch_count as u32;
                    self.shared.info.prefetch_count.store(prefetch_count as u32, Ordering::Relaxed);
                }
                self.send(Method::BasicQosOk {});
            }
            Method::BasicConsume { queue, consumer_tag, no_ack, exclusive, nowait, arguments, .. } => {
                self.consume(queue, consumer_tag, no_ack, exclusive, nowait, arguments).await?;
            }
            Method::BasicCancel { consumer_tag, nowait } => {
                let tag: Arc<str> = Arc::from(consumer_tag.as_str());
                if let Some(c) = self.consumers.remove(&tag) {
                    self.shared.info.consumers.fetch_sub(1, Ordering::Relaxed);
                    c.sink.cancelled.store(true, Ordering::Release);
                    if let Some(k) = c.reply_key {
                        self.reply_key = None;
                        reply_to::unregister(&k);
                        let _ = c.queue.send(mq_core::queue::QueueCmd::Delete { if_unused: false, if_empty: false, reply: oneshot::channel().0 }).await;
                    } else {
                        c.queue.cancel(self.shared.id, tag).await;
                    }
                }
                if !nowait {
                    self.send(Method::BasicCancelOk { consumer_tag });
                }
            }
            Method::BasicGet { queue, no_ack, .. } => {
                let queue = self.queue_name(queue)?;
                self.check(Access::Read, true, &queue)?;
                let q = vh.queue_for(&queue, self.conn_id())?;
                match q.get(self.shared.id, no_ack).await? {
                    None => self.send(Method::BasicGetEmpty { cluster_id: String::new() }),
                    Some(d) => {
                        let tag = self.shared.next_tag();
                        if !no_ack {
                            // basic.get deliveries don't count against prefetch (RabbitMQ).
                            self.shared.tags.lock().unacked.insert(tag, Unacked { queue: q.clone(), seq: d.seq, consumer: None });
                            self.shared.info.unacked.fetch_add(1, Ordering::Relaxed);
                        }
                        self.shared.info.delivered.fetch_add(1, Ordering::Relaxed);
                        vh.stats.delivered.fetch_add(1, Ordering::Relaxed);
                        vh.stats.delivered_bytes.fetch_add(d.msg.body.len() as u64, Ordering::Relaxed);
                        self.conn.send(Out::GetOk { channel: self.shared.number, tag, d });
                    }
                }
            }
            Method::BasicAck { delivery_tag, multiple } => self.settle(delivery_tag, multiple, Outcome::Ack).await?,
            Method::BasicReject { delivery_tag, requeue } => {
                self.settle(delivery_tag, false, if requeue { Outcome::Requeue } else { Outcome::Reject }).await?;
            }
            Method::BasicNack { delivery_tag, multiple, requeue } => {
                self.settle(delivery_tag, multiple, if requeue { Outcome::Requeue } else { Outcome::Reject }).await?;
            }
            Method::BasicRecover { requeue } | Method::BasicRecoverAsync { requeue } => {
                if !requeue {
                    return Err(AmqpError::new(NOT_IMPLEMENTED, "requeue=false"));
                }
                // Not deferred by tx: acked-but-uncommitted deliveries stay out of it.
                let taken = self.take(0, true)?;
                self.apply(taken, Outcome::Requeue).await;
                if matches!(m, Method::BasicRecover { .. }) {
                    self.send(Method::BasicRecoverOk {});
                }
            }

            Method::ConfirmSelect { nowait } => {
                if self.tx.is_some() {
                    return Err(AmqpError::precondition("cannot switch from tx to confirm mode"));
                }
                if self.confirm_next.is_none() {
                    self.confirm_next = Some(1);
                }
                self.shared.info.confirm.store(true, Ordering::Relaxed);
                if !nowait {
                    self.send(Method::ConfirmSelectOk {});
                }
            }
            Method::TxSelect {} => {
                if self.confirm_next.is_some() {
                    return Err(AmqpError::precondition("cannot switch from confirm to tx mode"));
                }
                if self.tx.is_none() {
                    self.tx = Some(Vec::new());
                }
                self.shared.info.transactional.store(true, Ordering::Relaxed);
                self.send(Method::TxSelectOk {});
            }
            Method::TxCommit {} => {
                let Some(ops) = self.tx.replace(Vec::new()) else {
                    return Err(AmqpError::precondition("channel is not transactional"));
                };
                let mut waits = Vec::new();
                for op in ops {
                    match op {
                        TxOp::Publish(c) => {
                            let (done, rx) = oneshot::channel();
                            let ticket = ConfirmTicket::new(move |ok| {
                                let _ = done.send(ok);
                            });
                            self.publish_now(c, Some(ticket)).await?;
                            waits.push(rx);
                        }
                        TxOp::Settle(taken, outcome) => self.apply(taken, outcome).await,
                    }
                }
                // Commit-ok means durable, like RabbitMQ.
                for w in waits {
                    let _ = w.await;
                }
                self.send(Method::TxCommitOk {});
            }
            Method::TxRollback {} => {
                let Some(ops) = self.tx.replace(Vec::new()) else {
                    return Err(AmqpError::precondition("channel is not transactional"));
                };
                // Rolled-back acks make their deliveries unacked again.
                let mut t = self.shared.tags.lock();
                for op in ops {
                    if let TxOp::Settle(taken, _) = op {
                        t.unacked.extend(taken);
                    }
                }
                drop(t);
                self.send(Method::TxRollbackOk {});
            }
            other => {
                return Err(AmqpError::command_invalid(format!("unexpected method {} on a channel", other.name())));
            }
        }
        Ok(false)
    }

    fn wake_consumers(&self) {
        // Parked deliveries first: they were sent by queues before any woken now.
        loop {
            let Some(d) = self.shared.tags.lock().parked.pop_front() else { break };
            let sink = self.consumers.get(&d.consumer_tag).map(|c| c.sink.clone());
            if let Err(d) = send_delivery(&self.shared, sink.as_deref(), d) {
                self.shared.tags.lock().parked.push_front(d);
                return;
            }
        }
        let n = self.consumers.len();
        if n == 0 {
            return;
        }
        let start = self.wake_rr.fetch_add(1, Ordering::Relaxed) % n;
        for c in self.consumers.values().cycle().skip(start).take(n) {
            c.queue.wake();
        }
    }

    async fn consume(&mut self, queue: String, tag: String, no_ack: bool, exclusive: bool, nowait: bool, args: FieldTable) -> Result<()> {
        let tag = if tag.is_empty() { format!("amq.ctag-{}", &mq_core::vhost::generate_queue_name()[8..]) } else { tag };
        let tag: Arc<str> = Arc::from(tag.as_str());
        if self.consumers.contains_key(&tag) {
            return Err(AmqpError::not_allowed(format!("attempt to reuse consumer tag '{tag}'")));
        }
        let vh = self.conn.vhost.clone();
        let sink = Arc::new(Sink {
            chan: self.shared.clone(),
            no_ack,
            prefetch: self.prefetch,
            unacked: AtomicU32::new(0),
            ready: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });

        // Direct reply-to: a private transient queue only this consumer can see.
        if queue == reply_to::PSEUDO_QUEUE {
            if !no_ack {
                return Err(AmqpError::precondition("reply consumer cannot acknowledge"));
            }
            if self.reply_key.is_some() {
                return Err(AmqpError::precondition("reply consumer already set"));
            }
            let key = format!("{}.{:x}", mq_core::vhost::generate_queue_name()[8..].replace(['-', '_'], "x"), self.shared.id);
            let meta = QueueMeta {
                name: format!("{}{key}", reply_to::PREFIX),
                vhost: vh.name.clone(),
                durable: false,
                auto_delete: false,
                exclusive_owner: Some(self.conn.info.id),
                arguments: FieldTable::new(),
                created_at: 0,
            };
            let q = mq_core::queue::engine::spawn(meta, Default::default(), None, Arc::new(NoHost)).await.map_err(AmqpError::from)?;
            reply_to::register(&key, &q);
            self.reply_key = Some(key.clone());
            self.subscribe(q, tag, sink, false, 0, args, nowait, Some(key)).await?;
            return Ok(());
        }

        let queue = self.queue_name(queue)?;
        self.check(Access::Read, true, &queue)?;
        let q = vh.queue_for(&queue, self.conn_id())?;
        if q.kind == QueueType::Stream && self.prefetch == 0 && self.shared.global_prefetch.load(Ordering::Relaxed) == 0 {
            return Err(AmqpError::precondition("consumer prefetch count is not set for stream queue"));
        }
        let priority = match args.get("x-priority") {
            None => 0,
            Some(FieldValue::I8(_) | FieldValue::U8(_) | FieldValue::I16(_) | FieldValue::U16(_) | FieldValue::I32(_) | FieldValue::U32(_) | FieldValue::I64(_)) => {
                args.get_i64("x-priority").unwrap_or(0) as i32
            }
            Some(v) => {
                return Err(AmqpError::precondition(format!("invalid arg 'x-priority' for queue '{queue}': {{unacceptable_type,{}}}", v.to_json())));
            }
        };
        self.subscribe(q, tag, sink, exclusive, priority, args, nowait, None).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn subscribe(
        &mut self,
        q: QueueRef,
        tag: Arc<str>,
        sink: Arc<Sink>,
        exclusive: bool,
        priority: i32,
        args: FieldTable,
        nowait: bool,
        reply_key: Option<String>,
    ) -> Result<()> {
        let spec = ConsumerSpec {
            tag: tag.clone(),
            channel: self.shared.id,
            conn: self.conn.info.id,
            no_ack: sink.no_ack,
            exclusive,
            priority,
            args,
            sink: sink.clone(),
        };
        q.consume(spec).await?;
        if !nowait {
            self.send(Method::BasicConsumeOk { consumer_tag: tag.to_string() });
        }
        // Deliveries may start only once consume-ok is queued ahead of them.
        sink.ready.store(true, Ordering::Release);
        self.conn.watch_queue(&q);
        q.wake();
        self.shared.info.consumers.fetch_add(1, Ordering::Relaxed);
        self.consumers.insert(tag, Consumer { queue: q, sink, reply_key });
        Ok(())
    }

    /// Ack/nack/reject: validated at once (RabbitMQ: also on transacted channels), applied now or
    /// on tx.commit.
    async fn settle(&mut self, tag: u64, multiple: bool, outcome: Outcome) -> Result<()> {
        let taken = self.take(tag, multiple)?;
        match &mut self.tx {
            Some(ops) => ops.push(TxOp::Settle(taken, outcome)),
            None => self.apply(taken, outcome).await,
        }
        Ok(())
    }

    /// [`ChannelCmd::Acks`]: like that many `basic.ack`s in a row. An unknown tag stops the run
    /// with the channel error, after the acks before it took effect.
    async fn settle_acks(&mut self, acks: Vec<(u64, bool)>) -> Result<()> {
        let mut taken = Vec::new();
        let mut res = Ok(());
        for (tag, multiple) in acks {
            match self.take(tag, multiple) {
                Ok(t) => taken.extend(t),
                Err(e) => {
                    res = Err(e);
                    break;
                }
            }
        }
        match &mut self.tx {
            Some(ops) => ops.push(TxOp::Settle(taken, Outcome::Ack)),
            None => self.apply(taken, Outcome::Ack).await,
        }
        res
    }

    /// Remove deliveries from the unacked set. Like RabbitMQ the tag itself must be unacked, even
    /// with `multiple` (tag 0 + multiple means everything).
    fn take(&self, tag: u64, multiple: bool) -> Result<Vec<(u64, Unacked)>> {
        let mut t = self.shared.tags.lock();
        if multiple && tag == 0 {
            return Ok(std::mem::take(&mut t.unacked).into_iter().collect());
        }
        if !t.unacked.contains_key(&tag) {
            return Err(AmqpError::precondition(format!("unknown delivery tag {tag}")));
        }
        if !multiple {
            return Ok(t.unacked.remove(&tag).map(|u| vec![(tag, u)]).unwrap_or_default());
        }
        let rest = t.unacked.split_off(&(tag + 1));
        Ok(std::mem::replace(&mut t.unacked, rest).into_iter().collect())
    }

    async fn apply(&mut self, taken: Vec<(u64, Unacked)>, outcome: Outcome) {
        if taken.is_empty() {
            return;
        }
        let n = taken.len() as u32;
        // Only consumer deliveries hold prefetch credit.
        let credited = taken.iter().filter(|(_, u)| u.consumer.is_some()).count() as u32;
        self.shared.unacked.fetch_sub(credited, Ordering::AcqRel);
        self.shared.info.unacked.fetch_sub(n as u64, Ordering::Relaxed);
        if outcome == Outcome::Ack {
            self.shared.info.acked.fetch_add(n as u64, Ordering::Relaxed);
            self.conn.vhost.stats.acked.fetch_add(n as u64, Ordering::Relaxed);
        }
        let global_full = {
            let g = self.shared.global_prefetch.load(Ordering::Relaxed);
            g > 0 && credited > 0 && self.shared.unacked.load(Ordering::Relaxed) + credited >= g
        };
        let mut by_queue: Vec<(QueueRef, Vec<u64>)> = Vec::new();
        for (_, u) in taken {
            if let Some(c) = u.consumer.as_ref().and_then(|t| self.consumers.get(t)) {
                c.sink.unacked.fetch_sub(1, Ordering::AcqRel);
            }
            match by_queue.iter_mut().find(|(q, _)| Arc::ptr_eq(q, &u.queue)) {
                Some((_, v)) => v.push(u.seq),
                None => by_queue.push((u.queue, vec![u.seq])),
            }
        }
        for (q, seqs) in by_queue {
            q.settle(self.shared.id, seqs, outcome).await;
        }
        if global_full {
            self.wake_consumers();
        }
    }

    async fn publish(&mut self, c: Content) -> Result<()> {
        if c.immediate {
            return Err(AmqpError::new(NOT_IMPLEMENTED, "immediate=true"));
        }
        self.shared.info.published.fetch_add(1, Ordering::Relaxed);
        if let Some(tx) = &mut self.tx {
            tx.push(TxOp::Publish(c));
            return Ok(());
        }
        let ticket = self.confirm_next.map(|seq| {
            self.confirm_next = Some(seq + 1);
            let conn = self.conn.clone();
            let n = self.shared.number;
            let info = self.shared.info.clone();
            ConfirmTicket::new(move |ok| {
                info.confirmed.fetch_add(1, Ordering::Relaxed);
                let m = if ok { Method::BasicAck { delivery_tag: seq, multiple: false } } else { Method::BasicNack { delivery_tag: seq, multiple: false, requeue: false } };
                conn.send(Out::Method(n, m));
            })
        });
        self.publish_now(c, ticket).await
    }

    async fn publish_now(&mut self, c: Content, ticket: Option<ConfirmTicket>) -> Result<()> {
        let vh = self.conn.vhost.clone();
        self.check(Access::Write, false, &c.exchange)?;
        let mut msg = Message::from_wire(c.exchange.clone(), c.routing_key.clone(), c.props, c.body).map_err(|e| AmqpError::new(FRAME_ERROR, e.to_string()))?;
        // RabbitMQ 3.13+: dead-letter history is the broker's own. Publisher-set x-death headers
        // still reach consumers verbatim (props_raw is untouched) but don't seed the history.
        if let Some(h) = msg.props.headers.as_mut()
            && DEATH_HEADERS.iter().any(|k| h.get(k).is_some())
        {
            for k in DEATH_HEADERS {
                h.remove(k);
            }
        }
        if let Some(uid) = &msg.props.user_id
            && *uid != self.conn.user.name
            && !self.conn.user.has_tag("impersonator")
        {
            return Err(AmqpError::precondition(format!("user_id property set to '{uid}' but authenticated user was '{}'", self.conn.user.name)));
        }
        if let Some(exp) = &msg.props.expiration
            && exp.trim().parse::<u64>().is_err()
        {
            let why = if exp.trim().parse::<i64>().is_ok() { format!("{{value_negative,{exp}}}") } else { "no_integer".into() };
            return Err(AmqpError::precondition(format!("invalid expiration '{exp}': {why}")));
        }
        for h in ["CC", "BCC"] {
            if let Some(v) = msg.props.header(h).filter(|v| v.as_array().is_none()) {
                // RabbitMQ: sender-selected distribution headers must be arrays of strings.
                return Err(AmqpError::precondition(format!("invalid header '{h}': {{unacceptable_type_in_header,{}}}", v.to_json())));
            }
        }
        // Topic permissions apply to topic exchanges.
        if !c.exchange.is_empty()
            && let Some(ex) = vh.exchange(&c.exchange)
            && ex.kind == ExchangeKind::Topic
        {
            self.conn.broker.check_topic(&self.conn.user, &vh.name, &c.exchange, &c.routing_key, Access::Write, self.conn.client_id.as_deref())?;
        }
        if self.conn.broker.alarm.load(Ordering::Relaxed) {
            self.block_until_clear().await;
        }

        // Direct reply-to.
        let msg = match msg.props.reply_to.as_deref() {
            Some(reply_to::PSEUDO_QUEUE) => {
                let Some(k) = &self.reply_key else {
                    return Err(AmqpError::precondition("fast reply consumer does not exist"));
                };
                let mut p = msg.props.clone();
                p.reply_to = Some(format!("{}{k}", reply_to::PREFIX));
                msg.with_props(&msg.exchange, &msg.routing_key, p)
            }
            _ => msg,
        };
        if c.exchange.is_empty() && c.routing_key.starts_with(reply_to::PREFIX) {
            if let Some(q) = reply_to::lookup(&c.routing_key) {
                q.publish(Arc::new(msg), ticket).await?;
            } else if let Some(t) = ticket {
                t.settle(true);
            }
            return Ok(());
        }

        let outcome = vh.publish(&c.exchange, msg.clone_for_return(), ticket).await?;
        if outcome.routed == 0 && c.mandatory {
            self.shared.info.returned.fetch_add(1, Ordering::Relaxed);
            vh.stats.returned.fetch_add(1, Ordering::Relaxed);
            self.conn.send(Out::Return { channel: self.shared.number, code: NO_ROUTE, text: "NO_ROUTE".into(), msg: Arc::new(msg) });
        }
        if let Some(t) = outcome.confirm {
            t.settle(true);
        }
        Ok(())
    }

    async fn block_until_clear(&self) {
        let broker = &self.conn.broker;
        if self.conn.has_capability("connection.blocked") && !self.conn.info.blocked.swap(true, Ordering::AcqRel) {
            self.conn.send(Out::Method(0, Method::ConnectionBlocked { reason: "low on memory".into() }));
        }
        while broker.alarm.load(Ordering::Acquire) {
            let n = broker.alarm_cleared.notified();
            if !broker.alarm.load(Ordering::Acquire) {
                break;
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), n).await;
        }
        if self.conn.info.blocked.swap(false, Ordering::AcqRel) {
            self.conn.send(Out::Method(0, Method::ConnectionUnblocked {}));
        }
    }
}

trait CloneForReturn {
    fn clone_for_return(&self) -> Message;
}

impl CloneForReturn for Message {
    /// A cheap copy (bytes are shared) so the original can be returned to the publisher.
    fn clone_for_return(&self) -> Message {
        Message {
            exchange: self.exchange.clone(),
            routing_key: self.routing_key.clone(),
            props: self.props.clone(),
            props_raw: self.props_raw.clone(),
            body: self.body.clone(),
            received_ms: self.received_ms,
        }
    }
}

/// Host for private queues that never dead-letter or live in a vhost.
struct NoHost;

impl mq_core::queue::engine::QueueHost for NoHost {
    fn republish(&self, _exchange: String, _msg: Message, _from: String) {}
    fn queue_removed(&self, _name: &str) {}
}

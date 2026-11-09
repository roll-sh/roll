//! Sessions and links: session flow control, publishing links (exchange, queue and
//! anonymous targets, settlement after durability, unroutable messages released), consuming
//! links (link credit, drain, settle modes, outcomes, stream filters, consumer priority and
//! single active consumer) and the `/management` link pair.

use crate::address::{self, SourceAddr, TargetAddr};
use crate::conn::{Ctx, Out};
use crate::frames::*;
use crate::message::{self, Amqp1Message, DeliveryInfo, prop};
use crate::mgmt;
use crate::types::Value;
use bytes::{Bytes, BytesMut};
use mq_core::auth::Access;
use mq_core::broker::ChannelInfo;
use mq_core::error::*;
use mq_core::exchange::{Dest, ExchangeKind};
use mq_core::message::Message;
use mq_core::queue::{ChannelId, ConfirmTicket, ConsumerSink, ConsumerSpec, Delivery, Modification, Outcome, QueueRef, QueueType};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::vhost::DeclareQueue;
use mq_core::{AmqpError, VHost};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tracing::debug;

/// Our session incoming window (transfer frames), refreshed when half used.
const IN_WINDOW: u32 = 8192;
const OUT_WINDOW: u32 = u32::MAX;
/// Credit for the management node's request link (RabbitMQ grants 8).
const MGMT_CREDIT: u32 = 8;
const PROPERTIES_FILTER: u64 = 0x0000_468C_0000_0003;
const APP_PROPERTIES_FILTER: u64 = 0x0000_468C_0000_0004;
const OUTCOMES: [&str; 4] = ["amqp:accepted:list", "amqp:rejected:list", "amqp:released:list", "amqp:modified:list"];

/// Something a queue (or a confirm) tells the connection loop.
pub(crate) enum Event {
    Deliver { link: Arc<LinkShared>, d: Delivery },
    Cancelled { link: Arc<LinkShared> },
    Activated { link: Arc<LinkShared>, active: bool },
    Confirmed { channel: u16, session: ChannelId, handle: u32, link_id: u64, delivery_id: u32, result: Confirm },
    DrainCheck { link: Arc<LinkShared> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Confirm {
    Accepted,
    Released,
    Rejected,
}

impl Event {
    pub fn channel(&self) -> Option<u16> {
        Some(match self {
            Event::Deliver { link, .. } | Event::Cancelled { link } | Event::Activated { link, .. } | Event::DrainCheck { link } => link.channel,
            Event::Confirmed { channel, .. } => *channel,
        })
    }

    /// The session is gone; its queues already took back what it held.
    pub async fn orphaned(self) {}
}

/// Credit state of a consuming link, shared with the queue actor through the sink.
pub(crate) struct LinkShared {
    pub channel: u16,
    pub session: ChannelId,
    pub handle: u32,
    pub link_id: u64,
    pub queue: QueueRef,
    credit: Mutex<Credit>,
    pub closed: AtomicBool,
    events: mpsc::UnboundedSender<Event>,
}

#[derive(Default)]
struct Credit {
    credit: u32,
    delivery_count: u32,
    drain: bool,
}

struct Sink(Arc<LinkShared>);

impl ConsumerSink for Sink {
    fn has_credit(&self) -> bool {
        !self.0.closed.load(Ordering::Acquire) && self.0.credit.lock().credit > 0
    }

    fn deliver(&self, d: Delivery) {
        {
            let mut c = self.0.credit.lock();
            c.credit = c.credit.saturating_sub(1);
            c.delivery_count = c.delivery_count.wrapping_add(1);
        }
        let _ = self.0.events.send(Event::Deliver { link: self.0.clone(), d });
    }

    fn cancelled(&self, _tag: &str) {
        if !self.0.closed.load(Ordering::Acquire) {
            let _ = self.0.events.send(Event::Cancelled { link: self.0.clone() });
        }
    }

    fn activated(&self, _tag: &str, active: bool) {
        let _ = self.0.events.send(Event::Activated { link: self.0.clone(), active });
    }
}

/// A property filter expression (AMQP Filter Expressions §4.2), evaluated per message.
#[derive(Clone, Debug)]
struct PropFilter {
    properties: Vec<(usize, Value)>,
    application: Vec<(String, Value)>,
}

fn filter_value_matches(want: &Value, have: &Value) -> bool {
    if let (Value::String(w) | Value::Symbol(w), Value::String(h) | Value::Symbol(h)) = (want, have) {
        if let Some(p) = w.strip_prefix("&p:") {
            return h.starts_with(p);
        }
        if let Some(s) = w.strip_prefix("&s:") {
            return h.ends_with(s);
        }
        let w = w.strip_prefix('&').filter(|r| r.starts_with('&')).unwrap_or(w);
        return w == h;
    }
    match (want, have) {
        (Value::Binary(w), Value::Binary(h)) => w == h,
        (w, h) => match (w.as_i64(), h.as_i64()) {
            (Some(a), Some(b)) => a == b,
            _ => w == h,
        },
    }
}

impl PropFilter {
    fn matches(&self, m: &Amqp1Message) -> bool {
        self.properties.iter().all(|(i, want)| filter_value_matches(want, m.prop(*i)))
            && self.application.iter().all(|(k, want)| {
                m.application_properties.iter().find(|(key, _)| key.as_str() == Some(k)).is_some_and(|(_, v)| filter_value_matches(want, v))
            })
    }
}

fn property_index(name: &str) -> Option<usize> {
    Some(match name {
        "message-id" => prop::MESSAGE_ID,
        "user-id" => prop::USER_ID,
        "to" => prop::TO,
        "subject" => prop::SUBJECT,
        "reply-to" => prop::REPLY_TO,
        "correlation-id" => prop::CORRELATION_ID,
        "content-type" => prop::CONTENT_TYPE,
        "content-encoding" => prop::CONTENT_ENCODING,
        "absolute-expiry-time" => prop::ABSOLUTE_EXPIRY_TIME,
        "creation-time" => prop::CREATION_TIME,
        "group-id" => prop::GROUP_ID,
        "group-sequence" => prop::GROUP_SEQUENCE,
        "reply-to-group-id" => prop::REPLY_TO_GROUP_ID,
        _ => return None,
    })
}

enum PubTarget {
    /// `key: None` routes by each message's `subject` (v1 `/exchange/x`, `/queue`).
    Fixed { exchange: String, key: Option<String> },
    Anonymous,
}

enum InKind {
    Publish(PubTarget),
    Management,
}

struct Partial {
    delivery_id: Option<u32>,
    settled: bool,
    payload: BytesMut,
}

struct InLink {
    link_id: u64,
    name: String,
    kind: InKind,
    delivery_count: u32,
    credit: u32,
    max_credit: u32,
    unconfirmed: u32,
    partial: Option<Partial>,
}

enum OutKind {
    Queue {
        shared: Arc<LinkShared>,
        tag: Arc<str>,
        /// Private queue to delete when the link goes (v1 exchange sources, dynamic sources).
        temp_queue: Option<String>,
        filter: Option<PropFilter>,
    },
    Management {
        credit: u32,
        delivery_count: u32,
        pending: VecDeque<Amqp1Message>,
    },
}

struct OutLink {
    link_id: u64,
    name: String,
    kind: OutKind,
    send_settled: bool,
    max_message_size: Option<u64>,
}

struct Unsettled {
    queue: QueueRef,
    seq: u64,
    handle: u32,
}

enum Pending {
    /// A transfer frame (counts against the peer's incoming window).
    Transfer(u32, Bytes),
    /// Any other frame that must stay behind queued transfers.
    Other(Bytes),
}

pub(crate) struct Session {
    ctx: Arc<Ctx>,
    channel: u16,
    pub id: ChannelId,
    info: Arc<ChannelInfo>,
    next_incoming_id: u32,
    incoming_window: u32,
    next_outgoing_id: u32,
    remote_incoming_window: u32,
    handle_max: u32,
    next_delivery_id: u32,
    next_link_id: u64,
    incoming: HashMap<u32, InLink>,
    outgoing: HashMap<u32, OutLink>,
    /// Links we detached first: the client's answering detach must not be echoed again.
    detaching: std::collections::HashSet<u32>,
    unsettled: BTreeMap<u32, Unsettled>,
    pending: VecDeque<Pending>,
    accepted: Vec<u32>,
    ending: bool,
    perm_ok: HashSet<(u8, bool, String)>,
}

/// AMQP 0-9-1 reply code → AMQP 1.0 error condition.
pub(crate) fn error_for(e: &AmqpError) -> AmqpErr {
    let cond = match e.code {
        ACCESS_REFUSED => "amqp:unauthorized-access",
        NOT_FOUND => "amqp:not-found",
        RESOURCE_LOCKED => "amqp:resource-locked",
        PRECONDITION_FAILED => "amqp:precondition-failed",
        NOT_ALLOWED => "amqp:not-allowed",
        NOT_IMPLEMENTED => "amqp:not-implemented",
        RESOURCE_ERROR => "amqp:resource-limit-exceeded",
        _ => "amqp:internal-error",
    };
    AmqpErr::new(cond, e.text.clone())
}

fn invalid(text: impl Into<String>) -> AmqpErr {
    AmqpErr::new("amqp:invalid-field", text)
}

fn rejected_error() -> AmqpErr {
    AmqpErr::new("amqp:resource-limit-exceeded", "message rejected by a queue: maximum length reached or storage unavailable")
}

impl Session {
    pub fn begin(ctx: Arc<Ctx>, channel: u16, b: &Begin, out: &mut Out) -> Self {
        let id = ctx.broker.next_id();
        let info = Arc::new(ChannelInfo {
            id,
            conn: ctx.info.id,
            number: channel,
            conn_name: ctx.info.name.clone(),
            vhost: ctx.vhost.name.clone(),
            user: ctx.user.name.clone(),
            prefetch_count: 0.into(),
            global_prefetch_count: 0.into(),
            unacked: 0.into(),
            consumers: 0.into(),
            confirm: true.into(),
            transactional: false.into(),
            published: 0.into(),
            delivered: 0.into(),
            acked: 0.into(),
            confirmed: 0.into(),
            returned: 0.into(),
            redelivered: 0.into(),
        });
        ctx.broker.register_channel(info.clone());
        ctx.info.channels.fetch_add(1, Ordering::Relaxed);
        let s = Session {
            ctx,
            channel,
            id,
            info,
            next_incoming_id: b.next_outgoing_id,
            incoming_window: IN_WINDOW,
            next_outgoing_id: 0,
            remote_incoming_window: b.incoming_window,
            handle_max: b.handle_max.min(65535),
            next_delivery_id: 0,
            next_link_id: 0,
            incoming: HashMap::new(),
            outgoing: HashMap::new(),
            detaching: Default::default(),
            unsettled: BTreeMap::new(),
            pending: VecDeque::new(),
            accepted: Vec::new(),
            ending: false,
            perm_ok: HashSet::new(),
        };
        out.send(
            channel,
            &Performative::Begin(Begin {
                remote_channel: Some(channel),
                next_outgoing_id: s.next_outgoing_id,
                incoming_window: IN_WINDOW,
                outgoing_window: OUT_WINDOW,
                handle_max: s.handle_max,
                ..Default::default()
            }),
        );
        s
    }

    fn vh(&self) -> Arc<VHost> {
        self.ctx.vhost.clone()
    }

    fn check(&mut self, access: Access, is_queue: bool, name: &str) -> Result<(), AmqpErr> {
        let key = (access as u8, is_queue, name.to_string());
        if self.perm_ok.contains(&key) {
            return Ok(());
        }
        let kind = if is_queue { "queue" } else { "exchange" };
        self.ctx.broker.check(&self.ctx.user, &self.ctx.vhost.name, access, kind, name).map_err(|e| error_for(&e))?;
        if self.perm_ok.len() > 1024 {
            self.perm_ok.clear();
        }
        self.perm_ok.insert(key);
        Ok(())
    }

    fn session_flow(&self) -> Flow {
        Flow {
            next_incoming_id: Some(self.next_incoming_id),
            incoming_window: self.incoming_window,
            next_outgoing_id: self.next_outgoing_id,
            outgoing_window: OUT_WINDOW,
            ..Default::default()
        }
    }

    fn encode(&self, p: &Performative) -> Bytes {
        let mut b = BytesMut::new();
        write_frame(&mut b, FRAME_AMQP, self.channel, Some(&p.to_value()), &[]);
        b.freeze()
    }

    /// Send a frame, keeping it behind transfers still waiting for session window.
    fn send_ordered(&mut self, out: &mut Out, p: &Performative) {
        if self.pending.is_empty() {
            out.send(self.channel, p);
        } else {
            let f = self.encode(p);
            self.pending.push_back(Pending::Other(f));
        }
    }

    /// Write queued frames while the peer's incoming window allows.
    fn pump(&mut self, out: &mut Out) {
        while let Some(front) = self.pending.front() {
            match front {
                Pending::Transfer(..) if self.remote_incoming_window == 0 => break,
                Pending::Transfer(_, f) => {
                    out.raw(f);
                    self.next_outgoing_id = self.next_outgoing_id.wrapping_add(1);
                    self.remote_incoming_window -= 1;
                }
                Pending::Other(f) => out.raw(f),
            }
            self.pending.pop_front();
        }
    }

    /// End the session with an error; frames are ignored until the peer's end.
    fn fail(&mut self, out: &mut Out, e: AmqpErr) {
        debug!(channel = self.channel, error = %e, "ending AMQP 1.0 session");
        self.pending.clear();
        out.send(self.channel, &Performative::End(Some(e)));
        self.ending = true;
    }

    /// Refuse an attach: answer with a null terminus, then detach with the error.
    fn refuse(&mut self, out: &mut Out, a: &Attach, e: AmqpErr) {
        debug!(link = %a.name, error = %e, "refusing AMQP 1.0 link");
        let reply = Attach {
            name: a.name.clone(),
            handle: a.handle,
            role: !a.role,
            snd_settle_mode: a.snd_settle_mode,
            rcv_settle_mode: a.rcv_settle_mode,
            source: if a.role == ROLE_SENDER { a.source.clone() } else { None },
            target: if a.role == ROLE_RECEIVER { a.target.clone() } else { None },
            initial_delivery_count: (a.role == ROLE_RECEIVER).then_some(0),
            ..Default::default()
        };
        self.send_ordered(out, &Performative::Attach(Box::new(reply)));
        self.detach_link(out, a.handle, e);
    }

    /// Handle a performative on this session; returns true once the session is over.
    pub async fn on_performative(&mut self, p: Performative, payload: Bytes, out: &mut Out) -> bool {
        if self.ending {
            return matches!(p, Performative::End(_));
        }
        let r = match p {
            Performative::End(e) => {
                if let Some(e) = e {
                    debug!(channel = self.channel, error = %e, "client ended session with an error");
                }
                self.pump(out);
                out.send(self.channel, &Performative::End(None));
                return true;
            }
            Performative::Attach(a) => {
                self.attach(*a, out).await;
                Ok(())
            }
            Performative::Flow(f) => self.flow(f, out),
            Performative::Transfer(t) => self.transfer(t, payload, out).await,
            Performative::Disposition(d) => {
                self.disposition(d, out).await;
                Ok(())
            }
            Performative::Detach(d) => {
                self.detach(d, out).await;
                Ok(())
            }
            p => Err(AmqpErr::new("amqp:illegal-state", format!("unexpected {} on a session", p.name()))),
        };
        if let Err(e) = r {
            self.fail(out, e);
        }
        self.pump(out);
        false
    }

    // ── attach ──

    async fn attach(&mut self, a: Attach, out: &mut Out) {
        if a.handle > self.handle_max {
            self.fail(out, AmqpErr::new("amqp:connection:framing-error", format!("link handle {} exceeds handle-max {}", a.handle, self.handle_max)));
            return;
        }
        if self.incoming.contains_key(&a.handle) || self.outgoing.contains_key(&a.handle) {
            self.fail(out, AmqpErr::new("amqp:session:handle-in-use", format!("link handle {} is in use", a.handle)));
            return;
        }
        self.detaching.remove(&a.handle);
        let r = if a.role == ROLE_SENDER { self.attach_incoming(&a, out).await } else { self.attach_outgoing(&a, out).await };
        if let Err(e) = r {
            self.refuse(out, &a, e);
        }
    }

    fn link_id(&mut self) -> u64 {
        self.next_link_id += 1;
        self.next_link_id
    }

    /// The client sends: a publishing link (or the management request link).
    async fn attach_incoming(&mut self, a: &Attach, out: &mut Out) -> Result<(), AmqpErr> {
        let target = match &a.target {
            Some(TargetKind::Coordinator) => return Err(AmqpErr::new("amqp:not-implemented", "transactions are not supported")),
            Some(TargetKind::Target(t)) => t.clone(),
            None => return Err(invalid("Attach refused: target is required")),
        };
        let mut reply_target = target.clone();
        let (kind, max_credit) = if target.address.as_deref() == Some(mgmt::ADDRESS) {
            (InKind::Management, MGMT_CREDIT)
        } else if target.dynamic {
            if !target.capabilities.iter().any(|c| c == "temporary-queue") {
                return Err(AmqpErr::new("amqp:not-implemented", "Dynamic target not supported"));
            }
            let q = self.declare_temp_queue().await?;
            reply_target.address = Some(address::queue_address(&q));
            reply_target.expiry_policy = Some("link-detach".into());
            self.check(Access::Write, false, "")?;
            (InKind::Publish(PubTarget::Fixed { exchange: String::new(), key: Some(q) }), self.ctx.config.max_link_credit)
        } else {
            let t = match target.address.as_deref() {
                None => PubTarget::Anonymous,
                Some(addr) => match address::parse_target(addr) {
                    None => return Err(invalid(format!("Attach refused: bad address '{addr}'"))),
                    Some(t) => self.resolve_target(t).await?,
                },
            };
            (InKind::Publish(t), self.ctx.config.max_link_credit)
        };
        let delivery_count = a.initial_delivery_count.unwrap_or(0);
        let link_id = self.link_id();
        self.incoming.insert(
            a.handle,
            InLink { link_id, name: a.name.clone(), kind, delivery_count, credit: max_credit, max_credit, unconfirmed: 0, partial: None },
        );
        let reply = Attach {
            name: a.name.clone(),
            handle: a.handle,
            role: ROLE_RECEIVER,
            snd_settle_mode: a.snd_settle_mode,
            rcv_settle_mode: 0,
            source: a.source.clone(),
            target: Some(TargetKind::Target(reply_target)),
            max_message_size: Some(self.ctx.broker.config.max_message_size),
            properties: a.properties.clone(),
            ..Default::default()
        };
        self.send_ordered(out, &Performative::Attach(Box::new(reply)));
        let flow = Flow { handle: Some(a.handle), delivery_count: Some(delivery_count), link_credit: Some(max_credit), ..self.session_flow() };
        self.send_ordered(out, &Performative::Flow(flow));
        Ok(())
    }

    fn exchange_checks(&mut self, x: &str) -> Result<(), AmqpErr> {
        self.check(Access::Write, false, x)?;
        let vh = self.vh();
        let ex = vh.exchange(x).ok_or_else(|| AmqpErr::new("amqp:not-found", format!("no exchange '{x}' in vhost '{}'", vh.name)))?;
        if ex.internal {
            return Err(AmqpErr::new("amqp:unauthorized-access", format!("forbidden to publish to internal exchange '{x}' in vhost '{}'", vh.name)));
        }
        Ok(())
    }

    async fn resolve_target(&mut self, t: TargetAddr) -> Result<PubTarget, AmqpErr> {
        Ok(match t {
            TargetAddr::Exchange { exchange, key } => {
                self.exchange_checks(&exchange)?;
                PubTarget::Fixed { exchange, key }
            }
            TargetAddr::Queue(q) => {
                self.check(Access::Write, false, "")?;
                let vh = self.vh();
                if vh.queue(&q).is_none() {
                    return Err(AmqpErr::new("amqp:not-found", format!("no queue '{q}' in vhost '{}'", vh.name)));
                }
                PubTarget::Fixed { exchange: String::new(), key: Some(q) }
            }
            TargetAddr::DeclareQueue(q) => {
                self.check(Access::Write, false, "")?;
                if q.is_empty() {
                    PubTarget::Fixed { exchange: String::new(), key: None }
                } else {
                    self.ensure_queue(&q).await?;
                    PubTarget::Fixed { exchange: String::new(), key: Some(q) }
                }
            }
        })
    }

    /// v1 `/queue/:q`: declare a durable queue unless it exists.
    async fn ensure_queue(&mut self, q: &str) -> Result<(), AmqpErr> {
        let vh = self.vh();
        if vh.queue(q).is_some() {
            return Ok(());
        }
        self.check(Access::Configure, true, q)?;
        let d = DeclareQueue { name: q.into(), durable: true, exclusive_owner: None, auto_delete: false, args: FieldTable::new(), passive: false };
        vh.declare_queue(d).await.map_err(|e| error_for(&e))?;
        Ok(())
    }

    /// A private queue owned by this connection.
    async fn declare_temp_queue(&mut self) -> Result<String, AmqpErr> {
        let vh = self.vh();
        let name = mq_core::vhost::generate_queue_name();
        self.check(Access::Configure, true, &name)?;
        let d = DeclareQueue { name: name.clone(), durable: false, exclusive_owner: Some(self.ctx.info.id), auto_delete: false, args: FieldTable::new(), passive: false };
        vh.declare_queue(d).await.map_err(|e| error_for(&e))?;
        Ok(name)
    }

    /// The client receives: a consuming link (or the management response link).
    async fn attach_outgoing(&mut self, a: &Attach, out: &mut Out) -> Result<(), AmqpErr> {
        let Some(source) = a.source.clone() else { return Err(invalid("Attach refused: source_required")) };
        let send_settled = a.snd_settle_mode == SND_SETTLED;
        if source.address.as_deref() == Some(mgmt::ADDRESS) {
            let link_id = self.link_id();
            self.outgoing.insert(
                a.handle,
                OutLink {
                    link_id,
                    name: a.name.clone(),
                    kind: OutKind::Management { credit: 0, delivery_count: 0, pending: VecDeque::new() },
                    send_settled: true,
                    max_message_size: a.max_message_size,
                },
            );
            let reply = Attach {
                name: a.name.clone(),
                handle: a.handle,
                role: ROLE_SENDER,
                snd_settle_mode: SND_SETTLED,
                rcv_settle_mode: a.rcv_settle_mode,
                source: Some(source),
                target: a.target.clone(),
                initial_delivery_count: Some(0),
                max_message_size: a.max_message_size,
                properties: a.properties.clone(),
                ..Default::default()
            };
            self.send_ordered(out, &Performative::Attach(Box::new(reply)));
            return Ok(());
        }

        let mut source_reply = source.clone();
        let mut temp_queue = None;
        let queue_name = if source.dynamic {
            let q = self.declare_temp_queue().await?;
            source_reply.address = Some(address::queue_address(&q));
            source_reply.expiry_policy = Some("link-detach".into());
            temp_queue = Some(q.clone());
            q
        } else {
            let addr = source.address.clone().unwrap_or_default();
            match address::parse_source(&addr) {
                None => return Err(invalid(format!("Attach refused: bad address '{addr}'"))),
                Some(SourceAddr::Queue(q)) => q,
                Some(SourceAddr::DeclareQueue(q)) => {
                    self.ensure_queue(&q).await?;
                    q
                }
                Some(SourceAddr::Exchange { exchange, key }) => {
                    let vh = self.vh();
                    let ex = vh.exchange(&exchange).ok_or_else(|| AmqpErr::new("amqp:not-found", format!("no exchange '{exchange}' in vhost '{}'", vh.name)))?;
                    self.check(Access::Read, false, &exchange)?;
                    if ex.kind == ExchangeKind::Topic {
                        self.ctx.broker.check_topic(&self.ctx.user, &vh.name, &exchange, &key, Access::Read, None).map_err(|e| error_for(&e))?;
                    }
                    let q = self.declare_temp_queue().await?;
                    self.check(Access::Write, true, &q)?;
                    vh.bind(&exchange, Dest::queue(&q), &key, FieldTable::new(), Some(self.ctx.info.id)).await.map_err(|e| error_for(&e))?;
                    temp_queue = Some(q.clone());
                    q
                }
            }
        };
        let vh = self.vh();
        let queue = match vh.queue_for(&queue_name, Some(self.ctx.info.id)) {
            Ok(q) => q,
            Err(e) if e.code == RESOURCE_LOCKED => {
                return Err(AmqpErr::new("amqp:resource-locked", format!("cannot obtain exclusive access to locked queue '{queue_name}' in vhost '{}'", vh.name)));
            }
            Err(e) => return Err(error_for(&e)),
        };
        self.check(Access::Read, true, &queue_name)?;

        // Consumer arguments from link properties and filters.
        let mut args = FieldTable::new();
        let mut priority = 0;
        if let Some(Value::Map(props)) = &a.properties {
            for (k, v) in props {
                match (k.as_str(), v) {
                    (Some("rabbitmq:priority"), Value::Int(p)) => {
                        priority = *p;
                        args.insert("x-priority", FieldValue::I32(*p));
                    }
                    (Some("rabbitmq:consumer-timeout"), Value::Uint(t)) => args.insert("x-consumer-timeout", FieldValue::U32(*t)),
                    _ => {}
                }
            }
        }
        let is_stream = queue.kind == QueueType::Stream;
        let mut effective = Vec::new();
        let mut pf = PropFilter { properties: vec![], application: vec![] };
        for (k, v) in source.filter.iter().flatten() {
            let Value::Described(d, inner) = v else { continue };
            let accepted = match (d.as_ref(), inner.as_ref()) {
                (Value::Symbol(n), val) if n == "rabbitmq:stream-offset-spec" && is_stream => {
                    let fv = match val {
                        Value::Timestamp(ms) => Some(FieldValue::Timestamp((*ms / 1000).max(0) as u64)),
                        Value::String(s) | Value::Symbol(s) => Some(FieldValue::str(s.clone())),
                        v => v.as_u64().map(|o| FieldValue::I64(o as i64)),
                    };
                    fv.map(|fv| args.insert("x-stream-offset", fv)).is_some()
                }
                (Value::Symbol(n), val) if n == "rabbitmq:stream-filter" && is_stream => match val {
                    Value::String(s) | Value::Symbol(s) => {
                        args.insert("x-stream-filter", FieldValue::str(s.clone()));
                        true
                    }
                    Value::List(l) | Value::Array(l) => {
                        args.insert("x-stream-filter", FieldValue::Array(l.iter().filter_map(|x| x.as_str().map(FieldValue::str)).collect()));
                        true
                    }
                    _ => false,
                },
                (Value::Symbol(n), Value::Bool(b)) if n == "rabbitmq:stream-match-unfiltered" && is_stream => {
                    args.insert("x-stream-match-unfiltered", FieldValue::Bool(*b));
                    true
                }
                (d, Value::Map(m)) if is_stream && is_filter(d, "amqp:properties-filter", PROPERTIES_FILTER) => {
                    let mut ok = true;
                    for (pk, pv) in m {
                        match pk.as_str().and_then(property_index) {
                            Some(i) => pf.properties.push((i, pv.clone())),
                            None => ok = false,
                        }
                    }
                    ok
                }
                (d, Value::Map(m)) if is_stream && is_filter(d, "amqp:application-properties-filter", APP_PROPERTIES_FILTER) => {
                    for (pk, pv) in m {
                        if let Some(k) = pk.as_str() {
                            pf.application.push((k.to_string(), pv.clone()));
                        }
                    }
                    true
                }
                _ => false,
            };
            if accepted {
                effective.push((k.clone(), v.clone()));
            }
        }
        let filter = (!pf.properties.is_empty() || !pf.application.is_empty()).then_some(pf);

        let link_id = self.link_id();
        let shared = Arc::new(LinkShared {
            channel: self.channel,
            session: self.id,
            handle: a.handle,
            link_id,
            queue: queue.clone(),
            credit: Mutex::new(Credit::default()),
            closed: AtomicBool::new(false),
            events: self.ctx.events.clone(),
        });
        let tag: Arc<str> = Arc::from(format!("ctag-{}-{}", a.handle, link_id).as_str());
        let spec = ConsumerSpec {
            tag: tag.clone(),
            channel: self.id,
            conn: self.ctx.info.id,
            no_ack: send_settled,
            exclusive: false,
            priority,
            args,
            sink: Arc::new(Sink(shared.clone())),
        };
        if let Err(e) = queue.consume(spec).await {
            if let Some(q) = temp_queue {
                let _ = vh.delete_queue(&q, false, false, Some(self.ctx.info.id)).await;
            }
            return Err(error_for(&e));
        }
        self.info.consumers.fetch_add(1, Ordering::Relaxed);
        source_reply.filter = (!effective.is_empty()).then_some(effective);
        source_reply.default_outcome = Some(DeliveryState::Released.to_value());
        source_reply.outcomes = OUTCOMES.iter().map(|s| s.to_string()).collect();
        source_reply.capabilities = vec![];
        let reply = Attach {
            name: a.name.clone(),
            handle: a.handle,
            role: ROLE_SENDER,
            snd_settle_mode: if send_settled { SND_SETTLED } else { SND_UNSETTLED },
            rcv_settle_mode: a.rcv_settle_mode,
            source: Some(source_reply),
            target: a.target.clone(),
            initial_delivery_count: Some(0),
            max_message_size: a.max_message_size,
            properties: a.properties.clone(),
            ..Default::default()
        };
        self.send_ordered(out, &Performative::Attach(Box::new(reply)));
        self.outgoing.insert(
            a.handle,
            OutLink { link_id, name: a.name.clone(), kind: OutKind::Queue { shared, tag, temp_queue, filter }, send_settled, max_message_size: a.max_message_size },
        );
        Ok(())
    }

    // ── flow ──

    fn flow(&mut self, f: Flow, out: &mut Out) -> Result<(), AmqpErr> {
        let nid = f.next_incoming_id.unwrap_or(0);
        self.remote_incoming_window = nid.wrapping_add(f.incoming_window).wrapping_sub(self.next_outgoing_id);
        if self.remote_incoming_window > f.incoming_window {
            self.remote_incoming_window = 0;
        }
        let Some(handle) = f.handle else {
            if f.echo {
                let sf = self.session_flow();
                self.send_ordered(out, &Performative::Flow(sf));
            }
            return Ok(());
        };
        if let Some(l) = self.incoming.get(&handle) {
            if f.echo {
                let flow = Flow { handle: Some(handle), delivery_count: Some(l.delivery_count), link_credit: Some(l.credit), ..self.session_flow() };
                self.send_ordered(out, &Performative::Flow(flow));
            }
            return Ok(());
        }
        let Some(link) = self.outgoing.get_mut(&handle) else {
            return Err(AmqpErr::new("amqp:session:unattached-handle", format!("flow for unattached link handle {handle}")));
        };
        let link_credit = f.link_credit.unwrap_or(0);
        match &mut link.kind {
            OutKind::Management { credit, delivery_count, .. } => {
                let rcv = f.delivery_count.unwrap_or(*delivery_count);
                let c = rcv.wrapping_add(link_credit).wrapping_sub(*delivery_count);
                *credit = if c > link_credit { 0 } else { c };
                if f.drain {
                    *delivery_count = delivery_count.wrapping_add(*credit);
                    *credit = 0;
                }
                let (dc, cr) = (*delivery_count, *credit);
                self.pump_mgmt(handle, out);
                if f.drain || f.echo {
                    let flow = Flow { handle: Some(handle), delivery_count: Some(dc), link_credit: Some(cr), drain: f.drain, ..self.session_flow() };
                    self.send_ordered(out, &Performative::Flow(flow));
                }
            }
            OutKind::Queue { shared, .. } => {
                let shared = shared.clone();
                let (dc, cr) = {
                    let mut c = shared.credit.lock();
                    let rcv = f.delivery_count.unwrap_or(c.delivery_count);
                    let n = rcv.wrapping_add(link_credit).wrapping_sub(c.delivery_count);
                    c.credit = if n > link_credit { 0 } else { n };
                    c.drain = f.drain;
                    (c.delivery_count, c.credit)
                };
                shared.queue.wake();
                if f.drain {
                    // Report back once the queue has delivered what it can.
                    let ev = self.ctx.events.clone();
                    tokio::spawn(async move {
                        let q = shared.queue.clone();
                        let _ = q.consumers().await;
                        let _ = q.consumers().await;
                        let _ = ev.send(Event::DrainCheck { link: shared });
                    });
                } else if f.echo {
                    let flow = Flow { handle: Some(handle), delivery_count: Some(dc), link_credit: Some(cr), ..self.session_flow() };
                    self.send_ordered(out, &Performative::Flow(flow));
                }
            }
        }
        Ok(())
    }

    // ── transfers from the client ──

    async fn transfer(&mut self, t: Transfer, payload: Bytes, out: &mut Out) -> Result<(), AmqpErr> {
        self.next_incoming_id = self.next_incoming_id.wrapping_add(1);
        self.incoming_window = self.incoming_window.saturating_sub(1);
        if self.incoming_window < IN_WINDOW / 2 {
            self.incoming_window = IN_WINDOW;
            let sf = self.session_flow();
            out.send(self.channel, &Performative::Flow(sf));
        }
        let max = self.ctx.broker.config.max_message_size;
        let Some(link) = self.incoming.get_mut(&t.handle) else {
            return Err(AmqpErr::new("amqp:session:unattached-handle", format!("transfer on unattached link handle {}", t.handle)));
        };
        let p = link.partial.get_or_insert_with(|| Partial { delivery_id: t.delivery_id, settled: false, payload: BytesMut::new() });
        p.settled |= t.settled.unwrap_or(false);
        if p.delivery_id.is_none() {
            p.delivery_id = t.delivery_id;
        }
        if t.aborted {
            link.partial = None;
            link.delivery_count = link.delivery_count.wrapping_add(1);
            link.credit = link.credit.saturating_sub(1);
            return Ok(());
        }
        if (p.payload.len() + payload.len()) as u64 > max {
            let size = p.payload.len() + payload.len();
            let handle = t.handle;
            self.incoming.remove(&handle);
            let e = AmqpErr::new("amqp:link:message-size-exceeded", format!("message size ({size} bytes) > maximum message size ({max} bytes)"));
            self.detach_link(out, handle, e);
            return Ok(());
        }
        p.payload.extend_from_slice(&payload);
        if t.more {
            return Ok(());
        }
        let Some(p) = link.partial.take() else { return Ok(()) };
        link.delivery_count = link.delivery_count.wrapping_add(1);
        link.credit = link.credit.saturating_sub(1);
        let payload = p.payload.freeze();
        match link.kind {
            InKind::Management => self.management_request(t.handle, p.delivery_id, p.settled, payload, out).await,
            InKind::Publish(_) => self.publish(t.handle, p.delivery_id, p.settled, payload, out).await,
        }
        self.maybe_grant(t.handle, out);
        Ok(())
    }

    fn maybe_grant(&mut self, handle: u32, out: &mut Out) {
        let Some(l) = self.incoming.get_mut(&handle) else { return };
        if l.credit <= l.max_credit / 2 && l.unconfirmed < l.max_credit {
            l.credit = l.max_credit;
            let flow = Flow { handle: Some(handle), delivery_count: Some(l.delivery_count), link_credit: Some(l.credit), ..self.session_flow() };
            self.send_ordered(out, &Performative::Flow(flow));
        }
    }

    fn disposition_out(&mut self, out: &mut Out, id: u32, state: DeliveryState) {
        let d = Disposition { role: ROLE_RECEIVER, first: id, last: None, settled: true, state: Some(state), batchable: false };
        self.send_ordered(out, &Performative::Disposition(d));
    }

    fn detach_in(&mut self, out: &mut Out, handle: u32, e: AmqpErr) {
        self.incoming.remove(&handle);
        self.detach_link(out, handle, e);
    }

    async fn publish(&mut self, handle: u32, delivery_id: Option<u32>, settled: bool, payload: Bytes, out: &mut Out) {
        let reject = |s: &mut Self, out: &mut Out, e: AmqpErr| {
            if let (false, Some(id)) = (settled, delivery_id) {
                s.disposition_out(out, id, DeliveryState::Rejected(Some(e)));
            }
        };
        let m = match Amqp1Message::decode(payload) {
            Ok(m) => m,
            Err(e) => return reject(self, out, AmqpErr::new("amqp:decode-error", e.0)),
        };
        let Some(link) = self.incoming.get(&handle) else { return };
        let link_id = link.link_id;
        let subject = m.prop_str(prop::SUBJECT).unwrap_or_default().to_string();
        let (exchange, key, anonymous) = match &link.kind {
            InKind::Publish(PubTarget::Fixed { exchange, key }) => (exchange.clone(), key.clone().unwrap_or(subject), false),
            InKind::Publish(PubTarget::Anonymous) => {
                let to = match m.prop(prop::TO) {
                    Value::String(s) => s.clone(),
                    _ => {
                        let e = AmqpErr::new("amqp:precondition-failed", "anonymous terminus requires 'to' address to be set");
                        return self.anonymous_error(out, handle, delivery_id, settled, e);
                    }
                };
                match address::parse_target_v2(&to) {
                    Some(TargetAddr::Exchange { exchange, key }) => (exchange, key.unwrap_or_default(), true),
                    Some(TargetAddr::Queue(q)) => (String::new(), q, true),
                    _ => {
                        let e = AmqpErr::new("amqp:precondition-failed", format!("bad 'to' address string: {to}"));
                        return self.anonymous_error(out, handle, delivery_id, settled, e);
                    }
                }
            }
            InKind::Management => return,
        };
        if anonymous && let Err(e) = self.exchange_checks(&exchange) {
            return self.anonymous_error(out, handle, delivery_id, settled, e);
        }
        let vh = self.vh();
        if !anonymous && vh.exchange(&exchange).is_none() {
            // The link's exchange was deleted.
            if let (false, Some(id)) = (settled, delivery_id) {
                self.disposition_out(out, id, DeliveryState::Released);
            }
            let e = AmqpErr::new("amqp:not-found", format!("no exchange '{exchange}' in vhost '{}'", vh.name));
            return self.detach_in(out, handle, e);
        }
        let (props, body) = message::to_core(&m);
        if let Some(uid) = &props.user_id
            && *uid != self.ctx.user.name
            && !self.ctx.user.has_tag("impersonator")
        {
            let e = AmqpErr::new("amqp:unauthorized-access", format!("user_id property set to '{uid}' but authenticated user was '{}'", self.ctx.user.name));
            return self.detach_in(out, handle, e);
        }
        if !exchange.is_empty()
            && let Some(ex) = vh.exchange(&exchange)
            && ex.kind == ExchangeKind::Topic
            && let Err(e) = self.ctx.broker.check_topic(&self.ctx.user, &vh.name, &exchange, &key, Access::Write, None)
        {
            return reject(self, out, error_for(&e));
        }
        self.info.published.fetch_add(1, Ordering::Relaxed);
        let msg = Message::new(exchange.clone(), key, props, body);
        let unroutable = Arc::new(AtomicBool::new(false));
        let ticket = match (settled, delivery_id) {
            (false, Some(id)) => {
                if let Some(l) = self.incoming.get_mut(&handle) {
                    l.unconfirmed += 1;
                }
                let ev = self.ctx.events.clone();
                let (channel, session, flag) = (self.channel, self.id, unroutable.clone());
                Some(ConfirmTicket::new(move |ok| {
                    let result = if !ok {
                        Confirm::Rejected
                    } else if flag.load(Ordering::Acquire) {
                        Confirm::Released
                    } else {
                        Confirm::Accepted
                    };
                    let _ = ev.send(Event::Confirmed { channel, session, handle, link_id, delivery_id: id, result });
                }))
            }
            _ => None,
        };
        match vh.publish(&exchange, msg, ticket).await {
            Ok(o) => {
                if o.routed == 0 {
                    unroutable.store(true, Ordering::Release);
                    self.info.returned.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(t) = o.confirm {
                    t.settle(true);
                }
            }
            Err(e) => debug!(exchange = %exchange, error = %e, "AMQP 1.0 publish failed"),
        }
    }

    /// Routing errors on the anonymous terminus: reject unsettled deliveries, detach otherwise.
    fn anonymous_error(&mut self, out: &mut Out, handle: u32, delivery_id: Option<u32>, settled: bool, mut e: AmqpErr) {
        match (settled, delivery_id) {
            (false, Some(id)) => self.disposition_out(out, id, DeliveryState::Rejected(Some(e))),
            _ => {
                e.info = delivery_id.map(|id| Value::Map(vec![(Value::sym("delivery-id"), Value::Uint(id))]));
                self.detach_in(out, handle, e);
            }
        }
    }

    async fn management_request(&mut self, handle: u32, delivery_id: Option<u32>, settled: bool, payload: Bytes, out: &mut Out) {
        let Some(name) = self.incoming.get(&handle).map(|l| l.name.clone()) else { return };
        let req = match Amqp1Message::decode(payload) {
            Ok(m) => m,
            Err(e) => {
                if let (false, Some(id)) = (settled, delivery_id) {
                    self.disposition_out(out, id, DeliveryState::Rejected(Some(AmqpErr::new("amqp:decode-error", e.0))));
                }
                return;
            }
        };
        if let (false, Some(id)) = (settled, delivery_id) {
            self.disposition_out(out, id, DeliveryState::Accepted);
        }
        if req.prop_str(prop::REPLY_TO) != Some("$me") {
            debug!("management request without reply-to '$me' ignored");
        }
        let ctx = self.ctx.clone();
        let mctx = mgmt::Ctx { broker: &ctx.broker, vhost: &ctx.vhost, user: &ctx.user, conn: ctx.info.id };
        let resp = mgmt::handle(&mctx, &req).await;
        // RabbitMQ pairs by link name; clients that cannot reuse a name get the session's
        // only management receiver.
        let mgmt_links: Vec<(u32, bool)> =
            self.outgoing.iter().filter(|(_, l)| matches!(l.kind, OutKind::Management { .. })).map(|(h, l)| (*h, l.name == name)).collect();
        let paired = mgmt_links.iter().find(|(_, same)| *same).or(if mgmt_links.len() == 1 { mgmt_links.first() } else { None });
        let Some(&(h, _)) = paired else {
            debug!(link = %name, "management response dropped: no paired receiving link");
            return;
        };
        if let Some(OutLink { kind: OutKind::Management { pending, .. }, .. }) = self.outgoing.get_mut(&h) {
            pending.push_back(resp);
        }
        self.pump_mgmt(h, out);
    }

    fn pump_mgmt(&mut self, handle: u32, out: &mut Out) {
        loop {
            let Some(OutLink { kind: OutKind::Management { credit, delivery_count, pending }, .. }) = self.outgoing.get_mut(&handle) else { return };
            if *credit == 0 {
                return;
            }
            let Some(m) = pending.pop_front() else { return };
            *credit -= 1;
            *delivery_count = delivery_count.wrapping_add(1);
            let mut payload = BytesMut::new();
            m.encode(&mut payload);
            let id = self.next_delivery_id;
            self.next_delivery_id = id.wrapping_add(1);
            self.queue_transfer(handle, id, true, payload.freeze(), out.max_frame);
            self.pump(out);
        }
    }

    /// Split a delivery into transfer frames and queue them.
    fn queue_transfer(&mut self, handle: u32, delivery_id: u32, settled: bool, payload: Bytes, max_frame: usize) {
        let mut rest = payload;
        let mut first = true;
        loop {
            let t = Transfer {
                handle,
                delivery_id: first.then_some(delivery_id),
                delivery_tag: first.then(|| Bytes::copy_from_slice(&delivery_id.to_be_bytes())),
                message_format: first.then_some(0),
                settled: first.then_some(settled),
                more: true,
                ..Default::default()
            };
            let with_more = Performative::Transfer(t.clone()).to_value().to_bytes();
            let room = max_frame.saturating_sub(8 + with_more.len()).max(1);
            let mut f = BytesMut::new();
            if rest.len() <= room {
                let last = Performative::Transfer(Transfer { more: false, ..t });
                write_frame(&mut f, FRAME_AMQP, self.channel, Some(&last.to_value()), &rest);
                self.pending.push_back(Pending::Transfer(handle, f.freeze()));
                return;
            }
            let chunk = rest.split_to(room);
            f.extend_from_slice(&[0, 0, 0, 0, 2, FRAME_AMQP]);
            f.extend_from_slice(&self.channel.to_be_bytes());
            f.extend_from_slice(&with_more);
            f.extend_from_slice(&chunk);
            let n = f.len() as u32;
            f[0..4].copy_from_slice(&n.to_be_bytes());
            self.pending.push_back(Pending::Transfer(handle, f.freeze()));
            first = false;
        }
    }

    // ── dispositions from the client ──

    async fn disposition(&mut self, d: Disposition, out: &mut Out) {
        if d.role != ROLE_RECEIVER {
            return;
        }
        let last = d.last.unwrap_or(d.first);
        if last < d.first {
            return;
        }
        let ids: Vec<u32> = self.unsettled.range(d.first..=last).map(|(k, _)| *k).collect();
        if ids.is_empty() {
            return;
        }
        let op = match &d.state {
            Some(DeliveryState::Accepted) => Some(Outcome::Ack),
            Some(DeliveryState::Rejected(_)) => Some(Outcome::Reject),
            Some(DeliveryState::Modified { undeliverable_here: true, .. }) => Some(Outcome::Reject),
            Some(DeliveryState::Modified { delivery_failed: true, .. }) => Some(Outcome::Requeue),
            // released, modified without flags, or settled without an outcome (default-outcome).
            _ => None,
        };
        // RabbitMQ 4 merges a `modified` outcome's `x-opt-*` annotations into the message.
        let modification = match &d.state {
            Some(DeliveryState::Modified { delivery_failed, undeliverable_here, message_annotations: Some(Value::Map(anns)) }) => {
                let mut annotations = mq_core::FieldTable::new();
                for (k, v) in anns {
                    if let (Some(k), Some(v)) = (k.as_str().filter(|k| k.starts_with("x-opt-")), message::to_091(v)) {
                        annotations.insert(k, v);
                    }
                }
                (!annotations.is_empty()).then_some(Modification { annotations, delivery_failed: *delivery_failed, undeliverable_here: *undeliverable_here })
            }
            _ => None,
        };
        let mut by_queue: Vec<(QueueRef, Vec<u64>)> = Vec::new();
        for id in &ids {
            let Some(u) = self.unsettled.remove(id) else { continue };
            match by_queue.iter_mut().find(|(q, _)| Arc::ptr_eq(q, &u.queue)) {
                Some((_, v)) => v.push(u.seq),
                None => by_queue.push((u.queue, vec![u.seq])),
            }
        }
        let n = ids.len() as u64;
        self.info.unacked.fetch_sub(n.min(self.info.unacked.load(Ordering::Relaxed)), Ordering::Relaxed);
        if op == Some(Outcome::Ack) {
            self.info.acked.fetch_add(n, Ordering::Relaxed);
            self.ctx.vhost.stats.acked.fetch_add(n, Ordering::Relaxed);
        }
        for (q, seqs) in by_queue {
            match (&modification, op) {
                (Some(m), _) => q.modify(self.id, seqs, m.clone()).await,
                (None, Some(o)) => q.settle(self.id, seqs, o).await,
                (None, None) => q.release(self.id, seqs).await,
            }
        }
        if !d.settled {
            let reply = Disposition { role: ROLE_SENDER, first: d.first, last: d.last, settled: true, state: d.state, batchable: false };
            self.send_ordered(out, &Performative::Disposition(reply));
        }
    }

    // ── detach ──

    async fn detach(&mut self, d: Detach, out: &mut Out) {
        if let Some(e) = &d.error {
            debug!(handle = d.handle, error = %e, "client detached link with an error");
        }
        if self.incoming.remove(&d.handle).is_none()
            && let Some(link) = self.outgoing.remove(&d.handle)
        {
            self.close_outgoing(d.handle, link).await;
        }
        if self.detaching.remove(&d.handle) {
            return; // the answer to our own detach
        }
        self.send_ordered(out, &Performative::Detach(Detach { handle: d.handle, closed: d.closed, error: None }));
    }

    /// Detach a link with an error; the client answers with its own detach.
    fn detach_link(&mut self, out: &mut Out, handle: u32, e: AmqpErr) {
        self.detaching.insert(handle);
        self.send_ordered(out, &Performative::Detach(Detach { handle, closed: true, error: Some(e) }));
    }

    /// Stop a consuming link: cancel the consumer, return what it held, drop its temp queue.
    async fn close_outgoing(&mut self, handle: u32, link: OutLink) {
        self.pending.retain(|p| !matches!(p, Pending::Transfer(h, _) if *h == handle));
        let OutKind::Queue { shared, tag, temp_queue, .. } = link.kind else { return };
        shared.closed.store(true, Ordering::Release);
        shared.queue.cancel(self.id, tag).await;
        self.info.consumers.fetch_sub(1, Ordering::Relaxed);
        let held: Vec<u32> = self.unsettled.iter().filter(|(_, u)| u.handle == handle).map(|(k, _)| *k).collect();
        let mut seqs = Vec::new();
        for id in held {
            if let Some(u) = self.unsettled.remove(&id) {
                seqs.push(u.seq);
            }
        }
        if !seqs.is_empty() {
            self.info.unacked.fetch_sub((seqs.len() as u64).min(self.info.unacked.load(Ordering::Relaxed)), Ordering::Relaxed);
            shared.queue.release(self.id, seqs).await;
        }
        if let Some(q) = temp_queue {
            let _ = self.ctx.vhost.delete_queue(&q, false, false, Some(self.ctx.info.id)).await;
        }
    }

    // ── events ──

    pub async fn on_event(&mut self, ev: Event, out: &mut Out) {
        match ev {
            Event::Deliver { link, d } => self.deliver(link, d, out).await,
            Event::Cancelled { link } => {
                if self.link_matches(&link) && !self.ending {
                    let vh = self.ctx.vhost.name.clone();
                    if let Some(l) = self.outgoing.remove(&link.handle) {
                        let qn = link.queue.meta.name.clone();
                        self.close_outgoing(link.handle, l).await;
                        let e = AmqpErr::new("amqp:resource-deleted", format!("queue '{qn}' in vhost '{vh}' deleted"));
                        self.detach_link(out, link.handle, e);
                    }
                }
            }
            Event::Activated { link, active } => {
                if self.link_matches(&link) && !self.ending {
                    let (dc, cr) = {
                        let c = link.credit.lock();
                        (c.delivery_count, c.credit)
                    };
                    let flow = Flow {
                        handle: Some(link.handle),
                        delivery_count: Some(dc),
                        link_credit: Some(cr),
                        properties: Some(Value::Map(vec![(Value::sym("rabbitmq:active"), Value::Bool(active))])),
                        ..self.session_flow()
                    };
                    self.send_ordered(out, &Performative::Flow(flow));
                }
            }
            Event::DrainCheck { link } => {
                if self.link_matches(&link) && !self.ending {
                    let (dc, cr) = {
                        let mut c = link.credit.lock();
                        if c.drain {
                            c.delivery_count = c.delivery_count.wrapping_add(c.credit);
                            c.credit = 0;
                            c.drain = false;
                        }
                        (c.delivery_count, c.credit)
                    };
                    let flow = Flow { handle: Some(link.handle), delivery_count: Some(dc), link_credit: Some(cr), drain: true, ..self.session_flow() };
                    self.send_ordered(out, &Performative::Flow(flow));
                }
            }
            Event::Confirmed { session, handle, link_id, delivery_id, result, .. } => {
                if session != self.id || self.ending {
                    return;
                }
                if let Some(l) = self.incoming.get_mut(&handle)
                    && l.link_id == link_id
                {
                    l.unconfirmed = l.unconfirmed.saturating_sub(1);
                }
                self.info.confirmed.fetch_add(1, Ordering::Relaxed);
                match result {
                    Confirm::Accepted => self.accepted.push(delivery_id),
                    Confirm::Released => self.disposition_out(out, delivery_id, DeliveryState::Released),
                    Confirm::Rejected => self.disposition_out(out, delivery_id, DeliveryState::Rejected(Some(rejected_error()))),
                }
                self.maybe_grant(handle, out);
            }
        }
        self.pump(out);
    }

    fn link_matches(&self, link: &LinkShared) -> bool {
        link.session == self.id && self.outgoing.get(&link.handle).is_some_and(|l| l.link_id == link.link_id)
    }

    /// Accepted outcomes collected during an event batch, sent as ranges.
    pub fn flush_dispositions(&mut self, out: &mut Out) {
        if self.accepted.is_empty() {
            return;
        }
        let mut ids = std::mem::take(&mut self.accepted);
        ids.sort_unstable();
        ids.dedup();
        let mut i = 0;
        while i < ids.len() {
            let first = ids[i];
            let mut last = first;
            while i + 1 < ids.len() && ids[i + 1] == last.wrapping_add(1) {
                i += 1;
                last = ids[i];
            }
            i += 1;
            let d = Disposition {
                role: ROLE_RECEIVER,
                first,
                last: (last != first).then_some(last),
                settled: true,
                state: Some(DeliveryState::Accepted),
                batchable: false,
            };
            self.send_ordered(out, &Performative::Disposition(d));
        }
        self.pump(out);
    }

    async fn deliver(&mut self, link: Arc<LinkShared>, d: Delivery, out: &mut Out) {
        let ok = self.link_matches(&link) && !self.ending;
        let Some(l) = self.outgoing.get(&link.handle).filter(|_| ok) else {
            // The link is gone: hand the message back untouched.
            if !link.closed.load(Ordering::Acquire) || link.session == self.id {
                link.queue.release(self.id, vec![d.seq]).await;
            }
            return;
        };
        let (send_settled, max_size) = (l.send_settled, l.max_message_size);
        let filter = match &l.kind {
            OutKind::Queue { filter, .. } => filter.clone(),
            _ => None,
        };
        let is_stream = d.queue.kind == QueueType::Stream;
        let info = DeliveryInfo {
            exchange: &d.msg.exchange,
            routing_key: &d.msg.routing_key,
            redelivered: d.redelivered,
            delivery_count: if d.queue.kind == QueueType::Quorum { d.delivery_count } else { 0 },
            stream_offset: is_stream.then_some(d.seq),
        };
        let mut payload = BytesMut::new();
        message::from_core(&d.msg.props, &d.msg.body, &info, &mut payload);
        let payload = payload.freeze();
        if let Some(f) = &filter
            && !Amqp1Message::decode(payload.clone()).is_ok_and(|m| f.matches(&m))
        {
            // Filtered out: the client never sees it, so the credit is still theirs.
            {
                let mut c = link.credit.lock();
                c.credit += 1;
                c.delivery_count = c.delivery_count.wrapping_sub(1);
            }
            if !send_settled && !is_stream {
                link.queue.release(self.id, vec![d.seq]).await;
            }
            link.queue.wake();
            return;
        }
        if let Some(max) = max_size
            && payload.len() as u64 > max
        {
            if let Some(l) = self.outgoing.remove(&link.handle) {
                self.close_outgoing(link.handle, l).await;
            }
            if !send_settled {
                link.queue.release(self.id, vec![d.seq]).await;
            }
            let e = AmqpErr::new("amqp:link:message-size-exceeded", format!("message size ({} bytes) > maximum message size ({max} bytes)", payload.len()));
            self.detach_link(out, link.handle, e);
            return;
        }
        let id = self.next_delivery_id;
        self.next_delivery_id = id.wrapping_add(1);
        if !send_settled {
            self.unsettled.insert(id, Unsettled { queue: d.queue.clone(), seq: d.seq, handle: link.handle });
            self.info.unacked.fetch_add(1, Ordering::Relaxed);
        }
        self.info.delivered.fetch_add(1, Ordering::Relaxed);
        if d.redelivered {
            self.info.redelivered.fetch_add(1, Ordering::Relaxed);
        }
        let vs = &self.ctx.vhost.stats;
        vs.delivered.fetch_add(1, Ordering::Relaxed);
        vs.delivered_bytes.fetch_add(d.msg.body.len() as u64, Ordering::Relaxed);
        self.queue_transfer(link.handle, id, send_settled, payload, out.max_frame);
        self.pump(out);
    }

    /// Session over: cancel consumers and return everything it held.
    pub async fn cleanup(self) {
        let mut queues: Vec<QueueRef> = Vec::new();
        let mut temps = Vec::new();
        for (_, l) in self.outgoing {
            if let OutKind::Queue { shared, tag, temp_queue, .. } = l.kind {
                shared.closed.store(true, Ordering::Release);
                shared.queue.cancel(self.id, tag).await;
                if !queues.iter().any(|q| Arc::ptr_eq(q, &shared.queue)) {
                    queues.push(shared.queue.clone());
                }
                temps.extend(temp_queue);
            }
        }
        for u in self.unsettled.into_values() {
            if !queues.iter().any(|q| Arc::ptr_eq(q, &u.queue)) {
                queues.push(u.queue);
            }
        }
        for q in queues {
            q.channel_closed(self.id).await;
        }
        for q in temps {
            let _ = self.ctx.vhost.delete_queue(&q, false, false, Some(self.ctx.info.id)).await;
        }
        self.ctx.broker.channel_closed(self.id);
        self.ctx.info.channels.fetch_sub(1, Ordering::Relaxed);
    }
}

fn is_filter(d: &Value, name: &str, code: u64) -> bool {
    match d {
        Value::Symbol(s) => s == name,
        Value::Ulong(c) => *c == code,
        _ => false,
    }
}

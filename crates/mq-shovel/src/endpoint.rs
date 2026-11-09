//! Endpoints: one AMQP session (connection + channel) on the local broker, driven directly
//! through `mq-core`, or on a remote broker over AMQP 0-9-1 (lapin, `amqp://` or `amqps://`).

use crate::convert::{self, short};
use crate::uri::Target;
use anyhow::{Context, anyhow, bail};
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use lapin::options::*;
use lapin::{Connection, ConnectionProperties};
use mq_core::VHost;
use mq_core::broker::{Broker, ConnectionInfo};
use mq_core::error::NOT_FOUND;
use mq_core::exchange::Dest;
use mq_core::message::{Message, MessageRef};
use mq_core::queue::{ChannelId, ConfirmTicket, ConsumerSink, ConsumerSpec, Delivery, Outcome, QueueRef};
use mq_core::value::FieldTable;
use mq_core::vhost::DeclareQueue;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A message taken from a source.
pub struct InMsg {
    /// Acknowledge or reject it with this tag.
    pub tag: u64,
    pub msg: MessageRef,
    pub redelivered: bool,
}

/// Resolves to whether the destination accepted the message (publisher confirm).
pub type Confirm = BoxFuture<'static, anyhow::Result<bool>>;

#[derive(Clone)]
struct ConsumeSpec {
    queue: String,
    prefetch: u32,
    no_ack: bool,
    args: FieldTable,
}

pub enum Session {
    Local(Local),
    Remote(Box<Remote>),
}

/// A direct connection to the in-process broker.
pub struct Local {
    broker: Arc<Broker>,
    vh: Arc<VHost>,
    info: Arc<ConnectionInfo>,
    channel: ChannelId,
    spec: Option<ConsumeSpec>,
    consumer: Option<LocalConsumer>,
}

struct LocalConsumer {
    queue: QueueRef,
    tag: Arc<str>,
    sink: Arc<LocalSink>,
    rx: mpsc::UnboundedReceiver<Event>,
}

enum Event {
    Delivery(Delivery),
    Cancelled,
}

struct LocalSink {
    tx: mpsc::UnboundedSender<Event>,
    inflight: AtomicU32,
    prefetch: u32,
    active: AtomicBool,
}

impl ConsumerSink for LocalSink {
    fn has_credit(&self) -> bool {
        self.active.load(Ordering::Acquire) && (self.prefetch == 0 || self.inflight.load(Ordering::Acquire) < self.prefetch)
    }

    fn deliver(&self, d: Delivery) {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        let _ = self.tx.send(Event::Delivery(d));
    }

    fn cancelled(&self, _tag: &str) {
        let _ = self.tx.send(Event::Cancelled);
    }
}

/// A connection to a remote broker.
pub struct Remote {
    conn: Connection,
    ch: lapin::Channel,
    name: String,
    confirms: bool,
    spec: Option<ConsumeSpec>,
    consumer: Option<lapin::Consumer>,
    paused: bool,
}

impl Session {
    /// Open a session; `label` names the connection (management UI).
    pub async fn connect(target: &Target, broker: &Arc<Broker>, label: &str) -> anyhow::Result<Session> {
        match target {
            Target::Local { vhost } => Ok(Session::Local(Local::open(broker, vhost, label).await?)),
            Target::Remote { uri, .. } => {
                let props = ConnectionProperties::default().with_connection_name(label.into());
                let conn = tokio::time::timeout(CONNECT_TIMEOUT, Connection::connect(uri, props))
                    .await
                    .map_err(|_| anyhow!("timed out connecting to {}", crate::uri::redact(uri)))?
                    .with_context(|| format!("connecting to {}", crate::uri::redact(uri)))?;
                let ch = conn.create_channel().await?;
                Ok(Session::Remote(Box::new(Remote {
                    conn,
                    ch,
                    name: label.to_string(),
                    confirms: false,
                    spec: None,
                    consumer: None,
                    paused: false,
                })))
            }
        }
    }

    /// Connection name, as shown in status output.
    pub fn connection_name(&self) -> String {
        match self {
            Session::Local(l) => l.info.name.clone(),
            Session::Remote(r) => r.name.clone(),
        }
    }

    /// Declare a durable queue unless it exists; returns its ready message count.
    pub async fn ensure_queue(&mut self, name: &str, args: &FieldTable) -> anyhow::Result<u64> {
        match self {
            Session::Local(l) => match l.vh.queue(name) {
                Some(q) => Ok(q.shared.ready.load(Ordering::Relaxed)),
                None => {
                    let ok = l.vh.declare_queue(declare(name, true, None, false, args)).await?;
                    Ok(ok.messages as u64)
                }
            },
            Session::Remote(r) => {
                if let Some(n) = r.passive_queue(name).await {
                    return Ok(n);
                }
                let opts = QueueDeclareOptions { durable: true, ..Default::default() };
                let q = r.ch.queue_declare(short(name), opts, convert::table_to_lapin(args)).await?;
                Ok(q.message_count() as u64)
            }
        }
    }

    /// Ready messages in a queue, if it exists.
    pub async fn queue_len(&mut self, name: &str) -> Option<u64> {
        match self {
            Session::Local(l) => l.vh.queue(name).map(|q| q.shared.ready.load(Ordering::Relaxed)),
            Session::Remote(r) => r.passive_queue(name).await,
        }
    }

    /// Declare a queue (an empty name asks for a server-named one); returns its name.
    pub async fn declare_queue(&mut self, name: &str, durable: bool, exclusive: bool, auto_delete: bool, args: &FieldTable) -> anyhow::Result<String> {
        match self {
            Session::Local(l) => {
                let owner = exclusive.then_some(l.info.id);
                Ok(l.vh.declare_queue(declare(name, durable, owner, auto_delete, args)).await?.name)
            }
            Session::Remote(r) => {
                let opts = QueueDeclareOptions { durable, exclusive, auto_delete, ..Default::default() };
                let q = r.ch.queue_declare(short(name), opts, convert::table_to_lapin(args)).await?;
                Ok(q.name().as_str().to_string())
            }
        }
    }

    /// Declare an exchange unless it exists.
    pub async fn ensure_exchange(&mut self, name: &str, kind: &str, durable: bool, auto_delete: bool, internal: bool, args: &FieldTable) -> anyhow::Result<()> {
        let exists = match self {
            Session::Local(l) => l.vh.exchange(name).is_some(),
            Session::Remote(r) => {
                let tmp = r.conn.create_channel().await?;
                let opts = ExchangeDeclareOptions { passive: true, ..Default::default() };
                let ok = tmp.exchange_declare(short(name), lapin::ExchangeKind::Custom(kind.into()), opts, Default::default()).await.is_ok();
                if ok {
                    let _ = tmp.close(200, "OK".into()).await;
                }
                ok
            }
        };
        if exists { Ok(()) } else { self.declare_exchange(name, kind, durable, auto_delete, internal, args).await }
    }

    pub async fn declare_exchange(&mut self, name: &str, kind: &str, durable: bool, auto_delete: bool, internal: bool, args: &FieldTable) -> anyhow::Result<()> {
        match self {
            Session::Local(l) => {
                l.vh.declare_exchange(name, kind, durable, auto_delete, internal, args.clone(), false).await?;
            }
            Session::Remote(r) => {
                let opts = ExchangeDeclareOptions { passive: false, durable, auto_delete, internal, nowait: false };
                r.ch.exchange_declare(short(name), lapin::ExchangeKind::Custom(kind.into()), opts, convert::table_to_lapin(args)).await?;
            }
        }
        Ok(())
    }

    /// Delete an exchange; a missing one is fine.
    pub async fn delete_exchange(&mut self, name: &str) -> anyhow::Result<()> {
        match self {
            Session::Local(l) => match l.vh.delete_exchange(name, false).await {
                Err(e) if e.code != NOT_FOUND => Err(e.into()),
                _ => Ok(()),
            },
            Session::Remote(r) => Ok(r.ch.exchange_delete(short(name), ExchangeDeleteOptions::default()).await?),
        }
    }

    pub async fn bind_queue(&mut self, queue: &str, exchange: &str, key: &str, args: &FieldTable) -> anyhow::Result<()> {
        match self {
            Session::Local(l) => Ok(l.vh.bind(exchange, Dest::queue(queue), key, args.clone(), Some(l.info.id)).await?),
            Session::Remote(r) => {
                Ok(r.ch.queue_bind(short(queue), short(exchange), short(key), QueueBindOptions::default(), convert::table_to_lapin(args)).await?)
            }
        }
    }

    /// A handle for topology changes alongside a running consumer (its own channel).
    pub async fn control(&self) -> anyhow::Result<Control> {
        match self {
            Session::Local(l) => Ok(Control::Local(l.vh.clone())),
            Session::Remote(r) => Ok(Control::Remote(r.conn.create_channel().await?)),
        }
    }

    /// Turn on publisher confirms (local publishes are always confirmed).
    pub async fn confirm_select(&mut self) -> anyhow::Result<()> {
        if let Session::Remote(r) = self {
            r.ch.confirm_select(ConfirmSelectOptions::default()).await?;
            r.confirms = true;
        }
        Ok(())
    }

    /// Start consuming (or only remember what to consume when `start` is false; see
    /// [`Session::resume`]).
    pub async fn consume(&mut self, queue: &str, prefetch: u32, no_ack: bool, args: FieldTable, start: bool) -> anyhow::Result<()> {
        let spec = ConsumeSpec { queue: queue.to_string(), prefetch, no_ack, args };
        match self {
            Session::Local(l) => {
                l.spec = Some(spec);
                if start {
                    l.subscribe().await?;
                }
            }
            Session::Remote(r) => {
                r.ch.basic_qos(prefetch.min(u16::MAX as u32) as u16, BasicQosOptions::default()).await?;
                r.spec = Some(spec);
                r.paused = true;
                if start {
                    r.resume().await?;
                }
            }
        }
        Ok(())
    }

    /// Stop receiving new messages; unacknowledged ones stay ours.
    pub async fn pause(&mut self) -> anyhow::Result<()> {
        match self {
            Session::Local(l) => {
                if let Some(c) = &l.consumer
                    && c.sink.active.swap(false, Ordering::AcqRel)
                {
                    c.queue.cancel(l.channel, c.tag.clone()).await;
                }
            }
            Session::Remote(r) => {
                if !r.paused
                    && let Some(c) = &r.consumer
                {
                    r.ch.basic_cancel(c.tag(), BasicCancelOptions::default()).await?;
                    r.paused = true;
                }
            }
        }
        Ok(())
    }

    pub async fn resume(&mut self) -> anyhow::Result<()> {
        match self {
            Session::Local(l) => {
                if l.consumer.as_ref().is_none_or(|c| !c.sink.active.load(Ordering::Acquire)) {
                    l.subscribe().await?;
                }
                Ok(())
            }
            Session::Remote(r) => r.resume().await,
        }
    }

    /// The next message. Pending forever while nothing is consumed.
    pub async fn next(&mut self) -> anyhow::Result<InMsg> {
        match self {
            Session::Local(l) => l.next().await,
            Session::Remote(r) => r.next().await,
        }
    }

    pub async fn ack(&mut self, tag: u64) -> anyhow::Result<()> {
        self.settle(tag, Outcome::Ack).await
    }

    pub async fn nack(&mut self, tag: u64, requeue: bool) -> anyhow::Result<()> {
        self.settle(tag, if requeue { Outcome::Requeue } else { Outcome::Reject }).await
    }

    async fn settle(&mut self, tag: u64, outcome: Outcome) -> anyhow::Result<()> {
        match self {
            Session::Local(l) => {
                if let Some(c) = &l.consumer {
                    c.queue.settle(l.channel, vec![tag], outcome).await;
                    c.sink.inflight.fetch_sub(1, Ordering::AcqRel);
                    c.queue.wake();
                }
                Ok(())
            }
            Session::Remote(r) => {
                match outcome {
                    Outcome::Ack => r.ch.basic_ack(tag, BasicAckOptions::default()).await?,
                    o => r.ch.basic_nack(tag, BasicNackOptions { multiple: false, requeue: o == Outcome::Requeue }).await?,
                }
                Ok(())
            }
        }
    }

    /// Publish `msg` to its exchange with its routing key, or straight into `queue` (keeping
    /// the message's exchange and routing key, like a local delivery).
    pub async fn publish(&mut self, msg: Message, queue: Option<&str>) -> anyhow::Result<Confirm> {
        match self {
            Session::Local(l) => {
                let (tx, rx) = oneshot::channel();
                let ticket = ConfirmTicket::new(move |ok| {
                    let _ = tx.send(ok);
                });
                match queue {
                    Some(q) => l.vh.publish_to_queue(q, Arc::new(msg), Some(ticket)).await?,
                    None => {
                        let exchange = msg.exchange.clone();
                        let out = l.vh.publish(&exchange, msg, Some(ticket)).await?;
                        // Unroutable messages are confirmed, like RabbitMQ.
                        if let Some(t) = out.confirm {
                            t.settle(true);
                        }
                    }
                }
                Ok(Box::pin(async move { Ok(rx.await.unwrap_or(false)) }))
            }
            Session::Remote(r) => {
                let (exchange, key) = match queue {
                    Some(q) => ("", q),
                    None => (msg.exchange.as_str(), msg.routing_key.as_str()),
                };
                let props = convert::props_to_lapin(&msg.props);
                let pc = r.ch.basic_publish(short(exchange), short(key), BasicPublishOptions::default(), &msg.body, props).await?;
                if r.confirms {
                    Ok(Box::pin(async move { Ok(pc.await?.is_ack()) }))
                } else {
                    Ok(Box::pin(async { Ok(true) }))
                }
            }
        }
    }

    /// Resolves (with a reason) once the session can no longer be used: force-closed through
    /// the management API, or the remote connection dropped.
    pub async fn closed(&self) -> String {
        match self {
            Session::Local(l) => {
                l.info.close.notified().await;
                l.info.close_reason.lock().clone().unwrap_or_else(|| "connection closed".into())
            }
            Session::Remote(r) => loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if !r.conn.status().connected() {
                    return "connection to the remote broker was lost".into();
                }
            },
        }
    }
}

fn declare(name: &str, durable: bool, exclusive_owner: Option<u64>, auto_delete: bool, args: &FieldTable) -> DeclareQueue {
    DeclareQueue { name: name.to_string(), durable, exclusive_owner, auto_delete, args: args.clone(), passive: false }
}

impl Local {
    async fn open(broker: &Arc<Broker>, vhost: &str, label: &str) -> anyhow::Result<Local> {
        let vh = broker.open_vhost(vhost).await.ok_or_else(|| anyhow!("vhost '{vhost}' does not exist"))?;
        let id = broker.next_id();
        let addr = SocketAddr::from(([0, 0, 0, 0], 0));
        let info = Arc::new(ConnectionInfo {
            id,
            name: format!("{label} (direct {id})"),
            protocol: "Direct 0-9-1".into(),
            vhost: vhost.to_string(),
            user: "none".into(),
            peer: addr,
            local: addr,
            client_properties: serde_json::json!({ "product": "roll queues shovel", "connection_name": label }),
            connected_at_ms: mq_core::now_ms(),
            tls: false,
            auth_mechanism: "none".into(),
            frame_max: 0.into(),
            channel_max: 0.into(),
            heartbeat: 0.into(),
            recv_oct: 0.into(),
            send_oct: 0.into(),
            recv_cnt: 0.into(),
            send_cnt: 0.into(),
            channels: 1.into(),
            blocked: AtomicBool::new(false),
            close_reason: Mutex::new(None),
            close: tokio::sync::Notify::new(),
        });
        broker.register_connection(info.clone());
        Ok(Local { broker: broker.clone(), vh, info, channel: broker.next_id(), spec: None, consumer: None })
    }

    async fn subscribe(&mut self) -> anyhow::Result<()> {
        let spec = self.spec.clone().ok_or_else(|| anyhow!("nothing to consume"))?;
        let q = self.vh.queue_for(&spec.queue, Some(self.info.id))?;
        let c = match self.consumer.take() {
            Some(c) if Arc::ptr_eq(&c.queue, &q) => c,
            old => {
                if let Some(o) = old {
                    o.queue.channel_closed(self.channel).await;
                }
                let (tx, rx) = mpsc::unbounded_channel();
                let sink = Arc::new(LocalSink { tx, inflight: AtomicU32::new(0), prefetch: spec.prefetch, active: AtomicBool::new(false) });
                let tag: Arc<str> = Arc::from(format!("amq.ctag-direct-{}", self.channel).as_str());
                LocalConsumer { queue: q, tag, sink, rx }
            }
        };
        c.sink.active.store(true, Ordering::Release);
        let consume = ConsumerSpec {
            tag: c.tag.clone(),
            channel: self.channel,
            conn: self.info.id,
            no_ack: spec.no_ack,
            exclusive: false,
            priority: 0,
            args: spec.args.clone(),
            sink: c.sink.clone(),
        };
        let r = c.queue.consume(consume).await;
        let queue = c.queue.clone();
        self.consumer = Some(c);
        r?;
        queue.wake();
        Ok(())
    }

    async fn next(&mut self) -> anyhow::Result<InMsg> {
        let no_ack = self.spec.as_ref().is_some_and(|s| s.no_ack);
        let close = &self.info.close;
        let Some(c) = self.consumer.as_mut() else {
            close.notified().await;
            bail!("{}", self.info.close_reason.lock().clone().unwrap_or_else(|| "connection closed".into()));
        };
        loop {
            let ev = tokio::select! {
                _ = close.notified() => bail!("{}", self.info.close_reason.lock().clone().unwrap_or_else(|| "connection closed".into())),
                ev = c.rx.recv() => ev,
            };
            match ev {
                Some(Event::Delivery(d)) => {
                    if no_ack {
                        c.sink.inflight.fetch_sub(1, Ordering::AcqRel);
                        c.queue.wake();
                    }
                    return Ok(InMsg { tag: d.seq, msg: d.msg, redelivered: d.redelivered });
                }
                Some(Event::Cancelled) if !c.sink.active.load(Ordering::Acquire) => continue,
                Some(Event::Cancelled) => bail!("consumer cancelled: queue '{}' was deleted", c.queue.meta.name),
                None => bail!("consumer channel closed"),
            }
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (broker, id, channel) = (self.broker.clone(), self.info.id, self.channel);
        let consumer = self.consumer.take().map(|c| {
            c.sink.active.store(false, Ordering::Release);
            (c.queue, c.tag)
        });
        rt.spawn(async move {
            if let Some((q, tag)) = consumer {
                q.cancel(channel, tag).await;
                q.channel_closed(channel).await;
            }
            broker.connection_closed(id).await;
        });
    }
}

impl Remote {
    /// Message count of an existing queue, checked on a throwaway channel (a failed passive
    /// declare closes the channel).
    async fn passive_queue(&self, name: &str) -> Option<u64> {
        let tmp = self.conn.create_channel().await.ok()?;
        let opts = QueueDeclareOptions { passive: true, ..Default::default() };
        let q = tmp.queue_declare(short(name), opts, Default::default()).await.ok()?;
        let _ = tmp.close(200, "OK".into()).await;
        Some(q.message_count() as u64)
    }

    async fn resume(&mut self) -> anyhow::Result<()> {
        // A cancelled consumer may still be draining; resume once it has finished.
        if !self.paused || self.consumer.is_some() {
            return Ok(());
        }
        let spec = self.spec.clone().ok_or_else(|| anyhow!("nothing to consume"))?;
        let opts = BasicConsumeOptions { no_ack: spec.no_ack, ..Default::default() };
        let c = self.ch.basic_consume(short(&spec.queue), "".into(), opts, convert::table_to_lapin(&spec.args)).await?;
        self.consumer = Some(c);
        self.paused = false;
        Ok(())
    }

    async fn next(&mut self) -> anyhow::Result<InMsg> {
        loop {
            let Some(c) = self.consumer.as_mut() else {
                return std::future::pending().await;
            };
            match c.next().await {
                Some(Ok(d)) => {
                    let props = convert::props_from_lapin(&d.properties);
                    let msg = Message::new(d.exchange.as_str(), d.routing_key.as_str(), props, Bytes::from(d.data));
                    return Ok(InMsg { tag: d.delivery_tag, msg: Arc::new(msg), redelivered: d.redelivered });
                }
                Some(Err(e)) => return Err(e.into()),
                None if self.paused => self.consumer = None,
                None => bail!("consumer cancelled by the remote broker"),
            }
        }
    }
}

/// Topology changes on their own channel (federation keeps upstream bindings in sync while
/// the link consumes).
#[derive(Clone)]
pub enum Control {
    Local(Arc<VHost>),
    Remote(lapin::Channel),
}

impl Control {
    pub async fn bind_exchange(&self, dest: &str, source: &str, key: &str, args: &FieldTable) -> anyhow::Result<()> {
        match self {
            Control::Local(vh) => Ok(vh.bind(source, Dest::exchange(dest), key, args.clone(), None).await?),
            Control::Remote(ch) => {
                Ok(ch.exchange_bind(short(dest), short(source), short(key), ExchangeBindOptions::default(), convert::table_to_lapin(args)).await?)
            }
        }
    }

    pub async fn unbind_exchange(&self, dest: &str, source: &str, key: &str, args: &FieldTable) -> anyhow::Result<()> {
        match self {
            Control::Local(vh) => Ok(vh.unbind(source, Dest::exchange(dest), key, args.clone(), None).await?),
            Control::Remote(ch) => {
                Ok(ch.exchange_unbind(short(dest), short(source), short(key), ExchangeUnbindOptions::default(), convert::table_to_lapin(args)).await?)
            }
        }
    }
}

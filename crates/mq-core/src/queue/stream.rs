//! Stream queues: an append-only log read by offset. Consumers keep their own cursor, messages
//! are never removed by consumption, and retention trims whole segments by size or age (cold
//! segments live in the S3 tier when the backend tiers).
//!
//! An entry is either a message routed through an exchange (stored in the internal AMQP 0-9-1
//! format) or a *native* entry written by the stream protocol: the client's AMQP 1.0 bytes, kept
//! verbatim for stream consumers, plus an AMQP 0-9-1 view for every other protocol. Stream
//! protocol subscribers get whole chunks of consecutive entries ([`ChunkSink`]), and publishing
//! ids are tracked per publisher reference so retried publishes are deduplicated.

use super::*;
use crate::error::{AmqpError, NOT_IMPLEMENTED};
use crate::log::Log;
use crate::message::{Message, Properties};
use crate::value::{DecodeError, DecodeResult, FieldValue, get_longstr, get_shortstr, get_u8, get_u16, get_u32, get_u64, put_longstr, put_shortstr, take};
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::{HashMap, VecDeque};
use std::sync::Weak;
use std::time::Duration;
use tokio::time::Instant;
use tracing::warn;

const MAILBOX: usize = 4096;
/// Recent messages kept in memory so live consumers never touch the log.
const CACHE_BYTES: u64 = 64 << 20;
const READ_BATCH: usize = 512;
/// Entries read per pass when building chunks.
const CHUNK_READ: usize = 4096;
/// Passes per chunk subscriber before giving the others a turn.
const CHUNK_ROUNDS: usize = 16;
/// Sparse index of (offset, timestamp) every this many messages, for timestamp seeks.
const TS_EVERY: u64 = 256;
/// Most entries in one chunk (the osiris chunk header counts them in 16 bits).
pub const MAX_CHUNK_ENTRIES: usize = u16::MAX as usize;
/// Opens a native entry. A stored message starts with its exchange name as a short string, and
/// no 255-byte exchange name starts with NUL.
const NATIVE_MAGIC: [u8; 3] = [0xFF, 0x00, 0x01];

/// One stored stream entry.
#[derive(Clone, Debug)]
pub enum Entry {
    /// Routed through an exchange (AMQP 0-9-1 and 1.0, MQTT, STOMP, HTTP, …).
    Msg(MessageRef),
    /// Written by the stream protocol.
    Native(Native),
}

/// A stream-protocol entry: the AMQP 1.0 message as the client sent it, and an AMQP 0-9-1 view.
#[derive(Clone, Debug)]
pub struct Native {
    /// The stored bytes.
    pub raw: Bytes,
    /// The AMQP 1.0 encoded message (a slice of `raw`).
    pub amqp: Bytes,
    pub received_ms: u64,
    /// Filter value (publish v2).
    pub filter: Option<Bytes>,
    /// Publisher reference and publishing id (deduplication).
    pub writer: Option<(Bytes, u64)>,
    /// Where the AMQP 0-9-1 view starts in `raw`.
    view: usize,
}

/// The AMQP 0-9-1 view of a native entry, as [`encode_native`] stores it.
pub struct NativeView<'a> {
    pub exchange: &'a str,
    pub routing_key: &'a str,
    /// Encoded basic properties (see [`Properties::encode`]).
    pub props_raw: &'a [u8],
    pub body: NativeBody<'a>,
}

/// The AMQP 0-9-1 body of a native entry.
pub enum NativeBody<'a> {
    /// `len` bytes at `offset` of the AMQP 1.0 message (a single data section's content).
    Slice { offset: usize, len: usize },
    /// Anything else (concatenated data sections, encoded value sections).
    Inline(&'a [u8]),
}

/// Append a native entry to `out`.
pub fn encode_native(out: &mut BytesMut, received_ms: u64, filter: Option<&[u8]>, writer: Option<(&[u8], u64)>, view: &NativeView<'_>, amqp: &[u8]) {
    out.reserve(32 + amqp.len() + view.props_raw.len() + view.exchange.len() + view.routing_key.len());
    out.put_slice(&NATIVE_MAGIC);
    out.put_u64(received_ms);
    match filter {
        Some(f) => {
            let n = f.len().min(u16::MAX as usize - 1);
            out.put_u16(n as u16);
            out.put_slice(&f[..n]);
        }
        None => out.put_u16(u16::MAX),
    }
    match writer {
        Some((w, id)) if !w.is_empty() => {
            let n = w.len().min(255);
            out.put_u8(n as u8);
            out.put_slice(&w[..n]);
            out.put_u64(id);
        }
        _ => out.put_u8(0),
    }
    put_shortstr(out, view.exchange);
    put_shortstr(out, view.routing_key);
    put_longstr(out, view.props_raw);
    match view.body {
        NativeBody::Slice { offset, len } => {
            out.put_u8(0);
            out.put_u32(offset as u32);
            out.put_u32(len as u32);
        }
        NativeBody::Inline(b) => {
            out.put_u8(1);
            put_longstr(out, b);
        }
    }
    put_longstr(out, amqp);
}

impl Native {
    pub fn decode(raw: Bytes) -> DecodeResult<Native> {
        let mut b = raw.clone();
        if take(&mut b, 3)?[..] != NATIVE_MAGIC {
            return Err(DecodeError("not a native stream entry"));
        }
        let received_ms = get_u64(&mut b)?;
        let filter = match get_u16(&mut b)? {
            u16::MAX => None,
            n => Some(take(&mut b, n as usize)?),
        };
        let writer = match get_u8(&mut b)? {
            0 => None,
            n => {
                let w = take(&mut b, n as usize)?;
                Some((w, get_u64(&mut b)?))
            }
        };
        let view = raw.len() - b.len();
        for _ in 0..2 {
            let n = get_u8(&mut b)? as usize;
            take(&mut b, n)?;
        }
        get_longstr(&mut b)?;
        match get_u8(&mut b)? {
            0 => {
                take(&mut b, 8)?;
            }
            _ => {
                get_longstr(&mut b)?;
            }
        }
        let amqp = get_longstr(&mut b)?;
        Ok(Native { raw, amqp, received_ms, filter, writer, view })
    }

    /// The AMQP 0-9-1 message other protocols consume.
    pub fn message(&self) -> DecodeResult<Message> {
        let mut b = self.raw.slice(self.view..);
        let exchange = get_shortstr(&mut b)?;
        let routing_key = get_shortstr(&mut b)?;
        let props_raw = get_longstr(&mut b)?;
        let body = match get_u8(&mut b)? {
            0 => {
                let (offset, len) = (get_u32(&mut b)? as usize, get_u32(&mut b)? as usize);
                if offset.checked_add(len).is_none_or(|end| end > self.amqp.len()) {
                    return Err(DecodeError("native body out of range"));
                }
                self.amqp.slice(offset..offset + len)
            }
            _ => get_longstr(&mut b)?,
        };
        let props = Properties::decode(&mut props_raw.clone())?;
        Ok(Message { exchange, routing_key, props, props_raw, body, received_ms: self.received_ms })
    }
}

impl Entry {
    pub fn decode(raw: Bytes) -> DecodeResult<Entry> {
        if raw.starts_with(&NATIVE_MAGIC) {
            Native::decode(raw).map(Entry::Native)
        } else {
            Message::decode(&mut raw.clone()).map(|m| Entry::Msg(Arc::new(m)))
        }
    }

    pub fn received_ms(&self) -> u64 {
        match self {
            Entry::Msg(m) => m.received_ms,
            Entry::Native(n) => n.received_ms,
        }
    }

    /// The stream filter value (`x-stream-filter-value` header for routed messages).
    pub fn filter_value(&self) -> Option<&[u8]> {
        match self {
            Entry::Msg(m) => m.props.header("x-stream-filter-value").and_then(|v| v.as_str()).map(str::as_bytes),
            Entry::Native(n) => n.filter.as_deref(),
        }
    }

    /// The message as AMQP-family consumers see it.
    pub fn to_message(&self) -> Option<MessageRef> {
        match self {
            Entry::Msg(m) => Some(m.clone()),
            Entry::Native(n) => n.message().ok().map(Arc::new),
        }
    }

    /// Bytes as stored.
    pub fn stored_len(&self) -> usize {
        match self {
            Entry::Msg(m) => m.encoded_len(),
            Entry::Native(n) => n.raw.len(),
        }
    }

    /// Upper estimate of the entry's size in a delivered chunk.
    pub fn wire_len(&self) -> usize {
        match self {
            Entry::Msg(m) => 4 + 128 + m.body.len() + 2 * m.props_raw.len() + m.exchange.len() + m.routing_key.len(),
            Entry::Native(n) => 4 + n.amqp.len(),
        }
    }

    fn stored(&self) -> Bytes {
        match self {
            Entry::Msg(m) => encode(m),
            Entry::Native(n) => n.raw.clone(),
        }
    }
}

/// A run of consecutive entries for a stream-protocol subscriber.
pub struct Chunk {
    /// Offset of the first entry (the chunk id).
    pub first: u64,
    pub entries: Vec<Entry>,
    /// Last durable offset (deliver v2's committed chunk id).
    pub committed: u64,
}

/// How the stream protocol receives chunks. Implementations must never block.
pub trait ChunkSink: Send + Sync {
    /// Whether another chunk may go out now (credit left and the socket isn't backed up).
    fn has_credit(&self) -> bool;
    /// Byte budget for one chunk's entries.
    fn max_bytes(&self) -> usize;
    /// Hand over a chunk (consumes one credit).
    fn deliver(&self, chunk: Chunk);
    /// The stream went away (deleted, moved or stopped).
    fn cancelled(&self);
}

/// A stream-protocol subscription.
pub struct ChunkSubscription {
    /// Unique within the broker (from `broker.next_id()`); unsubscribing names it.
    pub id: ChannelId,
    pub conn: ConnId,
    /// Shown as the consumer tag in the management API.
    pub tag: Arc<str>,
    pub offset: StreamOffset,
    /// Filter values; entries without one pass only with `match_unfiltered`.
    pub filters: Option<Vec<Bytes>>,
    pub match_unfiltered: bool,
    pub properties: FieldTable,
    pub sink: Arc<dyn ChunkSink>,
}

/// A stream-protocol publish: entries encoded with [`encode_native`], confirmed once durable.
/// Entries whose publishing id isn't above the last one stored for the publisher reference are
/// dropped (and still confirmed), like RabbitMQ's deduplication.
pub struct NativePublish {
    pub writer: Option<Arc<str>>,
    /// (publishing id, encoded entry)
    pub entries: Vec<(u64, Bytes)>,
    pub confirm: ConfirmTicket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamStats {
    pub first: u64,
    pub next: u64,
}

/// Commands only stream queues understand (sent with the `stream_*` methods of [`QueueHandle`]).
pub enum StreamCmd {
    Publish(NativePublish),
    Subscribe { sub: ChunkSubscription, reply: oneshot::Sender<u64> },
    Unsubscribe { id: ChannelId },
    Resolve { offset: StreamOffset, reply: oneshot::Sender<u64> },
    /// Last publishing id stored for a publisher reference, raised to `seed` first.
    WriterSequence { writer: String, seed: u64, reply: oneshot::Sender<u64> },
    Stats { reply: oneshot::Sender<StreamStats> },
}

impl QueueHandle {
    async fn stream_send(&self, cmd: StreamCmd) -> Result<()> {
        let tx = self
            .stream_tx
            .as_ref()
            .ok_or_else(|| AmqpError::precondition(format!("queue '{}' in vhost '{}' is not a stream", self.meta.name, self.meta.vhost)))?;
        tx.send(cmd).await.map_err(|_| gone(&self.meta))
    }

    /// Whether this queue accepts stream-protocol commands.
    pub fn is_stream(&self) -> bool {
        self.stream_tx.is_some()
    }

    pub async fn stream_publish(&self, p: NativePublish) -> Result<()> {
        self.stream_send(StreamCmd::Publish(p)).await
    }

    /// Start a chunk subscription; returns the offset delivery starts at.
    pub async fn stream_subscribe(&self, sub: ChunkSubscription) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.stream_send(StreamCmd::Subscribe { sub, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn stream_unsubscribe(&self, id: ChannelId) {
        let _ = self.stream_send(StreamCmd::Unsubscribe { id }).await;
    }

    pub async fn stream_resolve(&self, offset: StreamOffset) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.stream_send(StreamCmd::Resolve { offset, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn stream_writer_sequence(&self, writer: &str, seed: u64) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.stream_send(StreamCmd::WriterSequence { writer: writer.to_string(), seed, reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }

    pub async fn stream_stats(&self) -> Result<StreamStats> {
        let (reply, rx) = oneshot::channel();
        self.stream_send(StreamCmd::Stats { reply }).await?;
        rx.await.map_err(|_| gone(&self.meta))
    }
}

struct StreamConsumer {
    spec: ConsumerSpec,
    next: u64,
    filters: Option<Vec<String>>,
    match_unfiltered: bool,
}

struct ChunkConsumer {
    sub: ChunkSubscription,
    next: u64,
}

impl ChunkConsumer {
    fn passes(&self, e: &Entry) -> bool {
        let Some(f) = &self.sub.filters else { return true };
        match e.filter_value() {
            Some(v) => f.iter().any(|x| &x[..] == v),
            None => self.sub.match_unfiltered,
        }
    }

    fn emit(&self, first: u64, entries: Vec<Entry>, committed: u64) {
        self.sub.sink.deliver(Chunk { first, entries, committed });
    }
}

pub(crate) struct Stream {
    meta: QueueMeta,
    config: QueueConfig,
    shared: Arc<QueueShared>,
    me: Weak<QueueHandle>,
    host: Arc<dyn super::engine::QueueHost>,
    log: Arc<dyn Log>,
    cache: VecDeque<(u64, Entry)>,
    cache_bytes: u64,
    ts_index: VecDeque<(u64, u64)>,
    consumers: Vec<StreamConsumer>,
    chunk_consumers: Vec<ChunkConsumer>,
    pending: Vec<Entry>,
    pending_confirms: Vec<ConfirmTicket>,
    pending_replies: Vec<oneshot::Sender<PublishResult>>,
    /// Last durable publishing id per publisher reference.
    writers: HashMap<String, u64>,
    /// Publishing ids accepted into `pending`, not yet durable.
    pending_writers: HashMap<String, u64>,
    last_retention: Instant,
    /// Appends since retention last ran (max-length may now cut something).
    appended: bool,
    /// Bytes appended since the active segment started (`x-stream-max-segment-size-bytes`).
    since_roll: u64,
    deleted: bool,
    stopped: bool,
}

pub async fn spawn(meta: QueueMeta, config: QueueConfig, log: Arc<dyn Log>, host: Arc<dyn super::engine::QueueHost>) -> anyhow::Result<QueueRef> {
    let (tx, rx) = mpsc::channel(MAILBOX);
    let (stx, srx) = mpsc::channel(MAILBOX);
    let shared = Arc::new(QueueShared::default());
    let mut handle = QueueHandle::new(meta.clone(), QueueType::Stream, shared.clone(), tx);
    handle.stream_tx = Some(stx);
    let handle = Arc::new(handle);
    let mut s = Stream {
        meta,
        config,
        shared,
        me: Arc::downgrade(&handle),
        host,
        log,
        cache: VecDeque::new(),
        cache_bytes: 0,
        ts_index: VecDeque::new(),
        consumers: Vec::new(),
        chunk_consumers: Vec::new(),
        pending: Vec::new(),
        pending_confirms: Vec::new(),
        pending_replies: Vec::new(),
        writers: HashMap::new(),
        pending_writers: HashMap::new(),
        last_retention: Instant::now(),
        appended: false,
        since_roll: 0,
        deleted: false,
        stopped: false,
    };
    s.recover().await?;
    s.publish_stats();
    tokio::spawn(s.run(rx, srx));
    Ok(handle)
}

/// Encode a stream entry: the message itself (it carries its own timestamp).
fn encode(msg: &Message) -> Bytes {
    let mut b = BytesMut::with_capacity(msg.encoded_len());
    msg.encode(&mut b);
    b.freeze()
}

/// Parse `x-stream-offset` from consumer arguments.
pub fn parse_offset(v: Option<&FieldValue>) -> std::result::Result<StreamOffset, AmqpError> {
    let Some(v) = v else { return Ok(StreamOffset::Next) };
    match v {
        FieldValue::Timestamp(t) => Ok(StreamOffset::Timestamp(t * 1000)),
        _ => {
            if let Some(s) = v.as_str() {
                return match s {
                    "first" => Ok(StreamOffset::First),
                    "last" => Ok(StreamOffset::Last),
                    "next" => Ok(StreamOffset::Next),
                    s => {
                        // An interval like "1h": that long ago.
                        let ms = super::config::parse_age(s).ok_or_else(|| AmqpError::precondition(format!("invalid x-stream-offset '{s}'")))?;
                        Ok(StreamOffset::Timestamp(crate::now_ms().saturating_sub(ms)))
                    }
                };
            }
            match v.as_i64() {
                Some(n) if n >= 0 => Ok(StreamOffset::Offset(n as u64)),
                _ => Err(AmqpError::precondition("invalid x-stream-offset")),
            }
        }
    }
}

fn filter_values(v: Option<&FieldValue>) -> Option<Vec<String>> {
    let v = v?;
    match v.as_array() {
        Some(a) => Some(a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
        None => v.as_str().map(|s| vec![s.to_string()]),
    }
}

impl Stream {
    async fn recover(&mut self) -> anyhow::Result<()> {
        let (first, next) = self.log.bounds();
        self.shared.first_offset.store(first, Ordering::Relaxed);
        self.shared.next_offset.store(next, Ordering::Relaxed);
        // Rebuild the timestamp index and publisher sequences, and warm the cache with the tail.
        let mut at = first;
        while at < next {
            let batch = self.log.read(at, 4096, 32 << 20).await?;
            if batch.is_empty() {
                break;
            }
            for (off, data) in batch {
                at = off + 1;
                let entry = Entry::decode(data)?;
                if off.is_multiple_of(TS_EVERY) || off == first {
                    self.ts_index.push_back((off, entry.received_ms()));
                }
                if let Entry::Native(Native { writer: Some((w, id)), .. }) = &entry {
                    let w = String::from_utf8_lossy(w);
                    let last = self.writers.entry(w.into_owned()).or_insert(0);
                    *last = (*last).max(*id);
                }
                if next - off <= 10_000 {
                    self.cache_push(off, entry);
                }
            }
        }
        Ok(())
    }

    fn cache_push(&mut self, off: u64, entry: Entry) {
        self.cache_bytes += entry.stored_len() as u64;
        self.cache.push_back((off, entry));
        while self.cache_bytes > CACHE_BYTES && self.cache.len() > 1 {
            let Some((_, e)) = self.cache.pop_front() else { break };
            self.cache_bytes -= e.stored_len() as u64;
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<QueueCmd>, mut srx: mpsc::Receiver<StreamCmd>) {
        loop {
            let wait = self.retention_wait();
            tokio::select! {
                c = rx.recv() => match c {
                    Some(c) => self.handle(c).await,
                    None => break,
                },
                c = srx.recv() => match c {
                    Some(c) => self.handle_stream(c).await,
                    None => break,
                },
                _ = tokio::time::sleep(wait.unwrap_or(Duration::MAX)), if wait.is_some() => {
                    self.retention().await;
                    continue;
                }
            }
            let mut n = 0;
            while n < 512 && !self.deleted && !self.stopped {
                let mut any = false;
                if let Ok(c) = rx.try_recv() {
                    self.handle(c).await;
                    any = true;
                }
                if !self.deleted
                    && !self.stopped
                    && let Ok(c) = srx.try_recv()
                {
                    self.handle_stream(c).await;
                    any = true;
                }
                if !any {
                    break;
                }
                n += 1;
            }
            self.flush().await;
            if self.deleted || self.stopped {
                break;
            }
            self.deliver().await;
            if self.deliver_chunks().await
                && let Some(me) = self.me.upgrade()
            {
                me.wake();
            }
            if self.last_retention.elapsed() > Duration::from_secs(5) {
                self.retention().await;
            }
            self.publish_stats();
        }
    }

    async fn handle(&mut self, cmd: QueueCmd) {
        match cmd {
            QueueCmd::Publish { msg, confirm, reply } => {
                self.pending.push(Entry::Msg(msg));
                if let Some(c) = confirm {
                    self.pending_confirms.push(c);
                }
                if let Some(r) = reply {
                    self.pending_replies.push(r);
                }
            }
            QueueCmd::Consume { spec, reply } => {
                let r = self.add_consumer(*spec).await;
                let _ = reply.send(r);
            }
            QueueCmd::Cancel { channel, tag, reply } => {
                let before = self.consumers.len();
                self.consumers.retain(|c| !(c.spec.channel == channel && c.spec.tag == tag));
                if let Some(r) = reply {
                    let _ = r.send(before != self.consumers.len());
                }
            }
            QueueCmd::Get { reply, .. } => {
                let _ = reply.send(Err(AmqpError::new(
                    NOT_IMPLEMENTED,
                    format!("basic.get not supported by stream queues queue '{}' in vhost '{}'", self.meta.name, self.meta.vhost),
                )));
            }
            QueueCmd::ChannelClosed { channel } => self.consumers.retain(|c| c.spec.channel != channel),
            // Acks only return credit on streams.
            QueueCmd::Settle { .. } | QueueCmd::Recover { .. } | QueueCmd::Release { .. } | QueueCmd::Modify { .. } => {}
            QueueCmd::Wake => {
                if let Some(me) = self.me.upgrade() {
                    me.woke();
                }
            }
            QueueCmd::Purge { reply } => {
                // RabbitMQ refuses purges on streams; report nothing removed.
                let _ = reply.send(0);
            }
            QueueCmd::Delete { if_unused, reply, .. } => {
                if if_unused && !(self.consumers.is_empty() && self.chunk_consumers.is_empty()) {
                    let _ = reply.send(Err(AmqpError::precondition(format!("queue '{}' in vhost '{}' in use", self.meta.name, self.meta.vhost))));
                } else {
                    let (first, next) = self.log.bounds();
                    self.delete_self();
                    let _ = reply.send(Ok(next - first));
                }
            }
            QueueCmd::Reconfigure { config } => self.config = *config,
            QueueCmd::Lease { reply, .. } => {
                let _ = reply.send(vec![]);
            }
            QueueCmd::SettleLease { reply, .. } | QueueCmd::ExtendLease { reply, .. } => {
                let _ = reply.send(false);
            }
            QueueCmd::Peek { offset, count, reply } => {
                let (first, _) = self.log.bounds();
                let entries = self.read(first + offset as u64, count).await;
                let v = entries
                    .into_iter()
                    .filter_map(|(seq, e)| e.to_message().map(|msg| Peeked { seq, msg, redelivered: false, delivery_count: 0 }))
                    .collect();
                let _ = reply.send(v);
            }
            QueueCmd::Touch => {}
            QueueCmd::Consumers { reply } => {
                let mut v: Vec<ConsumerInfo> = self
                    .consumers
                    .iter()
                    .map(|c| ConsumerInfo {
                        consumer_tag: c.spec.tag.to_string(),
                        channel: c.spec.channel,
                        connection: c.spec.conn,
                        ack_required: !c.spec.no_ack,
                        exclusive: c.spec.exclusive,
                        priority: c.spec.priority,
                        active: true,
                        arguments: c.spec.args.clone(),
                    })
                    .collect();
                v.extend(self.chunk_consumers.iter().map(|c| ConsumerInfo {
                    consumer_tag: c.sub.tag.to_string(),
                    channel: c.sub.id,
                    connection: c.sub.conn,
                    ack_required: false,
                    exclusive: false,
                    priority: 0,
                    active: true,
                    arguments: c.sub.properties.clone(),
                }));
                let _ = reply.send(v);
            }
            QueueCmd::Stop { reply } => {
                self.flush().await;
                for c in self.consumers.drain(..) {
                    c.spec.sink.cancelled(&c.spec.tag);
                }
                for c in self.chunk_consumers.drain(..) {
                    c.sub.sink.cancelled();
                }
                self.stopped = true;
                let _ = reply.send(());
            }
        }
    }

    async fn handle_stream(&mut self, cmd: StreamCmd) {
        match cmd {
            StreamCmd::Publish(p) => {
                // Sub-batch entries share one publishing id: accept them all once the first is.
                let mut accepted = None;
                for (id, raw) in p.entries {
                    let Ok(native) = Native::decode(raw) else { continue };
                    if let Some(w) = &p.writer
                        && accepted != Some(id)
                    {
                        let last = self.pending_writers.get(&**w).or_else(|| self.writers.get(&**w)).copied();
                        if last.is_some_and(|l| id <= l) {
                            continue;
                        }
                        match self.pending_writers.get_mut(&**w) {
                            Some(l) => *l = id,
                            None => {
                                self.pending_writers.insert(w.to_string(), id);
                            }
                        }
                        accepted = Some(id);
                    }
                    self.pending.push(Entry::Native(native));
                }
                self.pending_confirms.push(p.confirm);
            }
            StreamCmd::Subscribe { sub, reply } => {
                self.flush().await;
                let next = self.resolve(&sub.offset).await;
                self.chunk_consumers.push(ChunkConsumer { sub, next });
                let _ = reply.send(next);
            }
            StreamCmd::Unsubscribe { id } => self.chunk_consumers.retain(|c| c.sub.id != id),
            StreamCmd::Resolve { offset, reply } => {
                self.flush().await;
                let _ = reply.send(self.resolve(&offset).await);
            }
            StreamCmd::WriterSequence { writer, seed, reply } => {
                self.flush().await;
                let last = self.writers.entry(writer).or_insert(0);
                *last = (*last).max(seed);
                let _ = reply.send(*last);
            }
            StreamCmd::Stats { reply } => {
                self.flush().await;
                let (first, next) = self.log.bounds();
                let _ = reply.send(StreamStats { first, next });
            }
        }
    }

    async fn resolve(&self, offset: &StreamOffset) -> u64 {
        let (first, next) = self.log.bounds();
        match *offset {
            StreamOffset::First => first,
            StreamOffset::Last => next.saturating_sub(1).max(first),
            StreamOffset::Next => next,
            StreamOffset::Offset(n) => n.clamp(first, next),
            StreamOffset::Timestamp(ts) => self.seek_timestamp(ts).await,
        }
    }

    async fn add_consumer(&mut self, spec: ConsumerSpec) -> Result<()> {
        self.flush().await;
        let offset = parse_offset(spec.args.get("x-stream-offset"))?;
        let start = self.resolve(&offset).await;
        let filters = filter_values(spec.args.get("x-stream-filter"));
        let match_unfiltered = spec.args.get_bool("x-stream-match-unfiltered").unwrap_or(false);
        self.consumers.push(StreamConsumer { spec, next: start, filters, match_unfiltered });
        Ok(())
    }

    async fn seek_timestamp(&self, ts: u64) -> u64 {
        let (first, next) = self.log.bounds();
        // Start from the last indexed point before `ts`, then scan forward.
        let mut at = self.ts_index.iter().rev().find(|(_, t)| *t < ts).map(|(o, _)| *o).unwrap_or(first).max(first);
        while at < next {
            let batch = self.read(at, READ_BATCH).await;
            if batch.is_empty() {
                break;
            }
            for (off, e) in &batch {
                if e.received_ms() >= ts {
                    return *off;
                }
                at = off + 1;
            }
        }
        next
    }

    async fn read(&self, from: u64, max: usize) -> Vec<(u64, Entry)> {
        if let Some((cfirst, _)) = self.cache.front()
            && from >= *cfirst
        {
            let skip = (from - cfirst) as usize;
            return self.cache.iter().skip(skip).take(max).cloned().collect();
        }
        match self.log.read(from, max, 8 << 20).await {
            Ok(v) => v.into_iter().filter_map(|(o, b)| Entry::decode(b).ok().map(|e| (o, e))).collect(),
            Err(e) => {
                warn!(queue = %self.meta.name, error = %e, "stream read failed");
                vec![]
            }
        }
    }

    async fn flush(&mut self) {
        if self.pending.is_empty() {
            // Everything in the batch was a duplicate: confirm it.
            for c in self.pending_confirms.drain(..) {
                c.settle(true);
            }
            for r in self.pending_replies.drain(..) {
                let _ = r.send(PublishResult::Accepted);
            }
            return;
        }
        let entries = std::mem::take(&mut self.pending);
        let count = entries.len() as u64;
        let data: Vec<Bytes> = entries.iter().map(Entry::stored).collect();
        let appended: u64 = data.iter().map(|d| d.len() as u64).sum();
        let ok = match self.log.append(data).await {
            Ok(first) => {
                self.appended = true;
                for (i, e) in entries.into_iter().enumerate() {
                    let off = first + i as u64;
                    if off.is_multiple_of(TS_EVERY) || self.ts_index.is_empty() {
                        self.ts_index.push_back((off, e.received_ms()));
                    }
                    self.cache_push(off, e);
                }
                for (w, id) in self.pending_writers.drain() {
                    self.writers.insert(w, id);
                }
                self.shared.published.fetch_add(count, Ordering::Relaxed);
                self.since_roll += appended;
                true
            }
            Err(e) => {
                warn!(queue = %self.meta.name, error = %e, "stream append failed");
                self.pending_writers.clear();
                false
            }
        };
        for c in self.pending_confirms.drain(..) {
            c.settle(ok);
        }
        for r in self.pending_replies.drain(..) {
            let _ = r.send(if ok { PublishResult::Accepted } else { PublishResult::Rejected });
        }
        // Segments roll at the stream's own size, and retention runs on every roll (RabbitMQ).
        if let Some(max) = self.config.stream_max_segment_size_bytes
            && self.since_roll >= max
        {
            self.since_roll = 0;
            if self.log.roll().await.is_ok() {
                self.retention().await;
            }
        }
        self.shared.last_activity_ms.store(crate::now_ms() as i64, Ordering::Relaxed);
    }

    async fn deliver(&mut self) {
        let Some(me) = self.me.upgrade() else { return };
        let (_, next) = self.log.bounds();
        let mut delivered = 0u64;
        for i in 0..self.consumers.len() {
            let mut budget = 2048;
            while budget > 0 && self.consumers[i].next < next && self.consumers[i].spec.sink.has_credit() {
                let from = self.consumers[i].next;
                let batch = self.read(from, READ_BATCH.min(budget)).await;
                if batch.is_empty() {
                    // Retention moved past the cursor.
                    self.consumers[i].next = self.log.bounds().0.max(from + 1).min(next);
                    break;
                }
                for (off, entry) in batch {
                    let c = &mut self.consumers[i];
                    if off < c.next {
                        continue;
                    }
                    if !c.spec.sink.has_credit() {
                        break;
                    }
                    c.next = off + 1;
                    budget -= 1;
                    if let Some(f) = &c.filters {
                        let pass = match entry.filter_value() {
                            Some(v) => f.iter().any(|x| x.as_bytes() == v),
                            None => c.match_unfiltered,
                        };
                        if !pass {
                            continue;
                        }
                    }
                    let Some(msg) = entry.to_message() else { continue };
                    c.spec.sink.deliver(Delivery {
                        queue: me.clone(),
                        consumer_tag: c.spec.tag.clone(),
                        seq: off,
                        msg,
                        redelivered: false,
                        delivery_count: 0,
                        remaining: 0,
                    });
                    delivered += 1;
                }
            }
        }
        if delivered > 0 {
            self.shared.delivered.fetch_add(delivered, Ordering::Relaxed);
        }
    }

    /// Hand whole chunks to stream-protocol subscribers. Returns true if some subscriber still
    /// has credit and unread entries (the caller re-wakes the actor).
    async fn deliver_chunks(&mut self) -> bool {
        let (_, next) = self.log.bounds();
        let committed = next.saturating_sub(1);
        let mut delivered = 0u64;
        let mut more = false;
        for i in 0..self.chunk_consumers.len() {
            let mut rounds = 0;
            loop {
                let c = &self.chunk_consumers[i];
                if c.next >= next || !c.sub.sink.has_credit() {
                    break;
                }
                if rounds == CHUNK_ROUNDS {
                    more = true;
                    break;
                }
                rounds += 1;
                let from = c.next;
                let batch = self.read(from, CHUNK_READ).await;
                if batch.is_empty() {
                    self.chunk_consumers[i].next = self.log.bounds().0.max(from + 1).min(next);
                    break;
                }
                let c = &mut self.chunk_consumers[i];
                let max_bytes = c.sub.sink.max_bytes().max(1);
                let mut chunk: Vec<Entry> = Vec::new();
                let (mut first, mut bytes) = (0u64, 0usize);
                let mut stalled = false;
                for (off, e) in batch {
                    if off < c.next {
                        continue;
                    }
                    let pass = c.passes(&e);
                    let size = e.wire_len();
                    if !chunk.is_empty() && (!pass || bytes + size > max_bytes || chunk.len() >= MAX_CHUNK_ENTRIES) {
                        delivered += chunk.len() as u64;
                        c.emit(first, std::mem::take(&mut chunk), committed);
                        if !c.sub.sink.has_credit() {
                            stalled = true;
                            break;
                        }
                    }
                    c.next = off + 1;
                    if !pass {
                        continue;
                    }
                    if chunk.is_empty() {
                        first = off;
                        bytes = 0;
                    }
                    bytes += size;
                    chunk.push(e);
                }
                if !chunk.is_empty() {
                    delivered += chunk.len() as u64;
                    c.emit(first, chunk, committed);
                }
                if stalled {
                    break;
                }
            }
        }
        if delivered > 0 {
            self.shared.delivered.fetch_add(delivered, Ordering::Relaxed);
        }
        more
    }

    /// How long until retention could remove something with no new appends: shortly after
    /// appends stop (max-length), or when the oldest chunk passes max-age. `None` when nothing
    /// can change, so an idle stream sets no timer at all.
    fn retention_wait(&self) -> Option<Duration> {
        let (first, next) = self.log.bounds();
        if first == next {
            return None;
        }
        let soon = Duration::from_secs(10);
        if self.appended {
            return Some(soon);
        }
        // Age cuts up to the newest indexed chunk past the horizon, so the next cut comes when
        // the second-oldest one ages out.
        let age = self.config.max_age_ms?;
        let next_cut = self.ts_index.get(1)?.1;
        Some(Duration::from_millis((next_cut + age).saturating_sub(crate::now_ms())).max(soon))
    }

    async fn retention(&mut self) {
        self.last_retention = Instant::now();
        self.appended = false;
        let (first, next) = self.log.bounds();
        if first == next {
            return;
        }
        let mut cut = first;
        if let Some(max) = self.config.max_length_bytes {
            let bytes = self.log.bytes();
            if bytes > max {
                let excess = (bytes - max) as f64 / bytes as f64;
                cut = cut.max(first + ((next - first) as f64 * excess) as u64);
            }
        }
        if let Some(age) = self.config.max_age_ms {
            let horizon = crate::now_ms().saturating_sub(age);
            if let Some((off, _)) = self.ts_index.iter().rev().find(|(_, t)| *t < horizon) {
                cut = cut.max(*off);
            }
        }
        if cut > first {
            if self.log.roll().await.is_ok() {
                self.since_roll = 0;
            }
            if let Ok(new_first) = self.log.truncate_prefix(cut).await {
                while self.ts_index.front().is_some_and(|(o, _)| *o < new_first) {
                    self.ts_index.pop_front();
                }
            }
        }
    }

    fn delete_self(&mut self) {
        self.deleted = true;
        self.shared.deleted.store(true, Ordering::Release);
        for c in self.consumers.drain(..) {
            c.spec.sink.cancelled(&c.spec.tag);
        }
        for c in self.chunk_consumers.drain(..) {
            c.sub.sink.cancelled();
        }
        let log = self.log.clone();
        tokio::spawn(async move {
            let _ = log.destroy().await;
        });
        self.host.queue_removed(&self.meta.name);
    }

    fn publish_stats(&self) {
        let (first, next) = self.log.bounds();
        let s = &self.shared;
        s.first_offset.store(first, Ordering::Relaxed);
        s.next_offset.store(next, Ordering::Relaxed);
        s.ready.store(next - first, Ordering::Relaxed);
        s.ready_bytes.store(self.log.bytes(), Ordering::Relaxed);
        s.log_bytes.store(self.log.bytes(), Ordering::Relaxed);
        s.consumers.store((self.consumers.len() + self.chunk_consumers.len()) as u64, Ordering::Relaxed);
        s.available.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_roundtrip() {
        let amqp = b"\x00\x53\x75\xa0\x05hello";
        let props = Properties { content_type: Some("text/plain".into()), ..Default::default() }.to_bytes();
        let view = NativeView { exchange: "", routing_key: "s", props_raw: &props, body: NativeBody::Slice { offset: 5, len: 5 } };
        let mut b = BytesMut::new();
        encode_native(&mut b, 42, Some(b"eu"), Some((b"w1", 7)), &view, amqp);
        let Entry::Native(n) = Entry::decode(b.freeze()).unwrap() else { panic!("not native") };
        assert_eq!((n.received_ms, n.filter.as_deref(), &n.amqp[..]), (42, Some(&b"eu"[..]), &amqp[..]));
        assert_eq!(n.writer.as_ref().map(|(w, id)| (&w[..], *id)), Some((&b"w1"[..], 7)));
        let m = n.message().unwrap();
        assert_eq!((&m.body[..], m.routing_key.as_str(), m.props.content_type.as_deref()), (&b"hello"[..], "s", Some("text/plain")));
        let m2 = Message::new("x", "k", Properties::default(), Bytes::from_static(b"b"));
        assert!(matches!(Entry::decode(encode(&m2)).unwrap(), Entry::Msg(_)));
    }
}

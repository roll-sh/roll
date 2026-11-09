//! Connection lifecycle: protocol header, SASL, tuning, vhost open, the frame reader, the
//! coalescing writer, heartbeats and close handshakes.

use crate::Stream;
use crate::channel::{Channel, ChannelCmd, Content};
use crate::method::*;
use anyhow::{Context, bail};
use bytes::{Buf, Bytes, BytesMut};
use mq_core::broker::{Broker, ConnectionInfo, PRODUCT, RABBITMQ_VERSION};
use mq_core::error::*;
use mq_core::message::MessageRef;
use mq_core::queue::{Delivery, QueueHandle, QueueType};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::{AmqpError, VHost};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Hands connections speaking another protocol on the same port (AMQP 1.0) to its server.
pub type Handoff = Arc<
    dyn Fn(Arc<Broker>, Box<dyn Stream>, [u8; 8], SocketAddr, SocketAddr, bool) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

#[derive(Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// AMQP 1.0 (RabbitMQ 4 serves both on 5672).
    pub amqp10: Option<Handoff>,
}

/// Accept AMQP connections forever.
pub async fn serve(broker: Arc<Broker>, config: ServerConfig) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.listen).await.with_context(|| format!("bind {}", config.listen))?;
    serve_listener(broker, listener, config).await
}

/// Accept AMQP connections on an already bound listener.
pub async fn serve_listener(broker: Arc<Broker>, listener: TcpListener, config: ServerConfig) -> anyhow::Result<()> {
    info!(addr = %config.listen, "amqp listening");
    broker.listeners.write().push(mq_core::broker::Listener {
        protocol: "amqp".into(),
        port: listener.local_addr()?.port(),
        ip_address: config.listen.ip().to_string(),
    });
    loop {
        let (socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(config.listen);
        let broker = broker.clone();
        let amqp10 = config.amqp10.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(broker, socket, peer, local, false, amqp10).await {
                debug!(%peer, error = %e, "amqp connection ended");
            }
        });
    }
}

/// Outbound items, encoded by the writer task.
pub(crate) enum Out {
    Method(u16, Method),
    Deliver { channel: u16, tag: u64, d: Delivery },
    GetOk { channel: u16, tag: u64, d: Delivery },
    Return { channel: u16, code: u16, text: String, msg: MessageRef },
    /// Flush and stop.
    Close,
}

/// Per-connection state shared by the reader, writer, channels and consumer sinks.
pub(crate) struct ConnShared {
    pub broker: Arc<Broker>,
    pub vhost: Arc<VHost>,
    pub user: mq_core::auth::User,
    pub info: Arc<ConnectionInfo>,
    pub out: mpsc::UnboundedSender<Out>,
    pub frame_max: u32,
    /// Deliveries queued for the writer (socket backpressure for consumers).
    pub backlog: AtomicUsize,
    /// A consumer was refused credit because of the backlog; wake queues once it drains.
    pub stalled: AtomicBool,
    pub consumer_queues: Mutex<Vec<Weak<QueueHandle>>>,
    pub capabilities: FieldTable,
    /// Connection-level errors raised by channels.
    pub ctl: mpsc::UnboundedSender<Ctl>,
    pub client_id: Option<String>,
}

pub(crate) const BACKLOG_HIGH: usize = 4096;
const BACKLOG_LOW: usize = 1024;

impl ConnShared {
    pub fn send(&self, o: Out) {
        let _ = self.out.send(o);
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.get_bool(name).unwrap_or(false)
    }

    pub fn watch_queue(&self, q: &Arc<QueueHandle>) {
        let mut v = self.consumer_queues.lock();
        v.retain(|w| w.strong_count() > 0);
        if !v.iter().any(|w| w.as_ptr() == Arc::as_ptr(q)) {
            v.push(Arc::downgrade(q));
        }
    }

    pub fn wake_queues(&self) {
        for q in self.consumer_queues.lock().iter().filter_map(|w| w.upgrade()) {
            q.wake();
        }
    }
}

pub(crate) enum Ctl {
    /// Close the connection with this error, caused by `class`/`method`.
    Error(AmqpError, u16, u16),
    /// A channel sent channel.close and now awaits close-ok.
    ChannelClosing(u16),
    /// The writer gave up on the socket (peer stopped reading); drop the connection.
    Hangup,
}

/// AMQP's minimum frame_max, and the limit before tune-ok (RabbitMQ's ?FRAME_MIN_SIZE).
const FRAME_MIN_SIZE: u32 = 4096;

struct FrameReader<R> {
    rd: R,
    buf: BytesMut,
    /// Bytes read so far, partial frames included (heartbeat liveness).
    total: u64,
    /// How much to reserve for the next read: starts small so an idle connection costs a few
    /// KiB, and doubles (up to [`READ_CHUNK_MAX`]) whenever a read fills all the room it had.
    chunk: usize,
}

const READ_CHUNK_MIN: usize = 4 * 1024;
const READ_CHUNK_MAX: usize = 64 * 1024;

impl<R> FrameReader<R> {
    fn new(rd: R) -> Self {
        FrameReader { rd, buf: BytesMut::new(), total: 0, chunk: READ_CHUNK_MIN }
    }
}

struct Frame {
    kind: u8,
    channel: u16,
    payload: Bytes,
}

impl<R: tokio::io::AsyncRead + Unpin> FrameReader<R> {
    async fn fill(&mut self, n: usize) -> anyhow::Result<()> {
        while self.buf.len() < n {
            if self.buf.capacity() - self.buf.len() < 512 {
                self.buf.reserve(self.chunk.max(n - self.buf.len()));
            }
            let room = self.buf.capacity() - self.buf.len();
            let read = self.rd.read_buf(&mut self.buf).await?;
            if read == 0 {
                bail!("connection closed by peer");
            }
            if read == room {
                self.chunk = (self.chunk * 2).min(READ_CHUNK_MAX);
            }
            self.total += read as u64;
        }
        Ok(())
    }

    async fn next(&mut self, frame_max: u32) -> Result<Frame, FrameError> {
        self.fill(7).await.map_err(FrameError::Io)?;
        let kind = self.buf[0];
        let channel = u16::from_be_bytes([self.buf[1], self.buf[2]]);
        let size = u32::from_be_bytes([self.buf[3], self.buf[4], self.buf[5], self.buf[6]]) as usize;
        if size as u64 + 8 > frame_max as u64 && frame_max > 0 {
            return Err(FrameError::Protocol(AmqpError::new(FRAME_ERROR, format!("frame size {size} exceeds frame_max {frame_max}"))));
        }
        self.fill(7 + size + 1).await.map_err(FrameError::Io)?;
        let mut f = self.buf.split_to(7 + size + 1);
        if f[7 + size] != FRAME_END {
            return Err(FrameError::Protocol(AmqpError::new(FRAME_ERROR, "frame end marker missing")));
        }
        f.advance(7);
        f.truncate(size);
        Ok(Frame { kind, channel, payload: f.freeze() })
    }
}

enum FrameError {
    Io(anyhow::Error),
    Protocol(AmqpError),
}

fn server_properties(broker: &Broker) -> FieldTable {
    let caps: FieldTable = [
        "publisher_confirms",
        "exchange_exchange_bindings",
        "basic.nack",
        "consumer_cancel_notify",
        "connection.blocked",
        "consumer_priorities",
        "authentication_failure_close",
        "per_consumer_qos",
        "direct_reply_to",
    ]
    .into_iter()
    .map(|c| (c, FieldValue::Bool(true)))
    .collect();
    let mut t = FieldTable::new();
    t.insert("capabilities", caps);
    t.insert("cluster_name", broker.config.cluster_name.as_str());
    t.insert("copyright", "Copyright (c) roll");
    t.insert("information", "roll queues — RabbitMQ-compatible serverless message broker");
    t.insert("platform", "Rust");
    t.insert("product", PRODUCT);
    t.insert("version", RABBITMQ_VERSION);
    t.insert("product_name", "roll queues");
    t.insert("product_version", env!("CARGO_PKG_VERSION"));
    t
}

async fn write_method<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, channel: u16, m: Method) -> anyhow::Result<()> {
    let mut b = BytesMut::new();
    m.frame(channel, &mut b);
    w.write_all(&b).await?;
    Ok(())
}

async fn expect_method<R: tokio::io::AsyncRead + Unpin>(r: &mut FrameReader<R>, frame_max: u32) -> anyhow::Result<Method> {
    loop {
        let f = match tokio::time::timeout(Duration::from_secs(30), r.next(frame_max)).await {
            Ok(Ok(f)) => f,
            Ok(Err(FrameError::Io(e))) => return Err(e),
            Ok(Err(FrameError::Protocol(e))) => bail!("{e}"),
            Err(_) => bail!("handshake timed out"),
        };
        match f.kind {
            FRAME_HEARTBEAT => continue,
            FRAME_METHOD => return Ok(Method::parse(f.payload)?),
            k => bail!("unexpected frame type {k} during handshake"),
        }
    }
}

fn parse_credentials(mechanism: &str, response: &Bytes) -> Option<(String, String)> {
    match mechanism {
        "PLAIN" => {
            let mut parts = response.split(|b| *b == 0);
            let _authzid = parts.next()?;
            let user = String::from_utf8(parts.next()?.to_vec()).ok()?;
            let pass = String::from_utf8(parts.next().unwrap_or_default().to_vec()).ok()?;
            Some((user, pass))
        }
        "AMQPLAIN" => {
            // A field table without its length prefix (and some clients include it).
            let mut framed = BytesMut::new();
            bytes::BufMut::put_u32(&mut framed, response.len() as u32);
            framed.extend_from_slice(response);
            let t = FieldTable::decode(&mut framed.freeze()).or_else(|_| FieldTable::decode(&mut response.clone())).ok()?;
            Some((t.get_str("LOGIN")?.to_string(), t.get_str("PASSWORD").unwrap_or("").to_string()))
        }
        _ => None,
    }
}

/// Serve one connection whose transport is already established.
pub async fn serve_connection<S: Stream>(
    broker: Arc<Broker>,
    mut stream: S,
    peer: SocketAddr,
    local: SocketAddr,
    tls: bool,
    amqp10: Option<Handoff>,
) -> anyhow::Result<()> {
    let mut header = [0u8; 8];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut header)).await.context("protocol header timeout")??;
    if &header != PROTOCOL_HEADER {
        // AMQP 1.0 (protocol id 0 = AMQP, 3 = SASL) shares the port.
        if &header[..4] == b"AMQP"
            && header[5] == 1
            && (header[4] == 0 || header[4] == 3 || header[4] == 2)
            && let Some(h) = amqp10
        {
            h(broker, Box::new(stream), header, peer, local, tls).await;
            return Ok(());
        }
        stream.write_all(PROTOCOL_HEADER).await?;
        bail!("unsupported protocol header {header:?}");
    }
    if broker.draining.load(Ordering::Relaxed) {
        // Let the client retry elsewhere (the proxy or load balancer picks another node).
        stream.shutdown().await.ok();
        bail!("draining");
    }

    let (rd, mut wr) = tokio::io::split(stream);
    let mut reader = FrameReader::new(rd);
    write_method(
        &mut wr,
        0,
        Method::ConnectionStart {
            version_major: 0,
            version_minor: 9,
            server_properties: server_properties(&broker),
            mechanisms: Bytes::from_static(b"PLAIN AMQPLAIN"),
            locales: Bytes::from_static(b"en_US"),
        },
    )
    .await?;

    let Method::ConnectionStartOk { client_properties, mechanism, response, .. } = expect_method(&mut reader, FRAME_MIN_SIZE).await? else {
        bail!("expected connection.start-ok");
    };
    let capabilities = client_properties.get("capabilities").and_then(|v| v.as_table()).cloned().unwrap_or_default();
    let fail_close = capabilities.get_bool("authentication_failure_close").unwrap_or(false);

    let user = match parse_credentials(&mechanism, &response) {
        None => Err(AmqpError::new(ACCESS_REFUSED, format!("unsupported authentication mechanism '{mechanism}'"))),
        Some((u, p)) => broker.authenticate(&u, &p, Some(peer)),
    };
    let user = match user {
        Ok(u) => u,
        Err(e) => {
            warn!(%peer, error = %e, "authentication failed");
            if fail_close {
                write_method(&mut wr, 0, Method::ConnectionClose { reply_code: e.code, reply_text: e.reply_text(), class_id: 0, method_id: 0 }).await?;
            }
            wr.shutdown().await.ok();
            return Ok(());
        }
    };

    let cfg = &broker.config;
    write_method(&mut wr, 0, Method::ConnectionTune { channel_max: cfg.channel_max, frame_max: cfg.frame_max, heartbeat: cfg.heartbeat }).await?;
    let Method::ConnectionTuneOk { channel_max, frame_max, heartbeat } = expect_method(&mut reader, FRAME_MIN_SIZE).await? else {
        bail!("expected connection.tune-ok");
    };
    // Like RabbitMQ: a frame_max under the 4096 minimum or over ours is a hard error.
    if frame_max != 0 && (frame_max < FRAME_MIN_SIZE || (cfg.frame_max != 0 && frame_max > cfg.frame_max)) {
        let e = AmqpError::not_allowed(if frame_max < FRAME_MIN_SIZE {
            format!("frame_max={frame_max} < {FRAME_MIN_SIZE} min size")
        } else {
            format!("frame_max={frame_max} > {} max size", cfg.frame_max)
        });
        write_method(&mut wr, 0, Method::ConnectionClose { reply_code: e.code, reply_text: e.reply_text(), class_id: 10, method_id: 31 }).await?;
        wr.shutdown().await.ok();
        return Ok(());
    }
    let frame_max = if frame_max == 0 { cfg.frame_max } else { frame_max };
    let channel_max = if channel_max == 0 { cfg.channel_max } else { channel_max.min(cfg.channel_max) };

    let Method::ConnectionOpen { virtual_host, .. } = expect_method(&mut reader, frame_max).await? else {
        bail!("expected connection.open");
    };
    let refuse = async |wr: &mut WriteHalf<S>, e: AmqpError| -> anyhow::Result<()> {
        write_method(wr, 0, Method::ConnectionClose { reply_code: e.code, reply_text: e.reply_text(), class_id: 10, method_id: 40 }).await?;
        wr.shutdown().await.ok();
        Ok(())
    };
    if let Err(e) = broker.check_vhost_access(&user, &virtual_host) {
        return refuse(&mut wr, e).await;
    }
    let Some(vhost) = broker.open_vhost(&virtual_host).await else {
        return refuse(&mut wr, AmqpError::new(NOT_ALLOWED, format!("vhost '{virtual_host}' is unavailable"))).await;
    };
    if let Err(e) = broker.check_connection_limits(&user, &virtual_host) {
        return refuse(&mut wr, e).await;
    }
    write_method(&mut wr, 0, Method::ConnectionOpenOk { known_hosts: String::new() }).await?;

    let id = broker.next_id();
    let info = Arc::new(ConnectionInfo {
        id,
        name: format!("{peer} -> {local}"),
        protocol: "AMQP 0-9-1".into(),
        vhost: virtual_host.clone(),
        user: user.name.clone(),
        peer,
        local,
        client_properties: client_properties.to_json(),
        connected_at_ms: mq_core::now_ms(),
        tls,
        auth_mechanism: mechanism.clone(),
        frame_max: frame_max.into(),
        channel_max: (channel_max as u32).into(),
        heartbeat: (heartbeat as u32).into(),
        recv_oct: 0.into(),
        send_oct: 0.into(),
        recv_cnt: 0.into(),
        send_cnt: 0.into(),
        channels: 0.into(),
        blocked: AtomicBool::new(false),
        close_reason: Mutex::new(None),
        close: tokio::sync::Notify::new(),
    });
    broker.register_connection(info.clone());

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let client_id = client_properties.get_str("connection_name").map(String::from);
    let shared = Arc::new(ConnShared {
        broker: broker.clone(),
        vhost,
        user,
        info: info.clone(),
        out: out_tx,
        frame_max,
        backlog: AtomicUsize::new(0),
        stalled: AtomicBool::new(false),
        consumer_queues: Mutex::new(Vec::new()),
        capabilities,
        ctl: ctl_tx,
        client_id,
    });
    let writer = tokio::spawn(writer_loop(wr, out_rx, shared.clone(), heartbeat));
    let result = reader_loop(reader, shared.clone(), ctl_rx, channel_max, heartbeat).await;
    shared.send(Out::Close);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    broker.connection_closed(id).await;
    result
}

struct ChannelEntry {
    tx: mpsc::Sender<ChannelCmd>,
    /// Server sent channel.close; drop frames until close-ok.
    closing: bool,
    pending: Option<PendingContent>,
}

struct PendingContent {
    method: Method,
    props: Option<Bytes>,
    size: u64,
    body: BytesMut,
}

async fn reader_loop<R: tokio::io::AsyncRead + Unpin>(
    mut reader: FrameReader<R>,
    conn: Arc<ConnShared>,
    mut ctl: mpsc::UnboundedReceiver<Ctl>,
    channel_max: u16,
    heartbeat: u16,
) -> anyhow::Result<()> {
    let mut channels: HashMap<u16, ChannelEntry> = HashMap::new();
    // RabbitMQ's receive-side heartbeat check: every interval, see whether any bytes arrived; two
    // silent intervals in a row close the connection (so silence of 2-3 intervals is tolerated).
    let hb_period = Duration::from_secs(if heartbeat > 0 { heartbeat as u64 } else { 3600 });
    let mut hb_tick = tokio::time::interval_at(tokio::time::Instant::now() + hb_period, hb_period);
    hb_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut hb_seen, mut hb_missed) = (0u64, 0u32);
    let frame_max = conn.frame_max;
    let mut closing: Option<tokio::time::Instant> = None;

    let result: anyhow::Result<()> = async {
        loop {
            let frame = tokio::select! {
                biased;
                c = ctl.recv() => {
                    match c {
                        Some(Ctl::Error(e, class, method)) => {
                            close_connection(&conn, &e, class, method);
                            closing = Some(tokio::time::Instant::now());
                        }
                        Some(Ctl::Hangup) => return Ok(()),
                        Some(Ctl::ChannelClosing(n)) => {
                            if let Some(ch) = channels.get_mut(&n) {
                                ch.closing = true;
                                ch.pending = None;
                            }
                        }
                        None => {}
                    }
                    continue;
                }
                _ = conn.info.close.notified() => {
                    let reason = conn.info.close_reason.lock().clone().unwrap_or_else(|| "closed via management plugin".into());
                    close_connection(&conn, &AmqpError::new(CONNECTION_FORCED, reason), 0, 0);
                    closing = Some(tokio::time::Instant::now());
                    continue;
                }
                _ = tokio::time::sleep_until(closing.unwrap_or_else(tokio::time::Instant::now) + Duration::from_secs(3)), if closing.is_some() => {
                    // The peer never answered close-ok.
                    return Ok(());
                }
                _ = hb_tick.tick(), if heartbeat > 0 => {
                    if reader.total != hb_seen {
                        (hb_seen, hb_missed) = (reader.total, 0);
                    } else {
                        hb_missed += 1;
                        if hb_missed >= 2 {
                            warn!(conn = %conn.info.name, "missed heartbeats from client, timeout: {heartbeat}s");
                            return Ok(());
                        }
                    }
                    continue;
                }
                f = reader.next(frame_max) => f,
            };
            let frame = match frame {
                Err(FrameError::Io(e)) => return Err(e),
                Err(FrameError::Protocol(e)) => {
                    // The stream can't be resynchronised after a bad frame: send the close and
                    // hang up (the writer flushes it first), like RabbitMQ.
                    if closing.is_none() {
                        close_connection(&conn, &e, 0, 0);
                    }
                    return Ok(());
                }
                Ok(f) => f,
            };
            conn.info.recv_oct.fetch_add(frame.payload.len() as u64 + 8, Ordering::Relaxed);
            conn.info.recv_cnt.fetch_add(1, Ordering::Relaxed);
            if frame.kind == FRAME_HEARTBEAT {
                continue;
            }
            if frame.channel == 0 {
                if frame.kind != FRAME_METHOD {
                    let e = AmqpError::new(UNEXPECTED_FRAME, "non-method frame on channel 0");
                    close_connection(&conn, &e, 0, 0);
                    closing = Some(tokio::time::Instant::now());
                    continue;
                }
                match Method::parse(frame.payload) {
                    Ok(Method::ConnectionClose { .. }) => {
                        conn.send(Out::Method(0, Method::ConnectionCloseOk {}));
                        return Ok(());
                    }
                    Ok(Method::ConnectionCloseOk {}) => return Ok(()),
                    Ok(Method::ConnectionUpdateSecret { .. }) => conn.send(Out::Method(0, Method::ConnectionUpdateSecretOk {})),
                    Ok(Method::ConnectionBlocked { .. } | Method::ConnectionUnblocked {}) => {}
                    Ok(m) => {
                        let (c, mid) = m.ids();
                        close_connection(&conn, &AmqpError::command_invalid(format!("unexpected method {} on channel 0", m.name())), c, mid);
                        closing = Some(tokio::time::Instant::now());
                    }
                    Err(e) => {
                        close_connection(&conn, &AmqpError::new(SYNTAX_ERROR, e.to_string()), 0, 0);
                        closing = Some(tokio::time::Instant::now());
                    }
                }
                continue;
            }
            if closing.is_some() {
                continue; // only close-ok matters now
            }
            let n = frame.channel;
            match frame.kind {
                FRAME_METHOD => {
                    let m = match Method::parse(frame.payload) {
                        Ok(m) => m,
                        Err(e) => {
                            close_connection(&conn, &AmqpError::new(SYNTAX_ERROR, e.to_string()), 0, 0);
                            closing = Some(tokio::time::Instant::now());
                            continue;
                        }
                    };
                    if let Method::ChannelOpen { .. } = m {
                        if channels.contains_key(&n) {
                            close_connection(&conn, &AmqpError::new(CHANNEL_ERROR, "second 'channel.open' seen"), 20, 10);
                            closing = Some(tokio::time::Instant::now());
                            continue;
                        }
                        if n > channel_max {
                            close_connection(&conn, &AmqpError::not_allowed(format!("channel number {n} exceeds negotiated channel_max {channel_max}")), 20, 10);
                            closing = Some(tokio::time::Instant::now());
                            continue;
                        }
                        if let Some(max) = conn.user.limit("max-channels")
                            && conn.broker.user_channels(&conn.user.name) as i64 >= max
                        {
                            close_connection(&conn, &AmqpError::not_allowed(format!("number of channels opened for user '{}' has reached the maximum allowed user limit ({max})", conn.user.name)), 20, 10);
                            closing = Some(tokio::time::Instant::now());
                            continue;
                        }
                        let tx = Channel::spawn(conn.clone(), n);
                        channels.insert(n, ChannelEntry { tx, closing: false, pending: None });
                        conn.send(Out::Method(n, Method::ChannelOpenOk { channel_id: Bytes::new() }));
                        continue;
                    }
                    let Some(ch) = channels.get_mut(&n) else {
                        if let Method::ChannelCloseOk {} = m {
                            // Answer to our channel.close that crossed the client's own close.
                            continue;
                        }
                        let (c, mid) = m.ids();
                        close_connection(&conn, &AmqpError::new(CHANNEL_ERROR, format!("expected 'channel.open' on channel {n}")), c, mid);
                        closing = Some(tokio::time::Instant::now());
                        continue;
                    };
                    if ch.closing {
                        // Awaiting close-ok; RabbitMQ drops everything else (and accepts close).
                        match m {
                            Method::ChannelCloseOk {} => {
                                channels.remove(&n);
                            }
                            Method::ChannelClose { .. } => {
                                conn.send(Out::Method(n, Method::ChannelCloseOk {}));
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if ch.pending.is_some() {
                        let (c, mid) = m.ids();
                        close_connection(&conn, &AmqpError::new(UNEXPECTED_FRAME, "expected content header for class 60, got non content header frame instead"), c, mid);
                        closing = Some(tokio::time::Instant::now());
                        continue;
                    }
                    if m.has_content() {
                        ch.pending = Some(PendingContent { method: m, props: None, size: 0, body: BytesMut::new() });
                        continue;
                    }
                    let is_close = matches!(m, Method::ChannelClose { .. });
                    if ch.tx.send(ChannelCmd::Method(m)).await.is_err() {
                        channels.remove(&n);
                        continue;
                    }
                    if is_close {
                        channels.remove(&n);
                    }
                }
                FRAME_HEADER => {
                    let Some(ch) = channels.get_mut(&n) else { continue };
                    if ch.closing {
                        continue;
                    }
                    let Some(p) = ch.pending.as_mut().filter(|p| p.props.is_none()) else {
                        close_connection(&conn, &AmqpError::new(UNEXPECTED_FRAME, "unexpected content header"), 0, 0);
                        closing = Some(tokio::time::Instant::now());
                        continue;
                    };
                    let mut payload = frame.payload;
                    if payload.len() < 14 {
                        close_connection(&conn, &AmqpError::new(FRAME_ERROR, "short content header"), 0, 0);
                        closing = Some(tokio::time::Instant::now());
                        continue;
                    }
                    let class = payload.get_u16();
                    if class != 60 {
                        close_connection(&conn, &AmqpError::new(UNEXPECTED_FRAME, format!("expected content header for class 60, got one for class {class} instead")), 60, 40);
                        closing = Some(tokio::time::Instant::now());
                        continue;
                    }
                    let _weight = payload.get_u16();
                    p.size = payload.get_u64();
                    p.props = Some(payload);
                    let max = conn.broker.config.max_message_size;
                    if p.size > max {
                        let e = AmqpError::precondition(format!("message size {} is larger than configured max size {max}", p.size));
                        ch.pending = None;
                        let _ = ch.tx.send(ChannelCmd::Fail(e, 60, 40)).await;
                        continue;
                    }
                    if p.size == 0 {
                        let p = ch.pending.take().unwrap();
                        dispatch_content(ch, p).await;
                    } else {
                        p.body.reserve(p.size as usize);
                    }
                }
                FRAME_BODY => {
                    let Some(ch) = channels.get_mut(&n) else { continue };
                    if ch.closing {
                        continue;
                    }
                    let Some(p) = ch.pending.as_mut().filter(|p| p.props.is_some()) else {
                        close_connection(&conn, &AmqpError::new(UNEXPECTED_FRAME, "unexpected content body"), 0, 0);
                        closing = Some(tokio::time::Instant::now());
                        continue;
                    };
                    p.body.extend_from_slice(&frame.payload);
                    if p.body.len() as u64 > p.size {
                        close_connection(&conn, &AmqpError::new(FRAME_ERROR, "content body exceeds declared size"), 0, 0);
                        closing = Some(tokio::time::Instant::now());
                        continue;
                    }
                    if p.body.len() as u64 == p.size {
                        let p = ch.pending.take().unwrap();
                        dispatch_content(ch, p).await;
                    }
                }
                k => {
                    close_connection(&conn, &AmqpError::new(FRAME_ERROR, format!("unknown frame type {k}")), 0, 0);
                    closing = Some(tokio::time::Instant::now());
                }
            }
        }
    }
    .await;

    for (_, ch) in channels.drain() {
        let _ = ch.tx.send(ChannelCmd::Shutdown).await;
    }
    result
}

async fn dispatch_content(ch: &mut ChannelEntry, p: PendingContent) {
    let Method::BasicPublish { exchange, routing_key, mandatory, immediate, .. } = p.method else { return };
    let content = Content { exchange, routing_key, mandatory, immediate, props: p.props.unwrap_or_default(), body: p.body.freeze() };
    let _ = ch.tx.send(ChannelCmd::Publish(content)).await;
}

fn close_connection(conn: &ConnShared, e: &AmqpError, class_id: u16, method_id: u16) {
    debug!(conn = %conn.info.name, error = %e, "closing connection");
    conn.send(Out::Method(0, Method::ConnectionClose { reply_code: e.code, reply_text: e.reply_text(), class_id, method_id }));
}

async fn writer_loop<W: tokio::io::AsyncWrite + Unpin>(mut wr: W, mut rx: mpsc::UnboundedReceiver<Out>, conn: Arc<ConnShared>, heartbeat: u16) {
    // Grows with the traffic and goes back to the allocator once the connection is idle, so
    // an idle connection holds no write buffer.
    let mut buf = BytesMut::new();
    // Like RabbitMQ: a heartbeat after half the interval without outgoing frames.
    let hb = if heartbeat > 0 { Duration::from_millis(heartbeat as u64 * 500) } else { Duration::from_secs(3600) };
    let frame_max = conn.frame_max;
    let mut quiet = Duration::ZERO;
    'outer: loop {
        // With a buffer to give back, wake up early to free it; then wait out the heartbeat.
        let wait = if buf.capacity() > 0 { IDLE_RELEASE.min(hb - quiet) } else { hb - quiet };
        let first = match tokio::time::timeout(wait, rx.recv()).await {
            Ok(Some(o)) => o,
            Ok(None) => break,
            Err(_) => {
                buf = BytesMut::new();
                quiet += wait;
                if quiet >= hb {
                    quiet = Duration::ZERO;
                    if heartbeat > 0 {
                        heartbeat_frame(&mut buf);
                        if !matches!(tokio::time::timeout(SEND_TIMEOUT, wr.write_all(&buf)).await, Ok(Ok(()))) {
                            let _ = conn.ctl.send(Ctl::Hangup);
                            return;
                        }
                        buf = BytesMut::new();
                    }
                }
                continue;
            }
        };
        quiet = Duration::ZERO;
        let mut item = Some(first);
        let mut delivered = 0usize;
        let mut frames = 0u64;
        while let Some(o) = item.take() {
            frames += 1;
            match o {
                Out::Close => {
                    let _ = tokio::time::timeout(SEND_TIMEOUT, wr.write_all(&buf)).await;
                    break 'outer;
                }
                Out::Method(ch, m) => m.frame(ch, &mut buf),
                Out::Deliver { channel, tag, d } => {
                    delivered += 1;
                    let props = delivery_props(&d);
                    Method::BasicDeliver {
                        consumer_tag: d.consumer_tag.to_string(),
                        delivery_tag: tag,
                        redelivered: d.redelivered,
                        exchange: d.msg.exchange.clone(),
                        routing_key: d.msg.routing_key.clone(),
                    }
                    .frame(channel, &mut buf);
                    content_frames(channel, &props, &d.msg.body, frame_max, &mut buf);
                }
                Out::GetOk { channel, tag, d } => {
                    let props = delivery_props(&d);
                    Method::BasicGetOk {
                        delivery_tag: tag,
                        redelivered: d.redelivered,
                        exchange: d.msg.exchange.clone(),
                        routing_key: d.msg.routing_key.clone(),
                        message_count: d.remaining,
                    }
                    .frame(channel, &mut buf);
                    content_frames(channel, &props, &d.msg.body, frame_max, &mut buf);
                }
                Out::Return { channel, code, text, msg } => {
                    Method::BasicReturn { reply_code: code, reply_text: text, exchange: msg.exchange.clone(), routing_key: msg.routing_key.clone() }
                        .frame(channel, &mut buf);
                    content_frames(channel, &msg.props_raw, &msg.body, frame_max, &mut buf);
                }
            }
            if buf.len() < 512 * 1024 {
                item = rx.try_recv().ok();
            }
        }
        conn.info.send_oct.fetch_add(buf.len() as u64, Ordering::Relaxed);
        conn.info.send_cnt.fetch_add(frames, Ordering::Relaxed);
        match tokio::time::timeout(SEND_TIMEOUT, wr.write_all(&buf)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => break,
            Err(_) => {
                // RabbitMQ's send_timeout/send_timeout_close: a peer that stopped reading is dropped.
                warn!(conn = %conn.info.name, "write timed out after {}s, closing connection", SEND_TIMEOUT.as_secs());
                let _ = conn.ctl.send(Ctl::Hangup);
                return;
            }
        }
        buf.clear();
        if buf.capacity() > 4 << 20 {
            buf = BytesMut::new();
        }
        if delivered > 0 {
            let left = conn.backlog.fetch_sub(delivered, Ordering::AcqRel) - delivered;
            if left < BACKLOG_LOW && conn.stalled.swap(false, Ordering::AcqRel) {
                conn.wake_queues();
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = wr.flush().await;
        let _ = wr.shutdown().await;
    })
    .await;
}

/// How long a connection's writer keeps its buffer with nothing to send.
const IDLE_RELEASE: Duration = Duration::from_secs(1);

/// How long one socket write may block before the connection is dropped (RabbitMQ: 15s).
const SEND_TIMEOUT: Duration = Duration::from_secs(15);

/// Properties as delivered: streams add `x-stream-offset`, quorum queues `x-delivery-count`.
fn delivery_props(d: &Delivery) -> Bytes {
    let add = match d.queue.kind {
        QueueType::Stream => Some(("x-stream-offset", FieldValue::I64(d.seq as i64))),
        QueueType::Quorum if d.delivery_count > 0 => Some(("x-delivery-count", FieldValue::I64(d.delivery_count as i64))),
        _ => None,
    };
    match add {
        None => d.msg.props_raw.clone(),
        Some((k, v)) => {
            let mut p = d.msg.props.clone();
            p.headers.get_or_insert_with(FieldTable::new).insert(k, v);
            p.to_bytes()
        }
    }
}


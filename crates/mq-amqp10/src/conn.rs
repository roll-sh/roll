//! Connection lifecycle: protocol headers, SASL (PLAIN, optional ANONYMOUS), open/close,
//! the frame reader, a coalescing writer with heartbeats, force-close and the event loop
//! that drives every session of the connection.

use crate::frames::{AmqpErr, FRAME_AMQP, FRAME_SASL, Open, Performative, SASL_AUTH, SASL_OK, Sasl, parse_frame, write_frame};
use crate::session::{Event, Session};
use crate::types::Value;
use anyhow::{Context, bail};
use bytes::{Buf, Bytes, BytesMut};
use mq_amqp::Stream;
use mq_core::auth::User;
use mq_core::broker::{Broker, ConnectionInfo, PRODUCT, RABBITMQ_VERSION};
use mq_core::VHost;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub const AMQP_HEADER: [u8; 8] = *b"AMQP\x00\x01\x00\x00";
pub const SASL_HEADER: [u8; 8] = *b"AMQP\x03\x01\x00\x00";
/// The smallest max-frame-size a peer may ask for (spec 2.7.1).
const MIN_MAX_FRAME: u32 = 512;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// AMQP 1.0 server settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address for the standalone server (`serve`); unused for port-shared hand-offs.
    pub listen: SocketAddr,
    /// SASL ANONYMOUS (and clients that skip SASL) log in as this user; `None` disables it.
    pub anonymous_user: Option<String>,
    /// Largest frame accepted from clients (RabbitMQ's `frame_max`).
    pub max_frame_size: u32,
    /// Credit granted to publishing links (RabbitMQ's `max_link_credit`).
    pub max_link_credit: u32,
    /// Highest session channel number (RabbitMQ's `session_max_per_connection` − 1).
    pub channel_max: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: SocketAddr::from(([127, 0, 0, 1], 5672)),
            anonymous_user: None,
            max_frame_size: 128 * 1024,
            max_link_credit: 128,
            channel_max: 255,
        }
    }
}

/// A hand-off for `mq_amqp::ServerConfig::amqp10`: AMQP 1.0 connections on the AMQP port.
pub fn handoff(config: Config) -> mq_amqp::Handoff {
    Arc::new(move |broker: Arc<Broker>, stream: Box<dyn Stream>, header: [u8; 8], peer: SocketAddr, local: SocketAddr, tls: bool| {
        let config = config.clone();
        Box::pin(async move {
            if let Err(e) = serve_connection(broker, stream, header, peer, local, tls, config).await {
                debug!(%peer, error = %e, "amqp 1.0 connection ended");
            }
        })
    })
}

/// Accept AMQP 1.0 connections forever on `config.listen`.
pub async fn serve(broker: Arc<Broker>, config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.listen).await.with_context(|| format!("bind {}", config.listen))?;
    serve_listener(broker, listener, config).await
}

/// Accept AMQP 1.0 connections on an already bound listener.
pub async fn serve_listener(broker: Arc<Broker>, listener: TcpListener, config: Config) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, "amqp 1.0 listening");
    broker.listeners.write().push(mq_core::broker::Listener { protocol: "amqp".into(), port: addr.port(), ip_address: addr.ip().to_string() });
    loop {
        let (mut socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(addr);
        let broker = broker.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let mut header = [0u8; 8];
            match tokio::time::timeout(Duration::from_secs(10), socket.read_exact(&mut header)).await {
                Ok(Ok(_)) => {}
                _ => return,
            }
            if let Err(e) = serve_connection(broker, socket, header, peer, local, false, config).await {
                debug!(%peer, error = %e, "amqp 1.0 connection ended");
            }
        });
    }
}

/// Reads whole frames; cancel-safe (partial reads stay buffered).
pub(crate) struct FrameReader<R> {
    rd: R,
    buf: BytesMut,
    max: u32,
}

pub(crate) enum ReadError {
    Io(anyhow::Error),
    Framing(String),
}

impl<R: tokio::io::AsyncRead + Unpin> FrameReader<R> {
    async fn fill(&mut self, n: usize) -> Result<(), ReadError> {
        while self.buf.len() < n {
            match self.rd.read_buf(&mut self.buf).await {
                Ok(0) => return Err(ReadError::Io(anyhow::anyhow!("connection closed by peer"))),
                Ok(_) => {}
                Err(e) => return Err(ReadError::Io(e.into())),
            }
        }
        Ok(())
    }

    async fn header(&mut self) -> Result<[u8; 8], ReadError> {
        self.fill(8).await?;
        let mut h = [0u8; 8];
        self.buf.split_to(8).copy_to_slice(&mut h);
        Ok(h)
    }

    /// The next complete frame (header included).
    async fn next(&mut self) -> Result<Bytes, ReadError> {
        self.fill(4).await?;
        let size = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if size < 8 {
            return Err(ReadError::Framing(format!("frame size {size} is smaller than the frame header")));
        }
        if size > self.max {
            return Err(ReadError::Framing(format!("frame size {size} exceeds max frame size {}", self.max)));
        }
        self.fill(size as usize).await?;
        Ok(self.buf.split_to(size as usize).freeze())
    }
}

/// Connection-wide state shared by sessions.
pub(crate) struct Ctx {
    pub broker: Arc<Broker>,
    pub vhost: Arc<VHost>,
    pub user: User,
    pub info: Arc<ConnectionInfo>,
    pub events: mpsc::UnboundedSender<Event>,
    pub config: Config,
}

/// Outbound frames of one loop iteration, handed to the writer in one piece.
pub(crate) struct Out {
    pub buf: BytesMut,
    /// The peer's max frame size (capped), for splitting transfers.
    pub max_frame: usize,
    pub frames: u64,
}

impl Out {
    pub fn send(&mut self, channel: u16, p: &Performative) {
        write_frame(&mut self.buf, FRAME_AMQP, channel, Some(&p.to_value()), &[]);
        self.frames += 1;
    }

    pub fn raw(&mut self, frame: &[u8]) {
        self.buf.extend_from_slice(frame);
        self.frames += 1;
    }
}

enum WriterMsg {
    Data(Bytes, u64),
    Close,
}

fn sasl_frame(s: &Sasl) -> BytesMut {
    let mut b = BytesMut::new();
    write_frame(&mut b, FRAME_SASL, 0, Some(&s.to_value()), &[]);
    b
}

fn amqp_frame(p: &Performative) -> BytesMut {
    let mut b = BytesMut::new();
    write_frame(&mut b, FRAME_AMQP, 0, Some(&p.to_value()), &[]);
    b
}

fn decode_body<T>(f: Bytes, kind: u8, parse: impl FnOnce(Value) -> crate::types::CodecResult<T>) -> anyhow::Result<T> {
    let raw = parse_frame(f)?;
    if raw.kind != kind {
        bail!("unexpected frame type {}", raw.kind);
    }
    let mut body = raw.body;
    if body.is_empty() {
        bail!("unexpected empty frame");
    }
    Ok(parse(Value::decode(&mut body)?)?)
}

fn server_properties(broker: &Broker) -> Value {
    let s = |k: &str, v: &str| (Value::sym(k), Value::str(v));
    Value::Map(vec![
        s("product", PRODUCT),
        s("version", RABBITMQ_VERSION),
        s("platform", "Rust"),
        s("copyright", "Copyright (c) roll"),
        s("information", "roll queues — RabbitMQ-compatible serverless message broker"),
        s("cluster_name", &broker.config.cluster_name),
        // RabbitMQ's AMQP 1.0 clients refuse to start without it (they use it for queue affinity).
        s("node", &broker.config.node_name),
        s("product_name", "roll queues"),
    ])
}

/// The client's `open.properties` as a table (shown by the management API).
fn client_properties(p: &Option<Value>) -> mq_core::FieldTable {
    let mut t = mq_core::FieldTable::new();
    if let Some(Value::Map(m)) = p {
        for (k, v) in m {
            if let (Some(k), Some(fv)) = (k.as_str(), crate::message::to_091(v)) {
                t.insert(k, fv);
            }
        }
    }
    t
}

/// Serve one AMQP 1.0 connection whose 8-byte protocol header has already been read.
pub async fn serve_connection<S: Stream>(
    broker: Arc<Broker>,
    stream: S,
    header: [u8; 8],
    peer: SocketAddr,
    local: SocketAddr,
    tls: bool,
    config: Config,
) -> anyhow::Result<()> {
    let (rd, mut wr) = tokio::io::split(stream);
    let mut reader = FrameReader { rd, buf: BytesMut::with_capacity(64 * 1024), max: config.max_frame_size.max(MIN_MAX_FRAME) };
    if broker.draining.load(Ordering::Relaxed) {
        wr.shutdown().await.ok();
        bail!("draining");
    }

    // ── SASL ──
    let (user, mechanism) = match header {
        SASL_HEADER => {
            wr.write_all(&SASL_HEADER).await?;
            let mut mechs = vec!["PLAIN".to_string()];
            if config.anonymous_user.is_some() {
                mechs.push("ANONYMOUS".into());
            }
            wr.write_all(&sasl_frame(&Sasl::Mechanisms(mechs))).await?;
            let f = match tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.next()).await {
                Ok(Ok(f)) => f,
                Ok(Err(ReadError::Io(e))) => return Err(e),
                Ok(Err(ReadError::Framing(e))) => bail!("{e}"),
                Err(_) => bail!("SASL handshake timed out"),
            };
            let init = decode_body(f, FRAME_SASL, Sasl::decode)?;
            let Sasl::Init { mechanism, initial_response, .. } = init else { bail!("expected sasl-init") };
            let user = match mechanism.as_str() {
                "PLAIN" => {
                    let resp = initial_response.unwrap_or_default();
                    let mut parts = resp.split(|b| *b == 0);
                    let _authzid = parts.next();
                    let u = parts.next().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                    let p = parts.next().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                    broker.authenticate(&u, &p, Some(peer)).map_err(|e| e.text)
                }
                "ANONYMOUS" => anonymous(&broker, &config, peer),
                m => Err(format!("unsupported authentication mechanism '{m}'")),
            };
            match user {
                Ok(u) => {
                    wr.write_all(&sasl_frame(&Sasl::Outcome { code: SASL_OK })).await?;
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.header()).await {
                        Ok(Ok(h)) if h == AMQP_HEADER => {}
                        Ok(Ok(h)) => {
                            wr.write_all(&AMQP_HEADER).await.ok();
                            bail!("unexpected protocol header {h:?} after SASL");
                        }
                        Ok(Err(ReadError::Io(e))) => return Err(e),
                        _ => bail!("protocol header timeout"),
                    }
                    (u, mechanism)
                }
                Err(text) => {
                    warn!(%peer, mechanism = %mechanism, error = %text, "AMQP 1.0 authentication failed");
                    wr.write_all(&sasl_frame(&Sasl::Outcome { code: SASL_AUTH })).await.ok();
                    wr.shutdown().await.ok();
                    return Ok(());
                }
            }
        }
        AMQP_HEADER => match anonymous(&broker, &config, peer) {
            Ok(u) => (u, "ANONYMOUS".to_string()),
            Err(_) => {
                // SASL is required: tell the client which header we expect.
                wr.write_all(&SASL_HEADER).await.ok();
                wr.shutdown().await.ok();
                return Ok(());
            }
        },
        _ => {
            wr.write_all(&SASL_HEADER).await.ok();
            wr.shutdown().await.ok();
            bail!("unsupported AMQP 1.0 protocol header {header:?}");
        }
    };
    wr.write_all(&AMQP_HEADER).await?;

    // ── open ──
    let f = match tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.next()).await {
        Ok(Ok(f)) => f,
        Ok(Err(ReadError::Io(e))) => return Err(e),
        Ok(Err(ReadError::Framing(e))) => bail!("{e}"),
        Err(_) => bail!("open timed out"),
    };
    let open = match decode_body(f, FRAME_AMQP, Performative::decode)? {
        Performative::Open(o) => o,
        p => bail!("expected open, got {}", p.name()),
    };
    let heartbeat_ms = broker.config.heartbeat as u32 * 1000;
    let our_open = Performative::Open(Open {
        container_id: broker.config.cluster_name.clone(),
        hostname: None,
        max_frame_size: config.max_frame_size,
        channel_max: config.channel_max,
        idle_time_out: (heartbeat_ms > 0).then_some(heartbeat_ms / 2),
        offered_capabilities: vec!["LINK_PAIR_V1_0".into(), "ANONYMOUS-RELAY".into()],
        desired_capabilities: vec![],
        properties: Some(server_properties(&broker)),
    });
    let vhost_name = match open.hostname.as_deref() {
        Some(h) if h.starts_with("vhost:") => h["vhost:".len()..].to_string(),
        _ => broker.config.default_vhost.clone(),
    };
    let refuse = async |wr: &mut tokio::io::WriteHalf<S>, e: AmqpErr| -> anyhow::Result<()> {
        let mut b = amqp_frame(&our_open);
        b.extend_from_slice(&amqp_frame(&Performative::Close(Some(e))));
        wr.write_all(&b).await?;
        wr.shutdown().await.ok();
        Ok(())
    };
    if !broker.vhost_exists(&vhost_name) {
        let e = AmqpErr::new("amqp:unauthorized-access", format!("AMQP 1.0 connection failed: virtual host '{vhost_name}' does not exist"));
        return refuse(&mut wr, e).await;
    }
    let check = broker.check_vhost_access(&user, &vhost_name).and_then(|_| broker.check_connection_limits(&user, &vhost_name));
    if let Err(e) = check {
        let cond = if e.text.contains("limit") { "amqp:resource-limit-exceeded" } else { "amqp:unauthorized-access" };
        return refuse(&mut wr, AmqpErr::new(cond, e.text)).await;
    }
    if open.max_frame_size < MIN_MAX_FRAME {
        let e = AmqpErr::new("amqp:frame-size-too-small", format!("max_frame_size ({}) < minimum maximum frame size ({MIN_MAX_FRAME})", open.max_frame_size));
        return refuse(&mut wr, e).await;
    }
    if let Some(t) = open.idle_time_out
        && t > 0
        && t < 1000
    {
        let e = AmqpErr::new("amqp:not-allowed", format!("idle-time-out ({t} ms) < minimum idle-time-out (1000 ms)"));
        return refuse(&mut wr, e).await;
    }
    let Some(vhost) = broker.open_vhost(&vhost_name).await else {
        return refuse(&mut wr, AmqpErr::new("amqp:internal-error", format!("AMQP 1.0 connection failed: virtual host '{vhost_name}' is down"))).await;
    };
    wr.write_all(&amqp_frame(&our_open)).await?;

    let id = broker.next_id();
    let client_props = client_properties(&open.properties);
    let info = Arc::new(ConnectionInfo {
        id,
        name: format!("{peer} -> {local}"),
        protocol: "AMQP 1-0".into(),
        vhost: vhost_name.clone(),
        user: user.name.clone(),
        peer,
        local,
        client_properties: client_props.to_json(),
        connected_at_ms: mq_core::now_ms(),
        tls,
        auth_mechanism: mechanism,
        frame_max: open.max_frame_size.into(),
        channel_max: (open.channel_max.min(config.channel_max) as u32).into(),
        heartbeat: (heartbeat_ms / 1000).into(),
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
    debug!(container = %open.container_id, user = %user.name, vhost = %vhost_name, "AMQP 1.0 connection opened");

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let ctx = Arc::new(Ctx { broker: broker.clone(), vhost, user, info: info.clone(), events: events_tx, config: config.clone() });
    let (wtx, wrx) = mpsc::unbounded_channel();
    // The peer wants a frame at least every half of its idle timeout.
    let send_every = open.idle_time_out.filter(|t| *t > 0).map(|t| Duration::from_millis((t / 2).max(500) as u64));
    let writer = tokio::spawn(writer_loop(wr, wrx, info.clone(), send_every));
    let max_frame = open.max_frame_size.min(1 << 20) as usize;
    let recv_timeout = (heartbeat_ms > 0).then(|| Duration::from_millis(heartbeat_ms as u64 * 2));
    let mut conn = Conn { ctx, sessions: HashMap::new(), out: Out { buf: BytesMut::new(), max_frame, frames: 0 }, channel_max: open.channel_max.min(config.channel_max) };
    let result = conn.run(&mut reader, events_rx, &wtx, recv_timeout).await;
    conn.shutdown().await;
    let _ = wtx.send(WriterMsg::Close);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    broker.connection_closed(id).await;
    result
}

fn anonymous(broker: &Broker, config: &Config, peer: SocketAddr) -> Result<User, String> {
    let name = config.anonymous_user.as_deref().ok_or("ANONYMOUS login is disabled")?;
    let user = broker.user(name).ok_or_else(|| format!("user '{name}' does not exist"))?;
    if !peer.ip().is_loopback() && broker.config.loopback_users.iter().any(|u| u == name) {
        return Err(format!("user '{name}' can only connect via localhost"));
    }
    Ok(user)
}

async fn writer_loop<W: tokio::io::AsyncWrite + Unpin>(mut wr: W, mut rx: mpsc::UnboundedReceiver<WriterMsg>, info: Arc<ConnectionInfo>, every: Option<Duration>) {
    const HEARTBEAT: [u8; 8] = [0, 0, 0, 8, 2, 0, 0, 0];
    let idle = every.unwrap_or(Duration::from_secs(3600));
    loop {
        let msg = match tokio::time::timeout(idle, rx.recv()).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(_) => {
                if every.is_some() && wr.write_all(&HEARTBEAT).await.is_err() {
                    break;
                }
                continue;
            }
        };
        match msg {
            WriterMsg::Close => break,
            WriterMsg::Data(b, frames) => {
                // Coalesce whatever else is already queued into one write.
                let mut buf = BytesMut::from(&b[..]);
                let mut n = frames;
                let mut close = false;
                while buf.len() < 1 << 20 {
                    match rx.try_recv() {
                        Ok(WriterMsg::Data(b, f)) => {
                            buf.extend_from_slice(&b);
                            n += f;
                        }
                        Ok(WriterMsg::Close) => {
                            close = true;
                            break;
                        }
                        Err(_) => break,
                    }
                }
                info.send_oct.fetch_add(buf.len() as u64, Ordering::Relaxed);
                info.send_cnt.fetch_add(n, Ordering::Relaxed);
                if wr.write_all(&buf).await.is_err() {
                    break;
                }
                if close {
                    break;
                }
            }
        }
    }
    let _ = wr.flush().await;
    let _ = wr.shutdown().await;
}

struct Conn {
    ctx: Arc<Ctx>,
    sessions: HashMap<u16, Session>,
    out: Out,
    channel_max: u16,
}

/// Why the loop should stop.
enum Stop {
    /// Close handshake done (or the peer is gone).
    Done,
}

impl Conn {
    async fn run<R: tokio::io::AsyncRead + Unpin>(
        &mut self,
        reader: &mut FrameReader<R>,
        mut events: mpsc::UnboundedReceiver<Event>,
        wtx: &mpsc::UnboundedSender<WriterMsg>,
        recv_timeout: Option<Duration>,
    ) -> anyhow::Result<()> {
        let idle = recv_timeout.unwrap_or(Duration::from_secs(u64::MAX / 4));
        let mut closing: Option<tokio::time::Instant> = None;
        let info = self.ctx.info.clone();
        loop {
            let r: Result<Option<Stop>, anyhow::Error> = tokio::select! {
                biased;
                ev = events.recv() => {
                    if let Some(ev) = ev {
                        self.on_event(ev).await;
                        // Drain what's queued so dispositions and transfers coalesce.
                        let mut n = 0;
                        while n < 1024 {
                            match events.try_recv() {
                                Ok(ev) => self.on_event(ev).await,
                                Err(_) => break,
                            }
                            n += 1;
                        }
                        for s in self.sessions.values_mut() {
                            s.flush_dispositions(&mut self.out);
                        }
                    }
                    Ok(None)
                }
                _ = info.close.notified(), if closing.is_none() => {
                    let reason = info.close_reason.lock().clone().unwrap_or_else(|| "closed via management plugin".into());
                    self.close(Some(AmqpErr::new("amqp:connection:forced", reason)));
                    closing = Some(tokio::time::Instant::now());
                    Ok(None)
                }
                _ = tokio::time::sleep(Duration::from_secs(3)), if closing.is_some() => Ok(Some(Stop::Done)),
                f = tokio::time::timeout(idle, reader.next()) => match f {
                    Err(_) => {
                        warn!(conn = %info.name, "missed heartbeats from AMQP 1.0 client");
                        Ok(Some(Stop::Done))
                    }
                    Ok(Err(ReadError::Io(e))) => Err(e),
                    Ok(Err(ReadError::Framing(text))) => {
                        self.close(Some(AmqpErr::new("amqp:connection:framing-error", text)));
                        closing = Some(tokio::time::Instant::now());
                        Ok(None)
                    }
                    Ok(Ok(frame)) => {
                        info.recv_oct.fetch_add(frame.len() as u64, Ordering::Relaxed);
                        info.recv_cnt.fetch_add(1, Ordering::Relaxed);
                        match self.on_frame(frame, closing.is_some()).await {
                            Ok(true) => Ok(Some(Stop::Done)),
                            Ok(false) => Ok(None),
                            Err(e) => {
                                self.close(Some(e));
                                closing = Some(tokio::time::Instant::now());
                                Ok(None)
                            }
                        }
                    }
                },
            };
            if !self.out.buf.is_empty() {
                let frames = std::mem::take(&mut self.out.frames);
                let _ = wtx.send(WriterMsg::Data(self.out.buf.split().freeze(), frames));
            }
            match r {
                Ok(None) => {}
                Ok(Some(Stop::Done)) => break Ok(()),
                Err(e) => break Err(e),
            }
        }
    }

    fn close(&mut self, e: Option<AmqpErr>) {
        if let Some(e) = &e {
            debug!(conn = %self.ctx.info.name, error = %e, "closing AMQP 1.0 connection");
        }
        self.out.send(0, &Performative::Close(e));
    }

    /// Handle one frame; `Ok(true)` ends the connection.
    async fn on_frame(&mut self, frame: Bytes, closing: bool) -> Result<bool, AmqpErr> {
        let raw = parse_frame(frame).map_err(|e| AmqpErr::new("amqp:connection:framing-error", e.0))?;
        if raw.body.is_empty() {
            return Ok(false);
        }
        if raw.kind != FRAME_AMQP {
            return Err(AmqpErr::new("amqp:connection:framing-error", format!("unexpected frame type {}", raw.kind)));
        }
        let mut body = raw.body;
        let v = Value::decode(&mut body).map_err(|e| AmqpErr::new("amqp:decode-error", e.0))?;
        let p = Performative::decode(v).map_err(|e| AmqpErr::new("amqp:decode-error", e.0))?;
        let ch = raw.channel;
        match p {
            Performative::Close(e) => {
                if let Some(e) = e {
                    debug!(conn = %self.ctx.info.name, error = %e, "client closed the connection with an error");
                }
                if !closing {
                    self.out.send(0, &Performative::Close(None));
                }
                return Ok(true);
            }
            _ if closing => return Ok(false),
            Performative::Open(_) => return Err(AmqpErr::new("amqp:illegal-state", "unexpected open")),
            Performative::Begin(b) => {
                if self.sessions.contains_key(&ch) {
                    return Err(AmqpErr::new("amqp:connection:framing-error", format!("channel {ch} is already in use")));
                }
                if ch > self.channel_max {
                    return Err(AmqpErr::new("amqp:connection:framing-error", format!("channel number ({ch}) exceeds maximum channel number ({})", self.channel_max)));
                }
                if b.remote_channel.is_some() {
                    return Err(AmqpErr::new("amqp:not-implemented", "server-initiated sessions are not supported"));
                }
                let s = Session::begin(self.ctx.clone(), ch, &b, &mut self.out);
                self.sessions.insert(ch, s);
            }
            p => {
                let Some(s) = self.sessions.get_mut(&ch) else {
                    return Err(AmqpErr::new("amqp:connection:framing-error", format!("{} on unknown channel {ch}", p.name())));
                };
                let ended = s.on_performative(p, body, &mut self.out).await;
                if ended && let Some(s) = self.sessions.remove(&ch) {
                    s.cleanup().await;
                }
            }
        }
        Ok(false)
    }

    async fn on_event(&mut self, ev: Event) {
        let Some(ch) = ev.channel() else { return };
        match self.sessions.get_mut(&ch) {
            Some(s) => s.on_event(ev, &mut self.out).await,
            None => ev.orphaned().await,
        }
    }

    async fn shutdown(&mut self) {
        for (_, s) in self.sessions.drain() {
            s.cleanup().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_carries_the_properties_rabbitmq_clients_read() {
        let broker = Broker::start(mq_core::BrokerConfig::default(), mq_core::meta::MemMeta::new(), mq_core::log::MemStore::new()).await.unwrap();
        let Value::Map(props) = server_properties(&broker) else { panic!("not a map") };
        let get = |k: &str| props.iter().find(|(key, _)| *key == Value::sym(k)).map(|(_, v)| v.clone());
        for k in ["product", "version", "node", "cluster_name"] {
            assert!(matches!(get(k), Some(Value::String(_))), "{k} missing");
        }
        assert_eq!(get("node"), Some(Value::str(broker.config.node_name.clone())));
    }
}

//! One STOMP connection: login and version negotiation, the frame loop (subscriptions, sends,
//! acks, transactions, receipts), the consumer sinks, the coalescing writer and heart-beats.

use crate::dest::{self, Dest, DestError};
use crate::frame::{self, Encoder, Frame, Limits, Parser, SUPPORTED_VERSIONS, Version};
use crate::{Config, Stream, props};
use anyhow::Context;
use bytes::{Bytes, BytesMut};
use mq_core::auth::{Access, User};
use mq_core::broker::{Broker, ChannelInfo, ConnectionInfo, RABBITMQ_VERSION};
use mq_core::exchange::{Dest as BindDest, ExchangeKind};
use mq_core::message::Message;
use mq_core::queue::{ConfirmTicket, ConsumerSink, ConsumerSpec, Delivery, Outcome, QueueHandle, QueueRef, QueueType};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::vhost::DeclareQueue;
use mq_core::{AmqpError, VHost};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

const BACKLOG_HIGH: usize = 4096;
const BACKLOG_LOW: usize = 1024;
const MAX_TRANSACTIONS: usize = 16;
const MAX_TRANSACTION_ACTIONS: usize = 1024;
const UNAUTHENTICATED_MAX_FRAME: usize = 64 * 1024;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Accept STOMP connections forever.
pub async fn serve(broker: Arc<Broker>, config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.listen).await.with_context(|| format!("bind {}", config.listen))?;
    serve_listener(broker, listener, config).await
}

/// Accept STOMP connections on an already bound listener.
pub async fn serve_listener(broker: Arc<Broker>, listener: TcpListener, config: Config) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, "stomp listening");
    broker.listeners.write().push(mq_core::broker::Listener { protocol: "stomp".into(), port: addr.port(), ip_address: addr.ip().to_string() });
    let config = Arc::new(config);
    loop {
        let (socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(addr);
        let (broker, config) = (broker.clone(), config.clone());
        tokio::spawn(async move {
            if let Err(e) = serve_connection(broker, socket, peer, local, false, config).await {
                debug!(%peer, error = %e, "stomp connection ended");
            }
        });
    }
}

/// A STOMP ERROR: the `message` header and the body text.
#[derive(Debug)]
pub(crate) struct StompError {
    message: String,
    detail: String,
}

fn err(message: impl Into<String>, detail: impl Into<String>) -> StompError {
    StompError { message: message.into(), detail: detail.into() }
}

impl From<AmqpError> for StompError {
    /// AMQP errors surface the way RabbitMQ reports them: `message:not_found`, the text as body.
    fn from(e: AmqpError) -> Self {
        StompError { message: e.name().to_ascii_lowercase(), detail: e.text }
    }
}

type Res<T> = Result<T, StompError>;

/// Erlang's `~tp` rendering of a binary, which RabbitMQ uses in several error texts.
fn tp(s: &str) -> String {
    format!("<<\"{s}\">>")
}

fn error_frame(e: &StompError, extra: &[(&str, &str)], trailing_lf: bool) -> Bytes {
    let mut h = vec![("message", e.message.as_str()), ("content-type", "text/plain"), ("version", SUPPORTED_VERSIONS)];
    h.extend_from_slice(extra);
    frame::encode("ERROR", &h, e.detail.as_bytes(), trailing_lf)
}

/// Read until the peer closes or a short grace period passes, so an ERROR isn't lost to a
/// reset caused by unread input.
async fn linger<R: AsyncRead + Unpin>(rd: &mut R) {
    let mut buf = [0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        while let Ok(n) = rd.read(&mut buf).await {
            if n == 0 {
                break;
            }
        }
    })
    .await;
}

async fn read_frame<R: AsyncRead + Unpin>(rd: &mut R, p: &mut Parser, v: Option<Version>) -> Result<Option<Frame>, StompError> {
    loop {
        match p.next(v) {
            Ok(Some(f)) => return Ok(Some(f)),
            Ok(None) => {}
            Err(e) => return Err(err("Invalid frame", e.to_string())),
        }
        match rd.read_buf(&mut p.buf).await {
            Ok(0) | Err(_) => return Ok(None),
            Ok(_) => {}
        }
    }
}

struct Login {
    user: User,
    vhost: Arc<VHost>,
    version: Version,
    /// Our send interval and the client's, in ms (0 = off).
    send_ms: u64,
    recv_ms: u64,
    mechanism: &'static str,
}

/// RabbitMQ rounds heart-beat intervals to whole seconds (at least one).
fn heartbeat_ms(ms: u64) -> u64 {
    match ms {
        0 => 0,
        1..1000 => 1000,
        ms => ms / 1000 * 1000,
    }
}

async fn login(broker: &Arc<Broker>, config: &Config, f: &Frame, peer: SocketAddr) -> Res<Login> {
    let version = Version::negotiate(f.header("accept-version")).ok_or_else(|| err("Version mismatch", format!("Supported versions are {SUPPORTED_VERSIONS}\n")))?;
    let h = |k: &str| f.header(k).map(|v| if version == Version::V10 { v.trim_start() } else { v }.to_string());
    let (default_login, default_pass) = config.default_login.clone().unzip();
    let (login, mechanism) = match h("login") {
        Some(l) => (l, "stomp_headers"),
        None => (default_login.unwrap_or_default(), "config"),
    };
    let pass = h("passcode").or(default_pass).unwrap_or_default();
    let vhost_name = h("host").or_else(|| config.default_vhost.clone()).unwrap_or_else(|| broker.config.default_vhost.clone());
    let user = broker.authenticate(&login, &pass, Some(peer)).map_err(|e| {
        warn!(%peer, user = %login, "STOMP login failed: {}", e.text);
        if e.text.contains("can only connect via localhost") {
            err("Bad CONNECT", "non-loopback access denied")
        } else {
            err("Bad CONNECT", format!("Access refused for user '{login}'"))
        }
    })?;
    let denied = || err("Bad CONNECT", format!("Virtual host '{vhost_name}' access denied"));
    broker.check_vhost_access(&user, &vhost_name).map_err(|_| denied())?;
    let vhost = broker.open_vhost(&vhost_name).await.ok_or_else(denied)?;
    broker.check_connection_limits(&user, &vhost_name).map_err(|e| {
        if e.text.contains("User connection limit") {
            err("Bad CONNECT", "Connection refused: user connection limit reached")
        } else {
            err("Bad CONNECT", "Connection refused: vhost connection limit reached")
        }
    })?;
    let hb = h("heart-beat").unwrap_or_else(|| "0,0".into());
    let (cx, cy) = hb
        .split_once(',')
        .and_then(|(x, y)| Some((x.trim().parse::<u64>().ok()?, y.trim().parse::<u64>().ok()?)))
        .ok_or_else(|| err("Processing error", format!("Invalid heart-beat header '{hb}'")))?;
    Ok(Login { user, vhost, version, send_ms: heartbeat_ms(cy), recv_ms: heartbeat_ms(cx), mechanism })
}

/// Serve one connection whose transport is already established. `tls` is reported to the
/// management API.
pub async fn serve_connection<S: Stream>(
    broker: Arc<Broker>,
    stream: S,
    peer: SocketAddr,
    local: SocketAddr,
    tls: bool,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    if broker.draining.load(Ordering::Relaxed) {
        wr.shutdown().await.ok();
        return Ok(());
    }
    let limits = Limits { max_frame: config.max_frame_size.min(UNAUTHENTICATED_MAX_FRAME), max_headers: config.max_headers, max_header_len: config.max_header_length };
    let mut parser = Parser::new(limits);
    let tlf = config.trailing_lf;
    let refuse = async |wr: &mut tokio::io::WriteHalf<S>, rd: &mut tokio::io::ReadHalf<S>, e: StompError| {
        let _ = wr.write_all(&error_frame(&e, &[], tlf)).await;
        let _ = wr.shutdown().await;
        linger(rd).await;
    };
    let first = match tokio::time::timeout(LOGIN_TIMEOUT, read_frame(&mut rd, &mut parser, None)).await {
        Ok(Ok(Some(f))) => f,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => {
            refuse(&mut wr, &mut rd, e).await;
            return Ok(());
        }
        Err(_) => {
            debug!(%peer, "stomp login timed out");
            return Ok(());
        }
    };
    let explicit = matches!(first.command.as_str(), "CONNECT" | "STOMP");
    if !explicit && !config.implicit_connect {
        refuse(&mut wr, &mut rd, err("Illegal command", "You must log in using CONNECT first")).await;
        return Ok(());
    }
    let connect = if explicit { first.clone() } else { Frame::default() };
    let auth = match login(&broker, &config, &connect, peer).await {
        Ok(l) => l,
        Err(e) => {
            refuse(&mut wr, &mut rd, e).await;
            return Ok(());
        }
    };
    let session = dest::guid("session");
    if explicit {
        let hb = format!("{},{}", auth.send_ms, auth.recv_ms);
        let server = format!("RabbitMQ/{RABBITMQ_VERSION}");
        let headers = [("server", server.as_str()), ("session", session.as_str()), ("heart-beat", hb.as_str()), ("version", auth.version.as_str())];
        wr.write_all(&frame::encode("CONNECTED", &headers, b"", tlf)).await?;
        wr.flush().await?;
    }
    parser.limits.max_frame = config.max_frame_size;

    let id = broker.next_id();
    let name = format!("{peer} -> {local}");
    let vhost_name = auth.vhost.name.clone();
    let info = Arc::new(ConnectionInfo {
        id,
        name: name.clone(),
        protocol: format!("STOMP {}", auth.version.as_str()),
        vhost: vhost_name.clone(),
        user: auth.user.name.clone(),
        peer,
        local,
        client_properties: serde_json::json!({ "product": "STOMP client" }),
        connected_at_ms: mq_core::now_ms(),
        tls,
        auth_mechanism: auth.mechanism.into(),
        frame_max: 0.into(),
        channel_max: 1.into(),
        heartbeat: ((auth.send_ms / 1000) as u32).into(),
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
    let chan = Arc::new(ChannelInfo {
        id: broker.next_id(),
        conn: id,
        number: 1,
        conn_name: name,
        vhost: vhost_name,
        user: auth.user.name.clone(),
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
    broker.register_channel(chan.clone());

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        broker: broker.clone(),
        vhost: auth.vhost,
        user: auth.user,
        info: info.clone(),
        chan,
        out: out_tx,
        ctl: ctl_tx,
        session,
        version: auth.version,
        trailing_lf: tlf,
        backlog: AtomicUsize::new(0),
        stalled: AtomicBool::new(false),
        closing: AtomicBool::new(false),
        watched: Mutex::new(Vec::new()),
        acks: Mutex::new(Acks::default()),
    });
    let writer = tokio::spawn(writer_loop(wr, out_rx, shared.clone(), auth.send_ms));
    let mut s = Session {
        conn: shared.clone(),
        config,
        subs: HashMap::new(),
        reply_queues: HashMap::new(),
        declared: HashSet::new(),
        txs: HashMap::new(),
        perm_ok: HashSet::new(),
        tx_settled: None,
    };
    let pending = (!explicit).then_some(first);
    let close = session_loop(&mut rd, &mut parser, &mut s, ctl_rx, pending, auth.recv_ms).await;
    s.release().await;
    shared.send(Out::Close);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    broker.connection_closed(id).await;
    if close == Closed::ByServer {
        linger(&mut rd).await;
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Closed {
    ByPeer,
    ByServer,
}

async fn session_loop<R: AsyncRead + Unpin>(
    rd: &mut R,
    parser: &mut Parser,
    s: &mut Session,
    mut ctl: mpsc::UnboundedReceiver<Ctl>,
    mut pending: Option<Frame>,
    recv_ms: u64,
) -> Closed {
    let tolerance = (recv_ms > 0).then(|| Duration::from_millis(recv_ms * 2));
    let mut last_read = Instant::now();
    let info = s.conn.info.clone();
    loop {
        loop {
            let f = match pending.take() {
                Some(f) => f,
                None => match parser.next(Some(s.conn.version)) {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    Err(e) => {
                        s.fail(&err("Invalid frame", e.to_string()), None);
                        return Closed::ByServer;
                    }
                },
            };
            info.recv_cnt.fetch_add(1, Ordering::Relaxed);
            let receipt = f.header("receipt").map(String::from);
            match s.handle(f).await {
                Ok(Flow::Continue) => {}
                Ok(Flow::Stop) => return Closed::ByServer,
                Err(e) => {
                    s.fail(&e, receipt.as_deref());
                    return Closed::ByServer;
                }
            }
        }
        tokio::select! {
            biased;
            c = ctl.recv() => {
                let Some(c) = c else { return Closed::ByServer };
                if let Err(e) = s.on_ctl(c).await {
                    s.fail(&e, None);
                    return Closed::ByServer;
                }
            }
            _ = info.close.notified() => {
                let reason = info.close_reason.lock().clone().unwrap_or_else(|| "closed via management plugin".into());
                s.fail(&err("Connection forced", reason), None);
                return Closed::ByServer;
            }
            _ = tokio::time::sleep_until(last_read + tolerance.unwrap_or(Duration::ZERO)), if tolerance.is_some() => {
                warn!(conn = %info.name, "STOMP detected missed client heartbeats, timeout: {recv_ms} ms");
                return Closed::ByPeer;
            }
            r = rd.read_buf(&mut parser.buf) => {
                match r {
                    Ok(0) | Err(_) => return Closed::ByPeer,
                    Ok(n) => {
                        info.recv_oct.fetch_add(n as u64, Ordering::Relaxed);
                        last_read = Instant::now();
                    }
                }
            }
        }
    }
}

/// Outbound items, encoded by the writer task.
enum Out {
    Frame(Bytes),
    Message { sub: Arc<Sub>, dtag: u64, d: Delivery },
    /// Flush and close the socket.
    Close,
}

/// Events from consumer sinks to the session task.
enum Ctl {
    Cancelled(Arc<str>),
    TimedOut(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckMode {
    Auto,
    /// Cumulative.
    Client,
    ClientIndividual,
}

/// A subscription's state shared with its consumer sink and its pending acks.
struct Sub {
    tag: Arc<str>,
    /// `subscription` header of its MESSAGE frames (`id`-based subscriptions only).
    id: Option<String>,
    ack: AckMode,
    prefetch: u32,
    unacked: AtomicU32,
    cancelled: AtomicBool,
}

struct Pending {
    dtag: u64,
    sub: Arc<Sub>,
    queue: QueueRef,
    seq: u64,
}

#[derive(Default)]
struct Acks {
    next: u64,
    /// Unacknowledged deliveries in delivery order.
    pending: VecDeque<Pending>,
}

/// Per-connection state shared by the session, the writer and the consumer sinks.
struct Shared {
    broker: Arc<Broker>,
    vhost: Arc<VHost>,
    user: User,
    info: Arc<ConnectionInfo>,
    chan: Arc<ChannelInfo>,
    out: mpsc::UnboundedSender<Out>,
    ctl: mpsc::UnboundedSender<Ctl>,
    session: String,
    version: Version,
    trailing_lf: bool,
    /// Deliveries queued for the writer (socket backpressure for consumers).
    backlog: AtomicUsize,
    /// A sink was refused credit because of the backlog; wake queues once it drains.
    stalled: AtomicBool,
    closing: AtomicBool,
    watched: Mutex<Vec<Weak<QueueHandle>>>,
    acks: Mutex<Acks>,
}

impl Shared {
    fn send(&self, o: Out) {
        let _ = self.out.send(o);
    }

    fn frame(&self, command: &str, headers: &[(&str, &str)]) {
        self.send(Out::Frame(frame::encode(command, headers, b"", self.trailing_lf)));
    }

    fn watch(&self, q: &QueueRef) {
        let mut v = self.watched.lock();
        v.retain(|w| w.strong_count() > 0);
        if !v.iter().any(|w| w.as_ptr() == Arc::as_ptr(q)) {
            v.push(Arc::downgrade(q));
        }
    }

    fn wake_queues(&self) {
        let qs: Vec<QueueRef> = self.watched.lock().iter().filter_map(|w| w.upgrade()).collect();
        for q in qs {
            q.wake();
        }
    }
}

struct Sink {
    conn: Arc<Shared>,
    sub: Arc<Sub>,
}

impl ConsumerSink for Sink {
    fn has_credit(&self) -> bool {
        let (c, s) = (&self.conn, &self.sub);
        if s.cancelled.load(Ordering::Acquire) || c.closing.load(Ordering::Acquire) {
            return false;
        }
        if c.backlog.load(Ordering::Acquire) >= BACKLOG_HIGH {
            c.stalled.store(true, Ordering::Release);
            return false;
        }
        s.ack == AckMode::Auto || s.prefetch == 0 || s.unacked.load(Ordering::Acquire) < s.prefetch
    }

    fn deliver(&self, d: Delivery) {
        let (c, s) = (&self.conn, &self.sub);
        let dtag = {
            let mut a = c.acks.lock();
            let dtag = a.next;
            a.next += 1;
            if s.ack != AckMode::Auto {
                a.pending.push_back(Pending { dtag, sub: s.clone(), queue: d.queue.clone(), seq: d.seq });
            }
            dtag
        };
        if s.ack != AckMode::Auto {
            s.unacked.fetch_add(1, Ordering::AcqRel);
            c.chan.unacked.fetch_add(1, Ordering::Relaxed);
        }
        c.chan.delivered.fetch_add(1, Ordering::Relaxed);
        if d.redelivered {
            c.chan.redelivered.fetch_add(1, Ordering::Relaxed);
        }
        let vs = &c.vhost.stats;
        vs.delivered.fetch_add(1, Ordering::Relaxed);
        vs.delivered_bytes.fetch_add(d.msg.body.len() as u64, Ordering::Relaxed);
        c.backlog.fetch_add(1, Ordering::AcqRel);
        c.send(Out::Message { sub: s.clone(), dtag, d });
    }

    fn cancelled(&self, tag: &str) {
        if !self.sub.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.conn.ctl.send(Ctl::Cancelled(Arc::from(tag)));
        }
    }

    fn timed_out(&self, _tag: &str, timeout_ms: u64) {
        let _ = self.conn.ctl.send(Ctl::TimedOut(timeout_ms));
    }
}

struct Subscription {
    sub: Arc<Sub>,
    queue: QueueRef,
    dest: Dest,
    /// `id='…'` or `destination='…'`, for error texts.
    description: String,
    /// Value of the `subscription` header on server-cancel errors.
    header: String,
}

enum Flow {
    Continue,
    Stop,
}

/// A frame's consumer tag, like RabbitMQ: `T_<id>` or `Q_<destination>`.
enum Tag {
    Ok { tag: Arc<str>, description: String, header: String, id: Option<String> },
    InvalidPrefix,
    Missing,
}

fn consumer_tag(f: &Frame) -> Tag {
    if let Some(id) = f.header("id") {
        if id.starts_with("/temp-queue/") {
            return Tag::InvalidPrefix;
        }
        return Tag::Ok { tag: Arc::from(format!("T_{id}")), description: format!("id='{id}'"), header: id.into(), id: Some(id.into()) };
    }
    match f.header("destination") {
        Some(d) => Tag::Ok { tag: Arc::from(format!("Q_{d}")), description: format!("destination='{d}'"), header: d.into(), id: None },
        None => Tag::Missing,
    }
}

fn has_durable(f: &Frame) -> bool {
    f.bool_header("durable") == Some(true) || f.bool_header("persistent") == Some(true)
}

fn check_queue_name(name: &str) -> Res<String> {
    let n = dest::strip_cr_lf(name);
    if n.starts_with("amq.") {
        return Err(AmqpError::access_refused(format!("queue name '{n}' contains reserved prefix 'amq.*'")).into());
    }
    Ok(n)
}

fn invalid_header(h: &str) -> StompError {
    AmqpError::precondition(format!("invalid value for header '{h}'")).into()
}

/// Queue arguments taken from headers (RabbitMQ's list, plus a few common extensions).
fn queue_arguments(f: &Frame) -> Res<FieldTable> {
    const INTS: [&str; 9] = [
        "x-expires",
        "x-max-length",
        "x-max-length-bytes",
        "x-max-priority",
        "x-message-ttl",
        "x-stream-max-segment-size-bytes",
        "x-stream-filter-size-bytes",
        "x-delivery-limit",
        "x-initial-cluster-size",
    ];
    const STRS: [&str; 5] = ["x-dead-letter-exchange", "x-dead-letter-routing-key", "x-max-age", "x-queue-type", "x-overflow"];
    let mut t = FieldTable::new();
    for (k, v) in &f.headers {
        let k = k.as_str();
        if INTS.contains(&k) {
            t.insert(k, FieldValue::I64(v.trim().parse().map_err(|_| invalid_header(k))?));
        } else if STRS.contains(&k) {
            t.insert(k, v.trim());
        } else if k == "x-single-active-consumer" {
            t.insert(k, FieldValue::Bool(f.bool_header(k).ok_or_else(|| invalid_header(k))?));
        }
    }
    Ok(t)
}

/// Consumer arguments taken from SUBSCRIBE headers.
fn subscribe_arguments(f: &Frame) -> Res<FieldTable> {
    let mut t = FieldTable::new();
    if let Some(v) = f.header("x-stream-offset") {
        let off = match v {
            "first" | "last" | "next" => Some(FieldValue::str(v)),
            _ => match (v.strip_prefix("offset="), v.strip_prefix("timestamp=")) {
                (Some(n), _) => Some(FieldValue::I64(n.parse().map_err(|_| invalid_header("x-stream-offset"))?)),
                (_, Some(n)) => Some(FieldValue::Timestamp(n.parse::<i64>().map_err(|_| invalid_header("x-stream-offset"))?.max(0) as u64)),
                _ => None,
            },
        };
        if let Some(o) = off {
            t.insert("x-stream-offset", o);
        }
    }
    if let Some(v) = f.header("x-stream-filter") {
        t.insert("x-stream-filter", FieldValue::Array(v.split(',').map(FieldValue::str).collect()));
    }
    if let Some(b) = f.bool_header("x-stream-match-unfiltered") {
        t.insert("x-stream-match-unfiltered", FieldValue::Bool(b));
    }
    if f.has("x-priority") {
        t.insert("x-priority", FieldValue::I64(f.int_header("x-priority").ok_or_else(|| invalid_header("x-priority"))?));
    }
    Ok(t)
}

struct Session {
    conn: Arc<Shared>,
    config: Arc<Config>,
    subs: HashMap<Arc<str>, Subscription>,
    /// `/temp-queue/<id>` → the private queue behind it.
    reply_queues: HashMap<String, String>,
    /// `/queue/` destinations already declared on this connection.
    declared: HashSet<String>,
    txs: HashMap<String, Vec<Frame>>,
    perm_ok: HashSet<(u8, bool, String)>,
    /// Deliveries settled by cumulative acks during the running COMMIT.
    tx_settled: Option<HashSet<(Arc<str>, u64)>>,
}

impl Session {
    fn fail(&self, e: &StompError, receipt: Option<&str>) {
        debug!(conn = %self.conn.info.name, message = %e.message, detail = %e.detail, "STOMP error frame sent");
        let extra: Vec<(&str, &str)> = receipt.map(|r| ("receipt-id", r)).into_iter().collect();
        self.conn.send(Out::Frame(error_frame(e, &extra, self.conn.trailing_lf)));
    }

    fn conn_id(&self) -> Option<u64> {
        Some(self.conn.info.id)
    }

    fn check(&mut self, access: Access, queue: bool, name: &str) -> Res<()> {
        let key = (access as u8, queue, name.to_string());
        if self.perm_ok.contains(&key) {
            return Ok(());
        }
        let kind = if queue { "queue" } else { "exchange" };
        self.conn.broker.check(&self.conn.user, &self.conn.vhost.name, access, kind, name)?;
        if self.perm_ok.len() > 1024 {
            self.perm_ok.clear();
        }
        self.perm_ok.insert(key);
        Ok(())
    }

    fn check_topic(&self, exchange: &str, routing_key: &str, access: Access) -> Res<()> {
        let vh = &self.conn.vhost;
        if let Some(x) = vh.exchange(exchange)
            && x.kind == ExchangeKind::Topic
        {
            self.conn.broker.check_topic(&self.conn.user, &vh.name, exchange, routing_key, access, None)?;
        }
        Ok(())
    }

    async fn handle(&mut self, f: Frame) -> Res<Flow> {
        let mut flow = Flow::Continue;
        match f.command.as_str() {
            "DISCONNECT" => flow = Flow::Stop,
            "SUBSCRIBE" => self.subscribe(&f).await?,
            "UNSUBSCRIBE" => self.unsubscribe(&f).await?,
            "SEND" | "ACK" | "NACK" => match f.header("transaction") {
                Some(tx) => {
                    let Some(actions) = self.txs.get_mut(tx) else {
                        return Err(err("Bad transaction", format!("Invalid transaction identifier: {}\n", tp(tx))));
                    };
                    if actions.len() >= MAX_TRANSACTION_ACTIONS {
                        return Err(err("Transaction too large", format!("Too many actions in transaction (limit {MAX_TRANSACTION_ACTIONS})")));
                    }
                    actions.push(f.clone());
                }
                None => {
                    self.action(&f, None).await?;
                }
            },
            "BEGIN" => {
                let tx = tx_header(&f, "BEGIN")?;
                if self.txs.len() >= MAX_TRANSACTIONS {
                    return Err(err("Transaction limit exceeded", format!("Too many concurrent transactions (limit {MAX_TRANSACTIONS})")));
                }
                self.txs.insert(tx, Vec::new());
                self.conn.chan.transactional.store(true, Ordering::Relaxed);
            }
            "COMMIT" => {
                let tx = tx_header(&f, "COMMIT")?;
                let actions = self.txs.remove(&tx).ok_or_else(|| err("Bad transaction", format!("Invalid transaction identifier: {}\n", tp(&tx))))?;
                self.commit(actions).await?;
            }
            "ABORT" => {
                let tx = tx_header(&f, "ABORT")?;
                self.txs.remove(&tx).ok_or_else(|| err("Bad transaction", format!("Invalid transaction identifier: {}\n", tp(&tx))))?;
            }
            "CONNECT" | "STOMP" => return Err(err("Bad CONNECT", "Already connected")),
            other => return Err(err("Bad command", format!("Could not interpret command \"{other}\"\n"))),
        }
        // SEND receipts go out once the message is durable.
        if f.command != "SEND"
            && let Some(r) = f.header("receipt")
        {
            self.conn.frame("RECEIPT", &[("receipt-id", r)]);
        }
        Ok(flow)
    }

    /// SEND, ACK or NACK, now (outside a transaction or at COMMIT).
    async fn action(&mut self, f: &Frame, waits: Option<&mut Vec<oneshot::Receiver<bool>>>) -> Res<()> {
        match f.command.as_str() {
            "SEND" => self.send(f, waits).await,
            "ACK" => self.ack(f, false).await,
            _ => self.ack(f, true).await,
        }
    }

    async fn commit(&mut self, actions: Vec<Frame>) -> Res<()> {
        self.tx_settled = Some(HashSet::new());
        let mut waits = Vec::new();
        let mut result = Ok(());
        for f in &actions {
            result = self.action(f, Some(&mut waits)).await;
            if result.is_err() {
                break;
            }
        }
        self.tx_settled = None;
        result?;
        // The COMMIT receipt means every message in the transaction is durable.
        for w in waits {
            let _ = w.await;
        }
        Ok(())
    }

    async fn on_ctl(&mut self, c: Ctl) -> Res<()> {
        match c {
            Ctl::Cancelled(tag) => {
                if let Some(s) = self.subs.remove(&tag) {
                    self.conn.chan.consumers.fetch_sub(1, Ordering::Relaxed);
                    let e = err(
                        "Server cancelled subscription",
                        format!("The server has canceled a subscription.\nNo more messages will be delivered for {}.\n", tp(&s.description)),
                    );
                    // Informational: the connection stays open, as with RabbitMQ.
                    self.conn.send(Out::Frame(error_frame(&e, &[("subscription", &s.header)], self.conn.trailing_lf)));
                }
                Ok(())
            }
            Ctl::TimedOut(ms) => Err(err(
                "Consumer timeout",
                format!("delivery acknowledgement timed out. Timeout value used: {ms} ms. This timeout value can be configured, see consumers doc guide to learn more"),
            )),
        }
    }

    fn destination(&self, f: &Frame, command: &str) -> Res<(Dest, String)> {
        let Some(h) = f.header("destination") else {
            return Err(err("Missing destination", format!("'{command}' must include a 'destination' header\n")));
        };
        match dest::parse(h) {
            Ok(d) => Ok((d, h.to_string())),
            Err(DestError::Unknown(c)) => {
                Err(err("Unknown destination", format!("'{c}' is not a valid destination.\nValid destination types are: {}.\n", dest::VALID_PREFIXES)))
            }
            Err(DestError::Invalid { kind, content }) => Err(err("Invalid destination", format!("'{content}' is not a valid {kind} destination\n"))),
        }
    }

    /// Declare (or check) a queue with STOMP's defaults for the destination and the frame's
    /// `durable`/`auto-delete`/`exclusive` and `x-*` headers. Returns whether it was created.
    async fn declare(&mut self, name: &str, d: &Dest, f: &Frame) -> Res<bool> {
        let vh = self.conn.vhost.clone();
        let (def_durable, def_exclusive, def_auto_delete) = match d {
            Dest::Queue(_) => (Some(true), None, None),
            Dest::Exchange { .. } => (None, Some(true), Some(true)),
            Dest::Topic(_) => (None, Some(false), Some(true)),
            _ => (Some(false), None, None),
        };
        // `persistent` is an alias of `durable` for queue declarations.
        let key = if f.has("durable") { "durable" } else { "persistent" };
        let durable = match (f.has(key), f.bool_header(key)) {
            (true, Some(b)) => b,
            (true, None) => return Err(AmqpError::precondition(format!("invalid value for 'durable' for queue '{name}' in vhost '{}'", vh.name)).into()),
            (false, _) => def_durable.unwrap_or(false),
        };
        let auto_delete = f.bool_header("auto-delete").or(def_auto_delete);
        let exclusive = f.bool_header("exclusive").or(def_exclusive);
        let args = queue_arguments(f)?;
        let (mut durable, mut exclusive, auto_delete) =
            if durable { (true, exclusive.unwrap_or(false), auto_delete.unwrap_or(false)) } else { (false, true, auto_delete.unwrap_or(true)) };
        if matches!(args.get_str("x-queue-type"), Some("quorum" | "stream")) {
            (durable, exclusive) = (true, false);
        } else {
            durable = durable && !exclusive;
        }
        self.check(Access::Configure, true, name)?;
        let created = vh.queue(name).is_none();
        if created && let Some(dlx) = args.get_str("x-dead-letter-exchange") {
            let dlx = dlx.to_string();
            self.check(Access::Read, true, name)?;
            self.check(Access::Write, false, &dlx)?;
        }
        vh.declare_queue(DeclareQueue { name: name.into(), durable, exclusive_owner: exclusive.then_some(self.conn.info.id), auto_delete, args, passive: false }).await?;
        Ok(created)
    }

    /// The queue behind a destination, declared as needed: `(name, created)`. `None` for
    /// destinations that publish straight to an exchange.
    async fn ensure_endpoint(&mut self, source: bool, d: &Dest, dest_hdr: &str, command: &str, f: &Frame) -> Res<Option<(String, bool)>> {
        match d {
            Dest::Queue(n) if n.is_empty() => Err(err("Invalid destination", "Destination cannot be blank")),
            Dest::Queue(n) => {
                if self.declared.contains(n) {
                    return Ok(Some((n.clone(), false)));
                }
                let name = check_queue_name(n)?;
                let created = self.declare(&name, d, f).await?;
                self.declared.insert(name.clone());
                Ok(Some((name, created)))
            }
            Dest::Exchange { .. } | Dest::Topic(_) if source => {
                let (_, key) = d.routing(&self.config.default_topic_exchange);
                let id = has_durable(f).then(|| f.header("id")).flatten();
                let name = check_queue_name(&dest::subscription_queue_name(&key, id, f.header("x-queue-name")))?;
                let created = self.declare(&name, d, f).await?;
                Ok(Some((name, created)))
            }
            Dest::Exchange { .. } | Dest::Topic(_) => Ok(None),
            Dest::AmqQueue(n) | Dest::ReplyQueue(n) => Ok(Some((n.clone(), false))),
            Dest::TempQueue(_) => Err(err("Invalid destination", format!("'{dest_hdr}' is not a valid destination for '{command}'\n"))),
        }
    }

    async fn delete_queue(&self, name: &str) {
        let _ = self.conn.vhost.delete_queue(name, false, false, self.conn_id()).await;
    }

    async fn subscribe(&mut self, f: &Frame) -> Res<()> {
        let tag = consumer_tag(f);
        if let Tag::InvalidPrefix = tag {
            return Err(err("Invalid id", "SUBSCRIBE 'id' may not start with /temp-queue/\n"));
        }
        let durable = has_durable(f);
        if durable && f.header("x-queue-name").is_some_and(|n| dest::strip_cr_lf(n).trim().is_empty()) {
            return Err(err("Invalid x-queue-name", "Header 'x-queue-name' may not be empty for durable subscriptions"));
        }
        if durable && !f.has("id") {
            return Err(err("Missing Header", "Header 'id' is required for durable subscriptions"));
        }
        let (d, dest_hdr) = self.destination(f, "SUBSCRIBE")?;
        let Tag::Ok { tag, description, header, id } = tag else {
            return Err(err("Missing destination", "'SUBSCRIBE' must include a 'destination' header\n"));
        };
        let (exchange, key) = d.routing(&self.config.default_topic_exchange);
        if let Dest::Topic(_) = d {
            self.check_topic(&exchange, &key, Access::Read)?;
        }
        let prefetch = f.int_header("prefetch-count").unwrap_or(0).clamp(0, u32::MAX as i64) as u32;
        let ack = match f.header("ack").unwrap_or("auto") {
            "auto" => AckMode::Auto,
            "client" => AckMode::Client,
            "client-individual" => AckMode::ClientIndividual,
            other => return Err(err("Invalid header", format!("'{other}' is not a valid ack mode"))),
        };
        let args = subscribe_arguments(f)?;
        if self.subs.contains_key(&tag) {
            return Err(err("Duplicated subscription identifier", format!("A subscription identified by '{tag}' already exists.")));
        }
        let Some((qname, created)) = self.ensure_endpoint(true, &d, &dest_hdr, "SUBSCRIBE", f).await? else {
            return Err(err("Invalid destination", format!("'{dest_hdr}' is not a valid destination for 'SUBSCRIBE'\n")));
        };
        let r = self.bind_and_consume(&qname, &exchange, &key, tag.clone(), id, ack, prefetch, args).await;
        let (sub, queue) = match r {
            Ok(x) => x,
            Err(e) => {
                if created {
                    self.delete_queue(&qname).await;
                }
                return Err(e);
            }
        };
        self.subs.insert(tag, Subscription { sub, queue, dest: d, description, header });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn bind_and_consume(
        &mut self,
        qname: &str,
        exchange: &str,
        key: &str,
        tag: Arc<str>,
        id: Option<String>,
        ack: AckMode,
        prefetch: u32,
        args: FieldTable,
    ) -> Res<(Arc<Sub>, QueueRef)> {
        let vh = self.conn.vhost.clone();
        if !(exchange.is_empty() && key == qname) {
            self.check(Access::Write, true, qname)?;
            self.check(Access::Read, false, exchange)?;
            self.check_topic(exchange, key, Access::Read)?;
            vh.bind(exchange, BindDest::queue(qname), key, FieldTable::new(), self.conn_id()).await?;
        }
        self.consume(qname, tag, id, ack, prefetch, args).await
    }

    async fn consume(&mut self, qname: &str, tag: Arc<str>, id: Option<String>, ack: AckMode, prefetch: u32, args: FieldTable) -> Res<(Arc<Sub>, QueueRef)> {
        self.check(Access::Read, true, qname)?;
        let q = self.conn.vhost.queue_for(qname, self.conn_id())?;
        let sub = Arc::new(Sub { tag: tag.clone(), id, ack, prefetch, unacked: AtomicU32::new(0), cancelled: AtomicBool::new(false) });
        let priority = args.get_i64("x-priority").unwrap_or(0).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let spec = ConsumerSpec {
            tag,
            channel: self.conn.chan.id,
            conn: self.conn.info.id,
            no_ack: ack == AckMode::Auto,
            exclusive: false,
            priority,
            args,
            sink: Arc::new(Sink { conn: self.conn.clone(), sub: sub.clone() }),
        };
        q.consume(spec).await?;
        self.conn.watch(&q);
        q.wake();
        self.conn.chan.consumers.fetch_add(1, Ordering::Relaxed);
        if prefetch > 0 {
            self.conn.chan.prefetch_count.store(prefetch, Ordering::Relaxed);
        }
        Ok((sub, q))
    }

    async fn unsubscribe(&mut self, f: &Frame) -> Res<()> {
        if has_durable(f) && !f.has("id") {
            return Err(err("Missing Header", "Header 'id' is required for durable subscriptions"));
        }
        let tag = match consumer_tag(f) {
            Tag::Ok { tag, description, .. } => (tag, description),
            Tag::InvalidPrefix => return Err(err("Invalid id", "UNSUBSCRIBE 'id' may not start with /temp-queue/\n")),
            Tag::Missing => return Err(err("Missing destination or id", "UNSUBSCRIBE must include a 'destination' or 'id' header")),
        };
        let Some(s) = self.subs.remove(&tag.0) else {
            return Err(err(
                "No subscription found",
                format!("UNSUBSCRIBE must refer to an existing subscription.\nSubscription to {} not found.\n", tp(&tag.1)),
            ));
        };
        s.sub.cancelled.store(true, Ordering::Release);
        s.queue.cancel(self.conn.chan.id, tag.0).await;
        self.conn.chan.consumers.fetch_sub(1, Ordering::Relaxed);
        if matches!(s.dest, Dest::Topic(_)) && has_durable(f) {
            let name = s.queue.meta.name.clone();
            self.conn.broker.check(&self.conn.user, &self.conn.vhost.name, Access::Configure, "queue", &name)?;
            self.delete_queue(&name).await;
        }
        Ok(())
    }

    /// The private queue behind `/temp-queue/<id>`, created and consumed on first use.
    async fn reply_queue(&mut self, temp_id: &str) -> Res<String> {
        if let Some(q) = self.reply_queues.get(temp_id) {
            return Ok(q.clone());
        }
        let vh = self.conn.vhost.clone();
        let ok = vh
            .declare_queue(DeclareQueue { name: String::new(), durable: false, exclusive_owner: self.conn_id(), auto_delete: true, args: FieldTable::new(), passive: false })
            .await?;
        let name = ok.name;
        let header = format!("/temp-queue/{temp_id}");
        let tag: Arc<str> = Arc::from(format!("T_{header}"));
        let r = match self.check(Access::Configure, true, &name) {
            Ok(()) => self.consume(&name, tag.clone(), Some(header.clone()), AckMode::Auto, 0, FieldTable::new()).await,
            Err(e) => Err(e),
        };
        let (sub, queue) = match r {
            Ok(x) => x,
            Err(e) => {
                self.delete_queue(&name).await;
                return Err(e);
            }
        };
        let description = format!("id='{header}'");
        self.subs.insert(tag, Subscription { sub, queue, dest: Dest::Queue(name.clone()), description, header });
        self.reply_queues.insert(temp_id.to_string(), name.clone());
        Ok(name)
    }

    async fn send(&mut self, f: &Frame, waits: Option<&mut Vec<oneshot::Receiver<bool>>>) -> Res<()> {
        if f.has("message-id") {
            return Err(err("Invalid header", "'message-id' is not allowed on 'SEND'.\n"));
        }
        let (d, dest_hdr) = self.destination(f, "SEND")?;
        self.ensure_endpoint(false, &d, &dest_hdr, "SEND", f).await?;
        let mut props = props::to_properties(f);
        if let Some(Ok(Dest::TempQueue(id))) = f.header("reply-to").map(dest::parse) {
            props.reply_to = Some(self.reply_queue(&id).await?);
        }
        let (exchange, key) = d.routing(&self.config.default_topic_exchange);
        self.check(Access::Write, false, &exchange)?;
        self.check_topic(&exchange, &key, Access::Write)?;
        let user = &self.conn.user;
        if let Some(uid) = &props.user_id
            && *uid != user.name
            && !user.has_tag("impersonator")
        {
            return Err(AmqpError::access_refused(format!("user_id property set to '{uid}' but authenticated user was '{}'", user.name)).into());
        }
        let max = self.conn.broker.config.max_message_size;
        if f.body.len() as u64 > max {
            return Err(AmqpError::precondition(format!("message size {} is larger than configured max size {max}", f.body.len())).into());
        }
        self.block_while_alarm().await;
        let msg = Message::new(exchange.clone(), key.clone(), props, f.body.clone());
        let receipt = f.header("receipt").map(|r| frame::encode("RECEIPT", &[("receipt-id", r)], b"", self.conn.trailing_lf));
        let done = waits.map(|w| {
            let (tx, rx) = oneshot::channel();
            w.push(rx);
            tx
        });
        let ticket = (receipt.is_some() || done.is_some()).then(|| {
            let (out, chan) = (self.conn.out.clone(), self.conn.chan.clone());
            ConfirmTicket::new(move |ok| {
                chan.confirmed.fetch_add(1, Ordering::Relaxed);
                // A rejected message gets no receipt, like RabbitMQ.
                if ok && let Some(r) = receipt {
                    let _ = out.send(Out::Frame(r));
                }
                if let Some(d) = done {
                    let _ = d.send(ok);
                }
            })
        });
        self.conn.chan.published.fetch_add(1, Ordering::Relaxed);
        // Direct reply-to queues of AMQP clients live outside the vhost.
        if exchange.is_empty() && key.starts_with(mq_amqp::reply_to::PREFIX) {
            match mq_amqp::reply_to::lookup(&key) {
                Some(q) => q.publish(Arc::new(msg), ticket).await?,
                None => {
                    if let Some(t) = ticket {
                        t.settle(true);
                    }
                }
            }
            return Ok(());
        }
        let outcome = self.conn.vhost.publish(&exchange, msg, ticket).await?;
        if let Some(t) = outcome.confirm {
            t.settle(true);
        }
        Ok(())
    }

    async fn block_while_alarm(&self) {
        let broker = &self.conn.broker;
        while broker.alarm.load(Ordering::Acquire) {
            let n = broker.alarm_cleared.notified();
            if !broker.alarm.load(Ordering::Acquire) {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), n).await;
        }
    }

    async fn ack(&mut self, f: &Frame, nack: bool) -> Res<()> {
        let command = if nack { "NACK" } else { "ACK" };
        let hname = if self.conn.version == Version::V12 { "id" } else { "message-id" };
        let Some(v) = f.header(hname) else {
            return Err(err("Missing header", format!("'{command}' must include the {} header\n", tp(hname))));
        };
        let parsed = match v.split("@@").collect::<Vec<_>>()[..] {
            [ctag, session, dtag] if session == self.conn.session => dtag.parse::<u64>().ok().map(|d| (ctag.to_string(), d)),
            _ => None,
        };
        let Some((ctag, dtag)) = parsed else {
            return Err(err("Invalid header", format!("'{command}' must include a valid {} header\n", tp(hname))));
        };
        let requeue = f.bool_header("requeue").unwrap_or(self.config.default_nack_requeue);
        let outcome = match (nack, requeue) {
            (false, _) => Outcome::Ack,
            (true, true) => Outcome::Requeue,
            (true, false) => Outcome::Reject,
        };
        let taken: Vec<Pending> = {
            let mut a = self.conn.acks.lock();
            match a.pending.iter().position(|p| p.dtag == dtag) {
                Some(i) if *a.pending[i].sub.tag == *ctag => {
                    if a.pending[i].sub.ack == AckMode::Client {
                        a.pending.drain(..=i).collect()
                    } else {
                        a.pending.remove(i).into_iter().collect()
                    }
                }
                _ => Vec::new(),
            }
        };
        if taken.is_empty() {
            if self.tx_settled.as_ref().is_some_and(|s| s.contains(&(Arc::from(ctag.as_str()), dtag))) {
                return Ok(());
            }
            return Err(err("Message not found", format!("unknown delivery tag {dtag}\n")));
        }
        if let Some(set) = &mut self.tx_settled {
            set.extend(taken.iter().filter(|p| p.dtag != dtag).map(|p| (p.sub.tag.clone(), p.dtag)));
        }
        self.settle(taken, outcome).await;
        Ok(())
    }

    async fn settle(&self, taken: Vec<Pending>, outcome: Outcome) {
        let c = &self.conn;
        let n = taken.len() as u64;
        c.chan.unacked.fetch_sub(n.min(c.chan.unacked.load(Ordering::Relaxed)), Ordering::Relaxed);
        if outcome == Outcome::Ack {
            c.chan.acked.fetch_add(n, Ordering::Relaxed);
            c.vhost.stats.acked.fetch_add(n, Ordering::Relaxed);
        }
        let mut by_queue: Vec<(QueueRef, Vec<u64>)> = Vec::new();
        for p in taken {
            p.sub.unacked.fetch_sub(1, Ordering::AcqRel);
            match by_queue.iter_mut().find(|(q, _)| Arc::ptr_eq(q, &p.queue)) {
                Some((_, v)) => v.push(p.seq),
                None => by_queue.push((p.queue, vec![p.seq])),
            }
        }
        for (q, seqs) in by_queue {
            q.settle(c.chan.id, seqs, outcome).await;
            // Streams don't track acks; credit is ours, so nudge them either way.
            q.wake();
        }
    }

    /// Requeue everything this connection holds and drop its consumers.
    async fn release(&mut self) {
        self.conn.closing.store(true, Ordering::Release);
        for s in self.subs.values() {
            s.sub.cancelled.store(true, Ordering::Release);
        }
        self.subs.clear();
        self.conn.acks.lock().pending.clear();
        let queues: Vec<QueueRef> = self.conn.watched.lock().iter().filter_map(|w| w.upgrade()).collect();
        for q in queues {
            q.channel_closed(self.conn.chan.id).await;
        }
        self.conn.broker.channel_closed(self.conn.chan.id);
    }
}

fn tx_header(f: &Frame, command: &str) -> Res<String> {
    f.header("transaction").map(String::from).ok_or_else(|| err("Missing transaction", format!("'{command}' must include a 'transaction' header\n")))
}

/// Encode a MESSAGE frame. Server-generated headers win over the message's own, which win
/// over its AMQP headers.
fn encode_message(buf: &mut BytesMut, conn: &Shared, sub: &Sub, dtag: u64, d: &Delivery) {
    let message_id = format!("{}@@{}@@{dtag}", sub.tag, conn.session);
    let extra: Vec<(&str, FieldValue)> = match d.queue.kind {
        QueueType::Stream => vec![("x-stream-offset", FieldValue::I64(d.seq as i64))],
        QueueType::Quorum if d.delivery_count > 0 => vec![("x-delivery-count", FieldValue::I64(d.delivery_count as i64))],
        _ => vec![],
    };
    let mut h = props::message_headers(&d.msg.props, &extra);
    if let Some(id) = &sub.id {
        h.set("subscription", id.clone());
    }
    h.set("destination", dest::format(&d.msg.exchange, &d.msg.routing_key));
    h.set("message-id", message_id.clone());
    h.set("redelivered", d.redelivered.to_string());
    if sub.ack != AckMode::Auto && conn.version == Version::V12 {
        h.set("ack", message_id);
    }
    let mut e = Encoder::new(buf, "MESSAGE");
    for (k, v) in h.0.iter().filter(|(k, _)| k != "content-length") {
        e.header(k, v);
    }
    e.body(&d.msg.body, conn.trailing_lf);
}

async fn writer_loop<W: AsyncWrite + Unpin>(mut wr: W, mut rx: mpsc::UnboundedReceiver<Out>, conn: Arc<Shared>, send_ms: u64) {
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let hb = if send_ms > 0 { Duration::from_millis(send_ms) } else { Duration::from_secs(3600) };
    'outer: loop {
        let first = match tokio::time::timeout(hb, rx.recv()).await {
            Ok(Some(o)) => o,
            Ok(None) => break,
            Err(_) => {
                if send_ms > 0 {
                    if wr.write_all(b"\n").await.is_err() || wr.flush().await.is_err() {
                        break;
                    }
                    conn.info.send_oct.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
        };
        let mut item = Some(first);
        let (mut delivered, mut frames) = (0usize, 0u64);
        let mut closing = false;
        while let Some(o) = item.take() {
            match o {
                Out::Close => {
                    closing = true;
                    break;
                }
                Out::Frame(b) => buf.extend_from_slice(&b),
                Out::Message { sub, dtag, d } => {
                    delivered += 1;
                    encode_message(&mut buf, &conn, &sub, dtag, &d);
                }
            }
            frames += 1;
            if buf.len() < 512 * 1024 {
                item = rx.try_recv().ok();
            }
        }
        conn.info.send_oct.fetch_add(buf.len() as u64, Ordering::Relaxed);
        conn.info.send_cnt.fetch_add(frames, Ordering::Relaxed);
        if wr.write_all(&buf).await.is_err() || wr.flush().await.is_err() {
            break;
        }
        buf.clear();
        if buf.capacity() > 4 << 20 {
            buf = BytesMut::with_capacity(64 * 1024);
        }
        if delivered > 0 {
            let left = conn.backlog.fetch_sub(delivered, Ordering::AcqRel).saturating_sub(delivered);
            if left < BACKLOG_LOW && conn.stalled.swap(false, Ordering::AcqRel) {
                conn.wake_queues();
            }
        }
        if closing {
            break 'outer;
        }
    }
    let _ = wr.flush().await;
    let _ = wr.shutdown().await;
}

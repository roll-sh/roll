//! One MQTT connection: CONNECT (credentials, vhost, client-id takeover, session resume), then
//! the session loop — publishes with confirms, subscriptions as bindings of the session queues
//! (`mqtt-subscription-<client id>qos0|qos1`), deliveries with QoS 1 credit, keep-alive and
//! will messages — following RabbitMQ 4's MQTT plugin.

use crate::convert::{self, H_RETAIN};
use crate::packet::{self, DecodeError, Packet, Props, Publish, SubOpts, V5, V31, V311, rc};
use crate::state::{self, BrokerState, Retained, SessionCtl};
use crate::topic;
use crate::{Config, Stream};
use anyhow::bail;
use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use mq_core::auth::{Access, User};
use mq_core::broker::{ChannelInfo, ConnectionInfo};
use mq_core::error::{ACCESS_REFUSED, PRECONDITION_FAILED};
use mq_core::exchange::Dest;
use mq_core::queue::{ConfirmTicket, ConsumerSink, ConsumerSpec, Delivery, Outcome, QueueHandle, QueueRef, QueueType};
use mq_core::value::{FieldTable, FieldValue};
use mq_core::vhost::DeclareQueue;
use mq_core::{AmqpError, Broker, VHost};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const CONSUMER_TAG: &str = "mqtt";
const BACKLOG_HIGH: usize = 4096;
const BACKLOG_LOW: usize = 1024;
/// Margin added to a delayed will's queue expiry so the message dead-letters first.
const WILL_QUEUE_EXPIRY_MARGIN_MS: i64 = 1_000;

/// `mqtt-subscription-<client id>qos<n>`, RabbitMQ's session queue names.
pub fn queue_name(client_id: &str, qos: u8) -> String {
    format!("mqtt-subscription-{client_id}qos{qos}")
}

/// `mqtt-will-<client id>`: holds a delayed will message until it dead-letters.
pub fn will_queue_name(client_id: &str) -> String {
    format!("mqtt-will-{client_id}")
}

// ── socket I/O ──

enum ReadError {
    Closed,
    Io(std::io::Error),
    Decode(DecodeError),
}

struct Reader<R> {
    rd: R,
    buf: BytesMut,
    info: Option<Arc<ConnectionInfo>>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    /// The next packet. Cancel-safe: partial input stays buffered.
    async fn next(&mut self, version: u8, max: u32) -> Result<Packet, ReadError> {
        loop {
            if let Some(p) = packet::decode(&mut self.buf, version, max).map_err(ReadError::Decode)? {
                if let Some(i) = &self.info {
                    i.recv_cnt.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(p);
            }
            let n = self.rd.read_buf(&mut self.buf).await.map_err(ReadError::Io)?;
            if n == 0 {
                return Err(ReadError::Closed);
            }
            if let Some(i) = &self.info {
                i.recv_oct.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
    }
}

enum Out {
    /// Encoded packet; `true` for deliveries (socket backpressure accounting).
    Data(Bytes, bool),
    Close,
}

/// State shared by the session task, its writer and its consumer sinks.
struct Shared {
    info: Arc<ConnectionInfo>,
    events: mpsc::UnboundedSender<Ev>,
    /// Deliveries handed over but not yet on the wire.
    backlog: AtomicUsize,
    stalled: AtomicBool,
    /// QoS 1 deliveries awaiting PUBACK.
    inflight: AtomicU32,
    prefetch: u32,
    closing: AtomicBool,
    queues: Mutex<Vec<Weak<QueueHandle>>>,
}

impl Shared {
    fn wake_queues(&self) {
        for q in self.queues.lock().iter().filter_map(|w| w.upgrade()) {
            q.wake();
        }
    }

    fn release_backlog(&self, n: usize) {
        let left = self.backlog.fetch_sub(n, Ordering::AcqRel).saturating_sub(n);
        if left < BACKLOG_LOW && self.stalled.swap(false, Ordering::AcqRel) {
            self.wake_queues();
        }
    }
}

enum Ev {
    Deliver(Box<Delivery>, bool),
    Confirmed(u16, bool),
    Cancelled,
    TimedOut,
}

struct Sink {
    shared: Arc<Shared>,
    no_ack: bool,
}

impl ConsumerSink for Sink {
    fn has_credit(&self) -> bool {
        let s = &self.shared;
        if s.closing.load(Ordering::Acquire) {
            return false;
        }
        if s.backlog.load(Ordering::Acquire) >= BACKLOG_HIGH {
            s.stalled.store(true, Ordering::Release);
            return false;
        }
        self.no_ack || s.inflight.load(Ordering::Acquire) < s.prefetch
    }

    fn deliver(&self, d: Delivery) {
        let s = &self.shared;
        if !self.no_ack {
            s.inflight.fetch_add(1, Ordering::AcqRel);
        }
        s.backlog.fetch_add(1, Ordering::AcqRel);
        let _ = s.events.send(Ev::Deliver(Box::new(d), self.no_ack));
    }

    fn cancelled(&self, _tag: &str) {
        if !self.shared.closing.load(Ordering::Acquire) {
            let _ = self.shared.events.send(Ev::Cancelled);
        }
    }

    fn timed_out(&self, _tag: &str, _timeout_ms: u64) {
        let _ = self.shared.events.send(Ev::TimedOut);
    }
}

async fn writer_loop<W: AsyncWrite + Unpin>(mut wr: W, mut rx: mpsc::UnboundedReceiver<Out>, shared: Arc<Shared>) {
    let mut buf = BytesMut::with_capacity(64 * 1024);
    'outer: while let Some(first) = rx.recv().await {
        let mut item = Some(first);
        let mut delivered = 0usize;
        let mut frames = 0u64;
        let mut close = false;
        while let Some(o) = item.take() {
            match o {
                Out::Close => {
                    close = true;
                    break;
                }
                Out::Data(b, d) => {
                    buf.extend_from_slice(&b);
                    frames += 1;
                    delivered += d as usize;
                }
            }
            if buf.len() < 512 * 1024 {
                item = rx.try_recv().ok();
            }
        }
        shared.info.send_oct.fetch_add(buf.len() as u64, Ordering::Relaxed);
        shared.info.send_cnt.fetch_add(frames, Ordering::Relaxed);
        if !buf.is_empty() && wr.write_all(&buf).await.is_err() {
            break;
        }
        buf.clear();
        if delivered > 0 {
            shared.release_backlog(delivered);
        }
        if close {
            break 'outer;
        }
    }
    let _ = wr.flush().await;
    let _ = wr.shutdown().await;
}

// ── connect ──

async fn write_raw<W: AsyncWrite + Unpin>(wr: &mut W, p: &Packet, version: u8) {
    let _ = wr.write_all(&p.to_bytes(version)).await;
    let _ = wr.flush().await;
}

fn connack(version: u8, session_present: bool, code: u8, props: Props) -> Packet {
    let code = if version == V5 { code } else { rc::connack_v3(code) };
    Packet::ConnAck { session_present, code, props }
}

fn protocol_name(version: u8) -> &'static str {
    match version {
        V31 => "MQTT 3.1",
        V311 => "MQTT 3.1.1",
        _ => "MQTT 5.0",
    }
}

fn generate_client_id() -> String {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&mq_core::rand_u64().to_le_bytes());
    b[8..].copy_from_slice(&mq_core::rand_u64().to_le_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The vhost and user name for a CONNECT username, like RabbitMQ: `vhost:user` (split at the
/// last colon), else the listener port mapping, else the configured or broker default.
fn pick_vhost(broker: &Broker, cfg: &Config, username: &str, local: SocketAddr) -> (String, String) {
    if !cfg.ignore_colons_in_username
        && let Some((vh, user)) = username.rsplit_once(':')
    {
        return (vh.to_string(), user.to_string());
    }
    let port = local.port().to_string();
    let mapped = broker
        .global_parameters()
        .into_iter()
        .find(|(k, _)| k == "mqtt_port_to_vhost_mapping")
        .and_then(|(_, v)| v.get(&port).and_then(|x| x.as_str()).map(String::from));
    let vh = mapped.or_else(|| cfg.default_vhost.clone()).unwrap_or_else(|| broker.config.default_vhost.clone());
    (vh, username.to_string())
}

/// Serve one MQTT connection over an established transport.
pub(crate) async fn run<S: Stream>(broker: Arc<Broker>, stream: S, peer: SocketAddr, local: SocketAddr, tls: bool, cfg: Arc<Config>) -> anyhow::Result<()> {
    let (rd, mut wr) = tokio::io::split(stream);
    let mut reader = Reader { rd, buf: BytesMut::with_capacity(8 * 1024), info: None };
    let first = match tokio::time::timeout(cfg.login_timeout, reader.next(0, cfg.max_packet_size_unauthenticated)).await {
        Err(_) => bail!("login timeout"),
        Ok(Ok(Packet::Connect(c))) => *c,
        Ok(Ok(_)) => bail!("expected CONNECT"),
        Ok(Err(ReadError::Decode(DecodeError::UnsupportedVersion(v)))) => {
            write_raw(&mut wr, &Packet::ConnAck { session_present: false, code: 1, props: Props::default() }, V311).await;
            bail!("unsupported MQTT protocol version {v}");
        }
        Ok(Err(ReadError::Decode(e))) => bail!("{e}"),
        Ok(Err(ReadError::Io(e))) => return Err(e.into()),
        Ok(Err(ReadError::Closed)) => return Ok(()),
    };
    let version = first.version;
    let refuse = async |wr: &mut tokio::io::WriteHalf<S>, code: u8, why: String| -> anyhow::Result<()> {
        warn!(%peer, code, "MQTT connection refused: {why}");
        write_raw(wr, &connack(version, false, code, Props::default()), version).await;
        let _ = wr.shutdown().await;
        Ok(())
    };
    if broker.draining.load(Ordering::Relaxed) {
        return refuse(&mut wr, rc::SERVER_UNAVAILABLE, "broker is draining".into()).await;
    }
    let mut c = first;
    if let Some(m) = &c.props.auth_method {
        return refuse(&mut wr, rc::BAD_AUTHENTICATION_METHOD, format!("extended authentication (method {m}) is not supported")).await;
    }
    let assigned = c.client_id.is_empty();
    if assigned {
        if !c.clean_start && version < V5 {
            return refuse(&mut wr, rc::CLIENT_ID_NOT_VALID, "client ID must be provided for non-clean session".into()).await;
        }
        c.client_id = generate_client_id();
    }
    // Credentials: both, or anonymous (RabbitMQ's `mqtt.allow_anonymous`).
    let password = c.password.as_ref().map(|p| String::from_utf8_lossy(p).into_owned());
    let (username, password) = match (&c.username, password) {
        (Some(u), Some(p)) if !p.is_empty() => (u.clone(), p),
        (None, None) if cfg.allow_anonymous => (broker.config.default_user.clone(), broker.config.default_pass.clone()),
        _ => return refuse(&mut wr, rc::BAD_USERNAME_OR_PASSWORD, "no or incomplete credentials provided".into()).await,
    };
    let (vhost_name, username) = pick_vhost(&broker, &cfg, &username, local);
    if !broker.vhost_exists(&vhost_name) {
        return refuse(&mut wr, rc::BAD_USERNAME_OR_PASSWORD, format!("virtual host '{vhost_name}' does not exist")).await;
    }
    let user = match broker.authenticate(&username, &password, Some(peer)) {
        Ok(u) => u,
        Err(e) if e.text.contains("localhost") => return refuse(&mut wr, rc::NOT_AUTHORIZED, e.text).await,
        Err(e) => return refuse(&mut wr, rc::BAD_USERNAME_OR_PASSWORD, format!("access refused for user '{username}': {}", e.text)).await,
    };
    if let Err(e) = broker.check_connection_limits(&user, &vhost_name) {
        return refuse(&mut wr, rc::QUOTA_EXCEEDED, e.text).await;
    }
    if let Err(e) = broker.check_vhost_access(&user, &vhost_name) {
        return refuse(&mut wr, rc::NOT_AUTHORIZED, e.text).await;
    }
    let Some(vhost) = broker.open_vhost(&vhost_name).await else {
        return refuse(&mut wr, rc::NOT_AUTHORIZED, format!("vhost '{vhost_name}' is down")).await;
    };
    if let Some(w) = &mut c.will {
        if version == V5 && w.qos == 2 {
            return refuse(&mut wr, rc::QOS_NOT_SUPPORTED, "will QoS 2".into()).await;
        }
        if !topic::valid_name(&w.topic) {
            return refuse(&mut wr, rc::TOPIC_NAME_INVALID, format!("invalid will topic '{}'", w.topic)).await;
        }
        w.qos = w.qos.min(1);
    }
    let max_expiry = cfg.max_session_expiry_interval_secs;
    let session_expiry = match (version, c.clean_start) {
        (V5, _) => match c.props.session_expiry.unwrap_or(0) {
            u32::MAX => max_expiry,
            s => s.min(max_expiry),
        },
        (_, true) => 0,
        (_, false) => max_expiry,
    };

    let id = broker.next_id();
    let mut client_properties = serde_json::json!({ "client_id": c.client_id });
    if !c.props.user.is_empty() {
        client_properties["user_property"] = serde_json::Value::Object(c.props.user.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect());
    }
    let info = Arc::new(ConnectionInfo {
        id,
        name: format!("{peer} -> {local}"),
        protocol: protocol_name(version).into(),
        vhost: vhost_name.clone(),
        user: user.name.clone(),
        peer,
        local,
        client_properties,
        connected_at_ms: mq_core::now_ms(),
        tls,
        auth_mechanism: "none".into(),
        frame_max: 0.into(),
        channel_max: 0.into(),
        heartbeat: (c.keep_alive as u32).into(),
        recv_oct: 0.into(),
        send_oct: 0.into(),
        recv_cnt: 1.into(),
        send_cnt: 0.into(),
        channels: 1.into(),
        blocked: AtomicBool::new(false),
        close_reason: Mutex::new(None),
        close: tokio::sync::Notify::new(),
    });
    broker.register_connection(info.clone());
    reader.info = Some(info.clone());
    let chan = Arc::new(ChannelInfo {
        id: broker.next_id(),
        conn: id,
        number: 1,
        conn_name: info.name.clone(),
        vhost: vhost_name.clone(),
        user: user.name.clone(),
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
    broker.register_channel(chan.clone());

    let prefetch = (cfg.prefetch as u32).min(c.props.receive_maximum.filter(|r| *r > 0).unwrap_or(u16::MAX) as u32).max(1);
    chan.prefetch_count.store(prefetch, Ordering::Relaxed);
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        info: info.clone(),
        events: ev_tx,
        backlog: AtomicUsize::new(0),
        stalled: AtomicBool::new(false),
        inflight: AtomicU32::new(0),
        prefetch,
        closing: AtomicBool::new(false),
        queues: Mutex::new(Vec::new()),
    });
    let writer = tokio::spawn(writer_loop(wr, out_rx, shared.clone()));
    let ctl = SessionCtl::new(id);
    let mut s = Session {
        state: state::of(&broker),
        broker: broker.clone(),
        vhost,
        user,
        cfg: cfg.clone(),
        version,
        client_id: c.client_id.clone(),
        clean_start: c.clean_start,
        session_expiry,
        keep_alive: c.keep_alive,
        will: c.will.clone(),
        info: info.clone(),
        chan,
        shared,
        events: ev_rx,
        out: out_tx,
        ctl: ctl.clone(),
        subs: HashMap::new(),
        queues: [None, None],
        consuming: [false, false],
        next_packet_id: 1,
        inflight: HashMap::new(),
        pending: HashSet::new(),
        unroutable: HashSet::new(),
        aliases_in: HashMap::new(),
        aliases_out: HashMap::new(),
        alias_max_out: if version == V5 { c.props.topic_alias_maximum.unwrap_or(0).min(cfg.topic_alias_maximum) } else { 0 },
        max_packet_out: if version == V5 { c.props.maximum_packet_size.unwrap_or(u32::MAX) } else { u32::MAX },
        local_pubs: HashMap::new(),
        perm_ok: HashSet::new(),
    };

    // Take over an existing session with this client id (the new connection's will settings
    // decide whether the old one publishes its will, like RabbitMQ).
    if let Some(old) = s.state.register(&vhost_name, &s.client_id, ctl.clone()) {
        let delayed_will = c.will.as_ref().and_then(|w| w.props.will_delay).unwrap_or(0) > 0;
        info!(client_id = %s.client_id, "MQTT disconnecting older connection with duplicate client id");
        old.take_over(!(!c.clean_start && delayed_will)).await;
    }
    let result = match s.start().await {
        Ok(session_present) => {
            let mut props = Props {
                maximum_qos: Some(1),
                topic_alias_maximum: Some(cfg.topic_alias_maximum),
                maximum_packet_size: Some(cfg.max_packet_size_authenticated),
                shared_sub_available: Some(0),
                session_expiry: Some(session_expiry),
                ..Default::default()
            };
            if assigned && version == V5 {
                props.assigned_client_id = Some(s.client_id.clone());
            }
            s.send(&connack(version, session_present, rc::SUCCESS, props));
            info!(conn = %info.name, client_id = %s.client_id, vhost = %vhost_name, protocol = protocol_name(version), "accepted MQTT connection");
            let will = s.run(&mut reader).await;
            s.finish(will).await;
            Ok(())
        }
        Err(code) => {
            warn!(conn = %info.name, client_id = %s.client_id, code, "rejected MQTT connection");
            s.send(&connack(version, false, code, Props::default()));
            s.will = None;
            s.finish(false).await;
            Ok(())
        }
    };
    let _ = s.out.send(Out::Close);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    s.state.unregister(&vhost_name, &s.client_id, id);
    broker.connection_closed(id).await;
    ctl.finished();
    result
}

// ── session ──

/// A subscription: its options and the binding that implements it.
#[derive(Clone, Debug, PartialEq)]
struct Sub {
    opts: SubOpts,
    id: Option<u32>,
    args: FieldTable,
}

enum SubErr {
    Refused,
    Quota,
    Other,
}

impl From<AmqpError> for SubErr {
    fn from(e: AmqpError) -> Self {
        if e.code == ACCESS_REFUSED {
            SubErr::Refused
        } else if e.code == PRECONDITION_FAILED && e.text.contains("queue limit") {
            SubErr::Quota
        } else {
            SubErr::Other
        }
    }
}

/// How the session loop ends: should the will be published?
type Stop = bool;

struct Session {
    state: Arc<BrokerState>,
    broker: Arc<Broker>,
    vhost: Arc<VHost>,
    user: User,
    cfg: Arc<Config>,
    version: u8,
    client_id: String,
    clean_start: bool,
    /// Seconds; 0 ends the session with the connection.
    session_expiry: u32,
    keep_alive: u16,
    will: Option<packet::Will>,
    info: Arc<ConnectionInfo>,
    chan: Arc<ChannelInfo>,
    shared: Arc<Shared>,
    events: mpsc::UnboundedReceiver<Ev>,
    out: mpsc::UnboundedSender<Out>,
    ctl: Arc<SessionCtl>,
    subs: HashMap<String, Sub>,
    queues: [Option<QueueRef>; 2],
    consuming: [bool; 2],
    next_packet_id: u16,
    /// Server → client QoS 1 deliveries awaiting PUBACK.
    inflight: HashMap<u16, (QueueRef, u64)>,
    /// Client → server QoS 1 publishes awaiting their confirm.
    pending: HashSet<u16>,
    unroutable: HashSet<u16>,
    aliases_in: HashMap<u16, String>,
    aliases_out: HashMap<String, u16>,
    alias_max_out: u16,
    max_packet_out: u32,
    /// Messages this connection published, for No Local: keyed by the address of their
    /// property buffer, which the held clone keeps alive (and so unique) while tracked.
    local_pubs: HashMap<usize, (Bytes, Instant)>,
    perm_ok: HashSet<(u8, bool, String)>,
}

impl Session {
    fn v5(&self) -> bool {
        self.version == V5
    }

    fn send(&self, p: &Packet) {
        let _ = self.out.send(Out::Data(p.to_bytes(self.version), false));
    }

    fn disconnect(&self, code: u8) {
        if self.v5() {
            self.send(&Packet::Disconnect { code, props: Props::default() });
        }
    }

    fn sparkplug(&self) -> bool {
        self.cfg.sparkplug
    }

    fn check(&mut self, access: Access, is_queue: bool, name: &str) -> mq_core::Result<()> {
        let key = (access as u8, is_queue, name.to_string());
        if self.perm_ok.contains(&key) {
            return Ok(());
        }
        self.broker.check(&self.user, &self.vhost.name, access, if is_queue { "queue" } else { "exchange" }, name)?;
        if self.perm_ok.len() > 64 {
            self.perm_ok.clear();
        }
        self.perm_ok.insert(key);
        Ok(())
    }

    fn check_topic(&self, mqtt_topic: &str, access: Access) -> mq_core::Result<()> {
        let key = topic::mqtt_to_amqp(mqtt_topic, self.sparkplug());
        self.broker.check_topic(&self.user, &self.vhost.name, &self.cfg.exchange, &key, access, Some(&self.client_id))
    }

    fn queue_name(&self, qos: u8) -> String {
        queue_name(&self.client_id, qos)
    }

    /// Session setup after authentication: clear a pending delayed will, then start clean or
    /// resume the stored session. Returns Session Present, or a CONNACK reason code.
    async fn start(&mut self) -> Result<bool, u8> {
        let will_q = will_queue_name(&self.client_id);
        self.delete_queue(&will_q).await.map_err(|e| if e.code == ACCESS_REFUSED { rc::NOT_AUTHORIZED } else { rc::IMPLEMENTATION_SPECIFIC })?;
        if !self.clean_start {
            // The queues are keyed on the client id, not the user: the resuming user must be
            // allowed to read every stored subscription.
            for qos in [0u8, 1] {
                for b in self.bindings_of(qos) {
                    let f = topic::amqp_to_mqtt(&b.0, self.sparkplug());
                    if self.check_topic(&f, Access::Read).is_err() {
                        return Err(rc::NOT_AUTHORIZED);
                    }
                }
            }
        }
        let mut present = false;
        for qos in [0u8, 1] {
            let name = self.queue_name(qos);
            let Some(q) = self.vhost.queue(&name) else { continue };
            if self.clean_start {
                self.delete_queue(&name).await.map_err(|e| if e.code == ACCESS_REFUSED { rc::NOT_AUTHORIZED } else { rc::IMPLEMENTATION_SPECIFIC })?;
                continue;
            }
            if q.meta.exclusive_owner.is_some_and(|o| o != self.info.id) {
                // Left over from a connection that is still going away; created again on subscribe.
                continue;
            }
            self.queues[qos as usize] = Some(q.clone());
            self.consume(qos, &q).await.map_err(|e| if e.code == ACCESS_REFUSED { rc::NOT_AUTHORIZED } else { rc::IMPLEMENTATION_SPECIFIC })?;
            present = true;
        }
        if present {
            self.restore_subscriptions().await.map_err(|e| if e.code == ACCESS_REFUSED { rc::NOT_AUTHORIZED } else { rc::IMPLEMENTATION_SPECIFIC })?;
        }
        Ok(present)
    }

    /// (binding key, args) of the session queue for `qos`.
    fn bindings_of(&self, qos: u8) -> Vec<(String, FieldTable)> {
        let Some(x) = self.vhost.exchange(&self.cfg.exchange) else { return vec![] };
        let dest = Dest::queue(self.queue_name(qos));
        x.bindings().into_iter().filter(|b| b.dest == dest).map(|b| (b.routing_key, b.args)).collect()
    }

    /// Rebuild the subscription table from the session queues' bindings, converting binding
    /// arguments when a session moves between protocol versions.
    async fn restore_subscriptions(&mut self) -> mq_core::Result<()> {
        for qos in [0u8, 1] {
            if self.queues[qos as usize].is_none() {
                continue;
            }
            for (key, args) in self.bindings_of(qos) {
                let filter = topic::amqp_to_mqtt(&key, self.sparkplug());
                let (opts, id) = match args.get("x-mqtt-subscription-opts").and_then(|v| v.as_table()) {
                    Some(t) => opts_from_table(t, qos),
                    None => (SubOpts { qos, ..Default::default() }, None),
                };
                let want = self.binding_args(&filter, &opts, id);
                if want != args {
                    self.vhost.bind(&self.cfg.exchange, Dest::queue(self.queue_name(qos)), &key, want.clone(), Some(self.info.id)).await?;
                    self.vhost.unbind(&self.cfg.exchange, Dest::queue(self.queue_name(qos)), &key, args, Some(self.info.id)).await?;
                }
                self.subs.insert(filter, Sub { opts, id, args: want });
            }
        }
        Ok(())
    }

    /// RabbitMQ's binding arguments: none for v3/v4, the subscription options for v5.
    fn binding_args(&self, filter: &str, o: &SubOpts, id: Option<u32>) -> FieldTable {
        if !self.v5() {
            return FieldTable::new();
        }
        let mut t = FieldTable::new();
        if let Some(id) = id {
            t.insert("id", FieldValue::I64(id as i64));
        }
        t.insert("no-local", FieldValue::Bool(o.no_local));
        t.insert("qos", FieldValue::I64(o.qos as i64));
        t.insert("retain-as-published", FieldValue::Bool(o.retain_as_published));
        t.insert("retain-handling", FieldValue::I64(o.retain_handling as i64));
        let mut args = FieldTable::new();
        args.insert("x-binding-key", topic::mqtt_to_amqp(filter, self.sparkplug()));
        args.insert("x-mqtt-subscription-opts", FieldValue::Table(t));
        args
    }

    async fn delete_queue(&mut self, name: &str) -> mq_core::Result<()> {
        if self.vhost.queue(name).is_none() {
            return Ok(());
        }
        self.check(Access::Configure, true, name)?;
        match self.vhost.delete_queue(name, false, false, None).await {
            Err(e) if e.code == mq_core::error::NOT_FOUND => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// The session queue for `qos`, declared on first use.
    async fn ensure_queue(&mut self, qos: u8) -> mq_core::Result<QueueRef> {
        if let Some(q) = &self.queues[qos as usize]
            && !q.is_deleted()
        {
            return Ok(q.clone());
        }
        let name = self.queue_name(qos);
        if let Some(q) = self.vhost.queue(&name) {
            if q.meta.exclusive_owner.is_none_or(|o| o == self.info.id) {
                self.queues[qos as usize] = Some(q.clone());
                return Ok(q);
            }
            self.delete_queue(&name).await?;
        }
        self.check(Access::Configure, true, &name)?;
        let mut args = FieldTable::new();
        let exclusive = self.session_expiry == 0;
        if exclusive {
            // Exclusive queues are always classic, whatever the vhost's default type.
            if self.vhost.default_queue_type() != QueueType::Classic {
                args.insert("x-queue-type", "classic");
            }
        } else {
            if self.session_expiry != u32::MAX {
                args.insert("x-expires", FieldValue::I64(self.session_expiry as i64 * 1000));
            }
            if qos == 1
                && let Some(t) = &self.cfg.durable_queue_type
            {
                args.insert("x-queue-type", t.as_str());
            }
        }
        let ok = self
            .vhost
            .declare_queue(DeclareQueue {
                name,
                durable: true,
                exclusive_owner: exclusive.then_some(self.info.id),
                auto_delete: false,
                args,
                passive: false,
            })
            .await?;
        self.queues[qos as usize] = Some(ok.queue.clone());
        Ok(ok.queue)
    }

    async fn consume(&mut self, qos: u8, q: &QueueRef) -> mq_core::Result<()> {
        if self.consuming[qos as usize] {
            return Ok(());
        }
        self.check(Access::Read, true, &q.meta.name)?;
        let sink = Arc::new(Sink { shared: self.shared.clone(), no_ack: qos == 0 });
        q.consume(ConsumerSpec {
            tag: Arc::from(CONSUMER_TAG),
            channel: self.chan.id,
            conn: self.info.id,
            no_ack: qos == 0,
            exclusive: false,
            priority: 0,
            args: FieldTable::new(),
            sink,
        })
        .await?;
        self.consuming[qos as usize] = true;
        {
            let mut v = self.shared.queues.lock();
            v.retain(|w| w.strong_count() > 0);
            v.push(Arc::downgrade(q));
        }
        self.chan.consumers.fetch_add(1, Ordering::Relaxed);
        q.wake();
        Ok(())
    }

    /// Bind or unbind a subscription with RabbitMQ's permission checks (write on the queue,
    /// read on the exchange, topic read).
    async fn binding(&mut self, qos: u8, filter: &str, args: FieldTable, add: bool) -> mq_core::Result<()> {
        let qname = self.queue_name(qos);
        self.check(Access::Write, true, &qname)?;
        let x = self.cfg.exchange.clone();
        self.check(Access::Read, false, &x)?;
        self.check_topic(filter, Access::Read)?;
        let key = topic::mqtt_to_amqp(filter, self.sparkplug());
        let conn = Some(self.info.id);
        if add { self.vhost.bind(&x, Dest::queue(qname), &key, args, conn).await } else { self.vhost.unbind(&x, Dest::queue(qname), &key, args, conn).await }
    }

    fn next_packet_id(&mut self) -> u16 {
        loop {
            let id = self.next_packet_id;
            self.next_packet_id = if id == u16::MAX { 1 } else { id + 1 };
            if !self.inflight.contains_key(&id) {
                return id;
            }
        }
    }

    /// Topic Alias for an outgoing PUBLISH (v5, when the client allows aliases).
    fn alias_out(&mut self, topic: String, props: &mut Props) -> String {
        if self.alias_max_out == 0 {
            return topic;
        }
        if let Some(a) = self.aliases_out.get(&topic) {
            props.topic_alias = Some(*a);
            return String::new();
        }
        if self.aliases_out.len() < self.alias_max_out as usize && topic.len() > 1 {
            let a = self.aliases_out.len() as u16 + 1;
            self.aliases_out.insert(topic.clone(), a);
            props.topic_alias = Some(a);
        }
        topic
    }

    // ── the loop ──

    /// Returns whether the will should be published.
    async fn run<R: AsyncRead + Unpin>(&mut self, reader: &mut Reader<R>) -> Stop {
        let ka = if self.keep_alive > 0 { Some(Duration::from_millis(self.keep_alive as u64 * 1500)) } else { None };
        let mut last = tokio::time::Instant::now();
        let max_in = self.cfg.max_packet_size_authenticated;
        let ctl = self.ctl.clone();
        let info = self.info.clone();
        loop {
            let deadline = last + ka.unwrap_or(Duration::from_secs(86_400 * 365));
            let stop = tokio::select! {
                biased;
                _ = info.close.notified() => {
                    let reason = info.close_reason.lock().clone().unwrap_or_default();
                    info!(conn = %info.name, client_id = %self.client_id, %reason, "closing MQTT connection");
                    self.disconnect(if self.broker.draining.load(Ordering::Relaxed) { rc::SERVER_SHUTTING_DOWN } else { 0x98 });
                    Some(true)
                }
                _ = ctl.kick.notified() => {
                    warn!(conn = %info.name, client_id = %self.client_id, "MQTT disconnecting client with duplicate id");
                    self.disconnect(rc::SESSION_TAKEN_OVER);
                    Some(ctl.send_will.load(Ordering::Acquire))
                }
                Some(ev) = self.events.recv() => self.on_event(ev).await,
                _ = tokio::time::sleep_until(deadline), if ka.is_some() => {
                    warn!(conn = %info.name, client_id = %self.client_id, "closing MQTT connection (keepalive timeout)");
                    self.disconnect(rc::KEEP_ALIVE_TIMEOUT);
                    Some(true)
                }
                r = reader.next(self.version, max_in) => {
                    last = tokio::time::Instant::now();
                    match r {
                        Ok(p) => self.on_packet(p).await,
                        Err(ReadError::Closed) => Some(true),
                        Err(ReadError::Io(e)) => {
                            debug!(conn = %info.name, error = %e, "MQTT network error");
                            Some(true)
                        }
                        Err(ReadError::Decode(e)) => {
                            warn!(conn = %info.name, client_id = %self.client_id, error = %e, "MQTT packet error");
                            self.disconnect(e.reason_code());
                            Some(true)
                        }
                    }
                }
            };
            if let Some(will) = stop {
                return will;
            }
        }
    }

    async fn on_packet(&mut self, p: Packet) -> Option<Stop> {
        match p {
            Packet::Publish(p) => return self.on_publish(p).await,
            Packet::PubAck { id, code, .. } => self.on_puback(id, code).await,
            // QoS 2 is not supported (RabbitMQ downgrades it to QoS 1).
            Packet::PubRec { .. } | Packet::PubRel { .. } | Packet::PubComp { .. } => {}
            Packet::Subscribe { id, props, filters } => return self.on_subscribe(id, props, filters).await,
            Packet::Unsubscribe { id, filters, .. } => self.on_unsubscribe(id, filters).await,
            Packet::PingReq => self.send(&Packet::PingResp),
            Packet::Disconnect { code, props } => {
                self.on_disconnect_expiry(props.session_expiry);
                // Anything but a normal disconnection publishes the will [v5 3.14].
                return Some(code != rc::SUCCESS);
            }
            Packet::Auth { .. } => {
                warn!(client_id = %self.client_id, "MQTT extended authentication is not supported");
                self.disconnect(rc::BAD_AUTHENTICATION_METHOD);
                return Some(true);
            }
            other => {
                warn!(client_id = %self.client_id, packet = ?std::mem::discriminant(&other), "unexpected MQTT packet");
                self.disconnect(rc::PROTOCOL_ERROR);
                return Some(true);
            }
        }
        None
    }

    fn on_disconnect_expiry(&mut self, requested: Option<u32>) {
        let Some(req) = requested else { return };
        let max = self.cfg.max_session_expiry_interval_secs;
        let new = if req == u32::MAX { max } else { req.min(max) };
        if self.session_expiry == 0 && new > 0 {
            warn!(client_id = %self.client_id, "MQTT protocol error: ignoring Session Expiry Interval in DISCONNECT because it was 0 in CONNECT");
            return;
        }
        self.session_expiry = new;
    }

    async fn on_event(&mut self, ev: Ev) -> Option<Stop> {
        match ev {
            Ev::Deliver(d, no_ack) => self.deliver(*d, no_ack).await,
            Ev::Confirmed(id, ok) => {
                if !self.pending.remove(&id) {
                    return None;
                }
                let unroutable = self.unroutable.remove(&id);
                self.chan.confirmed.fetch_add(1, Ordering::Relaxed);
                let code = match (ok, unroutable) {
                    (true, false) => rc::SUCCESS,
                    (true, true) if self.v5() => rc::NO_MATCHING_SUBSCRIBERS,
                    (true, true) => rc::SUCCESS,
                    // v3 has no negative acknowledgement; RabbitMQ sends nothing.
                    (false, _) if !self.v5() => return None,
                    (false, _) => rc::QUOTA_EXCEEDED,
                };
                self.send(&Packet::PubAck { id, code, props: Props::default() });
            }
            Ev::Cancelled => {
                info!(client_id = %self.client_id, "terminating MQTT connection because its consuming queue is down");
                return Some(true);
            }
            Ev::TimedOut => {
                info!(client_id = %self.client_id, "terminating MQTT connection because a consumer timed out");
                return Some(true);
            }
        }
        None
    }

    // ── publishing ──

    async fn on_publish(&mut self, mut p: Publish) -> Option<Stop> {
        if p.qos == 2 && self.v5() {
            warn!(client_id = %self.client_id, "received a PUBLISH with QoS 2; disconnecting");
            self.disconnect(rc::QOS_NOT_SUPPORTED);
            return Some(true);
        }
        if let Some(alias) = p.props.topic_alias.take() {
            if alias == 0 || alias > self.cfg.topic_alias_maximum {
                warn!(client_id = %self.client_id, alias, "invalid Topic Alias");
                self.disconnect(rc::TOPIC_ALIAS_INVALID);
                return Some(true);
            }
            if p.topic.is_empty() {
                match self.aliases_in.get(&alias) {
                    Some(t) => p.topic = t.clone(),
                    None => {
                        warn!(client_id = %self.client_id, alias, "unknown Topic Alias");
                        self.disconnect(rc::PROTOCOL_ERROR);
                        return Some(true);
                    }
                }
            } else {
                self.aliases_in.insert(alias, p.topic.clone());
            }
        }
        if !topic::valid_name(&p.topic) {
            warn!(client_id = %self.client_id, topic = %p.topic, "invalid topic name in PUBLISH");
            self.disconnect(rc::TOPIC_NAME_INVALID);
            return Some(true);
        }
        let max = self.broker.config.max_message_size;
        if p.payload.len() as u64 > max {
            warn!(client_id = %self.client_id, size = p.payload.len(), max, "message is larger than the configured max size");
            self.disconnect(rc::PACKET_TOO_LARGE);
            return Some(true);
        }
        let qos = p.qos.min(1);
        if qos == 1 && self.pending.contains(&p.packet_id) {
            // A re-sent PUBLISH that is already on its way to the queues.
            return None;
        }
        if let Err(e) = self.check_publish(&p.topic) {
            warn!(client_id = %self.client_id, error = %e, "MQTT publish refused");
            if self.cfg.disconnect_on_unauthorized {
                return Some(true);
            }
            if qos == 1 {
                self.send(&Packet::PubAck { id: p.packet_id, code: rc::NOT_AUTHORIZED, props: Props::default() });
            }
            return None;
        }
        if self.broker.alarm.load(Ordering::Relaxed) {
            self.block_until_clear().await;
        }
        let msg = convert::to_message(&self.cfg.exchange, &p.topic, qos, p.retain, &p.props, p.payload.clone(), self.sparkplug());
        if self.subs.values().any(|s| s.opts.no_local) {
            let now = Instant::now();
            if self.local_pubs.len() > 4096 {
                self.local_pubs.retain(|_, (_, t)| now.duration_since(*t) < Duration::from_secs(60));
            }
            if !msg.props_raw.is_empty() {
                self.local_pubs.insert(msg.props_raw.as_ptr() as usize, (msg.props_raw.clone(), now));
            }
        }
        let ticket = (qos == 1).then(|| {
            self.pending.insert(p.packet_id);
            let events = self.shared.events.clone();
            let id = p.packet_id;
            ConfirmTicket::new(move |ok| {
                let _ = events.send(Ev::Confirmed(id, ok));
            })
        });
        self.chan.published.fetch_add(1, Ordering::Relaxed);
        match self.vhost.publish(&self.cfg.exchange, msg, ticket).await {
            Ok(outcome) => {
                if let Some(t) = outcome.confirm {
                    if outcome.routed == 0 {
                        self.unroutable.insert(p.packet_id);
                    }
                    t.settle(true);
                }
            }
            Err(e) => {
                warn!(client_id = %self.client_id, error = %e, "MQTT publish failed");
                self.pending.remove(&p.packet_id);
                return Some(true);
            }
        }
        if p.retain {
            self.store_retained(&p.topic, qos, &p.props, p.payload).await;
        }
        None
    }

    fn check_publish(&mut self, mqtt_topic: &str) -> mq_core::Result<()> {
        let x = self.cfg.exchange.clone();
        self.check(Access::Write, false, &x)?;
        self.check_topic(mqtt_topic, Access::Write)
    }

    async fn store_retained(&self, mqtt_topic: &str, qos: u8, props: &Props, payload: Bytes) {
        let app = Props {
            payload_format: props.payload_format,
            content_type: props.content_type.clone(),
            response_topic: props.response_topic.clone(),
            correlation_data: props.correlation_data.clone(),
            user: props.user.clone(),
            ..Default::default()
        };
        let r = Retained {
            topic: mqtt_topic.to_string(),
            qos,
            payload,
            props: app,
            expires_at_ms: props.message_expiry.map(|s| mq_core::now_ms() + s as u64 * 1000),
        };
        if let Err(e) = self.state.retain(&self.vhost.name, r).await {
            warn!(topic = mqtt_topic, error = %e, "failed to store retained message");
        }
    }

    async fn block_until_clear(&self) {
        let broker = &self.broker;
        self.info.blocked.store(true, Ordering::Release);
        while broker.alarm.load(Ordering::Acquire) {
            let n = broker.alarm_cleared.notified();
            if !broker.alarm.load(Ordering::Acquire) {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), n).await;
        }
        self.info.blocked.store(false, Ordering::Release);
    }

    // ── delivering ──

    async fn settle(&self, q: &QueueRef, seq: u64, outcome: Outcome) {
        q.settle(self.chan.id, vec![seq], outcome).await;
        let before = self.shared.inflight.fetch_sub(1, Ordering::AcqRel);
        self.chan.unacked.store(before.saturating_sub(1) as u64, Ordering::Relaxed);
        if before >= self.shared.prefetch {
            q.wake();
        }
    }

    async fn deliver(&mut self, d: Delivery, no_ack: bool) {
        let sub_qos: u8 = if no_ack { 0 } else { 1 };
        let mut out = convert::from_message(&d.msg, mq_core::now_ms(), self.sparkplug());
        let qos = out.qos.min(sub_qos);
        let matched: Vec<&Sub> = self
            .subs
            .iter()
            .filter(|(f, s)| s.opts.qos == sub_qos && topic::amqp_matches(&topic::mqtt_to_amqp(f, self.cfg.sparkplug), &d.msg.routing_key))
            .map(|(_, s)| s)
            .collect();
        let local = !matched.is_empty()
            && matched.iter().all(|s| s.opts.no_local)
            && !d.msg.props_raw.is_empty()
            && self.local_pubs.contains_key(&(d.msg.props_raw.as_ptr() as usize));
        // At-most-once subscribers never see redeliveries.
        if local || (d.redelivered && qos == 0) {
            self.shared.release_backlog(1);
            if !no_ack {
                self.settle(&d.queue, d.seq, Outcome::Ack).await;
            }
            return;
        }
        let retain = self.v5() && out.retain && matched.iter().any(|s| s.opts.retain_as_published);
        if self.v5() {
            let mut ids: Vec<u32> = matched.iter().filter_map(|s| s.id).collect();
            ids.sort_unstable();
            ids.dedup();
            out.props.subscription_ids = ids;
        } else {
            out.props = Props::default();
        }
        let topic = self.alias_out(out.topic, &mut out.props);
        let packet_id = if qos == 1 { self.next_packet_id() } else { 0 };
        let pkt = Packet::Publish(Publish { dup: d.redelivered, qos, retain, topic, packet_id, props: out.props, payload: out.payload });
        let bytes = pkt.to_bytes(self.version);
        if bytes.len() as u64 > self.max_packet_out as u64 {
            // Too large for the client: discard as if delivered [MQTT-3.1.2-25].
            debug!(client_id = %self.client_id, size = bytes.len(), "dropping PUBLISH larger than the client's maximum packet size");
            self.shared.release_backlog(1);
            if !no_ack {
                self.settle(&d.queue, d.seq, Outcome::Reject).await;
            }
            return;
        }
        let _ = self.out.send(Out::Data(bytes, true));
        self.chan.delivered.fetch_add(1, Ordering::Relaxed);
        if d.redelivered {
            self.chan.redelivered.fetch_add(1, Ordering::Relaxed);
        }
        let vs = &self.vhost.stats;
        vs.delivered.fetch_add(1, Ordering::Relaxed);
        vs.delivered_bytes.fetch_add(d.msg.body.len() as u64, Ordering::Relaxed);
        if !no_ack {
            if qos == 1 {
                self.chan.unacked.fetch_add(1, Ordering::Relaxed);
                self.inflight.insert(packet_id, (d.queue, d.seq));
            } else {
                // A QoS 0 message on a QoS 1 subscription is never acknowledged by the client.
                self.settle(&d.queue, d.seq, Outcome::Ack).await;
            }
        }
    }

    async fn on_puback(&mut self, id: u16, code: u8) {
        let Some((q, seq)) = self.inflight.remove(&id) else { return };
        // A failure reason code still counts as acknowledged and is not redelivered [MQTT-4.4.0-2].
        let outcome = if code < 0x80 { Outcome::Ack } else { Outcome::Reject };
        if outcome == Outcome::Ack {
            self.chan.acked.fetch_add(1, Ordering::Relaxed);
            self.vhost.stats.acked.fetch_add(1, Ordering::Relaxed);
        }
        self.settle(&q, seq, outcome).await;
    }

    // ── subscriptions ──

    async fn on_subscribe(&mut self, packet_id: u16, props: Props, filters: Vec<(String, SubOpts)>) -> Option<Stop> {
        let sub_id = props.subscription_ids.first().copied();
        let mut codes = Vec::with_capacity(filters.len());
        let mut retained: Vec<(String, u8, Option<u32>)> = Vec::new();
        let mut fatal: Option<u8> = None;
        for (filter, mut opts) in filters {
            if let Some(c) = fatal {
                codes.push(c);
                continue;
            }
            if self.v5() && topic::is_shared(&filter) {
                codes.push(rc::SHARED_SUBSCRIPTIONS_NOT_SUPPORTED);
                continue;
            }
            if !topic::valid_filter(&filter) {
                codes.push(if self.v5() { rc::TOPIC_FILTER_INVALID } else { rc::UNSPECIFIED });
                continue;
            }
            opts.qos = opts.qos.min(1);
            let rh = opts.retain_handling;
            let args = self.binding_args(&filter, &opts, sub_id);
            let new = Sub { opts, id: sub_id, args };
            if self.subs.get(&filter) == Some(&new) {
                codes.push(opts.qos);
                if rh == 0 {
                    retained.push((filter, opts.qos, sub_id));
                }
                continue;
            }
            match self.subscribe(&filter, new).await {
                Ok(()) => {
                    codes.push(opts.qos);
                    if rh < 2 {
                        retained.push((filter, opts.qos, sub_id));
                    }
                }
                Err(e) => {
                    let (code, is_fatal) = match e {
                        SubErr::Refused => (if self.v5() { rc::NOT_AUTHORIZED } else { rc::UNSPECIFIED }, self.cfg.disconnect_on_unauthorized),
                        SubErr::Quota => (if self.v5() { rc::QUOTA_EXCEEDED } else { rc::UNSPECIFIED }, true),
                        SubErr::Other => (rc::UNSPECIFIED, true),
                    };
                    warn!(client_id = %self.client_id, filter = %filter, code, "MQTT subscription failed");
                    codes.push(code);
                    if is_fatal {
                        fatal = Some(code);
                    }
                }
            }
        }
        self.send(&Packet::SubAck { id: packet_id, props: Props::default(), codes });
        if fatal.is_some() {
            return Some(true);
        }
        for (filter, qos, id) in retained {
            self.send_retained(&filter, qos, id).await;
        }
        None
    }

    async fn subscribe(&mut self, filter: &str, new: Sub) -> Result<(), SubErr> {
        let qos = new.opts.qos;
        let q = self.ensure_queue(qos).await?;
        self.binding(qos, filter, new.args.clone(), true).await?;
        if let Some(old) = self.subs.get(filter).cloned()
            && old != new
        {
            self.binding(old.opts.qos, filter, old.args, false).await?;
        }
        self.subs.insert(filter.to_string(), new);
        self.consume(qos, &q).await?;
        Ok(())
    }

    async fn send_retained(&mut self, filter: &str, sub_qos: u8, sub_id: Option<u32>) {
        let now = mq_core::now_ms();
        for r in self.state.retained_matching(&self.vhost.name, filter, now).await {
            if self.check_topic(&r.topic, Access::Read).is_err() {
                continue;
            }
            let qos = r.qos.min(sub_qos);
            let mut props = if self.v5() { r.props.clone() } else { Props::default() };
            if self.v5() {
                props.message_expiry = r.expires_at_ms.map(|e| (e.saturating_sub(now) / 1000) as u32);
                props.subscription_ids = sub_id.into_iter().collect();
            }
            let topic = self.alias_out(r.topic.clone(), &mut props);
            let packet_id = if qos == 1 { self.next_packet_id() } else { 0 };
            let pkt = Packet::Publish(Publish { dup: false, qos, retain: true, topic, packet_id, props, payload: r.payload });
            let bytes = pkt.to_bytes(self.version);
            if bytes.len() as u64 <= self.max_packet_out as u64 {
                let _ = self.out.send(Out::Data(bytes, false));
            }
        }
    }

    async fn on_unsubscribe(&mut self, packet_id: u16, filters: Vec<String>) {
        let mut codes = Vec::with_capacity(filters.len());
        for f in filters {
            let Some(sub) = self.subs.get(&f).cloned() else {
                codes.push(rc::NO_SUBSCRIPTION_EXISTED);
                continue;
            };
            match self.binding(sub.opts.qos, &f, sub.args, false).await {
                Ok(()) => {
                    self.subs.remove(&f);
                    codes.push(rc::SUCCESS);
                }
                Err(e) if e.code == ACCESS_REFUSED => codes.push(rc::NOT_AUTHORIZED),
                Err(_) => codes.push(rc::UNSPECIFIED),
            }
        }
        self.send(&Packet::UnsubAck { id: packet_id, props: Props::default(), codes });
    }

    // ── end of session ──

    async fn finish(&mut self, send_will: bool) {
        self.shared.closing.store(true, Ordering::Release);
        for (i, q) in self.queues.clone().into_iter().enumerate() {
            if let Some(q) = q
                && self.consuming[i]
            {
                q.channel_closed(self.chan.id).await;
            }
        }
        self.inflight.clear();
        if send_will {
            self.publish_will().await;
        }
        if self.session_expiry == 0 {
            // Resumed (non-exclusive) session queues end with the session too.
            for qos in [0u8, 1] {
                let name = self.queue_name(qos);
                if self.vhost.queue(&name).is_some_and(|q| q.meta.exclusive_owner.is_none()) {
                    let _ = self.delete_queue(&name).await;
                }
            }
        }
        self.broker.channel_closed(self.chan.id);
        self.info.channels.store(0, Ordering::Relaxed);
    }

    async fn publish_will(&mut self) {
        let Some(w) = self.will.take() else { return };
        if self.check_publish(&w.topic).is_err() {
            debug!(client_id = %self.client_id, topic = %w.topic, "will message not permitted");
            return;
        }
        let delay = if self.v5() { w.props.will_delay.unwrap_or(0) } else { 0 };
        if delay > 0 && self.session_expiry > 0 {
            if let Err(e) = self.schedule_will(&w, delay).await {
                debug!(client_id = %self.client_id, error = %e, "failed to schedule delayed will message");
            }
            return;
        }
        let msg = convert::to_message(&self.cfg.exchange, &w.topic, w.qos, w.retain, &w.props, w.payload.clone(), self.sparkplug());
        match self.vhost.publish(&self.cfg.exchange, msg, None).await {
            Ok(_) => debug!(client_id = %self.client_id, topic = %w.topic, "sent will message"),
            Err(e) => debug!(client_id = %self.client_id, error = %e, "failed to send will message"),
        }
        if w.retain {
            self.store_retained(&w.topic, w.qos, &w.props, w.payload).await;
        }
    }

    /// A delayed will waits in `mqtt-will-<client id>` with a TTL and dead-letters to the topic
    /// when it expires; reconnecting deletes the queue (and with it the will).
    async fn schedule_will(&mut self, w: &packet::Will, delay: u32) -> mq_core::Result<()> {
        let name = will_queue_name(&self.client_id);
        self.check(Access::Configure, true, &name)?;
        let mut args = FieldTable::new();
        if self.session_expiry != u32::MAX {
            args.insert("x-expires", FieldValue::I64(self.session_expiry as i64 * 1000 + WILL_QUEUE_EXPIRY_MARGIN_MS));
        }
        args.insert("x-dead-letter-exchange", self.cfg.exchange.as_str());
        args.insert("x-dead-letter-routing-key", topic::mqtt_to_amqp(&w.topic, self.sparkplug()));
        if self.vhost.default_queue_type() != QueueType::Classic {
            args.insert("x-queue-type", "classic");
        }
        self.vhost.declare_queue(DeclareQueue { name: name.clone(), durable: true, exclusive_owner: None, auto_delete: false, args, passive: false }).await?;
        let mut ttl = delay.min(self.session_expiry) as u64 * 1000;
        if self.session_expiry != u32::MAX && delay >= self.session_expiry {
            // Publish the will just before the session (and the queue) expires.
            ttl = ttl.saturating_sub(5);
        }
        let mut msg = convert::to_message("", &w.topic, w.qos, w.retain, &w.props, w.payload.clone(), self.sparkplug());
        let mut p = msg.props.clone();
        p.expiration = Some(ttl.to_string());
        p.delivery_mode = Some(2);
        if let Some(h) = p.headers.as_mut() {
            h.remove(H_RETAIN);
        }
        msg = msg.with_props("", &name, p);
        self.vhost.publish_to_queue(&name, Arc::new(msg), None).await
    }
}

fn opts_from_table(t: &FieldTable, qos: u8) -> (SubOpts, Option<u32>) {
    let o = SubOpts {
        qos: t.get_i64("qos").map(|q| q.clamp(0, 1) as u8).unwrap_or(qos),
        no_local: t.get_bool("no-local").unwrap_or(false),
        retain_as_published: t.get_bool("retain-as-published").unwrap_or(false),
        retain_handling: t.get_i64("retain-handling").map(|r| r.clamp(0, 2) as u8).unwrap_or(0),
    };
    (o, t.get_i64("id").map(|i| i as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_rabbitmq() {
        assert_eq!(queue_name("c1", 0), "mqtt-subscription-c1qos0");
        assert_eq!(queue_name("c1", 1), "mqtt-subscription-c1qos1");
        assert_eq!(will_queue_name("c1"), "mqtt-will-c1");
    }

    #[test]
    fn subscription_opts_table() {
        let mut t = FieldTable::new();
        t.insert("id", FieldValue::I64(7));
        t.insert("no-local", FieldValue::Bool(true));
        t.insert("qos", FieldValue::I64(1));
        t.insert("retain-as-published", FieldValue::Bool(false));
        t.insert("retain-handling", FieldValue::I64(2));
        let (o, id) = opts_from_table(&t, 0);
        assert_eq!(o, SubOpts { qos: 1, no_local: true, retain_as_published: false, retain_handling: 2 });
        assert_eq!(id, Some(7));
    }
}

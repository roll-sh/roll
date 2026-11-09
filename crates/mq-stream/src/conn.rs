//! One stream protocol connection.
//!
//! The handshake (peer properties, SASL, tune, open) runs inline on the socket. After `Open`
//! this task reads and handles commands, a writer task encodes responses, confirms and chunks
//! into coalesced socket writes (and sends heartbeats), and each subscription has a
//! [`ChunkSink`] the stream actor hands whole chunks to, bounded by chunk credit and the
//! writer's backlog.

use crate::amqp10::{self, InBody};
use crate::manage;
use crate::proto::*;
use crate::sac::{Key as SacKey, Notice};
use crate::Server;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use mq_core::auth::{Access, User};
use mq_core::broker::{ConnectionInfo, Event, PRODUCT, RABBITMQ_VERSION};
use mq_core::queue::stream::{Chunk, ChunkSink, ChunkSubscription, Entry, NativeBody, NativePublish, NativeView, encode_native};
use mq_core::queue::{ChannelId, ConfirmTicket, ConnId, QueueRef, StreamOffset};
use mq_core::value::FieldTable;
use mq_core::vhost::VHost;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;
use tracing::{debug, warn};

/// Frame ceiling until `Open` succeeds (`stream.initial_frame_max`).
const INITIAL_FRAME_MAX: u32 = 8192;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Queued socket bytes above which subscriptions pause, and below which they resume.
const BACKLOG_HIGH: usize = 8 << 20;
const BACKLOG_LOW: usize = 2 << 20;
/// Bytes coalesced into one socket write.
const WRITE_BATCH: usize = 1 << 20;
/// How long a closing connection waits for the client's close response.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// Items for the writer task.
enum Out {
    Frame(Bytes),
    Chunk { sub: u8, live: Arc<AtomicBool>, chunk: Chunk, est: usize },
    Confirm { publisher: u8, ids: Vec<u64> },
    Error { publisher: u8, ids: Vec<u64>, code: u16 },
    /// Flush and close the socket.
    Close,
}

enum Ctl {
    /// The writer drained its backlog: paused subscriptions may continue.
    Wake,
    /// A stream this connection uses was deleted or stopped.
    StreamGone(String),
}

/// State shared by the reader, the writer and the chunk sinks.
struct Shared {
    out: mpsc::UnboundedSender<Out>,
    ctl: mpsc::UnboundedSender<Ctl>,
    backlog: AtomicUsize,
    stalled: AtomicBool,
    /// Client takes deliver v2 (committed chunk id).
    deliver_v2: AtomicBool,
    info: Arc<ConnectionInfo>,
}

/// Feeds one subscription's chunks to the writer.
struct SubSink {
    id: u8,
    stream: String,
    credit: AtomicI64,
    max_bytes: usize,
    /// Cleared on unsubscribe: chunks still queued are dropped.
    live: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl ChunkSink for SubSink {
    fn has_credit(&self) -> bool {
        if self.credit.load(Ordering::Acquire) <= 0 || !self.live.load(Ordering::Relaxed) {
            return false;
        }
        if self.shared.backlog.load(Ordering::Relaxed) > BACKLOG_HIGH {
            self.shared.stalled.store(true, Ordering::Release);
            return false;
        }
        true
    }

    fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    fn deliver(&self, chunk: Chunk) {
        self.credit.fetch_sub(1, Ordering::AcqRel);
        let est = 128 + chunk.entries.iter().map(Entry::wire_len).sum::<usize>();
        self.shared.backlog.fetch_add(est, Ordering::Relaxed);
        let _ = self.shared.out.send(Out::Chunk { sub: self.id, live: self.live.clone(), chunk, est });
    }

    fn cancelled(&self) {
        let _ = self.shared.ctl.send(Ctl::StreamGone(self.stream.clone()));
    }
}

struct Sub {
    stream: String,
    queue: QueueRef,
    spec: StreamOffset,
    filters: Option<Vec<Bytes>>,
    match_unfiltered: bool,
    properties: FieldTable,
    tag: Arc<str>,
    sink: Arc<SubSink>,
    /// Registration with the stream actor (absent while a SAC member is inactive).
    reg: Option<ChannelId>,
    sac: Option<SacKey>,
    /// SAC: the coordinator made this the active consumer.
    active: bool,
}

struct Publisher {
    stream: String,
    reference: Option<Arc<str>>,
    queue: QueueRef,
}

/// An outstanding server → client `ConsumerUpdate`.
struct Pending {
    sub: u8,
    active: bool,
    stepping_down: bool,
    key: SacKey,
}

enum Flow {
    Continue,
    Stop,
}

struct FrameIn {
    rd: OwnedReadHalf,
    buf: BytesMut,
}

enum Next {
    Frame(Bytes),
    TooLarge,
}

impl FrameIn {
    /// The next complete frame in the buffer (without its size prefix).
    fn next(&mut self, max: u32) -> Option<Next> {
        let size = u32::from_be_bytes(self.buf.get(..4)?.try_into().ok()?);
        if max != 0 && size > max {
            return Some(Next::TooLarge);
        }
        let need = 4 + size as usize;
        if self.buf.len() < need {
            self.buf.reserve(need - self.buf.len());
            return None;
        }
        self.buf.advance(4);
        Some(Next::Frame(self.buf.split_to(size as usize).freeze()))
    }

    async fn fill(&mut self) -> std::io::Result<usize> {
        if self.buf.capacity() - self.buf.len() < 16 << 10 {
            self.buf.reserve(64 << 10);
        }
        self.rd.read_buf(&mut self.buf).await
    }
}

/// What the handshake settled.
struct Session {
    user: User,
    vhost: Arc<VHost>,
    vhost_name: String,
    frame_max: u32,
    heartbeat: u32,
    client_properties: Vec<(String, String)>,
}

fn negotiate(client: u32, server: u32) -> u32 {
    match (client, server) {
        (0, s) => s,
        (c, 0) => c,
        (c, s) => c.min(s),
    }
}

async fn write_frame(wr: &mut OwnedWriteHalf, b: &mut BytesMut, at: usize) -> std::io::Result<()> {
    finish(b, at);
    wr.write_all(b).await?;
    b.clear();
    Ok(())
}

/// Peer properties, SASL PLAIN, tune and open. `None`: the connection is to be closed.
async fn handshake(server: &Server, fin: &mut FrameIn, wr: &mut OwnedWriteHalf, peer: SocketAddr) -> anyhow::Result<Option<Session>> {
    let broker = &server.broker;
    let mut b = BytesMut::with_capacity(512);
    let mut client_properties = Vec::new();
    let mut user: Option<User> = None;
    let (mut frame_max, mut heartbeat) = (server.config.frame_max, server.config.heartbeat);
    loop {
        let frame = loop {
            match fin.next(INITIAL_FRAME_MAX) {
                Some(Next::Frame(f)) => break f,
                Some(Next::TooLarge) => return Ok(None),
                None => {
                    if fin.fill().await? == 0 {
                        return Ok(None);
                    }
                }
            }
        };
        let mut r = Rd::new(&frame);
        let key = r.u16()?;
        let _version = r.u16()?;
        match key {
            PEER_PROPERTIES => {
                let corr = r.u32()?;
                client_properties = r.map()?;
                let at = response(&mut b, PEER_PROPERTIES, corr, OK);
                let props = [
                    ("product", PRODUCT),
                    ("version", RABBITMQ_VERSION),
                    ("cluster_name", broker.config.cluster_name.as_str()),
                    ("platform", "Rust"),
                    ("copyright", "Copyright (c) roll"),
                    ("information", "roll queues — RabbitMQ-compatible serverless message broker"),
                ];
                put_map(&mut b, props.into_iter());
                write_frame(wr, &mut b, at).await?;
            }
            SASL_HANDSHAKE => {
                let corr = r.u32()?;
                let at = response(&mut b, SASL_HANDSHAKE, corr, OK);
                b.put_u32(1);
                put_str(&mut b, b"PLAIN");
                write_frame(wr, &mut b, at).await?;
            }
            SASL_AUTHENTICATE => {
                let corr = r.u32()?;
                let mechanism = r.opt_str()?.unwrap_or_default();
                let data = r.bytes()?.unwrap_or_default();
                let code = if mechanism != b"PLAIN" {
                    SASL_MECHANISM_NOT_SUPPORTED
                } else {
                    let parts: Vec<&[u8]> = data.split(|c| *c == 0).collect();
                    match parts[..] {
                        [_, u, p] => {
                            let (u, p) = (String::from_utf8_lossy(u), String::from_utf8_lossy(p));
                            match broker.authenticate(&u, &p, Some(peer)) {
                                Ok(found) => {
                                    user = Some(found);
                                    OK
                                }
                                Err(e) if e.text.contains("only connect via localhost") => AUTHENTICATION_FAILURE_LOOPBACK,
                                Err(_) => AUTHENTICATION_FAILURE,
                            }
                        }
                        _ => SASL_ERROR,
                    }
                };
                let at = response(&mut b, SASL_AUTHENTICATE, corr, code);
                write_frame(wr, &mut b, at).await?;
                if code != OK {
                    warn!(%peer, code, "stream authentication failed");
                    return Ok(None);
                }
                let at = begin(&mut b, TUNE, 1);
                b.put_u32(server.config.frame_max);
                b.put_u32(server.config.heartbeat);
                write_frame(wr, &mut b, at).await?;
            }
            k if k == TUNE || k == TUNE | RESPONSE => {
                frame_max = negotiate(r.u32()?, server.config.frame_max);
                heartbeat = r.u32()?;
            }
            OPEN => {
                let corr = r.u32()?;
                let vhost_name = r.string()?;
                let Some(user) = user.take() else { return Ok(None) };
                let opened = match broker.check_vhost_access(&user, &vhost_name) {
                    Ok(()) => match broker.open_vhost(&vhost_name).await {
                        Some(vh) => broker.check_connection_limits(&user, &vhost_name).ok().map(|_| vh),
                        None => None,
                    },
                    Err(_) => None,
                };
                let Some(vhost) = opened else {
                    warn!(%peer, vhost = %vhost_name, "stream connection refused vhost access");
                    let at = response(&mut b, OPEN, corr, VHOST_ACCESS_FAILURE);
                    write_frame(wr, &mut b, at).await?;
                    return Ok(None);
                };
                let at = response(&mut b, OPEN, corr, OK);
                let port = server.config.advertised_port.to_string();
                put_map(&mut b, [("advertised_host", server.config.advertised_host.as_str()), ("advertised_port", port.as_str())].into_iter());
                write_frame(wr, &mut b, at).await?;
                return Ok(Some(Session { user, vhost, vhost_name, frame_max, heartbeat, client_properties }));
            }
            HEARTBEAT => {}
            CLOSE => {
                let corr = r.u32()?;
                let at = response(&mut b, CLOSE, corr, OK);
                write_frame(wr, &mut b, at).await?;
                return Ok(None);
            }
            _ => return Ok(None),
        }
    }
}

/// Serve one accepted socket until it closes.
pub(crate) async fn run(server: Arc<Server>, sock: TcpStream, peer: SocketAddr, local: SocketAddr) -> anyhow::Result<()> {
    let broker = server.broker.clone();
    if broker.draining.load(Ordering::Relaxed) {
        return Ok(());
    }
    let (rd, mut wr) = sock.into_split();
    let mut fin = FrameIn { rd, buf: BytesMut::with_capacity(64 << 10) };
    let session = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&server, &mut fin, &mut wr, peer)).await;
    let Ok(Ok(Some(s))) = session else {
        let _ = wr.shutdown().await;
        return match session {
            Ok(Err(e)) => Err(e),
            _ => Ok(()),
        };
    };

    let id = broker.next_id();
    let props: serde_json::Map<String, serde_json::Value> = s.client_properties.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect();
    let name = s.client_properties.iter().find(|(k, _)| k == "connection_name").map(|(_, v)| v.clone());
    let info = Arc::new(ConnectionInfo {
        id,
        name: name.map_or_else(|| format!("{peer} -> {local}"), |n| format!("{peer} -> {local} ({n})")),
        protocol: "stream".into(),
        vhost: s.vhost_name.clone(),
        user: s.user.name.clone(),
        peer,
        local,
        client_properties: serde_json::Value::Object(props),
        connected_at_ms: mq_core::now_ms(),
        tls: false,
        auth_mechanism: "PLAIN".into(),
        frame_max: s.frame_max.into(),
        channel_max: 0.into(),
        heartbeat: s.heartbeat.into(),
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
    let (sac_tx, sac_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        out: out_tx,
        ctl: ctl_tx,
        backlog: AtomicUsize::new(0),
        stalled: AtomicBool::new(false),
        deliver_v2: AtomicBool::new(false),
        info: info.clone(),
    });
    let writer = tokio::spawn(writer_loop(wr, out_rx, shared.clone(), s.heartbeat));
    let max_inflight = server.config.max_unconfirmed.max(1);
    let mut conn = Conn {
        server: server.clone(),
        id,
        user: s.user,
        vhost: s.vhost,
        vhost_name: s.vhost_name,
        frame_max: s.frame_max,
        heartbeat: s.heartbeat,
        shared: shared.clone(),
        sac_tx,
        publishers: HashMap::new(),
        pub_refs: HashSet::new(),
        subs: HashMap::new(),
        pending: HashMap::new(),
        corr: 0,
        closing: None,
        inflight: Arc::new(Semaphore::new(max_inflight)),
        max_inflight,
        resp: BytesMut::with_capacity(4096),
        entries: BytesMut::new(),
        props: BytesMut::with_capacity(256),
        body: Vec::new(),
    };
    let result = conn.serve(&mut fin, ctl_rx, sac_rx).await;
    conn.cleanup().await;
    let _ = shared.out.send(Out::Close);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    broker.connection_closed(id).await;
    result
}

struct Conn {
    server: Arc<Server>,
    id: ConnId,
    user: User,
    vhost: Arc<VHost>,
    vhost_name: String,
    frame_max: u32,
    heartbeat: u32,
    shared: Arc<Shared>,
    sac_tx: mpsc::UnboundedSender<Notice>,
    publishers: HashMap<u8, Publisher>,
    pub_refs: HashSet<(String, Arc<str>)>,
    subs: HashMap<u8, Sub>,
    pending: HashMap<u32, Pending>,
    corr: u32,
    /// Set once the server sent `Close`: until then, wait for the client's response.
    closing: Option<Instant>,
    inflight: Arc<Semaphore>,
    max_inflight: usize,
    resp: BytesMut,
    entries: BytesMut,
    props: BytesMut,
    body: Vec<u8>,
}

impl Conn {
    async fn serve(&mut self, fin: &mut FrameIn, mut ctl: mpsc::UnboundedReceiver<Ctl>, mut sac: mpsc::UnboundedReceiver<Notice>) -> anyhow::Result<()> {
        let mut events = self.server.broker.events.subscribe();
        let mut last_read = Instant::now();
        loop {
            while let Some(next) = fin.next(self.frame_max) {
                match next {
                    Next::TooLarge => {
                        warn!(conn = self.id, "stream frame too large");
                        self.close_with(FRAME_TOO_LARGE, "frame too large");
                        return Ok(());
                    }
                    Next::Frame(f) => {
                        self.shared.info.recv_cnt.fetch_add(1, Ordering::Relaxed);
                        match self.handle(f).await {
                            Ok(Flow::Continue) => {}
                            Ok(Flow::Stop) => return Ok(()),
                            Err(Malformed) => {
                                if self.closing.is_some() {
                                    return Ok(());
                                }
                                self.close_with(UNKNOWN_FRAME, "unknown frame");
                            }
                        }
                    }
                }
            }
            let deadline = match (self.closing, self.heartbeat) {
                (Some(d), _) => d,
                (None, 0) => Instant::now() + Duration::from_secs(3600),
                (None, hb) => last_read + Duration::from_secs(hb as u64 * 3),
            };
            tokio::select! {
                n = fin.fill() => {
                    let n = n?;
                    if n == 0 {
                        return Ok(());
                    }
                    last_read = Instant::now();
                    self.shared.info.recv_oct.fetch_add(n as u64, Ordering::Relaxed);
                }
                Some(c) = ctl.recv() => match c {
                    Ctl::Wake => self.wake_all(),
                    Ctl::StreamGone(s) => self.stream_gone(&s).await,
                },
                Some(n) = sac.recv() => self.on_sac(n).await,
                e = events.recv() => {
                    if let Ok(Event::QueueDeleted { vhost, name }) = e
                        && vhost == self.vhost_name
                    {
                        self.stream_gone(&name).await;
                    }
                }
                _ = self.shared.info.close.notified(), if self.closing.is_none() => {
                    let reason = self.shared.info.close_reason.lock().clone().unwrap_or_else(|| "closed via management plugin".into());
                    self.close_with(OK, &reason);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if self.closing.is_none() {
                        debug!(conn = self.id, "stream heartbeat timeout");
                    }
                    return Ok(());
                }
            }
        }
    }

    fn send(&mut self, at: usize) {
        finish(&mut self.resp, at);
        let f = self.resp.split().freeze();
        let _ = self.shared.out.send(Out::Frame(f));
    }

    fn respond(&mut self, key: u16, corr: u32, code: u16) {
        let at = response(&mut self.resp, key, corr, code);
        self.send(at);
    }

    fn next_corr(&mut self) -> u32 {
        self.corr = self.corr.wrapping_add(1);
        self.corr
    }

    /// Send `Close` and wait (bounded) for the client's response.
    fn close_with(&mut self, code: u16, reason: &str) {
        let corr = self.next_corr();
        let at = begin(&mut self.resp, CLOSE, 1);
        self.resp.put_u32(corr);
        self.resp.put_u16(code);
        put_str(&mut self.resp, reason.as_bytes());
        self.send(at);
        self.closing = Some(Instant::now() + CLOSE_WAIT);
    }

    fn check(&self, access: Access, kind: &str, name: &str) -> bool {
        self.server.broker.check(&self.user, &self.vhost_name, access, kind, name).is_ok()
    }

    fn check_read_or_write(&self, kind: &str, name: &str) -> bool {
        self.check(Access::Read, kind, name) || self.check(Access::Write, kind, name)
    }

    async fn handle(&mut self, frame: Bytes) -> R<Flow> {
        let mut r = Rd::new(&frame);
        let key = r.u16()?;
        let version = r.u16()?;
        if self.closing.is_some() {
            return Ok(match key {
                k if k == CLOSE | RESPONSE => Flow::Stop,
                CLOSE => {
                    let corr = r.u32()?;
                    self.respond(CLOSE, corr, OK);
                    Flow::Stop
                }
                _ => Flow::Continue,
            });
        }
        match key {
            PUBLISH => self.publish(version, r).await?,
            CREDIT => self.credit(r)?,
            STORE_OFFSET => self.store_offset(r)?,
            HEARTBEAT => {}
            k if k == CONSUMER_UPDATE | RESPONSE => self.consumer_update_response(r).await?,
            k if k == TUNE || k == TUNE | RESPONSE || k == HEARTBEAT | RESPONSE => {}
            k if k == CLOSE | RESPONSE => return Ok(Flow::Stop),
            CLOSE => {
                let corr = r.u32()?;
                self.respond(CLOSE, corr, OK);
                return Ok(Flow::Stop);
            }
            k if k & RESPONSE != 0 => {}
            k => {
                let corr = r.u32()?;
                match k {
                    DECLARE_PUBLISHER => self.declare_publisher(corr, r).await?,
                    QUERY_PUBLISHER_SEQUENCE => self.query_publisher_sequence(corr, r).await?,
                    DELETE_PUBLISHER => self.delete_publisher(corr, r).await?,
                    SUBSCRIBE => self.subscribe(corr, r).await?,
                    QUERY_OFFSET => self.query_offset(corr, r).await?,
                    UNSUBSCRIBE => {
                        let id = r.u8()?;
                        let code = if self.subs.contains_key(&id) {
                            self.remove_sub(id, true).await;
                            OK
                        } else {
                            SUBSCRIPTION_ID_DOES_NOT_EXIST
                        };
                        self.respond(UNSUBSCRIBE, corr, code);
                    }
                    CREATE_STREAM => self.create_stream(corr, r).await?,
                    DELETE_STREAM => {
                        let stream = r.string()?;
                        let code = if !self.check(Access::Configure, "queue", &stream) {
                            ACCESS_REFUSED
                        } else {
                            manage::delete_stream(&self.vhost, &stream).await
                        };
                        self.respond(DELETE_STREAM, corr, code);
                        if code == OK {
                            self.stream_gone(&stream).await;
                        }
                    }
                    METADATA => self.metadata(corr, r)?,
                    ROUTE => self.route(corr, r)?,
                    PARTITIONS => self.partitions(corr, r)?,
                    EXCHANGE_COMMAND_VERSIONS => self.command_versions(corr, r)?,
                    STREAM_STATS => self.stream_stats(corr, r).await?,
                    CREATE_SUPER_STREAM => self.create_super_stream(corr, r).await?,
                    DELETE_SUPER_STREAM => self.delete_super_stream(corr, r).await?,
                    RESOLVE_OFFSET_SPEC => self.resolve_offset_spec(corr, r).await?,
                    PEER_PROPERTIES | SASL_HANDSHAKE | SASL_AUTHENTICATE | OPEN => {
                        // Re-authentication is not supported after open.
                        self.respond(k, corr, SASL_ERROR);
                    }
                    _ => return Err(Malformed),
                }
            }
        }
        Ok(Flow::Continue)
    }

    // ── publishing ──

    async fn declare_publisher(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let id = r.u8()?;
        let reference = r.opt_str()?.map(|b| String::from_utf8_lossy(b).into_owned()).filter(|s| !s.is_empty());
        let stream = r.string()?;
        let code = if self.server.broker.alarm.load(Ordering::Relaxed) {
            PRECONDITION_FAILED
        } else if reference.as_ref().is_some_and(|x| x.len() > 255) {
            if self.check(Access::Write, "queue", &stream) { PRECONDITION_FAILED } else { ACCESS_REFUSED }
        } else if !self.check(Access::Write, "queue", &stream) {
            ACCESS_REFUSED
        } else if self.publishers.contains_key(&id)
            || reference.as_ref().is_some_and(|x| self.pub_refs.contains(&(stream.clone(), Arc::from(x.as_str()))))
        {
            PRECONDITION_FAILED
        } else {
            match manage::lookup(&self.vhost, &stream) {
                Err(c) => c,
                Ok(queue) => {
                    let reference: Option<Arc<str>> = reference.map(Arc::from);
                    if let Some(rf) = &reference {
                        // Seed the stream with the sequence kept beyond retention.
                        let seq = self.server.store.sequence(&self.vhost_name, &stream, rf).await;
                        if seq > 0 {
                            let _ = queue.stream_writer_sequence(rf, seq).await;
                        }
                        self.pub_refs.insert((stream.clone(), rf.clone()));
                    }
                    self.publishers.insert(id, Publisher { stream, reference, queue });
                    OK
                }
            }
        };
        self.respond(DECLARE_PUBLISHER, corr, code);
        Ok(())
    }

    async fn query_publisher_sequence(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let reference = r.string()?;
        let stream = r.string()?;
        let (code, seq) = if !self.check(Access::Read, "queue", &stream) {
            (ACCESS_REFUSED, 0)
        } else {
            match manage::lookup(&self.vhost, &stream) {
                Err(c) => (c, 0),
                Ok(q) => {
                    let seed = self.server.store.sequence(&self.vhost_name, &stream, &reference).await;
                    (OK, q.stream_writer_sequence(&reference, seed).await.unwrap_or(seed))
                }
            }
        };
        let at = response(&mut self.resp, QUERY_PUBLISHER_SEQUENCE, corr, code);
        self.resp.put_u64(seq);
        self.send(at);
        Ok(())
    }

    async fn delete_publisher(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let id = r.u8()?;
        let code = match self.publishers.remove(&id) {
            Some(p) => {
                self.forget_publisher(p);
                OK
            }
            None => PUBLISHER_DOES_NOT_EXIST,
        };
        self.respond(DELETE_PUBLISHER, corr, code);
        Ok(())
    }

    /// Drop a publisher, keeping its sequence for deduplication after retention trims it.
    fn forget_publisher(&mut self, p: Publisher) {
        let Some(rf) = p.reference else { return };
        self.pub_refs.remove(&(p.stream.clone(), rf.clone()));
        let (store, vhost) = (self.server.store.clone(), self.vhost_name.clone());
        tokio::spawn(async move {
            if let Ok(seq) = p.queue.stream_writer_sequence(&rf, 0).await
                && seq > 0
            {
                store.store_sequence(&vhost, &p.stream, &rf, seq);
            }
        });
    }

    fn publish_error(&self, publisher: u8, ids: Vec<u64>, code: u16) {
        if !ids.is_empty() {
            let _ = self.shared.out.send(Out::Error { publisher, ids, code });
        }
    }

    /// Encode one message as a native entry into `self.entries`.
    fn encode_entry(&mut self, stream: &str, writer: Option<(&[u8], u64)>, filter: Option<&[u8]>, msg: &[u8], now: u64) -> Bytes {
        let (props, body) = (&mut self.props, &mut self.body);
        let view = match amqp10::convert_in(msg, stream, props, body) {
            Ok(v) => Some(v),
            Err(_) => {
                props.clear();
                props.put_u16(0);
                None
            }
        };
        let nv = NativeView {
            exchange: view.as_ref().map_or("", |v| v.exchange),
            routing_key: view.as_ref().map_or(stream, |v| v.routing_key),
            props_raw: props,
            body: match view.as_ref().map(|v| &v.body) {
                Some(InBody::Slice { offset, len }) => NativeBody::Slice { offset: *offset, len: *len },
                Some(InBody::Concat) => NativeBody::Inline(body),
                None => NativeBody::Slice { offset: 0, len: msg.len() },
            },
        };
        encode_native(&mut self.entries, now, filter, writer, &nv, msg);
        self.entries.split().freeze()
    }

    async fn publish(&mut self, version: u16, mut r: Rd<'_>) -> R<()> {
        let publisher = r.u8()?;
        let count = r.u32()? as usize;
        let Some(p) = self.publishers.get(&publisher) else {
            let mut ids = Vec::new();
            for _ in 0..count {
                ids.push(r.u64()?);
                if version >= 2 {
                    r.opt_str()?;
                }
                skip_entry(&mut r)?;
            }
            self.publish_error(publisher, ids, PUBLISHER_DOES_NOT_EXIST);
            return Ok(());
        };
        let (stream, queue, reference) = (p.stream.clone(), p.queue.clone(), p.reference.clone());
        let now = mq_core::now_ms();
        let cap = count.min(r.remaining() / 12 + 1);
        let (mut ids, mut entries, mut rejected) = (Vec::with_capacity(cap), Vec::with_capacity(cap), Vec::new());
        self.entries.reserve(r.remaining() * 2 + cap * 64);
        for _ in 0..count {
            let id = r.u64()?;
            let filter = if version >= 2 { r.opt_str()? } else { None };
            let writer = reference.as_deref().map(|w| (w.as_bytes(), id));
            let head = r.peek_u32()?;
            if head & 0x8000_0000 == 0 {
                let len = r.u32()? as usize;
                let msg = r.take(len)?;
                entries.push((id, self.encode_entry(&stream, writer, filter, msg, now)));
            } else {
                let flags = r.u8()?;
                let n = r.u16()?;
                let uncompressed = r.u32()? as usize;
                let size = r.u32()? as usize;
                let data = r.take(size)?;
                // Compressed batches are unpacked here and stored as plain entries, so every
                // reader (0-9-1, AMQP 1.0, streams) sees ordinary messages.
                let data: std::borrow::Cow<'_, [u8]> = match (flags >> 4) & 0x7 {
                    0 => data.into(),
                    codec => match decompress(codec, data, uncompressed) {
                        Some(d) => d.into(),
                        None => {
                            rejected.push(id);
                            continue;
                        }
                    },
                };
                let mut sub = Rd::new(&data);
                for _ in 0..n {
                    let len = (sub.u32()? & 0x7fff_ffff) as usize;
                    let msg = sub.take(len)?;
                    entries.push((id, self.encode_entry(&stream, writer, filter, msg, now)));
                }
            }
            ids.push(id);
        }
        self.publish_error(publisher, rejected, PRECONDITION_FAILED);
        if ids.is_empty() {
            return Ok(());
        }
        let bytes: usize = entries.iter().map(|(_, e)| e.len()).sum();
        self.vhost.throttle(entries.len() as u64, bytes as u64).await;
        let permits = ids.len().min(self.max_inflight) as u32;
        if let Ok(p) = self.inflight.clone().acquire_many_owned(permits).await {
            p.forget();
        }
        let (out, sem) = (self.shared.out.clone(), self.inflight.clone());
        let confirm = ConfirmTicket::new(move |ok| {
            sem.add_permits(permits as usize);
            let _ = out.send(if ok { Out::Confirm { publisher, ids } } else { Out::Error { publisher, ids, code: STREAM_NOT_AVAILABLE } });
        });
        // A failed send drops the ticket, which reports the error.
        let _ = queue.stream_publish(NativePublish { writer: reference, entries, confirm }).await;
        Ok(())
    }

    // ── consuming ──

    async fn subscribe(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let id = r.u8()?;
        let stream = r.string()?;
        let spec = offset_spec(&mut r)?.unwrap_or(StreamOffset::Next);
        let credit = r.u16()?;
        let props = r.opt_map()?;
        let prop = |k: &str| props.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str());
        let sac = prop("single-active-consumer") == Some("true");
        let name = prop("name").map(str::to_string);
        let readable = self.check(Access::Read, "queue", &stream);
        let code = if name.as_ref().is_some_and(|n| n.len() > 255) {
            if readable { PRECONDITION_FAILED } else { ACCESS_REFUSED }
        } else if !readable {
            ACCESS_REFUSED
        } else {
            match manage::lookup(&self.vhost, &stream) {
                Err(c) => c,
                Ok(_) if self.subs.contains_key(&id) => SUBSCRIPTION_ID_ALREADY_EXISTS,
                Ok(_) if sac && name.is_none() => PRECONDITION_FAILED,
                Ok(queue) => {
                    let filters: Vec<Bytes> = props.iter().filter(|(k, _)| k.starts_with("filter.")).map(|(_, v)| Bytes::from(v.clone())).collect();
                    let mut properties = FieldTable::new();
                    for (k, v) in &props {
                        properties.insert(k.as_str(), v.as_str());
                    }
                    let max_bytes = match self.frame_max {
                        0 => 1 << 20,
                        f => (f as usize).saturating_sub(256).max(1024),
                    };
                    let sink = Arc::new(SubSink {
                        id,
                        stream: stream.clone(),
                        credit: AtomicI64::new(0),
                        max_bytes,
                        live: Arc::new(AtomicBool::new(true)),
                        shared: self.shared.clone(),
                    });
                    let sac_key = sac.then(|| (self.vhost_name.clone(), stream.clone(), name.clone().unwrap_or_default()));
                    let mut sub = Sub {
                        stream: stream.clone(),
                        queue,
                        spec,
                        filters: (!filters.is_empty()).then_some(filters),
                        match_unfiltered: prop("match-unfiltered") == Some("true"),
                        properties,
                        tag: Arc::from(name.clone().unwrap_or_else(|| id.to_string())),
                        sink,
                        reg: None,
                        sac: sac_key.clone(),
                        active: !sac,
                    };
                    let start = sub.spec.clone();
                    let ok = sac || self.register(&mut sub, start).await;
                    if ok {
                        self.respond(SUBSCRIBE, corr, OK);
                        sub.sink.credit.store(credit as i64, Ordering::Release);
                        sub.queue.wake();
                        self.subs.insert(id, sub);
                        if let Some(key) = sac_key {
                            let partition = prop("super-stream").map_or(-1, |ss| manage::partition_index(&self.vhost, ss, &stream));
                            let active = self.server.sac.register(key, partition, self.id, id, self.sac_tx.clone());
                            if let Some(s) = self.subs.get_mut(&id) {
                                s.active = active;
                            }
                        }
                        return Ok(());
                    }
                    STREAM_DOES_NOT_EXIST
                }
            }
        };
        self.respond(SUBSCRIBE, corr, code);
        Ok(())
    }

    /// Register a subscription with its stream actor, reading from `offset`.
    async fn register(&self, sub: &mut Sub, offset: StreamOffset) -> bool {
        let reg = self.server.broker.next_id();
        let cs = ChunkSubscription {
            id: reg,
            conn: self.id,
            tag: sub.tag.clone(),
            offset,
            filters: sub.filters.clone(),
            match_unfiltered: sub.match_unfiltered,
            properties: sub.properties.clone(),
            sink: sub.sink.clone(),
        };
        match sub.queue.stream_subscribe(cs).await {
            Ok(_) => {
                sub.reg = Some(reg);
                true
            }
            Err(_) => false,
        }
    }

    fn credit(&mut self, mut r: Rd<'_>) -> R<()> {
        let id = r.u8()?;
        let credit = r.i16()?;
        let code = match self.subs.get(&id) {
            None => SUBSCRIPTION_ID_DOES_NOT_EXIST,
            Some(s) => {
                s.sink.credit.fetch_add(credit as i64, Ordering::AcqRel);
                if s.reg.is_some() {
                    s.queue.wake();
                    OK
                } else {
                    // Credit for an inactive single active consumer.
                    PRECONDITION_FAILED
                }
            }
        };
        if code != OK {
            let at = begin(&mut self.resp, CREDIT | RESPONSE, 1);
            self.resp.put_u16(code);
            self.resp.put_u8(id);
            self.send(at);
        }
        Ok(())
    }

    /// Remove a subscription; `notify_sac` also hands over its group.
    async fn remove_sub(&mut self, id: u8, notify_sac: bool) {
        let Some(sub) = self.subs.remove(&id) else { return };
        sub.sink.live.store(false, Ordering::Release);
        if let Some(reg) = sub.reg {
            sub.queue.stream_unsubscribe(reg).await;
        }
        if let Some(key) = sub.sac {
            self.server.sac.unregister(&key, self.id, id);
            let stepping: Vec<u32> = self.pending.iter().filter(|(_, p)| p.sub == id && !p.active && p.stepping_down).map(|(c, _)| *c).collect();
            for c in stepping {
                self.pending.remove(&c);
                if notify_sac {
                    self.server.sac.activate(&key);
                }
            }
        }
    }

    async fn on_sac(&mut self, n: Notice) {
        let key: SacKey = (self.vhost_name.clone(), n.stream.clone(), n.name.clone());
        let Some(sub) = self.subs.get_mut(&n.sub).filter(|s| s.sac.as_ref() == Some(&key)) else {
            if !n.active && n.stepping_down {
                self.server.sac.activate(&key);
            }
            return;
        };
        sub.active = n.active;
        let reg = if n.active { None } else { sub.reg.take() };
        let queue = sub.queue.clone();
        if let Some(reg) = reg {
            queue.stream_unsubscribe(reg).await;
        }
        let corr = self.next_corr();
        self.pending.insert(corr, Pending { sub: n.sub, active: n.active, stepping_down: n.stepping_down, key });
        let at = begin(&mut self.resp, CONSUMER_UPDATE, 1);
        self.resp.put_u32(corr);
        self.resp.put_u8(n.sub);
        self.resp.put_u8(n.active as u8);
        self.send(at);
    }

    async fn consumer_update_response(&mut self, mut r: Rd<'_>) -> R<()> {
        let corr = r.u32()?;
        let _code = r.u16()?;
        let spec = offset_spec(&mut r)?;
        let Some(p) = self.pending.remove(&corr) else { return Ok(()) };
        let Some(mut sub) = self.subs.remove(&p.sub) else { return Ok(()) };
        if sub.active && sub.reg.is_none() {
            // Re-check: the coordinator may have moved on since the notice.
            if self.server.sac.is_active(&p.key, self.id, p.sub) {
                let offset = spec.unwrap_or_else(|| sub.spec.clone());
                if !self.register(&mut sub, offset).await {
                    debug!(conn = self.id, stream = %sub.stream, "activating single active consumer failed");
                }
            }
        } else if !sub.active && p.stepping_down {
            self.server.sac.activate(&p.key);
        }
        self.subs.insert(p.sub, sub);
        Ok(())
    }

    fn wake_all(&self) {
        for s in self.subs.values().filter(|s| s.reg.is_some()) {
            s.queue.wake();
        }
    }

    /// A stream went away: drop what uses it and tell the client (`MetadataUpdate`).
    async fn stream_gone(&mut self, stream: &str) {
        let current = manage::lookup(&self.vhost, stream).ok();
        let gone = |q: &QueueRef| q.is_deleted() || current.as_ref().is_none_or(|c| !Arc::ptr_eq(c, q));
        let subs: Vec<u8> = self.subs.iter().filter(|(_, s)| s.stream == stream && gone(&s.queue)).map(|(i, _)| *i).collect();
        let pubs: Vec<u8> = self.publishers.iter().filter(|(_, p)| p.stream == stream && gone(&p.queue)).map(|(i, _)| *i).collect();
        if subs.is_empty() && pubs.is_empty() {
            return;
        }
        for id in subs {
            self.remove_sub(id, true).await;
        }
        for id in pubs {
            if let Some(p) = self.publishers.remove(&id)
                && let Some(rf) = p.reference
            {
                self.pub_refs.remove(&(p.stream, rf));
            }
        }
        let at = begin(&mut self.resp, METADATA_UPDATE, 1);
        self.resp.put_u16(STREAM_NOT_AVAILABLE);
        put_str(&mut self.resp, stream.as_bytes());
        self.send(at);
    }

    fn store_offset(&mut self, mut r: Rd<'_>) -> R<()> {
        let reference = r.string()?;
        let stream = r.string()?;
        let offset = r.u64()?;
        if reference.is_empty() || reference.len() > 255 {
            return Ok(());
        }
        let subscribed = self.subs.values().any(|s| s.stream == stream);
        if (subscribed || self.check(Access::Read, "queue", &stream)) && manage::lookup(&self.vhost, &stream).is_ok() {
            self.server.store.store_offset(&self.vhost_name, &stream, &reference, offset);
        }
        Ok(())
    }

    async fn query_offset(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let reference = r.string()?;
        let stream = r.string()?;
        let (code, offset) = if !self.check(Access::Read, "queue", &stream) {
            (ACCESS_REFUSED, 0)
        } else if let Err(c) = manage::lookup(&self.vhost, &stream) {
            (c, 0)
        } else {
            match self.server.store.query_offset(&self.vhost_name, &stream, &reference).await {
                Some(o) => (OK, o),
                None => (NO_OFFSET, 0),
            }
        };
        let at = response(&mut self.resp, QUERY_OFFSET, corr, code);
        self.resp.put_u64(offset);
        self.send(at);
        Ok(())
    }

    async fn resolve_offset_spec(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let stream = r.string()?;
        let spec = offset_spec(&mut r)?;
        let _props = r.opt_map()?;
        let (code, offset) = if !self.check(Access::Read, "queue", &stream) {
            (ACCESS_REFUSED, 0)
        } else {
            match manage::lookup(&self.vhost, &stream) {
                Err(c) => (c, 0),
                Ok(q) => match q.stream_resolve(spec.unwrap_or(StreamOffset::Next)).await {
                    Ok(o) => (OK, o),
                    Err(_) => (NO_OFFSET, 0),
                },
            }
        };
        let at = response(&mut self.resp, RESOLVE_OFFSET_SPEC, corr, code);
        self.resp.put_u16(OFFSET_OFFSET);
        self.resp.put_u64(offset);
        self.send(at);
        Ok(())
    }

    // ── streams and super streams ──

    async fn create_stream(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let name = r.string()?;
        let args = r.opt_map()?;
        let code = match manage::enforce_name(&name) {
            None => PRECONDITION_FAILED,
            Some(name) if !self.check(Access::Configure, "queue", &name) => ACCESS_REFUSED,
            Some(name) => match manage::stream_args(&args) {
                Err(()) => PRECONDITION_FAILED,
                Ok(t) => manage::create_stream(&self.vhost, &name, t).await,
            },
        };
        self.respond(CREATE_STREAM, corr, code);
        Ok(())
    }

    fn metadata(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let streams = r.strings()?;
        // Broker 0 is this node; streams owned by other cluster nodes name theirs as leader.
        let mut brokers: Vec<(String, u32)> = Vec::new();
        let mut broker_ref = |host: String, port: u32| match brokers.iter().position(|b| b.0 == host && b.1 == port) {
            Some(i) => i as u16,
            None => {
                brokers.push((host, port));
                (brokers.len() - 1) as u16
            }
        };
        let here = (self.server.config.advertised_host.clone(), self.server.config.advertised_port as u32);
        // (code, leader, replica): a stream owned by another node lists that node as its replica
        // too, because clients read from a replica (or, with none, from the node they asked).
        let answers: Vec<(u16, u16, Option<u16>)> = streams
            .iter()
            .map(|s| {
                if !self.check_read_or_write("queue", s) {
                    return (ACCESS_REFUSED, 0xffff, None);
                }
                match manage::find(&self.vhost, s) {
                    Ok(manage::Found::Here(_)) => (OK, broker_ref(here.0.clone(), here.1), None),
                    Ok(manage::Found::Elsewhere(Some(addr))) => {
                        let r = broker_ref(addr.ip().to_string(), addr.port() as u32);
                        (OK, r, Some(r))
                    }
                    Ok(manage::Found::Elsewhere(None)) => (STREAM_NOT_AVAILABLE, 0xffff, None),
                    Err(c) => (c, 0xffff, None),
                }
            })
            .collect();
        let at = begin(&mut self.resp, METADATA | RESPONSE, 1);
        self.resp.put_u32(corr);
        self.resp.put_u32(brokers.len() as u32);
        for (i, (host, port)) in brokers.iter().enumerate() {
            self.resp.put_u16(i as u16);
            put_str(&mut self.resp, host.as_bytes());
            self.resp.put_u32(*port);
        }
        self.resp.put_u32(streams.len() as u32);
        for (s, (code, leader, replica)) in streams.iter().zip(answers) {
            put_str(&mut self.resp, s.as_bytes());
            self.resp.put_u16(code);
            self.resp.put_u16(leader);
            match replica {
                Some(r) => {
                    self.resp.put_u32(1);
                    self.resp.put_u16(r);
                }
                None => self.resp.put_u32(0),
            }
        }
        self.send(at);
        Ok(())
    }

    fn put_streams(&mut self, key: u16, corr: u32, code: u16, streams: &[String]) {
        let at = response(&mut self.resp, key, corr, code);
        self.resp.put_u32(streams.len() as u32);
        for s in streams {
            put_str(&mut self.resp, s.as_bytes());
        }
        self.send(at);
    }

    fn route(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let routing_key = r.string()?;
        let super_stream = r.string()?;
        let (code, streams) = if !self.check(Access::Write, "exchange", &super_stream) {
            (ACCESS_REFUSED, vec![])
        } else {
            match manage::route(&self.vhost, &super_stream, &routing_key) {
                Err(c) => (c, vec![]),
                Ok(s) if s.iter().all(|x| self.check(Access::Write, "queue", x)) => (OK, s),
                Ok(_) => (ACCESS_REFUSED, vec![]),
            }
        };
        self.put_streams(ROUTE, corr, code, &streams);
        Ok(())
    }

    fn partitions(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let super_stream = r.string()?;
        let (code, streams) = if !self.check_read_or_write("exchange", &super_stream) {
            (ACCESS_REFUSED, vec![])
        } else {
            match manage::partitions(&self.vhost, &super_stream) {
                Err(c) => (c, vec![]),
                Ok(s) if s.iter().all(|x| self.check_read_or_write("queue", x)) => (OK, s),
                Ok(_) => (ACCESS_REFUSED, vec![]),
            }
        };
        self.put_streams(PARTITIONS, corr, code, &streams);
        Ok(())
    }

    fn command_versions(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let n = r.count()?;
        for _ in 0..n {
            let (key, _min, max) = (r.u16()?, r.u16()?, r.u16()?);
            if key == DELIVER && max >= 2 {
                self.shared.deliver_v2.store(true, Ordering::Release);
            }
        }
        let at = response(&mut self.resp, EXCHANGE_COMMAND_VERSIONS, corr, OK);
        self.resp.put_u32(COMMAND_VERSIONS.len() as u32);
        for (k, min, max) in COMMAND_VERSIONS {
            self.resp.put_u16(*k);
            self.resp.put_u16(*min);
            self.resp.put_u16(*max);
        }
        self.send(at);
        Ok(())
    }

    async fn stream_stats(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let stream = r.string()?;
        let (code, stats) = if !self.check(Access::Read, "queue", &stream) {
            (ACCESS_REFUSED, None)
        } else {
            match manage::lookup(&self.vhost, &stream) {
                Err(c) => (c, None),
                Ok(q) => match q.stream_stats().await {
                    Ok(s) => (OK, Some(s)),
                    Err(_) => (STREAM_NOT_AVAILABLE, None),
                },
            }
        };
        let at = response(&mut self.resp, STREAM_STATS, corr, code);
        match stats {
            Some(s) => {
                let (first, last) = if s.next > s.first { (s.first as i64, s.next as i64 - 1) } else { (-1, -1) };
                self.resp.put_u32(4);
                for (k, v) in [("committed_chunk_id", last), ("first_chunk_id", first), ("last_chunk_id", last), ("committed_offset", last)] {
                    put_str(&mut self.resp, k.as_bytes());
                    self.resp.put_i64(v);
                }
            }
            None => self.resp.put_u32(0),
        }
        self.send(at);
        Ok(())
    }

    async fn create_super_stream(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let name = r.string()?;
        let partitions = r.strings()?;
        let keys = r.strings()?;
        let args = r.opt_map()?;
        let code = match manage::enforce_name(&name) {
            None => PRECONDITION_FAILED,
            Some(name) => {
                let allowed = self.check(Access::Configure, "exchange", &name)
                    && self.check(Access::Write, "exchange", &name)
                    && partitions.iter().all(|p| self.check(Access::Configure, "queue", p) && self.check(Access::Read, "exchange", &name));
                if allowed { manage::create_super_stream(&self.vhost, &name, &partitions, &keys, &args).await } else { ACCESS_REFUSED }
            }
        };
        self.respond(CREATE_SUPER_STREAM, corr, code);
        Ok(())
    }

    async fn delete_super_stream(&mut self, corr: u32, mut r: Rd<'_>) -> R<()> {
        let name = r.string()?;
        let mut deleted = Vec::new();
        let code = if !self.check(Access::Configure, "exchange", &name) {
            ACCESS_REFUSED
        } else {
            match manage::partitions(&self.vhost, &name) {
                Err(c) => c,
                Ok(parts) if parts.iter().all(|p| self.check(Access::Configure, "queue", p)) => {
                    let code = manage::delete_super_stream(&self.vhost, &name, &parts).await;
                    deleted = parts;
                    code
                }
                Ok(_) => ACCESS_REFUSED,
            }
        };
        self.respond(DELETE_SUPER_STREAM, corr, code);
        for p in deleted {
            self.stream_gone(&p).await;
        }
        Ok(())
    }

    /// Release everything when the connection ends.
    async fn cleanup(&mut self) {
        let ids: Vec<u8> = self.subs.keys().copied().collect();
        for id in ids {
            self.remove_sub(id, true).await;
        }
        let pubs: Vec<Publisher> = self.publishers.drain().map(|(_, p)| p).collect();
        for p in pubs {
            self.forget_publisher(p);
        }
    }
}

/// Largest sub-entry batch we inflate (its declared uncompressed size).
const MAX_UNCOMPRESSED: usize = 64 << 20;

/// Inflate a compressed sub-entry batch (the client's codecs: gzip, Snappy framed, LZ4 frame,
/// zstd); `None` if it is corrupt, unknown or not the size it claims.
fn decompress(codec: u8, data: &[u8], uncompressed: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    if uncompressed > MAX_UNCOMPRESSED {
        return None;
    }
    let reader: Box<dyn Read + '_> = match codec {
        1 => Box::new(flate2::read::GzDecoder::new(data)),
        2 => Box::new(snap::read::FrameDecoder::new(data)),
        3 => Box::new(lz4_flex::frame::FrameDecoder::new(data)),
        4 => Box::new(zstd::stream::read::Decoder::new(data).ok()?),
        _ => return None,
    };
    let mut out = Vec::with_capacity(uncompressed);
    // One byte past the declared size tells a lying header from an exact one.
    reader.take(uncompressed as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() == uncompressed).then_some(out)
}

/// Skip a published entry (simple or sub-batch).
fn skip_entry(r: &mut Rd<'_>) -> R<()> {
    let head = r.u32()?;
    if head & 0x8000_0000 == 0 {
        r.take(head as usize)?;
    } else {
        // flags(1) count(2) were part of `head`; then uncompressed size, size, data.
        let _uncompressed = r.u32()?;
        let size = r.u32()? as usize;
        r.take(size)?;
    }
    Ok(())
}

fn offset_spec(r: &mut Rd<'_>) -> R<Option<StreamOffset>> {
    Ok(Some(match r.u16()? {
        OFFSET_NONE => return Ok(None),
        OFFSET_FIRST => StreamOffset::First,
        OFFSET_LAST => StreamOffset::Last,
        OFFSET_NEXT => StreamOffset::Next,
        OFFSET_OFFSET => StreamOffset::Offset(r.u64()?),
        OFFSET_TIMESTAMP => StreamOffset::Timestamp(r.i64()?.max(0) as u64),
        _ => return Err(Malformed),
    }))
}

/// Encode a chunk as a `Deliver` frame: an osiris user chunk of simple entries.
fn encode_chunk(buf: &mut BytesMut, sub: u8, v2: bool, c: &Chunk) {
    let at = begin(buf, DELIVER, if v2 { 2 } else { 1 });
    buf.put_u8(sub);
    if v2 {
        buf.put_u64(c.committed);
    }
    let hdr = buf.len();
    buf.put_bytes(0, 48);
    let data = buf.len();
    for e in &c.entries {
        let p = buf.len();
        buf.put_u32(0);
        match e {
            Entry::Native(n) => buf.put_slice(&n.amqp),
            Entry::Msg(m) => amqp10::encode_message(m, buf),
        }
        let n = ((buf.len() - p - 4) as u32) & 0x7fff_ffff;
        buf[p..p + 4].copy_from_slice(&n.to_be_bytes());
    }
    let data_len = (buf.len() - data) as u32;
    let crc = crc32fast::hash(&buf[data..]);
    let n = c.entries.len();
    let ts = c.entries.first().map_or(0, Entry::received_ms);
    let h = &mut buf[hdr..hdr + 48];
    h[0] = 0x51;
    h[1] = 0;
    h[2..4].copy_from_slice(&(n as u16).to_be_bytes());
    h[4..8].copy_from_slice(&(n as u32).to_be_bytes());
    h[8..16].copy_from_slice(&ts.to_be_bytes());
    h[16..24].copy_from_slice(&1u64.to_be_bytes());
    h[24..32].copy_from_slice(&c.first.to_be_bytes());
    h[32..36].copy_from_slice(&crc.to_be_bytes());
    h[36..40].copy_from_slice(&data_len.to_be_bytes());
    finish(buf, at);
}

/// Returns the backlog estimate the item accounted for.
fn encode_out(buf: &mut BytesMut, item: Out, shared: &Shared) -> (usize, bool) {
    match item {
        Out::Frame(f) => buf.put_slice(&f),
        Out::Chunk { sub, live, chunk, est } => {
            if live.load(Ordering::Acquire) {
                encode_chunk(buf, sub, shared.deliver_v2.load(Ordering::Relaxed), &chunk);
            }
            return (est, false);
        }
        Out::Confirm { publisher, ids } => {
            let at = begin(buf, PUBLISH_CONFIRM, 1);
            buf.put_u8(publisher);
            buf.put_u32(ids.len() as u32);
            for id in ids {
                buf.put_u64(id);
            }
            finish(buf, at);
        }
        Out::Error { publisher, ids, code } => {
            let at = begin(buf, PUBLISH_ERROR, 1);
            buf.put_u8(publisher);
            buf.put_u32(ids.len() as u32);
            for id in ids {
                buf.put_u64(id);
                buf.put_u16(code);
            }
            finish(buf, at);
        }
        Out::Close => return (0, true),
    }
    (0, false)
}

async fn writer_loop(mut wr: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Out>, shared: Arc<Shared>, heartbeat: u32) {
    let mut buf = BytesMut::with_capacity(256 << 10);
    let idle = Duration::from_secs(heartbeat.max(1) as u64);
    loop {
        let first = if heartbeat > 0 {
            match tokio::time::timeout(idle, rx.recv()).await {
                Ok(x) => x,
                Err(_) => {
                    let at = begin(&mut buf, HEARTBEAT, 1);
                    finish(&mut buf, at);
                    if wr.write_all(&buf).await.is_err() {
                        return;
                    }
                    buf.clear();
                    continue;
                }
            }
        } else {
            rx.recv().await
        };
        let Some(first) = first else { break };
        let (mut est, mut done) = encode_out(&mut buf, first, &shared);
        let mut frames = 1u64;
        while !done && buf.len() < WRITE_BATCH {
            let Ok(item) = rx.try_recv() else { break };
            let (e, d) = encode_out(&mut buf, item, &shared);
            est += e;
            done = d;
            frames += 1;
        }
        if !buf.is_empty() {
            if let Err(e) = wr.write_all(&buf).await {
                debug!(error = %e, "stream write failed");
                break;
            }
            shared.info.send_oct.fetch_add(buf.len() as u64, Ordering::Relaxed);
            shared.info.send_cnt.fetch_add(frames, Ordering::Relaxed);
            if buf.capacity() > 4 * WRITE_BATCH {
                buf = BytesMut::with_capacity(256 << 10);
            } else {
                buf.clear();
            }
        }
        if est > 0 {
            let left = shared.backlog.fetch_sub(est, Ordering::Relaxed).saturating_sub(est);
            if left < BACKLOG_LOW && shared.stalled.swap(false, Ordering::AcqRel) {
                let _ = shared.ctl.send(Ctl::Wake);
            }
        }
        if done {
            break;
        }
    }
    let _ = wr.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mq_core::message::{Message, Properties};

    #[test]
    fn decompresses_every_client_codec() {
        use std::io::Write;
        let plain: Vec<u8> = (0..10_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        let gzip = {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&plain).unwrap();
            e.finish().unwrap()
        };
        let snappy = {
            let mut e = snap::write::FrameEncoder::new(Vec::new());
            e.write_all(&plain).unwrap();
            e.into_inner().unwrap()
        };
        let lz4 = {
            let mut e = lz4_flex::frame::FrameEncoder::new(Vec::new());
            e.write_all(&plain).unwrap();
            e.finish().unwrap()
        };
        let zstd = zstd::encode_all(&plain[..], 3).unwrap();
        for (codec, data) in [(1, gzip), (2, snappy), (3, lz4), (4, zstd)] {
            assert_eq!(decompress(codec, &data, plain.len()).as_deref(), Some(&plain[..]), "codec {codec}");
            // A header that lies about the size is refused, either way.
            assert_eq!(decompress(codec, &data, plain.len() - 1), None, "codec {codec} short");
            assert_eq!(decompress(codec, &data, plain.len() + 1), None, "codec {codec} long");
        }
        assert_eq!(decompress(5, b"x", 1), None);
        assert_eq!(decompress(1, b"not gzip", 8), None);
        assert_eq!(decompress(1, b"", MAX_UNCOMPRESSED + 1), None);
    }

    #[test]
    fn chunk_layout_and_crc() {
        let m = Arc::new(Message::new("", "s", Properties::default(), Bytes::from_static(b"abc")));
        let c = Chunk { first: 10, entries: vec![Entry::Msg(m.clone()), Entry::Msg(m)], committed: 11 };
        let mut b = BytesMut::new();
        encode_chunk(&mut b, 3, true, &c);
        let mut r = Rd::new(&b);
        assert_eq!(r.u32().unwrap() as usize, b.len() - 4);
        assert_eq!((r.u16().unwrap(), r.u16().unwrap(), r.u8().unwrap(), r.u64().unwrap()), (DELIVER, 2, 3, 11));
        let h = r.take(48).unwrap();
        assert_eq!((h[0], u16::from_be_bytes([h[2], h[3]])), (0x51, 2));
        assert_eq!(u64::from_be_bytes(h[24..32].try_into().unwrap()), 10);
        let data_len = u32::from_be_bytes(h[36..40].try_into().unwrap()) as usize;
        assert_eq!(data_len, r.remaining());
        assert_eq!(u32::from_be_bytes(h[32..36].try_into().unwrap()), crc32fast::hash(r.buf));
        let len = r.u32().unwrap() as usize;
        let mut props = BytesMut::new();
        let v = amqp10::convert_in(r.take(len).unwrap(), "s", &mut props, &mut Vec::new()).unwrap();
        assert_eq!(v.routing_key, "s");
    }
}

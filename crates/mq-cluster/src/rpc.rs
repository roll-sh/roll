//! Internal RPC between brokers: length-prefixed frames carrying a JSON header plus raw blobs
//! (messages, field tables), multiplexed over one connection per node pair. Requests are applied
//! in the order they arrive, so publishes to a queue keep their order across nodes.
//!
//! The connection is an HTTP/1.1 upgrade of `GET /internal/queues/rpc` on the other node's
//! internal API (its roll-cluster advertise URL: 7070 on the fleet), signed with the cluster
//! secret like every internal hop. The answer (101) names the node's client listeners.
//!
//! Frame: `u32 len | u8 kind | u64 id | u32 head_len | head (JSON) | u16 blobs | (u32 len | bytes)*`.
//!
//! A node that dies doesn't close its connections (a killed machine sends no FIN or RST), so a
//! request in flight to it would wait for TCP to give up, minutes later. Both ends watch the
//! connection instead: the connecting node pings every [`PING_EVERY`] and drops a connection that
//! brought nothing back for [`DEAD_AFTER`] (its requests then read as `Moved` and go to the next
//! owner, its subscriptions register again there); the other end drops a pinging peer that went
//! as quiet, so the deliveries it held go back to their queues.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use mq_core::AmqpError;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The internal route other nodes open the RPC connection on.
pub const PATH: &str = "/internal/queues/rpc";
/// The `Upgrade` protocol of [`PATH`].
pub const PROTOCOL: &str = "roll-queues-rpc";
/// The 101's header naming the answering node's client listeners (JSON: protocol → address).
pub const LISTENERS_HEADER: &str = "x-roll-queues-listeners";

pub const REQ: u8 = 1;
pub const RESP: u8 = 2;
pub const PUSH: u8 = 3;
const MAX_FRAME: usize = 256 << 20;
/// How often a connecting node pings the other end.
pub const PING_EVERY: Duration = Duration::from_secs(2);
/// A connection silent this long is dead (less than a lease, so it's dropped before the next
/// owner can take over).
pub const DEAD_AFTER: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: u8,
    pub id: u64,
    pub head: Value,
    pub blobs: Vec<Bytes>,
}

impl Frame {
    pub fn encode(&self, out: &mut BytesMut) {
        let head = serde_json::to_vec(&self.head).unwrap_or_default();
        let len = 1 + 8 + 4 + head.len() + 2 + self.blobs.iter().map(|b| 4 + b.len()).sum::<usize>();
        out.reserve(4 + len);
        out.put_u32(len as u32);
        out.put_u8(self.kind);
        out.put_u64(self.id);
        out.put_u32(head.len() as u32);
        out.put_slice(&head);
        out.put_u16(self.blobs.len() as u16);
        for b in &self.blobs {
            out.put_u32(b.len() as u32);
            out.put_slice(b);
        }
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Frame>> {
        let len = match r.read_u32().await {
            Ok(n) => n as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        };
        if !(15..=MAX_FRAME).contains(&len) {
            return Err(std::io::Error::other(format!("bad frame length {len}")));
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await?;
        let mut b = Bytes::from(buf);
        let bad = || std::io::Error::other("truncated frame");
        let kind = b.get_u8();
        let id = b.get_u64();
        let hl = b.get_u32() as usize;
        if b.remaining() < hl + 2 {
            return Err(bad());
        }
        let head = serde_json::from_slice(&b.split_to(hl)).map_err(std::io::Error::other)?;
        let n = b.get_u16();
        let mut blobs = Vec::with_capacity(n as usize);
        for _ in 0..n {
            if b.remaining() < 4 {
                return Err(bad());
            }
            let l = b.get_u32() as usize;
            if b.remaining() < l {
                return Err(bad());
            }
            blobs.push(b.split_to(l));
        }
        Ok(Some(Frame { kind, id, head, blobs }))
    }
}

/// Requests a node sends to the owner of a queue (`q` is the cluster queue key).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    /// blob 0: the message.
    Publish { q: String, wait: bool, confirm: bool },
    /// blob 0: consumer arguments (field table).
    Consume { q: String, sub: u64, channel: u64, conn: u64, tag: String, no_ack: bool, exclusive: bool, priority: i32, credit: u32 },
    Credit { sub: u64, n: u32 },
    Cancel { q: String, sub: u64, channel: u64, tag: String },
    Get { q: String, channel: u64, no_ack: bool },
    Settle { q: String, channel: u64, seqs: Vec<u64>, outcome: u8 },
    Release { q: String, channel: u64, seqs: Vec<u64> },
    /// blob 0: annotations to merge (field table).
    Modify { q: String, channel: u64, seqs: Vec<u64>, failed: bool, undeliverable: bool },
    ChannelClosed { q: String, channel: u64 },
    Recover { q: String, channel: u64 },
    Purge { q: String },
    Delete { q: String, if_unused: bool, if_empty: bool },
    Lease { q: String, max: usize, visibility_ms: u64 },
    SettleLease { q: String, seq: u64, delivery_count: u32, outcome: u8 },
    ExtendLease { q: String, seq: u64, delivery_count: u32, visibility_ms: u64 },
    Peek { q: String, offset: usize, count: usize },
    Touch { q: String },
    Consumers { q: String },
    Stats { qs: Vec<String> },
    /// A stream-protocol publish. blobs: the entries (as stored), one per id in `ids`.
    StreamPublish { q: String, writer: Option<String>, ids: Vec<u64> },
    /// A stream-protocol subscription reading from `offset`, fed by credit (in chunks). blob 0:
    /// its properties (field table); with `filtered`, blobs 1.. are the filter values.
    StreamSubscribe { q: String, sub: u64, conn: u64, tag: String, offset: Offset, filtered: bool, match_unfiltered: bool, credit: u32, max_bytes: usize },
    StreamUnsubscribe { q: String, sub: u64 },
    StreamResolve { q: String, offset: Offset },
    StreamWriterSequence { q: String, writer: String, seed: u64 },
    StreamStats { q: String },
    /// Liveness (answered with an empty `ok`; a node that doesn't know it answers an error, which
    /// shows it's alive just as well).
    Ping,
}

/// A stream offset specification on the wire (`StreamOffset`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Offset {
    First,
    Last,
    Next,
    At(u64),
    /// Unix ms.
    Time(u64),
}

impl From<&mq_core::queue::StreamOffset> for Offset {
    fn from(o: &mq_core::queue::StreamOffset) -> Self {
        use mq_core::queue::StreamOffset as S;
        match *o {
            S::First => Offset::First,
            S::Last => Offset::Last,
            S::Next => Offset::Next,
            S::Offset(n) => Offset::At(n),
            S::Timestamp(t) => Offset::Time(t),
        }
    }
}

impl From<Offset> for mq_core::queue::StreamOffset {
    fn from(o: Offset) -> Self {
        use mq_core::queue::StreamOffset as S;
        match o {
            Offset::First => S::First,
            Offset::Last => S::Last,
            Offset::Next => S::Next,
            Offset::At(n) => S::Offset(n),
            Offset::Time(t) => S::Timestamp(t),
        }
    }
}

/// Events the owner pushes to a subscribed consumer's node.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Push {
    /// blob 0: the message.
    Deliver { sub: u64, seq: u64, redelivered: bool, delivery_count: u32, remaining: u32 },
    Cancelled { sub: u64 },
    Activated { sub: u64, active: bool },
    TimedOut { sub: u64, timeout_ms: u64 },
    /// The queue moved to another node: subscribe there.
    Moved { sub: u64 },
    /// A chunk for a stream-protocol subscription. blobs: its entries (as stored).
    Chunk { sub: u64, first: u64, next: u64, committed: u64 },
}

/// An owner's answer.
#[derive(Debug)]
pub enum Reply {
    Ok(Value, Vec<Bytes>),
    Err(AmqpError),
    /// This node doesn't own the queue (any more).
    Moved,
}

pub fn ok_frame(id: u64, v: Value, blobs: Vec<Bytes>) -> Frame {
    Frame { kind: RESP, id, head: json!({ "ok": v }), blobs }
}

pub fn err_frame(id: u64, e: &AmqpError) -> Frame {
    Frame { kind: RESP, id, head: json!({ "err": { "code": e.code, "text": e.text } }), blobs: vec![] }
}

pub fn moved_frame(id: u64) -> Frame {
    Frame { kind: RESP, id, head: json!({ "moved": true }), blobs: vec![] }
}

fn reply_of(f: Frame) -> Reply {
    if let Some(v) = f.head.get("ok") {
        return Reply::Ok(v.clone(), f.blobs);
    }
    if let Some(e) = f.head.get("err") {
        let code = e["code"].as_u64().unwrap_or(541) as u16;
        return Reply::Err(AmqpError::new(code, e["text"].as_str().unwrap_or("remote error").to_string()));
    }
    Reply::Moved
}

/// Receives pushes for one subscription.
pub trait PushHandler: Send + Sync {
    fn push(&self, ev: Push, blobs: Vec<Bytes>);
    /// The connection to the owner dropped.
    fn disconnected(&self);
}

/// A connection to another node.
pub struct Peer {
    pub node: String,
    /// The node's client listeners (`amqp`, `stream`, …), from its answer to the upgrade.
    pub listeners: BTreeMap<String, SocketAddr>,
    tx: mpsc::Sender<Frame>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Frame>>>,
    subs: Mutex<HashMap<u64, Arc<dyn PushHandler>>>,
    closed: AtomicBool,
    /// When a frame last came in (`mq_core::now_ms`).
    last_rx: AtomicU64,
}

impl Peer {
    /// Open the RPC connection to `node`, signed by `cluster`.
    pub async fn connect(node: &roll_cluster::Node, cluster: &roll_cluster::Cluster) -> anyhow::Result<Arc<Peer>> {
        let (io, listeners) = tokio::time::timeout(Duration::from_secs(3), upgrade(node, cluster)).await.map_err(|_| anyhow::anyhow!("connecting to {} timed out", node.id))??;
        let (r, mut w) = tokio::io::split(io);
        let (tx, mut rx) = mpsc::channel::<Frame>(8192);
        let peer = Arc::new(Peer {
            node: node.id.clone(),
            listeners,
            tx,
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            last_rx: AtomicU64::new(mq_core::now_ms()),
        });
        // Writer: coalesce whatever is queued into one write.
        tokio::spawn(async move {
            let mut buf = BytesMut::with_capacity(64 << 10);
            while let Some(f) = rx.recv().await {
                f.encode(&mut buf);
                while buf.len() < (1 << 20) {
                    match rx.try_recv() {
                        Ok(f) => f.encode(&mut buf),
                        Err(_) => break,
                    }
                }
                if w.write_all(&buf).await.is_err() || w.flush().await.is_err() {
                    break;
                }
                buf.clear();
            }
        });
        let p = peer.clone();
        let reader = tokio::spawn(async move {
            let mut r = BufReader::with_capacity(256 << 10, r);
            while let Ok(Some(f)) = Frame::read(&mut r).await {
                p.last_rx.store(mq_core::now_ms(), Ordering::Relaxed);
                match f.kind {
                    RESP => {
                        if let Some(tx) = p.pending.lock().remove(&f.id) {
                            let _ = tx.send(f);
                        }
                    }
                    PUSH => {
                        let Ok(ev) = serde_json::from_value::<Push>(f.head) else { continue };
                        let sub = match &ev {
                            Push::Deliver { sub, .. } | Push::Cancelled { sub } | Push::Activated { sub, .. } | Push::TimedOut { sub, .. } | Push::Moved { sub } | Push::Chunk { sub, .. } => *sub,
                        };
                        let h = p.subs.lock().get(&sub).cloned();
                        if let Some(h) = h {
                            h.push(ev, f.blobs);
                        }
                    }
                    _ => {}
                }
            }
            p.close();
        });
        // Ping, and drop the connection when the other end stops answering.
        let p = Arc::downgrade(&peer);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PING_EVERY).await;
                let Some(p) = p.upgrade() else { return };
                if p.is_closed() {
                    return;
                }
                let quiet = mq_core::now_ms().saturating_sub(p.last_rx.load(Ordering::Relaxed));
                if quiet > DEAD_AFTER.as_millis() as u64 {
                    tracing::warn!(node = %p.node, quiet_ms = quiet, "queues rpc: no answer from the node; dropping the connection");
                    reader.abort();
                    p.close();
                    return;
                }
                p.send(&Req::Ping);
            }
        });
        Ok(peer)
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.pending.lock().clear();
        let subs: Vec<_> = self.subs.lock().drain().map(|(_, h)| h).collect();
        for h in subs {
            h.disconnected();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.tx.is_closed()
    }

    /// Queue a request (in order) and return a future for its reply.
    pub async fn start(&self, req: &Req, blobs: Vec<Bytes>) -> oneshot::Receiver<Frame> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if self.is_closed() {
            return rx;
        }
        self.pending.lock().insert(id, tx);
        let head = serde_json::to_value(req).unwrap_or(Value::Null);
        if self.tx.send(Frame { kind: REQ, id, head, blobs }).await.is_err() {
            self.pending.lock().remove(&id);
        }
        rx
    }

    /// Wait for a reply started with [`Peer::start`]. A dropped connection reads as `Moved`
    /// (ask the metadata store who owns the queue now).
    pub async fn finish(rx: oneshot::Receiver<Frame>) -> Reply {
        match rx.await {
            Ok(f) => reply_of(f),
            Err(_) => Reply::Moved,
        }
    }

    pub async fn call(&self, req: &Req, blobs: Vec<Bytes>) -> Reply {
        let rx = self.start(req, blobs).await;
        Self::finish(rx).await
    }

    /// Fire and forget (credit grants, releases). Never dropped: a lost credit grant would
    /// stall a consumer for good, so a full queue hands the frame to a task that waits.
    pub fn send(&self, req: &Req) {
        let head = serde_json::to_value(req).unwrap_or(Value::Null);
        if let Err(mpsc::error::TrySendError::Full(f)) = self.tx.try_send(Frame { kind: REQ, id: 0, head, blobs: vec![] }) {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(f).await;
            });
        }
    }

    pub fn subscribe(&self, sub: u64, h: Arc<dyn PushHandler>) {
        if self.is_closed() {
            h.disconnected();
            return;
        }
        self.subs.lock().insert(sub, h);
    }

    pub fn unsubscribe(&self, sub: u64) {
        self.subs.lock().remove(&sub);
    }
}

/// The HTTP/1.1 upgrade behind [`Peer::connect`]: the stream, and the node's listeners.
async fn upgrade(node: &roll_cluster::Node, cluster: &roll_cluster::Cluster) -> anyhow::Result<(hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>, BTreeMap<String, SocketAddr>)> {
    use axum::http::{Method, StatusCode, header};
    let authority = node.url.strip_prefix("http://").unwrap_or(&node.url).split('/').next().unwrap_or_default().to_string();
    let tcp = tokio::net::TcpStream::connect(&authority).await?;
    let _ = tcp.set_nodelay(true);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tcp)).await?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let mut req = axum::http::Request::get(PATH)
        .header(header::HOST, &authority)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, PROTOCOL)
        .body(http_body_util::Empty::<Bytes>::new())?;
    req.headers_mut().extend(cluster.sign(&Method::GET, PATH));
    let resp = sender.send_request(req).await?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        anyhow::bail!("{} refused the cluster RPC connection: {}", node.id, resp.status());
    }
    let listeners = resp.headers().get(LISTENERS_HEADER).and_then(|v| serde_json::from_slice(v.as_bytes()).ok()).unwrap_or_default();
    Ok((hyper_util::rt::TokioIo::new(hyper::upgrade::on(resp).await?), listeners))
}

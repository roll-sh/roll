//! Client side of the keeper protocol: one pipelined TCP connection per keeper, shared by every
//! log, reconnected on demand.

use crate::proto::{self, Reply, Request};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use mq_core::transport::Transport;
use tokio::sync::{mpsc, oneshot};

/// The keeper could not be reached (connect failure, broken connection or timeout). The
/// request may or may not have been applied.
#[derive(Debug)]
pub struct Unreachable(pub String);

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "keeper unreachable: {}", self.0)
    }
}

impl std::error::Error for Unreachable {}

struct Shared {
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    alive: AtomicBool,
}

impl Shared {
    fn kill(&self) {
        self.alive.store(false, Ordering::Release);
        self.pending.lock().clear();
    }
}

struct Conn {
    tx: mpsc::UnboundedSender<Bytes>,
    shared: Arc<Shared>,
    tasks: [tokio::task::JoinHandle<()>; 2],
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.tasks.iter().for_each(|t| t.abort());
    }
}

/// Bytes exchanged with a keeper, as encoded and as sent (after compression).
#[derive(Debug, Default)]
pub struct Traffic {
    pub sent_raw: AtomicU64,
    pub sent_wire: AtomicU64,
    pub received_raw: AtomicU64,
    pub received_wire: AtomicU64,
}

/// A connection to one keeper.
pub struct KeeperClient {
    pub addr: SocketAddr,
    pub az: String,
    /// Per-request timeout.
    pub timeout: Duration,
    transport: Arc<Transport>,
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
    failed_at: Mutex<Option<Instant>>,
    next_id: AtomicU64,
    /// Compress large frames when the keeper reads them.
    compress: bool,
    pub traffic: Arc<Traffic>,
}

impl KeeperClient {
    pub fn new(addr: SocketAddr, az: impl Into<String>, timeout: Duration) -> Arc<Self> {
        Self::with_transport(addr, az, timeout, Transport::plain("keeper"))
    }

    /// Connect through `transport` (TLS, shared secret).
    pub fn with_transport(addr: SocketAddr, az: impl Into<String>, timeout: Duration, transport: Arc<Transport>) -> Arc<Self> {
        Self::with_options(addr, az, timeout, transport, true)
    }

    /// `compress: false` sends every frame as it is (CPU-bound brokers in a single AZ).
    pub fn with_options(addr: SocketAddr, az: impl Into<String>, timeout: Duration, transport: Arc<Transport>, compress: bool) -> Arc<Self> {
        Arc::new(KeeperClient {
            addr,
            az: az.into(),
            timeout,
            transport,
            conn: tokio::sync::Mutex::new(None),
            failed_at: Mutex::new(None),
            next_id: AtomicU64::new(1),
            compress,
            traffic: Arc::default(),
        })
    }

    /// Whether the last attempt to talk to this keeper worked.
    pub fn is_connected(&self) -> bool {
        self.failed_at.lock().is_none()
    }

    async fn connection(&self) -> Result<Arc<Conn>, Unreachable> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref()
            && c.shared.alive.load(Ordering::Acquire)
        {
            return Ok(c.clone());
        }
        *slot = None;
        // Don't hammer a keeper that just refused us; callers back off and retry.
        if let Some(t) = *self.failed_at.lock()
            && t.elapsed() < Duration::from_millis(100)
        {
            return Err(Unreachable(format!("{} recently failed", self.addr)));
        }
        let sock = match self.transport.connect(self.addr, self.timeout.min(Duration::from_secs(3))).await {
            Ok(s) => s,
            Err(e) => return Err(self.failed(format!("{}: {e:#}", self.addr))),
        };
        let (mut rd, mut wr) = sock.into_split();
        // Say what we read and learn what the keeper reads before anything else goes either way.
        let hello = async {
            let ping = proto::Request::Ping { features: if self.compress { proto::FEATURES } else { 0 } };
            wr.write_all(&proto::encode_request(0, &ping)).await?;
            wr.flush().await?;
            match proto::read_frame(&mut rd).await?.map(proto::decode_reply).transpose()? {
                Some((_, Reply::Pong { features })) => anyhow::Ok(features),
                other => anyhow::bail!("unexpected reply to ping: {other:?}"),
            }
        };
        let features = match tokio::time::timeout(self.timeout.min(Duration::from_secs(3)), hello).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => return Err(self.failed(format!("{}: {e:#}", self.addr))),
            Err(_) => return Err(self.failed(format!("{}: no reply to ping", self.addr))),
        };
        let zstd = self.compress && features & proto::FEATURE_ZSTD != 0;
        let shared = Arc::new(Shared { pending: Mutex::new(HashMap::new()), alive: AtomicBool::new(true) });
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        let (s, traffic) = (shared.clone(), self.traffic.clone());
        let writer = tokio::spawn(async move {
            let mut buf = Vec::with_capacity(64 << 10);
            let mut z = proto::Compressor::default();
            let mut wire = |f: Bytes| if zstd { z.frame(f) } else { f };
            while let Some(f) = rx.recv().await {
                let mut raw = f.len();
                buf.clear();
                buf.extend_from_slice(&wire(f));
                while buf.len() < (1 << 20)
                    && let Ok(f) = rx.try_recv()
                {
                    raw += f.len();
                    buf.extend_from_slice(&wire(f));
                }
                traffic.sent_raw.fetch_add(raw as u64, Ordering::Relaxed);
                traffic.sent_wire.fetch_add(buf.len() as u64, Ordering::Relaxed);
                if wr.write_all(&buf).await.is_err() || wr.flush().await.is_err() {
                    break;
                }
            }
            s.kill();
        });
        let (s, traffic) = (shared.clone(), self.traffic.clone());
        let reader = tokio::spawn(async move {
            let mut rd = tokio::io::BufReader::with_capacity(256 << 10, rd);
            while let Ok(Some(body)) = proto::read_frame(&mut rd).await {
                traffic.received_wire.fetch_add(4 + body.len() as u64, Ordering::Relaxed);
                let Ok(body) = proto::inflate(body) else { break };
                traffic.received_raw.fetch_add(4 + body.len() as u64, Ordering::Relaxed);
                let Ok((id, reply)) = proto::decode_reply(body) else { break };
                if let Some(tx) = s.pending.lock().remove(&id) {
                    let _ = tx.send(reply);
                }
            }
            s.kill();
        });
        *self.failed_at.lock() = None;
        let c = Arc::new(Conn { tx, shared, tasks: [writer, reader] });
        *slot = Some(c.clone());
        Ok(c)
    }

    fn failed(&self, why: String) -> Unreachable {
        *self.failed_at.lock() = Some(Instant::now());
        Unreachable(why)
    }

    /// Send a request and wait for its reply. Transport failures are [`Unreachable`].
    pub async fn call(&self, req: &Request) -> Result<Reply, Unreachable> {
        self.call_timeout(req, self.timeout).await
    }

    pub async fn call_timeout(&self, req: &Request, timeout: Duration) -> Result<Reply, Unreachable> {
        let conn = self.connection().await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        conn.shared.pending.lock().insert(id, tx);
        if !conn.shared.alive.load(Ordering::Acquire) || conn.tx.send(proto::encode_request(id, req)).is_err() {
            conn.shared.pending.lock().remove(&id);
            return Err(self.failed(format!("{}: connection closed", self.addr)));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(_)) => Err(self.failed(format!("{}: connection lost", self.addr))),
            Err(_) => {
                conn.shared.pending.lock().remove(&id);
                Err(Unreachable(format!("{}: request timed out", self.addr)))
            }
        }
    }

    /// Drop the current connection (the next call reconnects).
    pub async fn disconnect(&self) {
        if let Some(c) = self.conn.lock().await.take() {
            c.shared.kill();
        }
    }
}

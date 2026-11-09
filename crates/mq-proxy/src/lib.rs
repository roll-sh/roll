//! roll queues front proxy: wake-on-connect, AZ-aware routing, connection draining.
//!
//! A tiny L4 proxy, one per availability zone. Brokers publish their client listeners in their
//! cluster heartbeat (`mq_node` records in the metadata store); the proxy follows those records
//! and sends each new connection to a live, non-draining broker, preferring its own AZ (no
//! cross-AZ transfer charges) and the broker with the fewest connections it has handed out.
//!
//! When no broker is running (the cluster scaled to zero) the proxy holds the client's socket,
//! runs the [`Waker`] (start an ECS task, an instance, a process…) and forwards the connection
//! as soon as a broker heartbeats. Clients just see a slower connect.
//!
//! Draining brokers get no new connections; their existing ones are closed gradually by the
//! broker itself and reconnect through the proxy to another broker.

pub mod ecs;

use mq_core::meta::MetaStore;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Metadata kind brokers heartbeat under (see `mq-cluster`).
pub const NODE: &str = "mq_node";

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Starts brokers when none is running.
pub trait Waker: Send + Sync + 'static {
    fn wake(&self) -> BoxFuture<anyhow::Result<()>>;
}

/// Runs a shell command to wake the cluster, e.g.
/// `aws ecs update-service --cluster roll --service queues --desired-count 1`.
pub struct CommandWaker(pub String);

impl Waker for CommandWaker {
    fn wake(&self) -> BoxFuture<anyhow::Result<()>> {
        let cmd = self.0.clone();
        Box::pin(async move {
            let status = tokio::process::Command::new("sh").arg("-c").arg(&cmd).status().await?;
            anyhow::ensure!(status.success(), "wake command exited with {status}");
            Ok(())
        })
    }
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// `(protocol, listen address)`: the protocol names the broker listener to forward to
    /// (`amqp`, `mqtt`, `stomp`, `stream`, `http`, `management`, …).
    pub listeners: Vec<(String, SocketAddr)>,
    /// This proxy's availability zone.
    pub az: Option<String>,
    /// A broker is gone when its heartbeat hasn't changed for this long.
    pub node_ttl: Duration,
    /// How often broker records are re-read.
    pub refresh: Duration,
    /// How long a connection waits for a broker to start.
    pub wake_timeout: Duration,
    /// Run the waker at most this often.
    pub wake_every: Duration,
    pub connect_timeout: Duration,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            listeners: vec![("amqp".into(), SocketAddr::from(([0, 0, 0, 0], 5672)))],
            az: None,
            node_ttl: Duration::from_secs(5),
            refresh: Duration::from_millis(500),
            wake_timeout: Duration::from_secs(60),
            wake_every: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(3),
        }
    }
}

#[derive(Clone, Debug)]
struct Backend {
    listeners: HashMap<String, SocketAddr>,
    az: Option<String>,
    draining: bool,
    hb: u64,
    last_seen: Instant,
    /// Connect failed recently: skip until then.
    bad_until: Option<Instant>,
}

/// Live view of the brokers, kept fresh from the metadata store.
pub struct Directory {
    meta: Arc<dyn MetaStore>,
    config: ProxyConfig,
    nodes: RwLock<HashMap<String, Backend>>,
    conns: Mutex<HashMap<String, Arc<AtomicUsize>>>,
    changed: Notify,
}

/// A broker chosen for one connection; counts against it until dropped.
pub struct Lease {
    pub node: String,
    pub addr: SocketAddr,
    count: Arc<AtomicUsize>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Directory {
    pub fn new(meta: Arc<dyn MetaStore>, config: ProxyConfig) -> Arc<Self> {
        Arc::new(Directory { meta, config, nodes: RwLock::default(), conns: Mutex::default(), changed: Notify::new() })
    }

    /// Re-read broker records.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let records = self.meta.list(NODE).await?;
        let now = Instant::now();
        let mut nodes = self.nodes.write();
        nodes.retain(|k, _| records.iter().any(|(r, _)| r == k));
        for (id, v) in records {
            let Some(mut b) = backend_of(&v, now) else { continue };
            if let Some(old) = nodes.get(&id) {
                if old.hb == b.hb {
                    b.last_seen = old.last_seen;
                }
                b.bad_until = old.bad_until.filter(|t| *t > now);
            }
            nodes.insert(id, b);
        }
        drop(nodes);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Follow the metadata store until the directory is dropped.
    pub fn spawn_refresh(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let every = self.config.refresh;
        tokio::spawn(async move {
            loop {
                let Some(d) = me.upgrade() else { return };
                if let Err(e) = d.refresh().await {
                    warn!(error = %e, "reading broker records failed");
                }
                drop(d);
                tokio::time::sleep(every).await;
            }
        });
    }

    fn alive(&self, b: &Backend, now: Instant) -> bool {
        b.last_seen.elapsed() < self.config.node_ttl && !b.draining && b.bad_until.is_none_or(|t| t <= now)
    }

    fn count(&self, node: &str) -> Arc<AtomicUsize> {
        self.conns.lock().entry(node.to_string()).or_default().clone()
    }

    /// Pick a broker for `protocol`: alive, not draining, same AZ first, fewest connections.
    pub fn pick(&self, protocol: &str) -> Option<Lease> {
        let now = Instant::now();
        let nodes = self.nodes.read();
        let mut best: Option<(bool, usize, &String, SocketAddr)> = None;
        for (id, b) in nodes.iter() {
            if !self.alive(b, now) {
                continue;
            }
            let Some(addr) = b.listeners.get(protocol) else { continue };
            let other_az = self.config.az.is_some() && b.az != self.config.az;
            let n = self.count(id).load(Ordering::Acquire);
            let better = match &best {
                None => true,
                Some((o, c, bid, _)) => (other_az, n, id) < (*o, *c, *bid),
            };
            if better {
                best = Some((other_az, n, id, *addr));
            }
        }
        let (_, _, id, addr) = best?;
        let count = self.count(id);
        count.fetch_add(1, Ordering::AcqRel);
        Some(Lease { node: id.clone(), addr, count })
    }

    /// Don't send connections to `node` for a few seconds (connect failed).
    pub fn mark_bad(&self, node: &str) {
        if let Some(b) = self.nodes.write().get_mut(node) {
            b.bad_until = Some(Instant::now() + Duration::from_secs(5));
        }
    }

    /// Connections handed out per broker (for status and tests).
    pub fn connections(&self) -> HashMap<String, usize> {
        self.conns.lock().iter().map(|(k, v)| (k.clone(), v.load(Ordering::Acquire))).collect()
    }
}

fn backend_of(v: &Value, now: Instant) -> Option<Backend> {
    let listeners = v["listeners"].as_object()?.iter().filter_map(|(k, a)| Some((k.clone(), a.as_str()?.parse().ok()?))).collect();
    Some(Backend {
        listeners,
        az: v["az"].as_str().map(String::from),
        draining: v["draining"].as_bool().unwrap_or(false),
        hb: v["hb"].as_u64().unwrap_or(0),
        last_seen: now,
        bad_until: None,
    })
}

pub struct Proxy {
    pub directory: Arc<Directory>,
    waker: Option<Arc<dyn Waker>>,
    last_wake: Mutex<Option<Instant>>,
}

impl Proxy {
    pub fn new(directory: Arc<Directory>, waker: Option<Arc<dyn Waker>>) -> Arc<Self> {
        Arc::new(Proxy { directory, waker, last_wake: Mutex::new(None) })
    }

    /// Bind every configured listener and serve until one fails.
    pub async fn serve(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut set = tokio::task::JoinSet::new();
        for (protocol, addr) in self.directory.config.listeners.clone() {
            let listener = TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
            info!(%protocol, %addr, "proxy listening");
            set.spawn(self.clone().serve_listener(protocol, listener));
        }
        match set.join_next().await {
            Some(r) => r?,
            None => Ok(()),
        }
    }

    pub async fn serve_listener(self: Arc<Self>, protocol: String, listener: TcpListener) -> anyhow::Result<()> {
        let protocol: Arc<str> = protocol.into();
        loop {
            let (socket, peer) = listener.accept().await?;
            let me = self.clone();
            let protocol = protocol.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle(&protocol, socket).await {
                    debug!(%peer, error = %e, "proxied connection ended");
                }
            });
        }
    }

    async fn handle(&self, protocol: &str, mut client: TcpStream) -> anyhow::Result<()> {
        client.set_nodelay(true)?;
        let (lease, mut upstream) = self.connect(protocol).await?;
        debug!(node = %lease.node, addr = %lease.addr, protocol, "forwarding");
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        drop(lease);
        Ok(())
    }

    /// Find a broker and connect, waking the cluster and retrying other brokers as needed.
    pub async fn connect(&self, protocol: &str) -> anyhow::Result<(Lease, TcpStream)> {
        let d = &self.directory;
        let deadline = Instant::now() + d.config.wake_timeout;
        loop {
            let changed = d.changed.notified();
            if let Some(lease) = d.pick(protocol) {
                match tokio::time::timeout(d.config.connect_timeout, TcpStream::connect(lease.addr)).await {
                    Ok(Ok(s)) => {
                        s.set_nodelay(true)?;
                        return Ok((lease, s));
                    }
                    Ok(Err(e)) => warn!(node = %lease.node, addr = %lease.addr, error = %e, "broker refused connection"),
                    Err(_) => warn!(node = %lease.node, addr = %lease.addr, "broker connect timed out"),
                }
                d.mark_bad(&lease.node);
                continue;
            }
            self.wake();
            if Instant::now() >= deadline {
                anyhow::bail!("no broker for {protocol} became available");
            }
            let _ = tokio::time::timeout(Duration::from_millis(250), changed).await;
        }
    }

    fn wake(&self) {
        let Some(w) = self.waker.clone() else { return };
        {
            let mut last = self.last_wake.lock();
            if last.is_some_and(|t| t.elapsed() < self.directory.config.wake_every) {
                return;
            }
            *last = Some(Instant::now());
        }
        info!("no broker running; waking the cluster");
        tokio::spawn(async move {
            if let Err(e) = w.wake().await {
                warn!(error = %e, "wake failed");
            }
        });
    }
}

/// Answer every connection with `HTTP/1.1 200 OK` (load balancer health checks).
pub async fn serve_health(listener: TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut s, _)) = listener.accept().await else { continue };
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let _ = tokio::time::timeout(Duration::from_millis(500), s.read(&mut buf)).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
        });
    }
}

//! Replication: read replicas and failover.
//!
//! A primary keeps a bounded **backlog** of its write stream. A replica connects to the primary
//! node's replication port, asks for the stream from its offset, applies it and acknowledges
//! offsets (for `WAIT`). A replica that is too far behind for the backlog restores from object
//! storage (the durable log) and asks again, so a new replica never needs a full copy from
//! the primary.
//!
//! Replicas can also **tail object storage** instead ([`Follower::tail_storage`]): about a flush
//! interval of lag, no connection to the primary at all, and it works across regions and while
//! the primary is scaled to zero.
//!
//! Wire protocol (RESP), replica → primary: `REPLICATE <db> <offset> <secret>`, answered by
//! `+CONTINUE <offset>` followed by the raw stream, or `-RESYNC <durable offset>`. The replica
//! then sends `REPLCONF ACK <offset>` about every 100ms.

use crate::database::{Database, FeedSink};
use crate::persist::{self, Objects};
use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use redis_engine::commands::eq;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

pub mod psync;

/// ROLE's replica link states.
const LINK_STATES: [&str; 5] = ["connect", "connecting", "handshake", "sync", "connected"];

/// The tail of a database's write stream, for replicas to catch up from.
pub struct Backlog {
    inner: Mutex<BacklogInner>,
    pub cap: usize,
    pub wake: Notify,
    /// Acknowledged offset per connected replica.
    pub acks: Mutex<HashMap<u64, u64>>,
    pub acked: Notify,
    next_replica: AtomicU64,
    /// Connected replicas: address and Unix ms of their last acknowledgement (INFO `slaveN`).
    pub peers: Mutex<HashMap<u64, (SocketAddr, i64)>>,
    /// Its subscription to the database's write stream.
    sink: AtomicU64,
    /// Unix ms since when no replica is connected (0: some are), for `repl-backlog-ttl`.
    pub idle_since: std::sync::atomic::AtomicI64,
    /// The Redis-protocol replicas among `peers` (FAILOVER's candidates).
    pub redis_peers: Mutex<std::collections::HashSet<u64>>,
    /// Created for Redis replicas (PSYNC/SYNC), so `repl-backlog-ttl` frees it once none is
    /// left; a node's own replication keeps its backlog.
    pub for_redis: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct BacklogInner {
    chunks: VecDeque<Bytes>,
    /// Offset of the first byte in `chunks`.
    start: u64,
    end: u64,
    bytes: usize,
}

impl Backlog {
    pub fn new(cap: usize, offset: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BacklogInner { start: offset, end: offset, ..Default::default() }),
            cap,
            wake: Notify::new(),
            acks: Mutex::new(HashMap::new()),
            acked: Notify::new(),
            next_replica: AtomicU64::new(1),
            peers: Mutex::new(HashMap::new()),
            sink: AtomicU64::new(0),
            for_redis: std::sync::atomic::AtomicBool::new(false),
            redis_peers: Mutex::new(Default::default()),
            idle_since: std::sync::atomic::AtomicI64::new(redis_engine::util::now_ms()),
        })
    }

    /// Attach a backlog to a primary.
    pub fn attach(db: &Database, cap: usize) -> Arc<Self> {
        // Subscribe first so nothing falls between the offset and the first chunk.
        let b = Self::new(cap, 0);
        let (id, offset) = db.attach_backlog(b.clone());
        b.sink.store(id, Ordering::Relaxed);
        {
            let mut i = b.inner.lock();
            if i.chunks.is_empty() {
                i.start = offset;
                i.end = offset;
            }
        }
        *db.backlog.lock() = Some(b.clone());
        b
    }

    /// Free the database's backlog (`freeReplicationBacklog`).
    pub fn detach(db: &Database) {
        if let Some(b) = db.backlog.lock().take() {
            db.detach_backlog(b.sink.load(Ordering::Relaxed));
        }
    }

    /// Stream bytes from `offset` (up to the current end), or `None` if `offset` is no longer
    /// (or not yet) in the backlog.
    pub fn read_from(&self, offset: u64) -> Option<Vec<Bytes>> {
        let i = self.inner.lock();
        if offset < i.start || offset > i.end {
            return None;
        }
        let mut out = Vec::new();
        let mut pos = i.start;
        for c in &i.chunks {
            let end = pos + c.len() as u64;
            if end > offset {
                let skip = offset.saturating_sub(pos) as usize;
                out.push(c.slice(skip..));
            }
            pos = end;
        }
        Some(out)
    }

    pub fn end(&self) -> u64 {
        self.inner.lock().end
    }

    /// Offsets of the first byte held and of the end.
    pub fn range(&self) -> (u64, u64) {
        let i = self.inner.lock();
        (i.start, i.end)
    }

    /// Replicas whose acknowledged offset is at least `offset`.
    pub fn acked_count(&self, offset: u64) -> usize {
        self.acks.lock().values().filter(|&&a| a >= offset).count()
    }

    pub fn replicas(&self) -> Vec<u64> {
        self.acks.lock().values().copied().collect()
    }

    /// Forget the history: the stream continues at `offset` with other contents (a full
    /// resynchronization from a primary).
    pub fn reset(&self, offset: u64) {
        let mut i = self.inner.lock();
        i.chunks.clear();
        i.bytes = 0;
        i.start = offset;
        i.end = offset;
    }

    /// Attach a replica at `addr` (as INFO lists it) streaming from `offset`.
    pub fn register(&self, addr: SocketAddr, offset: u64) -> u64 {
        let rid = self.next_replica.fetch_add(1, Ordering::Relaxed);
        self.acks.lock().insert(rid, offset);
        self.peers.lock().insert(rid, (addr, redis_engine::util::now_ms()));
        self.redis_peers.lock().insert(rid);
        rid
    }

    /// A replica acknowledged `offset`.
    pub fn ack(&self, rid: u64, offset: u64) {
        if let Some(a) = self.acks.lock().get_mut(&rid) {
            *a = (*a).max(offset);
        }
        if let Some(p) = self.peers.lock().get_mut(&rid) {
            p.1 = redis_engine::util::now_ms();
        }
        self.acked.notify_waiters();
    }

    pub fn unregister(&self, rid: u64) {
        self.acks.lock().remove(&rid);
        self.forget_peer(rid);
        self.acked.notify_waiters();
    }

    /// A replica left: start the idle clock when it was the last.
    pub fn forget_peer(&self, rid: u64) {
        self.redis_peers.lock().remove(&rid);
        let mut peers = self.peers.lock();
        peers.remove(&rid);
        if peers.is_empty() {
            self.idle_since.store(redis_engine::util::now_ms(), Ordering::Relaxed);
        }
    }
}

impl FeedSink for Backlog {
    fn write(&self, chunk: &Bytes, end: u64) {
        {
            let mut i = self.inner.lock();
            if i.end != end - chunk.len() as u64 {
                // Offsets jumped (e.g. state restored underneath): restart the backlog.
                i.chunks.clear();
                i.bytes = 0;
                i.start = end - chunk.len() as u64;
            }
            i.chunks.push_back(chunk.clone());
            i.bytes += chunk.len();
            i.end = end;
            while i.bytes > self.cap && i.chunks.len() > 1 {
                let c = i.chunks.pop_front().unwrap();
                i.bytes -= c.len();
                i.start += c.len() as u64;
            }
        }
        self.wake.notify_waiters();
    }
}

/// How the replication server finds a primary database and its backlog.
pub trait Primaries: Send + Sync + 'static {
    fn primary(&self, db: &str) -> Option<Primary>;
}

pub type Primary = (Arc<Database>, Arc<Backlog>);

/// Primaries registered explicitly (single node, tests).
#[derive(Default)]
pub struct PrimaryMap(pub Mutex<HashMap<String, Primary>>);

impl PrimaryMap {
    pub fn add(&self, db: Arc<Database>, cap: usize) -> Arc<Backlog> {
        let b = Backlog::attach(&db, cap);
        self.0.lock().insert(db.id.clone(), (db, b.clone()));
        b
    }
}

impl Primaries for PrimaryMap {
    fn primary(&self, db: &str) -> Option<Primary> {
        self.0.lock().get(db).cloned()
    }
}

/// Serve replication links on an internal port. `secret` authenticates nodes to each other.
pub async fn serve(listener: TcpListener, primaries: Arc<dyn Primaries>, secret: String, durable: Arc<dyn Fn(&str) -> u64 + Send + Sync>) -> anyhow::Result<()> {
    let secret = Arc::new(secret);
    loop {
        let (sock, peer) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        let (primaries, secret, durable) = (primaries.clone(), secret.clone(), durable.clone());
        tokio::spawn(async move {
            if let Err(e) = serve_link(sock, peer, primaries, &secret, durable).await {
                tracing::debug!(%peer, error = %e, "replication link closed");
            }
        });
    }
}

async fn serve_link(
    mut sock: TcpStream,
    peer: SocketAddr,
    primaries: Arc<dyn Primaries>,
    secret: &str,
    durable: Arc<dyn Fn(&str) -> u64 + Send + Sync>,
) -> anyhow::Result<()> {
    let mut inbuf = BytesMut::new();
    let args = read_command(&mut sock, &mut inbuf).await?;
    if args.len() != 4 || !eq(&args[0], "replicate") {
        sock.write_all(b"-ERR expected REPLICATE <db> <offset> <secret>\r\n").await?;
        return Ok(());
    }
    if !ct_eq(&args[3], secret.as_bytes()) {
        sock.write_all(b"-NOAUTH bad replication secret\r\n").await?;
        return Ok(());
    }
    let id = String::from_utf8_lossy(&args[1]).into_owned();
    let Some((db, backlog)) = primaries.primary(&id) else {
        sock.write_all(b"-ERR no such primary\r\n").await?;
        return Ok(());
    };
    let mut offset: u64 = std::str::from_utf8(&args[2])?.parse()?;
    if backlog.read_from(offset).is_none() {
        sock.write_all(format!("-RESYNC {}\r\n", durable(&id)).as_bytes()).await?;
        return Ok(());
    }
    sock.write_all(format!("+CONTINUE {offset}\r\n").as_bytes()).await?;
    let rid = backlog.next_replica.fetch_add(1, Ordering::Relaxed);
    // Acks are kept as replication offsets (what WAIT compares), ours speak stream positions.
    backlog.acks.lock().insert(rid, db.stream_to_repl(offset));
    backlog.peers.lock().insert(rid, (peer, redis_engine::util::now_ms()));
    tracing::info!(db = id, %peer, offset, "replica attached");
    let (mut rd, mut wr) = sock.into_split();
    let acks = {
        let backlog = backlog.clone();
        let db = db.clone();
        tokio::spawn(async move {
            let mut buf = inbuf;
            loop {
                match read_command_split(&mut rd, &mut buf).await {
                    Ok(a) if a.len() == 3 && eq(&a[0], "replconf") && eq(&a[1], "ack") => {
                        if let Some(o) = std::str::from_utf8(&a[2]).ok().and_then(|s| s.parse().ok()) {
                            backlog.acks.lock().insert(rid, db.stream_to_repl(o));
                            if let Some(p) = backlog.peers.lock().get_mut(&rid) {
                                p.1 = redis_engine::util::now_ms();
                            }
                            backlog.acked.notify_waiters();
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        })
    };
    let result: anyhow::Result<()> = async {
        loop {
            let woken = backlog.wake.notified();
            let Some(chunks) = backlog.read_from(offset) else {
                anyhow::bail!("replica fell out of the backlog");
            };
            if chunks.is_empty() {
                tokio::select! {
                    _ = woken => {}
                    // Liveness probe so dead replicas are noticed: an empty line, outside the
                    // stream's offsets (a PING would count in them).
                    _ = tokio::time::sleep(Duration::from_secs(10)) => wr.write_all(b"\r\n").await.map(|_| ())?,
                }
                continue;
            }
            for c in chunks {
                wr.write_all(&c).await?;
                offset += c.len() as u64;
                db.io.repl_out.fetch_add(c.len() as u64, Ordering::Relaxed);
            }
        }
    }
    .await;
    acks.abort();
    backlog.acks.lock().remove(&rid);
    backlog.forget_peer(rid);
    backlog.acked.notify_waiters();
    result
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn read_command(sock: &mut TcpStream, buf: &mut BytesMut) -> anyhow::Result<Vec<Bytes>> {
    loop {
        if let Some(a) = redis_proto::parse_request(buf, Default::default()).map_err(|e| anyhow::anyhow!("{e}"))? {
            return Ok(a);
        }
        if sock.read_buf(buf).await? == 0 {
            anyhow::bail!("closed");
        }
    }
}

async fn read_command_split(rd: &mut tokio::net::tcp::OwnedReadHalf, buf: &mut BytesMut) -> anyhow::Result<Vec<Bytes>> {
    loop {
        if let Some(a) = redis_proto::parse_request(buf, Default::default()).map_err(|e| anyhow::anyhow!("{e}"))? {
            return Ok(a);
        }
        if rd.read_buf(buf).await? == 0 {
            anyhow::bail!("closed");
        }
    }
}

/// Wait until `n` replicas acknowledged `offset`, or `timeout` (0 = forever). Returns how many
/// did (Redis' `WAIT`).
pub async fn wait_acks(backlog: &Backlog, offset: u64, n: usize, timeout: Duration) -> usize {
    let deadline = (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout);
    loop {
        let woken = backlog.acked.notified();
        let have = backlog.acked_count(offset);
        if have >= n {
            return have;
        }
        match deadline {
            Some(d) => {
                if tokio::time::timeout_at(d, woken).await.is_err() {
                    return backlog.acked_count(offset);
                }
            }
            None => woken.await,
        }
    }
}

/// Slowest storage poll of an idle primary (the most a quiet replica lags once writes resume).
pub const MAX_TAIL_POLL: Duration = Duration::from_secs(5);

/// The replica side: keeps a read-only database in sync with its primary.
pub struct Follower {
    pub db: Arc<Database>,
    /// Applied stream offset (also the database's own feed offset).
    pub applied: AtomicU64,
    pub connected: std::sync::atomic::AtomicBool,
    /// Unix ms of the last applied data or ping from the primary.
    pub last_contact: AtomicU64,
    /// Unix ms the link last went up or down.
    pub link_changed: AtomicU64,
    /// A full resynchronization is being received (INFO `master_sync_in_progress`).
    pub syncing: std::sync::atomic::AtomicBool,
    /// ROLE's link state, an index into [`LINK_STATES`].
    state: std::sync::atomic::AtomicU8,
    stop: Notify,
    /// For resyncs from object storage, which can start from a snapshot.
    codec: Option<Arc<dyn persist::SnapshotCodec>>,
    /// Following a Redis primary: `applied` is a replication offset, and the stream position
    /// moves with it as the primary's bytes are re-published.
    pub redis: std::sync::atomic::AtomicBool,
}

impl Follower {
    pub fn new(db: Arc<Database>) -> Arc<Self> {
        Self::with_codec(db, None)
    }

    pub fn with_codec(db: Arc<Database>, codec: Option<Arc<dyn persist::SnapshotCodec>>) -> Arc<Self> {
        for s in &db.shards {
            s.engine.lock().replica = true;
        }
        let applied = db.stream_offset();
        Arc::new(Self {
            db,
            applied: AtomicU64::new(applied),
            connected: Default::default(),
            last_contact: AtomicU64::new(0),
            link_changed: AtomicU64::new(redis_engine::util::now_ms() as u64),
            syncing: Default::default(),
            state: Default::default(),
            stop: Notify::new(),
            codec,
            redis: Default::default(),
        })
    }

    /// The link's state as ROLE shows it.
    pub fn state(&self) -> &'static str {
        LINK_STATES[self.state.load(Ordering::Relaxed) as usize]
    }

    fn set_state(&self, s: &str) {
        let i = LINK_STATES.iter().position(|x| *x == s).unwrap_or(0);
        self.state.store(i as u8, Ordering::Relaxed);
    }

    /// Lag behind the primary's stream, in bytes, if known.
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::SeqCst)
    }

    /// Stop following and make the database writable (failover). The stream continues from the
    /// applied offset, so the promoted database's log picks up where the old primary's left.
    pub fn promote(&self) {
        self.stop.notify_waiters();
        self.stop.notify_one();
        for s in &self.db.shards {
            s.engine.lock().replica = false;
        }
        // A storage follower's stream is where it applied up to; a Redis follower's moved with
        // every re-published byte already.
        if !self.redis.load(Ordering::Relaxed) {
            self.db.set_offset(self.applied());
        }
        // A promoted replica's replication offset goes on with its stream (Redis keeps the
        // backlog it created while syncing).
        self.db.follow_stream();
    }

    /// Stop following, staying read-only (the replica is being unloaded or deleted).
    pub fn stop(&self) {
        self.stop.notify_waiters();
        self.stop.notify_one();
    }

    fn apply(&self, buf: &mut BytesMut, dbsel: &mut u16) -> anyhow::Result<()> {
        let mut effects = Vec::new();
        let mut consumed = 0usize;
        let limits = redis_proto::Limits::default();
        loop {
            let before = buf.len();
            let Some(args) = redis_proto::parse_request(buf, limits).map_err(|e| anyhow::anyhow!("corrupt stream: {e}"))? else { break };
            // Empty lines are the link's keepalive, outside the stream.
            if args.is_empty() {
                continue;
            }
            consumed += before - buf.len();
            // In-band control: counted in the offset, nothing to apply.
            if eq(&args[0], "replconf") {
                continue;
            }
            if args.len() == 2 && eq(&args[0], "select") {
                *dbsel = std::str::from_utf8(&args[1])?.parse()?;
                continue;
            }
            if args.len() == 1 && eq(&args[0], "ping") {
                continue;
            }
            effects.push(redis_engine::Effect { db: *dbsel, args });
        }
        if consumed > 0 {
            if !effects.is_empty() {
                self.db.engine(0).apply_all(&effects);
            }
            let applied = self.applied.fetch_add(consumed as u64, Ordering::SeqCst) + consumed as u64;
            self.db.set_offset(applied);
            self.last_contact.store(redis_engine::util::now_ms() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Restore from object storage, discarding local state (the backlog no longer has our offset).
    async fn resync(&self, objects: &Objects, primary_db: &str) -> anyhow::Result<()> {
        let fresh = Database::new(primary_db, Default::default(), 1);
        let m = persist::restore(objects, &fresh, self.codec.as_deref()).await?;
        // Move the restored keyspace into our database.
        {
            let mut src = fresh.engine(0);
            let mut dst = self.db.engine(0);
            dst.flush_all();
            std::mem::swap(&mut dst.dbs, &mut src.dbs);
        }
        self.applied.store(m.end, Ordering::SeqCst);
        self.db.set_offset(m.end);
        Ok(())
    }

    /// Follow a primary over a replication link until [`Follower::promote`].
    pub async fn stream(self: Arc<Self>, primary: SocketAddr, primary_db: String, secret: String, objects: Option<Objects>) {
        *self.db.upstream.lock() =
            Some(crate::database::Upstream { host: primary.ip().to_string(), port: primary.port(), follower: Arc::downgrade(&self), user: false });
        let mut backoff = Duration::from_millis(50);
        loop {
            let stopped = self.stop.notified();
            tokio::select! {
                _ = stopped => return,
                r = self.stream_once(primary, &primary_db, &secret, objects.as_ref()) => {
                    self.connected.store(false, Ordering::SeqCst);
                    self.set_state("connect");
                    self.link_changed.store(redis_engine::util::now_ms() as u64, Ordering::Relaxed);
                    if let Err(e) = r {
                        tracing::debug!(error = %e, "replication link lost");
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    }

    async fn stream_once(&self, primary: SocketAddr, primary_db: &str, secret: &str, objects: Option<&Objects>) -> anyhow::Result<()> {
        let mut sock = TcpStream::connect(primary).await?;
        sock.set_nodelay(true)?;
        let mut out = BytesMut::new();
        let off = self.applied().to_string();
        redis_proto::encode_command([b"REPLICATE".as_slice(), primary_db.as_bytes(), off.as_bytes(), secret.as_bytes()], &mut out);
        sock.write_all(&out).await?;
        let mut buf = BytesMut::with_capacity(64 << 10);
        let line = loop {
            if let Some(p) = buf.windows(2).position(|w| w == b"\r\n") {
                let l = buf.split_to(p + 2);
                break String::from_utf8_lossy(&l[..p]).into_owned();
            }
            if sock.read_buf(&mut buf).await? == 0 {
                anyhow::bail!("primary closed the link");
            }
        };
        if let Some(rest) = line.strip_prefix("-RESYNC ") {
            let durable: u64 = rest.trim().parse()?;
            let Some(objects) = objects else { anyhow::bail!("primary needs a resync but no object storage is configured") };
            // The durable log covers up to `durable`; the backlog is expected to reach back that
            // far (if writes outpace it, this repeats until they slow down).
            self.resync(objects, primary_db).await?;
            tracing::info!(db = primary_db, durable, applied = self.applied(), "replica resynced from object storage");
            anyhow::bail!("resynced from object storage, reconnecting");
        }
        if !line.starts_with("+CONTINUE") {
            anyhow::bail!("primary refused replication: {line}");
        }
        self.connected.store(true, Ordering::SeqCst);
        self.set_state("connected");
        self.link_changed.store(redis_engine::util::now_ms() as u64, Ordering::Relaxed);
        let (mut rd, mut wr) = sock.into_split();
        let mut dbsel = 0u16;
        // Stream bytes may have arrived together with the handshake line.
        self.apply(&mut buf, &mut dbsel)?;
        let mut ack = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                r = rd.read_buf(&mut buf) => {
                    let n = r?;
                    if n == 0 {
                        anyhow::bail!("primary closed the link");
                    }
                    self.db.io.repl_in.fetch_add(n as u64, Ordering::Relaxed);
                    self.apply(&mut buf, &mut dbsel)?;
                }
                _ = ack.tick() => {
                    let mut out = BytesMut::new();
                    let off = self.applied().to_string();
                    redis_proto::encode_command([b"REPLCONF".as_slice(), b"ACK", off.as_bytes()], &mut out);
                    wr.write_all(&out).await?;
                }
            }
        }
    }

    /// Follow a database through object storage only: poll for new log segments every
    /// `interval`. Lag is about the primary's flush interval plus `interval`.
    /// Polls every `interval` while the primary writes, backing off to [`MAX_TAIL_POLL`] while
    /// it is quiet (each poll is a GET and a LIST).
    pub async fn tail_storage(self: Arc<Self>, objects: Objects, primary_db: String, interval: Duration) {
        let mut seen: HashMap<(String, u64), ()> = HashMap::new();
        let mut dbsel = 0u16;
        let mut wait = interval;
        loop {
            let stopped = self.stop.notified();
            tokio::select! {
                _ = stopped => return,
                _ = tokio::time::sleep(wait) => {}
            }
            let before = self.applied();
            if let Err(e) = self.tail_once(&objects, &primary_db, &mut seen, &mut dbsel).await {
                tracing::debug!(error = %e, "storage tail failed");
            }
            wait = if self.applied() != before { interval } else { (wait * 2).min(MAX_TAIL_POLL.max(interval)) };
        }
    }

    async fn tail_once(&self, objects: &Objects, id: &str, seen: &mut HashMap<(String, u64), ()>, dbsel: &mut u16) -> anyhow::Result<()> {
        let Some(raw) = objects.get(&persist::manifest_key(id)).await? else { return Ok(()) };
        let m: persist::Manifest = serde_json::from_slice(&raw)?;
        if m.snapshots.first().is_some_and(|s| s.offset > self.applied()) {
            // The log we'd need is gone (behind the oldest snapshot): start over.
            self.resync(objects, id).await?;
            return Ok(());
        }
        let applied = self.applied();
        // Listed segments that end before what's applied hold nothing new.
        let mut segs: Vec<(String, u64)> = m.segments.iter().filter(|s| s.end > applied).map(|s| (s.node.clone(), s.seq)).collect();
        if let Some(node) = &m.node {
            let mut later: Vec<u64> = objects
                .list(&format!("redis/seg/{node}/"))
                .await?
                .iter()
                .filter_map(|(k, _)| k.rsplit('/').next()?.strip_suffix(".zst")?.parse().ok())
                .filter(|s| *s > m.after_seq)
                .collect();
            later.sort_unstable();
            segs.extend(later.into_iter().map(|s| (node.clone(), s)));
        }
        for seg in segs {
            if seen.contains_key(&seg) {
                continue;
            }
            let Some(data) = objects.get(&persist::segment_key(&seg.0, seg.1)).await? else { continue };
            let raw = tokio::task::spawn_blocking(move || zstd::decode_all(&data[..])).await??;
            for f in persist::frames(&raw) {
                let applied = self.applied();
                if f.db != id || f.end() <= applied {
                    continue;
                }
                if f.start != applied {
                    anyhow::bail!("gap in the log at {applied}");
                }
                let mut buf = BytesMut::from(f.data);
                self.apply(&mut buf, dbsel)?;
            }
            seen.insert(seg, ());
        }
        Ok(())
    }
}

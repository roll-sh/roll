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

/// The tail of a database's write stream, for replicas to catch up from.
pub struct Backlog {
    inner: Mutex<BacklogInner>,
    pub cap: usize,
    pub wake: Notify,
    /// Acknowledged offset per connected replica.
    pub acks: Mutex<HashMap<u64, u64>>,
    pub acked: Notify,
    next_replica: AtomicU64,
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
        })
    }

    /// Attach a backlog to a primary.
    pub fn attach(db: &Database, cap: usize) -> Arc<Self> {
        // Subscribe first so nothing falls between the offset and the first chunk.
        let b = Self::new(cap, 0);
        let (_, offset) = db.subscribe(b.clone());
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

    /// Replicas whose acknowledged offset is at least `offset`.
    pub fn acked_count(&self, offset: u64) -> usize {
        self.acks.lock().values().filter(|&&a| a >= offset).count()
    }

    pub fn replicas(&self) -> Vec<u64> {
        self.acks.lock().values().copied().collect()
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
    let Some((_db, backlog)) = primaries.primary(&id) else {
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
    backlog.acks.lock().insert(rid, offset);
    tracing::info!(db = id, %peer, offset, "replica attached");
    let (mut rd, mut wr) = sock.into_split();
    let acks = {
        let backlog = backlog.clone();
        tokio::spawn(async move {
            let mut buf = inbuf;
            loop {
                match read_command_split(&mut rd, &mut buf).await {
                    Ok(a) if a.len() == 3 && eq(&a[0], "replconf") && eq(&a[1], "ack") => {
                        if let Some(o) = std::str::from_utf8(&a[2]).ok().and_then(|s| s.parse().ok()) {
                            backlog.acks.lock().insert(rid, o);
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
                    // Liveness probe so dead replicas are noticed.
                    _ = tokio::time::sleep(Duration::from_secs(10)) => wr.write_all(b"*1\r\n$4\r\nPING\r\n").await.map(|_| ())?,
                }
                continue;
            }
            for c in chunks {
                wr.write_all(&c).await?;
                offset += c.len() as u64;
            }
        }
    }
    .await;
    acks.abort();
    backlog.acks.lock().remove(&rid);
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

/// The replica side: keeps a read-only database in sync with its primary.
pub struct Follower {
    pub db: Arc<Database>,
    /// Applied stream offset (also the database's own feed offset).
    pub applied: AtomicU64,
    pub connected: std::sync::atomic::AtomicBool,
    /// Unix ms of the last applied data or ping from the primary.
    pub last_contact: AtomicU64,
    stop: Notify,
}

impl Follower {
    pub fn new(db: Arc<Database>) -> Arc<Self> {
        for s in &db.shards {
            s.engine.lock().replica = true;
        }
        let applied = db.repl_offset();
        Arc::new(Self {
            db,
            applied: AtomicU64::new(applied),
            connected: Default::default(),
            last_contact: AtomicU64::new(0),
            stop: Notify::new(),
        })
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
        self.db.set_offset(self.applied());
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
            consumed += before - buf.len();
            if args.is_empty() {
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
        let m = persist::restore(objects, &fresh, None).await?;
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
        let mut backoff = Duration::from_millis(50);
        loop {
            let stopped = self.stop.notified();
            tokio::select! {
                _ = stopped => return,
                r = self.stream_once(primary, &primary_db, &secret, objects.as_ref()) => {
                    self.connected.store(false, Ordering::SeqCst);
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
        let (mut rd, mut wr) = sock.into_split();
        let mut dbsel = 0u16;
        // Stream bytes may have arrived together with the handshake line.
        self.apply(&mut buf, &mut dbsel)?;
        let mut ack = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                r = rd.read_buf(&mut buf) => {
                    if r? == 0 {
                        anyhow::bail!("primary closed the link");
                    }
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
    pub async fn tail_storage(self: Arc<Self>, objects: Objects, primary_db: String, interval: Duration) {
        let mut seen: HashMap<(String, u64), ()> = HashMap::new();
        let mut dbsel = 0u16;
        loop {
            let stopped = self.stop.notified();
            tokio::select! {
                _ = stopped => return,
                _ = tokio::time::sleep(interval) => {}
            }
            if let Err(e) = self.tail_once(&objects, &primary_db, &mut seen, &mut dbsel).await {
                tracing::debug!(error = %e, "storage tail failed");
            }
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
        let mut segs: Vec<(String, u64)> = m.segments.iter().map(|s| (s.node.clone(), s.seq)).collect();
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

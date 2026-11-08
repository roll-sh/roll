//! roll streams on the fleet (docs/cloud/README.md; the broker README, "On the fleet"): many
//! nodes, one owner per stream.
//!
//! - **Ownership.** A stream (a tenant's whole Kafka cluster: what SASL tenancy maps a key to)
//!   has one owner node, held through the roll-cluster lease `stream:cluster:<stream id>`. Its
//!   connections, HTTP requests, partitions, coordinators and background work all run there.
//! - **Connections** are accepted by any node. The first request that needs the stream (after
//!   SASL) picks where the connection runs: here if this node owns the stream, else the rest of
//!   the connection is spliced to the owner ([`Splicer`]), with the bytes already read first.
//!   While the stream moves, the connection waits (riding out the move) instead of failing.
//! - **Acknowledgements** come from object storage, not the local disk ([`stream_log::LogConfig::remote`]):
//!   the owner writes what its partitions appended to a write-ahead log object every
//!   [`Fleet::linger`] (one object per stream, whatever the number of partitions), records it in
//!   the catalog, and only then moves the high watermarks. `acks=all` (and the HTTP API's
//!   default) waits for that; a consumer never sees a record that isn't in object storage.
//!   Chunks are uploaded as on one node, and the log objects they cover are deleted.
//! - **Fencing.** A new owner bumps the stream's generation in the catalog ([`Store::claim`]);
//!   every catalog write of the stream checks it in the same transaction, and object keys carry
//!   it, so an owner that lost its lease can't acknowledge, index or delete anything.
//! - **Takeover.** The new owner drops whatever it has locally of the stream (its disk is a cache
//!   and may be stale), reads the catalog, replays the write-ahead log after what the chunks
//!   hold (which also rebuilds idempotent-producer and transaction state), recovers the
//!   transaction coordinator, and serves.
//! - **Handoff** (drain, rebalancing): connections close over a moment (clients reconnect to the
//!   same hostname and reach the new owner), requests in flight finish, the write-ahead log is
//!   flushed and partitions uploaded, and only then is the lease released.
//!
//! [`Store::claim`]: crate::store::Store::claim

use crate::broker::{Broker, Partition, Stream};
use crate::store::{WalRow, is_fenced};
use crate::wal;
use bytes::Bytes;
use futures::future::BoxFuture;
use roll_cluster::{Cluster, Guard, Node, Ownership, Route};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc, watch};
use tracing::{debug, info, warn};

const KEY_PREFIX: &str = "stream:cluster:";
/// How long a connection or request waits for its stream to get an owner that serves it.
pub const RIDE_OUT: Duration = Duration::from_secs(15);
/// Write-ahead log objects are kept this long after chunks cover them: a producer retrying a
/// batch whose acknowledgement a move cut off is still recognised as a duplicate by the next
/// owner, which rebuilds producer state from them.
const WAL_RETAIN_MS: i64 = 60_000;
/// Unacknowledged bytes a stream may hold before produce is refused (object storage is down).
const MAX_PENDING: usize = 256 << 20;

/// The ownership key of a stream.
pub fn key(stream: &str) -> String {
    format!("{KEY_PREFIX}{stream}")
}

/// The stream an ownership key names.
pub fn stream_of(key: &str) -> Option<&str> {
    key.strip_prefix(KEY_PREFIX)
}

/// A byte stream a connection runs on.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

/// What the owner needs to carry on a connection another node accepted.
pub struct Hop {
    /// The stream the connection authenticated as.
    pub stream: String,
    /// The client.
    pub peer: SocketAddr,
    /// The broker address the client was told (the tenant hostname it connected with).
    pub advertised: Option<(String, u16)>,
    /// What the accepting node already read from the client: the owner reads it first.
    pub first: Bytes,
}

/// Hands a client connection to another node (rolld: `roll_edge::splice`; tests: in process).
/// The owner serves it with [`crate::server::serve_spliced`].
pub trait Splicer: Send + Sync + 'static {
    fn splice(&self, to: &Node, io: Box<dyn Io>, hop: Hop) -> BoxFuture<'static, std::io::Result<()>>;
}

/// The broker's part in the fleet.
pub struct Fleet {
    pub cluster: Arc<Cluster>,
    pub splicer: Arc<dyn Splicer>,
    /// How long the write-ahead log gathers appends before writing an object: `acks=all`
    /// latency against object-storage requests (one PUT per busy stream per round).
    pub linger: Duration,
    /// Ownership changes, in order, per stream.
    workers: Mutex<HashMap<String, mpsc::UnboundedSender<Ownership>>>,
}

impl Fleet {
    pub fn new(cluster: Arc<Cluster>, splicer: Arc<dyn Splicer>) -> Self {
        Self { cluster, splicer, linger: Duration::from_millis(50), workers: Mutex::default() }
    }
}

/// Whether this node serves a stream, and under which tenure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serving {
    No,
    /// Taking it over: partitions may open (to replay), nothing is served yet.
    Recovering { lease: i64 },
    Yes { lease: i64, generation: i64 },
}

/// A stream's tenure on this node (unused on one node).
pub struct Tenure {
    serving: watch::Sender<Serving>,
    /// Partitions with appends the write-ahead log hasn't taken yet.
    dirty: Mutex<Vec<Arc<Partition>>>,
    wake: Notify,
    /// The next write-ahead log sequence number; held while an object is written.
    seq: tokio::sync::Mutex<i64>,
    /// Bytes appended and not yet in the write-ahead log.
    pending: AtomicUsize,
    /// Connections and requests serving the stream.
    active: AtomicUsize,
}

impl Default for Tenure {
    fn default() -> Self {
        Self {
            serving: watch::channel(Serving::No).0,
            dirty: Mutex::default(),
            wake: Notify::new(),
            seq: tokio::sync::Mutex::new(1),
            pending: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        }
    }
}

impl Tenure {
    pub fn serving(&self) -> Serving {
        *self.serving.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<Serving> {
        self.serving.subscribe()
    }

    fn set(&self, s: Serving) {
        self.serving.send_replace(s);
        self.wake.notify_one();
    }

    /// The stream was deleted here.
    pub(crate) fn set_gone(&self) {
        self.set(Serving::No);
    }

    fn is(&self, generation: i64) -> bool {
        matches!(self.serving(), Serving::Yes { generation: g, .. } if g == generation)
    }

    /// A partition appended: queue it for the next write-ahead log object.
    pub(crate) fn appended(&self, p: &Arc<Partition>, bytes: usize) {
        self.pending.fetch_add(bytes, Ordering::Relaxed);
        if !p.wal_dirty.swap(true, Ordering::AcqRel) {
            self.dirty.lock().unwrap().push(p.clone());
        }
        self.wake.notify_one();
    }

    /// Whether more appends can be held until object storage takes them.
    pub(crate) fn has_room(&self) -> bool {
        self.pending.load(Ordering::Relaxed) < MAX_PENDING
    }

    fn take_dirty(&self) -> Vec<Arc<Partition>> {
        let ps = std::mem::take(&mut *self.dirty.lock().unwrap());
        for p in &ps {
            p.wal_dirty.store(false, Ordering::Release);
        }
        ps
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

/// A connection or request serving a stream on its owner: holds the lease's guard (a handoff
/// waits for it) and counts as active.
pub struct Lease {
    pub guard: Guard,
    stream: Arc<Stream>,
}

impl Lease {
    pub(crate) fn new(guard: Guard, stream: Arc<Stream>) -> Self {
        stream.tenure.active.fetch_add(1, Ordering::AcqRel);
        Self { guard, stream }
    }

    /// Resolves when this node stops serving the stream under the tenure it serves it under
    /// now (it was fenced, or gave the stream up), even if the lease didn't say so.
    pub(crate) fn tenure_ended(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.stream.tenure.subscribe();
        async move {
            let now = *rx.borrow();
            let _ = rx.wait_for(|s| *s != now).await;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.stream.tenure.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A stream taken over: its tenure's generation, and its transactions to finish.
type TakenOver = (Arc<Stream>, i64, Vec<(String, String)>);

/// Where a connection or request for a stream runs.
pub enum Routed {
    Local(Lease),
    Forward(Node),
    /// No owner could serve it within [`RIDE_OUT`].
    Unavailable,
}

impl Broker {
    pub(crate) fn fleet(&self) -> &Fleet {
        self.fleet.as_ref().expect("on the fleet")
    }

    /// Whether this node may serve (or open partitions of) `stream`: always on one node.
    pub fn serves(&self, stream: &Stream) -> bool {
        self.fleet.is_none() || !matches!(stream.tenure.serving(), Serving::No)
    }

    /// The streams this node serves (every stream on one node): background work runs for these.
    pub fn served_streams(&self) -> Vec<Arc<Stream>> {
        self.streams().into_iter().filter(|s| self.serves(s)).collect()
    }

    /// Route `stream` to its owner, riding out a move for up to [`RIDE_OUT`].
    pub async fn route_stream(self: &Arc<Self>, stream: &Arc<Stream>) -> Routed {
        let fleet = self.fleet();
        let key = key(&stream.id);
        let deadline = Instant::now() + RIDE_OUT;
        loop {
            let wait = match fleet.cluster.route_async(&key).await {
                Ok(Route::Local(guard)) => match self.wait_serving(stream, guard.epoch(), deadline).await {
                    true => return Routed::Local(Lease::new(guard, stream.clone())),
                    false => Duration::from_millis(50),
                },
                Ok(Route::Forward(node)) => return Routed::Forward(node),
                Ok(Route::Unavailable { retry_after }) => retry_after,
                Err(e) => {
                    debug!(stream = %stream.id, error = %e, "routing a stream failed");
                    Duration::from_millis(250)
                }
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Routed::Unavailable;
            }
            tokio::time::sleep(wait.min(left).min(Duration::from_secs(1))).await;
        }
    }

    /// Wait until this node serves `stream` under the lease `epoch` (its takeover is done).
    /// False if the tenure is another one, or `deadline` passes.
    pub async fn wait_serving(&self, stream: &Stream, epoch: i64, deadline: Instant) -> bool {
        let mut rx = stream.tenure.subscribe();
        let ok = |s: &Serving| matches!(s, Serving::Yes { lease, .. } if *lease == epoch);
        let done = |s: &Serving| ok(s) || matches!(s, Serving::Yes { lease, .. } | Serving::Recovering { lease } if *lease > epoch);
        match tokio::time::timeout_at(deadline.into(), rx.wait_for(done)).await {
            Ok(Ok(s)) => ok(&s),
            _ => false,
        }
    }

    /// Follow this node's ownership changes: take streams over and give them up, in order per
    /// stream. Runs for the broker's life.
    pub(crate) async fn follow_ownership(self: Arc<Self>) {
        let mut rx = self.fleet().cluster.subscribe_ownership();
        while let Some(ev) = rx.recv().await {
            let key = match &ev {
                Ownership::Acquired { key, .. } | Ownership::Lost { key } => key,
            };
            let Some(id) = stream_of(key).map(str::to_string) else { continue };
            let mut workers = self.fleet().workers.lock().unwrap();
            let tx = workers.entry(id.clone()).or_insert_with(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(self.clone().stream_worker(id.clone(), rx));
                tx
            });
            let _ = tx.send(ev);
        }
    }

    async fn stream_worker(self: Arc<Self>, id: String, mut rx: mpsc::UnboundedReceiver<Ownership>) {
        while let Some(ev) = rx.recv().await {
            match ev {
                Ownership::Acquired { epoch, .. } => self.take_over(&id, epoch).await,
                Ownership::Lost { .. } => self.give_up(&id).await,
            }
        }
    }

    /// Take `id` over under the lease `lease`, retrying while the lease lasts.
    pub async fn take_over(self: &Arc<Self>, id: &str, lease: i64) {
        let k = key(id);
        let mut backoff = Duration::from_millis(100);
        loop {
            if self.fleet().cluster.epoch(&k) != Some(lease) {
                return;
            }
            match self.try_take_over(id, lease).await {
                Ok(Some((s, generation, unfinished))) => {
                    info!(stream = id, generation, "serving the stream here");
                    tokio::spawn(self.clone().wal_writer(s.clone(), generation));
                    if let Some(guard) = self.fleet().cluster.guard(&k) {
                        tokio::spawn(self.clone().hold(s, guard, generation));
                    }
                    // Their markers go through the write-ahead log, which runs now.
                    let b = self.clone();
                    tokio::spawn(async move { b.txns.finish(&b, unfinished).await });
                    return;
                }
                Ok(None) => return,
                Err(e) => {
                    warn!(stream = id, error = %e, "taking the stream over failed; retrying");
                    if let Some(s) = self.stream_cached(id) {
                        s.tenure.set(Serving::No);
                        self.store.end_tenure(id);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    async fn try_take_over(self: &Arc<Self>, id: &str, lease: i64) -> anyhow::Result<Option<TakenOver>> {
        let k = key(id);
        let (me, sid) = (self.clone(), id.to_string());
        let (row, topics, generation, rows, remote) = blocking(move || {
            let Some(row) = me.store.stream(&sid)? else { return Ok((None, vec![], 0, vec![], vec![])) };
            let topics = me.store.topics_of(&row.id)?;
            let generation = me.store.claim(&row.id, me.fleet().cluster.node_id())?;
            let rows = me.store.wal(&row.id)?;
            let remote = me.store.partitions_with_chunks(&row.id)?;
            Ok((Some(row), topics, generation, rows, remote))
        })
        .await?;
        let Some(row) = row else {
            // Deleted: nothing to serve, and nobody should take it again.
            self.forget_stream(id);
            self.fleet().cluster.retire(&k);
            return Ok(None);
        };
        let s = self.adopt(row, topics);
        s.tenure.set(Serving::Recovering { lease });
        self.wipe_local(&s).await;
        // What the previous owner acknowledged after its last chunks.
        let mut replayed = 0usize;
        for r in &rows {
            let data = self.objects.get(&r.key).await?.ok_or_else(|| anyhow::anyhow!("write-ahead log object {} is missing", r.key))?;
            for (topic, index, batch) in wal::decode(&data)? {
                let Some(t) = s.topic_by_id(&topic) else { continue };
                let Some(p) = t.partition(index) else { continue };
                let log = self.log(&t, &p).map_err(|e| anyhow::anyhow!(e.message()))?;
                if log.replay(batch, crate::ids::now_ms())? {
                    replayed += 1;
                }
            }
        }
        for (topic, index) in remote {
            if let Some(p) = s.topic_by_id(&topic).and_then(|t| t.partition(index)) {
                p.remote_data.store(true, Ordering::Relaxed);
            }
        }
        let (me, sid) = (self.clone(), s.id.clone());
        let unfinished = blocking(move || me.txns.recover_stream(&me, &sid)).await?;
        *s.tenure.seq.lock().await = rows.last().map_or(1, |r| r.seq + 1);
        if self.fleet().cluster.epoch(&k) != Some(lease) {
            anyhow::bail!("the lease moved during the takeover");
        }
        s.tenure.set(Serving::Yes { lease, generation });
        debug!(stream = %s.id, objects = rows.len(), batches = replayed, "replayed the write-ahead log");
        Ok(Some((s, generation, unfinished)))
    }

    /// Stop serving `id`: its lease is gone. Partitions close (waiting requests get
    /// NOT_LEADER_OR_FOLLOWER), coordinator state goes, the local copies are dropped.
    pub async fn give_up(self: &Arc<Self>, id: &str) {
        let Some(s) = self.stream_cached(id) else { return };
        if matches!(s.tenure.serving(), Serving::No) {
            return;
        }
        s.tenure.set(Serving::No);
        self.store.end_tenure(id);
        s.tenure.take_dirty();
        s.tenure.pending.store(0, Ordering::Relaxed);
        self.groups.drop_stream(id);
        self.txns.drop_stream(id);
        self.wipe_local(&s).await;
        info!(stream = id, "no longer serving the stream here");
    }

    /// Another node claimed `s` while this one still holds its lease: stop, and hand the lease
    /// on so whoever should own the stream takes it over properly (with a claim of its own).
    async fn fenced(self: &Arc<Self>, s: &Stream) {
        warn!(stream = %s.id, "another node claimed the stream; stopping and handing the lease on");
        self.give_up(&s.id).await;
        self.fleet().cluster.handoff(&key(&s.id));
    }

    /// Test hook: stop as a killed process would, as far as this broker goes: no stream is
    /// served from now on, and whatever wasn't in the write-ahead log yet is gone.
    #[doc(hidden)]
    pub async fn halt(self: &Arc<Self>) {
        for s in self.served_streams() {
            self.give_up(&s.id).await;
        }
    }

    /// Close every partition of `s` and delete its local files.
    async fn wipe_local(&self, s: &Stream) {
        let mut dirs = vec![];
        for t in s.topics() {
            for p in t.partitions() {
                p.close();
            }
            dirs.push(self.config.topic_dir(&t.id));
        }
        let _ = tokio::task::spawn_blocking(move || {
            for d in dirs {
                let _ = std::fs::remove_dir_all(d);
            }
        })
        .await;
    }

    /// Write what `s`'s partitions appended to the write-ahead log, every [`Fleet::linger`]
    /// while there is any, for as long as this tenure lasts.
    async fn wal_writer(self: Arc<Self>, s: Arc<Stream>, generation: i64) {
        let linger = self.fleet().linger;
        while s.tenure.is(generation) {
            let _ = tokio::time::timeout(Duration::from_secs(1), s.tenure.wake.notified()).await;
            if !s.tenure.is(generation) {
                break;
            }
            if s.tenure.dirty.lock().unwrap().is_empty() {
                continue;
            }
            tokio::time::sleep(linger).await;
            if let Err(e) = self.flush_wal(&s, generation).await {
                if is_fenced(&e) && s.tenure.is(generation) {
                    self.fenced(&s).await;
                    break;
                }
                warn!(stream = %s.id, error = %e, "writing the write-ahead log failed");
            }
        }
    }

    /// Write one write-ahead log object with everything appended so far, record it, and make it
    /// visible. Returns the batches written. Retries object storage and the catalog while the
    /// tenure lasts; a fenced catalog write ends it.
    pub async fn flush_wal(self: &Arc<Self>, s: &Arc<Stream>, generation: i64) -> anyhow::Result<usize> {
        let mut seq = s.tenure.seq.lock().await;
        let mut takes = vec![];
        for p in s.tenure.take_dirty() {
            if let Some(log) = p.loaded()
                && let Some(t) = log.take_wal()
            {
                s.tenure.pending.fetch_sub(t.bytes.min(s.tenure.pending.load(Ordering::Relaxed)), Ordering::Relaxed);
                takes.push((p, log, t));
            }
        }
        if takes.is_empty() {
            return Ok(0);
        }
        let body = wal::encode(takes.iter().map(|(p, _, t)| (p.topic_id, p.index, &t.batches[..])));
        let row = WalRow {
            seq: *seq,
            key: wal::key(&s.id, generation, *seq),
            ends: takes.iter().map(|(p, _, t)| (p.topic_id, p.index, t.first, t.end)).collect(),
            bytes: body.len() as i64,
            created_at: crate::ids::now_ms(),
        };
        let mut backoff = Duration::from_millis(50);
        while let Err(e) = self.objects.put(&row.key, body.clone()).await {
            if !s.tenure.is(generation) {
                return Err(e);
            }
            warn!(stream = %s.id, error = %e, "writing a write-ahead log object failed; retrying");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
        s.usage.puts.fetch_add(1, Ordering::Relaxed);
        loop {
            let (b, sid, r) = (self.clone(), s.id.clone(), row.clone());
            match blocking(move || b.store.insert_wal(&sid, &r)).await {
                Ok(()) => break,
                Err(e) if is_fenced(&e) || !s.tenure.is(generation) => return Err(e),
                Err(e) => {
                    warn!(stream = %s.id, error = %e, "recording a write-ahead log object failed; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                }
            }
        }
        debug!(stream = %s.id, seq = *seq, partitions = takes.len(), bytes = row.bytes, "wrote a write-ahead log object");
        *seq += 1;
        let n = takes.iter().map(|(_, _, t)| t.batches.len()).sum();
        for (_, log, t) in takes {
            log.set_durable(t.end);
        }
        Ok(n)
    }

    /// Hold the tenure's guard: when a handoff starts, wait for the stream's connections and
    /// requests to finish, flush the write-ahead log and upload the partitions, then let go.
    async fn hold(self: Arc<Self>, s: Arc<Stream>, guard: Guard, generation: i64) {
        guard.ending().await;
        let k = key(&s.id);
        if self.fleet().cluster.owns(&k) && s.tenure.is(generation) {
            let started = Instant::now();
            debug!(stream = %s.id, active = s.tenure.active(), "a handoff started; waiting for the stream's connections");
            let deadline = started + self.fleet().cluster.config().handoff_timeout.mul_f32(0.6);
            while s.tenure.active() > 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            loop {
                match self.flush_wal(&s, generation).await {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) => {
                        warn!(stream = %s.id, error = %e, "flushing the write-ahead log before a handoff failed");
                        break;
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_secs(1));
            let _ = tokio::time::timeout(left, self.upload_stream(&s)).await;
            info!(stream = %s.id, took_ms = started.elapsed().as_millis() as u64, "handing the stream off");
        }
        drop(guard);
    }

    /// Delete write-ahead log objects whose records are all in chunks (after
    /// [`WAL_RETAIN_MS`], and never one an open transaction still needs), oldest first.
    pub(crate) async fn gc_wal(self: &Arc<Self>, s: &Arc<Stream>) -> anyhow::Result<usize> {
        let (b, id) = (self.clone(), s.id.clone());
        let rows = blocking(move || b.store.wal(&id)).await?;
        let now = crate::ids::now_ms();
        let mut done = vec![];
        for r in rows {
            if now - r.created_at < WAL_RETAIN_MS {
                break;
            }
            let mut covered = true;
            for (topic, index, _, end) in &r.ends {
                let Some(p) = s.topic_by_id(topic).and_then(|t| t.partition(*index)) else { continue };
                covered &= match p.loaded() {
                    Some(log) => log.stats().pending_upload_bytes == 0 && log.uploaded_end() >= *end && log.open_txns().iter().all(|(_, first)| first >= end),
                    // Closed partitions are fully uploaded and have no open transactions.
                    None => {
                        let (b, t, i) = (self.clone(), *topic, *index);
                        blocking(move || b.store.uploaded_end(&t, i)).await?.unwrap_or(0) >= *end
                    }
                };
                if !covered {
                    break;
                }
            }
            if !covered {
                break;
            }
            done.push(r);
        }
        if done.is_empty() {
            return Ok(0);
        }
        let (b, id, seqs) = (self.clone(), s.id.clone(), done.iter().map(|r| r.seq).collect::<Vec<_>>());
        blocking(move || b.store.delete_wal(&id, &seqs)).await?;
        for r in &done {
            if let Err(e) = self.objects.delete(&r.key).await {
                debug!(key = %r.key, error = %e, "deleting a write-ahead log object");
            }
        }
        Ok(done.len())
    }
}

/// Run catalog work off the async workers (it may wait on Postgres).
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| anyhow::anyhow!("catalog task: {e}"))?
}

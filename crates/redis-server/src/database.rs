//! A database: one tenant's keyspace, split into shards that each run their own engine.
//!
//! Shards give a database multi-core parallelism (keys are routed by hash slot, like Redis
//! Cluster) and are the unit that moves between nodes when a database scales out.
//!
//! Every engine access goes through [`Database::engine`]. When the guard drops, the effects the
//! commands produced are encoded as a RESP command stream (with `SELECT` when the logical db
//! changes, like Redis' replication stream) and handed to the [`Feed`]'s subscribers: the
//! durable log and replicas. Holding the engine lock while publishing keeps the stream in
//! execution order.

use crate::repl::Backlog;
use bytes::{Bytes, BytesMut};
use parking_lot::{Mutex, MutexGuard, RwLock};
use redis_engine::acl::Acl;
use redis_engine::commands::scripting::ScriptCtl;
use redis_engine::{Config, Effect, Engine, Hub};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// How long a connection waits for a busy engine by yielding before it parks.
const YIELD_FOR: Duration = Duration::from_millis(5);

pub struct Shard {
    pub engine: Mutex<Engine>,
    /// The engine's running-script state, readable while a script holds the engine.
    pub scripts: Arc<ScriptCtl>,
    /// A CLIENT PAUSE (or failover's) may hold commands back: set by the guard that leaves one
    /// set, so a command can skip locking the engine to check when nothing is paused.
    pub paused: AtomicBool,
}

/// Receives the database's write stream.
pub trait FeedSink: Send + Sync {
    /// `chunk` is RESP-encoded commands; `end` is the replication offset after it.
    fn write(&self, chunk: &Bytes, end: u64);
}

#[derive(Default)]
pub struct Feed {
    /// Logical db of the last command in the stream (`u16::MAX`: none yet, emit `SELECT`).
    last_db: u16,
    /// Bytes of write stream produced since the database was created: the position durability,
    /// snapshots and the backlog's chunks use.
    pub offset: u64,
    /// Redis' `master_repl_offset`: it only follows the stream while a replication backlog
    /// exists (one past `offset - delta`); without one it stays put, but for one per propagated
    /// command while the write log is durable (the AOF's fsync tracking).
    pub repl: u64,
    /// A backlog is attached: `repl` follows `offset`, `delta` apart.
    pub attached: bool,
    pub delta: u64,
    sinks: Vec<(u64, Arc<dyn FeedSink>)>,
    next_sink: u64,
}

pub struct Database {
    pub id: String,
    pub shards: Vec<Shard>,
    pub hub: Arc<Hub>,
    pub feed: Mutex<Feed>,
    /// ACL users and log, shared by every shard's engine.
    pub acl: Arc<RwLock<Acl>>,
    /// Connections currently attached (for scale-to-zero decisions and metering).
    pub connections: AtomicU64,
    /// Unix ms of the last command (idle detection).
    pub last_active: AtomicU64,
    pub commands: AtomicU64,
    /// Bytes read from / written to clients (metering).
    pub net_in: AtomicU64,
    pub net_out: AtomicU64,
    /// Set when the database is being unloaded or moved: new commands get `LOADING`/`TRYAGAIN`.
    pub closed: AtomicBool,
    /// Durable (in object storage) stream offset, when the database is persisted.
    pub durability: Mutex<Option<Durability>>,
    /// Replies to writes wait until they are durable (zero acknowledged-write loss).
    pub sync_writes: AtomicBool,
    /// Replication backlog, when this database is a primary with replicas.
    pub backlog: Mutex<Option<Arc<Backlog>>>,
    /// Signalled to disconnect every client (delete, restore, move to another node).
    pub kicked: tokio::sync::Notify,
    pub evicting: AtomicBool,
    /// Connection-level counters for INFO.
    pub io: IoStats,
    /// Replication id of this database's write stream (INFO `master_replid`), and the previous
    /// one with the offset it is valid up to (`master_replid2`, `second_repl_offset`).
    pub replid: Mutex<String>,
    pub replid2: Mutex<Option<(String, u64)>>,
    /// The primary this database follows, when it is a replica.
    pub upstream: Mutex<Option<Upstream>>,
    /// A FAILOVER under way (`server.failover_state`).
    pub failover: Mutex<Option<Failover>>,
    /// Signalled when the replication history changes (REPLICAOF): Redis replicas reconnect.
    pub replicas_reset: tokio::sync::Notify,
    /// A SHUTDOWN waiting for replicas to catch up: SHUTDOWN ABORT signals it.
    pub shutdown: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

/// FAILOVER's state: waiting for a replica to catch up, or handing over to `target`.
#[derive(Clone, Debug, Default)]
pub struct Failover {
    pub in_progress: bool,
    pub target: Option<(String, u16)>,
    /// Unix ms when it times out (0: never).
    pub end_ms: i64,
    pub force: bool,
    /// The port we announce to the target once we follow it (`REPLCONF listening-port`).
    pub listening_port: u16,
}

/// The primary a replica database follows.
#[derive(Clone)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
    pub follower: std::sync::Weak<crate::repl::Follower>,
    /// Set up with REPLICAOF (Redis' protocol) rather than by the platform.
    pub user: bool,
}

/// A random 40-character replication id.
pub fn new_replid() -> String {
    let mut rng = redis_engine::util::Rng::new();
    (0..40).map(|_| char::from_digit((rng.next_u64() % 16) as u32, 16).unwrap()).collect()
}

/// Connection-level counters (INFO stats and threads).
#[derive(Default)]
pub struct IoStats {
    /// Connections that attached to the database (`total_connections_received`).
    pub accepted: AtomicU64,
    /// Connections attached right now, and the ones turned away by `maxclients`.
    pub clients: AtomicU64,
    pub rejected: AtomicU64,
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    /// Reads that parsed commands, and how many they parsed.
    pub events: AtomicU64,
    pub pipeline_sum: AtomicU64,
    /// Replication stream bytes received (replica) and sent (primary).
    pub repl_in: AtomicU64,
    pub repl_out: AtomicU64,
    /// Full and partial resynchronizations served (INFO `sync_*`).
    pub sync_full: AtomicU64,
    pub sync_partial_ok: AtomicU64,
    pub sync_partial_err: AtomicU64,
    /// Replicas attached with Redis' protocol (they get PINGs in the stream).
    pub redis_replicas: AtomicU64,
}

/// INFO's view of the server around an engine.
struct DbHost(std::sync::Weak<Database>);

impl redis_engine::commands::info::Host for DbHost {
    fn counters(&self) -> redis_engine::commands::info::Counters {
        let Some(db) = self.0.upgrade() else { return Default::default() };
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        redis_engine::commands::info::Counters {
            connections: l(&db.io.accepted),
            net_in: l(&db.net_in),
            net_out: l(&db.net_out),
            net_repl_in: l(&db.io.repl_in),
            net_repl_out: l(&db.io.repl_out),
            reads: l(&db.io.reads),
            writes: l(&db.io.writes),
            processing_events: l(&db.io.events),
            pipeline_sum: l(&db.io.pipeline_sum),
            pipeline_cnt: l(&db.io.events),
            rejected_connections: l(&db.io.rejected),
            sync_full: l(&db.io.sync_full),
            sync_partial_ok: l(&db.io.sync_partial_ok),
            sync_partial_err: l(&db.io.sync_partial_err),
        }
    }

    fn replication(&self) -> redis_engine::commands::info::Replication {
        use redis_engine::commands::info::{MasterInfo, ReplicaInfo, Replication};
        let Some(db) = self.0.upgrade() else { return Default::default() };
        let mut r = Replication { replid: db.replid.lock().clone(), offset: db.repl_offset(), ..Default::default() };
        r.failover_state = match &*db.failover.lock() {
            None => "no-failover",
            Some(f) if f.in_progress => "failover-in-progress",
            Some(_) => "waiting-for-sync",
        };
        r.replid2 = db.replid2.lock().clone();
        let backlog = db.backlog.lock().clone();
        if let Some(b) = backlog {
            let (start, end) = b.range();
            r.backlog = Some((b.cap as u64, db.stream_to_repl(start) + 1, end - start));
            let now = redis_engine::util::now_ms();
            let acks = b.acks.lock().clone();
            for (id, (addr, at)) in b.peers.lock().iter() {
                r.replicas.push(ReplicaInfo {
                    ip: addr.ip().to_string(),
                    port: addr.port(),
                    offset: acks.get(id).copied().unwrap_or(0),
                    lag: (now - *at).max(0) / 1000,
                });
            }
        }
        if let Some(u) = db.upstream.lock().clone()
            && let Some(f) = u.follower.upgrade()
        {
            let now = redis_engine::util::now_ms();
            let last = f.last_contact.load(Ordering::Relaxed) as i64;
            let changed = (f.link_changed.load(Ordering::Relaxed) / 1000) as i64;
            let up = f.connected.load(Ordering::Relaxed);
            r.master = Some(MasterInfo {
                host: u.host.clone(),
                port: u.port,
                state: f.state(),
                sync_in_progress: f.syncing.load(Ordering::Relaxed),
                managed: !u.user,
                link_up: up,
                last_io: if last > 0 { (now - last) / 1000 } else { -1 },
                offset: f.applied(),
                down_since: if up { 0 } else { changed },
                up_since: if up { changed } else { 0 },
            });
        }
        r
    }

    fn aof_enabled(&self) -> bool {
        self.0.upgrade().is_some_and(|db| db.durability.lock().is_some())
    }
}

#[derive(Clone)]
pub struct Durability {
    pub durable: watch::Receiver<u64>,
    /// Ask for an early flush.
    pub kick: Arc<dyn Fn() + Send + Sync>,
}

impl Durability {
    /// Wait until `offset` is durable, up to `timeout` (`None`: forever). Returns whether it is.
    pub async fn wait(&self, offset: u64, timeout: Option<Duration>) -> bool {
        let mut rx = self.durable.clone();
        if *rx.borrow() >= offset {
            return true;
        }
        (self.kick)();
        let fut = rx.wait_for(|d| *d >= offset);
        match timeout {
            Some(t) => matches!(tokio::time::timeout(t, fut).await, Ok(Ok(_))),
            None => fut.await.is_ok(),
        }
    }
}

/// Exclusive access to a shard's engine. Publishes the effects of whatever ran when dropped.
pub struct EngineGuard<'a> {
    db: &'a Database,
    shard: usize,
    eng: MutexGuard<'a, Engine>,
    /// When the lock was taken and the command count then (INFO's event-loop cycles).
    since: u64,
    commands: u64,
}

impl<'a> EngineGuard<'a> {
    fn new(db: &'a Database, shard: usize, eng: MutexGuard<'a, Engine>) -> Self {
        let commands = eng.stats.commands;
        Self { db, shard, eng, since: redis_engine::util::mono_us(), commands }
    }
}

impl Deref for EngineGuard<'_> {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.eng
    }
}

impl DerefMut for EngineGuard<'_> {
    fn deref_mut(&mut self) -> &mut Engine {
        &mut self.eng
    }
}

impl Drop for EngineGuard<'_> {
    fn drop(&mut self) {
        self.eng.before_sleep();
        let paused = self.eng.clients.pause.is_some() || self.eng.clients.failover_pause;
        self.db.shards[self.shard].paused.store(paused, Ordering::SeqCst);
        let cmds = self.eng.stats.commands.saturating_sub(self.commands);
        self.eng.cycle_done(cmds, redis_engine::util::mono_us().saturating_sub(self.since));
        if !self.eng.effects.is_empty() {
            // Published in place: the list keeps its allocation for the next batch.
            self.db.publish(&self.eng.effects);
            self.eng.effects.clear();
        }
    }
}

impl Database {
    pub fn new(id: impl Into<String>, config: Config, shards: usize) -> Arc<Self> {
        let hub = Hub::new();
        let acl = Arc::new(RwLock::new(Acl::new()));
        let shards = (0..shards.max(1))
            .map(|i| {
                let mut e = Engine::new(config.clone(), hub.clone());
                e.shard = i as u16;
                e.acl = acl.clone();
                let scripts = e.scripting.ctl.clone();
                Shard { engine: Mutex::new(e), scripts, paused: AtomicBool::new(false) }
            })
            .collect();
        let replid = new_replid();
        let db = Arc::new(Self {
            id: id.into(),
            shards,
            hub,
            feed: Mutex::new(Feed { last_db: u16::MAX, ..Feed::default() }),
            acl,
            connections: AtomicU64::new(0),
            last_active: AtomicU64::new(redis_engine::util::now_ms() as u64),
            commands: AtomicU64::new(0),
            net_in: AtomicU64::new(0),
            net_out: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            durability: Mutex::new(None),
            sync_writes: AtomicBool::new(false),
            backlog: Mutex::new(None),
            kicked: tokio::sync::Notify::new(),
            evicting: AtomicBool::new(false),
            io: IoStats::default(),
            replid: Mutex::new(replid),
            replid2: Mutex::new(None),
            upstream: Mutex::new(None),
            failover: Mutex::new(None),
            replicas_reset: tokio::sync::Notify::new(),
            shutdown: Mutex::new(None),
        });
        let host: Arc<dyn redis_engine::commands::info::Host> = Arc::new(DbHost(Arc::downgrade(&db)));
        for s in &db.shards {
            s.engine.lock().host = Some(host.clone());
        }
        db
    }

    /// Close the database to new sessions and disconnect current clients. They reconnect and
    /// find the database wherever it is next (reloaded, restored, or on another node).
    pub fn close_and_kick(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.evicting.store(true, Ordering::SeqCst);
        self.kicked.notify_waiters();
    }

    pub fn shard(&self, i: usize) -> &Shard {
        &self.shards[i]
    }

    /// Lock shard `i`'s engine.
    pub fn engine(&self, i: usize) -> EngineGuard<'_> {
        EngineGuard::new(self, i, self.shards[i].engine.lock())
    }

    /// Lock shard `i`'s engine, unless a script holding it runs past `busy-reply-threshold`:
    /// then `None`, and the caller answers with [`redis_engine::commands::scripting::while_busy`].
    pub fn engine_unless_busy(&self, i: usize) -> Option<EngineGuard<'_>> {
        let shard = &self.shards[i];
        loop {
            if let Some(eng) = shard.engine.try_lock_for(std::time::Duration::from_millis(10)) {
                return Some(EngineGuard::new(self, i, eng));
            }
            if shard.scripts.busy.load(Ordering::SeqCst) || shard.scripts.debugging.load(Ordering::SeqCst) {
                return None;
            }
        }
    }

    /// [`Database::engine_unless_busy`] for a connection's task: while another holds the engine
    /// briefly (a batch of commands), yield to the worker's other connections instead of parking
    /// its thread, so their reads, parsing and replies go on meanwhile; past that, park.
    pub async fn engine_unless_busy_async(&self, i: usize) -> Option<EngineGuard<'_>> {
        let shard = &self.shards[i];
        // Timed from the first miss.
        let mut start = None;
        loop {
            if let Some(eng) = shard.engine.try_lock() {
                return Some(EngineGuard::new(self, i, eng));
            }
            let t = redis_engine::util::mono_us();
            if t.saturating_sub(*start.get_or_insert(t)) > YIELD_FOR.as_micros() as u64 || shard.scripts.busy.load(Ordering::SeqCst) || shard.scripts.debugging.load(Ordering::SeqCst) {
                return self.engine_unless_busy(i);
            }
            tokio::task::yield_now().await;
        }
    }

    /// Whether a pause may hold commands back on shard `i` (else none does).
    pub fn may_be_paused(&self, i: usize) -> bool {
        self.shards[i].paused.load(Ordering::SeqCst)
    }

    pub fn touch(&self, n: u64) {
        self.last_active.store(redis_engine::util::now_ms() as u64, Ordering::Relaxed);
        self.commands.fetch_add(n, Ordering::Relaxed);
    }

    /// Unix ms since the last command.
    pub fn idle_ms(&self) -> u64 {
        (redis_engine::util::now_ms() as u64).saturating_sub(self.last_active.load(Ordering::Relaxed))
    }

    pub fn used_memory(&self) -> usize {
        self.shards.iter().map(|s| s.engine.lock().used_memory()).sum()
    }

    pub fn total_keys(&self) -> usize {
        self.shards.iter().map(|s| s.engine.lock().total_keys()).sum()
    }

    /// Subscribe to the write stream. Returns the id (for [`Database::unsubscribe`]) and the
    /// current offset; everything after that offset reaches `sink`.
    pub fn subscribe(&self, sink: Arc<dyn FeedSink>) -> (u64, u64) {
        let mut f = self.feed.lock();
        let id = f.next_sink;
        f.next_sink += 1;
        f.sinks.push((id, sink));
        // The next chunk starts with SELECT so a subscriber can join mid-stream.
        f.last_db = u16::MAX;
        (id, f.offset)
    }

    pub fn unsubscribe(&self, id: u64) {
        self.feed.lock().sinks.retain(|(i, _)| *i != id);
    }

    /// Redis' `master_repl_offset` (what ROLE, INFO, PSYNC and WAIT speak).
    pub fn repl_offset(&self) -> u64 {
        self.feed.lock().repl
    }

    /// The write stream's position (durability, snapshots, the backlog's chunks).
    pub fn stream_offset(&self) -> u64 {
        self.feed.lock().offset
    }

    /// A replication offset as a stream position, while a backlog is attached.
    pub fn repl_to_stream(&self, repl: u64) -> u64 {
        repl + self.feed.lock().delta
    }

    /// A stream position as a replication offset, while a backlog is attached.
    pub fn stream_to_repl(&self, offset: u64) -> u64 {
        offset.saturating_sub(self.feed.lock().delta)
    }

    /// Continue the stream at `offset` (after restoring state up to it, or a primary's stream
    /// as a replica): the replication offset is the same.
    pub fn set_offset(&self, offset: u64) {
        let mut f = self.feed.lock();
        f.offset = offset;
        f.repl = offset;
        f.delta = 0;
        f.last_db = u16::MAX;
    }

    /// Subscribe a replication backlog (`createReplicationBacklog`): from now on the replication
    /// offset follows the stream. Returns the sink id and the stream offset it starts at.
    pub fn attach_backlog(&self, sink: Arc<dyn FeedSink>) -> (u64, u64) {
        let (id, offset) = self.subscribe(sink);
        let mut f = self.feed.lock();
        f.attached = true;
        f.delta = f.offset - f.repl;
        (id, offset)
    }

    /// FAILOVER's write pause on every shard (`PAUSE_DURING_FAILOVER`).
    pub fn set_failover_pause(&self, on: bool) {
        for i in 0..self.shards.len() {
            self.engine(i).clients.failover_pause = on;
        }
    }

    /// `clearFailoverState`: no failover, writes resume.
    pub fn clear_failover(&self) {
        *self.failover.lock() = None;
        self.set_failover_pause(false);
    }

    /// From now on the replication offset follows the stream (a promoted replica).
    pub fn follow_stream(&self) {
        let mut f = self.feed.lock();
        f.attached = true;
        f.delta = f.offset - f.repl;
    }

    /// The backlog is gone (`freeReplicationBacklog`): the replication offset stops following
    /// the stream.
    pub fn detach_backlog(&self, id: u64) {
        self.unsubscribe(id);
        self.feed.lock().attached = false;
    }

    /// Call with the engine locked when capturing a snapshot: returns the offset the snapshot is
    /// consistent with, and makes the stream after it self-contained (starts with `SELECT`).
    pub fn snapshot_point(&self) -> u64 {
        let mut f = self.feed.lock();
        f.last_db = u16::MAX;
        f.offset
    }

    /// Put a command outside any database into the stream (PING, REPLCONF GETACK), as Redis'
    /// replication cron does: replicas count it in their offset.
    pub fn publish_raw(&self, args: &[&[u8]]) {
        let mut f = self.feed.lock();
        let mut out = BytesMut::new();
        redis_proto::encode_command(args, &mut out);
        f.offset += out.len() as u64;
        if f.attached {
            f.repl += out.len() as u64;
        }
        let chunk = out.freeze();
        for (_, s) in &f.sinks {
            s.write(&chunk, f.offset);
        }
    }

    /// Append a primary's stream verbatim (a replica's own stream, for its replicas and backlog).
    pub fn publish_bytes(&self, chunk: Bytes) {
        let mut f = self.feed.lock();
        f.offset += chunk.len() as u64;
        if f.attached {
            f.repl += chunk.len() as u64;
        }
        for (_, s) in &f.sinks {
            s.write(&chunk, f.offset);
        }
    }

    fn publish(&self, effects: &[Effect]) {
        let mut f = self.feed.lock();
        // The stream's length first: without subscribers that is all that's needed, else it
        // sizes the chunk.
        let mut len = 0;
        let mut last_db = f.last_db;
        for e in effects {
            if e.db != last_db {
                len += redis_proto::command_len(&[b"SELECT".as_slice(), e.db.to_string().as_bytes()]);
                last_db = e.db;
            }
            len += redis_proto::command_len(&e.args);
        }
        let mut out = BytesMut::new();
        if !f.sinks.is_empty() {
            out.reserve(len);
            for e in effects {
                if e.db != f.last_db {
                    redis_proto::encode_command([b"SELECT".as_slice(), e.db.to_string().as_bytes()], &mut out);
                    f.last_db = e.db;
                }
                redis_proto::encode_command(&e.args, &mut out);
            }
            debug_assert_eq!(out.len(), len);
        }
        f.last_db = last_db;
        f.offset += len as u64;
        if f.attached {
            f.repl += len as u64;
        } else if self.durability.lock().is_some() {
            // `replicationFeedSlaves` without a backlog: one per propagated command.
            f.repl += effects.len() as u64;
        }
        if f.sinks.is_empty() {
            return;
        }
        let chunk = out.freeze();
        for (_, s) in &f.sinks {
            s.write(&chunk, f.offset);
        }
    }

    /// Background maintenance (Redis' serverCron): active expiry and incremental rehashing,
    /// `hz` times a second. Stops when the database is closed and dropped.
    pub fn start_cron(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut ticks = 0u64;
            loop {
                tick.tick().await;
                ticks += 1;
                let Some(db) = weak.upgrade() else { return };
                if db.closed.load(Ordering::Relaxed) {
                    return;
                }
                // Redis' replication cron: a PING in the stream every repl-ping-replica-period.
                if ticks.is_multiple_of(crate::repl::psync::PING_PERIOD_S * 10) && db.io.redis_replicas.load(Ordering::Relaxed) > 0 {
                    db.publish_raw(&[b"PING"]);
                }
                crate::repl::psync::failover_cron(&db, redis_engine::util::now_ms());
                if ticks.is_multiple_of(10) {
                    let ttl = db.engine_unless_busy(0).and_then(|e| e.cfg.get(b"repl-backlog-ttl").first().and_then(|(_, v)| v.parse().ok()));
                    if let Some(ttl) = ttl {
                        crate::repl::psync::backlog_ttl_cron(&db, ttl, redis_engine::util::now_ms());
                    }
                }
                for i in 0..db.shards.len() {
                    // A busy script holds the engine: skip the tick rather than tie up a worker
                    // that SCRIPT KILL may need.
                    let Some(mut eng) = db.engine_unless_busy(i) else { continue };
                    // DEBUG PAUSE-CRON.
                    if eng.debug.pause_cron {
                        continue;
                    }
                    let hz = eng.cfg.hz.max(1) as u64;
                    // Redis' slow cycle: 25% of each tick's CPU time at most.
                    let started = std::time::Instant::now();
                    eng.active_expire(250_000 / hz);
                    eng.rehash(100);
                    eng.cron_metrics();
                    eng.persist_cron(redis_engine::util::now_ms());
                    eng.search_cron(redis_engine::util::now_ms());
                    if i == 0 {
                        eng.clients_cron(redis_engine::util::now_ms());
                        eng.hub.templates.cron(redis_engine::util::now_ms());
                        eng.evict_clients(true);
                    }
                    eng.metrics.cron_us += started.elapsed().as_micros() as u64;
                }
            }
        });
    }
}

/// A feed sink that collects the stream (tests).
#[derive(Default)]
pub struct VecFeed(pub Mutex<Vec<u8>>);

impl FeedSink for VecFeed {
    fn write(&self, chunk: &Bytes, _end: u64) {
        self.0.lock().extend_from_slice(chunk);
    }
}
